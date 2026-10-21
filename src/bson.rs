//! BSON codec for vector attributes and index settings.
//!
//! Conversion, encoding/decoding and field lookup are all done by the `bson` crate (the official
//! MongoDB implementation).
//!
//! JSON → BSON follows the crate's extended JSON rules: a field value written in a reserved form
//! such as `{"$numberLong":"5"}` is stored as that type (int64 5). Keys of the top-level object
//! are always read as field names -- interpreting the top level too would make attributes like
//! `{"$date":..,"a":..}` parse as a date and get rejected.

use std::fmt;

use bson::raw::RawBsonRef;
use bson::{Bson, Document, RawDocument};
use serde_json::Value as Json;

/// Maximum nesting depth of objects/arrays.
pub const MAX_DEPTH: usize = 32;

/// The empty document used by elements without attributes.
pub const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BsonError {
    NotAnObject,
    BadJson(String),
    Malformed,
    TooDeep,
}

impl fmt::Display for BsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => f.write_str("metadata must be a JSON object"),
            Self::BadJson(m) => write!(f, "invalid JSON: {m}"),
            Self::Malformed => f.write_str("malformed BSON document"),
            Self::TooDeep => write!(f, "nesting deeper than {MAX_DEPTH}"),
        }
    }
}

impl std::error::Error for BsonError {}

impl From<BsonError> for crate::error::Error {
    fn from(e: BsonError) -> Self {
        Self::bad_request(e.to_string())
    }
}

/// The value of a top-level field, distinguishing only what filter comparisons need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Num(f64),
    Str(&'a [u8]),
    Bool(bool),
    Null,
    Other,
}

/// The document length given by the first 4 bytes (LE i32). `Some` only if it is at least 5 and
/// fits in the buffer.
///
/// Element values carry the vector right after the document, so this length is used to slice out
/// just the document before handing it to the crate.
pub fn doc_len(buf: &[u8]) -> Option<usize> {
    let head: [u8; 4] = buf.get(..4)?.try_into().ok()?;
    let n = usize::try_from(i32::from_le_bytes(head)).ok()?;
    (n >= 5 && n <= buf.len()).then_some(n)
}

/// Validates and opens the leading document, ignoring any trailing bytes (the vector).
fn raw(buf: &[u8]) -> Option<&RawDocument> {
    let n = doc_len(buf)?;
    RawDocument::from_bytes(&buf[..n]).ok()
}

pub fn from_json(json: &[u8]) -> Result<Vec<u8>, BsonError> {
    let parsed: Json = serde_json::from_slice(json).map_err(|e| {
        if e.to_string().contains("recursion limit") {
            BsonError::TooDeep
        } else {
            BsonError::BadJson(e.to_string())
        }
    })?;
    let Json::Object(map) = parsed else {
        return Err(BsonError::NotAnObject);
    };
    if map.values().any(|v| depth(v) >= MAX_DEPTH) {
        return Err(BsonError::TooDeep);
    }
    // Rejects NUL in keys and malformed reserved forms (`{"$numberLong":"x"}`).
    let doc = Document::try_from(map).map_err(|e| BsonError::BadJson(e.to_string()))?;
    doc.to_vec().map_err(|e| BsonError::BadJson(e.to_string()))
}

/// Nesting depth of a value: 0 for scalars, 1 + the deepest child for objects and arrays.
fn depth(v: &Json) -> usize {
    match v {
        Json::Object(m) => 1 + m.values().map(depth).max().unwrap_or(0),
        Json::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Compact JSON. Uses the crate's relaxed extended JSON, so the types we produce come out as
/// plain JSON (doubles keep a fraction, e.g. `1.0`).
pub fn to_json(doc: &[u8]) -> Result<String, BsonError> {
    let raw = raw(doc).ok_or(BsonError::Malformed)?;
    let doc = Document::try_from(raw).map_err(|_| BsonError::Malformed)?;
    serde_json::to_string(&Bson::Document(doc).into_relaxed_extjson())
        .map_err(|_| BsonError::Malformed)
}

/// Looks up a top-level field. `None` if the document is malformed or lacks the field.
///
/// Searches the raw bytes without decoding or copying the document -- the filter calls this for
/// every node visited during a search.
pub fn lookup<'a>(doc: &'a [u8], field: &[u8]) -> Option<Value<'a>> {
    let key = std::str::from_utf8(field).ok()?;
    Some(match raw(doc)?.get(key).ok()?? {
        RawBsonRef::Double(f) => Value::Num(f),
        RawBsonRef::Int32(i) => Value::Num(f64::from(i)),
        RawBsonRef::Int64(i) => Value::Num(i as f64),
        RawBsonRef::String(s) => Value::Str(s.as_bytes()),
        RawBsonRef::Boolean(b) => Value::Bool(b),
        RawBsonRef::Null => Value::Null,
        _ => Value::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        to_json(&from_json(s.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn round_trips_every_scalar_type() {
        let s = r#"{"a":1,"b":-5000000000,"c":1.5,"d":"x y","e":true,"f":null}"#;
        assert_eq!(rt(s), s);
    }

    #[test]
    fn nested_objects_and_arrays_round_trip() {
        let s = r#"{"o":{"p":[1,"two",{"q":false}]}}"#;
        assert_eq!(rt(s), s);
    }

    #[test]
    fn key_order_is_kept() {
        assert_eq!(rt(r#"{"z":1,"a":2}"#), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn only_an_object_becomes_a_document() {
        for s in ["[1,2]", "\"x\"", "1"] {
            assert_eq!(from_json(s.as_bytes()), Err(BsonError::NotAnObject), "{s}");
        }
        for s in ["", "{"] {
            assert!(
                matches!(from_json(s.as_bytes()), Err(BsonError::BadJson(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn empty_object_is_the_five_byte_document() {
        assert_eq!(from_json(b"{}").unwrap(), EMPTY);
        assert_eq!(to_json(&EMPTY).unwrap(), "{}");
    }

    #[test]
    fn lookup_reads_top_level_fields_only() {
        let d = from_json(br#"{"n":3,"s":"v","o":{"n":9}}"#).unwrap();
        assert_eq!(lookup(&d, b"n"), Some(Value::Num(3.0)));
        assert_eq!(lookup(&d, b"s"), Some(Value::Str(b"v")));
        assert_eq!(lookup(&d, b"o"), Some(Value::Other));
        assert_eq!(lookup(&d, b"x"), None);
    }

    #[test]
    fn doc_len_refuses_short_or_lying_headers() {
        assert_eq!(doc_len(&[4, 0, 0, 0]), None);
        assert_eq!(doc_len(&[9, 0, 0, 0, 0]), None);
        assert_eq!(doc_len(&[255, 255, 255, 255, 0]), None);
        assert_eq!(doc_len(&EMPTY), Some(5));
    }

    #[test]
    fn a_truncated_document_is_malformed() {
        let d = from_json(br#"{"a":1}"#).unwrap();
        assert_eq!(to_json(&d[..d.len() - 1]), Err(BsonError::Malformed));
    }

    #[test]
    fn doubles_print_with_a_fraction_and_depth_is_capped() {
        assert_eq!(rt(r#"{"a":2.0}"#), r#"{"a":2.0}"#);
        let deep = |n: usize| format!("{}1{}", "[".repeat(n), "]".repeat(n));
        let ok = format!(r#"{{"a":{}}}"#, deep(MAX_DEPTH - 1));
        assert!(from_json(ok.as_bytes()).is_ok());
        let bad = format!(r#"{{"a":{}}}"#, deep(MAX_DEPTH));
        assert_eq!(from_json(bad.as_bytes()), Err(BsonError::TooDeep));
    }

    #[test]
    fn extended_json_values_take_their_type() {
        // `{"$numberLong":"5"}` means int64 5.
        let d = from_json(br#"{"price":{"$numberLong":"5"},"n":1}"#).unwrap();
        assert_eq!(lookup(&d, b"price"), Some(Value::Num(5.0)));
        assert_eq!(to_json(&d).unwrap(), r#"{"price":5,"n":1}"#);
    }

    #[test]
    fn top_level_keys_are_always_field_names() {
        let s = r#"{"$date":1,"a":2}"#;
        assert_eq!(rt(s), s);
    }

    #[test]
    fn a_malformed_extended_json_value_is_refused() {
        assert!(matches!(
            from_json(br#"{"price":{"$numberLong":"x"}}"#),
            Err(BsonError::BadJson(_))
        ));
    }

    #[test]
    fn a_nul_in_a_key_is_refused() {
        assert!(matches!(
            from_json(b"{\"a\\u0000b\":1}"),
            Err(BsonError::BadJson(_))
        ));
    }

    #[test]
    fn bytes_after_the_document_are_ignored() {
        // Element values carry the vector after the document.
        let mut v = from_json(br#"{"a":7}"#).unwrap();
        v.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(lookup(&v, b"a"), Some(Value::Num(7.0)));
        assert_eq!(to_json(&v).unwrap(), r#"{"a":7}"#);
    }
}
