//! `knaix chat`, run the way a script runs it: through the binary, against a
//! fake control plane or a fake local node on loopback, with no terminal.

mod common;

use common::{knaix, knaix_reading, nodes, record_local_node, scratch_home, server, INSTANCES};
use serde_json::{json, Value};
use std::fs;
use std::process::Stdio;

#[test]
fn with_several_nodes_and_no_default_a_script_is_told_to_name_one() {
    let home = scratch_home("chat-several");
    let api = server(vec![nodes(json!([
        {"id": "u-1", "name": "alpha", "state": "running"},
        {"id": "u-2", "name": "beta", "state": "running"},
    ]))]);

    let chat = knaix(&home, &api.url, &["chat", "what is due?"]);

    assert_eq!(chat.code, 2, "{}{}", chat.out, chat.err);
    assert!(
        chat.err.contains(
            "Choose a node to ask: you have 2 (alpha, beta). Pass -n <name>, or run 'knaix use <name>' to make one the default."
        ),
        "{}",
        chat.err
    );
    for call in api.calls() {
        assert_eq!(call.route, INSTANCES, "only the list was asked for");
        assert_eq!(call.auth.as_deref(), Some("Bearer sess"));
        assert_eq!(call.body, Value::Null);
    }
}

const CHAT: &str = "POST /api/nodes/u-1/native-chat";

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

fn hosted_answer(model: &str) -> String {
    sse(&[
        (
            "meta",
            json!({
                "citations": [
                    {"index": 1, "content": "Receipts are due within 30 days of travel.", "source": {"name": "policy.md"}},
                    {"index": 2, "content": "Cabin class is economy.", "source": {"name": "travel.md"}},
                ],
                "model": model,
                "conversationId": "c-9",
                "scope": {"chunksUsed": 3, "chunksTotal": 10},
            }),
        ),
        ("", json!({"token": "Within 30 days [1]."})),
        ("done", json!({"citedIndexes": [1]})),
    ])
}

#[test]
fn a_hosted_answer_prints_with_what_it_was_grounded_in() {
    let home = scratch_home("chat-hosted");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer("mock"))]);

    let chat = knaix(
        &home,
        &api.url,
        &[
            "chat",
            "-n",
            "alpha",
            "--brief",
            "--k",
            "5",
            "when are receipts due?",
        ],
    );

    assert_eq!(chat.code, 0, "{}", chat.err);
    for expected in [
        "--k applies to a local node; a hosted node's retrieval is set by the control plane.",
        "AI: Within 30 days [1].",
        "Grounded in:",
        "[1] policy.md",
        "Receipts are due within 30 days of travel.",
        "Grounded in the first 3 of 10 passages; the rest did not fit.",
        "Mock answer: the wording is synthetic; retrieval and citations are real. No model is configured on this node.",
    ] {
        assert!(chat.out.contains(expected), "missing {expected:?} in:\n{}", chat.out);
    }
    assert!(!chat.out.contains("travel.md"), "retrieved but not cited");
    let asked = api.calls().into_iter().find(|c| c.route == CHAT).unwrap();
    assert_eq!(asked.auth.as_deref(), Some("Bearer sess"));
    assert_eq!(
        asked.body,
        json!({"message": "when are receipts due?", "stream": true, "verbosity": "brief"})
    );
}

#[test]
fn a_hosted_answer_as_json_carries_everything_a_script_needs() {
    let home = scratch_home("chat-hosted-json");
    let api = server(vec![alpha(), (CHAT, 200, hosted_answer("gpt-x"))]);

    let chat = knaix(
        &home,
        &api.url,
        &["-o", "json", "chat", "-n", "alpha", "--detailed", "when?"],
    );

    assert_eq!(chat.code, 0, "{}", chat.err);
    let answer: Value = serde_json::from_str(&chat.out).unwrap();
    assert_eq!(answer["answer"], "Within 30 days [1].");
    assert_eq!(answer["model"], "gpt-x");
    assert_eq!(answer["conversationId"], "c-9");
    assert_eq!(answer["scope"], json!({"chunksUsed": 3, "chunksTotal": 10}));
    assert_eq!(answer["citations"][0]["cited"], true);
    assert_eq!(answer["citations"][1]["cited"], false);
    assert!(answer["firstTokenMs"].is_u64() && answer["totalMs"].is_u64());
    let asked = api.calls().into_iter().find(|c| c.route == CHAT).unwrap();
    assert_eq!(asked.body["verbosity"], "detailed");
}

#[test]
fn a_hosted_refusal_exits_with_its_code_and_scoping_is_for_a_local_node() {
    let home = scratch_home("chat-hosted-refused");
    let api = server(vec![
        alpha(),
        (CHAT, 403, json!({"error": "no access"}).to_string()),
    ]);

    let refused = knaix(&home, &api.url, &["chat", "-n", "alpha", "q"]);
    assert_eq!(refused.code, 3, "{}", refused.err);
    assert!(
        refused
            .err
            .contains("Chat failed on node: HTTP 403 Forbidden - no access"),
        "{}",
        refused.err
    );

    let scoped = knaix(
        &home,
        &api.url,
        &["chat", "-n", "alpha", "--doc", "policy", "q"],
    );
    assert_eq!(scoped.code, 2, "{}", scoped.err);
    assert!(
        scoped.err.contains("--doc applies to a local node"),
        "{}",
        scoped.err
    );

    let deep = knaix(&home, &api.url, &["chat", "-n", "alpha", "--k", "0", "q"]);
    assert_eq!(deep.code, 2, "{}", deep.err);
}

const STREAM: &str = "POST /api/query/answer/stream";
const DOCUMENTS: &str = "POST /api/kb/documents";

fn local_answer() -> String {
    sse(&[
        (
            "meta",
            json!({
                "citations": [
                    {"index": 1, "content": "Pets are welcome.", "metadata": {"source": {"name": "_knaix_durable_memory.md"}}},
                    {"index": 2, "content": "Checkout is at 11.", "metadata": {"source": {"name": "house.md"}}},
                ],
                "model": "mock",
                "scope": {"chunksUsed": 4, "chunksTotal": 4},
            }),
        ),
        (
            "",
            json!({"token": "Checkout is at 11 [2], pets welcome [1]."}),
        ),
        ("done", json!({})),
    ])
}

#[test]
fn a_local_answer_scoped_to_a_document_reads_that_document() {
    let home = scratch_home("chat-local");
    let node = server(vec![
        (STREAM, 200, local_answer()),
        (
            DOCUMENTS,
            200,
            json!({"documents": [
                {"document_id": "d-1", "source": "house.md"},
                {"document_id": "d-2", "source": "menu.md"},
            ]})
            .to_string(),
        ),
    ]);
    record_local_node(&home, &node.url);
    let question = home.join("question.txt");
    fs::write(&question, "when is checkout?").unwrap();

    let chat = knaix_reading(
        &home,
        "http://127.0.0.1:9",
        &[
            "chat", "-n", "local", "--doc", "house", "--doc", "house.md", "--k", "3", "-",
        ],
        Stdio::from(fs::File::open(&question).unwrap()),
    );

    let out = &chat.out;
    assert_eq!(chat.code, 0, "{out}{}", chat.err);
    for expected in [
        "--k has no effect with --doc",
        "AI: Checkout is at 11 [2], pets welcome [1].",
        "[1] your saved note (/remember)",
        "[2] house.md",
        "Mock answer: the wording is synthetic; retrieval and citations are real. 'knaix local setup' connects a model.",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
    assert!(
        !out.contains("Grounded in the first"),
        "the whole document fitted"
    );
    let asked = node
        .calls()
        .into_iter()
        .find(|c| c.route == STREAM)
        .unwrap();
    assert_eq!(
        asked.body["query"], "when is checkout?",
        "read from standard input"
    );
    assert_eq!(asked.body["instance_id"], "inst-1");
    assert_eq!(
        asked.body["policy"]["document_ids"],
        json!(["d-1"]),
        "named twice, asked once: {}",
        asked.body
    );
}

#[test]
fn a_local_answer_as_json_has_no_thread_and_a_down_node_is_an_error() {
    let home = scratch_home("chat-local-json");
    let node = server(vec![(STREAM, 200, local_answer())]);
    record_local_node(&home, &node.url);

    let chat = knaix(
        &home,
        "http://127.0.0.1:9",
        &["-o", "json", "chat", "-n", "local", "q"],
    );
    assert_eq!(chat.code, 0, "{}", chat.err);
    let answer: Value = serde_json::from_str(&chat.out).unwrap();
    assert_eq!(answer["conversationId"], Value::Null);
    assert_eq!(answer["model"], "mock");
    assert_eq!(answer["citations"][1]["cited"], true);

    record_local_node(&home, "http://127.0.0.1:9");
    let down = knaix(&home, "http://127.0.0.1:9", &["chat", "-n", "local", "q"]);
    assert_eq!(down.code, 4, "unavailable: {}", down.out);
    assert!(
        down.err.contains("Could not reach the local node."),
        "{}",
        down.err
    );
}
