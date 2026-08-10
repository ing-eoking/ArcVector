//! arcus Map engine wrapper — the source of truth for every vector.
//!
//! One index is one Map item (key = index name); one vector is one Map element
//! (field = vector id, value = the [`crate::codec`] layout). Map is used rather
//! than plain KV items because key-based lookup, replication and TTL already ride
//! on the Map path.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::codec::Layout;
use crate::engine_api::*;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Collection type id for Map, as the engine's `get_elem_info` expects it.
const ITEM_TYPE_MAP: c_int = 3;

pub const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;

static ENGINE: AtomicPtr<engine_interface_v1> = AtomicPtr::new(ptr::null_mut());
static GET_SERVER_API_FN: OnceLock<unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1> =
    OnceLock::new();

pub fn set_server_api(f: unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1) {
    let _ = GET_SERVER_API_FN.set(f);
}

/// Resolve (and cache) the engine handle. The engine is not available at
/// extension-initialize time, so this is resolved lazily on first use.
pub fn ensure_engine() -> *mut engine_interface_v1 {
    let cached = ENGINE.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let Some(get_api) = GET_SERVER_API_FN.get() else {
        return ptr::null_mut();
    };
    unsafe {
        let server = get_api();
        if server.is_null() {
            return ptr::null_mut();
        }
        let engine_ptr = (*server).engine;
        if engine_ptr.is_null() {
            return ptr::null_mut();
        }
        let eng = engine_ptr as *mut engine_interface_v1;
        // Racing resolvers must agree on one pointer.
        match ENGINE.compare_exchange(ptr::null_mut(), eng, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => eng,
            Err(existing) => existing,
        }
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
    /// Engine returned a code we do not translate individually.
    Engine(u32),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unavailable => write!(f, "engine unavailable"),
            StoreError::KeyGone => write!(f, "index not found in engine"),
            StoreError::ElemGone => write!(f, "element not found"),
            StoreError::Overflow => write!(f, "index is full"),
            StoreError::Engine(c) => write!(f, "engine error {c}"),
        }
    }
}

fn translate(code: u32) -> StoreError {
    match code {
        ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT => StoreError::KeyGone,
        ENGINE_ERROR_CODE_ENGINE_ELEM_ENOENT => StoreError::ElemGone,
        ENGINE_ERROR_CODE_ENGINE_EOVERFLOW => StoreError::Overflow,
        other => StoreError::Engine(other),
    }
}

macro_rules! vtable {
    ($eng:expr, $field:ident) => {
        match (*$eng).$field {
            Some(f) => f,
            None => return Err(StoreError::Unavailable),
        }
    };
}

/// Read a `uint32` engine configuration value, falling back to `default`.
fn config_u32(cookie: *const c_void, key: &std::ffi::CStr, default: u32) -> u32 {
    let eng = ensure_engine();
    if eng.is_null() {
        return default;
    }
    unsafe {
        let Some(get_config) = (*eng).get_config else {
            return default;
        };
        let mut out: u32 = 0;
        let ret = get_config(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr(),
            &mut out as *mut u32 as *mut c_void,
        );
        if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS && out > 0 { out } else { default }
    }
}

pub fn max_element_bytes(cookie: *const c_void) -> u32 {
    config_u32(cookie, c"max_element_bytes", DEFAULT_MAX_ELEMENT_BYTES)
}

pub fn max_map_size(cookie: *const c_void) -> u32 {
    config_u32(cookie, c"max_map_size", 50_000)
}

/// Create the Map item backing an index.
///
/// `maxcount` and `exptime` are the index's element-count limit and TTL — the
/// Map constraints the index inherits.
pub fn create_map(
    cookie: *const c_void,
    key: &str,
    maxcount: Option<u32>,
    exptime: Option<u32>,
) -> Result<(), StoreError> {
    let eng = ensure_engine();
    if eng.is_null() {
        return Err(StoreError::Unavailable);
    }
    unsafe {
        let create = vtable!(eng, map_struct_create);
        let mut attr = std::mem::zeroed::<item_attr>();
        attr.readable = 1;
        if let Some(m) = maxcount {
            attr.maxcount = m as i32;
        }
        if let Some(e) = exptime {
            attr.exptime = e;
        }
        let ret = create(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr() as *const c_void,
            key.len() as c_int,
            &mut attr,
            0,
        );
        if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            Ok(())
        } else {
            Err(translate(ret))
        }
    }
}

/// Delete the whole Map item — this is how an index is dropped.
pub fn drop_map(cookie: *const c_void, key: &str) -> Result<(), StoreError> {
    let eng = ensure_engine();
    if eng.is_null() {
        return Err(StoreError::Unavailable);
    }
    unsafe {
        let remove = vtable!(eng, remove);
        let ret = remove(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr() as *const c_void,
            key.len(),
            0,
            0,
        );
        if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            Ok(())
        } else {
            Err(translate(ret))
        }
    }
}

/// Insert or replace one element.
pub fn put_elem(
    cookie: *const c_void,
    key: &str,
    field: &str,
    value: &[u8],
) -> Result<(), StoreError> {
    let eng = ensure_engine();
    if eng.is_null() {
        return Err(StoreError::Unavailable);
    }
    unsafe {
        let alloc = vtable!(eng, map_elem_alloc);
        let insert = vtable!(eng, map_elem_insert);
        let elem_free = vtable!(eng, map_elem_free);
        let elem_info = vtable!(eng, get_elem_info);

        let mut eitem_ptr: *mut eitem = ptr::null_mut();
        let ret = alloc(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr() as *const c_void,
            key.len() as c_int,
            field.len(),
            value.len(),
            &mut eitem_ptr,
        );
        if ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            return Err(translate(ret));
        }

        let mut info = std::mem::zeroed::<eitem_info>();
        elem_info(eng as *mut ENGINE_HANDLE, cookie, ITEM_TYPE_MAP, eitem_ptr, &mut info);
        if info.score.is_null() || info.value.is_null() {
            elem_free(eng as *mut ENGINE_HANDLE, cookie, eitem_ptr);
            return Err(StoreError::Unavailable);
        }
        ptr::copy_nonoverlapping(field.as_ptr(), info.score as *mut u8, field.len());
        ptr::copy_nonoverlapping(value.as_ptr(), info.value as *mut u8, value.len());

        let mut replaced = false;
        let mut created = false;
        let ret = insert(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr() as *const c_void,
            key.len() as c_int,
            eitem_ptr,
            true, // replace_if_exist
            ptr::null_mut(),
            &mut replaced,
            &mut created,
            0,
        );
        if ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            elem_free(eng as *mut ENGINE_HANDLE, cookie, eitem_ptr);
            return Err(translate(ret));
        }
        Ok(())
    }
}

pub fn delete_elem(cookie: *const c_void, key: &str, field: &str) -> Result<(), StoreError> {
    let eng = ensure_engine();
    if eng.is_null() {
        return Err(StoreError::Unavailable);
    }
    unsafe {
        let delete = vtable!(eng, map_elem_delete);
        let f = field_t { value: field.as_ptr() as *mut c_char, length: field.len() };
        let mut del_count: u32 = 0;
        let mut dropped = false;
        let ret = delete(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr() as *const c_void,
            key.len() as c_int,
            1,
            &f,
            false, // drop_if_empty: an emptied index must keep existing
            &mut del_count,
            &mut dropped,
            0,
        );
        if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS && del_count > 0 {
            Ok(())
        } else if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            Err(StoreError::ElemGone)
        } else {
            Err(translate(ret))
        }
    }
}

/// Run `f` over the raw values of the requested elements, then release them.
///
/// `map_elem_get` mallocs its result array even for a single field
/// (coll_map.c:934) and hands back refcounted element pointers, so every early
/// return has to go through the same release path. Callers get borrowed slices
/// and must copy anything they need to keep.
fn with_elems<T>(
    cookie: *const c_void,
    key: &str,
    field: Option<&str>,
    f: impl FnOnce(&[(&[u8], &[u8])]) -> T,
) -> Result<T, StoreError> {
    let eng = ensure_engine();
    if eng.is_null() {
        return Err(StoreError::Unavailable);
    }
    unsafe {
        let get = vtable!(eng, map_elem_get);
        let release = vtable!(eng, map_elem_release);
        let elem_info = vtable!(eng, get_elem_info);

        let fld = field.map(|s| field_t {
            value: s.as_ptr() as *mut c_char,
            length: s.len(),
        });
        let (numfields, flist) = match &fld {
            Some(f) => (1, f as *const field_t),
            None => (0, ptr::null()),
        };

        let mut eresult = std::mem::zeroed::<elems_result>();
        let ret = get(
            eng as *mut ENGINE_HANDLE,
            cookie,
            key.as_ptr() as *const c_void,
            key.len() as c_int,
            numfields,
            flist,
            false, // delete
            false, // drop_if_empty
            &mut eresult,
            0,
        );

        if ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS
            || eresult.elem_count == 0
            || eresult.elem_array.is_null()
        {
            if !eresult.elem_array.is_null() {
                release(
                    eng as *mut ENGINE_HANDLE,
                    cookie,
                    eresult.elem_array,
                    eresult.elem_count as c_int,
                );
                free(eresult.elem_array as *mut c_void);
            }
            return Err(if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
                StoreError::ElemGone
            } else {
                translate(ret)
            });
        }

        let n = eresult.elem_count as usize;
        let mut views: Vec<(&[u8], &[u8])> = Vec::with_capacity(n);
        for i in 0..n {
            let elem = *eresult.elem_array.add(i);
            let mut info = std::mem::zeroed::<eitem_info>();
            elem_info(eng as *mut ENGINE_HANDLE, cookie, ITEM_TYPE_MAP, elem, &mut info);
            let field_bytes = if info.score.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(info.score as *const u8, info.nscore as usize)
            };
            let value_bytes = if info.value.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(info.value as *const u8, info.nbytes as usize)
            };
            views.push((field_bytes, value_bytes));
        }

        let out = f(&views);

        release(
            eng as *mut ENGINE_HANDLE,
            cookie,
            eresult.elem_array,
            eresult.elem_count as c_int,
        );
        free(eresult.elem_array as *mut c_void);
        Ok(out)
    }
}

/// Copy the fixed filter slot of one element — the search predicate's hot path.
///
/// The slot lives at a constant offset ([`crate::codec::FILTER_OFFSET`]), so no
/// decoding is needed and only one cache line is touched. The surrounding
/// `map_elem_get` still costs a global `cache_lock` plus a malloc; replacing it
/// with a narrower engine call changes this function's body and nothing else.
pub fn get_filter_slot(
    cookie: *const c_void,
    key: &str,
    field: &str,
    layout: &Layout,
    out: &mut Vec<u8>,
) -> Result<(), StoreError> {
    with_elems(cookie, key, Some(field), |elems| {
        let (_, value) = elems[0];
        out.clear();
        match layout.filter_of(value) {
            Ok(slot) => {
                out.extend_from_slice(slot);
                true
            }
            Err(_) => false,
        }
    })
    .and_then(|ok| if ok { Ok(()) } else { Err(StoreError::ElemGone) })
}

/// Read one element's full value.
pub fn get_elem(cookie: *const c_void, key: &str, field: &str) -> Result<Vec<u8>, StoreError> {
    with_elems(cookie, key, Some(field), |elems| elems[0].1.to_vec())
}

/// Read every element — used to rebuild the usearch index from Map.
pub fn get_all(cookie: *const c_void, key: &str) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
    match with_elems(cookie, key, None, |elems| {
        elems
            .iter()
            .map(|(f, v)| (String::from_utf8_lossy(f).into_owned(), v.to_vec()))
            .collect::<Vec<_>>()
    }) {
        Ok(v) => Ok(v),
        // An index with no elements yet is empty, not missing.
        Err(StoreError::ElemGone) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}
