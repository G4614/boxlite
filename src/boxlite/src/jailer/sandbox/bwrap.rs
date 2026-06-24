//! BwrapSandbox — Linux isolation via bubblewrap.
//!
//! Implements the [`Sandbox`] trait using bubblewrap (bwrap) for
//! namespace isolation, bind mounts, and environment sanitization.

use super::{Sandbox, SandboxContext};
use crate::jailer::{bwrap, cgroup};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::Path;
use std::process::Command;

/// Linux sandbox using bubblewrap for namespace isolation.
#[derive(Debug)]
pub struct BwrapSandbox;

impl BwrapSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BwrapSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for BwrapSandbox {
    fn is_available(&self) -> bool {
        bwrap::is_available()
    }

    fn setup(&self, ctx: &SandboxContext) -> BoxliteResult<()> {
        // Preflight: verify bwrap can create user namespaces before proceeding.
        if bwrap::is_available()
            && let Err(diagnostic) = bwrap::can_create_user_namespace()
        {
            return Err(BoxliteError::Config(format!(
                "Sandbox preflight failed: bwrap cannot create user namespaces.\n\n\
                 {diagnostic}\n\n\
                 To skip the sandbox (development only):\n  \
                   SecurityOptions::disabled()"
            )));
        }

        let cgroup_config = cgroup::CgroupConfig::from(ctx.resource_limits);

        match cgroup::setup_cgroup(ctx.id, &cgroup_config) {
            Ok(path) => {
                tracing::info!(id = %ctx.id, path = %path.display(), "Cgroup created");
            }
            Err(e) => {
                tracing::warn!(id = %ctx.id, error = %e,
                    "Cgroup setup failed (continuing without cgroup limits)");
            }
        }

        Ok(())
    }

    fn apply(&self, ctx: &SandboxContext, cmd: &mut Command) {
        let binary = cmd.get_program().to_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let mut bwrap_cmd = bwrap::BwrapCommand::new();

        // =====================================================================
        // Namespace and session isolation
        // =====================================================================
        bwrap_cmd
            .with_default_namespaces()
            .with_die_with_parent()
            .with_new_session();

        // =====================================================================
        // System directories (read-only)
        // =====================================================================
        bwrap_cmd
            .ro_bind_if_exists("/usr", "/usr")
            .ro_bind_if_exists("/lib", "/lib")
            .ro_bind_if_exists("/lib64", "/lib64")
            .ro_bind_if_exists("/bin", "/bin")
            .ro_bind_if_exists("/sbin", "/sbin");

        // =====================================================================
        // Devices and special mounts
        // =====================================================================
        bwrap_cmd
            .with_dev()
            .dev_bind_if_exists("/dev/kvm", "/dev/kvm")
            .dev_bind_if_exists("/dev/net/tun", "/dev/net/tun")
            .with_proc()
            .tmpfs("/tmp");

        // =====================================================================
        // Bind all pre-computed paths (system dirs + user volumes)
        // =====================================================================
        for pa in ctx.writable_paths() {
            bwrap_cmd.bind(&pa.path, &pa.path);
            tracing::debug!(path = %pa.path.display(), "bwrap: bind (rw)");
        }
        for pa in ctx.readonly_paths() {
            bwrap_cmd.ro_bind(&pa.path, &pa.path);
            tracing::debug!(path = %pa.path.display(), "bwrap: ro-bind");
        }

        // =====================================================================
        // Environment sanitization
        // =====================================================================
        bwrap_cmd
            .with_clearenv()
            .setenv("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .setenv("HOME", "/root");

        if let Some(binary_dir) = Path::new(&binary).parent() {
            bwrap_cmd.setenv("LD_LIBRARY_PATH", ld_library_path_for_binary(binary_dir));
            tracing::debug!(
                binary_dir = %binary_dir.display(),
                "bwrap: set LD_LIBRARY_PATH with command binary directory first"
            );
        }

        match crate::jailer::landlock::serialize_rules_for_env(&ctx.paths) {
            Ok(rules) => {
                bwrap_cmd
                    .setenv(crate::jailer::landlock::LANDLOCK_RULES_ENV, rules)
                    .setenv(
                        crate::jailer::landlock::LANDLOCK_NETWORK_ENABLED_ENV,
                        crate::jailer::landlock::network_enabled_env_value(ctx.network_enabled),
                    );
                tracing::debug!("bwrap: pass Landlock rules to shim");
            }
            Err(e) => {
                // Serializing Landlock rules is effectively infallible, but if it
                // ever fails we must NOT start the shim un-sandboxed. `apply()`
                // can't return an error (Sandbox trait), so inject a sentinel that
                // the shim's `apply_rules_from_env` rejects (invalid JSON) — the
                // shim then fails closed instead of silently skipping Landlock.
                tracing::error!(error = %e, "bwrap: failed to serialize Landlock rules; failing closed");
                bwrap_cmd.setenv(
                    crate::jailer::landlock::LANDLOCK_RULES_ENV,
                    "__boxlite_landlock_serialization_failed__",
                );
            }
        }

        // Preserve debugging environment variables
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            bwrap_cmd.setenv("RUST_LOG", rust_log);
        }
        if let Ok(rust_backtrace) = std::env::var("RUST_BACKTRACE") {
            bwrap_cmd.setenv("RUST_BACKTRACE", rust_backtrace);
        }

        bwrap_cmd.chdir("/");

        // Apply Landlock to the shim via an LD_PRELOAD constructor, NOT an exec
        // wrapper. bwrap itself must run un-Landlocked — a filesystem Landlock
        // domain denies every mount syscall, so applying it before bwrap finishes
        // would EPERM bwrap's own `mount(MS_SLAVE, "/")`. Instead bwrap sets
        // LD_PRELOAD (and the seal marker) ONLY in the child's env via `--setenv`.
        // When bwrap exec's the shim, the loader runs the preload library's
        // `.init_array` constructor, which applies Landlock in the shim's process
        // AFTER all of bwrap's mounts and BEFORE the shim's `main()`. No extra
        // exec hop, no shim source change.
        //
        // The library is co-located with the shim in the runtime bundle; the
        // ruleset whitelists the shim's own directory (read+exec) so child exec's
        // can reload it. If it's missing the runtime is corrupt: fail closed by
        // pointing bwrap at a path that can't exec, so the box never starts
        // un-sandboxed.
        let preload_lib = Path::new(&binary)
            .parent()
            .map(|dir| dir.join(crate::jailer::landlock::SEAL_PRELOAD_LIB));
        match preload_lib {
            Some(lib) if lib.exists() => {
                bwrap_cmd
                    .setenv("LD_PRELOAD", lib.to_string_lossy().into_owned())
                    .setenv(crate::jailer::landlock::SEAL_MARKER_ENV, "1");
                *cmd = bwrap_cmd.build(Path::new(&binary), &args);
            }
            other => {
                let missing = other
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<no parent dir>".to_string());
                tracing::error!(
                    lib = %missing,
                    "bwrap: Landlock preload library missing; failing closed (box will not start)"
                );
                *cmd = bwrap_cmd.build(
                    Path::new("/nonexistent/boxlite-landlock-preload-missing"),
                    &args,
                );
            }
        }

        // Add cgroup join as a pre_exec hook (async-signal-safe).
        if let Some(cgroup_procs) = cgroup::build_cgroup_procs_path(ctx.id) {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(move || {
                    let _ = cgroup::add_self_to_cgroup_raw(&cgroup_procs);
                    Ok(())
                });
            }
        }
    }

    fn name(&self) -> &'static str {
        "bwrap"
    }
}

fn ld_library_path_for_binary(binary_dir: &Path) -> String {
    let mut paths = vec![binary_dir.to_string_lossy().to_string()];
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH")
        && !existing.is_empty()
    {
        paths.push(existing);
    }
    paths.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ld_library_path_prioritizes_command_binary_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let binary_dir = tmp.path().join("boxes").join("box-id").join("bin");
        let path = ld_library_path_for_binary(&binary_dir);
        let expected = binary_dir.to_string_lossy().to_string();
        assert_eq!(path.split(':').next(), Some(expected.as_str()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_bwrap_passes_landlock_rules_to_shim_env() {
        if !BwrapSandbox::new().is_available() {
            eprintln!("bwrap not available, skipping");
            return;
        }

        let limits = Box::leak(Box::new(
            crate::runtime::advanced_options::ResourceLimits::default(),
        ));
        let ctx = SandboxContext {
            id: "test",
            paths: vec![crate::jailer::sandbox::PathAccess {
                path: std::path::PathBuf::from("/tmp"),
                writable: true,
            }],
            resource_limits: limits,
            network_enabled: false,
            sandbox_profile: None,
        };
        let mut cmd = Command::new("/bin/true");

        BwrapSandbox::new().apply(&ctx, &mut cmd);

        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|window| {
            window[0] == "--setenv" && window[1] == crate::jailer::landlock::LANDLOCK_RULES_ENV
        }));
        assert!(args.windows(3).any(|window| {
            window[0] == "--setenv"
                && window[1] == crate::jailer::landlock::LANDLOCK_NETWORK_ENABLED_ENV
                && window[2] == "0"
        }));
    }
}
