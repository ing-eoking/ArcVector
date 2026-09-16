use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::registry::{self, VectorIndex};

const TICK: Duration = Duration::from_secs(1);

/// 한 라운드에 한 인덱스가 그 자리에서 drain하는 이벤트 수의 상한.
///
/// `elements.release` → `do_item_release`가 unlink 콜백을 재진입시켜 같은 큐에
/// 새 이벤트를 밀어 넣을 수 있다(`retire.rs`). 이 상한이 없으면 delete 압력이
/// 몰리는 인덱스 하나의 안쪽 루프가 한 라운드보다 훨씬 오래 돌 수 있고, 그동안
/// 다른 모든 인덱스의 `retry_stuck()`이 이 루프 뒤에서 기다린다.
const RECLAIM_BATCH: usize = 64;

struct Sweeper {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    retired: Vec<Arc<VectorIndex>>,
    /// 종이 울렸다. 훑을 것이 있다는 표시.
    ///
    /// 뮤텍스 안에서 세우는 것이 요점이다. 밖에서 `notify_one`만 하면 sweeper가
    /// "일 없음"을 확인하고 `wait`에 들어가기 전에 울린 알림이 사라진다.
    pending: bool,
}

static SWEEPER: LazyLock<Sweeper> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("arcvector-sweep".to_owned())
        .spawn(run)
        .expect("spawn the sweeper thread");
    Sweeper {
        state: Mutex::new(State::default()),
        wake: Condvar::new(),
    }
});

/// Starts the sweeper if it is not running. Called when an index enters the
/// registry, which is the first moment there is anything to sweep -- and it has
/// to be then rather than at load time, because `-d` forks after the extensions
/// load and no thread survives that.
pub(in crate::handler) fn ensure_sweeper() {
    LazyLock::force(&SWEEPER);
}

/// sweeper를 깨운다. 봉인된 배리어를 푼 검색이 부른다.
pub(in crate::handler) fn wake() {
    state().pending = true;
    SWEEPER.wake.notify_one();
}

fn state() -> MutexGuard<'static, State> {
    SWEEPER.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Hands a graph no longer in the registry to the sweeper to drop.
///
/// Freeing one returns every element it holds to the engine, which is work a
/// request should not be made to wait through.
pub(in crate::handler) fn retire(evicted: Option<Arc<VectorIndex>>) {
    let Some(index) = evicted else { return };
    let mut state = state();
    if state.retired.try_reserve(1).is_err() {
        drop(state);
        drop(index);
        return;
    }
    state.retired.push(index);
    SWEEPER.wake.notify_one();
}

fn run() {
    let mut last_round = Instant::now();
    loop {
        let retired = {
            let mut state = state();
            if state.retired.is_empty() && !state.pending {
                state = SWEEPER
                    .wake
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            // 훑기 전에 내린다. 훑는 도중에 울린 종은 플래그를 다시 세우므로
            // 다음 바퀴가 집어간다 -- 헛도는 경우는 있어도 빠뜨리지 않는다.
            state.pending = false;
            std::mem::take(&mut state.retired)
        };

        drop(retired);

        let indexes = registry::indexes().unwrap_or_default();

        // 종이 울렸든 틱이 왔든, 놓아줄 것은 매 바퀴 놓아준다.
        //
        // `RECLAIM_BATCH`로 인덱스당 한도를 둔다. 한도에 걸려 루프를 그만둔
        // 것은 아직 남았을 수 있다는 뜻이므로, `pending`을 세워 다음 바퀴가
        // `TICK`을 기다리지 않고 바로 이어받게 한다.
        for index in &indexes {
            if drain_bounded(|| index.ann.drain_retired()) {
                state().pending = true;
            }
        }

        // 종을 울려줄 주체가 없는 일만 틱 주기로 남긴다.
        if last_round.elapsed() >= TICK {
            last_round = Instant::now();
            for index in &indexes {
                index.ann.retry_stuck();
            }
        }
    }
}

/// `drain_once`를 최대 `RECLAIM_BATCH`번 부른다. 한도에 걸려 멈췄으면 `true` --
/// 그 인덱스에 아직 더 남아 있을 수 있다는 뜻이다.
///
/// 로직을 클로저로 뽑아 둔 것은 전역 스레드·레지스트리 없이 이 경계 자체를
/// 테스트하기 위해서다.
fn drain_bounded(mut drain_once: impl FnMut() -> bool) -> bool {
    let mut n = 0;
    while n < RECLAIM_BATCH && drain_once() {
        n += 1;
    }
    n == RECLAIM_BATCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_stops_at_the_batch_bound_even_if_work_keeps_reappearing() {
        // `elements.release`가 재진입시킨 unlink 콜백이 같은 큐에 계속 새
        // 이벤트를 밀어 넣을 수 있다(retire.rs) -- `drain_once`가 영원히 진행을
        // 보고해도 다른 모든 인덱스를 굶기면 안 된다.
        let calls = std::cell::Cell::new(0usize);
        let hit_bound = drain_bounded(|| {
            calls.set(calls.get() + 1);
            true
        });

        assert!(
            hit_bound,
            "한도에 걸린 것을 보고해야 다음 바퀴가 바로 이어받는다"
        );
        assert_eq!(calls.get(), RECLAIM_BATCH, "정확히 한도에서 멈춘다");
    }

    #[test]
    fn draining_stops_early_when_the_queue_runs_out_before_the_bound() {
        let calls = std::cell::Cell::new(0usize);
        let hit_bound = drain_bounded(|| {
            let n = calls.get() + 1;
            calls.set(n);
            n < 5
        });

        assert!(!hit_bound, "한도 전에 큐가 비었으면 더 남은 게 없다");
        assert_eq!(calls.get(), 5);
    }
}
