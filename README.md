# ArcVector

Vector similarity search as an **arcus-memcached ASCII protocol extension**.

ArcVector loads into a running arcus daemon as a dynamic library and adds
`vcreate` / `vadd` / `VSIM` and friends to the ASCII protocol. Vectors live in
arcus **Map collections**, so they inherit the engine's memory accounting,
eviction, TTL and replication; the ANN graph is a
[usearch](https://github.com/unum-cloud/usearch) index kept alongside as a cache.

For how it works inside, see [docs/INTERNALS.md](docs/INTERNALS.md).

---

## Build

```sh
cargo build --release        # -> target/release/libarcusv.{so,dylib}
```

`build.rs` runs bindgen over the vendored arcus headers in `include/`, so libclang
must be available. usearch compiles a C++ core, so a C++17 toolchain is needed too.

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

Maximum dimension, with the engine's default 16 KB `max_element_bytes` and the
fixed 144-byte element overhead:

| quant | f32 | f16 | i8 | b1 |
|---|---|---|---|---|
| max dim | 4,060 | 8,120 | **16,240** | 129,920 |

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

`ATTR` is optional, must be a **JSON object**, and is capped at **128 bytes**.

**Replies** `STORED` · `OVERFLOWED` (at `MAXCOUNT`)

```
vadd docs v1 7 2 ATTR 23 {"abc":123,"def":"abc"}
0.1 0.2
→ STORED
```

Whitespace inside the JSON is fine (`{"a": 1, "b": 2}`). Only pathological
spacing — a space around every colon and comma — exhausts memcached's 30-token
line budget, and that is reported as such.

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

## vget · vdel · vdrop · vlist

| Command | Replies |
|---|---|
| `vget <index> <id>` | `VALUE <id> <attrlen>` + attr + `END` · `NOT_FOUND` |
| `vdel <index> <id>` | `DELETED` · `NOT_FOUND` |
| `vdrop <index>` | `DROPPED` · `NOT_FOUND` |
| `vlist` | one `INDEX <name> dim=… quant=… metric=… attrbytes=128 count=… maxcount=…` per index, then `END` |

`vget` returns attributes only, not coordinates.

---

## Limitations

- **Never run against a live daemon.** The engine call path in `src/store.rs` is
  covered by unit tests and code review only; there is no integration test.
- `vget` cannot read coordinates back.
- Filters support `AND` over top-level fields. No `OR`, no parentheses, no nested
  paths, and no spaces inside a term value.
- The usearch graph is not serialized. After a restart it is rebuilt from Map on
  first use.
