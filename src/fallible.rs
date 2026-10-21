//! Allocations that grow with the result count, made refusable instead of fatal.
//!
//! `Vec::reserve` and `collect` go to `handle_alloc_error` on failure, which ends in `abort` --
//! it never comes back as an error, so the caller cannot stop it. The memory one search holds is
//! proportional to its hit count, and that count is ultimately chosen by the client via `<num>`,
//! so a single request could take the daemon down.
//!
//! `try_reserve` returns the same failure as a value. This module just wraps it so a shortfall
//! becomes a `SERVER_ERROR` ([`crate::error::Error::Index`]).
//!
//! **This does not prevent every OOM.** Under Linux's default overcommit, the allocator hands out
//! memory it cannot back yet, so this check passes and the kernel may kill the process when the
//! pages are touched. The default heuristic does still refuse a single request larger than total
//! memory, so it is effective for large one-shot reservations like these. Memory consumed by many
//! small allocations is not caught by any API, since each allocation individually succeeds.

use crate::error::{Error, Result};

/// Reserves room for `extra` more elements, or `SERVER_ERROR`.
pub fn reserve<T>(v: &mut Vec<T>, extra: usize) -> Result<()> {
    v.try_reserve(extra).map_err(|_| too_big::<T>(extra))
}

/// An empty `Vec` with capacity for `n` elements, or `SERVER_ERROR`.
pub fn vec<T>(n: usize) -> Result<Vec<T>> {
    let mut v = Vec::new();
    reserve(&mut v, n)?;
    Ok(v)
}

/// A zero-filled buffer of `n` bytes, or `SERVER_ERROR`.
///
/// Like `vec![0u8; n]`, but returns failure as a value. The capacity is reserved exactly up
/// front, so the subsequent `resize` does not reallocate.
pub fn zeroed(n: usize) -> Result<Vec<u8>> {
    let mut v = vec(n)?;
    v.resize(n, 0);
    Ok(v)
}

/// Reserves room for `extra` more bytes in a string.
pub fn reserve_str(s: &mut String, extra: usize) -> Result<()> {
    s.try_reserve(extra).map_err(|_| too_big::<u8>(extra))
}

/// A copy of `s`, or `SERVER_ERROR`.
pub fn string(s: &str) -> Result<String> {
    let mut out = String::new();
    reserve_str(&mut out, s.len())?;
    out.push_str(s);
    Ok(out)
}

/// Moves `value` to the heap, or `SERVER_ERROR`.
///
/// Like `Box::new`, but returns failure as a value (`Box::try_new` is not stable yet).
pub fn boxed<T>(value: T) -> Result<Box<T>> {
    let layout = std::alloc::Layout::new::<T>();
    if layout.size() == 0 {
        return Ok(Box::new(value));
    }
    let raw = unsafe { std::alloc::alloc(layout) }.cast::<T>();
    if raw.is_null() {
        return Err(too_big::<T>(1));
    }
    unsafe {
        raw.write(value);
        Ok(Box::from_raw(raw))
    }
}

fn too_big<T>(n: usize) -> Error {
    Error::Index(format!(
        "out of memory for {n} results ({} bytes)",
        n.saturating_mul(size_of::<T>())
    ))
}

/// Trial-reserves, **with the same allocator and size**, the two arrays the binding allocates on
/// entry, then releases them immediately. Fails with `SERVER_ERROR` if they cannot be reserved.
///
/// usearch's `Matches` is a cxx `rust::Vec`, whose `reserve` is not implemented in C++ but calls
/// back into Rust's `Vec::reserve` (cxx's `rust_vec.rs`). A failure inside the binding aborts and
/// cannot be recovered there, so the only place to back out is before entering it.
///
/// # What this does and does not prevent
///
/// **It only catches failures that surface at allocation time:** absurd sizes whose capacity
/// computation overflows, and environments where the allocator refuses on the spot, such as
/// `vm.overcommit_memory=2` or `RLIMIT_AS` (`ulimit -v`). There the abort becomes `SERVER_ERROR`.
///
/// **It does not prevent being OOM-killed.** A successful `try_reserve_exact` means address space
/// was granted, not physical memory. The binding then **fills** that region with zeros, and
/// physical pages are attached at that point; if they run out, the process is SIGKILLed after this
/// check has already passed. cgroup limits are charged when pages are written, not allocated, so
/// they are not caught either -- in containers that is the most common way to die. Under default
/// overcommit even a 24 GB probe once passed, so sizes reaching here should be assumed to almost
/// always pass.
///
/// Touching the probe buffer would not help: without physical memory the process just dies while
/// filling the probe instead. It only moves the same death earlier.
///
/// So **do not rely on this function as a defense against memory exhaustion.** That defense lives
/// outside the process: size `-m` to the machine's memory (the graph is outside the accounting, so
/// RSS is several times that), and set cgroup limits and a restart policy. For deterministic
/// behavior use strict overcommit or `RLIMIT_AS`; only in those environments does this check
/// pull its weight.
pub fn probe_pair<A, B>(n: usize) -> Result<()> {
    let mut a: Vec<A> = Vec::new();
    let mut b: Vec<B> = Vec::new();
    a.try_reserve_exact(n).map_err(|_| too_big::<A>(n))?;
    b.try_reserve_exact(n).map_err(|_| too_big::<B>(n))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unaffordable size comes back as `SERVER_ERROR` rather than aborting.
    ///
    /// This is the module's whole reason to exist -- with `Vec::reserve` this call would have
    /// ended the process.
    #[test]
    fn an_impossible_size_comes_back_as_an_error() {
        let huge = usize::MAX / 16;
        let err = vec::<u64>(huge).expect_err("this much memory cannot be available");
        assert_eq!(err.blame(), crate::error::Blame::Server);

        let mut v: Vec<(u64, f32)> = Vec::new();
        assert!(reserve(&mut v, huge).is_err());

        let mut s = String::new();
        assert!(reserve_str(&mut s, huge).is_err());
    }

    /// The pre-binding probe likewise comes back as an error.
    ///
    /// This checks the **wiring** -- that an allocator refusal yields an error instead of an
    /// abort. Whether it is safe under memory pressure cannot be tested here, or from inside the
    /// process at all; see `probe_pair`.
    #[test]
    fn the_binding_probe_turns_a_refusal_into_an_error() {
        assert!(probe_pair::<u64, f32>(usize::MAX / 16).is_err());
    }

    /// An ordinary size passes, and the reservation is released immediately.
    #[test]
    fn an_ordinary_size_passes() {
        assert!(probe_pair::<u64, f32>(50_000).is_ok());
        assert_eq!(vec::<u64>(50_000).unwrap().capacity().min(50_000), 50_000);
    }
}
