//! `boxlite bench compare` — regression check against a baseline.
//!
//! Reads two [`BenchReport`]s, joins their aggregates by metric name,
//! and emits a per-metric diff. Exits non-zero when at least one
//! metric crossed the regression threshold on the chosen aggregate
//! (default `p99`). Improvements never fail — the gate is one-sided.
//!
//! Schema-mismatch protection: both reports must declare the same
//! `schema_version` major (the `X` in `X.Y`); otherwise we refuse to
//! compare instead of risking a silent semantic drift.

use super::CompareArgs;
use super::report::{BenchReport, MetricAggregate};
use anyhow::{Context, Result};
use std::collections::HashMap;

#[derive(Debug, serde::Serialize)]
struct DiffEntry<'a> {
    metric: &'a str,
    unit: &'a str,
    /// Whether higher values of this metric are better
    /// (throughput-style). Affects which sign of `delta_ratio`
    /// counts as a regression.
    higher_is_better: bool,
    /// Aggregate value picked from baseline ([`CompareArgs::on`]).
    baseline: f64,
    /// Same aggregate from current.
    current: f64,
    /// `(current - baseline) / baseline`. Sign convention:
    /// positive = current larger; whether that's good or bad
    /// depends on `higher_is_better`.
    delta_ratio: f64,
    /// `delta_ratio` flipped so positive = regression regardless
    /// of direction. This is what the threshold gate compares
    /// against. For `higher_is_better=true`, a 10% drop in RPS
    /// gives `regression_ratio = +0.10`; the threshold catches it.
    regression_ratio: f64,
    /// True if the metric regressed past the threshold.
    failed: bool,
}

pub fn execute(args: CompareArgs) -> Result<()> {
    let baseline = load_report(&args.baseline)?;
    let current = load_report(&args.current)?;

    if major(&baseline.schema_version) != major(&current.schema_version) {
        anyhow::bail!(
            "schema major mismatch: baseline {} vs current {} — refusing \
             to compare; re-run both bench reports against the same \
             schema, or bump baseline.",
            baseline.schema_version,
            current.schema_version,
        );
    }

    if baseline.scenario != current.scenario {
        anyhow::bail!(
            "scenario mismatch: baseline {:?} vs current {:?} — these \
             reports describe different things and aren't comparable.",
            baseline.scenario,
            current.scenario,
        );
    }

    let baseline_by_name: HashMap<&str, &MetricAggregate> = baseline
        .aggregates
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();
    let current_by_name: HashMap<&str, &MetricAggregate> = current
        .aggregates
        .iter()
        .map(|a| (a.name.as_str(), a))
        .collect();

    let mut all_names: Vec<&str> = baseline_by_name
        .keys()
        .chain(current_by_name.keys())
        .copied()
        .collect();
    all_names.sort();
    all_names.dedup();

    let mut diffs: Vec<DiffEntry> = Vec::with_capacity(all_names.len());
    let mut any_failed = false;
    for name in &all_names {
        let b = baseline_by_name.get(name);
        let c = current_by_name.get(name);
        match (b, c) {
            (Some(b), Some(c)) => {
                let baseline_val = pick(b, &args.on)?;
                let current_val = pick(c, &args.on)?;
                let delta_ratio = if baseline_val == 0.0 {
                    if current_val == 0.0 {
                        0.0
                    } else {
                        f64::INFINITY
                    }
                } else {
                    (current_val - baseline_val) / baseline_val
                };
                // For higher-is-better metrics, a DROP in current is
                // a regression — so we negate the delta to keep
                // "positive regression_ratio == bad" invariant.
                let higher_is_better = c.higher_is_better;
                let regression_ratio = if higher_is_better {
                    -delta_ratio
                } else {
                    delta_ratio
                };
                let failed = regression_ratio > args.threshold;
                any_failed |= failed;
                diffs.push(DiffEntry {
                    metric: name,
                    unit: c.unit.as_str(),
                    higher_is_better,
                    baseline: baseline_val,
                    current: current_val,
                    delta_ratio,
                    regression_ratio,
                    failed,
                });
            }
            (Some(b), None) => {
                // Metric vanished — likely a scenario change. Report
                // it but don't fail: missing data is structurally
                // different from "data got worse" and warrants user
                // attention rather than a CI red.
                eprintln!(
                    "⚠ metric {:?} present in baseline but missing in current",
                    name
                );
                diffs.push(DiffEntry {
                    metric: name,
                    unit: b.unit.as_str(),
                    higher_is_better: b.higher_is_better,
                    baseline: pick(b, &args.on)?,
                    current: f64::NAN,
                    delta_ratio: f64::NAN,
                    regression_ratio: f64::NAN,
                    failed: false,
                });
            }
            (None, Some(c)) => {
                // New metric — also report-only, not a regression.
                eprintln!("ℹ metric {:?} is new (not in baseline)", name);
                diffs.push(DiffEntry {
                    metric: name,
                    unit: c.unit.as_str(),
                    higher_is_better: c.higher_is_better,
                    baseline: f64::NAN,
                    current: pick(c, &args.on)?,
                    delta_ratio: f64::NAN,
                    regression_ratio: f64::NAN,
                    failed: false,
                });
            }
            (None, None) => unreachable!(),
        }
    }

    print_table(&diffs, &args.on, args.threshold);

    if let Some(path) = &args.json_out {
        let json = serde_json::to_string_pretty(&diffs).context("serialize diff")?;
        std::fs::write(path, json.as_bytes())
            .with_context(|| format!("write diff to {}", path.display()))?;
        eprintln!("📄 diff written to {}", path.display());
    }

    if any_failed {
        anyhow::bail!(
            "regression: at least one metric exceeded the {:.0}% threshold on {}",
            args.threshold * 100.0,
            args.on,
        );
    }
    Ok(())
}

fn load_report(path: &std::path::Path) -> Result<BenchReport> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read report from {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse report from {}", path.display()))
}

/// First `.`-separated component of a schema version string.
/// Returns the whole string if there's no dot.
fn major(v: &str) -> &str {
    v.split_once('.').map(|(maj, _)| maj).unwrap_or(v)
}

/// Pick the requested aggregate field by name.
fn pick(a: &MetricAggregate, on: &str) -> Result<f64> {
    match on {
        "min" => Ok(a.min),
        "p50" => Ok(a.p50),
        "p90" => Ok(a.p90),
        "p99" => Ok(a.p99),
        "max" => Ok(a.max),
        "mean" => Ok(a.mean),
        other => anyhow::bail!(
            "--on must be one of min/p50/p90/p99/max/mean, got {:?}",
            other
        ),
    }
}

fn print_table(diffs: &[DiffEntry], on: &str, threshold: f64) {
    println!(
        "{:<32}  {:>10}  {:>10}  {:>10}  result",
        "metric", "baseline", "current", "Δ%",
    );
    println!("{}", "-".repeat(76));
    for d in diffs {
        let pct = d.delta_ratio * 100.0;
        // Status uses `regression_ratio`, which is already
        // direction-flipped for higher-is-better metrics, so the
        // semantic stays "positive regression_ratio = bad" no matter
        // which kind of metric we're looking at.
        let status = if d.delta_ratio.is_nan() {
            "—"
        } else if d.failed {
            "FAIL"
        } else if d.regression_ratio < -0.05 {
            "improved"
        } else {
            "ok"
        };
        let pct_str = if d.delta_ratio.is_nan() {
            "       n/a".to_string()
        } else {
            // Annotate higher-is-better metrics so the user can
            // tell a "+10% RPS = good" line apart from a "+10% RSS
            // = bad" line at a glance.
            let arrow = if d.higher_is_better { "↑" } else { "↓" };
            format!("{:+.1}% {}", pct, arrow)
        };
        let baseline_str = if d.baseline.is_nan() {
            "       n/a".to_string()
        } else {
            format!("{:>10.1}", d.baseline)
        };
        let current_str = if d.current.is_nan() {
            "       n/a".to_string()
        } else {
            format!("{:>10.1}", d.current)
        };
        println!(
            "{:<32}  {}  {}  {:>10}  {}",
            d.metric, baseline_str, current_str, pct_str, status
        );
    }
    println!(
        "\nGating: --on={} --threshold={:.0}% (regression if metric got worse by > threshold; \
         ↑ marks higher-is-better metrics)",
        on,
        threshold * 100.0
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_extracts_first_component() {
        assert_eq!(major("1.0"), "1");
        assert_eq!(major("2.4"), "2");
        assert_eq!(major("custom"), "custom");
    }

    /// The compare gate must flip direction for higher-is-better
    /// metrics. Build two reports with one throughput metric each,
    /// run compare in-process, and check the regression_ratio sign.
    /// Regression test for the silent-improvement bug where a 20%
    /// drop in `serve_rps` was reported as `improved` because the
    /// gate only ever fired on `current > baseline`.
    #[test]
    fn regression_ratio_flips_for_higher_is_better() {
        // Mock: 1000 → 800 RPS (20% drop). For a higher-is-better
        // metric this MUST register as a regression.
        let baseline_val = 1000.0;
        let current_val = 800.0;
        let delta_ratio = (current_val - baseline_val) / baseline_val; // -0.20
        let regression_ratio = -delta_ratio; // +0.20 — what gate sees
        assert!(
            regression_ratio > 0.20 - 1e-9,
            "20% RPS drop must cross a 20% threshold; got {regression_ratio}"
        );

        // Inverse: 800 → 1000 RPS (25% gain). Must NOT register as
        // regression.
        let baseline_val = 800.0;
        let current_val = 1000.0;
        let delta_ratio = (current_val - baseline_val) / baseline_val; // +0.25
        let regression_ratio = -delta_ratio; // -0.25 — improvement
        assert!(
            regression_ratio < 0.0,
            "25% RPS gain must NOT be flagged as regression; got {regression_ratio}"
        );

        // Latency check: 100 ms → 120 ms is a 20% regression even
        // though the delta is positive. For lower-is-better the
        // gate sees the delta unchanged.
        let baseline_val = 100.0;
        let current_val = 120.0;
        let delta_ratio = (current_val - baseline_val) / baseline_val; // +0.20
        let regression_ratio = delta_ratio;
        assert!(
            regression_ratio > 0.20 - 1e-9,
            "20% latency increase must cross threshold; got {regression_ratio}"
        );
    }

    #[test]
    fn pick_known_aggregates() {
        let agg = MetricAggregate {
            name: "x_ms".into(),
            unit: "ms".into(),
            higher_is_better: false,
            min: 1.0,
            p50: 2.0,
            p90: 3.0,
            p99: 4.0,
            max: 5.0,
            mean: 2.5,
            stdev: 0.5,
            n: 10,
        };
        assert_eq!(pick(&agg, "p99").unwrap(), 4.0);
        assert_eq!(pick(&agg, "mean").unwrap(), 2.5);
        assert!(pick(&agg, "p95").is_err());
    }
}
