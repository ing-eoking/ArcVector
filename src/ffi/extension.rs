//! Extension entry point invoked when memcached loads this library,
//! and ASCII protocol extension callbacks (`accept`/`execute`/`abort`).

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::bindings::{
    self, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, EXTENSION_ERROR_CODE,
    EXTENSION_ERROR_CODE_EXTENSION_FATAL, EXTENSION_ERROR_CODE_EXTENSION_SUCCESS, GET_SERVER_API,
    extension_type_t_EXTENSION_ASCII_PROTOCOL, token_t,
};
use super::conn_state;
use super::server::{self, ResponseHandler};
use crate::protocol::request::{self, MAX_BODY_BYTES, Parsed};
use crate::protocol::tokens::Tokens;
use crate::protocol::{self, nread};
use crate::runtime::offload;

unsafe extern "C" fn accept_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) -> bool {
    let tokens = unsafe { Tokens::new(argv, argc) };

    let Ok(Parsed::Body {
        len,
        unknown_len,
        request,
    }) = request::parse(&tokens)
    else {
        return tokens.command().is_some();
    };
    // Refuse bodies exceeding the limit or with indeterminate length. `execute` will close the connection.
    if len > MAX_BODY_BYTES || unknown_len {
        return true;
    }

    unsafe {
        nread::expect_body(cookie, request, len, ndata, ptr_out);
    }
    true
}

fn should_close(len: usize, unknown_len: bool) -> Option<String> {
    if len > MAX_BODY_BYTES {
        Some(format!(
            "declaring a {len}-byte body (limit {MAX_BODY_BYTES})"
        ))
    } else if unknown_len {
        Some("with a segment length that could not be determined".to_owned())
    } else {
        None
    }
}

/// Executes the command. Replies are dispatched via the `on_io_complete` callback (`offload`).
///
/// Returns `true` on handled commands (including error responses). Returns `false` to instruct
/// the server core to close the connection (used only for undrainable bodies).
unsafe extern "C" fn execute_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: ResponseHandler,
) -> bool {
    let tokens = unsafe { Tokens::new(argv, argc) };

    // If the body is oversized or has indeterminate segments, the server core cannot
    // drain it safely. Responding normally would cause trailing body bytes to be parsed
    // as subsequent commands. Close the connection with a diagnostic warning.
    if let Ok(Parsed::Body {
        len, unknown_len, ..
    }) = request::parse(&tokens)
        && let Some(why) = should_close(len, unknown_len)
    {
        eprintln!("ArcVector: closing a connection that sent a body-carrying line {why}");
        return false;
    }

    match unsafe { protocol::parse(cookie, &tokens) } {
        Err(e) => unsafe { offload::reply(cookie, Err(e), handler) },
        Ok(request) => unsafe { offload::begin(cookie, request, handler) },
    }
    true
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    drop(unsafe { conn_state::take(cookie) });
}

unsafe extern "C" fn get_name_vector(_cmd_cookie: *const c_void) -> *const c_char {
    c"arcus-vector-db".as_ptr()
}

/// Under SASL (`-S`), the server queries this authorization mask (`check_ascii_auth`).
/// All vector commands operate on index Maps and require Map-level authorization.
unsafe extern "C" fn get_auth_flag_vector() -> u16 {
    bindings::AUTHZ_MAP as u16
}

static mut VECTOR_DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR =
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
        get_name: Some(get_name_vector),
        get_auth_flag: Some(get_auth_flag_vector),
        accept: Some(accept_vector_cmd),
        execute: Some(execute_vector_cmd),
        abort: Some(abort_vector_cmd),
        cookie: ptr::null_mut(),
        next: ptr::null_mut(),
    };

#[unsafe(no_mangle)]
pub extern "C" fn memcached_extensions_initialize(
    _config: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    let Some(get_api) = get_server_api else {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    };
    server::set_api(get_api);

    unsafe {
        let server = get_api();
        if server.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
        match server::missing_core_members() {
            None => return EXTENSION_ERROR_CODE_EXTENSION_FATAL,
            Some(missing) if !missing.is_empty() => {
                eprintln!(
                    "ArcVector: the server core lacks {}; this extension needs an arcus-memcached EE build (vectest)",
                    missing.join(", ")
                );
                return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
            }
            Some(_) => {}
        }
        let extension = (*server).extension;
        if extension.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
        let Some(register) = (*extension).register_extension else {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        };
        if !register(
            extension_type_t_EXTENSION_ASCII_PROTOCOL,
            (&raw mut VECTOR_DESCRIPTOR).cast::<c_void>(),
        ) {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
    }

    // Install item lifecycle hooks. Called during extension initialization before engine
    // initialization, ensuring initial links during persistence recovery are captured.
    crate::trigger::event::install();

    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_oversize_or_undeterminable_body_closes_the_connection() {
        assert!(should_close(MAX_BODY_BYTES + 1, false).is_some());
        assert!(should_close(0, true).is_some());
        assert!(should_close(30, true).is_some());
        assert!(should_close(30, false).is_none());
        assert!(should_close(MAX_BODY_BYTES, false).is_none());
    }
}
