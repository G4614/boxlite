//! Real backup round-trip through MinIO.
//!
//! Proves the claim that backing a box up to S3-compatible object storage
//! needs nothing inside boxlite: export produces a file, any S3 client moves
//! it, and import reads it back. The archive crosses a real MinIO server —
//! uploaded, deleted locally, re-downloaded — before being imported and run.
//!
//! Requires a MinIO reachable at `BOXLITE_TEST_S3_ENDPOINT` (default
//! http://127.0.0.1:29000) with the bucket `BOXLITE_TEST_S3_BUCKET`
//! (default boxlite-backup). Skips itself when that is absent, so it never
//! fails a normal test run.

mod common;

use boxlite::runtime::options::{BoxliteOptions, ExportOptions};
use boxlite::{BoxCommand, BoxliteRuntime};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn endpoint() -> String {
    std::env::var("BOXLITE_TEST_S3_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:29000".to_string())
}

fn bucket() -> String {
    std::env::var("BOXLITE_TEST_S3_BUCKET").unwrap_or_else(|_| "boxlite-backup".to_string())
}

/// Run the aws CLI against the MinIO endpoint.
fn aws(args: &[&str]) -> std::process::Output {
    Command::new("aws")
        .env("AWS_ACCESS_KEY_ID", "minioadmin")
        .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .arg("--endpoint-url")
        .arg(endpoint())
        .args(args)
        .output()
        .expect("run aws cli")
}

fn minio_available() -> bool {
    let out = aws(&["s3", "ls", &format!("s3://{}", bucket())]);
    out.status.success()
}

fn sha256(path: &Path) -> String {
    let mut file = std::fs::File::open(path).expect("open archive for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("read archive for hashing");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hex::encode(hasher.finalize())
}

#[tokio::test]
async fn a_box_survives_a_round_trip_through_minio() {
    if !minio_available() {
        eprintln!("skipping: no MinIO bucket {} at {}", bucket(), endpoint());
        return;
    }

    let home = boxlite_test_utils::home::PerTestBoxHome::new();
    let runtime = BoxliteRuntime::new(BoxliteOptions {
        home_dir: home.path.clone(),
        image_registries: common::test_registries(),
    })
    .expect("create runtime");

    // A box carrying a marker only a genuine restore can reproduce.
    let source = runtime
        .create(common::alpine_opts(), Some("minio-src".to_string()))
        .await
        .expect("create box");
    source.start().await.expect("start");
    let marker = "backed-up-through-minio";
    let cmd = BoxCommand::new("sh").args(["-c", &format!("echo {marker} > /root/marker")]);
    source.exec(cmd).await.expect("exec").wait().await.ok();
    source.stop().await.expect("stop");

    // Export, then hand the file to object storage and forget it locally.
    let export_dir = TempDir::new_in("/tmp").unwrap();
    let archive = source
        .export(ExportOptions::default(), export_dir.path())
        .await
        .expect("export");
    let local_digest = sha256(archive.path());
    let size = std::fs::metadata(archive.path()).unwrap().len();
    let key = format!("s3://{}/minio-roundtrip.boxlite", bucket());

    let up = aws(&["s3", "cp", &archive.path().to_string_lossy(), &key]);
    assert!(
        up.status.success(),
        "upload failed: {}",
        String::from_utf8_lossy(&up.stderr)
    );
    std::fs::remove_file(archive.path()).expect("drop the local archive");
    assert!(!archive.path().exists(), "the archive must be gone locally");

    // Pull it back from MinIO and require the bytes to be identical.
    let restore_dir = TempDir::new_in("/tmp").unwrap();
    let restored = restore_dir.path().join("restored.boxlite");
    let down = aws(&["s3", "cp", &key, &restored.to_string_lossy()]);
    assert!(
        down.status.success(),
        "download failed: {}",
        String::from_utf8_lossy(&down.stderr)
    );
    assert_eq!(
        sha256(&restored),
        local_digest,
        "MinIO must return the archive byte-for-byte"
    );
    println!("\n=== archive crossed MinIO: {size} bytes, sha256 {local_digest} ===");

    // Import the downloaded archive and prove the box actually works.
    let imported = runtime
        .import_box(
            boxlite::runtime::options::BoxArchive::new(restored),
            Some("minio-restored".to_string()),
        )
        .await
        .expect("import the archive fetched from MinIO");

    imported.start().await.expect("start the restored box");
    let read_back = BoxCommand::new("cat").args(["/root/marker"]);
    let mut exec = imported
        .exec(read_back)
        .await
        .expect("exec on restored box");

    let mut stdout = String::new();
    if let Some(mut stream) = exec.stdout() {
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            stdout.push_str(&chunk);
        }
    }
    let result = exec.wait().await.expect("wait");
    assert_eq!(result.exit_code, 0, "reading the marker must succeed");
    assert_eq!(
        stdout.trim(),
        marker,
        "restored box must carry the marker written before backup"
    );
    println!(
        "=== restored box returned the marker: {:?} ===\n",
        stdout.trim()
    );

    imported.stop().await.expect("stop");
    let _ = aws(&["s3", "rm", &key]);
    let _ = runtime.shutdown(Some(common::TEST_SHUTDOWN_TIMEOUT)).await;
}
