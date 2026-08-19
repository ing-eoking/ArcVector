//! arcus Map engine access.
//!
//! All unsafety is concentrated in [`Store::for_cookie`]; everything downstream
//! takes `&self` and hands out Rust values. Split into `error` (engine codes),
//! `map` (the Map item) and `elem` (the elements inside it).

mod elem;
mod error;
mod map;

pub use error::StoreError;
pub use map::{INDEX_FLAGS, MapProbe};

use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::engine_api::{ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE, engine_interface_v1};
use crate::handler::arcus::abi;

pub const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;
const DEFAULT_MAX_MAP_SIZE: u32 = 50_000;

static ENGINE: AtomicPtr<engine_interface_v1> = AtomicPtr::new(ptr::null_mut());
/// Resolve and cache the engine handle.
fn engine() -> *mut engine_interface_v1 {
    let cached = ENGINE.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let server = crate::server::handle();
    if server.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: a non-null SERVER_HANDLE_V1 from memcached is fully initialized.
    let handle = unsafe { (*server).engine }.cast::<engine_interface_v1>();
    if handle.is_null() {
        return ptr::null_mut();
    }
    // The first moment the bindings can be checked against the server that loaded
    // us: at registration the engine is not wired up yet.
    // SAFETY: `handle` is the engine's own vtable, non-null and process-lifetime.
    if !unsafe { abi::verify(server, &*handle) } {
        return ptr::null_mut();
    }
    // Racing resolvers must agree on a single pointer.
    match ENGINE.compare_exchange(ptr::null_mut(), handle, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => handle,
        Err(existing) => existing,
    }
}

/// Engine access bound to one connection.
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
    pub unsafe fn for_cookie(cookie: *const c_void) -> Option<Self> {
        let engine = engine();
        (!engine.is_null()).then_some(Self { engine, cookie })
    }

    /// A store with no connection behind it, for the rebuild thread.
    pub fn detached() -> Option<Self> {
        let engine = engine();
        (!engine.is_null()).then_some(Self {
            engine,
            cookie: ptr::null(),
        })
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
}
