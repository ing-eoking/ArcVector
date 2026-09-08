//! The master side of replication: a TCP listener, one writer thread per
//! connected replica, and the bounded channel whose overflow is how the
//! master learns a replica has fallen behind.
//!
//! Publishing (`publish`) runs on a request thread and must never block and
//! never fail -- that is what `try_send` buys. A full channel is not merely
//! backpressure: it is the definition of "this replica has fallen behind".
//! The chain that gets there is TCP itself -- a replica that applies slowly
//! stops reading, its receive window closes, the writer's blocking write
//! times out (`WRITE_TIMEOUT`), the channel behind it fills, and `try_send`
//! returns `Full`. State is kept per replica, not shared, so one slow peer
//! fills only its own queue and never touches another's.
//!
//! Ownership: `replicas()` is the *sole* strong owner of every `Replica`. A
//! writer thread (`serve`) holds only a `Weak`, and the channel `Receiver`
//! it takes out of the `Replica` on the way in. That is what lets a
//! reconnect or a `close()` actually stop an old writer: dropping the
//! list's `Arc` drops the `Replica`, which drops the channel's sender, which
//! turns the writer's blocking `rx.recv()` into `Err` once whatever was
//! already queued has drained. A writer that instead kept its own strong
//! `Arc` (the previous bug here) would keep the `Replica` alive forever --
//! parked in `recv()`, not socket I/O, it never sees the peer's RST, and
//! nothing else would ever drop the sender for it.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use super::wire::{self, Msg, Op, PROTO_VER};
use crate::handler::registry;

/// Deep enough to ride out a scheduling hiccup, shallow enough that a replica
/// that has genuinely stopped is noticed rather than buffered indefinitely.
const DEPTH: usize = 1024;

/// A wedged peer must not hold its writer thread forever while its channel
/// silently fills.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection that does not say SYNC is not a replica.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);

/// Where `accept_loop`'s backoff starts, and how far it is let to grow, on a
/// persistent `accept` error (EMFILE is exactly that): enough to stop a
/// busy-spin at 100% CPU inside someone else's daemon, not so much that a
/// transient blip goes unnoticed for long.
const ACCEPT_BACKOFF_START: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

static FULL_EVENTS: AtomicUsize = AtomicUsize::new(0);
static RESYNCS_SENT: AtomicUsize = AtomicUsize::new(0);

pub struct Replica {
    pub node_id: String,
    tx: SyncSender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    depth: AtomicUsize,
    resync: AtomicBool,
}

impl Replica {
    pub fn new(node_id: String, depth: usize) -> Self {
        let (tx, rx) = sync_channel(depth);
        Self {
            node_id,
            tx,
            rx: Mutex::new(Some(rx)),
            depth: AtomicUsize::new(0),
            resync: AtomicBool::new(false),
        }
    }

    /// Queues a message, or marks this replica behind. Never blocks: publishing
    /// runs on a request thread.
    ///
    /// The depth counter is incremented *before* `try_send`, not after: the
    /// writer thread's matching `fetch_sub` runs only once it has actually
    /// received a message, which can only happen after `try_send` placed it
    /// in the channel, which can only happen after this increment. Bumping
    /// the counter after the send instead would let the writer's decrement
    /// win the race on a fast enough peer, underflowing an `AtomicUsize`
    /// into a number nowhere near the truth.
    pub fn offer(&self, msg: Msg) -> bool {
        if self.resync.load(Ordering::Acquire) {
            return false;
        }
        self.depth.fetch_add(1, Ordering::AcqRel);
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.depth.fetch_sub(1, Ordering::AcqRel);
                FULL_EVENTS.fetch_add(1, Ordering::Relaxed);
                self.mark_behind();
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.depth.fetch_sub(1, Ordering::AcqRel);
                false
            }
        }
    }

    /// Marks the replica for a fresh `SNAPSHOT`. This does not drain the
    /// queue: by the time a writer thread exists it has already taken `rx`
    /// out of the `Mutex` (see `take_receiver`), so a drain through `self.rx`
    /// here would find `None` and do nothing. Nothing depends on an early
    /// drain anyway -- the writer thread empties the channel itself through
    /// `rx.recv()`, and `offer`'s check of `resync` above already refuses
    /// every message from here on. The flag alone is enough to make "behind"
    /// stick; do not reach for the drain again.
    fn mark_behind(&self) {
        self.resync.store(true, Ordering::Release);
    }

    pub fn needs_resync(&self) -> bool {
        self.resync.load(Ordering::Acquire)
    }

    /// Reports the flag once, clearing it, so one fall-behind sends one RESYNC.
    pub fn take_resync_flag(&self) -> bool {
        self.resync.swap(false, Ordering::AcqRel)
    }

    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Acquire)
    }

    fn take_receiver(&self) -> Option<Receiver<Msg>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

fn replicas() -> &'static Mutex<Vec<Arc<Replica>>> {
    static REPLICAS: OnceLock<Mutex<Vec<Arc<Replica>>>> = OnceLock::new();
    REPLICAS.get_or_init(|| Mutex::new(Vec::new()))
}

static LISTENING: AtomicBool = AtomicBool::new(false);

/// The address `open` last bound, kept so `close` can connect to itself to
/// unblock the acceptor's blocking `accept()`, and so a second `open` can
/// hand back the address already in use instead of binding another one.
static LISTEN_ADDR: Mutex<Option<SocketAddr>> = Mutex::new(None);

pub fn publish(index: &str, id: &str, op: Op) {
    if !LISTENING.load(Ordering::Acquire) {
        return;
    }
    let list = replicas().lock().unwrap_or_else(|e| e.into_inner());
    for replica in list.iter() {
        // A replica already marked behind refuses every `offer` regardless,
        // so building the message for it would only be a wasted allocation.
        if replica.needs_resync() {
            continue;
        }
        // `String::to_owned` allocates infallibly and aborts the process on
        // exhaustion -- exactly what this project's `try_reserve` rule
        // exists to avoid (see `wire.rs`, `registry.rs`). One replica that
        // cannot afford a copy of this delta is treated the same as one
        // whose channel is full: it is marked behind rather than silently
        // missing an update with no signal at all.
        match (try_to_owned(index), try_to_owned(id)) {
            (Some(index), Some(id)) => {
                replica.offer(Msg::Delta { index, id, op });
            }
            _ => replica.mark_behind(),
        }
    }
}

/// `String::to_owned`, but a failed allocation returns `None` instead of
/// aborting the process.
fn try_to_owned(s: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve(s.len()).ok()?;
    out.push_str(s);
    Some(out)
}

pub fn queued() -> usize {
    replicas()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|r| r.depth())
        .sum()
}

pub fn full_events() -> usize {
    FULL_EVENTS.load(Ordering::Relaxed)
}

pub fn resyncs_sent() -> usize {
    RESYNCS_SENT.load(Ordering::Relaxed)
}

/// Idempotent: a second call while already listening is a no-op that hands
/// back the address already bound, rather than binding a second ephemeral
/// port and spawning a second acceptor that orphans the first.
pub fn open() -> std::io::Result<SocketAddr> {
    if LISTENING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return LISTEN_ADDR
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "ArcVector replication listener is already open",
                )
            });
    }
    let listener = TcpListener::bind("0.0.0.0:0")?;
    let addr = listener.local_addr()?;
    *LISTEN_ADDR.lock().unwrap_or_else(|e| e.into_inner()) = Some(addr);
    let spawned = std::thread::Builder::new()
        .name("arcvector-repl-accept".to_owned())
        .spawn(move || accept_loop(&listener));
    if let Err(e) = spawned {
        // Roll the guard back so a later `open` is not wedged forever
        // believing a listener exists that never got its acceptor.
        LISTENING.store(false, Ordering::Release);
        *LISTEN_ADDR.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(e);
    }
    Ok(addr)
}

/// Stops the acceptor and forgets every replica. Safe to call repeatedly --
/// a later task calls this every time the node is demoted, so it must not
/// misbehave on a `close` that finds nothing open.
///
/// The acceptor is parked in a blocking `accept()` that only re-checks
/// `LISTENING` once a connection arrives, so clearing the flag alone would
/// leave the port bound and accepting until some other connection happened
/// to wake it. Connecting to our own just-vacated address is the standard
/// way to unblock a blocking `accept()` from another thread.
pub fn close() {
    if LISTENING.swap(false, Ordering::AcqRel)
        && let Some(addr) = LISTEN_ADDR.lock().unwrap_or_else(|e| e.into_inner()).take()
    {
        let wake = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port());
        let _ = TcpStream::connect(wake);
    }
    replicas().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A lock-free partial reset, for exactly one caller:
/// `repl::mod::after_fork_in_child`, and only when spawning the real restart
/// thread (which calls the ordinary, locking `close()` above) itself
/// failed. A fork handler may touch atomics and may at most spawn a thread
/// -- nothing else -- so this cannot take `LISTEN_ADDR`'s or `replicas()`'s
/// `Mutex` the way `close()` does; it clears only the atomic guard.
///
/// That is still enough for the node to recover on its own, rather than
/// staying permanently wrong: `LISTENING` reads `true` in the child forever
/// otherwise -- copied memory, no thread left to have set it, and now no
/// restart thread either to reset it the normal way -- so a later `open()`
/// (whenever some beat next gets around to calling one) would be
/// CAS-refused and hand back the address of a listener nothing is accepting
/// on any more, forever. With `LISTENING` cleared, that same later `open()`
/// does a real bind instead, and simply overwrites the stale `LISTEN_ADDR`
/// once it succeeds -- the same as any other reopen. `replicas()` is left
/// as-is (whatever `Replica` entries were copied from the parent go stale,
/// their writer threads equally gone in the fork, but nothing dereferences
/// them incorrectly as a result -- the next real `close()`, from an actual
/// demotion, clears the list the normal way).
pub fn reset_after_unstarted_fork_restart() {
    LISTENING.store(false, Ordering::Release);
}

/// Serializes every test in this crate that calls the real `open`/`close`
/// against every other one, held for the duration of the caller's block.
///
/// `LISTENING`/`LISTEN_ADDR` are process-global, and `open` has a narrow
/// window between its `LISTENING` CAS succeeding and `LISTEN_ADDR` actually
/// being set (a real `TcpListener::bind` happens in between) during which a
/// concurrent `open` elsewhere sees "already listening" but finds no address
/// yet and returns `Err`. That gap is not this lock's to close -- it is a
/// pre-existing property of `open` itself, reachable in production only
/// across two threads racing a promotion, and left alone here per review --
/// but `cargo test` runs every `#[test]` fn concurrently in one process, and
/// without this, this module's own reopen test and `role::tests`'s
/// heartbeat test (which calls `open` indirectly through `acquire_listener`)
/// hit that exact window against each other on every run, not just rarely.
#[cfg(test)]
pub(crate) fn serialize_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn accept_loop(listener: &TcpListener) {
    let mut backoff = ACCEPT_BACKOFF_START;
    while LISTENING.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((sock, _)) => {
                backoff = ACCEPT_BACKOFF_START;
                if !LISTENING.load(Ordering::Acquire) {
                    // Woken by close()'s self-connect, not a real replica.
                    break;
                }
                // `Builder::spawn`, not the panicking `std::thread::spawn`:
                // under thread or fd exhaustion a panic here would unwind
                // *this* acceptor thread, silently killing the listener
                // while `LISTENING` stays true.
                if let Err(e) = std::thread::Builder::new()
                    .name("arcvector-repl-writer".to_owned())
                    .spawn(move || {
                        if let Some(replica) = handshake(&sock) {
                            serve(sock, replica);
                        }
                    })
                {
                    eprintln!("ArcVector: could not spawn a replica writer thread: {e}");
                }
            }
            Err(_) if !LISTENING.load(Ordering::Acquire) => break,
            Err(_) => {
                // A persistent accept error (EMFILE is exactly that) must
                // not spin at 100% CPU inside someone else's daemon.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
            }
        }
    }
}

/// Reads the replica's SYNC. A connection that says nothing within the timeout
/// is dropped: it is not a replica, whatever else it may be.
fn handshake(sock: &TcpStream) -> Option<Weak<Replica>> {
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
    let mut reader = sock.try_clone().ok()?;
    let frame = wire::read_frame(&mut reader).ok()?;
    let Ok(Msg::Sync {
        node_id, proto_ver, ..
    }) = wire::decode(&frame)
    else {
        return None;
    };
    if proto_ver != PROTO_VER {
        eprintln!(
            "ArcVector: refusing replica {node_id}: protocol {proto_ver}, we speak {PROTO_VER}"
        );
        return None;
    }

    let replica = Arc::new(Replica::new(node_id.clone(), DEPTH));
    let weak = Arc::downgrade(&replica);
    let mut list = replicas().lock().unwrap_or_else(|e| e.into_inner());
    // A reconnect from the same node drops its old handle from the list.
    // `serve` holds only a `Weak`, so if this was the old `Replica`'s last
    // strong reference, dropping it here drops the `Replica` itself -- and
    // with it the channel's sender, which is what turns the old writer
    // thread's `rx.recv()` into `Err` once it has drained whatever was
    // already queued, letting that thread exit instead of leaking forever.
    list.retain(|r| r.node_id != node_id);
    if list.try_reserve(1).is_err() {
        return None;
    }
    list.push(replica);
    Some(weak)
}

/// Runs for the life of one connection, then removes this replica's handle
/// from `replicas()` -- the one cleanup every exit path in `run` funnels
/// into, so a failed initial snapshot leaves no more of a stale entry than a
/// failed `send` does.
fn serve(mut sock: TcpStream, replica: Weak<Replica>) {
    let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = sock.set_nodelay(true);

    run(&mut sock, &replica);

    if let Some(strong) = replica.upgrade() {
        replicas()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|r| !Arc::ptr_eq(r, &strong));
    }
}

/// The body of `serve`. Returns on the first failure of any kind -- a
/// `Replica` already gone, a snapshot that could not be built, a `send`
/// that failed -- so `serve` has exactly one place to run its cleanup
/// rather than each early return needing its own copy.
///
/// `replica` is upgraded fresh each time a field on it needs touching, and
/// the resulting strong reference is never held across the blocking
/// `rx.recv()`: that is what lets `replicas()` dropping its `Arc` (a
/// reconnect, or `close`) actually end this thread instead of being held
/// open by the very thread it is trying to stop.
fn run(sock: &mut TcpStream, replica: &Weak<Replica>) -> Option<()> {
    let rx = replica.upgrade()?.take_receiver()?;

    let snap = snapshot()?;
    send(sock, &snap).ok()?;

    loop {
        let msg = rx.recv().ok()?;
        let strong = replica.upgrade();
        if let Some(s) = &strong {
            s.depth.fetch_sub(1, Ordering::AcqRel);
        }
        send(sock, &msg).ok()?;
        if let Some(s) = &strong
            && s.take_resync_flag()
        {
            RESYNCS_SENT.fetch_add(1, Ordering::Relaxed);
            send(
                sock,
                &Msg::Resync {
                    index: String::new(),
                },
            )
            .ok()?;
        }
    }
}

/// The names only. A replica resolves the vectors from its own Map, which
/// arcus has already replicated; it cannot derive the names, because its
/// registry is empty at startup and nothing enumerates arcus keys.
///
/// Returns `None` rather than a `Snapshot` naming zero indexes when the
/// listing could not be built under memory pressure: on the wire, empty
/// means "the master has no indexes," and a replica that believed that
/// would act on it. A connection that cannot be given an honest snapshot is
/// not served a dishonest one -- the caller drops it instead.
fn snapshot() -> Option<Msg> {
    let indexes = registry::indexes().ok()?;
    let mut names = Vec::new();
    names.try_reserve(indexes.len()).ok()?;
    for index in &indexes {
        names.push(try_to_owned(&index.name)?);
    }
    Some(Msg::Snapshot { indexes: names })
}

fn send(sock: &mut TcpStream, msg: &Msg) -> std::io::Result<()> {
    sock.write_all(&wire::encode(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replica_that_stops_reading_is_marked_for_resync_once() {
        let replica = Replica::new("node-a".to_owned(), 4);
        for i in 0..4 {
            assert!(replica.offer(delta(i)), "within capacity");
        }
        assert!(!replica.offer(delta(4)), "capacity is spent");
        assert!(
            replica.needs_resync(),
            "and that is what falling behind means"
        );
        assert!(!replica.offer(delta(5)), "still refused");
        assert!(replica.take_resync_flag(), "reported exactly once");
        assert!(!replica.take_resync_flag());
    }

    #[test]
    fn one_slow_replica_does_not_affect_another() {
        let slow = Replica::new("slow".to_owned(), 1);
        let fast = Replica::new("fast".to_owned(), 8);
        slow.offer(delta(0));
        slow.offer(delta(1));
        assert!(slow.needs_resync());
        assert!(fast.offer(delta(0)), "its own capacity is untouched");
        assert!(!fast.needs_resync());
    }

    /// Pins the property this task exists for: once a replica has fallen
    /// behind, what keeps refusing it is the `resync` flag, not merely a
    /// full channel. Draining the channel by hand and trying again is what
    /// tells the two apart -- without the flag, a drained channel would
    /// have room and `offer` would go straight back to `try_send` and
    /// succeed, which must not happen here.
    #[test]
    fn a_replica_marked_behind_stays_refused_even_after_its_queue_drains() {
        let replica = Replica::new("node-a".to_owned(), 1);
        assert!(replica.offer(delta(0)), "within capacity");
        assert!(!replica.offer(delta(1)), "capacity is spent, now behind");

        let rx = replica.take_receiver().expect("nothing has claimed it yet");
        while rx.try_recv().is_ok() {}

        assert!(
            !replica.offer(delta(2)),
            "still refused: marked behind, not just a channel that happened to be full"
        );
        assert!(replica.take_resync_flag(), "one event, one flag");
        assert!(!replica.take_resync_flag(), "not reported twice");
    }

    fn delta(n: usize) -> crate::repl::wire::Msg {
        crate::repl::wire::Msg::Delta {
            index: "idx".to_owned(),
            id: format!("v{n}"),
            op: crate::repl::wire::Op::Upsert,
        }
    }

    /// Pins the property a node promoted a second time depends on:
    /// `close` does not just flip a flag, it actually lets a later `open`
    /// bind a fresh listener rather than being CAS-refused back to whatever
    /// was open before. Needs no engine -- `open`/`close` only ever touch a
    /// real `TcpListener` and this module's own statics -- but does share
    /// `LISTENING`/`LISTEN_ADDR` with every other test in this binary that
    /// touches them; ending on `close()` leaves that shared state as it
    /// found it (not listening) for whatever runs next.
    #[test]
    fn a_listener_reopened_after_a_close_binds_a_different_port() {
        let _guard = serialize_tests();
        let first = open().expect("binding an ephemeral port does not fail");
        close();
        let second = open().expect("closing must let a later open bind again");
        assert_ne!(
            first, second,
            "a fresh bind gets a fresh ephemeral port, not the one just closed"
        );
        close();
    }

    /// The same shape as the reopen test above, but through the lock-free
    /// reset `repl::mod`'s fork handler falls back to when it could not
    /// even spawn the real restart thread (see that module's doc). Needs no
    /// fork: `reset_after_unstarted_fork_restart` is a bare atomic store,
    /// and this pins exactly the property that store exists for -- that a
    /// later `open` sees `LISTENING == false` and does a real bind, rather
    /// than being CAS-refused back to the first (by then dead) address.
    /// Revert the remedy to a no-op and the second `open` hands back the
    /// first address unchanged, failing the `assert_ne!` below.
    #[test]
    fn the_lock_free_reset_lets_a_later_open_bind_a_different_port_too() {
        let _guard = serialize_tests();
        let first = open().expect("binding an ephemeral port does not fail");
        reset_after_unstarted_fork_restart();
        let second = open().expect("the reset must let a later open bind again");
        assert_ne!(
            first, second,
            "a fresh bind gets a fresh ephemeral port, not the one just reset"
        );
        close();
    }
}
