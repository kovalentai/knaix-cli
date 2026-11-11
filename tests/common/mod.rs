//! What the binary tests share: a home of their own, a fake server on loopback
//! that records what it was sent, and a way to run `knaix` as a script would.

// Each test file uses its own share of these.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// A home of its own for one test, under the system temp directory.
pub fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".knaix")).unwrap();
    dir
}

pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

/// One request the fake server received.
#[derive(Clone, Debug)]
pub struct Call {
    pub route: String,
    pub auth: Option<String>,
    /// The body as JSON, or null when it is not.
    pub body: Value,
    /// The body as it came, for one that is not JSON (a multipart upload).
    pub raw: String,
}

/// A server on loopback answering `"METHOD /path"` routes with a status and a
/// body sent as it is: JSON, or an event stream. Anything else is a 404.
pub struct Server {
    pub url: String,
    calls: Arc<Mutex<Vec<Call>>>,
}

pub fn server(routes: Vec<(&str, u16, String)>) -> Server {
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
                raw: String::from_utf8_lossy(&body).into_owned(),
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
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

/// `knaix <args>` signed in to `api`, with no terminal to prompt on.
pub fn knaix(home: &Path, api: &str, args: &[&str]) -> Run {
    knaix_reading(home, api, args, Stdio::null())
}

/// `knaix` with `stdin` as its standard input. Killed, failing the test, if
/// it has not finished in a minute: a command waiting for input a script
/// cannot give is the failure some of these tests look for.
pub fn knaix_reading(home: &Path, api: &str, args: &[&str], stdin: Stdio) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_knaix"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KNAIX_TOKEN", "sess")
        .env("KNAIX_API_URL", api)
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .env_remove("XDG_CONFIG_HOME")
        .current_dir(home)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args)
        .spawn()
        .expect("failed to run knaix");
    // Drained while it runs: a pipe nobody reads fills, and a command that
    // prints more than it holds would stop and read as one that never finished.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("knaix {args:?} did not finish");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    Run {
        code: status.code().expect("killed by a signal"),
        out: out.join().unwrap(),
        err: err.join().unwrap(),
    }
}

pub const INSTANCES: &str = "GET /api/instances";

/// The account's node list, as the control plane answers it.
pub fn nodes(list: Value) -> (&'static str, u16, String) {
    (INSTANCES, 200, json!({ "data": list }).to_string())
}

/// `local.json` for a node served at `url`.
pub fn record_local_node(home: &Path, url: &str) {
    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    let node = json!({"port": port, "instance_id": "inst-1", "image": "img"});
    fs::write(home.join(".knaix/local.json"), node.to_string()).unwrap();
}
