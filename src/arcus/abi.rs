//! Checking that the bindings match the daemon that loaded us.
//!
//! `engine_interface_v1` is called by offset and arcus has no single layout, so a
//! wrong pairing calls whatever function sits at the offset. `docs/내부구조.md` §7
//! for what can be checked and why; `docs/engine-abi.md` for operators.

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

/// Whether a daemon version string names an EE build.
pub fn version_is_ee(version: &str) -> bool {
    version.split('-').any(|part| part == "E")
}

/// The daemon's version string, or `None` if the server API does not offer one.
///
/// # Safety
///
/// `server` must be the live `SERVER_HANDLE_V1` memcached returned from
/// `get_server_api`.
pub unsafe fn daemon_version(server: *const SERVER_HANDLE_V1) -> Option<String> {
    if server.is_null() {
        return None;
    }
    // SAFETY: caller guarantees `server` is live. `core` is member 2 of a struct
    // whose leading members are identical in every arcus tree, and
    // `server_version` is member 3 of `core` — both inside the stable prefix, so
    // this call is safe even when the *engine* vtable is misaligned.
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

/// One line naming what loaded us and what we were built for.
pub fn fingerprint(version: Option<&str>) -> String {
    format!(
        "ArcVector: daemon {}, built for {}",
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

/// Whether a call has behaved in a way only a misaligned vtable explains.
///
/// Latched, and never cleared: the vtable does not become correct later. Once it
/// is set nothing the engine says can be trusted, which is why commands stop
/// rather than act on what they read.
pub fn mismatched() -> bool {
    MISMATCHED.load(std::sync::atomic::Ordering::Acquire)
}

/// Say that the engine behaved in a way only a misaligned vtable explains.
pub fn report_mismatch(symptom: &str) {
    MISMATCHED.store(true, std::sync::atomic::Ordering::Release);
    MISMATCH_REPORTED.call_once(|| {
        eprintln!(
            "ArcVector: ENGINE ABI MISMATCH — {symptom}.\n\
             ArcVector: {}\n\
             ArcVector: arcus has no single vtable layout and nothing at runtime\n\
             ArcVector: identifies one, so the bindings must be generated from the\n\
             ArcVector: daemon's own headers and configure flags:\n\
             ArcVector:   ARCVECTOR_ENGINE_INCLUDE=<tree>/include \\\n\
             ArcVector:   cargo build --features daemon-replication\n\
             ArcVector: see docs/engine-abi.md.",
            built_for(),
        );
    });
}

/// Report the pairing, and decide whether the vtable may be used.
///
/// `false` only on hard evidence: a required member the daemon left null.
///
/// # Safety
///
/// `server` must be the live `SERVER_HANDLE_V1`, and `vt` the engine's own vtable.
pub unsafe fn verify(server: *const SERVER_HANDLE_V1, vt: &engine_interface_v1) -> bool {
    *CHECKED.get_or_init(|| {
        // SAFETY: guaranteed by the caller.
        let version = unsafe { daemon_version(server) };
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
                     ArcVector: function. Rebuild against this daemon's headers:\n\
                     ArcVector:   ARCVECTOR_ENGINE_INCLUDE=<tree>/include \\\n\
                     ArcVector:   cargo build --features daemon-replication\n\
                     ArcVector: see docs/engine-abi.md.",
                    fingerprint(version.as_deref()),
                    env!("ARCVECTOR_ABI_TREE"),
                    if version_is_ee(v) {
                        "an EE daemon"
                    } else {
                        "an OSS daemon"
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
             ArcVector: the daemon left these required members null: {}\n\
             ArcVector: arcus has no single vtable layout, and nothing at runtime\n\
             ArcVector: identifies one, so the bindings must be generated against\n\
             ArcVector: the daemon's own headers and configure flags:\n\
             ArcVector:   ARCVECTOR_ENGINE_INCLUDE=<tree>/include \\\n\
             ArcVector:   cargo build --features daemon-replication\n\
             ArcVector: see docs/engine-abi.md.",
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
        // `types.h` defines SCAN_COMMAND, so a default build always carries it.
        // ENABLE_REPLICATION comes from `configure` instead, so it is never
        // present unless the builder asked — the vendored headers alone cannot
        // supply it, whichever tree they came from.
        if env!("ARCVECTOR_ABI_HEADERS") == "include" {
            assert!(text.contains("scan"), "{text}");
            // The `daemon-replication` feature is the one thing that can put it
            // there, which is the whole reason the feature exists.
            assert_eq!(
                text.contains("replication"),
                cfg!(feature = "daemon-replication"),
                "ENABLE_REPLICATION comes from the feature or not at all: {text}"
            );
        }
    }

    #[test]
    fn an_all_null_vtable_reports_every_required_member() {
        // SAFETY: `engine_interface_v1` is all `Option<fn>` and raw values, so
        // all-zero is a valid (if useless) instance — which is the point: it is
        // what a badly misaligned read looks like.
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
        // The OSS header does not mention `rp_cmd` anywhere, not even inside an
        // `#ifdef`, so the tree is decidable from the header text alone. Which
        // tree is vendored is a deployment choice, not something to pin here —
        // what must hold is that the answer is one of the two and that the
        // fingerprint says which, so a wrong pairing is legible in the log.
        let tree = env!("ARCVECTOR_ABI_TREE");
        assert!(tree == "ee" || tree == "oss", "{tree}");
        assert!(built_for().starts_with(tree), "{}", built_for());
    }
}
