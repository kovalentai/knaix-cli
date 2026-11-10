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
/// A `Null` body sends nothing at all. Any other route answers 404 with no
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
                    let body = if reply.is_null() {
                        String::new()
                    } else {
                        reply.to_string()
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
