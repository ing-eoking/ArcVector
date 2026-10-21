mod common;

use common::{Server, assert_contains, assert_reply, index_name};

macro_rules! session {
    ($server:ident, $client:ident) => {
        let $server = Server::start();
        let mut $client = $server.connect();
    };
}

/// Parses (vkey, score) pairs from `VECTORS <n>` reply, skipping any metadata JSON lines.
fn hits(reply: &str) -> Vec<(String, f64)> {
    let mut lines = reply.split("\r\n");
    let head = lines.next().expect("a reply has a first line");
    assert!(head.starts_with("VECTORS "), "{reply:?}");
    lines
        .filter(|l| !l.is_empty() && *l != "END" && !l.starts_with('{'))
        .map(|l| {
            let mut parts = l.split(' ');
            let vkey = parts.next().unwrap().to_owned();
            let score = parts.next().expect("a score").parse().expect("a number");
            (vkey, score)
        })
        .collect()
}

fn vkeys(reply: &str) -> Vec<String> {
    hits(reply).into_iter().map(|(k, _)| k).collect()
}

fn create_l2(client: &mut common::Client, ix: &str, dim: usize) {
    assert_reply(
        &client.send(&format!("vop create {ix} {dim} 0 metric l2")),
        "CREATED\r\n",
    );
}

#[test]
fn the_extension_loads_and_claims_its_commands() {
    session!(server, client);
    assert_reply(&client.send("vop drop nothing-here"), "NOT_FOUND\r\n");
    assert_contains(&client.send("vnonsense"), "ERROR");
    drop(server);
}

#[test]
fn create_answers_created_then_exists() {
    session!(_server, client);
    let ix = index_name("create");
    assert_reply(&client.send(&format!("vop create {ix} 2 0")), "CREATED\r\n");
    assert_reply(&client.send(&format!("vop create {ix} 2 0")), "EXISTS\r\n");
    assert_reply(&client.send(&format!("vop drop {ix}")), "DROPPED\r\n");
}

#[test]
fn create_on_a_plain_key_is_type_mismatch() {
    session!(_server, client);
    assert_reply(&client.send_body("set it-plain 0 0 1", "x"), "STORED\r\n");
    assert_reply(&client.send("vop create it-plain 2 0"), "TYPE_MISMATCH\r\n");
    assert_reply(&client.send("vop drop it-plain"), "TYPE_MISMATCH\r\n");
}

#[test]
fn upsert_stores_then_replaces() {
    session!(_server, client);
    let ix = index_name("upsert");
    create_l2(&mut client, &ix, 2);
    assert_reply(&client.upsert(&ix, "v1", "0.1 0.2"), "STORED\r\n");
    assert_reply(&client.upsert(&ix, "v1", "5.0 5.0"), "REPLACED\r\n");
    let found = hits(&client.search_vector(&ix, 1, "5.0 5.0", None, false));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "v1");
    assert!(found[0].1.abs() < 1e-6, "{found:?}");
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn upsert_on_a_missing_index_is_not_found() {
    session!(_server, client);
    let ix = index_name("noindex");
    assert_reply(&client.upsert(&ix, "v1", "0.1 0.2"), "NOT_FOUND\r\n");
}

#[test]
fn bin_and_str_vectors_find_the_same_neighbour() {
    session!(_server, client);
    let ix = index_name("binstr");
    create_l2(&mut client, &ix, 2);
    assert_reply(&client.upsert_bin(&ix, "b", &[1.0, 2.0]), "STORED\r\n");
    assert_reply(&client.upsert(&ix, "s", "9.0 9.0"), "STORED\r\n");

    let by_str = hits(&client.search_vector(&ix, 1, "1.0 2.0", None, false));
    assert_eq!(by_str[0].0, "b");
    assert!(by_str[0].1.abs() < 1e-6);

    let bytes: Vec<u8> = [9.0f32, 9.0].iter().flat_map(|c| c.to_le_bytes()).collect();
    let reply = client.send_bytes(&format!("vop search {ix} vector 1 {}", bytes.len()), &bytes);
    assert_eq!(hits(&reply)[0].0, "s");
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn meta_round_trips_through_getmeta_as_json() {
    session!(_server, client);
    let ix = index_name("meta");
    create_l2(&mut client, &ix, 2);
    let json = r#"{"a":1,"s":"x y"}"#;
    assert_eq!(json.len(), 17);
    assert_reply(
        &client.upsert_meta(&ix, "v1", "0.1 0.2", json),
        "STORED\r\n",
    );
    assert_reply(
        &client.getmeta(&ix, "v1"),
        &format!("META v1 17\r\n{json}\r\n"),
    );
    assert_reply(&client.getmeta(&ix, "nobody"), "NOT_FOUND_VECTOR\r\n");
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn setmeta_changes_what_a_filter_matches() {
    session!(_server, client);
    let ix = index_name("setmeta");
    create_l2(&mut client, &ix, 2);
    client.upsert_meta(&ix, "v1", "1.0 0.0", r#"{"cat":"tech"}"#);
    client.upsert_meta(&ix, "v2", "0.0 1.0", r#"{"cat":"news"}"#);

    let tech = Some("cat = tech");
    assert_eq!(
        vkeys(&client.search_vector(&ix, 5, "1.0 0.0", tech, false)),
        ["v1"]
    );

    assert_reply(
        &client.setmeta(&ix, "v2", r#"{"cat":"tech"}"#),
        "STORED\r\n",
    );
    let both = vkeys(&client.search_vector(&ix, 5, "1.0 0.0", tech, false));
    assert_eq!(both, ["v1", "v2"]);

    let found = hits(&client.search_vector(&ix, 1, "0.0 1.0", None, false));
    assert_eq!(found[0].0, "v2");
    assert!(found[0].1.abs() < 1e-6);

    assert_reply(&client.setmeta(&ix, "nobody", "{}"), "NOT_FOUND_VECTOR\r\n");
    assert_reply(
        &client.setmeta(&index_name("setmeta-none"), "v1", "{}"),
        "NOT_FOUND\r\n",
    );
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn search_by_vkey_and_by_vector_rank_by_distance() {
    session!(_server, client);
    let ix = index_name("rank");
    create_l2(&mut client, &ix, 2);
    client.upsert(&ix, "near", "1.0 0.0");
    client.upsert(&ix, "mid", "3.0 0.0");
    client.upsert(&ix, "far", "9.0 0.0");

    let by_vector = hits(&client.search_vector(&ix, 3, "1.0 0.0", None, false));
    let order: Vec<_> = by_vector.iter().map(|h| h.0.as_str()).collect();
    assert_eq!(order, ["near", "mid", "far"]);
    assert!(by_vector[0].1.abs() < 1e-6, "{by_vector:?}");
    assert!(
        by_vector.windows(2).all(|w| w[0].1 <= w[1].1),
        "{by_vector:?}"
    );

    let by_vkey = hits(&client.search_vkey(&ix, "near", 3, None, false));
    assert_eq!(by_vkey[0].0, "near");
    assert!(by_vkey[0].1.abs() < 1e-6);
    assert_eq!(
        vkeys(&client.search_vkey(&ix, "far", 3, None, false)),
        ["far", "mid", "near"]
    );
    assert!(
        client
            .search_vkey(&ix, "near", 3, None, false)
            .ends_with("END\r\n")
    );
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn search_by_a_missing_vkey_is_not_found_vector() {
    session!(_server, client);
    let ix = index_name("novkey");
    create_l2(&mut client, &ix, 2);
    assert_reply(
        &client.search_vkey(&ix, "nobody", 3, None, false),
        "NOT_FOUND_VECTOR\r\n",
    );
    assert_reply(
        &client.search_vkey(&index_name("novkey-none"), "nobody", 3, None, false),
        "NOT_FOUND\r\n",
    );
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn withmeta_appends_metabytes_and_the_json_line() {
    session!(_server, client);
    let ix = index_name("withmeta");
    create_l2(&mut client, &ix, 2);
    let json = r#"{"a":1,"s":"x y"}"#;
    client.upsert_meta(&ix, "v1", "1.0 0.0", json);

    for reply in [
        client.search_vector(&ix, 1, "1.0 0.0", None, true),
        client.search_vkey(&ix, "v1", 1, None, true),
    ] {
        let lines: Vec<_> = reply.split("\r\n").collect();
        assert_eq!(lines[0], "VECTORS 1", "{reply:?}");
        let mut head = lines[1].split(' ');
        assert_eq!(head.next(), Some("v1"));
        assert!(head.next().unwrap().parse::<f64>().unwrap().abs() < 1e-6);
        assert_eq!(head.next(), Some("17"), "{reply:?}");
        assert_eq!(lines[2], json);
        assert_eq!(lines[3], "END");
    }
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn a_filter_with_or_and_quoted_spaces_selects_the_right_rows() {
    session!(_server, client);
    let ix = index_name("filter");
    create_l2(&mut client, &ix, 2);
    client.upsert_meta(&ix, "a", "1.0 0.0", r#"{"city":"new york","n":1}"#);
    client.upsert_meta(&ix, "b", "2.0 0.0", r#"{"city":"seoul","n":2}"#);
    client.upsert_meta(&ix, "c", "3.0 0.0", r#"{"city":"busan","n":3}"#);

    let mut by = |f: &str| {
        let mut v = vkeys(&client.search_vector(&ix, 5, "1.0 0.0", Some(f), false));
        v.sort();
        v
    };
    assert_eq!(by("city = 'new york'"), ["a"]);
    assert_eq!(by("city = seoul or city = busan"), ["b", "c"]);
    assert_eq!(
        by("n >= 2 and city = busan or city = 'new york'"),
        ["a", "c"]
    );
    assert_eq!(by("n!=2"), ["a", "c"]);
    assert!(by("missing = 1").is_empty());
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn every_write_shows_in_the_next_search_on_the_same_connection() {
    session!(_server, client);
    let ix = index_name("read-your-writes");
    create_l2(&mut client, &ix, 2);
    for round in 0..20 {
        let vkey = format!("v{round}");
        assert_reply(&client.upsert(&ix, &vkey, "1.0 0.0"), "STORED\r\n");
        let found = client.search_vector(&ix, 1, "1.0 0.0", None, false);
        assert_contains(&found, &vkey);

        assert_reply(
            &client.send(&format!("vop delete {ix} {vkey}")),
            "DELETED\r\n",
        );
        let gone = client.search_vector(&ix, 1, "1.0 0.0", None, false);
        assert!(
            gone.starts_with("VECTORS 0\r\n"),
            "round {round}: the deleted vector is still found: {gone}"
        );
    }
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn delete_then_delete_again_is_not_found_vector() {
    session!(_server, client);
    let ix = index_name("delete");
    create_l2(&mut client, &ix, 2);
    client.upsert(&ix, "v1", "1.0 0.0");
    assert_reply(&client.send(&format!("vop delete {ix} v1")), "DELETED\r\n");
    assert_reply(
        &client.send(&format!("vop delete {ix} v1")),
        "NOT_FOUND_VECTOR\r\n",
    );
    assert_reply(
        &client.send(&format!("vop delete {} v1", index_name("delete-none"))),
        "NOT_FOUND\r\n",
    );
    assert_reply(
        &client.search_vector(&ix, 5, "1.0 0.0", None, false),
        "VECTORS 0\r\nEND\r\n",
    );
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn drop_then_upsert_is_not_found() {
    session!(_server, client);
    let ix = index_name("dropped");
    create_l2(&mut client, &ix, 2);
    client.upsert(&ix, "v1", "1.0 0.0");
    assert_reply(&client.send(&format!("vop drop {ix}")), "DROPPED\r\n");
    assert_reply(&client.upsert(&ix, "v1", "1.0 0.0"), "NOT_FOUND\r\n");
    assert_reply(&client.send(&format!("vop drop {ix}")), "NOT_FOUND\r\n");
}

#[test]
fn an_index_recreated_under_a_dropped_name_starts_empty_with_its_own_shape() {
    session!(_server, client);
    let ix = index_name("recreated");
    create_l2(&mut client, &ix, 2);
    for n in 0..50 {
        client.upsert(&ix, &format!("old{n}"), "1.0 0.0");
    }
    assert_reply(&client.send(&format!("vop drop {ix}")), "DROPPED\r\n");

    create_l2(&mut client, &ix, 3);
    assert_reply(&client.upsert(&ix, "new", "0.0 1.0 0.0"), "STORED\r\n");
    let reply = client.search_vector(&ix, 10, "0.0 1.0 0.0", None, false);
    assert!(reply.starts_with("VECTORS 1\r\n"), "{reply}");
    assert_contains(&reply, "new");
    assert!(
        !reply.contains("old"),
        "the old index's vectors stay out: {reply}"
    );
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn a_short_body_is_a_bad_data_chunk_and_the_connection_survives() {
    session!(_server, client);
    let ix = index_name("badchunk");
    create_l2(&mut client, &ix, 2);
    assert_contains(
        &client.send_raw(&format!("vop upsert {ix} v1 7 str\r\n1.0 0.0XX")),
        "CLIENT_ERROR bad data chunk",
    );
    assert_reply(&client.upsert(&ix, "v1", "1.0 0.0"), "STORED\r\n");
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn maxcount_full_is_a_server_error() {
    session!(_server, client);
    let ix = index_name("maxcount");
    assert_reply(
        &client.send(&format!("vop create {ix} 2 0 2 metric l2")),
        "CREATED\r\n",
    );
    assert_reply(&client.upsert(&ix, "a", "1.0 0.0"), "STORED\r\n");
    assert_reply(&client.upsert(&ix, "b", "0.0 1.0"), "STORED\r\n");
    assert_reply(
        &client.upsert(&ix, "c", "1.0 1.0"),
        "SERVER_ERROR index is full\r\n",
    );
    assert_reply(&client.upsert(&ix, "a", "0.5 0.0"), "REPLACED\r\n");
    assert_reply(&client.send(&format!("vop delete {ix} b")), "DELETED\r\n");
    assert_reply(&client.upsert(&ix, "c", "1.0 1.0"), "STORED\r\n");
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn concurrent_upserts_of_one_vkey_all_answer() {
    let server = Server::start();
    let ix = index_name("sameupsert");
    let mut setup = server.connect();
    create_l2(&mut setup, &ix, 2);

    std::thread::scope(|scope| {
        for t in 0..8 {
            let server = &server;
            let ix = ix.clone();
            scope.spawn(move || {
                let mut client = server.connect();
                for round in 0..10 {
                    let json = format!(r#"{{"t":{t}}}"#);
                    let reply = client.upsert_meta(&ix, "same", &format!("{t}.0 {round}.0"), &json);
                    assert!(
                        reply == "STORED\r\n" || reply == "REPLACED\r\n",
                        "thread {t} got: {reply:?}"
                    );
                }
            });
        }
    });

    let meta = setup.getmeta(&ix, "same");
    assert!(
        (0..8).any(|t| meta == format!("META same 7\r\n{{\"t\":{t}}}\r\n")),
        "{meta:?}"
    );
    assert_eq!(
        vkeys(&setup.search_vector(&ix, 5, "1.0 1.0", None, false)),
        ["same"]
    );
    setup.send(&format!("vop drop {ix}"));
}

#[test]
fn many_connections_search_the_same_index_at_once() {
    let server = Server::start();
    let ix = index_name("concurrent");
    let mut setup = server.connect();
    create_l2(&mut setup, &ix, 4);
    for i in 0..50 {
        setup.upsert(&ix, &format!("v{i}"), &format!("{i}.0 1.0 2.0 3.0"));
    }

    std::thread::scope(|scope| {
        for t in 0..8 {
            let server = &server;
            let ix = ix.clone();
            scope.spawn(move || {
                let mut client = server.connect();
                for _ in 0..20 {
                    let reply =
                        client.search_vector(&ix, 5, &format!("{t}.0 1.0 2.0 3.0"), None, false);
                    assert!(
                        reply.starts_with("VECTORS 5") && reply.ends_with("END\r\n"),
                        "thread {t} got: {reply}"
                    );
                }
            });
        }
    });

    setup.send(&format!("vop drop {ix}"));
}

#[test]
fn a_flush_takes_the_index_with_it() {
    // `flush_all` marks items expired; index disappears on subsequent lookups.
    session!(_server, client);
    let ix = index_name("flushed");
    create_l2(&mut client, &ix, 2);
    assert_reply(&client.upsert(&ix, "a", "1.0 0.0"), "STORED\r\n");

    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_contains(&client.send("flush_all"), "OK");

    assert_reply(
        &client.search_vector(&ix, 1, "1.0 0.0", None, false),
        "NOT_FOUND\r\n",
    );
    assert_reply(&client.getmeta(&ix, "a"), "NOT_FOUND\r\n");
    assert_reply(&client.upsert(&ix, "b", "0.5 0.5"), "NOT_FOUND\r\n");
    assert_reply(&client.send(&format!("vop delete {ix} a")), "NOT_FOUND\r\n");
    create_l2(&mut client, &ix, 2);
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn an_index_expires_as_a_whole() {
    session!(_server, client);
    let ix = index_name("iexp");
    assert_reply(
        &client.send(&format!("vop create {ix} 2 1 metric l2")),
        "CREATED\r\n",
    );
    assert_reply(&client.upsert(&ix, "a", "1.0 0.0"), "STORED\r\n");

    std::thread::sleep(std::time::Duration::from_secs(2));

    assert_reply(
        &client.search_vector(&ix, 1, "1.0 0.0", None, false),
        "NOT_FOUND\r\n",
    );
    assert_reply(&client.upsert(&ix, "b", "0.5 0.5"), "NOT_FOUND\r\n");
    create_l2(&mut client, &ix, 2);
    client.send(&format!("vop drop {ix}"));
}

#[test]
fn a_huge_result_count_answers_what_the_index_holds() {
    session!(_server, client);
    let ix = index_name("hugenum");
    create_l2(&mut client, &ix, 2);
    for i in 0..5 {
        client.upsert(&ix, &format!("v{i}"), &format!("{i} 0"));
    }
    let reply = client.search_vector(&ix, 2147483647, "0 0", None, false);
    assert_eq!(vkeys(&reply).len(), 5, "{reply}");
    assert_contains(&client.search_vkey(&ix, "v0", 1, None, false), "VECTORS 1");
    client.send(&format!("vop drop {ix}"));
}
