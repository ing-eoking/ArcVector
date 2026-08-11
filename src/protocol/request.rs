//! Turning a command line into a typed request.
//!
//! Nothing here touches the engine or an index. Parsing lives entirely on this
//! side of the boundary so that [`crate::command`] handlers receive plain data
//! and can be exercised without a server, and so that every syntax rule has one
//! place to live.
//!
//! Two commands carry a body, and their lines are parsed into a [`Body`] that
//! [`crate::pending`] holds until the bytes arrive. The rest resolve to a
//! [`Line`] and run immediately.

use super::tokens::Tokens;
use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::search::Metric;
use crate::storage::element;
use crate::storage::quantize::Quant;

/// Upper bound on one transferred body, so a malformed length cannot ask for an
/// enormous allocation.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The commands this extension answers. Names match case-insensitively, so
/// `VSIM` and `vsim` are the same command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    VCreate,
    VAdd,
    VSim,
    VGet,
    VDel,
    VDrop,
    VList,
}

impl Cmd {
    pub fn parse(name: &str) -> Option<Cmd> {
        const NAMES: [(&str, Cmd); 7] = [
            ("vcreate", Cmd::VCreate),
            ("vadd", Cmd::VAdd),
            ("vsim", Cmd::VSim),
            ("vget", Cmd::VGet),
            ("vdel", Cmd::VDel),
            ("vdrop", Cmd::VDrop),
            ("vlist", Cmd::VList),
        ];
        NAMES
            .iter()
            .find(|(text, _)| name.eq_ignore_ascii_case(text))
            .map(|(_, cmd)| *cmd)
    }
}

/// Where a `VSIM` gets its query coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimSource {
    /// Coordinates arrive in the body.
    Vector,
    /// The query is a vector already stored under a key.
    Key,
}

impl SimSource {
    pub fn parse(name: &str) -> Option<SimSource> {
        if name.eq_ignore_ascii_case("VECTOR") {
            Some(SimSource::Vector)
        } else if name.eq_ignore_ascii_case("KEY") {
            Some(SimSource::Key)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Index geometry and tuning from a `vcreate` line.
#[derive(Clone, Copy, Debug)]
pub struct Create<'a> {
    pub index: &'a str,
    pub dim: usize,
    pub metric: Metric,
    pub quant: Quant,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,
    pub maxcount: Option<u32>,
    pub exptime: Option<u32>,
}

impl Create<'_> {
    /// Defaults for everything a `vcreate` line may omit.
    fn with_defaults(index: &str, dim: usize) -> Create<'_> {
        Create {
            index,
            dim,
            metric: Metric::Cos,
            quant: Quant::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
            maxcount: None,
            exptime: None,
        }
    }
}

/// A `VSIM KEY` query.
#[derive(Debug)]
pub struct SimKey<'a> {
    pub index: &'a str,
    pub key: &'a str,
    pub k: usize,
    pub filter: Option<Filter>,
}

/// A request fully determined by its command line.
#[derive(Debug)]
pub enum Line<'a> {
    Create(Create<'a>),
    SimKey(SimKey<'a>),
    Get { index: &'a str, id: &'a str },
    Del { index: &'a str, id: &'a str },
    Drop { index: &'a str },
    List,
}

/// A `vadd` line, awaiting its coordinates.
#[derive(Debug)]
pub struct Add {
    pub index: String,
    pub id: String,
    /// Dimension the client declared, checked against the parsed coordinate
    /// count and against the index.
    pub dim: usize,
    pub attr: Vec<u8>,
}

/// A `VSIM VECTOR` line, awaiting its coordinates.
#[derive(Debug)]
pub struct Sim {
    pub index: String,
    pub k: usize,
    pub dim: usize,
    pub filter: Option<Filter>,
}

/// A request whose body has still to arrive.
#[derive(Debug)]
pub enum Body {
    Add(Add),
    Sim(Sim),
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn malformed() -> Error {
    Error::bad_request("bad command line format")
}

/// Parse a line that needs no body.
pub fn parse_line<'a>(tokens: &Tokens<'a>) -> Result<Line<'a>> {
    match tokens.command() {
        Some(Cmd::VCreate) => parse_create(tokens).map(Line::Create),
        Some(Cmd::VSim) => parse_sim_key(tokens).map(Line::SimKey),
        Some(Cmd::VGet) => {
            let (index, id) = two_names(tokens)?;
            Ok(Line::Get { index, id })
        }
        Some(Cmd::VDel) => {
            let (index, id) = two_names(tokens)?;
            Ok(Line::Del { index, id })
        }
        Some(Cmd::VDrop) => {
            if tokens.len() != 2 {
                return Err(malformed());
            }
            Ok(Line::Drop {
                index: tokens.text(1)?,
            })
        }
        Some(Cmd::VList) => {
            if tokens.len() != 1 {
                return Err(malformed());
            }
            Ok(Line::List)
        }
        // These carry a body and are parsed by `parse_body`.
        Some(Cmd::VAdd) => Err(malformed()),
        None => Err(Error::bad_request(format!(
            "unknown command {}",
            tokens.text(0).unwrap_or("")
        ))),
    }
}

fn two_names<'a>(tokens: &Tokens<'a>) -> Result<(&'a str, &'a str)> {
    if tokens.len() != 3 {
        return Err(malformed());
    }
    Ok((tokens.text(1)?, tokens.text(2)?))
}

/// `vcreate <index> <dim> [METRIC m] [QUANT q] [M n] [EFC n] [EFS n]
///          [MAXCOUNT n] [EXPTIME n]`
fn parse_create<'a>(tokens: &Tokens<'a>) -> Result<Create<'a>> {
    if tokens.len() < 3 {
        return Err(malformed());
    }
    let dim: usize = tokens.parse(2, "dimension")?;
    if dim == 0 || dim > u16::MAX as usize {
        return Err(Error::bad_request("dimension out of range (1..65535)"));
    }
    let mut create = Create::with_defaults(tokens.text(1)?, dim);

    for (key, raw) in tokens.options(3)? {
        let number = |what: &str| -> Result<usize> {
            raw.parse::<usize>()
                .map_err(|_| Error::bad_request(format!("invalid {what} '{raw}'")))
        };
        match key.as_str() {
            "METRIC" => {
                create.metric = Metric::parse(raw)
                    .ok_or_else(|| Error::bad_request(format!("unknown metric '{raw}'")))?;
            }
            "QUANT" => {
                create.quant = Quant::parse(raw)
                    .ok_or_else(|| Error::bad_request(format!("unknown quantization '{raw}'")))?;
            }
            "M" => create.connectivity = number("M")?,
            "EFC" => create.expansion_add = number("EFC")?,
            "EFS" => create.expansion_search = number("EFS")?,
            // item_attr.maxcount is an i32, so a larger value would wrap to a
            // negative limit that rejects every insert.
            "MAXCOUNT" => {
                let n = number("MAXCOUNT")?;
                if n == 0 || n > i32::MAX as usize {
                    return Err(Error::bad_request(
                        "MAXCOUNT must be between 1 and 2147483647",
                    ));
                }
                create.maxcount = Some(n as u32);
            }
            "EXPTIME" => {
                let n = number("EXPTIME")?;
                if n > u32::MAX as usize {
                    return Err(Error::bad_request("EXPTIME is out of range"));
                }
                create.exptime = Some(n as u32);
            }
            other => return Err(Error::bad_request(format!("unknown option {other}"))),
        }
    }

    create.metric.check_quant(create.quant)?;
    Ok(create)
}

/// `VSIM KEY <index> <num> <key> [FILTER <n> <term>...]`
fn parse_sim_key<'a>(tokens: &Tokens<'a>) -> Result<SimKey<'a>> {
    if tokens.len() < 5 {
        return Err(malformed());
    }
    let source = tokens.text(1)?;
    match SimSource::parse(source) {
        Some(SimSource::Key) => {}
        // VECTOR reaches this path only when its body phase was refused.
        Some(SimSource::Vector) => return Err(malformed()),
        None => {
            return Err(Error::bad_request(format!(
                "expected VECTOR or KEY, got '{source}'"
            )));
        }
    }
    Ok(SimKey {
        index: tokens.text(2)?,
        k: result_count(tokens, 3)?,
        key: tokens.text(4)?,
        filter: filter_clause(tokens, 5)?,
    })
}

fn result_count(tokens: &Tokens, at: usize) -> Result<usize> {
    let k: usize = tokens.parse(at, "result count")?;
    if k == 0 {
        return Err(Error::bad_request("result count must be at least 1"));
    }
    Ok(k)
}

/// Which token holds the body length, for the two commands that have a body.
///
/// `VSIM KEY` is excluded: its query is already in the index.
pub fn body_length_at(tokens: &Tokens) -> Option<usize> {
    match tokens.command()? {
        Cmd::VAdd => Some(3),
        Cmd::VSim if tokens.text(1).ok().and_then(SimSource::parse)? == SimSource::Vector => {
            Some(4)
        }
        _ => None,
    }
}

/// Parse a line that carries a body.
///
/// Only the length matters to the caller registering the body; a failure here is
/// still worth carrying, because the body must be drained before the client can
/// be told about it.
pub fn parse_body(tokens: &Tokens) -> Result<Body> {
    match tokens.command() {
        Some(Cmd::VAdd) => parse_add(tokens).map(Body::Add),
        Some(Cmd::VSim) => parse_sim(tokens).map(Body::Sim),
        _ => Err(malformed()),
    }
}

/// `vadd <index> <id> <veclen> <dim> [ATTR <attrlen> <attr JSON>]`
fn parse_add(tokens: &Tokens) -> Result<Add> {
    if tokens.len() < 5 {
        return Err(malformed());
    }
    Ok(Add {
        index: tokens.text(1)?.to_owned(),
        id: tokens.text(2)?.to_owned(),
        dim: tokens.parse(4, "dimension")?,
        attr: attr_clause(tokens, 5)?,
    })
}

/// `VSIM VECTOR <index> <num> <bytes> <dim> [FILTER <n> <term>...]`
fn parse_sim(tokens: &Tokens) -> Result<Sim> {
    if tokens.len() < 6 {
        return Err(malformed());
    }
    Ok(Sim {
        index: tokens.text(2)?.to_owned(),
        k: result_count(tokens, 3)?,
        dim: tokens.parse(5, "dimension")?,
        filter: filter_clause(tokens, 6)?,
    })
}

/// Why a body-carrying line never reached its body phase.
///
/// Registering the body is the only way to keep the stream in sync, and that
/// needs a length. Without one there is nothing to do but report it.
pub fn body_length_error(tokens: &Tokens, at: usize, what: &str) -> Error {
    match tokens.parse::<usize>(at, what) {
        Err(e) => e,
        Ok(n) if n > MAX_BODY_BYTES => Error::bad_request(format!(
            "{what} {n} exceeds the {MAX_BODY_BYTES}-byte transfer limit"
        )),
        Ok(_) => Error::bad_request("lost command state"),
    }
}

// ---------------------------------------------------------------------------
// Optional clauses
// ---------------------------------------------------------------------------

/// `ATTR <attrlen> <attr JSON>`, starting at token `at`. Optional.
fn attr_clause(tokens: &Tokens, at: usize) -> Result<Vec<u8>> {
    if tokens.len() <= at {
        return Ok(Vec::new());
    }
    let keyword = tokens.text(at)?;
    if !keyword.eq_ignore_ascii_case("ATTR") {
        return Err(Error::bad_request(format!(
            "expected ATTR after the dimension, got '{keyword}'"
        )));
    }
    let declared: usize = tokens.parse(at + 1, "ATTR length")?;
    if declared > element::ATTR_BYTES {
        return Err(Error::bad_request(format!(
            "ATTR is {declared} bytes, over the {}-byte limit",
            element::ATTR_BYTES
        )));
    }
    if declared == 0 {
        return Ok(Vec::new());
    }
    // The declared length drives the read; `tail` checks it against the span the
    // tokens cover, so a wrong length is reported rather than trusted.
    tokens.tail(at + 2, declared)
}

/// `FILTER <n> <term>...`, starting at token `at`. Optional.
///
/// Each term is one token, which is what keeps the clause immune to the
/// tokenizer — a free-form expression on the command line would not be. Terms
/// are ANDed and handed to the same parser a full expression would use, so a
/// term must not contain spaces.
fn filter_clause(tokens: &Tokens, at: usize) -> Result<Option<Filter>> {
    if tokens.len() <= at {
        return Ok(None);
    }
    let keyword = tokens.text(at)?;
    if !keyword.eq_ignore_ascii_case("FILTER") {
        return Err(Error::bad_request(format!(
            "expected FILTER, got '{keyword}'"
        )));
    }
    let count: usize = tokens.parse(at + 1, "filter term count")?;
    if count == 0 {
        return Ok(None);
    }

    let first = at + 2;
    let supplied = tokens.len().saturating_sub(first);
    if supplied != count {
        return Err(Error::bad_request(format!(
            "FILTER declares {count} terms but {supplied} were supplied"
        )));
    }

    let mut expression = String::new();
    for i in first..first + count {
        if i > first {
            expression.push_str(" AND ");
        }
        expression.push_str(tokens.text(i)?);
    }
    Filter::parse(&expression).map(Some).map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::tokens::tokenize_for_test as tokenize;
    use std::os::raw::c_int;

    /// Binds `$name` to a `Tokens` view of `$src`, keeping the backing buffer
    /// alive for the rest of the scope so borrowed results can escape the parse.
    macro_rules! tokens {
        ($name:ident = $src:expr) => {
            let (_buf, _raw) = tokenize($src);
            // SAFETY: `_buf` and `_raw` outlive `$name` within this scope.
            let $name = unsafe { Tokens::new(_raw.as_ptr(), _raw.len() as c_int) };
        };
    }

    fn on_line<T>(line: &str, f: impl FnOnce(&Tokens) -> T) -> T {
        tokens!(t = line);
        f(&t)
    }

    /// The message a line is refused with, routed the way the callbacks route it:
    /// a line that carries a body goes to `parse_body`, everything else to
    /// `parse_line`.
    fn err(line: &str) -> String {
        tokens!(t = line);
        let outcome = if body_length_at(&t).is_some() {
            parse_body(&t).map(|_| ())
        } else {
            parse_line(&t).map(|_| ())
        };
        match outcome {
            Err(e) => e.to_string(),
            Ok(()) => "accepted".to_owned(),
        }
    }

    // -- command names -------------------------------------------------------

    #[test]
    fn command_names_are_case_insensitive() {
        for (text, cmd) in [
            ("vsim", Cmd::VSim),
            ("VSIM", Cmd::VSim),
            ("VSim", Cmd::VSim),
            ("vcreate", Cmd::VCreate),
            ("VADD", Cmd::VAdd),
        ] {
            assert_eq!(Cmd::parse(text), Some(cmd), "{text}");
        }
        assert_eq!(Cmd::parse("vsimilar"), None);
        assert_eq!(Cmd::parse(""), None);
    }

    #[test]
    fn sim_sources_are_case_insensitive() {
        assert_eq!(SimSource::parse("VECTOR"), Some(SimSource::Vector));
        assert_eq!(SimSource::parse("vector"), Some(SimSource::Vector));
        assert_eq!(SimSource::parse("KEY"), Some(SimSource::Key));
        assert_eq!(SimSource::parse("ID"), None);
    }

    // -- which lines carry a body -------------------------------------------

    #[test]
    fn only_vadd_and_vsim_vector_carry_a_body() {
        assert_eq!(on_line("vadd i d 28 7", body_length_at), Some(3));
        assert_eq!(
            on_line("VSIM VECTOR docs 10 4096 1024", body_length_at),
            Some(4)
        );
        // VSIM KEY's query is already stored.
        assert_eq!(on_line("VSIM KEY docs 10 v1", body_length_at), None);
        assert_eq!(on_line("vlist", body_length_at), None);
        assert_eq!(on_line("vget docs v1", body_length_at), None);
        assert_eq!(on_line("nonsense", body_length_at), None);
    }

    // -- vcreate ------------------------------------------------------------

    #[test]
    fn vcreate_defaults_are_filled_in() {
        tokens!(t = "vcreate docs 1024");
        let c = parse_create(&t).unwrap();
        assert_eq!(c.index, "docs");
        assert_eq!(c.dim, 1024);
        assert_eq!(c.metric, Metric::Cos);
        assert_eq!(c.quant, Quant::F32);
        assert!(c.maxcount.is_none());
        assert!(c.exptime.is_none());
    }

    #[test]
    fn vcreate_options_are_case_insensitive() {
        tokens!(t = "vcreate docs 8 quant i8 metric l2 efs 64");
        let c = parse_create(&t).unwrap();
        assert_eq!(c.quant, Quant::I8);
        assert_eq!(c.metric, Metric::L2);
        assert_eq!(c.expansion_search, 64);
    }

    #[test]
    fn vcreate_rejects_unknown_options_and_values() {
        assert!(err("vcreate docs 8 NOPE 1").contains("unknown option"));
        assert!(err("vcreate docs 8 QUANT f64").contains("unknown quantization"));
        assert!(err("vcreate docs 8 METRIC manhattan").contains("unknown metric"));
        // Removed knobs must not silently succeed.
        assert!(err("vcreate docs 8 FBYTES 128").contains("unknown option"));
        assert!(err("vcreate docs 8 THREADS 8").contains("unknown option"));
    }

    #[test]
    fn vcreate_rejects_an_out_of_range_dimension() {
        assert!(err("vcreate docs 0").contains("out of range"));
        assert!(err("vcreate docs 65536").contains("out of range"));
        assert!(err("vcreate docs abc").contains("dimension"));
    }

    #[test]
    fn maxcount_is_bounded_by_the_engines_i32_field() {
        tokens!(t = "vcreate docs 8 MAXCOUNT 2147483647");
        let c = parse_create(&t).unwrap();
        assert_eq!(c.maxcount, Some(2_147_483_647));
        assert!(err("vcreate docs 8 MAXCOUNT 2147483648").contains("MAXCOUNT"));
        assert!(err("vcreate docs 8 MAXCOUNT 0").contains("MAXCOUNT"));
    }

    #[test]
    fn incompatible_metric_and_quantization_are_rejected_at_parse_time() {
        // b1 vectors carry no magnitude, so cosine is meaningless on them.
        assert!(err("vcreate docs 8 QUANT b1 METRIC cos").contains("b1"));
        assert!(err("vcreate docs 8 QUANT f32 METRIC hamming").contains("b1"));
        tokens!(t = "vcreate docs 8 QUANT b1 METRIC hamming");
        assert!(parse_create(&t).is_ok());
    }

    // -- vadd ---------------------------------------------------------------

    #[test]
    fn vadd_yields_index_id_dimension_and_attr() {
        let Body::Add(a) = on_line(
            r#"vadd index doc1 7 2 ATTR 23 {"abc":123,"def":"abc"}"#,
            parse_body,
        )
        .unwrap() else {
            panic!("expected an Add")
        };
        assert_eq!(a.index, "index");
        assert_eq!(a.id, "doc1");
        assert_eq!(a.dim, 2);
        assert_eq!(a.attr, br#"{"abc":123,"def":"abc"}"#);
    }

    #[test]
    fn vadd_attr_is_optional() {
        let Body::Add(a) = on_line("vadd index doc1 7 2", parse_body).unwrap() else {
            panic!("expected an Add")
        };
        assert_eq!(a.dim, 2);
        assert!(a.attr.is_empty());
    }

    #[test]
    fn vadd_needs_a_dimension() {
        // Without one there is nothing to check the coordinate count against.
        assert!(err("vadd index doc1 28").contains("bad command line format"));
        assert!(err("vadd index doc1 28 abc").contains("dimension"));
    }

    // -- ATTR clause --------------------------------------------------------

    fn attr_of(line: &str) -> Result<Vec<u8>> {
        on_line(line, |t| attr_clause(t, 5))
    }

    #[test]
    fn attr_with_spaces_is_reassembled() {
        let json = r#"{"cat": "tech", "ts": 1723248000}"#;
        assert_eq!(
            attr_of(&format!("vadd docs v1 16 4 ATTR {} {json}", json.len())).unwrap(),
            json.as_bytes()
        );
    }

    #[test]
    fn conventionally_spaced_json_fits_the_token_budget() {
        // json.dumps / jq style: one space after each comma.
        let json = format!(
            "{{{}}}",
            (0..14)
                .map(|i| format!("\"k{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(json.len() <= element::ATTR_BYTES);
        assert_eq!(
            attr_of(&format!("vadd docs v1 16 4 ATTR {} {json}", json.len())).unwrap(),
            json.as_bytes()
        );
    }

    #[test]
    fn json_padded_with_whitespace_everywhere_exhausts_the_token_budget() {
        // Only 67 bytes, but already past MAX_TOKENS, after which the remaining
        // length is not recoverable from the array we are handed.
        let body = (0..6)
            .map(|i| format!("\"k{i}\" : {i}"))
            .collect::<Vec<_>>()
            .join(" , ");
        let json = format!("{{ {body} }}");
        assert!(json.len() < element::ATTR_BYTES, "{} bytes", json.len());
        let msg = attr_of(&format!("vadd docs v1 16 4 ATTR {} {json}", json.len()))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("less whitespace"), "{msg}");
    }

    #[test]
    fn attr_keyword_and_length_are_validated() {
        assert!(attr_of("vadd docs v1 16 4 attr 2 {}").is_ok());
        assert!(
            attr_of("vadd docs v1 16 4 ATTRS 2 {}")
                .unwrap_err()
                .to_string()
                .contains("expected ATTR")
        );
        assert!(
            attr_of("vadd docs v1 16 4 ATTR 129 {}")
                .unwrap_err()
                .to_string()
                .contains("over the 128-byte limit")
        );
        assert!(
            attr_of("vadd docs v1 16 4 ATTR 99 {}")
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert!(
            attr_of("vadd docs v1 16 4 ATTR abc {}")
                .unwrap_err()
                .to_string()
                .contains("ATTR length")
        );
        assert!(attr_of("vadd docs v1 16 4 ATTR 0").unwrap().is_empty());
    }

    #[test]
    fn an_attr_exactly_at_the_limit_is_accepted() {
        // Built from real content: leading spaces are separators to the
        // tokenizer, not part of the value.
        let json = format!(r#"{{"k":"{}"}}"#, "x".repeat(120));
        assert_eq!(json.len(), element::ATTR_BYTES);
        assert!(attr_of(&format!("vadd docs v1 16 4 ATTR 128 {json}")).is_ok());
    }

    // -- VSIM ---------------------------------------------------------------

    #[test]
    fn vsim_vector_yields_index_k_dimension_and_filter() {
        let Body::Sim(s) = on_line(
            "VSIM VECTOR docs 10 8192 1024 FILTER 1 cat=tech",
            parse_body,
        )
        .unwrap() else {
            panic!("expected a Sim")
        };
        assert_eq!(s.index, "docs");
        assert_eq!(s.k, 10);
        assert_eq!(s.dim, 1024);
        assert!(s.filter.is_some());
    }

    #[test]
    fn vsim_key_yields_index_k_and_key() {
        tokens!(t = "VSIM KEY docs 5 v1");
        let Line::SimKey(s) = parse_line(&t).unwrap() else {
            panic!("expected a SimKey")
        };
        assert_eq!(s.index, "docs");
        assert_eq!(s.k, 5);
        assert_eq!(s.key, "v1");
        assert!(s.filter.is_none());
    }

    #[test]
    fn vsim_rejects_an_unknown_source_and_a_zero_count() {
        assert!(err("VSIM ID docs 5 v1").contains("expected VECTOR or KEY"));
        assert!(err("VSIM KEY docs 0 v1").contains("result count"));
        assert!(err("VSIM VECTOR docs 0 4096 1024").contains("result count"));
    }

    // -- FILTER clause ------------------------------------------------------

    fn filter_of(line: &str) -> Result<Option<Filter>> {
        on_line(line, |t| filter_clause(t, 5))
    }

    #[test]
    fn filter_terms_are_anded_together() {
        let f = filter_of("VSIM KEY docs 5 v1 FILTER 2 cat=tech ts>1700000000")
            .unwrap()
            .unwrap();
        assert!(f.matches(br#"{"cat":"tech","ts":1723248000}"#));
        // Both terms must hold.
        assert!(!f.matches(br#"{"cat":"tech","ts":1}"#));
        assert!(!f.matches(br#"{"cat":"news","ts":1723248000}"#));
    }

    #[test]
    fn an_absent_or_empty_filter_is_none() {
        assert!(filter_of("VSIM KEY docs 5 v1").unwrap().is_none());
        assert!(filter_of("VSIM KEY docs 5 v1 FILTER 0").unwrap().is_none());
    }

    #[test]
    fn a_filter_count_that_disagrees_with_the_terms_is_rejected() {
        // Guards against a term being dropped or swept in from elsewhere.
        assert!(
            filter_of("VSIM KEY docs 5 v1 FILTER 3 cat=tech")
                .unwrap_err()
                .to_string()
                .contains("declares 3 terms but 1")
        );
        assert!(
            filter_of("VSIM KEY docs 5 v1 FILTER 1 cat=tech lang=ko")
                .unwrap_err()
                .to_string()
                .contains("declares 1 terms but 2")
        );
    }

    #[test]
    fn a_misspelled_or_unparseable_filter_is_reported() {
        assert!(
            filter_of("VSIM KEY docs 5 v1 FILTERS 1 cat=tech")
                .unwrap_err()
                .to_string()
                .contains("expected FILTER")
        );
        assert!(filter_of("VSIM KEY docs 5 v1 FILTER 1 nonsense").is_err());
    }

    // -- line-only commands -------------------------------------------------

    #[test]
    fn simple_lines_parse_and_reject_wrong_arity() {
        tokens!(get = "vget docs v1");
        assert!(matches!(parse_line(&get), Ok(Line::Get { .. })));
        tokens!(del = "vdel docs v1");
        assert!(matches!(parse_line(&del), Ok(Line::Del { .. })));
        tokens!(drop_ = "vdrop docs");
        assert!(matches!(parse_line(&drop_), Ok(Line::Drop { .. })));
        tokens!(list = "vlist");
        assert!(matches!(parse_line(&list), Ok(Line::List)));

        assert!(err("vget docs").contains("bad command line format"));
        assert!(err("vdel docs v1 extra").contains("bad command line format"));
        assert!(err("vdrop").contains("bad command line format"));
        assert!(err("vlist extra").contains("bad command line format"));
        assert!(err("nonsense").contains("unknown command"));
    }

    #[test]
    fn an_unusable_body_length_is_reported_not_guessed() {
        let msg = on_line("vadd i d abc 7", |t| {
            body_length_error(t, 3, "vector length")
        })
        .to_string();
        assert!(msg.contains("vector length"), "{msg}");

        let msg = on_line("vadd i d 99999999999 7", |t| {
            body_length_error(t, 3, "vector length")
        })
        .to_string();
        assert!(msg.contains("transfer limit"), "{msg}");
    }
}
