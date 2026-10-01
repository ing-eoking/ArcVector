use crate::error::{Error, Result};

pub(super) fn numbers(text: &[u8], what: &str) -> Result<Vec<f32>> {
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

pub(super) fn coord_vectors(text: &[u8], dim: usize, what: &str) -> Result<Vec<Vec<f32>>> {
    if dim == 0 {
        return Err(Error::bad_request("dimension must be at least 1"));
    }
    let all = numbers(text, what)?;
    if all.is_empty() || all.len() % dim != 0 {
        return Err(Error::bad_request(format!(
            "{} coordinates is not a whole number of {dim}-dimension vectors",
            all.len()
        )));
    }
    if !all.iter().all(|x| x.is_finite()) {
        return Err(Error::bad_request(format!(
            "{what} contains NaN or infinity"
        )));
    }
    let mut out = crate::room::vec(all.len() / dim)?;
    for chunk in all.chunks(dim) {
        let mut one = crate::room::vec(dim)?;
        one.extend_from_slice(chunk);
        out.push(one);
    }
    Ok(out)
}

pub(super) fn coords(text: &[u8], dim: usize, what: &str) -> Result<Vec<f32>> {
    let mut all = coord_vectors(text, dim, what)?;
    if all.len() != 1 {
        return Err(Error::bad_request(format!(
            "expected {dim} coordinates, got {}",
            all.len() * dim
        )));
    }
    Ok(all.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_are_parsed_from_text() {
        assert_eq!(coords(b"0.1 0.2", 2, "vector").unwrap(), vec![0.1, 0.2]);
        assert_eq!(b"0.1 0.2".len(), 7);

        assert_eq!(
            coords(b"1  -2.5\t3e2", 3, "vector").unwrap(),
            vec![1.0, -2.5, 300.0]
        );
    }

    #[test]
    fn a_coordinate_count_that_disagrees_with_the_dimension_is_named() {
        let msg = coords(b"0.1 0.2 0.3", 2, "vector").unwrap_err().to_string();
        assert!(msg.contains("whole number of 2-dimension"), "{msg}");

        let msg = coords(b"0.1", 2, "vector").unwrap_err().to_string();
        assert!(msg.contains("whole number of 2-dimension"), "{msg}");

        assert!(coords(b"", 2, "vector").is_err());
    }

    #[test]
    fn a_non_numeric_coordinate_is_quoted_back() {
        let msg = coords(b"0.1 abc", 2, "vector").unwrap_err().to_string();
        assert!(msg.contains("'abc' is not a number"), "{msg}");
    }

    #[test]
    fn non_finite_coordinates_are_rejected() {
        for bad in ["NaN", "inf", "-inf"] {
            let text = format!("0.1 {bad}");
            let msg = coords(text.as_bytes(), 2, "query").unwrap_err().to_string();
            assert!(msg.contains("NaN or infinity"), "{bad}: {msg}");
        }
    }

    #[test]
    fn a_batch_splits_into_whole_vectors() {
        let batch = coord_vectors(b"1 2 3 4 5 6", 3, "query").unwrap();
        assert_eq!(batch, vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);

        let msg = coord_vectors(b"1 2 3 4", 3, "query")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("whole number of 3-dimension"), "{msg}");
    }

    #[test]
    fn a_zero_dimension_does_not_divide_by_zero() {
        assert!(coord_vectors(b"1 2", 0, "query").is_err());
    }
}
