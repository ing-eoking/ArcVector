//! 결과 개수만큼 커지는 할당을, 죽는 대신 거절할 수 있게 한다.
//!
//! `Vec::reserve`나 `collect`는 자리를 못 잡으면 `handle_alloc_error`로 가고
//! 그 끝은 `abort`다 -- 오류로 돌아오지 않으므로 호출한 쪽에서 막을 방법이
//! 없다. 검색 한 건이 잡는 양은 찾은 히트 수에 비례하는데, 그 수는 결국
//! 클라이언트가 `<num>`으로 정하므로, 요청 하나가 데몬을 눕히는 자리가 된다.
//!
//! `try_reserve`는 같은 실패를 값으로 돌려준다. 여기서는 그것만 감싸, 모자라면
//! `SERVER_ERROR`가 되게 한다. [`crate::error::Error::Index`]가 그 자리다.
//!
//! **이것이 OOM을 전부 막지는 않는다.** overcommit이 켜진 리눅스 기본 설정에서
//! 할당기는 당장 못 줄 메모리도 일단 내주므로, 이 확인은 통과하고 실제로 쓸 때
//! 커널이 프로세스를 죽일 수 있다. 다만 기본 휴리스틱도 한 번에 전체 메모리를
//! 넘는 요청은 거절하므로, 한 번에 크게 잡는 이런 자리에서는 실제로 듣는다.
//! 작은 할당이 조금씩 쌓여 메모리를 먹는 쪽은 어떤 API로도 잡히지 않는다 --
//! 할당 하나하나가 성공하기 때문이다.

use crate::error::{Error, Result};

/// `extra`개를 더 담을 자리를 잡는다. 못 잡으면 `SERVER_ERROR`.
pub fn reserve<T>(v: &mut Vec<T>, extra: usize) -> Result<()> {
    v.try_reserve(extra).map_err(|_| too_big::<T>(extra))
}

/// `n`개짜리 빈 `Vec`. 못 잡으면 `SERVER_ERROR`.
pub fn vec<T>(n: usize) -> Result<Vec<T>> {
    let mut v = Vec::new();
    reserve(&mut v, n)?;
    Ok(v)
}

/// `n`바이트가 더 들어갈 자리를 문자열에 잡는다.
pub fn reserve_str(s: &mut String, extra: usize) -> Result<()> {
    s.try_reserve(extra).map_err(|_| too_big::<u8>(extra))
}

fn too_big<T>(n: usize) -> Error {
    Error::Index(format!(
        "out of memory for {n} results ({} bytes)",
        n.saturating_mul(size_of::<T>())
    ))
}
