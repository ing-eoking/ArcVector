use std::sync::Arc;

pub(super) const STAGED_TAG: u64 = 1 << 63;

pub(super) const fn is_staged(key: u64) -> bool {
    key & STAGED_TAG != 0
}

pub trait Elements: Send + Sync {
    fn id_at(&self, addr: u64) -> Option<Arc<str>>;

    fn release(&self, addrs: &[u64]);
}
