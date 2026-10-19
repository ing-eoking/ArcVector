use super::vector::check_index;
use crate::command::request::Create;
use crate::error::{Error, Reply, Result};
use crate::handler::access::resolve;
use crate::handler::arcus::element::{IndexConfig, Layout};
use crate::handler::arcus::engine::{self, Store, StoreError};
use crate::handler::quant::Quant;
use crate::handler::registry;

/// `maxcount`를 안 적었을 때의 값.
const DEFAULT_MAXCOUNT: u32 = 4000;

pub fn create(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;
    check_index(name)?;

    let layout = Layout::new(dim, quant);
    check_dimension_fits(store, layout, quant)?;
    // 그래프는 링크 콜백이 세운다. 거기서 거절될 설정은 Map을 만들기 전에 막는다.
    metric.check_quant(quant)?;

    let config = IndexConfig {
        dim,
        metric,
        quant,
        m: spec.m,
        efc: spec.efc,
        efs: spec.efs,
    };
    let maxcount = map_size_for(store, spec.maxcount)?;

    // Map과 설정을 한 번의 엔진 호출로 만든다. 설정 없는 Map이 복제본에 보이는
    // 순간이 없다. 그 링크가 콜백을 부르고, 콜백이 값을 읽어 인덱스를 등록한다
    // -- 복제본과 재생이 타는 바로 그 길이다.
    //
    // 등록하는 `registry::put`이 sweeper와 풀을 처음 띄우는 자리이기도 하다.
    // 콜백은 cache lock 아래라 스레드를 거기서 만들지 않게 여기서 먼저 띄운다.
    crate::handler::access::sweep::ensure_sweeper();
    crate::handler::access::pool::ensure_pool();
    let exptime = crate::server::realtime(spec.exptime);
    let created = store.create_index_map(
        name,
        &config.encode(),
        engine::index_attr(maxcount, exptime),
    );

    match created {
        Ok(()) if registry::contains(name) => Ok(Reply::Created),
        Ok(()) => {
            // Map은 섰는데 콜백이 그래프를 세우지 못했다(할당 실패). 이대로 두면
            // 그 키는 어느 명령에도 `TYPE_MISMATCH`로 답하는 빈 Map이 된다.
            if let Err(e) = store.drop_map(name)
                && e != StoreError::KeyGone
            {
                eprintln!(
                    "ArcVector: could not remove the Map of '{name}' after a failed create: {e}"
                );
            }
            Err(Error::Index(format!(
                "the graph for '{name}' could not be built"
            )))
        }
        // 그 키에 무언가 있다. 우리 인덱스면 `EXISTS`, 아니면 일반 키나 우리가
        // 읽지 못하는 Map이다.
        Err(StoreError::KeyExists) => Ok(if registry::contains(name) {
            Reply::Exists
        } else {
            Reply::TypeMismatch
        }),
        Err(e) => Err(e.into()),
    }
}

/// Map에 잡을 칸 수. 서버의 `max_map_size`를 넘으면 `CLIENT_ERROR`다.
/// 적지 않았으면 기본값이되, 서버 상한이 그보다 낮으면 그 상한이다.
fn map_size_for(store: &Store, maxcount: Option<u32>) -> Result<u32> {
    let ceiling = store.max_map_size().max(1);
    let Some(n) = maxcount else {
        return Ok(DEFAULT_MAXCOUNT.min(ceiling));
    };
    if n == 0 || n > ceiling {
        return Err(Error::bad_request(format!(
            "maxcount {n} is out of range (1..={ceiling}, the server's max_map_size)"
        )));
    }
    Ok(n)
}

pub fn drop_index(store: &Store, name: &str) -> Result<Reply> {
    check_index(name)?;
    // 이 시각 뒤에 만들어진 같은 이름의 인덱스는 건드리지 않는다.
    let stamp = registry::now();

    // 키가 인덱스와 같은 공간에 있다. 일반 키를 `vop drop`으로 지우면 안 된다.
    match resolve(store, name) {
        Ok(_) => {}
        Err(Error::TypeMismatch) => return Ok(Reply::TypeMismatch),
        Err(Error::NoSuchIndex) => {
            // 키가 없다. 혹시 남은 엔트리가 있으면 그것도 이제 없는 인덱스다.
            registry::remove_if_stale(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e),
    }

    // Map을 지운다. 그 unlink가 콜백을 부르고, 콜백이 인덱스를 레지스트리에서
    // 내린다 -- 여기서나 복제본에서나 같다. 복제본은 이 명령이 아니라 복제된
    // 삭제를 보기 때문이다.
    //
    // 그래프는 같이 버리지 않는다. 요소들은 엔진의 컬렉션 삭제 스레드가 나중에
    // 지우고, 그때마다 오는 unlink가 `registry::holding`으로 이 그래프를 다시
    // 찾아 참조를 돌려준다.
    match store.drop_map(name) {
        Ok(()) | Err(StoreError::KeyGone) => {}
        Err(e) => return Err(e.into()),
    }
    // Map이 그 사이 없어졌으면 콜백도 없었으니 엔트리가 아직 있다. 그 사이 같은
    // 이름으로 새로 만든 인덱스는 `stamp` 뒤에 올라 있어 내리지 않는다.
    registry::remove_if_stale(name, stamp);
    Ok(Reply::Dropped)
}

fn check_dimension_fits(store: &Store, layout: Layout, quant: Quant) -> Result<()> {
    let limit = store.max_element_bytes() as usize;

    // 메타가 가장 작은 경우(빈 문서)로 잰다. 메타가 큰 요소는 쓸 때 다시 잰다.
    let stored = layout.stored_len(crate::bson::EMPTY.len());
    if stored <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {stored} bytes, over max_element_bytes {limit} \
         (max dimension is {} for quant {quant})",
        Layout::max_dim_for(quant, limit),
    )))
}
