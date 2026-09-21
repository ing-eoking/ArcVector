//! 인덱스를 잠시 못 쓰게 잠그는 장치.
//!
//! sweeper의 큐에 넣지 못하면 그래프와 저장소가 어긋난 채로 남는다. 그래프가
//! 지워진 벡터를 들고 있거나, 우리 것이 아닌 포인터를 가리키게 된다. 그 상태로
//! 계속 답하면 틀린 id를 내놓거나 해제된 메모리를 읽는다.
//!
//! 그래서 걸어 잠그고 `SERVER_ERROR`로 답하다가, sweeper가 밀린 것을 어느 정도
//! 비우면 푼다. **이미 돌고 있던 검색도 함께 끊긴다** -- 그게 이 설계에서 엔진의
//! 강제 해제를 안전하게 만드는 장치다.

use std::sync::atomic::{AtomicUsize, Ordering};

/// 최상위 비트. 이 인덱스는 잠겨 있다.
const HALTED: usize = 1 << (usize::BITS - 1);

/// 나머지 비트. 지금 아이템을 역참조하고 있는 수.
// 트리거 핸들러(잠금)와 역참조 지점(가드), sweeper(해제)가 아직 안 붙었다.
#[allow(dead_code)]
const COUNT: usize = !HALTED;

/// 잠금 비트와 진행 중인 역참조 수를 **한 워드에** 담는다.
///
/// 따로 두면 확인과 역참조 사이에 창이 남는다 -- 그 사이에 엔진이 아이템을
/// 해제하면 use-after-free다. `fetch_add`의 반환값에 잠금 비트가 같이 실려 오니,
/// 확인과 등록이 한 연산이라 창이 없다. 배리어 슬롯과 같은 수법이다.
pub(crate) struct Halt {
    state: AtomicUsize,
}

impl Halt {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    /// 아이템 역참조를 시작한다. 잠겨 있으면 `None`.
    ///
    /// 돌려받은 가드가 살아 있는 동안 `halt()`는 돌아오지 않는다. 즉 가드를 쥔
    /// 채로는 엔진이 그 아이템을 해제하지 않는다.
    #[allow(dead_code)] // 역참조 지점이 아직 안 붙었다.
    pub(crate) fn touch(&self) -> Option<Touching<'_>> {
        let prev = self.state.fetch_add(1, Ordering::AcqRel);
        if prev & HALTED != 0 {
            self.state.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Touching(self))
    }

    /// 잠그고, **진행 중인 역참조가 끝나기를 기다린다.**
    ///
    /// 기다리는 것은 역참조 하나뿐이라 짧다 -- 포인터를 따라가 128바이트를 읽는
    /// 시간이다. 검색 한 건이 아니다. 그래서 cache lock을 쥔 콜백에서 불러도
    /// 데몬이 서지 않는다.
    #[allow(dead_code)] // 트리거 핸들러가 아직 안 붙었다.
    pub(crate) fn halt(&self) {
        self.state.fetch_or(HALTED, Ordering::AcqRel);
        while self.state.load(Ordering::Acquire) & COUNT > 0 {
            std::hint::spin_loop();
        }
    }

    /// 다시 받는다. sweeper가 밀린 것을 비우고 나서 부른다.
    #[allow(dead_code)] // sweeper가 아직 안 부른다.
    pub(crate) fn resume(&self) {
        self.state.fetch_and(COUNT, Ordering::AcqRel);
    }

    pub(crate) fn is_halted(&self) -> bool {
        self.state.load(Ordering::Acquire) & HALTED != 0
    }

    #[cfg(test)]
    fn touching(&self) -> usize {
        self.state.load(Ordering::Acquire) & COUNT
    }
}

/// 역참조 하나가 진행 중임을 표시하는 가드.
#[must_use = "the dereference is only protected while this is alive"]
#[allow(dead_code)] // 역참조 지점이 아직 안 붙었다.
pub(crate) struct Touching<'a>(&'a Halt);

impl Drop for Touching<'_> {
    fn drop(&mut self) {
        self.0.state.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dereference_is_counted_while_it_runs() {
        let h = Halt::new();
        assert_eq!(h.touching(), 0);

        let t = h.touch().expect("안 걸렸으니 통과한다");
        assert_eq!(h.touching(), 1);

        drop(t);
        assert_eq!(h.touching(), 0);
    }

    #[test]
    fn a_halted_index_turns_every_dereference_away() {
        let h = Halt::new();
        h.halt();

        assert!(h.is_halted());
        assert!(h.touch().is_none(), "걸렸으면 역참조를 시작하지 못한다");
        assert_eq!(h.touching(), 0, "물러난 쪽이 카운트를 남기지 않는다");
    }

    #[test]
    fn resuming_lets_dereferences_back_in() {
        let h = Halt::new();
        h.halt();
        h.resume();

        assert!(!h.is_halted());
        assert!(h.touch().is_some());
    }

    #[test]
    fn halting_waits_for_the_dereferences_already_running() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let h = Arc::new(Halt::new());
        let t = h.touch().expect("먼저 하나 잡아둔다");

        let returned = Arc::new(AtomicBool::new(false));
        let (h2, seen) = (Arc::clone(&h), Arc::clone(&returned));
        let waiter = std::thread::spawn(move || {
            h2.halt();
            seen.store(true, Ordering::Release);
        });

        // 진행 중인 역참조가 있는 동안에는 halt가 돌아오면 안 된다. 돌아왔다면
        // 엔진이 그 아이템을 해제해도 된다고 믿는 것이고, 그건 use-after-free다.
        for _ in 0..50 {
            assert!(
                !returned.load(Ordering::Acquire),
                "역참조가 끝나지 않았는데 halt가 돌아왔다"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        drop(t);
        waiter.join().unwrap();
        assert!(returned.load(Ordering::Acquire));
    }
}
