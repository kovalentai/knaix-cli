//! End-to-end checks that the documented exit codes reach the shell.
//!
//! The unit tests in `exit` cover the classification. These cover the wiring,
//! which is the part that actually breaks: a code is only worth documenting if
//! `main` returns it, and nothing inside the process can prove that.
//!
//! Each code asserted here is published in the README. A change that moves one
//! breaks callers silently, so these tests are the contract.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A control plane that answers every request with the same JSON body.
///
/// Enough to reach the paths that need the API to have *answered*, which is
/// what separates "not found" from "unreachable". Returns the base URL; the
/// thread serves until the process ends.
fn serve(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-exit-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("could not create scratch home");
    dir
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home).env("KNAIX_NO_UPDATE_CHECK", "1");
    cmd
}

fn code_of(cmd: &mut Command) -> i32 {
    cmd.output()
        .expect("failed to run knaix")
        .status
        .code()
        .expect("knaix was killed by a signal")
}

/// Write a local node record pointing at a port with nothing on it, so the
/// transport fails rather than the lookup.
fn record_unreachable_local_node(home: &Path) {
    let dir = home.join(".knaix");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("local.json"),
        // Port 9 is discard: a connection there is refused immediately.
        r#"{"port":9,"instance_id":"exit-test","image":"none"}"#,
    )
    .unwrap();
    fs::write(
        dir.join("config.json"),
        r#"{"default_node_id":"local","api_url":"http://127.0.0.1:9"}"#,
    )
    .unwrap();
}

#[test]
fn success_is_zero() {
    let home = scratch_home("ok");
    assert_eq!(code_of(knaix(&home).arg("--version")), 0);
}

#[test]
fn a_bad_flag_is_a_usage_error() {
    let home = scratch_home("badflag");
    assert_eq!(code_of(knaix(&home).args(["--no-such-flag"])), 2);
}

#[test]
fn no_subcommand_is_a_usage_error() {
    let home = scratch_home("nosub");
    assert_eq!(code_of(&mut knaix(&home)), 2);
}

/// Every command that needs the control plane goes through `get_token`, so a
/// machine that has never logged in reports auth rather than a generic failure.
#[test]
fn not_logged_in_is_an_auth_error() {
    let home = scratch_home("noauth");
    assert_eq!(code_of(knaix(&home).arg("list")), 3);
}

/// The local node is addressed without a token, so this must not be reported as
/// an auth problem: nothing is missing except a running node.
#[test]
fn no_local_node_is_a_precondition_error() {
    let home = scratch_home("nolocal");
    let dir = home.join(".knaix");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), r#"{"default_node_id":"local"}"#).unwrap();

    assert_eq!(code_of(knaix(&home).args(["chat", "hello"])), 7);
}

/// A node that is recorded but not listening is unreachable, which is worth
/// telling apart from one that answered and refused.
#[test]
fn an_unreachable_node_is_reported_as_unavailable() {
    let home = scratch_home("unreachable");
    record_unreachable_local_node(&home);

    assert_eq!(code_of(knaix(&home).args(["chat", "hello"])), 4);
}

/// The refusal is the point: a script that forgot `--yes` must not have the
/// store deleted, and must be able to tell that refusal from a crash.
#[test]
fn reset_without_confirmation_is_denied() {
    let home = scratch_home("denied");
    record_unreachable_local_node(&home);

    let code = code_of(knaix(&home).args(["local", "reset"]));
    // Docker gates this command before the confirmation check, so on a machine
    // without Docker the honest answer is the precondition, not the refusal.
    assert!(
        code == 6 || code == 7,
        "expected denied (6), or precondition (7) where Docker is absent, got {code}"
    );
}

/// The API answered and had no such node. That is a different fact from the API
/// being unreachable, and the codes have to say so.
#[test]
fn an_unknown_node_is_not_found() {
    let home = scratch_home("notfound");
    let api = serve(r#"{"data":[]}"#);

    let code = code_of(
        knaix(&home)
            .env("KNAIX_TOKEN", "test-token")
            .env("KNAIX_API_URL", &api)
            .args(["chat", "-n", "no-such-node", "hello"]),
    );
    assert_eq!(code, 5, "an unknown node should be NotFound");
}

/// The same command against a control plane that is not listening is a
/// transport failure, and must report unavailable rather than not-found.
#[test]
fn the_same_lookup_against_a_dead_api_is_unavailable() {
    let home = scratch_home("deadapi");

    let code = code_of(
        knaix(&home)
            .env("KNAIX_TOKEN", "test-token")
            // Port 9 is discard: the connection is refused, not answered.
            .env("KNAIX_API_URL", "http://127.0.0.1:9")
            .args(["chat", "-n", "no-such-node", "hello"]),
    );
    assert_eq!(code, 4, "an unreachable API should be Unavailable");
}

/// Docker missing is the precondition case these codes exist for, and it broke
/// once already: the helper attached the code, and the caller rebuilt the error
/// with `anyhow!("...: {e}")`, which dropped it and reported a plain failure.
/// Rebuilding an error anywhere in this path will fail this test.
#[test]
fn a_missing_docker_is_a_precondition_not_a_generic_failure() {
    let home = scratch_home("nodocker");
    let empty = scratch_home("emptypath");

    let code = code_of(
        knaix(&home)
            // The binary is invoked by absolute path, so emptying PATH hides
            // docker without hiding knaix.
            .env("PATH", &empty)
            .args(["local", "reset", "--yes"]),
    );
    assert_eq!(code, 7, "a missing docker should be Precondition");
}

/// A reader that stops reading must end the process, not crash it.
///
/// Rust ignores SIGPIPE at startup, so writing to a closed pipe became an
/// ordinary error that the print macros panicked on: `knaix top | head` ended
/// in a Rust panic and an invitation to file a crash report. The process now
/// dies from the signal, which a shell reports as 141, and says nothing --
/// exactly what `yes | head` does.
///
/// Driven through a shell because the closed pipe is the point, and the parent
/// has to be the one that closes it.
#[cfg(unix)]
#[test]
fn a_closed_pipe_ends_quietly_rather_than_panicking() {
    let home = scratch_home("sigpipe");
    let bin = env!("CARGO_BIN_EXE_knaix");

    // `top` is the only producer that keeps writing indefinitely, which is what
    // this needs: anything with a fixed size fits in the pipe buffer, the write
    // succeeds before the reader is missed, and the test passes with the fix
    // reverted. `top` writes a table on every interval, so a write after the
    // reader has gone is guaranteed rather than raced for.
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("'{bin}' top --interval 1 | head -c 200"))
        .env("HOME", &home)
        // Nothing listens here, so the run needs no control plane and still
        // prints its table.
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .output()
        .expect("failed to run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "a closed pipe panicked instead of ending: {stderr}"
    );
    assert!(
        !stderr.contains("report"),
        "a closed pipe offered a crash report: {stderr}"
    );
}

/// Tagging a failure must not change what the user reads.
#[test]
fn the_error_text_still_says_what_went_wrong() {
    let home = scratch_home("message");
    let out = knaix(&home).arg("list").output().expect("failed to run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Not logged in"),
        "expected the original message on stderr, got: {stderr}"
    );
}
