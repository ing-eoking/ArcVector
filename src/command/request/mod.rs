mod layout;
mod parse;

pub use parse::parse;

use super::filter::Filter;
use super::tokens::Tokens;
use crate::Quant;
use crate::error::{Error, Result};
use crate::{ATTR_BYTES, Metric};

pub const MAX_BODY_BYTES: usize = 16 * 1024;

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
    VSim,
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
            ("vsim", Cmd::VSim),
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
    pub maxcount: Option<u32>,
    pub exptime: Option<u32>,
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
            maxcount: None,
            exptime: None,
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
    pub id: String,

    pub dim: usize,
    pub attr: Vec<u8>,
}

#[derive(Debug)]
pub struct Sim {
    pub index: String,
    pub k: usize,
    pub dim: usize,
    pub filter: Option<Filter>,
    pub with_attr: bool,
}

#[derive(Debug)]
pub enum Body {
    Add(Add),
    Sim(Sim),
}
