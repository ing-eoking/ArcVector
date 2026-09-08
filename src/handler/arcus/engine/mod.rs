mod elem;
mod error;
mod kv;
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

/// Null until `crate::attach` parks a connection. Only `background_keyed`
/// insists on a real one.
///
/// A build with neither `migration` nor `replication` never compiles `attach`
/// at all, and has no gate that looks at the cookie, so it is always null here.
#[cfg(not(parked_cookie))]
fn background_cookie() -> *const c_void {
    ptr::null()
}

#[cfg(parked_cookie)]
fn background_cookie() -> *const c_void {
    crate::attach::cookie()
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

    /// A store a background thread may pass a key to, `None` until
    /// `crate::attach` has a connection parked and again while a lost one is
    /// replaced; the caller skips that round.
    ///
    /// The check used to be `#[cfg(feature = "migration")]`, justified by
    /// "without `migration` the server compiles `ACTION_BEFORE_READ` away, so the
    /// null cookie is fine" -- **that justification was wrong**, and nothing like
    /// it should be reinstated. Two separate gates dereference the cookie, and
    /// only one of them is migration's:
    ///
    /// * `ACTION_BEFORE_READ` really is `#ifdef ENABLE_MIGRATION`, and a read of
    ///   a key this node has handed off reaches `set_not_my_key_info`, which
    ///   writes the new owner through the cookie.
    /// * `ACTION_BEFORE_WRITE` is `#if defined(ENABLE_REPLICATION) ||
    ///   defined(ENABLE_MIGRATION)` (`engines/default/default_engine.c`), so a
    ///   `replication`-only build has it too. Its master arm calls
    ///   `do_check_master_switchover_done(cookie)`, which during a switchover
    ///   reaches `set_switchover_node(cookie, ..)` (`c->swover_node[0] = ..`) or
    ///   `get_thread_index(cookie)` (`c->thread->index`) -- both unguarded. Only
    ///   the slave arm handles a null cookie.
    ///
    /// No background *write* goes through here any more -- `repl::role` sends
    /// the owner key over `crate::attach`'s connection instead, so the daemon
    /// runs it on the worker thread that owns that connection. The null check
    /// stays regardless: the reads that remain (`recovery::run_builder`,
    /// `access::sweep::probe_round`, the replica's `slave` resolves) run the
    /// migration gate, and nothing stops a future caller from adding a write
    /// back. Outside a switchover a null cookie happens to survive the write
    /// gate (`WTHREAD_SET_LAST_CSET_SEQ` is `if (cookie)`-guarded); the first
    /// ZK-driven switchover is where it would take the daemon down.
    ///
    /// `cfg(parked_cookie)` -- `migration || replication` -- is exactly the set
    /// of builds where one of those two gates is compiled into the server, and
    /// also exactly the set where `attach` is compiled and so a cookie can ever
    /// arrive. A build with neither has no gate to trip, and refusing the null
    /// cookie there would refuse *every* keyed background call for the life of
    /// the process: `recovery::run_builder` would requeue every rebuild
    /// forever, so a `persistence`-only node could never rebuild a graph from a
    /// Map that outlived it, and `access::sweep::probe_round` would never probe.
    /// The `abi::verify` check is what makes the correspondence sound -- a crate
    /// built without these features cannot attach to a server that has them,
    /// because the added vtable members change the member count it compares.
    pub fn background_keyed() -> Option<Self> {
        let store = Self::background()?;
        #[cfg(parked_cookie)]
        if store.cookie.is_null() {
            return None;
        }
        Some(store)
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
}

pub struct DetachedElements;

impl crate::handler::usearch::Elements for DetachedElements {
    fn id_at(&self, addr: u64) -> Option<std::sync::Arc<str>> {
        Store::background()?.id_at(addr)
    }

    fn release(&self, addrs: &[u64]) {
        if let Some(store) = Store::background() {
            store.release_held(addrs);
        }
    }
}
