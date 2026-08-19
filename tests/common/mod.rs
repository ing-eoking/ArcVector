//! Harness for driving a real arcus server with the extension loaded.
//!
//! ```text
//! ARCVECTOR_MEMCACHED        path to the memcached binary       (required)
//! ARCVECTOR_ENGINE           path to default_engine.so          (required)
//! ARCVECTOR_MEMCACHED_ARGS   extra server arguments, space separated
//! ```
//!
//! An unset variable panics rather than skips, and each test gets its own server
//! on its own unix socket so parallel runs cannot collide.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

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

fn from_env(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The test binary lives in `target/<profile>/deps/`, so the cdylib is two directories up.
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

/// `cargo test` does not rebuild the cdylib the server loads, so a stale one must panic, not skip.
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
         `cargo test` does not rebuild the cdylib the server loads, so these tests \
         would verify stale code.\n\
         Run `cargo build` first.",
        module.display()
    );
}

pub struct Server {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Server {
    pub fn start() -> Self {
        let (Some(memcached), Some(engine)) = (
            from_env("ARCVECTOR_MEMCACHED"),
            from_env("ARCVECTOR_ENGINE"),
        ) else {
            panic!(
                "{}",
                missing("ARCVECTOR_MEMCACHED and ARCVECTOR_ENGINE are not set")
            )
        };
        for (what, path) in [("memcached", &memcached), ("engine", &engine)] {
            assert!(
                path.exists(),
                "{}",
                missing(&format!("{what} not found at {}", path.display()))
            );
        }
        let Some(module) = extension() else {
            panic!(
                "{}",
                missing("the extension library was not found next to the test binary")
            )
        };
        assert_fresh(&module);

        // `sun_path` is about 104 bytes on macOS, and the temp dir can be long enough to matter.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let socket = PathBuf::from(format!(
            "/tmp/arcv-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let log = socket.with_extension("log");
        let _ = std::fs::remove_file(&socket);

        // A file rather than a pipe: nothing has to drain it, so a chatty server cannot block.
        let child = Command::new(&memcached)
            .args(["-E".as_ref(), engine.as_os_str()])
            .args(["-X".as_ref(), module.as_os_str()])
            .args(["-s".as_ref(), socket.as_os_str()])
            // Several workers, so the concurrency paths are actually live.
            .args(["-t", "4"])
            .args(extra_args())
            .stdout(Stdio::null())
            .stderr(File::create(&log).map_or(Stdio::null(), Stdio::from))
            .spawn()
            .unwrap_or_else(|e| panic!("could not run {}: {e}", memcached.display()));

        let mut server = Self { child, socket, log };
        server.wait_until_listening();
        server.assert_extension_registered(&module);
        server
    }

    fn wait_until_listening(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the server exited with {status} instead of listening.\n{}",
                    self.diagnostics()
                );
            }
            if UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "the server never accepted a connection on {}.\n{}",
            self.socket.display(),
            self.diagnostics()
        );
    }

    fn assert_extension_registered(&self, module: &Path) {
        let reply = self.connect().send("vlist");
        assert!(
            reply == "END\r\n",
            "the server is up but did not answer `vlist`, so {} was not registered \
             as an extension.\nGot {reply:?}.\n{}",
            module.display(),
            self.diagnostics()
        );
    }

    fn diagnostics(&self) -> String {
        match std::fs::read_to_string(&self.log) {
            Ok(text) if !text.trim().is_empty() => format!("server output:\n{text}"),
            _ => "the server wrote no diagnostics.".to_owned(),
        }
    }

    pub fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.socket).expect("the server is listening");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the read timeout is settable");
        Client { stream }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.log);
    }
}

fn extra_args() -> Vec<String> {
    std::env::var("ARCVECTOR_MEMCACHED_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Building this target asked for a server, so not having one is a failure.
fn missing(reason: &str) -> String {
    format!(
        "{reason}.\n\
         Run `make test` to get a server from the container, or point\n\
         ARCVECTOR_MEMCACHED / ARCVECTOR_ENGINE at an arcus build yourself."
    )
}

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn send(&mut self, line: &str) -> String {
        self.write(&format!("{line}\r\n"));
        self.read_reply()
    }

    pub fn send_body(&mut self, line: &str, body: &str) -> String {
        self.write(&format!("{line}\r\n{body}\r\n"));
        self.read_reply()
    }

    /// Tests that *want* a wrong length call [`Client::send_body`] directly.
    pub fn vadd(&mut self, index: &str, id: &str, dim: usize, coords: &str) -> String {
        self.send_body(&format!("vadd {index} {id} {} {dim}", coords.len()), coords)
    }

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

    pub fn vsim(&mut self, index: &str, k: usize, dim: usize, coords: &str) -> String {
        self.send_body(
            &format!("VSIM VECTOR {index} {k} {} {dim}", coords.len()),
            coords,
        )
    }

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

    /// A wrong body length leaves surplus bytes that memcached answers as one more command.
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

    /// Read until the reply is terminated, so tests never race the server.
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

/// Every reply ends in a fixed status or error line, so the bodies need no parsing.
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
