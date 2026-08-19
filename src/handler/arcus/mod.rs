//! An index is one Map item; a vector is one Map element.

pub mod abi;
pub mod element;
pub mod engine;

pub use element::Layout;
pub use engine::{Store, StoreError};
