//! ArcVector: vector search as an ASCII-protocol extension to ARCUS memcached.
//!
//! An index is one ARCUS Map carrying the event flag, and a vector is one element of that Map.
//! When the engine reports element lifecycle events (`trigger`), lane threads apply them to the
//! HNSW graph (`index`), and `vop` commands (`cmd`) search that graph.
//!
//! | Module | Role |
//! |---|---|
//! | [`ffi`] | C ABI boundary: load entry point, protocol callbacks, core API, bindings |
//! | [`protocol`] | Parses `vop` command lines and bodies |
//! | [`cmd`] | Executes a single command |
//! | [`engine`] | Reads and writes the ARCUS engine (Maps and elements) |
//! | [`index`] | HNSW graph, index registry, and when-to-release-references machinery |
//! | [`trigger`] | Receives engine item events and queues graph work onto lanes |
//! | [`runtime`] | Lane thread pool, sweeper, reply path |

mod bson;
mod cmd;
mod engine;
mod error;
mod fallible;
mod ffi;
mod index;
mod protocol;
mod runtime;
mod trigger;
