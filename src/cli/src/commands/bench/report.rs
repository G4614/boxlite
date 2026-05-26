//! Versioned JSON output schema for `boxlite bench run`.
//!
//! The on-wire schema is stable: anything consuming a bench report —
//! the `compare` subcommand, CI dashboards, baseline files committed to
//! the repo — relies on these struct shapes. Bump
//! [`BenchReport::schema_version`] whenever a field changes meaning or
//! a metric is renamed; downstream consumers can then refuse mismatched
//! reports loudly instead of silently mis-interpreting them.
//!
//! The split between [`BenchReport`] (overall envelope), per-run
//! [`Sample`]s, and per-metric [`Aggregate`]s lets the comparator work
//! on the aggregates without re-reading the raw samples, while leaving
//! the raw samples in the JSON for anyone who wants to do their own
//! statistics later.

use serde::{Deserialize, Serialize};

/// Schema version for [`BenchReport`]. Bump on any breaking change to
/// the field shape; the comparator refuses reports from a different
/// major schema and downgrades to a warning on a newer minor.
pub const SCHEMA_VERSION: &str = "1.0";

/// Top-level bench report. One produced per `boxlite bench run` call.
#[derive(Debug, Serialize, Deserialize)]
pub struct BenchReport {
    /// On-wire schema version (see [`SCHEMA_VERSION`]).
    pub schema_version: String,

    /// Scenario name (must match a registered scenario).
    pub scenario: String,

    /// Host + boxlite snapshot at run time. Lets `compare` flag
    /// apples-to-oranges comparisons (e.g., different kernel, different
    /// CPU count, very different git commit) instead of silently
    /// blaming a regression on the code.
    pub metadata: ReportMetadata,

    /// Effective number of samples that contributed to aggregates
    /// (i.e., `runs - warmup`). Stored so the diff tool can warn when
    /// the sample count is too low for the stated percentile to be
    /// statistically meaningful.
    pub sample_count: usize,

    /// Number of warmup runs that were collected but excluded from
    /// aggregation.
    pub warmup_count: usize,

    /// Raw per-run samples in collection order. Kept so consumers can
    /// roll their own statistics without re-running the bench.
    pub samples: Vec<Sample>,

    /// Per-metric aggregates over the non-warmup samples.
    pub aggregates: Vec<MetricAggregate>,
}

/// One iteration of a scenario. The set of `metrics` keys depends on
/// the scenario; the comparator joins by `metric.name`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Sample {
    /// 1-indexed iteration number within the run (warmup or measured).
    pub iteration: usize,

    /// True if this sample was dropped from aggregation (warmup).
    pub warmup: bool,

    /// Wall-clock duration of the iteration (ms). Some scenarios may
    /// also break this down further via the `metrics` map below.
    pub wall_ms: f64,

    /// Scenario-specific named metrics. Use `_ms` / `_bytes` / `_pct`
    /// suffixes so the comparator can pick a reasonable display unit.
    pub metrics: std::collections::BTreeMap<String, f64>,
}

/// Aggregate over the non-warmup samples for a single metric.
#[derive(Debug, Serialize, Deserialize)]
pub struct MetricAggregate {
    pub name: String,
    /// Unit hint inferred from the name suffix (`ms`, `bytes`, `pct`,
    /// `count`, `per_sec`, `rps`, `secs`, or `?`). Used by the
    /// comparator for human-readable formatting; semantics live in
    /// the metric name itself.
    pub unit: String,
    /// Whether *higher* values of this metric are better
    /// (throughput-style: `rps`, `mb_per_sec`) versus lower
    /// (latency / resource cost). Inferred from the metric name
    /// suffix. The comparator uses this to flip the regression
    /// check — for higher-is-better metrics a -20% delta is a
    /// regression, not an improvement.
    #[serde(default)]
    pub higher_is_better: bool,
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
    /// Sample standard deviation (Bessel-corrected, denominator `n-1`).
    /// `0.0` when fewer than 2 samples were collected.
    pub stdev: f64,
    /// Effective `n` (number of non-warmup samples that contributed).
    pub n: usize,
}

/// Host + boxlite snapshot captured at run start. Lets two reports
/// disagree noisily when their environments differ enough that a
/// diff would be meaningless.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReportMetadata {
    /// RFC3339 timestamp of when this run started (UTC).
    pub started_at: String,

    /// Free-form label from `--label`, when provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// `git rev-parse HEAD`, or `"unknown"` if not in a git tree.
    pub git_commit: String,

    /// boxlite version (CARGO_PKG_VERSION).
    pub boxlite_version: String,

    /// Host snapshot — see [`host_info::HostInfo`].
    pub host: super::host_info::HostInfo,
}

impl BenchReport {
    /// Construct a fresh report envelope. Aggregates are filled in
    /// after the run loop completes; samples accumulate as they're
    /// collected.
    pub fn new(scenario: String, metadata: ReportMetadata) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            scenario,
            metadata,
            sample_count: 0,
            warmup_count: 0,
            samples: Vec::new(),
            aggregates: Vec::new(),
        }
    }

    /// Resolve the metric-name-to-unit hint used in [`MetricAggregate::unit`].
    ///
    /// Convention: the trailing token picks the unit. `_ms` →
    /// milliseconds, `_secs` → seconds, `_bytes` → bytes, `_pct` →
    /// percent, `_count` → count, `_per_sec` → "per_sec" (used
    /// e.g. by `*_mb_per_sec`), `_rps` → "rps", anything else → `?`.
    pub fn unit_hint(metric_name: &str) -> &'static str {
        if metric_name.ends_with("_ms") {
            "ms"
        } else if metric_name.ends_with("_secs") {
            "secs"
        } else if metric_name.ends_with("_bytes") {
            "bytes"
        } else if metric_name.ends_with("_pct") {
            "pct"
        } else if metric_name.ends_with("_count") {
            "count"
        } else if metric_name.ends_with("_per_sec") {
            "per_sec"
        } else if metric_name.ends_with("_rps") {
            "rps"
        } else {
            "?"
        }
    }

    /// Returns `true` when higher values of this metric are
    /// better (throughput-style). Inferred from the metric name
    /// suffix:
    ///   * `_rps` / `_per_sec` → throughput → higher is better
    ///   * `_ms` / `_secs` / `_bytes` / `_count` / `_pct` →
    ///     cost-style → lower is better
    ///   * Everything else → `false` (lower-is-better default).
    ///     Conservative default; means a hand-named metric without
    ///     a clear suffix falls back to "regression on increase",
    ///     which matches the historic compare behavior.
    pub fn higher_is_better(metric_name: &str) -> bool {
        metric_name.ends_with("_rps") || metric_name.ends_with("_per_sec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Schema version is exposed so consumers can refuse mismatched
    /// reports. Lock the constant value here — bumping it is a
    /// deliberate breaking change.
    #[test]
    fn schema_version_is_pinned() {
        assert_eq!(SCHEMA_VERSION, "1.0");
    }

    #[test]
    fn unit_hint_resolves_known_suffixes() {
        assert_eq!(BenchReport::unit_hint("total_create_duration_ms"), "ms");
        assert_eq!(BenchReport::unit_hint("rss_bytes"), "bytes");
        assert_eq!(BenchReport::unit_hint("cpu_idle_pct"), "pct");
        assert_eq!(BenchReport::unit_hint("commands_executed_count"), "count");
        assert_eq!(BenchReport::unit_hint("soak_secs"), "secs");
        assert_eq!(BenchReport::unit_hint("pull_mb_per_sec"), "per_sec");
        assert_eq!(BenchReport::unit_hint("serve_rps"), "rps");
        assert_eq!(BenchReport::unit_hint("nonsense"), "?");
    }

    /// Direction inference must flag throughput-style metrics as
    /// higher-is-better; everything else (latency, resource cost)
    /// stays lower-is-better. The compare gating relies on this:
    /// a buggy `false` for `serve_rps` would mean a 50% drop in RPS
    /// reports as "improved" and never trips a regression.
    #[test]
    fn higher_is_better_recognizes_throughput_suffixes() {
        assert!(BenchReport::higher_is_better("serve_rps"));
        assert!(BenchReport::higher_is_better("pull_mb_per_sec"));
        assert!(BenchReport::higher_is_better("disk_write_mb_per_sec"));
        assert!(BenchReport::higher_is_better("net_tcp_mb_per_sec"));
        assert!(!BenchReport::higher_is_better("total_create_ms"));
        assert!(!BenchReport::higher_is_better("rss_bytes"));
        assert!(!BenchReport::higher_is_better("cpu_idle_pct"));
        assert!(!BenchReport::higher_is_better("commands_executed_count"));
        assert!(!BenchReport::higher_is_better("nonsense"));
    }
}
