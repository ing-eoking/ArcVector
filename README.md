# ArcVector

Vector similarity search as an **arcus-memcached ASCII protocol extension**.

ArcVector loads into a running arcus server as a dynamic library and adds
`vcreate` / `vadd` / `VSIM` and friends to the ASCII protocol. Vectors live in
arcus **Map collections**, so they inherit the engine's memory accounting,
eviction, TTL and replication; the ANN graph is a
[usearch](https://github.com/unum-cloud/usearch) index kept alongside as a cache.

내부 구조·빌드·복구 로직은 [docs/내부구조.md](docs/내부구조.md), 알면서 안 고친 것들은
[docs/미해결.md](docs/미해결.md)에 있다.

---

## Build

```sh
cargo build --release        # -> target/release/libarcusv.{so,dylib}
make test                    # all tests, in a container with a real server
make unit                    # unit tests only, on the host
```

`make test` is the full suite: the integration tests need an arcus server that
this crate does not build, and the container also exercises the Linux `.so` that
ships rather than the macOS `.dylib` development produces. See
[docker/README.md](docker/README.md).

`build.rs` runs bindgen over the vendored arcus headers in `include/`, so libclang
must be available. usearch compiles a C++ core, so a C++17 toolchain is needed too.

`engine_interface_v1` is a vtable called **by offset**, and its layout follows the
server's `configure` flags — `ENABLE_REPLICATION`, `ENABLE_MIGRATION` and
`ENABLE_CLUSTER_AWARE` each insert members into it. None of the APIs this crate
calls lives inside those blocks; what moves is where two of them sit. So the
bindings have to be generated for the server that will load the library.

One cargo feature per flag:

```sh
cargo build --release --features replication
cargo build --release                          # a server built without it
```

If you have the server's source tree, its `config.h` lists exactly which to pass.

`ARCVECTOR_ENGINE_INCLUDE` points at a different header tree. A wrong pairing is
refused at load time with the rebuild command in the log, rather than crashing the
server — see [docs/내부구조.md §11](docs/내부구조.md).

Load it into arcus with `-X`:

```sh
memcached -E .../default_engine.so -X .../libarcusv.so
```

## Conventions

- Command names, keywords and enum values are **case-insensitive**:
  `VSIM` = `vsim`, `METRIC COS` = `metric cos`.
- Coordinates travel as **whitespace-separated decimal text**, not binary.
- Errors are `CLIENT_ERROR <reason>` when the request was at fault and
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
130-byte element overhead and the 2-byte terminator arcus counts as part of every
collection element:

| quant | f32 | f16 | i8 | b1 |
|---|---|---|---|---|
| max dim | 4,063 | 8,126 | **16,252** | 130,016 |

```
vcreate docs 1024 METRIC cos QUANT i8 MAXCOUNT 1000000
→ CREATED
```

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

With no whitespace to spend, the full 128-byte region is usable — more than the
spaced form ever reached.

### Durability under replication

**A write answers before the replica has acknowledged it, even on a cluster
configured for sync replication.** This applies to every write command here —
`vcreate`, `vadd`, `vdel`, `vdrop` — and it is the standing behaviour, not an
occasional case: measured over 500 `vadd`s, all 500 replied ahead of the
acknowledgement.

The write itself is committed locally and on its way to the replica; what is
missing is the wait. Parking a connection until the acknowledgement arrives needs
a `conn` field that memcached exposes to its own commands and not to extensions,
so a protocol extension cannot do it.

The exposure is narrow but real: a master lost inside that window drops a write
its client was told had succeeded — the same guarantee arcus gives under
`no_sync_mode 1`, applied to vector commands only. Everything else is unaffected;
in particular a failover rebuilds the index from whatever the Map holds, so a
write that did not reach the replica is simply absent rather than corrupting
anything.

[docs/내부구조.md §12](docs/내부구조.md) has the mechanism.

## VSIM

```
VSIM VECTOR <index> <num> <bytes> <dim> [FILTER <n> <term>...]
<coordinates>

VSIM KEY <index> <num> <key> [FILTER <n> <term>...]
```

`<num>` is how many neighbours to return.

- **VECTOR** reads `<bytes>` of coordinate text and splits it into
  `count / dim` query vectors, so several queries share one round trip.
- **KEY** searches with a vector already in the index. No body, and no
  re-encoding — the stored bytes are already quantized.

**Reply**, one group per query:

```
QUERY <query_no> <count>
VALUE <id> <distance> <attrlen>
<attr JSON>
...
END
```

`FILTER <n> <term>...` takes a **term count**, then that many terms. Each term is
a single token, so it must not contain spaces; terms are combined with `AND`.
Operators are `=` `!=` `<` `<=` `>` `>=`, over top-level JSON fields only. A
condition on an absent field is **always false**, `!=` included.

```
VSIM VECTOR docs 10 15 2 FILTER 2 abc>12 def=abc
0.1 0.2 0.3 0.4
→ QUERY 0 10
  VALUE v1 0.12 23
  {"abc":123,"def":"abc"}
  ...
  QUERY 1 7
  ...
  END

VSIM KEY docs 5 v1
→ QUERY 0 5
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

`vsetattr` replaces the whole attribute region, so `<attrlen> 0` with no JSON clears it. The
vector is untouched and the search still finds the id at the same coordinates — the graph
follows the element rather than being rebuilt, which is a key rename rather than the ~370us
insert a `vadd` pays.

```
vsetattr docs v1 20 {"cat":"news","n":2}\r\n
→ STORED

vsetattr docs v1 0\r\n
→ STORED          # attributes cleared
```

## vstats

```
vstats\r\n
```

Reports the memory this module holds **outside** the engine. arcus's `-m` limit
covers slab memory, so the Map items holding vectors are already accounted for;
the usearch graph and the id map are plain heap allocations in the module and are
not. That gap is what this reports. There is no `stats vector` equivalent —
memcached only consults an extension for an *unknown* command, so `stats` cannot
be hooked.

```
vstats
→ STAT indexes 2
  STAT vectors 201
  STAT idmap_bytes 13556
  STAT index_held_bytes 33600720
  STAT index_used_bytes 51384
  STAT attr_bytes_per_vector 128
  STAT docs:vectors 200
  STAT docs:idmap_bytes 13490
  STAT docs:index_held_bytes 16800360
  STAT docs:index_used_bytes 51200
  STAT docs:reserved 1024
  ... one block per index ...
  END
```

Totals first, then a block per index so it is clear which one is growing.

| Field | Meaning |
|---|---|
| `indexes` | live index count |
| `vectors` | live vectors — not the number ever added |
| `idmap_bytes` | the `id ↔ u64` map: entries plus id text. An estimate; excludes hash-map slack |
| `index_held_bytes` | bytes usearch has taken from the allocator. **Chunked** — see below |
| `index_used_bytes` | the part of that carrying real graph nodes and vectors |
| `reserved` | members usearch has room for; grows ahead of `vectors` |
| `attr_bytes_per_vector` | the fixed ATTR region, 128 |

### Why held and used are both reported

usearch allocates its graph and its vectors from two tape allocators that grow in
**8 MiB chunks**. Measured on an otherwise idle index:

| | `index_held_bytes` |
|---|---|
| empty | 23 KB |
| 1 vector | 16.8 MB |
| 1 000 vectors | 16.8 MB |
| 1 000 vectors, dim 1024 | 16.8 MB |

The first insert takes one chunk from each allocator and nothing moves after
that, at any dimension. So held memory alone would report every index as the same
size — which is why `index_used_bytes` exists, and it is the figure that tracks
the vector count.

**The practical consequence: an index holds ~16.8 MB from its first vector
onward, whether it has one vector or a thousand.** A hundred small indexes cost
~1.7 GB of resident memory regardless of the data in them, and none of it is
visible to `-m` or to `vlist`'s `count`. Prefer fewer, larger indexes.

Neither figure falls when vectors are deleted. A tape allocator hands out bytes
in order and has no per-object free, so `vdel` returns the Map element and the
`idmap_bytes` — both of which do drop — but the usearch tape keeps its high-water
mark. Only `vdrop` releases it, by dropping the whole index. So on a workload that
churns ids, `index_used_bytes` reads as total ever inserted rather than currently
live; compare it against `vectors` to tell the two apart.

---

## Limitations

- `vgetattr` cannot read coordinates back, and there is no command that can.
- Filters support `AND` over top-level fields. No `OR`, no parentheses, no nested
  paths, and no spaces inside a term value.
- The usearch graph is not serialized. After a restart it is rebuilt from Map on
  first use.
- Every non-empty index holds ~16.8 MB of usearch allocator chunks regardless of
  its size, and it is invisible to `-m`. Many small indexes are expensive; see
  [vstats](#why-held-and-used-are-both-reported).
