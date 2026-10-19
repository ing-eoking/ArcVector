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
    assert_contains(&listed, "count=0");

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
        "ATTR v1={}\r\nEND\r\n",
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

    let reply = client.send_body(&format!("vadd {ix} bad 7"), "0.11 0.21");
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

    let reply = client.send_body(&format!("vadd {ix} v 7 ATTR 99 {{}}"), "0.1 0.2");
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
fn a_dimension_over_the_element_limit_is_refused_at_create() {
    // A vector is a Map element, so the ceiling is the server's
    // `max_element_bytes` (16K by default), which the engine reports through
    // get_config. `vcreate` sizes the widest element the index can produce --
    // a full ATTR and the vector -- and refuses a dimension that cannot fit,
    // before any `vadd` finds out the hard way.
    session!(_server, client);
    let ix = index_name("toobig");

    let reply = client.send(&format!("vcreate {ix} 6000 QUANT f32"));
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "max_element_bytes");
    assert_reply(&client.send(&format!("vdrop {ix}")), "NOT_FOUND\r\n");
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

    // The metadata is a reserved field inside the index's Map, which no ASCII
    // command can name: its field has a space in it, and the tokeniser splits
    // there. So it never shows up as a vector, and the counts stay honest.
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        "ATTR v1={}\r\nEND\r\n",
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
    let reply = client.send_body(&format!("vadd {ix} v1 3 attr {spaced}"), "1 0");
    assert!(
        reply.contains("no spaces"),
        "a spaced ATTR must name the reason: {reply}"
    );

    let tight = r#"{"cat":"tech"}"#;
    assert_eq!(
        client
            .send_body(&format!("vadd {ix} v1 3 attr {tight}"), "1 0")
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
            .send_body(&format!("vadd {ix} v2 3 attr {full}"), "1 0")
            .trim_end(),
        "STORED"
    );

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_vector_written_straight_into_the_map_is_adopted() {
    // A replica never sees `vadd`. It sees elements, linked by the replication
    // thread, and nobody waits on those: the link callback writes the address
    // down and the pool puts it in the graph a moment later.
    //
    // A plain `mop insert` into the index's Map takes exactly that path.
    session!(_server, client);
    let src = index_name("adoptsrc");
    let dst = index_name("adoptdst");

    client.send(&format!("vcreate {src} 2 METRIC l2"));
    client.send(&format!("vcreate {dst} 2 METRIC l2"));
    assert_reply(&client.vadd(&src, "a", 2, "1.0 0.0"), "STORED\r\n");

    let vector = client
        .elem_value(&format!("arcus_event:{src}"), "a")
        .expect("a vector is an element a client can read");
    assert_reply(
        &client.insert_elem(&format!("arcus_event:{dst}"), "a", &vector),
        "STORED\r\n",
    );

    let mut found = String::new();
    for _ in 0..100 {
        found = client.vsim(&dst, 1, 2, "1.0 0.0");
        if found.contains("a 1") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_contains(&found, "a 1");
    assert_contains(&client.send("vstats"), "STAT vectors_awaiting_metadata 0");

    client.send(&format!("vdrop {src}"));
    client.send(&format!("vdrop {dst}"));
}

#[test]
fn a_vector_is_searchable_as_soon_as_vadd_answers() {
    // `vadd` answers only after the pool has put the vector in the graph, so a
    // client never has to poll for its own write. Many in a row, each searched
    // for straight away, is how a reply that raced the insertion would show.
    session!(_server, client);
    let ix = index_name("readyourwrite");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    for i in 0..50 {
        let id = format!("v{i}");
        let coords = format!("{i}.0 0.0");
        assert_reply(&client.vadd(&ix, &id, 2, &coords), "STORED\r\n");
        assert_contains(&client.vsim(&ix, 1, 2, &coords), &format!("{id} "));
    }

    // Overwriting with a new vector moves it in the graph, not just in the store.
    assert_reply(&client.vadd(&ix, "v0", 2, "100.0 100.0"), "STORED\r\n");
    assert_contains(&client.vsim(&ix, 1, 2, "100.0 100.0"), "v0 ");
    assert_contains(&client.send("vlist"), "count=50");

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_body_the_graph_cannot_read_leaves_the_reference_accounting_straight() {
    // The engine hands over a reference at link only when the callback accepts,
    // and takes one back at unlink when it declines. So the two answers have to
    // agree about one element: declining a vector the graph refused would mean
    // the later unlink releases a reference that was never given.
    //
    // A short body in a well-formed index Map is the way to make the graph
    // refuse one without touching anything else.
    session!(_server, client);
    let ix = index_name("shortbody");
    let map = format!("arcus_event:{ix}");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    assert_reply(&client.vadd(&ix, "good", 2, "1.0 0.0"), "STORED\r\n");

    assert_reply(&client.insert_elem(&map, "bad", b"short"), "STORED\r\n");

    // The store has it, the graph does not, and the search still answers.
    assert!(client.elem_value(&map, "bad").is_some());
    assert_contains(&client.vsim(&ix, 5, 2, "1.0 0.0"), "good 1");

    assert_reply(&client.delete_elem(&map, "bad"), "DELETED\r\n");

    // Whatever the unlink did with that reference, the index and the daemon are
    // still standing and still counting right.
    assert_reply(&client.vadd(&ix, "next", 2, "0.0 1.0"), "STORED\r\n");
    let hits = client.vsim(&ix, 5, 2, "1.0 0.0");
    assert_contains(&hits, "VECTORS 2");
    assert_contains(&hits, "good 1");

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
fn an_index_expires_as_a_whole() {
    // A Map element has no expiry of its own, so `vadd` takes none. The index
    // does: `vcreate ... EXPTIME` is the Map's exptime, and when it passes the
    // next command's probe unlinks the Map and the index goes with it.
    session!(_server, client);
    let ix = index_name("iexp");
    assert_reply(
        &client.send(&format!("vcreate {ix} 2 METRIC l2 EXPTIME 1")),
        "CREATED\r\n",
    );
    assert_reply(&client.vadd(&ix, "a", 2, "1.0 0.0"), "STORED\r\n");
    assert_contains(&client.vsim(&ix, 1, 2, "1.0 0.0"), "a 1");

    std::thread::sleep(std::time::Duration::from_secs(2));

    assert_reply(&client.vsim(&ix, 1, 2, "1.0 0.0"), "NOT_FOUND\r\n");
    assert_reply(&client.vadd(&ix, "b", 2, "0.5 0.5"), "NOT_FOUND\r\n");
    assert_reply(
        &client.send(&format!("vcreate {ix} 2 METRIC l2")),
        "CREATED\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn an_index_takes_no_more_than_its_maxcount() {
    // The engine enforces the limit, as the Map's maxcount. An id already
    // stored is an overwrite and does not grow the index, so it still goes in.
    session!(_server, client);
    let ix = index_name("maxcount");
    assert_reply(
        &client.send(&format!("vcreate {ix} 2 METRIC l2 MAXCOUNT 2")),
        "CREATED\r\n",
    );
    assert_contains(&client.send("vlist"), "maxcount=2");

    assert_reply(&client.vadd(&ix, "a", 2, "1.0 0.0"), "STORED\r\n");
    assert_reply(&client.vadd(&ix, "b", 2, "0.0 1.0"), "STORED\r\n");
    assert_reply(&client.vadd(&ix, "c", 2, "1.0 1.0"), "OVERFLOWED\r\n");
    assert_reply(&client.vadd(&ix, "a", 2, "0.5 0.0"), "STORED\r\n");

    let hits = client.vsim(&ix, 5, 2, "1.0 0.0");
    assert_contains(&hits, "VECTORS 2");
    assert!(!hits.contains("c "), "{hits}");

    // A slot freed is a slot to fill.
    assert_reply(&client.send(&format!("vdel {ix} b")), "DELETED\r\n");
    assert_reply(&client.vadd(&ix, "c", 2, "1.0 1.0"), "STORED\r\n");

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
