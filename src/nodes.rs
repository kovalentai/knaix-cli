use crate::config::load_config;
use crate::exit::{Code, WithCode};
use crate::upload_filter::{SkipReason, UploadFilter};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use colored::*;
use crossterm::{cursor, execute};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
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
    /// Milliseconds from sending the question to the first token arriving.
    ///
    /// Everything before that first token is retrieval, reranking, and prompt
    /// assembly; everything after it is generation. Splitting the two is the
    /// only way to tell a slow knowledge base from a slow model, which is what
    /// `knaix bench` reports. Absent when the answer did not stream.
    pub first_token_ms: Option<u128>,
    /// The thread this answer belongs to, for a hosted node. Sent back with the
    /// next question so the control plane answers it in context. Absent for a
    /// local node, which has no control plane to keep the thread, and absent
    /// when the control plane could not open one.
    pub conversation_id: Option<String>,
}

/// What one answer stream produced. A struct rather than a tuple because both
/// readers return it and it now carries five things.
struct StreamOutcome {
    text: String,
    citations: Vec<Citation>,
    model: Option<String>,
    first_token_ms: Option<u128>,
    conversation_id: Option<String>,
}

/// What a caller wants done with the tokens as they arrive.
///
/// A bool could say "print" but not how, which left the REPL choosing between
/// streaming raw and rendering markdown at the end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Echo {
    /// Accumulate silently; the caller prints the finished answer, or nothing.
    Silent,
    /// Print each token as it arrives. The only shape safe to pipe: no escapes
    /// are added that were not in the answer.
    Raw,
    /// Render markdown progressively, a line or a fenced block at a time.
    Markdown,
}

impl Echo {
    /// Whether anything is printed as the answer arrives.
    fn prints(self) -> bool {
        self != Echo::Silent
    }
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

/// The node's default output ceiling, mirrored here so the CLI can ask for a
/// share of it. The node clamps whatever arrives to its own configured value,
/// so asking for more than it allows is safe: the ask is a request, not a
/// promise, and the node has the last word.
const NODE_OUTPUT_CEILING: u32 = 1024;

impl Verbosity {
    /// How many output tokens to ask the node for.
    ///
    /// The system prompt asks for a shape and this makes the ask stick. Without
    /// it a detailed answer is silently cut off at the node's default, which is
    /// one of the two reasons answers come back shorter than they were asked to
    /// be; the prompt alone was the other.
    fn max_tokens(self) -> u32 {
        match self {
            Verbosity::Brief => NODE_OUTPUT_CEILING / 4,
            Verbosity::Normal => NODE_OUTPUT_CEILING,
            Verbosity::Detailed => NODE_OUTPUT_CEILING * 3,
        }
    }

    /// The wire name the control plane reads. A hosted node is prompted there,
    /// so the level travels rather than the prompt it produces.
    fn as_str(self) -> &'static str {
        match self {
            Verbosity::Brief => "brief",
            Verbosity::Normal => "normal",
            Verbosity::Detailed => "detailed",
        }
    }
}

/// The grounding instructions the local node answers under when a command does
/// not supply its own. The direct `/api/query/answer` route applies no default
/// of its own, so without this the model is handed an empty system prompt and
/// tends to return a single terse line. This lives on the CLI because it is the
/// one path that drives the node directly; the hosted path is prompted by the
/// control plane in front of it. The `shape` clause is the only part that moves
/// with verbosity; grounding, citation, and formatting rules are constant.
fn answer_system(verbosity: Verbosity) -> String {
    // "Answering questions" was the whole of this, and a model given only that
    // refuses anything that is not a lookup: asked to draw up a quiz from a
    // study guide it had just ingested, it replied that the knowledge base held
    // nothing about administering quizzes, which is true and useless. Grounding
    // is about where the facts come from, not about what may be asked for, so
    // the two are stated separately now.
    let grounding = "You are a helpful assistant working from a private knowledge base. \
Base everything you say on the provided context, and cite the passages you draw on with their \
[n] markers. The request may ask you to do something with that material rather than look \
something up: summarize it, draw questions or exercises from it, outline it, compare parts of \
it. Do what was asked, built only from what the context contains. When the context does not \
hold enough to do it, say so plainly rather than guessing.";
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
        "max_tokens": verbosity.max_tokens(),
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
    /// Suppress commentary, keeping results and errors.
    pub quiet: bool,
}

impl KnaixContext {
    pub fn new(output_format: String) -> Self {
        Self::with_quiet(output_format, false)
    }

    pub fn with_quiet(output_format: String, quiet: bool) -> Self {
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
            quiet,
        }
    }

    /// Print progress or commentary: what a person watching wants and a script
    /// does not.
    ///
    /// Only this is silenced. Results still print and errors still reach
    /// stderr, because a quiet flag that could hide a failure would make every
    /// script using it less safe than one that did not.
    pub fn info(&self, line: &str) {
        if !self.quiet {
            println!("{line}");
        }
    }

    /// Whether to draw progress bars and spinners. They are commentary too, and
    /// on a pipe they are noise that no reader will ever redraw.
    pub fn show_progress(&self) -> bool {
        !self.quiet
    }

    /// A spinner that draws nothing when quiet.
    ///
    /// Hidden rather than skipped so callers keep one code path: a caller that
    /// has to branch around the spinner eventually branches around something
    /// else too.
    pub fn spinner(&self) -> ProgressBar {
        self.drawn(ProgressBar::new_spinner())
    }

    /// A progress bar over a known length, hidden when quiet.
    pub fn progress_bar(&self, len: u64) -> ProgressBar {
        self.drawn(ProgressBar::new(len))
    }

    fn drawn(&self, pb: ProgressBar) -> ProgressBar {
        if self.quiet {
            pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }
        pb
    }

    pub fn get_token(&self) -> Result<&String> {
        self.config
            .token
            .as_ref()
            .context("Not logged in. Run 'knaix login' first.")
            .coded(Code::Auth)
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
    let node = crate::local::load()
        .ok_or_else(|| {
            anyhow!(
                "No local node has been started. Run '{}' first.",
                "knaix local up"
            )
        })
        .coded(Code::Precondition)?;
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

/// Find a named node in a list already in hand.
///
/// For callers that have fetched the node list for their own reasons and would
/// otherwise fetch it again to resolve one name against it. Shares
/// `node_matches` with the fetching resolvers, so what counts as a match cannot
/// drift between the two paths.
pub(crate) fn find_node<'a>(nodes: &'a [Node], wanted: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| node_matches(n, wanted))
}

/// Fetch the caller's nodes once, for resolution and listing alike.
pub(crate) async fn fetch_nodes(ctx: &KnaixContext) -> Result<Vec<Node>> {
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
        ))
        .coded(Code::for_status(resp.status().as_u16()));
    }

    let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(serde_json::from_value(wrapper["data"].clone()).unwrap_or_default())
}

/// A duration a person can read at a glance.
///
/// Milliseconds below a second, seconds above one. Four or five digits of
/// milliseconds makes the reader divide before they can react to the number,
/// which is the opposite of what a timing readout is for.
pub fn format_duration_ms(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// The model name as a person should read it.
///
/// The local node namespaces what it reports as `local:<model>`, which is its
/// own bookkeeping and not something the user chose or would recognise. The
/// raw value is what the node said, so it stays in `--json`; only the display
/// drops the prefix.
pub fn display_model(model: &str) -> &str {
    model.strip_prefix("local:").unwrap_or(model)
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
    // Answered before the token is read, because neither a session nor the
    // control plane has anything to do with it. Reaching for them first is what
    // made this report a DNS failure on a machine that was working fine.
    if node_id == Some(crate::local::LOCAL_NODE_ID) {
        return Err(anyhow!(
            "The local node keeps chunks and no document registry, so its documents cannot be listed.\n  {} retrieves from them and cites what it used; {} empties the store.",
            crate::brand::cmd("chat -n local"),
            crate::brand::cmd("local reset")
        ))
        .coded(Code::Error);
    }

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
            return Err(anyhow!("Failed to fetch documents: HTTP {}", resp.status()))
                .coded(Code::for_status(resp.status().as_u16()));
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
                crate::brand::cmd("up"),
                crate::brand::cmd("local up")
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
                        node_type = "BYO TAILNET".yellow();
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
        return Err(anyhow!("Failed to fetch nodes: HTTP {}", resp.status()))
            .coded(Code::for_status(resp.status().as_u16()));
    }
    Ok(())
}

pub async fn select_node_interactively(ctx: &KnaixContext) -> Result<Option<String>> {
    let token = ctx.get_token()?;

    // Show spinner while fetching nodes
    let pb = ctx.spinner();
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
        ))
        .coded(Code::for_status(resp.status().as_u16()));
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
            crate::brand::cmd("up"),
            crate::brand::cmd("local up")
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
pub(crate) fn node_uuid(node: &Node) -> Result<String> {
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
        ))
        .coded(Code::NotFound),
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

/// How long a wait has to run before the spinner puts a number on it. Under
/// this, a count is noise on an answer that is about to arrive anyway.
const WAITED_AFTER: Duration = Duration::from_secs(5);

/// How the elapsed wait reads once it is worth showing.
///
/// Written against a duration rather than the progress state so the thresholds
/// can be tested without a live bar.
fn waited_label(elapsed: Duration) -> String {
    if elapsed < WAITED_AFTER {
        return String::new();
    }
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ")
    } else {
        format!("{}m{:02}s ", secs / 60, secs % 60)
    }
}

/// The spinner both chat paths run behind.
///
/// A long generation used to sit on one unchanging line, which reads the same
/// whether the model is working or the connection has died. Past a few seconds
/// it counts, so the wait is visibly moving. The count leads the message
/// because `wide_msg` claims the rest of the line.
fn chat_spinner(ctx: &KnaixContext) -> ProgressBar {
    let pb = ctx.spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars(
                "\u{280b}\u{2819}\u{2839}\u{2838}\u{283c}\u{2834}\u{2826}\u{2827}\u{2807}\u{280f}",
            )
            // Wide, so a progress line naming several documents is truncated to
            // the terminal rather than wrapping into the answer below it.
            .template("{spinner:.cyan} {waited}{wide_msg}")
            .unwrap()
            .with_key(
                "waited",
                |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let _ = write!(w, "{}", waited_label(state.elapsed()));
                },
            ),
    );
    // Replaced with the retrieved sources as soon as the `meta` frame lands.
    pb.set_message("Searching your documents...");
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

/// Whether a failed question is worth asking again without its thread.
///
/// A conversation can go away underneath a session: deleted from the dashboard,
/// or opened against a node the caller has since lost. The control plane answers
/// 404 for that, and it answers 404 for a missing node too, so the body has to
/// distinguish them. Retrying blind would cost a second request on every 404;
/// ending the session on a thread the user never mentioned would be worse.
fn resend_without_conversation(status: u16, body: &serde_json::Value, sent_one: bool) -> bool {
    if status != 404 || !sent_one {
        return false;
    }
    body["error"]
        .as_str()
        .is_some_and(|e| e.to_ascii_lowercase().contains("conversation"))
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
    echo: Echo,
    history: &[ChatTurn],
    verbosity: Verbosity,
    conversation: Option<&str>,
) -> Result<Option<ChatAnswer>> {
    if let Target::Local { base, instance_id } = target {
        // A local node has no control plane to hold a thread, so its context is
        // the history the caller replays.
        let _ = conversation;
        return chat_local(ctx, base, instance_id, message, echo, history, verbosity).await;
    }
    // The control plane holds the thread and replays it into the prompt, so the
    // hosted path sends the conversation id rather than the transcript.
    let _ = history;
    let node_uuid = &target.label();
    let token = ctx.get_token()?;

    let pb = chat_spinner(ctx);

    let url = format!("{}/api/nodes/{}/native-chat", ctx.config.api_url, node_uuid);

    // Timed from before the request leaves, so time-to-first-token covers the
    // whole wait a person actually sits through, not just the node's share.
    let asked_at = std::time::Instant::now();

    // Sent with the question, and dropped once if the control plane no longer
    // knows the thread. See `resend_without_conversation`.
    let mut thread = conversation.map(|s| s.to_string());
    let resp = loop {
        let mut payload = serde_json::json!({
            "message": message,
            "stream": true,
            // The level, not a prompt: a hosted node is prompted by the control
            // plane, which maps this to both a shape and a cap.
            "verbosity": verbosity.as_str(),
        });
        if let Some(id) = &thread {
            payload["conversationId"] = serde_json::json!(id);
        }

        let resp = ctx
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            // An answer takes as long as the model takes; the client default
            // that protects quick lookups would cut off a long generation
            // mid-stream.
            .timeout(Duration::from_secs(300))
            .json(&payload)
            .send()
            .await
            .inspect_err(|_| pb.finish_and_clear())
            .context("Networking error during chat request")?;

        if resp.status().is_success() {
            break resp;
        }

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if resend_without_conversation(status.as_u16(), &body, thread.is_some()) {
            thread = None;
            continue;
        }

        pb.finish_and_clear();
        let detail = body["error"].as_str().unwrap_or("no detail");
        return Err(anyhow!("Chat failed on node: HTTP {} - {}", status, detail))
            .coded(Code::for_status(status.as_u16()));
    };

    let StreamOutcome {
        text,
        citations,
        model,
        first_token_ms,
        conversation_id,
    } = read_chat_stream(resp, &pb, echo, asked_at).await?;

    if echo.prints() {
        print_citations(&citations);
    }

    Ok(Some(ChatAnswer {
        text,
        citations,
        model,
        first_token_ms,
        conversation_id,
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
    echo: Echo,
    history: &[ChatTurn],
    verbosity: Verbosity,
) -> Result<Option<ChatAnswer>> {
    let pb = chat_spinner(ctx);

    let body = build_local_answer_body(instance_id, message, history, verbosity);

    // Timed from before the request leaves, so time-to-first-token covers the
    // whole wait a person actually sits through, not just the node's share.
    let asked_at = std::time::Instant::now();
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
        return chat_local_blocking(ctx, base, &body, &pb, echo).await;
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
        ))
        .coded(Code::for_status(status.as_u16()));
    }

    let outcome = read_local_answer_stream(resp, &pb, echo, asked_at).await?;

    Ok(Some(ChatAnswer {
        text: outcome.text,
        citations: outcome.citations,
        model: outcome.model,
        first_token_ms: outcome.first_token_ms,
        conversation_id: None,
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
    echo: Echo,
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
        ))
        .coded(Code::for_status(status.as_u16()));
    }

    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let text = body["answer"].as_str().unwrap_or_default().to_string();
    let citations = node_citations(&body["citations"], &text);
    let model = body["model"].as_str().map(|s| s.to_string());

    // Nothing streamed, so there is no progressive rendering to do; the answer
    // is printed the way this echo mode would have assembled it.
    match echo {
        Echo::Markdown => {
            let mut md = crate::markdown::MarkdownStream::new();
            md.push(&text);
            md.finish();
        }
        Echo::Raw => println!("{} {}", "AI:".cyan().bold(), text),
        Echo::Silent => {}
    }
    if echo.prints() {
        print_citations(&citations);
    }

    Ok(Some(ChatAnswer {
        text,
        citations,
        model,
        // Nothing streamed, so there was no first token to time. Reporting the
        // whole wait here would read as an instant answer that then took
        // seconds to finish, which is the opposite of what happened.
        first_token_ms: None,
        conversation_id: None,
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
    echo: Echo,
    asked_at: std::time::Instant,
) -> Result<StreamOutcome> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut answer = String::new();
    let mut citations: Vec<Citation> = Vec::new();
    let mut model: Option<String> = None;
    let mut markdown = (echo == Echo::Markdown).then(crate::markdown::MarkdownStream::new);
    let mut event = String::new();
    let mut first_token = true;
    let mut first_token_ms: Option<u128> = None;
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
                    // Parsed here rather than at the end so the sources can be
                    // named while the model is still generating; which of them
                    // the answer used is stamped on once it is complete.
                    citations = parse_node_citations(&parsed["citations"]);
                    model = parsed["model"].as_str().map(|s| s.to_string());
                    pb.set_message(retrieval_progress(&citations));
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
                        // Clear the spinner only once the first token is in
                        // hand, so the line it occupied is reused.
                        if first_token && echo.prints() {
                            pb.finish_and_clear();
                            // The markdown renderer owns whole lines, so a
                            // prefix on the first one would only misalign the
                            // rest of the answer under it.
                            if echo == Echo::Raw {
                                print!("{} ", "AI:".cyan().bold());
                            }
                        }
                        match &mut markdown {
                            Some(md) => md.push(token),
                            None if echo == Echo::Raw => {
                                print!("{}", token);
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                            }
                            None => {}
                        }
                        if first_token {
                            first_token_ms = Some(asked_at.elapsed().as_millis());
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
    if let Some(md) = &mut markdown {
        md.finish();
    } else if echo == Echo::Raw && !first_token {
        println!();
    }

    if let Some(code) = stream_error {
        return Err(anyhow!("Local node could not answer: {}", code));
    }

    stamp_cited(&mut citations, &answer);
    if echo.prints() {
        print_citations(&citations);
    }
    Ok(StreamOutcome {
        text: answer,
        citations,
        model,
        first_token_ms,
        conversation_id: None,
    })
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
    let mut citations = parse_node_citations(raw);
    stamp_cited(&mut citations, answer);
    citations
}

/// The passages the node retrieved, before anything is known about which of
/// them the answer used.
///
/// Split out because the `meta` frame arrives before the first token, so the
/// sources can be named while the model is still generating. `cited` stays
/// false until the answer can settle it.
fn parse_node_citations(raw: &serde_json::Value) -> Vec<Citation> {
    raw.as_array()
        .map(|items| {
            items
                .iter()
                .map(|c| Citation {
                    index: c["index"].as_u64().map(|n| n as u32),
                    content: c["content"].as_str().map(|s| s.to_string()),
                    source: serde_json::from_value(c["metadata"]["source"].clone()).ok(),
                    cited: Some(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Mark the passages whose `[n]` markers appear in the finished answer.
fn stamp_cited(citations: &mut [Citation], answer: &str) {
    let referenced = referenced_indexes(answer);
    for citation in citations.iter_mut() {
        citation.cited = Some(
            citation
                .index
                .map(|i| referenced.contains(&i))
                .unwrap_or(false),
        );
    }
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
    echo: Echo,
    asked_at: std::time::Instant,
) -> Result<StreamOutcome> {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut answer = String::new();
    let mut citations: Vec<Citation> = Vec::new();
    let mut model: Option<String> = None;
    let mut markdown = (echo == Echo::Markdown).then(crate::markdown::MarkdownStream::new);
    // Which citations the answer actually referenced. It arrives at the end, in
    // the `done` frame, rather than on the citations themselves.
    let mut cited_indexes: Vec<u32> = Vec::new();
    let mut conversation_id: Option<String> = None;
    let mut event = String::new();
    let mut first_token = true;
    let mut first_token_ms: Option<u128> = None;
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
                    // Null when the control plane could not open a thread, so a
                    // failure to persist is not read as a thread called "null".
                    conversation_id = parsed["conversationId"].as_str().map(|s| s.to_string());
                    // Retrieval is done; the rest of the wait is the model. Say
                    // what it found rather than leaving "Thinking..." up.
                    pb.set_message(retrieval_progress(&citations));
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
                        // Clear the spinner only once the first token is in
                        // hand, so the line it occupied is reused.
                        if first_token && echo.prints() {
                            pb.finish_and_clear();
                            // The markdown renderer owns whole lines, so a
                            // prefix on the first one would only misalign the
                            // rest of the answer under it.
                            if echo == Echo::Raw {
                                print!("{} ", "AI:".cyan().bold());
                            }
                        }
                        match &mut markdown {
                            Some(md) => md.push(token),
                            None if echo == Echo::Raw => {
                                print!("{}", token);
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                            }
                            None => {}
                        }
                        if first_token {
                            first_token_ms = Some(asked_at.elapsed().as_millis());
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
    if let Some(md) = &mut markdown {
        md.finish();
    } else if echo == Echo::Raw && !first_token {
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

    Ok(StreamOutcome {
        text: answer,
        citations,
        model,
        first_token_ms,
        conversation_id,
    })
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
            // Null for a local node, and for a hosted one that could not open a
            // thread. A script that wants a follow-up answered in context sends
            // this back; there is nothing else to correlate two questions by.
            "conversationId": answer.conversation_id,
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

/// How many source names the progress line lists before summarising the rest.
/// Two fits a narrow terminal; past that the names stop being the useful part.
const PROGRESS_SOURCES: usize = 2;

/// The progress line shown once retrieval lands and generation begins.
///
/// Both answer streams send the citations before the first token, so the wait
/// for the model is time we can already say something about. Naming the
/// documents turns it into evidence that retrieval found the right ones.
fn retrieval_progress(citations: &[Citation]) -> String {
    if citations.is_empty() {
        // Not a failure: the model is told to say it lacks the answer. Saying so
        // now explains an "I don't have that" that is still seconds away.
        return "No matching passages found; answering without context...".to_string();
    }

    let mut names: Vec<String> = Vec::new();
    for citation in citations {
        let name = citation_source_name(citation);
        if !names.contains(&name) {
            names.push(name);
        }
    }

    let passages = if citations.len() == 1 {
        "1 passage".to_string()
    } else {
        format!("{} passages", citations.len())
    };

    let shown = names
        .iter()
        .take(PROGRESS_SOURCES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let rest = names.len().saturating_sub(PROGRESS_SOURCES);
    let sources = if rest > 0 {
        format!("{} and {} more", shown, rest)
    } else {
        shown
    };

    format!("Reading {} from {}...", passages, sources)
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

    ctx.info(&format!(
        "\n  {} {} ({})",
        "Uploading".cyan(),
        file_name.bold().white(),
        format_file_size(file_size).dimmed()
    ));

    let pb = ctx.progress_bar(file_size);
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
                ctx.info(&format!(
                    "  {} {} ingested ({} chunk{}).",
                    "✓".green(),
                    file_name.white(),
                    chunks,
                    if chunks == 1 { "" } else { "s" }
                ));
                Ok(chunks)
            } else {
                let err = data["error"].as_str().unwrap_or("Unknown error");
                Err(anyhow!("Upload failed: HTTP {} - {}", status, err))
                    .coded(Code::for_status(status.as_u16()))
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
    ctx.info(&format!(
        "\n  {} {} ({})",
        "Ingesting".cyan(),
        file_name.bold().white(),
        format_file_size(bytes.len() as u64).dimmed()
    ));

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
        return Err(anyhow!("Ingest failed: HTTP {} - {}", status, detail))
            .coded(Code::for_status(status.as_u16()));
    }

    let chunks = body["chunkCount"].as_u64().unwrap_or(0);
    ctx.info(&format!(
        "  {} {} ingested ({} chunk{}), embedded by {}.",
        "\u{2713}".green(),
        file_name.white(),
        chunks,
        if chunks == 1 { "" } else { "s" },
        body["embeddingProvider"]
            .as_str()
            .unwrap_or("the node")
            .dimmed()
    ));
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

/// How an upload chooses files. Not whether it sends them: that is decided
/// before this is built, because planning needs no node and sending does.
pub struct UploadOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub all: bool,
}

/// What an upload would send, worked out without touching the network.
///
/// Separated from sending because deciding which files qualify is entirely
/// local: it reads the directory and the filters and nothing else. Keeping it
/// joined to the send meant `--dry-run` needed a reachable node to answer a
/// question no node is involved in.
pub struct UploadPlan {
    pub queue: Vec<PathBuf>,
    pub summary: UploadSummary,
    /// Set when the caller named one file rather than a directory.
    pub single_file: Option<String>,
}

pub fn plan_upload(
    ctx: &KnaixContext,
    file_path: &str,
    opts: &UploadOptions,
) -> Result<UploadPlan> {
    let base_path = Path::new(file_path);
    if !base_path.exists() {
        return Err(anyhow!("Path not found: {}", file_path)).coded(Code::NotFound);
    }

    let filter = UploadFilter::new(&opts.include, &opts.exclude, opts.all)?;

    if !base_path.is_dir() {
        // A named file is uploaded because it was named. Filters describe how
        // to search a directory, not permission to ignore an explicit request.
        return Ok(UploadPlan {
            queue: vec![base_path.to_path_buf()],
            summary: UploadSummary::default(),
            single_file: Some(file_name_of(base_path)),
        });
    }

    // Said before the walk, not after it. It is the only thing that speaks
    // during a long scan, so printing it once the scan is done makes it a
    // report of the past rather than a sign of life.
    ctx.info(&format!("{} Scanning {}", "Info:".blue(), file_path.bold()));

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

    Ok(UploadPlan {
        queue,
        summary,
        single_file: None,
    })
}

/// Report a plan without sending anything. Needs no node.
pub fn report_plan(plan: &UploadPlan, file_path: &str) {
    if let Some(name) = &plan.single_file {
        println!("  {} {}", "would ingest".dimmed(), name);
        return;
    }
    report_dry_run(&plan.queue, &plan.summary, Path::new(file_path));
}

/// Send a plan. The plan is made first and separately, so a bad path or a glob
/// that matches nothing is reported before a node is ever resolved: typing a
/// path that does not exist should say so, not ask you to log in.
pub async fn upload(
    ctx: &KnaixContext,
    target: &Target,
    file_path: &str,
    plan: UploadPlan,
) -> Result<()> {
    let base_path = Path::new(file_path);

    let UploadPlan {
        queue,
        mut summary,
        single_file,
    } = plan;

    if let Some(file_name) = single_file {
        return upload_single_file(ctx, target, base_path, &file_name)
            .await
            .map(|_| ());
    }

    if queue.is_empty() {
        ctx.info(&format!(
            "{} Nothing to ingest. {} file(s) were skipped; pass {} to see why, or {} to send everything.",
            "Info:".blue(),
            summary.skipped.len(),
            "--dry-run".cyan(),
            "--all".cyan()
        ));
        return Ok(());
    }

    let total = queue.len();
    for (i, path) in queue.iter().enumerate() {
        let file_name = file_name_of(path);
        ctx.info(&format!(
            "  {} {} of {}",
            "[".dimmed(),
            i + 1,
            format!("{}]", total).dimmed()
        ));
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

    if ctx.show_progress() {
        report_summary(&summary);
    }

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

    let pb = ctx.spinner();
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
                    .coded(Code::for_status(resp.status().as_u16()))
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

    let pb = ctx.spinner();
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
                    .coded(Code::for_status(resp.status().as_u16()))
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

    let pb = ctx.spinner();
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
                        crate::brand::cmd("list"),
                        crate::brand::cmd(&format!("metrics {}", id))
                    ),
                    None => println!(
                        "  Boot takes a few minutes. Run {} to see it come up.",
                        crate::brand::cmd("list")
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

/// Where `--file` reads from, given the node's memory directory.
///
/// `join` replaces the directory outright when handed an absolute path, so an
/// unchecked name here reads any file the user can read rather than one of
/// their notes. The listing prints bare file names, and this accepts exactly
/// what that prints.
///
/// Checking the name is not enough on its own. A name that cannot escape can
/// still point somewhere that does: reading follows a symlink, so a link left
/// in the notes directory reads whatever it targets. Both sides are resolved
/// and the file has to still be in the directory afterwards.
fn memory_file_path(memory_dir: &std::path::Path, file_name: &str) -> Result<std::path::PathBuf> {
    let checked = crate::stdin_arg::checked_name("--file", file_name)?;
    let path = memory_dir.join(checked);

    // Only when it resolves. A name that does not exist yet is the caller's
    // "no such note" case, reported below with the listing that fixes it.
    if let (Ok(real), Ok(root)) = (path.canonicalize(), memory_dir.canonicalize()) {
        if real.parent() != Some(root.as_path()) {
            return Err(anyhow!(
                "{} leaves the notes directory for node memory. Only files inside it can be read.",
                file_name
            ))
            .coded(Code::Denied);
        }
    }
    Ok(path)
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
        let file_path = memory_file_path(&memory_dir, file_name)?;

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
            // `symlink_metadata` does not follow the link, so a link out of the
            // directory is not offered as a note. `is_file` would follow it and
            // list whatever it points at as though it were one.
            let is_plain_file = tokio::fs::symlink_metadata(&path)
                .await
                .map(|m| m.file_type().is_file())
                .unwrap_or(false);
            if is_plain_file {
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

    /// Four or five digits of milliseconds is a number the reader has to divide
    /// before they can react to it, which is the opposite of what a timing
    /// readout is for.
    #[test]
    fn long_durations_are_read_in_seconds() {
        assert_eq!(format_duration_ms(0), "0 ms");
        assert_eq!(format_duration_ms(737), "737 ms");
        assert_eq!(format_duration_ms(999), "999 ms");
        assert_eq!(format_duration_ms(1000), "1.0 s");
        assert_eq!(format_duration_ms(13796), "13.8 s");
    }

    /// `local:` is the node's own bookkeeping. It is not something the user
    /// chose, and it is not in the name they would type at Ollama.
    #[test]
    fn the_nodes_model_prefix_is_not_shown_to_the_user() {
        assert_eq!(display_model("local:gemma4:latest"), "gemma4:latest");
        // A hosted model name is left exactly as the node reported it.
        assert_eq!(display_model("claude-sonnet-4"), "claude-sonnet-4");
        // Only a leading prefix, so a model that merely contains the word is
        // untouched.
        assert_eq!(display_model("acme/local:v2"), "acme/local:v2");
    }

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

    /// The leading text two prompts agree on, byte for byte.
    fn common_prefix(a: &str, b: &str) -> String {
        a.chars()
            .zip(b.chars())
            .take_while(|(x, y)| x == y)
            .map(|(x, _)| x)
            .collect()
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
        // But every level still grounds and cites. Asserted on the shared
        // prefix rather than on a chosen phrase: the wording of the grounding
        // clause is allowed to change, its being identical across the three is
        // what this protects. A phrase match broke on the first rewording and
        // said nothing about the invariant.
        let shared = common_prefix(&common_prefix(&brief, &normal), &detailed);
        assert!(
            shared.contains("[n]"),
            "every level must ask for citations: {shared}"
        );
        assert!(
            shared.contains("context"),
            "every level must ground in the context: {shared}"
        );
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

    /// The progress line is built from the `meta` frame, which lands before a
    /// single token of the answer does. Nothing may be claimed there that is
    /// only knowable afterwards -- above all not which passages were cited.
    #[test]
    fn sources_are_named_from_meta_before_the_answer_exists() {
        let raw = serde_json::json!([
            { "index": 1, "content": "p", "metadata": { "source": { "name": "a.md" } } },
            { "index": 2, "content": "q", "metadata": { "source": { "name": "b.md" } } }
        ]);
        let parsed = parse_node_citations(&raw);

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].source.as_ref().unwrap().name.as_deref(),
            Some("a.md")
        );
        // Unknowable this early, so claimed for nothing.
        assert!(parsed.iter().all(|c| c.cited == Some(false)));
    }

    /// Parsing early and stamping late has to land where parsing late did, or
    /// the local path's citations would differ from the blocking path's.
    #[test]
    fn parsing_early_and_stamping_late_matches_doing_both_at_the_end() {
        let raw = serde_json::json!([
            { "index": 1, "content": "p", "metadata": { "source": { "name": "a.md" } } },
            { "index": 2, "content": "q", "metadata": { "source": { "name": "b.md" } } }
        ]);
        let answer = "Grounded in [2].";

        let mut split = parse_node_citations(&raw);
        stamp_cited(&mut split, answer);
        let at_once = node_citations(&raw, answer);

        let flags = |cs: &[Citation]| cs.iter().map(|c| c.cited).collect::<Vec<_>>();
        assert_eq!(flags(&split), flags(&at_once));
        assert_eq!(flags(&split), vec![Some(false), Some(true)]);
    }

    /// The prompt asks for a shape; this is what makes the ask stick. Without a
    /// cap, a detailed answer is cut off at the node's default.
    #[test]
    fn a_detailed_answer_is_given_more_room_than_a_brief_one() {
        assert!(Verbosity::Brief.max_tokens() < Verbosity::Normal.max_tokens());
        assert!(Verbosity::Detailed.max_tokens() > Verbosity::Normal.max_tokens());
    }

    /// Normal has to stay exactly the node's own default, so a user who passes
    /// no flag gets the length they always got.
    #[test]
    fn normal_asks_for_the_nodes_own_ceiling() {
        assert_eq!(Verbosity::Normal.max_tokens(), NODE_OUTPUT_CEILING);
    }

    #[test]
    fn the_local_body_carries_the_cap_alongside_the_prompt() {
        let body = build_local_answer_body("abc", "why?", &[], Verbosity::Detailed);
        assert_eq!(
            body["max_tokens"].as_u64(),
            Some(Verbosity::Detailed.max_tokens() as u64)
        );
        // Both halves travel: the cap does not replace the shape.
        assert!(body["system"].as_str().unwrap().contains("thorough answer"));
    }

    /// The control plane matches on these exact strings, and anything it does
    /// not recognise silently becomes normal, so a typo here is invisible.
    #[test]
    fn the_wire_names_are_the_ones_the_control_plane_reads() {
        assert_eq!(Verbosity::Brief.as_str(), "brief");
        assert_eq!(Verbosity::Normal.as_str(), "normal");
        assert_eq!(Verbosity::Detailed.as_str(), "detailed");
    }

    /// A fast answer must not pick up a counter it never needed.
    #[test]
    fn a_short_wait_is_not_counted() {
        assert_eq!(waited_label(Duration::from_secs(0)), "");
        assert_eq!(waited_label(Duration::from_millis(4_999)), "");
    }

    /// Past the threshold the wait is visibly moving, which is the difference
    /// between a model working and a connection that has died.
    #[test]
    fn a_long_wait_is_counted_in_seconds() {
        assert_eq!(waited_label(Duration::from_secs(5)), "5s ");
        assert_eq!(waited_label(Duration::from_secs(59)), "59s ");
    }

    /// Three digits of seconds is a number the reader has to divide before they
    /// can react to it.
    #[test]
    fn a_wait_past_a_minute_reads_as_minutes() {
        assert_eq!(waited_label(Duration::from_secs(60)), "1m00s ");
        assert_eq!(waited_label(Duration::from_secs(125)), "2m05s ");
        assert_eq!(waited_label(Duration::from_secs(3_600)), "60m00s ");
    }

    #[test]
    fn the_progress_line_names_the_documents_retrieval_found() {
        let cites = vec![citation_named("Handbook.md"), citation_named("Refunds.md")];
        assert_eq!(
            retrieval_progress(&cites),
            "Reading 2 passages from Handbook.md, Refunds.md..."
        );
    }

    #[test]
    fn one_passage_is_not_pluralised() {
        assert_eq!(
            retrieval_progress(&[citation_named("Handbook.md")]),
            "Reading 1 passage from Handbook.md..."
        );
    }

    /// Several passages from one document are one source to a reader.
    #[test]
    fn repeated_sources_are_named_once() {
        let cites = vec![citation_named("Handbook.md"), citation_named("Handbook.md")];
        assert_eq!(
            retrieval_progress(&cites),
            "Reading 2 passages from Handbook.md..."
        );
    }

    /// A long list would push the spinner past the width of a terminal, and the
    /// names stop being the useful part well before that.
    #[test]
    fn a_long_source_list_is_summarised() {
        let cites = vec![
            citation_named("a.md"),
            citation_named("b.md"),
            citation_named("c.md"),
            citation_named("d.md"),
        ];
        assert_eq!(
            retrieval_progress(&cites),
            "Reading 4 passages from a.md, b.md and 2 more..."
        );
    }

    /// Retrieval finding nothing is not an error, but it does decide the answer
    /// that is still seconds away; saying so early explains it.
    #[test]
    fn an_empty_retrieval_says_so_rather_than_naming_nothing() {
        assert_eq!(
            retrieval_progress(&[]),
            "No matching passages found; answering without context..."
        );
    }

    /// A thread deleted underneath a session must not end it. The question is
    /// asked again without the id, and the control plane opens a new thread.
    #[test]
    fn a_vanished_conversation_is_asked_again_without_it() {
        let body = serde_json::json!({ "error": "Conversation not found" });
        assert!(resend_without_conversation(404, &body, true));
    }

    /// A 404 for the node itself is the caller's real error. Retrying it would
    /// spend a second request to arrive at the same failure.
    #[test]
    fn a_404_that_is_not_about_the_thread_is_not_retried() {
        let body = serde_json::json!({ "error": "Node not found" });
        assert!(!resend_without_conversation(404, &body, true));
        assert!(!resend_without_conversation(
            404,
            &serde_json::json!({}),
            true
        ));
    }

    /// Nothing to drop, so nothing to retry: a second identical request would
    /// only ask the same question twice.
    #[test]
    fn a_first_question_is_never_retried() {
        let body = serde_json::json!({ "error": "Conversation not found" });
        assert!(!resend_without_conversation(404, &body, false));
    }

    /// Only 404 means the thread is gone. A 500 is the control plane's problem
    /// and asking again would double a question that may already have run.
    #[test]
    fn other_statuses_are_left_alone() {
        let body = serde_json::json!({ "error": "Conversation not found" });
        for status in [400, 401, 403, 429, 500, 503] {
            assert!(
                !resend_without_conversation(status, &body, true),
                "status {status}"
            );
        }
    }

    /// The progress line goes through the same naming as the citation list, so
    /// a saved note is not shown by its internal filename here either.
    #[test]
    fn a_remember_note_is_named_the_same_way_in_progress() {
        let cites = vec![citation_named(NOTES_FILE)];
        assert_eq!(
            retrieval_progress(&cites),
            "Reading 1 passage from your saved note (/remember)..."
        );
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

    /// `--file` names one note, so a path never reaches the filesystem. An
    /// absolute one is the case that matters: `join` would drop the memory
    /// directory and read whatever was named.
    #[test]
    fn a_memory_file_that_is_a_path_is_refused() {
        let dir = std::path::Path::new("/home/u/.knaix/memory/local");
        for bad in [
            "/etc/passwd",
            "../../../../../etc/passwd",
            "../escape.md",
            "docs/notes.md",
            "..",
            ".",
            "",
        ] {
            let e = memory_file_path(dir, bad).unwrap_err();
            assert_eq!(
                crate::exit::code_of(&e),
                Code::Usage,
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn a_memory_file_that_is_a_name_resolves_inside_the_directory() {
        let dir = std::path::Path::new("/home/u/.knaix/memory/local");
        assert_eq!(
            memory_file_path(dir, "_knaix_durable_memory.md").unwrap(),
            dir.join("_knaix_durable_memory.md")
        );
    }

    /// A name that cannot escape can still point somewhere that does. Reading
    /// follows the link, so the check has to be on where it lands.
    #[cfg(unix)]
    #[test]
    fn a_note_that_is_a_symlink_out_of_the_directory_is_refused() {
        // Cleared first, not just afterwards. A failing assertion skips the
        // cleanup below, and `symlink` refuses a path that already exists, so a
        // run that leaves this behind would fail every later run on the same
        // pid for a reason that has nothing to do with the code under test.
        let root = std::env::temp_dir().join(format!("knaix-memlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        let outside = root.join("secret.txt");
        std::fs::write(&outside, b"not a note").unwrap();

        std::os::unix::fs::symlink(&outside, dir.join("innocent.md")).unwrap();
        let e = memory_file_path(&dir, "innocent.md").unwrap_err();
        assert_eq!(crate::exit::code_of(&e), Code::Denied);

        // A real note beside it still reads.
        std::fs::write(dir.join("real.md"), b"a note").unwrap();
        assert!(memory_file_path(&dir, "real.md").is_ok());

        let _ = std::fs::remove_dir_all(&root);
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
