use crate::config::load_config;
use crate::upload_filter::{SkipReason, UploadFilter};
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
use std::path::PathBuf;
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

#[derive(Deserialize, serde::Serialize, Debug, Clone)]
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

/// One prior turn of a conversation, sent to the local node so a follow-up
/// ("what about the exceptions?") is answered in the context of what came
/// before rather than as a fresh, contextless question.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

/// How long an answer the local node is asked for. The difference is only in
/// the system prompt; retrieval and citations are the same at every level.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Verbosity {
    /// One or two sentences, the essential point only.
    Brief,
    /// A direct answer plus the supporting detail worth having.
    #[default]
    Normal,
    /// Everything the context supports, organized.
    Detailed,
}

/// The grounding instructions the local node answers under when a command does
/// not supply its own. The direct `/api/query/answer` route applies no default
/// of its own, so without this the model is handed an empty system prompt and
/// tends to return a single terse line. This lives on the CLI because it is the
/// one path that drives the node directly; the hosted path is prompted by the
/// control plane in front of it. The `shape` clause is the only part that moves
/// with verbosity; grounding, citation, and formatting rules are constant.
fn answer_system(verbosity: Verbosity) -> String {
    let grounding =
        "You are a helpful assistant answering questions from a private knowledge base. \
Answer using only the provided context. Cite the passages you draw on with their [n] markers. \
If the context does not contain the answer, say so plainly rather than guessing.";
    let shape = match verbosity {
        Verbosity::Brief => "Answer in one or two sentences, the essential point only.",
        Verbosity::Normal => {
            "Lead with a direct answer, then add the supporting detail, conditions, and exceptions the context contains."
        }
        Verbosity::Detailed => {
            "Give a thorough answer: the direct answer, then all the relevant detail, conditions, exceptions, and caveats the context contains, organized clearly."
        }
    };
    let formatting =
        "Keep formatting simple for a terminal: short paragraphs, and at most a single flat \
level of '- ' bullets with no nesting and no indented lines.";
    format!("{grounding} {shape} {formatting}")
}

/// Char budget for the REPL transcript sent as history. Roughly 1500 tokens, so
/// a long session does not grow the request body without bound; the node caps
/// the turn count on its side as well. Whole exchanges are dropped, oldest
/// first, so the model never sees a half a turn.
pub const HISTORY_CHAR_BUDGET: usize = 6000;

fn history_chars(history: &[ChatTurn]) -> usize {
    history.iter().map(|t| t.content.len()).sum()
}

/// Drop whole oldest user/assistant pairs until the transcript fits the budget,
/// always keeping at least the most recent exchange so a follow-up still has its
/// immediate context even when a single turn is unusually long.
fn trim_history(history: &mut Vec<ChatTurn>, char_budget: usize) {
    while history.len() > 2 && history_chars(history) > char_budget {
        history.drain(0..2);
    }
}

/// Build the body for the local answer route in one place, so the system prompt
/// and history it sends can be checked by a test without a live node.
fn build_local_answer_body(
    instance_id: &str,
    message: &str,
    history: &[ChatTurn],
    verbosity: Verbosity,
) -> serde_json::Value {
    serde_json::json!({
        "instance_id": instance_id,
        "query": message,
        "system": answer_system(verbosity),
        "history": history,
    })
}

/// Append a finished exchange to a running transcript, then trim the oldest
/// turns so the history stays within `char_budget`.
pub fn record_turn(history: &mut Vec<ChatTurn>, user: &str, assistant: &str, char_budget: usize) {
    history.push(ChatTurn {
        role: "user".to_string(),
        content: user.to_string(),
    });
    history.push(ChatTurn {
        role: "assistant".to_string(),
        content: assistant.to_string(),
    });
    trim_history(history, char_budget);
}

/// One grounded passage the node returned alongside an answer.
#[derive(Deserialize, serde::Serialize, Debug, Clone, Default)]
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
        .context("Could not reach the Kovalent API")?;

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
            .context("Could not reach the Kovalent API")?;

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
        .context("Could not reach the Kovalent API")?;

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
                "{} No hosted nodes yet. {} provisions one on your account; {} runs one on this machine with no account.",
                "Info:".blue(),
                "knaix up".cyan(),
                "knaix local up".cyan()
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
        .context("Could not reach the Kovalent API")?;

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
            "{} No hosted nodes yet. {} provisions one on your account; {} runs one on this machine with no account.",
            "Info:".blue(),
            "knaix up".cyan(),
            "knaix local up".cyan()
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
    history: &[ChatTurn],
    verbosity: Verbosity,
) -> Result<Option<ChatAnswer>> {
    if let Target::Local { base, instance_id } = target {
        return chat_local(
            ctx,
            base,
            instance_id,
            message,
            stream_to_stdout,
            history,
            verbosity,
        )
        .await;
    }
    // The hosted path is prompted and, where supported, kept in session by the
    // control plane; the history and verbosity that shape the local node's
    // system prompt do not apply to it.
    let _ = (history, verbosity);
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
        // An answer takes as long as the model takes; the client default that
        // protects quick lookups would cut off a long generation mid-stream.
        .timeout(Duration::from_secs(300))
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
    history: &[ChatTurn],
    verbosity: Verbosity,
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

    let body = build_local_answer_body(instance_id, message, history, verbosity);

    let resp = ctx
        .client
        .post(format!("{}/api/query/answer/stream", base))
        // First question after a start can include a model server loading its
        // weights, which routinely outlasts the client's default timeout.
        .timeout(Duration::from_secs(300))
        .json(&body)
        .send()
        .await
        .inspect_err(|_| pb.finish_and_clear())
        .context("Could not reach the local node. Is it running? Try 'knaix local status'.")?;

    // A node image built before streaming has no such route. Rather than fail
    // against an older local node, answer through the blocking endpoint it does
    // have: the reserved `local` node upgrades on its own schedule, not the CLI's.
    if matches!(resp.status().as_u16(), 404 | 405) {
        return chat_local_blocking(ctx, base, &body, &pb, print).await;
    }

    // Any other non-2xx never reaches the event stream: it is a normal JSON body.
    if !resp.status().is_success() {
        pb.finish_and_clear();
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

    let (text, citations, model) = read_local_answer_stream(resp, &pb, print).await?;

    Ok(Some(ChatAnswer {
        text,
        citations,
        model,
    }))
}

/// The pre-streaming path: one blocking request that returns the whole answer.
///
/// Kept for a local node whose image predates the streaming route, so a CLI
/// ahead of the node still answers. The request body is identical; only the
/// endpoint and the one-shot parse differ.
async fn chat_local_blocking(
    ctx: &KnaixContext,
    base: &str,
    body: &serde_json::Value,
    pb: &ProgressBar,
    print: bool,
) -> Result<Option<ChatAnswer>> {
    let resp = ctx
        .client
        .post(format!("{}/api/query/answer", base))
        .timeout(Duration::from_secs(300))
        .json(body)
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

/// Consume the local node's SSE answer stream.
///
/// The stream carries a `meta` frame with the citations and model, then a
/// `data:` frame per token, then a terminating `done` (or `error`). Tokens
/// print as they arrive when `print` is set, so a long answer feels responsive;
/// the REPL passes `print` false and accumulates the whole answer to render as
/// markdown. Citations are mapped from the node's shape at the end, and which
/// ones the answer used is read back out of its `[n]` markers, exactly as the
/// non-streamed local path did.
async fn read_local_answer_stream(
    resp: reqwest::Response,
    pb: &ProgressBar,
    print: bool,
) -> Result<(String, Vec<Citation>, Option<String>)> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut answer = String::new();
    let mut raw_citations = serde_json::Value::Null;
    let mut model: Option<String> = None;
    let mut event = String::new();
    let mut first_token = true;
    let mut stream_error: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Answer stream ended unexpectedly")?;
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
                    raw_citations = parsed["citations"].clone();
                    model = parsed["model"].as_str().map(|s| s.to_string());
                }
                "error" => {
                    stream_error = Some(
                        parsed["message"]
                            .as_str()
                            .or_else(|| parsed["error"].as_str())
                            .unwrap_or("unknown error")
                            .to_string(),
                    );
                }
                "done" => {}
                _ => {
                    if let Some(token) = parsed["token"].as_str() {
                        if print {
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
            // Each frame carries its whole payload on one `data:` line, so the
            // event ends with it; leaving the name set would misread the next.
            event.clear();
        }
    }

    pb.finish_and_clear();
    if print && !first_token {
        println!();
    }

    if let Some(code) = stream_error {
        return Err(anyhow!("Local node could not answer: {}", code));
    }

    let citations = node_citations(&raw_citations, &answer);
    if print {
        print_citations(&citations);
    }
    Ok((answer, citations, model))
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

/// True when the deterministic mock wrote the answer.
///
/// The local node reports the mock explicitly or omits the model entirely; a
/// hosted node that omits it is merely not saying, which must not be read as
/// "mock".
fn is_mock_answer(target: &Target, model: Option<&str>) -> bool {
    match model {
        Some("mock") => true,
        None => target.is_local(),
        Some(_) => false,
    }
}

/// One dim line under an answer whose wording came from the mock, so a
/// transcript never reads as a model's work. Retrieval and citations are real
/// either way; only the prose is synthetic.
pub fn print_answer_footer(target: &Target, answer: &ChatAnswer) {
    if !is_mock_answer(target, answer.model.as_deref()) {
        return;
    }
    let hint = if target.is_local() {
        "'knaix local setup' connects a model."
    } else {
        "No model is configured on this node."
    };
    println!(
        "{}",
        format!(
            "Mock answer: the wording is synthetic; retrieval and citations are real. {}",
            hint
        )
        .dimmed()
    );
}

/// The answer as one JSON object, for scripting. Citations carry their
/// `cited` flag rather than being pre-filtered, so a consumer can choose
/// between "what the answer used" and "what the node considered".
pub fn print_answer_json(answer: &ChatAnswer) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "answer": answer.text,
            "model": answer.model,
            "citations": answer.citations,
        }))?
    );
    Ok(())
}

/// How a citation names its source in the "Grounded in" list.
///
/// A `/remember` note is stored as a file in the corpus so retrieval can cite
/// it like any document. Shown by its internal filename it means nothing to the
/// reader -- they asked about their README, not a file called
/// `_knaix_durable_memory.md` -- so name it for what it is: their own saved
/// note. The index is left untouched, so the `[n]` the answer refers to still
/// lines up.
fn citation_source_name(citation: &Citation) -> String {
    let raw = citation
        .source
        .as_ref()
        .and_then(|s| s.name.clone())
        .unwrap_or_else(|| "unknown source".to_string());
    if raw == NOTES_FILE {
        "your saved note (/remember)".to_string()
    } else {
        raw
    }
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
        let name = citation_source_name(citation);
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

pub async fn upload_single_file(
    ctx: &KnaixContext,
    target: &Target,
    path: &Path,
    file_name: &str,
) -> Result<u64> {
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
        // Sized to the upload, not the default: a large document on a slow
        // link outlasts the 30 seconds that protect quick lookups.
        .timeout(Duration::from_secs(600))
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
                Ok(chunks)
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
) -> Result<u64> {
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
        // Parsing and embedding a large document can outlast the default.
        .timeout(Duration::from_secs(600))
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
    Ok(chunks)
}

/// What a directory upload did, so the summary can be specific.
#[derive(Default)]
pub struct UploadSummary {
    pub ingested: usize,
    pub chunks: u64,
    pub skipped: Vec<(String, SkipReason)>,
    /// Directories never descended into. Their contents are never walked, so
    /// there is no file count to give -- naming the directories is both cheaper
    /// and more useful than a number.
    pub pruned_dirs: Vec<String>,
    pub failed: Vec<(String, String)>,
}

pub struct UploadOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub all: bool,
    pub dry_run: bool,
}

pub async fn upload(
    ctx: &KnaixContext,
    target: &Target,
    file_path: &str,
    opts: &UploadOptions,
) -> Result<()> {
    let base_path = Path::new(file_path);
    if !base_path.exists() {
        return Err(anyhow!("Path not found: {}", file_path));
    }

    let filter = UploadFilter::new(&opts.include, &opts.exclude, opts.all)?;

    if !base_path.is_dir() {
        // A named file is uploaded because it was named. Filters describe how
        // to search a directory, not permission to ignore an explicit request.
        let file_name = file_name_of(base_path);
        if opts.dry_run {
            println!("  {} {}", "would ingest".dimmed(), file_name);
            return Ok(());
        }
        return upload_single_file(ctx, target, base_path, &file_name)
            .await
            .map(|_| ());
    }

    println!("{} Scanning {}", "Info:".blue(), file_path.bold());

    // Collect first so the count is known before uploading, which is what
    // makes "3 of 12" possible and lets --dry-run report without sending.
    let mut queue: Vec<PathBuf> = Vec::new();
    let mut summary = UploadSummary::default();

    let pruned = std::cell::RefCell::new(Vec::new());
    let walker = WalkDir::new(base_path).into_iter().filter_entry(|e| {
        if !e.file_type().is_dir() || e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_str().unwrap_or_default();
        if filter.should_enter(name) {
            true
        } else {
            pruned.borrow_mut().push(name.to_string());
            false
        }
    });

    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let relative = path.strip_prefix(base_path).unwrap_or(path);
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

        match filter.verdict(relative, size) {
            Some(reason) => summary
                .skipped
                .push((relative.display().to_string(), reason)),
            None => queue.push(path.to_path_buf()),
        }
    }

    summary.pruned_dirs = pruned.into_inner();
    summary.pruned_dirs.sort();
    summary.pruned_dirs.dedup();

    if opts.dry_run {
        report_dry_run(&queue, &summary, base_path);
        return Ok(());
    }

    if queue.is_empty() {
        println!(
            "{} Nothing to ingest. {} file(s) were skipped; pass {} to see why, or {} to send everything.",
            "Info:".blue(),
            summary.skipped.len(),
            "--dry-run".cyan(),
            "--all".cyan()
        );
        return Ok(());
    }

    let total = queue.len();
    for (i, path) in queue.iter().enumerate() {
        let file_name = file_name_of(path);
        println!(
            "  {} {} of {}",
            "[".dimmed(),
            i + 1,
            format!("{}]", total).dimmed()
        );
        // One bad file must not abandon the rest: a partial ingest that stops
        // wherever it happened to fail is worse than a complete one with a
        // named failure, because nothing says how far it got.
        match upload_single_file(ctx, target, path, &file_name).await {
            Ok(chunks) => {
                summary.ingested += 1;
                summary.chunks += chunks;
            }
            Err(e) => {
                println!("  {} {}: {}", "✗".red(), file_name, e);
                summary.failed.push((file_name, e.to_string()));
            }
        }
    }

    report_summary(&summary);

    if summary.failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} of {} file(s) failed to ingest",
            summary.failed.len(),
            total
        ))
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("file")
        .to_string()
}

fn report_dry_run(queue: &[PathBuf], summary: &UploadSummary, base: &Path) {
    println!(
        "\n{}",
        format!("Would ingest {} file(s):", queue.len()).bold()
    );
    for path in queue.iter().take(50) {
        let rel = path.strip_prefix(base).unwrap_or(path);
        println!("  {} {}", "+".green(), rel.display());
    }
    if queue.len() > 50 {
        println!("  {} and {} more", "+".green(), queue.len() - 50);
    }

    if !summary.pruned_dirs.is_empty() {
        println!(
            "\n{} {}",
            "Not descended into:".dimmed(),
            summary.pruned_dirs.join(", ").dimmed()
        );
    }

    if !summary.skipped.is_empty() {
        // Grouped, because a hundred lines of "unsupported type" teaches less
        // than one line saying a hundred were.
        let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for (_, reason) in &summary.skipped {
            *counts.entry(reason.describe()).or_default() += 1;
        }
        println!(
            "\n{}",
            format!("Skipping {} file(s):", summary.skipped.len()).dimmed()
        );
        for (reason, count) in counts {
            println!("  {} {} ({})", "-".dimmed(), reason.dimmed(), count);
        }
    }
    println!();
}

fn report_summary(summary: &UploadSummary) {
    println!(
        "\n{} Ingested {} file(s), {} chunk(s).",
        "✓".green(),
        summary.ingested,
        summary.chunks
    );
    if !summary.skipped.is_empty() {
        println!(
            "  {} {} skipped (run with {} to see which).",
            "-".dimmed(),
            summary.skipped.len(),
            "--dry-run".cyan()
        );
    }
    for (name, err) in &summary.failed {
        println!("  {} {}: {}", "✗".red(), name, err);
    }
    println!();
}

/// Health and latency for the local node, measured against it directly.
///
/// The control plane's metrics route reports what *it* can see of a hosted
/// node. There is no control plane here, and the node is one hop away on
/// loopback, so the honest number is the one measured from this machine.
async fn get_local_metrics(ctx: &KnaixContext, base: &str) -> Result<()> {
    let url = format!("{}/health", base);
    let started = std::time::Instant::now();
    let resp = ctx.client.get(&url).send().await.with_context(|| {
        format!(
            "Could not reach the local node at {}. Is it running? Try 'knaix local status'.",
            base
        )
    })?;
    let latency = started.elapsed().as_millis();

    let healthy = resp.status().is_success();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();

    if ctx.output_format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "node": crate::local::LOCAL_NODE_ID,
                "url": base,
                "healthy": healthy,
                "latencyMs": latency,
                "ready": body["ready"],
                "binding": body["binding"],
                "tier": body["tier"],
                "peers": body["peers"],
            }))?
        );
        return Ok(());
    }

    println!("\n{}", "Local node health:".bold().underline());
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec!["Metric", "Value"]);
    table.add_row(vec![
        "Status".dimmed().to_string(),
        if healthy && body["ready"].as_bool().unwrap_or(false) {
            "HEALTHY".green().to_string()
        } else {
            "NOT READY".yellow().to_string()
        },
    ]);
    table.add_row(vec!["URL".dimmed().to_string(), base.cyan().to_string()]);
    table.add_row(vec![
        "Latency".dimmed().to_string(),
        format!("{}ms", latency).blue().to_string(),
    ]);
    table.add_row(vec![
        "Binding".dimmed().to_string(),
        body["binding"].as_str().unwrap_or("unknown").to_string(),
    ]);
    table.add_row(vec![
        "Tier".dimmed().to_string(),
        body["tier"].as_str().unwrap_or("unknown").to_string(),
    ]);
    table.add_row(vec![
        "Peers".dimmed().to_string(),
        body["peers"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
            .to_string(),
    ]);
    println!("{table}\n");
    Ok(())
}

pub async fn get_metrics_for(ctx: &KnaixContext, target: &Target) -> Result<()> {
    match target {
        Target::Local { base, .. } => get_local_metrics(ctx, base).await,
        Target::Remote { uuid } => get_metrics(ctx, uuid).await,
    }
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

pub async fn get_logs_for(ctx: &KnaixContext, target: &Target, lines: usize) -> Result<()> {
    match target {
        // The node is a container on this machine; its logs are docker's, and
        // going through a control plane to reach them would be absurd.
        Target::Local { .. } => crate::local::logs(lines),
        Target::Remote { uuid } => get_logs(ctx, uuid, lines).await,
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

/// The notes file a `/remember` lands in. One file per node, appended to.
const NOTES_FILE: &str = "_knaix_durable_memory.md";

/// Append a fact to the target's notes file under ~/.knaix/memory.
/// Returns the file written, so the caller can say where the note went.
pub async fn save_note(target: &Target, fact: &str) -> Result<PathBuf> {
    let home_dir = home::home_dir().unwrap_or_else(|| Path::new(".").to_path_buf());
    let mem_dir = home_dir
        .join(".knaix")
        .join("memory")
        .join(memory_key(target));
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

    let file_path = mem_dir.join(NOTES_FILE);
    if !file_path.exists() {
        tokio::fs::write(&file_path, "# Notes saved from the Knaix REPL\n\n").await?;
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
    Ok(file_path)
}

/// Send the notes file to the node's knowledge base without narrating, so a
/// saved note is retrievable by later questions rather than only sitting on
/// disk. The caller reports the outcome; failing silently would let "saved"
/// mean less than it says.
pub async fn ingest_note(ctx: &KnaixContext, target: &Target, path: &Path) -> Result<()> {
    match target {
        Target::Local { base, instance_id } => {
            let bytes = tokio::fs::read(path)
                .await
                .context("Could not read the notes file")?;
            let resp = ctx
                .client
                .post(format!("{}/api/kb/ingest", base))
                .json(&serde_json::json!({
                    "instance_id": instance_id,
                    "content_base64": BASE64.encode(&bytes),
                    "filename": NOTES_FILE,
                }))
                .send()
                .await
                .context("Could not reach the local node")?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(anyhow!("HTTP {}", resp.status()))
            }
        }
        Target::Remote { uuid } => {
            upload_single_file_silent(
                ctx.config.clone(),
                uuid.clone(),
                path.to_path_buf(),
                NOTES_FILE.to_string(),
            )
            .await
        }
    }
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

/// Directory name a target's memory is filed under.
///
/// Memory has always been local files; the control plane was only ever asked
/// for a name to put on the directory. The local node has a name already.
pub fn memory_key(target: &Target) -> String {
    target.label()
}

pub async fn view_memory(_ctx: &KnaixContext, node_id: &str, file: Option<&str>) -> Result<()> {
    let home_dir = home::home_dir().unwrap_or_else(|| std::path::Path::new(".").to_path_buf());
    let memory_dir = home_dir.join(".knaix").join("memory").join(node_id);

    if !memory_dir.exists() {
        println!(
            "{} No notes saved for node {}. In the REPL, {} saves one.",
            "Info:".blue(),
            node_id.bold(),
            "/remember <fact>".cyan()
        );
        return Ok(());
    }

    if let Some(file_name) = file {
        let file_path = memory_dir.join(file_name);

        if !file_path.exists() {
            return Err(anyhow!(
                "No file named {} for node {}. Run 'knaix memory' to list them.",
                file_name,
                node_id
            ));
        }

        let contents = tokio::fs::read_to_string(&file_path)
            .await
            .context("Failed to read memory file")?;

        println!(
            "\n{} Notes for node {} ({})\n",
            "●".magenta(),
            node_id.bold(),
            file_path.display().to_string().dimmed()
        );

        let skin = termimad::MadSkin::default_dark();
        skin.print_text(&contents);

        println!("\n{}", "--- End ---".dimmed());
    } else {
        println!(
            "\n{} Notes for node {} ({})",
            "●".magenta(),
            node_id.bold(),
            memory_dir.display().to_string().dimmed()
        );
        println!("{}\n", "knaix memory --file <filename> reads one.".dimmed());

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
                println!("  {} {}", "-".dimmed(), f.cyan());
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
    fn the_local_answer_body_carries_the_system_prompt_and_history() {
        // The route applies no default system prompt, so an empty one is what
        // makes the model answer in a single terse line. The body must always
        // carry grounding instructions and the turns so far.
        let history = vec![
            ChatTurn {
                role: "user".into(),
                content: "when are receipts due?".into(),
            },
            ChatTurn {
                role: "assistant".into(),
                content: "Within 30 days [1].".into(),
            },
        ];
        let body = build_local_answer_body("abc", "and cabin class?", &history, Verbosity::Normal);
        assert_eq!(body["instance_id"], "abc");
        assert_eq!(body["query"], "and cabin class?");
        let system = body["system"].as_str().unwrap();
        assert!(!system.is_empty(), "system prompt must not be empty");
        assert!(
            system.contains("[n]"),
            "the prompt should ask the model to cite: {system}"
        );
        assert_eq!(body["history"].as_array().unwrap().len(), 2);
        assert_eq!(body["history"][0]["role"], "user");
        assert_eq!(body["history"][1]["content"], "Within 30 days [1].");
    }

    fn citation_named(name: &str) -> Citation {
        Citation {
            source: Some(DocumentSource {
                r#type: None,
                name: Some(name.to_string()),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_remember_note_is_named_as_the_readers_own_note() {
        // Shown by its internal filename, a saved note reads as noise the user
        // never uploaded; it should be named for what it is instead.
        let note = citation_named(NOTES_FILE);
        assert_eq!(citation_source_name(&note), "your saved note (/remember)");
    }

    #[test]
    fn an_uploaded_documents_name_is_left_alone() {
        let doc = citation_named("README.md");
        assert_eq!(citation_source_name(&doc), "README.md");
    }

    #[test]
    fn a_sourceless_citation_falls_back_to_a_label() {
        assert_eq!(citation_source_name(&Citation::default()), "unknown source");
    }

    #[test]
    fn verbosity_changes_the_prompt_but_never_the_grounding_rules() {
        let brief = answer_system(Verbosity::Brief);
        let normal = answer_system(Verbosity::Normal);
        let detailed = answer_system(Verbosity::Detailed);
        // The three differ in the length instruction.
        assert!(brief.contains("one or two sentences"));
        assert!(detailed.contains("thorough"));
        assert_ne!(brief, normal);
        assert_ne!(normal, detailed);
        // But every level still grounds and cites.
        for s in [&brief, &normal, &detailed] {
            assert!(s.contains("[n]"), "must ask for citations: {s}");
            assert!(s.contains("only the provided context"), "must ground: {s}");
        }
    }

    #[test]
    fn record_turn_drops_whole_oldest_pairs_to_fit_the_budget() {
        let mut history = Vec::new();
        // Each turn is 10 chars of content; a 45-char budget holds two pairs
        // (four turns, 40 chars) but not three.
        for i in 0..10 {
            record_turn(
                &mut history,
                &format!("qqqqq{i:04}"),
                &format!("aaaaa{i:04}"),
                45,
            );
        }
        assert!(history_chars(&history) <= 45, "stays within budget");
        assert_eq!(history.len(), 4, "two whole pairs kept");
        assert_eq!(history[0].role, "user", "a pair opens on a user turn");
        assert_eq!(history.last().unwrap().content, "aaaaa0009", "recent kept");
    }

    #[test]
    fn record_turn_keeps_the_last_exchange_even_when_it_exceeds_the_budget() {
        // A single very long turn must not trim itself to nothing, or a
        // follow-up loses the context it was actually about.
        let mut history = Vec::new();
        record_turn(&mut history, &"x".repeat(500), &"y".repeat(500), 10);
        assert_eq!(history.len(), 2, "the most recent pair is always kept");
    }

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
    fn mock_detection_reads_the_model_field_conservatively() {
        let local = Target::Local {
            base: "http://127.0.0.1:8080".into(),
            instance_id: "a".into(),
        };
        let remote = Target::Remote { uuid: "u".into() };
        // The node names the mock explicitly.
        assert!(is_mock_answer(&local, Some("mock")));
        assert!(is_mock_answer(&remote, Some("mock")));
        // A local node that says nothing is running the mock; a hosted node
        // that says nothing is merely not saying, and must not be accused.
        assert!(is_mock_answer(&local, None));
        assert!(!is_mock_answer(&remote, None));
        assert!(!is_mock_answer(&local, Some("qwen3.5:latest")));
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

#[cfg(test)]
mod target_tests {
    use super::*;

    fn local() -> Target {
        Target::Local {
            base: "http://127.0.0.1:8080".into(),
            instance_id: "11111111-2222-4333-8444-555555555555".into(),
        }
    }

    #[test]
    fn memory_is_filed_under_a_name_that_needs_no_control_plane() {
        // Memory is local files. Resolving a UUID through the API just to name
        // a directory is what made `knaix memory` fail with a DNS error on a
        // machine that has no control plane.
        assert_eq!(memory_key(&local()), crate::local::LOCAL_NODE_ID);
        assert_eq!(
            memory_key(&Target::Remote {
                uuid: "abc-123".into()
            }),
            "abc-123"
        );
    }

    #[test]
    fn a_local_target_is_recognisable_without_a_network_call() {
        assert!(local().is_local());
        assert!(!Target::Remote { uuid: "x".into() }.is_local());
    }

    #[test]
    fn the_local_label_is_the_reserved_name_users_type() {
        // `knaix use local` writes this, and every command has to route on it.
        assert_eq!(local().label(), "local");
    }
}
