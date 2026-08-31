use std::env;
use std::path::{Path, PathBuf};

/// The server `configure` flags that move members in `engine_interface_v1`, in
/// the order they read best in a file name.
const ABI_FLAGS: [&str; 3] = ["replication", "migration", "cluster-aware"];

fn main() {
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=bindings");
    println!("cargo:rerun-if-env-changed=ARCVECTOR_ENGINE_INCLUDE");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    let include = env::var("ARCVECTOR_ENGINE_INCLUDE").unwrap_or_else(|_| "include".to_owned());
    let header = format!("{include}/memcached/engine.h");

    println!("cargo::rustc-check-cfg=cfg(recovery)");
    if enabled("replication") || enabled("persistence") {
        println!("cargo:rustc-cfg=recovery");
    }

    let generated = out.join("engine_api.rs");
    bindings(&include, &header, &generated);

    let text = std::fs::read_to_string(&generated).expect("engine_api.rs is in place");
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

    let source = std::fs::read_to_string(&header).expect("the engine header is readable");
    let tree = if source.contains("rp_cmd") {
        "ee"
    } else {
        "oss"
    };
    println!("cargo:rustc-env=ARCVECTOR_ABI_TREE={tree}");
}

/// Names the committed bindings for the flags this build has on.
///
/// The flags decide the vtable's shape, so each combination is its own file.
fn variant() -> String {
    let on: Vec<&str> = ABI_FLAGS.into_iter().filter(|f| enabled(f)).collect();
    if on.is_empty() {
        "base".to_owned()
    } else {
        on.join("+")
    }
}

/// Puts `engine_api.rs` in `OUT_DIR`, from the copy committed under `bindings/`.
///
/// The headers under `include/` are fixed and so is what bindgen makes of them,
/// so the generated file is committed rather than regenerated on every machine
/// that builds this. That is the whole reason a plain `cargo build` needs no
/// libclang, and hence no LLVM: bindgen is not in the dependency graph at all
/// unless `regen-bindings` is on.
#[cfg(not(feature = "regen-bindings"))]
fn bindings(_include: &str, _header: &str, generated: &Path) {
    let variant = variant();
    let committed = Path::new("bindings").join(format!("{variant}.rs"));

    assert!(
        env::var_os("ARCVECTOR_ENGINE_INCLUDE").is_none(),
        "ARCVECTOR_ENGINE_INCLUDE points this build at headers other than the ones \
         under include/, and the bindings committed in bindings/ were generated \
         from those. Build with --features regen-bindings (which needs libclang) \
         to translate the headers you are pointing at."
    );

    assert!(
        committed.exists(),
        "no bindings committed for this combination of flags ({variant}); \
         `make bindings` regenerates the set on a machine that has libclang"
    );

    std::fs::copy(&committed, generated).expect("failed to copy the committed bindings");
    println!("cargo:rerun-if-changed={}", committed.display());
}

/// Translates the headers afresh, and writes the result back over the committed
/// copy so the two cannot drift apart unnoticed.
///
/// Everything the C library declares is kept out. Those headers are written per
/// platform -- the macOS run emitted a hundred `__darwin_*` aliases where glibc
/// would emit its own -- and letting them through is what would stop one
/// generated file from serving every target.
#[cfg(feature = "regen-bindings")]
fn bindings(include: &str, header: &str, generated: &Path) {
    let mut builder = bindgen::Builder::default()
        .header(header)
        .clang_arg(format!("-I{include}"))
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE");

    for flag in ABI_FLAGS {
        if enabled(flag) {
            builder = builder.clang_arg(format!("-DENABLE_{}", macro_case(flag)));
        }
    }

    builder
        .layout_tests(false)
        .blocklist_type("c_void")
        .allowlist_file(".*memcached/.*\\.h")
        // The handful that still come through, pinned to a spelling every LP64
        // target agrees on. `FILE` is only ever passed as `*mut FILE`, so nothing
        // needs its layout -- which is exactly what differs between libcs.
        .blocklist_type("FILE|__sFILE|__sFILEX|__sbuf|fpos_t|time_t|__darwin_.*|__int64_t")
        .raw_line("pub type FILE = ::std::os::raw::c_void;")
        .raw_line("pub type time_t = ::std::os::raw::c_long;")
        .generate()
        .expect("bindgen failed")
        .write_to_file(generated)
        .expect("failed to write engine_api.rs");

    if env::var_os("ARCVECTOR_ENGINE_INCLUDE").is_none() {
        let dir = Path::new("bindings");
        std::fs::create_dir_all(dir).expect("failed to create bindings/");
        std::fs::copy(generated, dir.join(format!("{}.rs", variant())))
            .expect("failed to refresh the committed bindings");
    }
}

fn macro_case(feature: &str) -> String {
    feature.to_uppercase().replace('-', "_")
}

fn enabled(feature: &str) -> bool {
    env::var_os(format!("CARGO_FEATURE_{}", macro_case(feature))).is_some()
}
