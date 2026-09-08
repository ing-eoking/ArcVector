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

unsafe fn store_for(cookie: *const c_void) -> Result<Store> {
    unsafe { Store::for_cookie(cookie) }.ok_or_else(|| {
        Error::Store(if arcus::abi::refused() {
            StoreError::AbiMismatch
        } else {
            StoreError::Unavailable
        })
    })
}

pub unsafe fn run(cookie: *const c_void, request: Request) -> Result<Reply> {
    // Answered before anything else, and without a store: this is how the
    // parked connection hands over its cookie, and it has to work even on a
    // build where the engine ABI is refused, or the attach thread would keep
    // reconnecting to be told no.
    #[cfg(parked_cookie)]
    if let Request::Line(Line::Attach { token }) = request {
        if !crate::attach::is_ours(token) {
            return Err(Error::bad_request("unknown command vattach"));
        }
        crate::attach::adopt(cookie);
        return Ok(Reply::Body("ATTACHED\r\n".to_owned()));
    }

    if arcus::abi::mismatched() {
        return Err(Error::Store(StoreError::AbiMismatch));
    }

    let store = &unsafe { store_for(cookie) }?;

    match request {
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
        #[cfg(parked_cookie)]
        Request::Line(Line::Attach { .. }) => unreachable!("answered above"),
    }
}

/// Builds or fetches `name` exactly as a client request resolving it would,
/// without a request behind it. The replica side of replication
/// (`repl::slave`) calls this to converge an index proactively from a
/// `Snapshot` or `Resync`, rather than waiting on the first client read that
/// happens to land here to trigger the same build through `access::resolve`.
///
/// Exposed as this one function rather than widening `access` or `resolve`
/// itself: `access` stays private, `resolve` stays `pub(super)`, and
/// replication gets the single seam it needs instead of a door into every
/// other private helper this module has.
#[cfg(feature = "replication")]
pub(crate) fn resolve_for_replica(
    store: &Store,
    name: &str,
) -> Result<std::sync::Arc<registry::VectorIndex>> {
    access::resolve(store, name)
}
