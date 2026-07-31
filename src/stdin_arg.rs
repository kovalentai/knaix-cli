//! Reading an argument from standard input, so `knaix` composes with pipes.
//!
//! A tool that only accepts its input as an argument cannot sit in the middle
//! of a pipeline, and the shell's own answer to long or generated input is to
//! pipe it. `-` is the conventional way to ask for that, and it is worth
//! following exactly rather than inventing a flag.

use crate::exit::{Code, WithCode};
use anyhow::{Context, Result};
use std::io::Read;

/// The argument that means "read it from standard input".
pub const STDIN: &str = "-";

pub fn is_stdin(arg: &str) -> bool {
    arg == STDIN
}

/// Read all of standard input as text.
///
/// Refusing empty input is deliberate. The common way to reach this by accident
/// is a pipeline whose first command produced nothing, and asking a node an
/// empty question spends a request to be told nothing.
pub fn read_text(what: &str) -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("Could not read standard input")?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!(
            "Nothing arrived on standard input for {what}."
        ))
        .coded(Code::Usage);
    }
    Ok(trimmed.to_string())
}

/// Read all of standard input as bytes, for content rather than an argument.
pub fn read_bytes(what: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .context("Could not read standard input")?;
    if buf.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(anyhow::anyhow!(
            "Nothing arrived on standard input for {what}."
        ))
        .coded(Code::Usage);
    }
    Ok(buf)
}

/// A document name that is safe to place inside a directory.
///
/// `Path::join` replaces the whole path when handed an absolute one, so a name
/// is not merely cosmetic: it decides where bytes land. It must be one plain
/// file name and nothing else.
///
/// Rejected rather than quietly trimmed to its last component. A caller who
/// wrote a path meant a path, and silently filing their document somewhere else
/// under a different name is not an improvement on saying no.
///
/// `flag` names the option being checked, so the refusal points at what the
/// caller actually typed. Shared by every flag that takes a bare file name.
pub fn checked_name<'a>(flag: &str, name: &'a str) -> Result<&'a str> {
    use std::path::Component;

    let trimmed = name.trim();
    // Exactly one ordinary component. Counting components is not enough: `.`
    // and `..` are each a single component, and `..` is the interesting one.
    // Backslash is checked by hand because it is only a separator on Windows,
    // and a name is refused on every platform or the rule moves with the host.
    let mut components = std::path::Path::new(trimmed).components();
    let ok = !trimmed.contains('\\')
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();

    if !ok {
        return Err(anyhow::anyhow!(
            "{flag} takes a file name, not a path: {name:?}"
        ))
        .coded(Code::Usage);
    }
    Ok(trimmed)
}

/// A file that deletes itself, so piped content can go through the same upload
/// path as a named file without leaving anything behind.
pub struct TempFile {
    /// The directory this created, and the only thing it may remove. Held
    /// separately rather than derived from the file's parent: deriving it means
    /// a name that escapes the directory also redirects the delete, which is
    /// how cleaning up one file becomes removing someone else's directory.
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl TempFile {
    pub fn write(name: &str, bytes: &[u8]) -> Result<Self> {
        let name = checked_name("--name", name)?;
        // The counter is what makes the name unique. The clock is not enough on
        // its own: two calls in one process can read the same instant, and the
        // second then writes into the first's directory and deletes it on drop.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "knaix-stdin-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).context("Could not create a temporary directory")?;
        let path = dir.join(name);
        std::fs::write(&path, bytes).context("Could not stage standard input for upload")?;
        Ok(Self { dir, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Best effort: the process is ending either way, and failing to clean
        // up a temp file is not worth reporting over whatever came before.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_dash_means_stdin() {
        assert!(is_stdin("-"));
        // A path that merely starts with a dash is a path, and a question that
        // starts with one is a question.
        assert!(!is_stdin("--"));
        assert!(!is_stdin("-n"));
        assert!(!is_stdin("./-"));
        assert!(!is_stdin(""));
        assert!(!is_stdin("- "));
    }

    #[test]
    fn a_temp_file_holds_its_bytes_and_then_disappears() {
        let path;
        {
            let tmp = TempFile::write("stdin.md", b"hello from a pipe").unwrap();
            path = tmp.path().to_path_buf();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello from a pipe");
            assert_eq!(path.file_name().unwrap(), "stdin.md");
        }
        assert!(!path.exists(), "the temp file should be gone");
        assert!(!path.parent().unwrap().exists(), "and its directory too");
    }

    /// A name is not cosmetic: it decides where bytes land, and the cleanup
    /// that follows decides what gets deleted. An absolute name once escaped
    /// the temp directory and took `remove_dir_all` with it, so writing to
    /// `/tmp/x.md` deleted `/tmp`. Everything here must be refused.
    #[test]
    fn a_name_that_is_a_path_is_refused() {
        for bad in [
            "/tmp/escape.md",
            "../escape.md",
            "../../../../etc/passwd",
            "docs/notes.md",
            "/",
            "..",
            ".",
            "",
            "   ",
            r"C:\Windows\notes.md",
        ] {
            let e = checked_name("--name", bad).unwrap_err();
            assert_eq!(
                crate::exit::code_of(&e),
                Code::Usage,
                "{bad:?} should be refused"
            );
            assert!(
                TempFile::write(bad, b"x").is_err(),
                "{bad:?} should not be written"
            );
        }
    }

    #[test]
    fn an_ordinary_name_is_accepted() {
        for good in ["stdin.md", "weekly-report.md", "notes.txt", " spaced.md "] {
            assert!(
                checked_name("--name", good).is_ok(),
                "{good:?} should be accepted"
            );
        }
        assert_eq!(checked_name("--name", " spaced.md ").unwrap(), "spaced.md");
    }

    /// The cleanup must remove only the directory it created, whatever the
    /// file inside it is called.
    #[test]
    fn cleanup_removes_only_its_own_directory() {
        let neighbour =
            std::env::temp_dir().join(format!("knaix-bystander-{}", std::process::id()));
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(neighbour.join("keep.txt"), b"keep me").unwrap();

        {
            let tmp = TempFile::write("stdin.md", b"x").unwrap();
            assert!(tmp.path().starts_with(std::env::temp_dir()));
        }

        assert!(
            neighbour.join("keep.txt").exists(),
            "a neighbouring temp directory must survive"
        );
        let _ = std::fs::remove_dir_all(&neighbour);
    }

    #[test]
    fn two_temp_files_do_not_collide() {
        let a = TempFile::write("stdin.md", b"a").unwrap();
        let b = TempFile::write("stdin.md", b"b").unwrap();
        assert_ne!(a.path(), b.path());
        assert_eq!(std::fs::read_to_string(a.path()).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(b.path()).unwrap(), "b");
    }
}
