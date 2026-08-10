//! ArcVector — vector search as an arcus ASCII protocol extension.
//!
//! Layering:
//!
//! ```text
//! lib.rs    extension registration, tokenizing, the nread state machine
//!   codec   element byte layout          (pure)
//!   quant   f32 -> f16/i8/b1             (pure)
//!   filter  filter expressions           (pure)
//!   index   usearch wrapper, concurrency
//!   store   arcus Map engine calls
//! ```
//!
//! The single consistency rule: **the usearch index is a pure cache rebuildable
//! from Map. If it is not in Map, it does not exist.** Restart, eviction, TTL
//! expiry and replication slaves are all covered by that one rule.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unsafe_op_in_unsafe_fn)]
// bindgen's generated engine_api.rs trips this on the engine's bitfields.
#![allow(unnecessary_transmutes)]

pub mod engine_api;

pub mod codec;
pub mod filter;
pub mod index;
pub mod quant;
pub mod store;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use codec::Layout;
use engine_api::*;
use filter::Filter;
use index::{AnnIndex, Metric};
use quant::Quant;
use store::StoreError;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

type EXTENSION_RESPONSE_HANDLER =
    Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

/// Upper bound on a single transferred blob, to keep a malformed length from
/// requesting an enormous allocation.
const MAX_BLOB_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Index registry
// ---------------------------------------------------------------------------

pub struct VectorIndex {
    pub name: String,
    pub ann: AnnIndex,
    pub maxcount: u32,
    /// Serializes lazy rebuild so two workers cannot rebuild the same index.
    build: Mutex<bool>,
}

static INDICES: LazyLock<RwLock<HashMap<String, Arc<VectorIndex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Look an index up and release the registry lock immediately. A concurrent
/// `vdrop` may unregister it while we work; our `Arc` keeps it alive until we
/// are done.
fn lookup(name: &str) -> Option<Arc<VectorIndex>> {
    INDICES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

/// Populate the usearch index from Map on first use.
///
/// Covers cold start after a restart and replication slaves, where Map holds the
/// data but the in-memory graph does not exist yet.
fn ensure_built(cookie: *const c_void, idx: &VectorIndex) -> Result<(), String> {
    let mut built = idx.build.lock().unwrap_or_else(|e| e.into_inner());
    if *built {
        return Ok(());
    }
    let elems = store::get_all(cookie, &idx.name).map_err(|e| e.to_string())?;
    for (field, value) in elems {
        let element = idx
            .ann
            .layout
            .decode(&value)
            .map_err(|e| format!("{} in element '{}'", e, field))?;
        idx.ann.add(&field, element.vector)?;
    }
    *built = true;
    Ok(())
}

// ---------------------------------------------------------------------------
// Protocol helpers
// ---------------------------------------------------------------------------

fn token_str(t: &token_t) -> String {
    if t.value.is_null() || t.length == 0 {
        return String::new();
    }
    unsafe {
        let bytes = std::slice::from_raw_parts(t.value as *const u8, t.length);
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn respond(handler: EXTENSION_RESPONSE_HANDLER, cookie: *const c_void, msg: &str) {
    if let Some(h) = handler {
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(0);
        unsafe {
            h(cookie, buf.len() as c_int - 1, buf.as_ptr() as *const c_char);
        }
    }
}

fn client_error(handler: EXTENSION_RESPONSE_HANDLER, cookie: *const c_void, msg: &str) -> bool {
    respond(handler, cookie, &format!("CLIENT_ERROR {msg}\r\n"));
    true
}

fn server_error(handler: EXTENSION_RESPONSE_HANDLER, cookie: *const c_void, msg: &str) -> bool {
    respond(handler, cookie, &format!("SERVER_ERROR {msg}\r\n"));
    true
}

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}

// ---------------------------------------------------------------------------
// vcreate
// ---------------------------------------------------------------------------

unsafe fn cmd_vcreate(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc < 3 {
        return client_error(handler, cookie, "bad command line format");
    }

    let name = token_str(&tokens[1]);
    let Ok(dim) = token_str(&tokens[2]).parse::<usize>() else {
        return client_error(handler, cookie, "dimension must be a positive integer");
    };
    if dim == 0 || dim > u16::MAX as usize {
        return client_error(handler, cookie, "dimension out of range (1..65535)");
    }

    let mut metric = Metric::Cos;
    let mut q = Quant::F32;
    let mut filter_bytes = codec::DEFAULT_FILTER_BYTES;
    let mut threads = index::DEFAULT_THREADS;
    let mut connectivity = 0usize;
    let mut efc = 0usize;
    let mut efs = 0usize;
    let mut maxcount: Option<u32> = None;
    let mut exptime: Option<u32> = None;

    let mut i = 3usize;
    while i < argc as usize {
        let key = token_str(&tokens[i]).to_ascii_uppercase();
        if i + 1 >= argc as usize {
            return client_error(handler, cookie, &format!("option {key} needs a value"));
        }
        let val = token_str(&tokens[i + 1]);
        i += 2;

        macro_rules! num {
            ($t:ty) => {
                match val.parse::<$t>() {
                    Ok(v) => v,
                    Err(_) => {
                        return client_error(handler, cookie, &format!("invalid value for {key}"));
                    }
                }
            };
        }

        match key.as_str() {
            "METRIC" => match Metric::parse(&val) {
                Some(m) => metric = m,
                None => return client_error(handler, cookie, "unknown metric"),
            },
            "QUANT" => match Quant::parse(&val) {
                Some(v) => q = v,
                None => return client_error(handler, cookie, "unknown quantization"),
            },
            "FBYTES" => match Layout::validate_filter_bytes(num!(usize)) {
                Ok(v) => filter_bytes = v,
                Err(e) => return client_error(handler, cookie, e),
            },
            "THREADS" => threads = num!(usize).clamp(1, 1024),
            "M" => connectivity = num!(usize),
            "EFC" => efc = num!(usize),
            "EFS" => efs = num!(usize),
            "MAXCOUNT" => maxcount = Some(num!(u32)),
            "EXPTIME" => exptime = Some(num!(u32)),
            _ => return client_error(handler, cookie, &format!("unknown option {key}")),
        }
    }

    if let Err(e) = metric.check_quant(q) {
        return client_error(handler, cookie, &e);
    }

    // The Map element size limit is the index's real dimension ceiling. The
    // engine does not enforce it on the API path and oversized elements have
    // been observed to abort the daemon, so it is checked here.
    let limit = store::max_element_bytes(cookie) as usize;
    let layout = Layout::new(dim, q, filter_bytes);
    if layout.element_len() > limit {
        let max_dim = Layout::max_dim_for(q, filter_bytes, limit);
        return client_error(
            handler,
            cookie,
            &format!(
                "element would be {} bytes, over max_element_bytes {} \
                 (max dimension is {} for quant {} with FBYTES {})",
                layout.element_len(),
                limit,
                max_dim,
                q.as_str(),
                filter_bytes
            ),
        );
    }

    let cap = maxcount.unwrap_or_else(|| store::max_map_size(cookie));

    if INDICES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&name)
    {
        respond(handler, cookie, "EXISTS\r\n");
        return true;
    }

    let ann = match AnnIndex::new(layout, metric, connectivity, efc, efs, threads) {
        Ok(a) => a,
        Err(e) => return server_error(handler, cookie, &e),
    };

    if let Err(e) = store::create_map(cookie, &name, maxcount, exptime) {
        // The engine may still hold an orphan Map from before a restart. Clear it
        // and retry once so the module and engine can resynchronize.
        if store::drop_map(cookie, &name).is_err() {
            return server_error(handler, cookie, &e.to_string());
        }
        if let Err(e) = store::create_map(cookie, &name, maxcount, exptime) {
            return server_error(handler, cookie, &e.to_string());
        }
    }

    let mut reg = INDICES.write().unwrap_or_else(|e| e.into_inner());
    if reg.contains_key(&name) {
        respond(handler, cookie, "EXISTS\r\n");
        return true;
    }
    reg.insert(
        name.clone(),
        Arc::new(VectorIndex {
            name,
            ann,
            maxcount: cap,
            // Freshly created: nothing in Map to rebuild from.
            build: Mutex::new(true),
        }),
    );
    respond(handler, cookie, "CREATED\r\n");
    true
}

// ---------------------------------------------------------------------------
// vadd / vsearch bodies (delivered via nread)
// ---------------------------------------------------------------------------

fn do_vadd(
    cookie: *const c_void,
    index_name: &str,
    id: &str,
    vec_bytes: usize,
    data: &[u8],
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let Some(idx) = lookup(index_name) else {
        return client_error(handler, cookie, "index not found");
    };
    if let Err(e) = ensure_built(cookie, &idx) {
        return server_error(handler, cookie, &e);
    }

    let coords = bytes_to_f32(&data[..vec_bytes]);
    if coords.len() != idx.ann.layout.dim {
        return client_error(
            handler,
            cookie,
            &format!("expected {} dimensions, got {}", idx.ann.layout.dim, coords.len()),
        );
    }
    if !finite(&coords) {
        return client_error(handler, cookie, "vector contains NaN or infinity");
    }

    let json = &data[vec_bytes..];
    if !json.is_empty() && serde_json::from_slice::<serde_json::Value>(json).is_err() {
        return client_error(handler, cookie, "filter payload is not valid JSON");
    }
    if json.len() > idx.ann.layout.filter_bytes {
        return client_error(
            handler,
            cookie,
            &format!(
                "filter is {} bytes, over the {}-byte slot",
                json.len(),
                idx.ann.layout.filter_bytes
            ),
        );
    }

    if idx.ann.key_of(id).is_none() && idx.ann.len() >= idx.maxcount as usize {
        respond(handler, cookie, "OVERFLOWED\r\n");
        return true;
    }

    let quantized = quant::encode(&coords, idx.ann.layout.quant);
    let value = match idx.ann.layout.encode(&quantized, json) {
        Ok(v) => v,
        Err(e) => return client_error(handler, cookie, &e.to_string()),
    };

    // Map first: it is the source of truth. If the usearch insert below fails,
    // the vector is simply invisible to search until the next rebuild.
    match store::put_elem(cookie, index_name, id, &value) {
        Ok(()) => {}
        Err(StoreError::Overflow) => {
            respond(handler, cookie, "OVERFLOWED\r\n");
            return true;
        }
        Err(StoreError::KeyGone) => {
            INDICES
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(index_name);
            return client_error(handler, cookie, "index was evicted");
        }
        Err(e) => return server_error(handler, cookie, &e.to_string()),
    }

    if let Err(e) = idx.ann.add(id, &quantized) {
        return server_error(handler, cookie, &e);
    }
    respond(handler, cookie, "STORED\r\n");
    true
}

fn do_vsearch(
    cookie: *const c_void,
    index_name: &str,
    k: usize,
    vec_bytes: usize,
    data: &[u8],
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let Some(idx) = lookup(index_name) else {
        return client_error(handler, cookie, "index not found");
    };
    if let Err(e) = ensure_built(cookie, &idx) {
        return server_error(handler, cookie, &e);
    }

    let coords = bytes_to_f32(&data[..vec_bytes]);
    if coords.len() != idx.ann.layout.dim {
        return client_error(
            handler,
            cookie,
            &format!("expected {} dimensions, got {}", idx.ann.layout.dim, coords.len()),
        );
    }
    if !finite(&coords) {
        return client_error(handler, cookie, "query contains NaN or infinity");
    }

    let expr = &data[vec_bytes..];
    let compiled = if expr.is_empty() {
        None
    } else {
        let text = String::from_utf8_lossy(expr).into_owned();
        match Filter::parse(&text) {
            Ok(f) => Some(f),
            Err(e) => return client_error(handler, cookie, &e.to_string()),
        }
    };

    let query = quant::encode(&coords, idx.ann.layout.quant);

    // Reused across every predicate call so the hot path allocates nothing after
    // the first visited node.
    let scratch = RefCell::new(Vec::with_capacity(idx.ann.layout.filter_bytes));
    let layout = idx.ann.layout;

    let hits = {
        let accept = |key: u64| -> bool {
            let Some(f) = compiled.as_ref() else {
                return true;
            };
            let Some(id) = idx.ann.id_of(key) else {
                return false;
            };
            let mut buf = scratch.borrow_mut();
            match store::get_filter_slot(cookie, index_name, id, &layout, &mut buf) {
                Ok(()) => f.matches(&buf),
                // Evicted or vanished mid-search: treat as no longer a candidate.
                Err(_) => false,
            }
        };
        match idx.ann.search(&query, k, accept) {
            Ok(h) => h,
            Err(e) => return server_error(handler, cookie, &e),
        }
    };

    let mut out = String::new();
    for (key, distance) in hits {
        let Some(id) = idx.ann.id_of(key) else { continue };
        let json = match store::get_elem(cookie, index_name, id) {
            Ok(v) => match layout.decode(&v) {
                Ok(e) => String::from_utf8_lossy(e.filter).into_owned(),
                Err(_) => String::new(),
            },
            Err(_) => continue,
        };
        out.push_str(&format!("VALUE {id} {distance} {}\r\n{json}\r\n", json.len()));
    }
    out.push_str("END\r\n");
    respond(handler, cookie, &out);
    true
}

// ---------------------------------------------------------------------------
// vget / vdel / vdrop / vlist
// ---------------------------------------------------------------------------

unsafe fn cmd_vget(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 3 {
        return client_error(handler, cookie, "bad command line format");
    }
    let name = token_str(&tokens[1]);
    let id = token_str(&tokens[2]);

    let Some(idx) = lookup(&name) else {
        return client_error(handler, cookie, "index not found");
    };
    match store::get_elem(cookie, &name, &id) {
        Ok(value) => match idx.ann.layout.decode(&value) {
            Ok(e) => {
                let json = String::from_utf8_lossy(e.filter);
                respond(
                    handler,
                    cookie,
                    &format!("VALUE {id} {}\r\n{json}\r\nEND\r\n", json.len()),
                );
                true
            }
            Err(e) => server_error(handler, cookie, &e.to_string()),
        },
        Err(StoreError::ElemGone) | Err(StoreError::KeyGone) => {
            respond(handler, cookie, "NOT_FOUND\r\n");
            true
        }
        Err(e) => server_error(handler, cookie, &e.to_string()),
    }
}

unsafe fn cmd_vdel(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 3 {
        return client_error(handler, cookie, "bad command line format");
    }
    let name = token_str(&tokens[1]);
    let id = token_str(&tokens[2]);

    let Some(idx) = lookup(&name) else {
        return client_error(handler, cookie, "index not found");
    };

    // Map first, then the cache. A failure to remove the graph node only leaves
    // a ghost, which the predicate filters out and a rebuild clears.
    let removed = match store::delete_elem(cookie, &name, &id) {
        Ok(()) => true,
        Err(StoreError::ElemGone) | Err(StoreError::KeyGone) => false,
        Err(e) => return server_error(handler, cookie, &e.to_string()),
    };
    if let Err(e) = idx.ann.remove(&id) {
        return server_error(handler, cookie, &e);
    }

    respond(handler, cookie, if removed { "DELETED\r\n" } else { "NOT_FOUND\r\n" });
    true
}

unsafe fn cmd_vdrop(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 2 {
        return client_error(handler, cookie, "bad command line format");
    }
    let name = token_str(&tokens[1]);

    let known = INDICES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&name)
        .is_some();
    // Attempt the engine delete regardless, so an orphan Map left by a restart
    // can still be cleaned up through vdrop.
    let dropped = store::drop_map(cookie, &name).is_ok();

    respond(handler, cookie, if known || dropped { "DROPPED\r\n" } else { "NOT_FOUND\r\n" });
    true
}

unsafe fn cmd_vlist(
    cookie: *const c_void,
    argc: c_int,
    _argv: *mut token_t,
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    if argc != 1 {
        return client_error(handler, cookie, "bad command line format");
    }
    let reg = INDICES.read().unwrap_or_else(|e| e.into_inner());
    let mut names: Vec<&String> = reg.keys().collect();
    names.sort();

    let mut out = String::new();
    for name in names {
        let idx = &reg[name];
        let l = &idx.ann.layout;
        out.push_str(&format!(
            "INDEX {} dim={} quant={} metric={} fbytes={} count={} maxcount={}\r\n",
            name,
            l.dim,
            l.quant.as_str(),
            idx.ann.metric.as_str(),
            l.filter_bytes,
            idx.ann.len(),
            idx.maxcount,
        ));
    }
    out.push_str("END\r\n");
    respond(handler, cookie, &out);
    true
}

// ---------------------------------------------------------------------------
// nread state machine
// ---------------------------------------------------------------------------

enum PendingCmd {
    Vadd { index: String, id: String, vec_bytes: usize },
    Vsearch { index: String, k: usize, vec_bytes: usize },
}

struct Pending {
    cmd: PendingCmd,
    buffer: *mut u8,
    len: usize,
}

// The buffer is owned by exactly one connection at a time and is never shared;
// the map itself is mutex-guarded.
unsafe impl Send for Pending {}

static PENDING: LazyLock<Mutex<HashMap<usize, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

unsafe fn run_pending(cookie: *const c_void, handler: EXTENSION_RESPONSE_HANDLER) -> bool {
    let state = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(cookie as usize));
    let Some(state) = state else {
        return server_error(handler, cookie, "lost command state");
    };
    let data = std::slice::from_raw_parts(state.buffer, state.len).to_vec();
    free(state.buffer as *mut c_void);

    match state.cmd {
        PendingCmd::Vadd { index, id, vec_bytes } => {
            do_vadd(cookie, &index, &id, vec_bytes, &data, handler)
        }
        PendingCmd::Vsearch { index, k, vec_bytes } => {
            do_vsearch(cookie, &index, k, vec_bytes, &data, handler)
        }
    }
}

/// Reserve the nread buffer and register the pending command.
///
/// Returning `true` without setting `ndata` makes memcached run `execute` on the
/// command line, where the error is reported.
unsafe extern "C" fn accept_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) -> bool {
    if argc < 1 {
        return false;
    }
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    let cmd = token_str(&tokens[0]);

    let parse_len = |t: &token_t, multiple_of_4: bool| -> Option<usize> {
        let n: usize = token_str(t).parse().ok()?;
        if n > MAX_BLOB_BYTES {
            return None;
        }
        if multiple_of_4 && (n == 0 || n % 4 != 0) {
            return None;
        }
        Some(n)
    };

    match cmd.as_str() {
        "vcreate" | "vget" | "vdel" | "vdrop" | "vlist" => true,

        // vadd <index> <id> <veclen> [jsonlen]  then  <vector><json>\r\n
        "vadd" => {
            if argc != 4 && argc != 5 {
                return true;
            }
            let Some(vec_bytes) = parse_len(&tokens[3], true) else {
                return true;
            };
            let json_len = if argc == 5 {
                match parse_len(&tokens[4], false) {
                    Some(n) => n,
                    None => return true,
                }
            } else {
                0
            };
            let total = vec_bytes + json_len;
            let buf = malloc(total + 2) as *mut u8;
            if buf.is_null() {
                return true;
            }
            *ndata = total + 2;
            *ptr_out = buf as *mut c_char;
            PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(
                cookie as usize,
                Pending {
                    cmd: PendingCmd::Vadd {
                        index: token_str(&tokens[1]),
                        id: token_str(&tokens[2]),
                        vec_bytes,
                    },
                    buffer: buf,
                    len: total,
                },
            );
            true
        }

        // vsearch <index> <k> <veclen> [filterlen]  then  <vector><filter>\r\n
        //
        // The filter travels in the body rather than as command-line tokens so
        // that expressions containing spaces are not at the mercy of the
        // tokenizer's field limit.
        "vsearch" => {
            if argc != 4 && argc != 5 {
                return true;
            }
            let Ok(k) = token_str(&tokens[2]).parse::<usize>() else {
                return true;
            };
            if k == 0 {
                return true;
            }
            let Some(vec_bytes) = parse_len(&tokens[3], true) else {
                return true;
            };
            let filter_len = if argc == 5 {
                match parse_len(&tokens[4], false) {
                    Some(n) => n,
                    None => return true,
                }
            } else {
                0
            };
            let total = vec_bytes + filter_len;
            let buf = malloc(total + 2) as *mut u8;
            if buf.is_null() {
                return true;
            }
            *ndata = total + 2;
            *ptr_out = buf as *mut c_char;
            PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(
                cookie as usize,
                Pending {
                    cmd: PendingCmd::Vsearch {
                        index: token_str(&tokens[1]),
                        k,
                        vec_bytes,
                    },
                    buffer: buf,
                    len: total,
                },
            );
            true
        }

        _ => false,
    }
}

unsafe extern "C" fn execute_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    if argc == 0 {
        return run_pending(cookie, handler);
    }
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    match token_str(&tokens[0]).as_str() {
        "vcreate" => cmd_vcreate(cookie, argc, argv, handler),
        "vget" => cmd_vget(cookie, argc, argv, handler),
        "vdel" => cmd_vdel(cookie, argc, argv, handler),
        "vdrop" => cmd_vdrop(cookie, argc, argv, handler),
        "vlist" => cmd_vlist(cookie, argc, argv, handler),
        // Well-formed vadd/vsearch complete through nread (argc == 0); arriving
        // here means the command line itself was malformed.
        "vadd" | "vsearch" => client_error(handler, cookie, "bad command line format"),
        _ => client_error(handler, cookie, "unknown command"),
    }
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    if let Some(state) = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(cookie as usize))
    {
        free(state.buffer as *mut c_void);
    }
}

unsafe extern "C" fn get_name_vector(_cmd_cookie: *const c_void) -> *const c_char {
    c"arcus-vector-db".as_ptr()
}

static mut VECTOR_DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR =
    EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
        get_name: Some(get_name_vector),
        get_auth_flag: None,
        accept: Some(accept_vector_cmd),
        execute: Some(execute_vector_cmd),
        abort: Some(abort_vector_cmd),
        cookie: ptr::null_mut(),
        next: ptr::null_mut(),
    };

#[unsafe(no_mangle)]
pub extern "C" fn memcached_extensions_initialize(
    _config: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    unsafe {
        let Some(get_api) = get_server_api else {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        };
        store::set_server_api(get_api);

        let server = get_api();
        if server.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
        let ext = (*server).extension;
        if ext.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
        let Some(register) = (*ext).register_extension else {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        };
        if !register(
            extension_type_t_EXTENSION_ASCII_PROTOCOL,
            &raw mut VECTOR_DESCRIPTOR as *mut _ as *mut c_void,
        ) {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
    }
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS
}
