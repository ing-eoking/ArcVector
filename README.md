# ArcVector

Vector similarity search for **arcus-memcached**, as an ASCII protocol extension.

ArcVector loads into a running arcus server as a dynamic library and adds the
`vop` command set to the ASCII protocol. An index is one arcus **Map collection**
whose key is the index name; each vector is a Map element keyed by its vkey. So
vectors inherit the engine's memory accounting, eviction, TTL and replication.
The ANN graph is a [usearch](https://github.com/unum-cloud/usearch) index kept
alongside as a cache.

It targets arcus-memcached-EE (the extension API with asynchronous replies and
per-element item events).

---

## Build

```sh
cargo build --release        # -> target/release/libarcusv.{so,dylib}
cargo test --lib             # unit tests only, on the host

# full suite, in a container with a real server
docker build -f docker/Dockerfile -t arcvector-test .
docker run --rm arcvector-test
```

`build.rs` runs bindgen over the vendored arcus headers in `include/`, so libclang
must be available. usearch compiles a C++ core, so a C++17 toolchain is needed too.

The integration tests need an arcus server this crate does not build, and the
container also exercises the Linux `.so` that ships rather than the macOS `.dylib`
development produces — see [docker/README.md](docker/README.md).

### Matching the server

`engine_interface_v1` is a vtable called **by offset**, and its layout follows the
server's `configure` flags. One cargo feature per flag:

```sh
cargo build --release --features replication
cargo build --release                          # a server built without it
```

`replication` turns `cluster-aware` on with it; the opposite pairing is not a
server that exists. To follow a different server, vendor its headers in and
retranslate — there is no build flag that reads them from somewhere else:

```sh
cp <tree>/config.h <tree>/config_static.h .
cp <tree>/include/memcached/*.h include/memcached/
cargo build --features regen-bindings
```

A wrong pairing is refused at load time, with the rebuild command in the log,
rather than crashing the server. See [docs/내부구조.md §11](docs/내부구조.md).

## Conventions

- Command names and keywords are **case-insensitive**: `VOP` = `vop`, `METRIC L2` = `metric l2`.
- Vector data is `bin` (the default: little-endian `f32`, `vector_bytes == dim * 4`) or
  `str` (whitespace-separated decimal text, `dim` numbers). Quantization is done by the server.
- Metadata is a **JSON object** on the wire and **BSON** in the engine. `getmeta` and
  `withmeta` emit compact JSON (no spaces, key order kept, numbers normalized);
  `metabytes` is the byte length of the JSON line.
- Every body section ends with its own `\r\n`. A section that does not is
  `CLIENT_ERROR bad data chunk`.
- Errors are `CLIENT_ERROR <reason>` when the request was at fault,
  `SERVER_ERROR <reason>` when we were. `NOT_FOUND` means no such index,
  `NOT_FOUND_VECTOR` no such vkey, `TYPE_MISMATCH` a key that exists but is not an index.
- Search scores are **raw metric distances, smaller is closer**: cos `1-cos`,
  l2 squared Euclidean, ip `1-dot`, hamming, tanimoto `1-similarity`.

Index settings (`dim`, `metric`, `quant`, `M`, `efc`, `efs`) live in the Map's value
as a BSON document, so a replica learns them from the replicated Map itself.

---

## Commands

### vop create

```
vop create <index> <dim> <expiretime> [<maxcount>] [metric m] [quant q] [M n] [efc c] [efs s]
```

| Option | Default | Values |
|---|---|---|
| `maxcount` | 4000 | max vectors; above the server's `max_map_size` is `CLIENT_ERROR` |
| `metric` | `cos` | `cos`, `l2`, `ip`, `hamming`, `tanimoto` |
| `quant` | `f32` | `f32`, `f16`, `i8`, `b1` |
| `M` / `efc` / `efs` | 16 / 128 / 64 | HNSW connectivity / expansion on insert / on search |

`<expiretime>` follows the memcached rule (0 = never, small = seconds, large = unix
time) and expires the whole index with its vectors.

**Replies** `CREATED` · `EXISTS` · `TYPE_MISMATCH`

`b1` takes only `hamming` or `tanimoto`, and those two require `b1`. A dimension whose
vector does not fit `max_element_bytes` is refused at create.

### vop upsert

```
vop upsert <index> <vkey> <vector_bytes> [meta <metabytes>] [bin|str]
[<metadata>\r\n]<vector_data>\r\n
```

**Replies** `STORED` (new vkey) · `REPLACED` (existing vkey) · `NOT_FOUND` ·
`TYPE_MISMATCH` · `SERVER_ERROR index is full` (at `maxcount`)

The reply is sent after the vector is in the graph, so the next search from any
client sees it. The index is never created implicitly.

### vop setmeta

```
vop setmeta <index> <vkey> <metabytes>
<metadata>\r\n
```

**Replies** `STORED` · `NOT_FOUND` · `NOT_FOUND_VECTOR` · `TYPE_MISMATCH`

Replaces the metadata and keeps the vector bytes. The graph is not rebuilt.

### vop search

```
vop search <index> vkey <vkey> <num> [filter <filter_bytes>] [withmeta]
[<filter>\r\n]

vop search <index> vector <num> <vector_bytes> [filter <filter_bytes>] [withmeta] [bin|str]
[<filter>\r\n]<vector_data>\r\n
```

`filter`, `withmeta` and `bin|str` may come in any order.

```
VECTORS <count>\r\n
<vkey> <score>\r\n                          without withmeta
<vkey> <score> <metabytes>\r\n<json>\r\n    with withmeta
...
END\r\n
```

**Other replies** `NOT_FOUND` · `NOT_FOUND_VECTOR` (vkey search, unknown vkey) · `TYPE_MISMATCH`

A filter is `<field> <op> <value>` (spaces around `op` optional) with `op` one of
`=` `!=` `<` `<=` `>` `>=`. A value containing spaces is quoted `'...'`. Conditions
join with `and` / `or`; **`and` binds tighter than `or`**. Fields are top-level BSON
fields, and a condition on an absent field is false, `!=` included.

```
vop search docs vector 10 15 filter 28 withmeta str
n >= 2 and city = 'new york'
0.1 0.2 0.3 0.4
```

### vop getmeta

```
vop getmeta <index> <vkey>
```

**Replies** `META <vkey> <metabytes>\r\n<json>\r\n` · `NOT_FOUND` · `NOT_FOUND_VECTOR` · `TYPE_MISMATCH`

### vop delete

```
vop delete <index> <vkey>
```

**Replies** `DELETED` · `NOT_FOUND` · `NOT_FOUND_VECTOR` · `TYPE_MISMATCH`

The reply follows the engine delete; the graph removal happens a moment later, but a
search right after the reply already skips the deleted vector.

### vop drop

```
vop drop <index>
```

**Replies** `DROPPED` · `NOT_FOUND` · `TYPE_MISMATCH`

---

## Replication and failover

Built with `--features replication`, ArcVector replicates its indexes alongside
arcus's own replication, so **a promoted node serves from a warm graph** instead of
rebuilding.

Roles are discovered, not configured. A ten-second heartbeat asks the daemon
itself — `stats replication`'s `mode` — so the same beat detects a switchover in
either direction. The master then streams *which id changed in which index* to
its replicas over plain TCP, and each replica resolves those ids against its own
arcus-replicated Map — identifiers only, never vectors. A graph that drifts from
its Map is repaired against it rather than rebuilt from nothing.

See [docs/복제.md](docs/복제.md) for the flow, the channel and what a full queue means.

A restart is still cold: the graph is not serialized, and is rebuilt from the Map
on first use.

### Write durability

Writes go through the EE asynchronous-reply path: the reply to `upsert`, `setmeta`,
`delete` and `drop` is held until both the graph work and, on a sync-replication
master, the replica's acknowledgement are done. A write is therefore not
acknowledged to the client ahead of the replica.

---

## Memory outside the engine

arcus's `-m` limit covers slab memory, so the Map items holding vectors are
already accounted for. The usearch graph and the address set are plain heap
allocations in the module and are not. There is no command that reports them
(`vstats` is gone, and memcached cannot hook `stats`).

usearch allocates its graph and its vectors from two tape allocators that grow in
**8 MiB chunks**: **an index holds ~16.8 MB from its first vector onward, whether it
has one vector or a thousand.** A hundred small indexes cost ~1.7 GB resident
regardless of the data in them. Prefer fewer, larger indexes.

The memory does not fall when vectors are deleted: a tape allocator has no
per-object free, so `vop delete` returns the Map element but the usearch tape keeps
its high-water mark. Only `vop drop` releases it.

---

## Known limitations

- Metadata is read and written whole; there is no partial update and no command that
  reads the vector back.
- A filter reads top-level fields only: no parentheses, no nested paths.
- A concurrent `upsert` of the same vkey between `setmeta`'s read and its update can
  be overwritten: `setmeta` writes back the vector bytes it read.
- If the graph insert of an `upsert` fails, the element stays stored but unindexed
  and the client gets `SERVER_ERROR`.
- The graph is not serialized. A restart rebuilds it from the Map on first use.
  (A *failover* does not, with `replication`.)
- Every non-empty index holds ~16.8 MB of usearch allocator chunks regardless of
  its size, invisible to `-m`. Many small indexes are expensive.

## Docs

| | |
|---|---|
| [docs/내부구조.md](docs/내부구조.md) | internals, build, recovery |
| [docs/복제.md](docs/복제.md) | how index replication works |
| [docs/미해결.md](docs/미해결.md) | known and unfixed |
| [docs/코드리뷰.md](docs/코드리뷰.md) | review guide — a source excerpt and a checklist per section |
