mod layout;
mod parse;

pub use parse::parse;

use super::filter::Filter;
use crate::Metric;
use crate::Quant;
use crate::error::{Error, Result};

/// 본문 전체(구간마다 `\r\n` 포함)의 상한. 요소 최대 크기와 필터 최대
/// 4096바이트를 넉넉히 덮는 값이다.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// `filter` 구간의 최대 바이트.
pub const MAX_FILTER_BYTES: usize = 4096;

/// 프로토콜이 결과 개수로 받는 최대값.
///
/// 문서가 말하는 범위 그대로이고, 여기서 정책으로 더 깎지 않는다. 몇 개까지
/// 받을지는 클라이언트가 정할 일이고, 큰 값이 와도 검색은 **인덱스가 담은 노드
/// 수까지만** 묻기 때문이다(`Shards::merged`) -- 담은 것보다 많이 답할 수는
/// 없으니 결과는 같고, 쓰는 메모리는 요청한 숫자가 아니라 가진 데이터에
/// 비례한다. 그래서 이 숫자는 파싱 범위일 뿐 할당 상한이 아니다.
pub const MAX_RESULTS: usize = i32::MAX as usize;

#[derive(Debug)]
pub enum Parsed<'a> {
    Line(Line<'a>),

    Body { len: usize, request: Result<Body> },
}

pub fn body_refused(len: usize) -> Error {
    if len > MAX_BODY_BYTES {
        Error::bad_request(format!(
            "vector length {len} exceeds the {MAX_BODY_BYTES}-byte transfer limit"
        ))
    } else {
        Error::bad_request("lost command state")
    }
}

/// 이 확장이 가로채는 명령은 `vop` 하나이고, 일은 하위 명령이 나눈다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Vop,
}

impl Cmd {
    pub fn parse(name: &str) -> Option<Self> {
        name.eq_ignore_ascii_case("vop").then_some(Cmd::Vop)
    }
}

/// `vop <sub> ...`의 하위 명령(토큰 1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sub {
    Create,
    Upsert,
    SetMeta,
    Search,
    GetMeta,
    Delete,
    Drop,
}

impl Sub {
    pub fn parse(name: &str) -> Option<Self> {
        const NAMES: [(&str, Sub); 7] = [
            ("create", Sub::Create),
            ("upsert", Sub::Upsert),
            ("setmeta", Sub::SetMeta),
            ("search", Sub::Search),
            ("getmeta", Sub::GetMeta),
            ("delete", Sub::Delete),
            ("drop", Sub::Drop),
        ];
        NAMES
            .iter()
            .find(|(text, _)| name.eq_ignore_ascii_case(text))
            .map(|(_, sub)| *sub)
    }
}

/// 벡터 본문의 형식. `bin`은 little-endian f32, `str`은 공백으로 구분한 10진수.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Bin,
    Str,
}

/// 본문을 이루는 구간들의 바이트 수(각 구간 뒤 `\r\n`은 빼고).
///
/// 구간 순서는 meta, filter, vector다. meta와 filter가 함께 오는 명령은 없다.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Segments {
    pub meta: Option<usize>,
    pub filter: Option<usize>,
    pub vector: Option<usize>,
}

/// [`Segments::split`]이 자른 구간들. 끝의 `\r\n`은 들어 있지 않다.
#[derive(Debug, PartialEq, Eq)]
pub struct Parts<'b> {
    pub meta: Option<&'b [u8]>,
    pub filter: Option<&'b [u8]>,
    pub vector: Option<&'b [u8]>,
}

impl Segments {
    /// 본문 전체 바이트 수. 구간마다 `\r\n` 2바이트가 붙는다.
    ///
    /// 클라이언트가 준 숫자라 넘칠 수 있다. 포화시키면 `MAX_BODY_BYTES`에서
    /// 걸린다.
    pub fn total(&self) -> usize {
        [self.meta, self.filter, self.vector]
            .into_iter()
            .flatten()
            .fold(0usize, |sum, n| sum.saturating_add(n).saturating_add(2))
    }

    /// 한 덩어리로 받은 본문을 선언된 길이대로 자르고 구간마다 끝의
    /// `\r\n`을 확인한다. 어긋나면 `bad data chunk`.
    pub fn split<'b>(&self, body: &'b [u8]) -> Result<Parts<'b>> {
        let bad = || Error::bad_request("bad data chunk");
        if body.len() != self.total() {
            return Err(bad());
        }
        let mut rest = body;
        let mut take = |len: Option<usize>| -> Result<Option<&'b [u8]>> {
            let Some(len) = len else { return Ok(None) };
            let (segment, tail) = rest.split_at(len + 2);
            rest = tail;
            let (data, crlf) = segment.split_at(len);
            if crlf != b"\r\n" {
                return Err(bad());
            }
            Ok(Some(data))
        };
        Ok(Parts {
            meta: take(self.meta)?,
            filter: take(self.filter)?,
            vector: take(self.vector)?,
        })
    }
}

/// 예전 `vsearch`의 출처. 파서는 더 만들지 않고 Task 8이 지운다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimSource {
    Vector,
    Key,
}

impl SimSource {
    pub fn parse(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("VECTOR") {
            Some(Self::Vector)
        } else if name.eq_ignore_ascii_case("KEY") {
            Some(Self::Key)
        } else {
            None
        }
    }
}

/// 예전 `vcreate`의 요청. 파서는 더 만들지 않고 Task 8이 지운다.
#[derive(Clone, Copy, Debug)]
pub struct OldCreate<'a> {
    pub index: &'a str,
    pub dim: usize,
    pub metric: Metric,
    pub quant: Quant,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,
    /// How many vectors the index takes. `None` is the server's `max_map_size`.
    pub maxcount: Option<u32>,
    /// Seconds until the whole index expires. `None` never does.
    pub exptime: Option<u32>,
}

/// 예전 명령의 요청. Task 8이 지운다.
#[derive(Debug)]
pub struct SimKey<'a> {
    pub index: &'a str,
    pub key: &'a str,
    pub k: usize,
    pub filter: Option<Filter>,
    pub with_attr: bool,
}

/// `vop create <index> <dim> <expiretime> [<maxcount>] [metric m] [quant q] [M n] [efc c] [efs s]`
///
/// `maxcount`가 `None`이면 핸들러가 기본값 4000을 쓴다.
#[derive(Clone, Copy, Debug)]
pub struct Create<'a> {
    pub index: &'a str,
    pub dim: usize,
    pub exptime: u32,
    pub maxcount: Option<u32>,
    pub metric: Metric,
    pub quant: Quant,
    pub m: usize,
    pub efc: usize,
    pub efs: usize,
}

pub const DEFAULT_M: usize = 16;
pub const DEFAULT_EFC: usize = 128;
pub const DEFAULT_EFS: usize = 64;

/// 본문이 있는 `vop` 명령은 본문 구간의 크기를 함께 들고 있다(`execute`가 자른다).
#[derive(Debug)]
pub struct Upsert {
    pub index: String,
    pub vkey: String,
    pub format: Format,
    pub has_meta: bool,
    pub segments: Segments,
}

#[derive(Debug)]
pub struct SetMeta {
    pub index: String,
    pub vkey: String,
    pub segments: Segments,
}

/// `vop search <index> vkey <vkey> <num> [filter <f>] [withmeta]`
#[derive(Debug)]
pub struct SearchKey {
    pub index: String,
    pub vkey: String,
    pub k: usize,
    pub with_meta: bool,
    pub has_filter: bool,
    pub segments: Segments,
}

/// `vop search <index> vector <num> <vector_bytes> [filter <f>] [withmeta] [bin|str]`
#[derive(Debug)]
pub struct SearchVector {
    pub index: String,
    pub k: usize,
    pub format: Format,
    pub with_meta: bool,
    pub has_filter: bool,
    pub segments: Segments,
}

#[derive(Clone, Copy, Debug)]
pub struct GetMeta<'a> {
    pub index: &'a str,
    pub vkey: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct Delete<'a> {
    pub index: &'a str,
    pub vkey: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct Drop<'a> {
    pub index: &'a str,
}

/// 본문이 없는 명령. `OldCreate`부터 `Stats`까지는 예전 명령의 것으로,
/// 파서가 더 만들지 않고 Task 8이 지운다.
#[derive(Debug)]
pub enum Line<'a> {
    Create(Create<'a>),
    /// filter 본문이 없는 vkey 검색.
    SearchKey(SearchKey),
    GetMeta(GetMeta<'a>),
    Delete(Delete<'a>),
    Drop(Drop<'a>),

    OldCreate(OldCreate<'a>),
    SimKey(SimKey<'a>),
    GetAttr {
        index: &'a str,
        id: &'a str,
    },
    SetAttr {
        index: &'a str,
        id: &'a str,
        attr: &'a [u8],
    },
    Del {
        index: &'a str,
        id: &'a str,
    },
    OldDrop {
        index: &'a str,
    },
    List,
    Stats,
}

/// 예전 `vadd`의 요청. Task 8이 지운다.
#[derive(Debug)]
pub struct Add {
    pub index: String,
    pub vkey: String,
    pub attr: Vec<u8>,
}

/// 예전 `vsearch vector`의 요청. Task 8이 지운다.
#[derive(Debug)]
pub struct Sim {
    pub index: String,
    pub k: usize,
    pub filter: Option<Filter>,
    pub with_attr: bool,
}

/// 본문이 있는 명령. `Add`와 `Sim`은 예전 것으로 파서가 더 만들지 않고
/// Task 8이 지운다.
#[derive(Debug)]
pub enum Body {
    Upsert(Upsert),
    SetMeta(SetMeta),
    /// filter 본문이 있는 vkey 검색.
    SearchKey(SearchKey),
    SearchVector(SearchVector),

    Add(Add),
    Sim(Sim),
}

impl Body {
    /// `execute`가 본문을 자를 때 쓰는 구간 크기.
    pub fn segments(&self) -> Option<Segments> {
        match self {
            Body::Upsert(b) => Some(b.segments),
            Body::SetMeta(b) => Some(b.segments),
            Body::SearchKey(b) => Some(b.segments),
            Body::SearchVector(b) => Some(b.segments),
            Body::Add(_) | Body::Sim(_) => None,
        }
    }
}
