//! Running rpc-server inside a sandbox.
//!
//! Required, not optional: the device refuses to start rpc-server without one,
//! because the sandbox is what keeps an unknown ggml-rpc bug from becoming
//! access to the owner's files or network.
//!
//! - Linux: rpc-server gets a network namespace with nothing but loopback (no
//!   route to the tailnet, the LAN or the internet), the home directory
//!   replaced by an empty tmpfs with only the install (read-only), the tensor
//!   cache and the bridge's socket directory bound in, and no new privileges.
//!   A transient systemd user unit where systemd user sessions work, otherwise
//!   bubblewrap with the same view. Because the gateway cannot reach into that
//!   namespace over TCP, `knaix device bridge` runs inside with rpc-server and
//!   hands connections across on a Unix socket.
//! - macOS: `sandbox-exec` with a profile that hides `/Users` and `/Volumes`
//!   except the install (read) and the cache (read and write), allows writes
//!   nowhere else but system temp, allows network on loopback only, and will
//!   not start a shell or a scripting runtime. The gateway reaches rpc-server
//!   on its loopback port.
//!
//! Every sandbox is probed with a harmless command first, so one that cannot
//! actually apply its policy is found before rpc-server depends on it.

use crate::device::gateway::Upstream;
use crate::exit::{Code, WithCode};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Systemd,
    Bwrap,
    SandboxExec,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Systemd => "systemd",
            Kind::Bwrap => "bwrap",
            Kind::SandboxExec => "sandbox-exec",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub kind: Kind,
    /// The program to run and its arguments, sandbox wrapper first.
    pub program: String,
    pub args: Vec<String>,
    /// Environment for the wrapper; the device's own is cleared. For
    /// `sandbox-exec` this is also rpc-server's, since it passes it through.
    pub env: Vec<(String, String)>,
    /// Where the gateway reaches rpc-server once it is up.
    pub upstream: Upstream,
    /// A systemd unit to stop on shutdown, when the sandbox is one.
    pub unit: Option<String>,
}

pub struct Paths<'a> {
    /// ggml-rpc-server.
    pub binary: &'a Path,
    /// The directory it was installed to, with its libraries.
    pub install: &'a Path,
    /// The tensor cache: the one directory rpc-server writes.
    pub cache: &'a Path,
    /// The knaix binary, which runs the bridge inside the Linux sandbox.
    pub exe: &'a Path,
    /// Where the bridge puts its socket (Linux).
    pub socket_dir: &'a Path,
    /// rpc-server's loopback port inside the sandbox.
    pub port: u16,
}

impl Paths<'_> {
    pub fn socket(&self) -> PathBuf {
        self.socket_dir.join("rpc.sock")
    }
}

fn rpc_args(p: &Paths) -> Vec<String> {
    vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        p.port.to_string(),
        "--cache".into(),
    ]
}

/// rpc-server behind the bridge: what runs inside a Linux sandbox.
fn bridged(p: &Paths) -> Vec<String> {
    let mut args = vec![
        p.exe.display().to_string(),
        "device".into(),
        "bridge".into(),
        "--socket".into(),
        p.socket().display().to_string(),
        "--port".into(),
        p.port.to_string(),
        "--".into(),
        p.binary.display().to_string(),
    ];
    args.extend(rpc_args(p));
    args
}

/// The systemd unit name for this device's rpc-server.
pub fn unit_name() -> String {
    "knaix-rpc-server".to_string()
}

/// `systemd-run --user` properties. Pure, so the policy is tested as text.
pub fn systemd_properties(p: &Paths, env: &[(String, String)]) -> Vec<String> {
    let mut props = vec![
        "PrivateNetwork=yes".to_string(),
        "NoNewPrivileges=yes".to_string(),
        "ProtectSystem=strict".to_string(),
        "ProtectHome=tmpfs".to_string(),
        format!("BindReadOnlyPaths={}", p.install.display()),
        format!("BindReadOnlyPaths={}", p.exe.display()),
        format!("BindPaths={}", p.cache.display()),
        format!("BindPaths={}", p.socket_dir.display()),
        "PrivateTmp=yes".to_string(),
        "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX".to_string(),
        "SystemCallFilter=@system-service".to_string(),
        "LockPersonality=yes".to_string(),
        "RestrictRealtime=yes".to_string(),
        "UMask=0077".to_string(),
    ];
    for (k, v) in env {
        props.push(format!("Environment={k}={v}"));
    }
    props
}

pub fn systemd_plan(p: &Paths, env: &[(String, String)], session: &[(String, String)]) -> Plan {
    let mut args = vec![
        "--user".to_string(),
        "--quiet".to_string(),
        "--collect".to_string(),
        "--wait".to_string(),
        "--pipe".to_string(),
        format!("--unit={}", unit_name()),
    ];
    for prop in systemd_properties(p, env) {
        args.push("-p".into());
        args.push(prop);
    }
    args.push("--".into());
    args.extend(bridged(p));
    Plan {
        kind: Kind::Systemd,
        program: "systemd-run".into(),
        args,
        // systemd-run finds the user manager through these; they are session
        // addresses, not secrets, and they stop at the wrapper.
        env: session.to_vec(),
        upstream: Upstream::Unix(p.socket()),
        unit: Some(unit_name()),
    }
}

pub fn bwrap_plan(p: &Paths, env: &[(String, String)], home: &Path) -> Plan {
    let mut args: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-net",
        "--unshare-user-try",
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--cap-drop",
        "ALL",
        "--ro-bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let d = |path: &Path| path.display().to_string();
    args.extend([
        "--tmpfs".into(),
        d(home),
        "--ro-bind".into(),
        d(p.install),
        d(p.install),
        "--ro-bind".into(),
        d(p.exe),
        d(p.exe),
        "--bind".into(),
        d(p.cache),
        d(p.cache),
        "--bind".into(),
        d(p.socket_dir),
        d(p.socket_dir),
        "--clearenv".into(),
    ]);
    for (k, v) in env {
        args.extend(["--setenv".into(), k.clone(), v.clone()]);
    }
    args.push("--".into());
    args.extend(bridged(p));
    Plan {
        kind: Kind::Bwrap,
        program: "bwrap".into(),
        args,
        env: Vec::new(),
        upstream: Upstream::Unix(p.socket()),
        unit: None,
    }
}

/// The macOS profile. Later rules win in SBPL, so each allow after a deny
/// carves out exactly what rpc-server needs.
pub fn sandbox_exec_profile(p: &Paths) -> String {
    let q = |path: &Path| {
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    };
    format!(
        r#"(version 1)
(allow default)
(deny network*)
(allow network-bind (local ip "localhost:*"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
(allow network-outbound (literal "/private/var/run/syslog"))
(deny file-read* file-write* (subpath "/Users") (subpath "/Volumes"))
(deny file-write*)
(allow file-write* (subpath "/private/var/folders") (subpath "/private/tmp") (subpath "/dev"))
(allow file-read* (subpath "{install}"))
(allow file-read* file-write* (subpath "{cache}"))
(allow file-read-metadata)
(deny process-exec (literal "/bin/sh") (literal "/bin/bash") (literal "/bin/zsh") (literal "/bin/dash") (literal "/bin/ksh") (literal "/bin/csh") (literal "/bin/tcsh") (literal "/usr/bin/env") (literal "/usr/bin/osascript") (literal "/usr/bin/curl") (literal "/usr/bin/nc") (literal "/usr/bin/python3") (literal "/usr/bin/perl") (literal "/usr/bin/ruby"))
"#,
        install = q(p.install),
        cache = q(p.cache),
    )
}

pub fn sandbox_exec_plan(p: &Paths, env: &[(String, String)]) -> Plan {
    let mut args = vec![
        "-p".to_string(),
        sandbox_exec_profile(p),
        p.binary.display().to_string(),
    ];
    args.extend(rpc_args(p));
    Plan {
        kind: Kind::SandboxExec,
        program: "/usr/bin/sandbox-exec".into(),
        args,
        env: env.to_vec(),
        upstream: Upstream::Tcp(p.port),
        unit: None,
    }
}

fn runs(program: &str, args: &[String], env: &[(String, String)]) -> bool {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !env.is_empty() {
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .envs(env.iter().cloned());
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// The probe: the same wrapper around `true`, so a sandbox that cannot apply
/// its own policy is found here and not halfway through a pool load. What
/// follows the wrapper (the bridge or rpc-server) is replaced.
fn probe(plan: &Plan, first_inner: &str) -> bool {
    let truth = if Path::new("/usr/bin/true").exists() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let mut args = plan.args.clone();
    let Some(pos) = args.iter().position(|a| a == first_inner) else {
        return false;
    };
    args.truncate(pos);
    if plan.kind == Kind::Systemd {
        // A different unit name, so a probe never collides with a live server.
        for a in args.iter_mut() {
            if a.starts_with("--unit=") {
                *a = format!("--unit={}-probe-{}", unit_name(), std::process::id());
            }
        }
        args.retain(|a| a != "--pipe");
    }
    args.push(truth.to_string());
    runs(&plan.program, &args, &plan.env)
}

/// The variables that let systemd-run find the user manager.
pub fn session_env() -> Vec<(String, String)> {
    ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// Choose the strongest sandbox this machine can actually run.
pub fn choose(p: &Paths, env: &[(String, String)]) -> Result<Plan> {
    let home = home::home_dir().unwrap_or_else(|| PathBuf::from("/home"));
    choose_on(std::env::consts::OS, p, env, &home, &which, &probe)
}

/// `choose`, given the OS, the home to hide, how to find a wrapper and how to
/// probe a plan.
fn choose_on(
    os: &str,
    p: &Paths,
    env: &[(String, String)],
    home: &Path,
    find: &dyn Fn(&str) -> Option<PathBuf>,
    works: &dyn Fn(&Plan, &str) -> bool,
) -> Result<Plan> {
    match os {
        "macos" => {
            let plan = sandbox_exec_plan(p, env);
            if works(&plan, &p.binary.display().to_string()) {
                return Ok(plan);
            }
            Err(anyhow!(
                "macOS refused to start the rpc-server sandbox (sandbox-exec). The device will not run rpc-server without it."
            ))
            .coded(Code::Precondition)
        }
        "linux" => {
            let exe = p.exe.display().to_string();
            // Resolved to absolute paths here, because rpc-server is started
            // with a minimal PATH that need not contain wherever these live.
            if let Some(program) = find("systemd-run") {
                let mut systemd = systemd_plan(p, env, &session_env());
                systemd.program = program.display().to_string();
                if works(&systemd, &exe) {
                    return Ok(systemd);
                }
            }
            if let Some(program) = find("bwrap") {
                let mut bwrap = bwrap_plan(p, env, home);
                bwrap.program = program.display().to_string();
                if works(&bwrap, &exe) {
                    return Ok(bwrap);
                }
            }
            Err(anyhow!(
                "No sandbox is available for rpc-server. Enable systemd user sessions with user namespaces, or install bubblewrap (`bwrap`), then run this again."
            ))
            .coded(Code::Precondition)
        }
        _ => Err(anyhow!(
            "Compute devices are not supported on this platform yet."
        ))
        .coded(Code::Precondition),
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// Stop a systemd-run unit left from an earlier run, so its socket is free.
pub fn stop_unit(unit: &str) {
    stop_unit_on(std::env::consts::OS, "systemctl", unit);
}

fn stop_unit_on(os: &str, systemctl: &str, unit: &str) {
    if os != "linux" {
        return;
    }
    let _ = Command::new(systemctl)
        .args(["--user", "stop", &format!("{unit}.service")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ScratchHome;
    use crate::exit::{code_of, Code};
    #[cfg(unix)]
    use crate::testing::{args_of, fake_tool};

    struct Fixture {
        binary: PathBuf,
        install: PathBuf,
        cache: PathBuf,
        exe: PathBuf,
        sockets: PathBuf,
    }

    fn linux() -> Fixture {
        Fixture {
            binary: PathBuf::from("/home/maya/.knaix/llama/b11669-ubuntu-x64/ggml-rpc-server"),
            install: PathBuf::from("/home/maya/.knaix/llama/b11669-ubuntu-x64"),
            cache: PathBuf::from("/home/maya/.knaix/device/rpc-cache"),
            exe: PathBuf::from("/home/maya/.cargo/bin/knaix"),
            sockets: PathBuf::from("/home/maya/.knaix/device/rpc"),
        }
    }

    fn paths(f: &Fixture) -> Paths<'_> {
        Paths {
            binary: &f.binary,
            install: &f.install,
            cache: &f.cache,
            exe: &f.exe,
            socket_dir: &f.sockets,
            port: 41234,
        }
    }

    #[test]
    fn rpc_server_binds_loopback_only() {
        let f = linux();
        let p = paths(&f);
        for plan in [
            systemd_plan(&p, &[], &[]),
            bwrap_plan(&p, &[], Path::new("/home/maya")),
            sandbox_exec_plan(&p, &[]),
        ] {
            let at = plan.args.iter().position(|a| a == "--host").unwrap();
            assert_eq!(plan.args[at + 1], "127.0.0.1", "{:?}", plan.kind);
            assert!(!plan.args.iter().any(|a| a == "0.0.0.0"));
        }
    }

    #[test]
    fn on_linux_rpc_server_gets_no_network_and_the_gateway_uses_the_socket() {
        let f = linux();
        let p = paths(&f);
        let systemd = systemd_plan(&p, &[], &[]);
        assert!(systemd.args.iter().any(|a| a == "PrivateNetwork=yes"));
        assert_eq!(systemd.upstream, Upstream::Unix(f.sockets.join("rpc.sock")));
        let bwrap = bwrap_plan(&p, &[], Path::new("/home/maya"));
        assert!(bwrap.args.iter().any(|a| a == "--unshare-net"));
        assert_eq!(bwrap.upstream, Upstream::Unix(f.sockets.join("rpc.sock")));
        for plan in [&systemd, &bwrap] {
            let joined = plan.args.join(" ");
            assert!(
                joined.contains("device bridge --socket"),
                "{:?} does not run the bridge",
                plan.kind
            );
        }
        // macOS restricts network in the profile, so it keeps TCP.
        assert_eq!(sandbox_exec_plan(&p, &[]).upstream, Upstream::Tcp(41234));
    }

    #[test]
    fn the_systemd_unit_hides_home_and_binds_only_what_runs() {
        let f = linux();
        let p = paths(&f);
        let props =
            systemd_properties(&p, &[("LLAMA_CACHE".into(), f.cache.display().to_string())]);
        for required in [
            "PrivateNetwork=yes",
            "NoNewPrivileges=yes",
            "ProtectHome=tmpfs",
            "ProtectSystem=strict",
        ] {
            assert!(props.iter().any(|x| x == required), "missing {required}");
        }
        assert!(props.contains(&format!("BindReadOnlyPaths={}", f.install.display())));
        assert!(props.contains(&format!("BindReadOnlyPaths={}", f.exe.display())));
        assert!(props.contains(&format!("BindPaths={}", f.cache.display())));
        assert!(props.contains(&format!("BindPaths={}", f.sockets.display())));
        assert!(props
            .iter()
            .any(|x| x.starts_with("Environment=LLAMA_CACHE=")));
    }

    #[test]
    fn systemd_run_gets_the_session_address_and_nothing_else() {
        let f = linux();
        let p = paths(&f);
        let session = vec![("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string())];
        let plan = systemd_plan(&p, &[("LLAMA_CACHE".into(), "/c".into())], &session);
        assert_eq!(plan.env, session);
    }

    #[test]
    fn bubblewrap_hides_home_and_clears_the_environment() {
        let f = linux();
        let p = paths(&f);
        let plan = bwrap_plan(&p, &[("A".into(), "1".into())], Path::new("/home/maya"));
        let joined = plan.args.join(" ");
        assert!(joined.contains("--tmpfs /home/maya"));
        assert!(joined.contains(&format!("--ro-bind {0} {0}", f.install.display())));
        assert!(joined.contains(&format!("--ro-bind {0} {0}", f.exe.display())));
        assert!(joined.contains("--clearenv"));
        assert!(joined.contains("--die-with-parent"));
        // Home is hidden before the install and cache are bound back into it.
        let tmpfs = joined.find("--tmpfs /home/maya").unwrap();
        let install = joined
            .find(&format!("--ro-bind {}", f.install.display()))
            .unwrap();
        assert!(tmpfs < install);
    }

    #[test]
    fn the_macos_profile_denies_home_then_allows_install_and_cache() {
        let f = Fixture {
            binary: PathBuf::from("/Users/maya/.knaix/llama/b11669-macos-arm64/ggml-rpc-server"),
            install: PathBuf::from("/Users/maya/.knaix/llama/b11669-macos-arm64"),
            cache: PathBuf::from("/Users/maya/.knaix/device/rpc-cache"),
            exe: PathBuf::from("/opt/homebrew/bin/knaix"),
            sockets: PathBuf::from("/Users/maya/.knaix/device/rpc"),
        };
        let p = paths(&f);
        let profile = sandbox_exec_profile(&p);
        let deny_users = profile
            .find(r#"(deny file-read* file-write* (subpath "/Users")"#)
            .unwrap();
        let allow_install = profile
            .find(&format!(
                r#"(allow file-read* (subpath "{}"))"#,
                f.install.display()
            ))
            .unwrap();
        let allow_cache = profile
            .find(&format!(
                r#"(allow file-read* file-write* (subpath "{}"))"#,
                f.cache.display()
            ))
            .unwrap();
        assert!(
            deny_users < allow_install && deny_users < allow_cache,
            "later rules win in SBPL"
        );
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(r#"(allow network-inbound (local ip "localhost:*"))"#));
        assert!(!profile.contains("(allow network*)"));
        assert!(profile.contains(r#"(literal "/bin/sh")"#));
    }

    #[test]
    fn quotes_in_paths_cannot_break_out_of_the_profile() {
        let f = Fixture {
            binary: PathBuf::from("/Users/x/b"),
            install: PathBuf::from("/Users/x\") (allow file-read* (subpath \"/"),
            cache: PathBuf::from("/Users/x/c"),
            exe: PathBuf::from("/k"),
            sockets: PathBuf::from("/s"),
        };
        let profile = sandbox_exec_profile(&paths(&f));
        assert!(profile.contains(r#"\")"#));
        assert!(!profile.contains(r#"(subpath "/Users/x") (allow"#));
    }

    /// What `choose_on` probed: each plan's kind and what followed the wrapper.
    type Probed = std::cell::RefCell<Vec<(Kind, String, String)>>;

    fn choose_with(os: &str, found: &[&str], working: &[Kind], probed: &Probed) -> Result<Plan> {
        let f = linux();
        let p = paths(&f);
        let find = |name: &str| {
            found
                .contains(&name)
                .then(|| PathBuf::from(format!("/opt/{name}")))
        };
        let works = |plan: &Plan, first_inner: &str| {
            probed.borrow_mut().push((
                plan.kind.clone(),
                plan.program.clone(),
                first_inner.to_string(),
            ));
            working.contains(&plan.kind)
        };
        choose_on(os, &p, &[], Path::new("/home/maya"), &find, &works)
    }

    #[test]
    fn macos_gets_sandbox_exec_or_nothing() {
        let probed = Probed::default();
        let plan = choose_with("macos", &[], &[Kind::SandboxExec], &probed).unwrap();
        assert_eq!(plan.kind.as_str(), "sandbox-exec");
        assert_eq!(plan.program, "/usr/bin/sandbox-exec");
        // Probed with rpc-server replaced, not the bridge.
        assert_eq!(
            probed.take(),
            [(
                Kind::SandboxExec,
                "/usr/bin/sandbox-exec".into(),
                linux().binary.display().to_string()
            )]
        );

        let e = choose_with("macos", &[], &[], &probed).unwrap_err();
        assert_eq!(code_of(&e), Code::Precondition);
        assert!(e
            .to_string()
            .contains("refused to start the rpc-server sandbox"));
    }

    #[test]
    fn linux_prefers_systemd_then_bubblewrap_each_by_its_full_path() {
        let exe = linux().exe.display().to_string();
        let probed = Probed::default();
        let both = ["systemd-run", "bwrap"];

        let plan = choose_with("linux", &both, &[Kind::Systemd, Kind::Bwrap], &probed).unwrap();
        assert_eq!(plan.kind.as_str(), "systemd");
        assert_eq!(plan.program, "/opt/systemd-run");
        assert_eq!(
            probed.take(),
            [(Kind::Systemd, "/opt/systemd-run".into(), exe.clone())]
        );

        // A systemd that cannot apply its policy falls back to bubblewrap,
        // which hides the home it was given.
        let plan = choose_with("linux", &both, &[Kind::Bwrap], &probed).unwrap();
        assert_eq!(plan.kind.as_str(), "bwrap");
        assert_eq!(plan.program, "/opt/bwrap");
        assert!(plan.args.join(" ").contains("--tmpfs /home/maya"));
        assert_eq!(
            probed.take(),
            [
                (Kind::Systemd, "/opt/systemd-run".into(), exe.clone()),
                (Kind::Bwrap, "/opt/bwrap".into(), exe.clone()),
            ]
        );

        let plan = choose_with("linux", &["bwrap"], &[Kind::Bwrap], &probed).unwrap();
        assert_eq!(plan.kind, Kind::Bwrap);
        assert_eq!(probed.take().len(), 1, "systemd-run is not there to probe");
    }

    #[test]
    fn linux_without_a_working_sandbox_and_other_systems_are_refused() {
        let probed = Probed::default();
        for found in [&[][..], &["systemd-run", "bwrap"][..]] {
            let e = choose_with("linux", found, &[], &probed).unwrap_err();
            assert_eq!(code_of(&e), Code::Precondition);
            assert!(e.to_string().contains("No sandbox is available"), "{e}");
        }
        assert_eq!(probed.take().len(), 2, "each wrapper found was probed");

        let e = choose_with("freebsd", &["bwrap"], &[Kind::Bwrap], &probed).unwrap_err();
        assert_eq!(code_of(&e), Code::Precondition);
        assert!(e.to_string().contains("not supported on this platform"));
        assert!(probed.take().is_empty());
    }

    #[cfg(unix)]
    fn truth() -> &'static str {
        if Path::new("/usr/bin/true").exists() {
            "/usr/bin/true"
        } else {
            "/bin/true"
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_runs_the_wrapper_around_true_under_its_own_unit() {
        let home = ScratchHome::new();
        let f = linux();
        let p = paths(&f);
        let exe = f.exe.display().to_string();
        let mut plan = systemd_plan(&p, &[], &[]);
        plan.program = fake_tool(&home.0, "systemd-run", "exit 0");
        assert!(probe(&plan, &exe));
        let line = args_of(&plan.program).pop().unwrap();
        let args: Vec<&str> = line.split(' ').collect();
        assert_eq!(args.last(), Some(&truth()));
        assert!(args
            .contains(&format!("--unit=knaix-rpc-server-probe-{}", std::process::id()).as_str()));
        assert!(
            !args.contains(&"--unit=knaix-rpc-server"),
            "never the live unit"
        );
        assert!(!args.contains(&"--pipe"));
        assert!(
            !line.contains("device bridge"),
            "the bridge is replaced: {line}"
        );
        assert!(args.contains(&"PrivateNetwork=yes"), "the policy is kept");

        // A wrapper that fails, and a plan without what the probe replaces.
        let mut bwrap = bwrap_plan(&p, &[], Path::new("/home/maya"));
        bwrap.program = fake_tool(&home.0, "bwrap", "exit 1");
        assert!(!probe(&bwrap, &exe));
        let line = args_of(&bwrap.program).pop().unwrap();
        assert!(
            line.contains("--unshare-net") && line.ends_with(truth()),
            "{line}"
        );
        assert!(!probe(&bwrap, "/not/in/the/plan"));
        assert_eq!(args_of(&bwrap.program).len(), 1, "nothing ran");
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_with_an_environment_passes_only_that_and_a_minimal_path() {
        let home = ScratchHome::new();
        let f = linux();
        let p = paths(&f);
        let tool = fake_tool(&home.0, "sandbox-exec", "env > \"$0.env\"");
        let read_env = || std::fs::read_to_string(format!("{tool}.env")).unwrap();

        let mut plan = sandbox_exec_plan(&p, &[("LLAMA_CACHE".into(), "/c".into())]);
        plan.program = tool.clone();
        assert!(probe(&plan, &f.binary.display().to_string()));
        let env = read_env();
        assert!(env.contains("LLAMA_CACHE=/c\n"), "{env}");
        assert!(
            env.contains("PATH=/usr/bin:/bin:/usr/sbin:/sbin\n"),
            "{env}"
        );
        assert!(
            !env.contains("HOME="),
            "the device's environment is cleared: {env}"
        );

        // With none to give, the wrapper keeps the device's own.
        plan.env.clear();
        assert!(probe(&plan, &f.binary.display().to_string()));
        assert!(read_env().contains("HOME="));
    }

    #[cfg(unix)]
    #[test]
    fn a_leftover_unit_is_stopped_only_on_linux() {
        let home = ScratchHome::new();
        let systemctl = fake_tool(&home.0, "systemctl", "exit 0");
        stop_unit_on("macos", &systemctl, "knaix-rpc-server");
        assert!(args_of(&systemctl).is_empty());
        stop_unit_on("linux", &systemctl, "knaix-rpc-server");
        assert_eq!(
            args_of(&systemctl),
            ["--user stop knaix-rpc-server.service"]
        );
    }
}
