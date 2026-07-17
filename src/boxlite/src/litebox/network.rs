//! Network sub-resource on LiteBox.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

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
pub trait BoxConnection: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

impl<T> BoxConnection for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

type ConnectFuture = Pin<Box<dyn Future<Output = BoxliteResult<Box<dyn BoxConnection>>> + Send>>;
type Connector = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

struct LocalEndpoint {
    _directory: tempfile::TempDir,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for LocalEndpoint {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// A reusable box service tunnel target. Creating it prepares a stable endpoint;
/// each [`open_stream`](Self::open_stream) call establishes a fresh connection.
pub struct BoxTunnel {
    endpoint: BoxEndpoint,
    connector: Connector,
    _local_endpoint: Option<LocalEndpoint>,
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
            _local_endpoint: None,
        }
    }

    pub(crate) async fn local<F, Fut, C>(connect: F) -> BoxliteResult<Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BoxliteResult<C>> + Send + 'static,
        C: BoxConnection + 'static,
    {
        let upstream: Connector = Arc::new(move || {
            let future = connect();
            Box::pin(async move {
                future
                    .await
                    .map(|stream| Box::new(stream) as Box<dyn BoxConnection>)
            })
        });
        let directory = tempfile::Builder::new()
            .prefix("boxlite-tunnel-")
            .tempdir()
            .map_err(|error| BoxliteError::Network(format!("create tunnel directory: {error}")))?;
        let path = directory.path().join("service.sock");
        let listener = tokio::net::UnixListener::bind(&path).map_err(|error| {
            BoxliteError::Network(format!("bind tunnel socket {}: {error}", path.display()))
        })?;
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                let upstream = Arc::clone(&upstream);
                tokio::spawn(async move {
                    match upstream().await {
                        Ok(mut service) => {
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut service).await;
                        }
                        Err(error) => {
                            tracing::debug!(%error, "local tunnel connection failed");
                        }
                    }
                });
            }
        });
        let connect_path = path.clone();
        Ok(Self {
            endpoint: BoxEndpoint::UnixSocket(path),
            connector: Arc::new(move || {
                let path = connect_path.clone();
                Box::pin(async move {
                    tokio::net::UnixStream::connect(&path)
                        .await
                        .map(|stream| Box::new(stream) as Box<dyn BoxConnection>)
                        .map_err(|error| {
                            BoxliteError::Network(format!(
                                "connect tunnel socket {}: {error}",
                                path.display()
                            ))
                        })
                })
            }),
            _local_endpoint: Some(LocalEndpoint {
                _directory: directory,
                accept_task,
            }),
        })
    }

    /// Return the stable endpoint prepared by [`NetworkHandle::tunnel`].
    pub async fn endpoint(&self) -> BoxliteResult<BoxEndpoint> {
        Ok(self.endpoint.clone())
    }

    /// Establish a new connection to this tunnel's endpoint.
    pub async fn open_stream(&self) -> BoxliteResult<Box<dyn BoxConnection>> {
        (self.connector)().await
    }

    /// Deprecated alias for [`open_stream`](Self::open_stream).
    #[deprecated(note = "use BoxTunnel::open_stream")]
    pub async fn connect(&self) -> BoxliteResult<Box<dyn BoxConnection>> {
        self.open_stream().await
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

        let mut first = tunnel.open_stream().await.unwrap();
        let mut first_peer = peer_rx.recv().await.unwrap();
        let mut second = tunnel.open_stream().await.unwrap();
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
        let tunnel = BoxTunnel::local(move || {
            let peer_tx = peer_tx.clone();
            async move {
                let (stream, peer) = UnixStream::pair().map_err(|error| {
                    BoxliteError::Network(format!("test socket pair failed: {error}"))
                })?;
                peer_tx.send(peer).unwrap();
                Ok(stream)
            }
        })
        .await
        .unwrap();

        let BoxEndpoint::UnixSocket(path) = tunnel.endpoint().await.unwrap() else {
            panic!("local tunnel must expose a Unix socket");
        };
        assert!(path.exists());
        assert!(peer_rx.try_recv().is_err(), "tunnel() must not connect");

        let mut first = tunnel.open_stream().await.unwrap();
        let mut first_peer = peer_rx.recv().await.unwrap();
        let mut second = tunnel.open_stream().await.unwrap();
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
