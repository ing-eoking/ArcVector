use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-env-changed=ARCVECTOR_ENGINE_INCLUDE");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    let include = env::var("ARCVECTOR_ENGINE_INCLUDE").unwrap_or_else(|_| "include".to_owned());
    let header = format!("{include}/memcached/engine.h");

    let mut builder = bindgen::Builder::default()
        .header(&header)
        .clang_arg(format!("-I{include}"))
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE");

    for flag in ["replication", "migration", "cluster-aware"] {
        if enabled(flag) {
            builder = builder.clang_arg(format!("-DENABLE_{}", macro_case(flag)));
        }
    }

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

    let text = std::fs::read_to_string(&generated).expect("bindgen wrote engine_api.rs");
    let vtable = text
        .split_once("pub struct engine_interface_v1 {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    let members = vtable.matches("pub ").count();

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

    let source = std::fs::read_to_string(&header).expect("bindgen read this header");
    let tree = if source.contains("rp_cmd") {
        "ee"
    } else {
        "oss"
    };
    println!("cargo:rustc-env=ARCVECTOR_ABI_TREE={tree}");
}

fn macro_case(feature: &str) -> String {
    feature.to_uppercase().replace('-', "_")
}

fn enabled(feature: &str) -> bool {
    env::var_os(format!("CARGO_FEATURE_{}", macro_case(feature))).is_some()
}
