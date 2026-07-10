use crate::config::load_config;
use anyhow::{anyhow, Context, Result};
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
    pub name: String,
    pub state: String,
    pub instance_id: Option<String>,
    pub private_ip: Option<String>,
    pub model: Option<String>,
    pub credentials: Option<Credentials>,
    pub config: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Credentials {
    pub api_key: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Document {
    pub id: serde_json::Value,
    pub name: String,
    pub r#type: String,
    pub location: Option<String>,
}

/// The KnaixContext holds its own wam-up HTTP client and configuration.
/// This allows for connection pooling and TLS session reuse between commands.
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
        // List Documents for a specific node
        let url = format!("{}/api/nodes/{}/documents", ctx.config.api_url, nid);
        let resp = ctx
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .send()
            .await
            .context("Failed to connect to Kovalent API")?;

        if resp.status().is_success() {
            let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
            let docs_val = &wrapper["documents"];

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
                table.set_header(vec!["Name", "Type", "Location"]);

                for doc in docs {
                    table.add_row(vec![
                        doc.name,
                        doc.r#type,
                        doc.location.unwrap_or_else(|| "N/A".to_string()),
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

    // Auto-select if only one node exists
    if nodes.len() == 1 {
        let id = nodes[0].instance_id.clone().or(Some(nodes[0].name.clone()));
        if let Some(ref nid) = id {
            println!("{} Auto-selected node: {}", "Info:".blue(), nid.cyan());
        }
        return Ok(id);
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
        return Ok(nodes[index]
            .instance_id
            .clone()
            .or(Some(nodes[index].name.clone())));
    }

    Ok(None)
}

/// Resolves the best node ID to use.
/// 1. If `manual_id` is Some, it uses that (no failover).
/// 2. If `manual_id` is None, it tries the default node from config.
/// 3. If no default is set or the default node is not 'running', triggers selection.
pub async fn resolve_node_id(
    ctx: &KnaixContext,
    manual_id: Option<String>,
) -> Result<Option<String>> {
    if let Some(id) = manual_id {
        return Ok(Some(id));
    }

    // Get token or fail early
    let token = match ctx.config.token {
        Some(ref t) => t,
        None => return select_node_interactively(ctx).await,
    };

    // Lazy Validation: If we have a default node, check its health before committing
    if let Some(ref def_id) = ctx.config.default_node_id {
        let url = format!("{}/api/instances", ctx.config.api_url);

        if let Ok(resp) = ctx
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .send()
            .await
        {
            if resp.status().is_success() {
                let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
                let nodes_val = &wrapper["data"];
                if let Ok(nodes) = serde_json::from_value::<Vec<Node>>(nodes_val.clone()) {
                    let found = nodes
                        .iter()
                        .find(|n| n.instance_id.as_deref() == Some(def_id) || n.name == *def_id);

                    if let Some(node) = found {
                        if node.state == "running" {
                            return Ok(Some(def_id.clone()));
                        } else {
                            println!(
                                "{} Default node [{}] is {}, falling back to selection...",
                                "Info:".blue(),
                                def_id.cyan(),
                                node.state.yellow()
                            );
                        }
                    } else {
                        println!(
                            "{} Default node [{}] no longer exists, falling back to selection...",
                            "Info:".blue(),
                            def_id.cyan()
                        );
                    }
                }
            }
        }
    }

    // Fallback: full interactive selector
    select_node_interactively(ctx).await
}

pub async fn chat(
    ctx: &KnaixContext,
    node_id: &str,
    message: &str,
    stream_to_stdout: bool,
) -> Result<Option<String>> {
    let token = ctx.get_token()?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Locating node...");
    pb.enable_steady_tick(Duration::from_millis(100));

    // Resolve API key and connection metadata
    let instances_url = format!("{}/api/instances", ctx.config.api_url);
    let mut api_key = "knaix-private-key".to_string();
    let mut is_byot = false;
    let mut node_ip = String::new();

    if let Ok(resp) = ctx
        .client
        .get(&instances_url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
    {
        if resp.status().is_success() {
            let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
            let nodes_val = &wrapper["data"];
            let nodes: Vec<Node> = serde_json::from_value(nodes_val.clone()).unwrap_or_default();

            let target_node = nodes
                .iter()
                .find(|n| n.instance_id.as_deref() == Some(node_id) || n.name == node_id);

            if let Some(node) = target_node {
                if let Some(creds) = &node.credentials {
                    if let Some(key) = &creds.api_key {
                        api_key = key.clone();
                    }
                }
                if let Some(cfg) = &node.config {
                    is_byot = cfg.get("isByot").and_then(|v| v.as_bool()).unwrap_or(false);
                }
                if let Some(iid) = &node.instance_id {
                    node_ip = iid.clone();
                }
            }
        }
    }

    pb.set_message("Thinking...");

    let url;
    let req_builder;

    if is_byot {
        if node_ip.is_empty() {
            pb.finish_and_clear();
            return Err(anyhow!("BYOT node ID not found. Cannot resolve MagicDNS."));
        }
        url = format!(
            "http://{}:8080/api/v1/workspace/default/stream-chat",
            node_ip
        );
        req_builder = ctx
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key));
        pb.suspend(|| {
            println!(
                "{} {}",
                "[Mesh]".magenta(),
                format!("Direct Sovereign Link: http://{}:8080", node_ip).dimmed()
            );
        });
    } else {
        url = format!(
            "{}/api/nodes/{}/proxy/api/v1/workspace/default/stream-chat",
            ctx.config.api_url, node_id
        );
        req_builder = ctx
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .header("X-Target-Authorization", format!("Bearer {}", api_key));
    }

    let payload = serde_json::json!({ "message": message, "mode": "chat" });

    match req_builder.json(&payload).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                pb.finish_and_clear();

                if stream_to_stdout {
                    print!("{} ", "AI:".cyan().bold());
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }

                let mut stream = resp.bytes_stream();
                let mut assistant_text = String::new();

                while let Some(chunk_result) = stream.next().await {
                    if let Ok(chunk) = chunk_result {
                        let text = String::from_utf8_lossy(&chunk);
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let json_str = &line[6..];
                                if let Ok(data) =
                                    serde_json::from_str::<serde_json::Value>(json_str)
                                {
                                    if let Some(resp_text) = data["textResponse"].as_str() {
                                        if stream_to_stdout {
                                            print!("{}", resp_text);
                                            let _ = std::io::Write::flush(&mut std::io::stdout());
                                        }
                                        assistant_text.push_str(resp_text);
                                    }
                                }
                            }
                        }
                    }
                }

                if stream_to_stdout {
                    println!();
                }

                return Ok(Some(assistant_text));
            } else {
                pb.finish_and_clear();
                return Err(anyhow!("Chat failed on node: HTTP {}", resp.status()));
            }
        }
        Err(e) => {
            pb.finish_and_clear();
            return Err(e).context("Networking error during chat request");
        }
    }
}

pub async fn chat_silent(
    config: crate::config::Config,
    node_id: String,
    message: String,
) -> Result<Option<String>> {
    let token = config.token.context("Not logged in")?;
    let client = reqwest::Client::new();
    let instances_url = format!("{}/api/instances", config.api_url);
    let mut api_key = "knaix-private-key".to_string();
    let mut is_byot = false;
    let mut node_ip = String::new();

    if let Ok(resp) = client
        .get(&instances_url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
    {
        if resp.status().is_success() {
            let wrapper: serde_json::Value = resp.json().await.unwrap_or_default();
            let nodes_val = &wrapper["data"];
            let nodes: Vec<Node> = serde_json::from_value(nodes_val.clone()).unwrap_or_default();
            if let Some(node) = nodes
                .iter()
                .find(|n| n.instance_id.as_deref() == Some(&node_id) || n.name == node_id)
            {
                if let Some(creds) = &node.credentials {
                    if let Some(key) = &creds.api_key {
                        api_key = key.clone();
                    }
                }
                if let Some(cfg) = &node.config {
                    is_byot = cfg.get("isByot").and_then(|v| v.as_bool()).unwrap_or(false);
                }
                if let Some(iid) = &node.instance_id {
                    node_ip = iid.clone();
                }
            }
        }
    }

    let url;
    let req_builder;
    if is_byot {
        if node_ip.is_empty() {
            return Err(anyhow!("BYOT node ID not found"));
        }
        url = format!(
            "http://{}:8080/api/v1/workspace/default/stream-chat",
            node_ip
        );
        req_builder = client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", api_key));
    } else {
        url = format!(
            "{}/api/nodes/{}/proxy/api/v1/workspace/default/stream-chat",
            config.api_url, node_id
        );
        req_builder = client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .header("X-Target-Authorization", format!("Bearer {}", api_key));
    }

    let payload = serde_json::json!({ "message": message, "mode": "chat" });
    match req_builder.json(&payload).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                let mut stream = resp.bytes_stream();
                let mut assistant_text = String::new();
                while let Some(chunk_result) = stream.next().await {
                    if let Ok(chunk) = chunk_result {
                        let text = String::from_utf8_lossy(&chunk);
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let json_str = &line[6..];
                                if let Ok(data) =
                                    serde_json::from_str::<serde_json::Value>(json_str)
                                {
                                    if let Some(resp_text) = data["textResponse"].as_str() {
                                        assistant_text.push_str(resp_text);
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Some(assistant_text))
            } else {
                Err(anyhow!("Chat failed on node: HTTP {}", resp.status()))
            }
        }
        Err(e) => Err(e).context("Networking error during chat request"),
    }
}

pub async fn upload_single_file(
    ctx: &KnaixContext,
    node_id: &str,
    path: &Path,
    file_name: &str,
) -> Result<()> {
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
    let url = format!("{}/api/nodes/{}/documents", ctx.config.api_url, node_id);

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
                println!(
                    "  {} {} uploaded to knowledge base.",
                    "✓".green(),
                    file_name.white()
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

pub async fn upload(ctx: &KnaixContext, node_id: &str, file_path: &str) -> Result<()> {
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
                upload_single_file(ctx, node_id, path, &file_name).await?;
            }
        }
    } else {
        let file_name = base_path
            .file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("file")
            .to_string();
        upload_single_file(ctx, node_id, base_path, &file_name).await?;
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
    let payload = serde_json::json!({ "tier": "Community" });

    match ctx
        .client
        .post(&url)
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();

                // Get the instance ID from typical response or default to "Node"
                let instance_id = data
                    .get("data")
                    .and_then(|d| d.get("instanceId"))
                    .and_then(|i| i.as_str())
                    .unwrap_or("Node");

                pb.set_message(format!("[DONE] Requesting compute: {}", instance_id));
                // Simulate boot stages according to enterprise polish specification
                tokio::time::sleep(Duration::from_secs(1)).await;
                pb.set_message(format!("[BUSY] Booting kernel for {}...", instance_id));
                tokio::time::sleep(Duration::from_secs(2)).await;
                pb.set_message("[BUSY] Establishing Tailscale route...");
                tokio::time::sleep(Duration::from_secs(2)).await;

                pb.finish_with_message(format!(
                    "{} Node {} provisioned successfully.",
                    "✓".green(),
                    instance_id.cyan()
                ));
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
    let url = format!("{}/api/nodes/{}/documents", config.api_url, node_id);
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
                println!("  {} {}", "📄", f.cyan());
            }
        }
        println!();
    }

    Ok(())
}
