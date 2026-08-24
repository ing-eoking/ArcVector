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

pub mod command;
pub mod error;
pub mod handler;
pub mod owner;
pub mod server;

pub use handler::arcus::element::ATTR_BYTES;
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
use server::{Responder, ResponseHandler};

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

unsafe extern "C" fn execute_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: ResponseHandler,
) -> bool {
    let tokens = unsafe { Tokens::new(argv, argc) };

    let outcome = unsafe { command::parse(cookie, &tokens) }
        .and_then(|request| unsafe { handler::run(cookie, request) });
    Responder::new(handler, cookie).reply(outcome);
    true
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    drop(unsafe { nread::take_body(cookie) });
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
    eprintln!("ArcVector: owner {}", owner::install());

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
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS
}
