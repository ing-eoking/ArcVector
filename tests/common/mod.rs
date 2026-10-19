use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

const TERMINATORS: [&str; 11] = [
    "END\r\n",
    "STORED\r\n",
    "CREATED\r\n",
    "EXISTS\r\n",
    "DELETED\r\n",
    "DROPPED\r\n",
    "NOT_FOUND\r\n",
    "REPLACED\r\n",
    "NOT_FOUND_ELEMENT\r\n",
    "NOT_FOUND_VECTOR\r\n",
    "TYPE_MISMATCH\r\n",
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
        let reply = self.connect().send("vop drop it-registration-probe");
        assert!(
            reply == "NOT_FOUND\r\n",
            "the server is up but did not answer `vop`, so {} was not registered \
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

    /// 줄 끝 `\r\n`을 덧붙이지 않고 그대로 보낸다.
    pub fn send_raw(&mut self, raw: &str) -> String {
        self.write(raw);
        self.read_reply()
    }

    pub fn send_body(&mut self, line: &str, body: &str) -> String {
        self.write(&format!("{line}\r\n{body}\r\n"));
        self.read_reply()
    }

    /// `vop upsert <index> <vkey> <len> str` -- 좌표를 텍스트로 보낸다.
    pub fn upsert(&mut self, index: &str, vkey: &str, coords: &str) -> String {
        self.send_body(
            &format!("vop upsert {index} {vkey} {} str", coords.len()),
            coords,
        )
    }

    /// `vop upsert ... meta <m> str` -- 본문은 메타데이터 줄, 벡터 줄 순서다.
    pub fn upsert_meta(&mut self, index: &str, vkey: &str, coords: &str, json: &str) -> String {
        self.send_body(
            &format!(
                "vop upsert {index} {vkey} {} meta {} str",
                coords.len(),
                json.len()
            ),
            &format!("{json}\r\n{coords}"),
        )
    }

    /// `vop upsert` with `bin` (little-endian f32) 벡터.
    pub fn upsert_bin(&mut self, index: &str, vkey: &str, coords: &[f32]) -> String {
        let bytes: Vec<u8> = coords.iter().flat_map(|c| c.to_le_bytes()).collect();
        self.send_bytes(
            &format!("vop upsert {index} {vkey} {}", bytes.len()),
            &bytes,
        )
    }

    /// `vop search <index> vector <num> <len> [filter <f>] [withmeta] str`
    pub fn search_vector(
        &mut self,
        index: &str,
        num: usize,
        coords: &str,
        filter: Option<&str>,
        withmeta: bool,
    ) -> String {
        let mut head = format!("vop search {index} vector {num} {}", coords.len());
        let mut body = String::new();
        if let Some(f) = filter {
            head += &format!(" filter {}", f.len());
            body += &format!("{f}\r\n");
        }
        if withmeta {
            head += " withmeta";
        }
        head += " str";
        body += coords;
        self.send_body(&head, &body)
    }

    /// `vop search <index> vkey <vkey> <num> [filter <f>] [withmeta]`
    pub fn search_vkey(
        &mut self,
        index: &str,
        vkey: &str,
        num: usize,
        filter: Option<&str>,
        withmeta: bool,
    ) -> String {
        let mut head = format!("vop search {index} vkey {vkey} {num}");
        if let Some(f) = filter {
            head += &format!(" filter {}", f.len());
        }
        if withmeta {
            head += " withmeta";
        }
        match filter {
            Some(f) => self.send_body(&head, f),
            None => self.send(&head),
        }
    }

    /// `vop setmeta <index> <vkey> <len>`
    pub fn setmeta(&mut self, index: &str, vkey: &str, json: &str) -> String {
        self.send_body(&format!("vop setmeta {index} {vkey} {}", json.len()), json)
    }

    pub fn getmeta(&mut self, index: &str, vkey: &str) -> String {
        self.send(&format!("vop getmeta {index} {vkey}"))
    }

    /// 헤더 줄과 이진 본문(끝 `\r\n` 포함 전송)을 보낸다.
    pub fn send_bytes(&mut self, line: &str, body: &[u8]) -> String {
        let mut raw = format!("{line}\r\n").into_bytes();
        raw.extend_from_slice(body);
        raw.extend_from_slice(b"\r\n");
        self.stream
            .write_all(&raw)
            .expect("the connection accepts writes");
        self.stream.flush().expect("the connection flushes");
        self.read_reply()
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
    // META <vkey> <metabytes>\r\n<json>\r\n 은 고정 종결 문자열이 없다.
    if reply.starts_with(b"META ") {
        let Some(head) = reply.windows(2).position(|w| w == b"\r\n") else {
            return false;
        };
        let len = std::str::from_utf8(&reply[..head])
            .ok()
            .and_then(|h| h.rsplit(' ').next())
            .and_then(|n| n.parse::<usize>().ok());
        return len.is_some_and(|n| reply.len() >= head + 2 + n + 2);
    }
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
