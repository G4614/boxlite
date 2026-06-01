//! Structural recovery reserve: a fixed-size file the OS counts as used so
//! `rm` / `gc` always have somewhere to land on a host that's otherwise
//! out of disk.
//!
//! How it replaces the older policy walls:
//!
//! The previous design did `statvfs(home)` + threshold-compare at every
//! host-write CLI / REST handler (6 + 6 entry points) plus an admission
//! task at box start. That scaled badly with new entry points (every one
//! needs to remember to call `enforce_recovery_budget`), was TOCTOU-prone
//! (check → write window), and produced unfriendly errors only when boxlite
//! happened to be the next caller.
//!
//! This module preallocates [`RESERVE_BYTES`] bytes into
//! `$BOXLITE_HOME/.reserve` at runtime init. From then on the kernel
//! enforces the floor for free: every `write(2)` that would push the host
//! filesystem below ~zero hits `ENOSPC`, with no boxlite policy code
//! involved. Releasing the reserve to do recovery is a metadata-only
//! `unlink(2)` of the file (works even at 0 free), surfaced via
//! `boxlite reserve-release`.
//!
//! ## Picking the size
//!
//! [`RESERVE_BYTES`] is sized for "give `boxlite rm` / `boxlite gc` enough
//! headroom to start a SQLite WAL transaction + tar a small archive +
//! survive an ext4 journal flush." Measured floors:
//!
//!  - `boxlite rm` minimum: ~1 MiB (SQLite WAL grow + state row update)
//!  - `boxlite gc` minimum: ~1 MiB (same DB write surface)
//!  - ext4 journal slack:   ~4 MiB on a typical 64 MiB journal
//!
//! 64 MiB is a 50× safety margin — large enough that the recovery commands
//! are never themselves the cause of "still out of disk after release,"
//! small enough that a fresh `boxlite` install on a 1 GiB dev VM still
//! has room for a small image pull after the reserve is laid down.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Bytes preallocated into the reserve file. Pinned at the smallest size
/// proven sufficient for the recovery commands plus a wide safety margin
/// (see module doc); changing this in production is a config decision
/// that wants its own follow-up, not a casual tweak.
pub const RESERVE_BYTES: u64 = 64 * 1024 * 1024;

/// Filename inside `$BOXLITE_HOME` that holds the reserve. Dot-prefixed
/// so a casual `ls $BOXLITE_HOME` doesn't accidentally surface it as
/// something the operator should `rm` — the only legitimate path to
/// remove it is via `boxlite reserve-release`.
pub const RESERVE_FILENAME: &str = ".reserve";

/// Absolute path to the reserve file for a given home dir.
pub fn reserve_path(home_dir: &Path) -> PathBuf {
    home_dir.join(RESERVE_FILENAME)
}

/// Idempotently allocate `RESERVE_BYTES` of real disk into
/// `$home_dir/.reserve`. Safe to call on every runtime construction.
///
/// On a fresh home dir: creates the file with `fallocate(mode=0)` so the
/// blocks are physically allocated (not sparse). The kernel's free-space
/// accounting drops by `RESERVE_BYTES` from that moment on, and every
/// subsequent write — boxlite's, the operator's, anything else's — sees
/// the reduced free as if the disk were that much smaller.
///
/// On a re-open: verifies the file is at least `RESERVE_BYTES` and tops
/// it up if not. An operator who already `rm`'d the reserve in an
/// emergency gets it back the next time a runtime constructs; without
/// the top-up they would silently lose the reserve permanently.
///
/// Probe failure (host fs doesn't support fallocate, falls open):
/// we degrade to zero-writing the file. That covers tmpfs / NFS / some
/// FUSE backends where fallocate returns `EOPNOTSUPP`. The cost is a
/// 64 MiB write at first init, paid once.
pub fn ensure_reserve(home_dir: &Path) -> BoxliteResult<()> {
    let path = reserve_path(home_dir);

    // Already there at the right size? Nothing to do — this is the
    // steady-state hot path on every runtime construction.
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() >= RESERVE_BYTES
    {
        return Ok(());
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| BoxliteError::Storage(format!("open reserve {}: {e}", path.display())))?;

    // Try fallocate first — single syscall, real blocks, fast.
    // SAFETY: fd is owned by `file`, len is positive, mode=0 is the
    // "allocate" variant (not punch-hole / collapse-range).
    let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, RESERVE_BYTES as libc::off_t) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    // Only fall back on EOPNOTSUPP / ENOSYS — anything else is a real
    // failure we want surfaced. EOPNOTSUPP shows up on tmpfs, some FUSE
    // backends, and certain NFS configurations.
    let raw = err.raw_os_error();
    if raw != Some(libc::EOPNOTSUPP) && raw != Some(libc::ENOSYS) {
        return Err(BoxliteError::Storage(format!(
            "fallocate reserve {}: {err}",
            path.display()
        )));
    }

    // Zero-write fallback. This is the one-time cost: 64 MiB sequential
    // write to a fresh file. Use a 4 MiB buffer so we don't pay 64M
    // syscalls.
    let mut file = file;
    let buf = vec![0u8; 4 * 1024 * 1024];
    let mut remaining = RESERVE_BYTES;
    while remaining > 0 {
        let n = (remaining as usize).min(buf.len());
        file.write_all(&buf[..n]).map_err(|e| {
            BoxliteError::Storage(format!(
                "write reserve {} (fallocate-fallback): {e}",
                path.display()
            ))
        })?;
        remaining -= n as u64;
    }
    file.sync_all().map_err(|e| {
        BoxliteError::Storage(format!(
            "fsync reserve {} (fallocate-fallback): {e}",
            path.display()
        ))
    })?;
    Ok(())
}

/// Released bytes; used by the CLI to report the recovered headroom.
#[derive(Debug, Clone, Copy)]
pub struct ReserveReleased {
    pub bytes: u64,
}

/// Remove the reserve file, freeing its blocks back to the host
/// filesystem. Returns the size that was released (0 if the file
/// wasn't there to begin with — idempotent, not an error).
///
/// `unlink(2)` is metadata-only on ext4 / xfs / btrfs: changes a
/// directory entry, drops the inode's refcount. **Doesn't require
/// any free disk** to execute — which is the whole point: when the
/// host filesystem is at 0 free, this is still callable.
pub fn release_reserve(home_dir: &Path) -> BoxliteResult<ReserveReleased> {
    let path = reserve_path(home_dir);
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(ReserveReleased { bytes }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReserveReleased { bytes: 0 }),
        Err(e) => Err(BoxliteError::Storage(format!(
            "remove reserve {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Fresh home: reserve is allocated to exactly `RESERVE_BYTES` and the
    /// host filesystem's `f_bavail` reflects the consumption.
    #[test]
    fn ensure_reserve_allocates_real_blocks() {
        let home = TempDir::new().unwrap();
        let pre = statvfs_bavail_bytes(home.path());
        ensure_reserve(home.path()).expect("ensure_reserve");
        let meta = std::fs::metadata(reserve_path(home.path())).unwrap();
        assert_eq!(meta.len(), RESERVE_BYTES);
        let post = statvfs_bavail_bytes(home.path());
        // Available bytes must drop by at least most of the reserve.
        // Allow some slack for filesystem-block alignment and concurrent
        // tmp activity (the test runs on shared /tmp).
        let drop = pre.saturating_sub(post);
        assert!(
            drop >= RESERVE_BYTES.saturating_sub(4 * 1024 * 1024),
            "ensure_reserve must consume ~{RESERVE_BYTES} bytes of \
             available space; saw a drop of {drop} (pre={pre} post={post})"
        );
    }

    /// Second `ensure_reserve` is a no-op: the file is already at the
    /// right size, so no extra blocks are charged. Pin this so a future
    /// refactor that "always re-fallocates" doesn't silently double
    /// the cost on every runtime construction.
    #[test]
    fn ensure_reserve_is_idempotent() {
        let home = TempDir::new().unwrap();
        ensure_reserve(home.path()).unwrap();
        let after_first = statvfs_bavail_bytes(home.path());
        ensure_reserve(home.path()).unwrap();
        let after_second = statvfs_bavail_bytes(home.path());
        // Same size on disk.
        assert_eq!(
            std::fs::metadata(reserve_path(home.path())).unwrap().len(),
            RESERVE_BYTES
        );
        // No new bytes charged on the second call.
        let delta = after_first.abs_diff(after_second);
        assert!(
            delta <= 4 * 1024 * 1024,
            "idempotent ensure_reserve must not consume more space on a \
             second call; available dropped by {delta} bytes \
             (after_first={after_first} after_second={after_second})"
        );
    }

    /// Emergency case: operator already deleted the reserve and runtime
    /// is constructed again. The next `ensure_reserve` must put it
    /// back. Without this, a one-time emergency release would
    /// permanently lose the floor.
    #[test]
    fn ensure_reserve_recreates_after_external_removal() {
        let home = TempDir::new().unwrap();
        ensure_reserve(home.path()).unwrap();
        std::fs::remove_file(reserve_path(home.path())).unwrap();
        ensure_reserve(home.path()).unwrap();
        assert_eq!(
            std::fs::metadata(reserve_path(home.path())).unwrap().len(),
            RESERVE_BYTES
        );
    }

    /// Release is a metadata-only `unlink` — works even when the host
    /// fs is at 0 free, returns the released byte count, and is
    /// idempotent (second call returns 0 not error).
    #[test]
    fn release_reserve_returns_bytes_and_is_idempotent() {
        let home = TempDir::new().unwrap();
        ensure_reserve(home.path()).unwrap();

        let first = release_reserve(home.path()).expect("first release");
        assert_eq!(
            first.bytes, RESERVE_BYTES,
            "first release must report the full preallocated size"
        );
        assert!(
            !reserve_path(home.path()).exists(),
            "reserve file must be gone after release"
        );

        let second = release_reserve(home.path()).expect("second release");
        assert_eq!(second.bytes, 0, "double release is a no-op, not an error");
    }

    fn statvfs_bavail_bytes(p: &Path) -> u64 {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
        assert_eq!(rc, 0, "statvfs setup");
        s.f_bavail as u64 * s.f_frsize as u64
    }
}
