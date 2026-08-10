//! ASCII protocol plumbing: argument access, replies, and the two-phase body
//! transfer memcached calls `nread`.

use std::collections::HashMap;
use std::os::raw::{c_char, c_int, c_void};
use std::str::FromStr;
use std::sync::{LazyLock, Mutex, PoisonError};

use crate::engine_api::token_t;
use crate::error::{Error, Reply, Result};

/// Upper bound on one transferred blob, so a malformed length cannot ask for an
/// enormous allocation.
pub const MAX_BLOB_BYTES: usize = 8 * 1024 * 1024;

/// Mirrors memcached's own `MAX_TOKENS` (memcached.c:8066). At this many tokens
/// the tokenizer stops splitting and leaves the remainder of the line in one
/// undelimited piece.
pub const MAX_TOKENS: usize = 30;

pub type ResponseHandler =
    Option<unsafe extern "C" fn(*const c_void, c_int, *const c_char) -> bool>;

/// Borrowed view of a tokenized command line.
///
/// Tokens point into memcached's connection buffer and are valid only for the
/// callback that produced them, which `'a` ties them to.
#[derive(Clone, Copy)]
pub struct Tokens<'a> {
    tokens: &'a [token_t],
}

impl<'a> Tokens<'a> {
    /// # Safety
    ///
    /// `argv` must point to `argc` tokens that stay valid for `'a`.
    pub unsafe fn new(argv: *const token_t, argc: c_int) -> Tokens<'a> {
        let len = argc.max(0) as usize;
        // SAFETY: guaranteed by the caller; a zero length tolerates a dangling
        // pointer only because from_raw_parts is never called with one.
        let tokens = if len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(argv, len) }
        };
        Tokens { tokens }
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The command name, or an empty string for a body-only invocation.
    pub fn command(&self) -> &'a str {
        self.text(0).unwrap_or("")
    }

    /// Borrow token `i` as UTF-8.
    pub fn text(&self, i: usize) -> Result<&'a str> {
        let token = self
            .tokens
            .get(i)
            .ok_or_else(|| Error::bad_request("bad command line format"))?;
        if token.value.is_null() || token.length == 0 {
            return Ok("");
        }
        // SAFETY: the token points to `length` bytes of the connection buffer,
        // which outlives `'a`.
        let bytes = unsafe { std::slice::from_raw_parts(token.value.cast::<u8>(), token.length) };
        std::str::from_utf8(bytes).map_err(|_| Error::bad_request("argument is not valid UTF-8"))
    }

    /// Parse token `i`, naming it in the error message.
    pub fn parse<T: FromStr>(&self, i: usize, what: &str) -> Result<T> {
        let raw = self.text(i)?;
        raw.parse::<T>()
            .map_err(|_| Error::bad_request(format!("invalid {what} '{raw}'")))
    }

    /// Reassemble the tail of the command line from token `from` onward,
    /// recovering the bytes exactly as the client sent them.
    ///
    /// [`tokenize_command`] splits on single spaces and overwrites each one with
    /// `'\0'` in place, leaving runs of two or more spaces untouched. Since the
    /// tokens are consecutive slices of that one buffer, reading from the first
    /// token to the end of the last and mapping `'\0'` back to `' '` reproduces
    /// the original text. JSON cannot contain a bare NUL, so the mapping is
    /// unambiguous. memcached itself reassembles command lines the same way.
    ///
    /// Errors if the tail is empty or longer than `limit`.
    ///
    /// [`tokenize_command`]: https://github.com/naver/arcus-memcached/blob/master/mc_util.c
    pub fn tail(&self, from: usize, limit: usize) -> Result<Vec<u8>> {
        // Past the token limit memcached stops splitting and leaves the rest of
        // the line in one piece; we cannot tell how long that piece is from the
        // slice we were given, so refuse instead of guessing.
        if self.len() >= MAX_TOKENS {
            return Err(Error::bad_request(
                "too many arguments; send ATTR JSON without spaces",
            ));
        }

        let tail = self
            .tokens
            .get(from..)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Error::bad_request("bad command line format"))?;
        let first = tail
            .iter()
            .find(|t| !t.value.is_null() && t.length > 0)
            .ok_or_else(|| Error::bad_request("bad command line format"))?;
        let last = tail
            .iter()
            .rev()
            .find(|t| !t.value.is_null() && t.length > 0)
            .ok_or_else(|| Error::bad_request("bad command line format"))?;

        // SAFETY: both pointers address the same command-line buffer, and `last`
        // never precedes `first`, so the difference is the byte span between them.
        let span = unsafe { last.value.offset_from(first.value) };
        let len = span.unsigned_abs() + last.length;
        if len > limit {
            return Err(Error::bad_request(format!(
                "ATTR is {len} bytes, over the {limit}-byte limit"
            )));
        }

        // SAFETY: `len` spans from the first token to the end of the last, all
        // within the single buffer the tokens point into.
        let raw = unsafe { std::slice::from_raw_parts(first.value.cast::<u8>(), len) };
        Ok(raw
            .iter()
            .map(|b| if *b == 0 { b' ' } else { *b })
            .collect())
    }

    /// Read the trailing `KEY value` options, upper-casing each key.
    pub fn options(&self, from: usize) -> Result<Vec<(String, &'a str)>> {
        let rest = self.len().saturating_sub(from);
        if !rest.is_multiple_of(2) {
            let key = self.text(self.len() - 1)?;
            return Err(Error::bad_request(format!("option {key} needs a value")));
        }
        (from..self.len())
            .step_by(2)
            .map(|i| Ok((self.text(i)?.to_ascii_uppercase(), self.text(i + 1)?)))
            .collect()
    }
}

/// Writes one response back to the connection.
#[derive(Clone, Copy)]
pub struct Responder {
    handler: ResponseHandler,
    cookie: *const c_void,
}

impl Responder {
    pub fn new(handler: ResponseHandler, cookie: *const c_void) -> Responder {
        Responder { handler, cookie }
    }

    pub fn send(&self, msg: &str) {
        let Some(handler) = self.handler else { return };
        // The handler takes a NUL-terminated string plus its length.
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(0);
        // SAFETY: `buf` stays alive for the call and is NUL-terminated.
        unsafe {
            handler(
                self.cookie,
                msg.len() as c_int,
                buf.as_ptr().cast::<c_char>(),
            );
        }
    }

    /// The single place a command outcome becomes an ASCII response.
    pub fn reply(&self, outcome: Result<Reply>) {
        match outcome {
            Ok(reply) => self.send(reply.as_str()),
            Err(e) => self.send(&format!("{} {e}\r\n", e.blame().prefix())),
        }
    }
}

/// Tokenizes a command line the way memcached's `tokenize_command` does: one
/// buffer, each token-terminating space overwritten with NUL, runs of two or more
/// spaces left intact. Returns the buffer, which must outlive the tokens.
#[cfg(test)]
pub fn tokenize_for_test(line: &str) -> (Vec<u8>, Vec<token_t>) {
    let mut buf = line.as_bytes().to_vec();
    let base = buf.as_mut_ptr();
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < buf.len() {
        if buf[i] == b' ' {
            if start != i {
                spans.push((start, i - start));
                buf[i] = 0;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start != buf.len() {
        spans.push((start, buf.len() - start));
    }
    let tokens = spans
        .into_iter()
        .map(|(off, len)| token_t {
            // SAFETY: `off` lies within `buf`, which the caller keeps alive.
            value: unsafe { base.add(off) }.cast::<c_char>(),
            length: len,
        })
        .collect();
    (buf, tokens)
}

/// A command whose body still has to arrive.
#[derive(Debug)]
pub enum PendingCmd {
    Add {
        index: String,
        id: String,
        /// Attribute JSON taken off the command line, or the reason it could not
        /// be read. `accept` has no way to answer the client, so the failure
        /// travels here and the handler reports it.
        attr: std::result::Result<Vec<u8>, Error>,
    },
    Search {
        index: String,
        k: usize,
        vec_bytes: usize,
    },
}

/// Buffer memcached fills with the command body.
///
/// Ownership stays here — memcached only writes into it — so dropping the entry
/// releases it whether the command completed or was aborted.
#[derive(Debug)]
pub struct Pending {
    pub cmd: PendingCmd,
    /// Body plus the two trailing CRLF bytes memcached appends.
    buffer: Vec<u8>,
    body_len: usize,
}

impl Pending {
    fn new(cmd: PendingCmd, body_len: usize) -> Pending {
        Pending {
            cmd,
            buffer: vec![0; body_len + 2],
            body_len,
        }
    }

    pub fn body(&self) -> &[u8] {
        &self.buffer[..self.body_len]
    }

    /// Split into the command and its body, dropping the trailing CRLF.
    ///
    /// Consuming lets the caller move values out of the command — notably the
    /// deferred `attr` result — instead of cloning them.
    pub fn into_parts(mut self) -> (PendingCmd, Vec<u8>) {
        self.buffer.truncate(self.body_len);
        (self.cmd, self.buffer)
    }
}

static PENDING: LazyLock<Mutex<HashMap<usize, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pending() -> std::sync::MutexGuard<'static, HashMap<usize, Pending>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Register `cmd` for `cookie` and expose the receive buffer to memcached.
///
/// # Safety
///
/// `ndata` and `ptr_out` must be the out-parameters of the `accept` callback,
/// and memcached must write at most `body_len + 2` bytes into the buffer.
pub unsafe fn expect_body(
    cookie: *const c_void,
    cmd: PendingCmd,
    body_len: usize,
    ndata: *mut usize,
    ptr_out: *mut *mut c_char,
) {
    let mut state = Pending::new(cmd, body_len);
    let len = state.buffer.len();
    let ptr = state.buffer.as_mut_ptr().cast::<c_char>();
    // Publish before handing the pointer over, so an immediate abort finds it.
    pending().insert(cookie as usize, state);
    // SAFETY: guaranteed by the caller.
    unsafe {
        *ndata = len;
        *ptr_out = ptr;
    }
}

/// Reclaim the body registered for `cookie`.
pub fn take_body(cookie: *const c_void) -> Option<Pending> {
    pending().remove(&(cookie as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn tokens_from(strs: &[&str]) -> Vec<token_t> {
        strs.iter()
            .map(|s| token_t {
                value: s.as_ptr().cast::<c_char>().cast_mut(),
                length: s.len(),
            })
            .collect()
    }

    fn view<'a>(raw: &'a [token_t]) -> Tokens<'a> {
        // SAFETY: `raw` outlives the returned view.
        unsafe { Tokens::new(raw.as_ptr(), raw.len() as c_int) }
    }

    #[test]
    fn empty_token_lists_are_safe_to_view() {
        // SAFETY: a zero count never dereferences the pointer.
        let t = unsafe { Tokens::new(ptr::null(), 0) };
        assert!(t.is_empty());
        assert_eq!(t.command(), "");
        // A negative argc must be treated as empty rather than wrapping.
        let t = unsafe { Tokens::new(ptr::null(), -1) };
        assert!(t.is_empty());
    }

    #[test]
    fn tokens_are_borrowed_not_copied() {
        let raw = tokens_from(&["vcreate", "docs", "1024"]);
        let t = view(&raw);
        assert_eq!(t.len(), 3);
        assert_eq!(t.command(), "vcreate");
        assert_eq!(t.text(1).unwrap(), "docs");
        assert_eq!(t.parse::<usize>(2, "dimension").unwrap(), 1024);
    }

    #[test]
    fn reading_past_the_end_is_a_client_error() {
        let raw = tokens_from(&["vlist"]);
        let t = view(&raw);
        assert!(t.text(1).is_err());
        assert!(t.parse::<usize>(1, "k").is_err());
    }

    #[test]
    fn parse_errors_name_the_argument() {
        let raw = tokens_from(&["vcreate", "docs", "abc"]);
        let msg = view(&raw)
            .parse::<usize>(2, "dimension")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("dimension"), "{msg}");
        assert!(msg.contains("abc"), "{msg}");
    }

    use super::tokenize_for_test as tokenize;

    #[test]
    fn tail_recovers_json_containing_single_spaces() {
        // The tokenizer split this into five pieces and put NULs where the
        // spaces were; tail must hand back the original bytes.
        let json = r#"{"cat": "tech", "ts": 1}"#;
        let (buf, raw) = tokenize(&format!("vadd docs v1 16 ATTR 24 {json}"));
        let t = view(&raw);
        assert!(t.len() > 7, "expected the JSON to be split up");
        assert_eq!(t.tail(6, 128).unwrap(), json.as_bytes());
        drop(buf);
    }

    #[test]
    fn tail_recovers_json_with_no_spaces() {
        let json = r#"{"cat":"tech"}"#;
        let (_buf, raw) = tokenize(&format!("vadd docs v1 16 ATTR 14 {json}"));
        assert_eq!(view(&raw).tail(6, 128).unwrap(), json.as_bytes());
    }

    #[test]
    fn tail_preserves_runs_of_consecutive_spaces() {
        // tokenize_command only NULs a space that terminates a token, so double
        // spaces survive in the buffer and must survive the round trip too.
        let json = r#"{"a":  1}"#;
        let (_buf, raw) = tokenize(&format!("vadd docs v1 16 ATTR 9 {json}"));
        assert_eq!(view(&raw).tail(6, 128).unwrap(), json.as_bytes());
    }

    #[test]
    fn tail_rejects_a_span_over_the_limit() {
        let json = "x".repeat(200);
        let (_buf, raw) = tokenize(&format!("vadd docs v1 16 ATTR 200 {json}"));
        let msg = view(&raw).tail(6, 128).unwrap_err().to_string();
        assert!(msg.contains("over the 128-byte limit"), "{msg}");
    }

    #[test]
    fn tail_rejects_an_absent_tail() {
        let (_buf, raw) = tokenize("vadd docs v1 16 ATTR 5");
        assert!(view(&raw).tail(6, 128).is_err());
    }

    #[test]
    fn tail_refuses_when_the_tokenizer_ran_out_of_slots() {
        // At MAX_TOKENS the remainder of the line is left undelimited and its
        // length is not recoverable from the slice we get, so tail must refuse
        // rather than read past the end.
        let line = (0..MAX_TOKENS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let (_buf, raw) = tokenize(&line);
        let msg = view(&raw).tail(6, 128).unwrap_err().to_string();
        assert!(msg.contains("too many arguments"), "{msg}");
    }

    #[test]
    fn options_are_upper_cased_and_paired() {
        let raw = tokens_from(&["vcreate", "docs", "8", "quant", "i8", "METRIC", "cos"]);
        let opts = view(&raw).options(3).unwrap();
        assert_eq!(opts, vec![("QUANT".into(), "i8"), ("METRIC".into(), "cos")]);
    }

    #[test]
    fn a_dangling_option_key_is_rejected() {
        let raw = tokens_from(&["vcreate", "docs", "8", "QUANT"]);
        let msg = view(&raw).options(3).unwrap_err().to_string();
        assert!(msg.contains("QUANT"), "{msg}");
    }

    #[test]
    fn no_options_is_not_an_error() {
        let raw = tokens_from(&["vcreate", "docs", "8"]);
        assert!(view(&raw).options(3).unwrap().is_empty());
    }

    #[test]
    fn body_buffer_reserves_room_for_the_trailing_crlf() {
        let p = Pending::new(
            PendingCmd::Search {
                index: "docs".into(),
                k: 5,
                vec_bytes: 16,
            },
            16,
        );
        assert_eq!(p.buffer.len(), 18);
        assert_eq!(p.body().len(), 16);
    }

    #[test]
    fn bodies_are_keyed_by_cookie_and_taken_once() {
        let cookie = 0xF00D as *const c_void;
        let mut ndata = 0usize;
        let mut ptr: *mut c_char = ptr::null_mut();
        // SAFETY: both out-parameters are live locals.
        unsafe {
            expect_body(
                cookie,
                PendingCmd::Add {
                    index: "docs".into(),
                    id: "v1".into(),
                    attr: Ok(br#"{"a":1}"#.to_vec()),
                },
                8,
                &mut ndata,
                &mut ptr,
            );
        }
        assert_eq!(ndata, 10);
        assert!(!ptr.is_null());

        assert!(take_body(cookie).is_some());
        // A second take must not resurrect it — that would double-free.
        assert!(take_body(cookie).is_none());
    }
}
