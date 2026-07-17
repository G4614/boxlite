"""SyncNetworkHandle - synchronous network operations for a box."""

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from ._box import SyncBox

__all__ = ["SyncBoxTunnel", "SyncNetworkHandle"]


class SyncBoxTunnel:
    """Lazy synchronous tunnel handle for a box service port."""

    def __init__(self, box: "SyncBox", tunnel) -> None:
        self._box = box
        self._tunnel = tunnel

    def connect(self):
        """Open a blocking socket to the target service."""
        import socket

        fd = self._box._sync(self._tunnel.connect_fd())
        tunnel = socket.socket(fileno=fd)
        tunnel.setblocking(True)
        return tunnel

    def endpoint(self):
        """Return the cloud URL or local Unix socket path."""
        return self._box._sync(self._tunnel.endpoint())


class SyncNetworkHandle:
    """Synchronous wrapper for a box's network handle."""

    def __init__(self, box: "SyncBox") -> None:
        self._box = box

    def tunnel(self, port: int) -> SyncBoxTunnel:
        """Return a lazy tunnel handle for a port inside the box."""
        tunnel = self._box._sync(self._box._box.network.tunnel(port))
        return SyncBoxTunnel(self._box, tunnel)
