//! POL-32: `boxlite pull` and `boxlite images` over REST. The CLI
//! talks to a tiny std-only stub serving the two new endpoints from
//! `openapi/box.openapi.yaml` (`POST /v1/images/pull`, `GET /v1/images`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

struct Stub {
    port: u16,
}

impl Stub {
    fn start<H>(handler: H) -> Self
    where
        H: Fn(&str, &str, &[u8]) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(peek) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(peek);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => {
                            let lower = line.to_ascii_lowercase();
                            if let Some(rest) = lower.strip_prefix("content-length:") {
                                content_length = rest.trim().parse().unwrap_or(0);
                            }
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut body);
                }

                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let raw_path = parts.next().unwrap_or("");
                let path = raw_path.split('?').next().unwrap_or(raw_path);

                let (status, resp_body) = handler(method, path, &body);
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    404 => "Not Found",
                    422 => "Unprocessable",
                    _ => "OK",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {len}\r\nConnection: close\r\n\r\n{resp_body}",
                    len = resp_body.len(),
                );
                let _ = stream.flush();
            }
        });
        Self { port }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

fn cli(home: &TempDir) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("boxlite"));
    cmd.env("BOXLITE_HOME", home.path())
        .env_remove("BOXLITE_API_KEY")
        .env_remove("BOXLITE_REST_URL")
        .env_remove("BOXLITE_PROFILE")
        .timeout(Duration::from_secs(30));
    cmd
}

fn write_p1_creds(home: &TempDir, url: &str) {
    let body = format!(
        "[profiles.p1]\nurl = \"{url}\"\napi_key = \"k_test\"\nauth_method = \"api_key\"\n"
    );
    std::fs::write(home.path().join("credentials.toml"), body).unwrap();
}

const IMAGE_INFO_JSON: &str = r#"{
  "reference":"alpine:latest",
  "repository":"docker.io/library/alpine",
  "tag":"latest",
  "id":"sha256:1111222233334444555566667777888899990000aaaabbbbccccddddeeeeffff",
  "cached_at":"2026-06-01T00:00:00Z",
  "size_bytes":3145728
}"#;

/// `boxlite --profile p1 pull alpine:latest` POSTs to `/v1/images/pull`
/// with the requested reference and prints the wire metadata. The
/// stub asserts the request body shape so a regression that loses
/// the `reference` field (or sends GET-with-query, etc.) fails here.
#[test]
fn pull_routes_to_rest_and_prints_metadata() {
    let captured_body = Arc::new(std::sync::Mutex::new(String::new()));
    let captured = captured_body.clone();
    let stub = Stub::start(move |method, path, body| {
        if method == "POST" && path == "/v1/images/pull" {
            *captured.lock().unwrap() = String::from_utf8_lossy(body).to_string();
            (200, IMAGE_INFO_JSON.to_string())
        } else {
            (
                404,
                r#"{"error":{"message":"nope","type":"NotFoundError","code":"not_found"}}"#
                    .to_string(),
            )
        }
    });
    let home = TempDir::new().unwrap();
    write_p1_creds(&home, &stub.url());

    cli(&home)
        .args(["--profile", "p1", "pull", "alpine:latest"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Pulled: alpine:latest")
                .and(predicate::str::contains("sha256:1111")),
        );

    let body = captured_body.lock().unwrap().clone();
    assert!(
        body.contains("\"reference\":\"alpine:latest\"")
            || body.contains("\"reference\": \"alpine:latest\""),
        "request body must carry the reference; got: {body}"
    );
}

/// `--quiet` prints just the image id — useful for `IMAGE_ID=$(boxlite
/// --profile p1 pull -q alpine:latest)` automation.
#[test]
fn pull_quiet_prints_only_id() {
    let stub = Stub::start(|method, path, _body| {
        if method == "POST" && path == "/v1/images/pull" {
            (200, IMAGE_INFO_JSON.to_string())
        } else {
            (
                404,
                r#"{"error":{"message":"nope","type":"NotFoundError","code":"not_found"}}"#
                    .to_string(),
            )
        }
    });
    let home = TempDir::new().unwrap();
    write_p1_creds(&home, &stub.url());

    cli(&home)
        .args(["--profile", "p1", "pull", "-q", "alpine:latest"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("sha256:1111").and(predicate::str::contains("Pulled:").not()),
        );
}

/// `boxlite --profile p1 images` GETs `/v1/images` and renders the
/// list in the existing table shape — no separate REST renderer.
#[test]
fn images_list_routes_to_rest() {
    let calls = Arc::new(AtomicU32::new(0));
    let calls_in = calls.clone();
    let stub = Stub::start(move |method, path, _body| {
        if method == "GET" && path == "/v1/images" {
            calls_in.fetch_add(1, Ordering::SeqCst);
            (200, format!(r#"{{"images":[{IMAGE_INFO_JSON}]}}"#))
        } else {
            (
                404,
                r#"{"error":{"message":"nope","type":"NotFoundError","code":"not_found"}}"#
                    .to_string(),
            )
        }
    });
    let home = TempDir::new().unwrap();
    write_p1_creds(&home, &stub.url());

    cli(&home)
        .args(["--profile", "p1", "images"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("docker.io/library/alpine")
                .and(predicate::str::contains("latest")),
        );

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "list must hit /v1/images exactly once"
    );
}

/// Server-side failure (422 from pull) surfaces as a non-zero exit so
/// scripts can branch. The wire `image_pull_failed` code maps back to
/// `BoxliteError::Image`, which the CLI prints with its message.
#[test]
fn pull_propagates_server_failure() {
    let stub = Stub::start(|method, path, _body| {
        if method == "POST" && path == "/v1/images/pull" {
            (
                422,
                r#"{"error":{"message":"manifest unknown","type":"ImageError","code":"image_pull_failed"}}"#.to_string(),
            )
        } else {
            (
                404,
                r#"{"error":{"message":"nope","type":"NotFoundError","code":"not_found"}}"#
                    .to_string(),
            )
        }
    });
    let home = TempDir::new().unwrap();
    write_p1_creds(&home, &stub.url());

    cli(&home)
        .args(["--profile", "p1", "pull", "alpine:doesnotexist"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("manifest unknown"));
}
