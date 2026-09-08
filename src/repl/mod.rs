pub mod master;
pub mod role;
pub mod slave;
pub mod wire;

pub use wire::Op;

/// Starts replication. Idempotent, and safe to call before the daemon has a
/// listening socket of its own: neither the heartbeat nor the slave loop
/// waits for one. `role::beat` acquires the master listener itself, lazily,
/// on whichever beat first needs it (`role::acquire_listener`) -- `start`
/// does not open it eagerly the way an earlier version of this function did.
///
/// That is a deliberate narrowing, not just a simplification, of the same
/// fork hazard `after_fork_in_child` (below) exists for. `start` runs
/// synchronously on the daemon's own init thread, strictly before `-d`'s
/// `fork()` -- so anything it does here runs guaranteed-once, never racing
/// the fork itself. Opening a real listener and resolving `advertised`
/// (`role::acquire_listener`'s job now) can mean binding a socket, spawning
/// a thread, and running the address fallback chain; keeping all of that
/// out of `start` means it can only ever happen from within a background
/// thread's own normal execution, not from the synchronous call the daemon
/// itself is waiting on. That leaves exactly one place that still touches
/// `role::State`'s `published`, `master`'s `LISTEN_ADDR`, or
/// `role::cached_advertised`'s own cache before any fork could possibly have
/// happened: the heartbeat thread's own first beat, spawned by
/// `role::start_heartbeat` just below. `role::spawn_heartbeat` sleeps one
/// full `HEARTBEAT` before that first beat for exactly this reason -- see
/// its own doc for what that does and does not close.
pub fn start() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    unsafe { pthread_atfork(None, None, Some(after_fork_in_child)) };
    role::start_heartbeat();
    ensure_slave_loop();
}

/// Opens the master listener and publishes its address, logging rather than
/// propagating a bind failure. Called only from `restart_after_fork` (a
/// background thread's ordinary execution, well past any fork) -- never
/// from `start` itself; see `start`'s doc for why, and `after_fork_in_child`
/// for why it is safe here even though it was not safe to inline directly
/// into the fork handler.
fn open_listener() {
    match master::open() {
        Ok(bound) => role::shared().set_listen_addr(Some(role::advertised(bound))),
        Err(e) => eprintln!(
            "ArcVector: replication listener not opened ({e}); the heartbeat will retry it"
        ),
    }
}

/// Starts the slave's connect loop if one is not already running, and says so
/// if it cannot. Idempotent, and cheap enough (one atomic compare-exchange in
/// the common case) to call from every beat.
///
/// The listener half has always had a lazy re-arm: `role::beat` calls
/// `master::open` on every beat it holds `Role::Master`, so a listener that
/// failed to open, or that a fork took away, comes back on its own. The slave
/// half had none. `start` and `restart_after_fork` both spawned it with
/// `let _ = ..`, so a failed spawn was silent, and nothing ever tried again --
/// leaving a node that believes it is a replica with nothing dialling the
/// master, which looks exactly like a healthy replica whose master has no
/// updates. `role::beat` now calls this whenever it holds `Role::Replica`,
/// which gives the slave the same self-healing the listener has.
pub(crate) fn ensure_slave_loop() {
    if !slave::claim_loop_slot() {
        return; // One is already running.
    }
    if let Err(e) = std::thread::Builder::new()
        .name("arcvector-repl-slave".to_owned())
        .spawn(slave::connect_loop)
    {
        slave::release_loop_slot();
        eprintln!(
            "ArcVector: could not start the replication slave loop ({e}); \
             a later heartbeat retries this while this node is a replica"
        );
    }
}

/// `-d` forks after the extensions load, and only the forking thread
/// survives into the daemon -- `role::start_heartbeat`'s doc covers the
/// general shape of this problem, which its own atfork hook already solves
/// for the heartbeat thread alone. This hook covers the other two: the
/// acceptor thread `master::open` spawned, and the slave's `connect_loop`
/// thread, both gone in the child even though the memory they left behind
/// (`master`'s `LISTENING`/`LISTEN_ADDR`, and the address this crate
/// published through `role`) survives the fork unchanged.
///
/// **This function itself must do nothing but spawn a thread.** POSIX
/// allows a fork handler running in the child to call only async-signal-safe
/// functions: every other thread's state -- including any lock any of them
/// held -- is frozen exactly as it was at the instant of `fork()`, with no
/// thread left alive to ever release it. `master::close` takes a `Mutex`;
/// `open_listener` takes another one, allocates, and calls `pthread_create`;
/// `open_listener` -> `role::advertised` binds a socket. All of that is fine
/// once running
/// inside a normal thread's own context, which is exactly where
/// `restart_after_fork` runs it -- but calling any of it directly here,
/// synchronously inside the handler, risks deadlocking on a lock `role`'s
/// own heartbeat thread happened to be holding at the instant of `fork` --
/// its first beat is spawned before any fork can happen at all (see
/// `start`'s doc), which is exactly the window `role::spawn_heartbeat`'s own
/// pre-first-beat sleep now narrows. `src/attach.rs` already spawns a
/// thread directly from its own child handler with nothing else around it;
/// this matches that shape rather than the version of this function from
/// two rounds ago, which called `master::close`/`open_listener` here
/// directly and was wrong to.
///
/// A failed spawn here used to be silently swallowed (`let _ = ...`), which
/// left the child with `LISTENING` frozen `true` over a dead, inherited
/// listener forever -- no restart thread ever ran to reset it, so a later
/// `open()` would be CAS-refused back to that same dead address for the
/// rest of the process's life, with no output anywhere saying so. This
/// branch cannot do what `restart_after_fork` does (that needs the very
/// locks this handler must not take), but `LISTENING` itself is a plain
/// atomic, and clearing just that -- see `master::
/// reset_after_unstarted_fork_restart` -- is enough to let some later
/// `beat` reopen for real instead of staying wrong forever. `eprintln!` is
/// not on the strict async-signal-safe list either (it locks `Stderr`
/// internally), but silence is the one outcome this round's review ruled
/// out outright, and a thread-spawn failure this early is already a sign
/// of a process in serious trouble (fd or memory exhaustion) where a small
/// added risk of also blocking here is accepted over certainly saying
/// nothing at all.
extern "C" fn after_fork_in_child() {
    if std::thread::Builder::new()
        .name("arcvector-repl-fork-restart".to_owned())
        .spawn(restart_after_fork)
        .is_err()
    {
        master::reset_after_unstarted_fork_restart();
        // Same shape, for the slave half: the flag is copied `true` out of the
        // parent, whose loop thread did not survive the fork, so without this
        // `ensure_slave_loop` would refuse to start one for the rest of the
        // process's life. A plain atomic store, which is all this handler may
        // do -- the spawn itself is left to whichever beat next finds this
        // node a replica.
        slave::release_loop_slot();
        eprintln!(
            "ArcVector: could not restart replication after fork; the feature is down \
             until a later beat reopens its listener on its own"
        );
    }
}

/// The actual restart, moved out of the fork handler above into a normal
/// thread's own execution, where locking and allocating are fine.
///
/// `master::close` is reused as the reset, not written fresh: it already
/// does exactly what a restart needs, flipping the CAS-guarded `LISTENING`
/// back to `false` so the following `open` is a real bind rather than a
/// refused one, and dropping every `Replica` whose writer thread is equally
/// gone. Without it, `LISTENING` reads `true` in the child forever -- copied
/// memory, no thread left to have set it -- so a later `open` (from a
/// promotion, say) is CAS-refused and hands back the address of a listener
/// nothing is accepting on any more; a replica's `connect` to that address
/// completes into the kernel's backlog and simply hangs. `open_listener`
/// then reopens for real right away, rather than leaving that to whichever
/// beat happens to run next -- there is no more fork to race by this point,
/// so there is no reason to defer it the way `start` now does.
fn restart_after_fork() {
    master::close();
    open_listener();
    // The slot is still held by the parent's loop thread, which did not
    // survive the fork; hand it back before asking for a new loop, or the
    // claim below refuses and the child never dials anyone.
    slave::release_loop_slot();
    ensure_slave_loop();
}

unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> std::os::raw::c_int;
}

/// Tells the replicas an id changed. Costs one atomic load when this node is
/// not a master or has no replicas, which is every call on most deployments.
///
/// Called after the engine write has committed, never before: a replica that
/// resolves the id against its own Map must be able to find something there.
pub fn publish(index: &str, id: &str, op: Op) {
    if role::shared().role() != role::Role::Master {
        return;
    }
    master::publish(index, id, op);
}
