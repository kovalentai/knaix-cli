//! Deciding which files in a directory are worth ingesting.
//!
//! A knowledge base is only as good as what goes into it. Walking a project
//! directory and sending everything puts `node_modules`, `.git` objects and
//! build output into the corpus, where they dilute retrieval and cost embedding
//! time for text nobody will ever ask about.
//!
//! So the walk is opinionated by default -- skip the directories that are never
//! documentation, and skip files the node cannot read anyway -- and every part
//! of that is overridable, because a default that cannot be turned off is a
//! guess imposed on someone who knows better.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;

/// Directory names that are never a tenant's documents. Matched by name at any
/// depth, so a nested `node_modules` is skipped too.
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".terraform",
    "vendor",
    ".cache",
    ".idea",
    ".vscode",
];

/// Extensions the ingest pipeline can actually parse. Anything else is refused
/// by the node, so sending it spends a request to be told no.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "pdf", "docx", "txt", "md", "markdown", "text", "csv", "log", "json", "html", "htm",
];

/// Why a file was not uploaded, so the summary can say something better than a
/// count.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SkipReason {
    /// The node has no parser for this type.
    UnsupportedType,
    /// Did not match an `--include` the caller supplied.
    NotIncluded,
    /// Matched an `--exclude` the caller supplied.
    Excluded,
    /// Nothing to embed.
    Empty,
}

impl SkipReason {
    pub fn describe(&self) -> &'static str {
        match self {
            SkipReason::UnsupportedType => "unsupported type",
            SkipReason::NotIncluded => "did not match --include",
            SkipReason::Excluded => "matched --exclude",
            SkipReason::Empty => "empty file",
        }
    }
}

#[derive(Debug)]
pub struct UploadFilter {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
    /// Send every readable file, whatever its extension, and walk directories
    /// normally skipped. For the caller who means it.
    all: bool,
}

impl UploadFilter {
    pub fn new(include: &[String], exclude: &[String], all: bool) -> Result<Self> {
        Ok(Self {
            include: build_globs(include).context("invalid --include pattern")?,
            exclude: build_globs(exclude).context("invalid --exclude pattern")?,
            all,
        })
    }

    /// Whether to descend into a directory. Checked during the walk rather than
    /// after, so a large `node_modules` is never traversed at all.
    pub fn should_enter(&self, dir_name: &str) -> bool {
        self.all || !SKIPPED_DIRS.contains(&dir_name)
    }

    /// Whether to upload one file, and if not, why.
    ///
    /// `relative` is the path as the user would recognize it, so glob patterns
    /// match what they typed rather than an absolute path they never saw.
    pub fn verdict(&self, relative: &Path, size: u64) -> Option<SkipReason> {
        if size == 0 {
            return Some(SkipReason::Empty);
        }

        // An explicit --exclude always wins: it is the most specific thing the
        // caller said.
        if let Some(set) = &self.exclude {
            if set.is_match(relative) {
                return Some(SkipReason::Excluded);
            }
        }

        // An explicit --include replaces the type default entirely. Someone
        // asking for '*.rs' means it, and would not thank us for refusing on
        // the grounds that the node usually only takes documents.
        if let Some(set) = &self.include {
            return if set.is_match(relative) {
                None
            } else {
                Some(SkipReason::NotIncluded)
            };
        }

        if self.all {
            return None;
        }

        let ext = relative
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
            None
        } else {
            Some(SkipReason::UnsupportedType)
        }
    }
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        // Bare patterns like '*.md' should match at any depth, which is what
        // someone typing them means; a pattern with a slash is taken literally.
        let expanded = if pattern.contains('/') {
            pattern.clone()
        } else {
            format!("**/{}", pattern)
        };
        builder.add(Glob::new(&expanded).with_context(|| format!("bad pattern: {}", pattern))?);
    }
    Ok(Some(builder.build()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn filter(include: &[&str], exclude: &[&str], all: bool) -> UploadFilter {
        UploadFilter::new(
            &include.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &exclude.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            all,
        )
        .unwrap()
    }

    #[test]
    fn noise_directories_are_not_entered() {
        let f = filter(&[], &[], false);
        for dir in ["node_modules", ".git", "target", "__pycache__"] {
            assert!(!f.should_enter(dir), "{dir} should be skipped");
        }
        assert!(f.should_enter("docs"));
        assert!(f.should_enter("src"));
    }

    #[test]
    fn unsupported_types_are_skipped_before_the_node_refuses_them() {
        // The node answers 400 for these. Sending them spends a request to be
        // told no, and used to abort the whole run.
        let f = filter(&[], &[], false);
        assert_eq!(
            f.verdict(&PathBuf::from("logo.png"), 10),
            Some(SkipReason::UnsupportedType)
        );
        assert_eq!(f.verdict(&PathBuf::from("notes.md"), 10), None);
        assert_eq!(f.verdict(&PathBuf::from("paper.pdf"), 10), None);
    }

    #[test]
    fn an_include_replaces_the_type_default() {
        // Someone asking for '*.rs' means it, even though the node does not
        // normally take source files.
        let f = filter(&["*.rs"], &[], false);
        assert_eq!(f.verdict(&PathBuf::from("src/main.rs"), 10), None);
        assert_eq!(
            f.verdict(&PathBuf::from("README.md"), 10),
            Some(SkipReason::NotIncluded)
        );
    }

    #[test]
    fn exclude_beats_include() {
        let f = filter(&["*.md"], &["CHANGELOG.md"], false);
        assert_eq!(f.verdict(&PathBuf::from("docs/guide.md"), 10), None);
        assert_eq!(
            f.verdict(&PathBuf::from("CHANGELOG.md"), 10),
            Some(SkipReason::Excluded)
        );
    }

    #[test]
    fn bare_patterns_match_at_any_depth() {
        // '*.md' should mean what a user means by it, not just top level.
        let f = filter(&["*.md"], &[], false);
        assert_eq!(f.verdict(&PathBuf::from("a/b/c/deep.md"), 10), None);
    }

    #[test]
    fn a_pattern_with_a_slash_is_taken_literally() {
        let f = filter(&["docs/*.md"], &[], false);
        assert_eq!(f.verdict(&PathBuf::from("docs/guide.md"), 10), None);
        assert_eq!(
            f.verdict(&PathBuf::from("other/guide.md"), 10),
            Some(SkipReason::NotIncluded)
        );
    }

    #[test]
    fn empty_files_are_skipped() {
        // Nothing to embed, and the node rejects them.
        let f = filter(&[], &[], false);
        assert_eq!(
            f.verdict(&PathBuf::from("empty.md"), 0),
            Some(SkipReason::Empty)
        );
    }

    #[test]
    fn all_overrides_both_defaults() {
        let f = filter(&[], &[], true);
        assert!(f.should_enter("node_modules"));
        assert_eq!(f.verdict(&PathBuf::from("logo.png"), 10), None);
    }

    #[test]
    fn a_bad_pattern_is_reported_rather_than_ignored() {
        // Silently matching nothing would look like an empty directory.
        let err = UploadFilter::new(&[], &["[".to_string()], false).unwrap_err();
        assert!(err.to_string().contains("--exclude"), "{err}");
    }
}
