//! Vector indexes: the HNSW graph and the registry that looks them up by name.
//!
//! The graph (`graph`) stores vectors in usearch keyed by element address. When an element
//! reference may be handed back to the engine is decided by `retire` (have all searches
//! finished?) and `halt` (lockout when the queue overflows).

pub mod elements;
pub mod graph;
pub mod halt;
pub mod metric;
pub mod quant;
pub mod registry;
pub(crate) mod retire;

pub use elements::Elements;
pub use graph::{Accept, AnnIndex};
pub use metric::Metric;
pub use quant::Quant;
