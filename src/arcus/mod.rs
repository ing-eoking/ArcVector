//! Storing vectors in the arcus engine.
//!
//! An index is one Map item; a vector is one Map element. Map was chosen over
//! plain KV items because key-based lookup, replication and TTL already ride on
//! that path, and everything here follows from it:
//!
//! - [`element`] is what one element contains — the scalar kind a coordinate is
//!   stored as, and where those bytes sit after a header and a fixed attribute
//!   region. The shape is arcus's to dictate: the size ceiling is
//!   `max_element_bytes`, and the fixed 144-byte overhead is picked to land well
//!   in arcus's slab classes.
//! - [`engine`] calls the `engine_interface_v1` vtable to read and write them.
//!   The only module in the crate that handles raw pointers.
//!
//! **This side is the source of truth.** [`crate::usearch`] keeps a cache of it
//! that can be rebuilt at any time, which is why the dependency runs one way:
//! the index knows the stored format, the store knows nothing of the index.

pub mod element;
pub mod engine;

pub use element::{Layout, Quant};
pub use engine::{Store, StoreError};
