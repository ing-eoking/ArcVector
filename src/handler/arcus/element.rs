//! Where a vector's bytes sit inside a stored element.
//!
//! Pure: no engine, no index, no server. `docs/내부구조.md` §4.

use crate::handler::quant::Quant;

const HEADER_LEN: usize = 2;

/// Field name of the reserved element holding an index's metadata.
pub const META_FIELD: &str = "AV META";

/// Constant offset of the ATTR region within an element value.
pub const ATTR_OFFSET: usize = HEADER_LEN;

/// Fixed size of the ATTR region.
pub const ATTR_BYTES: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The metadata JSON is absent, unparsable, or missing a field.
    BadMetadata(String),
    UnknownQuant(u8),
    /// Buffer shorter than the layout requires.
    Truncated {
        need: usize,
        got: usize,
    },
    /// Header disagrees with the index's declared layout.
    LayoutMismatch,
    /// Attribute JSON longer than the fixed ATTR region.
    AttrTooLarge {
        limit: usize,
        got: usize,
    },
    /// Vector byte count disagrees with `dim` and `quant`.
    VectorLenMismatch {
        need: usize,
        got: usize,
    },
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

/// Fixed geometry of every element in one index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    pub dim: usize,
    pub quant: Quant,
}

impl Layout {
    /// Where the vector begins. Constant, because ATTR has a fixed size.
    pub const VECTOR_OFFSET: usize = HEADER_LEN + ATTR_BYTES;

    pub const fn new(dim: usize, quant: Quant) -> Self {
        Self { dim, quant }
    }

    pub const fn vector_bytes(&self) -> usize {
        self.quant.vector_bytes(self.dim)
    }

    /// Bytes the engine appends to every collection element for the terminator.
    pub const STORED_TERMINATOR: usize = 2;

    pub const fn element_len(&self) -> usize {
        Self::VECTOR_OFFSET + self.vector_bytes()
    }

    /// What the engine allocates, and what must fit `max_element_bytes`.
    pub const fn stored_len(&self) -> usize {
        self.element_len() + Self::STORED_TERMINATOR
    }

    /// Largest dimension whose element fits `budget`. Zero if not even one does.
    pub const fn max_dim_for(quant: Quant, max_element_bytes: usize) -> usize {
        let overhead = Self::VECTOR_OFFSET + Self::STORED_TERMINATOR;
        if max_element_bytes <= overhead {
            return 0;
        }
        quant.max_dim(max_element_bytes - overhead)
    }

    /// `vector` must already be quantized to `self`.
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
        buf[0..2].copy_from_slice(&(attr.len() as u16).to_le_bytes());
        buf[ATTR_OFFSET..ATTR_OFFSET + attr.len()].copy_from_slice(attr);
        buf[Self::VECTOR_OFFSET..].copy_from_slice(vector);
        Ok(buf)
    }

    /// Borrow the attribute and vector regions out of a stored element value.
    ///
    /// Every offset comes from `self`; the header is read only for `alen`.
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
            vector: &buf[Self::VECTOR_OFFSET..need],
        })
    }

    /// Read only the ATTR region — the hot path used by the search predicate.
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

/// Per-index metadata, stored under [`META_FIELD`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetaRecord {
    pub metric: String,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,
    /// Identifies the graph currently built from this Map.
    pub owner: u64,
}

/// A token no other node can produce.
pub fn mint_owner() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(0x4156_0000_0000_0001);
    h.finish()
}

impl MetaRecord {
    /// Serialize into the element value, which is **entirely JSON** — no header,
    /// so neither a constant offset nor the 128-byte ATTR ceiling applies.
    pub fn encode(&self, layout: Layout) -> Vec<u8> {
        format!(
            r#"{{"dim":{},"quant":"{}","metric":"{}","m":{},"efc":{},"efs":{},"owner":"{:016x}"}}"#,
            layout.dim,
            layout.quant,
            self.metric,
            self.connectivity,
            self.expansion_add,
            self.expansion_search,
            self.owner,
        )
        .into_bytes()
    }

    /// Read one back, along with the layout it records.
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
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .ok_or_else(|| miss("owner"))?;

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

    /// The `owner` alone, for the per-command staleness check.
    pub fn owner_of(buf: &[u8]) -> Result<u64, CodecError> {
        Self::decode(buf).map(|(meta, _)| meta.owner)
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

    // -- scalar kinds and conversion ----------------------------------------

    #[test]
    fn vector_bytes_matches_scalar_width() {
        assert_eq!(Quant::F32.vector_bytes(1024), 4096);
        assert_eq!(Quant::F16.vector_bytes(1024), 2048);
        assert_eq!(Quant::I8.vector_bytes(1024), 1024);
        assert_eq!(Quant::B1.vector_bytes(1024), 128);
        // Non-multiple-of-8 dimensions round up to whole bytes.
        assert_eq!(Quant::B1.vector_bytes(1), 1);
        assert_eq!(Quant::B1.vector_bytes(9), 2);
    }

    #[test]
    fn max_dim_is_the_inverse_of_vector_bytes() {
        // 16KB element limit minus a 16B header and a 64B filter slot.
        let budget = 16 * 1024 - 16 - 64;
        assert_eq!(Quant::F32.max_dim(budget), 4076);
        assert_eq!(Quant::F16.max_dim(budget), 8152);
        assert_eq!(Quant::I8.max_dim(budget), 16304);
        assert_eq!(Quant::B1.max_dim(budget), 130432);

        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            assert!(q.vector_bytes(q.max_dim(budget)) <= budget, "{q:?}");
        }
    }

    // -- element layout -----------------------------------------------------

    fn layout() -> Layout {
        Layout::new(4, Quant::I8)
    }

    #[test]
    fn the_vector_offset_is_a_constant() {
        assert_eq!(ATTR_OFFSET, 2);
        assert_eq!(Layout::VECTOR_OFFSET, 130);

        // It must not move with dim or quant — that is the whole point.
        for dim in [1usize, 128, 4096] {
            for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                let l = Layout::new(dim, q);
                assert_eq!(l.element_len(), 130 + l.vector_bytes());
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

    /// Elements written before this module accounted for [`Layout::STORED_TERMINATOR`]
    /// are two bytes shorter, and a rebuild only gets to rewrite them if it can
    /// read them first.
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
        assert_eq!(from_bare.vector, from_terminated.vector);
        assert_eq!(from_bare.attr, from_terminated.attr);
        assert_eq!(from_bare.vector, &vector[..]);

        // One byte short of the payload is still a truncation, terminator or not.
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
        assert_eq!(buf.len(), 130 + 4);

        let e = l.decode(&buf).unwrap();
        assert_eq!(e.attr, attr);
        assert_eq!(e.vector, &vector[..]);
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
        // Only `alen` bytes are meaningful; the rest of the region must be zeroed
        // so bytes from an earlier, longer value can never leak.
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
        // The value is the whole element, so there is no fixed region to overflow.
        let layout = Layout::new(Layout::max_dim_for(Quant::B1, 16 * 1024), Quant::F32);
        let meta = MetaRecord {
            metric: "tanimoto".to_owned(),
            connectivity: u32::MAX as usize,
            expansion_add: u32::MAX as usize,
            expansion_search: u32::MAX as usize,
            owner: u64::MAX,
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
            Err(CodecError::Truncated { need: 2, got: 1 })
        );
    }

    #[test]
    fn attr_of_survives_a_truncated_vector_tail() {
        // The predicate only needs attributes, so a damaged tail must not stop it
        // from making a decision.
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        let short = &good[..Layout::VECTOR_OFFSET];
        assert_eq!(l.attr_of(short).unwrap(), b"{}");
        assert!(l.decode(short).is_err());
    }

    #[test]
    fn max_dim_for_is_the_exact_ceiling() {
        let limit = 16 * 1024;
        // 16384 - 130 header/ATTR - 2 terminator = 16252 bytes of coordinates.
        assert_eq!(Layout::max_dim_for(Quant::F32, limit), 4063);
        assert_eq!(Layout::max_dim_for(Quant::F16, limit), 8126);
        assert_eq!(Layout::max_dim_for(Quant::I8, limit), 16252);
        assert_eq!(Layout::max_dim_for(Quant::B1, limit), 130_016);

        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            let d = Layout::max_dim_for(q, limit);
            assert!(Layout::new(d, q).stored_len() <= limit, "{q:?}");
            assert!(Layout::new(d + 1, q).stored_len() > limit, "{q:?}");
        }
    }

    #[test]
    fn max_dim_for_handles_a_budget_below_the_fixed_overhead() {
        assert_eq!(Layout::max_dim_for(Quant::I8, 16), 0);
        assert_eq!(Layout::max_dim_for(Quant::I8, Layout::VECTOR_OFFSET), 0);
        // The terminator is part of the budget, so one byte past the vector
        // offset still leaves no room for a coordinate.
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
