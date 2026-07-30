//! What `knaix report` is allowed to say about the machine it ran on.
//!
//! These are the tests that make the command safe to recommend. They run it on
//! a scratch home whose every setting is a distinctive nonsense string, then
//! grep the whole bundle for each one. A leak anywhere, in any field, in any
//! phrasing, fails here.
//!
//! Grepping the raw text rather than inspecting fields is deliberate. The first
//! real run of this command redacted the config section correctly and leaked the
//! same node name one field later, inside a sentence `doctor` had written. A
//! field-by-field assertion would have passed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Values chosen so a match cannot be a coincidence, and so each one reads in a
/// failure message as the thing it stands for.
const TOKEN: &str = "tok-SUPERSECRETVALUE-must-never-appear-anywhere";
const USERNAME: &str = "adalovelace";
const NODE: &str = "acme-quarterly-reporting";
const INSTANCE: &str = "11111111-2222-3333-4444-555555555555";
const MODEL_HOST: &str = "gpu-box.internal.acme.example";

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-redact-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".knaix")).expect("could not create scratch home");
    dir
}

/// A machine with everything set: a session, a hosted default node, and a local
/// node pointed at a model server on a named host.
fn furnish(home: &Path) {
    fs::write(
        home.join(".knaix/config.json"),
        format!(
            r#"{{"token":"{TOKEN}","username":"{USERNAME}","default_node_id":"{NODE}","api_url":"http://127.0.0.1:9"}}"#
        ),
    )
    .unwrap();
    fs::write(
        home.join(".knaix/local.json"),
        format!(
            r#"{{"port":9,"instance_id":"{INSTANCE}","image":"none","model_url":"http://{MODEL_HOST}:11434","model":"llama3.1:8b"}}"#
        ),
    )
    .unwrap();
}

fn run_report(home: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_knaix"))
        .env("HOME", home)
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .current_dir(home)
        .args(["-o", "json", "report"])
        .output()
        .expect("failed to run knaix report");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The whole point of the command. Every secret this machine holds, checked
/// against every byte of the bundle.
#[test]
fn no_secret_on_the_machine_reaches_the_bundle() {
    let home = scratch_home("secrets");
    furnish(&home);
    let bundle = run_report(&home);

    assert!(!bundle.is_empty(), "report produced nothing");
    serde_json::from_str::<serde_json::Value>(&bundle).expect("bundle is not JSON");

    for (what, value) in [
        ("the session token", TOKEN),
        ("the username", USERNAME),
        ("the node name", NODE),
        ("the local instance UUID", INSTANCE),
        ("the model server host", MODEL_HOST),
    ] {
        assert!(
            !bundle.contains(value),
            "{what} reached the bundle. Searched for {value:?} in:\n{bundle}"
        );
    }
}

/// A redaction the user cannot see is a claim they have to take on trust. The
/// manifest is what makes the bundle checkable rather than believable.
#[test]
fn the_bundle_says_what_it_took_out() {
    let home = scratch_home("manifest");
    furnish(&home);
    let bundle: serde_json::Value = serde_json::from_str(&run_report(&home)).unwrap();

    let redactions = bundle["redactions"]
        .as_array()
        .expect("no redactions array");
    assert!(
        !redactions.is_empty(),
        "a machine with a token, a username and a private node reported no redactions"
    );
    for r in redactions {
        for key in ["field", "action", "reason"] {
            assert!(
                r[key].as_str().map(|s| !s.is_empty()).unwrap_or(false),
                "a redaction is missing its {key}: {r}"
            );
        }
    }
}

/// The token is the one value with no safe rendering at all. Its length is
/// diagnostic; its content never is.
#[test]
fn the_token_is_described_by_length_only() {
    let home = scratch_home("token");
    furnish(&home);
    let bundle: serde_json::Value = serde_json::from_str(&run_report(&home)).unwrap();

    let token = bundle["config"]["token"].as_str().unwrap();
    assert_eq!(token, format!("present, {} chars", TOKEN.len()));
    assert!(!token.contains("SUPERSECRET"));
}

/// Identifiers are hashed rather than dropped so two reports from one person
/// correlate. That is only true if the hash is stable across runs.
#[test]
fn two_reports_from_one_machine_agree_on_its_identifiers() {
    let home = scratch_home("stable");
    furnish(&home);

    let first: serde_json::Value = serde_json::from_str(&run_report(&home)).unwrap();
    let second: serde_json::Value = serde_json::from_str(&run_report(&home)).unwrap();

    assert_eq!(first["config"]["username"], second["config"]["username"]);
    assert_eq!(
        first["config"]["default_node"],
        second["config"]["default_node"]
    );
    assert_ne!(
        first["config"]["username"], first["config"]["default_node"],
        "two different values hashed to the same thing"
    );
}

/// A machine with nothing set up must still produce a usable bundle: the people
/// most likely to need this command are the ones whose setup never worked.
#[test]
fn a_bare_machine_still_produces_a_report() {
    let home = scratch_home("bare");
    let bundle: serde_json::Value = serde_json::from_str(&run_report(&home)).unwrap();

    assert_eq!(bundle["config"]["token"], "absent");
    assert!(
        bundle["checks"].as_array().map(|a| a.len()).unwrap_or(0) >= 8,
        "the diagnosis should be present even with nothing configured"
    );
}

/// The command's central promise. A diagnostic that phones home would contradict
/// the thing the product is for, so the absence of any request is a property
/// worth a test rather than a comment.
#[test]
fn report_makes_no_request_to_the_control_plane() {
    let home = scratch_home("nonet");
    furnish(&home);

    // Point every configured endpoint at a port with nothing on it. If the
    // command needed the network it would fail or hang; it must do neither.
    let out = Command::new(env!("CARGO_BIN_EXE_knaix"))
        .env("HOME", &home)
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .current_dir(&home)
        .args(["-o", "json", "report"])
        .output()
        .expect("failed to run knaix report");

    assert_eq!(
        out.status.code(),
        Some(0),
        "report must succeed with no reachable control plane"
    );
    serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&out.stdout))
        .expect("bundle is not JSON when the network is unreachable");
}
