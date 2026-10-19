use std::cell::RefCell;
use std::fmt::Write as _;

use super::coords::vector as decode_vector;
use super::vector::{check_index, check_vkey, meta_of_stored};
use crate::command::filter::Filter;
use crate::command::request::{Parts, SearchKey, SearchVector};
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, map_is_gone};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::quant;
use crate::handler::registry::{self, VectorIndex};
use crate::handler::usearch::Accept;

/// accept 콜백이 한 노드에 내린 판정.
///
/// `Option<Option<String>>`이 아니라 이것인 이유는, 바깥 `None`이 이미
/// [`Store::with_elem_at`]의 "읽을 수 없다"로 쓰이고 있어서다.
enum Verdict {
    /// 받는다. `withmeta`면 필터가 읽어 둔 메타가 들려 간다 -- 렌더가 같은 값을
    /// 다시 읽지 않도록.
    Take(Option<String>),
    /// 필터가 걸렀다.
    Drop,
    /// 필터는 받았는데 메타를 JSON으로 내지 못했다. 저장된 BSON이 깨졌다.
    Corrupt,
}

fn judged_meta(judged: &RefCell<Vec<(u64, String)>>, key: u64) -> Option<String> {
    let judged = judged.borrow();
    judged
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, a)| a.clone())
}

/// 필터 구간을 읽는다. 있다고 선언했는데 비었으면 `Filter::parse`가 거절한다.
fn filter_of(has_filter: bool, parts: &Parts) -> Result<Option<Filter>> {
    if !has_filter {
        return Ok(None);
    }
    let text = parts
        .filter
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    let text =
        std::str::from_utf8(text).map_err(|_| Error::bad_request("filter is not valid UTF-8"))?;
    Ok(Some(Filter::parse(text)?))
}

/// 질의 하나의 결과 행. `(vkey, score, withmeta의 JSON)`.
type Row = (String, f32, Option<String>);

fn similar(
    store: &Store,
    index: &VectorIndex,
    query: &[u8],
    k: usize,
    filter: Option<&Filter>,
    with_meta: bool,
) -> Result<Vec<Row>> {
    let layout = index.ann.layout;

    let judged: RefCell<Vec<(u64, String)>> = RefCell::new(Vec::new());
    // 순회 안에서는 오류를 낼 수 없어 적어 두고, 끝난 뒤 필터 없는 길과 같은
    // `CorruptElement`로 답한다. 조용히 빼면 깨진 요소가 검색에서 사라질 뿐
    // 아무도 모른다.
    let corrupt = std::cell::Cell::new(false);
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
        let meta = match store.with_elem_at(key, |_field, value| match layout.split(value) {
            Ok((meta, _)) if filter.matches(meta) => {
                if !with_meta {
                    return Verdict::Take(None);
                }
                match crate::bson::to_json(meta) {
                    Ok(json) => Verdict::Take(Some(json)),
                    Err(_) => Verdict::Corrupt,
                }
            }
            _ => Verdict::Drop,
        }) {
            Some(Verdict::Take(meta)) => meta,
            Some(Verdict::Corrupt) => {
                corrupt.set(true);
                return false;
            }
            // 읽을 수 없거나 필터가 걸렀다.
            Some(Verdict::Drop) | None => return false,
        };

        let Some(meta) = meta else {
            // `withmeta`가 아니라 렌더에 넘겨줄 메타가 없다.
            return true;
        };

        let mut judged = judged.borrow_mut();

        if judged.try_reserve(1).is_err() {
            return false;
        }
        judged.push((key, meta));
        true
    };

    // 필터가 없어도 건다. 잠긴 인덱스의 역참조를 막는 가드가 여기 있고,
    // 판정이 순회 안에서 나야 usearch가 거절당한 만큼 더 방문해 k를 채운다.
    let accept: Option<Accept> = Some(&accepted as Accept);

    let named = index.ann.search(query, k, accept)?;
    if corrupt.get() {
        eprintln!(
            "ArcVector: an element of index '{}' matched the filter but its metadata is unreadable",
            index.name
        );
        return Err(StoreError::CorruptElement.into());
    }

    let mut rendered = crate::room::vec(named.len().min(k))?;
    for (key, id, distance) in named {
        if rendered.len() == k {
            break;
        }
        let meta = if !with_meta {
            None
        } else if let Some(meta) = judged_meta(&judged, key) {
            Some(meta)
        } else {
            let stored = match store
                .get_elem(&index.name, &id)
                .and_then(|body| meta_of_stored(layout, &body))
            {
                Ok(stored) => stored,

                // 검색과 여기 사이에 끼어든 삭제. 행 하나만 빠지고 질의는 선다.
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
            Some(stored)
        };
        rendered.push((id.to_string(), distance, meta));
    }
    Ok(rendered)
}

/// `VECTORS <count>` 머리, 행, `END`.
///
/// 행은 `<vkey> <score>`, `withmeta`면 `<vkey> <score> <metabytes>\r\n<json>`이다.
/// `metabytes`는 JSON의 바이트 수다.
fn render(rows: &[Row]) -> Result<String> {
    let mut out = String::new();
    // 행 하나하나가 문자열을 늘리는데, `write!`가 쓰는 늘림은 못 잡으면
    // abort다. 들 것을 이미 손에 쥐고 있으니 정확히 재서 한 번에 잡고, 그
    // 자리가 없으면 답을 짓기 전에 거절한다.
    //
    // 행당 덧붙는 것은 점수와 공백, 길이 자릿수, CRLF다. `f32`의 `Display`는
    // 지수 표기를 쓰지 않아 비정규수에서 48자까지 간다 -- 부호 하나에 `0.` 둘,
    // 그리고 소수점 아래 45자리. 거기에 공백과 길이(20자리), CRLF 넷을 더해도
    // `ROW_BYTES` 안쪽이다. **넉넉해야 하는 이유가 있다**: 모자라게 잡으면 뒤의
    // `write!`가 무르게 늘려 버려, 여기서 거절하려던 그 실패가 abort로 돌아온다.
    const ROW_BYTES: usize = 96;
    const HEADER_AND_END: usize = 48;
    let body_bytes = rows
        .iter()
        .map(|(id, _, meta)| id.len() + meta.as_ref().map_or(0, String::len) + ROW_BYTES)
        .try_fold(HEADER_AND_END, |sum, n| sum.checked_add(n))
        .ok_or_else(|| Error::Index("answer does not fit in memory".into()))?;
    crate::room::reserve_str(&mut out, body_bytes)?;

    let _ = write!(out, "VECTORS {}\r\n", rows.len());
    for (id, score, meta) in rows {
        match meta {
            Some(json) => {
                let _ = write!(out, "{id} {score} {}\r\n{json}\r\n", json.len());
            }
            None => {
                let _ = write!(out, "{id} {score}\r\n");
            }
        }
    }
    out.push_str("END\r\n");
    Ok(out)
}

/// **풀 스레드(아무 레인).** 벡터로 찾는다.
pub fn search_vector(store: &Store, spec: &SearchVector, body: &[u8]) -> Result<Reply> {
    let SearchVector {
        index: name,
        k,
        format,
        with_meta,
        has_filter,
        segments,
    } = spec;
    check_index(name)?;
    let parts = segments.split(body)?;
    let filter = filter_of(*has_filter, &parts)?;

    let index = for_read(store, name)?;
    let layout = index.ann.layout;
    let query = parts
        .vector
        .ok_or_else(|| Error::bad_request("bad data chunk"))?;
    let query = decode_vector(query, layout.dim, *format)?;
    let query = quant::encode(&query, layout.quant)?;

    let rows = similar(store, &index, &query, *k, filter.as_ref(), *with_meta)?;
    Ok(Reply::Body(render(&rows)?))
}

/// **풀 스레드(아무 레인).** 저장된 벡터로 찾는다.
pub fn search_key(store: &Store, spec: &SearchKey, body: &[u8]) -> Result<Reply> {
    let SearchKey {
        index: name,
        vkey,
        k,
        with_meta,
        has_filter,
        segments,
    } = spec;
    check_index(name)?;
    check_vkey(vkey)?;
    let parts = segments.split(body)?;
    let filter = filter_of(*has_filter, &parts)?;

    let index = for_read(store, name)?;
    let stamp = registry::now();
    let addr = match store.hold_elem(name, vkey) {
        Ok(addr) => addr,
        // 질의 벡터 자신이 없다. 인덱스에 대한 판정이 아니라 이 질의의 결과다.
        Err(StoreError::ElemGone) => return Ok(Reply::NotFoundVector),
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e.into()),
    };

    // 그래프에 아직 없어도 요소는 늘 벡터를 담고 있다. 거기서 읽으면 어느 쪽이든
    // 검색된다.
    let layout = index.ann.layout;
    let query = match index.ann.vector_of(addr) {
        Ok(Some(query)) => Ok(query),
        Ok(None) => store
            .with_elem_at(addr, |_field, value| {
                layout.split(value).ok().map(|(_, v)| v.to_vec())
            })
            .flatten()
            .ok_or(Error::Unreadable),
        Err(e) => Err(e),
    };
    // 질의를 읽어 내려고 잡았을 뿐이다. 그래프는 제 참조를 따로 갖고 있다.
    store.release_elems(&[addr]);
    let query = query?;

    let rows = similar(store, &index, &query, *k, filter.as_ref(), *with_meta)?;
    Ok(Reply::Body(render(&rows)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, score: f32, meta: Option<&str>) -> (String, f32, Option<String>) {
        (id.to_owned(), score, meta.map(str::to_owned))
    }

    #[test]
    fn renders_vectors_without_meta() {
        let rows = [row("a", 0.5, None), row("b", 1.0, None)];
        assert_eq!(
            render(&rows).unwrap(),
            "VECTORS 2\r\na 0.5\r\nb 1\r\nEND\r\n"
        );
        assert_eq!(render(&[]).unwrap(), "VECTORS 0\r\nEND\r\n");
    }

    #[test]
    fn renders_metabytes_as_the_json_byte_length() {
        let rows = [
            row("a", 0.25, Some(r#"{"t":"한"}"#)),
            row("b", 0.5, Some("{}")),
        ];
        // `한`은 세 바이트라 글자 수(9)가 아니라 11이다.
        assert_eq!(
            render(&rows).unwrap(),
            "VECTORS 2\r\na 0.25 11\r\n{\"t\":\"한\"}\r\nb 0.5 2\r\n{}\r\nEND\r\n"
        );
    }
}
