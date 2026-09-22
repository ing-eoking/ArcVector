use std::sync::Arc;

pub trait Elements: Send + Sync {
    fn id_at(&self, addr: u64) -> Option<Arc<str>>;

    fn release(&self, addrs: &[u64]);
}
