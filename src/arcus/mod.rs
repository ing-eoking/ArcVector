//! Storing vectors in the arcus engine — the source of truth.
//!
//! An index is one Map item; a vector is one Map element. [`crate::usearch`]
//! keeps a cache of this side that can be rebuilt at any time, so the dependency
//! runs one way: the index knows the stored format, the store knows nothing of
//! the index.
//!
//! `docs/내부구조.md` §6.

pub mod abi;
pub mod element;
pub mod engine;
pub mod quant;

pub use element::{Layout, Quant};
pub use engine::{Store, StoreError};
