//! Throughput scenarios: how many bytes per second / requests per
//! second this boxlite version moves.
//!
//! Phase 4 ships ONE scenario, `throughput-image-pull`. The
//! in-box workload axes (gvproxy net via iperf3 inside the guest,
//! qcow2 I/O via fio inside the guest) and the REST-server RPS
//! axis (`boxlite serve` hammered with reqwest) are deliberately
//! deferred — both require a non-trivial workload runner (vendored
//! binaries inside a test image, or a separate server-lifecycle
//! harness) and the headline image-pull number is enough to gate
//! the first regression class anyone will hit on a slow mirror.
//!
//! `throughput-image-pull` — measures end-to-end pull rate for
//! alpine:latest:
//!   * fresh `--home` per iteration → guarantees a real pull, no
//!     cache short-circuit;
//!   * wall-clocks `ImageHandle::pull("alpine:latest")` (the
//!     pull-only path; we explicitly avoid `BoxliteRuntime::create`
//!     because the latter folds in disk format + cow build cost);
//!   * counts the pulled bytes from the layer tarball files
//!     materialized in the home, so MB/s reflects what actually
//!     hit disk.
//!
//! Metrics produced:
//!   * `pull_wall_ms` — pull duration in ms (also `wall_ms` from
//!     the runner, since the iteration body is just the pull).
//!   * `pulled_bytes` — sum of layer tarball sizes on disk.
//!   * `pull_mb_per_sec` — `(pulled_bytes / 1_048_576) /
//!     (pull_wall_ms / 1000)`. The headline.
//!
//! Cold-cache only. There's no warm-cache variant — once the image
//! is pulled, a second pull is essentially a no-op (boxlite's
//! manifest cache skips unchanged blobs), and the resulting "1 ms
//! to pull alpine" number would be meaningless.

use super::super::runner::{RunContext, Scenario};
use super::common::build_runtime;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Instant;
use tempfile::TempDir;

const IMAGE: &str = "alpine:latest";

pub struct ImagePull {
    /// Hold the previous iteration's home until the next one starts
    /// so the temp-dir teardown cost (which can take a sec on a big
    /// layer set) doesn't pollute the next iteration's `wall_ms`.
    previous_home: Option<TempDir>,
}

impl ImagePull {
    pub fn new() -> Self {
        Self {
            previous_home: None,
        }
    }
}

#[async_trait]
impl Scenario for ImagePull {
    fn name(&self) -> &str {
        "throughput-image-pull"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        let tmp = TempDir::new().context("mkdir image-pull home")?;
        let home_path = tmp.path().to_path_buf();
        let rt = build_runtime(ctx.global, home_path.clone())?;

        let images = rt
            .images()
            .context("BoxliteRuntime::images() — no local image manager?")?;

        let start = Instant::now();
        let image_object = images
            .pull(IMAGE)
            .await
            .with_context(|| format!("pull {IMAGE}"))?;
        let pull_wall_ms = start.elapsed().as_secs_f64() * 1000.0;

        // Sum the on-disk size of every layer tarball the pull
        // materialized. Tarballs are dense (not sparse like qcow2), so
        // `metadata().len()` is what landed on disk. A failed stat on
        // any single layer is silently treated as 0 — if every layer
        // failed to stat the surrounding `pull()` would already have
        // errored. Inline because `ImageObject` is `crate`-private in
        // `boxlite` and we can't name it from here.
        let pulled_bytes: u64 = image_object
            .layer_tarballs()
            .into_iter()
            .filter_map(|path| std::fs::metadata(&path).ok().map(|m| m.len()))
            .sum();

        let mut metrics = BTreeMap::new();
        metrics.insert("pull_wall_ms".into(), pull_wall_ms);
        metrics.insert("pulled_bytes".into(), pulled_bytes as f64);
        if pull_wall_ms > 0.0 {
            let mib = pulled_bytes as f64 / (1024.0 * 1024.0);
            let mb_per_sec = mib / (pull_wall_ms / 1000.0);
            // No `_ms`/`_bytes`/`_pct`/`_count` suffix → the report's
            // unit hint resolves to "?"; `compare` shows the literal
            // value. The metric name itself documents the unit.
            metrics.insert("pull_mb_per_sec".into(), mb_per_sec);
        }

        self.previous_home = Some(tmp);
        Ok(metrics)
    }
}
