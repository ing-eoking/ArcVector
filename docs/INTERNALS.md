# ArcVector internals

How the module is put together, which library APIs it leans on, and why the
non-obvious decisions are the way they are. For the command reference see
[../README.md](../README.md); for the original design record see
[superpowers/specs/2026-08-10-arcvector-usearch-design.md](superpowers/specs/2026-08-10-arcvector-usearch-design.md).

---

## 1. What this is

arcus-memcached can load an **ASCII protocol extension**: a shared library that
registers a descriptor of callbacks and thereby claims command names on the wire.
ArcVector is such an extension. It runs *inside* the daemon, on the daemon's
worker threads, and talks to the storage engine through the same
`engine_interface_v1` vtable memcached itself uses.

Two layers make up an index:

```
query ──▶ usearch HNSW graph ──▶ nearest keys ──▶ arcus Map ──▶ attributes
          (search accelerator)                    (source of truth)
```

They are joined by a key. The graph finds candidates; Map holds the bytes.

---

## 2. Module map

The store holds vectors, the index finds them, and the registry pairs the two:

```
lib.rs        registration and the four memcached callbacks   <- the FFI boundary
  protocol/   the wire — memcached's ASCII protocol
    tokens      read the token array, write the reply
    request     command line -> typed request                  (pure)
    pending     a command waiting for its body
  vector      what a stored vector is: scalar kind + byte layout   (pure)
  filter      attribute filter expressions                         (pure)
  store       arcus Map — holds vectors, the source of truth   (raw pointers)
  index       usearch — finds them, a cache of the store
  registry    the live store/index pairs, and the rebuild
  command     one handler per command
  error       one error type, and who gets blamed for it
```

One file per idea, and one directory — `protocol/`, because the wire really is
three stages: read the tokens, type them, hold the request while its body arrives.

`vector` is the pair `Quant` and `Layout`: how a coordinate is represented and
where those bytes sit in an element. They are one decision, not two, and they
belong to neither external system. `store` writes exactly those bytes into a Map
element and `index` hands exactly those bytes to usearch — which is why rebuilding
an index from the store is lossless, and why splitting them across the two sides
would have been wrong.

Dependencies run one way. `store` and `index` both use `vector`; `index` knows it
is a cache of `store`; `store` never knows `index` exists. `registry` is the only
thing that holds both.

Two boundaries carry most of the weight:Two boundaries carry most of the weight:

**Parsing never touches state.** `protocol::request` turns tokens into `Line` /
`Body` values and stops. `command` handlers take those values and never see a
token. So syntax has exactly one home, and handlers are reachable from unit tests
without a server.

**Raw pointers live only in `store`.** `Store::for_cookie` is the single `unsafe`
constructor; every method below it takes `&self` and returns owned or borrowed Rust
values. `vector` and `filter` are pure and have no idea a daemon exists.

---

## 3. Request lifecycle

memcached hands an extension four callbacks. ArcVector uses all of them.

| Callback | Job |
|---|---|
| `get_name` | identifies the extension |
| `accept` | claim the command; decide how many body bytes to expect |
| `execute` | run the command and answer |
| `abort` | release state when a command dies mid-transfer |

Most commands finish in `execute` straight off the line. `vadd` and
`VSIM VECTOR` announce a byte count and send coordinates afterwards — memcached
calls this an `nread`. For those, `accept` registers a buffer and `execute` is
called a second time with an **empty token array**, which is how the code knows a
body arrived.

### Why `accept` never refuses a malformed line

`accept` has no response handler; it cannot answer the client. Refusing there
would leave the coordinates unread in the connection buffer, and memcached would
parse them as the *next command line* — one bad request corrupts the rest of the
connection.

So `accept` inspects only the byte count. Everything else is parsed too, but a
failure is stored as a `Result` inside the pending entry, and the handler reports
it once the body has been drained. Refusing is reserved for a byte count that
cannot be read at all, because without one there is no way to know how much to
discard.

### Why the ATTR JSON can sit on the command line

memcached's tokenizer
([`mc_util.c:238`](https://github.com/naver/arcus-memcached/blob/master/mc_util.c))
splits on spaces and **overwrites each token-terminating space with `'\0'` in
place**, leaving runs of two or more spaces untouched. The tokens are consecutive
slices of that one buffer, so reading from the first token to the end of the last
and mapping `'\0'` back to `' '` reproduces the client's bytes exactly. JSON
cannot contain a raw NUL, which is what makes the mapping unambiguous. memcached
reassembles command lines the same way when it logs them (`memcached.c:8078`).

The declared `attrlen` drives the read, and the token-derived span is what bounds
it — that check is why a lying length cannot read out of bounds.

Past `MAX_TOKENS` (30) the tokenizer stops splitting and leaves the remainder
undelimited, recording its length in a slot **beyond the array the extension is
handed**. The span cannot be verified there, so that case is refused rather than
guessed at. Measured ceiling: no-space JSON works to the full 128 bytes,
`json.dumps`-style works to 14 fields / 120 bytes, and only a space around every
colon and comma exhausts the budget (6 fields, 67 bytes).

Search filters do not use this route — a free-form expression has no length bound
— which is why `FILTER` takes one token per term instead.

---

## 4. Element layout

One index is one Map item (key = index name). One vector is one Map element
(field = vector id).

```
 off  0    2   3     4       6        8       16              144
     +----+---+-----+-------+--------+-------+---------------+---------------+
     |"AV"|ver|quant|dim u16|alen u16|rsvd 8B| ATTR 128B     | quantized vec |
     +----+---+-----+-------+--------+-------+---------------+---------------+
```

The ATTR region is a **fixed 128 bytes** whatever the actual JSON length, so the
vector always begins at offset 144 and the search predicate can read attributes
from a constant offset with no decoding — one or two cache lines, no arithmetic.

Why 128: a realistic attribute object
(`{"category":"tech","lang":"ko","ts":1723248000,"score":0.87}`) is 62 bytes, so
this leaves room for roughly double. It also wastes *less* slab space than a
smaller region would — at 1024 dimensions with `i8`, `16 + 128 + 1024 = 1168`
fits arcus's 1184-byte class with 1.4% waste, where a 64-byte region gives 1104
and wastes 6.8% in that same class. Cost is 128 MB per million vectors against
1 GB for the vectors themselves.

`alen` records the real JSON length, so the zero padding is never mistaken for
content. The header's magic, version, `dim` and `quant` catch a Map whose elements
were written by an index with different parameters.

### Quantization

Clients always send decimal text. The module converts once and stores the **same
bytes** in both Map and usearch, so rebuilding the index from Map is lossless —
there is no requantization step.

| quant | conversion | usearch scalar |
|---|---|---|
| f32 | as-is | `F32` |
| f16 | IEEE binary16 in an `i16` container | `F16` |
| i8 | L2-normalize, then ×127 clamped | `I8` |
| b1 | `x > 0` → one bit, LSB-first | `B1` |

The b1 bit order matches usearch's `b1x8` addressing so the two agree byte for
byte.

---

## 5. Consistency: one rule

> **The usearch index is a cache rebuildable from Map at any time.
> If it is not in Map, it does not exist.**

Everything awkward follows from that single statement:

- **Restart** — the graph is empty, so the first search rebuilds it from Map.
- **Eviction / TTL expiry** — the Map key is gone, `KEY_ENOENT` surfaces, the
  index is dropped from the registry.
- **Replication slaves** — Map operations replicate normally, so a slave has the
  data and builds its own graph lazily.
- **Partial failure** — if the Map write succeeds and the usearch insert does
  not, the vector is merely invisible to search until the next rebuild. Nothing
  is lost, so the rule is not violated.

Writes therefore always go **Map first**, cache second.

`vadd` re-checks `max_element_bytes` on every call rather than trusting the check
`vcreate` did, because that limit can be lowered at runtime.

---

## 6. Concurrency

arcus runs a fixed set of libevent worker threads (`settings.num_threads`,
default 4). Commands execute on those threads, so concurrency into the module is
bounded by the worker count — and a slow `vsim` blocks its worker's entire event
loop, which is why `k` and `ef_search` want sane bounds.

### The usearch thread-context trap

The Rust binding always calls into C++ with `any_thread()`, which pops a context
from a **fixed-size pool**. When the pool is empty it does **not block** — it
fails:

```cpp
// include/usearch/index_dense.hpp:2140
thread_lock_t thread_lock_(std::size_t thread_id) const {
    if (thread_id != any_thread()) return {*this, thread_id, false};
    std::unique_lock<std::mutex> lock(available_threads_mutex_);
    if (!available_threads_.try_pop(thread_id))
        return {*this, any_thread(), false};   // -> "Reserve capacity ahead of insertions!"
    return {*this, thread_id, true};
}
```

`-t` above 64 only warns, so no constant can be assumed safe. Instead the module
reserves `THREAD_SLOTS` (64) contexts and gates every entry on a semaphore with
exactly that many permits:

```
concurrent entries <= permits == reserved contexts   ⇒   the pop cannot fail
```

The value is not correctness-critical — the invariant holds for any of them — so
it is a throughput/memory tradeoff, not a client-facing setting. It is a constant
for that reason. A regression test drives 16 concurrent searchers through 2
reserved contexts and asserts zero failures.

### Locks

| Structure | Guard | Note |
|---|---|---|
| index registry | `RwLock<HashMap<_, Arc<VectorIndex>>>` | read lock released immediately after cloning the `Arc` |
| `built` flag | `Mutex<bool>` | serializes the lazy rebuild |
| usearch index | `RwLock<Index>` | **write only for `reserve`** |
| key → id | `boxcar::Vec` | lock-free append and read |
| id → key | `RwLock<HashMap>` | write path only |

`add`, `search` and `remove` take the **read** lock: usearch guards concurrent
construction, search and updates internally with striped spin-locks. The `RwLock`
exists solely to make `reserve` exclusive, since that reallocates the node arrays.
Taking the write lock drains all readers, which is exactly the guarantee `reserve`
needs — permits do not have to be reclaimed separately.

`key → id` must be lock-free because the search predicate reads it once per
visited graph node. A `RwLock` there would mean hundreds of atomic RMWs per query
and would block `vadd` for the duration of a search.

**Lock order** is `registry → built → ann → by_id → [engine cache_lock]`, never
the reverse. The engine never calls back into the module, so its lock is always
innermost and always released; a cycle is structurally impossible.

A `vdrop` concurrent with a search is safe: the registry drops its `Arc`, but the
in-flight search holds its own, so the index stays alive until that search
finishes.

---

## 7. Search path

```
VSIM ──▶ usearch filtered_search(query, k, predicate)
             predicate(key):
               id = key_to_id[key]                     lock-free
               store.read_attr_slot(index, id, ...)    engine call, 128 bytes
               filter.matches(attr)                    allocation-free scan
         ──▶ re-read each hit's element for its attributes
         ──▶ QUERY group per query vector
```

The predicate runs **inside** usearch's graph traversal, so its cost multiplies by
the number of visited nodes. Two things keep it small: the ATTR region sits at a
constant offset so nothing is decoded, and the filter is compiled once per query
then evaluated with a zero-allocation scanner over the raw JSON rather than by
building a `serde_json::Value`.

What remains expensive is the engine call itself. `map_elem_get` takes the
engine's **global `cache_lock`** and mallocs its result array even for a single
field ([`coll_map.c:934`](https://github.com/naver/arcus-memcached/blob/master/engines/default/coll_map.c)),
and no collection API returns a single element without that. It is confined behind
`Store::read_attr_slot` so a narrower engine call — one that copies `offset..len`
under the lock, with no malloc and no refcount — would change that method's body
and nothing else.

Tombstoned keys are filtered before the caller sees them, and a candidate that
vanishes mid-search simply fails the predicate.

---

## 8. Libraries and the APIs used

### usearch 2.26 — ANN index

A C++ single-header engine with cxx-based Rust bindings.

| Used | For |
|---|---|
| `Index::new(&IndexOptions)` | `dimensions`, `metric`, `quantization`, `connectivity`, `expansion_add`, `expansion_search`, `multi: false` |
| `reserve_capacity_and_threads(cap, threads)` | capacity **and** the thread-context pool; the plain `reserve` would leave the pool at its default |
| `add::<T>(key, &[T])` | typed per quantization: `f32`, `f16` (via `f16::from_i16s`), `i8`, `b1x8::from_u8s` |
| `filtered_search::<T, F>(query, k, F: Fn(u64) -> bool)` | the predicate hook the attribute filter rides on |
| `remove(key)` | delete; also used before re-adding, since `multi: false` **rejects a duplicate key** |
| `MetricKind` / `ScalarKind` | `Cos`, `L2sq`, `IP`, `Hamming`, `Tanimoto` / `F32`, `F16`, `I8`, `B1` |

Keys are `u64`, so the module keeps its own `String ↔ u64` mapping. Updating an
existing id is therefore `remove` then `add`; a concurrent search can miss that
vector inside the window, which is acceptable because Map is the source of truth.

Not used: `save`/`load` (the graph is rebuilt from Map instead), `exact_search`,
custom metrics.

### boxcar 0.2 — lock-free append-only vector

Backs `key → id`. Chosen for exactly one property: indexed reads and appends
without a lock, on the predicate's hot path.

### serde_json 1 — ATTR validation

Used **only** to verify at `vadd` that ATTR parses and is a JSON *object*. Query
evaluation deliberately avoids it: building a `Value` per visited node would
allocate, so `filter` scans the raw bytes instead.

### bindgen 0.72 — engine ABI

`build.rs` generates the bindings from `include/engine.h` into `OUT_DIR`, and
`lib.rs` pulls them in with `include!`. Writing to `OUT_DIR` rather than into
`src/` keeps the build from touching the source tree, so a read-only checkout
works and two profiles can build the same sources without contending for one
generated file.

`include/` holds only the headers `engine.h` transitively includes;
`include/memcached/*.h` are symlinks to their siblings one level up, which is what
supplies the `memcached/` prefix those headers include by.

### arcus `engine_interface_v1`

| Used | For |
|---|---|
| `map_struct_create` | create the Map backing an index, with `maxcount` and `exptime` |
| `map_elem_alloc` / `map_elem_insert` / `map_elem_free` | write one element |
| `map_elem_get` / `map_elem_release` | read one element or all of them |
| `map_elem_delete` | delete one element |
| `remove` | drop the whole index |
| `get_elem_info` | zero-copy view of an element's field and value |
| `get_config` | `max_element_bytes`, `max_map_size` |

`map_elem_get` allocates the result array with `malloc` and hands back refcounted
elements, so an RAII guard owns the release-and-free and every exit path goes
through it.

**The engine does not enforce `max_element_bytes` on the API path.** Validation
lives only at the protocol layer, and an oversized element corrupts the slab
allocator's accounting (`do_smmgr_free` assertion in `slabs.c`) — which is why
the module checks the limit itself, in `vcreate` and again in every `vadd`.

---

## 9. Errors

One `Error` type, and `Error::blame()` decides the prefix from the error itself
rather than at the call site — `Responder::reply` is the only place an outcome
becomes an ASCII response.

The interesting case is `codec`: an oversized ATTR is the client's fault
(`CLIENT_ERROR`), while a bad magic byte or version means what *we* stored is
wrong (`SERVER_ERROR`).

---

## 10. Testing

111 unit tests need no daemon; 18 integration tests need one.

- `codec`, `quant`, `filter`, `request`, `error` are pure and fully covered:
  layout round-trips, quantization accuracy, filter parsing and evaluation, every
  command-line rejection path.
- `index` exercises usearch directly — add/search/remove, every quantization,
  capacity growth past the initial reservation, and the concurrency invariants.
- `protocol` reconstructs command lines through a stub that reproduces
  memcached's tokenizer, including the NUL-for-space rewrite.
- `store` covers error translation and the slice helpers; the engine calls
  themselves cannot be reached without a daemon.

`tests/` drives a real daemon with the extension loaded — the only way to reach
`store.rs`, since the engine calls are unreachable from unit tests. Each test
starts its own daemon on its own unix socket and kills it on drop. `make test`
runs the whole suite in a container that supplies the daemon; see
`docker/README.md`.

That target is behind the `integration` feature, so a plain `cargo test` does not
build it and says nothing about it — rather than listing tests that never ran.
Requesting the feature asserts a daemon is reachable, so a missing one fails.
There is no skip path: a test reporting success without running is the failure mode
this suite mistook for green twice.

Two hazards the harness guards against, both found by hitting them:

- **A stale library.** `cargo test` builds the rlib the harness links against but
  not the `cdylib` the daemon loads, so it would happily verify old code. The
  harness compares timestamps and fails rather than skipping — a stale result is a
  wrong result, not a missing one.
- **Port collisions.** Allocating a free TCP port and then spawning leaves a
  window for a parallel test to take it, and the daemon that loses exits. Unix
  sockets remove the window entirely.
- **Skipping a broken setup.** Skipping when anything went wrong turned a
  container that shipped a library with no extension entry point into a green
  suite of 18 "passing" tests. Only an *absent* daemon skips now; a daemon that
  will not start, or starts without our extension registered, fails with its own
  output attached.

Lengths the protocol requires are derived from what is actually sent, because a
test that miscounts tests the wrong thing.
