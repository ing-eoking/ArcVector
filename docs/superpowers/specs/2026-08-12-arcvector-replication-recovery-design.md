# Index recovery: surviving restart, switchover and replication

Design record. Status: **implemented**. Every test named in §6 passes on a live
pair with no ignores.

Supersedes nothing; extends
[2026-08-10-arcvector-usearch-design.md](2026-08-10-arcvector-usearch-design.md)
and the consistency rule in [내부구조.md](../../내부구조.md) §5.

---

## 1. The problem

내부구조.md states one rule:

> The usearch index is a cache rebuildable from Map at any time.
> If it is not in Map, it does not exist.

**The rule is not true today.** An index is described by seven values — `dim`,
`quant`, `metric`, `M`, `EFC`, `EFS`, `maxcount` — and only some of them are in
Map:

| Value | Where it lives | Survives process death? |
|---|---|---|
| `dim`, `quant` | element header, bytes 3–5 | yes |
| `maxcount`, `exptime` | Map item attributes | yes |
| `metric`, `M`, `EFC`, `EFS` | `registry::VectorIndex`, RAM only | **no** |

`create_map` builds its `item_attr` from `mem::zeroed()`, so `flags` is 0 and
carries nothing. `vcreate` puts the rest in the registry and nowhere else.

The consequence appears wherever the Map outlives the process that made it:

- **Replication switchover.** The promoted node has every element and no index.
- **EE persistence restart.** Checkpoint and command log restore the Map;
  `metric` and the HNSW parameters are gone.
- **Any second daemon reading the same data.**

`ensure_built` cannot help. It rebuilds a graph *into an index object that
already exists*; it cannot construct one, because it does not know the metric.

There is a second, sharper failure. `vcreate` on a Map it did not create does
this:

```rust
// handler.rs:106
if store.create_map(name, spec.maxcount, spec.exptime).is_err() {
    store.drop_map(name)?;
    store.create_map(name, spec.maxcount, spec.exptime)?;
}
```

On a promoted node, the first `vcreate` **deletes every replicated vector.** The
comment calls the Map an "orphan from before a restart", which was true when
nothing else could produce one. Replication makes it false.

## 2. Goals

1. An index is fully reconstructible from its Map item alone.
2. A Map that ArcVector did not create is never mistaken for one it did, and is
   never destroyed.
3. Reconstruction does not stall a worker thread.
4. The role query needs no build flag of its own (§3.6).
5. Only the node that owns the data reconstructs.

**Non-goals.** Keeping a slave's graph warm before switchover (§8). Persisting
the graph itself — `save`/`load` stay unused. Cross-version element migration
beyond the existing version byte.

## 3. Design

### 3.1 The metadata element

The four missing values move into the Map item, as a **reserved element** beside
the vectors.

**Field name:** `AV META` — the space included.

Reserved by making the name unrepresentable rather than by forbidding it, so no
validation rule is added anywhere:

| Path | Result | Why |
|---|---|---|
| `mop insert` | cannot create | the field is a command-line token, split on spaces |
| `mop get` / `mop delete` | cannot name | a space-separated list checked against `numfields`, so it always splits into two — `EBADVALUE` |
| binary protocol | no path | LOP/SOP/BOP opcodes exist; MOP does not |
| `vadd` | cannot collide | ids are command-line tokens too |

`mop get <key> 0 0` still dumps it, so the element stays observable to an
operator. The engine stores a field as a length plus raw bytes — command log,
checkpoint snapshot and replication change-set alike — so there is no text round
trip for the space to break.

Two alternatives were rejected. **A `\x01`-prefixed name plus a rule that `vadd`
reject control bytes** was the first implementation and is not sufficient: the
engine does not restrict control bytes in a field, so `mop insert` can create the
same name and corrupt the metadata, which under §3.5 costs the whole Map.
**A field name equal to the index name** forbids one arbitrary id the user cannot
predict, and still needs an explicit block.

**Value:** the existing 16-byte element header, followed by the ATTR region
holding JSON. No vector. Total 144 bytes.

```
 off  0    2   3     4       6        8    9        16              144
     +----+---+-----+-------+--------+----+--------+---------------+
     |"AV"|ver|quant|dim u16|jlen u16|rt=1|rsvd 7B | metadata JSON |
     +----+---+-----+-------+--------+----+--------+---------------+
```

Byte 8 — the first of the header's eight reserved bytes — becomes a **record
type**: `0` = vector, `1` = index metadata. One framing, one magic, one version
byte, two record kinds. `quant` and `dim` keep their normal meaning, so the
metadata element is self-describing and agrees with its neighbours by
construction.

The JSON carries what the header cannot:

```json
{"metric":"cos","m":16,"efc":128,"efs":64,"owner":"7f3a1c9e0b2d5468"}
```

68 bytes against a 128-byte region. JSON rather than a packed struct because it
is readable from a plain `mop get` during an incident, and because a later field
costs nothing.

`owner` is **minted fresh every time an index is built** — it belongs to the
graph, not to the node. §3.7 is what it is for.

`maxcount` and `exptime` are deliberately absent: they are Map item attributes,
the engine already replicates and persists them, and duplicating them would
create two sources of truth that `setattr` could drive apart.

**Why inside the Map item.** It is created, replicated, evicted, expired and
dropped atomically with the data it describes. A sibling key would need its own
lifecycle and could outlive or predecease the vectors.

### 3.2 The flags marker

`vcreate` sets the Map item's `flags` to:

```
0x4156_0000 | FORMAT_VERSION     // "AV" << 16, version 1 in the low half
```

Read with `get_item_info`, which is one hash lookup and no element read. It
answers "is this plausibly one of ours?" before anything expensive.

**flags is a hint, never an authority.** A client can set any flags it likes via
`mop create <key> <flags> <exptime> <maxcount>`. The authority is the metadata
element: magic `"AV"`, a supported version, record type 1, and JSON that parses.
A Map passing the flags check but failing the element check is **not** ours and
is left untouched.

A key-derived hash was considered and rejected. Anyone able to write the item can
compute the hash too, so it adds no safety, and it makes the value opaque in a
dump. A fixed magic plus a version number is legible and leaves room to evolve.

Falling out of this for free: a Map auto-created by `mop insert` has flags 0 and
is correctly classified as not ours.

### 3.3 Index states

There is no separate state machine. The metadata element's `owner` is the whole
state, and the in-memory index mirrors it:

| Map `owner` | index `owner` | Meaning | Commands |
|---|---|---|---|
| a token | the same token | this node built the graph behind this Map | normal |
| a token | anything else | someone else has been serving; the graph here predates their writes | take over (§3.5) |
| `0` | `0` | a rebuild is in flight **here** | reads `UNREADABLE`, writes proceed |
| `0` | a token | the node that was rebuilding is gone | take over |

`0` is reserved and never minted, so "rebuilding" needs no extra field to be
distinguishable from a real owner. Both halves are read on every command, which
is what makes the third row possible: a demoted and re-promoted node keeps its
registry across both transitions — the process never died — so nothing but the
comparison would notice that the graph is old.

**Reads answer `UNREADABLE` while rebuilding; writes do not wait.** A rebuild of a
large index is not instant, and refusing writes for its duration would make a
switchover look like an outage to every writer. So a write during a rebuild goes
to Map and to the partial graph as usual. What that costs is described in §3.5:
the refill has to tolerate a concurrent producer.

`UNREADABLE` is the engine's own `ENGINE_UNREADABLE` (`0x3c`), the reply arcus
already uses for a collection that exists but cannot be read yet, so no new client
vocabulary is introduced.

`vdrop` is allowed at any time. It removes the Map, and the rebuild's closing
re-read (§3.5) discards the doomed build.

### 3.4 Probing

Today `registry::require(name)` returns `NoSuchIndex` on a miss. It gains a step:

```
registry miss
  └─ getattr(name)
       ├─ no such item        -> NOT_FOUND                (unchanged)
       ├─ flags != AV magic   -> NOT_FOUND                (someone else's Map)
       └─ flags match         -> build an empty graph from the metadata,
                                 take over (§3.5)
```

A graph constructed right there is empty whatever the Map says, so it is always
taken over — including when the Map already reads `0`. Skipping that case would
strand the state a failed rebuild leaves behind (registry entry dropped, Map still
`0`): it would look like a rebuild already in flight, and no command would ever
start another one.

`vcreate` on an existing Map takes the same route and answers `EXISTS` — never
`drop_map`. The destructive retry in `handler.rs:106-111` is deleted.

### 3.5 The builder

One thread, asleep until there is work, woken by a condvar. There is no polling:
the trigger is always a request that referenced the index. A name already queued
is not queued again, and an index this node already owns is never queued.

**Taking over**, on the worker, before anything is enqueued:

1. Stamp the Map's metadata with `owner = 0`. Every node, this one included, now
   reads "rebuilding".
2. Empty the graph and the id map, and mark the index rebuilding in memory.
3. Enqueue the name.

Emptying first rather than building alongside is a memory decision: usearch floors
at roughly 16.8 MB per non-empty index and the vectors sit on top, so a million
1024-dimension `i8` vectors would mean holding two copies of about a gigabyte.
Answering `UNREADABLE` is more honest than answering from a graph already known
to be stale.

**The rebuild thread never writes.** A thread with no connection has no cookie,
and the engine's write path dereferences one — `do_check_master_switchover_done`
calls `set_switchover_node(cookie, …)` and `get_thread_index(cookie)` with no null
guard, so a write from a detached thread crashes a master that is mid-switchover:
precisely when a rebuild is most likely to be running. This was not theoretical.
An earlier revision had the thread stamp its own finishing token and it took the
daemon down with a SIGSEGV in `get_thread_index`.

Reads from a detached thread are clear: `ACTION_BEFORE_READ` is empty without
`ENABLE_MIGRATION`, `rp_after_check` returns immediately when the thread-local
cookie is null, `WTHREAD_SET_LAST_CSET_SEQ` guards on it, and arcus itself calls
the slave switchover check with none — its log says "zk thread".

So the split follows the hazard:

```
worker : stamp owner = 0 → empty the graph → enqueue
thread : read the metadata, get_all, refill the graph   (reads only)
         → mark refilled
worker : next command — re-read the Map, mint a token, stamp it, serve again
```

With `daemon-migration` even a read reaches `set_not_my_key_info(cookie, …)`, so
that build stays inline and the caller retries once.

Per queued item, on the thread:

1. Read the role (§3.6). Not master and not standalone → drop the item.
2. Read `AV META` and decide what it is:

   | What was found | What happens |
   |---|---|
   | valid metadata | continue |
   | magic `"AV"`, **version we do not know** | leave everything alone, log. This is not damage — it is a newer node's format, and a rolling upgrade produces it by design. Deleting here would let an old node destroy what a new one wrote. |
   | absent, wrong magic, wrong record type, unparsable JSON | **delete the Map and the registry entry**, log loudly |

   The delete is deliberate, and it is the *only* failure that deletes anything.
   The Map is ours — its flags say so — and without the metric and HNSW parameters
   it cannot become an index again, so keeping it means every later command retries
   a recovery that cannot succeed. Removing it is also what makes a failure backoff
   unnecessary: there is nothing left to retry.

3. Reserve capacity for the whole element count up front, so the refill does not
   grow the graph one element at a time under a lock.
4. `get_all`, skipping the metadata field, decoding and adding each vector with
   **`add_unless_known`**.
5. Mark the index refilled. The thread stops here; it has written nothing.

Any failure that is not step 2's damage — an allocation failure, an element that
will not decode — drops the registry entry and nothing else, and the command that
hits the missing entry starts a fresh rebuild (§3.4). The client sees
`SERVER_ERROR`.

**`add_unless_known` is what makes concurrent writes safe.** `get_all` hands the
thread the elements as of one moment. A `vadd` that lands afterwards has already
put the newer vector in the graph, so the snapshot's copy must not overwrite it:
an id the id map already knows is skipped. The reverse case — a `vdel` for an id
still in the snapshot — is handled by the id map keeping a **tombstone** for
anything deleted while a rebuild is in flight, so the refill cannot resurrect it.
Tombstones are cleared when the rebuild ends.

This replaces an earlier design in which the rebuild refused writes outright. That
version needed no reasoning about snapshots, but it made every switchover a write
outage.

**Claiming**, on the next worker command to touch the index:

1. Re-read the Map's metadata. Still `0` → this build is still the one that counts.
   Anything else → another node claimed it meanwhile; fail, and the ordinary
   comparison in §3.3 starts a fresh take-over.
2. Mint a fresh `owner` (§3.7), write it into the metadata element, clear the
   rebuilding flag, and publish the token in memory.

Until that stamp lands the Map still reads `0`, so no other node can mistake the
state. The cost is that an index finishing its rebuild with no traffic stays
`UNREADABLE` until the next command arrives — and that command is the one that
makes it readable, so a client that retries sees it come back.

The claim logs one line, which is where the token becomes traceable:

```
ArcVector: index 'docs' is serving again, owner=7f3a1c9e0b2d5468
```

**One build at a time**, enforced by the single builder thread and by the queue
refusing duplicates.

### 3.6 Role detection, and why it was dropped

This section originally specified a role check. `stats replication` is served by
the **engine**, not the frontend (EE `default_engine.c:1299` → `rp_stats`), and
`get_stats` is a member of `engine_interface_v1`, so the module could ask which
side of a pair it was on with no new server API and no build flag:

```
get_stats(h, &mut role as *mut _, "replication", 11, collect_mode)

ENGINE_KEY_ENOENT               ->  serve   (built without ENABLE_REPLICATION)
"disabled" | "alone" | "master" ->  serve
"slave", or anything unknown    ->  refuse
```

It is **not implemented**, because `owner` (§3.7) turned out to answer the same
question:

- **token mismatch** — the node tries to take over, and the take-over stamps the
  Map before discarding anything (§3.5). That stamp is a write, which the engine
  refuses on a replica with `ENGINE_REPL_SLAVE`. The graph survives and the client
  is told where it landed, by the engine's error rather than by our check.
- **token match** — the replica's graph does describe its Map. A master stamps its
  new token *before* writing any element, and both go through the same Map item,
  so replication delivers them in that order: a replica cannot hold a matching
  token and stale data at once. Its answers are correct.

The check therefore bought nothing `owner` did not already provide, at the cost of
an engine call per command. The deliberate consequence: a replica **does** serve
reads while its token matches, and stops the moment the master takes over. That is
a policy change from §3.8, which is retained below only as the record of what was
considered.

### 3.7 Noticing that a serving index has gone stale

Everything above builds an index that is missing. This is about one that is
**present and wrong**.

Switch over from A to B and back. A's process never died, so A still holds the
graph it built as master — but while B was master, B's writes reached A's Map
through replication, *below* the extension, where nothing told A. A resumes
serving from a graph that no longer describes its own Map.

Measured on the pair, with B's writes standing in for what B would serve
(`docs/replication-testing.md` for the setup):

```
Map 실제         VALUE 0 4        ← four elements
vlist            count=3          ← the graph says three
vget stale2 v4   VALUE v4         ← key lookup finds it: it reads Map
vsim  (v4 방향)  v3, v2, v1       ← vector search does not: v4 is at distance 0
```

The two paths disagree because they read different things:

```
vget  ──▶ store.get_elem(name, id)     ──▶ arcus Map        the truth
vsim  ──▶ ann.filtered_search(…)       ──▶ usearch graph    a cache of it
```

`vget` never consults the graph — it reads the Map element by field, so it finds
whatever replication delivered. `vsim` walks the graph, and v4 was never added to
*this node's* graph, so no amount of proximity brings it back. Nothing errors,
because a graph that does not know a vector simply does not return it.

**Polling the role cannot catch this.** A→B→A leaves the role reading `master`
both before and after, so any sampling of it — however frequent — sees no change.
Something that differs *per graph* is required, and it has to live where both
nodes can see it, which is the Map.

**The mechanism.** The metadata element carries `owner`, a token minted fresh on
every build. A `VectorIndex` remembers the token it was built under. The Map's
`owner` is read and compared before every command:

| | |
|---|---|
| equal | the graph is current — proceed |
| different | someone else has been serving; take over (§3.5) |
| `0`, and this node is the one rebuilding | the two are equal, so this is the row above, not this one |
| metadata gone or damaged | §3.5 step 2 |

The token is written *last*, by the worker that finds the refill complete. A
rebuild that never gets there leaves `0` in the Map, which every node reads as
"nobody owns this" — so the failure mode is another rebuild, never a stale graph
believed current.

**A token, not a counter.** A counter would be read-modify-write, and elements
have no CAS, so two nodes building during a switchover could both land on `6`.
Tokens only have to *differ*, which removes the race entirely.

**Minting one, with no new dependency.** The crate has no RNG in its tree and does
not need one: `std::collections::hash_map::RandomState` is seeded from the OS, and
each `new()` advances its keys, so hashing a fixed value through a fresh one yields
a distinct `u64`.

```rust
fn mint_owner() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(OWNER_DOMAIN);   // any fixed value; the key is what varies
    h.finish()
}
```

Recorded as 16 hex characters in the JSON. Measured: one million mints in a single
process produced one million distinct values, and separate processes start from
different OS seeds.

Sixty-four bits is ample here because the comparison is against **one** stored
token, not a population — the chance a fresh token happens to equal the specific
one already in the Map is 2⁻⁶⁴. Birthday bounds would apply only if tokens were
being checked against each other in bulk, which never happens.

**Why not a name instead of entropy.** A readable identity would be nicer in a
dump, and every candidate was tried. Each fails the one property that matters for
software running on many machines at once: producing a value no other node can
produce.

| Candidate | Obtainable? | Distinct across machines? |
|---|---|---|
| `ip:port` | **yes** — `get_socket_fd(cookie)` is member 8 of `SERVER_CORE_API`, inside the prefix both trees share, and `getsockname` on that fd works with std alone | **no** — see below |
| `pid` | `std::process::id()` | no — containers commonly run the daemon as pid 1, so every node reports `1` |
| hostname | needs `libc` promoted to a direct dependency | by convention only, and a long FQDN does not fit the 128-byte ATTR region |
| `self_sid` | `stats replication` | it is regenerated on every handshake (`replication.c:2638`), so a node's own value changes without it rebuilding — a permanent source of spurious rebuilds |
| OS entropy | `RandomState`, std alone | **yes** |

`ip:port` deserves the detail, because it is the obvious answer and it *almost*
works. The trap is that `get_socket_fd` hands over the **connection** socket, not
the listening one, so binding `0.0.0.0` does not yield `0.0.0.0` — the kernel
reports the local address actually used for that connection. Measured:

```
listening socket getsockname() = ('0.0.0.0', 56120)
client via 127.0.0.1           → ('127.0.0.1', 56120)
client via 192.168.1.193       → ('192.168.1.193', 56120)
```

Two consequences, and the second is fatal. One node yields different values
depending on who connected — it identifies the connection, not the node. And
**the loopback address is the same on every machine**: a build first triggered by
a local health check or admin tool mints `127.0.0.1:11211` on host A and exactly
the same on host B. Two nodes, one token, and §3.7 stops catching anything.

Entropy has no such path because it is not derived from anything a second machine
could share. It also needs no coordination — no registry, no ZooKeeper round trip
— which is what makes it usable at the moment a node is deciding, alone, whether
its own graph is current.

The address is still worth having; it just belongs in the build log (§3.5), where
varying per connection is harmless and "which machine built this" is exactly the
question being asked.

Time-plus-pid was considered too and is worse than either: clocks step backwards
and pids recycle.

**It does not fire when nothing happened.** If B is promoted but never receives a
command, B never builds, so the token is untouched — and B serving nothing means
the Map is untouched too. A resumes on its existing graph. The invariant is:

> the token changes only when someone builds, and someone builds only when they
> are about to serve. If nobody built, nobody wrote.

**What it does not cover.** A Map changed by something other than ArcVector — a
client issuing `mop insert` against the index key — moves the data without
touching the token. Such an element has no `AV` magic and fails `decode` at the
next rebuild, but until then it is invisible. That hole predates this section and
is not closed by it.

**Cost.** One `map_elem_get` of a single known field per command. Measured
round-trips on the pair: `vget` 26 µs (which already performs one such read),
`vsim` k=10 65 µs, `vadd` 73 µs. One more read of the same kind is a small
fraction of a round trip even on the cheapest command, and the alternative —
checking periodically instead — buys that fraction back by answering wrongly for
up to the sampling interval.

### 3.8 A replica does not serve vector commands — *superseded*

> **Not implemented.** This section argued for refusing every vector command on a
> replica, and it is kept as the record of what was considered. §3.6 explains why
> it was dropped: a replica whose token no longer matches is stopped by the engine
> refusing the take-over's write, and one whose token still matches is answering
> from a graph that provably describes its Map. The observation below — that a
> demoted node keeps answering — is real; what changed is the conclusion that it
> must be stopped unconditionally.

§3.5 says a replica never builds. It does not say what happens to a graph a node
already had when it was demoted, and the answer today is: it keeps answering from
it. Observed on the pair, immediately after a switchover, on the node that had
just been master:

```
role   : slave
vlist  : INDEX probe … count=3     ← still registered
vsim   : QUERY 0 2 | v3 | v2       ← still answering
vget v1: VALUE v1                  ← accurate, because it reads Map
```

A replica's Map keeps moving under replication while its graph is frozen at the
moment of demotion, so `vget` and `vsim` on the same node drift apart with no
error on either — the same contradiction as §3.7, reached without ever being
promoted again.

**Writes are already refused; reads are not.** The engine answers a write on a
replica with `ENGINE_REPL_SLAVE`, and because `vadd` writes to Map *before*
touching the graph, a refused write leaves the graph alone. Reads get no such
signal — arcus permits them from a replica on purpose:

```
slave, mop insert : REPL_SLAVE      ← the engine says so
slave, mop get    : VALUE 0 1       ← allowed, deliberately
```

**The rule: a replica serves no vector command at all.** Not the reads either.
Checked once, before dispatch, rather than woven into each path:

| Command | On a replica |
|---|---|
| `vcreate` `vadd` `vdel` `vdrop` | refused |
| `vsim` `vget` | refused |
| `vlist` | empty — a replica holds no index to list |
| `vstats` | served; it reports this module's own memory, not index contents |

Refusing is stated plainly rather than disguised as `NOT_FOUND`, so a client that
reached the wrong node learns which node to use:

```
SERVER_ERROR this node is a replica; vector commands are served by the master
```

A replica that finds an index still registered — the demoted case above — drops it
at the same moment. There is nothing to keep it current with.

**Why not just let the engine say it.** That was the first instinct and it covers
exactly half: writes. Leaning on it for reads would mean serving a frozen graph
until someone happened to write, which is the failure this section exists to
close.

**Per command, not sampled.** The cost was measured rather than assumed.
Pipelined, 3000 commands each, server-side:

| | per command |
|---|---|
| `version` (floor) | 2.88 µs |
| `stats replication` | 2.84 µs |
| `vlist` | 3.63 µs |

`rp_stats` formats about fifteen short values and disappears into the per-command
floor — the difference measures as −0.04 µs, below the noise. Reached from the
module it is cheaper still, since nothing formats or transmits the stats text. A
sampled check would have traded correctness for something unmeasurable.

Together with §3.7 the two checks divide cleanly:

| check | question | source |
|---|---|---|
| role (§3.8) | may I serve at all? | `get_stats("replication")` |
| `owner` (§3.7) | is my graph current? | the metadata element |

### 3.9 Capacity accounting

The metadata element occupies one Map slot, and it must not come out of the
user's budget. `vcreate` therefore creates the Map one slot larger than the
`MAXCOUNT` asked for — clamped to the engine's own ceiling, where there is nothing
left to widen into and the vector count gives up the slot instead — and every
count exposed or enforced is over the Map's `maxcount - 1`:

- `vadd`'s overflow check
- `vcount`, and `count` / `maxcount` in `vstats` and `vlist`
- the refill loop, which skips the metadata field rather than fail `decode` on it

## 4. Error contract

| Situation | Reply |
|---|---|
| read of an index being rebuilt | `UNREADABLE` |
| write to an index being rebuilt | served normally |
| Map exists, flags not ours | `NOT_FOUND` |
| Map ours, metadata missing or damaged | `NOT_FOUND`; the Map is deleted (§3.5) and the loss is logged |
| Map ours, metadata a version we do not know | `NOT_FOUND`; nothing is touched — a newer node wrote it |
| `vcreate` over an existing ArcVector Map | `EXISTS`, rebuild enqueued |
| role is slave | `SERVER_ERROR this node is a replica…` |
| serving index whose `owner` no longer matches (§3.7) | rebuild enqueued; reads `UNREADABLE` until it completes |
| rebuild failed for any reason other than damage | `SERVER_ERROR`; nothing is deleted and the next command retries |
| any vector command on a replica (§3.8) | `SERVER_ERROR this node is a replica; vector commands are served by the master`, and any index it still holds is dropped |

`SERVER_ERROR` rather than `CLIENT_ERROR`: the request was well-formed and the
server is not ready, matching `Error::blame()`'s existing split.

## 5. Changes by file

| File | Change |
|---|---|
| `arcus/element.rs` | record type at byte 8; metadata encode/decode; `MetaRecord` with `owner` |
| `arcus/engine.rs` | `create_map` sets flags; `get_item_info` wrapper; `get_stats` wrapper; read one element by field for the `owner` check |
| `registry.rs` | `owner: AtomicU64` replacing `built`; builder thread and queue; `take_over` and `claim_refilled` |
| `usearch/index.rs` | `add_unless_known`; id-map tombstones; `reserve`/`clear`/`begin_rebuild`/`end_rebuild`; one lock order, `inner` before `ids` |
| `command/handler.rs` | delete the `drop_map` retry; probe on miss; role and `owner` checks before every command; id byte validation; the reserved metadata slot |
| `error.rs` | `Error::Unreadable` and `Error::ReplicaNode`, both blamed `SERVER_ERROR` |

## 6. Testing

Unit, no daemon:

- metadata element round-trip; record type dispatch; every rejection —
  bad magic, wrong version, wrong record type, malformed JSON, unknown metric
- vector id byte validation, including the reserved field name itself
- flags magic encode/decode, and rejection of flags 0 and of a foreign value
- state transitions including the stale-generation discard

Integration, real daemon:

- **The regression that motivates this:** create an index, add vectors, drop the
  registry entry without touching the Map, `vcreate` again — every vector must
  survive and `EXISTS` must come back. This fails today.
- restart-equivalent recovery: same, then `vsim` — `UNREADABLE`, then the correct
  results once the rebuild is claimed
- a `mop`-created Map with the same key is never touched by `vcreate` or `vdrop`
- `vadd` during a rebuild is accepted, and the refill does not overwrite it
- `vdel` during a rebuild is not undone by the refill
- `vdrop` during a rebuild leaves nothing registered
- `get_stats("replication")` on the OSS test daemon returns `KEY_ENOENT` and the
  node is treated as standalone

Replication, in `tests/replication.rs` behind the `replication-tests` feature:

- the role query, both roles, and the switchover that swaps them
- flags and vector elements surviving replication
- a slave holding the data and refusing vector queries
- a promoted node holding the data with no index
- `a_demoted_node_stops_answering_vector_queries` — switch over, then query the
  node that was master. `#[ignore]`d until §3.8 lands.
- `a_double_switchover_does_not_leave_a_stale_index` — A→B→A, with a write in the
  middle, then the same query answered by key and by vector. Both must agree.
  `#[ignore]`d until this lands; it currently shows them disagreeing.
- `vcreate_must_not_destroy_a_replicated_map`, `#[ignore]`d until this lands

That target needs an EE daemon, a ZooKeeper and a provisioned pair, none of which
the `docker/` harness supplies, so it stays **out of `make test` entirely** —
a green required suite must not imply switchover was covered. Setup is in
[replication-testing.md](../../replication-testing.md).

## 7. Migration

Existing Maps have flags 0 and no metadata element, so they probe as not-ours and
are ignored — not destroyed. `vdrop` then `vcreate` re-creates them in the new
format. There is no in-place upgrade, and none is warranted before a release.

## 8. What a live pair showed

Everything below was measured against an EE pair
(`arcus-memcached 0.9.5-E-139`, ports 11311/11312, ZooKeeper on 2181) with the
extension loaded. Setup is in [replication-testing.md](../../replication-testing.md);
the assertions are `tests/replication.rs`.

**Confirmed as designed.**

| Premise | Result |
|---|---|
| §3.6 role query | `stats replication` answers `STAT mode master` / `STAT mode slave` as its **first** line, through the engine vtable, with no new server API |
| the role tracks a switchover | `replication switchover` → `OK`, and the two nodes report the swapped roles about 8 s later |
| §3.2 flags survive replication | `mop create` with flags `0x41560001` read back byte-identical on the slave |
| vectors survive replication | three `vadd`s on the master, all three elements present on the slave with the `AV` magic intact |
| §1's gap is real | after a switchover the promoted node holds all three elements and `vlist` is empty — data present, index unknown |
| §1's destructive `vcreate` | reproduced: `vcreate` on the promoted node answers `CREATED` and the replicated elements are gone. Pinned by `vcreate_must_not_destroy_a_replicated_map`, `#[ignore]`d until this design lands |

**Two blockers this design did not account for.** Both are upstream of index
recovery: they stop ArcVector working on an EE daemon at all.

### 8.1 The vtable layout is not fixed across arcus builds

`engine_interface_v1` is called by offset, and members are added and removed
inside it:

- EE drops `btree_elem_smget_old`, which OSS has
- `SCAN_COMMAND` adds `prefixscan`, `keyscan`, `get_prefix_info`
- `ENABLE_REPLICATION` adds seven `rp_*`
- `ENABLE_MIGRATION` adds six more, including `scrubber_in_running`

Everything after the insertion point shifts, `get_item_info` and `get_elem_info`
included. Nothing at runtime tells the variants apart — `interface` is 1 in all of
them.

Observed, in order:

| Build | Symptom |
|---|---|
| vendored OSS headers, EE daemon | `SIGSEGV` in `item_cachedump`, called from `Store::config_u32`: OSS slot 60 is `get_config`, EE slot 60 is `cachedump` |
| EE headers, no feature defines | `SERVER_ERROR engine unavailable` on `vadd`: `get_elem_info` read at the wrong slot and came back `NULL` |
| EE headers, `SCAN_COMMAND ENABLE_REPLICATION` | everything works |

`build.rs` now takes `ARCVECTOR_ENGINE_INCLUDE` and a `daemon-*` cargo feature
per `configure` flag, so a build can be aimed at a specific daemon, and `arcus::abi` refuses a pairing it
can prove wrong: the tree the headers came from is a build-time fact (the OSS
header never mentions `rp_cmd`, even inside an `#ifdef`), and the daemon's tree is
readable at runtime through `server_version()`, which lives in `SERVER_CORE_API`
rather than the vtable in question. Cross-tree is refused before the first call;
a feature mismatch within a tree surfaces from `put_elem` as
`SERVER_ERROR engine ABI mismatch`.

That makes the failure loud rather than fatal. It is still a workaround — the
artifact remains per-daemon-configuration.

The fix belongs in EE — extend the struct by **appending after `errinfo`** rather
than inserting, and keep the common prefix identical to OSS. Then one library
really does serve both, which is what §3.6 assumed. That is EE work with its own
design; until it happens, ArcVector ships one artifact per daemon configuration
and must say which.

### 8.2 `EWOULDBLOCK` from a sync master is not a failure — and reading it as one
crashed the daemon

On a master in sync mode, `rp_after_check` → `rp_wait` (`replication.c`) registers
the caller as a waiter for the slave's acknowledgement and returns
`ENGINE_EWOULDBLOCK`. It runs **after** the operation, with the original result in
hand: the write already happened. The code means *do not answer yet*, not *it did
not happen*.

This module read it as failure, and both write paths then undid work the engine
had committed:

```
vadd on a sync master
  map_elem_insert -> ENGINE_EWOULDBLOCK   (element linked)
  treated as error -> map_elem_free(elem)
  Assertion failed: (elem->linked == 0), function map_elem_free,
                    file coll_map.c, line 444
```

The daemon aborts. Observed on the pair: the master died mid-command, its Map was
gone, and the slave still held the data. `vcreate` had the milder version of the
same fault — it `drop_map`ped the Map it had just created and reported an error
for a Map that had briefly existed.

Fixed by treating `ENGINE_EWOULDBLOCK` as completion (`arcus::engine::completed`).
The replication tests now run in the daemon's **default sync mode** rather than
switching it off, which is what keeps this from coming back.

**The waiting is not done, and is not going to be.** A built-in command sets
`c->ewouldblock`, and `conn_parse_cmd` then consults `should_io_blocked`, which
parks the connection while `current_io_wait > 0` — a counter `rp_wait` has already
incremented through the public `waitfor_io_complete`. Every link in that chain is
reachable from an extension except `c->ewouldblock`, a `conn` field with no server
API behind it. Two things worth knowing before anyone tries:

- **Do not call `waitfor_io_complete` from the module.** The engine already does,
  and a second increment leaves `current_io_wait` permanently above zero, so the
  next built-in command on that connection waits for a notification that never
  comes. Its two callers are `replication.c:5642` and `cmdlogmgr.c:378`.
- **Resuming does not re-run the command.** `conn_mwrite` transmits the response
  that was composed before the connection parked. An extension's reply reaches the
  same buffer through `ascii_response_handler` and `write_and_free`, so nothing
  about the reply would need to change — only the parking.

### The consequence, and why it is left as is

A vector write on a sync master replies before the replica has acknowledged it.
Measured on the pair, over 500 `vadd`s:

```
sync_exec_count  +501
sync_wait_count  +501     100%
```

**Every** write. That is expected rather than pathological: `rp_wait` defers
whenever the change set it just produced is unacknowledged, which is always true
immediately after a write. Built-in commands hit the same 100% — they simply wait.

So this is not an occasional caveat, it is the standing behaviour of vector writes
under replication: **committed locally and replicating, answered without waiting
for the acknowledgement.** A master lost inside that window drops a write the
client was told had succeeded — the same exposure as `no_sync_mode 1`, for vector
commands only.

Nothing else degrades. There is no crash, no counter leak (`notify_io_complete`
decrements the waiter it was given and logs no premature notification — verified),
and no divergence between graph and Map, because the graph is rebuilt from whatever
the Map holds. A write that did not reach the replica is simply absent after a
failover.

It is therefore **documented rather than fixed**. Signalling it per reply was
considered and rejected: at 100% the marker never varies, so it carries no
information while forcing every client parser to change. A constant property
belongs in the reference, not in the response.

## 9. Deferred: a warm slave

Building the graph on the slave would make switchover instant. It is not done
here because **replication replays at the engine layer and the extension gets no
hook** — `rp_map_elem_inserted` and friends are internal to the engine. A graph
built on a slave goes stale the moment the next element replays, and nothing
would tell it. A silently wrong answer is worse than a slow correct one.

The cost accepted instead: every switchover pays one cold rebuild, its duration
proportional to the index size, during which the index answers `NOT_READY`.

Removing that cost needs an engine callback on replayed collection operations —
a separate piece of work, in EE, with its own design.
