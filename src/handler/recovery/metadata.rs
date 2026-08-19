//! The reserved Map element that says what an index is.

use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::{Store, StoreError};
use crate::handler::usearch::{AnnIndex, Metric, THREAD_SLOTS};

/// Read an index's metadata element, or say why it cannot be used.
///
/// There is no "written by a newer build" answer here: nothing carries a version
/// yet. The low half of [`INDEX_FLAGS`](crate::handler::arcus::engine::INDEX_FLAGS)
/// is where one belongs — it is read through `getattr` before any element, so a
/// Map a newer build wrote would fail `looks_like_index` and be left alone.
pub enum MetaState {
    Usable(MetaRecord, Layout),
    /// Absent or unparsable.
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
