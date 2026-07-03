//! BwrapSandbox — Linux isolation via bubblewrap.
//!
//! Implements the [`Sandbox`] trait using bubblewrap (bwrap) for
//! namespace isolation, bind mounts, and environment sanitization.

use super::{Sandbox, SandboxContext};
use crate::jailer::{bwrap, cgroup};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
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

        // gvproxy runs inside this sandbox and resolves allow-listed hostnames
        // via the Go net resolver, which reads /etc/resolv.conf. Without it the
        // resolver falls back to [::1]:53 (refused) and every allowed host maps
        // to the 0.0.0.0 sinkhole, so guest egress fails with ECONNREFUSED.
        // ro_bind follows the source symlink (…→ systemd stub-resolv.conf); the
        // shared host netns keeps 127.0.0.53:53 reachable inside the sandbox.
        bwrap_cmd.ro_bind_if_exists("/etc/resolv.conf", "/etc/resolv.conf");

        // gvproxy's MITM upstream leg re-originates TLS to the real upstream and
        // must verify its certificate against the system trust store. Go's x509
        // loader reads /etc/ssl/certs/ca-certificates.crt (Debian/Ubuntu). Without
        // it, upstream verification fails with "x509: certificate signed by unknown
        // authority" and every substituted request returns 502.
        bwrap_cmd.ro_bind_if_exists("/etc/ssl/certs", "/etc/ssl/certs");
        bwrap_cmd.ro_bind_if_exists("/etc/pki", "/etc/pki");

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

        // Preserve debugging environment variables
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            bwrap_cmd.setenv("RUST_LOG", rust_log);
        }
        if let Ok(rust_backtrace) = std::env::var("RUST_BACKTRACE") {
            bwrap_cmd.setenv("RUST_BACKTRACE", rust_backtrace);
        }

        bwrap_cmd.chdir("/");

        // Replace the command with bwrap-wrapped version.
        *cmd = bwrap_cmd.build(std::path::Path::new(&binary), &args);

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
