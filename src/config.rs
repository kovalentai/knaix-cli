use anyhow::{Context, Result};
use colored::Colorize;
use home::home_dir;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Config {
    pub token: Option<String>,
    pub username: Option<String>,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    pub default_node_id: Option<String>,
    #[serde(default)]
    pub last_update_check: Option<u64>,
    #[serde(default)]
    pub latest_known_version: Option<String>,
}

fn default_api_url() -> String {
    "https://api.kovalentai.com".to_string()
}

/// Returns the full path to the .knaix/config.json file.
pub fn get_config_path() -> PathBuf {
    let mut path = home_dir().expect("Could not find home directory");
    path.push(".knaix");
    if !path.exists() {
        fs::create_dir_all(&path).expect("Could not create config directory");
    }
    path.push("config.json");
    path
}

/// Loads the config exactly as stored on disk, with no environment overrides
/// applied.
///
/// Anything that mutates and then saves the config must start here. Starting
/// from `load_config` would fold an ephemeral `KNAIX_TOKEN` or `KNAIX_API_URL`
/// into the saved file, writing a credential the caller only meant to use for
/// the current process to disk in plaintext.
pub fn load_stored_config() -> Config {
    let path = get_config_path();
    if path.exists() {
        let content = fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
        serde_json::from_str(&content).unwrap_or_else(|_| Config::default())
    } else {
        Config::default()
    }
}

/// Loads the effective config: what is on disk, with environment overrides
/// applied on top. Use this for reading; never save the result.
pub fn load_config() -> Config {
    let mut config = load_stored_config();

    if let Ok(env_url) = env::var("KNAIX_API_URL") {
        config.api_url = env_url;
    }
    if let Ok(env_token) = env::var("KNAIX_TOKEN") {
        config.token = Some(env_token);
    }

    config
}

/// Saves the config to disk atomically using the Temporary File + Rename pattern.
/// Also hardens filesystem permissions to 0o600 on Unix to protect secrets.
pub fn save_config(config: &Config) -> Result<()> {
    let target_path = get_config_path();
    let parent_dir = target_path
        .parent()
        .context("Config path has no parent directory")?;

    // 1. Create a hidden temporary file in the same directory as the target config.
    // This ensures they are on the same filesystem, which is required for atomic rename.
    let mut temp_path = PathBuf::from(parent_dir);
    temp_path.push(".config.json.tmp");

    // 2. Serialize and write the config to the temporary file.
    let content = serde_json::to_string_pretty(config).context("Could not serialize config")?;

    {
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("Could not create temp config at {}", temp_path.display()))?;

        // Apply 0o600 permissions immediately on creation if possible
        #[cfg(unix)]
        {
            let mut perms = file.metadata()?.permissions();
            perms.set_mode(0o600);
            file.set_permissions(perms).ok();
        }

        file.write_all(content.as_bytes())
            .context("Failed to write data to temp config")?;
        file.sync_all().context("Failed to flush config to disk")?;
    }

    // 3. Hardening: Ensure file and parent dir permissions are correct if not already set.
    #[cfg(unix)]
    {
        if let Ok(metadata) = fs::metadata(&temp_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            if let Err(e) = fs::set_permissions(&temp_path, perms) {
                eprintln!(
                    "{} Failed to harden config permissions: {}",
                    "Warning:".yellow(),
                    e
                );
            }
        }
    }

    // 4. Atomic Rename: Replace the old config with the new one.
    // This is an atomic operation on most filesystems; prevents corruption.
    fs::rename(&temp_path, &target_path).with_context(|| {
        format!(
            "Failed to move {} to {}",
            temp_path.display(),
            target_path.display()
        )
    })?;

    Ok(())
}
