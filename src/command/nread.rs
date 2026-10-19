use std::os::raw::{c_char, c_void};

use super::request::Body;
use crate::conn_state::{self, ConnState};
use crate::error::Error;

/// `len`은 본문 전체의 바이트 수다. 구간마다 끝의 `\r\n`을 포함하며
/// (`Segments::total`), 그것을 확인하는 것은 `Segments::split`의 일이다.
#[derive(Debug)]
pub struct Pending {
    request: std::result::Result<Body, Error>,

    buffer: Vec<u8>,
}

impl Pending {
    fn new(request: std::result::Result<Body, Error>, len: usize) -> Self {
        Self {
            request,
            buffer: vec![0; len],
        }
    }

    pub fn into_parts(self) -> (std::result::Result<Body, Error>, Vec<u8>) {
        (self.request, self.buffer)
    }
}

pub unsafe fn expect_body(
    cookie: *const c_void,
    request: std::result::Result<Body, Error>,
    len: usize,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) {
    let mut state = Pending::new(request, len);
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
    use crate::command::request::{Segments, SetMeta};

    fn setmeta() -> (Body, Segments) {
        let segments = Segments {
            meta: Some(7),
            ..Segments::default()
        };
        let body = Body::SetMeta(SetMeta {
            index: "docs".into(),
            vkey: "v1".into(),
            segments,
        });
        (body, segments)
    }

    #[test]
    fn the_buffer_holds_every_segment_with_its_crlf() {
        let (body, segments) = setmeta();
        let p = Pending::new(Ok(body), segments.total());
        assert_eq!(p.buffer.len(), 9);
        let (request, bytes) = p.into_parts();
        assert!(request.is_ok());
        assert_eq!(bytes.len(), 9);
    }

    #[test]
    fn a_refused_line_still_gets_a_body_to_drain() {
        let p = Pending::new(Err(Error::bad_request("nope")), 9);
        let (request, body) = p.into_parts();
        assert!(request.is_err());
        assert_eq!(body.len(), 9);
    }

    #[test]
    fn a_body_not_followed_by_crlf_is_a_bad_data_chunk() {
        let (_, segments) = setmeta();
        let err = segments.split(b"0.11 0.21").unwrap_err();
        assert_eq!(
            err.to_string(),
            "bad data chunk",
            "a mis-declared length must be refused"
        );
    }
}
