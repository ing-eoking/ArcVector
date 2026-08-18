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

use crate::command::Request;
use crate::command::request::{Body, Line};
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

/// Run one parsed command.
///
/// # Safety
///
/// `cookie` must be the connection cookie of the `execute` call being served.
pub unsafe fn run(cookie: *const c_void, request: Request) -> Result<Reply> {
    // A misaligned vtable was caught mid-call. Nothing the engine reports after
    // that can be trusted — including "this element is missing", which the
    // recovery path would otherwise read as damage and answer by deleting a Map.
    if arcus::abi::mismatched() {
        return Err(Error::Store(StoreError::AbiMismatch));
    }

    // SAFETY: guaranteed by the caller.
    let store = &unsafe { store_for(cookie) }?;

    match request {
        Request::Body(Body::Add(spec), bytes) => vadd(store, &spec, &bytes),
        Request::Body(Body::Sim(spec), bytes) => vsim_vector(store, &spec, &bytes),
        Request::Line(Line::Create(spec)) => vcreate(store, &spec),
        Request::Line(Line::SimKey(spec)) => vsim_key(store, &spec),
        Request::Line(Line::Get { index, id }) => vget(store, index, id),
        Request::Line(Line::Del { index, id }) => vdel(store, index, id),
        Request::Line(Line::Drop { index }) => vdrop(store, index),
        Request::Line(Line::List) => vlist(),
        Request::Line(Line::Stats) => vstats(),
    }
}
