use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::engine_api::{ENGINE_ERROR_CODE, SERVER_CORE_API, SERVER_HANDLE_V1, rel_time_t};
use crate::error::{Reply, Result};

static GET_SERVER_API: OnceLock<unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1> = OnceLock::new();

pub fn set_api(f: unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1) {
    let _ = GET_SERVER_API.set(f);
}

pub fn handle() -> *mut SERVER_HANDLE_V1 {
    match GET_SERVER_API.get() {
        Some(get_api) => unsafe { get_api() },
        None => std::ptr::null_mut(),
    }
}

fn core() -> *const SERVER_CORE_API {
    let server = handle();
    if server.is_null() {
        return std::ptr::null();
    }

    unsafe { (*server).core }
}

/// The server's clock, in the same units an item's `exptime` is written in.
///
/// Both are `rel_time_t` -- seconds since the process started -- so an item is
/// expired exactly when its `exptime` has fallen behind this. Nothing here
/// converts to or from wall time; that is `realtime`'s job, and only a caller
/// setting an expiry needs it.
///
/// `0` when the server API is out of reach, which
/// [`crate::handler::arcus::engine::is_expired`] reads as "expire nothing".
/// Callers take this **once** per command rather than per item: a search that
/// asked twice could accept a vector early in its traversal and reject the same
/// one later, and the extra indirect call would land in the hottest loop there
/// is.
pub fn current_time() -> rel_time_t {
    let core = core();
    if core.is_null() {
        return 0;
    }

    match unsafe { (*core).get_current_time } {
        Some(now) => unsafe { now() },
        None => 0,
    }
}

/// Tells the core this connection has one more answer outstanding.
///
/// **Called before the work is handed to the pool, never after.** The core
/// only takes a connection out of the event loop when this count is above
/// zero, so a pool thread that finished first would otherwise leave the
/// connection parked with nothing left to wake it.
pub unsafe fn waitfor_io_complete(cookie: *const c_void) {
    let core = core();
    if core.is_null() {
        return;
    }
    if let Some(waitfor) = unsafe { (*core).waitfor_io_complete } {
        unsafe { waitfor(cookie) };
    }
}

/// One outstanding answer is ready.
///
/// The core puts the connection back on its thread's pending list and drives
/// the state machine from `conn_waking`, which is where our callback runs.
/// Safe to call on a connection that has gone away: the core checks for that
/// under the thread lock.
pub unsafe fn notify_io_complete(cookie: *const c_void, status: ENGINE_ERROR_CODE) {
    let core = core();
    if core.is_null() {
        return;
    }
    if let Some(notify) = unsafe { (*core).notify_io_complete } {
        unsafe { notify(cookie, status) };
    }
}

pub unsafe fn store_conn_state(cookie: *const c_void, data: *mut c_void) -> bool {
    let core = core();
    if core.is_null() {
        return false;
    }

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

/// 연결에 매달린 것을 **지우지 않고** 본다.
///
/// `block` 훅이 "이 연결이 답을 기다리는 중인가"를 묻는 자리라 필요하다 --
/// 가져가 버리면 뒤이어 깨어날 콜백이 답할 것을 잃는다.
pub unsafe fn peek_conn_state(cookie: *const c_void) -> *mut c_void {
    let core = core();
    if core.is_null() {
        return std::ptr::null_mut();
    }
    match unsafe { (*core).get_engine_specific } {
        Some(get) => unsafe { get(cookie) },
        None => std::ptr::null_mut(),
    }
}

pub unsafe fn take_conn_state(cookie: *const c_void) -> *mut c_void {
    let core = core();
    if core.is_null() {
        return std::ptr::null_mut();
    }

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

pub type ResponseHandler =
    Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

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

        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(0);

        unsafe {
            handler(
                self.cookie,
                msg.len() as c_int,
                buf.as_ptr().cast::<c_char>(),
            );
        }
    }

    pub fn reply(&self, outcome: Result<Reply>) {
        match outcome {
            Ok(reply) => self.send(reply.as_str()),
            Err(e) => self.send(&format!("{} {e}\r\n", e.blame().prefix())),
        }
    }
}
