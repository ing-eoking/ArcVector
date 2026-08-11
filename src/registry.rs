//! The live index registry and its one consistency rule.
//!
//! > The usearch index is a cache rebuildable from Map at any time.
//! > If it is not in Map, it does not exist.
//!
//! Restart, eviction, TTL expiry and replication slaves all follow from that.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};

use crate::error::{Error, Result};
use crate::search::AnnIndex;
use crate::storage::map::Store;

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,
    /// Element-count limit inherited from the backing Map.
    pub maxcount: u32,
    /// Serializes the lazy rebuild so two workers cannot rebuild together.
    built: Mutex<bool>,
}

impl VectorIndex {
    pub fn new(name: String, ann: AnnIndex, maxcount: u32, built: bool) -> VectorIndex {
        VectorIndex {
            name,
            ann,
            maxcount,
            built: Mutex::new(built),
        }
    }

    /// Populate the usearch index from Map on first use.
    ///
    /// This is the cold-start path after a restart and on replication slaves,
    /// where Map holds the data but the in-memory graph does not exist yet.
    pub fn ensure_built(&self, store: &Store) -> Result<()> {
        let mut built = self.built.lock().unwrap_or_else(PoisonError::into_inner);
        if *built {
            return Ok(());
        }
        for (field, value) in store.get_all(&self.name)? {
            let element = self
                .ann
                .layout
                .decode(&value)
                .map_err(|e| Error::bad_request(format!("{e} in element '{field}'")))?;
            self.ann.add(&field, element.vector)?;
        }
        *built = true;
        Ok(())
    }
}

static INDICES: LazyLock<RwLock<HashMap<String, Arc<VectorIndex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn read() -> std::sync::RwLockReadGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.read().unwrap_or_else(PoisonError::into_inner)
}

fn write() -> std::sync::RwLockWriteGuard<'static, HashMap<String, Arc<VectorIndex>>> {
    INDICES.write().unwrap_or_else(PoisonError::into_inner)
}

/// Look an index up, releasing the registry lock immediately.
///
/// A concurrent `vdrop` may unregister it while the caller works; the returned
/// `Arc` keeps it alive until the caller is done.
pub fn get(name: &str) -> Option<Arc<VectorIndex>> {
    read().get(name).cloned()
}

/// Like [`get`], but reports a missing index as a client error.
pub fn require(name: &str) -> Result<Arc<VectorIndex>> {
    get(name).ok_or(Error::NoSuchIndex)
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

/// Register `index` unless its name is already taken.
pub fn insert(index: VectorIndex) -> bool {
    let mut reg = write();
    match reg.entry(index.name.clone()) {
        std::collections::hash_map::Entry::Occupied(_) => false,
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(Arc::new(index));
            true
        }
    }
}

pub fn remove(name: &str) -> bool {
    write().remove(name).is_some()
}

/// Every registered index, ordered by name for stable `vlist` output.
pub fn snapshot() -> Vec<Arc<VectorIndex>> {
    let mut all: Vec<Arc<VectorIndex>> = read().values().cloned().collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Metric;
    use crate::storage::element::Layout;
    use crate::storage::quantize::Quant;

    fn index(name: &str) -> VectorIndex {
        let ann = AnnIndex::new(Layout::new(4, Quant::F32), Metric::L2, 0, 0, 0, 2).unwrap();
        VectorIndex::new(name.to_owned(), ann, 100, true)
    }

    #[test]
    fn insert_is_rejected_for_a_duplicate_name() {
        let name = "registry-dup-test";
        assert!(insert(index(name)));
        assert!(
            !insert(index(name)),
            "second insert must not replace the first"
        );
        assert!(remove(name));
        assert!(!remove(name));
    }

    #[test]
    fn get_returns_none_after_removal() {
        let name = "registry-lifecycle-test";
        assert!(get(name).is_none());
        assert!(insert(index(name)));
        assert!(contains(name));

        // An in-flight caller holds the index alive past deregistration.
        let held = get(name).unwrap();
        assert!(remove(name));
        assert!(get(name).is_none());
        assert_eq!(held.name, name);
    }

    #[test]
    fn snapshot_is_ordered_by_name() {
        for n in ["registry-sort-b", "registry-sort-a", "registry-sort-c"] {
            insert(index(n));
        }
        let names: Vec<String> = snapshot()
            .iter()
            .map(|i| i.name.clone())
            .filter(|n| n.starts_with("registry-sort-"))
            .collect();
        assert_eq!(
            names,
            ["registry-sort-a", "registry-sort-b", "registry-sort-c"]
        );
        for n in ["registry-sort-a", "registry-sort-b", "registry-sort-c"] {
            remove(n);
        }
    }
}
