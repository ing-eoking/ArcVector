use crate::error::{Error, Result};
use crate::handler::arcus::element::{Layout, META_FIELD, MetaRecord};
use crate::handler::arcus::engine::Store;
use crate::handler::meta::{MetaState, read_metadata};
use crate::handler::usearch::{AnnIndex, Metric};

pub fn build_ann(meta: &MetaRecord, layout: Layout) -> Result<AnnIndex> {
    let metric = Metric::parse(&meta.metric)
        .ok_or_else(|| Error::bad_request(format!("unknown metric '{}'", meta.metric)))?;
    AnnIndex::new(
        layout,
        metric,
        meta.connectivity,
        meta.expansion_add,
        meta.expansion_search,
    )
}

/// Write `owner` into the Map's metadata element, keeping everything else.
pub(super) fn stamp(store: &Store, name: &str, owner: u64) -> Result<()> {
    let (meta, layout) = match read_metadata(store, name) {
        MetaState::Usable(meta, layout) => (meta, layout),
        MetaState::NoMap => return Err(Error::NoSuchIndex),
        MetaState::Damaged(why) => return Err(Error::bad_request(why)),
        MetaState::Unknown(e) => return Err(e.into()),
    };
    let claimed = MetaRecord { owner, ..meta };
    store.put_elem(name, META_FIELD, &claimed.encode(layout))?;
    Ok(())
}
