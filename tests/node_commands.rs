//! Listing nodes, their health and logs, and starting one, run the way a
//! script runs them: through the binary, against a fake control plane or a
//! fake local node on loopback, with no terminal.

mod common;

use common::{knaix, knaix_signed_out, nodes, record_local_node, scratch_home, server};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const PROVISION: &str = "POST /api/instances";

#[test]
fn up_refused_exits_with_the_code_its_status_means() {
    let home = scratch_home("nodes-up-refused");
    for (status, code) in [(401, 3), (403, 3), (402, 1)] {
        let api = server(vec![(
            PROVISION,
            status,
            json!({"error": "no"}).to_string(),
        )]);

        let up = knaix(&home, &api.url, &["up"]);

        assert_eq!(up.code, code, "HTTP {status}: {}", up.err);
        assert!(
            up.err.contains("Failed to provision node: HTTP"),
            "{}",
            up.err
        );
        assert!(
            up.err.contains(" - no"),
            "the control plane's reason: {}",
            up.err
        );
    }
}

#[test]
fn up_names_the_node_it_requested_when_the_control_plane_does() {
    let home = scratch_home("nodes-up");
    let api = server(vec![(
        PROVISION,
        201,
        json!({"data": {"instanceId": "i-9"}}).to_string(),
    )]);

    let up = knaix(&home, &api.url, &["up"]);

    assert_eq!(up.code, 0, "{}", up.err);
    assert!(
        up.out.contains(
            "Boot takes a few minutes. Check on it with knaix list or knaix metrics i-9."
        ),
        "{}",
        up.out
    );
    let asked = &api.calls()[0];
    assert_eq!(asked.auth.as_deref(), Some("Bearer sess"));
    assert_eq!(asked.body, json!({}), "the control plane picks the tier");

    let unnamed = server(vec![(
        PROVISION,
        202,
        json!({"data": {"status": "queued"}}).to_string(),
    )]);
    let up = knaix(&home, &unnamed.url, &["up"]);
    assert!(
        up.out.contains("Run knaix list to see it come up."),
        "{}",
        up.out
    );

    let up = knaix(&home, "http://127.0.0.1:9", &["up"]);
    assert_eq!(up.code, 4, "{}", up.err);
    assert!(
        up.err
            .contains("Networking error during provisioning request"),
        "{}",
        up.err
    );
}

fn two_nodes() -> (&'static str, u16, String) {
    nodes(json!([
        {"id": "u-1", "name": "alpha", "state": "running", "instanceId": "i-1", "privateIp": "100.64.0.7", "model": "llama3"},
        {"id": "u-2", "name": "beta", "state": "stopped", "config": {"isByot": true}},
    ]))
}

#[test]
fn list_shows_the_accounts_nodes_with_the_local_one_last() {
    let home = scratch_home("nodes-list");
    record_local_node(&home, "http://127.0.0.1:9");
    let api = server(vec![two_nodes()]);

    let list = knaix(&home, &api.url, &["list"]);
    assert_eq!(list.code, 0, "{}", list.err);
    for expected in [
        "alpha",
        "i-1",
        "running",
        "100.64.0.7",
        "MANAGED",
        "llama3",
        "beta",
        "stopped",
        "BYO TAILNET",
        "Standard",
        "local",
        "not started",
        "mock",
    ] {
        assert!(
            list.out.contains(expected),
            "missing {expected:?} in:\n{}",
            list.out
        );
    }
    assert!(
        list.out.find("beta").unwrap() < list.out.find("LOCAL").unwrap(),
        "{}",
        list.out
    );

    let json_out = knaix(&home, &api.url, &["-o", "json", "list"]);
    let all: Value = serde_json::from_str(&json_out.out).unwrap();
    let names: Vec<_> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alpha", "beta", "local"]);
    assert_eq!(all[2]["local"], true);
    assert_eq!(all[2]["state"], "stopped", "no container, as JSON says it");
    assert_eq!(all[2]["instanceId"], "local");
    assert_eq!(all[2]["id"], "inst-1");
}

#[test]
fn list_with_nothing_to_show_or_refused_says_so() {
    let home = scratch_home("nodes-list-empty");
    let none = server(vec![nodes(json!([]))]);
    let list = knaix(&home, &none.url, &["list"]);
    assert_eq!(list.code, 0, "{}", list.err);
    assert!(list.out.contains("No hosted nodes yet."), "{}", list.out);

    for (status, code) in [(401, 3), (500, 1)] {
        let refusing = server(vec![("GET /api/instances", status, String::new())]);
        let list = knaix(&home, &refusing.url, &["list"]);
        assert_eq!(list.code, code, "HTTP {status}: {}", list.err);
        assert!(
            list.err
                .contains(&format!("Failed to fetch nodes: HTTP {status}")),
            "{}",
            list.err
        );
    }
}

#[test]
fn signed_out_the_local_node_is_still_listed() {
    let home = scratch_home("nodes-list-signed-out");
    let api = server(vec![two_nodes()]);

    let nothing = knaix_signed_out(&home, &api.url, &["list"]);
    assert_eq!(
        nothing.code, 3,
        "no session and no local node: {}",
        nothing.err
    );

    record_local_node(&home, "http://127.0.0.1:9");
    let list = knaix_signed_out(&home, &api.url, &["list"]);
    assert_eq!(list.code, 0, "{}", list.err);
    assert!(list.out.contains("local"), "{}", list.out);
    assert!(
        list.out
            .contains("Hosted nodes are not listed: no session on this machine."),
        "{}",
        list.out
    );

    let json_out = knaix_signed_out(&home, &api.url, &["-o", "json", "list"]);
    let all: Value = serde_json::from_str(&json_out.out).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["name"], "local");
    assert!(api.calls().is_empty(), "nothing asked of the account");
}

const DOCS: &str = "GET /api/knowledge/u-1/documents";

#[test]
fn list_of_a_hosted_node_shows_its_documents() {
    let home = scratch_home("nodes-list-docs");
    let docs = json!({"data": [
        {"id": "d-1", "source": {"type": "file", "name": "policy.md"}, "chunkCount": 3, "createdAt": "2026-11-01"},
        {"id": "d-2"},
    ]});
    let api = server(vec![two_nodes(), (DOCS, 200, docs.to_string())]);

    let list = knaix(&home, &api.url, &["list", "alpha"]);
    assert_eq!(list.code, 0, "{}", list.err);
    for expected in [
        "Knowledge Base for Node alpha:",
        "policy.md",
        "file",
        "3",
        "2026-11-01",
        "N/A",
    ] {
        assert!(
            list.out.contains(expected),
            "missing {expected:?} in:\n{}",
            list.out
        );
    }

    let json_out = knaix(&home, &api.url, &["-o", "json", "list", "-n", "alpha"]);
    let listed: Value = serde_json::from_str(&json_out.out).unwrap();
    assert_eq!(listed, docs["data"], "as the control plane gave them");

    let empty = server(vec![
        two_nodes(),
        (DOCS, 200, json!({"data": []}).to_string()),
    ]);
    let list = knaix(&home, &empty.url, &["list", "alpha"]);
    assert!(
        list.out.contains("No documents found in knowledge base."),
        "{}",
        list.out
    );

    let missing = server(vec![two_nodes(), (DOCS, 404, String::new())]);
    let list = knaix(&home, &missing.url, &["list", "alpha"]);
    assert_eq!(list.code, 5, "{}", list.err);
    assert!(
        list.err.contains("Failed to fetch documents: HTTP 404"),
        "{}",
        list.err
    );
}

fn use_local(home: &Path) {
    fs::write(
        home.join(".knaix/config.json"),
        json!({"default_node_id": "local"}).to_string(),
    )
    .unwrap();
}

#[test]
fn list_of_the_local_node_shows_what_it_holds_and_what_answers() {
    let home = scratch_home("nodes-list-local");
    let node = server(vec![(
        "POST /api/kb/documents",
        200,
        json!({"documents": [
            {"document_id": "h-1", "source": "house.md", "chunks": 2, "created_at": "2026-11-02"},
            {"document_id": "h-2", "source": "menu.md", "chunks": 1},
        ]})
        .to_string(),
    )]);
    record_local_node(&home, &node.url);

    let docs_flag = knaix(&home, "http://127.0.0.1:9", &["list", "--docs"]);
    assert_eq!(docs_flag.code, 7, "no default to list: {}", docs_flag.err);
    assert!(
        docs_flag
            .err
            .contains("--docs lists the documents on the default node"),
        "{}",
        docs_flag.err
    );

    use_local(&home);
    for args in [&["list"][..], &["list", "--docs"], &["list", "local"]] {
        let list = knaix(&home, "http://127.0.0.1:9", args);
        assert_eq!(list.code, 0, "{args:?}: {}", list.err);
        for expected in [
            "Knowledge Base for the local node:",
            "house.md",
            "menu.md",
            "not started",
            "2 documents, 3 chunks, answered by",
        ] {
            assert!(
                list.out.contains(expected),
                "{args:?} missing {expected:?} in:\n{}",
                list.out
            );
        }
    }

    let json_out = knaix(&home, "http://127.0.0.1:9", &["-o", "json", "list"]);
    let listed: Value = serde_json::from_str(&json_out.out).unwrap();
    assert_eq!(listed[0]["id"], "h-1");
    assert_eq!(listed[0]["source"]["name"], "house.md");
    assert_eq!(listed[0]["chunkCount"], 2);

    let hosted = server(vec![two_nodes()]);
    let nodes_flag = knaix(&home, &hosted.url, &["list", "--nodes"]);
    assert!(
        nodes_flag.out.contains("alpha"),
        "--nodes lists nodes whatever the default: {}",
        nodes_flag.out
    );

    let empty = server(vec![(
        "POST /api/kb/documents",
        200,
        json!({"documents": []}).to_string(),
    )]);
    record_local_node(&home, &empty.url);
    let list = knaix(&home, "http://127.0.0.1:9", &["list"]);
    assert!(
        list.out.contains("The local node holds no documents yet."),
        "{}",
        list.out
    );
}

const HEALTH: &str = "GET /api/nodes/u-1/health";

#[test]
fn metrics_of_a_hosted_node_as_a_table_or_as_it_came() {
    let home = scratch_home("nodes-metrics");
    let health = json!({"nodeId": "u-1", "nodeName": "alpha", "healthy": true, "latencyMs": 42.0, "checkedAt": "2026-11-11T10:00:00Z"});
    let api = server(vec![two_nodes(), (HEALTH, 200, health.to_string())]);

    let metrics = knaix(&home, &api.url, &["metrics", "alpha"]);
    assert_eq!(metrics.code, 0, "{}", metrics.err);
    for expected in [
        "Node Health & Metrics:",
        "u-1",
        "alpha",
        "HEALTHY",
        "42ms",
        "2026-11-11T10:00:00Z",
    ] {
        assert!(
            metrics.out.contains(expected),
            "missing {expected:?} in:\n{}",
            metrics.out
        );
    }

    let json_out = knaix(&home, &api.url, &["-o", "json", "metrics", "-n", "alpha"]);
    assert_eq!(
        serde_json::from_str::<Value>(&json_out.out).unwrap(),
        health
    );

    let sick = server(vec![
        two_nodes(),
        (HEALTH, 200, json!({"healthy": false}).to_string()),
    ]);
    let metrics = knaix(&home, &sick.url, &["metrics", "alpha"]);
    for expected in ["UNHEALTHY", "Unknown", "N/A"] {
        assert!(
            metrics.out.contains(expected),
            "missing {expected:?} in:\n{}",
            metrics.out
        );
    }
    assert!(
        !metrics.out.contains("Latency"),
        "none measured: {}",
        metrics.out
    );

    let refusing = server(vec![two_nodes(), (HEALTH, 503, String::new())]);
    let metrics = knaix(&home, &refusing.url, &["metrics", "alpha"]);
    assert_eq!(metrics.code, 1, "{}", metrics.err);
    assert!(
        metrics.err.contains("Failed to fetch metrics: HTTP 503"),
        "{}",
        metrics.err
    );
}

#[test]
fn metrics_of_the_local_node_are_measured_from_this_machine() {
    let home = scratch_home("nodes-metrics-local");
    let ready =
        json!({"ready": true, "binding": "127.0.0.1", "tier": "community", "peers": ["a", "b"]});
    let node = server(vec![("GET /health", 200, ready.to_string())]);
    record_local_node(&home, &node.url);

    let metrics = knaix(&home, "http://127.0.0.1:9", &["metrics", "local"]);
    assert_eq!(metrics.code, 0, "{}", metrics.err);
    for expected in [
        "Local node health:",
        "HEALTHY",
        "127.0.0.1",
        "community",
        "2",
    ] {
        assert!(
            metrics.out.contains(expected),
            "missing {expected:?} in:\n{}",
            metrics.out
        );
    }

    let json_out = knaix(
        &home,
        "http://127.0.0.1:9",
        &["-o", "json", "metrics", "local"],
    );
    let measured: Value = serde_json::from_str(&json_out.out).unwrap();
    assert_eq!(measured["node"], "local");
    assert_eq!(measured["url"], node.url);
    assert_eq!(measured["healthy"], true);
    assert_eq!(measured["tier"], "community");
    assert!(measured["latencyMs"].is_u64());

    let waking = server(vec![(
        "GET /health",
        503,
        json!({"ready": false}).to_string(),
    )]);
    record_local_node(&home, &waking.url);
    let metrics = knaix(&home, "http://127.0.0.1:9", &["metrics", "local"]);
    for expected in ["NOT READY", "unknown"] {
        assert!(
            metrics.out.contains(expected),
            "missing {expected:?} in:\n{}",
            metrics.out
        );
    }

    record_local_node(&home, "http://127.0.0.1:9");
    let metrics = knaix(&home, "http://127.0.0.1:9", &["metrics", "local"]);
    assert_eq!(metrics.code, 4, "{}", metrics.err);
    assert!(
        metrics
            .err
            .contains("Could not reach the local node at http://127.0.0.1:9."),
        "{}",
        metrics.err
    );
}

const LOGS: &str = "GET /api/nodes/u-1/logs?limit=5";

#[test]
fn logs_of_a_hosted_node_as_lines_or_json_however_they_came() {
    let home = scratch_home("nodes-logs");
    let lines = json!({"logs": ["boot", "ready"]});
    let api = server(vec![two_nodes(), (LOGS, 200, lines.to_string())]);

    let logs = knaix(&home, &api.url, &["logs", "alpha", "-l", "5"]);
    assert_eq!(logs.code, 0, "{}", logs.err);
    assert!(
        logs.out
            .contains("--- Logs: u-1 (last 5) ---\nboot\nready\n--- End ---"),
        "{}",
        logs.out
    );
    let json_out = knaix(&home, &api.url, &["-o", "json", "logs", "alpha", "-l", "5"]);
    assert_eq!(
        serde_json::from_str::<Value>(&json_out.out).unwrap(),
        json!({"lines": ["boot", "ready"]})
    );

    let text = server(vec![
        two_nodes(),
        (LOGS, 200, json!({"logs": "boot\nready"}).to_string()),
    ]);
    let logs = knaix(&home, &text.url, &["logs", "alpha", "-l", "5"]);
    assert!(logs.out.contains("boot\nready"), "{}", logs.out);
    let json_out = knaix(
        &home,
        &text.url,
        &["-o", "json", "logs", "alpha", "-l", "5"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&json_out.out).unwrap(),
        json!({"lines": ["boot", "ready"]})
    );

    let none = server(vec![two_nodes(), (LOGS, 200, json!({}).to_string())]);
    let logs = knaix(&home, &none.url, &["logs", "alpha", "-l", "5"]);
    assert!(logs.out.contains("No logs available."), "{}", logs.out);
    let json_out = knaix(
        &home,
        &none.url,
        &["-o", "json", "logs", "alpha", "-l", "5"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&json_out.out).unwrap(),
        json!({"lines": []})
    );

    let refusing = server(vec![two_nodes(), (LOGS, 403, String::new())]);
    let logs = knaix(&home, &refusing.url, &["logs", "alpha", "-l", "5"]);
    assert_eq!(logs.code, 3, "{}", logs.err);
    assert!(
        logs.err.contains("Failed to fetch logs: HTTP 403"),
        "{}",
        logs.err
    );
}
