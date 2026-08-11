//! Map element byte layout.
//!
//! ```text
//!  off  0    2   3     4       6        8       16              144
//!      +----+---+-----+-------+--------+-------+---------------+---------------+
//!      |"AV"|ver|quant|dim u16|alen u16|rsvd 8B| ATTR 128B     | quantized vec |
//!      +----+---+-----+-------+--------+-------+---------------+---------------+
//! ```
//!
//! The ATTR region is a **fixed 128 bytes** regardless of how much JSON a vector
//! actually carries, so the vector always starts at [`Layout::VECTOR_OFFSET`] and
//! the search predicate can read attributes from the constant [`ATTR_OFFSET`]
//! without decoding anything — one or two cache lines, no arithmetic.
//!
//! Why 128 bytes: a realistic attribute object
//! (`{"category":"tech","lang":"ko","ts":1723248000,"score":0.87}`) is 62 bytes,
//! so 128 leaves room for roughly twice that. It also lands better in arcus's
//! slab classes than a smaller slot would — at 1024 dimensions with `i8`,
//! `16 + 128 + 1024 = 1168` fits the 1184-byte class with 1.4% waste, where a
//! 64-byte region would give 1104 bytes and waste 6.8% in that same class. The
//! per-vector cost is 128 MB per million vectors, against 1 GB for the `i8`
//! vectors themselves.

use super::quant::Quant;

const MAGIC: [u8; 2] = *b"AV";

/// Layout version. Bumped whenever the on-element byte layout changes.
const VERSION: u8 = 2;

const HEADER_LEN: usize = 16;

/// Constant offset of the ATTR region within an element value.
pub const ATTR_OFFSET: usize = HEADER_LEN;

/// Fixed size of the ATTR region. Attribute JSON larger than this is rejected at
/// `vadd` rather than being allowed to spill into the vector.
pub const ATTR_BYTES: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    BadMagic,
    UnsupportedVersion(u8),
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
            CodecError::BadMagic => f.write_str("bad element magic"),
            CodecError::UnsupportedVersion(v) => write!(f, "unsupported layout version {v}"),
            CodecError::UnknownQuant(q) => write!(f, "unknown quantization {q}"),
            CodecError::Truncated { need, got } => {
                write!(f, "element truncated (need {need} bytes, got {got})")
            }
            CodecError::LayoutMismatch => f.write_str("element layout does not match index"),
            CodecError::AttrTooLarge { limit, got } => {
                write!(f, "ATTR is {got} bytes, over the {limit}-byte limit")
            }
            CodecError::VectorLenMismatch { need, got } => {
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
        Layout { dim, quant }
    }

    pub const fn vector_bytes(&self) -> usize {
        self.quant.vector_bytes(self.dim)
    }

    /// Total element value size. This is what must fit `max_element_bytes`.
    pub const fn element_len(&self) -> usize {
        Self::VECTOR_OFFSET + self.vector_bytes()
    }

    /// Largest dimension whose element fits `max_element_bytes`. Zero when not
    /// even one coordinate fits.
    pub const fn max_dim_for(quant: Quant, max_element_bytes: usize) -> usize {
        if max_element_bytes <= Self::VECTOR_OFFSET {
            return 0;
        }
        quant.max_dim(max_element_bytes - Self::VECTOR_OFFSET)
    }

    /// Build an element value from an already-quantized vector and attribute JSON.
    ///
    /// The ATTR region is zero-padded; `alen` in the header records the real
    /// length so trailing zeros are never mistaken for content.
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
    ///
    /// Deliberately avoids [`decode`](Self::decode): no vector bounds are needed,
    /// so a truncated tail still yields usable attributes.
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
    let attr_len = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    if attr_len > ATTR_BYTES {
        return Err(CodecError::LayoutMismatch);
    }
    Ok(Header {
        quant,
        dim: u16::from_le_bytes([buf[4], buf[5]]) as usize,
        attr_len,
    })
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
        assert_eq!(Layout::max_dim_for(Quant::F32, limit), 4060);
        assert_eq!(Layout::max_dim_for(Quant::F16, limit), 8120);
        assert_eq!(Layout::max_dim_for(Quant::I8, limit), 16240);
        assert_eq!(Layout::max_dim_for(Quant::B1, limit), 129_920);

        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            let d = Layout::max_dim_for(q, limit);
            assert!(Layout::new(d, q).element_len() <= limit, "{q:?}");
            assert!(Layout::new(d + 1, q).element_len() > limit, "{q:?}");
        }
    }

    #[test]
    fn max_dim_for_handles_a_budget_below_the_fixed_overhead() {
        assert_eq!(Layout::max_dim_for(Quant::I8, 16), 0);
        assert_eq!(Layout::max_dim_for(Quant::I8, Layout::VECTOR_OFFSET), 0);
        assert_eq!(Layout::max_dim_for(Quant::I8, Layout::VECTOR_OFFSET + 1), 1);
    }
}
