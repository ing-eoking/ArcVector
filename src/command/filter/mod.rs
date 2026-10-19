use crate::bson::{Value, lookup};

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
            Self::Empty => write!(f, "empty filter expression"),
            Self::ExpectedField => write!(f, "expected a field name"),
            Self::ExpectedOperator(s) => write!(f, "expected an operator near '{s}'"),
            Self::ExpectedValue(s) => write!(f, "expected a value near '{s}'"),
            Self::UnterminatedString => write!(f, "unterminated quoted string"),
            Self::TrailingInput(s) => write!(f, "unexpected trailing input '{s}'"),
        }
    }
}

impl Filter {
    pub fn parse(src: &str) -> Result<Self, ParseError> {
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
        Ok(Self { terms })
    }

    pub fn matches(&self, doc: &[u8]) -> bool {
        self.terms
            .iter()
            .any(|term| term.iter().all(|c| eval_cond(c, doc)))
    }
}

fn eval_cond(c: &Cond, doc: &[u8]) -> bool {
    let Some(found) = lookup(doc, c.field.as_bytes()) else {
        return false;
    };
    match (&found, &c.value) {
        (Value::Num(a), Operand::Num(b)) => cmp_ord(a.partial_cmp(b), c.op),
        (Value::Str(a), Operand::Str(b)) => cmp_ord(Some((*a).cmp(b.as_bytes())), c.op),
        (Value::Bool(a), Operand::Str(b)) => match c.op {
            Op::Eq => bool_str_eq(*a, b),
            Op::Ne => !bool_str_eq(*a, b),
            _ => false,
        },
        (Value::Null, Operand::Str(b)) => match c.op {
            Op::Eq => b == "null",
            Op::Ne => b != "null",
            _ => false,
        },
        // Other(중첩 문서/배열)도 타입 불일치로 본다.
        _ => matches!(c.op, Op::Ne),
    }
}

fn bool_str_eq(a: bool, b: &str) -> bool {
    (a && b == "true") || (!a && b == "false")
}

fn cmp_ord(ord: Option<std::cmp::Ordering>, op: Op) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};

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

    fn eat_keyword(&mut self, kw: &str) -> bool {
        let end = self.i + kw.len();
        if end > self.s.len() {
            return false;
        }
        if !self.s[self.i..end].eq_ignore_ascii_case(kw.as_bytes()) {
            return false;
        }

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
            self.i += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bson;

    const DOC: &[u8] =
        br#"{"cat":"tech","lang":"ko","ts":1723248000,"score":-1.5,"ok":true,"nil":null}"#;

    fn bs(json: &[u8]) -> Vec<u8> {
        bson::from_json(json).unwrap()
    }

    fn on(expr: &str, json: &[u8]) -> bool {
        Filter::parse(expr).unwrap().matches(&bs(json))
    }

    fn m(expr: &str) -> bool {
        on(expr, DOC)
    }

    #[test]
    fn keywords_are_case_insensitive_but_not_prefixes() {
        assert!(m("cat = tech and lang = ko"));
        assert!(m("cat = sports or lang = ko"));
        let f = Filter::parse("ANDROID = 1").unwrap();
        assert!(f.matches(&bs(br#"{"ANDROID":1}"#)));
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
        assert!(!f.matches(&bs(b"{}")));
        assert!(!f.matches(&bson::EMPTY));
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
        assert!(m("cat = sports AND lang = ko OR ts > 1"));
        assert!(!m("cat = sports AND lang = ko OR ts > 9999999999"));
        assert!(m("cat = tech AND lang = ko"));
        assert!(!m("cat = tech AND lang = en"));
    }

    #[test]
    fn missing_field_is_always_false_even_for_ne() {
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
    fn and_binds_tighter_than_or() {
        let e = "a=1 or b=2 and c=3";
        assert!(on(e, br#"{"a":1}"#));
        assert!(!on(e, br#"{"b":2}"#));
        assert!(on(e, br#"{"b":2,"c":3}"#));
    }

    #[test]
    fn keywords_are_case_insensitive() {
        for e in ["a=1 AND b=2", "a=1 and b=2"] {
            assert!(on(e, br#"{"a":1,"b":2}"#));
            assert!(!on(e, br#"{"a":1,"b":3}"#));
        }
    }

    #[test]
    fn spaces_around_the_operator_are_optional() {
        for e in ["a=1", "a = 1", "a =1"] {
            assert!(on(e, br#"{"a":1}"#));
        }
    }

    #[test]
    fn quoted_values_keep_spaces_and_operators() {
        assert!(on("title = 'a <= b'", br#"{"title":"a <= b"}"#));
    }

    #[test]
    fn a_missing_field_is_false_even_for_ne() {
        assert!(!on("x != 1", br#"{"a":1}"#));
    }

    #[test]
    fn int64_and_double_compare_as_numbers() {
        assert!(on("n > 4000000000", br#"{"n":5000000000}"#));
        assert!(on("f <= 1.5", br#"{"f":1.5}"#));
    }

    #[test]
    fn a_nested_value_is_a_type_mismatch() {
        assert!(!on("a = 1", br#"{"a":{"b":1}}"#));
        assert!(on("a != 1", br#"{"a":[1]}"#));
    }
}
