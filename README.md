# ArcVector

Vector similarity search for **arcus-memcached**, as an ASCII protocol extension.

ArcVector loads into a running arcus server as a dynamic library and adds
`vcreate` / `vadd` / `VSIM` and friends to the ASCII protocol. Vectors live in
arcus **Map collections**, so they inherit the engine's memory accounting,
eviction, TTL and replication. The ANN graph is a
[usearch](https://github.com/unum-cloud/usearch) index kept alongside as a cache.

---

## Quick start

```sh
cargo build --release --features replication
memcached -E .../default_engine.so -X .../libarcusv.so
```

```
vcreate docs 1024 METRIC cos QUANT i8
→ CREATED

vadd docs v1 7 2
0.1 0.2
→ STORED

VSIM KEY docs 5 v1
→ QUERY 0 5
  VALUE v1 0
  VALUE v7 0.08
  END
```

## Build

```sh
cargo build --release        # -> target/release/libarcusv.{so,dylib}
make test                    # full suite, in a container with a real server
make unit                    # unit tests only, on the host
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
server that exists. `ARCVECTOR_ENGINE_INCLUDE` points at a different header tree.
If you have the server's source, its `config.h` lists exactly which flags to pass.

A wrong pairing is refused at load time, with the rebuild command in the log,
rather than crashing the server. See [docs/내부구조.md §11](docs/내부구조.md).

## Conventions

- Command names, keywords and enum values are **case-insensitive**:
  `VSIM` = `vsim`, `METRIC COS` = `metric cos`.
- Coordinates travel as **whitespace-separated decimal text**, not binary.
- Errors are `CLIENT_ERROR <reason>` when the request was at fault,
  `SERVER_ERROR <reason>` when we were.

---

## vcreate

```
vcreate <index> <dim> [METRIC <m>] [QUANT <q>] [M <n>] [EFC <n>] [EFS <n>]
                      [MAXCOUNT <n>] [EXPTIME <n>]
```

| Option | Default | Values |
|---|---|---|
| `METRIC` | `cos` | `cos`(`cosine`), `l2`(`l2sq`, `euclidean`), `ip`(`dot`), `hamming`, `tanimoto`(`jaccard`) |
| `QUANT` | `f32` | `f32`, `f16`, `i8`, `b1` |
| `M` | usearch default | HNSW connectivity |
| `EFC` / `EFS` | usearch default | expansion on insert / on search |
| `MAXCOUNT` | engine `max_map_size` | max vectors, 1 … 2147483647 |
| `EXPTIME` | none | TTL on the backing Map |

**Replies** `CREATED` · `EXISTS`

Quantization and metric are not independent. `b1` packs one bit per dimension and
carries no magnitude, so it takes only `hamming` or `tanimoto`; conversely those
two metrics require `b1`. `i8` L2-normalizes before scaling, so distances live in
the normalized space.

Maximum dimension, with the engine's default 16 KB `max_element_bytes`, the fixed
130-byte element overhead and the 2-byte terminator arcus counts per element:

| quant | f32 | f16 | i8 | b1 |
|---|---|---|---|---|
| max dim | 4,063 | 8,126 | **16,252** | 130,016 |

## vadd

```
vadd <index> <id> <veclen> <dim> [ATTR <attrlen> <attr JSON>]
<coordinates>
```

`<veclen>` is the **byte length of the coordinate text** and `<dim>` is the
coordinate count. Both are required: text length does not determine how many
numbers it holds (`0.1 0.2` is 7 bytes and 2 coordinates; `1 2 3` is 5 bytes and
3), so each checks the other.

`ATTR` is optional, must be a **JSON object**, and is capped at **128 bytes**. It
has to arrive as a single argument, so **no spaces inside the JSON** — the command
line is tokenized on spaces and the pieces cannot be reassembled unambiguously.

**Replies** `STORED` · `OVERFLOWED` (at `MAXCOUNT`)

```
vadd docs v1 7 2 ATTR 23 {"abc":123,"def":"abc"}
0.1 0.2
→ STORED
```

## VSIM

```
VSIM VECTOR <index> <num> <bytes> <dim> [FILTER <n> <term>...] [WITHATTR]
<coordinates>

VSIM KEY <index> <num> <key> [FILTER <n> <term>...] [WITHATTR]
```

`<num>` is how many neighbours to return.

- **VECTOR** reads `<bytes>` of coordinate text and splits it into `count / dim`
  query vectors, so several queries share one round trip.
- **KEY** searches with a vector already in the index. No body, and no
  re-encoding — the stored bytes are already quantized.

`FILTER` and `WITHATTR` are both optional and may come in either order.

**Reply**, one group per query. `WITHATTR` decides the `VALUE` line:

```
QUERY <query_no> <count>          without WITHATTR
VALUE <id> <distance>
...
END

QUERY <query_no> <count>          with WITHATTR
VALUE <id> <distance> <attrlen>
<attr JSON>
...
END
```

Without `WITHATTR` a hit costs no engine read — the reply is built from the graph
alone. The trade is that this fastest path (no `FILTER`, no `WITHATTR`) can return
the id of an element deleted between the search and the reply; anything that reads
the element catches that.

`FILTER <n> <term>...` takes a **term count**, then that many terms. Each term is
a single token, so it must not contain spaces; terms are combined with `AND`.
Operators are `=` `!=` `<` `<=` `>` `>=`, over top-level JSON fields only. A
condition on an absent field is **always false**, `!=` included.

```
VSIM VECTOR docs 10 15 2 FILTER 2 abc>12 def=abc WITHATTR
0.1 0.2 0.3 0.4
→ QUERY 0 10
  VALUE v1 0.12 23
  {"abc":123,"def":"abc"}
  ...
  END
```

## vgetattr · vsetattr · vdel · vdrop · vlist

| Command | Replies |
|---|---|
| `vgetattr <index> <id>` | `VALUE <id> <attrlen>` + attr + `END` · `NOT_FOUND` |
| `vsetattr <index> <id> <attrlen> <attr JSON>` | `STORED` · `NOT_FOUND` |
| `vdel <index> <id>` | `DELETED` · `NOT_FOUND` |
| `vdrop <index>` | `DROPPED` · `NOT_FOUND` |
| `vlist` | one `INDEX <name> dim=… quant=… metric=… attrbytes=128 count=… maxcount=…` per index, then `END` |
| `vstats` | module memory, then `END` |

`vgetattr` returns attributes only, not coordinates.

`vsetattr` replaces the whole attribute region, so `<attrlen> 0` with no JSON
clears it. **The graph is not touched**: the region is a fixed width, so the value
going back is exactly as long as the one that came out and the engine writes it in
place — no allocation, no relink, and none of the ~370us HNSW insert a `vadd` pays.

```
vsetattr docs v1 20 {"cat":"news","n":2}
→ STORED

vsetattr docs v1 0
→ STORED          # attributes cleared
```

---

## Replication and failover

Built with `--features replication`, ArcVector replicates its indexes alongside
arcus's own replication, so **a promoted node serves from a warm graph** instead of
rebuilding.

Roles are discovered, not configured. Every node writes a heartbeat key that
arcus's replication gate accepts on the master and refuses on a replica, so the
verdict is the role; the same beat detects switchover in either direction. The
master then streams *which id changed in which index* to its replicas over plain
TCP, and each replica resolves those ids against its own arcus-replicated Map —
identifiers only, never vectors. A replica that falls too far behind rebuilds
rather than chasing.

A restart is still cold: the graph is not serialized, and is rebuilt from the Map
on first use.

### Write durability

**A write answers before the replica has acknowledged it, even on a cluster
configured for sync replication.** This applies to `vcreate`, `vadd`, `vdel` and
`vdrop`, and it is standing behaviour, not an occasional case — measured over 500
`vadd`s, all 500 replied ahead of the acknowledgement.

The write is committed locally and on its way to the replica; what is missing is
the wait. Parking a connection until the acknowledgement arrives needs a `conn`
field memcached exposes to its own commands and not to extensions.

The exposure is narrow but real: a master lost inside that window drops a write
its client was told had succeeded — the same guarantee arcus gives under
`no_sync_mode 1`, applied to vector commands only. Nothing is corrupted by it; the
write is simply absent. [docs/내부구조.md §12](docs/내부구조.md) has the mechanism.

---

## vstats — memory outside the engine

arcus's `-m` limit covers slab memory, so the Map items holding vectors are
already accounted for. The usearch graph and the address set are plain heap
allocations in the module and are not. That gap is what this reports.

```
vstats
→ STAT indexes 2
  STAT vectors 201
  STAT addr_set_bytes 13556
  STAT index_held_bytes 33600720
  STAT index_used_bytes 51384
  STAT attr_bytes_per_vector 128
  STAT docs:vectors 200
  STAT docs:addr_set_bytes 13490
  STAT docs:index_held_bytes 16800360
  STAT docs:index_used_bytes 51200
  STAT docs:reserved 1024
  ... one block per index ...
  END
```

| Field | Meaning |
|---|---|
| `indexes` | live index count |
| `vectors` | live vectors — not the number ever added |
| `addr_set_bytes` | the set of element addresses the graph holds. An estimate; excludes hash-map slack |
| `index_held_bytes` | bytes usearch has taken from the allocator |
| `index_used_bytes` | the part of that carrying real graph nodes and vectors |
| `reserved` | members usearch has room for; grows ahead of `vectors` |
| `attr_bytes_per_vector` | the fixed ATTR region, 128 |

There is no `stats vector` equivalent — memcached only consults an extension for
an *unknown* command, so `stats` cannot be hooked.

### Two figures, because held is chunked

usearch allocates its graph and its vectors from two tape allocators that grow in
**8 MiB chunks**. Measured on an otherwise idle index:

| | `index_held_bytes` |
|---|---|
| empty | 23 KB |
| 1 vector | 16.8 MB |
| 1 000 vectors | 16.8 MB |
| 1 000 vectors, dim 1024 | 16.8 MB |

The first insert takes one chunk from each allocator and nothing moves after that,
at any dimension — so held memory alone would report every index as the same size.
`index_used_bytes` is the figure that tracks the vector count.

**An index holds ~16.8 MB from its first vector onward, whether it has one vector
or a thousand.** A hundred small indexes cost ~1.7 GB resident regardless of the
data in them, and none of it is visible to `-m` or to `vlist`'s `count`. Prefer
fewer, larger indexes.

Neither figure falls when vectors are deleted. A tape allocator hands out bytes in
order and has no per-object free, so `vdel` returns the Map element and the
`addr_set_bytes` — both of which do drop — but the usearch tape keeps its
high-water mark. Only `vdrop` releases it. On a workload that churns ids,
`index_used_bytes` reads as total ever inserted rather than currently live;
compare it against `vectors` to tell the two apart.

---

## Limitations

- `vgetattr` cannot read coordinates back, and there is no command that can.
- Filters support `AND` over top-level fields. No `OR`, no parentheses, no nested
  paths, and no spaces inside a term value.
- Writes answer before the replica acknowledges — see
  [Write durability](#write-durability).
- The graph is not serialized. A restart rebuilds it from the Map on first use.
  (A *failover* does not, with `replication`.)
- Every non-empty index holds ~16.8 MB of usearch allocator chunks regardless of
  its size, invisible to `-m`. Many small indexes are expensive.

## Docs

| | |
|---|---|
| [docs/내부구조.md](docs/내부구조.md) | internals, build, recovery |
| [docs/미해결.md](docs/미해결.md) | known and unfixed |
| [docs/코드리뷰.md](docs/코드리뷰.md) | review guide — a source excerpt and a checklist per section |
