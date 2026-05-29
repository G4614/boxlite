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
use std::time::{Duration, SystemTime};

/// Don't reclaim a disk-image modified within this window. A concurrent box
/// start builds its disk-image and creates the backing overlay back-to-back
/// (sub-second); skipping recently-touched files avoids deleting one mid-start
/// without serializing the start hot path behind a global lock. A just-built
/// disk-image becomes collectable once it ages past this and no box backs it.
const DISK_IMAGE_GRACE: Duration = Duration::from_secs(600);

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
            // Only ever reclaim `*.ext4` disk-images. The directory should hold
            // nothing else, but a stray partial/temp/foreign file must never be
            // deleted just because it aged out of the grace window.
            if path.extension().and_then(|e| e.to_str()) != Some("ext4") {
                continue;
            }
            // Compare by canonical path so a symlinked/relative backing pointer
            // still matches the file we're considering.
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if pinned.contains(&canonical) {
                continue; // a box overlay backs onto this — keep it
            }

            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Skip recently-modified files: a concurrent start may have just
            // built this disk-image and not yet created its backing overlay.
            if let Ok(modified) = meta.modified()
                && SystemTime::now()
                    .duration_since(modified)
                    .map(|age| age < DISK_IMAGE_GRACE)
                    .unwrap_or(true)
            {
                continue;
            }

            let bytes = meta.len();
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

        // Three disk-images: one backed by a box, one old orphan (collectable),
        // one fresh orphan (protected by the grace period).
        const SZ: u64 = 1024 * 1024;
        let backed = disk_images_dir.join("sha256-backed.ext4");
        let orphan = disk_images_dir.join("sha256-orphan.ext4");
        let fresh = disk_images_dir.join("sha256-fresh.ext4");
        std::fs::write(&backed, vec![0u8; SZ as usize]).unwrap();
        std::fs::write(&orphan, vec![0u8; SZ as usize]).unwrap();
        std::fs::write(&fresh, vec![0u8; SZ as usize]).unwrap();

        // Age the orphan well past the grace window so it's collectable.
        let old = filetime::FileTime::from_unix_time(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - DISK_IMAGE_GRACE.as_secs()
                - 60) as i64,
            0,
        );
        filetime::set_file_mtime(&orphan, old).unwrap();

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

        // Dry run: only the aged orphan qualifies (backed is pinned, fresh is
        // within the grace window). Nothing deleted.
        let preview = runtime
            .collect_garbage(&GcOptions { dry_run: true })
            .unwrap();
        assert_eq!(
            preview.disk_images_removed, 1,
            "only the aged orphan qualifies"
        );
        assert!(
            orphan.exists() && backed.exists() && fresh.exists(),
            "dry run must not delete"
        );

        // Real run: aged orphan gone; backed (pinned) and fresh (grace) kept.
        let done = runtime
            .collect_garbage(&GcOptions { dry_run: false })
            .unwrap();
        assert_eq!(done.disk_images_removed, 1);
        assert!(!orphan.exists(), "aged orphan should be reclaimed");
        assert!(backed.exists(), "box-backed disk-image must be kept");
        assert!(fresh.exists(), "recently-built disk-image kept by grace");
    }

    /// Write a disk-image of `bytes` zeros and, if `age_secs > 0`, backdate its
    /// mtime that far into the past (to move it past the grace window).
    fn make_disk_image(
        dir: &std::path::Path,
        name: &str,
        bytes: usize,
        age_secs: u64,
    ) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![0u8; bytes]).unwrap();
        if age_secs > 0 {
            let now = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let t = filetime::FileTime::from_unix_time((now - age_secs) as i64, 0);
            filetime::set_file_mtime(&p, t).unwrap();
        }
        p
    }

    /// Create a box overlay whose qcow2 backing chain pins `backing`. Returns the
    /// `Disk` — the caller must keep it alive (Drop deletes the qcow2 unless
    /// persistent, which would un-pin the backing image mid-test).
    fn pin_with_overlay(
        runtime: &RuntimeImpl,
        box_name: &str,
        backing: &std::path::Path,
        size: u64,
    ) -> crate::disk::Disk {
        let disks = runtime.layout.boxes_dir().join(box_name).join("disks");
        std::fs::create_dir_all(&disks).unwrap();
        crate::disk::Qcow2Helper::create_cow_child_disk(
            backing,
            crate::disk::BackingFormat::Raw,
            &disks.join("disk.qcow2"),
            size,
        )
        .expect("create cow child")
    }

    /// GC reclaims only `*.ext4` disk-images. A stray non-`.ext4` file (a
    /// partial build, a temp, a foreign artifact) that has aged past the grace
    /// window must be left untouched and never counted — otherwise GC could
    /// delete unrelated files that happen to live in the disk-images directory.
    #[test]
    fn gc_ignores_non_ext4_orphans() {
        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");
        let dir = runtime.layout.image_layout().disk_images_dir();
        std::fs::create_dir_all(&dir).unwrap();

        const SZ: usize = 4096;
        let aged = DISK_IMAGE_GRACE.as_secs() + 120;
        // Both aged well past grace and both unpinned — only the extension differs.
        let ext4 = make_disk_image(&dir, "sha256-orphan.ext4", SZ, aged);
        let foreign = make_disk_image(&dir, "partial-build.tmp", SZ, aged);

        let report = runtime.collect_garbage(&GcOptions::default()).unwrap();

        assert_eq!(
            report.disk_images_removed, 1,
            "only the .ext4 orphan may be reclaimed"
        );
        assert_eq!(
            report.disk_images_bytes, SZ as u64,
            "byte accounting must exclude the non-.ext4 file"
        );
        assert!(!ext4.exists(), "aged .ext4 orphan should be reclaimed");
        assert!(
            foreign.exists(),
            "aged non-.ext4 file must be left untouched by GC"
        );
    }

    /// Stress at scale: a directory full of aged orphans, fresh orphans, and
    /// box-backed images. GC must reclaim exactly the aged orphans (byte-exact),
    /// keep everything else, and be idempotent on a second pass.
    #[test]
    fn gc_at_scale_reclaims_only_aged_orphans() {
        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");
        let dir = runtime.layout.image_layout().disk_images_dir();
        std::fs::create_dir_all(&dir).unwrap();

        const AGED: usize = 150;
        const FRESH: usize = 40;
        const BACKED: usize = 20;
        const SZ: usize = 4096;
        let aged_secs = DISK_IMAGE_GRACE.as_secs() + 120;

        for i in 0..AGED {
            make_disk_image(&dir, &format!("aged-{i:04}.ext4"), SZ, aged_secs);
        }
        for i in 0..FRESH {
            make_disk_image(&dir, &format!("fresh-{i:04}.ext4"), SZ, 0);
        }
        // Backed images are aged too, so only the live overlay pin keeps them —
        // proves pinning, not the grace window, protects them.
        let mut overlays = Vec::with_capacity(BACKED);
        for i in 0..BACKED {
            let img = make_disk_image(&dir, &format!("backed-{i:04}.ext4"), SZ, aged_secs);
            overlays.push(pin_with_overlay(
                &runtime,
                &format!("backedbox-{i:04}"),
                &img,
                SZ as u64,
            ));
        }

        let report = runtime.collect_garbage(&GcOptions::default()).unwrap();
        assert_eq!(
            report.disk_images_removed, AGED as u64,
            "exactly the aged orphans are reclaimed"
        );
        assert_eq!(
            report.disk_images_bytes,
            (AGED * SZ) as u64,
            "byte accounting must sum only the reclaimed orphans"
        );

        for i in 0..AGED {
            assert!(
                !dir.join(format!("aged-{i:04}.ext4")).exists(),
                "aged orphan {i} must be gone"
            );
        }
        for i in 0..FRESH {
            assert!(
                dir.join(format!("fresh-{i:04}.ext4")).exists(),
                "fresh orphan {i} must be kept by grace"
            );
        }
        for i in 0..BACKED {
            assert!(
                dir.join(format!("backed-{i:04}.ext4")).exists(),
                "pinned image {i} must be kept"
            );
        }

        let again = runtime.collect_garbage(&GcOptions::default()).unwrap();
        assert_eq!(
            again.disk_images_removed, 0,
            "second pass must reclaim nothing (idempotent)"
        );
        drop(overlays);
    }

    /// Concurrency stress for the lock-free safety claim: hammer `collect_garbage`
    /// in a tight loop while another thread simulates a flood of box starts, each
    /// building a fresh disk-image and its backing overlay. No just-built or
    /// pinned image may ever be deleted (fresh mtime + the pin both protect it),
    /// while pre-seeded aged orphans are still reaped during the churn.
    #[test]
    fn gc_under_concurrent_starts_never_deletes_live_or_fresh_images() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = Arc::new(
            RuntimeImpl::new(BoxliteOptions {
                home_dir: home.path.clone(),
                image_registries: vec![],
            })
            .expect("create runtime"),
        );
        let dir = runtime.layout.image_layout().disk_images_dir();
        std::fs::create_dir_all(&dir).unwrap();

        const AGED: usize = 60;
        const STARTS: usize = 50;
        const SZ: usize = 4096;
        let aged_secs = DISK_IMAGE_GRACE.as_secs() + 120;
        for i in 0..AGED {
            make_disk_image(&dir, &format!("aged-{i:04}.ext4"), SZ, aged_secs);
        }

        let done = Arc::new(AtomicBool::new(false));
        let starter = {
            let runtime = runtime.clone();
            let dir = dir.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                // Keep overlays alive for the whole run so their pins hold.
                let mut overlays = Vec::with_capacity(STARTS);
                for i in 0..STARTS {
                    let img = make_disk_image(&dir, &format!("live-{i:04}.ext4"), SZ, 0);
                    overlays.push(pin_with_overlay(
                        &runtime,
                        &format!("livebox-{i:04}"),
                        &img,
                        SZ as u64,
                    ));
                }
                done.store(true, Ordering::Release);
                overlays
            })
        };

        // Race GC against the start flood. The pass cap is a safety bound: if a
        // regression breaks grace/pin protection the starter panics (its backing
        // image gets deleted mid-create) and never sets `done`, so without the
        // cap this loop would spin until the harness timeout instead of failing
        // fast on the joined panic below.
        let mut passes = 0u32;
        while !done.load(Ordering::Acquire) && passes < 1_000_000 {
            runtime.collect_garbage(&GcOptions::default()).unwrap();
            passes += 1;
        }
        let overlays = starter
            .join()
            .expect("starter thread (backing image deleted under GC?)");
        // A few more passes now that every start has landed.
        for _ in 0..3 {
            runtime.collect_garbage(&GcOptions::default()).unwrap();
        }

        for i in 0..STARTS {
            assert!(
                dir.join(format!("live-{i:04}.ext4")).exists(),
                "live start image {i} was deleted under concurrent GC (passes={passes})"
            );
        }
        for i in 0..AGED {
            assert!(
                !dir.join(format!("aged-{i:04}.ext4")).exists(),
                "aged orphan {i} should be reaped despite the start churn"
            );
        }
        drop(overlays);
    }
}
