//! Disk-usage view for `boxlite df`.
//!
//! The disk protection model after the #618 redesign is:
//!
//!   * structural fallocate reserve (`~/.boxlite/.reserve`, default
//!     64 MiB) as a kernel-level floor — the policy walls
//!     (`enforce_recovery_budget`) it replaced no longer exist;
//!   * `boxlite gc` for proactive cache reclaim
//!     (see [`crate::runtime::gc`]).
//!
//! What an operator actually wants to see in one shot, given that model:
//!
//! 1. **Host headroom** — total / free / used%, plus the **reserve health**
//!    (present? at full size? released?) so the operator knows whether the
//!    crisis-recovery floor is in place.
//! 2. **Where the home dir's space went** — per-category sum (boxes /
//!    bases / images / other) so "10 GiB grew overnight, why" has somewhere
//!    structured to look without `du -sh ~/.boxlite/*`.
//! 3. **How much GC would free** — a dry-run [`crate::runtime::gc::collect_garbage`]
//!    so the operator can decide "should I run `boxlite gc`" without
//!    actually running it.
//!
//! Local backends only. REST runtimes return `Unsupported` (the disk being
//! described is the server's; a future REST endpoint is the right place
//! for it).

use crate::runtime::gc::{GcOptions, GcReport};
use crate::runtime::rt_impl::RuntimeImpl;
use crate::util::reserve::{RESERVE_BYTES, reserve_path};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::{Path, PathBuf};

/// Health of the structural fallocate reserve. The reserve is the only
/// crisis-recovery mechanism after #618; an operator checking `df` wants
/// to know it's there before they need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveStatus {
    /// File present, on-disk size matches the documented constant.
    /// Steady-state — recovery floor intact.
    Healthy { bytes: u64 },
    /// File present but smaller than the constant. Top-up `fallocate`
    /// could not extend it (host was full when the runtime last ran
    /// `ensure_reserve`). Recovery budget partially eroded.
    Partial { bytes: u64, expected: u64 },
    /// No reserve file. Either the runtime has not yet booted on this
    /// home, or the file was unlinked by `boxlite reserve-release`
    /// (crisis recovery). Until `ensure_reserve` runs again, the host
    /// has no kernel-level floor protecting `rm`/`gc` from a hard ENOSPC.
    Absent,
}

/// Free / total / fill summary for the filesystem hosting `~/.boxlite/`,
/// plus the reserve health.
#[derive(Debug, Clone)]
pub struct HostUsage {
    pub home_dir: PathBuf,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub reserve: ReserveStatus,
}

/// Per-category footprint inside `~/.boxlite/`. Sizes are recursive (sum
/// of every file under the dir). Missing dirs report 0 — a fresh home that
/// hasn't pulled an image has no `images/` dir and that's a valid 0, not
/// an error.
#[derive(Debug, Clone, Default)]
pub struct HomeFootprint {
    /// `boxes/` — per-box workspace (overlay qcow2 + logs + sockets).
    pub boxes_bytes: u64,
    /// `bases/` — immutable snapshot / clone bases.
    pub bases_bytes: u64,
    /// `images/` — pulled OCI layers + manifest cache + built disk-images.
    pub images_bytes: u64,
    /// Everything else under home: `db/`, `logs/`, `tmp/`, `locks/`, etc.
    pub other_bytes: u64,
}

impl HomeFootprint {
    pub fn total_bytes(&self) -> u64 {
        self.boxes_bytes + self.bases_bytes + self.images_bytes + self.other_bytes
    }
}

/// Aggregate `boxlite df` view. The reclaimable count comes from a
/// dry-run [`crate::runtime::gc::collect_garbage`] so the figures use the
/// *same* sweep logic that `boxlite gc` would apply — no risk of a
/// "df says 4 GiB, gc reclaims 2 GiB" drift.
#[derive(Debug, Clone)]
pub struct DiskUsageReport {
    pub host: HostUsage,
    pub home: HomeFootprint,
    pub reclaimable: GcReport,
}

impl RuntimeImpl {
    /// Build a [`DiskUsageReport`]. Per-piece errors degrade gracefully:
    /// a per-dir read failure logs and reports 0; a statvfs failure
    /// reports 0/0 host headroom and is logged. Partial visibility is
    /// more useful than a hard error.
    pub fn disk_usage(&self) -> BoxliteResult<DiskUsageReport> {
        let home_dir = self.layout.home_dir().to_path_buf();

        let (free_bytes, total_bytes) = host_statvfs(&home_dir).unwrap_or_else(|e| {
            tracing::warn!(
                path = %home_dir.display(),
                error = %e,
                "statvfs failed during df; reporting 0/0 host headroom"
            );
            (0, 0)
        });
        let reserve = reserve_status(&home_dir);
        let host = HostUsage {
            home_dir: home_dir.clone(),
            total_bytes,
            free_bytes,
            reserve,
        };

        let home = HomeFootprint {
            boxes_bytes: dir_size_or_zero(&self.layout.boxes_dir()),
            bases_bytes: dir_size_or_zero(&self.layout.bases_dir()),
            images_bytes: dir_size_or_zero(&self.layout.images_dir()),
            other_bytes: dir_size_or_zero(&self.layout.db_dir())
                + dir_size_or_zero(&self.layout.logs_dir())
                + dir_size_or_zero(&self.layout.temp_dir())
                + dir_size_or_zero(&self.layout.locks_dir()),
        };

        let reclaimable = self.collect_garbage(&GcOptions { dry_run: true })?;

        Ok(DiskUsageReport {
            host,
            home,
            reclaimable,
        })
    }
}

/// Classify the reserve file at `<home>/.reserve`.
fn reserve_status(home_dir: &Path) -> ReserveStatus {
    let path = reserve_path(home_dir);
    match std::fs::metadata(&path) {
        Ok(m) => {
            let bytes = m.len();
            if bytes >= RESERVE_BYTES {
                ReserveStatus::Healthy { bytes }
            } else {
                ReserveStatus::Partial {
                    bytes,
                    expected: RESERVE_BYTES,
                }
            }
        }
        Err(_) => ReserveStatus::Absent,
    }
}

/// `(free_bytes, total_bytes)` for the filesystem hosting `path`. Wraps
/// `statvfs(2)` directly — `crate::util::reserve` keeps the same dance
/// private for its own use; df just re-rolls it to avoid widening
/// reserve.rs's surface for a single caller.
fn host_statvfs(path: &Path) -> BoxliteResult<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|e| {
        BoxliteError::Storage(format!(
            "path {} to CString for statvfs: {e}",
            path.display()
        ))
    })?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if rc != 0 {
        return Err(BoxliteError::Storage(format!(
            "statvfs {} (for df): {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    let frsize = s.f_frsize as u64;
    Ok((s.f_bavail as u64 * frsize, s.f_blocks as u64 * frsize))
}

/// Recursive byte total for `path`, or 0 if the dir doesn't exist / can't
/// be read. Mirrors the swallow-errors posture of [`crate::runtime::gc`].
fn dir_size_or_zero(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in entries.flatten() {
            let kind = match entry.file_type() {
                Ok(k) => k,
                Err(_) => continue,
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file()
                && let Ok(meta) = entry.metadata()
            {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::options::BoxliteOptions;
    use crate::util::reserve::ensure_reserve;
    use boxlite_test_utils::home::PerTestBoxHome;

    /// Fresh runtime → host headroom non-zero, reserve healthy at exactly
    /// `RESERVE_BYTES`, every footprint category 0 except possibly `other`
    /// (db init may write a sqlite file), zero reclaimable.
    #[test]
    fn fresh_runtime_reports_healthy_reserve_and_zero_reclaim() {
        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");
        // RuntimeImpl::new doesn't lay down the reserve (that's done by
        // BoxliteRuntime::new at the layer above). Do it explicitly so
        // the test asserts the steady-state shape, not the bootstrap one.
        ensure_reserve(runtime.layout.home_dir()).expect("reserve");

        let r = runtime.disk_usage().expect("df");
        assert!(r.host.total_bytes > 0, "host total should be non-zero");
        assert!(r.host.free_bytes > 0, "host free should be non-zero");
        assert_eq!(
            r.host.reserve,
            ReserveStatus::Healthy {
                bytes: RESERVE_BYTES
            },
            "fresh runtime + ensure_reserve = exactly RESERVE_BYTES on disk"
        );
        assert_eq!(r.home.boxes_bytes, 0);
        assert_eq!(r.home.bases_bytes, 0);
        assert_eq!(r.home.images_bytes, 0);
        assert_eq!(
            r.reclaimable.total_bytes(),
            0,
            "no orphans on a fresh runtime"
        );
        assert!(r.reclaimable.dry_run, "df must use dry-run for reclaim");
    }

    /// `boxlite reserve-release` unlinks the file. After that `df` must
    /// report `Absent` — this is what tells an operator "I just used my
    /// 64 MiB recovery budget, top it up by restarting the runtime."
    #[test]
    fn released_reserve_shows_absent() {
        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");
        ensure_reserve(runtime.layout.home_dir()).expect("reserve");

        // Sanity: file is there, df agrees.
        assert!(matches!(
            runtime.disk_usage().unwrap().host.reserve,
            ReserveStatus::Healthy { .. }
        ));

        // Crisis recovery — unlink the reserve.
        std::fs::remove_file(reserve_path(runtime.layout.home_dir()))
            .expect("unlink reserve to simulate release");

        assert_eq!(
            runtime.disk_usage().unwrap().host.reserve,
            ReserveStatus::Absent
        );
    }

    /// `total_bytes()` is the sum of the four categories. Pins the
    /// JSON-emitted shape: scripts can rely on `boxes + bases + images +
    /// other = total` without a round-trip through `du`.
    #[test]
    fn home_footprint_total_equals_category_sum() {
        let f = HomeFootprint {
            boxes_bytes: 100,
            bases_bytes: 200,
            images_bytes: 300,
            other_bytes: 50,
        };
        assert_eq!(f.total_bytes(), 650);
    }
}
