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
