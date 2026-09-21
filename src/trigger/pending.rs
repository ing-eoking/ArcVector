//! 메타가 아직 안 온 인덱스의 벡터를 붙잡아 두는 곳.
//!
//! 복제본에는 `vcreate`가 오지 않는다. 오는 것은 아이템뿐이고, 벡터
//! `arcus_event{idx}:<id>`가 메타 `arcus_event{idx}:`보다 먼저 도착할 수 있다.
//! 레지스트리에 인덱스가 없다고 그 링크를 흘려보내면 그 벡터는 나중에 메타가
//! 와도 그래프에 없다.
//!
//! 그래서 주소만 여기 적어두고, 메타가 오면 그때 그래프를 세워 쭉 넣는다.
//! 엔진이 LINK에서 `ret`과 무관하게 `ITEM_REFCOUNT_INCR`를 하므로 참조는 이미
//! 우리 것이다 -- 적어둘 것은 주소뿐이다.
//!
//! # 복구 스레드와 unlink 콜백을 가르는 것
//!
//! 복구가 주소 하나를 그래프에 넣는 동안 그 아이템의 unlink가 올 수 있다. 여기서
//! 배리어(`retire.rs`)를 쓸 필요는 없다 -- 슬롯의 상태가 이미 둘을 갈라놓는다:
//!
//! | 상태 | 뜻 | unlink가 오면 |
//! |---|---|---|
//! | `Pending` | 아직 아무도 안 집었다 | `Dropped`로 두고 **거절** -- 그래프에 없으니 엔진이 회수한다 |
//! | `Claimed` | 복구가 넣는 중이다 | `Dropped`로 두고 **받는다** -- 뒷정리는 복구가 한다 |
//! | `Done` | 그래프에 들어갔다 | 평소 경로(`unlink_at` + `retire_one`) |
//!
//! `Claimed`인 동안에는 아무도 그 주소를 놓아주지 않으므로, 복구가 락 없이
//! 아이템을 역참조해도 안전하다. 복구는 넣은 **뒤에** 상태를 다시 보고,
//! `Dropped`가 돼 있으면 방금 넣은 노드를 도로 뺀다. 넣고 나서 확인하는 순서라
//! 빠지는 창이 없다.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

/// 모든 인덱스를 통틀어 붙잡아 둘 수 있는 주소 수.
///
/// 정상적으로는 거의 비어 있다. 메타는 `vcreate`가 벡터보다 **먼저** 쓰므로
/// 복제도 재생도 그 순서로 도착하고, 여기 쌓이는 것은 그 사이의 창뿐이다.
/// 이 수에 닿는다는 것은 메타가 영영 안 오는 키가 있다는 뜻이라, 상한은
/// 메모리를 지키는 쪽으로만 잡는다.
const MAX: usize = 65_536;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// 적어뒀고 아직 아무도 안 집었다.
    Pending,
    /// 복구가 그래프에 넣는 중이다.
    Claimed,
    /// 넣기 전에 빠졌다. 복구가 넣었다면 도로 빼야 한다.
    Dropped,
    /// 그래프에 들어갔다. 이제부터는 평소 경로가 맡는다.
    Done,
}

#[derive(Default)]
struct Waiting {
    /// 도착 순서. 복구가 이 순서로 훑는다.
    order: Vec<u64>,
    state: HashMap<u64, Slot>,
    /// 자리가 없어 흘려보낸 주소가 있다.
    incomplete: bool,
}

#[derive(Default)]
struct Store {
    by_index: HashMap<String, Waiting>,
    held: usize,
}

static PENDING: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::default()));

fn store() -> MutexGuard<'static, Store> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// unlink 콜백이 무엇을 해야 하는지.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unlinked {
    /// 그래프에 들어간 적이 없다. 거절하면 엔진이 참조를 회수한다.
    NeverLinked,
    /// 복구가 넣는 중이다. 받아두면 복구가 도로 뺀다.
    Recovering,
    /// 여기 없다. 평소 경로가 맡는다.
    NotWaiting,
}

/// 주소를 붙잡아 둔다. 자리가 없으면 `false`.
pub(crate) fn push(index: &str, addr: u64) -> bool {
    let mut store = store();
    if store.held >= MAX {
        if let Some(waiting) = store.by_index.get_mut(index) {
            waiting.incomplete = true;
        }
        return false;
    }
    store.held += 1;
    let waiting = store.by_index.entry(index.to_owned()).or_default();
    waiting.order.push(addr);
    waiting.state.insert(addr, Slot::Pending);
    true
}

/// `cursor`부터 아직 `Pending`인 주소 하나를 집어 `Claimed`로 바꾼다.
///
/// `cursor`는 훑은 자리까지 올라간다. 복구 스레드 하나만 부른다.
pub(crate) fn claim(index: &str, cursor: &mut usize) -> Option<u64> {
    let mut store = store();
    let waiting = store.by_index.get_mut(index)?;
    while let Some(&addr) = waiting.order.get(*cursor) {
        *cursor += 1;
        if waiting.state.get(&addr) == Some(&Slot::Pending) {
            waiting.state.insert(addr, Slot::Claimed);
            return Some(addr);
        }
    }
    None
}

/// 그래프에 넣고 나서 부른다. 그대로 둬도 되면 `true`.
///
/// `false`면 넣는 사이에 unlink가 지나갔다는 뜻이라, 부른 쪽이 노드를 도로 빼고
/// 참조를 돌려줘야 한다.
pub(crate) fn finish(index: &str, addr: u64) -> bool {
    let mut store = store();
    let Some(waiting) = store.by_index.get_mut(index) else {
        return false;
    };
    match waiting.state.get(&addr) {
        Some(Slot::Claimed) => {
            waiting.state.insert(addr, Slot::Done);
            true
        }
        _ => false,
    }
}

/// unlink 콜백이 부른다. 이 주소를 어떻게 할지 답한다.
pub(crate) fn unlink(index: &str, addr: u64) -> Unlinked {
    let mut store = store();
    let Some(waiting) = store.by_index.get_mut(index) else {
        return Unlinked::NotWaiting;
    };
    match waiting.state.get(&addr) {
        Some(Slot::Pending) => {
            waiting.state.insert(addr, Slot::Dropped);
            Unlinked::NeverLinked
        }
        Some(Slot::Claimed) => {
            waiting.state.insert(addr, Slot::Dropped);
            Unlinked::Recovering
        }
        _ => Unlinked::NotWaiting,
    }
}

/// 이 인덱스의 목록을 버린다. 자리가 없어 흘려보낸 것이 있었으면 `true`.
///
/// 복구가 끝나고 부른다. 이 시점에는 인덱스가 이미 레지스트리에 있어서 새로
/// 들어오는 링크가 여기로 오지 않는다 -- 링크 콜백은 전부 엔진의 cache lock
/// 아래에서 직렬화되고, 레지스트리 등록은 메타 콜백이 그 락 안에서 끝냈다.
pub(crate) fn forget(index: &str) -> bool {
    let mut store = store();
    let Some(waiting) = store.by_index.remove(index) else {
        return false;
    };
    store.held = store.held.saturating_sub(waiting.order.len());
    waiting.incomplete
}

/// 아직 붙잡고 있는 주소 수. 진단용.
pub(crate) fn held() -> usize {
    store().held
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) {
        forget(name);
    }

    #[test]
    fn a_vector_that_arrives_before_its_metadata_is_kept() {
        let ix = "pending-kept";
        fresh(ix);
        assert!(push(ix, 0x10));
        assert!(push(ix, 0x20));

        let mut cursor = 0;
        assert_eq!(claim(ix, &mut cursor), Some(0x10));
        assert_eq!(claim(ix, &mut cursor), Some(0x20));
        assert_eq!(claim(ix, &mut cursor), None, "두 개뿐이었다");
        forget(ix);
    }

    #[test]
    fn unlinking_before_anyone_claims_it_hands_the_reference_back() {
        let ix = "pending-early-unlink";
        fresh(ix);
        push(ix, 0x10);

        assert_eq!(unlink(ix, 0x10), Unlinked::NeverLinked);

        let mut cursor = 0;
        assert_eq!(
            claim(ix, &mut cursor),
            None,
            "빠진 주소를 복구가 다시 집으면 해제된 아이템을 그래프에 넣는다"
        );
        forget(ix);
    }

    #[test]
    fn unlinking_while_recovery_holds_it_leaves_the_cleanup_to_recovery() {
        let ix = "pending-late-unlink";
        fresh(ix);
        push(ix, 0x10);
        let mut cursor = 0;
        assert_eq!(claim(ix, &mut cursor), Some(0x10));

        // 복구가 넣는 중에 unlink가 왔다. 여기서 거절하면 엔진이 해제해 버리는데
        // 복구는 그 주소를 역참조하고 있다.
        assert_eq!(unlink(ix, 0x10), Unlinked::Recovering);
        assert!(
            !finish(ix, 0x10),
            "복구는 방금 넣은 노드를 도로 빼야 한다는 답을 받아야 한다"
        );
        forget(ix);
    }

    #[test]
    fn a_vector_already_in_the_graph_falls_through_to_the_usual_path() {
        let ix = "pending-done";
        fresh(ix);
        push(ix, 0x10);
        let mut cursor = 0;
        claim(ix, &mut cursor);
        assert!(finish(ix, 0x10));

        assert_eq!(unlink(ix, 0x10), Unlinked::NotWaiting);
        forget(ix);
    }

    #[test]
    fn a_name_nobody_is_waiting_on_is_not_ours_to_answer_for() {
        assert_eq!(unlink("pending-unknown", 0x10), Unlinked::NotWaiting);
    }

    #[test]
    fn forgetting_gives_the_room_back() {
        let ix = "pending-room";
        fresh(ix);
        let before = held();
        push(ix, 0x10);
        push(ix, 0x20);
        assert_eq!(held(), before + 2);

        assert!(!forget(ix), "흘려보낸 것이 없었다");
        assert_eq!(held(), before, "자리를 돌려주지 않으면 상한이 새어나간다");
    }
}
