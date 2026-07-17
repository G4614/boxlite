"""REST E2E coverage for the Python SDK's public network handle."""

from __future__ import annotations

import asyncio

import boxlite
import pytest


GUEST_PORT = 18080
MARKER = b"python-sdk-tunnel-e2e"


async def _get_over_tunnel(box: boxlite.SimpleBox) -> bytes:
    tunnel = await box.network.tunnel(GUEST_PORT)
    connection = await tunnel.connect()
    loop = asyncio.get_running_loop()
    response = bytearray()
    try:
        await loop.sock_sendall(
            connection,
            b"GET /tunnel-e2e-python.txt HTTP/1.0\r\nHost: tunnel.test\r\n\r\n",
        )
        while len(response) < 64 * 1024:
            chunk = await asyncio.wait_for(loop.sock_recv(connection, 8192), timeout=5)
            if not chunk:
                break
            response.extend(chunk)
            if MARKER in response:
                break
        return bytes(response)
    finally:
        connection.close()


async def _wait_for_http(box: boxlite.SimpleBox) -> bytes:
    deadline = asyncio.get_running_loop().time() + 30
    last_error: Exception | None = None
    while asyncio.get_running_loop().time() < deadline:
        try:
            response = await _get_over_tunnel(box)
            if MARKER in response:
                return response
            last_error = AssertionError(f"unexpected HTTP response: {response!r}")
        except (OSError, RuntimeError, asyncio.TimeoutError) as exc:
            last_error = exc
        await asyncio.sleep(0.25)
    raise AssertionError(f"guest HTTP service was not reachable through tunnel: {last_error}")


@pytest.mark.asyncio
async def test_python_sdk_tunnel_proxies_http_from_rest_box(rt, image):
    """SDK -> REST API -> runner -> gvproxy tunnel returns the guest HTTP body."""
    async with boxlite.SimpleBox(image=image, runtime=rt, auto_remove=True) as box:
        server = await box.exec(
            "sh",
            "-lc",
            (
                f"printf '%s\\n' '{MARKER.decode()}' > /root/tunnel-e2e-python.txt; "
                f"python3 -m http.server {GUEST_PORT} --bind 0.0.0.0 --directory /root "
                ">/tmp/tunnel-e2e-python.log 2>&1 &"
            ),
        )
        assert server.exit_code == 0, server.stderr

        response = await _wait_for_http(box)
        assert response.startswith(b"HTTP/1.0 200") or response.startswith(b"HTTP/1.1 200")
        assert MARKER in response
