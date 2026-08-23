//! One module per group of commands, and the text-to-coordinates step they share.
//!
//! Everything here is a command's own logic. What they all lean on — deciding which graph
//! serves a name ([`super::access`]), the two stores, the registry — is beside this, not in it.

mod coords;
mod index;
mod search;
mod vector;

pub use index::{vcreate, vdrop, vlist, vstats};
pub use search::{vsim_key, vsim_vector};
pub use vector::{vadd, vdel, vgetattr, vsetattr};
