//! `knaix doctor` -- one command that says why nothing works.
//!
//! Every other command fails at the first thing that is wrong and stops, so
//! diagnosing a broken setup means running four commands and assembling the
//! answer yourself. This runs every check, reports all of them, and says what
//! to do about each one it did not like.
//!
//! The exit code follows one rule: doctor fails when something on the path to
//! your node is broken, and warns about anything that is not on it. The path is
//! everything a command traverses to reach the node it addresses -- the project
//! file and the API URL that decide where it goes, the control plane and the
//! session that authorize it, and the node itself. What is off that path is a
//! warning: a machine with no Docker is fine if the default node is hosted, and
//! an unreachable control plane is fine if the default node is local. Reporting
//! either as a failure would make the command useless to half its users.
//!
//! The first failure supplies the code, and the checks run in path order, so a
//! script reads the earliest broken thing rather than a later symptom of it.

use crate::exit::{Code, WithCode};
use crate::nodes::{KnaixContext, Target};
use anyhow::{anyhow, Result};
use colored::*;
use reqwest::header::AUTHORIZATION;
use serde::Serialize;
use std::time::{Duration, Instant};

/// How a single check came out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Health {
    Ok,
    /// Something to know about that does not stop the node from answering.
    Warn,
    /// Broken, with the code the command exits with.
    Fail(Code),
}

impl Health {
    fn label(self) -> String {
        match self {
            Health::Ok => "ok".green().to_string(),
            Health::Warn => "warn".yellow().to_string(),
            Health::Fail(_) => "fail".red().to_string(),
        }
    }

    fn word(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Warn => "warn",
            Health::Fail(_) => "fail",
        }
    }
}

/// One line of the report: what was checked, how it came out, and what to do.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub health: Health,
    pub detail: String,
    /// The command that fixes it. Absent when there is nothing to fix.
    pub remedy: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            health: Health::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, remedy: Option<String>) -> Self {
        Self {
            name,
            health: Health::Warn,
            detail: detail.into(),
            remedy,
        }
    }

    fn fail(
        name: &'static str,
        code: Code,
        detail: impl Into<String>,
        remedy: Option<String>,
    ) -> Self {
        Self {
            name,
            health: Health::Fail(code),
            detail: detail.into(),
            remedy,
        }
    }
}

impl Serialize for Check {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut out = s.serialize_struct("Check", 4)?;
        out.serialize_field("name", self.name)?;
        out.serialize_field("status", self.health.word())?;
        out.serialize_field("detail", &self.detail)?;
        out.serialize_field("remedy", &self.remedy)?;
        out.end()
    }
}

/// Which node the machine's commands would address, decided without a network
/// call. Resolving properly needs the control plane, and doctor has to be able
/// to diagnose a machine that cannot reach it.
enum Intent {
    /// The reserved local node, named explicitly or as the saved default.
    Local,
    /// A hosted node, by whatever identifier the user wrote.
    Hosted(String),
    /// Nothing chosen yet.
    Unset,
}

impl Intent {
    /// Whether the control plane and a session are on this machine's path.
    ///
    /// Only a chosen hosted node puts them there. A machine with nothing chosen
    /// yet is not broken, it is unconfigured, and the one finding worth
    /// reporting is that -- with both ways out of it -- rather than an auth
    /// failure for an account the user may never want.
    fn addresses_a_hosted_node(&self) -> bool {
        matches!(self, Intent::Hosted(_))
    }
}

/// The full report, so `--json` hands back every check rather than a verdict.
#[derive(Serialize)]
struct Report {
    ok: bool,
    checks: Vec<Check>,
}

/// Run every check and hand back the findings, without printing anything.
///
/// Separate from `run` so `knaix report` can put the same diagnosis in a bundle
/// rather than growing a second one that would drift from this.
pub async fn collect(ctx: &KnaixContext, node_flag: Option<String>) -> Vec<Check> {
    let mut checks = Vec::new();

    // Read the project file here rather than taking main's copy: a file that
    // will not parse breaks every other command, which makes it exactly the
    // kind of thing doctor exists to name. main skips the pre-parse for this
    // command so the failure arrives here instead of before it.
    let (project_check, project_node) = check_project();
    let intent = intent_of(ctx, node_flag, project_node);

    checks.push(check_cli());
    checks.push(project_check);
    checks.push(check_api_url(ctx));

    let control_plane = check_control_plane(ctx, &intent).await;
    let reached_control_plane = control_plane.health == Health::Ok;
    checks.push(control_plane);

    // The node list the session check fetches is the same list the target check
    // needs to resolve a name, so it is fetched once and handed along.
    let (auth_check, nodes) = check_auth(ctx, &intent, reached_control_plane).await;
    checks.push(auth_check);
    checks.push(check_docker(&intent));
    checks.push(check_local_node(ctx, &intent).await);
    checks.push(check_target(ctx, &intent, reached_control_plane, nodes.as_deref()).await);

    checks
}

/// The code a set of checks exits with: the first failure, in path order.
pub fn verdict(checks: &[Check]) -> Option<Code> {
    checks.iter().find_map(|c| match c.health {
        Health::Fail(code) => Some(code),
        _ => None,
    })
}

pub async fn run(ctx: &KnaixContext, node_flag: Option<String>) -> Result<()> {
    let checks = collect(ctx, node_flag).await;

    // The first failure supplies the code, and the checks run in the order a
    // request travels, so a script sees the earliest thing in the path that is
    // broken rather than a later symptom of it.
    let failure = verdict(&checks);

    if ctx.output_format == "json" {
        let report = Report {
            ok: failure.is_none(),
            checks,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&checks);
    }

    match failure {
        None => Ok(()),
        Some(code) => Err(anyhow!(
            "Something is wrong with this setup. The failing checks above say what."
        ))
        .coded(code),
    }
}

/// Which node this machine's commands would address: an explicit flag, then the
/// project file, then the saved default. The same order every other command
/// resolves in, minus the network.
fn intent_of(ctx: &KnaixContext, flag: Option<String>, from_project: Option<String>) -> Intent {
    let named = flag
        .or(from_project)
        .or_else(|| ctx.config.default_node_id.clone());
    match named {
        Some(id) if id == crate::local::LOCAL_NODE_ID => Intent::Local,
        Some(id) => Intent::Hosted(id),
        None => Intent::Unset,
    }
}

fn check_cli() -> Check {
    let current = env!("CARGO_PKG_VERSION");
    match crate::update::newer_version_available() {
        Some(latest) => Check::warn(
            "cli",
            format!("v{current}, but v{latest} is available"),
            Some("curl -sSL https://knaix.com/install.sh | sh".to_string()),
        ),
        None => Check::ok("cli", format!("v{current}")),
    }
}

/// The project file, and the node it names.
fn check_project() -> (Check, Option<String>) {
    match crate::project::current() {
        Ok(None) => (
            Check::ok("project file", "none in this directory or above"),
            None,
        ),
        Ok(Some(p)) => {
            let detail = match &p.node {
                Some(node) => format!("{} addresses {}", crate::project::FILE_NAME, node),
                None => format!("{}, no node recorded", crate::project::FILE_NAME),
            };
            (Check::ok("project file", detail), p.node)
        }
        // Every command reads this file, so a broken one breaks all of them.
        Err(e) => (
            Check::fail(
                "project file",
                Code::Error,
                format!("{e}"),
                Some(crate::brand::cmd("init --force")),
            ),
            None,
        ),
    }
}

fn check_api_url(ctx: &KnaixContext) -> Check {
    let url = &ctx.config.api_url;
    match url::Url::parse(url) {
        Ok(parsed) if parsed.has_host() => Check::ok("api url", url.clone()),
        _ => Check::fail(
            "api url",
            Code::Error,
            format!("{url} is not a URL requests can be sent to"),
            Some(crate::brand::cmd(
                "config --api-url https://api.kovalentai.com",
            )),
        ),
    }
}

async fn check_control_plane(ctx: &KnaixContext, intent: &Intent) -> Check {
    let url = format!("{}/health", ctx.config.api_url);
    let started = Instant::now();
    let reached = ctx
        .client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let latency = started.elapsed().as_millis();

    if reached {
        return Check::ok("control plane", format!("reachable, {latency}ms"));
    }

    let detail = format!("could not reach {}", ctx.config.api_url);
    // A local-only user never touches it, and telling them their setup is
    // broken because a service they do not use is unreachable would be wrong.
    if intent.addresses_a_hosted_node() {
        Check::fail(
            "control plane",
            Code::Unavailable,
            detail,
            Some("check your network connection and try again".to_string()),
        )
    } else {
        Check::warn(
            "control plane",
            format!("{detail} (not needed for the local node)"),
            None,
        )
    }
}

/// Whether the stored session is accepted, and the node list proving it.
///
/// The list is returned rather than counted and dropped: it is the same list
/// the target check needs to resolve a name, and fetching it twice per run
/// asked the control plane a question it had already answered.
async fn check_auth(
    ctx: &KnaixContext,
    intent: &Intent,
    reached: bool,
) -> (Check, Option<Vec<crate::nodes::Node>>) {
    let login = Some(crate::brand::cmd("login"));

    if ctx.config.token.is_none() {
        let check = if intent.addresses_a_hosted_node() {
            Check::fail("auth", Code::Auth, "no session on this machine", login)
        } else {
            Check::warn(
                "auth",
                "not logged in (the local node needs no account)",
                None,
            )
        };
        return (check, None);
    }

    // A stored token proves nothing until the control plane accepts it, and
    // asking is the only way to know. Nothing to ask when it is unreachable.
    if !reached {
        return (
            Check::warn(
                "auth",
                "a session is stored, but the control plane could not be reached to check it",
                None,
            ),
            None,
        );
    }

    match crate::nodes::fetch_nodes(ctx).await {
        Ok(nodes) => {
            let who = ctx.config.username.as_deref().unwrap_or("session");
            let check = Check::ok("auth", format!("{who}, {} node(s)", nodes.len()));
            (check, Some(nodes))
        }
        // The fetch tags a rejected credential, so the code it carries is what
        // separates "you are not who you say" from "the request did not work".
        Err(e) if crate::exit::code_of(&e) == Code::Auth => (
            Check::fail("auth", Code::Auth, "the stored session was rejected", login),
            None,
        ),
        Err(e) => (
            Check::warn("auth", format!("could not verify the session: {e}"), None),
            None,
        ),
    }
}

fn check_docker(intent: &Intent) -> Check {
    use crate::local::Probe;
    match crate::local::docker_version(crate::local::PROBE_WAIT) {
        Probe::Answered(v) => Check::ok("docker", format!("server {v}")),
        // Docker took the question and never answered. Naming that is the
        // point: it is a state the other commands hang in rather than report.
        Probe::Unresponsive => Check::warn(
            "docker",
            format!(
                "did not answer within {}s; the daemon may be wedged",
                crate::local::PROBE_WAIT.as_secs()
            ),
            Some("restart Docker".to_string()),
        ),
        // Only the local node needs it, so this is never a failure on its own:
        // the target check below is what reports an unusable local setup.
        Probe::Absent if matches!(intent, Intent::Local) => Check::warn(
            "docker",
            "not available, and the default node is the local one",
            Some("start Docker Desktop, or install Docker".to_string()),
        ),
        Probe::Absent => Check::warn(
            "docker",
            "not available (only 'knaix local' needs it)",
            None,
        ),
    }
}

async fn check_local_node(ctx: &KnaixContext, intent: &Intent) -> Check {
    // Bounded, for the same reason the docker check is: asking about the
    // container goes through the same daemon that may be the thing wrong.
    let local = crate::local::summarize_within(crate::local::PROBE_WAIT);
    let is_default = matches!(intent, Intent::Local);
    let start = Some(crate::brand::cmd("local up"));

    if local.state != "running" {
        let detail = match local.state.as_str() {
            "none" => "none on this machine".to_string(),
            "unknown" => "docker did not say whether a container is running".to_string(),
            other => format!("container is {other}"),
        };
        // Not running is only a failure when it is the node commands address,
        // and the target check reports that, so this one stays a warning.
        return Check::warn("local node", detail, if is_default { start } else { None });
    }

    let Some(url) = local.url else {
        return Check::warn(
            "local node",
            "the container is running but no port was recorded",
            start,
        );
    };

    match local_health(ctx, &url).await {
        Some(body) if body["ready"].as_bool().unwrap_or(false) => {
            // Naming the model, the way `local status` does. Which model is
            // answering is the fact someone checking their setup most wants
            // confirmed, and reporting only its address withholds it.
            let node = crate::local::load();
            let answers = match node.as_ref().and_then(|n| n.model_url.as_deref()) {
                Some(server) => match node.as_ref().and_then(|n| n.model.as_deref()) {
                    Some(model) => format!("answering with {model} at {server}"),
                    None => format!("answering with a model at {server}"),
                },
                None => "answering with the deterministic mock".to_string(),
            };
            Check::ok("local node", format!("ready on {url}, {answers}"))
        }
        Some(_) => Check::warn(
            "local node",
            format!("running on {url} but not ready"),
            None,
        ),
        None => Check::warn(
            "local node",
            format!("the container is running but {url} did not answer"),
            Some(crate::brand::cmd("local logs")),
        ),
    }
}

async fn local_health(ctx: &KnaixContext, base: &str) -> Option<serde_json::Value> {
    let resp = ctx
        .client
        .get(format!("{base}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// The last thing on the path: can the node this machine's commands address
/// actually be reached?
///
/// `nodes` is the list the session check already fetched, present only when the
/// session was verified. Absent means there was nothing to fetch it with, and
/// the checks above have already said why.
async fn check_target(
    ctx: &KnaixContext,
    intent: &Intent,
    reached: bool,
    nodes: Option<&[crate::nodes::Node]>,
) -> Check {
    match intent {
        Intent::Unset => Check::fail(
            "target node",
            Code::Precondition,
            "no node is configured, so commands have nothing to address",
            Some(format!(
                "{} for a free local node, or {} for a hosted one",
                crate::brand::cmd("local up"),
                crate::brand::cmd("up")
            )),
        ),
        Intent::Local => {
            let start = Some(crate::brand::cmd("local up"));
            // The state file outlives the container, so a node that was started
            // and then stopped still resolves. Reporting that as "not answering"
            // would send someone looking at ports for a node that is not running.
            let state = crate::local::summarize_within(crate::local::PROBE_WAIT).state;
            if state != "running" {
                // "unknown" means docker never answered, so whether a node is
                // running is the one thing this check could not establish.
                let detail = if state == "unknown" {
                    "local is the default node, but docker did not say whether it is running"
                } else {
                    "local is the default node, but no local node is running"
                };
                return Check::fail("target node", Code::Precondition, detail, start);
            }
            match crate::nodes::resolve_target(ctx, Some(crate::local::LOCAL_NODE_ID.to_string()))
                .await
            {
                Ok(Some(Target::Local { base, .. })) => match local_health(ctx, &base).await {
                    Some(body) if body["ready"].as_bool().unwrap_or(false) => {
                        Check::ok("target node", format!("local, ready on {base}"))
                    }
                    _ => Check::fail(
                        "target node",
                        Code::Unavailable,
                        format!("local is running, but {base} is not answering ready"),
                        Some(crate::brand::cmd("local logs")),
                    ),
                },
                _ => Check::fail(
                    "target node",
                    Code::Precondition,
                    "local is the default node, but none has been started",
                    start,
                ),
            }
        }
        Intent::Hosted(named) => {
            if !reached {
                return Check::fail(
                    "target node",
                    Code::Unavailable,
                    format!("{named}, but the control plane could not be reached to find it"),
                    None,
                );
            }
            // Resolved against the list the session check already fetched.
            let Some(nodes) = nodes else {
                return Check::fail(
                    "target node",
                    Code::Auth,
                    format!("{named}, but the session could not be used to look it up"),
                    Some(crate::brand::cmd("login")),
                );
            };
            match crate::nodes::find_node(nodes, named) {
                Some(node) => match crate::nodes::node_uuid(node) {
                    Ok(uuid) => hosted_health(ctx, &uuid).await,
                    Err(e) => Check::fail(
                        "target node",
                        crate::exit::code_of(&e),
                        format!("{named}: {e}"),
                        None,
                    ),
                },
                None => Check::fail(
                    "target node",
                    Code::NotFound,
                    format!("no node on this account matches {named}"),
                    Some(crate::brand::cmd("list")),
                ),
            }
        }
    }
}

async fn hosted_health(ctx: &KnaixContext, uuid: &str) -> Check {
    let url = format!("{}/api/nodes/{}/health", ctx.config.api_url, uuid);
    let started = Instant::now();
    let resp = ctx
        .client
        .get(&url)
        .header(
            AUTHORIZATION,
            format!("Bearer {}", ctx.config.token.as_deref().unwrap_or("")),
        )
        .send()
        .await;
    let latency = started.elapsed().as_millis();

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            if body["healthy"].as_bool().unwrap_or(false) {
                Check::ok("target node", format!("{uuid}, healthy, {latency}ms"))
            } else {
                Check::fail(
                    "target node",
                    Code::Unavailable,
                    format!("{uuid} reports unhealthy"),
                    Some(crate::brand::cmd("logs")),
                )
            }
        }
        Ok(r) => Check::fail(
            "target node",
            Code::for_status(r.status().as_u16()),
            format!("{uuid}: HTTP {}", r.status()),
            None,
        ),
        Err(e) => Check::fail(
            "target node",
            Code::Unavailable,
            format!("{uuid}: {e}"),
            None,
        ),
    }
}

fn print_report(checks: &[Check]) {
    println!("\n{}", "Diagnosis:".bold().underline());

    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_header(vec!["Result", "Check", "Detail"]);
    for check in checks {
        table.add_row(vec![
            check.health.label(),
            check.name.dimmed().to_string(),
            check.detail.clone(),
        ]);
    }
    println!("{table}");

    // Remedies go under the table rather than in a fourth column, which would
    // wrap a command across lines and make it unusable to copy.
    let remedies: Vec<&Check> = checks
        .iter()
        .filter(|c| c.health != Health::Ok && c.remedy.is_some())
        .collect();
    if !remedies.is_empty() {
        println!("\n{}", "What to do:".bold());
        for check in remedies {
            println!(
                "  {} {}",
                format!("{}:", check.name).dimmed(),
                check.remedy.as_deref().unwrap_or("").cyan()
            );
        }
    }

    if checks.iter().any(|c| matches!(c.health, Health::Fail(_))) {
        println!();
    } else {
        // Naming the two commands makes the claim checkable. "Everything a
        // command needs" leaves the reader to guess which commands, right
        // after they may have watched a check warn.
        println!(
            "\n{} Everything {} and {} need is in place.\n",
            "✓".green(),
            crate::brand::cmd("chat"),
            crate::brand::cmd("upload")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(health: Health) -> Check {
        Check {
            name: "test",
            health,
            detail: String::new(),
            remedy: None,
        }
    }

    fn first_failure(checks: &[Check]) -> Option<Code> {
        checks.iter().find_map(|c| match c.health {
            Health::Fail(code) => Some(code),
            _ => None,
        })
    }

    /// The checks run in the order a request travels, so the code belongs to the
    /// earliest broken thing in the path rather than a later symptom of it.
    #[test]
    fn the_first_failure_supplies_the_code() {
        let checks = vec![
            check(Health::Ok),
            check(Health::Warn),
            check(Health::Fail(Code::Auth)),
            check(Health::Fail(Code::Unavailable)),
        ];
        assert_eq!(first_failure(&checks), Some(Code::Auth));
    }

    /// Warnings are for things that do not stop a node answering, so a run that
    /// only warns must still exit 0. Otherwise nobody can put doctor in CI.
    #[test]
    fn warnings_alone_are_not_a_failure() {
        let checks = vec![check(Health::Ok), check(Health::Warn)];
        assert_eq!(first_failure(&checks), None);
    }

    /// Only a chosen hosted node puts the control plane and a session on the
    /// path. Reporting either as broken for the other two would fail a
    /// local-only machine, and a brand new one, for services they never use.
    #[test]
    fn only_a_hosted_node_puts_the_account_on_the_path() {
        assert!(Intent::Hosted("abc".into()).addresses_a_hosted_node());
        assert!(!Intent::Local.addresses_a_hosted_node());
        assert!(!Intent::Unset.addresses_a_hosted_node());
    }

    /// A check serializes under the names a script reads, not the Rust ones.
    #[test]
    fn json_reports_the_status_as_a_word() {
        let json = serde_json::to_value(check(Health::Fail(Code::Auth))).unwrap();
        assert_eq!(json["status"], "fail");
        assert_eq!(json["name"], "test");
    }
}
