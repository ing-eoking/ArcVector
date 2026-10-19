/// 토큰 1은 하위 명령이다.
pub(super) const SUB: usize = 1;
pub(super) const INDEX: usize = 2;

/// `vop create <index> <dim> <expiretime> [<maxcount>] [options]`
pub(super) mod create {
    pub const DIM: usize = 3;
    pub const EXPTIME: usize = 4;
    pub const MAXCOUNT_OR_OPTIONS: usize = 5;
}

/// `vop upsert <index> <vkey> <vector_bytes> [meta <metabytes>] [bin|str]`
pub(super) mod upsert {
    pub const VKEY: usize = 3;
    pub const VECTOR_BYTES: usize = 4;
    pub const TAIL: usize = 5;
}

/// `vop setmeta <index> <vkey> <metabytes>`
pub(super) mod setmeta {
    pub const VKEY: usize = 3;
    pub const META_BYTES: usize = 4;
    pub const LEN: usize = 5;
}

/// `vop search <index> vkey|vector ...`
pub(super) mod search {
    pub const SOURCE: usize = 3;
}

/// `vop search <index> vkey <vkey> <num> [...]`
pub(super) mod search_key {
    pub const VKEY: usize = 4;
    pub const NUM: usize = 5;
    pub const TAIL: usize = 6;
}

/// `vop search <index> vector <num> <vector_bytes> [...]`
pub(super) mod search_vector {
    pub const NUM: usize = 4;
    pub const VECTOR_BYTES: usize = 5;
    pub const TAIL: usize = 6;
}

/// `vop getmeta|delete <index> <vkey>`
pub(super) mod keyed {
    pub const VKEY: usize = 3;
    pub const LEN: usize = 4;
}

/// `vop drop <index>`
pub(super) mod drop {
    pub const LEN: usize = 3;
}
