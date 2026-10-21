//! The callback through which the engine reports the lifecycle of index Maps and their elements.
//!
//! Five cases arrive through one callback. `event_data_t` says which case it is, carries the
//! related pointers (`target`, `source`), and has slots for how many references we take
//! (`target_ref`, `source_ref`). Element events also carry the Map item the element belongs to
//! (`coll_item`) -- an element does not record its Map, so this is the only way to tell which
//! index it belongs to. The Map's key is the index name.
//!
//! An index comes into being when its Map is linked (`ITEM_LINK`). The config lives in the Map
//! item's value and the callback reads it on the spot -- the same path whether the Map came from
//! this node's `vop create`, replication, or persistence replay. Elements are always inserted
//! into a linked Map, so by the time an element event arrives its index is already in the
//! registry. If it is not, it is not our index.
//!
//! After the callback returns, the engine moves as many references as we wrote.
//!
//! | | Engine does | We write |
//! |---|---|---|
//! | ELEM_INSERT | `target += target_ref` | 1 if held, 0 if not |
//! | ELEM_DELETE | `target -= target_ref` | 1 to release now, 0 to keep holding or if never held |
//! | ELEM_REPLACE | `target += target_ref`, `source -= source_ref` | new follows INSERT, old follows DELETE |
//! | ITEM_LINK / ITEM_UNLINK | same | always 0 -- Map item references are never exchanged |
//!
//! After a DELETE or REPLACE, an old element whose refcount drops to 0 is freed by the engine on
//! the spot.
//!
//! # The one invariant
//!
//! **We always hold one reference to every element of an event Map.** INSERT and the new side
//! of REPLACE are always answered with 1. Only `vop create` (and its replication/replay) sets
//! the event flag, so every Map that sends events is one of our index Maps, and the engine
//! assumes so too (`do_map_elem_update`: a refcount of 0 means not an event Map).
//!
//! So on delete, "do I hold this?" is always "yes", and the only question is **who releases it,
//! and when**:
//!
//! | The element's index | Answer | Released by |
//! |---|---|---|
//! | exists | hold | the vkey's lane, after removing it from the graph (`apply`) |
//! | missing (cleaned up or never built) | release now | the engine; nothing can dereference it |
//!
//! Indexes are looked up by name together with the **Map address** (`registry::of_map`), so
//! elements of an index recreated under the same name never mix with those of the dropped one.
//!
//! # No graph work here
//!
//! **This function is called with the engine's `cache_lock` held.** It runs on whichever thread
//! linked the element -- a worker for client writes, the replication apply thread on a replica,
//! the recovery thread at startup, or the engine's collection-delete thread when a Map is
//! dropped. Touching HNSW here would stall the whole daemon, so for each element event the
//! callback creates one graph job and queues it on the lane chosen by the element's vkey hash.
//! Jobs for the same vkey line up on one lane in event order, so they reach the graph in that
//! order.
//!
//! The node carrying a job comes from one of two places:
//!
//! * A node this thread's write command (`vop upsert`, `setmeta`, `delete`) [`offer`]ed before
//!   writing to the engine. It carries the connection, which the lane wakes to reply when done.
//!   The client's next search after the reply sees the result.
//! * Otherwise (replication, replay, expiry) the callback allocates one. There is no connection
//!   to reply to.
//!
//! The only engine functions callable here are `get_item_info` and `get_elem_info`; everything
//! else re-acquires `cache_lock` and deadlocks on the spot.

use std::cell::Cell;
use std::hash::{DefaultHasher, Hasher};
use std::os::raw::c_void;
use std::sync::Arc;

use crate::engine::element::{CodecError, IndexConfig};
use crate::engine::{MapElements, Store};
use crate::index::AnnIndex;
use crate::index::registry::{self, VectorIndex};
use crate::runtime::offload::{self, Node};
use crate::trigger::apply::{Graph, Op};

use crate::ffi::bindings::{
    ENGINE_EVENT_TYPE, ENGINE_EVENT_TYPE_ON_EVENT_ITEM, event_data_t, event_type_t,
    event_type_t_EVENT_ELEM_DELETE, event_type_t_EVENT_ELEM_INSERT,
    event_type_t_EVENT_ELEM_REPLACE, event_type_t_EVENT_ITEM_LINK, event_type_t_EVENT_ITEM_UNLINK,
};

/// What was decided for the outgoing (deleted or replaced) old element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Old {
    /// Keep holding it. It was in the graph, so the sweeper releases it after in-flight searches
    /// end, or the inserting lane does.
    Held,
    /// It never reached the graph. Nothing dereferences it, so the engine reclaims it now.
    Returned,
    /// We never held it. There is no reference to drop.
    NotOurs,
}

/// What the callback decided. Separates dispatch from engine pointers so it can be tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    /// Whether the incoming element (`target` of INSERT/REPLACE) is held.
    new: bool,
    /// What to do with the outgoing element (`target` of DELETE, `source` of REPLACE).
    old: Old,
}

impl Decision {
    /// No references exchanged: item events and elements that are not ours.
    pub(crate) const NOTHING: Self = Self {
        new: false,
        old: Old::NotOurs,
    };

    pub(crate) fn insert(held: bool) -> Self {
        Self {
            new: held,
            old: Old::NotOurs,
        }
    }

    pub(crate) fn delete(old: Old) -> Self {
        Self { new: false, old }
    }

    pub(crate) fn replace(new: bool, old: Old) -> Self {
        Self { new, old }
    }
}

/// Reference deltas returned to the engine, as `target_ref` and `source_ref`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refs {
    pub target: u16,
    pub source: u16,
}

/// Converts a decision into the reference counts the engine moves (the first table in the module
/// docs).
pub(crate) fn refs_for(event: Event, decision: Decision) -> Refs {
    let returned = u16::from(decision.old == Old::Returned);
    let held = u16::from(decision.new);
    match event {
        Event::ItemLink | Event::ItemUnlink => Refs {
            target: 0,
            source: 0,
        },
        Event::Insert => Refs {
            target: held,
            source: 0,
        },
        Event::Delete => Refs {
            target: returned,
            source: 0,
        },
        Event::Replace => Refs {
            target: held,
            source: returned,
        },
    }
}

/// The five cases the engine reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// A Map was linked. If its value is a config document, **this is where the index is born.**
    ItemLink,
    /// A Map is being unlinked. **This is where the index goes away.**
    ItemUnlink,
    /// An element arrived. Answer whether we hold it.
    Insert,
    /// An element is leaving. Answer whether to release our reference now.
    Delete,
    /// An element is replaced by a new one. Move the graph over and let go of old.
    Replace,
}

impl Event {
    pub(crate) fn of(ty: event_type_t) -> Option<Self> {
        match ty {
            t if t == event_type_t_EVENT_ITEM_LINK => Some(Self::ItemLink),
            t if t == event_type_t_EVENT_ITEM_UNLINK => Some(Self::ItemUnlink),
            t if t == event_type_t_EVENT_ELEM_INSERT => Some(Self::Insert),
            t if t == event_type_t_EVENT_ELEM_DELETE => Some(Self::Delete),
            t if t == event_type_t_EVENT_ELEM_REPLACE => Some(Self::Replace),
            _ => None,
        }
    }
}

/// Registers the callback. Called once from `memcached_extensions_initialize`.
///
/// That runs inside the getopt loop, **before** engine initialization and therefore before
/// persistence recovery relinks elements, so links made by recovery are not missed.
pub fn install() {
    let server = crate::ffi::server::handle();
    if server.is_null() {
        eprintln!("ArcVector: no server API; the item event callback is not installed");
        return;
    }
    let callback = unsafe { (*server).callback };
    if callback.is_null() {
        eprintln!("ArcVector: no callback API; the item event callback is not installed");
        return;
    }
    let Some(register) = (unsafe { (*callback).register_callback }) else {
        eprintln!("ArcVector: register_callback missing; the item event callback is not installed");
        return;
    };
    unsafe {
        register(
            std::ptr::null_mut(),
            ENGINE_EVENT_TYPE_ON_EVENT_ITEM,
            Some(on_item_event),
            std::ptr::null(),
        );
    }
}

/// The node a write command offered before writing to the engine, and the index it pinned.
#[derive(Clone, Copy)]
struct Offer {
    node: *mut Node,
    /// The index the command pinned. The node is used only when the event's index is this one --
    /// work for an index recreated under the same name after the command pinned the old one must
    /// not carry that command's connection. Null outside [`expect`], so no event takes the node.
    expect: *const VectorIndex,
}

thread_local! {
    /// This thread's "current job". Present only while a write command runs.
    ///
    /// Thread-local suffices: the callback is invoked synchronously, mid-write, on the very
    /// thread that wrote to the engine.
    static CURRENT: Cell<Option<Offer>> = const { Cell::new(None) };
}

/// Offers a node before a write command writes to the engine. **On a worker thread.**
///
/// The node must belong to a connection that has already done `waitfor_io_complete` and
/// `on_io_complete` -- the lane may finish and wake it before this command's engine write even
/// returns.
pub(crate) fn offer(node: *mut Node) {
    CURRENT.with(|current| {
        current.set(Some(Offer {
            node,
            expect: std::ptr::null(),
        }))
    });
}

/// If `f` (an engine write) changes elements of `index`, the offered node carries that work.
pub(crate) fn expect<T>(index: &Arc<VectorIndex>, f: impl FnOnce() -> T) -> T {
    let set = |expect| {
        CURRENT.with(|current| {
            if let Some(offer) = current.get() {
                current.set(Some(Offer { expect, ..offer }));
            }
        })
    };
    set(Arc::as_ptr(index));
    let out = f();
    set(std::ptr::null());
    out
}

/// Withdraws the offered node. `None` if the callback took it -- the node now belongs to the
/// lane, which wakes the connection when done.
pub(crate) fn withdraw() -> Option<*mut Node> {
    CURRENT
        .with(|current| current.take())
        .map(|offer| offer.node)
}

/// The node this thread's command offered, if it is for `index`. Taking it clears the slot.
fn take_offered(index: &Arc<VectorIndex>) -> Option<*mut Node> {
    CURRENT.with(|current| {
        let offer = current.get()?;
        if !std::ptr::eq(offer.expect, Arc::as_ptr(index)) {
            return None;
        }
        current.set(None);
        Some(offer.node)
    })
}

/// Puts a taken node back when the job could not be queued.
fn give_back(index: &Arc<VectorIndex>, node: *mut Node) {
    CURRENT.with(|current| {
        current.set(Some(Offer {
            node,
            expect: Arc::as_ptr(index),
        }))
    });
}

/// Called by the engine as `perform_callbacks(ON_EVENT_ITEM, &ev, NULL)`.
///
/// Because `mc_util.c` calls `h->cb(c, type, data, cb_data)`, **`event_data` is `&ev` and
/// `cookie` is NULL**. Reading them the other way round dereferences a null pointer.
unsafe extern "C" fn on_item_event(
    _cookie: *const c_void,
    _type: ENGINE_EVENT_TYPE,
    event_data: *const c_void,
    _cb_data: *const c_void,
) {
    if event_data.is_null() {
        return;
    }
    // The engine passes `ev` from its own stack and reads `target_ref` and `source_ref` after we
    // return to move references, so our answer is written there.
    let ev = unsafe { &mut *(event_data as *mut event_data_t) };
    let Some(event) = Event::of(ev.type_) else {
        return;
    };

    let decision = unsafe { dispatch(event, ev) };

    let refs = refs_for(event, decision);
    ev.target_ref = refs.target;
    ev.source_ref = refs.source;
}

/// # Safety
///
/// The pointers in `ev` are kept alive by the engine and valid for the duration of this call.
unsafe fn dispatch(event: Event, ev: &event_data_t) -> Decision {
    let store = Store::background();

    match event {
        Event::ItemLink => {
            if let Some(store) = &store {
                on_create(store, ev.target as u64);
            }
            Decision::NOTHING
        }
        Event::ItemUnlink => {
            let map = ev.target as u64;
            if let Some(store) = &store {
                store.with_key_at(map, |key| {
                    if let Some(name) = crate::trigger::key::parse(key) {
                        on_drop(name, map);
                    }
                });
            }
            Decision::NOTHING
        }
        Event::Insert | Event::Delete | Event::Replace => {
            let map = ev.coll_item as u64;
            let (target, source) = (ev.target as u64, ev.source as u64);
            // Borrow the key. Copying the name would allocate under the cache lock, and if that
            // failed there would be no way to answer this event.
            let routed = store.as_ref().and_then(|store| {
                store.with_key_at(map, |key| {
                    let index = crate::trigger::key::parse(key)
                        .and_then(|name| registry::of_map(name, map));
                    on_element(store, event, index, target, source)
                })
            });
            // The engine could not be queried or the key could not be read. Same as a missing
            // index -- it is still an event Map element, so hold new and release old now.
            routed.unwrap_or_else(|| on_element_unrouted(event))
        }
    }
}

/// One element event. `index` is the index of the element's Map; `None` means it was cleaned up
/// or never built.
fn on_element(
    store: &Store,
    event: Event,
    index: Option<Arc<VectorIndex>>,
    target: u64,
    source: u64,
) -> Decision {
    let Some(index) = index else {
        return on_element_unrouted(event);
    };
    let op = match event {
        Event::Insert => Op::Insert(target),
        Event::Delete => Op::Delete(target),
        _ => Op::Replace {
            old: source,
            new: target,
        },
    };
    // The lane is chosen by vkey. Old and new of a replace share the vkey.
    let lane = store
        .with_elem_at(target, |field, _value| vkey_hash(field))
        .unwrap_or(0);

    match queue(&index, op, lane) {
        // The lane releases whatever needs releasing.
        Queued::Yes => on_element_queued(event),
        // Cleaned up in the meantime. Same as a missing index.
        Queued::Gone => on_element_unrouted(event),
        Queued::Failed => {
            // No room for the job or no lane to take it. The new element stays held outside the
            // graph; the outgoing one cannot be removed from the graph, so it stays held too --
            // releasing it would let searches holding its address read freed memory.
            eprintln!(
                "ArcVector: no room to queue graph work for '{}'; the graph lags the \
                 store and a removed vector stays held",
                index.name
            );
            on_element_queued(event)
        }
    }
}

/// The answer once a job is queued on a lane. Hold the new element and the outgoing one too --
/// the lane removes it from the graph and releases it.
fn on_element_queued(event: Event) -> Decision {
    match event {
        Event::Insert => Decision::insert(true),
        Event::Delete => Decision::delete(Old::Held),
        _ => Decision::replace(true, Old::Held),
    }
}

/// An element event whose index cannot be found. Hold the new element; release the outgoing one
/// now.
fn on_element_unrouted(event: Event) -> Decision {
    match event {
        Event::Insert => Decision::insert(true),
        Event::Delete => Decision::delete(Old::Returned),
        _ => Decision::replace(true, Old::Returned),
    }
}

/// Result of [`queue`].
#[derive(Debug, PartialEq, Eq)]
enum Queued {
    Yes,
    /// The index was cleaned up; there is nowhere to queue the job.
    Gone,
    /// No node could be created, or no lane accepted it.
    Failed,
}

/// Queues one graph job on the lane for hash `lane`.
///
/// Uses the node offered by this thread's command if it is for this index (so its connection is
/// woken); otherwise allocates a new node.
fn queue(index: &Arc<VectorIndex>, op: Op, lane: u64) -> Queued {
    let Some(pin) = index.pin() else {
        return Queued::Gone;
    };
    let offered = take_offered(index);
    let graph = Graph { index: pin, op };
    match unsafe { offload::enqueue(offered, graph, lane) } {
        Ok(()) => Queued::Yes,
        Err(()) => {
            if let Some(node) = offered {
                give_back(index, node);
            }
            Queued::Failed
        }
    }
}

/// vkey hash used to choose a lane.
fn vkey_hash(vkey: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(vkey);
    hasher.finish()
}

/// The index's Map was unlinked. Detach it from the registry and mark it dropped.
///
/// `vop drop` on the master, the replicated delete on replicas and in replay, and expiry or
/// eviction anywhere all end up here. Cleanup happens immediately if no command pins the index,
/// otherwise when the last one releases it (`registry`). Until cleanup, element events still
/// find this index and remove elements from the graph; afterwards they release them on the spot.
fn on_drop(name: &str, map: u64) {
    registry::drop_map(name, map);
}

/// A Map with the event flag was linked. If its value is a config document, build and register
/// the index.
///
/// **This is the moment the index comes into being in this process.** This node's `vop create`,
/// a Map arriving via replication, and persistence replay all reach this link. Replicas never
/// receive `vop create`, so this is the only way an index appears there.
///
/// It serves immediately after registration. A freshly linked Map has no elements, and elements
/// that follow are inserted by the jobs their events queue.
///
/// If it cannot be built (the value is not a config, or there is no room), the Map has no index.
/// Its elements are still held and released when they leave (see the module docs).
fn on_create(store: &Store, map: u64) {
    // The index owns its name, so copy it here. If that fails, do not build.
    let Some(Ok(name)) = store
        .with_key_at(map, |key| {
            crate::trigger::key::parse(key).map(crate::fallible::string)
        })
        .flatten()
    else {
        return;
    };
    let Some(index) = store
        .with_item_value_at(map, |value| build_from_value(name, map, value))
        .flatten()
    else {
        return;
    };
    if registry::put(index).is_err() {
        eprintln!("ArcVector: the index registry could not grow; a new index stays unknown");
    }
}

/// Builds an index from a Map value without registering it.
///
/// `None` if the value does not decode as a config document -- not our index; skipped silently.
///
/// `AnnIndex::new` allocates the usearch graph, so this allocates under the cache lock. That
/// happens once per index, which is acceptable.
fn build_from_value(name: String, map: u64, value: &[u8]) -> Option<VectorIndex> {
    let config = match IndexConfig::decode(value) {
        Ok(config) => config,
        // An out-of-range value means our config is corrupt. Report it rather than skipping
        // silently.
        Err(e @ CodecError::OutOfRange(_)) => {
            eprintln!("ArcVector: not building the graph for '{name}': {e}");
            return None;
        }
        Err(_) => return None,
    };
    match AnnIndex::new(
        config.layout(),
        config.metric,
        config.m,
        config.efc,
        config.efs,
        Arc::new(MapElements),
    ) {
        Ok(ann) => Some(VectorIndex::ours(name, map, ann)),
        Err(e) => {
            eprintln!("ArcVector: could not build the graph for '{name}': {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(target: u16, source: u16) -> Refs {
        Refs { target, source }
    }

    #[test]
    fn insert_takes_one_when_held_and_none_when_declined() {
        assert_eq!(refs_for(Event::Insert, Decision::insert(true)), refs(1, 0));
        // Not held: no reference taken, nothing to release later.
        assert_eq!(refs_for(Event::Insert, Decision::insert(false)), refs(0, 0));
    }

    #[test]
    fn delete_returns_now_only_what_never_reached_the_graph() {
        // In the graph: kept, released by the sweeper after the barrier.
        assert_eq!(
            refs_for(Event::Delete, Decision::delete(Old::Held)),
            refs(0, 0)
        );
        // No index: nothing dereferences it, so the engine reclaims it now.
        assert_eq!(
            refs_for(Event::Delete, Decision::delete(Old::Returned)),
            refs(1, 0)
        );
        // Never held: must not drop a reference we never took.
        assert_eq!(
            refs_for(Event::Delete, Decision::delete(Old::NotOurs)),
            refs(0, 0)
        );
    }

    #[test]
    fn replace_takes_new_and_keeps_old_for_the_sweeper() {
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(true, Old::Held)),
            refs(1, 0)
        );
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(true, Old::Returned)),
            refs(1, 1)
        );
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(true, Old::NotOurs)),
            refs(1, 0)
        );
        // If new is not held the engine does not bump it; old is decided separately.
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(false, Old::Held)).target,
            0
        );
        assert_eq!(
            refs_for(Event::Replace, Decision::replace(false, Old::Returned)),
            refs(0, 1)
        );
    }

    #[test]
    fn item_events_never_move_references() {
        for event in [Event::ItemLink, Event::ItemUnlink] {
            for decision in [
                Decision::NOTHING,
                Decision::insert(true),
                Decision::replace(true, Old::Returned),
            ] {
                assert_eq!(refs_for(event, decision), refs(0, 0), "{event:?}");
            }
        }
    }

    #[test]
    fn the_five_event_types_are_told_apart() {
        assert_eq!(
            Event::of(event_type_t_EVENT_ITEM_LINK),
            Some(Event::ItemLink)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ITEM_UNLINK),
            Some(Event::ItemUnlink)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ELEM_INSERT),
            Some(Event::Insert)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ELEM_DELETE),
            Some(Event::Delete)
        );
        assert_eq!(
            Event::of(event_type_t_EVENT_ELEM_REPLACE),
            Some(Event::Replace)
        );
        assert_eq!(Event::of(9999), None, "unknown values are ignored");
    }

    #[test]
    fn an_item_link_builds_the_index_from_the_map_value() {
        use crate::index::Metric;
        use crate::index::quant::Quant;

        let config = IndexConfig {
            dim: 4,
            metric: Metric::L2,
            quant: Quant::F32,
            m: 16,
            efc: 128,
            efs: 64,
        };
        // The Map value arrives with the engine's trailing CRLF intact.
        let index = build_from_value("photos".to_owned(), 0x1000, &config.encode())
            .expect("a Map holding a config document is our index");
        assert_eq!(index.name, "photos");
        assert_eq!(index.ann.layout, config.layout());
    }

    #[test]
    fn a_replicated_config_out_of_bounds_builds_no_index() {
        use crate::index::Metric;
        use crate::index::quant::Quant;

        // Values from replication/recovery never went through `create`'s validation.
        for (dim, m, efc, efs) in [
            (4, u32::MAX as usize, 128, 64),
            (4, 16, u32::MAX as usize, 64),
            (4, 16, 128, u32::MAX as usize),
            (0, 16, 128, 64),
            (70000, 16, 128, 64),
        ] {
            let config = IndexConfig {
                dim,
                metric: Metric::L2,
                quant: Quant::F32,
                m,
                efc,
                efs,
            };
            assert!(build_from_value("bad".to_owned(), 0x1000, &config.encode()).is_none());
        }
    }

    #[test]
    fn a_map_whose_value_is_not_a_config_is_not_ours() {
        // A Map made with `mop create` has a value of just CRLF.
        assert!(build_from_value("plain".to_owned(), 0x1000, b"\r\n").is_none());
        assert!(build_from_value("plain".to_owned(), 0x1000, b"").is_none());
        assert!(build_from_value("plain".to_owned(), 0x1000, &crate::bson::EMPTY).is_none());
    }

    /// Test index, not put in the registry.
    fn graph(name: &str, map: u64) -> Arc<VectorIndex> {
        use crate::index::Metric;
        use crate::index::quant::Quant;

        let config = IndexConfig {
            dim: 4,
            metric: Metric::L2,
            quant: Quant::F32,
            m: 16,
            efc: 128,
            efs: 64,
        };
        Arc::new(build_from_value(name.to_owned(), map, &config.encode()).expect("a config"))
    }

    /// Fake node address; never dereferenced.
    fn fake_node(n: usize) -> *mut Node {
        n as *mut Node
    }

    #[test]
    fn no_node_is_taken_outside_a_write() {
        // With no command running (e.g. the replication apply thread) there is no node to hand
        // over.
        let index = graph("offer-none", 0x2000);
        assert!(take_offered(&index).is_none());
        assert!(withdraw().is_none());

        // Offered but outside an engine write (`expect`): no event takes it.
        offer(fake_node(0x10));
        assert!(take_offered(&index).is_none());
        assert_eq!(
            withdraw(),
            Some(fake_node(0x10)),
            "the command gets it back"
        );
        assert!(withdraw().is_none(), "withdrawing clears the slot");
    }

    #[test]
    fn a_write_hands_its_node_only_to_the_index_it_holds() {
        let held = graph("offer-held", 0x3000);
        // An index recreated under the same name after the command pinned the old one.
        let recreated = graph("offer-held", 0x4000);

        offer(fake_node(0x20));
        let taken = expect(&held, || {
            (
                take_offered(&recreated),
                take_offered(&held),
                take_offered(&held),
            )
        });
        assert_eq!(
            taken,
            (None, Some(fake_node(0x20)), None),
            "the recreated index's work gets a node of its own, and the node goes once"
        );
        assert!(withdraw().is_none(), "a taken node belongs to the lane");
    }

    #[test]
    fn a_node_given_back_stays_with_the_command() {
        let held = graph("offer-back", 0x5000);
        offer(fake_node(0x30));
        expect(&held, || {
            let node = take_offered(&held).expect("the write's own index");
            give_back(&held, node);
        });
        assert_eq!(withdraw(), Some(fake_node(0x30)));
    }

    #[test]
    fn element_answers_hold_new_and_leave_old_to_the_lane_or_the_engine() {
        // Queued on a lane: hold the outgoing element too; the lane removes and releases it.
        assert_eq!(on_element_queued(Event::Insert), Decision::insert(true));
        assert_eq!(
            on_element_queued(Event::Delete),
            Decision::delete(Old::Held)
        );
        assert_eq!(
            on_element_queued(Event::Replace),
            Decision::replace(true, Old::Held)
        );
        // No index: hold the new element; the engine reclaims the outgoing one now.
        assert_eq!(on_element_unrouted(Event::Insert), Decision::insert(true));
        assert_eq!(
            on_element_unrouted(Event::Delete),
            Decision::delete(Old::Returned)
        );
        assert_eq!(
            on_element_unrouted(Event::Replace),
            Decision::replace(true, Old::Returned)
        );
    }

    #[test]
    fn the_same_vkey_always_hashes_to_the_same_lane() {
        assert_eq!(vkey_hash(b"v1"), vkey_hash(b"v1"));
        assert_ne!(vkey_hash(b"v1"), vkey_hash(b"v2"));
    }
}
