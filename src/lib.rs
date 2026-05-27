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
            1.0 - (dot / (norm_q.sqrt() * norm_v.sqrt()))
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
        }
    }

    fn insert(&self, id: &str, vec: &[f32]) {
        let numeric_id = {
            let mut state = self.id_state.lock().unwrap();
            let nid = state.id_map.len();
            state.id_map.push(id.to_string());
            state.str_map.insert(id.to_string(), nid);
            nid
        };
        self.inner.insert((vec, numeric_id));
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

    fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let (fetch_k, deleted_snapshot) = {
            let state = self.id_state.lock().unwrap();
            (k + state.deleted.len(), state.deleted.clone())
        };
        let ef_search = (k * 10).max(50);
        let results = self.inner.search(query, fetch_k, ef_search);
        let state = self.id_state.lock().unwrap();
        results
            .into_iter()
            .filter(|n| !deleted_snapshot.contains(&n.d_id))
            .take(k)
            .map(|n| (state.id_map[n.d_id].clone(), n.distance))
            .collect()
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

struct VaddPendingState {
    index_name: String,
    vector_id: String,
    vector_floats: Vec<f32>,
    payload_len: usize,
    buffer: *mut u8,
}

unsafe impl Send for VaddPendingState {}
unsafe impl Sync for VaddPendingState {}

static VADD_PENDING: LazyLock<Mutex<HashMap<usize, VaddPendingState>>> =
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

fn parse_vector(token_str: &str) -> Vec<f32> {
    let trimmed = token_str.trim_matches(|c| c == '[' || c == ']');
    trimmed
        .split(',')
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect()
}

const META_FIELD: &[u8] = b"_meta";
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

    // vcreate <name> <dim> <metric>              → FLAT (default)
    // vcreate <name> <dim> <metric> FLAT         → FLAT
    // vcreate <name> <dim> <metric> HNSW         → HNSW, max_elements=1_000_000
    // vcreate <name> <dim> <metric> HNSW <max>   → HNSW, max_elements=<max>
    let storage = if argc >= 5 {
        match get_token_string(&tokens[4]).to_uppercase().as_str() {
            "HNSW" => {
                let max_elements = if argc == 6 {
                    match get_token_string(&tokens[5]).parse::<usize>() {
                        Ok(n) if n > 0 => n,
                        _ => {
                            send_response(response_handler, cookie, "CLIENT_ERROR bad max_elements\r\n");
                            return true;
                        }
                    }
                } else {
                    1_000_000
                };
                IndexStorage::Hnsw(HnswWrapper::new(metric, max_elements))
            }
            "FLAT" => {
                if argc == 6 {
                    send_response(response_handler, cookie, "CLIENT_ERROR FLAT does not take max_elements\r\n");
                    return true;
                }
                IndexStorage::Flat
            }
            _ => {
                send_response(response_handler, cookie, "CLIENT_ERROR unsupported index type (use FLAT or HNSW)\r\n");
                return true;
            }
        }
    } else {
        IndexStorage::Flat
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
    let create_ret = map_struct_create_fn(
        eng as *mut ENGINE_HANDLE,
        cookie,
        index_name.as_ptr() as *const c_void,
        index_name.len() as c_int,
        &mut attrp,
        0,
    );
    if create_ret != ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        send_response(response_handler, cookie, "CLIENT_ERROR index already exists\r\n");
        return true;
    }

    let mut meta_bytes = [0u8; 9];
    meta_bytes[0..8].copy_from_slice(&(dimension as u64).to_ne_bytes());
    meta_bytes[8] = metric as u8;
    map_insert_elem(cookie, &index_name, META_FIELD, &meta_bytes, false);

    let mut indices = INDICES.write().unwrap();
    if indices.contains_key(&index_name) {
        // map_struct_create는 성공했으므로 ARCUS에 생성된 맵을 제거
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

    let mut value_bytes: Vec<u8> = vector_floats.iter()
        .flat_map(|f| f.to_ne_bytes())
        .collect();
    value_bytes.extend_from_slice(payload);
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

    if let IndexStorage::Hnsw(ref hnsw) = index_arc.storage {
        hnsw.insert(vector_id, vector_floats);
    }
    index_arc.vectors.write().unwrap().insert(vector_id.to_string(), vector_floats.to_vec());

    send_response(response_handler, cookie, "STORED\r\n");
    true
}

unsafe fn execute_vadd(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 4 {
        send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
        return true;
    }
    let index_name = get_token_string(&tokens[1]);
    let vector_id  = get_token_string(&tokens[2]);
    let float_str  = get_token_string(&tokens[3]);
    let vector_floats = parse_vector(&float_str);
    execute_vadd_with_payload(cookie, &index_name, &vector_id, &vector_floats, &[], response_handler)
}

unsafe fn execute_vadd_nread(
    cookie: *const c_void,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let state = VADD_PENDING.lock().unwrap().remove(&(cookie as usize));
    let state = match state {
        Some(s) => s,
        None => {
            send_response(response_handler, cookie, "SERVER_ERROR lost vadd state\r\n");
            return true;
        }
    };
    let payload = std::slice::from_raw_parts(state.buffer, state.payload_len).to_vec();
    free(state.buffer as *mut c_void);

    execute_vadd_with_payload(
        cookie,
        &state.index_name,
        &state.vector_id,
        &state.vector_floats,
        &payload,
        response_handler,
    )
}

unsafe fn execute_vsearch(
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    if argc != 4 {
        send_response(response_handler, cookie, "CLIENT_ERROR bad command line format\r\n");
        return true;
    }

    let index_name = get_token_string(&tokens[1]);
    let k: usize = match get_token_string(&tokens[2]).parse() {
        Ok(v) => v,
        Err(_) => {
            send_response(response_handler, cookie, "CLIENT_ERROR bad k\r\n");
            return true;
        }
    };
    let float_str = get_token_string(&tokens[3]);
    let query_vector = parse_vector(&float_str);

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

    if query_vector.len() != index_arc.dimension {
        send_response(response_handler, cookie, "CLIENT_ERROR query vector dimension mismatch\r\n");
        return true;
    }

    let results: Vec<(String, f32)> = match &index_arc.storage {
        IndexStorage::Hnsw(hnsw) => hnsw.search(&query_vector, k),
        IndexStorage::Flat => {
            let vectors = index_arc.vectors.read().unwrap();
            let mut flat_results: Vec<(String, f32)> = vectors
                .iter()
                .map(|(vid, v)| {
                    let dist = compute_distance(&query_vector, v, index_arc.metric);
                    (vid.clone(), dist)
                })
                .collect();
            flat_results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            flat_results.truncate(k);
            flat_results
        }
    };

    let mut out = String::new();
    for (vid, dist) in results {
        let payload = map_get_payload(cookie, &index_name, &vid, index_arc.dimension);
        if payload.is_empty() {
            out.push_str(&format!("{} {}\r\n", vid, dist));
        } else {
            let payload_str = String::from_utf8_lossy(&payload);
            out.push_str(&format!("{} {} {}\r\n", vid, dist, payload_str));
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
    if removed {
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
        "vcreate" | "vsearch" | "vdel" | "vdrop" | "vlist" => true,
        "vadd" => {
            if argc == 4 {
                true
            } else if argc == 5 {
                let len_str = get_token_string(&tokens[4]);
                let payload_len: usize = match len_str.parse() {
                    Ok(n) => n,
                    Err(_) => return false,
                };
                let buf_size = payload_len + 2;
                let buf = malloc(buf_size) as *mut u8;
                if buf.is_null() { return false; }

                *ndata = buf_size;
                *ptr = buf as *mut c_char;

                let index_name    = get_token_string(&tokens[1]);
                let vector_id     = get_token_string(&tokens[2]);
                let vector_floats = parse_vector(&get_token_string(&tokens[3]));
                VADD_PENDING.lock().unwrap().insert(cookie as usize, VaddPendingState {
                    index_name,
                    vector_id,
                    vector_floats,
                    payload_len,
                    buffer: buf,
                });
                true
            } else {
                false
            }
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
        return execute_vadd_nread(cookie, response_handler);
    }

    let tokens = std::slice::from_raw_parts(argv, argc as usize);
    let cmd = get_token_string(&tokens[0]);
    match cmd.as_str() {
        "vcreate" => execute_vcreate(cookie, argc, argv, response_handler),
        "vadd" => execute_vadd(cookie, argc, argv, response_handler),
        "vsearch" => execute_vsearch(cookie, argc, argv, response_handler),
        "vdel" => execute_vdel(cookie, argc, argv, response_handler),
        "vdrop" => execute_vdrop(cookie, argc, argv, response_handler),
        "vlist" => execute_vlist(cookie, argc, argv, response_handler),
        _ => {
            send_response(response_handler, cookie, "CLIENT_ERROR unknown command\r\n");
            true
        }
    }
}

unsafe extern "C" fn abort_vector_cmd(_cmd_cookie: *const c_void, cookie: *const c_void) {
    if let Some(state) = VADD_PENDING.lock().unwrap().remove(&(cookie as usize)) {
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
        hnsw.insert("a", &[1.0, 0.0]);
        hnsw.insert("b", &[0.0, 1.0]);
        hnsw.insert("c", &[1.0, 1.0]);

        let results = hnsw.search(&[1.0, 0.0], 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "a");
        assert_eq!(results[0].1, 0.0);
    }

    #[test]
    fn test_hnsw_delete() {
        let hnsw = HnswWrapper::new(MetricType::L2, 100_000);
        hnsw.insert("a", &[1.0, 0.0]);
        hnsw.insert("b", &[0.9, 0.1]);

        hnsw.delete("a");
        let results = hnsw.search(&[1.0, 0.0], 1);
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
                    h.insert(&id, &vec);
                }
            })
        }).collect();

        let search_handles: Vec<_> = (0..4).map(|_| {
            let h = Arc::clone(&hnsw);
            thread::spawn(move || {
                for _ in 0..5 {
                    let _ = h.search(&[1.0, 1.0], 3);
                }
            })
        }).collect();

        for h in handles { h.join().unwrap(); }
        for h in search_handles { h.join().unwrap(); }

        let results = hnsw.search(&[0.0, 0.0], 5);
        assert!(results.len() <= 5);
    }
}
