//! What this machine can comfortably run.
//!
//! `knaix local setup` used to list every model a server had pulled as an equal
//! choice. On a 16 GB laptop a 30 GB model is in that list, and picking it
//! means swapping to disk and an answer that never really arrives. Sizing the
//! choices against the machine is what turns the list into a recommendation.

use sysinfo::System;

/// Total physical memory, in bytes.
pub fn total_memory() -> u64 {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.total_memory()
}

/// How a model's weights sit against the memory this machine has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Runs with room left for everything else.
    Comfortable,
    /// Runs, but this machine will not have much else to give.
    Tight,
    /// Does not fit. It will swap, and answers arrive at reading speed or not
    /// at all.
    TooLarge,
    /// The server did not say how big it is, so nothing is claimed.
    Unknown,
}

/// Weights this far into total memory still leave room for the OS, the node,
/// and the KV cache the context grows into.
const COMFORTABLE: f64 = 0.45;

/// Past this the weights alone crowd out everything else running.
const USABLE: f64 = 0.70;

/// Judge one model against a memory budget.
pub fn fit(size_bytes: Option<u64>, total_bytes: u64) -> Fit {
    let (Some(size), true) = (size_bytes, total_bytes > 0) else {
        return Fit::Unknown;
    };
    let share = size as f64 / total_bytes as f64;
    if share <= COMFORTABLE {
        Fit::Comfortable
    } else if share <= USABLE {
        Fit::Tight
    } else {
        Fit::TooLarge
    }
}

impl Fit {
    /// The note shown beside a model in the picker.
    pub fn note(self) -> &'static str {
        match self {
            Fit::Comfortable => "fits comfortably",
            Fit::Tight => "tight on this machine",
            Fit::TooLarge => "larger than this machine can hold",
            Fit::Unknown => "",
        }
    }

    /// Whether a choice should be steered away from rather than merely labelled.
    pub fn is_discouraged(self) -> bool {
        matches!(self, Fit::TooLarge)
    }
}

/// Bytes as the size a person would say out loud.
pub fn human_size(bytes: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    if bytes as f64 >= GB {
        format!("{:.1} GB", bytes as f64 / GB)
    } else {
        format!("{:.0} MB", bytes as f64 / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    #[test]
    fn a_small_model_on_a_big_machine_is_comfortable() {
        assert_eq!(fit(Some(4 * GB), 32 * GB), Fit::Comfortable);
        // Exactly on the line is still comfortable.
        assert_eq!(fit(Some(144 * GB / 10), 32 * GB), Fit::Comfortable);
    }

    #[test]
    fn a_model_filling_most_of_memory_is_tight() {
        assert_eq!(fit(Some(10 * GB), 16 * GB), Fit::Tight);
    }

    #[test]
    fn a_model_larger_than_memory_does_not_fit() {
        assert_eq!(fit(Some(30 * GB), 16 * GB), Fit::TooLarge);
        assert!(fit(Some(30 * GB), 16 * GB).is_discouraged());
    }

    #[test]
    fn an_unmeasured_model_is_not_judged() {
        // Only Ollama reports sizes. Everywhere else, saying nothing beats
        // guessing at a number the server never gave.
        assert_eq!(fit(None, 32 * GB), Fit::Unknown);
        assert_eq!(fit(Some(4 * GB), 0), Fit::Unknown);
        assert!(Fit::Unknown.note().is_empty());
        assert!(!Fit::Unknown.is_discouraged());
    }

    #[test]
    fn sizes_read_the_way_they_are_spoken() {
        assert_eq!(human_size(9_608_350_718), "8.9 GB");
        assert_eq!(human_size(367), "0 MB");
        assert_eq!(human_size(6_594_474_711), "6.1 GB");
    }

    #[test]
    fn this_machine_reports_some_memory() {
        // A machine running the test suite has memory; a zero here means the
        // probe failed and every model would come back Unknown.
        assert!(total_memory() > 0);
    }
}
