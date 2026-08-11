//! arcus Map engine access — the source of truth for every vector.
//!
//! One index is one Map item (key = index name); one vector is one Map element
//! (field = vector id, value = the [`crate::codec`] layout). Map is used rather
//! than plain KV items because key-based lookup, replication and TTL already
//! ride on the Map path.
//!
//! All unsafety is concentrated in [`Store::for_cookie`]. Everything downstream
//! takes `&self` and hands out owned or borrowed Rust values, so no other module
//! deals in raw pointers.

use std::ffi::CStr;
use std::fmt;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::arcus::element::Layout;
use crate::engine_api::{
    ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT, ENGINE_ERROR_CODE_ENGINE_EOVERFLOW,
    ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT, ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE,
    SERVER_HANDLE_V1, eitem, eitem_info, elems_result, engine_interface_v1, field_t, item_attr,
};

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Collection type id for Map, as the engine's `get_elem_info` expects it.
const ITEM_TYPE_MAP: c_int = 3;

pub const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;
const DEFAULT_MAX_MAP_SIZE: u32 = 50_000;

static ENGINE: AtomicPtr<engine_interface_v1> = AtomicPtr::new(ptr::null_mut());
static GET_SERVER_API: OnceLock<unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1> = OnceLock::new();

/// Record the server-API accessor handed to the extension at load time.
pub fn set_server_api(f: unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1) {
    let _ = GET_SERVER_API.set(f);
}

/// Resolve and cache the engine handle.
///
/// The engine is not wired up when the extension initializes, so this resolves
/// lazily on first use.
fn engine() -> *mut engine_interface_v1 {
    let cached = ENGINE.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let Some(get_api) = GET_SERVER_API.get() else {
        return ptr::null_mut();
    };
    // SAFETY: memcached handed us this function pointer during extension
    // initialization and it stays valid for the process lifetime.
    let server = unsafe { get_api() };
    if server.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: a non-null SERVER_HANDLE_V1 from memcached is fully initialized.
    let handle = unsafe { (*server).engine }.cast::<engine_interface_v1>();
    if handle.is_null() {
        return ptr::null_mut();
    }
    // Racing resolvers must agree on a single pointer.
    match ENGINE.compare_exchange(ptr::null_mut(), handle, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => handle,
        Err(existing) => existing,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The engine handle or a required vtable entry is missing.
    Unavailable,
    /// The Map item is gone — evicted, expired or dropped.
    KeyGone,
    /// The Map exists but has no such element.
    ElemGone,
    /// Map is full (`maxcount` / `max_map_size`).
    Overflow,
    /// An engine code we do not translate individually.
    Engine(u32),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Unavailable => f.write_str("engine unavailable"),
            StoreError::KeyGone => f.write_str("index not found in engine"),
            StoreError::ElemGone => f.write_str("element not found"),
            StoreError::Overflow => f.write_str("index is full"),
            StoreError::Engine(code) => write!(f, "engine error {code}"),
        }
    }
}

impl std::error::Error for StoreError {}

type Result<T> = std::result::Result<T, StoreError>;

fn translate(code: u32) -> StoreError {
    match code {
        ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT => StoreError::KeyGone,
        ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT => StoreError::ElemGone,
        ENGINE_ERROR_CODE_ENGINE_EOVERFLOW => StoreError::Overflow,
        other => StoreError::Engine(other),
    }
}

/// Length of a key or field as the engine's `int` parameters want it.
///
/// The protocol bounds keys at 250 bytes and fields at `MAX_FIELD_LENG`, so this
/// cannot truncate in practice; saturate rather than wrap if that ever changes.
fn as_int(len: usize) -> c_int {
    c_int::try_from(len).unwrap_or(c_int::MAX)
}

fn check(code: u32) -> Result<()> {
    if code == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        Ok(())
    } else {
        Err(translate(code))
    }
}

/// Engine access bound to one connection.
///
/// Cheap to copy and never stored: it lives only for the extension callback that
/// created it, which is what makes the raw pointers inside it safe to use
/// without synchronization.
#[derive(Clone, Copy)]
pub struct Store {
    engine: *mut engine_interface_v1,
    cookie: *const c_void,
}

impl Store {
    /// # Safety
    ///
    /// `cookie` must be the connection cookie memcached passed to the extension
    /// callback that is currently running, and the returned `Store` must not
    /// outlive that callback.
    pub unsafe fn for_cookie(cookie: *const c_void) -> Option<Store> {
        let engine = engine();
        (!engine.is_null()).then_some(Store { engine, cookie })
    }

    fn handle(&self) -> *mut ENGINE_HANDLE {
        self.engine.cast::<ENGINE_HANDLE>()
    }

    fn vtable(&self) -> &engine_interface_v1 {
        // SAFETY: `for_cookie` rejects a null engine, and the engine outlives
        // every connection.
        unsafe { &*self.engine }
    }

    /// Read a `uint32` engine configuration value, falling back to `default`.
    fn config_u32(&self, key: &CStr, default: u32) -> u32 {
        let Some(get_config) = self.vtable().get_config else {
            return default;
        };
        let mut out: u32 = 0;
        // SAFETY: `out` is a live u32, which is what the engine writes for a
        // uint32-typed configuration key.
        let code = unsafe {
            get_config(
                self.handle(),
                self.cookie,
                key.as_ptr(),
                ptr::from_mut(&mut out).cast::<c_void>(),
            )
        };
        if code == ENGINE_ERROR_CODE_ENGINE_SUCCESS && out > 0 {
            out
        } else {
            default
        }
    }

    /// Per-element size limit. This is what caps an index's dimension.
    pub fn max_element_bytes(&self) -> u32 {
        self.config_u32(c"max_element_bytes", DEFAULT_MAX_ELEMENT_BYTES)
    }

    /// Default element-count limit for a new Map.
    pub fn max_map_size(&self) -> u32 {
        self.config_u32(c"max_map_size", DEFAULT_MAX_MAP_SIZE)
    }

    /// Create the Map item backing an index.
    ///
    /// `maxcount` and `exptime` are the element-count limit and TTL the index
    /// inherits from Map.
    pub fn create_map(&self, key: &str, maxcount: Option<u32>, exptime: Option<u32>) -> Result<()> {
        let Some(create) = self.vtable().map_struct_create else {
            return Err(StoreError::Unavailable);
        };
        // SAFETY: all-zero is a valid item_attr; the engine reads the fields we
        // set plus the zeroed defaults.
        let mut attr: item_attr = unsafe { std::mem::zeroed() };
        attr.readable = 1;
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

    /// Insert or replace one element.
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
                value.len(),
                ptr::from_mut(&mut item),
            )
        })?;

        // SAFETY: `item` came from a successful alloc, so its field and value
        // regions exist with exactly the sizes requested above.
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
                ptr::copy_nonoverlapping(
                    value.as_ptr(),
                    info.value.cast::<u8>().cast_mut(),
                    value.len(),
                );
            }
            ok
        };
        if !filled {
            // SAFETY: `item` was never inserted, so it is still ours to free.
            unsafe { elem_free(self.handle(), self.cookie, item) };
            return Err(StoreError::Unavailable);
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
            // SAFETY: insert failed, so `item` is still ours to free.
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
    ///
    /// `map_elem_get` mallocs its result array even for a single field
    /// (coll_map.c:934) and returns refcounted element pointers, so [`Elems`]
    /// owns the cleanup and every exit path releases exactly once. Callers get
    /// borrowed slices and must copy whatever they keep.
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

        // Take ownership before the error check so a partial result is still
        // released.
        // SAFETY: `result` is exactly what the call above wrote.
        let elems = unsafe { Elems::new(self, &result) };
        check(code)?;
        let Some(items) = elems.as_slice() else {
            return Err(StoreError::ElemGone);
        };

        let views: Vec<(&[u8], &[u8])> = items
            .iter()
            .map(|item| {
                // SAFETY: each entry is a live Map element held by our refcount,
                // and get_elem_info reports the extents of its own storage.
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
    ///
    /// The region sits at a constant offset ([`super::element::ATTR_OFFSET`]), so
    /// nothing has to be decoded and one or two cache lines are touched. The
    /// surrounding `map_elem_get` still costs a global `cache_lock` and a malloc;
    /// narrowing that is a change to this method's body alone.
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

    /// Read one element's full value.
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
/// `ptr` must be null, or point to `len` initialized bytes that outlive `'a`.
unsafe fn slice_or_empty<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: guaranteed by the caller.
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
    /// `result` must be exactly what a `map_elem_get` call wrote, so `elem_array`
    /// is null or an engine-allocated array of `elem_count` refcounted elements
    /// that has not yet been released.
    unsafe fn new(store: &'a Store, result: &elems_result) -> Elems<'a> {
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
            // SAFETY: we hold the only reference to this array and each entry
            // still carries the refcount `map_elem_get` took.
            unsafe {
                release(
                    self.store.handle(),
                    self.store.cookie,
                    self.array,
                    self.count as c_int,
                );
            }
        }
        // SAFETY: the array itself is plain malloc memory owned by the caller
        // and is not freed anywhere else.
        unsafe { free(self.array.cast::<c_void>()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_codes_map_to_distinguishable_errors() {
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            StoreError::KeyGone
        );
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT),
            StoreError::ElemGone
        );
        assert_eq!(
            translate(ENGINE_ERROR_CODE_ENGINE_EOVERFLOW),
            StoreError::Overflow
        );
        assert_eq!(translate(9999), StoreError::Engine(9999));
    }

    #[test]
    fn check_only_accepts_success() {
        assert!(check(ENGINE_ERROR_CODE_ENGINE_SUCCESS).is_ok());
        assert_eq!(
            check(ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT),
            Err(StoreError::KeyGone)
        );
    }

    #[test]
    fn empty_slices_are_returned_for_null_or_zero_length() {
        let data = [1u8, 2, 3];
        // SAFETY: a null pointer, and a valid pointer with zero length, are both
        // handled without dereferencing; the third case reads 3 live bytes.
        unsafe {
            assert!(slice_or_empty(ptr::null(), 8).is_empty());
            assert!(slice_or_empty(data.as_ptr(), 0).is_empty());
            assert_eq!(slice_or_empty(data.as_ptr(), 3), &data[..]);
        }
    }
}
