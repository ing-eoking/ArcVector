mod error;
mod kv;

pub use error::StoreError;

use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::engine_api::{ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE, engine_interface_v1};
use crate::handler::arcus::abi;

pub const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;
const DEFAULT_MAX_ITEM_SIZE: u32 = 1024 * 1024;

static ENGINE: AtomicPtr<engine_interface_v1> = AtomicPtr::new(ptr::null_mut());
fn engine() -> *mut engine_interface_v1 {
    let cached = ENGINE.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let server = crate::server::handle();
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
    ///
    /// Anything that takes a key must come from [`Store::background_keyed`],
    /// which is the same store with the cookie proven present.
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

    /// The item size this crate assumes, for the early check in `vcreate`.
    ///
    /// **Not the server's real limit.** The engine answers `get_config` for
    /// `max_element_bytes` and the collection sizes but not for
    /// `item_size_max`, so there is nothing to ask. The authority is the engine
    /// itself: `allocate` returns `ENGINE_E2BIG` when no slab class fits
    /// (`default_engine.c`), which surfaces as `StoreError::TooBig` on the
    /// `vadd` that trips it.
    ///
    /// This constant only catches the obvious case at `vcreate` time, against
    /// the engine's own default. A server started with a smaller `-I` accepts
    /// the `vcreate` and refuses the first `vadd`.
    pub fn max_item_size(&self) -> u32 {
        DEFAULT_MAX_ITEM_SIZE
    }

    /// Reads an index's metadata item.
    pub fn read_meta(
        &self,
        index: &str,
    ) -> Result<
        (
            crate::handler::arcus::element::MetaRecord,
            crate::handler::arcus::element::Layout,
        ),
        StoreError,
    > {
        let raw = self.get_kv(&crate::trigger::key::meta_key(index))?;
        crate::handler::arcus::element::MetaRecord::decode(&raw)
            .map_err(|_| StoreError::CorruptElement)
    }

    /// Writes an index's metadata item, refusing to replace one already there.
    ///
    /// `add` rather than `set`: `vcreate` has to answer `EXISTS` for a name
    /// already taken, and asking the engine is one round trip where a read
    /// followed by a write is two and races.
    pub fn add_meta(
        &self,
        index: &str,
        meta: &crate::handler::arcus::element::MetaRecord,
        layout: crate::handler::arcus::element::Layout,
    ) -> Result<(), StoreError> {
        // `add_kv` keeps the reference `allocate` took. Nothing holds metadata
        // the way the graph holds a vector, so it goes straight back.
        let addr = self.add_kv(&crate::trigger::key::meta_key(index), &meta.encode(layout))?;
        self.release_items(&[addr]);
        Ok(())
    }
}

/// How the graph reaches the item behind an address.
///
/// The graph holds raw item pointers and reads through them with no lock, which
/// the reference the link hook handed over makes sound. Releasing is the only
/// call here that takes the cache lock, and it must not run while the graph
/// lock is held -- `AnnIndex::reclaim` is the one caller, and its doc says why.
pub struct ItemElements;

impl crate::handler::usearch::Elements for ItemElements {
    fn id_at(&self, addr: u64) -> Option<std::sync::Arc<str>> {
        let store = Store::background()?;
        store.with_item_at(addr, |key, _value| {
            crate::trigger::key::parse(key).map(|p| std::sync::Arc::from(p.id))
        })?
    }

    fn release(&self, addrs: &[u64]) {
        if let Some(store) = Store::background() {
            store.release_items(addrs);
        }
    }
}
