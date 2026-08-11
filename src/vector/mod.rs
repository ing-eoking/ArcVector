//! How a vector and its attributes are represented, independent of everything else.
//!
//! - [`codec`] lays out one Map element: a header, a fixed attribute region, then
//!   the quantized coordinates.
//! - [`quant`] converts `f32` into the scalar kind an index stores.
//! - [`filter`] compiles and evaluates the expressions that query the attribute
//!   region.
//!
//! All three are pure. No engine, no index, no daemon — which is why they carry
//! most of the test coverage: the layout, the quantization arithmetic and the
//! filter grammar can all be pinned down without a server anywhere in sight.

pub mod codec;
pub mod filter;
pub mod quant;
