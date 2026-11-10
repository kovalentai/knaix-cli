//! What this machine remembers about being a compute device.
//!
//! Two files, both under `~/.knaix` and both written 0600 and atomically:
//!
//! - `device.json`: the enrolment. The device token, the pinned ticket key, the
//!   gateway port. Written by `join` and `rekey`, removed by `leave`.
//! - `device/runtime.json`: what the running device process is doing, written
//!   by `knaix device run` so `status` can report it without asking it.
//!
//! Logs live beside the runtime file: `gateway.log` (every refusal, with its
//! reason, which never goes on the wire) and `rpc-server.log`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DeviceState {
    pub device_id: String,
    /// `kvd_...`. Authenticates heartbeats, and nothing else.
    pub token: String,
    pub name: String,
    /// The control plane this device enrolled with. Heartbeats go here even if
    /// `knaix config` later points the CLI somewhere else.
    pub api_url: String,
    /// The ticket key pinned at enrolment. A different key is a rotation for
    /// the owner to confirm with `knaix device rekey`, never one to adopt.
    pub ticket_key_id: String,
    pub ticket_key_pem: String,
    pub gateway_port: u16,
    pub backend: String,
    /// Which pinned llama.cpp archive is installed for this machine.
    pub variant: String,
    /// `--max-memory`, when the owner capped what the device offers.
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
    /// `tag:c-<clientId>` on Kovalent's tailnet; absent on the account's own.
    #[serde(default)]
    pub compute_tag: Option<String>,
    /// Whether `join` brought Tailscale up itself, so `leave` can say whether
    /// logging Tailscale out is the owner's call or undoing our own step.
    #[serde(default)]
    pub joined_tailnet_by_knaix: bool,
    pub enrolled_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Runtime {
    pub pid: u32,
    pub started_at: u64,
    /// `ip:port` the gateway listens on, or None until it does.
    pub gateway: Option<String>,
    pub rpc_port: Option<u16>,
    pub rpc_server_running: bool,
    /// Which sandbox rpc-server runs under: `systemd`, `bwrap` or `sandbox-exec`.
    pub sandbox: Option<String>,
    pub last_heartbeat_at: Option<u64>,
    pub last_heartbeat_ok: bool,
    pub last_error: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The key id the control plane reported on the last heartbeat. Differs
    /// from the pinned one after a rotation the owner has not confirmed.
    pub server_ticket_key_id: Option<String>,
    pub usable_memory_bytes: Option<u64>,
}

pub fn knaix_dir() -> PathBuf {
    let mut p = crate::config::get_config_path();
    p.pop();
    p
}

pub fn state_path() -> PathBuf {
    knaix_dir().join("device.json")
}

/// `~/.knaix/device`, created on first use.
pub fn device_dir() -> Result<PathBuf> {
    let dir = knaix_dir().join("device");
    create_private_dir(&dir)?;
    Ok(dir)
}

pub fn runtime_path() -> Result<PathBuf> {
    Ok(device_dir()?.join("runtime.json"))
}

pub fn gateway_log_path() -> Result<PathBuf> {
    Ok(device_dir()?.join("gateway.log"))
}

pub fn rpc_log_path() -> Result<PathBuf> {
    Ok(device_dir()?.join("rpc-server.log"))
}

pub fn service_log_path() -> Result<PathBuf> {
    Ok(device_dir()?.join("service.log"))
}

/// The one directory rpc-server may write: its tensor cache.
pub fn rpc_cache_dir() -> Result<PathBuf> {
    let dir = device_dir()?.join("rpc-cache");
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Where the Linux bridge puts its socket, bound into the sandbox.
///
/// Not under `~/.knaix`: a Unix socket path is limited to about 100 bytes, and
/// a long home directory runs past it. The per-user runtime directory systemd
/// provides is short and private; without one, a directory in the system temp
/// that this user owns and no one else can enter.
pub fn rpc_socket_dir() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    private_socket_dir(socket_dir_in(runtime))
}

/// The socket directory under `runtime`, systemd's per-user directory, when
/// there is one.
fn socket_dir_in(runtime: Option<PathBuf>) -> PathBuf {
    match runtime {
        Some(base) if base.is_dir() => base.join("knaix-device"),
        _ => std::env::temp_dir().join(format!("knaix-device-{}", user_id())),
    }
}

/// `dir`, created if need be, once it is a short, private directory of ours.
fn private_socket_dir(dir: PathBuf) -> Result<PathBuf> {
    create_private_dir(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(&dir)?;
        if meta.file_type().is_symlink() || meta.uid() != user_id() || meta.mode() & 0o077 != 0 {
            return Err(anyhow::anyhow!(
                "{} is not a private directory owned by you; remove it and run this again.",
                dir.display()
            ));
        }
    }
    if dir.join("rpc.sock").as_os_str().len() > 100 {
        return Err(anyhow::anyhow!(
            "The socket path under {} is too long for a Unix socket.",
            dir.display()
        ));
    }
    Ok(dir)
}

fn user_id() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("Could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn load() -> Result<Option<DeviceState>> {
    load_from(&state_path())
}

pub fn load_from(path: &Path) -> Result<Option<DeviceState>> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map(Some).with_context(|| {
            format!(
                "{} is not a device record this version can read",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("Could not read {}", path.display())),
    }
}

pub fn save(state: &DeviceState) -> Result<()> {
    write_private(
        &state_path(),
        serde_json::to_string_pretty(state)?.as_bytes(),
    )
}

pub fn remove() -> Result<()> {
    match fs::remove_file(state_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("Could not remove the device record"),
    }
}

pub fn load_runtime() -> Option<Runtime> {
    let raw = fs::read_to_string(runtime_path().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_runtime(runtime: &Runtime) -> Result<()> {
    write_private(
        &runtime_path()?,
        serde_json::to_string_pretty(runtime)?.as_bytes(),
    )
}

pub fn remove_runtime() {
    if let Ok(p) = runtime_path() {
        let _ = fs::remove_file(p);
    }
}

/// Write a file readable by its owner only, replacing the old one in a single
/// rename so a crash never leaves half a token on disk.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("The path has no parent directory")?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("Could not write {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("Could not replace {}", path.display()))?;
    Ok(())
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "12 s ago", "4 min ago", "3 h ago", for status output.
pub fn ago(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=59 => format!("{d} s ago"),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=172_799 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

/// Append a line to a log, starting a fresh file once it passes 5 MB and
/// keeping the previous one as `.1`, so a device left running for months does
/// not fill a disk with refusals.
pub fn append_log(path: &Path, line: &str) {
    const MAX: u64 = 5 * 1024 * 1024;
    if fs::metadata(path).map(|m| m.len() > MAX).unwrap_or(false) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut f) = options.open(path) {
        let _ = writeln!(f, "{} {}", now_unix(), line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DeviceState {
        DeviceState {
            device_id: "88888888-8888-4888-8888-888888888888".into(),
            token: "kvd_secret".into(),
            name: "studio".into(),
            api_url: "https://api.kovalentai.com".into(),
            ticket_key_id: "c1d40ab1863aca6e".into(),
            ticket_key_pem: "-----BEGIN PUBLIC KEY-----\n...\n-----END PUBLIC KEY-----\n".into(),
            gateway_port: 47100,
            backend: "metal".into(),
            variant: "macos-arm64".into(),
            max_memory_bytes: None,
            compute_tag: Some("tag:c-acme".into()),
            joined_tailnet_by_knaix: true,
            enrolled_at: 1_793_000_000,
        }
    }

    #[test]
    fn the_record_round_trips_and_is_private() {
        let dir = std::env::temp_dir().join(format!("knaix-device-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device.json");
        write_private(&path, serde_json::to_string(&sample()).unwrap().as_bytes()).unwrap();
        assert_eq!(load_from(&path).unwrap(), Some(sample()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the token must be readable by its owner only");
        }
        // No temporary file is left behind.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_record_is_not_an_error() {
        let path = std::env::temp_dir().join("knaix-no-such-device.json");
        assert_eq!(load_from(&path).unwrap(), None);
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(100, 112), "12 s ago");
        assert_eq!(ago(0, 240), "4 min ago");
        assert_eq!(ago(0, 3 * 3600), "3 h ago");
        assert_eq!(ago(0, 3 * 86_400), "3 days ago");
        assert_eq!(ago(200, 100), "0 s ago");
    }

    use crate::config::ScratchHome;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_record_is_saved_loaded_and_forgotten_under_the_knaix_home() {
        let home = ScratchHome::new();
        assert_eq!(state_path(), home.0.join(".knaix/device.json"));
        assert_eq!(load().unwrap(), None);

        save(&sample()).unwrap();
        assert_eq!(load().unwrap(), Some(sample()));
        #[cfg(unix)]
        assert_eq!(mode(&state_path()), 0o600);

        remove().unwrap();
        assert_eq!(load().unwrap(), None);
        remove().unwrap();
    }

    #[test]
    fn a_record_that_cannot_be_read_or_removed_says_so() {
        let _home = ScratchHome::new();
        fs::write(state_path(), "{ not json").unwrap();
        let err = load().unwrap_err();
        assert!(
            err.to_string()
                .ends_with("is not a device record this version can read"),
            "{err}"
        );

        // A directory where the file belongs can be neither read nor removed.
        fs::remove_file(state_path()).unwrap();
        fs::create_dir(state_path()).unwrap();
        assert!(load()
            .unwrap_err()
            .to_string()
            .starts_with("Could not read"));
        assert_eq!(
            remove().unwrap_err().to_string(),
            "Could not remove the device record"
        );
    }

    #[test]
    fn the_runtime_file_round_trips_and_junk_reads_as_none() {
        let _home = ScratchHome::new();
        assert_eq!(load_runtime(), None);
        let rt = Runtime {
            pid: 42,
            gateway: Some("100.64.0.9:47100".into()),
            ..Runtime::default()
        };
        save_runtime(&rt).unwrap();
        assert_eq!(load_runtime(), Some(rt));
        remove_runtime();
        assert_eq!(load_runtime(), None);

        fs::write(runtime_path().unwrap(), "junk").unwrap();
        assert_eq!(load_runtime(), None);
    }

    #[test]
    fn the_device_files_live_in_one_private_directory() {
        let home = ScratchHome::new();
        let dir = home.0.join(".knaix/device");
        assert_eq!(device_dir().unwrap(), dir);
        assert_eq!(gateway_log_path().unwrap(), dir.join("gateway.log"));
        assert_eq!(rpc_log_path().unwrap(), dir.join("rpc-server.log"));
        assert_eq!(service_log_path().unwrap(), dir.join("service.log"));
        assert_eq!(rpc_cache_dir().unwrap(), dir.join("rpc-cache"));
        #[cfg(unix)]
        assert_eq!((mode(&dir), mode(&dir.join("rpc-cache"))), (0o700, 0o700));
    }

    #[test]
    fn a_log_starts_afresh_past_five_megabytes_and_keeps_the_last_one() {
        let home = ScratchHome::new();
        let log = home.0.join("gateway.log");
        append_log(&log, "refused: bad ticket");
        let line = fs::read_to_string(&log).unwrap();
        assert!(line.ends_with(" refused: bad ticket\n"), "{line:?}");
        #[cfg(unix)]
        assert_eq!(mode(&log), 0o600);

        fs::write(&log, vec![b'x'; 5 * 1024 * 1024 + 1]).unwrap();
        append_log(&log, "next");
        assert_eq!(
            fs::metadata(log.with_extension("log.1")).unwrap().len(),
            5 * 1024 * 1024 + 1
        );
        assert!(fs::read_to_string(&log).unwrap().ends_with(" next\n"));
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_directory_must_be_short_private_and_ours() {
        let home = ScratchHome::new();
        let dir = private_socket_dir(home.0.join("sock")).unwrap();
        assert_eq!(mode(&dir), 0o700);

        // A link could point the socket somewhere another user controls.
        let link = home.0.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let err = private_socket_dir(link).unwrap_err();
        assert!(err
            .to_string()
            .ends_with("is not a private directory owned by you; remove it and run this again."));

        let long = home.0.join("x".repeat(100));
        let err = private_socket_dir(long).unwrap_err();
        assert!(err.to_string().ends_with("is too long for a Unix socket."));

        // The real one: the runtime directory, or one of ours in the temp dir.
        assert!(rpc_socket_dir().unwrap().is_dir());
    }

    #[test]
    fn the_socket_lives_in_the_runtime_directory_when_there_is_one() {
        let home = ScratchHome::new();
        assert_eq!(
            socket_dir_in(Some(home.0.clone())),
            home.0.join("knaix-device")
        );
        let fallback = std::env::temp_dir().join(format!("knaix-device-{}", user_id()));
        assert_eq!(socket_dir_in(None), fallback);
        assert_eq!(socket_dir_in(Some(home.0.join("missing"))), fallback);
    }
}
