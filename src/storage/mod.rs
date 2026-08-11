//! The arcus side: how a vector is stored, and where.
//!
//! An index is one Map item; a vector is one Map element. That choice is why these
//! three belong together — each answers part of "what does a stored vector look
//! like, and how does it get there":
//!
//! - [`quantize`] turns `f32` coordinates into the bytes that get stored.
//! - [`element`] lays those bytes out with a header and a fixed attribute region.
//!   The shape is dictated by arcus, not by us: the size limit is
//!   `max_element_bytes`, and the fixed 144-byte overhead is chosen to land well
//!   in arcus's slab classes.
//! - [`map`] calls the engine's `engine_interface_v1` vtable to read and write
//!   them. The only module in the crate that handles raw pointers.
//!
//! Map is the **source of truth**. [`crate::search`] keeps a cache of it that can
//! be rebuilt at any time, so the dependency runs one way: search knows about
//! storage, never the reverse.

pub mod element;
pub mod map;
pub mod quantize;

pub use element::Layout;
pub use map::{Store, StoreError};
