//! End-to-end checks for `knaix doctor` and `knaix bench`.
//!
//! Both commands are about reporting honestly on a machine that is already
//! broken, so the interesting cases are all failure cases, and none of them can
//! be reached from inside the process: doctor's whole point is that it keeps
//! going where every other command stops, and only the exit code proves it did.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-doc-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("could not create scratch home");
    dir
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home)
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        // Port 9 is discard: the control plane is unreachable rather than
        // absent, which is the state a broken machine is actually in.
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        // Run from the scratch home so a `.knaix.toml` in the repo being tested
        // from cannot change what these assert.
        .current_dir(home);
    cmd
}

fn write_config(home: &Path, json: &str) {
    let dir = home.join(".knaix");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), json).unwrap();
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Run {
    let out = cmd.output().expect("failed to run knaix");
    Run {
        code: out.status.code().expect("knaix was killed by a signal"),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

/// A `docker` on PATH that behaves as told, so the states a real daemon will
/// not enter on demand can be tested.
///
/// Returns a directory to put at the front of PATH. The binary is invoked by
/// absolute path, so this hides docker without hiding knaix.
fn fake_docker(home: &Path, script: &str) -> PathBuf {
    let bin = home.join("fakebin");
    fs::create_dir_all(&bin).unwrap();
    let path = bin.join("docker");
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// Record a newer release as already found, the way the background check would.
/// `last_update_check` is far enough ahead that the check does not run again and
/// overwrite it.
fn cache_a_newer_version(home: &Path, config: &str) {
    let mut v: serde_json::Value = serde_json::from_str(config).unwrap();
    v["latest_known_version"] = serde_json::json!("999.0.0");
    v["last_update_check"] = serde_json::json!(99_999_999_999u64);
    write_config(home, &v.to_string());
}

fn checks(stdout: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(stdout).expect("doctor did not emit JSON")
}

fn status_of<'a>(report: &'a serde_json::Value, name: &str) -> &'a str {
    report["checks"]
        .as_array()
        .expect("checks should be an array")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check named {name}"))["status"]
        .as_str()
        .expect("status should be a string")
}

/// The whole point of the command: every other one stops at the first problem,
/// so a doctor that did the same would be worth nothing.
#[test]
fn doctor_reports_every_check_even_when_an_early_one_fails() {
    let home = scratch_home("allchecks");
    write_config(&home, r#"{"default_node_id":"some-hosted-node"}"#);

    let out = run(knaix(&home).args(["-o", "json", "doctor"]));
    let report = checks(&out.stdout);

    assert_eq!(report["ok"], false);
    // The control plane is unreachable, and the checks after it still ran.
    assert_eq!(status_of(&report, "control plane"), "fail");
    assert_eq!(status_of(&report, "target node"), "fail");
    assert!(
        report["checks"].as_array().unwrap().len() >= 8,
        "every check should be reported, got {}",
        report["checks"]
    );
}

/// A machine with nothing set up is not ready, and a script has to be able to
/// tell that from a node that is there and refusing.
#[test]
fn a_machine_with_no_node_configured_is_a_precondition() {
    let home = scratch_home("nonode");
    assert_eq!(run(knaix(&home).arg("doctor")).code, 7);
}

/// A local-only user never touches the control plane. Failing them because a
/// service they do not use is unreachable would make the command useless to the
/// half of the audience that never signs up.
#[test]
fn an_unreachable_control_plane_is_only_a_warning_for_a_local_node() {
    let home = scratch_home("localonly");
    write_config(&home, r#"{"default_node_id":"local"}"#);

    let out = run(knaix(&home).args(["-o", "json", "doctor"]));
    let report = checks(&out.stdout);

    assert_eq!(status_of(&report, "control plane"), "warn");
    assert_eq!(status_of(&report, "auth"), "warn");
    // The failure is the missing node, not the control plane, so the code is
    // the precondition rather than unavailable.
    assert_eq!(out.code, 7, "the missing local node should be the failure");
}

/// A hosted default with no session is an auth problem, and the code has to say
/// so rather than reporting the node lookup that could not be attempted.
#[test]
fn a_hosted_default_with_no_session_reports_auth() {
    let home = scratch_home("noauth");
    write_config(&home, r#"{"default_node_id":"some-hosted-node"}"#);

    let out = run(knaix(&home).args(["-o", "json", "doctor"]));
    assert_eq!(status_of(&checks(&out.stdout), "auth"), "fail");
}

/// Every command reads `.knaix.toml` before it runs, so a file that will not
/// parse breaks all of them -- including, without this, the one command whose
/// job is to find out why.
#[test]
fn doctor_names_a_broken_project_file_that_stops_every_other_command() {
    let home = scratch_home("brokenproject");
    write_config(&home, r#"{"default_node_id":"local"}"#);
    fs::write(home.join(".knaix.toml"), "node = [this is not toml").unwrap();

    // The control: another command cannot get past the file at all.
    assert_ne!(
        run(knaix(&home).arg("status")).code,
        0,
        "a broken project file should stop an ordinary command"
    );

    let out = run(knaix(&home).args(["-o", "json", "doctor"]));
    assert_eq!(status_of(&checks(&out.stdout), "project file"), "fail");
}

#[test]
fn bench_refuses_a_run_count_it_cannot_honour() {
    let home = scratch_home("badruns");
    write_config(&home, r#"{"default_node_id":"local"}"#);
    let dir = home.join(".knaix");
    fs::write(
        dir.join("local.json"),
        r#"{"port":9,"instance_id":"bench-test","image":"none"}"#,
    )
    .unwrap();

    assert_eq!(
        run(knaix(&home).args(["bench", "--runs", "0"])).code,
        2,
        "zero runs is a usage error"
    );
    assert_eq!(
        run(knaix(&home).args(["bench", "--runs", "1000"])).code,
        2,
        "a load test is a usage error, not a benchmark"
    );
}

/// Reachability is measured first so that an unreachable node is reported as
/// one, rather than as a confusing ingest failure -- and so that nothing is
/// written to a node that was never going to answer.
#[test]
fn bench_reports_an_unreachable_node_before_it_ingests_anything() {
    let home = scratch_home("benchdead");
    write_config(&home, r#"{"default_node_id":"local"}"#);
    let dir = home.join(".knaix");
    fs::write(
        dir.join("local.json"),
        r#"{"port":9,"instance_id":"bench-test","image":"none"}"#,
    )
    .unwrap();

    let out = run(knaix(&home).arg("bench"));
    assert_eq!(out.code, 4, "an unreachable node should be Unavailable");
    assert!(
        out.stderr.contains("Nothing was ingested"),
        "the failure should say the node was left alone, got: {}",
        out.stderr
    );
}

/// Docker is the one dependency that can accept a question and never answer,
/// and a wedged daemon is precisely a state doctor exists to name. Before the
/// probe deadline it hung here forever, printing nothing at all -- the report
/// that would have named the problem taken down by the problem.
#[cfg(unix)]
#[test]
fn doctor_does_not_hang_on_a_wedged_docker_daemon() {
    let home = scratch_home("wedged");
    write_config(&home, r#"{"default_node_id":"local"}"#);
    // Accepts the call and never returns, the way a wedged daemon does.
    let bin = fake_docker(&home, "sleep 900");

    let started = std::time::Instant::now();
    let out = run(knaix(&home)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .arg("doctor"));
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "doctor should give up on docker, took {elapsed:?}"
    );
    assert!(
        out.stdout.contains("did not answer"),
        "the report should name the wedged daemon, got: {}",
        out.stdout
    );
    // And it is a warning, not the verdict: the local node not running is what
    // actually stops a command, and that is the code the run carries.
    assert_eq!(out.code, 7);
}

/// The banner goes to stdout after whatever the command printed. On a JSON run
/// that lands after the document, and every script that parses the output
/// breaks the day an upgrade is published.
#[test]
fn an_available_upgrade_does_not_break_json_output() {
    let home = scratch_home("bannerjson");
    cache_a_newer_version(&home, r#"{"default_node_id":"local"}"#);

    // Not the usual helper: this needs the update check left switched on.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", &home)
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .current_dir(&home)
        .args(["-o", "json", "doctor"]);
    let out = run(&mut cmd);

    serde_json::from_str::<serde_json::Value>(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "JSON output did not parse ({e}); stdout was:\n{}",
            out.stdout
        )
    });
    // Parsing is the contract. The install command legitimately appears inside
    // the document as the `cli` check's remedy, so the banner is identified by
    // its own wording rather than by the URL the two share.
    assert!(
        !out.stdout
            .contains("A new version of Knaix CLI is available"),
        "the upgrade banner must not be appended to a JSON document"
    );
}

/// doctor reports the version as one of its checks, so the trailing banner is
/// the same news twice in one screen.
#[test]
fn doctor_does_not_repeat_the_upgrade_notice() {
    let home = scratch_home("bannertext");
    cache_a_newer_version(&home, r#"{"default_node_id":"local"}"#);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", &home)
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .current_dir(&home)
        .arg("doctor");
    let out = run(&mut cmd);

    assert!(
        out.stdout.contains("999.0.0"),
        "doctor should still report the available upgrade, got: {}",
        out.stdout
    );
    assert!(
        !out.stdout
            .contains("A new version of Knaix CLI is available"),
        "the banner repeats what the cli check already said, got: {}",
        out.stdout
    );
}
