//! Everything between a byte on the socket and a typed request.
//!
//! The three pieces are one concern split by stage, which is why they live
//! together:
//!
//! - [`protocol`] reads memcached's token array and writes the one response.
//!   The only place that touches `token_t`.
//! - [`request`] turns those tokens into typed values. Pure: it knows nothing of
//!   the engine, of indexes, or of state.
//! - [`pending`] holds a parsed request while its body is still arriving, for the
//!   two commands that have one.
//!
//! Nothing here decides what a command *does*; that is [`crate::command`]. The
//! boundary is deliberate — handlers receive plain data, so they can be exercised
//! without a server, and every syntax rule has exactly one home.

pub mod pending;
pub mod protocol;
pub mod request;
