use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use ::usearch::{Index, IndexOptions, ScalarKind, b1x8, f16};

use super::held::{self, Elements, HeldSet};
use super::metric::Metric;
use crate::error::Error;
use crate::handler::arcus::element::Layout;
use crate::handler::quant::Quant;

type Result<T> = std::result::Result<T, Error>;

fn usearch_err(e: impl std::fmt::Display) -> Error {
    Error::Index(e.to_string())
}

const fn scalar_kind(q: Quant) -> ScalarKind {
    match q {
        Quant::F32 => ScalarKind::F32,
        Quant::F16 => ScalarKind::F16,
        Quant::I8 => ScalarKind::I8,
        Quant::B1 => ScalarKind::B1,
    }
}

const MIN_CAPACITY: usize = 1024;

const THREAD_SLOTS: usize = 64;

pub type Accept<'a> = &'a dyn Fn(u64) -> bool;

#[must_use = "an unnamed node is invisible; publish it or discard it"]
pub struct Staged<'a> {
    key: u64,

    owner: u64,

    in_flight: &'a AtomicUsize,
}

impl Staged<'_> {
    pub fn key(&self) -> u64 {
        self.key
    }
}

impl<'a> Staged<'a> {
    fn new(key: u64, owner: u64, in_flight: &'a AtomicUsize) -> Self {
        in_flight.fetch_add(1, Ordering::Relaxed);
        Staged {
            key,
            owner,
            in_flight,
        }
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for AnnIndex {
    fn drop(&mut self) {
        let outgoing = self
            .held
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take_all();
        if !outgoing.is_empty() {
            self.elements.release(&outgoing);
        }
    }
}

#[derive(Debug)]
pub enum PublishError<E> {
    Store(E),

    Mapping(Error),
}

#[must_use = "the count only covers the write while this is alive"]
pub struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        InFlight(count)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct AnnIndex {
    pub layout: Layout,
    pub metric: Metric,
    threads: usize,

    inner: RwLock<Index>,
    reserved: AtomicUsize,

    held: RwLock<HeldSet>,

    epoch: AtomicU64,

    next_placeholder: AtomicU64,

    elements: Arc<dyn Elements>,

    in_flight: AtomicUsize,

    rebuilding: AtomicBool,
}

impl AnnIndex {
    pub fn new(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
        elements: Arc<dyn Elements>,
    ) -> Result<Self> {
        Self::with_threads(
            layout,
            metric,
            connectivity,
            expansion_add,
            expansion_search,
            THREAD_SLOTS,
            elements,
        )
    }

    fn with_threads(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
        threads: usize,
        elements: Arc<dyn Elements>,
    ) -> Result<Self> {
        metric.check_quant(layout.quant)?;

        let options = IndexOptions {
            dimensions: layout.dim,
            metric: metric.kind(),
            quantization: scalar_kind(layout.quant),
            connectivity,
            expansion_add,
            expansion_search,
            multi: false,
        };
        let index = Index::new(&options).map_err(usearch_err)?;
        let threads = threads.max(1);
        index
            .reserve_capacity_and_threads(MIN_CAPACITY, threads)
            .map_err(usearch_err)?;

        Ok(Self {
            layout,
            metric,
            threads,
            inner: RwLock::new(index),
            reserved: AtomicUsize::new(MIN_CAPACITY),
            in_flight: AtomicUsize::new(0),
            next_placeholder: AtomicU64::new(0),
            elements,
            epoch: AtomicU64::new(0),
            held: RwLock::new(HeldSet::default()),
            rebuilding: AtomicBool::new(false),
        })
    }

    pub fn len(&self) -> usize {
        self.held().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn held(&self) -> std::sync::RwLockReadGuard<'_, HeldSet> {
        self.held.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn addr_set_bytes(&self) -> usize {
        self.held().bytes()
    }

    pub fn held_bytes(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .memory_usage()
    }

    pub fn used_bytes(&self) -> usize {
        let s = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .memory_stats();
        let tape = |allocated: usize, wasted: usize, reserved: usize| {
            allocated.saturating_sub(wasted).saturating_sub(reserved)
        };
        tape(s.graph_allocated, s.graph_wasted, s.graph_reserved)
            + tape(s.vectors_allocated, s.vectors_wasted, s.vectors_reserved)
    }

    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Acquire)
    }

    fn ensure_capacity(&self, needed: usize) -> Result<()> {
        if needed <= self.reserved.load(Ordering::Acquire) {
            return Ok(());
        }
        let index = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let current = self.reserved.load(Ordering::Acquire);
        if needed <= current {
            return Ok(());
        }
        let target = (current * 2).max(needed).max(MIN_CAPACITY);
        index
            .reserve_capacity_and_threads(target, self.threads)
            .map_err(usearch_err)?;
        self.reserved.store(target, Ordering::Release);
        Ok(())
    }

    pub fn stage(&self, vector: &[u8], owner: u64) -> Result<Staged<'_>> {
        self.check_vector(vector)?;

        self.ensure_capacity(self.live() + THREAD_SLOTS)?;

        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);

        let key = held::STAGED_TAG | self.next_placeholder.fetch_add(1, Ordering::Relaxed);

        let staged = Staged::new(key, owner, &self.in_flight);

        self.typed_add(&index, key, vector)?;
        Ok(staged)
    }

    pub fn insert_published<E>(
        &self,
        staged: Staged<'_>,
        owner: u64,
        displaced: impl FnOnce() -> Option<u64>,
        link: impl FnOnce() -> std::result::Result<u64, E>,
    ) -> std::result::Result<(), PublishError<E>> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        let live = staged.owner == owner;

        if live && let Err(e) = held.reserve() {
            drop(held);
            drop(index);
            let key = staged.key;
            drop(staged);
            self.drop_node(key);
            return Err(PublishError::Mapping(Error::Index(format!(
                "the held set could not grow: {e}"
            ))));
        }

        let displaced = displaced();
        let written = link();
        let key = staged.key;
        drop(staged);

        let addr = match written {
            Ok(addr) => addr,
            Err(e) => {
                if let Some(old) = displaced {
                    self.elements.release(&[old]);
                }
                drop(held);
                drop(index);
                self.drop_node(key);
                return Err(PublishError::Store(e));
            }
        };

        if !live {
            let mut back = vec![addr];
            back.extend(displaced);
            self.elements.release(&back);
            drop(held);
            drop(index);
            self.drop_node(key);
            return Ok(());
        }

        let renamed = index.rename(key, addr);
        if let Err(e) = renamed {
            let mut back = vec![addr];
            back.extend(displaced);
            self.elements.release(&back);
            drop(held);
            drop(index);
            eprintln!("ArcVector: could not name a published node: {e}");
            self.drop_node(key);
            return Ok(());
        }
        held.publish(addr);

        let retired = displaced.is_some_and(|old| held.take(old));
        if let Some(old) = displaced {
            let back = if retired { vec![old, old] } else { vec![old] };
            self.elements.release(&back);
        }
        drop(held);
        drop(index);
        if retired && let Some(old) = displaced {
            self.drop_node(old);
        }
        Ok(())
    }

    pub fn update_published<E>(
        &self,
        locate: impl FnOnce() -> Option<(u64, Vec<u8>)>,
        write: impl FnOnce(Vec<u8>) -> std::result::Result<u64, E>,
    ) -> std::result::Result<Option<()>, PublishError<E>> {
        let _slack = InFlight::new(&self.in_flight);

        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);

        let Some((old, value)) = locate() else {
            return Ok(None);
        };

        if let Err(e) = held.reserve() {
            return Err(PublishError::Mapping(Error::Index(format!(
                "the held set could not grow: {e}"
            ))));
        }

        let new = write(value).map_err(PublishError::Store)?;

        if !held.take(old) {
            self.elements.release(&[new]);
            return Ok(Some(()));
        }

        self.elements.release(&[old]);

        if let Err(e) = index.rename(old, new) {
            self.elements.release(&[new]);
            drop(held);
            drop(index);
            eprintln!("ArcVector: could not move a node to its new element: {e}");
            self.drop_node(old);
            return Ok(Some(()));
        }
        held.publish(new);
        Ok(Some(()))
    }

    pub fn forget_unreadable(&self, addr: u64) -> bool {
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        let was_held =
            if self.rebuilding.load(Ordering::Acquire) && held.reserve_tombstone().is_ok() {
                held.take_tombstoned(addr)
            } else {
                held.take(addr)
            };
        if was_held {
            self.elements.release(&[addr]);
        }
        drop(held);
        self.drop_node(addr);
        was_held
    }

    pub fn discard(&self, staged: Staged<'_>) {
        self.drop_node(staged.key);
    }

    fn drop_node(&self, key: u64) {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let _ = index.remove(key);
    }

    pub fn add_unless_known(
        &self,
        addr: u64,
        vector: impl FnOnce() -> Result<Option<Vec<u8>>>,
    ) -> Result<bool> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);

        if held.is_known(addr) {
            return Ok(false);
        }
        let Some(vector) = vector()? else {
            return Ok(false);
        };
        if vector.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "vector is {} bytes, expected {}",
                vector.len(),
                self.layout.vector_bytes()
            )));
        }
        held.reserve()
            .map_err(|e| Error::Index(format!("the held set could not grow: {e}")))?;
        self.typed_add(&index, addr, &vector)?;
        held.publish(addr);
        Ok(true)
    }

    fn check_vector(&self, vector: &[u8]) -> Result<()> {
        if vector.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "vector is {} bytes, expected {}",
                vector.len(),
                self.layout.vector_bytes()
            )));
        }
        Ok(())
    }

    fn live(&self) -> usize {
        self.held().len()
    }

    fn typed_add(&self, index: &Index, key: u64, vector: &[u8]) -> Result<()> {
        match self.layout.quant {
            Quant::F32 => index.add(key, &to_f32(vector)),
            Quant::F16 => index.add(key, f16::from_i16s(&to_i16(vector))),
            Quant::I8 => index.add(key, &to_i8(vector)),
            Quant::B1 => index.add(key, b1x8::from_u8s(vector)),
        }
        .map_err(usearch_err)
    }

    pub fn vector_of(&self, addr: u64) -> Result<Option<Vec<u8>>> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let held = self.held();
        if !held.contains(addr) {
            return Ok(None);
        }
        let key = addr;
        let dim = self.layout.dim;

        let bytes = match self.layout.quant {
            Quant::F32 => {
                let mut out = vec![0f32; dim];
                index.get(key, &mut out).map_err(usearch_err)?;
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::F16 => {
                let mut out = vec![0i16; dim];
                index
                    .get(key, f16::from_mut_i16s(&mut out))
                    .map_err(usearch_err)?;
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::I8 => {
                let mut out = vec![0i8; dim];
                index.get(key, &mut out).map_err(usearch_err)?;
                out.iter().map(|v| *v as u8).collect()
            }
            Quant::B1 => {
                let mut out = vec![0u8; dim];
                index
                    .get(key, b1x8::from_mut_u8s(&mut out))
                    .map_err(usearch_err)?;
                out.truncate(self.layout.vector_bytes());
                out
            }
        };
        if bytes.len() != self.layout.vector_bytes() {
            return Err(Error::Index(format!(
                "the graph returned {} bytes for {addr:#x}, expected {}",
                bytes.len(),
                self.layout.vector_bytes()
            )));
        }
        Ok(Some(bytes))
    }

    pub fn reserve(&self, count: usize) -> Result<()> {
        self.ensure_capacity(count)
    }

    pub fn begin_rebuild(&self) -> Result<()> {
        self.rebuilding.store(true, Ordering::Release);
        self.clear_with()
    }

    pub fn end_rebuild(&self) {
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        self.rebuilding.store(false, Ordering::Release);
        held.forget_tombstones();
    }

    pub fn clear_with(&self) -> Result<()> {
        let index = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        index.reset().map_err(usearch_err)?;

        index
            .reserve_capacity_and_threads(MIN_CAPACITY, self.threads)
            .map_err(usearch_err)?;
        self.reserved.store(MIN_CAPACITY, Ordering::Release);

        let outgoing = held.take_all();
        if !outgoing.is_empty() {
            self.elements.release(&outgoing);
        }
        self.epoch.fetch_add(1, Ordering::Release);
        drop(held);
        Ok(())
    }

    pub fn remove_published<E>(
        &self,
        take: impl FnOnce() -> std::result::Result<Option<u64>, E>,
    ) -> std::result::Result<Option<bool>, PublishError<E>> {
        let _slack = InFlight::new(&self.in_flight);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);

        let tombstone = self.rebuilding.load(Ordering::Acquire);
        if tombstone && let Err(e) = held.reserve_tombstone() {
            return Err(PublishError::Mapping(Error::Index(format!(
                "the held set could not grow: {e}"
            ))));
        }

        let Some(addr) = take().map_err(PublishError::Store)? else {
            return Ok(None);
        };

        let had_node = if tombstone {
            held.take_tombstoned(addr)
        } else {
            held.take(addr)
        };
        if had_node {
            self.elements.release(&[addr]);
        }
        drop(held);

        if had_node {
            self.drop_node(addr);
        }
        Ok(Some(had_node))
    }

    fn resolve(&self, hits: &[(u64, f32)], entered: u64) -> Vec<(u64, Arc<str>, f32)> {
        let held = self.held();
        if self.epoch.load(Ordering::Acquire) != entered {
            return Vec::new();
        }
        hits.iter()
            .filter_map(|(key, distance)| {
                let id = self.name(&held, *key)?;
                Some((*key, id, *distance))
            })
            .collect()
    }

    fn name(&self, held: &HeldSet, key: u64) -> Option<Arc<str>> {
        if !Self::is_named(held, key) {
            return None;
        }
        self.elements.id_at(key)
    }

    fn is_named(held: &HeldSet, key: u64) -> bool {
        !held::is_staged(key) && held.contains(key)
    }

    fn unfiltered(&self, index: &Index, query: &[u8], k: usize) -> Result<::usearch::ffi::Matches> {
        match self.layout.quant {
            Quant::F32 => index.search(&to_f32(query), k),
            Quant::F16 => index.search(f16::from_i16s(&to_i16(query)), k),
            Quant::I8 => index.search(&to_i8(query), k),
            Quant::B1 => index.search(b1x8::from_u8s(query), k),
        }
        .map_err(usearch_err)
    }

    fn matches<P: Fn(u64) -> bool>(
        &self,
        index: &Index,
        query: &[u8],
        k: usize,
        predicate: P,
    ) -> Result<::usearch::ffi::Matches> {
        match self.layout.quant {
            Quant::F32 => index.filtered_search(&to_f32(query), k, predicate),
            Quant::F16 => index.filtered_search(f16::from_i16s(&to_i16(query)), k, predicate),
            Quant::I8 => index.filtered_search(&to_i8(query), k, predicate),
            Quant::B1 => index.filtered_search(b1x8::from_u8s(query), k, predicate),
        }
        .map_err(usearch_err)
    }

    pub fn search(
        &self,
        query: &[u8],
        k: usize,
        accept: Option<Accept<'_>>,
    ) -> Result<Vec<(u64, Arc<str>, f32)>> {
        if query.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "query is {} bytes, expected {}",
                query.len(),
                self.layout.vector_bytes()
            )));
        }

        let entered = self.epoch.load(Ordering::Acquire);

        let k = k.saturating_add(self.in_flight.load(Ordering::Relaxed));

        let matches = {
            let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            match accept {
                None => self.unfiltered(&index, query, k),

                Some(matches) => self.matches(&index, query, k, |key| {
                    let held = self.held();
                    Self::is_named(&held, key) && matches(key)
                }),
            }?
        };

        let hits: Vec<(u64, f32)> = matches.keys.into_iter().zip(matches.distances).collect();
        Ok(self.resolve(&hits, entered))
    }
}

fn to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn to_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn to_i8(bytes: &[u8]) -> Vec<i8> {
    bytes.iter().map(|b| *b as i8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::quant::Quant;
    use crate::handler::usearch::metric::Metric;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeStore {
        by_addr: RwLock<std::collections::HashMap<u64, Arc<str>>>,
        by_id: RwLock<std::collections::HashMap<(usize, Arc<str>), u64>>,
        next: AtomicU64,
        released: AtomicUsize,
    }

    static FAKE: std::sync::LazyLock<Arc<FakeStore>> = std::sync::LazyLock::new(|| {
        Arc::new(FakeStore {
            by_addr: RwLock::new(std::collections::HashMap::new()),
            by_id: RwLock::new(std::collections::HashMap::new()),

            next: AtomicU64::new(0x7f00_0000_0000),
            released: AtomicUsize::new(0),
        })
    });

    impl FakeStore {
        fn link(&self, idx: &AnnIndex, id: &str) -> u64 {
            let addr = self.next.fetch_add(64, Ordering::Relaxed);
            let id: Arc<str> = Arc::from(id);
            self.by_addr.write().unwrap().insert(addr, Arc::clone(&id));
            self.by_id.write().unwrap().insert((map_of(idx), id), addr);
            addr
        }

        fn addr_of(&self, idx: &AnnIndex, id: &str) -> Option<u64> {
            self.by_id
                .read()
                .unwrap()
                .get(&(map_of(idx), Arc::from(id)))
                .copied()
        }

        fn unlink(&self, idx: &AnnIndex, id: &str) -> Option<u64> {
            self.by_id
                .write()
                .unwrap()
                .remove(&(map_of(idx), Arc::from(id)))
        }

        fn ids_of(&self, idx: &AnnIndex) -> std::collections::HashSet<String> {
            let map = map_of(idx);
            self.by_id
                .read()
                .unwrap()
                .keys()
                .filter(|(m, _)| *m == map)
                .map(|(_, id)| id.to_string())
                .collect()
        }
    }

    fn map_of(idx: &AnnIndex) -> usize {
        std::ptr::from_ref(idx) as usize
    }

    impl Elements for FakeStore {
        fn id_at(&self, addr: u64) -> Option<Arc<str>> {
            self.by_addr.read().unwrap().get(&addr).cloned()
        }

        fn release(&self, addrs: &[u64]) {
            let mut by_addr = self.by_addr.write().unwrap();
            for addr in addrs {
                by_addr.remove(addr);
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn publish(idx: &AnnIndex, id: &str, staged: Staged<'_>, owner: u64) -> u64 {
        let linked = std::sync::Mutex::new(0u64);
        let done: std::result::Result<(), PublishError<()>> = idx.insert_published(
            staged,
            owner,
            || FAKE.addr_of(idx, id),
            || {
                let addr = FAKE.link(idx, id);
                *linked.lock().unwrap() = addr;
                Ok(addr)
            },
        );
        assert!(done.is_ok(), "publish failed");
        *linked.lock().unwrap()
    }

    fn remove(idx: &AnnIndex, id: &str) -> bool {
        let addr = FAKE.unlink(idx, id);
        let removed: std::result::Result<Option<bool>, PublishError<()>> =
            idx.remove_published(|| Ok(addr));
        let Ok(removed) = removed else {
            panic!("remove failed");
        };
        removed.unwrap_or(false)
    }

    #[test]
    fn a_stored_vector_reads_back_byte_for_byte() {
        for (quant, metric, dim) in [
            (Quant::F32, Metric::L2, 4usize),
            (Quant::F16, Metric::L2, 4),
            (Quant::I8, Metric::Cos, 4),
            (Quant::B1, Metric::Hamming, 16),
        ] {
            let idx = build(dim, quant, metric, 2);
            let layout = Layout::new(dim, quant);

            let stored: Vec<u8> = (0..layout.vector_bytes()).map(|i| (i as u8) | 1).collect();

            let staged = idx.stage(&stored, OWNER).expect("stage");
            let addr = publish(&idx, "v1", staged, OWNER);

            assert_eq!(
                idx.vector_of(addr).expect("read back"),
                Some(stored.clone()),
                "{quant:?} did not round-trip through the graph"
            );
        }
    }

    #[test]
    fn an_address_the_graph_does_not_hold_has_no_vector() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        assert_eq!(idx.vector_of(0x7fff_0000_0000).expect("read back"), None);
    }

    fn build(dim: usize, quant: Quant, metric: Metric, threads: usize) -> AnnIndex {
        AnnIndex::with_threads(
            Layout::new(dim, quant),
            metric,
            0,
            0,
            0,
            threads,
            Arc::clone(&FAKE) as Arc<dyn Elements>,
        )
        .unwrap()
    }

    fn exhaust_contexts(idx: &AnnIndex) {
        idx.inner.write().unwrap().reset().unwrap();
    }

    fn assert_names_resolve_to_nodes(idx: &AnnIndex, note: &str) {
        let index = idx.inner.read().unwrap();
        for addr in idx.held().live_addrs() {
            assert!(
                index.contains(addr),
                "{note}: the set holds {addr:#x}, which the graph does not have"
            );
            assert!(
                FAKE.id_at(addr).is_some(),
                "{note}: {addr:#x} is held but its element is gone"
            );
        }
    }

    fn assert_one_key_per_id(idx: &AnnIndex, note: &str) {
        let live = idx.held().live_addrs();
        let mut seen: std::collections::HashMap<Arc<str>, u64> = std::collections::HashMap::new();
        for addr in &live {
            let Some(id) = FAKE.id_at(*addr) else {
                continue;
            };
            if let Some(other) = seen.insert(id.clone(), *addr) {
                panic!("{note}: {id} is named by both {other:#x} and {addr:#x}");
            }
        }
        assert_eq!(seen.len(), live.len());
    }

    const OWNER: u64 = 7;

    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        put(idx, id, coords)
    }

    fn put(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        let staged = idx
            .stage(
                &crate::handler::quant::encode(coords, idx.layout.quant),
                OWNER,
            )
            .unwrap();
        publish(idx, id, staged, OWNER)
    }

    fn search(idx: &AnnIndex, coords: &[f32], k: usize) -> Vec<String> {
        let q = crate::handler::quant::encode(coords, idx.layout.quant);
        idx.search(&q, k, None)
            .unwrap()
            .into_iter()
            .map(|(_, id, _)| id.to_string())
            .collect()
    }

    #[test]
    fn add_and_search_returns_the_nearest_neighbour() {
        let idx = build(4, Quant::F32, Metric::L2, 4);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);
        add(&idx, "c", &[0.0, 0.0, 1.0, 0.0]);
        assert_eq!(idx.len(), 3);

        let hits = search(&idx, &[0.9, 0.1, 0.0, 0.0], 1);
        assert_eq!(hits, vec!["a"]);
        assert_eq!(search(&idx, &[0.0, 0.0, 0.9, 0.0], 1), vec!["c"]);
    }

    #[test]
    fn every_quantization_round_trips_through_usearch() {
        for (q, m) in [
            (Quant::F32, Metric::Cos),
            (Quant::F16, Metric::Cos),
            (Quant::I8, Metric::Cos),
            (Quant::B1, Metric::Hamming),
        ] {
            let idx = build(8, q, m, 2);
            add(&idx, "x", &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0]);
            add(&idx, "y", &[-1.0, -1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0]);

            let hits = search(&idx, &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0], 1);
            assert_eq!(hits, vec!["x"], "quant {q:?}");
        }
    }

    #[test]
    fn usearch_removes_a_node_while_our_callback_is_still_inside_it() {
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 4));
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "b", &[0.9, 0.1, 0.0, 0.0]);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<u64>();
        let (removed_tx, removed_rx) = std::sync::mpsc::channel::<usize>();

        let other = Arc::clone(&idx);
        let remover = std::thread::spawn(move || {
            let key = entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the callback never said which node it was inside");
            let index = other.inner.read().unwrap_or_else(PoisonError::into_inner);
            let removed = index.remove(key).expect("remove");
            removed_tx.send(removed).unwrap();
        });

        let query = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);
        let reported = std::cell::Cell::new(false);
        let witnessed = std::cell::Cell::new(0usize);
        let probe = |key: u64| {
            if !reported.replace(true) {
                entered_tx.send(key).unwrap();
                witnessed.set(
                    removed_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("the node was not removed while the callback was inside it"),
                );
            }
            true
        };
        let _ = idx.search(&query, 2, Some(&probe as Accept)).unwrap();
        remover.join().unwrap();

        assert_eq!(
            witnessed.get(),
            1,
            "usearch took the node out with the callback for that same node still running, \
             which is why the held set is read-locked across the dereference"
        );
    }

    #[test]
    fn the_predicate_excludes_what_it_rejects() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "keep", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "skip", &[1.0, 0.0, 0.0, 0.0]);

        let q = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);

        let keep = |key: u64| FAKE.id_at(key).is_some_and(|id| &*id == "keep");
        let hits = idx.search(&q, 10, Some(&keep as Accept)).unwrap();
        let ids: Vec<String> = hits.iter().map(|(_, id, _)| id.to_string()).collect();
        assert_eq!(ids, vec!["keep"]);
    }

    #[test]
    fn readding_an_id_moves_it_to_a_fresh_key() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let first = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        let second = put(&idx, "a", &[0.0, 1.0, 0.0, 0.0]);
        assert_ne!(second, first);
        assert!(
            !idx.held().contains(first),
            "the displaced address is not held any more"
        );
        assert_eq!(
            FAKE.id_at(first),
            None,
            "and its refcount went back, so it answers nothing"
        );
        assert_eq!(idx.len(), 1, "an update must not grow the index");
        assert_eq!(
            search(&idx, &[0.0, 1.0, 0.0, 0.0], 5),
            vec!["a"],
            "one hit, at the new coordinates"
        );
    }

    #[test]
    fn churn_does_not_grow_the_index() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        for round in 0..200 {
            let keys: Vec<u64> = (0..5)
                .map(|i| add(&idx, &format!("r{round}-{i}"), &[i as f32, 1.0, 2.0, 3.0]))
                .collect();
            for (i, _) in keys.into_iter().enumerate() {
                assert!(remove(&idx, &format!("r{round}-{i}")));
            }
            assert_eq!(idx.len(), 0, "round {round} leaked live entries");
        }

        assert_eq!(idx.len(), 0);
        assert_eq!(
            idx.reserved.load(Ordering::Acquire),
            MIN_CAPACITY,
            "the reservation grew with keys rather than with members"
        );
    }

    #[test]
    fn removed_ids_disappear_from_results() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let a = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);

        assert!(remove(&idx, "a"));
        assert!(
            !remove(&idx, "a"),
            "a second remove of the same key names nothing"
        );
        assert_eq!(idx.len(), 1);
        assert!(!idx.held().contains(a));

        let hits = search(&idx, &[1.0, 0.0, 0.0, 0.0], 10);
        assert_eq!(hits, vec!["b"]);
    }

    #[test]
    fn a_failed_stage_leaves_no_mapping_behind() {
        let idx = build(2, Quant::F32, Metric::Cos, 2);
        add(&idx, "a", &[1.0, 0.0]);
        exhaust_contexts(&idx);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        assert!(idx.stage(&v, OWNER).is_err());

        assert_eq!(idx.len(), 1, "the failed write must not count");
    }

    #[test]
    fn a_failed_refill_add_leaves_no_mapping_behind() {
        let idx = build(2, Quant::F32, Metric::Cos, 2);
        exhaust_contexts(&idx);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let addr = FAKE.link(&idx, "b");
        assert!(idx.add_unless_known(addr, || Ok(Some(v.to_vec()))).is_err());
        assert!(!idx.held().contains(addr));
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn a_staged_node_is_invisible_until_it_is_published() {
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();

        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[0.0, 1.0], 5), vec!["a"], "no unnamed hits");

        publish(&idx, "b", staged, OWNER);
        assert_eq!(idx.len(), 2);
        assert_eq!(search(&idx, &[0.0, 1.0], 1), vec!["b"]);
    }

    #[test]
    fn a_staged_node_does_not_cost_a_result_slot() {
        let idx = build(2, Quant::F32, Metric::L2, 4);
        for i in 0..5 {
            add(&idx, &format!("v{i}"), &[1.0 + i as f32, 0.0]);
        }
        let nearest = crate::handler::quant::encode(&[0.0, 0.0], Quant::F32);
        let staged = idx.stage(&nearest, OWNER).unwrap();

        let named = search(&idx, &[0.0, 0.0], 3);
        assert_eq!(
            named,
            vec!["v0", "v1", "v2"],
            "three named hits, nearest first"
        );

        idx.discard(staged);
        assert_eq!(search(&idx, &[0.0, 0.0], 3), vec!["v0", "v1", "v2"]);
    }

    #[test]
    fn the_in_flight_count_falls_back_to_zero_however_a_stage_ends() {
        let idx = build(2, Quant::F32, Metric::L2, 4);
        let v = crate::handler::quant::encode(&[1.0, 0.0], Quant::F32);
        assert_eq!(idx.in_flight.load(Ordering::Relaxed), 0);

        let staged = idx.stage(&v, OWNER).unwrap();
        assert_eq!(idx.in_flight.load(Ordering::Relaxed), 1);
        publish(&idx, "a", staged, OWNER);
        assert_eq!(idx.in_flight.load(Ordering::Relaxed), 0, "published");

        let staged = idx.stage(&v, OWNER).unwrap();
        idx.discard(staged);
        assert_eq!(idx.in_flight.load(Ordering::Relaxed), 0, "discarded");

        drop(idx.stage(&v, OWNER).unwrap());
        assert_eq!(
            idx.in_flight.load(Ordering::Relaxed),
            0,
            "dropped unsettled"
        );

        exhaust_contexts(&idx);
        assert!(idx.stage(&v, OWNER).is_err());
        assert_eq!(idx.in_flight.load(Ordering::Relaxed), 0, "failed insert");
    }

    #[test]
    fn a_discarded_overwrite_leaves_the_mapping_alone() {
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        let key = add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        idx.discard(idx.stage(&v, OWNER).unwrap());

        assert!(idx.held().contains(key), "the graph never moved");
        assert_eq!(FAKE.id_at(key).as_deref(), Some("a"));
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[1.0, 0.0], 5), vec!["a"]);
    }

    #[test]
    fn a_takeover_inside_the_staging_window_voids_the_stage() {
        const TAKEOVER: u64 = 0;
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();
        let staged_key = staged.key();
        idx.begin_rebuild().unwrap();

        publish(&idx, "b", staged, TAKEOVER);
        assert_eq!(idx.len(), 0, "the stage did not survive the takeover");

        let abandoned = staged_key;
        let again = idx.stage(&v, TAKEOVER).unwrap();
        publish(&idx, "b", again, TAKEOVER);
        assert!(
            !idx.held().contains(abandoned),
            "the node staged against the wiped graph is held by nothing"
        );
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[0.0, 1.0], 5), vec!["b"]);
    }

    #[test]
    fn a_delete_inside_the_staging_window_is_not_lost() {
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();

        assert!(remove(&idx, "a"));
        assert_eq!(idx.len(), 0);

        publish(&idx, "a", staged, OWNER);
        assert_eq!(idx.len(), 1);
        assert_eq!(
            search(&idx, &[0.0, 1.0], 5),
            vec!["a"],
            "the linked element must be searchable"
        );
    }

    #[test]
    fn a_discard_after_a_delete_in_the_window_drops_only_its_own_node() {
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();
        assert!(remove(&idx, "a"));

        idx.discard(staged);
        assert_eq!(idx.len(), 0);
        assert!(search(&idx, &[0.0, 1.0], 5).is_empty());
    }

    #[test]
    fn a_cleared_index_is_still_searchable() {
        let idx = build(2, Quant::F32, Metric::Cos, 2);
        add(&idx, "a", &[1.0, 0.0]);

        idx.clear_with().unwrap();

        idx.reserve(0).unwrap();

        assert!(search(&idx, &[1.0, 0.0], 3).is_empty());
        add(&idx, "b", &[0.0, 1.0]);
        assert_eq!(search(&idx, &[0.0, 1.0], 3), vec!["b"]);
    }

    #[test]
    fn growth_past_the_initial_reservation_succeeds() {
        let idx = build(4, Quant::F32, Metric::L2, 4);
        for i in 0..1100 {
            add(&idx, &format!("v{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }
        assert_eq!(idx.len(), 1100);
        assert_eq!(search(&idx, &[1099.0, 0.0, 0.0, 0.0], 1), vec!["v1099"]);
    }

    #[test]
    fn concurrent_searchers_within_the_slot_count_succeed() {
        let idx = Arc::new(build(8, Quant::F32, Metric::Cos, 16));
        for i in 0..200 {
            add(
                &idx,
                &format!("v{i}"),
                &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
            );
        }

        let failures = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for t in 0..16 {
            let idx = Arc::clone(&idx);
            let failures = Arc::clone(&failures);
            handles.push(std::thread::spawn(move || {
                let q = crate::handler::quant::encode(
                    &[t as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
                    Quant::F32,
                );
                for _ in 0..50 {
                    if idx.search(&q, 5, None).is_err() {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(failures.load(Ordering::SeqCst), 0);
    }

    fn choice(t: usize, i: usize, m: usize) -> usize {
        (t.wrapping_mul(0x9E37)
            .wrapping_add(i)
            .wrapping_mul(2654435761))
            % m
    }

    #[test]
    fn concurrent_writers_deleters_and_searchers_keep_the_mapping_consistent() {
        const IDS: usize = 24;
        let dim = 8;
        let idx = Arc::new(build(dim, Quant::F32, Metric::L2, 16));
        for i in 0..IDS {
            let id = format!("id{i}");

            add(&idx, &id, &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        }

        let stop = Arc::new(AtomicBool::new(false));

        let discarded: Arc<std::sync::Mutex<std::collections::HashSet<u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let leaked = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for t in 0..4 {
            let idx = Arc::clone(&idx);
            let discarded = Arc::clone(&discarded);
            handles.push(std::thread::spawn(move || {
                for i in 0..250 {
                    let id = format!("id{}", choice(t, i, IDS));
                    let coords: Vec<f32> = (0..dim).map(|d| (t * 31 + i + d) as f32).collect();
                    let v = crate::handler::quant::encode(&coords, Quant::F32);
                    let staged = idx.stage(&v, OWNER).unwrap();
                    let key = staged.key();
                    if choice(t, i + 7, 4) == 0 {
                        discarded.lock().unwrap().insert(key);
                        idx.discard(staged);
                    } else {
                        let done: std::result::Result<(), PublishError<()>> = idx.insert_published(
                            staged,
                            OWNER,
                            || FAKE.addr_of(&idx, &id),
                            || Ok(FAKE.link(&idx, &id)),
                        );
                        done.unwrap();
                    }
                }
            }));
        }

        for t in 4..6 {
            let idx = Arc::clone(&idx);
            handles.push(std::thread::spawn(move || {
                for i in 0..250 {
                    let id = format!("id{}", choice(t, i, IDS));

                    let removed: std::result::Result<Option<bool>, PublishError<()>> =
                        idx.remove_published(|| Ok(FAKE.unlink(&idx, &id)));
                    removed.unwrap();
                }
            }));
        }

        for t in 6..8 {
            let idx = Arc::clone(&idx);
            let stop = Arc::clone(&stop);
            let discarded = Arc::clone(&discarded);
            let leaked = Arc::clone(&leaked);
            handles.push(std::thread::spawn(move || {
                let mut i = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let coords: Vec<f32> = (0..dim).map(|d| (t + i + d) as f32).collect();
                    let q = crate::handler::quant::encode(&coords, Quant::F32);
                    for (key, _, _) in idx.search(&q, 5, None).unwrap() {
                        if discarded.lock().unwrap().contains(&key) {
                            leaked.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    i += 1;
                }
            }));
        }

        for (n, h) in handles.into_iter().enumerate() {
            h.join().unwrap();
            if n == 5 {
                stop.store(true, Ordering::Relaxed);
            }
        }

        assert_one_key_per_id(&idx, "after concurrent writers, deleters and searchers");
        assert_names_resolve_to_nodes(&idx, "after concurrent writers, deleters and searchers");
        assert_eq!(
            leaked.load(Ordering::Relaxed),
            0,
            "a discarded key resolved to an id, so the caller would have rendered it"
        );

        for key in discarded.lock().unwrap().iter() {
            assert!(!idx.held().contains(*key), "discarded key {key} is held");
        }

        let live = FAKE.ids_of(&idx);
        let named: std::collections::HashSet<String> = idx
            .held()
            .live_addrs()
            .into_iter()
            .filter_map(|a| FAKE.id_at(a))
            .map(|id| id.to_string())
            .collect();
        assert_eq!(
            named, live,
            "the graph and the Map disagree on which ids exist"
        );
    }

    #[test]
    fn a_rebuild_racing_writers_leaves_the_mapping_consistent() {
        const IDS: usize = 16;
        let dim = 8;
        let idx = Arc::new(build(dim, Quant::F32, Metric::L2, 16));
        for i in 0..IDS {
            let id = format!("id{i}");

            add(&idx, &id, &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        }

        let mut handles = Vec::new();
        for t in 0..4 {
            let idx = Arc::clone(&idx);
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    let id = format!("id{}", choice(t, i, IDS));
                    let coords: Vec<f32> = (0..dim).map(|d| (t + i + d) as f32).collect();
                    let v = crate::handler::quant::encode(&coords, Quant::F32);

                    if let Ok(staged) = idx.stage(&v, OWNER) {
                        let done: std::result::Result<(), PublishError<()>> = idx.insert_published(
                            staged,
                            OWNER,
                            || FAKE.addr_of(&idx, &id),
                            || Ok(FAKE.link(&idx, &id)),
                        );
                        done.unwrap();
                    }
                }
            }));
        }
        {
            let idx = Arc::clone(&idx);
            handles.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    idx.begin_rebuild().unwrap();
                    std::thread::yield_now();
                    idx.end_rebuild();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_one_key_per_id(&idx, "after a rebuild raced writers");

        let _ = add(&idx, "after", &[9.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(
            search(&idx, &[9.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], 1),
            vec!["after"]
        );
        assert_one_key_per_id(&idx, "after writing to a raced index");
    }

    #[test]
    fn concurrent_adds_and_searches_keep_every_address_named() {
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 16));

        let keys: Arc<std::sync::Mutex<Vec<(String, u64)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for t in 0..8 {
            let idx = Arc::clone(&idx);
            let keys = Arc::clone(&keys);
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    let id = format!("t{t}-{i}");
                    let staged = idx
                        .stage(
                            &crate::handler::quant::encode(
                                &[t as f32, i as f32, 0.0, 0.0],
                                Quant::F32,
                            ),
                            OWNER,
                        )
                        .unwrap();

                    let addr = publish(&idx, &id, staged, OWNER);
                    keys.lock().unwrap().push((id, addr));
                    let q =
                        crate::handler::quant::encode(&[t as f32, i as f32, 0.0, 0.0], Quant::F32);
                    let _ = idx.search(&q, 3, None).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(idx.len(), 800);
        assert_one_key_per_id(&idx, "after concurrent adds and searches");
        for (id, key) in keys.lock().unwrap().iter() {
            assert_eq!(FAKE.id_at(*key).as_deref(), Some(id.as_str()));
            assert!(idx.held().contains(*key));
        }
    }

    #[test]
    fn held_memory_is_chunked_while_used_memory_tracks_the_data() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let empty = idx.held_bytes();
        add(&idx, "v0", &[0.0, 0.0, 0.0, 0.0]);
        let one = idx.held_bytes();
        let one_used = idx.used_bytes();
        assert!(one > empty * 2, "the first insert takes a chunk");

        for i in 1..500 {
            add(&idx, &format!("v{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }
        assert_eq!(
            idx.held_bytes(),
            one,
            "held memory is chunked, not per-vector"
        );
        assert!(
            idx.used_bytes() > one_used * 100,
            "used memory tracks the vector count"
        );
        assert!(
            idx.used_bytes() < idx.held_bytes(),
            "used memory cannot exceed what is held"
        );
    }

    #[test]
    fn wrong_vector_length_is_rejected() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        assert!(idx.stage(&[0u8; 8], OWNER).is_err());
        assert!(idx.search(&[0u8; 8], 1, None).is_err());
    }

    #[test]
    #[ignore]
    fn stage_costs_far_more_than_publish() {
        let idx = build(768, Quant::F32, Metric::Cos, 8);
        let owner = 0;

        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let raw: Vec<f32> = (0..768)
                .map(|i| ((seed >> (i % 40)) & 0xFFFF) as f32 / 65535.0)
                .collect();
            crate::handler::quant::encode(&raw, Quant::F32)
        };

        for _ in 0..2000 {
            let staged = idx.stage(&next(), owner).unwrap();
            publish(&idx, "warm", staged, owner);
        }

        const N: usize = 2000;
        let mut stage_ns = 0u128;
        let mut publish_ns = 0u128;
        for i in 0..N {
            let id = format!("v{i}");

            let vector = next();
            let t = std::time::Instant::now();
            let staged = idx.stage(&vector, owner).unwrap();
            stage_ns += t.elapsed().as_nanos();

            let t = std::time::Instant::now();
            publish(&idx, &id, staged, owner);
            publish_ns += t.elapsed().as_nanos();
        }
        println!(
            "stage {:.1}us  publish {:.2}us  ratio {:.0}x",
            stage_ns as f64 / N as f64 / 1000.0,
            publish_ns as f64 / N as f64 / 1000.0,
            stage_ns as f64 / publish_ns as f64
        );
    }

    #[test]
    fn keys_captured_before_a_wipe_are_discarded() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let key = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        let entered = idx.epoch.load(Ordering::Acquire);

        idx.clear_with().unwrap();
        assert_eq!(
            FAKE.id_at(key),
            None,
            "the wipe handed the address back, so it names nothing"
        );
        add(&idx, "b", &[1.0, 0.0, 0.0, 0.0]);

        assert!(
            idx.resolve(&[(key, 0.0)], entered).is_empty(),
            "a search that captured keys before the wipe resolves none of them"
        );
    }

    #[test]
    fn an_update_moves_the_node_to_the_new_element() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let old = add(&idx, "v1", &[1.0, 0.0, 0.0, 0.0]);

        let settled: std::result::Result<Option<()>, PublishError<()>> =
            idx.update_published(|| Some((old, vec![0u8; 8])), |_| Ok(FAKE.link(&idx, "v1")));
        assert!(settled.is_ok(), "the update went through");

        let live = idx.held().live_addrs();
        assert_eq!(live.len(), 1, "one node, not two");
        assert_ne!(live[0], old, "and it stands on the new element");
        assert_eq!(
            FAKE.id_at(old),
            None,
            "the old element's refcount went back, so it answers nothing"
        );
        assert_eq!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5), vec!["v1"]);
    }

    #[test]
    fn a_refused_update_leaves_the_node_where_it_was() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let old = add(&idx, "v1", &[1.0, 0.0, 0.0, 0.0]);

        let settled: std::result::Result<Option<()>, PublishError<()>> =
            idx.update_published(|| Some((old, vec![0u8; 8])), |_| Err(()));
        assert!(matches!(settled, Err(PublishError::Store(()))));

        assert_eq!(idx.held().live_addrs(), vec![old]);
        assert_eq!(FAKE.id_at(old).as_deref(), Some("v1"));
        assert_eq!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5), vec!["v1"]);
    }

    #[test]
    fn a_staged_key_is_never_dereferenced() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let v = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();

        assert!(
            held::is_staged(staged.key()),
            "a staged node carries the tag"
        );
        assert!(
            idx.resolve(&[(staged.key(), 0.0)], idx.epoch.load(Ordering::Acquire))
                .is_empty(),
            "and resolves to nothing, without the store being asked"
        );
        idx.discard(staged);
    }
}
