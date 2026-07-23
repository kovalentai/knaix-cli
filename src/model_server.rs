//! Finding OpenAI-compatible model servers on this machine.
//!
//! Every serving stack worth supporting -- Ollama, LM Studio, vLLM,
//! llama-server -- answers `GET /v1/models` with the same list shape, so one
//! request both finds a server and learns what it hosts. That is what lets
//! `knaix local setup` offer real choices instead of asking for a URL and a
//! model name typed from memory, where every typo surfaces as a 404 at the
//! first question.

use futures_util::future::join_all;
use serde::Deserialize;
use std::time::Duration;

/// A server that answered, and the models it says it hosts.
pub struct FoundServer {
    pub url: String,
    pub label: String,
    pub models: Vec<String>,
}

/// Where the common stacks listen by default. The node's own port (8080,
/// llama-server's default too) is deliberately absent: a server there
/// conflicts with the node and cannot be offered.
const CANDIDATES: &[(&str, &str)] = &[
    ("http://localhost:11434", "Ollama"),
    ("http://localhost:1234", "LM Studio"),
    ("http://localhost:8000", "vLLM"),
    ("http://localhost:8081", "llama-server"),
];

/// Name a server by the port it answers on. Display only; nothing behaves
/// differently on the guess.
pub fn label_for(url: &str) -> &'static str {
    let port = url::Url::parse(url)
        .ok()
        .and_then(|u| u.port_or_known_default());
    match port {
        Some(11434) => "Ollama",
        Some(1234) => "LM Studio",
        Some(8000) => "vLLM",
        Some(8081) => "llama-server",
        _ => "A model server",
    }
}

/// The list endpoint for a base URL, tolerating a base that already ends in
/// /v1, which is how OpenAI-compatible URLs are usually quoted.
fn models_endpoint(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

#[derive(Deserialize)]
struct ModelList {
    // Not defaulted: a 200 without a `data` array is some other service that
    // happens to live on a probed port, not a model server.
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Model ids from a /v1/models body, or None when the answer is not that shape.
fn parse_models(body: &str) -> Option<Vec<String>> {
    serde_json::from_str::<ModelList>(body)
        .ok()
        .map(|list| list.data.into_iter().map(|m| m.id).collect())
}

/// Ask one URL whether a model server lives there.
pub async fn probe(url: &str, label: &str, timeout: Duration) -> Option<FoundServer> {
    let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
    let resp = client.get(models_endpoint(url)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    Some(FoundServer {
        url: url.trim_end_matches('/').to_string(),
        label: label.to_string(),
        models: parse_models(&body)?,
    })
}

/// Probe the usual ports, plus the remembered URL if there is one, in
/// parallel. The slowest answer bounds the wait, not the sum.
pub async fn discover(remembered: Option<String>) -> Vec<FoundServer> {
    let mut targets: Vec<(String, String)> = Vec::new();
    if let Some(url) = remembered {
        let url = url.trim_end_matches('/').to_string();
        targets.push((url.clone(), label_for(&url).to_string()));
    }
    for (url, label) in CANDIDATES {
        if !targets.iter().any(|(t, _)| t == url) {
            targets.push((url.to_string(), label.to_string()));
        }
    }
    let probes = targets
        .iter()
        .map(|(url, label)| probe(url, label, Duration::from_millis(600)));
    join_all(probes).await.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_endpoint_tolerates_how_base_urls_are_quoted() {
        assert_eq!(
            models_endpoint("http://localhost:11434"),
            "http://localhost:11434/v1/models"
        );
        assert_eq!(
            models_endpoint("http://localhost:11434/"),
            "http://localhost:11434/v1/models"
        );
        // OpenAI-compatible URLs are often quoted with /v1 already on them.
        assert_eq!(
            models_endpoint("http://localhost:1234/v1"),
            "http://localhost:1234/v1/models"
        );
    }

    #[test]
    fn a_model_list_yields_its_ids_in_order() {
        let body = r#"{"object":"list","data":[{"id":"qwen3.5:latest","object":"model"},{"id":"phi4:latest","object":"model"}]}"#;
        assert_eq!(
            parse_models(body).unwrap(),
            vec!["qwen3.5:latest", "phi4:latest"]
        );
    }

    #[test]
    fn an_answer_that_is_not_a_model_list_is_rejected() {
        // Whatever answered is not a model server; offering it would turn the
        // picker's promise -- these choices work -- into a guess.
        assert!(parse_models("<html>hi</html>").is_none());
        assert!(parse_models(r#"{"status":"ok"}"#).is_none());
        // An empty list is still a server, just one with nothing pulled yet.
        assert_eq!(parse_models(r#"{"data":[]}"#).unwrap().len(), 0);
    }

    #[test]
    fn servers_are_named_by_their_port() {
        assert_eq!(label_for("http://localhost:11434"), "Ollama");
        assert_eq!(label_for("http://192.168.1.50:1234"), "LM Studio");
        assert_eq!(label_for("http://localhost:8000"), "vLLM");
        assert_eq!(label_for("http://somewhere:9999"), "A model server");
        assert_eq!(label_for("not a url"), "A model server");
    }
}
