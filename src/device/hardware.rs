//! What this machine can offer a pool: its backend and its usable memory.
//!
//! Usable memory is what is left after the machine's own headroom, and it is
//! measured again on every heartbeat: a machine that starts a game and loses
//! VRAM reports less, and the planner re-plans without it. `--max-memory` only
//! ever lowers the offer.
//!
//! The parsers are pure functions over each tool's output, so the policy is
//! tested against captured output rather than whatever GPU the test machine has.

use crate::exit::{Code, WithCode};
use anyhow::{anyhow, Result};
use std::process::Command;
use std::time::Duration;

pub const GIB: u64 = 1 << 30;
/// The planner excludes anything smaller, so enrolling it would only add a
/// device that is never used.
pub const MIN_USABLE: u64 = GIB;
/// The CUDA 12.8 build needs at least this driver.
pub const MIN_NVIDIA_DRIVER: u32 = 570;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Metal,
    Cuda,
    Rocm,
    Vulkan,
    Cpu,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Metal => "metal",
            Backend::Cuda => "cuda",
            Backend::Rocm => "rocm",
            Backend::Vulkan => "vulkan",
            Backend::Cpu => "cpu",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "metal" => Ok(Backend::Metal),
            "cuda" => Ok(Backend::Cuda),
            "rocm" => Ok(Backend::Rocm),
            "vulkan" => Ok(Backend::Vulkan),
            "cpu" => Ok(Backend::Cpu),
            other => Err(anyhow!(
                "Unknown backend '{other}'. Use metal, cuda, rocm, vulkan or cpu."
            ))
            .coded(Code::Usage),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub backend: Backend,
    pub total_bytes: u64,
    pub usable_bytes: u64,
    /// One line for the owner: "2 × NVIDIA GeForce RTX 4090", "Apple silicon".
    pub detail: String,
}

/// One GPU as nvidia-smi or rocm-smi reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// `nvidia-smi --query-gpu=memory.total,memory.free,driver_version,name
///  --format=csv,noheader,nounits`: one line per GPU, MiB.
pub fn parse_nvidia(output: &str) -> (Vec<Gpu>, Option<u32>) {
    let mut gpus = Vec::new();
    let mut driver = None;
    for line in output.lines() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 4 {
            continue;
        }
        let (Ok(total), Ok(free)) = (fields[0].parse::<u64>(), fields[1].parse::<u64>()) else {
            continue;
        };
        if driver.is_none() {
            driver = fields[2]
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok());
        }
        gpus.push(Gpu {
            name: fields[3..].join(", "),
            total_bytes: total * 1024 * 1024,
            free_bytes: free * 1024 * 1024,
        });
    }
    (gpus, driver)
}

/// `rocm-smi --showmeminfo vram --json`: bytes, as strings, per card.
pub fn parse_rocm(output: &str) -> Vec<Gpu> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    let Some(cards) = v.as_object() else {
        return Vec::new();
    };
    let mut gpus = Vec::new();
    for (card, info) in cards {
        if !card.starts_with("card") {
            continue;
        }
        let read = |key: &str| -> Option<u64> {
            info.get(key).and_then(|x| {
                x.as_u64()
                    .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
            })
        };
        let (Some(total), Some(used)) = (
            read("VRAM Total Memory (B)"),
            read("VRAM Total Used Memory (B)"),
        ) else {
            continue;
        };
        gpus.push(Gpu {
            name: card.clone(),
            total_bytes: total,
            free_bytes: total.saturating_sub(used),
        });
    }
    gpus
}

/// What a backend offers, before `--max-memory` and the floor.
pub fn policy(backend: Backend, ram_bytes: u64, gpus: &[Gpu]) -> (u64, u64) {
    match backend {
        // In line with `hardware.rs`'s USABLE, minus room for the OS and apps.
        Backend::Metal => {
            let usable = ((ram_bytes as f64) * 0.70) as u64;
            (ram_bytes, usable.saturating_sub(2 * GIB))
        }
        Backend::Cuda | Backend::Rocm => {
            let total: u64 = gpus.iter().map(|g| g.total_bytes).sum();
            let free: u64 = gpus.iter().map(|g| g.free_bytes).sum();
            (total, ((free as f64) * 0.90) as u64)
        }
        Backend::Cpu => (ram_bytes, ram_bytes / 2),
        // No reliable cross-vendor reading; the owner states it.
        Backend::Vulkan => (0, 0),
    }
}

/// Apply `--max-memory` (downward only) and the floor.
pub fn finish(backend: Backend, total: u64, usable: u64, max: Option<u64>) -> Result<(u64, u64)> {
    let (total, usable) = match (backend, max) {
        (Backend::Vulkan, Some(m)) => (m, m),
        (Backend::Vulkan, None) => {
            return Err(anyhow!(
                "Vulkan does not report how much memory a GPU has free. Say how much to offer with --max-memory, for example --max-memory 12G."
            ))
            .coded(Code::Usage)
        }
        (_, Some(m)) => (total, usable.min(m)),
        (_, None) => (total, usable),
    };
    if usable < MIN_USABLE {
        return Err(anyhow!(
            "This machine can offer {} to a pool, under the 1 GB a pool can use. Free some memory, or use a machine with more.",
            crate::hardware::human_size(usable)
        ))
        .coded(Code::Precondition);
    }
    Ok((total, usable.min(total)))
}

/// Parse a size such as `12G`, `12GB`, `12GiB`, `8192M` or plain bytes.
pub fn parse_size(raw: &str) -> Result<u64> {
    let s = raw.trim().to_ascii_lowercase();
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let unit = s[digits.len()..].trim();
    let n: f64 = digits
        .parse()
        .map_err(|_| anyhow!("'{raw}' is not a size. Use a number with G or M, for example 12G."))
        .coded(Code::Usage)?;
    let mult = match unit {
        "" | "b" => 1u64,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => {
            return Err(anyhow!(
                "'{raw}' is not a size. Use a number with G or M, for example 12G."
            ))
            .coded(Code::Usage)
        }
    };
    Ok((n * mult as f64) as u64)
}

/// Run a probe with a deadline; None when the tool is absent or fails.
fn probe(program: &str, args: &[&str], wait: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// Asks a tool for its output; None when it is absent or fails.
type Probe<'a> = &'a dyn Fn(&str, &[&str]) -> Option<String>;

fn nvidia(probe: Probe) -> Option<(Vec<Gpu>, Option<u32>)> {
    let out = probe(
        "nvidia-smi",
        &[
            "--query-gpu=memory.total,memory.free,driver_version,name",
            "--format=csv,noheader,nounits",
        ],
    )?;
    let parsed = parse_nvidia(&out);
    (!parsed.0.is_empty()).then_some(parsed)
}

fn rocm(probe: Probe) -> Option<Vec<Gpu>> {
    let out = probe("rocm-smi", &["--showmeminfo", "vram", "--json"])?;
    let gpus = parse_rocm(&out);
    (!gpus.is_empty()).then_some(gpus)
}

fn apple_silicon() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

fn describe(gpus: &[Gpu]) -> String {
    match gpus {
        [] => String::new(),
        [one] => one.name.clone(),
        many if many.iter().all(|g| g.name == many[0].name) => {
            format!("{} × {}", many.len(), many[0].name)
        }
        many => many
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Detect what this machine offers, or use the backend the owner asked for.
pub fn detect(requested: Option<Backend>, max: Option<u64>) -> Result<Offer> {
    let probe = |program: &str, args: &[&str]| probe(program, args, Duration::from_secs(10));
    let ram = crate::hardware::total_memory();
    detect_with(requested, max, ram, apple_silicon(), &probe)
}

/// `detect`, given the machine: its RAM, whether it is Apple silicon, and how
/// to ask the GPU tools.
fn detect_with(
    requested: Option<Backend>,
    max: Option<u64>,
    ram: u64,
    apple: bool,
    probe: Probe,
) -> Result<Offer> {
    let (backend, gpus, detail) = match requested {
        Some(Backend::Metal) | None if apple => {
            (Backend::Metal, Vec::new(), "Apple silicon".to_string())
        }
        Some(Backend::Metal) => {
            return Err(anyhow!("Metal needs a Mac with Apple silicon.")).coded(Code::Usage)
        }
        Some(Backend::Cpu) => (Backend::Cpu, Vec::new(), "CPU".to_string()),
        Some(Backend::Vulkan) => (Backend::Vulkan, Vec::new(), "Vulkan".to_string()),
        Some(Backend::Cuda) => {
            let (gpus, driver) = nvidia(probe)
                .ok_or_else(|| anyhow!("No NVIDIA GPU answered nvidia-smi on this machine."))
                .coded(Code::Precondition)?;
            check_driver(driver)?;
            let d = describe(&gpus);
            (Backend::Cuda, gpus, d)
        }
        Some(Backend::Rocm) => {
            let gpus = rocm(probe)
                .ok_or_else(|| anyhow!("No AMD GPU answered rocm-smi on this machine. The ROCm build needs ROCm installed."))
                .coded(Code::Precondition)?;
            let d = describe(&gpus);
            (Backend::Rocm, gpus, d)
        }
        None => {
            if let Some((gpus, driver)) = nvidia(probe) {
                check_driver(driver)?;
                let d = describe(&gpus);
                (Backend::Cuda, gpus, d)
            } else if let Some(gpus) = rocm(probe) {
                let d = describe(&gpus);
                (Backend::Rocm, gpus, d)
            } else {
                (Backend::Cpu, Vec::new(), "CPU".to_string())
            }
        }
    };
    let (total, usable) = policy(backend, ram, &gpus);
    let (total_bytes, usable_bytes) = finish(backend, total, usable, max)?;
    Ok(Offer {
        backend,
        total_bytes,
        usable_bytes,
        detail,
    })
}

fn check_driver(driver: Option<u32>) -> Result<()> {
    match driver {
        Some(d) if d < MIN_NVIDIA_DRIVER => Err(anyhow!(
            "The NVIDIA driver is version {d}, and the CUDA build needs {MIN_NVIDIA_DRIVER} or newer. Update the driver, or use --backend vulkan --max-memory <size>."
        ))
        .coded(Code::Precondition),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_output_is_summed_per_gpu() {
        let out = "24564, 23000, 575.57.08, NVIDIA GeForce RTX 4090\n24564, 20000, 575.57.08, NVIDIA GeForce RTX 4090\n";
        let (gpus, driver) = parse_nvidia(out);
        assert_eq!(driver, Some(575));
        assert_eq!(gpus.len(), 2);
        assert_eq!(describe(&gpus), "2 × NVIDIA GeForce RTX 4090");
        let (total, usable) = policy(Backend::Cuda, 64 * GIB, &gpus);
        assert_eq!(total, 2 * 24564 * 1024 * 1024);
        assert_eq!(usable, ((43000u64 * 1024 * 1024) as f64 * 0.9) as u64);
    }

    #[test]
    fn junk_lines_in_nvidia_output_are_skipped() {
        let (gpus, driver) = parse_nvidia("No devices were found\n");
        assert!(gpus.is_empty());
        assert_eq!(driver, None);
    }

    #[test]
    fn an_old_driver_is_refused_with_the_way_out() {
        let err = check_driver(Some(535)).unwrap_err().to_string();
        assert!(err.contains("570"), "{err}");
        assert!(err.contains("--backend vulkan"), "{err}");
        assert!(check_driver(Some(570)).is_ok());
        assert!(check_driver(None).is_ok());
    }

    #[test]
    fn rocm_json_reads_used_and_total() {
        let out = r#"{"card0": {"VRAM Total Memory (B)": "17163091968", "VRAM Total Used Memory (B)": "1163091968"}, "system": {}}"#;
        let gpus = parse_rocm(out);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].free_bytes, 16_000_000_000);
        assert!(parse_rocm("not json").is_empty());
    }

    #[test]
    fn metal_keeps_headroom_for_the_mac() {
        let (total, usable) = policy(Backend::Metal, 64 * GIB, &[]);
        assert_eq!(total, 64 * GIB);
        assert_eq!(usable, ((64 * GIB) as f64 * 0.7) as u64 - 2 * GIB);
        // A small Mac offers nothing rather than underflowing.
        assert_eq!(policy(Backend::Metal, 2 * GIB, &[]).1, 0);
    }

    #[test]
    fn cpu_offers_half_the_ram() {
        assert_eq!(policy(Backend::Cpu, 32 * GIB, &[]), (32 * GIB, 16 * GIB));
    }

    #[test]
    fn max_memory_only_lowers_the_offer() {
        assert_eq!(
            finish(Backend::Cpu, 32 * GIB, 16 * GIB, Some(8 * GIB)).unwrap(),
            (32 * GIB, 8 * GIB)
        );
        assert_eq!(
            finish(Backend::Cpu, 32 * GIB, 16 * GIB, Some(64 * GIB)).unwrap(),
            (32 * GIB, 16 * GIB)
        );
    }

    #[test]
    fn vulkan_needs_a_stated_size() {
        assert!(finish(Backend::Vulkan, 0, 0, None).is_err());
        assert_eq!(
            finish(Backend::Vulkan, 0, 0, Some(12 * GIB)).unwrap(),
            (12 * GIB, 12 * GIB)
        );
    }

    #[test]
    fn under_a_gigabyte_is_refused() {
        let err = finish(Backend::Cpu, GIB, GIB / 2, None).unwrap_err();
        assert_eq!(crate::exit::code_of(&err), Code::Precondition);
    }

    #[test]
    fn sizes_parse_in_binary_units() {
        assert_eq!(parse_size("12G").unwrap(), 12 * GIB);
        assert_eq!(parse_size("12gb").unwrap(), 12 * GIB);
        assert_eq!(parse_size("1.5GiB").unwrap(), (1.5 * GIB as f64) as u64);
        assert_eq!(parse_size("8192M").unwrap(), 8 * GIB);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("12Q").is_err());
    }

    #[test]
    fn backends_parse_by_name() {
        assert_eq!(Backend::parse("CUDA").unwrap(), Backend::Cuda);
        assert!(Backend::parse("tpu").is_err());
    }

    use crate::exit::{code_of, Code};

    const RTX: &str = "24564, 23000, 575.57.08, NVIDIA GeForce RTX 4090\n";
    const OLD_DRIVER: &str = "24564, 23000, 535.10.01, NVIDIA GeForce RTX 3090\n";
    const RADEON: &str = r#"{"card0": {"VRAM Total Memory (B)": "17163091968", "VRAM Total Used Memory (B)": "1163091968"}}"#;

    /// A machine whose `nvidia-smi` and `rocm-smi` answer as given, or are absent.
    fn machine(
        nvidia: Option<&'static str>,
        rocm: Option<&'static str>,
    ) -> impl Fn(&str, &[&str]) -> Option<String> {
        move |program, _| match program {
            "nvidia-smi" => nvidia.map(str::to_string),
            "rocm-smi" => rocm.map(str::to_string),
            other => panic!("probed {other}"),
        }
    }

    fn offer(
        requested: Option<Backend>,
        apple: bool,
        gpus: impl Fn(&str, &[&str]) -> Option<String>,
    ) -> Result<Offer> {
        detect_with(requested, Some(64 * GIB), 64 * GIB, apple, &gpus)
    }

    #[test]
    fn a_mac_offers_metal_and_only_a_mac_can() {
        let o = offer(None, true, machine(None, None)).unwrap();
        assert_eq!(
            (o.backend, o.detail.as_str()),
            (Backend::Metal, "Apple silicon")
        );
        assert_eq!(
            offer(Some(Backend::Metal), true, machine(None, None))
                .unwrap()
                .backend,
            Backend::Metal
        );

        let err = offer(Some(Backend::Metal), false, machine(None, None)).unwrap_err();
        assert_eq!(
            (code_of(&err), err.to_string()),
            (Code::Usage, "Metal needs a Mac with Apple silicon.".into())
        );
    }

    #[test]
    fn a_named_backend_is_used_without_probing_for_another() {
        let cpu = offer(Some(Backend::Cpu), false, machine(None, None)).unwrap();
        assert_eq!(
            (cpu.backend, cpu.detail.as_str(), cpu.usable_bytes),
            (Backend::Cpu, "CPU", 32 * GIB)
        );
        let vulkan = offer(Some(Backend::Vulkan), false, machine(None, None)).unwrap();
        assert_eq!(
            (vulkan.backend, vulkan.detail.as_str()),
            (Backend::Vulkan, "Vulkan")
        );
    }

    #[test]
    fn cuda_needs_an_nvidia_gpu_on_a_recent_enough_driver() {
        let o = offer(Some(Backend::Cuda), false, machine(Some(RTX), None)).unwrap();
        assert_eq!(
            (o.backend, o.detail.as_str()),
            (Backend::Cuda, "NVIDIA GeForce RTX 4090")
        );

        let err = offer(Some(Backend::Cuda), false, machine(None, None)).unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert_eq!(
            err.to_string(),
            "No NVIDIA GPU answered nvidia-smi on this machine."
        );
        // A tool that answers with no GPU in it is no GPU.
        assert!(offer(
            Some(Backend::Cuda),
            false,
            machine(Some("No devices were found\n"), None)
        )
        .is_err());

        let err = offer(Some(Backend::Cuda), false, machine(Some(OLD_DRIVER), None)).unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert!(
            err.to_string()
                .starts_with("The NVIDIA driver is version 535"),
            "{err}"
        );
    }

    #[test]
    fn rocm_needs_an_amd_gpu() {
        let o = offer(Some(Backend::Rocm), false, machine(None, Some(RADEON))).unwrap();
        assert_eq!((o.backend, o.detail.as_str()), (Backend::Rocm, "card0"));
        let err = offer(Some(Backend::Rocm), false, machine(None, Some("{}"))).unwrap_err();
        assert_eq!(code_of(&err), Code::Precondition);
        assert!(
            err.to_string().starts_with("No AMD GPU answered rocm-smi"),
            "{err}"
        );
    }

    #[test]
    fn detection_prefers_nvidia_then_amd_then_the_cpu() {
        assert_eq!(
            offer(None, false, machine(Some(RTX), Some(RADEON)))
                .unwrap()
                .backend,
            Backend::Cuda
        );
        assert_eq!(
            offer(None, false, machine(None, Some(RADEON)))
                .unwrap()
                .backend,
            Backend::Rocm
        );
        assert_eq!(
            offer(None, false, machine(None, None)).unwrap().backend,
            Backend::Cpu
        );
        assert!(offer(None, false, machine(Some(OLD_DRIVER), None)).is_err());
    }

    #[test]
    fn gpus_are_described_by_name_and_count() {
        let gpu = |name: &str| Gpu {
            name: name.into(),
            total_bytes: 0,
            free_bytes: 0,
        };
        assert_eq!(describe(&[]), "");
        assert_eq!(describe(&[gpu("A"), gpu("B")]), "A, B");
        assert_eq!(describe(&[gpu("A"), gpu("A")]), "2 × A");
    }

    #[test]
    fn junk_rocm_output_reads_as_no_gpus() {
        assert!(parse_rocm("[1, 2]").is_empty());
        assert!(parse_rocm(r#"{"card0": {}}"#).is_empty());
        let (gpus, _) = parse_nvidia("x, y, 575.1, Name\n");
        assert!(gpus.is_empty());
    }

    #[test]
    fn every_backend_has_the_name_it_parses_from() {
        for b in [
            Backend::Metal,
            Backend::Cuda,
            Backend::Rocm,
            Backend::Vulkan,
            Backend::Cpu,
        ] {
            assert_eq!(Backend::parse(b.as_str()).unwrap(), b);
        }
    }

    #[test]
    fn this_machine_offers_something() {
        let o = detect(Some(Backend::Cpu), None).unwrap();
        assert_eq!(o.total_bytes, crate::hardware::total_memory());
        let auto = detect(None, None).unwrap();
        assert_eq!(auto.backend == Backend::Metal, apple_silicon());
    }

    #[cfg(unix)]
    #[test]
    fn a_probe_gives_up_on_a_tool_that_fails_or_hangs() {
        use crate::testing::fake_tool;
        let home = crate::config::ScratchHome::new();
        let wait = Duration::from_millis(300);
        let ok = fake_tool(&home.0, "ok", "echo answer");
        let failing = fake_tool(&home.0, "failing", "echo partial; exit 1");
        let hung = fake_tool(&home.0, "hung", "exec sleep 30");

        assert_eq!(probe(&ok, &["--q"], wait).as_deref(), Some("answer\n"));
        assert_eq!(probe(&failing, &[], wait), None);
        assert_eq!(probe("/no/such/tool", &[], wait), None);
        let started = std::time::Instant::now();
        assert_eq!(probe(&hung, &[], wait), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited out the hung tool"
        );
    }
}
