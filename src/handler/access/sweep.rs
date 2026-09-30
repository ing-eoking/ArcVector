use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::handler::registry;

const TICK: Duration = Duration::from_secs(1);

/// 한 라운드에 한 인덱스가 그 자리에서 drain하는 이벤트 수의 상한.
///
/// `elements.release` → `do_item_release`가 unlink 콜백을 재진입시켜 같은 큐에
/// 새 이벤트를 밀어 넣을 수 있다(`retire.rs`). 이 상한이 없으면 delete 압력이
/// 몰리는 인덱스 하나의 안쪽 루프가 한 라운드보다 훨씬 오래 돌 수 있고, 그동안
/// 다른 모든 인덱스의 `retry_stuck()`이 이 루프 뒤에서 기다린다.
const RECLAIM_BATCH: usize = 64;

/// 훑기를 기다리는 만료 키의 상한.
///
/// 넘치면 버린다. 놓친 키의 벡터는 그래프에 그대로 남아 있으니 다음 검색이 그
/// 노드를 다시 밟을 때 또 올라온다 -- 여기서 무한정 받아주면 검색이 만드는
/// 만큼 데몬의 메모리가 늘어난다.
const REAP_QUEUE: usize = 1024;

struct Sweeper {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    /// 종이 울렸다. 훑을 것이 있다는 표시.
    ///
    /// 뮤텍스 안에서 세우는 것이 요점이다. 밖에서 `notify_one`만 하면 sweeper가
    /// "일 없음"을 확인하고 `wait`에 들어가기 전에 울린 알림이 사라진다.
    pending: bool,

    /// 검색이 밟고 지나간 만료 벡터의 키.
    ///
    /// **주소가 아니라 키다.** 여기 올라온 뒤 훑기가 집어 가기까지 한 틱이
    /// 비는데, 그 사이에 `vdel`이나 다른 무엇이 같은 아이템을 unlink하고 참조를
    /// 반납해 버릴 수 있다. 주소를 들고 있었다면 그때부터 해제된 메모리를
    /// 가리킨다. 키는 낡아도 `get`이 없다고 답할 뿐이다.
    expired: Vec<String>,
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

/// 만료로 보이는 키를 훑기에 넘긴다.
///
/// 검색이 순회 중에 적어 둔 것을, 순회가 끝난 뒤에 넘긴다. **묻는 일은 여기서
/// 하지 않는다** -- 키 하나마다 `get`과 `release`로 cache lock을 두 번 잡으므로,
/// 조회 스레드가 그것을 떠안지 않도록 훑기로 옮긴 것이다.
///
/// 자리가 없으면 넘친 만큼 버린다. 빠뜨려도 그 벡터는 그래프에 남아 있고, 다음
/// 검색이 같은 노드를 밟으면 다시 올라온다.
pub(in crate::handler) fn reap_later(keys: Vec<String>) {
    {
        let mut state = state();
        if !enqueue_bounded(&mut state.expired, keys) {
            return;
        }
        state.pending = true;
    }
    SWEEPER.wake.notify_one();
}

/// 큐에 자리가 있는 만큼만 받아 넣고, 넘친 것은 버린다. 하나라도 들어갔으면
/// `true` -- 아무것도 안 들어갔는데 종을 울려 봐야 헛바퀴다.
///
/// 전역 상태에서 떼어 둔 것은 이 경계만 따로 시험하기 위해서다.
fn enqueue_bounded(queue: &mut Vec<String>, mut keys: Vec<String>) -> bool {
    let room = REAP_QUEUE.saturating_sub(queue.len());
    if room == 0 {
        return false;
    }
    keys.truncate(room);
    if keys.is_empty() || queue.try_reserve(keys.len()).is_err() {
        return false;
    }
    queue.append(&mut keys);
    true
}

/// 한 바퀴가 물어볼 몫을 큐 앞에서 떼어 낸다.
fn take_batch(queue: &mut Vec<String>) -> Vec<String> {
    let n = RECLAIM_BATCH.min(queue.len());
    queue.drain(..n).collect()
}

/// 넘겨받은 키를 엔진에 물어본다. 묻는 것이 곧 정리다.
///
/// `get`이 `do_item_isvalid`를 돌리고, 거기서 나는 `do_item_unlink`가
/// `EVENT_UNLINK`로 돌아와 `on_unlink`이 노드를 빼고 참조를 반납한다. `vdel`이
/// 지나가는 그 길이라 여기에 따로 셈할 것이 없다.
///
/// 살아 있는 것으로 밝혀지면 `get`이 그냥 돌려주고 끝난다. 이미 사라졌으면
/// 없다고 답한다. 둘 다 이 함수가 할 일이 없다는 뜻이다.
///
/// 남았으면 `true`를 돌려 다음 바퀴가 `TICK`을 기다리지 않게 한다.
fn reap_round() -> bool {
    let batch = take_batch(&mut state().expired);
    if batch.is_empty() {
        return false;
    }

    // 엔진에 닿지 못하면 이번 바퀴는 거른다. 키는 이미 큐에서 뺐지만 그
    // 벡터들은 그래프에 그대로라 다음 검색이 다시 올려준다.
    let Some(store) = crate::handler::arcus::engine::Store::background() else {
        return false;
    };
    for key in &batch {
        let _ = store.touch_kv(key);
    }

    !state().expired.is_empty()
}

fn state() -> MutexGuard<'static, State> {
    SWEEPER.state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run() {
    let mut last_round = Instant::now();
    loop {
        {
            let mut state = state();
            if !state.pending {
                state = SWEEPER
                    .wake
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            // 훑기 전에 내린다. 훑는 도중에 울린 종은 플래그를 다시 세우므로
            // 다음 바퀴가 집어간다 -- 헛도는 경우는 있어도 빠뜨리지 않는다.
            state.pending = false;
        }

        // 버려지는 중인 그래프도 훑는다. 그쪽에도 놓아줄 주소가 쌓인다.
        let indexes = registry::all();

        // 종이 울렸든 틱이 왔든, 놓아줄 것은 매 바퀴 놓아준다.
        //
        // `RECLAIM_BATCH`로 인덱스당 한도를 둔다. 한도에 걸려 루프를 그만둔
        // 것은 아직 남았을 수 있다는 뜻이므로, `pending`을 세워 다음 바퀴가
        // `TICK`을 기다리지 않고 바로 이어받게 한다.
        for index in &indexes {
            if drain_bounded(|| index.ann.drain_retired()) {
                state().pending = true;
            }
            // 비웠으면 다시 조회를 받는다. 잠긴 동안 그 인덱스는 통째로
            // `SERVER_ERROR`이므로 늦게 풀 이유가 없다.
            index.ann.resume_if_drained();
        }

        // 검색이 두고 간 만료 키를 물어본다. 놓아주는 일보다 뒤에 둔다 --
        // 이쪽이 일으키는 unlink가 위 큐에 새 주소를 밀어 넣으므로, 먼저 비워
        // 두는 편이 한 바퀴에 더 흘러간다.
        if reap_round() {
            state().pending = true;
        }

        // 종을 울려줄 주체가 없는 일만 틱 주기로 남긴다.
        if last_round.elapsed() >= TICK {
            last_round = Instant::now();
            for index in &indexes {
                index.ann.retry_stuck();

                // 복구를 못 맡긴 인덱스를 다시 맡긴다. 풀의 대기열이 가득 차
                // 있었을 수 있는데, `BUILDING`으로 남으면 그 이름은 어떤 명령에도
                // 안 보이므로 여기서 집어주지 않으면 영영 안 올라온다.
                if index.state() == registry::BUILDING {
                    crate::trigger::recover::submit(index);
                } else if crate::trigger::waiting::pending(&index.name) {
                    // 링크 콜백이 적어 두고 풀에 넘기지 못한 주소들. 서빙
                    // 중인 인덱스는 복구가 다시 맡아주지 않으므로, 여기가
                    // 그것들이 그래프에 들어가는 마지막 길이다.
                    crate::trigger::recover::submit_adopt(index);
                }
            }

            // 다 비운 그래프를 버린다. 비었다는 것은 그 인덱스의 아이템을
            // 하나도 안 쥐고 있다는 뜻이라, 더 답해줄 것이 없다.
            registry::reap_drained();
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

    fn keys(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("k{i}")).collect()
    }

    #[test]
    fn the_expired_queue_takes_what_fits_and_drops_the_rest() {
        let mut queue = Vec::new();
        assert!(enqueue_bounded(&mut queue, keys(REAP_QUEUE - 1)));
        assert_eq!(queue.len(), REAP_QUEUE - 1);

        // 넘치는 만큼만 잘라 받는다. 못 받은 벡터는 그래프에 남아 있으니 다음
        // 검색이 다시 올려준다 -- 여기서 무한정 받으면 데몬 메모리가 검색을
        // 따라 늘어난다.
        assert!(enqueue_bounded(&mut queue, keys(10)));
        assert_eq!(queue.len(), REAP_QUEUE, "상한을 넘겨 받지 않는다");

        assert!(
            !enqueue_bounded(&mut queue, keys(1)),
            "가득 찼으면 안 받았다고 답해야 종을 헛울리지 않는다"
        );
        assert_eq!(queue.len(), REAP_QUEUE);
    }

    #[test]
    fn an_empty_handoff_is_not_worth_a_bell() {
        let mut queue = Vec::new();
        assert!(!enqueue_bounded(&mut queue, Vec::new()));
        assert!(queue.is_empty());
    }

    #[test]
    fn a_round_takes_at_most_one_batch_and_leaves_the_rest_in_order() {
        let mut queue = keys(RECLAIM_BATCH + 3);

        let first = take_batch(&mut queue);
        assert_eq!(first.len(), RECLAIM_BATCH);
        assert_eq!(first[0], "k0", "큐 앞에서 뗀다");
        assert_eq!(queue.len(), 3, "남은 것은 다음 바퀴 몫이다");

        let second = take_batch(&mut queue);
        assert_eq!(second, keys(RECLAIM_BATCH + 3)[RECLAIM_BATCH..]);
        assert!(queue.is_empty());
        assert!(take_batch(&mut queue).is_empty());
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
