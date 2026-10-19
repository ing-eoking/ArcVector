use std::cell::RefCell;
use std::fmt::Write as _;

use super::coords::coord_vectors;
use crate::command::filter::Filter;
use crate::command::request::{Sim, SimKey};
use crate::error::{Error, Reply, Result};
use crate::handler::access::for_read;
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::VectorIndex;
use crate::handler::usearch::Accept;

/// accept 콜백이 한 노드에 내린 판정.
///
/// `Option<Option<String>>`이 아니라 이것인 이유는, 바깥 `None`이 이미
/// [`Store::with_elem_at`]의 "읽을 수 없다"로 쓰이고 있어서다.
enum Verdict {
    /// 받는다. 필터가 읽어 둔 attr이 있으면 들려 보낸다 -- 렌더가 같은 값을
    /// 다시 읽지 않도록.
    Take(Option<String>),
    /// 필터가 걸렀다.
    Drop,
}

fn judged_attr(judged: &RefCell<Vec<(u64, String)>>, key: u64) -> Option<String> {
    let judged = judged.borrow();
    judged
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, a)| a.clone())
}

/// The line that opens one query's hits.
///
/// A graph still being rebuilt answers from the part of itself that exists,
/// which is not the nearest neighbours — it is the nearest of what has been
/// added so far. `PARTIAL_VECTORS` names that, and carries the counts so the
/// caller can judge how far off the answer may be.
fn query_header(hits: usize, rebuild: Option<crate::error::Rebuild>) -> String {
    match rebuild {
        None => format!("VECTORS {hits}\r\n"),
        Some(p) => format!("PARTIAL_VECTORS {hits} {}/{}\r\n", p.done, p.total),
    }
}

#[allow(clippy::too_many_arguments)]
fn similar(
    store: &Store,
    index: &VectorIndex,
    query: &[u8],
    k: usize,
    filter: Option<&Filter>,
    with_attr: bool,
    out: &mut String,
) -> Result<()> {
    let layout = index.ann.layout;

    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    let accepted = |key: u64| -> bool {
        // 순회 중 방문하는 노드마다 엔진 메모리를 따라간다. 콜백이 거절하면
        // 엔진이 그 자리에서 해제하므로, 가드 없이 읽으면 해제된 메모리를 본다.
        let Some(_touching) = index.ann.touch_for_deref() else {
            return false;
        };

        let Some(filter) = filter else {
            // 필터가 없으면 그래프에 있다는 것이 곧 판정이다.
            return true;
        };
        let Some(Verdict::Take(attr)) =
            store.with_elem_at(key, |_field, value| match layout.attr_of(value) {
                Ok(attr) if filter.matches(attr) => {
                    Verdict::Take(Some(String::from_utf8_lossy(attr).into_owned()))
                }
                _ => Verdict::Drop,
            })
        else {
            // 읽을 수 없거나 필터가 걸렀다.
            return false;
        };

        let Some(attr) = attr else {
            // 필터가 없어 렌더에 넘겨줄 attr이 없다.
            return true;
        };

        let mut judged = judged.borrow_mut();

        if judged.try_reserve(1).is_err() {
            return false;
        }
        judged.push((key, attr));
        true
    };

    // 필터가 없어도 건다. 잠긴 인덱스의 역참조를 막는 가드가 여기 있고,
    // 판정이 순회 안에서 나야 usearch가 거절당한 만큼 더 방문해 k를 채운다.
    let accept: Option<Accept> = Some(&accepted as Accept);

    let named = index.ann.search(query, k, accept)?;

    let mut rendered = crate::room::vec(named.len().min(k))?;
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }
        let attr = if !with_attr {
            None
        } else if let Some(attr) = judged_attr(&judged, key) {
            Some(attr)
        } else {
            let stored = match store
                .get_elem(&crate::trigger::key::map_key(&index.name), &id)
                .and_then(|body| {
                    layout
                        .attr_of(&body)
                        .map(<[u8]>::to_vec)
                        .map_err(|_| StoreError::CorruptElement)
                }) {
                Ok(stored) => stored,

                // A delete that landed between the search and here. One hit
                // goes, the query stands.
                Err(StoreError::ElemGone) => continue,

                Err(StoreError::CorruptElement) => {
                    // 그래프에서 빼지 않는다. 이 요소가 실제로 사라질 때
                    // unlink 이벤트가 오고, 그때 콜백이 노드를 뺀다.
                    eprintln!(
                        "ArcVector: element '{id}' of index '{}' is unreadable",
                        index.name
                    );
                    return Err(StoreError::CorruptElement.into());
                }
                Err(_) => continue,
            };
            Some(String::from_utf8_lossy(&stored).into_owned())
        };
        rendered.push((
            id.to_owned(),
            index.ann.metric.score(distance, layout.dim),
            attr,
        ));
    }

    out.push_str(&query_header(rendered.len(), index.rebuilding_progress()));
    // 행 하나하나가 문자열을 늘리는데, `write!`가 쓰는 늘림은 못 잡으면
    // abort다. 들 것을 이미 손에 쥐고 있으니 정확히 재서 한 번에 잡고, 그
    // 자리가 없으면 답을 짓기 전에 거절한다.
    //
    // 행당 덧붙는 것은 점수와 공백 둘과 CRLF다. `f32`의 `Display`는 지수
    // 표기를 쓰지 않아 비정규수에서 48자까지 간다 -- 부호 하나에 `0.` 둘,
    // 그리고 소수점 아래 45자리. 거기에 공백·CRLF 넷을 더해도 `SCORE_BYTES`
    // 안쪽이다. **넉넉해야 하는 이유가 있다**: 모자라게 잡으면 뒤의 `write!`가
    // 무르게 늘려 버려, 여기서 거절하려던 그 실패가 abort로 돌아온다.
    const SCORE_BYTES: usize = 64;
    let body_bytes = rendered
        .iter()
        .map(|(id, _, attr)| id.len() + attr.as_ref().map_or(0, String::len) + SCORE_BYTES)
        .try_fold(0usize, |sum, n| sum.checked_add(n))
        .ok_or_else(|| Error::Index("answer does not fit in memory".into()))?;
    crate::room::reserve_str(out, body_bytes)?;
    for (id, score, attr) in rendered {
        match attr {
            // attr은 공백 없는 JSON 한 덩어리라 길이를 앞세울 필요가 없다.
            Some(attr) => {
                let _ = write!(out, "{id} {score} {attr}\r\n");
            }
            None => {
                let _ = write!(out, "{id} {score}\r\n");
            }
        }
    }
    Ok(())
}

pub fn vsim_vector(store: &Store, spec: &Sim, body: &[u8]) -> Result<Reply> {
    let Sim {
        index: name,
        k,
        filter,
        with_attr,
    } = spec;
    let (k, with_attr) = (*k, *with_attr);
    let filter = filter.as_ref();

    let index = for_read(store, name)?;
    let layout = index.ann.layout;

    let mut out = String::new();
    for query in coord_vectors(body, layout.dim, "query")?.iter() {
        let quantized = quant::encode(query, layout.quant)?;
        similar(store, &index, &quantized, k, filter, with_attr, &mut out)?;
    }
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

pub fn vsim_key(store: &Store, spec: &SimKey) -> Result<Reply> {
    let SimKey {
        index: name,
        key,
        k,
        filter,
        with_attr,
    } = spec;
    let (k, with_attr, filter) = (*k, *with_attr, filter.as_ref());

    let index = for_read(store, name)?;
    let addr = match store.hold_elem(&crate::trigger::key::map_key(name), key) {
        Ok(addr) => addr,
        // The query vector's own element. Gone means gone: this is a NOT_FOUND
        // for the query, never a verdict on the index.
        Err(StoreError::ElemGone | StoreError::KeyGone) => return Ok(Reply::NotFound),
        Err(e) => return Err(e.into()),
    };

    // The graph may not hold this element yet, but the element always carries
    // the vector: read it from there so `vsearch KEY` works either way.
    let layout = index.ann.layout;
    let query = match index.ann.vector_of(addr) {
        Ok(Some(query)) => Ok(query),
        Ok(None) => store
            .with_elem_at(addr, |_field, value| {
                layout.vector_of(value).map(<[u8]>::to_vec)
            })
            .flatten()
            .ok_or(Error::Unreadable),
        Err(e) => Err(e),
    };
    // Only taken to read the query out; the graph keeps its own.
    store.release_elems(&[addr]);
    let query = query?;

    let mut out = String::new();
    similar(store, &index, &query, k, filter, with_attr, &mut out)?;
    out.push_str("END\r\n");
    Ok(Reply::Body(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Rebuild;

    #[test]
    fn a_whole_index_writes_the_plain_header() {
        assert_eq!(query_header(5, None), "VECTORS 5\r\n");
    }

    #[test]
    fn a_half_built_index_says_what_it_answered_from() {
        let half = Rebuild {
            done: 693_000,
            total: 999_000,
        };
        assert_eq!(
            query_header(5, Some(half)),
            "PARTIAL_VECTORS 5 693000/999000\r\n"
        );
    }

    #[test]
    fn a_partial_query_that_found_nothing_still_reports_progress() {
        let just_begun = Rebuild {
            done: 0,
            total: 999_000,
        };
        assert_eq!(
            query_header(0, Some(just_begun)),
            "PARTIAL_VECTORS 0 0/999000\r\n"
        );
    }
}
