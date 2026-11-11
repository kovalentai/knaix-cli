//! Helpers shared by the unit tests of more than one module.

use axum::{
    body::Bytes,
    http::{HeaderMap, Method, StatusCode, Uri},
    Router,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One request the fake control plane received.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub route: String,
    pub auth: Option<String>,
    pub body: Value,
}

/// A control plane on loopback. `url` is its origin; `calls` what it was sent.
pub struct FakeApi {
    pub url: String,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeApi {
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

/// Answer each `"METHOD /path"` in `routes` with its status and JSON body.
/// A `Null` body sends nothing at all, and a string is sent as it is, which is
/// how a test serves an event stream. Any other route answers 404 with no
/// body, as a missing one would.
pub async fn fake_api(routes: Vec<(&str, u16, Value)>) -> FakeApi {
    let routes: HashMap<String, (u16, Value)> = routes
        .into_iter()
        .map(|(r, s, b)| (r.to_string(), (s, b)))
        .collect();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = calls.clone();
    let app = Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| async move {
            let route = format!("{method} {}", uri.path());
            log.lock().unwrap().push(Call {
                route: route.clone(),
                auth: headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string()),
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            });
            match routes.get(&route) {
                Some((status, reply)) => {
                    let body = match reply {
                        Value::Null => String::new(),
                        Value::String(raw) => raw.clone(),
                        json => json.to_string(),
                    };
                    (StatusCode::from_u16(*status).unwrap(), body)
                }
                None => (StatusCode::NOT_FOUND, String::new()),
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FakeApi { url, calls }
}

/// A host serving `files` by path, each with a status and body; anything else
/// answers 404, as a missing key would. Returns its origin.
pub async fn serve_files(files: Vec<(String, u16, Vec<u8>)>) -> String {
    let files: HashMap<String, (u16, Vec<u8>)> =
        files.into_iter().map(|(p, s, b)| (p, (s, b))).collect();
    let app = Router::new().fallback(move |uri: Uri| async move {
        let (status, body) = files.get(uri.path()).cloned().unwrap_or((404, Vec::new()));
        (StatusCode::from_u16(status).unwrap(), body)
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    origin
}

/// A stand-in program at `dir/name`, returned as its path. It answers a
/// version probe (how helpers are asked whether they are installed), records
/// every other call's arguments as one line in `<path>.args`, then runs `body`
/// as shell.
#[cfg(unix)]
pub fn fake_tool(dir: &std::path::Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in --version|version) exit 0;; esac\n\
             echo \"$@\" >> \"$0.args\"\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A thread forking while the file was open for writing makes the first
    // exec fail with "text file busy" until that child execs in turn.
    for _ in 0..100 {
        match Command::new(&path).arg("--version").status() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => break,
        }
    }
    path.to_string_lossy().into_owned()
}

/// The arguments of each call a `fake_tool` received, in order.
#[cfg(unix)]
pub fn args_of(tool: &str) -> Vec<String> {
    std::fs::read_to_string(format!("{tool}.args"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}
