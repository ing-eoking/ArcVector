//! Wire mechanics: reading a tokenized command line, and writing one response.
//!
//! This is the only module that touches memcached's token array. What the tokens
//! *mean* is [`crate::request`]'s business.

use std::os::raw::{c_char, c_int, c_void};
use std::str::FromStr;

use crate::engine_api::token_t;
use crate::error::{Error, Reply, Result};
use crate::request::Cmd;

/// Mirrors memcached's own `MAX_TOKENS` (memcached.c:8066). At this many tokens
/// the tokenizer stops splitting and leaves the rest of the line in one
/// undelimited piece.
const MAX_TOKENS: usize = 30;

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
        let tokens = if len == 0 {
            // from_raw_parts must not be called with a dangling pointer, and a
            // zero-length line is exactly when argv may be null.
            &[][..]
        } else {
            // SAFETY: guaranteed by the caller.
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

    /// The command, or `None` for a body-only invocation or an unknown name.
    pub fn command(&self) -> Option<Cmd> {
        Cmd::parse(self.text(0).unwrap_or(""))
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

    /// Read the trailing `KEY value` options from token `from`, upper-casing each
    /// key so callers can match on it.
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

    /// Byte extent of the command line from token `from` to the end of the line.
    ///
    /// The tokens are consecutive slices of one buffer, so the span from the first
    /// of them to the end of the last is exactly the remaining line.
    fn tail_span(&self, from: usize) -> Result<(*const u8, usize)> {
        let malformed = || Error::bad_request("bad command line format");
        let tail = self.tokens.get(from..).ok_or_else(malformed)?;
        let mut real = tail.iter().filter(|t| !t.value.is_null() && t.length > 0);
        let first = real.next().ok_or_else(malformed)?;
        let last = real.next_back().unwrap_or(first);

        // SAFETY: both pointers address the same command-line buffer and `last`
        // never precedes `first`, so the difference is the span between them.
        let offset = unsafe { last.value.offset_from(first.value) };
        Ok((
            first.value.cast::<u8>(),
            offset.unsigned_abs() + last.length,
        ))
    }

    /// Read exactly `len` bytes of command line from token `from`, recovering the
    /// text as the client sent it.
    ///
    /// memcached's tokenizer overwrites each token-terminating space with `'\0'`
    /// in place and leaves runs of two or more spaces alone, so mapping `'\0'`
    /// back to `' '` reproduces the original bytes. JSON cannot contain a raw NUL,
    /// which is what makes the mapping unambiguous. memcached reassembles command
    /// lines the same way when it logs them (memcached.c:8078).
    ///
    /// `len` comes from the client, so it is checked against the span the tokens
    /// actually cover — that is what keeps the read in bounds.
    pub fn tail(&self, from: usize, len: usize) -> Result<Vec<u8>> {
        // Past the token limit the rest of the line is left undelimited, and its
        // length lives in a slot beyond the array we were handed. The span cannot
        // be verified, so refuse rather than read unbounded.
        if self.len() >= MAX_TOKENS {
            return Err(Error::bad_request(
                "too many arguments; send ATTR JSON with less whitespace",
            ));
        }

        let (start, available) = self.tail_span(from)?;
        if len != available {
            return Err(Error::bad_request(format!(
                "declared length {len} does not match the {available} bytes supplied"
            )));
        }

        // SAFETY: `len` equals a span the tokens proved to be within the single
        // command-line buffer they point into.
        let raw = unsafe { std::slice::from_raw_parts(start, len) };
        Ok(raw
            .iter()
            .map(|b| if *b == 0 { b' ' } else { *b })
            .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn view<'a>(raw: &'a [token_t]) -> Tokens<'a> {
        // SAFETY: `raw` outlives the returned view.
        unsafe { Tokens::new(raw.as_ptr(), raw.len() as c_int) }
    }

    #[test]
    fn empty_token_lists_are_safe_to_view() {
        // SAFETY: a zero count never dereferences the pointer.
        let t = unsafe { Tokens::new(ptr::null(), 0) };
        assert!(t.is_empty());
        assert_eq!(t.command(), None);
        // A negative argc must read as empty rather than wrapping.
        let t = unsafe { Tokens::new(ptr::null(), -1) };
        assert!(t.is_empty());
    }

    #[test]
    fn tokens_are_borrowed_not_copied() {
        let (_buf, raw) = tokenize_for_test("vcreate docs 1024");
        let t = view(&raw);
        assert_eq!(t.len(), 3);
        assert_eq!(t.command(), Some(Cmd::VCreate));
        assert_eq!(t.text(1).unwrap(), "docs");
        assert_eq!(t.parse::<usize>(2, "dimension").unwrap(), 1024);
    }

    #[test]
    fn reading_past_the_end_is_a_client_error() {
        let (_buf, raw) = tokenize_for_test("vlist");
        let t = view(&raw);
        assert!(t.text(1).is_err());
        assert!(t.parse::<usize>(1, "k").is_err());
    }

    #[test]
    fn parse_errors_name_the_argument() {
        let (_buf, raw) = tokenize_for_test("vcreate docs abc");
        let msg = view(&raw)
            .parse::<usize>(2, "dimension")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("dimension"), "{msg}");
        assert!(msg.contains("abc"), "{msg}");
    }

    #[test]
    fn options_are_upper_cased_and_paired() {
        let (_buf, raw) = tokenize_for_test("vcreate docs 8 quant i8 METRIC cos");
        let opts = view(&raw).options(3).unwrap();
        assert_eq!(opts, vec![("QUANT".into(), "i8"), ("METRIC".into(), "cos")]);
    }

    #[test]
    fn a_dangling_option_key_is_rejected() {
        let (_buf, raw) = tokenize_for_test("vcreate docs 8 QUANT");
        let msg = view(&raw).options(3).unwrap_err().to_string();
        assert!(msg.contains("QUANT"), "{msg}");
    }

    #[test]
    fn no_options_is_not_an_error() {
        let (_buf, raw) = tokenize_for_test("vcreate docs 8");
        assert!(view(&raw).options(3).unwrap().is_empty());
    }

    #[test]
    fn tail_recovers_json_containing_single_spaces() {
        // The tokenizer split this into five pieces and put NULs where the
        // spaces were; tail must hand back the original bytes.
        let json = r#"{"cat": "tech", "ts": 1}"#;
        let (_buf, raw) = tokenize_for_test(&format!("vadd docs v1 16 4 ATTR 24 {json}"));
        let t = view(&raw);
        assert!(t.len() > 8, "expected the JSON to be split up");
        assert_eq!(t.tail(7, json.len()).unwrap(), json.as_bytes());
    }

    #[test]
    fn tail_recovers_json_with_no_spaces() {
        let json = r#"{"cat":"tech"}"#;
        let (_buf, raw) = tokenize_for_test(&format!("vadd docs v1 16 4 ATTR 14 {json}"));
        assert_eq!(view(&raw).tail(7, json.len()).unwrap(), json.as_bytes());
    }

    #[test]
    fn tail_preserves_runs_of_consecutive_spaces() {
        // Only a space that terminates a token is overwritten, so double spaces
        // survive in the buffer and must survive the round trip too.
        let json = r#"{"a":  1}"#;
        let (_buf, raw) = tokenize_for_test(&format!("vadd docs v1 16 4 ATTR 9 {json}"));
        assert_eq!(view(&raw).tail(7, json.len()).unwrap(), json.as_bytes());
    }

    #[test]
    fn tail_rejects_a_length_that_disagrees_with_the_line() {
        let json = r#"{"a":1}"#;
        let (_buf, raw) = tokenize_for_test(&format!("vadd docs v1 16 4 ATTR 99 {json}"));
        let msg = view(&raw).tail(7, 99).unwrap_err().to_string();
        assert!(msg.contains("does not match the 7 bytes"), "{msg}");
    }

    #[test]
    fn tail_rejects_an_absent_tail() {
        let (_buf, raw) = tokenize_for_test("vadd docs v1 16 4 ATTR 5");
        assert!(view(&raw).tail(7, 5).is_err());
    }

    #[test]
    fn tail_refuses_when_the_tokenizer_ran_out_of_slots() {
        // At MAX_TOKENS the remainder of the line is undelimited and its length
        // is not recoverable from the array we get, so tail must refuse rather
        // than read past a bound it cannot verify.
        let line = (0..MAX_TOKENS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let (_buf, raw) = tokenize_for_test(&line);
        let msg = view(&raw).tail(6, 8).unwrap_err().to_string();
        assert!(msg.contains("too many arguments"), "{msg}");
    }
}
