use std::os::raw::{c_char, c_void};

use super::request::Body;
use crate::error::Error;
use crate::server;

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

    /// Fill in the CRLF memcached would have written.
    #[cfg(test)]
    fn terminate(mut self) -> Self {
        let n = self.body_len;
        self.buffer[n..].copy_from_slice(b"\r\n");
        self
    }

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

/// # Safety
///
/// `ndata`/`ptr_out` are `accept`'s out-parameters, and memcached writes at most `body_len + 2` bytes.
pub unsafe fn expect_body(
    cookie: *const c_void,
    request: std::result::Result<Body, Error>,
    body_len: usize,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) {
    // SAFETY: caller-guaranteed. Anything already there is a dead transfer; reclaim it rather than leak.
    drop(unsafe { take_body(cookie) });

    let mut state = Box::new(Pending::new(request, body_len));
    let len = state.buffer.len();
    let ptr = state.buffer.as_mut_ptr().cast::<c_char>();
    // SAFETY: caller-guaranteed. The box comes back through `take_body`, which `execute` and `abort` both call — and `conn_close` runs `abort` first.
    unsafe {
        let raw = Box::into_raw(state);
        if !server::store_conn_state(cookie, raw.cast::<c_void>()) {
            // No host to hold it: take it back rather than leak.
            drop(Box::from_raw(raw));
            return;
        }
        *ndata = len;
        *ptr_out = ptr;
    }
}

/// # Safety
///
/// live cookie, and the slot holds null or a `Pending` this module put there.
pub unsafe fn take_body(cookie: *const c_void) -> Option<Box<Pending>> {
    let data = unsafe { server::take_conn_state(cookie) };
    if data.is_null() {
        return None;
    }
    // SAFETY: the slot only ever holds a box this module leaked into it.
    Some(unsafe { Box::from_raw(data.cast::<Pending>()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::request::Add;

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
        let p = Pending::new(Err(Error::bad_request("nope")), 7);
        let (request, body) = p.terminate().into_parts();
        assert!(request.is_err());
        assert_eq!(body.len(), 7);
    }

    #[test]
    fn a_body_not_followed_by_crlf_is_a_bad_data_chunk() {
        // A short body would store a truncated vector and leave the remainder in the stream.
        let mut p = Pending::new(Ok(add()), 7);
        p.buffer.copy_from_slice(b"0.11 0.21");
        let (request, _body) = p.into_parts();
        assert_eq!(
            request.unwrap_err().to_string(),
            "bad data chunk",
            "a mis-declared length must be refused"
        );
    }
}
