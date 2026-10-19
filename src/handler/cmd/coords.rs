use crate::command::request::Format;
use crate::error::{Error, Result};
use crate::handler::arcus::element::CodecError;

fn numbers(text: &[u8], what: &str) -> Result<Vec<f32>> {
    let text = std::str::from_utf8(text)
        .map_err(|_| Error::bad_request(format!("{what} is not valid UTF-8")))?;
    // `collect`로 모으지 않는다 -- 크기를 모르는 반복자라 두 배씩 늘려가며
    // 잡는데, 그 늘림은 못 잡으면 abort다. 토큰 하나가 최소 한 글자에 구분자
    // 하나이므로 본문 길이의 절반이면 넉넉히 담긴다.
    let mut out = crate::room::vec(text.len().div_ceil(2))?;
    for token in text.split_ascii_whitespace() {
        let x = token.parse::<f32>().map_err(|_| {
            Error::bad_request(format!("{what} coordinate '{token}' is not a number"))
        })?;
        // 미리 잡은 자리를 넘을 수 없지만, 넘더라도 `push`가 abort하지 않도록.
        crate::room::reserve(&mut out, 1)?;
        out.push(x);
    }
    Ok(out)
}

/// 본문을 `dim`차원 벡터 하나로 읽는다.
///
/// `bin`은 little-endian f32 `dim`개(`dim * 4`바이트), `str`은 공백으로 구분한
/// 10진수 `dim`개다. 양자화는 부른 쪽이 한다.
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
            let mut out = crate::room::vec(dim)?;
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
        // 첫 바이트가 낮은 자리다: 1.0f32 = 00 00 80 3f.
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
