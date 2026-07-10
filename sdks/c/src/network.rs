//! Network handle operations for the BoxLite C SDK.

use std::net::SocketAddr;
use std::os::raw::c_char;
use std::os::fd::IntoRawFd;
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
    out_fd: *mut i32,
    out_error: *mut CBoxliteError,
) -> BoxliteErrorCode {
    unsafe {
        if network.is_null() {
            write_error(out_error, null_pointer_error("network"));
            return BoxliteErrorCode::InvalidArgument;
        }
        if out_fd.is_null() {
            write_error(out_error, null_pointer_error("out_fd"));
            return BoxliteErrorCode::InvalidArgument;
        }
        *out_fd = -1;

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
        match network_ref
            .tokio_rt
            .block_on(network_ref.handle.network().tunnel(target))
        {
            Ok(tunnel) => match tunnel.into_fd() {
                Some(fd) => {
                    *out_fd = fd.into_raw_fd();
                    BoxliteErrorCode::Ok
                }
                None => {
                    write_error(
                        out_error,
                        BoxliteError::Unsupported(
                            "box network tunnel transport cannot be exported as fd".into(),
                        ),
                    );
                    BoxliteErrorCode::Unsupported
                }
            },
            Err(e) => {
                write_error(out_error, e);
                BoxliteErrorCode::Network
            }
        }
    }
}
