//! The usearch index itself: capacity, the key mapping, and add/search/remove.
//!
//! usearch keys are `u64` while ours are strings, so this owns both directions of
//! that mapping. The `key -> id` side is lock-free on purpose: the search
//! predicate reads it once per visited graph node.
//!
//! Concurrency has two halves. usearch guards concurrent construction, search and
//! updates internally, so those take a **read** lock; the `RwLock` exists only to
//! make `reserve` exclusive, since that reallocates the node arrays. The other
//! half — not exhausting usearch's thread-context pool — lives in
//! [`super::threads`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

// `::` because this module shares a name with the crate it wraps.
use ::usearch::{Index, IndexOptions, ScalarKind, b1x8, f16};

use super::metric::Metric;
use super::threads::Semaphore;
use crate::arcus::element::Layout;
use crate::arcus::element::Quant;
use crate::error::Error;

type Result<T> = std::result::Result<T, Error>;

/// usearch reports failures as a `cxx::Exception`; carry its message through
/// without depending on the cxx crate directly.
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

// ---------------------------------------------------------------------------
// Id mapping
// ---------------------------------------------------------------------------

/// The `String` ↔ `u64` mapping usearch needs, both directions under one lock so
/// they cannot disagree with each other.
///
/// Keys are handed out by a counter and never reused. They may therefore be
/// sparse, which usearch does not mind: `reserve` sizes for a member *count*, not
/// a key range — verified by adding `u64::MAX - 1` to an index reserved for eight.
/// That is what lets a delete actually free its entry instead of leaving a
/// tombstone behind, and it removes the reuse hazard a free list would introduce,
/// where a key could change meaning under a search already in flight.
#[derive(Default)]
struct IdMap {
    by_key: HashMap<u64, Arc<str>>,
    by_id: HashMap<Arc<str>, u64>,
    next: u64,
}

impl IdMap {
    /// The key for `id`, minting one if it is new. Returns whether it existed.
    fn intern(&mut self, id: &str) -> (u64, bool) {
        if let Some(key) = self.by_id.get(id) {
            return (*key, true);
        }
        let key = self.next;
        self.next += 1;
        let id: Arc<str> = Arc::from(id);
        self.by_key.insert(key, Arc::clone(&id));
        self.by_id.insert(id, key);
        (key, false)
    }

    fn forget(&mut self, id: &str) -> Option<u64> {
        let key = self.by_id.remove(id)?;
        self.by_key.remove(&key);
        Some(key)
    }
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

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
    /// Both directions of the key mapping.
    ///
    /// The search predicate takes a read lock here once per visited graph node.
    /// That is affordable because the predicate already pays for an engine call on
    /// the same path — a global mutex and a malloc — against which two atomic
    /// operations do not register.
    ids: RwLock<IdMap>,
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
            ids: RwLock::new(IdMap::default()),
        })
    }

    /// How many vectors are live. Not the number ever added.
    pub fn len(&self) -> usize {
        self.ids().by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lock-free key -> id lookup. Returns `None` for tombstoned slots.
    /// The id a usearch key stands for, or `None` if it has been removed.
    ///
    /// Returns an `Arc` rather than a borrow so the lock is released before the
    /// caller uses it — the predicate goes on to make an engine call, and holding
    /// this lock across that would serialize every search behind every insert.
    pub fn id_of(&self, key: u64) -> Option<Arc<str>> {
        self.ids().by_key.get(&key).map(Arc::clone)
    }

    pub fn key_of(&self, id: &str) -> Option<u64> {
        self.ids().by_id.get(id).copied()
    }

    fn ids(&self) -> std::sync::RwLockReadGuard<'_, IdMap> {
        self.ids.read().unwrap_or_else(PoisonError::into_inner)
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
        // vadds of the same id cannot end up with two different keys: one lock
        // covers both directions of the mapping.
        let (key, existed, live) = {
            let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);
            let (key, existed) = ids.intern(id);
            (key, existed, ids.by_key.len())
        };

        // Capacity tracks live members, not keys ever handed out, so churn does not
        // grow the reservation.
        self.ensure_capacity(live)?;

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
            let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);
            match ids.forget(id) {
                Some(key) => key,
                None => return Ok(false),
            }
        };

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
    use crate::arcus::element::Quant;
    use crate::usearch::metric::Metric;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn build(dim: usize, quant: Quant, metric: Metric, threads: usize) -> AnnIndex {
        AnnIndex::new(Layout::new(dim, quant), metric, 0, 0, 0, threads).unwrap()
    }

    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) {
        idx.add(id, &crate::arcus::element::encode(coords, idx.layout.quant))
            .unwrap();
    }

    fn search(idx: &AnnIndex, coords: &[f32], k: usize) -> Vec<String> {
        let q = crate::arcus::element::encode(coords, idx.layout.quant);
        idx.search(&q, k, |_| true)
            .unwrap()
            .into_iter()
            .filter_map(|(key, _)| idx.id_of(key).map(|id| id.to_string()))
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

        let q = crate::arcus::element::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);
        let hits = idx
            .search(&q, 10, |key| {
                idx.id_of(key).is_some_and(|id| &*id == "keep")
            })
            .unwrap();
        let ids: Vec<String> = hits
            .iter()
            .filter_map(|(k, _)| idx.id_of(*k).map(|id| id.to_string()))
            .collect();
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
    fn churn_does_not_grow_the_index() {
        // The regression this guards: keys used to index an append-only table, so a
        // removed entry left a tombstone and the reservation tracked every key ever
        // handed out. A thousand replacements of the same few vectors grew both
        // without bound while the live count never moved.
        let idx = build(4, Quant::F32, Metric::L2, 2);
        for round in 0..200 {
            for i in 0..5 {
                add(&idx, &format!("r{round}-{i}"), &[i as f32, 1.0, 2.0, 3.0]);
            }
            for i in 0..5 {
                assert!(idx.remove(&format!("r{round}-{i}")).unwrap());
            }
            assert_eq!(idx.len(), 0, "round {round} leaked live entries");
        }

        // Keys are never reused, so they climb; what must not climb is the amount
        // of state kept for them.
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.ids().by_key.len(), 0, "key -> id entries leaked");
        assert_eq!(idx.ids().by_id.len(), 0, "id -> key entries leaked");
        assert_eq!(idx.ids().next, 1000, "keys are handed out monotonically");
        // 1000 keys were issued, but the reservation only ever needed the live
        // count, which peaked at five.
        assert_eq!(
            idx.reserved.load(Ordering::Acquire),
            MIN_CAPACITY,
            "the reservation grew with keys rather than with members"
        );
    }

    #[test]
    fn a_sparse_key_is_fine_for_usearch() {
        // What makes never reusing a key affordable: usearch reserves for a member
        // count, not a key range.
        let idx = build(4, Quant::F32, Metric::L2, 2);
        for i in 0..50 {
            add(&idx, &format!("v{i}"), &[i as f32, 1.0, 2.0, 3.0]);
            idx.remove(&format!("v{i}")).unwrap();
        }
        add(&idx, "last", &[1.0, 1.0, 2.0, 3.0]);
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[1.0, 1.0, 2.0, 3.0], 1), vec!["last"]);
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
                let q = crate::arcus::element::encode(
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
                        &crate::arcus::element::encode(&[t as f32, i as f32, 0.0, 0.0], Quant::F32),
                    )
                    .unwrap();
                    let q =
                        crate::arcus::element::encode(&[t as f32, i as f32, 0.0, 0.0], Quant::F32);
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
                assert_eq!(idx.id_of(key).as_deref(), Some(id.as_str()));
            }
        }
    }

    #[test]
    fn wrong_vector_length_is_rejected() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        assert!(idx.add("a", &[0u8; 8]).is_err());
        assert!(idx.search(&[0u8; 8], 1, |_| true).is_err());
    }
}
