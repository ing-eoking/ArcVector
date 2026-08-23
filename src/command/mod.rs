pub mod filter;
pub mod nread;
pub mod request;
pub mod tokens;

use std::os::raw::c_void;

use crate::error::{Error, Result};
use request::{Body, Line, Parsed};
use tokens::Tokens;

pub enum Request<'a> {
    Line(Line<'a>),

    Body(Body, Vec<u8>),
}

pub unsafe fn parse<'a>(cookie: *const c_void, tokens: &Tokens<'a>) -> Result<Request<'a>> {
    if tokens.is_empty() {
        let waiting = unsafe { nread::take_body(cookie) }
            .ok_or_else(|| Error::bad_request("lost command state"))?;
        let (parsed, bytes) = waiting.into_parts();
        return Ok(Request::Body(parsed?, bytes));
    }

    match request::parse(tokens)? {
        Parsed::Line(line) => Ok(Request::Line(line)),

        Parsed::Body { len, .. } => Err(request::body_refused(len)),
    }
}
