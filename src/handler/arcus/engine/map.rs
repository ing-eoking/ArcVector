use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check, written};
use crate::engine_api::{
    ENGINE_ITEM_ATTR, ENGINE_ITEM_ATTR_ATTR_COUNT, ENGINE_ITEM_ATTR_ATTR_MAXCOUNT,
    ENGINE_ITEM_ATTR_ATTR_TYPE, ENGINE_ITEM_TYPE_ITEM_TYPE_MAP, eitem, eitem_info, elems_result,
    field_t, item, item_attr, item_info,
};
use crate::handler::arcus::abi;
use crate::handler::arcus::element::Layout;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

const ITEM_TYPE_MAP: c_int = ENGINE_ITEM_TYPE_ITEM_TYPE_MAP as c_int;

/// What `getattr` says about the key an index lives at.
#[derive(Clone, Copy, Debug)]
pub struct MapProbe {
    pub is_map: bool,
    pub count: u32,
    pub maxcount: u32,
}

/// The attributes `vcreate` creates an index's Map with.
///
/// `readable` so a half-written Map is never what a reader sees, `maxcount`
/// for the engine to enforce, and `exptime` already converted to the engine's
/// clock. Zero for either means the engine's default.
pub fn index_attr(maxcount: u32, exptime: u32) -> item_attr {
    let mut attr: item_attr = unsafe { std::mem::zeroed() };
    attr.readable = 1;
    // 이 플래그가 선 Map만 엔진이 요소 이벤트를 보낸다. 복제본에도 따라간다.
    attr.event = 1;
    attr.maxcount = i32::try_from(maxcount).unwrap_or(i32::MAX);
    attr.exptime = exptime;
    attr
}

/// What an insert did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inserted {
    /// The Map did not exist and the insert created it.
    pub created: bool,
    /// The field was there and the insert replaced its element.
    pub replaced: bool,
}

fn selector(field: &str) -> field_t {
    field_t {
        value: field.as_ptr().cast::<c_char>().cast_mut(),
        length: field.len(),
    }
}

impl Store {
    /// Asks the engine about a key, which makes it run `do_item_isvalid` there.
    ///
    /// That side effect is the point at a command's entrance: an expired Map,
    /// one under a `flush_all`, or one whose prefix was flushed is unlinked by
    /// this call, and the unlink comes back as the event that drops the index
    /// from the registry -- before the caller looks the name up.
    pub fn probe_map(&self, key: &str) -> Result<MapProbe> {
        let Some(getattr) = self.vtable().getattr else {
            return Err(StoreError::Unavailable);
        };
        let mut ids: [ENGINE_ITEM_ATTR; 3] = [
            ENGINE_ITEM_ATTR_ATTR_TYPE,
            ENGINE_ITEM_ATTR_ATTR_COUNT,
            ENGINE_ITEM_ATTR_ATTR_MAXCOUNT,
        ];
        let mut attr: item_attr = unsafe { std::mem::zeroed() };
        let code = unsafe {
            getattr(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                ids.as_mut_ptr(),
                ids.len() as u32,
                ptr::from_mut(&mut attr),
                0,
            )
        };
        let is_map = u32::from(attr.type_) == ENGINE_ITEM_TYPE_ITEM_TYPE_MAP;
        match check(code) {
            Ok(()) => {}
            // A KV item refuses the collection attributes. It is still an
            // answer: the key holds something that is not an index.
            Err(StoreError::Engine(_)) if !is_map => {
                return Ok(MapProbe {
                    is_map: false,
                    count: 0,
                    maxcount: 0,
                });
            }
            Err(e) => return Err(e),
        }
        Ok(MapProbe {
            is_map,
            count: u32::try_from(attr.count).unwrap_or(0),
            maxcount: u32::try_from(attr.maxcount).unwrap_or(0),
        })
    }

    /// Removes the whole Map. Its elements go later, from the engine's
    /// collection-delete thread, each with its own unlink event.
    pub fn drop_map(&self, key: &str) -> Result<()> {
        let Some(remove) = self.vtable().remove else {
            return Err(StoreError::Unavailable);
        };
        written(unsafe {
            remove(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                key.len(),
                0, // cas: unconditional
                0, // vbucket
            )
        })
    }

    /// Writes one element.
    ///
    /// `replace` overwrites an element already at `field`; without it the
    /// engine answers `ElemExists`. `create` makes the Map with those
    /// attributes when it is not there; without it a missing Map is
    /// `KeyGone`.
    ///
    /// **This is the call that fires the link event**, under the engine's
    /// cache lock and on this thread. The body gets a CRLF, which the engine
    /// expects of every element value.
    ///
    /// **Worker thread only.** The write runs the engine's write hooks, and
    /// replication hangs per-thread state off them.
    pub fn insert_elem(
        &self,
        key: &str,
        field: &str,
        value: &[u8],
        replace: bool,
        create: Option<item_attr>,
    ) -> Result<Inserted> {
        let vt = self.vtable();
        let (Some(alloc), Some(elem_free), Some(elem_info), Some(insert)) = (
            vt.map_elem_alloc,
            vt.map_elem_free,
            vt.get_elem_info,
            vt.map_elem_insert,
        ) else {
            return Err(StoreError::Unavailable);
        };

        let mut elem: *mut eitem = ptr::null_mut();
        check(unsafe {
            alloc(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                field.len(),
                value.len() + Layout::STORED_TERMINATOR,
                ptr::from_mut(&mut elem),
            )
        })?;

        let filled = unsafe {
            let mut info: eitem_info = std::mem::zeroed();
            elem_info(
                self.handle(),
                self.cookie,
                ITEM_TYPE_MAP,
                elem,
                ptr::from_mut(&mut info),
            );
            let ok = !info.score.is_null()
                && !info.value.is_null()
                && info.nscore as usize == field.len()
                && info.nbytes as usize == value.len() + Layout::STORED_TERMINATOR;
            if ok {
                ptr::copy_nonoverlapping(
                    field.as_ptr(),
                    info.score.cast::<u8>().cast_mut(),
                    field.len(),
                );
                let dst = info.value.cast::<u8>().cast_mut();
                ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
                ptr::copy_nonoverlapping(b"\r\n".as_ptr(), dst.add(value.len()), 2);
            }
            ok
        };
        if !filled {
            unsafe { elem_free(self.handle(), self.cookie, elem) };
            abi::report_mismatch("get_elem_info did not describe the element it allocated");
            return Err(StoreError::AbiMismatch);
        }

        let mut attr = create;
        let attrp = attr.as_mut().map_or(ptr::null_mut(), ptr::from_mut);
        let mut done = Inserted::default();
        let code = unsafe {
            insert(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                elem,
                replace,
                attrp,
                ptr::from_mut(&mut done.replaced),
                ptr::from_mut(&mut done.created),
                0, // vbucket
            )
        };
        // EWOULDBLOCK은 링크된 뒤의 복제 대기라 성공이다. 그때 요소는 엔진 것이고
        // `done`도 엔진이 채웠다 -- 여기서 풀면 링크된 요소를 푼다.
        if let Err(e) = written(code) {
            // Not linked, so nothing else saw it. It is still ours to free.
            unsafe { elem_free(self.handle(), self.cookie, elem) };
            return Err(e);
        }
        Ok(done)
    }

    /// Removes one element. `ElemGone` when there was nothing at `field`.
    pub fn delete_elem(&self, key: &str, field: &str) -> Result<()> {
        let Some(delete) = self.vtable().map_elem_delete else {
            return Err(StoreError::Unavailable);
        };
        let selector = selector(field);
        let mut deleted: u32 = 0;
        let mut dropped = false;
        written(unsafe {
            delete(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                1,
                ptr::from_ref(&selector),
                false, // drop_if_empty: the metadata keeps it from emptying anyway
                ptr::from_mut(&mut deleted),
                ptr::from_mut(&mut dropped),
                0,
            )
        })?;
        if deleted > 0 {
            Ok(())
        } else {
            Err(StoreError::ElemGone)
        }
    }

    /// Fetches the element at `field` and **keeps** the reference `get` took,
    /// handing back its address.
    ///
    /// Give it back with [`Store::release_elems`].
    pub fn hold_elem(&self, key: &str, field: &str) -> Result<u64> {
        let Some(get) = self.vtable().map_elem_get else {
            return Err(StoreError::Unavailable);
        };
        let selector = selector(field);
        let mut result: elems_result = unsafe { std::mem::zeroed() };
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                1,
                ptr::from_ref(&selector),
                false, // delete
                false, // drop_if_empty
                ptr::from_mut(&mut result),
                0,
            )
        };
        let found = if result.elem_array.is_null() || result.elem_count == 0 {
            None
        } else {
            let first = unsafe { *result.elem_array } as u64;
            if result.elem_count > 1 {
                // One field was asked for. Anything past the first is not
                // ours to keep.
                let rest = unsafe {
                    std::slice::from_raw_parts(
                        result.elem_array.add(1),
                        result.elem_count as usize - 1,
                    )
                };
                let rest: Vec<u64> = rest.iter().map(|e| *e as u64).collect();
                self.release_elems(&rest);
            }
            Some(first)
        };
        if !result.elem_array.is_null() {
            unsafe { free(result.elem_array.cast::<c_void>()) };
        }
        check(code)?;
        found.ok_or(StoreError::ElemGone)
    }

    /// Copies the value at `field` out of the store.
    pub fn get_elem(&self, key: &str, field: &str) -> Result<Vec<u8>> {
        let addr = self.hold_elem(key, field)?;
        let value = self.with_elem_at(addr, |_field, value| value.to_vec());
        self.release_elems(&[addr]);
        value.ok_or(StoreError::CorruptElement)
    }

    /// Gives back element references: ones [`Store::hold_elem`] took, or ones
    /// the link event handed over.
    ///
    /// Takes the cache lock, so this must not run while the graph lock is held
    /// and never from inside the event callback.
    pub fn release_elems(&self, addrs: &[u64]) {
        if addrs.is_empty() {
            return;
        }
        let Some(release) = self.vtable().map_elem_release else {
            return;
        };
        let mut array: Vec<*mut eitem> = addrs.iter().map(|a| *a as *mut eitem).collect();
        unsafe {
            release(
                self.handle(),
                self.cookie,
                array.as_mut_ptr(),
                as_int(array.len()),
            );
        }
    }

    /// Reads an element's field and value in place, taking no lock and no
    /// reference.
    ///
    /// `get_elem_info` is a pure field read in the default engine, so this is
    /// one of the two engine calls that may be made from inside the event
    /// callback -- everything else takes the cache lock the callback already
    /// holds.
    ///
    /// The caller must already own a reference to the element, or be inside
    /// the callback that is handing one over. The borrow is only valid for the
    /// duration of `f`.
    pub fn with_elem_at<T>(&self, addr: u64, f: impl FnOnce(&[u8], &[u8]) -> T) -> Option<T> {
        let elem_info = self.vtable().get_elem_info?;
        let elem = addr as *const eitem;
        if elem.is_null() {
            return None;
        }
        let mut info: eitem_info = unsafe { std::mem::zeroed() };
        unsafe {
            elem_info(
                self.handle(),
                self.cookie,
                ITEM_TYPE_MAP,
                elem,
                ptr::from_mut(&mut info),
            );
        }
        if info.score.is_null() || info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        let field =
            unsafe { std::slice::from_raw_parts(info.score.cast::<u8>(), info.nscore as usize) };
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };
        Some(f(field, value))
    }

    /// Reads a hash item's key in place, taking no lock and no reference.
    ///
    /// The other engine call the event callback may make: `get_item_info` is a
    /// field read too. The event hands over the Map an element belongs to, and
    /// its key is how the callback learns which index that is.
    pub fn with_key_at<T>(&self, addr: u64, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
        let info_of = self.vtable().get_item_info?;
        let it = addr as *const item;
        if it.is_null() {
            return None;
        }
        let mut info: item_info = unsafe { std::mem::zeroed() };
        if !unsafe { info_of(self.handle(), self.cookie, it, ptr::from_mut(&mut info)) } {
            return None;
        }
        if info.key.is_null() {
            return None;
        }
        let key = unsafe { std::slice::from_raw_parts(info.key.cast::<u8>(), info.nkey as usize) };
        Some(f(key))
    }
}
