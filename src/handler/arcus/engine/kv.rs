use std::os::raw::c_void;
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{
    ENGINE_STORE_OPERATION_OPERATION_ADD, ENGINE_STORE_OPERATION_OPERATION_SET, item, item_info,
};

impl Store {
    /// Reads a top-level key.
    ///
    /// Only `AV OWNER` uses this. Index data lives in Maps, which `elem.rs`
    /// reaches; a plain key is how one fact is published to a whole
    /// replication group without belonging to any index.
    pub fn get_kv(&self, key: &str) -> Result<Vec<u8>> {
        let vt = self.vtable();
        let (Some(get), Some(release), Some(info_of)) = (vt.get, vt.release, vt.get_item_info)
        else {
            return Err(StoreError::Unavailable);
        };

        let mut it: *mut item = ptr::null_mut();
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                ptr::from_mut(&mut it),
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                0, // vbucket
            )
        };
        check(code)?;
        if it.is_null() {
            return Err(StoreError::KeyGone);
        }

        let mut info: item_info = unsafe { std::mem::zeroed() };
        let ok = unsafe { info_of(self.handle(), self.cookie, it, ptr::from_mut(&mut info)) };
        let out = if ok && !info.value.is_null() && info.naddnl == 0 {
            let len = info.nbytes as usize;
            Ok(unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), len).to_vec() })
        } else {
            Err(StoreError::CorruptElement)
        };
        unsafe { release(self.handle(), self.cookie, it) };
        out
    }

    /// Writes a top-level key, replacing whatever was there.
    ///
    /// `OPERATION_SET` rather than `ADD`. `ADD` refuses an existing key
    /// (`ENGINE_NOT_STORED`, `do_item_store_add`), which matters here because
    /// the value a promoted node has to replace is the *previous* master's,
    /// replicated in before the switchover. Emulating a replace with a remove
    /// and an add leaves the key absent in between -- and a slave attaching in
    /// that gap reads nothing, which is the one situation this key exists for.
    ///
    /// **Only ever reached from a worker thread, with that thread's own
    /// cookie**, because a keyed write stamps `last_cset_seqs[thr_idx]` --
    /// unlocked, indexed by worker thread, safe only under memcached's own
    /// one-write-per-worker-thread invariant. A background thread calling this
    /// with a borrowed cookie breaks that invariant and can cost a real
    /// client its replication wait. `repl::role` therefore does not call this;
    /// it sends `vowner` over `crate::attach`'s connection and the handler
    /// calls it here, on the worker thread the daemon picked.
    ///
    /// The allocate runs the replication gate under this key's own name, so on
    /// a replica this fails with `ENGINE_REPL_SLAVE` before anything is
    /// allocated.
    ///
    /// Both engine calls go through `check`, so `ENGINE_EWOULDBLOCK` is an
    /// acceptance, not an error. On a **sync**-replication master
    /// `rp_after_check` -> `rp_wait` returns it whenever a slave has not caught
    /// up with this write's cset yet (`engines/default/replication.c`): the
    /// gate let the write through and the daemon will notify the cookie's
    /// connection when the slave acknowledges. Reading that as an error made
    /// `repl::role::probe_once` classify an ordinary sync master as
    /// `Probe::Refused`, so it demoted itself, closed its listener, dropped its
    /// replicas and re-promoted on the next beat -- over and over. The allocate
    /// path already used `check` for exactly this reason; only the store call
    /// compared against `ENGINE_SUCCESS` by hand.
    ///
    /// **Keeps the reference `allocate` took** and hands back the item address,
    /// so the graph can read through it with no lock -- the same contract as
    /// [`Store::hold_kv`]. Give it back with [`Store::release_items`]. A failed
    /// store releases before returning.
    pub fn set_kv(&self, key: &str, value: &[u8]) -> Result<u64> {
        self.store_kv(key, value, ENGINE_STORE_OPERATION_OPERATION_SET)
    }

    /// Writes a key only if it is not there, answering `NotStored` when it is.
    ///
    /// `vcreate` needs that distinction and nothing else does: asking the
    /// engine is one round trip where a read-then-write is two and races.
    ///
    /// Owns its reference on success, like [`Store::set_kv`].
    pub fn add_kv(&self, key: &str, value: &[u8]) -> Result<u64> {
        self.store_kv(key, value, ENGINE_STORE_OPERATION_OPERATION_ADD)
    }

    fn store_kv(&self, key: &str, value: &[u8], operation: u32) -> Result<u64> {
        let vt = self.vtable();
        let (Some(allocate), Some(store), Some(release), Some(info_of)) =
            (vt.allocate, vt.store, vt.release, vt.get_item_info)
        else {
            return Err(StoreError::Unavailable);
        };

        let mut it: *mut item = ptr::null_mut();
        let code = unsafe {
            allocate(
                self.handle(),
                self.cookie,
                ptr::from_mut(&mut it),
                key.as_ptr().cast::<c_void>(),
                key.len(),
                value.len(),
                0, // flags
                0, // exptime: never
                0, // cas
            )
        };
        check(code)?;

        let mut info: item_info = unsafe { std::mem::zeroed() };
        if !unsafe { info_of(self.handle(), self.cookie, it, ptr::from_mut(&mut info)) } {
            unsafe { release(self.handle(), self.cookie, it) };
            return Err(StoreError::CorruptElement);
        }
        let room = info.nbytes as usize;
        if room < value.len() || info.naddnl != 0 || info.value.is_null() {
            unsafe { release(self.handle(), self.cookie, it) };
            return Err(StoreError::CorruptElement);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                value.as_ptr(),
                info.value.cast::<u8>().cast_mut(),
                value.len(),
            );
        }

        let mut cas: u64 = 0;
        let code = unsafe {
            store(
                self.handle(),
                self.cookie,
                it,
                ptr::from_mut(&mut cas),
                operation,
                0, // vbucket
            )
        };
        if let Err(err) = check(code) {
            unsafe { release(self.handle(), self.cookie, it) };
            return Err(err);
        }
        Ok(it as u64)
    }

    /// Reads an item's key and value in place, taking no lock and no reference.
    ///
    /// `get_item_info` is a pure field read in the default engine, so this is
    /// the **only** engine call that may be made from inside the
    /// `ON_ITEM_TRIGER` callback -- everything else goes through
    /// `LOCK_CACHE()`, which the callback already holds.
    ///
    /// The caller must already own a reference to the item: the one the link
    /// hook handed over, or one from [`Store::hold_kv`]. Nothing here takes one,
    /// so the borrow is only valid for the duration of `f`.
    pub fn with_item_at<T>(&self, addr: u64, f: impl FnOnce(&[u8], &[u8]) -> T) -> Option<T> {
        let info_of = self.vtable().get_item_info?;
        let it = addr as *mut item;
        if it.is_null() {
            return None;
        }

        let mut info: item_info = unsafe { std::mem::zeroed() };
        if !unsafe { info_of(self.handle(), self.cookie, it, ptr::from_mut(&mut info)) } {
            return None;
        }
        if info.key.is_null() || info.value.is_null() || info.naddnl != 0 {
            return None;
        }
        let key = unsafe { std::slice::from_raw_parts(info.key.cast::<u8>(), info.nkey as usize) };
        let value =
            unsafe { std::slice::from_raw_parts(info.value.cast::<u8>(), info.nbytes as usize) };
        Some(f(key, value))
    }

    /// Fetches a key and **keeps** the reference `get` took, handing back the
    /// item pointer as an address.
    ///
    /// The graph stores that address and reads through it with no lock; the
    /// reference is what makes that sound. Give it back with
    /// [`Store::release_items`] -- and never from a path that holds the graph
    /// lock, because `release` takes the cache lock.
    pub fn hold_kv(&self, key: &str) -> Result<u64> {
        let Some(get) = self.vtable().get else {
            return Err(StoreError::Unavailable);
        };

        let mut it: *mut item = ptr::null_mut();
        let code = unsafe {
            get(
                self.handle(),
                self.cookie,
                ptr::from_mut(&mut it),
                key.as_ptr().cast::<c_void>(),
                as_int(key.len()),
                0, // vbucket
            )
        };
        check(code)?;
        if it.is_null() {
            return Err(StoreError::KeyGone);
        }
        Ok(it as u64)
    }

    /// Gives back references taken by [`Store::hold_kv`], or handed over by the
    /// link hook.
    ///
    /// Takes the cache lock, so this must not run while the graph lock is held.
    /// The sweeper is the only caller that should exist; everything else queues
    /// through `access::sweep::release_later`.
    pub fn release_items(&self, addrs: &[u64]) {
        let Some(release) = self.vtable().release else {
            return;
        };
        for addr in addrs {
            let it = *addr as *mut item;
            if !it.is_null() {
                unsafe { release(self.handle(), self.cookie, it) };
            }
        }
    }

    /// Removes a key. `KeyGone` when there was nothing there.
    pub fn delete_kv(&self, key: &str) -> Result<()> {
        let Some(remove) = self.vtable().remove else {
            return Err(StoreError::Unavailable);
        };
        let code = unsafe {
            remove(
                self.handle(),
                self.cookie,
                key.as_ptr().cast::<c_void>(),
                key.len(),
                0, // cas: unconditional
                0, // vbucket
            )
        };
        check(code)
    }

    /// Invalidates every key under `prefix`, effective now.
    ///
    /// The daemon stamps the prefix's `oldest_live` and unlinks part of the LRU
    /// eagerly, leaving the rest to expire when touched -- so this is not a
    /// promise that every unlink callback has already fired. `vdrop` therefore
    /// deletes the metadata key first, which releases the graph outright, and
    /// lets the late callbacks land on an index nothing has registered.
    ///
    /// Takes the prefix as a buffer, so the space-free `arcus_trig{index}`
    /// reaches the engine without passing through ASCII tokenisation.
    pub fn flush_prefix(&self, prefix: &str) -> Result<()> {
        let Some(flush) = self.vtable().flush else {
            return Err(StoreError::Unavailable);
        };
        let code = unsafe {
            flush(
                self.handle(),
                self.cookie,
                prefix.as_ptr().cast::<c_void>(),
                as_int(prefix.len()),
                0, // when: immediately
            )
        };
        check(code)
    }
}
