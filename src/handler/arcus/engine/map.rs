//! The Map item behind an index: probing it, creating it, dropping it.

use std::os::raw::c_void;
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{ENGINE_ITEM_ATTR_ATTR_FLAGS, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, item_attr};

/// Item flags marking a Map as ours: `"AV"`, and nothing else.
///
/// A hint, never an authority — a client can set any flags through
/// `mop create <key> <flags> …` — so this only answers "plausibly ours?" cheaply,
/// before the metadata element decides.
///
/// Neither this nor an element carries a format version. Adding one is what a
/// change to the stored layout would need, and it has to ship *first*: a build
/// with no version reads a newer Map, fails, and judges it damaged.
pub const INDEX_FLAGS: u32 = 0x4156_0000;

/// What a Map looks like from the outside, before any element is read.
#[derive(Clone, Copy, Debug)]
pub struct MapProbe {
    pub is_map: bool,
    pub flags: u32,
    pub count: u32,
    pub maxcount: u32,
}

impl MapProbe {
    /// Whether this is plausibly ours. A hint; the metadata element decides.
    pub fn looks_like_index(&self) -> bool {
        self.is_map && self.flags == INDEX_FLAGS
    }
}

impl Store {
    /// What `getattr` says about a Map, without reading any element.
    pub fn probe_map(&self, key: &str) -> Result<MapProbe> {
        let Some(getattr) = self.vtable().getattr else {
            return Err(StoreError::Unavailable);
        };
        // SAFETY: `key`, `ids` and `attr` all outlive the call, and the engine
        // fills `attr` for the ids it is given.
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

    pub fn create_map(&self, key: &str, maxcount: Option<u32>, exptime: Option<u32>) -> Result<()> {
        let Some(create) = self.vtable().map_struct_create else {
            return Err(StoreError::Unavailable);
        };
        // SAFETY: all-zero is a valid item_attr; the engine reads the fields we
        // set plus the zeroed defaults.
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
        // SAFETY: `key` and `attr` outlive the call.
        check(unsafe {
            create(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                ptr::from_mut(&mut attr),
                0,
            )
        })
    }

    /// Delete the whole Map item — this is how an index is dropped.
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
