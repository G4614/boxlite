//! Filesystem ownership verification and repair after disk mount.
//!
//! After mounting a container rootfs disk, ownership should match the image's
//! exec user. If the ext4 build regresses and produces root-owned inodes,
//! `verify_and_repair_ownership` detects the mismatch, repairs it, and emits
//! a WARN so the regression is visible in logs.

use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use boxlite_shared::errors::BoxliteResult;
use nix::dir::{Dir, OwningIter};
use nix::fcntl::{openat, AtFlags, OFlag};
use nix::libc;
use nix::sys::stat::{fstat, fstatat, Mode};
use nix::unistd::{close, fchown, fchownat, Gid, Uid};

const WARNING_SAMPLE_LIMIT: usize = 8;

/// Verify that `path` (a mounted rootfs) has the expected exec-user ownership.
///
/// A mismatch means the ext4 build regressed — ext4 should preserve ownership.
/// When a mismatch is detected the entire tree is repaired and a WARN is logged.
/// Root-exec containers (uid=0, gid=0) are skipped: ownership is already root.
pub(crate) fn verify_and_repair_ownership(path: &Path, uid: u32, gid: u32) -> BoxliteResult<()> {
    if uid == 0 && gid == 0 {
        // Root-exec: ownership is already root everywhere. Nothing to verify.
        return Ok(());
    }

    if ownership_matches(path, uid, gid) {
        tracing::debug!(
            "Rootfs ownership at {} matches exec user {}:{} — no repair needed",
            path.display(),
            uid,
            gid
        );
        return Ok(());
    }

    // Ownership is wrong: the ext4 build regressed. Repair and warn.
    tracing::warn!(
        "Rootfs ownership at {} does not match exec user {}:{}; repairing. \
         This indicates an ext4 build regression — ownership should be \
         set at image-build time.",
        path.display(),
        uid,
        gid
    );

    let start = std::time::Instant::now();
    let report = RecursiveChowner::new(uid, gid).chown_path(path);
    let duration = start.elapsed();

    if report.has_warnings() {
        tracing::warn!(
            "Ownership repair at {} to {}:{} completed with warnings in {:?}: \
             visited={}, changed={}, failures={}, cycles={}, samples={:?}",
            path.display(),
            uid,
            gid,
            duration,
            report.visited,
            report.changed,
            report.failures,
            report.cycles,
            report.warning_samples
        );
    } else {
        tracing::info!(
            "Repaired rootfs ownership at {} to {}:{} in {:?} (visited={}, changed={})",
            path.display(),
            uid,
            gid,
            duration,
            report.visited,
            report.changed
        );
    }

    Ok(())
}

/// Sample root dir + first few entries to cheaply detect ownership mismatches.
fn ownership_matches(path: &Path, expected_uid: u32, expected_gid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.uid() != expected_uid || meta.gid() != expected_gid {
        return false;
    }

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.take(5).flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.uid() != expected_uid || meta.gid() != expected_gid {
                    return false;
                }
            }
        }
    }

    true
}

struct RecursiveChowner {
    uid: Uid,
    gid: Gid,
    report: ChownReport,
}

impl RecursiveChowner {
    fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid: Uid::from_raw(uid),
            gid: Gid::from_raw(gid),
            report: ChownReport::default(),
        }
    }

    fn chown_path(mut self, path: &Path) -> ChownReport {
        let stat = match fstatat(None, path, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) => {
                self.report.record_failure("stat", path, error);
                return self.report;
            }
        };
        self.report.visited = 1;

        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            self.chown_operand(path);
            return self.report;
        }

        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let root = match Dir::open(path, flags, Mode::empty()) {
            Ok(root) => root,
            Err(error) => {
                self.report.record_failure("open directory", path, error);
                return self.report;
            }
        };
        let root_stat = match fstat(root.as_raw_fd()) {
            Ok(stat) => stat,
            Err(error) => {
                self.report.record_failure("stat directory", path, error);
                return self.report;
            }
        };

        let mut stack = vec![DirectoryFrame::new(root, path.to_path_buf(), &root_stat)];
        while !stack.is_empty() {
            let next_entry = stack
                .last_mut()
                .expect("non-empty directory stack")
                .entries
                .next();

            let entry = match next_entry {
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    let frame = stack.pop().expect("non-empty directory stack");
                    self.report
                        .record_failure("read directory", &frame.path, error);
                    continue;
                }
                None => {
                    let frame = stack.pop().expect("non-empty directory stack");
                    self.chown_directory(frame);
                    continue;
                }
            };

            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }

            let parent = stack.last().expect("entry has a parent frame");
            let parent_fd = parent.entries.as_raw_fd();
            let path = parent
                .path
                .join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            self.report.visited += 1;

            let stat = match fstatat(Some(parent_fd), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(error) => {
                    self.report.record_failure("stat", &path, error);
                    continue;
                }
            };

            if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
                self.chown_entry(parent_fd, name, &path);
                continue;
            }

            let child_fd = match openat(
                Some(parent_fd),
                name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(error) => {
                    self.report.record_failure("open directory", &path, error);
                    continue;
                }
            };

            let child_stat = match fstat(child_fd) {
                Ok(stat) => stat,
                Err(error) => {
                    self.report.record_failure("stat directory", &path, error);
                    let _ = close(child_fd);
                    continue;
                }
            };

            let child_identity = DirectoryIdentity::from(&child_stat);
            if stack
                .iter()
                .any(|ancestor| ancestor.identity == child_identity)
            {
                let _ = close(child_fd);
                self.report.record_cycle(&path);
                continue;
            }

            let child_dir = match Dir::from_fd(child_fd) {
                Ok(dir) => dir,
                Err(error) => {
                    self.report
                        .record_failure("open directory stream for", &path, error);
                    continue;
                }
            };
            stack.push(DirectoryFrame::new(child_dir, path, &child_stat));
        }

        self.report
    }

    fn chown_operand(&mut self, path: &Path) {
        // Pre-check: skip if ownership already matches.
        // fchownat clears set-ID bits and file capabilities on owner change,
        // so skipping unnecessary calls preserves privileged-executable metadata.
        if let Ok(st) = fstatat(None, path, AtFlags::AT_SYMLINK_NOFOLLOW) {
            if st.st_uid == self.uid.as_raw() && st.st_gid == self.gid.as_raw() {
                return;
            }
        }
        match fchownat(
            None,
            path,
            Some(self.uid),
            Some(self.gid),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(()) => self.report.changed += 1,
            Err(error) => self.report.record_failure("chown", path, error),
        }
    }

    fn chown_entry(&mut self, parent_fd: i32, name: &std::ffi::CStr, path: &Path) {
        // Pre-check: skip if ownership already matches (preserves set-ID bits).
        if let Ok(st) = fstatat(Some(parent_fd), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            if st.st_uid == self.uid.as_raw() && st.st_gid == self.gid.as_raw() {
                return;
            }
        }
        match fchownat(
            Some(parent_fd),
            name,
            Some(self.uid),
            Some(self.gid),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(()) => self.report.changed += 1,
            Err(error) => self.report.record_failure("chown", path, error),
        }
    }

    fn chown_directory(&mut self, frame: DirectoryFrame) {
        // Pre-check: skip if ownership already matches (preserves set-ID bits).
        if let Ok(st) = fstat(frame.entries.as_raw_fd()) {
            if st.st_uid == self.uid.as_raw() && st.st_gid == self.gid.as_raw() {
                return;
            }
        }
        match fchown(frame.entries.as_raw_fd(), Some(self.uid), Some(self.gid)) {
            Ok(()) => self.report.changed += 1,
            Err(error) => self
                .report
                .record_failure("chown directory", &frame.path, error),
        }
    }
}

struct DirectoryFrame {
    entries: OwningIter,
    path: PathBuf,
    identity: DirectoryIdentity,
}

impl DirectoryFrame {
    fn new(directory: Dir, path: PathBuf, stat: &libc::stat) -> Self {
        Self {
            entries: directory.into_iter(),
            path,
            identity: DirectoryIdentity::from(stat),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct DirectoryIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

impl From<&libc::stat> for DirectoryIdentity {
    fn from(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }
}

#[derive(Debug, Default)]
struct ChownReport {
    visited: usize,
    changed: usize,
    failures: usize,
    cycles: usize,
    warning_samples: Vec<String>,
}

impl ChownReport {
    fn has_warnings(&self) -> bool {
        self.failures != 0 || self.cycles != 0
    }

    fn record_failure(&mut self, operation: &str, path: &Path, error: nix::Error) {
        self.failures += 1;
        self.record_sample(format!(
            "{} {}: {}",
            operation,
            path.to_string_lossy(),
            error
        ));
    }

    fn record_cycle(&mut self, path: &Path) {
        self.cycles += 1;
        self.record_sample(format!(
            "skipped directory cycle at {}",
            path.to_string_lossy()
        ));
    }

    fn record_sample(&mut self, sample: String) {
        if self.warning_samples.len() < WARNING_SAMPLE_LIMIT {
            self.warning_samples.push(sample);
        }
    }
}
