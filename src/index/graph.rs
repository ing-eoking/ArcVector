use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use ::usearch::{Index, IndexOptions, ScalarKind, VectorType, b1x8, f16, ffi::Matches};

use super::elements::Elements;
use super::metric::Metric;
use crate::engine::element::Layout;
use crate::error::Error;
use crate::index::halt::Halt;
use crate::index::quant::Quant;
use crate::index::retire::{Progress, Retirement};

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

/// Upper bound on queue entries a search processes on its way out.
///
/// This cost is added to the request's response time, so it must be bounded. The old
/// `RECLAIM_BATCH` existed for the same reason.
const READER_DRAIN_BUDGET: usize = 8;

pub type Accept<'a> = &'a dyn Fn(u64) -> bool;

#[cfg(test)]
static NEXT_IDENTITY: AtomicUsize = AtomicUsize::new(1);

impl Drop for AnnIndex {
    /// Releases every `Release` still pending in `retirement`.
    ///
    /// This is the only place where that is safe: searches hold an `Arc<VectorIndex>` while
    /// running (see `cmd::access`), so by the time this `drop` can run no search exists.
    /// Whatever the remaining barriers in `retirement` were guarding, there is nothing left
    /// to protect, so the addresses can be returned directly without going through the queue
    /// order. Simply discarding `retirement` (as the old code did) would leak addresses that
    /// had already left the graph and survived only as `Release` entries: once the index
    /// leaves the registry the sweeper cannot reach them, so they stay held until the
    /// process exits.
    fn drop(&mut self) {
        self.retirement.release_pending(&*self.elements);
    }
}

/// How many independent usearch graphs one index spreads its vectors over.
///
/// usearch takes a global level lock and a handful of per-structure mutexes on
/// every insertion, and past two concurrent writers a single graph spends more
/// of the insert waiting on those than computing distances: the scaling probes
/// below measure x1.5 over four threads on one shared graph, and x3.0 on four
/// separate ones -- the machine's own ceiling. Shards are the separate-graphs
/// case behind one face.
///
/// More shards buy insert concurrency and cost search fan-out, since a query
/// asks every shard for the full k, so the count follows the number of writers
/// the server realistically runs at once, not the core count.
const GRAPH_SHARDS: usize = 1;

/// The graph, as `GRAPH_SHARDS` usearch indexes behind the API of one.
///
/// A vector is added to the shard its key hashes to and never moves: the
/// rename from placeholder key to element address happens inside that shard,
/// so an address does not hash to the shard that holds it, and lookups by key
/// ask the routed shard first and then the rest. Each ask is one hash-table
/// probe, nothing next to the distance work it stands in front of.
///
/// A search asks every shard for the full k and keeps the k nearest of what
/// comes back. That answers exactly what the single graph answered: every
/// vector is in some shard, and a vector among the true k nearest is among its
/// own shard's k nearest, so the merge cannot have lost it.
struct Shards {
    shards: Vec<Index>,
    /// Set for a shard once that shard's first `add` has returned. Until then
    /// a search must not enter that graph at all.
    ///
    /// usearch's `add` reserves its slot by bumping `nodes_count_`
    /// (index.hpp:3222) fifteen lines before it stores the node
    /// (index.hpp:3237) and eighteen before the callback fills the vector
    /// pointer (index.hpp:3240). `search` guards on that same counter
    /// (index.hpp:3448), so it walks into the window and starts at
    /// `entry_slot_`, still its initial 0 -- the slot being filled.
    /// `vectors_lookup_` is never zeroed (`buffer_gt` skips `construct_at` for
    /// trivially constructible types, index.hpp:418), the distance kernel
    /// takes the pointer with no null check, and the process dies.
    /// `usearch_add_search_race` reproduces it on a raw index on 2.26.0,
    /// 2.26.1 and 2.26.2.
    ///
    /// Only the entry point is exposed this way. Every other slot is reached
    /// through links, and a link to a slot is formed after that slot's vector
    /// is in place, so one finished add per shard closes the window for good.
    /// The flag has to live here because usearch cannot answer the question:
    /// `size()` reads the very counter that is published too early.
    populated: Vec<AtomicBool>,
}

impl Shards {
    fn new(options: &IndexOptions) -> Result<Self> {
        let mut shards = Vec::with_capacity(GRAPH_SHARDS);
        for _ in 0..GRAPH_SHARDS {
            shards.push(Index::new(options).map_err(usearch_err)?);
        }
        let populated = (0..GRAPH_SHARDS).map(|_| AtomicBool::new(false)).collect();
        Ok(Self { shards, populated })
    }

    /// Where a new key goes. Placeholder keys count up and element addresses
    /// are aligned pointers; the Fibonacci multiply spreads either, and taking
    /// the high half keeps an address's trailing zero bits out of the pick.
    fn route_to(&self, key: u64) -> usize {
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % self.shards.len()
    }

    /// The shard that holds `key` now, or None. Renames keep a key in the
    /// shard its placeholder hashed to, so this cannot be recomputed -- it is
    /// asked, routed shard first since most keys were never renamed.
    fn holding(&self, key: u64) -> Option<&Index> {
        let routed = self.route_to(key);
        if self.shards.len() == 1 || self.shards[routed].contains(key) {
            return Some(&self.shards[routed]);
        }
        (0..self.shards.len())
            .filter(|i| *i != routed)
            .map(|i| &self.shards[i])
            .find(|shard| shard.contains(key))
    }

    /// Total node count across all shards.
    fn size(&self) -> usize {
        self.shards.iter().map(|shard| shard.size()).sum()
    }

    fn add<T: VectorType>(&self, key: u64, vector: &[T]) -> Result<()> {
        let shard = self.route_to(key);
        self.shards[shard].add(key, vector).map_err(usearch_err)?;
        // Strictly after the add returns: a search that observes this has the
        // shard's entry point pointing at a node whose vector is written.
        self.populated[shard].store(true, Ordering::Release);
        Ok(())
    }

    fn get<T: VectorType>(&self, key: u64, buffer: &mut [T]) -> Result<usize> {
        match self.holding(key) {
            Some(shard) => shard.get(key, buffer).map_err(usearch_err),
            None => Ok(0),
        }
    }

    /// Whether any shard actually holds this key.
    ///
    /// Do not use `holding()` for this: with a single shard it returns that shard without
    /// checking, so it answers `Some` even for absent keys. That is fine for callers about to
    /// act on the key (a following `remove`/`rename` returns 0), but not for existence checks.
    fn contains(&self, key: u64) -> bool {
        self.shards.iter().any(|shard| shard.contains(key))
    }

    fn rename(&self, from: u64, to: u64) -> Result<usize> {
        match self.holding(from) {
            Some(shard) => shard.rename(from, to).map_err(usearch_err),
            None => Ok(0),
        }
    }

    fn remove(&self, key: u64) -> Result<usize> {
        match self.holding(key) {
            Some(shard) => shard.remove(key).map_err(usearch_err),
            None => Ok(0),
        }
    }

    /// Keys spread by hash, not by count, so a shard can run somewhat over an
    /// even split; an eighth on top covers that spread many times out.
    fn reserve_capacity_and_threads(&self, capacity: usize, threads: usize) -> Result<()> {
        let n = self.shards.len();
        let per_shard = capacity / n + capacity / (8 * n) + THREAD_SLOTS;
        for shard in &self.shards {
            shard
                .reserve_capacity_and_threads(per_shard, threads)
                .map_err(usearch_err)?;
        }
        Ok(())
    }

    fn search<T: VectorType>(&self, query: &[T], count: usize) -> Result<Matches> {
        self.merged(count, |shard, want| {
            shard.search(query, want).map_err(usearch_err)
        })
    }

    fn filtered_search<T: VectorType, F>(
        &self,
        query: &[T],
        count: usize,
        filter: F,
    ) -> Result<Matches>
    where
        F: Fn(u64) -> bool,
    {
        self.merged(count, |shard, want| {
            shard
                .filtered_search(query, want, &filter)
                .map_err(usearch_err)
        })
    }

    /// Queries every shard and returns the `count` nearest results by distance.
    ///
    /// **The count asked of each shard is capped at that shard's node count.** A search cannot
    /// return more than the nodes it has, so results are unchanged; without the cap, the
    /// requested number turns directly into an allocation -- usearch's Rust binding reserves
    /// and zero-fills a `count`-sized result buffer before searching (`rust/lib.cpp`), and that
    /// `reserve` aborts on failure rather than returning an error. An unbounded `<num>` could
    /// therefore take the daemon down with a single number. Capping by node count makes memory
    /// proportional to **the data held** rather than to the request, and since that data
    /// already occupies far more space in vectors, no separate limit is needed.
    ///
    /// The graph's internal buffers (`top`/`next`) shrink to `max(efs, want)` as well, but
    /// those already report failure as a value (`result.failed("Out of memory!")` in
    /// `index.hpp`), so they do not abort.
    fn merged(
        &self,
        count: usize,
        one: impl Fn(&Index, usize) -> Result<Matches>,
    ) -> Result<Matches> {
        let mut all: Vec<(u64, f32)> = Vec::new();
        for (shard, populated) in self.shards.iter().zip(&self.populated) {
            // A shard nothing has landed in yet is skipped, not asked: see
            // `populated`. An empty shard has nothing to contribute anyway.
            if !populated.load(Ordering::Acquire) {
                continue;
            }
            // A shard cannot return more than it holds. `size()` may still read 0 right
            // after `populated` is set, so always ask for at least one.
            let want = count.min(shard.size().max(1));

            // **Reserve before asking.** The space is needed anyway (`extend` aborts if it
            // cannot allocate), and a shard returns at most `want`, so reserving now costs
            // the same. The ordering is the point: the binding reserves the same amount on
            // entry, and its `reserve` aborts on failure (`rust/lib.cpp`), so learning first
            // that memory is short lets us back out with `SERVER_ERROR`. `probe_pair` sizes
            // the binding's allocation separately, so this checks the amount that will be
            // live at once. Another request can still allocate between the check and the
            // real allocation, so this is not a guarantee either.
            crate::fallible::reserve(&mut all, want)?;
            // Probe the binding's allocation up front too: it aborts on failure, so this is
            // the only place we can back out. It only catches **failures that surface at
            // allocation time** and cannot prevent OOM -- see `fallible::probe_pair`.
            crate::fallible::probe_pair::<u64, f32>(want)?;

            let matches = one(shard, want)?;
            all.extend(matches.keys.into_iter().zip(matches.distances));
        }
        // A stable sort aborts if it cannot allocate its scratch buffer. Order among equal
        // distances does not matter, so sort in place.
        all.sort_unstable_by(|a, b| a.1.total_cmp(&b.1));
        all.truncate(count);

        let mut keys = crate::fallible::vec(all.len())?;
        let mut distances = crate::fallible::vec(all.len())?;
        keys.extend(all.iter().map(|(key, _)| *key));
        distances.extend(all.iter().map(|(_, distance)| *distance));
        Ok(Matches { keys, distances })
    }
}
pub struct AnnIndex {
    pub layout: Layout,
    threads: usize,

    inner: RwLock<Shards>,
    reserved: AtomicUsize,

    elements: Arc<dyn Elements>,

    /// Locks this index when a failed enqueue has left the graph out of sync with the store.
    halt: Halt,

    #[cfg(test)]
    identity: usize,

    stuck: std::sync::Mutex<Vec<u64>>,
    pub(super) retirement: Retirement,
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
        let threads = threads.max(1);
        let index = Shards::new(&options)?;
        index.reserve_capacity_and_threads(MIN_CAPACITY, threads)?;

        Ok(Self {
            layout,
            threads,
            inner: RwLock::new(index),
            reserved: AtomicUsize::new(MIN_CAPACITY),
            elements,
            halt: Halt::new(),
            #[cfg(test)]
            identity: NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed),
            stuck: std::sync::Mutex::new(Vec::new()),
            retirement: Retirement::new()?,
        })
    }

    /// Resumes the index if the retirement backlog has drained sufficiently. Called by sweeper.
    ///
    /// Halting rejects all queries; resuming too early (e.g. on 1 freed slot) could immediately re-halt
    /// on the next delete, so wait until occupancy drops notably.
    pub fn resume_if_drained(&self) {
        if self.halt.is_halted() && self.retirement.drained_enough() {
            eprintln!("ArcVector: the retirement backlog has drained; taking reads again");
            self.halt.resume();
        }
    }

    /// Whether this index is currently halted. Checked at entry of all command handlers.
    pub fn is_halted(&self) -> bool {
        self.halt.is_halted()
    }

    /// Number of nodes in the graph, tracked directly by usearch.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .size()
    }

    /// Whether the graph contains the given address. Used by the unlink callback.
    pub fn holds(&self, addr: u64) -> bool {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(addr)
    }

    /// Returns true if all pending elements have been drained and released to the engine.
    pub fn is_drained(&self) -> bool {
        self.retirement.is_empty()
            && self
                .stuck
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
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
        index.reserve_capacity_and_threads(target, self.threads)?;
        self.reserved.store(target, Ordering::Release);
        Ok(())
    }

    /// Processes one item from the retirement queue. Returns `true` if work was done.
    ///
    /// Called only by the sweeper; must not be called while holding the graph lock
    /// because `release` acquires the engine's cache lock.
    pub fn drain_retired(&self) -> bool {
        matches!(
            self.retirement.drain_once(&*self.elements),
            Progress::Released(_)
        )
    }

    /// Installs a wakeup bell for the sweeper when registering the index.
    pub fn set_bell(&self) {
        self.retirement
            .set_bell(Arc::new(crate::runtime::sweep::wake));
    }

    /// Queues an address into the sweeper retirement list. Returns `false` if full.
    /// Must be called after removing from the graph so concurrent searches cannot reach it.
    pub fn retire_one(&self, addr: u64) -> bool {
        self.retirement.retire(&[addr])
    }

    /// Halts the index when the retirement queue overflows.
    /// Waits for in-flight dereferences to complete so the engine can safely free items.
    pub fn halt_for_overflow(&self) {
        eprintln!(
            "ArcVector: the retirement queue is full; halting this index until the \
             sweeper works the backlog down"
        );
        self.halt.halt();
    }

    /// Releases element reference immediately when not present in the graph.
    /// Called only on lane threads.
    pub fn release_now(&self, addr: u64) {
        self.elements.release(&[addr]);
    }

    /// Releases an element that was removed from the graph but failed to enqueue into retirement.
    /// Called only on lane threads.
    pub fn release_after_halt(&self, addr: u64) {
        self.halt_for_overflow();
        self.elements.release(&[addr]);
    }

    /// Inserts a node into the graph using its element address as the key.
    /// Called on worker pool threads outside the cache lock.
    pub fn link_node(&self, addr: u64, vector: &[u8]) -> Result<()> {
        self.check_vector(vector)?;
        self.ensure_capacity(self.len() + 1)?;
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        self.typed_add(&index, addr, vector)
    }

    /// Removes a node from the graph.
    /// Returns `true` if successfully removed. Returns `false` if `remove` failed (e.g. OOM),
    /// in which case the address is queued in `stuck` for sweeper retries.
    pub fn unlink_at(&self, addr: u64) -> bool {
        self.unlink_node(addr)
    }

    /// Moves a node from `old` to `new` address on element replacement.
    pub fn rename_node(&self, old: u64, new: u64) -> bool {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        match index.rename(old, new) {
            Ok(1) => true,
            Ok(0) => false,
            Ok(count) => {
                eprintln!("ArcVector: renaming a node moved {count} keys, not one");
                false
            }
            Err(e) => {
                eprintln!("ArcVector: could not rename a node: {e}");
                false
            }
        }
    }

    fn drop_node(&self, key: u64) -> bool {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        index.remove(key).is_ok()
    }

    fn unlink_node(&self, addr: u64) -> bool {
        if self.drop_node(addr) {
            return true;
        }
        let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
        if stuck.try_reserve(1).is_err() {
            eprintln!(
                "ArcVector: node {addr:#x} is still in the graph and cannot be queued for \
                 another attempt; its element stays held"
            );
            return false;
        }
        stuck.push(addr);
        false
    }

    pub fn retry_stuck(&self) {
        let pending = {
            let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
            if stuck.is_empty() {
                return;
            }
            std::mem::take(&mut *stuck)
        };
        // Re-add to stuck if unlinking fails. Addresses unlinked but not queued
        // will be retried on the next tick.
        let mut unqueued = pending;
        unqueued.retain(|&addr| self.unlink_node(addr) && !self.retire_one(addr));
        if unqueued.is_empty() {
            return;
        }
        let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
        if stuck.try_reserve(unqueued.len()).is_ok() {
            stuck.append(&mut unqueued);
        } else {
            eprintln!(
                "ArcVector: {} unlinked nodes could not be queued for release; their \
                 elements stay held",
                unqueued.len()
            );
        }
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

    fn typed_add(&self, index: &Shards, key: u64, vector: &[u8]) -> Result<()> {
        match self.layout.quant {
            Quant::F32 => index.add(key, &to_f32(vector)),
            Quant::F16 => index.add(key, f16::from_i16s(&to_i16(vector))),
            Quant::I8 => index.add(key, &to_i8(vector)),
            Quant::B1 => index.add(key, b1x8::from_u8s(vector)),
        }
    }

    pub fn vector_of(&self, addr: u64) -> Result<Option<Vec<u8>>> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let key = addr;
        let dim = self.layout.dim;

        let bytes = match self.layout.quant {
            Quant::F32 => {
                let mut out = vec![0f32; dim];
                if index.get(key, &mut out)? == 0 {
                    return Ok(None);
                }
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::F16 => {
                let mut out = vec![0i16; dim];
                if index.get(key, f16::from_mut_i16s(&mut out))? == 0 {
                    return Ok(None);
                }
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::I8 => {
                let mut out = vec![0i8; dim];
                if index.get(key, &mut out)? == 0 {
                    return Ok(None);
                }
                out.iter().map(|v| *v as u8).collect()
            }
            Quant::B1 => {
                let mut out = vec![0u8; dim];
                if index.get(key, b1x8::from_mut_u8s(&mut out))? == 0 {
                    return Ok(None);
                }
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

    /// Turns the traversal's hits into answerable results, dropping the ones
    /// that stopped being answerable while the search ran.
    ///
    /// A delete that lands mid-search is the case this exists for. The search
    /// may have put that address in its candidate list before the delete, and
    /// usearch will still offer it afterwards -- `remove` only flips a key
    /// under a mutex the traversal does not take. The graph's own set is the
    /// authority on what is still there, and asking it costs one lock and a
    /// hash lookup per hit, with no engine call.
    ///
    /// The address itself stays valid throughout: the reference the graph took
    /// is not handed back until every search that could hold it has finished.
    /// So this is about answering a stale hit, never about reading freed
    /// memory.
    fn resolve(&self, hits: &[(u64, f32)], epoch: usize) -> Result<Vec<(u64, Arc<str>, f32)>> {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut out = crate::fallible::vec(hits.len())?;
        out.extend(hits.iter().filter_map(|(key, distance)| {
            let _touching = self.halt.touch(epoch)?;
            if !index.contains(*key) {
                return None;
            }
            let id = self.elements.id_at(*key)?;
            Some((*key, id, *distance))
        }));
        Ok(out)
    }

    fn unfiltered(&self, index: &Shards, query: &[u8], k: usize) -> Result<Matches> {
        match self.layout.quant {
            Quant::F32 => index.search(&to_f32(query), k),
            Quant::F16 => index.search(f16::from_i16s(&to_i16(query)), k),
            Quant::I8 => index.search(&to_i8(query), k),
            Quant::B1 => index.search(b1x8::from_u8s(query), k),
        }
    }

    fn matches<P: Fn(u64) -> bool>(
        &self,
        index: &Shards,
        query: &[u8],
        k: usize,
        predicate: P,
    ) -> Result<Matches> {
        match self.layout.quant {
            Quant::F32 => index.filtered_search(&to_f32(query), k, predicate),
            Quant::F16 => index.filtered_search(f16::from_i16s(&to_i16(query)), k, predicate),
            Quant::I8 => index.filtered_search(&to_i8(query), k, predicate),
            Quant::B1 => index.filtered_search(b1x8::from_u8s(query), k, predicate),
        }
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

        // Epoch before traversal begins; only dereference under this epoch.
        let epoch = self.halt.epoch();
        let reading = self.retirement.enter();

        let matches = {
            let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            match accept {
                None => self.unfiltered(&index, query, k),
                Some(accept) => self.matches(&index, query, k, |key| {
                    let Some(_touching) = self.halt.touch(epoch) else {
                        return false;
                    };
                    accept(key)
                }),
            }?
        };

        let mut hits: Vec<(u64, f32)> = crate::fallible::vec(matches.keys.len())?;
        hits.extend(matches.keys.into_iter().zip(matches.distances));
        let answer = self.resolve(&hits, epoch)?;

        // If halted while running, return Busy rather than incomplete results.
        if self.halt.epoch() != epoch {
            return Err(Error::Busy);
        }

        // If this search was the last on a sealed barrier, drain retired elements immediately.
        if reading.leave() {
            self.drain_bounded();
        }
        Ok(answer)
    }

    /// Drains a bounded number of retired items without holding locks.
    fn drain_bounded(&self) {
        let mut budget = READER_DRAIN_BUDGET;
        while budget > 0 && self.drain_retired() {
            budget -= 1;
        }
        if budget == 0 {
            self.retirement.ring();
        }
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
    use crate::index::metric::Metric;
    use crate::index::quant::Quant;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
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

        fn unlink(&self, idx: &AnnIndex, id: &str) -> Option<u64> {
            self.by_id
                .write()
                .unwrap()
                .remove(&(map_of(idx), Arc::from(id)))
        }
    }

    fn map_of(idx: &AnnIndex) -> usize {
        idx.identity
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

    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        put(idx, id, coords)
    }

    fn put(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        let encoded = crate::index::quant::encode(coords, idx.layout.quant).unwrap();
        publish(idx, id, &encoded)
    }

    fn search(idx: &AnnIndex, coords: &[f32], k: usize) -> Vec<String> {
        let q = crate::index::quant::encode(coords, idx.layout.quant).unwrap();
        idx.search(&q, k, None)
            .unwrap()
            .into_iter()
            .map(|(_, id, _)| id.to_string())
            .collect()
    }

    fn publish(idx: &AnnIndex, id: &str, coords: &[u8]) -> u64 {
        let addr = FAKE.link(idx, id);
        idx.link_node(addr, coords).expect("graph accepts");
        addr
    }

    fn remove(idx: &AnnIndex, id: &str) -> bool {
        let Some(addr) = FAKE.unlink(idx, id) else {
            return false;
        };
        idx.unlink_at(addr);
        idx.retire_one(addr);
        true
    }

    fn distances(idx: &AnnIndex, coords: &[f32], k: usize) -> Vec<(String, f32)> {
        let q = crate::index::quant::encode(coords, idx.layout.quant).unwrap();
        idx.search(&q, k, None)
            .unwrap()
            .into_iter()
            .map(|(_, id, d)| (id.to_string(), d))
            .collect()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    // Wire scores match distances produced by usearch directly.

    #[test]
    fn cos_distance_is_one_minus_cos_and_zero_for_the_same_vector() {
        let idx = build(2, Quant::F32, Metric::Cos, 2);
        add(&idx, "same", &[1.0, 2.0]);
        add(&idx, "orth", &[2.0, -1.0]);
        add(&idx, "opp", &[-1.0, -2.0]);
        let got = distances(&idx, &[1.0, 2.0], 3);
        assert_eq!(got[0].0, "same");
        assert!(close(got[0].1, 0.0), "{got:?}");
        assert!(close(got[1].1, 1.0), "{got:?}");
        assert!(close(got[2].1, 2.0), "{got:?}");
    }

    #[test]
    fn l2_distance_is_the_squared_euclidean_distance() {
        let idx = build(2, Quant::F32, Metric::L2, 2);
        add(&idx, "o", &[0.0, 0.0]);
        let got = distances(&idx, &[3.0, 4.0], 1);
        assert!(close(got[0].1, 25.0), "{got:?}");
    }

    #[test]
    fn ip_distance_is_one_minus_the_dot_product() {
        let idx = build(2, Quant::F32, Metric::IP, 2);
        add(&idx, "x", &[1.0, 0.0]);
        add(&idx, "y", &[0.6, 0.8]);
        let got = distances(&idx, &[1.0, 0.0], 2);
        assert_eq!(got[0].0, "x");
        assert!(close(got[0].1, 0.0), "{got:?}");
        assert!(close(got[1].1, 1.0 - 0.6), "{got:?}");
    }

    #[test]
    fn hamming_distance_is_the_number_of_differing_bits() {
        let idx = build(8, Quant::B1, Metric::Hamming, 2);
        add(&idx, "a", &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        add(&idx, "b", &[1.0, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0]);
        let got = distances(&idx, &[1.0; 8], 2);
        assert_eq!(got[0].0, "a");
        assert!(close(got[0].1, 0.0), "{got:?}");
        assert!(close(got[1].1, 3.0), "{got:?}");
    }

    #[test]
    fn tanimoto_distance_is_one_minus_the_similarity() {
        // Active bits {0,1,2,3} and {0,1}: intersection 2, union 4 -> similarity 0.5.
        let idx = build(8, Quant::B1, Metric::Tanimoto, 2);
        add(&idx, "a", &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0]);
        add(&idx, "b", &[1.0, 1.0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0]);
        let got = distances(&idx, &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0], 2);
        assert!(close(got[0].1, 0.0), "{got:?}");
        assert!(close(got[1].1, 0.5), "{got:?}");
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
    fn a_halted_index_answers_nothing_even_mid_search() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5), vec!["a"]);

        // Halted due to full queue. The resolve guard prevents any hits from leaking.
        idx.halt_for_overflow();
        assert!(
            search(&idx, &[1.0, 0.0, 0.0, 0.0], 5).is_empty(),
            "halted index returns empty results even for in-flight searches"
        );
    }

    #[test]
    fn a_halted_index_comes_back_once_the_sweeper_has_drained_it() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        let mut fake = 0x1000_0000u64;
        while idx.retire_one(fake) {
            fake += 1;
        }
        idx.halt_for_overflow();
        assert!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5).is_empty(), "halted");

        idx.resume_if_drained();
        assert!(idx.is_halted(), "remains halted until drained");

        while idx.drain_retired() {}
        idx.resume_if_drained();

        assert!(!idx.is_halted(), "resumes once drained");
        assert_eq!(search(&idx, &[1.0, 0.0, 0.0, 0.0], 5), vec!["a"]);
    }

    #[test]
    fn linking_a_node_makes_it_findable_by_its_address() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let coords = crate::index::quant::encode(&[1.0, 0.0, 0.0, 0.0], idx.layout.quant).unwrap();
        let addr = FAKE.link(&idx, "a");

        idx.link_node(addr, &coords).expect("graph accepts");

        let index = idx.inner.read().unwrap();
        assert!(index.contains(addr), "address added as key");
    }

    #[test]
    fn unlinking_takes_the_node_out() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let coords = crate::index::quant::encode(&[1.0, 0.0, 0.0, 0.0], idx.layout.quant).unwrap();
        let addr = FAKE.link(&idx, "a");
        idx.link_node(addr, &coords).unwrap();

        idx.unlink_at(addr);
        assert!(
            !idx.inner.read().unwrap().contains(addr),
            "no longer in graph"
        );
    }

    #[test]
    fn renaming_moves_the_node_to_the_new_address() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let coords = crate::index::quant::encode(&[1.0, 0.0, 0.0, 0.0], idx.layout.quant).unwrap();
        let old = FAKE.link(&idx, "a");
        idx.link_node(old, &coords).unwrap();
        let new = FAKE.link(&idx, "a2");

        assert!(idx.rename_node(old, new));

        let index = idx.inner.read().unwrap();
        assert!(!index.contains(old), "old address removed");
        assert!(index.contains(new), "new address inserted");
    }

    #[test]
    fn renaming_an_address_the_graph_never_had_reports_it() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        assert!(
            !idx.rename_node(0xdead_beef, 0xfeed_face),
            "cannot rename non-existent address"
        );
    }

    #[test]
    fn the_last_search_out_of_a_sealed_barrier_drains_on_its_way_out() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let doomed = add(&idx, "goes", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "stays", &[0.0, 1.0, 0.0, 0.0]);

        // Concurrent delete arrives during traversal, sealing the barrier.
        let deleted = std::cell::Cell::new(false);
        let accept = |_key: u64| -> bool {
            if !deleted.replace(true) {
                remove(&idx, "goes");
            }
            true
        };

        let q = crate::index::quant::encode(&[0.0, 1.0, 0.0, 0.0], idx.layout.quant).unwrap();
        let answered = idx.search(&q, 5, Some(&accept)).unwrap();
        assert!(deleted.get(), "traversal called predicate");
        assert!(!answered.is_empty());

        assert!(
            FAKE.id_at(doomed).is_none(),
            "last exiting search should have drained the queue"
        );
    }

    #[test]
    fn a_running_search_holds_back_the_address_it_could_have_seen() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let addr = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        let reading = idx.retirement.enter();
        assert!(remove(&idx, "a"), "removed from graph");

        while idx.drain_retired() {}
        assert!(
            FAKE.id_at(addr).is_some(),
            "cannot release while search might observe address"
        );

        drop(reading);
        while idx.drain_retired() {}
        assert!(FAKE.id_at(addr).is_none(), "released once search finished");
    }

    #[test]
    fn a_search_that_starts_after_the_delete_does_not_hold_it_back() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let addr = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        assert!(remove(&idx, "a"));
        let late = idx.retirement.enter();

        while idx.drain_retired() {}
        assert!(
            FAKE.id_at(addr).is_none(),
            "searches started after delete do not hold it back"
        );
        drop(late);
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

        let query = crate::index::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32).unwrap();
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
             which is why the dereference holds the graph read lock and a touch guard"
        );
    }

    #[test]
    fn the_predicate_excludes_what_it_rejects() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        add(&idx, "keep", &[1.0, 0.0, 0.0, 0.0]);
        add(&idx, "skip", &[1.0, 0.0, 0.0, 0.0]);

        let q = crate::index::quant::encode(&[1.0, 0.0, 0.0, 0.0], Quant::F32).unwrap();

        let keep = |key: u64| FAKE.id_at(key).is_some_and(|id| &*id == "keep");
        let hits = idx.search(&q, 10, Some(&keep as Accept)).unwrap();
        let ids: Vec<String> = hits.iter().map(|(_, id, _)| id.to_string()).collect();
        assert_eq!(ids, vec!["keep"]);
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
    fn growth_past_the_initial_reservation_succeeds() {
        let idx = build(4, Quant::F32, Metric::L2, 4);
        for i in 0..1100 {
            add(&idx, &format!("v{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }
        assert_eq!(idx.len(), 1100);
        assert_eq!(search(&idx, &[1099.0, 0.0, 0.0, 0.0], 1), vec!["v1099"]);
    }

    #[test]
    fn concurrent_searchers_all_succeed() {
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
                let q = crate::index::quant::encode(
                    &[t as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
                    Quant::F32,
                )
                .unwrap();
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

    /// Verifies usearch builds and searches at the boundaries of allowed HNSW parameters
    /// (`element::M_RANGE` / `EF_RANGE`).
    #[test]
    fn usearch_builds_at_the_largest_allowed_hnsw_parameters() {
        use crate::engine::element::{EF_RANGE, M_RANGE};
        let options = ::usearch::IndexOptions {
            dimensions: 8,
            metric: Metric::Cos.kind(),
            quantization: ScalarKind::F32,
            connectivity: *M_RANGE.end(),
            expansion_add: *EF_RANGE.end(),
            expansion_search: *EF_RANGE.end(),
            multi: false,
        };
        let index = Index::new(&options).unwrap();
        index.reserve(16).unwrap();
        index.add(1, &[1.0; 8]).unwrap();
        index
            .add(2, &[2.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0])
            .unwrap();
        assert_eq!(index.search(&[1.0; 8], 2).unwrap().keys.len(), 2);
    }

    #[test]
    #[ignore]
    fn scaling_probe_raw_usearch() {
        const DIM: usize = 1024;
        const PER_THREAD: usize = 2_000;

        fn run(threads: usize) -> f64 {
            let options = ::usearch::IndexOptions {
                dimensions: DIM,
                metric: Metric::Cos.kind(),
                quantization: ScalarKind::F32,
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let index = Index::new(&options).unwrap();
            index
                .reserve_capacity_and_threads(threads * PER_THREAD + 16, THREAD_SLOTS)
                .unwrap();
            let start = std::time::Instant::now();
            std::thread::scope(|scope| {
                for t in 0..threads {
                    let index = &index;
                    scope.spawn(move || {
                        for i in 0..PER_THREAD {
                            let coords: Vec<f32> = (0..DIM)
                                .map(|d| ((t * 7919 + i * 31 + d) % 997) as f32)
                                .collect();
                            index.add((t * PER_THREAD + i) as u64, &coords).unwrap();
                        }
                    });
                }
            });
            (threads * PER_THREAD) as f64 / start.elapsed().as_secs_f64()
        }

        let one = run(1);
        let four = run(4);
        println!(
            "raw usearch    1 thread: {one:8.0}/s   4 threads: {four:8.0}/s   scaling x{:.2}",
            four / one
        );
    }

    /// Raw usearch, adds racing searches on one small graph -- the shape one
    /// shard sees. If this crashes, the null-vector segfault is usearch's own
    /// add/search race and not this crate's bookkeeping.
    #[test]
    #[ignore]
    fn usearch_add_search_race() {
        for round in 0..200 {
            let options = ::usearch::IndexOptions {
                dimensions: 4,
                metric: Metric::L2.kind(),
                quantization: ScalarKind::F32,
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let index = Index::new(&options).unwrap();
            index.reserve_capacity_and_threads(4096, 16).unwrap();
            std::thread::scope(|scope| {
                for t in 0..4u64 {
                    let index = &index;
                    scope.spawn(move || {
                        for i in 0..200u64 {
                            index
                                .add(t * 1000 + i, &[t as f32, i as f32, 0.0, 0.0])
                                .unwrap();
                        }
                    });
                }
                for _ in 0..4 {
                    let index = &index;
                    scope.spawn(move || {
                        for i in 0..200u64 {
                            let _ = index.search(&[i as f32, 1.0, 0.0, 0.0], 3).unwrap();
                        }
                    });
                }
            });
            if round % 50 == 0 {
                println!("round {round}");
            }
        }
        println!("no crash in 200 rounds");
    }

    /// What sharding costs in memory: the same vectors in one raw usearch
    /// graph, then in Shards, with both allocators reported side by side.
    #[test]
    fn searches_racing_the_first_add_do_not_crash() {
        for _ in 0..200 {
            let idx = Arc::new(build(4, Quant::F32, Metric::L2, 16));
            let mut handles = Vec::new();

            for t in 0..2u64 {
                let idx = Arc::clone(&idx);
                handles.push(std::thread::spawn(move || {
                    for i in 0..50 {
                        add(&idx, &format!("k{t}-{i}"), &[t as f32, i as f32, 0.0, 0.0]);
                    }
                }));
            }
            for _ in 0..4 {
                let idx = Arc::clone(&idx);
                handles.push(std::thread::spawn(move || {
                    for i in 0..50 {
                        let q = crate::index::quant::encode(&[i as f32, 0.0, 0.0, 0.0], Quant::F32)
                            .unwrap();
                        idx.search(&q, 3, None).unwrap();
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        }
    }

    /// Adds and searches overlapping under a load that never lets up: a bulk
    /// insert running flat out must not hold queries up, and the two must not
    /// crash when they overlap. Nothing serialises them any more, so this is
    /// the soak that says so.
    #[test]
    fn a_bulk_load_does_not_starve_searches() {
        let idx = Arc::new(build(4, Quant::F32, Metric::L2, 16));
        for i in 0..200 {
            add(&idx, &format!("seed{i}"), &[i as f32, 0.0, 0.0, 0.0]);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let searched = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for t in 0..4 {
            let idx = Arc::clone(&idx);
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut i = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    add(
                        &idx,
                        &format!("bulk{t}-{i}"),
                        &[t as f32, i as f32, 1.0, 0.0],
                    );
                    i += 1;
                }
            }));
        }

        for _ in 0..2 {
            let idx = Arc::clone(&idx);
            let searched = Arc::clone(&searched);
            handles.push(std::thread::spawn(move || {
                for i in 0..300 {
                    let q = crate::index::quant::encode(&[i as f32, 0.0, 0.0, 0.0], Quant::F32)
                        .unwrap();
                    idx.search(&q, 5, None).unwrap();
                    searched.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // the searchers finish on their own; the adders run until told
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while searched.load(Ordering::Relaxed) < 600 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            searched.load(Ordering::Relaxed),
            600,
            "searches did not get through a bulk load"
        );
    }

    /// What sharding costs a search: one raw graph against
    /// Shards, single-threaded latency and four-thread throughput.
    #[test]
    #[ignore]
    fn search_cost() {
        const DIM: usize = 1024;
        const N: usize = 60_000;
        const K: usize = 100;
        const QUERIES: usize = 400;

        fn vector(i: usize) -> Vec<f32> {
            (0..DIM).map(|d| ((i * 31 + d) % 997) as f32).collect()
        }
        fn query(i: usize) -> Vec<f32> {
            (0..DIM).map(|d| ((i * 71 + d * 3) % 997) as f32).collect()
        }

        let options = ::usearch::IndexOptions {
            dimensions: DIM,
            metric: Metric::Cos.kind(),
            quantization: ScalarKind::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            multi: false,
        };

        let one = Index::new(&options).unwrap();
        one.reserve_capacity_and_threads(N + THREAD_SLOTS, THREAD_SLOTS)
            .unwrap();
        for i in 0..N {
            one.add(i as u64, &vector(i)).unwrap();
        }
        let many = Shards::new(&options).unwrap();
        many.reserve_capacity_and_threads(N, THREAD_SLOTS).unwrap();
        for i in 0..N {
            many.add(i as u64, &vector(i)).unwrap();
        }

        let run = |threads: usize, sharded: bool| -> f64 {
            let start = std::time::Instant::now();
            std::thread::scope(|scope| {
                for t in 0..threads {
                    let one = &one;
                    let many = &many;
                    scope.spawn(move || {
                        for i in 0..QUERIES {
                            let q = query(t * QUERIES + i);
                            if sharded {
                                many.search(&q, K).unwrap();
                            } else {
                                one.search(&q, K).unwrap();
                            }
                        }
                    });
                }
            });
            (threads * QUERIES) as f64 / start.elapsed().as_secs_f64()
        };

        for (label, sharded) in [("one graph", false), ("sharded ", true)] {
            let s1 = run(1, sharded);
            let s4 = run(4, sharded);
            println!(
                "{label}   1 thread: {s1:8.0} q/s ({:6.2} ms)   4 threads: {s4:8.0} q/s   scaling x{:.2}",
                1000.0 / s1,
                s4 / s1
            );
        }
    }

    /// Four threads, each with an index of its own -- no shared state at all.
    /// What this measures is the machine: if even fully independent indexes
    /// stop at x1.5, the ceiling is memory bandwidth / cores, not locks.
    #[test]
    #[ignore]
    fn scaling_probe_independent_indexes() {
        const DIM: usize = 1024;
        const PER_THREAD: usize = 2_000;

        fn make() -> Index {
            let options = ::usearch::IndexOptions {
                dimensions: DIM,
                metric: Metric::Cos.kind(),
                quantization: ScalarKind::F32,
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let index = Index::new(&options).unwrap();
            index
                .reserve_capacity_and_threads(PER_THREAD + 16, 4)
                .unwrap();
            index
        }

        // one thread, one index: the baseline
        let index = make();
        let start = std::time::Instant::now();
        for i in 0..PER_THREAD {
            let coords: Vec<f32> = (0..DIM).map(|d| ((i * 31 + d) % 997) as f32).collect();
            index.add(i as u64, &coords).unwrap();
        }
        let one = PER_THREAD as f64 / start.elapsed().as_secs_f64();

        // four threads, four indexes: nothing shared
        let indexes: Vec<Index> = (0..4).map(|_| make()).collect();
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for (t, index) in indexes.iter().enumerate() {
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        let coords: Vec<f32> = (0..DIM)
                            .map(|d| ((t * 7919 + i * 31 + d) % 997) as f32)
                            .collect();
                        index.add(i as u64, &coords).unwrap();
                    }
                });
            }
        });
        let four = (4 * PER_THREAD) as f64 / start.elapsed().as_secs_f64();
        println!(
            "independent    1 thread: {one:8.0}/s   4 threads: {four:8.0}/s   scaling x{:.2}",
            four / one
        );
    }
}
