//! Handles a single `vop` command.
//!
//! Engine writes happen here on the worker thread; graph work and searches run on the lanes
//! (`runtime::offload`). Each function in this module notes which thread it runs on.

pub mod access;
mod coords;
mod index;
mod meta;
mod search;
mod vector;

pub use index::{create, drop_index};
pub use meta::getmeta;
pub use search::{search_key, search_vector};
pub use vector::{delete, setmeta_write, upsert_write};

use std::os::raw::c_void;

use crate::engine::{self, Store, StoreError};
use crate::error::{Error, Reply, Result};
use crate::protocol::Request;
use crate::protocol::request::Line;

unsafe fn store_for(cookie: *const c_void) -> Result<Store> {
    unsafe { Store::for_cookie(cookie) }.ok_or_else(|| {
        Error::Store(if engine::abi::refused() {
            StoreError::AbiMismatch
        } else {
            StoreError::Unavailable
        })
    })
}

pub unsafe fn run(cookie: *const c_void, request: Request) -> Result<Reply> {
    if engine::abi::mismatched() {
        return Err(Error::Store(StoreError::AbiMismatch));
    }

    let store = &unsafe { store_for(cookie) }?;

    match request {
        Request::Line(Line::Create(spec)) => create(store, &spec),
        Request::Line(Line::GetMeta(spec)) => getmeta(store, &spec),
        Request::Line(Line::Drop(spec)) => drop_index(store, spec.index),
        // Writes and searches are sent to the lanes by `offload`.
        Request::Body(..) | Request::Line(Line::Delete(_) | Line::SearchKey(_)) => {
            Err(Error::bad_request("request is not routed here"))
        }
    }
}
