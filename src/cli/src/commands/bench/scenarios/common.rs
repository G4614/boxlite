//! Shared helpers for bench scenarios.
//!
//! Anything that more than one scenario needs (per-iteration runtime
//! construction, the default alpine `BoxOptions`, the panic-safe
//! teardown guard) lives here so individual scenarios stay focused on
//! the metric they're producing rather than re-implementing the
//! box-lifecycle plumbing.

use crate::cli::GlobalFlags;
use anyhow::{Context, Result};
use boxlite::runtime::options::RootfsSpec;
use boxlite::{BoxOptions, BoxliteRuntime};
use std::path::PathBuf;

/// Image used by every default-config scenario.
pub const DEFAULT_IMAGE: &str = "alpine:latest";

/// Build a runtime rooted at `home`, honouring whatever the user
/// passed through `GlobalFlags` (registry list, etc.). We can't reuse
/// `global.create_runtime()` directly because we override the home
/// per iteration; mutating the resolved options is the closest thing
/// to "same options, different home" without re-doing the registry
/// resolution by hand.
pub fn build_runtime(global: &GlobalFlags, home: PathBuf) -> Result<BoxliteRuntime> {
    let mut opts = global
        .resolve_runtime_options()
        .context("resolve runtime options for bench iteration")?;
    opts.home_dir = home;
    global
        .create_runtime_with_options(opts)
        .context("instantiate per-iteration runtime")
}

/// Default-config `BoxOptions` for `alpine:latest`. `auto_remove=true`
/// is explicit so a future change to the default doesn't silently flip
/// the bench teardown semantic.
pub fn alpine_options() -> BoxOptions {
    BoxOptions {
        rootfs: RootfsSpec::Image(DEFAULT_IMAGE.into()),
        auto_remove: true,
        ..Default::default()
    }
}

/// RAII guard that force-removes the box if its scope ends abnormally
/// (panic, `?`-propagated error). On the happy path the scenario calls
/// `disarm()` after a successful `stop` so the guard's drop is a
/// no-op. Without this, a panic inside any post-start step would leak
/// a running libkrun VM holding gvproxy host ports across the rest of
/// the bench run.
pub struct BoxGuard<'a> {
    rt: &'a BoxliteRuntime,
    box_id: String,
    armed: bool,
}

impl<'a> BoxGuard<'a> {
    pub fn new(rt: &'a BoxliteRuntime, box_id: String) -> Self {
        Self {
            rt,
            box_id,
            armed: true,
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BoxGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The bench harness uses tokio's multi-thread runtime
        // (see `boxlite-cli`'s `tokio::main`), so `block_in_place` is
        // available and lets us await `rt.remove` from this sync Drop.
        let id = self.box_id.clone();
        let rt = self.rt.clone();
        tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(async move {
                rt.remove(&id, true).await.ok();
            });
        });
    }
}

/// Spawned `boxlite serve` child + the URL to talk to it. Kept in
/// one struct so REST/WS scenarios don't each re-write the
/// probe-port → spawn → poll-ready dance, and so `kill_on_drop`
/// fires once the struct goes out of scope.
pub struct ServeChild {
    pub url: String,
    pub _home: tempfile::TempDir,
    pub child: tokio::process::Child,
}

impl ServeChild {
    /// Probe a free port, spawn `boxlite serve` against a fresh
    /// `TempDir` home, and block until `/v1/config` answers 200.
    /// `boxlite-cli` re-execs its own binary, so the parent's build
    /// is what the child runs (any code change here goes live).
    ///
    /// `extra_registries` are forwarded as `--registry` flags before
    /// the `serve` subcommand so the child can pull images through
    /// the same mirrors the parent was given (otherwise the child
    /// hits docker.io directly and is rate-limited fast under sweep
    /// load).
    pub async fn spawn(home_label: &str, extra_registries: &[String]) -> Result<Self> {
        use std::net::TcpListener as StdTcpListener;
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        use tokio::process::Command;

        let port = {
            let probe = StdTcpListener::bind(("127.0.0.1", 0))
                .with_context(|| format!("probe free port for {home_label}"))?;
            probe
                .local_addr()
                .map(|a| a.port())
                .context("read probed addr")?
        };
        let home =
            tempfile::TempDir::new().with_context(|| format!("mkdir {home_label} child home"))?;
        let bin = std::env::current_exe().context("locate current boxlite binary")?;
        let mut cmd = Command::new(&bin);
        cmd.arg("--home").arg(home.path());
        for r in extra_registries {
            cmd.arg("--registry").arg(r);
        }
        let child = cmd
            .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawn boxlite serve child")?;

        let url = format!("http://127.0.0.1:{port}");
        let probe_url = format!("{url}/v1/config");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .context("build reqwest client (serve ready probe)")?;
        let ready_at = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() > ready_at {
                anyhow::bail!("boxlite serve did not answer /v1/config within 15s on {url}");
            }
            if let Ok(r) = client.get(&probe_url).send().await
                && r.status().is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(Self {
            url,
            _home: home,
            child,
        })
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        // `kill_on_drop(true)` on the Command builder does most of the
        // work, but explicit start_kill makes the intent obvious and
        // surfaces faster than waiting for tokio's destructor walk.
        let _ = self.child.start_kill();
    }
}
