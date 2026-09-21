use std::env;
use std::path::{Path, PathBuf};

/// Where the committed translation of the headers lives.
///
/// One file, not a set: which members `engine_interface_v1` has is decided by
/// the server's `config.h`, and that file is vendored next to the headers it
/// configures. There is nothing left for a cargo feature to select.
const COMMITTED: &str = "bindings/engine_api.rs";

/// A fingerprint of the headers the committed translation was made from,
/// written when it is generated and compared on every other build.
const FINGERPRINT: &str = "bindings/headers.fingerprint";

/// The vendored public headers of the server this is built against.
///
/// Always this directory. There is no way to point the build at a server tree
/// somewhere else, on purpose: the translation is committed, and a build that
/// read headers from outside the crate would either contradict the committed
/// copy or quietly overwrite it. Following a different server means copying
/// that server's headers in here and retranslating, which leaves the crate
/// saying in its own files which server it is for.
const INCLUDE: &str = "include";

/// The server's configuration headers, which sit at the crate root rather than
/// under `include/`.
///
/// That is the server tree's own layout -- `config.h` at the top, the public
/// headers under `include/memcached` -- so both copy across without being
/// rearranged. `config.h` ends with `#include "config_static.h"`, a quoted
/// include resolved next to itself, which is why the second file has to keep
/// it company.
const CONFIG_FILES: [&str; 2] = ["config.h", "config_static.h"];

fn main() {
    println!("cargo:rerun-if-changed={INCLUDE}");
    println!("cargo:rerun-if-changed=bindings");
    // Named one by one rather than as a directory: the config files sit at the
    // crate root, and telling cargo to watch that would rerun this on every
    // edit anywhere in the tree.
    for name in CONFIG_FILES {
        println!("cargo:rerun-if-changed={name}");
    }

    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    let header = format!("{INCLUDE}/memcached/engine.h");
    let generated = out.join("engine_api.rs");
    bindings(&header, &generated);

    let text = std::fs::read_to_string(&generated).expect("engine_api.rs is in place");
    let vtable = text
        .split_once("pub struct engine_interface_v1 {")
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_default();
    let members = vtable.matches("pub ").count();

    // Read back off the translation rather than off a cargo feature, so what
    // this reports is what the bindings actually contain.
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
    if defined("ENABLE_PERSISTENCE") {
        features.push("persistence");
    }
    features.sort_unstable();

    println!("cargo:rustc-env=ARCVECTOR_ABI_MEMBERS={members}");
    println!(
        "cargo:rustc-env=ARCVECTOR_ABI_FEATURES={}",
        features.join(",")
    );
    println!("cargo:rustc-env=ARCVECTOR_ABI_HEADERS={INCLUDE}");

    let source = std::fs::read_to_string(&header).expect("the engine header is readable");
    let tree = if source.contains("rp_cmd") {
        "ee"
    } else {
        "oss"
    };
    println!("cargo:rustc-env=ARCVECTOR_ABI_TREE={tree}");
}

/// Whether the vendored `config.h` turns a macro on.
///
/// autoconf writes the off case as `/* #undef NAME */`, so a plain substring
/// search would answer yes to both. Only a real `#define` counts.
fn defined(macro_name: &str) -> bool {
    let Ok(text) = std::fs::read_to_string("config.h") else {
        return false;
    };
    text.lines().any(|line| {
        line.strip_prefix("#define")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix(macro_name))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })
}

/// Puts `engine_api.rs` in `OUT_DIR`, from the copy committed under `bindings/`.
///
/// The committed copy is what keeps libclang, and hence LLVM, out of an
/// ordinary build: bindgen is not in the dependency graph at all unless
/// `regen-bindings` is on.
#[cfg(not(feature = "regen-bindings"))]
fn bindings(_header: &str, generated: &Path) {
    let committed = Path::new(COMMITTED);

    assert!(
        committed.exists(),
        "{COMMITTED} is missing; `cargo build --features regen-bindings` writes it"
    );

    check_headers_match();

    std::fs::copy(committed, generated).expect("failed to copy the committed bindings");
    println!("cargo:rerun-if-changed={COMMITTED}");
}

/// Refuses to build when the headers have moved since the bindings were
/// generated from them.
///
/// The vtable is called by offset, so a header that adds or drops one member
/// shifts every member after it -- and the runtime check cannot see that,
/// because the slot it lands on still holds a perfectly valid function pointer
/// from the neighbouring entry. That is how a `types.h` which stopped defining
/// `JHPARK_OLD_SMGET_INTERFACE` turned `get_config` into `item_cachedump` and
/// segfaulted the daemon on the first `vcreate`. Comparing a fingerprint here
/// turns that into a build error that names the fix.
#[cfg(not(feature = "regen-bindings"))]
fn check_headers_match() {
    let now = header_fingerprint();
    match std::fs::read_to_string(FINGERPRINT).map(|t| t.trim().to_owned()) {
        Ok(recorded) if recorded == now => {}
        Ok(recorded) => panic!(
            "\n\
             {INCLUDE}/ or a config header at the crate root has changed since\n\
             {COMMITTED} was generated from them.\n\
             \n\
               recorded: {recorded}\n\
               now:      {now}\n\
             \n\
             The engine vtable is called by offset, so a member added or removed\n\
             in a header silently shifts every call after it. Retranslate:\n\
             \n\
               cargo build --features regen-bindings\n"
        ),
        Err(_) => panic!(
            "\n\
             {FINGERPRINT} is missing, so nothing records which headers\n\
             {COMMITTED} was generated from. Run:\n\
             \n\
               cargo build --features regen-bindings\n"
        ),
    }
}

/// A cheap content hash over every header under `include/`, plus the config
/// files at the crate root.
///
/// `config.h` and `config_static.h` are in here for the same reason `config.h`
/// is passed to clang: they decide which members the vtable has, so a build
/// against a differently configured server must not reuse this translation.
/// They are named explicitly because they live outside the include directory,
/// which is walked wholesale -- their own directory is the crate root and is
/// full of files that say nothing about the ABI.
///
/// FNV-1a rather than anything stronger on purpose: this catches an honest
/// edit, not an adversary, and it keeps build.rs dependency-free.
fn header_fingerprint() -> String {
    let mut names = Vec::new();
    collect_headers(Path::new(INCLUDE), &mut names);
    // A config file that has gone missing simply drops out, which moves the
    // fingerprint -- exactly what a deleted header does, and what the check
    // reading this is there to catch.
    names.extend(
        CONFIG_FILES
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file()),
    );
    names.sort();

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for path in &names {
        // The path, not just the name: a header moving between directories
        // changes what includes resolve to.
        feed(path.as_os_str().as_encoded_bytes());
        feed(
            &std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display())),
        );
    }
    format!("fnv1a64={hash:016x} files={}", names.len())
}

fn collect_headers(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        if path.is_dir() {
            collect_headers(&path, out);
        } else if path.extension().is_some_and(|e| e == "h") {
            out.push(path);
        }
    }
}

/// Translates the headers afresh, and writes the result back over the
/// committed copy so the two cannot drift apart unnoticed.
///
/// `-include config.h` is what makes the cargo features unnecessary. The
/// server's own build puts that file in front of every translation unit, so
/// doing the same here means `ENABLE_REPLICATION`, `ENABLE_MIGRATION` and
/// everything after them come from the file the server was configured with
/// rather than from flags a person has to remember to repeat. A flag added to
/// arcus tomorrow needs no change here.
///
/// Everything the C library declares is kept out. Those headers are written
/// per platform -- the macOS run emitted a hundred `__darwin_*` aliases where
/// glibc would emit its own -- and letting them through is what would stop one
/// generated file from serving every target.
#[cfg(feature = "regen-bindings")]
fn bindings(header: &str, generated: &Path) {
    assert!(
        Path::new("config.h").exists(),
        "config.h is missing from the crate root. Copy it from the server tree \
         together with config_static.h and include/memcached: it is what says which \
         ENABLE_* flags that server was built with, and so what shape its vtable has. \
         It goes at the top of the crate, beside include/ rather than inside it, \
         which is where the server keeps its own."
    );

    bindgen::Builder::default()
        .header(header)
        .clang_arg(format!("-I{INCLUDE}"))
        // `util.h` asks for `<config.h>`, so the crate root has to be on the
        // search path as well -- it no longer comes along with `include`.
        .clang_arg("-I.")
        .clang_arg("-include")
        .clang_arg("config.h")
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE")
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

    let dir = Path::new("bindings");
    std::fs::create_dir_all(dir).expect("failed to create bindings/");
    std::fs::copy(generated, COMMITTED).expect("failed to refresh the committed bindings");
    // Only now, with the committed copy actually made from these headers, is
    // the fingerprint true.
    std::fs::write(FINGERPRINT, format!("{}\n", header_fingerprint()))
        .expect("failed to record the header fingerprint");
}
