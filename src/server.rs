//! The host memcached gives an extension: `SERVER_HANDLE_V1`.
//!
//! Not the storage engine — that is [`crate::handler::arcus::engine`], reached
//! *through* this handle. What lives here is the accessor itself and the host
//! calls that belong to a connection rather than to storage: the one pointer
//! memcached keeps per `conn`, and the callback that writes a response back to
//! it. Every unsafe call into the host with a connection cookie is in this file.

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
        // SAFETY: memcached handed us this function pointer during extension
        // initialization and it stays valid for the process lifetime.
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

/// Hang one pointer off a connection, replacing whatever was there.
///
/// memcached keeps a single `void *` per `conn` for this. It is nominally the
/// storage engine's, and no arcus engine in either tree touches it — the field is
/// written and read only through these two calls.
///
/// # Safety
///
/// `cookie` must be a live connection cookie, and `data` either null or a pointer
/// this crate owns and will reclaim through [`take_conn_state`].
///
/// Returns whether the host took it. `false` means there is no host — the
/// extension has not been initialized — and the caller still owns `data`.
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

/// Take back what [`store_conn_state`] left, clearing the slot.
///
/// # Safety
///
/// `cookie` must be a live connection cookie.
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

/// One response, written to the connection the command arrived on.
///
/// Unlike the per-connection slot above, the handler is not part of
/// `SERVER_CORE_API`: memcached passes it as an argument to `execute`, so a
/// responder is only good for the call it was built in.
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
