# 회수 배리어 큐 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 아이템 참조 회수를 epoch·리더 슬롯 기반 정지 판정에서 FIFO 배리어 큐로 바꿔, 시각 비교를 없애고 검색이 끊이지 않아도 회수가 진행되게 한다.

**Architecture:** 검색과 retire를 같은 FIFO 큐에 넣으면 "누가 먼저였나"가 큐 순서 그 자체가 된다. retire 사이에 들어온 검색들은 배리어 하나를 공유하고, retire가 그것을 봉인해 큐에 넣는다. sweeper는 큐를 앞에서부터 걸으며 안 풀린 배리어에서 서고, `Release`에 닿으면 그 주소를 들고 있을 수 있는 검색이 없다는 뜻이므로 그냥 놓아준다.

**Tech Stack:** Rust 2024 (cdylib 확장), usearch, arcus-memcached-EE (C), Docker 기반 통합 테스트.

**Spec:** [docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md](../specs/2026-09-16-arcvector-retire-barrier-queue-design.md)

## Global Constraints

- **`held`에서 뺀 다음에 큐에 넣는다.** 이 순서가 설계 전체의 안전성 근거다. 주소를 `held.give_up()`/`take_all()`로 뺀 **뒤에** `retire()`를 부른다.
- **`release`는 sweeper만 부른다.** 그래프 락도 큐 뮤텍스도 cache lock도 쥐지 않은 채여야 한다. 예외는 `Drop for AnnIndex`와 `unclaimed()` 둘뿐이고, 그 둘은 `held`에 들어간 적 없는 주소만 다룬다.
- **unlink 콜백 안에서는 `get_item_info` 말고 어떤 엔진 함수도 부르지 않는다.** 콜백은 `LOCK_CACHE()`를 쥐고 들어온다.
- **메모리 부족으로 데몬을 죽이지 않는다.** `Vec`은 `try_reserve`로 늘리고, 실패하면 로그를 남기고 진행한다. `Arc::new`처럼 실패가 abort인 할당을 retire 경로에 두지 않는다.
- **`AnnIndex::epoch`는 건드리지 않는다.** 회수 기계가 아니라 인덱스 세대 카운터다. `clear_with()`만 올린다.
- `CAP = 1024`, `POOL = CAP + 2 = 1026`, `SEALED = 1 << (usize::BITS - 1)`, `COUNT = !SEALED`.

## 명령어

| 무엇 | 명령 |
|---|---|
| 단위 테스트 | `cargo test --lib` |
| 특정 테스트 | `cargo test --lib <name> -- --nocapture` |
| 린트 (필수) | `make lint` |
| 통합 테스트 | `make test` (도커. arcus 서버가 필요하다) |

**모든 Task는 `cargo test --lib`와 `make lint`가 통과해야 끝난다.**

## 파일 구조

| 파일 | 책임 |
|---|---|
| `src/handler/usearch/retire.rs` (신규) | 배리어 슬롯, 이벤트 큐, 게이트, `drain_once`. 엔진을 모르고 `Elements`만 안다 |
| `src/handler/usearch/mod.rs` | `retire` 모듈 공개 |
| `src/handler/usearch/index.rs` | `AnnIndex`가 `Retirement`를 갖고, `search()`가 `Reading`을 든다. 옛 회수 기계 삭제 |
| `src/handler/access/sweep.rs` | sweeper 루프가 `drain_once`를 돈다. 1초 틱 제거 |
| `src/server.rs` | 전역 시계(`COARSE`, `tick`, `advance_epoch`) 삭제 |
| `src/handler/registry.rs` | 죽은 `touch()`/`last_access` 삭제 |
| `src/error.rs` | 게이트 타임아웃용 `Error::Busy` 추가 |

---

### Task 1: `insert_published` 오류 경로를 큐로 돌린다

`link()`가 실패했을 때 `displaced`를 그 자리에서 `release`한다. 그 주소는 아직 `held`에 있어서 검색이 hit으로 내놓을 수 있고, `held` 쓰기 락과 그래프 읽기 락을 쥔 채 cache lock을 잡는다. 지금도 있는 버그이고, 이 계획의 나머지가 기대는 불변식(`held`에서 먼저 뺀다)을 깨는 유일한 경로다.

**Files:**
- Modify: `src/handler/usearch/index.rs:592-600`
- Test: `src/handler/usearch/index.rs` (같은 파일의 `mod tests`)

**Interfaces:**
- Consumes: 없음
- Produces: 없음 (동작 수정만)

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`mod tests` 안, `overlapping_searches_do_not_starve_a_retirement` 옆에 넣는다.

```rust
    #[test]
    fn a_failed_link_does_not_release_the_displaced_address_inline() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let old = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        // 같은 id를 다시 쓰는데 link가 실패한다. 옛 주소는 아직 held에 있으므로
        // 그 자리에서 놓아주면 안 된다 -- 도는 검색이 그것을 hit으로 낼 수 있다.
        let staged = idx
            .stage(&crate::handler::quant::encode(
                &[1.0, 0.0, 0.0, 0.0],
                idx.layout.quant,
            ))
            .unwrap();
        let done: std::result::Result<Published, PublishError<()>> =
            idx.insert_published(staged, || FAKE.addr_of(&idx, "a"), || Err(()));
        assert!(matches!(done, Err(PublishError::Store(()))));

        assert!(
            FAKE.id_at(old).is_some(),
            "실패 경로가 옛 주소를 즉시 놓아주었다. 큐를 거쳐야 한다"
        );
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib a_failed_link_does_not_release_the_displaced_address_inline -- --nocapture`
Expected: FAIL — `실패 경로가 옛 주소를 즉시 놓아주었다`

- [ ] **Step 3: 최소 구현**

`src/handler/usearch/index.rs`의 오류 갈래를 통째로 바꾼다. 원래:

```rust
        let addr = match written {
            Ok(addr) => addr,
            Err(e) => {
                if let Some(old) = displaced {
                    self.elements.release(&[old]);
                }
                drop(held);
                drop(index);
                let _ = self.drop_node(key);
                return Err(PublishError::Store(e));
            }
        };
```

바꾼 뒤:

```rust
        let addr = match written {
            Ok(addr) => addr,
            Err(e) => {
                // held에서 먼저 빼고 큐로 보낸다. 여기서 release하면 아직 held에
                // 있는 주소를 놓아주는 것이고, 게다가 두 락을 쥔 채 cache lock을
                // 잡는 것이라 잠금 순서도 뒤집힌다.
                let taken = displaced.is_some_and(|old| held.give_up(old, tombstone));
                drop(held);
                drop(index);
                let _ = self.drop_node(key);
                self.drop_displaced(displaced, taken);
                return Err(PublishError::Store(e));
            }
        };
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test --lib -- --nocapture`
Expected: 전부 PASS

- [ ] **Step 5: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/index.rs
git commit -m "fix: queue the displaced address when a link fails

The error path released it inline while it was still in the held set, so a
search already running could answer from freed memory -- and it did so under
the held write lock and the graph read lock, taking the cache lock in the
direction the trigger design forbids."
```

---

### Task 2: `Retirement` 골격 — 슬롯 붙기, 떼기, 봉인

배리어 한 칸은 워드 하나다. 봉인 비트와 붙은 검색 수를 같이 넣어, RMW 하나가 둘을 동시에 보게 한다. 따로 두면 Dekker 패턴이라 `SeqCst`가 필요해진다.

**Files:**
- Create: `src/handler/usearch/retire.rs`
- Modify: `src/handler/usearch/mod.rs`

**Interfaces:**
- Consumes: 없음
- Produces:
  - `pub(super) struct Retirement`
  - `Retirement::new() -> Retirement`
  - `Retirement::enter(&self) -> Reading<'_>` (Task 5에서 `Result`로 바뀐다)
  - `pub(super) struct Reading<'a>` — `Drop`에서 슬롯을 내린다
  - `#[cfg(test)] Retirement::open_slot(&self) -> usize`
  - `#[cfg(test)] Retirement::slot_count(&self, slot: usize) -> usize`
  - `#[cfg(test)] Retirement::seal_open(&self) -> usize`
  - 상수 `CAP`, `POOL`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`src/handler/usearch/retire.rs`를 만들고 테스트만 넣는다.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_holds_the_open_slot_until_it_ends() {
        let r = Retirement::new();
        let slot = r.open_slot();
        assert_eq!(r.slot_count(slot), 0);

        let reading = r.enter();
        assert_eq!(r.slot_count(slot), 1, "검색이 붙었다");

        drop(reading);
        assert_eq!(r.slot_count(slot), 0, "검색이 떨어졌다");
    }

    #[test]
    fn two_searches_share_one_slot() {
        let r = Retirement::new();
        let slot = r.open_slot();

        let a = r.enter();
        let b = r.enter();
        assert_eq!(r.slot_count(slot), 2);

        drop(a);
        assert_eq!(r.slot_count(slot), 1);
        drop(b);
        assert_eq!(r.slot_count(slot), 0);
    }

    #[test]
    fn sealing_returns_the_count_at_that_moment() {
        let r = Retirement::new();
        let a = r.enter();
        let b = r.enter();

        assert_eq!(r.seal_open(), 2, "봉인 순간의 카운트를 정확히 돌려준다");
        drop(a);
        drop(b);
    }

    #[test]
    fn a_search_that_meets_a_sealed_slot_goes_to_the_next_one() {
        let r = Retirement::new();
        let first = r.open_slot();

        let held_open = r.enter(); // 봉인이 0을 돌려주지 않도록 하나 붙여둔다
        assert_eq!(r.seal_open(), 1);

        let late = r.enter();
        assert_ne!(
            r.open_slot(),
            first,
            "봉인된 슬롯에는 못 붙으므로 다음 칸으로 넘어갔다"
        );
        assert_eq!(r.slot_count(first), 1, "봉인된 칸에는 먼저 붙은 하나만 남는다");
        assert_eq!(r.slot_count(r.open_slot()), 1, "늦게 온 검색은 새 칸에 붙었다");

        drop(late);
        drop(held_open);
    }
}
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 컴파일 실패 — `Retirement` 없음

- [ ] **Step 3: 최소 구현**

`src/handler/usearch/retire.rs`의 테스트 모듈 **위**에 넣는다.

```rust
//! 아이템 참조를 언제 엔진에 돌려줘도 되는지를 큐 순서로 답한다.
//!
//! 검색과 retire를 같은 FIFO에 넣으면 "누가 먼저였나"가 큐 순서 그 자체가 된다.
//! `Release`보다 앞에 있는 배리어는 그 주소가 `held`에서 빠지기 전에 시작한
//! 검색이므로 막아야 하고, 뒤에 있는 배리어는 빠진 뒤에 시작해 `resolve()`의
//! `held.contains()`에 걸리므로 막을 이유가 없다.
//!
//! 자세한 논증은 `docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md`.

use std::sync::atomic::{AtomicUsize, Ordering};

/// 큐가 이만큼 차면 게이트를 닫는다. 고수위 표시이지 상한이 아니다.
pub(super) const CAP: usize = 1024;

/// 배리어 슬롯 수. 게이트가 `CAP`에서 닫힌 뒤 배리어는 최대 하나 더 생긴다.
pub(super) const POOL: usize = CAP + 2;

/// 최상위 비트. 이 배리어는 봉인되어 새 검색을 받지 않는다.
const SEALED: usize = 1 << (usize::BITS - 1);

/// 나머지 비트. 붙어 있는 검색 수.
const COUNT: usize = !SEALED;

/// 검색 하나를 슬롯에 붙인다. 봉인돼 있었으면 물러나고 `false`.
///
/// `fetch_add`의 반환값에 SEALED 비트가 원자적으로 실려 오는 것이 요점이다.
/// 플래그와 카운트를 다른 워드에 두면 이 확인과 `seal`의 확인이 서로를 못 보는
/// 실행이 허용되어(Dekker) `SeqCst`가 필요해진다.
fn attach(cell: &AtomicUsize) -> bool {
    let prev = cell.fetch_add(1, Ordering::AcqRel);
    if prev & SEALED != 0 {
        cell.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    true
}

/// 슬롯을 봉인하고, 봉인된 그 순간에 붙어 있던 검색 수를 돌려준다.
fn seal(cell: &AtomicUsize) -> usize {
    cell.fetch_or(SEALED, Ordering::AcqRel) & COUNT
}

pub(super) struct Retirement {
    slots: Box<[AtomicUsize]>,
    /// 지금 열린 슬롯 번호. retire만 쓰고 검색은 읽기만 한다.
    open: AtomicUsize,
}

impl Retirement {
    pub(super) fn new() -> Self {
        Self {
            slots: (0..POOL).map(|_| AtomicUsize::new(0)).collect(),
            open: AtomicUsize::new(0),
        }
    }

    /// 검색 하나를 지금 열린 배리어에 붙인다.
    ///
    /// 봉인 중이면 물러났다가 `open`을 다시 읽는다. 물러나는 것은 `held`를 읽기
    /// 전이므로 그 검색은 아무 주소도 보지 못했다.
    pub(super) fn enter(&self) -> Reading<'_> {
        loop {
            let slot = self.open.load(Ordering::Acquire);
            if attach(&self.slots[slot]) {
                return Reading { owner: self, slot };
            }
            std::hint::spin_loop();
        }
    }

    #[cfg(test)]
    pub(super) fn open_slot(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn slot_count(&self, slot: usize) -> usize {
        self.slots[slot].load(Ordering::Acquire) & COUNT
    }

    /// 열린 슬롯을 봉인하고 다음 칸을 연다. 테스트에서 retire 없이 쓴다.
    #[cfg(test)]
    pub(super) fn seal_open(&self) -> usize {
        let slot = self.open.load(Ordering::Acquire);
        let live = seal(&self.slots[slot]);
        self.open.store((slot + 1) % POOL, Ordering::Release);
        live
    }
}

/// 검색이 도는 동안 살아 있는 가드.
///
/// `slot`을 들고 다니는 이유: 봉인된 배리어는 큐로 가고 새 배리어가 열리므로,
/// 종료할 때 "지금 열린 배리어"에 내리면 엉뚱한 칸을 내리게 된다. 합계로 갈음할
/// 수도 없다 -- 검색은 끝나는 순서가 제각각이라, 나중 배리어의 검색이 앞 배리어의
/// 빚을 갚아버린다.
#[must_use = "the search is only counted while this is alive"]
pub(super) struct Reading<'a> {
    owner: &'a Retirement,
    slot: usize,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.owner.slots[self.slot].fetch_sub(1, Ordering::AcqRel);
    }
}
```

`src/handler/usearch/mod.rs`에 한 줄 더한다.

```rust
pub mod held;
pub mod index;
pub mod metric;
pub(crate) mod retire;

pub use held::Elements;
pub use index::{Accept, AnnIndex, PublishError, Published, Staged};
pub use metric::Metric;
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 4개 PASS

- [ ] **Step 5: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/retire.rs src/handler/usearch/mod.rs
git commit -m "feat: barrier slots that a search attaches to and a retire seals

One word per barrier, the seal flag packed with the attached count, so the
attach sees the flag and the seal reads the count in the same RMW. A search
carries the slot it joined, because barriers move to the queue underneath it
and aggregate counts cannot tell which one a finishing search belonged to."
```

---

### Task 3: 큐와 슬롯 넘김 규칙

`retire()`가 봉인과 push를 한 덩어리로 한다. 나뉘면 retire 둘이 겹칠 때 배리어와 `Release`의 순서가 뒤집힌다. 그리고 슬롯은 배리어가 **실제로 큐에 들어갈 때만** 넘어간다 — 그래야 검색 없는 delete가 이어져도 아직 큐에 있는 칸을 덮어쓰지 않는다.

**Files:**
- Modify: `src/handler/usearch/retire.rs`

**Interfaces:**
- Consumes: Task 2의 `Retirement`, `Reading`, `attach`, `seal`, `CAP`, `POOL`
- Produces:
  - `Retirement::retire(&self, addrs: &[u64])`
  - `enum Event { Barrier(usize), Release(Vec<u64>) }` (모듈 내부)
  - `#[cfg(test)] Retirement::queue_len(&self) -> usize`
  - `#[cfg(test)] Retirement::queue_shape(&self) -> Vec<char>` — 배리어는 `'B'`, 릴리스는 `'R'`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn a_retire_with_no_search_attached_queues_no_barrier() {
        let r = Retirement::new();
        let slot = r.open_slot();

        r.retire(&[0x10]);
        r.retire(&[0x20]);

        assert_eq!(r.queue_shape(), vec!['R'], "붙은 검색이 없으면 배리어가 없다");
        assert_eq!(r.open_slot(), slot, "슬롯도 넘어가지 않았다");
    }

    #[test]
    fn consecutive_releases_coalesce_into_the_tail() {
        let r = Retirement::new();
        for addr in 0..50u64 {
            r.retire(&[addr]);
        }
        assert_eq!(r.queue_len(), 1, "꼬리에 주소만 붙는다");
    }

    #[test]
    fn slots_do_not_wrap_when_no_search_is_running() {
        let r = Retirement::new();
        let slot = r.open_slot();
        for addr in 0..(POOL as u64 * 3) {
            r.retire(&[addr]);
        }
        assert_eq!(r.open_slot(), slot, "배리어가 큐에 안 들어가면 슬롯은 제자리다");
    }

    #[test]
    fn a_retire_with_a_search_attached_queues_a_barrier_first() {
        let r = Retirement::new();
        let slot = r.open_slot();
        let reading = r.enter();

        r.retire(&[0x10]);

        assert_eq!(r.queue_shape(), vec!['B', 'R'], "배리어가 릴리스보다 앞이다");
        assert_ne!(r.open_slot(), slot, "배리어가 큐에 들어갔으므로 슬롯이 넘어갔다");
        drop(reading);
    }

    #[test]
    fn concurrent_retires_never_put_two_barriers_side_by_side() {
        // 봉인과 push가 한 덩어리가 아니면 [B, B, R, R]처럼 배리어가 붙어 나오고,
        // 그러면 두 번째 배리어 뒤의 릴리스를 첫 배리어가 안 막게 된다.
        let r = Arc::new(Retirement::new());
        let _reading = r.enter(); // 모든 retire가 배리어를 만들도록 하나 붙여둔다

        std::thread::scope(|s| {
            for t in 0..4u64 {
                let r = Arc::clone(&r);
                s.spawn(move || {
                    for i in 0..50u64 {
                        let _hold = r.enter();
                        r.retire(&[t * 1000 + i]);
                    }
                });
            }
        });

        let shape = r.queue_shape();
        for pair in shape.windows(2) {
            assert_ne!(pair, ['B', 'B'], "배리어 둘이 붙어 나왔다: {shape:?}");
        }
    }

    #[test]
    fn a_search_that_starts_after_a_retire_joins_the_next_barrier() {
        let r = Retirement::new();
        let first = r.enter();
        r.retire(&[0x10]);
        let second = r.enter();

        r.retire(&[0x20]);

        assert_eq!(
            r.queue_shape(),
            vec!['B', 'R', 'B', 'R'],
            "두 번째 검색은 두 번째 배리어에 붙어 첫 릴리스를 막지 않는다"
        );
        drop(first);
        drop(second);
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 컴파일 실패 — `retire`, `queue_shape`, `queue_len` 없음

- [ ] **Step 3: 최소 구현**

`use` 줄과 구조체를 고친다.

```rust
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
```

`Event`와 `Queue`를 `Retirement` 정의 위에 더한다.

```rust
/// 큐에 들어가는 두 가지.
enum Event {
    /// 이 슬롯의 배리어가 풀릴 때까지 sweeper가 선다.
    Barrier(usize),
    /// 놓아줄 주소들.
    Release(Vec<u64>),
}

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
}

impl Queue {
    /// 꼬리가 `Release`면 칸을 늘리지 않고 주소만 붙인다.
    ///
    /// 할당이 실패하면 그 배치를 포기한다. 아이템이 프로세스 끝까지 묶이지만,
    /// 메모리 부족으로 데몬을 죽이는 것보다 낫다.
    fn push_addrs(&mut self, addrs: &[u64]) {
        if let Some(Event::Release(tail)) = self.events.back_mut()
            && tail.try_reserve(addrs.len()).is_ok()
        {
            tail.extend_from_slice(addrs);
            return;
        }
        let mut owned = Vec::new();
        if owned.try_reserve_exact(addrs.len()).is_err() || self.events.try_reserve(1).is_err() {
            eprintln!(
                "ArcVector: could not queue {} item reference(s) for release; \
                 they stay pinned until this process ends",
                addrs.len()
            );
            return;
        }
        owned.extend_from_slice(addrs);
        self.events.push_back(Event::Release(owned));
    }

    fn push_barrier(&mut self, slot: usize) -> bool {
        if self.events.try_reserve(1).is_err() {
            return false;
        }
        self.events.push_back(Event::Barrier(slot));
        true
    }
}
```

`Retirement`에 큐를 더한다.

```rust
pub(super) struct Retirement {
    slots: Box<[AtomicUsize]>,
    open: AtomicUsize,
    queue: Mutex<Queue>,
}

impl Retirement {
    pub(super) fn new() -> Self {
        Self {
            slots: (0..POOL).map(|_| AtomicUsize::new(0)).collect(),
            open: AtomicUsize::new(0),
            queue: Mutex::new(Queue::default()),
        }
    }
```

그리고 `retire`를 더한다. `seal_open`은 Task 2의 원시 연산 테스트가 쓰므로 그대로 둔다 — 슬롯 넘김의 **진짜 규칙은 `retire`에 있다**는 주석을 달아준다.

```rust
    /// 주소를 놓아줄 목록에 넣는다. 엔진을 건드리지 않는다.
    ///
    /// **호출 전에 그 주소가 `held`에서 빠져 있어야 한다.** 그래야 이 뒤에 시작한
    /// 검색이 `resolve()`에서 걸러져 그 주소에 닿지 못한다.
    ///
    /// 봉인과 push가 한 덩어리인 것은 retire 둘이 겹칠 때 순서가 뒤집히지 않게
    /// 하기 위해서다. `[B(b1), R(y), B(b), R(x)]`가 되면 b에 붙은 검색이 y를 들고
    /// 있을 수 있는데 `R(y)`를 막지 못한다.
    pub(super) fn retire(&self, addrs: &[u64]) {
        if addrs.is_empty() {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);

        let slot = self.open.load(Ordering::Acquire);
        if seal(&self.slots[slot]) == 0 {
            // 아무도 안 붙었다. 봉인만 풀고 슬롯을 그대로 쓴다.
            //
            // `store(0)`이 아니라 `fetch_and`인 이유: 봉인과 이 줄 사이에 붙으려다
            // 물러나는 검색이 있을 수 있고, 그 `fetch_add`와 `fetch_sub` 사이에
            // 0을 덮어쓰면 뺄 때 언더플로가 난다. 플래그만 끈다.
            self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
        } else if q.push_barrier(slot) {
            // 배리어가 큐에 들어갔을 때만 슬롯이 넘어간다. 그래야 살아 있는
            // 배리어 수가 큐 안의 배리어 수와 같아져 슬롯이 감기지 않는다.
            self.open.store((slot + 1) % POOL, Ordering::Release);
        } else {
            // 배리어를 못 넣었다. 그대로 주소를 넣으면 그것을 지켜줄 배리어가 없는
            // 채로 큐에 들어간다 -- 붙어 있던 검색이 그 주소를 들고 있을 수 있다.
            // 이 배치를 포기한다. 아이템이 묶이지만 해제 순서를 깨는 것보다 낫다.
            self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
            eprintln!(
                "ArcVector: could not queue a barrier; {} item reference(s) stay \
                 pinned until this process ends",
                addrs.len()
            );
            return;
        }

        q.push_addrs(addrs);
    }

    #[cfg(test)]
    pub(super) fn queue_len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .len()
    }

    #[cfg(test)]
    pub(super) fn queue_shape(&self) -> Vec<char> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .iter()
            .map(|e| match e {
                Event::Barrier(_) => 'B',
                Event::Release(_) => 'R',
            })
            .collect()
    }
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 10개 PASS

동시성 테스트가 `Arc`를 쓰므로 테스트 모듈 맨 위에 `use std::sync::Arc;`를 더한다.

- [ ] **Step 5: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/retire.rs
git commit -m "feat: queue barriers and releases in one order under one lock

A slot advances only when its barrier actually reaches the queue, so deletes
with no search running reuse the same slot forever and the pool never wraps
onto a barrier a search still holds. Consecutive releases coalesce into the
tail, so a run of deletes costs one cell."
```

---

### Task 4: `drain_once` — 큐를 순서대로 걷는다

sweeper가 하는 일 전부. 안 풀린 배리어에서 서고, 풀린 배리어는 슬롯을 되돌리고 버리고, `Release`는 **뮤텍스를 놓은 뒤에** 놓아준다.

**Files:**
- Modify: `src/handler/usearch/retire.rs`

**Interfaces:**
- Consumes: Task 3의 `Retirement`, `Event`, `Queue`
- Produces:
  - `pub(super) enum Progress { Released(usize), Blocked, Empty }`
  - `Retirement::drain_once(&self, elements: &dyn Elements) -> Progress`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

테스트 모듈 맨 위에 작은 가짜 저장소를 둔다.

```rust
    use crate::handler::usearch::held::Elements;
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(Default)]
    struct FakeElements {
        freed: StdMutex<Vec<u64>>,
    }

    impl Elements for FakeElements {
        fn id_at(&self, _addr: u64) -> Option<Arc<str>> {
            None
        }
        fn release(&self, addrs: &[u64]) {
            self.freed.lock().unwrap().extend_from_slice(addrs);
        }
    }

    impl FakeElements {
        fn freed(&self) -> Vec<u64> {
            self.freed.lock().unwrap().clone()
        }
    }

    #[test]
    fn an_unresolved_barrier_blocks_everything_behind_it() {
        let r = Retirement::new();
        let e = FakeElements::default();

        let reading = r.enter();
        r.retire(&[0x10]);

        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        assert!(e.freed().is_empty(), "앞선 검색이 살아 있으면 못 놓아준다");

        drop(reading);
        assert!(matches!(r.drain_once(&e), Progress::Released(0)), "배리어를 버린다");
        assert!(matches!(r.drain_once(&e), Progress::Released(1)), "그 다음이 릴리스");
        assert_eq!(e.freed(), vec![0x10]);
    }

    #[test]
    fn a_search_that_started_after_the_retire_does_not_block_it() {
        let r = Retirement::new();
        let e = FakeElements::default();

        r.retire(&[0x10]);
        let late = r.enter(); // retire 뒤에 시작했으므로 0x10에 닿을 수 없다

        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(e.freed(), vec![0x10], "나중 검색은 앞의 릴리스를 막지 않는다");
        drop(late);
    }

    #[test]
    fn an_empty_queue_reports_empty() {
        let r = Retirement::new();
        let e = FakeElements::default();
        assert!(matches!(r.drain_once(&e), Progress::Empty));
    }

    #[test]
    fn a_resolved_barrier_gives_its_slot_back() {
        let r = Retirement::new();
        let e = FakeElements::default();

        let reading = r.enter();
        let slot = r.open_slot();
        r.retire(&[0x10]);
        drop(reading);

        assert!(matches!(r.drain_once(&e), Progress::Released(0)));
        assert_eq!(r.slot_count(slot), 0, "버린 슬롯은 깨끗하게 되돌아왔다");
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 컴파일 실패 — `drain_once`, `Progress` 없음

- [ ] **Step 3: 최소 구현**

`use`에 한 줄 더한다.

```rust
use crate::handler::usearch::held::Elements;
```

그리고 `Retirement`에 더한다.

```rust
/// `drain_once` 한 번이 무엇을 했는지.
pub(super) enum Progress {
    /// 항목 하나를 처리했다. 값은 놓아준 주소 수(배리어면 0).
    Released(usize),
    /// 머리의 배리어가 아직 안 풀렸다.
    Blocked,
    /// 큐가 비었다.
    Empty,
}

impl Retirement {
    /// 큐의 머리 하나를 처리한다.
    ///
    /// `release`는 **뮤텍스를 놓은 뒤에** 부른다. `release` → `do_item_release`가
    /// unlink 콜백을 재진입시킬 수 있고, 그 콜백이 같은 뮤텍스를 다시 잡는다.
    pub(super) fn drain_once(&self, elements: &dyn Elements) -> Progress {
        let batch = {
            let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            match q.events.front() {
                None => return Progress::Empty,

                Some(Event::Barrier(slot)) => {
                    let slot = *slot;
                    if self.slots[slot].load(Ordering::Acquire) & COUNT > 0 {
                        return Progress::Blocked;
                    }
                    // 봉인만 푼다. `store(0)`이면 물러나는 중인 검색의 `+1`을
                    // 덮어써 그 `-1`에서 언더플로가 난다.
                    self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
                    q.events.pop_front();
                    return Progress::Released(0);
                }

                Some(Event::Release(_)) => match q.events.pop_front() {
                    Some(Event::Release(addrs)) => addrs,
                    _ => unreachable!("just matched a release"),
                },
            }
        };

        elements.release(&batch);
        Progress::Released(batch.len())
    }
}
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 13개 PASS

- [ ] **Step 5: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/retire.rs
git commit -m "feat: walk the retirement queue in order, stopping at live barriers

Reaching a release means every barrier ahead of it has cleared, which is
exactly the claim that no search can still hold those addresses. The batch
leaves the mutex before release runs, because release can re-enter the unlink
callback and that callback takes the same mutex."
```

---

### Task 5: 게이트 — 캡에 닿으면 검색을 세운다

큐가 자라는 조건은 하나다: retire 시점에 붙은 검색이 있을 때. 검색을 막으면 진행 중인 것들이 끝나면서 배리어가 풀리고, 그 뒤 retire는 `seal()`이 0이라 배리어를 안 만든다. 막힌 검색은 아무것도 붙잡고 있지 않으므로 데드락이 없다.

**Files:**
- Modify: `src/handler/usearch/retire.rs`
- Modify: `src/error.rs`

**Interfaces:**
- Consumes: Task 4의 `Retirement`, `Progress`
- Produces:
  - `Retirement::enter(&self) -> crate::error::Result<Reading<'_>>` (시그니처가 바뀐다)
  - `Error::Busy`
  - `#[cfg(test)] Retirement::with_cap(cap: usize) -> Retirement`
  - `#[cfg(test)] Retirement::is_gated(&self) -> bool`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn the_gate_closes_when_the_queue_reaches_the_cap() {
        let r = Retirement::with_cap(2);
        let a = r.enter().unwrap();
        r.retire(&[0x10]); // [B, R]  = 2칸
        assert!(r.is_gated(), "캡에 닿으면 게이트가 닫힌다");
        drop(a);
    }

    #[test]
    fn a_drained_queue_opens_the_gate_again() {
        let r = Retirement::with_cap(2);
        let e = FakeElements::default();
        let a = r.enter().unwrap();
        r.retire(&[0x10]);
        assert!(r.is_gated());

        drop(a);
        while !matches!(r.drain_once(&e), Progress::Empty) {}
        assert!(!r.is_gated(), "큐가 빠지면 게이트가 열린다");
    }

    #[test]
    fn a_gated_search_gives_up_rather_than_waiting_forever() {
        let r = Retirement::with_cap(2);
        let a = r.enter().unwrap();
        r.retire(&[0x10]);
        assert!(r.is_gated());

        // a가 끝나지 않으므로 큐가 빠지지 않는다. 대기는 타임아웃으로 끝난다.
        let refused = r.enter_deadline(std::time::Duration::from_millis(50));
        assert!(matches!(refused, Err(crate::error::Error::Busy)));
        drop(a);
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 컴파일 실패 — `with_cap`, `is_gated`, `Error::Busy` 없음

- [ ] **Step 3: 최소 구현**

`src/error.rs`의 `enum Error`에 한 줄 더한다.

```rust
pub enum Error {
    BadRequest(String),
    NoSuchIndex,

    IndexEvicted,

    Unreadable,
    Rebuilding,
    Busy,
    Codec(CodecError),
    Filter(ParseError),
    Store(StoreError),
    Index(String),
}
```

`blame()`의 서버 갈래에 넣는다.

```rust
            Self::Unreadable | Self::Rebuilding | Self::Busy => Blame::Server,
```

`Display`에도 한 줄 더한다 (다른 갈래들 사이에).

```rust
            Self::Busy => write!(
                f,
                "the index is releasing memory and cannot take new searches"
            ),
```

`src/handler/usearch/retire.rs`: `use`를 늘린다.

```rust
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use crate::error::{Error, Result};
```

`Queue`에 게이트 상태를 넣는다.

```rust
#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    gated: bool,
}
```

`Retirement`에 필드를 더한다.

```rust
pub(super) struct Retirement {
    slots: Box<[AtomicUsize]>,
    open: AtomicUsize,
    queue: Mutex<Queue>,
    gate: Condvar,
    cap: usize,
}
```

`new`/`with_cap`:

```rust
    pub(super) fn new() -> Self {
        Self::with_cap_inner(CAP)
    }

    #[cfg(test)]
    pub(super) fn with_cap(cap: usize) -> Self {
        Self::with_cap_inner(cap)
    }

    fn with_cap_inner(cap: usize) -> Self {
        Self {
            slots: (0..POOL).map(|_| AtomicUsize::new(0)).collect(),
            open: AtomicUsize::new(0),
            queue: Mutex::new(Queue::default()),
            gate: Condvar::new(),
            cap,
        }
    }
```

`enter`를 게이트를 보도록 바꾼다.

```rust
/// 게이트가 닫혀 있을 때 검색이 기다리는 시간. 넘으면 거절한다.
const GATE_TIMEOUT: Duration = Duration::from_secs(1);

impl Retirement {
    pub(super) fn enter(&self) -> Result<Reading<'_>> {
        self.enter_deadline(GATE_TIMEOUT)
    }

    pub(super) fn enter_deadline(&self, timeout: Duration) -> Result<Reading<'_>> {
        self.wait_for_gate(timeout)?;
        loop {
            let slot = self.open.load(Ordering::Acquire);
            if attach(&self.slots[slot]) {
                return Ok(Reading { owner: self, slot });
            }
            std::hint::spin_loop();
        }
    }

    /// 게이트가 열릴 때까지 기다린다. 기다리는 동안 이 검색은 어떤 배리어에도
    /// 붙어 있지 않으므로, 진행 중인 검색들이 끝나 큐가 빠지는 것을 막지 않는다.
    fn wait_for_gate(&self, timeout: Duration) -> Result<()> {
        let mut q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if !q.gated {
            return Ok(());
        }
        let (q, wait) = self
            .gate
            .wait_timeout_while(q, timeout, |q| q.gated)
            .unwrap_or_else(PoisonError::into_inner);
        if wait.timed_out() && q.gated {
            return Err(Error::Busy);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn is_gated(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .gated
    }
```

`retire`의 끝에서 게이트를 건다. `q.push_addrs(addrs);` 다음 줄에 넣는다.

```rust
        q.push_addrs(addrs);
        q.gated = q.events.len() >= self.cap;
```

`drain_once`가 자리가 나면 연다. 배리어 갈래의 `q.events.pop_front();` 다음, 그리고 릴리스 갈래의 `pop_front` 다음에 같은 두 줄을 넣는다 — 한 곳에 모으려면 작은 메서드로 뺀다.

```rust
impl Queue {
    /// 자리가 났으면 게이트를 연다. 푸는 것은 `drain_once`가 한다.
    fn reopen_if_room(&mut self, cap: usize) -> bool {
        if self.gated && self.events.len() < cap {
            self.gated = false;
            return true;
        }
        false
    }
}
```

`drain_once`의 배리어 갈래:

```rust
                Some(Event::Barrier(slot)) => {
                    let slot = *slot;
                    if self.slots[slot].load(Ordering::Acquire) & COUNT > 0 {
                        return Progress::Blocked;
                    }
                    self.slots[slot].fetch_and(COUNT, Ordering::AcqRel);
                    q.events.pop_front();
                    if q.reopen_if_room(self.cap) {
                        self.gate.notify_all();
                    }
                    return Progress::Released(0);
                }
```

릴리스 갈래:

```rust
                Some(Event::Release(_)) => {
                    let taken = match q.events.pop_front() {
                        Some(Event::Release(addrs)) => addrs,
                        _ => unreachable!("just matched a release"),
                    };
                    if q.reopen_if_room(self.cap) {
                        self.gate.notify_all();
                    }
                    taken
                }
```

- [ ] **Step 4: 앞선 테스트를 새 시그니처에 맞춘다**

`enter()`가 `Result`를 돌려주게 됐으므로, Task 2~4에서 쓴 `r.enter()`를 전부
`r.enter().unwrap()`으로 바꾼다. 해당 테스트는 게이트가 닫힌 적이 없으므로 항상 `Ok`다.

```bash
grep -n "r.enter()" src/handler/usearch/retire.rs
```

- [ ] **Step 5: 통과를 확인한다**

Run: `cargo test --lib -- --nocapture`
Expected: 전부 PASS (retire 모듈 17개 포함)

- [ ] **Step 6: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/retire.rs src/error.rs
git commit -m "feat: hold searches at the cap so the queue can drain

The queue grows only when a retire finds searches attached, so stopping new
searches stops the growth: the ones in flight finish, their barrier clears,
and every retire after that seals an empty barrier. A waiting search holds no
barrier, so it cannot be what the sweeper is waiting for. After a second it
gives up with Busy rather than hanging the connection."
```

---

### Task 6: `AnnIndex`를 `Retirement`로 갈아끼운다

`search()`의 가드를 `Searching`에서 `Reading`으로, `AnnIndex::retire()`를 `Retirement::retire()`로, `reclaim()`을 `drain_once()`로 바꾼다. 옛 기계는 아직 지우지 않는다 (Task 8).

**Files:**
- Modify: `src/handler/usearch/index.rs`
- Modify: `src/handler/access/sweep.rs:74-81`

**Interfaces:**
- Consumes: Task 5의 `Retirement`, `Reading`, `Progress`
- Produces:
  - `AnnIndex.retirement: Retirement` (비공개 필드)
  - `AnnIndex::drain_retired(&self) -> bool` — 한 항목 처리했으면 `true`
  - `AnnIndex::search`가 `Error::Busy`를 낼 수 있다

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`index.rs`의 `mod tests`에서 기존 두 테스트의 `Searching::new(&idx)`를 `idx.retirement.enter().unwrap()`으로 바꾸고, 새 테스트를 더한다.

```rust
    #[test]
    fn a_running_search_holds_back_the_address_it_could_have_seen() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let addr = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        let reading = idx.retirement.enter().unwrap();
        assert!(remove(&idx, "a"), "그래프에서 뺐다");

        while idx.drain_retired() {}
        assert!(
            FAKE.id_at(addr).is_some(),
            "이 검색이 그 주소를 봤을 수 있으므로 아직 못 놓아준다"
        );

        drop(reading);
        while idx.drain_retired() {}
        assert!(FAKE.id_at(addr).is_none(), "검색이 끝났으니 돌려준다");
    }

    #[test]
    fn a_search_that_starts_after_the_delete_does_not_hold_it_back() {
        let idx = build(4, Quant::F32, Metric::L2, 2);
        let addr = add(&idx, "a", &[1.0, 0.0, 0.0, 0.0]);

        assert!(remove(&idx, "a"));
        let late = idx.retirement.enter().unwrap();

        while idx.drain_retired() {}
        assert!(
            FAKE.id_at(addr).is_none(),
            "삭제 뒤에 시작한 검색은 그 주소에 닿을 수 없으므로 막지 않는다"
        );
        drop(late);
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib -- --nocapture`
Expected: 컴파일 실패 — `retirement` 필드 없음, `drain_retired` 없음

- [ ] **Step 3: 최소 구현**

`index.rs` 위쪽 `use`에 더한다.

```rust
use crate::handler::usearch::retire::{Progress, Retirement};
```

`struct AnnIndex`에 필드를 더한다 (기존 `retired` 옆).

```rust
    pub(super) retirement: Retirement,
```

생성자에 초기화를 더한다 (`retired: std::sync::Mutex::new(Vec::new()),` 옆).

```rust
            retirement: Retirement::new(),
```

`AnnIndex::retire`의 몸통을 갈아끼운다. 옛 몸통(시계·튜플 큐)을 지우고:

```rust
    /// 주소를 놓아줄 목록에 넣는다. 엔진을 건드리지 않는다.
    ///
    /// 호출 전에 그 주소가 `held`에서 빠져 있어야 한다 — 그래야 이 뒤에 시작한
    /// 검색이 `resolve()`에서 걸러진다.
    fn retire(&self, addrs: &[u64]) {
        self.retirement.retire(addrs);
    }
```

`search()`의 가드를 바꾼다.

```rust
        let _reading = self.retirement.enter()?;
        let entered = self.epoch.load(Ordering::Acquire);
```

(`let _searching = Searching::new(self);` 줄을 이것으로 대체한다. `entered` 줄은 그대로 둔다 — `epoch`는 인덱스 세대 카운터라 이 설계와 무관하다.)

`reclaim()`을 대신할 메서드를 더한다.

```rust
    /// 큐의 항목 하나를 처리한다. 무언가 했으면 `true`.
    ///
    /// sweeper만 부른다. `release`가 cache lock을 잡으므로 그래프 락 아래에서
    /// 부르면 안 된다.
    pub fn drain_retired(&self) -> bool {
        matches!(
            self.retirement.drain_once(&*self.elements),
            Progress::Released(_)
        )
    }
```

`sweep.rs`의 루프에서 `index.ann.reclaim()`을 바꾼다.

```rust
            for index in registry::indexes().unwrap_or_default() {
                index.ann.retry_stuck();
                while index.ann.drain_retired() {}
            }
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test --lib -- --nocapture`
Expected: 전부 PASS

- [ ] **Step 5: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/index.rs src/handler/access/sweep.rs
git commit -m "feat: move AnnIndex onto the barrier queue

search() now holds a Reading for its whole span, resolve() included, because
the dangerous dereference is id_at and not the traversal. The sweeper walks
the queue instead of comparing stamps."
```

---

### Task 7: 종 — 마지막 검색이 sweeper를 깨운다

봉인된 배리어를 0으로 만든 검색이 sweeper를 깨운다. 봉인되지 않은 배리어에서는 아무도 기다리지 않으므로 울리지 않는다 — 그래서 평상시 검색 종료 비용은 원자 감산 하나다.

**Files:**
- Modify: `src/handler/usearch/retire.rs`
- Modify: `src/handler/access/sweep.rs`

**Interfaces:**
- Consumes: Task 6의 `AnnIndex::drain_retired`
- Produces:
  - `Retirement::set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>)` — sweeper를 깨우는 콜백
  - `sweep::wake()` (pub(in crate::handler))

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn the_last_search_of_a_sealed_barrier_rings_the_bell() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        let a = r.enter().unwrap();
        let b = r.enter().unwrap();
        r.retire(&[0x10]); // 봉인(2)

        drop(a);
        assert_eq!(rings.load(Ordering::Relaxed), 0, "아직 마지막이 아니다");
        drop(b);
        assert_eq!(rings.load(Ordering::Relaxed), 1, "마지막이 울린다");
    }

    #[test]
    fn an_unsealed_barrier_does_not_ring() {
        use std::sync::atomic::AtomicUsize as Counter;

        let r = Retirement::new();
        let rings = Arc::new(Counter::new(0));
        let seen = Arc::clone(&rings);
        r.set_bell(Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        }));

        drop(r.enter().unwrap());
        assert_eq!(
            rings.load(Ordering::Relaxed),
            0,
            "봉인 안 된 배리어는 아무도 안 기다린다"
        );
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test --lib retire:: -- --nocapture`
Expected: 컴파일 실패 — `set_bell` 없음

- [ ] **Step 3: 최소 구현**

`retire.rs`의 `use`에 더한다.

```rust
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
```

`Retirement`에 필드를 더한다.

```rust
    bell: OnceLock<Arc<dyn Fn() + Send + Sync>>,
```

생성자에 `bell: OnceLock::new(),`를 더하고, 메서드를 더한다.

```rust
    /// sweeper를 깨우는 방법을 알려준다. 등록 전에는 아무것도 하지 않는다.
    pub(super) fn set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.bell.set(bell);
    }

    /// 종을 울린다. 큐 뮤텍스를 한 번 거치는 것은 알림 유실을 막기 위해서다 --
    /// sweeper가 "안 풀렸다"를 확인하고 `wait`에 들어가기 전에 울리면 그 알림이
    /// 사라진다.
    fn ring(&self) {
        let Some(bell) = self.bell.get() else { return };
        drop(self.queue.lock().unwrap_or_else(PoisonError::into_inner));
        bell();
    }
```

`Drop for Reading`을 바꾼다.

```rust
impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let prev = self.owner.slots[self.slot].fetch_sub(1, Ordering::AcqRel);
        if prev & COUNT == 1 && prev & SEALED != 0 {
            // 내가 마지막이고, 이 배리어는 큐에서 sweeper를 세우고 있다.
            self.owner.ring();
        }
    }
}
```

`sweep.rs`에 `wake()`를 더한다.

```rust
/// sweeper를 깨운다. 봉인된 배리어를 푼 검색이 부른다.
pub(in crate::handler) fn wake() {
    SWEEPER.wake.notify_one();
}
```

`registry.rs`에서 인덱스를 등록할 때 종을 연결한다. `sweep::ensure_sweeper();`를 부르는 두 곳 바로 다음에 넣는다.

```rust
    sweep::ensure_sweeper();
    index.ann.set_bell();
```

`index.rs`에 그 배선을 더한다.

```rust
    /// sweeper를 깨우는 종을 단다. 레지스트리에 들어갈 때 한 번 부른다.
    pub fn set_bell(&self) {
        self.retirement
            .set_bell(std::sync::Arc::new(crate::handler::access::sweep::wake));
    }
```

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test --lib -- --nocapture`
Expected: 전부 PASS

- [ ] **Step 5: 린트하고 커밋한다**

```bash
make lint
git add src/handler/usearch/retire.rs src/handler/access/sweep.rs src/handler/registry.rs src/handler/usearch/index.rs
git commit -m "feat: wake the sweeper when a sealed barrier clears

Only the search that takes a sealed barrier to zero rings, so the ordinary
exit is one atomic subtract. The ring goes through the queue mutex so it
cannot land in the window between the sweeper's check and its wait."
```

---

### Task 8: 옛 회수 기계를 걷어낸다

이제 아무도 안 쓰는 것들을 지운다. 지울 것과 **닮았지만 남는 것**을 헷갈리지 않는 것이 이 Task의 전부다.

**Files:**
- Modify: `src/handler/usearch/index.rs`
- Modify: `src/server.rs`
- Modify: `src/handler/registry.rs`
- Modify: `src/handler/access/sweep.rs`

**Interfaces:**
- Consumes: Task 7까지의 전부
- Produces: 없음 (삭제만)

- [ ] **Step 1: 지울 것을 확인한다**

```bash
cargo test --lib 2>&1 | tail -5
grep -n "Searching\|READER_SLOTS\|unslotted\|REGISTERING\|NO_READER\|HOME\|oldest_reader\|take_ready\|RECLAIM_BATCH\|queued\|coarse_now\|advance_epoch\|COARSE\|last_access" src/handler/usearch/index.rs src/server.rs src/handler/registry.rs src/handler/access/sweep.rs
```

Expected: 위 이름들이 남아 있고, 테스트는 전부 통과한다.

- [ ] **Step 2: `index.rs`에서 지운다**

- `struct Searching`와 그 `impl`, `impl Drop for Searching`
- 상수 `READER_SLOTS`, `NO_READER`, `REGISTERING`, `RECLAIM_BATCH`
- `static NEXT_HOME`과 `thread_local! { static HOME }`
- `AnnIndex`의 필드 `readers`, `unslotted`, `retired`, `queued`와 생성자의 대응 줄
- 메서드 `oldest_reader()`, `reclaim()`
- 자유 함수 `take_ready()`와 그 테스트

**남기는 것 (지우지 말 것):**

| 남긴다 | 왜 |
|---|---|
| `AnnIndex::epoch`, `Staged::at_epoch`, `search()`의 `entered`, `resolve(&hits, entered)` | 인덱스 세대 카운터다. `clear_with()`만 올린다. 지우면 리셋 뒤 재사용된 아이템 포인터가 `held.contains()`를 통과해 **엉뚱한 id**를 답한다 |
| `in_flight` / `InFlight` | `k` 보정과 staging용 |
| `stuck` / `retry_stuck()` | 그래프에서 안 빠진 노드의 재시도 |
| `registry::CLOCK` | `remove_if_stale`의 stamp. 별개 카운터다 |

- [ ] **Step 3: `server.rs`에서 지운다**

`COARSE`, `coarse_now()`, `tick()`, `advance_epoch()`, `advance()`와 그 테스트 모듈(`mod tests`의 `coarse_now` 관련 부분)을 지운다.

- [ ] **Step 4: `registry.rs`와 `sweep.rs`에서 지운다**

`registry.rs`: `VectorIndex`의 `last_access` 필드, 생성자의 초기화, `fn touch()`, 그리고 `touch()` 호출부. **쓰는 곳이 없어서 지우는 것**이다 (`last_access`를 읽는 코드가 없다).

`sweep.rs`: `run()`의 `crate::server::tick();` 호출과, 그것을 감싸던 `last_round`/`TICK` 기반 분기를 지운다. `TICK`은 `wait_timeout`의 백스톱으로 남긴다 — `retry_stuck`은 종을 울려줄 주체가 없다.

- [ ] **Step 5: 통과를 확인하고 커밋한다**

Run: `cargo test --lib -- --nocapture`
Expected: 전부 PASS

Run: `make lint`
Expected: 경고 없음 (죽은 코드가 남아 있으면 clippy가 잡는다)

```bash
git add src/handler/usearch/index.rs src/server.rs src/handler/registry.rs src/handler/access/sweep.rs
git commit -m "refactor: drop the epoch reclamation machinery

The 128 reader slots, the two-step claim, the unslotted fallback and the
global clock all existed to compare a retirement's stamp against a reader's.
The queue order answers that now.

AnnIndex::epoch stays -- only clear_with moves it, so it is an index
generation, and without it a recycled item pointer would pass the held-set
check and answer a wrong id."
```

---

### Task 9: 통합 테스트와 서버 API 정리

**Files:**
- Test: `make test` (도커)
- Modify: `../arcus-memcached` (별도 저장소, `deps` 브랜치)

**Interfaces:**
- Consumes: Task 8까지의 전부
- Produces: 없음

- [ ] **Step 1: 통합 테스트를 돌린다**

Run: `make test`
Expected: 통합 테스트 통과. 실패하면 기존에 알려진 실패인지 `git stash` 후 비교해 확인한다.

- [ ] **Step 2: `get_num_threads`를 뺄지 정한다**

이 서버 API는 리더 슬롯 수를 워커 수에 맞추려고 넣은 것이라 목적이 사라졌다. 다만 **arcus-memcached 저장소의 별도 커밋**이고, 다른 용도로 남길 수도 있다.

빼기로 하면 `../arcus-memcached`의 `deps` 브랜치에서:
- `include/memcached/server_api.h`의 `get_num_threads` 선언
- `memcached.c`의 `static int get_num_threads(void)` 정의와 `get_server_api()`의 `.get_num_threads = get_num_threads,`

세 곳을 되돌린다. `mc_engine.v1` 대입을 한 줄 올린 변경과 `ON_ITEM_TRIGER` 추가는 **트리거 설계의 것이므로 건드리지 않는다.**

- [ ] **Step 3: 스펙 상태를 갱신하고 커밋한다**

`docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md`의 `Status: **proposed**`를 `Status: **implemented**`로 바꾼다.

```bash
git add docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md
git commit -m "docs: mark the retirement barrier queue design implemented"
```
