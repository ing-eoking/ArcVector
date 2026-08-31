mod elem;
mod error;
mod map;

pub use elem::{HeldAddr, HeldElem, HeldMap, PendingElem};
pub use error::StoreError;
pub use map::{FORMAT_VERSION, MapProbe, index_attr};

use std::ffi::CStr;
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::engine_api::{ENGINE_ERROR_CODE_ENGINE_SUCCESS, ENGINE_HANDLE, engine_interface_v1};
use crate::handler::arcus::abi;

pub const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;
const DEFAULT_MAX_MAP_SIZE: u32 = 50_000;

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

    pub fn max_map_size(&self) -> u32 {
        self.config_u32(c"max_map_size", DEFAULT_MAX_MAP_SIZE)
    }

    /// Whether this node is a replica, asked by trying to write.
    ///
    /// A master is not told it is a master; only a replica is told it is one, by
    /// refusing the write with `ENGINE_REPL_SLAVE`. So the question is put as a
    /// throwaway `SET` on a key that expires in a second, and the refusal is the
    /// answer. Anything else — including a failure to allocate, which says
    /// nothing about the role — reads as "not a replica".
    pub fn ping_slave(&self) -> bool {
        let key = b"arcus:zk-ping";
        let mut item = ptr::null_mut();

        let allocate = self
            .vtable()
            .allocate
            .expect("allocate function pointer is null");
        let store_fn = self.vtable().store.expect("store function pointer is null");
        let release = self
            .vtable()
            .release
            .expect("release function pointer is null");

        let code = unsafe {
            allocate(
                self.handle(),
                self.cookie,
                &mut item,
                key.as_ptr() as *const c_void,
                key.len(),
                1, // nbytes
                0, // flags
                1, // exptime
                0, // cas
            )
        };

        if code != ENGINE_ERROR_CODE_ENGINE_SUCCESS || item.is_null() {
            return false;
        }

        let mut cas = 0;
        let code = unsafe {
            store_fn(
                self.handle(),
                self.cookie,
                item,
                &mut cas,
                crate::engine_api::ENGINE_STORE_OPERATION_OPERATION_SET,
                0, // vbucket
            )
        };

        unsafe {
            release(self.handle(), self.cookie, item);
        }

        code == crate::handler::arcus::engine::error::ENGINE_REPL_SLAVE
    }
}

pub struct DetachedElements;

impl crate::handler::usearch::Elements for DetachedElements {
    fn id_at(&self, addr: u64) -> Option<std::sync::Arc<str>> {
        Store::detached()?.id_at(addr)
    }

    fn release(&self, addrs: &[u64]) {
        if let Some(store) = Store::detached() {
            store.release_held(addrs);
        }
    }
}
