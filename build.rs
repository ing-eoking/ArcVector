//! Generates Rust bindings for the arcus engine ABI.
//!
//! The headers under `src/c` are vendored copies of arcus-memcached's public
//! interface. Only `engine.h` and what it transitively includes are kept.

fn main() {
    println!("cargo:rerun-if-changed=src/c");

    bindgen::Builder::default()
        .header("src/c/engine.h")
        .clang_arg("-Isrc/c")
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE")
        .layout_tests(false)
        .blocklist_type("c_void")
        .generate()
        .expect("bindgen failed")
        .write_to_file("src/engine_api.rs")
        .expect("failed to write src/engine_api.rs");
}
