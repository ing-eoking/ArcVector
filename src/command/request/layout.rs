pub(super) const SEARCH_SOURCE: usize = 1;

/// `vadd <index> <vkey> <veclen> <exptime> [attr <JSON>]`
pub(super) mod add {
    pub const INDEX: usize = 1;
    pub const VKEY: usize = 2;
    pub const BODY_LEN: usize = 3;
    pub const EXPTIME: usize = 4;
    pub const TAIL: usize = 5;
}

/// `vsearch vector <index> <num> <veclen> [...]`
///
/// 차원은 인덱스가 알고 있으므로 받지 않는다.
pub(super) mod search_vector {
    pub const INDEX: usize = 2;
    pub const NUM: usize = 3;
    pub const BODY_LEN: usize = 4;
    pub const TAIL: usize = 5;
}

/// `vsearch key <index> <vkey> <num> [...]`
pub(super) mod search_key {
    pub const INDEX: usize = 2;
    pub const VKEY: usize = 3;
    pub const NUM: usize = 4;
    pub const TAIL: usize = 5;
}

pub(super) mod create {
    pub const INDEX: usize = 1;
    pub const DIM: usize = 2;
    pub const TAIL: usize = 3;
}

pub(super) mod names {
    pub const INDEX: usize = 1;
    pub const ID: usize = 2;
    pub const TAIL: usize = 3;
}

/// `vsetattr <index> <vkey> <JSON>`
pub(super) mod setattr {
    pub const INDEX: usize = 1;
    pub const VKEY: usize = 2;
    pub const ATTR: usize = 3;
    pub const TAIL: usize = 4;
}

pub(super) mod drop {
    pub const INDEX: usize = 1;
    pub const TAIL: usize = 2;
}
