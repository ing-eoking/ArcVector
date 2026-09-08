use std::os::raw::c_void;
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{ENGINE_STORE_OPERATION_OPERATION_SET, item, item_info};

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

    /// Writes a top-level key, creating it.
    ///
    /// The allocate runs the replication gate under this key's own name, so on
    /// a replica this fails with `ENGINE_REPL_SLAVE` before anything is
    /// allocated. That refusal is what `repl::role` reads as the answer to
    /// "am I the master".
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
    pub fn set_kv(&self, key: &str, value: &[u8]) -> Result<()> {
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
                ENGINE_STORE_OPERATION_OPERATION_SET,
                0, // vbucket
            )
        };
        unsafe { release(self.handle(), self.cookie, it) };
        check(code)
    }
}
