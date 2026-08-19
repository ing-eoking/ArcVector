use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{eitem, eitem_info, elems_result, field_t};
use crate::handler::arcus::abi;
use crate::handler::arcus::element::Layout;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Collection type id for Map, as the engine's `get_elem_info` expects it.
const ITEM_TYPE_MAP: c_int = 3;

impl Store {
    pub fn put_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let vt = self.vtable();
        let (Some(alloc), Some(insert), Some(elem_free), Some(elem_info)) = (
            vt.map_elem_alloc,
            vt.map_elem_insert,
            vt.map_elem_free,
            vt.get_elem_info,
        ) else {
            return Err(StoreError::Unavailable);
        };

        let mut item: *mut eitem = ptr::null_mut();
        // SAFETY: `item` is a live out-parameter the engine fills on success.
        check(unsafe {
            alloc(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                field.len(),
                value.len() + Layout::STORED_TERMINATOR,
                ptr::from_mut(&mut item),
            )
        })?;

        // SAFETY: `item` came from a successful alloc with exactly the sizes requested above.
        let filled = unsafe {
            let mut info: eitem_info = std::mem::zeroed();
            elem_info(
                self.handle(),
                self.cookie,
                ITEM_TYPE_MAP,
                item,
                ptr::from_mut(&mut info),
            );
            let ok = !info.score.is_null() && !info.value.is_null();
            if ok {
                ptr::copy_nonoverlapping(
                    field.as_ptr(),
                    info.score.cast::<u8>().cast_mut(),
                    field.len(),
                );
                let dst = info.value.cast::<u8>().cast_mut();
                ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
                // The engine sized the element for this terminator.
                ptr::copy_nonoverlapping(b"\r\n".as_ptr(), dst.add(value.len()), 2);
            }
            ok
        };
        if !filled {
            // SAFETY: `item` was never inserted, so it is still ours to free.
            unsafe { elem_free(self.handle(), self.cookie, item) };
            // The engine allocated the element and then failed to describe it.
            abi::report_mismatch("get_elem_info did not describe the element it allocated");
            return Err(StoreError::AbiMismatch);
        }

        let mut replaced = false;
        let mut created = false;
        // SAFETY: ownership of `item` passes to the engine only on success.
        let code = unsafe {
            insert(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                item,
                true, // replace_if_exist
                ptr::null_mut(),
                ptr::from_mut(&mut replaced),
                ptr::from_mut(&mut created),
                0,
            )
        };
        check(code).inspect_err(|_| {
            // SAFETY: `item` never became part of the Map — EWOULDBLOCK means it *is* linked, and freeing it there trips `assert(elem->linked == 0)`.
            unsafe { elem_free(self.handle(), self.cookie, item) };
        })
    }

    pub fn delete_elem(&self, key: &str, field: &str) -> Result<()> {
        let Some(delete) = self.vtable().map_elem_delete else {
            return Err(StoreError::Unavailable);
        };
        let selector = field_t {
            value: field.as_ptr().cast::<c_char>().cast_mut(),
            length: field.len(),
        };
        let mut deleted: u32 = 0;
        let mut dropped = false;
        // SAFETY: `key`, `selector` and the out-parameters outlive the call.
        let code = unsafe {
            delete(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                1,
                ptr::from_ref(&selector),
                false, // drop_if_empty: an emptied index must keep existing
                ptr::from_mut(&mut deleted),
                ptr::from_mut(&mut dropped),
                0,
            )
        };
        check(code)?;
        if deleted > 0 {
            Ok(())
        } else {
            Err(StoreError::ElemGone)
        }
    }

    /// Run `f` over the raw (field, value) pairs of the requested elements.
    fn with_elems<T>(
        &self,
        key: &str,
        field: Option<&str>,
        f: impl FnOnce(&[(&[u8], &[u8])]) -> T,
    ) -> Result<T> {
        let vt = self.vtable();
        let (Some(get), Some(elem_info)) = (vt.map_elem_get, vt.get_elem_info) else {
            return Err(StoreError::Unavailable);
        };

        let selector = field.map(|s| field_t {
            value: s.as_ptr().cast::<c_char>().cast_mut(),
            length: s.len(),
        });
        let (numfields, flist) = match &selector {
            Some(f) => (1, ptr::from_ref(f)),
            None => (0, ptr::null()),
        };

        // SAFETY: all-zero is a valid elems_result; the engine fills it in.
        let mut result: elems_result = unsafe { std::mem::zeroed() };
        // SAFETY: `key` and `selector` outlive the call.
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                numfields,
                flist,
                false, // delete
                false, // drop_if_empty
                ptr::from_mut(&mut result),
                0,
            )
        };

        // SAFETY: `result` is what the call above wrote; taken before the error check so a partial result is released too.
        let elems = unsafe { Elems::new(self, &result) };
        check(code)?;
        let Some(items) = elems.as_slice() else {
            return Err(StoreError::ElemGone);
        };

        let views: Vec<(&[u8], &[u8])> = items
            .iter()
            .map(|item| {
                // SAFETY: each entry is a live Map element held by our refcount.
                unsafe {
                    let mut info: eitem_info = std::mem::zeroed();
                    elem_info(
                        self.handle(),
                        self.cookie,
                        ITEM_TYPE_MAP,
                        *item,
                        ptr::from_mut(&mut info),
                    );
                    (
                        slice_or_empty(info.score.cast::<u8>(), info.nscore as usize),
                        slice_or_empty(info.value.cast::<u8>(), info.nbytes as usize),
                    )
                }
            })
            .collect();

        Ok(f(&views))
    }

    /// Copy the fixed ATTR region of one element — the search predicate's hot path.
    pub fn read_attr_slot(
        &self,
        key: &str,
        field: &str,
        layout: &Layout,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.with_elems(key, Some(field), |elems| {
            let (_, value) = elems[0];
            let slot = layout.attr_of(value).ok()?;
            out.clear();
            out.extend_from_slice(slot);
            Some(())
        })?
        .ok_or(StoreError::ElemGone)
    }

    pub fn get_elem(&self, key: &str, field: &str) -> Result<Vec<u8>> {
        self.with_elems(key, Some(field), |elems| elems[0].1.to_vec())
    }

    /// Read every element — used to rebuild the usearch index from Map.
    pub fn get_all(&self, key: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let all = self.with_elems(key, None, |elems| {
            elems
                .iter()
                .map(|(field, value)| (String::from_utf8_lossy(field).into_owned(), value.to_vec()))
                .collect::<Vec<_>>()
        });
        match all {
            // An index with no elements yet is empty, not missing.
            Err(StoreError::ElemGone) => Ok(Vec::new()),
            other => other,
        }
    }
}

/// # Safety
///
/// `ptr` is null, or `len` initialized bytes that outlive `'a`.
unsafe fn slice_or_empty<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// Owns the element array `map_elem_get` allocated and releases it on drop.
struct Elems<'a> {
    store: &'a Store,
    array: *mut *mut eitem,
    count: usize,
}

impl<'a> Elems<'a> {
    /// # Safety
    ///
    /// `result` must be exactly what a `map_elem_get` call wrote, not yet released.
    unsafe fn new(store: &'a Store, result: &elems_result) -> Self {
        Elems {
            store,
            array: result.elem_array,
            count: result.elem_count as usize,
        }
    }

    fn as_slice(&self) -> Option<&[*mut eitem]> {
        if self.array.is_null() || self.count == 0 {
            return None;
        }
        // SAFETY: the engine allocated `count` entries at `array`.
        Some(unsafe { std::slice::from_raw_parts(self.array, self.count) })
    }
}

impl Drop for Elems<'_> {
    fn drop(&mut self) {
        if self.array.is_null() {
            return;
        }
        if let Some(release) = self.store.vtable().map_elem_release {
            // SAFETY: we hold the only reference, and each entry still carries `map_elem_get`'s refcount.
            unsafe {
                release(
                    self.store.handle(),
                    self.store.cookie,
                    self.array,
                    self.count as c_int,
                );
            }
        }
        // SAFETY: the array is plain malloc memory owned by us and freed nowhere else.
        unsafe { free(self.array.cast::<c_void>()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slices_are_returned_for_null_or_zero_length() {
        let data = [1u8, 2, 3];
        // SAFETY: null and zero-length are handled without dereferencing; the third case reads 3 live bytes.
        unsafe {
            assert!(slice_or_empty(ptr::null(), 8).is_empty());
            assert!(slice_or_empty(data.as_ptr(), 0).is_empty());
            assert_eq!(slice_or_empty(data.as_ptr(), 3), &data[..]);
        }
    }
}
