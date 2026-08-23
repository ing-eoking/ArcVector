use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

use crate::engine_api::{SERVER_CORE_API, SERVER_HANDLE_V1};
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

static COARSE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn coarse_now() -> u64 {
    COARSE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn tick() {
    advance(now());
}

fn advance(seen: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let _ = COARSE.fetch_update(Relaxed, Relaxed, |cur| Some(seen.max(cur + 1)));
}

fn now() -> u64 {
    let core = core();
    if core.is_null() {
        return 0;
    }
    unsafe {
        match (*core).get_current_time {
            Some(get) => u64::from(get()),
            None => 0,
        }
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

#[cfg(test)]
mod tests {
    use super::{COARSE, advance, coarse_now};
    use std::sync::atomic::Ordering::Relaxed;

    /// `get_current_time` is `gettimeofday` minus the process start, so it can step backwards.
    /// Reclamation compares stamps to decide when an address is nobody's to read, and a stamp
    /// that goes back would free one out from under a search.
    #[test]
    fn the_coarse_clock_never_goes_back_and_never_stalls() {
        COARSE.store(100, Relaxed);

        advance(140);
        assert_eq!(coarse_now(), 140, "it follows the server's clock forward");

        advance(50);
        assert_eq!(
            coarse_now(),
            141,
            "a step backwards still moves it on by one"
        );

        advance(50);
        assert_eq!(
            coarse_now(),
            142,
            "and keeps moving, so reclamation cannot stall"
        );

        advance(200);
        assert_eq!(
            coarse_now(),
            200,
            "once the clock catches up it takes over again"
        );
    }
}
