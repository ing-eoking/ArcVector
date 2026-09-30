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
pub mod server;
pub mod trigger;

pub use handler::arcus::element::ATTR_BYTES;
pub use handler::quant::Quant;
pub use handler::usearch::Metric;

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use command::nread;
use command::request::{self, MAX_BODY_BYTES, Parsed};
use command::tokens::Tokens;
use engine_api::{
    ENGINE_EVENT_TYPE, EXTENSION_ASCII_PROTOCOL_DESCRIPTOR, EXTENSION_ERROR_CODE,
    EXTENSION_ERROR_CODE_EXTENSION_FATAL, EXTENSION_ERROR_CODE_EXTENSION_SUCCESS, GET_SERVER_API,
    extension_type_t_EXTENSION_ASCII_PROTOCOL, token_t,
};
use handler::offload;
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

    let outcome = match unsafe { command::parse(cookie, &tokens) } {
        Err(e) => Err(e),
        Ok(request) => match unsafe { offload::begin(cookie, handler, request) } {
            // 넘겼다. 응답을 쓰지 않고 돌아가면, 코어가 곧 `block`을 물어보고
            // 연결을 재운다.
            None => return true,
            Some(outcome) => outcome,
        },
    };

    Responder::new(handler, cookie).reply(outcome);
    true
}

/// What `block` hands back: a bare function pointer the core casts.
///
/// `extension.h` declares the return as `void (*)(void)` and the core casts it
/// to `EVENT_CALLBACK` before calling it with four arguments. bindgen reads
/// that empty parameter list as two pointers, which matches neither the
/// declaration nor the call -- so this names the type bindgen produced, and
/// [`block_vector_cmd`] does the same cast the core does.
type BlockResult = Option<unsafe extern "C" fn(*const c_void, *const c_void)>;

/// 이 연결이 답을 기다리는 중인가.
///
/// `execute`가 돌아온 직후 코어가 묻는다. `None`이면 응답이 이미 버퍼에
/// 있다는 뜻이고, 아니면 코어가 연결을 `conn_waking`에 세워 두었다가 저
/// 콜백으로 깨운다.
unsafe extern "C" fn block_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
) -> BlockResult {
    if !offload::is_waiting(cookie) {
        return None;
    }
    // The core stores this and calls it as an `EVENT_CALLBACK`, with the four
    // arguments `wake_vector_cmd` is written for. Nothing in the declared
    // types says so, so the cast is the same one `memcached.c` writes:
    //
    //     c->aiocb = (EVENT_CALLBACK)cmd->block(cmd->cookie, c);
    //
    // Declaring `block` as returning `EVENT_CALLBACK` would remove the cast
    // from both sides and let the types carry the truth instead.
    Some(unsafe {
        std::mem::transmute::<*const (), unsafe extern "C" fn(*const c_void, *const c_void)>(
            wake_vector_cmd as *const (),
        )
    })
}

/// `conn_waking`에서 코어가 부른다. 워커 스레드다.
unsafe extern "C" fn wake_vector_cmd(
    cookie: *const c_void,
    _type: ENGINE_EVENT_TYPE,
    _event_data: *const c_void,
    _cb_data: *const c_void,
) {
    unsafe { offload::wake(cookie) };
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    drop(unsafe { nread::take_body(cookie) });
    // 답을 기다리던 중이었다면 그 자리도 비운다. 풀이 아직 돌고 있어도
    // 보낼 곳이 없어질 뿐이고, 코어는 닫힌 연결의 notify를 알아서 버린다.
    offload::forget(cookie);
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
        block: Some(block_vector_cmd),
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
