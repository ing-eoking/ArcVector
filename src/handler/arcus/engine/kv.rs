use std::os::raw::c_void;
use std::ptr;

use super::Store;
use super::error::{Result, StoreError, as_int, check};
use crate::engine_api::{
    ENGINE_STORE_OPERATION_OPERATION_ADD, ENGINE_STORE_OPERATION_OPERATION_SET, item, item_info,
    rel_time_t,
};

/// Whether an item's `exptime` says it is already gone, as of `now`.
///
/// The comparison `do_item_isvalid` makes, and deliberately only that one. The
/// engine also refuses an item whose prefix was invalidated and one older than
/// `oldest_live` after a `flush_all`, but both of those are answered by asking
/// the engine for the item, not by reading a field off it -- so a caller that
/// wants the whole verdict calls `get`, and this is for the callers that are
/// already holding the item and cannot afford to.
///
/// `0` means no expiry, and a sticky item carries `rel_time_t::MAX`, which no
/// real `now` reaches. Neither needs a branch of its own.
///
/// A `now` of `0` -- what [`crate::server::current_time`] gives back when the
/// server API is out of reach -- expires nothing. Leaving an expired vector in
/// a result is a smaller wrong than dropping a live one.
pub fn is_expired(exptime: rel_time_t, now: rel_time_t) -> bool {
    exptime != 0 && exptime <= now
}

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
        let addr = self.allocate_kv(key, value)?;
        self.link_allocated(addr, operation)?;
        Ok(addr)
    }

    /// Asks the engine for an item and fills it, **without linking it**.
    ///
    /// Splitting the write in two is what lets the graph be built before the
    /// item is visible: the address is settled here, so the node can be staged
    /// at `addr | STAGED` while nothing can reach the item yet, and
    /// [`Store::link_allocated`] later turns that into a rename rather than an
    /// insert. The expensive half then happens with no lock held at all.
    ///
    /// On success the caller owns the reference and must hand it to
    /// `link_allocated` or [`Store::discard_allocated`].
    ///
    /// **Worker thread only.** `allocate` runs the engine's write hooks, and
    /// replication hangs per-thread state off them.
    pub fn allocate_kv(&self, key: &str, value: &[u8]) -> Result<u64> {
        let vt = self.vtable();
        let (Some(allocate), Some(release), Some(info_of)) =
            (vt.allocate, vt.release, vt.get_item_info)
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
        Ok(it as u64)
    }

    /// Links an item [`Store::allocate_kv`] prepared.
    ///
    /// This is the call that fires the link event, so it is also the call that
    /// turns a staged node into a real one. The reference goes with it either
    /// way: on failure the item is released here.
    ///
    /// **Worker thread only**, for the same reason as `allocate_kv`.
    pub fn link_allocated(&self, addr: u64, operation: u32) -> Result<()> {
        let vt = self.vtable();
        let (Some(store), Some(release)) = (vt.store, vt.release) else {
            return Err(StoreError::Unavailable);
        };
        let it = addr as *mut item;

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
        Ok(())
    }

    /// Gives back an item that was allocated but never linked.
    ///
    /// Nothing ever saw it, so this is a plain release -- no unlink event
    /// follows and no graph node is involved.
    pub fn discard_allocated(&self, addr: u64) {
        if let Some(release) = self.vtable().release {
            let it = addr as *mut item;
            if !it.is_null() {
                unsafe { release(self.handle(), self.cookie, it) };
            }
        }
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
        self.with_item_info(addr, |_exptime, key, value| f(key, value))
    }

    /// [`Store::with_item_at`], with the verdict on the item's expiry handed to
    /// the closure alongside its key and value.
    ///
    /// Expired items are not simply hidden: the caller still gets the key, which
    /// is the only thing that lets a search note one down for cleaning up later.
    /// An address would not do -- it stops meaning anything the moment the graph
    /// lets go of the node -- and the key is right here, in the header the
    /// expiry was read from.
    ///
    /// The whole thing costs nothing over the plain read: `get_item_info` fills
    /// `exptime` whether or not anyone looks at it, and the field is in the same
    /// 64-byte `hash_item` header the key and value pointers come from. There is
    /// no second dereference and no lock.
    ///
    /// It is a narrower verdict than the engine's, all the same. `flush_all` and
    /// an invalidated prefix leave `exptime` untouched and are caught only by
    /// [`Store::get_kv`] or [`Store::touch_kv`], which let `do_item_isvalid`
    /// answer in full. Use this where an item is already in hand and the cache
    /// lock is not affordable; use those at a command's entrance, once.
    ///
    /// Take `now` from [`crate::server::current_time`] once per command, never
    /// per item.
    pub fn with_item_as_of<T>(
        &self,
        addr: u64,
        now: rel_time_t,
        f: impl FnOnce(bool, &[u8], &[u8]) -> T,
    ) -> Option<T> {
        self.with_item_info(addr, |exptime, key, value| {
            f(is_expired(exptime, now), key, value)
        })
    }

    /// Reads an item's header in place and hands the closure what it holds.
    ///
    /// The one place the `item_info` dance is written down; the two readers
    /// above differ only in what they do with `exptime`.
    fn with_item_info<T>(
        &self,
        addr: u64,
        f: impl FnOnce(rel_time_t, &[u8], &[u8]) -> T,
    ) -> Option<T> {
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
        Some(f(info.exptime, key, value))
    }

    /// Asks the engine for a key and gives the reference straight back.
    ///
    /// Taken for the side effect, not the value. `get` runs `do_item_isvalid`,
    /// and an item that fails it is unlinked **there**, inside the engine --
    /// which is what turns a lazily expired item, one flushed by `flush_all`,
    /// or one under an invalidated prefix into an `EVENT_UNLINK` this crate
    /// hears. Nothing else makes those three visible: they leave the item
    /// linked and its `exptime` untouched until somebody asks.
    ///
    /// So this is how a command entrance says "is this index still real". It
    /// copies no value -- the meta record can be large and nobody here wants
    /// it.
    ///
    /// Takes the cache lock twice, so it belongs at a command's entrance, once
    /// per command, and never inside a traversal.
    pub fn touch_kv(&self, key: &str) -> Result<()> {
        let addr = self.hold_kv(key)?;
        self.release_items(&[addr]);
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::is_expired;
    use crate::engine_api::rel_time_t;

    /// arcus의 스티키 아이템. `IS_STICKY_EXPTIME`이 보는 그 값이다.
    const STICKY: rel_time_t = rel_time_t::MAX;

    #[test]
    fn exptime_zero_never_expires() {
        assert!(!is_expired(0, 0));
        assert!(!is_expired(0, rel_time_t::MAX - 1));
    }

    #[test]
    fn a_sticky_item_never_expires() {
        assert!(!is_expired(STICKY, 1_000_000));
    }

    #[test]
    fn an_exptime_in_the_past_is_expired() {
        assert!(is_expired(100, 200));
    }

    #[test]
    fn an_exptime_in_the_future_is_not() {
        assert!(!is_expired(300, 200));
    }

    #[test]
    fn the_boundary_counts_as_expired() {
        // `do_item_isvalid`가 `exptime <= current_time`으로 본다. 같은 초에
        // 만료된 것을 살아 있다고 답하면 엔진과 판정이 갈린다.
        assert!(is_expired(200, 200));
    }

    #[test]
    fn a_now_of_zero_expires_nothing() {
        // `get_current_time`을 못 구했을 때의 값. 판정할 수 없으면 거르지
        // 않는다 -- 살아 있는 벡터를 검색에서 빼는 편이 더 나쁘다.
        assert!(!is_expired(100, 0));
        assert!(!is_expired(STICKY, 0));
    }
}
