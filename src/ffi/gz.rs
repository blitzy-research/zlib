//! `extern "C"` shims for zlib's **`gz*`** gzip file-I/O API.
//!
//! This module reproduces, with byte-for-byte C ABI fidelity, the family of
//! `gz*` functions declared in `zlib.h` (`gzopen`, `gzread`, `gzwrite`,
//! `gzgets`, `gzclose`, …). Each shim validates raw C inputs, converts C
//! strings / file descriptors into safe Rust types, bridges to the idiomatic
//! `crate::gz` implementation, and translates the result back into the exact
//! integer / pointer sentinel a C caller expects.
//!
//! # Opaque handle model
//!
//! The public `gzFile` handle (`*mut gzFile_s`) is an *opaque* pointer to a
//! `#[repr(C)]` `GzHandle` whose FIRST field is a live C-layout `gzFile_s`
//! `{ have, next, pos }` prefix; the idiomatic `GzState` lives in its own
//! allocation behind the prefix. The handle is:
//!
//! * **open** — `Box::into_raw(Box::new(GzHandle { prefix, state })) as gzFile`
//!   (the prefix starts cleared).
//! * **operations** — the handle is *borrowed*, never owned, through a
//!   `GzBorrow` guard that reconciles the prefix on entry and re-syncs it on
//!   exit (see below). The box is not reconstructed.
//! * **close** — the requested direction is checked first, through a *borrow*;
//!   only if it matches is the `Box<GzHandle>` reconstructed, *exactly once*
//!   (`take_for_close`). Its inner `Box<GzState>` then goes to the idiomatic
//!   `gz::gzclose*_release` finalizer, which flushes/finishes writers, emits the
//!   gzip trailer, frees buffers, and hands back the still-open
//!   [`std::fs::File`]; the shim closes that descriptor itself so a failing
//!   `close(2)` can be reported as `Z_ERRNO`, exactly as C does. The prefix drops
//!   with the handle.
//!
//! This guarantees a sound lifecycle: one allocation on open, guarded borrows on
//! every operation, one deallocation on close — no double-free, no leak. The
//! direction-before-ownership ordering is essential rather than stylistic: C
//! tests `state->mode` *before* any `free`/`close` (`gzread.c` L650-L651,
//! `gzwrite.c` L677-L678), so `gzclose_w` on a reader is a pure no-op and the
//! caller may retry with `gzclose_r`. Reclaiming the box before that test would
//! drop the allocation while the caller still held the pointer — a
//! use-after-free on the next operation and a double-free on the retry.
//!
//! # Descriptor ownership across `gzdopen`
//!
//! `gzdopen` adopts a raw descriptor, so *when* it adopts is part of the ABI. C
//! performs every mode-grammar rejection and every `malloc` before storing the
//! descriptor in `state->fd` (`gzlib.c` L150-L197 and L206-L210 precede L263), so
//! **no** `gzdopen` failure closes the caller's descriptor. This shim matches
//! that: the mode is pre-validated before `adopt_descriptor`, and every
//! allocation failure after adoption releases the descriptor rather than closing
//! it. There are two such points and each has its own release mechanism:
//!
//! * *Inside* the gz layer — the `<fd:N>` name, the state box, and the retained
//!   path name are all allocated before the file is opened, and a failure of any
//!   of them dissolves the adopted `File` with `core::mem::forget`, which needs no
//!   `unsafe` and keeps that layer's zero-`unsafe` guarantee.
//! * *Here*, if the opaque handle allocation fails after the state was built
//!   successfully — the descriptor is lifted out with `release_descriptor`,
//!   which likewise ends Rust ownership without closing.
//!
//! # `gzgetc` / `gzgetc_` — live `gzFile_s` prefix
//!
//! In C, `gzgetc` is a *macro* that peeks the `{have, next, pos}` triple
//! directly through the handle for a branch-free fast path, falling back to the
//! real `gzgetc_` function only when the buffer is empty. To make this
//! macro-compatible, the opaque handle *begins* with a live `gzFile_s` prefix.
//! Each shim borrows the handle through a `GzBorrow` guard that, on entry,
//! **reconciles** any bytes the macro consumed straight from the prefix
//! (advancing the idiomatic cursor to match) and, on exit, **re-syncs** the
//! prefix to re-expose the current read buffer (`have`, a raw `next` pointer
//! into `out_buf`, and `pos`). The real `gzgetc`/`gzgetc_` functions are still
//! exported for callers that take the function pointer, and they observe
//! identical return values; the macro fast path is supported too.
//!
//! # `gzprintf` / `gzvprintf` — supported zlib ABI variant (no secure `*printf`)
//!
//! For its two variadic entry points this crate **formally implements the
//! `NO_vsnprintf && !ZLIB_INSECURE` zlib build variant** — a first-class,
//! documented zlib configuration, not a partial port. Reference zlib compiled
//! without a secure `vsnprintf`/`snprintf` still *exports* both `gzprintf` and
//! `gzvprintf`, has them return `Z_STREAM_ERROR`, and advertises that fact by
//! setting `zlibCompileFlags` bit 27. This crate reproduces that contract
//! byte-for-byte (see [`crate::util::version::zlib_compile_flags`]): the symbols
//! exist with the correct C signatures, return `Z_STREAM_ERROR`, and bit 27 is
//! set. A C caller that links or `LD_PRELOAD`-injects this object resolves both
//! symbols and observes exactly the documented variant's behavior.
//!
//! This variant is *mandated* by two AAP constraints, not chosen for
//! convenience. Rendering a C `va_list` requires the `c_variadic` language
//! feature (`core::ffi::VaList`), which is nightly-only, so a true variadic
//! definition is incompatible with the stable **MSRV 1.85** contract
//! (AAP §0.7.2 standard S7); and delegating to a C `vsnprintf` would reintroduce a C
//! dependency, violating the **zero-C-dependency** rule (AAP §0.5.2). Emulating
//! the C ABI's argument promotion by hand cannot recover the caller's original
//! types, so no safe stable-Rust rendering exists. The documented
//! error-returning variant is therefore the faithful, in-contract choice.
//!
//! Rust consumers have **no** functional gap: the idiomatic
//! `crate::gz::gzprintf` / `crate::gz::gzvprintf` accept
//! [`core::fmt::Arguments`] (via [`format_args!`]) and perform full formatted
//! output. **Every other** `gz*` symbol is fully functional through the C ABI.
//!
//! # `gzerror` message pointer
//!
//! `gzerror` must return a `*const c_char` that stays valid until the next `gz*`
//! call on the handle. The idiomatic error accessor yields a borrowed,
//! non-NUL-terminated `&str`; to hand C a stable, NUL-terminated pointer the
//! handle owns a `CString` mirror of its message (`GzState::msg_c`, kept in
//! lockstep with `GzState::msg` by `GzState::error`). `gzerror` returns a
//! pointer into that mirror — the specific `"{path}: {detail}"` text, matching
//! reference zlib byte-for-byte — falling back to the literal `"out of memory"`
//! for `Z_MEM_ERROR` (which stores no heap message) and to the empty string
//! when there is no detail. The `errnum` out-parameter carries the
//! authoritative, exact code.
//!
//! # Feature gating & safety
//!
//! **This module is compiled unconditionally, and every one of its 34
//! `#[unsafe(no_mangle)]` entry points is emitted in every Cargo feature
//! configuration** — the emitted `cdylib`/`staticlib` must present the complete
//! zlib C symbol table regardless of how the crate was configured (AAP §0.3.1,
//! §0.8.1 D-4). A C consumer links against one ABI; a build that dropped
//! `gzbuffer` or `gzclose_r` would be *unlinkable*, not merely reduced.
//!
//! The `gz-io` feature (which implies `std`, required for `std::fs::File` I/O,
//! `CStr`, and `from_raw_fd`) therefore gates each function **body**, never the
//! `mod` declaration and never an exported item:
//!
//! * **With `gz-io`** — every shim bridges to `crate::gz` and reproduces the C
//!   behaviour exactly.
//! * **Without `gz-io`** — every shim still exists with its exact C signature and
//!   returns that entry point's documented failure sentinel: `NULL` for the
//!   pointer-returning `gzopen`/`gzopen64`/`gzdopen`/`gzgets`/`gzerror` family,
//!   `-1` for `gzbuffer`/`gzread`/`gzgetc`/`gzgetc_`/`gzungetc`/`gzputc`/
//!   `gzputs`/`gzrewind`/`gzseek`/`gzseek64`/`gztell`/`gztell64`/`gzoffset`/
//!   `gzoffset64`, `0` for `gzwrite`/`gzeof`/`gzdirect` and the
//!   `z_size_t`-returning `gzfread`/`gzfwrite`, `Z_STREAM_ERROR` for
//!   `gzsetparams`/`gzflush`/`gzclose`/`gzclose_r`/`gzclose_w`, and a no-op for
//!   the `void`-returning `gzclearerr`. This is the same discipline as the
//!   `gzprintf`/`gzvprintf` concession above: the symbol resolves and the failure
//!   is observable through the return value instead of at link time.
//!
//! Only the `std`-dependent *internals* — the `GzHandle`/`GzBorrow` ownership
//! types, the boxing and close plumbing, the platform `close(2)` binding, and the
//! path conversion — carry `#[cfg(feature = "gz-io")]`, so a
//! `--no-default-features` build compiles no unreachable machinery while still
//! emitting every symbol. `src/ffi/mod.rs` pins both halves of this contract with
//! `no_exported_gz_symbol_is_feature_gated` (no `cfg` may reach an exported item)
//! and `every_exported_c_symbol_resolves_to_a_live_address` (each of the 95
//! exports is a live, distinct address in the row under test).
//!
//! This module is part of `src/ffi/**`, the crate's designated `unsafe`
//! boundary: every `unsafe` block carries a `// SAFETY:` justification, and no
//! shim may unwind across the C boundary — fallible bodies run inside the
//! `guard_*` helpers so a panic is caught and converted to the function's C
//! error sentinel.

// Every exported function here shares one uniform safety contract, stated under
// "Feature gating & safety" above: the caller passes a `gzFile` this module
// itself produced (or null, which each shim rejects), NUL-terminated C strings
// for paths and modes, and buffers valid for the lengths given. Documenting that
// once at the module level is clearer than repeating an identical `# Safety`
// section on each of the 34 `extern "C"` shims, so the per-item lint is allowed
// here.
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_int, c_uint};
use core::ptr;

use crate::error::ReturnCode;
use crate::ffi::types::*;

// Everything below is needed only by the gzip file-I/O *implementation*, which is
// compiled when `gz-io` is enabled. The `extern "C"` entry points themselves are
// always compiled (see the module docs), so their signatures depend on nothing
// gated here.
#[cfg(feature = "gz-io")]
use core::ffi::CStr;
#[cfg(feature = "gz-io")]
use core::slice;

#[cfg(feature = "gz-io")]
use alloc::boxed::Box;

#[cfg(all(unix, feature = "gz-io"))]
use std::os::fd::FromRawFd;

#[cfg(all(windows, feature = "gz-io"))]
use std::os::windows::io::FromRawHandle;

#[cfg(all(any(unix, windows), feature = "gz-io"))]
use crate::gz::GzFile;
#[cfg(feature = "gz-io")]
use crate::gz::{self, GzMode, GzState};

// ===========================================================================
// Return-code constants
// ===========================================================================
//
// `constants.rs` intentionally does not expose bare `Z_*` return-code integer
// aliases (idiomatic code uses `ReturnCode`), so the two codes the `gz*` C
// contract branches on are materialized locally from the authoritative enum.

/// `Z_OK` (`0`) — success. Part of the return-code vocabulary; consumed by the
/// in-crate tests and available for readable success comparisons.
#[allow(dead_code)]
const Z_OK: c_int = ReturnCode::Ok.as_c_int();
/// `Z_STREAM_ERROR` (`-2`) — inconsistent stream / invalid handle.
const Z_STREAM_ERROR: c_int = ReturnCode::StreamError.as_c_int();
/// `Z_ERRNO` (`-1`) — an OS-level error; the code the C finalizers return when
/// `close(state->fd)` fails (`gzread.c` L665-L667, `gzwrite.c` L695-L696).
#[allow(dead_code)]
const Z_ERRNO: c_int = ReturnCode::ErrNo.as_c_int();

// ===========================================================================
// Local helpers
// ===========================================================================

/// Converts a non-null C path string into an owned [`std::path::PathBuf`].
///
/// On Unix the raw bytes are used verbatim (`OsStrExt::from_bytes`) so that
/// paths which are not valid UTF-8 are preserved exactly, matching C behavior.
/// On other targets the bytes must be valid UTF-8 (constructing a `PathBuf`
/// losslessly from arbitrary bytes is not portably available); an invalid
/// encoding yields `None`, causing the open to fail with a null handle.
///
/// # Safety
///
/// `path` must be non-null and point to a NUL-terminated C string that remains
/// valid for the duration of the call.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
unsafe fn cpath_to_pathbuf(path: *const c_char) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: the caller guarantees `path` is non-null and NUL-terminated.
    let cstr = unsafe { CStr::from_ptr(path) };
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        cstr.to_bytes(),
    )))
}

/// Portable (non-Unix) fallback: requires the path to be valid UTF-8.
///
/// # Safety
///
/// See the Unix variant: `path` must be non-null and NUL-terminated.
#[cfg(feature = "gz-io")]
#[cfg(not(unix))]
unsafe fn cpath_to_pathbuf(path: *const c_char) -> Option<std::path::PathBuf> {
    // SAFETY: the caller guarantees `path` is non-null and NUL-terminated.
    let cstr = unsafe { CStr::from_ptr(path) };
    cstr.to_str().ok().map(std::path::PathBuf::from)
}

// ---------------------------------------------------------------------------
// Live `gzFile_s` prefix for the `gzgetc(g)` macro fast-path
// ---------------------------------------------------------------------------
//
// zlib's `gzgetc(g)` is a *macro* that reads the `{ have, next, pos }` prefix
// directly through the `gzFile` pointer:
//
//   ((g)->have ? ((g)->have--, (g)->pos++, *((g)->next)++) : (gzgetc)(g))
//
// In C, `gz_statep` embeds `struct gzFile_s x` as its first member, so the macro
// and every real `gz*` function share ONE storage for `have`/`next`/`pos`. Our
// idiomatic [`GzState`] is not `#[repr(C)]` (it owns a `Vec`, `File`, `String`,
// …) and tracks `next` as an *index*, so we cannot expose it directly. Instead
// the opaque handle is a `#[repr(C)]` [`GzHandle`] whose FIRST field is a live
// [`gzFile_s`] prefix. The prefix shadows the idiomatic cursor and is bridged
// by [`GzHandle::reconcile`] (absorb macro-side consumption) on entry and
// [`GzHandle::sync`] (re-expose the read buffer) on exit of every shim.

/// The opaque `gzFile` handle: a `#[repr(C)]` wrapper whose leading
/// [`gzFile_s`] prefix (offset 0) is what the C `gzgetc(g)` macro reads and
/// mutates. The idiomatic [`GzState`] lives in its own allocation behind a
/// `Box`; only the prefix is ABI-visible.
#[cfg(feature = "gz-io")]
#[repr(C)]
struct GzHandle {
    /// C `gzFile_s` prefix consumed by the `gzgetc(g)` macro fast-path. MUST be
    /// the first field so `gzFile` (a `*mut gzFile_s`) aliases it at offset 0.
    prefix: gzFile_s,
    /// The idiomatic gz state (separate allocation; never inspected by C).
    state: Box<GzState>,
}

#[cfg(feature = "gz-io")]
impl GzHandle {
    /// Absorbs any bytes the C `gzgetc(g)` macro consumed directly from the
    /// prefix since the last [`sync`](Self::sync), advancing the idiomatic cursor
    /// to match, then clears the prefix so `sync` fully re-derives it.
    ///
    /// The macro only runs in READ mode; in WRITE/NONE mode the prefix is always
    /// cleared and this is a no-op beyond clearing.
    #[inline]
    fn reconcile(&mut self) {
        if self.state.mode == GzMode::Read {
            // The macro decrements `prefix.have` (and advances `next`/`pos`) once
            // per byte it delivered; the delta is the count it consumed. Saturate
            // defensively — `prefix.have <= state.have` always holds because
            // `sync` sets them equal and only the macro (a decrement) runs in
            // between.
            let consumed = self.state.have.saturating_sub(self.prefix.have as usize);
            self.state.next += consumed;
            self.state.have -= consumed;
            self.state.pos += consumed as i64;
        }
        self.prefix.have = 0;
        self.prefix.next = ptr::null_mut();
    }

    /// Re-exposes the idiomatic read buffer through the `gzFile_s` prefix so the
    /// C `gzgetc(g)` macro fast-path can consume it directly. In WRITE/NONE mode
    /// (or when nothing is buffered) the prefix is cleared; `pos` always mirrors
    /// the idiomatic position.
    #[inline]
    fn sync(&mut self) {
        self.prefix.pos = self.state.pos;
        if self.state.mode == GzMode::Read && self.state.have > 0 {
            self.prefix.have = self.state.have as c_uint;
            // SAFETY: the read driver maintains `next + have <= out_buf.len()`, so
            // `&out_buf[next]` is in-bounds and `have` bytes are readable from it.
            // The macro only ever READS through this pointer (`*next++`); no Rust
            // code aliases the region while the macro owns it (the shim has
            // returned and the `GzBorrow` has been dropped), so exposing it as a
            // raw `*mut` is sound. It is recomputed on every `sync`, so a buffer
            // reallocation between calls can never leave it dangling.
            let next = unsafe { self.state.out_buf.as_ptr().add(self.state.next) };
            self.prefix.next = next as *mut u8;
        } else {
            self.prefix.have = 0;
            self.prefix.next = ptr::null_mut();
        }
    }
}

/// Reads the direction of the handle behind `file` **without taking ownership**
/// of it.
///
/// This exists so the close shims can reproduce C's *validate-then-act* order
/// exactly. C inspects `state->mode` while the allocation is still owned by the
/// caller and returns `Z_STREAM_ERROR` for a wrong-direction request **without
/// touching any allocation** (`gzread.c` L650-L651 `if (state->mode != GZ_READ)
/// return Z_STREAM_ERROR;` and the matching `gzwrite.c` guard in `gzclose_w`),
/// leaving the handle fully usable. Reconstructing the owning [`Box`] first —
/// and therefore dropping it on the rejection path — would free a handle the
/// caller is still entitled to use.
///
/// Unlike [`gz_handle`] this deliberately does **not** go through
/// [`GzBorrow`]: it reads one `Copy` field and must not reconcile or re-sync the
/// `gzFile_s` prefix, so that a rejected close leaves the handle byte-for-byte
/// as it was.
///
/// # Safety
///
/// Same contract as [`gz_handle`]: `file` must be a non-null handle produced by
/// [`box_state`] (i.e. by `gzopen*`/`gzdopen`) and not yet closed.
#[cfg(feature = "gz-io")]
#[inline]
unsafe fn gz_handle_mode(file: gzFile) -> GzMode {
    // SAFETY: per the contract `file` points at a live `GzHandle` whose
    // `gzFile_s` prefix sits at offset 0 (`#[repr(C)]`), so the pointer may be
    // read as a `*const GzHandle`. Only the `Copy` `mode` field is read, through
    // a shared borrow that ends with this expression; nothing is mutated and no
    // ownership is taken.
    unsafe { (*(file as *const GzHandle)).state.mode }
}

/// Borrows the [`GzHandle`] behind the opaque `file` pointer.
///
/// # Safety
///
/// `file` must be a non-null handle produced by [`box_state`] (i.e. by
/// `gzopen*`/`gzdopen`) and not yet closed. Because [`gzFile_s`] is the first
/// field of the `#[repr(C)]` [`GzHandle`], the `gzFile` pointer has the same
/// address as the `GzHandle`.
#[cfg(feature = "gz-io")]
#[inline]
unsafe fn gz_handle<'a>(file: gzFile) -> &'a mut GzHandle {
    // SAFETY: per the contract `file` points at a live `GzHandle` (prefix at
    // offset 0); the returned borrow is used only for the duration of one shim.
    unsafe { &mut *(file as *mut GzHandle) }
}

/// RAII borrow of a [`GzHandle`]'s idiomatic [`GzState`] that keeps the C
/// `gzFile_s` prefix consistent: it [`reconcile`](GzHandle::reconcile)s on
/// creation (absorbing any `gzgetc` macro consumption) and
/// [`sync`](GzHandle::sync)s on drop (re-exposing the read buffer). Dereferences
/// to [`GzState`] so existing shim bodies are unchanged.
#[cfg(feature = "gz-io")]
struct GzBorrow<'a> {
    handle: &'a mut GzHandle,
}

#[cfg(feature = "gz-io")]
impl<'a> GzBorrow<'a> {
    #[inline]
    fn new(handle: &'a mut GzHandle) -> Self {
        handle.reconcile();
        Self { handle }
    }
}

#[cfg(feature = "gz-io")]
impl Drop for GzBorrow<'_> {
    #[inline]
    fn drop(&mut self) {
        self.handle.sync();
    }
}

#[cfg(feature = "gz-io")]
impl core::ops::Deref for GzBorrow<'_> {
    type Target = GzState;
    #[inline]
    fn deref(&self) -> &GzState {
        &self.handle.state
    }
}

#[cfg(feature = "gz-io")]
impl core::ops::DerefMut for GzBorrow<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut GzState {
        &mut self.handle.state
    }
}

/// Which open direction a close entry point requires of its handle.
///
/// Mirrors the `state->mode` test each C finalizer performs **before** it frees
/// or closes anything (`gzread.c` L650-L651, `gzwrite.c` L677-L678); see
/// [`take_for_close`].
#[cfg(feature = "gz-io")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CloseDirection {
    /// `gzclose` — dispatches on the mode rather than demanding one, so any live
    /// direction is acceptable (`gzclose.c` L11-L23).
    Either,
    /// `gzclose_r` — requires [`GzMode::Read`].
    Read,
    /// `gzclose_w` — requires [`GzMode::Write`].
    Write,
}

/// Reads the open direction of the handle behind `file` through a **borrow** and,
/// only if it matches `want`, reclaims the owning `Box<GzHandle>`.
///
/// # Why the ordering matters
///
/// Every C finalizer tests `state->mode` before it frees or closes anything:
///
/// ```c
/// if (state->mode != GZ_READ) return Z_STREAM_ERROR;   /* gzread.c  L650-L651 */
/// if (state->mode != GZ_WRITE) return Z_STREAM_ERROR;  /* gzwrite.c L677-L678 */
/// ```
///
/// A wrong-direction close is therefore a **pure no-op** in C: nothing is freed,
/// the descriptor stays open, and the caller still holds a fully live `gzFile` it
/// can retry with the correct closer — verified against reference zlib, which
/// answers `gzclose_w` on a reader with `Z_STREAM_ERROR` any number of times and
/// then still completes a `gzclose_r` with `Z_OK`.
///
/// Reconstructing the `Box` *before* the direction test would hand ownership to a
/// function that may reject the call, dropping the allocation while the caller
/// still holds the pointer — a use-after-free on the next operation and a
/// double-free on the retry with the correct closer (CWE-416, CWE-415). Taking
/// the box only after the direction matches removes that window entirely: on
/// refusal nothing is consumed and exactly one live handle remains.
///
/// The direction is read through a short-lived shared borrow rather than a
/// [`GzBorrow`] guard, because `GzBorrow` reconciles and then clears the
/// [`gzFile_s`] prefix. C mutates nothing on a refused close, so the prefix — and
/// with it the `gzgetc` macro fast path — must survive a rejection untouched.
///
/// # Safety
///
/// `file` must be a non-null handle produced by [`box_state`] and not yet closed.
/// On `Some`, ownership transfers to the caller and `file` is dangling; on `None`
/// the handle is untouched and remains valid.
#[cfg(feature = "gz-io")]
#[inline]
unsafe fn take_for_close(file: gzFile, want: CloseDirection) -> Option<Box<GzHandle>> {
    // The direction is read through [`gz_handle_mode`], the one place in this
    // module that touches `state.mode` without taking ownership: it borrows the
    // handle (and, through its `Box`, the `GzState`) only for that expression and
    // copies the `GzMode` out, so no reference outlives the read and none is live
    // when the box is reclaimed below. Nothing is mutated, so a rejected close
    // leaves the handle — prefix included — bit-for-bit as C leaves it.
    //
    // SAFETY: `take_for_close`'s own contract is `gz_handle_mode`'s contract —
    // `file` is a non-null, not-yet-closed handle produced by [`box_state`], so
    // its `gzFile_s` prefix sits at offset 0 and the cast inside is the identity
    // on the address.
    let mode = unsafe { gz_handle_mode(file) };

    let matches = match want {
        // `gzclose` has no mode test of its own; it dispatches with
        // `state->mode == GZ_READ ? gzclose_r(file) : gzclose_w(file)`. Both
        // arms then apply their own test, so a handle in neither direction is
        // rejected by the callee without being freed. Screening for a live
        // direction here reproduces that outcome without consuming the handle.
        CloseDirection::Either => matches!(mode, GzMode::Read | GzMode::Write),
        CloseDirection::Read => mode == GzMode::Read,
        CloseDirection::Write => mode == GzMode::Write,
    };

    if !matches {
        return None;
    }

    // SAFETY: `file` came from `Box::into_raw(Box<GzHandle>)` in `box_state` and
    // has not been closed, so reconstructing the box reclaims that exact
    // allocation. The direction matched, so the finalizer this feeds cannot
    // refuse the handle, and the box is therefore consumed exactly once.
    Some(unsafe { Box::from_raw(file as *mut GzHandle) })
}

// -- platform-close-failure seam --------------------------------------------

#[cfg(feature = "gz-io")]
#[cfg(test)]
std::thread_local! {
    /// When set, [`platform_close`] reports failure without consulting the OS,
    /// driving the `Z_ERRNO` arm of [`finish_close`].
    ///
    /// Reference zlib turns a failing `close(2)` into `Z_ERRNO`
    /// (`gzread.c` L665-L667, `gzwrite.c` L695-L696) — a path no ordinary
    /// in-process test can reach, because a descriptor a live `File` owns is by
    /// construction closable. The C conformance harness provokes it by stealing
    /// the descriptor behind the library's back; this seam reproduces the outcome
    /// deterministically instead.
    ///
    /// Thread-local because the harness runs each `#[test]` on its own thread, so
    /// concurrently running tests cannot observe one another's flag. Compiled out
    /// of every shipped artifact.
    static FORCE_CLOSE_FAILURE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Runs `body` with the platform-close-failure seam armed, clearing it again
/// afterwards even if `body` panics.
#[cfg(feature = "gz-io")]
#[cfg(test)]
fn with_forced_close_failure<R>(body: impl FnOnce() -> R) -> R {
    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            FORCE_CLOSE_FAILURE.set(false);
        }
    }
    FORCE_CLOSE_FAILURE.set(true);
    let _disarm = Disarm;
    body()
}

// POSIX `close(2)`. Declared directly rather than pulled from a `libc` crate to
// preserve the zero-C-dependency rule (AAP §0.5.2): this is a link-time
// reference to the platform C runtime that `std` already links, not a new crate
// dependency. It is the counterpart of the `malloc`/`free` declarations in
// `src/ffi/alloc.rs`.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
unsafe extern "C" {
    /// Closes a file descriptor, returning `0` on success and `-1` on failure.
    fn close(fd: c_int) -> c_int;
}

// Win32 `CloseHandle`. A Rust `File` owns a `HANDLE` (not a CRT `int fd`) on
// Windows, so the observable close result comes from `CloseHandle`, which returns
// a non-zero `BOOL` on success.
#[cfg(feature = "gz-io")]
#[cfg(windows)]
unsafe extern "system" {
    /// Closes an open object handle; non-zero on success.
    fn CloseHandle(handle: *mut core::ffi::c_void) -> c_int;
}

/// Closes `file`'s descriptor and reports whether the platform close succeeded.
///
/// This is the `unsafe` half of the C `close(state->fd)` step that
/// [`crate::gz::gzclose_r_release`] and [`crate::gz::gzclose_w_release`]
/// deliberately leave undone: [`std::fs::File`]'s [`Drop`] discards the result,
/// but zlib reports a failure as [`Z_ERRNO`](crate::error::ReturnCode::ErrNo), so the descriptor must be closed
/// explicitly to observe it.
///
/// Returns `true` on success, `false` if the platform reported an error.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
fn platform_close(file: std::fs::File) -> bool {
    #[cfg(test)]
    if FORCE_CLOSE_FAILURE.get() {
        // Still release the descriptor — a leaked fd would destabilise the rest
        // of the suite — then report the failure C would have reported.
        drop(file);
        return false;
    }

    use std::os::fd::IntoRawFd;

    // Take the raw descriptor so `File`'s `Drop` does not also close it: a double
    // close could shut a descriptor another thread has since been handed.
    let fd = file.into_raw_fd();

    // SAFETY: `fd` was just released from a live `File`, so it is a valid open
    // descriptor that nothing else owns, and `close(2)` is safe to call on it
    // exactly once. Ownership ended with `into_raw_fd`, so no Rust destructor
    // will close it again.
    unsafe { close(fd) == 0 }
}

/// Windows counterpart of [`platform_close`], using `CloseHandle` on the `HANDLE`
/// a [`std::fs::File`] owns.
#[cfg(feature = "gz-io")]
#[cfg(windows)]
fn platform_close(file: std::fs::File) -> bool {
    #[cfg(test)]
    if FORCE_CLOSE_FAILURE.get() {
        drop(file);
        return false;
    }

    use std::os::windows::io::IntoRawHandle;

    // Take the raw handle so `File`'s `Drop` does not also close it.
    let handle = file.into_raw_handle();

    // SAFETY: `handle` was just released from a live `File`, so it is a valid
    // open object handle that nothing else owns, and `CloseHandle` is safe to
    // call on it exactly once. Ownership ended with `into_raw_handle`, so no Rust
    // destructor will close it again.
    unsafe { CloseHandle(handle) != 0 }
}

/// Fallback [`platform_close`] for targets that are neither Unix nor Windows,
/// where `std` exposes no way to observe the platform close result. The handle is
/// closed by [`Drop`] and success is reported, matching the pre-existing
/// behaviour on such targets.
#[cfg(feature = "gz-io")]
#[cfg(not(any(unix, windows)))]
fn platform_close(file: std::fs::File) -> bool {
    #[cfg(test)]
    if FORCE_CLOSE_FAILURE.get() {
        drop(file);
        return false;
    }

    drop(file);
    true
}

/// Applies zlib's close-result precedence to the `(status, descriptor)` pair a
/// `gz::gzclose*_release` finalizer produced.
///
/// C spells the rule two ways that mean the same thing — `gzclose_w` writes
/// `if (close(state->fd) == -1) ret = Z_ERRNO;` before `return ret`
/// (`gzwrite.c` L695-L696), letting a close failure override the accumulated
/// flush status, and `gzclose_r` writes `return ret ? Z_ERRNO : err;`
/// (`gzread.c` L665-L667). Either way: a failing close yields
/// [`Z_ERRNO`](crate::error::ReturnCode::ErrNo) and a succeeding one yields the
/// accumulated status.
///
/// `None` means the finalizer refused the handle (wrong direction) and released
/// no descriptor, so `status` — `Z_STREAM_ERROR` — passes through unchanged.
#[cfg(feature = "gz-io")]
#[inline]
fn finish_close(status: c_int, released: Option<std::fs::File>) -> c_int {
    match released {
        // C `close(state->fd)`: a failure becomes `Z_ERRNO`, a success leaves the
        // accumulated status in place.
        Some(file) => {
            if platform_close(file) {
                status
            } else {
                ReturnCode::ErrNo.as_c_int()
            }
        }
        None => status,
    }
}

/// Boxes an idiomatic open result into the opaque `gzFile` handle, translating
/// failure into the C `NULL` sentinel. The handle is a [`GzHandle`] whose
/// leading [`gzFile_s`] prefix starts cleared (`have = 0`), so the `gzgetc`
/// macro falls through to the real function until the first read populates it.
#[cfg(feature = "gz-io")]
#[inline]
fn box_state(result: Result<Box<GzState>, ReturnCode>) -> gzFile {
    match result {
        Ok(state) => {
            // Fallible boxing: C `gzopen` reports every allocation failure by
            // returning `NULL`, never by aborting (`gzlib.c` L206-L210), so an
            // exhausted Rust heap must produce the same `NULL` sentinel
            // (AAP §0.6.5). The dropped `state` closes its file descriptor and
            // releases its buffers.
            let handle = GzHandle {
                prefix: gzFile_s {
                    have: 0,
                    next: ptr::null_mut(),
                    pos: 0,
                },
                state,
            };
            match crate::ffi::alloc::try_box(handle) {
                Some(boxed) => Box::into_raw(boxed) as gzFile,
                None => ptr::null_mut(),
            }
        }
        Err(_) => ptr::null_mut(),
    }
}

// POSIX `fcntl`, the one way to reconcile an already-open descriptor's
// close-on-exec and non-blocking state with what a gzip mode string asked for.
// Declared here for the same reason `close`, `CloseHandle` and `_get_osfhandle`
// are: it resolves against the platform C runtime, never against a C zlib, so the
// zero-C-dependency rule is preserved, and the raw call is confined to this
// boundary module — `src/gz/**` stays `unsafe`-free (AAP §0.8.1 D-6).
#[cfg(feature = "gz-io")]
#[cfg(unix)]
unsafe extern "C" {
    /// `int fcntl(int fd, int cmd, ...)`
    ///
    /// Declared variadic, exactly as POSIX specifies it, so the compiler emits a
    /// conforming variadic call. `F_GETFD`/`F_GETFL` take no third argument;
    /// `F_SETFD`/`F_SETFL` take one `int`.
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

/// `fcntl` command: get the descriptor flags (`FD_CLOEXEC`).
#[cfg(feature = "gz-io")]
#[cfg(unix)]
const F_GETFD: c_int = 1;
/// `fcntl` command: set the descriptor flags.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
const F_SETFD: c_int = 2;
/// `fcntl` command: get the file status flags (`O_NONBLOCK`, …).
#[cfg(feature = "gz-io")]
#[cfg(unix)]
const F_GETFL: c_int = 3;
/// `fcntl` command: set the file status flags.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
const F_SETFL: c_int = 4;

/// Reconciles a descriptor **opened by path** with the close-on-exec state C
/// would have given it.
///
/// C puts `O_CLOEXEC` into `oflag` only for mode `'e'` (`gzlib.c` L134-L138) and
/// then calls `open(path, oflag, 0666)`, so without `'e'` the descriptor is
/// **not** close-on-exec and survives an `exec`. A Rust [`std::fs::File`] is
/// close-on-exec unconditionally, so the divergence is in the *absence* of the
/// flag, and matching C means clearing `FD_CLOEXEC` here (finding M6-02).
///
/// `O_NONBLOCK` needs nothing on this path: it *is* expressible as an open flag
/// and `crate::gz` already passes it through `OpenOptionsExt::custom_flags`.
///
/// A failing `fcntl` is ignored, exactly as C ignores its own `fcntl` results.
/// The consequence of an ignored failure is a descriptor that is close-on-exec
/// when C's would not have been — the pre-existing behaviour, never anything
/// less safe.
///
/// This applies to the **C ABI only**. The idiomatic `crate::gz::gzopen` keeps
/// the standard library's always-close-on-exec descriptors, which is strictly
/// safer and carries no C obligation.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
fn reconcile_opened_descriptor(state: &GzState, mode: &[u8]) {
    use std::os::fd::AsRawFd;

    if gz::descriptor_request(mode).cloexec {
        // C asked for `O_CLOEXEC`, which is what `File::open` already produced.
        return;
    }

    let fd = state.file.as_raw_fd();
    // SAFETY: `fd` is owned by `state.file`, which is alive for this call, so it
    // is a valid open descriptor. `F_GETFD` reads flags and takes no third
    // argument.
    let flags = unsafe { fcntl(fd, F_GETFD) };
    if flags < 0 {
        return;
    }
    let cleared = flags & !crate::gz::DescriptorRequest::FD_CLOEXEC;
    if cleared != flags {
        // SAFETY: as above; `F_SETFD` takes exactly one `int` argument, supplied
        // here, and the descriptor is still owned by `state.file`.
        let _ = unsafe { fcntl(fd, F_SETFD, cleared) };
    }
}

/// Reconciles an **adopted** descriptor (`gzdopen`) with the status flags C sets
/// on it.
///
/// C never calls `open` on this path, so it applies the requested bit with
/// `fcntl` instead:
///
/// ```c
/// if (oflag & O_NONBLOCK)
///     fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
/// if (oflag & O_CLOEXEC)
///     fcntl(fd, F_SETFD, fcntl(fd, F_GETFD) | O_CLOEXEC);
/// ```
///
/// (`gzlib.c` L253-L263.) Only the first is reproduced, and that is deliberate:
/// **C's second `fcntl` is a no-op on every POSIX platform.** `F_SETFD`'s only
/// defined flag is `FD_CLOEXEC`, which is `1`, whereas `O_CLOEXEC` is a
/// completely different bit (`0o2000000` on Linux); Linux implements `F_SETFD` as
/// `set_close_on_exec(fd, arg & FD_CLOEXEC)`, so OR-ing `O_CLOEXEC` contributes
/// nothing at all. Measured against reference zlib: an adopted descriptor that
/// started *without* close-on-exec still has none after `gzdopen(fd, "wbe")`,
/// and one that started *with* it keeps it. Reproducing the no-op would mean
/// setting close-on-exec where C does not (finding M6-03).
///
/// A failing `fcntl` is ignored, as in C.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
fn reconcile_adopted_descriptor(state: &GzState, mode: &[u8]) {
    use std::os::fd::AsRawFd;

    if !gz::descriptor_request(mode).nonblock {
        return;
    }

    let fd = state.file.as_raw_fd();
    // SAFETY: `fd` is owned by `state.file`, which is alive for this call.
    // `F_GETFL` reads the status flags and takes no third argument.
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return;
    }
    // SAFETY: as above; `F_SETFL` takes exactly one `int` argument.
    let _ = unsafe {
        fcntl(
            fd,
            F_SETFL,
            flags | crate::gz::DescriptorRequest::O_NONBLOCK,
        )
    };
}

// ===========================================================================
// Phase 1 — Open shims  (<- gzlib.c)
// ===========================================================================

/// `gzFile gzopen(const char *path, const char *mode)`
///
/// Opens `path` for reading or writing according to the zlib `mode` string
/// (level digits, `r`/`w`/`a`, `b`, strategy flags, `T`, `G`, …). The mode
/// string is parsed entirely inside the idiomatic layer. Returns `NULL` on any
/// failure (including a null `path`/`mode` or a non-decodable path).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzopen(path: *const c_char, mode: *const c_char) -> gzFile {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (path, mode);
        ptr::null_mut()
    }

    #[cfg(feature = "gz-io")]
    {
        guard_ptr(ptr::null_mut(), || -> gzFile {
            if path.is_null() || mode.is_null() {
                return ptr::null_mut();
            }
            // SAFETY: `path` is non-null (checked) and, per the C contract, a
            // caller-owned NUL-terminated string valid for this call.
            let Some(pathbuf) = (unsafe { cpath_to_pathbuf(path) }) else {
                return ptr::null_mut();
            };
            // SAFETY: `mode` is non-null (checked) and NUL-terminated.
            //
            // `to_bytes`, not `to_str`: C walks the mode one raw byte at a time and
            // ignores every byte it does not recognise (`gzlib.c` L113-L170), so
            // `"rb\xff"` opens exactly what `"rb"` opens. Requiring UTF-8 here would
            // turn that accepted open into `NULL`.
            let mode_bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
            let opened = gz::gzopen_bytes(pathbuf, mode_bytes);
            // C's descriptor is close-on-exec only for mode `'e'`; a Rust `File`
            // always is. See `reconcile_opened_descriptor`.
            #[cfg(unix)]
            if let Ok(state) = opened.as_ref() {
                reconcile_opened_descriptor(state, mode_bytes);
            }
            box_state(opened)
        })
    }
}

/// `gzFile gzopen64(const char *path, const char *mode)`
///
/// 64-bit-offset variant. Rust file offsets are 64-bit by default, so this
/// simply delegates to the idiomatic `gzopen64` (behaviorally identical to
/// [`gzopen`] on this platform).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzopen64(path: *const c_char, mode: *const c_char) -> gzFile {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (path, mode);
        ptr::null_mut()
    }

    #[cfg(feature = "gz-io")]
    {
        guard_ptr(ptr::null_mut(), || -> gzFile {
            if path.is_null() || mode.is_null() {
                return ptr::null_mut();
            }
            // SAFETY: `path` is non-null (checked) and NUL-terminated.
            let Some(pathbuf) = (unsafe { cpath_to_pathbuf(path) }) else {
                return ptr::null_mut();
            };
            // SAFETY: `mode` is non-null (checked) and NUL-terminated.
            //
            // `to_bytes`, not `to_str`: C walks the mode one raw byte at a time and
            // ignores every byte it does not recognise (`gzlib.c` L113-L170), so
            // `"rb\xff"` opens exactly what `"rb"` opens. Requiring UTF-8 here would
            // turn that accepted open into `NULL`.
            let mode_bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
            let opened = gz::gzopen64_bytes(pathbuf, mode_bytes);
            // See `gzopen` — identical descriptor reconciliation.
            #[cfg(unix)]
            if let Ok(state) = opened.as_ref() {
                reconcile_opened_descriptor(state, mode_bytes);
            }
            box_state(opened)
        })
    }
}

// Windows CRT `_get_osfhandle`: maps a CRT `int` file descriptor onto the OS
// `HANDLE` backing it, which is what a Rust `std::fs::File` owns on Windows.
// Declaring the CRT entry point directly is the approach this module already
// takes for `close` and `CloseHandle`; it resolves against the platform C
// runtime, never against a C zlib, so the zero-C-dependency rule is preserved.
#[cfg(feature = "gz-io")]
#[cfg(windows)]
unsafe extern "C" {
    /// Returns the OS handle backing CRT descriptor `fd`, or `-1`/`-2` when that
    /// descriptor is not open.
    fn _get_osfhandle(fd: c_int) -> isize;
}

/// Adopts the caller's raw descriptor into a [`std::fs::File`] that owns it.
///
/// # Why an invalid descriptor is adopted rather than rejected
///
/// `zlib.h` L1422-L1426 states the contract outright: `gzdopen` returns `NULL`
/// "if there was insufficient memory to allocate the gzFile state, if an invalid
/// mode was specified …, or if fd is -1", and "the file descriptor is not used
/// until the next gz\* read, write, seek, or close operation, so gzdopen will not
/// detect if fd is invalid (unless fd is -1)". C implements exactly that: one
/// `fd == -1` test (`gzlib.c` L300) and no other descriptor validation at all.
///
/// So a descriptor that is negative-but-not-`-1`, or merely closed, must open
/// successfully and fail at the *first* read, write, seek or close. Rejecting it
/// here would turn C's **deferred** [`Z_ERRNO`](crate::error::ReturnCode::ErrNo) into an **immediate** `NULL`,
/// denying the caller the very handle it needs to read that error from. This
/// helper therefore adopts unconditionally; `fd == -1` is screened by the caller,
/// before adoption, because that is the one case C screens too.
///
/// # Safety
///
/// The caller transfers ownership of `fd`: nothing else may own or close it. The
/// descriptor need *not* be open — the raw-conversion contract of
/// [`std::fs::File`] is about sole ownership, and every operation on a closed
/// descriptor simply fails, which is precisely the deferred error C exhibits.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
unsafe fn adopt_descriptor(fd: c_int) -> std::fs::File {
    // SAFETY: forwarded verbatim from this function's own contract — the caller
    // transferred sole ownership of `fd`, so wrapping it here creates exactly one
    // owner and no other Rust value can close it.
    unsafe { std::fs::File::from_raw_fd(fd) }
}

/// Windows counterpart of `adopt_descriptor`, resolving the CRT descriptor to
/// the OS handle a [`std::fs::File`] owns on this platform.
///
/// The rejection policy, and the reason an invalid descriptor is adopted rather
/// than refused, are identical to the Unix twin — see its documentation.
///
/// One Windows-only residual difference is deliberate and documented rather than
/// silent: C's Windows build keeps the CRT `int` descriptor and closes it with
/// `_close`, releasing both the OS handle *and* the CRT table slot, whereas a
/// [`std::fs::File`] owns the `HANDLE` and closes it with `CloseHandle`, leaving
/// the CRT slot allocated. The observable close result — the value
/// [`gzclose`]/[`gzclose_w`] reports — comes from closing the handle either way,
/// so the ABI-visible behaviour matches; only the CRT-internal slot differs, and
/// a caller must not `_close` a descriptor whose ownership it has handed away.
#[cfg(feature = "gz-io")]
#[cfg(windows)]
unsafe fn adopt_descriptor(fd: c_int) -> std::fs::File {
    // SAFETY: `_get_osfhandle` only reads the CRT's own descriptor table and
    // reports `-1`/`-2` for a descriptor that is not open, so it is safe to call
    // for any `int`, valid or not.
    let raw = unsafe { _get_osfhandle(fd) };

    // SAFETY: the caller transferred sole ownership of `fd`, so the handle the CRT
    // reports for it has exactly one owner, which now becomes this `File`. A
    // `-1`/`-2` result (`INVALID_HANDLE_VALUE`) is adopted deliberately rather
    // than rejected — see the Unix twin for why C's deferred-failure contract
    // requires it. Every `ReadFile`/`WriteFile` on such a handle fails, raising
    // the same `Z_ERRNO` C raises on its first `_read`.
    unsafe { std::fs::File::from_raw_handle(raw as *mut core::ffi::c_void) }
}

/// Ends Rust ownership of `file`'s descriptor **without closing it**, handing it
/// back to the caller exactly as it was passed in.
///
/// This is the release half of the `gzdopen` ownership contract: C never reaches
/// its allocation failures with the descriptor stored (`gzlib.c` assigns
/// `state->fd` only at the open/adopt step), so no `gzdopen` failure may close
/// what the caller still owns.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
fn release_descriptor(file: std::fs::File) {
    use std::os::fd::IntoRawFd;

    let _ = file.into_raw_fd();
}

/// Windows counterpart of `release_descriptor`: dissolves the [`std::fs::File`]
/// without `CloseHandle`, leaving both the OS handle and the CRT descriptor the
/// caller passed in open and usable.
#[cfg(feature = "gz-io")]
#[cfg(windows)]
fn release_descriptor(file: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;

    let _ = file.into_raw_handle();
}

/// `gzFile gzdopen(int fd, const char *mode)`
///
/// Associates a `gz*` stream with an already-open file descriptor. On **success**
/// ownership of `fd` transfers to the returned handle, which closes it in
/// [`gzclose`]/[`gzclose_r`]/[`gzclose_w`].
///
/// On **failure the caller keeps `fd`**, open and usable, exactly as in C. C
/// `gzdopen` builds a `<fd:N>` path string and calls `gz_open`, which performs
/// every mode-grammar rejection and its own `malloc` before it ever stores the
/// descriptor in `state->fd` (`gzlib.c` L150-L197 and L206-L210 precede L263), so
/// no C failure path closes the caller's descriptor and a C caller may retry or
/// `close(fd)` itself. This shim reproduces that contract on every failure path:
///
/// * `fd == -1` and a null `mode` are rejected before `adopt_descriptor`, so
///   the descriptor is never adopted;
/// * an **invalid mode string** (`"r+"`, `"rT"`, `"wG"`, a string with no
///   `r`/`w`/`a`, …) is rejected by `crate::gz::validate_mode`, also before
///   adoption — this is the check C performs at `gzlib.c` L150-L197; and
/// * if the handle allocation fails after adoption, the descriptor is released
///   back to the OS-owned world with `release_descriptor` rather than closed —
///   the analogue of C's `malloc` failure at `gzlib.c` L206-L210, which likewise
///   leaves `fd` open.
///
/// Returns `NULL` on any of those failures.
///
/// # Which descriptors are rejected — exactly one
///
/// Only `fd == -1`. Every other `int`, including a negative one and a closed one,
/// yields a live handle whose first read, write, seek or close reports
/// [`Z_ERRNO`](crate::error::ReturnCode::ErrNo), because that is the documented C contract (`zlib.h` L1422-L1426,
/// `gzlib.c` L300). `adopt_descriptor` carries the full argument.
///
/// # The mode is bytes, not UTF-8
///
/// C walks the mode one raw byte at a time and ignores every byte it does not
/// recognise (`gzlib.c` L113-L170), so `"rb\xff"` opens exactly what `"rb"`
/// opens. The mode is therefore read with [`CStr::to_bytes`](core::ffi::CStr::to_bytes) and never required
/// to be valid UTF-8.
///
/// [`File`]: std::fs::File
#[cfg(any(unix, windows))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzdopen(fd: c_int, mode: *const c_char) -> gzFile {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (fd, mode);
        ptr::null_mut()
    }

    #[cfg(feature = "gz-io")]
    {
        guard_ptr(ptr::null_mut(), || -> gzFile {
            // `fd == -1` is the ONLY descriptor C rejects (`gzlib.c` L300); every
            // other value is adopted and its errors deferred to the first
            // operation. Widening this test to `fd < 0` would refuse handles C
            // hands back.
            if mode.is_null() || fd == -1 {
                return ptr::null_mut();
            }
            // `to_bytes`, not `to_str`: C walks the mode one raw byte at a time and
            // ignores every byte it does not recognise (`gzlib.c` L113-L170), so
            // `"rb\xff"` opens exactly what `"rb"` opens.
            //
            // SAFETY: `mode` is non-null (checked) and NUL-terminated.
            let mode_bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
            // C `gz_open` validates the whole mode grammar before it takes the
            // caller's descriptor, so every rejection must happen while `fd` is still
            // the caller's. Adopting first and letting the open fail would close a
            // descriptor C leaves open — an ownership divergence a C caller cannot
            // detect and that turns its own later `close(fd)` into a double close.
            if gz::validate_mode(mode_bytes).is_err() {
                return ptr::null_mut();
            }
            // SAFETY: the caller transfers ownership of `fd` (the `-1` sentinel is
            // screened above), so nothing else owns or will close it. Adoption
            // happens only now, with the mode already proven acceptable, so the
            // remaining failure mode is an exhausted heap — handled by releasing the
            // descriptor below rather than closing it. The descriptor need not be
            // open: `adopt_descriptor` documents why C's contract requires adopting
            // an invalid one and deferring the error.
            let file = unsafe { adopt_descriptor(fd) };
            let opened = gz::gzdopen_bytes(file, mode_bytes);
            // C applies `O_NONBLOCK` to an adopted descriptor with `fcntl`,
            // because it never calls `open` here. See
            // `reconcile_adopted_descriptor` for why its companion `O_CLOEXEC`
            // `fcntl` is deliberately *not* reproduced.
            #[cfg(unix)]
            if let Ok(state) = opened.as_ref() {
                reconcile_adopted_descriptor(state, mode_bytes);
            }
            dopen_state(opened)
        })
    }
}

/// [`box_state`] for the `gzdopen` path: identical on success, but an allocation
/// failure **releases** the adopted descriptor instead of closing it.
///
/// C reaches its `malloc` failures (`gzlib.c` L206-L210 and the `state->path`
/// allocation) before `state->fd = fd`, so no allocation failure in `gzdopen`
/// closes the caller's descriptor. Here the descriptor is already inside a
/// [`std::fs::File`] whose [`Drop`] *would* close it, so it is lifted out of the
/// state before the fallible boxing and only put back once that boxing has
/// succeeded. If the boxing fails the descriptor is handed back to the OS-owned
/// world with `release_descriptor`, deliberately leaving it open for the caller
/// — precisely what "the caller still owns `fd`" means — while the rest of the
/// state drops, matching C's `free(state)`.
#[cfg(feature = "gz-io")]
#[cfg(any(unix, windows))]
fn dopen_state(result: Result<Box<GzState>, ReturnCode>) -> gzFile {
    // The mode was pre-validated, so a mode rejection cannot reach here; an
    // exhausted allocator can, because `gzdopen`/`gz_open` allocate the `<fd:N>`
    // name, the state, and the retained path name before opening anything.
    //
    // On every one of those paths the descriptor is handed BACK to the caller
    // unclosed — `gz_open`'s `abandon_adopted` dissolves the adopted `File`
    // with `core::mem::forget` rather than dropping it — which is precisely C's
    // contract: no `gzdopen` failure in reference zlib closes the descriptor it
    // was given (`gzlib.c` assigns `state->fd` only at the open/adopt step, so
    // every earlier `return NULL` leaves `fd` untouched). There is therefore
    // nothing for this function to release, and returning `NULL` here leaves the
    // caller owning exactly what it owned before the call.
    let Ok(mut state) = result else {
        return ptr::null_mut();
    };

    // Lift the descriptor out so the fallible boxing below cannot close it. This
    // is what makes the ownership transfer atomic with respect to success: until
    // the handle exists, nothing that can be dropped owns the caller's `fd`.
    let released = state.file.release();

    let handle = GzHandle {
        prefix: gzFile_s {
            have: 0,
            next: ptr::null_mut(),
            pos: 0,
        },
        state,
    };

    match crate::ffi::alloc::try_box(handle) {
        Some(mut boxed) => {
            // The handle exists; ownership of the descriptor now transfers to it,
            // to be closed by `gzclose`/`gzclose_r`/`gzclose_w`.
            if let Some(file) = released {
                boxed.state.file = GzFile::new(file);
            }
            Box::into_raw(boxed) as gzFile
        }
        None => {
            // Allocation failed. C never got this far with the descriptor, so
            // give it back to the caller unclosed: `release_descriptor` dissolves
            // the `File` without closing, leaving `fd` exactly as the caller
            // passed it. `handle` (and with it the buffers) drops here — C's
            // `free(state)`.
            if let Some(file) = released {
                release_descriptor(file);
            }
            ptr::null_mut()
        }
    }
}

/// `gzFile gzdopen(int fd, const char *mode)`  *(neither Unix nor Windows)*
///
/// Adopting a raw C `int` file descriptor requires a platform primitive that maps
/// it onto whatever `std` owns — `from_raw_fd` on Unix, `_get_osfhandle` plus
/// `from_raw_handle` on Windows (both implemented above). Targets that are
/// neither expose no such primitive, so the symbol is retained for ABI
/// completeness but always fails here; use [`gzopen`]/`gzopen_w` instead.
#[cfg(not(any(unix, windows)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzdopen(_fd: c_int, _mode: *const c_char) -> gzFile {
    ptr::null_mut()
}

/// `int gzbuffer(gzFile file, unsigned size)`
///
/// Sets the internal buffer size for a freshly opened handle. Returns `0` on
/// success and `-1` if called after the first read/write has begun (the engine
/// enforces the ordering) or for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzbuffer(file: gzFile, size: c_uint) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, size);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle from `gzopen*`/`gzdopen`; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzbuffer(state, size)
        })
    }
}

/// `int gzsetparams(gzFile file, int level, int strategy)`
///
/// Dynamically updates the compression level and strategy of a write stream.
/// Returns a zlib return code (`Z_OK` on success), or `Z_STREAM_ERROR` for a
/// null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzsetparams(file: gzFile, level: c_int, strategy: c_int) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, level, strategy);
        Z_STREAM_ERROR
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(Z_STREAM_ERROR, || {
            if file.is_null() {
                return Z_STREAM_ERROR;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzsetparams(state, level, strategy)
        })
    }
}

// ===========================================================================
// Phase 2 — Read shims  (<- gzread.c)
// ===========================================================================

/// `int gzread(gzFile file, voidp buf, unsigned len)`
///
/// Reads up to `len` uncompressed bytes into `buf`. Returns the number of bytes
/// actually read (`0` at end of file), or `-1` on error or a null handle.
///
/// # A null `buf` with `len == 0`
///
/// C never inspects `buf`. It validates the handle and the direction, clears any
/// recoverable error, and then calls the internal `gz_read`, whose very first
/// statement is `if (len == 0) return 0;` (`gzread.c` L321-L322) — so the buffer
/// pointer is never touched and `gzread(file, NULL, 0)` answers `0`, exactly like
/// `gzread(file, buf, 0)`. This shim reproduces that: `buf` is rejected only when
/// `len` is nonzero.
///
/// A null `buf` with a nonzero `len` is *undefined behaviour in C* — reference
/// zlib passes the pointer to `memcpy` and segfaults. Returning `-1` there is a
/// deliberately safe superset of the C contract, not a divergence from any
/// defined behaviour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzread(file: gzFile, buf: voidp, len: c_uint) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, buf, len);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            // C inspects `buf` nowhere; a zero-length request returns before the
            // pointer is used (`gzread.c` L321-L322). Only a nonzero length makes a
            // null buffer unusable.
            if file.is_null() || (buf.is_null() && len != 0) {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            let out: &mut [u8] = if len == 0 {
                // No bytes are requested, so no buffer is formed - `buf` may legally
                // be null here and `from_raw_parts_mut` requires non-null even for a
                // zero length.
                &mut []
            } else {
                // SAFETY: `buf` is non-null (checked above, since `len != 0`) and, per
                // the C contract, valid for writes of `len` bytes.
                unsafe { slice::from_raw_parts_mut(buf as *mut u8, len as usize) }
            };
            gz::gzread(state, out)
        })
    }
}

/// `z_size_t gzfread(voidp buf, z_size_t size, z_size_t nitems, gzFile file)`
///
/// Reads `nitems` items of `size` bytes each. Returns the number of full items
/// read. On multiplication overflow the idiomatic layer records
/// `Z_STREAM_ERROR` and returns `0`, mirroring C. Note the C argument order:
/// the handle is the *last* parameter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzfread(
    buf: voidp,
    size: z_size_t,
    nitems: z_size_t,
    file: gzFile,
) -> z_size_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (buf, size, nitems, file);
        0
    }

    #[cfg(feature = "gz-io")]
    {
        guard_size(0, || {
            if file.is_null() {
                return 0;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            let Some(len) = size.checked_mul(nitems) else {
                // Overflow: let the idiomatic layer record Z_STREAM_ERROR and
                // return 0 (an empty slice cannot itself trigger a read).
                return gz::gzfread(state, &mut [], size, nitems);
            };
            if len == 0 {
                // Zero-length request (size == 0 or nitems == 0): drive the
                // idiomatic checks with an empty slice; `buf` may legitimately be
                // null in this case.
                return gz::gzfread(state, &mut [], size, nitems);
            }
            if buf.is_null() {
                // Non-zero length with no destination: decline safely rather than
                // constructing a slice over a null pointer.
                return 0;
            }
            // SAFETY: `buf` is non-null (checked) and valid for `len` bytes;
            // `len == size * nitems` did not overflow.
            let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len) };
            gz::gzfread(state, out, size, nitems)
        })
    }
}

/// `int gzgetc(gzFile file)`
///
/// Reads one byte, returning it as `0..=255`, or `-1` at end of file / on error
/// / for a null handle. Exported as a real function; see the module docs for
/// the macro-parity note.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzgetc(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzgetc(state)
        })
    }
}

/// `int gzgetc_(gzFile file)`
///
/// The explicit-function form of [`gzgetc`] (`ZLIB_1.2.5.2`). Behaviorally
/// identical; both symbols are exported so either linkage resolves.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzgetc_(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzgetc_(state)
        })
    }
}

/// `char *gzgets(gzFile file, char *buf, int len)`
///
/// Reads a NUL-terminated line (at most `len - 1` bytes plus the terminator)
/// into `buf`. Returns `buf` on success, or `NULL` at end of file with no data
/// read, on error, or for a null handle / null buffer / non-positive `len`. The
/// idiomatic layer guarantees NUL-termination within `len`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzgets(file: gzFile, buf: *mut c_char, len: c_int) -> *mut c_char {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, buf, len);
        ptr::null_mut()
    }

    #[cfg(feature = "gz-io")]
    {
        guard_ptr(ptr::null_mut(), || -> *mut c_char {
            if file.is_null() || buf.is_null() || len <= 0 {
                return ptr::null_mut();
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            // `.cast::<u8>()` rather than `as *mut u8`: `c_char` is `i8` on x86_64
            // but `u8` on aarch64 and s390x, where the `as` form becomes an
            // identity cast and `clippy::unnecessary_cast` fires — a lint a
            // Linux/x86_64-only lane never surfaces. The method form expresses the
            // same reinterpretation on every target with no lint exemption.
            // SAFETY: `buf` is non-null (checked) and valid for `len` bytes
            // (`len > 0` checked). The idiomatic writer NUL-terminates within it.
            let out = unsafe { slice::from_raw_parts_mut(buf.cast::<u8>(), len as usize) };
            match gz::gzgets(state, out) {
                Some(_) => buf,
                None => ptr::null_mut(),
            }
        })
    }
}

/// `int gzungetc(int c, gzFile file)`
///
/// Pushes one byte back into the read stream. Returns `c` on success, or `-1`
/// on error / for a null handle. Note the C argument order: `c` precedes the
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzungetc(c: c_int, file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (c, file);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzungetc(c, state)
        })
    }
}

// ===========================================================================
// Phase 3 — Write shims  (<- gzwrite.c)
// ===========================================================================

/// `int gzwrite(gzFile file, voidpc buf, unsigned len)`
///
/// Compresses and writes `len` bytes from `buf`. Returns the number of
/// (uncompressed) bytes written, or `0` on error / for a null handle (or a null
/// `buf` with `len > 0`) — note C `gzwrite` uses `0`, not `-1`, as its error
/// sentinel.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzwrite(file: gzFile, buf: voidpc, len: c_uint) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, buf, len);
        0
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(0, || {
            if file.is_null() {
                return 0;
            }
            if buf.is_null() && len != 0 {
                return 0;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            let input = if len == 0 {
                // Empty write: avoid forming a slice over a possibly-null pointer.
                &[][..]
            } else {
                // SAFETY: `buf` is non-null (checked above for `len > 0`) and valid
                // for reads of `len` bytes per the C contract.
                unsafe { slice::from_raw_parts(buf as *const u8, len as usize) }
            };
            gz::gzwrite(state, input)
        })
    }
}

/// `z_size_t gzfwrite(voidpc buf, z_size_t size, z_size_t nitems, gzFile file)`
///
/// Writes `nitems` items of `size` bytes each. Returns the number of full items
/// written. On multiplication overflow the idiomatic layer records
/// `Z_STREAM_ERROR` and returns `0`. Note the C argument order: the handle is
/// the *last* parameter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzfwrite(
    buf: voidpc,
    size: z_size_t,
    nitems: z_size_t,
    file: gzFile,
) -> z_size_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (buf, size, nitems, file);
        0
    }

    #[cfg(feature = "gz-io")]
    {
        guard_size(0, || {
            if file.is_null() {
                return 0;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            let Some(len) = size.checked_mul(nitems) else {
                // Overflow: idiomatic layer records Z_STREAM_ERROR and returns 0.
                return gz::gzfwrite(state, &[], size, nitems);
            };
            if len == 0 {
                // Zero-length request: drive the idiomatic checks with an empty
                // slice; `buf` may legitimately be null.
                return gz::gzfwrite(state, &[], size, nitems);
            }
            if buf.is_null() {
                return 0;
            }
            // SAFETY: `buf` is non-null (checked) and valid for `len` bytes;
            // `len == size * nitems` did not overflow.
            let input = unsafe { slice::from_raw_parts(buf as *const u8, len) };
            gz::gzfwrite(state, input, size, nitems)
        })
    }
}

/// `int gzputc(gzFile file, int c)`
///
/// Writes the low byte of `c`. Returns the byte written on success, or `-1` on
/// error / for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzputc(file: gzFile, c: c_int) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, c);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzputc(state, c)
        })
    }
}

/// `int gzputs(gzFile file, const char *s)`
///
/// Writes the NUL-terminated string `s` (excluding the terminator). Returns the
/// number of bytes written, or `-1` on error / for a null handle or null `s`.
///
/// The idiomatic writer takes `&str`; a string that is not valid UTF-8 is
/// written faithfully via the byte-oriented writer, still honoring the `-1`
/// error sentinel.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzputs(file: gzFile, s: *const c_char) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, s);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() || s.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            // SAFETY: `s` is non-null (checked) and a NUL-terminated C string.
            let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
            match core::str::from_utf8(bytes) {
                Ok(text) => gz::gzputs(state, text),
                Err(_) => {
                    // Non-UTF-8: write raw bytes, preserving gzputs' -1-on-error
                    // contract (`gzwrite` returns 0 on error).
                    let n = gz::gzwrite(state, bytes);
                    if n == 0 && !bytes.is_empty() { -1 } else { n }
                }
            }
        })
    }
}

/// `int gzflush(gzFile file, int flush)`
///
/// Flushes pending output with the given flush mode. Returns a zlib return code
/// (`Z_OK` on success), or `Z_STREAM_ERROR` for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzflush(file: gzFile, flush: c_int) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, flush);
        Z_STREAM_ERROR
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(Z_STREAM_ERROR, || {
            if file.is_null() {
                return Z_STREAM_ERROR;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzflush(state, flush)
        })
    }
}

// ---------------------------------------------------------------------------
// gzprintf / gzvprintf — the `NO_vsnprintf && !ZLIB_INSECURE` zlib ABI variant
// ---------------------------------------------------------------------------
//
// These two symbols implement the documented zlib build variant in which no
// secure `vsnprintf`/`snprintf` is available: both functions are exported with
// their exact C signatures and return `Z_STREAM_ERROR`, and `zlibCompileFlags`
// bit 27 is set to advertise it (`zlib.h`: bit 27 "1 means gzprintf() returns
// an error" / "gzprintf() returns Z_STREAM_ERROR"; see `crate::util::version`).
// This is a faithful reproduction of a real zlib configuration, not a stub with
// deferred work.
//
// Why this variant (and not a functional C-variadic definition): rendering a C
// `va_list` requires the unstable, nightly-only `c_variadic` feature, which is
// incompatible with the crate's stable MSRV-1.85 contract (AAP §0.7.2 standard S7), and
// delegating to a C `vsnprintf` would reintroduce a C dependency, violating the
// zero-C-dependency rule (AAP §0.5.2). Rust consumers lose nothing: the
// idiomatic `crate::gz::gzprintf`/`gzvprintf` render `core::fmt::Arguments`
// fully. Only the raw C-variadic ABI entry points reflect the documented
// variant.
//
// ABI note: the exports use the *fixed* leading parameters of the C prototypes.
// On the SysV (x86-64) and Win64 C ABIs a caller invoking `gzprintf(f, fmt,
// ...)` passes the fixed leading arguments (`file`, `format`) in the same
// registers a non-variadic callee reads, and the caller owns stack cleanup, so
// a non-variadic callee that ignores the trailing arguments is ABI-safe. The C
// `va_list` of `gzvprintf` is represented as an opaque pointer.

/// `int gzvprintf(gzFile file, const char *format, va_list va)`
/// *(no-secure-`*printf` zlib ABI variant)*
///
/// Returns `Z_STREAM_ERROR` unconditionally, implementing the documented
/// `NO_vsnprintf && !ZLIB_INSECURE` zlib variant (its companion flag is
/// `zlibCompileFlags` bit 27). Rendering a C `va_list` requires the nightly-only
/// `c_variadic` feature, incompatible with the crate's stable MSRV (AAP §0.7.2 standard S7);
/// see the module note above for the ABI rationale and the fully functional
/// idiomatic `crate::gz::gzvprintf`.
#[unsafe(no_mangle)]
pub extern "C" fn gzvprintf(
    _file: gzFile,
    _format: *const c_char,
    _va: *mut core::ffi::c_void,
) -> c_int {
    Z_STREAM_ERROR
}

/// `int gzprintf(gzFile file, const char *format, ...)`
/// *(no-secure-`*printf` zlib ABI variant)*
///
/// See [`gzvprintf`]: returns `Z_STREAM_ERROR`, implementing the documented
/// `NO_vsnprintf && !ZLIB_INSECURE` zlib variant (advertised via
/// `zlibCompileFlags` bit 27) because a true C-variadic definition would need
/// the nightly-only `c_variadic` feature (AAP §0.7.2 standard S7). Rust callers use the
/// fully functional idiomatic `crate::gz::gzprintf`.
#[unsafe(no_mangle)]
pub extern "C" fn gzprintf(_file: gzFile, _format: *const c_char) -> c_int {
    Z_STREAM_ERROR
}

// ===========================================================================
// Phase 4 — Seek / position / status shims  (<- gzlib.c)
// ===========================================================================
//
// The 32-bit-offset entry points (`gzseek`, `gztell`, `gzoffset`) share their
// implementation with the 64-bit variants; the idiomatic layer computes in
// `i64` and the result is narrowed to `z_off_t` (`c_long`) at the boundary,
// exactly matching the C `off_t`/`off64_t` split.

/// Narrows a 64-bit offset to the C `z_off_t`, answering `-1` when the value does
/// not fit — the exact contract of the three narrow `gz*` entry points.
///
/// All three spell it identically in C (`gzlib.c` L437-L442, L459-L464,
/// L486-L491):
///
/// ```text
/// ret = gztell64(file);
/// return ret == (z_off_t)ret ? (z_off_t)ret : -1;
/// ```
///
/// The comparison is the whole point: the value is narrowed, widened back, and
/// accepted only if it survived the round trip. A bare `as` cast would instead
/// truncate silently, and truncation is not a harmless approximation here — it
/// manufactures a *plausible* offset. On a 32-bit target, `gztell` after seeking
/// to `1 << 40` truncates to `0`, so a caller checking for the documented `-1`
/// sees success and reads a position that is off by a terabyte. Reference C
/// answers `-1`; this helper is what makes the port answer `-1` too.
///
/// On targets where `z_off_t` is already 64 bits (LP64 Unix, which is where the
/// suite runs) the round trip is the identity and the branch is never taken; the
/// guard exists for ILP32 and LLP64 (32-bit Unix, and Windows, where `c_long` is
/// 32 bits regardless of pointer width).
#[cfg(feature = "gz-io")]
#[inline]
fn narrow_off(wide: z_off64_t) -> z_off_t {
    let narrow = wide as z_off_t;
    if z_off64_t::from(narrow) == wide {
        narrow
    } else {
        -1
    }
}

/// `z_off_t gzseek(gzFile file, z_off_t offset, int whence)`
///
/// Repositions the stream. Returns the resulting uncompressed offset, or `-1`
/// on error / for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzseek(file: gzFile, offset: z_off_t, whence: c_int) -> z_off_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, offset, whence);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        narrow_off(guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            // Widen the C `off_t` to the engine's 64-bit offset. On 32-bit targets
            // (`z_off_t == i32`) this is a real widening; on 64-bit it is a no-op.
            gz::gzseek(state, z_off64_t::from(offset), whence)
        }))
    }
}

/// `z_off64_t gzseek64(gzFile file, z_off64_t offset, int whence)` (`ZLIB_1.2.3.3`)
///
/// 64-bit-offset variant of [`gzseek`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzseek64(file: gzFile, offset: z_off64_t, whence: c_int) -> z_off64_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, offset, whence);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzseek64(state, offset, whence)
        })
    }
}

/// `int gzrewind(gzFile file)`
///
/// Rewinds a read stream to the beginning. Returns `0` on success, or `-1` on
/// error / for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzrewind(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzrewind(state)
        })
    }
}

/// `z_off_t gztell(gzFile file)`
///
/// Returns the current uncompressed offset, or `-1` for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gztell(file: gzFile) -> z_off_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        narrow_off(guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed immutably.
            let guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &*guard;
            gz::gztell(state)
        }))
    }
}

/// `z_off64_t gztell64(gzFile file)` (`ZLIB_1.2.3.3`)
///
/// 64-bit-offset variant of [`gztell`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gztell64(file: gzFile) -> z_off64_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed immutably.
            let guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &*guard;
            gz::gztell64(state)
        })
    }
}

/// `z_off_t gzoffset(gzFile file)` (`ZLIB_1.2.3.5`)
///
/// Returns the current *compressed* file offset, or `-1` on error / for a null
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzoffset(file: gzFile) -> z_off_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        narrow_off(guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzoffset(state)
        }))
    }
}

/// `z_off64_t gzoffset64(gzFile file)` (`ZLIB_1.2.3.5`)
///
/// 64-bit-offset variant of [`gzoffset`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzoffset64(file: gzFile) -> z_off64_t {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        -1
    }

    #[cfg(feature = "gz-io")]
    {
        guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzoffset64(state)
        })
    }
}

/// `int gzeof(gzFile file)`
///
/// Returns `1` once a read has attempted to go past end of file (mirroring C's
/// `past` flag semantics), otherwise `0`; `0` for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzeof(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        0
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(0, || {
            if file.is_null() {
                return 0;
            }
            // SAFETY: non-null handle; borrowed immutably.
            let guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &*guard;
            gz::gzeof(state)
        })
    }
}

/// `int gzdirect(gzFile file)` (`ZLIB_1.2.2.3`)
///
/// Returns `1` if the stream is being copied through transparently
/// (uncompressed input), `0` if it is being decompressed as gzip; `0` for a
/// null handle. May trigger a header look on first read, matching C.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzdirect(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        0
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(0, || {
            if file.is_null() {
                return 0;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzdirect(state)
        })
    }
}

// ===========================================================================
// Phase 5 — Error / close shims  (<- gzlib.c, gzclose.c)
// ===========================================================================

/// `const char *gzerror(gzFile file, int *errnum)`
///
/// Returns a pointer to the current error message for `file` and, if `errnum`
/// is non-null, writes the machine-readable error code there. The returned
/// pointer is the handle's own NUL-terminated message mirror — the specific
/// `"{path}: {detail}"` text (byte-for-byte matching reference zlib), the
/// literal `"out of memory"` for `Z_MEM_ERROR`, or the empty string when there
/// is no detail (see the module docs). It stays valid until the next `gz*` call
/// records a new error on the handle; the `errnum` value is the authoritative
/// code.
///
/// Matches C by returning `NULL` for a null handle (and does not write
/// `errnum`). A handle that is neither an open reader nor writer likewise
/// yields `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzerror(file: gzFile, errnum: *mut c_int) -> *const c_char {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file, errnum);
        ptr::null()
    }

    #[cfg(feature = "gz-io")]
    {
        guard_const_ptr(ptr::null(), || -> *const c_char {
            if file.is_null() {
                return ptr::null();
            }
            // SAFETY: non-null handle; borrowed immutably.
            let guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &*guard;
            // Only a live reader/writer reports an error (matches C's mode check).
            if state.mode != GzMode::Read && state.mode != GzMode::Write {
                return ptr::null();
            }
            let code = state.err;
            if !errnum.is_null() {
                // SAFETY: `errnum` is a non-null, caller-owned `int` slot.
                unsafe { *errnum = code.as_c_int() };
            }
            // Return the same text the idiomatic `gz::gzerror` reports, but as a
            // stable, NUL-terminated pointer (see the module docs). `Z_MEM_ERROR`
            // deliberately stores no heap message, so synthesise the literal
            // `"out of memory"` without allocating; otherwise hand back a pointer
            // into the handle's owned `msg_c` mirror of `msg` — the specific
            // `"{path}: {detail}"` string, or the empty string when no detail is
            // present. The pointer stays valid until the next `gz*` call records a
            // new error and replaces `msg_c`, exactly matching the C lifetime.
            if code == ReturnCode::MemError {
                c"out of memory".as_ptr()
            } else {
                match &state.msg_c {
                    Some(cs) => cs.as_ptr(),
                    None => c"".as_ptr(),
                }
            }
        })
    }
}

/// `void gzclearerr(gzFile file)` (`ZLIB_1.2.0.2`)
///
/// Clears the error and end-of-file indicators for `file`. A no-op for a null
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzclearerr(file: gzFile) {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
    }

    #[cfg(feature = "gz-io")]
    {
        // Void return: there is no value to substitute, so the canonical
        // `void` guard simply swallows the panic (never unwind into C).
        guard_void(|| {
            if file.is_null() {
                return;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzclearerr(state);
        });
    }
}

/// `int gzclose(gzFile file)`
///
/// Flushes and closes `file` (finishing the gzip stream and freeing buffers for
/// writers), reclaiming the boxed state and closing the descriptor. Returns a
/// zlib return code: `Z_STREAM_ERROR` for a null handle or one in neither
/// direction, `Z_ERRNO` if the platform close failed, otherwise the finalizer's
/// accumulated status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzclose(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        Z_STREAM_ERROR
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(Z_STREAM_ERROR, || {
            if file.is_null() {
                return Z_STREAM_ERROR;
            }
            // SAFETY: `file` was produced by `gzopen*`/`gzdopen` as
            // `Box::into_raw(Box<GzHandle>)` and has not been closed. `take_for_close`
            // inspects the direction through a borrow and reclaims the box exactly
            // once, only when a live direction is present — so a handle it refuses is
            // left valid for the caller, as in C.
            let Some(handle) = (unsafe { take_for_close(file, CloseDirection::Either) }) else {
                return Z_STREAM_ERROR;
            };
            // The idiomatic finalizer performs every C step except `close(fd)` and
            // hands the descriptor back; `finish_close` performs that close and
            // applies C's precedence. The `gzFile_s` prefix drops with the handle.
            let (status, released) = gz::gzclose_release(handle.state);
            finish_close(status, released)
        })
    }
}

/// `int gzclose_r(gzFile file)` (`ZLIB_1.2.3.5`)
///
/// Closes a read stream specifically. Returns a zlib return code:
/// `Z_STREAM_ERROR` for a null handle **or one not opened for reading** (in which
/// case the handle is left untouched and may be closed with [`gzclose_w`], exactly
/// as in C), `Z_ERRNO` if the platform close failed, otherwise `Z_OK` or a
/// preserved `Z_BUF_ERROR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzclose_r(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        Z_STREAM_ERROR
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(Z_STREAM_ERROR, || {
            if file.is_null() {
                return Z_STREAM_ERROR;
            }
            // SAFETY: `file` was produced as `Box::into_raw(Box<GzHandle>)` and has
            // not been closed. The box is reclaimed only if the handle is a reader,
            // mirroring C's `if (state->mode != GZ_READ) return Z_STREAM_ERROR;`
            // *preceding* every `free`/`close`; a writer is therefore refused without
            // being consumed, so no use-after-free or double-free window exists.
            let Some(handle) = (unsafe { take_for_close(file, CloseDirection::Read) }) else {
                return Z_STREAM_ERROR;
            };
            let (status, released) = gz::gzclose_r_release(handle.state);
            finish_close(status, released)
        })
    }
}

/// `int gzclose_w(gzFile file)` (`ZLIB_1.2.3.5`)
///
/// Closes a write stream specifically, emitting the final `Z_FINISH` block and
/// the gzip trailer. Returns a zlib return code: `Z_STREAM_ERROR` for a null
/// handle **or one not opened for writing** (in which case the handle is left
/// untouched and may be closed with [`gzclose_r`], exactly as in C), `Z_ERRNO` if
/// the platform close failed, otherwise the accumulated flush status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzclose_w(file: gzFile) -> c_int {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (file,);
        Z_STREAM_ERROR
    }

    #[cfg(feature = "gz-io")]
    {
        guard_int(Z_STREAM_ERROR, || {
            if file.is_null() {
                return Z_STREAM_ERROR;
            }
            // SAFETY: `file` was produced as `Box::into_raw(Box<GzHandle>)` and has
            // not been closed. The box is reclaimed only if the handle is a writer,
            // mirroring C's `if (state->mode != GZ_WRITE) return Z_STREAM_ERROR;`
            // *preceding* every `free`/`close`; a reader is therefore refused without
            // being consumed, so no use-after-free or double-free window exists.
            let Some(handle) = (unsafe { take_for_close(file, CloseDirection::Write) }) else {
                return Z_STREAM_ERROR;
            };
            let (status, released) = gz::gzclose_w_release(handle.state);
            finish_close(status, released)
        })
    }
}

// ===========================================================================
// Windows wide-character open  (<- gzlib.c: gzopen_w)
// ===========================================================================

/// `gzFile gzopen_w(const wchar_t *path, const char *mode)`  *(Windows)*
///
/// Wide-character (`UTF-16`) path variant of [`gzopen`], present only on
/// Windows to match zlib's `_WIN32` build. The UTF-16 path is decoded via
/// `OsString::from_wide` and forwarded to the idiomatic open. Returns `NULL` on
/// any failure.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gzopen_w(path: *const u16, mode: *const c_char) -> gzFile {
    #[cfg(not(feature = "gz-io"))]
    {
        let _ = (path, mode);
        ptr::null_mut()
    }

    #[cfg(feature = "gz-io")]
    {
        guard_ptr(ptr::null_mut(), || -> gzFile {
            if path.is_null() || mode.is_null() {
                return ptr::null_mut();
            }
            // SAFETY: `path` is a non-null, NUL-terminated wide string; count its
            // code units up to (excluding) the terminator.
            let len = unsafe {
                let mut n = 0usize;
                while *path.add(n) != 0 {
                    n += 1;
                }
                n
            };
            // SAFETY: `path[..len]` are `len` initialized `u16` code units.
            let units = unsafe { slice::from_raw_parts(path, len) };
            use std::os::windows::ffi::OsStringExt;
            let os = std::ffi::OsString::from_wide(units);
            // SAFETY: `mode` is non-null (checked) and NUL-terminated. Bytes, not
            // UTF-8: see the note in `gzopen`.
            let mode_bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
            box_state(gz::gzopen_bytes(std::path::PathBuf::from(os), mode_bytes))
        })
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(feature = "gz-io")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::gz::test_temp::{TempFile, create_new_file};
    use std::ffi::CString;

    /// A hardened temporary `.gz` path (and its `CString` form) for a test.
    ///
    /// The returned [`TempFile`] guard owns an exclusively created, caller-private
    /// directory and removes it — with everything inside — when it drops, including
    /// on the unwinding path taken by a failing assertion. Callers must therefore
    /// keep the guard bound for as long as the path is needed and must **not** call
    /// `remove_file` themselves.
    ///
    /// Tests that reach the file only through `cpath` still bind the guard, as
    /// `let (_path, cpath) = …`. That leading underscore silences the unused-variable
    /// lint **without** dropping the value — unlike `let _ = …`, which would drop the
    /// guard immediately and delete the directory before `gzopen` ever saw it. Do not
    /// "tidy" such a binding away.
    ///
    /// # Why not `temp_dir().join(format!("…_{pid}.gz"))`
    ///
    /// That name is computable by anyone on the host from public information, and
    /// the `gzopen(…, "wb")` this module tests resolves it with `O_TRUNC` and
    /// *without* `O_EXCL` — so a symlink planted at the predicted name redirects
    /// every write to a target of the planter's choosing and truncates it on the way
    /// (CWE-377 insecure temporary file, CWE-59 link following, CWE-367
    /// time-of-check/time-of-use through the `remove_file`-then-open sequences the
    /// old helper relied on). Moving the uniqueness onto an exclusively created
    /// private directory removes all three: see
    /// [`crate::gz::test_temp`] for the full rationale.
    ///
    /// # Panics
    ///
    /// If no private directory can be created, or if the resulting path is not
    /// valid UTF-8 (it cannot be: every component is ASCII by construction).
    fn unique_path(tag: &str) -> (TempFile, CString) {
        let temp = TempFile::new(tag);
        let c = CString::new(temp.path().to_str().expect("temp path is valid UTF-8"))
            .expect("temp path contains no interior NUL");
        (temp, c)
    }

    #[test]
    fn write_then_read_round_trip() {
        let (_path, cpath) = unique_path("rt");
        let data = b"hello, gzip world!\nsecond line with more bytes\n";
        unsafe {
            // Write path: open "wb", write, close_w.
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null(), "gzopen for write returned NULL");
            let n = gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint);
            assert_eq!(n, data.len() as c_int);
            assert_eq!(gzclose_w(wf), Z_OK);

            // Read path: open "rb", read back, verify bytes, verify EOF.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null(), "gzopen for read returned NULL");
            let mut buf = std::vec![0u8; data.len()];
            let got = gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint);
            assert_eq!(got, data.len() as c_int);
            assert_eq!(&buf[..], &data[..]);
            // Reading past the end yields 0 and sets the EOF indicator.
            let mut extra = [0u8; 1];
            assert_eq!(gzread(rf, extra.as_mut_ptr() as voidp, 1), 0);
            assert_eq!(gzeof(rf), 1);
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    #[test]
    fn puts_gets_getc_round_trip() {
        let (_path, cpath) = unique_path("lines");
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            // "first line\n" is 11 bytes; gzputs returns the count written.
            assert_eq!(gzputs(wf, c"first line\n".as_ptr()), 11);
            assert_eq!(gzputc(wf, b'A' as c_int), b'A' as c_int);
            assert_eq!(gzclose_w(wf), Z_OK);

            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            let mut line = [0u8; 64];
            let p = gzgets(rf, line.as_mut_ptr() as *mut c_char, line.len() as c_int);
            assert!(!p.is_null());
            // The returned line includes the newline and is NUL-terminated.
            let s = CStr::from_ptr(p).to_str().unwrap();
            assert_eq!(s, "first line\n");
            // The trailing 'A' written via gzputc follows.
            assert_eq!(gzgetc(rf), b'A' as c_int);
            // At end of file, gzgetc_ returns -1.
            assert_eq!(gzgetc_(rf), -1);
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    #[test]
    fn null_handle_sentinels() {
        unsafe {
            let nul: gzFile = ptr::null_mut();
            let mut b = [0u8; 4];
            // Reads / writes.
            assert_eq!(gzread(nul, b.as_mut_ptr() as voidp, 4), -1);
            assert_eq!(gzwrite(nul, b.as_ptr() as voidpc, 4), 0);
            assert_eq!(gzfread(b.as_mut_ptr() as voidp, 1, 4, nul), 0);
            assert_eq!(gzfwrite(b.as_ptr() as voidpc, 1, 4, nul), 0);
            // Close family.
            assert_eq!(gzclose(nul), Z_STREAM_ERROR);
            assert_eq!(gzclose_r(nul), Z_STREAM_ERROR);
            assert_eq!(gzclose_w(nul), Z_STREAM_ERROR);
            // Status.
            assert_eq!(gzeof(nul), 0);
            assert_eq!(gzdirect(nul), 0);
            // Position.
            assert_eq!(gztell(nul), -1);
            assert_eq!(gztell64(nul), -1);
            assert_eq!(gzoffset(nul), -1);
            assert_eq!(gzoffset64(nul), -1);
            assert_eq!(gzseek(nul, 0, 0), -1);
            assert_eq!(gzseek64(nul, 0, 0), -1);
            assert_eq!(gzrewind(nul), -1);
            // Byte I/O.
            assert_eq!(gzgetc(nul), -1);
            assert_eq!(gzgetc_(nul), -1);
            assert_eq!(gzputc(nul, b'x' as c_int), -1);
            assert_eq!(gzungetc(b'x' as c_int, nul), -1);
            assert_eq!(gzputs(nul, c"x".as_ptr()), -1);
            assert_eq!(gzflush(nul, 0), Z_STREAM_ERROR);
            // Config.
            assert_eq!(gzbuffer(nul, 8192), -1);
            assert_eq!(gzsetparams(nul, 6, 0), Z_STREAM_ERROR);
            // Formatted write (the documented `NO_vsnprintf && !ZLIB_INSECURE`
            // zlib variant): both variadic entry points return Z_STREAM_ERROR
            // unconditionally, matching `zlibCompileFlags` bit 27.
            //
            // A NULL handle alone cannot distinguish "always fails" from "fails
            // only on a bad handle", so the *unconditional* half of that
            // contract is pinned separately, on a live writable handle, by
            // `variadic_printf_stubs_fail_on_a_valid_write_handle` below.
            assert_eq!(gzprintf(nul, c"%d".as_ptr()), Z_STREAM_ERROR);
            assert_eq!(
                gzvprintf(nul, c"%d".as_ptr(), ptr::null_mut()),
                Z_STREAM_ERROR
            );
            // Pointer-returning.
            assert!(gzgets(nul, b.as_mut_ptr() as *mut c_char, 4).is_null());
            assert!(gzerror(nul, ptr::null_mut()).is_null());
            // Void: must not panic or crash.
            gzclearerr(nul);
        }
    }

    /// The `NO_vsnprintf && !ZLIB_INSECURE` variant contract (AAP §0.8.2
    /// Divergence 1) pinned on a **valid, writable** handle.
    ///
    /// [`gzprintf`] / [`gzvprintf`] are ABI-compatible stubs that return
    /// `Z_STREAM_ERROR` *unconditionally* — never conditionally on the handle —
    /// and the crate advertises exactly that variant through `zlibCompileFlags`
    /// bit 27. `null_handle_sentinels` observes them only through a NULL handle,
    /// which any null check satisfies and which therefore leaves the
    /// unconditional half of the contract unproven: a hypothetical
    /// partially-functional implementation would pass that assertion.
    ///
    /// This test verifies the unconditional stub contract on a live handle. It
    /// drives both raw exports on an open `"wb"` handle with a format string
    /// containing **no conversion specifier** — the
    /// easiest possible input, which a functional implementation would copy
    /// verbatim and report a length for — and pins the whole contract:
    ///
    /// * each call returns `Z_STREAM_ERROR` on the valid handle;
    /// * the advertised flag (bit 27) and the observed behavior agree;
    /// * the stubs are inert — nothing is staged (`gztell` stays `0`) and the
    ///   handle's error state is untouched;
    /// * the handle survives and finalizes cleanly (`gzclose_w == Z_OK`);
    /// * the finished member independently decodes to an **empty** payload, so
    ///   no byte reached the file either.
    ///
    /// Its companion, [`idiomatic_printf_renders_through_the_same_handle`],
    /// proves the divergence is confined to the raw C-variadic ABI.
    #[test]
    fn variadic_printf_stubs_fail_on_a_valid_write_handle() {
        let (_path, cpath) = unique_path("printfstub");

        // The behavior asserted below is the one the flags word advertises;
        // assert them together so the pair can never drift apart silently.
        assert_ne!(
            crate::util::zlib_compile_flags() & (1 << 27),
            0,
            "compile flags must advertise the gzprintf-returns-error variant"
        );

        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null(), "gzopen for write returned NULL");

            // Conversion-free format: a functional gzprintf would render these
            // 10 bytes verbatim and return 10.
            let fmt = c"plain text";

            assert_eq!(
                gzprintf(wf, fmt.as_ptr()),
                Z_STREAM_ERROR,
                "gzprintf must fail on a VALID handle, not only on NULL"
            );
            assert_eq!(
                gzvprintf(wf, fmt.as_ptr(), ptr::null_mut()),
                Z_STREAM_ERROR,
                "gzvprintf must fail on a VALID handle, not only on NULL"
            );

            // Inert: no byte staged, position untouched.
            assert_eq!(gztell(wf), 0, "the stubs must not stage any byte");

            // Inert: the handle's error state is untouched. The stubs report
            // through the return value only, so a subsequent `gzerror` must
            // still read as "no error" — matching a C build whose `gzprintf`
            // returns the error without recording it on the file.
            let mut errnum: c_int = Z_STREAM_ERROR;
            let msg = gzerror(wf, &raw mut errnum);
            assert_eq!(errnum, Z_OK, "the stubs must not poison the handle");
            assert!(!msg.is_null(), "gzerror must return a valid C string");
            assert!(
                CStr::from_ptr(msg).to_bytes().is_empty(),
                "no error message may be recorded"
            );

            // The handle is still fully usable and finalizes normally.
            assert_eq!(
                gzclose_w(wf),
                Z_OK,
                "the handle must survive the rejected stub calls"
            );

            // Independent decode: a valid but EMPTY gzip member.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null(), "the finished member must be readable");
            let mut buf = [0u8; 32];
            assert_eq!(
                gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                0,
                "the decoded payload must be empty — nothing reached the file"
            );
            assert_eq!(gzeof(rf), 1);
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    /// The control for [`variadic_printf_stubs_fail_on_a_valid_write_handle`]:
    /// the stub contract is confined to the **raw C-variadic ABI** entry points.
    ///
    /// Rust callers lose nothing — [`crate::gz::gzprintf`] and
    /// [`crate::gz::gzvprintf`] render [`core::fmt::Arguments`] in full. Driving
    /// the idiomatic layer through the *same* `gzFile` the raw export just
    /// rejected proves the split is a deliberate, ABI-scoped divergence rather
    /// than "formatted writes do not work in this crate".
    #[test]
    fn idiomatic_printf_renders_through_the_same_handle() {
        let (_path, cpath) = unique_path("printfctl");
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null(), "gzopen for write returned NULL");

            // Raw C-variadic entry point on this handle: rejected.
            assert_eq!(gzprintf(wf, c"%d".as_ptr()), Z_STREAM_ERROR);

            // Same handle, idiomatic entry points: both render and report the
            // formatted byte count.
            {
                // SAFETY: `wf` is a live, non-null handle from `gzopen` that has
                // not been closed, so borrowing its `GzHandle` is valid.
                let mut guard = GzBorrow::new(gz_handle(wf));
                assert_eq!(
                    gz::gzprintf(&mut guard, format_args!("{}+{}={}", 2, 3, 5)),
                    5,
                    "the idiomatic gzprintf renders and reports its length"
                );
                assert_eq!(
                    gz::gzvprintf(&mut guard, format_args!(" ok")),
                    3,
                    "the idiomatic gzvprintf renders and reports its length"
                );
            }

            // The rendered bytes are staged (unlike the stub calls above).
            assert_eq!(gztell(wf), 8, "8 rendered bytes are staged");
            assert_eq!(gzclose_w(wf), Z_OK);

            // The member decodes to exactly the idiomatically rendered text.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            let mut buf = [0u8; 32];
            let got = gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint);
            assert_eq!(got, 8);
            assert_eq!(&buf[..8], b"2+3=5 ok");
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    /// The two `gz`-local panic guards behave exactly like their
    /// [`crate::ffi::types`] siblings.
    ///
    /// [`guard_size`] and [`guard_const_ptr`] exist because the shared guard set
    /// has no [`z_size_t`] or `*const T` variant, and they are the last line of
    /// defense for `gzfread` / `gzfwrite` (whose C contract returns a plain
    /// `z_size_t` with no error sentinel, so the substituted default *is* what
    /// the C caller observes) and for `gzerror` (whose sentinel is `NULL`).
    /// Being separate implementations, they are exercised directly rather than
    /// by analogy with the shared guards.
    #[test]
    fn local_panic_guards_pass_values_through_and_substitute_defaults() {
        // Pass-through, with defaults deliberately different from the results.
        assert_eq!(guard_size(0, || 41), 41);
        assert_eq!(guard_size(1, || z_size_t::MAX), z_size_t::MAX);

        let bytes: [u8; 2] = [3, 4];
        let live: *const u8 = bytes.as_ptr();
        assert_eq!(guard_const_ptr(ptr::null(), || live), live);

        // Panic substitutes the caller's default. The shared helper silences the
        // default hook (so the deliberate panics print no backtrace) and
        // serializes the process-global hook swap against the sibling
        // `ffi::types` guard tests, which run concurrently in this same binary.
        let (sized, zeroed, nulled, fallback) = crate::ffi::types::with_silenced_panic_hook(|| {
            (
                guard_size(7, || panic!("boundary panic")),
                guard_size(0, || panic!("boundary panic")),
                guard_const_ptr(ptr::null::<u8>(), || panic!("boundary panic")),
                guard_const_ptr(live, || panic!("boundary panic")),
            )
        });

        assert_eq!(sized, 7, "gzfread/gzfwrite observe the caller's default");
        assert_eq!(
            zeroed, 0,
            "a zero default is substituted just as faithfully"
        );
        assert!(nulled.is_null(), "gzerror's sentinel is NULL");
        assert_eq!(
            fallback, live,
            "the guard returns the caller's default, not a hard-coded NULL"
        );
        assert_eq!(bytes, [3, 4], "the pointee is untouched");
    }

    #[test]
    fn gzbuffer_before_and_after_io() {
        let (_path, cpath) = unique_path("buf");
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            // Before any I/O: accepted.
            assert_eq!(gzbuffer(wf, 16384), 0);
            // Trigger I/O.
            let d = b"x";
            assert_eq!(gzwrite(wf, d.as_ptr() as voidpc, 1), 1);
            // After I/O has begun: rejected with -1.
            assert_eq!(gzbuffer(wf, 8192), -1);
            assert_eq!(gzclose_w(wf), Z_OK);
        }
    }

    #[cfg(unix)]
    #[test]
    fn gzdopen_fd_round_trip() {
        use std::os::fd::IntoRawFd;
        let (path, _cpath) = unique_path("dopen");
        let data = b"descriptor round trip\n";
        unsafe {
            // Write via a descriptor handed to gzdopen.
            // Exclusive creation: no symlink at this name is followed and
            // nothing is truncated (the directory did not exist a moment ago).
            let fd = path.create().into_raw_fd();
            let wf = gzdopen(fd, c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(
                gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(gzclose_w(wf), Z_OK);

            // Read back via a fresh descriptor.
            let file = std::fs::File::open(&path).unwrap();
            let fd = file.into_raw_fd();
            let rf = gzdopen(fd, c"rb".as_ptr());
            assert!(!rf.is_null());
            let mut buf = std::vec![0u8; data.len()];
            assert_eq!(
                gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(&buf[..], &data[..]);
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    /// The live `gzFile_s` prefix lets the C `gzgetc(g)` *macro* consume
    /// buffered bytes directly through the handle pointer, and the next real
    /// `gz*` call reconciles that consumption into the idiomatic cursor.
    ///
    /// This exercises the [`GzHandle`] prefix / [`GzBorrow`] reconcile-sync
    /// bridge end to end: a real read populates the prefix, we mutate it exactly
    /// as the C macro does (`have--, pos++, *next++`), then assert that a later
    /// library call absorbs the delta and continues byte-exactly.
    #[test]
    fn gzgetc_macro_prefix_reconcile() {
        let (_path, cpath) = unique_path("c2prefix");
        let data = b"ABCDEFGHIJ";
        unsafe {
            // Write known bytes and close.
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(
                gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(gzclose_w(wf), Z_OK);

            // Open for read; the handle's prefix starts cleared (`have == 0`).
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());

            // First real read returns 'A' and, on shim exit, syncs the prefix so
            // the macro fast-path can take over the buffered remainder.
            assert_eq!(gzgetc(rf), b'A' as c_int);

            // Inspect the live `gzFile_s` prefix exactly as the C macro would.
            let handle = &mut *(rf as *mut GzHandle);
            assert!(
                handle.prefix.have >= 1,
                "prefix must expose buffered bytes after the first read"
            );
            assert!(!handle.prefix.next.is_null(), "prefix.next must be live");
            assert_eq!(handle.prefix.pos, 1, "pos reflects the one byte read");

            // Simulate the macro consuming ONE byte straight from the prefix:
            //   ((g)->have ? ((g)->have--, (g)->pos++, *((g)->next)++) : ...)
            let macro_byte = *handle.prefix.next;
            assert_eq!(macro_byte, b'B', "macro reads the 2nd byte directly");
            handle.prefix.have -= 1;
            handle.prefix.next = handle.prefix.next.add(1);
            handle.prefix.pos += 1;

            // A subsequent library call reconciles the macro-side consumption:
            // gztell reports the reconciled position (2 bytes consumed total).
            assert_eq!(gztell(rf), 2, "gztell reconciles the macro-consumed byte");

            // Reading continues byte-exactly from the 3rd byte onward.
            assert_eq!(gzgetc(rf), b'C' as c_int);
            assert_eq!(gzgetc(rf), b'D' as c_int);

            let mut rest = [0u8; 6];
            assert_eq!(
                gzread(rf, rest.as_mut_ptr() as voidp, rest.len() as c_uint),
                6
            );
            assert_eq!(&rest, b"EFGHIJ");
            assert_eq!(gztell(rf), data.len() as z_off_t);
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    // =======================================================================
    // Descriptor lifecycle: direction validation, `gzdopen` ownership, and the
    // fallible platform close.
    //
    // Every expectation below was captured from reference zlib built from this
    // repository's own C sources; see the `gzlife` differential harness. The
    // three contracts pinned here are:
    //
    //   1. a wrong-direction close frees nothing and returns Z_STREAM_ERROR, so
    //      the caller's handle stays live and closable  (`gzread.c` L650-L651,
    //      `gzwrite.c` L677-L678);
    //   2. no `gzdopen` failure closes the caller's descriptor
    //      (`gzlib.c` L150-L197 and L206-L210 precede L263); and
    //   3. a failing `close(2)` surfaces as Z_ERRNO
    //      (`gzread.c` L665-L667, `gzwrite.c` L695-L696).
    // =======================================================================

    /// Decompresses `path` with the independent `flate2`/`miniz_oxide` decoder,
    /// proving the gzip member is complete and well-formed.
    fn decode_gzip_file(path: &std::path::Path) -> std::vec::Vec<u8> {
        use std::io::Read as _;
        let f = std::fs::File::open(path).expect("open for verification");
        let mut out = std::vec::Vec::new();
        flate2::read::GzDecoder::new(f)
            .read_to_end(&mut out)
            .expect("stream decodes as gzip");
        out
    }

    /// A wrong-direction close is a **pure no-op**: it reports `Z_STREAM_ERROR`,
    /// frees nothing, and leaves the handle fully usable — so the caller can keep
    /// working and then close it with the correct finalizer.
    ///
    /// This is the regression test for the use-after-free / double-free defect in
    /// which the owning `Box<GzHandle>` was reconstructed *before* the direction
    /// was validated (CWE-416, CWE-415). Under that ordering the first refused
    /// call already deallocated the handle, so this test's continued use of it
    /// was a use-after-free and the final correct close a double free —
    /// reproduced against the C harness as an immediate
    /// `free(): double free detected in tcache 2` abort.
    ///
    /// Mirrors reference-zlib harness cases `1a`, `1b`, `1c`, and `1d`.
    #[test]
    fn wrong_direction_close_is_a_no_op_and_leaves_the_handle_usable() {
        let (path, cpath) = unique_path("wrongdir");
        unsafe {
            // ---- writer refuses gzclose_r, three times, then closes cleanly.
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            let head = b"payload";
            assert_eq!(
                gzwrite(wf, head.as_ptr() as voidpc, head.len() as c_uint),
                head.len() as c_int
            );

            // C `1d`: repeated wrong-direction closes each return Z_STREAM_ERROR.
            for attempt in 0..3 {
                assert_eq!(
                    gzclose_r(wf),
                    Z_STREAM_ERROR,
                    "gzclose_r on a writer must be refused (attempt {attempt})"
                );
            }

            // The refusal records nothing on the handle: C returns before it can
            // reach `gz_error`, so `gzerror` must still answer `Z_OK` with the
            // empty message a freshly opened handle carries.
            let mut errnum: c_int = Z_STREAM_ERROR;
            let msg = gzerror(wf, &raw mut errnum);
            assert_eq!(errnum, Z_OK, "a rejected close must not poison the handle");
            assert!(!msg.is_null() && CStr::from_ptr(msg).to_bytes().is_empty());

            // The handle survived every refusal: it still reports its position and
            // still accepts writes.
            assert_eq!(gztell(wf), head.len() as z_off_t);
            let tail = b" and more";
            assert_eq!(
                gzwrite(wf, tail.as_ptr() as voidpc, tail.len() as c_uint),
                tail.len() as c_int
            );

            // C `1d`: the correct finalizer then succeeds — exactly one teardown.
            assert_eq!(gzclose_w(wf), Z_OK);
        }

        // C `1c`: everything written before and after the refusals is present and
        // the member is properly finalized.
        assert_eq!(decode_gzip_file(&path), b"payload and more".to_vec());

        unsafe {
            // ---- reader refuses gzclose_w, three times, then closes cleanly.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());

            let mut first = [0u8; 7];
            assert_eq!(
                gzread(rf, first.as_mut_ptr() as voidp, first.len() as c_uint),
                7
            );
            assert_eq!(&first, b"payload");

            for attempt in 0..3 {
                assert_eq!(
                    gzclose_w(rf),
                    Z_STREAM_ERROR,
                    "gzclose_w on a reader must be refused (attempt {attempt})"
                );
            }

            // Same on the read side: the refusal left the error state pristine.
            let mut errnum: c_int = Z_STREAM_ERROR;
            let msg = gzerror(rf, &raw mut errnum);
            assert_eq!(errnum, Z_OK, "a rejected close must not poison the handle");
            assert!(!msg.is_null() && CStr::from_ptr(msg).to_bytes().is_empty());

            // Every status accessor still works on the surviving handle.
            assert_eq!(gzeof(rf), 0);
            assert_eq!(gzdirect(rf), 0);

            // Reading resumes exactly where it left off — nothing was torn down.
            let mut rest = [0u8; 9];
            assert_eq!(
                gzread(rf, rest.as_mut_ptr() as voidp, rest.len() as c_uint),
                9
            );
            assert_eq!(&rest, b" and more");
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    /// A refused close must leave the live `gzFile_s` prefix **bit-for-bit**
    /// untouched.
    ///
    /// C mutates nothing on a wrong-direction close, so the `gzgetc` macro
    /// fast-path must still be armed afterwards. The direction is therefore read
    /// through a plain borrow rather than a [`GzBorrow`] guard, whose
    /// [`reconcile`](GzHandle::reconcile) would clear `have`/`next` as a side
    /// effect.
    #[test]
    fn refused_close_leaves_the_gzgetc_prefix_untouched() {
        let (_path, cpath) = unique_path("refuseprefix");
        let data = b"ABCDEFGHIJ";
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(
                gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(gzclose_w(wf), Z_OK);

            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());

            // One real read populates the prefix for the macro fast path.
            assert_eq!(gzgetc(rf), b'A' as c_int);
            let before = {
                let handle = &*(rf as *const GzHandle);
                assert!(handle.prefix.have >= 1, "prefix must be armed");
                (handle.prefix.have, handle.prefix.next, handle.prefix.pos)
            };

            // A refused close must not touch the prefix.
            assert_eq!(gzclose_w(rf), Z_STREAM_ERROR);
            let after = {
                let handle = &*(rf as *const GzHandle);
                (handle.prefix.have, handle.prefix.next, handle.prefix.pos)
            };
            assert_eq!(before, after, "a refused close must not mutate the prefix");

            // And the macro fast path still works, byte-exactly.
            let handle = &mut *(rf as *mut GzHandle);
            let macro_byte = *handle.prefix.next;
            assert_eq!(macro_byte, b'B');
            handle.prefix.have -= 1;
            handle.prefix.next = handle.prefix.next.add(1);
            handle.prefix.pos += 1;

            assert_eq!(gztell(rf), 2);
            assert_eq!(gzgetc(rf), b'C' as c_int);
            assert_eq!(gzclose_r(rf), Z_OK);
        }
    }

    /// `gzclose` accepts either direction, dispatching as C's
    /// `state->mode == GZ_READ ? gzclose_r(file) : gzclose_w(file)` does.
    #[test]
    fn gzclose_dispatches_on_either_direction() {
        let (path, cpath) = unique_path("dispatch");
        let data = b"either direction";
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(
                gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(gzclose(wf), Z_OK, "gzclose finalizes a writer");

            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            let mut buf = std::vec![0u8; data.len()];
            assert_eq!(
                gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(gzclose(rf), Z_OK, "gzclose closes a reader");
        }
        assert_eq!(decode_gzip_file(&path), data.to_vec());
    }

    /// Probes whether `fd` is still an open descriptor, without taking ownership
    /// of it — the `fstat`-based analogue of the C harness's
    /// `fcntl(fd, F_GETFD) != -1`.
    #[cfg(unix)]
    fn fd_is_alive(fd: c_int) -> bool {
        // SAFETY: wrapping the descriptor in a `ManuallyDrop<File>` gives a
        // borrow-only view: `metadata()` issues an `fstat` and the `File` is never
        // dropped, so the descriptor is neither closed nor otherwise disturbed. A
        // closed or never-valid descriptor answers `EBADF`.
        let probe = core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
        probe.metadata().is_ok()
    }

    /// A failed `gzdopen` must leave the caller's descriptor **open**.
    ///
    /// C validates the entire mode grammar before it stores the descriptor in
    /// `state->fd` (`gzlib.c` L150-L197 precede L263), so a rejected `gzdopen`
    /// never closes `fd` and the caller may retry or close it itself. Adopting the
    /// descriptor into a [`std::fs::File`] before validating would close it on the
    /// failure path, silently turning the caller's own later `close(fd)` into a
    /// double close (CWE-672).
    ///
    /// Mirrors reference-zlib harness case `2`, mode for mode.
    #[cfg(unix)]
    #[test]
    fn failed_gzdopen_leaves_the_callers_descriptor_open() {
        use std::os::fd::IntoRawFd;

        // Every mode reference zlib rejects: read+write, forced-transparent read,
        // `G` while writing, and three strings with no r/w/a at all.
        let rejected: [&core::ffi::CStr; 6] = [c"r+", c"rT", c"wG", c"b", c"", c"9"];

        let (path, _cpath) = unique_path("dopenfail");
        // Seed exclusively: `std::fs::write` truncates and follows a link, which
        // the guard's private directory makes moot but which must not be relied on.
        {
            use std::io::Write as _;
            let mut seed = path.create();
            seed.write_all(b"seed").expect("seed the fixture");
        }

        for mode in rejected {
            let fd = std::fs::File::open(&path).unwrap().into_raw_fd();
            assert!(fd_is_alive(fd), "descriptor must start open");

            // SAFETY: `fd` is a live descriptor and `mode` is a NUL-terminated
            // C string literal.
            let gz = unsafe { gzdopen(fd, mode.as_ptr()) };
            assert!(
                gz.is_null(),
                "gzdopen must reject mode {mode:?} exactly as C does"
            );
            assert!(
                fd_is_alive(fd),
                "a rejected gzdopen must NOT close the caller's descriptor (mode {mode:?})"
            );

            // The caller still owns it, so the caller closes it.
            // SAFETY: `fd` is still open and unowned by any Rust value.
            drop(unsafe { std::fs::File::from_raw_fd(fd) });
        }

        // The `-1` sentinel is the ONE descriptor C rejects outright
        // (`gzlib.c` L300 `if (fd == -1 || ...)`). Every other value is adopted
        // and its errors deferred — see
        // `gzdopen_rejects_only_the_minus_one_sentinel`.
        // SAFETY: no descriptor is dereferenced on this path.
        assert!(unsafe { gzdopen(-1, c"rb".as_ptr()) }.is_null());
    }

    /// `gzdopen` rejects **exactly one** descriptor value: the `-1` sentinel.
    ///
    /// `zlib.h` L1422-L1426 is explicit — `gzdopen` returns `NULL` "if fd is -1",
    /// and "the file descriptor is not used until the next gz\* read, write, seek,
    /// or close operation, so gzdopen will not detect if fd is invalid (unless fd
    /// is -1)". C implements precisely that: one `fd == -1` test (`gzlib.c` L300)
    /// and no other descriptor validation at all.
    ///
    /// So a descriptor that is negative-but-not-`-1`, or simply not open, must
    /// **open successfully** and surface [`Z_ERRNO`](crate::error::ReturnCode::ErrNo) at the *first* operation. The
    /// values asserted here are the ones measured against reference zlib: right
    /// after the open `gzerror` reports no error at all, the first read returns
    /// `-1` with `errnum == Z_ERRNO` and a non-empty message, and the close
    /// likewise reports `Z_ERRNO` because closing that descriptor fails.
    ///
    /// A guard of `fd < 0` — which is what a naive port writes — would refuse a
    /// handle C hands back, and in doing so would deny the caller the very handle
    /// it needs to read the deferred error from.
    #[test]
    fn gzdopen_rejects_only_the_minus_one_sentinel() {
        // Only `-1`.
        // SAFETY: no descriptor is dereferenced when the sentinel is rejected.
        assert!(
            unsafe { gzdopen(-1, c"rb".as_ptr()) }.is_null(),
            "gzlib.c L300: fd == -1 is rejected outright"
        );

        // `-5` and a large unallocated descriptor are both adopted, and both defer.
        //
        // Asserted on Unix only. The *implementation* is platform-generic and
        // faithful on both — `adopt_descriptor` adopts whatever the platform
        // reports, exactly as C stores whatever `int` it was handed — but on
        // Windows what an out-of-range CRT descriptor *does* is decided by the
        // CRT's invalid-parameter handler, which `_get_osfhandle` invokes and
        // whose default action is CRT-version-specific. Reference zlib has the
        // identical exposure there (its `_read` validates the descriptor through
        // the same handler), so matching C means inheriting it; what would be
        // wrong is to pin a specific outcome to it in an assertion.
        #[cfg(unix)]
        for fd in [-5 as c_int, 9999 as c_int] {
            // SAFETY: `fd` is not the `-1` sentinel and `mode` is a NUL-terminated
            // literal. Adopting an invalid descriptor is the documented C contract;
            // the handle is closed below.
            let gz = unsafe { gzdopen(fd, c"rb".as_ptr()) };
            assert!(
                !gz.is_null(),
                "gzdopen must ADOPT fd {fd} and defer the error, as C does"
            );

            // Immediately after the open, C has reported nothing.
            let mut errnum: c_int = 12345;
            // SAFETY: `gz` is a live handle from `gzdopen`; `errnum` is a valid
            // out-parameter.
            let msg = unsafe { gzerror(gz, &raw mut errnum) };
            assert_eq!(errnum, Z_OK, "a fresh gzdopen handle carries no error");
            assert!(
                !msg.is_null(),
                "gzerror never returns NULL for a live handle"
            );
            // SAFETY: `msg` is a non-null NUL-terminated string owned by the handle.
            assert!(
                unsafe { CStr::from_ptr(msg) }.to_bytes().is_empty(),
                "the message is empty until an operation fails"
            );

            // The first read is where the invalid descriptor finally shows up.
            let mut buf = [0u8; 8];
            // SAFETY: `gz` is live and `buf` is valid for `buf.len()` bytes.
            let got = unsafe { gzread(gz, buf.as_mut_ptr() as voidp, buf.len() as c_uint) };
            assert_eq!(got, -1, "the deferred read must fail");
            // SAFETY: as above.
            let msg = unsafe { gzerror(gz, &raw mut errnum) };
            assert_eq!(errnum, Z_ERRNO, "the deferred failure is an OS error");
            // SAFETY: as above.
            assert!(
                !unsafe { CStr::from_ptr(msg) }.to_bytes().is_empty(),
                "a failed read must leave a message"
            );

            // Closing that descriptor fails too, so the close reports Z_ERRNO.
            // SAFETY: `gz` is live and is consumed here.
            assert_eq!(
                unsafe { gzclose_r(gz) },
                Z_ERRNO,
                "closing an invalid descriptor reports Z_ERRNO"
            );
        }
    }

    /// The mode string is parsed as raw **bytes**, so a mode that is not valid
    /// UTF-8 opens exactly what its recognised bytes describe.
    ///
    /// C walks the mode one byte at a time and its `switch` ends in
    /// `default: /* could consider as an error, but just ignore */ ;`
    /// (`gzlib.c` L113-L170), so `"rb\xff"` is `"rb"` with one ignored byte.
    /// Requiring UTF-8 at the shim would reject a mode reference zlib accepts —
    /// measured as six divergent probe lines before this fix.
    #[test]
    fn a_non_utf8_mode_opens_exactly_what_its_recognised_bytes_describe() {
        let (path, cpath) = unique_path("modebytes");
        let payload = b"non-utf8 mode payload\n";

        // Three modes with a byte that is not valid UTF-8 in any position, plus a
        // level digit after one of them to prove parsing continues past the byte.
        let write_mode = CString::new(std::vec![b'w', b'b', b'9', 0xfe]).unwrap();
        let read_modes = [
            CString::new(std::vec![b'r', b'b', 0xff]).unwrap(),
            CString::new(std::vec![0xff, b'r', b'b']).unwrap(),
            CString::new(std::vec![b'r', 0x80, b'b']).unwrap(),
        ];

        unsafe {
            // `wb` + '9' + 0xFE — the digit must still be seen as the level.
            let wf = gzopen(cpath.as_ptr(), write_mode.as_ptr());
            assert!(!wf.is_null(), "a non-UTF-8 write mode must still open");
            assert_eq!(
                gzwrite(wf, payload.as_ptr() as voidpc, payload.len() as c_uint),
                payload.len() as c_int
            );
            assert_eq!(gzclose_w(wf), Z_OK);

            for mode in &read_modes {
                let rf = gzopen(cpath.as_ptr(), mode.as_ptr());
                assert!(
                    !rf.is_null(),
                    "a non-UTF-8 read mode must open exactly as C does"
                );
                let mut buf = std::vec![0u8; payload.len()];
                assert_eq!(
                    gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                    payload.len() as c_int
                );
                assert_eq!(&buf[..], &payload[..]);
                assert_eq!(gzclose_r(rf), Z_OK);
            }

            // The same must hold through `gzdopen`, which shares the parser.
            // Releasing a descriptor to hand to `gzdopen` is a Unix-only step.
            #[cfg(unix)]
            {
                use std::os::fd::IntoRawFd;

                let fd = std::fs::File::open(&path).unwrap().into_raw_fd();
                let df = gzdopen(fd, read_modes[0].as_ptr());
                assert!(
                    !df.is_null(),
                    "gzdopen must accept a non-UTF-8 mode too (fd is adopted)"
                );
                let mut buf = std::vec![0u8; payload.len()];
                assert_eq!(
                    gzread(df, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                    payload.len() as c_int
                );
                assert_eq!(&buf[..], &payload[..]);
                assert_eq!(gzclose_r(df), Z_OK);
            }
        }

        let _ = std::fs::remove_file(&path);
    }

    /// `gzread(file, NULL, 0)` returns `0`, not `-1`.
    ///
    /// C validates the handle, the direction and the error ladder, calls
    /// `gz_error(state, Z_OK, NULL)`, checks `(int)len < 0`, and then enters
    /// `gz_read`, whose first statement is
    /// `if (len == 0) return 0;` (`gzread.c` L321-L322). **`buf` is never
    /// inspected on that path.** A blanket null-`buf` rejection therefore answers
    /// `-1` where C answers `0`.
    ///
    /// A null `buf` with a *non-zero* length is genuine undefined behaviour in C
    /// (reference zlib segfaults there), so this port's `-1` for that case is a
    /// deliberate safe superset and is asserted here to stay that way.
    #[test]
    fn a_zero_length_read_ignores_the_buffer_pointer_entirely() {
        let (path, cpath) = unique_path("zeroread");
        let payload = b"zero-length reads must not disturb the stream\n";

        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(
                gzwrite(wf, payload.as_ptr() as voidpc, payload.len() as c_uint),
                payload.len() as c_int
            );
            assert_eq!(gzclose_w(wf), Z_OK);

            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());

            // The finding, twice over: a null buffer with a zero length is a no-op
            // that succeeds, and so is a real buffer with a zero length.
            assert_eq!(
                gzread(rf, ptr::null_mut(), 0),
                0,
                "gzread.c L321-L322: len == 0 returns 0 before `buf` is looked at"
            );
            let mut buf = std::vec![0u8; payload.len()];
            assert_eq!(gzread(rf, buf.as_mut_ptr() as voidp, 0), 0);

            // Neither call reported an error, and neither consumed anything: the
            // full payload is still there.
            let mut errnum: c_int = 12345;
            let _ = gzerror(rf, &raw mut errnum);
            assert_eq!(errnum, Z_OK, "a zero-length read is not an error");
            assert_eq!(
                gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                payload.len() as c_int,
                "a zero-length read must not consume the stream"
            );
            assert_eq!(&buf[..], &payload[..]);
            assert_eq!(gzclose_r(rf), Z_OK);

            // A null HANDLE is still rejected, zero length or not.
            assert_eq!(gzread(ptr::null_mut(), ptr::null_mut(), 0), -1);
            // A null buffer with a NON-zero length stays rejected: C is UB there,
            // and refusing is the safe superset.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            assert_eq!(
                gzread(rf, ptr::null_mut(), 4),
                -1,
                "null buf + nonzero len is UB in C; this port refuses it"
            );
            assert_eq!(gzclose_r(rf), Z_OK);
        }

        let _ = std::fs::remove_file(&path);
    }

    /// The narrowing seek/tell/offset entry points refuse a value their `z_off_t`
    /// cannot represent, rather than truncating it.
    ///
    /// All three C wrappers are the same three lines — narrow, widen back, accept
    /// only if the round trip survived:
    ///
    /// ```c
    /// z_off64_t ret = gzseek64(file, (z_off64_t)offset, whence);
    /// return ret == (z_off_t)ret ? (z_off_t)ret : -1;
    /// ```
    ///
    /// (`gzlib.c` L437-L442, L459-L464, L486-L491.) A bare `as z_off_t` cast
    /// truncates instead: measured on `i686-unknown-linux-gnu`, after
    /// `gzseek64(f, 1 << 40)` reference C answers `gztell() == -1` while the
    /// truncating port answered `0` — a plausible offset, wrong by a terabyte,
    /// that a caller checking for `-1` accepts.
    ///
    /// The boundary assertion below is width-conditional by construction: where
    /// `z_off_t` is as wide as `z_off64_t` no value is unrepresentable, and the
    /// `checked_add` yields `None`. The behaviour on a narrow target is proven by
    /// the 32-bit acceptance probe; what is asserted unconditionally here is that
    /// every representable value round-trips unchanged, and — structurally, below
    /// — that all three entry points actually route through the helper.
    #[test]
    fn a_narrowed_offset_that_cannot_round_trip_becomes_minus_one() {
        for representable in [
            0 as z_off64_t,
            1,
            -1,
            10,
            z_off64_t::from(z_off_t::MAX),
            z_off64_t::from(z_off_t::MIN),
        ] {
            assert_eq!(
                narrow_off(representable),
                representable as z_off_t,
                "a representable offset must survive unchanged"
            );
        }

        if let Some(beyond) = z_off64_t::from(z_off_t::MAX).checked_add(1) {
            assert_eq!(
                narrow_off(beyond),
                -1,
                "gzlib.c L437-L442: a value the narrow type cannot hold becomes -1"
            );
            assert_eq!(narrow_off(z_off64_t::MAX), -1);
        }
        if let Some(below) = z_off64_t::from(z_off_t::MIN).checked_sub(1) {
            assert_eq!(narrow_off(below), -1);
            assert_eq!(narrow_off(z_off64_t::MIN), -1);
        }
    }

    /// Structural companion to
    /// `a_narrowed_offset_that_cannot_round_trip_becomes_minus_one`: each of the
    /// three narrowing entry points must route its wide result through
    /// [`narrow_off`], and none may cast a guard result straight to `z_off_t`.
    ///
    /// The value test above cannot fail on a 64-bit host, where `z_off_t` is
    /// already 64 bits wide and nothing is unrepresentable. This assertion has
    /// teeth on every target: reintroducing the truncating `as z_off_t` cast fails
    /// it here, immediately, on the development host.
    #[test]
    fn every_narrowing_offset_entry_point_routes_through_narrow_off() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ffi/gz.rs");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()));

        // Consider the SHIPPED code only. The assertions below name the very
        // fragments they look for, so the test module has to be excised or every
        // needle would match its own spelling here.
        let module_marker = "\nmod tests {";
        let shipped = match text.find(module_marker) {
            Some(at) => &text[..at],
            None => panic!("the test module marker must be present"),
        };
        // Drop line comments so prose about the old cast cannot satisfy or defeat
        // the assertions either.
        let code = shipped
            .lines()
            .map(|line| match line.find("//") {
                Some(at) => &line[..at],
                None => line,
            })
            .collect::<std::vec::Vec<_>>()
            .join("\n");

        // The helper exists exactly once.
        let definition = std::format!("fn {}(", "narrow_off");
        assert_eq!(
            code.matches(&definition).count(),
            1,
            "narrow_off must be defined exactly once"
        );

        // It is applied exactly three times — once per narrowing entry point.
        let application = std::format!("{}({}(", "narrow_off", "guard_off");
        assert_eq!(
            code.matches(&application).count(),
            3,
            "gzseek, gztell and gzoffset must each wrap their guard in narrow_off"
        );

        // And the truncating cast this replaced is gone from the shipped code.
        let truncating_cast = std::format!("}}) as {}", "z_off_t");
        assert!(
            !code.contains(&truncating_cast),
            "a bare cast of a guard result to the narrow offset type silently \
             truncates on 32-bit targets (gzlib.c L437-L442 refuses instead)"
        );

        // The helper itself must perform the round trip, not merely narrow. On a
        // 64-bit host no value is unrepresentable, so the value test above cannot
        // catch a helper quietly reduced to a bare cast; this can.
        let at = code.find(&definition).expect("narrow_off must be present");
        let rest = &code[at..];
        let helper = &rest[..rest
            .find("\n}")
            .expect("narrow_off must be a complete item")];
        let round_trip = std::format!("{}::from(narrow) == wide", "z_off64_t");
        assert!(
            helper.contains(&round_trip),
            "narrow_off must widen the narrowed value back and compare, which is \
             C's `ret == (z_off_t)ret` test (gzlib.c L441)"
        );
        assert!(
            helper.contains("-1"),
            "narrow_off must answer -1 when the round trip fails"
        );
    }

    /// A **successful** `gzdopen` takes ownership: the descriptor stays open while
    /// the handle lives and is closed by the finalizer.
    ///
    /// Mirrors reference-zlib harness case `2b`.
    #[cfg(unix)]
    #[test]
    fn successful_gzdopen_transfers_descriptor_ownership() {
        use std::os::fd::IntoRawFd;

        let (path, _cpath) = unique_path("dopenown");
        let data = b"ownership test";
        let fd = path.create().into_raw_fd();

        // SAFETY: `fd` is a live descriptor being handed to `gzdopen`, and the
        // mode is a NUL-terminated literal.
        let wf = unsafe { gzdopen(fd, c"wb".as_ptr()) };
        assert!(!wf.is_null());
        assert!(
            fd_is_alive(fd),
            "the adopted descriptor stays open while the handle lives"
        );

        // SAFETY: `wf` is a live handle from `gzdopen`; the buffer is valid for
        // the length given.
        assert_eq!(
            unsafe { gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint) },
            data.len() as c_int
        );
        // SAFETY: `wf` is a live write handle, closed exactly once here.
        assert_eq!(unsafe { gzclose_w(wf) }, Z_OK);
        assert!(
            !fd_is_alive(fd),
            "the finalizer must close the descriptor it owns"
        );

        assert_eq!(decode_gzip_file(&path), data.to_vec());
    }

    /// A failing platform close surfaces as `Z_ERRNO` from **both** finalizers,
    /// and the gzip member is still finalized first.
    ///
    /// C: `gzclose_w` does `if (close(state->fd) == -1) ret = Z_ERRNO;` *after*
    /// the `Z_FINISH` flush (`gzwrite.c` L685-L696), and `gzclose_r` does
    /// `return ret ? Z_ERRNO : err;` (`gzread.c` L665-L667). Mirrors
    /// reference-zlib harness cases `3r`, `3w`, and `3b`.
    #[test]
    fn injected_close_failure_reports_z_errno_for_both_directions() {
        let (path, cpath) = unique_path("closefail");
        let data = b"close failure path";

        unsafe {
            // ---- write side: Z_FINISH still runs, then the close fails.
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(
                gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(
                with_forced_close_failure(|| gzclose_w(wf)),
                Z_ERRNO,
                "a failing close(2) overrides the accumulated write status"
            );
        }
        // The member was finalized before the close was attempted, so it is
        // complete and independently decodable despite the reported error.
        assert_eq!(decode_gzip_file(&path), data.to_vec());

        unsafe {
            // ---- read side.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            let mut buf = std::vec![0u8; data.len()];
            assert_eq!(
                gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(&buf[..], &data[..]);
            assert_eq!(
                with_forced_close_failure(|| gzclose_r(rf)),
                Z_ERRNO,
                "a failing close(2) turns Z_OK into Z_ERRNO on the read side"
            );

            // ---- C `3b`: with the seam disarmed both directions report Z_OK.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            assert_eq!(gzclose_r(rf), Z_OK);

            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());
            assert_eq!(gzclose_w(wf), Z_OK);
        }
    }

    /// A wrong-direction rejection outranks a close failure, because a refused
    /// close releases no descriptor and therefore never reaches `close(2)`.
    #[test]
    fn close_failure_does_not_mask_a_wrong_direction_rejection() {
        let (_path, cpath) = unique_path("failmask");
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());

            assert_eq!(
                with_forced_close_failure(|| gzclose_r(wf)),
                Z_STREAM_ERROR,
                "the direction check precedes the close, so Z_STREAM_ERROR wins"
            );

            // The handle is untouched and still closes cleanly with the seam
            // disarmed.
            assert_eq!(gzclose_w(wf), Z_OK);
        }
    }

    /// [`finish_close`]'s full precedence table: a released descriptor that closes
    /// cleanly passes the status through, one that fails yields `Z_ERRNO`, and a
    /// refusal (no descriptor) passes the status through untouched.
    #[test]
    fn finish_close_precedence_table() {
        let (path, _cpath) = unique_path("precedence");

        // No descriptor released (the wrong-direction arm): status passes through.
        assert_eq!(finish_close(Z_STREAM_ERROR, None), Z_STREAM_ERROR);
        assert_eq!(finish_close(Z_OK, None), Z_OK);

        // Each arm consumes a descriptor, so each needs its own file: exclusive
        // creation cannot re-truncate one path four times, which is precisely the
        // property that makes it safe. All four are siblings inside the guard's
        // private directory and are removed with it.
        let fresh = |name: &str| create_new_file(&path.sibling(name));

        // Descriptor released and the close succeeds: status passes through, for
        // both a success and a preserved error status.
        assert_eq!(finish_close(Z_OK, Some(fresh("ok.bin"))), Z_OK);
        let buf_error = ReturnCode::BufError.as_c_int();
        assert_eq!(
            finish_close(buf_error, Some(fresh("buferr.bin"))),
            buf_error
        );

        // Descriptor released and the close fails: Z_ERRNO overrides everything.
        assert_eq!(
            with_forced_close_failure(|| finish_close(Z_OK, Some(fresh("failok.bin")))),
            Z_ERRNO
        );
        assert_eq!(
            with_forced_close_failure(|| finish_close(buf_error, Some(fresh("failerr.bin")))),
            Z_ERRNO,
            "a close failure overrides an accumulated error status too"
        );
    }

    /// `take_for_close` reclaims the box only for a matching direction, and the
    /// `CloseDirection::Either` screen accepts both live directions.
    #[test]
    fn take_for_close_only_claims_a_matching_direction() {
        let (_path, cpath) = unique_path("takeonly");
        unsafe {
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null());

            // A reader-only request is refused, leaving the handle intact.
            assert!(take_for_close(wf, CloseDirection::Read).is_none());
            // `Either` accepts a writer.
            let claimed = take_for_close(wf, CloseDirection::Either);
            assert!(claimed.is_some());
            // Give the reclaimed box back to the finalizer so it is not leaked and
            // the descriptor is closed exactly once.
            let (status, released) =
                gz::gzclose_w_release(claimed.expect("writer was claimed").state);
            assert_eq!(finish_close(status, released), Z_OK);

            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null());
            assert!(take_for_close(rf, CloseDirection::Write).is_none());
            let claimed = take_for_close(rf, CloseDirection::Either);
            let (status, released) =
                gz::gzclose_r_release(claimed.expect("reader was claimed").state);
            assert_eq!(finish_close(status, released), Z_OK);
        }
    }

    /// `gzopen_w` — the Windows-only wide-character open — must actually *work*,
    /// not merely link.
    ///
    /// C declares it under `#if defined(_WIN32) && !defined(Z_SOLO)`
    /// (`zlib.h` L2041-L2042) and this port gates it with `#[cfg(windows)]`, so
    /// on a Linux or macOS row the symbol is correctly absent and no test there
    /// can reach it. That left the Windows row as the only place the wide path is
    /// compiled *and* — until this test — the only exported entry point that was
    /// never executed anywhere: a `gzopen_w` that returned `NULL`
    /// unconditionally, or that decoded the UTF-16 code units into a different
    /// name than the caller asked for, would still have satisfied a
    /// compile-and-signature check (AAP §0.7.2 S8 — platform claims require
    /// platform coverage).
    ///
    /// The round trip is written through a wide path holding non-ASCII code
    /// units, so `OsString::from_wide` has to reconstruct the name exactly; the
    /// file the wide units *name* is then confirmed to exist and to hold a
    /// well-formed gzip member using `std` and the independent reference decoder,
    /// i.e. without trusting `gzopen_w` to verify itself. The same wide path is
    /// re-opened for reading and required to return the payload byte-for-byte
    /// with `gzerror` clean, EOF set, and a successful close in each direction.
    /// The null-argument guards are exercised last, because C's `gzopen_w` must
    /// refuse them rather than dereference.
    #[cfg(windows)]
    #[test]
    fn wide_path_open_round_trip() {
        use crate::gz::test_temp::TempDir;
        use std::os::windows::ffi::OsStrExt as _;

        // Non-ASCII code units make the UTF-16 decode load-bearing, so the *file
        // name* keeps them. The uniqueness and exclusivity live on the enclosing
        // directory instead: it is created with `mkdir(2)` create-new semantics, so
        // this name cannot have been pre-placed as a symlink, and the guard removes
        // the whole directory on drop -- including when an assertion unwinds, which
        // the previous fixed `%TEMP%\…_<pid>.gz` path plus manual `remove_file`
        // could not do.
        let dir = TempDir::new("ffi_gz_wide");
        let path = dir.child("wide_\u{e9}\u{4e2d}.gz");
        // C's `const wchar_t *` is NUL-terminated; `encode_wide` is not.
        let wide: std::vec::Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(core::iter::once(0))
            .collect();

        let data = b"wide-path gzip payload\nsecond line with more bytes\n";
        // SAFETY: every call below receives a NUL-terminated wide path, a
        // NUL-terminated mode literal, and handles returned by `gzopen_w` itself;
        // the buffers named by the read/write calls are live locals whose lengths
        // match the counts passed alongside them.
        unsafe {
            // ---- write through the wide path
            let wf = gzopen_w(wide.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null(), "gzopen_w for write returned NULL");
            assert_eq!(
                gzwrite(wf, data.as_ptr() as voidpc, data.len() as c_uint),
                data.len() as c_int,
                "every byte must be accepted through a wide-path handle"
            );
            let mut errnum: c_int = Z_STREAM_ERROR;
            let msg = gzerror(wf, &raw mut errnum);
            assert_eq!(errnum, Z_OK, "a healthy wide-path writer reports Z_OK");
            assert!(!msg.is_null() && CStr::from_ptr(msg).to_bytes().is_empty());
            assert_eq!(gzclose_w(wf), Z_OK, "the wide-path member must finalize");
        }

        // The wide units named *this* file, and it holds a complete gzip member —
        // both established without going back through `gzopen_w`.
        assert!(
            path.exists(),
            "gzopen_w must create the file its wide path names"
        );
        assert_eq!(decode_gzip_file(&path), data.to_vec());

        // SAFETY: as above — the same NUL-terminated wide path and mode literals,
        // a handle from `gzopen_w`, and live local buffers with matching counts.
        unsafe {
            // ---- read back through the wide path
            let rf = gzopen_w(wide.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null(), "gzopen_w for read returned NULL");
            let mut buf = std::vec![0u8; data.len()];
            assert_eq!(
                gzread(rf, buf.as_mut_ptr() as voidp, buf.len() as c_uint),
                data.len() as c_int
            );
            assert_eq!(
                &buf[..],
                &data[..],
                "a wide-path round trip must be byte-exact"
            );
            // Reading past the end yields 0 and sets the EOF indicator.
            let mut extra = [0u8; 1];
            assert_eq!(gzread(rf, extra.as_mut_ptr() as voidp, 1), 0);
            assert_eq!(gzeof(rf), 1);
            let mut errnum: c_int = Z_STREAM_ERROR;
            let msg = gzerror(rf, &raw mut errnum);
            assert_eq!(errnum, Z_OK, "a clean read must not latch an error");
            assert!(!msg.is_null() && CStr::from_ptr(msg).to_bytes().is_empty());
            assert_eq!(gzclose_r(rf), Z_OK);

            // ---- null guards: refused, never dereferenced
            assert!(
                gzopen_w(ptr::null(), c"rb".as_ptr()).is_null(),
                "a null wide path must be refused"
            );
            assert!(
                gzopen_w(wide.as_ptr(), ptr::null()).is_null(),
                "a null mode must be refused"
            );
        }
    }

    /// Reads the raw descriptor out of a live `gzFile` handle.
    ///
    /// `state.file` owns the descriptor for as long as the handle is open, so the
    /// returned value is valid until the matching `gzclose*`; nothing here takes
    /// ownership of it.
    ///
    /// # Safety
    ///
    /// `file` must be a non-null, not-yet-closed handle produced by
    /// `gzopen*`/`gzdopen`.
    #[cfg(unix)]
    unsafe fn handle_fd(file: gzFile) -> c_int {
        use std::os::fd::AsRawFd;
        // SAFETY: per the contract `file` is a live handle, so `gz_handle`'s
        // requirements are met; the borrow ends with this expression and only a
        // `Copy` descriptor number is read out of it.
        unsafe { gz_handle(file).state.file.as_raw_fd() }
    }

    /// `fcntl(fd, F_GETFD) & FD_CLOEXEC`, as a bool — the exact quantity the C
    /// acceptance probe prints as `cloexec=`.
    #[cfg(unix)]
    fn fd_cloexec(fd: c_int) -> bool {
        // SAFETY: `fd` is a live descriptor owned by the caller's handle;
        // `F_GETFD` reads the descriptor flags and takes no third argument.
        let flags = unsafe { fcntl(fd, F_GETFD) };
        assert!(flags >= 0, "F_GETFD must succeed on a live descriptor");
        flags & crate::gz::DescriptorRequest::FD_CLOEXEC != 0
    }

    /// `fcntl(fd, F_GETFL) & O_NONBLOCK`, as a bool — the exact quantity the C
    /// acceptance probe prints as `nonblock=`.
    #[cfg(unix)]
    fn fd_nonblock(fd: c_int) -> bool {
        // SAFETY: as above; `F_GETFL` reads the file status flags and takes no
        // third argument.
        let flags = unsafe { fcntl(fd, F_GETFL) };
        assert!(flags >= 0, "F_GETFL must succeed on a live descriptor");
        flags & crate::gz::DescriptorRequest::O_NONBLOCK != 0
    }

    /// A descriptor `gzopen`ed **without** mode `e` must not be close-on-exec, and
    /// one opened **with** `e` must be (finding M6-02).
    ///
    /// C accumulates `oflag` in its mode loop and adds `O_CLOEXEC` only for `'e'`
    /// (`gzlib.c` L134-L138), then calls `open(path, oflag, 0666)`. Reference zlib
    /// therefore hands back an *inheritable* descriptor for every ordinary mode
    /// string and a close-on-exec one only for `'e'`. A [`std::fs::File`] is
    /// close-on-exec unconditionally, so the divergence is in the **absence** of
    /// the flag: without [`reconcile_opened_descriptor`] a C caller that
    /// `fork`s and `exec`s while holding an open gzip file would lose it.
    ///
    /// The asserted values are the ones measured against reference zlib, probe
    /// cases `m1_wb_fdstate` (`cloexec=0 nonblock=0`), `m2_wbe_fdstate`
    /// (`cloexec=1 nonblock=0`), `m3_rb_fdstate` (`cloexec=0 nonblock=0`),
    /// `m4_rbe_fdstate` (`cloexec=1 nonblock=0`) and `m5_wbN_fdstate`
    /// (`cloexec=0 nonblock=1`).
    #[cfg(unix)]
    #[test]
    fn gzopen_reproduces_c_close_on_exec_state() {
        let (path, cpath) = unique_path("openflags");
        // SAFETY: `cpath` is a NUL-terminated path, every mode is a C string
        // literal, and each handle is read only while open and closed exactly once.
        unsafe {
            // ---- write, no `e`: C asks for no O_CLOEXEC, so the flag is clear.
            let wf = gzopen(cpath.as_ptr(), c"wb".as_ptr());
            assert!(!wf.is_null(), "gzopen \"wb\" must succeed");
            let fd = handle_fd(wf);
            assert!(
                !fd_cloexec(fd),
                "mode \"wb\" asks for no O_CLOEXEC, so FD_CLOEXEC must be clear"
            );
            assert!(!fd_nonblock(fd), "mode \"wb\" asks for no O_NONBLOCK");
            assert_eq!(gzclose_w(wf), Z_OK);

            // ---- write, with `e`: C asks for O_CLOEXEC explicitly.
            let ef = gzopen(cpath.as_ptr(), c"wbe".as_ptr());
            assert!(!ef.is_null(), "gzopen \"wbe\" must succeed");
            let fd = handle_fd(ef);
            assert!(
                fd_cloexec(fd),
                "mode \"wbe\" asks for O_CLOEXEC, so FD_CLOEXEC must be set"
            );
            assert!(!fd_nonblock(fd), "mode \"wbe\" asks for no O_NONBLOCK");
            assert_eq!(gzclose_w(ef), Z_OK);

            // ---- read, no `e`: the same rule, the other direction.
            let rf = gzopen(cpath.as_ptr(), c"rb".as_ptr());
            assert!(!rf.is_null(), "gzopen \"rb\" must succeed");
            assert!(
                !fd_cloexec(handle_fd(rf)),
                "mode \"rb\" asks for no O_CLOEXEC either"
            );
            assert_eq!(gzclose_r(rf), Z_OK);

            // ---- read, with `e`.
            let ref_ = gzopen(cpath.as_ptr(), c"rbe".as_ptr());
            assert!(!ref_.is_null(), "gzopen \"rbe\" must succeed");
            assert!(
                fd_cloexec(handle_fd(ref_)),
                "mode \"rbe\" asks for O_CLOEXEC"
            );
            assert_eq!(gzclose_r(ref_), Z_OK);

            // ---- `N` alone must not smuggle close-on-exec back in, and must
            //      still deliver the non-blocking descriptor it asked for.
            let nf = gzopen(cpath.as_ptr(), c"wbN".as_ptr());
            assert!(!nf.is_null(), "gzopen \"wbN\" must succeed");
            let fd = handle_fd(nf);
            assert!(!fd_cloexec(fd), "mode \"wbN\" still asks for no O_CLOEXEC");
            assert!(fd_nonblock(fd), "mode \"N\" must open non-blocking");
            assert_eq!(gzclose_w(nf), Z_OK);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// `gzdopen` must apply `O_NONBLOCK` for mode `N` and must leave the adopted
    /// descriptor's close-on-exec state exactly as the caller left it
    /// (finding M6-03).
    ///
    /// C has no `open` call on this path, so it reconciles with `fcntl`
    /// (`gzlib.c` L253-L263):
    ///
    /// ```c
    /// if (oflag & O_NONBLOCK) fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
    /// if (oflag & O_CLOEXEC)  fcntl(fd, F_SETFD, fcntl(fd, F_GETFD) | O_CLOEXEC);
    /// ```
    ///
    /// The second call is a **no-op** on every POSIX platform — `F_SETFD`'s only
    /// defined flag is `FD_CLOEXEC` (`1`) while `O_CLOEXEC` is a different bit
    /// entirely (`0o2000000` on Linux), and Linux computes
    /// `set_close_on_exec(fd, arg & FD_CLOEXEC)` — which is exactly why
    /// [`reconcile_adopted_descriptor`] omits it. This test pins that down from
    /// **both** starting states, so a future "completion" of the pair fails here
    /// rather than silently making descriptors close-on-exec that C leaves alone.
    ///
    /// The asserted values are the ones measured against reference zlib: probe
    /// cases `m7_dopen_wbe` (started `cloexec=0` → still `cloexec=0`),
    /// `m11_dopen_wbe` (started `cloexec=1` → still `cloexec=1`),
    /// `m8_after_dopen_wbeN` and `m10_after_dopen_rbeN` (`nonblock=1`), and
    /// `m6_after_dopen_wb` (no `N`, still `nonblock=0`).
    #[cfg(unix)]
    #[test]
    fn gzdopen_applies_nonblock_and_never_touches_close_on_exec() {
        use std::os::fd::IntoRawFd;

        let (path, _cpath) = unique_path("dopenflags");
        std::fs::write(&path, b"seed").unwrap();

        // A fresh descriptor on `path` with FD_CLOEXEC forced to `want` and
        // O_NONBLOCK clear — the two starting states the C probe measures.
        let fresh = |want: bool| -> c_int {
            let fd = std::fs::File::open(&path).unwrap().into_raw_fd();
            let target = if want {
                crate::gz::DescriptorRequest::FD_CLOEXEC
            } else {
                0
            };
            // SAFETY: `fd` was just produced by `File::open` and is owned by this
            // closure's caller; `F_SETFD` takes exactly one `int`, supplied here.
            assert!(
                unsafe { fcntl(fd, F_SETFD, target) } >= 0,
                "F_SETFD must succeed on a freshly opened descriptor"
            );
            assert_eq!(fd_cloexec(fd), want, "the starting state must be exact");
            assert!(!fd_nonblock(fd), "a freshly opened file is blocking");
            fd
        };

        for started_cloexec in [false, true] {
            // ---- mode with `N`: O_NONBLOCK on, FD_CLOEXEC untouched.
            let fd = fresh(started_cloexec);
            // SAFETY: `fd` is a live descriptor and the mode is a C string
            // literal; the handle is closed exactly once below.
            let f = unsafe { gzdopen(fd, c"rbeN".as_ptr()) };
            assert!(!f.is_null(), "gzdopen \"rbeN\" must succeed");
            // SAFETY: `f` is the live handle just returned by `gzdopen`.
            let adopted = unsafe { handle_fd(f) };
            assert!(
                fd_nonblock(adopted),
                "mode \"N\" must set O_NONBLOCK on the adopted descriptor"
            );
            assert_eq!(
                fd_cloexec(adopted),
                started_cloexec,
                "gzdopen must not change close-on-exec (started {started_cloexec})"
            );
            // SAFETY: `f` is a live reader handle, closed exactly once.
            assert_eq!(unsafe { gzclose_r(f) }, Z_OK);

            // ---- mode `e` without `N`: neither flag moves, because C's
            //      O_CLOEXEC `fcntl` is the proven no-op.
            let fd = fresh(started_cloexec);
            // SAFETY: as above.
            let f = unsafe { gzdopen(fd, c"rbe".as_ptr()) };
            assert!(!f.is_null(), "gzdopen \"rbe\" must succeed");
            // SAFETY: as above.
            let adopted = unsafe { handle_fd(f) };
            assert!(
                !fd_nonblock(adopted),
                "without \"N\" the adopted descriptor stays blocking"
            );
            assert_eq!(
                fd_cloexec(adopted),
                started_cloexec,
                "C's O_CLOEXEC fcntl is a no-op, so \"e\" must change nothing \
                 (started {started_cloexec})"
            );
            // SAFETY: as above.
            assert_eq!(unsafe { gzclose_r(f) }, Z_OK);

            // ---- no flags at all: still nothing moves.
            let fd = fresh(started_cloexec);
            // SAFETY: as above.
            let f = unsafe { gzdopen(fd, c"rb".as_ptr()) };
            assert!(!f.is_null(), "gzdopen \"rb\" must succeed");
            // SAFETY: as above.
            let adopted = unsafe { handle_fd(f) };
            assert!(!fd_nonblock(adopted), "plain \"rb\" stays blocking");
            assert_eq!(
                fd_cloexec(adopted),
                started_cloexec,
                "plain \"rb\" must not change close-on-exec (started {started_cloexec})"
            );
            // SAFETY: as above.
            assert_eq!(unsafe { gzclose_r(f) }, Z_OK);
        }

        let _ = std::fs::remove_file(&path);
    }
}
