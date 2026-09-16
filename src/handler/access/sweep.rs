use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::registry::{self, VectorIndex};

const TICK: Duration = Duration::from_secs(1);

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
        for index in &indexes {
            while index.ann.drain_retired() {}
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
