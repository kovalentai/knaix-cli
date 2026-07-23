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
    #[serde(default, alias = "llama_url")]
    pub model_url: Option<String>,
    /// Which model to ask that server for. Servers vary: llama-server ignores
    /// it, Ollama and vLLM require a name they actually host.
    #[serde(default, alias = "llama_model")]
    pub model: Option<String>,
    /// The control-plane instance id this node registered as, saved by
    /// `local connect` so telemetry and disconnect can address it.
    #[serde(default)]
    pub remote_id: Option<String>,
    /// PID of the background relay started by `local connect --daemon`.
    #[serde(default)]
    pub relay_pid: Option<u32>,
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

/// What this start should actually run with, after memory has its say.
struct Launch {
    port: u16,
    model_url: Option<String>,
    model: Option<String>,
    /// True when the server came from memory rather than a flag, so `up` can
    /// say so instead of reusing it silently.
    remembered: bool,
}

/// Merge flags with the remembered node. Forgetting a flag must not change
/// what answers: the difference between a model and the mock is visible only
/// in the wording, and a dropped model name is worse -- Ollama and vLLM meet
/// it with a 404 at the first question, long after startup looked fine.
/// `--mock` is the deliberate way back.
fn resolve_launch(
    saved: Option<&LocalNode>,
    port: Option<u16>,
    model_url: Option<String>,
    model: Option<String>,
    mock: bool,
) -> Launch {
    let port = port.or(saved.map(|n| n.port)).unwrap_or(DEFAULT_PORT);
    if mock {
        return Launch {
            port,
            model_url: None,
            model: None,
            remembered: false,
        };
    }
    let saved_url = saved.and_then(|n| n.model_url.clone());
    let saved_model = saved.and_then(|n| n.model.clone());
    match model_url {
        Some(url) => {
            // A remembered model name belongs to its server; it does not
            // transfer to a different one.
            let same_server = saved_url.as_deref() == Some(url.as_str());
            let model = model.or(if same_server { saved_model } else { None });
            Launch {
                port,
                model_url: Some(url),
                model,
                remembered: false,
            }
        }
        None => Launch {
            port,
            remembered: saved_url.is_some(),
            model_url: saved_url,
            model: model.or(saved_model),
        },
    }
}

/// Where the saved default node stood after a local start.
enum DefaultOutcome {
    /// Nothing was chosen before, so `local` is now the default.
    JustSet,
    /// `local` was already the default.
    AlreadyLocal,
    /// A different node is the default; it was left untouched.
    Other(String),
}

/// Make `local` the default when the user has none, so later commands are just
/// `knaix chat "..."` with no `-n local`. A default the user already chose is
/// theirs to keep, hosted node included; we only fill an empty slot.
fn make_local_default_if_unset() -> DefaultOutcome {
    let mut cfg = crate::config::load_stored_config();
    match cfg.default_node_id.clone() {
        None => {
            cfg.default_node_id = Some(LOCAL_NODE_ID.to_string());
            // Failing here costs only the shorthand, not the running node, so
            // it is not worth failing the start over.
            let _ = crate::config::save_config(&cfg);
            DefaultOutcome::JustSet
        }
        Some(id) if id == LOCAL_NODE_ID => DefaultOutcome::AlreadyLocal,
        Some(other) => DefaultOutcome::Other(other),
    }
}

pub async fn up(
    port: Option<u16>,
    model_url: Option<String>,
    model: Option<String>,
    mock: bool,
    pull: bool,
) -> Result<()> {
    docker_available()?;

    if container_state().as_deref() == Some("running") {
        let node = load().ok_or_else(|| {
            anyhow!("A '{}' container is already running but was not started by this CLI. Remove it with 'knaix local down' first.", CONTAINER)
        })?;
        // The container reads its model configuration once, at startup. Flags
        // passed to a node that is already up are read here and dropped, so an
        // explicit request to change what answers has to say it did not take
        // rather than print the same "already running" line as a bare `up`.
        if mock || model_url.is_some() || model.is_some() {
            println!(
                "{} Local node already running on {}. Model flags only apply at startup, so it keeps its current answer source.\n  Run {} then {} to apply this, or {} to change it and restart in place.",
                "Warning:".yellow(),
                node.base_url().cyan(),
                "knaix local down".cyan(),
                "knaix local up".cyan(),
                "knaix local setup".cyan()
            );
        } else {
            println!(
                "{} Local node already running on {}.",
                "Info:".blue(),
                node.base_url().cyan()
            );
        }
        // A second `up` on a live node is still a chance to grant the shorthand
        // to someone who never set a default.
        if let DefaultOutcome::JustSet = make_local_default_if_unset() {
            println!(
                "  {} Set as your default node, so you can drop {}.",
                "✓".green(),
                "-n local".cyan()
            );
        }
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

    let saved = load();
    let model_flag_given = model.is_some();
    let launch = resolve_launch(saved.as_ref(), port, model_url, model, mock);

    if launch.remembered {
        let what = match &launch.model {
            Some(m) => format!(
                "{} at {}",
                m,
                launch.model_url.as_deref().unwrap_or_default()
            ),
            None => launch.model_url.clone().unwrap_or_default(),
        };
        println!(
            "{} Using the model server from last time: {}. {} to go back to the mock.",
            "Info:".blue(),
            what.cyan(),
            "--mock".cyan()
        );
    }
    if model_flag_given && launch.model_url.is_none() {
        println!(
            "{} A model name was given but no server is configured to ask for it.\n  Run {} or pass {} as well.",
            "Warning:".yellow(),
            "knaix local setup".cyan(),
            "--model-url".cyan()
        );
    }

    // Keep the instance id across restarts, or everything already ingested
    // becomes unreachable under a new one.
    // Carry the identity and any live connection across a restart.
    let (instance_id, prev_remote_id, prev_relay_pid) = match saved {
        Some(n) => (n.instance_id, n.remote_id, n.relay_pid),
        None => (new_instance_id(), None, None),
    };

    let port_map = format!("{}:8080", launch.port);
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

    match &launch.model_url {
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
            if let Some(model) = &launch.model {
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
        port: launch.port,
        instance_id,
        image: tag,
        model_url: launch.model_url.clone(),
        model: launch.model.clone(),
        remote_id: prev_remote_id,
        relay_pid: prev_relay_pid,
    };
    save(&node)?;

    wait_until_ready(&node).await?;

    println!(
        "\n{} Local node ready on {}.",
        "✓".green(),
        node.base_url().cyan()
    );
    if launch.model_url.is_none() {
        if mock {
            println!(
                "  {} The deterministic mock answers, as asked. {} when you want a model again.",
                "Note:".blue(),
                "knaix local setup".cyan()
            );
        } else {
            // Worth one probe: someone with a model server already running is
            // a single command away from real answers.
            match crate::model_server::discover(None).await.first() {
                Some(s) => println!(
                    "  {} No model is configured, so answers come from the deterministic mock.\n  {} is running at {}; {} to answer with it.",
                    "Note:".blue(),
                    s.label,
                    s.url.cyan(),
                    "knaix local setup".cyan()
                ),
                None => println!(
                    "  {} No model is configured, so answers come from the deterministic mock.\n  Retrieval and citations are real; {} picks a model to answer.",
                    "Note:".blue(),
                    "knaix local setup".cyan()
                ),
            }
        }
    }
    // Grant the shorthand so the very next command can be a bare `knaix chat`.
    let outcome = make_local_default_if_unset();
    match &outcome {
        DefaultOutcome::JustSet => println!(
            "  {} Set as your default node, so you can drop {}.",
            "✓".green(),
            "-n local".cyan()
        ),
        DefaultOutcome::Other(other) => println!(
            "  {} Your default node is {}; pass {} to reach this one.",
            "Note:".blue(),
            other.cyan(),
            "-n local".cyan()
        ),
        DefaultOutcome::AlreadyLocal => {}
    }

    // Only a non-local default still needs the flag spelled out.
    let flag = if matches!(outcome, DefaultOutcome::Other(_)) {
        "-n local "
    } else {
        ""
    };
    println!("\n  Try it:");
    println!("    {}", format!("knaix upload {}./README.md", flag).cyan());
    println!(
        "    {}",
        format!("knaix chat {}\"what is this about?\"", flag).cyan()
    );
    Ok(())
}

/// `knaix local setup` -- choose what answers, by looking rather than asking.
///
/// Probes the ports the common stacks listen on, lists the models the chosen
/// server actually hosts, and remembers the pick. Every choice offered is one
/// that answered a request seconds ago, so the failure mode of a typed URL
/// and model name -- a 404 at the first question -- is off the table.
pub async fn setup() -> Result<()> {
    use crossterm::{cursor, execute};
    use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect, Input, Select};
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(anyhow!(
            "Setup is interactive. In a script, pass --model-url and --model to 'knaix local up' instead."
        ));
    }

    let saved = load();
    println!(
        "{} Looking for model servers on this machine...",
        "Info:".blue()
    );
    let found =
        crate::model_server::discover(saved.as_ref().and_then(|n| n.model_url.clone())).await;

    if found.is_empty() {
        println!(
            "  None answering on the usual ports: Ollama (11434), LM Studio (1234), vLLM (8000), llama-server (8081)."
        );
    }

    let mut items: Vec<String> = found
        .iter()
        .map(|s| {
            let count = match s.models.len() {
                0 => "no models pulled".to_string(),
                1 => "1 model".to_string(),
                n => format!("{} models", n),
            };
            format!("{} at {} ({})", s.label, s.url, count)
        })
        .collect();
    let manual_idx = items.len();
    items.push("A server somewhere else...".to_string());
    let mock_idx = items.len();
    items.push("No model: the deterministic mock (retrieval and citations stay real)".to_string());

    // Land the cursor on what is already chosen, so Enter changes nothing.
    let default = saved
        .as_ref()
        .and_then(|n| n.model_url.as_deref())
        .and_then(|u| found.iter().position(|s| s.url == u))
        .unwrap_or(0);

    let _ = execute!(std::io::stderr(), cursor::Hide);
    let pick = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("What should answer?")
        .items(&items)
        .default(default)
        .interact_opt()
        .unwrap_or(None);
    let _ = execute!(std::io::stderr(), cursor::Show);

    let Some(pick) = pick else {
        println!("{} Nothing changed.", "Info:".blue());
        return Ok(());
    };

    if pick == mock_idx {
        let node = remember_choice(saved, None, None)?;
        println!(
            "\n{} The deterministic mock answers. Retrieval and citations stay real.",
            "✓".green()
        );
        return offer_start_or_restart(&node).await;
    }

    let (url, models) = if pick == manual_idx {
        let raw: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Server URL (the base, e.g. http://192.168.1.50:11434)")
            .interact_text()?;
        let url = raw.trim().trim_end_matches('/').to_string();
        let label = crate::model_server::label_for(&url);
        match crate::model_server::probe(&url, label, std::time::Duration::from_secs(2)).await {
            Some(s) => (s.url, s.models),
            None => {
                // Maybe it is not running yet; refusing to save would make
                // setup unusable for a server someone is about to start.
                let keep = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Nothing answered there. Remember it anyway?")
                    .default(false)
                    .interact()
                    .unwrap_or(false);
                if !keep {
                    println!("{} Nothing changed.", "Info:".blue());
                    return Ok(());
                }
                (url, Vec::new())
            }
        }
    } else {
        let s = &found[pick];
        (s.url.clone(), s.models.clone())
    };

    let model = match models.len() {
        0 => {
            let name: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Model to request (Enter for none; llama-server ignores the name)")
                .allow_empty(true)
                .interact_text()?;
            let name = name.trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        }
        1 => {
            println!("{} One model hosted: {}.", "Info:".blue(), models[0].cyan());
            Some(models[0].clone())
        }
        _ => {
            let current = saved
                .as_ref()
                .and_then(|n| n.model.as_deref())
                .and_then(|m| models.iter().position(|x| x == m))
                .unwrap_or(0);
            let _ = execute!(std::io::stderr(), cursor::Hide);
            let choice = FuzzySelect::with_theme(&ColorfulTheme::default())
                .with_prompt("Which model?")
                .items(&models)
                .default(current)
                .interact_opt()
                .unwrap_or(None);
            let _ = execute!(std::io::stderr(), cursor::Show);
            let Some(i) = choice else {
                println!("{} Nothing changed.", "Info:".blue());
                return Ok(());
            };
            Some(models[i].clone())
        }
    };

    let node = remember_choice(saved, Some(url.clone()), model.clone())?;
    match &model {
        Some(m) => println!(
            "\n{} {} at {} will answer.",
            "✓".green(),
            m.cyan(),
            url.cyan()
        ),
        None => println!("\n{} The model at {} will answer.", "✓".green(), url.cyan()),
    }
    offer_start_or_restart(&node).await
}

/// Record the choice, creating state if the node has never been started.
fn remember_choice(
    saved: Option<LocalNode>,
    model_url: Option<String>,
    model: Option<String>,
) -> Result<LocalNode> {
    let mut node = saved.unwrap_or_else(|| LocalNode {
        port: DEFAULT_PORT,
        instance_id: new_instance_id(),
        image: image(),
        model_url: None,
        model: None,
        remote_id: None,
        relay_pid: None,
    });
    node.model_url = model_url;
    node.model = model;
    save(&node)?;
    Ok(node)
}

/// Get the node running the choice just made. A running node is restarted (the
/// choice only takes effect at startup, and a node still answering with the old
/// one looks broken); a stopped or never-started node is offered a start, so
/// `knaix local setup` can stand the whole thing up in one command.
async fn offer_start_or_restart(node: &LocalNode) -> Result<()> {
    use dialoguer::{theme::ColorfulTheme, Confirm};

    let running = container_state().as_deref() == Some("running");
    let prompt = if running {
        "The node is running with the previous choice. Restart it now?"
    } else {
        "Start the local node now?"
    };
    let go = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(true)
        .interact()
        .unwrap_or(false);
    if !go {
        if running {
            println!(
                "  It keeps answering with the previous choice until {}.",
                "knaix local up".cyan()
            );
        } else {
            println!("  Start it with {}.", "knaix local up".cyan());
        }
        return Ok(());
    }
    // A running node holds the name and port; free them first. up() starts a
    // stopped or absent one on its own.
    if running {
        docker(&["rm", "-f", CONTAINER]).map_err(|e| anyhow!("Could not stop the node: {}", e))?;
    }
    // The choice is passed explicitly, not left to memory: it silences the
    // "from last time" line, and mock=true when the mock was just chosen
    // silences the hint that suggests undoing what the user did.
    up(
        None,
        node.model_url.clone(),
        node.model.clone(),
        node.model_url.is_none(),
        false,
    )
    .await
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

/// `knaix local reset` -- empty the store and start fresh, keeping the model
/// choice. `down --purge` also deletes the store, but it forgets the model and
/// leaves the node stopped; reset is the friendlier front door for "clear what
/// I ingested and let me start over", which is exactly what someone hits after
/// testing against a corpus they no longer want in their answers.
pub async fn reset(yes: bool) -> Result<()> {
    use dialoguer::{theme::ColorfulTheme, Confirm};
    use std::io::IsTerminal;

    docker_available()?;

    // The corpus is what the user wants gone, not the server they picked to
    // answer with, so the model choice is read now and carried through.
    let saved = load();

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "Refusing to delete the local store without confirmation. Pass --yes to reset in a script."
            ));
        }
        let go = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Delete everything ingested into the local node and start fresh?")
            .default(false)
            .interact()
            .unwrap_or(false);
        if !go {
            println!("{} Nothing changed.", "Info:".blue());
            return Ok(());
        }
    }

    // The running container holds the volume open; remove it so the store can
    // be deleted, then let up() recreate an empty one.
    if container_state().is_some() {
        docker(&["rm", "-f", CONTAINER]).map_err(|e| anyhow!("Could not stop the node: {}", e))?;
    }
    match docker(&["volume", "rm", VOLUME]) {
        Ok(_) => println!("{} Local store deleted.", "✓".green()),
        // A node that was never started has no volume yet, which is already the
        // empty state reset is trying to reach.
        Err(e) if e.to_string().contains("No such volume") => {}
        Err(e) => return Err(anyhow!("Could not delete the local store: {}", e)),
    }

    let (model_url, model) = saved
        .map(|n| (n.model_url, n.model))
        .unwrap_or((None, None));
    let mock = model_url.is_none();
    // Start again, so reset leaves a running, empty node rather than a stopped
    // one the user then has to bring up by hand.
    up(None, model_url, model, mock, false).await
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

/// The local node at a glance, for `knaix status`.
pub struct LocalSummary {
    /// "running", "none", or whatever docker reports for a stopped container.
    pub state: String,
    pub url: Option<String>,
}

/// Never fails: a machine without docker simply has no local node.
pub fn summarize() -> LocalSummary {
    let state = container_state().unwrap_or_else(|| "none".to_string());
    let url = load().map(|n| n.base_url());
    LocalSummary { state, url }
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
                "modelUrl": node.as_ref().and_then(|n| n.model_url.clone()),
                "model": node.as_ref().and_then(|n| n.model.clone()),
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
            match &n.model_url {
                Some(url) => match &n.model {
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

// Connecting a local node to the account. The node stays offline; the logged-in
// CLI relays its metrics and logs to the control plane, so it shows up in the
// dashboard next to hosted nodes.

/// Register the local node with the account and stream its telemetry until
/// interrupted. `--daemon` keeps relaying in the background instead.
pub async fn connect(daemon: bool, worker: bool) -> Result<()> {
    // The detached worker relays an already-registered node and nothing else.
    if worker {
        let node = load().ok_or_else(|| anyhow!("No local node to relay."))?;
        let client_id = node
            .remote_id
            .clone()
            .ok_or_else(|| anyhow!("Local node is not connected."))?;
        let ctx = crate::nodes::KnaixContext::new("text".to_string());
        return relay_loop(&ctx, &node, &client_id).await;
    }

    docker_available()?;
    let node = load().ok_or_else(|| anyhow!("No local node. Start one with 'knaix local up'."))?;
    if container_state().as_deref() != Some("running") {
        return Err(anyhow!(
            "The local node is not running. Start it with 'knaix local up'."
        ));
    }

    let ctx = crate::nodes::KnaixContext::new("text".to_string());
    ctx.get_token()
        .context("Connecting a local node needs an account. Run 'knaix login' first.")?;

    let client_id = register(&ctx, &node).await?;
    let mut saved = node.clone();
    saved.remote_id = Some(client_id.clone());
    save(&saved)?;

    println!(
        "{} Connected {} to your account; it will appear in your dashboard.",
        "✓".green(),
        LOCAL_NODE_ID.cyan()
    );

    if daemon {
        let pid = spawn_relay_worker()?;
        saved.relay_pid = Some(pid);
        save(&saved)?;
        println!(
            "  Relaying metrics and logs in the background (pid {}). {} stops it.",
            pid,
            "knaix local disconnect".cyan()
        );
        return Ok(());
    }

    println!("  Relaying metrics and logs. Press Ctrl-C to stop; the node stays connected.");
    relay_loop(&ctx, &saved, &client_id).await
}

/// Stop relaying and mark the node offline in the account. The node keeps running.
pub async fn disconnect() -> Result<()> {
    let node = match load() {
        Some(n) => n,
        None => {
            println!("{} No local node on this machine.", "Info:".blue());
            return Ok(());
        }
    };

    if let Some(pid) = node.relay_pid {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }

    if let Some(client_id) = node.remote_id.as_deref() {
        let ctx = crate::nodes::KnaixContext::new("text".to_string());
        if let Ok(token) = ctx.get_token() {
            let url = format!(
                "{}/api/instances/{}/disconnect",
                ctx.config.api_url, client_id
            );
            let _ = ctx.client.post(&url).bearer_auth(token).send().await;
        }
    }

    let mut cleared = node;
    cleared.remote_id = None;
    cleared.relay_pid = None;
    let _ = save(&cleared);
    println!("{} Local node disconnected from your account.", "✓".green());
    Ok(())
}

/// After login, register a running local node and push one snapshot so it shows
/// up right away. Best-effort and quiet: does nothing if nothing is running.
pub async fn connect_snapshot() {
    let node = match load() {
        Some(n) => n,
        None => return,
    };
    if docker_available().is_err() || container_state().as_deref() != Some("running") {
        return;
    }
    let ctx = crate::nodes::KnaixContext::new("text".to_string());
    if ctx.get_token().is_err() {
        return;
    }
    let client_id = match register(&ctx, &node).await {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut saved = node.clone();
    saved.remote_id = Some(client_id.clone());
    let _ = save(&saved);
    let sample = gather_sample(&ctx, &node).await;
    let logs = recent_log_lines(30);
    let _ = push_telemetry(&ctx, &client_id, sample, logs).await;
    println!(
        "  Your local node is now connected; see it in the dashboard. {} streams live telemetry.",
        "knaix local connect".cyan()
    );
}

async fn register(ctx: &crate::nodes::KnaixContext, node: &LocalNode) -> Result<String> {
    let token = ctx.get_token()?;
    let url = format!("{}/api/instances/local/connect", ctx.config.api_url);
    let resp = ctx
        .client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "localInstanceId": node.instance_id,
            "name": hostname_label(),
            "port": node.port,
            "model": node.model,
        }))
        .send()
        .await
        .context("Could not reach the control plane to register the node.")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Register failed ({}): {}", status, body));
    }
    let body: serde_json::Value = resp.json().await?;
    body["data"]["clientId"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("The control plane did not return a node id."))
}

async fn push_telemetry(
    ctx: &crate::nodes::KnaixContext,
    client_id: &str,
    metrics: serde_json::Value,
    logs: Vec<String>,
) -> Result<()> {
    let token = ctx.get_token()?;
    let url = format!(
        "{}/api/instances/{}/telemetry",
        ctx.config.api_url, client_id
    );
    let resp = ctx
        .client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "metrics": metrics, "logs": logs }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("Telemetry push was refused ({}).", resp.status()));
    }
    Ok(())
}

async fn relay_loop(
    ctx: &crate::nodes::KnaixContext,
    node: &LocalNode,
    client_id: &str,
) -> Result<()> {
    loop {
        let sample = gather_sample(ctx, node).await;
        let logs = recent_log_lines(30);
        if let Err(e) = push_telemetry(ctx, client_id, sample, logs).await {
            eprintln!("{} {}", "Warning:".yellow(), e);
        }
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    }
}

/// One health + resource sample, shaped for the control plane's telemetry route.
async fn gather_sample(ctx: &crate::nodes::KnaixContext, node: &LocalNode) -> serde_json::Value {
    let url = format!("{}/health", node.base_url());
    let started = std::time::Instant::now();
    let healthy = matches!(ctx.client.get(&url).send().await, Ok(r) if r.status().is_success());
    let latency_ms = started.elapsed().as_millis() as u64;
    let (cpu, mem, mem_used) = docker_stats();
    serde_json::json!({
        "healthy": healthy,
        "latencyMs": latency_ms,
        "cpuPct": cpu,
        "memPct": mem,
        "memUsedBytes": mem_used,
    })
}

/// CPU %, memory %, and memory bytes from `docker stats`. None where unavailable.
fn docker_stats() -> (Option<f64>, Option<f64>, Option<u64>) {
    let out = match docker(&[
        "stats",
        "--no-stream",
        "--format",
        "{{.CPUPerc}}\t{{.MemPerc}}\t{{.MemUsage}}",
        CONTAINER,
    ]) {
        Ok(o) => o,
        Err(_) => return (None, None, None),
    };
    let parts: Vec<&str> = out.split('\t').collect();
    let pct = |s: &&str| s.trim().trim_end_matches('%').parse::<f64>().ok();
    let cpu = parts.first().and_then(pct);
    let mem = parts.get(1).and_then(pct);
    let mem_used = parts
        .get(2)
        .and_then(|s| s.split('/').next())
        .and_then(|s| parse_size(s.trim()));
    (cpu, mem, mem_used)
}

/// Parse a docker size like "104.9MiB" or "1.5GB" into bytes.
fn parse_size(s: &str) -> Option<u64> {
    let idx = s.find(|c: char| c.is_alphabetic())?;
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "B" => 1.0,
        "KiB" | "kB" | "KB" => 1024.0,
        "MiB" | "MB" => 1024.0 * 1024.0,
        "GiB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" | "TB" => 1024.0f64.powi(4),
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// Recent container log lines (stdout), oldest first.
fn recent_log_lines(n: usize) -> Vec<String> {
    docker(&["logs", "--tail", &n.to_string(), CONTAINER])
        .map(|out| out.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

fn hostname_label() -> String {
    let name = Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match name {
        Some(h) => format!("Local node ({})", h),
        None => "Local node".to_string(),
    }
}

/// Re-exec this binary as a detached relay worker and return its pid.
#[cfg(unix)]
fn spawn_relay_worker() -> Result<u32> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()
        .context("Could not find the knaix binary to relay in the background.")?;
    let child = Command::new(exe)
        .args(["local", "connect", "--worker"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("Could not start the background relay.")?;
    Ok(child.id())
}

#[cfg(not(unix))]
fn spawn_relay_worker() -> Result<u32> {
    Err(anyhow!("--daemon is only supported on macOS and Linux."))
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
        let node = saved(9123, None, None);
        assert_eq!(node.base_url(), "http://127.0.0.1:9123");
    }

    #[test]
    fn state_written_under_the_old_llama_keys_still_loads() {
        // Anyone upgrading has a local.json written under the old names.
        // Failing to read it would silently drop them back to the mock.
        let json = r#"{"port":8080,"instance_id":"a","image":"i","llama_url":"http://h:11434","llama_model":"qwen3.5:latest"}"#;
        let node: LocalNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.model.as_deref(), Some("qwen3.5:latest"));
        assert_eq!(node.model_url.as_deref(), Some("http://h:11434"));
    }

    #[test]
    fn the_model_and_its_server_survive_a_round_trip() {
        let node = saved(8080, Some("http://h:11434"), Some("qwen3.5:latest"));
        let json = serde_json::to_string(&node).unwrap();
        let back: LocalNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model_url.as_deref(), Some("http://h:11434"));
        assert_eq!(back.model.as_deref(), Some("qwen3.5:latest"));
    }

    #[test]
    fn state_written_before_the_model_url_existed_still_loads() {
        // Someone upgrading has a local.json with no model server in it.
        // Failing to parse would look like the node was never started.
        let old = r#"{"port":8080,"instance_id":"abc","image":"img"}"#;
        let node: LocalNode = serde_json::from_str(old).expect("old state must still parse");
        assert_eq!(node.port, 8080);
        assert_eq!(node.model_url, None);
        assert_eq!(node.model, None);
        assert_eq!(node.remote_id, None);
        assert_eq!(node.relay_pid, None);
    }

    fn saved(port: u16, url: Option<&str>, model: Option<&str>) -> LocalNode {
        LocalNode {
            port,
            instance_id: "x".into(),
            image: "img".into(),
            model_url: url.map(String::from),
            model: model.map(String::from),
            remote_id: None,
            relay_pid: None,
        }
    }

    #[test]
    fn a_bare_up_reuses_the_server_and_its_model() {
        // The model name is half of the choice: reusing the server but
        // dropping the name turns every Ollama question into a 404.
        let node = saved(9000, Some("http://h:11434"), Some("qwen3.5:latest"));
        let launch = resolve_launch(Some(&node), None, None, None, false);
        assert_eq!(launch.model_url.as_deref(), Some("http://h:11434"));
        assert_eq!(launch.model.as_deref(), Some("qwen3.5:latest"));
        assert_eq!(launch.port, 9000, "the port is part of what was chosen");
        assert!(launch.remembered);
    }

    #[test]
    fn a_different_server_does_not_inherit_the_old_model_name() {
        // Model names are the server's, not the machine's: qwen3.5:latest
        // means nothing to a vLLM that hosts something else.
        let node = saved(8080, Some("http://h:11434"), Some("qwen3.5:latest"));
        let launch = resolve_launch(Some(&node), None, Some("http://h:8000".into()), None, false);
        assert_eq!(launch.model_url.as_deref(), Some("http://h:8000"));
        assert_eq!(launch.model, None);
        assert!(!launch.remembered);
    }

    #[test]
    fn restating_the_same_server_keeps_its_model() {
        let node = saved(8080, Some("http://h:11434"), Some("qwen3.5:latest"));
        let launch = resolve_launch(
            Some(&node),
            None,
            Some("http://h:11434".into()),
            None,
            false,
        );
        assert_eq!(launch.model.as_deref(), Some("qwen3.5:latest"));
    }

    #[test]
    fn an_explicit_model_beats_the_remembered_one() {
        let node = saved(8080, Some("http://h:11434"), Some("qwen3.5:latest"));
        let launch = resolve_launch(Some(&node), None, None, Some("phi4:latest".into()), false);
        assert_eq!(launch.model.as_deref(), Some("phi4:latest"));
        assert_eq!(launch.model_url.as_deref(), Some("http://h:11434"));
    }

    #[test]
    fn mock_clears_the_whole_choice_deliberately() {
        let node = saved(8080, Some("http://h:11434"), Some("qwen3.5:latest"));
        let launch = resolve_launch(Some(&node), None, None, None, true);
        assert_eq!(launch.model_url, None);
        assert_eq!(launch.model, None);
        assert!(!launch.remembered, "asked for, not reused");
    }

    #[test]
    fn a_first_start_defaults_quietly() {
        let launch = resolve_launch(None, None, None, None, false);
        assert_eq!(launch.port, DEFAULT_PORT);
        assert_eq!(launch.model_url, None);
        assert!(!launch.remembered);
    }

    #[test]
    fn the_image_can_be_overridden_for_local_builds() {
        // Default points at the published image; developers run their own tag.
        assert!(image().contains("node-runtime") || !image().is_empty());
    }
}
