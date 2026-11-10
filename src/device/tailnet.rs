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
    find_in(std::env::var_os("PATH"), &candidates)
}

fn find_in(path: Option<std::ffi::OsString>, candidates: &[&str]) -> Option<PathBuf> {
    path.iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("tailscale"))
        .chain(candidates.iter().map(PathBuf::from))
        .find(|p| p.is_file())
}

pub fn require() -> Result<PathBuf> {
    installed(find())
}

fn installed(found: Option<PathBuf>) -> Result<PathBuf> {
    found
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
    up_with_key_as(needs_sudo().then_some("sudo"), bin, auth_key, hostname)
}

/// `up_with_key`, run through `elevate` (sudo) when it is given.
fn up_with_key_as(elevate: Option<&str>, bin: &Path, auth_key: &str, hostname: &str) -> Result<()> {
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
    let mut cmd = if let Some(elevate) = elevate {
        let mut c = Command::new(elevate);
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

    use crate::config::ScratchHome;
    use crate::exit::code_of;

    #[cfg(unix)]
    use crate::testing::{args_of, fake_tool};

    #[test]
    fn the_cli_is_found_on_the_path_before_the_usual_places() {
        let home = ScratchHome::new();
        let (on_path, app) = (home.0.join("bin"), home.0.join("app/Tailscale"));
        std::fs::create_dir_all(&on_path).unwrap();
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(on_path.join("tailscale"), "").unwrap();
        std::fs::write(&app, "").unwrap();
        let path = Some(std::env::join_paths([&home.0, &on_path]).unwrap());
        let app_str = app.to_str().unwrap();

        assert_eq!(find_in(path, &[app_str]), Some(on_path.join("tailscale")));
        assert_eq!(
            find_in(None, &["/no/such/tailscale", app_str]),
            Some(app.clone())
        );
        assert_eq!(find_in(None, &["/no/such/tailscale"]), None);
        // This machine's answer, whichever it is, agrees with `require`.
        assert_eq!(find().is_some(), require().is_ok());
    }

    #[test]
    fn a_machine_without_tailscale_is_told_where_to_get_it() {
        let err = installed(None).unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert!(
            err.to_string().contains("https://tailscale.com/download"),
            "{err}"
        );
        assert_eq!(installed(Some("/t".into())).unwrap(), PathBuf::from("/t"));
    }

    #[cfg(unix)]
    #[test]
    fn status_reads_the_client_even_when_it_exits_non_zero_logged_out() {
        let home = ScratchHome::new();
        let ts = fake_tool(
            &home.0,
            "tailscale",
            r#"echo '{"BackendState":"NeedsLogin","Self":{"TailscaleIPs":[]}}'; exit 1"#,
        );
        let s = status(Path::new(&ts)).unwrap();
        assert_eq!(
            (s.backend_state.as_str(), s.running()),
            ("NeedsLogin", false)
        );
        assert_eq!(args_of(&ts), ["status --json"]);

        let err = status(&home.0.join("missing")).unwrap_err();
        assert_eq!(err.to_string(), "Could not run tailscale status");
    }

    /// A tailscale that keeps a copy of the key file it was pointed at, and
    /// that file's permissions, beside itself.
    #[cfg(unix)]
    fn key_reading_tailscale(home: &Path, name: &str, exit: i32) -> String {
        fake_tool(
            home,
            name,
            &format!(
                r#"f=$(echo "$@" | sed -n 's/.*--auth-key=file:\([^ ]*\).*/\1/p')
cat "$f" > "$0.key"; ls -l "$f" | cut -c1-10 > "$0.mode"; exit {exit}"#
            ),
        )
    }

    #[cfg(unix)]
    #[test]
    fn the_join_key_reaches_tailscale_only_in_a_private_file_that_is_then_deleted() {
        let home = ScratchHome::new();
        let ts = key_reading_tailscale(&home.0, "tailscale", 0);

        up_with_key_as(None, Path::new(&ts), "tskey-secret", "studio").unwrap();

        let args = args_of(&ts).concat();
        let key_file = crate::device::state::device_dir()
            .unwrap()
            .join(format!(".tailscale-key.{}", std::process::id()));
        assert_eq!(
            args,
            format!(
                "up --auth-key=file:{} --hostname=studio --reset --timeout=60s",
                key_file.display()
            )
        );
        assert!(
            !args.contains("tskey-secret"),
            "the key was on the command line"
        );
        let read = |ext: &str| std::fs::read_to_string(format!("{ts}.{ext}")).unwrap();
        assert_eq!(read("key"), "tskey-secret");
        assert_eq!(read("mode").trim(), "-rw-------");
        assert!(!key_file.exists(), "the key file outlived the call");
    }

    #[cfg(unix)]
    #[test]
    fn tailscale_up_runs_under_sudo_when_asked_and_a_failure_still_deletes_the_key() {
        let home = ScratchHome::new();
        let ts = key_reading_tailscale(&home.0, "tailscale", 1);
        let sudo = fake_tool(&home.0, "sudo", r#"exec "$@""#);
        let key_file = crate::device::state::device_dir()
            .unwrap()
            .join(format!(".tailscale-key.{}", std::process::id()));

        let err =
            up_with_key_as(Some(&sudo), Path::new(&ts), "tskey-secret", "studio").unwrap_err();

        assert_eq!(code_of(&err), Code::Precondition);
        assert!(
            err.to_string().starts_with("tailscale up did not finish."),
            "{err}"
        );
        assert!(args_of(&sudo)[0].starts_with(&format!("{ts} up --auth-key=file:")));
        assert_eq!(
            std::fs::read_to_string(format!("{ts}.key")).unwrap(),
            "tskey-secret"
        );
        assert!(!key_file.exists());

        let err = up_with_key_as(None, &home.0.join("missing"), "k", "h").unwrap_err();
        assert_eq!(err.to_string(), "Could not run tailscale up");
        assert!(!key_file.exists());
    }

    #[test]
    fn only_linux_needs_root_to_bring_tailscale_up() {
        #[cfg(target_os = "linux")]
        assert_eq!(needs_sudo(), unsafe { libc::geteuid() } != 0);
        #[cfg(not(target_os = "linux"))]
        assert!(!needs_sudo());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn whois_names_the_peers_tags_or_nothing_when_it_cannot_say() {
        let home = ScratchHome::new();
        let ts = fake_tool(
            &home.0,
            "tailscale",
            r#"echo '{"Node":{"Tags":["tag:u-acme"]}}'"#,
        );
        assert_eq!(
            whois_tags(Path::new(&ts), "100.64.0.7").await,
            Some(vec!["tag:u-acme".to_string()])
        );
        assert_eq!(args_of(&ts), ["whois --json 100.64.0.7"]);

        let refusing = fake_tool(&home.0, "tailscale-no", "exit 1");
        assert_eq!(whois_tags(Path::new(&refusing), "100.64.0.7").await, None);
        assert_eq!(
            whois_tags(&home.0.join("missing"), "100.64.0.7").await,
            None
        );
    }
}
