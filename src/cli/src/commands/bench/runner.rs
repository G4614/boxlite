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
use std::collections::BTreeMap;
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
}

/// Per-run context handed to every iteration. Holds the GlobalFlags
/// snapshot the user invoked with, so scenarios can build a runtime
/// the same way the rest of the CLI does.
///
/// Fields are unused inside Phase 0 (the registry is empty); the
/// `dead_code` allow keeps `-D warnings` clean until Phase 1+ scenarios
/// start consuming them.
#[allow(dead_code)]
pub struct RunContext<'a> {
    pub global: &'a GlobalFlags,
    /// 1-indexed iteration number (warmup + measured).
    pub iteration: usize,
    /// True if this iteration's sample will be dropped from aggregates.
    pub warmup: bool,
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
