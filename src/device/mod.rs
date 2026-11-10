//! `knaix device`: lend this machine to a pooled model (ADR 0013).
//!
//! A device runs llama.cpp's `rpc-server` on loopback, inside a sandbox, behind
//! a gateway that admits only connections opening with a ticket the control
//! plane signed for this device. The pool's coordinator reaches the gateway
//! over the tailnet; nothing else can reach anything.
//!
//! - `join` enrols the machine and starts it (in the foreground, or as a user
//!   service with `--daemon`).
//! - `status`, `ls`, `logs` report on it.
//! - `rekey` confirms the control plane's new ticket key after a rotation.
//! - `leave` stops it and removes it from the account.
//! - `run` (hidden) is what the service starts.

pub mod api;
pub mod bridge;
pub mod gateway;
pub mod hardware;
pub mod llama;
pub mod runner;
pub mod sandbox;
pub mod service;
pub mod state;
pub mod tailnet;
pub mod ticket;

use crate::brand;
use crate::exit::{Code, WithCode};
use crate::nodes::KnaixContext;
use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use colored::*;
use serde_json::{json, Value};
use std::io::IsTerminal;

/// The gateway's default port on the tailnet address.
pub const DEFAULT_PORT: u16 = 47100;

#[derive(Subcommand)]
pub enum DeviceAction {
    /// Lend this machine's GPU or memory to a pooled model
    ///
    /// Installs the pinned llama.cpp rpc-server, connects Tailscale with the
    /// device tag, enrols the machine in your account, and starts it. Runs in
    /// the foreground until Ctrl-C, or as a background service with --daemon.
    Join {
        /// What to call this machine in your account (defaults to its hostname)
        #[clap(long)]
        name: Option<String>,
        /// Offer at most this much memory to pools, e.g. 12G
        #[clap(long, value_name = "SIZE")]
        max_memory: Option<String>,
        /// The gateway's port on the tailnet address
        #[clap(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Use this backend instead of detecting one: metal, cuda, rocm, vulkan or cpu
        #[clap(long)]
        backend: Option<String>,
        /// Keep running in the background, and start again after a restart
        #[clap(long)]
        daemon: bool,
        /// Switch Tailscale to Kovalent's network without asking, if it is on another
        #[clap(long)]
        yes: bool,
    },

    /// This machine as a device: enrolment, gateway, heartbeat, warnings
    Status,

    /// The compute devices in your account
    #[clap(alias = "list")]
    Ls,

    /// Stop lending this machine and remove it from your account
    Leave {
        /// Do not ask for confirmation
        #[clap(long)]
        yes: bool,
        /// Also delete the downloaded llama.cpp build, the tensor cache and the logs
        #[clap(long)]
        purge: bool,
    },

    /// Confirm the control plane's new ticket key after a rotation
    Rekey {
        /// Do not ask for confirmation
        #[clap(long)]
        yes: bool,
    },

    /// The gateway's log (every refused connection, with why) and rpc-server's output
    Logs {
        /// Number of lines to show from each log
        #[clap(short = 'n', long, default_value = "50")]
        lines: usize,
    },

    /// Internal: run the device (what the background service starts)
    #[clap(hide = true)]
    Run,
}

pub async fn dispatch(ctx: &KnaixContext, action: DeviceAction) -> Result<()> {
    match action {
        DeviceAction::Join {
            name,
            max_memory,
            port,
            backend,
            daemon,
            yes,
        } => join(ctx, name, max_memory, port, backend, daemon, yes).await,
        DeviceAction::Status => status(ctx).await,
        DeviceAction::Ls => ls(ctx).await,
        DeviceAction::Leave { yes, purge } => leave(ctx, yes, purge).await,
        DeviceAction::Rekey { yes } => rekey(ctx, yes).await,
        DeviceAction::Logs { lines } => logs(lines),
        DeviceAction::Run => run_service().await,
    }
}

fn supported() -> Result<()> {
    if cfg!(windows) {
        return Err(anyhow!(
            "Compute devices are not supported on Windows yet. Use a Mac with Apple silicon or a Linux machine."
        ))
        .coded(Code::Precondition);
    }
    Ok(())
}

fn confirm(prompt: &str, yes: bool, refuse: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!("{refuse} Pass --yes to go ahead without a prompt."))
            .coded(Code::Denied);
    }
    Ok(
        dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(prompt)
            .default(false)
            .interact()
            .unwrap_or(false),
    )
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".to_string())
}

/// A name Tailscale accepts as a hostname.
pub fn tailscale_hostname(name: &str) -> String {
    let mut out: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-');
    let out = if out.is_empty() { "device" } else { out };
    out.chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// The words for each warning the control plane can attach.
pub fn explain_warning(w: &str) -> String {
    if let Some(floor) = w.strip_prefix("llama_build_below_floor:") {
        return format!("llama.cpp is older than the minimum safe build ({floor}); pools will not use this device.");
    }
    match w {
        "llama_build_unknown" => {
            "The control plane does not know which llama.cpp build this device runs.".into()
        }
        "llama_build_blocked" => {
            "This llama.cpp build is blocked for a known problem; pools will not use this device."
                .into()
        }
        "no_gateway_endpoint" => {
            "The gateway is not serving, so pools cannot reach this device.".into()
        }
        other => other.to_string(),
    }
}

fn removed_marker() -> Option<std::path::PathBuf> {
    state::device_dir().ok().map(|d| d.join("removed"))
}

#[allow(clippy::too_many_arguments)]
async fn join(
    ctx: &KnaixContext,
    name: Option<String>,
    max_memory: Option<String>,
    port: u16,
    backend: Option<String>,
    daemon: bool,
    yes: bool,
) -> Result<()> {
    supported()?;
    let session = ctx.get_token()?.clone();
    let api_url = ctx.config.api_url.trim_end_matches('/').to_string();
    let max = max_memory
        .as_deref()
        .map(hardware::parse_size)
        .transpose()?;
    if port == 0 {
        return Err(anyhow!("--port must be a port number, not 0.")).coded(Code::Usage);
    }

    if let Some(pid) = runner::running_pid() {
        let device = state::load()?;
        let name = device.map(|d| d.name).unwrap_or_default();
        ctx.info(&format!(
            "{} This machine is already lending itself as {} (process {pid}). See {}.",
            "Info:".blue(),
            name.bold(),
            brand::cmd("device status")
        ));
        return Ok(());
    }

    let existing = unless_removed(ctx, &api_url, &session, state::load()?).await?;
    let requested = backend
        .as_deref()
        .map(hardware::Backend::parse)
        .transpose()?;
    let requested = requested.or_else(|| {
        existing
            .as_ref()
            .and_then(|d| hardware::Backend::parse(&d.backend).ok())
    });

    // 1. What this machine offers.
    let offer = hardware::detect(
        requested,
        max.or(existing.as_ref().and_then(|d| d.max_memory_bytes)),
    )?;
    ctx.info(&format!(
        "  {} {} with {} to offer ({})",
        "Hardware:".dimmed(),
        offer.backend.as_str().bold(),
        crate::hardware::human_size(offer.usable_bytes).bold(),
        offer.detail
    ));

    // 2. The pinned llama.cpp build.
    let variant = llama::variant_for(std::env::consts::OS, std::env::consts::ARCH, offer.backend)?;
    if llama::verify(variant).is_err() {
        let total: u64 = variant.archives.iter().map(|a| a.bytes).sum();
        ctx.info(&format!(
            "  {} downloading llama.cpp {} ({}), checked against its pinned checksum",
            "llama.cpp:".dimmed(),
            llama::TAG,
            crate::hardware::human_size(total)
        ));
    }
    let binary = llama::ensure_installed(&ctx.client, variant, ctx.show_progress()).await?;
    ctx.info(&format!(
        "  {} {} verified",
        "llama.cpp:".dimmed(),
        llama::BUILD
    ));

    // 3. A sandbox that actually starts, before anything is enrolled.
    let cache = state::rpc_cache_dir()?;
    let install = llama::install_dir(variant);
    let exe = std::env::current_exe().context("Could not find the knaix binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let socket_dir = state::rpc_socket_dir()?;
    let plan = sandbox::choose(
        &sandbox::Paths {
            binary: &binary,
            install: &install,
            cache: &cache,
            exe: &exe,
            socket_dir: &socket_dir,
            port: 1,
        },
        &llama::runtime_env(&install, &cache),
    )?;
    ctx.info(&format!("  {} {}", "Sandbox:".dimmed(), plan.kind.as_str()));

    // 4. The tailnet.
    let Some(net) = connect_tailnet(
        ctx,
        &session,
        &api_url,
        &tailnet::require()?,
        tailnet::elevate(),
        existing.as_ref().is_some_and(|d| d.joined_tailnet_by_knaix),
        name.as_deref(),
        port,
        &|prompt, refuse| confirm(prompt, yes, refuse),
    )
    .await?
    else {
        return Ok(());
    };

    // 5. Enrol, unless this machine already is a device of this account.
    let device = enrol(
        ctx, &session, &api_url, existing, name, max, port, &offer, variant, net,
    )
    .await?;

    // 6. Start.
    if daemon {
        return background(ctx, &device.name, &service::system());
    }
    ctx.info(&format!(
        "\n{} {} is lending itself to your pools. Press Ctrl-C to stop; run with {} to keep it running in the background.",
        "✓".green(),
        device.name.bold(),
        "--daemon".cyan()
    ));
    let quiet = ctx.quiet;
    runner::run(&move |line: &str| {
        if !quiet {
            println!("{line}");
        }
    })
    .await
}

/// `existing`, unless the account no longer lists it. A record whose device was
/// removed from the account (from the dashboard, say) would only be refused at
/// the first heartbeat, so it is forgotten and the machine enrolled afresh.
/// Kept when the account cannot be asked: the heartbeat will tell.
async fn unless_removed(
    ctx: &KnaixContext,
    api_url: &str,
    session: &str,
    existing: Option<state::DeviceState>,
) -> Result<Option<state::DeviceState>> {
    let Some(d) = existing else { return Ok(None) };
    if d.api_url != api_url {
        return Ok(Some(d));
    }
    let Ok(list) = api::list(&ctx.client, api_url, session).await else {
        return Ok(Some(d));
    };
    let known = list["devices"]
        .as_array()
        .is_some_and(|ds| ds.iter().any(|x| x["id"] == d.device_id.as_str()));
    if known {
        return Ok(Some(d));
    }
    ctx.info(&format!(
        "  {} {} is no longer in your account; enrolling it again",
        "Device:".dimmed(),
        d.name
    ));
    state::remove()?;
    Ok(None)
}

/// Where this machine is on the tailnet, as joining found or made it.
#[derive(Debug, PartialEq)]
struct Tailnet {
    /// The gateway's address, `<tailnet IPv4>:<port>`.
    endpoint: String,
    /// The account's compute tag on Kovalent's tailnet; None on its own.
    compute_tag: Option<String>,
    /// Whether knaix put this machine on the tailnet, so `leave` takes it off.
    joined_by_us: bool,
}

/// Put this machine on the account's tailnet, unless it already is. None when
/// the owner declines to switch it from another tailnet. `ask` asks the owner
/// (`prompt`), or refuses with `refuse` when it cannot.
#[allow(clippy::too_many_arguments)]
async fn connect_tailnet(
    ctx: &KnaixContext,
    session: &str,
    api_url: &str,
    ts: &std::path::Path,
    elevate: Option<&str>,
    joined_before: bool,
    name: Option<&str>,
    port: u16,
    ask: &dyn Fn(&str, &str) -> Result<bool>,
) -> Result<Option<Tailnet>> {
    let mut status = tailnet::status(ts)?;
    let mut joined_by_us = joined_before;
    let compute_tag: Option<String> = if let Some(tag) = status.compute_tag() {
        Some(tag.to_string())
    } else {
        match api::join_key(&ctx.client, api_url, session).await? {
            api::JoinKey::OwnTailnet => {
                if !status.running() {
                    return Err(anyhow!(
                        "This account uses its own tailnet. Connect this machine to it with Tailscale, then run this again."
                    ))
                    .coded(Code::Precondition);
                }
                None
            }
            api::JoinKey::Managed { auth_key, tag } => {
                if status.running() {
                    let on = status
                        .tailnet
                        .clone()
                        .unwrap_or_else(|| "another tailnet".into());
                    let go = ask(
                        &format!(
                            "Tailscale here is connected to {on}. Switch it to Kovalent's network as a device? It disconnects from {on}."
                        ),
                        &format!("Tailscale here is connected to {on}, and joining would switch it."),
                    )?;
                    if !go {
                        // Declining is not a failure (see the exit codes in
                        // the README); only the non-interactive refusal is.
                        println!(
                            "{} Nothing changed. Use a machine that is not on another tailnet, or run this again with --yes to switch.",
                            "Info:".blue()
                        );
                        return Ok(None);
                    }
                }
                let display = name.map(str::to_string).unwrap_or_else(hostname);
                ctx.info(&format!(
                    "  {} connecting with the device tag",
                    "Tailnet:".dimmed()
                ));
                tailnet::up_with_key(elevate, ts, &auth_key, &tailscale_hostname(&display))?;
                joined_by_us = true;
                status = tailnet::status(ts)?;
                if status.compute_tag() != Some(tag.as_str()) || !status.running() {
                    return Err(anyhow!(
                        "Tailscale connected, but not with the device tag {tag}. Run `tailscale status` to check, then run this again."
                    ))
                    .coded(Code::Precondition);
                }
                Some(tag)
            }
        }
    };
    let ip = status
        .ipv4
        .ok_or_else(|| anyhow!("Tailscale has no IPv4 address on this machine yet. Wait a moment and run this again."))
        .coded(Code::Precondition)?;
    let endpoint = format!("{ip}:{port}");
    ctx.info(&format!(
        "  {} {} ({})",
        "Tailnet:".dimmed(),
        endpoint,
        compute_tag.as_deref().unwrap_or("your own tailnet")
    ));
    Ok(Some(Tailnet {
        endpoint,
        compute_tag,
        joined_by_us,
    }))
}

/// The device record for this machine: `existing` brought up to date when it
/// belongs to this control plane, else a new enrolment. Saved either way, and
/// a note that an earlier device was removed is cleared.
#[allow(clippy::too_many_arguments)]
async fn enrol(
    ctx: &KnaixContext,
    session: &str,
    api_url: &str,
    existing: Option<state::DeviceState>,
    name: Option<String>,
    max: Option<u64>,
    port: u16,
    offer: &hardware::Offer,
    variant: &llama::Variant,
    net: Tailnet,
) -> Result<state::DeviceState> {
    let device = match existing {
        Some(mut d) if d.api_url == api_url => {
            d.gateway_port = port;
            d.backend = offer.backend.as_str().into();
            d.variant = variant.id.into();
            if max.is_some() {
                d.max_memory_bytes = max;
            }
            d.compute_tag = net.compute_tag;
            d.joined_tailnet_by_knaix = net.joined_by_us;
            if let Some(n) = name {
                d.name = n;
            }
            state::save(&d)?;
            ctx.info(&format!("  {} already enrolled as {}", "Device:".dimmed(), d.name.bold()));
            d
        }
        Some(d) => {
            return Err(anyhow!(
                "This machine is enrolled with another control plane ({}). Run `knaix device leave` first.",
                d.api_url
            ))
            .coded(Code::Precondition)
        }
        None => {
            let display = name.unwrap_or_else(hostname);
            let report = api::Report {
                backend: offer.backend.as_str().into(),
                total_memory_bytes: offer.total_bytes,
                usable_memory_bytes: offer.usable_bytes,
                llama_build: Some(llama::BUILD.into()),
                rpc_endpoint: Some(net.endpoint),
            };
            let enrolled = api::enroll(&ctx.client, api_url, session, &display, &report).await?;
            let device_id = enrolled.device["id"].as_str().unwrap_or_default().to_string();
            let key = match &enrolled.ticket_key {
                Some(k) => ticket::PinnedKey::from_pem(&k.public_key_pem)
                    .ok()
                    .filter(|p| p.key_id == k.key_id)
                    .map(|_| k.clone()),
                None => None,
            };
            let Some(key) = key.filter(|_| !device_id.is_empty()) else {
                // A device without a key could never verify a ticket. Take it
                // back out rather than leave an enrolment nothing can use.
                if !device_id.is_empty() {
                    let _ = api::remove(&ctx.client, api_url, session, &device_id).await;
                }
                return Err(anyhow!(
                    "The control plane is not issuing tunnel tickets right now, so this machine was not enrolled. Try again later."
                ))
                .coded(Code::Unavailable);
            };
            let d = state::DeviceState {
                device_id,
                token: enrolled.token,
                name: display,
                api_url: api_url.to_string(),
                ticket_key_id: key.key_id,
                ticket_key_pem: key.public_key_pem,
                gateway_port: port,
                backend: offer.backend.as_str().into(),
                variant: variant.id.into(),
                max_memory_bytes: max,
                compute_tag: net.compute_tag,
                joined_tailnet_by_knaix: net.joined_by_us,
                enrolled_at: state::now_unix(),
            };
            state::save(&d)?;
            ctx.info(&format!(
                "  {} enrolled as {} (ticket key {})",
                "Device:".dimmed(),
                d.name.bold(),
                d.ticket_key_id
            ));
            for w in enrolled.warnings.iter().filter(|w| *w != "no_gateway_endpoint") {
                ctx.info(&format!("  {} {}", "Warning:".yellow(), explain_warning(w)));
            }
            d
        }
    };
    if let Some(m) = removed_marker() {
        let _ = std::fs::remove_file(m);
    }
    Ok(device)
}

/// Install the background service through `manager` and say how to manage it.
fn background(ctx: &KnaixContext, name: &str, manager: &service::Manager) -> Result<()> {
    let path = manager.install()?;
    ctx.info(&format!(
        "\n{} {} is lending itself in the background ({}).",
        "✓".green(),
        name.bold(),
        path.display()
    ));
    ctx.info(&format!(
        "  {} shows how it is doing; {} stops it and removes it from your account.",
        brand::cmd("device status"),
        brand::cmd("device leave")
    ));
    if manager.os == "linux" {
        ctx.info(&format!(
            "  To keep it running while you are logged out, run {}.",
            "loginctl enable-linger $USER".cyan()
        ));
    }
    Ok(())
}

async fn run_service() -> Result<()> {
    supported()?;
    let result = runner::run(&|line: &str| eprintln!("{line}")).await;
    if let Err(e) = &result {
        if crate::exit::code_of(e) == Code::Auth {
            // Removed from the account. Restarting would only be refused again,
            // so stop the service and leave a note for `status`.
            if let Some(m) = removed_marker() {
                let _ = state::write_private(&m, state::now_unix().to_string().as_bytes());
            }
            if let Ok(p) = state::gateway_log_path() {
                state::append_log(&p, "removed from the account; stopping the service");
            }
            let _ = service::uninstall();
        }
    }
    result
}

async fn status(ctx: &KnaixContext) -> Result<()> {
    let device = state::load()?;
    let json_out = ctx.output_format == "json";
    let Some(device) = device else {
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "enrolled": false }))?
            );
        } else {
            println!(
                "This machine is not a compute device. {} lends it to your pools.",
                brand::cmd("device join")
            );
        }
        return Ok(());
    };
    let runtime = state::load_runtime().filter(|r| runner::alive(r.pid));
    let removed = removed_marker().is_some_and(|m| m.exists());
    let install_ok = llama::VARIANTS
        .iter()
        .find(|v| v.id == device.variant)
        .map(|v| llama::verify(v).is_ok())
        .unwrap_or(false);
    let pending_key = runtime
        .as_ref()
        .and_then(|r| r.server_ticket_key_id.clone())
        .filter(|id| id != &device.ticket_key_id);

    // The account's view, when the session allows it.
    let account: Option<Value> = match ctx.get_token() {
        Ok(session) => api::list(&ctx.client, &device.api_url, session)
            .await
            .ok()
            .and_then(|v| {
                v["devices"]
                    .as_array()?
                    .iter()
                    .find(|d| d["id"] == device.device_id.as_str())
                    .cloned()
            }),
        Err(_) => None,
    };
    let now = state::now_unix();

    if json_out {
        let out = json!({
            "enrolled": true,
            "deviceId": device.device_id,
            "name": device.name,
            "apiUrl": device.api_url,
            "backend": device.backend,
            "llamaBuild": llama::BUILD,
            "llamaInstallVerified": install_ok,
            "gateway": runtime.as_ref().and_then(|r| r.gateway.clone()),
            "running": runtime.is_some(),
            "service": service::installed(),
            "rpcServerRunning": runtime.as_ref().map(|r| r.rpc_server_running).unwrap_or(false),
            "sandbox": runtime.as_ref().and_then(|r| r.sandbox.clone()),
            "lastHeartbeatAt": runtime.as_ref().and_then(|r| r.last_heartbeat_at),
            "lastHeartbeatOk": runtime.as_ref().map(|r| r.last_heartbeat_ok),
            "lastError": runtime.as_ref().and_then(|r| r.last_error.clone()),
            "usableMemoryBytes": runtime.as_ref().and_then(|r| r.usable_memory_bytes),
            "ticketKeyId": device.ticket_key_id,
            "pendingTicketKeyId": pending_key,
            "computeTag": device.compute_tag,
            "removedFromAccount": removed,
            "online": account.as_ref().and_then(|a| a["online"].as_bool()),
            "poolNodeIds": account.as_ref().map(|a| a["poolNodeIds"].clone()),
            "warnings": account.as_ref().map(|a| a["warnings"].clone())
                .or_else(|| runtime.as_ref().map(|r| json!(r.warnings))),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "{} {}",
        device.name.bold(),
        format!("({})", device.device_id).dimmed()
    );
    let line = |k: &str, v: String| println!("  {:<14} {}", k.dimmed(), v);
    if removed {
        line("Account", "removed from your account; run `knaix device leave`, then `knaix device join` to lend it again".red().to_string());
    }
    let state_word = match (&runtime, service::installed()) {
        (Some(_), true) => "running (background service)".green().to_string(),
        (Some(_), false) => "running".green().to_string(),
        (None, true) => "service installed, not running".yellow().to_string(),
        (None, false) => "stopped".yellow().to_string(),
    };
    line("State", state_word);
    if let Some(a) = &account {
        let online = a["online"].as_bool().unwrap_or(false);
        line(
            "Account",
            if online {
                "online".green().to_string()
            } else {
                "offline".yellow().to_string()
            },
        );
        let pools = a["poolNodeIds"].as_array().map(|p| p.len()).unwrap_or(0);
        line(
            "Pools",
            if pools == 0 {
                "not in a pool yet".to_string()
            } else {
                format!("in {pools} pool{}", if pools == 1 { "" } else { "s" })
            },
        );
    }
    let memory = runtime
        .as_ref()
        .and_then(|r| r.usable_memory_bytes)
        .map(crate::hardware::human_size)
        .unwrap_or_else(|| "measured when running".into());
    line("Offers", format!("{memory} ({})", device.backend));
    line(
        "llama.cpp",
        if install_ok {
            format!("{} {}", llama::BUILD, "verified".green())
        } else {
            format!(
                "{} {}",
                llama::BUILD,
                "not installed or changed; run `knaix device join`".red()
            )
        },
    );
    if let Some(rt) = &runtime {
        line(
            "Gateway",
            rt.gateway.clone().unwrap_or_else(|| "not listening".into()),
        );
        line(
            "rpc-server",
            if rt.rpc_server_running {
                format!(
                    "running on loopback, sandboxed ({})",
                    rt.sandbox.clone().unwrap_or_default()
                )
            } else {
                "not running".yellow().to_string()
            },
        );
        line(
            "Heartbeat",
            match (rt.last_heartbeat_at, rt.last_heartbeat_ok) {
                (Some(t), true) => format!("{} {}", "ok".green(), state::ago(t, now)),
                (Some(t), false) => {
                    format!("{} (last success {})", "failing".red(), state::ago(t, now))
                }
                (None, _) => "none yet".into(),
            },
        );
        if let Some(e) = &rt.last_error {
            line("Last error", e.clone());
        }
    }
    line(
        "Tailnet",
        device
            .compute_tag
            .clone()
            .unwrap_or_else(|| "your own tailnet".into()),
    );
    line("Ticket key", device.ticket_key_id.clone());
    if let Some(new) = pending_key {
        println!(
            "\n  {} The control plane now signs with key {new}. Pools cannot use this device until you confirm it: {}",
            "Action:".yellow(),
            brand::cmd("device rekey")
        );
    }
    let warnings: Vec<String> = account
        .as_ref()
        .and_then(|a| a["warnings"].as_array().cloned())
        .map(|w| {
            w.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .or_else(|| runtime.as_ref().map(|r| r.warnings.clone()))
        .unwrap_or_default();
    for w in warnings {
        println!("  {} {}", "Warning:".yellow(), explain_warning(&w));
    }
    if runtime.is_none() && !removed {
        println!(
            "\n  {} starts it again ({} keeps it running in the background).",
            brand::cmd("device join"),
            "--daemon".cyan()
        );
    }
    Ok(())
}

async fn ls(ctx: &KnaixContext) -> Result<()> {
    let session = ctx.get_token()?;
    let api_url = ctx.config.api_url.trim_end_matches('/');
    let body = api::list(&ctx.client, api_url, session).await?;
    let devices = body["devices"].as_array().cloned().unwrap_or_default();
    if ctx.output_format == "json" {
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(());
    }
    if devices.is_empty() {
        println!(
            "No compute devices yet. {} on a machine lends it to your pools.",
            brand::cmd("device join")
        );
        return Ok(());
    }
    let mine = state::load().ok().flatten().map(|d| d.device_id);
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec![
        "Name",
        "Backend",
        "Offers",
        "llama.cpp",
        "State",
        "Pools",
        "Warnings",
    ]);
    for d in &devices {
        let id = d["id"].as_str().unwrap_or_default();
        let mut name = d["name"].as_str().unwrap_or_default().to_string();
        if mine.as_deref() == Some(id) {
            name.push_str(" (this machine)");
        }
        let online = d["online"].as_bool().unwrap_or(false);
        let warnings = d["warnings"].as_array().map(|w| w.len()).unwrap_or(0);
        table.add_row(vec![
            name,
            d["backend"].as_str().unwrap_or_default().to_string(),
            d["usableMemoryBytes"]
                .as_u64()
                .map(crate::hardware::human_size)
                .unwrap_or_default(),
            d["llamaBuild"].as_str().unwrap_or("unknown").to_string(),
            if online {
                "online".green().to_string()
            } else {
                "offline".yellow().to_string()
            },
            d["poolNodeIds"]
                .as_array()
                .map(|p| p.len())
                .unwrap_or(0)
                .to_string(),
            if warnings == 0 {
                String::new()
            } else {
                warnings.to_string().yellow().to_string()
            },
        ]);
    }
    println!("{table}");
    Ok(())
}

async fn leave(ctx: &KnaixContext, yes: bool, purge: bool) -> Result<()> {
    let Some(device) = state::load()? else {
        if purge {
            purge_files();
            println!(
                "{} Removed the llama.cpp build, cache and logs.",
                "✓".green()
            );
        } else {
            println!("This machine is not a compute device.");
        }
        return Ok(());
    };
    if !confirm(
        &format!(
            "Stop lending {} and remove it from your account?",
            device.name
        ),
        yes,
        "Leaving stops the device and removes it from your account.",
    )? {
        println!("{} Nothing changed.", "Info:".blue());
        return Ok(());
    }

    // Stop first, so nothing heartbeats the device back while it is removed.
    let had_service = service::uninstall()?;
    let stopped = runner::stop_running();
    sandbox::stop_unit(&sandbox::unit_name());

    let removed_remotely = match ctx.get_token() {
        Ok(session) => api::remove(&ctx.client, &device.api_url, session, &device.device_id).await,
        Err(e) => Err(e),
    };
    state::remove()?;
    state::remove_runtime();
    if let Some(m) = removed_marker() {
        let _ = std::fs::remove_file(m);
    }
    if purge {
        purge_files();
    }

    if had_service || stopped {
        println!(
            "{} Stopped {} and forgot its device token and ticket key.",
            "✓".green(),
            device.name.bold()
        );
    } else {
        println!(
            "{} {} was not running. Forgot its device token and ticket key.",
            "✓".green(),
            device.name.bold()
        );
    }
    match removed_remotely {
        Ok(()) => println!("  Removed from your account."),
        Err(e) => println!(
            "  {} It is still listed in your account ({e}). Remove it from the dashboard's Devices page, or log in and run {}.",
            "Note:".yellow(),
            brand::cmd("device ls")
        ),
    }
    if device.joined_tailnet_by_knaix {
        println!(
            "  Tailscale is still connected to Kovalent's network with the device tag, where no rule lets anything reach it. Run {} to disconnect it.",
            "tailscale logout".cyan()
        );
    }
    Ok(())
}

fn purge_files() {
    let _ = std::fs::remove_dir_all(llama::install_root());
    let _ = std::fs::remove_dir_all(state::knaix_dir().join("device"));
}

async fn rekey(ctx: &KnaixContext, yes: bool) -> Result<()> {
    let mut device = state::load()?
        .ok_or_else(|| anyhow!("This machine is not a compute device."))
        .coded(Code::Precondition)?;
    let served = api::ticket_key(&ctx.client, &device.api_url).await?;
    if served.key_id == device.ticket_key_id {
        println!(
            "{} The pinned key ({}) is the one the control plane signs with. Nothing to do.",
            "✓".green(),
            device.ticket_key_id
        );
        return Ok(());
    }
    let key = ticket::PinnedKey::from_pem(&served.public_key_pem)
        .context("The control plane served a ticket key this device cannot use")?;
    if key.key_id != served.key_id {
        return Err(anyhow!(
            "The served key's id does not match the key itself ({} vs {}). Not pinning it.",
            served.key_id,
            key.key_id
        ))
        .coded(Code::Denied);
    }
    println!("  Pinned key:  {}", device.ticket_key_id);
    println!("  Served key:  {}", served.key_id.bold());
    println!(
        "  Check the served key id against the one your administrator announced before confirming."
    );
    if !confirm(
        "Pin the served key?",
        yes,
        "Rekeying changes which tickets this device accepts.",
    )? {
        println!("{} Nothing changed.", "Info:".blue());
        return Ok(());
    }
    device.ticket_key_id = served.key_id;
    device.ticket_key_pem = served.public_key_pem;
    state::save(&device)?;
    println!("{} Pinned {}.", "✓".green(), device.ticket_key_id);
    if service::installed() {
        service::restart();
        println!("  Restarted the background service with the new key.");
    } else if runner::stop_running() {
        println!(
            "  Stopped the running device so it does not keep the old key. Start it again with {}.",
            brand::cmd("device join")
        );
    }
    Ok(())
}

fn tail(path: &std::path::Path, lines: usize) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = raw.lines().collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// Gateway log lines start with a Unix time; show it as an age.
fn humanize(line: &str, now: u64) -> String {
    match line.split_once(' ') {
        Some((t, rest)) => match t.parse::<u64>() {
            Ok(ts) => format!("{:>12}  {rest}", state::ago(ts, now)),
            Err(_) => line.to_string(),
        },
        None => line.to_string(),
    }
}

fn logs(lines: usize) -> Result<()> {
    let now = state::now_unix();
    for (title, path) in [
        ("Gateway", state::gateway_log_path()?),
        ("rpc-server", state::rpc_log_path()?),
        ("Service", state::service_log_path()?),
    ] {
        let shown = tail(&path, lines);
        if shown.is_empty() && title == "Service" {
            continue;
        }
        println!("{} {}", title.bold(), path.display().to_string().dimmed());
        if shown.is_empty() {
            println!("  (empty)");
        }
        for l in shown {
            println!("  {}", humanize(&l, now));
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_become_tailscale_names() {
        assert_eq!(
            tailscale_hostname("Maya's MacBook Pro"),
            "maya-s-macbook-pro"
        );
        assert_eq!(tailscale_hostname("--studio--"), "studio");
        assert_eq!(tailscale_hostname("!!!"), "device");
        assert_eq!(tailscale_hostname(&"a".repeat(80)).len(), 63);
    }

    #[test]
    fn warnings_read_as_sentences() {
        assert!(explain_warning("llama_build_below_floor:8585").contains("8585"));
        assert!(explain_warning("no_gateway_endpoint").contains("gateway"));
        assert!(
            explain_warning("llama_build_blocked").starts_with("This llama.cpp build is blocked")
        );
        assert_eq!(explain_warning("something_new"), "something_new");
    }

    #[test]
    fn log_times_show_as_ages() {
        assert_eq!(
            humanize("100 refused 1.2.3.4: expired", 160),
            format!("{:>12}  refused 1.2.3.4: expired", "1 min ago")
        );
        assert_eq!(humanize("plain line", 0), "plain line");
    }

    /// `join`'s parts, against a fake control plane and a stand-in Tailscale.
    #[cfg(unix)]
    mod joining {
        use super::*;
        use crate::config::{Config, ScratchHome};
        use crate::exit::code_of;
        use crate::testing::{args_of, fake_api, fake_tool, Call};
        use std::cell::RefCell;

        /// The ticket key in the RPC tunnel test vectors.
        const KEY_ID: &str = "c1d40ab1863aca6e";
        const KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAt6E/WRc+E5MpT+ohkHoqZeZG0Y3Hv1hE7bAlp5ykVFY=\n-----END PUBLIC KEY-----\n";
        const DEVICES: &str = "GET /api/compute/devices";
        const JOIN_KEY: &str = "POST /api/compute/join-key";
        const ENROLL: &str = "POST /api/compute/devices";
        const GIB: u64 = 1 << 30;

        fn ctx(api_url: &str) -> KnaixContext {
            KnaixContext {
                config: Config {
                    token: Some("sess".into()),
                    api_url: api_url.into(),
                    ..Config::default()
                },
                client: reqwest::Client::new(),
                output_format: "text".into(),
                quiet: true,
            }
        }

        fn device(api_url: &str) -> state::DeviceState {
            state::DeviceState {
                device_id: "dev-1".into(),
                token: "kvd_secret".into(),
                name: "studio".into(),
                api_url: api_url.into(),
                ticket_key_id: KEY_ID.into(),
                ticket_key_pem: KEY_PEM.into(),
                gateway_port: 47100,
                backend: "metal".into(),
                variant: "macos-arm64".into(),
                max_memory_bytes: Some(8 * GIB),
                compute_tag: Some("tag:c-acme".into()),
                joined_tailnet_by_knaix: false,
                enrolled_at: 1,
            }
        }

        fn routes(calls: &[Call]) -> Vec<&str> {
            calls.iter().map(|c| c.route.as_str()).collect()
        }

        /// `record` as `unless_removed` leaves it, asking the account at `api`.
        async fn checked(
            api: &str,
            record: Option<state::DeviceState>,
        ) -> Option<state::DeviceState> {
            unless_removed(&ctx(api), api, "sess", record)
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn a_device_the_account_still_lists_is_kept_and_one_it_does_not_is_forgotten() {
            let _home = ScratchHome::new();
            let listed =
                fake_api(vec![(DEVICES, 200, json!({"devices": [{"id": "dev-1"}]}))]).await;
            let unlisted =
                fake_api(vec![(DEVICES, 200, json!({"devices": [{"id": "other"}]}))]).await;
            let failing = fake_api(vec![(DEVICES, 500, json!({}))]).await;

            assert_eq!(checked(&listed.url, None).await, None);
            assert!(listed.calls().is_empty(), "nothing to ask about");
            assert_eq!(
                checked(&listed.url, Some(device(&listed.url))).await,
                Some(device(&listed.url))
            );
            assert_eq!(listed.calls()[0].auth.as_deref(), Some("Bearer sess"));
            let unknown = checked(&failing.url, Some(device(&failing.url))).await;
            assert_eq!(
                unknown,
                Some(device(&failing.url)),
                "the heartbeat will tell"
            );
            // Enrolled with another control plane: not this account's to ask.
            let elsewhere = checked(&unlisted.url, Some(device("http://elsewhere"))).await;
            assert_eq!(elsewhere, Some(device("http://elsewhere")));
            assert!(unlisted.calls().is_empty());

            state::save(&device(&unlisted.url)).unwrap();
            assert_eq!(
                checked(&unlisted.url, Some(device(&unlisted.url))).await,
                None
            );
            assert_eq!(state::load().unwrap(), None, "and its record is gone");
        }

        fn status(backend_state: &str, ips: &[&str], tags: &[&str]) -> Value {
            json!({
                "BackendState": backend_state,
                "Self": {"TailscaleIPs": ips, "Tags": tags},
                "CurrentTailnet": {"Name": "home.example"},
            })
        }

        /// A Tailscale reporting `before`, and `after` once `up` has run.
        fn tailscale(dir: &std::path::Path, before: Value, after: Value) -> String {
            fake_tool(
                dir,
                "tailscale",
                &format!(
                    "case \"$1\" in\n\
                     status) if [ -e \"$0.up\" ]; then echo '{after}'; else echo '{before}'; fi ;;\n\
                     up) touch \"$0.up\" ;;\n\
                     esac"
                ),
            )
        }

        fn managed() -> (&'static str, u16, Value) {
            (
                JOIN_KEY,
                200,
                json!({"authKey": "tskey-secret", "tag": "tag:c-acme"}),
            )
        }

        fn tagged() -> Value {
            status("Running", &["fd7a::1", "100.64.0.1"], &["tag:c-acme"])
        }

        fn joined(compute_tag: Option<&str>, joined_by_us: bool) -> Option<Tailnet> {
            Some(Tailnet {
                endpoint: "100.64.0.1:47100".into(),
                compute_tag: compute_tag.map(str::to_string),
                joined_by_us,
            })
        }

        /// Never asked: the case at hand has nothing to confirm.
        fn unasked(_: &str, _: &str) -> Result<bool> {
            panic!("asked to switch tailnets");
        }

        async fn connect(
            api: &str,
            ts: &str,
            elevate: Option<&str>,
            joined_before: bool,
            name: Option<&str>,
            ask: &dyn Fn(&str, &str) -> Result<bool>,
        ) -> Result<Option<Tailnet>> {
            let at = std::path::Path::new(ts);
            connect_tailnet(
                &ctx(api),
                "sess",
                api,
                at,
                elevate,
                joined_before,
                name,
                47100,
                ask,
            )
            .await
        }

        #[tokio::test]
        async fn a_machine_already_tagged_or_on_the_accounts_own_tailnet_joins_as_it_is() {
            let home = ScratchHome::new();
            let own = json!({"error": "This account uses its own tailnet"});
            let api = fake_api(vec![(JOIN_KEY, 409, own)]).await;

            let ts = tailscale(&home.0, tagged(), json!({}));
            let net = connect(&api.url, &ts, None, true, None, &unasked).await;
            assert_eq!(net.unwrap(), joined(Some("tag:c-acme"), true));
            assert_eq!(args_of(&ts), ["status --json"]);
            assert!(api.calls().is_empty(), "a tagged machine needs no key");

            let ts = tailscale(&home.0, status("Running", &["100.64.0.1"], &[]), json!({}));
            let net = connect(&api.url, &ts, None, false, None, &unasked).await;
            assert_eq!(net.unwrap(), joined(None, false));
            assert_eq!(routes(&api.calls()), [JOIN_KEY]);
            assert_eq!(api.calls()[0].auth.as_deref(), Some("Bearer sess"));

            let ts = tailscale(&home.0, status("NeedsLogin", &[], &[]), json!({}));
            let err = connect(&api.url, &ts, None, false, None, &unasked)
                .await
                .unwrap_err();
            assert_eq!(code_of(&err), Code::Precondition);
            let msg = err.to_string();
            assert!(
                msg.starts_with("This account uses its own tailnet. Connect"),
                "{msg}"
            );

            let v6_only = status("Running", &["fd7a::1"], &["tag:c-acme"]);
            let ts = tailscale(&home.0, v6_only, json!({}));
            let err = connect(&api.url, &ts, None, false, None, &unasked)
                .await
                .unwrap_err();
            assert_eq!(code_of(&err), Code::Precondition);
            assert!(
                err.to_string().starts_with("Tailscale has no IPv4 address"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn a_machine_off_any_tailnet_joins_kovalents_with_the_key() {
            let home = ScratchHome::new();
            let api = fake_api(vec![managed()]).await;
            let ts = tailscale(&home.0, status("NeedsLogin", &[], &[]), tagged());
            let sudo = fake_tool(&home.0, "sudo", "exec \"$@\"");

            let net = connect(
                &api.url,
                &ts,
                Some(&sudo),
                false,
                Some("Maya's Studio"),
                &unasked,
            )
            .await;

            assert_eq!(net.unwrap(), joined(Some("tag:c-acme"), true));
            let up = &args_of(&sudo)[0];
            assert!(up.starts_with(&format!("{ts} up --auth-key=file:")), "{up}");
            assert!(
                up.ends_with(" --hostname=maya-s-studio --reset --timeout=60s"),
                "{up}"
            );
            assert_eq!(
                args_of(&ts),
                ["status --json", &up[ts.len() + 1..], "status --json"]
            );
        }

        #[tokio::test]
        async fn a_join_without_a_name_uses_the_hostname_and_one_without_the_tag_is_refused() {
            let home = ScratchHome::new();
            let api = fake_api(vec![managed()]).await;
            let untagged = status("Running", &["100.64.0.1"], &["tag:other"]);
            let ts = tailscale(&home.0, status("Stopped", &[], &[]), untagged);

            let err = connect(&api.url, &ts, None, false, None, &unasked)
                .await
                .unwrap_err();

            assert_eq!(code_of(&err), Code::Precondition);
            let msg = err.to_string();
            assert!(
                msg.starts_with("Tailscale connected, but not with the device tag tag:c-acme."),
                "{msg}"
            );
            let up = &args_of(&ts)[1];
            let name = tailscale_hostname(&hostname());
            assert!(up.contains(&format!(" --hostname={name} ")), "{up}");
        }

        #[tokio::test]
        async fn a_machine_on_another_tailnet_switches_only_when_the_owner_agrees() {
            let home = ScratchHome::new();
            let api = fake_api(vec![managed()]).await;
            let ts = tailscale(&home.0, status("Running", &["100.101.0.7"], &[]), tagged());
            // Answered in turn: no, cannot ask, yes.
            let answers = RefCell::new(vec![
                Ok(true),
                Err(anyhow!("no terminal")).coded(Code::Denied),
                Ok(false),
            ]);
            let asked = RefCell::new(Vec::new());
            let ask = |prompt: &str, refuse: &str| {
                asked
                    .borrow_mut()
                    .push((prompt.to_string(), refuse.to_string()));
                answers.borrow_mut().pop().unwrap()
            };

            let net = connect(&api.url, &ts, None, false, Some("studio"), &ask).await;
            assert_eq!(net.unwrap(), None);
            assert_eq!(args_of(&ts), ["status --json"], "declining changes nothing");
            let (prompt, refuse) = asked.borrow()[0].clone();
            assert!(
                prompt.starts_with("Tailscale here is connected to home.example. Switch it"),
                "{prompt}"
            );
            assert_eq!(
                refuse,
                "Tailscale here is connected to home.example, and joining would switch it."
            );

            let err = connect(&api.url, &ts, None, false, Some("studio"), &ask)
                .await
                .unwrap_err();
            assert_eq!(code_of(&err), Code::Denied);
            assert_eq!(args_of(&ts).len(), 2, "nor does being unable to ask");

            let net = connect(&api.url, &ts, None, false, Some("studio"), &ask).await;
            assert_eq!(net.unwrap(), joined(Some("tag:c-acme"), true));
            assert!(args_of(&ts)[3].starts_with("up --auth-key=file:"));
            assert_eq!(asked.borrow().len(), 3);
        }

        fn offer() -> hardware::Offer {
            hardware::Offer {
                backend: hardware::Backend::Cpu,
                total_bytes: 32 * GIB,
                usable_bytes: 24 * GIB,
                detail: "CPU".into(),
            }
        }

        fn variant() -> &'static llama::Variant {
            llama::variant_for("linux", "x86_64", hardware::Backend::Cpu).unwrap()
        }

        /// `enrol` for the machine at 100.64.0.1, on port 47200.
        async fn enrolled(
            api: &str,
            existing: Option<state::DeviceState>,
            name: Option<&str>,
            max: Option<u64>,
        ) -> Result<state::DeviceState> {
            let net = joined(Some("tag:c-acme"), true).unwrap();
            let name = name.map(str::to_string);
            enrol(
                &ctx(api),
                "sess",
                api,
                existing,
                name,
                max,
                47200,
                &offer(),
                variant(),
                net,
            )
            .await
        }

        fn removed_note() -> std::path::PathBuf {
            let note = removed_marker().unwrap();
            std::fs::create_dir_all(note.parent().unwrap()).unwrap();
            std::fs::write(&note, "1").unwrap();
            note
        }

        #[tokio::test]
        async fn an_existing_device_of_this_control_plane_is_brought_up_to_date() {
            let _home = ScratchHome::new();
            let api = fake_api(vec![]).await;
            let note = removed_note();

            let d = enrolled(&api.url, Some(device(&api.url)), None, None)
                .await
                .unwrap();

            let mut want = device(&api.url);
            want.gateway_port = 47200;
            want.backend = "cpu".into();
            want.variant = "ubuntu-x64".into();
            want.joined_tailnet_by_knaix = true;
            assert_eq!(d, want, "the cap and name are kept unless given");
            assert_eq!(state::load().unwrap(), Some(want));
            assert!(!note.exists(), "a device again, so no longer removed");

            let d = enrolled(&api.url, Some(d), Some("garage"), Some(4 * GIB))
                .await
                .unwrap();
            assert_eq!(
                (d.name.as_str(), d.max_memory_bytes),
                ("garage", Some(4 * GIB))
            );
            assert_eq!(state::load().unwrap(), Some(d));
            assert!(api.calls().is_empty(), "nothing to enrol");
        }

        #[tokio::test]
        async fn a_device_of_another_control_plane_must_leave_it_first() {
            let _home = ScratchHome::new();
            let api = fake_api(vec![]).await;

            let err = enrolled(&api.url, Some(device("http://elsewhere")), None, None)
                .await
                .unwrap_err();

            assert_eq!(code_of(&err), Code::Precondition);
            assert_eq!(
                err.to_string(),
                "This machine is enrolled with another control plane (http://elsewhere). Run `knaix device leave` first."
            );
            assert_eq!(state::load().unwrap(), None);
            assert!(api.calls().is_empty());
        }

        fn key(key_id: &str, pem: &str) -> Value {
            json!({"keyId": key_id, "publicKeyPem": pem})
        }

        #[tokio::test]
        async fn a_new_device_is_enrolled_with_what_it_offers_and_saved() {
            let _home = ScratchHome::new();
            let reply = json!({
                "device": {"id": "dev-new"},
                "token": "kvd_new",
                "ticketKey": key(KEY_ID, KEY_PEM),
                "warnings": ["llama_build_unknown", "no_gateway_endpoint"],
            });
            let api = fake_api(vec![(ENROLL, 201, reply)]).await;
            let note = removed_note();
            let before = state::now_unix();

            let d = enrolled(&api.url, None, Some("garage"), Some(4 * GIB))
                .await
                .unwrap();

            let calls = api.calls();
            assert_eq!(routes(&calls), [ENROLL]);
            assert_eq!(calls[0].auth.as_deref(), Some("Bearer sess"));
            assert_eq!(
                calls[0].body,
                json!({
                    "name": "garage",
                    "platform": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "backend": "cpu",
                    "totalMemoryBytes": 32 * GIB,
                    "usableMemoryBytes": 24 * GIB,
                    "llamaBuild": llama::BUILD,
                    "rpcEndpoint": "100.64.0.1:47100",
                })
            );
            let want = state::DeviceState {
                device_id: "dev-new".into(),
                token: "kvd_new".into(),
                name: "garage".into(),
                api_url: api.url.clone(),
                ticket_key_id: KEY_ID.into(),
                ticket_key_pem: KEY_PEM.into(),
                gateway_port: 47200,
                backend: "cpu".into(),
                variant: "ubuntu-x64".into(),
                max_memory_bytes: Some(4 * GIB),
                compute_tag: Some("tag:c-acme".into()),
                joined_tailnet_by_knaix: true,
                enrolled_at: d.enrolled_at,
            };
            assert_eq!(d, want);
            assert!(d.enrolled_at >= before);
            assert_eq!(state::load().unwrap(), Some(want));
            assert!(!note.exists());
        }

        #[tokio::test]
        async fn a_new_device_without_a_usable_ticket_key_is_taken_back_out() {
            let _home = ScratchHome::new();
            let unusable = [
                Value::Null,
                key("0ld0ld0ld0ld0ld0", KEY_PEM),
                key(KEY_ID, "not a key"),
            ];
            for ticket_key in unusable {
                let reply = json!({"device": {"id": "dev-new"}, "token": "kvd_new", "ticketKey": ticket_key});
                let api = fake_api(vec![
                    (ENROLL, 201, reply),
                    ("DELETE /api/compute/devices/dev-new", 204, Value::Null),
                ])
                .await;

                let err = enrolled(&api.url, None, Some("garage"), None)
                    .await
                    .unwrap_err();

                assert_eq!(code_of(&err), Code::Unavailable, "{ticket_key}");
                assert!(
                    err.to_string()
                        .starts_with("The control plane is not issuing tunnel tickets"),
                    "{err}"
                );
                assert_eq!(
                    routes(&api.calls()),
                    [ENROLL, "DELETE /api/compute/devices/dev-new"],
                    "{ticket_key}"
                );
                assert_eq!(api.calls()[1].auth.as_deref(), Some("Bearer sess"));
                assert_eq!(state::load().unwrap(), None);
            }

            // Nothing to take back out when the control plane named no device.
            let reply =
                json!({"device": {}, "token": "kvd_new", "ticketKey": key(KEY_ID, KEY_PEM)});
            let api = fake_api(vec![(ENROLL, 201, reply)]).await;
            let err = enrolled(&api.url, None, Some("garage"), None)
                .await
                .unwrap_err();
            assert_eq!(code_of(&err), Code::Unavailable);
            assert_eq!(routes(&api.calls()), [ENROLL]);
            assert_eq!(state::load().unwrap(), None);
        }

        #[test]
        fn the_background_service_is_installed_through_the_systems_manager() {
            let home = ScratchHome::new();
            let ctx = ctx("http://127.0.0.1:9");
            for (os, program, file) in [
                (
                    "macos",
                    "launchctl",
                    "Library/LaunchAgents/com.kovalent.knaix-device.plist",
                ),
                (
                    "linux",
                    "systemctl",
                    ".config/systemd/user/knaix-device.service",
                ),
            ] {
                let manager = fake_tool(&home.0, program, "exit 0");
                background(
                    &ctx,
                    "studio",
                    &service::Manager {
                        os,
                        program: &manager,
                    },
                )
                .unwrap();
                assert!(home.0.join(file).exists(), "{os}");
                assert!(!args_of(&manager).is_empty(), "{os}");
            }

            let refusing = fake_tool(&home.0, "launchctl", "exit 1");
            let err = background(
                &ctx,
                "studio",
                &service::Manager {
                    os: "macos",
                    program: &refusing,
                },
            )
            .unwrap_err();
            assert_eq!(code_of(&err), Code::Precondition);
        }
    }
}
