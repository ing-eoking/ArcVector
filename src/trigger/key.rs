//! The one place the index key's shape is written down.
//!
//! An index is one arcus Map whose key is the index name itself. Every vector
//! is one element of it, the vector's id is the element's field, and the
//! index's settings are the Map item's own value.
//!
//! Indexes and plain keys share one key space -- that is what `TYPE_MISMATCH`
//! means on the wire: the key is there, but it is not an index.
//!
//! Nothing here touches the engine or a thread, so the whole format is unit
//! tested rather than inferred from a running server.

/// `KEY_MAX_LENGTH` in the daemon. The name is the key, so this is the longest
/// name an index can have.
pub const MAX_INDEX_NAME: usize = 250;

/// `MAX_FIELD_LENG` in the daemon. A vector id is a Map field, so this is
/// the longest id a `vadd` can store.
pub const MAX_ID: usize = 250;

/// Rejects names the key shape cannot carry.
///
/// A space is not in `mc_isnamechar` and would split the key on the wire.
pub fn valid_index_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_INDEX_NAME && !name.contains(' ')
}

/// The index a Map key names, or `None` when the key cannot be one.
///
/// The key *is* the name. Whether the Map behind it is an index of ours is
/// not decided here -- that is the registry's answer, which the Map's link
/// event filled in from the Map's value.
pub fn parse(key: &[u8]) -> Option<&str> {
    let name = std::str::from_utf8(key).ok()?;
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_name_is_its_own_key() {
        assert_eq!(parse(b"photos"), Some("photos"));
        // 접두사가 없다. 콜론이 들어도 이름의 일부다.
        assert_eq!(parse(b"a:b:c"), Some("a:b:c"));
        assert_eq!(parse(b"arcus_event:photos"), Some("arcus_event:photos"));
        assert!(parse(b"").is_none(), "빈 이름은 없다");
        assert!(
            parse(b"\xff\xfe").is_none(),
            "UTF-8이 아닌 키는 이름이 아니다"
        );
    }

    #[test]
    fn names_with_spaces_or_over_250_bytes_are_refused() {
        assert!(valid_index_name("photos"));
        assert!(valid_index_name(&"a".repeat(250)));
        assert!(!valid_index_name(&"a".repeat(251)));
        assert!(!valid_index_name(""));
        assert!(!valid_index_name("a b"));
        assert_eq!(MAX_INDEX_NAME, 250);
    }
}
