//! The system Tailscale client: the address the gateway binds and the identity
//! of each peer that connects to it.
//!
//! A device joins with the account's compute tag (`tag:c-<clientId>`), not the
//! user tag `knaix local connect` uses. No rule but a pool's opens the compute
//! tag, so the device is unreachable until a pool plans it, and then only from
//! that pool's coordinator, on the gateway port (ADR 0013, section 13).
//!
//! This is the plan's option A: the installed client, on one tailnet at a
//! time. A machine already on another tailnet is told so rather than switched
//! silently.

use crate::exit::{Code, WithCode};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Status {
    /// `Running`, `NeedsLogin`, `Stopped`, `Starting`.
    pub backend_state: String,
    pub ipv4: Option<Ipv4Addr>,
    pub tags: Vec<String>,
    pub tailnet: Option<String>,
}

impl Status {
    pub fn running(&self) -> bool {
        self.backend_state == "Running" && self.ipv4.is_some()
    }

    /// The account's compute tag, when this machine carries one.
    pub fn compute_tag(&self) -> Option<&str> {
        self.tags
            .iter()
            .map(String::as_str)
            .find(|t| t.starts_with("tag:c-"))
    }
}

/// `tailscale status --json`, the parts this needs.
pub fn parse_status(json: &str) -> Result<Status> {
    let v: Value = serde_json::from_str(json).context("tailscale status did not return JSON")?;
    let me = &v["Self"];
    let ipv4 = me["TailscaleIPs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|ip| ip.parse::<Ipv4Addr>().ok());
    let tags = me["Tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let tailnet = v["CurrentTailnet"]["Name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(Status {
        backend_state: v["BackendState"].as_str().unwrap_or_default().to_string(),
        ipv4,
        tags,
        tailnet,
    })
}

/// `tailscale whois --json <ip>`: the tags of the node at that address.
pub fn parse_whois_tags(json: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(json).ok()?;
    let node = v.get("Node")?;
    Some(
        node["Tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// The tag hosted nodes of the account carry, from the device's compute tag:
/// `tag:c-acme` → `tag:u-acme`. A pool's coordinator is one of them.
pub fn node_tag_for(compute_tag: &str) -> Option<String> {
    compute_tag
        .strip_prefix("tag:c-")
        .filter(|id| !id.is_empty())
        .map(|id| format!("tag:u-{id}"))
}

/// Find the Tailscale CLI: on the PATH, or inside the macOS app.
pub fn find() -> Option<PathBuf> {
    let candidates = [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/usr/bin/tailscale",
    ];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("tailscale");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

pub fn require() -> Result<PathBuf> {
    find()
        .ok_or_else(|| {
            anyhow!("Tailscale is not installed. Install it from https://tailscale.com/download, then run this again.")
        })
        .coded(Code::Precondition)
}

pub fn status(bin: &Path) -> Result<Status> {
    let out = Command::new(bin)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("Could not run tailscale status")?;
    // `status` exits non-zero when logged out but still prints the JSON.
    parse_status(&String::from_utf8_lossy(&out.stdout))
}

/// Bring Tailscale up on Kovalent's tailnet with a one-time compute key.
///
/// The key goes to Tailscale in a 0600 file, never on a command line where any
/// local process could read it. `--reset` because `tailscale up` refuses to
/// change accounts without restating every non-default flag; the caller has
/// already asked the owner before switching a machine that was on a tailnet.
pub fn up_with_key(bin: &Path, auth_key: &str, hostname: &str) -> Result<()> {
    let dir = crate::device::state::device_dir()?;
    let key_file = dir.join(format!(".tailscale-key.{}", std::process::id()));
    crate::device::state::write_private(&key_file, auth_key.as_bytes())?;
    let mut args: Vec<String> = vec![
        "up".into(),
        format!("--auth-key=file:{}", key_file.display()),
        format!("--hostname={hostname}"),
        "--reset".into(),
        "--timeout=60s".into(),
    ];
    let mut cmd = if needs_sudo() {
        let mut c = Command::new("sudo");
        args.insert(0, bin.display().to_string());
        c.args(&args);
        c
    } else {
        let mut c = Command::new(bin);
        c.args(&args);
        c
    };
    let status = cmd.status();
    let _ = std::fs::remove_file(&key_file);
    let status = status.context("Could not run tailscale up")?;
    if !status.success() {
        return Err(anyhow!(
            "tailscale up did not finish. Check that Tailscale is running, then run this again."
        ))
        .coded(Code::Precondition);
    }
    Ok(())
}

/// On Linux, `tailscale up` needs root unless the user is Tailscale's operator.
fn needs_sudo() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::geteuid() != 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Ask Tailscale which node an address belongs to, and return its tags.
/// None when it cannot say, which the gateway treats as a refusal.
pub async fn whois_tags(bin: &Path, ip: &str) -> Option<Vec<String>> {
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(bin)
            .args(["whois", "--json", ip])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_whois_tags(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"{
      "BackendState": "Running",
      "Self": {"HostName": "studio", "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"], "Tags": ["tag:c-acme"]},
      "CurrentTailnet": {"Name": "kovalent.example", "MagicDNSSuffix": "tail1234.ts.net"}
    }"#;

    #[test]
    fn a_running_compute_device_is_read_off_the_status() {
        let s = parse_status(RUNNING).unwrap();
        assert!(s.running());
        assert_eq!(s.ipv4, Some(Ipv4Addr::new(100, 101, 102, 103)));
        assert_eq!(s.compute_tag(), Some("tag:c-acme"));
        assert_eq!(s.tailnet.as_deref(), Some("kovalent.example"));
    }

    #[test]
    fn a_logged_out_client_is_not_running() {
        let s = parse_status(r#"{"BackendState": "NeedsLogin", "Self": {"TailscaleIPs": null}}"#)
            .unwrap();
        assert!(!s.running());
        assert_eq!(s.compute_tag(), None);
    }

    #[test]
    fn a_user_tagged_machine_has_no_compute_tag() {
        let s = parse_status(r#"{"BackendState": "Running", "Self": {"TailscaleIPs": ["100.1.2.3"], "Tags": ["tag:u-acme"]}}"#).unwrap();
        assert!(s.running());
        assert_eq!(s.compute_tag(), None);
    }

    #[test]
    fn whois_reads_the_peer_tags() {
        let tags = parse_whois_tags(
            r#"{"Node": {"Name": "kv-node", "Tags": ["tag:u-acme"]}, "UserProfile": {}}"#,
        )
        .unwrap();
        assert_eq!(tags, vec!["tag:u-acme".to_string()]);
        // A node owned by a person rather than a tag has none.
        assert_eq!(
            parse_whois_tags(r#"{"Node": {"Name": "laptop"}}"#).unwrap(),
            Vec::<String>::new()
        );
        assert!(parse_whois_tags("{}").is_none());
    }

    #[test]
    fn the_node_tag_follows_from_the_compute_tag() {
        assert_eq!(node_tag_for("tag:c-acme").as_deref(), Some("tag:u-acme"));
        assert_eq!(node_tag_for("tag:u-acme"), None);
        assert_eq!(node_tag_for("tag:c-"), None);
    }
}
