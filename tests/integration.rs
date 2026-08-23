//! End-to-end tests behind the `integration` feature: a plain `cargo test` does not build them, and a missing server fails rather than skips.

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
    assert_contains(&listed, "attrbytes=128 count=0");

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
    assert_reply(
        &got,
        &format!("VALUE v1 {}\r\n{attr}\r\nEND\r\n", attr.len()),
    );

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
        "VALUE v1 0\r\n\r\nEND\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

/// `vsetattr` replaces the attributes and leaves the vector where it was — the search still
/// finds the id, and at the same coordinates.
#[test]
fn set_attr_replaces_the_attributes_and_keeps_the_vector() {
    session!(_server, client);
    let ix = index_name("setattr");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    let first = r#"{"cat":"tech"}"#;
    client.vadd_attr(&ix, "v1", 2, "1.0 0.0", first);

    let second = r#"{"cat":"news","n":2}"#;
    assert_reply(
        &client.send(&format!("vsetattr {ix} v1 {} {second}", second.len())),
        "STORED\r\n",
    );
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        &format!("VALUE v1 {}\r\n{second}\r\nEND\r\n", second.len()),
    );

    // The graph followed the element rather than being rebuilt: same id, same place.
    assert_contains(&client.vsim(&ix, 1, 2, "1.0 0.0"), "VALUE v1 0");

    // A zero length clears them.
    assert_reply(&client.send(&format!("vsetattr {ix} v1 0")), "STORED\r\n");
    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        "VALUE v1 0\r\n\r\nEND\r\n",
    );

    assert_reply(
        &client.send(&format!("vsetattr {ix} nobody 2 {{}}")),
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
    assert_contains(&hits, "QUERY 0 2");
    let near_at = hits.find("VALUE near").expect("near is present");
    let far_at = hits.find("VALUE far").expect("far is present");
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
    assert_contains(&hits, "QUERY 0 1");
    assert_contains(&hits, "QUERY 1 1");
    let first = hits.split("QUERY 1").next().unwrap();
    assert_contains(first, "VALUE a");
    let second = hits.split("QUERY 1").nth(1).unwrap();
    assert_contains(second, "VALUE b");
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
    assert_contains(&hits, "QUERY 0 1");
    assert_contains(&hits, "VALUE high");
    assert!(
        !hits.contains("VALUE low"),
        "the filter must exclude it:\n{hits}"
    );

    let none = client.vsim_filter(&ix, 5, 2, "0.1 0.2", &["score>9999"]);
    assert_contains(&none, "QUERY 0 0");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn search_by_key_uses_the_stored_vector() {
    session!(_server, client);
    let ix = index_name("bykey");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    client.vadd(&ix, "anchor", 2, "0.1 0.2");
    client.vadd(&ix, "other", 2, "0.2 0.3");

    let hits = client.send(&format!("VSIM KEY {ix} 2 anchor"));
    assert_contains(&hits, "QUERY 0 2");
    assert!(
        hits.find("VALUE anchor").unwrap() < hits.find("VALUE other").unwrap(),
        "{hits}"
    );

    assert_reply(
        &client.send(&format!("VSIM KEY {ix} 2 missing")),
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
        assert_contains(&hits, "VALUE v1");
        client.send(&format!("vdrop {ix}"));
    }
}

#[test]
fn a_mis_declared_body_length_is_refused_without_storing() {
    session!(_server, client);
    let ix = index_name("chunk");
    client.send(&format!("vcreate {ix} 2"));

    // Deliberately wrong: veclen says 7, the body is 9 bytes. This once stored a truncated vector and desynced the connection.
    let reply = client.send_body(&format!("vadd {ix} bad 7 2"), "0.11 0.21");
    assert_contains(&reply, "CLIENT_ERROR bad data chunk");
    // The surplus bytes are answered as one more nonsense command; discard it.
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

    let reply = client.vadd(&ix, "v", 3, "0.1 0.2");
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "3-dimension");
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
    assert_contains(&reply, "has dimension 2");
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

    let big = format!(r#"{{"k":"{}"}}"#, "x".repeat(200));
    let reply = client.vadd_attr(&ix, "v", 2, "0.1 0.2", &big);
    assert_contains(&reply, "CLIENT_ERROR");

    assert_contains(&client.send("vlist"), &format!("INDEX {ix}"));
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn operating_on_a_missing_index_is_a_client_error() {
    session!(_server, client);
    let ix = index_name("absent");
    assert_contains(&client.send(&format!("vgetattr {ix} v1")), "CLIENT_ERROR");
    assert_contains(&client.send(&format!("vdel {ix} v1")), "CLIENT_ERROR");
    assert_contains(&client.send(&format!("VSIM KEY {ix} 1 v1")), "CLIENT_ERROR");
    assert_contains(&client.vadd(&ix, "v1", 2, "0.1 0.2"), "CLIENT_ERROR");
}

#[test]
fn an_incompatible_metric_and_quantization_are_refused() {
    session!(_server, client);
    let ix = index_name("badquant");
    // b1 carries no magnitude, so cosine is meaningless on it.
    assert_contains(
        &client.send(&format!("vcreate {ix} 8 QUANT b1 METRIC cos")),
        "CLIENT_ERROR",
    );
    assert_reply(&client.send("vlist"), "END\r\n");
}

#[test]
fn a_dimension_over_the_element_limit_is_refused() {
    session!(_server, client);
    let ix = index_name("toobig");
    // f32 tops out at 4060 dimensions with the default 16KB max_element_bytes.
    let reply = client.send(&format!("vcreate {ix} 5000 QUANT f32"));
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "max_element_bytes");
    assert_reply(
        &client.send(&format!("vcreate {ix} 5000 QUANT i8")),
        "CREATED\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn maxcount_stops_inserts_at_the_limit() {
    session!(_server, client);
    let ix = index_name("maxcount");
    client.send(&format!("vcreate {ix} 2 MAXCOUNT 2"));

    for id in ["a", "b"] {
        assert_reply(&client.vadd(&ix, id, 2, "0.1 0.2"), "STORED\r\n");
    }
    assert_reply(&client.vadd(&ix, "c", 2, "0.1 0.2"), "OVERFLOWED\r\n");
    // Replacing an existing id is not a new insert, so it still fits.
    assert_reply(&client.vadd(&ix, "a", 2, "0.3 0.4"), "STORED\r\n");
    client.send(&format!("vdrop {ix}"));
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
    assert_eq!(stat(&full, "attr_bytes_per_vector"), 128);
    let idmap = stat(&full, &format!("{ix}:idmap_bytes"));
    let used = stat(&full, &format!("{ix}:index_used_bytes"));
    let held = stat(&full, &format!("{ix}:index_held_bytes"));
    assert!(idmap > 0 && used > 0, "idmap {idmap} used {used}");
    assert!(held >= used, "held {held} < used {used}");

    for i in 0..40 {
        client.send(&format!("vdel {ix} v{i}"));
    }
    let empty = client.send("vstats");
    assert_contains(&empty, &format!("STAT {ix}:vectors 0"));
    assert_eq!(stat(&empty, &format!("{ix}:idmap_bytes")), 0);
    // The chunks stay held after the vectors are gone, and used is a high-water mark — which is why vstats reports both.
    assert_eq!(stat(&empty, &format!("{ix}:index_held_bytes")), held);
    assert_eq!(stat(&empty, &format!("{ix}:index_used_bytes")), used);

    client.send(&format!("vdrop {ix}"));
}

#[test]
fn many_connections_search_the_same_index_at_once() {
    // The invariants are unit-tested; only here do they run on the server's worker threads.
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
                        hits.contains("QUERY 0 5") && hits.ends_with("END\r\n"),
                        "thread {t} got: {hits}"
                    );
                }
            });
        }
    });

    setup.send(&format!("vdrop {ix}"));
}

/// `AV META` contains a space, which no ASCII `mop` command can express — the earlier `\x01` name relied on a rule of ours instead.
#[test]
fn the_metadata_field_is_unreachable_from_the_protocol() {
    session!(_server, client);
    let ix = index_name("reserved");
    assert_reply(&client.send(&format!("vcreate {ix} 2")), "CREATED\r\n");
    assert_reply(&client.vadd(&ix, "v1", 2, "1 0"), "STORED\r\n");

    // Sent without its body on purpose: the line is refused before the body is read.
    let reply = client.send(&format!("mop insert {ix} AV META 5 0 0 0"));
    assert!(
        reply.contains("CLIENT_ERROR"),
        "a client must not be able to create the reserved field: {reply}"
    );

    let reply = client.send_body(&format!("mop get {ix} 7 1"), "AV META");
    assert!(
        reply.contains("CLIENT_ERROR"),
        "nor request it by name: {reply}"
    );

    let dump = client.send(&format!("mop get {ix} 0 0"));
    assert!(
        dump.contains("AV META") && dump.contains("\"metric\""),
        "a full dump must still show the metadata element: {dump}"
    );

    assert_reply(
        &client.send(&format!("vgetattr {ix} v1")),
        "VALUE v1 0\r\n\r\nEND\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

/// The tokenizer overwrites the terminating space with a NUL, and a NUL is also a byte a client could send, so one token it is.
#[test]
fn attr_json_must_arrive_as_one_argument() {
    session!(_server, client);
    let ix = index_name("attrtok");
    client.send(&format!("vcreate {ix} 2"));

    let spaced = r#"{"cat": "tech"}"#;
    let reply = client.send_body(
        &format!("vadd {ix} v1 3 2 ATTR {} {spaced}", spaced.len()),
        "1 0",
    );
    assert!(
        reply.contains("no spaces"),
        "a spaced ATTR must name the reason: {reply}"
    );

    let tight = r#"{"cat":"tech"}"#;
    assert_eq!(
        client
            .send_body(
                &format!("vadd {ix} v1 3 2 ATTR {} {tight}", tight.len()),
                "1 0"
            )
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
    assert!(full.len() <= 128, "{} bytes", full.len());
    assert_eq!(
        client
            .send_body(
                &format!("vadd {ix} v2 3 2 ATTR {} {full}", full.len()),
                "1 0"
            )
            .trim_end(),
        "STORED"
    );

    client.send(&format!("vdrop {ix}"));
}
