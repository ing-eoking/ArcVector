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

    let Ok(Parsed::Body { len, request }) = request::parse(&tokens) else {
        return tokens.command().is_some();
    };
    if len > MAX_BODY_BYTES {
        return true;
    }

    unsafe {
        nread::expect_body(cookie, request, len, ndata, ptr_out);
    }
    true
}

/// 명령을 처리한다. 답은 언제나 `on_io_complete` 콜백이 쓴다(`offload`).
///
/// 언제나 `true`다. 실패도 답으로 보낸다 -- `false`는 연결을 닫으라는 뜻이다.
unsafe extern "C" fn execute_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    _handler: ResponseHandler,
) -> bool {
    let tokens = unsafe { Tokens::new(argv, argc) };

    match unsafe { command::parse(cookie, &tokens) } {
        Err(e) => unsafe { offload::reply(cookie, Err(e)) },
        Ok(request) => unsafe { offload::begin(cookie, request) },
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
