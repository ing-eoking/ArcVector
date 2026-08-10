//! usearch index wrapper: concurrency gating, capacity growth and id mapping.
//!
//! # Why the semaphore exists
//!
//! usearch's Rust bindings always call into C++ with `any_thread()`, which pops a
//! context from a **fixed-size pool**. When the pool is empty it does not block —
//! it fails with "Reserve capacity ahead of insertions!". Since arcus allows more
//! worker threads than any constant we could assume (`-t` above 64 only warns),
//! correctness cannot rest on the worker count.
//!
//! Instead we hold the invariant
//!
//! ```text
//! concurrent entries into usearch <= permits == reserved thread contexts
//! ```
//!
//! by gating every `add`/`search`/`remove` on a semaphore whose permit count equals
//! the reserved context count. Excess workers wait instead of failing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, RwLock};

use usearch::{Index, IndexOptions, MetricKind, ScalarKind, b1x8, f16};

use crate::codec::Layout;
use crate::error::Error;
use crate::quant::Quant;

type Result<T> = std::result::Result<T, Error>;

/// usearch reports failures as a `cxx::Exception`; carry its message through
/// without depending on the cxx crate directly.
fn usearch_err(e: impl std::fmt::Display) -> Error {
    Error::Index(e.to_string())
}

/// Distance metric. Restricted per quantization by [`Metric::check_quant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Cos,
    L2,
    IP,
    Hamming,
    Tanimoto,
}

impl Metric {
    pub fn parse(s: &str) -> Option<Metric> {
        match s.to_ascii_lowercase().as_str() {
            "cos" | "cosine" => Some(Metric::Cos),
            "l2" | "l2sq" | "euclidean" => Some(Metric::L2),
            "ip" | "dot" => Some(Metric::IP),
            "hamming" => Some(Metric::Hamming),
            "tanimoto" | "jaccard" => Some(Metric::Tanimoto),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Metric::Cos => "cos",
            Metric::L2 => "l2",
            Metric::IP => "ip",
            Metric::Hamming => "hamming",
            Metric::Tanimoto => "tanimoto",
        }
    }

    const fn kind(self) -> MetricKind {
        match self {
            Metric::Cos => MetricKind::Cos,
            Metric::L2 => MetricKind::L2sq,
            Metric::IP => MetricKind::IP,
            Metric::Hamming => MetricKind::Hamming,
            Metric::Tanimoto => MetricKind::Tanimoto,
        }
    }

    /// Reject metric/quantization pairs whose distances would be meaningless.
    pub fn check_quant(self, quant: Quant) -> Result<()> {
        let bitwise = matches!(self, Metric::Hamming | Metric::Tanimoto);
        match (quant, bitwise) {
            (Quant::B1, false) => Err(Error::bad_request(format!(
                "quantization b1 requires a bitwise metric (hamming or tanimoto), got {self}"
            ))),
            (q, true) if q != Quant::B1 => Err(Error::bad_request(format!(
                "metric {self} requires quantization b1, got {q}"
            ))),
            _ => Ok(()),
        }
    }
}

impl std::fmt::Display for Metric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

const fn scalar_kind(q: Quant) -> ScalarKind {
    match q {
        Quant::F32 => ScalarKind::F32,
        Quant::F16 => ScalarKind::F16,
        Quant::I8 => ScalarKind::I8,
        Quant::B1 => ScalarKind::B1,
    }
}

// ---------------------------------------------------------------------------
// Semaphore
// ---------------------------------------------------------------------------

struct Semaphore {
    avail: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Semaphore {
            avail: Mutex::new(n.max(1)),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let mut avail = self.avail.lock().unwrap_or_else(PoisonError::into_inner);
        while *avail == 0 {
            avail = self.cv.wait(avail).unwrap_or_else(PoisonError::into_inner);
        }
        *avail -= 1;
        Permit { sem: self }
    }
}

struct Permit<'a> {
    sem: &'a Semaphore,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut avail = self
            .sem
            .avail
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *avail += 1;
        drop(avail);
        self.sem.cv.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Id mapping
// ---------------------------------------------------------------------------

struct IdSlot {
    id: Box<str>,
    alive: AtomicBool,
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// Thread contexts reserved per index, and therefore the semaphore's permit
/// count. Not a client-facing setting: it follows from how many worker threads
/// the server runs, which the client has no view of.
///
/// The value is not correctness-critical — the semaphore holds the invariant for
/// any value — so it is purely a throughput/memory tradeoff. Below the worker
/// count, excess workers queue briefly; above it, the surplus per-thread buffers
/// are wasted (usearch allocates `bytes_per_vector * threads` for casting, so a
/// 4096-dimension f32 index costs about 1 MB here). 64 is chosen because `-t`
/// only warns past that (memcached.c:16107), so it covers every reachable worker
/// count without queueing.
///
/// Deriving it from `settings.num_threads` was considered and rejected: the
/// symbol is exported, but reading it needs the `struct settings` layout, whose
/// field offsets shift with build-time `#ifdef`s.
pub const THREAD_SLOTS: usize = 64;
const MIN_CAPACITY: usize = 1024;

pub struct AnnIndex {
    pub layout: Layout,
    pub metric: Metric,
    threads: usize,
    /// Write-locked only for `reserve`; every other operation takes a read lock,
    /// because usearch guards concurrent add/search/remove internally.
    inner: RwLock<Index>,
    permits: Semaphore,
    reserved: AtomicUsize,
    /// key -> id. Append-only and lock-free: the search predicate reads it once
    /// per visited graph node, so a lock here would serialize the whole search.
    ids: boxcar::Vec<IdSlot>,
    /// id -> key. Write path only; never touched by the predicate.
    by_id: RwLock<HashMap<Box<str>, u64>>,
}

impl AnnIndex {
    pub fn new(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
        threads: usize,
    ) -> Result<AnnIndex> {
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

        Ok(AnnIndex {
            layout,
            metric,
            threads,
            inner: RwLock::new(index),
            permits: Semaphore::new(threads),
            reserved: AtomicUsize::new(MIN_CAPACITY),
            ids: boxcar::Vec::new(),
            by_id: RwLock::new(HashMap::new()),
        })
    }

    pub fn len(&self) -> usize {
        self.by_id
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lock-free key -> id lookup. Returns `None` for tombstoned slots.
    pub fn id_of(&self, key: u64) -> Option<&str> {
        let slot = self.ids.get(key as usize)?;
        if slot.alive.load(Ordering::Acquire) {
            Some(&slot.id)
        } else {
            None
        }
    }

    pub fn key_of(&self, id: &str) -> Option<u64> {
        self.by_id
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .copied()
    }

    /// Grow usearch's capacity so `needed` keys fit.
    ///
    /// Taking the write lock is what makes this safe: it drains all readers, so
    /// no thread is inside usearch while the node arrays are reallocated. Permits
    /// need not be reclaimed separately.
    fn ensure_capacity(&self, needed: usize) -> Result<()> {
        if needed <= self.reserved.load(Ordering::Acquire) {
            return Ok(());
        }
        let index = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let current = self.reserved.load(Ordering::Acquire);
        if needed <= current {
            return Ok(()); // another writer grew it while we waited
        }
        let target = (current * 2).max(needed).max(MIN_CAPACITY);
        index
            .reserve_capacity_and_threads(target, self.threads)
            .map_err(usearch_err)?;
        self.reserved.store(target, Ordering::Release);
        Ok(())
    }

    /// Insert or replace `id`. `vector` is the already-quantized byte form.
    pub fn add(&self, id: &str, vector: &[u8]) -> Result<u64> {
        if vector.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "vector is {} bytes, expected {}",
                vector.len(),
                self.layout.vector_bytes()
            )));
        }

        // Allocate (or reuse) the key while holding by_id, so two concurrent
        // vadds of the same id cannot end up with two different keys.
        let (key, existed) = {
            let mut by_id = self.by_id.write().unwrap_or_else(PoisonError::into_inner);
            match by_id.get(id) {
                Some(k) => {
                    let k = *k;
                    if let Some(slot) = self.ids.get(k as usize) {
                        slot.alive.store(true, Ordering::Release);
                    }
                    (k, true)
                }
                None => {
                    let k = self.ids.push(IdSlot {
                        id: id.into(),
                        alive: AtomicBool::new(true),
                    }) as u64;
                    by_id.insert(id.into(), k);
                    (k, false)
                }
            }
        };

        self.ensure_capacity(key as usize + 1)?;

        let _permit = self.permits.acquire();
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        if existed {
            // The index is built with `multi: false`, so usearch rejects a second
            // add under the same key. An update is therefore remove-then-add.
            // A concurrent search can miss this vector inside that window; Map
            // remains the source of truth, so nothing is lost.
            index.remove(key).map_err(usearch_err)?;
        }
        self.typed_add(&index, key, vector)?;
        Ok(key)
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

    /// Remove `id`. Returns false when it was not present.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let key = {
            let mut by_id = self.by_id.write().unwrap_or_else(PoisonError::into_inner);
            match by_id.remove(id) {
                Some(k) => k,
                None => return Ok(false),
            }
        };
        if let Some(slot) = self.ids.get(key as usize) {
            slot.alive.store(false, Ordering::Release);
        }

        let _permit = self.permits.acquire();
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        index.remove(key).map_err(usearch_err)?;
        Ok(true)
    }

    /// k-NN search. `accept` is called once per visited graph node and must be
    /// cheap — it runs inside usearch's traversal.
    pub fn search<F>(&self, query: &[u8], k: usize, accept: F) -> Result<Vec<(u64, f32)>>
    where
        F: Fn(u64) -> bool,
    {
        if query.len() != self.layout.vector_bytes() {
            return Err(Error::bad_request(format!(
                "query is {} bytes, expected {}",
                query.len(),
                self.layout.vector_bytes()
            )));
        }

        // Tombstoned keys must never reach the caller, even if usearch still
        // holds the node.
        let alive_and_accepted = |key: u64| self.id_of(key).is_some() && accept(key);

        let _permit = self.permits.acquire();
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let matches = match self.layout.quant {
            Quant::F32 => index.filtered_search(&to_f32(query), k, alive_and_accepted),
            Quant::F16 => {
                index.filtered_search(f16::from_i16s(&to_i16(query)), k, alive_and_accepted)
            }
            Quant::I8 => index.filtered_search(&to_i8(query), k, alive_and_accepted),
            Quant::B1 => index.filtered_search(b1x8::from_u8s(query), k, alive_and_accepted),
        }
        .map_err(usearch_err)?;

        Ok(matches.keys.into_iter().zip(matches.distances).collect())
    }
}

// The stored bytes come straight out of an arcus item and carry no alignment
// guarantee, so each conversion copies rather than reinterpreting in place.
// This is once per operation, not once per visited node.

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
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn metric_quant_compatibility() {
        assert!(Metric::Cos.check_quant(Quant::I8).is_ok());
        assert!(Metric::L2.check_quant(Quant::F32).is_ok());
        assert!(Metric::Hamming.check_quant(Quant::B1).is_ok());
        assert!(Metric::Tanimoto.check_quant(Quant::B1).is_ok());

        // b1 vectors carry no magnitude, so cosine/L2/IP are meaningless.
        assert!(Metric::Cos.check_quant(Quant::B1).is_err());
        // ...and bitwise metrics are meaningless on non-bit vectors.
        assert!(Metric::Hamming.check_quant(Quant::F32).is_err());
    }

    #[test]
    fn metric_names_roundtrip() {
        for m in [
            Metric::Cos,
            Metric::L2,
            Metric::IP,
            Metric::Hamming,
            Metric::Tanimoto,
        ] {
            assert_eq!(Metric::parse(m.as_str()), Some(m));
        }
        assert_eq!(Metric::parse("cosine"), Some(Metric::Cos));
        assert_eq!(Metric::parse("nonsense"), None);
    }

    #[test]
    fn semaphore_bounds_concurrent_holders() {
        let sem = Arc::new(Semaphore::new(3));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let sem = Arc::clone(&sem);
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let _p = sem.acquire();
                    let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::yield_now();
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // This is the invariant that keeps usearch's context pool from draining.
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "peak {}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    fn build(dim: usize, quant: Quant, metric: Metric, threads: usize) -> AnnIndex {
        AnnIndex::new(Layout::new(dim, quant), metric, 0, 0, 0, threads).unwrap()
    }

    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) {
        idx.add(id, &crate::quant::encode(coords, idx.layout.quant))
            .unwrap();
    }

    fn search(idx: &AnnIndex, coords: &[f32], k: usize) -> Vec<String> {
        let q = crate::quant::encode(coords, idx.layout.quant);
        idx.search(&q, k, |_| true)
            .unwrap()
            .into_iter()
            .filter_map(|(key, _)| idx.id_of(key).map(str::to_owned))
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
        // Guards the ScalarKind mapping and the typed add/search dispatch.
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
    fn predicate_excludes_rejected_keys() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "keep", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "skip", &[1.0, 0.0, 0.0, 0.0]);

        let q = crate::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);
        let hits = idx
            .search(&q, 10, |key| idx.id_of(key) == Some("keep"))
            .unwrap();
        let ids: Vec<&str> = hits.iter().filter_map(|(k, _)| idx.id_of(*k)).collect();
        assert_eq!(ids, vec!["keep"]);
    }

    #[test]
    fn readding_an_id_reuses_its_key() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        let first = idx.key_of("a").unwrap();
        add(&idx, "a", &[0.0, 1.0, 0.0, 0.0]);
        assert_eq!(idx.key_of("a"), Some(first));
        assert_eq!(idx.len(), 1, "an update must not grow the index");
    }

    #[test]
    fn removed_ids_disappear_from_results() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);

        assert!(idx.remove("a").unwrap());
        assert!(!idx.remove("a").unwrap(), "second remove reports absence");
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.key_of("a"), None);

        let hits = search(&idx, &[1.0, 0.0, 0.0, 0.0], 10);
        assert_eq!(hits, vec!["b"]);
    }

    #[test]
    fn growth_past_the_initial_reservation_succeeds() {
        // MIN_CAPACITY is 1024; crossing it exercises ensure_capacity's
        // read-lock -> write-lock -> reserve path.
        let idx = build(4, Quant::F32, Metric::L2, 4);
        for i in 0..1100 {
            add(&idx, &format!("v{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }
        assert_eq!(idx.len(), 1100);
        assert_eq!(search(&idx, &[1099.0, 0.0, 0.0, 0.0], 1), vec!["v1099"]);
    }

    #[test]
    fn more_concurrent_searchers_than_threads_still_succeed() {
        // The trap this guards: usearch pops thread contexts from a fixed pool
        // and FAILS rather than blocking when it is empty. With 2 reserved
        // contexts and 16 concurrent searchers, an ungated implementation
        // returns "Reserve capacity ahead of insertions!" instead of results.
        let idx = Arc::new(build(8, Quant::F32, Metric::Cos, 2));
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
                let q = crate::quant::encode(
                    &[t as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
                    Quant::F32,
                );
                for _ in 0..50 {
                    if idx.search(&q, 5, |_| true).is_err() {
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

    #[test]
    fn concurrent_adds_and_searches_do_not_corrupt_the_id_map() {
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 4));
        let mut handles = Vec::new();
        for t in 0..8 {
            let idx = Arc::clone(&idx);
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    let id = format!("t{t}-{i}");
                    idx.add(
                        &id,
                        &crate::quant::encode(&[t as f32, i as f32, 0.0, 0.0], Quant::F32),
                    )
                    .unwrap();
                    let q = crate::quant::encode(&[t as f32, i as f32, 0.0, 0.0], Quant::F32);
                    let _ = idx.search(&q, 3, |_| true).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(idx.len(), 800);
        // Every id must map back to itself through key -> id.
        for t in 0..8 {
            for i in 0..100 {
                let id = format!("t{t}-{i}");
                let key = idx.key_of(&id).expect("id missing");
                assert_eq!(idx.id_of(key), Some(id.as_str()));
            }
        }
    }

    #[test]
    fn wrong_vector_length_is_rejected() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        assert!(idx.add("a", &[0u8; 8]).is_err());
        assert!(idx.search(&[0u8; 8], 1, |_| true).is_err());
    }

    #[test]
    fn semaphore_permits_are_returned_on_panic_unwind() {
        let sem = Arc::new(Semaphore::new(1));
        let s2 = Arc::clone(&sem);
        let _ = std::thread::spawn(move || {
            let _p = s2.acquire();
            panic!("boom");
        })
        .join();
        // If Drop had not run, this would deadlock.
        let _p = sem.acquire();
    }
}
