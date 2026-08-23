//! Where each command keeps its arguments. The only module that names token positions.
//!
//! Required arguments occupy `0..TAIL`, so `TAIL` is both the minimum token count and
//! where the optional clause — `ATTR`, `FILTER`, or `vcreate`'s options — begins.

/// `VECTOR` or `KEY`, in either `vsim` form.
pub(super) const SIM_SOURCE: usize = 1;

/// `vadd <index> <id> <veclen> <dim> [ATTR <attrlen> <attr JSON>]`
pub(super) mod add {
    pub const INDEX: usize = 1;
    pub const ID: usize = 2;
    pub const BODY_LEN: usize = 3;
    pub const DIM: usize = 4;
    pub const TAIL: usize = 5;
}

/// `vsim VECTOR <index> <num> <veclen> <dim> [FILTER <n> <term>...]`
pub(super) mod sim_vector {
    pub const INDEX: usize = 2;
    pub const K: usize = 3;
    pub const BODY_LEN: usize = 4;
    pub const DIM: usize = 5;
    pub const TAIL: usize = 6;
}

/// `vsim KEY <index> <num> <key> [FILTER <n> <term>...]`
pub(super) mod sim_key {
    pub const INDEX: usize = 2;
    pub const K: usize = 3;
    pub const KEY: usize = 4;
    pub const TAIL: usize = 5;
}

/// `vcreate <index> <dim> [METRIC m] [QUANT q] [M n] [EFC n] [EFS n] [MAXCOUNT n] [EXPTIME n]`
pub(super) mod create {
    pub const INDEX: usize = 1;
    pub const DIM: usize = 2;
    pub const TAIL: usize = 3;
}

/// `vgetattr <index> <id>`, `vdel <index> <id>`
pub(super) mod names {
    pub const INDEX: usize = 1;
    pub const ID: usize = 2;
    pub const TAIL: usize = 3;
}

/// `vsetattr <index> <id> <attrlen> <attr JSON>`
///
/// The length comes before the value, as `vadd`'s does. There is no `ATTR` keyword: nothing
/// else can follow, so a keyword would only be a word to get wrong.
pub(super) mod setattr {
    pub const INDEX: usize = 1;
    pub const ID: usize = 2;
    pub const ATTR_LEN: usize = 3;
    pub const ATTR: usize = 4;
    /// With an empty ATTR the JSON token is absent, so the line can be either length.
    pub const TAIL_EMPTY: usize = 4;
    pub const TAIL: usize = 5;
}

/// `vdrop <index>`
pub(super) mod drop {
    pub const INDEX: usize = 1;
    pub const TAIL: usize = 2;
}
