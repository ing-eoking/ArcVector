//! The one place the index key's shape is written down.
//!
//! An index is one arcus Map at `arcus_event:<index>`. Every vector is one
//! element of it, the vector's id is the element's field, and the index's
//! metadata sits in the same Map under a field no client can name.
//!
//! Nothing here touches the engine or a thread, so the whole format is unit
//! tested rather than inferred from a running server.

/// What `do_map_item_alloc` compares the front of a key against before it
/// marks the Map `COLL_META_FLAG_EVENT`.
///
/// Any Map whose key starts with this fires the callbacks -- including one a
/// client created by hand with `mop create`, which is why the metadata field
/// is what actually decides whether a Map is an index of ours.
pub const PREFIX: &str = "arcus_event:";

/// `KEY_MAX_LENGTH` (250) less [`PREFIX`].
pub const MAX_INDEX_NAME: usize = 250 - PREFIX.len();

/// `MAX_FIELD_LENG` in the daemon. A vector id is a Map field, so this is
/// the longest id a `vadd` can store.
pub const MAX_ID: usize = 250;

/// Rejects names the key shape cannot carry.
///
/// A space is not in `mc_isnamechar` and would split the key on the wire.
pub fn valid_index_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_INDEX_NAME && !name.contains(' ')
}

/// The key of the Map one index lives in.
pub fn map_key(index: &str) -> String {
    format!("{PREFIX}{index}")
}

/// The field an index's metadata lives under.
///
/// The space is what makes it safe. A client's id arrives as one ASCII
/// protocol token, and the tokeniser splits on spaces -- so no `vadd` can ever
/// name this field, and no vector can land on the metadata.
pub const META_ID: &str = " meta";

/// Whether a field names an index's metadata rather than one of its vectors.
pub fn is_meta(field: &[u8]) -> bool {
    field == META_ID.as_bytes()
}

/// The index a Map key names, or `None` when the key is not ours.
///
/// The engine already compared the prefix before it raised the event; this
/// is the check that decides, and it is total -- every key that is not
/// exactly `arcus_event:<name>` with a non-empty UTF-8 name is refused.
pub fn parse(key: &[u8]) -> Option<&str> {
    let key = std::str::from_utf8(key).ok()?;
    let index = key.strip_prefix(PREFIX)?;
    (!index.is_empty()).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_key_round_trips() {
        let key = map_key("photos");
        assert_eq!(key, "arcus_event:photos");
        assert_eq!(parse(key.as_bytes()), Some("photos"));
    }

    #[test]
    fn no_id_a_client_can_send_is_the_metadata_field() {
        // 클라이언트의 id는 ASCII 토큰 하나로 도착하고, 토큰화는 공백에서
        // 쪼개진다. 그래서 공백이 든 id는 `vadd`로 만들 수가 없다 -- 그것이
        // 메타가 벡터와 부딪히지 않는 이유 전부다.
        assert!(META_ID.contains(' '));
        assert!(is_meta(META_ID.as_bytes()));
        for id in ["meta", "", "v42", "meta ", "arcus_event"] {
            assert!(!is_meta(id.as_bytes()), "'{id}'가 메타로 잡히면 안 된다");
        }
    }

    #[test]
    fn a_name_may_contain_colons() {
        assert_eq!(parse(b"arcus_event:a:b:c"), Some("a:b:c"));
    }

    #[test]
    fn keys_that_are_not_ours_are_refused() {
        assert!(parse(b"arcus_eventful:foo").is_none());
        assert!(parse(b"arcus_event").is_none());
        assert!(parse(b"arcus_event:").is_none(), "empty index name");
        assert!(parse(b"aaa").is_none());
    }

    #[test]
    fn a_key_that_is_not_utf8_is_refused() {
        assert!(parse(b"arcus_event:\xff\xfe").is_none());
    }

    #[test]
    fn index_names_are_bounded_and_have_no_spaces() {
        assert!(valid_index_name("photos"));
        assert!(valid_index_name(&"a".repeat(MAX_INDEX_NAME)));
        assert!(!valid_index_name(&"a".repeat(MAX_INDEX_NAME + 1)));
        assert!(!valid_index_name(""));
        assert!(!valid_index_name("a b"));
        assert_eq!(map_key(&"a".repeat(MAX_INDEX_NAME)).len(), 250);
    }
}
