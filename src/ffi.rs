//! C ABI boundary: every place where the memcached core and engine call into this crate, and
//! where this crate calls back into them.

pub mod bindings;
pub mod conn_state;
pub mod extension;
pub mod server;
