mod coords;
mod index;
mod meta;
mod search;
mod vector;

pub use index::{create, drop_index};
pub use meta::getmeta;
pub use search::{search_key, search_vector};
pub use vector::{
    Added, UpsertPlan, delete, setmeta_write, upsert_insert, upsert_reply, upsert_write,
};
