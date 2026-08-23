//! `EWOULDBLOCK` is not a failure — see [`completed`] and `docs/내부구조.md` §8.

use std::fmt;
use std::os::raw::c_int;

use crate::engine_api::{
    ENGINE_ERROR_CODE_ENGINE_EBADTYPE, ENGINE_ERROR_CODE_ENGINE_ELEM_EEXISTS,
    ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT, ENGINE_ERROR_CODE_ENGINE_EOVERFLOW,
    ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK, ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_SUCCESS,
};

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Unavailable,
    AbiMismatch,
    KeyGone,
    ElemGone,
    /// The field is already in the Map. Only an insert that refuses to replace can see it.
    ElemExists,
    /// The key holds an item that is not a Map.
    BadType,
    /// `get_elem_info` described an element that cannot be one.
    ///
    /// Reading the bytes it points at would go outside the allocation, so the caller is handed
    /// this instead. It is not a use-after-free detector — freed slab memory usually still
    /// reads, and holds another element by then — but the description of *that* element does
    /// not fit the one we asked for, and the mismatch is what shows up here.
    CorruptElement,
    Overflow,
    ReplicaSlave,
    Engine(u32),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("engine unavailable"),
            Self::AbiMismatch => f.write_str(
                "engine ABI mismatch — this library was built against a different \
                 arcus configuration than the server; see docs/내부구조.md §11",
            ),
            Self::KeyGone => f.write_str("index not found in engine"),
            Self::ElemGone => f.write_str("element not found"),
            Self::ElemExists => f.write_str("element already exists"),
            Self::BadType => f.write_str("key holds an item that is not a Map"),
            Self::CorruptElement => {
                f.write_str("the engine described an element that cannot be read")
            }
            Self::Overflow => f.write_str("index is full"),
            Self::ReplicaSlave => {
                f.write_str("this node is a replica; vector commands are served by the master")
            }
            Self::Engine(code) => write!(f, "engine error {code}"),
        }
    }
}

impl std::error::Error for StoreError {}

pub(super) type Result<T> = std::result::Result<T, StoreError>;

/// `ENGINE_REPL_SLAVE`, which the engine returns for a write on a replica.
const ENGINE_REPL_SLAVE: u32 = 0x61;

pub(super) fn translate(code: u32) -> StoreError {
    match code {
        ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT => StoreError::KeyGone,
        ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT => StoreError::ElemGone,
        ENGINE_ERROR_CODE_ENGINE_ELEM_EEXISTS => StoreError::ElemExists,
        ENGINE_ERROR_CODE_ENGINE_EBADTYPE => StoreError::BadType,
        ENGINE_ERROR_CODE_ENGINE_EOVERFLOW => StoreError::Overflow,
        ENGINE_REPL_SLAVE => StoreError::ReplicaSlave,
        other => StoreError::Engine(other),
    }
}

/// Length of a key or field as the engine's `int` parameters want it.
pub(super) fn as_int(len: usize) -> c_int {
    c_int::try_from(len).unwrap_or(c_int::MAX)
}

/// Whether an engine code means the operation completed.
pub(super) fn completed(code: u32) -> bool {
    code == ENGINE_ERROR_CODE_ENGINE_SUCCESS || code == ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK
}

pub(super) fn check(code: u32) -> Result<()> {
    if completed(code) {
        Ok(())
    } else {
        Err(translate(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_codes_map_to_distinguishable_errors() {
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            StoreError::KeyGone
        );
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT),
            StoreError::ElemGone
        );
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_ELEM_EEXISTS),
            StoreError::ElemExists
        );
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_EBADTYPE),
            StoreError::BadType
        );
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_EOVERFLOW),
            StoreError::Overflow
        );
        assert_eq!(translate(9999), StoreError::Engine(9999));
    }

    #[test]
    fn check_only_accepts_success() {
        assert!(check(ENGINE_ERROR_CODE_ENGINE_SUCCESS).is_ok());
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            Err(StoreError::KeyGone)
        );
    }

    /// The check exists so a bad length never becomes a slice; the messages have to name that.
    #[test]
    fn a_corrupt_element_reads_as_a_server_side_failure() {
        let e: crate::error::Error = StoreError::CorruptElement.into();
        assert_eq!(e.blame(), crate::error::Blame::Server);
        assert!(
            StoreError::CorruptElement.to_string().contains("read"),
            "the wire message should say the element could not be read"
        );
    }
}
