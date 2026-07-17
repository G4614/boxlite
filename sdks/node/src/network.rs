use std::net::SocketAddr;
use std::os::fd::IntoRawFd;
use std::sync::Arc;

use boxlite::LiteBox;
use boxlite::litebox::{BoxEndpoint, BoxTunnel};
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::util::map_err;

/// Handle for network operations on a box.
#[napi]
pub struct JsNetworkHandle {
    pub(crate) handle: Arc<LiteBox>,
}

/// A reusable tunnel to one service port in a box.
#[napi]
pub struct JsBoxTunnel {
    handle: Arc<BoxTunnel>,
}

#[napi]
impl JsNetworkHandle {
    #[napi]
    pub async fn tunnel(&self, port: u16) -> Result<JsBoxTunnel> {
        if port == 0 {
            return Err(Error::from_reason("tunnel port must be non-zero"));
        }
        let target: SocketAddr = format!("{}:{port}", boxlite::net::constants::GUEST_IP)
            .parse()
            .expect("BoxLite guest IP must be a valid socket address");
        let tunnel = self
            .handle
            .network()
            .tunnel(target)
            .await
            .map_err(map_err)?;
        Ok(JsBoxTunnel {
            handle: Arc::new(tunnel),
        })
    }
}

#[napi]
impl JsBoxTunnel {
    #[napi]
    pub async fn endpoint(&self) -> Result<String> {
        match self.handle.endpoint().await.map_err(map_err)? {
            BoxEndpoint::Url(url) => Ok(url),
            BoxEndpoint::UnixSocket(path) => Ok(path.to_string_lossy().into_owned()),
        }
    }

    /// Open a fresh stream and return its owned Unix file descriptor.
    #[napi(js_name = "connectFd")]
    pub async fn connect_fd(&self) -> Result<i32> {
        let connection = self.handle.connect().await.map_err(map_err)?;
        Ok(connection.into_fd().map_err(map_err)?.into_raw_fd())
    }
}
