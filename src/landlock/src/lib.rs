// Copyright 2025 BoxLite Contributors
// SPDX-License-Identifier: Apache-2.0

//! Landlock rule building + enforcement, factored out of the boxlite runtime.
//!
//! Built as both an rlib (host-side rule building/serialization) and a cdylib
//! (`libboxlite_landlock.so`). bwrap `LD_PRELOAD`s the cdylib into the shim via
//! `--setenv` (gated by [`SEAL_MARKER_ENV`]); the library's `.init_array`
//! constructor applies Landlock in the shim's process — after bwrap finishes its
//! mounts, before the shim's `main()` — without an exec wrapper or any shim
//! source change. (A filesystem Landlock domain denies every mount syscall, so
//! Landlock cannot be applied before bwrap; the loader hook is the first point
//! that runs in the shim's process once bwrap has finished mounting.)

use std::path::PathBuf;

/// Environment variable carrying the serialized [`PathAccess`] rules.
pub const LANDLOCK_RULES_ENV: &str = "BOXLITE_LANDLOCK_RULES";
/// Environment variable carrying whether Landlock should allow TCP networking ("1"/"0").
pub const LANDLOCK_NETWORK_ENABLED_ENV: &str = "BOXLITE_LANDLOCK_NETWORK_ENABLED";
/// Gate for the `LD_PRELOAD` constructor. bwrap sets this to `"1"` via
/// `--setenv` ONLY in the shim's environment, so the constructor enforces in the
/// preloaded shim and is a no-op when the cdylib is otherwise loaded (e.g. the
/// rlib linked into the host, where this is never set).
pub const SEAL_MARKER_ENV: &str = "BOXLITE_LANDLOCK_SEAL";

/// A host path the sandboxed workload may access.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PathAccess {
    /// Host filesystem path.
    pub path: PathBuf,
    /// Whether write access is required.
    pub writable: bool,
}

/// Error building, serializing, or applying Landlock rules.
#[derive(Debug)]
pub struct LandlockError(pub String);

impl std::fmt::Display for LandlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for LandlockError {}

/// Serialize path access rules for the env handoff.
pub fn serialize_rules(paths: &[PathAccess]) -> Result<String, LandlockError> {
    serde_json::to_string(paths).map_err(|e| LandlockError(format!("serialize rules: {e}")))
}

/// Encode a network flag for [`LANDLOCK_NETWORK_ENABLED_ENV`].
pub fn network_enabled_env_value(network_enabled: bool) -> &'static str {
    if network_enabled {
        "1"
    } else {
        "0"
    }
}

/// Parse the network flag; only "1"/"0" accepted, else fail closed.
// Only consumed by the Linux ruleset path (and the tests); on other platforms
// the lib build sees it unused.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn network_enabled_from_env(value: Option<&str>) -> Result<bool, LandlockError> {
    match value {
        Some("1") => Ok(true),
        Some("0") => Ok(false),
        other => Err(LandlockError(format!(
            "missing/invalid {LANDLOCK_NETWORK_ENABLED_ENV} (got {other:?}); \
             refusing to apply Landlock with assumed networking"
        ))),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        network_enabled_from_env, LandlockError, PathAccess, LANDLOCK_NETWORK_ENABLED_ENV,
        LANDLOCK_RULES_ENV,
    };
    use landlock::{
        Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, RulesetError, ABI,
    };
    use std::os::fd::{IntoRawFd, RawFd};

    const TARGET_ABI: ABI = ABI::V5;
    const SYSTEM_READ_PATHS: &[&str] = &[
        "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc", "/dev",
    ];
    const SYSTEM_WRITE_PATHS: &[&str] = &["/tmp"];
    /// Device files bwrap dev-binds that the VMM needs read+write+ioctl on.
    /// `/dev` itself is only read+exec above, so these must be allowed
    /// explicitly or libkrun's `open("/dev/kvm", O_RDWR)` fails with EACCES.
    const DEVICE_WRITE_PATHS: &[&str] = &["/dev/kvm", "/dev/net/tun"];

    fn map_err(context: &str, err: RulesetError) -> LandlockError {
        LandlockError(format!("{context}: {err}"))
    }

    fn build_ruleset(
        paths: &[PathAccess],
        network_enabled: bool,
    ) -> Result<Option<RawFd>, LandlockError> {
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(TARGET_ABI))
            .map_err(|e| map_err("handle filesystem access", e))?;
        if !network_enabled {
            ruleset = ruleset
                .handle_access(AccessNet::from_all(TARGET_ABI))
                .map_err(|e| map_err("handle network access", e))?;
        }
        let mut created = ruleset
            .create()
            .map_err(|e| map_err("create ruleset", e))?
            .set_compatibility(CompatLevel::BestEffort);

        let read_access = AccessFs::from_read(TARGET_ABI) | AccessFs::Execute;
        for path in SYSTEM_READ_PATHS {
            if let Ok(fd) = PathFd::new(path) {
                created = created
                    .add_rule(PathBeneath::new(fd, read_access))
                    .map_err(|e| map_err(&format!("add rule for {path}"), e))?;
            }
        }
        let all_access = AccessFs::from_all(TARGET_ABI);
        for path in SYSTEM_WRITE_PATHS.iter().chain(DEVICE_WRITE_PATHS) {
            if let Ok(fd) = PathFd::new(path) {
                created = created
                    .add_rule(PathBeneath::new(fd, all_access))
                    .map_err(|e| map_err(&format!("add rule for {path}"), e))?;
            }
        }
        for pa in paths {
            let real = pa.path.canonicalize().unwrap_or_else(|_| pa.path.clone());
            let fd = match PathFd::new(&real) {
                Ok(fd) => fd,
                Err(_) => continue, // path doesn't exist in this view — skip
            };
            let access = if pa.writable {
                AccessFs::from_all(TARGET_ABI)
            } else {
                AccessFs::from_read(TARGET_ABI) | AccessFs::Execute
            };
            created = created
                .add_rule(PathBeneath::new(fd, access))
                .map_err(|e| map_err(&format!("add rule for {}", real.display()), e))?;
        }

        let owned: Option<std::os::fd::OwnedFd> = created.into();
        Ok(owned.map(|fd| fd.into_raw_fd()))
    }

    /// Apply the ruleset to the current thread. Returns errno (0 on success).
    ///
    /// # Safety
    /// `ruleset_fd` must be a valid Landlock ruleset fd; it is consumed (closed).
    unsafe fn restrict_self_raw(ruleset_fd: RawFd) -> i32 {
        let ret = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if ret != 0 {
            let errno = *libc::__errno_location();
            libc::close(ruleset_fd);
            return errno;
        }
        let ret = libc::syscall(
            libc::SYS_landlock_restrict_self,
            ruleset_fd as libc::c_long,
            0,
        );
        let errno = if ret != 0 {
            *libc::__errno_location()
        } else {
            0
        };
        libc::close(ruleset_fd);
        errno
    }

    /// Build the ruleset from the env handoff and restrict the current process.
    ///
    /// Missing rules env = Landlock not requested (no-op). Unsupported kernels
    /// degrade. A ruleset build error or restrict_self failure is returned so the
    /// caller (the preload constructor) fails closed instead of running un-sandboxed.
    pub fn apply_rules_from_env() -> Result<(), LandlockError> {
        let Ok(rules_json) = std::env::var(LANDLOCK_RULES_ENV) else {
            return Ok(());
        };
        let mut paths: Vec<PathAccess> = serde_json::from_str(&rules_json)
            .map_err(|e| LandlockError(format!("parse env rules: {e}")))?;

        // Allow the shim's own runtime directory (read+exec). The preload library
        // and the shim are co-located in the extracted runtime dir; whitelisting
        // it lets child exec's reload the preload library and the dynamic load of
        // the shim's sibling runtime libs (e.g. libkrunfw) succeed. When loaded as
        // the LD_PRELOAD constructor, `current_exe()` is the shim, so its parent is
        // exactly that directory.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                paths.push(PathAccess {
                    path: dir.to_path_buf(),
                    writable: false,
                });
            }
        }

        let network_enabled =
            network_enabled_from_env(std::env::var(LANDLOCK_NETWORK_ENABLED_ENV).ok().as_deref())?;

        match build_ruleset(&paths, network_enabled)? {
            Some(fd) => {
                let errno = unsafe { restrict_self_raw(fd) };
                if errno != 0 {
                    return Err(LandlockError(format!(
                        "restrict_self failed: {}",
                        std::io::Error::from_raw_os_error(errno)
                    )));
                }
            }
            None => { /* Landlock unsupported on this kernel — degrade gracefully */ }
        }
        Ok(())
    }

    /// `LD_PRELOAD` entry point: runs at library load, before the host binary's
    /// `main()`. When bwrap preloads this cdylib into the shim and sets
    /// [`SEAL_MARKER_ENV`], the loader runs this in the shim's process — after
    /// bwrap's mounts — and applies Landlock. Gated by the marker so the same
    /// code linked into the host (rlib) stays inert there.
    ///
    /// Fails closed: a build/restrict error aborts the process so the shim never
    /// runs un-sandboxed. The marker being unset is the only no-op path.
    #[used]
    #[link_section = ".init_array"]
    static SEAL_CTOR: extern "C" fn() = seal_on_load;

    extern "C" fn seal_on_load() {
        if std::env::var(super::SEAL_MARKER_ENV).as_deref() != Ok("1") {
            return;
        }
        if let Err(e) = apply_rules_from_env() {
            // async-signal context is irrelevant here (loader runs single-threaded
            // before main), but we must not let the shim proceed un-sandboxed.
            eprintln!("boxlite-landlock: preload enforcement failed: {e}");
            unsafe { libc::abort() };
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::apply_rules_from_env;

/// No-op on non-Linux (Landlock is Linux-only).
#[cfg(not(target_os = "linux"))]
pub fn apply_rules_from_env() -> Result<(), LandlockError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_flag_round_trips_and_fails_closed() {
        assert_eq!(network_enabled_env_value(true), "1");
        assert_eq!(network_enabled_env_value(false), "0");
        assert!(matches!(network_enabled_from_env(Some("1")), Ok(true)));
        assert!(matches!(network_enabled_from_env(Some("0")), Ok(false)));
        // Missing / garbage must fail closed, never default to enabled.
        assert!(network_enabled_from_env(None).is_err());
        assert!(network_enabled_from_env(Some("")).is_err());
        assert!(network_enabled_from_env(Some("true")).is_err());
    }

    #[test]
    fn rules_round_trip_through_serialization() {
        let rules = vec![
            PathAccess {
                path: "/work".into(),
                writable: true,
            },
            PathAccess {
                path: "/ro".into(),
                writable: false,
            },
        ];
        let json = serialize_rules(&rules).unwrap();
        let back: Vec<PathAccess> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back[0].writable);
        assert!(!back[1].writable);
    }
}
