pub mod arcus;
pub mod quant;
pub mod registry;
pub mod usearch;

pub(crate) mod access;
pub(crate) mod cmd;
pub(crate) mod offload;
pub(crate) mod order;

use std::os::raw::c_void;

use crate::command::Request;
use crate::command::request::Line;
use crate::error::{Error, Reply, Result};
use arcus::{Store, StoreError};

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

    match request {
        Request::Line(Line::Create(spec)) => cmd::create(store, &spec),
        Request::Line(Line::GetMeta(spec)) => cmd::getmeta(store, &spec),
        Request::Line(Line::Delete(spec)) => cmd::delete(store, &spec),
        Request::Line(Line::Drop(spec)) => cmd::drop_index(store, spec.index),
        // 본문이 있는 명령과 검색은 `offload`가 풀과 레인으로 보낸다.
        Request::Body(..) | Request::Line(Line::SearchKey(_)) => {
            Err(Error::bad_request("request is not routed here"))
        }
    }
}
