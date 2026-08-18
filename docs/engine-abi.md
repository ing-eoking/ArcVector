# Building for a particular arcus

ArcVector reaches the engine through `engine_interface_v1`, a struct of function
pointers that C code calls **by position**. Rust names the members, but the
compiler turns each name into an index, and that index is fixed when the bindings
are generated. If the daemon's struct has a different shape, every call after the
first difference lands on the wrong function.

This page is how to build so that does not happen.

---

## 1. Which flags matter, and which do not

Three `configure` flags insert members into structs this crate reads:

| Flag | Adds | Where |
|---|---|---|
| `ENABLE_REPLICATION` | 7 members | `engine_interface_v1`, before `get_item_info` |
| `ENABLE_MIGRATION` | 6 members | same place |
| `ENABLE_CLUSTER_AWARE` | 2 members | `SERVER_CORE_API` |

**No API this crate calls lives inside those blocks.** ArcVector uses no
replication, migration or cluster API — it creates a Map, writes elements, reads
them back. What the flags change is *where* two of those calls sit:
`get_item_info` and `get_elem_info` are declared after the blocks, so their
position moves even though their availability never does.

Flags that do **not** matter: `ENABLE_ZK_INTEGRATION`, `ENABLE_SASL`,
`ENABLE_STICKY_ITEM`. They never appear in the public headers, so ZooKeeper
support changes nothing here.

Two more decide the shape but are **not knobs**, because they live in the tree's
own `types.h` and arrive with the headers: `SCAN_COMMAND` and the
`SUPPORT_BOP_*` family. `JHPARK_OLD_SMGET_INTERFACE` is the reason the OSS and EE
trees differ before any flag is considered — OSS defines it, EE does not.

## 2. Saying which arcus

One cargo feature per flag:

```sh
cargo build --release --features daemon-replication
cargo build --release --features daemon-replication,daemon-cluster-aware
cargo build --release                        # a daemon built with neither
```

The `daemon-` prefix is deliberate: these describe the daemon, not a capability of
this crate. They gate no Rust code — `build.rs` reads `CARGO_FEATURE_*` and turns
each into a `-D` for bindgen, so the whole effect is in the generated
`engine_api.rs`.

Read the daemon's flags off its own `config.h` if you have the source tree:

```sh
$ grep -E 'ENABLE_(REPLICATION|MIGRATION|CLUSTER_AWARE)' /path/to/arcus-memcached/config.h
#define ENABLE_CLUSTER_AWARE 1
#define ENABLE_REPLICATION 1
```

→ `--features daemon-replication,daemon-cluster-aware`.

Without the tree, §1's table tells you what to look for on the running daemon.

## 3. Which headers

`include/memcached/` is vendored — `engine.h` and the nine headers it transitively
includes, nothing else. The directory is named `memcached/` because every one of
them refers to its siblings by that prefix (`#include <memcached/types.h>`), so
the include path is the parent.

That is the same shape as an arcus source tree, which means one can be used
directly:

```sh
ARCVECTOR_ENGINE_INCLUDE=/path/to/arcus-memcached-EE/include cargo build --release
```

To re-vendor by hand, copy the ten headers into `include/memcached/` and nothing
above it. A stray `include/*.h` is not read; a missing `include/memcached/*.h` is
`'memcached/types.h' file not found`.

## 4. What happens when it is wrong

The library reports the pairing on its first engine call:

```
ArcVector: daemon 0.9.5-E-139, built for ee tree, 76 members,
           features=[cluster_aware,replication,scan], headers=include,
           from config:/path/to/arcus-memcached-EE/config.h
```

Read it as a pair — the daemon on the left, the build on the right. `-E` in the
version means an EE daemon; `oss tree` beside it is a mismatch.

Three checks run, and none of them guesses:

1. **Cross-tree is refused outright.** The tree is decided at build time from the
   header text (the OSS header never mentions `rp_cmd`, not even inside an
   `#ifdef`) and at run time from `server_version()`, which lives in
   `SERVER_CORE_API` — a different struct, whose leading members are identical in
   both trees, so it is safe to call however misaligned the engine vtable is. This
   is the pairing that would otherwise `SIGSEGV` in `item_cachedump` before
   anything could look at it.

2. **A null member is refused.** Every member ahead of the optional blocks is
   checked; arcus never ships one unset, so a null means the struct is not the one
   these bindings describe.

3. **A misbehaving call is named.** A member read at the wrong offset usually
   lands on another real function — valid pointer, wrong result. `put_elem`
   catches that case: the engine allocates an element and then fails to describe
   it, which no runtime condition produces.

Anything caught answers `SERVER_ERROR engine ABI mismatch` and logs the rebuild
command. The daemon stays up.

**Case 3 latches.** Once a call has misbehaved, every later vector command answers
the same way instead of running. That is not only for the error message: with a
misaligned vtable the engine's answers cannot be trusted, and one of them —
"this element is missing" — is what the recovery path reads as a damaged index and
answers by **deleting the Map**. Refusing up front is what keeps a build mistake
from costing data. Observed on an EE daemon with a library built without
`ENABLE_REPLICATION`:

```
ArcVector: ENGINE ABI MISMATCH — get_elem_info did not describe the element it allocated.
vcreate → SERVER_ERROR engine ABI mismatch …
vadd    → SERVER_ERROR engine ABI mismatch …
vsim    → SERVER_ERROR engine ABI mismatch …
```

Nothing is deleted, and the fix is a rebuild.

`ARCVECTOR_ABI_DEBUG=1` lists every member the module needs and whether it
resolved.

## 5. `daemon-persistence`, which is not an ABI flag

`ENABLE_PERSISTENCE` lives in `engines/default/*.h`, not `include/memcached/`, so
it moves no vtable member and passes no define. The feature exists for a different
reason: it states that the daemon restores items on restart, so a Map can outlive
this process's graph.

That is the same thing replication implies, and `build.rs` turns on `cfg(recovery)`
when either is set. Neither one means nothing can outlive the graph, and the
ownership token, the per-command metadata read and the rebuild thread all compile
out. An index whose graph is gone then answers `NOT_FOUND`, and a Map that cannot
be rebuilt is named rather than deleted:

```
vcreate docs 128
  → CLIENT_ERROR a Map already exists at 'docs' and this build cannot rebuild an
    index from it; drop it with 'vdrop docs' and create it again
```

Set it if the daemon's `config.h` has `ENABLE_PERSISTENCE`. Getting it wrong costs
no crash — unlike the three above — only a recovery that does not happen.

## 6. The real fix

All of this exists because EE inserts its members *into* the struct. If they were
appended after `errinfo`, the common prefix would be stable and one library would
serve every build. That is a change to EE, with its own design.
