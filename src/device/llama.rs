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
    ensure_installed_from(BASE_URL, client, variant, show_progress).await
}

/// `ensure_installed`, downloading from the releases at `base`.
async fn ensure_installed_from(
    base: &str,
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
            download(client, base, archive, &path, show_progress).await?;
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
    base: &str,
    archive: &Archive,
    path: &Path,
    show_progress: bool,
) -> Result<()> {
    let url = format!("{base}/{TAG}/{}", archive.name);
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
    runtime_env_on(std::env::consts::OS, install, cache)
}

fn runtime_env_on(os: &str, install: &Path, cache: &Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("LLAMA_CACHE".to_string(), cache.display().to_string()),
        ("HOME".to_string(), cache.display().to_string()),
        (
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        ),
    ];
    if os == "linux" {
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

    use crate::config::ScratchHome;
    use crate::exit::code_of;
    use crate::testing::serve_files;

    /// A release archive like llama.cpp's: one top-level directory holding the
    /// server, a library and a link to it, and a file the device does not need.
    #[cfg(unix)]
    fn tarball(home: &Path) -> Vec<u8> {
        let top = home.join("pack/llama-b11669-bin");
        fs::create_dir_all(&top).unwrap();
        fs::write(top.join(BINARY), "#!/bin/sh\n").unwrap();
        fs::write(top.join("libggml-base.so"), "lib").unwrap();
        std::os::unix::fs::symlink("libggml-base.so", top.join("libggml-base.so.0")).unwrap();
        fs::write(top.join("llama-server"), "not needed").unwrap();
        let out = home.join("pack/archive.tar.gz");
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(home.join("pack"))
            .arg("llama-b11669-bin")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        fs::read(out).unwrap()
    }

    fn digest(bytes: &[u8]) -> String {
        crate::device::ticket::hex(&Sha256::digest(bytes))
    }

    /// A variant with one archive, pinned to `sha256`.
    fn variant(id: &str, sha256: String, bytes: u64) -> &'static Variant {
        let name: &'static str = Box::leak(format!("llama-{TAG}-{id}.tar.gz").into_boxed_str());
        let archives =
            Box::leak(vec![a(name, Box::leak(sha256.into_boxed_str()), bytes)].into_boxed_slice());
        Box::leak(Box::new(Variant {
            id: Box::leak(id.to_string().into_boxed_str()),
            backend: Backend::Cpu,
            archives,
        }))
    }

    async fn host(v: &Variant, status: u16, bytes: Vec<u8>) -> String {
        serve_files(vec![(
            format!("/{TAG}/{}", v.archives[0].name),
            status,
            bytes,
        )])
        .await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_pinned_archive_installs_only_what_runs_and_hashes_it() {
        let home = ScratchHome::new();
        let archive = tarball(&home.0);
        let v = variant("test-x", digest(&archive), archive.len() as u64);
        let base = host(v, 200, archive).await;

        let bin = ensure_installed_from(&base, &reqwest::Client::new(), v, true)
            .await
            .unwrap();

        let dir = install_dir(v);
        assert_eq!(bin, dir.join(BINARY));
        let record: InstallRecord =
            serde_json::from_str(&fs::read_to_string(record_path(&dir)).unwrap()).unwrap();
        let names: Vec<&str> = record.files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [BINARY, "libggml-base.so"],
            "links are not hashed, unneeded files not kept"
        );
        assert_eq!(record.files[0].1, sha256_file(&bin).unwrap());
        assert!(
            dir.join("libggml-base.so.0").exists(),
            "the library link is kept"
        );
        assert!(!dir.join("llama-server").exists());
        assert!(fs::read_dir(install_root()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging")));

        // Installed and unchanged: nothing is downloaded again.
        let again =
            ensure_installed_from("http://127.0.0.1:9", &reqwest::Client::new(), v, false).await;
        assert_eq!(again.unwrap(), bin);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_changed_install_is_refused_and_reinstalled() {
        let home = ScratchHome::new();
        let archive = tarball(&home.0);
        let v = variant("test-y", digest(&archive), archive.len() as u64);
        let base = host(v, 200, archive).await;
        let bin = ensure_installed_from(&base, &reqwest::Client::new(), v, false)
            .await
            .unwrap();

        fs::write(&bin, "#!/bin/sh\necho tampered\n").unwrap();
        let err = verify(v).unwrap_err();
        assert_eq!(code_of(&err), Code::Denied);
        assert!(
            err.to_string().starts_with(&format!("{BINARY} in ")),
            "{err}"
        );
        assert!(err.to_string().ends_with(
            "changed since it was installed. Run `knaix device join` again to reinstall it."
        ));

        ensure_installed_from(&base, &reqwest::Client::new(), v, false)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&bin).unwrap(), "#!/bin/sh\n");
    }

    #[tokio::test]
    async fn an_archive_that_is_not_the_pinned_one_installs_nothing() {
        let _home = ScratchHome::new();
        let v = variant("test-z", "0".repeat(64), 5);
        let base = host(v, 200, b"other".to_vec()).await;

        let err = ensure_installed_from(&base, &reqwest::Client::new(), v, false)
            .await
            .unwrap_err();

        assert_eq!(code_of(&err), Code::Denied);
        assert!(
            err.to_string()
                .contains("does not match its pinned checksum"),
            "{err}"
        );
        assert!(!install_dir(v).exists());
        assert!(!install_root()
            .join(format!(".staging-{}", std::process::id()))
            .exists());
    }

    #[tokio::test]
    async fn a_download_that_fails_says_which_archive_and_why() {
        let _home = ScratchHome::new();
        let v = variant("test-w", "0".repeat(64), 5);
        let missing = host(v, 404, Vec::new()).await;
        let client = reqwest::Client::new();

        let err = ensure_installed_from(&missing, &client, v, false)
            .await
            .unwrap_err();
        assert_eq!(code_of(&err), Code::Unavailable);
        assert_eq!(
            err.to_string(),
            format!("Downloading {} failed: 404 Not Found", v.archives[0].name)
        );

        let err = ensure_installed_from("http://127.0.0.1:9", &client, v, false)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Could not download {}", v.archives[0].name)
        );
    }

    #[tokio::test]
    async fn an_archive_tar_cannot_unpack_is_an_error() {
        let _home = ScratchHome::new();
        let junk = b"not a tarball".to_vec();
        let v = variant("test-v", digest(&junk), junk.len() as u64);
        let base = host(v, 200, junk).await;
        let err = ensure_installed_from(&base, &reqwest::Client::new(), v, false)
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("tar could not unpack"), "{err}");
    }

    #[test]
    fn an_install_record_that_is_missing_wrong_or_incomplete_is_refused() {
        let _home = ScratchHome::new();
        let v = variant("test-u", "0".repeat(64), 1);
        let err = verify(v).unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert_eq!(
            err.to_string(),
            format!("llama.cpp {TAG} is not installed for this machine.")
        );

        let dir = install_dir(v);
        fs::create_dir_all(&dir).unwrap();
        fs::write(record_path(&dir), "junk").unwrap();
        assert_eq!(
            verify(v).unwrap_err().to_string(),
            "The llama.cpp install record is unreadable"
        );

        let write = |build: &str, files: Vec<(String, String)>| {
            let r = InstallRecord {
                tag: TAG.into(),
                build: build.into(),
                variant: v.id.into(),
                files,
            };
            fs::write(record_path(&dir), serde_json::to_string(&r).unwrap()).unwrap();
        };
        write("1-old", vec![]);
        let err = verify(v).unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert_eq!(
            err.to_string(),
            format!("The installed llama.cpp is 1-old (test-u), not the pinned {BUILD} (test-u).")
        );

        write(BUILD, vec![("libggml-base.so".into(), "x".into())]);
        assert_eq!(
            verify(v).unwrap_err().to_string(),
            format!("The llama.cpp install has no {BINARY}.")
        );

        write(BUILD, vec![(BINARY.into(), "x".into())]);
        assert!(verify(v)
            .unwrap_err()
            .to_string()
            .starts_with("Could not read"));
    }

    #[test]
    fn only_linux_is_told_where_the_libraries_are() {
        let on = |os| runtime_env_on(os, Path::new("/opt/llama"), Path::new("/c"));
        assert!(on("linux").contains(&("LD_LIBRARY_PATH".into(), "/opt/llama".into())));
        assert!(on("macos").iter().all(|(k, _)| k != "LD_LIBRARY_PATH"));
        assert_eq!(
            runtime_env(Path::new("/i"), Path::new("/c")),
            runtime_env_on(std::env::consts::OS, Path::new("/i"), Path::new("/c"))
        );
    }

    #[test]
    fn every_supported_machine_has_its_archive() {
        for (os, arch, backend, id) in [
            ("macos", "x86_64", Backend::Cpu, "macos-x64"),
            ("linux", "x86_64", Backend::Cpu, "ubuntu-x64"),
            ("linux", "aarch64", Backend::Cpu, "ubuntu-arm64"),
            ("linux", "x86_64", Backend::Vulkan, "ubuntu-vulkan-x64"),
            ("linux", "x86_64", Backend::Rocm, "ubuntu-rocm-10.0-x64"),
        ] {
            assert_eq!(variant_for(os, arch, backend).unwrap().id, id);
        }
        assert_eq!(
            a("n", "s", 1),
            Archive {
                name: "n",
                sha256: "s",
                bytes: 1
            }
        );
    }
}
