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
//! that: the mode is pre-validated before `File::from_raw_fd`, and an allocation
//! failure after adoption releases the descriptor with `into_raw_fd` instead of
//! closing it.
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

#[cfg(all(unix, feature = "gz-io"))]
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

/// Panic guard for the `z_size_t`-returning shims (`gzfread` / `gzfwrite`).
///
/// [`crate::ffi::types`] provides `guard_int` / `guard_off` / `guard_ptr` but no
/// `usize` variant, so this mirrors the same catch-and-default behavior for
/// [`z_size_t`]. `gz-io` implies `std`, so [`std::panic::catch_unwind`] is
/// always available here.
#[cfg(feature = "gz-io")]
#[inline]
fn guard_size(
    default: z_size_t,
    f: impl FnOnce() -> z_size_t + core::panic::UnwindSafe,
) -> z_size_t {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// Panic guard for the sole `*const c_char`-returning shim (`gzerror`).
#[cfg(feature = "gz-io")]
#[inline]
fn guard_const_ptr<T>(
    default: *const T,
    f: impl FnOnce() -> *const T + core::panic::UnwindSafe,
) -> *const T {
    std::panic::catch_unwind(f).unwrap_or(default)
}

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
/// but zlib reports a failure as [`Z_ERRNO`], so the descriptor must be closed
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
            let Ok(mode_str) = (unsafe { CStr::from_ptr(mode) }).to_str() else {
                return ptr::null_mut();
            };
            box_state(gz::gzopen(pathbuf, mode_str))
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
            let Ok(mode_str) = (unsafe { CStr::from_ptr(mode) }).to_str() else {
                return ptr::null_mut();
            };
            box_state(gz::gzopen64(pathbuf, mode_str))
        })
    }
}

/// `gzFile gzdopen(int fd, const char *mode)`  *(Unix)*
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
/// * a negative `fd`, a null `mode`, and a non-UTF-8 `mode` are rejected before
///   `File::from_raw_fd`, so the descriptor is never adopted;
/// * an **invalid mode string** (`"r+"`, `"rT"`, `"wG"`, a string with no
///   `r`/`w`/`a`, …) is rejected by `crate::gz::validate_mode`, also before
///   adoption — this is the check C performs at `gzlib.c` L150-L197; and
/// * if the handle allocation fails after adoption, the descriptor is released
///   back to the OS-owned world with `into_raw_fd` rather than closed — the
///   analogue of C's `malloc` failure at `gzlib.c` L206-L210, which likewise
///   leaves `fd` open.
///
/// Returns `NULL` on any of those failures.
///
/// [`File`]: std::fs::File
#[cfg(unix)]
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
            if mode.is_null() || fd < 0 {
                return ptr::null_mut();
            }
            // SAFETY: `mode` is non-null (checked) and NUL-terminated.
            let Ok(mode_str) = (unsafe { CStr::from_ptr(mode) }).to_str() else {
                return ptr::null_mut();
            };
            // C `gz_open` validates the whole mode grammar before it takes the
            // caller's descriptor, so every rejection must happen while `fd` is still
            // the caller's. Adopting first and letting the open fail would close a
            // descriptor C leaves open — an ownership divergence a C caller cannot
            // detect and that turns its own later `close(fd)` into a double close.
            if gz::validate_mode(mode_str).is_err() {
                return ptr::null_mut();
            }
            // SAFETY: the caller transfers ownership of `fd`, a valid open OS file
            // descriptor (negatives rejected above). Adoption happens only now, with
            // the mode already proven acceptable, so the remaining failure mode is an
            // exhausted heap — handled by releasing the descriptor below rather than
            // closing it.
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            dopen_state(gz::gzdopen(file, mode_str))
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
/// world with `into_raw_fd`, deliberately leaving it open for the caller —
/// precisely what "the caller still owns `fd`" means — while the rest of the
/// state drops, matching C's `free(state)`.
#[cfg(feature = "gz-io")]
#[cfg(unix)]
fn dopen_state(result: Result<Box<GzState>, ReturnCode>) -> gzFile {
    // The mode was pre-validated and the descriptor already adopted, so `Err`
    // here is unreachable in practice. Should it ever occur, the `GzState` was
    // never constructed and the transient `File` has already been dropped by
    // `gz_open`, so there is nothing left to release.
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
            // give it back to the caller unclosed: `into_raw_fd` dissolves the
            // `File` without closing, leaving `fd` exactly as the caller passed
            // it. `handle` (and with it the buffers) drops here — C's
            // `free(state)`.
            if let Some(file) = released {
                use std::os::fd::IntoRawFd;
                let _ = file.into_raw_fd();
            }
            ptr::null_mut()
        }
    }
}

/// `gzFile gzdopen(int fd, const char *mode)`  *(non-Unix fallback)*
///
/// Adopting a raw C `int` file descriptor is a POSIX concept with no portable
/// `std` equivalent off Unix (Windows uses `HANDLE`s). The symbol is retained
/// for ABI completeness but always fails here; use [`gzopen`]/`gzopen_w`
/// instead on such targets.
#[cfg(not(unix))]
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
/// actually read (`0` at end of file), or `-1` on error or a null handle/buffer.
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
            if file.is_null() || buf.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            // SAFETY: `buf` is non-null (checked) and, per the C contract, valid for
            // writes of `len` bytes.
            let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
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
            // SAFETY: `buf` is non-null (checked) and valid for `len` bytes
            // (`len > 0` checked). The idiomatic writer NUL-terminates within it.
            let out = unsafe { slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
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
        guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            // Widen the C `off_t` to the engine's 64-bit offset. On 32-bit targets
            // (`z_off_t == i32`) this is a real widening; on 64-bit it is a no-op.
            gz::gzseek(state, offset as z_off64_t, whence)
        }) as z_off_t
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
        guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed immutably.
            let guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &*guard;
            gz::gztell(state)
        }) as z_off_t
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
        guard_off(-1, || {
            if file.is_null() {
                return -1;
            }
            // SAFETY: non-null handle; borrowed, not owned.
            let mut guard = GzBorrow::new(unsafe { gz_handle(file) });
            let state = &mut *guard;
            gz::gzoffset(state)
        }) as z_off_t
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
        // Void return: catch any panic and swallow it (never unwind into C).
        let _ = std::panic::catch_unwind(|| {
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
            // SAFETY: `mode` is non-null (checked) and NUL-terminated.
            let Ok(mode_str) = (unsafe { CStr::from_ptr(mode) }).to_str() else {
                return ptr::null_mut();
            };
            box_state(gz::gzopen(std::path::PathBuf::from(os), mode_str))
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
    use std::ffi::CString;
    use std::path::PathBuf;

    /// Builds a process-unique temp path (and its `CString` form) for a test.
    fn unique_path(tag: &str) -> (PathBuf, CString) {
        let mut p = std::env::temp_dir();
        p.push(std::format!(
            "zlibrs_ffi_gz_{tag}_{}.gz",
            std::process::id()
        ));
        let c = CString::new(p.to_str().expect("temp path is valid UTF-8")).unwrap();
        (p, c)
    }

    #[test]
    fn write_then_read_round_trip() {
        let (path, cpath) = unique_path("rt");
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
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn puts_gets_getc_round_trip() {
        let (path, cpath) = unique_path("lines");
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
        let _ = std::fs::remove_file(&path);
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
        let (path, cpath) = unique_path("printfstub");

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
        let _ = std::fs::remove_file(&path);
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
        let (path, cpath) = unique_path("printfctl");
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
        let _ = std::fs::remove_file(&path);
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
        let (path, cpath) = unique_path("buf");
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
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn gzdopen_fd_round_trip() {
        use std::os::fd::IntoRawFd;
        let (path, _cpath) = unique_path("dopen");
        let data = b"descriptor round trip\n";
        unsafe {
            // Write via a descriptor handed to gzdopen.
            let file = std::fs::File::create(&path).unwrap();
            let fd = file.into_raw_fd();
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
        let _ = std::fs::remove_file(&path);
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
        let (path, cpath) = unique_path("c2prefix");
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
        let _ = std::fs::remove_file(&path);
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

        let _ = std::fs::remove_file(&path);
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
        let (path, cpath) = unique_path("refuseprefix");
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
        let _ = std::fs::remove_file(&path);
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
        let _ = std::fs::remove_file(&path);
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
        std::fs::write(&path, b"seed").unwrap();

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

        // A negative descriptor is rejected outright (C `if (fd == -1 || ...)`).
        // SAFETY: no descriptor is dereferenced on this path.
        assert!(unsafe { gzdopen(-1, c"rb".as_ptr()) }.is_null());

        let _ = std::fs::remove_file(&path);
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
        let fd = std::fs::File::create(&path).unwrap().into_raw_fd();

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
        let _ = std::fs::remove_file(&path);
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

        let _ = std::fs::remove_file(&path);
    }

    /// A wrong-direction rejection outranks a close failure, because a refused
    /// close releases no descriptor and therefore never reaches `close(2)`.
    #[test]
    fn close_failure_does_not_mask_a_wrong_direction_rejection() {
        let (path, cpath) = unique_path("failmask");
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
        let _ = std::fs::remove_file(&path);
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

        // Descriptor released and the close succeeds: status passes through, for
        // both a success and a preserved error status.
        let f = std::fs::File::create(&path).unwrap();
        assert_eq!(finish_close(Z_OK, Some(f)), Z_OK);
        let f = std::fs::File::create(&path).unwrap();
        let buf_error = ReturnCode::BufError.as_c_int();
        assert_eq!(finish_close(buf_error, Some(f)), buf_error);

        // Descriptor released and the close fails: Z_ERRNO overrides everything.
        let f = std::fs::File::create(&path).unwrap();
        assert_eq!(
            with_forced_close_failure(|| finish_close(Z_OK, Some(f))),
            Z_ERRNO
        );
        let f = std::fs::File::create(&path).unwrap();
        assert_eq!(
            with_forced_close_failure(|| finish_close(buf_error, Some(f))),
            Z_ERRNO,
            "a close failure overrides an accumulated error status too"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// `take_for_close` reclaims the box only for a matching direction, and the
    /// `CloseDirection::Either` screen accepts both live directions.
    #[test]
    fn take_for_close_only_claims_a_matching_direction() {
        let (path, cpath) = unique_path("takeonly");
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
        let _ = std::fs::remove_file(&path);
    }
}
