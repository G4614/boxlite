//! In-box TCP throughput scenario.
//!
//! Originally planned as `throughput-net-iperf3` — pivoted to
//! `throughput-net-tcp-sink` because (a) iperf3 isn't in the
//! alpine base, and (b) the most common bench host (this one
//! included) doesn't have iperf3 installed either, so a strict
//! iperf3 scenario would SKIP everywhere it actually mattered.
//!
//! Architecture:
//!
//!   1. Host pre-probes a free port (`TcpListener::bind(("0.0.0.0",
//!      0))` → read `local_addr().port()` → drop). Inherits the
//!      same TOCTOU window as gvproxy's own auto-remap path; the
//!      race is tiny and a single retry is enough.
//!   2. Box is created with `-p <free_host_port>:9999` so gvproxy
//!      maps `host:free_port → guest:9999`.
//!   3. Inside the box, `sh -c 'nc -l -p 9999 > /dev/null'` runs
//!      via `boxlite exec`. busybox nc on alpine accepts one
//!      connection, dumps its stdin into /dev/null, then exits.
//!   4. Host side opens a `tokio::net::TcpStream` to
//!      `127.0.0.1:free_host_port`, writes `PAYLOAD_BYTES` of
//!      zeroes in chunks, then shuts down the write half.
//!   5. Wall time of the host-side write is the bench number.
//!      `nc` then exits cleanly, the exec returns, and we tear
//!      everything down.
//!
//! What this measures: the gvproxy host→guest forward path
//! end-to-end, including the userspace netstack hop. The guest
//! consumes the bytes synchronously (busybox nc) so we're not
//! just filling a kernel socket buffer on the host side.
//!
//! Reported metrics:
//!   * `net_tcp_mb_per_sec` — `PAYLOAD_BYTES / 1 MiB / wall_secs`.
//!   * `net_tcp_bytes` — `PAYLOAD_BYTES` (const, captured into report).
//!   * `net_tcp_wall_ms` — host-side wall time of the write +
//!     shutdown_write call.
//!
//! Shared `--home` across iterations (warm cache).

use super::super::runner::{RunContext, Scenario};
use super::common::{BoxGuard, build_runtime};
use anyhow::{Context, Result};
use async_trait::async_trait;
use boxlite::runtime::options::{PortProtocol, PortSpec, RootfsSpec};
use boxlite::{BoxCommand, BoxOptions};
use futures::StreamExt;
use std::collections::BTreeMap;
use std::net::TcpListener as StdTcpListener;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// 32 MiB payload. Big enough to amortize TCP slow-start, small
/// enough to keep the iteration well under 10 s on a sane
/// loopback path.
const PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
/// In-box port the nc sink listens on. Arbitrary but fixed so the
/// `-p` mapping is deterministic.
const GUEST_PORT: u16 = 9999;
/// How long to wait after kicking off the in-box `nc` before we
/// assume it's bound and start writing from the host. busybox `nc`
/// is fast; 500 ms covers exec round-trip + bind on slow VMs.
const NC_BIND_WAIT: Duration = Duration::from_millis(500);
/// Bound on the host-side TCP connect — if gvproxy is alive but
/// the forward never completes, surface that fast rather than
/// hanging the iteration.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on the entire host-side write loop. 60 s is generous
/// enough that even a 1 MB/s gvproxy path completes the 32 MiB
/// payload; a stall implies something's actually broken.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct NetTcpSink {
    home: Option<TempDir>,
}

impl NetTcpSink {
    pub fn new() -> Self {
        Self { home: None }
    }
}

#[async_trait]
impl Scenario for NetTcpSink {
    fn name(&self) -> &str {
        "throughput-net-tcp-sink"
    }

    async fn run_once(&mut self, ctx: &RunContext) -> Result<BTreeMap<String, f64>> {
        if self.home.is_none() {
            self.home = Some(TempDir::new().context("mkdir net-tcp-sink home")?);
        }
        let home_path = self
            .home
            .as_ref()
            .expect("just initialized")
            .path()
            .to_path_buf();
        let rt = build_runtime(ctx.global, home_path)?;

        // Probe a free host port. The race between drop() and
        // gvproxy's later bind is the same window the EXPOSE auto-
        // remap helper accepts; on the very rare loss we'd see
        // gvproxy_create fail and the scenario error out — which
        // is exactly what the test would want a user to see.
        let host_port = {
            let probe = StdTcpListener::bind(("0.0.0.0", 0)).context("probe a free host port")?;
            probe
                .local_addr()
                .map(|addr| addr.port())
                .context("read probed local addr")?
        };

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            auto_remove: true,
            ports: vec![PortSpec {
                host_port: Some(host_port),
                guest_port: GUEST_PORT,
                protocol: PortProtocol::Tcp,
                host_ip: None,
            }],
            ..Default::default()
        };
        let live = rt.create(opts, None).await.context("rt.create(alpine)")?;
        let mut guard = BoxGuard::new(&rt, live.id().to_string());
        live.start().await.context("box.start()")?;

        // Spawn nc in-box as a backgrounded exec. We don't await
        // its `wait()` yet — nc has to be listening when the host-
        // side write fires.
        //
        // Three things tried (in increasing complexity / failure
        // modes), pick of the litter is the `sleep N | nc …` form
        // documented below + explicit `kill` at the end:
        //   1. `< /dev/null` — instant stdin EOF, nc resets the
        //      socket after ~3.7 MiB (RST observed in dev).
        //   2. `< /dev/zero` — never EOFs, but busybox nc is single-
        //      threaded; nc's `read stdin → write socket` half
        //      blocks on socket-buffer pressure and the WHOLE
        //      select loop pauses (observed: host write stalls at
        //      ~10 MiB).
        //   3. `sleep N | nc -l …` — sleep keeps the pipe alive
        //      without ever sending data, so nc only exits when
        //      the SOCKET side closes (good). BUT after nc exits
        //      `sh` waits for sleep, adding ~N seconds of dead
        //      time. Fix: explicitly `kill` the exec from the host
        //      side after `shutdown_write` returns. SIGKILL on the
        //      sh process kills the pipeline group, so sleep dies
        //      with it and the exec.wait() unblocks immediately.
        // We pick a sleep duration that comfortably outlasts any
        // reasonable PAYLOAD_BYTES write (60s for 32 MiB is fine);
        // it never actually elapses on the happy path.
        let nc_cmd = BoxCommand::new("sh").args([
            "-c",
            &format!("sleep 60 | nc -l -p {GUEST_PORT} > /dev/null"),
        ]);
        let mut nc_exec = live.exec(nc_cmd).await.context("box.exec(nc -l)")?;

        // Drain nc's stdout/stderr in background tasks; otherwise
        // the unbounded channel would still buffer them but it's
        // cleaner not to leak the handles.
        if let Some(mut stdout) = nc_exec.stdout() {
            tokio::spawn(async move { while stdout.next().await.is_some() {} });
        }
        let stderr_task = nc_exec.stderr().map(|mut stderr| {
            tokio::spawn(async move {
                let mut buf = String::new();
                while let Some(chunk) = stderr.next().await {
                    buf.push_str(&chunk);
                }
                buf
            })
        });

        // Wait for nc to bind before we hit it. Without this the
        // first TcpStream::connect can race ahead and get
        // ECONNREFUSED.
        tokio::time::sleep(NC_BIND_WAIT).await;

        // Host-side write loop. 64 KiB chunks balance syscall
        // overhead against kernel socket buffer pressure on
        // slower hosts. Every host-side op is timeout-bounded so
        // a misbehaving gvproxy/nc surface as a clear error
        // instead of hanging the iteration indefinitely.
        let chunk = vec![0u8; 64 * 1024];
        let mut sent = 0usize;
        let write_start = Instant::now();
        let mut stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect(("127.0.0.1", host_port)),
        )
        .await
        .with_context(|| {
            format!(
                "connect timed out after {CONNECT_TIMEOUT:?} \
                 — gvproxy is bound on host:{host_port} but the \
                 forward to guest:{GUEST_PORT} isn't completing \
                 (likely nc didn't bind)"
            )
        })?
        .with_context(|| format!("connect to host:{host_port}"))?;
        let write_deadline = Instant::now() + WRITE_TIMEOUT;
        while sent < PAYLOAD_BYTES {
            let n = (PAYLOAD_BYTES - sent).min(chunk.len());
            let remaining = write_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!(
                    "host-side write stalled — sent {sent}/{PAYLOAD_BYTES} \
                     bytes within {WRITE_TIMEOUT:?}; gvproxy forward to \
                     guest:{GUEST_PORT} is not draining (nc may have died)"
                );
            }
            tokio::time::timeout(remaining, stream.write_all(&chunk[..n]))
                .await
                .with_context(|| {
                    format!(
                        "write_all stalled after sending {sent}/{PAYLOAD_BYTES} bytes \
                         (chunk size {n})"
                    )
                })?
                .with_context(|| format!("write_all chunk at offset {sent}"))?;
            sent += n;
        }
        stream
            .shutdown()
            .await
            .context("shutdown_write — signal EOF to nc")?;
        let wall_ms = write_start.elapsed().as_secs_f64() * 1000.0;

        // SIGKILL the `sh -c "sleep N | nc …"` process group so the
        // sleep doesn't outlive nc. The host shutdown_write made nc
        // exit, but without this kill `sh` would still wait the
        // remaining ~N seconds for `sleep` before reaping the
        // pipeline. With the kill the exec returns immediately.
        let _ = nc_exec.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), nc_exec.wait()).await;
        if let Some(task) = stderr_task {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }

        live.stop().await.context("box.stop()")?;
        guard.disarm();

        let mut metrics = BTreeMap::new();
        if wall_ms > 0.0 {
            let mib = PAYLOAD_BYTES as f64 / (1024.0 * 1024.0);
            metrics.insert("net_tcp_mb_per_sec".into(), mib / (wall_ms / 1000.0));
        }
        metrics.insert("net_tcp_bytes".into(), PAYLOAD_BYTES as f64);
        metrics.insert("net_tcp_wall_ms".into(), wall_ms);
        Ok(metrics)
    }
}
