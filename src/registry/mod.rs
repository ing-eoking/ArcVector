//! The live index registry and its ownership rule.
//!
//! > The usearch index is a cache rebuildable from Map at any time.
//! > If it is not in Map, it does not exist.
//!
//! The Map's metadata element carries an `owner` token; comparing it against the
//! one this node holds decides whether the graph here may serve, must be rebuilt,
//! or is already being rebuilt. `docs/내부구조.md` §6.

#[cfg(recovery)]
mod metadata;
#[cfg(recovery)]
mod recovery;

#[cfg(recovery)]
pub use metadata::{MetaState, build_ann, read_metadata};
#[cfg(recovery)]
pub use recovery::{claim_refilled, ensure_builder, take_over};

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::usearch::AnnIndex;

/// `owner` while a rebuild is in flight: claimed by nobody.
pub const REBUILDING: u64 = 0;

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,
    /// Element-count limit for vectors, already excluding the metadata element.
    pub maxcount: u32,
    owner: std::sync::atomic::AtomicU64,
    /// Set by the rebuild thread when the graph is complete.
    #[cfg(recovery)]
    refilled: std::sync::atomic::AtomicBool,
}

impl VectorIndex {
    pub fn new(name: String, ann: AnnIndex, maxcount: u32, owner: u64) -> Self {
        Self {
            name,
            ann,
            maxcount,
            owner: std::sync::atomic::AtomicU64::new(owner),
            #[cfg(recovery)]
            refilled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn owner(&self) -> u64 {
        self.owner.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn is_rebuilding(&self) -> bool {
        self.owner() == REBUILDING
    }

    /// Whether the graph is full again and only the token write is outstanding.
    #[cfg(recovery)]
    pub fn is_refilled(&self) -> bool {
        self.refilled.load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(recovery)]
    fn mark_refilled(&self) {
        self.refilled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(recovery)]
    fn set_owner(&self, owner: u64) {
        self.owner
            .store(owner, std::sync::atomic::Ordering::Release);
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

/// Look a name up, releasing the registry lock immediately.
pub fn get(name: &str) -> Option<Arc<VectorIndex>> {
    read().get(name).cloned()
}

pub fn contains(name: &str) -> bool {
    read().contains_key(name)
}

/// Register `index` unless the name is taken, returning whichever ends up live.
pub fn insert_or_get(index: VectorIndex) -> (Arc<VectorIndex>, bool) {
    let mut reg = write();
    let mut inserted = false;
    let entry = reg.entry(index.name.clone()).or_insert_with(|| {
        inserted = true;
        Arc::new(index)
    });
    (Arc::clone(entry), inserted)
}

pub fn remove(name: &str) -> bool {
    write().remove(name).is_some()
}

/// Every index that is serving, ordered by name for stable `vlist` output.
pub fn snapshot() -> Vec<Arc<VectorIndex>> {
    let mut all: Vec<Arc<VectorIndex>> = read()
        .values()
        .filter(|index| !index.is_rebuilding())
        .cloned()
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}
