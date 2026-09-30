use super::layout;
use super::*;

fn malformed() -> Error {
    Error::bad_request("bad command line format")
}

pub fn parse<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    match tokens.command() {
        Some(Cmd::VAdd) => two_phase(tokens, layout::add::BODY_LEN, |t| {
            parse_add(t).map(Body::Add)
        }),
        Some(Cmd::VSearch) => match search_source(tokens)? {
            SimSource::Vector => two_phase(tokens, layout::search_vector::BODY_LEN, |t| {
                parse_sim(t).map(Body::Sim)
            }),
            SimSource::Key => parse_search_key(tokens).map(|s| Parsed::Line(Line::SimKey(s))),
        },
        Some(Cmd::VCreate) => parse_create(tokens).map(|c| Parsed::Line(Line::Create(c))),
        Some(Cmd::VGetAttr) => {
            let (index, id) = two_names(tokens)?;
            Ok(Parsed::Line(Line::GetAttr { index, id }))
        }
        Some(Cmd::VSetAttr) => parse_set_attr(tokens),
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

fn search_source(tokens: &Tokens) -> Result<SimSource> {
    let raw = tokens.text(layout::SEARCH_SOURCE)?;
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

            other => return Err(Error::bad_request(format!("unknown option {other}"))),
        }
    }

    create.metric.check_quant(create.quant)?;
    Ok(create)
}

fn parse_search_key<'a>(tokens: &Tokens<'a>) -> Result<SimKey<'a>> {
    use layout::search_key as at;
    if tokens.len() < at::TAIL {
        return Err(malformed());
    }
    let Trailing { filter, with_attr } = trailing(tokens, at::TAIL)?;
    Ok(SimKey {
        index: tokens.text(at::INDEX)?,
        key: tokens.text(at::VKEY)?,
        k: result_count(tokens, at::NUM)?,
        filter,
        with_attr,
    })
}

fn result_count(tokens: &Tokens, at: usize) -> Result<usize> {
    let k: usize = tokens.parse(at, "result count")?;
    if k == 0 || k > MAX_RESULTS {
        return Err(Error::bad_request(format!(
            "result count must be between 1 and {MAX_RESULTS}"
        )));
    }
    Ok(k)
}

fn parse_add(tokens: &Tokens) -> Result<Add> {
    use layout::add as at;
    if tokens.len() < at::TAIL {
        return Err(malformed());
    }
    let exptime: i64 = tokens.parse(at::EXPTIME, "expiry")?;
    if !(0..=u32::MAX as i64).contains(&exptime) {
        return Err(Error::bad_request("expiry out of range"));
    }
    Ok(Add {
        index: tokens.text(at::INDEX)?.to_owned(),
        vkey: tokens.text(at::VKEY)?.to_owned(),
        exptime: exptime as u32,
        attr: attr_clause(tokens, at::TAIL)?,
    })
}

fn parse_sim(tokens: &Tokens) -> Result<Sim> {
    use layout::search_vector as at;
    if tokens.len() < at::TAIL {
        return Err(malformed());
    }
    let Trailing { filter, with_attr } = trailing(tokens, at::TAIL)?;
    Ok(Sim {
        index: tokens.text(at::INDEX)?.to_owned(),
        k: result_count(tokens, at::NUM)?,
        filter,
        with_attr,
    })
}

/// `vsetattr <index> <vkey> [<JSON>]`
///
/// JSON을 생략하면 속성을 비운다. 길이를 받지 않게 되면서 "0바이트"라고 말할
/// 자리가 없어졌는데, 인자를 빼는 것이 그 자리를 대신한다.
fn parse_set_attr<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    use layout::setattr as at;
    let json = match tokens.len() {
        at::TAIL_EMPTY => "",
        at::TAIL => tokens.text(at::ATTR)?,
        _ => {
            return Err(Error::bad_request(
                "the attr JSON must be a single argument with no spaces in it",
            ));
        }
    };
    if json.len() > ATTR_BYTES {
        return Err(Error::bad_request(format!(
            "attr is {} bytes, over the {ATTR_BYTES}-byte limit",
            json.len()
        )));
    }
    Ok(Parsed::Line(Line::SetAttr {
        index: tokens.text(at::INDEX)?,
        id: tokens.text(at::VKEY)?,
        attr: json.as_bytes(),
    }))
}

/// `attr <JSON>` -- 길이를 받지 않는다.
///
/// JSON은 토큰 하나라 공백이 들어갈 수 없고, 그것이 곧 길이를 대신한다.
fn attr_clause(tokens: &Tokens, at: usize) -> Result<Vec<u8>> {
    if tokens.len() <= at {
        return Ok(Vec::new());
    }
    let keyword = tokens.text(at)?;
    if !keyword.eq_ignore_ascii_case("ATTR") {
        return Err(Error::bad_request(format!(
            "expected attr, got '{keyword}'"
        )));
    }

    let json = tokens.text(at + 1)?;
    if tokens.len() > at + 2 {
        return Err(Error::bad_request(
            "the attr JSON must be a single argument with no spaces in it",
        ));
    }
    if json.len() > ATTR_BYTES {
        return Err(Error::bad_request(format!(
            "attr is {} bytes, over the {ATTR_BYTES}-byte limit",
            json.len()
        )));
    }
    Ok(json.as_bytes().to_vec())
}

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

    macro_rules! tokens {
        ($name:ident = $src:expr) => {
            let (_buf, _raw) = tokenize($src);

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
            ("vsearch", Cmd::VSearch),
            ("VSEARCH", Cmd::VSearch),
            ("VSearch", Cmd::VSearch),
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
        assert_eq!(body_len("VSEARCH VECTOR docs 10 4096"), Some(4096));
        assert_eq!(body_len("VSEARCH KEY docs v1 10"), None);
        assert_eq!(body_len("vlist"), None);
        assert_eq!(body_len("vgetattr docs v1"), None);
        assert_eq!(body_len("nonsense"), None);
    }

    #[test]
    fn a_body_length_survives_an_unusable_remainder() {
        tokens!(t = "vadd i d 28 abc");
        let Ok(Parsed::Body { len, request }) = parse(&t) else {
            panic!("expected a body-carrying command")
        };
        assert_eq!(len, 28);
        assert!(request.unwrap_err().to_string().contains("expiry"));
    }

    #[test]
    fn an_unusable_body_length_refuses_the_whole_line() {
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
    fn vcreate_takes_no_size_or_expiry_option() {
        // 둘 다 명령에서 빠졌다. 크기 상한은 이제 `vcreate`가 정하지 않고,
        // 만료는 벡터마다 `vadd`가 받는다.
        assert!(err("vcreate docs 8 MAXCOUNT 100").contains("unknown option"));
        assert!(err("vcreate docs 8 EXPTIME 60").contains("unknown option"));
    }

    #[test]
    fn incompatible_metric_and_quantization_are_rejected_at_parse_time() {
        assert!(err("vcreate docs 8 QUANT b1 METRIC cos").contains("b1"));
        assert!(err("vcreate docs 8 QUANT f32 METRIC hamming").contains("b1"));
        tokens!(t = "vcreate docs 8 QUANT b1 METRIC hamming");
        assert!(parse_create(&t).is_ok());
    }

    #[test]
    fn vadd_yields_index_id_dimension_and_attr() {
        let Body::Add(a) = on_line(
            r#"vadd index doc1 7 0 attr {"abc":123,"def":"abc"}"#,
            body_of,
        )
        .unwrap() else {
            panic!("expected an Add")
        };
        assert_eq!(a.index, "index");
        assert_eq!(a.vkey, "doc1");
        assert_eq!(a.exptime, 0);
        assert_eq!(a.attr, br#"{"abc":123,"def":"abc"}"#);
    }

    #[test]
    fn vadd_attr_is_optional() {
        let Body::Add(a) = on_line("vadd index doc1 7 2", body_of).unwrap() else {
            panic!("expected an Add")
        };
        assert_eq!(a.exptime, 2);
        assert!(a.attr.is_empty());
    }

    #[test]
    fn vadd_needs_an_expiry() {
        assert!(err("vadd index doc1 28").contains("bad command line format"));
        assert!(err("vadd index doc1 28 abc").contains("expiry"));
        assert!(err("vadd index doc1 28 -1").contains("expiry"));
    }

    fn attr_of(line: &str) -> Result<Vec<u8>> {
        on_line(line, |t| attr_clause(t, layout::add::TAIL))
    }

    #[test]
    fn attr_json_must_be_one_token() {
        let json = r#"{"cat": "tech"}"#;
        let msg = attr_of(&format!("vadd docs v1 16 0 attr {json}"))
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
            attr_of(&format!("vadd docs v1 16 0 attr {json}")).unwrap(),
            json.as_bytes()
        );
    }

    #[test]
    fn the_attr_keyword_and_its_json_are_validated() {
        assert!(attr_of("vadd docs v1 16 0 attr {}").is_ok());
        assert!(
            attr_of("vadd docs v1 16 0 ATTRS {}")
                .unwrap_err()
                .to_string()
                .contains("expected attr")
        );
        // 길이를 따로 받지 않으니, 한도를 넘는지는 실제 바이트 수로 본다.
        let over = format!(r#"{{"k":"{}"}}"#, "x".repeat(250));
        assert!(
            attr_of(&format!("vadd docs v1 16 0 attr {over}"))
                .unwrap_err()
                .to_string()
                .contains("over the 255-byte limit")
        );
        // JSON은 토큰 하나여야 한다. 공백이 들어가면 두 토큰이 된다.
        assert!(
            attr_of(r#"vadd docs v1 16 0 attr {"a": 1}"#)
                .unwrap_err()
                .to_string()
                .contains("single argument")
        );
    }

    #[test]
    fn an_attr_exactly_at_the_limit_is_accepted() {
        let json = format!(r#"{{"k":"{}"}}"#, "x".repeat(247));
        assert_eq!(json.len(), ATTR_BYTES);
        assert!(attr_of(&format!("vadd docs v1 16 0 attr {json}")).is_ok());
    }

    #[test]
    fn vsim_vector_yields_index_k_dimension_and_filter() {
        let Body::Sim(s) =
            on_line("VSEARCH VECTOR docs 10 8192 FILTER 1 cat=tech", body_of).unwrap()
        else {
            panic!("expected a Sim")
        };
        assert_eq!(s.index, "docs");
        assert_eq!(s.k, 10);
        assert!(s.filter.is_some());
    }

    #[test]
    fn vsim_key_yields_index_k_and_key() {
        tokens!(t = "VSEARCH KEY docs v1 5");
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
        assert!(err("VSEARCH ID docs v1 5").contains("expected VECTOR or KEY"));
        assert!(err("VSEARCH KEY docs v1 0").contains("result count"));
        assert!(err("VSEARCH VECTOR docs 0 4096").contains("result count"));
    }

    #[test]
    fn a_result_count_past_the_ceiling_is_refused_before_anything_is_sized_by_it() {
        let over = MAX_RESULTS + 1;
        assert!(err(&format!("VSEARCH KEY docs v1 {over}")).contains("result count"));
        assert!(err(&format!("VSEARCH VECTOR docs {over} 4096")).contains("result count"));

        assert!(on_line(
            &format!("VSEARCH KEY docs v1 {MAX_RESULTS}"),
            |t| { matches!(parse(t), Ok(Parsed::Line(_))) }
        ));
    }

    fn filter_of(line: &str) -> Result<Option<Filter>> {
        on_line(line, |t| trailing(t, layout::search_key::TAIL)).map(|t| t.filter)
    }

    fn trailing_of(line: &str) -> Result<Trailing> {
        on_line(line, |t| trailing(t, layout::search_key::TAIL))
    }

    #[test]
    fn filter_terms_are_anded_together() {
        let f = filter_of("VSEARCH KEY docs v1 5 FILTER 2 cat=tech ts>1700000000")
            .unwrap()
            .unwrap();
        assert!(f.matches(br#"{"cat":"tech","ts":1723248000}"#));
        assert!(!f.matches(br#"{"cat":"tech","ts":1}"#));
        assert!(!f.matches(br#"{"cat":"news","ts":1723248000}"#));
    }

    #[test]
    fn an_absent_or_empty_filter_is_none() {
        assert!(filter_of("VSEARCH KEY docs v1 5").unwrap().is_none());
        assert!(
            filter_of("VSEARCH KEY docs v1 5 FILTER 0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_filter_short_of_its_declared_terms_is_rejected() {
        assert!(
            filter_of("VSEARCH KEY docs v1 5 FILTER 3 cat=tech")
                .unwrap_err()
                .to_string()
                .contains("declares 3 terms but 1")
        );
    }

    #[test]
    fn a_term_past_the_declared_count_reads_as_a_clause() {
        let msg = filter_of("VSEARCH KEY docs v1 5 FILTER 1 cat=tech lang=ko")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("expected FILTER or WITHATTR"), "{msg}");
        assert!(msg.contains("lang=ko"), "{msg}");
    }

    #[test]
    fn withattr_is_read_with_or_without_a_filter() {
        let bare = trailing_of("VSEARCH KEY docs v1 5").unwrap();
        assert!(bare.filter.is_none() && !bare.with_attr);

        let only = trailing_of("VSEARCH KEY docs v1 5 WITHATTR").unwrap();
        assert!(only.filter.is_none() && only.with_attr);

        let reversed = trailing_of("VSEARCH KEY docs v1 5 WITHATTR FILTER 1 cat=tech").unwrap();
        assert!(
            reversed.with_attr && reversed.filter.is_some(),
            "either order is accepted"
        );

        let both = trailing_of("VSEARCH KEY docs v1 5 FILTER 1 cat=tech WITHATTR").unwrap();
        assert!(both.filter.is_some() && both.with_attr);

        let flipped = trailing_of("VSEARCH KEY docs v1 5 withattr FILTER 1 cat=tech").unwrap();
        assert!(flipped.filter.is_some() && flipped.with_attr);

        let empty = trailing_of("VSEARCH KEY docs v1 5 FILTER 0").unwrap();
        assert!(empty.filter.is_none() && !empty.with_attr);
    }

    #[test]
    fn a_misspelled_or_unparseable_filter_is_reported() {
        assert!(
            filter_of("VSEARCH KEY docs v1 5 FILTERS 1 cat=tech")
                .unwrap_err()
                .to_string()
                .contains("expected FILTER")
        );
        assert!(filter_of("VSEARCH KEY docs v1 5 FILTER 1 nonsense").is_err());
    }

    #[test]
    fn set_attr_takes_one_json_token() {
        tokens!(set = r#"vsetattr docs v1 {"cat":"tech"}"#);
        let Ok(Line::SetAttr { index, id, attr }) = line_of(&set) else {
            panic!("vsetattr did not parse");
        };
        assert_eq!((index, id), ("docs", "v1"));
        assert_eq!(attr, br#"{"cat":"tech"}"#);
    }

    #[test]
    fn set_attr_wants_exactly_one_json_argument() {
        // 길이를 받지 않으므로 인자 수가 곧 검사다. 공백이 든 JSON은 두
        // 토큰이 되어 여기서 걸린다.
        assert!(err(r#"vsetattr docs v1 {} extra"#).contains("single argument"));

        // JSON을 생략하면 비우는 것이다.
        tokens!(clear = "vsetattr docs v1");
        let Ok(Line::SetAttr { attr, .. }) = line_of(&clear) else {
            panic!("clearing did not parse");
        };
        assert!(attr.is_empty());

        let over = format!(r#"{{"k":"{}"}}"#, "x".repeat(250));
        assert!(err(&format!("vsetattr docs v1 {over}")).contains("over the"));
    }

    #[test]
    fn simple_lines_parse_and_reject_wrong_arity() {
        tokens!(get = "vgetattr docs v1");
        assert!(matches!(line_of(&get), Ok(Line::GetAttr { .. })));
        tokens!(del = "vdel docs v1");
        assert!(matches!(line_of(&del), Ok(Line::Del { .. })));
        tokens!(drop_ = "vdrop docs");
        assert!(matches!(line_of(&drop_), Ok(Line::Drop { .. })));
        tokens!(list = "vlist");
        assert!(matches!(line_of(&list), Ok(Line::List)));
        tokens!(stats = "vstats");
        assert!(matches!(line_of(&stats), Ok(Line::Stats)));

        assert!(err("vgetattr docs").contains("bad command line format"));
        assert!(err("vdel docs v1 extra").contains("bad command line format"));
        assert!(err("vdrop").contains("bad command line format"));
        assert!(err("vlist extra").contains("bad command line format"));
        assert!(err("vstats extra").contains("bad command line format"));
        assert!(err("nonsense").contains("unknown command"));
    }
}
