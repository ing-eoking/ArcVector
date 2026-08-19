//! Reading a tokenized command line.
//!
//! The only module that touches memcached's token array. Writing the response
//! back is [`crate::server::Responder`]'s job.

use std::os::raw::c_int;
use std::str::FromStr;

use super::request::Cmd;
use crate::engine_api::token_t;
use crate::error::{Error, Result};

#[derive(Clone, Copy)]
pub struct Tokens<'a> {
    tokens: &'a [token_t],
}

impl<'a> Tokens<'a> {
    /// # Safety
    ///
    /// `argv` must point to `argc` tokens that stay valid for `'a`.
    pub unsafe fn new(argv: *const token_t, argc: c_int) -> Self {
        let len = argc.max(0) as usize;
        let tokens = if len == 0 {
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

    /// Trailing `KEY value` options from token `from`, keys upper-cased.
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

/// Tokenize like memcached's `tokenize_command`. Returns the backing buffer.
#[cfg(test)]
pub fn tokenize_for_test(line: &str) -> (Vec<u8>, Vec<token_t>) {
    use std::os::raw::c_char;

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
}
