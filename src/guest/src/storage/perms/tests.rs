use super::*;
use nix::unistd::mkfifo;
use std::ffi::OsString;
use std::fs::{self, File, Permissions};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;

fn current_owner() -> (u32, u32) {
    (unsafe { libc::getuid() }, unsafe { libc::getgid() })
}

fn chown_path_for_current_process(path: &Path) -> ChownReport {
    let (uid, gid) = current_owner();
    RecursiveChowner::new(uid, gid).chown_path(path)
}

fn chown_path_with_test_faults(path: &Path, test_faults: TestFaults) -> ChownReport {
    let (uid, gid) = current_owner();
    RecursiveChowner::new(uid, gid)
        .with_test_faults(test_faults)
        .chown_path(path)
}

fn descent_failure_report(stage: TestDescentFailure) -> ChownReport {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let child = temp.path().join("child");
    fs::create_dir(&child).expect("create child directory");
    let test_faults = TestFaults {
        descent_failure: Some((child, stage)),
        read_failure: None,
    };
    chown_path_with_test_faults(temp.path(), test_faults)
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, Permissions::from_mode(mode)).expect("set fixture mode");
}

// ── RecursiveChowner tests ────────────────────────────────────────────────────
//
// These tests use `chown_path_for_current_process`, which targets the current
// process uid/gid. Because the pre-check skips inodes already correctly owned,
// `changed` is 0 for freshly created files (already owned by the current user).
// The tests below verify walk correctness (visited count, failure handling,
// cycle detection, symlink behaviour) — not that a redundant chown was issued.

#[test]
fn walks_non_utf8_and_deep_directory_names_without_recursion() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let non_utf8 = OsString::from_vec(vec![b'n', b'a', b'm', b'e', 0xff]);
    File::create(temp.path().join(non_utf8)).expect("create non-UTF-8 file");

    let mut deepest = temp.path().to_path_buf();
    for _ in 0..256 {
        deepest.push("d");
        fs::create_dir(&deepest).expect("create deep directory");
    }
    File::create(deepest.join("leaf")).expect("create deep leaf");

    let report = chown_path_for_current_process(temp.path());

    assert_eq!(report.failures, 0);
    assert_eq!(report.cycles, 0);
    // 1 root + 1 non-utf8 file + 256 nested dirs + 1 leaf = 259
    assert_eq!(report.visited, 259);
    // Pre-check: all inodes already owned by current user, so no chown needed.
    assert_eq!(report.changed, 0);
}

#[test]
fn directory_open_failure_does_not_chown_inode() {
    let report = descent_failure_report(TestDescentFailure::Open);

    assert_eq!(report.visited, 2);
    assert_eq!(report.failures, 1);
    // Pre-check: both inodes are already correctly owned, so no chown is issued
    // even for the root that was fully traversed.
    assert_eq!(report.changed, 0);
}

#[test]
fn directory_stat_failure_does_not_chown_inode() {
    let report = descent_failure_report(TestDescentFailure::Stat);

    assert_eq!(report.visited, 2);
    assert_eq!(report.failures, 1);
    assert_eq!(report.changed, 0);
}

#[test]
fn directory_stream_failure_does_not_chown_inode() {
    let report = descent_failure_report(TestDescentFailure::Stream);

    assert_eq!(report.visited, 2);
    assert_eq!(report.failures, 1);
    assert_eq!(report.changed, 0);
}

#[test]
fn directory_read_error_is_terminal_and_does_not_chown_inode() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    File::create(temp.path().join("unreachable")).expect("create sibling");
    let report = chown_path_with_test_faults(
        temp.path(),
        TestFaults {
            descent_failure: None,
            read_failure: Some(temp.path().to_path_buf()),
        },
    );

    assert_eq!(report.failures, 1);
    assert_eq!(report.visited, 1);
    assert_eq!(report.changed, 0);
}

#[test]
fn does_not_follow_external_or_dangling_symlinks() {
    let root = tempfile::tempdir().expect("create root directory");
    let outside = tempfile::tempdir().expect("create outside directory");
    let outside_file = outside.path().join("target");
    File::create(&outside_file).expect("create outside file");
    set_mode(&outside_file, 0o4755);

    symlink(&outside_file, root.path().join("external")).expect("create outside symlink");
    symlink("missing", root.path().join("dangling")).expect("create dangling symlink");

    let report = chown_path_for_current_process(root.path());

    assert_eq!(report.failures, 0);
    // root dir + external symlink + dangling symlink
    assert_eq!(report.visited, 3);
    // The setuid bit on the outside file must be preserved.
    assert_ne!(
        fs::metadata(&outside_file)
            .expect("stat outside file")
            .mode()
            & 0o4000,
        0,
        "following the symlink would clear the target's setuid bit"
    );
}

#[test]
fn handles_hardlinks_fifo_and_unix_socket() {
    let target_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let temp = tempfile::Builder::new()
        .prefix("boxlite-perms-")
        .tempdir_in(target_dir)
        .expect("create temporary directory");
    let original = temp.path().join("original");
    let hardlink = temp.path().join("hardlink");
    File::create(&original).expect("create hardlink source");
    fs::hard_link(&original, &hardlink).expect("create hardlink");
    mkfifo(
        &temp.path().join("fifo"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create FIFO");
    let socket = match UnixListener::bind(temp.path().join("socket")) {
        Ok(socket) => Some(socket),
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
            eprintln!("skipping Unix socket fixture: sandbox denies bind(2)");
            None
        }
        Err(error) => panic!("create Unix socket: {error}"),
    };

    let report = chown_path_for_current_process(temp.path());

    assert_eq!(report.failures, 0);
    assert_eq!(report.visited, if socket.is_some() { 5 } else { 4 });
    assert_eq!(fs::metadata(original).expect("stat hardlink").nlink(), 2);
    drop(socket);
}

#[test]
fn continues_after_entry_failures_and_bounds_warning_samples() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping permission-denied fixture as root");
        return;
    }

    let temp = tempfile::tempdir().expect("create temporary directory");
    let sibling = temp.path().join("sibling");
    File::create(&sibling).expect("create successful sibling");
    let blocked: Vec<_> = (0..10)
        .map(|index| {
            let path = temp.path().join(format!("blocked-{index}"));
            fs::create_dir(&path).expect("create blocked directory");
            set_mode(&path, 0);
            path
        })
        .collect();

    let report = chown_path_for_current_process(temp.path());

    for path in &blocked {
        set_mode(path, 0o700);
    }
    assert_eq!(report.failures, blocked.len());
    assert_eq!(report.warning_samples.len(), WARNING_SAMPLE_LIMIT);
    // Pre-check: root and sibling are already correctly owned — no chown issued.
    assert_eq!(report.changed, 0);
    assert!(sibling.exists());
}

#[test]
fn supports_file_fifo_and_socket_operands() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let file = temp.path().join("file");
    File::create(&file).expect("create file operand");
    let fifo = temp.path().join("fifo");
    mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create FIFO operand");
    let socket_path = temp.path().join("socket");
    let socket = match UnixListener::bind(&socket_path) {
        Ok(socket) => Some(socket),
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => None,
        Err(error) => panic!("create Unix socket: {error}"),
    };

    for path in [file, fifo] {
        let report = chown_path_for_current_process(&path);
        assert_eq!(report.visited, 1);
        // Pre-check: already owned by current user — no chown issued.
        assert_eq!(report.changed, 0);
        assert_eq!(report.failures, 0);
    }
    if socket.is_some() {
        let report = chown_path_for_current_process(&socket_path);
        assert_eq!(report.visited, 1);
        assert_eq!(report.changed, 0);
        assert_eq!(report.failures, 0);
    }
}
