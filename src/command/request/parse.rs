use super::layout;
use super::*;
use crate::command::tokens::Tokens;

fn malformed() -> Error {
    Error::bad_request("bad command line format")
}

pub fn parse<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    match tokens.command() {
        Some(Cmd::Vop) => {}
        None => {
            return Err(Error::bad_request(format!(
                "unknown command {}",
                tokens.text(0).unwrap_or("")
            )));
        }
    }
    let raw = tokens.text(layout::SUB)?;
    let sub =
        Sub::parse(raw).ok_or_else(|| Error::bad_request(format!("unknown subcommand '{raw}'")))?;
    match sub {
        Sub::Create => parse_create(tokens).map(|c| Parsed::Line(Line::Create(c))),
        Sub::Upsert => parse_upsert(tokens),
        Sub::SetMeta => parse_setmeta(tokens),
        Sub::Search => parse_search(tokens),
        Sub::GetMeta => {
            let (index, vkey) = keyed(tokens)?;
            Ok(Parsed::Line(Line::GetMeta(GetMeta { index, vkey })))
        }
        Sub::Delete => {
            let (index, vkey) = keyed(tokens)?;
            Ok(Parsed::Line(Line::Delete(Delete { index, vkey })))
        }
        Sub::Drop => {
            if tokens.len() != layout::drop::LEN {
                return Err(malformed());
            }
            Ok(Parsed::Line(Line::Drop(Drop {
                index: tokens.text(layout::INDEX)?,
            })))
        }
    }
}

fn keyed<'a>(tokens: &Tokens<'a>) -> Result<(&'a str, &'a str)> {
    if tokens.len() != layout::keyed::LEN {
        return Err(malformed());
    }
    Ok((
        tokens.text(layout::INDEX)?,
        tokens.text(layout::keyed::VKEY)?,
    ))
}

fn parse_create<'a>(tokens: &Tokens<'a>) -> Result<Create<'a>> {
    use layout::create as at;
    if tokens.len() <= at::EXPTIME {
        return Err(malformed());
    }
    let dim: usize = tokens.parse(at::DIM, "dimension")?;
    if dim == 0 || dim > u16::MAX as usize {
        return Err(Error::bad_request("dimension out of range (1..65535)"));
    }
    let mut create = Create {
        index: tokens.text(layout::INDEX)?,
        dim,
        exptime: tokens.parse(at::EXPTIME, "expiretime")?,
        maxcount: None,
        metric: Metric::Cos,
        quant: Quant::F32,
        m: DEFAULT_M,
        efc: DEFAULT_EFC,
        efs: DEFAULT_EFS,
    };

    // 다섯째 토큰이 숫자면 maxcount이고, 아니면 옵션이 거기서 시작한다.
    let mut options_at = at::MAXCOUNT_OR_OPTIONS;
    if let Ok(raw) = tokens.text(options_at)
        && raw.parse::<u32>().is_ok()
    {
        let n: u32 = tokens.parse(options_at, "maxcount")?;
        if n == 0 {
            return Err(Error::bad_request(format!("maxcount out of range '{raw}'")));
        }
        create.maxcount = Some(n);
        options_at += 1;
    }

    for (key, raw) in tokens.options(options_at)? {
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
            "M" => create.m = number("M")?,
            "EFC" => create.efc = number("efc")?,
            "EFS" => create.efs = number("efs")?,
            other => {
                return Err(Error::bad_request(format!(
                    "unknown option {}",
                    other.to_ascii_lowercase()
                )));
            }
        }
    }

    create.metric.check_quant(create.quant)?;
    Ok(create)
}

/// 꼬리 옵션(`meta <n>`, `filter <n>`, `withmeta`, `bin|str`)에서 읽은 것.
///
/// 순서는 가리지 않는다. 잘못된 옵션을 만나도 그때까지 읽은 구간 크기는
/// 남아 있어서, 호출한 쪽이 본문을 비울 길이를 알 수 있다.
#[derive(Default)]
struct Tail {
    meta: Option<usize>,
    filter: Option<usize>,
    with_meta: bool,
    format: Format,
}

/// `allowed`에 든 옵션만 받는다. 실패하면 읽다 만 `Tail`을 함께 돌려준다.
fn read_tail(
    tokens: &Tokens,
    from: usize,
    meta: bool,
    filter: bool,
    with_meta: bool,
    format: bool,
) -> (Tail, Result<()>) {
    let mut tail = Tail::default();
    let outcome = (|| {
        let mut i = from;
        while i < tokens.len() {
            let word = tokens.text(i)?;
            let is = |name: &str| word.eq_ignore_ascii_case(name);
            if meta && is("meta") {
                if tail.meta.is_some() {
                    return Err(Error::bad_request("meta given twice"));
                }
                tail.meta = Some(tokens.parse(i + 1, "metadata length")?);
                i += 2;
            } else if filter && is("filter") {
                if tail.filter.is_some() {
                    return Err(Error::bad_request("filter given twice"));
                }
                let n: usize = tokens.parse(i + 1, "filter length")?;
                tail.filter = Some(n);
                if n > MAX_FILTER_BYTES {
                    return Err(Error::bad_request(format!(
                        "filter is {n} bytes, over the {MAX_FILTER_BYTES}-byte limit"
                    )));
                }
                i += 2;
            } else if with_meta && is("withmeta") {
                tail.with_meta = true;
                i += 1;
            } else if format && is("bin") {
                tail.format = Format::Bin;
                i += 1;
            } else if format && is("str") {
                tail.format = Format::Str;
                i += 1;
            } else {
                return Err(Error::bad_request(format!(
                    "unknown option {}",
                    word.to_ascii_lowercase()
                )));
            }
        }
        Ok(())
    })();
    (tail, outcome)
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

/// 본문 길이를 알아낸 뒤의 결과를 `Parsed::Body`로 싼다. 요청이 틀려도
/// 길이만 알면 본문을 비울 수 있다.
fn with_body<'a>(segments: Segments, request: Result<Body>) -> Result<Parsed<'a>> {
    Ok(Parsed::Body {
        len: segments.total(),
        request,
    })
}

fn parse_upsert<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    use layout::upsert as at;
    if tokens.len() <= at::VECTOR_BYTES {
        return Err(malformed());
    }
    let vector: usize = tokens.parse(at::VECTOR_BYTES, "vector length")?;
    let (tail, outcome) = read_tail(tokens, at::TAIL, true, false, false, true);
    let segments = Segments {
        meta: tail.meta,
        filter: None,
        vector: Some(vector),
    };
    let request = outcome.and_then(|()| {
        Ok(Body::Upsert(Upsert {
            index: tokens.text(layout::INDEX)?.to_owned(),
            vkey: tokens.text(at::VKEY)?.to_owned(),
            format: tail.format,
            has_meta: tail.meta.is_some(),
            segments,
        }))
    });
    with_body(segments, request)
}

fn parse_setmeta<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    use layout::setmeta as at;
    if tokens.len() != at::LEN {
        return Err(malformed());
    }
    let meta: usize = tokens.parse(at::META_BYTES, "metadata length")?;
    let segments = Segments {
        meta: Some(meta),
        ..Segments::default()
    };
    let request = || {
        Ok(Body::SetMeta(SetMeta {
            index: tokens.text(layout::INDEX)?.to_owned(),
            vkey: tokens.text(at::VKEY)?.to_owned(),
            segments,
        }))
    };
    with_body(segments, request())
}

fn parse_search<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    let raw = tokens.text(layout::search::SOURCE)?;
    if raw.eq_ignore_ascii_case("vkey") {
        parse_search_key(tokens)
    } else if raw.eq_ignore_ascii_case("vector") {
        parse_search_vector(tokens)
    } else {
        Err(Error::bad_request(format!(
            "expected vkey or vector, got '{raw}'"
        )))
    }
}

fn parse_search_key<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    use layout::search_key as at;
    if tokens.len() <= at::NUM {
        return Err(malformed());
    }
    let k = result_count(tokens, at::NUM)?;
    let (tail, outcome) = read_tail(tokens, at::TAIL, false, true, true, false);
    let segments = Segments {
        filter: tail.filter,
        ..Segments::default()
    };
    let request = outcome.and_then(|()| {
        Ok(SearchKey {
            index: tokens.text(layout::INDEX)?.to_owned(),
            vkey: tokens.text(at::VKEY)?.to_owned(),
            k,
            with_meta: tail.with_meta,
            has_filter: tail.filter.is_some(),
            segments,
        })
    });
    match (tail.filter, request) {
        // 필터 본문이 없으면 줄만으로 끝난다.
        (None, request) => request.map(|s| Parsed::Line(Line::SearchKey(s))),
        (Some(_), request) => with_body(segments, request.map(Body::SearchKey)),
    }
}

fn parse_search_vector<'a>(tokens: &Tokens<'a>) -> Result<Parsed<'a>> {
    use layout::search_vector as at;
    if tokens.len() <= at::VECTOR_BYTES {
        return Err(malformed());
    }
    let k = result_count(tokens, at::NUM)?;
    let vector: usize = tokens.parse(at::VECTOR_BYTES, "vector length")?;
    let (tail, outcome) = read_tail(tokens, at::TAIL, false, true, true, true);
    let segments = Segments {
        meta: None,
        filter: tail.filter,
        vector: Some(vector),
    };
    let request = outcome.and_then(|()| {
        Ok(Body::SearchVector(SearchVector {
            index: tokens.text(layout::INDEX)?.to_owned(),
            k,
            format: tail.format,
            with_meta: tail.with_meta,
            has_filter: tail.filter.is_some(),
            segments,
        }))
    });
    with_body(segments, request)
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

    fn line_of<'a>(tokens: &Tokens<'a>) -> Result<Line<'a>> {
        match parse(tokens)? {
            Parsed::Line(line) => Ok(line),
            Parsed::Body { .. } => panic!("expected a line-only command"),
        }
    }

    fn body_of(line: &str) -> (usize, Body) {
        tokens!(t = line);
        match parse(&t).unwrap() {
            Parsed::Body { len, request } => (len, request.unwrap()),
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

    fn create_of(line: &str) -> Create<'static> {
        tokens!(t = line);
        let Ok(Line::Create(c)) = line_of(&t) else {
            panic!("expected a create")
        };
        // 테스트 안에서만: 토큰 버퍼가 사라져도 쓰도록 문자열을 새어 둔다.
        Create {
            index: Box::leak(c.index.to_owned().into_boxed_str()),
            ..c
        }
    }

    #[test]
    fn vop_and_its_subcommands_are_case_insensitive() {
        assert_eq!(Cmd::parse("vop"), Some(Cmd::Vop));
        assert_eq!(Cmd::parse("VOP"), Some(Cmd::Vop));
        assert_eq!(Cmd::parse("Vop"), Some(Cmd::Vop));
        assert_eq!(Cmd::parse("vadd"), None);
        assert_eq!(Cmd::parse("vcreate"), None);
        assert_eq!(Cmd::parse(""), None);
        for (text, sub) in [
            ("create", Sub::Create),
            ("UPSERT", Sub::Upsert),
            ("SetMeta", Sub::SetMeta),
            ("Search", Sub::Search),
            ("getmeta", Sub::GetMeta),
            ("DELETE", Sub::Delete),
            ("drop", Sub::Drop),
        ] {
            assert_eq!(Sub::parse(text), Some(sub), "{text}");
        }
        assert_eq!(Sub::parse("list"), None);
        assert!(matches!(
            {
                tokens!(t = "VOP DROP docs");
                line_of(&t)
            },
            Ok(Line::Drop(_))
        ));
    }

    #[test]
    fn create_fills_the_pdf_defaults() {
        let c = create_of("vop create docs 8 0");
        assert_eq!(c.index, "docs");
        assert_eq!(c.dim, 8);
        assert_eq!(c.exptime, 0);
        assert_eq!(c.m, 16);
        assert_eq!(c.efc, 128);
        assert_eq!(c.efs, 64);
        assert_eq!(c.metric, Metric::Cos);
        assert_eq!(c.quant, Quant::F32);
        assert_eq!(c.maxcount, None);
    }

    #[test]
    fn create_reads_a_positional_maxcount_then_options() {
        let c = create_of("vop create docs 8 60 100 metric l2 quant f16 M 8 efc 64 efs 32");
        assert_eq!(c.exptime, 60);
        assert_eq!(c.maxcount, Some(100));
        assert_eq!(c.metric, Metric::L2);
        assert_eq!(c.quant, Quant::F16);
        assert_eq!((c.m, c.efc, c.efs), (8, 64, 32));

        let c = create_of("vop create docs 8 60 METRIC l2");
        assert_eq!(c.maxcount, None, "키워드면 옵션의 시작이다");
        assert_eq!(c.metric, Metric::L2);
    }

    #[test]
    fn create_requires_an_expiretime() {
        assert!(err("vop create docs 8").contains("bad command line format"));
        assert!(err("vop create docs").contains("bad command line format"));
        assert!(err("vop create docs 8 abc").contains("expiretime"));
        assert!(err("vop create docs 0 0").contains("out of range"));
        assert!(err("vop create docs 8 0 0").contains("maxcount"));
        assert!(err("vop create docs 8 0 nope 1").contains("unknown option"));
        assert!(err("vop create docs 8 0 100 efs").contains("needs a value"));
    }

    #[test]
    fn create_rejects_b1_with_cos_and_hamming_without_b1() {
        assert!(err("vop create docs 8 0 quant b1 metric cos").contains("b1"));
        assert!(err("vop create docs 8 0 quant f32 metric hamming").contains("b1"));
        tokens!(t = "vop create docs 8 0 quant b1 metric hamming");
        assert!(line_of(&t).is_ok());
    }

    #[test]
    fn upsert_body_is_meta_then_vector() {
        let (len, body) = body_of("vop upsert i v 8 meta 7");
        assert_eq!(len, 7 + 2 + 8 + 2);
        let Body::Upsert(u) = body else {
            panic!("expected an upsert")
        };
        assert_eq!((u.index.as_str(), u.vkey.as_str()), ("i", "v"));
        assert!(u.has_meta);
        assert_eq!(u.format, Format::Bin);

        let mut raw = br#"{"a":1}"#.to_vec();
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(&[9; 8]);
        raw.extend_from_slice(b"\r\n");
        let parts = u.segments.split(&raw).unwrap();
        assert_eq!(parts.meta, Some(&br#"{"a":1}"#[..]));
        assert_eq!(parts.vector, Some(&[9u8; 8][..]));
        assert_eq!(parts.filter, None);
    }

    #[test]
    fn upsert_without_meta_is_vector_only() {
        let (len, body) = body_of("vop upsert i v 8 str");
        assert_eq!(len, 8 + 2);
        let Body::Upsert(u) = body else {
            panic!("expected an upsert")
        };
        assert!(!u.has_meta);
        assert_eq!(u.format, Format::Str);
        assert_eq!(u.segments.meta, None);
    }

    #[test]
    fn a_short_vector_after_a_good_meta_is_a_bad_data_chunk() {
        let (_, Body::Upsert(u)) = body_of("vop upsert i v 8 meta 7") else {
            panic!("expected an upsert")
        };
        // 길이가 모자란 것도, 길이는 맞는데 구간 끝이 \r\n이 아닌 것도 거절한다.
        let mut short = br#"{"a":1}"#.to_vec();
        short.extend_from_slice(b"\r\n123456\r\n");
        let msg = u.segments.split(&short).unwrap_err().to_string();
        assert_eq!(msg, "bad data chunk");

        let mut bad_end = br#"{"a":1}"#.to_vec();
        bad_end.extend_from_slice(b"\r\n12345678xx");
        assert_eq!(
            u.segments.split(&bad_end).unwrap_err().to_string(),
            "bad data chunk"
        );

        let mut bad_meta = br#"{"a":1}"#.to_vec();
        bad_meta.extend_from_slice(b"xx12345678\r\n");
        assert_eq!(
            u.segments.split(&bad_meta).unwrap_err().to_string(),
            "bad data chunk"
        );
    }

    #[test]
    fn search_vector_reads_num_before_vector_bytes() {
        let (len, body) = body_of("vop search docs vector 10 16 filter 5 withmeta str");
        assert_eq!(len, 5 + 2 + 16 + 2);
        let Body::SearchVector(s) = body else {
            panic!("expected a vector search")
        };
        assert_eq!(s.index, "docs");
        assert_eq!(s.k, 10);
        assert_eq!(s.segments.vector, Some(16));
        assert_eq!(s.segments.filter, Some(5));
        assert!(s.has_filter && s.with_meta);
        assert_eq!(s.format, Format::Str);

        let mut raw = b"a = 1".to_vec();
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(&[7; 16]);
        raw.extend_from_slice(b"\r\n");
        let parts = s.segments.split(&raw).unwrap();
        assert_eq!(parts.filter, Some(&b"a = 1"[..]));
        assert_eq!(parts.vector, Some(&[7u8; 16][..]));
        assert_eq!(parts.meta, None);

        let (len, _) = body_of("vop search docs vector 3 16");
        assert_eq!(len, 18, "필터가 없으면 벡터 구간뿐이다");
    }

    #[test]
    fn search_options_come_in_any_order() {
        for tail in [
            "filter 5 withmeta str",
            "withmeta filter 5 str",
            "str withmeta filter 5",
            "STR WITHMETA FILTER 5",
        ] {
            let (len, body) = body_of(&format!("vop search docs vector 10 16 {tail}"));
            assert_eq!(len, 25, "{tail}");
            let Body::SearchVector(s) = body else {
                panic!("expected a vector search")
            };
            assert!(s.has_filter && s.with_meta, "{tail}");
            assert_eq!(s.format, Format::Str, "{tail}");
        }
        tokens!(t = "vop search docs vkey v1 5 withmeta");
        let Ok(Line::SearchKey(s)) = line_of(&t) else {
            panic!("expected a key search line")
        };
        assert!(s.with_meta && !s.has_filter);
    }

    #[test]
    fn search_by_vkey_carries_only_a_filter_body() {
        tokens!(t = "vop search docs vkey v1 5");
        let Ok(Line::SearchKey(s)) = line_of(&t) else {
            panic!("expected a key search line")
        };
        assert_eq!((s.index.as_str(), s.vkey.as_str(), s.k), ("docs", "v1", 5));

        let (len, body) = body_of("vop search docs vkey v1 5 withmeta filter 12");
        assert_eq!(len, 14);
        let Body::SearchKey(s) = body else {
            panic!("expected a key search body")
        };
        assert!(s.has_filter && s.with_meta);
        assert_eq!(s.segments.filter, Some(12));
        assert_eq!(s.segments.vector, None);
    }

    #[test]
    fn setmeta_body_is_the_metadata_alone() {
        let (len, body) = body_of("vop setmeta docs v1 7");
        assert_eq!(len, 9);
        let Body::SetMeta(m) = body else {
            panic!("expected a setmeta")
        };
        assert_eq!((m.index.as_str(), m.vkey.as_str()), ("docs", "v1"));
        let parts = m.segments.split(b"{\"a\":1}\r\n").unwrap();
        assert_eq!(parts.meta, Some(&br#"{"a":1}"#[..]));
        assert!(err("vop setmeta docs v1").contains("bad command line format"));
        assert!(err("vop setmeta docs v1 abc").contains("metadata length"));
    }

    #[test]
    fn getmeta_delete_drop_check_arity() {
        tokens!(g = "vop getmeta docs v1");
        assert!(matches!(line_of(&g), Ok(Line::GetMeta(_))));
        tokens!(d = "vop delete docs v1");
        assert!(matches!(line_of(&d), Ok(Line::Delete(_))));
        tokens!(p = "vop drop docs");
        assert!(matches!(line_of(&p), Ok(Line::Drop(_))));

        assert!(err("vop getmeta docs").contains("bad command line format"));
        assert!(err("vop delete docs v1 extra").contains("bad command line format"));
        assert!(err("vop drop").contains("bad command line format"));
        assert!(err("vop drop docs extra").contains("bad command line format"));
        assert!(err("vop").contains("bad command line format"));
    }

    #[test]
    fn an_unknown_subcommand_or_option_is_a_client_error() {
        assert!(err("vop list").contains("unknown subcommand"));
        assert!(err("vop search docs ids v1 5").contains("expected vkey or vector"));
        assert!(err("vop search docs vkey v1 0").contains("result count"));
        assert!(err("vop search docs vector 0 16").contains("result count"));
        assert!(err("vop search docs vector 5 16 bogus").contains("unknown option"));
        assert!(err("vop upsert i v 8 attr 7").contains("unknown option"));
        assert!(err("vop upsert i v 8 meta").contains("bad command line format"));
        assert!(err("vop upsert i v abc").contains("vector length"));
        assert!(err("vop search docs vector 5 16 filter").contains("bad command line format"));
        assert!(err("vcreate docs 8").contains("unknown command"));
    }

    #[test]
    fn a_filter_over_4096_bytes_is_a_client_error() {
        assert!(err("vop search docs vector 5 16 filter 4096").contains("accepted"));
        assert!(err("vop search docs vector 5 16 filter 4097").contains("filter"));
        assert!(err("vop search docs vkey v1 5 filter 4097").contains("filter"));
    }

    #[test]
    fn a_body_over_the_limit_is_refused_but_drained() {
        let line = format!("vop upsert i v {}", MAX_BODY_BYTES);
        tokens!(t = &line);
        let Ok(Parsed::Body { len, .. }) = parse(&t) else {
            panic!("expected a body-carrying command")
        };
        assert!(len > MAX_BODY_BYTES, "accept는 버퍼를 열지 않는다");
        assert!(
            body_refused(len).to_string().contains("transfer limit"),
            "execute가 답하는 메시지"
        );
        assert_eq!(body_refused(28).to_string(), "lost command state");

        // 숫자가 넘쳐도 패닉 없이 상한에 걸린다.
        let line = format!("vop upsert i v {} meta {}", usize::MAX, usize::MAX);
        tokens!(t = &line);
        let Ok(Parsed::Body { len, .. }) = parse(&t) else {
            panic!("expected a body-carrying command")
        };
        assert!(len > MAX_BODY_BYTES);
    }

    #[test]
    fn a_body_length_survives_an_unusable_remainder() {
        tokens!(t = "vop upsert i v 28 nope");
        let Ok(Parsed::Body { len, request }) = parse(&t) else {
            panic!("expected a body-carrying command")
        };
        assert_eq!(len, 30, "본문은 그래도 비워야 한다");
        assert!(request.unwrap_err().to_string().contains("unknown option"));
    }
}
