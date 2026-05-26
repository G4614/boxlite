//! `boxlite bench` — runtime performance benchmark harness.
//!
//! Layout:
//!
//! ```text
//! boxlite bench list                                   — what scenarios exist
//! boxlite bench run <scenario> [--runs N] [--out PATH] — collect samples
//! boxlite bench compare <baseline.json> <current.json> — regress vs baseline
//! ```
//!
//! Output is a versioned JSON document ([`report::BenchReport`]) that pins
//! the scenario name, the per-run samples, the per-metric aggregates
//! (min/p50/p90/p99/max/mean/stdev), and a host snapshot (kernel, CPU,
//! mem, boxlite git commit) so two reports are diffable across machines
//! without ambiguity.
//!
//! The bench harness sits on top of the existing `BoxMetrics`
//! instrumentation (`src/boxlite/src/metrics/box_metrics.rs`) — every
//! `LiteBox` already records per-stage initialization durations and
//! exposes them via `LiteBox::metrics().await`. Scenarios just drive a
//! box (create / exec / etc.) and consume the resulting `BoxMetrics`
//! snapshot, so the harness adds zero new runtime overhead to the
//! non-bench path.

use crate::cli::GlobalFlags;
use clap::{Args, Subcommand};

pub mod compare;
pub mod host_info;
pub mod report;
pub mod runner;
pub mod scenarios;
pub mod stats;

#[derive(Args, Debug)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub command: BenchCommand,
}

#[derive(Subcommand, Debug)]
pub enum BenchCommand {
    /// List available benchmark scenarios.
    List,
    /// Run a scenario N times and emit a JSON report.
    Run(RunArgs),
    /// Compare a current report against a baseline.
    Compare(CompareArgs),
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Scenario name (use `boxlite bench list` to see options).
    #[arg(value_name = "SCENARIO")]
    pub scenario: String,

    /// Number of sample iterations. Higher N tightens percentile
    /// confidence at the cost of wall time; the default trades off
    /// runtime for usable p90/p99.
    #[arg(long, default_value_t = 10)]
    pub runs: usize,

    /// Drop the first N samples before aggregating. Useful when the
    /// first run pays an unrelated cost (image pull, base disk
    /// materialization) that you don't want polluting the steady-state
    /// percentiles.
    #[arg(long, default_value_t = 0)]
    pub warmup: usize,

    /// Write the report JSON here. Defaults to stdout.
    #[arg(long, value_name = "PATH")]
    pub out: Option<std::path::PathBuf>,

    /// Free-form label captured into the report (CI build id, branch
    /// name, etc.) so diff tooling can disambiguate two runs from the
    /// same commit.
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct CompareArgs {
    /// Baseline report (the "known good" numbers).
    #[arg(value_name = "BASELINE")]
    pub baseline: std::path::PathBuf,

    /// Current report under test.
    #[arg(value_name = "CURRENT")]
    pub current: std::path::PathBuf,

    /// Maximum allowed relative regression on the headline percentile
    /// (default p99) before the comparison exits non-zero. `0.20` =
    /// 20% slower than baseline = fail. Improvements are always OK.
    #[arg(long, default_value_t = 0.20)]
    pub threshold: f64,

    /// Which aggregate to gate on. p99 is the default because it
    /// catches tail regressions a mean would smooth away.
    #[arg(long, default_value = "p99")]
    pub on: String,

    /// Emit a JSON diff (additional to the human-readable table).
    #[arg(long, value_name = "PATH")]
    pub json_out: Option<std::path::PathBuf>,
}

pub async fn execute(args: BenchArgs, global: &GlobalFlags) -> anyhow::Result<()> {
    match args.command {
        BenchCommand::List => runner::list_scenarios(),
        BenchCommand::Run(run) => runner::run_scenario(run, global).await,
        BenchCommand::Compare(cmp) => compare::execute(cmp),
    }
}
