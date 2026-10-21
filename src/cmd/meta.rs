use std::fmt::Write as _;

use super::vector::{check_names, meta_of_stored};
use crate::cmd::access::{map_is_gone, resolve};
use crate::engine::{Store, StoreError};
use crate::error::{Error, Reply, Result};
use crate::index::registry;
use crate::protocol::request::GetMeta;

/// `META <vkey> <metabytes>\r\n<json>\r\n`, where `metabytes` is the JSON's byte length.
fn render(vkey: &str, json: &str) -> Result<String> {
    let mut out = String::new();
    // Name, JSON and the fixed part (`META `, a space, ~20 digits, two CRLFs) in one reservation.
    crate::fallible::reserve_str(&mut out, vkey.len() + json.len() + 40)?;
    let _ = write!(out, "META {vkey} {}\r\n{json}\r\n", json.len());
    Ok(out)
}

pub fn getmeta(store: &Store, spec: &GetMeta) -> Result<Reply> {
    let GetMeta { index: name, vkey } = *spec;
    check_names(name, vkey)?;
    // Reads only the element, not the graph, so halted indexes answer too (not `for_read`).
    let index = resolve(store, name)?;
    let stamp = registry::now();
    let layout = index.ann.layout;

    match store
        .get_elem(name, vkey)
        .and_then(|body| meta_of_stored(layout, &body))
    {
        Ok(json) => Ok(Reply::Body(render(vkey, &json)?)),
        Err(StoreError::ElemGone) => Ok(Reply::NotFoundVector),
        Err(StoreError::CorruptElement) => {
            // Not removed from the graph here: when the element actually goes away, the unlink
            // event arrives and the callback removes the node.
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
        // The length is in bytes, not characters.
        assert_eq!(
            render("v1", r#"{"t":"한"}"#).unwrap(),
            "META v1 11\r\n{\"t\":\"한\"}\r\n"
        );
        assert_eq!(render("v1", "{}").unwrap(), "META v1 2\r\n{}\r\n");
    }
}
