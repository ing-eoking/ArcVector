//! End-to-end tests against a real arcus daemon with the extension loaded.
//!
//! These are the only tests that exercise `store.rs` — the engine call path
//! cannot be reached from unit tests.
//!
//! This target is behind the `integration` feature, because it needs a daemon this
//! crate does not build. `make test` supplies one in a container and turns the
//! feature on; a plain `cargo test` does not build this file at all, and so says
//! nothing about tests it never ran.
//!
//! Asking for the feature is an assertion that a daemon is reachable, so a missing
//! one fails here rather than skipping.

mod common;

use common::{Daemon, assert_contains, assert_reply, index_name};

/// Start a daemon and open one connection to it.
macro_rules! session {
    ($daemon:ident, $client:ident) => {
        let $daemon = Daemon::start();
        let mut $client = $daemon.connect();
    };
}

#[test]
fn the_extension_loads_and_claims_its_commands() {
    session!(daemon, client);
    // Reaching the extension at all proves registration worked.
    assert_reply(&client.send("vlist"), "END\r\n");
    // An unknown verb must not be claimed, so memcached answers instead.
    assert_contains(&client.send("vnonsense"), "ERROR");
    drop(daemon);
}

#[test]
fn an_index_is_created_once_and_listed() {
    session!(_daemon, client);
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
    session!(_daemon, client);
    let ix = index_name("roundtrip");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));

    let attr = r#"{"abc":123,"def":"abc"}"#;
    assert_reply(
        &client.vadd_attr(&ix, "v1", 2, "0.1 0.2", attr),
        "STORED\r\n",
    );

    // The attributes must come back byte for byte, spaces and all.
    let got = client.send(&format!("vget {ix} v1"));
    assert_reply(
        &got,
        &format!("VALUE v1 {}\r\n{attr}\r\nEND\r\n", attr.len()),
    );

    assert_contains(&client.send("vlist"), "count=1");
    assert_reply(&client.send(&format!("vdel {ix} v1")), "DELETED\r\n");
    assert_reply(&client.send(&format!("vget {ix} v1")), "NOT_FOUND\r\n");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn attributes_are_optional_and_come_back_empty() {
    session!(_daemon, client);
    let ix = index_name("noattr");
    client.send(&format!("vcreate {ix} 2"));

    assert_reply(&client.vadd(&ix, "v1", 2, "0.1 0.2"), "STORED\r\n");
    assert_reply(
        &client.send(&format!("vget {ix} v1")),
        "VALUE v1 0\r\n\r\nEND\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn search_ranks_by_distance() {
    session!(_daemon, client);
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
    session!(_daemon, client);
    let ix = index_name("batch");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    client.vadd(&ix, "a", 2, "0.1 0.2");
    client.vadd(&ix, "b", 2, "9.0 9.0");

    // Four coordinates at dim 2 is two query vectors.
    let hits = client.vsim(&ix, 1, 2, "0.1 0.2 9.0 9.0");
    assert_contains(&hits, "QUERY 0 1");
    assert_contains(&hits, "QUERY 1 1");
    // Each query finds its own nearest.
    let first = hits.split("QUERY 1").next().unwrap();
    assert_contains(first, "VALUE a");
    let second = hits.split("QUERY 1").nth(1).unwrap();
    assert_contains(second, "VALUE b");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_filter_restricts_results_by_attribute() {
    session!(_daemon, client);
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

    // A filter no document satisfies yields an empty group, not an error.
    let none = client.vsim_filter(&ix, 5, 2, "0.1 0.2", &["score>9999"]);
    assert_contains(&none, "QUERY 0 0");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn search_by_key_uses_the_stored_vector() {
    session!(_daemon, client);
    let ix = index_name("bykey");
    client.send(&format!("vcreate {ix} 2 METRIC l2"));
    client.vadd(&ix, "anchor", 2, "0.1 0.2");
    client.vadd(&ix, "other", 2, "0.2 0.3");

    let hits = client.send(&format!("VSIM KEY {ix} 2 anchor"));
    assert_contains(&hits, "QUERY 0 2");
    // The anchor is its own nearest neighbour.
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
    session!(_daemon, client);
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

// -- rejection paths, which is where the wire format is easiest to get wrong --

#[test]
fn a_mis_declared_body_length_is_refused_without_storing() {
    session!(_daemon, client);
    let ix = index_name("chunk");
    client.send(&format!("vcreate {ix} 2"));

    // Deliberately wrong: veclen says 7 but "0.11 0.21" is 9 bytes. Not via the
    // length-deriving helper, since getting it wrong is the point. Before the
    // terminator check this stored a truncated vector and desynced the connection.
    let reply = client.send_body(&format!("vadd {ix} bad 7 2"), "0.11 0.21");
    assert_contains(&reply, "CLIENT_ERROR bad data chunk");
    // The two surplus bytes are still in the stream and memcached answers them as
    // one more nonsense command; discard that before carrying on.
    client.drain();
    assert_contains(&client.send("vlist"), "count=0");

    // The same line with the right length works.
    assert_reply(&client.vadd(&ix, "good", 2, "0.11 0.21"), "STORED\r\n");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_coordinate_count_that_disagrees_with_the_dimension_is_named() {
    session!(_daemon, client);
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
    session!(_daemon, client);
    let ix = index_name("indexdim");
    client.send(&format!("vcreate {ix} 2"));

    let reply = client.vadd(&ix, "v", 3, "0.1 0.2 0.3");
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "has dimension 2");
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn a_malformed_attr_is_refused_and_the_connection_survives() {
    session!(_daemon, client);
    let ix = index_name("attr");
    client.send(&format!("vcreate {ix} 2"));

    // Length disagrees with the JSON that follows.
    let reply = client.send_body(&format!("vadd {ix} v 7 2 ATTR 99 {{}}"), "0.1 0.2");
    assert_contains(&reply, "CLIENT_ERROR");

    // Not a JSON object.
    let reply = client.vadd_attr(&ix, "v", 2, "0.1 0.2", "[]");
    assert_contains(&reply, "CLIENT_ERROR");

    // Over the fixed 128-byte region.
    let big = format!(r#"{{"k":"{}"}}"#, "x".repeat(200));
    let reply = client.vadd_attr(&ix, "v", 2, "0.1 0.2", &big);
    assert_contains(&reply, "CLIENT_ERROR");

    // The connection is still usable, which is the point of draining the body.
    assert_contains(&client.send("vlist"), &format!("INDEX {ix}"));
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn operating_on_a_missing_index_is_a_client_error() {
    session!(_daemon, client);
    let ix = index_name("absent");
    assert_contains(&client.send(&format!("vget {ix} v1")), "CLIENT_ERROR");
    assert_contains(&client.send(&format!("vdel {ix} v1")), "CLIENT_ERROR");
    assert_contains(&client.send(&format!("VSIM KEY {ix} 1 v1")), "CLIENT_ERROR");
    assert_contains(&client.vadd(&ix, "v1", 2, "0.1 0.2"), "CLIENT_ERROR");
}

#[test]
fn an_incompatible_metric_and_quantization_are_refused() {
    session!(_daemon, client);
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
    session!(_daemon, client);
    let ix = index_name("toobig");
    // f32 tops out at 4060 dimensions with the default 16KB max_element_bytes.
    let reply = client.send(&format!("vcreate {ix} 5000 QUANT f32"));
    assert_contains(&reply, "CLIENT_ERROR");
    assert_contains(&reply, "max_element_bytes");
    // i8 fits the same dimension, which is the point of quantizing.
    assert_reply(
        &client.send(&format!("vcreate {ix} 5000 QUANT i8")),
        "CREATED\r\n",
    );
    client.send(&format!("vdrop {ix}"));
}

#[test]
fn maxcount_stops_inserts_at_the_limit() {
    session!(_daemon, client);
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
fn many_connections_search_the_same_index_at_once() {
    // The concurrency invariants are unit-tested, but only here do they run on
    // the daemon's own worker threads.
    let daemon = Daemon::start();
    let ix = index_name("concurrent");
    let mut setup = daemon.connect();
    setup.send(&format!("vcreate {ix} 4 METRIC l2"));
    for i in 0..50 {
        setup.vadd(&ix, &format!("v{i}"), 4, &format!("{i}.0 1.0 2.0 3.0"));
    }

    std::thread::scope(|scope| {
        for t in 0..8 {
            let daemon = &daemon;
            let ix = ix.clone();
            scope.spawn(move || {
                let mut client = daemon.connect();
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
