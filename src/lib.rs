#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unnecessary_transmutes)]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod engine_api;

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::{LazyLock, Mutex};

use engine_api::*;

type EXTENSION_RESPONSE_HANDLER = Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetricType {
    L2 = 0,
    Cosine = 1,
    IP = 2,
}

struct VectorIndex {
    dimension: usize,
    metric: MetricType,
    vectors: HashMap<String, Vec<f32>>,
}

static mut ENGINE: *mut engine_interface_v1 = ptr::null_mut();
static mut SERVER_API: *mut SERVER_HANDLE_V1 = ptr::null_mut();

static INDICES: LazyLock<Mutex<HashMap<String, VectorIndex>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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

fn calculate_distance(q: &[f32], v: &[f32], metric: MetricType) -> f32 {
    let len = q.len().min(v.len());
    match metric {
        MetricType::L2 => {
            let mut dist = 0.0;
            for i in 0..len {
                let diff = q[i] - v[i];
                dist += diff * diff;
            }
            dist
        }
        MetricType::Cosine => {
            let mut dot = 0.0;
            let mut norm_q = 0.0;
            let mut norm_v = 0.0;
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
            let mut dot = 0.0;
            for i in 0..len {
                dot += q[i] * v[i];
            }
            -dot
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

unsafe fn execute_vcreate(
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

    let mut map = INDICES.lock().unwrap();
    if map.contains_key(&index_name) {
        send_response(response_handler, cookie, "CLIENT_ERROR index already exists\r\n");
        return true;
    }

    let meta_key = format!("vmeta_{}", index_name);
    let mut meta_bytes = [0u8; 12];
    meta_bytes[0..8].copy_from_slice(&dimension.to_ne_bytes());
    meta_bytes[8] = metric as u8;

    let allocate = (*ENGINE).allocate.unwrap();
    let store = (*ENGINE).store.unwrap();
    let get_item_info = (*ENGINE).get_item_info.unwrap();
    let release = (*ENGINE).release.unwrap();

    let mut item_ptr: *mut item = ptr::null_mut();
    let err = allocate(
        ENGINE as *mut ENGINE_HANDLE,
        cookie,
        &mut item_ptr,
        meta_key.as_ptr() as *const c_void,
        meta_key.len(),
        12,
        0,
        0,
        0,
    );

    if err == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        let mut info = std::mem::zeroed::<item_info>();
        get_item_info(ENGINE as *mut ENGINE_HANDLE, cookie, item_ptr, &mut info);
        ptr::copy_nonoverlapping(meta_bytes.as_ptr(), info.value as *mut u8, 12);

        let mut cas: u64 = 0;
        store(
            ENGINE as *mut ENGINE_HANDLE,
            cookie,
            item_ptr,
            &mut cas,
            ENGINE_STORE_OPERATION_OPERATION_SET,
            0,
        );
        release(ENGINE as *mut ENGINE_HANDLE, cookie, item_ptr);
    }

    map.insert(
        index_name,
        VectorIndex {
            dimension,
            metric,
            vectors: HashMap::new(),
        },
    );

    send_response(response_handler, cookie, "CREATED\r\n");
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
    let vector_id = get_token_string(&tokens[2]);
    let float_str = get_token_string(&tokens[3]);
    let vector_floats = parse_vector(&float_str);

    let mut map = INDICES.lock().unwrap();
    let index = match map.get_mut(&index_name) {
        Some(idx) => idx,
        None => {
            send_response(response_handler, cookie, "CLIENT_ERROR index not found\r\n");
            return true;
        }
    };

    if vector_floats.len() != index.dimension {
        send_response(response_handler, cookie, "CLIENT_ERROR vector dimension mismatch\r\n");
        return true;
    }

    let vec_key = format!("vec_{}_{}", index_name, vector_id);
    let bytes_len = index.dimension * 4;

    let allocate = (*ENGINE).allocate.unwrap();
    let store = (*ENGINE).store.unwrap();
    let get_item_info = (*ENGINE).get_item_info.unwrap();
    let release = (*ENGINE).release.unwrap();

    let mut item_ptr: *mut item = ptr::null_mut();
    let err = allocate(
        ENGINE as *mut ENGINE_HANDLE,
        cookie,
        &mut item_ptr,
        vec_key.as_ptr() as *const c_void,
        vec_key.len(),
        bytes_len,
        0,
        0,
        0,
    );

    if err == ENGINE_ERROR_CODE_ENGINE_SUCCESS {
        let mut info = std::mem::zeroed::<item_info>();
        get_item_info(ENGINE as *mut ENGINE_HANDLE, cookie, item_ptr, &mut info);
        
        let floats_bytes = std::slice::from_raw_parts(vector_floats.as_ptr() as *const u8, bytes_len);
        ptr::copy_nonoverlapping(floats_bytes.as_ptr(), info.value as *mut u8, bytes_len);

        let mut cas: u64 = 0;
        store(
            ENGINE as *mut ENGINE_HANDLE,
            cookie,
            item_ptr,
            &mut cas,
            ENGINE_STORE_OPERATION_OPERATION_SET,
            0,
        );
        release(ENGINE as *mut ENGINE_HANDLE, cookie, item_ptr);
    }

    index.vectors.insert(vector_id, vector_floats);

    send_response(response_handler, cookie, "STORED\r\n");
    true
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

    let map = INDICES.lock().unwrap();
    let index = match map.get(&index_name) {
        Some(idx) => idx,
        None => {
            send_response(response_handler, cookie, "CLIENT_ERROR index not found\r\n");
            return true;
        }
    };

    if query_vector.len() != index.dimension {
        send_response(response_handler, cookie, "CLIENT_ERROR query vector dimension mismatch\r\n");
        return true;
    }

    let mut results = Vec::new();
    for (vid, v) in &index.vectors {
        let dist = calculate_distance(&query_vector, v, index.metric);
        results.push((vid.clone(), dist));
    }

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    results.truncate(k);

    let mut out = String::new();
    for (vid, dist) in results {
        out.push_str(&format!("{} {}\r\n", vid, dist));
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

    let mut map = INDICES.lock().unwrap();
    let index = match map.get_mut(&index_name) {
        Some(idx) => idx,
        None => {
            send_response(response_handler, cookie, "CLIENT_ERROR index not found\r\n");
            return true;
        }
    };

    if index.vectors.remove(&vector_id).is_some() {
        let vec_key = format!("vec_{}_{}", index_name, vector_id);
        let remove = (*ENGINE).remove.unwrap();
        remove(
            ENGINE as *mut ENGINE_HANDLE,
            cookie,
            vec_key.as_ptr() as *const c_void,
            vec_key.len(),
            0,
            0,
        );
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

    let mut map = INDICES.lock().unwrap();
    if map.remove(&index_name).is_some() {
        let meta_key = format!("vmeta_{}", index_name);
        let remove = (*ENGINE).remove.unwrap();
        remove(
            ENGINE as *mut ENGINE_HANDLE,
            cookie,
            meta_key.as_ptr() as *const c_void,
            meta_key.len(),
            0,
            0,
        );
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

    let map = INDICES.lock().unwrap();
    if map.is_empty() {
        send_response(response_handler, cookie, "END\r\n");
        return true;
    }

    let mut out = String::new();
    for (name, idx) in map.iter() {
        out.push_str(&format!("{} {} {}\r\n", name, idx.dimension, idx.vectors.len()));
    }
    out.push_str("END\r\n");

    send_response(response_handler, cookie, &out);
    true
}

unsafe extern "C" fn accept_vector_cmd(
    _cmd_cookie: *const c_void,
    _cookie: *mut c_void,
    argc: c_int,
    argv: *mut token_t,
    _ndata: *mut usize,
    _ptr: *mut *mut c_char,
) -> bool {
    if argc > 0 {
        let tokens = std::slice::from_raw_parts(argv, argc as usize);
        let cmd = get_token_string(&tokens[0]);
        if cmd == "vcreate" || cmd == "vadd" || cmd == "vsearch" || cmd == "vdel" || cmd == "vdrop" || cmd == "vlist" {
            return true;
        }
    }
    false
}

unsafe extern "C" fn execute_vector_cmd(
    _cmd_cookie: *const c_void,
    cookie: *const c_void,
    argc: c_int,
    argv: *mut token_t,
    response_handler: EXTENSION_RESPONSE_HANDLER,
) -> bool {
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

unsafe extern "C" fn get_name_vector(
    _cmd_cookie: *const c_void,
) -> *const c_char {
    b"arcus-vector-db\0".as_ptr() as *const c_char
}

static mut VECTOR_DESCRIPTOR: EXTENSION_ASCII_PROTOCOL_DESCRIPTOR = EXTENSION_ASCII_PROTOCOL_DESCRIPTOR {
    get_name: Some(get_name_vector),
    get_auth_flag: None,
    accept: Some(accept_vector_cmd),
    execute: Some(execute_vector_cmd),
    abort: None,
    cookie: ptr::null_mut(),
    next: ptr::null_mut(),
};

#[unsafe(no_mangle)]
pub extern "C" fn memcached_extensions_initialize(
    v1: *const c_char,
    get_server_api: GET_SERVER_API,
) -> EXTENSION_ERROR_CODE {
    unsafe {
        let expected = CStr::from_bytes_with_nul_unchecked(b"1\0");
        if CStr::from_ptr(v1) != expected {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }

        let get_api = match get_server_api {
            Some(f) => f,
            None => return EXTENSION_ERROR_CODE_EXTENSION_FATAL,
        };

        let server = get_api();
        if server.is_null() {
            return EXTENSION_ERROR_CODE_EXTENSION_FATAL;
        }

        SERVER_API = server;
        ENGINE = (*server).engine as *mut engine_interface_v1;

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
        assert_eq!(calculate_distance(&q, &v, MetricType::L2), 1.0);
    }

    #[test]
    fn test_distance_cosine() {
        let q = vec![1.0, 0.0];
        let v = vec![0.0, 1.0];
        assert_eq!(calculate_distance(&q, &v, MetricType::Cosine), 1.0);
        let v2 = vec![1.0, 0.0];
        assert_eq!(calculate_distance(&q, &v2, MetricType::Cosine), 0.0);
    }

    #[test]
    fn test_distance_ip() {
        let q = vec![1.0, 2.0];
        let v = vec![2.0, 3.0];
        assert_eq!(calculate_distance(&q, &v, MetricType::IP), -8.0);
    }
}
