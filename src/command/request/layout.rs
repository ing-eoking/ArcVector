pub(super) const SIM_SOURCE: usize = 1;

pub(super) mod add {
    pub const INDEX: usize = 1;
    pub const ID: usize = 2;
    pub const BODY_LEN: usize = 3;
    pub const DIM: usize = 4;
    pub const TAIL: usize = 5;
}

pub(super) mod sim_vector {
    pub const INDEX: usize = 2;
    pub const K: usize = 3;
    pub const BODY_LEN: usize = 4;
    pub const DIM: usize = 5;
    pub const TAIL: usize = 6;
}

pub(super) mod sim_key {
    pub const INDEX: usize = 2;
    pub const K: usize = 3;
    pub const KEY: usize = 4;
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

pub(super) mod setattr {
    pub const INDEX: usize = 1;
    pub const ID: usize = 2;
    pub const ATTR_LEN: usize = 3;
    pub const ATTR: usize = 4;

    pub const TAIL_EMPTY: usize = 4;
    pub const TAIL: usize = 5;
}

pub(super) mod drop {
    pub const INDEX: usize = 1;
    pub const TAIL: usize = 2;
}
