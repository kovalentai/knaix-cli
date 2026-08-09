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
    pub models: Vec<ModelInfo>,
}

/// One model a server lists, with whatever it will say about the weights.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Size of the weights on disk, where the server reports it. Only Ollama
    /// does; everywhere else this is None and the model is offered unjudged.
    pub size_bytes: Option<u64>,
    /// False for a model the server lists but cannot serve from this machine.
    pub local: bool,
}

impl ModelInfo {
    /// A model named but not measured, which is every server except Ollama.
    pub fn unmeasured(id: String) -> Self {
        Self {
            id,
            size_bytes: None,
            local: true,
        }
    }
}

/// Below this, an Ollama entry is a pointer to a hosted model rather than
/// weights on this disk: the manifest is a few hundred bytes and there is
/// nothing to run. Listing one as a choice is how a first run ends in a 403 on
/// every question.
const SMALLEST_REAL_WEIGHTS: u64 = 1 << 20;

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

/// Ollama's own listing, which unlike /v1/models carries the size of the
/// weights. Everything here is best effort: a server that is not Ollama
/// answers 404 and the models stay unmeasured.
#[derive(Deserialize)]
struct TagList {
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
    size: Option<u64>,
}

/// Sizes by model name from an /api/tags body.
fn parse_tags(body: &str) -> Vec<(String, Option<u64>)> {
    serde_json::from_str::<TagList>(body)
        .map(|list| list.models.into_iter().map(|m| (m.name, m.size)).collect())
        .unwrap_or_default()
}

/// Merge what /api/tags knows into the ids /v1/models gave.
fn merge_tags(ids: Vec<String>, tags: &[(String, Option<u64>)]) -> Vec<ModelInfo> {
    ids.into_iter()
        .map(|id| match tags.iter().find(|(name, _)| *name == id) {
            Some((_, size)) => ModelInfo {
                id,
                size_bytes: *size,
                local: size.is_none_or(|b| b >= SMALLEST_REAL_WEIGHTS),
            },
            None => ModelInfo::unmeasured(id),
        })
        .collect()
}

/// Ask one URL whether a model server lives there.
pub async fn probe(url: &str, label: &str, timeout: Duration) -> Option<FoundServer> {
    let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
    let resp = client.get(models_endpoint(url)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let ids = parse_models(&body)?;

    // Ollama alone reports weight sizes, and they are what tells a model that
    // fits this machine from one that will swap, and real weights from a
    // pointer to a hosted model.
    let base = url.trim_end_matches('/').trim_end_matches("/v1");
    let tags = match client.get(format!("{base}/api/tags")).send().await {
        Ok(r) if r.status().is_success() => parse_tags(&r.text().await.unwrap_or_default()),
        _ => Vec::new(),
    };

    Some(FoundServer {
        url: url.trim_end_matches('/').to_string(),
        label: label.to_string(),
        models: merge_tags(ids, &tags),
    })
}

/// Why a model could not answer, in the words the picker shows.
pub enum GenerationCheck {
    Works,
    Failed(String),
}

/// Ask a model for one token, to learn whether it answers at all before the
/// choice is saved. A server can list a model it cannot run -- a hosted entry
/// with no weights here is the common one -- and without this the first
/// question is where that surfaces.
pub async fn check_generation(
    url: &str,
    model: Option<&str>,
    timeout: Duration,
) -> GenerationCheck {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return GenerationCheck::Works;
    };
    let base = url.trim_end_matches('/').trim_end_matches("/v1");
    let mut body = serde_json::json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 1,
        "stream": false,
    });
    if let Some(m) = model {
        body["model"] = serde_json::Value::String(m.to_string());
    }

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => GenerationCheck::Works,
        Ok(r) => {
            let status = r.status();
            let detail = r.text().await.ok().and_then(|b| server_message(&b));
            GenerationCheck::Failed(match detail {
                Some(m) => format!("HTTP {}: {}", status.as_u16(), m),
                None => format!("HTTP {}", status.as_u16()),
            })
        }
        // A timeout here is not a verdict: a model that has to be loaded from
        // disk can outlast the check and still answer fine once warm.
        Err(e) if e.is_timeout() => GenerationCheck::Works,
        Err(_) => GenerationCheck::Failed("the server could not be reached".to_string()),
    }
}

/// The human-readable half of an OpenAI-shaped error body.
fn server_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let msg = v
        .get("error")
        .and_then(|e| e.get("message").or(Some(e)))
        .or_else(|| v.get("message"))?;
    let text = msg.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
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
    fn tag_sizes_are_read_and_attached_to_the_listed_ids() {
        // Shapes taken from a live Ollama: /v1/models names the models,
        // /api/tags is the only place the weights are measured.
        let tags = parse_tags(
            r#"{"models":[
                {"name":"gemma4:latest","size":9608350718},
                {"name":"qwen3.5:latest","size":6594474711}
            ]}"#,
        );
        let merged = merge_tags(
            vec!["gemma4:latest".to_string(), "qwen3.5:latest".to_string()],
            &tags,
        );
        assert_eq!(merged[0].size_bytes, Some(9_608_350_718));
        assert_eq!(merged[1].size_bytes, Some(6_594_474_711));
        assert!(merged.iter().all(|m| m.local));
    }

    #[test]
    fn a_model_hosted_elsewhere_is_marked_unrunnable() {
        // An Ollama Cloud entry is listed like any other but is a manifest of a
        // few hundred bytes with nothing to run behind it. Offered as a choice,
        // it answers every question with a 403.
        let tags =
            parse_tags(r#"{"models":[{"name":"gemini-3-flash-preview:latest","size":367}]}"#);
        let merged = merge_tags(vec!["gemini-3-flash-preview:latest".to_string()], &tags);
        assert!(!merged[0].local);
    }

    #[test]
    fn a_server_that_reports_no_sizes_leaves_every_model_offerable() {
        // Only Ollama answers /api/tags. Everywhere else the list is empty, and
        // treating "unmeasured" as "unrunnable" would empty the picker.
        let merged = merge_tags(vec!["local-model".to_string()], &[]);
        assert!(merged[0].local);
        assert_eq!(merged[0].size_bytes, None);
        assert!(parse_tags("<html>404</html>").is_empty());
    }

    #[test]
    fn a_refusal_is_quoted_back_in_the_servers_own_words() {
        assert_eq!(
            server_message(
                r#"{"error":{"message":"ollama cloud is disabled: remote model is unavailable"}}"#
            )
            .unwrap(),
            "ollama cloud is disabled: remote model is unavailable"
        );
        assert_eq!(
            server_message(r#"{"error":"model not found"}"#).unwrap(),
            "model not found"
        );
        assert!(server_message("not json").is_none());
        assert!(server_message(r#"{"error":{"message":"  "}}"#).is_none());
    }

    /// Against a real Ollama, which CI does not have. Run it by hand after
    /// touching discovery:
    ///
    ///     cargo test --  --ignored discovery_against_a_live_ollama
    #[tokio::test]
    #[ignore]
    async fn discovery_against_a_live_ollama() {
        let found = probe("http://localhost:11434", "Ollama", Duration::from_secs(5))
            .await
            .expect("no Ollama on 11434");
        assert!(!found.models.is_empty(), "nothing pulled to judge");
        // Whatever is pulled, a model that is really here has real weights.
        for m in found.models.iter().filter(|m| m.local) {
            assert!(
                m.size_bytes.is_none_or(|b| b >= SMALLEST_REAL_WEIGHTS),
                "{} passed as local with {:?} bytes",
                m.id,
                m.size_bytes
            );
        }
        // And one that is really here answers.
        if let Some(m) = found.models.iter().find(|m| m.local) {
            let verdict = check_generation(
                "http://localhost:11434",
                Some(&m.id),
                Duration::from_secs(120),
            )
            .await;
            if let GenerationCheck::Failed(why) = verdict {
                panic!("{} is pulled here but did not answer: {why}", m.id);
            }
        }

        // The other half of the promise: a model the picker sets aside really
        // is one that cannot answer.
        if let Some(m) = found.models.iter().find(|m| !m.local) {
            let verdict = check_generation(
                "http://localhost:11434",
                Some(&m.id),
                Duration::from_secs(30),
            )
            .await;
            assert!(
                matches!(verdict, GenerationCheck::Failed(_)),
                "{} was set aside but answers fine",
                m.id
            );
        }
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
