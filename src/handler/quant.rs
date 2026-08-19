//! How one coordinate is represented, and the bytes it becomes.
//!
//! Discriminants are persisted in the element header, so they are part of the
//! on-disk format. Pure: no engine, no index, no server.

/// Scalar kind of a stored vector. Persisted in the header — never renumber.
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
            0 => Some(Self::F32),
            1 => Some(Self::F16),
            2 => Some(Self::I8),
            3 => Some(Self::B1),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "f32" => Some(Self::F32),
            "f16" => Some(Self::F16),
            "i8" => Some(Self::I8),
            "b1" => Some(Self::B1),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F16 => "f16",
            Self::I8 => "i8",
            Self::B1 => "b1",
        }
    }

    pub const fn vector_bytes(self, dim: usize) -> usize {
        match self {
            Self::F32 => dim * 4,
            Self::F16 => dim * 2,
            Self::I8 => dim,
            Self::B1 => dim.div_ceil(8),
        }
    }

    pub const fn max_dim(self, budget: usize) -> usize {
        match self {
            Self::F32 => budget / 4,
            Self::F16 => budget / 2,
            Self::I8 => budget,
            Self::B1 => budget * 8,
        }
    }
}

impl std::fmt::Display for Quant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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

/// IEEE 754 binary16, as the raw bits usearch wants in its `i16` container.
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

/// Inverse of [`f32_to_f16_bits`]. Tests only.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
