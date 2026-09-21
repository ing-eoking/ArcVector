pub(crate) mod pool;
pub mod sweep;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handler::arcus::engine::Store;
use crate::handler::registry::{self, VectorIndex};

/// The registry is the whole answer.
///
/// An index exists in this process exactly when its metadata item has been
/// linked and the trigger callback registered it -- by `vcreate` on this node,
/// by arcus replicating the item in, or by the persistence log replaying it at
/// startup. All three reach `do_item_link`, so there is nothing left for a
/// command to read back from the engine: no ownership token to compare against
/// a Map, and no cold graph to rebuild, because what a search sees is what the
/// callback put there.
///
/// That is why this takes a `Store` it does not use. Every caller has one, and
/// keeping it in the signature means the storage layer can come back into this
/// decision without touching them again.
pub(super) fn resolve(_store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
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
