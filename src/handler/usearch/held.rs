use std::collections::{HashSet, TryReserveError};
use std::sync::Arc;

pub(super) const STAGED_TAG: u64 = 1 << 63;

pub(super) const fn is_staged(key: u64) -> bool {
    key & STAGED_TAG != 0
}

#[derive(Default)]
pub(super) struct HeldSet {
    live: HashSet<u64>,

    tombstones: HashSet<u64>,
}

impl HeldSet {
    pub(super) fn len(&self) -> usize {
        self.live.len()
    }

    pub(super) fn contains(&self, addr: u64) -> bool {
        self.live.contains(&addr)
    }

    pub(super) fn is_known(&self, addr: u64) -> bool {
        self.live.contains(&addr) || self.tombstones.contains(&addr)
    }

    pub(super) fn reserve(&mut self) -> Result<(), TryReserveError> {
        self.live.try_reserve(1)
    }

    pub(super) fn reserve_tombstone(&mut self) -> Result<(), TryReserveError> {
        self.tombstones.try_reserve(1)
    }

    pub(super) fn publish(&mut self, addr: u64) {
        debug_assert!(
            self.live.capacity() > self.live.len() || self.live.contains(&addr),
            "publish without reserve: the insert below can abort"
        );
        self.live.insert(addr);
        self.tombstones.remove(&addr);
    }

    pub(super) fn take(&mut self, addr: u64) -> bool {
        self.live.remove(&addr)
    }

    pub(super) fn take_tombstoned(&mut self, addr: u64) -> bool {
        let held = self.take(addr);
        self.tombstones.insert(addr);
        held
    }

    pub(super) fn take_all(&mut self) -> Vec<u64> {
        self.tombstones.clear();
        self.live.drain().collect()
    }

    pub(super) fn forget_tombstones(&mut self) {
        self.tombstones.clear();
    }

    #[cfg(test)]
    pub(super) fn live_addrs(&self) -> Vec<u64> {
        self.live.iter().copied().collect()
    }

    pub(super) fn bytes(&self) -> usize {
        (self.live.capacity() + self.tombstones.capacity()) * std::mem::size_of::<u64>()
    }
}

pub trait Elements: Send + Sync {
    fn id_at(&self, addr: u64) -> Option<Arc<str>>;

    fn release(&self, addrs: &[u64]);
}
