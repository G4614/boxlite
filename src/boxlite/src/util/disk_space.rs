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

    /// Severity must never get *stricter* as free space rises (Reject ⊃ Warn ⊃
    /// Ok). Sweeping the whole range catches any threshold inversion or off-by
    /// in `classify`, and pins the exact transitions at the documented floors.
    #[test]
    fn classify_severity_is_monotonic_and_hits_thresholds() {
        fn severity(v: &DiskSpaceVerdict) -> u8 {
            match v {
                DiskSpaceVerdict::Ok => 0,
                DiskSpaceVerdict::Warn(_) => 1,
                DiskSpaceVerdict::Reject(_) => 2,
            }
        }

        const TOTAL: u64 = 100 * 1024 * 1024 * 1024; // 100 GiB
        let step = TOTAL / 1000;
        let mut prev = u8::MAX; // start at most-strict so the first sample can't trip it
        let mut free = 0u64;
        while free <= TOTAL {
            let s = severity(&classify(free, TOTAL));
            assert!(
                s <= prev,
                "verdict got STRICTER as free rose: {free} bytes free → severity {s} > prev {prev}"
            );
            prev = s;
            free += step;
        }

        // Hard floor: just under rejects, exactly at does not.
        assert!(matches!(
            classify(disk_guard::MIN_FREE_BYTES_HARD - 1, TOTAL),
            DiskSpaceVerdict::Reject(_)
        ));
        assert!(!matches!(
            classify(disk_guard::MIN_FREE_BYTES_HARD, TOTAL),
            DiskSpaceVerdict::Reject(_)
        ));

        // Soft *bytes* floor on a disk small enough that the 10% fraction isn't
        // the binding constraint at 5 GiB (5 GiB / 40 GiB = 12.5% > 10%): just
        // under warns, exactly at is Ok.
        const SMALL: u64 = 40 * 1024 * 1024 * 1024;
        assert!(matches!(
            classify(disk_guard::MIN_FREE_BYTES_SOFT - 1, SMALL),
            DiskSpaceVerdict::Warn(_)
        ));
        assert_eq!(
            classify(disk_guard::MIN_FREE_BYTES_SOFT, SMALL),
            DiskSpaceVerdict::Ok
        );
    }

    /// End-to-end stress against the *real* `statvfs` path under genuine disk
    /// pressure. Gated on `BOXLITE_DISKTEST_HOME` pointing at a writable dir on a
    /// SMALL, dedicated filesystem (e.g. a loop-mounted ext4 — never your main
    /// disk). The test fills it until free crosses the hard floor, asserting the
    /// verdict goes from non-Reject to Reject through the production
    /// `available_and_total` + `classify` path, then removes its filler. Skips
    /// when the env var is unset, so CI and normal `make test` stay safe.
    #[test]
    fn real_disk_pressure_crosses_to_reject() {
        let Ok(dir) = std::env::var("BOXLITE_DISKTEST_HOME") else {
            eprintln!(
                "SKIP real_disk_pressure_crosses_to_reject: set BOXLITE_DISKTEST_HOME to a \
                 small (<8 GiB) dedicated FS to exercise real disk pressure"
            );
            return;
        };
        let dir = std::path::PathBuf::from(dir);

        let (free0, total0) = available_and_total(&dir).expect("statvfs BOXLITE_DISKTEST_HOME");
        // Hard safety rail: refuse to fill anything that looks like a real disk.
        assert!(
            total0 > 0 && total0 < 8 * 1024 * 1024 * 1024,
            "BOXLITE_DISKTEST_HOME must be a SMALL (<8 GiB) dedicated FS; got total {} — refusing to fill",
            human_bytes(total0)
        );
        // Empty verdict should not already be a hard reject, or there's nothing
        // to prove by filling.
        assert!(
            !matches!(classify(free0, total0), DiskSpaceVerdict::Reject(_)),
            "FS already below the hard floor ({} free); start with more headroom",
            human_bytes(free0)
        );

        // Remove the filler no matter how the test exits.
        struct Filler(std::path::PathBuf);
        impl Drop for Filler {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let filler = Filler(dir.join("__boxlite_disktest_filler__"));

        use std::io::Write;
        let mut f = std::fs::File::create(&filler.0).expect("create filler");
        let chunk = vec![0u8; 32 * 1024 * 1024]; // 32 MiB
        let mut verdict = classify(free0, total0);
        // Write until the guard would reject, ENOSPC, or a sane cap (256 chunks
        // = 8 GiB, matching the size rail above) to avoid an unbounded loop.
        for _ in 0..256 {
            if matches!(verdict, DiskSpaceVerdict::Reject(_)) {
                break;
            }
            if f.write_all(&chunk).is_err() {
                break; // ENOSPC — the FS is now as full as it gets
            }
            let _ = f.flush();
            let (free, total) = available_and_total(&dir).expect("statvfs during fill");
            verdict = classify(free, total);
        }
        let _ = f.sync_all();

        assert!(
            matches!(verdict, DiskSpaceVerdict::Reject(_)),
            "filling a small FS below the {} hard floor must yield Reject; final verdict {verdict:?}",
            human_bytes(disk_guard::MIN_FREE_BYTES_HARD)
        );
    }
}
