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
        Self::start_with(&[])
    }

    /// A server with extra command-line options, for a test whose subject is a
    /// server limit rather than the extension's own behaviour.
    ///
    /// `ARCVECTOR_MEMCACHED_ARGS` cannot serve this: it is read from the
    /// environment, which every concurrently running test shares.
    pub fn start_with(options: &[&str]) -> Self {
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

        static NEXT: AtomicU32 = AtomicU32::new(0);
        let socket = PathBuf::from(format!(
            "/tmp/arcv-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let log = socket.with_extension("log");
        let _ = std::fs::remove_file(&socket);

        let child = Command::new(&memcached)
            .args(["-E".as_ref(), engine.as_os_str()])
            .args(["-X".as_ref(), module.as_os_str()])
            .args(["-s".as_ref(), socket.as_os_str()])
            .args(["-t", "4"])
            .args(options)
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

    /// A connection that speaks the binary protocol.
    ///
    /// The one way a test can address a key the ascii protocol cannot spell.
    /// An index's metadata lives under an id with a space in it, which the
    /// ascii tokeniser would split -- that is exactly why no client `vadd` can
    /// collide with it. The binary protocol takes a key as a length and bytes,
    /// so it has no such trouble.
    ///
    /// A separate connection, because the server picks a protocol per
    /// connection from the first byte it sees (`negotiating_prot`).
    pub fn connect_binary(&self) -> BinaryClient {
        let stream = UnixStream::connect(&self.socket).expect("the server is listening");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("the read timeout is settable");
        BinaryClient { stream }
    }
}

/// Just enough of the binary protocol to move one item around.
pub struct BinaryClient {
    stream: UnixStream,
}

impl BinaryClient {
    const MAGIC_REQUEST: u8 = 0x80;
    const OP_GET: u8 = 0x00;
    const OP_SET: u8 = 0x01;

    pub fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        self.request(Self::OP_GET, key, &[], &[]);
        let (status, body, extlen) = self.response();
        // 4 bytes of flags come before the value.
        (status == 0).then(|| body[extlen..].to_vec())
    }

    pub fn set(&mut self, key: &str, value: &[u8]) -> bool {
        // extras: flags then expiry, both zero.
        self.request(Self::OP_SET, key, &[0u8; 8], value);
        self.response().0 == 0
    }

    fn request(&mut self, opcode: u8, key: &str, extras: &[u8], value: &[u8]) {
        let key = key.as_bytes();
        let total = extras.len() + key.len() + value.len();

        let mut out = Vec::with_capacity(24 + total);
        out.push(Self::MAGIC_REQUEST);
        out.push(opcode);
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.push(extras.len() as u8);
        out.push(0); // datatype
        out.extend_from_slice(&0u16.to_be_bytes()); // vbucket
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // opaque
        out.extend_from_slice(&0u64.to_be_bytes()); // cas
        out.extend_from_slice(extras);
        out.extend_from_slice(key);
        out.extend_from_slice(value);

        self.stream
            .write_all(&out)
            .expect("the connection accepts writes");
        self.stream.flush().expect("the connection flushes");
    }

    /// `(status, body, extlen)`.
    fn response(&mut self) -> (u16, Vec<u8>, usize) {
        let mut header = [0u8; 24];
        self.stream
            .read_exact(&mut header)
            .expect("the server answers with a binary header");

        let extlen = header[4] as usize;
        let status = u16::from_be_bytes([header[6], header[7]]);
        let bodylen = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;

        let mut body = vec![0u8; bodylen];
        self.stream
            .read_exact(&mut body)
            .expect("the server answers with the body it declared");
        (status, body, extlen)
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

    pub fn vsim_withattr(
        &mut self,
        index: &str,
        k: usize,
        dim: usize,
        coords: &str,
        terms: &[&str],
    ) -> String {
        let filter = if terms.is_empty() {
            String::new()
        } else {
            format!(" FILTER {} {}", terms.len(), terms.join(" "))
        };
        self.send_body(
            &format!(
                "VSIM VECTOR {index} {k} {} {dim}{filter} WITHATTR",
                coords.len()
            ),
            coords,
        )
    }

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

    /// Reads the payload of a single-key `get`, byte for byte.
    ///
    /// The bodies this crate stores are binary -- a length header, the
    /// attribute bytes, then the packed vector -- so a test that replays one
    /// cannot go through `String`.
    pub fn get_value(&mut self, key: &str) -> Option<Vec<u8>> {
        self.write(&format!("get {key}\r\n"));
        let reply = self.read_raw();
        let head = reply.windows(2).position(|w| w == b"\r\n")?;
        let header = std::str::from_utf8(&reply[..head]).ok()?;
        let len: usize = header.rsplit(' ').next()?.parse().ok()?;
        let from = head + 2;
        reply.get(from..from + len).map(<[u8]>::to_vec)
    }

    /// Writes a body verbatim with a plain `set`, bypassing this crate's
    /// commands. Replaying an item the way replication delivers it.
    pub fn set_value(&mut self, key: &str, body: &[u8]) -> String {
        self.set_value_exptime(key, 0, body)
    }

    /// [`Client::set_value`] with an expiry, for the one thing this crate's own
    /// commands cannot express: a *vector* that expires. `vcreate` takes an
    /// `EXPTIME` for the index, `vadd` takes none, so a test that wants an
    /// expired vector has to write the item itself.
    pub fn set_value_exptime(&mut self, key: &str, exptime: u32, body: &[u8]) -> String {
        let mut raw = format!("set {key} 0 {exptime} {}\r\n", body.len()).into_bytes();
        raw.extend_from_slice(body);
        raw.extend_from_slice(b"\r\n");
        self.stream
            .write_all(&raw)
            .expect("the connection accepts writes");
        self.stream.flush().expect("the connection flushes");
        String::from_utf8_lossy(&self.read_raw()).into_owned()
    }

    fn write(&mut self, raw: &str) {
        self.stream
            .write_all(raw.as_bytes())
            .expect("the connection accepts writes");
        self.stream.flush().expect("the connection flushes");
    }

    fn read_reply(&mut self) -> String {
        String::from_utf8_lossy(&self.read_raw()).into_owned()
    }

    fn read_raw(&mut self) -> Vec<u8> {
        let mut reply = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    reply.extend_from_slice(&buf[..n]);
                    if is_complete(&reply) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        reply
    }
}

fn is_complete(reply: &[u8]) -> bool {
    if TERMINATORS.iter().any(|t| reply.ends_with(t.as_bytes())) {
        return true;
    }
    let text = String::from_utf8_lossy(reply);
    let Some(last) = text
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
