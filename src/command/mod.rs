//! Receiving a command, interpreting it, and running it.
//!
//! This is the layer between the socket and the two backends. It is split by the
//! order the work happens in:
//!
//! - [`tokens`] reads memcached's token array and writes the one reply. The only
//!   module that touches `token_t`.
//! - [`request`] turns those tokens into typed values. Pure — no engine, no index,
//!   no state.
//! - [`pending`] holds a parsed request while its body is still arriving, for the
//!   two commands that have one.
//! - [`filter`] is the little expression language a `FILTER` clause is written in,
//!   parsed here and evaluated during a search.
//! - [`handler`] does the work, calling [`crate::arcus`] and [`crate::usearch`].
//!
//! Parsing never touches state and handlers never see a token, so syntax has one
//! home and handlers can be exercised without a server.

pub mod filter;
pub mod handler;
pub mod pending;
pub mod request;
pub mod tokens;

use std::os::raw::c_void;

use crate::arcus::{Store, StoreError};
use crate::error::{Error, Reply, Result};
use request::{Body, Line};
use tokens::Tokens;

/// Engine access for the callback currently running.
///
/// # Safety
///
/// `cookie` must be the cookie memcached passed to that callback.
unsafe fn store_for(cookie: *const c_void) -> Result<Store> {
    // SAFETY: guaranteed by the caller.
    unsafe { Store::for_cookie(cookie) }.ok_or(Error::Store(StoreError::Unavailable))
}

/// Route one command line, or resume one whose body has arrived.
///
/// # Safety
///
/// `tokens` must borrow the argument vector memcached passed to `execute`, and
/// `cookie` must be that call's connection cookie.
pub unsafe fn dispatch(cookie: *const c_void, tokens: &Tokens) -> Result<Reply> {
    // SAFETY: guaranteed by the caller.
    let store = unsafe { store_for(cookie) };

    // An empty argument vector means a body arrived for a two-phase command.
    if tokens.is_empty() {
        let waiting =
            pending::take_body(cookie).ok_or_else(|| Error::bad_request("lost command state"))?;
        let (request, bytes) = waiting.into_parts();
        let store = store?;
        return match request? {
            Body::Add(spec) => handler::vadd(&store, &spec, &bytes),
            Body::Sim(spec) => handler::vsim_vector(&store, &spec, &bytes),
        };
    }

    // A body-carrying line reaching `execute` means `accept` could not size its
    // body; otherwise the body phase above would have handled it.
    if let Some(at) = request::body_length_at(tokens) {
        let what = if at == 3 {
            "vector length"
        } else {
            "vector bytes"
        };
        return Err(request::body_length_error(tokens, at, what));
    }

    let store = &store?;
    match request::parse_line(tokens)? {
        Line::Create(spec) => handler::vcreate(store, &spec),
        Line::SimKey(spec) => handler::vsim_key(store, &spec),
        Line::Get { index, id } => handler::vget(store, index, id),
        Line::Del { index, id } => handler::vdel(store, index, id),
        Line::Drop { index } => handler::vdrop(store, index),
        Line::List => handler::vlist(),
    }
}
