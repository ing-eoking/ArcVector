//! Command handlers, one module per group, plus the access check they share.
//!
//! Every handler returns `Result<Reply>`; turning that into an ASCII response is
//! [`crate::command::tokens::Responder::reply`]'s job.

mod access;
mod coords;
mod index;
mod search;
mod vector;

pub use index::{vcreate, vdrop, vlist, vstats};
pub use search::{vsim_key, vsim_vector};
pub use vector::{vadd, vdel, vget};
