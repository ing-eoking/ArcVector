//! The Map's metadata element: the only authority on whether a name is one of our indexes.
//!
//! `getattr` cannot stand in for this read. The item flags it reports are settable by any
//! client with `mop create`, so a plain Map can wear our tag, and they say nothing about
//! `owner`. One `map_elem_get` answers both questions a command has to ask before it touches
//! anything — is this name an index, and is it the one this node's graph was built from.

use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::{Store, StoreError};

/// What the name holds, as the metadata element reports it.
pub enum MetaState {
    Usable(MetaRecord, Layout),
    /// No Map at all: expired, evicted, or dropped elsewhere.
    NoMap,
    /// A Map is there, but it is not an index of ours — no metadata element, or one that will
    /// not decode. A plain `mop` Map created at a name an index used to hold lands here.
    Damaged(String),
    /// The engine could not answer. Nothing is proven about the name, so nothing may be
    /// released on the strength of it.
    Unknown(StoreError),
}

pub fn read_metadata(store: &Store, name: &str) -> MetaState {
    let raw = match store.get_elem(name, META_FIELD) {
        Ok(bytes) => bytes,
        Err(StoreError::KeyGone) => return MetaState::NoMap,
        Err(StoreError::ElemGone) => return MetaState::Damaged("no metadata element".to_owned()),
        Err(e) => return MetaState::Unknown(e),
    };
    match MetaRecord::decode(&raw) {
        Ok((meta, layout)) => MetaState::Usable(meta, layout),
        Err(e) => MetaState::Damaged(e.to_string()),
    }
}
