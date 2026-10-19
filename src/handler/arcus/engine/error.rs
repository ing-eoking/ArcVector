use std::fmt;
use std::os::raw::c_int;

use crate::engine_api::{
    ENGINE_ERROR_CODE_ENGINE_E2BIG, ENGINE_ERROR_CODE_ENGINE_EBADTYPE,
    ENGINE_ERROR_CODE_ENGINE_ELEM_EEXISTS, ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_EOVERFLOW, ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK,
    ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS, ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT,
    ENGINE_ERROR_CODE_ENGINE_NOT_STORED, ENGINE_ERROR_CODE_ENGINE_SUCCESS,
};

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Unavailable,
    AbiMismatch,
    KeyGone,
    ElemGone,

    ElemExists,

    /// 만들려던 키가 이미 있다(`ENGINE_KEY_EEXISTS`). 무엇이 있는지는 부른 쪽이
    /// 가린다 -- `vcreate`는 우리 인덱스면 `EXISTS`, 아니면 `TYPE_MISMATCH`.
    KeyExists,

    /// `add` refused because the key was already there. `vcreate` reads this
    /// as `EXISTS` -- it is how the engine answers "does this index exist?"
    /// without a separate read.
    NotStored,

    /// The engine refused an item this large: no slab class fits it. The
    /// authority on the limit, since `get_config` does not report it.
    TooBig,

    BadType,

    CorruptElement,
    Overflow,
    ReplicaSlave,

    /// sync 복제 마스터에서 쓰기가 복제 ack를 기다린다(`ENGINE_EWOULDBLOCK`).
    /// 엔진이 이미 `waitfor_io_complete`를 불렀고, ack가 오면 깨운다. 쓰기는
    /// 된 것이라 [`written`]이 성공으로 바꾼다 -- 쓰기 호출자에게는 오지 않는다.
    WouldBlock,
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
            Self::KeyExists => f.write_str("the key already exists"),
            Self::BadType => f.write_str("key holds an item that is not a Map"),
            Self::CorruptElement => {
                f.write_str("the engine described an element that cannot be read")
            }
            Self::NotStored => f.write_str("the key is already there"),
            Self::TooBig => {
                f.write_str("the value is larger than this server's item size limit (memcached -I)")
            }
            Self::Overflow => f.write_str("index is full"),
            Self::ReplicaSlave => {
                f.write_str("this node is a replica; vector commands are served by the master")
            }
            Self::WouldBlock => f.write_str("the engine is waiting for a replica"),
            Self::Engine(code) => write!(f, "engine error {code}"),
        }
    }
}

impl std::error::Error for StoreError {}

pub(super) type Result<T> = std::result::Result<T, StoreError>;

pub const ENGINE_REPL_SLAVE: u32 = 0x61;

pub(super) fn translate(code: u32) -> StoreError {
    match code {
        ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT => StoreError::KeyGone,
        ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT => StoreError::ElemGone,
        ENGINE_ERROR_CODE_ENGINE_ELEM_EEXISTS => StoreError::ElemExists,
        ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS => StoreError::KeyExists,
        ENGINE_ERROR_CODE_ENGINE_NOT_STORED => StoreError::NotStored,
        ENGINE_ERROR_CODE_ENGINE_E2BIG => StoreError::TooBig,
        ENGINE_ERROR_CODE_ENGINE_EBADTYPE => StoreError::BadType,
        ENGINE_ERROR_CODE_ENGINE_EOVERFLOW => StoreError::Overflow,
        ENGINE_REPL_SLAVE => StoreError::ReplicaSlave,
        ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK => StoreError::WouldBlock,
        other => StoreError::Engine(other),
    }
}

pub(super) fn as_int(len: usize) -> c_int {
    c_int::try_from(len).unwrap_or(c_int::MAX)
}

/// 읽기의 결과. 읽기는 복제 ack를 기다리지 않으므로 성공만 받는다.
pub(super) fn check(code: u32) -> Result<()> {
    if code == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        Ok(())
    } else {
        Err(translate(code))
    }
}

/// 엔진 **쓰기**의 결과. `WouldBlock`은 성공이다.
///
/// sync 복제 마스터는 받아들인 쓰기에도 ack를 기다리며 EWOULDBLOCK을 준다. 그
/// 쓰기는 이미 링크됐고 출력 인자(`replaced`/`created` 등)도 채워져 있다 --
/// 오류로 읽으면 엔진이 받은 쓰기를 거절했다고 답하고, `insert_elem`은 링크된
/// 요소를 풀어 버린다.
pub(super) fn written(code: u32) -> Result<()> {
    match check(code) {
        Err(StoreError::WouldBlock) => Ok(()),
        other => other,
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
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_KEY_EEXISTS),
            StoreError::KeyExists
        );
        assert_eq!(translate(9999), StoreError::Engine(9999));
    }

    /// Every write goes through [`written`]: a sync-replication master waiting
    /// on a slave gets EWOULDBLOCK from an accepted write, and reading that as
    /// an error would refuse a write the engine took. Reads never wait on a
    /// slave, so [`check`] keeps it an error.
    #[test]
    fn written_accepts_a_write_that_is_still_waiting_on_a_slave() {
        assert!(written(ENGINE_ERROR_CODE_ENGINE_SUCCESS).is_ok());
        assert!(
            written(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK).is_ok(),
            "the gate accepted the write; it is only waiting for an acknowledgement"
        );
        assert_eq!(
            written(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            Err(StoreError::KeyGone)
        );
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK),
            Err(StoreError::WouldBlock)
        );
    }

    /// sync 복제 마스터에서 쓰기가 ack를 기다리면 엔진이 EWOULDBLOCK을 준다.
    /// 엔진은 이미 `waitfor_io_complete`를 불렀고 ack가 오면 깨운다 -- 쓰기는
    /// 된 것이다.
    #[test]
    fn ewouldblock_from_a_write_counts_as_written() {
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK),
            StoreError::WouldBlock
        );
        assert_eq!(written(ENGINE_ERROR_CODE_ENGINE_SUCCESS), Ok(()));
        assert_eq!(written(ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK), Ok(()));
        assert_eq!(
            written(ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT),
            Err(StoreError::ElemGone)
        );
    }

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
