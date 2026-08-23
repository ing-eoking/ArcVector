use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::{Store, StoreError};

pub enum MetaState {
    Usable(MetaRecord, Layout),

    NoMap,

    Damaged(String),

    Unknown(StoreError),
}

pub fn read_metadata(store: &Store, name: &str) -> MetaState {
    let raw = match store.get_elem(name, META_FIELD) {
        Ok(bytes) => bytes,
        Err(StoreError::KeyGone) => return MetaState::NoMap,
        Err(StoreError::ElemGone) => return MetaState::Damaged("no metadata element".to_owned()),
        Err(e) => return MetaState::Unknown(e),
    };
    match MetaRecord::decode(&raw) {
        Ok((meta, layout)) => MetaState::Usable(meta, layout),
        Err(e) => MetaState::Damaged(e.to_string()),
    }
}
