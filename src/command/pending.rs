//! Per-connection state for the two-phase transfer memcached calls `nread`.
//!
//! The parsed line is stored as a `Result`: refusing in `accept` would leave the
//! body unread, and memcached would parse it as the next command line.

use std::collections::HashMap;
use std::os::raw::{c_char, c_void};
use std::sync::{LazyLock, Mutex, PoisonError};

use super::request::Body;
use crate::error::Error;

/// A command line whose body has still to arrive, with the buffer for it.
#[derive(Debug)]
pub struct Pending {
    request: std::result::Result<Body, Error>,
    /// Body plus the two trailing CRLF bytes memcached appends.
    buffer: Vec<u8>,
    body_len: usize,
}

impl Pending {
    fn new(request: std::result::Result<Body, Error>, body_len: usize) -> Self {
        Self {
            request,
            buffer: vec![0; body_len + 2],
            body_len,
        }
    }

    /// Fill in the CRLF memcached would have written. Tests only.
    #[cfg(test)]
    fn terminate(mut self) -> Self {
        let n = self.body_len;
        self.buffer[n..].copy_from_slice(b"\r\n");
        self
    }

    /// Split into the parsed line and its body, dropping the trailing CRLF.
    pub fn into_parts(mut self) -> (std::result::Result<Body, Error>, Vec<u8>) {
        let terminated = self.buffer[self.body_len..] == *b"\r\n";
        self.buffer.truncate(self.body_len);
        let request = if terminated {
            self.request
        } else {
            Err(Error::bad_request("bad data chunk"))
        };
        (request, self.buffer)
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

pub fn take_body(cookie: *const c_void) -> Option<Pending> {
    table().remove(&(cookie as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::request::{Add, Sim};
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
        let (request, body) = p.terminate().into_parts();
        assert!(request.is_ok());
        assert_eq!(body.len(), 7);
    }

    #[test]
    fn a_refused_line_still_gets_a_body_to_drain() {
        // The stream stays in sync only because the bytes are consumed.
        let p = Pending::new(Err(Error::bad_request("nope")), 7);
        let (request, body) = p.terminate().into_parts();
        assert!(request.is_err());
        assert_eq!(body.len(), 7);
    }

    #[test]
    fn a_body_not_followed_by_crlf_is_a_bad_data_chunk() {
        // The client declared fewer bytes than it sent, so what we were handed is
        // truncated and the remainder is still in the stream. Storing it would
        // silently keep a short vector and desync the connection.
        let mut p = Pending::new(Ok(add()), 7);
        p.buffer.copy_from_slice(b"0.11 0.21");
        let (request, _body) = p.into_parts();
        assert_eq!(
            request.unwrap_err().to_string(),
            "bad data chunk",
            "a mis-declared length must be refused"
        );
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
        // SAFETY: both out-parameters are live locals, and each copy writes
        // exactly the `body_len + 2` bytes the buffer was sized for — which is
        // what memcached itself does, body followed by CRLF.
        unsafe {
            expect_body(a, Ok(add()), 3, &mut ndata, &mut ptr);
            ptr::copy_nonoverlapping(b"1 2\r\n".as_ptr(), ptr.cast::<u8>(), 5);

            expect_body(
                b,
                Ok(Body::Sim(Sim {
                    index: "docs".into(),
                    k: 1,
                    dim: 2,
                    filter: None,
                })),
                7,
                &mut ndata,
                &mut ptr,
            );
            ptr::copy_nonoverlapping(b"1 2 3 4\r\n".as_ptr(), ptr.cast::<u8>(), 9);
        }
        let (request_a, body_a) = take_body(a).unwrap().into_parts();
        let (request_b, body_b) = take_body(b).unwrap().into_parts();
        assert_eq!(body_a, b"1 2");
        assert_eq!(body_b, b"1 2 3 4");
        assert!(matches!(request_a, Ok(Body::Add(_))));
        assert!(matches!(request_b, Ok(Body::Sim(_))));
    }
}
