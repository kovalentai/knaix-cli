//! `knaix report` -- something you can read before you send it.
//!
//! The bug template asks a reporter to look up their version and their OS, and
//! then asks them not to paste tokens or private hostnames. The first half is
//! work the CLI could do; the second half is a request for care aimed at the
//! person least able to give it, because the output that shows the problem is
//! usually the output with their node's address in it.
//!
//! So this writes the bundle instead, redacted as it is built rather than
//! checked afterwards. See `redact` for why that is an allowlist.
//!
//! **It never uploads the report.** That is the design, not an unimplemented
//! feature. We sell a control plane that architecturally cannot read tenant
//! data; a command that packages up someone's environment and posts it to us on
//! the day they are having a bad time would contradict that in the one moment
//! they are paying closest attention. The bundle is a file. The user reads it,
//! and decides.
//!
//! That is a claim about the bundle, not about the process. Building a report
//! runs the same checks `doctor` runs, so it does reach the control plane and
//! the node to ask how they are. Both sentences have to stay true, and the
//! wording here is deliberately the narrower one.

use crate::exit::{Code, WithCode};
use crate::nodes::KnaixContext;
use crate::redact::{self, Manifest, Removal};
use anyhow::{anyhow, Context, Result};
use colored::*;
use serde::Serialize;

/// Where an issue would be opened. Used only to print a URL for the user to
/// follow; nothing is ever sent to it.
const ISSUES_URL: &str = "https://github.com/kovalentai/knaix-cli/issues/new";

#[derive(Serialize)]
pub struct Bundle {
    /// Bumped when the shape changes, so a reader knows what to expect.
    pub format: u32,
    pub generated_at: u64,
    pub cli: Cli,
    pub machine: Machine,
    pub config: ConfigShape,
    pub checks: Vec<crate::doctor::Check>,
    pub recent_failures: Vec<crate::diagnostics::Entry>,
    pub node_logs: Vec<String>,
    /// What this bundle took out, and why. The point is that a reader does not
    /// have to take the redaction on trust.
    pub redactions: Vec<Removal>,
}

#[derive(Serialize)]
pub struct Cli {
    pub version: String,
    pub target: String,
    /// brew, install script, cargo, or unknown. Which one decides whether an
    /// upgrade problem is ours or Homebrew's.
    pub installed_via: String,
}

#[derive(Serialize)]
pub struct Machine {
    pub os: String,
    pub arch: String,
    pub family: String,
    pub shell: String,
    pub term: String,
    pub color_depth: String,
    /// True when stdout is not a terminal, which changes what the CLI prints
    /// and is a common source of "the output looked wrong".
    pub piped: bool,
}

/// Which settings are present, never what they hold.
#[derive(Serialize)]
pub struct ConfigShape {
    pub token: String,
    pub username: String,
    pub default_node: String,
    pub api_url: String,
    pub project_file: String,
    pub local_node: String,
    pub local_model_server: String,
}

/// How this binary most likely got here.
///
/// Guessed from the path, because nothing records it. Worth having anyway: a
/// stale Homebrew formula and a stale install script are different bugs with
/// the same symptom, and we have now shipped one of each.
fn installed_via() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "unknown".to_string();
    };
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        "brew".to_string()
    } else if path.contains("/.knaix/") || path.contains("/usr/local/bin") {
        "install script".to_string()
    } else if path.contains("/target/") || path.contains("/.cargo/") {
        "cargo".to_string()
    } else {
        "unknown".to_string()
    }
}

/// The name of a variable's value, not the value: shells and terminals are a
/// short known set, and anything unrecognised is reported as set-but-unnamed
/// rather than quoted, since these can carry paths.
fn env_shape(var: &str, known: &[&str]) -> String {
    match std::env::var(var) {
        Ok(v) if v.is_empty() => "unset".to_string(),
        Ok(v) => {
            let leaf = v.rsplit('/').next().unwrap_or(&v).to_string();
            if known.contains(&leaf.as_str()) {
                leaf
            } else {
                "set".to_string()
            }
        }
        Err(_) => "unset".to_string(),
    }
}

fn machine() -> Machine {
    Machine {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        family: std::env::consts::FAMILY.to_string(),
        shell: env_shape("SHELL", &["zsh", "bash", "fish", "sh", "nu"]),
        term: env_shape(
            "TERM_PROGRAM",
            &[
                "iTerm.app",
                "Apple_Terminal",
                "vscode",
                "WarpTerminal",
                "ghostty",
                "Hyper",
                "alacritty",
                "WezTerm",
            ],
        ),
        color_depth: crate::brand::level_name().to_string(),
        piped: !std::io::IsTerminal::is_terminal(&std::io::stdout()),
    }
}

fn config_shape(ctx: &KnaixContext, manifest: &mut Manifest) -> ConfigShape {
    let c = &ctx.config;
    let local = crate::local::load();

    ConfigShape {
        token: redact::secret_shape(c.token.as_deref()),
        username: match c.username.as_deref() {
            Some(u) => {
                manifest.note("config.username", "hashed", "a username names a person");
                redact::hashed(u)
            }
            None => "absent".to_string(),
        },
        default_node: match c.default_node_id.as_deref() {
            Some(n) if n == crate::local::LOCAL_NODE_ID => "local".to_string(),
            Some(n) => {
                manifest.note(
                    "config.default_node_id",
                    "hashed",
                    "a node name can name a company or a project",
                );
                redact::hashed(n)
            }
            None => "absent".to_string(),
        },
        api_url: redact::api_url(&c.api_url, manifest),
        project_file: match crate::project::current() {
            Ok(Some(_)) => "present".to_string(),
            Ok(None) => "absent".to_string(),
            Err(_) => "present, will not parse".to_string(),
        },
        local_node: match &local {
            Some(_) => "recorded".to_string(),
            None => "absent".to_string(),
        },
        local_model_server: match local.as_ref().and_then(|n| n.model_url.as_deref()) {
            Some(url) => redact::model_url(url, manifest),
            None => "none, the mock answers".to_string(),
        },
    }
}

/// Recent lines from the local node, reduced to their skeletons.
///
/// Only the local node: a hosted node's logs come through the control plane and
/// belong to a tenant, and putting them in a file destined for a public issue
/// tracker is not a decision this command should make on anyone's behalf.
fn node_logs(manifest: &mut Manifest) -> Vec<String> {
    crate::local::recent_container_logs(40)
        .into_iter()
        .map(|l| redact::log_line(&l, manifest))
        .filter(|l| !l.is_empty())
        .collect()
}

pub async fn build(ctx: &KnaixContext, node_flag: Option<String>) -> Bundle {
    let mut manifest = Manifest::default();
    let scrubber = redact::Scrubber::for_this_machine();

    let config = config_shape(ctx, &mut manifest);

    // The checks are English with values interpolated, so they go through the
    // scrubber. Redacting the config and trusting these leaked a node name on
    // the first real run.
    let checks: Vec<crate::doctor::Check> = crate::doctor::collect(ctx, node_flag)
        .await
        .into_iter()
        .map(|mut c| {
            c.detail = scrubber.scrub(&c.detail, "check detail", &mut manifest);
            c.remedy = c
                .remedy
                .map(|r| scrubber.scrub(&r, "check remedy", &mut manifest));
            c
        })
        .collect();

    let node_logs = node_logs(&mut manifest);

    let recent_failures = crate::diagnostics::recent();
    if !recent_failures.is_empty() {
        manifest.note(
            "recent failures",
            "reduced",
            "recorded already redacted; arguments and messages were removed when they happened",
        );
    }

    Bundle {
        format: 1,
        generated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        cli: Cli {
            version: env!("CARGO_PKG_VERSION").to_string(),
            target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            installed_via: installed_via(),
        },
        machine: machine(),
        config,
        checks,
        recent_failures,
        node_logs,
        redactions: manifest.removals().to_vec(),
    }
}

/// Write a file only the user can read.
///
/// A report describes their machine, so it gets the same 0600 the saved session
/// gets. Created with the mode set rather than chmod'd afterwards: the gap
/// between `create` and `set_permissions` is a window where the file exists at
/// whatever the umask allows, and on a shared machine that is the whole
/// exposure.
fn write_private(path: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

pub async fn run(
    ctx: &KnaixContext,
    node_flag: Option<String>,
    out: Option<String>,
    open: bool,
) -> Result<()> {
    let bundle = build(ctx, node_flag).await;
    let json = serde_json::to_string_pretty(&bundle)?;

    // `-o json` means the caller wants the bundle itself, not a file and a
    // summary. Nothing else is printed, so it pipes.
    if ctx.output_format == "json" {
        println!("{json}");
        return Ok(());
    }

    let path = out.unwrap_or_else(|| format!("knaix-report-{}.json", bundle.generated_at));

    // Refuse rather than overwrite. `knaix init` already takes this line with
    // .knaix.toml, and a report is written with a path the user typed, which is
    // exactly where a typo lands on a file they wanted.
    if std::path::Path::new(&path).exists() {
        return Err(anyhow!(
            "{} already exists. Move it, or pass --out with a different path.",
            path
        ))
        .coded(Code::Precondition);
    }

    write_private(&path, &json)
        .with_context(|| format!("Could not write the report to {path}"))
        .coded(Code::Error)?;

    print_summary(&bundle, &path);

    if open {
        let url = issue_url(&bundle);
        println!(
            "\n  Opening a new issue. {}",
            "Attach the file above; it is not sent for you.".dimmed()
        );
        if let Err(e) = open::that(&url) {
            println!("  {} {e}", "Could not open a browser:".yellow());
            println!("  {url}");
        }
    }

    Ok(())
}

/// A prefilled issue, with the environment section filled and nothing else.
///
/// The bundle is attached by hand on purpose. A URL is the wrong place for
/// diagnostic data: it is length-limited, it lands in browser history, and it
/// reaches the server as a query string whatever the page then does with it.
fn issue_url(bundle: &Bundle) -> String {
    let body = format!(
        "## What happened\n\n<!-- describe it here -->\n\n\
         ## Environment\n\n\
         - knaix: {}\n- installed via: {}\n- os: {} {}\n- shell: {}\n\n\
         ## Diagnostics\n\n\
         <!-- attach the knaix-report-*.json file; drag it into this box -->\n",
        bundle.cli.version,
        bundle.cli.installed_via,
        bundle.machine.os,
        bundle.machine.arch,
        bundle.machine.shell,
    );
    format!("{ISSUES_URL}?labels=bug&body={}", urlencode(&body))
}

/// Percent-encode for a query string. Small enough to do here rather than take
/// a dependency for one call site.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn print_summary(bundle: &Bundle, path: &str) {
    println!("\n{}", "Report written:".bold().underline());
    println!("  {}", path.cyan());

    let failed = bundle
        .checks
        .iter()
        .filter(|c| !matches!(c.health, crate::doctor::Health::Ok))
        .count();
    println!(
        "\n  {} {} checks, {} not ok, {} recent failure(s), {} log line(s)",
        "Contains:".dimmed(),
        bundle.checks.len(),
        failed,
        bundle.recent_failures.len(),
        bundle.node_logs.len(),
    );

    println!("\n{}", "What was left out:".bold());
    if bundle.redactions.is_empty() {
        println!("  {}", "nothing sensitive was found to remove".dimmed());
    } else {
        for r in &bundle.redactions {
            println!(
                "  {} {} {}",
                format!("{}:", r.field).dimmed(),
                r.action,
                format!("({})", r.reason).dimmed()
            );
        }
    }

    println!(
        "\n  {} Your token is never included. Read the file before you share it.",
        "Note:".blue()
    );
    // Precise about which claim is being made. The report is never uploaded,
    // which is the promise. Saying "no request to Kovalent" overstated it:
    // building the report runs the same checks as doctor, and those do reach
    // the control plane. A privacy claim that is a little bit false is worse
    // than no claim, because it is the one thing a reader will test.
    println!(
        "  {} The report is never uploaded. Building it runs the same checks as {},",
        "Note:".blue(),
        crate::brand::cmd("doctor")
    );
    println!("        so it does contact your node and control plane to ask how they are.\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_issue_url_carries_no_diagnostics() {
        let bundle = Bundle {
            format: 1,
            generated_at: 0,
            cli: Cli {
                version: "0.4.6".into(),
                target: "aarch64-macos".into(),
                installed_via: "brew".into(),
            },
            machine: Machine {
                os: "macos".into(),
                arch: "aarch64".into(),
                family: "unix".into(),
                shell: "zsh".into(),
                term: "ghostty".into(),
                color_depth: "truecolor".into(),
                piped: false,
            },
            config: ConfigShape {
                token: "present, 328 chars".into(),
                username: "id:abc123".into(),
                default_node: "id:def456".into(),
                api_url: "https://api.kovalentai.com".into(),
                project_file: "absent".into(),
                local_node: "recorded".into(),
                local_model_server: "http://<removed>:11434".into(),
            },
            checks: vec![],
            recent_failures: vec![],
            node_logs: vec![],
            redactions: vec![],
        };

        let url = issue_url(&bundle);
        // The environment is fine to prefill; it names nothing.
        assert!(url.contains("0.4.6"));
        assert!(url.contains("brew"));
        // Nothing that identifies the user goes in a query string, not even the
        // hashes, which are still identifiers.
        assert!(!url.contains("abc123"), "a hashed id reached the URL");
        assert!(!url.contains("def456"), "a hashed id reached the URL");
        assert!(!url.contains("328"), "the token shape reached the URL");
    }

    #[test]
    fn urlencoding_escapes_what_a_query_string_cannot_carry() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("plain-Text_1.0~"), "plain-Text_1.0~");
        assert_eq!(urlencode("#"), "%23");
    }
}
