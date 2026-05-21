#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unnecessary_transmutes)]

pub mod engine_api;

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use engine_api::{
    extension_type_t_EXTENSION_ASCII_PROTOCOL,
    token_t,
    AUTHZ_NONE,
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR,
    EXTENSION_ERROR_CODE,
    EXTENSION_ERROR_CODE_EXTENSION_FATAL,
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS,
    GET_SERVER_API,
};

static mut NOOP_DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR =
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
        get_name: Some(get_name),
        get_auth_flag: Some(get_auth_flag),
        accept: Some(accept_command),
        execute: Some(execute_command),
        abort: Some(abort_command),
        cookie: &raw const NOOP_DESCRIPTOR as *const c_void,
        next: ptr::null_mut(),
    };

static mut ECHO_DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR =
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
        get_name: Some(get_name),
        get_auth_flag: Some(get_auth_flag),
        accept: Some(accept_command),
        execute: Some(execute_command),
        abort: Some(abort_command),
        cookie: &raw const ECHO_DESCRIPTOR as *const c_void,
        next: ptr::null_mut(),
    };

unsafe extern "C" fn get_name(cmd_cookie: *const c_void) -> *const c_char {
    if cmd_cookie == &raw const NOOP_DESCRIPTOR as *const c_void {
        "noop\0".as_ptr() as *const c_char
    } else {
        "echo\0".as_ptr() as *const c_char
    }
}

unsafe extern "C" fn get_auth_flag() -> u16 {
    AUTHZ_NONE as u16
}

unsafe extern "C" fn accept_command(
    cmd_cookie: *const c_void,
    _cookie: *mut c_void,
    _argc: c_int,
    argv: *mut token_t,
    _ndata: *mut usize,
    _ptr: *mut *mut c_char,
) -> bool {
    let cmd = unsafe { CStr::from_ptr((*argv).value) };
    let cmd_str = cmd.to_str().unwrap_or("");
    if cmd_cookie == &raw const NOOP_DESCRIPTOR as *const c_void {
        cmd_str == "noop"
    } else {
        cmd_str == "echo"
    }
}

unsafe extern "C" fn execute_command(
    cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: ::std::option::Option<
        unsafe extern "C" fn(
            cookie: *const ::std::os::raw::c_void,
            nbytes: ::std::os::raw::c_int,
            dta: *const ::std::os::raw::c_char,
        ) -> bool,
    >,
) -> bool {
    let handler = match response_handler {
        Some(h) => h,
        None => return false,
    };

    if cmd_cookie == &raw const NOOP_DESCRIPTOR as *const c_void {
        unsafe { handler(cookie, 4, "OK\r\n\0".as_ptr() as *const c_char) }
    } else {
        let first_token = unsafe { &*argv };
        if unsafe { !handler(cookie, first_token.length as c_int, first_token.value) } {
            return false;
        }

        for ii in 1..argc {
            let token = unsafe { &*argv.add(ii as usize) };
            if unsafe {
                !handler(cookie, 2, " [\0".as_ptr() as *const c_char)
                    || !handler(cookie, token.length as c_int, token.value)
                    || !handler(cookie, 1, "]\0".as_ptr() as *const c_char)
            } {
                return false;
            }
        }

        unsafe { handler(cookie, 2, "\r\n\0".as_ptr() as *const c_char) }
    }
}

unsafe extern "C" fn abort_command(
    _cmd_cookie: *const c_void,
    _cookie: *const c_void,
) {
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcached_extensions_initialize(
    _config: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    let get_api = match get_server_api {
        Some(f) => f,
        None => return EXTENSION_ERROR_CODE_EXTENSION_FATAL,
    };

    let server = unsafe { get_api() };
    if server.is_null() {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }

    let extension = unsafe { (*server).extension };
    if extension.is_null() {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }

    let register = match unsafe { (*extension).register_extension } {
        Some(f) => f,
        None => return EXTENSION_ERROR_CODE_EXTENSION_FATAL,
    };

    if unsafe {
        !register(
            extension_type_t_EXTENSION_ASCII_PROTOCOL,
            &raw mut NOOP_DESCRIPTOR as *mut c_void,
        )
    } {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }

    if unsafe {
        !register(
            extension_type_t_EXTENSION_ASCII_PROTOCOL,
            &raw mut ECHO_DESCRIPTOR as *mut c_void,
        )
    } {
        return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
    }

    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS
}
