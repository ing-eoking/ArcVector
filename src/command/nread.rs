use std::os::raw::{c_char, c_void};

use super::request::Body;
use crate::conn_state::{self, ConnState};
use crate::error::Error;

#[derive(Debug)]
pub struct Pending {
    request: std::result::Result<Body, Error>,

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

pub unsafe fn expect_body(
    cookie: *const c_void,
    request: std::result::Result<Body, Error>,
    body_len: usize,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) {
    let mut state = Pending::new(request, body_len);
    // 엔진이 여기에 본문을 써넣는다. `Vec`의 버퍼는 힙에 따로 있으므로,
    // `Pending`이 `ConnState` 안으로 들어가도 이 주소는 그대로다.
    let len = state.buffer.len();
    let ptr = state.buffer.as_mut_ptr().cast::<c_char>();

    if unsafe { conn_state::put(cookie, ConnState::Body(state)) } {
        unsafe {
            *ndata = len;
            *ptr_out = ptr;
        }
    }
}

pub unsafe fn take_body(cookie: *const c_void) -> Option<Pending> {
    unsafe { conn_state::take_body(cookie) }
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
