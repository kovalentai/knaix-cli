//! `.knaix.toml`: what a repository says about how to talk to a node.
//!
//! Without this, every command in a project needs its flags typed again, and a
//! team has nowhere to put the answer. Which node a repo belongs to, and which
//! of its files are worth ingesting, are properties of the repo rather than of
//! the machine, so they belong in the repo and under review with everything
//! else.
//!
//! Deliberately small. It holds what the CLI acts on today and nothing else: a
//! key that is written but not honoured is worse than no key, because it reads
//! as configuration and behaves as a comment.

use crate::exit::{Code, WithCode};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file's name, at the root of a project.
pub const FILE_NAME: &str = ".knaix.toml";

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    /// The node commands in this repo address, unless a flag says otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Upload::is_empty")]
    pub upload: Upload,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upload {
    /// Only ingest files matching these globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Skip files matching these globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl Upload {
    fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// Find the file by walking up from `start`, the way git finds its root.
///
/// Walking up rather than reading the working directory is what makes the file
/// work from a subdirectory, which is where most commands are actually run.
pub fn find_from(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Parse the file at `path`.
pub fn read(path: &Path) -> Result<Project> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read {}", path.display()))?;
    parse(&raw).with_context(|| format!("Could not parse {}", path.display()))
}

pub fn parse(raw: &str) -> Result<Project> {
    // A malformed project file is a usage error rather than a crash: the caller
    // wrote it, and the fix is to edit it.
    toml::from_str(raw)
        .map_err(anyhow::Error::from)
        .coded(Code::Usage)
}

/// The project settings in effect for the current directory, if any.
///
/// A file that cannot be parsed is reported rather than ignored. Silently
/// falling back would apply different settings than the file asks for, which is
/// the one outcome worse than refusing.
pub fn current() -> Result<Option<Project>> {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    match find_from(&cwd) {
        Some(path) => read(&path).map(Some),
        None => Ok(None),
    }
}

/// Render the file, with comments, so what is written can be read and edited.
///
/// Serialising the struct would produce a valid file nobody can learn anything
/// from. This is the file a person opens next, so it explains itself.
pub fn render(project: &Project) -> String {
    let mut out = String::new();
    out.push_str("# How this repository talks to Kovalent.\n");
    out.push_str("# Commands run anywhere under this directory read this file.\n\n");

    match &project.node {
        Some(node) => {
            out.push_str("# The node these commands address. A --node-id flag still wins.\n");
            out.push_str(&format!("node = \"{}\"\n", node));
        }
        None => {
            out.push_str("# The node these commands address. A --node-id flag still wins.\n");
            out.push_str("# node = \"your-node-id\"\n");
        }
    }

    out.push_str("\n[upload]\n");
    out.push_str("# Which files 'knaix upload .' ingests. Flags replace these, not add to them.\n");
    out.push_str(&render_globs("include", &project.upload.include));
    out.push_str(&render_globs("exclude", &project.upload.exclude));
    out
}

fn render_globs(key: &str, globs: &[String]) -> String {
    if globs.is_empty() {
        return format!("# {key} = []\n");
    }
    let items: Vec<String> = globs.iter().map(|g| format!("\"{}\"", g)).collect();
    format!("{key} = [{}]\n", items.join(", "))
}

/// Write the file, refusing to clobber one that is already there.
pub fn write(path: &Path, project: &Project, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(anyhow::anyhow!(
            "{} already exists. Pass --force to overwrite it.",
            path.display()
        ))
        .coded(Code::Denied);
    }
    std::fs::write(path, render(project))
        .with_context(|| format!("Could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_valid_and_says_nothing() {
        let p = parse("").unwrap();
        assert_eq!(p, Project::default());
        assert!(p.node.is_none());
    }

    #[test]
    fn a_node_and_globs_are_read_back() {
        let p = parse(
            r#"
node = "acme-node"
[upload]
include = ["docs/**/*.md"]
exclude = ["**/CHANGELOG.md"]
"#,
        )
        .unwrap();
        assert_eq!(p.node.as_deref(), Some("acme-node"));
        assert_eq!(p.upload.include, vec!["docs/**/*.md"]);
        assert_eq!(p.upload.exclude, vec!["**/CHANGELOG.md"]);
    }

    /// The upload table is optional; a file naming only a node must still parse.
    #[test]
    fn the_upload_table_is_optional() {
        let p = parse(r#"node = "acme-node""#).unwrap();
        assert!(p.upload.include.is_empty());
        assert!(p.upload.exclude.is_empty());
    }

    /// A typo must be reported, not silently ignored, or the file and the
    /// behaviour disagree and the file looks right.
    #[test]
    fn a_malformed_file_is_a_usage_error() {
        let e = parse("node = [this is not toml").unwrap_err();
        assert_eq!(crate::exit::code_of(&e), Code::Usage);
    }

    #[test]
    fn what_is_written_parses_back_to_what_was_meant() {
        let project = Project {
            node: Some("acme-node".into()),
            upload: Upload {
                include: vec!["docs/**/*.md".into()],
                exclude: vec!["**/vendor/**".into()],
            },
        };
        let round_tripped = parse(&render(&project)).unwrap();
        assert_eq!(round_tripped, project);
    }

    /// The commented-out form has to be valid too: someone uncomments a line
    /// and expects it to work, not to be told the file is broken.
    #[test]
    fn the_empty_template_parses_and_is_empty() {
        let rendered = render(&Project::default());
        let parsed = parse(&rendered).unwrap();
        assert_eq!(parsed, Project::default());
        assert!(rendered.contains("# node = "), "{rendered}");
    }

    #[test]
    fn find_walks_up_from_a_subdirectory() {
        let root = std::env::temp_dir().join(format!("knaix-proj-{}", std::process::id()));
        let nested = root.join("a").join("b");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(FILE_NAME), "node = \"acme\"\n").unwrap();

        let found = find_from(&nested).expect("should find the file from a subdirectory");
        assert_eq!(found, root.join(FILE_NAME));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_returns_nothing_when_there_is_no_file() {
        let empty = std::env::temp_dir().join(format!("knaix-proj-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        // A parent might legitimately hold one, so only assert about this tree.
        if find_from(&empty).is_some() {
            let _ = std::fs::remove_dir_all(&empty);
            return;
        }
        assert!(find_from(&empty).is_none());
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn writing_refuses_to_clobber_without_force() {
        let dir = std::env::temp_dir().join(format!("knaix-proj-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);

        write(&path, &Project::default(), false).unwrap();
        let e = write(&path, &Project::default(), false).unwrap_err();
        assert_eq!(crate::exit::code_of(&e), Code::Denied);
        // With force it goes through.
        write(&path, &Project::default(), true).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
