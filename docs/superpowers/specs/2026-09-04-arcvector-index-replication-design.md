# Index replication: keeping a replica's graph warm

Design record. Status: **proposed**.

Extends
[2026-08-12-arcvector-replication-recovery-design.md](2026-08-12-arcvector-replication-recovery-design.md),
which made a promoted node *able* to rebuild. This makes it *not need to*.

---

## 1. The problem

The owner-token design solved correctness. A node whose graph no longer explains
its Map notices, drains, and rebuilds from Map. Nothing is served from a graph
that has gone stale.

What it did not solve is **cost and availability**:

| | Today |
|---|---|
| Switchover | Promoted node rebuilds from scratch. 990k × 1024 f32 takes minutes. Reads answer `Rebuilding` until it finishes. |
| Replica reads | A replica's graph is only built when a read arrives and finds it cold. Its freshness is bound to the master's write traffic (§10.2 of the recovery design). |
| Total cluster CPU | Every node builds the same HNSW independently. |

The gap is that arcus replicates the Map but has no idea the extension holds a
derived structure. Nothing tells the replica that a vector was indexed.

**This design adds that channel.** A master streams "this id changed in this
index" to its replicas; each replica applies it to its own graph, resolving the
vector from its own replicated Map.

## 2. Goals

1. **Switchover is immediate.** A promoted node serves from a graph that was
   already current.
2. **Replicas are readable.** A replica's graph tracks the master continuously
   rather than being built on demand.
3. **The rebuild happens once.** A replica builds its HNSW at first sync, not at
   every role change.

## 3. Non-goals

- **Replicating vector data.** arcus already replicates the Map. The channel
  carries identifiers, never vectors.
- **Transferring a built graph.** Deferred; see §11.
- **Guaranteed delivery.** Every loss ends in an explicit `RESYNC` or a closed
  connection, and a reconnect begins with `SNAPSHOT` (§8). Nothing is lost
  quietly, but nothing is acknowledged either.

  The stream **does** replace the per-command owner-token check (§4.5), so it is
  load-bearing where that check used to be. What stands behind it is
  `access::sweep`'s reconciliation, which is slower and coarser. This is the
  design's main trade.
- **Ordering with arcus replication.** The two streams are independent TCP
  connections with no relative ordering. The apply path converges instead of
  assuming order (§7).

## 4. Role discovery

### 4.1 `AV OWNER`

A top-level arcus key, not a Map field. The name contains a space, so the ASCII
protocol cannot express it — the same protection `AV META` relies on. Its value
is where to reach the master:

```
"AV OWNER"  ->  "<ip>:<port>"
```

**A node keeps its own listener address to compare against.** Whether the probe
succeeded and what the key currently holds are different facts: right after a
switchover the demoted node's probe fails — telling it it is a replica — while
the value it reads is still the one it wrote itself, because the new master's
write has not replicated yet. An address equal to its own is stale by
definition, and it waits rather than dialling itself.

The address is remembered rather than re-read from the socket, since a demoted
node has already closed the listener by then. After a restart there is nothing to
remember, and nothing to fix: the new ephemeral port differs from the published
one, so the node dials the stale address, finds nobody, and retries.

**The name deliberately avoids the `arcus:` prefix.** That prefix marks an item
`ITEM_INTERNAL` (`item_base.c:1087`), which exempts it from `scrub stale`
(`items.c:1166`) — and from the replication gate, which is the whole probe. A key
that nothing refuses answers no question. Paying the exemption back is cheap: the
key is subject to `is_my_key`, so `scrub stale` may delete it, and the heartbeat
of §4.2 writes it again. That scrub runs only once per cluster membership change,
`zk_timeout` after a node joins (`arcus_zk.c:1522`), so the window is small and
the repair automatic.

### 4.2 Learning the role

**One heartbeat, on every node, whatever it believes its role to be.** Every 10
seconds it writes `AV OWNER`:

```
store("AV OWNER", "<my ip>:<my port>")
  ├ ENGINE_SUCCESS -> I am master, and the value is refreshed
  └ anything else  -> I am not master
```

The same write does three jobs. On a master it republishes the address, which is
how a value lost to `scrub stale` comes back (§4.1). On a replica it is the
promotion probe: the day it succeeds is the day this node became master. And its
refusal is the demotion probe.

**Anything other than success means "not master".** `ENGINE_REPL_SLAVE` is the
ordinary case; `ENGINE_SWITCHOVER_DONE` means the roles are mid-change;
`ENGINE_NOT_MY_KEY` means a multi-group cluster placed this key in another
group's hash slice (§13). All three lead to the same conduct — do not open a
listener, do not claim to be master — so none needs its own handling. The design
degrades rather than looping on an answer it cannot change.

**This works because `AV OWNER` is not `arcus:`-prefixed**: `rp_before_check`
returns `ENGINE_SUCCESS` for any key beginning `arcus:` before it looks at the
replication state (`replication.c:5585`), so a probe under such a name is
answered by no one. §4.6.

Two events beat the interval:

| Event | Meaning |
|---|---|
| A client write is refused `REPL_SLAVE` / `SWITCHOVER_DONE` | Demoted. Under load this arrives long before the heartbeat. |
| A replication connection drops | Either side may have changed role. Probe now. |

**Only the real master can write the key**, which is worth stating on its own: a
demoted node that has not yet noticed will have its heartbeat refused, so it
cannot overwrite the address its successor published. The refusal informs it and
protects the value in the same stroke.

**The role is cached** in an atomic, which is what `recovery::stamp` reads
instead of asking the engine (§4.6).

A node that changes role tears down what the old role built: a demoted master
closes its listener and drops its replica connections; a promoted replica closes
its connection to the old master and opens a listener.

**A promoted node checks itself before serving.** It may have been behind when
the role changed, and no acknowledgement told the old master so. It runs §7's
comparison over every index it holds — one `getattr` each — and rebuilds only
those that disagree with their Map. Promotion is therefore safe without an
acknowledgement, at the cost that a replica that was behind pays a rebuild for
the indexes it had missed.

### 4.3 How a replica finds the master

It reads its own local `AV OWNER`. The value arrived through arcus replication —
the same path that carries the Map. **There is no discovery protocol.** The
value is written when a node becomes master and not refreshed after, so a
replica treats it as a hint that may be stale and re-reads it whenever a
connection attempt fails.

A value equal to this node's own listener address is stale (§4.1) and is waited
on, not dialled. A value naming a master it cannot reach is retried, not
discarded: a crashed master's address stays published until whoever is promoted
overwrites it.

### 4.4 The advertised address

The port is ephemeral: bind `0.0.0.0:0` and read it back with `getsockname`.
Nothing has to be configured.

The IP must be routable from the replica. In order:

1. The arcus service socket's bind address, when it is specific (`-l 10.0.0.5`).
2. Otherwise the hostname's first resolved non-loopback address.
3. Otherwise the first non-loopback IPv4 on any interface.

### 4.5 What this removes: the per-command metadata read

`access::resolve` reads `AV META` on **every command** today. It is doing four
jobs, and the stream takes over the one that justified the cost:

| Job | After this design |
|---|---|
| `meta.owner != index.stamped_as()` — has another node's graph written this Map? | The stream says so directly. A node that was a replica through the other node's term has been applying its deltas all along. |
| Map expired or evicted | `access::sweep`'s `probe_map`, which already reports `KeyGone`. |
| Map replaced by a plain `mop` Map | The same probe's `is_map`. |
| `layout` — `dim` and `quant` | Needed only to build a graph for a name the registry does not hold. |

So metadata is read **on first touch only** — when `registry::get` misses. A
command against a registered index makes no extra engine call.

What this costs is honest: divergence detection moves from *every command* to
*the stream, and the sweeper's rotation behind it*. Detection is no longer
immediate, and the sweeper's count comparison is weaker than the token — equal
counts can hide a swap. The stream is what makes the trade worth taking, and §3
records that it is now load-bearing.

### 4.6 A defect this design depends on fixing

`Store::ping_slave` cannot work. It probes by deleting `arcus:repl-probe`, and
`rp_before_check` skips every key beginning `arcus:` before it reaches the
replication check. Master and replica both answer `ENGINE_KEY_ENOENT`, so the
function always reports "not a replica".

The consequence is in `recovery::stamp`, which calls it to skip the ownership
write on a replica. The skip never happens, `put_elem` runs, the replication gate
refuses it — under the index's own name, which *is* checked — and the error
reaches `resolve`. **A replica that holds a Map cannot build a graph for it; the
read fails.** That is goal 2 of this design, so the fix is a prerequisite, not a
follow-up.

The fix is the role cache of §4.2: `stamp` reads the cached role rather than
asking the engine, which costs nothing on the request path. `ping_slave` is then
unused and goes.

## 5. Transport

`std::net::TcpListener` / `TcpStream` with blocking threads. No async runtime:
this crate is a `cdylib` inside memcached, its direct dependencies are two, and
`-d` forks after the extensions load. A runtime would add threads, a large
dependency tree, and another thing that has to survive `fork`.

**Framing.** `[u32 len][u8 type][payload]`, little-endian, mirroring the shape of
arcus's own `msg_chan` header so the two read alike.

**Threads.** One acceptor, which reads the replica's `SYNC` and hands the socket
to a writer thread. From then the connection is write-only: replicas do not
acknowledge, so no reader thread is needed. A replica that has gone away surfaces
as a write error, and a replica that has hung surfaces as a write timeout
(`SO_SNDTIMEO`, 5s) — without one, a wedged peer would block its writer thread
forever while its channel silently filled.

**Silent connections are closed.** A connection that does not send `SYNC` within
500ms is dropped, matching `msg_chan`'s `handshake_timeout`. This is hygiene, not
a requirement of coexistence with `attach`: that scan rejects a listener whether
it closes silent connections or speaks first.

## 6. Protocol

Four messages.

```
replica -> master   SYNC     { node_id, proto_ver, accepts_graph: false }
master  -> replica  SNAPSHOT { indexes: [name, ...] }
                    DELTA    { index, id, op }
                    RESYNC   { index }
```

`op` is `Upsert` or `Delete`. `node_id` is `owner::ours()`, which already
identifies a process uniquely.

**`SNAPSHOT` carries index names only** — a few KB, never vectors. The replica
cannot derive the names itself: its `registry` is in-memory and empty at
startup, and finding which arcus keys are ArcVector indexes would mean scanning
the whole keyspace.

**There are no sequence numbers.** TCP orders a connection, and every way a delta
can be lost ends in either an explicit `RESYNC` or a closed socket — and a closed
socket forces a reconnect, which begins with `SNAPSHOT`. Nothing can go missing
quietly, so there is nothing for a counter to detect. §8 states the invariant
this rests on.

A draft of this design carried a `seq` per delta for gap detection. It was
solving a problem `RESYNC` already solved.

`accepts_graph` is always `false` today. It exists so §11 can be added without a
protocol break.

## 7. Applying deltas

On `SNAPSHOT`, for each named index the replica compares what it holds against
its own Map:

```
probe_map(name).count - 1  ==  index.ann.len() ?      (-1 is the AV META element)
  equal     -> nothing was missed; skip the rebuild
  different -> rebuild through recovery::fill
```

That is one `getattr` per index, the check `access::sweep` already trusts. A
brief disconnect with no writes in it costs nothing.

Deltas apply **convergently**, because the delta may arrive before arcus has
replicated the element it refers to:

- `Upsert` — read `(index, id)` from the replica's own Map. Present: apply to the
  graph. Absent: defer and retry on the next tick.
- `Delete` — remove from the graph. Safe whatever the Map says; if the Map still
  holds the element, a later rebuild is the authority.

The Map is the source of truth on the replica exactly as it is on the master. A
delta is a hint about *where to look*, never the data itself. This is what makes
the two replication streams' relative ordering irrelevant.

Rebuild and delta application may overlap. `add_unless_known` decides under the
mapping write lock, so applying a delta for an id the rebuild already placed is a
no-op. Nothing has to be buffered.

Two invariants make §8's flow control work, and both live here:

1. **The replica applies inline with reading.** It must not drain the socket into
   an unbounded buffer and apply later — that severs the backpressure the master
   depends on to notice a slow replica.
2. **The deferred set is bounded.** Deferred upserts are lag that has not
   reached the socket yet. Past a fixed size the replica resynchronises that
   index rather than letting the set hide how far behind it is.

## 8. Falling behind

The master holds a bounded `sync_channel(1024)` per replica. Publishing is
`try_send` from the request thread:

- `Ok` — the writer thread will send it.
- `Full` — that replica is behind. Its queue is drained and it is marked for
  resync; the writer sends `RESYNC` and the replica repeats §7.

One slow replica fills only its own channel. Publishing never blocks a request
and never fails.

When no replica is connected, or this node is not master, publishing returns
after one atomic load.

**The queue is the lag signal**, and TCP is what makes it one. A replica that
applies slowly stops reading; its receive window closes; the master's write
blocks and then times out; its channel fills; `Full` fires. Slowness anywhere in
the replica's pipeline arrives here, which is why §7's two invariants are
requirements and not advice.

**The invariant this design rests on:**

> Every lost delta is either an explicit `RESYNC` or a closed connection.
> No path drops one quietly.

A closed connection forces a reconnect, and a reconnect begins with `SNAPSHOT`.
That is what lets the protocol carry no sequence numbers.

**The master does not learn how far a replica has applied**, and does not need
to. What it exposes is its own state, all of it local: per replica the queued
depth, the number of `Full` events, and the number of `RESYNC` messages sent.
`SyncSender` has no length, so depth is a counter incremented on send and
decremented on receive.

## 9. Failure handling

| Situation | Response |
|---|---|
| Replica connection breaks | Replica reconnects, sends `SYNC`, §7 runs. Cheap when nothing changed. |
| Master unreachable | Replica retries, re-reading `AV OWNER` each time: a switchover changes the address. |
| Role changes | §4.2. Each side tears down what the old role built. |
| Demoted master with no write traffic | The heartbeat is refused within 10s. A replica disconnecting gets there sooner. |
| Promoted replica with no write traffic | The heartbeat succeeds within 10s, and it becomes master. |
| `AV OWNER` deleted by `scrub stale` | The next heartbeat writes it again (§4.1). |
| `AV OWNER` missing or unparsable | Replica waits and retries. No graph is dropped. |
| `AV OWNER` holds this node's own address | Stale: the new master's write has not replicated. Wait for it. |
| `AV OWNER` names an unreachable node | Retry. The address stays published until the next promotion overwrites it. |
| Deferred upsert never resolves | After 30s, or once the deferred set passes its size bound, the replica resynchronises that index. |
| Writer thread dies | Its `TcpStream` drops on unwind, closing the socket; the replica reconnects. |
| Protocol version mismatch | Master refuses the connection and logs. Better than streaming into a decoder that disagrees. |
| Replication feature off | None of this is compiled. |

Nothing here can lose data: every path's worst outcome is a rebuild from Map,
which is the behaviour that exists today.

## 10. Module structure

```
src/repl/
  mod.rs      role loop, AV OWNER, the publish hook      ~110
  wire.rs     framing and the four messages              ~70   (pure)
  master.rs   listener, per-replica writer, queues       ~110
  slave.rs    connect, converge, gap detection           ~110
```

Gated on `#[cfg(feature = "replication")]`. No new dependencies.

The publish hook is called from `vadd`, `vdel`, `vcreate` and `vdrop` — the
commands that change what the graph should hold. `vsetattr` is not among them:
attributes live in the Map and reach the replica through arcus.

Two existing files change with it:

- `access/mod.rs` — `resolve` stops reading metadata for a registered index
  (§4.5).
- `recovery.rs` — `stamp` reads the cached role instead of calling
  `ping_slave`, which is then deleted (§4.6).

Reused rather than rebuilt: `recovery::fill` for the rebuild, `Store::probe_map`
for the skip check and the promotion self-check, `owner::ours` for the identity
a replica gives in `SYNC`.

## 11. Deferred: transferring the graph

A master could ship `save_to_buffer` and let the replica `restore_from_buffer`,
skipping HNSW construction entirely — roughly 20–30s against 1–5 minutes.

It is deferred because the remap is the riskiest code in the feature. usearch
keys are element addresses, so a transferred graph refers to the master's heap.
`Index::rename` can repair this, but master and replica addresses share one `u64`
space, so a rename may collide with an address not yet remapped — requiring two
passes through a tagged temporary space (bit 62; bit 63 is `STAGED_TAG`) while
the replica holds all elements to keep addresses stable.

That buys a 3–10× speedup on a path that runs at first sync and after long
disconnects only — rare, once the stream keeps replicas warm. It also puts 4.5GB
on the same link arcus replicates over.

`SYNC.accepts_graph` reserves the negotiation, so this costs two fields today.

**Also deferred: a `PROGRESS` message.** A replica could report what it has
applied and how much it has deferred, giving the master a real lag figure rather
than the queue depth §8 settles for. It needs a reader thread on the master, and
it buys observability rather than correctness — promotion is already safe
through §4.2's self-check. Worth adding if operating the feature shows the queue
depth is not enough to explain a lagging replica.

## 12. Testing

| Level | What |
|---|---|
| Unit (`wire.rs`) | Frame round-trip, truncated frames, oversized length rejected, unknown type rejected. |
| Unit (queue) | `Full` marks for resync; one slow replica does not affect another. |
| Unit (backpressure) | A replica that stops reading drives its channel to `Full` and produces exactly one `RESYNC`. |
| Unit (stale owner) | An `AV OWNER` holding this node's own address is waited on, never dialled. |
| Unit (role cache) | Each of §4.2's four events moves the cached role; `stamp` reads it without touching the engine. |
| Unit (first touch) | A registered index resolves with no metadata read; an unregistered one reads it once. |
| Integration (`replication-tests`) | A live pair: a replica serves reads at all (the §4.6 defect, as a regression test); replica graph converges after writes; switchover serves immediately; a killed connection resynchronises; a replica started against a populated master catches up; a demoted node does not connect to itself; a demoted master with no write traffic gives up its listener when its replica disconnects. |

The integration tier needs a master/replica pair and ZooKeeper, which the
existing `replication-tests` feature and `docs/replication-testing.md` runbook
already provide.

## 13. Limits

- **Replica freshness is bounded by the stream, not guaranteed.** A replica may
  lag by the delta round-trip plus any deferred resolution. Reads against a
  replica are eventually consistent with the master.
- **The master cannot measure that lag.** It sees its own queue, not the
  replica's progress (§8). A replica that reads promptly and applies slowly looks
  healthy until its deferred set or its receive window gives it away.
- **A rebuild still blocks that index's reads on the replica**, exactly as today.
  The design removes rebuilds from the common path; it does not make them
  invisible.
- **One replication group.** `AV OWNER` is a fixed name, so it hashes to one
  slice. In a multi-group cluster with migration enabled, only the group owning
  that slice can write it; the others get `ENGINE_NOT_MY_KEY`, conclude they are
  not master, and simply do not replicate their indexes. Nothing breaks, but
  those groups get no warm replica. Giving each group a name in its own slice —
  the first `AV OWNER <n>` for which `is_my_key` holds, which master and replica
  derive alike because they own the same slices — would lift this, and is not
  built because migration is not enabled on the trees this targets.
- **Direct manipulation of an index key** (`mop insert` and friends) is still
  invisible to the graph — §10.1 of the recovery design, unchanged here.
