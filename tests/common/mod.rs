//! Harness for driving a real arcus daemon with the extension loaded.
//!
//! The daemon and engine are found through the environment, so the same tests run
//! against a local build and inside a container:
//!
//! ```text
//! ARCVECTOR_MEMCACHED   path to the memcached binary  (default <crate>/memcached)
//! ARCVECTOR_ENGINE      path to default_engine.so     (default <crate>/default_engine.so)
//! ```
//!
//! When either is missing the tests **skip** rather than fail — those artifacts
//! come from building arcus-memcached, which is not part of this crate's build.
//!
//! Each test gets its own daemon on its own **unix socket**. Sockets rather than
//! TCP ports because tests run in parallel: allocating a free port and then
//! spawning leaves a window in which another test can take it, and the daemon that
//! loses exits, which showed up as connections being refused mid-suite.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Reply lines that end a response, so a read knows when to stop.
const TERMINATORS: [&str; 8] = [
    "END\r\n",
    "STORED\r\n",
    "CREATED\r\n",
    "EXISTS\r\n",
    "DELETED\r\n",
    "DROPPED\r\n",
    "NOT_FOUND\r\n",
    "OVERFLOWED\r\n",
];

const ERROR_PREFIXES: [&str; 3] = ["CLIENT_ERROR", "SERVER_ERROR", "ERROR"];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn from_env_or(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map_or_else(|| crate_root().join(default), PathBuf::from)
}

/// The `cdylib` this crate built.
///
/// The test binary lives in `target/<profile>/deps/`, so the library is two
/// directories up. Cargo does not hand integration tests the artifact path.
fn extension() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?;
    ["libarcusv.dylib", "libarcusv.so"]
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.exists())
}

fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if let Ok(t) = meta.modified() {
                newest = Some(newest.map_or(t, |n| n.max(t)));
            }
        }
    }
    newest
}

/// Refuse to run against a library older than the sources.
///
/// `cargo test` builds the rlib the harness links against but **not** the
/// `cdylib` the daemon loads, so without this check a stale library would be
/// silently verified. Panics rather than skips: a stale result is a wrong result,
/// not a missing one.
fn assert_fresh(module: &Path) {
    let Ok(built) = module.metadata().and_then(|m| m.modified()) else {
        return;
    };
    let root = crate_root();
    let newest = [root.join("src"), root.join("include")]
        .iter()
        .filter_map(|dir| newest_mtime(dir))
        .chain(
            root.join("Cargo.toml")
                .metadata()
                .and_then(|m| m.modified())
                .ok(),
        )
        .max();
    assert!(
        newest.is_none_or(|newest| newest <= built),
        "{} is older than the sources.\n\
         `cargo test` does not rebuild the cdylib the daemon loads, so these tests \
         would verify stale code.\n\
         Run `cargo build` first.",
        module.display()
    );
}

/// A daemon with the extension loaded, stopped and cleaned up on drop.
pub struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    /// Start a daemon, or return `None` after printing why it could not.
    pub fn start() -> Option<Daemon> {
        let memcached = from_env_or("ARCVECTOR_MEMCACHED", "memcached");
        let engine = from_env_or("ARCVECTOR_ENGINE", "default_engine.so");

        for (what, path) in [("memcached", &memcached), ("engine", &engine)] {
            if !path.exists() {
                skip(&format!("{what} not found at {}", path.display()));
                return None;
            }
        }
        let Some(module) = extension() else {
            skip("the extension library was not found next to the test binary");
            return None;
        };
        assert_fresh(&module);

        // Short path: the sun_path field is about 104 bytes on macOS, and the
        // system temp dir can be long enough to matter.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let socket = PathBuf::from(format!(
            "/tmp/arcv-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&socket);

        let child = Command::new(&memcached)
            .args(["-E".as_ref(), engine.as_os_str()])
            .args(["-X".as_ref(), module.as_os_str()])
            .args(["-s".as_ref(), socket.as_os_str()])
            // Several workers, so the concurrency paths are actually live.
            .args(["-t", "4"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let mut daemon = Daemon { child, socket };
        if daemon.wait_until_listening() {
            Some(daemon)
        } else {
            skip("the daemon never accepted a connection");
            None
        }
    }

    fn wait_until_listening(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                eprintln!("daemon exited early with {status}");
                return false;
            }
            if UnixStream::connect(&self.socket).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    pub fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.socket).expect("the daemon is listening");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the read timeout is settable");
        Client { stream }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn skip(reason: &str) {
    eprintln!(
        "SKIP: {reason}.\n      \
         Build arcus-memcached and point ARCVECTOR_MEMCACHED / ARCVECTOR_ENGINE at it,\n      \
         or run the suite in the container (see docker/)."
    );
}

/// One connection, speaking the ASCII protocol.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Send a command line and read the whole reply.
    pub fn send(&mut self, line: &str) -> String {
        self.write(&format!("{line}\r\n"));
        self.read_reply()
    }

    /// Send a command line plus a body, as the two-phase commands need.
    pub fn send_body(&mut self, line: &str, body: &str) -> String {
        self.write(&format!("{line}\r\n{body}\r\n"));
        self.read_reply()
    }

    /// `vadd`, with `veclen` derived from the coordinates actually sent.
    ///
    /// Every length in this protocol is a chance to miscount, and a test that
    /// miscounts tests the wrong thing. Tests that *want* a wrong length call
    /// [`Client::send_body`] directly.
    pub fn vadd(&mut self, index: &str, id: &str, dim: usize, coords: &str) -> String {
        self.send_body(&format!("vadd {index} {id} {} {dim}", coords.len()), coords)
    }

    /// `vadd` with attributes, both lengths derived.
    pub fn vadd_attr(
        &mut self,
        index: &str,
        id: &str,
        dim: usize,
        coords: &str,
        attr: &str,
    ) -> String {
        self.send_body(
            &format!(
                "vadd {index} {id} {} {dim} ATTR {} {attr}",
                coords.len(),
                attr.len()
            ),
            coords,
        )
    }

    /// `VSIM VECTOR`, with the body byte count derived from the coordinates.
    pub fn vsim(&mut self, index: &str, k: usize, dim: usize, coords: &str) -> String {
        self.send_body(
            &format!("VSIM VECTOR {index} {k} {} {dim}", coords.len()),
            coords,
        )
    }

    /// `VSIM VECTOR` with a filter clause appended.
    pub fn vsim_filter(
        &mut self,
        index: &str,
        k: usize,
        dim: usize,
        coords: &str,
        terms: &[&str],
    ) -> String {
        self.send_body(
            &format!(
                "VSIM VECTOR {index} {k} {} {dim} FILTER {} {}",
                coords.len(),
                terms.len(),
                terms.join(" ")
            ),
            coords,
        )
    }

    /// Read and discard whatever is still buffered.
    ///
    /// A body length that disagrees with what was sent leaves the surplus bytes in
    /// the stream, and memcached answers them as one more (nonsense) command. That
    /// stray line is inherent to a length-prefixed protocol — memcached's own
    /// `set` behaves the same way — so a test that provokes it drains it here.
    pub fn drain(&mut self) {
        let previous = self.stream.read_timeout().ok().flatten();
        let _ = self
            .stream
            .set_read_timeout(Some(Duration::from_millis(200)));
        let mut buf = [0u8; 4096];
        while let Ok(n) = self.stream.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
        let _ = self.stream.set_read_timeout(previous);
    }

    fn write(&mut self, raw: &str) {
        self.stream
            .write_all(raw.as_bytes())
            .expect("the connection accepts writes");
        self.stream.flush().expect("the connection flushes");
    }

    /// Read until the reply is terminated, so tests never race the daemon.
    fn read_reply(&mut self) -> String {
        let mut reply = String::new();
        let mut buf = [0u8; 8192];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    reply.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if is_complete(&reply) {
                        break;
                    }
                }
                Err(_) => break, // timed out: return what arrived
            }
        }
        reply
    }
}

/// Has a complete reply been read?
///
/// Every reply ends with one of the fixed status lines or with an error line, so
/// this does not have to understand the bodies in between.
fn is_complete(reply: &str) -> bool {
    if TERMINATORS.iter().any(|t| reply.ends_with(t)) {
        return true;
    }
    let Some(last) = reply
        .strip_suffix("\r\n")
        .and_then(|s| s.rsplit("\r\n").next())
    else {
        return false;
    };
    ERROR_PREFIXES.iter().any(|p| last.starts_with(p))
}

/// A distinct index name per test, so a shared daemon could never confuse them.
pub fn index_name(test: &str) -> String {
    format!("it-{test}")
}

#[track_caller]
pub fn assert_reply(reply: &str, expected: &str) {
    assert_eq!(
        reply, expected,
        "\n  expected: {expected:?}\n  got:      {reply:?}"
    );
}

#[track_caller]
pub fn assert_contains(reply: &str, needle: &str) {
    assert!(
        reply.contains(needle),
        "\n  expected to contain: {needle:?}\n  got: {reply:?}"
    );
}
