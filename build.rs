//! Generates Rust bindings for the arcus engine ABI.
//!
//! `include/memcached/` holds vendored copies of arcus-memcached's public
//! interface — `engine.h` and the nine headers it transitively includes, nothing
//! else. The directory is named `memcached/` because every one of those headers
//! refers to its siblings by that prefix (`#include <memcached/types.h>`), so the
//! include path is the parent and the layout matches upstream's exactly.
//!
//! Bindings go to `OUT_DIR` rather than into `src/`, so the build never writes to
//! the source tree: a read-only checkout works, and two targets can build the same
//! sources at once without fighting over one generated file.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-env-changed=ARCVECTOR_ENGINE_INCLUDE");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    // `engine_interface_v1` is a vtable read by offset, and arcus builds do not lay
    // it out the same way. Two independent things decide the layout:
    //
    //   1. the source tree's own `types.h`, which hardcodes `SCAN_COMMAND`,
    //      `SUPPORT_BOP_SMGET` and `JHPARK_OLD_SMGET_INTERFACE` — the OSS tree
    //      defines the last one and the EE tree does not, which alone moves
    //      everything from `btree_elem_smget` on;
    //   2. `configure`, which sets `ENABLE_REPLICATION`, `ENABLE_MIGRATION` and
    //      `ENABLE_CLUSTER_AWARE` — no vendored header carries them, and the first
    //      two add seven and six members ahead of `get_item_info` and
    //      `get_elem_info`.
    //
    // Vendoring a tree's `include/` gets (1) for free. Only (2) has to be supplied,
    // by the cargo features, and it must match the server exactly: nothing at
    // runtime tells the variants apart — `interface` is 1 in all of them — so a
    // mismatch silently calls whatever occupies the offset. An EE server runs
    // `cachedump` for `get_config`.
    let include = env::var("ARCVECTOR_ENGINE_INCLUDE").unwrap_or_else(|_| "include".to_owned());
    let header = format!("{include}/memcached/engine.h");

    let mut builder = bindgen::Builder::default()
        .header(&header)
        .clang_arg(format!("-I{include}"))
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE");

    // Each feature is named after the server's `configure` flag with arcus's
    // `ENABLE_` prefix dropped, so the macro to define is derived rather than
    // written twice. `SCAN_COMMAND` and the `SUPPORT_BOP_*` family matter to the
    // layout too, but they live in the tree's `types.h` and arrive with the
    // headers, so they are not knobs.
    for flag in ["replication", "migration", "cluster-aware"] {
        if enabled(flag) {
            builder = builder.clang_arg(format!("-DENABLE_{}", macro_case(flag)));
        }
    }

    // Rebuilding a graph from Map only earns its cost where something can outlive
    // it — replication delivers a Map from a peer, persistence restores one on
    // restart. Neither means nothing can, so the machinery compiles out.
    println!("cargo::rustc-check-cfg=cfg(recovery)");
    if enabled("replication") || enabled("persistence") {
        println!("cargo:rustc-cfg=recovery");
    }

    let generated = out.join("engine_api.rs");
    builder
        .layout_tests(false)
        .blocklist_type("c_void")
        .generate()
        .expect("bindgen failed")
        .write_to_file(&generated)
        .expect("failed to write engine_api.rs");

    // Describe the layout that was just generated, so the running library can say
    // what it was built against. A mismatch cannot be detected automatically — see
    // `arcus::abi` — but it can be made obvious in the log and in `vstats`, which
    // is the difference between a five-minute diagnosis and a mystery SIGSEGV.
    let text = std::fs::read_to_string(&generated).expect("bindgen wrote engine_api.rs");
    let vtable = text
        .split_once("pub struct engine_interface_v1 {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    let members = vtable.matches("pub ").count();
    // Read back what was actually generated rather than what was asked for, so a
    // `config.h` that quietly lacks a flag is reported as it is. The vtable markers
    // are looked for in `engine_interface_v1`; `cluster_aware` adds to
    // `SERVER_CORE_API` instead, so it is looked for in the whole file.
    let mut features: Vec<&str> = [
        ("replication", "pub rp_cmd"),
        ("migration", "pub mg_prepare"),
        ("scan", "pub prefixscan"),
        ("smget_old", "pub btree_elem_smget_old"),
    ]
    .into_iter()
    .filter(|(_, marker)| vtable.contains(marker))
    .map(|(name, _)| name)
    .collect();
    if text.contains("pub is_zk_integrated") {
        features.push("cluster_aware");
    }
    // Persistence leaves no trace in the bindings, so it is reported from the
    // feature that declared it.
    if enabled("persistence") {
        features.push("persistence");
    }
    features.sort_unstable();

    println!("cargo:rustc-env=ARCVECTOR_ABI_MEMBERS={members}");
    println!(
        "cargo:rustc-env=ARCVECTOR_ABI_FEATURES={}",
        features.join(",")
    );
    println!("cargo:rustc-env=ARCVECTOR_ABI_HEADERS={include}");

    // Which tree these headers came from, read from the header *text* rather than
    // from what bindgen kept: the `rp_*` declarations exist only in the EE tree,
    // and looking at the source rather than the output makes the answer
    // independent of which defines were passed. The running library pairs this
    // with the server's own version string to catch a cross-tree build, which is
    // the one mismatch that crashes before anything can inspect it.
    let source = std::fs::read_to_string(&header).expect("bindgen read this header");
    let tree = if source.contains("rp_cmd") {
        "ee"
    } else {
        "oss"
    };
    println!("cargo:rustc-env=ARCVECTOR_ABI_TREE={tree}");
}

/// A cargo feature name as its `CARGO_FEATURE_*` / C macro spelling.
fn macro_case(feature: &str) -> String {
    feature.to_uppercase().replace('-', "_")
}

/// Whether the cargo feature of that name is on.
fn enabled(feature: &str) -> bool {
    env::var_os(format!("CARGO_FEATURE_{}", macro_case(feature))).is_some()
}
