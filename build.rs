//! Generates Rust bindings for the arcus engine ABI.
//!
//! The headers under `include/` are vendored copies of arcus-memcached's public
//! interface — only `engine.h` and what it transitively includes are kept. The
//! `include/memcached/` entries are symlinks to their siblings one level up,
//! which is what supplies the `memcached/` prefix those headers include by.
//!
//! Bindings go to `OUT_DIR` rather than into `src/`, so the build never writes to
//! the source tree: a read-only checkout works, and two targets can build the same
//! sources at once without fighting over one generated file.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=include");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    bindgen::Builder::default()
        .header("include/engine.h")
        .clang_arg("-Iinclude")
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE")
        .layout_tests(false)
        .blocklist_type("c_void")
        .generate()
        .expect("bindgen failed")
        .write_to_file(out.join("engine_api.rs"))
        .expect("failed to write engine_api.rs");
}
