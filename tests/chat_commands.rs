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
