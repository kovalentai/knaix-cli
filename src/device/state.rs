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
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(base) if base.is_dir() => base.join("knaix-device"),
        _ => std::env::temp_dir().join(format!("knaix-device-{}", user_id())),
    };
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
}
