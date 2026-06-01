//! Per-volume size-capped FS, host-side lifecycle.
//!
//! Layout (from cap to box):
//!   sparse image at `<img_path>`        — bounded host disk consumption
//!     ↓ `mkfs.ext4`
//!   ext4 filesystem inside the image    — in-image FS
//!     ↓ `fuse2fs` (user-space ext4)
//!   `<mount_point>` on host             — caller later virtiofs-shares it into the box
//!     ↓ virtiofs (handled elsewhere)
//!   box's `/data`                        — what the workload sees
//!
//! The in-image ext4 is sized at create time, so every layer above sees
//! ENOSPC at the cap — including the box, which is the point. The host disk
//! file is sparse so the actual on-host bytes consumed track real usage, not
//! the cap.
//!
//! This module owns ONLY the host-side mount lifecycle: create the image,
//! format it, mount it, tear it down. It does not know about boxes or
//! virtiofs — the caller wires the resulting `mount_point` into the VM.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Minimum size for a usable ext4 filesystem. ext4 needs ~10 MiB for journal
/// + reserved blocks + superblock copies before there's any room for user
/// data. Reject smaller requests at create time so the operator gets a clear
/// error instead of an opaque `mkfs` failure.
pub const MIN_SIZED_VOLUME_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum time to wait for `fuse2fs` to register the mount after spawn.
/// fuse2fs prints a banner and registers with the kernel asynchronously; on
/// a healthy host this is < 100 ms but we give substantial slack to ride
/// out a slow CI loop.
const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// A live size-capped volume on the host: image file + mount point + the
/// foreground `fuse2fs` daemon that serves it. Holding this struct keeps
/// the mount alive; [`teardown`] (or `Drop` as a safety net) unmounts and
/// deletes the image.
///
/// [`teardown`]: SizedVolumeMount::teardown
pub struct SizedVolumeMount {
    img_path: PathBuf,
    mount_point: PathBuf,
    /// `Some(child)` while the daemon is alive; `None` after teardown.
    fuse_child: Option<Child>,
}

impl SizedVolumeMount {
    /// Create the sparse image, format ext4 in it, and mount it at
    /// `mount_point` via `fuse2fs`. `mkfs_bin` and `fuse2fs_bin` are explicit
    /// so the caller controls which (bundled vs system) binary is used —
    /// production wires bundled paths, tests use system binaries.
    ///
    /// Side effects on success: `img_path` created (sparse, `size_bytes` long,
    /// ext4-formatted), `mount_point` created if missing, `fuse2fs` daemon
    /// running, mount registered with the kernel. On failure all created
    /// state is rolled back.
    pub fn create(
        img_path: &Path,
        mount_point: &Path,
        size_bytes: u64,
        mkfs_bin: &Path,
        fuse2fs_bin: &Path,
    ) -> BoxliteResult<Self> {
        if size_bytes < MIN_SIZED_VOLUME_BYTES {
            return Err(BoxliteError::Config(format!(
                "volume size must be at least {} bytes \
                 (ext4 needs room for journal + reserved blocks); requested {}",
                MIN_SIZED_VOLUME_BYTES, size_bytes
            )));
        }

        // 1. Create the sparse image. `set_len` reserves the length without
        //    writing zeros — actual on-host consumption tracks real usage.
        let f = std::fs::File::create(img_path).map_err(|e| {
            BoxliteError::Storage(format!("create image {}: {e}", img_path.display()))
        })?;
        f.set_len(size_bytes).map_err(|e| {
            let _ = std::fs::remove_file(img_path);
            BoxliteError::Storage(format!("set image size {}: {e}", img_path.display()))
        })?;
        drop(f);

        // 2. mkfs.ext4 into the image. `-F` forces (file already exists),
        //    `-q` keeps stderr clean unless there's a real error.
        let mke = Command::new(mkfs_bin)
            .args(["-t", "ext4", "-F", "-q"])
            .arg(img_path)
            .output()
            .map_err(|e| {
                let _ = std::fs::remove_file(img_path);
                BoxliteError::Storage(format!("spawn mke2fs ({}): {e}", mkfs_bin.display()))
            })?;
        if !mke.status.success() {
            let _ = std::fs::remove_file(img_path);
            return Err(BoxliteError::Storage(format!(
                "mke2fs {} ({}): {}",
                img_path.display(),
                mke.status,
                String::from_utf8_lossy(&mke.stderr).trim()
            )));
        }

        // 3. Mount point — create if missing.
        std::fs::create_dir_all(mount_point).map_err(|e| {
            let _ = std::fs::remove_file(img_path);
            BoxliteError::Storage(format!("mkdir mount_point {}: {e}", mount_point.display()))
        })?;

        // 4. Spawn fuse2fs in foreground (`-f`) so our Child handle IS the
        //    daemon — we can kill it directly without scraping /proc.
        //    `-o fakeroot` makes the FUSE FS report files as root-owned, so a
        //    later virtiofs share into the box behaves the way an in-VM
        //    block-device mount would.
        let parent_dev = parent_dev_id(mount_point);
        let child = Command::new(fuse2fs_bin)
            .args(["-f", "-o", "fakeroot"])
            .arg(img_path)
            .arg(mount_point)
            .spawn()
            .map_err(|e| {
                let _ = std::fs::remove_file(img_path);
                BoxliteError::Storage(format!("spawn fuse2fs ({}): {e}", fuse2fs_bin.display()))
            })?;

        // 5. Wait for the mount to register with the kernel — its dev id
        //    differs from the parent's once mounted. Poll up to the timeout.
        let deadline = Instant::now() + MOUNT_READY_TIMEOUT;
        let mount = Self {
            img_path: img_path.to_path_buf(),
            mount_point: mount_point.to_path_buf(),
            fuse_child: Some(child),
        };
        loop {
            if mount_dev_id(mount_point) != parent_dev {
                break;
            }
            if Instant::now() >= deadline {
                // Drop will run teardown_impl which kills + cleans up.
                return Err(BoxliteError::Storage(format!(
                    "fuse2fs failed to mount {} at {} within {}s",
                    img_path.display(),
                    mount_point.display(),
                    MOUNT_READY_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(mount)
    }

    /// Host path of the directory the box would see as the volume root.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Explicit unmount + image cleanup. Idempotent. Prefer this over
    /// relying on `Drop` so error context is preserved.
    pub fn teardown(mut self) -> BoxliteResult<()> {
        self.teardown_impl()
    }

    fn teardown_impl(&mut self) -> BoxliteResult<()> {
        // `-z` lazy-detaches so a held FD doesn't keep us stuck.
        let _ = Command::new("fusermount")
            .args(["-u", "-z"])
            .arg(&self.mount_point)
            .status();
        if let Some(mut child) = self.fuse_child.take() {
            // fuse2fs exits on its own after fusermount, but a kill is a safe
            // fast-path if it didn't.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.img_path);
        Ok(())
    }
}

impl Drop for SizedVolumeMount {
    fn drop(&mut self) {
        // Safety net if `teardown` wasn't called. Errors swallowed — we're
        // already on the drop path; nothing left to surface them to.
        let _ = self.teardown_impl();
    }
}

fn parent_dev_id(path: &Path) -> u64 {
    let parent = path.parent().unwrap_or(Path::new("/"));
    std::fs::metadata(parent).map(|m| m.dev()).unwrap_or(0)
}

fn mount_dev_id(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_mkfs() -> PathBuf {
        for p in ["/usr/sbin/mke2fs", "/sbin/mke2fs", "/usr/bin/mke2fs"] {
            if Path::new(p).exists() {
                return PathBuf::from(p);
            }
        }
        panic!("mke2fs not found in standard paths");
    }

    fn system_fuse2fs() -> Option<PathBuf> {
        for p in ["/usr/bin/fuse2fs", "/usr/sbin/fuse2fs", "/bin/fuse2fs"] {
            if Path::new(p).exists() {
                return Some(PathBuf::from(p));
            }
        }
        None
    }

    /// Below the minimum size → `Config` error before any fs work happens.
    #[test]
    fn rejects_too_small_size() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("tiny.img");
        let mnt = tmp.path().join("tinymnt");
        let mkfs = PathBuf::from("/usr/sbin/mke2fs"); // unused at this guard
        let fuse = PathBuf::from("/usr/bin/fuse2fs"); // unused
        let err = SizedVolumeMount::create(&img, &mnt, 1024 * 1024, &mkfs, &fuse)
            .err()
            .expect("must reject sizes below the ext4 minimum");
        assert!(matches!(err, BoxliteError::Config(_)), "got {err:?}");
        assert!(!img.exists(), "no image must be created on size validation failure");
    }

    /// End-to-end: mount, write past the cap → ENOSPC, teardown deletes the image.
    /// Skipped when `fuse2fs` isn't installed (CI without `fuse2fs` package).
    #[test]
    fn create_hits_enospc_at_cap_and_teardown_cleans_up() {
        let Some(fuse) = system_fuse2fs() else {
            eprintln!("SKIP: fuse2fs not installed (apt install fuse2fs)");
            return;
        };
        let mkfs = system_mkfs();

        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("vol.img");
        let mnt = tmp.path().join("mnt");

        let mount = SizedVolumeMount::create(&img, &mnt, 16 * 1024 * 1024, &mkfs, &fuse)
            .expect("create");
        assert!(img.exists(), "image file must exist after create");

        // Write up to and past 16 MiB — must hit ENOSPC well before 32 MiB.
        let target = mount.mount_point().join("payload");
        let mut f = std::fs::File::create(&target).expect("create payload");
        let chunk = vec![0xAA_u8; 1024 * 1024];
        let mut wrote = 0u64;
        let mut hit_enospc = false;
        for _ in 0..32 {
            use std::io::Write;
            match f.write_all(&chunk) {
                Ok(()) => wrote += chunk.len() as u64,
                Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                    hit_enospc = true;
                    break;
                }
                Err(e) => panic!("unexpected write error after {wrote} bytes: {e}"),
            }
        }
        assert!(
            hit_enospc,
            "must hit ENOSPC before writing past the 16 MiB cap; wrote {wrote} bytes"
        );
        drop(f);

        mount.teardown().expect("teardown");
        assert!(!img.exists(), "teardown must delete the image file");
    }
}
