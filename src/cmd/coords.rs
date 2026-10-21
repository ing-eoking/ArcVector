use crate::engine::element::CodecError;
use crate::error::{Error, Result};
use crate::protocol::request::Format;

fn numbers(text: &[u8], what: &str) -> Result<Vec<f32>> {
    let text = std::str::from_utf8(text)
        .map_err(|_| Error::bad_request(format!("{what} is not valid UTF-8")))?;
    // Not `collect`: for an iterator of unknown size it grows by doubling, and that growth
    // aborts on failure. Each token is at least one character plus a separator, so half the
    // body length is always enough.
    let mut out = crate::fallible::vec(text.len().div_ceil(2))?;
    for token in text.split_ascii_whitespace() {
        let x = token.parse::<f32>().map_err(|_| {
            Error::bad_request(format!("{what} coordinate '{token}' is not a number"))
        })?;
        // Cannot exceed the reservation, but keeps `push` from aborting even if it did.
        crate::fallible::reserve(&mut out, 1)?;
        out.push(x);
    }
    Ok(out)
}

/// Parses the body as one `dim`-dimensional vector.
///
/// `bin` is `dim` little-endian f32s (`dim * 4` bytes); `str` is `dim` whitespace-separated
/// decimals. Quantization is left to the caller.
pub(super) fn vector(body: &[u8], dim: usize, format: Format) -> Result<Vec<f32>> {
    if dim == 0 {
        return Err(Error::bad_request("dimension must be at least 1"));
    }
    let out = match format {
        Format::Bin => {
            let need = dim.saturating_mul(4);
            if body.len() != need {
                return Err(CodecError::VectorLenMismatch {
                    need,
                    got: body.len(),
                }
                .into());
            }
            let mut out = crate::fallible::vec(dim)?;
            out.extend(
                body.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );
            out
        }
        Format::Str => {
            let out = numbers(body, "vector")?;
            if out.len() != dim {
                return Err(Error::bad_request(format!(
                    "expected {dim} coordinates, got {}",
                    out.len()
                )));
            }
            out
        }
    };
    if !out.iter().all(|x| x.is_finite()) {
        return Err(Error::bad_request("vector contains NaN or infinity"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_vectors_are_little_endian_f32() {
        let mut body = Vec::new();
        for x in [1.0f32, -2.5, 300.0] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        assert_eq!(
            vector(&body, 3, Format::Bin).unwrap(),
            vec![1.0, -2.5, 300.0]
        );
        // The first byte is the least significant: 1.0f32 = 00 00 80 3f.
        assert_eq!(
            vector(&[0, 0, 0x80, 0x3f], 1, Format::Bin).unwrap(),
            vec![1.0]
        );
    }

    #[test]
    fn a_bin_vector_of_the_wrong_length_names_both_lengths() {
        let err = vector(&[0u8; 7], 2, Format::Bin).unwrap_err();
        assert_eq!(
            err.to_string(),
            "vector length mismatch (need 8 bytes, got 7)"
        );
        assert_eq!(err.blame(), crate::error::Blame::Client);
        assert!(vector(&[0u8; 12], 2, Format::Bin).is_err());
    }

    #[test]
    fn a_bin_vector_with_nan_or_infinity_is_rejected() {
        let body = f32::NAN.to_le_bytes();
        let msg = vector(&body, 1, Format::Bin).unwrap_err().to_string();
        assert!(msg.contains("NaN or infinity"), "{msg}");
    }

    #[test]
    fn str_vectors_need_exactly_dim_numbers() {
        assert_eq!(
            vector(b"0.1  -2.5\t3e2", 3, Format::Str).unwrap(),
            vec![0.1, -2.5, 300.0]
        );
        let msg = vector(b"0.1 0.2 0.3", 2, Format::Str)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("expected 2 coordinates, got 3"), "{msg}");
        let msg = vector(b"0.1", 2, Format::Str).unwrap_err().to_string();
        assert!(msg.contains("expected 2 coordinates, got 1"), "{msg}");
        assert!(vector(b"", 2, Format::Str).is_err());
    }

    #[test]
    fn a_non_numeric_coordinate_is_quoted_back() {
        let msg = vector(b"0.1 abc", 2, Format::Str).unwrap_err().to_string();
        assert!(msg.contains("'abc' is not a number"), "{msg}");
    }

    #[test]
    fn non_finite_str_coordinates_are_rejected() {
        for bad in ["NaN", "inf", "-inf"] {
            let text = format!("0.1 {bad}");
            let msg = vector(text.as_bytes(), 2, Format::Str)
                .unwrap_err()
                .to_string();
            assert!(msg.contains("NaN or infinity"), "{bad}: {msg}");
        }
    }

    #[test]
    fn a_zero_dimension_does_not_divide_by_zero() {
        assert!(vector(b"1 2", 0, Format::Str).is_err());
        assert!(vector(b"", 0, Format::Bin).is_err());
    }
}
