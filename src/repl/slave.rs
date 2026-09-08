//! The replica side of replication: dial whoever `AV OWNER` names, take the
//! master's index-name list, bring each index into agreement with this
//! node's own Map, and apply deltas as they arrive.
//!
//! A delta says *where to look*, not *what to write*: arcus already
//! replicates the Map, so this wire carries identifiers only, and a replica
//! resolves each one against its own Map. That is what lets the two
//! replication streams -- ours and arcus's -- need no relative ordering
//! between them: a delta that arrives before the element it names is simply
//! deferred and retried, never an error.
//!
//! Two things this file must keep, because the master's flow control depends
//! on them: the socket is read and applied inline, in the same loop, never
//! drained into an unbounded buffer for later (`session` below is the whole
//! argument for that); and the deferred set is bounded, both by count
//! (`MAX_DEFERRED`) and by age (`DEFER_PATIENCE`) -- past either, deferred
//! work has stopped being a race absorbed and started being lag hidden, and
//! the affected index is resynchronised instead. A resync that only marks a
//! flag and never actually rebuilds anything is the same failure wearing a
//! disguise, so every path that decides one is owed logs it and drives it
//! through the one place that knows how to build an `AnnIndex` from a Map
//! (`rebuild`, below) -- never a bare `recovery::fill`, which is a no-op on
//! an index that is not already `COLD`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::role;
use super::wire::{self, Msg, Op, PROTO_VER};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry;
use crate::handler::usearch::PublishError;

/// Past this many outstanding deferrals, the set itself would be hiding how
/// far behind this node is; the affected indexes are resynchronised instead
/// of letting it grow further.
const MAX_DEFERRED: usize = 4096;

/// How long a single deferred entry is tolerated before its index is
/// resynchronised, independent of how many other entries are deferred. A
/// count that never reaches `MAX_DEFERRED` can still hide one id that never
/// resolves; patience is what surfaces that case too.
const DEFER_PATIENCE: Duration = Duration::from_secs(30);

const RETRY: Duration = Duration::from_secs(1);

/// How often the frame-driven retry (as opposed to the idle-tick one) is
/// actually allowed to run its `Store` calls. Once per applied frame is not
/// a useful grain of retry -- under real traffic it is an O(n) pass over the
/// deferred set repeated far more often than matters, on the very thread
/// that must keep reading the socket for the master's flow control to work
/// at all. This keeps that cost bounded without giving up the responsiveness
/// the idle tick alone (`RETRY`, a full second) did not have.
const RETRY_THROTTLE: Duration = Duration::from_millis(200);

static DEFERRED: AtomicUsize = AtomicUsize::new(0);

/// Upserts this node cannot yet resolve against its own Map, kept until the
/// Map catches up or patience runs out.
#[derive(Default)]
pub struct Pending {
    waiting: HashMap<(String, String), Instant>,
    overflowed: bool,
    /// Indexes named by a `defer` that overflowed before it could be stored.
    /// Kept apart from `waiting`: the entry that trips `MAX_DEFERRED` is
    /// dropped, not inserted, so if its index has no other entry already
    /// waiting, `waiting.keys()` alone would never name it -- and a resync
    /// driven only by `indexes()` would silently skip the very index whose
    /// update was just thrown away.
    overflow_indexes: HashSet<String>,
}

impl Pending {
    pub fn defer(&mut self, index: String, id: String) {
        if self.waiting.len() >= MAX_DEFERRED || self.waiting.try_reserve(1).is_err() {
            self.overflowed = true;
            // Record the index before giving up on the entry -- otherwise an
            // index with nothing else pending is dropped here and never
            // named for the resync this triggers.
            if self.overflow_indexes.try_reserve(1).is_ok() {
                self.overflow_indexes.insert(index);
            }
            return;
        }
        self.waiting.insert((index, id), Instant::now());
        DEFERRED.store(self.waiting.len(), Ordering::Relaxed);
    }

    pub fn resolved(&mut self, index: &str, id: &str) {
        self.waiting.remove(&(index.to_owned(), id.to_owned()));
        DEFERRED.store(self.waiting.len(), Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn indexes(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .waiting
            .keys()
            .map(|(index, _)| index.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every index that must be resynchronised because the deferred set
    /// overflowed: everything still waiting, plus any index whose entry was
    /// dropped the moment the bound tripped and so never made it into
    /// `waiting` at all. This, not `indexes()`, is what `overflowed()`'s
    /// remedy must iterate.
    pub fn overflow_indexes(&self) -> Vec<String> {
        let mut names = self.indexes();
        names.extend(self.overflow_indexes.iter().cloned());
        names.sort();
        names.dedup();
        names
    }

    /// Indexes carrying at least one entry deferred longer than
    /// `DEFER_PATIENCE` as of `now`. Taking `now` as a parameter, rather than
    /// reading the clock internally, is what makes the deadline observable
    /// from a test without an actual 30-second wait or a mocked clock this
    /// crate has no dependency for: a test hands in an `Instant` already past
    /// the deadline instead.
    pub fn expired(&self, now: Instant) -> Vec<String> {
        let mut names: Vec<String> = self
            .waiting
            .iter()
            .filter(|&(_, &since)| now.saturating_duration_since(since) >= DEFER_PATIENCE)
            .map(|((index, _), _)| index.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Drops every entry deferred against `index`. Called once that index has
    /// just been resynchronised, so whatever was deferred for it is either
    /// already satisfied or, if the Map still does not have it, not worth
    /// retrying every tick until it does.
    fn forget_index(&mut self, index: &str) {
        self.waiting.retain(|(i, _), _| i != index);
        DEFERRED.store(self.waiting.len(), Ordering::Relaxed);
    }

    /// The decision an overflowed deferred set makes, without a `Store`:
    /// which indexes it names for resync (`overflow_indexes`, the union that
    /// also catches the entry that tripped the bound), and giving up on
    /// every entry as a result -- there is no path back to the caller from
    /// which some of them survive uncounted. Naming and discarding happen
    /// together, in one call, so the two cannot drift apart at a call site
    /// the way they did before this existed.
    pub fn take_overflowed(&mut self) -> Vec<String> {
        let indexes = self.overflow_indexes();
        *self = Self::default();
        DEFERRED.store(0, Ordering::Relaxed);
        indexes
    }

    /// The same split as `take_overflowed`, for the patience remedy instead
    /// of the count one: which indexes have an entry older than
    /// `DEFER_PATIENCE` as of `now`, with exactly those entries forgotten --
    /// nothing still within patience is touched.
    pub fn take_expired(&mut self, now: Instant) -> Vec<String> {
        let indexes = self.expired(now);
        for name in &indexes {
            self.forget_index(name);
        }
        indexes
    }
}

pub fn deferred() -> usize {
    DEFERRED.load(Ordering::Relaxed)
}

/// Whether a `connect_loop` thread exists. Set before the thread is spawned
/// and cleared when it leaves, so it is never briefly `false` while a real
/// loop is on its way up -- which would let a beat spawn a second one.
static LOOP_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reserves the single connect-loop slot for a caller that is about to spawn
/// one. `false` means a loop already holds it and the caller must not spawn.
pub fn claim_loop_slot() -> bool {
    LOOP_LIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Gives the slot back: after a spawn that failed, and from
/// `repl::after_fork_in_child`, where the flag is copied `true` into a child
/// whose loop thread did not survive the fork. A plain atomic store, which is
/// all a fork handler may do.
pub fn release_loop_slot() {
    LOOP_LIVE.store(false, Ordering::Release);
}

/// Dials whoever `AV OWNER` names, for as long as this node is a replica.
///
/// Never returns on its own. The guard exists for the abnormal exit -- a panic
/// unwinding out of the loop -- after which the slot has to be free again or
/// `repl::ensure_slave_loop` would believe a loop is still running and this
/// node would stay a replica with nothing dialling the master, silently and
/// permanently. That is the same failure the lazy re-arm exists to end.
pub fn connect_loop() {
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            release_loop_slot();
        }
    }
    let _slot = Slot;

    // Carried between rounds so a redirect survives the sleep: the owner key
    // that sent us to the wrong node is still the same stale value, and
    // re-reading it would send us straight back.
    let mut redirect: Option<String> = None;

    loop {
        if role::shared().role() != role::Role::Replica {
            std::thread::sleep(RETRY);
            redirect = None;
            continue;
        }
        redirect = dial_round(redirect.take());
        std::thread::sleep(RETRY);
    }
}

/// How many redirects one round follows before going back to the owner key.
///
/// A redirect is one node's opinion, and two nodes with stale views can name
/// each other. Following a few is what makes a genuine hand-off resolve in
/// one round instead of one per heartbeat; refusing to follow more is what
/// keeps a cycle from spinning this thread. Whatever is unresolved after this
/// waits for `RETRY` and starts again from a freshly read key, which by then
/// has usually caught up.
const MAX_REDIRECTS: usize = 3;

/// One round of dialling: the address to try, then whatever it redirects to.
///
/// Returns a redirect the next round should start from, if this one ran out
/// of hops still holding one. `None` means start again from the owner key --
/// either because the round finished cleanly, or because there is nothing
/// better to go on than what replication brings in.
fn dial_round(start: Option<String>) -> Option<String> {
    let mut target = match start.or_else(master_addr) {
        Some(addr) => addr,
        None => return None,
    };

    for _ in 0..MAX_REDIRECTS {
        // A redirect can name this node -- the peer's own view can be as
        // stale as ours -- and dialling ourselves would connect to our own
        // listener and hang on a snapshot that never comes.
        if role::shared().is_stale_owner(&target) {
            return None;
        }
        let Ok(sock) = TcpStream::connect(&target) else {
            // Down, not wrong. The address may still be right and the node
            // still coming up, so this is left for the next round to re-read
            // rather than being followed anywhere.
            return None;
        };
        match session(sock) {
            Some(next) if next != target => target = next,
            // Either the session ran and ended on its own terms, or the peer
            // redirected us to the address we just dialled, which is no new
            // information.
            _ => return None,
        }
    }
    Some(target)
}

/// The owner key arrived here through arcus replication, the same path that
/// carries the Map. A value equal to our own published address is one we
/// wrote before being demoted, and the new master's write has not replicated
/// yet.
fn master_addr() -> Option<String> {
    let addr = role::read_owner()?;
    if role::shared().is_stale_owner(&addr) {
        return None;
    }
    Some(addr)
}

/// Runs for the life of one connection to the master. Every frame is applied
/// -- `apply`, right below the `read_frame` that produced it -- before the
/// next one is read: nothing here accumulates frames in a buffer for later,
/// which is what lets a replica that is genuinely behind stop reading and
/// let TCP backpressure reach the master's send queue, rather than reading
/// ahead into memory while looking, from the master's side, perfectly caught
/// up.
///
/// Re-checks the role every iteration: a promotion can land mid-connection,
/// and a promoted node must stop applying deltas as soon as that is known
/// rather than riding the socket until it happens to close. The next task's
/// teardown on role change depends on this loop actually noticing.
fn session(mut sock: TcpStream) -> Option<String> {
    let _ = sock.set_nodelay(true);
    let hello = wire::encode(&Msg::Sync {
        node_id: crate::owner::ours().to_owned(),
        proto_ver: PROTO_VER,
        accepts_graph: false,
    });
    if sock.write_all(&hello).is_err() {
        return None;
    }

    let mut pending = Pending::default();
    // Both `None` until first used, at which point they are always due --
    // there is nothing to throttle against yet.
    let mut last_retry: Option<Instant> = None;
    let mut last_expiry_check: Option<Instant> = None;
    let mut reader = match sock.try_clone() {
        Ok(r) => r,
        Err(_) => return None,
    };
    let _ = reader.set_read_timeout(Some(RETRY));

    loop {
        if role::shared().role() != role::Role::Replica {
            return None;
        }

        match wire::read_frame(&mut reader) {
            Ok(frame) => match wire::decode(&frame) {
                // The node we dialled is not the master. It is the only one
                // that can say so authoritatively -- the owner key we read to
                // get here is whatever replicated in last -- so take its word
                // and its suggestion, and stop reading this connection.
                Ok(Msg::NotMaster { master }) => {
                    return Some(master).filter(|m| !m.is_empty());
                }
                Ok(msg) => {
                    apply(msg, &mut pending);
                    // Retried right away, not just on the idle tick below:
                    // on a busy stream the read timeout may never fire, and
                    // without this a deferred upsert would sit untried until
                    // it aged all the way into `expired`. Throttled
                    // (`RETRY_THROTTLE`), unlike the idle tick, because this
                    // arm can run once per frame -- without a floor on how
                    // often it actually does the O(n) work, a fast stream
                    // would turn every single delta into a full pass over
                    // the deferred set.
                    if !pending.is_empty() && should_retry(last_retry, Instant::now()) {
                        retry_deferred(&mut pending);
                        last_retry = Some(Instant::now());
                    }
                }
                Err(_) => return None, // A frame we cannot read desynchronises us.
            },
            Err(wire::WireError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Not throttled: this arm already runs at most once per
                // `RETRY` (a full second), which the idle-tick design always
                // relied on being the only rate limit it needed.
                if !pending.is_empty() {
                    retry_deferred(&mut pending);
                    last_retry = Some(Instant::now());
                }
            }
            Err(_) => return None,
        }

        // Throttled like the retry above and for the same reason: this is an
        // O(n) scan of `waiting` on every loop iteration otherwise, on the
        // thread that must keep reading the socket. A separate timestamp
        // from the retry's, since the two are independent pieces of work
        // that happen to share a rate limit, not one operation.
        if !pending.is_empty() && should_retry(last_expiry_check, Instant::now()) {
            for name in pending.take_expired(Instant::now()) {
                eprintln!(
                    "ArcVector: a deferred update for index '{name}' waited past \
                     {DEFER_PATIENCE:?}; resynchronising"
                );
                rebuild(&name, None);
            }
            last_expiry_check = Some(Instant::now());
        }
        if pending.overflowed() {
            let affected = pending.take_overflowed();
            eprintln!(
                "ArcVector: the deferred set passed {MAX_DEFERRED} entries; resynchronising \
                 {} index(es): {affected:?}",
                affected.len()
            );
            for name in &affected {
                rebuild(name, None);
            }
        }
    }
}

/// Whether the frame-driven retry, last run at `last_retry`, is due again as
/// of `now`. `last_retry` as a plain argument (not read from a clock or
/// stored state this function owns) is what makes the throttle itself
/// testable without waiting `RETRY_THROTTLE` in real time -- the same
/// approach `Pending::expired` uses for `DEFER_PATIENCE`.
fn should_retry(last_retry: Option<Instant>, now: Instant) -> bool {
    last_retry.is_none_or(|since| now.saturating_duration_since(since) >= RETRY_THROTTLE)
}

fn apply(msg: Msg, pending: &mut Pending) {
    match msg {
        // Handled where the connection can actually be abandoned, in
        // `session`; a redirect reaching here would mean the read loop let it
        // past, and there is nothing useful to do with it this far in.
        Msg::NotMaster { .. } => {}
        Msg::Snapshot { indexes } => {
            for name in indexes {
                converge(&name);
            }
        }
        Msg::Resync { index } => {
            if index.is_empty() {
                for held in registry::indexes().unwrap_or_default() {
                    converge(&held.name);
                }
            } else {
                converge(&index);
            }
        }
        Msg::Delta { index, id, op } => match op {
            Op::Upsert if id.is_empty() => converge(&index),
            Op::Delete if id.is_empty() => drop_index(&index),
            Op::Upsert => {
                // A name this delta's index has never been converged under in
                // this call: `apply` handles one delta, so the memo starts
                // empty and the first miss always gets its converge.
                if !upsert(&index, &id, &mut Converged::default()) {
                    pending.defer(index, id);
                }
            }
            Op::Delete => delete(&index, &id),
        },
        Msg::Sync { .. } => {} // A replica never receives one.
    }
}

/// Brings this index into agreement with its own Map: builds it from
/// scratch if this replica does not hold it at all, or evicts and rebuilds
/// it if what it holds disagrees with the Map's own count. The count check
/// is one `getattr`, the same one `access::sweep` already trusts for exactly
/// this comparison.
///
/// Public: a promoted node calls this on itself as a self-check, once it can
/// no longer trust that what it applied as a replica is complete.
pub fn converge(name: &str) {
    let Some(store) = Store::background_keyed() else {
        return;
    };
    let Some(index) = registry::get(name) else {
        // Nothing registered under this name yet -- a cold replica seeing it
        // for the first time in a `Snapshot`/`Resync`, most likely.
        rebuild(name, None);
        return;
    };
    match store.probe_map(name) {
        Ok(map) => {
            let in_map = map.count.saturating_sub(1) as usize;
            let named = index.ann.len();
            if in_map != named {
                eprintln!(
                    "ArcVector: replica's index '{name}' names {named} element(s) but its Map \
                     holds {in_map}; resynchronising"
                );
                // Passing the exact `Arc` just observed, not just the name:
                // `rebuild`'s eviction then goes through `remove_observed`'s
                // pointer check, so if another thread already rebuilt this
                // name correctly while this round trip to the Map was in
                // flight, that fresh index is left alone rather than
                // evicted on the strength of what we saw a moment ago.
                rebuild(name, Some(&index));
            }
        }
        Err(StoreError::KeyGone) => {
            registry::remove(name);
        }
        Err(_) => {}
    }
}

/// Forces `name` through a clean build against its Map: evicts whatever this
/// replica currently holds for it (see `evict_unless_building`), then builds
/// it through the same path a client request resolving it would use --
/// `access::resolve`, by way of `handler::resolve_for_replica`, the one
/// place that knows how to construct an `AnnIndex` from Map metadata.
///
/// The eviction is not optional. `access::resolve` hands back an already-
/// registered, `SERVING` index untouched -- that is the fast path every
/// ordinary read relies on -- so calling it on a live index it already has
/// does nothing at all. Only a *fresh* build (a name unknown to the
/// registry) reaches `recovery::drain` and lands `COLD`, which is what makes
/// the `recovery::fill` below actually run.
///
/// `observed` is the caller's evidence for the eviction, if it has any --
/// see `evict_unless_building`. Called only once a resync has already been
/// decided on: an index this replica does not hold at all (`None`), one
/// that disagrees with its own Map (`Some`, the exact `Arc` the mismatch was
/// read from), or one named by an overflowed or expired deferred set
/// (`None`, since neither remedy reads the index itself). Logs its outcome
/// either way, and only ever claims success once `rebuild_started` confirms
/// a fill actually ran -- a resync that happens invisibly, or that claims to
/// have happened when it did not, is exactly how this stayed a no-op twice.
fn rebuild(name: &str, observed: Option<&registry::VectorIndex>) {
    let Some(store) = Store::background_keyed() else {
        return;
    };
    evict_unless_building(name, observed);

    let index = match crate::handler::resolve_for_replica(&store, name) {
        Ok(index) => index,
        Err(e) => {
            eprintln!("ArcVector: replica could not resynchronise index '{name}': {e}");
            return;
        }
    };

    // `resolve` leaves a brand-new index `COLD` rather than filling it --
    // that step is normally `for_read`/`for_write`'s, run only once a
    // request lands. Nothing here waits for one, so it is triggered
    // directly; `None` when the index never reached `COLD` at all (the
    // eviction above lost a race, or was withheld for `BUILDING`).
    let fill_result = (index.state() == registry::COLD).then(|| {
        crate::handler::recovery::ensure_builder();
        crate::handler::recovery::fill(&store, &index)
    });

    if rebuild_started(fill_result.as_ref()) {
        eprintln!("ArcVector: replica resynchronised index '{name}'");
    } else if let Some(Err(e)) = &fill_result {
        eprintln!("ArcVector: replica resync of index '{name}' could not start a rebuild: {e}");
    } else {
        eprintln!(
            "ArcVector: replica resync of index '{name}' started no rebuild; \
             something else must have already resynchronised it"
        );
    }
}

/// Whether a rebuild actually started, decided purely from what `rebuild`
/// observed after resolving the index: `None` when it never reached `COLD`
/// (`recovery::fill` is never even called on a live index -- eviction
/// either lost a race to something else that got there first, or was
/// withheld because the index was `BUILDING`); `Some(Err(_))` when it did
/// reach `COLD` but `fill` itself could not start a build (its own
/// `BUILDER.enqueue` failing, which `fill` handles by abandoning the index
/// back to `COLD` rather than propagating a panic); `Some(Ok(()))` is the
/// only case that actually started work.
///
/// This is the entire decision `rebuild`'s log is based on, and unlike the
/// eviction and the build themselves, it needs neither a `Store` nor a live
/// index to test -- which is exactly the piece that let a false "success"
/// log through twice.
fn rebuild_started<E>(fill_result: Option<&Result<(), E>>) -> bool {
    matches!(fill_result, Some(Ok(())))
}

/// Evicts whatever is currently registered under `name`, honouring the same
/// refusal `registry::remove_if_stale` and `sweep::coldest` already give a
/// `BUILDING` index -- `registry::remove_observed` does not check state on
/// its own, so leaving a `BUILDING` index alone has to happen here instead.
///
/// `observed`, when given, is the exact instance a caller already read and
/// made a decision against (`converge`'s mismatch check, the only caller
/// that has one): eviction then goes through `remove_observed`'s pointer
/// check, so a name another thread has already rebuilt correctly in the
/// meantime -- while the caller was still in its own round trip to the Map
/// -- is left alone instead of evicted on a stale observation. `None` means
/// there is no earlier read to protect (an index never seen before, or one
/// the overflowed/expired remedies name without having read it themselves),
/// so whatever is currently registered, if anything, is evicted
/// unconditionally, short of the `BUILDING` guard above.
fn evict_unless_building(name: &str, observed: Option<&registry::VectorIndex>) {
    match observed {
        Some(index) => {
            if index.state() != registry::BUILDING {
                registry::remove_observed(name, index);
            }
        }
        None => {
            if let Some(current) = registry::get(name)
                && current.state() != registry::BUILDING
            {
                registry::remove_observed(name, &current);
            }
        }
    }
}

/// The index names one pass has already tried to converge, so a deferred set
/// full of entries for a single index this replica does not hold costs one
/// rebuild attempt per pass rather than one per entry (`MAX_DEFERRED` is 4096,
/// and `retry_deferred` runs every `RETRY_THROTTLE` on the same thread that
/// must keep reading the socket).
///
/// A `Vec` rather than a `HashSet`: it holds the *distinct unknown* index
/// names of one pass, which is one in every case worth optimising for, and a
/// linear scan of that beats hashing every name. Growth goes through
/// `try_reserve`, per this crate's rule; a refused allocation just means the
/// memo forgets, which costs a repeated converge, never correctness.
#[derive(Default)]
struct Converged(Vec<String>);

impl Converged {
    /// True the first time a name is offered, false afterwards.
    fn first_time(&mut self, name: &str) -> bool {
        if self.0.iter().any(|seen| seen == name) {
            return false;
        }
        if self.0.try_reserve(1).is_ok() {
            self.0.push(name.to_owned());
        }
        true
    }
}

/// True once the element is in this node's own Map. The delta is a hint about
/// where to look, never the data itself, so it may arrive before arcus has
/// replicated what it refers to -- that is not an error, just a reason to
/// come back later.
///
/// A name the registry does not hold is different, and used to be treated the
/// same: it would answer `false` and the caller would defer, but every retry
/// calls this same function, which misses the registry again, so nothing ever
/// registered the name and the entry simply aged into `DEFER_PATIENCE` (30s)
/// before a rebuild was forced. That is reachable on the ordinary
/// create-then-write path -- `vcreate` arrives as an empty-id `Upsert`, which
/// `apply` routes to `converge`, and on a replica whose Map has not replicated
/// yet that converge fails with no defer path of its own -- so a brand-new
/// index took ~30s to reach a replica. `converge` is the one thing that can
/// register the name, so a miss runs it here and then looks again; a defer is
/// what happens only if even that could not.
fn upsert(index_name: &str, id: &str, converged: &mut Converged) -> bool {
    let Some(store) = Store::background_keyed() else {
        return false;
    };
    let index = match registry::get(index_name) {
        Some(index) => index,
        None => {
            if !converged.first_time(index_name) {
                return false;
            }
            converge(index_name);
            match registry::get(index_name) {
                Some(index) => index,
                // The Map has not reached this replica yet either. Defer, and
                // let `DEFER_PATIENCE` remain the backstop it always was.
                None => return false,
            }
        }
    };
    let held = match store.hold_addr(index_name, id) {
        Ok(held) => held,
        Err(_) => return false,
    };
    let layout = index.ann.layout;
    // Not yet kept: if the vector is not readable at this address (the Map
    // has the id but not yet the value, or a corrupt read), dropping `held`
    // here releases the hold and the caller defers instead.
    let Some(bytes) = store.with_vector_at(held.addr(), layout, <[u8]>::to_vec) else {
        return false;
    };
    let addr = held.keep();
    match index.ann.add_unless_known(addr, || Ok(Some(bytes))) {
        Ok(true) => {}
        Ok(false) => index.ann.unclaimed(addr),
        Err(e) => {
            eprintln!(
                "ArcVector: '{index_name}' stored an element its graph would not take ({e}); \
                 dropping the graph so the next read rebuilds it from the Map"
            );
            index.ann.unclaimed(addr);
            registry::remove_observed(index_name, &index);
        }
    }
    true
}

/// A `vdrop` on the master, reaching this replica as an empty-id `Delete`:
/// release the graph. The Map itself is arcus's to delete and it replicates
/// that delete on its own, exactly as with a single-element `Delete`.
///
/// Through `registry::remove_if_stale` with a stamp taken before the decision,
/// not the bare `registry::remove` this used to call -- the same guard `delete`
/// below and `access::map_is_gone` already use. Without it a `vdrop` followed
/// by a `vcreate` of the same name evicts the index the `vcreate` had just
/// caused to be rebuilt: both deltas arrive in order on this one stream, but
/// the rebuild the second one starts runs on another thread and can publish
/// before this eviction gets around to running.
fn drop_index(index_name: &str) {
    let stamp = registry::now();
    if registry::remove_if_stale(index_name, stamp) {
        eprintln!("ArcVector: the master dropped index '{index_name}'; releasing its graph");
    }
}

/// Removes `id` from this index's *graph* only -- never from the Map.
///
/// arcus already replicates the delete against the Map itself; this channel
/// only says the graph should stop holding the id. That distinction is not
/// cosmetic: `take_addr` (what the master's own `vdel` uses) is
/// `map_elem_get(delete=true)`, a **write**, and a replica's
/// `ACTION_BEFORE_WRITE` gate refuses any write against a plain (non-
/// `arcus:`-prefixed) key -- every index Map is one, so on a real replica
/// that write is an unconditional no-op and the id would never leave the
/// graph.
///
/// `hold_addr` is a read (`map_elem_get(delete=false)`), which the gate
/// allows, and resolves the same address without touching the Map. What
/// removes the id from the *graph* is `AnnIndex::remove_published`, the same
/// graph-only half `vdel` runs alongside its (master-only) Map write:
/// unlinking the node and giving up the hold, so `ann.len()` stays right and
/// nothing is left for `resolve()` to later read out of a released element.
///
/// If the Map itself is gone (`StoreError::KeyGone`), the eviction uses
/// `registry::remove_if_stale` with a stamp taken before the attempt --
/// the same guard `access::map_is_gone` wraps for `vdel` -- rather than an
/// unconditional `registry::remove`, which could otherwise evict an index
/// that a concurrent rebuild had already re-published under this name.
fn delete(index_name: &str, id: &str) {
    let Some(store) = Store::background_keyed() else {
        return;
    };
    let Some(index) = registry::get(index_name) else {
        return;
    };
    let stamp = registry::now();
    let removed = index
        .ann
        .remove_published(|| match store.hold_addr(index_name, id) {
            Ok(held) => Ok(Some(held.addr())),
            Err(StoreError::ElemGone) => Ok(None),
            Err(e) => Err(e),
        });
    if let Err(PublishError::Store(StoreError::KeyGone)) = removed
        && registry::remove_if_stale(index_name, stamp)
    {
        eprintln!(
            "ArcVector: index '{index_name}' has no Map; releasing the graph it was built from"
        );
    }
}

/// Snapshots only the keys, not the whole map (which would also duplicate
/// every `Instant` and rebuild a second hash table's worth of capacity) --
/// cheaper, and necessary either way: `pending.resolved` below needs `&mut
/// pending`, so nothing can still be borrowing out of `pending.waiting`
/// while this loop runs.
fn retry_deferred(pending: &mut Pending) {
    let candidates: Vec<(String, String)> = pending.waiting.keys().cloned().collect();
    // Shared across the whole pass, unlike `apply`'s: `upsert` converges a
    // name the registry misses, and without this a deferred set holding many
    // ids of one still-unknown index would run that rebuild attempt once per
    // id, every `RETRY_THROTTLE`, on the socket-reading thread.
    let mut converged = Converged::default();
    for (index, id) in candidates {
        if upsert(&index, &id, &mut converged) {
            pending.resolved(&index, &id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot `repl::ensure_slave_loop` re-arms the connect loop through:
    /// exactly one holder at a time, and reusable once given back, which is
    /// what makes a failed spawn (or a fork, or a panicked loop) recoverable
    /// on a later beat instead of permanently silent. Nothing else in this
    /// crate's tests touches `LOOP_LIVE`.
    #[test]
    fn only_one_connect_loop_can_hold_the_slot() {
        assert!(claim_loop_slot(), "nothing holds it yet");
        assert!(!claim_loop_slot(), "a second loop must not start");
        release_loop_slot();
        assert!(claim_loop_slot(), "and the slot is reusable afterwards");
        release_loop_slot();
    }

    /// A redirect is followed, but only so far. Two nodes with stale views
    /// can name each other, and this thread must not spin between them.
    #[test]
    fn following_redirects_is_bounded() {
        assert_eq!(MAX_REDIRECTS, 3, "a cycle must cost a bounded round");
    }

    /// An empty `master` is "I do not know", not "nobody". `session` turns it
    /// into `None` so the next round re-reads the owner key and polls, rather
    /// than treating the empty string as an address to dial.
    #[test]
    fn a_redirect_that_names_nobody_is_not_an_address() {
        let named = Some("10.0.0.2:7654".to_owned()).filter(|m: &String| !m.is_empty());
        let unknown = Some(String::new()).filter(|m: &String| !m.is_empty());
        assert_eq!(named.as_deref(), Some("10.0.0.2:7654"));
        assert_eq!(unknown, None);
    }

    /// The memo `upsert`'s converge-on-miss is bounded by. Its only job is to
    /// answer "first time" once per name per pass; without it a deferred set
    /// holding `MAX_DEFERRED` ids of one unknown index would run that many
    /// rebuild attempts every `RETRY_THROTTLE`.
    #[test]
    fn one_pass_converges_a_name_once_however_many_entries_name_it() {
        let mut converged = Converged::default();
        assert!(converged.first_time("idx"), "nothing has been tried yet");
        assert!(!converged.first_time("idx"), "already tried this pass");
        assert!(
            converged.first_time("other"),
            "a different index is its own"
        );
        assert!(!converged.first_time("other"));
    }

    #[test]
    fn a_deferred_upsert_is_retried_and_then_forgotten() {
        let mut pending = Pending::default();
        pending.defer("idx".to_owned(), "v1".to_owned());
        assert_eq!(pending.len(), 1);
        pending.resolved("idx", "v1");
        assert_eq!(pending.len(), 0, "the Map caught up");
    }

    #[test]
    fn the_deferred_set_has_a_bound_that_forces_a_resync() {
        let mut pending = Pending::default();
        for i in 0..MAX_DEFERRED {
            pending.defer("idx".to_owned(), format!("v{i}"));
        }
        assert!(!pending.overflowed(), "at the bound, not past it");
        pending.defer("idx".to_owned(), "one-too-many".to_owned());
        assert!(
            pending.overflowed(),
            "deferred work is lag that has not reached the socket; \
             past the bound it would hide how far behind we are"
        );
    }

    #[test]
    fn the_indexes_needing_resync_are_named() {
        let mut pending = Pending::default();
        pending.defer("a".to_owned(), "v1".to_owned());
        pending.defer("b".to_owned(), "v2".to_owned());
        let mut names = pending.indexes();
        names.sort();
        assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);
    }

    /// Pins invariant 2 for the entry, not the container: a count that never
    /// reaches `MAX_DEFERRED` can still hide one id that never resolves, and
    /// patience is what still surfaces it. `expired` takes `now` as an
    /// argument rather than reading the clock itself, precisely so this is
    /// observable without an actual 30-second wait.
    #[test]
    fn a_deferred_entry_that_resolves_is_forgotten_and_one_that_never_resolves_forces_its_index_to_resync()
     {
        let mut pending = Pending::default();
        pending.defer("a".to_owned(), "v1".to_owned());
        pending.defer("b".to_owned(), "v2".to_owned());

        pending.resolved("a", "v1");
        assert_eq!(
            pending.indexes(),
            vec!["b".to_owned()],
            "a resolved and is forgotten"
        );

        let before_patience = Instant::now();
        assert!(
            pending.expired(before_patience).is_empty(),
            "not yet: patience has not run out"
        );

        let past_patience = before_patience + DEFER_PATIENCE + Duration::from_secs(1);
        assert_eq!(
            pending.expired(past_patience),
            vec!["b".to_owned()],
            "b never resolved, so its index is named for resync"
        );
    }

    /// Pins the fix for the bound dropping the very entry that trips it: an
    /// index with nothing else pending must still be named by the remedy
    /// `overflowed()` triggers, or its update is discarded with no resync to
    /// show for it.
    #[test]
    fn an_index_whose_only_deferred_entry_trips_the_bound_is_still_named_for_resync() {
        let mut pending = Pending::default();
        for i in 0..MAX_DEFERRED {
            pending.defer("a".to_owned(), format!("v{i}"));
        }
        assert!(!pending.overflowed());

        // "b" has never been deferred before; this call alone trips the
        // bound, so its entry is the one dropped.
        pending.defer("b".to_owned(), "v0".to_owned());
        assert!(pending.overflowed());
        assert!(
            pending.overflow_indexes().contains(&"b".to_owned()),
            "b's entry never made it into the waiting set, but it must still \
             be named or its dropped update is silently lost"
        );
    }

    /// `overflow_indexes` is a union: an index can be named both because it
    /// still has entries waiting and because a separate entry of its own
    /// tripped the bound. Either way it must appear exactly once.
    #[test]
    fn overflow_indexes_is_the_union_of_what_is_waiting_and_what_was_dropped() {
        let mut pending = Pending::default();
        pending.defer("a".to_owned(), "v1".to_owned());
        for i in 0..MAX_DEFERRED - 1 {
            pending.defer("filler".to_owned(), format!("v{i}"));
        }
        assert!(!pending.overflowed());
        // Trips the bound against "a" again -- "a" already has an entry
        // waiting, so this checks the union does not double-list it.
        pending.defer("a".to_owned(), "v2".to_owned());
        assert!(pending.overflowed());

        let mut names = pending.overflow_indexes();
        names.sort();
        names.dedup();
        assert_eq!(
            pending.overflow_indexes(),
            names,
            "no duplicate listing of an index named both ways"
        );
        assert!(pending.overflow_indexes().contains(&"a".to_owned()));
        assert!(pending.overflow_indexes().contains(&"filler".to_owned()));
    }

    /// Pins the decision `session`'s overflow branch now makes through
    /// `take_overflowed`: it names via the union (catching the entry that
    /// tripped the bound, as in the test above), and naming and discarding
    /// happen in the same call -- there is no way to read the names back out
    /// afterward and find some of them still silently waiting, nor to find
    /// the set cleared without ever having named the index that was lost.
    #[test]
    fn taking_the_overflow_names_every_affected_index_and_clears_the_whole_set() {
        let mut pending = Pending::default();
        for i in 0..MAX_DEFERRED {
            pending.defer("a".to_owned(), format!("v{i}"));
        }
        pending.defer("b".to_owned(), "v0".to_owned());
        assert!(pending.overflowed());

        let mut affected = pending.take_overflowed();
        affected.sort();
        assert_eq!(
            affected,
            vec!["a".to_owned(), "b".to_owned()],
            "both the index still waiting and the one whose entry was dropped"
        );
        assert_eq!(pending.len(), 0, "every entry is given up on, not just b's");
        assert!(
            !pending.overflowed(),
            "the reset is complete, not just the entries"
        );
        assert!(pending.overflow_indexes().is_empty());
    }

    /// Pins the same split for the patience remedy: `take_expired` names
    /// only what is actually past `DEFER_PATIENCE`, and forgets exactly the
    /// entries it names -- an entry that resolved on its own before
    /// patience ran out is neither named nor counted against the ones that
    /// are (real rebuilds need an engine this test does not have, but which
    /// entries survive this call does not).
    #[test]
    fn taking_the_expired_entries_names_only_those_and_forgets_only_those() {
        let mut pending = Pending::default();
        pending.defer("stale".to_owned(), "v1".to_owned());
        pending.defer("fresh".to_owned(), "v2".to_owned());

        let now = Instant::now();
        assert!(
            pending.take_expired(now).is_empty(),
            "neither has waited past patience yet"
        );
        assert_eq!(pending.len(), 2, "nothing named, so nothing forgotten");

        // The Map catches up for "fresh" before patience runs out; "stale"
        // never resolves.
        pending.resolved("fresh", "v2");

        let past_patience = now + DEFER_PATIENCE + Duration::from_secs(1);
        assert_eq!(
            pending.take_expired(past_patience),
            vec!["stale".to_owned()],
            "fresh already left the set by resolving; stale never did"
        );
        assert_eq!(
            pending.len(),
            0,
            "stale's entry is forgotten along with being named"
        );
    }

    /// The frame-driven retry's rate limit, pinned without a live socket or
    /// engine: never yet run, it is due immediately; just run, it is not due
    /// again until `RETRY_THROTTLE` has actually passed.
    #[test]
    fn the_frame_driven_retry_is_throttled_but_not_forever() {
        let now = Instant::now();
        assert!(should_retry(None, now), "never retried yet");
        assert!(!should_retry(Some(now), now), "too soon");
        assert!(
            !should_retry(Some(now), now + RETRY_THROTTLE / 2),
            "still too soon"
        );
        assert!(
            should_retry(Some(now), now + RETRY_THROTTLE),
            "enough time has passed"
        );
    }

    /// The exact three-way mapping the review asked for, and the one this
    /// task's two prior rounds each got wrong in a different way: round 1
    /// claimed success whenever the index was live and untouched (never
    /// `COLD`, so `fill` was never even called); round 2 claimed success
    /// even when `fill` itself returned `Err`. Both are `false` here, and
    /// this needs neither a `Store` nor a live index to check.
    #[test]
    fn a_rebuild_only_counts_as_started_when_it_landed_cold_and_fill_actually_ran() {
        assert!(
            !rebuild_started::<()>(None),
            "never reached COLD -- fill was never even called"
        );
        assert!(
            !rebuild_started(Some(&Err(()))),
            "COLD, but fill itself could not start a build"
        );
        assert!(
            rebuild_started::<()>(Some(&Ok(()))),
            "COLD, and fill actually started a build"
        );
    }
}
