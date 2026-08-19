//! `EWOULDBLOCK` is not a failure — see [`completed`] and `docs/내부구조.md` §8.

use std::fmt;
use std::os::raw::c_int;

use crate::engine_api::{
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
}
