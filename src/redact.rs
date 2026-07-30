//! What a diagnostic bundle is allowed to say about you.
//!
//! The rule here is an allowlist, and the reason is worth stating plainly: a
//! denylist of things that look sensitive works until the first customer whose
//! node is named after their employer, or whose model server is on a host that
//! names an internal project. Patterns catch what we thought of. So a field is
//! included because it is known to be safe, never merely because it failed to
//! match something.
//!
//! Identifiers that matter for correlation but not for reading are hashed
//! rather than dropped. Two reports from the same person agree on the hash of
//! their node, which is what support actually needs, and the hash says nothing
//! about what the node is called.
//!
//! Every removal is recorded. Someone deciding whether to attach a bundle
//! deserves to see the command's own account of what it took out, rather than
//! being asked to trust a claim they cannot check.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// The control plane we publish. Anything else is a customer's own deployment,
/// so its address is theirs and not ours to record.
const PUBLIC_API_URL: &str = "https://api.kovalentai.com";

/// The routes our own software serves, which is how a log line's path is judged.
///
/// Enumerated rather than pattern-matched, because a path only has to look like
/// a route to pass a shape test, and a customer's directory is under no
/// obligation to look different from ours.
const NODE_ROUTE_PREFIXES: &[&str] = &["/api", "/health", "/v1", "/mcp", "/metrics"];

/// One thing the bundle left out or replaced, and why.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Removal {
    /// Where it came from, as a person would name it.
    pub field: String,
    /// What was done: "removed", "hashed", "host removed".
    pub action: String,
    pub reason: String,
}

/// Records what was taken out while a bundle is built.
#[derive(Default, Debug)]
pub struct Manifest {
    removals: Vec<Removal>,
}

impl Manifest {
    pub fn note(&mut self, field: &str, action: &str, reason: &str) {
        let removal = Removal {
            field: field.to_string(),
            action: action.to_string(),
            reason: reason.to_string(),
        };
        // One line per kind of removal, not per occurrence: forty redacted log
        // filenames is a fact about the logs, not forty facts.
        if !self.removals.contains(&removal) {
            self.removals.push(removal);
        }
    }

    pub fn removals(&self) -> &[Removal] {
        &self.removals
    }
}

/// A stable, short stand-in for an identifier.
///
/// Truncated to 12 hex characters, which is far past collision trouble at the
/// scale of one person's nodes and short enough to read out loud. Prefixed so
/// nobody mistakes it for the real value.
pub fn hashed(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("id:{hex}")
}

/// Whether a token is present, and how long it is. Never what it is.
///
/// Length is worth keeping: "present, 328 chars" and "present, 12 chars" are
/// different bugs, and neither is a credential.
pub fn secret_shape(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() => format!("present, {} chars", v.len()),
        _ => "absent".to_string(),
    }
}

/// The API URL, if it is ours to record.
///
/// A customer pointing the CLI at their own control plane has told us the
/// hostname of something on their network. That it is non-default is the useful
/// fact; which host it is, is theirs.
pub fn api_url(url: &str, manifest: &mut Manifest) -> String {
    if url == PUBLIC_API_URL {
        return url.to_string();
    }
    manifest.note(
        "config.api_url",
        "removed",
        "a non-default control plane is a private address",
    );
    "<non-default, removed>".to_string()
}

/// A model server address with the host taken out.
///
/// The scheme and port are the diagnostic content: they tell us whether someone
/// is on Ollama's 11434 or an LM Studio port, and whether it is loopback. The
/// host can be an internal machine name, so it goes.
pub fn model_url(url: &str, manifest: &mut Manifest) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        manifest.note("local.model_url", "removed", "could not be parsed safely");
        return "<removed>".to_string();
    };
    manifest.note(
        "local.model_url",
        "host removed",
        "a model server host can name an internal machine",
    );
    match parsed.port() {
        Some(port) => format!("{}://<removed>:{}", parsed.scheme(), port),
        None => format!("{}://<removed>", parsed.scheme()),
    }
}

/// Replaces the specific values this machine is known to hold, wherever they
/// turn up in free text.
///
/// The checks `doctor` produces are English sentences with values interpolated
/// into them: "acme-prod, but the control plane could not be reached". Redacting
/// the config section and passing those through unchanged leaks the same node
/// name one field later, which is exactly what happened the first time this was
/// run against a real machine.
///
/// This is not the pattern-matching the module header warns against. Nothing
/// here guesses what looks sensitive: the CLI already holds these exact strings,
/// so the substitution is of known values, and the only judgement is which of
/// our own settings are the user's.
#[derive(Default)]
pub struct Scrubber {
    /// Longest first, so a value containing another is replaced before it.
    subs: Vec<(String, String)>,
}

impl Scrubber {
    /// Every value this machine holds that must not appear in free text.
    ///
    /// Reads the settings directly rather than taking a context, so the failure
    /// recorder can use it from `main`'s error path, where there is no context
    /// left to borrow.
    ///
    /// A model server host is included only when it is not loopback:
    /// `127.0.0.1` names nothing, and removing it would cost the reader the one
    /// detail that says where the node was expected to be.
    pub fn for_this_machine() -> Self {
        let mut s = Self::default();
        let c = crate::config::load_config();

        s.add(c.token.as_deref(), "<token removed>".to_string());
        if let Some(u) = c.username.as_deref() {
            s.add(Some(u), hashed(u));
        }
        if let Some(n) = c.default_node_id.as_deref() {
            if n != crate::local::LOCAL_NODE_ID {
                s.add(Some(n), hashed(n));
            }
        }

        if let Some(node) = crate::local::load() {
            s.add(Some(&node.instance_id), hashed(&node.instance_id));
            if let Some(host) = node
                .model_url
                .as_deref()
                .and_then(|u| url::Url::parse(u).ok())
                .and_then(|u| u.host_str().map(|h| h.to_string()))
            {
                let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]");
                if !loopback {
                    s.add(Some(&host), "<host removed>".to_string());
                }
            }
        }
        s
    }

    /// Register a value to replace. Short values are skipped: substituting a
    /// two-character string would shred unrelated prose, and nothing that short
    /// identifies anyone.
    pub fn add(&mut self, value: Option<&str>, replacement: String) {
        let Some(v) = value else { return };
        if v.len() < 3 {
            return;
        }
        self.subs.push((v.to_string(), replacement));
        self.subs.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
    }

    /// Free text with every known value replaced, and any UUID hashed.
    ///
    /// UUIDs get their own rule because the format is exact rather than
    /// suggestive: a canonical UUID in our output is always an instance
    /// identifier and never something a reader needs to see literally.
    pub fn scrub(&self, text: &str, field: &str, manifest: &mut Manifest) -> String {
        let mut out = text.to_string();

        for (value, replacement) in &self.subs {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), replacement);
                manifest.note(
                    field,
                    "replaced",
                    "carried a value from your settings, such as a node or user name",
                );
            }
        }

        if let Some(replaced) = hash_uuids(&out) {
            manifest.note(field, "hashed", "carried an instance UUID");
            out = replaced;
        }
        out
    }
}

/// Replace every canonical UUID in a string with its hash, or None if there
/// were none.
fn hash_uuids(text: &str) -> Option<String> {
    let is_uuid = |s: &str| {
        s.len() == 36
            && s.as_bytes().iter().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => *b == b'-',
                _ => b.is_ascii_hexdigit(),
            })
    };

    let mut found = false;
    let out: Vec<String> = text
        .split(' ')
        .map(|token| {
            // Punctuation commonly follows one: "uuid, healthy".
            let trimmed = token.trim_end_matches([',', '.', ':']);
            if is_uuid(trimmed) {
                found = true;
                token.replace(trimmed, &hashed(trimmed))
            } else {
                token.to_string()
            }
        })
        .collect();

    found.then(|| out.join(" "))
}

/// A log line reduced to the parts that are known to be safe.
///
/// Node logs are the most useful thing in a bundle and the most dangerous. This
/// keeps what a route tells us and drops everything else, rather than hunting
/// for the parts that look private. The difference is not stylistic: a first
/// attempt here removed quoted strings and paths with extensions, and leaked
/// `/var/data/Q3 Board Pack.pdf` on the first test, because a filename with a
/// space in it did not look like a path. Anything built by removal loses to the
/// input nobody pictured.
///
/// What survives: a timestamp, a bracketed component tag, a log level, an HTTP
/// method, and a route with no dot or space in it. The result is a skeleton,
/// which is what a bug report needs and all it is owed.
pub fn log_line(line: &str, manifest: &mut Manifest) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut dropped_any = false;

    for token in line.split_whitespace() {
        let bare = token.trim_matches(|c| c == '[' || c == ']');

        let is_timestamp = bare.len() >= 8
            && bare
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '.' | '+'));
        let is_tag = token.starts_with('[')
            && token.ends_with(']')
            && bare
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let is_level = matches!(
            bare.to_ascii_uppercase().as_str(),
            "ERROR" | "WARN" | "WARNING" | "INFO" | "DEBUG" | "TRACE"
        );
        let is_method = matches!(bare, "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD");
        // A route we serve, named from the list of routes we actually serve.
        // "Looks like a route" was not enough: `/var/data/Q3` has no dot and no
        // odd characters, so a shape test kept it. We know our own routes, so
        // there is no reason to guess at anyone else's paths.
        let is_route = NODE_ROUTE_PREFIXES
            .iter()
            .any(|prefix| bare == *prefix || bare.starts_with(&format!("{prefix}/")))
            && !bare.contains('.')
            && bare
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'));
        let is_status = bare.len() == 3 && bare.chars().all(|c| c.is_ascii_digit());

        if is_timestamp || is_tag || is_level || is_method || is_route || is_status {
            kept.push(token.to_string());
        } else {
            dropped_any = true;
        }
    }

    if dropped_any {
        manifest.note(
            "local node logs",
            "reduced",
            "kept the timestamp, level, method and route; dropped everything else, which can carry filenames and document text",
        );
    }
    kept.join(" ")
}

/// Command-line arguments with everything but the subcommand and flags removed.
///
/// The shape of the command is the diagnostic content. The values are the user's
/// question, their file path, their node name: all of it theirs.
pub fn argv(args: &[String], manifest: &mut Manifest) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut removed_any = false;
    // What the previous flag implies about the word after it. The output format
    // changes behaviour and names nothing, so it is the one value worth having.
    let mut next: Next = Next::Subcommand;

    enum Next {
        Subcommand,
        Keep,
        Remove,
        Judge,
    }

    for (i, arg) in args.iter().enumerate() {
        // argv[0] is a path to the binary, which on some machines is a home
        // directory with a real name in it.
        if i == 0 {
            out.push("knaix".to_string());
            continue;
        }

        if arg.starts_with('-') {
            // `--flag=value` carries its value with it.
            if let Some((flag, _)) = arg.split_once('=') {
                out.push(format!("{flag}=<removed>"));
                removed_any = true;
                next = Next::Judge;
                continue;
            }
            next = match arg.as_str() {
                "-o" | "--output" => Next::Keep,
                _ => Next::Remove,
            };
            out.push(arg.clone());
            continue;
        }

        match next {
            // The first bare word is the subcommand, which we want.
            Next::Subcommand => out.push(arg.clone()),
            Next::Keep => out.push(arg.clone()),
            _ => {
                out.push("<removed>".to_string());
                removed_any = true;
            }
        }
        next = Next::Judge;
    }

    if removed_any {
        manifest.note(
            "command arguments",
            "removed",
            "arguments carry questions, paths and node names",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_described_never_quoted() {
        let shape = secret_shape(Some("sk-live-abcdefghijklmnop"));
        assert_eq!(shape, "present, 24 chars");
        assert!(!shape.contains("sk-live"));
        assert_eq!(secret_shape(None), "absent");
        assert_eq!(secret_shape(Some("")), "absent");
    }

    /// Two reports from one person have to agree, or correlation is impossible.
    /// The hash must not be reversible to the name it stands for.
    #[test]
    fn identifiers_hash_stably_and_reveal_nothing() {
        let a = hashed("acme-prod");
        assert_eq!(a, hashed("acme-prod"));
        assert_ne!(a, hashed("acme-staging"));
        assert!(!a.contains("acme"));
        assert!(a.starts_with("id:"));
    }

    #[test]
    fn our_control_plane_is_named_and_theirs_is_not() {
        let mut m = Manifest::default();
        assert_eq!(
            api_url("https://api.kovalentai.com", &mut m),
            "https://api.kovalentai.com"
        );
        assert!(m.removals().is_empty());

        let private = api_url("https://kovalent.internal.acme.example", &mut m);
        assert!(!private.contains("acme"));
        assert_eq!(m.removals().len(), 1);
    }

    /// The port says which model server someone runs, which is most of the
    /// diagnostic value. The host can name an internal machine.
    #[test]
    fn a_model_server_keeps_its_port_and_loses_its_host() {
        let mut m = Manifest::default();
        let out = model_url("http://gpu-box.corp.example:11434", &mut m);
        assert_eq!(out, "http://<removed>:11434");
        assert!(!out.contains("corp"));
        assert!(!out.contains("gpu-box"));
    }

    #[test]
    fn a_log_line_keeps_its_route_and_loses_its_filenames() {
        let mut m = Manifest::default();
        let out = log_line(
            r#"[2026-07-30] POST /api/kb/ingest file="Q3 Board Pack.pdf" -> /var/data/Q3 Board Pack.pdf"#,
            &mut m,
        );
        assert!(
            out.contains("/api/kb/ingest"),
            "the route is the point: {out}"
        );
        assert!(!out.contains("Board"), "a document name leaked: {out}");
        assert!(!out.contains("var/data"), "a path leaked: {out}");
        assert!(!m.removals().is_empty());
    }

    #[test]
    fn a_command_keeps_its_shape_and_loses_its_values() {
        let mut m = Manifest::default();
        let out = argv(
            &[
                "/Users/realname/bin/knaix".into(),
                "chat".into(),
                "-n".into(),
                "acme-prod".into(),
                "what are the renewal terms?".into(),
            ],
            &mut m,
        );
        assert_eq!(out[0], "knaix");
        assert_eq!(out[1], "chat");
        assert_eq!(out[2], "-n");
        assert_eq!(out[3], "<removed>");
        assert_eq!(out[4], "<removed>");
        let joined = out.join(" ");
        assert!(!joined.contains("realname"));
        assert!(!joined.contains("acme"));
        assert!(!joined.contains("renewal"));
    }

    /// The output format is a flag value worth keeping: it changes behaviour and
    /// names nothing.
    #[test]
    fn the_output_format_survives_because_it_is_not_the_users() {
        let mut m = Manifest::default();
        let out = argv(
            &["knaix".into(), "doctor".into(), "-o".into(), "json".into()],
            &mut m,
        );
        assert_eq!(out, vec!["knaix", "doctor", "-o", "json"]);
    }

    /// A bundle repeating the same removal forty times buries the one that
    /// matters.
    #[test]
    fn the_manifest_records_a_kind_once() {
        let mut m = Manifest::default();
        for _ in 0..5 {
            log_line(r#"opened "secret.pdf""#, &mut m);
        }
        assert_eq!(m.removals().len(), 1);
    }
}
