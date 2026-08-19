use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::usearch::{AnnIndex, Metric, THREAD_SLOTS};

/// Nothing carries a version yet; the low half of `INDEX_FLAGS` is where one belongs, read before any element.
pub enum MetaState {
    Usable(MetaRecord, Layout),
    Damaged(String),
}

pub fn read_metadata(store: &Store, name: &str) -> MetaState {
    let raw = match store.get_elem(name, META_FIELD) {
        Ok(bytes) => bytes,
        Err(StoreError::ElemGone) => return MetaState::Damaged("no metadata element".to_owned()),
        Err(e) => return MetaState::Damaged(e.to_string()),
    };
    match MetaRecord::decode(&raw) {
        Ok((meta, layout)) => MetaState::Usable(meta, layout),
        Err(e) => MetaState::Damaged(e.to_string()),
    }
}

pub fn build_ann(meta: &MetaRecord, layout: Layout) -> Result<AnnIndex> {
    let metric = Metric::parse(&meta.metric)
        .ok_or_else(|| Error::bad_request(format!("unknown metric '{}'", meta.metric)))?;
    AnnIndex::new(
        layout,
        metric,
        meta.connectivity,
        meta.expansion_add,
        meta.expansion_search,
        THREAD_SLOTS,
    )
}

/// Write `owner` into the Map's metadata element, keeping everything else.
pub(super) fn stamp(store: &Store, name: &str, owner: u64) -> Result<()> {
    let (meta, layout) = match read_metadata(store, name) {
        MetaState::Usable(meta, layout) => (meta, layout),
        MetaState::Damaged(why) => return Err(Error::bad_request(why)),
    };
    let claimed = MetaRecord { owner, ..meta };
    store.put_elem(name, META_FIELD, &claimed.encode(layout))?;
    Ok(())
}
