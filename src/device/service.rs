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
    let home = crate::config::home().context("Could not find the home directory")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn unit_path() -> Result<PathBuf> {
    // A test's unit goes under its scratch home, never the real config directory.
    #[cfg(test)]
    if crate::config::in_scratch_home() {
        return unit_path_under(None);
    }
    unit_path_under(std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
}

fn unit_path_under(config: Option<PathBuf>) -> Result<PathBuf> {
    let base = config
        .or_else(|| crate::config::home().map(|h| h.join(".config")))
        .context("Could not find the config directory")?;
    Ok(base.join("systemd/user").join(UNIT))
}

/// The binary the service should start. A package manager's stable link on
/// the PATH (Homebrew's `/opt/homebrew/bin/knaix`) beats the versioned path
/// the running process resolves to, which the next upgrade deletes.
pub fn service_exe() -> Result<PathBuf> {
    let current = std::env::current_exe().context("Could not find the knaix binary")?;
    Ok(stable_link(current, std::env::var_os("PATH")))
}

/// The first `knaix` on `path` that is the same file as `current`, else `current`.
fn stable_link(current: PathBuf, path: Option<std::ffi::OsString>) -> PathBuf {
    let canonical = std::fs::canonicalize(&current).unwrap_or(current.clone());
    path.iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("knaix"))
        .find(|candidate| std::fs::canonicalize(candidate).ok().as_ref() == Some(&canonical))
        .unwrap_or(current)
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
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// The service manager for one OS, by the program that drives it: launchd's
/// `launchctl` or systemd's `systemctl`. Tests drive a stand-in.
pub struct Manager<'a> {
    pub os: &'a str,
    pub program: &'a str,
}

/// This machine's.
pub fn system() -> Manager<'static> {
    manager_for(std::env::consts::OS)
}

fn manager_for(os: &str) -> Manager<'_> {
    let program = match os {
        "macos" => "launchctl",
        _ => "systemctl",
    };
    Manager { os, program }
}

pub fn uninstall() -> Result<bool> {
    system().uninstall()
}

pub fn installed() -> bool {
    system().installed()
}

pub fn restart() -> bool {
    system().restart()
}

impl Manager<'_> {
    fn run(&self, args: &[&str]) -> bool {
        quiet(Command::new(self.program).args(args))
    }

    pub fn install(&self) -> Result<PathBuf> {
        let exe = service_exe()?;
        match self.os {
            "macos" => {
                let path = plist_path()?;
                let log = crate::device::state::service_log_path()?;
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(&path, launchd_plist(&exe, &log))?;
                let domain = format!("gui/{}", uid());
                // Replace a loaded copy, so a changed binary path takes effect.
                self.run(&["bootout", &format!("{domain}/{LABEL}")]);
                if !self.run(&["bootstrap", &domain, &path.display().to_string()]) {
                    return Err(anyhow!(
                        "launchd would not load {}. Run `launchctl bootstrap {domain} {}` to see why.",
                        path.display(),
                        path.display()
                    ))
                    .coded(Code::Precondition);
                }
                Ok(path)
            }
            "linux" => {
                let path = unit_path()?;
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(&path, systemd_unit(&exe))?;
                self.run(&["--user", "daemon-reload"]);
                if !self.run(&["--user", "enable", "--now", UNIT]) {
                    return Err(anyhow!(
                        "systemd would not start {UNIT}. Run `systemctl --user status {UNIT}` to see why."
                    ))
                    .coded(Code::Precondition);
                }
                Ok(path)
            }
            _ => Err(anyhow!("--daemon is only supported on macOS and Linux."))
                .coded(Code::Precondition),
        }
    }

    /// Stop and remove the service. Missing is fine: leaving wants it gone.
    pub fn uninstall(&self) -> Result<bool> {
        let path = match self.os {
            "macos" => {
                self.run(&["bootout", &format!("gui/{}/{LABEL}", uid())]);
                plist_path()?
            }
            "linux" => {
                self.run(&["--user", "disable", "--now", UNIT]);
                unit_path()?
            }
            _ => return Ok(false),
        };
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path)?;
        if self.os == "linux" {
            self.run(&["--user", "daemon-reload"]);
        }
        Ok(true)
    }

    pub fn installed(&self) -> bool {
        match self.os {
            "macos" => plist_path(),
            "linux" => unit_path(),
            _ => return false,
        }
        .is_ok_and(|p| p.exists())
    }

    /// Restart the service so it picks up a changed key or record.
    pub fn restart(&self) -> bool {
        match self.os {
            "macos" => self.run(&["kickstart", "-k", &format!("gui/{}/{LABEL}", uid())]),
            "linux" => self.run(&["--user", "restart", UNIT]),
            _ => false,
        }
    }
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

    use crate::config::ScratchHome;
    use crate::exit::{code_of, Code};

    #[cfg(unix)]
    use crate::testing::{args_of, fake_tool};

    #[cfg(unix)]
    fn manager<'a>(os: &'a str, program: &'a str) -> Manager<'a> {
        Manager { os, program }
    }

    #[cfg(unix)]
    #[test]
    fn on_macos_the_agent_is_written_loaded_restarted_and_removed() {
        let home = ScratchHome::new();
        let launchctl = fake_tool(&home.0, "launchctl", "exit 0");
        let launchd = manager("macos", &launchctl);
        let domain = format!("gui/{}", uid());

        let plist = launchd.install().unwrap();

        assert_eq!(
            plist,
            home.0.join(format!("Library/LaunchAgents/{LABEL}.plist"))
        );
        let written = std::fs::read_to_string(&plist).unwrap();
        assert!(written.contains(&format!(
            "<string>{}</string>",
            service_exe().unwrap().display()
        )));
        assert!(written.contains(".knaix/device/service.log"), "{written}");
        assert_eq!(
            args_of(&launchctl),
            [
                format!("bootout {domain}/{LABEL}"),
                format!("bootstrap {domain} {}", plist.display())
            ]
        );
        assert!(launchd.installed());

        assert!(launchd.restart());
        assert_eq!(
            args_of(&launchctl)[2],
            format!("kickstart -k {domain}/{LABEL}")
        );

        assert!(launchd.uninstall().unwrap());
        assert_eq!(args_of(&launchctl)[3], format!("bootout {domain}/{LABEL}"));
        assert!(!plist.exists() && !launchd.installed());
        assert!(
            !launchd.uninstall().unwrap(),
            "removing it twice is not a second removal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_agent_launchd_will_not_load_says_how_to_see_why() {
        let home = ScratchHome::new();
        let launchctl = fake_tool(&home.0, "launchctl", "exit 1");
        let err = manager("macos", &launchctl).install().unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert!(
            err.to_string().starts_with("launchd would not load "),
            "{err}"
        );
        assert!(!manager("macos", &launchctl).restart());
    }

    #[cfg(unix)]
    #[test]
    fn on_linux_the_unit_is_written_enabled_restarted_and_removed() {
        let home = ScratchHome::new();
        let systemctl = fake_tool(&home.0, "systemctl", "exit 0");
        let systemd = manager("linux", &systemctl);

        let unit = systemd.install().unwrap();

        assert_eq!(unit, home.0.join(".config/systemd/user").join(UNIT));
        let written = std::fs::read_to_string(&unit).unwrap();
        assert!(written.contains(&format!(
            "ExecStart=\"{}\" device run",
            service_exe().unwrap().display()
        )));
        assert_eq!(
            args_of(&systemctl),
            [
                "--user daemon-reload".to_string(),
                format!("--user enable --now {UNIT}")
            ]
        );
        assert!(systemd.installed());

        assert!(systemd.restart());
        assert!(systemd.uninstall().unwrap());
        assert_eq!(
            args_of(&systemctl)[2..],
            [
                format!("--user restart {UNIT}"),
                format!("--user disable --now {UNIT}"),
                "--user daemon-reload".to_string(),
            ]
        );
        assert!(!unit.exists() && !systemd.installed());
        assert!(!systemd.uninstall().unwrap());

        let failing = fake_tool(&home.0, "systemctl-no", "exit 1");
        let err = manager("linux", &failing).install().unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert_eq!(
            err.to_string(),
            format!(
                "systemd would not start {UNIT}. Run `systemctl --user status {UNIT}` to see why."
            )
        );
    }

    #[test]
    fn elsewhere_there_is_no_service_to_manage() {
        let _home = ScratchHome::new();
        let other = Manager {
            os: "windows",
            program: "none",
        };
        let err = other.install().unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert_eq!(
            err.to_string(),
            "--daemon is only supported on macOS and Linux."
        );
        assert!(!other.uninstall().unwrap());
        assert!(!other.installed());
        assert!(!other.restart());
    }

    #[test]
    fn the_unit_goes_under_the_config_directory_systemd_reads() {
        let home = ScratchHome::new();
        assert_eq!(
            unit_path_under(Some(PathBuf::from("/xdg"))).unwrap(),
            PathBuf::from("/xdg/systemd/user").join(UNIT)
        );
        assert_eq!(
            unit_path_under(None).unwrap(),
            home.0.join(".config/systemd/user").join(UNIT)
        );
    }

    #[test]
    fn each_os_is_managed_by_the_program_it_ships() {
        assert_eq!(manager_for("macos").program, "launchctl");
        assert_eq!(manager_for("linux").program, "systemctl");
        assert_eq!(system().os, std::env::consts::OS);
    }

    #[cfg(unix)]
    #[test]
    fn the_service_starts_the_stable_link_on_the_path_when_there_is_one() {
        let home = ScratchHome::new();
        let versioned = home.0.join("Cellar/knaix/0.7.0/bin/knaix");
        std::fs::create_dir_all(versioned.parent().unwrap()).unwrap();
        std::fs::write(&versioned, "").unwrap();
        let bin = home.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&versioned, bin.join("knaix")).unwrap();
        let path = |dirs: &[&Path]| Some(std::env::join_paths(dirs).unwrap());

        assert_eq!(
            stable_link(versioned.clone(), path(&[&home.0, &bin])),
            bin.join("knaix")
        );
        assert_eq!(stable_link(versioned.clone(), path(&[&home.0])), versioned);
        assert_eq!(stable_link(versioned.clone(), None), versioned);
        assert!(service_exe().unwrap().exists());
    }
}
