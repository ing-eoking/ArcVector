//! Reads and writes the ARCUS engine. An index is one Map; a vector is one of its elements.
//!
//! [`Store`] wraps the engine vtable. `element` defines the element value layout (metadata and
//! vector), and `abi` checks that the server provides the vtable this crate expects.

pub mod abi;
pub mod element;
mod error;
mod map;

pub use error::StoreError;
pub use map::index_attr;

use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::ffi::bindings::{ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE, engine_interface_v1};

pub const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;
const DEFAULT_MAX_MAP_SIZE: u32 = 50_000;

static ENGINE: AtomicPtr<engine_interface_v1> = AtomicPtr::new(ptr::null_mut());
fn engine() -> *mut engine_interface_v1 {
    let cached = ENGINE.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let server = crate::ffi::server::handle();
    if server.is_null() {
        return ptr::null_mut();
    }

    let handle = unsafe { (*server).engine }.cast::<engine_interface_v1>();
    if handle.is_null() {
        return ptr::null_mut();
    }

    if !unsafe { abi::verify(server, &*handle) } {
        return ptr::null_mut();
    }

    match ENGINE.compare_exchange(ptr::null_mut(), handle, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => handle,
        Err(existing) => existing,
    }
}

/// Always null.
///
/// There is no parked connection any more: nothing this crate runs in the
/// background passes a key to the engine. The trigger callback reads items
/// through `get_item_info`, which ignores the cookie, and the sweeper only
/// releases references, which ignores it too.
fn background_cookie() -> *const c_void {
    ptr::null()
}

#[derive(Clone, Copy)]
pub struct Store {
    engine: *mut engine_interface_v1,
    cookie: *const c_void,
}

impl Store {
    pub unsafe fn for_cookie(cookie: *const c_void) -> Option<Self> {
        let engine = engine();
        (!engine.is_null()).then_some(Self { engine, cookie })
    }

    /// A store for work no request asked for, limited to the engine calls that
    /// never look at the cookie: `get_elem_info`, the `*_elem_release` family,
    /// `get_config`. Always available, so a release is never skipped.
    pub fn background() -> Option<Self> {
        let engine = engine();
        (!engine.is_null()).then_some(Self {
            engine,
            cookie: background_cookie(),
        })
    }

    fn handle(&self) -> *mut ENGINE_HANDLE {
        self.engine.cast::<ENGINE_HANDLE>()
    }

    fn vtable(&self) -> &engine_interface_v1 {
        unsafe { &*self.engine }
    }

    fn config_u32(&self, key: &CStr, default: u32) -> u32 {
        let Some(get_config) = self.vtable().get_config else {
            return default;
        };
        let mut out: u32 = 0;

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

    pub fn max_element_bytes(&self) -> u32 {
        self.config_u32(c"max_element_bytes", DEFAULT_MAX_ELEMENT_BYTES)
    }

    /// The most elements one Map may hold on this server, which is also the
    /// largest `maxcount` the engine will honour.
    pub fn max_map_size(&self) -> u32 {
        self.config_u32(c"max_map_size", DEFAULT_MAX_MAP_SIZE)
    }
}

/// How the graph reaches the element behind an address.
///
/// The graph holds raw element pointers and reads through them with no lock,
/// which the reference the link event handed over makes sound. Releasing is
/// the only call here that takes the cache lock, and it must not run while the
/// graph lock is held -- the retirement queue's drain is the one caller, and
/// it releases only after dropping its own lock.
pub struct MapElements;

impl crate::index::Elements for MapElements {
    fn id_at(&self, addr: u64) -> Option<std::sync::Arc<str>> {
        let store = Store::background()?;
        store.with_elem_at(addr, |field, _value| {
            std::str::from_utf8(field).ok().map(std::sync::Arc::from)
        })?
    }

    fn release(&self, addrs: &[u64]) {
        if let Some(store) = Store::background() {
            store.release_elems(addrs);
        }
    }
}
