#![allow(dead_code)] // 다음 Task들이 채운다.

//! 아이템 참조를 언제 엔진에 돌려줘도 되는지를 큐 순서로 답한다.
//!
//! 검색과 retire를 같은 FIFO에 넣으면 "누가 먼저였나"가 큐 순서 그 자체가 된다.
//! `Release`보다 앞에 있는 배리어는 그 주소가 `held`에서 빠지기 전에 시작한
//! 검색이므로 막아야 하고, 뒤에 있는 배리어는 빠진 뒤에 시작해 `resolve()`의
//! `held.contains()`에 걸리므로 막을 이유가 없다.
//!
//! 자세한 논증은 `docs/superpowers/specs/2026-09-16-arcvector-retire-barrier-queue-design.md`.

use crate::handler::usearch::held::Elements;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::error::{Error, Result};

/// 큐가 이만큼 차면 게이트를 닫는다. 고수위 표시이지 상한이 아니다.
pub(super) const CAP: usize = 1024;

/// 배리어 슬롯 수. 게이트가 `CAP`에서 닫힌 뒤 배리어는 최대 하나 더 생긴다.
pub(super) const POOL: usize = CAP + 2;

/// 최상위 비트. 이 배리어는 봉인되어 새 검색을 받지 않는다.
const SEALED: usize = 1 << (usize::BITS - 1);

/// 나머지 비트. 붙어 있는 검색 수.
const COUNT: usize = !SEALED;

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
    gated: bool,
}

impl Queue {
    /// 자리가 났으면 게이트를 연다. 푸는 것은 `drain_once`가 한다.
    fn reopen_if_room(&mut self, cap: usize) -> bool {
        if self.gated && self.events.len() < cap {
            self.gated = false;
            return true;
        }
        false
    }

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

/// `drain_once` 한 번이 무엇을 했는지.
pub(super) enum Progress {
    /// 항목 하나를 처리했다. 값은 놓아준 주소 수(배리어면 0).
    Released(usize),
    /// 머리의 배리어가 아직 안 풀렸다.
    Blocked,
    /// 큐가 비었다.
    Empty,
}

/// 게이트가 닫혀 있을 때 검색이 기다리는 시간. 넘으면 거절한다.
const GATE_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct Retirement {
    slots: Box<[AtomicUsize]>,
    /// 지금 열린 슬롯 번호. retire만 쓰고 검색은 읽기만 한다.
    open: AtomicUsize,
    queue: Mutex<Queue>,
    gate: Condvar,
    cap: usize,
    bell: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl Retirement {
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
            bell: OnceLock::new(),
        }
    }

    /// sweeper를 깨우는 방법을 알려준다. 등록 전에는 아무것도 하지 않는다.
    pub(super) fn set_bell(&self, bell: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.bell.set(bell);
    }

    /// 종을 울린다. 봉인된 배리어를 0으로 만든 검색만 부른다.
    ///
    /// 알림 유실을 막는 것은 여기가 아니라 종 자체의 몫이다 -- sweeper가 기다리는
    /// 뮤텍스는 이 큐의 것이 아니라 sweeper 자신의 것이라, 여기서 큐 락을 거쳐봐야
    /// 아무것도 지켜주지 않는다. `sweep::wake`가 그 뮤텍스 안에서 플래그를 세운다.
    fn ring(&self) {
        let Some(bell) = self.bell.get() else { return };
        bell();
    }

    /// 검색 하나를 지금 열린 배리어에 붙인다.
    ///
    /// 봉인 중이면 물러났다가 `open`을 다시 읽는다. 물러나는 것은 `held`를 읽기
    /// 전이므로 그 검색은 아무 주소도 보지 못했다.
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
        let q = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
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

    #[cfg(test)]
    pub(super) fn open_slot(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn slot_count(&self, slot: usize) -> usize {
        self.slots[slot].load(Ordering::Acquire) & COUNT
    }

    /// 열린 슬롯을 봉인하고 다음 칸을 연다. 테스트에서 retire 없이 쓴다.
    ///
    /// 슬롯 넘김의 진짜 규칙은 `retire()`에 있다.
    #[cfg(test)]
    pub(super) fn seal_open(&self) -> usize {
        let slot = self.open.load(Ordering::Acquire);
        let live = seal(&self.slots[slot]);
        self.open.store((slot + 1) % POOL, Ordering::Release);
        live
    }

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
                    if q.reopen_if_room(self.cap) {
                        self.gate.notify_all();
                    }
                    return Progress::Released(0);
                }

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
            }
        };

        elements.release(&batch);
        Progress::Released(batch.len())
    }

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
        q.gated = q.events.len() >= self.cap;
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
        let prev = self.owner.slots[self.slot].fetch_sub(1, Ordering::AcqRel);
        if prev & COUNT == 1 && prev & SEALED != 0 {
            // 내가 마지막이고, 이 배리어는 큐에서 sweeper를 세우고 있다.
            self.owner.ring();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::Ordering;

    use crate::handler::usearch::held::Elements;

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

        let reading = r.enter().unwrap();
        r.retire(&[0x10]);

        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        assert!(e.freed().is_empty(), "앞선 검색이 살아 있으면 못 놓아준다");

        drop(reading);
        assert!(
            matches!(r.drain_once(&e), Progress::Released(0)),
            "배리어를 버린다"
        );
        assert!(
            matches!(r.drain_once(&e), Progress::Released(1)),
            "그 다음이 릴리스"
        );
        assert_eq!(e.freed(), vec![0x10]);
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

        let reading = r.enter().unwrap();
        let slot = r.open_slot();
        r.retire(&[0x10]);
        drop(reading);

        assert!(matches!(r.drain_once(&e), Progress::Released(0)));

        // 카운트가 0인 것만으로는 부족하다 -- slot_count가 SEALED를 가리므로,
        // fetch_and를 아예 안 해도 통과한다. 봉인이 실제로 풀렸는지 본다.
        assert!(
            attach(&r.slots[slot]),
            "봉인이 안 풀렸다. 이 칸이 다시 열리면 enter()가 영영 돈다"
        );
        r.slots[slot].fetch_sub(1, Ordering::AcqRel);
        assert_eq!(r.slot_count(slot), 0, "깨끗하게 되돌아왔다");
    }

    #[test]
    fn a_barrier_behind_a_release_does_not_block_it() {
        let r = Retirement::new();
        let e = FakeElements::default();

        // 먼저 붙은 검색이 첫 배리어를 만든다.
        let first = r.enter().unwrap();
        r.retire(&[0x10]);

        // 이 검색은 0x10이 held에서 빠진 뒤에 시작했으므로 그 주소에 닿을 수 없고,
        // 그래서 두 번째 배리어에 붙는다 -- Rel(0x10) 뒤에.
        let late = r.enter().unwrap();
        r.retire(&[0x20]);
        assert_eq!(r.queue_shape(), vec!['B', 'R', 'B', 'R']);

        drop(first);
        assert!(
            matches!(r.drain_once(&e), Progress::Released(0)),
            "첫 배리어가 풀렸다"
        );
        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(
            e.freed(),
            vec![0x10],
            "늦게 시작한 검색이 아직 도는데도 첫 릴리스는 통과한다"
        );

        // 그 뒤의 것은 late가 막고 있다.
        assert!(matches!(r.drain_once(&e), Progress::Blocked));
        drop(late);
        assert!(matches!(r.drain_once(&e), Progress::Released(0)));
        assert!(matches!(r.drain_once(&e), Progress::Released(1)));
        assert_eq!(e.freed(), vec![0x10, 0x20]);
    }

    #[test]
    fn a_search_holds_the_open_slot_until_it_ends() {
        let r = Retirement::new();
        let slot = r.open_slot();
        assert_eq!(r.slot_count(slot), 0);

        let reading = r.enter().unwrap();
        assert_eq!(r.slot_count(slot), 1, "검색이 붙었다");

        drop(reading);
        assert_eq!(r.slot_count(slot), 0, "검색이 떨어졌다");
    }

    #[test]
    fn two_searches_share_one_slot() {
        let r = Retirement::new();
        let slot = r.open_slot();

        let a = r.enter().unwrap();
        let b = r.enter().unwrap();
        assert_eq!(r.slot_count(slot), 2);

        drop(a);
        assert_eq!(r.slot_count(slot), 1);
        drop(b);
        assert_eq!(r.slot_count(slot), 0);
    }

    #[test]
    fn sealing_returns_the_count_at_that_moment() {
        let r = Retirement::new();
        let a = r.enter().unwrap();
        let b = r.enter().unwrap();

        assert_eq!(r.seal_open(), 2, "봉인 순간의 카운트를 정확히 돌려준다");
        drop(a);
        drop(b);
    }

    #[test]
    fn a_search_that_meets_a_sealed_slot_goes_to_the_next_one() {
        let r = Retirement::new();
        let first = r.open_slot();

        let held_open = r.enter().unwrap(); // 봉인이 0을 돌려주지 않도록 하나 붙여둔다
        assert_eq!(r.seal_open(), 1);

        let late = r.enter().unwrap();
        assert_ne!(
            r.open_slot(),
            first,
            "봉인된 슬롯에는 못 붙으므로 다음 칸으로 넘어갔다"
        );
        assert_eq!(
            r.slot_count(first),
            1,
            "봉인된 칸에는 먼저 붙은 하나만 남는다"
        );
        assert_eq!(
            r.slot_count(r.open_slot()),
            1,
            "늦게 온 검색은 새 칸에 붙었다"
        );

        drop(late);
        drop(held_open);
    }

    #[test]
    fn attaching_to_a_sealed_slot_backs_out_without_disturbing_the_count() {
        let r = Retirement::new();
        let slot = r.open_slot();

        let holder = r.enter().unwrap(); // 먼저 붙은 검색 하나
        assert_eq!(seal(&r.slots[slot]), 1, "봉인 순간의 카운트");

        // 봉인된 칸에 붙으려는 검색은 물러난다. enter()로는 이 경로를 못 밟는다 --
        // open이 안 넘어간 채로 부르면 영영 돈다.
        assert!(!attach(&r.slots[slot]), "봉인돼 있으면 붙지 못한다");
        assert_eq!(
            r.slot_count(slot),
            1,
            "물러난 검색이 카운트를 남기지도, 먼저 붙은 하나를 지우지도 않았다"
        );

        drop(holder);
        assert_eq!(r.slot_count(slot), 0, "언더플로 없이 0으로 돌아온다");
    }

    #[test]
    fn a_retire_with_no_search_attached_queues_no_barrier() {
        let r = Retirement::new();
        let slot = r.open_slot();

        r.retire(&[0x10]);
        r.retire(&[0x20]);

        assert_eq!(
            r.queue_shape(),
            vec!['R'],
            "붙은 검색이 없으면 배리어가 없다"
        );
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
        assert_eq!(
            r.open_slot(),
            slot,
            "배리어가 큐에 안 들어가면 슬롯은 제자리다"
        );
    }

    #[test]
    fn a_retire_with_a_search_attached_queues_a_barrier_first() {
        let r = Retirement::new();
        let slot = r.open_slot();
        let reading = r.enter().unwrap();

        r.retire(&[0x10]);

        assert_eq!(
            r.queue_shape(),
            vec!['B', 'R'],
            "배리어가 릴리스보다 앞이다"
        );
        assert_ne!(
            r.open_slot(),
            slot,
            "배리어가 큐에 들어갔으므로 슬롯이 넘어갔다"
        );
        drop(reading);
    }

    #[test]
    fn concurrent_retires_never_put_two_barriers_side_by_side() {
        // 봉인과 push가 한 덩어리가 아니면 [B, B, R, R]처럼 배리어가 붙어 나오고,
        // 그러면 두 번째 배리어 뒤의 릴리스를 첫 배리어가 안 막게 된다.
        let r = Arc::new(Retirement::new());
        let _reading = r.enter().unwrap(); // 모든 retire가 배리어를 만들도록 하나 붙여둔다

        std::thread::scope(|s| {
            for t in 0..4u64 {
                let r = Arc::clone(&r);
                s.spawn(move || {
                    for i in 0..50u64 {
                        let _hold = r.enter().unwrap();
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
        let first = r.enter().unwrap();
        r.retire(&[0x10]);
        let second = r.enter().unwrap();

        r.retire(&[0x20]);

        assert_eq!(
            r.queue_shape(),
            vec!['B', 'R', 'B', 'R'],
            "두 번째 검색은 두 번째 배리어에 붙어 첫 릴리스를 막지 않는다"
        );
        drop(first);
        drop(second);
    }

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
}
