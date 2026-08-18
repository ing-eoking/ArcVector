//! The wire: reading a command line and turning it into a typed request.
//!
//! Nothing here touches the engine, an index or any state. [`parse`] is the whole
//! outward face — it hands back a [`Request`] and stops, and [`crate::handler`]
//! is what acts on one.

pub mod filter;
pub mod pending;
pub mod request;
pub mod tokens;

use std::os::raw::c_void;

use crate::error::{Error, Result};
use request::{Body, Line};
use tokens::Tokens;

/// One command, parsed and ready to run.
pub enum Request<'a> {
    /// Settled by its command line alone.
    Line(Line<'a>),
    /// A two-phase command whose coordinates have now arrived.
    Body(Body, Vec<u8>),
}

/// Parse one `execute` call: a command line, or the body of one already accepted.
///
/// # Safety
///
/// `tokens` must borrow the argument vector memcached passed to `execute`, and
/// `cookie` must be that call's connection cookie.
pub unsafe fn parse<'a>(cookie: *const c_void, tokens: &Tokens<'a>) -> Result<Request<'a>> {
    // An empty argument vector means a body arrived for a two-phase command.
    if tokens.is_empty() {
        let waiting =
            pending::take_body(cookie).ok_or_else(|| Error::bad_request("lost command state"))?;
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
