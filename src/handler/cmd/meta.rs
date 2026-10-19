use std::fmt::Write as _;

use super::vector::{check_names, meta_of_stored};
use crate::command::request::GetMeta;
use crate::error::{Error, Reply, Result};
use crate::handler::access::{for_read, map_is_gone};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::registry;

/// `META <vkey> <metabytes>\r\n<json>\r\n`. `metabytes`는 JSON의 바이트 수다.
fn render(vkey: &str, json: &str) -> Result<String> {
    let mut out = String::new();
    // 이름과 JSON, 고정부(`META `, 공백, 자릿수 스무 개 남짓, CRLF 둘)를 한 번에 잡는다.
    crate::room::reserve_str(&mut out, vkey.len() + json.len() + 40)?;
    let _ = write!(out, "META {vkey} {}\r\n{json}\r\n", json.len());
    Ok(out)
}

pub fn getmeta(store: &Store, spec: &GetMeta) -> Result<Reply> {
    let GetMeta { index: name, vkey } = *spec;
    check_names(name, vkey)?;
    let index = for_read(store, name)?;
    let stamp = registry::now();
    let layout = index.ann.layout;

    match store
        .get_elem(name, vkey)
        .and_then(|body| meta_of_stored(layout, &body))
    {
        Ok(json) => Ok(Reply::Body(render(vkey, &json)?)),
        Err(StoreError::ElemGone) => Ok(Reply::NotFoundVector),
        Err(StoreError::CorruptElement) => {
            // 그래프에서 빼지 않는다. 이 요소가 실제로 사라질 때 unlink 이벤트가
            // 오고, 그때 콜백이 노드를 뺀다.
            eprintln!("ArcVector: element '{vkey}' of index '{name}' is unreadable");
            Err(Error::Store(StoreError::CorruptElement))
        }
        Err(StoreError::KeyGone) => {
            map_is_gone(name, stamp);
            Ok(Reply::NotFound)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_meta_block() {
        assert_eq!(
            render("v1", r#"{"a":1}"#).unwrap(),
            "META v1 7\r\n{\"a\":1}\r\n"
        );
        // 길이는 글자 수가 아니라 바이트 수다.
        assert_eq!(
            render("v1", r#"{"t":"한"}"#).unwrap(),
            "META v1 11\r\n{\"t\":\"한\"}\r\n"
        );
        assert_eq!(render("v1", "{}").unwrap(), "META v1 2\r\n{}\r\n");
    }
}
