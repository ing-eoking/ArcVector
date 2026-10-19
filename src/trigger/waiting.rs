//! 링크됐지만 아직 그래프에 들어가지 않은 벡터를 붙잡아 두는 곳.
//!
//! **sweeper의 큐와는 관계가 없다.** 여기 있는 주소는 아직 어느 그래프에도
//! 들어가지 않은 것들이고, 놓아줄 주소를 모아두는 `retire.rs`의 링과는 반대
//! 방향이다 -- 저쪽은 그래프에서 빠져 엔진에 돌려줄 것을, 이쪽은 그래프에
//! 들어갈 자리를 기다리는 것을 담는다.
//!
//! 요소가 링크되면 엔진이 cache lock을 쥔 채 콜백을 부른다. 거기서 HNSW에
//! 넣으면 그동안 데몬 전체가 서므로, 콜백은 주소만 여기 적고 풀이 넣는다.
//! 엔진이 LINK에서 우리 몫의 참조를 이미 잡아뒀으니 적어둘 것은 주소뿐이다.
//! 주소가 여기 오는 길은 셋이다.
//!
//! * `vadd`/`vsetattr` -- 그 명령의 워커 스레드에서 링크된 요소. 명령이 직접
//!   풀에 넘겨 넣고 나서 답하므로 `Reserved`로 적어 다른 누구도 집지 않게 한다.
//! * 복제·복구로 들어온 요소 -- `Pending`. 인덱스의 adopt가 순서대로 집어간다.
//! * 메타보다 먼저 도착한 요소 -- 역시 `Pending`. 복제본에는 `vcreate`가 오지
//!   않고, 요소가 메타 요소보다 먼저 올 수 있다. 메타가 오면 그때 그래프를
//!   세워 쭉 넣는다.
//!
//! # 넣는 스레드와 unlink 콜백을 가르는 것
//!
//! 풀이 주소 하나를 그래프에 넣는 동안 그 요소의 unlink가 올 수 있다. 여기서
//! 배리어(`retire.rs`)를 쓸 필요는 없다 -- 슬롯의 상태가 이미 둘을 갈라놓는다:
//!
//! | 상태 | 뜻 | unlink가 오면 |
//! |---|---|---|
//! | `Pending`/`Reserved` | 아직 아무도 안 집었다 | 지우고 **거절** -- 그래프에 없으니 엔진이 회수한다 |
//! | `Claimed` | 풀이 넣는 중이다 | `Dropped`로 두고 **받는다** -- 뒷정리는 넣던 쪽이 한다 |
//! | (없음) | 그래프에 들어갔다 | 평소 경로(`unlink_at` + `retire_one`) |
//!
//! `Claimed`인 동안에는 아무도 그 주소를 놓아주지 않으므로, 넣는 쪽이 락 없이
//! 요소를 역참조해도 안전하다. 넣은 **뒤에** [`finish`]로 상태를 다시 보고,
//! `Dropped`가 돼 있으면 방금 넣은 노드를 도로 뺀다. 넣고 나서 확인하는 순서라
//! 빠지는 창이 없다. 끝난 항목은 그 자리에서 지운다 -- 그래프에 들어간 것은
//! 더 이상 이 장부의 몫이 아니다.
//!
//! # 왜 개수 상한이 없나
//!
//! 여기 적힌 주소 하나하나는 우리가 참조를 쥐고 있는 요소다. 그래서 개수
//! 상한은 자기가 막겠다는 것을 못 막는다. 상한에 걸려 주소를 버려도 **요소는
//! 그대로 붙잡혀 있다** -- LINK의 거절을 엔진이 보지 않으니 참조를 돌려줄 길이
//! 그 자리에는 없다.
//!
//! 진짜 한계는 할당기다. `try_reserve`로 물어보고 안 되면 그때 물러난다 --
//! 임의의 숫자를 정해두는 것보다 정확하고, 데몬을 죽이지 않는다.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// 적어뒀고 아직 아무도 안 집었다. adopt가 집어간다.
    Pending,
    /// 링크한 명령이 직접 집어간다. adopt는 건너뛴다.
    Reserved,
    /// 풀이 그래프에 넣는 중이다.
    Claimed,
    /// 넣는 중에 빠졌다. 넣은 쪽이 도로 빼야 한다.
    Dropped,
}

/// 한 슬롯. 주소는 `order`의 어느 칸에 있는지까지 안다.
///
/// 칸 번호를 들고 있는 것은 replace 때문이다. 자리를 찾아 그 자리에서 바꿔
/// 끼우면 목록이 자라지 않고, 자라지 않으면 실패할 일도 없다.
#[derive(Clone, Copy)]
struct Entry {
    slot: Slot,
    at: usize,
}

/// `order`에서 지워진 칸. 사용자 영역 주소는 0이 될 수 없다.
const VACANT: u64 = 0;

#[derive(Default)]
struct Waiting {
    /// 도착 순서. adopt가 이 순서로 훑는다. 지운 칸은 [`VACANT`]다.
    order: Vec<u64>,
    state: HashMap<u64, Entry>,
    /// 할당기가 거절해 흘려보낸 주소가 있다.
    incomplete: bool,
}

impl Waiting {
    fn vacate(&mut self, addr: u64) -> Option<Entry> {
        let entry = self.state.remove(&addr)?;
        self.order[entry.at] = VACANT;
        if self.state.is_empty() {
            // 다 끝났다. 칸을 처음부터 다시 쓴다 -- 그러지 않으면 쉬지 않고
            // 들어오는 쓰기마다 빈 칸이 쌓인다.
            self.order.clear();
        } else if self.order.len() > 64 && self.state.len() * 2 < self.order.len() {
            self.compact();
        }
        Some(entry)
    }

    /// 빈 칸을 걷어낸다. 제자리에서 하므로 할당하지 않는다.
    ///
    /// 돌고 있는 adopt의 커서가 이 뒤로 밀린 항목을 건너뛸 수 있지만, 그것은
    /// `Pending`으로 남아 sweeper가 다음 틱에 다시 맡긴다.
    fn compact(&mut self) {
        let mut kept = 0;
        for i in 0..self.order.len() {
            let addr = self.order[i];
            if addr == VACANT {
                continue;
            }
            self.order[kept] = addr;
            if let Some(entry) = self.state.get_mut(&addr) {
                entry.at = kept;
            }
            kept += 1;
        }
        self.order.truncate(kept);
    }
}

#[derive(Default)]
struct Store {
    by_index: HashMap<String, Waiting>,
    /// 모든 인덱스를 통틀어 붙잡고 있는 주소 수. `vstats`가 보고한다.
    count: usize,
}

impl Store {
    fn vacate(&mut self, index: &str, addr: u64) -> Option<Entry> {
        let waiting = self.by_index.get_mut(index)?;
        let entry = waiting.vacate(addr)?;
        self.count = self.count.saturating_sub(1);
        if waiting.state.is_empty() && !waiting.incomplete {
            self.by_index.remove(index);
        }
        Some(entry)
    }
}

static PENDING: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::default()));

fn store() -> MutexGuard<'static, Store> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// replace 콜백이 무엇을 해야 하는지.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Replaced {
    /// new가 old의 자리를 그대로 물려받았다.
    ///
    /// old의 참조는 아직 우리 것이고 그래프에는 없으므로, 거절해서 엔진이
    /// 회수하게 한다.
    Swapped,
    /// 풀이 old를 넣는 중이다. 뒷정리는 그쪽 몫이고, new는 따로 잡아둔다.
    Recovering,
    /// old가 여기 없다. 평소 경로가 맡는다.
    NotWaiting,
}

/// unlink 콜백이 무엇을 해야 하는지.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unlinked {
    /// 그래프에 들어간 적이 없다. 거절하면 엔진이 참조를 회수한다.
    NeverLinked,
    /// 풀이 넣는 중이다. 받아두면 넣던 쪽이 도로 뺀다.
    Recovering,
    /// 여기 없다. 평소 경로가 맡는다.
    NotWaiting,
}

/// 이 이름으로 adopt를 기다리는 주소가 있나.
///
/// sweeper가 매 틱 묻는다. 링크 콜백이 풀에 넘기지 못했을 때 그 주소들이
/// 영영 그래프 밖에 남지 않게 하는 안전망이다. `Reserved`는 세지 않는다 --
/// 그것은 명령이 직접 집어간다.
pub(crate) fn pending(index: &str) -> bool {
    store().by_index.get(index).is_some_and(|waiting| {
        waiting
            .state
            .values()
            .any(|entry| entry.slot == Slot::Pending)
    })
}

/// 주소를 adopt가 집어가도록 붙잡아 둔다. 할당기가 거절하면 `false`.
pub(crate) fn push(index: &str, addr: u64) -> bool {
    hold(index, addr, Slot::Pending)
}

/// 주소를 링크한 명령이 직접 집어가도록 붙잡아 둔다. 할당기가 거절하면 `false`.
pub(crate) fn reserve(index: &str, addr: u64) -> bool {
    hold(index, addr, Slot::Reserved)
}

fn hold(index: &str, addr: u64, slot: Slot) -> bool {
    let mut store = store();
    if !store.by_index.contains_key(index) && store.by_index.try_reserve(1).is_err() {
        return false;
    }
    let held = {
        let waiting = store.by_index.entry(index.to_owned()).or_default();
        if waiting.order.try_reserve(1).is_err() || waiting.state.try_reserve(1).is_err() {
            // 이 이름은 이제 저장소보다 모자라다. adopt가 끝날 때 그렇게 알린다.
            waiting.incomplete = true;
            false
        } else {
            let at = waiting.order.len();
            waiting.order.push(addr);
            waiting.state.insert(addr, Entry { slot, at });
            true
        }
    };
    if held {
        store.count += 1;
    }
    held
}

/// 이미 적혀 있는 `Pending` 주소를 명령 몫으로 돌린다. 돌렸으면 `true`.
///
/// replace가 자리를 물려준 새 주소를, 그 쓰기를 한 명령이 직접 집어가게 할 때
/// 쓴다.
pub(crate) fn reserve_waiting(index: &str, addr: u64) -> bool {
    let mut store = store();
    match store
        .by_index
        .get_mut(index)
        .and_then(|waiting| waiting.state.get_mut(&addr))
    {
        Some(entry) if entry.slot == Slot::Pending => {
            entry.slot = Slot::Reserved;
            true
        }
        _ => false,
    }
}

/// `cursor`부터 아직 `Pending`인 주소 하나를 집어 `Claimed`로 바꾼다.
///
/// `cursor`는 훑은 자리까지 올라간다. adopt 하나만 부른다.
pub(crate) fn claim(index: &str, cursor: &mut usize) -> Option<u64> {
    let mut store = store();
    let waiting = store.by_index.get_mut(index)?;
    while let Some(&addr) = waiting.order.get(*cursor) {
        *cursor += 1;
        if let Some(entry) = waiting.state.get_mut(&addr)
            && entry.slot == Slot::Pending
        {
            entry.slot = Slot::Claimed;
            return Some(addr);
        }
    }
    None
}

/// 명령이 자기가 링크한 주소 하나를 집는다. 아직 아무도 안 집었으면 `true`.
///
/// `false`면 그 사이에 빠졌거나 같은 id의 다른 쓰기가 자리를 물려받았다는
/// 뜻이다. 어느 쪽이든 이 명령이 넣을 것은 남아 있지 않다.
pub(crate) fn claim_addr(index: &str, addr: u64) -> bool {
    let mut store = store();
    let Some(entry) = store
        .by_index
        .get_mut(index)
        .and_then(|waiting| waiting.state.get_mut(&addr))
    else {
        return false;
    };
    if matches!(entry.slot, Slot::Pending | Slot::Reserved) {
        entry.slot = Slot::Claimed;
        return true;
    }
    false
}

/// 기다리던 주소가 새것으로 교체됐다.
///
/// **자리를 그대로 물려받는다.** old를 빼고 new를 덧붙이면 목록이 replace마다
/// 자라고, 자라다 할당기에 거절당하면 그 벡터는 그래프에서 빠진다. 크기가 안
/// 변하는 연산이니 자랄 이유가 없고, 그래서 이 경로는 실패하지 않는다.
///
/// `reserved`면 new를 이 쓰기를 한 명령이 집어가도록 적는다.
pub(crate) fn replace(index: &str, old: u64, new: u64, reserved: bool) -> Replaced {
    let mut store = store();
    let Some(waiting) = store.by_index.get_mut(index) else {
        return Replaced::NotWaiting;
    };
    let Some(entry) = waiting.state.get(&old).copied() else {
        return Replaced::NotWaiting;
    };

    match entry.slot {
        Slot::Pending | Slot::Reserved => {
            waiting.state.remove(&old);
            waiting.order[entry.at] = new;
            let slot = if reserved {
                Slot::Reserved
            } else {
                Slot::Pending
            };
            waiting.state.insert(new, Entry { slot, at: entry.at });
            Replaced::Swapped
        }
        Slot::Claimed => {
            // 풀이 old를 역참조하고 있다. 자리를 빼앗으면 그쪽이 넣고 나서
            // 볼 표가 없어진다. 묘비를 찍어 되돌리게 하고 new는 따로 잡는다.
            if let Some(entry) = waiting.state.get_mut(&old) {
                entry.slot = Slot::Dropped;
            }
            Replaced::Recovering
        }
        Slot::Dropped => Replaced::NotWaiting,
    }
}

/// 그래프에 넣고 나서 부른다. 그대로 둬도 되면 `true`.
///
/// `false`면 넣는 사이에 unlink가 지나갔다는 뜻이라, 부른 쪽이 노드를 도로 빼고
/// 참조를 돌려줘야 한다. 어느 쪽이든 항목은 여기서 지워진다.
///
/// 항목이 없으면 `true`다. 알 수 없을 때 한쪽으로만 틀려야 한다면 새는 쪽이다
/// -- 요소가 아직 링크돼 있으면 그 unlink가 참조를 돌려주고, 여기서 넘겨짚어
/// 놓아주면 그때 한 번 더 놓아 이중 해제가 된다.
pub(crate) fn finish(index: &str, addr: u64) -> bool {
    let mut store = store();
    match store.vacate(index, addr) {
        Some(entry) => entry.slot != Slot::Dropped,
        None => true,
    }
}

/// unlink 콜백이 부른다. 이 주소를 어떻게 할지 답한다.
pub(crate) fn unlink(index: &str, addr: u64) -> Unlinked {
    let mut store = store();
    let Some(entry) = store
        .by_index
        .get_mut(index)
        .and_then(|waiting| waiting.state.get_mut(&addr))
    else {
        return Unlinked::NotWaiting;
    };
    match entry.slot {
        Slot::Pending | Slot::Reserved => {}
        Slot::Claimed => {
            entry.slot = Slot::Dropped;
            return Unlinked::Recovering;
        }
        Slot::Dropped => return Unlinked::NotWaiting,
    }
    store.vacate(index, addr);
    Unlinked::NeverLinked
}

/// 흘려보낸 것이 있었는지 묻고 표시를 내린다. adopt가 끝나고 부른다.
///
/// 목록은 버리지 않는다. adopt가 훑는 사이에도 새 주소가 들어올 수 있고, 그것을
/// 버리면 참조를 쥔 채 그래프 밖에 남는다.
pub(crate) fn take_incomplete(index: &str) -> bool {
    let mut store = store();
    let Some(waiting) = store.by_index.get_mut(index) else {
        return false;
    };
    let incomplete = std::mem::take(&mut waiting.incomplete);
    if waiting.state.is_empty() {
        store.by_index.remove(index);
    }
    incomplete
}

/// 아직 붙잡고 있는 주소 수. `vstats`가 이것을 보고한다.
pub(crate) fn count() -> usize {
    store().count
}

/// 이 인덱스의 목록을 통째로 버린다. 흘려보낸 것이 있었으면 `true`.
#[cfg(test)]
fn forget(index: &str) -> bool {
    let mut store = store();
    let Some(waiting) = store.by_index.remove(index) else {
        return false;
    };
    store.count = store.count.saturating_sub(waiting.state.len());
    waiting.incomplete
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 이 모듈의 시험은 전역 목록 하나를 공유한다. `count()`는 이름별이 아니라
    /// 전체 수라, 병렬로 돌면 서로의 `push`를 세어 엉뚱한 곳에서 터진다.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn alone(name: &str) -> MutexGuard<'static, ()> {
        let guard = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        forget(name);
        guard
    }

    #[test]
    fn a_vector_that_arrives_before_its_metadata_is_kept() {
        let ix = "pending-kept";
        let _alone = alone(ix);
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
        let _alone = alone(ix);
        push(ix, 0x10);

        assert_eq!(unlink(ix, 0x10), Unlinked::NeverLinked);

        let mut cursor = 0;
        assert_eq!(
            claim(ix, &mut cursor),
            None,
            "빠진 주소를 다시 집으면 해제된 요소를 그래프에 넣는다"
        );
        forget(ix);
    }

    #[test]
    fn unlinking_while_it_is_being_inserted_leaves_the_cleanup_to_the_inserter() {
        let ix = "pending-late-unlink";
        let _alone = alone(ix);
        push(ix, 0x10);
        let mut cursor = 0;
        assert_eq!(claim(ix, &mut cursor), Some(0x10));

        // 넣는 중에 unlink가 왔다. 여기서 거절하면 엔진이 해제해 버리는데
        // 넣는 쪽은 그 주소를 역참조하고 있다.
        assert_eq!(unlink(ix, 0x10), Unlinked::Recovering);
        assert!(
            !finish(ix, 0x10),
            "넣는 쪽은 방금 넣은 노드를 도로 빼야 한다는 답을 받아야 한다"
        );
        forget(ix);
    }

    #[test]
    fn a_vector_already_in_the_graph_falls_through_to_the_usual_path() {
        let ix = "pending-done";
        let _alone = alone(ix);
        push(ix, 0x10);
        let mut cursor = 0;
        claim(ix, &mut cursor);
        assert!(finish(ix, 0x10));

        assert_eq!(unlink(ix, 0x10), Unlinked::NotWaiting);
        assert_eq!(count(), 0, "그래프에 들어간 것은 장부에서 지워진다");
        forget(ix);
    }

    #[test]
    fn finishing_against_a_list_that_is_gone_leaves_the_address_alone() {
        let _alone = alone("pending-vanished");
        // 놓아주라고 답하면 -- 요소가 아직 링크돼 있는데 -- 그 unlink가 한 번
        // 더 놓아 이중 해제가 된다. 모를 때는 새는 쪽으로 틀린다.
        assert!(finish("pending-vanished", 0x10));
    }

    #[test]
    fn a_reserved_address_is_left_for_the_command_that_linked_it() {
        let ix = "reserved-skip";
        let _alone = alone(ix);
        reserve(ix, 0x10);
        push(ix, 0x20);

        let mut cursor = 0;
        assert_eq!(
            claim(ix, &mut cursor),
            Some(0x20),
            "adopt는 명령이 맡은 주소를 건너뛴다"
        );
        assert!(
            !pending(ix),
            "명령이 맡은 것은 sweeper가 다시 맡기지 않는다"
        );

        assert!(claim_addr(ix, 0x10), "명령은 자기 주소를 집는다");
        assert!(!claim_addr(ix, 0x10), "두 번 집을 수는 없다");
        assert!(finish(ix, 0x10));
        forget(ix);
    }

    #[test]
    fn a_command_finds_nothing_to_insert_after_a_newer_write_took_its_place() {
        let ix = "reserved-replaced";
        let _alone = alone(ix);
        reserve(ix, 0x10);

        assert_eq!(replace(ix, 0x10, 0x20, true), Replaced::Swapped);
        assert!(!claim_addr(ix, 0x10), "옛 주소는 더 이상 없다");
        assert!(claim_addr(ix, 0x20), "새 쓰기의 명령이 새 주소를 집는다");
        forget(ix);
    }

    #[test]
    fn replacing_a_waiting_vector_takes_over_its_place() {
        // 자리를 물려받지 않고 덧붙이면 목록이 replace마다 자라고, 자라다
        // 할당기에 거절당하면 그 벡터가 그래프에서 빠진다.
        let ix = "waiting-replace";
        let _alone = alone(ix);
        push(ix, 0x10);
        let before = count();

        assert_eq!(replace(ix, 0x10, 0x20, false), Replaced::Swapped);
        assert_eq!(count(), before, "자리를 물려받았으니 더 잡지 않는다");

        let mut cursor = 0;
        assert_eq!(
            claim(ix, &mut cursor),
            Some(0x20),
            "adopt는 새 주소를 집는다"
        );
        assert_eq!(claim(ix, &mut cursor), None, "옛 주소는 남아 있지 않다");
        forget(ix);
    }

    #[test]
    fn replacing_one_being_inserted_leaves_it_to_the_inserter() {
        let ix = "waiting-replace-claimed";
        let _alone = alone(ix);
        push(ix, 0x10);
        let mut cursor = 0;
        assert_eq!(claim(ix, &mut cursor), Some(0x10));

        assert_eq!(replace(ix, 0x10, 0x20, false), Replaced::Recovering);
        assert!(!finish(ix, 0x10), "넣는 쪽이 도로 빼야 한다는 답을 받는다");
        forget(ix);
    }

    #[test]
    fn replacing_something_nobody_is_waiting_on_is_not_ours() {
        let ix = "waiting-replace-unknown";
        let _alone = alone(ix);
        assert_eq!(replace(ix, 0x10, 0x20, false), Replaced::NotWaiting);
    }

    #[test]
    fn a_name_nobody_is_waiting_on_is_not_ours_to_answer_for() {
        let _alone = alone("pending-unknown");
        assert_eq!(unlink("pending-unknown", 0x10), Unlinked::NotWaiting);
    }

    #[test]
    fn settled_entries_give_the_room_back() {
        let ix = "pending-room";
        let _alone = alone(ix);
        let before = count();
        push(ix, 0x10);
        push(ix, 0x20);
        assert_eq!(count(), before + 2);

        let mut cursor = 0;
        while let Some(addr) = claim(ix, &mut cursor) {
            assert!(finish(ix, addr));
        }
        assert_eq!(count(), before, "자리를 돌려주지 않으면 장부가 새어나간다");
        assert!(!take_incomplete(ix), "흘려보낸 것이 없었다");
    }

    #[test]
    fn compaction_keeps_every_live_entry_findable() {
        let ix = "pending-compact";
        let _alone = alone(ix);
        for addr in 1..=200u64 {
            reserve(ix, addr * 0x10);
        }
        // 대부분을 끝내 빈 칸을 만든다. 압축이 칸 번호를 고쳐 주지 않으면
        // 남은 것의 replace가 엉뚱한 칸을 덮는다.
        for addr in 1..=190u64 {
            assert!(claim_addr(ix, addr * 0x10));
            assert!(finish(ix, addr * 0x10));
        }
        assert_eq!(replace(ix, 195 * 0x10, 0x99990, true), Replaced::Swapped);
        for addr in (191..=200u64).filter(|a| *a != 195) {
            assert!(claim_addr(ix, addr * 0x10), "{addr}");
        }
        assert!(claim_addr(ix, 0x99990));
        forget(ix);
    }
}
