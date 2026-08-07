//! `knaix list` against the node on this machine.
//!
//! The command used to refuse this outright, on the grounds that a local node
//! kept chunks and no document registry. That stopped being true when
//! `/api/kb/documents` shipped for `chat --doc`, and the refusal outlived it by
//! a release: the data was there, the client function to fetch it was there,
//! and `list` still said it could not be done.
//!
//! Driven through a stub node rather than a real one, so these run on a machine
//! with no Docker and no account. The point being asserted is that no token and
//! no control plane are involved, which is what makes the command work on the
//! machine that cannot reach the API.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A node that answers every request with the same JSON body.
fn serve_node(body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind");
    let port = listener.local_addr().unwrap().port();
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
    port
}

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-lslocal-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("could not create scratch home");
    dir
}

/// Record a local node on `port`, and point the API at a port nothing serves.
///
/// The dead API is deliberate: it is the situation the command has to work in.
fn record_local_node(home: &Path, port: u16) {
    let dir = home.join(".knaix");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("local.json"),
        format!(
            r#"{{"port":{port},"instance_id":"11111111-2222-3333-4444-555555555555","image":"none"}}"#
        ),
    )
    .unwrap();
    // Port 9 is discard: any reach for the control plane is refused at once.
    fs::write(
        dir.join("config.json"),
        r#"{"api_url":"http://127.0.0.1:9"}"#,
    )
    .unwrap();
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home).env("KNAIX_NO_UPDATE_CHECK", "1");
    cmd
}

const TWO_DOCUMENTS: &str = r#"{"documents":[
  {"document_id":"d-1","source":"Handbook.md","chunks":12,"created_at":"2026-08-06T03:03:56.140Z"},
  {"document_id":"d-2","source":"Refunds.md","chunks":3,"created_at":"2026-08-05T00:45:03.286Z"}
]}"#;

/// The table, with no account anywhere in reach.
#[test]
fn the_local_nodes_documents_are_listed_without_a_control_plane() {
    let home = scratch_home("table");
    record_local_node(&home, serve_node(TWO_DOCUMENTS));

    let out = knaix(&home)
        .args(["list", "-n", "local"])
        .output()
        .expect("failed to run knaix");

    assert!(
        out.status.success(),
        "exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in ["Handbook.md", "Refunds.md", "12", "Chunks"] {
        assert!(stdout.contains(expected), "missing {expected}: {stdout}");
    }
}

/// The positional form is the one the help text shows, so it gets the same
/// treatment as the flag rather than being resolved as a hosted node name.
#[test]
fn local_works_as_a_positional_argument_too() {
    let home = scratch_home("positional");
    record_local_node(&home, serve_node(TWO_DOCUMENTS));

    let out = knaix(&home)
        .args(["list", "local"])
        .output()
        .expect("failed to run knaix");

    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Handbook.md"));
}

/// A script reads the node's own field names, not a second spelling of them.
#[test]
fn json_output_carries_the_nodes_field_names() {
    let home = scratch_home("json");
    record_local_node(&home, serve_node(TWO_DOCUMENTS));

    let out = knaix(&home)
        .args(["list", "-n", "local", "-o", "json"])
        .output()
        .expect("failed to run knaix");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("not valid JSON");
    let docs = parsed.as_array().expect("expected an array");
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[0]["document_id"], "d-1");
    assert_eq!(docs[0]["source"], "Handbook.md");
    assert_eq!(docs[0]["chunks"], 12);
    assert_eq!(docs[0]["created_at"], "2026-08-06T03:03:56.140Z");
}

/// An empty node is not an error, and says what to do about it.
#[test]
fn a_node_holding_nothing_says_so_and_succeeds() {
    let home = scratch_home("empty");
    record_local_node(&home, serve_node(r#"{"documents":[]}"#));

    let out = knaix(&home)
        .args(["list", "-n", "local"])
        .output()
        .expect("failed to run knaix");

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no documents yet"), "printed: {stdout}");
    assert!(stdout.contains("upload"), "printed: {stdout}");
}

/// Enumerating by name is what `--sweep` is built on, and on a local node it
/// used to return nothing: the sweep reported removing zero and the only remedy
/// left on offer was `local reset`, which empties the store. Everything the user
/// ingested, to clear one synthetic handbook.
///
/// The stub answers every request with the same list, so what is asserted is the
/// count the sweep decided to act on: only the prefixed documents.
#[test]
fn a_local_sweep_finds_the_generated_documents_and_leaves_the_rest() {
    let home = scratch_home("sweep");
    record_local_node(
        &home,
        serve_node(
            r#"{"documents":[
              {"document_id":"b-1","source":"knaix-bench-a1.md","chunks":4,"created_at":"2026-08-06T03:03:56.140Z"},
              {"document_id":"b-2","source":"knaix-bench-b2.md","chunks":4,"created_at":"2026-08-06T03:03:56.140Z"},
              {"document_id":"real","source":"Handbook.md","chunks":9,"created_at":"2026-08-05T00:45:03.286Z"}
            ]}"#,
        ),
    );

    let out = knaix(&home)
        .args(["bench", "-n", "local", "--sweep"])
        .output()
        .expect("failed to run knaix");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Removed 2 benchmark document(s)"),
        "the sweep did not find the generated documents: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("local reset"),
        "the sweep still points at emptying the whole store: {stdout}"
    );
}

/// The note printed when the control plane is unreachable has to name a remedy
/// that works. It used to offer `use local`, which `list` does not read: the
/// user ran it, ran `list` again, and got the same error and the same note.
#[test]
fn the_failure_note_for_list_offers_only_the_flag_that_works() {
    let home = scratch_home("note");
    record_local_node(&home, serve_node(TWO_DOCUMENTS));

    let out = knaix(&home)
        .args(["list"])
        .env("KNAIX_TOKEN", "test-token")
        .output()
        .expect("failed to run knaix");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("A local node is running"),
        "the note did not appear: {stderr}"
    );
    assert!(
        stderr.contains("-n local"),
        "the note did not offer the flag: {stderr}"
    );
    assert!(
        !stderr.contains("use local"),
        "the note still offers a default that list does not read: {stderr}"
    );
}
