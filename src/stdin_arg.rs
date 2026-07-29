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

/// A file that deletes itself, so piped content can go through the same upload
/// path as a named file without leaving anything behind.
pub struct TempFile {
    path: std::path::PathBuf,
}

impl TempFile {
    pub fn write(name_hint: &str, bytes: &[u8]) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "knaix-stdin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).context("Could not create a temporary directory")?;
        let path = dir.join(name_hint);
        std::fs::write(&path, bytes).context("Could not stage standard input for upload")?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Best effort: the process is ending either way, and failing to clean
        // up a temp file is not worth reporting over whatever came before.
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
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

    #[test]
    fn two_temp_files_do_not_collide() {
        let a = TempFile::write("stdin.md", b"a").unwrap();
        let b = TempFile::write("stdin.md", b"b").unwrap();
        assert_ne!(a.path(), b.path());
        assert_eq!(std::fs::read_to_string(a.path()).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(b.path()).unwrap(), "b");
    }
}
