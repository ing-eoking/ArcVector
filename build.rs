use std::env;
use std::path::{Path, PathBuf};

/// The server `configure` flags that move members in the structs this crate
/// binds, in the order they read best in a file name.
///
/// `cluster-aware` moves nothing in `engine_interface_v1` -- it is named here
/// because it shapes `SERVER_CORE_API`, and because Cargo.toml has `replication`
/// imply it, so the two always appear together in a variant name.
const ABI_FLAGS: [&str; 3] = ["replication", "migration", "cluster-aware"];

/// Where the fingerprint of the headers the committed bindings were made from
/// lives. Written by a `regen-bindings` build, checked by every other one.
const FINGERPRINT: &str = "bindings/headers.fingerprint";

fn main() {
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=bindings");
    println!("cargo:rerun-if-env-changed=ARCVECTOR_ENGINE_INCLUDE");

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    let include = env::var("ARCVECTOR_ENGINE_INCLUDE").unwrap_or_else(|_| "include".to_owned());
    let header = format!("{include}/memcached/engine.h");

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
/// Cargo has already applied the `replication -> cluster-aware` implication by
/// the time this reads CARGO_FEATURE_*, so `--features replication` lands on
/// `replication+cluster-aware.rs` and the lone `replication` name never occurs.
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
fn bindings(include: &str, _header: &str, generated: &Path) {
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

    check_headers_match(include);

    std::fs::copy(&committed, generated).expect("failed to copy the committed bindings");
    println!("cargo:rerun-if-changed={}", committed.display());
}

/// Refuses to build when `include/memcached` has moved since the committed
/// bindings were generated from it.
///
/// The vtable is called by offset, so a header that adds or drops one member
/// shifts every member after it -- and the runtime check cannot see that,
/// because the slot it lands on still holds a perfectly valid function pointer
/// from the neighbouring entry. That is how a `types.h` that stopped defining
/// `JHPARK_OLD_SMGET_INTERFACE` turned `get_config` into `item_cachedump` and
/// segfaulted the daemon on the first `vcreate`. Comparing a fingerprint here
/// turns that into a build error that names the fix.
#[cfg(not(feature = "regen-bindings"))]
fn check_headers_match(include: &str) {
    let now = header_fingerprint(include);
    let recorded = std::fs::read_to_string(FINGERPRINT).map(|t| t.trim().to_owned());

    match recorded {
        Ok(recorded) if recorded == now => {}
        Ok(recorded) => panic!(
            "\n\
             {include}/memcached has changed since bindings/ was generated from it.\n\
             \n\
               recorded: {recorded}\n\
               now:      {now}\n\
             \n\
             The engine vtable is called by offset, so a member added or removed\n\
             in a header silently shifts every call after it. Regenerate:\n\
             \n\
               make bindings          # needs libclang\n\
             \n\
             or, to follow a server tree: make sync-headers TREE=<path>\n"
        ),
        Err(_) => panic!(
            "\n\
             {FINGERPRINT} is missing, so nothing records which headers\n\
             bindings/ was generated from. Run `make bindings` to write it.\n"
        ),
    }
}

/// A cheap content hash over every header bindgen reads.
///
/// FNV-1a rather than anything stronger on purpose: this catches an honest
/// edit, not an adversary, and it keeps build.rs free of dependencies.
fn header_fingerprint(include: &str) -> String {
    let dir = Path::new(include).join("memcached");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "h"))
        .collect();
    names.sort();

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for path in &names {
        feed(
            path.file_name()
                .expect("a directory entry has a name")
                .as_encoded_bytes(),
        );
        feed(
            &std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())),
        );
    }
    format!("fnv1a64={hash:016x} files={}", names.len())
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
        // Only now, with the committed copy actually made from these headers,
        // is the fingerprint true. A build pointed elsewhere by
        // ARCVECTOR_ENGINE_INCLUDE refreshes neither.
        std::fs::write(FINGERPRINT, format!("{}\n", header_fingerprint(include)))
            .expect("failed to record the header fingerprint");
    }
}

fn macro_case(feature: &str) -> String {
    feature.to_uppercase().replace('-', "_")
}

fn enabled(feature: &str) -> bool {
    env::var_os(format!("CARGO_FEATURE_{}", macro_case(feature))).is_some()
}
