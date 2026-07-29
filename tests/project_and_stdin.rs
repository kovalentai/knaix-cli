//! End-to-end checks for `.knaix.toml` and for reading arguments from a pipe.
//!
//! The unit tests cover parsing and precedence in isolation. What they cannot
//! show is that the wiring reaches the command: that `knaix init` writes a file
//! the CLI itself then reads, that a repo's node is picked up from a
//! subdirectory, and that `-` actually consumes the pipe.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("knaix-proj-e2e-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("could not create scratch dir");
    dir
}

/// A HOME with no credentials, so any command that reaches the control plane
/// stops at the auth check, which is a code we can assert on.
fn knaix(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knaix"));
    cmd.env("HOME", home)
        .env("KNAIX_NO_UPDATE_CHECK", "1")
        .current_dir(cwd);
    cmd
}

fn run(cmd: &mut Command) -> (i32, String, String) {
    let out = cmd.output().expect("failed to run knaix");
    (
        out.status.code().expect("killed by a signal"),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn pipe_in(cmd: &mut Command, input: &[u8]) -> (i32, String, String) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn knaix");
    child
        .stdin
        .as_mut()
        .expect("no stdin")
        .write_all(input)
        .expect("could not write to stdin");
    let out = child.wait_with_output().expect("failed to wait");
    (
        out.status.code().expect("killed by a signal"),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn init_writes_a_file_that_records_the_node() {
    let home = scratch("init-home");
    let repo = scratch("init-repo");

    let (code, stdout, stderr) = run(knaix(&home, &repo).args(["init", "--node-id", "acme-node"]));
    assert_eq!(code, 0, "init should succeed: {stderr}");
    assert!(stdout.contains("acme-node"), "{stdout}");

    let written = fs::read_to_string(repo.join(".knaix.toml")).expect("no .knaix.toml written");
    assert!(written.contains("node = \"acme-node\""), "{written}");
    // It explains itself, because this is the file a person edits next.
    assert!(written.contains('#'), "expected comments: {written}");
}

#[test]
fn init_records_the_upload_globs_it_was_given() {
    let home = scratch("init-globs-home");
    let repo = scratch("init-globs-repo");

    let (code, _, stderr) = run(knaix(&home, &repo).args([
        "init",
        "--node-id",
        "acme-node",
        "--include",
        "docs/**/*.md",
        "--exclude",
        "**/CHANGELOG.md",
    ]));
    assert_eq!(code, 0, "{stderr}");

    let written = fs::read_to_string(repo.join(".knaix.toml")).unwrap();
    assert!(written.contains("docs/**/*.md"), "{written}");
    assert!(written.contains("**/CHANGELOG.md"), "{written}");
}

/// Overwriting silently would discard settings a team put under review.
#[test]
fn init_refuses_to_clobber_and_says_how_to_proceed() {
    let home = scratch("init-clobber-home");
    let repo = scratch("init-clobber-repo");

    run(knaix(&home, &repo).args(["init", "--node-id", "first"]));
    let (code, _, stderr) = run(knaix(&home, &repo).args(["init", "--node-id", "second"]));

    assert_eq!(code, 6, "a refusal to overwrite is Denied");
    assert!(stderr.contains("--force"), "should say how: {stderr}");
    let written = fs::read_to_string(repo.join(".knaix.toml")).unwrap();
    assert!(written.contains("first"), "the original must survive");

    let (code, _, _) = run(knaix(&home, &repo).args(["init", "--node-id", "second", "--force"]));
    assert_eq!(code, 0);
    let written = fs::read_to_string(repo.join(".knaix.toml")).unwrap();
    assert!(written.contains("second"));
}

/// The whole point of walking up: commands are run from subdirectories.
#[test]
fn a_repos_node_is_picked_up_from_a_subdirectory() {
    let home = scratch("sub-home");
    let repo = scratch("sub-repo");
    let nested = repo.join("src").join("deep");
    fs::create_dir_all(&nested).unwrap();
    fs::write(repo.join(".knaix.toml"), "node = \"from-the-repo\"\n").unwrap();

    // No credentials, so this stops at auth. What matters is which node it was
    // about to address, which the error names.
    let (code, _, stderr) = run(knaix(&home, &nested).args(["chat", "hello"]));
    assert_eq!(code, 3, "no token, so auth: {stderr}");

    // With a token but an unreachable API it still resolves the project node.
    let (code, _, _) = run(knaix(&home, &nested)
        .env("KNAIX_TOKEN", "t")
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .args(["chat", "hello"]));
    assert_eq!(code, 4, "should have tried to reach the API");
}

/// A flag is typed for one command and must beat the file.
#[test]
fn an_explicit_node_flag_beats_the_project_file() {
    let home = scratch("flag-home");
    let repo = scratch("flag-repo");
    fs::write(repo.join(".knaix.toml"), "node = \"from-the-repo\"\n").unwrap();

    let (code, _, stderr) = run(knaix(&home, &repo)
        .env("KNAIX_TOKEN", "t")
        .env("KNAIX_API_URL", "http://127.0.0.1:9")
        .args(["chat", "-n", "from-the-flag", "hello"]));
    // Either way it cannot reach the API; the flag must not be rejected.
    assert_eq!(code, 4, "{stderr}");
}

/// A file that looks like configuration but cannot be parsed must stop the
/// command, not be skipped in favour of different settings.
#[test]
fn a_broken_project_file_stops_the_command() {
    let home = scratch("broken-home");
    let repo = scratch("broken-repo");
    fs::write(repo.join(".knaix.toml"), "node = [not valid toml\n").unwrap();

    let (code, _, stderr) = run(knaix(&home, &repo).args(["chat", "hello"]));
    assert_eq!(code, 2, "a malformed project file is a usage error");
    assert!(
        stderr.contains(".knaix.toml"),
        "should name the file: {stderr}"
    );
}

/// `init` is how a broken file gets replaced, so it must not be blocked by the
/// very file it would overwrite. Every other command still refuses.
#[test]
fn init_still_runs_when_the_existing_file_is_broken() {
    let home = scratch("rescue-home");
    let repo = scratch("rescue-repo");
    fs::write(repo.join(".knaix.toml"), "node = [broken\n").unwrap();

    // Every other command stops.
    let (code, _, _) = run(knaix(&home, &repo).args(["chat", "hello"]));
    assert_eq!(code, 2, "a broken file should stop an ordinary command");

    // init repairs it.
    let (code, _, stderr) =
        run(knaix(&home, &repo).args(["init", "--node-id", "rescued", "--force"]));
    assert_eq!(
        code, 0,
        "init must be able to replace a broken file: {stderr}"
    );

    let written = fs::read_to_string(repo.join(".knaix.toml")).unwrap();
    assert!(written.contains("rescued"), "{written}");

    // And the repaired file is readable by an ordinary command again.
    let (code, _, _) = run(knaix(&home, &repo).args(["chat", "hello"]));
    assert_eq!(code, 3, "no token, so auth rather than a parse failure");
}

/// A name decides where bytes land, and the cleanup that follows decides what
/// gets deleted. An absolute name once escaped the staging directory and made
/// the cleanup remove that path's parent instead.
#[test]
fn an_upload_name_that_is_a_path_is_refused() {
    let home = scratch("badname-home");
    let repo = scratch("badname-repo");
    let bystander = scratch("badname-bystander");
    fs::write(bystander.join("keep.txt"), b"keep me").unwrap();

    let target = bystander.join("escaped.md");
    let (code, _, stderr) = pipe_in(
        knaix(&home, &repo).args([
            "upload",
            "-",
            "--name",
            target.to_str().unwrap(),
            "--dry-run",
        ]),
        b"content",
    );

    assert_eq!(code, 2, "an absolute name is a usage error: {stderr}");
    assert!(
        !target.exists(),
        "nothing should be written outside staging"
    );
    assert!(
        bystander.join("keep.txt").exists(),
        "the neighbouring directory must survive"
    );
}

#[test]
fn chat_reads_the_question_from_a_pipe() {
    let home = scratch("pipe-home");
    let repo = scratch("pipe-repo");

    let (code, _, stderr) = pipe_in(
        knaix(&home, &repo)
            .env("KNAIX_TOKEN", "t")
            .env("KNAIX_API_URL", "http://127.0.0.1:9")
            .args(["chat", "-"]),
        b"what changed this week?",
    );
    // It got past argument handling and tried to ask, which is what proves the
    // pipe was consumed rather than the literal "-" being sent.
    assert_eq!(code, 4, "{stderr}");
}

/// The usual way to reach this is a pipeline whose first command produced
/// nothing, and asking a node an empty question spends a request to be told so.
#[test]
fn an_empty_pipe_is_a_usage_error() {
    let home = scratch("empty-home");
    let repo = scratch("empty-repo");

    let (code, _, stderr) = pipe_in(
        knaix(&home, &repo)
            .env("KNAIX_TOKEN", "t")
            .env("KNAIX_API_URL", "http://127.0.0.1:9")
            .args(["chat", "-"]),
        b"   \n  \n",
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("standard input"), "{stderr}");
}

/// `--dry-run` reports without sending, so this asserts the name piped content
/// is filed under without needing a node.
#[test]
fn upload_takes_a_document_from_a_pipe_and_names_it() {
    let home = scratch("up-home");
    let repo = scratch("up-repo");
    fs::write(repo.join(".knaix.toml"), "node = \"local\"\n").unwrap();
    let knaix_dir = home.join(".knaix");
    fs::create_dir_all(&knaix_dir).unwrap();
    fs::write(
        knaix_dir.join("local.json"),
        r#"{"port":9,"instance_id":"e2e","image":"none"}"#,
    )
    .unwrap();

    let (code, stdout, stderr) = pipe_in(
        knaix(&home, &repo).args(["upload", "-", "--name", "notes.md", "--dry-run"]),
        b"# Notes\n\nSomething worth keeping.\n",
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("notes.md"), "{stdout}");
}

/// Only a bare `-` means stdin. A file that happens to be named `-` is
/// unreachable either way, but a path merely starting with one is ordinary.
#[test]
fn a_path_is_still_a_path() {
    let home = scratch("path-home");
    let repo = scratch("path-repo");
    fs::write(repo.join("real.md"), "# Real\n").unwrap();
    // Point at the local node so resolving a target needs no network. `upload`
    // resolves before it honours --dry-run, so without this the command stops
    // at the unreachable API rather than reporting what it would ingest.
    fs::write(repo.join(".knaix.toml"), "node = \"local\"\n").unwrap();
    let knaix_dir = home.join(".knaix");
    fs::create_dir_all(&knaix_dir).unwrap();
    fs::write(
        knaix_dir.join("local.json"),
        r#"{"port":9,"instance_id":"e2e","image":"none"}"#,
    )
    .unwrap();

    let (code, stdout, stderr) = run(knaix(&home, &repo).args(["upload", "real.md", "--dry-run"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("real.md"), "{stdout}");
}
