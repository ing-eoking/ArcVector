# 아이템 트리거 복제 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ArcVector의 벡터 저장을 arcus Map 원소에서 KV 아이템으로 옮기고, 복제·복구·클라이언트 쓰기를 모두 `ON_ITEM_TRIGER` 콜백 하나로 수렴시켜 델타 TCP 채널과 그 부속 스레드 전부를 제거한다.

**Architecture:** 벡터 하나가 `arcus_trig{index}:<id>` 키의 KV 아이템 하나가 된다. arcus가 그 아이템을 복제하거나 persistence 로그에서 복구하면 `do_item_link`가 ArcVector의 콜백을 부르고, 콜백이 `cache_lock`을 쥔 채 그 자리에서 usearch에 넣는다. 그래프는 지금처럼 아이템 포인터를 들고 refcount로 지키며, 해제만 sweeper로 미뤄 잠금 순서를 한 방향으로 유지한다.

**Tech Stack:** Rust 2024 (cdylib 확장), usearch, arcus-memcached-EE (C), Docker 기반 통합 테스트.

**Spec:** [docs/superpowers/specs/2026-09-14-arcvector-item-trigger-design.md](../specs/2026-09-14-arcvector-item-trigger-design.md)

## Global Constraints

- 키 접두사는 `arcus_trig`이고 그 다음 바이트가 반드시 `{`다. C 훅과 Rust 파서가 같은 규칙을 본다.
- 벡터 키는 `arcus_trig{<index>}:<id>`, 인덱스 메타 키는 `arcus_trig{<index>}:` (id가 빈 문자열).
- 인덱스 이름은 1~238바이트, `{` `}` `:` 공백 금지. (`PREFIX_MAX_LENGTH` 250 − `arcus_trig{}` 12)
- 콜백 안에서 부를 수 있는 엔진 함수는 `get_item_info` 하나뿐이다. 나머지는 전부 `cache_lock`을 잡으므로 데드락이다.
- 그래프 락을 쥔 채 `release`를 부르지 않는다. 해제는 항상 sweeper로 미룬다.
- 메모리 부족으로 데몬을 죽이지 않는다. 로그를 남기고 진행한다.
- 배포 전제: `ARCUS_ENABLE_SHARD_KEY=1`, 서버가 `ENABLE_REPLICATION`으로 빌드됨.

## 명령어

| 무엇 | 명령 |
|---|---|
| 단위 테스트 | `cargo test --lib` |
| 특정 테스트 | `cargo test --lib <name> -- --nocapture` |
| 린트 (필수) | `cargo fmt --check && cargo clippy --all-targets --features integration,replication-tests,migration,cluster-aware,persistence -- -D warnings` |
| 통합 테스트 | `make test` (도커. arcus 서버가 필요하다) |

`make lint`가 `LINT_FEATURES` 전부로 clippy를 돌리므로, 기능 게이트 뒤의 코드도 여기서 잡힌다. **모든 Task는 `cargo test --lib`와 `make lint`가 통과해야 끝난다.**

## 파일 구조

| 파일 | 책임 |
|---|---|
| `src/repl/trigger/key.rs` (신규) | 키 만들기·파싱. 엔진도 스레드도 안 쓰는 순수 모듈 |
| `src/repl/trigger/mod.rs` (신규) | 콜백 등록과 이벤트 적용 |
| `src/handler/arcus/engine/kv.rs` | KV 원시 연산. `delete_kv`·`hold_kv`·`release_items`·`flush_prefix`·`with_item_at` |
| `src/handler/arcus/engine/mod.rs` | `ItemElements` — 그래프가 아이템에 닿는 유일한 통로 |
| `src/handler/arcus/element.rs` | 값 레이아웃. `owner` 제거, `cfg(recovery)` 분기 제거 |
| `src/handler/cmd/{index,vector,search}.rs` | 명령이 KV를 쓰고 지운다. 그래프는 만지지 않는다 |
| `src/handler/access/sweep.rs` | 버려진 그래프 free + 해제 목록 드레인 |
| 삭제 | `src/repl/{master,slave,wire,role}.rs`, `src/attach.rs`, `src/owner.rs`, `src/handler/recovery.rs`, `src/handler/arcus/engine/{map,elem}.rs` |

---

## 전제조건 — arcus-memcached-EE 쪽 (이 계획의 범위 밖)

**이 계획은 ArcVector만 고친다.** 서버 쪽은 따로 작업되며, 아래가 그 계약이다. 하나라도
어긋나면 Task 4 이후의 통합 테스트가 실패하고, 그때 봐야 할 곳이 여기다.

| 요구 | 어디 | 지금 상태 |
|---|---|---|
| `ON_ITEM_TRIGER = 5`가 `ENGINE_EVENT_TYPE`에 있고 `MAX_ENGINE_EVENT_TYPE`이 6 | `include/memcached/callback.h` | 되어 있음 |
| `do_item_link`가 `arcus_trig` + `{`로 시작하는 키에 대해 콜백을 쏜다 | `engines/default/item_base.c` | 훅은 있으나 `RP_ROLE_MODE == RP_MODE_SLAVE` 게이트가 남아 있고 `key[10] == '{'` 가드가 없다 |
| **link 훅이 `ITEM_REFCOUNT_INCR(it)`로 참조를 확장에 넘긴다** | 같은 곳 | 아직 없음 |
| `do_item_unlink`가 같은 조건에 `event_data = NULL`로 콜백을 쏜다 | 같은 곳 | 되어 있음 (`{` 가드만 없음) |
| `mc_engine.v1`이 `init_engine` **전에** 채워진다 | `memcached.c` | 적용됨 |

세 번째 줄이 이 계획에서 가장 중요하다. 그래프는 아이템 포인터를 들고 락 없이 읽으므로,
link 시점에 refcount가 올라가 있지 않으면 **검색이 해제된 메모리를 읽는다.** 서버 쪽에
그것이 들어가기 전에는 Task 5 이후를 실서버에 붙이지 않는다.

게이트(`RP_ROLE_MODE == RP_MODE_SLAVE`)가 남아 있는 동안에는 마스터의 자기 쓰기와
persistence 복구에서 트리거가 안 뜨므로, Task 5의 통합 테스트가 `count=0`으로 실패한다.
그 실패는 ArcVector의 버그가 아니라 이 표의 두 번째 줄이다.

---

## Task 1: 트리거 키 코덱

**Files:**
- Create: `src/repl/trigger/key.rs`
- Modify: `src/repl/mod.rs` (모듈 선언만)

**Interfaces:**
- Consumes: 없음
- Produces:
  - `pub const PREFIX: &str = "arcus_trig"`
  - `pub const MAX_INDEX_NAME: usize = 238`
  - `pub fn valid_index_name(name: &str) -> bool`
  - `pub fn vector_key(index: &str, id: &str) -> String`
  - `pub fn meta_key(index: &str) -> String`
  - `pub fn index_prefix(index: &str) -> String`
  - `pub struct Parsed<'a> { pub index: &'a str, pub id: &'a str }`
  - `pub fn parse(key: &[u8]) -> Option<Parsed<'_>>` — `id`가 빈 문자열이면 메타 키다

엔진도 스레드도 쓰지 않는 순수 모듈이므로 형식 전체가 여기서 단위 테스트된다. `wire.rs`가 그랬던 것과 같은 자리다.

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`src/repl/trigger/key.rs` 맨 아래에:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_key_round_trips() {
        let key = vector_key("photos", "v42");
        assert_eq!(key, "arcus_trig{photos}:v42");

        let parsed = parse(key.as_bytes()).expect("parses");
        assert_eq!(parsed.index, "photos");
        assert_eq!(parsed.id, "v42");
    }

    #[test]
    fn a_meta_key_parses_with_an_empty_id() {
        let key = meta_key("photos");
        assert_eq!(key, "arcus_trig{photos}:");

        let parsed = parse(key.as_bytes()).expect("parses");
        assert_eq!(parsed.index, "photos");
        assert_eq!(parsed.id, "");
    }

    #[test]
    fn an_id_may_contain_colons() {
        let parsed = parse(b"arcus_trig{ix}:a:b:c").expect("parses");
        assert_eq!(parsed.index, "ix");
        assert_eq!(parsed.id, "a:b:c");
    }

    #[test]
    fn keys_that_are_not_ours_are_refused() {
        // The C hook compares ten bytes and then one brace; the parser must
        // refuse everything that comparison lets through but we do not own.
        assert!(parse(b"arcus_trigger:foo").is_none());
        assert!(parse(b"arcus_trig").is_none());
        assert!(parse(b"arcus_trig{").is_none());
        assert!(parse(b"arcus_trig{ix}").is_none(), "no delimiter after the name");
        assert!(parse(b"arcus_trig{}:v1").is_none(), "empty index name");
        assert!(parse(b"arcus_trig{ix}x:v1").is_none(), "brace not followed by ':'");
        assert!(parse("arcus_trig{ix}:\u{feff}".as_bytes()).is_some(), "utf8 ids are fine");
        assert!(parse(&[b'a'; 3]).is_none());
    }

    #[test]
    fn a_key_that_is_not_utf8_is_refused() {
        assert!(parse(b"arcus_trig{\xff\xfe}:v1").is_none());
    }

    #[test]
    fn the_index_prefix_is_the_key_up_to_the_delimiter() {
        assert_eq!(index_prefix("photos"), "arcus_trig{photos}");
        // The prefix must be what the daemon derives from a vector key, or a
        // flush aimed at it matches nothing.
        let key = vector_key("photos", "v1");
        let derived = &key[..key.find(':').expect("has a delimiter")];
        assert_eq!(derived, index_prefix("photos"));
    }

    #[test]
    fn index_names_are_bounded_and_have_no_reserved_characters() {
        assert!(valid_index_name("photos"));
        assert!(valid_index_name(&"a".repeat(MAX_INDEX_NAME)));
        assert!(!valid_index_name(&"a".repeat(MAX_INDEX_NAME + 1)));
        assert!(!valid_index_name(""));
        assert!(!valid_index_name("a{b"));
        assert!(!valid_index_name("a}b"));
        assert!(!valid_index_name("a:b"));
        assert!(!valid_index_name("a b"));
    }
}
```

- [ ] **Step 2: 테스트가 실패하는 것을 본다**

```bash
cd /Users/yeoncheol/Github/ArcVector
cargo test --lib repl::trigger::key
```

Expected: 컴파일 실패 — `module 'trigger' not found` 또는 `parse not found`.

- [ ] **Step 3: 구현을 쓴다**

`src/repl/trigger/key.rs`의 테스트 위에:

```rust
//! The one place the trigger key's shape is written down.
//!
//! A key with no engine and no thread behind it, so the whole format is unit
//! tested here rather than inferred from a running server.

/// What the C hook compares. See `do_item_link` in `engines/default/item_base.c`.
pub const PREFIX: &str = "arcus_trig";

/// `PREFIX_MAX_LENGTH` (250) less `arcus_trig{}` (12). A longer name makes the
/// daemon refuse the key outright with `ENGINE_PREFIX_ENAME`, because the
/// prefix it derives is the whole `arcus_trig{name}`.
pub const MAX_INDEX_NAME: usize = 238;

/// Rejects names the key shape cannot carry. `{` and `}` would end the name
/// early, `:` would be read as the delimiter, and a space makes the derived
/// prefix name invalid (`mc_isvalidname`), which the daemon refuses.
pub fn valid_index_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_INDEX_NAME
        && !name.contains(['{', '}', ':', ' '])
}

pub fn vector_key(index: &str, id: &str) -> String {
    format!("{PREFIX}{{{index}}}:{id}")
}

pub fn meta_key(index: &str) -> String {
    vector_key(index, "")
}

/// The prefix the daemon derives from any of this index's keys -- everything
/// before the first delimiter. `flush` takes this, and `stats prefix` reports
/// under it.
pub fn index_prefix(index: &str) -> String {
    format!("{PREFIX}{{{index}}}")
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub index: &'a str,
    /// Empty for the index's metadata key.
    pub id: &'a str,
}

/// Splits a key the hook handed us. `None` for anything that is not ours --
/// the hook's ten-byte compare lets `arcus_trigger:...` through, so this is
/// the check that actually decides.
pub fn parse(key: &[u8]) -> Option<Parsed<'_>> {
    let key = std::str::from_utf8(key).ok()?;
    let rest = key.strip_prefix(PREFIX)?.strip_prefix('{')?;
    let close = rest.find('}')?;
    let (index, after) = rest.split_at(close);
    if index.is_empty() {
        return None;
    }
    let id = after.strip_prefix("}:")?;
    Some(Parsed { index, id })
}
```

`src/repl/mod.rs` 맨 위에 모듈을 단다. 기존 `pub mod master;` 줄들 옆에:

```rust
pub mod trigger;
```

그리고 `src/repl/trigger/mod.rs`를 만들어 한 줄만 둔다 (Task 5가 채운다):

```rust
pub mod key;
```

- [ ] **Step 4: 테스트가 통과하는 것을 본다**

```bash
cargo test --lib repl::trigger::key
```

Expected: 7개 전부 PASS.

- [ ] **Step 5: 린트하고 커밋**

```bash
cargo fmt
make lint
git add src/repl/trigger/ src/repl/mod.rs
git commit -m "feat: the trigger key's shape, in one tested module"
```

---

## Task 2: KV 원시 연산

**Files:**
- Modify: `src/handler/arcus/engine/kv.rs`
- Test: 같은 파일의 `#[cfg(test)]` 없음 — 엔진이 있어야 하므로 Task 4의 통합 테스트가 덮는다

**Interfaces:**
- Consumes: `key::vector_key`, `key::index_prefix` (Task 1)
- Produces: `Store`의 메서드 다섯
  - `pub fn delete_kv(&self, key: &str) -> Result<()>`
  - `pub fn hold_kv(&self, key: &str) -> Result<u64>` — 참조를 쥔 채 아이템 포인터를 `u64`로 돌려준다. 호출자가 `release_items`로 놓아줄 책임을 진다
  - `pub fn release_items(&self, addrs: &[u64])`
  - `pub fn flush_prefix(&self, prefix: &str) -> Result<()>`
  - `pub fn with_item_at<T>(&self, addr: u64, f: impl FnOnce(&[u8], &[u8]) -> T) -> Option<T>` — `f(key, value)`. **락을 잡지 않는다**

`with_item_at`이 콜백 안에서 쓸 수 있는 유일한 읽기다. `get_item_info`가 순수 필드 읽기라서 그렇다 (`default_engine.c`의 `get_item_info`).

- [ ] **Step 1: `with_item_at`을 쓴다**

`src/handler/arcus/engine/kv.rs`의 `impl Store` 안에 추가한다.

```rust
    /// Reads an item's key and value in place, holding no lock.
    ///
    /// `get_item_info` is a pure field read in the default engine -- it takes
    /// no cache lock -- which is what makes this callable from inside the
    /// `ON_ITEM_TRIGER` callback, where the cache lock is already held and
    /// every other engine call would deadlock.
    ///
    /// The caller must already hold a reference to the item (either the one
    /// the hook handed over, or one from `hold_kv`); nothing here takes one.
    pub fn with_item_at<T>(&self, addr: u64, f: impl FnOnce(&[u8], &[u8]) -> T) -> Option<T> {
        let info_of = self.vtable().get_item_info?;
        let it = addr as *mut item;
        if it.is_null() {
            return None;
        }

        let mut info: item_info = unsafe { std::mem::zeroed() };
        if !unsafe { info_of(self.handle(), self.cookie, it, ptr::from_mut(&mut info)) } {
            return None;
        }
        if info.key.is_null() || info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        let key = unsafe { std::slice::from_raw_parts(info.key.cast::<u8>(), info.nkey as usize) };
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };
        Some(f(key, value))
    }
```

- [ ] **Step 2: `hold_kv`와 `release_items`를 쓴다**

```rust
    /// Fetches a key and **keeps** the reference `get` took.
    ///
    /// The returned address is the item pointer. The graph stores it and reads
    /// through it without a lock; the reference is what makes that sound. Hand
    /// it back with `release_items` -- never from a path that holds the graph
    /// lock, because `release` takes the cache lock.
    pub fn hold_kv(&self, key: &str) -> Result<u64> {
        let vt = self.vtable();
        let Some(get) = vt.get else {
            return Err(StoreError::Unavailable);
        };

        let mut it: *mut item = ptr::null_mut();
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                ptr::from_mut(&mut it),
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                0, // vbucket
            )
        };
        check(code)?;
        if it.is_null() {
            return Err(StoreError::KeyGone);
        }
        Ok(it as u64)
    }

    /// Gives back references taken by `hold_kv`, or handed over by the trigger
    /// hook.
    ///
    /// Takes the cache lock, so this must never run while the graph lock is
    /// held -- see the sweeper, which is the only caller that matters.
    pub fn release_items(&self, addrs: &[u64]) {
        let Some(release) = self.vtable().release else {
            return;
        };
        for addr in addrs {
            let it = *addr as *mut item;
            if !it.is_null() {
                unsafe { release(self.handle(), self.cookie, it) };
            }
        }
    }
```

- [ ] **Step 3: `delete_kv`와 `flush_prefix`를 쓴다**

```rust
    /// Removes a key. `KeyGone` when it was not there.
    pub fn delete_kv(&self, key: &str) -> Result<()> {
        let Some(remove) = self.vtable().remove else {
            return Err(StoreError::Unavailable);
        };
        let code = unsafe {
            remove(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                0, // cas
                0, // vbucket
            )
        };
        check(code)
    }

    /// Invalidates every key under `prefix`, now.
    ///
    /// The daemon sets the prefix's `oldest_live` and unlinks part of the LRU
    /// eagerly, leaving the rest to expire on access -- so this is not a
    /// promise that every unlink callback has already fired. `vdrop` therefore
    /// deletes the metadata key first, which releases the graph outright, and
    /// lets the late callbacks land on an index that is no longer registered.
    pub fn flush_prefix(&self, prefix: &str) -> Result<()> {
        let Some(flush) = self.vtable().flush else {
            return Err(StoreError::Unavailable);
        };
        let code = unsafe {
            flush(
                self.handle(),
                self.cookie,
                prefix.as_ptr().cast::<std::os::raw::c_char>(),
                as_int(prefix.len()),
                0, // when: immediately
            )
        };
        check(code)
    }
```

`remove`와 `flush`의 정확한 인자 순서는 `bindings/replication+cluster-aware.rs`의 `engine_interface_v1`에서 확인한다. 시그니처가 다르면 그 파일을 따른다 — 여기 적힌 것이 아니라.

```bash
grep -n "pub remove:" -A12 bindings/replication+cluster-aware.rs
grep -n "pub flush:" -A10 bindings/replication+cluster-aware.rs
```

- [ ] **Step 4: 컴파일과 린트**

```bash
cargo build
make lint
```

Expected: 통과. `kv.rs` 상단의 `use` 목록에 `item`이 이미 있으므로 추가할 것은 없다.

- [ ] **Step 5: 커밋**

```bash
git add src/handler/arcus/engine/kv.rs
git commit -m "feat: KV primitives the trigger path needs"
```

---

## Task 3: 델타 채널과 그 부속을 걷어낸다

**Files:**
- Delete: `src/repl/master.rs`, `src/repl/slave.rs`, `src/repl/wire.rs`, `src/repl/role.rs`, `src/attach.rs`, `src/owner.rs`, `src/handler/recovery.rs`
- Modify: `src/repl/mod.rs`, `src/lib.rs`, `src/handler/mod.rs`, `src/handler/cmd/{index,vector}.rs`, `src/handler/access/{mod,sweep}.rs`, `src/handler/registry.rs`, `src/handler/arcus/element.rs`, `src/handler/arcus/engine/mod.rs`, `build.rs`, `Cargo.toml`
- Delete: `tests/replication.rs`

**Interfaces:**
- Consumes: 없음
- Produces: `repl::start()`가 사라지고, `repl::publish()`가 사라지고, `MetaRecord`에서 `owner` 필드가 사라지고, `Layout::element_len`의 `cfg(recovery)` 분기가 사라진다 (벡터가 항상 값 안에 들어간다).

이 Task가 끝나면 남는 것은 **Map 기반 단일 노드 ArcVector**다. 복제도 재구축도 없다. Task 4가 저장을 KV로 옮기고, Task 5가 복제를 트리거로 되살린다.

- [ ] **Step 1: 지운다**

```bash
cd /Users/yeoncheol/Github/ArcVector
git rm src/repl/master.rs src/repl/slave.rs src/repl/wire.rs src/repl/role.rs
git rm src/attach.rs src/owner.rs src/handler/recovery.rs tests/replication.rs
```

- [ ] **Step 2: `src/repl/mod.rs`를 비운다**

내용을 통째로 이것으로 바꾼다.

```rust
//! Index replication, carried by arcus's own item replication.
//!
//! There is no channel of our own any more. A vector is one KV item whose key
//! the daemon recognises, so replicating the item *is* the notification -- see
//! `trigger`.

pub mod trigger;
```

- [ ] **Step 3: `src/lib.rs`에서 호출부를 뺀다**

- `#[cfg(parked_cookie)] pub mod attach;` 줄과 그 위의 긴 주석 블록을 지운다.
- `pub mod owner;` 줄을 지운다.
- `eprintln!("ArcVector: owner {}", owner::install());` 줄을 지운다.
- `memcached_extensions_initialize` 끝의 `#[cfg(parked_cookie)] attach::start();`와 `#[cfg(feature = "replication")] repl::start();` 두 줄을 지운다.

- [ ] **Step 4: 호출부를 따라가며 지운다**

```bash
cargo build 2>&1 | head -60
```

에러가 가리키는 곳을 순서대로 고친다. 예상되는 것:

| 위치 | 조치 |
|---|---|
| `src/handler/cmd/index.rs:37` | `let owner = crate::owner::ours();` 및 그것을 쓰는 `MetaRecord { owner, .. }` 필드를 지운다 |
| `src/handler/cmd/index.rs:72,140` | `crate::repl::publish(..)` 두 줄과 그 `#[cfg]` 속성을 지운다 |
| `src/handler/cmd/vector.rs:56,62,266` | 같은 방식으로 `repl::publish` 세 줄을 지운다 |
| `src/handler/mod.rs:37,40,49` | `attach::is_ours` / `attach::adopt`를 쓰는 `vowner` 분기 전체를 지운다 |
| `src/handler/access/mod.rs:37` | `recovery::stamp(..)` 호출과 그 `#[cfg(recovery)]` 분기를 지운다 |
| `src/handler/registry.rs` | `stamped_as` · `mark_owned_by` · `mark_ours` · `is_ours`와 `owner` 필드를 지운다 |
| `src/handler/arcus/engine/mod.rs` | `background_cookie` 두 갈래를 `ptr::null()` 하나로 합치고, `background_keyed`를 지운다 |

- [ ] **Step 5: `MetaRecord`에서 `owner`를 뺀다**

`src/handler/arcus/element.rs`:
- `pub owner: String,` 필드를 지운다.
- `encode`의 포맷 문자열에서 `,"owner":{}`와 그 인자를 지운다.
- `decode`의 `let owner = ...` 블록과 구조체 리터럴의 `owner,`를 지운다.

같은 파일에서 `cfg(recovery)` 분기도 없앤다 — 벡터는 이제 항상 값 안에 있다.

```rust
    pub const fn element_len(&self) -> usize {
        Self::VECTOR_OFFSET + self.vector_bytes()
    }

    pub fn vector_of<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        let need = self.element_len();
        (buf.len() >= need).then(|| &buf[Self::VECTOR_OFFSET..need])
    }
```

`Element<'a>`의 `#[cfg(recovery)] pub vector: &'a [u8],`에서 속성만 떼고 필드는 남긴다. `decode`의 `#[cfg(recovery)] vector: ..`도 마찬가지.

- [ ] **Step 6: `cfg`를 정리한다**

`build.rs`에서 `recovery`와 `parked_cookie`를 내보내는 부분을 지운다.

```bash
grep -n "recovery\|parked_cookie" build.rs
```

나온 줄들을 지우고, 크레이트 전체에서 남은 것이 없는지 확인한다.

```bash
grep -rn "cfg(recovery)\|cfg(not(recovery))\|parked_cookie" src/ build.rs
```

Expected: 출력 없음.

`Cargo.toml`에서 `replication-tests` 기능과 `[[test]]` 블록을 지운다. `Makefile`의 `LINT_FEATURES`에서도 `replication-tests,`를 뺀다.

- [ ] **Step 7: 남은 테스트를 고친다**

`cargo test --lib` 컴파일 에러가 나오는 단위 테스트는 사라진 기능(소유권 스탬프, 재구축)을 보던 것들이다. 지운다. **기능을 살리려고 테스트를 고치지 않는다** — 이 Task는 그 기능을 없애는 것이다.

```bash
cargo test --lib 2>&1 | head -40
```

- [ ] **Step 8: 통과하는 것을 본다**

```bash
cargo test --lib
make lint
```

Expected: 전부 PASS, clippy 경고 0.

- [ ] **Step 9: 통합 테스트**

```bash
make test
```

Expected: 재구축을 보던 테스트를 뺀 나머지가 전부 PASS. 실패하는 것이 있으면 그것이 재구축에 의존하는 테스트인지 확인하고, 맞으면 지운다.

- [ ] **Step 10: 커밋**

```bash
git add -A
git commit -m "refactor: remove the delta channel, the parked connection and rebuild

The trigger callback replaces all three. This commit only takes them out;
the trigger itself arrives with the KV storage it needs."
```

---

## Task 4: 저장을 Map에서 KV로 옮긴다

**Files:**
- Modify: `src/handler/arcus/engine/mod.rs`, `src/handler/cmd/index.rs`, `src/handler/cmd/vector.rs`, `src/handler/cmd/search.rs`, `src/handler/access/mod.rs`, `src/handler/access/meta.rs`
- Delete: `src/handler/arcus/engine/map.rs`, `src/handler/arcus/engine/elem.rs`
- Test: `tests/integration.rs` (기존 테스트가 그대로 통과해야 한다)

**Interfaces:**
- Consumes: Task 1의 `key::*`, Task 2의 `Store::{delete_kv, hold_kv, release_items, flush_prefix, with_item_at, get_kv, set_kv}`
- Produces:
  - `pub struct ItemElements;` — `crate::handler::usearch::Elements` 구현. `DetachedElements`를 대체한다
  - `Store::read_meta(&self, index: &str) -> Result<(MetaRecord, Layout)>`
  - `Store::write_meta(&self, index: &str, meta: &MetaRecord, layout: Layout) -> Result<()>`

이 Task는 크다. 명령의 겉모습(`vcreate`/`vadd`/`vdel`/`vdrop`/`vsearch`/`vgetattr`의 응답)이 하나도 바뀌지 않는 것이 성공 기준이고, 그래서 **기존 통합 테스트가 그대로 판정한다.**

그래프 삽입은 아직 명령 핸들러가 한다. Task 5가 그것을 콜백으로 옮긴다.

- [ ] **Step 1: `ItemElements`를 쓴다**

`src/handler/arcus/engine/mod.rs`의 `DetachedElements`를 통째로 이것으로 바꾼다.

```rust
/// How the graph reaches the item behind an address.
///
/// The graph holds raw item pointers and reads through them with no lock,
/// which the reference taken at link time makes sound. Releasing is the only
/// engine call here that takes the cache lock, and it is reached exclusively
/// from the sweeper -- never while the graph lock is held.
pub struct ItemElements;

impl crate::handler::usearch::Elements for ItemElements {
    fn id_at(&self, addr: u64) -> Option<std::sync::Arc<str>> {
        let store = Store::background()?;
        store.with_item_at(addr, |key, _value| {
            crate::repl::trigger::key::parse(key).map(|p| std::sync::Arc::from(p.id))
        })?
    }

    fn release(&self, addrs: &[u64]) {
        if let Some(store) = Store::background() {
            store.release_items(addrs);
        }
    }
}
```

`pub use elem::{..}` 및 `pub use map::{..}` 줄과 `mod elem; mod map;` 선언을 지운다.

- [ ] **Step 2: 메타 읽기/쓰기를 쓴다**

같은 파일의 `impl Store` 안에:

```rust
    /// Reads an index's metadata key.
    pub fn read_meta(
        &self,
        index: &str,
    ) -> Result<(crate::handler::arcus::element::MetaRecord, crate::handler::arcus::element::Layout), StoreError>
    {
        let raw = self.get_kv(&crate::repl::trigger::key::meta_key(index))?;
        crate::handler::arcus::element::MetaRecord::decode(&raw)
            .map_err(|_| StoreError::CorruptElement)
    }

    /// Writes an index's metadata key, refusing to replace one that is there.
    ///
    /// `vcreate` must answer `EXISTS` rather than silently re-creating, and an
    /// `add` is how the engine answers that question without a read first.
    pub fn add_meta(
        &self,
        index: &str,
        meta: &crate::handler::arcus::element::MetaRecord,
        layout: crate::handler::arcus::element::Layout,
    ) -> Result<(), StoreError> {
        self.add_kv(
            &crate::repl::trigger::key::meta_key(index),
            &meta.encode(layout),
        )
    }
```

`add_kv`는 `set_kv`를 복사해 `ENGINE_STORE_OPERATION_OPERATION_SET`을 `..._ADD`로 바꾼 것이다. `bindings/`에서 정확한 상수 이름을 확인한다.

```bash
grep -n "ENGINE_STORE_OPERATION_OPERATION_ADD" bindings/replication+cluster-aware.rs
```

- [ ] **Step 3: `vcreate`/`vdrop`을 옮긴다**

`src/handler/cmd/index.rs`:

`vcreate`에서 `store.probe_map` · `alloc_elem` · `insert_creating` 경로를 지우고 이렇게 만든다.

```rust
    if !crate::repl::trigger::key::valid_index_name(name) {
        return Err(Error::bad_request(format!(
            "index name must be 1..={} bytes and contain none of '{{', '}}', ':', ' '",
            crate::repl::trigger::key::MAX_INDEX_NAME
        )));
    }
    match store.add_meta(name, &meta, layout) {
        Ok(()) => { /* fall through to registering the graph */ }
        Err(StoreError::ElemExists) => return already_there(store, name),
        Err(e) => return Err(e.into()),
    }
```

`add_kv`가 이미 있는 키에 대해 무엇을 돌려주는지 확인해 `ElemExists`인지 다른 변형인지 맞춘다 (`do_item_store_add`는 `ENGINE_NOT_STORED`를 낸다).

`vdrop`은 이렇게 된다.

```rust
pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    // The metadata key goes first: deleting it releases the graph outright,
    // where the flush only promises that the vectors stop being visible.
    let had_meta = match store.delete_kv(&crate::repl::trigger::key::meta_key(name)) {
        Ok(()) => true,
        Err(StoreError::KeyGone) => false,
        Err(e) => return Err(e.into()),
    };
    let known = registry::remove(name);
    if had_meta {
        if let Err(e) = store.flush_prefix(&crate::repl::trigger::key::index_prefix(name)) {
            eprintln!("ArcVector: '{name}' dropped, but its vectors were not flushed ({e})");
        }
    }

    Ok(if known || had_meta {
        Reply::Dropped
    } else {
        Reply::NotFound
    })
}
```

- [ ] **Step 4: `vadd`/`vdel`을 옮긴다**

`src/handler/cmd/vector.rs`의 `vadd`에서 `alloc_elem`/`PendingElem`/`insert_published` 조합을 이것으로 바꾼다.

```rust
    let staged = index.ann.stage(&quantized)?;

    let mut value = vec![0u8; index.ann.layout.full_stored_len()];
    layout.write(&mut value, &quantized, attr)?;

    let vkey = crate::repl::trigger::key::vector_key(name, id);
    if let Err(e) = store.set_kv(&vkey, &value) {
        index.ann.discard(staged);
        return store_failed(name, stamp, e);
    }

    // The write has committed, so the item exists and this reference is the
    // one the graph keeps. Task 5 moves this to the callback.
    match store.hold_kv(&vkey) {
        Ok(addr) => match index.ann.insert_published(staged, || None, || Ok::<u64, StoreError>(addr)) {
            Ok(_) => Ok(Reply::Stored),
            Err(PublishError::Store(e)) => store_failed(name, stamp, e),
            Err(PublishError::Mapping(e)) => Err(e),
        },
        Err(e) => {
            index.ann.discard(staged);
            store_failed(name, stamp, e)
        }
    }
```

`vdel`은 `take_addr` 대신:

```rust
    let vkey = crate::repl::trigger::key::vector_key(name, id);
    match store.delete_kv(&vkey) {
        Ok(()) => {
            index.ann.remove_published(id, ..);   // 기존 시그니처에 맞춘다
            Ok(Reply::Deleted)
        }
        Err(StoreError::KeyGone) => Ok(Reply::NotFound),
        Err(e) => Err(e.into()),
    }
```

`remove_published`의 현재 시그니처(`src/handler/usearch/index.rs:1008`)를 읽고 그에 맞춘다. 주소를 받는 형태면 `hold_kv`로 주소를 먼저 얻는다.

- [ ] **Step 5: `vsearch`/`vgetattr`을 옮긴다**

`src/handler/cmd/search.rs:57`의 `store.with_attr_at(key, layout, ..)`를 `with_item_at`으로 바꾼다.

```rust
        let Some(passed) = store.with_item_at(key, |_k, value| {
            layout.attr_of(value).ok().is_some_and(|attr| filter.matches(attr))
        }) else {
            return false;
        };
```

`search.rs:195`의 `with_vector_at`도 같은 방식으로 `with_item_at(addr, |_k, value| layout.vector_of(value).map(<[u8]>::to_vec))`가 된다.

`src/handler/access/meta.rs`의 `read_metadata`는 `store.read_meta(name)`을 부르도록 바꾼다. `access/mod.rs`의 `probe_map` 호출부는 인덱스 존재 여부를 메타 키로 판정하도록 바꾼다 (`store.get_kv(meta_key(name))`의 `KeyGone` 여부).

- [ ] **Step 5b: `ItemElements`를 꽂고 크기 예산을 바꾼다**

`DetachedElements`를 만드는 자리 둘을 `ItemElements`로 바꾼다 (`recovery.rs`의 것은 Task 3에서 이미 파일째 사라졌다).

```bash
grep -rn "DetachedElements" src/
```

- `src/handler/registry.rs:452`
- `src/handler/cmd/index.rs:32`

**크기 예산이 바뀐다.** 값이 Map 원소가 아니라 아이템 본문이 되므로 상한이
`max_element_bytes`가 아니라 `max_item_size`다. `Store`에 하나 더한다.

```rust
    /// The largest item body the daemon will store. Replaces
    /// `max_element_bytes` as the budget a vector has to fit in, now that a
    /// vector is an item rather than a Map element.
    pub fn max_item_size(&self) -> u32 {
        self.config_u32(c"max_item_size", DEFAULT_MAX_ITEM_SIZE)
    }
```

`DEFAULT_MAX_ITEM_SIZE`는 `1024 * 1024`다 (arcus의 기본 1MB). 그리고 세 호출부를 바꾼다.

- `src/handler/cmd/index.rs:112` — `store.max_element_bytes()` → `store.max_item_size()`
- `src/handler/cmd/index.rs:118-121` — 에러 문구의 `max_element_bytes`를 `max_item_size`로
- `src/handler/cmd/vector.rs:114,119` — 같은 방식

`Layout::max_dim_for`의 인자 이름도 `max_item_bytes`로 바꾼다. **계산식은 그대로다** —
헤더 + attr + 종단자라는 고정 오버헤드가 달라지지 않는다. `element.rs`의
`max_dim_for_is_the_exact_ceiling` 테스트는 예산을 인자로 받으므로 고칠 것이 없다.

`Store::max_element_bytes`와 `max_map_size`는 호출자가 없어지므로 지운다.

- [ ] **Step 5c: `vlist`의 개수를 그래프에서 읽는다**

`src/handler/cmd/index.rs:202`의 `count=`는 지금 Map의 `getattr` 개수와 그래프 개수를 견주어
내보낸다. 비교 대상이 없어졌으므로 `index.ann.len()` 하나로 답한다. `maxcount=`도 Map의
속성이었으므로 `MetaRecord`에 담긴 값을 쓴다.

기존 통합 테스트의 `attrbytes=128 count=0`, `count=1` 단정이 그대로 통과해야 한다.

- [ ] **Step 5d: 테스트 서버에 샤드키를 켠다**

`{index}`가 샤드키로 해석되려면 `ARCUS_ENABLE_SHARD_KEY=1`이 있어야 한다
([cluster_config.c:668](../../../../arcus-memcached-EE/cluster_config.c)). 단일 노드
테스트에서는 없어도 키가 저장되지만, 배포와 다른 환경에서 테스트하는 셈이 되므로 맞춘다.

- `docker/Dockerfile`의 서버 실행 부분에 `ENV ARCUS_ENABLE_SHARD_KEY=1`
- `tests/common/mod.rs`에서 서버를 `Command`로 띄우는 자리에 `.env("ARCUS_ENABLE_SHARD_KEY", "1")`

```bash
grep -n "Command::new\|\.env(" tests/common/mod.rs | head
```

- [ ] **Step 6: 지운다**

```bash
git rm src/handler/arcus/engine/map.rs src/handler/arcus/engine/elem.rs
cargo build 2>&1 | head -60
```

남은 참조를 따라가며 고친다. `HeldMap`·`HeldAddr`·`PendingElem`·`MapProbe`·`index_attr`·`hold_all`·`probe_map`·`take_elem`·`put_elem`·`delete_elem`·`get_attr`이 전부 사라져야 한다.

- [ ] **Step 7: 단위 테스트와 린트**

```bash
cargo test --lib
make lint
```

Expected: PASS. `usearch/index.rs`의 `FAKE` 테스트들은 `Elements`만 쓰므로 영향이 없어야 한다.

- [ ] **Step 8: 통합 테스트로 판정한다**

```bash
make test
```

Expected: `tests/integration.rs`가 **수정 없이** 전부 통과. 응답 문자열이 하나라도 달라지면 옮기는 과정에서 의미가 바뀐 것이다. 테스트를 고치지 말고 구현을 고친다.

- [ ] **Step 9: 커밋**

```bash
git add -A
git commit -m "refactor: a vector is one KV item, not a Map element

The key is arcus_trig{index}:<id>, which gives each index a prefix of its
own -- vdrop is a delete plus a flush. Commands answer exactly as before."
```

---

## Task 5: 트리거 콜백

**Files:**
- Modify: `src/repl/trigger/mod.rs`, `src/lib.rs`, `src/handler/cmd/vector.rs`, `src/handler/cmd/index.rs`
- Test: `src/repl/trigger/mod.rs`의 `#[cfg(test)]`, 그리고 `tests/integration.rs`에 새 테스트 하나

**Interfaces:**
- Consumes: Task 1의 `key::parse`, Task 2의 `Store::with_item_at`, Task 4의 `ItemElements`
- Produces:
  - `pub fn install()` — `register_callback(NULL, ON_ITEM_TRIGER, ..)`
  - `pub enum Event<'a> { MetaLink { index: &'a str, value: &'a [u8] }, MetaUnlink { index: &'a str }, VectorLink { index: &'a str, id: &'a str, value: &'a [u8] }, VectorUnlink { index: &'a str, id: &'a str } }`
  - `pub fn classify<'a>(key: &'a [u8], value: &'a [u8], linked: bool) -> Option<Event<'a>>`
  - `pub fn apply(event: Event<'_>, addr: u64)`

`classify`가 순수 함수라 단위 테스트가 거기 붙는다. `apply`는 엔진과 레지스트리를 만지므로 통합 테스트가 본다.

- [ ] **Step 1: `classify`의 실패하는 테스트를 쓴다**

`src/repl/trigger/mod.rs` 아래에:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linked_vector_is_an_upsert() {
        let ev = classify(b"arcus_trig{ix}:v1", b"payload", true).expect("classified");
        assert_eq!(
            ev,
            Event::VectorLink { index: "ix", id: "v1", value: b"payload" }
        );
    }

    #[test]
    fn an_unlinked_vector_is_a_delete() {
        let ev = classify(b"arcus_trig{ix}:v1", b"payload", false).expect("classified");
        assert_eq!(ev, Event::VectorUnlink { index: "ix", id: "v1" });
    }

    #[test]
    fn an_empty_id_is_the_index_itself() {
        assert_eq!(
            classify(b"arcus_trig{ix}:", b"{}", true).expect("classified"),
            Event::MetaLink { index: "ix", value: b"{}" }
        );
        assert_eq!(
            classify(b"arcus_trig{ix}:", b"{}", false).expect("classified"),
            Event::MetaUnlink { index: "ix" }
        );
    }

    #[test]
    fn a_key_that_is_not_ours_classifies_as_nothing() {
        assert!(classify(b"arcus_trigger:x", b"", true).is_none());
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

```bash
cargo test --lib repl::trigger
```

Expected: `classify not found`.

- [ ] **Step 3: `classify`와 `Event`를 쓴다**

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    MetaLink { index: &'a str, value: &'a [u8] },
    MetaUnlink { index: &'a str },
    VectorLink { index: &'a str, id: &'a str, value: &'a [u8] },
    VectorUnlink { index: &'a str, id: &'a str },
}

/// Sorts one hook firing into what it means. Pure, so the rules live in one
/// place and are tested without a server.
pub fn classify<'a>(key: &'a [u8], value: &'a [u8], linked: bool) -> Option<Event<'a>> {
    let parsed = key::parse(key)?;
    Some(match (parsed.id.is_empty(), linked) {
        (true, true) => Event::MetaLink { index: parsed.index, value },
        (true, false) => Event::MetaUnlink { index: parsed.index },
        (false, true) => Event::VectorLink { index: parsed.index, id: parsed.id, value },
        (false, false) => Event::VectorUnlink { index: parsed.index, id: parsed.id },
    })
}
```

- [ ] **Step 4: 통과를 확인한다**

```bash
cargo test --lib repl::trigger
```

Expected: 4개 PASS.

- [ ] **Step 5: 콜백과 등록을 쓴다**

같은 파일에:

```rust
use std::os::raw::c_void;

/// `ON_ITEM_TRIGER`. The server's `callback.h` gives it 5; the committed
/// bindings predate the enum member, so it is written out here.
const ON_ITEM_TRIGER: u32 = 5;

/// Registers the callback. Called from `memcached_extensions_initialize`,
/// which runs from the getopt loop -- strictly before the engine initialises,
/// and so before persistence recovery relinks anything.
pub fn install() {
    let server = crate::server::handle();
    if server.is_null() {
        eprintln!("ArcVector: no server API; the item trigger is not installed");
        return;
    }
    let callback = unsafe { (*server).callback };
    if callback.is_null() {
        eprintln!("ArcVector: no callback API; the item trigger is not installed");
        return;
    }
    let Some(register) = (unsafe { (*callback).register_callback }) else {
        eprintln!("ArcVector: register_callback missing; the item trigger is not installed");
        return;
    };
    unsafe {
        register(
            std::ptr::null_mut(),
            ON_ITEM_TRIGER,
            Some(on_item_trigger),
            std::ptr::null(),
        );
    }
}

/// Runs with the engine's `cache_lock` held, on whichever thread linked the
/// item -- a worker thread for a client write, the replication apply thread
/// for a replica, the recovery thread at startup.
///
/// **Only `get_item_info` may be called from here.** Everything else in the
/// engine takes `cache_lock` and would deadlock on the spot.
///
/// On a link the hook has already handed us a reference (`ITEM_REFCOUNT_INCR`
/// in `do_item_link`). Every path out of this function must dispose of it:
/// into the graph, or onto the sweeper's release list.
unsafe extern "C" fn on_item_trigger(
    cookie: *const c_void,
    _type: u32,
    event_data: *const c_void,
    _cb_data: *const c_void,
) {
    let addr = cookie as u64;
    if addr == 0 {
        return;
    }
    let linked = !event_data.is_null();

    let Some(store) = crate::handler::arcus::engine::Store::background() else {
        // No vtable yet. Nothing is registered, so nothing is owed -- except
        // the reference, which we cannot give back either. Say so once.
        if linked {
            eprintln!("ArcVector: item trigger fired before the engine was reachable");
        }
        return;
    };

    let decided = store.with_item_at(addr, |key, value| {
        classify(key, value, linked).map(|ev| apply_owned(ev, addr))
    });

    if linked && !matches!(decided, Some(Some(Kept::ByGraph))) {
        crate::handler::access::sweep::release_later(addr);
    }
}

/// Who owns the reference after `apply`.
enum Kept {
    /// The graph took it; the sweeper will release it when the item unlinks.
    ByGraph,
    /// Nobody took it.
    Nobody,
}
```

- [ ] **Step 6: `apply_owned`를 쓴다**

```rust
fn apply_owned(event: Event<'_>, addr: u64) -> Kept {
    use crate::handler::registry;

    match event {
        Event::MetaLink { index, value } => {
            if let Err(e) = registry::adopt_from_meta(index, value) {
                eprintln!("ArcVector: index '{index}' arrived but could not be built ({e})");
            }
            Kept::Nobody
        }

        Event::MetaUnlink { index } => {
            // Freeing a large HNSW here would stall the daemon: we hold the
            // cache lock. Hand it to the sweeper instead.
            crate::handler::access::sweep::retire(registry::take(index));
            Kept::Nobody
        }

        Event::VectorLink { index, id, value } => {
            let Some(ix) = registry::get(index) else {
                eprintln!("ArcVector: a vector arrived for unknown index '{index}' (id {id})");
                return Kept::Nobody;
            };
            let Some(vector) = ix.ann.layout.vector_of(value) else {
                eprintln!("ArcVector: '{index}' id {id} has no readable vector");
                return Kept::Nobody;
            };
            match ix.ann.add_unless_known(addr, vector) {
                Ok(true) => Kept::ByGraph,
                Ok(false) => Kept::Nobody,
                Err(e) => {
                    eprintln!("ArcVector: '{index}' id {id} was not indexed ({e})");
                    Kept::Nobody
                }
            }
        }

        Event::VectorUnlink { index, id: _ } => {
            if let Some(ix) = registry::get(index) {
                // Drops the address from the graph and queues the release; the
                // graph lock is held in there, so nothing may call the engine.
                ix.ann.forget(&[addr]);
            }
            crate::handler::access::sweep::release_later(addr);
            Kept::Nobody
        }
    }
}
```

`add_unless_known`(`src/handler/usearch/index.rs:857`)과 `forget`(같은 파일 650)의 현재 시그니처를 읽고 정확히 맞춘다. `registry::adopt_from_meta`와 `registry::take`은 새로 만든다 — 전자는 `MetaRecord::decode` → `AnnIndex::new` → `registry::insert_or_get`, 후자는 `registry::remove`가 `Option<Arc<VectorIndex>>`를 돌려주도록 고친 것이다.

- [ ] **Step 7: 등록하고, 핸들러에서 그래프 삽입을 뺀다**

`src/lib.rs`의 `memcached_extensions_initialize` 끝에:

```rust
    repl::trigger::install();
```

그리고 Task 4의 Step 4에서 남겨 둔 `hold_kv` + `insert_published` 블록을 지운다. `vadd`는 이제 `set_kv`까지만 한다 — 그래프는 그 `set_kv` 안에서 콜백이 채운다. `vdel`의 `remove_published`도 지운다. `vcreate`의 레지스트리 등록도 지운다 (메타 KV의 link 콜백이 한다).

- [ ] **Step 7b: `vadd`가 그래프에 들어갔는지 확인하고 못 들어갔으면 되돌린다**

콜백이 메모리 부족으로 벡터를 건너뛰면 화해할 수단이 없으므로 그 벡터는 영구히 그래프에
없다(스펙 §9). 복제·복구 경로에는 갚을 상대가 없지만 **클라이언트 경로에는 있다** —
`vadd`는 자기 id를 알고 응답을 고를 수 있다.

`src/handler/cmd/vector.rs`의 `vadd` 끝에:

```rust
    store.set_kv(&vkey, &value).or_else(|e| store_failed(name, stamp, e))?;

    // The callback ran inside that write, on this thread. If it did not take
    // the vector, nothing will: there is no reconciliation pass any more. Undo
    // the write rather than answering STORED for a vector no search can find.
    if !index.ann.contains_id(id) {
        if let Err(e) = store.delete_kv(&vkey) {
            eprintln!("ArcVector: '{name}' id {id} was not indexed and could not be rolled back ({e})");
        }
        return Err(Error::Index(format!(
            "'{name}' id {id} could not be indexed; the write was rolled back"
        )));
    }
    Ok(Reply::Stored)
```

`AnnIndex::contains_id(&self, id: &str) -> bool`를 새로 만든다 — `HeldSet`의 주소를
`Elements::id_at`으로 되짚어 비교하면 O(n)이라 쓸 수 없으므로, `add_unless_known`이
성공한 주소를 돌려주게 하고 `vadd`가 그 주소를 받아 `HeldSet::contains`로 묻는 편이 낫다.
어느 쪽이든 **O(1)이어야 한다**; 구현하면서 `index.rs`의 기존 구조에 맞는 쪽을 고른다.

`Error::Index`는 `SERVER_ERROR`로 나간다 — `src/error.rs`에서 확인한다.

- [ ] **Step 8: 통합 테스트를 하나 더한다**

`tests/integration.rs`에:

```rust
#[test]
fn the_graph_is_built_by_the_trigger_not_the_handler() {
    session!(_server, client);
    let ix = index_name("trigger");
    assert_reply(
        &client.send(&format!("vcreate {ix} 2 METRIC l2 QUANT f32")),
        "CREATED\r\n",
    );

    // vadd writes a KV and returns; the graph entry exists only if the link
    // callback ran, on this very thread, inside that write.
    assert_reply(&client.vadd(&ix, "v1", 2, "0.1 0.2"), "STORED\r\n");
    assert_contains(&client.send("vlist"), "count=1");

    let hit = client.send(&format!("vsearch {ix} 1 2 0.1 0.2"));
    assert_contains(&hit, "v1");

    // ... and the unlink callback is what takes it back out.
    assert_reply(&client.send(&format!("vdel {ix} v1")), "DELETED\r\n");
    assert_contains(&client.send("vlist"), "count=0");

    client.send(&format!("vdrop {ix}"));
}
```

`client.vadd`와 `vsearch`의 정확한 형태는 `tests/common/mod.rs`와 기존 테스트에서 확인해 맞춘다.

- [ ] **Step 9: 돌린다**

```bash
cargo test --lib
make lint
make test
```

Expected: 전부 PASS. `count=1`이 안 나오면 콜백이 안 뜬 것이다 — 서버가 전제조건의 훅을 가진 빌드인지, `key[10] == '{'`가 맞는지 확인한다.

- [ ] **Step 10: 커밋**

```bash
git add -A
git commit -m "feat: the graph is built by the item trigger

One callback covers a client write, a replica's apply and persistence
recovery, because all three reach do_item_link."
```

---

## Task 6: 지연 해제

**Files:**
- Modify: `src/handler/access/sweep.rs`

**Interfaces:**
- Consumes: Task 2의 `Store::release_items`
- Produces:
  - `pub(crate) fn release_later(addr: u64)` — 엔진을 부르지 않는다. 목록에 넣고 sweeper를 깨운다
  - `pub(crate) fn retire(evicted: Option<Arc<VectorIndex>>)` — 지금 것 그대로

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`src/handler/access/sweep.rs` 아래에:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_queue_up_without_touching_the_engine() {
        // `release_later` runs with the cache lock held, so it must not call
        // the engine -- it may only move the address onto the list.
        let mut state = State::default();
        assert!(state.pending.is_empty());
        state.pending.push(0xdead_beef);
        assert_eq!(state.take_pending(), vec![0xdead_beef]);
        assert!(state.pending.is_empty(), "taking drains the list");
    }

    #[test]
    fn a_list_that_cannot_grow_drops_the_address_rather_than_aborting() {
        let mut state = State::default();
        assert!(state.push_release(1));
        assert_eq!(state.pending.len(), 1);
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

```bash
cargo test --lib access::sweep
```

Expected: `no field 'pending'`.

- [ ] **Step 3: 구현한다**

`State`에 필드를 더한다.

```rust
#[derive(Default)]
struct State {
    retired: Vec<Arc<VectorIndex>>,
    /// Item references waiting to be given back to the engine.
    ///
    /// They arrive from the trigger callback, which holds the cache lock, and
    /// from the graph's own removal path, which holds the graph lock. Neither
    /// may call `release` -- it takes the cache lock -- so the only thing done
    /// on those threads is this push.
    pending: Vec<u64>,
}

impl State {
    fn push_release(&mut self, addr: u64) -> bool {
        if self.pending.try_reserve(1).is_err() {
            return false;
        }
        self.pending.push(addr);
        true
    }

    fn take_pending(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.pending)
    }
}

/// Queues an item reference for release. Calls no engine function, so it is
/// safe from inside the trigger callback and from under the graph lock.
pub(crate) fn release_later(addr: u64) {
    let mut state = state();
    if !state.push_release(addr) {
        // Losing the address leaks one item until the process ends. Saying so
        // is better than aborting on an allocation failure.
        eprintln!("ArcVector: could not queue an item release; one item is pinned");
        return;
    }
    SWEEPER.wake.notify_one();
}
```

`run()`의 루프 안, 지금 `retired`를 비우는 자리 옆에 해제를 더한다.

```rust
        let releasing = state().take_pending();
        if !releasing.is_empty() {
            if let Some(store) = crate::handler::arcus::engine::Store::background() {
                // No graph lock is held here, which is the whole reason this
                // runs on the sweeper rather than where the address came from.
                store.release_items(&releasing);
            }
        }
```

`BATCH`(10)를 `releasing`에도 적용해 한 번에 너무 오래 `cache_lock`을 잡지 않게 한다.

- [ ] **Step 4: 통과를 확인한다**

```bash
cargo test --lib access::sweep
```

Expected: 2개 PASS.

- [ ] **Step 5: `AnnIndex`의 해제 경로를 잇는다**

`ItemElements::release`가 이제 `release_later`로 가야 한다 — `Elements::release`는 그래프 락 아래에서 불리기 때문이다.

```rust
    fn release(&self, addrs: &[u64]) {
        for addr in addrs {
            crate::handler::access::sweep::release_later(*addr);
        }
    }
```

`Store::release_items`의 직접 호출자는 이제 sweeper 하나뿐이어야 한다.

```bash
grep -rn "release_items" src/
```

Expected: 정의 한 줄, sweeper 한 줄. 그 외에 있으면 잠금 순서가 깨진 것이다.

- [ ] **Step 6: 돌리고 커밋**

```bash
cargo test --lib
make lint
make test
git add -A
git commit -m "fix: release item references from the sweeper, never under a lock"
```

---

## Task 7: 문서

**Files:**
- Rewrite: `docs/복제.md`
- Modify: `docs/내부구조.md`, `docs/미해결.md`, `README.md`
- Modify: `docs/superpowers/specs/2026-09-14-arcvector-item-trigger-design.md` (상태를 **채택됨**으로)

- [ ] **Step 1: `docs/복제.md`를 다시 쓴다**

지금 문서는 소켓 세 개·큐·역할 하트비트·지연 집합을 설명하는데 그것이 전부 사라졌다. 스펙의 §2·§4·§5·§6·§7을 문서 어조로 옮긴다. 절 구성:

1. 문제 — 인덱스가 두 벌로 존재한다 (그대로 살린다)
2. 해결 — 아이템이 곧 알림이다
3. 키와 값
4. 콜백 계약과 무엇을 부를 수 없는가
5. 잠금 순서
6. 명령 경로
7. 한계

- [ ] **Step 2: `docs/내부구조.md`에서 사라진 것을 지운다**

```bash
grep -n "Map\|owner\|recovery\|attach\|파킹\|델타\|하트비트" docs/내부구조.md
```

나온 절을 KV·트리거 기준으로 고친다.

- [ ] **Step 3: `docs/미해결.md`를 갱신한다**

스펙 §11의 한계 일곱을 옮긴다. 해결된 것(순서 경쟁, resync, Snapshot)은 지운다.

- [ ] **Step 4: 스펙의 상태를 바꾼다**

`상태: **제안**.` → `상태: **채택됨** (2026-09-14 구현).`

- [ ] **Step 5: 커밋**

```bash
git add -A
git commit -m "docs: rewrite the replication page around the item trigger"
```
