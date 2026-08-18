//! Command handlers, one module per group, plus the access check they share.
//!
//! Every handler returns `Result<Reply>`; turning that into an ASCII response is
//! [`crate::command::tokens::Responder::reply`]'s job.

pub mod arcus;
pub mod registry;
pub mod usearch;

mod access;
mod coords;
mod index;
mod search;
mod vector;

use std::os::raw::c_void;

use arcus::{Store, StoreError};
use index::{vcreate, vdrop, vlist, vstats};
use search::{vsim_key, vsim_vector};
use vector::{vadd, vdel, vget};

use crate::command::request::{Body, Line};
use crate::command::tokens::Tokens;
use crate::error::{Error, Reply, Result};

/// Engine access for the callback currently running.
///
/// # Safety
///
/// `cookie` must be the cookie memcached passed to that callback.
unsafe fn store_for(cookie: *const c_void) -> Result<Store> {
    // SAFETY: guaranteed by the caller.
    unsafe { Store::for_cookie(cookie) }.ok_or_else(|| {
        // A refusal means the load-time ABI check rejected this pairing.
        Error::Store(if arcus::abi::refused() {
            StoreError::AbiMismatch
        } else {
            StoreError::Unavailable
        })
    })
}

/// Route one command line, or resume one whose body has arrived.
///
/// # Safety
///
/// `tokens` must borrow the argument vector memcached passed to `execute`, and
/// `cookie` must be that call's connection cookie.
pub unsafe fn dispatch(cookie: *const c_void, tokens: &Tokens) -> Result<Reply> {
    // A misaligned vtable was caught mid-call. Nothing the engine reports after
    // that can be trusted — including "this element is missing", which the
    // recovery path would otherwise read as damage and answer by deleting a Map.
    if arcus::abi::mismatched() {
        return Err(Error::Store(StoreError::AbiMismatch));
    }

    // SAFETY: guaranteed by the caller.
    let store = unsafe { store_for(cookie) };

    // An empty argument vector means a body arrived for a two-phase command.
    if tokens.is_empty() {
        let waiting = crate::command::pending::take_body(cookie)
            .ok_or_else(|| Error::bad_request("lost command state"))?;
        let (request, bytes) = waiting.into_parts();
        let store = store?;
        return match request? {
            Body::Add(spec) => vadd(&store, &spec, &bytes),
            Body::Sim(spec) => vsim_vector(&store, &spec, &bytes),
        };
    }

    // Reaching here means `accept` could not size the body.
    if let Some(at) = crate::command::request::body_length_at(tokens) {
        let what = if at == 3 {
            "vector length"
        } else {
            "vector bytes"
        };
        return Err(crate::command::request::body_length_error(tokens, at, what));
    }

    let store = &store?;
    match crate::command::request::parse_line(tokens)? {
        Line::Create(spec) => vcreate(store, &spec),
        Line::SimKey(spec) => vsim_key(store, &spec),
        Line::Get { index, id } => vget(store, index, id),
        Line::Del { index, id } => vdel(store, index, id),
        Line::Drop { index } => vdrop(store, index),
        Line::List => vlist(),
        Line::Stats => vstats(),
    }
}
