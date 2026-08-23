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

use super::held::{self, Elements, HeldSet};
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

impl Drop for AnnIndex {
    fn drop(&mut self) {
        // Every element this graph pinned goes back. Nothing else will do it: the entry that
        // owned this index is already out of the registry by the time the last `Arc` falls, so
        // there is no caller left to ask. Skipped without it, an expired index would keep its
        // elements allocated for the life of the process while the engine's accounting says
        // they are free.
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

pub struct AnnIndex {
    pub layout: Layout,
    pub metric: Metric,
    threads: usize,
    /// Write-locked only to grow capacity; every other operation reads.
    inner: RwLock<Index>,
    reserved: AtomicUsize,
    /// The elements this graph holds a refcount on, addressed by the usearch key that *is*
    /// their address. See [`HeldSet`].
    held: RwLock<HeldSet>,
    /// Bumped by `clear`. A search that captured keys before a wipe must not dereference them
    /// afterwards — the holds are on their way out and the addresses can be handed to different
    /// elements. Same idea as `Staged`'s owner token, for readers.
    epoch: AtomicU64,
    /// Serial for staged nodes' placeholder keys. Never reused, never an address.
    next_placeholder: AtomicU64,
    /// The store the graph's keys are addresses into. Held for the index's lifetime because
    /// [`Drop`] needs it: every refcount this graph took has to go back when it does.
    elements: Arc<dyn Elements>,
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

    /// Only the tests vary the context count; every caller gets `THREAD_SLOTS`.
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

    /// How many vectors are live. Not the number ever added.
    pub fn len(&self) -> usize {
        self.held().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn held(&self) -> std::sync::RwLockReadGuard<'_, HeldSet> {
        self.held.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Module memory the held-address set costs, which arcus cannot see.
    pub fn addr_set_bytes(&self) -> usize {
        self.held().bytes()
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
        // A placeholder, not the element's address. The address is known already — the caller
        // allocated the element — but a node keyed by it would be *nameable*: the field bytes
        // are in that allocation, so a search could dereference them and answer with an id
        // whose element is not linked yet, for a write that can still fail. The tag keeps the
        // node invisible until `insert_published` renames it, which is after the link.
        let key = held::STAGED_TAG | self.next_placeholder.fetch_add(1, Ordering::Relaxed);

        // Counted in before the node exists, so the count is never behind the graph.
        let staged = Staged::new(key, owner, &self.in_flight);

        // A failure needs no undo: the key named nothing, so nothing named the node, and
        // `staged` drops on the way out and takes its count with it.
        self.typed_add(&index, key, vector)?;
        Ok(staged)
    }

    /// The two registrations of an insert, under one hold on the held set.
    ///
    /// This is the only place either store becomes visible, and the element write runs *inside*
    /// the hold on purpose: a lookup dereferences under the same lock, so it sees the state
    /// before the graph was touched or after the element was linked, never between. Without
    /// that, a plain `vsim` — which reads no element at all — would answer with an id whose
    /// element is not in the Map yet, for a write that can still fail.
    ///
    /// Holding it across the engine call costs less than it looks: `map_elem_insert` takes the
    /// engine's cache lock, so the writes queueing here were queueing anyway. It also orders
    /// same-id writes by itself, which is what a per-id lock used to do.
    ///
    /// `displaced` finds the element this write replaces — the graph cannot say, because it
    /// knows an outgoing element only by its address, and only the Map can name one. It runs
    /// **inside the hold**, which is what serializes it against the link: read outside, two
    /// writes to one id would both see the same outgoing address, and only the first would
    /// retire it — leaving the second's predecessor held forever, with a node still answering.
    ///
    /// Whatever it returns comes with a refcount of its own, and this call hands that back.
    ///
    /// A takeover since the stage emptied the graph, so the node is gone and must not be
    /// named. The element is still written: a rebuild reads the Map, so the write survives
    /// there and the refill indexes it.
    pub fn insert_published<E>(
        &self,
        staged: Staged<'_>,
        owner: u64,
        displaced: impl FnOnce() -> Option<u64>,
        link: impl FnOnce() -> std::result::Result<u64, E>,
    ) -> std::result::Result<(), PublishError<E>> {
        // `inner` first and `held` second, everywhere. `clear` takes `inner`'s write lock and
        // then this one, so a call that took them the other way round — and this one needs both,
        // because naming a published node is a `rename` on the graph — would deadlock against it.
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        let live = staged.owner == owner;

        // The room comes before the element write it gates. A std collection that cannot grow
        // aborts instead of returning, so the growth has to be asked for where the answer can
        // still be a reply. Nothing is written yet, so the only thing to unwind is this call's
        // own node.
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
                // The element was never written, so nothing left this node and the graph has to
                // read as it did before. The node still carries its placeholder, so nothing
                // could have named it in the meantime.
                if let Some(old) = displaced {
                    self.elements.release(&[old]);
                }
                drop(held);
                drop(index);
                self.drop_node(key);
                return Err(PublishError::Store(e));
            }
        };

        // A takeover since the stage emptied the graph, so this node is gone and must not be
        // named. The element is still written — a rebuild reads the Map, so the write survives
        // there — but the refcount `link` took is nobody's to keep.
        if !live {
            let mut back = vec![addr];
            back.extend(displaced);
            self.elements.release(&back);
            drop(held);
            drop(index);
            self.drop_node(key);
            return Ok(());
        }

        // The link happened, so the address is an element in the Map and the node may answer
        // under it. `rename` moves the key without touching the graph — the address is fresh,
        // so nothing can already hold it.
        let renamed = index.rename(key, addr);
        if let Err(e) = renamed {
            // The element is written and the node cannot be named for it. Leave the Map alone —
            // a rebuild indexes it — and take the node out rather than leave one answering
            // under a placeholder.
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

        // The element this write replaced is now unreachable. Two refcounts may stand on it —
        // the one `displaced` took to name it, and the graph's, if the graph had it — and both
        // go back before the lock does, which is what keeps a lookup from reading an address on
        // its way out. See `HeldSet::take`.
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

    /// Move a node from the element it stood on to the one that replaced it.
    ///
    /// For `vsetattr`, which rewrites a value without touching the vector. The write replaces
    /// the element rather than editing it: the engine edits in place only when nothing holds a
    /// refcount, and the graph holds one on every element it keys a node by — so that path is
    /// out of reach whenever it matters, and `map_elem_update` would not report the new address
    /// anyway. A replace hands the engine an allocation of ours, so the address is known before
    /// the call and there is one code path instead of two.
    ///
    /// What the graph does is a `rename`: a hash entry, not a graph insert. That is why this is
    /// not a `vadd` — no HNSW work, no staging, no capacity.
    ///
    /// Both closures run inside the hold, which serializes this against writes to the same id.
    /// `locate` reports where the element is now and what it holds; `write` puts the new value
    /// in and reports where it landed, with a refcount the graph takes over.
    ///
    /// The count is raised for the whole call, as a delete's is: a search that read the old key
    /// before the move resolves after it, finds that address unheld, and drops the row.
    pub fn update_published<E>(
        &self,
        locate: impl FnOnce() -> Option<(u64, Vec<u8>)>,
        write: impl FnOnce(Vec<u8>) -> std::result::Result<u64, E>,
    ) -> std::result::Result<Option<()>, PublishError<E>> {
        let _slack = InFlight::new(&self.in_flight);
        // `inner` then `held`, as everywhere.
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);

        let Some((old, value)) = locate() else {
            return Ok(None);
        };
        // Room for the address that comes back, taken before anything is written.
        if let Err(e) = held.reserve() {
            return Err(PublishError::Mapping(Error::Index(format!(
                "the held set could not grow: {e}"
            ))));
        }

        // Nothing is undone if this fails: the element it would have replaced is still linked
        // and the graph still holds it.
        let new = write(value).map_err(PublishError::Store)?;

        if !held.take(old) {
            // No node stands on it — a rebuild has not reached this element. The refcount the
            // write took is nobody's.
            self.elements.release(&[new]);
            return Ok(Some(()));
        }
        // The element the node stood on is unlinked now, and the graph's refcount on it goes
        // back before the lock does.
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

    /// Drop the node at `addr` without touching the Map.
    ///
    /// For an element the engine describes in a way that cannot be read. The Map is left alone —
    /// deleting on a description we do not trust would be acting on the same bad reading — but
    /// the graph must stop offering it, or every search returns a row that cannot render.
    ///
    /// Reports whether this graph held it.
    pub fn forget_unreadable(&self, addr: u64) -> bool {
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        let was_held = if self.rebuilding.load(Ordering::Acquire) {
            // A refill must not put it back. A refusal to make room is not worth failing over
            // here: the worst of it is that the refill re-adds an element nobody can read.
            let _ = held.reserve_tombstone();
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

    /// Throw a staged node away. The mapping never named it, so there is nothing to restore.
    pub fn discard(&self, staged: Staged<'_>) {
        self.drop_node(staged.key);
    }

    /// Detach a node from the graph. The refcount is the caller's to hand back.
    ///
    /// A failure to remove leaks a node nothing holds, which every lookup already drops, so
    /// there is nothing for a caller to do about it.
    fn drop_node(&self, key: u64) {
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let _ = index.remove(key);
    }

    /// Add the element at `addr` unless a rebuild has a reason to leave it alone.
    ///
    /// `vector` runs inside the hold: the refill's decision and its claim on the address are
    /// one step, so a live write and a replay of the same element cannot interleave. The
    /// refcount on the snapshot element belongs to the caller until this returns `true`, at
    /// which point the graph has taken it over.
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

    /// The stored vector for the element at `addr`, read back out of the graph.
    ///
    /// usearch keeps the vectors it was given, in the quantization the index was built with —
    /// the same bytes the Map used to carry. That copy is why a build without recovery does not
    /// store a second one, and this is what `vsim KEY` reads its query from.
    ///
    /// `None` means the graph has no node for that element.
    pub fn vector_of(&self, addr: u64) -> Result<Option<Vec<u8>>> {
        // `inner` before `held`, as everywhere — see `insert_published`.
        let index = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        if !self.held().contains(addr) {
            return Ok(None);
        }
        let key = addr;
        let dim = self.layout.dim;
        // Read in the index's own scalar type, so nothing is converted and the bytes come back
        // exactly as `Quant::encode` would have written them.
        let bytes = match self.layout.quant {
            Quant::F32 => {
                let mut out = vec![0f32; dim];
                index.get(key, &mut out).map_err(usearch_err)?;
                out.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            Quant::F16 => {
                // `f16` is a transparent newtype over `i16`, so the buffer is the plain one.
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
                // Transparent over `u8`, and the packed bytes are the stored form — but the
                // binding measures this buffer against `dimensions()`, which for a binary index
                // counts bits, and refuses anything that is not a multiple of it. So ask for
                // `dim` bytes, which is always at least the `dim / 8` it writes, and keep that
                // many. `add` has no such check, which is why only the read side pays it.
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

    /// Throw away every member, keeping the index's shape.
    pub fn begin_rebuild(&self) -> Result<()> {
        // Recording deletes starts before the graph is emptied, so a racing delete is still remembered.
        self.rebuilding.store(true, Ordering::Release);
        self.clear_with()
    }

    /// Stop recording deletes and drop what was recorded.
    pub fn end_rebuild(&self) {
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        self.rebuilding.store(false, Ordering::Release);
        held.forget_tombstones();
    }

    /// Throw away every member and hand back every refcount.
    pub fn clear_with(&self) -> Result<()> {
        let index = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);
        index.reset().map_err(usearch_err)?;
        // `reset` deallocates the thread contexts along with the members, so put the floor
        // back under both: an `AnnIndex` is never without a context for a search to take.
        index
            .reserve_capacity_and_threads(MIN_CAPACITY, self.threads)
            .map_err(usearch_err)?;
        self.reserved.store(MIN_CAPACITY, Ordering::Release);
        // Anything staged against the graph this emptied must not be named afterwards. The
        // caller changes the index's owner before getting here, which is what says so.
        //
        // Every refcount goes back before the lock does. Those addresses can then be handed to
        // different elements, and a search that captured one before this point would read the
        // wrong id from it — the epoch is what makes it discard them instead.
        let outgoing = held.take_all();
        if !outgoing.is_empty() {
            self.elements.release(&outgoing);
        }
        self.epoch.fetch_add(1, Ordering::Release);
        drop(held);
        Ok(())
    }

    /// The two removals of a delete, under one hold on the held set.
    ///
    /// `take` unlinks the element and reports **where it was** — the engine first, always,
    /// because that is the copy replicas and the persistence log follow, and because the
    /// address it hands back is the graph's key for that element. Under the same hold as the
    /// graph removal, a lookup cannot see the element gone while the node still answers for it,
    /// nor the reverse.
    ///
    /// The count is raised for the whole call: a search that read this key just before the hold
    /// resolves after it, finds nothing holding the address, and drops the row.
    ///
    /// `None` means there was no element. `Some(had_node)` also says whether the graph had one
    /// — a rebuild that has not reached this element yet leaves it out.
    pub fn remove_published<E>(
        &self,
        take: impl FnOnce() -> std::result::Result<Option<u64>, E>,
    ) -> std::result::Result<Option<bool>, PublishError<E>> {
        let _slack = InFlight::new(&self.in_flight);
        let mut held = self.held.write().unwrap_or_else(PoisonError::into_inner);

        // While rebuilding, a deleted element is remembered so the refill cannot replay it —
        // and the room for that has to be taken *before* the element goes. Once the engine has
        // deleted it there is no failure left to report: the delete is already on its way to
        // the replicas, and refusing here would answer for a state that no longer exists.
        let tombstone = self.rebuilding.load(Ordering::Acquire);
        if tombstone && let Err(e) = held.reserve_tombstone() {
            return Err(PublishError::Mapping(Error::Index(format!(
                "the held set could not grow: {e}"
            ))));
        }

        let Some(addr) = take().map_err(PublishError::Store)? else {
            return Ok(None);
        };

        // `take` left the caller holding a refcount of its own on the unlinked element, so the
        // address stays valid whether or not the graph had it.
        let had_node = if tombstone {
            held.take_tombstoned(addr)
        } else {
            held.take(addr)
        };
        if had_node {
            self.elements.release(&[addr]);
        }
        drop(held);

        // After the hold, and unchecked: it leaves a node nothing holds, which every lookup
        // already drops.
        if had_node {
            self.drop_node(addr);
        }
        Ok(Some(had_node))
    }

    /// Translate search hits under one hold, so no write's two steps are seen half-done.
    ///
    /// Keys the mapping does not name are dropped: a node staged by a `vadd` that has not
    /// reached [`Self::insert_published`] yet — it still carries its placeholder tag — or one
    /// whose element a `vdel` just took, which the held set no longer has.
    ///
    /// **The read lock is what makes the dereference safe.** A release takes the write lock, so
    /// an address checked here cannot be handed back before it is read.
    ///
    /// `entered` is the epoch the search started in. A `clear` between then and now handed every
    /// address back, and the allocator may have given one to a different element since — these
    /// keys describe a graph that no longer exists.
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

    /// The id a key stands for, or `None` if nothing can answer for it.
    ///
    /// The caller holds the read lock, which is the safety condition for the dereference.
    fn name(&self, held: &HeldSet, key: u64) -> Option<Arc<str>> {
        if held::is_staged(key) || !held.contains(key) {
            return None;
        }
        self.elements.id_at(key)
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
    /// Capturing the keys and turning them into ids is one call on purpose. The second half
    /// dereferences element addresses, and only this side knows the lock that makes that safe.
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
                Some(matches) => {
                    let held = self.held();
                    self.matches(&index, query, k, |key| match self.name(&held, key) {
                        Some(id) => matches(key, &id),
                        None => false,
                    })
                }
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

    /// Stands in for the Map: hands out addresses, remembers what each one is named, and
    /// forgets an address when its refcount comes back.
    ///
    /// Addresses are never reused, which is the one way it is kinder than a slab — the tests
    /// that care about reuse are the ones that check an address is released, not that the next
    /// allocation collides with it.
    ///
    /// Keyed by index as well as id, because the tests share one of these and run in parallel:
    /// two of them writing `"a"` are two different Maps, as they would be in a server.
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
            // Away from zero and from the staged tag, like a real heap address.
            next: AtomicU64::new(0x7f00_0000_0000),
            released: AtomicUsize::new(0),
        })
    });

    impl FakeStore {
        /// Link a new element for `id`, returning where it lives.
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

        /// Unlink `id`'s element, keeping the address readable — the engine does the same
        /// while a refcount stands.
        fn unlink(&self, idx: &AnnIndex, id: &str) -> Option<u64> {
            self.by_id
                .write()
                .unwrap()
                .remove(&(map_of(idx), Arc::from(id)))
        }

        /// Every id this index's Map still holds.
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

    /// Which Map an index's elements belong to. The index's own address does as well as
    /// anything: it outlives every element the test gives it.
    fn map_of(idx: &AnnIndex) -> usize {
        std::ptr::from_ref(idx) as usize
    }

    impl Elements for FakeStore {
        fn id_at(&self, addr: u64) -> Option<Arc<str>> {
            self.by_addr.read().unwrap().get(&addr).cloned()
        }

        /// A released address is one the engine may hand to a different element, so it stops
        /// answering here. A dereference after this is the bug the refcount exists to prevent.
        fn release(&self, addrs: &[u64]) {
            let mut by_addr = self.by_addr.write().unwrap();
            for addr in addrs {
                by_addr.remove(addr);
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The production insert with a stubbed element write: the graph writes, the ordering and
    /// the lock are all real, only the engine call is absent.
    fn publish(idx: &AnnIndex, id: &str, staged: Staged<'_>, owner: u64) -> u64 {
        // One cell, because the address is chosen inside the hold — the same place production
        // reads it — and the test still needs it afterwards.
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

    /// Likewise for the delete, with the element already known to be there.
    fn remove(idx: &AnnIndex, id: &str) -> bool {
        let addr = FAKE.unlink(idx, id);
        let removed: std::result::Result<Option<bool>, PublishError<()>> =
            idx.remove_published(|| Ok(addr));
        let Ok(removed) = removed else {
            panic!("remove failed");
        };
        removed.unwrap_or(false)
    }

    /// The claim a build without recovery rests on: usearch gives back exactly the bytes the
    /// Map used to store, for every quantization, so `vsim KEY` needs no second copy.
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
            // Distinct, non-zero bytes, so a wrong length or a swapped pair would show.
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

    /// Leave usearch with no thread contexts, which is what an over-subscribed worker pool
    /// looks like from the inside: the next entry fails instead of waiting. `reserved` is
    /// left alone on purpose, so `ensure_capacity` does not quietly undo it.
    fn exhaust_contexts(idx: &AnnIndex) {
        idx.inner.write().unwrap().reset().unwrap();
    }

    /// Every held address must be a node the graph actually holds, and must still name
    /// something — a released address answers nothing.
    ///
    /// Only checkable at rest. `clear` takes `inner`'s write lock and `publish` takes the held
    /// set's, so they do not exclude each other. Correct code violates this *during* a rebuild
    /// and satisfies it once the writers stop.
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

    /// No two addresses may name the same id. A second one is a second hit for that id in
    /// every search, and means an overwrite failed to retire what it displaced.
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

    /// Stands in for `VectorIndex`'s token: the same value means no takeover happened.
    const OWNER: u64 = 7;

    /// A settled write, returning the address the graph now keys this id by.
    fn add(idx: &AnnIndex, id: &str, coords: &[f32]) -> u64 {
        put(idx, id, coords)
    }

    /// A write, whether or not the id is already there — `publish` finds what it displaces.
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
        // An overwrite links a new element, so the node moves to that address and the old one
        // is retired — the graph must not be left holding both.
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
        // `len` feeds vadd's maxcount check, so a phantom entry can reject a real one.
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

        assert!(idx.held().contains(key), "the graph never moved");
        assert_eq!(FAKE.id_at(key).as_deref(), Some("a"));
        assert_eq!(idx.len(), 1);
        assert_eq!(search(&idx, &[1.0, 0.0], 5), vec!["a"]);
    }

    #[test]
    fn a_takeover_inside_the_staging_window_voids_the_stage() {
        // `take_over` sets the index's owner to REBUILDING and then empties the graph, so a
        // publish carrying the older token is naming a node that is no longer there. The wipe
        // also hands every address back, so the owner token is what stops a stale publish from
        // keying a node by an element the engine may have given to somebody else.
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

        idx.clear_with().unwrap();
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

    #[test]
    fn concurrent_writers_deleters_and_searchers_keep_the_mapping_consistent() {
        // Every writer works the same small pool of ids, so publishes, discards and deletes
        // land on each other rather than running side by side. The element write and the
        // element delete happen inside the published calls, which is where the engine's happen
        // — a test that touched `elements` outside them would be racing on its own.
        const IDS: usize = 24;
        let dim = 8;
        let idx = Arc::new(build(dim, Quant::F32, Metric::L2, 16));
        for i in 0..IDS {
            let id = format!("id{i}");
            // `add` links the element through the store, so the Map records it too.
            add(&idx, &id, &[i as f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
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
                        // The Map is read and written inside the call, which is what the
                        // engine does: both live under the same hold, and a test that touched
                        // the store outside it would be racing on its own.
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

        // Deleters: the vdel that lands inside somebody's staging window.
        for t in 4..6 {
            let idx = Arc::clone(&idx);
            handles.push(std::thread::spawn(move || {
                for i in 0..250 {
                    let id = format!("id{}", choice(t, i, IDS));
                    // `vdel` unlinks the element and learns its address in one engine call,
                    // inside the same hold that takes the node out.
                    let removed: std::result::Result<Option<bool>, PublishError<()>> =
                        idx.remove_published(|| Ok(FAKE.unlink(&idx, &id)));
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
        // And it stays that way at rest: nothing a discard threw away is held.
        for key in discarded.lock().unwrap().iter() {
            assert!(!idx.held().contains(*key), "discarded key {key} is held");
        }
        // Every element the Map still holds has a node, and nothing else does.
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
        // `clear` empties the graph under writers that already staged against it. The owner
        // check is what keeps those publishes from naming nodes that are gone.
        const IDS: usize = 16;
        let dim = 8;
        let idx = Arc::new(build(dim, Quant::F32, Metric::L2, 16));
        for i in 0..IDS {
            let id = format!("id{i}");
            // `add` links the element through the store, so the Map records it too.
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
                    // A stage can fail once `clear` has dropped the reservation to the floor.
                    if let Ok(staged) = idx.stage(&v, OWNER) {
                        // Both Map accesses inside the call, as the engine does them.
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
    fn concurrent_adds_and_searches_keep_every_address_named() {
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
                    // The published key is the element's address, not the placeholder the
                    // stage carried — `publish` renames it once the element is linked.
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

    /// A wipe hands every address back, and the engine may give one to a different element.
    /// The epoch is what keeps a search from dereferencing keys it captured before that.
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

    /// What `vsetattr` rests on: the value is replaced, so the node has to follow the element —
    /// and following it is a rename, not a graph insert.
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

    /// A write that refuses changes nothing: the element it would have replaced is still linked
    /// and the graph still holds it.
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

    /// The tag is what makes a staged node unreadable, not the held set — its address is a
    /// real element the caller allocated, and its field bytes are already written.
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
