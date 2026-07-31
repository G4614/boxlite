//! A shared layer store: many archives over one content-addressed pool.
//!
//! The layout is the one an incremental mirror wants:
//!
//! ```text
//! <store>/archives/<name>.json   one manifest per archive — the references
//! <store>/layers/<hex>.zst       shared pool, named by content
//! ```
//!
//! Archives of different boxes share every layer they have in common, so the
//! pool grows by what is new, not by what is exported. What holds a layer in
//! the pool is the manifests that name it; [`ArchiveStore::gc`] sweeps the
//! rest. Deleting an archive is deleting its manifest — the bytes it pinned
//! stay until a sweep finds them unreferenced.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::archive::{
    ArchiveManifest, LAYERS_DIR, MANIFEST_FILENAME, STORE_ARCHIVES_DIR, layer_entry_name,
    validate_store_archive_name,
};

/// What a sweep did, and what it left alone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// Unreferenced objects removed.
    pub swept: usize,
    /// Bytes those objects held.
    pub bytes_freed: u64,
    /// Unreferenced objects left in place because they are younger than the
    /// grace period — possibly a publish in flight whose manifest has not
    /// landed yet.
    pub kept_in_grace: usize,
}

/// A handle on a shared layer store.
///
/// Purely a view over a directory: opening one creates nothing and holds no
/// lock. Publishing into a store is an export with
/// `ExportOptions { as_directory: true, archive_name: Some(..) }` whose
/// destination is the store root.
pub struct ArchiveStore {
    root: PathBuf,
}

impl ArchiveStore {
    /// Open a store rooted at `root`.
    ///
    /// Refuses a single-archive directory (a root `manifest.json`): its
    /// layers all belong to that one archive, and sweeping it with an empty
    /// `archives/` beside it would destroy them.
    pub fn open(root: impl Into<PathBuf>) -> BoxliteResult<Self> {
        let root = root.into();
        if root.join(MANIFEST_FILENAME).exists() {
            return Err(BoxliteError::InvalidArgument(format!(
                "{} is a single-archive directory, not a store: it has a root {}",
                root.display(),
                MANIFEST_FILENAME
            )));
        }
        Ok(Self { root })
    }

    /// Names of the archives published here, in no particular order.
    pub fn archives(&self) -> BoxliteResult<Vec<String>> {
        let dir = self.root.join(STORE_ARCHIVES_DIR);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !stem.starts_with('.')
            {
                names.push(stem.to_string());
            }
        }
        Ok(names)
    }

    /// Path of a published archive's manifest — the value to hand to import.
    pub fn archive_path(&self, name: &str) -> BoxliteResult<PathBuf> {
        validate_store_archive_name(name)?;
        Ok(self
            .root
            .join(STORE_ARCHIVES_DIR)
            .join(format!("{name}.json")))
    }

    /// Drop an archive's manifest, releasing its hold on the pool.
    ///
    /// Returns whether it existed. The layers it pinned stay on disk until
    /// [`gc`](Self::gc) finds them unreferenced — removal is cheap and safe,
    /// reclamation is the sweep's job.
    pub fn remove(&self, name: &str) -> BoxliteResult<bool> {
        let path = self.archive_path(name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(BoxliteError::Storage(format!(
                "Failed to remove {}: {}",
                path.display(),
                e
            ))),
        }
    }

    /// Sweep pool objects no manifest references.
    ///
    /// Fails closed: a manifest that cannot be parsed aborts the sweep, since
    /// its references are unknown and anything deleted might be them. Objects
    /// younger than `grace` are kept even when unreferenced — a publish writes
    /// its objects before its manifest, so a sweep that runs inside that
    /// window sees layers nothing names yet. The grace period must exceed the
    /// longest publish that can run concurrently; the publisher additionally
    /// re-checks its objects after the manifest lands and rewrites anything a
    /// sweep took.
    ///
    /// Only content-named `.zst` objects and stale `.partial` staging files
    /// are candidates; anything else in the pool directory is left untouched.
    pub fn gc(&self, grace: Duration) -> BoxliteResult<GcReport> {
        let archives_dir = self.root.join(STORE_ARCHIVES_DIR);
        if !archives_dir.is_dir() {
            return Err(BoxliteError::InvalidArgument(format!(
                "{} is not an archive store: it has no {}/ directory",
                self.root.display(),
                STORE_ARCHIVES_DIR
            )));
        }

        let mut referenced: HashSet<PathBuf> = HashSet::new();
        for entry in read_dir(&archives_dir)? {
            let path = entry?.path();
            if !path.extension().is_some_and(|e| e == "json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| {
                BoxliteError::Storage(format!("Failed to read {}: {}", path.display(), e))
            })?;
            let manifest: ArchiveManifest = serde_json::from_str(&text).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Refusing to sweep: {} is not a readable manifest ({}); its references are unknown",
                    path.display(),
                    e
                ))
            })?;
            for layer in &manifest.layers {
                referenced.insert(
                    self.root
                        .join(format!("{}.zst", layer_entry_name(&layer.digest))),
                );
            }
        }

        let layers_dir = self.root.join(LAYERS_DIR);
        let mut report = GcReport::default();
        if !layers_dir.is_dir() {
            return Ok(report);
        }
        let now = std::time::SystemTime::now();
        for entry in read_dir(&layers_dir)? {
            let path = entry?.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let sweepable = is_object_name(name) || name.ends_with(".partial");
            if !sweepable || referenced.contains(&path) {
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                // Raced with another sweeper or a publisher's rename.
                Err(_) => continue,
            };
            let age = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .unwrap_or(Duration::ZERO);
            if age < grace {
                report.kept_in_grace += 1;
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    report.swept += 1;
                    report.bytes_freed += meta.len();
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(BoxliteError::Storage(format!(
                        "Failed to sweep {}: {}",
                        path.display(),
                        e
                    )));
                }
            }
        }
        Ok(report)
    }
}

/// Whether a pool filename is a content-named object (`<hex>.zst`).
fn is_object_name(name: &str) -> bool {
    name.strip_suffix(".zst")
        .is_some_and(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

fn read_dir(dir: &Path) -> BoxliteResult<std::fs::ReadDir> {
    std::fs::read_dir(dir)
        .map_err(|e| BoxliteError::Storage(format!("Failed to read {}: {}", dir.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with hand-written manifests and objects, no boxes involved.
    fn store(root: &Path) -> ArchiveStore {
        std::fs::create_dir_all(root.join(STORE_ARCHIVES_DIR)).unwrap();
        std::fs::create_dir_all(root.join(LAYERS_DIR)).unwrap();
        ArchiveStore::open(root).unwrap()
    }

    fn publish(root: &Path, name: &str, digests: &[&str]) {
        for d in digests {
            std::fs::write(root.join(LAYERS_DIR).join(format!("{d}.zst")), d).unwrap();
        }
        let layers: Vec<String> = digests
            .iter()
            .map(|d| format!(r#"{{"digest":"sha256:{d}","format":"qcow2"}}"#))
            .collect();
        let manifest = format!(
            r#"{{"version":6,"box_name":null,"image":"t","guest_disk_checksum":"","container_disk_checksum":"","layers":[{}],"exported_at":"2026-07-31T00:00:00Z"}}"#,
            layers.join(",")
        );
        std::fs::write(
            root.join(STORE_ARCHIVES_DIR).join(format!("{name}.json")),
            manifest,
        )
        .unwrap();
    }

    fn pool(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root.join(LAYERS_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The pool holds the union; a layer stays while any manifest names it.
    #[test]
    fn a_layer_stays_while_any_archive_references_it() {
        let temp = tempfile::TempDir::new_in("/tmp").unwrap();
        let root = temp.path();
        let s = store(root);
        publish(root, "one", &["aa", "bb"]);
        publish(root, "two", &["aa", "cc"]);

        assert!(s.remove("two").unwrap());
        let report = s.gc(Duration::ZERO).unwrap();
        assert_eq!(report.swept, 1, "only the layer unique to `two` goes");
        assert_eq!(pool(root), vec!["aa.zst", "bb.zst"]);

        assert!(s.remove("one").unwrap());
        let report = s.gc(Duration::ZERO).unwrap();
        assert_eq!(report.swept, 2);
        assert!(pool(root).is_empty());
        assert!(!s.remove("one").unwrap(), "second removal reports absence");
    }

    /// Unreferenced but young objects survive: a publish in flight writes its
    /// objects before its manifest, and the sweeper must not eat them.
    #[test]
    fn an_unreferenced_object_inside_the_grace_period_is_kept() {
        let temp = tempfile::TempDir::new_in("/tmp").unwrap();
        let root = temp.path();
        let s = store(root);
        std::fs::write(root.join(LAYERS_DIR).join("dd.zst"), "orphan").unwrap();

        let report = s.gc(Duration::from_secs(3600)).unwrap();
        assert_eq!((report.swept, report.kept_in_grace), (0, 1));
        assert_eq!(pool(root), vec!["dd.zst"]);

        let report = s.gc(Duration::ZERO).unwrap();
        assert_eq!(report.swept, 1);
    }

    /// A manifest that cannot be parsed aborts the sweep entirely: its
    /// references are unknown, so nothing is safe to delete.
    #[test]
    fn a_corrupt_manifest_aborts_the_sweep() {
        let temp = tempfile::TempDir::new_in("/tmp").unwrap();
        let root = temp.path();
        let s = store(root);
        publish(root, "good", &["aa"]);
        std::fs::write(root.join(LAYERS_DIR).join("ee.zst"), "orphan").unwrap();
        std::fs::write(
            root.join(STORE_ARCHIVES_DIR).join("bad.json"),
            "not a manifest",
        )
        .unwrap();

        let err = s.gc(Duration::ZERO).unwrap_err();
        assert!(err.to_string().contains("Refusing to sweep"), "{err}");
        assert_eq!(pool(root), vec!["aa.zst", "ee.zst"], "nothing was deleted");
    }

    /// Files that are not content-named objects are never sweep candidates.
    #[test]
    fn foreign_files_in_the_pool_are_left_alone() {
        let temp = tempfile::TempDir::new_in("/tmp").unwrap();
        let root = temp.path();
        let s = store(root);
        std::fs::write(root.join(LAYERS_DIR).join("README.txt"), "notes").unwrap();
        std::fs::write(root.join(LAYERS_DIR).join("zz.zst.partial"), "stale").unwrap();

        let report = s.gc(Duration::ZERO).unwrap();
        assert_eq!(report.swept, 1, "only the stale staging file goes");
        assert_eq!(pool(root), vec!["README.txt"]);
    }

    /// A single-archive directory must not be opened as a store: an empty
    /// archives/ beside a root manifest would make every layer look orphaned.
    #[test]
    fn a_single_archive_directory_is_refused() {
        let temp = tempfile::TempDir::new_in("/tmp").unwrap();
        let root = temp.path();
        std::fs::write(root.join(MANIFEST_FILENAME), "{}").unwrap();
        assert!(ArchiveStore::open(root).is_err());
    }

    /// Archive names are path components, nothing more.
    #[test]
    fn a_path_escaping_archive_name_is_refused() {
        let temp = tempfile::TempDir::new_in("/tmp").unwrap();
        let s = store(temp.path());
        for bad in ["../evil", "a/b", "", ".hidden", "a\\b"] {
            assert!(s.archive_path(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
