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

    access::sweep::maybe(store);
    reply
}
