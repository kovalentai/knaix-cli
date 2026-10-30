//! `knaix device run`: the long-running half of a device.
//!
//! Starts rpc-server inside its sandbox on a random loopback port, the gateway
//! on the tailnet address, and the heartbeat. Restarts rpc-server when it
//! exits, re-checking the binary every time. Exits, for the service manager to
//! restart, when the tailnet address changes, and for good when the control
//! plane says the device was removed.

use crate::device::api::{self, Report};
use crate::device::gateway::{self, Gateway, SharedUpstream, Upstream};
use crate::device::hardware::{self, Backend};
use crate::device::state::{self, DeviceState, Runtime};
use crate::device::ticket::PinnedKey;
use crate::device::{llama, sandbox, tailnet};
use crate::exit::{Code, WithCode};
use anyhow::{anyhow, Context, Result};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;

/// Shared between the tasks; saved to `runtime.json` on every change.
struct Shared {
    runtime: Mutex<Runtime>,
    upstream: SharedUpstream,
    stopping: AtomicBool,
    /// Wakes the heartbeat early when rpc-server starts or stops, so the
    /// planner hears about the endpoint now rather than at the next beat.
    changed: tokio::sync::Notify,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Runtime)) {
        let mut rt = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut rt);
        let _ = state::save_runtime(&rt);
    }
}

/// Whether another device process is already running on this machine.
pub fn running_pid() -> Option<u32> {
    let rt = state::load_runtime()?;
    alive(rt.pid).then_some(rt.pid)
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    pid != 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    false
}

/// Ask a running device process to stop, and wait for it.
#[cfg(unix)]
pub fn stop_running() -> bool {
    let Some(pid) = running_pid() else {
        return false;
    };
    if pid == std::process::id() {
        return false;
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    for _ in 0..50 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[cfg(not(unix))]
pub fn stop_running() -> bool {
    false
}

/// Wait for the tailnet address, for up to `wait`.
async fn tailnet_address(bin: &Path, wait: Duration) -> Result<tailnet::Status> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Ok(s) = tailnet::status(bin) {
            if s.running() {
                return Ok(s);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "Tailscale is not connected on this machine. Start it, then run this again."
            ))
            .coded(Code::Precondition);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// The knaix binary to run as the bridge inside the Linux sandbox, by its real
/// path: the sandbox binds that file read-only, and a symlink would bind the
/// link rather than the binary.
fn bridge_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Could not find the knaix binary")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn free_loopback_port() -> Result<u16> {
    let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(l.local_addr()?.port())
}

async fn answers(upstream: &Upstream) -> bool {
    match upstream {
        Upstream::Tcp(port) => tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, *port))
            .await
            .is_ok(),
        #[cfg(unix)]
        Upstream::Unix(path) => tokio::net::UnixStream::connect(path).await.is_ok(),
        #[cfg(not(unix))]
        Upstream::Unix(_) => false,
    }
}

/// Wait for rpc-server (or, on Linux, the bridge in front of it) to answer.
/// Metal and CUDA initialisation can take several seconds.
async fn wait_ready(upstream: &Upstream, child: &mut tokio::process::Child) -> bool {
    for _ in 0..240 {
        if let Ok(Some(_)) = child.try_wait() {
            return false;
        }
        if answers(upstream).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

fn set_upstream(shared: &Shared, value: Option<Upstream>) {
    *shared.upstream.write().unwrap_or_else(|e| e.into_inner()) = value;
    shared.changed.notify_one();
}

/// Keep rpc-server running, sandboxed, until the device stops.
async fn supervise_rpc(shared: Arc<Shared>, variant: &'static llama::Variant) {
    let mut backoff = Duration::from_secs(5);
    let rpc_log = state::rpc_log_path().ok();
    loop {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let attempt = start_rpc(&shared, variant, rpc_log.as_deref()).await;
        set_upstream(&shared, None);
        shared.update(|rt| {
            rt.rpc_server_running = false;
            rt.rpc_port = None;
        });
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        match attempt {
            Ok(ran_for) => {
                if let Some(log) = &rpc_log {
                    state::append_log(log, "rpc-server exited; restarting");
                }
                if ran_for > Duration::from_secs(120) {
                    backoff = Duration::from_secs(5);
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if let Some(log) = &rpc_log {
                    state::append_log(log, &format!("could not start rpc-server: {msg}"));
                }
                shared.update(|rt| rt.last_error = Some(msg));
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One run of rpc-server, start to exit. Returns how long it ran.
async fn start_rpc(
    shared: &Arc<Shared>,
    variant: &'static llama::Variant,
    rpc_log: Option<&Path>,
) -> Result<Duration> {
    // The binary is checked against its install hashes before every start.
    let binary = llama::verify(variant)?;
    let install = llama::install_dir(variant);
    let cache = state::rpc_cache_dir()?;
    let port = free_loopback_port()?;
    let env = llama::runtime_env(&install, &cache);
    let exe = bridge_exe()?;
    let socket_dir = state::rpc_socket_dir()?;
    let plan = sandbox::choose(
        &sandbox::Paths {
            binary: &binary,
            install: &install,
            cache: &cache,
            exe: &exe,
            socket_dir: &socket_dir,
            port,
        },
        &env,
    )?;
    if let Some(unit) = &plan.unit {
        sandbox::stop_unit(unit);
    }
    if let Upstream::Unix(path) = &plan.upstream {
        let _ = std::fs::remove_file(path);
    }

    let mut cmd = tokio::process::Command::new(&plan.program);
    cmd.args(&plan.args)
        .env_clear()
        // The wrappers find their own helpers on PATH; nothing else of the
        // device's environment is passed on.
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .envs(plan.env.iter().cloned())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(log) = rpc_log {
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let err = out.try_clone()?;
        cmd.stdout(out).stderr(err);
    } else {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    let started = std::time::Instant::now();
    let mut child = cmd.spawn().context("Could not start rpc-server")?;
    let kind = plan.kind.as_str().to_string();
    if !wait_ready(&plan.upstream, &mut child).await {
        let _ = child.kill().await;
        if let Some(unit) = &plan.unit {
            sandbox::stop_unit(unit);
        }
        return Err(anyhow!(
            "rpc-server did not start listening; see `knaix device logs`"
        ));
    }
    set_upstream(shared, Some(plan.upstream.clone()));
    shared.update(|rt| {
        rt.rpc_server_running = true;
        rt.rpc_port = Some(port);
        rt.sandbox = Some(kind);
        rt.last_error = None;
    });

    // Wait for it to exit, or for the device to stop.
    loop {
        tokio::select! {
            _ = child.wait() => break,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if shared.stopping.load(Ordering::SeqCst) {
                    let _ = child.kill().await;
                    break;
                }
            }
        }
    }
    if let Some(unit) = &plan.unit {
        sandbox::stop_unit(unit);
    }
    Ok(started.elapsed())
}

fn report(
    device: &DeviceState,
    backend: Backend,
    endpoint: Option<String>,
) -> (Report, Option<String>) {
    match hardware::detect(Some(backend), device.max_memory_bytes) {
        Ok(offer) => (
            Report {
                backend: backend.as_str().to_string(),
                total_memory_bytes: offer.total_bytes,
                usable_memory_bytes: offer.usable_bytes,
                llama_build: Some(llama::BUILD.to_string()),
                rpc_endpoint: endpoint,
            },
            None,
        ),
        // A machine that cannot offer anything right now still heartbeats,
        // as offering nothing, so the planner leaves it out instead of
        // waiting for it to go stale.
        Err(e) => (
            Report {
                backend: backend.as_str().to_string(),
                total_memory_bytes: 0,
                usable_memory_bytes: 0,
                llama_build: Some(llama::BUILD.to_string()),
                rpc_endpoint: None,
            },
            Some(format!("{e:#}")),
        ),
    }
}

/// Run the device until it is stopped or removed. `say` prints progress for a
/// foreground run; the service passes a no-op.
pub async fn run(say: &dyn Fn(&str)) -> Result<()> {
    let device = state::load()?
        .ok_or_else(|| {
            anyhow!("This machine is not a compute device. Run `knaix device join` first.")
        })
        .coded(Code::Precondition)?;
    if let Some(pid) = running_pid() {
        if pid != std::process::id() {
            return Err(anyhow!(
                "The device is already running on this machine (process {pid}). See `knaix device status`."
            ))
            .coded(Code::Precondition);
        }
    }
    let backend = Backend::parse(&device.backend)?;
    let variant = llama::VARIANTS
        .iter()
        .find(|v| v.id == device.variant)
        .ok_or_else(|| anyhow!("This version of knaix does not know the llama.cpp build '{}'. Run `knaix device join` again.", device.variant))
        .coded(Code::Precondition)?;
    llama::verify(variant)?;
    let key = PinnedKey::from_pem(&device.ticket_key_pem)?;
    if key.key_id != device.ticket_key_id {
        return Err(anyhow!(
            "The pinned ticket key does not match its recorded id. Run `knaix device rekey`."
        ))
        .coded(Code::Denied);
    }

    let ts = tailnet::require()?;
    let status = tailnet_address(&ts, Duration::from_secs(120)).await?;
    if let Some(tag) = &device.compute_tag {
        if status.compute_tag() != Some(tag.as_str()) {
            return Err(anyhow!(
                "This machine is no longer on Kovalent's tailnet with its device tag ({tag}). Run `knaix device join` to rejoin."
            ))
            .coded(Code::Precondition);
        }
    }
    let ip = status.ipv4.expect("running() checked the address");
    let listener = TcpListener::bind((ip, device.gateway_port))
        .await
        .with_context(|| format!("Could not listen on {ip}:{}; is another program using that port? Choose another with `knaix device join --port`.", device.gateway_port))
        .coded(Code::Precondition)?;
    let endpoint = format!("{ip}:{}", device.gateway_port);

    let shared = Arc::new(Shared {
        runtime: Mutex::new(Runtime {
            pid: std::process::id(),
            started_at: state::now_unix(),
            gateway: Some(endpoint.clone()),
            ..Runtime::default()
        }),
        upstream: Arc::new(RwLock::new(None)),
        stopping: AtomicBool::new(false),
        changed: tokio::sync::Notify::new(),
    });
    shared.update(|_| {});

    let gw = Gateway::new(gateway::Config {
        key,
        device_id: device.device_id.clone(),
        upstream: shared.upstream.clone(),
        node_tag: device
            .compute_tag
            .as_deref()
            .and_then(tailnet::node_tag_for),
        tailscale: Some(ts.clone()),
        log: state::gateway_log_path().ok(),
        now: gateway::system_now,
    });
    let gateway_task = tokio::spawn(gw.serve(listener));
    let rpc_task = tokio::spawn(supervise_rpc(shared.clone(), variant));
    say(&format!("  Gateway listening on {endpoint}"));

    let outcome = heartbeat_loop(&device, backend, &ts, ip, &endpoint, &shared, say).await;

    shared.stopping.store(true, Ordering::SeqCst);
    gateway_task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), rpc_task).await;
    sandbox::stop_unit(&sandbox::unit_name());
    state::remove_runtime();
    outcome
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn heartbeat_loop(
    device: &DeviceState,
    backend: Backend,
    ts: &Path,
    ip: Ipv4Addr,
    endpoint: &str,
    shared: &Arc<Shared>,
    say: &dyn Fn(&str),
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let mut wait = Duration::from_secs(0);
    let mut failures: u32 = 0;
    let mut beats: u64 = 0;
    let mut told_rotation = false;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            _ = &mut signal => {
                say("  Stopping.");
                return Ok(());
            }
            _ = tokio::time::sleep(wait) => {}
            _ = shared.changed.notified() => {}
        }
        beats += 1;

        // A changed address means the gateway is listening somewhere nobody
        // can reach. Exit, and let the service start fresh.
        if beats.is_multiple_of(4) {
            match tailnet::status(ts) {
                Ok(s) if s.ipv4 == Some(ip) => {}
                _ => {
                    return Err(anyhow!(
                        "The tailnet address changed or Tailscale stopped; restarting the device."
                    ))
                    .coded(Code::Unavailable)
                }
            }
        }

        let serving = shared.upstream.read().map(|u| u.is_some()).unwrap_or(false);
        let (rep, offer_error) = report(device, backend, serving.then(|| endpoint.to_string()));
        match api::heartbeat(&client, &device.api_url, &device.token, &rep).await {
            Ok(beat) => {
                failures = 0;
                let rotated = beat
                    .ticket_key_id
                    .as_deref()
                    .is_some_and(|id| id != device.ticket_key_id);
                if rotated && !told_rotation {
                    told_rotation = true;
                    say("  The control plane's ticket key changed. Pools cannot use this device until you confirm the new key with `knaix device rekey`.");
                    if let Ok(p) = state::gateway_log_path() {
                        state::append_log(&p, "ticket key rotated on the control plane; still verifying with the pinned key until `knaix device rekey`");
                    }
                }
                shared.update(|rt| {
                    rt.last_heartbeat_at = Some(state::now_unix());
                    rt.last_heartbeat_ok = true;
                    rt.warnings = beat.warnings.clone();
                    rt.server_ticket_key_id = beat.ticket_key_id.clone();
                    rt.usable_memory_bytes = Some(rep.usable_memory_bytes);
                    if let Some(e) = &offer_error {
                        rt.last_error = Some(e.clone());
                    }
                });
                wait = Duration::from_secs(beat.next_heartbeat_seconds.unwrap_or(15).clamp(5, 300));
            }
            Err(e) if crate::exit::code_of(&e) == Code::Auth => {
                shared.update(|rt| {
                    rt.last_heartbeat_ok = false;
                    rt.last_error = Some(format!("{e:#}"));
                });
                return Err(e);
            }
            Err(e) => {
                failures += 1;
                shared.update(|rt| {
                    rt.last_heartbeat_ok = false;
                    rt.last_error = Some(format!("{e:#}"));
                });
                // 15 s, then backing off to 2 minutes while the control plane
                // is away; the gateway keeps serving open pools meanwhile.
                wait = Duration::from_secs((15u64 << failures.min(3)).min(120));
            }
        }
    }
}
