//! `--daemon`: a user service that runs `knaix device run`, so the device
//! survives logouts, sleeps and reboots. A launchd agent on macOS, a systemd
//! user unit on Linux. Both restart the process when it exits, which is also
//! how a changed tailnet address or a refused binary gets a clean retry.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::exit::{Code, WithCode};

pub const LABEL: &str = "com.kovalent.knaix-device";
pub const UNIT: &str = "knaix-device.service";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn launchd_plist(exe: &Path, log: &Path) -> String {
    let exe = xml_escape(&exe.display().to_string());
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>device</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

pub fn systemd_unit(exe: &Path) -> String {
    // systemd splits ExecStart on spaces; quote the path.
    let exe = exe.display().to_string().replace('"', "\\\"");
    format!(
        r#"[Unit]
Description=Kovalent compute device (knaix device run)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart="{exe}" device run
Restart=always
RestartSec=10
# The device process starts rpc-server in its own sandboxed transient unit;
# this one only needs to reach the tailnet and the control plane.
NoNewPrivileges=yes

[Install]
WantedBy=default.target
"#
    )
}

fn plist_path() -> Result<PathBuf> {
    let home = home::home_dir().context("Could not find the home directory")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn unit_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home::home_dir().map(|h| h.join(".config")))
        .context("Could not find the config directory")?;
    Ok(base.join("systemd/user").join(UNIT))
}

/// The binary the service should start. A package manager's stable link on
/// the PATH (Homebrew's `/opt/homebrew/bin/knaix`) beats the versioned path
/// the running process resolves to, which the next upgrade deletes.
pub fn service_exe() -> Result<PathBuf> {
    let current = std::env::current_exe().context("Could not find the knaix binary")?;
    let canonical = std::fs::canonicalize(&current).unwrap_or(current.clone());
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("knaix");
            if std::fs::canonicalize(&candidate).ok().as_ref() == Some(&canonical) {
                return Ok(candidate);
            }
        }
    }
    Ok(current)
}

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn uid() -> u32 {
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub fn install() -> Result<PathBuf> {
    let exe = service_exe()?;
    if cfg!(target_os = "macos") {
        {
            let path = plist_path()?;
            let log = crate::device::state::service_log_path()?;
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, launchd_plist(&exe, &log))?;
            let domain = format!("gui/{}", uid());
            // Replace a loaded copy, so a changed binary path takes effect.
            quiet(Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]));
            let ok = quiet(Command::new("launchctl").args([
                "bootstrap",
                &domain,
                &path.display().to_string(),
            ]));
            if !ok {
                return Err(anyhow!(
                    "launchd would not load {}. Run `launchctl bootstrap {domain} {}` to see why.",
                    path.display(),
                    path.display()
                ))
                .coded(Code::Precondition);
            }
            return Ok(path);
        }
    }
    if cfg!(target_os = "linux") {
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, systemd_unit(&exe))?;
        quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
        let ok = quiet(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]));
        if !ok {
            return Err(anyhow!(
                "systemd would not start {UNIT}. Run `systemctl --user status {UNIT}` to see why."
            ))
            .coded(Code::Precondition);
        }
        return Ok(path);
    }
    Err(anyhow!("--daemon is only supported on macOS and Linux.")).coded(Code::Precondition)
}

/// Stop and remove the service. Missing is fine: leaving wants it gone.
pub fn uninstall() -> Result<bool> {
    if cfg!(target_os = "macos") {
        {
            let path = plist_path()?;
            quiet(Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]));
            if path.exists() {
                std::fs::remove_file(&path)?;
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if cfg!(target_os = "linux") {
        let path = unit_path()?;
        quiet(Command::new("systemctl").args(["--user", "disable", "--now", UNIT]));
        if path.exists() {
            std::fs::remove_file(&path)?;
            quiet(Command::new("systemctl").args(["--user", "daemon-reload"]));
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn installed() -> bool {
    if cfg!(target_os = "macos") {
        return plist_path().map(|p| p.exists()).unwrap_or(false);
    }
    if cfg!(target_os = "linux") {
        return unit_path().map(|p| p.exists()).unwrap_or(false);
    }
    false
}

/// Restart the service so it picks up a changed key or record.
pub fn restart() -> bool {
    if cfg!(target_os = "macos") {
        return quiet(Command::new("launchctl").args([
            "kickstart",
            "-k",
            &format!("gui/{}/{LABEL}", uid()),
        ]));
    }
    if cfg!(target_os = "linux") {
        return quiet(Command::new("systemctl").args(["--user", "restart", UNIT]));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_runs_the_device_and_keeps_it_alive() {
        let p = launchd_plist(
            Path::new("/opt/homebrew/bin/knaix"),
            Path::new("/Users/m/.knaix/device/service.log"),
        );
        assert!(p.contains("<string>/opt/homebrew/bin/knaix</string>\n    <string>device</string>\n    <string>run</string>"));
        assert!(p.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(p.contains(LABEL));
    }

    #[test]
    fn paths_are_escaped_in_the_plist() {
        let p = launchd_plist(Path::new("/Users/a&b/<knaix>"), Path::new("/l"));
        assert!(p.contains("/Users/a&amp;b/&lt;knaix&gt;"));
    }

    #[test]
    fn the_unit_restarts_and_quotes_the_binary() {
        let u = systemd_unit(Path::new("/home/m/my tools/knaix"));
        assert!(u.contains(r#"ExecStart="/home/m/my tools/knaix" device run"#));
        assert!(u.contains("Restart=always"));
        assert!(u.contains("WantedBy=default.target"));
    }
}
