use crate::config;
use colored::*;
use reqwest::Client;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RELEASE_URL: &str = "https://releases.knaix.com/latest-version";
const CHECK_INTERVAL_SECS: u64 = 86400; // 24 hours

pub async fn check_for_update_async() {
    let mut config = config::load_config();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_secs();

    let should_check = match config.last_update_check {
        Some(last_check) => now.saturating_sub(last_check) >= CHECK_INTERVAL_SECS,
        None => true,
    };

    if should_check {
        if let Some(latest) = fetch_latest_version().await {
            config.last_update_check = Some(now);
            config.latest_known_version = Some(latest.clone());
            let _ = config::save_config(&config);
        }
    }
}

async fn fetch_latest_version() -> Option<String> {
    let client = Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .ok()?;

    if let Ok(response) = client.get(RELEASE_URL).send().await {
        if response.status().is_success() {
            if let Ok(latest_version) = response.text().await {
                let latest_version = latest_version.trim().to_string();
                if !latest_version.is_empty() {
                    return Some(latest_version);
                }
            }
        }
    }
    None
}

pub fn print_update_banner() {
    let config = config::load_config();
    if let Some(latest) = config.latest_known_version {
        let current = env!("CARGO_PKG_VERSION");

        let parse = |v: &str| -> Vec<u32> {
            v.split('.')
                .map(|s| s.parse::<u32>().unwrap_or(0))
                .collect()
        };

        let current_parts = parse(current);
        let latest_parts = parse(&latest);
        let mut is_newer = false;

        for i in 0..std::cmp::max(current_parts.len(), latest_parts.len()) {
            let c = current_parts.get(i).unwrap_or(&0);
            let l = latest_parts.get(i).unwrap_or(&0);
            if l > c {
                is_newer = true;
                break;
            } else if l < c {
                break;
            }
        }

        if is_newer {
            let border = "─".repeat(50);
            println!("\n{}", border.dimmed());
            println!(
                "{} {} (current: {})",
                "Info: A new version of Knaix CLI is available:".blue(),
                latest.green().bold(),
                current.dimmed()
            );
            println!(
                "Run: {}",
                "curl -sSL https://knaix.com/install.sh | sh".cyan()
            );
            println!("{}\n", border.dimmed());
        }
    }
}
