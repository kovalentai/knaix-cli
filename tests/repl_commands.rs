//! `knaix repl`, fed a script on standard input the way a pipe feeds it: the
//! session reads one line at a time, with no terminal, until the input ends.

mod common;

use base64::Engine;
use common::{knaix_reading, nodes, record_local_node, scratch_home, server, Run, Server};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Stdio;

const CHAT: &str = "POST /api/nodes/u-1/native-chat";
const UPLOAD: &str = "POST /api/knowledge/u-1/documents";

fn alpha() -> (&'static str, u16, String) {
    nodes(json!([{"id": "u-1", "name": "alpha", "state": "running"}]))
}

/// An event stream: each frame on its own lines, ended by a blank one.
fn sse(frames: &[(&str, Value)]) -> String {
    frames
        .iter()
        .map(|(event, data)| match *event {
            "" => format!("data: {data}\n\n"),
            event => format!("event: {event}\ndata: {data}\n\n"),
        })
        .collect()
}

/// A hosted answer citing the first of two passages, in thread `thread`.
fn hosted_answer(thread: Option<&str>) -> String {
    let mut meta = json!({
        "citations": [
            {"index": 1, "content": "Receipts are due within 30 days of travel.", "source": {"name": "policy.md"}},
            {"index": 2, "content": "Cabin class is economy.\nBusiness for flights over 8 hours.", "source": {"name": "travel.md"}},
        ],
        "model": "gpt-x",
    });
    if let Some(thread) = thread {
        meta["conversationId"] = json!(thread);
    }
    sse(&[
        ("meta", meta),
        ("", json!({"token": "Within 30 days [1]."})),
        ("done", json!({"citedIndexes": [1]})),
    ])
}

/// `knaix repl <args>` with `script` as what is typed, one line each.
fn repl(home: &Path, api: &str, args: &[&str], script: &[&str]) -> Run {
    let typed = home.join("typed.txt");
    fs::write(&typed, script.join("\n") + "\n").unwrap();
    let mut all = vec!["repl"];
    all.extend(args);
    knaix_reading(
        home,
        api,
        &all,
        Stdio::from(fs::File::open(&typed).unwrap()),
    )
}

fn has(run: &Run, expected: &[&str]) {
    for e in expected {
        assert!(
            run.out.contains(e),
            "missing {e:?} in:\n{}{}",
            run.out,
            run.err
        );
    }
}

fn asked(api: &Server) -> Vec<Value> {
    api.calls()
        .into_iter()
        .filter(|c| c.route == CHAT)
        .map(|c| c.body)
        .collect()
}

#[test]
fn a_hosted_session_follows_its_thread_until_reset() {
    let home = scratch_home("repl-hosted");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer(Some("c-9")))]);

    let run = repl(
        &home,
        &api.url,
        &["-n", "alpha", "--brief"],
        &[
            "",
            "/source 1",
            "/reset",
            "when are receipts due?",
            "and for cabin class?",
            "/reset",
            "a fresh question",
            "/exit",
            "never sent",
        ],
    );

    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    has(
        &run,
        &[
            "session with u-1. /help lists commands; /exit ends the session.",
            "Info: Nothing to show yet. Ask a question first.",
            "Info: Nothing to clear yet.",
            "Within 30 days [1].",
            "[1] policy.md",
            "Tip: /source <n> prints a passage in full.",
            "Starting a new conversation. Earlier questions leave the context; the previous conversation is kept.",
            "Session closed (3 messages sent).",
        ],
    );
    assert_eq!(run.out.matches("Tip: /source").count(), 1, "said once");
    let bodies = asked(&api);
    assert_eq!(bodies.len(), 3, "the line after /exit is not sent");
    assert_eq!(
        bodies[0],
        json!({"message": "when are receipts due?", "stream": true, "verbosity": "brief"})
    );
    assert_eq!(bodies[1]["message"], "and for cabin class?");
    assert_eq!(bodies[1]["conversationId"], "c-9", "the thread is followed");
    assert_eq!(
        bodies[2].get("conversationId"),
        None,
        "reset starts a new one"
    );
}

#[test]
fn source_prints_a_passage_whole_and_says_which_it_cannot() {
    let home = scratch_home("repl-source");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer(Some("c-9")))]);

    let run = repl(
        &home,
        &api.url,
        &["-n", "alpha"],
        &["q", "/source 2", "/source 9", "/source two", "/quit"],
    );

    assert_eq!(run.code, 0, "{}", run.err);
    has(
        &run,
        &[
            "[2] travel.md",
            "Retrieved for this question, but the answer did not cite it.",
            "Cabin class is economy.\nBusiness for flights over 8 hours.",
            "Error: No passage [9] in the last answer. It retrieved 1, 2.",
            "Error: Usage: /source <n>, the number in a [n] marker.",
            "Session closed (1 message sent).",
        ],
    );
}

#[test]
fn the_session_commands_change_what_is_asked_and_refuse_what_they_cannot_do() {
    let home = scratch_home("repl-commands");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer(Some("c-9")))]);

    let run = repl(
        &home,
        &api.url,
        &["-n", "alpha", "--k", "4"],
        &[
            "/help",
            "/k 5",
            "/k many",
            "/doc house.md",
            "/detailed",
            "first",
            "/normal",
            "second",
            "/frobnicate now",
            "/remember",
        ],
    );

    assert_eq!(run.code, 0, "{}", run.err);
    has(
        &run,
        &[
            "Note: --k applies to a local node; a hosted node's retrieval is set by the control plane.",
            "REPL commands:",
            "\n  /help                      Show this help\n",
            "\n  /brief, /normal, /detailed Set how much detail answers carry\n",
            "Note: /k applies to a local node; a hosted node's retrieval is set by the control plane.",
            "Error: Usage: /k <n>, between 1 and",
            "Note: /doc applies to a local node; a hosted node's retrieval is set by the control plane.",
            "Answers are now detailed.",
            "Answers are now normal.",
            "Error: Unknown command /frobnicate. /help lists commands; a message without a leading '/' is sent to the node.",
            "Error: Usage: /remember <fact>",
            "Session ended.",
        ],
    );
    let bodies = asked(&api);
    assert_eq!(bodies[0]["verbosity"], "detailed");
    assert_eq!(bodies[1]["verbosity"], "normal");
}

const STREAM: &str = "POST /api/query/answer/stream";
const INGEST: &str = "POST /api/kb/ingest";

fn local_answer(text: &str) -> String {
    sse(&[
        (
            "meta",
            json!({
                "citations": [{"index": 1, "content": "Checkout is at 11.", "metadata": {"source": {"name": "house.md"}}}],
                "model": "llama",
            }),
        ),
        ("", json!({"token": text})),
        ("done", json!({})),
    ])
}

/// A local node holding house.md and menu.md, answering every question.
fn local_node(ingest: u16) -> Server {
    server(vec![
        (STREAM, 200, local_answer("Checkout is at 11 [1].")),
        (
            "POST /api/kb/documents",
            200,
            json!({"documents": [
                {"document_id": "d-1", "source": "house.md"},
                {"document_id": "d-2", "source": "menu.md"},
            ]})
            .to_string(),
        ),
        (INGEST, ingest, json!({"chunkCount": 1}).to_string()),
    ])
}

fn sent(node: &Server, route: &str) -> Vec<Value> {
    node.calls()
        .into_iter()
        .filter(|c| c.route == route)
        .map(|c| c.body)
        .collect()
}

#[test]
fn a_local_session_scopes_its_answers_and_carries_its_history() {
    let home = scratch_home("repl-local");
    let node = local_node(200);
    record_local_node(&home, &node.url);

    let run = repl(
        &home,
        "http://127.0.0.1:9",
        &["-n", "local"],
        &[
            "/k 0",
            "/k 7",
            "/doc nothing",
            "/doc house.md",
            "when is checkout?",
            "/doc",
            "and breakfast?",
            "/reset",
            "/reset",
        ],
    );

    assert_eq!(run.code, 0, "{}", run.err);
    has(
        &run,
        &[
            "session with local.",
            "Error: --k must be between 1 and 50",
            "Answers are now grounded in up to 7 passages.",
            "Error: No document matching 'nothing'. This node holds:",
            "Answers are now grounded in house.md.",
            "Checkout is at 11 [1].",
            "Answers are grounded in a search of the whole corpus again.",
            "Starting fresh. Earlier questions leave the context.",
            "Info: Nothing to clear yet.",
            "Session closed (2 messages sent).",
        ],
    );
    assert!(
        !run.out.contains("was not added to the conversation"),
        "a local node keeps its context in the history it is sent"
    );
    let bodies = sent(&node, STREAM);
    assert_eq!(bodies[0]["query"], "when is checkout?");
    assert_eq!(bodies[0]["policy"]["k"], 7);
    assert_eq!(bodies[0]["policy"]["document_ids"], json!(["d-1"]));
    assert_eq!(bodies[0]["history"], json!([]));
    assert_eq!(bodies[1]["policy"].get("document_ids"), None);
    assert_eq!(
        bodies[1]["history"],
        json!([
            {"role": "user", "content": "when is checkout?"},
            {"role": "assistant", "content": "Checkout is at 11 [1]."},
        ])
    );
}

fn notes(home: &Path, node: &str) -> String {
    fs::read_to_string(
        home.join(".knaix/memory")
            .join(node)
            .join("_knaix_durable_memory.md"),
    )
    .unwrap()
}

#[test]
fn remember_saves_a_note_and_says_whether_the_local_node_took_it() {
    let home = scratch_home("repl-remember-local");
    let node = local_node(200);
    record_local_node(&home, &node.url);

    let run = repl(
        &home,
        "http://127.0.0.1:9",
        &["-n", "local"],
        &["/remember Pets are welcome", "/memory"],
    );

    assert_eq!(run.code, 0, "{}", run.err);
    has(
        &run,
        &[
            "Noted. Saved to",
            "and ingested, so later questions can retrieve it.",
            "_knaix_durable_memory.md",
        ],
    );
    let saved = notes(&home, "local");
    assert!(
        saved.starts_with("# Notes saved from the Knaix REPL\n\n- [TS "),
        "{saved}"
    );
    assert!(saved.ends_with("]: Pets are welcome\n"), "{saved}");
    let ingested = &sent(&node, INGEST)[0];
    assert_eq!(ingested["instance_id"], "inst-1");
    assert_eq!(ingested["filename"], "_knaix_durable_memory.md");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(ingested["content_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, saved.as_bytes(), "the whole notes file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = home.join(".knaix/memory/local");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("_knaix_durable_memory.md")), 0o600);
    }

    let refusing = local_node(500);
    record_local_node(&home, &refusing.url);
    let run = repl(
        &home,
        "http://127.0.0.1:9",
        &["-n", "local"],
        &["/remember Checkout is at 11"],
    );
    has(
        &run,
        &["but the node did not ingest it (HTTP 500 Internal Server Error); it will not surface in answers."],
    );
    assert_eq!(
        notes(&home, "local").lines().count(),
        4,
        "appended, not rewritten"
    );
}

#[test]
fn remember_on_a_hosted_node_uploads_the_notes_file() {
    let home = scratch_home("repl-remember-hosted");
    let api = server(vec![
        alpha(),
        (UPLOAD, 200, json!({"success": true}).to_string()),
    ]);

    let run = repl(
        &home,
        &api.url,
        &["-n", "alpha"],
        &["/remember Receipts within 30 days"],
    );

    has(&run, &["and ingested, so later questions can retrieve it."]);
    let upload = api.calls().into_iter().find(|c| c.route == UPLOAD).unwrap();
    assert_eq!(upload.auth.as_deref(), Some("Bearer sess"));
    assert!(
        upload.raw.contains("filename=\"_knaix_durable_memory.md\""),
        "{}",
        upload.raw
    );
    assert!(
        upload.raw.contains("]: Receipts within 30 days\n"),
        "{}",
        upload.raw
    );

    for (status, body, said) in [
        (
            200,
            json!({"success": false, "error": "quota"}),
            "Upload failed: quota",
        ),
        (200, json!({}), "Upload failed: Unknown"),
        (413, json!({}), "HTTP 413 Payload Too Large"),
    ] {
        let api = server(vec![alpha(), (UPLOAD, status, body.to_string())]);
        let run = repl(&home, &api.url, &["-n", "alpha"], &["/remember x"]);
        has(&run, &[&format!("but the node did not ingest it ({said})")]);
    }
}

#[test]
fn a_turn_that_fails_says_so_and_leaves_the_session_going() {
    let home = scratch_home("repl-failing");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer(None))]);
    let run = repl(&home, &api.url, &["-n", "alpha"], &["one", "two"]);
    has(&run, &["Session closed (2 messages sent)."]);
    assert_eq!(
        run.out
            .matches("That answer was not added to the conversation")
            .count(),
        1,
        "said once a session:\n{}",
        run.out
    );

    let empty = server(vec![
        alpha(),
        (
            CHAT,
            200,
            sse(&[("meta", json!({"citations": []})), ("done", json!({}))]),
        ),
    ]);
    let run = repl(&home, &empty.url, &["-n", "alpha"], &["anyone?"]);
    has(&run, &["Warning: Node returned an empty response."]);

    let refusing = server(vec![
        alpha(),
        (CHAT, 500, json!({"error": "down"}).to_string()),
    ]);
    let run = repl(&home, &refusing.url, &["-n", "alpha"], &["hello", "/exit"]);
    assert_eq!(run.code, 0, "a failed turn does not end the session");
    has(
        &run,
        &["Error: Chat failed on node: HTTP 500 Internal Server Error - down"],
    );
}

#[test]
fn a_node_id_that_is_a_path_is_refused_before_a_note_can_land_outside_memory() {
    let home = scratch_home("repl-escape");
    let api = server(vec![
        nodes(json!([{"id": "../../outside", "name": "alpha", "state": "running"}])),
        (
            "POST /api/knowledge/../../outside/documents",
            200,
            json!({"success": true}).to_string(),
        ),
    ]);

    let run = repl(&home, &api.url, &["-n", "alpha"], &["/remember a secret"]);

    assert_ne!(run.code, 0, "{}", run.out);
    assert!(
        run.err
            .contains("Node alpha has an id that is not a plain name: \"../../outside\""),
        "{}",
        run.err
    );
    assert!(
        !home.join("outside").exists(),
        "nothing written outside ~/.knaix/memory"
    );
}

#[test]
fn input_that_is_not_text_ends_the_session_as_a_failure() {
    let home = scratch_home("repl-garbled");
    let api = server(vec![alpha()]);
    let typed = home.join("typed.bin");
    fs::write(&typed, b"\xff\xfe\n").unwrap();

    let run = knaix_reading(
        &home,
        &api.url,
        &["repl", "-n", "alpha"],
        Stdio::from(fs::File::open(&typed).unwrap()),
    );

    assert_eq!(run.code, 1, "{}", run.out);
    assert!(
        run.err
            .contains("Could not read the next line: stream did not contain valid UTF-8"),
        "{}",
        run.err
    );
    assert!(!run.out.contains("Session closed"), "{}", run.out);
}

#[test]
fn a_note_that_cannot_be_saved_is_not_sent_and_says_why() {
    let home = scratch_home("repl-unsaved");
    fs::create_dir_all(home.join(".knaix/memory")).unwrap();
    fs::write(
        home.join(".knaix/memory/u-1"),
        "a file where the directory goes",
    )
    .unwrap();
    let api = server(vec![
        alpha(),
        (UPLOAD, 200, json!({"success": true}).to_string()),
    ]);

    let run = repl(
        &home,
        &api.url,
        &["-n", "alpha"],
        &["/remember x", "/memory"],
    );

    assert_eq!(run.code, 0, "{}", run.err);
    has(
        &run,
        &["Error: Could not save the note: Failed to create memory directory"],
    );
    assert!(!run.out.contains("Noted."), "{}", run.out);
    assert!(
        api.calls().iter().all(|c| c.route != UPLOAD),
        "nothing uploaded"
    );
    assert_eq!(
        run.out.matches("Error:").count(),
        2,
        "/memory says it could not list the notes too:\n{}",
        run.out
    );
}
