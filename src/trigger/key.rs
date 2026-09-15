//! The one place the trigger key's shape is written down.
//!
//! Nothing here touches the engine or a thread, so the whole format is unit
//! tested rather than inferred from a running server -- the same reason
//! `repl/wire.rs` was its own module before this design replaced it.

/// What `do_item_alloc` compares the first eleven bytes of a key against
/// before it sets `ITEM_WITH_EVENT`.
///
/// Any key starting with this fires the callback -- including one a client
/// wrote by hand, which is why `parse` below is the check that actually
/// decides what is ours.
pub const PREFIX: &str = "arcus_event";

/// `PREFIX_MAX_LENGTH` (250) less `arcus_event{}` (13).
///
/// The daemon derives a prefix name from every key that contains the
/// delimiter, and for ours that name is the whole `arcus_event{index}`. A
/// longer one is refused outright with `ENGINE_PREFIX_ENAME`, before the item
/// is ever linked -- so this is a hard bound, not a policy.
pub const MAX_INDEX_NAME: usize = 237;

/// Rejects names the key shape cannot carry.
///
/// `{` and `}` would end the name early and `:` would be read as the
/// delimiter. A space is refused for a different reason: it is not in
/// `mc_isnamechar`, so the prefix name the daemon derives would be invalid and
/// the write would fail at `prefix_link` with no item linked and no callback.
pub fn valid_index_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_INDEX_NAME && !name.contains(['{', '}', ':', ' '])
}

/// The key one vector lives at.
pub fn vector_key(index: &str, id: &str) -> String {
    format!("{PREFIX}{{{index}}}:{id}")
}

/// The key an index's metadata lives at -- a vector key with an empty id.
pub fn meta_key(index: &str) -> String {
    vector_key(index, "")
}

/// The prefix the daemon derives from any of this index's keys: everything
/// before the first delimiter. `flush` takes this, and `stats prefix` reports
/// under it.
pub fn index_prefix(index: &str) -> String {
    format!("{PREFIX}{{{index}}}")
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub index: &'a str,
    /// Empty for the index's own metadata key.
    pub id: &'a str,
}

/// Splits a key the hook handed over, or `None` when it is not ours.
///
/// The hook compares ten bytes and one brace, which still lets a client key
/// through if one happens to start that way; this is the check that actually
/// decides, and it is deliberately total -- every shape that is not exactly
/// `arcus_event{name}:rest` with a non-empty name is refused.
pub fn parse(key: &[u8]) -> Option<Parsed<'_>> {
    let key = std::str::from_utf8(key).ok()?;
    let rest = key.strip_prefix(PREFIX)?.strip_prefix('{')?;
    let close = rest.find('}')?;
    let (index, after) = rest.split_at(close);
    if index.is_empty() {
        return None;
    }
    let id = after.strip_prefix("}:")?;
    Some(Parsed { index, id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_key_round_trips() {
        let key = vector_key("photos", "v42");
        assert_eq!(key, "arcus_event{photos}:v42");

        let parsed = parse(key.as_bytes()).expect("parses");
        assert_eq!(parsed.index, "photos");
        assert_eq!(parsed.id, "v42");
    }

    #[test]
    fn a_meta_key_parses_with_an_empty_id() {
        let key = meta_key("photos");
        assert_eq!(key, "arcus_event{photos}:");

        let parsed = parse(key.as_bytes()).expect("parses");
        assert_eq!(parsed.index, "photos");
        assert_eq!(parsed.id, "");
    }

    #[test]
    fn an_id_may_contain_colons() {
        let parsed = parse(b"arcus_event{ix}:a:b:c").expect("parses");
        assert_eq!(parsed.index, "ix");
        assert_eq!(parsed.id, "a:b:c");
    }

    #[test]
    fn keys_that_are_not_ours_are_refused() {
        // The hook's eleven-byte compare lets the first of these through, so the
        // parser is what keeps a client's key out of the graph.
        assert!(parse(b"arcus_eventful:foo").is_none());
        assert!(parse(b"arcus_event").is_none());
        assert!(parse(b"arcus_event{").is_none());
        assert!(
            parse(b"arcus_event{ix}").is_none(),
            "no delimiter after the name"
        );
        assert!(parse(b"arcus_event{}:v1").is_none(), "empty index name");
        assert!(
            parse(b"arcus_event{ix}x:v1").is_none(),
            "brace not followed by ':'"
        );
        assert!(parse(b"aaa").is_none());
    }

    #[test]
    fn a_key_that_is_not_utf8_is_refused() {
        assert!(parse(b"arcus_event{\xff\xfe}:v1").is_none());
    }

    #[test]
    fn the_index_prefix_is_what_the_daemon_derives_from_a_vector_key() {
        assert_eq!(index_prefix("photos"), "arcus_event{photos}");

        // If these two ever disagree, a flush aimed at the prefix matches
        // nothing and `vdrop` silently leaves every vector behind.
        let key = vector_key("photos", "v1");
        let derived = &key[..key.find(':').expect("has a delimiter")];
        assert_eq!(derived, index_prefix("photos"));
    }

    #[test]
    fn index_names_are_bounded_and_have_no_reserved_characters() {
        assert!(valid_index_name("photos"));
        assert!(valid_index_name(&"a".repeat(MAX_INDEX_NAME)));
        assert!(!valid_index_name(&"a".repeat(MAX_INDEX_NAME + 1)));
        assert!(!valid_index_name(""));
        assert!(!valid_index_name("a{b"));
        assert!(!valid_index_name("a}b"));
        assert!(!valid_index_name("a:b"));
        assert!(!valid_index_name("a b"));
    }

    #[test]
    fn a_name_at_the_bound_still_fits_the_prefix_the_daemon_allows() {
        // 250 is PREFIX_MAX_LENGTH in include/memcached/types.h.
        let name = "a".repeat(MAX_INDEX_NAME);
        assert!(valid_index_name(&name));
        assert_eq!(index_prefix(&name).len(), 250);
    }
}
