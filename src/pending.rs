//! Per-connection state for the two-phase transfer memcached calls `nread`.
//!
//! `vadd` and `VSIM VECTOR` announce a byte count on their command line and send
//! the coordinates afterwards. Between the two, the parsed line and the buffer
//! memcached will fill live here, keyed by connection cookie.
//!
//! The parsed line is stored as a `Result`. The `accept` callback has no way to
//! answer the client, and refusing there would leave the body unread — memcached
//! would then parse those coordinates as the next command line. So a body is
//! always registered and drained, and the failure travels with it for the handler
//! to report.

use std::collections::HashMap;
use std::os::raw::{c_char, c_void};
use std::sync::{LazyLock, Mutex, PoisonError};

use crate::error::Error;
use crate::request::Body;

/// A command line whose body has still to arrive, with the buffer for it.
///
/// The buffer is a plain `Vec`: memcached only writes into it, so ownership stays
/// here and dropping the entry releases it whether the command completed or was
/// aborted.
#[derive(Debug)]
pub struct Pending {
    request: std::result::Result<Body, Error>,
    /// Body plus the two trailing CRLF bytes memcached appends.
    buffer: Vec<u8>,
    body_len: usize,
}

impl Pending {
    fn new(request: std::result::Result<Body, Error>, body_len: usize) -> Pending {
        Pending {
            request,
            buffer: vec![0; body_len + 2],
            body_len,
        }
    }

    /// Split into the parsed line and its body, dropping the trailing CRLF.
    ///
    /// Consuming lets the caller move the `Result` out instead of cloning it.
    pub fn into_parts(mut self) -> (std::result::Result<Body, Error>, Vec<u8>) {
        self.buffer.truncate(self.body_len);
        (self.request, self.buffer)
    }
}

static PENDING: LazyLock<Mutex<HashMap<usize, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn table() -> std::sync::MutexGuard<'static, HashMap<usize, Pending>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Register `request` for `cookie` and expose the receive buffer to memcached.
///
/// # Safety
///
/// `ndata` and `ptr_out` must be the out-parameters of the `accept` callback, and
/// memcached must write at most `body_len + 2` bytes into the buffer.
pub unsafe fn expect_body(
    cookie: *const c_void,
    request: std::result::Result<Body, Error>,
    body_len: usize,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) {
    let mut state = Pending::new(request, body_len);
    let len = state.buffer.len();
    let ptr = state.buffer.as_mut_ptr().cast::<c_char>();
    // Publish before handing the pointer over, so an immediate abort finds it.
    table().insert(cookie as usize, state);
    // SAFETY: guaranteed by the caller.
    unsafe {
        *ndata = len;
        *ptr_out = ptr;
    }
}

/// Reclaim the body registered for `cookie`, if any.
pub fn take_body(cookie: *const c_void) -> Option<Pending> {
    table().remove(&(cookie as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Add, Sim};
    use std::ptr;

    fn add() -> Body {
        Body::Add(Add {
            index: "docs".into(),
            id: "v1".into(),
            dim: 2,
            attr: br#"{"a":1}"#.to_vec(),
        })
    }

    #[test]
    fn the_buffer_reserves_room_for_the_trailing_crlf() {
        let p = Pending::new(Ok(add()), 7);
        assert_eq!(p.buffer.len(), 9);
        let (_request, body) = p.into_parts();
        assert_eq!(body.len(), 7);
    }

    #[test]
    fn a_refused_line_still_gets_a_body_to_drain() {
        // The stream stays in sync only because the bytes are consumed.
        let p = Pending::new(Err(Error::bad_request("nope")), 7);
        let (request, body) = p.into_parts();
        assert!(request.is_err());
        assert_eq!(body.len(), 7);
    }

    #[test]
    fn bodies_are_keyed_by_cookie_and_taken_once() {
        // A cookie is only ever a map key here; never dereferenced.
        let cookie = ptr::without_provenance::<c_void>(0xF00D);
        let mut ndata = 0usize;
        let mut ptr: *mut c_char = ptr::null_mut();
        // SAFETY: both out-parameters are live locals.
        unsafe { expect_body(cookie, Ok(add()), 8, &mut ndata, &mut ptr) };
        assert_eq!(ndata, 10);
        assert!(!ptr.is_null());

        assert!(take_body(cookie).is_some());
        // A second take must not resurrect it — that would be a double free.
        assert!(take_body(cookie).is_none());
    }

    #[test]
    fn different_connections_do_not_share_state() {
        // Two distinct cookie values; never dereferenced.
        let a = ptr::without_provenance::<c_void>(1);
        let b = ptr::without_provenance::<c_void>(2);
        let mut ndata = 0usize;
        let mut ptr: *mut c_char = ptr::null_mut();
        // SAFETY: both out-parameters are live locals.
        unsafe {
            expect_body(a, Ok(add()), 4, &mut ndata, &mut ptr);
            expect_body(
                b,
                Ok(Body::Sim(Sim {
                    index: "docs".into(),
                    k: 1,
                    dim: 2,
                    filter: None,
                })),
                8,
                &mut ndata,
                &mut ptr,
            );
        }
        let (_, body_a) = take_body(a).unwrap().into_parts();
        let (request_b, body_b) = take_body(b).unwrap().into_parts();
        assert_eq!(body_a.len(), 4);
        assert_eq!(body_b.len(), 8);
        assert!(matches!(request_b, Ok(Body::Sim(_))));
    }
}
