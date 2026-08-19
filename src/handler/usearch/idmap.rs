//! The `String` ↔ `u64` mapping usearch needs, and the deletes a rebuild must
//! not undo.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Default)]
pub(super) struct IdMap {
    pub(super) by_key: HashMap<u64, Arc<str>>,
    pub(super) by_id: HashMap<Arc<str>, u64>,
    /// Ids deleted while a rebuild is running.
    pub(super) tombstones: HashSet<Arc<str>>,
    pub(super) next: u64,
    /// Total length of the live id strings, kept as a running sum.
    id_bytes: usize,
}

impl IdMap {
    /// The key for `id`, minting one if it is new. Returns whether it existed.
    pub(super) fn intern(&mut self, id: &str) -> (u64, bool) {
        if let Some(key) = self.by_id.get(id) {
            return (*key, true);
        }
        let key = self.next;
        self.next += 1;
        self.id_bytes += id.len();
        let id: Arc<str> = Arc::from(id);
        self.by_key.insert(key, Arc::clone(&id));
        self.by_id.insert(id, key);
        (key, false)
    }

    pub(super) fn forget(&mut self, id: &str) -> Option<u64> {
        let key = self.by_id.remove(id)?;
        if let Some(gone) = self.by_key.remove(&key) {
            self.id_bytes -= gone.len();
            self.tombstones.remove(&gone);
        }
        Some(key)
    }

    /// Forget `id`, but remember that it was deleted.
    pub(super) fn forget_tombstoned(&mut self, id: &str) -> Option<u64> {
        let key = self.by_id.remove(id)?;
        let gone = self.by_key.remove(&key)?;
        self.id_bytes -= gone.len();
        self.tombstones.insert(gone);
        Some(key)
    }

    /// Whether a rebuild should leave this id alone: already present, or deleted
    /// by the live path since the rebuild began.
    pub(super) fn is_known(&self, id: &str) -> bool {
        self.by_id.contains_key(id) || self.tombstones.contains(id)
    }

    /// Module memory held by this mapping.
    pub(super) fn bytes(&self) -> usize {
        const ARC_HEADER: usize = 16; // strong + weak counts
        let per_entry = size_of::<u64>() + size_of::<Arc<str>>();
        // Both directions hold a key and an Arc, and the text is shared once.
        self.by_key.len() * per_entry * 2 + self.by_key.len() * ARC_HEADER + self.id_bytes
    }
}
