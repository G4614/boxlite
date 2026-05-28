//! Host disk free-space inspection for the box-startup admission guard.
//!
//! Splits the OS boundary (`statvfs`) from the pure decision (`classify`) so
//! the threshold logic is unit-testable without a real filesystem.

use crate::runtime::constants::disk_guard;
use std::io;
use std::path::Path;

/// Verdict for whether a box may start given current free space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskSpaceVerdict {
    /// Enough free space; proceed silently.
    Ok,
    /// Low on space; proceed but warn the operator. Carries a human message.
    Warn(String),
    /// Critically low; refuse to start. Carries a human message.
    Reject(String),
}

/// Free and total bytes for the filesystem backing `path`.
///
/// Uses `f_bavail` (blocks available to unprivileged users), matching what a
/// box's writes can actually consume — not `f_bfree`, which includes
/// root-reserved blocks.
pub fn available_and_total(path: &Path) -> io::Result<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // SAFETY: c_path is a valid NUL-terminated string; statvfs writes into the
    // zeroed stat struct and we check its return code before reading it.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let frsize = stat.f_frsize as u64;
    let free = stat.f_bavail as u64 * frsize;
    let total = stat.f_blocks as u64 * frsize;
    Ok((free, total))
}

/// Classify free space against the admission thresholds. Pure function.
pub fn classify(free_bytes: u64, total_bytes: u64) -> DiskSpaceVerdict {
    if free_bytes < disk_guard::MIN_FREE_BYTES_HARD {
        return DiskSpaceVerdict::Reject(format!(
            "host disk critically low: {} free (need at least {}); refusing to start box",
            human_bytes(free_bytes),
            human_bytes(disk_guard::MIN_FREE_BYTES_HARD),
        ));
    }

    let fraction = if total_bytes > 0 {
        free_bytes as f64 / total_bytes as f64
    } else {
        1.0
    };

    if free_bytes < disk_guard::MIN_FREE_BYTES_SOFT || fraction < disk_guard::MIN_FREE_FRACTION_SOFT
    {
        return DiskSpaceVerdict::Warn(format!(
            "host disk low: {} free ({:.0}% of {}); a box can exhaust it via volume writes",
            human_bytes(free_bytes),
            fraction * 100.0,
            human_bytes(total_bytes),
        ));
    }

    DiskSpaceVerdict::Ok
}

fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_below_hard_floor() {
        // 512 MiB free, 100 GiB disk → below the 1 GiB hard floor → reject.
        let v = classify(512 * 1024 * 1024, 100 * 1024 * 1024 * 1024);
        assert!(matches!(v, DiskSpaceVerdict::Reject(_)), "got {v:?}");
    }

    #[test]
    fn warn_below_soft_bytes_on_large_disk() {
        // 3 GiB free on a 1 TB disk → above hard, below 5 GiB soft → warn.
        let v = classify(3 * 1024 * 1024 * 1024, 1024 * 1024 * 1024 * 1024);
        assert!(matches!(v, DiskSpaceVerdict::Warn(_)), "got {v:?}");
    }

    #[test]
    fn warn_below_soft_fraction_on_small_disk() {
        // 6 GiB free on a 100 GiB disk = 6% → above byte floors but below the
        // 10% fraction → warn (the percentage tier catches small disks).
        let v = classify(6 * 1024 * 1024 * 1024, 100 * 1024 * 1024 * 1024);
        assert!(matches!(v, DiskSpaceVerdict::Warn(_)), "got {v:?}");
    }

    #[test]
    fn ok_when_plenty_free() {
        // 50 GiB free on a 100 GiB disk → above all thresholds → ok.
        let v = classify(50 * 1024 * 1024 * 1024, 100 * 1024 * 1024 * 1024);
        assert_eq!(v, DiskSpaceVerdict::Ok);
    }

    #[test]
    fn large_disk_at_9_percent_is_not_rejected() {
        // 90 GiB free on a 1 TB disk = ~9%: percentage is warn-only, never a
        // hard reject — 90 GiB is plenty for a box to run.
        let v = classify(90 * 1024 * 1024 * 1024, 1024 * 1024 * 1024 * 1024);
        assert!(matches!(v, DiskSpaceVerdict::Warn(_)), "got {v:?}");
    }

    #[test]
    fn statvfs_reports_positive_free_on_tmp() {
        // Boundary smoke: /tmp exists on any unix host and has some free space.
        let (free, total) = available_and_total(Path::new("/tmp")).expect("statvfs /tmp");
        assert!(total > 0, "total should be positive");
        assert!(free <= total, "free {free} should not exceed total {total}");
    }
}
