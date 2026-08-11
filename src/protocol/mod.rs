//! The memcached ASCII protocol: reading a command line, writing one response.
//!
//! Split by stage, because that is the order the work happens in:
//!
//! - [`tokens`] reads memcached's token array and writes the reply. The only
//!   place that touches `token_t`.
//! - [`request`] turns those tokens into typed values. Pure — it knows nothing of
//!   storage, of search, or of state.
//! - [`pending`] holds a parsed request while its body is still arriving, for the
//!   two commands that have one.
//!
//! Nothing here decides what a command *does*; that is [`crate::command`]. So
//! handlers receive plain data and can be exercised without a server, and every
//! syntax rule has exactly one home.

pub mod pending;
pub mod request;
pub mod tokens;
