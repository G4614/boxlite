//! Network sub-resource on LiteBox.

use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use boxlite_shared::errors::BoxliteResult;

use crate::net::BoxInternalTunnel;
use crate::runtime::backend::BoxNetworkBackend;

/// A descriptor for a box service tunnel.
pub enum BoxEndpoint {
    /// A URL clients can use to reach a remote box service.
    Url(String),
    /// An already-connected local tunnel socket.
    Fd(OwnedFd),
}

impl BoxEndpoint {
    fn try_clone(&self) -> BoxliteResult<Self> {
        match self {
            Self::Url(url) => Ok(Self::Url(url.clone())),
            Self::Fd(fd) => fd
                .try_clone()
                .map(Self::Fd)
                .map_err(|error| BoxliteError::Network(format!("clone local tunnel fd: {error}"))),
        }
    }
}

/// Public byte-stream capability for a box service connection.
pub trait BoxConnection: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

impl<T> BoxConnection for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

/// A box service tunnel target. [`NetworkHandle::tunnel`] establishes its
/// transport; callers can inspect its [`endpoint`](Self::endpoint) or consume
/// it through [`open_stream`](Self::open_stream).
pub struct BoxTunnel {
    endpoint: Arc<tokio::sync::Mutex<Option<BoxEndpoint>>>,
    stream: Arc<tokio::sync::Mutex<Option<BoxInternalTunnel>>>,
}

impl BoxTunnel {
    pub(crate) fn new(endpoint: Option<BoxEndpoint>, stream: Option<BoxInternalTunnel>) -> Self {
        Self {
            endpoint: Arc::new(tokio::sync::Mutex::new(endpoint)),
            stream: Arc::new(tokio::sync::Mutex::new(stream)),
        }
    }

    /// Return a clone of the endpoint prepared by [`NetworkHandle::tunnel`].
    pub async fn endpoint(&self) -> BoxliteResult<BoxEndpoint> {
        self.endpoint
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| {
                BoxliteError::InvalidState("tunnel endpoint has already been consumed".into())
            })?
            .try_clone()
    }

    /// Consume the transport established by [`NetworkHandle::tunnel`].
    pub async fn open_stream(&self) -> BoxliteResult<Box<dyn BoxConnection>> {
        let fd = {
            let mut endpoint = self.endpoint.lock().await;
            match endpoint.as_ref() {
                Some(BoxEndpoint::Fd(_)) => match endpoint.take() {
                    Some(BoxEndpoint::Fd(fd)) => Some(fd),
                    _ => unreachable!("endpoint variant changed while locked"),
                },
                _ => None,
            }
        };
        if let Some(fd) = fd {
            let stream = std::os::unix::net::UnixStream::from(fd);
            stream.set_nonblocking(true).map_err(|error| {
                BoxliteError::Network(format!("configure local tunnel fd: {error}"))
            })?;
            let stream = tokio::net::UnixStream::from_std(stream).map_err(|error| {
                BoxliteError::Network(format!("adopt local tunnel fd: {error}"))
            })?;
            return Ok(Box::new(stream));
        }

        self.stream
            .lock()
            .await
            .take()
            .map(|stream| Box::new(stream) as Box<dyn BoxConnection>)
            .ok_or_else(|| {
                BoxliteError::InvalidState("tunnel stream has already been consumed".into())
            })
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

    /// Establish a tunnel target, returning a [`BoxTunnel`] with a URL or FD
    /// endpoint and an already-prepared raw stream.
    ///
    /// This is the single tunnel entry point: callers that only want the raw
    /// stream use the SDK-specific `open_stream()` wrapper.
    pub async fn tunnel(&self, target: SocketAddr) -> BoxliteResult<BoxTunnel> {
        self.network_backend.tunnel(target).await
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use super::*;

    struct TestBackend {
        peer: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
    }

    #[async_trait::async_trait]
    impl BoxNetworkBackend for TestBackend {
        async fn tunnel(&self, _target: SocketAddr) -> BoxliteResult<BoxTunnel> {
            let (stream, peer) = UnixStream::pair().map_err(|error| {
                BoxliteError::Network(format!("test socket pair failed: {error}"))
            })?;
            *self.peer.lock().await = Some(peer);
            Ok(BoxTunnel::new(
                Some(BoxEndpoint::Url(
                    "https://3000-box.proxy.example.test".to_string(),
                )),
                Some(BoxInternalTunnel::from_local(
                    stream,
                    "192.168.127.2:3000".parse().unwrap(),
                )),
            ))
        }
    }

    #[tokio::test]
    async fn remote_box_prepares_url_and_stream_in_tunnel() {
        let peer = Arc::new(tokio::sync::Mutex::new(None));
        let backend = Arc::new(TestBackend {
            peer: Arc::clone(&peer),
        });
        let network = NetworkHandle::new(backend.clone());
        let target = "192.168.127.2:3000".parse().unwrap();

        let tunnel = network.tunnel(target).await.unwrap();
        let endpoint = tunnel.endpoint().await.unwrap();
        assert!(
            matches!(endpoint, BoxEndpoint::Url(url) if url == "https://3000-box.proxy.example.test")
        );
        let mut stream = tunnel.open_stream().await.unwrap();
        let mut peer = peer.lock().await.take().unwrap();
        peer.write_all(b"remote").await.unwrap();
        let mut response = [0; 6];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"remote");
        assert!(matches!(
            tunnel.endpoint().await.unwrap(),
            BoxEndpoint::Url(url) if url == "https://3000-box.proxy.example.test"
        ));
    }

    struct LocalBackend {
        peer: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
    }

    #[async_trait::async_trait]
    impl BoxNetworkBackend for LocalBackend {
        async fn tunnel(&self, _target: SocketAddr) -> BoxliteResult<BoxTunnel> {
            let (stream, peer) = UnixStream::pair().map_err(|error| {
                BoxliteError::Network(format!("test socket pair failed: {error}"))
            })?;
            *self.peer.lock().await = Some(peer);
            let fd = BoxInternalTunnel::from_local(stream, "192.168.127.2:3000".parse().unwrap())
                .into_fd()
                .ok_or_else(|| BoxliteError::Network("local tunnel did not expose an fd".into()))?;
            Ok(BoxTunnel::new(Some(BoxEndpoint::Fd(fd)), None))
        }
    }

    #[tokio::test]
    async fn local_box_prepares_an_fd_in_tunnel() {
        let peer = Arc::new(tokio::sync::Mutex::new(None));
        let network = NetworkHandle::new(Arc::new(LocalBackend {
            peer: Arc::clone(&peer),
        }));
        let target = "192.168.127.2:3000".parse().unwrap();

        let tunnel = network.tunnel(target).await.unwrap();
        let endpoint = tunnel.endpoint().await.unwrap();
        assert!(matches!(endpoint, BoxEndpoint::Fd(_)));
        let mut stream = tunnel.open_stream().await.unwrap();
        let mut peer = peer.lock().await.take().unwrap();

        peer.write_all(b"local").await.unwrap();
        let mut response = [0; 5];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"local");
    }
}
