use crate::Metric;
use crate::bson;
use crate::handler::quant::Quant;

/// 빈 BSON 문서의 크기. 메타가 없는 요소도 이만큼은 차지한다.
const EMPTY_META_BYTES: usize = bson::EMPTY.len();

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    BadMetadata(String),
    /// 설정 문서는 읽혔으나 값이 허용 범위 밖이다(복제/복구로 들어온 값).
    OutOfRange(String),
    UnknownQuant(u8),
    Truncated { need: usize, got: usize },
    LayoutMismatch,
    VectorLenMismatch { need: usize, got: usize },
}

impl std::error::Error for CodecError {}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMetadata(m) => write!(f, "index metadata is unusable: {m}"),
            Self::OutOfRange(m) => write!(f, "index config out of range: {m}"),
            Self::UnknownQuant(q) => write!(f, "unknown quantization {q}"),
            Self::Truncated { need, got } => {
                write!(f, "element truncated (need {need} bytes, got {got})")
            }
            Self::LayoutMismatch => f.write_str("element layout does not match index"),
            Self::VectorLenMismatch { need, got } => {
                write!(f, "vector length mismatch (need {need} bytes, got {got})")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    pub dim: usize,
    pub quant: Quant,
}

impl Layout {
    pub const fn new(dim: usize, quant: Quant) -> Self {
        Self { dim, quant }
    }

    pub const fn vector_bytes(&self) -> usize {
        self.quant.vector_bytes(self.dim)
    }

    pub const STORED_TERMINATOR: usize = 2;

    /// BSON 메타 문서, 벡터 순서의 요소 크기.
    ///
    /// 메타는 문서 첫 4바이트가 제 길이를 말하므로 읽는 쪽은 언제나 그것을
    /// 먼저 본다. 벡터가 앉는 자리는 요소마다 다르다.
    pub const fn element_len(&self, meta_len: usize) -> usize {
        meta_len + self.vector_bytes()
    }

    /// 엔진에 실제로 놓이는 크기. `insert_elem`이 CRLF를 붙인다.
    pub const fn stored_len(&self, meta_len: usize) -> usize {
        self.element_len(meta_len) + Self::STORED_TERMINATOR
    }

    /// 크기 한도 안에서 담을 수 있는 가장 큰 차원.
    ///
    /// 메타가 가장 작은 경우(빈 문서)를 기준으로 한다. 메타가 커지는 만큼은
    /// 쓰는 시점에 `stored_len`으로 다시 잰다.
    pub const fn max_dim_for(quant: Quant, max_element_bytes: usize) -> usize {
        let overhead = EMPTY_META_BYTES + Self::STORED_TERMINATOR;
        if max_element_bytes <= overhead {
            return 0;
        }
        quant.max_dim(max_element_bytes - overhead)
    }

    pub fn encode(&self, vector: &[u8], meta: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut buf = vec![0u8; self.element_len(meta.len())];
        self.write(&mut buf, vector, meta)?;
        Ok(buf)
    }

    /// 메타, 벡터를 차례로 쓴다.
    ///
    /// `buf`는 정확히 `element_len(meta.len())` 이어야 한다. 호출자가 자리를
    /// 직접 잡을 수 있도록(`room`) 나눠 둔 것이다.
    pub fn write(&self, buf: &mut [u8], vector: &[u8], meta: &[u8]) -> Result<(), CodecError> {
        if vector.len() != self.vector_bytes() {
            return Err(CodecError::VectorLenMismatch {
                need: self.vector_bytes(),
                got: vector.len(),
            });
        }
        if bson::doc_len(meta) != Some(meta.len()) {
            return Err(CodecError::BadMetadata(
                "element metadata is not a BSON document".to_owned(),
            ));
        }
        let need = self.element_len(meta.len());
        if buf.len() != need {
            return Err(CodecError::Truncated {
                need,
                got: buf.len(),
            });
        }
        buf[..meta.len()].copy_from_slice(meta);
        buf[meta.len()..].copy_from_slice(vector);
        Ok(())
    }

    /// 값을 메타와 벡터로 나눈다.
    ///
    /// 메타 뒤에는 벡터만 있어야 한다. 엔진이 붙인 CRLF는 있어도 되고 없어도
    /// 되지만, 그 밖의 바이트가 남으면 이 인덱스의 요소가 아니다 -- 차원이나
    /// 양자화가 다른 값을 벡터로 읽어 그래프에 넣는 것보다 거절하는 편이 낫다.
    pub fn split<'a>(&self, value: &'a [u8]) -> Result<(&'a [u8], &'a [u8]), CodecError> {
        let declared = value
            .get(..4)
            .and_then(|h| <[u8; 4]>::try_from(h).ok())
            .map(i32::from_le_bytes)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n >= EMPTY_META_BYTES);
        let Some(meta_len) = declared else {
            return Err(CodecError::Truncated {
                need: EMPTY_META_BYTES,
                got: value.len(),
            });
        };
        let need = self.element_len(meta_len);
        if value.len() < need {
            return Err(CodecError::Truncated {
                need,
                got: value.len(),
            });
        }
        let tail = &value[need..];
        if !tail.is_empty() && tail != b"\r\n" {
            return Err(CodecError::LayoutMismatch);
        }
        Ok((&value[..meta_len], &value[meta_len..need]))
    }
}

/// 인덱스 설정. 인덱스 Map 아이템의 값으로 저장된다.
///
/// BSON 키는 `v`(=1), `dim`, `metric`, `quant`, `M`, `efc`, `efs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IndexConfig {
    pub dim: usize,
    pub metric: Metric,
    pub quant: Quant,
    pub m: usize,
    pub efc: usize,
    pub efs: usize,
}

/// HNSW 설정의 허용 범위. usearch는 M을 거의 그대로 노드마다의 이웃 배열 크기로
/// 쓴다 -- 시험에서 M=u32::MAX는 인덱스를 만드는 자리에서 메모리를 다 쓰고
/// 프로세스가 죽었다. 값이 cache lock 아래 `ITEM_LINK` 콜백에서 인덱스를 지을 때
/// 쓰이므로, `create`와 설정 복원(복제/복구) 둘 다 이 범위로 거른다.
/// M은 2..=1024, efc/efs는 1..=65536, dim은 1..=65535.
pub const M_RANGE: std::ops::RangeInclusive<usize> = 2..=1024;
pub const EF_RANGE: std::ops::RangeInclusive<usize> = 1..=65536;
pub const DIM_RANGE: std::ops::RangeInclusive<usize> = 1..=65535;

/// 설정 문서의 형식 버전.
const CONFIG_VERSION: usize = 1;

impl IndexConfig {
    pub const fn layout(&self) -> Layout {
        Layout::new(self.dim, self.quant)
    }

    /// BSON 문서에 엔진 쓰기 계층이 붙이는 것과 같은 CRLF를 더한 바이트.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8; 4];
        put_int(&mut out, "v", CONFIG_VERSION);
        put_int(&mut out, "dim", self.dim);
        put_str(&mut out, "metric", self.metric.as_str());
        put_str(&mut out, "quant", self.quant.as_str());
        put_int(&mut out, "M", self.m);
        put_int(&mut out, "efc", self.efc);
        put_int(&mut out, "efs", self.efs);
        out.push(0);
        let len = out.len() as i32;
        out[..4].copy_from_slice(&len.to_le_bytes());
        out.extend_from_slice(b"\r\n");
        out
    }

    /// 끝의 CRLF가 있어도 없어도 읽는다.
    pub fn decode(value: &[u8]) -> Result<Self, CodecError> {
        let bad = |m: String| CodecError::BadMetadata(m);
        let doc = bson::doc_len(value)
            .map(|n| &value[..n])
            .ok_or_else(|| bad("not a BSON document".to_owned()))?;
        let num = |k: &str| -> Result<usize, CodecError> {
            match bson::lookup(doc, k.as_bytes()) {
                Some(bson::Value::Num(n))
                    if n >= 0.0 && n.fract() == 0.0 && n <= u32::MAX as f64 =>
                {
                    Ok(n as usize)
                }
                _ => Err(bad(format!("missing or invalid '{k}'"))),
            }
        };
        let text = |k: &str| -> Result<&str, CodecError> {
            match bson::lookup(doc, k.as_bytes()) {
                Some(bson::Value::Str(s)) => {
                    std::str::from_utf8(s).map_err(|_| bad(format!("'{k}' is not text")))
                }
                _ => Err(bad(format!("missing '{k}'"))),
            }
        };

        if num("v")? != CONFIG_VERSION {
            return Err(bad("unsupported config version".to_owned()));
        }
        let metric = text("metric")?;
        let metric =
            Metric::parse(metric).ok_or_else(|| bad(format!("unknown metric '{metric}'")))?;
        let quant = text("quant")?;
        let quant = Quant::parse(quant).ok_or_else(|| bad(format!("unknown quant '{quant}'")))?;
        let config = Self {
            dim: num("dim")?,
            metric,
            quant,
            m: num("M")?,
            efc: num("efc")?,
            efs: num("efs")?,
        };
        let check = |name: &str, n: usize, range: &std::ops::RangeInclusive<usize>| {
            if range.contains(&n) {
                Ok(())
            } else {
                Err(CodecError::OutOfRange(format!(
                    "{name} {n} not in {}..={}",
                    range.start(),
                    range.end()
                )))
            }
        };
        check("dim", config.dim, &DIM_RANGE)?;
        check("M", config.m, &M_RANGE)?;
        check("efc", config.efc, &EF_RANGE)?;
        check("efs", config.efs, &EF_RANGE)?;
        Ok(config)
    }
}

/// 값이 i32에 들면 int32, 아니면 int64로 쓴다.
fn put_int(out: &mut Vec<u8>, key: &str, n: usize) {
    match i32::try_from(n) {
        Ok(n) => {
            out.push(0x10);
            put_key(out, key);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Err(_) => {
            out.push(0x12);
            put_key(out, key);
            out.extend_from_slice(&(n as i64).to_le_bytes());
        }
    }
}

fn put_str(out: &mut Vec<u8>, key: &str, s: &str) {
    out.push(0x02);
    put_key(out, key);
    out.extend_from_slice(&(s.len() as i32 + 1).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn put_key(out: &mut Vec<u8>, key: &str) {
    out.extend_from_slice(key.as_bytes());
    out.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bson;
    use crate::handler::quant::encode;

    #[test]
    fn vector_bytes_matches_scalar_width() {
        assert_eq!(Quant::F32.vector_bytes(1024), 4096);
        assert_eq!(Quant::F16.vector_bytes(1024), 2048);
        assert_eq!(Quant::I8.vector_bytes(1024), 1024);
        assert_eq!(Quant::B1.vector_bytes(1024), 128);
        assert_eq!(Quant::B1.vector_bytes(1), 1);
        assert_eq!(Quant::B1.vector_bytes(9), 2);
    }

    #[test]
    fn max_dim_is_the_inverse_of_vector_bytes() {
        let budget = 16 * 1024 - 16 - 64;
        assert_eq!(Quant::F32.max_dim(budget), 4076);
        assert_eq!(Quant::F16.max_dim(budget), 8152);
        assert_eq!(Quant::I8.max_dim(budget), 16304);
        assert_eq!(Quant::B1.max_dim(budget), 130432);

        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            assert!(q.vector_bytes(q.max_dim(budget)) <= budget, "{q:?}");
        }
    }

    fn layout() -> Layout {
        Layout::new(4, Quant::I8)
    }

    #[test]
    fn encode_then_split_returns_both_parts() {
        let l = Layout::new(4, Quant::F32);
        let vector = encode(&[1.0, 2.0, 3.0, 4.0], Quant::F32).unwrap();
        let meta = bson::from_json(br#"{"cat":"tech"}"#).unwrap();

        let buf = l.encode(&vector, &meta).unwrap();
        assert_eq!(buf.len(), l.element_len(meta.len()));
        assert_eq!(l.stored_len(meta.len()), buf.len() + 2);

        let (m, v) = l.split(&buf).unwrap();
        assert_eq!(m, &meta[..]);
        assert_eq!(v, &vector[..]);

        // 엔진이 붙인 종단자가 있어도 같은 답이다.
        let mut stored = buf.clone();
        stored.extend_from_slice(b"\r\n");
        assert_eq!(l.split(&stored).unwrap(), (m, v));
    }

    #[test]
    fn an_empty_meta_is_five_bytes_and_splits_back() {
        let l = layout();
        let buf = l.encode(&[9, 8, 7, 6], &bson::EMPTY).unwrap();
        assert_eq!(buf.len(), 5 + 4);
        assert_eq!(
            l.split(&buf).unwrap(),
            (&bson::EMPTY[..], &[9u8, 8, 7, 6][..])
        );
    }

    #[test]
    fn a_vector_of_the_wrong_length_is_refused() {
        assert_eq!(
            layout().encode(&[1, 2, 3], &bson::EMPTY),
            Err(CodecError::VectorLenMismatch { need: 4, got: 3 })
        );
    }

    #[test]
    fn a_value_shorter_than_its_meta_header_is_truncated() {
        let l = layout();
        assert!(matches!(l.split(&[]), Err(CodecError::Truncated { .. })));
        assert!(matches!(
            l.split(&[5, 0]),
            Err(CodecError::Truncated { .. })
        ));

        let good = l.encode(&[1, 2, 3, 4], &bson::EMPTY).unwrap();
        // 벡터가 모자란 것도, 메타가 선언한 길이에 못 미치는 것도 잘린 것이다.
        assert_eq!(
            l.split(&good[..good.len() - 1]),
            Err(CodecError::Truncated { need: 9, got: 8 })
        );
        assert!(matches!(
            l.split(&good[..4]),
            Err(CodecError::Truncated { .. })
        ));
    }

    /// 4바이트 머리 하나로 시작하는 값. 머리가 말하는 길이만 바꿔 본다.
    fn with_header(declared: i32, rest: &[u8]) -> Vec<u8> {
        let mut value = declared.to_le_bytes().to_vec();
        value.extend_from_slice(rest);
        value
    }

    #[test]
    fn a_header_under_five_bytes_is_truncated() {
        let l = layout();
        for declared in [0, 1, 4] {
            let value = with_header(declared, &[0; 16]);
            assert!(
                matches!(l.split(&value), Err(CodecError::Truncated { .. })),
                "{declared}"
            );
        }
    }

    #[test]
    fn a_negative_header_is_truncated() {
        let value = with_header(-5, &[0; 16]);
        assert!(matches!(
            layout().split(&value),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn a_header_longer_than_the_value_is_truncated() {
        let value = with_header(64, &[0; 8]);
        assert!(matches!(
            layout().split(&value),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn bytes_past_the_vector_other_than_the_terminator_are_refused() {
        let l = layout();
        let good = l.encode(&[1, 2, 3, 4], &bson::EMPTY).unwrap();
        for tail in [&b"x"[..], b"\r", b"\n\r", b"ab", b"\r\nx", b"\r\n\r\n"] {
            let mut value = good.clone();
            value.extend_from_slice(tail);
            assert_eq!(l.split(&value), Err(CodecError::LayoutMismatch), "{tail:?}");
        }
    }

    #[test]
    fn a_meta_that_is_not_a_document_is_refused() {
        assert!(matches!(
            layout().encode(&[1, 2, 3, 4], b"{}"),
            Err(CodecError::BadMetadata(_))
        ));
    }

    #[test]
    fn config_round_trips_through_bson() {
        for metric in [
            Metric::Cos,
            Metric::L2,
            Metric::IP,
            Metric::Hamming,
            Metric::Tanimoto,
        ] {
            for quant in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                if metric.check_quant(quant).is_err() {
                    continue;
                }
                let config = IndexConfig {
                    dim: 128,
                    metric,
                    quant,
                    m: 16,
                    efc: 128,
                    efs: 64,
                };
                let bytes = config.encode();
                assert!(bytes.ends_with(b"\r\n"));
                assert_eq!(bson::doc_len(&bytes), Some(bytes.len() - 2));
                assert_eq!(IndexConfig::decode(&bytes).unwrap(), config);
                assert_eq!(
                    IndexConfig::decode(&bytes[..bytes.len() - 2]).unwrap(),
                    config
                );
                assert_eq!(config.layout(), Layout::new(128, quant));
            }
        }
    }

    fn sample() -> IndexConfig {
        IndexConfig {
            dim: 4,
            metric: Metric::L2,
            quant: Quant::F32,
            m: 16,
            efc: 128,
            efs: 64,
        }
    }

    #[test]
    fn config_round_trips_at_the_widest_allowed_values() {
        let config = IndexConfig {
            dim: *DIM_RANGE.end(),
            m: *M_RANGE.end(),
            efc: *EF_RANGE.end(),
            efs: *EF_RANGE.end(),
            ..sample()
        };
        assert_eq!(IndexConfig::decode(&config.encode()).unwrap(), config);
        let config = IndexConfig {
            dim: *DIM_RANGE.start(),
            m: *M_RANGE.start(),
            efc: *EF_RANGE.start(),
            efs: *EF_RANGE.start(),
            ..sample()
        };
        assert_eq!(IndexConfig::decode(&config.encode()).unwrap(), config);
    }

    #[test]
    fn config_outside_the_bounds_is_refused_on_decode() {
        let max = u32::MAX as usize;
        for bad in [
            IndexConfig { dim: 0, ..sample() },
            IndexConfig { dim: 65536, ..sample() },
            IndexConfig { dim: max, ..sample() },
            IndexConfig { m: 0, ..sample() },
            IndexConfig { m: 1, ..sample() },
            IndexConfig { m: 1025, ..sample() },
            IndexConfig { m: max, ..sample() },
            IndexConfig { efc: 0, ..sample() },
            IndexConfig { efc: 65537, ..sample() },
            IndexConfig { efc: max, ..sample() },
            IndexConfig { efs: 0, ..sample() },
            IndexConfig { efs: 65537, ..sample() },
            IndexConfig { efs: max, ..sample() },
        ] {
            assert!(
                matches!(
                    IndexConfig::decode(&bad.encode()),
                    Err(CodecError::OutOfRange(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn config_uses_the_spec_keys() {
        let config = IndexConfig {
            dim: 4,
            metric: Metric::L2,
            quant: Quant::F32,
            m: 16,
            efc: 128,
            efs: 64,
        };
        let json = bson::to_json(&config.encode()[..config.encode().len() - 2]).unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"dim":4,"metric":"l2","quant":"f32","M":16,"efc":128,"efs":64}"#
        );
    }

    #[test]
    fn config_with_an_unknown_metric_is_bad_metadata() {
        let doc = bson::from_json(
            br#"{"v":1,"dim":4,"metric":"nope","quant":"f32","M":16,"efc":128,"efs":64}"#,
        )
        .unwrap();
        assert!(matches!(
            IndexConfig::decode(&doc),
            Err(CodecError::BadMetadata(_))
        ));
    }

    #[test]
    fn config_with_a_missing_key_or_wrong_version_is_bad_metadata() {
        for json in [
            r#"{"v":1,"dim":4,"metric":"l2","quant":"f32","M":16,"efc":128}"#,
            r#"{"v":2,"dim":4,"metric":"l2","quant":"f32","M":16,"efc":128,"efs":64}"#,
            r#"{"v":1,"dim":-4,"metric":"l2","quant":"f32","M":16,"efc":128,"efs":64}"#,
        ] {
            let doc = bson::from_json(json.as_bytes()).unwrap();
            assert!(
                matches!(IndexConfig::decode(&doc), Err(CodecError::BadMetadata(_))),
                "{json}"
            );
        }
        assert!(matches!(
            IndexConfig::decode(b"junk"),
            Err(CodecError::BadMetadata(_))
        ));
    }

    #[test]
    fn max_dim_counts_the_empty_document_and_terminator() {
        // 빈 메타 5바이트 + 종단자 2바이트가 고정 오버헤드다.
        assert_eq!(Layout::max_dim_for(Quant::I8, 16 * 1024), 16 * 1024 - 7);
        assert_eq!(Layout::max_dim_for(Quant::I8, 16 * 1024), 16377);
        assert_eq!(
            Layout::max_dim_for(Quant::F32, 16 * 1024),
            (16 * 1024 - 7) / 4
        );
        assert_eq!(Layout::max_dim_for(Quant::I8, 7), 0);
        assert_eq!(Layout::max_dim_for(Quant::I8, 8), 1);
        assert_eq!(Layout::max_dim_for(Quant::I8, 3), 0);
    }
}
