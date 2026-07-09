//! Network handle operations for the BoxLite C SDK.

use std::ffi::CString;
use std::net::SocketAddr;
use std::os::raw::c_char;
use std::ptr;
use std::sync::Arc;

use tokio::runtime::Runtime as TokioRuntime;

use boxlite::BoxliteError;
use boxlite::litebox::LiteBox;

use crate::error::{BoxliteErrorCode, null_pointer_error, write_error};
use crate::util::c_str_to_string;
use crate::{CBoxHandle, CBoxNetworkHandle, CBoxliteError};

/// Opaque handle for network operations on a box.
pub struct BoxNetworkHandle {
    handle: Arc<LiteBox>,
    tokio_rt: Arc<TokioRuntime>,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_box_network(
    handle: *mut CBoxHandle,
    out_network: *mut *mut CBoxNetworkHandle,
    out_error: *mut CBoxliteError,
) -> BoxliteErrorCode {
    unsafe {
        if handle.is_null() {
            write_error(out_error, null_pointer_error("handle"));
            return BoxliteErrorCode::InvalidArgument;
        }
        if out_network.is_null() {
            write_error(out_error, null_pointer_error("out_network"));
            return BoxliteErrorCode::InvalidArgument;
        }
        *out_network = ptr::null_mut();

        let handle_ref = &*handle;
        let network = Box::new(BoxNetworkHandle {
            handle: Arc::clone(&handle_ref.handle),
            tokio_rt: Arc::clone(&handle_ref.tokio_rt),
        });
        *out_network = Box::into_raw(network);
        BoxliteErrorCode::Ok
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_box_network_free(network: *mut CBoxNetworkHandle) {
    if !network.is_null() {
        unsafe {
            drop(Box::from_raw(network));
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_box_network_tunnel(
    network: *mut CBoxNetworkHandle,
    target_ip: *const c_char,
    target_port: u16,
    out_addr: *mut *mut c_char,
    out_error: *mut CBoxliteError,
) -> BoxliteErrorCode {
    unsafe {
        if network.is_null() {
            write_error(out_error, null_pointer_error("network"));
            return BoxliteErrorCode::InvalidArgument;
        }
        if out_addr.is_null() {
            write_error(out_error, null_pointer_error("out_addr"));
            return BoxliteErrorCode::InvalidArgument;
        }
        *out_addr = ptr::null_mut();

        let ip = match c_str_to_string(target_ip) {
            Ok(s) => s,
            Err(e) => {
                write_error(out_error, e);
                return BoxliteErrorCode::InvalidArgument;
            }
        };

        let target: SocketAddr = match format!("{ip}:{target_port}").parse() {
            Ok(target) => target,
            Err(e) => {
                write_error(
                    out_error,
                    BoxliteError::InvalidArgument(format!("invalid tunnel target: {e}")),
                );
                return BoxliteErrorCode::InvalidArgument;
            }
        };

        let network_ref = &*network;
        let listener = match std::net::TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => listener,
            Err(e) => {
                write_error(
                    out_error,
                    BoxliteError::Network(format!("failed to bind local tunnel listener: {e}")),
                );
                return BoxliteErrorCode::Network;
            }
        };
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(e) => {
                write_error(
                    out_error,
                    BoxliteError::Network(format!("failed to inspect local tunnel listener: {e}")),
                );
                return BoxliteErrorCode::Network;
            }
        };
        if let Err(e) = listener.set_nonblocking(true) {
            write_error(
                out_error,
                BoxliteError::Network(format!("failed to configure local tunnel listener: {e}")),
            );
            return BoxliteErrorCode::Network;
        }
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(e) => {
                write_error(
                    out_error,
                    BoxliteError::Network(format!("failed to create async tunnel listener: {e}")),
                );
                return BoxliteErrorCode::Network;
            }
        };

        let handle = Arc::clone(&network_ref.handle);
        network_ref.tokio_rt.spawn(async move {
            let accepted =
                tokio::time::timeout(std::time::Duration::from_secs(30), listener.accept()).await;
            if let Ok(Ok((mut client, _))) = accepted
                && let Ok(mut tunnel) = handle.network().tunnel(target).await
            {
                let _ = tokio::io::copy_bidirectional(&mut client, &mut tunnel).await;
            }
        });

        match CString::new(addr.to_string()) {
            Ok(addr) => {
                *out_addr = addr.into_raw();
                BoxliteErrorCode::Ok
            }
            Err(e) => {
                write_error(
                    out_error,
                    BoxliteError::Internal(format!("failed to encode tunnel address: {e}")),
                );
                BoxliteErrorCode::Internal
            }
        }
    }
}
