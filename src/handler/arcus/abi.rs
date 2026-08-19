//! `engine_interface_v1` is called by offset and arcus has no single layout, so a wrong pairing calls whatever sits there.

use std::ffi::CStr;

use crate::engine_api::{SERVER_HANDLE_V1, engine_interface_v1};

/// Vtable members this crate calls, in the order the struct declares them.
const REQUIRED: [&str; 12] = [
    "map_struct_create",
    "map_elem_alloc",
    "map_elem_free",
    "map_elem_insert",
    "map_elem_delete",
    "map_elem_get",
    "map_elem_release",
    "remove",
    "get_stats",
    "get_config",
    "get_item_info",
    "get_elem_info",
];

fn present(vt: &engine_interface_v1, name: &str) -> bool {
    match name {
        "map_struct_create" => vt.map_struct_create.is_some(),
        "map_elem_alloc" => vt.map_elem_alloc.is_some(),
        "map_elem_free" => vt.map_elem_free.is_some(),
        "map_elem_insert" => vt.map_elem_insert.is_some(),
        "map_elem_delete" => vt.map_elem_delete.is_some(),
        "map_elem_get" => vt.map_elem_get.is_some(),
        "map_elem_release" => vt.map_elem_release.is_some(),
        "remove" => vt.remove.is_some(),
        "get_stats" => vt.get_stats.is_some(),
        "get_config" => vt.get_config.is_some(),
        "get_item_info" => vt.get_item_info.is_some(),
        "get_elem_info" => vt.get_elem_info.is_some(),
        _ => true,
    }
}

/// The layout these bindings were generated against, recorded by `build.rs`.
pub fn built_for() -> String {
    format!(
        "{} tree, {} members, features=[{}], headers={}",
        env!("ARCVECTOR_ABI_TREE"),
        env!("ARCVECTOR_ABI_MEMBERS"),
        env!("ARCVECTOR_ABI_FEATURES"),
        env!("ARCVECTOR_ABI_HEADERS"),
    )
}

/// Whether the headers came from the EE tree. Decided at build time.
pub fn built_for_ee() -> bool {
    env!("ARCVECTOR_ABI_TREE") == "ee"
}

pub fn version_is_ee(version: &str) -> bool {
    version.split('-').any(|part| part == "E")
}

/// # Safety
///
/// `server` must be the live `SERVER_HANDLE_V1` from `get_server_api`.
pub unsafe fn server_version(server: *const SERVER_HANDLE_V1) -> Option<String> {
    if server.is_null() {
        return None;
    }
    // SAFETY: `core` and `server_version` sit in the prefix identical in every tree, so this holds even when the engine vtable is misaligned.
    unsafe {
        let core = (*server).core;
        if core.is_null() {
            return None;
        }
        let version = (*core).server_version?();
        if version.is_null() {
            return None;
        }
        Some(CStr::from_ptr(version).to_string_lossy().into_owned())
    }
}

pub fn missing(vt: &engine_interface_v1) -> Vec<&'static str> {
    REQUIRED
        .into_iter()
        .filter(|name| !present(vt, name))
        .collect()
}

pub fn fingerprint(version: Option<&str>) -> String {
    format!(
        "ArcVector: memcached {}, built for {}",
        version.unwrap_or("version unknown"),
        built_for(),
    )
}

/// Runs once, the first time the engine handle resolves.
static CHECKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether the load-time check ran and rejected this pairing.
pub fn refused() -> bool {
    CHECKED.get() == Some(&false)
}

/// Reported once, however many commands trip over it.
static MISMATCH_REPORTED: std::sync::Once = std::sync::Once::new();
static MISMATCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Latched and never cleared: the vtable does not become correct later, so commands stop rather than act on what they read.
pub fn mismatched() -> bool {
    MISMATCHED.load(std::sync::atomic::Ordering::Acquire)
}

pub fn report_mismatch(symptom: &str) {
    MISMATCHED.store(true, std::sync::atomic::Ordering::Release);
    MISMATCH_REPORTED.call_once(|| {
        eprintln!(
            "ArcVector: ENGINE ABI MISMATCH — {symptom}.\n\
             ArcVector: {}\n\
             ArcVector: arcus has no single vtable layout and nothing at runtime\n\
             ArcVector: identifies one, so the bindings must be generated from the\n\
             ArcVector: server's own headers and configure flags:\n\
             ArcVector:   ARCVECTOR_ENGINE_INCLUDE=<tree>/include \\\n\
             ArcVector:   cargo build --features replication\n\
             ArcVector: see docs/내부구조.md §11.",
            built_for(),
        );
    });
}

/// # Safety
///
/// `server` is the live handle, `vt` the engine's vtable; `false` only on a null required member.
pub unsafe fn verify(server: *const SERVER_HANDLE_V1, vt: &engine_interface_v1) -> bool {
    *CHECKED.get_or_init(|| {
        let version = unsafe { server_version(server) };
        if std::env::var_os("ARCVECTOR_ABI_DEBUG").is_some() {
            for name in REQUIRED {
                eprintln!("ArcVector: abi {name} = {}", present(vt, name));
            }
        }

        if let Some(v) = version.as_deref()
            && version_is_ee(v) != built_for_ee()
        {
            {
                eprintln!(
                    "{}\n\
                     ArcVector: REFUSING TO RUN — built against the {} tree, loaded by {}.\n\
                     ArcVector: the vtable is called by offset and the two trees do not\n\
                     ArcVector: agree on it, so every engine call would land on the wrong\n\
                     ArcVector: function. Rebuild against this server's headers:\n\
                     ArcVector:   ARCVECTOR_ENGINE_INCLUDE=<tree>/include \\\n\
                     ArcVector:   cargo build --features replication\n\
                     ArcVector: see docs/내부구조.md §11.",
                    fingerprint(version.as_deref()),
                    env!("ARCVECTOR_ABI_TREE"),
                    if version_is_ee(v) {
                        "an EE server"
                    } else {
                        "an OSS server"
                    },
                );
                return false;
            }
        }

        let absent = missing(vt);
        if absent.is_empty() {
            eprintln!("{}", fingerprint(version.as_deref()));
            return true;
        }
        eprintln!(
            "{}\n\
             ArcVector: REFUSING TO RUN — the engine vtable is misaligned.\n\
             ArcVector: the server left these required members null: {}\n\
             ArcVector: arcus has no single vtable layout, and nothing at runtime\n\
             ArcVector: identifies one, so the bindings must be generated against\n\
             ArcVector: the server's own headers and configure flags:\n\
             ArcVector:   ARCVECTOR_ENGINE_INCLUDE=<tree>/include \\\n\
             ArcVector:   cargo build --features replication\n\
             ArcVector: see docs/내부구조.md §11.",
            fingerprint(version.as_deref()),
            absent.join(", "),
        );
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_for_reports_what_build_rs_recorded() {
        let text = built_for();
        assert!(text.contains("members"), "{text}");
        assert!(text.contains("headers="), "{text}");
        // `types.h` always defines SCAN_COMMAND; ENABLE_REPLICATION can only come from the feature.
        if env!("ARCVECTOR_ABI_HEADERS") == "include" {
            assert!(text.contains("scan"), "{text}");
            assert_eq!(
                text.contains("replication"),
                cfg!(feature = "replication"),
                "ENABLE_REPLICATION comes from the feature or not at all: {text}"
            );
        }
    }

    #[test]
    fn an_all_null_vtable_reports_every_required_member() {
        // SAFETY: all-zero is a valid `engine_interface_v1` — which is the point: it is what a misaligned read looks like.
        let vt: engine_interface_v1 = unsafe { std::mem::zeroed() };
        assert_eq!(missing(&vt).len(), REQUIRED.len());
    }

    #[test]
    fn fingerprint_survives_an_unknown_version() {
        assert!(fingerprint(None).contains("version unknown"));
        assert!(fingerprint(Some("0.9.5-E-139")).contains("0.9.5-E-139"));
    }

    #[test]
    fn only_a_lone_e_component_marks_an_ee_version() {
        assert!(version_is_ee("0.9.5-E-139"));
        assert!(version_is_ee("1.2.3-E"));
        assert!(!version_is_ee("1.16.1"));
        // Substring matching would call both of these EE.
        assert!(!version_is_ee("1.2.3-EXPERIMENTAL"));
        assert!(!version_is_ee("1.2.3-EE"));
    }

    #[test]
    fn the_vendored_headers_name_their_tree() {
        // Which tree is vendored is a deployment choice; the fingerprint just has to name it.
        let tree = env!("ARCVECTOR_ABI_TREE");
        assert!(tree == "ee" || tree == "oss", "{tree}");
        assert!(built_for().starts_with(tree), "{}", built_for());
    }
}
