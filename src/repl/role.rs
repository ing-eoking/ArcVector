use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::handler::arcus::engine::Store;

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
/// The space is deliberate: no ASCII-protocol command can name this key, so
/// no client can read or overwrite what a node publishes here. Nothing ever
/// puts it on the wire -- `vowner` carries the *address*, and the handler
/// names the key on the worker thread -- so the space costs nothing.
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

/// This node's replication role, asked of the daemon over the connection
/// `crate::attach` parks rather than inferred from whether a write is refused.
///
/// `None` when the question could not be answered this round -- no parked
/// connection, or an exchange that failed -- so the caller has a round to skip
/// rather than a verdict to commit. `Probe` itself stays two-valued: "cannot
/// tell" is a fact about one attempt, not a third thing the state machine can
/// be in.
///
/// This used to be a write of the owner key, classified by whether the
/// replication gate accepted it, issued straight into the engine with the
/// parked connection's cookie. It read the role correctly and cost the daemon
/// something it should not have. `rp_before_check` stamps
/// `last_cset_seqs[thr_idx]`, an unlocked array indexed by *worker thread*
/// whose safety rests on memcached's own invariant of one in-flight write per
/// worker thread -- the source calls it "thread specific data". A background
/// thread borrowing a connection's cookie resolves to the worker thread that
/// connection sits on, which is also serving real clients, and `LOCK_CACHE`
/// is taken inside `item_store` rather than across `rp_before_check` and
/// `rp_after_check`, so it cannot serialise the two. A client whose sequence
/// is zeroed between its own write and its `rp_after_check` is told the write
/// is durable before the replica has it -- ordinary `set` traffic, not just
/// this crate's.
///
/// Going over the wire removes the borrowing entirely: the daemon runs
/// `stats replication` on the worker thread that owns that connection, which
/// is the arrangement all of that bookkeeping is written for.
///
/// `alone` and `disabled` both answer `Accepted`, which is what the old write
/// probe did: `rp_before_check` only refuses in `RP_MODE_SLAVE`, so a node
/// with replication off or with no peer attached took the write and read as a
/// master. Preserving that keeps a single-node deployment behaving as it did.
pub fn probe_once() -> Option<Probe> {
    classify_mode(&mode_line(&crate::attach::request("stats replication")?)?)
}

/// The value of the `mode` line in a `stats replication` reply, if it has one.
fn mode_line(reply: &[String]) -> Option<String> {
    reply.iter().find_map(|line| {
        line.strip_prefix("STAT mode ")
            .map(|mode| mode.trim().to_owned())
    })
}

/// What arcus's four `rp_mode_string` values mean here. An unrecognised one is
/// `None` -- a mode this crate has not been taught is not a role to guess at.
fn classify_mode(mode: &str) -> Option<Probe> {
    match mode {
        "slave" => Some(Probe::Refused),
        "master" | "alone" | "disabled" => Some(Probe::Accepted),
        _ => None,
    }
}

/// Publishes `addr` as this master's address, but only when the key does not
/// already say that.
///
/// Sends `vowner`; the daemon runs the actual engine write in
/// [`owner_command`] below, on the worker thread that owns the parked
/// connection. Returns whether the key names `addr` afterwards.
pub fn publish_owner(addr: SocketAddr) -> bool {
    let addr = addr.to_string();
    ask_owner(Some(&addr)).as_deref() == Some(addr.as_str())
}

/// Reads the owner key back, `None` when it is unset or the exchange failed.
pub fn read_owner() -> Option<String> {
    ask_owner(None)
}

/// One `vowner` exchange. `Some(addr)` publishes, `None` only reads.
fn ask_owner(addr: Option<&str>) -> Option<String> {
    let mut command = format!("vowner {}", crate::attach::token());
    if let Some(addr) = addr {
        command.push(' ');
        command.push_str(addr);
    }
    let reply = crate::attach::request(&command)?;
    parse_owner_reply(reply.first()?)
}

/// `OWNER <addr>` carries a value; a bare `OWNER` means the key is unset.
/// Anything else -- an error line, say -- is no answer at all.
fn parse_owner_reply(line: &str) -> Option<String> {
    match line.strip_prefix("OWNER") {
        Some("") => None,
        Some(rest) => Some(rest.trim_start().to_owned()),
        None => None,
    }
}

/// The `vowner` handler: runs on a memcached worker thread, holding that
/// thread's own connection cookie.
///
/// This is the whole point of routing the write through a command. A keyed
/// write stamps `last_cset_seqs[thr_idx]`, an unlocked array indexed by
/// worker thread whose safety rests on memcached's own invariant of one
/// in-flight write per worker thread. Reached here, that invariant holds: the
/// daemon picked the thread, and it is the thread that owns the cookie being
/// used. Reached from a background thread wearing a borrowed cookie it does
/// not, and a client sharing that worker thread can be told its write is
/// durable before the replica has it.
///
/// Reads before it writes, so a settled master writes nothing at all -- the
/// only writes left are the ones a promotion actually requires. The write
/// itself is a plain `set`: what a promoted node replaces is the previous
/// master's address, and replacing it in one operation is what keeps a slave
/// attaching at that moment from finding no key at all.
pub fn owner_command(store: &Store, addr: Option<&str>) -> String {
    let current = store
        .get_kv(OWNER_KEY)
        .ok()
        .and_then(|raw| String::from_utf8(raw).ok());

    let Some(wanted) = addr else {
        return reply_line(current.as_deref());
    };
    if current.as_deref() == Some(wanted) {
        return reply_line(Some(wanted));
    }
    match store.set_kv(OWNER_KEY, wanted.as_bytes()) {
        Ok(()) => reply_line(Some(wanted)),
        Err(_) => reply_line(None),
    }
}

fn reply_line(addr: Option<&str>) -> String {
    match addr {
        Some(addr) => format!("OWNER {addr}\r\n"),
        None => "OWNER\r\n".to_owned(),
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
    let Some(probe) = probe_once() else {
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
        // Getting the listener and publishing it happen in this order, on
        // every beat rather than only on a transition: a demotion's
        // `master::close` (or a `-d` fork -- see `repl::mod`'s child hook) can
        // take the listener out from under a node that still holds
        // `Role::Master`, and `observe` only reports a change the moment the
        // role itself moves, so nothing else would notice.
        //
        // Role detection no longer carries an address, which is what used to
        // make this awkward: the probe wrote the owner key, so it published
        // whatever address it had *before* `refresh_listener` could correct
        // it, and a promoted node advertised a dead port for a whole beat
        // until a second write fixed it. Now the address is only ever written
        // by `publish_owner`, after the listener is known, and `publish_owner`
        // reads before it writes -- so a settled master writes nothing at all.
        let addr = match state.listen_addr() {
            Some(_) => {
                refresh_listener(state);
                state.listen_addr()
            }
            None => acquire_listener(state),
        };
        match addr {
            Some(addr) => {
                if !publish_owner(addr) {
                    eprintln!(
                        "ArcVector: could not publish this master's address as {addr}; \
                         replicas keep the previous one until the next heartbeat"
                    );
                }
            }
            None => eprintln!(
                "ArcVector: this master has no listener to publish; \
                 replicas cannot reach it until one opens"
            ),
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

    /// `OWNER <addr>` carries a value, a bare `OWNER` says the key is unset,
    /// and anything else -- `CLIENT_ERROR ..` from a token the handler did not
    /// recognise, say -- is no answer rather than an empty one.
    #[test]
    fn an_owner_reply_is_read_apart_from_an_unset_key_and_an_error() {
        assert_eq!(
            parse_owner_reply("OWNER 10.0.0.1:7654").as_deref(),
            Some("10.0.0.1:7654")
        );
        assert_eq!(parse_owner_reply("OWNER"), None, "the key is unset");
        assert_eq!(
            parse_owner_reply("CLIENT_ERROR unknown command vowner"),
            None,
            "an error is not an address"
        );
    }

    /// An IPv6 address keeps its colons and brackets intact through the reply.
    #[test]
    fn an_ipv6_owner_survives_the_round_trip() {
        let addr: SocketAddr = "[::1]:7654".parse().expect("literal parses");
        let line = reply_line(Some(&addr.to_string()));
        let value = parse_owner_reply(line.trim_end()).expect("a value came back");
        assert_eq!(value.parse::<SocketAddr>().ok(), Some(addr));
    }

    /// The four values of `rp_mode_string[]`
    /// (`engines/default/replication.c`). `alone` and `disabled` read as
    /// master because the gate they replaced only ever refused in
    /// `RP_MODE_SLAVE`: a node with no peer, or with replication switched
    /// off, took the probe write and was read as a master. A single-node
    /// deployment has to keep behaving that way.
    #[test]
    fn only_slave_makes_this_node_a_replica() {
        assert_eq!(classify_mode("slave"), Some(Probe::Refused));
        assert_eq!(classify_mode("master"), Some(Probe::Accepted));
        assert_eq!(classify_mode("alone"), Some(Probe::Accepted));
        assert_eq!(classify_mode("disabled"), Some(Probe::Accepted));
    }

    /// A mode this crate has not been taught is silence, not a role. Guessing
    /// would demote a master the first time arcus grows a fifth state.
    #[test]
    fn an_unrecognised_mode_is_not_guessed_at() {
        assert_eq!(classify_mode("promoting"), None);
        assert_eq!(classify_mode(""), None);
    }

    #[test]
    fn the_mode_is_picked_out_of_a_whole_stats_reply() {
        let reply = [
            "STAT config:loglevel 0".to_owned(),
            "STAT mode slave".to_owned(),
            "STAT state RUNNING".to_owned(),
            "END".to_owned(),
        ];
        assert_eq!(mode_line(&reply).as_deref(), Some("slave"));
    }

    /// `stats replication` on a server with replication compiled out answers
    /// with no `mode` line at all, which must read as "cannot tell" rather
    /// than as any role.
    #[test]
    fn a_reply_without_a_mode_line_is_no_answer() {
        assert_eq!(mode_line(&["END".to_owned()]), None);
        assert_eq!(mode_line(&[]), None);
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

    /// Nothing else in this crate calls `shared().set_listen_addr(..)` -- only
    /// `beat`'s own `acquire_listener`/`refresh_listener` and `repl::mod`'s
    /// `open_listener`, none of which run outside a real process -- so
    /// `shared().listen_addr()` starts `None` here.
    ///
    /// `beat` no longer opens a listener before it knows the role.
    ///
    /// **This test asserted the opposite until the probe moved off the engine.**
    /// The old probe was a write of the owner key, so it needed an address to
    /// write, so `beat` called `acquire_listener` first -- unconditionally, on
    /// every node. That meant a pure replica bound a port and spawned an
    /// acceptor on its first beat only to close both one arm later.
    ///
    /// Now the role is read first (`probe_once`, over `crate::attach`'s
    /// connection) and the listener is acquired inside the `Role::Master` arm,
    /// where it is actually wanted. With no parked connection -- guaranteed in
    /// every unit test in this crate, since nothing here runs a daemon --
    /// `probe_once` answers `None` and `beat` returns having touched nothing.
    ///
    /// What this cannot pin without a live daemon: anything past that early
    /// return -- `state.observe`, either match arm, or a transition's side
    /// effects (`master::close` on demotion, `slave::converge` on promotion).
    /// Those need the integration tier.
    #[test]
    fn the_heartbeat_opens_no_listener_before_it_knows_the_role() {
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
            shared().listen_addr().is_none(),
            "a node that cannot tell its role must not bind a port speculatively"
        );
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
