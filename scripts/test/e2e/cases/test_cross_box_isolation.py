"""E2E P0 (multi-tenant isolation): one box cannot read another box's files.

Requirement (Sandbox platform §6.1 / §12.4): different sandboxes are isolated
— box B must not be able to read, list, or otherwise reach box A's filesystem.
Each box is a separate microVM with its own root filesystem, so a path written
in A must simply not exist in B.

This is the negative security assertion the suite was missing: it writes a
unique marker into box A's rootfs and proves box B (created independently)
cannot see the marker, the file, or A's data directory.
"""

from __future__ import annotations

import asyncio

import pytest
from conftest import drain

import boxlite

MARKER = "tenant-A-only-4c7e21"
SECRET_PATH = "/root/tenant_a_secret.txt"


async def _run(box, cmd: str) -> tuple[int, str, str]:
    ex = await box.exec("sh", ["-c", cmd], None)
    out, err = await drain(ex)
    rc = await asyncio.wait_for(ex.wait(), timeout=30)
    return rc.exit_code, out, err


@pytest.mark.asyncio
async def test_box_b_cannot_read_box_a_filesystem(rt, image):
    a = await rt.create(boxlite.BoxOptions(image=image, auto_remove=True))
    b = await rt.create(boxlite.BoxOptions(image=image, auto_remove=True))
    try:
        # A writes a private marker into its own rootfs.
        rc, _, err = await _run(a, f"printf '%s' '{MARKER}' > {SECRET_PATH} && sync")
        assert rc == 0, f"box A failed to write its secret: rc={rc} stderr={err!r}"

        # Sanity: A can read it back (proves the write is real, not a no-op).
        rc, out, _ = await _run(a, f"cat {SECRET_PATH}")
        assert rc == 0 and MARKER in out, f"box A cannot read its own secret: {out!r}"

        # B must NOT see A's file at all.
        rc, out, _ = await _run(b, f"cat {SECRET_PATH} 2>/dev/null; echo RC=$?")
        assert MARKER not in out, (
            f"ISOLATION BREACH: box B read box A's secret file → {out!r}"
        )
        assert "RC=0" not in out, (
            f"ISOLATION BREACH: {SECRET_PATH} is readable inside box B → {out!r}"
        )

        # And the marker must not appear anywhere reachable from B's rootfs.
        # Exclude /proc and /sys: grep -rl over their virtual files (e.g.
        # /proc/kcore) can stall past the exec timeout. `grep -rl` prints
        # matching *paths*, so any path line at all is a breach — the only
        # acceptable output is the DONE sentinel on its own line.
        rc, out, _ = await _run(
            b,
            f"timeout 20 grep -rl '{MARKER}' --exclude-dir=proc --exclude-dir=sys "
            "--exclude-dir=dev --exclude-dir=run --exclude-dir=tmp / "
            "2>/dev/null | head -n1; echo DONE",
        )
        lines = out.strip().splitlines()
        assert lines == ["DONE"], f"ISOLATION BREACH: marker found in box B: {out!r}"
    finally:
        await rt.remove(a.id, force=True)
        await rt.remove(b.id, force=True)
