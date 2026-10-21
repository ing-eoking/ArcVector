//! Connection-specific state held between `accept` and `execute`.
//!
//! memcached provides an extension pointer slot per connection
//! (`store_engine_specific` / `get_engine_specific`), used while receiving bodies.
//!
//! ```text
//! accept   Allocates space to receive body  → Body
//! execute  Extracts body (empties slot)
//! abort    Discards any remaining state
//! ```
//!
//! The slot is accessed only during `accept`, `execute`, and `abort`, all
//! executing on the connection's dedicated worker thread, requiring no locks.

use std::os::raw::c_void;

use crate::ffi::server;
use crate::protocol::nread::Pending;

pub enum ConnState {
    Body(Pending),
}

/// Attaches state to the connection. Discards any previously held state.
pub unsafe fn put(cookie: *const c_void, state: ConnState) -> bool {
    drop(unsafe { take(cookie) });

    let raw = Box::into_raw(Box::new(state));
    if unsafe { server::store_conn_state(cookie, raw.cast::<c_void>()) } {
        return true;
    }
    drop(unsafe { Box::from_raw(raw) });
    false
}

/// Takes and clears the state attached to the connection.
pub unsafe fn take(cookie: *const c_void) -> Option<Box<ConnState>> {
    let data = unsafe { server::take_conn_state(cookie) };
    if data.is_null() {
        return None;
    }
    Some(unsafe { Box::from_raw(data.cast::<ConnState>()) })
}

pub unsafe fn take_body(cookie: *const c_void) -> Option<Pending> {
    let state = unsafe { take(cookie) }?;
    match *state {
        ConnState::Body(body) => Some(body),
    }
}
