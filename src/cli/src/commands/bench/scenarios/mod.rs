//! Scenario registry — each axis appends to [`registry()`] and the
//! dispatcher in [`build_by_name`] without touching the runner
//! plumbing.
//!
//! The two functions are kept in lock-step by hand: a `match` arm in
//! [`build_by_name`] for every entry in [`registry`]. A test below
//! pins that invariant so it can't silently drift when a new
//! scenario lands.

use super::runner::Scenario;

pub mod clone_batch;
pub mod common;
pub mod dedup_lookup;
pub mod image_pull_cached;
pub mod inspect_list;
pub mod latency;
pub mod latency_jailed;
pub mod lifecycle;
pub mod runtime_shutdown;
pub mod snapshot;

/// One row in `boxlite bench list`. The registry is intentionally
/// static-data (vs. a `Box<dyn Fn>` table) so listing has zero
/// construction cost — building the actual scenario happens on
/// `run`, lazily, in [`build_by_name`].
pub struct ScenarioEntry {
    pub name: &'static str,
    pub description: &'static str,
}

/// All registered scenarios, in display order.
pub fn registry() -> &'static [ScenarioEntry] {
    &[
        ScenarioEntry {
            name: "latency-cold-start",
            description: "Fresh `--home` per iteration — measures alpine:latest \
                 first-box-on-a-fresh-machine create+start latency. \
                 Pull + base disk build + guest rootfs bootstrap all \
                 included.",
        },
        ScenarioEntry {
            name: "latency-warm-start",
            description: "Shared `--home` across iterations — measures \
                 steady-state second+ box create+start latency on a \
                 host that already has the image cache, base disk, \
                 and guest rootfs warm.",
        },
        ScenarioEntry {
            name: "latency-cold-start-jailed",
            description: "Cold-start with `SecurityOptions::maximum()` \
                 (jailer + seccomp + UID drop + new PID NS + chroot + \
                 close_fds + rlimits). Delta against latency-cold-\
                 start = the per-box isolation tax. SKIPs on platforms \
                 where full isolation is not available.",
        },
        ScenarioEntry {
            name: "latency-clone",
            description: "`LiteBox::clone_box` on a 64-MiB-staged \
                 source. Tests the COW-overlay + DB-row clone path.",
        },
        ScenarioEntry {
            name: "latency-clone-batch-10",
            description: "`LiteBox::clone_boxes(N=10)` batch optimized \
                 path; per-clone amortized ms should be much smaller \
                 than `latency-clone`'s single-call cost.",
        },
        ScenarioEntry {
            name: "latency-snapshot",
            description: "`SnapshotHandle::create` + `restore` \
                 round-trip on a 64-MiB-staged box.",
        },
        ScenarioEntry {
            name: "latency-inspect-list",
            description: "list_info + get_info latency at N=20 \
                 boxes. Tests SQLite query scaling for the runtime \
                 box store.",
        },
        ScenarioEntry {
            name: "latency-get-or-create-dedup",
            description: "100 `rt.get_or_create(name)` calls against a \
                 pre-materialized box. Floor number for name→box-id \
                 dedup-hit lookup (SQLite name index + LiteBox \
                 materialization). Reports first-call create cost \
                 separately for context.",
        },
        ScenarioEntry {
            name: "latency-image-pull-cached",
            description: "Warm-cache image pull: shared `--home` so \
                 the first iter populates and subsequent iters hit \
                 the manifest cache. Headline = the cost of \
                 `boxlite pull alpine` when the image is already \
                 local; should be ms-scale, not seconds. Distinct \
                 from `throughput-image-pull` which is cold-cache.",
        },
        ScenarioEntry {
            name: "latency-runtime-shutdown",
            description: "`rt.shutdown(default 10 s/box timeout)` \
                 with N=3 running boxes. Headline for container-\
                 orchestrator graceful-stop SLAs. Builds a fresh \
                 runtime per iteration because shutdown permanently \
                 disables it.",
        },
        ScenarioEntry {
            name: "throughput-export",
            description: "`LiteBox::export` on a 64-MiB-staged source. \
                 Reports archive bytes + MB/s. Tests the box-archive \
                 tarball serialization codepath. Shipped alongside \
                 `latency-clone` because both share the source-box \
                 staging helper in `lifecycle.rs`.",
        },
    ]
}

/// Construct a scenario by name. Returns `None` if the name doesn't
/// match any entry in [`registry()`]; the runner converts that into a
/// user-facing "unknown scenario" error.
pub fn build_by_name(name: &str) -> Option<Box<dyn Scenario>> {
    match name {
        "latency-cold-start" => Some(Box::new(latency::ColdStart::new())),
        "latency-warm-start" => Some(Box::new(latency::WarmStart::new())),
        "latency-cold-start-jailed" => {
            Some(Box::new(latency_jailed::LatencyColdStartJailed::new()))
        }
        "latency-clone" => Some(Box::new(lifecycle::LatencyClone::new())),
        "latency-clone-batch-10" => Some(Box::new(clone_batch::CloneBatch::new())),
        "latency-snapshot" => Some(Box::new(snapshot::Snapshot::new())),
        "latency-inspect-list" => Some(Box::new(inspect_list::InspectList::new())),
        "latency-get-or-create-dedup" => Some(Box::new(dedup_lookup::DedupLookup::new())),
        "latency-image-pull-cached" => Some(Box::new(image_pull_cached::ImagePullCached::new())),
        "latency-runtime-shutdown" => Some(Box::new(runtime_shutdown::RuntimeShutdown::new())),
        "throughput-export" => Some(Box::new(lifecycle::ThroughputExport::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `registry()` and `build_by_name()` are kept in lock-step by
    /// hand. This test enforces that contract — every entry in the
    /// registry must be constructible by name. A new scenario added
    /// to the registry without a matching arm here will surface as a
    /// missing-arm failure instead of a runtime "unknown scenario"
    /// error.
    #[test]
    fn every_registered_scenario_is_buildable() {
        for entry in registry() {
            let built = build_by_name(entry.name);
            assert!(
                built.is_some(),
                "registry entry {:?} has no build_by_name arm",
                entry.name
            );
            // Round-trip: built scenario's name() must match.
            let s = built.unwrap();
            assert_eq!(
                s.name(),
                entry.name,
                "build_by_name({:?}) returned a scenario reporting name() = {:?}",
                entry.name,
                s.name()
            );
        }
    }

    /// Names are user-facing; pin uniqueness in the registry so a
    /// rename accident can't collide with an existing scenario and
    /// silently overwrite it.
    #[test]
    fn registry_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in registry() {
            assert!(
                seen.insert(entry.name),
                "duplicate scenario name in registry: {:?}",
                entry.name
            );
        }
    }
}
