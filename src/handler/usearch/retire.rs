#![allow(dead_code)] // 다음 Task들이 채운다.

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

        let holder = r.enter(); // 먼저 붙은 검색 하나
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
}
