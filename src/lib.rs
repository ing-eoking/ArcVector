#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unnecessary_transmutes)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod engine_api;

use std::collections::{HashMap, HashSet};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}
use std::sync::{Arc, LazyLock, Mutex, RwLock, OnceLock};
use std::sync::atomic::{AtomicPtr, Ordering};

use engine_api::*;
use hnsw_rs::prelude::*;

type EXTENSION_RESPONSE_HANDLER = Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricType {
    L2 = 0,
    Cosine = 1,
    IP = 2,
}

#[derive(Clone)]
struct ArcDist(MetricType);

impl Distance<f32> for ArcDist {
    fn eval(&self, va: &[f32], vb: &[f32]) -> f32 {
        compute_distance(va, vb, self.0)
    }
}

fn compute_distance(q: &[f32], v: &[f32], metric: MetricType) -> f32 {
    let len = q.len().min(v.len());
    match metric {
        MetricType::L2 => {
            let mut dist = 0.0f32;
            for i in 0..len {
                let diff = q[i] - v[i];
                dist += diff * diff;
            }
            dist
        }
        MetricType::Cosine => {
            let mut dot = 0.0f32;
            let mut norm_q = 0.0f32;
            let mut norm_v = 0.0f32;
            for i in 0..len {
                dot += q[i] * v[i];
                norm_q += q[i] * q[i];
                norm_v += v[i] * v[i];
            }
            if norm_q == 0.0 || norm_v == 0.0 {
                return 1.0;
            }
            (1.0 - (dot / (norm_q.sqrt() * norm_v.sqrt()))).max(0.0)
        }
        MetricType::IP => {
            let mut dot = 0.0f32;
            for i in 0..len {
                dot += q[i] * v[i];
            }
            -dot
        }
    }
}

struct HnswIdState {
    id_map: Vec<String>,
    str_map: HashMap<String, usize>,
    deleted: HashSet<usize>,
}

struct HnswWrapper {
    inner: Hnsw<'static, f32, ArcDist>,
    id_state: Mutex<HnswIdState>,
    metric: MetricType,
}

impl HnswWrapper {
    fn new(metric: MetricType, max_elements: usize) -> Self {
        HnswWrapper {
            inner: Hnsw::<'static, f32, ArcDist>::new(16, max_elements, 16, 200, ArcDist(metric)),
            id_state: Mutex::new(HnswIdState {
                id_map: Vec::new(),
                str_map: HashMap::new(),
                deleted: HashSet::new(),
            }),
            metric,
        }
    }

    fn prepare_vec<'a>(&self, vec: &'a [f32]) -> std::borrow::Cow<'a, [f32]> {
        if self.metric == MetricType::Cosine {
            std::borrow::Cow::Owned(normalize_vector(vec))
        } else {
            std::borrow::Cow::Borrowed(vec)
        }
    }

    // Returns Err on allocation failure so the caller can answer SERVER_ERROR
    // instead of letting the default (infallible) allocator abort the daemon.
    // NOTE: the inner hnsw_rs graph allocates internally and is not fallible;
    // the try_reserve calls below act as an out-of-memory canary that trips
    // before we reach that uncatchable allocation.
    fn insert(&self, id: &str, vec: &[f32]) -> Result<(), ()> {
        let vec = self.prepare_vec(vec);
        let numeric_id = {
            let mut state = self.id_state.lock().unwrap();
            state.id_map.try_reserve(1).map_err(|_| ())?;
            state.str_map.try_reserve(1).map_err(|_| ())?;
            let nid = state.id_map.len();
            state.id_map.push(id.to_string());
            state.str_map.insert(id.to_string(), nid);
            nid
        };
        self.inner.insert((vec.as_ref(), numeric_id));
        Ok(())
    }

    fn delete(&self, id: &str) -> bool {
        let mut state = self.id_state.lock().unwrap();
        if let Some(&nid) = state.str_map.get(id) {
            state.deleted.insert(nid);
            true
        } else {
            false
        }
    }

    fn search(&self, query: &[f32], k: usize, ef: Option<usize>) -> Result<Vec<(String, f32)>, ()> {
        let (fetch_k, deleted_snapshot) = {
            let state = self.id_state.lock().unwrap();
            (k + state.deleted.len(), state.deleted.clone())
        };
        // ef_search: 클라이언트가 지정하면 그 값, 없으면 기존 기본식.
        // (hnsw_rs 는 ef >= 요청 개수여야 하므로 fetch_k 미만이면 끌어올린다)
        let ef_search = ef.unwrap_or((k * 10).max(50)).max(fetch_k);
        let inner = &self.inner;
        let query_owned = self.prepare_vec(query).into_owned();
        let raw_results = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inner.search(&query_owned, fetch_k, ef_search)
        })).map_err(|_| ())?;
        let state = self.id_state.lock().unwrap();
        Ok(raw_results
            .into_iter()
            .filter(|n| !deleted_snapshot.contains(&n.d_id))
            .take(k)
            .map(|n| (state.id_map[n.d_id].clone(), n.distance))
            .collect())
    }
}

enum IndexStorage {
    Flat,
    Hnsw(HnswWrapper),
}

impl IndexStorage {
    fn type_str(&self) -> &'static str {
        match self {
            IndexStorage::Flat => "FLAT",
            IndexStorage::Hnsw(_) => "HNSW",
        }
    }
}

struct VectorIndex {
    dimension: usize,
    metric: MetricType,
    vectors: RwLock<HashMap<String, Vec<f32>>>,
    storage: IndexStorage,
}

type IndexRegistry = RwLock<HashMap<String, Arc<VectorIndex>>>;

static ENGINE: AtomicPtr<engine_interface_v1> = AtomicPtr::new(ptr::null_mut());

static GET_SERVER_API_FN: OnceLock<unsafe extern "C" fn() -> *mut SERVER_HANDLE_V1> = OnceLock::new();

fn ensure_engine() -> *mut engine_interface_v1 {
    let ptr = ENGINE.load(Ordering::Acquire);
    if !ptr.is_null() {
        return ptr;
    }
    let get_api = match GET_SERVER_API_FN.get() {
        Some(f) => *f,
        None => return ptr::null_mut(),
    };
    unsafe {
        let server = get_api();
        if server.is_null() { return ptr::null_mut(); }
        let engine_ptr = (*server).engine;
        if engine_ptr.is_null() { return ptr::null_mut(); }
        let new_ptr = engine_ptr as *mut engine_interface_v1;
        match ENGINE.compare_exchange(ptr::null_mut(), new_ptr, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => new_ptr,
            Err(existing) => existing,
        }
    }
}

static INDICES: LazyLock<IndexRegistry> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

// Sanity caps to bound the per-connection nread allocation.
const MAX_VEC_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAYLOAD_LEN: usize = 16 * 1024 * 1024;

// Fallback if the engine config can't be queried.
const DEFAULT_MAX_ELEMENT_BYTES: u32 = 16 * 1024;

enum PendingCmd {
    Vadd { index_name: String, vector_id: String, vec_bytes: usize },
    Vsearch { index_name: String, k: usize, vec_bytes: usize, threshold: Option<f32>, ef: Option<usize> },
}

// Per-connection state for a command whose vector/payload arrives via nread.
struct PendingState {
    cmd: PendingCmd,
    buffer: *mut u8,
    data_len: usize, // meaningful bytes in buffer (excludes trailing \r\n)
}

unsafe impl Send for PendingState {}
unsafe impl Sync for PendingState {}

static PENDING: LazyLock<Mutex<HashMap<usize, PendingState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn get_token_string(token: &token_t) -> String {
    unsafe {
        let slice = std::slice::from_raw_parts(token.value as *const u8, token.length);
        String::from_utf8_lossy(slice).into_owned()
    }
}

fn send_response(response_handler: EXTENSION_RESPONSE_HANDLER, cookie: *const c_void, msg: &str) {
    if let Some(handler) = response_handler {
        unsafe {
            handler(cookie, msg.len() as i32, msg.as_ptr() as *const c_char);
        }
    }
}

// Decode a little-endian float32 blob (wire format) into a vector.
fn bytes_to_floats(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn is_valid_vector(v: &[f32]) -> bool {
    !v.is_empty() && v.iter().all(|x| x.is_finite())
}

fn normalize_vector(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

const ITEM_TYPE_MAP: c_int = 3;

enum MapInsertResult {
    Ok,
    KeyEvicted,
    Error,
}

unsafe fn map_insert_elem(
    cookie: *const c_void,
    map_key: &str,
    field: &[u8],
    value: &[u8],
    replace_if_exist: bool,
) -> MapInsertResult {
    let eng = ensure_engine();
    if eng.is_null() { return MapInsertResult::Error; }
    let map_elem_alloc = match (*eng).map_elem_alloc { Some(f) => f, None => return MapInsertResult::Error };
    let map_elem_insert = match (*eng).map_elem_insert { Some(f) => f, None => return MapInsertResult::Error };
    let map_elem_free = match (*eng).map_elem_free { Some(f) => f, None => return MapInsertResult::Error };
    let get_elem_info = match (*eng).get_elem_info { Some(f) => f, None => return MapInsertResult::Error };

    let mut eitem_ptr: *mut eitem = ptr::null_mut();
    let err = map_elem_alloc(
        eng as *mut ENGINE_HANDLE,
        cookie,
        map_key.as_ptr() as *const c_void,
        map_key.len() as c_int,
        field.len(),
        value.len(),
        &mut eitem_ptr,
    );
    if err != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        return MapInsertResult::Error;
    }

    let mut elem_info = std::mem::zeroed::<eitem_info>();
    get_elem_info(eng as *mut ENGINE_HANDLE, cookie, ITEM_TYPE_MAP, eitem_ptr, &mut elem_info);

    if elem_info.score.is_null() || elem_info.value.is_null() {
        map_elem_free(eng as *mut ENGINE_HANDLE, cookie, eitem_ptr);
        return MapInsertResult::Error;
    }
    ptr::copy_nonoverlapping(field.as_ptr(), elem_info.score as *mut u8, field.len());
    ptr::copy_nonoverlapping(value.as_ptr(), elem_info.value as *mut u8, value.len());

    let mut replaced = false;
    let mut created = false;
    let ret = map_elem_insert(
        eng as *mut ENGINE_HANDLE,
        cookie,
        map_key.as_ptr() as *const c_void,
        map_key.len() as c_int,
        eitem_ptr,
        replace_if_exist,
        ptr::null_mut(),
        &mut replaced,
        &mut created,
        0,
    );

    if ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        map_elem_free(eng as *mut ENGINE_HANDLE, cookie, eitem_ptr);
        if ret == ENGINE_ERROR_CODE_ENGINE_KEY_ENOENT {
            return MapInsertResult::KeyEvicted;
        }
        return MapInsertResult::Error;
    }
    MapInsertResult::Ok
}

unsafe fn execute_vcreate(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc < 4 || argc > 6 {
        send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
        return true;
    }

    let index_name = get_token_string(&tokens[1]);
    let dimension: usize = match get_token_string(&tokens[2]).parse() {
        Ok(d) => d,
        Err(_) => {
            send_response(response_handler, cookie, "CLIENT_ERROR bad dimension\r\n");
            return true;
        }
    };

    let metric_str = get_token_string(&tokens[3]).to_uppercase();
    let metric = match metric_str.as_str() {
        "L2" => MetricType::L2,
        "COSINE" => MetricType::Cosine,
        "IP" => MetricType::IP,
        _ => {
            send_response(response_handler, cookie, "CLIENT_ERROR unsupported metric\r\n");
            return true;
        }
    };

    let hnsw_max_elements: Option<u32> = if argc >= 5 {
        match get_token_string(&tokens[4]).to_uppercase().as_str() {
            "HNSW" => {
                let max: u32 = if argc == 6 {
                    match get_token_string(&tokens[5]).parse::<usize>() {
                        Ok(n) if n > 0 => match u32::try_from(n) {
                            Ok(v) => v,
                            Err(_) => {
                                send_response(response_handler, cookie, "CLIENT_ERROR max_elements too large\r\n");
                                return true;
                            }
                        },
                        _ => {
                            send_response(response_handler, cookie, "CLIENT_ERROR bad max_elements\r\n");
                            return true;
                        }
                    }
                } else {
                    1_000_000
                };
                Some(max)
            }
            "FLAT" => {
                if argc == 6 {
                    send_response(response_handler, cookie, "CLIENT_ERROR FLAT does not take max_elements\r\n");
                    return true;
                }
                None
            }
            _ => {
                send_response(response_handler, cookie, "CLIENT_ERROR unsupported index type (use FLAT or HNSW)\r\n");
                return true;
            }
        }
    } else {
        None
    };

    {
        let indices = INDICES.read().unwrap();
        if indices.contains_key(&index_name) {
            send_response(response_handler, cookie, "CLIENT_ERROR index already exists\r\n");
            return true;
        }
    }

    let eng = ensure_engine();
    if eng.is_null() {
        send_response(response_handler, cookie, "SERVER_ERROR engine not ready\r\n");
        return true;
    }
    let map_struct_create_fn = match (*eng).map_struct_create { Some(f) => f, None => {
        send_response(response_handler, cookie, "SERVER_ERROR map not supported\r\n");
        return true;
    }};

    let mut attrp = std::mem::zeroed::<item_attr>();
    attrp.readable = 1;
    if let Some(max) = hnsw_max_elements {
        attrp.maxcount = max as i32;
    }

    let mut create_ret = map_struct_create_fn(
        eng as *mut ENGINE_HANDLE,
        cookie,
        index_name.as_ptr() as *const c_void,
        index_name.len() as c_int,
        &mut attrp,
        0,
    );
    if create_ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        // 엔진엔 Map이 있는데 모듈 레지스트리(INDICES)엔 없는 'orphan' 상태일 수 있다.
        // (예: 데몬 재시작 후 Arcus가 컬렉션을 영속 복구했으나 모듈 상태는 초기화된 경우)
        // 이때는 stale Map을 지우고 다시 생성해 desync 를 자가 치유한다.
        let orphan = !INDICES.read().unwrap().contains_key(&index_name);
        if orphan {
            if let Some(remove) = (*eng).remove {
                remove(
                    eng as *mut ENGINE_HANDLE,
                    cookie,
                    index_name.as_ptr() as *const c_void,
                    index_name.len(),
                    0,
                    0,
                );
            }
            create_ret = map_struct_create_fn(
                eng as *mut ENGINE_HANDLE,
                cookie,
                index_name.as_ptr() as *const c_void,
                index_name.len() as c_int,
                &mut attrp,
                0,
            );
        }
        if create_ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
            send_response(response_handler, cookie, "CLIENT_ERROR index already exists or max_elements exceeds engine limit\r\n");
            return true;
        }
    }

    let mut indices = INDICES.write().unwrap();
    if indices.contains_key(&index_name) {
        if let Some(remove) = (*eng).remove {
            remove(
                eng as *mut ENGINE_HANDLE,
                cookie,
                index_name.as_ptr() as *const c_void,
                index_name.len(),
                0,
                0,
            );
        }
        send_response(response_handler, cookie, "CLIENT_ERROR index already exists\r\n");
        return true;
    }

    let storage = match hnsw_max_elements {
        Some(max) => IndexStorage::Hnsw(HnswWrapper::new(metric, max as usize)),
        None => IndexStorage::Flat,
    };

    indices.insert(
        index_name,
        Arc::new(VectorIndex {
            dimension,
            metric,
            vectors: RwLock::new(HashMap::new()),
            storage,
        }),
    );

    send_response(response_handler, cookie, "CREATED\r\n");
    true
}

unsafe fn map_get_payload(
    cookie: *const c_void,
    map_key: &str,
    vector_id: &str,
    dimension: usize,
) -> Vec<u8> {
    let eng = ensure_engine();
    if eng.is_null() { return vec![]; }
    let map_elem_get_fn = match (*eng).map_elem_get { Some(f) => f, None => return vec![] };
    let map_elem_release_fn = match (*eng).map_elem_release { Some(f) => f, None => return vec![] };
    let get_elem_info_fn = match (*eng).get_elem_info { Some(f) => f, None => return vec![] };

    let field = field_t {
        value: vector_id.as_ptr() as *mut c_char,
        length: vector_id.len(),
    };

    let mut eresult = std::mem::zeroed::<elems_result>();
    let ret = map_elem_get_fn(
        eng as *mut ENGINE_HANDLE,
        cookie,
        map_key.as_ptr() as *const c_void,
        map_key.len() as c_int,
        1,
        &field,
        false,
        false,
        &mut eresult,
        0,
    );

    if ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS
        || eresult.elem_count == 0
        || eresult.elem_array.is_null()
    {
        if !eresult.elem_array.is_null() {
            map_elem_release_fn(eng as *mut ENGINE_HANDLE, cookie,
                eresult.elem_array, eresult.elem_count as c_int);
            free(eresult.elem_array as *mut c_void);
        }
        return vec![];
    }

    let elem_ptr = *eresult.elem_array;
    let mut info = std::mem::zeroed::<eitem_info>();
    get_elem_info_fn(eng as *mut ENGINE_HANDLE, cookie, ITEM_TYPE_MAP, elem_ptr, &mut info);

    let payload = if !info.value.is_null() && info.nbytes as usize > dimension * 4 {
        let payload_offset = dimension * 4;
        let payload_len = info.nbytes as usize - payload_offset;
        let ptr = (info.value as *const u8).add(payload_offset);
        std::slice::from_raw_parts(ptr, payload_len).to_vec()
    } else {
        vec![]
    };

    map_elem_release_fn(eng as *mut ENGINE_HANDLE, cookie,
        eresult.elem_array, eresult.elem_count as c_int);
    free(eresult.elem_array as *mut c_void);

    payload
}

// Query the engine's configured max_element_bytes (per-collection-element limit).
unsafe fn get_max_element_bytes(cookie: *const c_void) -> u32 {
    let eng = ensure_engine();
    if eng.is_null() { return DEFAULT_MAX_ELEMENT_BYTES; }
    let get_config = match (*eng).get_config { Some(f) => f, None => return DEFAULT_MAX_ELEMENT_BYTES };
    let mut maxbytes: u32 = 0;
    let key = b"max_element_bytes\0";
    let ret = get_config(
        eng as *mut ENGINE_HANDLE,
        cookie,
        key.as_ptr() as *const c_char,
        &mut maxbytes as *mut u32 as *mut c_void,
    );
    if ret == ENGINE_ERROR_CODE_ENGINE_SUCCESS && maxbytes > 0 {
        maxbytes
    } else {
        DEFAULT_MAX_ELEMENT_BYTES
    }
}

unsafe fn execute_vadd_with_payload(
    cookie: *const c_void,
    index_name: &str,
    vector_id: &str,
    vector_floats: &[f32],
    payload: &[u8],
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let index_arc = {
        let reg = INDICES.read().unwrap();
        match reg.get(index_name) {
            Some(arc) => Arc::clone(arc),
            None => {
                send_response(response_handler, cookie, "CLIENT_ERROR index not found\r\n");
                return true;
            }
        }
    };

    if vector_floats.len() != index_arc.dimension {
        send_response(response_handler, cookie, "CLIENT_ERROR vector dimension mismatch\r\n");
        return true;
    }

    // Build the engine wire bytes with a fallible reservation so an
    // out-of-memory condition becomes a SERVER_ERROR rather than a daemon abort.
    let mut value_bytes: Vec<u8> = Vec::new();
    if value_bytes.try_reserve_exact(vector_floats.len() * 4 + payload.len()).is_err() {
        send_response(response_handler, cookie, "SERVER_ERROR out of memory\r\n");
        return true;
    }
    for f in vector_floats {
        value_bytes.extend_from_slice(&f.to_ne_bytes());
    }
    value_bytes.extend_from_slice(payload);

    // Enforce the engine's per-element size limit. The normal protocol path
    // (mop/bop/... insert) checks `max_element_bytes` before calling the engine,
    // but the direct engine API (map_elem_alloc) does not. Without this guard an
    // oversized element reaches the slab allocator and crashes the daemon
    // (assert in do_smmgr_free, slabs.c).
    let max_elem = get_max_element_bytes(cookie);
    if value_bytes.len() > max_elem as usize {
        send_response(response_handler, cookie,
            &format!("CLIENT_ERROR vector+payload too large ({} bytes > max_element_bytes {})\r\n",
                     value_bytes.len(), max_elem));
        return true;
    }

    // The module-side search structures (HNSW graph + `vectors` map) live in the
    // process heap, which is NOT bounded by `-m` and uses Rust's default
    // *infallible* allocator: a shortage there would abort the whole daemon.
    // Reserve the growable module allocations up front with try_reserve, before
    // the engine store, so a memory shortage is reported as SERVER_ERROR with
    // nothing stored anywhere.
    let mut owned_vec: Vec<f32> = Vec::new();
    let vectors_reserved = index_arc.vectors.write().unwrap().try_reserve(1).is_ok();
    if !vectors_reserved || owned_vec.try_reserve_exact(vector_floats.len()).is_err() {
        send_response(response_handler, cookie, "SERVER_ERROR out of memory\r\n");
        return true;
    }
    owned_vec.extend_from_slice(vector_floats);

    match map_insert_elem(cookie, index_name, vector_id.as_bytes(), &value_bytes, true) {
        MapInsertResult::KeyEvicted => {
            let mut reg = INDICES.write().unwrap();
            reg.remove(index_name);
            send_response(response_handler, cookie, "CLIENT_ERROR index evicted; use vcreate to rebuild\r\n");
            return true;
        }
        MapInsertResult::Error => {
            send_response(response_handler, cookie, "SERVER_ERROR storage write failed\r\n");
            return true;
        }
        MapInsertResult::Ok => {}
    }

    // Commit into the module index. The vectors-map slot was reserved above, and
    // HNSW reports allocation failure instead of aborting.
    if let IndexStorage::Hnsw(ref hnsw) = index_arc.storage {
        if hnsw.insert(vector_id, vector_floats).is_err() {
            send_response(response_handler, cookie, "SERVER_ERROR out of memory\r\n");
            return true;
        }
    }
    index_arc.vectors.write().unwrap().insert(vector_id.to_string(), owned_vec);

    send_response(response_handler, cookie, "STORED\r\n");
    true
}

unsafe fn execute_vsearch_core(
    cookie: *const c_void,
    index_name: &str,
    k: usize,
    query_vector: &[f32],
    threshold: Option<f32>,
    ef: Option<usize>,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let index_arc = {
        let reg = INDICES.read().unwrap();
        match reg.get(index_name) {
            Some(arc) => Arc::clone(arc),
            None => {
                send_response(response_handler, cookie, "CLIENT_ERROR index not found\r\n");
                return true;
            }
        }
    };

    if query_vector.len() != index_arc.dimension {
        send_response(response_handler, cookie, "CLIENT_ERROR query vector dimension mismatch\r\n");
        return true;
    }

    let results: Vec<(String, f32)> = match &index_arc.storage {
        IndexStorage::Hnsw(hnsw) => match hnsw.search(query_vector, k, ef) {
            Ok(r) => r,
            Err(_) => {
                send_response(response_handler, cookie, "SERVER_ERROR search failed (internal error)\r\n");
                return true;
            }
        },
        IndexStorage::Flat => {
            let vectors = index_arc.vectors.read().unwrap();
            let mut flat_results: Vec<(String, f32)> = vectors
                .iter()
                .map(|(vid, v)| {
                    let dist = compute_distance(query_vector, v, index_arc.metric);
                    (vid.clone(), dist)
                })
                .collect();
            flat_results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            flat_results.truncate(k);
            flat_results
        }
    };

    let mut out = String::new();
    for (vid, dist) in results.into_iter().filter(|(_, d)| threshold.map_or(true, |t| *d <= t)) {
        let payload = map_get_payload(cookie, index_name, &vid, index_arc.dimension);
        if payload.is_empty() {
            out.push_str(&format!("{} {} 0\r\n\r\n", vid, dist));
        } else {
            let payload_str = String::from_utf8_lossy(&payload);
            out.push_str(&format!("{} {} {}\r\n{}\r\n", vid, dist, payload.len(), payload_str));
        }
    }
    out.push_str("END\r\n");

    send_response(response_handler, cookie, &out);
    true
}

unsafe fn execute_vdel(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 3 {
        send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
        return true;
    }

    let index_name = get_token_string(&tokens[1]);
    let vector_id = get_token_string(&tokens[2]);

    let index_arc = {
        let reg = INDICES.read().unwrap();
        match reg.get(&index_name) {
            Some(arc) => Arc::clone(arc),
            None => {
                send_response(response_handler, cookie, "CLIENT_ERROR index not found\r\n");
                return true;
            }
        }
    };

    let removed = index_arc.vectors.write().unwrap().remove(&vector_id).is_some();

    if removed {
        if let IndexStorage::Hnsw(ref hnsw) = index_arc.storage {
            hnsw.delete(&vector_id);
        }
        let eng = ensure_engine();
        if !eng.is_null() {
            if let Some(map_elem_delete) = (*eng).map_elem_delete {
                let field = field_t {
                    value: vector_id.as_ptr() as *mut c_char,
                    length: vector_id.len(),
                };
                let mut del_count: u32 = 0;
                let mut dropped = false;
                map_elem_delete(
                    eng as *mut ENGINE_HANDLE,
                    cookie,
                    index_name.as_ptr() as *const c_void,
                    index_name.len() as c_int,
                    1,
                    &field,
                    false,
                    &mut del_count,
                    &mut dropped,
                    0,
                );
            }
        }
        send_response(response_handler, cookie, "DELETED\r\n");
    } else {
        send_response(response_handler, cookie, "NOT_FOUND\r\n");
    }
    true
}

unsafe fn execute_vdrop(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 2 {
        send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
        return true;
    }

    let index_name = get_token_string(&tokens[1]);

    let removed = {
        let mut reg = INDICES.write().unwrap();
        reg.remove(&index_name).is_some()
    };
    // INDICES 등록 여부와 무관하게 엔진 Map 삭제를 시도한다(orphan Map 정리).
    // 모듈과 엔진이 어긋난 상태에서도 vdrop 으로 stale Map 을 청소할 수 있게 한다.
    let eng = ensure_engine();
    if !eng.is_null() {
        if let Some(remove) = (*eng).remove {
            remove(
                eng as *mut ENGINE_HANDLE,
                cookie,
                index_name.as_ptr() as *const c_void,
                index_name.len(),
                0,
                0,
            );
        }
    }
    if removed {
        send_response(response_handler, cookie, "DROPPED\r\n");
    } else {
        send_response(response_handler, cookie, "NOT_FOUND\r\n");
    }
    true
}

unsafe fn execute_vlist(
    cookie: *const c_void,
    argc: c_int,
    _argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    if argc != 1 {
        send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
        return true;
    }

    let reg = INDICES.read().unwrap();
    if reg.is_empty() {
        send_response(response_handler, cookie, "END\r\n");
        return true;
    }

    let mut out = String::new();
    for (name, arc) in reg.iter() {
        out.push_str(&format!(
            "{} {} {} {}\r\n",
            name,
            arc.dimension,
            arc.vectors.read().unwrap().len(),
            arc.storage.type_str()
        ));
    }
    out.push_str("END\r\n");

    send_response(response_handler, cookie, &out);
    true
}

// Called once the vector (and optional payload) blob has been read via nread.
unsafe fn execute_pending_nread(
    cookie: *const c_void,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let state = PENDING.lock().unwrap().remove(&(cookie as usize));
    let state = match state {
        Some(s) => s,
        None => {
            send_response(response_handler, cookie, "SERVER_ERROR lost command state\r\n");
            return true;
        }
    };
    let data = std::slice::from_raw_parts(state.buffer, state.data_len).to_vec();
    free(state.buffer as *mut c_void);

    match state.cmd {
        PendingCmd::Vadd { index_name, vector_id, vec_bytes } => {
            let vector_floats = bytes_to_floats(&data[..vec_bytes]);
            let payload = &data[vec_bytes..];
            if !is_valid_vector(&vector_floats) {
                send_response(response_handler, cookie, "CLIENT_ERROR invalid vector (NaN or infinite values)\r\n");
                return true;
            }
            execute_vadd_with_payload(cookie, &index_name, &vector_id, &vector_floats, payload, response_handler)
        }
        PendingCmd::Vsearch { index_name, k, vec_bytes, threshold, ef } => {
            let query_vector = bytes_to_floats(&data[..vec_bytes]);
            if !is_valid_vector(&query_vector) {
                send_response(response_handler, cookie, "CLIENT_ERROR invalid vector (NaN or infinite values)\r\n");
                return true;
            }
            execute_vsearch_core(cookie, &index_name, k, &query_vector, threshold, ef, response_handler)
        }
    }
}

unsafe extern "C" fn accept_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    ndata: *mut usize,
    ptr: *mut *mut c_char,
) -> bool {
    if argc < 1 { return false; }
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    let cmd = get_token_string(&tokens[0]);
    match cmd.as_str() {
        "vcreate" | "vdel" | "vdrop" | "vlist" => true,
        // vadd <index> <id> <vec_bytes> [payload_len]
        // then <float32 vector blob><payload>\r\n via nread
        "vadd" => {
            if argc != 4 && argc != 5 {
                return true; // malformed: execute() reports the error
            }
            let vec_bytes: usize = match get_token_string(&tokens[3]).parse() {
                Ok(n) if n > 0 && n % 4 == 0 && n <= MAX_VEC_BYTES => n,
                _ => return true,
            };
            let payload_len: usize = if argc == 5 {
                match get_token_string(&tokens[4]).parse() {
                    Ok(n) if n <= MAX_PAYLOAD_LEN => n,
                    _ => return true,
                }
            } else {
                0
            };
            let data_len = vec_bytes + payload_len;
            let buf = malloc(data_len + 2) as *mut u8;
            if buf.is_null() { return true; }
            *ndata = data_len + 2;
            *ptr = buf as *mut c_char;
            let index_name = get_token_string(&tokens[1]);
            let vector_id = get_token_string(&tokens[2]);
            PENDING.lock().unwrap().insert(cookie as usize, PendingState {
                cmd: PendingCmd::Vadd { index_name, vector_id, vec_bytes },
                buffer: buf,
                data_len,
            });
            true
        }
        // vsearch <index> <k> <vec_bytes> [threshold] [ef]
        //   threshold 자리에 "-" 를 주면 임계값 없이 ef 만 지정할 수 있다.
        // then <float32 query blob>\r\n via nread
        "vsearch" => {
            if argc < 4 || argc > 6 {
                return true;
            }
            let k: usize = match get_token_string(&tokens[2]).parse() {
                Ok(n) => n,
                _ => return true,
            };
            let vec_bytes: usize = match get_token_string(&tokens[3]).parse() {
                Ok(n) if n > 0 && n % 4 == 0 && n <= MAX_VEC_BYTES => n,
                _ => return true,
            };
            let threshold: Option<f32> = if argc >= 5 {
                let t = get_token_string(&tokens[4]);
                if t == "-" {
                    None
                } else {
                    match t.parse() {
                        Ok(v) => Some(v),
                        _ => return true,
                    }
                }
            } else {
                None
            };
            let ef: Option<usize> = if argc == 6 {
                match get_token_string(&tokens[5]).parse() {
                    Ok(n) => Some(n),
                    _ => return true,
                }
            } else {
                None
            };
            let buf = malloc(vec_bytes + 2) as *mut u8;
            if buf.is_null() { return true; }
            *ndata = vec_bytes + 2;
            *ptr = buf as *mut c_char;
            let index_name = get_token_string(&tokens[1]);
            PENDING.lock().unwrap().insert(cookie as usize, PendingState {
                cmd: PendingCmd::Vsearch { index_name, k, vec_bytes, threshold, ef },
                buffer: buf,
                data_len: vec_bytes,
            });
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
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    if argc == 0 {
        return execute_pending_nread(cookie, response_handler);
    }

    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    let cmd = get_token_string(&tokens[0]);
    match cmd.as_str() {
        "vcreate" => execute_vcreate(cookie, argc, argv, response_handler),
        "vdel" => execute_vdel(cookie, argc, argv, response_handler),
        "vdrop" => execute_vdrop(cookie, argc, argv, response_handler),
        "vlist" => execute_vlist(cookie, argc, argv, response_handler),
        "vadd" | "vsearch" => {
            // Well-formed vadd/vsearch are completed via nread (argc == 0).
            // Reaching here means the command line itself was malformed.
            send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
            true
        }
        _ => {
            send_response(response_handler, cookie, "CLIENT_ERROR unknown command\r\n");
            true
        }
    }
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    if let Some(state) = PENDING.lock().unwrap().remove(&(cookie as usize)) {
        free(state.buffer as *mut c_void);
    }
}

unsafe extern "C" fn get_name_vector(_cmd_cookie: *const c_void) -> *const c_char {
    b"arcus-vector-db\0".as_ptr() as *const c_char
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
        let get_api = match get_server_api {
            Some(f) => f,
            None => return EXTENSION_ERROR_CODE_EXTENSION_FATAL,
        };
        let _ = GET_SERVER_API_FN.set(get_api);

        let server = get_api();
        if server.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }

        let ext = (*server).extension;
        if ext.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
        let register_extension = match (*ext).register_extension {
            Some(f) => f,
            None => return EXTENSION_ERROR_CODE_EXTENSION_FATAL,
        };

        let res = register_extension(
            extension_type_t_EXTENSION_ASCII_PROTOCOL,
            &raw mut VECTOR_DESCRIPTOR as *mut _ as *mut c_void,
        );
        if !res {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }
    }
    EXTENSION_ERROR_CODE_EXTENSION_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance_l2() {
        let q = vec![1.0, 2.0];
        let v = vec![1.0, 3.0];
        assert_eq!(compute_distance(&q, &v, MetricType::L2), 1.0);
    }

    #[test]
    fn test_distance_cosine() {
        let q = vec![1.0, 0.0];
        let v = vec![0.0, 1.0];
        assert_eq!(compute_distance(&q, &v, MetricType::Cosine), 1.0);
        let v2 = vec![1.0, 0.0];
        assert_eq!(compute_distance(&q, &v2, MetricType::Cosine), 0.0);
    }

    #[test]
    fn test_distance_ip() {
        let q = vec![1.0, 2.0];
        let v = vec![2.0, 3.0];
        assert_eq!(compute_distance(&q, &v, MetricType::IP), -8.0);
    }

    #[test]
    fn test_hnsw_insert_search() {
        let hnsw = HnswWrapper::new(MetricType::L2, 100_000);
        hnsw.insert("a", &[1.0, 0.0]).unwrap();
        hnsw.insert("b", &[0.0, 1.0]).unwrap();
        hnsw.insert("c", &[1.0, 1.0]).unwrap();

        let results = hnsw.search(&[1.0, 0.0], 1, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "a");
        assert_eq!(results[0].1, 0.0);
    }

    #[test]
    fn test_hnsw_delete() {
        let hnsw = HnswWrapper::new(MetricType::L2, 100_000);
        hnsw.insert("a", &[1.0, 0.0]).unwrap();
        hnsw.insert("b", &[0.9, 0.1]).unwrap();

        hnsw.delete("a");
        let results = hnsw.search(&[1.0, 0.0], 1, None).unwrap();
        assert_eq!(results[0].0, "b");
    }

    #[test]
    fn test_hnsw_concurrent_insert_search() {
        use std::sync::Arc;
        use std::thread;

        let hnsw = Arc::new(HnswWrapper::new(MetricType::L2, 100_000));

        let handles: Vec<_> = (0..4).map(|i| {
            let h = Arc::clone(&hnsw);
            thread::spawn(move || {
                for j in 0..10 {
                    let id = format!("vec_{}_{}", i, j);
                    let vec = vec![i as f32, j as f32];
                    h.insert(&id, &vec).unwrap();
                }
            })
        }).collect();

        let search_handles: Vec<_> = (0..4).map(|_| {
            let h = Arc::clone(&hnsw);
            thread::spawn(move || {
                for _ in 0..5 {
                    let _ = h.search(&[1.0, 1.0], 3, None);
                }
            })
        }).collect();

        for h in handles { h.join().unwrap(); }
        for h in search_handles { h.join().unwrap(); }

        let results = hnsw.search(&[0.0, 0.0], 5, None).unwrap();
        assert!(results.len() <= 5);
    }
}
