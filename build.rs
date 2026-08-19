//! Bindgen for the arcus engine ABI: vendored headers in `include/memcached/` (upstream's own `#include` prefix), output to `OUT_DIR`.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-env-changed=ARCVECTOR_ENGINE_INCLUDE");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    // The vtable is read by offset and no two arcus builds lay it out alike, so the features must match the server exactly: a mismatch calls whatever sits at the offset.
    let include = env::var("ARCVECTOR_ENGINE_INCLUDE").unwrap_or_else(|_| "include".to_owned());
    let header = format!("{include}/memcached/engine.h");

    let mut builder = bindgen::Builder::default()
        .header(&header)
        .clang_arg(format!("-I{include}"))
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE");

    // Feature names are the server's `configure` flags minus `ENABLE_`, so the macro to define is derived rather than written twice.
    for flag in ["replication", "migration", "cluster-aware"] {
        if enabled(flag) {
            builder = builder.clang_arg(format!("-DENABLE_{}", macro_case(flag)));
        }
    }

    // Rebuilding a graph from Map only earns its cost where replication or persistence can outlive the graph.
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

    // Record the layout so the log and `vstats` can name what this was built against; a mismatch cannot be detected automatically.
    let text = std::fs::read_to_string(&generated).expect("bindgen wrote engine_api.rs");
    let vtable = text
        .split_once("pub struct engine_interface_v1 {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    let members = vtable.matches("pub ").count();
    // Read back what bindgen emitted rather than what was asked for; `cluster_aware` lands in `SERVER_CORE_API`, not the engine vtable.
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
    // Persistence leaves no trace in the bindings, so it is reported from the feature.
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

    // `rp_*` declarations exist only in the EE tree, so the header text names the tree independent of the defines passed.
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
