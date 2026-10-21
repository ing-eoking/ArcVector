mod layout;
mod parse;

pub use parse::parse;

use crate::error::{Error, Result};
use crate::index::Metric;
use crate::index::Quant;

/// Upper bound on the total body size across all segments including trailing CRLF delimiters.
/// Commands exceeding this limit are refused and their connections closed.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Maximum byte length for a `filter` segment.
pub const MAX_FILTER_BYTES: usize = 4096;

/// Maximum number of search results requested via protocol.
pub const MAX_RESULTS: usize = i32::MAX as usize;

#[derive(Debug)]
pub enum Parsed<'a> {
    Line(Line<'a>),

    /// `len` is the sum of known segment lengths. If `unknown_len` is true, some segment
    /// lengths could not be determined, requiring connection closure instead of draining.
    Body {
        len: usize,
        unknown_len: bool,
        request: Result<Body>,
    },
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
    Vop,
}

impl Cmd {
    pub fn parse(name: &str) -> Option<Self> {
        name.eq_ignore_ascii_case("vop").then_some(Cmd::Vop)
    }
}

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

/// Vector body format: `bin` is little-endian f32, `str` is space-delimited decimals.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Bin,
    Str,
}

/// Segment lengths in bytes excluding trailing CRLF. Segment order is meta, filter, vector.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Segments {
    pub meta: Option<usize>,
    pub filter: Option<usize>,
    pub vector: Option<usize>,
}

/// Slices extracted by [`Segments::split`] excluding trailing CRLF delimiters.
#[derive(Debug, PartialEq, Eq)]
pub struct Parts<'b> {
    pub meta: Option<&'b [u8]>,
    pub filter: Option<&'b [u8]>,
    pub vector: Option<&'b [u8]>,
}

impl Segments {
    /// Total body bytes including 2 trailing CRLF bytes per segment.
    pub fn total(&self) -> usize {
        [self.meta, self.filter, self.vector]
            .into_iter()
            .flatten()
            .fold(0usize, |sum, n| sum.saturating_add(n).saturating_add(2))
    }

    /// Splits the contiguous body into segments according to declared lengths and verifies CRLF delimiters.
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

/// `vop create <index> <dim> <expiretime> [<maxcount>] [metric m] [quant q] [M n] [efc c] [efs s]`
///
/// If `maxcount` is `None`, defaults to 4000.
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

#[derive(Debug)]
pub struct Upsert {
    pub index: String,
    pub vkey: String,
    pub format: Format,
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

#[derive(Debug)]
pub enum Line<'a> {
    Create(Create<'a>),
    SearchKey(SearchKey),
    GetMeta(GetMeta<'a>),
    Delete(Delete<'a>),
    Drop(Drop<'a>),
}

#[derive(Debug)]
pub enum Body {
    Upsert(Upsert),
    SetMeta(SetMeta),
    SearchKey(SearchKey),
    SearchVector(SearchVector),
}
