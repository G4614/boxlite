"""E2E port of `src/boxlite/tests/mount_security.rs` (RO surface only).

Verifies a `read_only=True` volume:

  - shows up in `/proc/mounts` with the `ro` flag inside the guest
  - rejects writes (`echo > file` returns nonzero)

This is the day-1 RO contract; the GHSA-g6ww-w5j2-r7x3 remount-RW
attack is exhaustively covered at the FFI layer in
`sdks/python/tests/test_readonly_volume_remount.py` and the Rust
`mount_security.rs`, so we don't replay it here.

Runs against the e2e stack — the host bind-mount path must be a
directory the boxlite-runner systemd user can read.
"""

from __future__ import annotations

import asyncio
import os
import tempfile

import boxlite
import pytest

from conftest import drain


@pytest.mark.asyncio
async def test_readonly_volume_mount_flag_and_write_reject(rt, image):
    """RO host dir mounted into the guest:
       - `/proc/mounts` reports `ro,`
       - direct write inside the guest returns nonzero exit"""
    with tempfile.TemporaryDirectory(prefix="boxlite_e2e_ro_") as host_dir:
        # World-readable so the runner user can see it without specific gid
        # permissions; on the e2e host this still respects fs RO once
        # mounted because the runtime enforces RO at the virtiofs side.
        os.chmod(host_dir, 0o755)
        with open(os.path.join(host_dir, "marker.txt"), "w") as f:
            f.write("host-original\n")

        b = await rt.create(
            boxlite.BoxOptions(
                image=image,
                auto_remove=True,
                volumes=[(host_dir, "/mnt/ro", True)],  # (host, guest, read_only)
            ),
        )
        try:
            # 1) Mount is reported as read-only in /proc/mounts.
            ex = await b.exec(
                "sh", ["-c", "grep ' /mnt/ro ' /proc/mounts || true"], None,
            )
            out, _ = await drain(ex)
            rc = await asyncio.wait_for(ex.wait(), timeout=30)
            assert rc.exit_code == 0, f"grep /proc/mounts failed: rc={rc.exit_code}"
            assert " ro," in out or out.endswith(" ro\n") or " ro " in out, (
                f"volume not mounted read-only in guest: {out!r}"
            )

            # 2) Write attempt fails.
            ex = await b.exec(
                "sh",
                ["-c", "echo guest-write > /mnt/ro/marker.txt 2>&1; echo EXIT=$?"],
                None,
            )
            out, _ = await drain(ex)
            await asyncio.wait_for(ex.wait(), timeout=30)
            # Look for explicit failure marker; redirection itself may
            # set the shell's $? to nonzero, but we still need the
            # echo EXIT=… to land in stdout.
            assert "EXIT=0" not in out, (
                f"write to read-only volume unexpectedly succeeded: {out!r}"
            )

            # 3) Host file unchanged.
            with open(os.path.join(host_dir, "marker.txt"), "r") as f:
                assert f.read() == "host-original\n", (
                    "host file mutated through RO mount"
                )
        finally:
            try:
                await rt.remove(b.id, force=True)
            except Exception:
                pass
