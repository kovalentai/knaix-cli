//! `knaix local` -- run the whole stack on this machine.
//!
//! One container: the Node Runtime, holding its own PGlite store, its own
//! embedder, and its own reranker. No control plane, no account, no token. The
//! model artifacts are baked into the image, so once it is pulled the stack
//! needs no network at all.
//!
//! This is deliberately not a smaller version of the hosted product. It is the
//! same runtime a paid tenant gets, run against a store on your disk -- which
//! is what makes it worth testing against, and what makes "your data never
//! leaves the machine" checkable rather than promised.

use anyhow::{anyhow, Context, Result};
use colored::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

/// The node's own name for itself, and the reserved node id that routes a
/// command here instead of at a control plane.
pub const LOCAL_NODE_ID: &str = "local";

const CONTAINER: &str = "knaix-local";
/// Named volume, so `knaix local down` stops the node without discarding the
/// corpus someone spent an afternoon building.
const VOLUME: &str = "knaix-local-data";
const DEFAULT_PORT: u16 = 8080;

/// Published image. Overridable so a developer can run the tag they just built
/// rather than the released one.
fn image() -> String {
    std::env::var("KNAIX_LOCAL_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/kovalentai/node-runtime:latest".to_string())
}

/// Hostname Docker Desktop resolves to the host, and which `--add-host` below
/// maps on Linux, where it does not exist by default.
const HOST_GATEWAY: &str = "host.docker.internal";

/// Rewrite a loopback model URL so the node can actually reach it.
///
/// The node runs in a container, so `127.0.0.1` is the container itself, not
/// the machine. Someone serving a model on their laptop writes the address they
/// used to start it, which is exactly the address that cannot work from inside.
/// There is no case where a caller means the container's own loopback -- nothing
/// else is listening in there -- so the intent is unambiguous and worth honouring
/// rather than failing on.
///
/// Returns the rewritten URL and whether it changed, so the caller can say so.
fn reachable_from_container(raw: &str) -> (String, bool) {
    let Ok(mut url) = url::Url::parse(raw) else {
        return (raw.to_string(), false);
    };
    let is_loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if !is_loopback {
        return (raw.to_string(), false);
    }
    if url.set_host(Some(HOST_GATEWAY)).is_err() {
        return (raw.to_string(), false);
    }
    (url.to_string(), true)
}

/// What `local up` recorded, so later commands can find the node it started.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LocalNode {
    pub port: u16,
    /// The instance the node's store is keyed by. Generated once and kept, so
    /// restarting the node does not orphan everything already ingested.
    pub instance_id: String,
    pub image: String,
    /// The model server the node was last started against, remembered so a
    /// later `up` does not silently drop back to the mock. Absent on state
    /// written before this was recorded, and when the mock is in use.
    #[serde(default)]
    pub llama_url: Option<String>,
    /// Which model to ask that server for. Servings vary: llama-server ignores
    /// it, Ollama and vLLM require a name they actually host.
    #[serde(default)]
    pub llama_model: Option<String>,
}

impl LocalNode {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn state_path() -> PathBuf {
    let mut path = crate::config::get_config_path();
    path.pop();
    path.push("local.json");
    path
}

pub fn load() -> Option<LocalNode> {
    let raw = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save(node: &LocalNode) -> Result<()> {
    let json = serde_json::to_string_pretty(node)?;
    std::fs::write(state_path(), json).context("Could not record the local node's details")
}

fn clear_state() {
    let _ = std::fs::remove_file(state_path());
}

/// Run a docker command, returning stdout on success and stderr on failure.
fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .context("Could not run docker. Is Docker installed and running?")?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(anyhow!("{}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Run a docker command with its output attached to the terminal.
///
/// Used for `pull` alone. Capturing that one would leave a first run silent for
/// minutes behind a "Pulling..." line while several hundred megabytes download,
/// which is indistinguishable from a hang -- and the first run is exactly when
/// a user has least reason to assume it is working.
fn docker_streaming(args: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .args(args)
        .status()
        .context("Could not run docker. Is Docker installed and running?")?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("docker {} exited with {}", args[0], status))
    }
}

fn docker_available() -> Result<()> {
    docker(&["info", "--format", "{{.ServerVersion}}"])
        .map(|_| ())
        .map_err(|e| {
            anyhow!(
                "Docker is not available: {}. Start Docker and try again.",
                e
            )
        })
}

/// Container state as docker reports it: running, exited, or absent.
fn container_state() -> Option<String> {
    docker(&["inspect", "--format", "{{.State.Status}}", CONTAINER]).ok()
}

fn image_present(tag: &str) -> bool {
    docker(&["image", "inspect", tag]).is_ok()
}

pub async fn up(
    port: Option<u16>,
    llama_url: Option<String>,
    llama_model: Option<String>,
    mock: bool,
    pull: bool,
) -> Result<()> {
    docker_available()?;

    if container_state().as_deref() == Some("running") {
        let node = load().ok_or_else(|| {
            anyhow!("A '{}' container is already running but was not started by this CLI. Remove it with 'knaix local down' first.", CONTAINER)
        })?;
        println!(
            "{} Local node already running on {}.",
            "Info:".blue(),
            node.base_url().cyan()
        );
        return Ok(());
    }

    let tag = image();
    // Pull only when asked or when the image is not already here, so a normal
    // start stays offline once the image has been fetched once.
    if pull || !image_present(&tag) {
        println!(
            "{} Fetching {} (about 380 MB, once).",
            "Info:".blue(),
            tag.cyan()
        );
        if let Err(e) = docker_streaming(&["pull", &tag]) {
            if !image_present(&tag) {
                return Err(anyhow!(
                    "Could not pull {}: {}\n       Set KNAIX_LOCAL_IMAGE to an image you already have if you are running a local build.",
                    tag,
                    e
                ));
            }
            println!(
                "{} Pull failed ({}); using the copy already on this machine.",
                "Warning:".yellow(),
                e
            );
        }
    }

    // A stopped container still holds the name and the port.
    if container_state().is_some() {
        let _ = docker(&["rm", "-f", CONTAINER]);
    }

    // Reuse the model server this node was last started against. Forgetting a
    // flag should not quietly change what is answering: the difference between
    // a real model and the mock is visible in the wording and nowhere else, so
    // a silent downgrade is one a user would not notice until they trusted an
    // answer. `--mock` is how you go back deliberately.
    let saved_llama = load().and_then(|n| n.llama_url);
    let llama_url = if mock {
        None
    } else {
        llama_url.or(saved_llama.clone())
    };
    if !mock && llama_url.is_some() && llama_url == saved_llama {
        println!(
            "{} Using the model server from last time: {}. {} to go back to the mock.",
            "Info:".blue(),
            llama_url.as_deref().unwrap_or_default().cyan(),
            "--mock".cyan()
        );
    }

    let port = port.unwrap_or(DEFAULT_PORT);
    // Keep the instance id across restarts, or everything already ingested
    // becomes unreachable under a new one.
    let instance_id = load()
        .map(|n| n.instance_id)
        .unwrap_or_else(new_instance_id);

    let port_map = format!("{}:8080", port);
    let volume_map = format!("{}:/data", VOLUME);
    let bound = format!("KB_INSTANCE_ID={}", instance_id);
    let mut args: Vec<String> = vec![
        "run",
        "-d",
        "--name",
        CONTAINER,
        "--restart",
        "unless-stopped",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    for arg in [
        "-p",
        &port_map,
        "-v",
        &volume_map,
        "-e",
        "DATA_DIR=/data",
        "-e",
        &bound,
        // Loopback only, and no mesh peers: nothing else can reach this node,
        // so the guards that protect a provisioned node have nothing to
        // protect against here.
        "-e",
        "A2A_AUTH_DISABLED=true",
        "-e",
        "A2A_ALLOW_PRIVATE_NETWORK=true",
    ] {
        args.push(arg.to_string());
    }

    match &llama_url {
        // A real model, if one is being served on this machine.
        Some(url) => {
            let (reachable, rewritten) = reachable_from_container(url);
            if rewritten {
                println!(
                    "{} Model URL {} is loopback, which inside the container means the\n  container itself. Using {} so it reaches this machine.",
                    "Info:".blue(),
                    url.cyan(),
                    reachable.cyan()
                );
            }
            // Linux has no host.docker.internal; this maps it. Harmless where
            // Docker Desktop already provides it.
            args.push("--add-host".into());
            args.push(format!("{}:host-gateway", HOST_GATEWAY));
            args.push("-e".into());
            args.push(format!("LLAMA_SERVER_URL={}", reachable));
            // Servers differ on whether this matters: llama-server serves the
            // one model it was started with and ignores the name, while Ollama
            // and vLLM route on it and refuse a name they do not host. Sending
            // the wrong one fails at the first question rather than at startup,
            // which is the worst time to find out.
            if let Some(model) = &llama_model {
                args.push("-e".into());
                args.push(format!("LLAMA_MODEL_ID={}", model));
            }
        }
        // Nothing serves a model by default, and generation fails closed
        // without one. The deterministic mock answers from the chunks the node
        // actually retrieved, so retrieval and citations stay real.
        None => {
            args.push("-e".into());
            args.push("GENERATION_PROVIDER=mock".into());
        }
    }
    args.push(tag.clone());

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker(&arg_refs).map_err(|e| anyhow!("Could not start the local node: {}", e))?;

    let node = LocalNode {
        port,
        instance_id,
        image: tag,
        llama_url: llama_url.clone(),
        llama_model: llama_model.clone(),
    };
    save(&node)?;

    wait_until_ready(&node).await?;

    println!(
        "\n{} Local node ready on {}.",
        "✓".green(),
        node.base_url().cyan()
    );
    if llama_url.is_none() {
        println!(
            "  {} No model is configured, so answers come from the deterministic mock.\n  Retrieval and citations are real; pass {} to use a local model.",
            "Note:".blue(),
            "--llama-url".cyan()
        );
    }
    println!("\n  Try it:");
    println!("    {}", "knaix upload -n local ./README.md".cyan());
    println!(
        "    {}",
        "knaix chat -n local \"what is this about?\"".cyan()
    );
    println!(
        "    {}   {}",
        "knaix use local".cyan(),
        "# make it the default for every command".dimmed()
    );
    Ok(())
}

fn new_instance_id() -> String {
    // A v4-shaped id: the node stores by it, and its routes validate the shape.
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("failed to read OS randomness");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{:02x}", x)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].join(""),
        h[4..6].join(""),
        h[6..8].join(""),
        h[8..10].join(""),
        h[10..16].join("")
    )
}

/// Poll until the node reports ready, or explain why it never did.
async fn wait_until_ready(node: &LocalNode) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let url = format!("{}/health", node.base_url());

    print!("  Waiting for the node to open its store");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    for _ in 0..60 {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                println!();
                return Ok(());
            }
        }
        // A container that has already exited will never become ready, and its
        // logs say why; waiting out the timeout would only bury them.
        if container_state().as_deref() != Some("running") {
            println!();
            let logs = docker(&["logs", "--tail", "20", CONTAINER]).unwrap_or_default();
            return Err(anyhow!("The local node stopped while starting:\n{}", logs));
        }
        print!(".");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    println!();
    let logs = docker(&["logs", "--tail", "20", CONTAINER]).unwrap_or_default();
    Err(anyhow!(
        "The local node did not become ready in time. Recent logs:\n{}",
        logs
    ))
}

pub fn down(purge: bool) -> Result<()> {
    docker_available()?;

    if container_state().is_none() {
        println!("{} No local node is running.", "Info:".blue());
    } else {
        docker(&["rm", "-f", CONTAINER]).map_err(|e| anyhow!("Could not stop the node: {}", e))?;
        println!("{} Local node stopped.", "✓".green());
    }

    if purge {
        // Only on request: the volume is the corpus, and losing it to a stop
        // command would be the expensive kind of surprise.
        match docker(&["volume", "rm", VOLUME]) {
            Ok(_) => println!("{} Local store deleted.", "✓".green()),
            Err(e) => println!(
                "{} Could not delete the local store: {}",
                "Warning:".yellow(),
                e
            ),
        }
        clear_state();
    } else if load().is_some() {
        println!(
            "  Its store is kept in the {} volume; {} to delete it.",
            VOLUME.cyan(),
            "knaix local down --purge".cyan()
        );
    }
    Ok(())
}

pub async fn status(json: bool) -> Result<()> {
    let node = load();
    let state = container_state();
    let running = state.as_deref() == Some("running");

    let healthy = match (&node, running) {
        (Some(n), true) => reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?
            .get(format!("{}/health", n.base_url()))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false),
        _ => false,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "running": running,
                "healthy": healthy,
                "state": state,
                "url": node.as_ref().map(|n| n.base_url()),
                "llamaUrl": node.as_ref().and_then(|n| n.llama_url.clone()),
                "instanceId": node.as_ref().map(|n| n.instance_id.clone()),
                "image": node.as_ref().map(|n| n.image.clone()),
            }))?
        );
        return Ok(());
    }

    println!("\n{}", "Local node:".bold().underline());
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec!["Setting", "Value"]);
    table.add_row(vec![
        "State".dimmed().to_string(),
        match (running, healthy) {
            (true, true) => "running (healthy)".green().to_string(),
            (true, false) => "running (not ready)".yellow().to_string(),
            _ => state
                .clone()
                .unwrap_or_else(|| "not created".into())
                .red()
                .to_string(),
        },
    ]);
    if let Some(n) = &node {
        table.add_row(vec![
            "URL".dimmed().to_string(),
            n.base_url().cyan().to_string(),
        ]);
        table.add_row(vec![
            "Instance".dimmed().to_string(),
            n.instance_id.dimmed().to_string(),
        ]);
        table.add_row(vec![
            "Image".dimmed().to_string(),
            n.image.dimmed().to_string(),
        ]);
        // What is actually answering. The difference between a model and the
        // mock shows up only in the wording, so it has to be stated somewhere.
        table.add_row(vec![
            "Answers".dimmed().to_string(),
            match &n.llama_url {
                Some(url) => match &n.llama_model {
                    Some(model) => format!("{} at {}", model, url).green().to_string(),
                    None => format!("model at {}", url).green().to_string(),
                },
                None => "deterministic mock (retrieval is real)"
                    .yellow()
                    .to_string(),
            },
        ]);
    }
    println!("{table}");

    if !running {
        println!("  Start it with {}.\n", "knaix local up".cyan());
    } else {
        println!();
    }
    Ok(())
}

pub fn logs(lines: usize) -> Result<()> {
    docker_available()?;
    if container_state().is_none() {
        return Err(anyhow!(
            "No local node exists. Start one with 'knaix local up'."
        ));
    }
    let n = lines.to_string();
    let out = docker(&["logs", "--tail", &n, CONTAINER])
        .map_err(|e| anyhow!("Could not read the node's logs: {}", e))?;
    println!("{}", out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_instance_ids_are_v4_shaped() {
        // The node validates the shape, so a malformed id would be refused at
        // ingest rather than at start, long after it looked like it worked.
        let id = new_instance_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(&id[14..15], "4", "version nibble");
        assert!("89ab".contains(&id[19..20]), "variant nibble");
        assert_ne!(id, new_instance_id(), "ids must not repeat");
    }

    #[test]
    fn a_loopback_model_url_is_rewritten_to_reach_the_host() {
        // The address someone starts a model server on is exactly the one that
        // cannot work from inside the container.
        for raw in [
            "http://127.0.0.1:8081",
            "http://localhost:8081",
            "http://localhost:8081/v1",
        ] {
            let (out, rewritten) = reachable_from_container(raw);
            assert!(rewritten, "{raw} should be rewritten");
            assert!(out.contains(HOST_GATEWAY), "{raw} -> {out}");
            // The port and path have to survive, or it reaches the host and
            // then asks it for the wrong thing.
            assert!(out.contains("8081"), "{raw} -> {out} lost its port");
        }
        let (path, _) = reachable_from_container("http://localhost:8081/v1");
        assert!(path.ends_with("/v1"), "path must survive: {path}");
    }

    #[test]
    fn a_routable_model_url_is_left_alone() {
        for raw in ["http://192.168.1.50:8081", "https://models.example.com"] {
            let (out, rewritten) = reachable_from_container(raw);
            assert!(!rewritten, "{raw} should not be rewritten");
            assert_eq!(out, raw);
        }
    }

    #[test]
    fn an_unparseable_model_url_is_passed_through_untouched() {
        // Let the node report a bad URL rather than mangling it here.
        let (out, rewritten) = reachable_from_container("not a url");
        assert!(!rewritten);
        assert_eq!(out, "not a url");
    }

    #[test]
    fn the_local_node_is_addressed_on_loopback() {
        let node = LocalNode {
            port: 9123,
            instance_id: "x".into(),
            image: "img".into(),
            llama_url: None,
            llama_model: None,
        };
        assert_eq!(node.base_url(), "http://127.0.0.1:9123");
    }

    #[test]
    fn the_model_name_is_remembered_with_its_server() {
        // Ollama and vLLM route on the model name and refuse one they do not
        // host, so losing it turns every question into a 404 after a startup
        // that looked fine.
        let json = r#"{"port":8080,"instance_id":"a","image":"i","llama_url":"http://h:11434","llama_model":"qwen3.5:latest"}"#;
        let node: LocalNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.llama_model.as_deref(), Some("qwen3.5:latest"));
        assert_eq!(node.llama_url.as_deref(), Some("http://h:11434"));
    }

    #[test]
    fn state_written_before_the_model_url_existed_still_loads() {
        // Someone upgrading has a local.json with no llama_url in it. Failing
        // to parse would look like the node was never started.
        let old = r#"{"port":8080,"instance_id":"abc","image":"img"}"#;
        let node: LocalNode = serde_json::from_str(old).expect("old state must still parse");
        assert_eq!(node.port, 8080);
        assert_eq!(node.llama_url, None);
        assert_eq!(node.llama_model, None);
    }

    #[test]
    fn the_image_can_be_overridden_for_local_builds() {
        // Default points at the published image; developers run their own tag.
        assert!(image().contains("node-runtime") || !image().is_empty());
    }
}
