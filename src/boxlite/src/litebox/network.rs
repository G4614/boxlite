//! Network sub-resource on LiteBox.

use std::future::Future;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use crate::net::BoxInternalTunnel;
use crate::runtime::backend::BoxNetworkBackend;

/// A descriptor for a box service tunnel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoxEndpoint {
    /// A URL clients can use to reach a remote box service.
    Url(String),
    /// A local Unix socket that accepts a new connection for each client.
    UnixSocket(PathBuf),
}

/// Public byte-stream capability for a box service connection.
pub trait BoxConnection: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {
    /// Consume the connection and transfer its underlying descriptor to FFI callers.
    fn into_fd(self: Box<Self>) -> BoxliteResult<OwnedFd>;
}

impl BoxConnection for tokio::net::UnixStream {
    fn into_fd(self: Box<Self>) -> BoxliteResult<OwnedFd> {
        (*self)
            .into_std()
            .map(OwnedFd::from)
            .map_err(|error| BoxliteError::Network(format!("export tunnel fd: {error}")))
    }
}

impl BoxConnection for BoxInternalTunnel {
    fn into_fd(self: Box<Self>) -> BoxliteResult<OwnedFd> {
        (*self)
            .into_fd()
            .ok_or_else(|| BoxliteError::Unsupported("tunnel cannot be exported as an fd".into()))
    }
}

type ConnectFuture = Pin<Box<dyn Future<Output = BoxliteResult<Box<dyn BoxConnection>>> + Send>>;
type Connector = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

/// A reusable box service tunnel target. Creating it prepares a stable endpoint;
/// each [`connect`](Self::connect) call establishes a fresh connection.
pub struct BoxTunnel {
    endpoint: BoxEndpoint,
    connector: Connector,
}

impl BoxTunnel {
    pub(crate) fn new<F, Fut, C>(endpoint: BoxEndpoint, connect: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BoxliteResult<C>> + Send + 'static,
        C: BoxConnection + 'static,
    {
        let connector = Arc::new(move || {
            let future = connect();
            Box::pin(async move {
                future
                    .await
                    .map(|stream| Box::new(stream) as Box<dyn BoxConnection>)
            }) as ConnectFuture
        });
        Self {
            endpoint,
            connector,
        }
    }

    /// Return the stable endpoint prepared by [`NetworkHandle::tunnel`].
    pub async fn endpoint(&self) -> BoxliteResult<BoxEndpoint> {
        Ok(self.endpoint.clone())
    }

    /// Establish a new connection to this tunnel's endpoint.
    pub async fn connect(&self) -> BoxliteResult<Box<dyn BoxConnection>> {
        (self.connector)().await
    }
}

/// Handle for network operations on a LiteBox.
///
/// Obtained via `litebox.network()`. Owns backend handles and can be used
/// independently from the originating `LiteBox` borrow.
pub struct NetworkHandle {
    network_backend: Arc<dyn BoxNetworkBackend>,
}

impl NetworkHandle {
    pub(crate) fn new(network_backend: Arc<dyn BoxNetworkBackend>) -> Self {
        Self { network_backend }
    }

    /// Prepare a reusable tunnel endpoint without opening a data connection.
    pub async fn tunnel(&self, target: SocketAddr) -> BoxliteResult<BoxTunnel> {
        self.network_backend.tunnel(target).await
    }
}

#[cfg(test)]
mod tests {
    use boxlite_shared::errors::BoxliteError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use super::*;

    #[tokio::test]
    async fn remote_tunnel_opens_independent_streams_lazily() {
        let (peer_tx, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let tunnel = BoxTunnel::new(
            BoxEndpoint::Url("https://3000-box.proxy.example.test".to_string()),
            move || {
                let peer_tx = peer_tx.clone();
                async move {
                    let (stream, peer) = UnixStream::pair().map_err(|error| {
                        BoxliteError::Network(format!("test socket pair failed: {error}"))
                    })?;
                    peer_tx.send(peer).unwrap();
                    Ok(stream)
                }
            },
        );

        assert_eq!(
            tunnel.endpoint().await.unwrap(),
            BoxEndpoint::Url("https://3000-box.proxy.example.test".to_string())
        );
        assert!(peer_rx.try_recv().is_err(), "tunnel() must not connect");

        let mut first = tunnel.connect().await.unwrap();
        let mut first_peer = peer_rx.recv().await.unwrap();
        let mut second = tunnel.connect().await.unwrap();
        let mut second_peer = peer_rx.recv().await.unwrap();
        first_peer.write_all(b"one").await.unwrap();
        second_peer.write_all(b"two").await.unwrap();
        let mut first_response = [0; 3];
        let mut second_response = [0; 3];
        first.read_exact(&mut first_response).await.unwrap();
        second.read_exact(&mut second_response).await.unwrap();
        assert_eq!(&first_response, b"one");
        assert_eq!(&second_response, b"two");
    }

    #[tokio::test]
    async fn local_tunnel_exposes_reusable_unix_socket_lazily() {
        let (peer_tx, mut peer_rx) = tokio::sync::mpsc::unbounded_channel();
        let tunnel = BoxTunnel::new(
            BoxEndpoint::UnixSocket("/tmp/test-boxlite-tunnel.sock".into()),
            move || {
                let peer_tx = peer_tx.clone();
                async move {
                    let (stream, peer) = UnixStream::pair().map_err(|error| {
                        BoxliteError::Network(format!("test socket pair failed: {error}"))
                    })?;
                    peer_tx.send(peer).unwrap();
                    Ok(stream)
                }
            },
        );

        assert_eq!(
            tunnel.endpoint().await.unwrap(),
            BoxEndpoint::UnixSocket("/tmp/test-boxlite-tunnel.sock".into())
        );
        assert!(peer_rx.try_recv().is_err(), "tunnel() must not connect");

        let mut first = tunnel.connect().await.unwrap();
        let mut first_peer = peer_rx.recv().await.unwrap();
        let mut second = tunnel.connect().await.unwrap();
        let mut second_peer = peer_rx.recv().await.unwrap();
        first_peer.write_all(b"one").await.unwrap();
        second_peer.write_all(b"two").await.unwrap();
        let mut first_response = [0; 3];
        let mut second_response = [0; 3];
        first.read_exact(&mut first_response).await.unwrap();
        second.read_exact(&mut second_response).await.unwrap();
        assert_eq!(&first_response, b"one");
        assert_eq!(&second_response, b"two");
    }
}
