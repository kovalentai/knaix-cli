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

/// Where the checks look: the release host, and the programs that check a
/// signature and a build's provenance. Tests point them at stand-ins.
struct Sources<'a> {
    releases: &'a str,
    cosign: &'a str,
    gh: &'a str,
}

const PUBLISHED: Sources<'static> = Sources {
    releases: RELEASES_URL,
    cosign: "cosign",
    gh: "gh",
};

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

/// The release naming for an OS and architecture, as `std::env::consts`
/// spells them.
///
/// These are the names in the release bucket, which are not the Rust target
/// triples: the bucket has said `darwin`/`linux` and `arm64`/`x86_64` since the
/// first release and the installer reads them.
pub fn platform_slug(os: &str, arch: &str) -> Result<String> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => bail!("No published build for this operating system ({other})."),
    };
    let arch = match arch {
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
async fn published_digest(releases: &str, version: &str, artifact: &str) -> Result<String> {
    let url = format!("{releases}/v{version}/{artifact}.sha256");
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
///
/// Both spellings are tried because the two helpers disagree. `gh --version`
/// works; `cosign --version` exits 1 with "unknown flag", because cosign spells
/// it as a subcommand. Asking only the first way reported cosign as missing on
/// every machine that had it, so the signature check never ran for anyone and
/// said "cosign is not installed" while it sat on the PATH.
fn tool_available(tool: &str) -> bool {
    ["--version", "version"].iter().any(|probe| {
        Command::new(tool)
            .arg(probe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// A private directory to stage the signature bundle in.
///
/// `create_dir` refuses an existing path, so a name another user has already
/// taken fails here rather than being followed somewhere we did not intend. The
/// name carries the pid and a nanosecond clock so repeated runs do not collide
/// with themselves.
fn staging_dir() -> Result<PathBuf> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("knaix-verify-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).context("Could not create a directory to stage the signature")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    Ok(dir)
}

/// Download the signature bundle for an artifact, if one was published.
async fn fetch_bundle(releases: &str, version: &str, artifact: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{releases}/v{version}/{artifact}.cosign.bundle");
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
fn cosign_verify(cosign: &str, binary: &Path, bundle: &Path) -> Outcome {
    let output = Command::new(cosign)
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
fn gh_attestation_verify(gh: &str, binary: &Path) -> Outcome {
    let output = Command::new(gh)
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
async fn run_checks(
    binary: &Path,
    version: &str,
    artifact: &str,
    src: &Sources<'_>,
) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    // The digest is the one check that is never optional. Failing to compute or
    // to fetch it is a failure of the command, not a skipped check, because
    // there is then nothing left that says anything about this file.
    let actual = digest_file(binary)?;
    let expected = published_digest(src.releases, version, artifact).await?;
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

    let bundle = fetch_bundle(src.releases, version, artifact)
        .await
        .unwrap_or(None);
    let signature = match (bundle, tool_available(src.cosign)) {
        (None, _) => Outcome::Skipped(format!(
            "v{version} does not publish a signature for {artifact}"
        )),
        (Some(_), false) => {
            Outcome::Skipped("cosign is not installed, so the signature was not checked".into())
        }
        (Some(bytes), true) => {
            // Staged in a directory of our own rather than straight into the
            // temp dir. On Linux that is /tmp, shared and world-writable, and a
            // predictable name there is a file another user can point somewhere
            // else with a symlink before we write it. create_dir fails rather
            // than follows if the path is already taken.
            let dir = staging_dir()?;
            let path = dir.join(format!("{artifact}.cosign.bundle"));
            std::fs::write(&path, bytes).context("Could not stage the signature bundle")?;
            let outcome = cosign_verify(src.cosign, binary, &path);
            let _ = std::fs::remove_dir_all(&dir);
            outcome
        }
    };
    checks.push(Check {
        name: "signature",
        outcome: signature,
    });

    let provenance = if tool_available(src.gh) {
        gh_attestation_verify(src.gh, binary)
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
    verify(ctx, path, version, strict, &PUBLISHED).await
}

async fn verify(
    ctx: &KnaixContext,
    path: Option<String>,
    version: Option<String>,
    strict: bool,
    src: &Sources<'_>,
) -> Result<()> {
    let (binary, version) = resolve_target(path, version)?;
    let slug = platform_slug(std::env::consts::OS, std::env::consts::ARCH)?;
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

    let checks = run_checks(&binary, &version, &artifact, src).await?;

    if json {
        let report = json_report(&binary, &version, &artifact, &checks);
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", text_report(&checks));
    }

    if let Some(line) = conclude(&checks, &version, strict, json)? {
        println!("{line}");
    }
    Ok(())
}

fn json_report(
    binary: &Path,
    version: &str,
    artifact: &str,
    checks: &[Check],
) -> serde_json::Value {
    serde_json::json!({
        "binary": binary.display().to_string(),
        "version": version,
        "artifact": artifact,
        "checks": checks.iter().map(|c| serde_json::json!({
            "name": c.name,
            "status": c.outcome.word(),
            "detail": c.outcome.detail(),
        })).collect::<Vec<_>>(),
        "verified": is_verified(checks, false),
    })
}

fn text_report(checks: &[Check]) -> String {
    let mut out = String::from("\n");
    for check in checks {
        out += &format!(
            "  {} {:<12} {}\n",
            check.outcome.mark(),
            check.name.bold(),
            check.outcome.detail().dimmed()
        );
    }
    out + "\n"
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

/// Decide the outcome, and the closing line the text output ends with.
///
/// `json` has no closing line: the document already carries
/// `verified`, and a sentence printed after it would leave stdout unparseable.
/// Failures still return an error, which is written to stderr.
fn conclude(checks: &[Check], version: &str, strict: bool, json: bool) -> Result<Option<String>> {
    let failed: Vec<&Check> = checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Failed(_)))
        .collect();

    if !failed.is_empty() {
        let names: Vec<&str> = failed.iter().map(|c| c.name).collect();
        // The build-from-source case reaches here and is not tampering: a local
        // build is not byte-identical to the published one, so it fails the
        // checksum honestly. Telling that user their binary is compromised, with
        // no other explanation offered, is how a check earns a reputation for
        // crying wolf.
        return Err(anyhow!(
            "This binary is not the one published as v{version}: {} failed.\n\
             If you built it yourself, that is expected; this command compares \
             against the released build.\n\
             If you installed it, do not run it: reinstall from \
             https://knaix.com/install.sh and check again.",
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
        return Ok(None);
    }

    Ok(Some(if skipped.is_empty() {
        format!(
            "{} Verified: checksum, signature and provenance.",
            "✓".green()
        )
    } else {
        // Say what was not established, rather than letting a partial result
        // read as a full one.
        format!(
            "{} Checksum verified. Not checked: {}.",
            "✓".green(),
            skipped.join(", ")
        )
    }))
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
        let err = conclude(&checks, "0.5.0", false, false).unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Denied);

        let rendered = format!("{err}");
        assert!(rendered.contains("is not the one published"), "{rendered}");
        assert!(rendered.contains("checksum"), "{rendered}");
        // A local build fails this check honestly, so the message has to offer
        // that reading rather than only the tampering one.
        assert!(rendered.contains("built it yourself"), "{rendered}");
        assert!(rendered.contains("do not run it"), "{rendered}");
    }

    #[test]
    fn strict_refuses_a_check_that_could_not_run() {
        let checks = vec![
            check("checksum", Outcome::Passed("matches".into())),
            check("signature", Outcome::Skipped("cosign missing".into())),
        ];
        assert!(conclude(&checks, "0.5.0", false, false).is_ok());
        let err = conclude(&checks, "0.5.0", true, false).unwrap_err();
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
        assert!(conclude(&checks, "0.5.0", false, true).is_ok());
        assert!(conclude(&checks, "0.5.0", true, true).is_err());
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

    /// The regression this exists for: cosign spells its version as a
    /// subcommand, so probing only `--version` reported it missing on every
    /// machine that had it, and the signature check never ran.
    ///
    /// Uses real binaries rather than a stub. `sh` accepts neither spelling and
    /// is present everywhere this runs, so it stands in for the shape of tool
    /// that answers one way and not the other.
    #[test]
    fn a_tool_is_found_by_either_spelling() {
        // Present, and answers at least one probe.
        assert!(tool_available("cargo"), "cargo should be detected");
        // Absent entirely.
        assert!(!tool_available("knaix-no-such-tool-abcdef"));
    }

    /// The certificate identity must not match a fork's workflow of the same
    /// name, which is what an unanchored pattern would allow.
    #[test]
    fn the_signing_identity_is_anchored_to_this_repository() {
        assert!(CERT_IDENTITY.starts_with('^'));
        assert!(CERT_IDENTITY.ends_with('$'));
        assert!(CERT_IDENTITY.contains("kovalentai/knaix-cli"));
    }

    #[test]
    fn every_supported_platform_has_its_release_name_and_others_are_refused() {
        let slug = |os, arch| platform_slug(os, arch).map_err(|e| e.to_string());
        assert_eq!(slug("macos", "aarch64").unwrap(), "darwin-arm64");
        assert_eq!(slug("linux", "x86_64").unwrap(), "linux-x86_64");
        assert_eq!(slug("windows", "x86_64").unwrap(), "windows-x86_64");
        assert_eq!(
            slug("freebsd", "x86_64").unwrap_err(),
            "No published build for this operating system (freebsd)."
        );
        assert_eq!(
            slug("linux", "riscv64").unwrap_err(),
            "No published build for this architecture (riscv64)."
        );
    }

    #[test]
    fn a_file_with_a_version_is_checked_as_given() {
        let (path, version) = resolve_target(Some("./knaix".into()), Some("0.5.0".into())).unwrap();
        assert_eq!(
            (path, version.as_str()),
            (PathBuf::from("./knaix"), "0.5.0")
        );
    }

    #[test]
    fn the_closing_line_says_what_was_not_checked() {
        let all = vec![
            check("checksum", Outcome::Passed("a".into())),
            check("signature", Outcome::Passed("b".into())),
        ];
        let partial = vec![
            check("checksum", Outcome::Passed("a".into())),
            check("signature", Outcome::Skipped("b".into())),
            check("provenance", Outcome::Skipped("c".into())),
        ];
        let line = |c: &[Check]| conclude(c, "0.5.0", false, false).unwrap().unwrap();
        assert!(line(&all).ends_with("Verified: checksum, signature and provenance."));
        assert!(line(&partial).ends_with("Checksum verified. Not checked: signature, provenance."));
        assert_eq!(conclude(&all, "0.5.0", false, true).unwrap(), None);
    }

    fn mixed() -> Vec<Check> {
        vec![
            check("checksum", Outcome::Passed("matches".into())),
            check("signature", Outcome::Failed("bad signature".into())),
            check("provenance", Outcome::Skipped("no gh".into())),
        ]
    }

    #[test]
    fn the_text_report_marks_each_check_with_its_detail() {
        let report = text_report(&mixed());
        let lines: Vec<&str> = report.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3);
        for (line, (mark, name, detail)) in lines.iter().zip([
            ("✓", "checksum", "matches"),
            ("✗", "signature", "bad signature"),
            ("-", "provenance", "no gh"),
        ]) {
            assert!(
                line.contains(mark) && line.contains(name) && line.contains(detail),
                "{line:?}"
            );
        }
    }

    #[test]
    fn the_json_report_names_every_check_and_the_verdict() {
        let report = json_report(Path::new("/bin/knaix"), "0.5.0", "knaix-x", &mixed());
        assert_eq!(
            report,
            serde_json::json!({
                "binary": "/bin/knaix",
                "version": "0.5.0",
                "artifact": "knaix-x",
                "checks": [
                    {"name": "checksum", "status": "passed", "detail": "matches"},
                    {"name": "signature", "status": "failed", "detail": "bad signature"},
                    {"name": "provenance", "status": "skipped", "detail": "no gh"},
                ],
                "verified": false,
            })
        );
    }

    use crate::config::ScratchHome;
    use axum::{http::StatusCode, http::Uri, Router};
    use std::collections::HashMap;

    /// A release host serving `files` by path, each with a status and body.
    /// Anything else answers 404, as a missing key would.
    async fn releases(files: Vec<(String, u16, Vec<u8>)>) -> String {
        let files: HashMap<String, (u16, Vec<u8>)> =
            files.into_iter().map(|(p, s, b)| (p, (s, b))).collect();
        let app = Router::new().fallback(move |uri: Uri| async move {
            let (status, body) = files.get(uri.path()).cloned().unwrap_or((404, Vec::new()));
            (StatusCode::from_u16(status).unwrap(), body)
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        origin
    }

    fn digest_at(status: u16, body: &str) -> Vec<(String, u16, Vec<u8>)> {
        vec![(
            "/v1.0.0/knaix-x.sha256".into(),
            status,
            body.as_bytes().to_vec(),
        )]
    }

    const HEX: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

    #[tokio::test]
    async fn the_published_digest_is_read_from_the_sidecar_lowercased() {
        let host = releases(digest_at(
            200,
            &format!("{}  knaix-x\n", HEX.to_uppercase()),
        ))
        .await;
        assert_eq!(
            published_digest(&host, "1.0.0", "knaix-x").await.unwrap(),
            HEX
        );
    }

    #[tokio::test]
    async fn a_digest_that_cannot_be_read_says_why() {
        let missing = |status| async move {
            let host = releases(digest_at(status, "")).await;
            published_digest(&host, "1.0.0", "knaix-x")
                .await
                .unwrap_err()
        };
        // The bucket answers 403 for a key that is not there.
        for status in [403, 404] {
            let err = missing(status).await;
            assert_eq!(crate::exit::code_of(&err), Code::NotFound);
            assert_eq!(
                err.to_string(),
                "No published build of v1.0.0 for this platform (knaix-x)."
            );
        }
        let err = missing(500).await;
        assert_eq!(crate::exit::code_of(&err), Code::Unavailable);
        assert!(err
            .to_string()
            .ends_with("answered 500 Internal Server Error. Could not read the published digest."));

        let host = releases(digest_at(200, "not-a-digest")).await;
        let err = published_digest(&host, "1.0.0", "knaix-x")
            .await
            .unwrap_err();
        assert!(
            err.to_string().ends_with("is not a SHA-256 value."),
            "{err}"
        );

        let err = published_digest("http://127.0.0.1:9", "1.0.0", "knaix-x")
            .await
            .unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Unavailable);
    }

    #[tokio::test]
    async fn a_bundle_is_fetched_only_when_published() {
        let path = "/v1.0.0/knaix-x.cosign.bundle".to_string();
        let host = releases(vec![(path, 200, b"bundle".to_vec())]).await;
        assert_eq!(
            fetch_bundle(&host, "1.0.0", "knaix-x").await.unwrap(),
            Some(b"bundle".to_vec())
        );
        assert_eq!(fetch_bundle(&host, "2.0.0", "knaix-x").await.unwrap(), None);
        assert_eq!(
            fetch_bundle("http://127.0.0.1:9", "1.0.0", "knaix-x")
                .await
                .unwrap(),
            None
        );
    }

    /// A stand-in for cosign or gh: answers the version probe, records the rest
    /// of its arguments next to itself, then fails with `stderr` unless `exit`
    /// is 0.
    #[cfg(unix)]
    fn fake_tool(dir: &Path, name: &str, exit: i32, stderr: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1\" in --version|version) exit 0;; esac\n\
                 echo \"$@\" > \"$0.args\"\nprintf '%s\\n' '{stderr}' >&2\nexit {exit}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A thread forking while the file was open for writing makes the first
        // exec fail with "text file busy" until that child execs in turn.
        for _ in 0..100 {
            match Command::new(&path).arg("--version").status() {
                Err(e) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => break,
            }
        }
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    fn args_of(tool: &str) -> String {
        std::fs::read_to_string(format!("{tool}.args")).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn cosign_is_asked_for_this_repositorys_release_identity() {
        let home = ScratchHome::new();
        let ok = fake_tool(&home.0, "cosign-ok", 0, "");
        let bad = fake_tool(
            &home.0,
            "cosign-bad",
            1,
            "Error: none of the signatures matched",
        );
        let (binary, bundle) = (Path::new("/tmp/knaix"), Path::new("/tmp/knaix.bundle"));

        assert_eq!(
            cosign_verify(&ok, binary, bundle),
            Outcome::Passed(format!(
                "signed by the release workflow of {REPO}, verified by cosign"
            ))
        );
        assert_eq!(
            args_of(&ok).trim(),
            format!(
                "verify-blob --bundle /tmp/knaix.bundle --certificate-identity-regexp \
                 {CERT_IDENTITY} --certificate-oidc-issuer {CERT_ISSUER} /tmp/knaix"
            )
        );
        assert_eq!(
            cosign_verify(&bad, binary, bundle),
            Outcome::Failed(
                "cosign rejected the signature: Error: none of the signatures matched".into()
            )
        );
        let missing = home.0.join("no-cosign").to_string_lossy().into_owned();
        assert!(matches!(
            cosign_verify(&missing, binary, bundle),
            Outcome::Skipped(r) if r.starts_with("could not run cosign")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn provenance_tells_a_missing_attestation_from_a_bad_one() {
        let home = ScratchHome::new();
        let binary = Path::new("/tmp/knaix");
        let ok = fake_tool(&home.0, "gh-ok", 0, "");
        let none = fake_tool(&home.0, "gh-none", 1, "Error: HTTP 404: Not Found");
        let bad = fake_tool(&home.0, "gh-bad", 1, "Error: verification failed");

        assert_eq!(
            gh_attestation_verify(&ok, binary),
            Outcome::Passed(format!("built by GitHub Actions in {REPO}"))
        );
        assert_eq!(
            args_of(&ok).trim(),
            format!("attestation verify /tmp/knaix --repo {REPO}")
        );
        assert_eq!(
            gh_attestation_verify(&none, binary),
            Outcome::Skipped("this release does not publish build provenance".into())
        );
        assert_eq!(
            gh_attestation_verify(&bad, binary),
            Outcome::Failed("attestation not verified: Error: verification failed".into())
        );
        let missing = home.0.join("no-gh").to_string_lossy().into_owned();
        assert!(matches!(
            gh_attestation_verify(&missing, binary),
            Outcome::Skipped(r) if r.starts_with("could not run gh")
        ));
    }

    /// A binary, and a release host publishing `digest` and, when `bundle`, a
    /// signature for it, under the name this machine would download.
    async fn published(
        home: &ScratchHome,
        digest: Option<String>,
        bundle: bool,
    ) -> (PathBuf, String, String) {
        let binary = home.0.join("knaix");
        std::fs::write(&binary, b"knaix binary").unwrap();
        let artifact =
            artifact_name(&platform_slug(std::env::consts::OS, std::env::consts::ARCH).unwrap());
        let digest = digest.unwrap_or_else(|| digest_file(&binary).unwrap());
        let mut files = vec![(
            format!("/v1.0.0/{artifact}.sha256"),
            200,
            digest.into_bytes(),
        )];
        if bundle {
            files.push((
                format!("/v1.0.0/{artifact}.cosign.bundle"),
                200,
                b"bundle".to_vec(),
            ));
        }
        (binary, artifact, releases(files).await)
    }

    fn outcomes(checks: &[Check]) -> Vec<(&str, &str)> {
        checks.iter().map(|c| (c.name, c.outcome.word())).collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_published_signed_attested_binary_passes_every_check() {
        let home = ScratchHome::new();
        let (binary, artifact, host) = published(&home, None, true).await;
        let cosign = fake_tool(&home.0, "cosign", 0, "");
        let gh = fake_tool(&home.0, "gh", 0, "");
        let src = Sources {
            releases: &host,
            cosign: &cosign,
            gh: &gh,
        };

        let checks = run_checks(&binary, "1.0.0", &artifact, &src).await.unwrap();

        assert_eq!(
            outcomes(&checks),
            [
                ("checksum", "passed"),
                ("signature", "passed"),
                ("provenance", "passed")
            ]
        );
        // cosign was handed the bundle the host published, staged on disk.
        let staged = args_of(&cosign);
        assert!(
            staged.contains(&format!("{artifact}.cosign.bundle")),
            "{staged}"
        );

        let ctx = KnaixContext::new("text".into());
        let path = Some(binary.to_string_lossy().into_owned());
        verify(&ctx, path.clone(), Some("1.0.0".into()), true, &src)
            .await
            .unwrap();
        let json = KnaixContext::new("json".into());
        verify(&json, path, Some("1.0.0".into()), true, &src)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_binary_that_differs_from_the_release_is_refused() {
        let home = ScratchHome::new();
        let (binary, artifact, host) = published(&home, Some(HEX.into()), false).await;
        let missing = home.0.join("none").to_string_lossy().into_owned();
        let src = Sources {
            releases: &host,
            cosign: &missing,
            gh: &missing,
        };

        let checks = run_checks(&binary, "1.0.0", &artifact, &src).await.unwrap();
        assert_eq!(
            outcomes(&checks),
            [
                ("checksum", "failed"),
                ("signature", "skipped"),
                ("provenance", "skipped")
            ]
        );
        assert_eq!(
            checks[0].outcome.detail(),
            format!(
                "SHA-256 is {}, but v1.0.0 publishes {HEX}",
                digest_file(&binary).unwrap()
            )
        );
        assert_eq!(
            checks[1].outcome.detail(),
            format!("v1.0.0 does not publish a signature for {artifact}")
        );
        assert_eq!(
            checks[2].outcome.detail(),
            "the GitHub CLI is not installed, so provenance was not checked"
        );

        let ctx = KnaixContext::new("text".into());
        let path = Some(binary.to_string_lossy().into_owned());
        let err = verify(&ctx, path, Some("1.0.0".into()), false, &src)
            .await
            .unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Denied);
    }

    #[tokio::test]
    async fn a_published_signature_without_cosign_is_skipped_and_says_so() {
        let home = ScratchHome::new();
        let (binary, artifact, host) = published(&home, None, true).await;
        let missing = home.0.join("none").to_string_lossy().into_owned();
        let src = Sources {
            releases: &host,
            cosign: &missing,
            gh: &missing,
        };

        let checks = run_checks(&binary, "1.0.0", &artifact, &src).await.unwrap();
        assert_eq!(checks[0].outcome.word(), "passed");
        assert_eq!(
            checks[1].outcome,
            Outcome::Skipped("cosign is not installed, so the signature was not checked".into())
        );
    }
}
