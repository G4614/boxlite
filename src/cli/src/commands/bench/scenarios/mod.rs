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
pub mod copy_io;
pub mod dedup_lookup;
pub mod density;
pub mod disk;
pub mod disk_fio;
pub mod disk_read;
pub mod dns_latency;
pub mod exec_loop;
pub mod exec_parallel;
pub mod healthcheck_overhead;
pub mod image_pull_cached;
pub mod inspect_list;
pub mod latency;
pub mod latency_big_image;
pub mod latency_jailed;
pub mod latency_no_net;
pub mod lifecycle;
pub mod lifecycle_import;
pub mod many_ports;
pub mod multi_vcpu;
pub mod net;
pub mod net_iperf3;
pub mod net_iperf3_egress;
pub mod net_iperf3_parallel;
pub mod net_tcp_cps;
pub mod net_udp;
pub mod resource;
pub mod resource_density;
pub mod resource_load;
pub mod rest_cold_start;
pub mod rest_metrics_rps;
pub mod restart_loop;
pub mod runtime_metrics_poll;
pub mod runtime_shutdown;
pub mod serve_rps;
pub mod snapshot;
pub mod snapshot_loop;
pub mod soak;
pub mod soak_load;
pub mod stability;
pub mod throughput;
pub mod virtiofs;
pub mod volumes_multi;
pub mod ws_exec;

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
        ScenarioEntry {
            name: "resource-idle",
            description: "Idle alpine box footprint — RSS, COW disk \
                 bytes actually materialized, and CPU% sampled after \
                 a 3 s settle. Shared `--home` across iterations for \
                 steady-state numbers.",
        },
        ScenarioEntry {
            name: "resource-cpu-load",
            description: "Peg one vCPU at 100% via `stress-ng --cpu 1 \
                 --timeout 10s` in-box; sample RSS + CPU% every 2 s \
                 during the load. Catches libkrun-shim RSS growth \
                 that an idle scenario wouldn't see.",
        },
        ScenarioEntry {
            name: "resource-mem-pressure",
            description: "Box capped at 256 MiB, in-box `stress-ng \
                 --vm 1 --vm-bytes 200m --vm-keep` to test the cgroup \
                 ceiling. Reports peak RSS observed + stress-ng exit \
                 code (0 = clean, non-zero = OOM-kill).",
        },
        ScenarioEntry {
            name: "resource-density-10-idle",
            description: "10 idle alpine boxes coexisting; sums RSS + \
                 COW disk + host fd delta. Steady-state coexistence \
                 cost, distinct from density-parallel-10 (which \
                 measures concurrent spawn latency).",
        },
        ScenarioEntry {
            name: "resource-multi-vcpu-load",
            description: "Box with 4 vCPUs all saturated by stress-ng. \
                 Tests libkrun's vCPU thread mapping + multi-core \
                 KVM exit handling.",
        },
        ScenarioEntry {
            name: "resource-runtime-metrics-poll",
            description: "`rt.metrics()` poll cost at N=10 running \
                 boxes. 500 samples per iteration; mean/p50/p99/max \
                 in µs. Floor number for Prometheus scrape overhead.",
        },
        ScenarioEntry {
            name: "density-parallel-10",
            description: "Concurrent spawn of 10 alpine boxes through \
                 one runtime. Measures total burst wall time + the \
                 slowest box's individual latency, exposing the init- \
                 pipeline contention surcharge over a single warm \
                 start.",
        },
        ScenarioEntry {
            name: "throughput-image-pull",
            description: "Pull alpine:latest into a fresh `--home` and \
                 report MB/s based on layer tarball sizes on disk. \
                 Cold-cache every iteration. Headline number for \
                 registry / network changes.",
        },
        ScenarioEntry {
            name: "throughput-disk-write",
            description: "In-box sequential write throughput via \
                 `dd if=/dev/zero of=/tmp/... bs=1M count=64 \
                 conv=fsync`. Measures qcow2-COW-over-virtio \
                 bandwidth. Headline number for any disk-stack \
                 regression.",
        },
        ScenarioEntry {
            name: "throughput-disk-read",
            description: "Sequential dd read of a pre-staged 64 MiB \
                 file. Read-side counterpart to throughput-disk-\
                 write.",
        },
        ScenarioEntry {
            name: "throughput-disk-fio",
            description: "In-box fio: 4K random writes with fsync=1, \
                 reports IOPS / bw / clat p50-p99-p99.9. Complements \
                 throughput-disk-write's sequential MB/s with tail \
                 latency. One-time `apk add fio` in box.",
        },
        ScenarioEntry {
            name: "throughput-disk-fio-read",
            description: "fio 4K random reads from a pre-staged file. \
                 Read-side tail-latency counterpart to throughput-\
                 disk-fio.",
        },
        ScenarioEntry {
            name: "throughput-virtiofs",
            description: "In-box dd write to a `-v host_tmp:/host` \
                 mount. Measures virtiofs throughput — distinct \
                 from qcow2-COW (`throughput-disk-write`) since \
                 host volumes don't go through the overlay.",
        },
        ScenarioEntry {
            name: "throughput-net-tcp-sink",
            description: "Host→gvproxy→guest TCP throughput. Host \
                 writes 32 MiB to a busybox-nc sink in-box and \
                 measures wall time. Catches gvproxy/netstack \
                 regressions without needing iperf3 on host.",
        },
        ScenarioEntry {
            name: "throughput-net-iperf3",
            description: "Host iperf3 client → gvproxy → in-box iperf3 \
                 server. Reports proper bps + TCP retransmits. \
                 Requires `iperf3` on host (and `apk add iperf3` in \
                 box, one-time). SKIPs cleanly when host iperf3 is \
                 missing.",
        },
        ScenarioEntry {
            name: "throughput-net-iperf3-egress",
            description: "Reverse-direction iperf3: in-box client → \
                 gvproxy NAT → host server. Tests guest→host \
                 outbound path, distinct from -iperf3's host→guest \
                 path. Requires host iperf3.",
        },
        ScenarioEntry {
            name: "throughput-net-iperf3-parallel",
            description: "iperf3 -P 4 multi-stream. Aggregate bps + \
                 per-stream stdev (fairness signal) + total \
                 retransmits. Tests gvproxy parallel-connection \
                 handling.",
        },
        ScenarioEntry {
            name: "throughput-net-udp",
            description: "iperf3 -u UDP throughput at 1 Gbps target \
                 rate; reports bps + loss percent. Tests gvproxy's \
                 UDP path which is independent of the TCP forward. \
                 Currently SKIPs by default (gvproxy TCP+UDP same-\
                 port forward broken; BOXLITE_BENCH_UDP_FORCE=1 to \
                 attempt anyway).",
        },
        ScenarioEntry {
            name: "throughput-tcp-cps",
            description: "TCP connections-per-second establish rate \
                 via tight TcpStream::connect loop against an in-\
                 box busybox-nc respawn loop. Tests gvproxy SYN/\
                 accept hot-path.",
        },
        ScenarioEntry {
            name: "throughput-dns-latency",
            description: "20 `getent ahosts` lookups in-box for \
                 each of {host.docker.internal, example.com}. \
                 Tests gvproxy's embedded DNS resolver + the \
                 recursive-forward fallback path.",
        },
        ScenarioEntry {
            name: "throughput-serve-rps",
            description: "Spawn `boxlite serve` as a child, hammer \
                 GET /v1/config with 16 concurrent workers for 5 s, \
                 report achieved RPS. Floor number for the axum + \
                 tower + serde request stack.",
        },
        ScenarioEntry {
            name: "throughput-copy-into",
            description: "`LiteBox::copy_into` of a 64-MiB host file \
                 into the box; tar-stream MB/s.",
        },
        ScenarioEntry {
            name: "throughput-copy-out",
            description: "`LiteBox::copy_out` of a 64-MiB in-box file \
                 to host; tar-stream MB/s, validates host bytes. \
                 Stages payload on /root (rootfs) — /tmp is a tmpfs \
                 that the guest agent's file interface can't see.",
        },
        ScenarioEntry {
            name: "throughput-import",
            description: "`BoxliteRuntime::import_box` from a pre-\
                 exported .boxlite archive (64-MiB-staged source). \
                 Counterpart to throughput-export; together they \
                 form the cluster-migration round-trip story.",
        },
        ScenarioEntry {
            name: "throughput-many-ports-setup",
            description: "Create+start an alpine box with 16 `-p` \
                 forwards (host_port=None → OS-ephemeral). Measures \
                 the gvproxy port-table-fan-out cost; delta against \
                 `latency-warm-start` (which uses zero ports) is the \
                 per-port amortized setup tax.",
        },
        ScenarioEntry {
            name: "throughput-volumes-multi-setup",
            description: "Create+start an alpine box with 2 host-\
                 volume mounts (libkrun's `KRUN_VIRTIO_FS_MAX` cap). \
                 Measures virtiofs-fan-out cost at box-bring-up time \
                 — distinct from `throughput-virtiofs` (I/O \
                 bandwidth through one mount).",
        },
        ScenarioEntry {
            name: "stability-churn",
            description: "50 consecutive create+start+stop cycles \
                 through one shared `--home`. Reports per-cycle \
                 mean+max + host-side fd delta. Catches per-cycle \
                 leaks in fd / tempfile / DB-row accounting that \
                 only show up over a sustained churn workload.",
        },
        ScenarioEntry {
            name: "stability-soak",
            description: "Keep one alpine box alive for \
                 BOXLITE_BENCH_SOAK_SECS (default 30 s), sample \
                 RSS/COW/fd every 2 s, report first→last deltas. \
                 Catches steady-state idle leaks that churn misses.",
        },
        ScenarioEntry {
            name: "stability-soak-load",
            description: "Soak with continuous fio random-read \
                 workload running in-box. Sample RSS/COW/fd over \
                 BOXLITE_BENCH_SOAK_SECS (default 30 s). Catches \
                 under-load leaks (gvproxy goroutine pools, libkrun \
                 dirty-page buffers) that idle soak misses.",
        },
        ScenarioEntry {
            name: "stability-exec-loop",
            description: "500 boxlite-exec calls on a single running \
                 box; mean+max per-exec wall + host fd delta + \
                 guest RSS post-loop. Tolerant of partial completion \
                 — reports `exec_completed_count` so a regression \
                 that pushes the failure boundary lower than the \
                 historical ~#247 (boxlite 0.9.5 alpine x86_64) \
                 InitReady/IntermediateReady mismatch shows up.",
        },
        ScenarioEntry {
            name: "stability-exec-parallel",
            description: "20 concurrent execs on one box via \
                 tokio::spawn fan-out. Reports batch wall + per-\
                 exec p50/p99/max. Exposes lock contention in the \
                 guest's exec state map + gRPC server fairness.",
        },
        ScenarioEntry {
            name: "stability-restart-loop",
            description: "20 stop+start cycles on the SAME box \
                 (distinct from churn which recreates). Re-fetches \
                 the LiteBox handle via `rt.get` between cycles \
                 because `stop` invalidates the previous handle. \
                 Catches accumulators in the warm-restart path.",
        },
        ScenarioEntry {
            name: "stability-snapshot-loop",
            description: "20 sequential SnapshotHandle::create calls \
                 on the same box. Reports per-create mean/max + COW \
                 disk delta. Catches create-path accumulators \
                 (orphaned overlays, leaked DB rows) and qcow2 \
                 chain-depth scaling. Removes deliberately omitted \
                 — see scenario file header for the dep invariant.",
        },
        ScenarioEntry {
            name: "latency-cold-start-no-net",
            description: "Cold-start with `NetworkSpec::Disabled` — \
                 gvproxy is not started and the guest gets no eth0. \
                 Delta against `latency-cold-start` = gvproxy boot \
                 cost; compute-only workloads can shave that off.",
        },
        ScenarioEntry {
            name: "latency-cold-start-big-image",
            description: "Cold-start with `python:3.12-alpine` (~50MB, \
                 multi-layer). Stresses the layer-tarball-extraction \
                 and qcow2-base-build paths at non-trivial scale. \
                 Delta vs `latency-cold-start` (alpine ~3MB) reveals \
                 size-dependent stage scaling.",
        },
        ScenarioEntry {
            name: "resource-healthcheck-overhead",
            description: "Box with `HealthCheckOptions` at 500ms \
                 interval; sample CPU%/RSS over 10s. Delta vs \
                 `resource-idle` (no healthcheck) = the healthcheck \
                 ping cost; extrapolate × (real interval / 500ms) \
                 for production tuning.",
        },
        ScenarioEntry {
            name: "latency-rest-cold-start",
            description: "Cold-start over the REST API: spawn \
                 `boxlite serve` child, build `BoxliteRuntime::rest`, \
                 measure end-to-end create+start through HTTP. \
                 Delta vs `latency-cold-start` = REST overhead.",
        },
        ScenarioEntry {
            name: "latency-ws-exec",
            description: "100 echoes through `LiteBox::exec` over the \
                 REST/WebSocket exec channel. Per-exec mean/p50/p99/\
                 max ms. Delta vs `stability-exec-loop` (in-process) \
                 = REST + tungstenite + axum framing tax.",
        },
        ScenarioEntry {
            name: "throughput-rest-metrics-rps",
            description: "`GET /v1/metrics` RPS via reqwest hammer × \
                 16 workers × 5s. Caps how dense Prometheus scrape \
                 intervals can be before serve becomes the \
                 bottleneck.",
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
        "resource-idle" => Some(Box::new(resource::ResourceIdle::new())),
        "resource-cpu-load" => Some(Box::new(resource_load::CpuLoad::new())),
        "resource-mem-pressure" => Some(Box::new(resource_load::MemPressure::new())),
        "resource-density-10-idle" => Some(Box::new(resource_density::DensityIdle::new())),
        "resource-multi-vcpu-load" => Some(Box::new(multi_vcpu::MultiVcpu::new())),
        "resource-runtime-metrics-poll" => {
            Some(Box::new(runtime_metrics_poll::RuntimeMetricsPoll::new()))
        }
        "density-parallel-10" => Some(Box::new(density::DensityParallel10::new())),
        "throughput-image-pull" => Some(Box::new(throughput::ImagePull::new())),
        "throughput-disk-write" => Some(Box::new(disk::DiskWrite::new())),
        "throughput-disk-read" => Some(Box::new(disk_read::DiskRead::new())),
        "throughput-disk-fio" => Some(Box::new(disk_fio::DiskFio::new())),
        "throughput-disk-fio-read" => Some(Box::new(disk_read::DiskFioRead::new())),
        "throughput-virtiofs" => Some(Box::new(virtiofs::Virtiofs::new())),
        "throughput-net-tcp-sink" => Some(Box::new(net::NetTcpSink::new())),
        "throughput-net-iperf3" => Some(Box::new(net_iperf3::NetIperf3::new())),
        "throughput-net-iperf3-egress" => Some(Box::new(net_iperf3_egress::NetIperf3Egress::new())),
        "throughput-net-iperf3-parallel" => {
            Some(Box::new(net_iperf3_parallel::NetIperf3Parallel::new()))
        }
        "throughput-net-udp" => Some(Box::new(net_udp::NetUdp::new())),
        "throughput-tcp-cps" => Some(Box::new(net_tcp_cps::TcpCps::new())),
        "throughput-dns-latency" => Some(Box::new(dns_latency::DnsLatency::new())),
        "throughput-serve-rps" => Some(Box::new(serve_rps::ServeRps::new())),
        "throughput-copy-into" => Some(Box::new(copy_io::CopyInto::new())),
        "throughput-copy-out" => Some(Box::new(copy_io::CopyOut::new())),
        "throughput-import" => Some(Box::new(lifecycle_import::ThroughputImport::new())),
        "throughput-many-ports-setup" => Some(Box::new(many_ports::ManyPorts::new())),
        "throughput-volumes-multi-setup" => Some(Box::new(volumes_multi::VolumesMulti::new())),
        "stability-churn" => Some(Box::new(stability::Churn::new())),
        "stability-soak" => Some(Box::new(soak::Soak::new())),
        "stability-soak-load" => Some(Box::new(soak_load::SoakLoad::new())),
        "stability-exec-loop" => Some(Box::new(exec_loop::ExecLoop::new())),
        "stability-exec-parallel" => Some(Box::new(exec_parallel::ExecParallel::new())),
        "stability-restart-loop" => Some(Box::new(restart_loop::RestartLoop::new())),
        "stability-snapshot-loop" => Some(Box::new(snapshot_loop::SnapshotLoop::new())),
        "latency-cold-start-no-net" => Some(Box::new(latency_no_net::LatencyColdStartNoNet::new())),
        "latency-cold-start-big-image" => {
            Some(Box::new(latency_big_image::LatencyColdStartBigImage::new()))
        }
        "resource-healthcheck-overhead" => {
            Some(Box::new(healthcheck_overhead::HealthcheckOverhead::new()))
        }
        "latency-rest-cold-start" => Some(Box::new(rest_cold_start::RestColdStart::new())),
        "latency-ws-exec" => Some(Box::new(ws_exec::WsExec::new())),
        "throughput-rest-metrics-rps" => Some(Box::new(rest_metrics_rps::RestMetricsRps::new())),
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
