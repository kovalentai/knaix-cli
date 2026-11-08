//! `-o json` is declared global, so every command that prints a result has to
//! honour it. These run the binary and parse what it prints, which is the only
//! check that matters to a script piping the output into a parser: a stray
//! banner or a line of text fails here exactly as it would there.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-json-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".knaix")).expect("could not create scratch home");
    dir
}

fn knaix(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("KNAIX_TOKEN")
        .env("KNAIX_NO_UPDATE_CHECK", "1");
    cmd
}

fn write_config(home: &Path, body: &str) {
    fs::write(home.join(".knaix").join("config.json"), body).unwrap();
}

/// Run the command and parse stdout as one JSON document.
fn json_of(cmd: &mut Command) -> serde_json::Value {
    let out = cmd.output().expect("failed to run knaix");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON ({e}):\n{stdout}"))
}

/// A control plane that knows one running node and that node's logs.
fn serve_node_with_logs() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("could not bind");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let body = if path.starts_with("/api/instances") {
                r#"{"data":[{"id":"node-uuid","name":"alpha","state":"running"}]}"#
            } else if path.starts_with("/api/nodes/node-uuid/logs") {
                r#"{"logs":["first line","second line"]}"#
            } else {
                "{}"
            };
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

/// A local node record, so `memory` resolves to the local node without a
/// control plane. Nothing listens on the port, and `memory` never connects.
fn record_local_node(home: &Path) {
    fs::write(
        home.join(".knaix").join("local.json"),
        r#"{"port":9,"instance_id":"json-test","image":"none"}"#,
    )
    .unwrap();
    write_config(
        home,
        r#"{"default_node_id":"local","api_url":"http://127.0.0.1:9"}"#,
    );
}

#[test]
fn config_prints_the_api_url_as_json() {
    let home = scratch_home("config");
    write_config(&home, r#"{"api_url":"https://api.example.test"}"#);

    let v = json_of(knaix(&home).args(["-o", "json", "config"]));
    assert_eq!(v["apiUrl"], "https://api.example.test");
}

#[test]
fn setting_the_api_url_answers_in_json_too() {
    let home = scratch_home("configset");

    let v = json_of(knaix(&home).args([
        "-o",
        "json",
        "config",
        "--api-url",
        "https://set.example.test",
    ]));
    assert_eq!(v["apiUrl"], "https://set.example.test");
}

#[test]
fn config_still_prints_text_by_default() {
    let home = scratch_home("configtext");
    write_config(&home, r#"{"api_url":"https://api.example.test"}"#);

    let out = knaix(&home).arg("config").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Current Configuration"), "{stdout}");
    assert!(stdout.contains("https://api.example.test"), "{stdout}");
}

#[test]
fn memory_lists_note_files_as_json() {
    let home = scratch_home("memory");
    record_local_node(&home);
    let notes = home.join(".knaix").join("memory").join("local");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("b.md"), "second").unwrap();
    fs::write(notes.join("a.md"), "first").unwrap();

    let v = json_of(knaix(&home).args(["-o", "json", "memory"]));
    assert_eq!(v["node"], "local");
    assert_eq!(v["files"], serde_json::json!(["a.md", "b.md"]));
}

#[test]
fn memory_with_no_notes_is_an_empty_list_not_a_sentence() {
    let home = scratch_home("memoryempty");
    record_local_node(&home);

    let v = json_of(knaix(&home).args(["-o", "json", "memory"]));
    assert_eq!(v["files"], serde_json::json!([]));
}

#[test]
fn memory_reads_one_file_as_json() {
    let home = scratch_home("memoryfile");
    record_local_node(&home);
    let notes = home.join(".knaix").join("memory").join("local");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("facts.md"), "# Facts\nThe sky is blue.\n").unwrap();

    let v = json_of(knaix(&home).args(["-o", "json", "memory", "--file", "facts.md"]));
    assert_eq!(v["file"], "facts.md");
    assert_eq!(v["contents"], "# Facts\nThe sky is blue.\n");
}

#[test]
fn logs_from_a_hosted_node_are_a_json_array_of_lines() {
    let home = scratch_home("logs");
    let api = serve_node_with_logs();
    write_config(&home, &format!(r#"{{"api_url":"{api}"}}"#));

    let v = json_of(
        knaix(&home)
            .env("KNAIX_TOKEN", "test-token")
            .args(["-o", "json", "logs", "alpha"]),
    );
    assert_eq!(v["lines"], serde_json::json!(["first line", "second line"]));
}
