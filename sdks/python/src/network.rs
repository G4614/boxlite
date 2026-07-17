use std::net::SocketAddr;
use std::os::fd::IntoRawFd;
use std::sync::Arc;

use boxlite::LiteBox;
use boxlite::litebox::{BoxEndpoint, BoxTunnel};
use pyo3::prelude::*;

use crate::util::map_err;

/// Handle for network operations on a box.
#[pyclass(name = "NetworkHandle")]
pub(crate) struct PyNetworkHandle {
    pub(crate) handle: Arc<LiteBox>,
}

/// A reusable tunnel to one service port in a box.
#[pyclass(name = "BoxTunnel")]
pub(crate) struct PyBoxTunnel {
    handle: Arc<BoxTunnel>,
}

#[pymethods]
impl PyNetworkHandle {
    fn tunnel<'py>(&self, py: Python<'py>, port: u16) -> PyResult<Bound<'py, PyAny>> {
        if port == 0 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "tunnel port must be non-zero",
            ));
        }
        let handle = Arc::clone(&self.handle);
        let target: SocketAddr = format!("{}:{port}", boxlite::net::constants::GUEST_IP)
            .parse()
            .expect("BoxLite guest IP must be a valid socket address");
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let tunnel = handle.network().tunnel(target).await.map_err(map_err)?;
            Ok(PyBoxTunnel {
                handle: Arc::new(tunnel),
            })
        })
    }
}

#[pymethods]
impl PyBoxTunnel {
    fn endpoint<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            match handle.endpoint().await.map_err(map_err)? {
                BoxEndpoint::Url(url) => Ok(url),
                BoxEndpoint::UnixSocket(path) => Ok(path.to_string_lossy().into_owned()),
            }
        })
    }

    fn connect_fd<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let connection = handle.connect().await.map_err(map_err)?;
            Ok(connection.into_fd().map_err(map_err)?.into_raw_fd())
        })
    }
}
