//! Cgroup v2 setup for resource limiting.
//!
//! This module sets up cgroup v2 limits for the boxlite-shim process.
//! Cgroups are used to limit CPU, memory, and process count.
//!
//! ## Why Cgroups?
//!
//! - Prevent DoS attacks (fork bomb, memory exhaustion)
//! - Fair resource sharing between boxes
//! - Enforced by kernel, can't be bypassed from userspace
//!
//! ## Rootless Support
//!
//! This module supports both root and rootless operation:
//! - **Root**: Creates cgroups in `/sys/fs/cgroup/boxlite/`
//! - **Rootless**: Creates cgroups in the user's systemd service scope:
//!   `/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/boxlite/`
//!
//! ## Cgroup v2 Structure
//!
//! ```text
//! {cgroup_base}/              # /sys/fs/cgroup (root) or user service path (rootless)
//! └── boxlite/
//!     └── {box_id}/
//!         ├── cpu.max           # CPU limit
//!         ├── cpu.weight        # CPU shares
//!         ├── memory.max        # Memory limit
//!         ├── memory.high       # Memory throttle threshold
//!         ├── pids.max          # Max processes
//!         └── cgroup.procs      # Add process here
//! ```

use super::error::JailerError;
use crate::runtime::advanced_options::ResourceLimits;
use crate::runtime::id::BoxID;
use std::fs;
use std::path::{Path, PathBuf};

/// Base path for cgroup v2 filesystem.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// BoxLite cgroup name.
const BOXLITE_CGROUP: &str = "boxlite";

// ============================================================================
// Rootless Cgroup Support
// ============================================================================

/// Check if the current process is running as root.
#[cfg(target_os = "linux")]
fn is_root() -> bool {
    unsafe { libc::getuid() == 0 }
}

#[cfg(not(target_os = "linux"))]
fn is_root() -> bool {
    false
}

/// Get the user's systemd cgroup base path for rootless operation.
///
/// systemd chowns `user@{uid}.service/` at login regardless of delegation
/// status, so `path.exists()` is always true on a systemd host. We check
/// write permission with `access(2)` instead: the directory is only writable
/// when the user slice is actually delegated (logind / systemd-run --user).
#[cfg(target_os = "linux")]
fn get_user_cgroup_base() -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let path = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service",
        uid, uid
    ));
    let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()?;
    let writable = unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 };
    if writable { Some(path) } else { None }
}

#[cfg(not(target_os = "linux"))]
fn get_user_cgroup_base() -> Option<PathBuf> {
    None
}

/// Get the cgroup base from `/proc/self/cgroup` for rootless container environments
/// (Podman rootless, OCI containers with a delegated subtree).
#[cfg(target_os = "linux")]
fn get_container_cgroup_base() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = content
        .lines()
        .find(|l| l.starts_with("0::"))?
        .strip_prefix("0::")?
        .trim()
        .trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    let path = PathBuf::from(CGROUP_ROOT).join(rel);
    let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()?;
    let writable = unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 };
    if writable { Some(path) } else { None }
}

#[cfg(not(target_os = "linux"))]
fn get_container_cgroup_base() -> Option<PathBuf> {
    None
}

/// Get the cgroup base path for the current user.
///
/// - Root: returns `/sys/fs/cgroup`
/// - Non-root (systemd delegation): returns the user@{uid}.service subtree
/// - Non-root (container delegation): returns the delegated subtree from `/proc/self/cgroup`
/// - Non-root (neither): falls back to `/sys/fs/cgroup` (setup will warn)
fn get_cgroup_base() -> PathBuf {
    if is_root() {
        PathBuf::from(CGROUP_ROOT)
    } else {
        get_user_cgroup_base()
            .or_else(get_container_cgroup_base)
            .unwrap_or_else(|| PathBuf::from(CGROUP_ROOT))
    }
}

/// Configuration for cgroup resource limits.
#[derive(Debug, Clone, Default)]
pub struct CgroupConfig {
    /// Memory limit in bytes (memory.max).
    pub memory_max: Option<u64>,

    /// Memory high threshold in bytes (memory.high).
    /// Processes exceeding this are throttled.
    pub memory_high: Option<u64>,

    /// CPU weight (1-10000, default 100).
    /// Higher = more CPU time relative to other cgroups.
    pub cpu_weight: Option<u32>,

    /// CPU max in format "quota period" (e.g., "100000 100000" = 100%).
    /// First number is max microseconds per period.
    pub cpu_max: Option<(u64, u64)>,

    /// Maximum number of processes (pids.max).
    pub pids_max: Option<u64>,
}

/// Check if cgroup v2 is available and unified hierarchy is used.
pub fn is_cgroup_v2_available() -> bool {
    // Check if cgroup2 is mounted
    let cgroup_root = Path::new(CGROUP_ROOT);
    if !cgroup_root.exists() {
        return false;
    }

    // Check for cgroup.controllers (cgroup v2 indicator)
    let controllers = cgroup_root.join("cgroup.controllers");
    controllers.exists()
}

/// Get the path to a box's cgroup directory.
///
/// The base path depends on whether running as root or regular user:
/// - Root: `/sys/fs/cgroup/boxlite/{box_id}`
/// - User: `/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/boxlite/{box_id}`
pub fn cgroup_path(box_id: &str) -> PathBuf {
    get_cgroup_base().join(BOXLITE_CGROUP).join(box_id)
}

/// Kill every process in a box's cgroup via cgroup v2 `cgroup.kill`.
///
/// Reaps the box's *entire* process tree atomically — the outer bwrap launcher,
/// the inner pid-namespace bwrap, the shim, and the VM — regardless of
/// pid-namespace or process-group structure. A single-pid `SIGKILL` of the
/// recorded pid only hits the outer bwrap; a detached box's inner tree survives
/// it, since #851 stopped applying `--die-with-parent` to detached boxes. The
/// whole tree lives in the box's cgroup, so killing the cgroup by id reaps it
/// even after `state.pid` has been cleared.
///
/// Best-effort and idempotent: a no-op if the cgroup is gone, already empty, or
/// `cgroup.kill` is unavailable (kernel < 5.14 / cgroup v1 / no jailer). Returns
/// `true` if the kill file was written.
///
/// Takes a [`BoxID`] rather than a raw `&str` on purpose: this writes to a path
/// derived from the id, so it must be a safe single path component. `BoxID`'s
/// constructor ([`BoxID::parse`]/mint) is the one choke point that guarantees
/// that — its charset (`[A-Za-z0-9_-]`) excludes `/`, `\`, and `.`, so `..`/`.`
/// and path separators are unrepresentable. The type carries the guarantee, so
/// no per-call traversal check is needed (or could drift) here.
///
/// `pub(super)` on purpose: this is the cgroup *mechanism*, reached only through
/// the jailer's [`super::reap_box`] facade. Layers above the jailer (box,
/// runtime) reap by box semantics and never name cgroups.
pub(super) fn kill_cgroup(box_id: &BoxID) -> bool {
    let kill_file = cgroup_path(box_id.as_str()).join("cgroup.kill");
    std::fs::write(&kill_file, "1").is_ok()
}

/// Setup cgroup for a box.
///
/// Creates the cgroup directory and configures resource limits.
/// Must be called BEFORE spawning the process.
///
/// # Errors
///
/// Returns [`JailerError::Cgroup`] if:
/// - Cgroup v2 is not available on the system
/// - Failed to create the boxlite parent cgroup directory
/// - Failed to create the box-specific cgroup directory
/// - Failed to write resource limit configuration files
pub fn setup_cgroup(box_id: &str, config: &CgroupConfig) -> Result<PathBuf, JailerError> {
    if !is_cgroup_v2_available() {
        tracing::warn!("Cgroup v2 not available, skipping cgroup setup");
        return Err(JailerError::Cgroup("Cgroup v2 not available".to_string()));
    }

    let cgroup_base = get_cgroup_base();
    let boxlite_cgroup = cgroup_base.join(BOXLITE_CGROUP);
    let box_cgroup = boxlite_cgroup.join(box_id);

    tracing::debug!(
        cgroup_base = %cgroup_base.display(),
        is_root = is_root(),
        "Using cgroup base path"
    );

    // Create boxlite parent cgroup if needed, then (idempotently) delegate the
    // controllers to its children. Running enable_controllers every time — not
    // only on creation — repairs a parent left behind by an earlier build that
    // failed to delegate, so box children always end up with the controller
    // files.
    if !boxlite_cgroup.exists() {
        fs::create_dir(&boxlite_cgroup).map_err(|e| {
            JailerError::Cgroup(format!(
                "Failed to create boxlite cgroup at {}: {}",
                boxlite_cgroup.display(),
                e
            ))
        })?;
    }
    enable_controllers(&boxlite_cgroup)?;

    // Create box cgroup
    if !box_cgroup.exists() {
        fs::create_dir(&box_cgroup).map_err(|e| {
            JailerError::Cgroup(format!(
                "Failed to create box cgroup at {}: {}",
                box_cgroup.display(),
                e
            ))
        })?;
    }

    // Apply limits
    apply_limits(&box_cgroup, config)?;

    tracing::debug!(
        box_id = %box_id,
        path = %box_cgroup.display(),
        "Cgroup created"
    );

    Ok(box_cgroup)
}

/// Delegate controllers to child cgroups — but only those actually available
/// here. cgroup v2 rejects the *entire* `cgroup.subtree_control` write if any
/// named controller is absent, so the literal `+cpu +memory +pids` fails on
/// rootless/systemd-user hosts where the session is delegated only `memory`
/// and `pids` (no `cpu`). That failure left box cgroups with no controllers
/// and the DoS limits silently unenforced. Enable the intersection of what we
/// want with `cgroup.controllers` instead, so memory/pids still apply when cpu
/// isn't delegated.
fn enable_controllers(cgroup_path: &Path) -> Result<(), JailerError> {
    let controllers_path = cgroup_path.join("cgroup.controllers");
    let available = fs::read_to_string(&controllers_path).map_err(|e| {
        JailerError::Cgroup(format!(
            "Failed to read available controllers at {}: {}",
            controllers_path.display(),
            e
        ))
    })?;

    let enable: Vec<String> = ["cpu", "memory", "pids"]
        .iter()
        .filter(|want| available.split_whitespace().any(|have| have == **want))
        .map(|want| format!("+{want}"))
        .collect();

    if enable.is_empty() {
        return Err(JailerError::Cgroup(format!(
            "none of cpu/memory/pids are delegated to {} (available: [{}])",
            cgroup_path.display(),
            available.trim()
        )));
    }

    write_file(
        &cgroup_path.join("cgroup.subtree_control"),
        &enable.join(" "),
    )?;
    Ok(())
}

/// Apply resource limits to a cgroup.
fn apply_limits(cgroup_path: &Path, config: &CgroupConfig) -> Result<(), JailerError> {
    // Memory limit
    if let Some(memory_max) = config.memory_max {
        write_file(&cgroup_path.join("memory.max"), &memory_max.to_string())?;
    }

    // Memory high (throttle threshold)
    if let Some(memory_high) = config.memory_high {
        write_file(&cgroup_path.join("memory.high"), &memory_high.to_string())?;
    }

    // CPU weight
    if let Some(cpu_weight) = config.cpu_weight {
        write_file(&cgroup_path.join("cpu.weight"), &cpu_weight.to_string())?;
    }

    // CPU max (quota period)
    if let Some((quota, period)) = config.cpu_max {
        write_file(
            &cgroup_path.join("cpu.max"),
            &format!("{} {}", quota, period),
        )?;
    }

    // Pids max
    if let Some(pids_max) = config.pids_max {
        write_file(&cgroup_path.join("pids.max"), &pids_max.to_string())?;
    }

    Ok(())
}

/// Add a process to a cgroup.
///
/// Call this after spawning the process.
#[allow(dead_code)]
pub fn add_process(box_id: &str, pid: u32) -> Result<(), JailerError> {
    let cgroup_path = cgroup_path(box_id);
    let procs_file = cgroup_path.join("cgroup.procs");

    write_file(&procs_file, &pid.to_string())?;

    tracing::debug!(
        box_id = %box_id,
        pid = pid,
        "Process added to cgroup"
    );

    Ok(())
}

/// Remove a cgroup.
///
/// The cgroup must be empty (no processes) before removal.
#[allow(dead_code)]
pub fn remove_cgroup(box_id: &str) -> Result<(), JailerError> {
    let cgroup_path = cgroup_path(box_id);

    if cgroup_path.exists() {
        fs::remove_dir(&cgroup_path).map_err(|e| {
            JailerError::Cgroup(format!(
                "Failed to remove cgroup at {}: {}",
                cgroup_path.display(),
                e
            ))
        })?;

        tracing::debug!(
            box_id = %box_id,
            "Cgroup removed"
        );
    }

    Ok(())
}

/// Helper to write to a cgroup file.
fn write_file(path: &Path, content: &str) -> Result<(), JailerError> {
    fs::write(path, content)
        .map_err(|e| JailerError::Cgroup(format!("Failed to write to {}: {}", path.display(), e)))
}

/// Convert ResourceLimits to CgroupConfig.
impl From<&ResourceLimits> for CgroupConfig {
    fn from(limits: &ResourceLimits) -> Self {
        Self {
            memory_max: limits.max_memory,
            memory_high: limits.max_memory.map(|m| m * 9 / 10), // 90% of max
            cpu_weight: None,                                   // Could add to ResourceLimits
            cpu_max: limits.max_cpu_time.map(|t| {
                // Convert seconds to quota/period
                // 1 CPU = 100000/100000
                (t * 1_000_000, 1_000_000)
            }),
            pids_max: limits.max_processes,
        }
    }
}

// ============================================================================
// Cgroup join: async-signal-safe write in the child, verified from the parent
// ============================================================================
//
// The join has to happen in `pre_exec`, between `fork()` and `exec()`. Writing
// the PID from the parent after `spawn()` returns leaves a window in which the
// child is already running: `cgroup.procs` moves the one PID it is given, and
// anything the child forked inside that window stays outside the limits. There
// is no synchronisation available to close it, so the write stays in the child
// where it is ordered before the box does anything at all.
//
// The cost of that placement is that `pre_exec` cannot report: it runs in a
// forked child of a threaded process, where allocation and locks are not
// guaranteed to work, so there is no `tracing`, and even `io::Error::new`
// allocates. Returning `Err` is possible but fatal — it aborts the spawn, which
// is the wrong answer for "limits could not be applied". Hence the split below:
// the child writes, the parent reads `cgroup.procs` back and warns.

/// Pre-compute the `cgroup.procs` path for [`add_self_to_cgroup_raw`].
///
/// Done in the parent, where allocation is allowed, so the `pre_exec` hook only
/// has to hand an already-built C string to `open(2)`.
#[cfg(target_os = "linux")]
pub fn build_cgroup_procs_path(box_id: &str) -> Option<std::ffi::CString> {
    if !is_cgroup_v2_available() {
        return None;
    }
    let path = cgroup_path(box_id).join("cgroup.procs");
    std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()
}

/// Add the calling process to a cgroup — async-signal-safe, for `pre_exec`.
///
/// Only `open`/`write`/`close` and stack memory: no allocation, no locks, no
/// `tracing`. The error is a bare errno because constructing anything richer
/// would allocate. The caller cannot report it from here — that is what
/// [`verify_joined`] is for.
#[cfg(target_os = "linux")]
pub fn add_self_to_cgroup_raw(cgroup_procs_path: &std::ffi::CStr) -> Result<(), i32> {
    // Format the PID by hand: `write!`/`format!` may allocate.
    let mut pid_buf = [0u8; 24];
    let mut pid = unsafe { libc::getpid() } as u64;
    let mut len = 0usize;
    if pid == 0 {
        pid_buf[0] = b'0';
        len = 1;
    } else {
        let mut digits = [0u8; 20];
        let mut n = 0;
        while pid > 0 {
            digits[n] = b'0' + (pid % 10) as u8;
            pid /= 10;
            n += 1;
        }
        while n > 0 {
            n -= 1;
            pid_buf[len] = digits[n];
            len += 1;
        }
    }
    pid_buf[len] = b'\n';
    len += 1;

    let fd = unsafe { libc::open(cgroup_procs_path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(last_errno());
    }
    let written = unsafe { libc::write(fd, pid_buf.as_ptr() as *const libc::c_void, len) };
    let err = if written < 0 {
        Some(last_errno())
    } else {
        None
    };
    unsafe { libc::close(fd) };
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Read `errno` without allocating — safe to call from `pre_exec`.
///
/// `io::Error::last_os_error()` only wraps the raw value; unlike `Error::new`
/// it does not allocate, so it is usable in the post-fork context.
#[cfg(target_os = "linux")]
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// Confirm from the parent that the child actually landed in the box's cgroup.
///
/// Reads `cgroup.procs` back. This is the reporting half of the join: the child
/// could not tell anyone it failed, so the parent checks and warns. It also
/// catches the case the write itself cannot — a cgroup directory that exists
/// but whose limit files were never written (the rootless `EACCES` path), since
/// a box in a cgroup with no limits is indistinguishable from an unconfined one
/// until something reads the hierarchy back.
///
/// Caller and `pid` must share a PID namespace. The jailer runs on the host and
/// writes host PIDs, so that holds here; read from inside another namespace the
/// kernel projects unmappable PIDs as `0` and this would report a false miss.
#[cfg(target_os = "linux")]
pub fn verify_joined(box_id: &str, pid: u32) -> Result<(), String> {
    let path = cgroup_path(box_id).join("cgroup.procs");
    let contents =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    // The outer PID is the one `pre_exec` wrote. bwrap execs in place, so it is
    // still the process we spawned; descendants it forked afterwards inherit
    // the cgroup and appear alongside it.
    if contents
        .split_whitespace()
        .any(|entry| entry == pid.to_string())
    {
        return Ok(());
    }
    if contents.trim().is_empty() {
        return Err(format!(
            "cgroup {} is empty: the pre_exec join did not take effect",
            path.display()
        ));
    }
    Err(format!(
        "pid {pid} is not in {}; the box is running outside its cgroup",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cgroup_path() {
        let path = cgroup_path("test-box-123");
        // Path depends on whether running as root or regular user
        let expected_base = get_cgroup_base();
        let expected = expected_base.join("boxlite").join("test-box-123");
        assert_eq!(path, expected);
        // Verify the path ends with the expected suffix
        assert!(path.ends_with("boxlite/test-box-123"));
    }

    #[test]
    fn test_cgroup_v2_detection() {
        let available = is_cgroup_v2_available();
        println!("Cgroup v2 available: {}", available);
    }

    #[test]
    fn kill_cgroup_absent_is_noop() {
        // No cgroup exists for this id, so `cgroup.kill` can't be written:
        // kill_cgroup must report `false` and not panic. This locks the
        // best-effort/idempotent contract relied on by the no-jailer and
        // macOS-seatbelt paths (where there is no box cgroup to kill).
        let box_id = BoxID::parse("nonexistentbox000000000000").expect("valid id");
        assert!(
            !kill_cgroup(&box_id),
            "kill_cgroup must be a no-op (false) when the box has no cgroup"
        );
    }

    // Note: there is no `kill_cgroup_rejects_non_component_box_ids` test anymore.
    // The path-traversal guard moved into the type: `kill_cgroup` takes a
    // `BoxID`, and `BoxID::parse` already rejects `/`, `\`, `.`, `..`, and empty
    // ids (see `id::tests::test_parse_rejects_unsafe_characters`). A non-component
    // id is now unrepresentable at this call site, not merely rejected at runtime.

    #[test]
    fn test_cgroup_config_from_limits() {
        let limits = ResourceLimits {
            max_memory: Some(1024 * 1024 * 1024), // 1GB
            max_processes: Some(100),
            max_cpu_time: Some(60), // 60 seconds
            ..Default::default()
        };

        let config = CgroupConfig::from(&limits);

        assert_eq!(config.memory_max, Some(1024 * 1024 * 1024));
        assert_eq!(config.pids_max, Some(100));
        assert!(config.cpu_max.is_some());
    }

    /// The whole point of the rootless fix in this PR: when the parent's
    /// `cgroup.controllers` doesn't list `cpu` (the common rootless case),
    /// the atomic `+cpu +memory +pids` write fails and takes memory+pids
    /// down with it. `enable_controllers` must intersect what we want with
    /// what's available, so memory/pids still get delegated when cpu isn't.
    #[test]
    fn enable_controllers_writes_only_intersection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cgroup_path = dir.path();
        std::fs::write(cgroup_path.join("cgroup.controllers"), "memory pids\n")
            .expect("write cgroup.controllers");
        std::fs::write(cgroup_path.join("cgroup.subtree_control"), "")
            .expect("write cgroup.subtree_control");

        enable_controllers(cgroup_path).expect("must succeed with non-empty intersection");

        let written = std::fs::read_to_string(cgroup_path.join("cgroup.subtree_control"))
            .expect("read cgroup.subtree_control");
        assert!(
            written.contains("+memory"),
            "must enable memory when delegated; subtree_control={written:?}"
        );
        assert!(
            written.contains("+pids"),
            "must enable pids when delegated; subtree_control={written:?}"
        );
        assert!(
            !written.contains("+cpu"),
            "must NOT try to enable cpu when not delegated (the atomic write would fail and \
             take memory+pids with it); subtree_control={written:?}"
        );
    }

    /// All three controllers delegated (root / fully-privileged): every want
    /// is in the available set, all three get written.
    #[test]
    fn enable_controllers_writes_all_when_all_delegated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cgroup_path = dir.path();
        std::fs::write(
            cgroup_path.join("cgroup.controllers"),
            "cpuset cpu io memory hugetlb pids rdma misc\n",
        )
        .expect("write cgroup.controllers");
        std::fs::write(cgroup_path.join("cgroup.subtree_control"), "")
            .expect("write cgroup.subtree_control");

        enable_controllers(cgroup_path).expect("must succeed");

        let written = std::fs::read_to_string(cgroup_path.join("cgroup.subtree_control"))
            .expect("read cgroup.subtree_control");
        for want in ["+cpu", "+memory", "+pids"] {
            assert!(written.contains(want), "missing {want} in {written:?}");
        }
    }

    /// Pathological host: none of {cpu, memory, pids} are delegated. The
    /// function must Err loudly rather than silently writing an empty
    /// subtree_control (which would land later limits in the wrong place).
    #[test]
    fn enable_controllers_errors_when_none_delegated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cgroup_path = dir.path();
        std::fs::write(cgroup_path.join("cgroup.controllers"), "io rdma\n")
            .expect("write cgroup.controllers");
        std::fs::write(cgroup_path.join("cgroup.subtree_control"), "")
            .expect("write cgroup.subtree_control");

        let err = enable_controllers(cgroup_path).expect_err("must err on empty intersection");
        let msg = format!("{err}");
        assert!(
            msg.contains("none of cpu/memory/pids"),
            "error must spell out the missing controllers; got {msg:?}"
        );
    }
}
