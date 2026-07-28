"""E2E P0 (network egress): the sandbox honours the outbound allow-list.

Requirement (Sandbox platform §5.5): a box must only reach hosts it is
allowed to. `NetworkSpec(mode="disabled")` blocks *all* egress;
`mode="enabled", allow_net=[...]` permits only the listed IPv4/CIDR/host.

This pins the *enforcement*, not just that the field is accepted: we open
a real TCP socket from inside the guest and assert blocked vs. allowed.
Uses raw IPv4 targets (no DNS) so the result reflects the L3/L4 filter,
not name resolution. The base image ships a Python runtime, so we drive
the probe with `python3`.
"""

from __future__ import annotations

import asyncio

import pytest
from conftest import drain

import boxlite

# Two stable, well-known anycast resolvers on :443. We never send data —
# a completed TCP handshake is the only signal we need.
ALLOWED_IP = "1.1.1.1"
BLOCKED_IP = "8.8.8.8"

# Prints exactly one line: CONNECT_OK or CONNECT_FAIL:<ExceptionName>.
_PROBE = (
    "import socket, sys\n"
    "host, port = sys.argv[1], int(sys.argv[2])\n"
    "s = socket.socket()\n"
    "s.settimeout(6)\n"
    "try:\n"
    "    s.connect((host, port))\n"
    "    print('CONNECT_OK')\n"
    "except Exception as e:\n"
    "    print('CONNECT_FAIL:%s' % type(e).__name__)\n"
    "finally:\n"
    "    s.close()\n"
)


async def _probe(box, ip: str) -> str:
    """Return the guest's CONNECT_OK / CONNECT_FAIL line for ip:443."""
    ex = await box.exec("python3", ["-c", _PROBE, ip, "443"], None)
    out, err = await drain(ex)
    rc = await asyncio.wait_for(ex.wait(), timeout=30)
    assert rc.exit_code == 0, f"probe process failed rc={rc.exit_code} stderr={err!r}"
    line = out.strip()
    assert line.startswith(("CONNECT_OK", "CONNECT_FAIL")), (
        f"unexpected probe output: {out!r}"
    )
    return line


@pytest.mark.asyncio
async def test_network_disabled_blocks_all_egress(rt, image):
    """`mode="disabled"` → the guest cannot open any outbound connection."""
    b = await rt.create(
        boxlite.BoxOptions(
            image=image,
            auto_remove=True,
            network=boxlite.NetworkSpec(mode="disabled"),
        )
    )
    try:
        result = await _probe(b, ALLOWED_IP)
        assert result.startswith("CONNECT_FAIL"), (
            f"egress must be blocked with network disabled, but guest reached "
            f"{ALLOWED_IP}:443 → {result!r}"
        )
    finally:
        await rt.remove(b.id, force=True)


@pytest.mark.asyncio
async def test_allowlist_permits_only_listed_host(rt, image):
    """`mode="enabled", allow_net=[ALLOWED_IP/32]` → ALLOWED_IP reachable,
    BLOCKED_IP refused. Proves the allow-list is an actual filter, not a
    passthrough."""
    b = await rt.create(
        boxlite.BoxOptions(
            image=image,
            auto_remove=True,
            network=boxlite.NetworkSpec(mode="enabled", allow_net=[f"{ALLOWED_IP}/32"]),
        )
    )
    try:
        allowed = await _probe(b, ALLOWED_IP)
        blocked = await _probe(b, BLOCKED_IP)
        assert allowed.startswith("CONNECT_OK"), (
            f"allow-listed host {ALLOWED_IP} must be reachable → {allowed!r}"
        )
        assert blocked.startswith("CONNECT_FAIL"), (
            f"non-allow-listed host {BLOCKED_IP} must be blocked → {blocked!r}"
        )
    finally:
        await rt.remove(b.id, force=True)
