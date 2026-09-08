use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::handler::arcus::engine::{Store, StoreError};

/// What this node is, as far as the last probe could tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// No probe has answered yet. Neither a master nor a replica acts.
    Unknown,
    Master,
    Replica,
}

/// The answer to a write of `AV OWNER`.
///
/// Only acceptance means master. `ENGINE_REPL_SLAVE`, `ENGINE_SWITCHOVER_DONE`
/// and `ENGINE_NOT_MY_KEY` all lead to the same conduct -- do not open a
/// listener, do not claim to be master -- so they collapse into one variant
/// rather than each carrying its own handling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    Accepted,
    Refused,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transition {
    BecameMaster,
    BecameReplica,
}

const UNKNOWN: u8 = 0;
const MASTER: u8 = 1;
const REPLICA: u8 = 2;

pub struct State {
    role: AtomicU8,
    /// The address this node last published, kept so a value read back can be
    /// recognised as its own. It is remembered rather than read from the
    /// socket because a demoted node has closed the listener by then.
    published: Mutex<Option<SocketAddr>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            role: AtomicU8::new(UNKNOWN),
            published: Mutex::new(None),
        }
    }
}

impl State {
    pub fn role(&self) -> Role {
        match self.role.load(Ordering::Acquire) {
            MASTER => Role::Master,
            REPLICA => Role::Replica,
            _ => Role::Unknown,
        }
    }

    /// Records a probe result, returning the transition if the role moved.
    pub fn observe(&self, probe: Probe) -> Option<Transition> {
        let next = match probe {
            Probe::Accepted => MASTER,
            Probe::Refused => REPLICA,
        };
        let previous = self.role.swap(next, Ordering::AcqRel);
        if previous == next {
            return None;
        }
        Some(match probe {
            Probe::Accepted => Transition::BecameMaster,
            Probe::Refused => Transition::BecameReplica,
        })
    }

    pub fn set_listen_addr(&self, addr: Option<SocketAddr>) {
        *self.published.lock().unwrap_or_else(|e| e.into_inner()) = addr;
    }

    pub fn listen_addr(&self) -> Option<SocketAddr> {
        *self.published.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether an `AV OWNER` value is one this node published itself.
    ///
    /// Right after a switchover the demoted node's probe fails -- telling it it
    /// is a replica -- while the value it reads is still its own, because the
    /// new master's write has not replicated yet. Dialling it would connect the
    /// node to itself.
    ///
    /// Compared as parsed `SocketAddr`s, not raw strings: IPv6 has more than
    /// one valid spelling of the same address (zero-compression, hex case, a
    /// dropped scope id), and a string compare would call two spellings of our
    /// own address someone else's. A value that fails to parse is treated as
    /// not stale -- the node goes on to dial it and fails at connect, which is
    /// the right place for that failure to surface.
    pub fn is_stale_owner(&self, addr: &str) -> bool {
        match self.listen_addr() {
            Some(mine) => addr.parse::<SocketAddr>().is_ok_and(|a| a == mine),
            None => false,
        }
    }
}

/// Not `arcus:`-prefixed on purpose. That prefix marks an item ITEM_INTERNAL,
/// which exempts it from `scrub stale` -- and from the replication gate, which
/// is the whole probe. A key nothing refuses answers no question.
pub const OWNER_KEY: &str = "AV OWNER";

/// Long enough that the write is nothing, short enough that a value lost to
/// `scrub stale` is back before it matters. That scrub runs once per cluster
/// membership change, not continuously.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// The address to publish, given what the listener bound to.
///
/// A wildcard bind says nothing about where to reach this node, so a fallback
/// chain applies, in order: the arcus service socket's address when it is
/// specific, then the address this host would route from -- which covers every
/// case the middle link of the spec's own three-link chain used to, and one it
/// did not (a container whose `hostname` resolves only to loopback).
///
/// That middle link, `hostname_ip`, is gone. It ran `hostname` as a
/// subprocess, and `master::open` always binds `0.0.0.0:0`, so the wildcard
/// branch below is the only branch and it ran on the heartbeat thread every
/// `ADVERTISED_REFRESH` for the life of the process. This process registers
/// three `pthread_atfork` child handlers (`attach`, `repl`, `role`) and all
/// three `pthread_create`; a `fork` from a background thread is safe only via
/// `std::process::Command`'s `posix_spawn` fast path, which is an
/// implementation detail and not one to bet a customer's daemon on. It also
/// blocked on a resolver, on the same thread that detects demotion. The
/// remaining link needs no fork and no DNS.
pub fn advertised(bound: SocketAddr) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return bound;
    }
    if let Some(ip) = service_ip().or_else(interface_ip) {
        return SocketAddr::new(ip, bound.port());
    }
    bound
}

/// The address arcus is serving on, when it is not a wildcard. `attach` finds
/// the same socket by scanning this process's own file descriptors; this
/// reads only its address, not which one is the "real" service port, so it
/// skips `attach`'s handshake -- any specific, non-loopback listener this
/// process owns is a fine stand-in for "where to reach this host."
///
/// `attach` is compiled whenever `migration` or `replication` is on
/// (`cfg(parked_cookie)`, build.rs), and this module only exists under
/// `replication`, so it is always there to call. A `replication`-only build
/// used to compile `attach` away and carry a duplicate of its fd scan here for
/// this one lookup; that duplicate is gone with the reason for it.
fn service_ip() -> Option<IpAddr> {
    crate::attach::service_ip()
}

/// Last link in the fallback chain: any non-loopback IPv4 address this host
/// would use to reach the outside world.
///
/// `src/owner.rs` already declares a `getifaddrs` binding of its own (for
/// the BSD/Darwin half of `node_address`'s MAC lookup); a second, separately
/// typed declaration of the same C symbol here would trip
/// `clashing_extern_declarations` on every platform where both compile,
/// and reusing that one would mean exporting its BSD-only, MAC-shaped
/// `Ifaddrs` struct across a module boundary for a use it was never shaped
/// for. Interface enumeration is not the only way to ask the kernel "what
/// address would I use," though: connecting a UDP socket asks the kernel to
/// pick a route and a source address for it without sending anything --
/// UDP is connectionless, so `connect` here is purely a local route lookup,
/// no packet leaves this host and no reply is awaited. That is exactly what
/// a container whose `hostname` resolves only to `127.0.0.1` still has: its
/// container-network address is reachable by *some* route even when nothing
/// resolves it by name.
///
/// Filtered to IPv4 for the same reason `service_ip` already is:
/// `master::open` binds an IPv4-only wildcard, so a route that only offers an
/// IPv6 source address is not a candidate at all.
fn interface_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // A public IPv4 address used only as a routing target, not a real peer;
    // any forwardable address would do equally well. A numeric address, not
    // a hostname, so this does not also depend on DNS working.
    sock.connect("8.8.8.8:53").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

pub fn shared() -> &'static State {
    static SHARED: std::sync::OnceLock<State> = std::sync::OnceLock::new();
    SHARED.get_or_init(State::default)
}

/// One write of `AV OWNER`, and what it means. `None` when the write's
/// outcome says nothing about replication -- a transient loss of the vtable,
/// say -- so the caller has a round to skip rather than a verdict to commit.
///
/// `Probe` itself stays two-valued: "cannot tell" is a fact about this one
/// attempt, not a third thing the state machine can be in.
///
/// On a **sync**-replication master this write can enter the daemon's own
/// replication wait: `rp_after_check` runs `rp_wait` unless
/// `svcore->get_noreply(cookie)` is true, and that enqueues a `wait_entry`
/// holding the parked attach connection's cookie, to be released by
/// `notify_io_complete` once a slave acknowledges the cset. There is no way for
/// an extension to opt out -- `SERVER_CORE_API` exposes `get_noreply` but no
/// setter, and the daemon clears `c->noreply` in `out_string` at the end of
/// every command anyway, so the connection `attach` parks always reads as
/// "wants a reply". The consequence is bounded rather than removed:
/// `MULTI_NOTIFY_IO_COMPLETE` is unconditionally defined
/// (`include/memcached/types.h`), so `waitfor_io_complete` increments
/// `c->current_io_wait` and the matching `notify_io_complete` decrements it
/// again; the parked connection is never `io_blocked` (it has no command in
/// flight), so nothing is queued to a worker thread and no reply is ever
/// written for nobody to read.
pub fn probe_once(store: &Store, addr: SocketAddr) -> Option<Probe> {
    classify(store.set_kv(OWNER_KEY, addr.to_string().as_bytes()))
}

/// What one `AV OWNER` write means, as a pure function of its outcome.
///
/// Lifted out of `probe_once` so it can be tested: `Store` wraps a live engine
/// pointer and this crate has no in-process substitute for one, so the branch
/// itself is the only part of the probe a unit test can reach at all.
fn classify(outcome: std::result::Result<(), StoreError>) -> Option<Probe> {
    match outcome {
        Ok(()) => Some(Probe::Accepted),
        // Defence in depth, and a dead arm as long as `Store::set_kv` keeps
        // using `check`: `ENGINE_EWOULDBLOCK` is an acceptance there, so it
        // arrives as `Ok(())` above. If it ever reaches here as an error again
        // it must NOT read as a refusal. A sync-replication master gets it from
        // `rp_after_check` -> `rp_wait` whenever a slave is behind on this
        // write's cset -- an ordinary condition, not a rejection -- and calling
        // it `Refused` demotes the master, closes its listener, drops its
        // replicas, and re-promotes on the next beat, forever.
        Err(StoreError::Engine(code))
            if code == crate::engine_api::ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK =>
        {
            None
        }
        // Every refusal means the same thing. ReplicaSlave is the ordinary
        // case; a switchover in progress and a key belonging to another
        // migration group both land in Engine(_).
        Err(StoreError::ReplicaSlave) | Err(StoreError::Engine(_)) => Some(Probe::Refused),
        // No engine, or a vtable without `store` or `get_item_info`. This is
        // silence, not an answer -- committing Replica here would demote a
        // master the moment its vtable hiccups, which is a conclusion, not
        // the absence of one.
        Err(_) => None,
    }
}

/// Whether writes are refused here.
///
/// Stays `Role::Unknown` -- so this reads `false` -- until `start_heartbeat`
/// has run and its first beat has completed. `probe_once` only ever writes
/// a key, using the placeholder `UNPUBLISHED` until this node has a real
/// address to publish (see `beat`), so a listener bind that keeps failing
/// does not stop this from getting an honest answer on every tick -- unlike
/// an earlier version of this comment claimed while `beat` still returned
/// before probing at all in that case.
///
/// `recovery::stamp` asks this instead of the engine: `Store::ping_slave`
/// could not answer, because it probed under an `arcus:`-prefixed key that
/// the replication gate lets through untouched.
pub fn is_replica() -> bool {
    shared().role() == Role::Replica
}

/// Starts the heartbeat thread. Idempotent.
///
/// This crate is loaded before the daemon's `-d` forks, and only the forking
/// thread survives into the child -- whatever was spawned below is gone by
/// the time the daemon is actually running. `src/attach.rs` hits the same
/// wall and solves it the same way: register a `pthread_atfork` child hook
/// once, and have the hook itself do the unconditional respawn. The guard
/// here gates only that one-time registration, not the spawn -- a guard
/// around the spawn would be inherited into the child already "used", and
/// the thread could never be started again, freezing the role at `Unknown`
/// for the rest of the daemon's life.
pub fn start_heartbeat() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    unsafe { pthread_atfork(None, None, Some(after_fork_in_child)) };
    spawn_heartbeat(true);
}

/// Runs in the freshly-forked child, after `-d` daemonizes. See
/// `start_heartbeat` for why this must not be guarded the same way.
///
/// Passes `false` for `delay_first_beat`: by the time this hook runs, the
/// one fork this delay exists to protect against has already happened, so
/// waiting it out here would only add a second, pointless `HEARTBEAT` of
/// `Role::Unknown` on top of whatever the pre-fork thread already spent
/// asleep before the fork cut it short -- and `Unknown` reads as "not a
/// replica" (`is_replica`), so `recovery.rs` would take the master path on
/// a node that is actually a fresh replica for that whole extra window.
/// That is precisely the bug this task exists to fix, reappearing for
/// twenty seconds on every daemonized start if this hook delayed too.
extern "C" fn after_fork_in_child() {
    spawn_heartbeat(false);
}

/// `delay_first_beat` is `true` only for the initial, pre-fork spawn
/// (`start_heartbeat`) -- see `after_fork_in_child`'s doc for why the
/// post-fork respawn must not repeat it.
fn spawn_heartbeat(delay_first_beat: bool) {
    let _ = std::thread::Builder::new()
        .name("arcvector-repl-role".to_owned())
        .spawn(move || {
            // Sleeps one full `HEARTBEAT` before the very first `beat` --
            // the only pre-fork code path that ever takes `State::published`,
            // `master`'s `LISTEN_ADDR`, or `cached_advertised`'s own cache
            // lock. This thread is spawned before `-d` forks (`repl::mod`'s
            // doc covers why that cannot be avoided outright for a
            // non-daemonized deployment, which is also why this delay still
            // runs even when `delay_first_beat` is `true` only by way of
            // `start_heartbeat`, never unconditionally); without this
            // delay, a fork landing while the very first beat held any of
            // those locks would freeze it in the child with no owner left
            // alive, hanging both `repl::mod`'s restart thread and the
            // child's own respawned heartbeat forever, silently. A fixed
            // sleep, not a flag the atfork handler sets, because the
            // no-`-d` case still has to reach its first beat with no fork
            // ever happening to signal anything -- this is what makes it
            // work there too, at the cost of narrowing the window rather
            // than closing it: a fork delayed more than one `HEARTBEAT`
            // past extension init is still exposed. See `repl::mod`'s doc
            // for the rest of this story, including why a *second* fork
            // was never actually the reachable case.
            if delay_first_beat {
                std::thread::sleep(HEARTBEAT);
            }
            loop {
                beat();
                std::thread::sleep(HEARTBEAT);
            }
        });
}

/// The value `probe_once` writes while this node has never published a
/// real address (see `beat`). Never itself written into `State::published`
/// -- `is_stale_owner` and `listen_addr()` only ever see a real address or
/// `None`, exactly as before this existed. `0.0.0.0:0` specifically: no peer
/// could ever dial it, so a replica that reads this back off `AV OWNER`
/// simply fails to connect and retries, the same as any other address it
/// cannot reach.
const UNPUBLISHED: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

fn beat() {
    let state = shared();
    // Role detection needs no dialable listener at all -- `probe_once` only
    // ever writes a key. `acquire_listener` still tries for a real one
    // first (it costs nothing extra when it has never been tried), but a
    // bind failure there falls back to `UNPUBLISHED` rather than making
    // `beat` give up on probing this round -- the earlier version of this
    // function returned before `probe_once` in that case, which is exactly
    // the bug correction 5 exists to fix and correction 3 caught this
    // comment still describing wrong.
    let addr = match state.listen_addr() {
        Some(addr) => addr,
        None => acquire_listener(state).unwrap_or(UNPUBLISHED),
    };
    let Some(store) = Store::background_keyed() else {
        return;
    };
    let Some(probe) = probe_once(&store, addr) else {
        return; // Could not tell this round; try again at the next beat.
    };
    let transition = state.observe(probe);
    if state.role() == Role::Replica {
        // The lazy re-arm the slave half never had. `master::open` below is
        // called on every master beat for exactly this reason -- a listener
        // that failed to open, or that a `-d` fork took away, comes back on
        // its own -- and the connect loop now heals the same way instead of
        // depending on one spawn at startup having succeeded. Costs one atomic
        // compare-exchange when a loop is already running, which is every beat
        // on a healthy replica.
        crate::repl::ensure_slave_loop();
    }
    if state.role() == Role::Master {
        // Every beat, not just the one a transition lands on: a demotion's
        // `master::close` (or a `-d` fork -- see `repl::mod`'s child hook)
        // can take the listener out from under a node that still holds
        // `Role::Master`, and nothing else would ever notice, since
        // `observe` only reports a change the moment the role itself moves.
        if let Some(fresh) = refresh_listener(state)
            && fresh != addr
        {
            // The probe above published `addr`, which on a promotion is the
            // address this node had before it was demoted -- `master::close`
            // clears `master`'s own `LISTEN_ADDR` but deliberately leaves
            // `State::published` alone, because `is_stale_owner` needs it to
            // recognise its own value on `AV OWNER` and not dial itself.
            // `refresh_listener` has just bound a fresh ephemeral port, so
            // what is on `AV OWNER` right now is a port nothing is listening
            // on. Republishing here rather than waiting for the next beat is
            // the difference between replicas reconnecting immediately after
            // a switchover and dialling a refused port for up to `HEARTBEAT`
            // -- which is the latency this whole design exists to remove.
            //
            // The result is not fed back into `observe`: a refusal arriving
            // microseconds after an acceptance is a switchover this node will
            // see on its next beat anyway, and acting on it here would mean
            // handling a second transition inside the arm that is already
            // handling one.
            // `!= Accepted` rather than `is_none()`: a `Refused` here means
            // the gate turned this second write down, so the new address is
            // just as unpublished as if the write could not be attempted.
            if probe_once(&store, fresh) != Some(Probe::Accepted) {
                eprintln!(
                    "ArcVector: could not republish this master's address as {fresh}; \
                     replicas keep the previous one until the next heartbeat"
                );
            }
        }
    }
    match transition {
        Some(Transition::BecameMaster) => {
            eprintln!("ArcVector: this node is the replication master");
            // Its own graphs may be behind: it was a replica until now, and
            // no acknowledgement told the old master so. `converge` is the
            // same self-check a cold replica runs on a fresh `Snapshot`; it
            // rebuilds only what disagrees with this node's own Map.
            match crate::handler::registry::indexes() {
                Ok(indexes) => {
                    for index in indexes {
                        crate::repl::slave::converge(&index.name);
                    }
                }
                Err(_) => {
                    eprintln!(
                        "ArcVector: could not list indexes to converge after promotion; \
                         skipping this round"
                    );
                }
            }
        }
        Some(Transition::BecameReplica) => {
            eprintln!("ArcVector: this node is a replication replica");
            // `State::published` is deliberately NOT cleared alongside
            // `master`'s own `LISTEN_ADDR`: a demoted node still needs to
            // recognise its own address on `AV OWNER` (`is_stale_owner`) for
            // as long as the new master's write has not replicated here, or
            // `slave::master_addr` dials this node back to itself. The stale
            // address that leaves behind is dealt with on promotion instead,
            // where the master arm above republishes as soon as
            // `refresh_listener` has a real port again.
            crate::repl::master::close();
        }
        None => {}
    }
}

/// Tries to open a listener and publish it, for a beat that has never had
/// one. Attempted unconditionally, even with no engine attached at all --
/// `master::open` needs no `Store` -- so a first-ever beat can discover a
/// real, dialable address before it even knows whether this node will end
/// up wanting one. Uses the plain, uncached `advertised`: this only ever
/// runs once per address the node has never had, never on a recurring
/// per-beat basis, so there is nothing here for `refresh_listener`'s cache
/// to help with.
///
/// `None` only when the bind itself fails; `beat` falls back to
/// `UNPUBLISHED` in that case rather than giving up on probing this round.
fn acquire_listener(state: &State) -> Option<SocketAddr> {
    match crate::repl::master::open() {
        Ok(bound) => {
            let addr = advertised(bound);
            state.set_listen_addr(Some(addr));
            Some(addr)
        }
        Err(_) => None,
    }
}

/// Keeps the published address honest for as long as this node is master.
/// Returns the new address when it actually moved, so `beat` can write it to
/// `AV OWNER` in the same round rather than a heartbeat later.
///
/// Two different things can go stale between beats, and `master::open` is
/// idempotent enough to repair both in one call: if the listener itself is
/// gone, `open` really does bind a fresh one (a CAS-guarded flag, cleared by
/// whatever closed it, is what makes this a real reopen rather than a
/// refused one); if the listener is still fine but what `advertised` would
/// resolve it to has changed, `open` is a cheap no-op and only the address
/// recomputation (`cached_advertised`, below) matters. Both paths end the
/// same way: republish only if the result actually differs from what is
/// already published, so an unchanged environment costs one comparison, not
/// a fresh `AV OWNER` write on every tick -- `None` says exactly that
/// nothing moved.
fn refresh_listener(state: &State) -> Option<SocketAddr> {
    match crate::repl::master::open() {
        Ok(bound) => {
            let addr = cached_advertised(bound);
            if state.listen_addr() == Some(addr) {
                return None;
            }
            eprintln!("ArcVector: publishing this master's address as {addr}");
            state.set_listen_addr(Some(addr));
            Some(addr)
        }
        Err(e) => {
            eprintln!(
                "ArcVector: replication listener not (re)open while master ({e}); \
                 will retry next heartbeat"
            );
            None
        }
    }
}

/// How long a cached `advertised` answer is trusted before the second link of
/// the fallback chain -- `interface_ip`'s route lookup, which binds and
/// connects a UDP socket -- runs again on its own, rather than on every single
/// heartbeat. Long relative to `HEARTBEAT` on purpose: `service_ip` (the first
/// link, a scan of descriptors this process already owns) is still rechecked
/// every beat and forces an immediate recompute the moment its own answer
/// changes, so this interval only bounds how long a change `service_ip` cannot
/// see by itself -- a route that moved to another interface, say -- takes to
/// be noticed.
const ADVERTISED_REFRESH: Duration = Duration::from_secs(300);

/// `refresh_listener`'s view of `advertised`: same fallback chain, but
/// reruns only the parts worth rerunning. A specific (non-wildcard) bind
/// still short-circuits with no caching at all, exactly like `advertised`
/// itself -- there is no fallback chain to cache in that case. For a
/// wildcard bind, `service_ip` is cheap enough (no syscall beyond this
/// process's own descriptors) to check on every call; `interface_ip` runs only
/// when `service_ip`'s own answer has changed or `ADVERTISED_REFRESH` has
/// elapsed since the last time it did.
///
/// This mattered a great deal more when the chain's middle link ran `hostname`
/// as a subprocess and blocked on a resolver: a master with no arcus service
/// socket to find (a unix-socket-only or loopback-only deployment) paid that
/// every ten seconds forever, and a slow nameserver stalled the beat that runs
/// it, delaying the `AV OWNER` refresh, delaying demotion detection, and --
/// since this runs before `beat`'s match on the transition -- delaying the
/// promotion `converge` pass behind it too. With that link gone the saving is
/// smaller, but it is still a socket bind and a route lookup per beat, and the
/// caching is already written and tested.
fn cached_advertised(bound: SocketAddr) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return bound;
    }
    struct Cache {
        cheap: Option<IpAddr>,
        resolved: Option<IpAddr>,
        at: Instant,
    }
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

    let cheap = service_ip();
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let last = guard.as_ref().map(|c| (c.cheap, c.at));
    if cache_is_stale(last, cheap, Instant::now(), ADVERTISED_REFRESH) {
        let resolved = cheap.or_else(interface_ip);
        *guard = Some(Cache {
            cheap,
            resolved,
            at: Instant::now(),
        });
    }
    let ip = guard.as_ref().and_then(|c| c.resolved);
    ip.map_or(bound, |ip| SocketAddr::new(ip, bound.port()))
}

/// The caching decision `cached_advertised` makes, pulled out so it is
/// testable without a real clock wait or a real fallback chain: stale when
/// there has never been a cached answer, when the cheap link's own answer
/// has changed since the last check, or when `refresh` has elapsed since
/// the last recompute. `now` and `refresh` are parameters rather than
/// `Instant::now()`/`ADVERTISED_REFRESH` read internally, the same way
/// `slave::Pending::expired` takes `now` -- what makes the interval
/// observable from a test without an actual multi-minute wait.
fn cache_is_stale(
    last: Option<(Option<IpAddr>, Instant)>,
    cheap: Option<IpAddr>,
    now: Instant,
    refresh: Duration,
) -> bool {
    last.is_none_or(|(seen, at)| seen != cheap || now.saturating_duration_since(at) >= refresh)
}

unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> std::os::raw::c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sync-replication master whose slave is behind gets
    /// `ENGINE_EWOULDBLOCK` from the ordinary write path. `Store::set_kv` turns
    /// that into `Ok(())` (see its `check`), so the probe reads it as
    /// acceptance -- the gate did let the write through.
    #[test]
    fn a_write_that_still_waits_on_a_slave_is_an_acceptance() {
        assert_eq!(classify(Ok(())), Some(Probe::Accepted));
    }

    /// The defence-in-depth half of the same fix: should `set_kv` ever stop
    /// flattening `ENGINE_EWOULDBLOCK` into `Ok`, the probe must skip the round
    /// rather than conclude "not master". A plain `Engine(_)` arm classified it
    /// as `Probe::Refused`, which is what made a sync master flap.
    #[test]
    fn a_would_block_error_is_never_read_as_a_refusal() {
        let would_block =
            StoreError::Engine(crate::engine_api::ENGINE_ERROR_CODE_ENGINE_EWOULDBLOCK);
        assert_eq!(
            classify(Err(would_block)),
            None,
            "EWOULDBLOCK says nothing about who the master is"
        );
    }

    #[test]
    fn a_gate_refusal_is_the_only_thing_that_makes_this_node_a_replica() {
        assert_eq!(
            classify(Err(StoreError::ReplicaSlave)),
            Some(Probe::Refused)
        );
        assert_eq!(
            classify(Err(StoreError::Engine(0x62))),
            Some(Probe::Refused)
        );
        assert_eq!(
            classify(Err(StoreError::Unavailable)),
            None,
            "a missing vtable is silence, not a demotion"
        );
    }

    #[test]
    fn an_accepted_write_makes_this_node_the_master() {
        let state = State::default();
        assert_eq!(
            state.observe(Probe::Accepted),
            Some(Transition::BecameMaster)
        );
        assert_eq!(state.role(), Role::Master);
    }

    #[test]
    fn a_refused_write_makes_this_node_a_replica() {
        let state = State::default();
        assert_eq!(
            state.observe(Probe::Refused),
            Some(Transition::BecameReplica)
        );
        assert_eq!(state.role(), Role::Replica);
    }

    #[test]
    fn an_unchanged_role_reports_no_transition() {
        let state = State::default();
        state.observe(Probe::Accepted);
        assert_eq!(state.observe(Probe::Accepted), None, "still master");
    }

    #[test]
    fn a_switchover_moves_the_role_both_ways() {
        let state = State::default();
        state.observe(Probe::Accepted);
        assert_eq!(
            state.observe(Probe::Refused),
            Some(Transition::BecameReplica)
        );
        assert_eq!(
            state.observe(Probe::Accepted),
            Some(Transition::BecameMaster)
        );
    }

    #[test]
    fn an_address_equal_to_our_own_is_stale() {
        let state = State::default();
        state.set_listen_addr(Some("10.0.0.1:5000".parse().unwrap()));
        assert!(state.is_stale_owner("10.0.0.1:5000"), "we published this");
        assert!(!state.is_stale_owner("10.0.0.2:5000"), "someone else did");
    }

    #[test]
    fn any_address_is_usable_when_we_have_published_none() {
        let state = State::default();
        assert!(!state.is_stale_owner("10.0.0.1:5000"));
    }

    #[test]
    fn two_spellings_of_the_same_ipv6_address_are_both_stale() {
        let state = State::default();
        state.set_listen_addr(Some("[::1]:5000".parse().unwrap()));
        assert!(state.is_stale_owner("[::1]:5000"));
        assert!(state.is_stale_owner("[0:0:0:0:0:0:0:1]:5000"));
    }

    #[test]
    fn a_wildcard_bind_is_not_published_as_it_stands() {
        let bound: SocketAddr = "0.0.0.0:5000".parse().unwrap();
        let out = advertised(bound);
        assert_eq!(
            out.port(),
            5000,
            "the port is what the listener actually got"
        );
        // Re-derives the expected answer from the same links `advertised`
        // itself is documented to try, in the same order, rather than
        // asserting a loose negation of `advertised`'s own branch condition --
        // a prior version of this test asserted `!out.ip().is_unspecified() ||
        // (service_ip().is_none() && hostname_ip().is_none())`, which an
        // identity implementation (`|bound| bound`) passes on any host where
        // neither link finds anything, and this dev machine is exactly such a
        // host. Comparing against the real chain instead means a broken
        // precedence (say, trying `interface_ip` before `service_ip`), a
        // dropped link (the bug this test exists to catch: `interface_ip` was
        // missing from an earlier version of `advertised` entirely), or a lost
        // port would all show up as a mismatch here, on any host -- not only
        // one where the chain happens to find something.
        //
        // What this cannot catch: `advertised` and this expectation are built
        // from the exact same calls, so a `service_ip` or `interface_ip` that
        // is *itself* wrong (say, finding a loopback address it should have
        // filtered) would be wrong here too, identically -- there is no
        // independent oracle for "the right address" available in a unit test.
        let expected = service_ip()
            .or_else(interface_ip)
            .map_or(bound, |ip| SocketAddr::new(ip, bound.port()));
        assert_eq!(out, expected);
    }

    #[test]
    fn a_specific_bind_is_published_unchanged() {
        let bound: SocketAddr = "10.0.0.5:5000".parse().unwrap();
        assert_eq!(advertised(bound), bound);
    }

    /// Pins the transition wiring on the process-global `State` that `beat`
    /// actually reads and writes, not a private instance made for this test:
    /// a promotion and the demotion that follows it each report the right
    /// `Transition`, and the cached role -- the one value every other public
    /// function in this module reads back through `role()`/`is_replica()`
    /// -- lands where the last probe said it should, in both directions.
    #[test]
    fn a_promotion_then_a_demotion_leave_the_cached_role_where_the_last_probe_put_it() {
        let state = shared();
        assert_eq!(
            state.observe(Probe::Accepted),
            Some(Transition::BecameMaster)
        );
        assert_eq!(state.role(), Role::Master);
        assert_eq!(
            state.observe(Probe::Refused),
            Some(Transition::BecameReplica)
        );
        assert_eq!(state.role(), Role::Replica);
    }

    /// `beat` is the one thing that turns an observed transition into
    /// `master::open`/`close` and a convergence pass, so it is "the
    /// transition handler" this task's wiring hangs off of. Nothing else in
    /// this crate ever calls `shared().set_listen_addr(..)` (only `beat`'s
    /// own `acquire_listener`/`refresh_listener`, and `repl::mod`'s
    /// `open_listener` from its fork-restart thread, do -- `repl::start`
    /// itself no longer opens anything eagerly as of a later round of this
    /// task; see its doc), none of which run outside a real process, so
    /// `shared().listen_addr()` starts `None` here.
    ///
    /// This asserts the actual property correction 5 exists for -- a prior
    /// version of this test asserted only "does not panic", which a revert
    /// of correction 5 (`beat` giving up before `probe_once` whenever it had
    /// no listener) would still have passed, since giving up early panics
    /// nothing either. Asserting `listen_addr().is_some()` afterward instead
    /// pins that `beat` actually acquired one (`acquire_listener`, a real
    /// `TcpListener::bind` -- needs no `Store`, so this runs even here) --
    /// left open for whatever else in this binary runs after it, since
    /// `close()` below only closes `master`'s real listener, not `shared()`'s
    /// own cached address; there is no reset for that, the same as every
    /// other test in this file that touches the process-global `shared()`.
    ///
    /// What this cannot pin without a live `Store`: that `beat` reaches
    /// `state.observe` or either match arm at all -- `Store::background_keyed`
    /// is `None` with no engine attached, which is guaranteed true in every
    /// unit test in this crate, so this call still returns right after
    /// acquiring the listener. A transition's own side effects
    /// (`master::close` on demotion, `slave::converge` on promotion) are
    /// exercised by running the daemon, the same limit `mod.rs`'s own
    /// `publish_is_a_no_op_when_this_node_is_not_master` test already lives
    /// with for `master::publish`.
    #[test]
    fn the_heartbeat_acquires_a_listener_for_itself_and_does_not_panic_with_no_engine() {
        // See `master::serialize_tests`: this is the other half of the pair
        // that would otherwise race `master::tests`'s own reopen test over
        // the same process-global `LISTENING`/`LISTEN_ADDR`.
        let _guard = crate::repl::master::serialize_tests();
        assert!(
            shared().listen_addr().is_none(),
            "no earlier test published one"
        );
        beat();
        assert!(
            shared().listen_addr().is_some(),
            "beat must acquire a listener for itself, not just survive without one"
        );
        // Closes `master`'s real listener so it does not linger for
        // whatever else in this binary runs after it. `shared()`'s own
        // published address is deliberately left set -- see the doc above.
        crate::repl::master::close();
    }

    /// `beat` republishes `AV OWNER` in the same round only when
    /// `refresh_listener` actually moved the address, which is the signal that
    /// makes a promotion's stale-address window one beat shorter rather than
    /// one beat long. A `refresh_listener` that returned nothing (its shape
    /// before this fix), or that reported a move on every call, both fail here.
    #[test]
    fn refreshing_the_listener_reports_the_address_only_when_it_moved() {
        // Same reason as the heartbeat test below: `LISTENING`/`LISTEN_ADDR`
        // are process-global and `open` is called for real here.
        let _guard = crate::repl::master::serialize_tests();
        let state = State::default();
        let first = refresh_listener(&state);
        assert!(
            first.is_some(),
            "a state that has published nothing has just been given an address"
        );
        assert_eq!(state.listen_addr(), first, "and it was published");
        assert_eq!(
            refresh_listener(&state),
            None,
            "a second call changes nothing, so it must not ask for a republish"
        );
        crate::repl::master::close();
    }

    /// Pins fix #2's whole point: a first check has nothing to compare
    /// against, so it is always stale -- there is no cached answer yet to
    /// trust.
    #[test]
    fn a_first_ever_check_is_always_stale() {
        let ip = Some("10.0.0.1".parse().unwrap());
        assert!(cache_is_stale(
            None,
            ip,
            Instant::now(),
            Duration::from_secs(300)
        ));
    }

    /// The ordinary, hot-path case this fix exists for: `service_ip`'s
    /// answer has not moved and the refresh interval has not elapsed, so
    /// nothing here should be recomputed -- this is what keeps a master with no
    /// service socket to find from running `interface_ip`'s route lookup every
    /// ten seconds forever (and, before that link's predecessor was removed,
    /// from forking `hostname` and blocking on a resolver).
    #[test]
    fn an_unchanged_cheap_link_within_the_interval_is_not_stale() {
        let now = Instant::now();
        let ip = Some("10.0.0.1".parse().unwrap());
        let refresh = Duration::from_secs(300);
        assert!(!cache_is_stale(
            Some((ip, now)),
            ip,
            now + Duration::from_secs(1),
            refresh
        ));
    }

    /// The cheap link moving is trusted immediately, with no need to wait
    /// out the interval -- this is the signal a real environment change
    /// (the arcus service socket appearing or moving) is expected to give.
    #[test]
    fn a_changed_cheap_link_is_stale_immediately() {
        let now = Instant::now();
        let old_ip = Some("10.0.0.1".parse().unwrap());
        let new_ip = Some("10.0.0.2".parse().unwrap());
        assert!(cache_is_stale(
            Some((old_ip, now)),
            new_ip,
            now,
            Duration::from_secs(300)
        ));
    }

    /// The safety net for a change the cheap link cannot see by itself (a route
    /// that moved to another interface, say): even with no change at all in
    /// `service_ip`'s own answer, the interval alone eventually forces a
    /// recompute.
    #[test]
    fn an_unchanged_cheap_link_past_the_interval_is_stale() {
        let now = Instant::now();
        let ip = Some("10.0.0.1".parse().unwrap());
        let refresh = Duration::from_secs(300);
        assert!(cache_is_stale(
            Some((ip, now)),
            ip,
            now + refresh + Duration::from_secs(1),
            refresh
        ));
    }
}
