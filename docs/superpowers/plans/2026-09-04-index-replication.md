# Index Replication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A master streams "this id changed in this index" to its replicas so a promoted node serves from a graph that is already current, instead of rebuilding its HNSW from scratch.

**Architecture:** One heartbeat writes `AV OWNER`; whether it succeeds is the node's role, and its value is where replicas dial. Replicas connect over plain TCP and receive index names, then identifiers — never vectors, because arcus already replicates the Map. A replica resolves each identifier against its own Map, so the two replication streams need no relative ordering.

**Tech Stack:** Rust 2024, `std::net` and `std::thread` only. No async runtime, no new dependencies.

**Spec:** [docs/superpowers/specs/2026-09-04-arcvector-index-replication-design.md](../specs/2026-09-04-arcvector-index-replication-design.md)

## Global Constraints

- **No new dependencies.** `Cargo.toml`'s `[dependencies]` stays `usearch` and `serde_json`. No tokio, no crossbeam, no libc crate — declare the few C functions needed with `unsafe extern "C"`, as `src/attach.rs` already does.
- **Everything new is `#[cfg(feature = "replication")]`.** A build without it must compile, lint and test exactly as today.
- **`cargo clippy --all-targets` must add no warnings** for any feature combination. One pre-existing warning at `src/handler/registry.rs:215` ("this lint expectation is unfulfilled") is not yours; leave it.
- **Feature combinations that must build:** `(none)`, `migration`, `replication`, `replication,migration,cluster-aware`, `persistence`.
- **Key names carry a space** — `"AV OWNER"` — so the ASCII protocol cannot express them. Never name a key with the `arcus:` prefix: that marks the item `ITEM_INTERNAL` and exempts it from the replication gate, which is what the probe depends on.
- **Doc comments explain why, not what**, matching `Store::ping_slave` and `src/attach.rs`. Test names are sentences, matching `the_longest_untouched_names_come_first` in `src/handler/registry.rs`.
- **Engine error codes:** `ENGINE_REPL_SLAVE = 0x61`, `ENGINE_SWITCHOVER_WAIT = 0x62`, `ENGINE_SWITCHOVER_DONE = 0x63`, `ENGINE_NOT_MY_KEY = 0x64`.

## File Structure

| File | Responsibility |
|---|---|
| `src/handler/arcus/engine/kv.rs` (new) | Plain top-level KV get/set on `Store`. Only `AV OWNER` uses it. |
| `src/repl/mod.rs` (new) | Public entry points: `start`, `publish`, `role`. Owns the role state. |
| `src/repl/role.rs` (new) | The role state machine as pure logic, plus the heartbeat thread that drives it. |
| `src/repl/wire.rs` (new) | Framing and the four messages. Pure; no I/O. |
| `src/repl/master.rs` (new) | Listener, per-replica writer thread, bounded channels. |
| `src/repl/slave.rs` (new) | Connect, snapshot handling, convergent delta apply. |
| `src/handler/recovery.rs` | `stamp` reads the cached role instead of calling `ping_slave`. |
| `src/handler/arcus/engine/mod.rs` | `ping_slave` deleted; `kv` module declared. |
| `src/handler/access/mod.rs` | `resolve` stops reading metadata for a registered index. |
| `src/handler/cmd/vector.rs`, `src/handler/cmd/index.rs` | Call `repl::publish`. |
| `src/lib.rs` | `pub mod repl;` and `repl::start()`. |

---

### Task 1: Plain KV access on `Store`

`AV OWNER` is a top-level key, not a Map. Nothing in the crate reaches those yet.

**Files:**
- Create: `src/handler/arcus/engine/kv.rs`
- Modify: `src/handler/arcus/engine/mod.rs` (add `mod kv;` beside the existing `mod` lines)

**Interfaces:**
- Consumes: `Store::handle()`, `Store::vtable()`, `Store::cookie` (all private to `engine`), `error::{translate, check, Result, StoreError}`, `as_int`.
- Produces:
  - `Store::get_kv(&self, key: &str) -> Result<Vec<u8>>`
  - `Store::set_kv(&self, key: &str, value: &[u8]) -> Result<()>`

- [ ] **Step 1: Read the surrounding module to match its idiom**

Read `src/handler/arcus/engine/map.rs` end to end. It is the smallest file that calls the vtable and handles `item`/`item_info`; `kv.rs` is written in the same shape. Note how `probe_map` names the vtable function, checks it for `None`, and passes `self.cookie` through.

- [ ] **Step 2: Write `kv.rs`**

```rust
use std::os::raw::c_void;

use super::error::{Result, StoreError, as_int, check};
use super::{Store, item, item_info};
use crate::engine_api::{ENGINE_STORE_OPERATION_OPERATION_SET, ENGINE_ERROR_CODE_ENGINE_SUCCESS};

impl Store {
    /// Reads a top-level key.
    ///
    /// Only `AV OWNER` uses this. Index data lives in Maps, which `elem.rs`
    /// reaches; a plain key is how one fact is published to a whole
    /// replication group without belonging to any index.
    pub fn get_kv(&self, key: &str) -> Result<Vec<u8>> {
        let Some(get) = self.vtable().get else {
            return Err(StoreError::Unavailable);
        };
        let Some(release) = self.vtable().release else {
            return Err(StoreError::Unavailable);
        };
        let Some(info_of) = self.vtable().get_item_info else {
            return Err(StoreError::Unavailable);
        };

        let mut it: *mut item = std::ptr::null_mut();
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                std::ptr::from_mut(&mut it),
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                0,
            )
        };
        check(code)?;
        if it.is_null() {
            return Err(StoreError::KeyGone);
        }

        let mut info: item_info = unsafe { std::mem::zeroed() };
        info.nvalue = 1;
        let ok = unsafe { info_of(self.handle(), self.cookie, it, std::ptr::from_mut(&mut info)) };
        let out = if ok && !info.value[0].iov_base.is_null() {
            let len = info.value[0].iov_len;
            Ok(unsafe {
                std::slice::from_raw_parts(info.value[0].iov_base.cast::<u8>(), len).to_vec()
            })
        } else {
            Err(StoreError::CorruptElement)
        };
        unsafe { release(self.handle(), self.cookie, it) };
        out
    }

    /// Writes a top-level key, creating it.
    ///
    /// The allocate runs the replication gate under this key's own name, so on
    /// a replica this fails with `ENGINE_REPL_SLAVE` before anything is
    /// allocated. That refusal is what `repl::role` reads as the answer to
    /// "am I the master".
    pub fn set_kv(&self, key: &str, value: &[u8]) -> Result<()> {
        let Some(allocate) = self.vtable().allocate else {
            return Err(StoreError::Unavailable);
        };
        let Some(store) = self.vtable().store else {
            return Err(StoreError::Unavailable);
        };
        let Some(release) = self.vtable().release else {
            return Err(StoreError::Unavailable);
        };
        let Some(info_of) = self.vtable().get_item_info else {
            return Err(StoreError::Unavailable);
        };

        let mut it: *mut item = std::ptr::null_mut();
        let code = unsafe {
            allocate(
                self.handle(),
                self.cookie,
                std::ptr::from_mut(&mut it),
                key.as_ptr().cast::<c_void>(),
                key.len(),
                value.len(),
                0, // flags
                0, // exptime: never
                0, // cas
            )
        };
        check(code)?;

        let mut info: item_info = unsafe { std::mem::zeroed() };
        info.nvalue = 1;
        if !unsafe { info_of(self.handle(), self.cookie, it, std::ptr::from_mut(&mut info)) } {
            unsafe { release(self.handle(), self.cookie, it) };
            return Err(StoreError::CorruptElement);
        }
        let room = info.value[0].iov_len;
        if room < value.len() || info.value[0].iov_base.is_null() {
            unsafe { release(self.handle(), self.cookie, it) };
            return Err(StoreError::CorruptElement);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                value.as_ptr(),
                info.value[0].iov_base.cast::<u8>(),
                value.len(),
            );
        }

        let mut cas: u64 = 0;
        let code = unsafe {
            store(
                self.handle(),
                self.cookie,
                it,
                std::ptr::from_mut(&mut cas),
                ENGINE_STORE_OPERATION_OPERATION_SET,
                0,
            )
        };
        unsafe { release(self.handle(), self.cookie, it) };
        if code == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            Ok(())
        } else {
            Err(super::error::translate(code))
        }
    }
}
```

- [ ] **Step 3: Declare the module and re-export the types it needs**

In `src/handler/arcus/engine/mod.rs`, add `mod kv;` next to the other `mod` declarations. If `item` and `item_info` are not already imported there, add them to the `use crate::engine_api::{...}` list so `kv.rs`'s `use super::{Store, item, item_info};` resolves.

- [ ] **Step 4: Build every feature combination**

```bash
cd /Users/yeoncheol/Github/ArcVector
for f in "" "migration" "replication" "replication,migration,cluster-aware" "persistence"; do
  printf '%-38s ' "[${f:-base}]"
  if [ -z "$f" ]; then cargo build 2>&1 | tail -1; else cargo build --features "$f" 2>&1 | tail -1; fi
done
```

Expected: `Finished` on all five. `get_kv`/`set_kv` are unused for now, so add `#[allow(dead_code)]` on the `impl` block only if the build warns; remove it in Task 2 when they get a caller.

- [ ] **Step 5: Commit**

```bash
git add src/handler/arcus/engine/kv.rs src/handler/arcus/engine/mod.rs
git commit -m "feat: read and write plain top-level keys

AV OWNER is a key, not a Map field, and nothing in the crate reached one."
```

---

### Task 2: The role state machine

Pure logic first, so the transitions are testable without an engine.

**Files:**
- Create: `src/repl/role.rs`
- Create: `src/repl/mod.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Store::set_kv` (Task 1), `Store::background_keyed`, `StoreError`.
- Produces:
  - `pub enum Role { Unknown, Master, Replica }`
  - `pub fn role() -> Role`
  - `pub fn observe(probe: Probe) -> Option<Transition>`
  - `pub enum Probe { Accepted, Refused }`
  - `pub enum Transition { BecameMaster, BecameReplica }`
  - `pub fn start()` — spawns the heartbeat thread

- [ ] **Step 1: Write the failing tests**

Create `src/repl/role.rs` with only this test module and the type declarations it needs:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_accepted_write_makes_this_node_the_master() {
        let state = State::default();
        assert_eq!(state.observe(Probe::Accepted), Some(Transition::BecameMaster));
        assert_eq!(state.role(), Role::Master);
    }

    #[test]
    fn a_refused_write_makes_this_node_a_replica() {
        let state = State::default();
        assert_eq!(state.observe(Probe::Refused), Some(Transition::BecameReplica));
        assert_eq!(state.role(), Role::Replica);
    }

    #[test]
    fn an_unchanged_role_reports_no_transition() {
        let state = State::default();
        state.observe(Probe::Accepted);
        assert_eq!(state.observe(Probe::Accepted), None, "still master");
    }

    #[test]
    fn a_switchover_moves_the_role_both_ways() {
        let state = State::default();
        state.observe(Probe::Accepted);
        assert_eq!(state.observe(Probe::Refused), Some(Transition::BecameReplica));
        assert_eq!(state.observe(Probe::Accepted), Some(Transition::BecameMaster));
    }

    #[test]
    fn an_address_equal_to_our_own_is_stale() {
        let state = State::default();
        state.set_listen_addr(Some("10.0.0.1:5000".parse().unwrap()));
        assert!(state.is_stale_owner("10.0.0.1:5000"), "we published this");
        assert!(!state.is_stale_owner("10.0.0.2:5000"), "someone else did");
    }

    #[test]
    fn any_address_is_usable_when_we_have_published_none() {
        let state = State::default();
        assert!(!state.is_stale_owner("10.0.0.1:5000"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --features replication role:: 2>&1 | tail -20
```

Expected: compile errors — `State`, `Probe`, `Transition`, `Role` are not defined.

- [ ] **Step 3: Write the state machine**

Above the test module in `src/repl/role.rs`:

```rust
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// What this node is, as far as the last probe could tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// No probe has answered yet. Neither a master nor a replica acts.
    Unknown,
    Master,
    Replica,
}

/// The answer to a write of `AV OWNER`.
///
/// Only acceptance means master. `ENGINE_REPL_SLAVE`, `ENGINE_SWITCHOVER_DONE`
/// and `ENGINE_NOT_MY_KEY` all lead to the same conduct -- do not open a
/// listener, do not claim to be master -- so they collapse into one variant
/// rather than each carrying its own handling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    Accepted,
    Refused,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transition {
    BecameMaster,
    BecameReplica,
}

const UNKNOWN: u8 = 0;
const MASTER: u8 = 1;
const REPLICA: u8 = 2;

pub struct State {
    role: AtomicU8,
    /// The address this node last published, kept so a value read back can be
    /// recognised as its own. It is remembered rather than read from the
    /// socket because a demoted node has closed the listener by then.
    published: Mutex<Option<SocketAddr>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            role: AtomicU8::new(UNKNOWN),
            published: Mutex::new(None),
        }
    }
}

impl State {
    pub fn role(&self) -> Role {
        match self.role.load(Ordering::Acquire) {
            MASTER => Role::Master,
            REPLICA => Role::Replica,
            _ => Role::Unknown,
        }
    }

    /// Records a probe result, returning the transition if the role moved.
    pub fn observe(&self, probe: Probe) -> Option<Transition> {
        let next = match probe {
            Probe::Accepted => MASTER,
            Probe::Refused => REPLICA,
        };
        let previous = self.role.swap(next, Ordering::AcqRel);
        if previous == next {
            return None;
        }
        Some(match probe {
            Probe::Accepted => Transition::BecameMaster,
            Probe::Refused => Transition::BecameReplica,
        })
    }

    pub fn set_listen_addr(&self, addr: Option<SocketAddr>) {
        *self.published.lock().unwrap_or_else(|e| e.into_inner()) = addr;
    }

    pub fn listen_addr(&self) -> Option<SocketAddr> {
        *self.published.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether an `AV OWNER` value is one this node published itself.
    ///
    /// Right after a switchover the demoted node's probe fails -- telling it it
    /// is a replica -- while the value it reads is still its own, because the
    /// new master's write has not replicated yet. Dialling it would connect the
    /// node to itself.
    pub fn is_stale_owner(&self, addr: &str) -> bool {
        match self.listen_addr() {
            Some(mine) => addr == mine.to_string(),
            None => false,
        }
    }
}
```

- [ ] **Step 4: Wire the module in so the tests compile**

Create `src/repl/mod.rs`:

```rust
pub mod role;
```

In `src/lib.rs`, beside the existing `#[cfg(feature = "migration")] pub mod attach;`:

```rust
#[cfg(feature = "replication")]
pub mod repl;
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --features replication role:: 2>&1 | tail -20
```

Expected: 6 passed.

- [ ] **Step 6: Commit**

```bash
git add src/repl/role.rs src/repl/mod.rs src/lib.rs
git commit -m "feat: the replication role as a state machine

Only an accepted write means master; every refusal leads to the same
conduct, so they collapse into one variant."
```

---

### Task 3: The heartbeat, and the defect it fixes

`Store::ping_slave` probes by deleting `arcus:repl-probe`, and `rp_before_check` skips every key beginning `arcus:` before it reaches the replication check (`replication.c:5585`). It always reports "not a replica", so `recovery::stamp` never takes its skip, `put_elem` runs, the gate refuses it, and **a replica cannot build a graph at all**. This task makes the cached role the answer and deletes the broken probe.

**Files:**
- Modify: `src/repl/role.rs` (add the heartbeat and the global)
- Modify: `src/handler/recovery.rs:315-330` (`stamp`)
- Modify: `src/handler/arcus/engine/mod.rs` (delete `ping_slave`)

**Interfaces:**
- Consumes: `State` (Task 2), `Store::set_kv` (Task 1), `Store::background_keyed`.
- Produces:
  - `pub const OWNER_KEY: &str = "AV OWNER";`
  - `pub fn shared() -> &'static State`
  - `pub fn start_heartbeat()`
  - `pub fn probe_once(store: &Store, addr: SocketAddr) -> Probe`

- [ ] **Step 1: Add the heartbeat to `role.rs`**

```rust
use std::time::Duration;

use crate::handler::arcus::engine::{Store, StoreError};

/// Not `arcus:`-prefixed on purpose. That prefix marks an item ITEM_INTERNAL,
/// which exempts it from `scrub stale` -- and from the replication gate, which
/// is the whole probe. A key nothing refuses answers no question.
pub const OWNER_KEY: &str = "AV OWNER";

/// Long enough that the write is nothing, short enough that a value lost to
/// `scrub stale` is back before it matters. That scrub runs once per cluster
/// membership change, not continuously.
const HEARTBEAT: Duration = Duration::from_secs(10);

pub fn shared() -> &'static State {
    static SHARED: std::sync::OnceLock<State> = std::sync::OnceLock::new();
    SHARED.get_or_init(State::default)
}

/// One write of `AV OWNER`, and what it means.
pub fn probe_once(store: &Store, addr: SocketAddr) -> Probe {
    match store.set_kv(OWNER_KEY, addr.to_string().as_bytes()) {
        Ok(()) => Probe::Accepted,
        // Every refusal means the same thing. ReplicaSlave is the ordinary
        // case; a switchover in progress and a key belonging to another
        // migration group both land in Engine(_).
        Err(StoreError::ReplicaSlave) | Err(StoreError::Engine(_)) => Probe::Refused,
        // No engine, or a vtable without `store`. Nothing can be concluded, so
        // conclude the safe thing.
        Err(_) => Probe::Refused,
    }
}

pub fn start_heartbeat() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("arcvector-repl-role".to_owned())
        .spawn(|| {
            loop {
                beat();
                std::thread::sleep(HEARTBEAT);
            }
        });
}

fn beat() {
    let state = shared();
    let Some(addr) = state.listen_addr() else {
        return; // No listener yet; Task 5 provides one.
    };
    let Some(store) = Store::background_keyed() else {
        return;
    };
    if let Some(change) = state.observe(probe_once(&store, addr)) {
        eprintln!("ArcVector: replication role is now {change:?}");
    }
}
```

- [ ] **Step 2: Write the failing test for `stamp`**

Add to `src/handler/recovery.rs`'s test module (create one at the end of the file if absent):

```rust
#[cfg(all(test, feature = "replication"))]
mod role_tests {
    use crate::repl::role::{Probe, shared};

    #[test]
    fn a_replica_skips_the_ownership_write() {
        shared().observe(Probe::Refused);
        assert!(
            crate::repl::role::is_replica(),
            "stamp reads this instead of asking the engine, which cannot answer"
        );
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

```bash
cargo test --features replication role_tests 2>&1 | tail -20
```

Expected: FAIL — `is_replica` is not defined.

- [ ] **Step 4: Add `is_replica` and use it in `stamp`**

In `src/repl/role.rs`:

```rust
/// Whether writes are refused here. `recovery::stamp` asks this instead of the
/// engine: `Store::ping_slave` could not answer, because it probed under an
/// `arcus:`-prefixed key that the replication gate lets through untouched.
pub fn is_replica() -> bool {
    shared().role() == Role::Replica
}
```

In `src/handler/recovery.rs`, replace the first two lines of `stamp`:

```rust
pub(super) fn stamp(store: &Store, name: &str, owner: &str) -> Result<()> {
    #[cfg(feature = "replication")]
    if crate::repl::role::is_replica() {
        return Ok(());
    }
    let (meta, layout) = match read_metadata(store, name) {
```

- [ ] **Step 5: Delete `ping_slave`**

Remove the whole `ping_slave` function and its doc comment from `src/handler/arcus/engine/mod.rs:139-175`. Then confirm nothing else calls it:

```bash
grep -rn "ping_slave" --include="*.rs" src
```

Expected: no output. The comment in `src/handler/access/mod.rs:25-29` names it — rewrite that comment to say `stamp` consults the cached replication role.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test --features replication 2>&1 | grep "^test result"
cargo test 2>&1 | grep "^test result"
```

Expected: `ok` for both, with the replication build one test higher than before.

- [ ] **Step 7: Commit**

```bash
git add src/repl/role.rs src/handler/recovery.rs src/handler/arcus/engine/mod.rs src/handler/access/mod.rs
git commit -m "fix: a replica could not build a graph for a Map it holds

ping_slave probed by deleting an arcus:-prefixed key, and rp_before_check
lets every such key through before it reaches the replication check. It
always answered 'not a replica', so stamp never took its skip, put_elem
ran, the gate refused it under the index's own name, and the error
reached resolve. The cached role answers instead."
```

---

### Task 4: The wire format

**Files:**
- Create: `src/repl/wire.rs`
- Modify: `src/repl/mod.rs`

**Interfaces:**
- Consumes: nothing. This file is pure and has no `Store`.
- Produces:
  - `pub enum Msg { Sync { node_id: String, proto_ver: u16, accepts_graph: bool }, Snapshot { indexes: Vec<String> }, Delta { index: String, id: String, op: Op }, Resync { index: String } }`
  - `pub enum Op { Upsert, Delete }`
  - `pub fn encode(msg: &Msg) -> Vec<u8>`
  - `pub fn decode(frame: &[u8]) -> Result<Msg, WireError>`
  - `pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, WireError>`
  - `pub const PROTO_VER: u16 = 1;`

- [ ] **Step 1: Write the failing tests**

Create `src/repl/wire.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Msg) {
        let bytes = encode(&msg);
        let mut cursor = std::io::Cursor::new(&bytes);
        let frame = read_frame(&mut cursor).expect("a whole frame");
        assert_eq!(decode(&frame).expect("decodes"), msg);
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        roundtrip(Msg::Sync {
            node_id: "abc/1/2".to_owned(),
            proto_ver: PROTO_VER,
            accepts_graph: false,
        });
        roundtrip(Msg::Snapshot { indexes: vec!["a".to_owned(), "b".to_owned()] });
        roundtrip(Msg::Snapshot { indexes: Vec::new() });
        roundtrip(Msg::Delta {
            index: "idx".to_owned(),
            id: "v42".to_owned(),
            op: Op::Upsert,
        });
        roundtrip(Msg::Delta {
            index: "idx".to_owned(),
            id: "v42".to_owned(),
            op: Op::Delete,
        });
        roundtrip(Msg::Resync { index: "idx".to_owned() });
    }

    #[test]
    fn a_truncated_frame_is_refused_rather_than_guessed() {
        let bytes = encode(&Msg::Resync { index: "idx".to_owned() });
        let mut cursor = std::io::Cursor::new(&bytes[..bytes.len() - 1]);
        assert!(matches!(read_frame(&mut cursor), Err(WireError::Truncated)));
    }

    #[test]
    fn an_oversized_length_is_refused_before_it_is_allocated() {
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.push(0);
        let mut cursor = std::io::Cursor::new(&bytes);
        assert!(matches!(read_frame(&mut cursor), Err(WireError::TooLarge(_))));
    }

    #[test]
    fn an_unknown_message_type_is_refused() {
        assert!(matches!(decode(&[0xff, 0x00]), Err(WireError::UnknownType(0xff))));
    }

    #[test]
    fn an_empty_frame_is_refused() {
        assert!(matches!(decode(&[]), Err(WireError::Truncated)));
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test --features replication wire:: 2>&1 | tail -20
```

Expected: compile errors — nothing in `wire` is defined.

- [ ] **Step 3: Write the format**

Above the test module:

```rust
use std::io::Read;

/// Bumped when a change would make an older peer misread a frame. The master
/// refuses a `Sync` that does not match rather than streaming into a decoder
/// that disagrees.
pub const PROTO_VER: u16 = 1;

/// Frames are small: a name, an id, a handful of bytes. A cap this far above
/// anything legitimate turns a desynchronised stream into an error instead of
/// an allocation.
const MAX_FRAME: usize = 1 << 20;

const T_SYNC: u8 = 1;
const T_SNAPSHOT: u8 = 2;
const T_DELTA: u8 = 3;
const T_RESYNC: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Upsert,
    Delete,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Msg {
    Sync { node_id: String, proto_ver: u16, accepts_graph: bool },
    Snapshot { indexes: Vec<String> },
    Delta { index: String, id: String, op: Op },
    Resync { index: String },
}

#[derive(Debug)]
pub enum WireError {
    Truncated,
    TooLarge(usize),
    UnknownType(u8),
    BadUtf8,
    Io(std::io::Error),
}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take_str(b: &[u8], at: &mut usize) -> Result<String, WireError> {
    let len = take_u32(b, at)? as usize;
    let end = at.checked_add(len).ok_or(WireError::Truncated)?;
    let raw = b.get(*at..end).ok_or(WireError::Truncated)?;
    *at = end;
    String::from_utf8(raw.to_vec()).map_err(|_| WireError::BadUtf8)
}

fn take_u32(b: &[u8], at: &mut usize) -> Result<u32, WireError> {
    let end = at.checked_add(4).ok_or(WireError::Truncated)?;
    let raw: [u8; 4] = b.get(*at..end).ok_or(WireError::Truncated)?
        .try_into().map_err(|_| WireError::Truncated)?;
    *at = end;
    Ok(u32::from_le_bytes(raw))
}

/// `[u32 len][u8 type][payload]`, little-endian, the shape of arcus's own
/// `msg_chan` header so the two read alike.
pub fn encode(msg: &Msg) -> Vec<u8> {
    let mut body = Vec::new();
    match msg {
        Msg::Sync { node_id, proto_ver, accepts_graph } => {
            body.push(T_SYNC);
            put_str(&mut body, node_id);
            body.extend_from_slice(&proto_ver.to_le_bytes());
            body.push(u8::from(*accepts_graph));
        }
        Msg::Snapshot { indexes } => {
            body.push(T_SNAPSHOT);
            body.extend_from_slice(&(indexes.len() as u32).to_le_bytes());
            for name in indexes {
                put_str(&mut body, name);
            }
        }
        Msg::Delta { index, id, op } => {
            body.push(T_DELTA);
            put_str(&mut body, index);
            put_str(&mut body, id);
            body.push(match op {
                Op::Upsert => 0,
                Op::Delete => 1,
            });
        }
        Msg::Resync { index } => {
            body.push(T_RESYNC);
            put_str(&mut body, index);
        }
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn decode(frame: &[u8]) -> Result<Msg, WireError> {
    let (&kind, rest) = frame.split_first().ok_or(WireError::Truncated)?;
    let at = &mut 0usize;
    match kind {
        T_SYNC => {
            let node_id = take_str(rest, at)?;
            let end = at.checked_add(2).ok_or(WireError::Truncated)?;
            let raw: [u8; 2] = rest.get(*at..end).ok_or(WireError::Truncated)?
                .try_into().map_err(|_| WireError::Truncated)?;
            *at = end;
            let accepts_graph = *rest.get(*at).ok_or(WireError::Truncated)? != 0;
            Ok(Msg::Sync { node_id, proto_ver: u16::from_le_bytes(raw), accepts_graph })
        }
        T_SNAPSHOT => {
            let count = take_u32(rest, at)? as usize;
            let mut indexes = Vec::new();
            indexes.try_reserve(count.min(4096)).map_err(|_| WireError::TooLarge(count))?;
            for _ in 0..count {
                indexes.push(take_str(rest, at)?);
            }
            Ok(Msg::Snapshot { indexes })
        }
        T_DELTA => {
            let index = take_str(rest, at)?;
            let id = take_str(rest, at)?;
            let op = match rest.get(*at).ok_or(WireError::Truncated)? {
                0 => Op::Upsert,
                _ => Op::Delete,
            };
            Ok(Msg::Delta { index, id, op })
        }
        T_RESYNC => Ok(Msg::Resync { index: take_str(rest, at)? }),
        other => Err(WireError::UnknownType(other)),
    }
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, WireError> {
    let mut len = [0u8; 4];
    read_exactly(r, &mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(WireError::TooLarge(len));
    }
    let mut frame = Vec::new();
    frame.try_reserve(len).map_err(|_| WireError::TooLarge(len))?;
    frame.resize(len, 0);
    read_exactly(r, &mut frame)?;
    Ok(frame)
}

fn read_exactly<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), WireError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(WireError::Truncated),
        Err(e) => Err(WireError::Io(e)),
    }
}
```

- [ ] **Step 4: Declare the module**

In `src/repl/mod.rs`, add `pub mod wire;` above `pub mod role;`.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --features replication wire:: 2>&1 | tail -20
```

Expected: 5 passed.

- [ ] **Step 6: Commit**

```bash
git add src/repl/wire.rs src/repl/mod.rs
git commit -m "feat: the replication wire format

Four messages behind a length-prefixed frame, shaped like arcus's own
msg_chan header. Pure, so the whole format is unit tested."
```

---

### Task 5: The master's listener and per-replica queues

**Files:**
- Create: `src/repl/master.rs`
- Modify: `src/repl/mod.rs`

**Interfaces:**
- Consumes: `wire::{Msg, Op, PROTO_VER, encode, read_frame, decode}`, `role::{shared, Role}`, `registry::indexes`.
- Produces:
  - `pub fn open() -> std::io::Result<SocketAddr>` — binds, spawns the acceptor, returns the bound address
  - `pub fn close()`
  - `pub fn publish(index: &str, id: &str, op: Op)`
  - `pub fn queued() -> usize`, `pub fn full_events() -> usize`, `pub fn resyncs_sent() -> usize`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replica_that_stops_reading_is_marked_for_resync_once() {
        let replica = Replica::new("node-a".to_owned(), 4);
        for i in 0..4 {
            assert!(replica.offer(delta(i)), "within capacity");
        }
        assert!(!replica.offer(delta(4)), "capacity is spent");
        assert!(replica.needs_resync(), "and that is what falling behind means");
        assert!(!replica.offer(delta(5)), "still refused");
        assert_eq!(replica.take_resync_flag(), true, "reported exactly once");
        assert_eq!(replica.take_resync_flag(), false);
    }

    #[test]
    fn one_slow_replica_does_not_affect_another() {
        let slow = Replica::new("slow".to_owned(), 1);
        let fast = Replica::new("fast".to_owned(), 8);
        slow.offer(delta(0));
        slow.offer(delta(1));
        assert!(slow.needs_resync());
        assert!(fast.offer(delta(0)), "its own capacity is untouched");
        assert!(!fast.needs_resync());
    }

    #[test]
    fn a_marked_replica_drops_what_it_had_queued() {
        let replica = Replica::new("node-a".to_owned(), 2);
        replica.offer(delta(0));
        replica.offer(delta(1));
        replica.offer(delta(2));
        assert_eq!(replica.depth(), 0, "the queue is drained to make room for RESYNC");
    }

    fn delta(n: usize) -> crate::repl::wire::Msg {
        crate::repl::wire::Msg::Delta {
            index: "idx".to_owned(),
            id: format!("v{n}"),
            op: crate::repl::wire::Op::Upsert,
        }
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test --features replication master:: 2>&1 | tail -20
```

Expected: compile errors — `Replica` is not defined.

- [ ] **Step 3: Write `Replica` and the listener**

```rust
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::wire::{self, Msg, Op, PROTO_VER};
use crate::handler::registry;

/// Deep enough to ride out a scheduling hiccup, shallow enough that a replica
/// that has genuinely stopped is noticed rather than buffered indefinitely.
const DEPTH: usize = 1024;

/// A wedged peer must not hold its writer thread forever while its channel
/// silently fills.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection that does not say SYNC is not a replica.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);

static FULL_EVENTS: AtomicUsize = AtomicUsize::new(0);
static RESYNCS_SENT: AtomicUsize = AtomicUsize::new(0);

pub struct Replica {
    pub node_id: String,
    tx: SyncSender<Msg>,
    rx: Mutex<Option<Receiver<Msg>>>,
    depth: AtomicUsize,
    resync: AtomicBool,
}

impl Replica {
    pub fn new(node_id: String, depth: usize) -> Self {
        let (tx, rx) = sync_channel(depth);
        Self {
            node_id,
            tx,
            rx: Mutex::new(Some(rx)),
            depth: AtomicUsize::new(0),
            resync: AtomicBool::new(false),
        }
    }

    /// Queues a message, or marks this replica behind. Never blocks: publishing
    /// runs on a request thread.
    pub fn offer(&self, msg: Msg) -> bool {
        if self.resync.load(Ordering::Acquire) {
            return false;
        }
        match self.tx.try_send(msg) {
            Ok(()) => {
                self.depth.fetch_add(1, Ordering::AcqRel);
                true
            }
            Err(TrySendError::Full(_)) => {
                FULL_EVENTS.fetch_add(1, Ordering::Relaxed);
                self.mark_behind();
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Drains what is queued and marks the replica for a fresh SNAPSHOT. The
    /// drain is what makes room for the RESYNC that follows.
    fn mark_behind(&self) {
        self.resync.store(true, Ordering::Release);
        if let Some(rx) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            while rx.try_recv().is_ok() {
                self.depth.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    pub fn needs_resync(&self) -> bool {
        self.resync.load(Ordering::Acquire)
    }

    /// Reports the flag once, clearing it, so one fall-behind sends one RESYNC.
    pub fn take_resync_flag(&self) -> bool {
        self.resync.swap(false, Ordering::AcqRel)
    }

    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Acquire)
    }

    fn take_receiver(&self) -> Option<Receiver<Msg>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

fn replicas() -> &'static Mutex<Vec<Arc<Replica>>> {
    static REPLICAS: OnceLock<Mutex<Vec<Arc<Replica>>>> = OnceLock::new();
    REPLICAS.get_or_init(|| Mutex::new(Vec::new()))
}

static LISTENING: AtomicBool = AtomicBool::new(false);

pub fn publish(index: &str, id: &str, op: Op) {
    if !LISTENING.load(Ordering::Acquire) {
        return;
    }
    let list = replicas().lock().unwrap_or_else(|e| e.into_inner());
    for replica in list.iter() {
        replica.offer(Msg::Delta {
            index: index.to_owned(),
            id: id.to_owned(),
            op,
        });
    }
}

pub fn queued() -> usize {
    replicas()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|r| r.depth())
        .sum()
}

pub fn full_events() -> usize {
    FULL_EVENTS.load(Ordering::Relaxed)
}

pub fn resyncs_sent() -> usize {
    RESYNCS_SENT.load(Ordering::Relaxed)
}

pub fn open() -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("0.0.0.0:0")?;
    let addr = listener.local_addr()?;
    LISTENING.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("arcvector-repl-accept".to_owned())
        .spawn(move || accept_loop(&listener))?;
    Ok(addr)
}

pub fn close() {
    LISTENING.store(false, Ordering::Release);
    replicas().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

fn accept_loop(listener: &TcpListener) {
    while LISTENING.load(Ordering::Acquire) {
        let Ok((sock, _)) = listener.accept() else { continue };
        std::thread::spawn(move || {
            if let Some(replica) = handshake(&sock) {
                serve(sock, replica);
            }
        });
    }
}

/// Reads the replica's SYNC. A connection that says nothing within the timeout
/// is dropped: it is not a replica, whatever else it may be.
fn handshake(sock: &TcpStream) -> Option<Arc<Replica>> {
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
    let mut reader = sock.try_clone().ok()?;
    let frame = wire::read_frame(&mut reader).ok()?;
    let Ok(Msg::Sync { node_id, proto_ver, .. }) = wire::decode(&frame) else {
        return None;
    };
    if proto_ver != PROTO_VER {
        eprintln!("ArcVector: refusing replica {node_id}: protocol {proto_ver}, we speak {PROTO_VER}");
        return None;
    }

    let replica = Arc::new(Replica::new(node_id.clone(), DEPTH));
    let mut list = replicas().lock().unwrap_or_else(|e| e.into_inner());
    // A reconnect from the same node replaces its old handle; the old writer
    // finds its channel disconnected and stops.
    list.retain(|r| r.node_id != node_id);
    if list.try_reserve(1).is_err() {
        return None;
    }
    list.push(Arc::clone(&replica));
    Some(replica)
}

fn serve(mut sock: TcpStream, replica: Arc<Replica>) {
    let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = sock.set_nodelay(true);
    let Some(rx) = replica.take_receiver() else { return };

    if send(&mut sock, &snapshot()).is_err() {
        return;
    }
    while let Ok(msg) = rx.recv() {
        replica.depth.fetch_sub(1, Ordering::AcqRel);
        if send(&mut sock, &msg).is_err() {
            break;
        }
        if replica.take_resync_flag() {
            RESYNCS_SENT.fetch_add(1, Ordering::Relaxed);
            if send(&mut sock, &Msg::Resync { index: String::new() }).is_err() {
                break;
            }
        }
    }
    replicas()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|r| !Arc::ptr_eq(r, &replica));
}

/// The names only. A replica resolves the vectors from its own Map, which arcus
/// has already replicated; it cannot derive the names, because its registry is
/// empty at startup and nothing enumerates arcus keys.
fn snapshot() -> Msg {
    Msg::Snapshot {
        indexes: registry::indexes().iter().map(|i| i.name.clone()).collect(),
    }
}

fn send(sock: &mut TcpStream, msg: &Msg) -> std::io::Result<()> {
    sock.write_all(&wire::encode(msg))
}
```

- [ ] **Step 4: Declare the module and run the tests**

Add `pub mod master;` to `src/repl/mod.rs`, then:

```bash
cargo test --features replication master:: 2>&1 | tail -20
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/repl/master.rs src/repl/mod.rs
git commit -m "feat: the master's listener and per-replica queues

A full channel is what falling behind means: TCP carries a slow replica's
slowness back to it. One slow replica fills only its own."
```

---

### Task 6: Publishing from the commands that change the graph

**Files:**
- Modify: `src/repl/mod.rs`
- Modify: `src/handler/cmd/vector.rs` (`vadd`, `vdel`)
- Modify: `src/handler/cmd/index.rs` (`vcreate`, `vdrop`)

**Interfaces:**
- Consumes: `master::publish`, `role::{shared, Role}`, `wire::Op`.
- Produces: `pub fn publish(index: &str, id: &str, op: wire::Op)` on `crate::repl`.

- [ ] **Step 1: Add the hook to `src/repl/mod.rs`**

```rust
pub mod master;
pub mod role;
pub mod slave;
pub mod wire;

pub use wire::Op;

/// Tells the replicas an id changed. Costs one atomic load when this node is
/// not a master or has no replicas, which is every call on most deployments.
///
/// Called after the engine write has committed, never before: a replica that
/// resolves the id against its own Map must be able to find something there.
pub fn publish(index: &str, id: &str, op: Op) {
    if role::shared().role() != role::Role::Master {
        return;
    }
    master::publish(index, id, op);
}
```

- [ ] **Step 2: Call it from `vadd`**

In `src/handler/cmd/vector.rs`, in `vadd`, both arms that answer `Reply::Stored` publish an upsert before returning. The `Published::Indexed` arm becomes:

```rust
Ok(Published::Indexed) => {
    #[cfg(feature = "replication")]
    crate::repl::publish(name, id, crate::repl::Op::Upsert);
    Ok(Reply::Stored)
}
```

Apply the same two lines to the `Published::Unindexed(addr)` arm, after `index_it(...)` and before `Ok(Reply::Stored)`.

- [ ] **Step 3: Call it from `vdel`, `vcreate` and `vdrop`**

- `vdel` in `src/handler/cmd/vector.rs`: on the path that answers `Reply::Deleted`, publish `Op::Delete` with that index and id.
- `vcreate` in `src/handler/cmd/index.rs`: on the path that answers `Reply::Created`, publish `Op::Upsert` with the index name and an **empty id** — the replica reads that as "this index exists, build it" rather than a single element.
- `vdrop` in `src/handler/cmd/index.rs`: on the path that answers `Reply::Dropped`, publish `Op::Delete` with the index name and an empty id.

`vsetattr` publishes nothing: attributes live in the Map and reach the replica through arcus.

- [ ] **Step 4: Build every feature combination**

```bash
for f in "" "migration" "replication" "replication,migration,cluster-aware" "persistence"; do
  printf '%-38s ' "[${f:-base}]"
  if [ -z "$f" ]; then cargo build 2>&1 | tail -1; else cargo build --features "$f" 2>&1 | tail -1; fi
done
```

Expected: `Finished` on all five. A build without `replication` must not reference `crate::repl` at all.

- [ ] **Step 5: Commit**

```bash
git add src/repl/mod.rs src/handler/cmd/vector.rs src/handler/cmd/index.rs
git commit -m "feat: publish index changes to replicas

Only the four commands that change what the graph should hold. vsetattr
is not among them: attributes reach a replica through arcus."
```

---

### Task 7: The replica

**Files:**
- Create: `src/repl/slave.rs`

**Interfaces:**
- Consumes: `wire::{Msg, Op, PROTO_VER, encode, decode, read_frame}`, `role::{shared, OWNER_KEY}`, `Store::{get_kv, background_keyed, probe_map, get_elem}`, `registry::{get, indexes}`, `recovery::fill`, `owner::ours`.
- Produces: `pub fn connect_loop()`, `pub fn deferred() -> usize`

- [ ] **Step 1: Write the failing tests for the deferred set**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deferred_upsert_is_retried_and_then_forgotten() {
        let mut pending = Pending::default();
        pending.defer("idx".to_owned(), "v1".to_owned());
        assert_eq!(pending.len(), 1);
        pending.resolved("idx", "v1");
        assert_eq!(pending.len(), 0, "the Map caught up");
    }

    #[test]
    fn the_deferred_set_has_a_bound_that_forces_a_resync() {
        let mut pending = Pending::default();
        for i in 0..MAX_DEFERRED {
            pending.defer("idx".to_owned(), format!("v{i}"));
        }
        assert!(!pending.overflowed(), "at the bound, not past it");
        pending.defer("idx".to_owned(), "one-too-many".to_owned());
        assert!(
            pending.overflowed(),
            "deferred work is lag that has not reached the socket; \
             past the bound it would hide how far behind we are"
        );
    }

    #[test]
    fn the_indexes_needing_resync_are_named() {
        let mut pending = Pending::default();
        pending.defer("a".to_owned(), "v1".to_owned());
        pending.defer("b".to_owned(), "v2".to_owned());
        let mut names = pending.indexes();
        names.sort();
        assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
cargo test --features replication slave:: 2>&1 | tail -20
```

Expected: compile errors — `Pending` is not defined.

- [ ] **Step 3: Write `Pending` and the connect loop**

```rust
use std::collections::HashSet;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::role::{self, OWNER_KEY};
use super::wire::{self, Msg, Op, PROTO_VER};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry;

/// Past this, deferred work is hiding lag rather than absorbing a race, and the
/// affected indexes are resynchronised instead.
const MAX_DEFERRED: usize = 4096;

/// How long an unresolvable defer is tolerated before its index is resynced.
const DEFER_PATIENCE: Duration = Duration::from_secs(30);

const RETRY: Duration = Duration::from_secs(1);

static DEFERRED: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub struct Pending {
    waiting: HashSet<(String, String)>,
    overflowed: bool,
}

impl Pending {
    pub fn defer(&mut self, index: String, id: String) {
        if self.waiting.len() >= MAX_DEFERRED {
            self.overflowed = true;
            return;
        }
        if self.waiting.try_reserve(1).is_err() {
            self.overflowed = true;
            return;
        }
        self.waiting.insert((index, id));
        DEFERRED.store(self.waiting.len(), Ordering::Relaxed);
    }

    pub fn resolved(&mut self, index: &str, id: &str) {
        self.waiting.remove(&(index.to_owned(), id.to_owned()));
        DEFERRED.store(self.waiting.len(), Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn indexes(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .waiting
            .iter()
            .map(|(index, _)| index.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

pub fn deferred() -> usize {
    DEFERRED.load(Ordering::Relaxed)
}

/// Dials whoever `AV OWNER` names, for as long as this node is a replica.
pub fn connect_loop() {
    loop {
        if role::shared().role() != role::Role::Replica {
            std::thread::sleep(RETRY);
            continue;
        }
        match master_addr() {
            Some(addr) => {
                if let Ok(sock) = TcpStream::connect(&addr) {
                    session(sock);
                }
            }
            None => {}
        }
        std::thread::sleep(RETRY);
    }
}

/// `AV OWNER` arrived here through arcus replication, the same path that
/// carries the Map. A value equal to our own published address is one we wrote
/// before being demoted, and the new master's write has not replicated yet.
fn master_addr() -> Option<String> {
    let store = Store::background_keyed()?;
    let raw = store.get_kv(OWNER_KEY).ok()?;
    let addr = String::from_utf8(raw).ok()?;
    if role::shared().is_stale_owner(&addr) {
        return None;
    }
    Some(addr)
}

fn session(mut sock: TcpStream) {
    let _ = sock.set_nodelay(true);
    let hello = wire::encode(&Msg::Sync {
        node_id: crate::owner::ours().to_owned(),
        proto_ver: PROTO_VER,
        accepts_graph: false,
    });
    if sock.write_all(&hello).is_err() {
        return;
    }

    let mut pending = Pending::default();
    let mut reader = match sock.try_clone() {
        Ok(r) => r,
        Err(_) => return,
    };
    let _ = reader.set_read_timeout(Some(RETRY));

    loop {
        match wire::read_frame(&mut reader) {
            Ok(frame) => match wire::decode(&frame) {
                Ok(msg) => apply(msg, &mut pending),
                Err(_) => return, // A frame we cannot read desynchronises us.
            },
            Err(wire::WireError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                retry_deferred(&mut pending);
            }
            Err(_) => return,
        }
        if pending.overflowed() {
            for name in pending.indexes() {
                rebuild(&name);
            }
            pending = Pending::default();
        }
    }
}

fn apply(msg: Msg, pending: &mut Pending) {
    match msg {
        Msg::Snapshot { indexes } => {
            for name in indexes {
                converge(&name);
            }
        }
        Msg::Resync { index } => {
            if index.is_empty() {
                for held in registry::indexes() {
                    converge(&held.name);
                }
            } else {
                converge(&index);
            }
        }
        Msg::Delta { index, id, op } => match op {
            Op::Upsert if id.is_empty() => converge(&index),
            Op::Delete if id.is_empty() => {
                registry::remove(&index);
            }
            Op::Upsert => {
                if !upsert(&index, &id) {
                    pending.defer(index, id);
                }
            }
            Op::Delete => delete(&index, &id),
        },
        Msg::Sync { .. } => {} // A replica never receives one.
    }
}

/// Rebuilds this index only if it disagrees with its own Map. One `getattr`,
/// the check `access::sweep` already trusts.
fn converge(name: &str) {
    let Some(store) = Store::background_keyed() else { return };
    let Some(index) = registry::get(name) else {
        rebuild(name);
        return;
    };
    match store.probe_map(name) {
        Ok(map) => {
            let in_map = map.count.saturating_sub(1) as usize;
            if in_map != index.ann.len() {
                rebuild(name);
            }
        }
        Err(StoreError::KeyGone) => {
            registry::remove(name);
        }
        Err(_) => {}
    }
}

fn rebuild(name: &str) {
    let Some(store) = Store::background_keyed() else { return };
    let Some(index) = registry::get(name) else { return };
    #[cfg(recovery)]
    {
        crate::handler::recovery::ensure_builder();
        let _ = crate::handler::recovery::fill(&store, &index);
    }
    let _ = (&store, &index);
}

/// True once the element is in this node's own Map. The delta is a hint about
/// where to look, never the data itself, so it may arrive before arcus has
/// replicated what it refers to.
fn upsert(index_name: &str, id: &str) -> bool {
    let Some(store) = Store::background_keyed() else { return false };
    let Some(index) = registry::get(index_name) else { return false };
    match store.hold_addr(index_name, id) {
        Ok(held) => {
            let addr = held.keep();
            let layout = index.ann.layout;
            let vector = store.with_vector_at(addr, layout, <[u8]>::to_vec);
            match vector {
                Ok(Some(bytes)) => {
                    let _ = index.ann.add_unless_known(addr, || Ok(Some(bytes)));
                    true
                }
                _ => false,
            }
        }
        Err(_) => false,
    }
}

fn delete(index_name: &str, id: &str) {
    let Some(store) = Store::background_keyed() else { return };
    let Some(index) = registry::get(index_name) else { return };
    if let Ok(Some(held)) = store.take_addr(index_name, id) {
        index.ann.unclaimed(held.addr());
    }
}

fn retry_deferred(pending: &mut Pending) {
    for (index, id) in pending.waiting.clone() {
        if upsert(&index, &id) {
            pending.resolved(&index, &id);
        }
    }
}
```

- [ ] **Step 4: Reconcile the helper signatures against the real ones**

`upsert` and `delete` above use `Store::hold_addr`, `Store::take_addr`, `Store::with_vector_at` and `AnnIndex::{add_unless_known, unclaimed, layout}`. Open `src/handler/arcus/engine/elem.rs:296-360` and `src/handler/usearch/index.rs:813-864` and make the calls match exactly — argument order, whether a closure or a value is taken, and what the `Result` wraps. Adjust the code above rather than the existing APIs. Then:

```bash
cargo build --features replication 2>&1 | tail -20
```

Expected: `Finished`. Fix any mismatch the compiler names.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --features replication slave:: 2>&1 | tail -20
```

Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add src/repl/slave.rs src/repl/mod.rs
git commit -m "feat: the replica applies deltas against its own Map

A delta says where to look, not what to write, so the two replication
streams need no relative ordering. What cannot be resolved yet is
deferred, under a bound: deferred work is lag that has not reached the
socket."
```

---

### Task 8: Starting it, and reacting to role changes

**Files:**
- Modify: `src/repl/mod.rs`
- Modify: `src/repl/role.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `master::{open, close}`, `slave::connect_loop`, `role::{shared, start_heartbeat}`.
- Produces: `pub fn start()` on `crate::repl`.

- [ ] **Step 1: Make the published address routable**

`TcpListener::bind("0.0.0.0:0")` reports `0.0.0.0:<port>`, which no other host
can dial. Add to `src/repl/role.rs`:

```rust
/// The address to publish, given what the listener bound to.
///
/// A wildcard bind says nothing about where to reach this node, so the spec's
/// fallback chain applies: the arcus service socket's address when it is
/// specific, then the hostname, then any non-loopback interface.
pub fn advertised(bound: SocketAddr) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return bound;
    }
    if let Some(ip) = service_ip().or_else(hostname_ip) {
        return SocketAddr::new(ip, bound.port());
    }
    bound
}

/// The address arcus is serving on, when it is not a wildcard. `attach` finds
/// the same socket; this reads only its address.
fn service_ip() -> Option<std::net::IpAddr> { /* see below */ }

fn hostname_ip() -> Option<std::net::IpAddr> {
    let host = std::process::Command::new("hostname").output().ok()?;
    let host = String::from_utf8(host.stdout).ok()?;
    std::net::ToSocketAddrs::to_socket_addrs(&(host.trim(), 0))
        .ok()?
        .map(|a| a.ip())
        .find(|ip| !ip.is_loopback() && ip.is_ipv4())
}
```

For `service_ip`, reuse the scan in `src/attach.rs`: it already finds the
daemon's listening sockets with `SO_TYPE` + `getpeername`/`ENOTCONN`. Under
`#[cfg(feature = "migration")]` call into it; otherwise copy the twenty lines
rather than making `replication` depend on `migration`. Return `None` when the
address found is unspecified or loopback.

- [ ] **Step 2: Write the failing test for it**

```rust
#[test]
fn a_wildcard_bind_is_not_published_as_it_stands() {
    let bound: SocketAddr = "0.0.0.0:5000".parse().unwrap();
    let out = advertised(bound);
    assert_eq!(out.port(), 5000, "the port is what the listener actually got");
    assert!(!out.ip().is_unspecified(), "no peer can dial a wildcard");
}

#[test]
fn a_specific_bind_is_published_unchanged() {
    let bound: SocketAddr = "10.0.0.5:5000".parse().unwrap();
    assert_eq!(advertised(bound), bound);
}
```

Run: `cargo test --features replication advertised 2>&1 | tail -20`. The first
test may legitimately fail on a host with no non-loopback IPv4 — if so, assert
`out.ip().is_unspecified() == false || service_ip().is_none()` instead, and say
so in a comment.

- [ ] **Step 3: Add `start` to `src/repl/mod.rs`**

```rust
/// Starts replication. Idempotent, and safe to call before the daemon has a
/// listening socket of its own: everything here waits for its own trigger.
pub fn start() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    match master::open() {
        Ok(bound) => role::shared().set_listen_addr(Some(role::advertised(bound))),
        Err(e) => {
            eprintln!("ArcVector: replication listener not opened ({e}); no index replication");
            return;
        }
    }
    role::start_heartbeat();
    let _ = std::thread::Builder::new()
        .name("arcvector-repl-slave".to_owned())
        .spawn(slave::connect_loop);
}
```

- [ ] **Step 4: Act on transitions in `role.rs`**

Replace the body of `beat` so a transition tears down what the old role built:

```rust
fn beat() {
    let state = shared();
    let Some(addr) = state.listen_addr() else { return };
    let Some(store) = Store::background_keyed() else { return };
    match state.observe(probe_once(&store, addr)) {
        Some(Transition::BecameMaster) => {
            eprintln!("ArcVector: this node is the replication master");
            // Its own graphs may be behind: it was a replica until now, and no
            // acknowledgement told the old master so.
            for index in crate::handler::registry::indexes() {
                crate::repl::slave::converge_public(&index.name);
            }
        }
        Some(Transition::BecameReplica) => {
            eprintln!("ArcVector: this node is a replication replica");
            crate::repl::master::close();
        }
        None => {}
    }
}
```

Expose the convergence check from `slave.rs` for the promotion path by renaming `fn converge` to `pub fn converge_public` — or add `pub fn converge_public(name: &str) { converge(name) }` beside it, whichever reads better once written.

- [ ] **Step 5: Call `start` from the extension entry point**

In `src/lib.rs`, beside the existing `#[cfg(feature = "migration")] attach::start();`:

```rust
#[cfg(feature = "replication")]
repl::start();
```

- [ ] **Step 6: Build and test every feature combination**

```bash
for f in "" "migration" "replication" "replication,migration,cluster-aware" "persistence"; do
  printf '%-38s ' "[${f:-base}]"
  OUT=$({ if [ -z "$f" ]; then cargo clippy --all-targets 2>&1; else cargo clippy --features "$f" --all-targets 2>&1; fi; } \
    | grep -E "^(error|warning):" | grep -v unfulfilled)
  [ -z "$OUT" ] && echo clean || { echo; echo "$OUT" | head -5; }
done
cargo test 2>&1 | grep "^test result"
cargo test --features replication,migration,cluster-aware 2>&1 | grep "^test result"
```

Expected: `clean` for all five, `ok` for both test runs.

- [ ] **Step 7: Commit**

```bash
git add src/repl/mod.rs src/repl/role.rs src/lib.rs
git commit -m "feat: start replication and act on role changes

A promoted node checks every index against its Map before serving: it may
have been behind when the role changed, and nothing told the old master."
```

---

### Task 9: Stop reading metadata on every command

The per-command `AV META` read was doing four jobs, and the stream takes over the one that justified its cost.

**Files:**
- Modify: `src/handler/access/mod.rs:17-45` (`resolve`, the `cfg(recovery)` one)

**Interfaces:**
- Consumes: `registry::get`, `usable_metadata`.
- Produces: no signature change. `resolve` keeps `fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>>`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/handler/access/mod.rs` (create one if absent):

```rust
#[cfg(test)]
mod resolve_tests {
    #[test]
    fn a_registered_index_resolves_without_reading_metadata() {
        // `resolve` needs a live engine, so this asserts the shape rather than
        // the behaviour: the metadata read must sit inside the registry-miss
        // arm, not above the match.
        let source = include_str!("mod.rs");
        let body = source
            .split_once("pub(super) fn resolve")
            .expect("resolve is here")
            .1;
        let head = body.split_once("match registry::get(name)").expect("the match").0;
        assert!(
            !head.contains("usable_metadata"),
            "metadata is read on first touch only, not before every lookup"
        );
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test --features replication resolve_tests 2>&1 | tail -20
```

Expected: FAIL — `usable_metadata` is still called above the match.

- [ ] **Step 3: Move the read into the registry-miss arm**

Rewrite the `cfg(recovery)` `resolve` so the metadata read happens only when the registry misses:

```rust
#[cfg(recovery)]
pub(super) fn resolve(store: &Store, name: &str) -> Result<Arc<VectorIndex>> {
    let stamp = registry::now();

    // Metadata is read on first touch only. What it used to catch on every
    // command -- another node's graph having written this Map -- the
    // replication stream now reports directly, and `access::sweep` reconciles
    // what the stream misses.
    let (index, fresh, owner) = match registry::get(name) {
        Some(index) if index.state() == registry::BUILDING => return Err(Error::NoSuchIndex),
        Some(index) => (index, false, None),

        None => {
            let (meta, layout) = usable_metadata(store, name, stamp)?;
            // Taking an index over begins by writing the ownership token. On a
            // replica `stamp` sees the cached replication role and returns
            // without writing, so the replica can build the graph and serve
            // reads.
            recovery::stamp(store, name, crate::owner::NOBODY)?;
            let owner = meta.owner.clone();
            let (index, fresh) = empty_graph(store, name, &meta, layout)?;
            (index, fresh, Some(owner))
        }
    };

    if fresh || owner.is_some_and(|o| o != index.stamped_as()) {
        recovery::ensure_builder();
        recovery::drain(store, &index)?;
        return Ok(index);
    }

    if index.state() == registry::FILLING && index.is_refilled() {
        recovery::claim_refilled(store, &index)?;
    }
    Ok(index)
}
```

Check `empty_graph`'s real return type before writing this — if it returns `Result<(Arc<VectorIndex>, bool)>` the destructuring above is right; adjust if not.

- [ ] **Step 4: Run the whole suite**

```bash
cargo test --features replication 2>&1 | grep "^test result"
cargo test --features persistence 2>&1 | grep "^test result"
cargo test 2>&1 | grep "^test result"
```

Expected: `ok` for all three.

- [ ] **Step 5: Commit**

```bash
git add src/handler/access/mod.rs
git commit -m "perf: read index metadata on first touch, not every command

The stream reports what the per-command owner check used to catch, and
sweep reconciles what the stream misses. A command against a registered
index now makes no extra engine call."
```

---

### Task 10: Prove it on a live pair

**Files:**
- Modify: `tests/replication.rs`

**Interfaces:**
- Consumes: whatever harness `tests/replication.rs` already provides for bringing up a master/replica pair.

- [ ] **Step 1: Read the existing harness**

```bash
sed -n 1,80p tests/replication.rs
cat docs/replication-testing.md
```

Note how a pair is started and how a test addresses each node. The tests below are written against that harness, not a new one.

- [ ] **Step 2: Write the regression test for the defect Task 3 fixed**

```rust
#[test]
fn a_replica_serves_reads_for_a_map_it_holds() {
    let pair = Pair::start();
    pair.master().vcreate("idx", 4).expect("created");
    pair.master().vadd("idx", "a", &[1.0, 0.0, 0.0, 0.0]).expect("stored");
    pair.await_arcus_replication();

    // Before the ping_slave fix this answered SERVER_ERROR: stamp's skip never
    // fired, put_elem was refused under the index's own name, and the error
    // reached resolve.
    let hits = pair.replica().vsim("idx", &[1.0, 0.0, 0.0, 0.0], 1).expect("a replica can read");
    assert_eq!(hits.len(), 1);
}
```

- [ ] **Step 3: Write the convergence and switchover tests**

```rust
#[test]
fn a_replica_graph_converges_after_writes_to_the_master() {
    let pair = Pair::start();
    pair.master().vcreate("idx", 4).expect("created");
    for (id, v) in [("a", [1.0, 0.0, 0.0, 0.0]), ("b", [0.0, 1.0, 0.0, 0.0])] {
        pair.master().vadd("idx", id, &v).expect("stored");
    }
    pair.await_replica_count("idx", 2);
    assert_eq!(pair.replica().vsim("idx", &[1.0, 0.0, 0.0, 0.0], 2).unwrap().len(), 2);
}

#[test]
fn a_promoted_node_serves_without_rebuilding() {
    let pair = Pair::start();
    pair.master().vcreate("idx", 4).expect("created");
    pair.master().vadd("idx", "a", &[1.0, 0.0, 0.0, 0.0]).expect("stored");
    pair.await_replica_count("idx", 1);

    pair.switchover();
    let started = std::time::Instant::now();
    let hits = pair.master().vsim("idx", &[1.0, 0.0, 0.0, 0.0], 1).expect("serves");
    assert_eq!(hits.len(), 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "no rebuild was needed");
}

#[test]
fn a_killed_connection_resynchronises() {
    let pair = Pair::start();
    pair.master().vcreate("idx", 4).expect("created");
    pair.await_replica_count("idx", 0);

    pair.sever_replication_link();
    pair.master().vadd("idx", "a", &[1.0, 0.0, 0.0, 0.0]).expect("stored");
    pair.await_replica_count("idx", 1);
}

#[test]
fn a_demoted_node_does_not_connect_to_itself() {
    let pair = Pair::start();
    pair.switchover();
    // The demoted node's local AV OWNER still holds what it published. It must
    // wait for its successor's value rather than dial its own address.
    pair.await_replica_connected_to(pair.master_addr());
}
```

- [ ] **Step 4: Run the integration suite**

```bash
cargo test --features replication-tests --test replication 2>&1 | tail -30
```

Expected: all pass with no ignores. If the harness lacks `await_replica_count`, `switchover`, `sever_replication_link` or `await_replica_connected_to`, add them beside the helpers it already has — they are test scaffolding, not product code.

- [ ] **Step 5: Commit**

```bash
git add tests/replication.rs
git commit -m "test: index replication on a live pair

Including a regression test for the defect that stopped a replica reading
at all."
```

---

## Self-Review

**Spec coverage**

| Spec section | Task |
|---|---|
| §4.1 `AV OWNER`, stale-address rule | 2 (`is_stale_owner`), 3 (`OWNER_KEY`) |
| §4.2 heartbeat, transitions, promotion self-check | 2, 3, 8 |
| §4.3 replica finds the master | 7 (`master_addr`) |
| §4.4 ephemeral port, advertised address | 5 (`open`), 8 (`set_listen_addr`) |
| §4.5 per-command metadata read removed | 9 |
| §4.6 `ping_slave` defect | 3 |
| §5 transport, threads, timeouts | 5 |
| §6 four messages, no sequence numbers | 4 |
| §7 convergent apply, the two invariants | 7 (`Pending`, inline apply in `session`) |
| §8 queue as the lag signal, master-side stats | 5 (`Replica`, `queued`, `full_events`, `resyncs_sent`) |
| §9 failure handling | 5 (write timeout, reconnect), 7 (defer patience, resync) |
| §10 module structure | all |
| §12 testing | 2, 4, 5, 7, 9, 10 |

**§4.4's advertised address was missing on the first pass** — Task 5 binds `0.0.0.0:0` and publishing `local_addr` verbatim would hand replicas a wildcard no peer can dial. Fixed inline: Task 8 Steps 1–2 add `role::advertised` with the spec's fallback chain and its unit tests, and `start` publishes through it.

**Known gaps left deliberately**

- §11's deferred graph transfer is not built; `accepts_graph: false` reserves it (Task 4).
- §13's multi-group limit stands: `probe_once` folds `ENGINE_NOT_MY_KEY` into `Probe::Refused`, so such a node simply does not replicate.

**Type consistency** — `Op` is `wire::Op` throughout and re-exported as `crate::repl::Op` (Task 6). `Msg` variants match between `encode`, `decode` and `apply`. `Replica::offer` returns `bool` in both its tests and `master::publish`. `Pending::defer` takes owned `String`s in both its tests and `apply`.

**Two places where the plan defers to the code** — Task 7 Step 4 and Task 9 Step 3 tell the implementer to check real signatures (`hold_addr`, `take_addr`, `with_vector_at`, `add_unless_known`, `empty_graph`) before writing. These are existing APIs the plan quotes from memory of their declarations, not their bodies.
