#![allow(clippy::missing_safety_doc)]
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    unnecessary_transmutes,
    clippy::all,
    clippy::pedantic
)]
pub mod engine_api {
    include!(concat!(env!("OUT_DIR"), "/engine_api.rs"));
}

pub mod bson;
pub mod command;
mod conn_state;
pub mod error;
pub mod handler;
pub mod room;
pub mod server;
pub mod trigger;

pub use handler::quant::Quant;
pub use handler::usearch::Metric;

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use command::nread;
use command::request::{self, MAX_BODY_BYTES, Parsed};
use command::tokens::Tokens;
use engine_api::{
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, EXTENSION_ERROR_CODE,
    EXTENSION_ERROR_CODE_EXTENSION_FATAL, EXTENSION_ERROR_CODE_EXTENSION_SUCCESS, GET_SERVER_API,
    extension_type_t_EXTENSION_ASCII_PROTOCOL, token_t,
};
use handler::offload;
use server::ResponseHandler;

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
    // 상한을 넘는 본문과, 길이를 다 알 수 없는 줄은 받지 않는다. `execute`가 닫는다.
    if len > MAX_BODY_BYTES || unknown_len {
        return true;
    }

    unsafe {
        nread::expect_body(cookie, request, len, ndata, ptr_out);
    }
    true
}

/// 본문을 비울 수 없는 줄이면 연결을 닫는 이유.
fn should_close(len: usize, unknown_len: bool) -> Option<String> {
    if len > MAX_BODY_BYTES {
        Some(format!("declaring a {len}-byte body (limit {MAX_BODY_BYTES})"))
    } else if unknown_len {
        Some("with a segment length that could not be determined".to_owned())
    } else {
        None
    }
}

/// 명령을 처리한다. 답은 언제나 `on_io_complete` 콜백이 쓴다(`offload`).
///
/// 실패도 답으로 보내므로 보통 `true`다. `false`는 연결을 닫으라는 뜻이고, 비울 수
/// 없는 큰 본문에만 쓴다.
unsafe extern "C" fn execute_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: ResponseHandler,
) -> bool {
    let tokens = unsafe { Tokens::new(argv, argc) };

    // 상한을 넘는 본문은 코어가 버퍼 하나로만 받으므로 비울 길이 없이 버릴 수
    // 없다. 길이를 알 수 없는 구간이 있는 줄도 마찬가지다 -- 비울 만큼을 모르니
    // 그냥 답하면 클라이언트가 보낸 본문이 명령으로 읽힌다. 경고를 남기고 연결을
    // 닫게 한다.
    if let Ok(Parsed::Body {
        len, unknown_len, ..
    }) = request::parse(&tokens)
        && let Some(why) = should_close(len, unknown_len)
    {
        eprintln!("ArcVector: closing a connection that sent a body-carrying line {why}");
        return false;
    }

    match unsafe { command::parse(cookie, &tokens) } {
        Err(e) => unsafe { offload::reply(cookie, Err(e), handler) },
        Ok(request) => unsafe { offload::begin(cookie, request, handler) },
    }
    true
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    // 연결 슬롯에 남은 것(받다 만 본문)을 버린다. 기다리던 답은 슬롯이 아니라
    // `on_io_complete`의 `cb_data`에 있다.
    drop(unsafe { conn_state::take(cookie) });
}

unsafe extern "C" fn get_name_vector(_cmd_cookie: *const c_void) -> *const c_char {
    c"arcus-vector-db".as_ptr()
}

static mut VECTOR_DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR =
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
        get_name: Some(get_name_vector),
        get_auth_flag: None,
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
                    "ArcVector: the server core lacks {}; this extension needs an arcus-memcached EE build with the IO-wait list (vectest)",
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

    // 아이템 수명 이벤트를 받는다. 여기는 getopt 루프 안이라 엔진 초기화보다
    // 앞이고, 그래서 persistence 복구가 만드는 link도 놓치지 않는다.
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
