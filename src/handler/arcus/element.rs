use crate::handler::quant::Quant;

const HEADER_LEN: usize = 2;

pub const META_FIELD: &str = "AV META";

pub const ATTR_OFFSET: usize = HEADER_LEN;

pub const ATTR_BYTES: usize = 128;

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
    pub const VECTOR_OFFSET: usize = HEADER_LEN + ATTR_BYTES;

    pub const fn new(dim: usize, quant: Quant) -> Self {
        Self { dim, quant }
    }

    pub const fn vector_bytes(&self) -> usize {
        self.quant.vector_bytes(self.dim)
    }

    pub const STORED_TERMINATOR: usize = 2;

    #[cfg(recovery)]
    pub const fn element_len(&self) -> usize {
        Self::VECTOR_OFFSET + self.vector_bytes()
    }

    #[cfg(not(recovery))]
    pub const fn element_len(&self) -> usize {
        Self::VECTOR_OFFSET
    }

    pub const fn stored_len(&self) -> usize {
        self.element_len() + Self::STORED_TERMINATOR
    }

    pub const fn full_stored_len(&self) -> usize {
        Self::VECTOR_OFFSET + self.vector_bytes() + Self::STORED_TERMINATOR
    }

    pub const fn max_dim_for(quant: Quant, max_element_bytes: usize) -> usize {
        let overhead = Self::VECTOR_OFFSET + Self::STORED_TERMINATOR;
        if max_element_bytes <= overhead {
            return 0;
        }
        quant.max_dim(max_element_bytes - overhead)
    }

    pub fn encode(&self, vector: &[u8], attr: &[u8]) -> Result<Vec<u8>, CodecError> {
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

        let mut buf = vec![0u8; self.element_len()];
        self.write(&mut buf, vector, attr)?;
        Ok(buf)
    }

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
        if buf.len() != self.element_len() {
            return Err(CodecError::Truncated {
                need: self.element_len(),
                got: buf.len(),
            });
        }
        buf[0..2].copy_from_slice(&(attr.len() as u16).to_le_bytes());

        buf[ATTR_OFFSET..Self::VECTOR_OFFSET].fill(0);
        buf[ATTR_OFFSET..ATTR_OFFSET + attr.len()].copy_from_slice(attr);
        #[cfg(recovery)]
        buf[Self::VECTOR_OFFSET..].copy_from_slice(vector);
        Ok(())
    }

    pub fn decode<'a>(&self, buf: &'a [u8]) -> Result<Element<'a>, CodecError> {
        let head = parse_header(buf)?;
        let need = self.element_len();
        if buf.len() < need {
            return Err(CodecError::Truncated {
                need,
                got: buf.len(),
            });
        }
        Ok(Element {
            attr: &buf[ATTR_OFFSET..ATTR_OFFSET + head.attr_len],
            #[cfg(recovery)]
            vector: &buf[Self::VECTOR_OFFSET..need],
        })
    }

    pub fn set_attr(&self, buf: &mut [u8], attr: &[u8]) -> Result<(), CodecError> {
        if attr.len() > ATTR_BYTES {
            return Err(CodecError::AttrTooLarge {
                limit: ATTR_BYTES,
                got: attr.len(),
            });
        }
        if buf.len() < Self::VECTOR_OFFSET {
            return Err(CodecError::Truncated {
                need: Self::VECTOR_OFFSET,
                got: buf.len(),
            });
        }
        buf[0..HEADER_LEN].copy_from_slice(&(attr.len() as u16).to_le_bytes());
        buf[ATTR_OFFSET..Self::VECTOR_OFFSET].fill(0);
        buf[ATTR_OFFSET..ATTR_OFFSET + attr.len()].copy_from_slice(attr);
        Ok(())
    }

    pub fn attr_of<'a>(&self, buf: &'a [u8]) -> Result<&'a [u8], CodecError> {
        let head = parse_header(buf)?;
        let end = ATTR_OFFSET + head.attr_len;
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
    if buf.len() < HEADER_LEN {
        return Err(CodecError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    let attr_len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if attr_len > ATTR_BYTES {
        return Err(CodecError::LayoutMismatch);
    }
    Ok(Header { attr_len })
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetaRecord {
    pub metric: String,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,

    pub owner: String,
}

impl MetaRecord {
    pub fn encode(&self, layout: Layout) -> Vec<u8> {
        format!(
            r#"{{"dim":{},"quant":"{}","metric":"{}","m":{},"efc":{},"efs":{},"owner":{}}}"#,
            layout.dim,
            layout.quant,
            self.metric,
            self.connectivity,
            self.expansion_add,
            self.expansion_search,
            serde_json::Value::from(self.owner.as_str()),
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

        let owner = json
            .get("owner")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| miss("owner"))?
            .to_owned();

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
                owner,
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

    #[cfg(recovery)]
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
    fn the_vector_offset_is_a_constant() {
        assert_eq!(ATTR_OFFSET, 2);
        assert_eq!(Layout::VECTOR_OFFSET, 130);

        for dim in [1usize, 128, 4096] {
            for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                let l = Layout::new(dim, q);
                #[cfg(recovery)]
                assert_eq!(l.element_len(), 130 + l.vector_bytes());

                #[cfg(not(recovery))]
                assert_eq!(l.element_len(), 130);
                assert_eq!(l.full_stored_len(), 132 + l.vector_bytes());
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
        let vector = encode(&[1.0, 2.0, 3.0, 4.0], Quant::F32);
        let bare = l.encode(&vector, b"{}").expect("encodes");
        assert_eq!(bare.len(), l.element_len());

        let mut terminated = bare.clone();
        terminated.extend_from_slice(b"\r\n");
        assert_eq!(terminated.len(), l.stored_len());

        let from_bare = l.decode(&bare).expect("old shape decodes");
        let from_terminated = l.decode(&terminated).expect("new shape decodes");
        assert_eq!(from_bare.attr, from_terminated.attr);
        #[cfg(recovery)]
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
        assert_eq!(buf.len(), l.element_len());

        let e = l.decode(&buf).unwrap();
        assert_eq!(e.attr, attr);
        #[cfg(recovery)]
        {
            assert_eq!(buf.len(), 130 + 4);
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

        assert!(
            buf[ATTR_OFFSET + 2..Layout::VECTOR_OFFSET]
                .iter()
                .all(|b| *b == 0)
        );
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
        #[cfg(recovery)]
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
            owner: "ffee/1/2".to_owned(),
        };
        let encoded = meta.encode(layout);
        let (back, back_layout) = MetaRecord::decode(&encoded).unwrap();
        assert_eq!(back, meta);
        assert_eq!(back_layout.dim, layout.dim);
        assert_eq!(back_layout.quant, layout.quant);
    }

    #[test]
    fn an_attr_length_over_the_region_is_rejected() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();

        let mut bad = good.clone();
        bad[0..2].copy_from_slice(&(ATTR_BYTES as u16 + 1).to_le_bytes());
        assert_eq!(l.decode(&bad), Err(CodecError::LayoutMismatch));
        assert_eq!(l.attr_of(&bad), Err(CodecError::LayoutMismatch));
    }

    #[test]
    fn truncated_buffers_are_detected() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        assert_eq!(
            l.decode(&good[..good.len() - 1]),
            Err(CodecError::Truncated {
                need: l.element_len(),
                got: l.element_len() - 1
            })
        );

        assert_eq!(
            parse_header(&good[..1]),
            Err(CodecError::Truncated {
                need: HEADER_LEN,
                got: 1
            })
        );
        assert_eq!(
            parse_header(&good[..HEADER_LEN - 1]),
            Err(CodecError::Truncated {
                need: HEADER_LEN,
                got: HEADER_LEN - 1
            })
        );
    }

    #[test]
    fn attr_of_survives_a_truncated_vector_tail() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        let short = &good[..Layout::VECTOR_OFFSET];
        assert_eq!(l.attr_of(short).unwrap(), b"{}");

        #[cfg(recovery)]
        assert!(l.decode(short).is_err());
    }

    #[test]
    fn max_dim_for_is_the_exact_ceiling() {
        let limit = 16 * 1024;

        assert_eq!(Layout::max_dim_for(Quant::F32, limit), 4063);
        assert_eq!(Layout::max_dim_for(Quant::F16, limit), 8126);
        assert_eq!(Layout::max_dim_for(Quant::I8, limit), 16252);
        assert_eq!(Layout::max_dim_for(Quant::B1, limit), 130_016);

        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            let d = Layout::max_dim_for(q, limit);
            assert!(Layout::new(d, q).full_stored_len() <= limit, "{q:?}");
            assert!(Layout::new(d + 1, q).full_stored_len() > limit, "{q:?}");
        }
    }

    #[test]
    fn max_dim_for_handles_a_budget_below_the_fixed_overhead() {
        assert_eq!(Layout::max_dim_for(Quant::I8, 16), 0);
        assert_eq!(Layout::max_dim_for(Quant::I8, Layout::VECTOR_OFFSET), 0);

        assert_eq!(
            Layout::max_dim_for(Quant::I8, Layout::VECTOR_OFFSET + Layout::STORED_TERMINATOR),
            0
        );
        assert_eq!(
            Layout::max_dim_for(
                Quant::I8,
                Layout::VECTOR_OFFSET + Layout::STORED_TERMINATOR + 1
            ),
            1
        );
    }
}
