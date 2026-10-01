mod common;

use common::{Server, assert_contains, assert_reply, index_name};

macro_rules! session {
    ($server:ident, $client:ident) => {
        let $server = Server::start();
        let mut $client = $server.connect();
    };
}

#[test]
fn the_extension_loads_and_claims_its_commands() {
    session!(server, client);
    assert_reply(&client.send("vlist"), "END\r\n");
    assert_contains(&client.send("vnonsense"), "ERROR");
    drop(server);
}

#[test]
fn an_index_is_created_once_and_listed() {
    session!(_server, client);
    let ix = index_name("create");

    assert_reply(
        &client.send(&format!("vcreate {ix} 2 METRIC l2 QUANT f32")),
        "CREATED\r\n",
    );
    assert_reply(&client.send(&format!("vcreate {ix} 2")), "EXISTS\r\n");

    let listed = client.send("vlist");
    assert_contains(&listed, &format!("INDEX {ix} dim=2 quant=f32 metric=l2"));
    assert_contains(&listed, "attrbytes=255 count=0");

    assert_reply(&client.send(&format!("vdrop {ix}")), "DROPPED\r\n");
    assert_reply(&client.send(&format!("vdrop {ix}")), "NOT_FOUND\r\n");
}

#[test]
fn a_vector_round_trips_through_the_engine() {
    session!(_server, client);
    let ix = index_name("roundtrip");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    let attr = r#"{"abc":123,"def":"abc"}"#;
    assert_reply(
        &client.vadd_attr(&ix, "v1", 2, "0.1 0.2", attr),
        "STORED\r\n",
    );

    let got = client.send(&format!("vgetattr {ix} v1"));
    assert_reply(&got, &format!("ATTR v1={attr}\r\nEND\r\n"));

    assert_contains(&client.send("vlist"), "count=1");
    assert_reply(&client.send(&format!("vdel {ix} v1")), "DELETED\r\n");
    assert_reply(&client.send(&format!("vgetattr {ix} v1")), "NOT_FOUND\r\n");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn attributes_are_optional_and_come_back_empty() {
    session!(_server, client);
    let ix = index_name("noattr");
    client.send(&format!("vcreate {ix} 2"));

    assert_reply(&client.vadd(&ix, "v1", 2, "0.1 0.2"), "STORED\r\n");
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        "ATTR v1=\r\nEND\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn set_attr_replaces_the_attributes_and_keeps_the_vector() {
    session!(_server, client);
    let ix = index_name("setattr");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    let first = r#"{"cat":"tech"}"#;
    client.vadd_attr(&ix, "v1", 2, "1.0 0.0", first);

    let second = r#"{"cat":"news","n":2}"#;
    assert_reply(
        &client.send(&format!("vsetattr {ix} v1 {second}")),
        "STORED\r\n",
    );
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        &format!("ATTR v1={second}\r\nEND\r\n"),
    );

    assert_contains(&client.vsim(&ix, 1, 2, "1.0 0.0"), "v1 1");

    // 길이를 받지 않으므로 "비운다"를 따로 말할 수 없다. 빈 객체를 쓰는 것이
    // 가장 가까운 표현이고, 그러면 attr은 `{}` 두 바이트가 된다.
    assert_reply(
        &client.send(&format!("vsetattr {ix} v1 {{}}")),
        "STORED\r\n",
    );
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        "ATTR v1={}\r\nEND\r\n",
    );

    assert_reply(
        &client.send(&format!("vsetattr {ix} nobody {{}}")),
        "NOT_FOUND\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn search_ranks_by_distance() {
    session!(_server, client);
    let ix = index_name("rank");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    client.vadd(&ix, "near", 2, "0.1 0.2");
    client.vadd(&ix, "far", 2, "9.0 9.0");

    let hits = client.vsim(&ix, 2, 2, "0.1 0.2");
    assert_contains(&hits, "VECTORS 2");
    let near_at = hits.find("near").expect("near is present");
    let far_at = hits.find("far").expect("far is present");
    assert!(
        near_at < far_at,
        "the closer vector must rank first:\n{hits}"
    );
    assert!(hits.ends_with("END\r\n"), "{hits}");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_batch_of_queries_returns_one_group_each() {
    session!(_server, client);
    let ix = index_name("batch");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    client.vadd(&ix, "a", 2, "0.1 0.2");
    client.vadd(&ix, "b", 2, "9.0 9.0");

    let hits = client.vsim(&ix, 1, 2, "0.1 0.2 9.0 9.0");
    // 질의마다 블록이 하나씩. 헤더에 질의 번호는 더 이상 없다.
    assert_eq!(hits.matches("VECTORS 1").count(), 2, "{hits}");
    let mut blocks = hits.split("VECTORS 1").skip(1);
    assert_contains(blocks.next().unwrap(), "a");
    assert_contains(blocks.next().unwrap(), "b");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_filter_restricts_results_by_attribute() {
    session!(_server, client);
    let ix = index_name("filter");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    for (id, score) in [("low", 10), ("high", 900)] {
        let attr = format!(r#"{{"score":{score}}}"#);
        client.vadd_attr(&ix, id, 2, "0.1 0.2", &attr);
    }

    let hits = client.vsim_filter(&ix, 5, 2, "0.1 0.2", &["score>500"]);
    assert_contains(&hits, "VECTORS 1");
    assert_contains(&hits, "high");
    assert!(!hits.contains("low"), "the filter must exclude it:\n{hits}");

    let none = client.vsim_filter(&ix, 5, 2, "0.1 0.2", &["score>9999"]);
    assert_contains(&none, "VECTORS 0");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn withattr_answers_the_attributes_by_either_route() {
    session!(_server, client);
    let ix = index_name("withattr");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    let attr = r#"{"score":900}"#;
    client.vadd_attr(&ix, "one", 2, "0.1 0.2", attr);

    let plain = client.vsim_withattr(&ix, 5, 2, "0.1 0.2", &[]);
    assert_contains(&plain, &format!("one 1 {attr}"));

    let filtered = client.vsim_withattr(&ix, 5, 2, "0.1 0.2", &["score>500"]);
    assert_contains(&filtered, &format!("one 1 {attr}"));

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn search_by_key_uses_the_stored_vector() {
    session!(_server, client);
    let ix = index_name("bykey");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    client.vadd(&ix, "anchor", 2, "0.1 0.2");
    client.vadd(&ix, "other", 2, "0.2 0.3");

    let hits = client.send(&format!("vsearch key {ix} anchor 2"));
    assert_contains(&hits, "VECTORS 2");
    assert!(
        hits.find("anchor").unwrap() < hits.find("other").unwrap(),
        "{hits}"
    );

    assert_reply(
        &client.send(&format!("vsearch key {ix} missing 2")),
        "NOT_FOUND\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn quantizations_all_survive_a_round_trip_through_the_engine() {
    session!(_server, client);
    for (quant, metric) in [
        ("f32", "cos"),
        ("f16", "cos"),
        ("i8", "cos"),
        ("b1", "hamming"),
    ] {
        let ix = index_name(&format!("quant-{quant}"));
        assert_reply(
            &client.send(&format!("vcreate {ix} 4 QUANT {quant} METRIC {metric}")),
            "CREATED\r\n",
        );
        assert_reply(
            &client.vadd(&ix, "v1", 4, "1.0 1.0 -1.0 -1.0"),
            "STORED\r\n",
        );
        let hits = client.vsim(&ix, 1, 4, "1.0 1.0 -1.0 -1.0");
        assert_contains(&hits, "v1");
        client.send(&format!("vdrop {ix}"));
    }
}

#[test]
fn a_mis_declared_body_length_is_refused_without_storing() {
    session!(_server, client);
    let ix = index_name("chunk");
    client.send(&format!("vcreate {ix} 2"));

    let reply = client.send_body(&format!("vadd {ix} bad 7 2"), "0.11 0.21");
    assert_contains(&reply, "CLIENT_ERROR bad data chunk");

    client.drain();
    assert_contains(&client.send("vlist"), "count=0");

    assert_reply(&client.vadd(&ix, "good", 2, "0.11 0.21"), "STORED\r\n");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_coordinate_count_that_disagrees_with_the_dimension_is_named() {
    session!(_server, client);
    let ix = index_name("dim");
    client.send(&format!("vcreate {ix} 2"));

    // 차원은 인덱스가 정하므로 명령이 말하지 않는다. 좌표 개수가 그 차원의
    // 배수가 아니면 거기서 걸린다.
    let reply = client.vadd(&ix, "v", 3, "0.1 0.2 0.3");
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "2-dimension");
    assert_contains(&client.send("vlist"), "count=0");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_dimension_that_disagrees_with_the_index_is_named() {
    session!(_server, client);
    let ix = index_name("indexdim");
    client.send(&format!("vcreate {ix} 2"));

    let reply = client.vadd(&ix, "v", 3, "0.1 0.2 0.3");
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "2-dimension");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_malformed_attr_is_refused_and_the_connection_survives() {
    session!(_server, client);
    let ix = index_name("attr");
    client.send(&format!("vcreate {ix} 2"));

    let reply = client.send_body(&format!("vadd {ix} v 7 2 ATTR 99 {{}}"), "0.1 0.2");
    assert_contains(&reply, "CLIENT_ERROR");

    let reply = client.vadd_attr(&ix, "v", 2, "0.1 0.2", "[]");
    assert_contains(&reply, "CLIENT_ERROR");

    let big = format!(r#"{{"k":"{}"}}"#, "x".repeat(250));
    let reply = client.vadd_attr(&ix, "v", 2, "0.1 0.2", &big);
    assert_contains(&reply, "CLIENT_ERROR");

    assert_contains(&client.send("vlist"), &format!("INDEX {ix}"));
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn operating_on_a_missing_index_is_a_client_error() {
    session!(_server, client);
    let ix = index_name("absent");
    assert_reply(&client.send(&format!("vgetattr {ix} v1")), "NOT_FOUND\r\n");
    assert_reply(&client.send(&format!("vdel {ix} v1")), "NOT_FOUND\r\n");
    assert_reply(
        &client.send(&format!("vsearch key {ix} v1 1")),
        "NOT_FOUND\r\n",
    );
    assert_reply(&client.vadd(&ix, "v1", 2, "0.1 0.2"), "NOT_FOUND\r\n");
}

#[test]
fn an_incompatible_metric_and_quantization_are_refused() {
    session!(_server, client);
    let ix = index_name("badquant");

    assert_contains(
        &client.send(&format!("vcreate {ix} 8 QUANT b1 METRIC cos")),
        "CLIENT_ERROR",
    );
    assert_reply(&client.send("vlist"), "END\r\n");
}

#[test]
#[ignore = "needs -I below 256K, which corrupts the engine's heap -- see the comment"]
fn a_vector_over_the_server_item_limit_is_refused() {
    // Ignored because the only configuration that can reach the refusal is one
    // the engine cannot survive.
    //
    // `slabs.c` gives slab class 0 -- the one the small-memory manager carves
    // every item out of -- a chunk size of `SM_BLOCK_SIZE` (256K), then sets
    //
    //     p->perslab = config->item_size_max / p->size;
    //
    // With `-I` under 256K that division is 0, so `do_slabs_newslab` computes
    // `len = 0` and calls `malloc(0)`, which returns a valid non-null pointer.
    // The zero-byte block is then registered as a 256K page and the SM manager
    // writes a block header and a tail at `blck + 262144` into it. The heap is
    // corrupt from there on, and the daemon dies somewhere unrelated -- freeing
    // a Vec, spawning a thread, an Arc refcount at address 0x2.
    //
    // Reproduced with a stock server and no extension: 3/3 dead at -I 20k,
    // 128k; 0/3 at 256k, 1m. The boundary is exactly SM_BLOCK_SIZE.
    //
    // The refusal this test wants needs an item over `item_size_max`, and the
    // largest one `vadd` can carry is bounded by the 16384-byte transport limit
    // on the coordinate line -- about 32K. So there is no safe `-I` that also
    // reaches the refusal. Un-ignore once the engine handles `-I` under 256K.

    // A vector is an item now, so the ceiling is the server's item size (-I),
    // not `max_element_bytes`. The engine does not report that number through
    // get_config, so it is the engine that enforces it: `allocate` answers
    // ENGINE_E2BIG when no slab class fits. `vcreate` only catches the obvious
    // case against the 1 MiB default, which no dimension the protocol accepts
    // (1..65535) can exceed -- so the refusal lands on the write.
    let server = Server::start_with(&["-I", "20k"]);
    let mut client = server.connect();
    let ix = index_name("toobig");

    assert_reply(
        &client.send(&format!("vcreate {ix} 6000 QUANT f32")),
        "CREATED\r\n",
    );
    let coords = (0..6000)
        .map(|i| ((i % 7) as f32).to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let reply = client.vadd(&ix, "v1", 6000, &coords);
    assert_contains(&reply, "item size limit");

    client.send(&format!("vdrop {ix}"));
    drop(server);
}

#[test]
fn vstats_reports_module_memory_and_follows_the_vector_count() {
    session!(_server, client);
    let ix = index_name("stats");
    client.send(&format!("vcreate {ix} 4"));

    let stat = |body: &str, key: &str| -> usize {
        let needle = format!("STAT {key} ");
        body.lines()
            .find_map(|l| l.trim_end().strip_prefix(&needle)?.parse().ok())
            .unwrap_or_else(|| panic!("no {key} in:\n{body}"))
    };

    for i in 0..40 {
        assert_reply(
            &client.vadd(&ix, &format!("v{i}"), 4, &format!("{i}.0 1.0 2.0 3.0")),
            "STORED\r\n",
        );
    }

    let full = client.send("vstats");
    assert!(full.ends_with("END\r\n"), "{full}");
    assert_contains(&full, &format!("STAT {ix}:vectors 40"));
    assert_eq!(stat(&full, "attr_bytes_per_vector"), 255);
    let used = stat(&full, &format!("{ix}:index_used_bytes"));
    let held = stat(&full, &format!("{ix}:index_held_bytes"));
    assert!(used > 0, "used {used}");
    assert!(held >= used, "held {held} < used {used}");

    for i in 0..40 {
        client.send(&format!("vdel {ix} v{i}"));
    }
    let empty = client.send("vstats");
    assert_contains(&empty, &format!("STAT {ix}:vectors 0"));

    assert_eq!(stat(&empty, &format!("{ix}:index_held_bytes")), held);
    assert_eq!(stat(&empty, &format!("{ix}:index_used_bytes")), used);

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn many_connections_search_the_same_index_at_once() {
    let server = Server::start();
    let ix = index_name("concurrent");
    let mut setup = server.connect();
    setup.send(&format!("vcreate {ix} 4 METRIC l2"));
    for i in 0..50 {
        setup.vadd(&ix, &format!("v{i}"), 4, &format!("{i}.0 1.0 2.0 3.0"));
    }

    std::thread::scope(|scope| {
        for t in 0..8 {
            let server = &server;
            let ix = ix.clone();
            scope.spawn(move || {
                let mut client = server.connect();
                for _ in 0..20 {
                    let hits = client.vsim(&ix, 5, 4, &format!("{t}.0 1.0 2.0 3.0"));
                    assert!(
                        hits.contains("VECTORS 5") && hits.ends_with("END\r\n"),
                        "thread {t} got: {hits}"
                    );
                }
            });
        }
    });

    setup.send(&format!("vdrop {ix}"));
}

#[test]
fn an_index_answers_after_its_metadata_and_vectors_round_trip() {
    session!(_server, client);
    let ix = index_name("reserved");
    assert_reply(&client.send(&format!("vcreate {ix} 2")), "CREATED\r\n");
    assert_reply(&client.vadd(&ix, "v1", 2, "1 0"), "STORED\r\n");

    // This replaces a test of a property the storage no longer has. Metadata
    // used to be a reserved field inside the index's Map, which no ASCII
    // command could name. It is a key of its own now -- `arcus_event{ix}:` --
    // and a client can reach it, which docs/claude.md records as a deliberate
    // cost of the key shape. What is still worth pinning is that the commands
    // keep answering over the new layout.
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        "ATTR v1=\r\nEND\r\n",
    );
    assert_contains(&client.send("vlist"), &format!("INDEX {ix} dim=2"));
    assert_contains(&client.send("vlist"), "count=1");
    assert_reply(&client.send(&format!("vdrop {ix}")), "DROPPED\r\n");
}

#[test]
fn attr_json_must_arrive_as_one_argument() {
    session!(_server, client);
    let ix = index_name("attrtok");
    client.send(&format!("vcreate {ix} 2"));

    let spaced = r#"{"cat": "tech"}"#;
    let reply = client.send_body(&format!("vadd {ix} v1 3 2 attr {spaced}"), "1 0");
    assert!(
        reply.contains("no spaces"),
        "a spaced ATTR must name the reason: {reply}"
    );

    let tight = r#"{"cat":"tech"}"#;
    assert_eq!(
        client
            .send_body(&format!("vadd {ix} v1 3 2 attr {tight}"), "1 0")
            .trim_end(),
        "STORED"
    );

    let full = format!(
        "{{{}}}",
        (0..14)
            .map(|i| format!("\"k{i}\":{i}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(full.len() <= 255, "{} bytes", full.len());
    assert_eq!(
        client
            .send_body(&format!("vadd {ix} v2 3 2 attr {full}"), "1 0")
            .trim_end(),
        "STORED"
    );

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_replica_builds_its_index_when_the_metadata_lands_after_the_vectors() {
    // A replica never sees `vcreate`. It sees items, and a vector can arrive
    // before the metadata that says what the index is -- so the link callback
    // has to hold the vector until the metadata turns up and the graph exists.
    //
    // Replaying the two items by hand in that order is the only way to produce
    // it from one server: it is exactly what replication delivers.
    let server = Server::start();
    let mut client = server.connect();
    // The metadata's id has a space in it, which the ascii tokeniser would
    // split. Moving that item by hand takes the binary protocol, where a key
    // is a length and bytes.
    let mut binary = server.connect_binary();
    let src = index_name("replicasrc");
    let dst = index_name("replicadst");

    client.send(&format!("vcreate {src} 2 METRIC l2"));
    assert_reply(&client.vadd(&src, "a", 2, "1.0 0.0"), "STORED\r\n");

    let meta = binary
        .get(&format!("arcus_event{{{src}}}: meta"))
        .expect("the index metadata is an item that can be read back");
    let vector = client
        .get_value(&format!("arcus_event{{{src}}}:a"))
        .expect("a vector is an item a client can read");

    // The vector first, with nothing registered under this name.
    assert_reply(
        &client.set_value(&format!("arcus_event{{{dst}}}:a"), &vector),
        "STORED\r\n",
    );
    assert_reply(&client.vsim(&dst, 1, 2, "1.0 0.0"), "NOT_FOUND\r\n");

    // Now the metadata. The index appears and adopts what was waiting.
    assert!(
        binary.set(&format!("arcus_event{{{dst}}}: meta"), &meta),
        "the metadata item is storable under its own key"
    );

    let mut found = String::new();
    for _ in 0..100 {
        found = client.vsim(&dst, 1, 2, "1.0 0.0");
        if found.contains("a") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_contains(&found, "a 1");

    client.send(&format!("vdrop {src}"));
    client.send(&format!("vdrop {dst}"));
}

#[test]
fn a_body_the_graph_cannot_read_leaves_the_reference_accounting_straight() {
    // The engine hands over a reference at link only when the callback accepts,
    // and takes one back at unlink when it declines. So the two answers have to
    // agree about one item: declining a vector the graph refused would mean the
    // later unlink releases a reference that was never given.
    //
    // A short body under a well-formed vector key is the way to make the graph
    // refuse one without touching anything else.
    session!(_server, client);
    let ix = index_name("shortbody");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    assert_reply(&client.vadd(&ix, "good", 2, "1.0 0.0"), "STORED\r\n");

    let bad = format!("arcus_event{{{ix}}}:bad");
    assert_reply(&client.set_value(&bad, b"short"), "STORED\r\n");

    // The store has it, the graph does not, and the search still answers.
    assert_contains(&client.send(&format!("get {bad}")), "VALUE");
    assert_contains(&client.vsim(&ix, 5, 2, "1.0 0.0"), "good 1");

    assert_reply(&client.send(&format!("delete {bad}")), "DELETED\r\n");

    // Whatever the unlink did with that reference, the index and the daemon are
    // still standing and still counting right.
    assert_reply(&client.vadd(&ix, "next", 2, "0.0 1.0"), "STORED\r\n");
    let hits = client.vsim(&ix, 5, 2, "1.0 0.0");
    assert_contains(&hits, "VECTORS 2");
    assert_contains(&hits, "good 1");

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn an_expired_vector_drops_out_of_a_search() {
    // Expiry is lazy: `do_item_get` is what notices, and nothing gets a vector
    // by key during a search, so the item stays linked and the graph goes on
    // holding it. Without a check on the traversal, `vsim` answers with a
    // vector the server would refuse to hand over.
    session!(_server, client);
    let ix = index_name("expiring");

    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    assert_reply(&client.vadd(&ix, "lives", 2, "1.0 0.0"), "STORED\r\n");
    assert_reply(&client.vadd(&ix, "dies", 2, "0.9 0.0"), "STORED\r\n");

    // `vadd` takes no expiry, so rewrite the item itself with one. The replace
    // hands the graph the new item, which is the one that will expire.
    let body = client
        .get_value(&format!("arcus_event{{{ix}}}:dies"))
        .expect("a vector is an item a client can read");
    assert_reply(
        &client.set_value_exptime(&format!("arcus_event{{{ix}}}:dies"), 1, &body),
        "STORED\r\n",
    );

    // Both are still there while the expiry is in the future.
    let before = client.vsim(&ix, 2, 2, "1.0 0.0");
    assert_contains(&before, "lives");
    assert_contains(&before, "dies");

    std::thread::sleep(std::time::Duration::from_secs(2));

    // Nothing has touched the key, so the item is still linked and still in the
    // graph -- the traversal is the only thing that can leave it out.
    let after = client.vsim(&ix, 2, 2, "1.0 0.0");
    assert_contains(&after, "lives");
    assert!(
        !after.contains("dies"),
        "an expired vector came back from vsim: {after:?}"
    );

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_flush_takes_the_index_with_it() {
    // `flush_all` stamps `oldest_live` and then walks each LRU from the newest
    // item, unlinking as it goes -- but it stops at the first item older than
    // the stamp and leaves the rest merely doomed (`items.c`, do_item_flush_expired).
    // `do_item_isvalid` would refuse those, but nothing runs it until somebody
    // asks for the key, so the metadata item stays linked, no unlink callback
    // fires, and the registry would go on serving an index the server considers
    // gone.
    //
    // Asking for the metadata at the command's entrance is what runs it.
    session!(_server, client);
    let ix = index_name("flushed");

    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    assert_reply(&client.vadd(&ix, "a", 2, "1.0 0.0"), "STORED\r\n");
    assert_contains(&client.vsim(&ix, 1, 2, "1.0 0.0"), "a");

    // Load-bearing. `oldest_live` is set to one second ago, so items touched
    // since then are unlinked on the spot and the trigger callback cleans up
    // without anyone's help -- which is every item in a test that flushes the
    // moment it writes. Letting them age past the stamp is what puts the
    // metadata in the lazy case this test is about: flushed, still linked, and
    // invisible until something asks.
    std::thread::sleep(std::time::Duration::from_secs(3));

    assert_contains(&client.send("flush_all"), "OK");

    // Every command that names an index goes through the same lookup.
    assert_reply(&client.vsim(&ix, 1, 2, "1.0 0.0"), "NOT_FOUND\r\n");
    assert_reply(
        &client.send(&format!("vsearch key {ix} a 1")),
        "NOT_FOUND\r\n",
    );
    assert_reply(&client.send(&format!("vgetattr {ix} a")), "NOT_FOUND\r\n");
    assert_reply(
        &client.send(&format!("vsetattr {ix} a {{}}")),
        "NOT_FOUND\r\n",
    );
    assert_reply(&client.send(&format!("vdel {ix} a")), "NOT_FOUND\r\n");
    assert_reply(&client.vadd(&ix, "b", 2, "0.5 0.5"), "NOT_FOUND\r\n");

    // And the name is free again: `vcreate` builds a new index, not `EXISTS`.
    assert_reply(
        &client.send(&format!("vcreate {ix} 2 METRIC l2")),
        "CREATED\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}
#[test]
fn a_search_reaps_the_expired_vectors_it_walks_over() {
    // Leaving an expired vector out of the answer is only half of it. The graph
    // still holds the node, and this crate still holds the reference the link
    // callback handed over -- so the item cannot even be evicted. Nothing else
    // will notice: expiry is lazy, and nobody gets a vector by key.
    //
    // So the search does it. Asking the engine for the key is what runs
    // `do_item_isvalid`, and the unlink that follows comes back as the
    // `EVENT_UNLINK` that takes the node out and gives the reference back --
    // the same path a `vdel` takes, with no separate accounting of its own.
    session!(_server, client);
    let ix = index_name("reaped");

    let vectors = |client: &mut common::Client| -> String {
        let body = client.send("vstats");
        body.lines()
            .find_map(|l| {
                l.trim_end()
                    .strip_prefix(&format!("STAT {ix}:vectors "))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no vector count in:\n{body}"))
    };

    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    assert_reply(&client.vadd(&ix, "lives", 2, "1.0 0.0"), "STORED\r\n");
    assert_reply(&client.vadd(&ix, "dies", 2, "0.9 0.0"), "STORED\r\n");
    assert_eq!(vectors(&mut client), "2");

    let body = client
        .get_value(&format!("arcus_event{{{ix}}}:dies"))
        .expect("a vector is an item a client can read");
    assert_reply(
        &client.set_value_exptime(&format!("arcus_event{{{ix}}}:dies"), 1, &body),
        "STORED\r\n",
    );

    std::thread::sleep(std::time::Duration::from_secs(2));

    // Expired, but nothing has asked for it, so it is still linked and still a
    // node in the graph.
    assert_eq!(vectors(&mut client), "2");

    let hit = client.vsim(&ix, 2, 2, "1.0 0.0");
    assert_contains(&hit, "lives");
    assert!(!hit.contains("dies"), "{hit}");

    // The search hands the key to the sweeper and answers; the cleanup is the
    // sweeper's next round. Waiting for it is the point -- asserting straight
    // after the reply would pass or fail on how long a `vstats` round trip
    // happened to take.
    let mut waited = 0;
    while vectors(&mut client) != "1" && waited < 50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited += 1;
    }
    assert_eq!(
        vectors(&mut client),
        "1",
        "the sweeper never took the expired vector out of the graph"
    );

    // The node leaving the graph is only half of it. The item has to be
    // unlinked too -- that is what `on_unlink` runs on, and so what gives the
    // reference back. An expired item still in the store would mean the search
    // dropped the node on its own and kept the reference forever.
    assert!(
        client
            .get_value(&format!("arcus_event{{{ix}}}:dies"))
            .is_none(),
        "the expired item is still linked, so nothing released its reference"
    );

    // And the live one is untouched by the reaping.
    assert_contains(&client.vsim(&ix, 1, 2, "1.0 0.0"), "lives");
    assert!(
        client
            .get_value(&format!("arcus_event{{{ix}}}:lives"))
            .is_some(),
        "the reaping took a live vector with it"
    );

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn repeated_searches_drain_more_expired_vectors_than_one_batch_holds() {
    // One search reaps at most `REAP_BATCH` of them, so a graph that has been
    // sitting on more than that takes several. This is the case where a
    // reference is most likely to be released twice or not at all, because the
    // same traversal walks over nodes an earlier one already unlinked.
    session!(_server, client);
    let ix = index_name("drain");
    const LIVE: usize = 1;
    const DOOMED: usize = 80;

    let vectors = |client: &mut common::Client| -> usize {
        let body = client.send("vstats");
        body.lines()
            .find_map(|l| {
                l.trim_end()
                    .strip_prefix(&format!("STAT {ix}:vectors "))?
                    .parse()
                    .ok()
            })
            .unwrap_or_else(|| panic!("no vector count in:\n{body}"))
    };

    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    assert_reply(&client.vadd(&ix, "lives", 2, "1.0 0.0"), "STORED\r\n");
    for i in 0..DOOMED {
        let id = format!("d{i}");
        assert_reply(
            &client.vadd(&ix, &id, 2, &format!("0.9 {}", i as f32 / 1000.0)),
            "STORED\r\n",
        );
        let body = client
            .get_value(&format!("arcus_event{{{ix}}}:{id}"))
            .expect("a vector is an item a client can read");
        assert_reply(
            &client.set_value_exptime(&format!("arcus_event{{{ix}}}:{id}"), 1, &body),
            "STORED\r\n",
        );
    }
    assert_eq!(vectors(&mut client), LIVE + DOOMED);

    std::thread::sleep(std::time::Duration::from_secs(2));
    assert_eq!(
        vectors(&mut client),
        LIVE + DOOMED,
        "nothing has asked for them, so they are all still linked"
    );

    // Each search takes a bite and leaves it to the sweeper. None of them may
    // answer with an expired vector.
    let mut rounds = 0;
    loop {
        let before = vectors(&mut client);
        if before == LIVE {
            break;
        }

        let hit = client.vsim(&ix, 5, 2, "1.0 0.0");
        assert_contains(&hit, "lives");
        assert!(!hit.contains("d"), "an expired vector came back: {hit}");
        rounds += 1;

        // Wait for the sweeper to work this search's batch off before asking
        // for another. Without it the loop would spin on a count that has not
        // moved yet and call that progress.
        let mut waited = 0;
        while vectors(&mut client) == before && waited < 50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            waited += 1;
        }
        assert!(
            vectors(&mut client) < before,
            "the sweeper stopped draining at {before}"
        );
        assert!(rounds < 60, "the searches stopped draining at {before}");
    }

    // DOOMED is above `REAP_BATCH`, so no single search could have taken them
    // all -- which is the point of the cap and the reason this test is here.
    assert!(
        rounds >= 2,
        "one search drained {DOOMED} expired vectors; the batch cap did not hold"
    );
    eprintln!("drained {DOOMED} expired vectors in {rounds} searches");

    // The live one survived every round, in the store and in the graph.
    assert_eq!(vectors(&mut client), LIVE);
    assert_contains(&client.vsim(&ix, 5, 2, "1.0 0.0"), "lives");
    assert!(
        client
            .get_value(&format!("arcus_event{{{ix}}}:lives"))
            .is_some()
    );

    // The index still takes writes: the accounting it kept is still usable.
    assert_reply(&client.vadd(&ix, "after", 2, "0.0 1.0"), "STORED\r\n");
    assert_eq!(vectors(&mut client), LIVE + 1);

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_vector_expires_on_its_own_clock() {
    // 만료가 벡터마다 따로 붙는다. `vcreate`의 인덱스 단위 EXPTIME이 빠지고
    // `vadd`가 그 자리를 받은 결과다.
    session!(_server, client);
    let ix = index_name("vexp");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    assert_reply(&client.vadd_exp(&ix, "lives", 0, "1.0 0.0"), "STORED\r\n");
    assert_reply(&client.vadd_exp(&ix, "dies", 1, "0.9 0.0"), "STORED\r\n");

    let before = client.vsim(&ix, 2, 2, "1.0 0.0");
    assert_contains(&before, "lives 1");
    assert_contains(&before, "dies");

    std::thread::sleep(std::time::Duration::from_secs(2));

    let after = client.vsim(&ix, 2, 2, "1.0 0.0");
    assert_contains(&after, "lives 1");
    assert!(
        !after.contains("dies"),
        "an expired vector came back from vsearch: {after:?}"
    );

    client.send(&format!("vdrop {ix}"));
}

/// `<num>`에 프로토콜이 허용하는 가장 큰 값을 줘도 살아남는다.
///
/// 한때 이것 하나로 데몬이 죽었다. usearch의 Rust 바인딩은 검색을 부르기 전에
/// `count`개짜리 결과 버퍼를 잡고 0으로 채우는데, 그 `reserve`는 실패하면
/// 오류가 아니라 abort다 -- 21억은 24GB가 되고, 그 전에 OOM 킬러가 왔다.
/// 이제는 샤드가 담은 노드 수까지만 묻는다. 담은 것보다 많이 답할 수는 없으니
/// 결과는 같고, 쓰는 메모리는 요청이 아니라 가진 데이터에 비례한다.
#[test]
fn a_huge_result_count_answers_what_the_index_holds() {
    session!(_server, client);
    let ix = index_name("hugenum");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    for i in 0..5 {
        assert_reply(
            &client.vadd(&ix, &format!("v{i}"), 2, &format!("{i} 0")),
            "STORED\r\n",
        );
    }

    let hits = client.send_body(&format!("vsearch vector {ix} 2147483647 3"), "0 0");
    assert_contains(&hits, "VECTORS 5");
    assert_contains(&hits, "END");

    // 죽지 않았다는 것이 요점이므로, 뒤이은 명령이 답하는지까지 본다.
    assert_contains(&client.send(&format!("vsearch key {ix} v0 1")), "VECTORS 1");
    client.send(&format!("vdrop {ix}"));
}
