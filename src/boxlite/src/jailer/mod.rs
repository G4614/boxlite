//! Jailer module for BoxLite security isolation.
//!
//! This module provides defense-in-depth security for the boxlite-shim process,
//! implementing multiple isolation layers inspired by Firecracker's jailer.
//!
//! For the complete security design, see [`THREAT_MODEL.md`](./THREAT_MODEL.md).
//!
//! # Architecture
//!
//! ```text
//! Jail (trait — public contract, what callers see)
//! │   prepare()  → pre-spawn setup
//! │   command()  → confined command, ready to spawn
//! │
//! └── Jailer<S: Sandbox> (struct — implements Jail)
//!     │   translates SecurityOptions → SandboxContext
//!     │   delegates to S, adds pre_exec hook
//!     │
//!     └── Sandbox (trait — internal, platform-specific wrapping)
//!         ├── BwrapSandbox       (Linux — bubblewrap)
//!         ├── SeatbeltSandbox    (macOS — sandbox-exec)
//!         └── NoopSandbox        (unsupported / jailer disabled)
//! ```
//!
//! # Security Layers
//!
//! ## Linux
//! 1. **Namespace isolation** - Mount, PID, network namespaces
//! 2. **Chroot/pivot_root** - Filesystem isolation
//! 3. **Seccomp filtering** - Syscall whitelist
//! 4. **Privilege dropping** - Run as unprivileged user
//! 5. **Resource limits** - cgroups v2, rlimits
//!
//! ## macOS
//! 1. **Sandbox (Seatbelt)** - sandbox-exec with SBPL profile
//! 2. **Resource limits** - rlimits
//!
//! # Usage
//!
//! ```ignore
//! let jail = JailerBuilder::new()
//!     .with_box_id(&box_id)
//!     .with_layout(layout)
//!     .with_security(security)
//!     .build()?;
//!
//! jail.prepare()?;
//! let cmd = jail.command(&binary, &args);
//! cmd.spawn()?;
//! ```

// ============================================================================
// Module declarations
// ============================================================================

// Core modules
mod builder;
mod command;
mod common;
mod error;
mod pre_exec;
pub(crate) mod sandbox;
pub(crate) mod shim_copy;

// Linux-only modules
#[cfg(target_os = "linux")]
pub(crate) mod apparmor;
#[cfg(target_os = "linux")]
pub(crate) mod bwrap;
#[cfg(target_os = "linux")]
pub(crate) mod cgroup;
#[cfg(target_os = "linux")]
pub(crate) mod credentials;
#[cfg(target_os = "linux")]
pub mod landlock;
#[cfg(target_os = "linux")]
pub mod seccomp;

// ============================================================================
// Public re-exports
// ============================================================================

// Core types
pub use crate::runtime::advanced_options::{ResourceLimits, SecurityOptions};
pub use builder::JailerBuilder;
pub use error::{ConfigError, IsolationError, JailerError, SystemError};
pub use sandbox::{
    CompositeSandbox, NoopSandbox, PathAccess, PlatformSandbox, Sandbox, SandboxContext,
};

// Volume specification (convenience re-export)
pub use crate::runtime::options::VolumeSpec;

// Linux-specific exports
#[cfg(target_os = "linux")]
pub use bwrap::{build_shim_command, is_available as is_bwrap_available};
#[cfg(target_os = "linux")]
pub use landlock::{build_landlock_ruleset, is_landlock_available};
#[cfg(target_os = "linux")]
pub use sandbox::{BwrapSandbox, LandlockSandbox};
#[cfg(target_os = "linux")]
pub use seccomp::SeccompRole;

// macOS-specific exports
#[cfg(target_os = "macos")]
pub use sandbox::SeatbeltSandbox;
#[cfg(target_os = "macos")]
pub use sandbox::seatbelt::{
    SANDBOX_EXEC_PATH, get_base_policy, get_network_policy, is_sandbox_available,
};

// ============================================================================
// Jail trait — public contract
// ============================================================================

use boxlite_shared::errors::BoxliteResult;
use std::path::Path;
use std::process::Command;

/// Process confinement for subprocess isolation.
///
/// Provides the public contract for building isolated commands.
/// Callers don't know or care about the mechanism (bwrap, sandbox-exec, etc.).
///
/// ```ignore
/// let jail: &impl Jail = &jailer;
/// jail.prepare()?;
/// let cmd = jail.command(&binary, &args);
/// cmd.spawn()?;
/// ```
pub trait Jail: Send + Sync {
    /// Pre-spawn setup. Call before `command()`.
    ///
    /// On Linux: userns preflight + cgroup creation.
    /// On macOS: no-op.
    fn prepare(&self) -> BoxliteResult<()>;

    /// Build a confined command, ready to spawn.
    ///
    /// Returns a `Command` with sandbox wrapping and pre_exec hook
    /// (FD cleanup, rlimits, cgroup join, PID file).
    fn command(&self, binary: &Path, args: &[String]) -> Command;
}

// ============================================================================
// Jailer<S: Sandbox> — implements Jail
// ============================================================================

use crate::disk::read_backing_chain;
use crate::runtime::layout::BoxFilesystemLayout;
use std::path::PathBuf;

// ============================================================================
// Path access rules — granular filesystem permissions
// ============================================================================

/// Build granular [`PathAccess`] rules from the box layout.
///
/// Instead of granting access to the entire box directory, each file and
/// directory is listed individually with the minimum required access level.
///
/// ## Sandbox filesystem layout
///
/// ```text
/// {box_dir}/                          # NOT granted wholesale
/// ├── bin/                        [RO]  # copied shim binary + libkrunfw
/// ├── shared/                     [RW]  # guest-visible virtio-fs share root
/// ├── sockets/                    [RW]  # libkrun vsock/unix sockets
/// ├── tmp/                        [RW]  # shim/libkrun transient temp files
/// ├── logs/                       [RW]  # shim logging + VM console output
/// │   ├── boxlite-shim.log                # tracing_appender daily log
/// │   └── console.log                     # libkrun serial console (krun_set_console_output)
/// ├── exit                        [RW]  # crash_capture ExitInfo JSON
/// ├── disks/                      [RW]  # disk images
/// │   ├── disk.qcow2                      # VM/container root disk image
/// │   └── guest-rootfs.qcow2              # guest rootfs COW overlay
/// ├── mounts/                     [--]  # EXCLUDED: host writes, shim reads via shared/
/// ├── shim.pid                    [--]  # EXCLUDED: written by pre_exec (before sandbox)
/// └── shim.stderr                 [--]  # EXCLUDED: host creates before spawn
///
/// External read-only paths:
/// ~/.boxlite/rootfs/              [RO]  # shared guest rootfs backing directory
/// ~/.boxlite/layers/              [RO]  # disk fork points (snapshot/clone bases)
///
/// User volumes:
/// {host_path}                     [per VolumeSpec.read_only]
/// ```
fn build_path_access(layout: &BoxFilesystemLayout, volumes: &[VolumeSpec]) -> Vec<PathAccess> {
    let mut paths = Vec::new();

    // Writable directories (shim creates files inside these at runtime)
    // Note: mounts_dir not included — host writes before spawn, shim accesses via shared_dir
    for dir in [layout.sockets_dir(), layout.tmp_dir(), layout.logs_dir()] {
        if dir.exists() {
            paths.push(PathAccess {
                path: dir,
                writable: true,
            });
        }
    }

    // Writable files (pre-created before sandbox for bind-mounting)
    // Note: console_output_path() not listed — lives inside logs/ [RW subpath]
    for file in [
        layout.exit_file_path(),
        layout.disk_path(),
        layout.guest_rootfs_disk_path(),
    ] {
        if file.exists() {
            paths.push(PathAccess {
                path: file,
                writable: true,
            });
        }
    }

    // Qcow2 overlays may reference backing files outside box_dir (for example
    // ~/.boxlite/images/disk-images/*.ext4). Under deny-default seatbelt, those
    // backing files must be explicitly granted as read-only or libkrun fails
    // virtio-blk setup with EINVAL.
    //
    // Cloned boxes have multi-level backing chains (clone → source → base image),
    // so we traverse the full chain to grant access to every backing file.
    for qcow2 in [layout.disk_path(), layout.guest_rootfs_disk_path()] {
        if !qcow2.exists() {
            continue;
        }
        for backing_path in read_backing_chain(&qcow2) {
            if let Some(parent) = backing_path.parent().filter(|p| p.exists()) {
                paths.push(PathAccess {
                    path: parent.to_path_buf(),
                    writable: false,
                });
            }
            paths.push(PathAccess {
                path: backing_path,
                writable: false,
            });
        }
    }

    // Read-only directory (copied shim binary + libkrunfw)
    let bin_dir = layout.bin_dir();
    if bin_dir.exists() {
        paths.push(PathAccess {
            path: bin_dir,
            writable: false,
        });
    }

    // shared/ is exposed as a read-write virtio-fs share root on macOS.
    // libkrun's passthrough fs opens this path during worker init; under
    // deny-default seatbelt it must be writable to avoid EPERM startup panics.
    let shared_dir = layout.shared_dir();
    if shared_dir.exists() {
        paths.push(PathAccess {
            path: shared_dir,
            writable: true,
        });
    }

    // Bases directory: shared backing files (snapshots, clone bases, rootfs cache).
    // The qcow2 overlay references backing files in bases/ directly.
    // Disk images are data (read by the hypervisor, not executed on the host).
    if let Some(bases_dir) = layout
        .root()
        .parent()
        .and_then(|boxes| boxes.parent())
        .map(|home| home.join("bases"))
        .filter(|p| p.exists())
    {
        paths.push(PathAccess {
            path: bases_dir,
            writable: false,
        });
    }

    // User volumes
    for vol in volumes {
        let p = PathBuf::from(&vol.host_path);
        if p.exists() {
            paths.push(PathAccess {
                path: p,
                writable: !vol.read_only,
            });
        }
    }

    paths
}

/// Jailer provides process isolation for boxlite-shim.
///
/// Encapsulates security configuration and delegates to a [`Sandbox`]
/// for platform-specific wrapping. All common isolation (FD cleanup,
/// rlimits, cgroup join) is applied via `pre_exec` hook.
///
/// Construct via [`JailerBuilder`]:
///
/// ```ignore
/// use boxlite::jailer::{Jail, JailerBuilder};
///
/// let jail = JailerBuilder::new()
///     .with_box_id(&box_id)
///     .with_layout(layout)
///     .with_security(security)
///     .build()?;
///
/// jail.prepare()?;
/// let cmd = jail.command(&binary, &args);
/// cmd.spawn()?;
/// ```
#[derive(Debug)]
pub struct Jailer<S: Sandbox> {
    /// Platform-specific sandbox implementation.
    sandbox: S,
    /// Security configuration options.
    pub(crate) security: SecurityOptions,
    /// Volume mounts (for sandbox path restrictions).
    pub(crate) volumes: Vec<VolumeSpec>,
    /// Unique box identifier.
    pub(crate) box_id: String,
    /// Box filesystem layout (provides typed path accessors).
    pub(crate) layout: BoxFilesystemLayout,
    /// FDs to preserve through pre_exec: each (source_fd, target_fd) is dup2'd
    /// before FD cleanup. Used for watchdog pipe inheritance across fork.
    pub(crate) preserved_fds: Vec<(std::os::fd::RawFd, i32)>,
    /// Detach-mode process isolation: see [`pre_exec::add_pre_exec_hook`]
    /// — `true` adds `setsid()` to the pre_exec chain, `false` sets the
    /// child's process group to itself at `Command` build time.
    pub(crate) detach: bool,
    /// VM guest memory in MiB, used to derive the host cgroup memory limit.
    /// Read only on Linux (`setup_host_cgroup` is `#[cfg(target_os = "linux")]`),
    /// so quiet dead-code on the macOS build.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) vm_memory_mib: Option<u32>,
}

impl<S: Sandbox> Jail for Jailer<S> {
    fn prepare(&self) -> BoxliteResult<()> {
        // Host cgroup limits are independent of process-isolation sandboxing:
        // creating the cgroup only writes to /sys/fs/cgroup and needs no user
        // namespace, so it runs even when jailer_enabled is false.
        #[cfg(target_os = "linux")]
        self.setup_host_cgroup();

        if !self.security.jailer_enabled {
            return Ok(());
        }
        self.sandbox.setup(&self.context())
    }

    fn command(&self, binary: &Path, args: &[String]) -> Command {
        // Pre-create writable files + dirs for sandbox bind-mounting
        if self.security.jailer_enabled {
            let _ = std::fs::create_dir_all(self.layout.logs_dir());
            for path in [
                self.layout.exit_file_path(),
                self.layout.console_output_path(),
            ] {
                if !path.exists() {
                    let _ = std::fs::File::create(&path);
                }
            }
        }

        let mut ctx = self.context();

        // Grant read access to original binary's library directory so the
        // dynamic linker can load libraries from the original location.
        #[allow(clippy::collapsible_if)]
        if self.security.jailer_enabled {
            if let Some(lib_dir) = binary.parent().filter(|d| d.exists()) {
                ctx.paths.push(PathAccess {
                    path: lib_dir.to_path_buf(),
                    writable: false,
                });
            }
        }

        // Shim copy (Firecracker pattern) — shared for both platforms
        let effective_binary = if self.security.jailer_enabled {
            match shim_copy::copy_shim_to_box(binary, self.layout.root()) {
                Ok(copied) => {
                    tracing::info!(
                        original = %binary.display(),
                        copied = %copied.display(),
                        "Using copied shim binary (Firecracker pattern)"
                    );
                    copied
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to copy shim, using original");
                    binary.to_path_buf()
                }
            }
        } else {
            binary.to_path_buf()
        };

        // Start with a bare command. Sandbox.apply() modifies it in-place.
        let mut cmd = Command::new(&effective_binary);
        cmd.args(args);

        if self.security.jailer_enabled && self.sandbox.is_available() {
            tracing::info!(sandbox = self.sandbox.name(), "Applying sandbox isolation");
            self.sandbox.apply(&ctx, &mut cmd);
        } else if self.security.jailer_enabled {
            tracing::warn!("Sandbox not available, falling back to direct command");
        } else {
            tracing::info!("Jailer disabled, running shim without sandbox isolation");
        }

        // Join the host cgroup before the common hook so all subsequent
        // resource use is accounted to it. Independent of jailer_enabled,
        // matching the cgroup creation in prepare().
        #[cfg(target_os = "linux")]
        self.add_cgroup_join_hook(&mut cmd);

        // Pre-exec hook: FD preservation, FD cleanup, rlimits, PID file.
        // Sandbox-specific pre_exec hooks (Landlock) are already added by
        // sandbox.apply() above — Command supports multiple pre_exec closures.
        let resource_limits = self.security.resource_limits.clone();
        let pid_writer = self.pid_file_writer();
        pre_exec::add_pre_exec_hook(
            &mut cmd,
            resource_limits,
            pid_writer,
            self.preserved_fds.clone(),
            self.detach,
        );
        cmd
    }
}

impl<S: Sandbox> Jailer<S> {
    /// Get the security options.
    pub fn security(&self) -> &SecurityOptions {
        &self.security
    }

    /// Get mutable reference to security options.
    pub fn security_mut(&mut self) -> &mut SecurityOptions {
        &mut self.security
    }

    /// Get the volumes.
    pub fn volumes(&self) -> &[VolumeSpec] {
        &self.volumes
    }

    /// Get the box ID.
    pub fn box_id(&self) -> &str {
        &self.box_id
    }

    /// Get the box directory.
    pub fn box_dir(&self) -> &Path {
        self.layout.root()
    }

    /// Get the box filesystem layout.
    pub fn layout(&self) -> &BoxFilesystemLayout {
        &self.layout
    }

    /// Get the resource limits.
    pub fn resource_limits(&self) -> &ResourceLimits {
        &self.security.resource_limits
    }

    /// Translate SecurityOptions → SandboxContext.
    ///
    /// Delegates to [`build_path_access`] for granular filesystem rules.
    fn context(&self) -> SandboxContext<'_> {
        let paths = build_path_access(&self.layout, &self.volumes);
        tracing::debug!(
            box_id = %self.box_id,
            path_count = paths.len(),
            paths = ?paths,
            "Built sandbox path access list"
        );
        if std::env::var_os("BOXLITE_DEBUG_PRINT_SEATBELT").is_some() {
            eprintln!("BOXLITE_DEBUG paths for {}: {:#?}", self.box_id, paths);
        }

        SandboxContext {
            id: &self.box_id,
            paths,
            resource_limits: &self.security.resource_limits,
            network_enabled: self.security.network_enabled,
            sandbox_profile: self.security.sandbox_profile.as_deref(),
        }
    }

    /// Pre-allocate the PID file writer for the pre_exec hook. Returns
    /// `None` if the path can't be made into a CString (interior NUL).
    fn pid_file_writer(&self) -> Option<crate::util::PidFileWriter> {
        crate::util::PidFileWriter::at(&self.layout.pid_file_path()).ok()
    }

    /// Build the host cgroup config: explicit `resource_limits` plus default
    /// DoS limits (pids.max, memory.max derived from VM memory). These defaults
    /// populate only the cgroup — never the rlimit pre_exec hook — so they
    /// can't trigger RLIMIT_AS/NPROC/CPU, which would break or kill the VM.
    #[cfg(target_os = "linux")]
    fn cgroup_config(&self) -> cgroup::CgroupConfig {
        use crate::runtime::constants::vm_defaults::DEFAULT_MEMORY_MIB;

        /// Default host process cap. Baseline box uses ~22 host tasks (libkrun
        /// vCPUs + gvproxy + tokio); 1024 leaves wide headroom while still
        /// catching a runaway thread/fork leak in the VMM stack.
        const DEFAULT_HOST_PIDS_MAX: u64 = 1024;

        let limits = &self.security.resource_limits;
        let mut config = cgroup::CgroupConfig::from(limits);

        // memory.max: explicit override wins; otherwise 2× VM RAM + 512 MiB.
        // Guest RAM is hard-capped at VM size by libkrun, so this only fires on
        // VMM-side leaks — a deliberately loose cap that never kills a healthy box.
        if config.memory_max.is_none() {
            let vm_mib = self.vm_memory_mib.unwrap_or(DEFAULT_MEMORY_MIB) as u64;
            config.memory_max = Some(vm_mib * 2 * 1024 * 1024 + 512 * 1024 * 1024);
        }
        // pids.max: explicit override wins; otherwise the default cap.
        if config.pids_max.is_none() {
            config.pids_max = Some(DEFAULT_HOST_PIDS_MAX);
        }
        // CPU cap: explicit `ResourceLimits.max_cpu_time` wins (lands in
        // `cpu_max`); otherwise default to the host's online core count, in the
        // same spirit as `memory.max = 2× VM` and `pids.max = 1024` — a loose
        // ceiling that no healthy box hits, but bounds a runaway VMM that
        // spawns spinning threads. Set both `cpu_max` (rootful direct
        // cgroup-file write in `apply_limits`) and `cpu_quota_us_per_sec`
        // (rootless busctl property in `adopt_pid_into_scope`) so the cap
        // applies symmetrically — previously rootless deployments had NO CPU
        // cap whatsoever even when the user set `max_cpu_time`, because the
        // file-write path doesn't run rootless.
        let host_cores = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);
        let host_cpu_us_per_sec = host_cores.saturating_mul(1_000_000);
        if config.cpu_max.is_none() {
            config.cpu_max = Some((host_cpu_us_per_sec, 1_000_000));
        }
        if config.cpu_quota_us_per_sec.is_none() {
            config.cpu_quota_us_per_sec = Some(host_cpu_us_per_sec);
        }
        config
    }

    /// Create the host cgroup and write resource limits. Failure is non-fatal:
    /// a box that can't be cgroup-limited (e.g. no systemd user delegation) is
    /// still better than no box (matches prior bwrap behavior).
    #[cfg(target_os = "linux")]
    fn setup_host_cgroup(&self) {
        let config = self.cgroup_config();
        if !config.has_limits() {
            return;
        }
        // Root creates the cgroup up-front; the shim joins it via the pre_exec
        // hook. Rootless can't migrate a process across the root-owned
        // user.slice (EACCES), so limits are applied after spawn by adopting the
        // shim PID into a systemd scope — see place_shim_in_scope().
        if !cgroup::is_root() {
            return;
        }
        match cgroup::setup_cgroup(&self.box_id, &config) {
            Ok(path) => {
                tracing::info!(box_id = %self.box_id, path = %path.display(), "Host cgroup created")
            }
            // Loud (error, not warn): the whole point of this PR is to make
            // hosts safe against DoS, and silently degrading to unlimited is
            // exactly the failure mode the review flagged. The box still
            // starts (best-effort by design — see comment above), but the
            // operator needs to SEE this so they can fix the underlying
            // delegation / cgroup-v2 problem.
            Err(e) => tracing::error!(box_id = %self.box_id, error = %e,
                "Host cgroup setup failed — box starts without resource limits; fix the underlying delegation issue"),
        }
    }

    /// Add the async-signal-safe cgroup-join hook to the command. No-op when
    /// no limit is configured (no cgroup was created in prepare()).
    #[cfg(target_os = "linux")]
    fn add_cgroup_join_hook(&self, cmd: &mut Command) {
        // Only root joins the cgroup via pre_exec — it created the cgroup in
        // prepare() and can migrate into it. A rootless process can't, so its
        // limits are applied post-spawn by place_shim_in_scope() instead.
        if !cgroup::is_root() || !self.cgroup_config().has_limits() {
            return;
        }
        if let Some(cgroup_procs) = cgroup::build_cgroup_procs_path(&self.box_id) {
            use std::os::unix::process::CommandExt;
            // SAFETY: add_self_to_cgroup_raw uses only async-signal-safe syscalls.
            unsafe {
                cmd.pre_exec(move || {
                    let _ = cgroup::add_self_to_cgroup_raw(&cgroup_procs);
                    Ok(())
                });
            }
        }
    }

    /// Rootless host-limit placement: once the shim is running, ask systemd to
    /// adopt its PID into a transient scope carrying the configured limits.
    /// Call this right after spawning the shim. No-op for root (handled up-front
    /// by setup_host_cgroup + the pre_exec join) or when no limit is configured.
    /// Non-fatal — an unscoped box beats no box.
    #[cfg(target_os = "linux")]
    pub(crate) fn place_shim_in_scope(&self, pid: u32) {
        let config = self.cgroup_config();
        if !config.has_limits() || cgroup::is_root() {
            return;
        }
        match cgroup::adopt_pid_into_scope(&self.box_id, pid, &config) {
            Ok(()) => {
                tracing::info!(box_id = %self.box_id, pid, "Shim adopted into host cgroup scope")
            }
            // Same as setup_host_cgroup above: best-effort by design, but the
            // failure must be LOUD so operators notice (missing busctl,
            // no systemd user manager, dbus errors, etc. are all real
            // causes a silent warn would hide).
            Err(e) => tracing::error!(box_id = %self.box_id, pid, error = %e,
                "Host cgroup scope adoption failed — shim runs WITHOUT host limits; check busctl / systemd --user availability"),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::layout::FsLayoutConfig;
    use tempfile::tempdir;

    fn test_layout(box_dir: PathBuf) -> BoxFilesystemLayout {
        BoxFilesystemLayout::new(box_dir, FsLayoutConfig::without_bind_mount(), false)
    }

    #[test]
    fn test_build_path_access_empty_box_dir() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        let paths = build_path_access(&layout, &[]);

        // Empty box dir: no subdirectories exist yet, so no paths
        assert!(paths.is_empty(), "No paths for empty box dir");
    }

    #[cfg(target_os = "linux")]
    fn test_jailer(
        vm_memory_mib: Option<u32>,
        security: SecurityOptions,
    ) -> Jailer<PlatformSandbox> {
        let dir = tempdir().unwrap();
        crate::jailer::JailerBuilder::new()
            .with_box_id("cgroup-test")
            .with_layout(test_layout(dir.path().to_path_buf()))
            .with_security(security)
            .with_vm_memory_mib(vm_memory_mib)
            .build()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cgroup_config_defaults_scale_with_vm_memory() {
        // No explicit limits: defaults kick in. memory.max = 2× VM RAM + 512 MiB.
        let jail = test_jailer(Some(256), SecurityOptions::default());
        let config = jail.cgroup_config();

        assert_eq!(config.pids_max, Some(1024), "default host pids cap");
        assert_eq!(
            config.memory_max,
            Some(256 * 2 * 1024 * 1024 + 512 * 1024 * 1024),
            "memory.max derived from 256 MiB VM"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cgroup_config_defaults_use_vm_default_when_unset() {
        // No VM memory configured → falls back to DEFAULT_MEMORY_MIB (2048).
        let jail = test_jailer(None, SecurityOptions::default());
        let config = jail.cgroup_config();

        assert_eq!(
            config.memory_max,
            Some(2048 * 2 * 1024 * 1024 + 512 * 1024 * 1024),
            "memory.max derived from default 2048 MiB VM"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_cgroup_config_explicit_limits_override_defaults() {
        // Explicit resource_limits win over the derived defaults.
        let security = SecurityOptions {
            resource_limits: crate::runtime::advanced_options::ResourceLimits {
                max_processes: Some(50),
                max_memory: Some(100 * 1024 * 1024),
                ..Default::default()
            },
            ..SecurityOptions::default()
        };
        let jail = test_jailer(Some(256), security);
        let config = jail.cgroup_config();

        assert_eq!(config.pids_max, Some(50), "explicit pids override");
        assert_eq!(
            config.memory_max,
            Some(100 * 1024 * 1024),
            "explicit memory override"
        );
    }

    #[test]
    fn test_build_path_access_writable_dirs() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create writable dirs the shim would write to
        // Note: mounts_dir is NOT included — host writes before spawn, shim reads via shared_dir
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        std::fs::create_dir_all(layout.tmp_dir()).unwrap();
        std::fs::create_dir_all(layout.logs_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let writable_dirs: Vec<_> = paths
            .iter()
            .filter(|p| p.writable && p.path.is_dir())
            .collect();
        assert_eq!(
            writable_dirs.len(),
            3,
            "Should have 3 writable dirs (sockets, tmp, logs)"
        );

        // All should be writable
        for pa in &writable_dirs {
            assert!(pa.writable);
        }

        let tmp = paths.iter().find(|p| p.path == layout.tmp_dir());
        assert!(tmp.is_some(), "tmp/ should be included");
        assert!(tmp.unwrap().writable, "tmp/ should be writable");
    }

    #[test]
    fn test_build_path_access_writable_files() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Pre-create writable files (as the Jailer::command() does)
        // Note: console_output_path() is inside logs/ [RW subpath], not a standalone file grant
        std::fs::File::create(layout.exit_file_path()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let writable_files: Vec<_> = paths
            .iter()
            .filter(|p| p.writable && p.path.is_file())
            .collect();
        assert_eq!(
            writable_files.len(),
            1,
            "exit only (console.log covered by logs/ subpath)"
        );
    }

    #[test]
    fn test_build_path_access_ro_dirs() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create bin + shared dirs
        std::fs::create_dir_all(layout.bin_dir()).unwrap();
        std::fs::create_dir_all(layout.shared_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let bin = paths.iter().find(|p| p.path == layout.bin_dir());
        assert!(bin.is_some(), "bin/ should be included");
        assert!(!bin.unwrap().writable, "bin/ should be read-only");

        let shared = paths.iter().find(|p| p.path == layout.shared_dir());
        assert!(shared.is_some(), "shared/ should be included");
        assert!(shared.unwrap().writable, "shared/ should be writable");
    }

    #[test]
    fn test_build_path_access_shared_bases_dir() {
        // Simulate the home_dir/boxes/{id} structure
        let dir = tempdir().unwrap();
        let home_dir = dir.path().to_path_buf();
        let boxes_dir = home_dir.join("boxes");
        let box_dir = boxes_dir.join("test-box");
        std::fs::create_dir_all(&box_dir).unwrap();

        // Create home_dir/bases/ (shared backing files)
        let bases_dir = home_dir.join("bases");
        std::fs::create_dir_all(&bases_dir).unwrap();

        let layout = test_layout(box_dir);

        let paths = build_path_access(&layout, &[]);

        let bases_paths: Vec<_> = paths.iter().filter(|p| p.path == bases_dir).collect();
        assert_eq!(bases_paths.len(), 1, "Should include home_dir/bases/");
        assert!(!bases_paths[0].writable);
    }

    #[test]
    fn test_build_path_access_includes_qcow2_backing_file() {
        use crate::disk::{BackingFormat, Qcow2Helper};

        let dir = tempdir().unwrap();
        let home_dir = dir.path().to_path_buf();
        let boxes_dir = home_dir.join("boxes");
        let box_dir = boxes_dir.join("test-box");
        std::fs::create_dir_all(&box_dir).unwrap();

        // Simulate image cache backing file outside box_dir.
        let disk_images_dir = home_dir.join("images").join("disk-images");
        std::fs::create_dir_all(&disk_images_dir).unwrap();
        let base_disk = disk_images_dir.join("sha256-test.ext4");
        std::fs::write(&base_disk, vec![0u8; 1024 * 1024]).unwrap();

        let layout = test_layout(box_dir);
        let child_disk = Qcow2Helper::create_cow_child_disk(
            &base_disk,
            BackingFormat::Raw,
            &layout.disk_path(),
            16 * 1024 * 1024,
        )
        .unwrap();

        let paths = build_path_access(&layout, &[]);

        let expected_backing = base_disk.canonicalize().unwrap_or(base_disk);
        let backing_paths: Vec<_> = paths
            .iter()
            .filter(|p| {
                p.path.canonicalize().unwrap_or_else(|_| p.path.clone()) == expected_backing
            })
            .collect();
        assert_eq!(
            backing_paths.len(),
            1,
            "Expected qcow2 backing file to be included in sandbox paths"
        );
        assert!(!backing_paths[0].writable, "Backing file must be read-only");

        // Keep child disk alive until after assertions.
        let _ = child_disk.path();
    }

    #[test]
    fn test_build_path_access_volumes() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir);

        // Create volume host paths
        let vol_ro = dir.path().join("input");
        let vol_rw = dir.path().join("output");
        std::fs::create_dir_all(&vol_ro).unwrap();
        std::fs::create_dir_all(&vol_rw).unwrap();

        let volumes = vec![
            VolumeSpec {
                host_path: vol_ro.to_string_lossy().to_string(),
                guest_path: "/mnt/input".to_string(),
                read_only: true,
            },
            VolumeSpec {
                host_path: vol_rw.to_string_lossy().to_string(),
                guest_path: "/mnt/output".to_string(),
                read_only: false,
            },
        ];

        let paths = build_path_access(&layout, &volumes);

        let vol_paths: Vec<_> = paths
            .iter()
            .filter(|p| p.path == vol_ro || p.path == vol_rw)
            .collect();
        assert_eq!(vol_paths.len(), 2, "Both volumes should be listed");

        let ro_vol = vol_paths.iter().find(|p| p.path == vol_ro).unwrap();
        assert!(!ro_vol.writable, "RO volume should be read-only");

        let rw_vol = vol_paths.iter().find(|p| p.path == vol_rw).unwrap();
        assert!(rw_vol.writable, "RW volume should be writable");
    }

    #[test]
    fn test_build_path_access_nonexistent_volume_skipped() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        let volumes = vec![VolumeSpec {
            host_path: "/does/not/exist".to_string(),
            guest_path: "/mnt/data".to_string(),
            read_only: true,
        }];

        let paths = build_path_access(&layout, &volumes);

        assert!(
            paths.iter().all(|p| p.path != Path::new("/does/not/exist")),
            "Nonexistent volume should be skipped"
        );
    }

    #[test]
    fn test_build_path_access_no_whole_box_dir() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create all subdirectories
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        std::fs::create_dir_all(layout.mounts_dir()).unwrap();
        std::fs::create_dir_all(layout.logs_dir()).unwrap();
        std::fs::create_dir_all(layout.bin_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        // The box_dir itself should NOT appear as a path — only its children
        assert!(
            paths.iter().all(|p| p.path != box_dir),
            "box_dir should not be listed wholesale — only granular paths"
        );
    }

    /// mounts_dir must NOT appear in path access even when it exists on disk.
    /// The shim never writes to mounts/ — host writes before spawn, shim reads via shared_dir.
    #[test]
    fn test_build_path_access_mounts_dir_excluded() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());
        let mounts_base = layout.shared_layout().base().to_path_buf();

        // Create mounts_dir AND other dirs that SHOULD appear
        std::fs::create_dir_all(&mounts_base).unwrap();
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        std::fs::create_dir_all(layout.logs_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        // mounts_dir must be absent
        assert!(
            paths.iter().all(|p| p.path != mounts_base),
            "mounts_dir must NOT appear in path access"
        );

        // sockets_dir should be present (sanity check)
        assert!(
            paths.iter().any(|p| p.path == layout.sockets_dir()),
            "sockets_dir should be present"
        );
    }

    /// shared_dir must be writable because it is exposed as an RW virtio-fs share root.
    #[test]
    fn test_build_path_access_shared_dir_is_writable() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        std::fs::create_dir_all(layout.shared_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let shared = paths.iter().find(|p| p.path == layout.shared_dir());
        assert!(shared.is_some(), "shared_dir should be in path access");
        assert!(shared.unwrap().writable, "shared_dir must be writable");
    }

    /// After pre-creating files (as Jailer::command() does), all appear in path access as writable.
    /// console.log lives inside logs/ [RW subpath] — no separate PathAccess entry needed.
    #[test]
    fn test_build_path_access_captures_all_precreated_files() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        // Simulate pre-create (same as Jailer::command())
        std::fs::create_dir_all(layout.logs_dir()).unwrap();
        std::fs::File::create(layout.exit_file_path()).unwrap();
        std::fs::File::create(layout.console_output_path()).unwrap();

        let paths = build_path_access(&layout, &[]);

        // logs_dir covers both shim logs and console.log
        let logs = paths.iter().find(|p| p.path == layout.logs_dir());
        assert!(logs.is_some(), "logs_dir should be in path access");
        assert!(logs.unwrap().writable, "logs_dir should be writable");

        let exit = paths.iter().find(|p| p.path == layout.exit_file_path());
        assert!(exit.is_some(), "exit_file should be in path access");
        assert!(exit.unwrap().writable, "exit_file should be writable");

        // console.log should NOT have its own PathAccess — covered by logs/ subpath
        let console = paths
            .iter()
            .find(|p| p.path == layout.console_output_path());
        assert!(
            console.is_none(),
            "console.log should not be a standalone path access (covered by logs/)"
        );
    }

    /// End-to-end: builder -> prepare -> command with real tempdir.
    /// Verifies all the pieces (builder, layout, path access, pre-create) work together.
    #[test]
    fn test_jailer_full_flow_with_real_tempdir() {
        use crate::jailer::builder::JailerBuilder;
        use crate::runtime::advanced_options::SecurityOptions;

        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create a volume dir
        let vol_dir = dir.path().join("my-volume");
        std::fs::create_dir_all(&vol_dir).unwrap();

        let security = SecurityOptions {
            jailer_enabled: true,
            ..SecurityOptions::default()
        };

        let jail = JailerBuilder::new()
            .with_box_id("e2e-test")
            .with_layout(layout.clone())
            .with_security(security)
            .with_volumes(vec![VolumeSpec {
                host_path: vol_dir.to_string_lossy().to_string(),
                guest_path: "/mnt/data".to_string(),
                read_only: false,
            }])
            .build()
            .unwrap();

        // prepare() should succeed
        jail.prepare().unwrap();

        // command() should not panic and should pre-create files
        let _cmd = jail.command(
            std::path::Path::new("/usr/bin/boxlite-shim"),
            &["--engine".to_string(), "Libkrun".to_string()],
        );

        // Verify pre-create side effects
        assert!(
            layout.logs_dir().exists(),
            "logs_dir should be created by command()"
        );
        assert!(
            layout.exit_file_path().exists(),
            "exit file should be created by command()"
        );
        assert!(
            layout.console_output_path().exists(),
            "console.log should be created by command()"
        );
    }
}
