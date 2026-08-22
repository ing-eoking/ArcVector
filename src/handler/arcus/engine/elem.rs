use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{eitem, eitem_info, elems_result, field_t, item_attr};
use crate::handler::arcus::abi;
use crate::handler::arcus::element::Layout;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Collection type id for Map, as the engine's `get_elem_info` expects it.
const ITEM_TYPE_MAP: c_int = 3;

/// An element the engine has allocated and we have filled, but that is not part of the Map
/// yet. Nothing can observe it until [`PendingElem::insert`] — not even a `vget` on the same
/// id, which reads the Map. Dropping it hands the space back.
///
/// This is what lets a `vadd` do every fallible step before anything becomes visible: the
/// element body is reserved here, and only `insert` publishes it. `insert` can still fail —
/// `maxcount` and the hash-node allocation are both checked in `do_map_elem_link`, which it
/// calls — so its caller has to be able to undo whatever else it did.
///
/// It is also the point of no return: `do_map_elem_link` emits `CLOG_MAP_ELEM_INSERT`, which
/// is what carries the write to replicas and the persistence log. Allocating logs nothing, so
/// everything that can fail belongs on this side of it.
#[must_use = "an uninserted element is invisible; insert it or drop it"]
pub struct PendingElem<'a> {
    store: &'a Store,
    /// The Map this element was reserved for; `insert` cannot be pointed at another.
    key: &'a str,
    item: *mut eitem,
    /// The element's value bytes, inside the engine's allocation. Reserving before the value
    /// is known is the point: allocation is the step most likely to fail, and doing it first
    /// means a graph insert is never paid for and thrown away.
    value: *mut u8,
    value_len: usize,
}

impl Drop for PendingElem<'_> {
    fn drop(&mut self) {
        if self.item.is_null() {
            return;
        }
        let Some(elem_free) = self.store.vtable().map_elem_free else {
            return;
        };
        // SAFETY: a non-null `item` here was never inserted, so it is still ours to free.
        unsafe { elem_free(self.store.handle(), self.store.cookie, self.item) };
    }
}

impl PendingElem<'_> {
    /// The reserved value bytes, to be filled before `insert`.
    ///
    /// Nothing can read them: the element is not part of the Map until `insert`, so this is
    /// writing into memory only this handle can reach.
    pub fn value_mut(&mut self) -> &mut [u8] {
        // SAFETY: `value`/`value_len` came from the engine describing this allocation, and
        // `&mut self` is the only handle to it.
        unsafe { std::slice::from_raw_parts_mut(self.value, self.value_len) }
    }

    /// Publish the element into a Map that already exists, replacing whatever was under the
    /// same field. On failure `Drop` hands the space back.
    pub fn insert(self) -> Result<()> {
        self.publish(ptr::null_mut(), true).map(|_| ())
    }

    /// Publish the element, creating the Map with `attr` when it is not there. Reports
    /// whether this call is what created it.
    ///
    /// The engine does both under one cache lock and unlinks the Map again if the element
    /// cannot go in, so **a Map without this element cannot exist** — no window, and nothing
    /// for the caller to compensate. This is why there is no separate create call.
    ///
    /// Existing elements are not replaced, so a racing creator's metadata survives and the
    /// loser is told the element is already there. But a Map that exists *without* the
    /// element takes it: `false` means the element went into somebody else's Map, and the
    /// caller has to take it back out — see `vcreate`.
    pub fn insert_creating(self, mut attr: item_attr) -> Result<bool> {
        self.publish(ptr::from_mut(&mut attr), false)
    }

    /// `attr` non-null lets the engine create the Map; null requires it to exist. Reports
    /// whether the Map was created by this call.
    fn publish(mut self, attr: *mut item_attr, replace_if_exist: bool) -> Result<bool> {
        let Some(insert) = self.store.vtable().map_elem_insert else {
            return Err(StoreError::Unavailable);
        };
        let mut replaced = false;
        let mut created = false;
        // SAFETY: ownership of `item` passes to the engine only when the code says completed.
        // `attr` is either null or a live local of the caller frame.
        let code = unsafe {
            insert(
                self.store.handle(),
                self.store.cookie,
                self.key.as_ptr().cast::<c_void>(),
                as_int(self.key.len()),
                self.item,
                replace_if_exist,
                attr,
                ptr::from_mut(&mut replaced),
                ptr::from_mut(&mut created),
                0,
            )
        };
        // `check` counts EWOULDBLOCK as completed, which matters here: the element *is*
        // linked in that case, and freeing it would trip `assert(elem->linked == 0)`.
        check(code)?;
        // The engine owns it now; keep `Drop` from freeing it.
        self.item = ptr::null_mut();
        Ok(created)
    }
}

impl Store {
    /// Reserve and fill the element for `field`, without making it part of the Map.
    pub fn alloc_elem<'a>(
        &'a self,
        key: &'a str,
        field: &str,
        value: &[u8],
    ) -> Result<PendingElem<'a>> {
        let mut pending = self.reserve_elem(key, field, value.len())?;
        pending.value_mut().copy_from_slice(value);
        Ok(pending)
    }

    /// Reserve `len` value bytes for `field`, leaving them unwritten.
    pub fn reserve_elem<'a>(
        &'a self,
        key: &'a str,
        field: &str,
        len: usize,
    ) -> Result<PendingElem<'a>> {
        let vt = self.vtable();
        let (Some(alloc), Some(elem_free), Some(elem_info)) =
            (vt.map_elem_alloc, vt.map_elem_free, vt.get_elem_info)
        else {
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
                len + Layout::STORED_TERMINATOR,
                ptr::from_mut(&mut item),
            )
        })?;

        // SAFETY: `item` came from a successful alloc with exactly the sizes requested above.
        let mut value: *mut u8 = ptr::null_mut();
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
                // The engine sized the element for this terminator, so it can be written now
                // and the value bytes handed back on their own.
                ptr::copy_nonoverlapping(b"\r\n".as_ptr(), dst.add(len), 2);
                value = dst;
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

        Ok(PendingElem {
            store: self,
            key,
            item,
            value,
            value_len: len,
        })
    }

    /// Allocate and insert in one step, for writes with nothing to undo.
    pub fn put_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        self.alloc_elem(key, field, value)?.insert()
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
        let elems = self.fetch(key, field)?;
        let Some(items) = elems.as_slice() else {
            return Err(StoreError::ElemGone);
        };
        let views: Vec<(&[u8], &[u8])> = items.iter().map(|item| elems.view(*item)).collect();
        Ok(f(&views))
    }

    /// Read one element and delete it in the same call.
    ///
    /// `map_elem_get` takes a `delete` flag, so the value and its removal are one engine
    /// operation. `vdel` needs to know there was an element and needs it gone, and
    /// doing that as two calls would let a write land between them.
    pub fn take_elem(&self, key: &str, field: &str) -> Result<Vec<u8>> {
        self.fetch_with(key, Some(field), true)
            .and_then(|elems| match elems.as_slice() {
                Some(items) => Ok(elems.view(items[0]).1.to_vec()),
                None => Err(StoreError::ElemGone),
            })
    }

    /// `map_elem_get`, with the hold it takes still in place.
    fn fetch(&self, key: &str, field: Option<&str>) -> Result<Elems<'_>> {
        self.fetch_with(key, field, false)
    }

    fn fetch_with(&self, key: &str, field: Option<&str>, delete: bool) -> Result<Elems<'_>> {
        let vt = self.vtable();
        let Some(get) = vt.map_elem_get else {
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
                delete,
                // An emptied index must keep existing.
                false, // drop_if_empty
                ptr::from_mut(&mut result),
                0,
            )
        };

        // SAFETY: `result` is what the call above wrote; taken before the error check so a partial result is released too.
        let elems = unsafe { Elems::new(self, &result) };
        check(code)?;
        Ok(elems)
    }

    /// Fetch one element and keep the engine's hold on it.
    ///
    /// `map_elem_get` raises the element's refcount and `map_elem_release` lowers it; while
    /// raised the engine will not free the bytes, so they can be read again later without a
    /// second lookup. A concurrent overwrite links a new element and leaves this one
    /// unlinked-but-alive, so what is read stays the value this call saw. `Drop` releases.
    ///
    /// The caller is what bounds how many are held at once — each one keeps an element's
    /// memory from being reclaimed.
    pub fn hold_elem(&self, key: &str, field: &str) -> Result<HeldElem<'_>> {
        self.fetch(key, Some(field)).and_then(|elems| {
            if elems.as_slice().is_none() {
                return Err(StoreError::ElemGone);
            }
            Ok(HeldElem { elems })
        })
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

    /// The field and value bytes of one held element. `get_elem_info` fills in pointers the
    /// engine already has, so this is not a second lookup.
    fn view(&self, item: *mut eitem) -> (&'a [u8], &'a [u8]) {
        let Some(elem_info) = self.store.vtable().get_elem_info else {
            return (&[], &[]);
        };
        // SAFETY: `item` is a live Map element held by our refcount.
        unsafe {
            let mut info: eitem_info = std::mem::zeroed();
            elem_info(
                self.store.handle(),
                self.store.cookie,
                ITEM_TYPE_MAP,
                item,
                ptr::from_mut(&mut info),
            );
            (
                slice_or_empty(info.score.cast::<u8>(), info.nscore as usize),
                slice_or_empty(info.value.cast::<u8>(), info.nbytes as usize),
            )
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

/// One element the engine is still holding for us. Dropping it releases the hold.
pub struct HeldElem<'a> {
    elems: Elems<'a>,
}

impl HeldElem<'_> {
    /// The stored bytes, as they were when the hold was taken.
    pub fn value(&self) -> &[u8] {
        match self.elems.as_slice() {
            Some(items) => self.elems.view(items[0]).1,
            None => &[],
        }
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
