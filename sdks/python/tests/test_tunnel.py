from __future__ import annotations

import socket

import pytest

from boxlite.simplebox import SimpleBox


class _FakeTunnel:
    def __init__(self, fds: list[int], endpoint: str) -> None:
        self.fds = fds
        self.endpoint_value = endpoint

    async def endpoint(self) -> str:
        return self.endpoint_value

    async def connect_fd(self) -> int:
        return self.fds.pop(0)


class _FakeNetwork:
    def __init__(self, tunnel: _FakeTunnel) -> None:
        self.tunnel_value = tunnel
        self.ports: list[int] = []

    async def tunnel(self, port: int) -> _FakeTunnel:
        self.ports.append(port)
        return self.tunnel_value


class _FakeBox:
    def __init__(self, tunnel: _FakeTunnel) -> None:
        self.network = _FakeNetwork(tunnel)


@pytest.mark.asyncio
async def test_endpoint_returns_stable_unix_socket_path():
    box = SimpleBox.__new__(SimpleBox)
    box._started = True
    box._box = _FakeBox(_FakeTunnel([], "/tmp/boxlite/service.sock"))

    tunnel = await box.network.tunnel(3000)
    assert await tunnel.endpoint() == "/tmp/boxlite/service.sock"
    assert box._box.network.ports == [3000]


@pytest.mark.asyncio
async def test_connect_opens_fresh_sockets():
    first_local, first_peer = socket.socketpair()
    second_local, second_peer = socket.socketpair()
    box = SimpleBox.__new__(SimpleBox)
    box._started = True
    box._box = _FakeBox(
        _FakeTunnel([first_local.detach(), second_local.detach()], "unused")
    )

    tunnel = await box.network.tunnel(3000)
    first = await tunnel.connect()
    second = await tunnel.connect()
    try:
        first.sendall(b"one")
        second.sendall(b"two")
        assert first_peer.recv(3) == b"one"
        assert second_peer.recv(3) == b"two"
        assert first is not second
    finally:
        first.close()
        second.close()
        first_peer.close()
        second_peer.close()


@pytest.mark.asyncio
async def test_tunnel_requires_a_started_box():
    box = SimpleBox.__new__(SimpleBox)
    box._started = False

    with pytest.raises(RuntimeError, match="Box not started"):
        await box.network.tunnel(3000)
