pub(crate) mod pool;
pub mod sweep;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry::{self, VectorIndex};

/// The registry answers, once the engine has had a chance to disagree.
///
/// An index exists in this process when its metadata item has been linked and
/// the trigger callback registered it -- by `vcreate` on this node, by arcus
/// replicating the item in, or by the persistence log replaying it at startup.
/// All three reach `do_item_link`, so for anything that *removes* an index the
/// callback is the whole answer and there is nothing to read back.
///
/// Three removals never reach `do_item_unlink` on their own, though. A
/// `flush_all`, a prefix invalidated under the index, and a plain expiry all
/// leave the metadata item linked and merely doomed: `do_item_isvalid` would
/// refuse it, but nobody runs that until somebody asks for the key. Until then
/// the registry would go on serving an index the server considers gone.
///
/// So ask. [`Store::touch_kv`] is a `get` and an immediate release, which
/// makes the engine run `do_item_isvalid` and unlink the item right there if
/// it fails -- and that unlink comes back as the `EVENT_UNLINK` that drops the
/// index from the registry, before the lookup below runs. One cache-lock round
/// trip per command, against a search that takes thousands.
///
/// That is what the `Store` in this signature was being kept for.
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    // `KeyGone` is the answer here, not a failure: the metadata is gone, so the
    // index is. Any other error means the engine could not be asked at all --
    // fall through to the registry rather than refuse a command over it.
    if let Err(StoreError::KeyGone) = store.touch_kv(&crate::trigger::key::meta_key(name)) {
        return Err(Error::NoSuchIndex);
    }
    registry::serving(name).ok_or(Error::NoSuchIndex)
}

/// 읽는 쪽만 잠금을 본다.
///
/// 잠긴 인덱스는 그래프가 저장소와 어긋나 있다 -- sweeper의 큐에 못 넣어 놓아줄
/// 주소를 잃었다는 뜻이다. 그 상태에서 위험한 것은 **역참조하는 쪽**뿐이라
/// 조회만 막는다. 쓰기는 포인터를 따라가지 않고, 막아봐야 sweeper가 더 빨리
/// 비우지도 않는다.
pub(super) fn for_read(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let index = resolve(store, name)?;
    if index.ann.is_halted() {
        return Err(Error::Busy);
    }
    Ok(index)
}

pub(super) fn for_write(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    resolve(store, name)
}

/// Drops a registry entry whose backing storage turned out to be gone.
///
/// Reached when a command finds the engine has no item where the index says
/// there is one. `remove_if_stale` is what keeps this from racing a `vcreate`
/// that registered the same name after `stamp` was taken.
pub(super) fn map_is_gone(name: &str, stamp: u64) {
    if registry::remove_if_stale(name, stamp) {
        eprintln!("ArcVector: index '{name}' is no longer in the engine; releasing its graph");
    }
}
