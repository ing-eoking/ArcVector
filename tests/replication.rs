use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn node_a() -> u16 {
    port("ARCVECTOR_REPL_NODE_A", 11311)
}

fn node_b() -> u16 {
    port("ARCVECTOR_REPL_NODE_B", 11312)
}

struct Node(TcpStream);

impl Node {
    fn open(port: u16) -> Self {
        let addr = format!("127.0.0.1:{port}");
        let sock = TcpStream::connect(&addr).unwrap_or_else(|e| {
            panic!(
                "no server at {addr}: {e}\n\
                 Bring the pair up first — see docs/내부구조.md §2.4."
            )
        });
        sock.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        Self(sock)
    }

    /// Read until the socket goes quiet: these tests drive `mop` and `stats` too, and terminators differ per family.
    fn cmd(&mut self, line: &str, body: Option<&str>) -> String {
        self.cmd_raw(line, body.map(str::as_bytes))
    }

    /// A stored element is not text (`1.0` is `00 00 80 3F`), so the body cannot travel as a `&str`.
    fn cmd_raw(&mut self, line: &str, body: Option<&[u8]>) -> String {
        let mut out = line.as_bytes().to_vec();
        out.extend_from_slice(b"\r\n");
        if let Some(b) = body {
            out.extend_from_slice(b);
            out.extend_from_slice(b"\r\n");
        }
        self.0.write_all(&out).unwrap();
        self.0.flush().unwrap();

        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let deadline = Instant::now() + Duration::from_millis(900);
        while Instant::now() < deadline {
            match self.0.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).trim().to_owned()
    }

    /// A rebuild answers `SERVER_ERROR index is unreadable` by design; a client polls, so does this.
    fn cmd_settled(&mut self, line: &str, body: Option<&str>) -> String {
        for _ in 0..40 {
            let reply = self.cmd(line, body);
            if !reply.contains("unreadable") {
                return reply;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("'{line}' never stopped answering 'unreadable'");
    }

    fn mode(&mut self) -> String {
        self.cmd("stats replication", None)
            .lines()
            .find(|l| l.starts_with("STAT mode "))
            .map(|l| l.trim().rsplit(' ').next().unwrap().to_owned())
            .unwrap_or_else(|| "none".to_owned())
    }
}

/// Left in default **sync** mode on purpose: EWOULDBLOCK on every write once freed a linked element and took the server down.
fn pair() -> (Node, Node) {
    let (mut a, mut b) = (Node::open(node_a()), Node::open(node_b()));
    let (mode_a, mode_b) = (a.mode(), b.mode());
    assert!(
        (mode_a == "master" && mode_b == "slave") || (mode_a == "slave" && mode_b == "master"),
        "expected one master and one slave, got {}={mode_a} {}={mode_b}",
        node_a(),
        node_b(),
    );
    if mode_a == "master" { (a, b) } else { (b, a) }
}

/// The pair outlives any one `cargo test`, so a panicked run's leftover index must not collide with this one.
fn index_name(base: &str) -> String {
    format!("{base}_{}", std::process::id())
}

fn seed(master: &mut Node, index: &str) {
    master.cmd(&format!("vdrop {index}"), None);
    assert_eq!(
        master.cmd(&format!("vcreate {index} 4 METRIC l2"), None),
        "CREATED"
    );
    for (i, v) in ["1 0 0 0", "0 1 0 0", "0 0 1 0"].iter().enumerate() {
        let reply = master.cmd(&format!("vadd {index} v{} {} 4", i + 1, v.len()), Some(v));
        assert_eq!(reply, "STORED", "vadd v{} failed", i + 1);
    }
}

/// One element is the metadata, so the element count runs one ahead of the vector count.
fn vectors_in(node: &mut Node, key: &str) -> Option<u32> {
    map_head(node, key).map(|(_, elements)| elements.saturating_sub(1))
}

/// `mop get` answers `VALUE <flags> <count>` — the only way to read a collection's flags over ASCII.
fn map_head(node: &mut Node, key: &str) -> Option<(u32, u32)> {
    let reply = node.cmd(&format!("mop get {key} 0 0"), None);
    let head = reply.lines().next()?.trim().to_owned();
    let mut parts = head.split_whitespace();
    if parts.next()? != "VALUE" {
        return None;
    }
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// `stats replication` is answered by the engine, so `get_stats` reaches it with no new server API.
#[test]
fn stats_replication_reports_the_role() {
    let (mut master, mut slave) = pair();
    assert_eq!(master.mode(), "master");
    assert_eq!(slave.mode(), "slave");
}

/// Vectors are Map elements, so replication carries them — which is what makes rebuilding from Map possible.
#[test]
fn vector_elements_replicate_to_the_slave() {
    let index = index_name("repl_elems");
    let (mut master, mut slave) = pair();
    seed(&mut master, &index);
    std::thread::sleep(Duration::from_millis(700));

    assert_eq!(
        vectors_in(&mut master, &index),
        Some(3),
        "master vector count"
    );
    assert_eq!(
        vectors_in(&mut slave, &index),
        Some(3),
        "slave vector count"
    );

    master.cmd(&format!("vdrop {index}"), None);
}

/// Item flags survive replication byte for byte; recovery uses them as the cheap "is this Map ours?" check.
#[test]
fn item_flags_replicate_verbatim() {
    let key = index_name("repl_flags");
    let (mut master, mut slave) = pair();
    const MARKER: u32 = 0x4156_0001; // "AV" << 16 | format version

    master.cmd(&format!("mop delete {key} 0 0 drop"), None);
    assert_eq!(
        master.cmd(&format!("mop create {key} {MARKER} 0 100"), None),
        "CREATED"
    );
    assert_eq!(
        master.cmd(&format!("mop insert {key} f 5"), Some("datum")),
        "STORED"
    );
    std::thread::sleep(Duration::from_millis(700));

    assert_eq!(map_head(&mut master, &key).map(|h| h.0), Some(MARKER));
    assert_eq!(
        map_head(&mut slave, &key).map(|h| h.0),
        Some(MARKER),
        "flags must survive replication — the design reads them on the promoted node"
    );

    master.cmd(&format!("mop delete {key} 0 0 drop"), None);
}

/// A slave that was never master has no token, so it tries to take over — and the engine refuses a stamp on a replica.
#[test]
fn the_slave_has_no_index() {
    let index = index_name("repl_slave");
    let (mut master, mut slave) = pair();
    seed(&mut master, &index);
    std::thread::sleep(Duration::from_millis(700));

    assert_eq!(vectors_in(&mut slave, &index), Some(3));
    assert!(
        !slave.cmd("vlist", None).contains(&index),
        "the slave must not have registered the index"
    );
    let reply = slave.cmd(&format!("vsim VECTOR {index} 2 7 4"), Some("1 0 0 0"));
    assert!(
        reply.contains("ERROR"),
        "a replica must refuse vector queries, however it words it: {reply}"
    );

    master.cmd(&format!("vdrop {index}"), None);
}

/// The promoted node rebuilds from Map alone. Runs a switchover, so it leaves the roles swapped.
#[test]
fn a_promoted_node_recovers_its_index() {
    let index = index_name("repl_promote");
    let (mut master, _slave) = pair();
    seed(&mut master, &index);
    std::thread::sleep(Duration::from_millis(700));

    assert_eq!(master.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    let (mut promoted, _) = pair();
    assert_eq!(
        vectors_in(&mut promoted, &index),
        Some(3),
        "the promoted node must still hold every vector"
    );

    // A query is enough: the registry misses, the flags say ours, the metadata element supplies the parameters.
    let hits = promoted.cmd_settled(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    for id in ["v1", "v2", "v3"] {
        assert!(hits.contains(id), "recovered index is missing {id}: {hits}");
    }
    assert!(
        promoted.cmd("vlist", None).contains(&index),
        "and the index is registered again once rebuilt"
    );

    promoted.cmd(&format!("vdrop {index}"), None);
}

/// Ignored until recovery lands: `vcreate` treats a replicated Map as an orphan and drops it.
#[test]
fn vcreate_must_not_destroy_a_replicated_map() {
    let index = index_name("repl_destroy");
    let (mut master, _slave) = pair();
    seed(&mut master, &index);
    std::thread::sleep(Duration::from_millis(700));

    assert_eq!(master.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    let (mut promoted, _) = pair();
    assert_eq!(vectors_in(&mut promoted, &index), Some(3));

    let reply = promoted.cmd(&format!("vcreate {index} 4 METRIC l2"), None);
    assert_eq!(
        reply, "EXISTS",
        "vcreate over an existing ArcVector Map must report EXISTS, not re-create it"
    );
    assert_eq!(
        vectors_in(&mut promoted, &index),
        Some(3),
        "vcreate destroyed the replicated vectors"
    );

    // `EXISTS` alone would be satisfied by doing nothing; recovery has to make the index usable.
    let hits = promoted.cmd_settled(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    for id in ["v1", "v2", "v3"] {
        assert!(hits.contains(id), "rebuilt index is missing {id}: {hits}");
    }

    promoted.cmd(&format!("vdrop {index}"), None);
}

/// Switch A→B→A: B's writes reached A's Map below the extension, so A's graph no longer describes it — the `owner` token, not role polling, is what catches that.
#[test]
fn a_double_switchover_does_not_leave_a_stale_index() {
    let index = index_name("repl_stale");
    let (mut first, _) = pair();
    seed(&mut first, &index);
    assert!(first.cmd("vlist", None).contains(&format!("{index} dim=4")));

    assert_eq!(first.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    // B adds a fourth vector at `0 1 0 0`, so a query that way must return it at distance 0.
    let (mut second, _) = pair();
    assert_eq!(
        second.cmd_settled(&format!("vadd {index} v4 7 4"), Some("0 1 0 0")),
        "STORED",
        "the promoted node must be able to write after recovering"
    );

    assert_eq!(second.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    let (mut back, _) = pair();
    assert_eq!(
        vectors_in(&mut back, &index),
        Some(4),
        "the Map holds four vectors"
    );

    let by_key = back.cmd_settled(&format!("vgetattr {index} v4"), None);
    let by_vector = back.cmd_settled(&format!("vsim VECTOR {index} 4 7 4"), Some("0 1 0 0"));
    assert!(
        by_key.contains("v4"),
        "key lookup reads Map directly, so it finds v4: {by_key}"
    );
    assert!(
        by_vector.contains("v4"),
        "vector search must find v4 too — it is the nearest neighbour of its own \
         coordinates. Got: {by_vector}"
    );

    back.cmd(&format!("vdrop {index}"), None);
}

/// A master stamps a new token before writing any element, so a replica can never hold a matching token and stale data at once.
#[test]
fn a_demoted_node_answers_until_the_new_master_takes_over() {
    let index = index_name("repl_demoted");
    let (mut master, _) = pair();
    seed(&mut master, &index);

    assert_eq!(master.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    assert_eq!(
        master.mode(),
        "slave",
        "this test is about the node that just lost the role"
    );

    let hits = master.cmd(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    for id in ["v1", "v2", "v3"] {
        assert!(
            hits.contains(id),
            "nothing has written since the switchover, so the demoted node's graph \
             still matches its Map and {id} must come back: {hits}"
        );
    }

    // One write on the promoted node stamps a new token, which replication carries over.
    let (mut promoted, _) = pair();
    assert_eq!(
        promoted.cmd_settled(&format!("vadd {index} v4 7 4"), Some("0 1 0 0")),
        "STORED"
    );
    std::thread::sleep(Duration::from_millis(700));

    let reply = master.cmd(&format!("vsim VECTOR {index} 2 7 4"), Some("1 0 0 0"));
    assert!(
        reply.contains("replica"),
        "once the token has moved, the replica must name the reason rather than \
         answer from a graph it can no longer keep current: {reply}"
    );

    promoted.cmd(&format!("vdrop {index}"), None);
}

/// Untranslated, `ENGINE_REPL_SLAVE` (0x61) surfaced as `SERVER_ERROR engine error 97`.
#[test]
fn a_write_to_a_replica_names_the_reason() {
    let (mut master, mut slave) = pair();
    let index = index_name("repl_wronly");
    seed(&mut master, &index);
    std::thread::sleep(Duration::from_millis(700));

    let reply = slave.cmd(&format!("vcreate {index}_on_replica 4"), None);
    assert!(
        reply.contains("replica") && reply.contains("master"),
        "a command on a replica must say so, not leak an engine code: {reply}"
    );

    master.cmd(&format!("vdrop {index}"), None);
}
