//! 벡터 속성과 인덱스 설정을 담는 최소 BSON 코덱.
//!
//! JSON 객체 <-> BSON 문서 변환과, 필터가 쓰는 최상위 필드 조회만 갖춘다.
//! 지원 타입은 double, string, 객체, 배열, bool, null, int32, int64뿐이다.
//! 디코더는 모든 읽기를 바운드 검사하고, 어긋나면 패닉 대신 `Malformed`를 낸다.

use std::fmt;

use serde_json::{Map, Value as Json};

/// 중첩(객체/배열) 깊이 상한.
pub const MAX_DEPTH: usize = 32;

/// 속성이 없는 요소가 쓰는 빈 문서.
pub const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];

const T_DOUBLE: u8 = 0x01;
const T_STRING: u8 = 0x02;
const T_DOC: u8 = 0x03;
const T_ARRAY: u8 = 0x04;
const T_BOOL: u8 = 0x08;
const T_NULL: u8 = 0x0A;
const T_INT32: u8 = 0x10;
const T_INT64: u8 = 0x12;

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

/// 최상위 필드 하나의 값. 필터 비교에 필요한 것만 구분한다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Num(f64),
    Str(&'a [u8]),
    Bool(bool),
    Null,
    Other,
}

/// 첫 4바이트(LE i32)가 가리키는 문서 길이. 5 이상이고 버퍼 안에 들 때만 `Some`.
pub fn doc_len(buf: &[u8]) -> Option<usize> {
    let head: [u8; 4] = buf.get(..4)?.try_into().ok()?;
    let n = usize::try_from(i32::from_le_bytes(head)).ok()?;
    (n >= 5 && n <= buf.len()).then_some(n)
}

// ---------------------------------------------------------------- 인코더

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
    let mut out = Vec::new();
    enc_doc(&map, 1, &mut out)?;
    Ok(out)
}

fn enc_cstr(s: &str, out: &mut Vec<u8>) -> Result<(), BsonError> {
    if s.as_bytes().contains(&0) {
        return Err(BsonError::BadJson("key contains NUL".into()));
    }
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

fn enc_len(n: usize) -> Result<[u8; 4], BsonError> {
    i32::try_from(n)
        .map(i32::to_le_bytes)
        .map_err(|_| BsonError::BadJson("document too large".into()))
}

/// 길이 자리를 비워 두고 쓴 뒤 채운다. `depth`는 이 문서가 몇 번째 중첩인지(최상위 = 1).
fn enc_doc(map: &Map<String, Json>, depth: usize, out: &mut Vec<u8>) -> Result<(), BsonError> {
    if depth > MAX_DEPTH {
        return Err(BsonError::TooDeep);
    }
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (k, v) in map {
        enc_elem(k, v, depth, out)?;
    }
    out.push(0);
    let len = enc_len(out.len() - start)?;
    out[start..start + 4].copy_from_slice(&len);
    Ok(())
}

fn enc_array(items: &[Json], depth: usize, out: &mut Vec<u8>) -> Result<(), BsonError> {
    if depth > MAX_DEPTH {
        return Err(BsonError::TooDeep);
    }
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (i, v) in items.iter().enumerate() {
        enc_elem(&i.to_string(), v, depth, out)?;
    }
    out.push(0);
    let len = enc_len(out.len() - start)?;
    out[start..start + 4].copy_from_slice(&len);
    Ok(())
}

fn enc_elem(key: &str, v: &Json, depth: usize, out: &mut Vec<u8>) -> Result<(), BsonError> {
    let tag_at = out.len();
    out.push(0);
    enc_cstr(key, out)?;
    let tag = match v {
        Json::Null => T_NULL,
        Json::Bool(b) => {
            out.push(u8::from(*b));
            T_BOOL
        }
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Ok(i) = i32::try_from(i) {
                    out.extend_from_slice(&i.to_le_bytes());
                    T_INT32
                } else {
                    out.extend_from_slice(&i.to_le_bytes());
                    T_INT64
                }
            } else {
                let f = n
                    .as_f64()
                    .ok_or_else(|| BsonError::BadJson("number out of range".into()))?;
                out.extend_from_slice(&f.to_le_bytes());
                T_DOUBLE
            }
        }
        Json::String(s) => {
            out.extend_from_slice(&enc_len(s.len() + 1)?);
            out.extend_from_slice(s.as_bytes());
            out.push(0);
            T_STRING
        }
        Json::Object(m) => {
            enc_doc(m, depth + 1, out)?;
            T_DOC
        }
        Json::Array(a) => {
            enc_array(a, depth + 1, out)?;
            T_ARRAY
        }
    };
    out[tag_at] = tag;
    Ok(())
}

// ---------------------------------------------------------------- 디코더

/// 바운드 검사를 거치는 바이트 리더.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BsonError> {
        let end = self.pos.checked_add(n).ok_or(BsonError::Malformed)?;
        let s = self.buf.get(self.pos..end).ok_or(BsonError::Malformed)?;
        self.pos = end;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, BsonError> {
        Ok(self.take(1)?[0])
    }

    fn i32(&mut self) -> Result<i32, BsonError> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| BsonError::Malformed)?;
        Ok(i32::from_le_bytes(b))
    }

    fn i64(&mut self) -> Result<i64, BsonError> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| BsonError::Malformed)?;
        Ok(i64::from_le_bytes(b))
    }

    fn f64(&mut self) -> Result<f64, BsonError> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    /// NUL로 끝나는 키. NUL은 읽되 돌려주지 않는다.
    fn cstr(&mut self) -> Result<&'a [u8], BsonError> {
        let rest = self.buf.get(self.pos..).ok_or(BsonError::Malformed)?;
        let n = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(BsonError::Malformed)?;
        self.pos += n + 1;
        Ok(&rest[..n])
    }

    /// 길이 접두 문자열(끝 NUL 포함 길이). 본문만 돌려준다.
    fn string(&mut self) -> Result<&'a [u8], BsonError> {
        let n = usize::try_from(self.i32()?).map_err(|_| BsonError::Malformed)?;
        if n == 0 {
            return Err(BsonError::Malformed);
        }
        let s = self.take(n)?;
        let (last, body) = s.split_last().ok_or(BsonError::Malformed)?;
        if *last != 0 {
            return Err(BsonError::Malformed);
        }
        Ok(body)
    }

    /// 중첩 문서/배열의 본문 범위(앞 4바이트 길이 포함)를 잘라 새 리더로 준다.
    fn subdoc(&mut self) -> Result<&'a [u8], BsonError> {
        let rest = self.buf.get(self.pos..).ok_or(BsonError::Malformed)?;
        let n = doc_len(rest).ok_or(BsonError::Malformed)?;
        self.take(n)
    }
}

/// 문서 하나를 `[4바이트 길이][요소...][0]`으로 검증하며 열고 요소 영역 리더를 준다.
fn open(doc: &[u8]) -> Result<Reader<'_>, BsonError> {
    let n = doc_len(doc).ok_or(BsonError::Malformed)?;
    if doc[n - 1] != 0 {
        return Err(BsonError::Malformed);
    }
    Ok(Reader {
        buf: &doc[..n - 1],
        pos: 4,
    })
}

pub fn to_json(doc: &[u8]) -> Result<String, BsonError> {
    let mut out = String::new();
    dec_doc(doc, false, 1, &mut out)?;
    Ok(out)
}

fn dec_doc(doc: &[u8], array: bool, depth: usize, out: &mut String) -> Result<(), BsonError> {
    if depth > MAX_DEPTH {
        return Err(BsonError::TooDeep);
    }
    let mut r = open(doc)?;
    out.push(if array { '[' } else { '{' });
    let mut first = true;
    while r.pos < r.buf.len() {
        let tag = r.byte()?;
        let key = std::str::from_utf8(r.cstr()?).map_err(|_| BsonError::Malformed)?;
        if !first {
            out.push(',');
        }
        first = false;
        if !array {
            push_str(key, out)?;
            out.push(':');
        }
        match tag {
            T_DOUBLE => push_f64(r.f64()?, out),
            T_STRING => {
                let s = std::str::from_utf8(r.string()?).map_err(|_| BsonError::Malformed)?;
                push_str(s, out)?;
            }
            T_DOC | T_ARRAY => dec_doc(r.subdoc()?, tag == T_ARRAY, depth + 1, out)?,
            T_BOOL => match r.byte()? {
                0 => out.push_str("false"),
                1 => out.push_str("true"),
                _ => return Err(BsonError::Malformed),
            },
            T_NULL => out.push_str("null"),
            T_INT32 => out.push_str(&r.i32()?.to_string()),
            T_INT64 => out.push_str(&r.i64()?.to_string()),
            _ => return Err(BsonError::Malformed),
        }
    }
    out.push(if array { ']' } else { '}' });
    Ok(())
}

fn push_str(s: &str, out: &mut String) -> Result<(), BsonError> {
    let quoted = serde_json::to_string(s).map_err(|_| BsonError::Malformed)?;
    out.push_str(&quoted);
    Ok(())
}

/// 정수값이면 `1.0`, 아니면 Rust `{}` 표기. JSON에 없는 NaN/무한대는 null로 낸다.
fn push_f64(f: f64, out: &mut String) {
    if !f.is_finite() {
        out.push_str("null");
        return;
    }
    let s = f.to_string();
    out.push_str(&s);
    if !s.contains('.') {
        out.push_str(".0");
    }
}

/// 최상위 필드 하나를 찾는다. 잘못된 문서이거나 모르는 타입을 만나면 `None`.
pub fn lookup<'a>(doc: &'a [u8], field: &[u8]) -> Option<Value<'a>> {
    let mut r = open(doc).ok()?;
    while r.pos < r.buf.len() {
        let tag = r.byte().ok()?;
        let key = r.cstr().ok()?;
        let hit = key == field;
        let v = match tag {
            T_DOUBLE => Value::Num(r.f64().ok()?),
            T_STRING => Value::Str(r.string().ok()?),
            T_DOC | T_ARRAY => {
                r.subdoc().ok()?;
                Value::Other
            }
            T_BOOL => Value::Bool(match r.byte().ok()? {
                0 => false,
                1 => true,
                _ => return None,
            }),
            T_NULL => Value::Null,
            T_INT32 => Value::Num(f64::from(r.i32().ok()?)),
            T_INT64 => Value::Num(r.i64().ok()? as f64),
            _ => return None,
        };
        if hit {
            return Some(v);
        }
    }
    None
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
}
