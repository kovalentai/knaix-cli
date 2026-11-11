//! `knaix local` commands, run the way a user runs them.
//!
//! Every docker call runs `docker` from the PATH, so a stand-in script plays the
//! daemon, keeping the container's state in files beside it, and a loopback
//! server on the published port plays the node. HOME is a scratch directory, so
//! `local.json` and the default node are each test's own, and PATH holds only
//! the stand-in, so no test can touch a real container.
//!
//! Unix only: the stand-in is a shell script.
#![cfg(unix)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const IMAGE: &str = "ghcr.io/kovalentai/node-runtime:latest";
const INSTANCE: &str = "0b5e8a4c-1d2f-4e3a-9b8c-7d6e5f4a3b2c";

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-local-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".knaix")).unwrap();
    fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home)
        .env("PATH", home.join("bin"))
        .env_remove("KNAIX_TOKEN")
        .env_remove("KNAIX_LOCAL_IMAGE")
        .env_remove("XDG_CONFIG_HOME")
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .stdin(Stdio::null());
    cmd
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn run(cmd: &mut Command) -> Run {
    let out = cmd.output().expect("failed to run knaix");
    Run {
        code: out.status.code().expect("killed by a signal"),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A stand-in `docker` in the scratch home's `bin`. Its container lives in
/// files under `<script>.d`: `state` is what `inspect` reports (no file, no
/// container), `image` what it was started from, `port` what `docker port`
/// prints. Other files are knobs, set with `Docker::set`.
struct Docker {
    path: PathBuf,
    dir: PathBuf,
}

fn docker(home: &Path) -> Docker {
    use std::os::unix::fs::PermissionsExt;
    let path = home.join("bin/docker");
    // Its own PATH: knaix's holds only this script, and it needs cat and rm.
    let script = r#"#!/bin/sh
PATH=/usr/bin:/bin
d="$0.d"
echo "$@" >> "$0.args"
case "$1" in
  --version) echo "Docker version 27.0.0" ;;
  info) [ -e "$d/daemon-down" ] && exit 1; echo 27.0.0 ;;
  inspect)
    [ -e "$d/state" ] || { echo "Error: No such object: knaix-local" >&2; exit 1; }
    case "$3" in
      *State.Status*) cat "$d/state" ;;
      *Config.Image*) cat "$d/image" ;;
    esac ;;
  image) [ -e "$d/present" ] || exit 1 ;;
  pull) [ -e "$d/pull-fails" ] && exit 1; touch "$d/present" ;;
  run)
    for a in "$@"; do last="$a"; done
    echo "$last" > "$d/image"
    if [ -e "$d/run-state" ]; then cp "$d/run-state" "$d/state"; else echo running > "$d/state"; fi
    echo 4f3c2b1a ;;
  rm) rm -f "$d/state" ;;
  port) [ -e "$d/port" ] || exit 1; cat "$d/port" ;;
  volume) [ -e "$d/volume-error" ] && { cat "$d/volume-error" >&2; exit 1; }; echo knaix-local-data ;;
  logs) echo "node booting"; echo "store open" ;;
esac
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let dir = home.join("bin/docker.d");
    fs::create_dir_all(&dir).unwrap();
    Docker { path, dir }
}

impl Docker {
    fn set(&self, knob: &str, content: &str) -> &Self {
        fs::write(self.dir.join(knob), content).unwrap();
        self
    }

    fn unset(&self, knob: &str) {
        let _ = fs::remove_file(self.dir.join(knob));
    }

    /// The node `up` recorded, running and published on loopback at `port`.
    fn running_ours(&self, port: u16) -> &Self {
        self.set("state", "running")
            .set("image", IMAGE)
            .set("present", "")
            .set("port", &format!("127.0.0.1:{port}\n[::1]:{port}\n"))
    }

    /// Each call's arguments, then forgets them.
    fn calls(&self) -> Vec<String> {
        let args = PathBuf::from(format!("{}.args", self.path.display()));
        let calls = fs::read_to_string(&args).unwrap_or_default();
        let _ = fs::remove_file(&args);
        calls.lines().map(str::to_string).collect()
    }
}

fn the_run(calls: &[String]) -> &str {
    calls
        .iter()
        .find(|c| c.starts_with("run "))
        .unwrap_or_else(|| panic!("no docker run in {calls:#?}"))
}

/// Each route's status, body and how many milliseconds to wait before it.
type Answers = Arc<Mutex<HashMap<String, (u16, String, u64)>>>;

/// The node on loopback: `GET /health` and `POST /api/query/answer`, each
/// answering as `answer` last set. Records each request with its JSON body.
struct Node {
    port: u16,
    answers: Answers,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

fn node() -> Node {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let answers = Arc::new(Mutex::new(HashMap::from([
        ("GET /health".to_string(), (200, "{}".to_string(), 0)),
        (
            "POST /api/query/answer".to_string(),
            (200, r#"{"answer":"ok"}"#.to_string(), 0),
        ),
    ])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (table, log) = (answers.clone(), calls.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let route = line.split(' ').take(2).collect::<Vec<_>>().join(" ");
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(": ") {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.parse().unwrap();
                    }
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            // Chosen before the request is recorded, so a test waiting on the
            // record changes only the answers after it.
            let (status, reply, delay) =
                table
                    .lock()
                    .unwrap()
                    .get(&route)
                    .cloned()
                    .unwrap_or((404, "{}".into(), 0));
            log.lock().unwrap().push((
                route.clone(),
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            std::thread::sleep(std::time::Duration::from_millis(delay));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    Node {
        port,
        answers,
        calls,
    }
}

impl Node {
    fn answer(&self, route: &str, status: u16, body: &str) {
        self.answer_after(route, status, body, 0);
    }

    /// Answer `route` only after `ms` milliseconds.
    fn answer_after(&self, route: &str, status: u16, body: &str, ms: u64) {
        self.answers
            .lock()
            .unwrap()
            .insert(route.to_string(), (status, body.to_string(), ms));
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

/// A port nothing listens on.
fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What `up` recorded, with `overrides` laid over it.
fn record(home: &Path, port: u16, overrides: Value) {
    let mut node = json!({"port": port, "instance_id": INSTANCE, "image": IMAGE});
    node.as_object_mut()
        .unwrap()
        .extend(overrides.as_object().unwrap().clone());
    fs::write(home.join(".knaix/local.json"), node.to_string()).unwrap();
}

fn recorded(home: &Path) -> Option<Value> {
    let raw = fs::read_to_string(home.join(".knaix/local.json")).ok()?;
    Some(serde_json::from_str(&raw).unwrap())
}

fn default_node(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(home.join(".knaix/config.json")).ok()?;
    let config: Value = serde_json::from_str(&raw).unwrap();
    config["default_node_id"].as_str().map(str::to_string)
}

fn set_default_node(home: &Path, id: &str) {
    fs::write(
        home.join(".knaix/config.json"),
        json!({"default_node_id": id}).to_string(),
    )
    .unwrap();
}

fn local(home: &Path, args: &[&str]) -> Run {
    run(knaix(home).arg("local").args(args))
}

#[test]
fn without_docker_the_node_cannot_start_and_says_what_to_do() {
    let home = scratch_home("no-docker");

    let up = local(&home, &["up"]);
    assert_eq!(up.code, 7, "{}", up.err);
    assert!(up.err.contains("Docker is not installed"), "{}", up.err);

    let docker = docker(&home);
    docker.set("daemon-down", "");
    for args in [&["up"][..], &["down"], &["logs"], &["reset", "--yes"]] {
        let r = local(&home, args);
        assert_eq!(r.code, 7, "{args:?}: {}", r.err);
        assert!(
            r.err
                .contains("Docker is installed but not running. Start Docker"),
            "{args:?}: {}",
            r.err
        );
    }
}

#[test]
fn the_first_up_pulls_publishes_to_loopback_waits_and_warms_the_node() {
    let home = scratch_home("first-up");
    let docker = docker(&home);
    let node = node();
    let port = node.port.to_string();

    let up = local(&home, &["up", "--port", &port]);

    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    for expected in [
        format!("Fetching {IMAGE} (about 380 MB, once)."),
        "Warming the store and model (".into(),
        format!("Local node ready on http://127.0.0.1:{port}."),
        "No model is configured, so answers come from the deterministic mock.".into(),
        "Set as your default node, so you can drop -n local.".into(),
        "knaix chat \"what is this about?\"".into(),
    ] {
        assert!(
            up.out.contains(&expected),
            "missing {expected:?} in:\n{}",
            up.out
        );
    }
    let calls = docker.calls();
    assert!(calls.contains(&format!("pull {IMAGE}")), "{calls:#?}");
    let saved = recorded(&home).unwrap();
    let id = saved["instance_id"].as_str().unwrap();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "4", "a v4-shaped id: {id}");
    assert_eq!(saved["port"], node.port);
    assert_eq!(saved["image"], IMAGE);
    assert_eq!(saved["model_url"], Value::Null);
    let launched = the_run(&calls);
    let wanted = format!(
        "run -d --name knaix-local --restart unless-stopped -p 127.0.0.1:{port}:8080 \
         -v knaix-local-data:/data -e DATA_DIR=/data -e KB_INSTANCE_ID={id} \
         -e A2A_AUTH_DISABLED=true -e A2A_ALLOW_PRIVATE_NETWORK=true -e GENERATION_MAX_TOKENS="
    );
    assert!(launched.starts_with(&wanted), "{launched}");
    assert!(
        launched.ends_with(&format!(" -e GENERATION_PROVIDER=mock {IMAGE}")),
        "{launched}"
    );
    assert!(
        !launched.contains("GENERATION_TIMEOUT_MS"),
        "the mock waits on nothing"
    );
    assert_eq!(default_node(&home).as_deref(), Some("local"));

    let calls = node.calls();
    assert_eq!(calls[0].0, "GET /health");
    let (route, warm) = &calls[1];
    assert_eq!(route, "POST /api/query/answer");
    assert_eq!(warm["instance_id"], id);
    assert_eq!(warm["max_tokens"], 8);
    assert_eq!(warm["policy"]["rerank"], true);
}

#[test]
fn up_with_a_model_on_this_machine_reaches_it_from_the_container_and_remembers_it() {
    let home = scratch_home("model-up");
    let docker = docker(&home);
    docker.set("present", "");
    let node = node();
    let port = node.port.to_string();
    node.answer(
        "POST /api/query/answer",
        502,
        r#"{"message":"model 'llama3' not found"}"#,
    );
    set_default_node(&home, "hosted-1");

    let up = local(
        &home,
        &[
            "up",
            "--port",
            &port,
            "--model-url",
            "http://localhost:11434",
            "--model",
            "llama3",
            "--generation-timeout",
            "600",
        ],
    );

    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    for expected in [
        "Model URL http://localhost:11434 is loopback",
        "Using http://host.docker.internal:11434/ so it reaches this machine.",
        "The model did not answer: model 'llama3' not found",
        "Questions will fail until this is fixed.",
        "Your default node is hosted-1; pass -n local to reach this one.",
        "knaix chat -n local \"what is this about?\"",
    ] {
        assert!(
            up.out.contains(expected),
            "missing {expected:?} in:\n{}",
            up.out
        );
    }
    assert!(!up.out.contains("Fetching"), "the image is already here");
    let calls = docker.calls();
    let launched = the_run(&calls);
    assert!(
        launched.ends_with(&format!(
            " --add-host host.docker.internal:host-gateway \
             -e LLAMA_SERVER_URL=http://host.docker.internal:11434/ -e LLAMA_MODEL_ID=llama3 {IMAGE}"
        )),
        "{launched}"
    );
    assert!(
        launched.contains(" -e GENERATION_TIMEOUT_MS=600000 "),
        "{launched}"
    );
    let saved = recorded(&home).unwrap();
    assert_eq!(saved["model_url"], "http://localhost:11434");
    assert_eq!(saved["model"], "llama3");
    assert_eq!(saved["generation_timeout_secs"], 600);
    assert_eq!(
        default_node(&home).as_deref(),
        Some("hosted-1"),
        "theirs to keep"
    );

    // Stopped and started again with no flags: the same model answers.
    docker.unset("state");
    node.answer("POST /api/query/answer", 500, "upstream exploded");
    let again = local(&home, &["up"]);
    assert_eq!(again.code, 0, "{}{}", again.out, again.err);
    assert!(
        again.out.contains(
            "Using the model server from last time: llama3 at http://localhost:11434. --mock to go back to the mock."
        ),
        "{}",
        again.out
    );
    assert!(again
        .out
        .contains("The model did not answer: the model did not answer"));
    let calls = docker.calls();
    let launched = the_run(&calls);
    assert!(
        launched.contains(&format!("-p 127.0.0.1:{port}:8080")),
        "{launched}"
    );
    assert!(launched.contains("-e LLAMA_MODEL_ID=llama3 "), "{launched}");
    assert!(
        launched.contains("-e GENERATION_TIMEOUT_MS=600000 "),
        "{launched}"
    );

    // A server remembered without a model name: llama-server needs none.
    docker.unset("state");
    record(
        &home,
        node.port,
        json!({"model_url": "http://localhost:8081"}),
    );
    let again = local(&home, &["up"]);
    assert!(
        again
            .out
            .contains("Using the model server from last time: http://localhost:8081."),
        "{}",
        again.out
    );
    assert!(!the_run(&docker.calls()).contains("LLAMA_MODEL_ID"));
}

#[test]
fn a_model_name_without_a_server_is_called_out_and_the_mock_answers() {
    let home = scratch_home("model-no-server");
    let docker = docker(&home);
    docker.set("present", "");
    let node = node();
    node.answer("POST /api/query/answer", 404, "{}");
    set_default_node(&home, "local");

    let up = local(
        &home,
        &[
            "up",
            "--port",
            &node.port.to_string(),
            "--mock",
            "-m",
            "qwen",
        ],
    );

    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    for expected in [
        "A model name was given but no server is configured to ask for it.",
        "skipped; the first question will be slower.",
        "The deterministic mock answers, as asked.",
    ] {
        assert!(
            up.out.contains(expected),
            "missing {expected:?} in:\n{}",
            up.out
        );
    }
    assert!(
        !up.out.contains("Set as your default node"),
        "already the default"
    );
    assert!(the_run(&docker.calls()).ends_with(&format!("-e GENERATION_PROVIDER=mock {IMAGE}")));
}

#[test]
fn a_failed_pull_falls_back_to_the_image_here_or_stops() {
    let home = scratch_home("pull");
    let docker = docker(&home);
    docker.set("pull-fails", "");
    let node = node();
    let port = node.port.to_string();

    let up = local(&home, &["up", "--port", &port, "--mock"]);
    assert_eq!(up.code, 1, "{}", up.out);
    assert!(
        up.err
            .contains(&format!("Could not pull {IMAGE}: docker pull exited with")),
        "{}",
        up.err
    );
    assert!(
        up.err
            .contains("Set KNAIX_LOCAL_IMAGE to an image you already have"),
        "{}",
        up.err
    );
    assert!(!docker.calls().iter().any(|c| c.starts_with("run ")));

    docker.set("present", "");
    let up = run(knaix(&home)
        .env("KNAIX_LOCAL_IMAGE", "knaix-node:dev")
        .args(["local", "up", "--pull", "--mock", "--port", &port]));
    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    assert!(up.out.contains("Fetching knaix-node:dev"), "{}", up.out);
    assert!(
        up.out.contains("Pull failed (docker pull exited with"),
        "{}",
        up.out
    );
    assert!(
        up.out.contains("using the copy already on this machine."),
        "{}",
        up.out
    );
    assert!(the_run(&docker.calls()).ends_with(" knaix-node:dev"));
    assert_eq!(recorded(&home).unwrap()["image"], "knaix-node:dev");
}

#[test]
fn a_slow_node_is_waited_for_and_warmed_with_a_dot_a_second() {
    let home = scratch_home("slow");
    let docker = docker(&home);
    docker.set("present", "");
    let node = node();
    node.answer("GET /health", 503, "{}");
    node.answer_after("POST /api/query/answer", 200, "{}", 1_500);
    let (answers, calls) = (node.answers.clone(), node.calls.clone());
    // Ready once it has turned one poll away, so the next one finds it ready.
    std::thread::spawn(move || {
        while !calls
            .lock()
            .unwrap()
            .iter()
            .any(|(r, _)| r == "GET /health")
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let ready = (200, "{}".to_string(), 0);
        answers.lock().unwrap().insert("GET /health".into(), ready);
    });

    let up = local(&home, &["up", "--port", &node.port.to_string(), "--mock"]);

    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    assert!(
        up.out.contains("Waiting for the node to open its store."),
        "{}",
        up.out
    );
    assert!(
        up.out.contains("Warming the store and model."),
        "{}",
        up.out
    );
    let polls = node
        .calls()
        .iter()
        .filter(|(r, _)| r == "GET /health")
        .count();
    assert!(polls >= 2, "{polls} polls");
    assert!(docker
        .calls()
        .iter()
        .any(|c| c == "inspect --format {{.State.Status}} knaix-local"));
}

#[test]
fn a_node_that_exits_while_starting_shows_why() {
    let home = scratch_home("exits");
    let docker = docker(&home);
    // A container left from before holds the name until it is removed.
    docker
        .set("present", "")
        .set("state", "exited")
        .set("run-state", "exited");
    let port = closed_port().to_string();

    let up = local(&home, &["up", "--port", &port, "--mock"]);

    assert_eq!(up.code, 1, "{}", up.out);
    assert!(
        up.err
            .contains("The local node stopped while starting:\nnode booting\nstore open"),
        "{}",
        up.err
    );
    let calls = docker.calls();
    let removed = calls.iter().position(|c| c == "rm -f knaix-local").unwrap();
    let started = calls.iter().position(|c| c.starts_with("run ")).unwrap();
    assert!(removed < started, "{calls:#?}");
    assert!(
        calls.contains(&"logs --tail 20 knaix-local".to_string()),
        "{calls:#?}"
    );
}

#[test]
fn a_second_up_leaves_a_running_node_alone() {
    let home = scratch_home("second-up");
    let docker = docker(&home);
    let node = node();
    docker.running_ours(node.port);
    record(&home, node.port, json!({}));

    let up = local(&home, &["up"]);
    assert_eq!(up.code, 0, "{}", up.err);
    assert!(
        up.out.contains(&format!(
            "Local node already running on http://127.0.0.1:{}.",
            node.port
        )),
        "{}",
        up.out
    );
    assert!(up.out.contains("Set as your default node"), "{}", up.out);

    let up = local(&home, &["up", "--mock"]);
    assert!(
        up.out
            .contains("Model flags only apply at startup, so it keeps its current answer source."),
        "{}",
        up.out
    );
    assert!(!up.out.contains("Set as your default node"), "{}", up.out);
    assert!(!docker
        .calls()
        .iter()
        .any(|c| c.starts_with("run ") || c.starts_with("rm ")));
    assert!(node.calls().is_empty(), "nothing to wait for or warm");
}

#[test]
fn a_node_published_on_every_interface_is_recreated_on_loopback_keeping_its_store() {
    let home = scratch_home("exposed");
    let docker = docker(&home);
    let node = node();
    docker.running_ours(node.port).set(
        "port",
        &format!("127.0.0.1:{}\n0.0.0.0:{}\n", node.port, node.port),
    );
    record(&home, node.port, json!({}));

    let up = local(&home, &["up"]);

    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    assert!(
        up.out
            .contains("This node was published on every network interface"),
        "{}",
        up.out
    );
    let calls = docker.calls();
    assert!(
        calls.contains(&"rm -f knaix-local".to_string()),
        "{calls:#?}"
    );
    let launched = the_run(&calls);
    assert!(
        launched.contains(&format!("-p 127.0.0.1:{}:8080 ", node.port)),
        "{launched}"
    );
    assert!(
        launched.contains(&format!("KB_INSTANCE_ID={INSTANCE} ")),
        "{launched}"
    );
    assert_eq!(recorded(&home).unwrap()["instance_id"], INSTANCE);

    // Taken down even when it cannot be started again: left up, it would stay
    // open to the network.
    docker
        .running_ours(node.port)
        .set("port", &format!("0.0.0.0:{}\n", node.port))
        .set("pull-fails", "")
        .unset("present");
    let up = local(&home, &["up"]);
    assert_eq!(up.code, 1, "{}", up.out);
    assert!(up.err.contains("Could not pull"), "{}", up.err);
    assert_eq!(
        docker
            .calls()
            .iter()
            .filter(|c| *c == "rm -f knaix-local")
            .count(),
        1,
        "removed once, before the pull"
    );
}

#[test]
fn a_running_container_this_cli_did_not_start_is_not_touched() {
    let home = scratch_home("not-ours");
    let docker = docker(&home);
    docker.running_ours(8080).set("image", "someone/else:1");
    record(&home, 8080, json!({}));

    let up = local(&home, &["up"]);

    assert_eq!(up.code, 1, "{}", up.out);
    assert!(
        up.err.contains(
            "A 'knaix-local' container is already running but was not started by this CLI."
        ),
        "{}",
        up.err
    );
    assert!(!docker
        .calls()
        .iter()
        .any(|c| c.starts_with("rm ") || c.starts_with("run ")));

    // Nor is one docker cannot say the image of.
    docker.unset("image");
    let up = local(&home, &["up"]);
    assert_eq!(up.code, 1, "{}", up.out);
    assert!(up.err.contains("was not started by this CLI"), "{}", up.err);
}

#[test]
fn down_stops_the_node_and_deletes_the_store_only_when_asked() {
    let home = scratch_home("down");
    let docker = docker(&home);

    let down = local(&home, &["down"]);
    assert_eq!(down.code, 0, "{}", down.err);
    assert!(
        down.out.contains("No local node is running."),
        "{}",
        down.out
    );
    assert!(!down.out.contains("store is kept"));

    docker.running_ours(8080);
    record(&home, 8080, json!({}));
    let down = local(&home, &["down"]);
    assert!(down.out.contains("Local node stopped."), "{}", down.out);
    assert!(
        down.out.contains("Its store is kept in the knaix-local-data volume; knaix local down --purge to delete it."),
        "{}",
        down.out
    );
    assert!(docker.calls().contains(&"rm -f knaix-local".to_string()));
    assert!(
        recorded(&home).is_some(),
        "kept, so the corpus keeps its identity"
    );

    docker.set("volume-error", "Error: volume is in use");
    let down = local(&home, &["down", "--purge"]);
    assert_eq!(down.code, 0, "{}", down.err);
    assert!(
        down.out
            .contains("Could not delete the local store: Error: volume is in use"),
        "{}",
        down.out
    );
    assert!(recorded(&home).is_none());

    docker.unset("volume-error");
    let down = local(&home, &["down", "--purge"]);
    assert!(down.out.contains("Local store deleted."), "{}", down.out);
    assert!(docker
        .calls()
        .contains(&"volume rm knaix-local-data".to_string()));
}

#[test]
fn reset_empties_the_store_and_starts_again_with_the_same_model() {
    let home = scratch_home("reset");
    let docker = docker(&home);
    let node = node();

    let refused = local(&home, &["reset"]);
    assert_eq!(refused.code, 6, "{}", refused.err);
    assert!(refused
        .err
        .contains("Refusing to delete the local store without confirmation."));
    assert!(!docker
        .calls()
        .iter()
        .any(|c| c.starts_with("rm ") || c.starts_with("volume ")));

    docker.running_ours(node.port);
    record(
        &home,
        node.port,
        json!({"model_url": "http://localhost:11434", "model": "llama3"}),
    );
    docker.set("volume-error", "Error: volume is in use");
    let failed = local(&home, &["reset", "--yes"]);
    assert_eq!(failed.code, 1, "{}", failed.out);
    assert!(failed
        .err
        .contains("Could not delete the local store: Error: volume is in use"));

    docker.running_ours(node.port).unset("volume-error");
    let reset = local(&home, &["reset", "--yes"]);
    assert_eq!(reset.code, 0, "{}{}", reset.out, reset.err);
    assert!(reset.out.contains("Local store deleted."), "{}", reset.out);
    assert!(reset.out.contains("Local node ready on"), "{}", reset.out);
    let calls = docker.calls();
    let order: Vec<_> = ["rm -f knaix-local", "volume rm knaix-local-data", "run "]
        .iter()
        .map(|c| calls.iter().position(|call| call.starts_with(c)).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{calls:#?}");
    assert!(
        the_run(&calls).contains("-e LLAMA_MODEL_ID=llama3 "),
        "the model is kept"
    );

    // Never started, so no volume: already the empty store reset wants.
    docker.unset("state");
    record(&home, node.port, json!({}));
    docker.set("volume-error", "Error: No such volume: knaix-local-data");
    let reset = local(&home, &["reset", "--yes"]);
    assert_eq!(reset.code, 0, "{}{}", reset.out, reset.err);
    assert!(!reset.out.contains("Local store deleted."), "{}", reset.out);
    assert!(the_run(&docker.calls()).ends_with(&format!("-e GENERATION_PROVIDER=mock {IMAGE}")));
}

#[test]
fn status_says_whether_the_node_runs_answers_and_with_what() {
    let home = scratch_home("status");
    let docker = docker(&home);
    let node = node();
    let status = |format: &str| run(knaix(&home).args(["-o", format, "local", "status"]));

    let text = status("text");
    assert_eq!(text.code, 0, "{}", text.err);
    assert!(text.out.contains("not created"), "{}", text.out);
    assert!(
        text.out.contains("Start it with knaix local up."),
        "{}",
        text.out
    );
    let empty: Value = serde_json::from_str(&status("json").out).unwrap();
    assert_eq!(
        empty,
        json!({"running": false, "healthy": false, "state": null, "url": null,
               "modelUrl": null, "model": null, "instanceId": null, "image": null})
    );

    docker.running_ours(node.port);
    record(
        &home,
        node.port,
        json!({"model_url": "http://localhost:11434", "model": "llama3"}),
    );
    let full: Value = serde_json::from_str(&status("json").out).unwrap();
    assert_eq!(
        full,
        json!({"running": true, "healthy": true, "state": "running",
               "url": format!("http://127.0.0.1:{}", node.port),
               "modelUrl": "http://localhost:11434", "model": "llama3",
               "instanceId": INSTANCE, "image": IMAGE})
    );
    let text = status("text");
    for expected in [
        "running (healthy)",
        INSTANCE,
        "llama3 at http://localhost:11434",
    ] {
        assert!(
            text.out.contains(expected),
            "missing {expected:?} in:\n{}",
            text.out
        );
    }
    assert!(!text.out.contains("Start it with"), "{}", text.out);

    node.answer("GET /health", 503, "{}");
    record(
        &home,
        node.port,
        json!({"model_url": "http://localhost:11434"}),
    );
    let text = status("text");
    assert!(text.out.contains("running (not ready)"), "{}", text.out);
    let waking: Value = serde_json::from_str(&status("json").out).unwrap();
    assert_eq!(
        (&waking["running"], &waking["healthy"]),
        (&json!(true), &json!(false))
    );
    assert!(
        text.out.contains("model at http://localhost:11434"),
        "{}",
        text.out
    );

    docker.set("state", "exited");
    record(&home, node.port, json!({}));
    let text = status("text");
    assert!(text.out.contains("exited"), "{}", text.out);
    assert!(
        text.out.contains("deterministic mock (retrieval is real)"),
        "{}",
        text.out
    );
    assert!(text.out.contains("Start it with"), "{}", text.out);
}

#[test]
fn logs_show_the_nodes_last_lines_as_text_or_json() {
    let home = scratch_home("logs");
    let docker = docker(&home);

    let none = local(&home, &["logs"]);
    assert_eq!(none.code, 1);
    assert!(none
        .err
        .contains("No local node exists. Start one with 'knaix local up'."));

    docker.running_ours(8080);
    let text = local(&home, &["logs", "--lines", "5"]);
    assert_eq!(text.code, 0, "{}", text.err);
    assert_eq!(text.out, "node booting\nstore open\n");
    assert!(docker
        .calls()
        .contains(&"logs --tail 5 knaix-local".to_string()));

    let json_out = run(knaix(&home).args(["-o", "json", "local", "logs"]));
    let v: Value = serde_json::from_str(&json_out.out).unwrap();
    assert_eq!(v, json!({"lines": ["node booting", "store open"]}));
}

#[test]
fn setup_without_a_terminal_says_how_to_choose_a_model_in_a_script() {
    let home = scratch_home("setup");
    docker(&home);

    let setup = local(&home, &["setup"]);

    assert_eq!(setup.code, 1, "{}", setup.out);
    assert!(
        setup
            .err
            .contains("Setup is interactive. In a script, pass --model-url and --model"),
        "{}",
        setup.err
    );
}

const CONNECT: &str = "POST /api/instances/local/connect";
const TELEMETRY: &str = "POST /api/instances/c-1/telemetry";
const DISCONNECT: &str = "POST /api/instances/c-1/disconnect";

/// `knaix local <args>` signed in to the control plane `node` also plays,
/// with `kill` and `hostname` on the PATH behind the stand-in docker.
fn connected(home: &Path, node: &Node, args: &[&str]) -> Run {
    run(knaix(home)
        .env("PATH", format!("{}:/bin", home.join("bin").display()))
        .env("KNAIX_TOKEN", "sess")
        .env("KNAIX_API_URL", format!("http://127.0.0.1:{}", node.port))
        .arg("local")
        .args(args))
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Kills a relay worker the test started, should the test fail first.
struct Worker(u32);

impl Drop for Worker {
    fn drop(&mut self) {
        if alive(self.0) {
            unsafe { libc::kill(self.0 as i32, libc::SIGKILL) };
        }
    }
}

#[test]
fn connect_is_refused_until_there_is_a_running_node_and_a_session() {
    let home = scratch_home("connect-refused");
    let node = node();
    let refused = |r: Run, code: i32, want: &str| {
        assert_eq!(r.code, code, "{}{}", r.out, r.err);
        assert!(r.err.contains(want), "wanted {want:?}: {}", r.err);
    };

    // Without the /bin `connected` adds: on Linux that is /usr/bin, where a
    // real docker lives.
    refused(local(&home, &["connect"]), 7, "Docker is not installed");
    let docker = docker(&home);
    refused(
        connected(&home, &node, &["connect"]),
        1,
        "No local node. Start one with 'knaix local up'.",
    );
    record(&home, node.port, json!({}));
    refused(
        connected(&home, &node, &["connect"]),
        1,
        "The local node is not running. Start it with 'knaix local up'.",
    );
    docker.running_ours(node.port);
    refused(
        local(&home, &["connect"]),
        3,
        "Connecting a local node needs an account. Run 'knaix login' first.",
    );
    node.answer(CONNECT, 500, r#"{"error":"down"}"#);
    refused(
        connected(&home, &node, &["connect"]),
        1,
        "Register failed (500",
    );
    node.answer(CONNECT, 200, r#"{"data":{}}"#);
    refused(
        connected(&home, &node, &["connect"]),
        1,
        "The control plane did not return a node id.",
    );
    assert_eq!(recorded(&home).unwrap()["remote_id"], Value::Null);

    // The background worker relays a node already connected, and nothing else.
    refused(
        connected(&home, &node, &["connect", "--worker"]),
        1,
        "Local node is not connected.",
    );
    fs::remove_file(home.join(".knaix/local.json")).unwrap();
    refused(
        connected(&home, &node, &["connect", "--worker"]),
        1,
        "No local node to relay.",
    );
}

#[test]
fn connect_in_the_background_relays_until_disconnect_stops_it() {
    let home = scratch_home("connect");
    let docker = docker(&home);
    let node = node();
    docker.running_ours(node.port);
    record(&home, node.port, json!({"model": "llama3"}));
    node.answer(CONNECT, 200, r#"{"data":{"clientId":"c-1"}}"#);
    node.answer(TELEMETRY, 200, "{}");
    node.answer(DISCONNECT, 200, "{}");
    let pushes = || node.calls().iter().filter(|(r, _)| r == TELEMETRY).count();
    let relay_pid = || recorded(&home).unwrap()["relay_pid"].as_u64().unwrap() as u32;

    let up = connected(&home, &node, &["connect", "--daemon"]);

    assert_eq!(up.code, 0, "{}{}", up.out, up.err);
    assert!(
        up.out
            .contains("Connected local to your account; it will appear in your dashboard."),
        "{}",
        up.out
    );
    let first = Worker(relay_pid());
    assert!(
        up.out.contains(&format!(
            "Relaying metrics and logs in the background (pid {}).",
            first.0
        )),
        "{}",
        up.out
    );
    let (route, register) = &node.calls()[0];
    assert_eq!(route, CONNECT);
    assert_eq!(register["localInstanceId"], INSTANCE);
    assert_eq!(register["model"], "llama3");
    assert_eq!(recorded(&home).unwrap()["remote_id"], "c-1");
    wait_for("the worker's first sample", || pushes() > 0);
    let (_, sample) = node
        .calls()
        .into_iter()
        .find(|(r, _)| r == TELEMETRY)
        .unwrap();
    assert_eq!(sample["metrics"]["healthy"], true);
    assert_eq!(sample["logs"], json!(["node booting", "store open"]));

    // Again: one relay replaces the other, never two at once.
    let again = connected(&home, &node, &["connect", "--daemon"]);
    assert_eq!(again.code, 0, "{}", again.err);
    let second = Worker(relay_pid());
    assert_ne!(second.0, first.0);
    wait_for("the first worker to stop", || !alive(first.0));

    let down = connected(&home, &node, &["disconnect"]);
    assert_eq!(down.code, 0, "{}", down.err);
    assert!(
        down.out
            .contains("Local node disconnected from your account."),
        "{}",
        down.out
    );
    wait_for("the second worker to stop", || !alive(second.0));
    assert!(node.calls().iter().any(|(r, _)| r == DISCONNECT));
    let saved = recorded(&home).unwrap();
    assert_eq!(
        (&saved["remote_id"], &saved["relay_pid"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(saved["instance_id"], INSTANCE, "the node itself is kept");

    // Signed out: the relay still stops, but the account is not told.
    record(&home, node.port, json!({"remote_id": "c-1"}));
    let calls = node.calls().len();
    let down = run(knaix(&home)
        .env("PATH", format!("{}:/bin", home.join("bin").display()))
        .env("KNAIX_API_URL", format!("http://127.0.0.1:{}", node.port))
        .args(["local", "disconnect"]));
    assert!(down.out.contains("Local node disconnected"), "{}", down.out);
    assert_eq!(node.calls().len(), calls);
    assert_eq!(recorded(&home).unwrap()["remote_id"], Value::Null);

    // Already disconnected, and then no node at all: nothing to tell the account.
    let calls = node.calls().len();
    let down = connected(&home, &node, &["disconnect"]);
    assert!(down.out.contains("Local node disconnected"), "{}", down.out);
    fs::remove_file(home.join(".knaix/local.json")).unwrap();
    let down = connected(&home, &node, &["disconnect"]);
    assert!(
        down.out.contains("No local node on this machine."),
        "{}",
        down.out
    );
    assert_eq!(node.calls().len(), calls);
}
