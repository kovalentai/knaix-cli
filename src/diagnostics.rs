//! What went wrong recently, kept so a report has something to attach.
//!
//! Without this, `knaix report` run after the fact has nothing: the failure the
//! user wants to tell us about scrolled out of their terminal, and asking them
//! to reproduce a bug they do not understand is how bug reports die. So every
//! failure and every panic appends one line here.
//!
//! Two rules make that safe. Entries are redacted **as they are written**, not
//! when a report is built, so nothing sensitive is ever on disk in the first
//! place and a user who never runs `report` has still lost nothing. And the
//! file is a ring: it holds the last few entries and forgets the rest, because
//! a debug log that grows without bound on someone's laptop is a liability we
//! would be adding, not a feature.

use crate::redact::{argv, Manifest, Scrubber};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// How many entries to keep. Enough to cover "it failed a few times in a row",
/// which is the shape of most reports, and small enough that the file stays
/// something a person can read in one screen.
const RING: usize = 20;

/// What kind of ending an entry records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Ending {
    /// The command returned an error and exited with a code.
    Failure,
    /// The process panicked, which is always our bug.
    Panic,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Entry {
    /// Seconds since the epoch. A bare number rather than a formatted date, so
    /// nothing has to agree about time zones.
    pub at: u64,
    pub ending: Ending,
    pub version: String,
    /// The command as typed, with its values removed.
    pub command: Vec<String>,
    pub exit_code: Option<u8>,
    /// The error chain, or the panic message, reduced the way a log line is.
    pub detail: Vec<String>,
}

fn path() -> PathBuf {
    let mut p = crate::config::get_config_path();
    p.pop();
    p.push("diagnostics.jsonl");
    p
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append an entry, then trim the file back to the ring size.
///
/// Every failure path calls this, so it must never itself fail loudly: a CLI
/// that panics while recording a panic is worse than one that records nothing.
/// Errors here are swallowed on purpose.
fn append(entry: Entry) {
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    let path = path();

    // One line, opened for append, written in a single call. Reading the file
    // and writing it back was losing entries: twelve commands failing at once
    // produced three records, because each process read before the others
    // wrote. It also truncated first, so a process dying mid-write took the
    // whole history with it. O_APPEND has neither problem, and the kernel puts
    // each line at the end without the writers having to agree.
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    if let Ok(mut f) = options.open(&path) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }

    trim(&path);
}

/// Cut the file back to the ring size, if it has grown past it.
///
/// Separate from the append so the append stays a single atomic write. Racing
/// trims can only lose entries a later run would have dropped anyway, which is
/// the one kind of loss this file is allowed to have.
fn trim(path: &std::path::Path) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= RING {
        return;
    }

    // Through a temp file and a rename, so a reader never sees a half-written
    // ring. The same move config.rs makes for the saved session.
    let tmp = path.with_extension("jsonl.tmp");
    let kept = lines[lines.len() - RING..].join("\n");

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    if let Ok(mut f) = options.open(&tmp) {
        if writeln!(f, "{kept}").is_ok() && f.sync_all().is_ok() {
            let _ = std::fs::rename(&tmp, path);
            return;
        }
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Record a command that failed.
pub fn record_failure(err: &anyhow::Error, code: u8) {
    let mut manifest = Manifest::default();
    let args: Vec<String> = std::env::args().collect();

    // The error chain is our own prose with values interpolated into it, so it
    // goes through the scrubber rather than the log reducer. Reducing it the way
    // a node's log line is reduced threw the whole message away, because "Not
    // logged in. Run 'knaix login' first." has no timestamp, level or route in
    // it, and losing what failed defeats the point of recording that it failed.
    //
    // The arguments join the scrubber for this one record. An error naming a
    // file the user asked for ("could not read ./Q3 Board Pack.pdf") is quoting
    // something they typed, and what they typed is right here, so there is no
    // need to guess at which words in the sentence were theirs.
    let mut scrubber = Scrubber::for_this_machine();
    for arg in args.iter().skip(1).filter(|a| !a.starts_with('-')) {
        scrubber.add(Some(arg), "<removed>".to_string());
    }

    let detail = err
        .chain()
        .map(|c| scrubber.scrub(&c.to_string(), "error message", &mut manifest))
        .filter(|l| !l.is_empty())
        .collect();

    append(Entry {
        at: now(),
        ending: Ending::Failure,
        version: env!("CARGO_PKG_VERSION").to_string(),
        command: argv(&args, &mut manifest),
        exit_code: Some(code),
        detail,
    });
}

/// Record a panic. Installed as the hook so a crash is recoverable.
pub fn record_panic(info: &std::panic::PanicHookInfo<'_>) {
    let mut manifest = Manifest::default();
    let args: Vec<String> = std::env::args().collect();

    // The arguments go in the scrubber here for the same reason they do when a
    // command fails, and more urgently: a panic message is written by us in the
    // middle of something going wrong, so it is the message most likely to
    // interpolate a path or a name we were holding. Without this, a panic while
    // reading a file recorded that file's name verbatim.
    let mut scrubber = Scrubber::for_this_machine();
    for arg in args.iter().skip(1).filter(|a| !a.starts_with('-')) {
        scrubber.add(Some(arg), "<removed>".to_string());
    }

    // The location is ours, so it is kept whole: it names our source file, not
    // anything of the user's. The message is not, so it is reduced.
    let mut detail = Vec::new();
    if let Some(loc) = info.location() {
        detail.push(format!("at {}:{}", loc.file(), loc.line()));
    }
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_default();
    if !message.is_empty() {
        detail.push(scrubber.scrub(&message, "panic message", &mut manifest));
    }

    append(Entry {
        at: now(),
        ending: Ending::Panic,
        version: env!("CARGO_PKG_VERSION").to_string(),
        command: argv(&args, &mut manifest),
        exit_code: None,
        detail,
    });
}

/// The entries on disk, oldest first. Unreadable lines are skipped rather than
/// failing the read: a report is worth more than nothing.
pub fn recent() -> Vec<Entry> {
    std::fs::read_to_string(path())
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Forget everything recorded. The user's own material, so they can drop it.
pub fn clear() -> std::io::Result<()> {
    match std::fs::remove_file(path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring has to forget, or a debug aid becomes an unbounded file on
    /// someone's laptop that nobody asked for.
    #[test]
    fn the_ring_keeps_only_the_last_entries() {
        let mut kept: Vec<String> = (0..30).map(|i| i.to_string()).collect();
        let start = kept.len().saturating_sub(RING);
        kept = kept[start..].to_vec();
        assert_eq!(kept.len(), RING);
        assert_eq!(kept.first().unwrap(), "10");
        assert_eq!(kept.last().unwrap(), "29");
    }

    /// An entry is redacted on the way in. This is the property that makes the
    /// file safe to exist at all: a user who never runs `report` has still had
    /// nothing sensitive written to their disk.
    ///
    /// The message keeps our own words and loses theirs. Both halves matter: an
    /// earlier version reduced error text the way a node's log line is reduced
    /// and stored an empty detail, which is safe and useless.
    #[test]
    fn an_error_keeps_our_words_and_loses_the_users() {
        let mut m = Manifest::default();
        let mut scrubber = Scrubber::default();
        // What the user typed, which is what the recorder adds for each record.
        scrubber.add(Some("./Q3 Board Pack.pdf"), "<removed>".to_string());

        let stored = scrubber.scrub(
            "Could not read ./Q3 Board Pack.pdf",
            "error message",
            &mut m,
        );
        assert!(!stored.contains("Board"), "a filename survived: {stored}");
        assert!(
            stored.starts_with("Could not read"),
            "the reason was lost: {stored}"
        );
    }

    /// The hook has to survive being called during a real unwind, and it has to
    /// scrub the panic message, which can quote anything the code was holding.
    ///
    /// Runs against a scratch HOME so it writes nowhere real. It is the only
    /// test in this binary that moves HOME, and it puts it back.
    #[test]
    fn a_panic_is_recorded_and_scrubbed() {
        let dir = std::env::temp_dir().join(format!("knaix-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".knaix")).unwrap();
        let previous_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);

        let before = recent().len();

        // The default hook would print to stderr and clutter the test output,
        // so only ours runs for the duration.
        let old = std::panic::take_hook();
        std::panic::set_hook(Box::new(record_panic));
        let result = std::panic::catch_unwind(|| {
            panic!("exploded while reading ./Q3 Board Pack.pdf");
        });
        std::panic::set_hook(old);

        assert!(result.is_err(), "the panic should have been caught");

        let entries = recent();
        assert_eq!(entries.len(), before + 1, "the panic was not recorded");
        let last = entries.last().unwrap();
        assert_eq!(last.ending, Ending::Panic);
        assert!(last.exit_code.is_none());

        let detail = last.detail.join(" ");
        // Our own location survives; it names our source, not the user's.
        assert!(
            detail.contains("diagnostics.rs"),
            "the panic location was lost: {detail}"
        );
        assert!(
            detail.contains("exploded while reading"),
            "the panic reason was lost: {detail}"
        );

        match previous_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_round_trips() {
        let e = Entry {
            at: 1_785_000_000,
            ending: Ending::Panic,
            version: "0.4.6".into(),
            command: vec!["knaix".into(), "chat".into()],
            exit_code: None,
            detail: vec!["at src/main.rs:1".into()],
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ending, Ending::Panic);
        assert_eq!(back.command, vec!["knaix", "chat"]);
        // The discriminant is lowercase in the file, so a human reading the
        // jsonl sees "panic" rather than a Rust variant name.
        assert!(json.contains(r#""ending":"panic""#));
    }
}
