//! End-to-end deterministic reproducer of the bug PR #627 fixes
//! (`ws_terminal_probe_after_cut_must_not_drop_buffered_stdout`).
//!
//! The PR-bundled unit test mocks the WS server in-process. This test runs
//! the real production stack: a `boxlite serve` subprocess, and the
//! production `BoxliteRuntime::rest` REST client (which goes through
//! `attach_ws_pump` — the function PR #627 patches). A small TCP proxy
//! between them deterministically cuts the FIRST WebSocket upgrade after
//! the 101 response, forcing the client onto the `ProbeResult::Terminal`
//! short-circuit branch. The fix's re-attach is what lets the buffered
//! `"hello\n"` reach the consumer; without it stdout would silently be
//! empty.
//!
//! Two-sided in practice:
//!   - With PR #627 applied: the client re-attaches, the proxy lets the
//!     2nd WS through transparently, the runner replays its backlog,
//!     stdout collects `"hello\n"`, the test PASSES.
//!   - With PR #627 reverted (set `terminal_drain_attempted = true` at
//!     `litebox.rs:563`): the client bails on the Terminal probe, stdout
//!     is empty, the test FAILS with the assertion comparing `"" vs
//!     "hello\n"`. Verified locally.

#![allow(dead_code)]

use boxlite::runtime::options::{BoxOptions, RootfsSpec};
use boxlite::{BoxCommand, BoxliteRestOptions, BoxliteRuntime};
use boxlite_test_utils::TEST_REGISTRIES;
use boxlite_test_utils::home::PerTestBoxHome;
use std::net::TcpListener;
use std::process::{Child, Command as StdCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Helpers: free port, serve readiness, subprocess teardown
// ---------------------------------------------------------------------------

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

async fn wait_serve_ready(port: u16, timeout: Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/v1/config");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // System `curl` deliberately: proves the REST surface speaks to *any*
        // HTTP client, not just our own reqwest.
        let output = StdCommand::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "1",
                &url,
            ])
            .output();
        if let Ok(out) = output
            && out.status.success()
            && out.stdout == b"200"
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    false
}

/// `boxlite serve` subprocess wrapped in a `Drop` guard. SIGINT (not SIGTERM)
/// so the daemon hits its axum `with_graceful_shutdown` future and calls
/// `runtime.shutdown(timeout)` — stopping every running box before exit and
/// keeping `PerTestBoxHome::Drop`'s leak check happy.
struct ServeGuard {
    child: Child,
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TCP proxy that cuts the FIRST WebSocket upgrade
// ---------------------------------------------------------------------------

/// Proxy listens on its own port and forwards every byte to `target_port`,
/// EXCEPT the first time it sees a `Connection: Upgrade` / `Upgrade: websocket`
/// header pair. For that one connection it forwards the request, lets the
/// server send the `101 Switching Protocols` response back, then closes both
/// halves of the TCP pair — exactly the "proxy idle-cut on the upgrade
/// without any data frames" pattern that PR #627's mock test pins.
///
/// Subsequent connections — including the *second* WS upgrade that the fixed
/// client opens — are forwarded transparently. So:
///   - WS attach #1 → proxy cuts → client sees an immediate WS close.
///   - GET /v1/.../executions/{id} → HTTP, transparent → "completed" probe.
///   - WS attach #2 (only on the fixed path) → transparent → runner replays
///     `"hello\n"` + exit frame, client collects them.
async fn spawn_ws_cut_proxy(target_port: u16) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("proxy bind :0");
    let proxy_port = listener.local_addr().expect("proxy local_addr").port();
    let ws_seen = Arc::new(AtomicU64::new(0));

    let handle = tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let ws_seen = ws_seen.clone();
            tokio::spawn(async move {
                let _ = handle_proxy_conn(client, target_port, ws_seen).await;
            });
        }
    });
    (proxy_port, handle)
}

async fn handle_proxy_conn(
    client: TcpStream,
    target_port: u16,
    ws_seen: Arc<AtomicU64>,
) -> std::io::Result<()> {
    let server = TcpStream::connect(("127.0.0.1", target_port)).await?;
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();

    // Read just the request head (up to `\r\n\r\n`). We need it to decide if
    // this is a WS upgrade; everything after the head is opaque to us.
    let mut request_head = Vec::with_capacity(4096);
    let mut buf = [0u8; 1024];
    loop {
        let n = client_read.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        request_head.extend_from_slice(&buf[..n]);
        if request_head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if request_head.len() > 16384 {
            return Ok(()); // safety cap on malformed input
        }
    }
    let head_lower = String::from_utf8_lossy(&request_head).to_ascii_lowercase();
    let is_ws_upgrade =
        head_lower.contains("upgrade: websocket") && head_lower.contains("connection: ");

    // Forward the request head to the server unmodified.
    server_write.write_all(&request_head).await?;

    if !is_ws_upgrade {
        // Plain HTTP — fully transparent, copy both halves until either side
        // closes.
        let _ = tokio::join!(
            tokio::io::copy(&mut client_read, &mut server_write),
            tokio::io::copy(&mut server_read, &mut client_write),
        );
        return Ok(());
    }

    let is_first_ws = ws_seen.fetch_add(1, Ordering::SeqCst) == 0;

    if !is_first_ws {
        // 2nd+ WS upgrade — transparent. This is the path the fixed client's
        // re-attach uses, and it MUST work so the runner backlog can flow.
        let _ = tokio::join!(
            tokio::io::copy(&mut client_read, &mut server_write),
            tokio::io::copy(&mut server_read, &mut client_write),
        );
        return Ok(());
    }

    // First WS upgrade: forward the `101 Switching Protocols` response head
    // back, then close both halves. The client sees a valid upgrade followed
    // immediately by a WS close — same shape as a proxy idle-cut.
    let mut response_head = Vec::with_capacity(4096);
    loop {
        let n = server_read.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        response_head.extend_from_slice(&buf[..n]);
        if response_head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if response_head.len() > 16384 {
            break;
        }
    }
    let _ = client_write.write_all(&response_head).await;
    // Drop everything — the half-streams close their underlying socket on
    // drop, which is precisely the "cut" we're simulating.
    drop(client_write);
    drop(client_read);
    drop(server_write);
    drop(server_read);
    Ok(())
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boxlite_rest_attach_drain_after_proxy_cut_serves_stdout() {
    let server_home = PerTestBoxHome::new();
    let bin = env!("CARGO_BIN_EXE_boxlite");

    // 1. `boxlite serve` on an ephemeral port.
    let serve_port = pick_free_port();
    let mut serve_cmd = StdCommand::new(bin);
    serve_cmd
        .arg("--home")
        .arg(&server_home.path)
        .args(["serve", "--port"])
        .arg(serve_port.to_string())
        .args(["--host", "127.0.0.1"]);
    for reg in TEST_REGISTRIES {
        serve_cmd.arg("--registry").arg(reg);
    }
    serve_cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let child = serve_cmd.spawn().expect("spawn boxlite serve");
    let _serve_guard = ServeGuard { child };

    assert!(
        wait_serve_ready(serve_port, Duration::from_secs(30)).await,
        "boxlite serve never accepted GET /v1/config on 127.0.0.1:{serve_port}"
    );

    // 2. Proxy in front of serve. The client will talk to the proxy port; the
    // proxy cuts the first WS upgrade.
    let (proxy_port, proxy_handle) = spawn_ws_cut_proxy(serve_port).await;

    // 3. Production REST client (the path through `attach_ws_pump`).
    let opts = BoxliteRestOptions::new(format!("http://127.0.0.1:{proxy_port}"));
    let rt = BoxliteRuntime::rest(opts).expect("rest runtime");

    let box_opts = BoxOptions {
        rootfs: RootfsSpec::Image("alpine:latest".into()),
        auto_remove: true,
        ..Default::default()
    };
    let lb = rt
        .create(box_opts, None)
        .await
        .expect("create box via REST");
    let mut exec = lb
        .exec(BoxCommand::new("sh").args(["-c", "echo hello"]))
        .await
        .expect("exec echo via REST");

    // 4. Collect stdout. The first WS will be cut by the proxy. With PR
    // #627 applied the runtime re-attaches once (the proxy passes the 2nd
    // WS through transparently) and the runner's replay reaches us. Without
    // it, the pump short-circuits on the Terminal probe and this channel
    // never receives anything before closing.
    use futures::StreamExt;
    let mut stdout_stream = exec.stdout().expect("stdout handle");
    let mut collected = String::new();
    let collect = async {
        while let Some(chunk) = stdout_stream.next().await {
            collected.push_str(&chunk);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(60), collect).await;

    // 5. Wait for the exec result. Must time out cleanly rather than hang;
    // with the fix this completes in well under a second of the proxy cut.
    let result = tokio::time::timeout(Duration::from_secs(30), exec.wait())
        .await
        .expect("exec.wait() timed out — the pump never produced a terminal frame")
        .expect("exec.wait() error");

    assert_eq!(
        result.exit_code, 0,
        "echo hello should exit 0; got {}",
        result.exit_code
    );
    assert_eq!(
        collected, "hello\n",
        "stdout was dropped: the proxy cut the first WS attach, and without \
         PR #627's `terminal_drain_attempted` re-attach in `attach_ws_pump`, \
         the client bails on the Terminal status probe with empty stdout. \
         If this assertion fails reading `\"\"`, the fix has regressed."
    );

    proxy_handle.abort();
    let _ = proxy_handle.await;
}
