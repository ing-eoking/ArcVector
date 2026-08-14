//! Where a vector's bytes sit inside a stored element.
//!
//! Pure: no engine, no index, no daemon. `docs/내부구조.md` §4.

pub use super::quant::{Quant, encode};

const MAGIC: [u8; 2] = *b"AV";

/// Layout version. Bumped whenever the on-element byte layout changes.
const VERSION: u8 = 2;

const HEADER_LEN: usize = 16;

/// Offset of the record type, the first of the header's reserved bytes.
const RECORD_TYPE_OFFSET: usize = 8;

/// What a record in the Map is. One framing, one magic, two kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RecordType {
    Vector = 0,
    IndexMeta = 1,
}

impl RecordType {
    const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Vector),
            1 => Some(Self::IndexMeta),
            _ => None,
        }
    }
}

/// Field name of the reserved element holding an index's metadata.
pub const META_FIELD: &str = "AV META";

/// Constant offset of the ATTR region within an element value.
pub const ATTR_OFFSET: usize = HEADER_LEN;

/// Fixed size of the ATTR region.
pub const ATTR_BYTES: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    BadMagic,
    UnsupportedVersion(u8),
    /// The record type byte is one this build does not define.
    UnknownRecordType(u8),
    /// A vector was read where metadata was expected, or the reverse.
    WrongRecordType {
        want: RecordType,
        got: RecordType,
    },
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
            Self::BadMagic => f.write_str("bad element magic"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported layout version {v}"),
            Self::UnknownRecordType(t) => write!(f, "unknown record type {t}"),
            Self::WrongRecordType { want, got } => {
                write!(f, "expected a {want:?} record, found {got:?}")
            }
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

    /// Build an element value from an already-quantized vector and attribute JSON.
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
        buf[0..2].copy_from_slice(&MAGIC);
        buf[2] = VERSION;
        buf[3] = self.quant as u8;
        buf[4..6].copy_from_slice(&(self.dim as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&(attr.len() as u16).to_le_bytes());
        // buf[8..16] stays zero: reserved.

        buf[ATTR_OFFSET..ATTR_OFFSET + attr.len()].copy_from_slice(attr);
        buf[Self::VECTOR_OFFSET..].copy_from_slice(vector);
        Ok(buf)
    }

    /// Borrow the attribute and vector regions out of a stored element value.
    pub fn decode<'a>(&self, buf: &'a [u8]) -> Result<Element<'a>, CodecError> {
        let head = parse_header(buf)?;
        if head.record != RecordType::Vector {
            return Err(CodecError::WrongRecordType {
                want: RecordType::Vector,
                got: head.record,
            });
        }
        if head.dim != self.dim || head.quant != self.quant {
            return Err(CodecError::LayoutMismatch);
        }
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
    quant: Quant,
    dim: usize,
    attr_len: usize,
    record: RecordType,
}

fn parse_header(buf: &[u8]) -> Result<Header, CodecError> {
    if buf.len() < HEADER_LEN {
        return Err(CodecError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    if buf[0..2] != MAGIC {
        return Err(CodecError::BadMagic);
    }
    if buf[2] != VERSION {
        return Err(CodecError::UnsupportedVersion(buf[2]));
    }
    let quant = Quant::from_u8(buf[3]).ok_or(CodecError::UnknownQuant(buf[3]))?;
    let record = RecordType::from_u8(buf[RECORD_TYPE_OFFSET])
        .ok_or(CodecError::UnknownRecordType(buf[RECORD_TYPE_OFFSET]))?;
    let attr_len = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    if attr_len > ATTR_BYTES {
        return Err(CodecError::LayoutMismatch);
    }
    Ok(Header {
        quant,
        dim: u16::from_le_bytes([buf[4], buf[5]]) as usize,
        attr_len,
        record,
    })
}

/// What an index is, beyond what a vector element's header already says.
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
    /// Serialize into a full element value: header, then JSON in the ATTR region.
    pub fn encode(&self, layout: Layout) -> Result<Vec<u8>, CodecError> {
        let json = format!(
            r#"{{"metric":"{}","m":{},"efc":{},"efs":{},"owner":"{:016x}"}}"#,
            self.metric, self.connectivity, self.expansion_add, self.expansion_search, self.owner,
        );
        if json.len() > ATTR_BYTES {
            return Err(CodecError::AttrTooLarge {
                limit: ATTR_BYTES,
                got: json.len(),
            });
        }

        let mut buf = vec![0u8; Layout::VECTOR_OFFSET];
        buf[0..2].copy_from_slice(&MAGIC);
        buf[2] = VERSION;
        buf[3] = layout.quant as u8;
        buf[4..6].copy_from_slice(&(layout.dim as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&(json.len() as u16).to_le_bytes());
        buf[RECORD_TYPE_OFFSET] = RecordType::IndexMeta as u8;
        buf[ATTR_OFFSET..ATTR_OFFSET + json.len()].copy_from_slice(json.as_bytes());
        Ok(buf)
    }

    /// Read one back, along with the layout its header records.
    pub fn decode(buf: &[u8]) -> Result<(Self, Layout), CodecError> {
        let head = parse_header(buf)?;
        if head.record != RecordType::IndexMeta {
            return Err(CodecError::WrongRecordType {
                want: RecordType::IndexMeta,
                got: head.record,
            });
        }
        if buf.len() < ATTR_OFFSET + head.attr_len {
            return Err(CodecError::Truncated {
                need: ATTR_OFFSET + head.attr_len,
                got: buf.len(),
            });
        }

        let json: serde_json::Value =
            serde_json::from_slice(&buf[ATTR_OFFSET..ATTR_OFFSET + head.attr_len])
                .map_err(|e| CodecError::BadMetadata(e.to_string()))?;
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
            Layout::new(head.dim, head.quant),
        ))
    }

    /// The `owner` alone, for the per-command staleness check.
    pub fn owner_of(buf: &[u8]) -> Result<u64, CodecError> {
        Self::decode(buf).map(|(meta, _)| meta.owner)
    }
}

/// Borrowed view of a decoded element.
#[derive(Debug, PartialEq, Eq)]
pub struct Element<'a> {
    pub attr: &'a [u8],
    pub vector: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(ATTR_OFFSET, 16);
        assert_eq!(Layout::VECTOR_OFFSET, 144);
        // Being 16-byte aligned keeps the vector SIMD-friendly.
        assert_eq!(Layout::VECTOR_OFFSET % 16, 0);

        // It must not move with dim or quant — that is the whole point.
        for dim in [1usize, 128, 4096] {
            for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                let l = Layout::new(dim, q);
                assert_eq!(l.element_len(), 144 + l.vector_bytes());
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

    /// Both element shapes have to read the same.
    ///
    /// arcus sizes an element to include a trailing `\r\n` (see
    /// [`Layout::STORED_TERMINATOR`]), and elements written before this module
    /// accounted for that are two bytes shorter. A rebuild rewrites them, but it
    /// only gets the chance if it can read them first, so `decode` has to accept
    /// the buffer with or without the terminator.
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
        assert_eq!(buf.len(), 144 + 4);

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
    fn corrupt_headers_are_detected() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();

        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(l.decode(&bad), Err(CodecError::BadMagic));

        let mut bad = good.clone();
        bad[2] = 99;
        assert_eq!(l.decode(&bad), Err(CodecError::UnsupportedVersion(99)));

        let mut bad = good.clone();
        bad[3] = 7;
        assert_eq!(l.decode(&bad), Err(CodecError::UnknownQuant(7)));

        // A header describing a different dimension than the index expects.
        let mut bad = good.clone();
        bad[4..6].copy_from_slice(&99u16.to_le_bytes());
        assert_eq!(l.decode(&bad), Err(CodecError::LayoutMismatch));

        // An attr length that cannot fit the fixed region.
        let mut bad = good.clone();
        bad[6..8].copy_from_slice(&(ATTR_BYTES as u16 + 1).to_le_bytes());
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
            parse_header(&good[..4]),
            Err(CodecError::Truncated { need: 16, got: 4 })
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
        // 16384 - 144 header/ATTR - 2 terminator = 16238 bytes of coordinates.
        assert_eq!(Layout::max_dim_for(Quant::F32, limit), 4059);
        assert_eq!(Layout::max_dim_for(Quant::F16, limit), 8119);
        assert_eq!(Layout::max_dim_for(Quant::I8, limit), 16238);
        assert_eq!(Layout::max_dim_for(Quant::B1, limit), 129_904);

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
