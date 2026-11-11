//! `knaix chat`, run the way a script runs it: through the binary, against a
//! fake control plane or a fake local node on loopback, with no terminal.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-chat-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".knaix")).unwrap();
    dir
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

/// One request the fake server received.
#[derive(Clone, Debug)]
struct Call {
    route: String,
    auth: Option<String>,
    body: Value,
}

/// A server on loopback answering `"METHOD /path"` routes with a status and a
/// body sent as it is: JSON, or an event stream. Anything else is a 404.
struct Server {
    url: String,
    calls: Arc<Mutex<Vec<Call>>>,
}

fn server(routes: Vec<(&str, u16, String)>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let routes: HashMap<String, (u16, String)> = routes
        .into_iter()
        .map(|(r, s, b)| (r.to_string(), (s, b)))
        .collect();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let route = line.split(' ').take(2).collect::<Vec<_>>().join(" ");
            let (mut auth, mut length) = (None, 0usize);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(": ").unwrap_or((header, ""));
                match name.to_ascii_lowercase().as_str() {
                    "authorization" => auth = Some(value.to_string()),
                    "content-length" => length = value.parse().unwrap(),
                    _ => {}
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(Call {
                route: route.clone(),
                auth,
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            });
            let (status, reply) = routes.get(&route).cloned().unwrap_or((404, String::new()));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    Server { url, calls }
}

impl Server {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

/// `knaix <args>` signed in to `api`, with no terminal to prompt on. Killed,
/// failing the test, if it has not finished in a minute: a command waiting for
/// input a script cannot give is the failure some of these tests look for.
fn knaix(home: &Path, api: &str, args: &[&str]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_knaix"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KNAIX_TOKEN", "sess")
        .env("KNAIX_API_URL", api)
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .env_remove("XDG_CONFIG_HOME")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()
        .expect("failed to run knaix");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("knaix {args:?} did not finish");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code().expect("killed by a signal"),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const INSTANCES: &str = "GET /api/instances";

fn nodes(list: Value) -> (&'static str, u16, String) {
    (INSTANCES, 200, json!({ "data": list }).to_string())
}

#[test]
fn with_several_nodes_and_no_default_a_script_is_told_to_name_one() {
    let home = scratch_home("several");
    let api = server(vec![nodes(json!([
        {"id": "u-1", "name": "alpha", "state": "running"},
        {"id": "u-2", "name": "beta", "state": "running"},
    ]))]);

    let chat = knaix(&home, &api.url, &["chat", "what is due?"]);

    assert_eq!(chat.code, 2, "{}{}", chat.out, chat.err);
    assert!(
        chat.err.contains(
            "Choose a node to ask: you have 2 (alpha, beta). Pass -n <name>, or run 'knaix use <name>' to make one the default."
        ),
        "{}",
        chat.err
    );
    for call in api.calls() {
        assert_eq!(call.route, INSTANCES, "only the list was asked for");
        assert_eq!(call.auth.as_deref(), Some("Bearer sess"));
        assert_eq!(call.body, Value::Null);
    }
}

const CHAT: &str = "POST /api/nodes/u-1/native-chat";

fn alpha() -> (&'static str, u16, String) {
    nodes(json!([{"id": "u-1", "name": "alpha", "state": "running"}]))
}

/// An event stream: each frame on its own lines, ended by a blank one.
fn sse(frames: &[(&str, Value)]) -> String {
    frames
        .iter()
        .map(|(event, data)| match *event {
            "" => format!("data: {data}\n\n"),
            event => format!("event: {event}\ndata: {data}\n\n"),
        })
        .collect()
}

fn hosted_answer(model: &str) -> String {
    sse(&[
        (
            "meta",
            json!({
                "citations": [
                    {"index": 1, "content": "Receipts are due within 30 days of travel.", "source": {"name": "policy.md"}},
                    {"index": 2, "content": "Cabin class is economy.", "source": {"name": "travel.md"}},
                ],
                "model": model,
                "conversationId": "c-9",
                "scope": {"chunksUsed": 3, "chunksTotal": 10},
            }),
        ),
        ("", json!({"token": "Within 30 days [1]."})),
        ("done", json!({"citedIndexes": [1]})),
    ])
}

#[test]
fn a_hosted_answer_prints_with_what_it_was_grounded_in() {
    let home = scratch_home("hosted");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer("mock"))]);

    let chat = knaix(
        &home,
        &api.url,
        &[
            "chat",
            "-n",
            "alpha",
            "--brief",
            "--k",
            "5",
            "when are receipts due?",
        ],
    );

    assert_eq!(chat.code, 0, "{}", chat.err);
    for expected in [
        "--k applies to a local node; a hosted node's retrieval is set by the control plane.",
        "AI: Within 30 days [1].",
        "Grounded in:",
        "[1] policy.md",
        "Receipts are due within 30 days of travel.",
        "Grounded in the first 3 of 10 passages; the rest did not fit.",
        "Mock answer: the wording is synthetic; retrieval and citations are real. No model is configured on this node.",
    ] {
        assert!(chat.out.contains(expected), "missing {expected:?} in:\n{}", chat.out);
    }
    assert!(!chat.out.contains("travel.md"), "retrieved but not cited");
    let asked = api.calls().into_iter().find(|c| c.route == CHAT).unwrap();
    assert_eq!(asked.auth.as_deref(), Some("Bearer sess"));
    assert_eq!(
        asked.body,
        json!({"message": "when are receipts due?", "stream": true, "verbosity": "brief"})
    );
}

#[test]
fn a_hosted_answer_as_json_carries_everything_a_script_needs() {
    let home = scratch_home("hosted-json");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer("gpt-x"))]);

    let chat = knaix(
        &home,
        &api.url,
        &["-o", "json", "chat", "-n", "alpha", "--detailed", "when?"],
    );

    assert_eq!(chat.code, 0, "{}", chat.err);
    let answer: Value = serde_json::from_str(&chat.out).unwrap();
    assert_eq!(answer["answer"], "Within 30 days [1].");
    assert_eq!(answer["model"], "gpt-x");
    assert_eq!(answer["conversationId"], "c-9");
    assert_eq!(answer["scope"], json!({"chunksUsed": 3, "chunksTotal": 10}));
    assert_eq!(answer["citations"][0]["cited"], true);
    assert_eq!(answer["citations"][1]["cited"], false);
    assert!(answer["firstTokenMs"].is_u64() && answer["totalMs"].is_u64());
    let asked = api.calls().into_iter().find(|c| c.route == CHAT).unwrap();
    assert_eq!(asked.body["verbosity"], "detailed");
}

#[test]
fn a_hosted_refusal_exits_with_its_code_and_scoping_is_for_a_local_node() {
    let home = scratch_home("hosted-refused");
    let api = server(vec![
        alpha(),
        (CHAT, 403, json!({"error": "no access"}).to_string()),
    ]);

    let refused = knaix(&home, &api.url, &["chat", "-n", "alpha", "q"]);
    assert_eq!(refused.code, 3, "{}", refused.err);
    assert!(
        refused
            .err
            .contains("Chat failed on node: HTTP 403 Forbidden - no access"),
        "{}",
        refused.err
    );

    let scoped = knaix(
        &home,
        &api.url,
        &["chat", "-n", "alpha", "--doc", "policy", "q"],
    );
    assert_eq!(scoped.code, 2, "{}", scoped.err);
    assert!(
        scoped.err.contains("--doc applies to a local node"),
        "{}",
        scoped.err
    );

    let deep = knaix(&home, &api.url, &["chat", "-n", "alpha", "--k", "0", "q"]);
    assert_eq!(deep.code, 2, "{}", deep.err);
}

/// `local.json` for a node served at `url`.
fn record_local_node(home: &Path, url: &str) {
    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    let node = json!({"port": port, "instance_id": "inst-1", "image": "img"});
    fs::write(home.join(".knaix/local.json"), node.to_string()).unwrap();
}

const STREAM: &str = "POST /api/query/answer/stream";
const DOCUMENTS: &str = "POST /api/kb/documents";

fn local_answer() -> String {
    sse(&[
        (
            "meta",
            json!({
                "citations": [
                    {"index": 1, "content": "Pets are welcome.", "metadata": {"source": {"name": "_knaix_durable_memory.md"}}},
                    {"index": 2, "content": "Checkout is at 11.", "metadata": {"source": {"name": "house.md"}}},
                ],
                "model": "mock",
                "scope": {"chunksUsed": 4, "chunksTotal": 4},
            }),
        ),
        (
            "",
            json!({"token": "Checkout is at 11 [2], pets welcome [1]."}),
        ),
        ("done", json!({})),
    ])
}

#[test]
fn a_local_answer_scoped_to_a_document_reads_that_document() {
    let home = scratch_home("local");
    let node = server(vec![
        (STREAM, 200, local_answer()),
        (
            DOCUMENTS,
            200,
            json!({"documents": [
                {"document_id": "d-1", "source": "house.md"},
                {"document_id": "d-2", "source": "menu.md"},
            ]})
            .to_string(),
        ),
    ]);
    record_local_node(&home, &node.url);
    let question = home.join("question.txt");
    fs::write(&question, "when is checkout?").unwrap();

    let chat = Command::new(env!("CARGO_BIN_EXE_knaix"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("KNAIX_TOKEN")
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .current_dir(&home)
        .stdin(fs::File::open(&question).unwrap())
        .args([
            "chat", "-n", "local", "--doc", "house", "--doc", "house.md", "--k", "3", "-",
        ])
        .output()
        .unwrap();

    let out = String::from_utf8_lossy(&chat.stdout);
    assert!(
        chat.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&chat.stderr)
    );
    for expected in [
        "--k has no effect with --doc",
        "AI: Checkout is at 11 [2], pets welcome [1].",
        "[1] your saved note (/remember)",
        "[2] house.md",
        "Mock answer: the wording is synthetic; retrieval and citations are real. 'knaix local setup' connects a model.",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    assert!(
        !out.contains("Grounded in the first"),
        "the whole document fitted"
    );
    let asked = node
        .calls()
        .into_iter()
        .find(|c| c.route == STREAM)
        .unwrap();
    assert_eq!(
        asked.body["query"], "when is checkout?",
        "read from standard input"
    );
    assert_eq!(asked.body["instance_id"], "inst-1");
    assert_eq!(
        asked.body["policy"]["document_ids"],
        json!(["d-1"]),
        "named twice, asked once: {}",
        asked.body
    );
}

#[test]
fn a_local_answer_as_json_has_no_thread_and_a_down_node_is_an_error() {
    let home = scratch_home("local-json");
    let node = server(vec![(STREAM, 200, local_answer())]);
    record_local_node(&home, &node.url);

    let chat = knaix(
        &home,
        "http://127.0.0.1:9",
        &["-o", "json", "chat", "-n", "local", "q"],
    );
    assert_eq!(chat.code, 0, "{}", chat.err);
    let answer: Value = serde_json::from_str(&chat.out).unwrap();
    assert_eq!(answer["conversationId"], Value::Null);
    assert_eq!(answer["model"], "mock");
    assert_eq!(answer["citations"][1]["cited"], true);

    record_local_node(&home, "http://127.0.0.1:9");
    let down = knaix(&home, "http://127.0.0.1:9", &["chat", "-n", "local", "q"]);
    assert_eq!(down.code, 4, "unavailable: {}", down.out);
    assert!(
        down.err.contains("Could not reach the local node."),
        "{}",
        down.err
    );
}
