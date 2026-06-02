//! `boxlite df` — operator-facing disk-usage view.
//!
//! Text format (default) is meant to be read at a glance: one block for
//! host headroom + reserve health, one for `~/.boxlite/` footprint, one
//! for what GC would reclaim. JSON format is for scripts
//! (`jq '.reclaimable.total_bytes'`).

use boxlite::BoxliteError;
use boxlite::runtime::df::{DiskUsageReport, ReserveStatus};
use clap::{Args, ValueEnum};
use serde::Serialize;

#[derive(Args, Debug)]
pub struct DfArgs {
    /// Output format — `text` (default, human) or `json` (scripted).
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

pub async fn execute(args: DfArgs, global: &crate::cli::GlobalFlags) -> anyhow::Result<()> {
    let runtime = global.create_runtime()?;
    let report = match runtime.disk_usage() {
        Ok(r) => r,
        // REST runtimes intentionally don't implement this; surface the
        // unsupported case as a clean exit-1 rather than a confusing
        // partial output.
        Err(e @ BoxliteError::Unsupported(_)) => {
            anyhow::bail!("{e}");
        }
        Err(e) => return Err(e.into()),
    };

    match args.format {
        OutputFormat::Text => print_text(&report),
        OutputFormat::Json => print_json(&report)?,
    }
    Ok(())
}

fn print_text(r: &DiskUsageReport) {
    println!("Host filesystem ({}):", r.host.home_dir.display());
    println!(
        "  total: {}    free: {}    used: {}",
        human(r.host.total_bytes),
        human(r.host.free_bytes),
        percent_used(r.host.free_bytes, r.host.total_bytes),
    );
    let reserve = match &r.host.reserve {
        ReserveStatus::Healthy { bytes } => format!("healthy ({})", human(*bytes)),
        ReserveStatus::Partial { bytes, expected } => format!(
            "PARTIAL — {} of {} on disk (host was full when last topped up)",
            human(*bytes),
            human(*expected),
        ),
        ReserveStatus::Absent => {
            "ABSENT — recovery floor not in place (released, or runtime never booted)".to_string()
        }
    };
    println!("  reserve: {reserve}");

    println!();
    println!("~/.boxlite/ footprint:");
    println!("  boxes:  {}", human(r.home.boxes_bytes));
    println!("  bases:  {}", human(r.home.bases_bytes));
    println!("  images: {}", human(r.home.images_bytes));
    println!("  other:  {}", human(r.home.other_bytes));
    println!("  total:  {}", human(r.home.total_bytes()));

    println!();
    println!("Reclaimable via `boxlite gc`:");
    println!(
        "  orphan box dirs:    {} ({})",
        r.reclaimable.box_dirs_removed,
        human(r.reclaimable.box_dirs_bytes),
    );
    println!(
        "  orphan bases:       {} ({})",
        r.reclaimable.bases_removed,
        human(r.reclaimable.bases_bytes),
    );
    println!(
        "  orphan disk-images: {} ({})",
        r.reclaimable.disk_images_removed,
        human(r.reclaimable.disk_images_bytes),
    );
    println!(
        "  total:              {} ({})",
        r.reclaimable.total_removed(),
        human(r.reclaimable.total_bytes()),
    );
}

#[derive(Serialize)]
struct JsonView {
    host: JsonHost,
    home: JsonHome,
    reclaimable: JsonReclaim,
}

#[derive(Serialize)]
struct JsonHost {
    home_dir: String,
    total_bytes: u64,
    free_bytes: u64,
    reserve: JsonReserve,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum JsonReserve {
    Healthy { bytes: u64 },
    Partial { bytes: u64, expected: u64 },
    Absent,
}

#[derive(Serialize)]
struct JsonHome {
    boxes_bytes: u64,
    bases_bytes: u64,
    images_bytes: u64,
    other_bytes: u64,
    total_bytes: u64,
}

#[derive(Serialize)]
struct JsonReclaim {
    dry_run: bool,
    box_dirs_removed: u64,
    box_dirs_bytes: u64,
    bases_removed: u64,
    bases_bytes: u64,
    disk_images_removed: u64,
    disk_images_bytes: u64,
    total_bytes: u64,
}

fn print_json(r: &DiskUsageReport) -> anyhow::Result<()> {
    let view = JsonView {
        host: JsonHost {
            home_dir: r.host.home_dir.display().to_string(),
            total_bytes: r.host.total_bytes,
            free_bytes: r.host.free_bytes,
            reserve: match &r.host.reserve {
                ReserveStatus::Healthy { bytes } => JsonReserve::Healthy { bytes: *bytes },
                ReserveStatus::Partial { bytes, expected } => JsonReserve::Partial {
                    bytes: *bytes,
                    expected: *expected,
                },
                ReserveStatus::Absent => JsonReserve::Absent,
            },
        },
        home: JsonHome {
            boxes_bytes: r.home.boxes_bytes,
            bases_bytes: r.home.bases_bytes,
            images_bytes: r.home.images_bytes,
            other_bytes: r.home.other_bytes,
            total_bytes: r.home.total_bytes(),
        },
        reclaimable: JsonReclaim {
            dry_run: r.reclaimable.dry_run,
            box_dirs_removed: r.reclaimable.box_dirs_removed,
            box_dirs_bytes: r.reclaimable.box_dirs_bytes,
            bases_removed: r.reclaimable.bases_removed,
            bases_bytes: r.reclaimable.bases_bytes,
            disk_images_removed: r.reclaimable.disk_images_removed,
            disk_images_bytes: r.reclaimable.disk_images_bytes,
            total_bytes: r.reclaimable.total_bytes(),
        },
    };
    println!("{}", serde_json::to_string_pretty(&view)?);
    Ok(())
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;
const KIB: f64 = 1024.0;

fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn percent_used(free: u64, total: u64) -> String {
    if total == 0 {
        return "n/a".to_string();
    }
    let used = total.saturating_sub(free) as f64 / total as f64 * 100.0;
    format!("{used:.1}%")
}
