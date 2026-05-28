//! Garbage collection for the on-disk image caches under `~/.boxlite/`.
//!
//! Scope (deliberately narrow — see the notes below for why):
//!
//! - **`images/disk-images/*.ext4`** — the merged per-image rootfs that box COW
//!   overlays back onto (via an absolute path baked into the qcow2 header). A
//!   disk-image is removable only when *no* box overlay backs onto it; deleting
//!   a backed one would leave a box unable to start. This sweep reclaims the
//!   orphaned ones — built rootfs left behind after every backing box was
//!   removed. It's usually the largest reclaimable chunk.
//!
//! Out of scope here (handled elsewhere or deferred):
//! - Orphaned `boxes/<id>/` dirs and unreferenced base disks are already
//!   collected by [`RuntimeImpl::cleanup_orphaned_directories`] on startup and
//!   the `remove_box` → `try_gc_base` cascade respectively.
//! - LRU eviction of the re-pullable build cache (`images/layers`,
//!   `images/extracted`) needs blob-dedup accounting inside the image store and
//!   is a separate change.
//!
//! Safety: a disk-image is deleted only when it is provably absent from every
//! box overlay's backing chain — never on an age/size guess.

use crate::runtime::rt_impl::RuntimeImpl;
use boxlite_shared::errors::BoxliteResult;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What to sweep and how.
#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Report what would be reclaimed without deleting anything.
    pub dry_run: bool,
}

/// What a GC pass freed (or would free, under `dry_run`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub dry_run: bool,
    /// Orphaned image disk-images removed (no box overlay backs onto them).
    pub disk_images_removed: u64,
    pub disk_images_bytes: u64,
}

impl GcReport {
    pub fn total_bytes(&self) -> u64 {
        self.disk_images_bytes
    }

    pub fn total_removed(&self) -> u64 {
        self.disk_images_removed
    }
}

impl RuntimeImpl {
    /// Reclaim disk by removing orphaned image disk-images.
    pub fn collect_garbage(&self, opts: &GcOptions) -> BoxliteResult<GcReport> {
        let mut report = GcReport {
            dry_run: opts.dry_run,
            ..Default::default()
        };

        self.sweep_orphan_disk_images(opts.dry_run, &mut report);

        tracing::info!(
            dry_run = opts.dry_run,
            disk_images = report.disk_images_removed,
            bytes = report.total_bytes(),
            "Image cache GC pass complete"
        );
        Ok(report)
    }

    /// Remove `images/disk-images/*.ext4` files that no box overlay backs onto.
    ///
    /// The pinned set is built by walking every box overlay's qcow2 backing
    /// chain; only disk-images absent from it are removed.
    fn sweep_orphan_disk_images(&self, dry_run: bool, report: &mut GcReport) {
        let disk_images_dir = self.layout.image_layout().disk_images_dir();
        let pinned = self.disk_images_backed_by_boxes(&disk_images_dir);

        let entries = match std::fs::read_dir(&disk_images_dir) {
            Ok(e) => e,
            Err(_) => return, // dir absent → nothing to sweep
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Compare by canonical path so a symlinked/relative backing pointer
            // still matches the file we're considering.
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if pinned.contains(&canonical) {
                continue; // a box overlay backs onto this — keep it
            }

            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if !dry_run && std::fs::remove_file(&path).is_err() {
                continue;
            }
            tracing::info!(
                disk_image = %path.display(),
                bytes,
                dry_run,
                "Orphan image disk reclaimed"
            );
            report.disk_images_removed += 1;
            report.disk_images_bytes += bytes;
        }
    }

    /// Canonical paths of disk-images that some box overlay backs onto.
    ///
    /// Walks the backing chain of every `*.qcow2` under `boxes/*/disks/` and
    /// keeps the entries that live in `disk_images_dir`.
    fn disk_images_backed_by_boxes(&self, disk_images_dir: &Path) -> HashSet<PathBuf> {
        let disk_images_dir = disk_images_dir
            .canonicalize()
            .unwrap_or_else(|_| disk_images_dir.to_path_buf());
        let mut pinned = HashSet::new();

        let boxes_dir = self.layout.boxes_dir();
        let Ok(boxes) = std::fs::read_dir(&boxes_dir) else {
            return pinned;
        };
        for box_entry in boxes.flatten() {
            let disks_dir = box_entry.path().join("disks");
            let Ok(disks) = std::fs::read_dir(&disks_dir) else {
                continue;
            };
            for disk in disks.flatten() {
                let disk_path = disk.path();
                if disk_path.extension().and_then(|e| e.to_str()) != Some("qcow2") {
                    continue;
                }
                for backing in crate::disk::read_backing_chain(&disk_path) {
                    let canonical = backing.canonicalize().unwrap_or(backing);
                    if canonical.starts_with(&disk_images_dir) {
                        pinned.insert(canonical);
                    }
                }
            }
        }
        pinned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::options::BoxliteOptions;
    use crate::runtime::rt_impl::RuntimeImpl;
    use boxlite_test_utils::home::PerTestBoxHome;

    /// A disk-image with no box backing it is reclaimed; one that a box overlay
    /// backs onto is kept. Dry-run reports but deletes nothing.
    #[test]
    fn sweeps_orphan_disk_images_only() {
        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");

        let disk_images_dir = runtime.layout.image_layout().disk_images_dir();
        std::fs::create_dir_all(&disk_images_dir).unwrap();

        // Two disk-images: one will be backed by a box, one orphaned.
        const SZ: u64 = 1024 * 1024;
        let backed = disk_images_dir.join("sha256-backed.ext4");
        let orphan = disk_images_dir.join("sha256-orphan.ext4");
        std::fs::write(&backed, vec![0u8; SZ as usize]).unwrap();
        std::fs::write(&orphan, vec![0u8; SZ as usize]).unwrap();

        // A box overlay whose qcow2 backing chain points at `backed`.
        let disks = runtime
            .layout
            .boxes_dir()
            .join("01BOXBACKINGTESTAAAAAAAAAA")
            .join("disks");
        std::fs::create_dir_all(&disks).unwrap();
        // Bind the returned Disk: it deletes its file on drop unless persistent,
        // and we need the overlay to stay on disk for the backing-chain scan.
        let _overlay = crate::disk::Qcow2Helper::create_cow_child_disk(
            &backed,
            crate::disk::BackingFormat::Raw,
            &disks.join("disk.qcow2"),
            SZ,
        )
        .expect("create cow child");

        // Dry run: orphan reported, nothing deleted.
        let preview = runtime
            .collect_garbage(&GcOptions { dry_run: true })
            .unwrap();
        assert_eq!(
            preview.disk_images_removed, 1,
            "only the orphan is a candidate"
        );
        assert!(preview.disk_images_bytes >= 8192);
        assert!(
            orphan.exists() && backed.exists(),
            "dry run must not delete"
        );

        // Real run: orphan gone, backed disk-image kept.
        let done = runtime
            .collect_garbage(&GcOptions { dry_run: false })
            .unwrap();
        assert_eq!(done.disk_images_removed, 1);
        assert!(!orphan.exists(), "orphan disk-image should be reclaimed");
        assert!(backed.exists(), "box-backed disk-image must be kept");
    }
}
