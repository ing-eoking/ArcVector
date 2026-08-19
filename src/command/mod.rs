pub mod filter;
pub mod nread;
pub mod request;
pub mod tokens;

use std::os::raw::c_void;

use crate::error::{Error, Result};
use request::{Body, Line};
use tokens::Tokens;

pub enum Request<'a> {
    /// Settled by its command line alone.
    Line(Line<'a>),
    /// A two-phase command whose coordinates have now arrived.
    Body(Body, Vec<u8>),
}

/// # Safety
///
/// `tokens` must borrow `execute`'s argument vector, and `cookie` be that call's cookie.
pub unsafe fn parse<'a>(cookie: *const c_void, tokens: &Tokens<'a>) -> Result<Request<'a>> {
    // An empty argument vector means a body arrived for a two-phase command.
    if tokens.is_empty() {
        let waiting = unsafe { nread::take_body(cookie) }
            .ok_or_else(|| Error::bad_request("lost command state"))?;
        let (parsed, bytes) = waiting.into_parts();
        return Ok(Request::Body(parsed?, bytes));
    }

    // A body-carrying line reaching `execute` means `accept` could not size it.
    if let Some(at) = request::body_length_at(tokens) {
        let what = if at == 3 {
            "vector length"
        } else {
            "vector bytes"
        };
        return Err(request::body_length_error(tokens, at, what));
    }

    request::parse_line(tokens).map(Request::Line)
}
