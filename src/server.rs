//! The host handle memcached gives an extension, and the per-connection calls that reach through it.

use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::engine_api::{SERVER_CORE_API, SERVER_HANDLE_V1};
use crate::error::{Reply, Result};

static GET_SERVER_API: OnceLock<unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1> = OnceLock::new();

/// Record the accessor handed to the extension at load time.
pub fn set_api(f: unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1) {
    let _ = GET_SERVER_API.set(f);
}

/// The host handle, or null before the extension has been initialized.
pub fn handle() -> *mut SERVER_HANDLE_V1 {
    match GET_SERVER_API.get() {
        // SAFETY: memcached's accessor stays valid for the process lifetime.
        Some(get_api) => unsafe { get_api() },
        None => std::ptr::null_mut(),
    }
}

/// `SERVER_CORE_API`, whose leading members are identical in every arcus tree.
fn core() -> *const SERVER_CORE_API {
    let server = handle();
    if server.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a non-null SERVER_HANDLE_V1 from memcached is fully initialized.
    unsafe { (*server).core }
}

/// # Safety
///
/// live `cookie`; `data` null or ours to reclaim via [`take_conn_state`]. `false` means no host, and the caller still owns `data`.
pub unsafe fn store_conn_state(cookie: *const c_void, data: *mut c_void) -> bool {
    let core = core();
    if core.is_null() {
        return false;
    }
    // SAFETY: `core` is non-null and its leading members are stable.
    unsafe {
        match (*core).store_engine_specific {
            Some(store) => {
                store(cookie, data);
                true
            }
            None => false,
        }
    }
}

/// # Safety
///
/// `cookie` must be a live connection cookie. Clears the slot.
pub unsafe fn take_conn_state(cookie: *const c_void) -> *mut c_void {
    let core = core();
    if core.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `core` is non-null and its leading members are stable.
    unsafe {
        let Some(get) = (*core).get_engine_specific else {
            return std::ptr::null_mut();
        };
        let data = get(cookie);
        if !data.is_null() {
            let _ = store_conn_state(cookie, std::ptr::null_mut());
        }
        data
    }
}

/// The `execute` callback memcached hands us for writing one response.
pub type ResponseHandler =
    Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

/// The handler is `execute`'s argument, not part of `SERVER_CORE_API`, so a responder is good for one call only.
pub struct Responder {
    handler: ResponseHandler,
    cookie: *const c_void,
}

impl Responder {
    pub fn new(handler: ResponseHandler, cookie: *const c_void) -> Self {
        Self { handler, cookie }
    }

    pub fn send(&self, msg: &str) {
        let Some(handler) = self.handler else { return };
        // The handler takes a NUL-terminated string plus its length.
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(0);
        // SAFETY: `buf` stays alive for the call and is NUL-terminated.
        unsafe {
            handler(
                self.cookie,
                msg.len() as c_int,
                buf.as_ptr().cast::<c_char>(),
            );
        }
    }

    /// Turn a handler's outcome into the ASCII line the client sees.
    pub fn reply(&self, outcome: Result<Reply>) {
        match outcome {
            Ok(reply) => self.send(reply.as_str()),
            Err(e) => self.send(&format!("{} {e}\r\n", e.blame().prefix())),
        }
    }
}
