//! Replication tests, against a master/slave pair the developer brings up.
//!
//! Deliberately **not** part of `make test`. The pair needs an EE server built
//! with `ENABLE_REPLICATION`, a `ZooKeeper` ensemble, and znodes provisioned for the
//! two nodes — none of which this crate builds or the container supplies. Gating
//! them behind the `replication-tests` feature keeps the required suite honest: it
//! never claims to have covered switchover.
//!
//! ```sh
//! cargo test --features replication-tests -- --test-threads=1
//! cargo test --features replication-tests -- --ignored   # the unfixed bug
//! ```
//!
//! See `docs/내부구조.md §2.4` for bringing the pair up.
//!
//! `--test-threads=1` because the tests share the pair and one of them performs a
//! switchover, which changes global state every other test reads.
//!
//! Two environment variables name the pair; both default to the ports the doc
//! provisions:
//!
//! ```text
//! ARCVECTOR_REPL_NODE_A   default 11311
//! ARCVECTOR_REPL_NODE_B   default 11312
//! ```
//!
//! The nodes are named A and B rather than master and slave because a switchover
//! swaps them, and after the first run the roles are whatever the last run left.
//! Every test resolves the roles at its start instead of assuming them.

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

    /// Send one command, optionally with a body line, and read the reply.
    ///
    /// The reply is read until the socket goes quiet rather than until a known
    /// terminator: these tests drive `mop` and `stats` as well as the extension,
    /// and the terminator set differs per command family.
    fn cmd(&mut self, line: &str, body: Option<&str>) -> String {
        self.cmd_raw(line, body.map(str::as_bytes))
    }

    /// As [`Node::cmd`], but the body is bytes.
    ///
    /// A stored element is not text — an `f32` of `1.0` is `00 00 80 3F`, and
    /// `0x80` alone is not valid UTF-8 — so the body cannot travel as a `&str`.
    /// The ASCII protocol is length-prefixed for data blocks, so the bytes
    /// themselves are unconstrained.
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

    /// Send a command, retrying while the index is still being rebuilt.
    ///
    /// A rebuild answers reads `SERVER_ERROR index is unreadable …` by design: the
    /// first caller after a failover starts it and is told to come back. A client
    /// polls; so does this.
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

/// Resolve which port currently holds which role.
///
/// The pair is left in its default **sync** mode. It used to be switched to async
/// here, because a sync master answers every write with `ENGINE_EWOULDBLOCK` and
/// the module read that as failure — which freed an element the engine had just
/// linked and took the server down on `assert(elem->linked == 0)`. That is fixed;
/// running these tests in the default mode is what keeps it fixed.
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

/// An index name unique to this run.
///
/// The registry lives in the server's memory and the pair outlives any one
/// `cargo test`, so a run that panicked before its cleanup leaves an index
/// registered on whichever node happened to own it. A later run asserting "this
/// node does not know the index" would then fail for a reason that has nothing to
/// do with the code. Naming per process sidesteps that entirely — a stale entry
/// from a previous run can no longer collide with this one.
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

/// How many vectors a Map holds.
///
/// One element is the index's metadata, so the element count runs one ahead of
/// the vector count. Tests say what they mean by going through here.
fn vectors_in(node: &mut Node, key: &str) -> Option<u32> {
    map_head(node, key).map(|(_, elements)| elements.saturating_sub(1))
}

/// `mop get` answers `VALUE <flags> <count>`, which is the only way to read a
/// collection's flags over ASCII.
fn map_head(node: &mut Node, key: &str) -> Option<(u32, u32)> {
    let reply = node.cmd(&format!("mop get {key} 0 0"), None);
    let head = reply.lines().next()?.trim().to_owned();
    let mut parts = head.split_whitespace();
    if parts.next()? != "VALUE" {
        return None;
    }
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

// ---------------------------------------------------------------------------

/// The role query the recovery design depends on: `stats replication` is answered
/// by the *engine*, so `get_stats` on the vtable reaches it with no new server API.
#[test]
fn stats_replication_reports_the_role() {
    let (mut master, mut slave) = pair();
    assert_eq!(master.mode(), "master");
    assert_eq!(slave.mode(), "slave");
}

/// Vectors are Map elements, so replication carries them with no help from the
/// module. This is what makes "rebuild the graph from Map" possible on a promoted
/// node at all.
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

/// Item flags survive replication byte for byte. The recovery design uses them as
/// the cheap "is this Map ours?" pre-check on a node that has never seen the
/// index, so a flags value that did not replicate would sink it.
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

/// A slave that was never master has no graph, and cannot make one.
///
/// It has no token either, so the comparison against the Map's is a mismatch and
/// it tries to take over — which stamps the Map, and the engine refuses that on a
/// replica. Nothing role-aware is involved; the refusal is the engine's.
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

/// After a switchover the promoted node rebuilds from Map alone.
///
/// `metric`, `M`, `EFC` and `EFS` used to live only in the old master's registry,
/// which is why this could not work before: the elements survived and nothing
/// could say what index they belonged to. They now travel in the Map, so the
/// first query on the promoted node is enough.
///
/// Runs a switchover, so it leaves the roles swapped for whatever runs next —
/// which is why every test resolves roles at its start.
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

    // A query is enough to trigger recovery: the registry misses, the Map's flags
    // say it is ours, and the metadata element supplies the metric and HNSW
    // parameters the registry no longer has.
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

/// The destructive bug, pinned.
///
/// `vcreate` treats a Map it did not create as an orphan from before a restart
/// and `drop_map`s it before retrying (`handler.rs:106`). On a promoted node that
/// Map is the replicated data, and the first `vcreate` deletes all of it.
///
/// Ignored because it fails until the recovery design lands: `vcreate` must
/// answer `EXISTS` and leave the elements alone. Run it with
/// `cargo test --features replication-tests -- --ignored`.
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

    // `EXISTS` alone would be satisfied by doing nothing. The point of answering
    // it instead of re-creating is that recovery is now under way, so the index
    // has to become usable without any further prompting.
    let hits = promoted.cmd_settled(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    for id in ["v1", "v2", "v3"] {
        assert!(hits.contains(id), "rebuilt index is missing {id}: {hits}");
    }

    promoted.cmd(&format!("vdrop {index}"), None);
}

/// The stale-index hazard, pinned.
///
/// Switch A→B and back. A's process never died, so it still holds the graph it
/// built as master — but while B was master, B's writes reached A's Map through
/// replication, *below* the extension, where nothing told A. Without a check, A
/// resumes serving from a graph that no longer describes its own Map, and the two
/// read paths contradict each other with no error on either:
///
/// ```text
/// vget  ──▶ store.get_elem(name, id)  ──▶ arcus Map        the truth
/// vsim  ──▶ ann.filtered_search(…)    ──▶ usearch graph    a cache of it
/// ```
///
/// Polling the role cannot catch this: it reads `master` before and after, so no
/// sampling frequency sees a change. What differs is the *graph*, which is why
/// the metadata element carries an `owner` minted on every build.
///
/// B writes with `vadd` rather than `mop insert` deliberately. A raw `mop insert`
/// moves the data without going through this module, so the token stays put — the
/// hole the spec names and does not close. `vadd` is what a client actually does,
/// and it makes B build first, which is what changes the token.
#[test]
fn a_double_switchover_does_not_leave_a_stale_index() {
    let index = index_name("repl_stale");
    let (mut first, _) = pair();
    seed(&mut first, &index);
    assert!(first.cmd("vlist", None).contains(&format!("{index} dim=4")));

    // A -> B
    assert_eq!(first.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    // B recovers the index and adds a fourth vector at `0 1 0 0`, so a query in
    // that direction must return it at distance 0.
    let (mut second, _) = pair();
    assert_eq!(
        second.cmd_settled(&format!("vadd {index} v4 7 4"), Some("0 1 0 0")),
        "STORED",
        "the promoted node must be able to write after recovering"
    );

    // B -> A
    assert_eq!(second.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    let (mut back, _) = pair();
    assert_eq!(
        vectors_in(&mut back, &index),
        Some(4),
        "the Map holds four vectors"
    );

    // The point of the test: both read paths must agree about v4.
    let by_key = back.cmd_settled(&format!("vget {index} v4"), None);
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

/// A demoted node answers reads until the new master takes over, then stops.
///
/// There is no role check anywhere in the command path: `owner` carries the whole
/// answer. Right after a switchover the promoted node has served nothing, so the
/// token in Map is untouched and the demoted node's graph still describes its own
/// Map exactly. Its answers are correct, and refusing them would be refusing
/// correct data.
///
/// What makes that safe is the ordering: a master stamps a new token **before**
/// it writes any element, and replication delivers both through the same item in
/// order. So a replica can never hold a matching token and stale data at once —
/// the mismatch always arrives first.
///
/// This test walks both halves: correct answers while the token matches, and a
/// refusal the moment it does not. The refusal comes from the take-over's stamp,
/// which the engine rejects on a replica.
#[test]
fn a_demoted_node_answers_until_the_new_master_takes_over() {
    let index = index_name("repl_demoted");
    let (mut master, _) = pair();
    seed(&mut master, &index);

    assert_eq!(master.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    // `master` is the same connection, to what is now the replica.
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

    // One write on the promoted node is enough: it takes over, which stamps a new
    // token into the Map, and replication carries that to the demoted node.
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

/// A write that reaches a replica says so.
///
/// The engine answers `ENGINE_REPL_SLAVE` (0x61); untranslated it surfaced as
/// `SERVER_ERROR engine error 97`, which tells a client nothing about where to
/// send the write instead.
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
