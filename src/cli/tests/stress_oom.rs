//! Integration stress test: cgroup memory.max + pids.max prevent VM panic.
//!
//! 128MB VM + gcc install + 200×2MB fork+malloc+memset — without cgroup
//! this kills the VM 2/3 of the time. With cgroup it survives 100%.
//!
//! Requires: musl-gcc on the host (apt install musl-tools).

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::time::Duration;

fn compile_flood() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("boxlite-stress-bins");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("flood");
    if bin.exists() {
        return bin;
    }
    let src = dir.join("flood.c");
    std::fs::write(
        &src,
        r#"
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdio.h>
int main(int c, char **v) {
    int n = c > 1 ? atoi(v[1]) : 100;
    int mb = c > 2 ? atoi(v[2]) : 2;
    int i;
    for (i = 0; i < n; i++) {
        if (fork() == 0) {
            char *m = malloc(mb << 20);
            if (m) memset(m, 0x42, mb << 20);
            pause();
            _exit(0);
        }
    }
    printf("forked %d x %dMB\n", i, mb);
    fflush(stdout);
    pause();
}
"#,
    )
    .unwrap();
    let out = std::process::Command::new("musl-gcc")
        .args(["-static", "-o"])
        .arg(&bin)
        .arg(&src)
        .output()
        .expect("musl-gcc not found — apt install musl-tools");
    assert!(
        out.status.success(),
        "musl-gcc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    bin
}

fn base64_encode(path: &std::path::Path) -> String {
    use base64::Engine;
    let data = std::fs::read(path).unwrap();
    base64::engine::general_purpose::STANDARD.encode(&data)
}

/// 128MB VM, install gcc (eats cache), fork 200 children each memset 2MB.
/// Without cgroup this panics the VM ~2/3 of the time.
/// With cgroup memory.max + pids.max: 100% survival.
#[test]
fn stress_128mb_gcc_fork_200x2mb() {
    let home = PerTestBoxHome::new();
    let flood = compile_flood();
    let b64 = base64_encode(&flood);

    // Start 128MB box
    let out = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .args([
            "--home",
            home.path.to_str().unwrap(),
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "128",
            "alpine:latest",
            "sleep",
            "600",
        ])
        .timeout(Duration::from_secs(300))
        .output()
        .expect("failed to start box");
    assert!(out.status.success(), "box start failed");
    let box_id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // Install gcc (real-world memory pressure source)
    let out = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .args([
            "--home",
            home.path.to_str().unwrap(),
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            "apk add -q --no-cache gcc musl-dev 2>/dev/null && echo ok",
        ])
        .timeout(Duration::from_secs(120))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ok"), "gcc install failed");

    // Compile flood inside box
    let compile_cmd = format!(
        "cat > /tmp/f.c << 'CEOF'\n\
         #include <stdlib.h>\n#include <string.h>\n#include <unistd.h>\n#include <stdio.h>\n\
         int main(int c,char**v){{int n=c>1?atoi(v[1]):100,i;for(i=0;i<n;i++){{if(fork()==0){{char*m=malloc(2<<20);if(m)memset(m,66,2<<20);pause();_exit(0);}}}}\
         printf(\"forked %d\\n\",i);fflush(stdout);pause();}}\n\
         CEOF\n\
         gcc -o /tmp/f /tmp/f.c && echo compiled"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .args([
            "--home",
            home.path.to_str().unwrap(),
            "exec",
            &box_id,
            "--",
            "sh",
            "-c",
            &compile_cmd,
        ])
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("compiled"), "flood compile failed");

    // Run 200×2MB fork bomb
    let _ = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .args([
            "--home",
            home.path.to_str().unwrap(),
            "exec",
            &box_id,
            "--",
            "/tmp/f",
            "200",
        ])
        .timeout(Duration::from_secs(5))
        .output();

    std::thread::sleep(Duration::from_secs(15));

    // Assert box survived
    let out = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .args(["--home", home.path.to_str().unwrap(), "list"])
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&out.stdout);
    assert!(
        list.contains("Running"),
        "box must survive 128MB + gcc + 200×2MB fork bomb; got: {list}"
    );

    // Assert exec still works
    let out = Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .args([
            "--home",
            home.path.to_str().unwrap(),
            "exec",
            &box_id,
            "--",
            "echo",
            "alive",
        ])
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();
    assert!(out.status.success(), "exec must work after OOM pressure");
}
