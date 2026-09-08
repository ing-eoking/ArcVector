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

    fn cmd(&mut self, line: &str, body: Option<&str>) -> String {
        self.cmd_raw(line, body.map(str::as_bytes))
    }

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

    /// Retries until the index answers in full.
    ///
    /// A read taken while the graph rebuilds is answered from the part of it
    /// that exists and is headed `PARTIAL_QUERY`, so waiting on the error alone
    /// would let a half-built answer through and assert against it.
    fn cmd_settled(&mut self, line: &str, body: Option<&str>) -> String {
        for _ in 0..40 {
            let reply = self.cmd(line, body);
            if !reply.contains("unreadable") && !reply.contains("PARTIAL_QUERY") {
                return reply;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("'{line}' never finished rebuilding");
    }

    fn mode(&mut self) -> String {
        self.cmd("stats replication", None)
            .lines()
            .find(|l| l.starts_with("STAT mode "))
            .map(|l| l.trim().rsplit(' ').next().unwrap().to_owned())
            .unwrap_or_else(|| "none".to_owned())
    }
}

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

fn vectors_in(node: &mut Node, key: &str) -> Option<u32> {
    map_head(node, key).map(|(_, elements)| elements.saturating_sub(1))
}

fn map_head(node: &mut Node, key: &str) -> Option<(u32, u32)> {
    let reply = node.cmd(&format!("mop get {key} 0 0"), None);
    let head = reply.lines().next()?.trim().to_owned();
    let mut parts = head.split_whitespace();
    if parts.next()? != "VALUE" {
        return None;
    }
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The hit count out of a `vsim` reply's header line: `QUERY <n> <hits>` and
/// `PARTIAL_QUERY <n> <hits> <done>/<total>` both carry it as the third
/// token (see `query_header` in `src/handler/cmd/search.rs`). `None` for
/// anything else, including an error line -- which is what lets `await_vsim`
/// below treat "not answered yet" and "answered with an error" alike while a
/// replica is still catching up.
fn query_hits(reply: &str) -> Option<usize> {
    reply
        .lines()
        .next()?
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// Whether a `vsim` reply was answered from a graph still rebuilding
/// (`PARTIAL_QUERY`, see `cmd_settled`'s doc above) rather than a settled one.
fn is_partial(reply: &str) -> bool {
    reply.trim_start().starts_with("PARTIAL_QUERY")
}

/// Polls `vsim VECTOR` on `node` until it reports at least `want` hits for
/// `index` from a settled (non-`PARTIAL_QUERY`) graph, or panics after
/// `timeout`.
///
/// Neither arcus's own Map replication nor this crate's delta stream (design
/// §7) gives a test anything to wait on directly -- there is no ack, by
/// design (§3: "nothing is acknowledged"). Polling substitutes for that,
/// rather than picking a fixed sleep long enough to not be flaky and too long
/// to notice a real regression either way.
fn await_vsim(
    node: &mut Node,
    index: &str,
    k: usize,
    query: &str,
    want: usize,
    timeout: Duration,
) -> String {
    let line = format!("vsim VECTOR {index} {k} {} 4", query.len());
    let deadline = Instant::now() + timeout;
    loop {
        let reply = node.cmd(&line, Some(query));
        if !is_partial(&reply) && query_hits(&reply).is_some_and(|hits| hits >= want) {
            return reply;
        }
        if Instant::now() >= deadline {
            panic!("'{line}' never reported {want} hit(s) within {timeout:?}; last reply: {reply}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn stats_replication_reports_the_role() {
    let (mut master, mut slave) = pair();
    assert_eq!(master.mode(), "master");
    assert_eq!(slave.mode(), "slave");
}

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

#[test]
fn item_flags_replicate_verbatim() {
    let key = index_name("repl_flags");
    let (mut master, mut slave) = pair();
    const MARKER: u32 = 0x4156_0001;

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

// This test used to be `the_slave_has_no_index` and asserted the opposite of
// what follows: that `vsim` against a replica came back `ERROR`. That was
// true before the fix design §4.6 depends on -- `Store::ping_slave` could
// never detect it was running on a replica (it probes by deleting
// `arcus:repl-probe`, and `rp_before_check` answers every `arcus:`-prefixed
// key before it reaches the replication check, so the probe always said "not
// a replica"). `recovery::stamp` then attempted the ownership write only a
// master may make, the engine's replication gate refused it under the
// index's own (unprefixed) name -- which *is* checked -- and the error
// reached `resolve`. A replica holding a Map for an index could not build a
// graph for it at all; every read failed. Goal 2 of the whole design is that
// a replica is readable, so this is now the regression test for the defect
// that made it not be, not a pin of the old refusal.
#[test]
fn a_replica_serves_reads_for_a_map_it_holds() {
    let index = index_name("repl_reads");
    let (mut master, mut slave) = pair();
    master.cmd(&format!("vdrop {index}"), None);
    assert_eq!(
        master.cmd(&format!("vcreate {index} 4 METRIC l2"), None),
        "CREATED"
    );
    assert_eq!(
        master.cmd(&format!("vadd {index} v1 7 4"), Some("1 0 0 0")),
        "STORED"
    );

    let reply = await_vsim(&mut slave, &index, 1, "1 0 0 0", 1, Duration::from_secs(10));
    assert!(
        reply.contains("v1"),
        "a replica must be able to build and read its own graph: {reply}"
    );
    assert!(
        slave.cmd("vlist", None).contains(&index),
        "a readable replica also registers the index, the same as a master would"
    );

    master.cmd(&format!("vdrop {index}"), None);
}

/// Pins design §7's convergence, not just the one-shot snapshot the previous
/// test stops at: a write that lands *after* the replica has already caught
/// up must still reach it, through the ordinary per-id `Msg::Delta` ->
/// `slave::upsert` path rather than whatever path (`Msg::Snapshot` on
/// connect, or the registry-miss branch of `converge`) brought the first
/// batch across.
#[test]
fn a_replica_graph_converges_after_writes_to_the_master() {
    let index = index_name("repl_converge");
    let (mut master, mut slave) = pair();
    seed(&mut master, &index);

    let reply = await_vsim(&mut slave, &index, 3, "1 0 0 0", 3, Duration::from_secs(10));
    for id in ["v1", "v2", "v3"] {
        assert!(
            reply.contains(id),
            "replica graph is missing {id} after the initial batch: {reply}"
        );
    }

    assert_eq!(
        master.cmd(&format!("vadd {index} v4 7 4"), Some("0 1 0 0")),
        "STORED"
    );
    let reply = await_vsim(&mut slave, &index, 4, "1 0 0 0", 4, Duration::from_secs(10));
    assert!(
        reply.contains("v4"),
        "a write after the replica had already converged must reach it too: {reply}"
    );

    master.cmd(&format!("vdrop {index}"), None);
}

// This test used to be `a_promoted_node_recovers_its_index`, and reached for
// `cmd_settled` -- which exists specifically to tolerate a `PARTIAL_QUERY`
// answer from a graph still rebuilding -- to get past the promoted node's
// read. That framing predates this design: goal 3 is that switchover is
// immediate because the replica's graph was *already* current, kept that way
// by the delta stream while it was still a replica, not because promotion
// itself now knows how to rebuild faster. A promoted node paying a rebuild
// here would be the old behaviour surviving under a new name, so this
// version waits for the replica to actually converge before switching over,
// then asserts promotion serves without a rebuild step at all.
#[test]
fn a_promoted_node_serves_without_rebuilding() {
    let index = index_name("repl_promote");
    let (mut master, mut slave) = pair();
    seed(&mut master, &index);

    // Converge *before* the switchover -- otherwise a replica that has not
    // caught up yet would fall back to `converge`'s own rebuild-from-Map
    // once promoted (design §4.2's promotion self-check), which is exactly
    // the path this test means to rule out, not exercise.
    await_vsim(&mut slave, &index, 3, "1 0 0 0", 3, Duration::from_secs(10));

    assert_eq!(master.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    let (mut promoted, _) = pair();
    assert_eq!(
        vectors_in(&mut promoted, &index),
        Some(3),
        "the promoted node must still hold every vector"
    );

    let started = Instant::now();
    let reply = promoted.cmd(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    assert!(
        !is_partial(&reply),
        "a promoted node whose graph was already converged as a replica must not answer \
         mid-rebuild: {reply}"
    );
    for id in ["v1", "v2", "v3"] {
        assert!(reply.contains(id), "promoted node is missing {id}: {reply}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a warm graph answers far under this bound; {:?} suggests a rebuild ran anyway",
        started.elapsed()
    );
    assert!(
        promoted.cmd("vlist", None).contains(&index),
        "and the index is still registered post-promotion"
    );

    promoted.cmd(&format!("vdrop {index}"), None);
}

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

    let hits = promoted.cmd_settled(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    for id in ["v1", "v2", "v3"] {
        assert!(hits.contains(id), "rebuilt index is missing {id}: {hits}");
    }

    promoted.cmd(&format!("vdrop {index}"), None);
}

#[test]
fn a_double_switchover_does_not_leave_a_stale_index() {
    let index = index_name("repl_stale");
    let (mut first, _) = pair();
    seed(&mut first, &index);
    assert!(first.cmd("vlist", None).contains(&format!("{index} dim=4")));

    assert_eq!(first.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

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

// This test used to be `a_demoted_node_answers_until_the_new_master_takes_over`
// and, after the new master wrote a fourth vector, asserted the demoted node
// would refuse the next read by naming itself a replica. That pinned the
// *previous* design (2026-08-12-arcvector-replication-recovery-design.md),
// where `access::resolve` compared an owner token on every command and
// refused once it disagreed. This design removes exactly that per-command
// check (§4.5): for an already-registered index, `resolve`'s `Some(index)`
// arm returns straight from the registry with no comparison at all (see
// `src/handler/access/mod.rs`). So a demoted node never refuses a read on
// that account any more -- it answers from whatever its own graph holds,
// stale or not (§13: "eventually consistent"), and is expected to catch up
// by reconnecting as a replica of its own successor rather than by refusing
// until it does.
#[test]
fn a_demoted_node_keeps_serving_and_converges_to_the_new_master() {
    let index = index_name("repl_demoted");
    let (mut demoted, _) = pair();
    seed(&mut demoted, &index);

    assert_eq!(demoted.cmd("replication switchover", None), "OK");
    std::thread::sleep(Duration::from_secs(8));

    assert_eq!(
        demoted.mode(),
        "slave",
        "this test is about the node that just lost the role"
    );

    let hits = demoted.cmd(&format!("vsim VECTOR {index} 3 7 4"), Some("1 0 0 0"));
    for id in ["v1", "v2", "v3"] {
        assert!(
            hits.contains(id),
            "nothing has written since the switchover, so the demoted node's graph \
             still matches its Map and {id} must come back: {hits}"
        );
    }

    let (mut promoted, _) = pair();
    assert_eq!(
        promoted.cmd_settled(&format!("vadd {index} v4 7 4"), Some("0 1 0 0")),
        "STORED"
    );

    // The demoted node must now find its successor by re-reading its own
    // local `AV OWNER` -- it must not dial the address it published while it
    // was still master, which is stale the instant it loses the role (design
    // §4.1). If it did dial itself, it would never connect (its own listener
    // is already closed on demotion), never receive v4's delta, and this
    // would time out instead of converging. The new master's heartbeat only
    // republishes `AV OWNER` at most every 10s (§4.2), so this allows for
    // that plus the reconnect and the delta round-trip.
    let reply = await_vsim(
        &mut demoted,
        &index,
        4,
        "1 0 0 0",
        4,
        Duration::from_secs(20),
    );
    assert!(
        reply.contains("v4"),
        "the demoted node must converge to the new master, not stay frozen at what its \
         graph held when it lost the role: {reply}"
    );

    promoted.cmd(&format!("vdrop {index}"), None);
}

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
