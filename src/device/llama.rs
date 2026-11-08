//! The pinned llama.cpp `rpc-server`: which archive, where it lives, and the
//! checks that make it the one we pinned.
//!
//! One build for every device, and the same build the Node Runtime runs as the
//! pool's coordinator: the planner refuses a device whose build differs from
//! its coordinator's (`build_mismatch`), because the RPC protocol is versioned
//! with ggml and a mismatch fails mid-load. So the build here is not "the
//! newest", it is the platform's pin, and it moves with a platform release.
//!
//! The archives are llama.cpp's own release builds from GitHub, each pinned by
//! SHA-256 below. An archive is checked before anything in it is unpacked, and
//! the files that run are hashed at install and checked again before every
//! start, so a binary changed on disk is refused rather than run.

use crate::device::hardware::Backend;
use crate::exit::{Code, WithCode};
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The llama.cpp release tag and the build string it reports, formatted the
/// way the control plane and the Node Runtime compare builds:
/// `<build number>-<short commit>`.
pub const TAG: &str = "b11669";
pub const BUILD: &str = "11669-620d222";
const BASE_URL: &str = "https://github.com/ggml-org/llama.cpp/releases/download";
/// The binary inside each archive. llama.cpp renamed it from `rpc-server`.
pub const BINARY: &str = "ggml-rpc-server";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Archive {
    pub name: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variant {
    pub id: &'static str,
    pub backend: Backend,
    pub archives: &'static [Archive],
}

const fn a(name: &'static str, sha256: &'static str, bytes: u64) -> Archive {
    Archive {
        name,
        sha256,
        bytes,
    }
}

/// Every archive this CLI will install, by platform and backend.
pub const VARIANTS: &[Variant] = &[
    Variant {
        id: "macos-arm64",
        backend: Backend::Metal,
        archives: &[a(
            "llama-b11669-bin-macos-arm64.tar.gz",
            "dbed55493d72bc1ba2babc8770d669c2289b5bc4ee9fb2a4dfc158d45b7d13fd",
            12_191_582,
        )],
    },
    Variant {
        id: "macos-x64",
        backend: Backend::Cpu,
        archives: &[a(
            "llama-b11669-bin-macos-x64.tar.gz",
            "cab28238d78158109b48deb27b05d50e0dcef56275d2e76da9ca8281526c7532",
            11_677_404,
        )],
    },
    Variant {
        id: "ubuntu-x64",
        backend: Backend::Cpu,
        archives: &[a(
            "llama-b11669-bin-ubuntu-x64.tar.gz",
            "cf07c1c112d709bd67e522017471de57ed0ec8bea031d732c5422351c07f65ac",
            17_851_611,
        )],
    },
    Variant {
        id: "ubuntu-arm64",
        backend: Backend::Cpu,
        archives: &[a(
            "llama-b11669-bin-ubuntu-arm64.tar.gz",
            "9ac6fda2058d35677608a06f751a50122ddfa4ce0ddb7f084466b8e79b6d4a99",
            13_900_160,
        )],
    },
    Variant {
        id: "ubuntu-vulkan-x64",
        backend: Backend::Vulkan,
        archives: &[a(
            "llama-b11669-bin-ubuntu-vulkan-x64.tar.gz",
            "e8709e5ba6c1b093dea7f3de57f4aba842f9a188fba6b465782d65d6fcfa4d9f",
            30_497_889,
        )],
    },
    Variant {
        id: "ubuntu-vulkan-arm64",
        backend: Backend::Vulkan,
        archives: &[a(
            "llama-b11669-bin-ubuntu-vulkan-arm64.tar.gz",
            "d283522e479c0d532e3a04d715dd852ffb2d95925caa6d41ef2ba53a65e46a51",
            24_187_561,
        )],
    },
    Variant {
        id: "ubuntu-cuda-12.8-x64",
        backend: Backend::Cuda,
        // The CUDA runtime and cuBLAS ship separately; most machines with an
        // NVIDIA driver do not have them.
        archives: &[
            a(
                "llama-b11669-bin-ubuntu-cuda-12.8-x64.tar.gz",
                "4703ed1a3cd6e8504affb6cb9866f501e96d634d68a002dc9a8251c9e2819b49",
                172_525_113,
            ),
            a(
                "cudart-llama-b11669-bin-ubuntu-cuda-12.8-x64.tar.gz",
                "ab3eda89a82096bcba00b6448218c3c11eec2016b0f38d64884dc29736549f26",
                594_377_811,
            ),
        ],
    },
    Variant {
        id: "ubuntu-rocm-10.0-x64",
        backend: Backend::Rocm,
        archives: &[a(
            "llama-b11669-bin-ubuntu-rocm-10.0-x64.tar.gz",
            "02d36f06accd8fdcb2d41078361b21fb2df5d139f169bf4efcb8424a7dbbef01",
            245_070_738,
        )],
    },
];

/// The archive for this machine and backend, or why there is none.
pub fn variant_for(os: &str, arch: &str, backend: Backend) -> Result<&'static Variant> {
    let id = match (os, arch, backend) {
        ("macos", "aarch64", Backend::Metal) => "macos-arm64",
        ("macos", "x86_64", Backend::Cpu) => "macos-x64",
        ("linux", "x86_64", Backend::Cpu) => "ubuntu-x64",
        ("linux", "aarch64", Backend::Cpu) => "ubuntu-arm64",
        ("linux", "x86_64", Backend::Vulkan) => "ubuntu-vulkan-x64",
        ("linux", "aarch64", Backend::Vulkan) => "ubuntu-vulkan-arm64",
        ("linux", "x86_64", Backend::Cuda) => "ubuntu-cuda-12.8-x64",
        ("linux", "x86_64", Backend::Rocm) => "ubuntu-rocm-10.0-x64",
        ("windows", _, _) => {
            return Err(anyhow!(
                "Compute devices are not supported on Windows yet. Use a Mac with Apple silicon or a Linux machine."
            ))
            .coded(Code::Precondition)
        }
        _ => {
            return Err(anyhow!(
                "There is no {} build of llama.cpp for {os} on {arch}. Try --backend cpu.",
                backend.as_str()
            ))
            .coded(Code::Precondition)
        }
    };
    Ok(VARIANTS
        .iter()
        .find(|v| v.id == id)
        .expect("every mapped id is in VARIANTS"))
}

pub fn install_root() -> PathBuf {
    crate::device::state::knaix_dir().join("llama")
}

/// `~/.knaix/llama/b11669-<variant>`.
pub fn install_dir(variant: &Variant) -> PathBuf {
    install_root().join(format!("{TAG}-{}", variant.id))
}

/// What was installed, and the hash of every file that will run.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct InstallRecord {
    pub tag: String,
    pub build: String,
    pub variant: String,
    /// File name to SHA-256, for the binary and every library beside it.
    pub files: Vec<(String, String)>,
}

fn record_path(dir: &Path) -> PathBuf {
    dir.join("install.json")
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f =
        fs::File::open(path).with_context(|| format!("Could not read {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(crate::device::ticket::hex(&hasher.finalize()))
}

/// Whether a file in an archive is one the device needs: the binary, ggml's
/// libraries (backends load from beside the binary), and CUDA's runtime.
pub fn keep(name: &str) -> bool {
    name == BINARY
        || name == "LICENSE"
        || name.starts_with("libggml")
        || name.starts_with("libcudart")
        || name.starts_with("libcublas")
}

/// Check the installed files against the hashes taken at install.
pub fn verify(variant: &Variant) -> Result<PathBuf> {
    let dir = install_dir(variant);
    let raw = fs::read_to_string(record_path(&dir))
        .map_err(|_| anyhow!("llama.cpp {TAG} is not installed for this machine."))
        .coded(Code::Precondition)?;
    let record: InstallRecord =
        serde_json::from_str(&raw).context("The llama.cpp install record is unreadable")?;
    if record.build != BUILD || record.variant != variant.id {
        return Err(anyhow!(
            "The installed llama.cpp is {} ({}), not the pinned {BUILD} ({}).",
            record.build,
            record.variant,
            variant.id
        ))
        .coded(Code::Precondition);
    }
    if record.files.iter().all(|(name, _)| name != BINARY) {
        return Err(anyhow!("The llama.cpp install has no {BINARY}.")).coded(Code::Precondition);
    }
    for (name, expected) in &record.files {
        let got = sha256_file(&dir.join(name))?;
        if &got != expected {
            return Err(anyhow!(
                "{name} in {} changed since it was installed. Run `knaix device join` again to reinstall it.",
                dir.display()
            ))
            .coded(Code::Denied);
        }
    }
    Ok(dir.join(BINARY))
}

/// Install the pinned build for this variant, unless it is already in place.
pub async fn ensure_installed(
    client: &reqwest::Client,
    variant: &Variant,
    show_progress: bool,
) -> Result<PathBuf> {
    if let Ok(bin) = verify(variant) {
        return Ok(bin);
    }
    let dir = install_dir(variant);
    let staging = install_root().join(format!(".staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    crate::device::state::create_private_dir(&staging)?;

    let result = async {
        for archive in variant.archives {
            let path = staging.join(archive.name);
            download(client, archive, &path, show_progress).await?;
            unpack(&path, &staging.join("unpacked"))?;
            fs::remove_file(&path).ok();
        }
        // Replace any previous install in one step, keeping only what runs.
        let _ = fs::remove_dir_all(&dir);
        crate::device::state::create_private_dir(&dir)?;
        let mut files = Vec::new();
        let unpacked = staging.join("unpacked");
        for entry in fs::read_dir(&unpacked)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !keep(&name) {
                continue;
            }
            let target = dir.join(&name);
            fs::rename(entry.path(), &target)?;
            let meta = fs::symlink_metadata(&target)?;
            if meta.file_type().is_file() {
                files.push((name.clone(), sha256_file(&target)?));
            }
        }
        files.sort();
        let record = InstallRecord {
            tag: TAG.into(),
            build: BUILD.into(),
            variant: variant.id.into(),
            files,
        };
        crate::device::state::write_private(
            &record_path(&dir),
            serde_json::to_string_pretty(&record)?.as_bytes(),
        )?;
        verify(variant)
    }
    .await;
    let _ = fs::remove_dir_all(&staging);
    result
}

/// Stream an archive to disk, hashing as it arrives, and refuse it unless the
/// hash is the pinned one.
async fn download(
    client: &reqwest::Client,
    archive: &Archive,
    path: &Path,
    show_progress: bool,
) -> Result<()> {
    let url = format!("{BASE_URL}/{TAG}/{}", archive.name);
    let resp = client
        .get(&url)
        // Large archives on a slow link: the client's 30 s whole-request
        // timeout is for API calls, not this.
        .timeout(std::time::Duration::from_secs(60 * 60))
        .send()
        .await
        .with_context(|| format!("Could not download {}", archive.name))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "Downloading {} failed: {}",
            archive.name,
            resp.status()
        ))
        .coded(Code::Unavailable);
    }
    let bar = indicatif::ProgressBar::new(archive.bytes);
    if show_progress {
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "  {msg} [{bar:30}] {bytes}/{total_bytes} {eta}",
            )
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
            .progress_chars("=> "),
        );
        bar.set_message(format!("llama.cpp {TAG}"));
    } else {
        bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    let mut file = fs::File::create(path)?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("The download was interrupted")?;
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        bar.inc(chunk.len() as u64);
    }
    file.sync_all()?;
    bar.finish_and_clear();
    let got = crate::device::ticket::hex(&hasher.finalize());
    if got != archive.sha256 {
        let _ = fs::remove_file(path);
        return Err(anyhow!(
            "{} does not match its pinned checksum (got {got}). Nothing from it was installed.",
            archive.name
        ))
        .coded(Code::Denied);
    }
    Ok(())
}

/// Unpack a verified archive, dropping its top-level directory.
fn unpack(archive: &Path, into: &Path) -> Result<()> {
    fs::create_dir_all(into)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .arg("--strip-components=1")
        .status()
        .context("Could not run tar to unpack llama.cpp")?;
    if !status.success() {
        return Err(anyhow!("tar could not unpack {}", archive.display()));
    }
    Ok(())
}

/// The environment rpc-server needs to find its libraries and its cache, and
/// nothing else of the caller's: no tokens, no cloud credentials.
pub fn runtime_env(install: &Path, cache: &Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("LLAMA_CACHE".to_string(), cache.display().to_string()),
        ("HOME".to_string(), cache.display().to_string()),
        (
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        ),
    ];
    if cfg!(target_os = "linux") {
        env.push(("LD_LIBRARY_PATH".to_string(), install.display().to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILD_NUMBER: u32 = 11669;

    #[test]
    fn the_build_string_matches_the_tag() {
        assert_eq!(TAG, format!("b{BUILD_NUMBER}"));
        assert!(BUILD.starts_with(&format!("{BUILD_NUMBER}-")));
        // The control plane's floor (MIN_SAFE_LLAMA_BUILD) is 8585.
        const { assert!(BUILD_NUMBER >= 8585) };
    }

    #[test]
    fn every_archive_belongs_to_the_pinned_build_and_has_a_checksum() {
        for v in VARIANTS {
            assert!(!v.archives.is_empty());
            for a in v.archives {
                assert!(a.name.contains(TAG), "{} is not from {TAG}", a.name);
                assert_eq!(a.sha256.len(), 64);
                assert!(a.sha256.chars().all(|c| c.is_ascii_hexdigit()));
                assert!(a.bytes > 0);
            }
        }
    }

    #[test]
    fn machines_map_to_archives() {
        assert_eq!(
            variant_for("macos", "aarch64", Backend::Metal).unwrap().id,
            "macos-arm64"
        );
        assert_eq!(
            variant_for("linux", "x86_64", Backend::Cuda)
                .unwrap()
                .archives
                .len(),
            2
        );
        assert_eq!(
            variant_for("linux", "aarch64", Backend::Vulkan).unwrap().id,
            "ubuntu-vulkan-arm64"
        );
        assert!(variant_for("linux", "aarch64", Backend::Cuda).is_err());
        assert!(variant_for("macos", "aarch64", Backend::Cuda).is_err());
        let windows = variant_for("windows", "x86_64", Backend::Cpu)
            .unwrap_err()
            .to_string();
        assert!(windows.contains("Windows"), "{windows}");
    }

    #[test]
    fn only_what_runs_is_kept() {
        assert!(keep("ggml-rpc-server"));
        assert!(keep("libggml-cuda.so"));
        assert!(keep("libggml-metal.0.dylib"));
        assert!(keep("libcublasLt.so.12"));
        assert!(!keep("llama-server"));
        assert!(!keep("libllama.so"));
        assert!(!keep("libmtmd.so.0"));
    }

    #[test]
    fn rpc_server_gets_no_inherited_secrets() {
        let env = runtime_env(Path::new("/opt/llama"), Path::new("/c"));
        assert!(env
            .iter()
            .all(|(k, _)| !k.contains("TOKEN") && !k.starts_with("AWS")));
        assert!(env.iter().any(|(k, v)| k == "LLAMA_CACHE" && v == "/c"));
    }
}
