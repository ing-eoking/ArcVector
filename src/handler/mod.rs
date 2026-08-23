pub mod arcus;
pub mod quant;
#[cfg(recovery)]
pub mod recovery;
pub mod registry;
pub mod usearch;

mod access;
mod cmd;

use std::os::raw::c_void;

use arcus::{Store, StoreError};
use cmd::{vadd, vcreate, vdel, vdrop, vgetattr, vlist, vsetattr, vsim_key, vsim_vector, vstats};

use crate::command::Request;
use crate::command::request::{Body, Line};
use crate::error::{Error, Reply, Result};

/// # Safety
///
/// `cookie` must be the cookie memcached passed to that callback.
unsafe fn store_for(cookie: *const c_void) -> Result<Store> {
    unsafe { Store::for_cookie(cookie) }.ok_or_else(|| {
        // A refusal means the load-time ABI check rejected this pairing.
        Error::Store(if arcus::abi::refused() {
            StoreError::AbiMismatch
        } else {
            StoreError::Unavailable
        })
    })
}

/// # Safety
///
/// `cookie` must be the `execute` call's connection cookie.
pub unsafe fn run(cookie: *const c_void, request: Request) -> Result<Reply> {
    // A misaligned vtable was caught: nothing the engine reports can be trusted, "this element is missing" least of all.
    if arcus::abi::mismatched() {
        return Err(Error::Store(StoreError::AbiMismatch));
    }

    let store = &unsafe { store_for(cookie) }?;

    let reply = match request {
        Request::Body(Body::Add(spec), bytes) => vadd(store, &spec, &bytes),
        Request::Body(Body::Sim(spec), bytes) => vsim_vector(store, &spec, &bytes),
        Request::Line(Line::Create(spec)) => vcreate(store, &spec),
        Request::Line(Line::SimKey(spec)) => vsim_key(store, &spec),
        Request::Line(Line::GetAttr { index, id }) => vgetattr(store, index, id),
        Request::Line(Line::SetAttr { index, id, attr }) => vsetattr(store, index, id, attr),
        Request::Line(Line::Del { index, id }) => vdel(store, index, id),
        Request::Line(Line::Drop { index }) => vdrop(store, index),
        Request::Line(Line::List) => vlist(),
        Request::Line(Line::Stats) => vstats(),
    };

    // After the answer, so the sweep is never part of a command's latency, and on this thread
    // because it has the cookie an engine read may be handed — see `sweep`.
    access::sweep::maybe(store);
    reply
}
