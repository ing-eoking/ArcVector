use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::error::{Reply, Result};
use crate::ffi::bindings::{ENGINE_ERROR_CODE, SERVER_CORE_API, SERVER_HANDLE_V1, rel_time_t};

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

/// Converts client-supplied expiration time into internal server relative time (`rel_time_t`).
///
/// In memcached protocol, `exptime` can be relative seconds (low values) or Unix timestamp
/// (high values), and 0 means no expiration. Server `rel_time_t` counts seconds since process start.
pub fn realtime(exptime: u32) -> rel_time_t {
    if exptime == 0 {
        return 0;
    }
    let core = core();
    if core.is_null() {
        return 0;
    }
    match unsafe { (*core).realtime } {
        Some(convert) => unsafe { convert(i64::from(exptime)) },
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
/// The count is per connection (`MULTI_NOTIFY_IO_COMPLETE`): only when every
/// wait has been notified does the core put the connection back on its
/// thread's pending list and run the callback registered by
/// [`on_io_complete`], from `conn_io_callback`. Calling it before the
/// connection has actually parked just takes one off the count, which is also
/// how a wait that turned out not to be needed is withdrawn.
pub unsafe fn notify_io_complete(cookie: *const c_void, status: ENGINE_ERROR_CODE) {
    let core = core();
    if core.is_null() {
        return;
    }
    if let Some(notify) = unsafe { (*core).notify_io_complete } {
        unsafe { notify(cookie, status) };
    }
}

/// Callback signature accepted by [`on_io_complete`], matching `AIO_CALLBACK`.
///
/// Invoked by `conn_io_callback` on the worker thread once all pending I/O finishes.
/// Returning `false` causes the server core to close the connection.
pub type IoCallback = unsafe extern "C" fn(*const c_void, *mut c_void, ResponseHandler) -> bool;

/// Registers an I/O completion callback for this command (called at most once per command).
///
/// When `execute` returns `true`, the core transitions the connection to `conn_io_callback`.
/// Once all pending I/O notifications arrive, the core calls `cb(cookie, data, response_handler)`.
/// `data` must remain valid until the callback runs and is freed by `cb`.
/// Returns `false` on failure, in which case the caller must immediately handle response and cleanup.
pub unsafe fn on_io_complete(cookie: *const c_void, cb: IoCallback, data: *mut c_void) -> bool {
    let core = core();
    if core.is_null() {
        return false;
    }
    match unsafe { (*core).on_io_complete } {
        Some(register) => {
            unsafe { register(cookie, Some(cb), data) };
            true
        }
        None => false,
    }
}

/// Returns names of missing members in `SERVER_CORE_API`.
pub fn missing_members(members: &[(&'static str, bool)]) -> Vec<&'static str> {
    members
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect()
}

/// Verifies that the server core provides all required API members.
/// Returns a list of missing member names, or `None` if the core API handle is null.
pub fn missing_core_members() -> Option<Vec<&'static str>> {
    let core = core();
    if core.is_null() {
        return None;
    }
    let core = unsafe { &*core };
    Some(missing_members(&[
        ("waitfor_io_complete", core.waitfor_io_complete.is_some()),
        ("notify_io_complete", core.notify_io_complete.is_some()),
        ("on_io_complete", core.on_io_complete.is_some()),
        ("realtime", core.realtime.is_some()),
        (
            "store_engine_specific",
            core.store_engine_specific.is_some(),
        ),
        ("get_engine_specific", core.get_engine_specific.is_some()),
    ]))
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

    /// Sends a response string to the client.
    ///
    /// The C response handler expects a NUL-terminated buffer. If memory allocation fails,
    /// falls back to a static preallocated out-of-memory error reply.
    pub fn send(&self, msg: &str) -> bool {
        const SHORT: &[u8] = b"SERVER_ERROR out of memory\r\n\0";
        let Some(handler) = self.handler else {
            return true;
        };
        let write = |bytes: &[u8]| unsafe {
            handler(
                self.cookie,
                (bytes.len() - 1) as c_int,
                bytes.as_ptr().cast::<c_char>(),
            )
        };

        let mut buf = Vec::new();
        if c_int::try_from(msg.len()).is_ok() && buf.try_reserve_exact(msg.len() + 1).is_ok() {
            buf.extend_from_slice(msg.as_bytes());
            buf.push(0);
            if write(&buf) {
                return true;
            }
        }
        write(SHORT)
    }

    pub fn reply(&self, outcome: Result<Reply>) -> bool {
        match outcome {
            Ok(reply) => self.send(reply.as_str()),
            Err(e) => self.send(&e.wire()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_the_absent_members_in_order() {
        let got = missing_members(&[("a", true), ("b", false), ("c", true), ("d", false)]);
        assert_eq!(got, ["b", "d"]);
    }

    #[test]
    fn nothing_is_missing_when_all_are_present() {
        assert!(missing_members(&[("a", true), ("b", true)]).is_empty());
        assert!(missing_members(&[]).is_empty());
    }
}
