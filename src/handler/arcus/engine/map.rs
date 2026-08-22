use std::os::raw::c_void;
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{ENGINE_ITEM_ATTR_ATTR_FLAGS, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, item_attr};

/// Stored element layout. Bumped whenever the bytes of an element move, so a build that
/// cannot read them says so instead of reading a vector out of an ATTR.
///
/// 1 — `alen: u16`, `key: u64`, ATTR, vector.
pub const FORMAT_VERSION: u16 = 1;

/// Tag and version in one flag word. A hint, never an authority — a client can set any flags
/// — but a Map whose tag is ours and whose version is not is a Map this build must leave
/// alone rather than reinterpret.
pub const INDEX_FLAGS: u32 = INDEX_TAG | FORMAT_VERSION as u32;

/// The half that says "an ArcVector index".
const INDEX_TAG: u32 = 0x4156_0000;
const VERSION_MASK: u32 = 0x0000_FFFF;

#[derive(Clone, Copy, Debug)]
pub struct MapProbe {
    pub is_map: bool,
    pub flags: u32,
    pub count: u32,
    pub maxcount: u32,
}

impl MapProbe {
    /// Ours, and a version this build knows how to read.
    pub fn looks_like_index(&self) -> bool {
        self.is_map && self.is_tagged() && self.format_version() == FORMAT_VERSION
    }

    /// Ours by tag, whatever the version says. A Map that is ours but from a layout this
    /// build cannot read must not be deleted or rewritten — only refused.
    pub fn is_tagged(&self) -> bool {
        self.is_map && (self.flags & !VERSION_MASK) == INDEX_TAG
    }

    pub fn format_version(&self) -> u16 {
        (self.flags & VERSION_MASK) as u16
    }
}

/// Attributes for a Map that is one of ours, sized for `maxcount` elements.
///
/// Handed to `map_elem_insert`, which creates the Map with them when it is not there —
/// see [`PendingElem::insert_creating`](super::elem::PendingElem::insert_creating).
/// There is no separate create call: the engine's own two-in-one is the only way to get
/// a Map and its metadata element without a window between them.
pub fn index_attr(maxcount: Option<u32>, exptime: Option<u32>) -> item_attr {
    // SAFETY: all-zero is a valid `item_attr`.
    let mut attr: item_attr = unsafe { std::mem::zeroed() };
    attr.readable = 1;
    attr.flags = INDEX_FLAGS;
    if let Some(m) = maxcount {
        // Callers validate the range; saturate rather than wrap if one slips.
        attr.maxcount = i32::try_from(m).unwrap_or(i32::MAX);
    }
    if let Some(e) = exptime {
        attr.exptime = e;
    }
    attr
}

impl Store {
    /// What `getattr` says about a Map, without reading any element.
    pub fn probe_map(&self, key: &str) -> Result<MapProbe> {
        let Some(getattr) = self.vtable().getattr else {
            return Err(StoreError::Unavailable);
        };
        // SAFETY: `key`, `ids` and `attr` outlive the call.
        let (code, attr) = unsafe {
            let mut ids = [ENGINE_ITEM_ATTR_ATTR_FLAGS];
            let mut attr: item_attr = std::mem::zeroed();
            let code = getattr(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                ids.as_mut_ptr(),
                ids.len() as u32,
                ptr::from_mut(&mut attr),
                0,
            );
            (code, attr)
        };
        check(code)?;
        Ok(MapProbe {
            is_map: u32::from(attr.type_) == ENGINE_ITEM_TYPE_ITEM_TYPE_MAP,
            flags: attr.flags,
            count: u32::try_from(attr.count).unwrap_or(0),
            maxcount: u32::try_from(attr.maxcount).unwrap_or(0),
        })
    }

    pub fn drop_map(&self, key: &str) -> Result<()> {
        let Some(remove) = self.vtable().remove else {
            return Err(StoreError::Unavailable);
        };
        // SAFETY: `key` outlives the call.
        check(unsafe {
            remove(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                key.len(),
                0,
                0,
            )
        })
    }
}
