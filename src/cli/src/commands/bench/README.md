# `boxlite bench`

Runtime performance harness. Drive a boxlite box (or a `boxlite serve` child)
through a defined scenario, collect per-stage metrics, emit a versioned JSON
report, and gate a CI run against a baseline.

## Surface

```
boxlite bench list                                            # show scenarios
boxlite bench run <name> --runs N [--warmup M] [--out PATH] [--label X]
boxlite bench compare <baseline.json> <current.json> [--on p99] [--threshold 0.20]
```

`--runs` is the total sample count; `--warmup` drops the first M samples
from aggregates (use it when the first iteration pays a cold-cache cost the
rest don't). `--label` is captured in the report metadata for CI build IDs
etc. `--out` defaults to stdout.

## Scenarios

Forty-four scenarios across five axes. All use `alpine:latest`
unless noted; the runner honors `--registry` from `GlobalFlags`,
so the same mirror config that works for the rest of the CLI
works here. `apk add` is used inside the box for workload tools
(`fio`, `iperf3`, `stress-ng`) that aren't in the alpine base —
these installs are amortized across iterations via the shared
`--home`. Host-side prereqs (`iperf3` on PATH) are checked at
scenario start and the scenario SKIPs cleanly when missing.

### Latency
| Scenario | What it measures |
| --- | --- |
| `latency-cold-start` | Fresh `--home` per iteration; full first-box-on-fresh-machine cost. |
| `latency-cold-start-jailed` | Cold-start with `SecurityOptions::maximum()`; delta vs cold-start = isolation tax. SKIPs off-Linux. |
| `latency-warm-start` | Shared `--home`; steady-state subsequent-box create+start. |
| `latency-clone` | `LiteBox::clone_box` on a 64-MiB-staged source. |
| `latency-clone-batch-10` | `LiteBox::clone_boxes(N=10)` batch path; per-clone amortized ms. |
| `latency-snapshot` | `SnapshotHandle` create + restore + remove round-trip. |
| `latency-inspect-list` | `list_info` + `get_info` at N=20 boxes (SQLite query scaling). |
| `latency-get-or-create-dedup` | 100 `get_or_create(name)` hits on a pre-materialized box; µs/op floor for name→box-id lookup. |
| `latency-image-pull-cached` | `images.pull` with shared `--home`; ms-scale warm-cache pull (vs cold `throughput-image-pull`). |
| `latency-runtime-shutdown` | `rt.shutdown(default 10s/box timeout)` with N=3 running boxes; container-orchestrator graceful-stop SLA floor. |

### Resource
| Scenario | What it measures |
| --- | --- |
| `resource-idle` | Idle alpine box footprint — RSS / COW / CPU% after 3 s settle. |
| `resource-cpu-load` | One vCPU pegged by `stress-ng --cpu 1`; samples during load. |
| `resource-mem-pressure` | Box capped at 256 MiB; `stress-ng --vm 1 --vm-bytes 150m`. Catches OOM-kill regressions (exit transitions from `1` to `137`). |
| `resource-density-10-idle` | 10 idle boxes coexisting; total RSS + COW + host fd. |
| `resource-multi-vcpu-load` | 4 vCPUs all saturated; tests libkrun vCPU thread mapping. |
| `resource-runtime-metrics-poll` | `rt.metrics()` poll cost at N=10 boxes; 500 samples; µs mean/p50/p99/max. |

### Density
| Scenario | What it measures |
| --- | --- |
| `density-parallel-10` | 10 boxes spawned concurrently; per-box max/mean under contention. |

### Throughput
| Scenario | What it measures |
| --- | --- |
| `throughput-image-pull` | `images.pull(alpine:latest)` into a fresh home; MB/s. |
| `throughput-disk-write` | In-box dd 64 MiB seq write with fsync; qcow2-COW-over-virtio. |
| `throughput-disk-read` | In-box dd 64 MiB seq read from a pre-staged file. |
| `throughput-disk-fio` | In-box fio 4K random writes; IOPS + clat p50/p99/p999. |
| `throughput-disk-fio-read` | In-box fio 4K random reads; IOPS + clat tail. |
| `throughput-virtiofs` | In-box dd write to a `-v host_tmp:/host` mount; virtiofs MB/s. |
| `throughput-net-tcp-sink` | Host writes 32 MiB to in-box `nc` sink via `-p` forward. |
| `throughput-net-iperf3` | Host `iperf3` client → gvproxy → in-box server. bps + retransmits. |
| `throughput-net-iperf3-egress` | Reverse: in-box client → host server via gvproxy NAT (192.168.127.254). |
| `throughput-net-iperf3-parallel` | iperf3 -P 4; aggregate bps + per-stream stdev (fairness). |
| `throughput-net-udp` | iperf3 -u -b 1G; bps + loss percent. **Currently SKIPs** (gvproxy TCP+UDP same-port forward broken — set `BOXLITE_BENCH_UDP_FORCE=1` to attempt). |
| `throughput-tcp-cps` | TCP connection-establish rate via busybox-nc respawn loop. |
| `throughput-dns-latency` | 20 `getent ahosts` per target {internal, recursive-forward}. |
| `throughput-serve-rps` | Spawn `boxlite serve` child, hammer /v1/config with 16 workers. |
| `throughput-export` | `LiteBox::export` to .boxlite archive; MB/s + bytes. |
| `throughput-import` | `rt.import_box` from a pre-exported archive; counterpart to export, completes the migration round-trip. |
| `throughput-many-ports-setup` | Create+start a box with 16 `-p` forwards; gvproxy port-table fan-out cost. |
| `throughput-volumes-multi-setup` | Create+start a box with 2 `-v` host mounts (libkrun `KRUN_VIRTIO_FS_MAX`); virtiofs fan-out cost at bring-up. |
| `throughput-copy-into` | `LiteBox::copy_into` 64 MiB host→guest; tar-stream MB/s. |
| `throughput-copy-out` | `LiteBox::copy_out` 64 MiB guest→host; tar-stream MB/s. |

### Stability
| Scenario | What it measures |
| --- | --- |
| `stability-churn` | 50 create+start+stop cycles. Per-cycle latency + host fd delta. |
| `stability-soak` | One box alive for `BOXLITE_BENCH_SOAK_SECS` (default 30 s) idle. |
| `stability-soak-load` | Same window, in-box fio random-read loop. Catches under-load leaks. |
| `stability-exec-loop` | 500 serial execs on one box; fd + RSS leak check. |
| `stability-exec-parallel` | 20 concurrent execs; batch wall + per-exec p99 under contention. |
| `stability-restart-loop` | 20 stop+start on the SAME box; COW growth + warm-restart accumulators. |
| `stability-snapshot-loop` | 20 cycles of snapshot create+restore+remove; per-cycle ms + COW delta. Catches snapshot subsystem leaks. |

## Report schema

Versioned JSON (`schema_version = "1.0"`). Top-level shape:

```jsonc
{
  "schema_version": "1.0",
  "scenario": "latency-cold-start",
  "metadata": {
    "started_at": "2026-05-25T09:38:05.444+00:00",
    "label": null,
    "git_commit": "<sha>",
    "boxlite_version": "0.9.5",
    "host": { "kernel": "...", "arch": "...", "cpu_model": "...", "cpu_count": 4, "mem_total_bytes": ... }
  },
  "sample_count": N,                  // runs - warmup
  "warmup_count": M,
  "samples": [
    { "iteration": 1, "warmup": false, "wall_ms": 23063.9, "metrics": { "total_create_ms": 20745.0, ... } }
  ],
  "aggregates": [
    { "name": "total_create_ms", "unit": "ms", "higher_is_better": false,
      "min": 20745.0, "p50": 20745.0, "p90": 20745.0, "p99": 20745.0,
      "max": 20745.0, "mean": 20745.0, "stdev": 0.0, "n": 1 }
  ]
}
```

Conventions:

* Metric name suffix → unit hint:
  `_ms` / `_secs` / `_bytes` / `_pct` / `_count` / `_per_sec` / `_rps`.
* Suffix `_rps` or `_per_sec` ⇒ `higher_is_better: true`; everything else
  defaults to lower-is-better.
* `wall_ms` is automatically captured per iteration (the runner times
  around `Scenario::run_once`), so every report has at least one
  comparable headline number.

## `bench compare` semantics

Joins both reports' aggregates by metric name, picks the percentile from
`--on` (default `p99`), and computes a `regression_ratio` that's sign-
flipped for `higher_is_better` metrics. The gate fails when
`regression_ratio > --threshold` (default `0.20`).

For a latency metric: `current > baseline * (1 + 0.20)` fails.
For a throughput metric: `current < baseline * (1 - 0.20)` fails (a 20%
drop in RPS is the regression). Improvements never fail; the gate is
one-sided.

Schema-major mismatch or scenario mismatch refuses to compare loudly
instead of silently disagreeing about what was measured. Missing /
new metrics are reported but don't trip the gate.

## Adding a scenario

1. Implement `Scenario` (see `runner.rs`) in a new module under
   `scenarios/`. Use the helpers in `scenarios/common.rs`
   (`build_runtime`, `alpine_options`, `BoxGuard`) so per-iteration
   runtime construction and panic-safe teardown stay consistent.
2. Append an entry to `scenarios::registry()` AND a matching arm in
   `scenarios::build_by_name()`. The `every_registered_scenario_is_buildable`
   unit test will fail loudly if those two drift apart.
3. Use metric names with the unit suffixes above so the comparator's
   direction handling Just Works.

## Known limitations

* Some scenarios' iteration wall includes a long teardown (`live.stop()`
  on a box that just held an active exec can take tens of seconds).
  The bench DATA — `disk_write_mb_per_sec`, `net_tcp_mb_per_sec`,
  etc. — is timed around the specific operation and unaffected.
* `stability-soak` is gated by env var (`BOXLITE_BENCH_SOAK_SECS`)
  rather than a CLI flag. A future generic `--scenario-arg KEY=VAL`
  would replace this.
* `throughput-net-iperf3` needs `iperf3` on the host's PATH; the
  scenario SKIPs (with marker metric `iperf3_skipped=1`) rather
  than fail when missing. The in-box `iperf3` install is automatic
  via `apk add`.
* The in-box `apk add fio` / `apk add iperf3` steps make the first
  iteration of `throughput-disk-fio` / `throughput-net-iperf3` ~5 s
  slower than subsequent iterations. The shared `--home` carries
  the install on the COW so it's a one-time cost. Use `--warmup 1`
  for clean steady-state aggregates.
