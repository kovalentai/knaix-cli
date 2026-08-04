//! Proving that a `knaix` binary is the one that was published.
//!
//! The CLI is installed by piping a script into a shell, which is the least
//! verifiable way software is distributed. The published SHA-256 protects the
//! download against corruption in transit, but it is served from the same host
//! as the binary, so anyone able to write one can write the other: on its own a
//! digest proves transfer, not origin. A signature made by the release workflow
//! is what proves origin, and only the workflow can make one.
//!
//! Every check here reports what it actually did. A check that could not run is
//! reported as skipped, with the reason, and never counted as a pass -- an
//! installer that claims to have verified a download it never hashed is the
//! failure this module exists to answer.

use anyhow::{anyhow, bail, Context, Result};
use colored::*;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::exit::{Code, WithCode};
use crate::nodes::KnaixContext;

const RELEASES_URL: &str = "https://releases.knaix.com";
const REPO: &str = "kovalentai/knaix-cli";

/// The workflow allowed to have signed a release, as it appears in the signing
/// certificate. Anchored at both ends so a similarly named workflow in a fork
/// cannot satisfy it.
const CERT_IDENTITY: &str =
    r"^https://github\.com/kovalentai/knaix-cli/\.github/workflows/release\.yml@refs/tags/v.+$";
const CERT_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// What one check concluded.
///
/// `Skipped` carries its reason because a skipped check has to say why in the
/// output; a check that quietly disappears reads as one that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed(String),
    Failed(String),
    Skipped(String),
}

impl Outcome {
    fn mark(&self) -> colored::ColoredString {
        match self {
            Outcome::Passed(_) => "✓".green(),
            Outcome::Failed(_) => "✗".red(),
            Outcome::Skipped(_) => "-".yellow(),
        }
    }

    fn word(&self) -> &'static str {
        match self {
            Outcome::Passed(_) => "passed",
            Outcome::Failed(_) => "failed",
            Outcome::Skipped(_) => "skipped",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Outcome::Passed(d) | Outcome::Failed(d) | Outcome::Skipped(d) => d,
        }
    }
}

/// One named check and what it concluded.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub outcome: Outcome,
}

/// The release naming for the machine this is running on.
///
/// These are the names in the release bucket, which are not the Rust target
/// triples: the bucket has said `darwin`/`linux` and `arm64`/`x86_64` since the
/// first release and the installer reads them.
pub fn platform_slug() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => bail!("No published build for this operating system ({other})."),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        other => bail!("No published build for this architecture ({other})."),
    };
    Ok(format!("{os}-{arch}"))
}

/// The published file name for a platform, including the extension Windows
/// needs in order to be executable.
pub fn artifact_name(slug: &str) -> String {
    if slug.starts_with("windows-") {
        format!("knaix-{slug}.exe")
    } else {
        format!("knaix-{slug}")
    }
}

/// SHA-256 of a file, read in chunks so a binary is never held in memory.
pub fn digest_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Could not read {} to hash it", path.display()))
        .coded(Code::NotFound)?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("Could not read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// The digest published for a version and platform.
async fn published_digest(version: &str, artifact: &str) -> Result<String> {
    let url = format!("{RELEASES_URL}/v{version}/{artifact}.sha256");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Could not reach {url}"))
        .coded(Code::Unavailable)?;

    if response.status() == reqwest::StatusCode::FORBIDDEN
        || response.status() == reqwest::StatusCode::NOT_FOUND
    {
        // The bucket denies listing, so a key that is not there answers 403
        // rather than 404. Both mean the same thing to the caller.
        return Err(anyhow!(
            "No published build of v{version} for this platform ({artifact})."
        ))
        .coded(Code::NotFound);
    }

    if !response.status().is_success() {
        return Err(anyhow!(
            "{url} answered {}. Could not read the published digest.",
            response.status()
        ))
        .coded(Code::Unavailable);
    }

    let body = response.text().await.context("Could not read the digest")?;
    let digest = body.split_whitespace().next().unwrap_or("").to_lowercase();

    // A digest that is not 64 hex characters is not a digest. Comparing against
    // it would report a mismatch and blame the binary for a bad sidecar.
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("The published digest at {url} is not a SHA-256 value.");
    }

    Ok(digest)
}

/// Whether a helper is on PATH.
fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Download the signature bundle for an artifact, if one was published.
async fn fetch_bundle(version: &str, artifact: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{RELEASES_URL}/v{version}/{artifact}.cosign.bundle");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    Ok(Some(response.bytes().await?.to_vec()))
}

/// Verify the signature over a binary with cosign.
///
/// Both the identity of the signing workflow and the issuer that vouched for it
/// are pinned. Without them cosign will verify that *somebody* signed the file,
/// which is not the question being asked.
fn cosign_verify(binary: &Path, bundle: &Path) -> Outcome {
    let output = Command::new("cosign")
        .arg("verify-blob")
        .arg("--bundle")
        .arg(bundle)
        .arg("--certificate-identity-regexp")
        .arg(CERT_IDENTITY)
        .arg("--certificate-oidc-issuer")
        .arg(CERT_ISSUER)
        .arg(binary)
        .output();

    match output {
        Ok(out) if out.status.success() => Outcome::Passed(format!(
            "signed by the release workflow of {REPO}, verified by cosign"
        )),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let reason = err.lines().last().unwrap_or("cosign rejected it").trim();
            Outcome::Failed(format!("cosign rejected the signature: {reason}"))
        }
        Err(e) => Outcome::Skipped(format!("could not run cosign: {e}")),
    }
}

/// Verify the build provenance attestation with the GitHub CLI.
fn gh_attestation_verify(binary: &Path) -> Outcome {
    let output = Command::new("gh")
        .arg("attestation")
        .arg("verify")
        .arg(binary)
        .arg("--repo")
        .arg(REPO)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            Outcome::Passed(format!("built by GitHub Actions in {REPO}"))
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            if no_attestation_published(&err) {
                return Outcome::Skipped(
                    "this release does not publish build provenance".to_string(),
                );
            }
            let reason = err.lines().last().unwrap_or("gh rejected it").trim();
            Outcome::Failed(format!("attestation not verified: {reason}"))
        }
        Err(e) => Outcome::Skipped(format!("could not run gh: {e}")),
    }
}

/// Whether the attestation lookup found nothing, as opposed to finding one that
/// did not verify.
///
/// Releases cut before signing existed have no attestation at all, and the
/// lookup answers 404. Treating that as a failure would condemn every older
/// binary as tampered with, which is both wrong and the quickest way to teach
/// people to ignore this command.
fn no_attestation_published(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("http 404") || s.contains("no attestations found")
}

/// Run every check against one binary.
async fn run_checks(binary: &Path, version: &str, artifact: &str) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    // The digest is the one check that is never optional. Failing to compute or
    // to fetch it is a failure of the command, not a skipped check, because
    // there is then nothing left that says anything about this file.
    let actual = digest_file(binary)?;
    let expected = published_digest(version, artifact).await?;
    checks.push(Check {
        name: "checksum",
        outcome: if actual == expected {
            Outcome::Passed(format!("SHA-256 matches the published digest ({actual})"))
        } else {
            Outcome::Failed(format!(
                "SHA-256 is {actual}, but v{version} publishes {expected}"
            ))
        },
    });

    let bundle = fetch_bundle(version, artifact).await.unwrap_or(None);
    let signature = match (bundle, tool_available("cosign")) {
        (None, _) => Outcome::Skipped(format!(
            "v{version} does not publish a signature for {artifact}"
        )),
        (Some(_), false) => {
            Outcome::Skipped("cosign is not installed, so the signature was not checked".into())
        }
        (Some(bytes), true) => {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("{artifact}.cosign.bundle"));
            std::fs::write(&path, bytes).context("Could not stage the signature bundle")?;
            let outcome = cosign_verify(binary, &path);
            let _ = std::fs::remove_file(&path);
            outcome
        }
    };
    checks.push(Check {
        name: "signature",
        outcome: signature,
    });

    let provenance = if tool_available("gh") {
        gh_attestation_verify(binary)
    } else {
        Outcome::Skipped("the GitHub CLI is not installed, so provenance was not checked".into())
    };
    checks.push(Check {
        name: "provenance",
        outcome: provenance,
    });

    Ok(checks)
}

/// Which binary to check, and which published version to check it against.
fn resolve_target(path: Option<String>, version: Option<String>) -> Result<(PathBuf, String)> {
    match (path, version) {
        // No path: the running binary, against the version it says it is. Not
        // against the newest release -- someone deliberately on an older one is
        // not thereby running something unverified.
        (None, v) => {
            let exe = std::env::current_exe().context("Could not locate the running binary")?;
            Ok((exe, v.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())))
        }
        (Some(p), Some(v)) => Ok((PathBuf::from(p), v)),
        // A file on disk does not say which release it came from, and guessing
        // would produce a mismatch that looks like tampering.
        (Some(_), None) => Err(anyhow!(
            "Checking a file needs the version it should be: knaix verify <PATH> --version <VERSION>"
        ))
        .coded(Code::Usage),
    }
}

pub async fn run(
    ctx: &KnaixContext,
    path: Option<String>,
    version: Option<String>,
    strict: bool,
) -> Result<()> {
    let (binary, version) = resolve_target(path, version)?;
    let slug = platform_slug()?;
    let artifact = artifact_name(&slug);
    let json = ctx.output_format == "json";

    // Commentary belongs outside the document. `ctx.info` suppresses itself when
    // quiet but not when the caller asked for JSON, so a header printed through
    // it would leave `-o json` emitting something no parser accepts.
    if !json {
        ctx.info(&format!(
            "{} {} against v{}",
            "Verifying".blue(),
            binary.display().to_string().bold(),
            version
        ));
    }

    let checks = run_checks(&binary, &version, &artifact).await?;

    if json {
        print_json(&binary, &version, &artifact, &checks)?;
    } else {
        print_text(&checks);
    }

    conclude(&checks, strict, json)
}

fn print_json(binary: &Path, version: &str, artifact: &str, checks: &[Check]) -> Result<()> {
    let body = serde_json::json!({
        "binary": binary.display().to_string(),
        "version": version,
        "artifact": artifact,
        "checks": checks.iter().map(|c| serde_json::json!({
            "name": c.name,
            "status": c.outcome.word(),
            "detail": c.outcome.detail(),
        })).collect::<Vec<_>>(),
        "verified": is_verified(checks, false),
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

fn print_text(checks: &[Check]) {
    println!();
    for check in checks {
        println!(
            "  {} {:<12} {}",
            check.outcome.mark(),
            check.name.bold(),
            check.outcome.detail().dimmed()
        );
    }
    println!();
}

/// Whether the binary is verified.
///
/// A skipped check is not a pass. It does not by itself condemn the binary
/// either, so it only decides the answer under `--strict`, which is what a
/// pipeline wants: there, "could not check" and "failed" are the same event.
fn is_verified(checks: &[Check], strict: bool) -> bool {
    checks.iter().all(|c| match c.outcome {
        Outcome::Passed(_) => true,
        Outcome::Failed(_) => false,
        Outcome::Skipped(_) => !strict,
    })
}

/// Decide the outcome, and say so in the text output.
///
/// `json` suppresses the closing line only: the document already carries
/// `verified`, and a sentence printed after it would leave stdout unparseable.
/// Failures still return an error, which is written to stderr.
fn conclude(checks: &[Check], strict: bool, json: bool) -> Result<()> {
    let failed: Vec<&Check> = checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Failed(_)))
        .collect();

    if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().map(|c| c.name).collect();
        return Err(anyhow!(
            "This binary is not the published one: {} failed. Do not run it; \
             reinstall from https://knaix.com/install.sh and check again.",
            names.join(" and ")
        ))
        .coded(Code::Denied);
    }

    let skipped: Vec<&str> = checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Skipped(_)))
        .map(|c| c.name)
        .collect();

    if strict && !skipped.is_empty() {
        return Err(anyhow!(
            "--strict requires every check to run, and {} could not.",
            skipped.join(" and ")
        ))
        .coded(Code::Precondition);
    }

    if json {
        return Ok(());
    }

    if skipped.is_empty() {
        println!(
            "{} Verified: checksum, signature and provenance.",
            "✓".green()
        );
    } else {
        // Say what was not established, rather than letting a partial result
        // read as a full one.
        println!(
            "{} Checksum verified. Not checked: {}.",
            "✓".green(),
            skipped.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn check(name: &'static str, outcome: Outcome) -> Check {
        Check { name, outcome }
    }

    #[test]
    fn the_digest_is_of_the_file_contents() {
        let dir = std::env::temp_dir().join(format!("knaix-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"knaix").unwrap();
        drop(f);

        // Independently checkable: printf 'knaix' | shasum -a 256
        assert_eq!(
            digest_file(&path).unwrap(),
            format!("{:x}", Sha256::digest(b"knaix"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_not_found_rather_than_a_plain_error() {
        let err = digest_file(Path::new("/nonexistent/knaix")).unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::NotFound);
    }

    /// Windows binaries need the extension or they will not run, and the
    /// published name has to match what the installer downloads.
    #[test]
    fn windows_artifacts_carry_the_exe_extension() {
        assert_eq!(artifact_name("windows-x86_64"), "knaix-windows-x86_64.exe");
        assert_eq!(artifact_name("windows-arm64"), "knaix-windows-arm64.exe");
        assert_eq!(artifact_name("darwin-arm64"), "knaix-darwin-arm64");
        assert_eq!(artifact_name("linux-x86_64"), "knaix-linux-x86_64");
    }

    #[test]
    fn a_skipped_check_is_never_a_pass_under_strict() {
        let checks = vec![
            check("checksum", Outcome::Passed("matches".into())),
            check("signature", Outcome::Skipped("no cosign".into())),
        ];
        assert!(is_verified(&checks, false));
        assert!(!is_verified(&checks, true));
    }

    #[test]
    fn a_failure_condemns_the_binary_in_either_mode() {
        let checks = vec![
            check("checksum", Outcome::Passed("matches".into())),
            check("signature", Outcome::Failed("bad signature".into())),
        ];
        assert!(!is_verified(&checks, false));
        assert!(!is_verified(&checks, true));
    }

    /// The whole point of the command: a mismatch has to be refused, and with a
    /// code a script can act on.
    #[test]
    fn a_checksum_mismatch_exits_denied() {
        let checks = vec![check("checksum", Outcome::Failed("differs".into()))];
        let err = conclude(&checks, false, false).unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Denied);
        assert!(format!("{err}").contains("not the published one"), "{err}");
    }

    #[test]
    fn strict_refuses_a_check_that_could_not_run() {
        let checks = vec![
            check("checksum", Outcome::Passed("matches".into())),
            check("signature", Outcome::Skipped("cosign missing".into())),
        ];
        assert!(conclude(&checks, false, false).is_ok());
        let err = conclude(&checks, true, false).unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Precondition);
    }

    /// A path with no version is refused rather than guessed at, because a
    /// guess produces a mismatch that reads as tampering.
    #[test]
    fn checking_a_file_requires_the_version() {
        let err = resolve_target(Some("./knaix".into()), None).unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Usage);
    }

    #[test]
    fn the_running_binary_defaults_to_its_own_version() {
        let (_, version) = resolve_target(None, None).unwrap();
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    /// The JSON path must print the document and nothing else. A closing
    /// sentence after it is what made `-o json` unparseable elsewhere.
    #[test]
    fn json_output_prints_no_closing_sentence() {
        let checks = vec![
            check("checksum", Outcome::Passed("matches".into())),
            check("signature", Outcome::Skipped("no cosign".into())),
        ];
        // Nothing to assert on stdout from here, so assert the contract that
        // keeps it clean: json concludes without printing and without failing.
        assert!(conclude(&checks, false, true).is_ok());
        assert!(conclude(&checks, true, true).is_err());
    }

    /// A release with no attestation must not be reported as one whose
    /// attestation failed: every binary published before signing existed would
    /// be condemned as tampered with.
    #[test]
    fn a_release_without_provenance_is_skipped_not_failed() {
        let not_found = "Error: HTTP 404: Not Found (https://api.github.com/repos/\
             kovalentai/knaix-cli/attestations/sha256:abc)";
        assert!(no_attestation_published(not_found));
        assert!(no_attestation_published(
            "no attestations found for subject"
        ));
    }

    /// An attestation that exists and does not verify is the signal the command
    /// exists to raise, so it must survive the check above.
    #[test]
    fn a_bad_attestation_is_still_a_failure() {
        assert!(!no_attestation_published(
            "Error: verification failed: certificate identity mismatch"
        ));
        assert!(!no_attestation_published("signature does not match"));
    }

    /// The certificate identity must not match a fork's workflow of the same
    /// name, which is what an unanchored pattern would allow.
    #[test]
    fn the_signing_identity_is_anchored_to_this_repository() {
        assert!(CERT_IDENTITY.starts_with('^'));
        assert!(CERT_IDENTITY.ends_with('$'));
        assert!(CERT_IDENTITY.contains("kovalentai/knaix-cli"));
    }
}
