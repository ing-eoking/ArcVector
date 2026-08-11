//! What a stored vector is: its scalar kind, and the bytes it becomes.
//!
//! Two halves of one decision. [`Quant`] fixes how a coordinate is represented —
//! `f32`, `f16`, `i8` or a single bit — and [`Layout`] says where those bytes sit
//! inside a Map element, after a header and a fixed-size attribute region.
//!
//! Neither half belongs to arcus or to usearch, which is why they live here rather
//! than beside either. [`crate::store`] writes these bytes into a Map element and
//! [`crate::index`] hands the very same bytes to usearch, and that is exactly why
//! rebuilding an index from the store is lossless: nothing is re-quantized.
//!
//! Pure. No engine, no index, no daemon — which is why most of the crate's test
//! coverage is here.

// ---------------------------------------------------------------------------
// Scalar kind
// ---------------------------------------------------------------------------

/// Scalar kind of a stored vector. The discriminant is persisted in the element
/// header, so values must never be renumbered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Quant {
    F32 = 0,
    F16 = 1,
    I8 = 2,
    B1 = 3,
}

impl Quant {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Quant::F32),
            1 => Some(Quant::F16),
            2 => Some(Quant::I8),
            3 => Some(Quant::B1),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "f32" => Some(Quant::F32),
            "f16" => Some(Quant::F16),
            "i8" => Some(Quant::I8),
            "b1" => Some(Quant::B1),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Quant::F32 => "f32",
            Quant::F16 => "f16",
            Quant::I8 => "i8",
            Quant::B1 => "b1",
        }
    }

    /// Bytes occupied by `dim` coordinates under this quantization.
    pub const fn vector_bytes(self, dim: usize) -> usize {
        match self {
            Quant::F32 => dim * 4,
            Quant::F16 => dim * 2,
            Quant::I8 => dim,
            Quant::B1 => dim.div_ceil(8),
        }
    }

    /// Largest `dim` whose vector fits in `budget` bytes.
    pub const fn max_dim(self, budget: usize) -> usize {
        match self {
            Quant::F32 => budget / 4,
            Quant::F16 => budget / 2,
            Quant::I8 => budget,
            Quant::B1 => budget * 8,
        }
    }
}

impl std::fmt::Display for Quant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Convert `f32` coordinates into the on-wire bytes for `quant`.
///
/// `I8` L2-normalizes first, so distances live in the normalized space — this is
/// why `vcreate` restricts `i8` to cosine-like metrics.
pub fn encode(v: &[f32], quant: Quant) -> Vec<u8> {
    match quant {
        Quant::F32 => {
            let mut out = Vec::with_capacity(v.len() * 4);
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
            out
        }
        Quant::F16 => {
            let mut out = Vec::with_capacity(v.len() * 2);
            for x in v {
                out.extend_from_slice(&f32_to_f16_bits(*x).to_le_bytes());
            }
            out
        }
        Quant::I8 => {
            let scale = l2_norm(v);
            let inv = if scale > 0.0 { 1.0 / scale } else { 0.0 };
            v.iter()
                .map(|x| {
                    let n = (x * inv * 127.0).round();
                    (n.clamp(-127.0, 127.0) as i8).cast_unsigned()
                })
                .collect()
        }
        Quant::B1 => {
            // LSB-first within each byte, matching usearch's `b1x8` bit addressing.
            let mut out = vec![0u8; v.len().div_ceil(8)];
            for (i, x) in v.iter().enumerate() {
                if *x > 0.0 {
                    out[i / 8] |= 1 << (i % 8);
                }
            }
            out
        }
    }
}

fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// IEEE 754 binary16 encoding, returned as the raw bits usearch expects in its
/// `i16` container. Handles subnormals, overflow-to-infinity and NaN.
pub fn f32_to_f16_bits(x: f32) -> i16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;

    if exp == 0xff {
        // Inf or NaN. Preserve NaN-ness by forcing a non-zero mantissa.
        let m = if mant != 0 { 0x0200 } else { 0 };
        return (sign | 0x7c00 | m).cast_signed();
    }

    // Rebase exponent: f32 bias 127 -> f16 bias 15.
    let new_exp = exp - 127 + 15;

    if new_exp >= 0x1f {
        return (sign | 0x7c00).cast_signed(); // overflow -> infinity
    }

    if new_exp <= 0 {
        // Subnormal, or too small to represent at all.
        if new_exp < -10 {
            return sign.cast_signed();
        }
        let mant_with_implicit = mant | 0x0080_0000;
        let shift = (14 - new_exp) as u32;
        let mut half = (mant_with_implicit >> shift) as u16;
        // Round to nearest, ties away from zero.
        if (mant_with_implicit >> (shift - 1)) & 1 == 1 {
            half += 1;
        }
        return (sign | half).cast_signed();
    }

    let mut half = (sign as u32) | ((new_exp as u32) << 10) | (mant >> 13);
    if (mant >> 12) & 1 == 1 {
        half += 1; // carries into the exponent naturally
    }
    (half as u16).cast_signed()
}

/// Inverse of [`f32_to_f16_bits`]. Only the tests need it today — nothing in the
/// command path decodes f16 back to f32 — so it is not part of the shipped library.
#[cfg(test)]
pub fn f16_bits_to_f32(bits: i16) -> f32 {
    let h = bits.cast_unsigned();
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x03ff) as u32;

    if exp == 0 {
        if mant == 0 {
            return f32::from_bits(sign);
        }
        // Subnormal: renormalize into f32's range.
        let mut e = -1i32;
        let mut m = mant;
        while m & 0x0400 == 0 {
            m <<= 1;
            e -= 1;
        }
        m &= 0x03ff;
        let new_exp = (e + 1 - 15 + 127) as u32;
        return f32::from_bits(sign | (new_exp << 23) | (m << 13));
    }
    if exp == 0x1f {
        return f32::from_bits(sign | 0x7f80_0000 | (mant << 13));
    }
    // Signed arithmetic: exp < 15 for every value below 1.0, and u32 would wrap.
    let new_exp = (exp as i32 - 15 + 127) as u32;
    f32::from_bits(sign | (new_exp << 23) | (mant << 13))
}

// ---------------------------------------------------------------------------
// Element layout
// ---------------------------------------------------------------------------

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

    #[test]
    fn f32_encoding_is_little_endian_roundtrip() {
        let v = [1.0f32, -2.5, 0.0];
        let bytes = encode(&v, Quant::F32);
        assert_eq!(bytes.len(), 12);
        let back: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(back, v);
    }

    #[test]
    fn f16_roundtrip_preserves_exactly_representable_values() {
        for x in [0.0f32, 1.0, -1.0, 0.5, -2.5, 65504.0, -65504.0] {
            let back = f16_bits_to_f32(f32_to_f16_bits(x));
            assert_eq!(back, x, "value {x}");
        }
    }

    #[test]
    fn f16_handles_specials_and_overflow() {
        assert!(f16_bits_to_f32(f32_to_f16_bits(f32::NAN)).is_nan());
        assert_eq!(
            f16_bits_to_f32(f32_to_f16_bits(f32::INFINITY)),
            f32::INFINITY
        );
        // Beyond f16's max finite value, so it must saturate to infinity.
        assert_eq!(f16_bits_to_f32(f32_to_f16_bits(1.0e30)), f32::INFINITY);
        // Far below f16's smallest subnormal, so it must flush to zero.
        assert_eq!(f16_bits_to_f32(f32_to_f16_bits(1.0e-30)), 0.0);
    }

    #[test]
    fn f16_roundtrip_stays_within_half_precision_error() {
        for i in 0..2000 {
            let x = (i as f32 - 1000.0) / 97.0;
            let back = f16_bits_to_f32(f32_to_f16_bits(x));
            let err = (back - x).abs();
            assert!(err <= x.abs() * 1e-3 + 1e-6, "x={x} back={back}");
        }
    }

    #[test]
    fn i8_normalizes_before_scaling() {
        // A unit vector along one axis maps that axis to full scale.
        let bytes = encode(&[1.0, 0.0, 0.0], Quant::I8);
        assert_eq!(bytes[0] as i8, 127);
        assert_eq!(bytes[1] as i8, 0);

        // Magnitude is discarded: scaling the input must not change the output.
        let a = encode(&[3.0, 4.0], Quant::I8);
        let b = encode(&[30.0, 40.0], Quant::I8);
        assert_eq!(a, b);
    }

    #[test]
    fn i8_zero_vector_does_not_divide_by_zero() {
        let bytes = encode(&[0.0, 0.0, 0.0], Quant::I8);
        assert_eq!(bytes, vec![0u8; 3]);
    }

    #[test]
    fn b1_packs_lsb_first() {
        // Bit i lives at byte i/8, bit position i%8 — usearch's b1x8 convention.
        let bytes = encode(&[1.0, -1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0], Quant::B1);
        assert_eq!(bytes.len(), 2);
        assert_eq!(bytes[0], 0b0000_0101);
        assert_eq!(bytes[1], 0b0000_0001);
    }

    #[test]
    fn quant_names_roundtrip() {
        for q in [Quant::F32, Quant::F16, Quant::I8, Quant::B1] {
            assert_eq!(Quant::parse(q.as_str()), Some(q));
            assert_eq!(Quant::from_u8(q as u8), Some(q));
        }
        assert_eq!(Quant::parse("f64"), None);
        assert_eq!(Quant::from_u8(4), None);
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
