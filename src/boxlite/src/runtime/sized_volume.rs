//! Sized-volume image preparation (host side).
//!
//! Layout (host → box, virtio-blk path):
//!   sparse image at `<img_path>`        — bounded host disk consumption
//!     ↓ `mkfs.ext4`
//!   ext4 inside the image                — sized at create time
//!     ↓ libkrun `krun_add_disk2`
//!   `/dev/vdN` inside the VM            — guest kernel sees it as block device
//!     ↓ guest agent mounts (`BlockDeviceMount`)
//!   box's `/data`                        — what the workload sees
//!
//! Every layer sees ENOSPC at the cap because the underlying ext4 is sized.
//! This module owns ONLY image creation; libkrun wiring and guest mount are
//! handled by the existing block-device path the rootfs already uses.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::Path;
use std::process::Command;

/// Minimum size for a usable ext4 filesystem. ext4 reserves room for the
/// journal, super-block copies, and the inode table; below ~16 MiB there is
/// no room for user data and `mke2fs` either fails or produces a useless
/// volume. Reject smaller requests up front with a clear error.
pub const MIN_SIZED_VOLUME_BYTES: u64 = 16 * 1024 * 1024;

/// Create a sparse image file at `img_path` of `size_bytes` and format it
/// as ext4 in place. The image is **not mounted on the host** — the caller
/// is expected to attach it to the VM as a virtio-blk device (libkrun's
/// `krun_add_disk2`), the guest agent then mounts `/dev/vdN`.
///
/// `mkfs_bin` is explicit so the caller can pick the bundled binary
/// (production: `boxlite::util::find_binary("mke2fs")`) or the system
/// binary (tests).
///
/// Side effects on success: `img_path` is created (sparse, `size_bytes`
/// long, ext4-formatted). On any failure step the image file is removed.
pub fn create_sized_volume_image(
    img_path: &Path,
    size_bytes: u64,
    mkfs_bin: &Path,
) -> BoxliteResult<()> {
    if size_bytes < MIN_SIZED_VOLUME_BYTES {
        return Err(BoxliteError::Config(format!(
            "volume size must be at least {} bytes \
             (ext4 needs room for journal + reserved blocks); requested {}",
            MIN_SIZED_VOLUME_BYTES, size_bytes
        )));
    }

    // 1. Sparse image. `set_len` reserves the length without writing zeros,
    //    so the on-host bytes track real usage, not the cap.
    let f = std::fs::File::create(img_path)
        .map_err(|e| BoxliteError::Storage(format!("create image {}: {e}", img_path.display())))?;
    f.set_len(size_bytes).map_err(|e| {
        let _ = std::fs::remove_file(img_path);
        BoxliteError::Storage(format!("set image size {}: {e}", img_path.display()))
    })?;
    drop(f);

    // 2. mkfs.ext4 in place. `-F` forces (the file already exists), `-q`
    //    keeps stderr clean unless there's a real error.
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn system_mkfs() -> PathBuf {
        for p in ["/usr/sbin/mke2fs", "/sbin/mke2fs", "/usr/bin/mke2fs"] {
            if Path::new(p).exists() {
                return PathBuf::from(p);
            }
        }
        panic!("mke2fs not found in standard paths");
    }

    /// Below the minimum size → `Config` error, no fs work attempted.
    #[test]
    fn rejects_too_small_size() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("tiny.img");
        let mkfs = PathBuf::from("/usr/sbin/mke2fs"); // unused at this guard
        let err = create_sized_volume_image(&img, 1024 * 1024, &mkfs)
            .expect_err("must reject sizes below the ext4 minimum");
        assert!(matches!(err, BoxliteError::Config(_)), "got {err:?}");
        assert!(
            !img.exists(),
            "no image must be created on size validation failure"
        );
    }

    /// The happy path: image created, sparse (on-host bytes ≪ declared length),
    /// ext4-formatted (mke2fs leaves an ext4 superblock the kernel recognises).
    #[test]
    fn creates_sparse_ext4_image() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("vol.img");
        let size = 16 * 1024 * 1024;
        create_sized_volume_image(&img, size, &system_mkfs()).expect("create");

        let meta = std::fs::metadata(&img).expect("stat image");
        assert_eq!(
            meta.len(),
            size,
            "image must be exactly the requested length"
        );
        // Sparse: blocks * 512 should be far smaller than the declared length.
        // After mke2fs there's metadata written (a few hundred KiB) but
        // nowhere near the full 16 MiB.
        let on_disk = meta.blocks() * 512;
        assert!(
            on_disk < size / 2,
            "image must be sparse (on-disk {} bytes vs declared {} bytes)",
            on_disk,
            size
        );

        // mke2fs writes the ext4 super-block magic `0xEF53` at offset 0x438.
        let bytes = std::fs::read(&img).expect("read");
        let magic = u16::from_le_bytes([bytes[0x438], bytes[0x439]]);
        assert_eq!(magic, 0xEF53, "missing ext4 super-block magic");
    }
}
