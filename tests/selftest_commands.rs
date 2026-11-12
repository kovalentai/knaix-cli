//! `knaix selftest`, run the way a script runs it: through the binary, against
//! a fake control plane or a fake local node that records what it was sent.

mod common;

use common::{
    knaix, nodes, record_local_node, scratch_home, server, server_answering, Run, Server,
};
use serde_json::{json, Value};

/// The bundled corpus: six documents, and this many questions in a full run.
const DOCUMENTS: usize = 6;
const QUESTIONS: usize = 52;

const STREAM: &str = "POST /api/query/answer/stream";
const LOCAL_LIST: &str = "POST /api/kb/documents";
const LOCAL_INGEST: &str = "POST /api/kb/ingest";
const LOCAL_DELETE: &str = "POST /api/kb/delete";

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

/// The same answer to every question, citing a passage that supports none.
fn answer(model: &str) -> String {
    sse(&[
        (
            "meta",
            json!({
                "citations": [{"index": 1, "content": "Nothing relevant.", "metadata": {"source": {"name": "other.md"}}}],
                "model": model,
            }),
        ),
        ("", json!({"token": "I am not sure [1]."})),
        ("done", json!({})),
    ])
}

/// A local node holding `held`, filing each upload as `d-new`.
fn local_node(held: Value, extra: Vec<(&'static str, u16, String)>) -> Server {
    let mut routes = extra;
    routes.extend([
        (LOCAL_LIST, 200, json!({ "documents": held }).to_string()),
        (
            LOCAL_INGEST,
            200,
            json!({"documentId": "d-new", "chunkCount": 3}).to_string(),
        ),
        (LOCAL_DELETE, 200, json!({}).to_string()),
        (STREAM, 200, answer("mock")),
    ]);
    // The first route given wins, so `extra` can replace any of these.
    let mut seen = std::collections::HashSet::new();
    routes.retain(|(r, _, _)| seen.insert(*r));
    server(routes)
}

fn bodies(node: &Server, route: &str) -> Vec<Value> {
    node.calls()
        .into_iter()
        .filter(|c| c.route == route)
        .map(|c| c.body)
        .collect()
}

fn has(run: &Run, expected: &[&str]) {
    for e in expected {
        assert!(
            run.out.contains(e) || run.err.contains(e),
            "missing {e:?} in:\n{}\n{}",
            run.out,
            run.err
        );
    }
}

fn selftest(home: &std::path::Path, args: &[&str]) -> Run {
    let mut all = vec!["selftest", "-n", "local"];
    all.extend(args);
    knaix(home, "http://127.0.0.1:9", &all)
}

#[test]
fn a_quick_run_answers_without_a_verdict_and_removes_what_it_uploaded() {
    let home = scratch_home("selftest-quick");
    let node = local_node(
        json!([{"document_id": "old", "source": "knaix-selftest-0000-legal-msa.md"}]),
        vec![],
    );
    record_local_node(&home, &node.url);

    let run = selftest(&home, &["--quick"]);

    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    has(
        &run,
        &[
            "1 self-test document(s) from an earlier run are on this node; clear them with knaix selftest --sweep.",
            "Self-test on node local (run ",
            "6 synthetic documents, 12 questions (quick subset).",
            "Answer quality: (against the local floors)",
            "A quick run asks 12 of the 52 questions and does not score the node.",
            "This node answered with the deterministic mock",
            "12 question(s) retrieved no supporting passage:",
            "and 2 more",
            "Node answered all 12 questions. Not scored: this was a quick run.",
        ],
    );
    let ingested = bodies(&node, LOCAL_INGEST);
    assert_eq!(ingested.len(), DOCUMENTS);
    let name = ingested[0]["filename"].as_str().unwrap();
    assert!(
        name.starts_with("knaix-selftest-") && name.ends_with("-legal-msa.md"),
        "{name}"
    );
    assert_eq!(ingested[0]["instance_id"], "inst-1");
    assert!(ingested[0]["text"].as_str().unwrap().len() > 100);
    assert_eq!(bodies(&node, STREAM).len(), 12);
    let deleted = bodies(&node, LOCAL_DELETE);
    assert_eq!(deleted.len(), DOCUMENTS, "each upload removed");
    assert!(
        deleted.iter().all(|d| d["document_id"] == "d-new"),
        "the earlier run's document is reported, not removed: {deleted:?}"
    );
}

#[test]
fn a_full_run_below_the_bar_fails_and_reports_why_as_json() {
    let home = scratch_home("selftest-full");
    let node = local_node(json!([]), vec![(STREAM, 200, answer("llama"))]);
    record_local_node(&home, &node.url);

    let run = knaix(
        &home,
        "http://127.0.0.1:9",
        &["-o", "json", "selftest", "-n", "local"],
    );

    assert_eq!(run.code, 1, "{}", run.err);
    assert!(
        run.err.contains(
            "Self-test below the local pass bar: hit-rate 0% (floor 95%), MRR 0.00 (floor 0.90), citation accuracy 0% (floor 85%)"
        ),
        "{}",
        run.err
    );
    let report: Value = serde_json::from_str(&run.out).unwrap();
    assert_eq!(report["questions"], QUESTIONS);
    assert_eq!(report["scored"], true);
    assert_eq!(report["passed"], false);
    assert_eq!(report["model"], "llama");
    assert_eq!(report["citation_accuracy_meaningful"], true);
    assert_eq!(report["floor_class"], "local");
    assert_eq!(report["outcomes"][0]["cited_supporting"], false);
    assert_eq!(bodies(&node, LOCAL_DELETE).len(), DOCUMENTS);

    let human = selftest(&home, &[]);
    assert_eq!(human.code, 1, "{}", human.err);
    has(
        &human,
        &[
            "FAIL",
            "and 42 more",
            "Node fell below the pass bar on 52 questions.",
        ],
    );
    assert!(!human.out.contains("PASS"), "{}", human.out);
    assert_eq!(
        human.out.matches("\n  - [").count(),
        10,
        "the first ten misses"
    );
}

#[test]
fn an_answer_that_cites_nothing_is_not_scored_for_citations() {
    let home = scratch_home("selftest-uncited");
    let uncited = answer("llama").replace(" [1].", ".");
    let node = local_node(json!([]), vec![(STREAM, 200, uncited)]);
    record_local_node(&home, &node.url);

    let run = knaix(
        &home,
        "http://127.0.0.1:9",
        &["-o", "json", "selftest", "-n", "local", "--quick"],
    );

    assert_eq!(run.code, 0, "{}", run.err);
    let report: Value = serde_json::from_str(&run.out).unwrap();
    assert_eq!(report["outcomes"][0]["cited_supporting"], Value::Null);
    assert_eq!(report["outcomes"][0]["answer"], "I am not sure.");
}

#[test]
fn a_node_that_does_not_answer_is_unavailable_not_below_the_bar() {
    let home = scratch_home("selftest-unanswered");
    let node = local_node(
        json!([]),
        vec![(STREAM, 503, json!({"message": "model loading"}).to_string())],
    );
    record_local_node(&home, &node.url);

    let run = selftest(&home, &["--quick"]);

    assert_eq!(run.code, 4, "{}{}", run.out, run.err);
    has(
        &run,
        &[
            "12 question(s) the node never answered:",
            "and 7 more",
            "Node did not answer 12 of 12 questions, so this run does not score it.",
            "Self-test incomplete: the node did not answer 12 of 12 questions.",
        ],
    );
    assert_eq!(
        run.out.matches("\n  - [").count(),
        5,
        "the first five causes"
    );
    assert!(
        !run.out.contains("deterministic mock"),
        "nothing answered, so no model is blamed:\n{}",
        run.out
    );
    assert_eq!(bodies(&node, LOCAL_DELETE).len(), DOCUMENTS);
}

#[test]
fn keep_leaves_the_corpus_and_says_how_to_remove_it() {
    let home = scratch_home("selftest-keep");
    let node = local_node(json!([]), vec![]);
    record_local_node(&home, &node.url);

    let run = selftest(&home, &["--quick", "--keep"]);

    assert_eq!(run.code, 0, "{}", run.err);
    has(
        &run,
        &["Keeping 6 self-test document(s) on the node (--keep). Remove them with knaix selftest --sweep."],
    );
    assert!(bodies(&node, LOCAL_DELETE).is_empty());
}

#[test]
fn sweep_removes_what_an_earlier_run_left_and_tests_nothing() {
    let home = scratch_home("selftest-sweep");
    let node = local_node(
        json!([
            {"document_id": "old-1", "source": "knaix-selftest-0000-legal-msa.md"},
            {"document_id": "old-2", "source": "knaix-selftest-0000-legal-dpa.md"},
            {"document_id": "mine", "source": "house.md"},
            {"document_id": "unnamed"},
        ]),
        vec![],
    );
    record_local_node(&home, &node.url);

    let run = selftest(&home, &["--sweep"]);

    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    has(
        &run,
        &["Removed 2 self-test document(s) left by an earlier run."],
    );
    let deleted: Vec<Value> = bodies(&node, LOCAL_DELETE)
        .into_iter()
        .map(|d| d["document_id"].clone())
        .collect();
    assert_eq!(deleted, vec![json!("old-1"), json!("old-2")]);
    assert!(bodies(&node, LOCAL_INGEST).is_empty(), "nothing tested");
    assert!(bodies(&node, STREAM).is_empty(), "nothing asked");

    let both = selftest(&home, &["--sweep", "--quick"]);
    assert_eq!(
        both.code, 2,
        "a sweep runs no test to shorten: {}",
        both.err
    );
}

#[test]
fn a_sweep_or_cleanup_that_cannot_delete_says_so() {
    let home = scratch_home("selftest-undeleted");
    let node = local_node(
        json!([{"document_id": "old-1", "source": "knaix-selftest-0000-legal-msa.md"}]),
        vec![(LOCAL_DELETE, 500, json!({}).to_string())],
    );
    record_local_node(&home, &node.url);

    let sweep = selftest(&home, &["--sweep"]);
    assert_eq!(sweep.code, 1, "{}", sweep.out);
    has(&sweep, &["Could not remove 1 of 1 self-test document(s)"]);

    let run = selftest(&home, &["--quick"]);
    assert_eq!(run.code, 0, "the measurement stands: {}", run.err);
    assert!(
        run.err.contains(
            "Could not remove 6 self-test document(s) from the node. They are named 'knaix-selftest-*'; remove them with knaix selftest --sweep."
        ),
        "{}",
        run.err
    );
}

#[test]
fn an_ingest_refused_part_way_removes_what_it_filed_and_fails() {
    let home = scratch_home("selftest-ingest");
    // Two documents are filed, then the third is refused.
    let filed = std::sync::atomic::AtomicUsize::new(0);
    let node = server_answering(move |call| match call.route.as_str() {
        LOCAL_LIST => (200, json!({"documents": []}).to_string()),
        LOCAL_INGEST => {
            let n = filed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                (200, json!({ "documentId": format!("d-{n}") }).to_string())
            } else {
                (413, json!({"message": "too large"}).to_string())
            }
        }
        LOCAL_DELETE => (200, "{}".to_string()),
        _ => (404, String::new()),
    });
    record_local_node(&home, &node.url);

    let run = selftest(&home, &[]);

    assert_eq!(run.code, 1, "{}", run.out);
    has(
        &run,
        &["-finance-expense.md: HTTP 413 Payload Too Large - too large"],
    );
    assert_eq!(bodies(&node, LOCAL_INGEST).len(), 3, "stops at the refusal");
    assert!(bodies(&node, STREAM).is_empty());
    let deleted: Vec<Value> = bodies(&node, LOCAL_DELETE)
        .into_iter()
        .map(|d| d["document_id"].clone())
        .collect();
    assert_eq!(
        deleted,
        vec![json!("d-0"), json!("d-1")],
        "what it filed, and only that"
    );
}

const LIST: &str = "GET /api/knowledge/u-1/documents";
const UPLOAD: &str = "POST /api/knowledge/u-1/documents";
const CHAT: &str = "POST /api/nodes/u-1/native-chat";

fn alpha() -> (&'static str, u16, String) {
    nodes(json!([{"id": "u-1", "name": "alpha", "state": "running"}]))
}

#[test]
fn a_hosted_run_uploads_asks_and_deletes_through_the_control_plane() {
    let home = scratch_home("selftest-hosted");
    let api = server(vec![
        alpha(),
        (
            LIST,
            200,
            json!({"data": [
                {"id": "old", "source": {"name": "knaix-selftest-0000-legal-msa.md"}},
                {"id": "mine", "source": {"name": "house.md"}},
                {"source": {"name": "knaix-selftest-0000-no-id.md"}},
            ]})
            .to_string(),
        ),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"documentId": "h-new"}}).to_string(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/h-new",
            204,
            String::new(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/old",
            204,
            String::new(),
        ),
        (
            CHAT,
            200,
            sse(&[
                (
                    "meta",
                    json!({"citations": [{"index": 1, "content": "Nothing.", "source": {"name": "x.md"}}], "model": "gpt-x"}),
                ),
                ("", json!({"token": "No [1]."})),
                ("done", json!({"citedIndexes": [1]})),
            ]),
        ),
    ]);

    let run = knaix(&home, &api.url, &["selftest", "-n", "alpha", "--quick"]);

    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    has(
        &run,
        &[
            "Self-test on node u-1",
            "(against the hosted floors)",
            "1 self-test document(s) from an earlier run",
        ],
    );
    assert!(
        !run.out.contains("deterministic mock"),
        "a real model answered"
    );
    let calls = api.calls();
    let uploads: Vec<_> = calls.iter().filter(|c| c.route == UPLOAD).collect();
    assert_eq!(uploads.len(), DOCUMENTS);
    assert_eq!(uploads[0].auth.as_deref(), Some("Bearer sess"));
    assert!(
        uploads[0].raw.contains("Content-Type: text/markdown"),
        "{}",
        uploads[0].raw
    );
    let deletes: Vec<_> = calls
        .iter()
        .filter(|c| c.route.starts_with("DELETE"))
        .map(|c| c.route.as_str())
        .collect();
    assert_eq!(
        deletes,
        vec!["DELETE /api/knowledge/u-1/documents/h-new"; DOCUMENTS]
    );

    let sweep = knaix(&home, &api.url, &["selftest", "-n", "alpha", "--sweep"]);
    assert_eq!(sweep.code, 0, "{}", sweep.err);
    has(
        &sweep,
        &["Removed 1 self-test document(s) left by an earlier run."],
    );
}

#[test]
fn a_hosted_refusal_exits_with_the_code_its_status_means() {
    let home = scratch_home("selftest-hosted-refused");
    let expired = server(vec![
        alpha(),
        (LIST, 401, json!({"error": "expired"}).to_string()),
    ]);
    let run = knaix(&home, &expired.url, &["selftest", "-n", "alpha"]);
    assert_eq!(run.code, 3, "{}", run.err);
    has(&run, &["Failed to list documents: HTTP 401 Unauthorized"]);

    let refused = server(vec![
        alpha(),
        (LIST, 200, json!({"data": []}).to_string()),
        (
            UPLOAD,
            200,
            json!({"success": false, "error": "quota reached"}).to_string(),
        ),
    ]);
    let run = knaix(&home, &refused.url, &["selftest", "-n", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.err);
    has(&run, &["-legal-msa.md: HTTP 200 OK - quota reached"]);

    let limited = server(vec![
        alpha(),
        (LIST, 200, json!({"data": []}).to_string()),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"documentId": "h-new"}}).to_string(),
        ),
        (CHAT, 429, json!({"error": "slow down"}).to_string()),
    ]);
    let run = knaix(&home, &limited.url, &["selftest", "-n", "alpha", "--quick"]);
    assert_eq!(run.code, 4, "{}", run.err);
    has(&run, &["Rate limited part-way through the run."]);
    let asked = limited.calls().iter().filter(|c| c.route == CHAT).count();
    assert_eq!(
        asked, 1,
        "a rate limit stops the run rather than spending more of it"
    );
    assert!(
        !run.out.contains("Answer quality"),
        "no report on a run that stopped"
    );
    let deletes = limited
        .calls()
        .iter()
        .filter(|c| c.route == "DELETE /api/knowledge/u-1/documents/h-new")
        .count();
    assert_eq!(deletes, DOCUMENTS, "a run that stopped still cleans up");
    has(
        &run,
        &["Could not remove 6 self-test document(s) from the node."],
    );
}

/// A node that answers each question from the passage that supports it, as a
/// node with a good retriever and a model that cites it would.
#[test]
fn a_node_that_finds_and_cites_every_passage_passes() {
    let home = scratch_home("selftest-pass");
    let questions: Vec<Value> =
        serde_json::from_str(include_str!("../assets/selftest/questions.json")).unwrap();
    let node = server_answering(move |call| match call.route.as_str() {
        LOCAL_LIST => (200, json!({"documents": []}).to_string()),
        LOCAL_INGEST => (200, json!({"documentId": "d-new"}).to_string()),
        LOCAL_DELETE => (200, "{}".to_string()),
        STREAM => {
            let q = questions
                .iter()
                .find(|q| q["query"] == call.body["query"])
                .expect("one of the bundled questions");
            let needles: Vec<&str> = q["goldNeedles"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap())
                .collect();
            let source = format!(
                "knaix-selftest-0000-{}.md",
                q["goldSourceKeys"][0].as_str().unwrap()
            );
            let body = sse(&[
                (
                    "meta",
                    json!({
                        // Listed out of order: rank is the [n] marker, not the position.
                        "citations": [
                            {"index": 2, "content": "Unrelated.", "metadata": {"source": {"name": "other.md"}}},
                            {"index": 1, "content": needles.join(" "), "metadata": {"source": {"name": source}}},
                        ],
                        "model": "llama",
                    }),
                ),
                ("", json!({"token": "As the policy says [1]."})),
                ("done", json!({})),
            ]);
            (200, body)
        }
        _ => (404, String::new()),
    });
    record_local_node(&home, &node.url);

    let run = selftest(&home, &[]);

    assert_eq!(run.code, 0, "{}{}", run.out, run.err);
    has(
        &run,
        &[
            "Hit rate @5",
            "100.0%",
            "1.000",
            "PASS",
            "Node answered 52 questions at or above every floor.",
        ],
    );
    assert!(
        !run.out.contains("FAIL") && !run.out.contains("Missed:"),
        "{}",
        run.out
    );
    assert_eq!(bodies(&node, STREAM).len(), QUESTIONS);
}
