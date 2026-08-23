use std::os::raw::c_void;
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{ENGINE_ITEM_ATTR_ATTR_FLAGS, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, item_attr};

pub const FORMAT_VERSION: u16 = 1;

pub const INDEX_FLAGS: u32 = INDEX_TAG | FORMAT_VERSION as u32;

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
    pub fn looks_like_index(&self) -> bool {
        self.is_map && self.is_tagged() && self.format_version() == FORMAT_VERSION
    }

    pub fn is_tagged(&self) -> bool {
        self.is_map && (self.flags & !VERSION_MASK) == INDEX_TAG
    }

    pub fn format_version(&self) -> u16 {
        (self.flags & VERSION_MASK) as u16
    }
}

pub fn index_attr(maxcount: Option<u32>, exptime: Option<u32>) -> item_attr {
    let mut attr: item_attr = unsafe { std::mem::zeroed() };
    attr.readable = 1;
    attr.flags = INDEX_FLAGS;
    if let Some(m) = maxcount {
        attr.maxcount = i32::try_from(m).unwrap_or(i32::MAX);
    }
    if let Some(e) = exptime {
        attr.exptime = e;
    }
    attr
}

impl Store {
    pub fn probe_map(&self, key: &str) -> Result<MapProbe> {
        let Some(getattr) = self.vtable().getattr else {
            return Err(StoreError::Unavailable);
        };

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
