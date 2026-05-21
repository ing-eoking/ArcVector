use std::env;

fn main() {
    let builder = bindgen::Builder::default()
        .header("src/c/engine.h")
        .clang_arg("-Isrc/c")
        .layout_tests(false)
        .blocklist_type("c_void")
        .clang_arg("-pthread")
        .clang_arg("-D_GNU_SOURCE");

    builder.generate()
        .expect("Bindgen failed")
        .write_to_file("src/engine_api.rs")
        .expect("Write failed");

    let files = ["src/c/server_api.c", "src/c/hash.c", "src/c/stats_prefix.c"];
    let all_exist = files.iter().all(|f| std::path::Path::new(f).exists());

    if all_exist {
        let mut cc_builder = cc::Build::new();
        for file in &files {
            cc_builder.file(file);
        }
        cc_builder
            .include("src/c")
            .flag("-pthread")
            .compile("server_framework");
    }

    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search=native={}", dir);

    let has_engine = std::path::Path::new(&dir).join("libengine.a").exists()
        || std::path::Path::new(&dir).join("engine.lib").exists();

    if all_exist {
        println!("cargo:rustc-link-lib=static=server_framework");
    }

    if has_engine {
        println!("cargo:rustc-link-arg=-Wl,--whole-archive");
        println!("cargo:rustc-link-lib=static=engine");
        println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");
    }
}
