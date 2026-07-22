//! End-to-end checks that environment credentials stay in the environment.
//!
//! `KNAIX_TOKEN` is the documented way to run the CLI headlessly in CI. A
//! command that mutates the config must not fold that token into the saved
//! file, or an ephemeral CI credential ends up on the runner's disk in
//! plaintext, outliving the job that was given it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds an empty HOME for one test, so each run starts with no config and
/// nothing touches the developer's real `~/.knaix`.
fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-test-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("could not create scratch home");
    dir
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home)
        // Keep the test off the network and away from the release endpoint.
        .env("KNAIX_NO_UPDATE_CHECK", "1");
    cmd
}

fn stored_config(home: &Path) -> String {
    fs::read_to_string(home.join(".knaix").join("config.json"))
        .expect("expected the command to have written a config file")
}

#[test]
fn env_token_is_not_written_to_the_config_file() {
    let home = scratch_home("token");

    let status = knaix(&home)
        .env("KNAIX_TOKEN", "ephemeral-ci-token-must-not-persist")
        .args(["use", "some-node"])
        .status()
        .expect("failed to run knaix");
    assert!(status.success(), "knaix use should succeed");

    let saved = stored_config(&home);
    assert!(
        !saved.contains("ephemeral-ci-token-must-not-persist"),
        "KNAIX_TOKEN leaked into the saved config:\n{saved}"
    );
    assert!(
        saved.contains("some-node"),
        "the command should still have saved the default node:\n{saved}"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn env_api_url_is_not_written_to_the_config_file() {
    let home = scratch_home("apiurl");

    let status = knaix(&home)
        .env("KNAIX_API_URL", "https://override.example.invalid")
        .args(["use", "some-node"])
        .status()
        .expect("failed to run knaix");
    assert!(status.success(), "knaix use should succeed");

    let saved = stored_config(&home);
    assert!(
        !saved.contains("override.example.invalid"),
        "KNAIX_API_URL leaked into the saved config:\n{saved}"
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn env_overrides_still_apply_to_the_running_command() {
    let home = scratch_home("effective");

    // status reports the effective config, so the override must show up there
    // even though it is never saved.
    let out = knaix(&home)
        .env("KNAIX_API_URL", "https://override.example.invalid")
        .args(["-o", "json", "status"])
        .output()
        .expect("failed to run knaix");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("override.example.invalid"),
        "the override should apply to the running command:\n{stdout}"
    );

    let _ = fs::remove_dir_all(&home);
}
