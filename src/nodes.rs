use crate::config::load_config;
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use colored::*;
use crossterm::{cursor, execute};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::header::AUTHORIZATION;
use reqwest::multipart;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio_util::codec::{BytesCodec, FramedRead};
use walkdir::WalkDir;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Node {
    /// The instance UUID. Every knowledge and chat route is keyed by this, so
    /// it is what the CLI resolves to before touching a node's data; the
    /// human-facing `instance_id` and `name` are only ever inputs.
    pub id: Option<String>,
    pub name: String,
    pub state: String,
    pub instance_id: Option<String>,
    pub private_ip: Option<String>,
    pub model: Option<String>,
    pub config: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct DocumentSource {
    pub r#type: Option<String>,
    pub name: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Document {
    pub id: String,
    pub source: Option<DocumentSource>,
    pub chunk_count: Option<u64>,
    pub created_at: Option<String>,
}

/// An answer and the passages it was grounded in. The two travel together so
/// every surface can show its sources, not just the one-shot command.
pub struct ChatAnswer {
    pub text: String,
    pub citations: Vec<Citation>,
    /// The model that produced the answer. A node running the deterministic
    /// mock reports it here, which is the difference between "this model chose
    /// its citations" and "the pipeline cited whatever ranked first".
    pub model: Option<String>,
}

/// One grounded passage the node returned alongside an answer.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Citation {
    pub index: Option<u32>,
    pub content: Option<String>,
    pub source: Option<DocumentSource>,
    pub cited: Option<bool>,
}

/// Holds the HTTP client and configuration shared by every request a single
/// command makes, so repeated calls reuse one connection pool and TLS session.
#[derive(Clone)]
pub struct KnaixContext {
    pub config: crate::config::Config,
    pub client: reqwest::Client,
    pub output_format: String,
}

impl KnaixContext {
    pub fn new(output_format: String) -> Self {
        let config = load_config();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Some(Duration::from_secs(15)))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            client,
            output_format,
        }
    }

    pub fn get_token(&self) -> Result<&String> {
        self.config
            .token
            .as_ref()
            .context("Not logged in. Run 'knaix login' first.")
    }
}

/// Where a command should send its work.
///
/// The two differ in more than a base URL: the local node exposes the intent
/// routes directly and needs no credential, while a hosted instance is reached
/// through the control plane, which authorizes the caller and holds the node's
/// key. Keeping them one type means every data command decides once, at the
/// edge, instead of threading a flag through the middle.
#[derive(Clone, Debug)]
pub enum Target {
    /// The node running on this machine. No account, no token.
    Local { base: String, instance_id: String },
    /// An instance the control plane provisioned and authorizes access to.
    Remote { uuid: String },
}

impl Target {
    /// Identifier to show a user; the local node has no meaningful UUID to them.
    pub fn label(&self) -> String {
        match self {
            Target::Local { .. } => crate::local::LOCAL_NODE_ID.to_string(),
            Target::Remote { uuid } => uuid.clone(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Target::Local { .. })
    }
}

/// Resolve the local node, failing with the command that would start it.
fn local_target() -> Result<Target> {
    let node = crate::local::load().ok_or_else(|| {
        anyhow!(
            "No local node has been started. Run '{}' first.",
            "knaix local up"
        )
    })?;
    Ok(Target::Local {
        base: node.base_url(),
        instance_id: node.instance_id,
    })
}

/// Resolve what a command should act on, local or hosted.
pub async fn resolve_target(
    ctx: &KnaixContext,
    manual_id: Option<String>,
) -> Result<Option<Target>> {
    // The reserved name wins wherever it appears, including as the saved
    // default, so `knaix use local` makes every later command local.
    let named = manual_id
        .clone()
        .or_else(|| ctx.config.default_node_id.clone());
    if named.as_deref() == Some(crate::local::LOCAL_NODE_ID) {
        return local_target().map(Some);
    }

    Ok(resolve_node_id(ctx, manual_id)
        .await?
        .map(|uuid| Target::Remote { uuid }))
}

/// True when a node is the one the user named, by UUID, instance id, or name.
/// Users read the last two off `knaix list`; the routes only accept the first.
fn node_matches(node: &Node, wanted: &str) -> bool {
    node.id.as_deref() == Some(wanted)
        || node.instance_id.as_deref() == Some(wanted)
        || node.name == wanted
}

/// Fetch the caller's nodes once, for resolution and listing alike.
async fn fetch_nodes(ctx: &KnaixContext) -> Result<Vec<Node>> {
    let token = ctx.get_token()?;
    let url = format!("{}/api/instances", ctx.config.api_url);

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to connect to Kovalent API")?;

    if !resp.status().is_success() {
        return Err(anyhow!(
            "Failed to fetch nodes: HTTP {}. Are you logged in?",
            resp.status()
        ));
    }

    let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(serde_json::from_value(wrapper["data"].clone()).unwrap_or_default())
}

pub fn format_file_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

pub async fn list_nodes(ctx: &KnaixContext, node_id: Option<&str>) -> Result<()> {
    let token = ctx.get_token()?;

    if let Some(nid) = node_id {
        // The knowledge base of one node. Resolve first: the route is keyed by
        // the instance UUID, but users pass whatever `knaix list` showed them.
        let uuid = resolve_node_uuid(ctx, nid).await?;
        let url = format!("{}/api/knowledge/{}/documents", ctx.config.api_url, uuid);
        let resp = ctx
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .send()
            .await
            .context("Failed to connect to Kovalent API")?;

        if resp.status().is_success() {
            let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
            let docs_val = &wrapper["data"];

            if ctx.output_format == "json" {
                let json_data = serde_json::to_string_pretty(docs_val).unwrap_or_default();
                println!("{}", json_data);
                return Ok(());
            }

            let docs: Vec<Document> = serde_json::from_value(docs_val.clone()).unwrap_or_default();

            if docs.is_empty() {
                println!("{} No documents found in knowledge base.", "Info:".blue());
            } else {
                println!(
                    "\n{}",
                    format!("Knowledge Base for Node {}:", nid)
                        .bold()
                        .underline()
                );
                let mut table = comfy_table::Table::new();
                table.load_preset(comfy_table::presets::UTF8_FULL);
                table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
                table.set_header(vec!["Name", "Type", "Chunks", "Ingested"]);

                for doc in docs {
                    let source = doc.source.unwrap_or(DocumentSource {
                        r#type: None,
                        name: None,
                    });
                    table.add_row(vec![
                        source.name.unwrap_or_else(|| "N/A".to_string()),
                        source.r#type.unwrap_or_else(|| "N/A".to_string()),
                        doc.chunk_count.unwrap_or(0).to_string(),
                        doc.created_at.unwrap_or_else(|| "N/A".to_string()),
                    ]);
                }
                println!("{table}\n");
            }
        } else {
            return Err(anyhow!("Failed to fetch documents: HTTP {}", resp.status()));
        }
        return Ok(());
    }

    // List Nodes (default behavior)
    let url = format!("{}/api/instances", ctx.config.api_url);

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to connect to Kovalent API")?;

    if resp.status().is_success() {
        let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
        let nodes_val = &wrapper["data"];

        if ctx.output_format == "json" {
            let json_data = serde_json::to_string_pretty(nodes_val).unwrap_or_default();
            println!("{}", json_data);
            return Ok(());
        }

        let nodes: Vec<Node> = serde_json::from_value(nodes_val.clone()).unwrap_or_default();

        if nodes.is_empty() {
            println!(
                "{} No nodes provisioned. Visit {} to provision your first Sovereign Node.",
                "Info:".blue(),
                "app.kovalentai.com".cyan().underline()
            );
        } else {
            println!("\n{}", "Your Kovalent Nodes:".bold().underline());
            let mut table = comfy_table::Table::new();
            table.load_preset(comfy_table::presets::UTF8_FULL);
            table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
            table.set_header(vec!["Name", "ID", "State", "IP", "Type", "Model"]);

            for node in nodes {
                let status = if node.state == "running" {
                    node.state.green()
                } else {
                    node.state.yellow()
                };
                let id_display = node
                    .instance_id
                    .clone()
                    .unwrap_or_else(|| "N/A".to_string());
                let ip_display = node.private_ip.clone().unwrap_or_else(|| "N/A".to_string());

                let mut node_type = "MANAGED".cyan();
                if let Some(cfg) = &node.config {
                    if cfg.get("isByot").and_then(|v| v.as_bool()).unwrap_or(false) {
                        node_type = "SOVEREIGN (BYOT)".yellow();
                    }
                }

                let model = node
                    .model
                    .clone()
                    .unwrap_or_else(|| "Standard".to_string())
                    .magenta();

                table.add_row(vec![
                    node.name.bold().white().to_string(),
                    id_display.cyan().to_string(),
                    status.to_string(),
                    ip_display.blue().to_string(),
                    node_type.to_string(),
                    model.to_string(),
                ]);
            }
            println!("{table}\n");
        }
    } else {
        return Err(anyhow!("Failed to fetch nodes: HTTP {}", resp.status()));
    }
    Ok(())
}

pub async fn select_node_interactively(ctx: &KnaixContext) -> Result<Option<String>> {
    let token = ctx.get_token()?;

    // Show spinner while fetching nodes
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Fetching available nodes...");
    pb.enable_steady_tick(Duration::from_millis(100));

    let url = format!("{}/api/instances", ctx.config.api_url);

    let resp = ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .context("Could not reach Kovalent API")?;

    if !resp.status().is_success() {
        pb.finish_and_clear();
        return Err(anyhow!(
            "Failed to fetch node list (HTTP {}). Are you logged in?",
            resp.status()
        ));
    }

    let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
    let nodes_val = &wrapper["data"];
    let nodes = serde_json::from_value::<Vec<Node>>(nodes_val.clone())
        .context("Failed to parse node list from API")?;

    pb.finish_and_clear();

    if nodes.is_empty() {
        println!(
            "{} No nodes provisioned. Visit {} to provision your first Sovereign Node.",
            "Info:".blue(),
            "app.kovalentai.com".cyan().underline()
        );
        return Ok(None);
    }

    // Auto-select if only one node exists. Report the friendly name but return
    // the UUID, which is what every data route is keyed by.
    if nodes.len() == 1 {
        println!(
            "{} Auto-selected node: {}",
            "Info:".blue(),
            nodes[0].name.cyan()
        );
        return Ok(node_uuid(&nodes[0]).ok());
    }

    // Multiple nodes: show fuzzy selector with cursor hidden for a clean UI
    use dialoguer::{theme::ColorfulTheme, FuzzySelect};

    let items: Vec<String> = nodes
        .iter()
        .map(|n| {
            let status = if n.state == "running" {
                n.state.green().to_string()
            } else {
                n.state.yellow().to_string()
            };
            format!("{} ({})", n.name, status)
        })
        .collect();

    let _ = execute!(std::io::stderr(), cursor::Hide);

    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a node")
        .default(0)
        .items(&items)
        .interact_opt()
        .unwrap_or(None);

    let _ = execute!(std::io::stderr(), cursor::Show);

    if let Some(index) = selection {
        return Ok(node_uuid(&nodes[index]).ok());
    }

    Ok(None)
}

/// The UUID a node's data routes are keyed by, or an error naming the node.
fn node_uuid(node: &Node) -> Result<String> {
    node.id.clone().ok_or_else(|| {
        anyhow!(
            "Node {} has no instance UUID; the control plane is too old for this CLI.",
            node.name
        )
    })
}

/// Turn anything the user can read off `knaix list` -- UUID, instance id, or
/// name -- into the instance UUID the knowledge and chat routes require.
pub async fn resolve_node_uuid(ctx: &KnaixContext, wanted: &str) -> Result<String> {
    let nodes = fetch_nodes(ctx).await?;
    match nodes.iter().find(|n| node_matches(n, wanted)) {
        Some(node) => node_uuid(node),
        None => Err(anyhow!(
            "No node matches '{}'. Run 'knaix list' to see your nodes.",
            wanted
        )),
    }
}

/// Resolves the node to act on, always as an instance UUID.
/// 1. If `manual_id` is Some, resolve exactly that (no failover).
/// 2. Otherwise try the default node from config.
/// 3. If there is no default, or it is gone or not running, offer selection.
pub async fn resolve_node_id(
    ctx: &KnaixContext,
    manual_id: Option<String>,
) -> Result<Option<String>> {
    if let Some(id) = manual_id {
        return resolve_node_uuid(ctx, &id).await.map(Some);
    }

    // No token means nothing to resolve against; go straight to selection,
    // which reports the auth failure in one place.
    if ctx.config.token.is_none() {
        return select_node_interactively(ctx).await;
    }

    if let Some(ref def_id) = ctx.config.default_node_id {
        // Validate the default before committing to it, so a stopped or
        // deleted node prompts instead of failing mid-command.
        if let Ok(nodes) = fetch_nodes(ctx).await {
            match nodes.iter().find(|n| node_matches(n, def_id)) {
                Some(node) if node.state == "running" => return node_uuid(node).map(Some),
                Some(node) => println!(
                    "{} Default node [{}] is {}, falling back to selection...",
                    "Info:".blue(),
                    def_id.cyan(),
                    node.state.yellow()
                ),
                None => println!(
                    "{} Default node [{}] no longer exists, falling back to selection...",
                    "Info:".blue(),
                    def_id.cyan()
                ),
            }
        }
    }

    // Fallback: full interactive selector
    select_node_interactively(ctx).await
}

/// Send one message to a node and stream the grounded answer back.
///
/// Talks to the native chat route, where the whole RAG pipeline runs on the
/// node itself: retrieval, rerank and synthesis happen behind the residency
/// boundary and the response carries the passages the answer was grounded in.
pub async fn chat(
    ctx: &KnaixContext,
    target: &Target,
    message: &str,
    stream_to_stdout: bool,
) -> Result<Option<ChatAnswer>> {
    if let Target::Local { base, instance_id } = target {
        return chat_local(ctx, base, instance_id, message, stream_to_stdout).await;
    }
    let node_uuid = &target.label();
    let token = ctx.get_token()?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars(
                "\u{280b}\u{2819}\u{2839}\u{2838}\u{283c}\u{2834}\u{2826}\u{2827}\u{2807}\u{280f}",
            )
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Thinking...");
    pb.enable_steady_tick(Duration::from_millis(100));

    let url = format!("{}/api/nodes/{}/native-chat", ctx.config.api_url, node_uuid);
    let payload = serde_json::json!({ "message": message, "stream": true });

    let resp = ctx
        .client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
        .inspect_err(|_| pb.finish_and_clear())
        .context("Networking error during chat request")?;

    if !resp.status().is_success() {
        pb.finish_and_clear();
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let detail = body["error"].as_str().unwrap_or("no detail");
        return Err(anyhow!("Chat failed on node: HTTP {} - {}", status, detail));
    }

    let (text, citations, model) = read_chat_stream(resp, &pb, stream_to_stdout).await?;

    if stream_to_stdout {
        print_citations(&citations);
    }

    Ok(Some(ChatAnswer {
        text,
        citations,
        model,
    }))
}

/// Ask the local node directly.
///
/// It answers as one JSON body rather than a stream, so there is nothing to
/// print progressively; the spinner covers the wait and the answer arrives
/// whole. No token is sent because there is no one to authorize against -- the
/// node is on loopback and belongs to whoever is running it.
async fn chat_local(
    ctx: &KnaixContext,
    base: &str,
    instance_id: &str,
    message: &str,
    print: bool,
) -> Result<Option<ChatAnswer>> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars(
                "\u{280b}\u{2819}\u{2839}\u{2838}\u{283c}\u{2834}\u{2826}\u{2827}\u{2807}\u{280f}",
            )
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Thinking...");
    pb.enable_steady_tick(Duration::from_millis(100));

    let resp = ctx
        .client
        .post(format!("{}/api/query/answer", base))
        .json(&serde_json::json!({ "instance_id": instance_id, "query": message }))
        .send()
        .await
        .inspect_err(|_| pb.finish_and_clear())
        .context("Could not reach the local node. Is it running? Try 'knaix local status'.")?;

    pb.finish_and_clear();

    if !resp.status().is_success() {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let detail = body["message"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("no detail");
        return Err(anyhow!(
            "Local node could not answer: HTTP {} - {}",
            status,
            detail
        ));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let text = body["answer"].as_str().unwrap_or_default().to_string();
    let citations = node_citations(&body["citations"], &text);
    let model = body["model"].as_str().map(|s| s.to_string());

    if print {
        println!("{} {}", "AI:".cyan().bold(), text);
        print_citations(&citations);
    }

    Ok(Some(ChatAnswer {
        text,
        citations,
        model,
    }))
}

/// Map the node's citation shape onto the CLI's.
///
/// The node keeps provenance under `metadata`, where its store put it, and does
/// not mark which passages the answer used -- the control plane derives that
/// from its own stream. Answering directly, the only record of what was used is
/// the answer text, so read the `[n]` markers back out of it. A passage the
/// answer never referenced is context the model saw, not a source it cited, and
/// showing the two alike would overstate what the answer rests on.
fn node_citations(raw: &serde_json::Value, answer: &str) -> Vec<Citation> {
    let referenced = referenced_indexes(answer);
    raw.as_array()
        .map(|items| {
            items
                .iter()
                .map(|c| {
                    let index = c["index"].as_u64().map(|n| n as u32);
                    Citation {
                        index,
                        content: c["content"].as_str().map(|s| s.to_string()),
                        source: serde_json::from_value(c["metadata"]["source"].clone()).ok(),
                        cited: Some(index.map(|i| referenced.contains(&i)).unwrap_or(false)),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The citation markers an answer actually used, as in "... [1] ... [3]".
fn referenced_indexes(answer: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let bytes: Vec<char> = answer.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '[' {
            let mut j = i + 1;
            let mut digits = String::new();
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                digits.push(bytes[j]);
                j += 1;
            }
            if !digits.is_empty() && j < bytes.len() && bytes[j] == ']' {
                if let Ok(n) = digits.parse::<u32>() {
                    if !found.contains(&n) {
                        found.push(n);
                    }
                }
                i = j;
            }
        }
        i += 1;
    }
    found
}

/// Consume the server-sent event stream, printing tokens as they land.
///
/// The stream carries three kinds of frame: a `meta` event holding the
/// citations, bare `data:` frames holding one token each, and a terminating
/// `done` (or `error`) event. Tokens are printed as they arrive so a long
/// answer feels responsive; the citations are held back and rendered after.
async fn read_chat_stream(
    resp: reqwest::Response,
    pb: &ProgressBar,
    stream_to_stdout: bool,
) -> Result<(String, Vec<Citation>, Option<String>)> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut answer = String::new();
    let mut citations: Vec<Citation> = Vec::new();
    let mut model: Option<String> = None;
    // Which citations the answer actually referenced. It arrives at the end, in
    // the `done` frame, rather than on the citations themselves.
    let mut cited_indexes: Vec<u32> = Vec::new();
    let mut event = String::new();
    let mut first_token = true;
    let mut stream_error: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Chat stream ended unexpectedly")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Frames are newline-delimited; keep any partial tail for the next chunk.
        while let Some(newline) = buffer.find('\n') {
            let line = buffer[..newline].trim_end_matches('\r').to_string();
            buffer.drain(..=newline);

            if let Some(name) = line.strip_prefix("event: ") {
                event = name.trim().to_string();
                continue;
            }
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let parsed: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match event.as_str() {
                "meta" => {
                    citations =
                        serde_json::from_value(parsed["citations"].clone()).unwrap_or_default();
                    model = parsed["model"].as_str().map(|s| s.to_string());
                }
                "error" => {
                    stream_error = Some(
                        parsed["error"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string(),
                    );
                }
                "done" => {
                    cited_indexes = parsed["citedIndexes"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_u64().map(|n| n as u32))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                _ => {
                    if let Some(token) = parsed["token"].as_str() {
                        if stream_to_stdout {
                            // Clear the spinner only once the first token is in
                            // hand, so the line it occupied is reused.
                            if first_token {
                                pb.finish_and_clear();
                                print!("{} ", "AI:".cyan().bold());
                            }
                            print!("{}", token);
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        }
                        first_token = false;
                        answer.push_str(token);
                    }
                }
            }
            // Every frame here carries its whole payload on one `data:` line, so
            // the event ends with it. Leaving the name set would make the next
            // token frame read as another `meta`.
            event.clear();
        }
    }

    pb.finish_and_clear();
    if stream_to_stdout && !first_token {
        println!();
    }

    if let Some(code) = stream_error {
        return Err(anyhow!("Chat failed on node: {}", code));
    }

    // Retrieval returns candidates; only some end up referenced. Mark the ones
    // the answer actually used, so the CLI shows sources rather than every
    // passage the node happened to consider.
    for citation in citations.iter_mut() {
        let referenced = citation
            .index
            .map(|i| cited_indexes.contains(&i))
            .unwrap_or(false);
        citation.cited = Some(referenced);
    }

    Ok((answer, citations, model))
}

/// Render the passages an answer was grounded in, so a claim can be checked
/// against the node's own corpus rather than taken on trust.
pub fn print_citations(citations: &[Citation]) {
    let cited: Vec<&Citation> = citations
        .iter()
        .filter(|c| c.cited.unwrap_or(false))
        .collect();
    if cited.is_empty() {
        return;
    }

    println!("\n{}", "Grounded in:".dimmed());
    for citation in cited {
        let index = citation.index.unwrap_or(0);
        let name = citation
            .source
            .as_ref()
            .and_then(|s| s.name.clone())
            .unwrap_or_else(|| "unknown source".to_string());
        let snippet = citation.content.clone().unwrap_or_default();
        let snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
        let snippet: String = if snippet.chars().count() > 160 {
            format!("{}...", snippet.chars().take(160).collect::<String>())
        } else {
            snippet
        };
        println!("  {} {}", format!("[{}]", index).cyan(), name.dimmed());
        println!("      {}", snippet);
    }
    println!();
}

/// Chat without touching stdout, for the REPL's background summarizer.
pub async fn chat_silent(
    config: crate::config::Config,
    node_uuid: String,
    message: String,
) -> Result<Option<String>> {
    let token = config.token.clone().context("Not logged in")?;
    let client = reqwest::Client::new();
    let url = format!("{}/api/nodes/{}/native-chat", config.api_url, node_uuid);

    let resp = client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .json(&serde_json::json!({ "message": message }))
        .send()
        .await
        .context("Networking error during chat request")?;

    if !resp.status().is_success() {
        return Err(anyhow!("Chat failed on node: HTTP {}", resp.status()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(body["answer"].as_str().map(|s| s.to_string()))
}

pub async fn upload_single_file(
    ctx: &KnaixContext,
    target: &Target,
    path: &Path,
    file_name: &str,
) -> Result<()> {
    if let Target::Local { base, instance_id } = target {
        return upload_local(ctx, base, instance_id, path, file_name).await;
    }
    let node_id = &target.label();
    let token = ctx.get_token()?;
    let file_size = tokio::fs::metadata(path)
        .await
        .context("Could not read file metadata")?
        .len();

    println!(
        "\n  {} {} ({})",
        "Uploading".cyan(),
        file_name.bold().white(),
        format_file_size(file_size).dimmed()
    );

    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "  {spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec})",
            )
            .unwrap()
            .progress_chars("=>-"),
    );
    pb.enable_steady_tick(Duration::from_millis(100));

    let file = File::open(path).await.context("Could not open file")?;
    let raw_stream = FramedRead::new(file, BytesCodec::new());

    let pb_clone = pb.clone();
    let tracked_stream = raw_stream.map(move |chunk_result| {
        chunk_result.map(|bytes| {
            pb_clone.inc(bytes.len() as u64);
            bytes.freeze()
        })
    });

    let body = reqwest::Body::wrap_stream(tracked_stream);
    let part = multipart::Part::stream_with_length(body, file_size)
        .file_name(file_name.to_string())
        .mime_str("application/octet-stream")
        .unwrap();

    let form = multipart::Form::new().part("file", part);
    // Native ingest: the control plane parses and chunks, and the chunks land
    // in the node's own store rather than a workspace on the node's runtime.
    let url = format!("{}/api/knowledge/{}/documents", ctx.config.api_url, node_id);

    match ctx
        .client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .multipart(form)
        .send()
        .await
    {
        Ok(resp) => {
            pb.finish_and_clear();
            let status = resp.status();
            let data: serde_json::Value = resp.json().await.unwrap_or_default();

            if status.is_success() && data["success"].as_bool().unwrap_or(false) {
                let chunks = data["data"]["chunkCount"].as_u64().unwrap_or(0);
                println!(
                    "  {} {} ingested ({} chunk{}).",
                    "✓".green(),
                    file_name.white(),
                    chunks,
                    if chunks == 1 { "" } else { "s" }
                );
                Ok(())
            } else {
                let err = data["error"].as_str().unwrap_or("Unknown error");
                Err(anyhow!("Upload failed: HTTP {} - {}", status, err))
            }
        }
        Err(e) => {
            pb.finish_and_clear();
            Err(e).context("Networking error during upload")
        }
    }
}

/// Send one file to the local node, which parses, chunks and embeds it itself.
///
/// The bytes go as base64 in a JSON body rather than multipart: the node's
/// ingest intent takes one shape for text, a file, or a URL, and a CLI that
/// spoke a second wire format for the same operation would be a second thing to
/// keep in step.
async fn upload_local(
    ctx: &KnaixContext,
    base: &str,
    instance_id: &str,
    path: &Path,
    file_name: &str,
) -> Result<()> {
    let bytes = tokio::fs::read(path)
        .await
        .context("Could not read the file")?;
    println!(
        "\n  {} {} ({})",
        "Ingesting".cyan(),
        file_name.bold().white(),
        format_file_size(bytes.len() as u64).dimmed()
    );

    let encoded = BASE64.encode(&bytes);
    let resp = ctx
        .client
        .post(format!("{}/api/kb/ingest", base))
        .json(&serde_json::json!({
            "instance_id": instance_id,
            "content_base64": encoded,
            "filename": file_name,
        }))
        .send()
        .await
        .context("Could not reach the local node. Is it running? Try 'knaix local status'.")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        let detail = body["message"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("unknown error");
        return Err(anyhow!("Ingest failed: HTTP {} - {}", status, detail));
    }

    let chunks = body["chunkCount"].as_u64().unwrap_or(0);
    println!(
        "  {} {} ingested ({} chunk{}), embedded by {}.",
        "\u{2713}".green(),
        file_name.white(),
        chunks,
        if chunks == 1 { "" } else { "s" },
        body["embeddingProvider"]
            .as_str()
            .unwrap_or("the node")
            .dimmed()
    );
    Ok(())
}

pub async fn upload(ctx: &KnaixContext, target: &Target, file_path: &str) -> Result<()> {
    let base_path = Path::new(file_path);
    if !base_path.exists() {
        return Err(anyhow!("Path not found: {}", file_path));
    }

    if base_path.is_dir() {
        println!(
            "{} Uploading directory: {}",
            "Info:".blue(),
            file_path.bold()
        );
        for entry in WalkDir::new(base_path).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() {
                let file_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or("file")
                    .to_string();
                upload_single_file(ctx, target, path, &file_name).await?;
            }
        }
    } else {
        let file_name = base_path
            .file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("file")
            .to_string();
        upload_single_file(ctx, target, base_path, &file_name).await?;
    }

    Ok(())
}

pub async fn get_metrics(ctx: &KnaixContext, node_id: &str) -> Result<()> {
    let token = ctx.get_token()?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message(format!("Fetching metrics for {}...", node_id.cyan()));
    pb.enable_steady_tick(Duration::from_millis(100));

    let url = format!("{}/api/nodes/{}/health", ctx.config.api_url, node_id);

    match ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();

                if ctx.output_format == "json" {
                    pb.finish_and_clear();
                    let json_data = serde_json::to_string_pretty(&data).unwrap_or_default();
                    println!("{}", json_data);
                    return Ok(());
                }

                pb.finish_and_clear();
                println!("\n{}", "Node Health & Metrics:".bold().underline());

                let mut table = comfy_table::Table::new();
                table.load_preset(comfy_table::presets::UTF8_FULL);
                table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
                table.set_header(vec!["Metric", "Value"]);

                table.add_row(vec![
                    "Node ID".dimmed().to_string(),
                    data["nodeId"]
                        .as_str()
                        .unwrap_or(node_id)
                        .white()
                        .to_string(),
                ]);
                table.add_row(vec![
                    "Name".dimmed().to_string(),
                    data["nodeName"]
                        .as_str()
                        .unwrap_or("Unknown")
                        .cyan()
                        .to_string(),
                ]);

                let healthy = data["healthy"].as_bool().unwrap_or(false);
                let status_str = if healthy {
                    "HEALTHY".green()
                } else {
                    "UNHEALTHY".red()
                };
                table.add_row(vec!["Status".dimmed().to_string(), status_str.to_string()]);

                if let Some(latency) = data["latencyMs"].as_f64() {
                    table.add_row(vec![
                        "Latency".dimmed().to_string(),
                        format!("{}ms", latency).blue().to_string(),
                    ]);
                }

                table.add_row(vec![
                    "Checked".dimmed().to_string(),
                    data["checkedAt"]
                        .as_str()
                        .unwrap_or("N/A")
                        .dimmed()
                        .to_string(),
                ]);

                println!("{table}\n");
                Ok(())
            } else {
                pb.finish_and_clear();
                Err(anyhow!("Failed to fetch metrics: HTTP {}", resp.status()))
            }
        }
        Err(e) => {
            pb.finish_and_clear();
            Err(e).context("Networking error during metrics request")
        }
    }
}

pub async fn get_logs(ctx: &KnaixContext, node_id: &str, lines: usize) -> Result<()> {
    let token = ctx.get_token()?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message(format!(
        "Fetching last {} lines from {}...",
        lines,
        node_id.cyan()
    ));
    pb.enable_steady_tick(Duration::from_millis(100));

    let url = format!(
        "{}/api/nodes/{}/logs?limit={}",
        ctx.config.api_url, node_id, lines
    );

    match ctx
        .client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(resp) => {
            pb.finish_and_clear();
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();

                println!(
                    "\n{}",
                    format!("--- Logs: {} (last {}) ---", node_id, lines).dimmed()
                );

                if let Some(rows) = data["logs"].as_array() {
                    for row in rows {
                        if let Some(msg) = row.as_str() {
                            println!("{}", msg);
                        }
                    }
                } else if let Some(msg) = data["logs"].as_str() {
                    println!("{}", msg);
                } else {
                    println!("{}", "No logs available.".yellow());
                }

                println!("{}", "--- End ---".dimmed());
                Ok(())
            } else {
                Err(anyhow!("Failed to fetch logs: HTTP {}", resp.status()))
            }
        }
        Err(e) => {
            pb.finish_and_clear();
            Err(e).context("Networking error during logs request")
        }
    }
}

pub async fn up(ctx: &KnaixContext) -> Result<()> {
    let token = ctx.get_token()?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Requesting compute...");
    pb.enable_steady_tick(Duration::from_millis(100));

    let url = format!("{}/api/instances", ctx.config.api_url);
    // Send no tier or deployment mode: the control plane derives both from the
    // caller's subscription, and a hardcoded tier here would silently ask for
    // the wrong placement on any account that is not Community.
    let payload = serde_json::json!({});

    match ctx
        .client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        // Provisioning pulls an image and boots a container, which routinely
        // outlasts the client's default timeout. Timing out here abandons a
        // request the control plane is still working on, so the node appears
        // minutes later with no sign the command created it.
        .timeout(Duration::from_secs(300))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();

                // The provisioning response reports status, not an identifier,
                // so name the node only when one actually came back rather than
                // inventing a placeholder and telling the user to look it up.
                let instance_id = data
                    .get("data")
                    .and_then(|d| d.get("instanceId"))
                    .and_then(|i| i.as_str());

                match instance_id {
                    Some(id) => pb.finish_with_message(format!(
                        "{} Node {} requested.",
                        "✓".green(),
                        id.cyan()
                    )),
                    None => pb.finish_with_message(format!("{} Node requested.", "✓".green())),
                }
                match instance_id {
                    Some(id) => println!(
                        "  Boot takes a few minutes. Check on it with {} or {}.",
                        "knaix list".cyan(),
                        format!("knaix metrics {}", id).cyan()
                    ),
                    None => println!(
                        "  Boot takes a few minutes. Run {} to see it come up.",
                        "knaix list".cyan()
                    ),
                }
            } else {
                let status = resp.status();
                pb.finish_and_clear();
                let err_msg = resp.json::<serde_json::Value>().await.unwrap_or_default();
                let err_str = err_msg["error"].as_str().unwrap_or("Unknown error");
                return Err(anyhow!(
                    "Failed to provision node: HTTP {} - {}",
                    status,
                    err_str
                ));
            }
        }
        Err(e) => {
            pb.finish_and_clear();
            return Err(e).context("Networking error during provisioning request");
        }
    }

    Ok(())
}

pub async fn memorize(ctx: &KnaixContext, node_id: &str, fact: &str, durable: bool) -> Result<()> {
    let home_dir = home::home_dir().unwrap_or_else(|| Path::new(".").to_path_buf());
    let mem_dir = home_dir.join(".knaix").join("memory").join(node_id);
    tokio::fs::create_dir_all(&mem_dir)
        .await
        .context("Failed to create memory directory")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&mem_dir, std::fs::Permissions::from_mode(0o700))
            .await
            .ok();
    }

    let file_name = if durable {
        "_knaix_durable_memory.md"
    } else {
        "_knaix_ephemeral_log.md"
    };
    let file_path = mem_dir.join(file_name);

    if !file_path.exists() {
        tokio::fs::write(&file_path, "# Sovereign Agentic Memory\n\n").await?;
    }

    let timestamp_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entry = format!("- [TS {}]: {}\n", timestamp_secs, fact);

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        file.set_permissions(perms).await.ok();
    }

    file.write_all(entry.as_bytes()).await?;

    let config_clone = ctx.config.clone();
    let nid = node_id.to_string();
    let fpath = file_path.clone();
    let fname = file_name.to_string();

    tokio::spawn(async move {
        let _ = upload_single_file_silent(config_clone, nid, fpath, fname).await;
    });

    Ok(())
}

pub async fn upload_single_file_silent(
    config: crate::config::Config,
    node_id: String,
    path: std::path::PathBuf,
    file_name: String,
) -> Result<()> {
    let token = config.token.context("Not logged in")?;
    let file_size = tokio::fs::metadata(&path)
        .await
        .context("Could not read file metadata")?
        .len();

    let file = File::open(&path).await.context("Could not open file")?;
    let raw_stream = FramedRead::new(file, BytesCodec::new());

    let body = reqwest::Body::wrap_stream(raw_stream);
    let part = multipart::Part::stream_with_length(body, file_size)
        .file_name(file_name.clone())
        .mime_str("application/octet-stream")
        .unwrap();

    let form = multipart::Form::new().part("file", part);
    let url = format!("{}/api/knowledge/{}/documents", config.api_url, node_id);
    let client = reqwest::Client::new();

    let resp = client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .multipart(form)
        .send()
        .await?;

    if resp.status().is_success() {
        let data: serde_json::Value = resp.json().await.unwrap_or_default();
        if data["success"].as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Upload failed: {}",
                data["error"].as_str().unwrap_or("Unknown")
            ))
        }
    } else {
        Err(anyhow::anyhow!("HTTP {}", resp.status()))
    }
}

pub async fn view_memory(_ctx: &KnaixContext, node_id: &str, file: Option<&str>) -> Result<()> {
    let home_dir = home::home_dir().unwrap_or_else(|| std::path::Path::new(".").to_path_buf());
    let memory_dir = home_dir.join(".knaix").join("memory").join(node_id);

    if !memory_dir.exists() {
        println!(
            "{} No memory data found for node {}.",
            "Info:".blue(),
            node_id.bold()
        );
        return Ok(());
    }

    if let Some(file_name) = file {
        let file_path = memory_dir.join(file_name);

        if !file_path.exists() {
            println!(
                "{} File {} does not exist for node {}.",
                "Error:".red(),
                file_name.bold(),
                node_id.bold()
            );
            return Ok(());
        }

        let contents = tokio::fs::read_to_string(&file_path)
            .await
            .context("Failed to read memory file")?;

        println!(
            "\n{} {} | {}: {}\n",
            "●".magenta(),
            "Sovereign Memory".bold(),
            "Node".dimmed(),
            node_id.bold()
        );

        let skin = termimad::MadSkin::default_dark();
        skin.print_text(&contents);

        println!("\n{}", "--- End of Memory ---".dimmed());
    } else {
        println!(
            "\n{} {} | {}: {}",
            "●".magenta(),
            "Sovereign Memory".bold(),
            "Node".dimmed(),
            node_id.bold()
        );
        println!(
            "{}\n",
            "Use `knaix memory --file <filename>` to read a file.".dimmed()
        );

        let mut entries = tokio::fs::read_dir(&memory_dir).await?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    files.push(fname.to_string());
                }
            }
        }

        if files.is_empty() {
            println!("  No files found.");
        } else {
            files.sort();
            for f in files {
                println!("  📄 {}", f.cyan());
            }
        }
        println!();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citation_markers_are_read_out_of_the_answer() {
        // Answering the node directly, the text is the only record of which
        // passages were actually used.
        assert_eq!(referenced_indexes("Grounded in [1] and [3]."), vec![1, 3]);
        assert_eq!(referenced_indexes("no markers here"), Vec::<u32>::new());
        // Repeats collapse; a passage cited twice is still one source.
        assert_eq!(referenced_indexes("[2] then [2] again"), vec![2]);
        // Not a marker.
        assert_eq!(referenced_indexes("[abc] [ 1 ] [1"), Vec::<u32>::new());
    }

    #[test]
    fn node_citations_hoist_source_and_mark_only_what_was_used() {
        let raw = serde_json::json!([
            { "index": 1, "content": "used passage",
              "metadata": { "source": { "name": "a.md", "type": "text" } } },
            { "index": 2, "content": "unused passage",
              "metadata": { "source": { "name": "b.md", "type": "text" } } }
        ]);
        let cites = node_citations(&raw, "Answer grounded in [1].");

        assert_eq!(cites.len(), 2);
        // Provenance lives under metadata on the node; the CLI reads it flat.
        assert_eq!(
            cites[0].source.as_ref().unwrap().name.as_deref(),
            Some("a.md")
        );
        assert_eq!(cites[0].cited, Some(true));
        // Retrieved but never referenced: context, not a source.
        assert_eq!(cites[1].cited, Some(false));
    }

    #[test]
    fn a_local_target_is_addressed_directly_and_labelled_local() {
        let t = Target::Local {
            base: "http://127.0.0.1:8080".into(),
            instance_id: "abc".into(),
        };
        assert!(t.is_local());
        assert_eq!(t.label(), "local");

        let r = Target::Remote { uuid: "u-1".into() };
        assert!(!r.is_local());
        assert_eq!(r.label(), "u-1");
    }
}
