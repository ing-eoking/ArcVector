use std::fmt::Write as _;
use std::sync::Arc;

use crate::command::request::OldCreate as Create;
use crate::error::{Error, Reply, Result};
use crate::handler::access::resolve;
use crate::handler::arcus::element::{IndexConfig, Layout};
use crate::handler::arcus::engine::{self, Store, StoreError};
use crate::handler::quant::Quant;
use crate::handler::registry::{self, VectorIndex};

pub fn vcreate(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;

    let layout = Layout::new(dim, quant);
    check_dimension_fits(store, layout, quant)?;
    // 그래프는 링크 콜백이 세운다. 거기서 거절될 설정은 Map을 만들기 전에 막는다.
    metric.check_quant(quant)?;

    let config = IndexConfig {
        dim,
        metric,
        quant,
        m: spec.connectivity,
        efc: spec.expansion_add,
        efs: spec.expansion_search,
    };

    // Map과 설정을 한 번의 엔진 호출로 만든다. 설정 없는 Map이 복제본에 보이는
    // 순간이 없다. 그 링크가 콜백을 부르고, 콜백이 값을 읽어 인덱스를 등록한다
    // -- 복제본과 재생이 타는 바로 그 길이다.
    //
    // 등록하는 `registry::put`이 sweeper와 풀을 처음 띄우는 자리이기도 하다.
    // 콜백은 cache lock 아래라 스레드를 거기서 만들지 않게 여기서 먼저 띄운다.
    crate::handler::access::sweep::ensure_sweeper();
    crate::handler::access::pool::ensure_pool();
    let exptime = spec.exptime.map_or(0, crate::server::realtime);
    let created = store.create_index_map(
        name,
        &config.encode(),
        engine::index_attr(map_size_for(store, spec.maxcount), exptime),
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

/// Map에 잡을 칸 수. 서버의 `max_map_size`를 넘지 않는다. 이름을 안 했으면
/// 그 상한이다.
fn map_size_for(store: &Store, maxcount: Option<u32>) -> u32 {
    let ceiling = store.max_map_size().max(1);
    maxcount.unwrap_or(ceiling).clamp(1, ceiling)
}

pub fn vdrop(store: &Store, name: &str) -> Result<Reply> {
    // 키가 인덱스와 같은 공간에 있다. 일반 키를 `vdrop`으로 지우면 안 된다.
    match resolve(store, name) {
        Ok(_) => {}
        Err(Error::TypeMismatch) => return Ok(Reply::TypeMismatch),
        Err(Error::NoSuchIndex) => {
            // 키가 없다. 혹시 남은 엔트리가 있으면 그것도 이제 없는 인덱스다.
            registry::remove(name);
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
    // Map이 그 사이 없어졌으면 콜백도 없었으니 엔트리가 아직 있다.
    registry::remove(name);
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

/// 한 인덱스가 답에 더하는 줄들의 넉넉한 상한.
///
/// `vstats`는 네 줄, `vlist`는 한 줄인데 둘 다 이름이 네 번/한 번 들어간다.
/// 고정부는 필드 이름과 u64 열 자리 남짓이라, 줄당 96바이트면 남는다. 정확할
/// 필요는 없다 -- 모자라면 뒤의 `write!`가 더 늘릴 뿐이고, 여기서 보려는 것은
/// **이만한 답을 지을 메모리가 있느냐**다.
const PER_INDEX_LINE: usize = 96;

fn room_for_rows(out: &mut String, indexes: &[Arc<VectorIndex>], lines: usize) -> Result<()> {
    let want = indexes
        .iter()
        .map(|index| (index.name.len() + PER_INDEX_LINE) * lines)
        .try_fold(0usize, |sum, n| sum.checked_add(n))
        .ok_or_else(|| Error::Index("listing does not fit in memory".into()))?;
    crate::room::reserve_str(out, want)
}

pub fn vstats() -> Result<Reply> {
    let indexes = registry::snapshot().map_err(|_| Error::Index("no room to list".into()))?;

    let mut vectors = 0usize;
    let mut held_bytes = 0usize;
    let mut used_bytes = 0usize;
    for index in &indexes {
        vectors += index.ann.len();
        held_bytes += index.ann.held_bytes();
        used_bytes += index.ann.used_bytes();
    }

    let mut out = String::new();
    let _ = write!(
        out,
        "STAT indexes {}\r\n\
         STAT vectors {vectors}\r\n\
         STAT index_held_bytes {held_bytes}\r\n\
         STAT index_used_bytes {used_bytes}\r\n\
         STAT vectors_awaiting_metadata {}\r\n",
        indexes.len(),
        crate::trigger::waiting::count(),
    );

    // 인덱스별 줄을 **같은 버퍼에** 잇는다. 따로 지었다가 붙이면 두 벌이 동시에
    // 떠서 피크가 두 배가 된다. 자리는 먼저 잡고, 없으면 답을 짓지 않는다.
    room_for_rows(&mut out, &indexes, 4)?;
    for index in &indexes {
        let name = &index.name;
        let _ = write!(
            out,
            "STAT {name}:vectors {}\r\n\
             STAT {name}:index_held_bytes {}\r\n\
             STAT {name}:index_used_bytes {}\r\n\
             STAT {name}:reserved {}\r\n",
            index.ann.len(),
            index.ann.held_bytes(),
            index.ann.used_bytes(),
            index.ann.reserved()
        );
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

pub fn vlist() -> Result<Reply> {
    let indexes = registry::snapshot().map_err(|_| Error::Index("no room to list".into()))?;

    let mut out = String::new();
    room_for_rows(&mut out, &indexes, 1)?;
    for index in &indexes {
        let layout = &index.ann.layout;
        let _ = writeln!(
            out,
            "INDEX {} dim={} quant={} metric={} count={} maxcount={}\r",
            index.name,
            layout.dim,
            layout.quant,
            index.ann.metric,
            index.ann.len(),
            index.maxcount,
        );
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}
