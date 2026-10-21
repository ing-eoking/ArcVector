use crate::engine::{Store, StoreError};
use crate::error::{Error, Result};
use crate::index::registry::{self, Reader};

/// The registry answers, once the engine has had a chance to disagree.
///
/// The index name is the key, and indexes share the key space with every
/// other item. An index exists in this process when its Map has been linked
/// and the event callback read the settings out of the Map's value and
/// registered it -- by `vop create` on this node, by arcus replicating the Map
/// in, or by the persistence log replaying it at startup. So "is this Map
/// ours" is the registry's answer, and the value is not read again here.
///
/// | engine | registry | answer |
/// |---|---|---|
/// | no such key | -- | `NoSuchIndex` |
/// | not a Map | -- | `TypeMismatch` |
/// | a Map | has the name | the index |
/// | a Map | does not | `TypeMismatch` |
/// For anything that *removes* an index, the unlink of its Map is the whole
/// answer and there is nothing to read back.
///
/// Three removals never unlink the Map on their own, though. A `flush_all`, a
/// prefix flush, and a plain expiry of the Map (`vop create ... EXPTIME`) all
/// leave it linked and merely doomed: `do_item_isvalid` would refuse it, but
/// nobody runs that until somebody asks for the key. Until then the registry
/// would go on serving an index the server considers gone.
///
/// So ask. [`Store::probe_map`] is a `getattr`, which makes the engine run
/// `do_item_isvalid` and unlink the Map right there if it fails -- and that
/// unlink comes back as the event that drops the index from the registry,
/// before the lookup below runs. One cache-lock round trip per command,
/// against a search that takes thousands.
pub(super) fn resolve(store: &Store, name: &str) -> Result<Reader> {
    // `KeyGone` is the answer here, not a failure: the Map is gone, so the
    // index is. Any other error means the engine could not be asked at all --
    // fall through to the registry rather than refuse a command over it.
    let index = match store.probe_map(name) {
        Err(StoreError::KeyGone) => return Err(Error::NoSuchIndex),
        Ok(false) => return Err(Error::TypeMismatch),
        Ok(true) => registry::get(name).ok_or(Error::TypeMismatch)?,
        Err(_) => registry::get(name).ok_or(Error::NoSuchIndex)?,
    };
    // Pin it. If it was removed in the meantime it no longer exists -- a removed index accepts
    // no new commands.
    index.read().ok_or(Error::NoSuchIndex)
}

/// Only readers check the halt flag.
///
/// A halted index has a graph that disagrees with storage -- it failed to enqueue onto the
/// sweeper and lost addresses it should have released. In that state only **dereferencing** is
/// dangerous, so only lookups are blocked. Writes do not follow pointers, and blocking them would
/// not make the sweeper drain any faster.
pub(super) fn for_read(store: &Store, name: &str) -> Result<Reader> {
    let index = resolve(store, name)?;
    if index.ann.is_halted() {
        return Err(Error::Busy);
    }
    Ok(index)
}

pub(super) fn for_write(store: &Store, name: &str) -> Result<Reader> {
    resolve(store, name)
}

/// Drops a registry entry whose backing storage turned out to be gone.
///
/// Reached when a command finds the engine has no item where the index says
/// there is one. `remove_if_stale` is what keeps this from racing a `vop create`
/// that registered the same name after `stamp` was taken.
pub(super) fn map_is_gone(name: &str, stamp: u64) {
    if registry::remove_if_stale(name, stamp) {
        eprintln!("ArcVector: index '{name}' is no longer in the engine; releasing its graph");
    }
}
