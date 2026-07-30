use crate::config;
use colored::*;
use reqwest::Client;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RELEASE_URL: &str = "https://releases.knaix.com/latest-version";
const CHECK_INTERVAL_SECS: u64 = 86400; // 24 hours

/// Whether the user has opted out of the daily version check.
///
/// The check is the only network request the CLI makes on its own behalf, so
/// it needs a way to turn off for air-gapped installs, CI, and anyone who
/// simply does not want it.
fn update_check_disabled() -> bool {
    matches!(
        std::env::var("KNAIX_NO_UPDATE_CHECK").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub async fn check_for_update_async() {
    if update_check_disabled() {
        return;
    }

    let config = config::load_stored_config();
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
            // Re-read before writing. The command running alongside this check
            // may have saved a token or a default node while the request was in
            // flight, and writing the copy loaded above would undo it.
            let mut config = config::load_stored_config();
            config.last_update_check = Some(now);
            config.latest_known_version = Some(latest);
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

/// Whether `b` is a later release than `a`, comparing dotted parts left to
/// right. A part that will not parse counts as 0, so a version the release feed
/// invents cannot be read as newer than one it did not.
fn is_newer(current: &str, candidate: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> {
        v.split('.')
            .map(|s| s.parse::<u32>().unwrap_or(0))
            .collect()
    };

    let current = parse(current);
    let candidate = parse(candidate);

    for i in 0..std::cmp::max(current.len(), candidate.len()) {
        let c = current.get(i).unwrap_or(&0);
        let l = candidate.get(i).unwrap_or(&0);
        if l > c {
            return true;
        } else if l < c {
            return false;
        }
    }
    false
}

/// The newer release the last check found, if there is one.
///
/// Reads what the background check already cached rather than asking the
/// release feed again: this is on the path of `knaix doctor`, and a diagnosis
/// that hangs on a network call is a diagnosis nobody waits for.
pub fn newer_version_available() -> Option<String> {
    if update_check_disabled() {
        return None;
    }
    let latest = config::load_stored_config().latest_known_version?;
    is_newer(env!("CARGO_PKG_VERSION"), &latest).then_some(latest)
}

pub fn print_update_banner() {
    let Some(latest) = newer_version_available() else {
        return;
    };

    let current = env!("CARGO_PKG_VERSION");
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

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn a_later_release_is_newer() {
        assert!(is_newer("0.4.5", "0.4.6"));
        assert!(is_newer("0.4.5", "0.5.0"));
        assert!(is_newer("0.9.9", "1.0.0"));
    }

    #[test]
    fn the_same_or_older_is_not() {
        assert!(!is_newer("0.4.5", "0.4.5"));
        assert!(!is_newer("0.4.5", "0.4.4"));
        // A build ahead of the feed, which happens to anyone running from
        // source. Telling them to downgrade would be worse than saying nothing.
        assert!(!is_newer("0.5.0", "0.4.9"));
    }

    /// The major number decides even when the minor is smaller, which the
    /// digit-by-digit string comparison this replaced got wrong.
    #[test]
    fn a_leading_part_outranks_the_rest() {
        assert!(is_newer("0.10.0", "1.2.0"));
        assert!(!is_newer("1.2.0", "0.10.0"));
    }
}
