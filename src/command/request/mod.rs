//! The vocabulary — one type per command, and nothing that touches a token.
//!
//! `parse` is what turns tokens into these.

mod parse;

pub use parse::{body_length_at, body_length_error, parse_body, parse_line};

use super::filter::Filter;
use super::tokens::Tokens;
use crate::Quant;
use crate::error::{Error, Result};
use crate::{ATTR_BYTES, Metric};

/// Upper bound on one transferred body.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The commands this extension answers. Matched case-insensitively.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    VCreate,
    VAdd,
    VSim,
    VGet,
    VDel,
    VDrop,
    VList,
    VStats,
}

impl Cmd {
    pub fn parse(name: &str) -> Option<Self> {
        const NAMES: [(&str, Cmd); 8] = [
            ("vcreate", Cmd::VCreate),
            ("vadd", Cmd::VAdd),
            ("vsim", Cmd::VSim),
            ("vget", Cmd::VGet),
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

/// Where a `VSIM` gets its query coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimSource {
    /// Coordinates arrive in the body.
    Vector,
    /// The query is a vector already stored under a key.
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

/// Index geometry and tuning from a `vcreate` line.
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
    /// Defaults for everything a `vcreate` line may omit.
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

/// A `VSIM KEY` query.
#[derive(Debug)]
pub struct SimKey<'a> {
    pub index: &'a str,
    pub key: &'a str,
    pub k: usize,
    pub filter: Option<Filter>,
}

/// A request fully determined by its command line.
#[derive(Debug)]
pub enum Line<'a> {
    Create(Create<'a>),
    SimKey(SimKey<'a>),
    Get { index: &'a str, id: &'a str },
    Del { index: &'a str, id: &'a str },
    Drop { index: &'a str },
    List,
    Stats,
}

/// A `vadd` line, awaiting its coordinates.
#[derive(Debug)]
pub struct Add {
    pub index: String,
    pub id: String,
    /// Dimension the client declared.
    pub dim: usize,
    pub attr: Vec<u8>,
}

/// A `VSIM VECTOR` line, awaiting its coordinates.
#[derive(Debug)]
pub struct Sim {
    pub index: String,
    pub k: usize,
    pub dim: usize,
    pub filter: Option<Filter>,
}

/// A request whose body has still to arrive.
#[derive(Debug)]
pub enum Body {
    Add(Add),
    Sim(Sim),
}
