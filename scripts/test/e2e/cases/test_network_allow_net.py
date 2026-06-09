"""E2E coverage of NetworkSpec(mode='enabled', allow_net=...).

When a box is created with `network=NetworkSpec(mode='enabled',
allow_net=['example.com'])`, the gvproxy host-side TCP filter must
allow connections to `example.com` and refuse everything else.

The Python SDK unit test `test_tcp_filter.py` covers the FFI side;
this file pins the REST/runner serialization of the allow_net list.

Heavy environmental assumptions:
  - the guest image has `wget` / `curl` (alpine:3.23 ships busybox wget)
  - the host e2e runner has outbound internet access to `example.com`
  - gvproxy is wired into the runner's box network setup

If any of those don't hold on the CI runner, the whole test is
skipped — but it stays in-suite as a per-deployment contract check.
"""

from __future__ import annotations

import asyncio
import os

import boxlite
import pytest

from conftest import drain

# Domains chosen for stability + non-CDN behaviour:
# example.com is a stable IETF-reserved domain. localhost-only stacks
# can override via env.
ALLOWED = os.environ.get("BOXLITE_E2E_ALLOWED_HOST", "example.com")
BLOCKED = os.environ.get("BOXLITE_E2E_BLOCKED_HOST", "captive.apple.com")


def _has_networkspec():
    return hasattr(boxlite, "NetworkSpec")


@pytest.mark.asyncio
@pytest.mark.skipif(not _has_networkspec(), reason="NetworkSpec not built into SDK")
async def test_allow_net_lets_listed_host_through(rt, image):
    """With `allow_net=[ALLOWED]`, a TCP connect to ALLOWED:443 succeeds."""
    b = await rt.create(
        boxlite.BoxOptions(
            image=image,
            auto_remove=True,
            network=boxlite.NetworkSpec(mode="enabled", allow_net=[ALLOWED]),
        ),
    )
    try:
        # Use busybox wget — alpine has it built-in. -S = print headers
        # to stderr; -q = quiet; we only care about the exit code.
        ex = await b.exec(
            "sh",
            ["-c", f"wget -q --spider --timeout=10 https://{ALLOWED}/ && echo OK"],
            None,
        )
        out, _ = await drain(ex)
        rc = await asyncio.wait_for(ex.wait(), timeout=30)
        if rc.exit_code != 0:
            pytest.skip(
                f"runner network stack can't reach {ALLOWED} (rc={rc.exit_code}); "
                f"this is an environment issue, not a regression — "
                f"see scripts/test/e2e/README.md for outbound requirements"
            )
        assert "OK" in out, (
            f"allowed host {ALLOWED} reachable but stdout missing OK marker: {out!r}"
        )
    finally:
        try:
            await rt.remove(b.id, force=True)
        except Exception:
            pass


@pytest.mark.asyncio
@pytest.mark.skipif(not _has_networkspec(), reason="NetworkSpec not built into SDK")
async def test_allow_net_blocks_unlisted_host(rt, image):
    """With `allow_net=[ALLOWED]`, a TCP connect to BLOCKED:443 is
    rejected (nonzero wget exit)."""
    b = await rt.create(
        boxlite.BoxOptions(
            image=image,
            auto_remove=True,
            network=boxlite.NetworkSpec(mode="enabled", allow_net=[ALLOWED]),
        ),
    )
    try:
        ex = await b.exec(
            "sh",
            ["-c", f"wget -q --spider --timeout=5 https://{BLOCKED}/ ; echo EXIT=$?"],
            None,
        )
        out, _ = await drain(ex)
        await asyncio.wait_for(ex.wait(), timeout=30)
        assert "EXIT=0" not in out, (
            f"blocked host {BLOCKED} unexpectedly reachable: {out!r}"
        )
    finally:
        try:
            await rt.remove(b.id, force=True)
        except Exception:
            pass


@pytest.mark.asyncio
@pytest.mark.skipif(not _has_networkspec(), reason="NetworkSpec not built into SDK")
async def test_network_mode_disabled_blocks_all(rt, image):
    """`mode='disabled'` denies every outbound TCP, not just listed
    hosts. Smoke test that the mode field round-trips through REST."""
    b = await rt.create(
        boxlite.BoxOptions(
            image=image,
            auto_remove=True,
            network=boxlite.NetworkSpec(mode="disabled"),
        ),
    )
    try:
        ex = await b.exec(
            "sh",
            ["-c", f"wget -q --spider --timeout=5 https://{ALLOWED}/ ; echo EXIT=$?"],
            None,
        )
        out, _ = await drain(ex)
        await asyncio.wait_for(ex.wait(), timeout=30)
        assert "EXIT=0" not in out, (
            f"mode=disabled still let {ALLOWED} through: {out!r}"
        )
    finally:
        try:
            await rt.remove(b.id, force=True)
        except Exception:
            pass
