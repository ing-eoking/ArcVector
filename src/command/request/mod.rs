mod layout;
mod parse;

pub use parse::parse;

use super::filter::Filter;
use super::tokens::Tokens;
use crate::Quant;
use crate::error::{Error, Result};
use crate::{ATTR_BYTES, Metric};

pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// 한 질의가 돌려줄 수 있는 결과의 최대 개수.
///
/// **할당 방어다.** usearch의 Rust 바인딩이 요청한 개수만큼 결과 배열을 만들어
/// 0으로 채운 뒤 찾은 만큼으로 줄인다(`rust/lib.cpp`의 `search_`) -- 요청한
/// 개수가 곧 할당이라, 21억을 받으면 질의 하나가 24GB를 잡으라고 시킨다.
/// 10,000이면 120KB다.
///
/// 넘으면 조용히 자르지 않고 거절한다. 잘라서 주면 클라이언트는 그것이 전부인
/// 줄 안다.
///
/// 인덱스에 담기는 벡터 수와는 무관하다. 그쪽에는 상한이 없다.
pub const MAX_RESULTS: usize = 10_000;

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    VCreate,
    VAdd,
    VSearch,
    VGetAttr,
    VSetAttr,
    VDel,
    VDrop,
    VList,
    VStats,
}

impl Cmd {
    pub fn parse(name: &str) -> Option<Self> {
        const NAMES: [(&str, Cmd); 9] = [
            ("vcreate", Cmd::VCreate),
            ("vadd", Cmd::VAdd),
            ("vsearch", Cmd::VSearch),
            ("vgetattr", Cmd::VGetAttr),
            ("vsetattr", Cmd::VSetAttr),
            ("vdel", Cmd::VDel),
            ("vdrop", Cmd::VDrop),
            ("vlist", Cmd::VList),
            ("vstats", Cmd::VStats),
        ];
        NAMES
            .iter()
            .find(|(text, _)| name.eq_ignore_ascii_case(text))
            .map(|(_, cmd)| *cmd)
    }
}

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

#[derive(Clone, Copy, Debug)]
pub struct Create<'a> {
    pub index: &'a str,
    pub dim: usize,
    pub metric: Metric,
    pub quant: Quant,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub expansion_search: usize,
}

impl Create<'_> {
    fn with_defaults(index: &str, dim: usize) -> Create<'_> {
        Create {
            index,
            dim,
            metric: Metric::Cos,
            quant: Quant::F32,
            connectivity: 0,
            expansion_add: 0,
            expansion_search: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct Trailing {
    pub filter: Option<Filter>,

    pub with_attr: bool,
}

#[derive(Debug)]
pub struct SimKey<'a> {
    pub index: &'a str,
    pub key: &'a str,
    pub k: usize,
    pub filter: Option<Filter>,
    pub with_attr: bool,
}

#[derive(Debug)]
pub enum Line<'a> {
    Create(Create<'a>),
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
    Drop {
        index: &'a str,
    },
    List,
    Stats,
}

#[derive(Debug)]
pub struct Add {
    pub index: String,
    pub vkey: String,

    /// 이 벡터 하나의 만료 시간(초). 0이면 만료되지 않는다.
    pub exptime: u32,
    pub attr: Vec<u8>,
}

#[derive(Debug)]
pub struct Sim {
    pub index: String,
    pub k: usize,
    pub filter: Option<Filter>,
    pub with_attr: bool,
}

#[derive(Debug)]
pub enum Body {
    Add(Add),
    Sim(Sim),
}
