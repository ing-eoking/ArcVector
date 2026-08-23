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

const ITEM_TYPE_MAP: c_int = 3;

struct Published {
    created: bool,

    replaced: bool,
}

#[must_use = "an uninserted element is invisible; insert it or drop it"]
pub struct PendingElem<'a> {
    store: &'a Store,

    key: &'a str,
    item: *mut eitem,

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

        unsafe { elem_free(self.store.handle(), self.store.cookie, self.item) };
    }
}

impl PendingElem<'_> {
    pub fn addr(&self) -> u64 {
        self.item as u64
    }

    pub fn value_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.value, self.value_len) }
    }

    pub fn insert(self) -> Result<bool> {
        self.publish(ptr::null_mut(), true).map(|p| p.replaced)
    }

    pub fn insert_creating(self, mut attr: item_attr) -> Result<bool> {
        self.publish(ptr::from_mut(&mut attr), false)
            .map(|p| p.created)
    }

    fn publish(mut self, attr: *mut item_attr, replace_if_exist: bool) -> Result<Published> {
        let Some(insert) = self.store.vtable().map_elem_insert else {
            return Err(StoreError::Unavailable);
        };
        let mut replaced = false;
        let mut created = false;

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

        check(code)?;

        self.item = ptr::null_mut();
        Ok(Published { created, replaced })
    }
}

impl Store {
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

                ptr::copy_nonoverlapping(b"\r\n".as_ptr(), dst.add(len), 2);
                value = dst;
            }
            ok
        };
        if !filled {
            unsafe { elem_free(self.handle(), self.cookie, item) };

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

    pub fn put_elem(&self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        self.alloc_elem(key, field, value)?.insert().map(|_| ())
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

        let code = unsafe {
            delete(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                1,
                ptr::from_ref(&selector),
                false,
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
        let views: Vec<(&[u8], &[u8])> = items
            .iter()
            .map(|item| elems.view(*item))
            .collect::<Result<_>>()?;
        Ok(f(&views))
    }

    pub fn take_elem(&self, key: &str, field: &str) -> Result<Vec<u8>> {
        self.fetch_with(key, Some(field), true)
            .and_then(|elems| match elems.as_slice() {
                Some(items) => Ok(elems.view(items[0])?.1.to_vec()),
                None => Err(StoreError::ElemGone),
            })
    }

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

        let mut result: elems_result = unsafe { std::mem::zeroed() };

        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                numfields,
                flist,
                delete,
                false,
                ptr::from_mut(&mut result),
                0,
            )
        };

        let elems = unsafe { Elems::new(self, &result) };
        check(code)?;
        Ok(elems)
    }

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

    pub fn get_attr(&self, key: &str, field: &str, layout: Layout) -> Result<Vec<u8>> {
        let held = self.hold_elem(key, field)?;
        layout
            .attr_of(held.value())
            .map(<[u8]>::to_vec)
            .map_err(|_| StoreError::CorruptElement)
    }

    pub fn hold_addr(&self, key: &str, field: &str) -> Result<HeldAddr<'_>> {
        let elems = self.fetch(key, Some(field))?;
        let addr = match elems.as_slice() {
            Some(items) => items[0] as u64,
            None => return Err(StoreError::ElemGone),
        };

        std::mem::forget(elems);
        Ok(HeldAddr { store: self, addr })
    }

    pub fn take_addr(&self, key: &str, field: &str) -> Result<Option<HeldAddr<'_>>> {
        match self.fetch_with(key, Some(field), true) {
            Ok(elems) => {
                let addr = match elems.as_slice() {
                    Some(items) => items[0] as u64,
                    None => return Ok(None),
                };
                std::mem::forget(elems);
                Ok(Some(HeldAddr { store: self, addr }))
            }
            Err(StoreError::ElemGone) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn with_attr_at<T>(
        &self,
        addr: u64,
        layout: Layout,
        f: impl FnOnce(&[u8]) -> T,
    ) -> Option<T> {
        let elems = Elems {
            store: self,
            array: ptr::null_mut(),
            count: 0,
        };
        let value = elems.view(addr as *mut eitem).ok()?.1;

        std::mem::forget(elems);
        Some(f(layout.attr_of(value).ok()?))
    }

    pub fn release_held(&self, addrs: &[u64]) {
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

    pub fn id_at(&self, addr: u64) -> Option<std::sync::Arc<str>> {
        let elems = Elems {
            store: self,
            array: ptr::null_mut(),
            count: 0,
        };
        let view = elems.view(addr as *mut eitem).ok()?;
        std::mem::forget(elems);
        Some(std::sync::Arc::from(
            String::from_utf8_lossy(view.0).as_ref(),
        ))
    }

    pub fn hold_all(&self, key: &str) -> Result<HeldMap> {
        let elems = match self.fetch(key, None) {
            Ok(elems) => elems,

            Err(StoreError::ElemGone) => {
                return Ok(HeldMap {
                    array: ptr::null_mut(),
                    count: 0,
                    kept: std::collections::HashSet::new(),
                });
            }
            Err(e) => return Err(e),
        };
        let held = HeldMap {
            array: elems.array,
            count: elems.count,
            kept: std::collections::HashSet::new(),
        };

        std::mem::forget(elems);
        Ok(held)
    }
}

unsafe fn slice_or_empty<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

struct Elems<'a> {
    store: &'a Store,
    array: *mut *mut eitem,
    count: usize,
}

impl<'a> Elems<'a> {
    unsafe fn new(store: &'a Store, result: &elems_result) -> Self {
        Elems {
            store,
            array: result.elem_array,
            count: result.elem_count as usize,
        }
    }

    fn view(&self, item: *mut eitem) -> Result<(&'a [u8], &'a [u8])> {
        let Some(elem_info) = self.store.vtable().get_elem_info else {
            return Err(StoreError::Unavailable);
        };

        let info = unsafe {
            let mut info: eitem_info = std::mem::zeroed();
            elem_info(
                self.store.handle(),
                self.store.cookie,
                ITEM_TYPE_MAP,
                item,
                ptr::from_mut(&mut info),
            );
            info
        };
        let field = info.score.cast::<u8>();
        let value = info.value.cast::<u8>();
        let (nfield, nvalue) = (info.nscore as usize, info.nbytes as usize);

        let sane = !field.is_null()
            && !value.is_null()
            && info.naddnl == 0
            && nfield <= u8::MAX as usize
            && nvalue <= u16::MAX as usize
            && nvalue <= self.store.max_element_bytes() as usize
            && std::ptr::eq(unsafe { field.add(nfield) }, value);
        if !sane {
            return Err(StoreError::CorruptElement);
        }

        unsafe { Ok((slice_or_empty(field, nfield), slice_or_empty(value, nvalue))) }
    }

    fn as_slice(&self) -> Option<&[*mut eitem]> {
        if self.array.is_null() || self.count == 0 {
            return None;
        }

        Some(unsafe { std::slice::from_raw_parts(self.array, self.count) })
    }
}

impl Drop for Elems<'_> {
    fn drop(&mut self) {
        if self.array.is_null() {
            return;
        }
        if let Some(release) = self.store.vtable().map_elem_release {
            unsafe {
                release(
                    self.store.handle(),
                    self.store.cookie,
                    self.array,
                    self.count as c_int,
                );
            }
        }

        unsafe { free(self.array.cast::<c_void>()) };
    }
}

pub struct HeldMap {
    array: *mut *mut eitem,
    count: usize,

    kept: std::collections::HashSet<u64>,
}

unsafe impl Send for HeldMap {}

impl HeldMap {
    pub fn read(&self, store: &Store) -> Vec<(u64, Vec<u8>, Vec<u8>)> {
        let Some(items) = self.as_slice() else {
            return Vec::new();
        };
        let elems = Elems {
            store,
            array: self.array,
            count: self.count,
        };
        let views = items
            .iter()
            .filter_map(|item| elems.view(*item).ok().map(|v| (*item as u64, v)))
            .map(|(addr, (field, value))| (addr, field.to_vec(), value.to_vec()))
            .collect();

        std::mem::forget(elems);
        views
    }

    pub fn keep(&mut self, addr: u64) -> bool {
        if self.kept.try_reserve(1).is_err() {
            return false;
        }
        self.kept.insert(addr);
        true
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn as_slice(&self) -> Option<&[*mut eitem]> {
        if self.array.is_null() || self.count == 0 {
            return None;
        }

        Some(unsafe { std::slice::from_raw_parts(self.array, self.count) })
    }
}

impl Drop for HeldMap {
    fn drop(&mut self) {
        if self.array.is_null() {
            return;
        }
        let Some(store) = Store::detached() else {
            return;
        };

        let mut releasing = 0usize;
        if self.count > 0 {
            let items = unsafe { std::slice::from_raw_parts_mut(self.array, self.count) };
            for i in 0..items.len() {
                if !self.kept.contains(&(items[i] as u64)) {
                    items.swap(releasing, i);
                    releasing += 1;
                }
            }
        }
        if releasing > 0
            && let Some(release) = store.vtable().map_elem_release
        {
            unsafe { release(store.handle(), store.cookie, self.array, releasing as c_int) };
        }

        unsafe { free(self.array.cast::<c_void>()) };
    }
}

#[must_use = "dropping this releases the address the graph is keyed by"]
pub struct HeldAddr<'a> {
    store: &'a Store,
    addr: u64,
}

impl HeldAddr<'_> {
    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn keep(self) -> u64 {
        let addr = self.addr;
        std::mem::forget(self);
        addr
    }
}

impl Drop for HeldAddr<'_> {
    fn drop(&mut self) {
        self.store.release_held(&[self.addr]);
    }
}

pub struct HeldElem<'a> {
    elems: Elems<'a>,
}

impl HeldElem<'_> {
    pub fn addr(&self) -> u64 {
        match self.elems.as_slice() {
            Some(items) => items[0] as u64,
            None => 0,
        }
    }

    pub fn value(&self) -> &[u8] {
        match self.elems.as_slice() {
            Some(items) => self.elems.view(items[0]).unwrap_or((&[], &[])).1,
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

        unsafe {
            assert!(slice_or_empty(ptr::null(), 8).is_empty());
            assert!(slice_or_empty(data.as_ptr(), 0).is_empty());
            assert_eq!(slice_or_empty(data.as_ptr(), 3), &data[..]);
        }
    }
}
