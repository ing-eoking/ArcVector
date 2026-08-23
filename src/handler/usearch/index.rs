//! Two locks. `inner` excludes capacity growth and nothing else, so stages and searches run
//! under it concurrently. `ids` is the one that orders writes: it is held across the element
//! write as well as the mapping write, so the two stores change together as far as any reader
//! can tell, and same-id writes need no second lock to queue behind.
//!
//! Order, wherever both are wanted: `inner` before `ids`. Node removals are therefore done
//! after the `ids` guard is released rather than under it.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

// `::` because this module shares a name with the crate it wraps.
use ::usearch::{Index, IndexOptions, ScalarKind, b1x8, f16};

use super::idmap::{IdMap, generation_of, key_of, slot_of};
use super::metric::Metric;
use crate::error::Error;
use crate::handler::arcus::element::Layout;
use crate::handler::quant::Quant;

type Result<T> = std::result::Result<T, Error>;

/// usearch reports failures as a `cxx::Exception`; carry its message through
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

/// Thread contexts reserved per index — the ceiling on how many workers can be inside
/// usearch at once. The Rust binding always enters as `any_thread()`, which pops a context
/// from a pool this sizes; an empty pool is not a wait but an error, surfaced to the client
/// as `SERVER_ERROR Reserve capacity ahead of ...`.
///
/// 64 is where memcached itself stops recommending worker counts (`memcached.c`, `-t` past
/// 64 warns), so every configuration it endorses fits. Raising it is linear in memory —
/// measured at ~1.1 MB per empty index here, ~4.4 MB at 256 — because usearch sizes its
/// stripe-lock table as `ceil2(threads * connectivity_max * 4)` cache lines.
const THREAD_SLOTS: usize = 64;

/// A `FILTER` clause: asked of every node a search visits, with the node's key and the id
/// naming it. The key comes along so a caller can keep what it read against it.
pub type Accept<'a> = &'a dyn Fn(u64, &str) -> bool;

/// A node in the graph that nothing names yet.
///
/// The mapping is what publishes a graph write — a node whose key `id_of` cannot resolve is
/// dropped by every search — so staging deliberately leaves the mapping alone. That is what
/// lets the Map element link first: until [`AnnIndex::publish`] names this node, no reader
/// and no concurrent `vdel` can see or touch it, and [`AnnIndex::discard`] has nothing to
/// put back. Neither settling call can fail.
#[must_use = "an unnamed node is invisible; publish it or discard it"]
pub struct Staged<'a> {
    key: u64,
    /// Who owned the index when this node went in. A takeover since then emptied the graph,
    /// so naming this key afterwards would name a node that is no longer there.
    owner: u64,
    /// Counts itself in while it exists, so a search can ask for enough extra results to
    /// cover the slots nodes like this one take. Decremented by `Drop`, not by `publish` and
    /// `discard`, so neither a failed insert nor a panic between staging and settling can
    /// leave the count permanently high.
    ///
    /// Because both settling calls take this by value, `Drop` runs after their bodies: the
    /// count falls only once the node has been named or taken out. **A node in the graph that
    /// no id names is therefore always counted**, which is what makes an under-read harmless
    /// — it can only mean a stage that began after the search looked. The ordering is not
    /// something a later edit can get wrong, either: a type with `Drop` cannot be taken apart
    /// early.
    in_flight: &'a AtomicUsize,
}

impl Staged<'_> {
    /// The graph key this node went in under, for a caller that has to name it in a reply or a
    /// test. The mapping records it at publish.
    pub fn key(&self) -> u64 {
        self.key
    }
}

impl<'a> Staged<'a> {
    /// Counts itself in. Made *before* the node goes into the graph, so no search can see an
    /// unnameable node the count does not cover — and if the insert then fails, `Drop` takes
    /// the count back with no error path to write.
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

/// Why a published write did not go through.
///
/// Two failures with nothing in common: the caller's own write refused, and the mapping unable
/// to take the binding that write needs. Keeping them apart is what lets `vadd` turn a full
/// index into `OVERFLOWED` while an allocation refusal answers `SERVER_ERROR` — and both are
/// replies, so neither ends the process.
///
/// Either way the mapping and the graph read as they did before the call.
#[derive(Debug)]
pub enum PublishError<E> {
    /// The element write refused. Whatever it means is the caller's to say.
    Store(E),
    /// The mapping could not grow. Nothing was written.
    Mapping(Error),
}

/// One write counted in flight without a node staged for it.
///
/// A delete needs this: between the moment a search resolved a key and the moment it renders
/// that row, the element can go, and the row drops. Counting the delete lets the search ask
/// for one more result so the answer is still `k` long.
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

/// Slots handed back, and the ones that can never come back on their own.
///
/// Kept out of `ids` so minting a key does not take the mapping lock: a stage would then queue
/// behind every search's translation for no reason.
#[derive(Default)]
struct Keys {
    /// Complete keys, generation already advanced. FIFO, so a slot is not handed straight back
    /// to the write that released it — that spreads the generation advance across slots and
    /// pushes exhaustion out by the number of live slots.
    free: std::collections::VecDeque<u64>,
    /// Slots whose generation reached `u32::MAX`. Handing one out again would let a key from
    /// before it retired answer, so it waits for a moment with no search in flight.
    retired: Vec<u32>,
    next_slot: u32,
}

/// Counts one search from before it captures keys to after it has translated them.
#[must_use = "the count only covers the search while this is alive"]
struct Searching<'a>(&'a AtomicUsize);

impl<'a> Searching<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Release);
        Searching(count)
    }
}

impl Drop for Searching<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

pub struct AnnIndex {
    pub layout: Layout,
    pub metric: Metric,
    threads: usize,
    /// Write-locked only to grow capacity; every other operation reads.
    inner: RwLock<Index>,
    reserved: AtomicUsize,
    ids: RwLock<IdMap>,
    /// Keys are minted here, not under `ids`: a fresh key needs no agreement with the
    /// mapping, only distinctness.
    keys: std::sync::Mutex<Keys>,
    /// Searches between capturing keys and translating them. A retired slot cannot be revived
    /// while this is non-zero, because such a search may hold a key the revival would make
    /// answerable again.
    ///
    /// One counter for the index rather than per worker: a search costs ~65us, so even heavily
    /// contended this pair of atomics is well under a percent of it.
    searching: AtomicUsize,
    /// Bumped by `clear`. A search that captured keys before a wipe must not translate them
    /// afterwards — the slots are handed out again from zero, so an old key would match a new
    /// id. Same idea as `Staged`'s owner token, for readers.
    epoch: AtomicU64,
    /// Staged nodes not yet published or discarded. A search asks for this many results
    /// beyond `k`, because each one can take a slot it will then be dropped from.
    in_flight: AtomicUsize,
    /// Set while a rebuild is refilling this index from Map.
    rebuilding: AtomicBool,
}

impl AnnIndex {
    pub fn new(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
    ) -> Result<Self> {
        Self::with_threads(
            layout,
            metric,
            connectivity,
            expansion_add,
            expansion_search,
            THREAD_SLOTS,
        )
    }

    /// Only the tests vary the context count; every caller gets `THREAD_SLOTS`.
    fn with_threads(
        layout: Layout,
        metric: Metric,
        connectivity: usize,
        expansion_add: usize,
        expansion_search: usize,
        threads: usize,
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
            keys: std::sync::Mutex::new(Keys::default()),
            searching: AtomicUsize::new(0),
            epoch: AtomicU64::new(0),
            ids: RwLock::new(IdMap::default()),
            rebuilding: AtomicBool::new(false),
        })
    }

    /// How many vectors are live. Not the number ever added.
    pub fn len(&self) -> usize {
        self.ids().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The id a usearch key stands for, or `None` if it has been removed.
    pub fn id_of(&self, key: u64) -> Option<Arc<str>> {
        self.ids().id_of(key).map(Arc::clone)
    }

    fn ids(&self) -> std::sync::RwLockReadGuard<'_, IdMap> {
        self.ids.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Module memory held by the key mapping, which arcus cannot see.
    pub fn id_map_bytes(&self) -> usize {
        self.ids().bytes()
    }

    /// Measured, not estimated, and chunked far ahead of the data (see [`Self::used_bytes`]).
    pub fn held_bytes(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .memory_usage()
    }

    /// Bytes of the held memory that actually carry graph nodes and vectors.
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

    /// Members usearch has room for, which grows ahead of the live count.
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
            return Ok(()); // another writer grew it while we waited
        }
        let target = (current * 2).max(needed).max(MIN_CAPACITY);
        index
            .reserve_capacity_and_threads(target, self.threads)
            .map_err(usearch_err)?;
        self.reserved.store(target, Ordering::Release);
        Ok(())
    }

    /// Insert `vector` into the graph under a key nothing names. `vector` is the
    /// already-quantized byte form. The id is not needed — and not taken — because
    /// naming the node is [`publish`](Self::publish)'s job, not this one's.
    pub fn stage(&self, vector: &[u8], owner: u64) -> Result<Staged<'_>> {
        self.check_vector(vector)?;
        // Capacity is settled first, because growing takes `inner`'s write lock. Room for
        // every possible concurrent stage, not just this one: each caller reads `live`
        // before adding to it, so `+ 1` lets N writers reserve for one node between them.
        // `THREAD_SLOTS` is the ceiling on how many can be inside usearch at once.
        self.ensure_capacity(self.live() + THREAD_SLOTS)?;

        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        // No mapping lock: minting a key coordinates with nothing, so concurrent stages do
        // not queue behind each other and searches never wait on one.
        let key = self.mint()?;

        // Counted in before the node exists, so the count is never behind the graph.
        let staged = Staged::new(key, owner, &self.in_flight);

        // A failure needs no undo: the key named nothing, so nothing named the node, and
        // `staged` drops on the way out and takes its count with it.
        self.typed_add(&index, key, vector)?;
        Ok(staged)
    }

    /// The two registrations of an insert, under one hold on the mapping.
    ///
    /// This is the only place either store becomes visible, and the element write runs *inside*
    /// the hold on purpose: a reader translates keys under the same lock, so it sees the state
    /// before the mapping was touched or after the element was linked, never between. Without
    /// that, a plain `vsim` — which reads no element at all — would answer with an id whose
    /// element is not in the Map yet, for a write that can still fail.
    ///
    /// Holding it across the engine call costs less than it looks: `map_elem_insert` takes the
    /// engine's cache lock, so the writes queueing here were queueing anyway. It also replaces
    /// the per-id lock this used to need — an exclusive hold orders same-id writes by itself.
    /// Nothing but the element write is inside: the node this write displaces comes from the
    /// mapping, so learning it costs a hash lookup rather than an engine read.
    ///
    /// A takeover since the stage emptied the graph, so the node is gone and must not be
    /// named. The element is still written: a rebuild reads the Map, so the write survives
    /// there and the refill indexes it.
    pub fn insert_published<T, E>(
        &self,
        id: &str,
        staged: Staged<'_>,
        owner: u64,
        store: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, PublishError<E>> {
        // Never taken while `inner` is held, so the order cannot invert against `clear`. Node
        // removals below happen after the guard is released for the same reason.
        let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);
        // A write already holds the mapping exclusively, which is where retired slots can be
        // put back without racing a translation.
        self.reclaim_retired(&mut ids);
        let live = staged.owner == owner;
        // The room for the binding comes before the binding, and before the element write it
        // gates. A std collection that cannot grow aborts instead of returning, so the growth
        // has to be asked for where the answer can still be a reply. Nothing is written yet,
        // so the only thing to unwind is this call's own node.
        if live && let Err(e) = ids.reserve_binding(id, staged.key) {
            drop(ids);
            let key = staged.key;
            drop(staged);
            self.drop_node(key);
            return Err(PublishError::Mapping(Error::Index(format!(
                "the id mapping could not grow: {e}"
            ))));
        }
        let displaced = if live { ids.bind(id, staged.key) } else { None };

        match store() {
            Ok(value) => {
                drop(ids);
                // Whichever node is now unreachable: the one this write replaced, or its own
                // if a takeover means nothing will name it.
                match (live, displaced) {
                    (true, Some(key)) => self.drop_node(key),
                    (false, _) => self.drop_node(staged.key),
                    (true, None) => {}
                }
                Ok(value)
            }
            Err(e) => {
                // The element was never written, so nothing left this node and the mapping has
                // to read as it did before. Neither restore can allocate: `bind` here only
                // overwrites an id `by_id` already holds, into a slot that already exists.
                if live {
                    match displaced {
                        Some(key) => {
                            ids.bind(id, key);
                        }
                        None => {
                            ids.forget(id);
                        }
                    }
                }
                drop(ids);
                self.drop_node(staged.key);
                Err(PublishError::Store(e))
            }
        }
    }

    /// Throw a staged node away. The mapping never named it, so there is nothing to restore.
    pub fn discard(&self, staged: Staged<'_>) {
        self.drop_node(staged.key);
    }

    /// Mint a key nothing has held before.
    ///
    /// A recycled slot comes back with its generation advanced, so the key is new even though
    /// the position is not. Running out is an error rather than a wrap: reviving a slot early
    /// would let a key a search is still holding answer for a different id.
    fn mint(&self) -> Result<u64> {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = keys.free.pop_front() {
            return Ok(key);
        }
        let slot = keys.next_slot;
        keys.next_slot = slot.checked_add(1).ok_or_else(|| {
            Error::Index(format!(
                "index is out of graph slots ({} retired and waiting for a quiet moment)",
                keys.retired.len()
            ))
        })?;
        Ok(key_of(slot, 0))
    }

    /// Detach a node nothing maps to any more, and hand its slot back.
    ///
    /// A failure to remove leaks a node no id names, which every reader already drops, so
    /// there is nothing for a caller to do about it. The slot goes back either way: the key
    /// that named it is dead whether or not the node went.
    fn drop_node(&self, key: u64) {
        {
            let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            let _ = index.remove(key);
        }
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        match generation_of(key).checked_add(1) {
            Some(next) => keys.free.push_back(key_of(slot_of(key) as u32, next)),
            // Its last generation is spent. Reviving it needs a moment with no search holding
            // keys, which `reclaim_retired` waits for.
            None => keys.retired.push(slot_of(key) as u32),
        }
    }

    /// Put retired slots back in service, if no search could be holding one of their old keys.
    ///
    /// Called by writes, which already hold `ids` exclusively. A search raises `searching`
    /// before it captures any key and lowers it after translating, so zero here means no key
    /// is in flight. On a permanently busy index this never fires — and that is survivable,
    /// because a slot retires at worst once a day under the most concentrated write load
    /// there is, so the slot space outlasts the process by a wide margin.
    fn reclaim_retired(&self, ids: &mut IdMap) {
        if self.searching.load(Ordering::Acquire) != 0 {
            return;
        }
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        if keys.retired.is_empty() {
            return;
        }
        // Re-check under the lock that raised it, so a search that started meanwhile is seen.
        if self.searching.load(Ordering::Acquire) != 0 {
            return;
        }
        let reviving: Vec<u32> = keys.retired.drain(..).collect();
        for slot in reviving {
            ids.revive(slot);
            keys.free.push_back(key_of(slot, 0));
        }
    }

    /// Add `id` only if a rebuild has no reason to leave it alone.
    ///
    /// `vector` runs inside the hold: the refill's read of the stored element and its claim on
    /// the mapping have to be one step, or a live write that lands between them is replayed
    /// over. `None` means the element went away since the refill listed it.
    ///
    /// The key is minted here. Nothing stored says what a rebuilt node should be called, so a
    /// refill names its nodes afresh; only the ids have to come back.
    pub fn add_unless_known(
        &self,
        id: &str,
        vector: impl FnOnce() -> Result<Option<Vec<u8>>>,
    ) -> Result<bool> {
        // `inner` before `ids`, the order `clear` takes them in.
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);

        if ids.is_known(id) {
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
        // `is_known` just said no, so the mapping is this call's to undo. The name goes on
        // before the insert here — unlike an insert's stage — because the refill has to hold
        // the `is_known` verdict and the claim on `id` under one lock.
        let key = self.mint()?;
        // Before the binding and before the node, so a refusal costs this call and nothing else.
        ids.reserve_binding(id, key)
            .map_err(|e| Error::Index(format!("the id mapping could not grow: {e}")))?;
        ids.bind(id, key);
        if let Err(e) = self.typed_add(&index, key, &vector) {
            ids.forget(id);
            return Err(e);
        }
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
        self.ids
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
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

    pub fn reserve(&self, count: usize) -> Result<()> {
        self.ensure_capacity(count)
    }

    /// Throw away every member, keeping the index's shape.
    pub fn begin_rebuild(&self) -> Result<()> {
        // Recording deletes starts before the graph is emptied, so a racing delete is still remembered.
        self.rebuilding.store(true, Ordering::Release);
        self.clear()
    }

    /// Stop recording deletes and drop what was recorded.
    pub fn end_rebuild(&self) {
        let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);
        self.rebuilding.store(false, Ordering::Release);
        ids.tombstones.clear();
    }

    pub fn clear(&self) -> Result<()> {
        let index = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);
        index.reset().map_err(usearch_err)?;
        // `reset` deallocates the thread contexts along with the members, so put the floor
        // back under both: an `AnnIndex` is never without a context for a search to take.
        index
            .reserve_capacity_and_threads(MIN_CAPACITY, self.threads)
            .map_err(usearch_err)?;
        self.reserved.store(MIN_CAPACITY, Ordering::Release);
        // Anything staged against the graph this emptied must not be named afterwards. The
        // caller changes the index's owner before getting here, which is what says so.
        *ids = IdMap::default();
        // Slots start over too, so the array does not carry a dead prefix the size of every
        // key the old graph ever used. That makes old keys answerable again, which is what the
        // epoch closes: a search that captured keys before this point discards them.
        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = Keys::default();
        self.epoch.fetch_add(1, Ordering::Release);
        drop(ids);
        Ok(())
    }

    /// The two removals of a delete, under one hold on the mapping.
    ///
    /// `take` unlinks the element and reports whether there was one — the engine first, always,
    /// because that is the copy replicas and the persistence log follow. Under the same hold as
    /// the mapping removal, a reader cannot see the element gone while the mapping still names
    /// the node, nor the reverse.
    ///
    /// The count is raised for the whole call: a search that resolved this id just before the
    /// hold renders after it, finds nothing to read, and drops the row.
    ///
    /// `None` means there was no element. `Some(named)` also says whether the mapping had a key
    /// for it — a rebuild that has not reached this id yet leaves it unnamed.
    pub fn remove_published<E>(
        &self,
        id: &str,
        take: impl FnOnce() -> std::result::Result<bool, E>,
    ) -> std::result::Result<Option<bool>, PublishError<E>> {
        let _slack = InFlight::new(&self.in_flight);
        let mut ids = self.ids.write().unwrap_or_else(PoisonError::into_inner);

        // While rebuilding, a deleted id is remembered so the refill cannot replay it — and
        // the room for that has to be taken *before* the element goes. Once the engine has
        // deleted it there is no failure left to report: the delete is already on its way to
        // the replicas, and refusing here would answer for a state that no longer exists.
        let tombstone = self.rebuilding.load(Ordering::Acquire);
        if tombstone && let Err(e) = ids.reserve_tombstone() {
            return Err(PublishError::Mapping(Error::Index(format!(
                "the id mapping could not grow: {e}"
            ))));
        }

        if !take().map_err(PublishError::Store)? {
            return Ok(None);
        }

        let key = if tombstone {
            ids.forget_tombstoned(id)
        } else {
            ids.forget(id)
        };
        drop(ids);

        // After the hold, and unchecked: it leaves a node no id names, which every reader
        // already drops.
        if let Some(key) = key {
            self.drop_node(key);
        }
        Ok(Some(key.is_some()))
    }

    /// Translate search hits under one hold, so no write's two steps are seen half-done.
    ///
    /// Keys the mapping does not name are dropped: a node staged by a `vadd` that has not
    /// reached [`Self::insert_published`] yet, one whose element a `vdel` just took, or one
    /// whose slot has since been recycled — the generation in the key is what tells those
    /// apart from a live node.
    ///
    /// `entered` is the epoch the search started in. A `clear` between then and now handed the
    /// slots out again from zero, so these keys describe a graph that no longer exists.
    fn resolve(&self, hits: &[(u64, f32)], entered: u64) -> Vec<(u64, Arc<str>, f32)> {
        let ids = self.ids();
        if self.epoch.load(Ordering::Acquire) != entered {
            return Vec::new();
        }
        hits.iter()
            .filter_map(|(key, distance)| {
                let id = ids.id_of(*key)?;
                Some((*key, Arc::clone(id), *distance))
            })
            .collect()
    }

    /// k-NN search. `accept` is called once per visited graph node and must be
    /// The quantization-typed search call with no callback at all.
    fn unfiltered(&self, index: &Index, query: &[u8], k: usize) -> Result<::usearch::ffi::Matches> {
        match self.layout.quant {
            Quant::F32 => index.search(&to_f32(query), k),
            Quant::F16 => index.search(f16::from_i16s(&to_i16(query)), k),
            Quant::I8 => index.search(&to_i8(query), k),
            Quant::B1 => index.search(b1x8::from_u8s(query), k),
        }
        .map_err(usearch_err)
    }

    /// The quantization-typed search call, so the predicate is built once per variant.
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

    /// k-NN search, returning what each hit is named. `accept` is the `FILTER` clause, called once per visited node with the
    /// key and the id naming it; `None` means there is no clause and usearch runs with no
    /// callback at all. The key comes along so a caller can keep what it read against it.
    ///
    /// Nothing else is checked per node. A staged node — in the graph, named by nothing —
    /// needs no check of its own: a `FILTER` cannot read the element it does not have yet and
    /// rejects it, and without a `FILTER` the caller drops it when `id_of` comes back empty
    /// while rendering. So the only per-node work is the clause the client asked for.
    ///
    /// A liveness callback of our own was measured and removed. It put a lock on usearch's
    /// otherwise lock-free read path, and there was nowhere good to hold it: per node it
    /// collapsed search (8 threads, 2.3 → 20.8 us/query), and once per search it moved onto
    /// the write path (`publish` p50 0.5 → 83 us under four searchers).
    ///
    /// **May return more than `k`.** A staged node can take a result slot, so this asks
    /// usearch for one extra per write in flight; unnameable ones are dropped here and the
    /// caller renders at most `k`. Over-asking also keeps the pruning radius from closing on
    /// the `k`th distance while an unnameable node sits inside it — otherwise a real
    /// neighbour is cut from the candidates before it can be considered.
    ///
    /// Capturing the keys and translating them is one call on purpose. A caller that did the
    /// two halves itself would have to raise `searching` before the first and lower it after
    /// the second, and getting that wrong lets a retired slot come back under a key this
    /// search is still holding.
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

        // Raised before any key is captured and lowered after they are all translated, which
        // is what lets a retired slot be revived only when no key is in flight.
        let _searching = Searching::new(&self.searching);
        let entered = self.epoch.load(Ordering::Acquire);

        // One extra per write in flight. `Relaxed` because the count carries no ordering:
        // the gap between a node entering the graph and the count rising is a window in time,
        // which no ordering closes, and reading low only costs the row this slack was added
        // to save. Saturating because the pairing that keeps the count small is structural —
        // `Staged::new` raises it and `Drop` lowers it — and an overflow here would panic on
        // a search rather than fail where the accounting broke.
        let k = k.saturating_add(self.in_flight.load(Ordering::Relaxed));

        let matches = {
            let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            match accept {
                // No callback: usearch takes its `is_dummy` path, which still excludes what it
                // removed itself, so nothing on this side is consulted per node.
                None => self.unfiltered(&index, query, k),
                Some(matches) => self.matches(&index, query, k, |key| match self.id_of(key) {
                    Some(id) => matches(key, &id),
                    None => false,
                }),
            }?
        };

        let hits: Vec<(u64, f32)> = matches.keys.into_iter().zip(matches.distances).collect();
        Ok(self.resolve(&hits, entered))
    }
}

// The stored bytes come straight out of an arcus item and carry no alignment

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

    /// The production insert with a stubbed element write: the mapping writes, the ordering
    /// and the lock are all real, only the engine call is absent.
    fn publish(idx: &AnnIndex, id: &str, staged: Staged<'_>, owner: u64) {
        let done: std::result::Result<(), PublishError<()>> =
            idx.insert_published(id, staged, owner, || Ok(()));
        assert!(done.is_ok(), "publish failed");
    }

    /// Likewise for the delete, with the element already known to be there.
    fn remove(idx: &AnnIndex, id: &str) -> bool {
        let removed: std::result::Result<Option<bool>, PublishError<()>> =
            idx.remove_published(id, || Ok(true));
        let Ok(removed) = removed else {
            panic!("remove failed");
        };
        removed.unwrap_or(false)
    }

    fn build(dim: usize, quant: Quant, metric: Metric, threads: usize) -> AnnIndex {
        AnnIndex::with_threads(Layout::new(dim, quant), metric, 0, 0, 0, threads).unwrap()
    }

    /// Leave usearch with no thread contexts, which is what an over-subscribed worker pool
    /// looks like from the inside: the next entry fails instead of waiting. `reserved` is
    /// left alone on purpose, so `ensure_capacity` does not quietly undo it.
    fn exhaust_contexts(idx: &AnnIndex) {
        idx.inner.write().unwrap().reset().unwrap();
    }

    /// Every name must point at a node the graph actually holds.
    ///
    /// Only checkable at rest. `clear` takes `inner`'s write lock and `publish` takes `ids`',
    /// so they do not exclude each other: even with the generation check, a `clear` can land
    /// between the check and the `bind`. Correct code violates this *during* a rebuild and
    /// satisfies it once the writers stop.
    fn assert_names_resolve_to_nodes(idx: &AnnIndex, note: &str) {
        let index = idx.inner.read().unwrap();
        for (key, id) in idx.ids().iter() {
            assert!(
                index.contains(key),
                "{note}: {id} names key {key}, which the graph does not have"
            );
        }
    }

    /// No two keys may name the same id. That is what a bijection means now that only one
    /// direction is kept: a second key naming an id is a second hit for it in every search.
    fn assert_one_key_per_id(idx: &AnnIndex, note: &str) {
        let ids = idx.ids();
        let mut seen: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        for (key, id) in ids.iter() {
            if let Some(other) = seen.insert(id, key) {
                panic!("{note}: {id} is named by both {other} and {key}");
            }
        }
        assert_eq!(seen.len(), ids.len());
    }

    /// Stands in for `VectorIndex`'s token: the same value means no takeover happened.
    const OWNER: u64 = 7;

    /// A settled write, returning the key the mapping now holds for this id.
    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        put(idx, id, coords)
    }

    /// A write, whether or not the id is already there — the mapping finds what it displaces.
    fn put(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        let staged = idx
            .stage(
                &crate::handler::quant::encode(coords, idx.layout.quant),
                OWNER,
            )
            .unwrap();
        let key = staged.key();
        publish(idx, id, staged, OWNER);
        key
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
    fn the_predicate_sees_the_id_and_excludes_what_it_rejects() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "keep", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "skip", &[1.0, 0.0, 0.0, 0.0]);

        let q = crate::handler::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32);
        let keep = |_key: u64, id: &str| id == "keep";
        let hits = idx.search(&q, 10, Some(&keep as Accept)).unwrap();
        let ids: Vec<String> = hits.iter().map(|(_, id, _)| id.to_string()).collect();
        assert_eq!(ids, vec!["keep"]);
    }

    #[test]
    fn readding_an_id_moves_it_to_a_fresh_key() {
        // The old node has to stay put until the element links, so the new vector cannot
        // share its key. Nothing outside the graph names keys, so only `len` has to hold.
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let first = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        let second = put(&idx, "a", &[0.0, 1.0, 0.0, 0.0]);
        assert_ne!(second, first);
        assert_eq!(idx.id_of(first), None, "the displaced key names nothing");
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
            idx.keys.lock().unwrap().next_slot,
            5,
            "slots come back, so 1000 writes need only the peak live count"
        );
        assert!(
            idx.keys.lock().unwrap().retired.is_empty(),
            "no slot came close to spending its generations"
        );
        assert_eq!(
            idx.reserved.load(Ordering::Acquire),
            MIN_CAPACITY,
            "the reservation grew with keys rather than with members"
        );
    }

    #[test]
    fn a_sparse_key_is_fine_for_usearch() {
        // usearch reserves for a member count, not a key range, so a recycled slot arriving
        // with a fresh generation is a key it has never seen and does not have to have room for.
        let idx = build(4, Quant::F32, Metric::L2, 2);
        for i in 0..50 {
            add(&idx, &format!("v{i}"), &[i as f32, 1.0, 2.0, 3.0]);
            remove(&idx, &format!("v{i}"));
        }
        add(&idx, "last", &[1.0, 1.0, 2.0, 3.0]);
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[1.0, 1.0, 2.0, 3.0], 1), vec!["last"]);
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
        assert_eq!(idx.id_of(a), None);

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
        // `len` feeds vadd's maxcount check, so a phantom entry can reject a real one.
        assert_eq!(idx.len(), 1, "the failed write must not count");
    }

    #[test]
    fn a_failed_refill_add_leaves_no_mapping_behind() {
        let idx = build(2, Quant::F32, Metric::Cos, 2);
        exhaust_contexts(&idx);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        assert!(idx.add_unless_known("b", || Ok(Some(v.to_vec()))).is_err());
        assert_eq!(idx.id_of(42), None);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn a_staged_node_is_invisible_until_it_is_published() {
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();
        // The node is in the graph, but no id names it and nothing counts it.
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[0.0, 1.0], 5), vec!["a"], "no unnamed hits");

        publish(&idx, "b", staged, OWNER);
        assert_eq!(idx.len(), 2);
        assert_eq!(search(&idx, &[0.0, 1.0], 1), vec!["b"]);
    }

    #[test]
    fn a_staged_node_does_not_cost_a_result_slot() {
        // Five named vectors, all near the query, and one staged node nearer than any of
        // them. Asking for three must still answer with three: the staged node takes a slot
        // in usearch's answer, so `search` asks for one more than it was told to.
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

        // And the slack is gone again once the write settles.
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

        // Dropped without settling — what a panic between staging and the engine call leaves.
        drop(idx.stage(&v, OWNER).unwrap());
        assert_eq!(
            idx.in_flight.load(Ordering::Relaxed),
            0,
            "dropped unsettled"
        );

        // A stage whose insert fails. Balanced under either ordering — taking the count
        // before the insert is for the window between the insert returning and the count
        // rising, which no test can observe — but this pins that the failure path leaves
        // nothing behind.
        exhaust_contexts(&idx);
        assert!(idx.stage(&v, OWNER).is_err());
        assert_eq!(idx.in_flight.load(Ordering::Relaxed), 0, "failed insert");
    }

    #[test]
    fn a_discarded_overwrite_leaves_the_mapping_alone() {
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        let key = add(&idx, "a", &[1.0, 0.0]);

        // What `vadd` does when the element cannot be linked.
        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        idx.discard(idx.stage(&v, OWNER).unwrap());

        assert_eq!(
            idx.id_of(key).as_deref(),
            Some("a"),
            "the mapping was never moved"
        );
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[1.0, 0.0], 5), vec!["a"]);
    }

    #[test]
    fn a_takeover_inside_the_staging_window_voids_the_stage() {
        // `take_over` sets the index's owner to REBUILDING and then empties the graph, so a
        // publish carrying the older token is naming a node that is no longer there. The wipe
        // also restarts the slots, so the owner token is the only thing standing between the
        // stale publish and a key that now belongs to somebody else.
        const TAKEOVER: u64 = 0;
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();
        let staged_key = staged.key();
        idx.begin_rebuild().unwrap();

        publish(&idx, "b", staged, TAKEOVER);
        assert_eq!(idx.len(), 0, "the stage did not survive the takeover");

        // The wipe hands slots out from zero again, so the next key may well be numerically
        // smaller. What must hold is that the abandoned key names nothing.
        let abandoned = staged_key;
        let again = idx.stage(&v, TAKEOVER).unwrap();
        publish(&idx, "b", again, TAKEOVER);
        assert_eq!(
            idx.id_of(abandoned),
            None,
            "the key staged against the wiped graph still names nothing"
        );
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[0.0, 1.0], 5), vec!["b"]);
    }

    #[test]
    fn a_delete_inside_the_staging_window_is_not_lost() {
        // The element links after a concurrent vdel removed the old one. Map is truth and
        // says the element is there, so the graph has to end up naming the new node —
        // never the state where the Map holds an element the graph knows nothing about.
        let idx = build(2, Quant::F32, Metric::Cos, 4);
        add(&idx, "a", &[1.0, 0.0]);

        let v = crate::handler::quant::encode(&[0.0, 1.0], Quant::F32);
        let staged = idx.stage(&v, OWNER).unwrap();

        // The vdel lands in the window: the element says key `old`, so that is all it can
        // remove — the staged node is not in the element and not in the mapping.
        assert!(remove(&idx, "a"));
        assert_eq!(idx.len(), 0);

        // The vadd's link then succeeds.
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

        // The link failed, so the delete stands.
        idx.discard(staged);
        assert_eq!(idx.len(), 0);
        assert!(search(&idx, &[0.0, 1.0], 5).is_empty());
    }

    #[test]
    fn a_cleared_index_is_still_searchable() {
        // `reset` frees the thread contexts, and `search` never reserves. Without the
        // floor `clear` puts back, this fails with "Reserve capacity ahead of searches!".
        let idx = build(2, Quant::F32, Metric::Cos, 2);
        add(&idx, "a", &[1.0, 0.0]);

        idx.clear().unwrap();
        // What a refill over a vectorless Map asks for: nothing.
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
        // One context per searcher, which is what THREAD_SLOTS buys: 16 against 16.
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

    /// Deterministic per-thread pseudo-randomness: no `Math.random`, same schedule every run
    /// in what each thread *decides*, while the OS still interleaves them differently.
    fn choice(t: usize, i: usize, m: usize) -> usize {
        (t.wrapping_mul(0x9E37)
            .wrapping_add(i)
            .wrapping_mul(2654435761))
            % m
    }

    /// Stands in for the Map: which ids have an element. No keys in here, because a stored
    /// element does not carry one either — the mapping owns both directions.
    type Elements = std::sync::Mutex<std::collections::HashSet<String>>;

    #[test]
    fn concurrent_writers_deleters_and_searchers_keep_the_mapping_consistent() {
        // Every writer works the same small pool of ids, so publishes, discards and deletes
        // land on each other rather than running side by side. The element write and the
        // element delete happen inside the published calls, which is where the engine's happen
        // — a test that touched `elements` outside them would be racing on its own.
        const IDS: usize = 24;
        let dim = 8;
        let idx = Arc::new(build(dim, Quant::F32, Metric::L2, 16));
        let elements: Arc<Elements> = Arc::new(std::sync::Mutex::new(Default::default()));
        for i in 0..IDS {
            let id = format!("id{i}");
            add(&idx, &id, &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
            elements.lock().unwrap().insert(id);
        }

        let stop = Arc::new(AtomicBool::new(false));
        // Keys whose stage was thrown away. The set only grows, so "was this ever discarded"
        // is a question with a stable answer — unlike "is this named right now", which a
        // concurrent writer can change between the search and the check.
        let discarded: Arc<std::sync::Mutex<std::collections::HashSet<u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let leaked = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        // Writers: stage, then publish or discard — a vadd whose insert did or did not land.
        for t in 0..4 {
            let idx = Arc::clone(&idx);
            let elements = Arc::clone(&elements);
            let discarded = Arc::clone(&discarded);
            handles.push(std::thread::spawn(move || {
                for i in 0..250 {
                    let id = format!("id{}", choice(t, i, IDS));
                    let coords: Vec<f32> = (0..dim).map(|d| (t * 31 + i + d) as f32).collect();
                    let v = crate::handler::quant::encode(&coords, Quant::F32);
                    let staged = idx.stage(&v, OWNER).unwrap();
                    let key = staged.key();
                    if choice(t, i + 7, 4) == 0 {
                        // Record before discarding, so a searcher can never see the key as
                        // returnable while the test still thinks it was published.
                        discarded.lock().unwrap().insert(key);
                        idx.discard(staged);
                    } else {
                        // The element is read and written inside the call, which is what the
                        // engine does: both live under the same hold on the mapping, and a
                        // test that touched `elements` outside it would be racing on its own.
                        let done: std::result::Result<(), PublishError<()>> =
                            idx.insert_published(&id, staged, OWNER, || {
                                elements.lock().unwrap().insert(id.clone());
                                Ok(())
                            });
                        done.unwrap();
                    }
                }
            }));
        }

        // Deleters: the vdel that lands inside somebody's staging window.
        for t in 4..6 {
            let idx = Arc::clone(&idx);
            let elements = Arc::clone(&elements);
            handles.push(std::thread::spawn(move || {
                for i in 0..250 {
                    let id = format!("id{}", choice(t, i, IDS));
                    // `vdel` takes the element and the key in it in one engine call, inside
                    // the same hold that removes the name.
                    let removed: std::result::Result<Option<bool>, PublishError<()>> =
                        idx.remove_published(&id, || Ok(elements.lock().unwrap().remove(&id)));
                    removed.unwrap();
                }
            }));
        }

        // Searchers: `search` may hand back a staged or discarded key — there is no callback
        // to refuse one, and the caller drops it when `id_of` comes back empty. What must
        // never happen is such a key *resolving*: a discarded key was never bound and never
        // will be, so if one ever names an id the caller's drop would not fire.
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
                        // Reaching here means the key resolved: `search` drops what it cannot
                        // name. A discarded key must never get that far.
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
        // And it stays that way at rest: nothing a discard threw away can be named.
        for key in discarded.lock().unwrap().iter() {
            assert_eq!(idx.id_of(*key), None, "discarded key {key} is named");
        }
        // Every element the Map still holds is named, and nothing else is.
        let live = elements.lock().unwrap().clone();
        let named: std::collections::HashSet<String> =
            idx.ids().iter().map(|(_, id)| id.to_string()).collect();
        assert_eq!(
            named, live,
            "the mapping and the Map disagree on which ids exist"
        );
    }

    #[test]
    fn a_rebuild_racing_writers_leaves_the_mapping_consistent() {
        // `clear` empties the graph under writers that already staged against it. The owner
        // check is what keeps those publishes from naming nodes that are gone.
        const IDS: usize = 16;
        let dim = 8;
        let idx = Arc::new(build(dim, Quant::F32, Metric::L2, 16));
        let elements: Arc<Elements> = Arc::new(std::sync::Mutex::new(Default::default()));
        for i in 0..IDS {
            let id = format!("id{i}");
            add(&idx, &id, &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
            elements.lock().unwrap().insert(id);
        }

        let mut handles = Vec::new();
        for t in 0..4 {
            let idx = Arc::clone(&idx);
            let elements = Arc::clone(&elements);
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    let id = format!("id{}", choice(t, i, IDS));
                    let coords: Vec<f32> = (0..dim).map(|d| (t + i + d) as f32).collect();
                    let v = crate::handler::quant::encode(&coords, Quant::F32);
                    // A stage can fail once `clear` has dropped the reservation to the floor.
                    if let Ok(staged) = idx.stage(&v, OWNER) {
                        // Both element accesses inside the call, as the engine does them.
                        let done: std::result::Result<(), PublishError<()>> =
                            idx.insert_published(&id, staged, OWNER, || {
                                elements.lock().unwrap().insert(id.clone());
                                Ok(())
                            });
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

        // Names are not asserted to resolve here: a rebuild running under writers legitimately
        // leaves some pointing at wiped nodes, and the interleaving that the generation check
        // exists for is pinned down by `a_clear_inside_the_staging_window_voids_the_stage`
        // instead — a threaded test cannot be relied on to reproduce one window. What this
        // test is for is everything nobody thought to order by hand: structural corruption,
        // a panic, a deadlock.
        assert_one_key_per_id(&idx, "after a rebuild raced writers");

        // And the index still works afterwards.
        let _ = add(&idx, "after", &[9.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(
            search(&idx, &[9.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], 1),
            vec!["after"]
        );
        assert_one_key_per_id(&idx, "after writing to a raced index");
    }

    #[test]
    fn concurrent_adds_and_searches_do_not_corrupt_the_id_map() {
        // 8 searchers plus the one add that holds the mapping lock: 9 entrants, 16 contexts.
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 16));
        // Ids are unique per thread here, so nothing displaces anything and the keys can be
        // collected rather than read back from an element.
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
                    let key = staged.key();
                    publish(&idx, &id, staged, OWNER);
                    keys.lock().unwrap().push((id, key));
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
            assert_eq!(idx.id_of(*key).as_deref(), Some(id.as_str()));
        }
    }

    #[test]
    fn held_memory_is_chunked_while_used_memory_tracks_the_data() {
        // usearch's tape allocators grow in 8 MiB chunks, so held leaps once and sits still while used tracks the data.
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

    /// What left `vadd`'s critical section, in wall time.
    ///
    /// `stage` is the graph insert, now prepared outside the id lock; `publish` is the hash
    /// map insert that is still inside it. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn stage_costs_far_more_than_publish() {
        let idx = build(768, Quant::F32, Metric::Cos, 8);
        let owner = 0;

        // Distinct vectors. Inserting the same one repeatedly degenerates the graph — every
        // candidate sits at distance zero — and measures nothing a real index does.
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

        // Warm the graph so neither phase is measuring an empty index.
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

    /// A slot whose generations are spent waits, and a later write puts it back.
    ///
    /// Reaching `u32::MAX` for real takes about 4.3 billion reuses of one slot, so the retired
    /// slot is planted rather than earned; what is under test is the handover, not the counter.
    #[test]
    fn a_retired_slot_comes_back_when_no_search_is_in_flight() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        idx.keys.lock().unwrap().retired.push(9);
        assert!(idx.keys.lock().unwrap().free.is_empty());

        // A write holds the mapping, which is where the handover happens.
        add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);
        let keys = idx.keys.lock().unwrap();
        assert!(
            keys.retired.is_empty(),
            "the slot was taken off the retired list"
        );
        assert!(
            keys.free.contains(&key_of(9, 0)),
            "and offered again at generation zero"
        );
    }

    #[test]
    fn a_retired_slot_stays_put_while_a_search_holds_keys() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        idx.keys.lock().unwrap().retired.push(9);

        // Stand in for a search between capturing keys and translating them.
        let searching = Searching::new(&idx.searching);
        add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);
        assert_eq!(
            idx.keys.lock().unwrap().retired,
            vec![9],
            "a key in flight could still be one this slot answered"
        );

        drop(searching);
        add(&idx, "c", &[0.0, 0.0, 1.0, 0.0]);
        assert!(
            idx.keys.lock().unwrap().retired.is_empty(),
            "and it comes back once nothing is holding keys"
        );
    }

    /// Slots are handed back, so a long churn does not walk the slot space forward.
    #[test]
    fn a_released_slot_is_offered_again_with_a_fresh_generation() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let first = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        assert!(remove(&idx, "a"));

        let second = add(&idx, "b", &[0.0, 1.0, 0.0, 0.0]);
        assert_eq!(
            slot_of(second),
            slot_of(first),
            "the slot came back rather than the space growing"
        );
        assert_ne!(second, first, "but not as the same key");
        assert_eq!(idx.id_of(first), None, "so the old key names nothing");
        assert_eq!(idx.id_of(second).as_deref(), Some("b"));
    }

    /// A wipe restarts the slots, and the epoch is what keeps a search from translating keys
    /// it captured against the graph that is gone.
    #[test]
    fn keys_captured_before_a_wipe_are_discarded() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let key = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        let entered = idx.epoch.load(Ordering::Acquire);

        idx.clear().unwrap();
        add(&idx, "b", &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            slot_of(add(&idx, "c", &[0.0, 1.0, 0.0, 0.0])),
            1,
            "slots start over after a wipe"
        );

        assert!(
            idx.resolve(&[(key, 0.0)], entered).is_empty(),
            "a search that captured keys before the wipe translates none of them"
        );
    }
}
