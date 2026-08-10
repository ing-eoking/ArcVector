//! Metadata filter expressions evaluated against the fixed filter slot.
//!
//! A filter is compiled once per query and then evaluated once per graph node
//! visited by the search. Evaluation therefore allocates nothing: it scans the
//! (tiny, flat) JSON in place rather than building a `serde_json::Value`.
//!
//! Grammar (base):
//!
//! ```text
//! expr  := term (OR term)*
//! term  := cond (AND cond)*
//! cond  := field op value
//! op    := "=" | "!=" | "<" | "<=" | ">" | ">="
//! value := number | "quoted string" | bare_string
//! ```
//!
//! Only top-level JSON fields are addressable; nested paths are a follow-up.
//! A condition on a missing field is always false — including `!=`, so that
//! `a != x OR b = y` cannot be satisfied merely by `a` being absent.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, PartialEq, Debug)]
enum Operand {
    Num(f64),
    Str(String),
}

#[derive(Clone, Debug)]
struct Cond {
    field: String,
    op: Op,
    value: Operand,
}

/// A compiled filter: an OR of ANDs (disjunctive normal form by construction,
/// since the grammar has no parentheses).
#[derive(Clone, Debug)]
pub struct Filter {
    terms: Vec<Vec<Cond>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    ExpectedField,
    ExpectedOperator(String),
    ExpectedValue(String),
    UnterminatedString,
    TrailingInput(String),
}

impl std::error::Error for ParseError {}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty filter expression"),
            ParseError::ExpectedField => write!(f, "expected a field name"),
            ParseError::ExpectedOperator(s) => write!(f, "expected an operator near '{s}'"),
            ParseError::ExpectedValue(s) => write!(f, "expected a value near '{s}'"),
            ParseError::UnterminatedString => write!(f, "unterminated quoted string"),
            ParseError::TrailingInput(s) => write!(f, "unexpected trailing input '{s}'"),
        }
    }
}

impl Filter {
    pub fn parse(src: &str) -> Result<Filter, ParseError> {
        let mut p = Parser {
            s: src.as_bytes(),
            i: 0,
        };
        p.skip_ws();
        if p.eof() {
            return Err(ParseError::Empty);
        }

        let mut terms = Vec::new();
        loop {
            terms.push(p.parse_term()?);
            p.skip_ws();
            if p.eat_keyword("OR") {
                continue;
            }
            break;
        }

        p.skip_ws();
        if !p.eof() {
            return Err(ParseError::TrailingInput(p.rest_snippet()));
        }
        Ok(Filter { terms })
    }

    /// Evaluate against a raw JSON object. Allocation-free.
    pub fn matches(&self, json: &[u8]) -> bool {
        self.terms
            .iter()
            .any(|term| term.iter().all(|c| eval_cond(c, json)))
    }
}

fn eval_cond(c: &Cond, json: &[u8]) -> bool {
    // A missing field never satisfies a condition, not even `!=`.
    let Some(found) = lookup(json, c.field.as_bytes()) else {
        return false;
    };
    match (&found, &c.value) {
        (JsonVal::Num(a), Operand::Num(b)) => cmp_ord(a.partial_cmp(b), c.op),
        (JsonVal::Str(a), Operand::Str(b)) => cmp_ord(Some((*a).cmp(b.as_bytes())), c.op),
        // A JSON number compared against an unquoted-but-non-numeric literal, or
        // a string compared against a number: only (in)equality is meaningful,
        // and they are never equal.
        (JsonVal::Bool(a), Operand::Str(b)) => match c.op {
            Op::Eq => bool_str_eq(*a, b),
            Op::Ne => !bool_str_eq(*a, b),
            _ => false,
        },
        (JsonVal::Null, Operand::Str(b)) => match c.op {
            Op::Eq => b == "null",
            Op::Ne => b != "null",
            _ => false,
        },
        _ => matches!(c.op, Op::Ne),
    }
}

fn bool_str_eq(a: bool, b: &str) -> bool {
    (a && b == "true") || (!a && b == "false")
}

fn cmp_ord(ord: Option<std::cmp::Ordering>, op: Op) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    // NaN comparisons yield None and must be false for every operator except Ne.
    let Some(o) = ord else {
        return matches!(op, Op::Ne);
    };
    match op {
        Op::Eq => o == Equal,
        Op::Ne => o != Equal,
        Op::Lt => o == Less,
        Op::Le => o != Greater,
        Op::Gt => o == Greater,
        Op::Ge => o != Less,
    }
}

// ---------------------------------------------------------------------------
// Filter expression parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn rest_snippet(&self) -> String {
        String::from_utf8_lossy(&self.s[self.i..self.s.len().min(self.i + 16)]).into_owned()
    }

    /// Consume `kw` (case-insensitive) when it appears as a whole word.
    fn eat_keyword(&mut self, kw: &str) -> bool {
        let end = self.i + kw.len();
        if end > self.s.len() {
            return false;
        }
        if !self.s[self.i..end].eq_ignore_ascii_case(kw.as_bytes()) {
            return false;
        }
        // Must not be a prefix of a longer identifier ("ANDROID" is not "AND").
        if end < self.s.len() && is_ident_byte(self.s[end]) {
            return false;
        }
        self.i = end;
        true
    }

    fn parse_term(&mut self) -> Result<Vec<Cond>, ParseError> {
        let mut conds = vec![self.parse_cond()?];
        loop {
            self.skip_ws();
            if self.eat_keyword("AND") {
                conds.push(self.parse_cond()?);
            } else {
                break;
            }
        }
        Ok(conds)
    }

    fn parse_cond(&mut self) -> Result<Cond, ParseError> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.s.len() && is_ident_byte(self.s[self.i]) {
            self.i += 1;
        }
        if self.i == start {
            return Err(ParseError::ExpectedField);
        }
        let field = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();

        self.skip_ws();
        let op = self.parse_op()?;

        self.skip_ws();
        let value = self.parse_value()?;
        Ok(Cond { field, op, value })
    }

    fn parse_op(&mut self) -> Result<Op, ParseError> {
        let two = |p: &Parser<'a>| -> Option<[u8; 2]> {
            if p.i + 1 < p.s.len() {
                Some([p.s[p.i], p.s[p.i + 1]])
            } else {
                None
            }
        };
        if let Some(pair) = two(self) {
            let op = match &pair {
                b"!=" => Some(Op::Ne),
                b"<=" => Some(Op::Le),
                b">=" => Some(Op::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.i += 2;
                return Ok(op);
            }
        }
        if self.i < self.s.len() {
            let op = match self.s[self.i] {
                b'=' => Some(Op::Eq),
                b'<' => Some(Op::Lt),
                b'>' => Some(Op::Gt),
                _ => None,
            };
            if let Some(op) = op {
                self.i += 1;
                return Ok(op);
            }
        }
        Err(ParseError::ExpectedOperator(self.rest_snippet()))
    }

    fn parse_value(&mut self) -> Result<Operand, ParseError> {
        if self.eof() {
            return Err(ParseError::ExpectedValue(String::new()));
        }
        if self.s[self.i] == b'"' || self.s[self.i] == b'\'' {
            let quote = self.s[self.i];
            self.i += 1;
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != quote {
                self.i += 1;
            }
            if self.eof() {
                return Err(ParseError::UnterminatedString);
            }
            let v = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
            self.i += 1; // closing quote
            return Ok(Operand::Str(v));
        }

        let start = self.i;
        while self.i < self.s.len() && !self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
        if self.i == start {
            return Err(ParseError::ExpectedValue(self.rest_snippet()));
        }
        let raw = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        match raw.parse::<f64>() {
            Ok(n) => Ok(Operand::Num(n)),
            Err(_) => Ok(Operand::Str(raw)),
        }
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

// ---------------------------------------------------------------------------
// Flat JSON scanner (allocation-free)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum JsonVal<'a> {
    Str(&'a [u8]),
    Num(f64),
    Bool(bool),
    Null,
}

/// Find a top-level field in a JSON object without allocating.
///
/// Nested objects and arrays are skipped wholesale; string contents are returned
/// raw (escape sequences are not decoded), which is sufficient for the small,
/// flat documents the fixed filter slot is sized for.
fn lookup<'a>(json: &'a [u8], field: &[u8]) -> Option<JsonVal<'a>> {
    let mut i = 0;
    skip_ws(json, &mut i);
    if i >= json.len() || json[i] != b'{' {
        return None;
    }
    i += 1;

    loop {
        skip_ws(json, &mut i);
        if i >= json.len() {
            return None;
        }
        if json[i] == b'}' {
            return None;
        }
        if json[i] != b'"' {
            return None; // malformed
        }
        let key = scan_string(json, &mut i)?;

        skip_ws(json, &mut i);
        if i >= json.len() || json[i] != b':' {
            return None;
        }
        i += 1;
        skip_ws(json, &mut i);

        if key == field {
            return scan_value(json, &mut i);
        }
        skip_value(json, &mut i)?;

        skip_ws(json, &mut i);
        if i >= json.len() {
            return None;
        }
        // A closing brace means the field is absent; anything else is malformed.
        // Either way the lookup is over.
        if json[i] != b',' {
            return None;
        }
        i += 1;
    }
}

fn skip_ws(s: &[u8], i: &mut usize) {
    while *i < s.len() && s[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

/// Consume a quoted string starting at `s[*i] == '"'`, returning its raw body.
fn scan_string<'a>(s: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    if *i >= s.len() || s[*i] != b'"' {
        return None;
    }
    *i += 1;
    let start = *i;
    while *i < s.len() {
        match s[*i] {
            b'\\' => *i += 2, // skip the escape pair so \" does not end the string
            b'"' => {
                let body = &s[start..*i];
                *i += 1;
                return Some(body);
            }
            _ => *i += 1,
        }
    }
    None
}

fn scan_value<'a>(s: &'a [u8], i: &mut usize) -> Option<JsonVal<'a>> {
    if *i >= s.len() {
        return None;
    }
    match s[*i] {
        b'"' => scan_string(s, i).map(JsonVal::Str),
        b't' if s[*i..].starts_with(b"true") => {
            *i += 4;
            Some(JsonVal::Bool(true))
        }
        b'f' if s[*i..].starts_with(b"false") => {
            *i += 5;
            Some(JsonVal::Bool(false))
        }
        b'n' if s[*i..].starts_with(b"null") => {
            *i += 4;
            Some(JsonVal::Null)
        }
        b'{' | b'[' => None, // nested values are not addressable in the base grammar
        _ => {
            let start = *i;
            while *i < s.len()
                && !matches!(s[*i], b',' | b'}' | b']')
                && !s[*i].is_ascii_whitespace()
            {
                *i += 1;
            }
            std::str::from_utf8(&s[start..*i])
                .ok()?
                .parse::<f64>()
                .ok()
                .map(JsonVal::Num)
        }
    }
}

fn skip_value(s: &[u8], i: &mut usize) -> Option<()> {
    if *i >= s.len() {
        return None;
    }
    match s[*i] {
        b'"' => {
            scan_string(s, i)?;
            Some(())
        }
        b'{' | b'[' => {
            let mut depth = 0i32;
            while *i < s.len() {
                match s[*i] {
                    b'"' => {
                        scan_string(s, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            *i += 1;
                            return Some(());
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
            None
        }
        _ => {
            while *i < s.len() && !matches!(s[*i], b',' | b'}' | b']') {
                *i += 1;
            }
            Some(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &[u8] =
        br#"{"cat":"tech","lang":"ko","ts":1723248000,"score":-1.5,"ok":true,"nil":null}"#;

    fn m(expr: &str) -> bool {
        Filter::parse(expr).unwrap().matches(DOC)
    }

    #[test]
    fn lookup_reads_each_value_kind() {
        assert_eq!(lookup(DOC, b"cat"), Some(JsonVal::Str(b"tech")));
        assert_eq!(lookup(DOC, b"ts"), Some(JsonVal::Num(1723248000.0)));
        assert_eq!(lookup(DOC, b"score"), Some(JsonVal::Num(-1.5)));
        assert_eq!(lookup(DOC, b"ok"), Some(JsonVal::Bool(true)));
        assert_eq!(lookup(DOC, b"nil"), Some(JsonVal::Null));
        assert_eq!(lookup(DOC, b"missing"), None);
    }

    #[test]
    fn lookup_does_not_match_a_key_prefix() {
        // "ca" must not match the key "cat".
        assert_eq!(lookup(DOC, b"ca"), None);
        assert_eq!(lookup(DOC, b"catx"), None);
    }

    #[test]
    fn lookup_skips_nested_values() {
        let doc = br#"{"a":{"b":1,"c":[1,2,{"d":3}]},"z":9}"#;
        assert_eq!(lookup(doc, b"z"), Some(JsonVal::Num(9.0)));
        // Nested containers are not addressable, but must not derail the scan.
        assert_eq!(lookup(doc, b"a"), None);
    }

    #[test]
    fn lookup_handles_escaped_quotes_in_values() {
        let doc = br#"{"a":"x\"y","z":1}"#;
        assert_eq!(lookup(doc, b"z"), Some(JsonVal::Num(1.0)));
        assert_eq!(lookup(doc, b"a"), Some(JsonVal::Str(br#"x\"y"#)));
    }

    #[test]
    fn lookup_tolerates_whitespace() {
        let doc = br#" { "a" : 1 , "b" : "x" } "#;
        assert_eq!(lookup(doc, b"a"), Some(JsonVal::Num(1.0)));
        assert_eq!(lookup(doc, b"b"), Some(JsonVal::Str(b"x")));
    }

    #[test]
    fn string_equality() {
        assert!(m("cat = tech"));
        assert!(m(r#"cat = "tech""#));
        assert!(!m("cat = sports"));
        assert!(m("cat != sports"));
    }

    #[test]
    fn numeric_comparisons() {
        assert!(m("ts > 1700000000"));
        assert!(m("ts >= 1723248000"));
        assert!(!m("ts > 1723248000"));
        assert!(m("score < 0"));
        assert!(m("score <= -1.5"));
    }

    #[test]
    fn and_or_precedence() {
        // AND binds tighter than OR.
        assert!(m("cat = sports AND lang = ko OR ts > 1"));
        assert!(!m("cat = sports AND lang = ko OR ts > 9999999999"));
        assert!(m("cat = tech AND lang = ko"));
        assert!(!m("cat = tech AND lang = en"));
    }

    #[test]
    fn keywords_are_case_insensitive_but_not_prefixes() {
        assert!(m("cat = tech and lang = ko"));
        assert!(m("cat = sports or lang = ko"));
        // A field named ANDROID must not be read as the AND keyword.
        let f = Filter::parse("ANDROID = 1").unwrap();
        assert!(f.matches(br#"{"ANDROID":1}"#));
    }

    #[test]
    fn missing_field_is_always_false_even_for_ne() {
        // Otherwise `nope != x` would silently pass for every document.
        assert!(!m("nope = x"));
        assert!(!m("nope != x"));
        assert!(!m("nope > 0"));
        assert!(!m("nope = x OR cat = sports"));
        assert!(m("nope = x OR cat = tech"));
    }

    #[test]
    fn bool_and_null_support_only_equality() {
        assert!(m("ok = true"));
        assert!(!m("ok = false"));
        assert!(m("ok != false"));
        assert!(m("nil = null"));
        assert!(!m("nil != null"));
        assert!(!m("ok > true"));
    }

    #[test]
    fn type_mismatch_is_unequal_rather_than_an_error() {
        assert!(!m("cat = 5"));
        assert!(m("cat != 5"));
        assert!(!m("ts = tech"));
    }

    #[test]
    fn parse_rejects_malformed_input() {
        assert!(matches!(Filter::parse(""), Err(ParseError::Empty)));
        assert!(matches!(Filter::parse("   "), Err(ParseError::Empty)));
        assert!(matches!(
            Filter::parse("cat"),
            Err(ParseError::ExpectedOperator(_))
        ));
        assert!(matches!(
            Filter::parse("= tech"),
            Err(ParseError::ExpectedField)
        ));
        assert!(matches!(
            Filter::parse("cat = "),
            Err(ParseError::ExpectedValue(_))
        ));
        assert!(matches!(
            Filter::parse(r#"cat = "x"#),
            Err(ParseError::UnterminatedString)
        ));
        assert!(matches!(
            Filter::parse("cat = a b"),
            Err(ParseError::TrailingInput(_))
        ));
    }

    #[test]
    fn empty_json_matches_nothing() {
        let f = Filter::parse("cat = tech").unwrap();
        assert!(!f.matches(b""));
        assert!(!f.matches(b"{}"));
    }
}
