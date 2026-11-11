//! A node's documents, run the way a script runs them: `ls`, `cat`, `export`,
//! `rm`, `upload` and `memory`, through the binary, against a fake control
//! plane or a fake local node on loopback, with no terminal.

mod common;

use base64::Engine as _;
use common::{knaix, knaix_reading, nodes, record_local_node, scratch_home, server, Server};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::Stdio;

const LIST: &str = "GET /api/knowledge/u-1/documents";
const UPLOAD: &str = "POST /api/knowledge/u-1/documents";

fn alpha() -> (&'static str, u16, String) {
    nodes(json!([{"id": "u-1", "name": "alpha", "state": "running"}]))
}

/// Two copies of policy.md and one travel.md, on the hosted node.
fn hosted_documents() -> (&'static str, u16, String) {
    let data = json!({"data": [
        {"id": "d-1", "source": {"name": "policy.md"}, "chunkCount": 3, "createdAt": "2026-11-01"},
        {"id": "d-2", "source": {"name": "policy.md"}, "chunkCount": 2, "createdAt": "2026-11-05"},
        {"id": "d-3", "source": {"name": "travel.md"}, "chunkCount": 1},
    ]});
    (LIST, 200, data.to_string())
}

fn routes_of(api: &Server) -> Vec<String> {
    api.calls().into_iter().map(|c| c.route).collect()
}

#[test]
fn cat_prints_one_document_and_refuses_a_name_that_is_ambiguous_or_unknown() {
    let home = scratch_home("docs-cat");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            "GET /api/knowledge/u-1/documents/d-3/content",
            200,
            json!({"data": {"content": "Economy class."}}).to_string(),
        ),
        (
            "GET /api/knowledge/u-1/documents/d-1/content",
            200,
            json!({"data": {"content": "Thirty days.\n"}}).to_string(),
        ),
    ]);

    let cat = knaix(&home, &api.url, &["cat", "-n", "alpha", "travel.md"]);
    assert_eq!(cat.code, 0, "{}", cat.err);
    assert_eq!(cat.out, "Economy class.\n", "ending on a newline it lacked");
    let read = api
        .calls()
        .into_iter()
        .find(|c| c.route.ends_with("/content"))
        .unwrap();
    assert_eq!(read.auth.as_deref(), Some("Bearer sess"));

    let by_id = knaix(&home, &api.url, &["cat", "-n", "alpha", "d-1"]);
    assert_eq!(by_id.out, "Thirty days.\n", "an id picks one copy");

    let ambiguous = knaix(&home, &api.url, &["cat", "-n", "alpha", "policy.md"]);
    assert_eq!(ambiguous.code, 2, "{}", ambiguous.err);
    for expected in [
        "2 copies of policy.md are filed here:",
        "d-1  ingested 2026-11-01",
        "d-2  ingested 2026-11-05",
        "Name one by its id",
    ] {
        assert!(
            ambiguous.err.contains(expected),
            "missing {expected:?}: {}",
            ambiguous.err
        );
    }

    let unknown = knaix(&home, &api.url, &["cat", "-n", "alpha", "menu.md"]);
    assert_eq!(unknown.code, 5, "{}", unknown.err);
    assert!(
        unknown.err.contains("No document named menu.md."),
        "{}",
        unknown.err
    );
}

#[test]
fn export_writes_a_new_file_or_stdout_and_never_overwrites() {
    let home = scratch_home("docs-export");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            "GET /api/knowledge/u-1/documents/d-3/content",
            200,
            json!({"data": {"content": "Economy class."}}).to_string(),
        ),
    ]);
    let out = home.join("travel-copy.md");
    let out_arg = out.display().to_string();

    let export = knaix(
        &home,
        &api.url,
        &["export", "-n", "alpha", "travel.md", "--out", &out_arg],
    );
    assert_eq!(export.code, 0, "{}", export.err);
    assert_eq!(fs::read_to_string(&out).unwrap(), "Economy class.\n");
    assert!(
        export
            .out
            .contains(&format!("Wrote travel.md (15 bytes) to {out_arg}")),
        "{}",
        export.out
    );

    fs::write(&out, "mine").unwrap();
    let again = knaix(
        &home,
        &api.url,
        &["export", "-n", "alpha", "travel.md", "--out", &out_arg],
    );
    assert_eq!(again.code, 6, "{}", again.err);
    assert!(
        again
            .err
            .contains("already exists. Move it, or pass a different --out."),
        "{}",
        again.err
    );
    assert_eq!(fs::read_to_string(&out).unwrap(), "mine", "left as it was");

    let to_stdout = knaix(&home, &api.url, &["export", "-n", "alpha", "travel.md"]);
    assert_eq!(to_stdout.out, "Economy class.\n");
}

#[test]
fn rm_names_what_it_removes_needs_yes_in_a_script_and_reports_what_failed() {
    let home = scratch_home("docs-rm");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            "DELETE /api/knowledge/u-1/documents/d-1",
            204,
            String::new(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/d-2",
            500,
            json!({"error": "busy"}).to_string(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/d-3",
            204,
            String::new(),
        ),
    ]);

    let unconfirmed = knaix(&home, &api.url, &["rm", "-n", "alpha", "policy.md"]);
    assert_eq!(unconfirmed.code, 6, "{}", unconfirmed.err);
    assert!(
        unconfirmed
            .out
            .contains("Removing 2 documents (5 chunks) from u-1:"),
        "{}",
        unconfirmed.out
    );
    assert!(
        unconfirmed.out.contains("- policy.md  ingested 2026-11-01"),
        "{}",
        unconfirmed.out
    );
    assert!(
        unconfirmed
            .err
            .contains("Pass --yes to remove it in a script."),
        "{}",
        unconfirmed.err
    );
    assert!(
        !routes_of(&api).iter().any(|r| r.starts_with("DELETE")),
        "nothing removed"
    );

    let partly = knaix(
        &home,
        &api.url,
        &["rm", "-n", "alpha", "policy.md", "--yes"],
    );
    assert_eq!(partly.code, 1, "{}", partly.out);
    assert!(
        partly
            .out
            .contains("policy.md: Could not remove the document: HTTP 500 Internal Server Error"),
        "{}",
        partly.out
    );
    assert!(partly.out.contains("Removed 1 document."), "{}", partly.out);
    assert!(
        partly.err.contains("1 of 2 could not be removed."),
        "{}",
        partly.err
    );

    let one = knaix(
        &home,
        &api.url,
        &["rm", "-n", "alpha", "travel.md", "--yes"],
    );
    assert_eq!(one.code, 0, "{}", one.err);
    assert!(
        one.out.contains("Removing 1 document (1 chunk) from u-1:"),
        "{}",
        one.out
    );

    let unknown = knaix(&home, &api.url, &["rm", "-n", "alpha", "menu.md", "--yes"]);
    assert_eq!(unknown.code, 5, "{}", unknown.err);
    assert!(
        unknown.err.contains("No document named menu.md on u-1."),
        "{}",
        unknown.err
    );
}

/// The local node's documents: one house.md.
fn local_node(extra: Vec<(&'static str, u16, String)>) -> Server {
    let mut routes = vec![(
        "POST /api/kb/documents",
        200,
        json!({"documents": [{"document_id": "h-1", "source": "house.md", "chunks": 2, "created_at": "2026-11-02"}]})
            .to_string(),
    )];
    routes.extend(extra);
    server(routes)
}

#[test]
fn a_local_nodes_documents_are_read_and_removed_on_the_node_itself() {
    let home = scratch_home("docs-local");
    let node = local_node(vec![
        (
            "POST /api/kb/document",
            200,
            json!({"content": "Checkout is at 11."}).to_string(),
        ),
        ("POST /api/kb/delete", 200, "{}".into()),
    ]);
    record_local_node(&home, &node.url);

    let cat = knaix(
        &home,
        "http://127.0.0.1:9",
        &["cat", "-n", "local", "house.md"],
    );
    assert_eq!(cat.out, "Checkout is at 11.\n", "{}", cat.err);
    let rm = knaix(
        &home,
        "http://127.0.0.1:9",
        &["rm", "-n", "local", "house.md", "--yes"],
    );
    assert_eq!(rm.code, 0, "{}", rm.err);

    let calls = node.calls();
    let read = calls
        .iter()
        .find(|c| c.route == "POST /api/kb/document")
        .unwrap();
    assert_eq!(
        read.body,
        json!({"instance_id": "inst-1", "document_id": "h-1"})
    );
    let removed = calls
        .iter()
        .find(|c| c.route == "POST /api/kb/delete")
        .unwrap();
    assert_eq!(
        removed.body,
        json!({"instance_id": "inst-1", "document_id": "h-1"})
    );
    assert!(
        calls.iter().all(|c| c.auth.is_none()),
        "nobody to authorize against"
    );
}

#[test]
fn an_older_or_unwell_local_node_says_why_it_cannot_read_or_remove() {
    let home = scratch_home("docs-local-old");
    let node = local_node(vec![
        ("POST /api/kb/document", 404, String::new()),
        ("POST /api/kb/delete", 500, String::new()),
    ]);
    record_local_node(&home, &node.url);

    let cat = knaix(
        &home,
        "http://127.0.0.1:9",
        &["cat", "-n", "local", "house.md"],
    );
    assert_eq!(cat.code, 7, "{}", cat.err);
    assert!(
        cat.err
            .contains("This node is too old to read a document back."),
        "{}",
        cat.err
    );

    let rm = knaix(
        &home,
        "http://127.0.0.1:9",
        &["rm", "-n", "local", "house.md", "--yes"],
    );
    assert_eq!(rm.code, 1, "{}", rm.out);
    assert!(
        rm.out.contains("Could not remove the document: HTTP 500"),
        "{}",
        rm.out
    );

    let old = server(vec![]);
    record_local_node(&home, &old.url);
    let listing = knaix(
        &home,
        "http://127.0.0.1:9",
        &["cat", "-n", "local", "house.md"],
    );
    assert_eq!(listing.code, 7, "{}", listing.err);
    assert!(
        listing
            .err
            .contains("This node is too old to list its documents."),
        "{}",
        listing.err
    );
}

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn a_file_uploaded_to_a_hosted_node_is_sent_whole_and_a_second_copy_is_called_out() {
    let home = scratch_home("docs-upload");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"chunkCount": 4}}).to_string(),
        ),
    ]);
    write(&home, "travel.md", "Economy, always.");

    let upload = knaix(&home, &api.url, &["upload", "-n", "alpha", "travel.md"]);

    assert_eq!(upload.code, 0, "{}", upload.err);
    assert!(
        upload.out.contains(
            "travel.md is already ingested. This adds another copy; --replace updates it instead."
        ),
        "{}",
        upload.out
    );
    assert!(
        upload.out.contains("travel.md ingested (4 chunks)."),
        "{}",
        upload.out
    );
    let sent = api.calls().into_iter().find(|c| c.route == UPLOAD).unwrap();
    assert_eq!(sent.auth.as_deref(), Some("Bearer sess"));
    assert!(sent.raw.contains("filename=\"travel.md\""), "{}", sent.raw);
    assert!(sent.raw.contains("Economy, always."), "{}", sent.raw);
}

#[test]
fn replace_removes_the_old_copies_only_once_the_new_one_is_in() {
    let home = scratch_home("docs-replace");
    write(&home, "policy.md", "Forty days.");
    let deletes = vec![
        (
            "DELETE /api/knowledge/u-1/documents/d-1",
            204,
            String::new(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/d-2",
            204,
            String::new(),
        ),
    ];

    let mut routes = vec![
        alpha(),
        hosted_documents(),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"chunkCount": 1}}).to_string(),
        ),
    ];
    routes.extend(deletes.clone());
    let api = server(routes);
    let replaced = knaix(
        &home,
        &api.url,
        &["upload", "-n", "alpha", "policy.md", "--replace"],
    );
    assert_eq!(replaced.code, 0, "{}", replaced.err);
    assert!(
        replaced
            .out
            .contains("replaced 2 copies already filed under policy.md"),
        "{}",
        replaced.out
    );
    let routes = routes_of(&api);
    let sent = routes.iter().position(|r| r == UPLOAD).unwrap();
    let removed = routes.iter().position(|r| r.starts_with("DELETE")).unwrap();
    assert!(sent < removed, "ingest first, delete after: {routes:?}");

    let mut routes = vec![
        alpha(),
        hosted_documents(),
        (UPLOAD, 413, json!({"error": "too large"}).to_string()),
    ];
    routes.extend(deletes);
    let refused_api = server(routes);
    let refused = knaix(
        &home,
        &refused_api.url,
        &["upload", "-n", "alpha", "policy.md", "--replace"],
    );
    assert_eq!(refused.code, 1, "{}", refused.out);
    assert!(
        refused
            .err
            .contains("Upload failed: HTTP 413 Payload Too Large - too large"),
        "{}",
        refused.err
    );
    assert!(
        !routes_of(&refused_api)
            .iter()
            .any(|r| r.starts_with("DELETE")),
        "a refused ingest loses nothing"
    );
}

#[test]
fn replace_reports_an_old_copy_it_could_not_remove() {
    let home = scratch_home("docs-replace-stuck");
    write(&home, "travel.md", "Business, sometimes.");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"chunkCount": 1}}).to_string(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/d-3",
            500,
            String::new(),
        ),
    ]);

    let upload = knaix(
        &home,
        &api.url,
        &["upload", "-n", "alpha", "travel.md", "--replace"],
    );

    assert_eq!(upload.code, 0, "a duplicate, not a hole: {}", upload.err);
    assert!(
        upload
            .out
            .contains("1 older copy of travel.md could not be removed and is still there."),
        "{}",
        upload.out
    );
}

#[test]
fn a_file_uploaded_to_the_local_node_is_ingested_there() {
    let home = scratch_home("docs-upload-local");
    write(&home, "notes.md", "Bring towels.");
    let node = local_node(vec![(
        "POST /api/kb/ingest",
        200,
        json!({"chunkCount": 1, "embeddingProvider": "onnx"}).to_string(),
    )]);
    record_local_node(&home, &node.url);

    let upload = knaix(
        &home,
        "http://127.0.0.1:9",
        &["upload", "-n", "local", "notes.md"],
    );

    assert_eq!(upload.code, 0, "{}", upload.err);
    assert!(
        upload
            .out
            .contains("notes.md ingested (1 chunk), embedded by onnx."),
        "{}",
        upload.out
    );
    let sent = node
        .calls()
        .into_iter()
        .find(|c| c.route == "POST /api/kb/ingest")
        .unwrap();
    assert_eq!(sent.body["instance_id"], "inst-1");
    assert_eq!(sent.body["filename"], "notes.md");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(sent.body["content_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, b"Bring towels.");

    let refusing = local_node(vec![(
        "POST /api/kb/ingest",
        422,
        json!({"message": "unreadable PDF"}).to_string(),
    )]);
    record_local_node(&home, &refusing.url);
    let refused = knaix(
        &home,
        "http://127.0.0.1:9",
        &["upload", "-n", "local", "notes.md"],
    );
    assert_eq!(refused.code, 1, "{}", refused.out);
    assert!(
        refused
            .err
            .contains("Ingest failed: HTTP 422 Unprocessable Entity - unreadable PDF"),
        "{}",
        refused.err
    );
}

/// A project: two documents, a dependency directory, and an image.
fn project(home: &Path) -> std::path::PathBuf {
    let dir = home.join("project");
    write(&dir, "a.md", "Alpha.");
    write(&dir, "docs/b.txt", "Beta.");
    write(&dir, "node_modules/dep/index.js", "ignored");
    fs::write(dir.join("logo.png"), [0x89, b'P', b'N', b'G']).unwrap();
    dir
}

#[test]
fn a_directory_is_planned_without_a_node_and_sent_file_by_file() {
    let home = scratch_home("docs-dir");
    let dir = project(&home).display().to_string();

    let dry = knaix(&home, "http://127.0.0.1:9", &["upload", &dir, "--dry-run"]);
    assert_eq!(dry.code, 0, "{}", dry.err);
    for expected in [
        "Would ingest 2 file(s):",
        "+ a.md",
        "Not descended into: node_modules",
        "Skipping 1 file(s):",
    ] {
        assert!(
            dry.out.contains(expected),
            "missing {expected:?} in:\n{}",
            dry.out
        );
    }

    let api = server(vec![
        alpha(),
        (LIST, 200, json!({"data": []}).to_string()),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"chunkCount": 2}}).to_string(),
        ),
    ]);
    let upload = knaix(&home, &api.url, &["upload", "-n", "alpha", &dir]);
    assert_eq!(upload.code, 0, "{}", upload.err);
    assert!(
        upload.out.contains("Ingested 2 file(s), 4 chunk(s)."),
        "{}",
        upload.out
    );
    assert!(
        upload
            .out
            .contains("1 skipped (run with --dry-run to see which)."),
        "{}",
        upload.out
    );
    let sent: Vec<_> = api
        .calls()
        .into_iter()
        .filter(|c| c.route == UPLOAD)
        .collect();
    assert_eq!(sent.len(), 2);

    let refusing = server(vec![
        alpha(),
        (LIST, 200, json!({"data": []}).to_string()),
        (UPLOAD, 500, json!({"error": "disk full"}).to_string()),
    ]);
    let failed = knaix(&home, &refusing.url, &["upload", "-n", "alpha", &dir]);
    assert_eq!(failed.code, 1, "{}", failed.out);
    assert!(
        failed
            .out
            .contains("a.md: Upload failed: HTTP 500 Internal Server Error - disk full"),
        "{}",
        failed.out
    );
    assert!(
        failed.err.contains("2 of 2 file(s) failed to ingest"),
        "{}",
        failed.err
    );
    let tried = refusing
        .calls()
        .into_iter()
        .filter(|c| c.route == UPLOAD)
        .count();
    assert_eq!(tried, 2, "one failure does not stop the rest");
}

#[test]
fn a_directory_with_nothing_to_send_says_so() {
    let home = scratch_home("docs-dir-empty");
    let dir = home.join("images");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("logo.png"), [0x89, b'P', b'N', b'G']).unwrap();
    let api = server(vec![alpha()]);

    let upload = knaix(
        &home,
        &api.url,
        &["upload", "-n", "alpha", &dir.display().to_string()],
    );

    assert_eq!(upload.code, 0, "{}", upload.err);
    assert!(
        upload
            .out
            .contains("Nothing to ingest. 1 file(s) were skipped"),
        "{}",
        upload.out
    );
}

#[test]
fn piped_input_is_filed_under_its_name() {
    let home = scratch_home("docs-stdin");
    write(&home, "piped.txt", "From a pipe.");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"chunkCount": 1}}).to_string(),
        ),
        (
            "DELETE /api/knowledge/u-1/documents/d-3",
            204,
            String::new(),
        ),
    ]);
    let piped = || Stdio::from(fs::File::open(home.join("piped.txt")).unwrap());

    let dry = knaix_reading(
        &home,
        &api.url,
        &["upload", "-", "--name", "travel.md", "--dry-run"],
        piped(),
    );
    assert!(dry.out.contains("would ingest travel.md"), "{}", dry.out);
    assert!(api.calls().is_empty(), "a dry run sends nothing");

    let upload = knaix_reading(
        &home,
        &api.url,
        &[
            "upload",
            "-n",
            "alpha",
            "-",
            "--name",
            "travel.md",
            "--replace",
        ],
        piped(),
    );

    assert_eq!(upload.code, 0, "{}", upload.err);
    let sent = api.calls().into_iter().find(|c| c.route == UPLOAD).unwrap();
    assert!(sent.raw.contains("filename=\"travel.md\""), "{}", sent.raw);
    assert!(sent.raw.contains("From a pipe."), "{}", sent.raw);
    assert!(
        upload
            .out
            .contains("replaced 1 copy already filed under travel.md"),
        "{}",
        upload.out
    );
}

#[test]
fn memory_lists_a_nodes_notes_and_reads_one() {
    let home = scratch_home("docs-memory");
    let api = server(vec![alpha()]);

    let none = knaix(&home, &api.url, &["memory", "-n", "alpha"]);
    assert_eq!(none.code, 0, "{}", none.err);
    assert!(
        none.out.contains("No notes saved for node u-1."),
        "{}",
        none.out
    );

    let notes = home.join(".knaix/memory/u-1");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("travel.md"), "# Travel\nAisle seat.").unwrap();
    fs::write(notes.join("food.md"), "No peanuts.").unwrap();

    let listed = knaix(&home, &api.url, &["memory", "-n", "alpha"]);
    let food = listed.out.find("- food.md").expect(&listed.out);
    let travel = listed.out.find("- travel.md").expect(&listed.out);
    assert!(food < travel, "sorted: {}", listed.out);

    let read = knaix(
        &home,
        &api.url,
        &["memory", "-n", "alpha", "--file", "travel.md"],
    );
    assert_eq!(read.code, 0, "{}", read.err);
    assert!(read.out.contains("Aisle seat."), "{}", read.out);

    let missing = knaix(
        &home,
        &api.url,
        &["memory", "-n", "alpha", "--file", "hotel.md"],
    );
    assert_eq!(missing.code, 1, "{}", missing.out);
    assert!(
        missing
            .err
            .contains("No file named hotel.md for node u-1. Run 'knaix memory' to list them."),
        "{}",
        missing.err
    );

    fs::remove_file(notes.join("travel.md")).unwrap();
    fs::remove_file(notes.join("food.md")).unwrap();
    let emptied = knaix(&home, &api.url, &["memory", "-n", "alpha"]);
    assert!(emptied.out.contains("No files found."), "{}", emptied.out);
}

#[test]
fn a_document_that_cannot_be_listed_or_read_is_an_error_naming_the_status() {
    let home = scratch_home("docs-unreadable");
    let api = server(vec![
        alpha(),
        hosted_documents(),
        (
            "GET /api/knowledge/u-1/documents/d-3/content",
            500,
            String::new(),
        ),
    ]);
    let cat = knaix(&home, &api.url, &["cat", "-n", "alpha", "travel.md"]);
    assert_eq!(cat.code, 1, "{}", cat.out);
    assert!(
        cat.err
            .contains("Could not read the document: HTTP 500 Internal Server Error"),
        "{}",
        cat.err
    );

    let unlisted = server(vec![alpha(), (LIST, 503, String::new())]);
    let cat = knaix(&home, &unlisted.url, &["cat", "-n", "alpha", "travel.md"]);
    assert_eq!(cat.code, 1, "{}", cat.out);
    assert!(
        cat.err
            .contains("Could not list documents: HTTP 503 Service Unavailable"),
        "{}",
        cat.err
    );

    let node = local_node(vec![("POST /api/kb/document", 500, String::new())]);
    record_local_node(&home, &node.url);
    let cat = knaix(
        &home,
        "http://127.0.0.1:9",
        &["cat", "-n", "local", "house.md"],
    );
    assert_eq!(cat.code, 1, "{}", cat.out);
    assert!(
        cat.err.contains("Could not read the document: HTTP 500"),
        "{}",
        cat.err
    );
}

#[test]
fn an_upload_goes_ahead_when_what_is_already_filed_cannot_be_listed() {
    let home = scratch_home("docs-upload-unlisted");
    write(&home, "travel.md", "Economy.");
    let api = server(vec![
        alpha(),
        (LIST, 503, String::new()),
        (
            UPLOAD,
            200,
            json!({"success": true, "data": {"chunkCount": 1}}).to_string(),
        ),
    ]);

    for args in [
        &["upload", "-n", "alpha", "travel.md"][..],
        &["upload", "-n", "alpha", "travel.md", "--replace"],
    ] {
        let upload = knaix(&home, &api.url, args);
        assert_eq!(upload.code, 0, "{args:?}: {}", upload.err);
        assert!(
            upload.out.contains("travel.md ingested (1 chunk)."),
            "{}",
            upload.out
        );
        assert!(
            !upload.out.contains("already"),
            "nothing known to warn about: {}",
            upload.out
        );
    }
    assert_eq!(api.calls().iter().filter(|c| c.route == UPLOAD).count(), 2);
    assert!(!api.calls().iter().any(|c| c.route.starts_with("DELETE")));
}

#[test]
fn a_long_dry_run_lists_fifty_and_counts_the_rest() {
    let home = scratch_home("docs-dry-long");
    let dir = home.join("many");
    for i in 0..52 {
        write(&dir, &format!("note-{i:02}.md"), "x");
    }

    let dry = knaix(
        &home,
        "http://127.0.0.1:9",
        &["upload", &dir.display().to_string(), "--dry-run"],
    );

    assert_eq!(dry.code, 0, "{}", dry.err);
    assert!(dry.out.contains("Would ingest 52 file(s):"), "{}", dry.out);
    assert_eq!(dry.out.matches("+ note-").count(), 50, "{}", dry.out);
    assert!(dry.out.contains("+ and 2 more"), "{}", dry.out);
}
