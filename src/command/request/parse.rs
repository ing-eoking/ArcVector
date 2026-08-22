use super::layout;
use super::*;

fn malformed() -> Error {
    Error::bad_request("bad command line format")
}

/// Sort one command line into a line-only request or a two-phase one.
///
/// The two-phase forms read their body length here, before anything else can fail:
/// a line whose remainder is unusable still has to say how many bytes to drain.
pub fn parse<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    match tokens.command() {
        Some(Cmd::VAdd) => two_phase(tokens, layout::add::BODY_LEN, |t| {
            parse_add(t).map(Body::Add)
        }),
        Some(Cmd::VSim) => match sim_source(tokens)? {
            SimSource::Vector => two_phase(tokens, layout::sim_vector::BODY_LEN, |t| {
                parse_sim(t).map(Body::Sim)
            }),
            SimSource::Key => parse_sim_key(tokens).map(|s| Parsed::Line(Line::SimKey(s))),
        },
        Some(Cmd::VCreate) => parse_create(tokens).map(|c| Parsed::Line(Line::Create(c))),
        Some(Cmd::VGet) => {
            let (index, id) = two_names(tokens)?;
            Ok(Parsed::Line(Line::Get { index, id }))
        }
        Some(Cmd::VDel) => {
            let (index, id) = two_names(tokens)?;
            Ok(Parsed::Line(Line::Del { index, id }))
        }
        Some(Cmd::VDrop) => {
            if tokens.len() != layout::drop::TAIL {
                return Err(malformed());
            }
            Ok(Parsed::Line(Line::Drop {
                index: tokens.text(layout::drop::INDEX)?,
            }))
        }
        Some(Cmd::VList) => no_arguments(tokens).map(|()| Parsed::Line(Line::List)),
        Some(Cmd::VStats) => no_arguments(tokens).map(|()| Parsed::Line(Line::Stats)),
        None => Err(Error::bad_request(format!(
            "unknown command {}",
            tokens.text(0).unwrap_or("")
        ))),
    }
}

/// A command whose coordinates arrive as a body: size it, then parse the rest.
fn two_phase<'a>(
    tokens: &Tokens,
    at: usize,
    parse_rest: impl FnOnce(&Tokens) -> Result<Body>,
) -> Result<Parsed<'a>> {
    let len = tokens.parse(at, "vector length")?;
    Ok(Parsed::Body {
        len,
        request: parse_rest(tokens),
    })
}

fn sim_source(tokens: &Tokens) -> Result<SimSource> {
    let raw = tokens.text(layout::SIM_SOURCE)?;
    SimSource::parse(raw)
        .ok_or_else(|| Error::bad_request(format!("expected VECTOR or KEY, got '{raw}'")))
}

fn no_arguments(tokens: &Tokens) -> Result<()> {
    if tokens.len() == 1 {
        Ok(())
    } else {
        Err(malformed())
    }
}

fn two_names<'a>(tokens: &Tokens<'a>) -> Result<(&'a str, &'a str)> {
    if tokens.len() != layout::names::TAIL {
        return Err(malformed());
    }
    Ok((
        tokens.text(layout::names::INDEX)?,
        tokens.text(layout::names::ID)?,
    ))
}

/// `vcreate <index> <dim> [METRIC m] [QUANT q] [M n] [EFC n] [EFS n]
fn parse_create<'a>(tokens: &Tokens<'a>) -> Result<Create<'a>> {
    if tokens.len() < layout::create::TAIL {
        return Err(malformed());
    }
    let dim: usize = tokens.parse(layout::create::DIM, "dimension")?;
    if dim == 0 || dim > u16::MAX as usize {
        return Err(Error::bad_request("dimension out of range (1..65535)"));
    }
    let mut create = Create::with_defaults(tokens.text(layout::create::INDEX)?, dim);

    for (key, raw) in tokens.options(layout::create::TAIL)? {
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
            // item_attr.maxcount is an i32; larger wraps to a negative limit.
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
    use layout::sim_key as at;
    if tokens.len() < at::TAIL {
        return Err(malformed());
    }
    let Trailing { filter, with_attr } = trailing(tokens, at::TAIL)?;
    Ok(SimKey {
        index: tokens.text(at::INDEX)?,
        k: result_count(tokens, at::K)?,
        key: tokens.text(at::KEY)?,
        filter,
        with_attr,
    })
}

fn result_count(tokens: &Tokens, at: usize) -> Result<usize> {
    let k: usize = tokens.parse(at, "result count")?;
    if k == 0 {
        return Err(Error::bad_request("result count must be at least 1"));
    }
    Ok(k)
}

/// `vadd <index> <id> <veclen> <dim> [ATTR <attrlen> <attr JSON>]`
fn parse_add(tokens: &Tokens) -> Result<Add> {
    use layout::add as at;
    if tokens.len() < at::TAIL {
        return Err(malformed());
    }
    Ok(Add {
        index: tokens.text(at::INDEX)?.to_owned(),
        id: tokens.text(at::ID)?.to_owned(),
        dim: tokens.parse(at::DIM, "dimension")?,
        attr: attr_clause(tokens, at::TAIL)?,
    })
}

/// `VSIM VECTOR <index> <num> <veclen> <dim> [FILTER <n> <term>...]`
fn parse_sim(tokens: &Tokens) -> Result<Sim> {
    use layout::sim_vector as at;
    if tokens.len() < at::TAIL {
        return Err(malformed());
    }
    let Trailing { filter, with_attr } = trailing(tokens, at::TAIL)?;
    Ok(Sim {
        index: tokens.text(at::INDEX)?.to_owned(),
        k: result_count(tokens, at::K)?,
        dim: tokens.parse(at::DIM, "dimension")?,
        filter,
        with_attr,
    })
}

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
    if declared > ATTR_BYTES {
        return Err(Error::bad_request(format!(
            "ATTR is {declared} bytes, over the {}-byte limit",
            ATTR_BYTES
        )));
    }
    if declared == 0 {
        return Ok(Vec::new());
    }

    // One token: a space would split the JSON, and the NUL the tokenizer leaves behind is a byte a client could have sent.
    let json = tokens.text(at + 2)?;
    if tokens.len() > at + 3 {
        return Err(Error::bad_request(
            "ATTR JSON must be a single argument with no spaces in it",
        ));
    }
    if json.len() != declared {
        return Err(Error::bad_request(format!(
            "declared ATTR length {declared} does not match the {} bytes supplied",
            json.len()
        )));
    }
    Ok(json.as_bytes().to_vec())
}

/// The optional clauses a `vsim` can end with, in any order, from token `at`.
///
/// `FILTER <n> <term>...` narrows the results, and `WITHATTR` asks for each hit's stored
/// attributes. They are read together because a `FILTER` already reads the element the
/// attributes live in — see `handler::search`.
fn trailing(tokens: &Tokens, at: usize) -> Result<Trailing> {
    let mut out = Trailing::default();
    let mut i = at;
    while i < tokens.len() {
        let word = tokens.text(i)?;
        if word.eq_ignore_ascii_case("FILTER") {
            let count: usize = tokens.parse(i + 1, "filter term count")?;
            let first = i + 2;
            let supplied = tokens.len().saturating_sub(first);
            if supplied < count {
                return Err(Error::bad_request(format!(
                    "FILTER declares {count} terms but {supplied} were supplied"
                )));
            }
            if count > 0 {
                out.filter = Some(and_terms(tokens, first, count)?);
            }
            i = first + count;
        } else if word.eq_ignore_ascii_case("WITHATTR") {
            out.with_attr = true;
            i += 1;
        } else {
            return Err(Error::bad_request(format!(
                "expected FILTER or WITHATTR, got '{word}'"
            )));
        }
    }
    Ok(out)
}

/// `count` terms from token `first`, joined as one expression.
fn and_terms(tokens: &Tokens, first: usize, count: usize) -> Result<Filter> {
    let mut expression = String::new();
    for i in first..first + count {
        if i > first {
            expression.push_str(" AND ");
        }
        expression.push_str(tokens.text(i)?);
    }
    Filter::parse(&expression).map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::tokens::tokenize_for_test as tokenize;
    use std::os::raw::c_int;

    /// Binds `$name` to a `Tokens` view of `$src`, keeping the buffer alive for the scope.
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

    fn line_of<'a>(tokens: &Tokens<'a>) -> Result<Line<'a>> {
        match parse(tokens)? {
            Parsed::Line(line) => Ok(line),
            Parsed::Body { .. } => panic!("expected a line-only command"),
        }
    }

    fn body_of(tokens: &Tokens) -> Result<Body> {
        match parse(tokens)? {
            Parsed::Body { request, .. } => request,
            Parsed::Line(_) => panic!("expected a body-carrying command"),
        }
    }

    /// The refusal message, routed as the callbacks route it: the line phase first, then the body.
    fn err(line: &str) -> String {
        tokens!(t = line);
        let outcome = match parse(&t) {
            Err(e) => Err(e),
            Ok(Parsed::Line(_)) => Ok(()),
            Ok(Parsed::Body { request, .. }) => request.map(|_| ()),
        };
        match outcome {
            Err(e) => e.to_string(),
            Ok(()) => "accepted".to_owned(),
        }
    }

    #[test]
    fn command_names_are_case_insensitive() {
        for (text, cmd) in [
            ("vsim", Cmd::VSim),
            ("VSIM", Cmd::VSim),
            ("VSim", Cmd::VSim),
            ("vcreate", Cmd::VCreate),
            ("VADD", Cmd::VAdd),
            ("VSTATS", Cmd::VStats),
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

    #[test]
    fn only_vadd_and_vsim_vector_carry_a_body() {
        fn body_len(line: &str) -> Option<usize> {
            tokens!(t = line);
            match parse(&t) {
                Ok(Parsed::Body { len, .. }) => Some(len),
                _ => None,
            }
        }
        assert_eq!(body_len("vadd i d 28 7"), Some(28));
        assert_eq!(body_len("VSIM VECTOR docs 10 4096 1024"), Some(4096));
        assert_eq!(body_len("VSIM KEY docs 10 v1"), None);
        assert_eq!(body_len("vlist"), None);
        assert_eq!(body_len("vget docs v1"), None);
        assert_eq!(body_len("nonsense"), None);
    }

    #[test]
    fn a_body_length_survives_an_unusable_remainder() {
        // The body still has to be drained, so the length outlives the rest of the line.
        tokens!(t = "vadd i d 28 abc");
        let Ok(Parsed::Body { len, request }) = parse(&t) else {
            panic!("expected a body-carrying command")
        };
        assert_eq!(len, 28);
        assert!(request.unwrap_err().to_string().contains("dimension"));
    }

    #[test]
    fn an_unusable_body_length_refuses_the_whole_line() {
        // Nothing can be drained without a length, so there is no body phase to enter.
        tokens!(t = "vadd i d abc 7");
        let msg = parse(&t).unwrap_err().to_string();
        assert!(msg.contains("vector length"), "{msg}");
    }

    #[test]
    fn a_body_line_that_reached_execute_names_the_limit_it_broke() {
        assert!(
            body_refused(MAX_BODY_BYTES + 1)
                .to_string()
                .contains("transfer limit")
        );
        assert_eq!(body_refused(28).to_string(), "lost command state");
    }

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

    #[test]
    fn vadd_yields_index_id_dimension_and_attr() {
        let Body::Add(a) = on_line(
            r#"vadd index doc1 7 2 ATTR 23 {"abc":123,"def":"abc"}"#,
            body_of,
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
        let Body::Add(a) = on_line("vadd index doc1 7 2", body_of).unwrap() else {
            panic!("expected an Add")
        };
        assert_eq!(a.dim, 2);
        assert!(a.attr.is_empty());
    }

    #[test]
    fn vadd_needs_a_dimension() {
        assert!(err("vadd index doc1 28").contains("bad command line format"));
        assert!(err("vadd index doc1 28 abc").contains("dimension"));
    }

    fn attr_of(line: &str) -> Result<Vec<u8>> {
        on_line(line, |t| attr_clause(t, layout::add::TAIL))
    }

    #[test]
    fn attr_json_must_be_one_token() {
        let json = r#"{"cat": "tech"}"#;
        let msg = attr_of(&format!("vadd docs v1 16 4 ATTR {} {json}", json.len()))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("no spaces in it"), "{msg}");
    }

    #[test]
    fn attr_json_without_spaces_fills_the_whole_region() {
        let json = format!(
            "{{{}}}",
            (0..14)
                .map(|i| format!("\"k{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(json.len() <= ATTR_BYTES);
        assert_eq!(
            attr_of(&format!("vadd docs v1 16 4 ATTR {} {json}", json.len())).unwrap(),
            json.as_bytes()
        );
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
        let json = format!(r#"{{"k":"{}"}}"#, "x".repeat(120));
        assert_eq!(json.len(), ATTR_BYTES);
        assert!(attr_of(&format!("vadd docs v1 16 4 ATTR 128 {json}")).is_ok());
    }

    #[test]
    fn vsim_vector_yields_index_k_dimension_and_filter() {
        let Body::Sim(s) =
            on_line("VSIM VECTOR docs 10 8192 1024 FILTER 1 cat=tech", body_of).unwrap()
        else {
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
        let Line::SimKey(s) = line_of(&t).unwrap() else {
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

    fn filter_of(line: &str) -> Result<Option<Filter>> {
        on_line(line, |t| trailing(t, layout::sim_key::TAIL)).map(|t| t.filter)
    }

    fn trailing_of(line: &str) -> Result<Trailing> {
        on_line(line, |t| trailing(t, layout::sim_key::TAIL))
    }

    #[test]
    fn filter_terms_are_anded_together() {
        let f = filter_of("VSIM KEY docs 5 v1 FILTER 2 cat=tech ts>1700000000")
            .unwrap()
            .unwrap();
        assert!(f.matches(br#"{"cat":"tech","ts":1723248000}"#));
        assert!(!f.matches(br#"{"cat":"tech","ts":1}"#));
        assert!(!f.matches(br#"{"cat":"news","ts":1723248000}"#));
    }

    #[test]
    fn an_absent_or_empty_filter_is_none() {
        assert!(filter_of("VSIM KEY docs 5 v1").unwrap().is_none());
        assert!(filter_of("VSIM KEY docs 5 v1 FILTER 0").unwrap().is_none());
    }

    #[test]
    fn a_filter_short_of_its_declared_terms_is_rejected() {
        assert!(
            filter_of("VSIM KEY docs 5 v1 FILTER 3 cat=tech")
                .unwrap_err()
                .to_string()
                .contains("declares 3 terms but 1")
        );
    }

    #[test]
    fn a_term_past_the_declared_count_reads_as_a_clause() {
        // With trailing clauses allowed, a token after the terms is a clause name, not a
        // miscount — the count says where the terms end.
        let msg = filter_of("VSIM KEY docs 5 v1 FILTER 1 cat=tech lang=ko")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("expected FILTER or WITHATTR"), "{msg}");
        assert!(msg.contains("lang=ko"), "{msg}");
    }

    #[test]
    fn withattr_is_read_with_or_without_a_filter() {
        let bare = trailing_of("VSIM KEY docs 5 v1").unwrap();
        assert!(bare.filter.is_none() && !bare.with_attr);

        let only = trailing_of("VSIM KEY docs 5 v1 WITHATTR").unwrap();
        assert!(only.filter.is_none() && only.with_attr);

        let both = trailing_of("VSIM KEY docs 5 v1 FILTER 1 cat=tech WITHATTR").unwrap();
        assert!(both.filter.is_some() && both.with_attr);

        // Either order, and case-insensitive like every other keyword.
        let flipped = trailing_of("VSIM KEY docs 5 v1 withattr FILTER 1 cat=tech").unwrap();
        assert!(flipped.filter.is_some() && flipped.with_attr);

        // `FILTER 0` still means no filtering, and says nothing about attributes.
        let empty = trailing_of("VSIM KEY docs 5 v1 FILTER 0").unwrap();
        assert!(empty.filter.is_none() && !empty.with_attr);
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

    #[test]
    fn simple_lines_parse_and_reject_wrong_arity() {
        tokens!(get = "vget docs v1");
        assert!(matches!(line_of(&get), Ok(Line::Get { .. })));
        tokens!(del = "vdel docs v1");
        assert!(matches!(line_of(&del), Ok(Line::Del { .. })));
        tokens!(drop_ = "vdrop docs");
        assert!(matches!(line_of(&drop_), Ok(Line::Drop { .. })));
        tokens!(list = "vlist");
        assert!(matches!(line_of(&list), Ok(Line::List)));
        tokens!(stats = "vstats");
        assert!(matches!(line_of(&stats), Ok(Line::Stats)));

        assert!(err("vget docs").contains("bad command line format"));
        assert!(err("vdel docs v1 extra").contains("bad command line format"));
        assert!(err("vdrop").contains("bad command line format"));
        assert!(err("vlist extra").contains("bad command line format"));
        assert!(err("vstats extra").contains("bad command line format"));
        assert!(err("nonsense").contains("unknown command"));
    }
}
