use super::vector::check_index;
use crate::cmd::access::resolve;
use crate::engine::element::{IndexConfig, Layout};
use crate::engine::{self, Store, StoreError};
use crate::error::{Error, Reply, Result};
use crate::index::quant::Quant;
use crate::index::registry;
use crate::protocol::request::{Create, MAX_BODY_BYTES};

const DEFAULT_MAXCOUNT: u32 = 4000;

pub fn create(store: &Store, spec: &Create) -> Result<Reply> {
    let Create {
        index: name,
        dim,
        metric,
        quant,
        ..
    } = *spec;
    check_index(name)?;

    let layout = Layout::new(dim, quant);
    check_dimension_fits(store, layout, quant)?;
    // The graph is built by the link callback; reject invalid configurations before creating the Map.
    metric.check_quant(quant)?;

    let config = IndexConfig {
        dim,
        metric,
        quant,
        m: spec.m,
        efc: spec.efc,
        efs: spec.efs,
    };
    let maxcount = map_size_for(store, spec.maxcount)?;

    // Map and configuration are created in a single engine call so a Map without
    // configuration is never visible to replicas. The item link invokes the callback,
    // which reads the configuration and registers the index.
    //
    // Ensure the sweeper thread and worker pool are running before the callback fires.
    crate::runtime::sweep::ensure_sweeper();
    crate::runtime::pool::ensure_pool();
    let exptime = crate::ffi::server::realtime(spec.exptime);
    let created = store.create_index_map(
        name,
        &config.encode(),
        engine::index_attr(maxcount, exptime),
    );

    match created {
        Ok(()) if registry::contains(name) => Ok(Reply::Created),
        Ok(()) => {
            // The Map was created, but the callback failed to build the graph (e.g. OOM).
            // Clean up the empty Map to prevent a dangling `TYPE_MISMATCH` key.
            if let Err(e) = store.drop_map(name)
                && e != StoreError::KeyGone
            {
                eprintln!(
                    "ArcVector: could not remove the Map of '{name}' after a failed create: {e}"
                );
            }
            Err(Error::Index(format!(
                "the graph for '{name}' could not be built"
            )))
        }
        // Key exists: return `EXISTS` if it is our index, or `TYPE_MISMATCH` otherwise.
        Err(StoreError::KeyExists) => Ok(if registry::contains(name) {
            Reply::Exists
        } else {
            Reply::TypeMismatch
        }),
        Err(e) => Err(e.into()),
    }
}

fn map_size_for(store: &Store, maxcount: Option<u32>) -> Result<u32> {
    let ceiling = store.max_map_size().max(1);
    let Some(n) = maxcount else {
        return Ok(DEFAULT_MAXCOUNT.min(ceiling));
    };
    if n == 0 || n > ceiling {
        return Err(Error::bad_request(format!(
            "maxcount {n} is out of range (1..={ceiling}, the server's max_map_size)"
        )));
    }
    Ok(n)
}

pub fn drop_index(store: &Store, name: &str) -> Result<Reply> {
    check_index(name)?;
    // Indices created after this timestamp must not be touched.
    let stamp = registry::now();

    match resolve(store, name) {
        Ok(_) => {}
        Err(Error::TypeMismatch) => return Ok(Reply::TypeMismatch),
        Err(Error::NoSuchIndex) => {
            registry::remove_if_stale(name, stamp);
            return Ok(Reply::NotFound);
        }
        Err(e) => return Err(e),
    }

    // Delete the Map. The unlink callback unregisters the index from the registry.
    // In-flight commands finish normally; remaining elements are reclaimed asynchronously.
    match store.drop_map(name) {
        Ok(()) | Err(StoreError::KeyGone) => {}
        Err(e) => return Err(e.into()),
    }
    // If the Map was removed externally, clean up any stale entry created before `stamp`.
    registry::remove_if_stale(name, stamp);
    Ok(Reply::Dropped)
}

fn dim_body_bytes(dim: usize) -> usize {
    dim.saturating_mul(4).saturating_add(2)
}

fn check_dimension_fits(store: &Store, layout: Layout, quant: Quant) -> Result<()> {
    let limit = store.max_element_bytes() as usize;

    // Check sizing with the minimal metadata (empty document); larger metadata is checked on write.
    let stored = layout.stored_len(crate::bson::EMPTY.len());

    // Stored vector may be quantized, but incoming binary vectors always arrive as f32.
    // Reject if the required body exceeds the transfer limit.
    let body = dim_body_bytes(layout.dim);
    if body > MAX_BODY_BYTES {
        return Err(Error::bad_request(format!(
            "a {}-dimension vector is a {body}-byte body, over the {MAX_BODY_BYTES}-byte \
             transfer limit (max dimension is {})",
            layout.dim,
            (MAX_BODY_BYTES - 2) / 4,
        )));
    }

    if stored <= limit {
        return Ok(());
    }
    Err(Error::bad_request(format!(
        "element would be {stored} bytes, over max_element_bytes {limit} \
         (max dimension is {} for quant {quant})",
        Layout::max_dim_for(quant, limit),
    )))
}
