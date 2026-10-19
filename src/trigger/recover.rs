//! 메타가 뒤늦게 도착한 인덱스의 그래프를 세운다.
//!
//! 복제본에는 `vcreate`가 오지 않으므로 인덱스는 메타 요소가 링크되는 순간
//! 생긴다. 그 전에 도착한 벡터들은 `waiting`에 주소만 적혀 있고, 여기서 그래프에
//! 들어간다. 이미 서빙 중인 인덱스에 복제·복구로 들어온 벡터도 같은 길을 탄다.
//!
//! **풀 스레드에서 돈다.** 메타 콜백은 엔진의 cache lock을 쥐고 있어서 거기서
//! 삽입을 돌리면 그동안 데몬 전체가 선다. 콜백은 인덱스를 `BUILDING`으로
//! 등록하고 일감만 넘긴다 -- `BUILDING`인 동안 `access::resolve`가 그 이름을
//! `NoSuchIndex`로 답하므로, 반쯤 찬 그래프가 조회에 보이지 않는다.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handler::access::pool::{self, Job};
use crate::handler::arcus::engine::{Store, StoreError};
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

/// 이미 서빙 중인 인덱스의 보류 목록을 비운다.
///
/// 링크 콜백이 cache lock을 쥔 채로는 HNSW 삽입을 할 수 없어 주소만 적어
/// 두는데, 그것을 집어가는 자리가 여기다. `submit`과 달리 인덱스 상태를
/// 건드리지 않는다 -- 이미 서빙 중이니 올릴 것이 없다.
pub fn submit_adopt(index: &Arc<VectorIndex>) -> bool {
    if !index.claim_recovery() {
        // 이미 누가 비우는 중이다. 그쪽이 우리 것까지 가져간다.
        return true;
    }
    if pool::submit(Job::Adopt(Arc::clone(index))) {
        return true;
    }
    index.release_recovery();
    false
}

/// 풀 스레드가 부른다. 보류된 것만 넣고 끝낸다.
pub(crate) fn adopt_work(index: &Arc<VectorIndex>) {
    adopt(index);
    index.release_recovery();
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
///
/// 명령이 맡은(`Reserved`) 주소는 건너뛴다. 그것은 [`insert_reserved`]가 넣는다.
pub fn adopt(index: &Arc<VectorIndex>) {
    let name = index.name.as_str();
    let store = Store::background();

    let mut cursor = 0usize;
    let mut linked = 0usize;
    let mut lost = 0usize;

    while let Some(addr) = waiting::claim(name, &mut cursor) {
        match insert_claimed(store.as_ref(), index, addr) {
            Ok(true) => linked += 1,
            _ => lost += 1,
        }
    }

    let incomplete = waiting::take_incomplete(name);

    // 기다린 것이 없었으면 조용히 지나간다 -- `vcreate`마다 한 줄씩 찍힐 자리다.
    if incomplete {
        eprintln!(
            "ArcVector: '{name}' adopted {linked} vectors, but the allocator refused \
             others while they waited -- the graph is short of the store and this \
             index needs a resync"
        );
    } else if linked > 0 || lost > 0 {
        eprintln!("ArcVector: '{name}' adopted {linked} vectors that were waiting");
    }
    if lost > 0 {
        eprintln!("ArcVector: '{name}' dropped {lost} vectors while adopting");
    }
}

/// 명령이 링크한 주소 하나를 넣는다. **풀 스레드에서 돈다.**
///
/// `vadd`가 이것이 끝나기를 기다렸다가 답하므로, 답을 받은 클라이언트의 다음
/// 검색은 그 벡터를 본다.
///
/// 주소를 못 집었으면 할 일이 없다 -- 그 사이에 빠졌거나 같은 id의 더 새
/// 쓰기가 자리를 물려받았다. 어느 쪽이든 이 쓰기의 벡터는 이미 저장소에 없다.
pub(crate) fn insert_reserved(index: &Arc<VectorIndex>, addr: u64) -> Result<()> {
    if !waiting::claim_addr(&index.name, addr) {
        return Ok(());
    }
    insert_claimed(Store::background().as_ref(), index, addr).map(|_| ())
}

/// `Claimed`로 집은 주소 하나를 그래프에 넣고 장부를 정리한다.
///
/// `Ok(true)`면 그래프에 남았다. `Ok(false)`면 넣는 사이에 빠져 도로 뺐고,
/// `Err`면 그래프가 받지 못했다. 뒤의 둘은 여기서 참조를 놓아준다.
///
/// 못 넣은 경우도 놓아줘야 한다. 그 주소는 링크 때 `target_ref = 1`로 답해 붙잡아 둔
/// 것이라 참조가 우리 것인데, 그래프에 노드가 없으니 나중에 그 요소가 빠질 때
/// `on_unlink`은 "우리 것이 아니다"로 읽는다. 여기서 안 놓으면 아무도 안
/// 놓는다. 아직 링크된 요소를 놓아주는 것은 안전하다 -- 엔진은 링크된 요소를
/// 해제하지 않는다.
fn insert_claimed(store: Option<&Store>, index: &Arc<VectorIndex>, addr: u64) -> Result<bool> {
    let layout = index.ann.layout;

    // `Claimed`인 동안에는 unlink 콜백이 이 주소를 놓아주지 않는다
    // (`waiting`의 표를 보라). 그래서 배리어 없이 역참조해도 된다.
    let inserted = store
        .ok_or(Error::Store(StoreError::Unavailable))
        .and_then(|store| {
            store
                .with_elem_at(addr, |_field, value| {
                    layout
                        .split(value)
                        .ok()
                        .map(|(_, vector)| index.ann.link_node(addr, vector))
                })
                .flatten()
                .unwrap_or(Err(Error::Store(StoreError::CorruptElement)))
        });

    // 넣고 **나서** 슬롯을 확인한다. 순서가 반대면 확인과 삽입 사이에 창이
    // 남는다. 실패했더라도 반드시 불러야 한다 -- 슬롯을 `Claimed`로 둔 채
    // 넘어가면 이 주소를 누가 놓아줘야 하는지가 정해지지 않는다.
    let kept = waiting::finish(&index.name, addr);
    if inserted.is_ok() && kept {
        return Ok(true);
    }

    if inserted.is_ok() {
        index.ann.unlink_at(addr);
    }
    if !index.ann.retire_one(addr) {
        index.ann.halt_for_overflow();
    }
    inserted.map(|()| false)
}
