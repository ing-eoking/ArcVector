//! The wire: reading a command line and turning it into a typed request.
//!
//! Nothing here touches the engine, an index or any state, which is what lets a
//! syntax rule have exactly one home and lets [`crate::handler`] be exercised
//! without a server.

pub mod filter;
pub mod pending;
pub mod request;
pub mod tokens;
