//! Map element byte layout.
//!
//! ```text
//!  off  0    2   3     4       6        8       16          16+F
//!      +----+---+-----+-------+--------+-------+-----------+---------------+
//!      |"AV"|ver|quant|dim u16|flen u16|rsvd 8B| filter F   | quantized vec |
//!      +----+---+-----+-------+--------+-------+-----------+---------------+
//! ```
//!
//! The filter slot precedes the vector so its offset is the constant
//! [`FILTER_OFFSET`], independent of `dim` and `quant`. The search predicate can
//! therefore read it without decoding anything, touching a single cache line.
//! Header (16B) plus a 16-byte-aligned `F` also keeps the vector 16-byte aligned.

use crate::quant::Quant;

pub const MAGIC: [u8; 2] = *b"AV";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 16;

/// Constant offset of the filter slot within an element value.
pub const FILTER_OFFSET: usize = HEADER_LEN;

pub const DEFAULT_FILTER_BYTES: usize = 64;
pub const MIN_FILTER_BYTES: usize = 16;
pub const MAX_FILTER_BYTES: usize = 256;
pub const FILTER_BYTES_ALIGN: usize = 16;

#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    BadMagic,
    UnsupportedVersion(u8),
    UnknownQuant(u8),
    /// Buffer shorter than the layout requires.
    Truncated { need: usize, got: usize },
    /// Header disagrees with the index's declared layout.
    LayoutMismatch,
    /// JSON longer than the fixed filter slot.
    FilterTooLarge { limit: usize, got: usize },
    /// Vector byte count disagrees with `dim` and `quant`.
    VectorLenMismatch { need: usize, got: usize },
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::BadMagic => write!(f, "bad element magic"),
            CodecError::UnsupportedVersion(v) => write!(f, "unsupported layout version {v}"),
            CodecError::UnknownQuant(q) => write!(f, "unknown quantization {q}"),
            CodecError::Truncated { need, got } => {
                write!(f, "element truncated (need {need} bytes, got {got})")
            }
            CodecError::LayoutMismatch => write!(f, "element layout does not match index"),
            CodecError::FilterTooLarge { limit, got } => {
                write!(f, "filter too large ({got} bytes, limit {limit})")
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
    pub filter_bytes: usize,
}

impl Layout {
    pub fn new(dim: usize, quant: Quant, filter_bytes: usize) -> Self {
        Layout { dim, quant, filter_bytes }
    }

    pub fn vector_bytes(&self) -> usize {
        self.quant.vector_bytes(self.dim)
    }

    pub fn vector_offset(&self) -> usize {
        HEADER_LEN + self.filter_bytes
    }

    /// Total element value size. This is what must fit in `max_element_bytes`.
    pub fn element_len(&self) -> usize {
        self.vector_offset() + self.vector_bytes()
    }

    /// Largest dimension that fits a `max_element_bytes` budget for this
    /// quantization and filter slot size. Returns 0 when even one coordinate
    /// cannot fit.
    pub fn max_dim_for(quant: Quant, filter_bytes: usize, max_element_bytes: usize) -> usize {
        let overhead = HEADER_LEN + filter_bytes;
        if max_element_bytes <= overhead {
            return 0;
        }
        quant.max_dim(max_element_bytes - overhead)
    }

    /// Validate a client-supplied filter slot size.
    pub fn validate_filter_bytes(n: usize) -> Result<usize, &'static str> {
        if n < MIN_FILTER_BYTES || n > MAX_FILTER_BYTES {
            return Err("FBYTES must be between 16 and 256");
        }
        if n % FILTER_BYTES_ALIGN != 0 {
            return Err("FBYTES must be a multiple of 16");
        }
        Ok(n)
    }

    /// Build an element value from an already-quantized vector and filter JSON.
    ///
    /// The filter slot is zero-padded; `flen` in the header records the real length.
    pub fn encode(&self, vector: &[u8], filter_json: &[u8]) -> Result<Vec<u8>, CodecError> {
        if vector.len() != self.vector_bytes() {
            return Err(CodecError::VectorLenMismatch {
                need: self.vector_bytes(),
                got: vector.len(),
            });
        }
        if filter_json.len() > self.filter_bytes {
            return Err(CodecError::FilterTooLarge {
                limit: self.filter_bytes,
                got: filter_json.len(),
            });
        }

        let mut buf = vec![0u8; self.element_len()];
        buf[0..2].copy_from_slice(&MAGIC);
        buf[2] = VERSION;
        buf[3] = self.quant as u8;
        buf[4..6].copy_from_slice(&(self.dim as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&(filter_json.len() as u16).to_le_bytes());
        // buf[8..16] stays zero: reserved.

        buf[FILTER_OFFSET..FILTER_OFFSET + filter_json.len()].copy_from_slice(filter_json);
        let vo = self.vector_offset();
        buf[vo..vo + vector.len()].copy_from_slice(vector);
        Ok(buf)
    }

    /// Borrow the filter and vector regions out of a stored element value.
    pub fn decode<'a>(&self, buf: &'a [u8]) -> Result<Element<'a>, CodecError> {
        let head = parse_header(buf)?;
        if head.dim != self.dim || head.quant != self.quant {
            return Err(CodecError::LayoutMismatch);
        }
        let need = self.element_len();
        if buf.len() < need {
            return Err(CodecError::Truncated { need, got: buf.len() });
        }
        if head.filter_len as usize > self.filter_bytes {
            return Err(CodecError::LayoutMismatch);
        }
        let vo = self.vector_offset();
        Ok(Element {
            dim: head.dim,
            quant: head.quant,
            filter: &buf[FILTER_OFFSET..FILTER_OFFSET + head.filter_len as usize],
            vector: &buf[vo..vo + self.vector_bytes()],
        })
    }

    /// Read only the filter slot — the hot path used by the search predicate.
    ///
    /// Deliberately avoids [`decode`](Self::decode): no vector bounds are needed,
    /// so a truncated tail still yields a usable filter.
    pub fn filter_of<'a>(&self, buf: &'a [u8]) -> Result<&'a [u8], CodecError> {
        let head = parse_header(buf)?;
        let flen = head.filter_len as usize;
        if flen > self.filter_bytes {
            return Err(CodecError::LayoutMismatch);
        }
        let end = FILTER_OFFSET + flen;
        if buf.len() < end {
            return Err(CodecError::Truncated { need: end, got: buf.len() });
        }
        Ok(&buf[FILTER_OFFSET..end])
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub quant: Quant,
    pub dim: usize,
    pub filter_len: u16,
}

pub fn parse_header(buf: &[u8]) -> Result<Header, CodecError> {
    if buf.len() < HEADER_LEN {
        return Err(CodecError::Truncated { need: HEADER_LEN, got: buf.len() });
    }
    if buf[0..2] != MAGIC {
        return Err(CodecError::BadMagic);
    }
    if buf[2] != VERSION {
        return Err(CodecError::UnsupportedVersion(buf[2]));
    }
    let quant = Quant::from_u8(buf[3]).ok_or(CodecError::UnknownQuant(buf[3]))?;
    Ok(Header {
        version: buf[2],
        quant,
        dim: u16::from_le_bytes([buf[4], buf[5]]) as usize,
        filter_len: u16::from_le_bytes([buf[6], buf[7]]),
    })
}

/// Borrowed view of a decoded element.
#[derive(Debug, PartialEq, Eq)]
pub struct Element<'a> {
    pub dim: usize,
    pub quant: Quant,
    pub filter: &'a [u8],
    pub vector: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new(4, Quant::I8, DEFAULT_FILTER_BYTES)
    }

    #[test]
    fn element_len_is_header_plus_filter_plus_vector() {
        let l = Layout::new(1024, Quant::I8, 64);
        assert_eq!(l.vector_offset(), 80);
        assert_eq!(l.element_len(), 16 + 64 + 1024);
        // The vector must start 16-byte aligned for SIMD-friendly access.
        assert_eq!(l.vector_offset() % 16, 0);
    }

    #[test]
    fn filter_offset_is_independent_of_dim_and_quant() {
        // The whole point of putting the filter first: one constant offset.
        for dim in [1usize, 128, 4096] {
            for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
                let l = Layout::new(dim, q, 64);
                let e = l.encode(&vec![0u8; l.vector_bytes()], b"{}").unwrap();
                assert_eq!(l.filter_of(&e).unwrap(), b"{}");
            }
        }
        assert_eq!(FILTER_OFFSET, 16);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let l = layout();
        let vector = vec![1u8, 2, 3, 4];
        let json = br#"{"cat":"tech"}"#;

        let buf = l.encode(&vector, json).unwrap();
        assert_eq!(buf.len(), l.element_len());

        let e = l.decode(&buf).unwrap();
        assert_eq!(e.dim, 4);
        assert_eq!(e.quant, Quant::I8);
        assert_eq!(e.filter, json);
        assert_eq!(e.vector, &vector[..]);
    }

    #[test]
    fn empty_filter_roundtrips() {
        let l = layout();
        let buf = l.encode(&[0, 0, 0, 0], b"").unwrap();
        assert_eq!(l.decode(&buf).unwrap().filter, b"");
        assert_eq!(l.filter_of(&buf).unwrap(), b"");
    }

    #[test]
    fn filter_slot_is_zero_padded() {
        let l = layout();
        let buf = l.encode(&[9, 9, 9, 9], b"{}").unwrap();
        // Only `flen` bytes are meaningful; the rest of the slot must be zeroed
        // so stale bytes can never leak between writes.
        assert!(buf[FILTER_OFFSET + 2..l.vector_offset()].iter().all(|b| *b == 0));
    }

    #[test]
    fn oversized_filter_is_rejected() {
        let l = layout();
        let json = vec![b'x'; DEFAULT_FILTER_BYTES + 1];
        assert_eq!(
            l.encode(&[0, 0, 0, 0], &json),
            Err(CodecError::FilterTooLarge {
                limit: DEFAULT_FILTER_BYTES,
                got: DEFAULT_FILTER_BYTES + 1
            })
        );
        // Exactly at the limit must still fit.
        assert!(l.encode(&[0, 0, 0, 0], &json[..DEFAULT_FILTER_BYTES]).is_ok());
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
    }

    #[test]
    fn truncated_buffers_are_detected() {
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        assert_eq!(
            l.decode(&good[..good.len() - 1]),
            Err(CodecError::Truncated { need: l.element_len(), got: l.element_len() - 1 })
        );
        assert!(matches!(
            parse_header(&good[..4]),
            Err(CodecError::Truncated { need: 16, got: 4 })
        ));
    }

    #[test]
    fn filter_of_survives_a_truncated_vector_tail() {
        // The predicate only needs the filter, so a damaged tail must not
        // prevent it from making a decision.
        let l = layout();
        let good = l.encode(&[0, 0, 0, 0], b"{}").unwrap();
        let short = &good[..l.vector_offset()];
        assert_eq!(l.filter_of(short).unwrap(), b"{}");
        assert!(l.decode(short).is_err());
    }

    #[test]
    fn max_dim_for_matches_the_documented_table() {
        let limit = 16 * 1024;
        assert_eq!(Layout::max_dim_for(Quant::F32, 64, limit), 4076);
        assert_eq!(Layout::max_dim_for(Quant::F16, 64, limit), 8152);
        assert_eq!(Layout::max_dim_for(Quant::I8, 64, limit), 16304);
        assert_eq!(Layout::max_dim_for(Quant::B1, 64, limit), 130432);

        // A layout built at exactly max_dim must fit the limit.
        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            let d = Layout::max_dim_for(q, 64, limit);
            assert!(Layout::new(d, q, 64).element_len() <= limit, "{q:?}");
            assert!(Layout::new(d + 1, q, 64).element_len() > limit, "{q:?}");
        }
    }

    #[test]
    fn max_dim_for_handles_a_budget_smaller_than_the_overhead() {
        assert_eq!(Layout::max_dim_for(Quant::I8, 64, 16), 0);
        assert_eq!(Layout::max_dim_for(Quant::I8, 64, 80), 0);
    }

    #[test]
    fn filter_bytes_validation() {
        assert_eq!(Layout::validate_filter_bytes(64), Ok(64));
        assert_eq!(Layout::validate_filter_bytes(16), Ok(16));
        assert_eq!(Layout::validate_filter_bytes(256), Ok(256));
        assert!(Layout::validate_filter_bytes(8).is_err());
        assert!(Layout::validate_filter_bytes(272).is_err());
        assert!(Layout::validate_filter_bytes(24).is_err());
    }
}
