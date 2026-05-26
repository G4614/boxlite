//! Scenario trait + the run loop that drives `boxlite bench run`.
//!
//! A scenario owns its setup, execution, and teardown — the runner
//! just calls them in sequence per iteration. Per-scenario state lives
//! on the trait object, so the runner stays generic. The reason for
//! pulling this into its own module (rather than inlining in `mod.rs`)
//! is that future phases register scenarios via
//! [`scenarios::registry()`] without touching the dispatch code.

use super::host_info;
use super::report::{BenchReport, ReportMetadata, Sample};
use super::scenarios;
use super::stats;
use crate::cli::GlobalFlags;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::FutureExt;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::time::Instant;

/// One benchmark scenario. Phases register concrete implementations
/// in [`super::scenarios::registry`].
///
/// The trait carries `name()` for the report; the human description
/// lives on [`super::scenarios::ScenarioEntry`] (consumed by
/// `boxlite bench list`) so we don't duplicate it on the runtime
/// trait object.
#[async_trait]
pub trait Scenario: Send {
    /// Stable scenario name — used as the CLI argument and as the
    /// `scenario` field in the report. Must be unique across the
    /// registry; the runner deduplicates by exact match.
    fn name(&self) -> &str;

    /// Run a single iteration. Wall time is measured by the runner;
    /// scenarios fill in any additional named metrics (per-stage
    /// timings, byte counts, resource snapshots) into the returned
    /// map. Failures should surface as `Err` so the runner can stop
    /// before the data set is polluted with degenerate samples.
    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>>;

    /// Called once after every iteration completes (including warmup).
    /// Default: no-op. Use this to teardown side-state when a single
    /// `run_once` doesn't own its own resources (e.g., when sharing a
    /// box across iterations is desirable — though the typical pattern
    /// is one box per iteration, owned inside `run_once`).
    async fn after_iteration(&mut self, _ctx: &RunContext) -> Result<()> {
        Ok(())
    }

    /// Called once at the end of `run_scenario` — both on success and
    /// on iteration failure — so scenarios with cross-iteration state
    /// (persistent source boxes, named runtime entries, host
    /// subprocesses) get a deterministic cleanup hook instead of
    /// relying on `Drop`. Errors are surfaced as warnings but don't
    /// mask the iteration result.
    ///
    /// Scenarios that own all their state inside `run_once` can leave
    /// this defaulted; the no-op cost is zero.
    async fn teardown(&mut self, _ctx: &TeardownContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// Per-run context handed to every iteration. Holds the GlobalFlags
/// snapshot the user invoked with, so scenarios can build a runtime
/// the same way the rest of the CLI does.
#[allow(dead_code)]
pub struct RunContext<'a> {
    pub global: &'a GlobalFlags,
    /// 1-indexed iteration number (warmup + measured).
    pub iteration: usize,
    /// True if this iteration's sample will be dropped from aggregates.
    pub warmup: bool,
}

/// Context handed to [`Scenario::teardown`]. Carries the
/// `GlobalFlags` so cleanup can rebuild a runtime against the same
/// home + registry config as `run_once` did. No iteration counter
/// because teardown is one-shot.
pub struct TeardownContext<'a> {
    pub global: &'a GlobalFlags,
}

/// Implements `boxlite bench list`.
pub fn list_scenarios() -> Result<()> {
    let registry = scenarios::registry();
    if registry.is_empty() {
        println!("(no scenarios registered yet)");
        return Ok(());
    }
    println!("Available scenarios:");
    for entry in registry {
        println!("  {:<28} {}", entry.name, entry.description);
    }
    Ok(())
}

/// Implements `boxlite bench run <scenario> --runs N --warmup M`.
pub async fn run_scenario(args: super::RunArgs, global: &GlobalFlags) -> Result<()> {
    if args.runs == 0 {
        anyhow::bail!("--runs must be ≥ 1");
    }
    if args.warmup >= args.runs {
        anyhow::bail!(
            "--warmup ({}) must be < --runs ({}); otherwise every \
             sample is discarded",
            args.warmup,
            args.runs
        );
    }

    let mut scenario = scenarios::build_by_name(&args.scenario).with_context(|| {
        format!(
            "unknown scenario: {:?}. Use `boxlite bench list` to see options.",
            args.scenario
        )
    })?;

    let metadata = ReportMetadata {
        started_at: chrono::Utc::now().to_rfc3339(),
        label: args.label.clone(),
        git_commit: read_git_commit(),
        boxlite_version: env!("CARGO_PKG_VERSION").to_string(),
        host: host_info::HostInfo::snapshot(),
    };

    let scenario_name = scenario.name().to_string();
    let mut report = BenchReport::new(scenario_name.clone(), metadata);

    // Per-metric value arrays, populated as we go and aggregated at
    // the end. Using BTreeMap keeps the metric ordering deterministic
    // in the report (JSON object iteration is unordered otherwise).
    let mut metric_samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();

    eprintln!(
        "🧪 bench: scenario={} runs={} warmup={}",
        scenario_name, args.runs, args.warmup,
    );

    // Iteration loop wrapped so the teardown call runs on BOTH paths.
    // Errors from iterations are stashed and re-raised AFTER teardown
    // gets its chance — so a scenario with persistent state (named
    // boxes, host child processes) always gets the cleanup hook,
    // regardless of which iteration blew up.
    //
    // The iteration future also races against SIGINT/SIGTERM so a
    // sweep's `timeout 600` (which sends SIGTERM) lets us run
    // teardown before exiting, instead of dropping VMs + temp dirs
    // on the floor. tokio::select! cancels the losing future via
    // drop — which runs every in-scope Drop including BoxGuard's
    // force-remove — and we then call teardown for cross-iteration
    // state explicitly.
    let iter_fut = async {
        for i in 1..=args.runs {
            let warmup = i <= args.warmup;
            let ctx = RunContext {
                global,
                iteration: i,
                warmup,
            };

            let start = Instant::now();
            let metrics = scenario.run_once(&ctx).await.with_context(|| {
                format!(
                    "iteration {i}/{} failed in scenario {scenario_name}",
                    args.runs
                )
            })?;
            let wall_ms = start.elapsed().as_secs_f64() * 1000.0;

            scenario.after_iteration(&ctx).await?;

            let sample = Sample {
                iteration: i,
                warmup,
                wall_ms,
                metrics: metrics.clone(),
            };
            if !warmup {
                // wall_ms is always part of the aggregates so every
                // scenario gets at least one comparable headline number.
                metric_samples
                    .entry("wall_ms".to_string())
                    .or_default()
                    .push(wall_ms);
                for (k, v) in metrics {
                    metric_samples.entry(k).or_default().push(v);
                }
            }
            eprintln!(
                "  iter {:>3}{} wall={:.1}ms",
                i,
                if warmup { " (warmup)" } else { "         " },
                wall_ms
            );
            report.samples.push(sample);
        }
        Ok::<(), anyhow::Error>(())
    };

    // Wrap iter_fut in catch_unwind so a `panic!` inside a scenario
    // (unwrap, indexing OOB, etc.) doesn't bypass the teardown hook
    // and bring down the binary mid-cleanup. AssertUnwindSafe is OK
    // here because after a panic we only invoke teardown — which
    // either re-uses the scenario's stashed state or no-ops — and
    // never read partially-mutated invariants from the scenario.
    let safe_iter = AssertUnwindSafe(iter_fut).catch_unwind();

    let iter_result: Result<()> = tokio::select! {
        biased;
        res = safe_iter => match res {
            Ok(inner) => inner,
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<unknown panic payload>".into());
                Err(anyhow::anyhow!(
                    "scenario {scenario_name:?} panicked: {msg}"
                ))
            }
        },
        sig = wait_for_signal() => Err(anyhow::anyhow!(
            "bench {scenario_name:?} interrupted by {sig}; running teardown before exit"
        )),
    };

    // Best-effort teardown: warnings only, don't mask the real error.
    let teardown_ctx = TeardownContext { global };
    if let Err(e) = scenario.teardown(&teardown_ctx).await {
        eprintln!(
            "⚠️  teardown for scenario {scenario_name:?} failed (resources may have leaked): {e:#}"
        );
    }

    // Last-ditch descendant cleanup on the unhappy path. When
    // SIGTERM cancels an in-flight `rt.create` / `box.start`, the
    // BoxGuard's Drop runs `rt.remove`, but libkrun VM and
    // boxlite-shim children spawned during VMM setup don't always
    // get reaped before our process exits — they become orphans of
    // init. Walk our descendant tree and SIGKILL any leftover. On
    // the success path no descendants should remain, so the pkill
    // is a no-op.
    if iter_result.is_err() {
        kill_descendants_of_self();
    }

    // Surface any iteration error AFTER teardown so the caller sees
    // the iteration failure (which is what they care about) instead
    // of the teardown warning.
    iter_result?;

    report.warmup_count = args.warmup;
    report.sample_count = args.runs - args.warmup;
    for (name, samples) in metric_samples {
        if let Some(agg) = stats::aggregate(&name, &samples) {
            report.aggregates.push(agg);
        }
    }

    write_report(&report, args.out.as_deref())?;
    print_summary(&report);
    Ok(())
}

/// Walk our descendant PID tree via `/proc/<pid>/task/<pid>/children`
/// and SIGKILL any `libkrun-VM` or `boxlite-shim` we find. Called
/// only on the failure / interrupted-by-signal path as a last-ditch
/// reap; on the happy path scenarios have already torn their boxes
/// down and this loop finds nothing to do.
fn kill_descendants_of_self() {
    use std::collections::VecDeque;
    use std::fs;

    fn children_of(pid: i32) -> Vec<i32> {
        let path = format!("/proc/{pid}/task/{pid}/children");
        fs::read_to_string(&path)
            .ok()
            .map(|s| {
                s.split_ascii_whitespace()
                    .filter_map(|t| t.parse::<i32>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn comm_of(pid: i32) -> Option<String> {
        let path = format!("/proc/{pid}/comm");
        fs::read_to_string(&path).ok().map(|s| s.trim().to_string())
    }

    let me = std::process::id() as i32;
    let mut queue: VecDeque<i32> = children_of(me).into_iter().collect();
    let mut killed = 0usize;
    while let Some(pid) = queue.pop_front() {
        for c in children_of(pid) {
            queue.push_back(c);
        }
        if let Some(comm) = comm_of(pid)
            && (comm == "libkrun VM" || comm == "boxlite-shim")
        {
            // SIGKILL — graceful was the runtime's job; we're past that.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
            killed += 1;
        }
    }
    if killed > 0 {
        eprintln!("⚠️  reaped {killed} orphan VM/shim descendant(s) after interrupted run");
    }
}

/// Wait for SIGINT or SIGTERM. Used by [`run_scenario`] to race
/// against the iteration loop so external timeouts (e.g., a sweep
/// wrapper's `timeout 600`) trigger teardown instead of dropping
/// in-flight state on the floor.
async fn wait_for_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    // If signal-handler install ever fails we fall back to never
    // resolving (the iteration loop wins the select). Better than
    // bailing the whole bench on a broken environment.
    let term = signal(SignalKind::terminate()).ok();
    let int = signal(SignalKind::interrupt()).ok();
    match (term, int) {
        (Some(mut t), Some(mut i)) => tokio::select! {
            _ = t.recv() => "SIGTERM",
            _ = i.recv() => "SIGINT",
        },
        (Some(mut t), None) => {
            let _ = t.recv().await;
            "SIGTERM"
        }
        (None, Some(mut i)) => {
            let _ = i.recv().await;
            "SIGINT"
        }
        (None, None) => std::future::pending::<&str>().await,
    }
}

fn read_git_commit() -> String {
    // git is best-effort; in a worktree without git, or in a stripped
    // CI environment, we still want a report — just with `"unknown"`
    // as the marker.
    use std::process::Command;
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn write_report(report: &BenchReport, out: Option<&std::path::Path>) -> Result<()> {
    let json = serde_json::to_string_pretty(report).context("serialize bench report")?;
    match out {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir -p {}", parent.display()))?;
            }
            std::fs::write(path, json.as_bytes())
                .with_context(|| format!("write report to {}", path.display()))?;
            eprintln!("📄 report written to {}", path.display());
        }
        None => {
            println!("{json}");
        }
    }
    Ok(())
}

fn print_summary(report: &BenchReport) {
    eprintln!("\n📊 aggregates (n={}):", report.sample_count);
    for agg in &report.aggregates {
        eprintln!(
            "  {:<32} p50={:>8.1} p90={:>8.1} p99={:>8.1} ({})",
            agg.name, agg.p50, agg.p90, agg.p99, agg.unit
        );
    }
}
