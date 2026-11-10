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

/// Wait for the tailnet address, for up to `wait`, asking `every` so often.
async fn tailnet_address(bin: &Path, wait: Duration, every: Duration) -> Result<tailnet::Status> {
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
        tokio::time::sleep(every).await;
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
    let ran = run_plan(shared, &plan, port, rpc_log).await;
    if let Some(unit) = &plan.unit {
        sandbox::stop_unit(unit);
    }
    ran
}

/// Start `plan`, wait for it to answer, and wait again for it to exit or for
/// the device to stop. Returns how long it ran.
async fn run_plan(
    shared: &Arc<Shared>,
    plan: &sandbox::Plan,
    port: u16,
    rpc_log: Option<&Path>,
) -> Result<Duration> {
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
    let status = tailnet_address(&ts, Duration::from_secs(120), Duration::from_secs(5)).await?;
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

    let outcome = heartbeat_loop(
        &device,
        backend,
        &ts,
        ip,
        &endpoint,
        &shared,
        say,
        shutdown_signal(),
    )
    .await;

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

/// When to beat next: when the control plane asked, within 5 s and 5 minutes.
fn next_beat(asked_seconds: Option<u64>) -> Duration {
    Duration::from_secs(asked_seconds.unwrap_or(15).clamp(5, 300))
}

/// How long to wait after `failures` heartbeats in a row failed: 30 s,
/// doubling to 2 minutes while the control plane is away.
fn retry_after(failures: u32) -> Duration {
    Duration::from_secs(15 << failures.min(3))
}

/// Heartbeat until `stop` completes, the control plane refuses the device, or
/// the tailnet address changes.
#[allow(clippy::too_many_arguments)]
async fn heartbeat_loop(
    device: &DeviceState,
    backend: Backend,
    ts: &Path,
    ip: Ipv4Addr,
    endpoint: &str,
    shared: &Arc<Shared>,
    say: &dyn Fn(&str),
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let mut wait = Duration::from_secs(0);
    let mut failures: u32 = 0;
    let mut beats: u64 = 0;
    let mut told_rotation = false;
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => {
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
                wait = next_beat(beat.next_heartbeat_seconds);
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
                // The gateway keeps serving open pools meanwhile.
                wait = retry_after(failures);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScratchHome;
    use crate::exit::code_of;
    use std::cell::RefCell;

    #[cfg(unix)]
    use crate::testing::{args_of, fake_api, fake_tool};
    #[cfg(unix)]
    use serde_json::json;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            runtime: Mutex::new(Runtime {
                pid: std::process::id(),
                last_error: Some("from before".into()),
                ..Runtime::default()
            }),
            upstream: Arc::new(RwLock::new(None)),
            stopping: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        })
    }

    fn saved() -> Runtime {
        state::load_runtime().expect("runtime.json was saved")
    }

    #[test]
    fn heartbeats_keep_to_the_asked_pace_within_bounds_and_back_off_when_failing() {
        assert_eq!(next_beat(None), Duration::from_secs(15));
        assert_eq!(next_beat(Some(30)), Duration::from_secs(30));
        assert_eq!(next_beat(Some(0)), Duration::from_secs(5));
        assert_eq!(next_beat(Some(86_400)), Duration::from_secs(300));
        let waits: Vec<u64> = (1..=5).map(|n| retry_after(n).as_secs()).collect();
        assert_eq!(waits, [30, 60, 120, 120, 120]);
    }

    #[cfg(unix)]
    const UP: &str =
        r#"echo '{"BackendState":"Running","Self":{"TailscaleIPs":["fd7a::1","100.64.0.1"]}}'"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn the_tailnet_address_is_waited_for_then_given_up_on() {
        let home = ScratchHome::new();
        let every = Duration::from_millis(10);
        // Logged out for two asks, then up.
        let ts = fake_tool(
            &home.0,
            "tailscale",
            &format!(
                "n=$(cat \"$0.n\" 2>/dev/null || echo 0); echo $((n+1)) > \"$0.n\"\n\
                 if [ \"$n\" -lt 2 ]; then echo '{{\"BackendState\":\"NeedsLogin\"}}'; else {UP}; fi"
            ),
        );
        let s = tailnet_address(Path::new(&ts), Duration::from_secs(10), every)
            .await
            .unwrap();
        assert_eq!(s.ipv4, Some(Ipv4Addr::new(100, 64, 0, 1)));
        assert_eq!(args_of(&ts), ["status --json"; 3]);

        let down = fake_tool(&home.0, "down", r#"echo '{"BackendState":"Stopped"}'"#);
        for bin in [down.as_str(), "/nonexistent/tailscale"] {
            let e = tailnet_address(Path::new(bin), Duration::ZERO, every)
                .await
                .unwrap_err();
            assert_eq!(code_of(&e), Code::Precondition);
            assert!(e.to_string().contains("Tailscale is not connected"), "{e}");
        }
        assert_eq!(args_of(&down).len(), 1, "asked once, then gave up");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_server_is_ready_once_it_answers_and_not_if_it_exits_first() {
        let home = ScratchHome::new();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let tcp = Upstream::Tcp(listener.local_addr().unwrap().port());
        let mut sleeping = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert!(wait_ready(&tcp, &mut sleeping).await);

        let socket = Upstream::Unix(home.0.join("rpc.sock"));
        let mut exits = tokio::process::Command::new("true").spawn().unwrap();
        assert!(!wait_ready(&socket, &mut exits).await);
        let _bound = tokio::net::UnixListener::bind(home.0.join("rpc.sock")).unwrap();
        assert!(answers(&socket).await);
    }

    #[cfg(unix)]
    fn plan(script: &str, upstream: Upstream) -> sandbox::Plan {
        sandbox::Plan {
            kind: sandbox::Kind::Bwrap,
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("GREETING".into(), "hello".into())],
            upstream,
            unit: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_server_that_answers_is_published_and_logged_until_the_device_stops() {
        let home = ScratchHome::new();
        let shared = shared();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = home.0.join("rpc.log");
        let p = plan(
            r#"echo $$; echo "$GREETING; PATH=$PATH; HOME=$HOME"; exec sleep 30"#,
            Upstream::Tcp(port),
        );
        let stop = async {
            while shared.upstream.read().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let rt = saved();
            assert!(rt.rpc_server_running);
            assert_eq!(rt.rpc_port, Some(port));
            assert_eq!(rt.sandbox.as_deref(), Some("bwrap"));
            assert_eq!(rt.last_error, None, "a start clears the last error");
            shared.stopping.store(true, Ordering::SeqCst);
        };
        let (ran, ()) = tokio::join!(run_plan(&shared, &p, port, Some(&log)), stop);
        assert!(
            ran.unwrap() < Duration::from_secs(30),
            "stopped, not waited out"
        );
        assert_eq!(*shared.upstream.read().unwrap(), Some(Upstream::Tcp(port)));
        let out = std::fs::read_to_string(&log).unwrap();
        let (pid, out) = out.split_once('\n').unwrap();
        // Only the plan's environment and a minimal PATH reach it.
        assert_eq!(out, "hello; PATH=/usr/bin:/bin:/usr/sbin:/sbin; HOME=\n");
        // Killed and reaped before it returns, so a restart finds the port free.
        assert!(
            !alive(pid.parse().unwrap()),
            "rpc-server {pid} outlived the stop"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rpc_server_that_exits_is_over_and_one_that_never_answers_is_an_error() {
        let home = ScratchHome::new();
        let shared = shared();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let ran = run_plan(&shared, &plan("sleep 0.2", Upstream::Tcp(port)), port, None)
            .await
            .unwrap();
        assert!(ran >= Duration::from_millis(200) && ran < Duration::from_secs(10));
        assert!(
            saved().rpc_server_running,
            "the supervisor clears it, not this"
        );

        // A stale socket from an earlier run is removed before the start.
        let sock = home.0.join("rpc.sock");
        std::fs::write(&sock, "stale").unwrap();
        state::remove_runtime();
        let e = run_plan(
            &shared,
            &plan("exit 3", Upstream::Unix(sock.clone())),
            1,
            None,
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("did not start listening"), "{e}");
        assert!(!sock.exists());
        assert!(state::load_runtime().is_none(), "nothing was published");

        let mut missing = plan("", Upstream::Tcp(port));
        missing.program = "/nonexistent/rpc-server".into();
        let e = run_plan(&shared, &missing, port, None).await.unwrap_err();
        assert!(e.to_string().contains("Could not start rpc-server"), "{e}");
    }

    #[cfg(unix)]
    fn device(api_url: &str, max_memory_bytes: Option<u64>) -> DeviceState {
        DeviceState {
            device_id: "dev".into(),
            token: "kvd_tok".into(),
            name: "studio".into(),
            api_url: api_url.into(),
            ticket_key_id: "pinned".into(),
            ticket_key_pem: String::new(),
            gateway_port: 47100,
            backend: "cpu".into(),
            variant: "ubuntu-x64".into(),
            max_memory_bytes,
            compute_tag: None,
            joined_tailnet_by_knaix: false,
            enrolled_at: 0,
        }
    }

    const HEARTBEAT: &str = "POST /api/compute/devices/heartbeat";
    const ENDPOINT: &str = "100.64.0.1:47100";

    /// Run the heartbeat loop with every beat woken at once, as rpc-server's
    /// changes wake it, so a test needs no real waits. Returns its outcome and
    /// what it said.
    #[cfg(unix)]
    async fn beat(
        device: &DeviceState,
        ts: &str,
        shared: &Arc<Shared>,
        stop: impl std::future::Future<Output = ()>,
    ) -> (Result<()>, Vec<String>) {
        let said = RefCell::new(Vec::new());
        let say = |line: &str| said.borrow_mut().push(line.to_string());
        let wake = async {
            loop {
                shared.changed.notify_one();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let ip = Ipv4Addr::new(100, 64, 0, 1);
        let run = async {
            tokio::select! {
            r = heartbeat_loop(device, Backend::Cpu, Path::new(ts), ip, ENDPOINT, shared, &say, stop) => r,
            _ = wake => unreachable!(),
            }
        };
        let outcome = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .expect("the heartbeat loop never ended");
        (outcome, said.take())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn heartbeats_record_the_beat_and_stop_when_asked() {
        let home = ScratchHome::new();
        let api = fake_api(vec![(
            HEARTBEAT,
            200,
            json!({"warnings": [], "ticketKeyId": "pinned"}),
        )])
        .await;
        let ts = fake_tool(&home.0, "tailscale", UP);
        let shared = shared();
        *shared.upstream.write().unwrap() = Some(Upstream::Tcp(1));
        let stop = async {
            while api.calls().len() < 5 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (outcome, said) = beat(&device(&api.url, None), &ts, &shared, stop).await;
        outcome.unwrap();
        assert_eq!(said, ["  Stopping."], "no key rotation to tell of");
        let calls = api.calls();
        assert!(calls.len() >= 5);
        for c in &calls {
            assert_eq!(c.auth.as_deref(), Some("Bearer kvd_tok"));
            assert_eq!(c.body["backend"], "cpu");
            assert_eq!(c.body["llamaBuild"], llama::BUILD);
            // Serving, so the planner hears where.
            assert_eq!(c.body["rpcEndpoint"], ENDPOINT);
            assert!(c.body["usableMemoryBytes"].as_u64().unwrap() > 0);
        }
        // The address is checked every fourth beat, and was unchanged.
        assert_eq!(args_of(&ts).len(), calls.len() / 4);
        let rt = saved();
        assert!(rt.last_heartbeat_ok && rt.last_heartbeat_at.is_some());
        assert_eq!(
            rt.last_error.as_deref(),
            Some("from before"),
            "kept unless an offer failed"
        );
        assert_eq!(rt.server_ticket_key_id.as_deref(), Some("pinned"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_rotated_key_is_told_once_an_empty_offer_still_beats_and_a_moved_address_ends_it() {
        let home = ScratchHome::new();
        let reply = json!({"warnings": ["llama_build_unknown"], "ticketKeyId": "rotated"});
        let api = fake_api(vec![(HEARTBEAT, 200, reply)]).await;
        let moved = fake_tool(
            &home.0,
            "tailscale",
            &UP.replace("100.64.0.1", "100.64.0.2"),
        );
        let shared = shared();
        // Under the 1 GB a pool can use: an offer of nothing.
        let (outcome, said) = beat(
            &device(&api.url, Some(1)),
            &moved,
            &shared,
            std::future::pending(),
        )
        .await;
        let e = outcome.unwrap_err();
        assert_eq!(code_of(&e), Code::Unavailable);
        assert!(e.to_string().contains("tailnet address changed"), "{e}");
        assert_eq!(
            api.calls().len(),
            3,
            "the address is checked at the fourth beat"
        );
        assert_eq!(args_of(&moved), ["status --json"]);
        for c in api.calls() {
            assert_eq!(c.body["usableMemoryBytes"], 0);
            assert_eq!(
                c.body["rpcEndpoint"],
                serde_json::Value::Null,
                "not serving"
            );
        }
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("ticket key changed"), "{said:?}");
        let rt = saved();
        assert!(rt.last_heartbeat_ok);
        assert_eq!(rt.warnings, ["llama_build_unknown"]);
        assert_eq!(rt.server_ticket_key_id.as_deref(), Some("rotated"));
        assert_eq!(rt.usable_memory_bytes, Some(0));
        assert!(rt.last_error.unwrap().contains("under the 1 GB"));
        let log = std::fs::read_to_string(state::gateway_log_path().unwrap()).unwrap();
        assert_eq!(log.matches("ticket key rotated").count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_control_plane_is_recorded_and_a_refusal_ends_the_device() {
        let home = ScratchHome::new();
        let ts = fake_tool(&home.0, "tailscale", UP);

        let away = fake_api(vec![(HEARTBEAT, 500, json!({"detail": "down"}))]).await;
        let failing = shared();
        let stop = async {
            while away.calls().len() < 5 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (outcome, _) = beat(&device(&away.url, None), &ts, &failing, stop).await;
        outcome.unwrap();
        let rt = saved();
        assert!(!rt.last_heartbeat_ok);
        assert!(rt.last_error.unwrap().contains("heartbeat was refused"));

        let refused = fake_api(vec![(HEARTBEAT, 401, serde_json::Value::Null)]).await;
        let again = shared();
        let (outcome, _) = beat(
            &device(&refused.url, None),
            &ts,
            &again,
            std::future::pending(),
        )
        .await;
        assert_eq!(code_of(&outcome.unwrap_err()), Code::Auth);
        assert_eq!(refused.calls().len(), 1);
        let rt = saved();
        assert!(!rt.last_heartbeat_ok);
        assert!(rt
            .last_error
            .unwrap()
            .contains("no longer recognizes this device"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn between_beats_it_waits_as_told_or_backs_off() {
        let home = ScratchHome::new();
        let ts = fake_tool(&home.0, "tailscale", UP);
        let shared = shared();
        let ip = Ipv4Addr::new(100, 64, 0, 1);
        let say = |_: &str| {};
        for (status, reply) in [(200, json!({"nextHeartbeatSeconds": 0})), (500, json!({}))] {
            let api = fake_api(vec![(HEARTBEAT, status, reply)]).await;
            let d = device(&api.url, None);
            let stop = tokio::time::sleep(Duration::from_millis(500));
            heartbeat_loop(
                &d,
                Backend::Cpu,
                Path::new(&ts),
                ip,
                ENDPOINT,
                &shared,
                &say,
                stop,
            )
            .await
            .unwrap();
            assert_eq!(api.calls().len(), 1, "a {status} beat was followed at once");
        }
    }
}
