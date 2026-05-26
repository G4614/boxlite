//! Host snapshot for bench reports.
//!
//! Captured once at run start and embedded in [`super::report::ReportMetadata`]
//! so the comparator can refuse apples-to-oranges diffs (different kernel,
//! different CPU count, different memory ceiling) instead of silently
//! blaming a regression on the code.
//!
//! Everything is best-effort: a missing `/proc` file or an unsupported
//! platform fills the field with `None`/`""`. We never panic — a bench
//! report with a partial host snapshot is more useful than no report.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub kernel: String,
    pub arch: String,
    pub cpu_model: String,
    pub cpu_count: u32,
    pub mem_total_bytes: u64,
}

impl HostInfo {
    /// Best-effort snapshot. Each field falls back to an empty/zero
    /// sentinel independently so a single missing source doesn't take
    /// the whole snapshot down.
    pub fn snapshot() -> Self {
        Self {
            kernel: read_kernel().unwrap_or_default(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_model: read_cpu_model().unwrap_or_default(),
            cpu_count: read_cpu_count(),
            mem_total_bytes: read_mem_total_bytes().unwrap_or(0),
        }
    }
}

fn read_kernel() -> Option<String> {
    // `uname -sr` equivalent via /proc/sys/kernel — no shell-out.
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()?
        .trim()
        .to_string();
    let name = std::fs::read_to_string("/proc/sys/kernel/ostype")
        .ok()?
        .trim()
        .to_string();
    Some(format!("{} {}", name, release))
}

fn read_cpu_model() -> Option<String> {
    // /proc/cpuinfo is Linux-specific. First "model name" line on x86,
    // first "Processor" line on aarch64; we accept both.
    let raw = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            return Some(rest.trim_start_matches([' ', '\t', ':']).trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("Processor") {
            return Some(rest.trim_start_matches([' ', '\t', ':']).trim().to_string());
        }
    }
    None
}

fn read_cpu_count() -> u32 {
    // `std::thread::available_parallelism` respects taskset / cgroup
    // affinity, which is exactly what we want for a bench: we should
    // record what we could actually use, not what the box has plugged
    // in.
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(0)
}

fn read_mem_total_bytes() -> Option<u64> {
    // /proc/meminfo's `MemTotal:` is reported in kB.
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb_str = rest.split_whitespace().next()?;
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// On Linux the snapshot must populate every field meaningfully.
    /// This catches accidental regressions where one of the readers
    /// silently goes None and the bench report ships with an empty
    /// host fingerprint (which would make `compare` think every host
    /// matched).
    #[test]
    fn snapshot_populates_every_field_on_linux() {
        let h = HostInfo::snapshot();
        assert!(!h.kernel.is_empty(), "/proc/sys/kernel/osrelease readable");
        assert!(!h.arch.is_empty(), "ARCH known at compile time");
        // cpu_model can legitimately be empty on exotic platforms; CPU
        // count and memory total must not be 0 on any real host.
        assert!(h.cpu_count > 0, "available_parallelism returned 0");
        assert!(h.mem_total_bytes > 0, "/proc/meminfo MemTotal returned 0");
    }
}
