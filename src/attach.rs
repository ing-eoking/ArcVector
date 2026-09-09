//! A connection this process holds open to its own service port, kept for the
//! sake of the `conn` the daemon builds behind it.
//!
//! Background threads call the engine without a request to borrow a cookie
//! from, and several engine entry points dereference that cookie without
//! checking it. The migration gate is the one that bites: a read of a key this
//! node no longer owns reaches `set_not_my_key_info`, which writes through the
//! cookie to record the new owner for the reply. A null cookie faults there.
//!
//! So this asks the daemon for a `conn` the only way an extension can -- by
//! being a client -- and parks it. Nothing is ever sent on the socket after the
//! handshake, so nobody reads the fields the engine writes through it, which is
//! what makes the cookie safe to share between background threads.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::raw::{c_int, c_void};
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

/// The reply the handler sends back, and how the thread recognises its own.
const ATTACH_ACK: &str = "ATTACHED";

/// `vattach` reaches the wire like any other command, so a client could send it
/// and have its own cookie adopted -- and then close, leaving a dangling one.
/// The command therefore carries a per-process secret that only this thread
/// knows, and the handler adopts nothing without it.
fn secret() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| format!("{:016x}", crate::owner::unpredictable()))
}

/// The token to put in a command sent on the parked connection, so the
/// handler can tell it apart from a client's.
pub fn token() -> &'static str {
    secret()
}

/// True if `token` came from this process's own attach thread.
pub fn is_ours(token: &str) -> bool {
    // Length is fixed and both sides are in-process, so a plain compare is
    // enough; there is no oracle to time against that a client can reach.
    token == secret()
}

/// Longer than the 500ms `handshake_timeout` a replication or migration
/// `msg_chan` gives a connection that has not said HELLO, so that its close
/// lands inside the read below rather than after it.
const SETTLE: Duration = Duration::from_millis(1200);

/// How long to wait before looking for the listening socket again. The scan
/// runs from extension init, which is before `server_socket()`, so the first
/// several rounds are expected to find nothing.
const RETRY: Duration = Duration::from_millis(200);

/// Highest fd the scan considers. The service socket is opened before any
/// client is accepted, so it takes a low number.
const MAX_FD: c_int = 1024;

static COOKIE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The cookie of the parked connection, or null before one is attached and
/// while a lost one is being replaced. Callers must handle null.
pub fn cookie() -> *const c_void {
    COOKIE.load(Ordering::Acquire).cast_const()
}

/// Records the cookie the daemon passed to the `vattach` handler.
pub fn adopt(cookie: *const c_void) {
    COOKIE.store(cookie.cast_mut(), Ordering::Release);
}

fn disown() {
    COOKIE.store(ptr::null_mut(), Ordering::Release);
}

/// Starts the thread that keeps a connection parked. Idempotent.
pub fn start() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    unsafe { pthread_atfork(None, None, Some(after_fork_in_child)) };
    spawn();
}

/// `-d` forks after the extensions load, and only the forking thread survives
/// into the daemon. Whatever was spawned above is gone; start it again here.
extern "C" fn after_fork_in_child() {
    disown();
    spawn();
}

fn spawn() {
    let _ = std::thread::Builder::new()
        .name("arcvector-attach".to_owned())
        .spawn(run);
}

fn run() {
    loop {
        match find_service() {
            Some(sock) => {
                park(sock);
                disown();
            }
            None => std::thread::sleep(RETRY),
        }
    }
}

/// How long one [`request`] waits for its reply. Generous, because a `set` on
/// a sync-replication master does not answer until a slave acknowledges the
/// cset (`rp_wait`), and that wait is itself bounded by the server's own
/// `max_sync_wait_msec`. A timeout here is treated as a dead connection.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often an idle connection is checked for the daemon having closed it.
/// The cookie is advertised to background threads for as long as the slot is
/// full, so this bounds how long a freed `conn` can be handed out.
const IDLE_POLL: Duration = Duration::from_secs(1);

/// How long that check blocks. Held across the `PARKED` lock, so it is kept
/// far below `IDLE_POLL`: a `request` arriving mid-check waits this long at
/// worst.
const IDLE_PEEK: Duration = Duration::from_millis(50);

/// The parked connection, once there is one, and whatever a previous reply
/// left unconsumed.
struct Parked {
    sock: Sock,
    rest: Vec<u8>,
}

static PARKED: Mutex<Option<Parked>> = Mutex::new(None);
/// Signalled when [`PARKED`] goes back to `None`, so the attach thread can
/// stop waiting and dial again.
static CLOSED: Condvar = Condvar::new();

/// Sends one ASCII command on the parked connection and returns its reply.
///
/// This is the reason the connection is worth holding beyond its cookie. The
/// daemon runs the command on the worker thread that owns this `conn`, which
/// is what makes it an ordinary client write: it takes the replication gate's
/// per-worker-thread bookkeeping in the one order that bookkeeping is safe
/// under, it replicates like any other write, and it waits for the slave's
/// acknowledgement in the normal way instead of leaving a `wait_entry` behind
/// for a cookie nobody is reading. Reaching the same engine call directly with
/// a borrowed cookie does none of that -- see
/// `handler::arcus::engine::Store::replication_mode` for what it costs.
///
/// The reply is read on the calling thread while the daemon's worker thread
/// runs the command, so anything this crate handles itself must not want a
/// lock the caller is holding -- this would wait on it forever. `vowner` is
/// written to that rule: it reads and writes one key through the engine and
/// touches nothing else, in particular not [`PARKED`], which the caller holds
/// for the whole exchange. Anything reaching the index machinery does not
/// belong here.
///
/// `None` means there was no connection, or the exchange failed -- in which
/// case the connection is dropped and the attach thread dials a new one.
pub fn request(command: &str) -> Option<Vec<String>> {
    let mut guard = PARKED.lock().unwrap_or_else(|e| e.into_inner());
    let parked = guard.as_mut()?;

    match exchange(parked, command) {
        Some(reply) => Some(reply),
        None => {
            // The cookie belongs to a connection that is no longer usable.
            *guard = None;
            disown();
            CLOSED.notify_all();
            None
        }
    }
}

fn exchange(parked: &mut Parked, command: &str) -> Option<Vec<String>> {
    parked.sock.set_read_timeout(Some(REPLY_TIMEOUT)).ok()?;
    (&parked.sock).write_all(command.as_bytes()).ok()?;
    (&parked.sock).write_all(b"\r\n").ok()?;
    (&parked.sock).flush().ok()?;

    let mut lines = Vec::new();
    loop {
        while let Some(line) = take_line(&mut parked.rest) {
            let done = terminates(&line, lines.is_empty());
            lines.push(line);
            if done {
                return Some(lines);
            }
        }
        let mut chunk = [0u8; 1024];
        match (&parked.sock).read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => parked.rest.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Splits one `\r\n`-terminated line off the front of `buf`.
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let end = buf.windows(2).position(|w| w == b"\r\n")?;
    let line = String::from_utf8_lossy(&buf[..end]).into_owned();
    buf.drain(..end + 2);
    Some(line)
}

/// Whether `line` ends the reply.
///
/// memcached answers either with one status line or with a run of `STAT`/
/// `VALUE` lines closed by `END`. Deciding on the first line which shape this
/// is keeps the rule to those two cases, rather than a list of every status
/// string the server might return.
fn terminates(line: &str, first: bool) -> bool {
    if line == "END" {
        return true;
    }
    first && !line.starts_with("STAT ") && !line.starts_with("VALUE ")
}

/// Publishes the socket for [`request`] and returns once the daemon has closed
/// it -- which invalidates the cookie, so the caller must drop it before
/// reconnecting.
fn park(sock: Sock) {
    let mut guard = PARKED.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Parked {
        sock,
        rest: Vec::new(),
    });

    loop {
        let (next, timed_out) = CLOSED
            .wait_timeout(guard, IDLE_POLL)
            .unwrap_or_else(|e| e.into_inner());
        guard = next;

        // A `request` that failed got here first and cleared the slot.
        let Some(parked) = guard.as_mut() else { return };

        if timed_out.timed_out() && !peer_still_there(parked) {
            *guard = None;
            disown();
            return;
        }
    }
}

/// Whether the daemon still has the other end of an idle connection.
///
/// This has to exist, and a `request` failing is not enough on its own. The
/// cookie stays published for as long as the slot is full, and
/// `Store::background_keyed` hands it to `recovery::run_builder` and
/// `access::sweep::probe_round` -- so a `conn` the daemon has freed must stop
/// being advertised promptly, not whenever something next happens to send. In
/// a `migration` build nothing sends at all: `repl` is not compiled, so
/// without this the cookie would dangle for the life of the process.
///
/// A short blocking read rather than a peek: the daemon does not talk first on
/// a client connection, so this times out while the connection is healthy,
/// returns zero once it is closed, and anything it does read belongs in `rest`
/// for the next reply to consume anyway.
fn peer_still_there(parked: &mut Parked) -> bool {
    if parked.sock.set_read_timeout(Some(IDLE_PEEK)).is_err() {
        return false;
    }
    let mut chunk = [0u8; 256];
    match (&parked.sock).read(&mut chunk) {
        Ok(0) => false,
        Ok(n) => {
            parked.rest.extend_from_slice(&chunk[..n]);
            true
        }
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

/// Connects to every listening socket this process owns and returns the one
/// that answers `vattach`, having stored its cookie.
fn find_service() -> Option<Sock> {
    let mut candidates: Vec<Sock> = listeners().into_iter().filter_map(connect).collect();
    if candidates.is_empty() {
        return None;
    }

    // A `msg_chan` listener accepts, waits for a HELLO it will never get, and
    // closes. The service socket has no idle timeout and stays. One timed read
    // does both the waiting and the telling apart, and the reads overlap in
    // wall-clock because every candidate was connected before the first one.
    candidates.retain(still_open);

    candidates.into_iter().find(handshake)
}

fn still_open(sock: &Sock) -> bool {
    if sock.set_read_timeout(Some(SETTLE)).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    match (&mut &*sock).read(&mut byte) {
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        // The handshake timeout closed it.
        Ok(0) => false,
        // It spoke first, which the service port does not do.
        Ok(1..) => false,
    }
}

fn handshake(sock: &Sock) -> bool {
    let mut sock = sock;
    let cmd = format!("vattach {}\r\n", secret());
    if sock.set_read_timeout(Some(SETTLE)).is_err() || sock.write_all(cmd.as_bytes()).is_err() {
        return false;
    }
    let mut reply = [0u8; 32];
    let Ok(read) = sock.read(&mut reply) else {
        return false;
    };
    // `adopt` already ran, on the worker thread that served the command. All
    // that is left is to agree the reply came from our own handler.
    if String::from_utf8_lossy(&reply[..read]).starts_with(ATTACH_ACK) {
        eprintln!("ArcVector: attached a connection for background work");
        return true;
    }
    disown();
    false
}

fn connect(addr: Addr) -> Option<Sock> {
    match addr {
        Addr::Inet(addr) => TcpStream::connect(addr).ok().map(|s| {
            let _ = s.set_nodelay(true);
            Sock::Tcp(s)
        }),
        Addr::Unix(path) => UnixStream::connect(path).ok().map(Sock::Unix),
    }
}

/*
 * Finding the listening sockets
 */

enum Addr {
    Inet(SocketAddr),
    Unix(String),
}

/// The address of the first specific, non-loopback, IPv4 listening socket
/// this process owns. `role::advertised` uses this as a routable stand-in
/// for the wildcard address its own replication listener bound to; it needs
/// only an address, not which candidate is actually the service port, so
/// unlike `find_service` it never runs the `vattach` handshake to tell them
/// apart.
///
/// `listening_addr` already turns a wildcard bind into loopback (see below),
/// so filtering out loopback here also filters out a wildcard bind -- there
/// is nothing further to check. IPv6 is filtered separately: `master::open`
/// binds an IPv4-only wildcard (`0.0.0.0:0`), so a listener whose only
/// address happens to be IPv6 is not a candidate at all -- publishing it
/// would hand a replica an address nothing on that listener can ever answer.
pub fn service_ip() -> Option<IpAddr> {
    listeners().into_iter().find_map(|addr| match addr {
        Addr::Inet(sa) if !sa.ip().is_loopback() && sa.ip().is_ipv4() => Some(sa.ip()),
        _ => None,
    })
}

/// Every listening socket this process owns, service and otherwise.
///
/// The extension is loaded into the daemon, so the daemon's own descriptors are
/// in reach; asking the kernel about them is how the port is learned without a
/// setting to read or a request to borrow a cookie from.
fn listeners() -> Vec<Addr> {
    (0..MAX_FD).filter_map(listening_addr).collect()
}

/// The address `fd` listens on, if it is a listening stream socket.
fn listening_addr(fd: c_int) -> Option<Addr> {
    // Closed descriptors and plain files fail here, and a UDP socket answers
    // SOCK_DGRAM. Note that `SO_ACCEPTCONN`, which asks the question directly,
    // is not gettable on macOS.
    if sock_type(fd)? != SOCK_STREAM {
        return None;
    }

    let local = sock_name(fd, getsockname)?;

    // A listening socket has no peer. A connected one -- a client of ours, or a
    // connection the daemon accepted -- answers here and is not what we want.
    if sock_name(fd, getpeername).is_some() {
        return None;
    }

    decode(&local)
}

fn sock_type(fd: c_int) -> Option<c_int> {
    let mut ty: c_int = 0;
    let mut len = size_of::<c_int>() as u32;
    let ok = unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_TYPE,
            ptr::from_mut(&mut ty).cast::<c_void>(),
            &mut len,
        )
    };
    (ok == 0).then_some(ty)
}

type NameFn = unsafe extern "C" fn(c_int, *mut c_void, *mut u32) -> c_int;

fn sock_name(fd: c_int, which: NameFn) -> Option<[u8; SOCKADDR_STORAGE_LEN]> {
    let mut raw = [0u8; SOCKADDR_STORAGE_LEN];
    let mut len = SOCKADDR_STORAGE_LEN as u32;
    let ok = unsafe { which(fd, raw.as_mut_ptr().cast::<c_void>(), &mut len) };
    (ok == 0).then_some(raw)
}

fn decode(raw: &[u8; SOCKADDR_STORAGE_LEN]) -> Option<Addr> {
    match family(raw) {
        AF_INET => {
            let port = u16::from_be_bytes([raw[2], raw[3]]);
            if port == 0 {
                return None;
            }
            let octets: [u8; 4] = raw[4..8].try_into().ok()?;
            let ip = Ipv4Addr::from(octets);
            // A wildcard bind is not an address to dial. Loopback also earns the
            // admin exemption that lets the connection past `maxconns`.
            let ip = if ip.is_unspecified() {
                Ipv4Addr::LOCALHOST
            } else {
                ip
            };
            Some(Addr::Inet(SocketAddr::new(IpAddr::V4(ip), port)))
        }
        AF_INET6 => {
            let port = u16::from_be_bytes([raw[2], raw[3]]);
            if port == 0 {
                return None;
            }
            let octets: [u8; 16] = raw[8..24].try_into().ok()?;
            let ip = Ipv6Addr::from(octets);
            let ip = if ip.is_unspecified() {
                Ipv6Addr::LOCALHOST
            } else {
                ip
            };
            Some(Addr::Inet(SocketAddr::new(IpAddr::V6(ip), port)))
        }
        AF_UNIX => {
            let path = &raw[SUN_PATH_OFFSET..];
            let end = path.iter().position(|b| *b == 0).unwrap_or(path.len());
            // An unnamed or abstract socket is not one we can dial by path.
            if end == 0 {
                return None;
            }
            Some(Addr::Unix(
                String::from_utf8_lossy(&path[..end]).into_owned(),
            ))
        }
        _ => None,
    }
}

/*
 * Platform detail
 */

const SOCKADDR_STORAGE_LEN: usize = 128;
const SUN_PATH_OFFSET: usize = 2;
const SOCK_STREAM: c_int = 1;
const AF_UNIX: u16 = 1;
const AF_INET: u16 = 2;

#[cfg(target_os = "linux")]
mod plat {
    use std::os::raw::c_int;
    pub const SOL_SOCKET: c_int = 1;
    pub const SO_TYPE: c_int = 3;
    pub const AF_INET6: u16 = 10;
    /// `sockaddr` opens with a two-byte family.
    pub fn family(raw: &[u8]) -> u16 {
        u16::from_ne_bytes([raw[0], raw[1]])
    }
}

#[cfg(target_vendor = "apple")]
mod plat {
    use std::os::raw::c_int;
    pub const SOL_SOCKET: c_int = 0xffff;
    pub const SO_TYPE: c_int = 0x1008;
    pub const AF_INET6: u16 = 30;
    /// `sockaddr` opens with a length byte, then a one-byte family.
    pub fn family(raw: &[u8]) -> u16 {
        u16::from(raw[1])
    }
}

use plat::{AF_INET6, SO_TYPE, SOL_SOCKET, family};

unsafe extern "C" {
    fn getsockopt(fd: c_int, level: c_int, name: c_int, val: *mut c_void, len: *mut u32) -> c_int;
    fn getsockname(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int;
    fn getpeername(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int;
    fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> c_int;
}

/*
 * The service socket is either kind, so both are dialled the same way.
 */

enum Sock {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Sock {
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_read_timeout(dur),
            Self::Unix(s) => s.set_read_timeout(dur),
        }
    }
}

impl Read for &Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => (&*s).read(buf),
            Sock::Unix(s) => (&*s).read(buf),
        }
    }
}

impl Write for &Sock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => (&*s).write(buf),
            Sock::Unix(s) => (&*s).write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sock::Tcp(s) => (&*s).flush(),
            Sock::Unix(s) => (&*s).flush(),
        }
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&mut &*self).read(buf)
    }
}
