//! 메타가 뒤늦게 도착한 인덱스의 그래프를 세운다.
//!
//! 복제본에는 `vcreate`가 오지 않으므로 인덱스는 메타 아이템이 링크되는 순간
//! 생긴다. 그 전에 도착한 벡터들은 `pending`에 주소만 적혀 있고, 여기서 그래프에
//! 들어간다.
//!
//! **풀 스레드에서 돈다.** 메타 콜백은 엔진의 cache lock을 쥐고 있어서 거기서
//! 삽입을 돌리면 그동안 데몬 전체가 선다. 콜백은 인덱스를 `BUILDING`으로
//! 등록하고 일감만 넘긴다 -- `BUILDING`인 동안 `access::resolve`가 그 이름을
//! `NoSuchIndex`로 답하므로, 반쯤 찬 그래프가 조회에 보이지 않는다.

use std::sync::Arc;

use crate::handler::access::pool::{self, Job};
use crate::handler::arcus::engine::Store;
use crate::handler::registry::VectorIndex;
use crate::trigger::waiting;

/// 복구를 풀에 맡긴다. 이미 맡겨져 있거나 대기열이 가득 차면 `false`.
///
/// 표를 먼저 끊어 두 번 맡기는 것을 막는다. 맡기지 못했으면 표를 돌려줘야
/// sweeper가 다음 틱에 다시 시도한다.
pub fn submit(index: &Arc<VectorIndex>) -> bool {
    if !index.claim_recovery() {
        return false;
    }
    if pool::submit(Job::Recover(Arc::clone(index))) {
        return true;
    }
    index.release_recovery();
    false
}

/// 풀 스레드가 부른다. 보류된 벡터를 넣고 인덱스를 서빙으로 올린다.
pub(crate) fn work(index: &Arc<VectorIndex>) {
    adopt(index);
    index.publish();
    index.mark_serving();
    index.release_recovery();
}

/// 이 이름으로 붙잡혀 있던 벡터를 그래프에 넣는다.
///
/// `vcreate`도 이것을 부른다. 이 노드에서 만든 인덱스라도, 복제로 먼저 들어온
/// 벡터가 그 이름으로 기다리고 있을 수 있다 -- `vcreate`는 레지스트리에 먼저
/// 등록하고 메타를 나중에 쓰므로, 메타 콜백은 "이미 아는 인덱스"를 보고 지나간다.
/// 붙잡힌 것을 집어가는 자리가 여기 하나뿐이어야 어느 쪽으로 만들어졌든 같은
/// 결과가 된다.
pub fn adopt(index: &Arc<VectorIndex>) {
    let name = index.name.as_str();
    let layout = index.ann.layout;

    let Some(store) = Store::background() else {
        // vtable이 없으면 아이템을 읽을 수 없다. 붙잡고 있어봐야 넣을 길이
        // 없으니 목록을 버린다 -- 참조는 각 아이템이 빠질 때 회수된다.
        waiting::forget(name);
        eprintln!("ArcVector: no engine vtable; '{name}' comes up with an empty graph");
        return;
    };

    let mut cursor = 0usize;
    let mut linked = 0usize;
    let mut lost = 0usize;

    while let Some(addr) = waiting::claim(name, &mut cursor) {
        // `Claimed`인 동안에는 unlink 콜백이 이 주소를 놓아주지 않는다
        // (`pending`의 표를 보라). 그래서 배리어 없이 역참조해도 된다.
        let took = store
            .with_item_at(addr, |_key, value| {
                layout
                    .vector_of(value)
                    .map(|vector| index.ann.link_node(addr, vector))
            })
            .flatten()
            .is_some_and(|r| r.is_ok());

        // 넣고 **나서** 슬롯을 확인한다. 순서가 반대면 확인과 삽입 사이에 창이
        // 남는다. 실패했더라도 반드시 불러야 한다 -- 슬롯을 `Claimed`로 둔 채
        // 넘어가면 이 주소를 누가 놓아줘야 하는지가 정해지지 않는다.
        let kept = waiting::finish(name, addr);

        // 들어갔으면 평소 경로가 맡는다. 그 밖에는 전부 여기서 놓아준다.
        //
        // 못 넣은 경우도 그렇다. 그 주소는 링크 때 `Took`으로 답해 붙잡아 둔
        // 것이라 참조가 우리 것인데, 그래프에 노드가 없으니 나중에 그 아이템이
        // 빠질 때 `on_unlink`은 "우리 것이 아니다"로 읽는다. 여기서 안 놓으면
        // 아무도 안 놓는다. 아직 링크된 아이템을 놓아주는 것은 안전하다 --
        // `do_item_release`는 링크된 것을 해제하지 않고 LRU로 되돌린다.
        if took && kept {
            linked += 1;
            continue;
        }

        lost += 1;
        index.ann.unlink_at(addr);
        if !index.ann.retire_one(addr) {
            index.ann.halt_for_overflow();
        }
    }

    let incomplete = waiting::forget(name);

    // 기다린 것이 없었으면 조용히 지나간다 -- `vcreate`마다 한 줄씩 찍힐 자리다.
    if incomplete {
        eprintln!(
            "ArcVector: '{name}' adopted {linked} vectors, but the allocator refused \
             others while it waited for its metadata -- the graph is short of the \
             store and this index needs a resync"
        );
    } else if linked > 0 || lost > 0 {
        eprintln!("ArcVector: '{name}' adopted {linked} vectors that arrived before it");
    }
    if lost > 0 {
        eprintln!("ArcVector: '{name}' dropped {lost} vectors while adopting");
    }
}
