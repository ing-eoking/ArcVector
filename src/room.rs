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

/// 바인딩이 들어가자마자 잡을 두 배열과 **같은 크기를 같은 할당기로** 미리
/// 잡아보고, 바로 놓는다. 못 잡으면 `SERVER_ERROR`.
///
/// usearch의 `Matches`는 cxx의 `rust::Vec`인데, 그 `reserve`는 C++ 쪽에
/// 구현이 있는 게 아니라 Rust의 `Vec::reserve`를 도로 불러낸다(cxx의
/// `rust_vec.rs`). 바인딩 쪽은 실패하면 abort라 그 안에서는 돌아설 수 없으므로,
/// 돌아설 수 있는 자리는 들어가기 전뿐이다.
///
/// # 이것이 막는 것과 막지 못하는 것
///
/// **막는 것은 할당 시점에 드러나는 실패뿐이다.** 용량 계산이 넘치는 터무니없는
/// 크기, `vm.overcommit_memory=2`, `RLIMIT_AS`(`ulimit -v`)처럼 할당기가 그
/// 자리에서 거절하는 환경이다. 거기서는 abort가 `SERVER_ERROR`가 된다.
///
/// **OOM으로 죽는 것은 막지 못한다.** `try_reserve_exact`가 성공했다는 것은
/// 주소공간을 받았다는 뜻이지 물리 메모리를 받았다는 뜻이 아니다. 바인딩은
/// 들어가서 그 영역을 0으로 **채우고**, 물리 페이지는 그때 붙는다. 모자라면
/// 그 자리에서 SIGKILL이고, 이 확인은 이미 통과한 뒤다. cgroup 한도도 할당이
/// 아니라 페이지를 쓸 때 과금되므로 같은 이유로 안 걸린다 -- 컨테이너에서는
/// 그쪽이 가장 흔한 사망 경로다. overcommit 기본값에서는 한때 24GB짜리 확인도
/// 통과했으니, 여기 오는 크기는 거의 언제나 통과한다고 봐야 한다.
///
/// 채워 보면 되지 않느냐 하면, 안 된다. 물리 메모리가 없으면 확인용 버퍼를
/// 채우다 죽는 것이지 덜 죽는 것이 아니다. 같은 죽음을 앞당길 뿐이다.
///
/// 그래서 이 함수를 **메모리 부족에 대한 방어로 믿으면 안 된다.** 거기에 대한
/// 방어는 프로세스 밖에 있다 -- `-m`을 머신 메모리에 맞게 잡고(그래프가 회계
/// 밖이라 RSS는 그 몇 배다), cgroup 한도와 재시작 정책을 두는 것. 결정적인
/// 동작이 필요하면 strict overcommit이나 `RLIMIT_AS`를 거는 쪽이고, 그 환경
/// 에서야 이 확인이 제 몫을 한다.
pub fn probe_pair<A, B>(n: usize) -> Result<()> {
    let mut a: Vec<A> = Vec::new();
    let mut b: Vec<B> = Vec::new();
    a.try_reserve_exact(n).map_err(|_| too_big::<A>(n))?;
    b.try_reserve_exact(n).map_err(|_| too_big::<B>(n))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 감당 못 할 크기는 abort가 아니라 `SERVER_ERROR`로 돌아온다.
    ///
    /// 이 모듈이 있는 이유 자체가 그것이다 -- `Vec::reserve`였다면 이 호출이
    /// 프로세스를 끝냈다.
    #[test]
    fn an_impossible_size_comes_back_as_an_error() {
        let huge = usize::MAX / 16;
        let err = vec::<u64>(huge).expect_err("이만한 자리가 있을 리 없다");
        assert_eq!(err.blame(), crate::error::Blame::Server);

        let mut v: Vec<(u64, f32)> = Vec::new();
        assert!(reserve(&mut v, huge).is_err());

        let mut s = String::new();
        assert!(reserve_str(&mut s, huge).is_err());
    }

    /// 바인딩에 들어가기 전 재보는 것도 마찬가지로 돌아온다.
    ///
    /// 확인하는 것은 **배선**이다 -- 할당기가 거절했을 때 abort 대신 오류가
    /// 나오는지. 메모리가 빠듯할 때 안전한지는 여기서 확인할 수 없고, 프로세스
    /// 안에서 확인할 방법도 없다. `probe_pair`의 설명을 볼 것.
    #[test]
    fn the_binding_probe_turns_a_refusal_into_an_error() {
        assert!(probe_pair::<u64, f32>(usize::MAX / 16).is_err());
    }

    /// 평범한 크기는 통과하고, 잡은 자리는 바로 놓는다.
    #[test]
    fn an_ordinary_size_passes() {
        assert!(probe_pair::<u64, f32>(50_000).is_ok());
        assert_eq!(vec::<u64>(50_000).unwrap().capacity().min(50_000), 50_000);
    }
}
