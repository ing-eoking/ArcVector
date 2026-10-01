use crate::handler::quant::Quant;

/// 값 앞머리의 attr 길이 한 바이트.
///
/// attr은 이 바이트가 말하는 만큼만 차지한다. 예전에는 128바이트를 늘 잡아
/// 두었는데, 대부분의 벡터가 짧은 attr을 쓰거나 아예 안 써서 그만큼이 통째로
/// 낭비였다.
const ATTR_LEN_BYTES: usize = 1;

pub const META_FIELD: &str = "AV META";

pub const ATTR_OFFSET: usize = ATTR_LEN_BYTES;

/// 한 바이트로 셀 수 있는 최대치.
pub const ATTR_BYTES: usize = 255;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    BadMetadata(String),
    UnknownQuant(u8),
    Truncated { need: usize, got: usize },
    LayoutMismatch,
    AttrTooLarge { limit: usize, got: usize },
    VectorLenMismatch { need: usize, got: usize },
}

impl std::error::Error for CodecError {}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMetadata(m) => write!(f, "index metadata is unusable: {m}"),
            Self::UnknownQuant(q) => write!(f, "unknown quantization {q}"),
            Self::Truncated { need, got } => {
                write!(f, "element truncated (need {need} bytes, got {got})")
            }
            Self::LayoutMismatch => f.write_str("element layout does not match index"),
            Self::AttrTooLarge { limit, got } => {
                write!(f, "ATTR is {got} bytes, over the {limit}-byte limit")
            }
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

    /// 이 attr 길이에서 벡터가 시작하는 자리.
    ///
    /// 아이템마다 다르다 -- 그것이 가변 길이 형식의 요점이고, 그래서 읽는
    /// 쪽은 언제나 첫 바이트를 먼저 봐야 한다.
    pub const fn vector_offset(attr_len: usize) -> usize {
        ATTR_LEN_BYTES + attr_len
    }

    /// 길이 바이트, attr, 벡터.
    ///
    /// The vector is always here now. It used to be left out of a build that
    /// could not rebuild a graph from storage, because nothing would ever read
    /// it back -- but it is what arcus carries to a replica, and the trigger
    /// callback reads it straight out of the item, so it is the only copy that
    /// reaches another node.
    pub const fn element_len(&self, attr_len: usize) -> usize {
        Self::vector_offset(attr_len) + self.vector_bytes()
    }

    pub const fn stored_len(&self, attr_len: usize) -> usize {
        self.element_len(attr_len) + Self::STORED_TERMINATOR
    }

    /// attr을 끝까지 채웠을 때의 크기.
    ///
    /// 크기 한도 검사가 이것을 쓴다. 지금 attr이 짧아도 나중에 `vsetattr`이
    /// 255바이트까지 늘릴 수 있으므로, 그때 못 쓰게 되는 것보다 처음부터
    /// 넉넉히 재는 편이 낫다.
    pub const fn max_stored_len(&self) -> usize {
        self.stored_len(ATTR_BYTES)
    }

    pub const fn max_dim_for(quant: Quant, max_element_bytes: usize) -> usize {
        let overhead = Self::vector_offset(ATTR_BYTES) + Self::STORED_TERMINATOR;
        if max_element_bytes <= overhead {
            return 0;
        }
        quant.max_dim(max_element_bytes - overhead)
    }

    pub fn encode(&self, vector: &[u8], attr: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut buf = vec![0u8; self.element_len(attr.len())];
        self.write(&mut buf, vector, attr)?;
        Ok(buf)
    }

    /// 길이 바이트, attr, 벡터를 차례로 쓴다.
    ///
    /// `buf`는 정확히 `element_len(attr.len())` 이어야 한다. 남는 자리를 0으로
    /// 채우던 예전 형식과 달리, 이제 버퍼 크기가 곧 내용의 크기다.
    pub fn write(&self, buf: &mut [u8], vector: &[u8], attr: &[u8]) -> Result<(), CodecError> {
        if vector.len() != self.vector_bytes() {
            return Err(CodecError::VectorLenMismatch {
                need: self.vector_bytes(),
                got: vector.len(),
            });
        }
        if attr.len() > ATTR_BYTES {
            return Err(CodecError::AttrTooLarge {
                limit: ATTR_BYTES,
                got: attr.len(),
            });
        }
        let need = self.element_len(attr.len());
        if buf.len() != need {
            return Err(CodecError::Truncated {
                need,
                got: buf.len(),
            });
        }

        buf[0] = attr.len() as u8;
        let vector_at = Self::vector_offset(attr.len());
        buf[ATTR_OFFSET..vector_at].copy_from_slice(attr);
        buf[vector_at..].copy_from_slice(vector);
        Ok(())
    }

    pub fn decode<'a>(&self, buf: &'a [u8]) -> Result<Element<'a>, CodecError> {
        let head = parse_header(buf)?;
        let need = self.element_len(head.attr_len);
        if buf.len() < need {
            return Err(CodecError::Truncated {
                need,
                got: buf.len(),
            });
        }
        let vector_at = Self::vector_offset(head.attr_len);
        Ok(Element {
            attr: &buf[ATTR_OFFSET..vector_at],
            vector: &buf[vector_at..need],
        })
    }

    pub fn vector_of<'a>(&self, buf: &'a [u8]) -> Option<&'a [u8]> {
        let head = parse_header(buf).ok()?;
        let need = self.element_len(head.attr_len);
        let vector_at = Self::vector_offset(head.attr_len);
        (buf.len() >= need).then(|| &buf[vector_at..need])
    }

    pub fn attr_of<'a>(&self, buf: &'a [u8]) -> Result<&'a [u8], CodecError> {
        let head = parse_header(buf)?;
        let end = Self::vector_offset(head.attr_len);
        if buf.len() < end {
            return Err(CodecError::Truncated {
                need: end,
                got: buf.len(),
            });
        }
        Ok(&buf[ATTR_OFFSET..end])
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Header {
    attr_len: usize,
}

fn parse_header(buf: &[u8]) -> Result<Header, CodecError> {
    if buf.len() < ATTR_LEN_BYTES {
        return Err(CodecError::Truncated {
            need: ATTR_LEN_BYTES,
            got: buf.len(),
        });
    }
    // 한 바이트라 `ATTR_BYTES`를 넘길 수가 없다 -- 예전 2바이트 형식에 있던
    // 범위 검사가 형식 자체로 사라졌다.
    Ok(Header {
        attr_len: buf[0] as usize,
    })
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetaRecord {
    pub metric: String,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,
}

impl MetaRecord {
    pub fn encode(&self, layout: Layout) -> Vec<u8> {
        format!(
            r#"{{"dim":{},"quant":"{}","metric":"{}","m":{},"efc":{},"efs":{}}}"#,
            layout.dim,
            layout.quant,
            self.metric,
            self.connectivity,
            self.expansion_add,
            self.expansion_search,
        )
        .into_bytes()
    }

    pub fn decode(buf: &[u8]) -> Result<(Self, Layout), CodecError> {
        let json: serde_json::Value =
            serde_json::from_slice(buf).map_err(|e| CodecError::BadMetadata(e.to_string()))?;
        let miss = |k: &str| CodecError::BadMetadata(format!("missing '{k}'"));
        let num = |k: &str| -> Result<usize, CodecError> {
            json.get(k)
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as usize)
                .ok_or_else(|| miss(k))
        };

        Ok((
            Self {
                metric: json
                    .get("metric")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| miss("metric"))?
                    .to_owned(),
                connectivity: num("m")?,
                expansion_add: num("efc")?,
                expansion_search: num("efs")?,
            },
            Layout::new(
                num("dim")?,
                json.get("quant")
                    .and_then(serde_json::Value::as_str)
                    .and_then(Quant::parse)
                    .ok_or_else(|| miss("quant"))?,
            ),
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Element<'a> {
    pub attr: &'a [u8],

    pub vector: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn the_vector_offset_follows_the_attr_length() {
        assert_eq!(ATTR_OFFSET, 1);
        assert_eq!(Layout::vector_offset(0), 1);
        assert_eq!(Layout::vector_offset(ATTR_BYTES), 256);

        for dim in [1usize, 128, 4096] {
            for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                let l = Layout::new(dim, q);
                // attr이 없으면 길이 바이트 하나뿐이다. 예전 형식은 여기서도
                // 130바이트를 잡았다.
                assert_eq!(l.element_len(0), 1 + l.vector_bytes());
                assert_eq!(l.stored_len(0), 3 + l.vector_bytes());
                assert_eq!(l.element_len(7), 8 + l.vector_bytes());
                assert_eq!(l.max_stored_len(), 258 + l.vector_bytes());
            }
        }
    }

    #[test]
    fn attr_is_readable_at_a_constant_offset_for_every_layout() {
        for dim in [1usize, 128, 4096] {
            for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                let l = Layout::new(dim, q);
                let e = l.encode(&vec![0u8; l.vector_bytes()], b"{}").unwrap();
                assert_eq!(l.attr_of(&e).unwrap(), b"{}");
            }
        }
    }

    #[test]
    fn decode_accepts_an_element_with_or_without_the_terminator() {
        let l = Layout::new(4, Quant::F32);
        let vector = encode(&[1.0, 2.0, 3.0, 4.0], Quant::F32).unwrap();
        let bare = l.encode(&vector, b"{}").expect("encodes");
        assert_eq!(bare.len(), l.element_len(2));

        let mut terminated = bare.clone();
        terminated.extend_from_slice(b"\r\n");
        assert_eq!(terminated.len(), l.stored_len(2));

        let from_bare = l.decode(&bare).expect("old shape decodes");
        let from_terminated = l.decode(&terminated).expect("new shape decodes");
        assert_eq!(from_bare.attr, from_terminated.attr);
        {
            assert_eq!(from_bare.vector, from_terminated.vector);
            assert_eq!(from_bare.vector, &vector[..]);
        }

        assert!(matches!(
            l.decode(&bare[..bare.len() - 1]),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let l = layout();
        let vector = vec![1u8, 2, 3, 4];
        let attr = br#"{"cat":"tech"}"#;

        let buf = l.encode(&vector, attr).unwrap();
        assert_eq!(buf.len(), l.element_len(attr.len()));

        let e = l.decode(&buf).unwrap();
        assert_eq!(e.attr, attr);
        {
            // 길이 바이트 하나 + attr 14 + 벡터 4. 예전 형식은 attr이 얼마든
            // 130 + 4 였다.
            assert_eq!(buf.len(), 1 + attr.len() + 4);
            assert_eq!(e.vector, &vector[..]);
        }
    }

    #[test]
    fn an_absent_attr_roundtrips_as_empty() {
        let l = layout();
        let buf = l.encode(&[0, 0, 0, 0], b"").unwrap();
        assert_eq!(l.decode(&buf).unwrap().attr, b"");
        assert_eq!(l.attr_of(&buf).unwrap(), b"");
    }

    #[test]
    fn the_attr_region_is_zero_padded() {
        let l = layout();
        let buf = l.encode(&[9, 9, 9, 9], b"{}").unwrap();

        // 남는 자리가 없다. attr 바로 뒤가 곧 벡터다.
        assert_eq!(buf.len(), l.element_len(2));
        assert_eq!(&buf[Layout::vector_offset(2)..], &[9, 9, 9, 9]);
    }

    #[test]
    fn an_attr_exactly_at_the_limit_fits_and_one_over_does_not() {
        let l = layout();
        let attr = vec![b'x'; ATTR_BYTES + 1];
        assert!(l.encode(&[0, 0, 0, 0], &attr[..ATTR_BYTES]).is_ok());
        assert_eq!(
            l.encode(&[0, 0, 0, 0], &attr),
            Err(CodecError::AttrTooLarge {
                limit: ATTR_BYTES,
                got: ATTR_BYTES + 1
            })
        );
    }

    #[test]
    fn a_full_attr_region_still_decodes() {
        let l = layout();
        let attr = vec![b'x'; ATTR_BYTES];
        let buf = l.encode(&[1, 2, 3, 4], &attr).unwrap();
        assert_eq!(l.decode(&buf).unwrap().attr, &attr[..]);
        assert_eq!(l.decode(&buf).unwrap().vector, &[1, 2, 3, 4]);
    }

    #[test]
    fn wrong_vector_length_is_rejected() {
        let l = layout();
        assert_eq!(
            l.encode(&[1, 2, 3], b""),
            Err(CodecError::VectorLenMismatch { need: 4, got: 3 })
        );
    }

    #[test]
    fn metadata_round_trips_at_the_widest_values() {
        let layout = Layout::new(Layout::max_dim_for(Quant::B1, 16 * 1024), Quant::F32);
        let meta = MetaRecord {
            metric: "tanimoto".to_owned(),
            connectivity: u32::MAX as usize,
            expansion_add: u32::MAX as usize,
            expansion_search: u32::MAX as usize,
        };
        let encoded = meta.encode(layout);
        let (back, back_layout) = MetaRecord::decode(&encoded).unwrap();
        assert_eq!(back, meta);
        assert_eq!(back_layout.dim, layout.dim);
        assert_eq!(back_layout.quant, layout.quant);
    }

    #[test]
    fn an_attr_length_cannot_overflow_its_own_byte() {
        // 예전 형식은 길이가 2바이트라 영역보다 큰 값을 적을 수 있었고, 그래서
        // `LayoutMismatch` 검사가 있었다. 한 바이트로는 `ATTR_BYTES`를 넘는
        // 값을 쓸 수가 없다 -- 그 오류가 형식 자체로 사라졌다.
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();

        let mut bad = good.clone();
        bad[0] = u8::MAX;
        assert_eq!(parse_header(&bad).unwrap().attr_len, ATTR_BYTES);
        // 버퍼가 그만큼 길지 않으니 잘렸다고 답한다.
        assert!(matches!(l.attr_of(&bad), Err(CodecError::Truncated { .. })));
    }

    #[test]
    fn truncated_buffers_are_detected() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        assert_eq!(
            l.decode(&good[..good.len() - 1]),
            Err(CodecError::Truncated {
                need: l.element_len(2),
                got: l.element_len(2) - 1
            })
        );

        // 길이 바이트 하나가 머리의 전부다. 빈 버퍼만이 짧을 수 있다.
        assert_eq!(
            parse_header(&[]),
            Err(CodecError::Truncated {
                need: ATTR_LEN_BYTES,
                got: 0
            })
        );
        assert_eq!(parse_header(&good[..1]).unwrap().attr_len, 2);
    }

    #[test]
    fn attr_of_survives_a_truncated_vector_tail() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        let short = &good[..Layout::vector_offset(2)];
        assert_eq!(l.attr_of(short).unwrap(), b"{}");

        assert!(l.decode(short).is_err());
    }

    #[test]
    fn max_dim_for_is_the_exact_ceiling() {
        let limit = 16 * 1024;

        // 최악의 attr(255바이트)을 가정한 여유다.
        let overhead = Layout::vector_offset(ATTR_BYTES) + Layout::STORED_TERMINATOR;
        assert_eq!(
            Layout::max_dim_for(Quant::F32, limit),
            (limit - overhead) / 4
        );
        assert_eq!(
            Layout::max_dim_for(Quant::F16, limit),
            (limit - overhead) / 2
        );
        assert_eq!(Layout::max_dim_for(Quant::I8, limit), limit - overhead);
        assert_eq!(
            Layout::max_dim_for(Quant::B1, limit),
            (limit - overhead) * 8
        );

        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            let d = Layout::max_dim_for(q, limit);
            assert!(Layout::new(d, q).max_stored_len() <= limit, "{q:?}");
            assert!(Layout::new(d + 1, q).max_stored_len() > limit, "{q:?}");
        }
    }

    #[test]
    fn max_dim_for_handles_a_budget_below_the_fixed_overhead() {
        assert_eq!(Layout::max_dim_for(Quant::I8, 16), 0);
        assert_eq!(
            Layout::max_dim_for(Quant::I8, Layout::vector_offset(ATTR_BYTES)),
            0
        );

        assert_eq!(
            Layout::max_dim_for(
                Quant::I8,
                Layout::vector_offset(ATTR_BYTES) + Layout::STORED_TERMINATOR
            ),
            0
        );
        assert_eq!(
            Layout::max_dim_for(
                Quant::I8,
                Layout::vector_offset(ATTR_BYTES) + Layout::STORED_TERMINATOR + 1
            ),
            1
        );
    }
}

#[cfg(test)]
mod meta_tests {
    use super::*;

    fn record() -> MetaRecord {
        MetaRecord {
            metric: "l2".to_owned(),
            connectivity: 16,
            expansion_add: 128,
            expansion_search: 64,
        }
    }

    #[test]
    fn the_metadata_decodes_with_the_terminator_the_store_adds() {
        // `Store::add_meta` ends the body with CRLF because the daemon asserts
        // on it when a client `get`s the key. Decoding has to survive that or
        // an index cannot be read back.
        let layout = Layout::new(4, Quant::F32);
        let mut body = record().encode(layout);
        body.extend_from_slice(b"\r\n");

        let (meta, decoded) = MetaRecord::decode(&body).expect("the stored shape decodes");
        assert_eq!(meta.metric, "l2");
        assert_eq!(meta.connectivity, 16);
        assert_eq!(decoded.dim, 4);
    }
}
