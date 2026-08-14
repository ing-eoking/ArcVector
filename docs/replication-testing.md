# Testing against a replicating pair

`tests/replication.rs` drives a real EE master/slave pair. It is behind the
`replication-tests` cargo feature and is **not** part of `make test`: the container
that suite runs in has no EE daemon, no ZooKeeper and no provisioned pair, and a
green required suite must not imply switchover was covered.

Everything below was verified on macOS/arm64 against `arcus-memcached 0.9.5-E-139`.

---

## 1. The daemon

An EE build with `ENABLE_REPLICATION`. Put the binary and engine somewhere and
note the paths; `data/` in this tree is one option.

The daemon's **compile-time feature set matters** — see §4. Find it out before
building the extension:

| Feature | How to tell |
|---|---|
| `ENABLE_REPLICATION` | `stats replication` answers instead of erroring |
| `SCAN_COMMAND` | `help` lists `scan` among the command types |
| `ENABLE_MIGRATION` | the startup log prints `server_mapping: migration_address=…` |

## 2. ZooKeeper

Arcus discovers its service code, replica group and replication address from ZK,
so the znodes must exist before a node starts. For a pair on ports 11311/11312 in
a group `gav` of service `test`:

```
/arcus_repl/cache_server_mapping/127.0.0.1:11311
    └── test^gav^127.0.0.1:21311^127.0.0.1:31311
/arcus_repl/cache_server_mapping/127.0.0.1:11312
    └── test^gav^127.0.0.1:21312^127.0.0.1:31312
/arcus_repl/group_list/test/gav
/arcus_repl/cache_list/test
```

The child of the mapping znode is parsed by `arcus_parse_server_mapping`
(`arcus_zk.c`) as `<service>^<group>^<repl address>[^<migration address>]`. The
fourth field is read only when the daemon has `ENABLE_MIGRATION`; a daemon without
it ignores the field rather than failing.

`group_list/test/<group>` must exist or the node logs
`Cluster or group path does not exist. Terminating.` and exits. Create it with
`zkCli`:

```sh
zkCli -server 127.0.0.1:2181
create /arcus_repl/cache_server_mapping/127.0.0.1:11311 ""
create /arcus_repl/cache_server_mapping/127.0.0.1:11311/test^gav^127.0.0.1:21311^127.0.0.1:31311 ""
create /arcus_repl/cache_server_mapping/127.0.0.1:11312 ""
create /arcus_repl/cache_server_mapping/127.0.0.1:11312/test^gav^127.0.0.1:21312^127.0.0.1:31312 ""
create /arcus_repl/group_list/test/gav ""
```

Put the pair in its **own group**. Two nodes in one group become a master and a
slave of each other, and a group nobody else uses cannot disturb a cluster that
happens to share the ensemble.

## 3. Starting the pair

```sh
./data/memcached -E ./data/default_engine.so -X ./target/debug/libarcusv.dylib \
                 -l 127.0.0.1 -p 11311 -U 0 -m 256 -z 127.0.0.1:2181 -v
# then the same with -p 11312
```

`-l 127.0.0.1` matters: the node looks itself up in `cache_server_mapping` by the
address it binds, so it must match the znode name. Start them a few seconds apart
— the first to register becomes the master.

Confirm:

```
$ printf 'stats replication\r\n' | nc 127.0.0.1 11311 | head -1
STAT mode master
```

## 4. Building the extension for *that* daemon

The vtable is called by offset and its layout follows the daemon's `configure`
flags, so the bindings must be generated for **this** daemon:

```sh
ARCVECTOR_ENGINE_INCLUDE=/path/to/arcus-memcached-EE/include \
cargo build --features daemon-replication
```

If `include/` already holds the EE tree's headers, the first line is unnecessary.

[engine-abi.md](engine-abi.md) has the full account: which flags matter, how to
read them off the daemon, and what a mismatch looks like. In short, the library
prints the pairing on its first engine call —

```
ArcVector: daemon 0.9.5-E-139, built for ee tree, 76 members,
           features=[replication,scan], headers=include
```

— and a mismatch answers `SERVER_ERROR engine ABI mismatch` with the rebuild
command in the log, rather than taking the daemon down.

## 5. Sync mode stays on

The pair is left in its default sync mode, deliberately.

A sync master answers every write with `ENGINE_EWOULDBLOCK`, which means "the write
happened, do not answer until the slave acknowledges". This module used to read it
as failure and free an element the engine had just linked, which trips
`assert(elem->linked == 0)` in `map_elem_free` and takes the daemon down. Running
these tests with sync on is what keeps that fixed.

One consequence remains: the reply goes out before the acknowledgement, because
setting `c->ewouldblock` — the flag that makes `conn_parse_cmd` park the connection
— is not reachable from an extension. It happens on **every** write, not
occasionally: measured over 500 `vadd`s, `stats replication detail` showed
`sync_wait_count` rise by 501 of 501. The spec's §8.2 has the reasoning and the two
traps to avoid when revisiting it.

## 6. Running

```sh
cargo test --features replication-tests -- --test-threads=1
cargo test --features replication-tests -- --ignored   # the unfixed destructive bug
```

`--test-threads=1` because the tests share one pair and one of them performs a
switchover. Roles are resolved at the start of every test rather than assumed, so
a run that leaves them swapped does not break the next one.

Ports come from `ARCVECTOR_REPL_NODE_A` / `ARCVECTOR_REPL_NODE_B`, defaulting to
11311 and 11312.

One test is `#[ignore]`d because it fails by design:
`vcreate_must_not_destroy_a_replicated_map` pins the bug where `vcreate` drops a
Map it did not create. It turns green when the recovery design lands.

## 7. Tearing down

Kill the daemons and remove the znodes:

```sh
deleteall /arcus_repl/cache_server_mapping/127.0.0.1:11311
deleteall /arcus_repl/cache_server_mapping/127.0.0.1:11312
deleteall /arcus_repl/group_list/test/gav
```

The `cache_list` entries are ephemeral and vanish with the sessions.
