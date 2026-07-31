//! C-ABI (`extern "C"`) shim layer for the zlib **inflate** (decompression) API
//! and the raw-callback **`inflateBack`** API.
//!
//! This module is one half of the crate's FFI drop-in boundary (the other being
//! [`crate::ffi::deflate`](mod@crate::ffi::deflate)). Each exported function reproduces the exact zlib C
//! signature (verified against `zlib.h` / `zlib.map` of `1.3.2.1-motley`), so a
//! C consumer can link against the emitted `cdylib`/`staticlib` and call these
//! symbols with no source changes.
//!
//! # Safe engine, unsafe boundary
//!
//! The actual decompression logic lives in the safe-Rust engine under
//! [`crate::inflate`]; it operates on the idiomatic [`ZStream`] and never uses
//! `unsafe`. This file is where the raw C `#[repr(C)]` [`z_stream`] is bridged
//! to that engine: raw pointers are validated, the opaque state is boxed into
//! `z_stream.state`, `Result`/`ReturnCode` values are re-materialized as the
//! integer codes C expects, and the idiomatic `Option<&str>` messages are
//! mapped back to `'static` C strings. Consequently every `unsafe` operation in
//! this file carries a `// SAFETY:` justification, and — because a Rust panic
//! must never unwind across the `extern "C"` boundary into C — every fallible
//! body is wrapped in one of the `guard_*` helpers, which catch unwinding (when
//! `std` is available) and substitute a safe integer default.
//!
//! # State-handle model
//!
//! * Regular inflate path: the boxed `InflateHandle` (an idiomatic
//!   [`ZStream`] plus a raw pointer to the caller's registered `gz_header`) is
//!   stored in `z_stream.state`; [`inflateEnd`] reclaims and drops it (RAII
//!   replaces the manual `inflateEnd` free).
//! * `inflateBack` path: a `Box<InflateState>` is stored directly, matching the
//!   [`crate::inflate::back`] engine which allocates and owns the decode window.
//!
//! # `inflateBack` callback bridge
//!
//! zlib's `inflateBack` pulls input and pushes output through the C callbacks
//! `in_func`/`out_func`. Those are wrapped into `CInFunc`/`COutFunc` which
//! implement the engine's [`InFunc`]/[`OutFunc`] traits, so the safe engine
//! drives decompression while the raw pointer dereferences stay isolated here.

// The entire module is the FFI boundary: every exported function shares the
// same uniform safety contract documented above (callers must pass a valid
// `z_stream` and valid buffers exactly as the zlib C API requires). Documenting
// that contract once at the module level is clearer than repeating a `# Safety`
// section on each of the ~21 shims, so the per-item lint is allowed here.
#![allow(clippy::missing_safety_doc)]

use core::ffi::{CStr, c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};
use core::{ptr, slice};

use alloc::boxed::Box;

use crate::constants::DEF_WBITS;
use crate::error::ReturnCode;
use crate::ffi::alloc::try_box;
use crate::ffi::types::{
    Bytef, CAllocator, HandleKind, advance_input, advance_output, guard_int, guard_ulong,
    gz_headerp, in_func, input_ptr_valid, input_slice, out_func, output_slice, peek_handle_kind,
    set_adler, set_data_type, set_msg, state_ptr_from_box, stream_buffers_valid, uInt, z_stream,
    z_streamp, zstream_with_caller_alloc,
};
use crate::inflate::back::{InFunc, OutFunc};
use crate::inflate::state::InflateState;
use crate::stream::ZStream;

// `gz_header` (the `#[repr(C)]` mirror) and the header write-back helper are
// only referenced by the gzip-only `inflateGetHeader` path and the header
// write-back inside `inflate`, so gate their imports to avoid unused warnings.
#[cfg(feature = "gzip")]
use crate::ffi::types::{gz_header, write_gz_header_from_idiomatic};
#[cfg(feature = "gzip")]
use crate::gz_header::GzHeader;

// ---------------------------------------------------------------------------
// Return-code constants
//
// `constants.rs` intentionally does not define the integer return codes (they
// are modeled by `ReturnCode`); we materialize the small subset referenced as
// literals in this file from the canonical `ReturnCode::as_c_int()` const fn so
// there is a single source of truth. Codes returned only by flattening an
// engine `Result` go through `ReturnCode::as_c_int()` directly and need no
// local constant here.
// ---------------------------------------------------------------------------

/// C `Z_OK` (0).
const Z_OK: c_int = ReturnCode::Ok.as_c_int();
/// C `Z_STREAM_ERROR` (-2).
const Z_STREAM_ERROR: c_int = ReturnCode::StreamError.as_c_int();
/// C `Z_VERSION_ERROR` (-6).
const Z_VERSION_ERROR: c_int = ReturnCode::VersionError.as_c_int();
/// `Z_MEM_ERROR` — an allocation (engine buffer or opaque handle) was refused.
const Z_MEM_ERROR: c_int = ReturnCode::MemError.as_c_int();

/// The value C's `inflateMark` returns when the stream state is unusable:
/// `-(1L << 16)` == `-65536`.
const INFLATE_MARK_ERR: c_long = -(1 << 16);

// ---------------------------------------------------------------------------
// `strm->adler` mirroring
// ---------------------------------------------------------------------------

/// Mirrors the engine's `adler` value into the caller's `z_stream`, but **only**
/// on the wrapper modes where C ever assigns `strm->adler`.
///
/// # Why this is gated
///
/// C's `inflate.c` contains exactly seven `strm->adler` assignments, and every
/// one of them is unreachable when `state->wrap == 0` (raw DEFLATE):
///
/// | C site | Guard |
/// |--------|-------|
/// | L109 (`inflateResetKeep`) | `if (state->wrap)` — explicit, with C's "to support ill-conceived Java test suite" comment |
/// | L550 | inside the zlib-header (`wrap & 1`) acceptance path |
/// | L690 | inside the gzip header-CRC (`wrap & 2`) path |
/// | L696 / L705 | `DICTID` / `DICT`, reachable only from a zlib header |
/// | L1079 / L1145 | `if ((state->wrap & 4) && out)` |
///
/// So for `wrap == 0` a C caller's `strm.adler` is **never** written by
/// `inflateInit2_`, `inflateReset*`, `inflateSync` or `inflate` — whatever value
/// the caller left there survives untouched for the whole life of the stream.
/// Because `ZStream::adler` is an owned field that starts at its own default
/// (`1`), mirroring it back unconditionally published that default into the
/// caller's struct and destroyed the caller's value. Gating on `wrap` restores
/// C's behavior exactly.
///
/// All of C's reset entry points funnel into `inflateResetKeep`, and
/// `inflateInit2_`/`inflateSync` funnel into `inflateReset2`/`inflateReset` in
/// turn, so the single L109 guard governs the whole family — which is why one
/// helper covers every call site here.
///
/// The field is also *only* ever an output mirror: the engine reads its own
/// `state.check`, never `strm.adler`, so declining to write it cannot perturb
/// decoding.
///
/// `inflateBackInit_` deliberately does not call this at all: `infback.c`
/// contains **zero** `strm->adler` assignments (its caller owns the check
/// value entirely), so any write there would be a divergence.
///
/// # Why gating alone is not enough
///
/// `wrap != 0` is necessary but not sufficient. On a wrapped stream that fails
/// early — a bad zlib window size, an unknown compression method, a bad FCHECK,
/// absent gzip magic — the decoder reaches `BAD` *before* L550/L690, so C again
/// writes nothing and the caller's value must survive. Callers that keep a
/// checksum of their own in the field therefore also need
/// [`seed_adler_mirror`] before the engine call, which makes a write-back on
/// such a path a value-preserving no-op.
#[inline]
fn mirror_adler_if_wrapped(sref: &mut z_stream, wrap: c_int, adler: u32) {
    if wrap != 0 {
        set_adler(sref, adler);
    }
}

/// Seeds the engine's `adler` mirror from the caller's `z_stream` so that a call
/// in which C would not have assigned `strm->adler` writes the value straight
/// back unchanged.
///
/// [`ZStream::adler`] is a pure *output* mirror: the decoder's authoritative
/// running checksum is `InflateState::check`, and nothing in `crate::inflate`
/// ever reads `strm.adler`. Seeding it is therefore invisible to decoding and
/// only affects what gets published at the boundary.
///
/// The `c_ulong` field is narrowed to `u32`, which cannot lose anything C would
/// have preserved: for every wrapped mode C assigns `strm->adler = state->wrap
/// & 1` while still inside `inflateInit2_` (`inflate.c` L108-L109), clearing any
/// high bits before the caller's first `inflate` call, and for `wrap == 0`
/// [`mirror_adler_if_wrapped`] declines to write the field at all — so the
/// narrowed value is never published on the one path where high bits could still
/// be live.
#[inline]
fn seed_adler_mirror<A: crate::stream::Allocator>(zs: &mut ZStream<A>, caller_adler: c_ulong) {
    zs.adler = caller_adler as u32;
}

// ---------------------------------------------------------------------------
// Panic guard for `c_long`-returning shims (`inflateMark`)
//
// `crate::ffi::types` provides `guard_int`/`guard_ulong`/`guard_off`/`guard_ptr`
// but not a `c_long` variant, so we add a private one mirroring their exact
// `#[cfg(feature = "std")]` structure: catch unwinding under `std`, run directly
// under `no_std` (where `catch_unwind` is unavailable and builds abort on panic).
// ---------------------------------------------------------------------------

/// Runs `f`, returning its `c_long` result, or `default` if it panics.
#[cfg(feature = "std")]
fn guard_long(default: c_long, f: impl FnOnce() -> c_long + core::panic::UnwindSafe) -> c_long {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
fn guard_long(_default: c_long, f: impl FnOnce() -> c_long + core::panic::UnwindSafe) -> c_long {
    f()
}

// ---------------------------------------------------------------------------
// Message-string bridge
// ---------------------------------------------------------------------------

/// Maps an idiomatic engine message (`Option<&str>`, i.e. a non-NUL-terminated
/// Rust string literal) to a `'static` NUL-terminated C string pointer suitable
/// for `z_stream.msg`.
///
/// The engine stores one of a fixed set of `&'static str` diagnostics; every one
/// is matched to a compile-time `c"…"` literal whose pointer is valid for the
/// life of the program. Anything unrecognized (including `None`) maps to a null
/// pointer, matching zlib's convention of a null `msg` when there is no message.
fn msg_to_cstr(msg: Option<&str>) -> *const c_char {
    let s = match msg {
        Some(s) => s,
        None => return ptr::null(),
    };
    let cstr: &CStr = match s {
        // --- inflate engine diagnostics (inflate.c / infback.c) ---
        "incorrect header check" => c"incorrect header check",
        "unknown compression method" => c"unknown compression method",
        "invalid window size" => c"invalid window size",
        "unknown header flags set" => c"unknown header flags set",
        "header crc mismatch" => c"header crc mismatch",
        "incorrect data check" => c"incorrect data check",
        "incorrect length check" => c"incorrect length check",
        "invalid block type" => c"invalid block type",
        "invalid stored block lengths" => c"invalid stored block lengths",
        "too many length or distance symbols" => c"too many length or distance symbols",
        "invalid code lengths set" => c"invalid code lengths set",
        "invalid bit length repeat" => c"invalid bit length repeat",
        "invalid code -- missing end-of-block" => c"invalid code -- missing end-of-block",
        "invalid literal/lengths set" => c"invalid literal/lengths set",
        "invalid distances set" => c"invalid distances set",
        "invalid literal/length code" => c"invalid literal/length code",
        "invalid distance code" => c"invalid distance code",
        "invalid distance too far back" => c"invalid distance too far back",
        // --- generic return-code messages (zutil.c `z_errmsg`) ---
        "need dictionary" => c"need dictionary",
        "stream end" => c"stream end",
        "file error" => c"file error",
        "stream error" => c"stream error",
        "data error" => c"data error",
        "insufficient memory" => c"insufficient memory",
        "buffer error" => c"buffer error",
        "incompatible version" => c"incompatible version",
        _ => return ptr::null(),
    };
    cstr.as_ptr()
}

// ---------------------------------------------------------------------------
// Opaque state handle for the regular inflate path
// ---------------------------------------------------------------------------

/// Owns the idiomatic decompression state behind a raw `z_stream.state`.
///
/// Boxed and installed by [`inflateInit2_`]; borrowed via [`inflate_handle`]
/// during operation; reclaimed and dropped by [`inflateEnd`]. Under the `gzip`
/// feature it also records the raw pointer to the caller's `gz_header`
/// (registered by [`inflateGetHeader`]) so the filled header can be written back
/// after each [`inflate`] call.
///
/// The leading [`HandleKind`] tag ([`HandleKind::INFLATE`]) lets the `End`/
/// accessor shims verify the handle *kind* before reinterpreting the opaque
/// `state` pointer, preventing the layout-mismatched deallocation that a blind
/// `Box::from_raw` cast would cause on cross-type misuse (FINDING-6). `#[repr(C)]`
/// guarantees `kind` sits at offset 0, matching [`DeflateHandle`].
#[repr(C)]
struct InflateHandle {
    /// Discriminant tag; always [`HandleKind::INFLATE`]. MUST be the first field.
    kind: HandleKind,
    /// The idiomatic stream carrying the boxed `InflateState` plus observable
    /// bookkeeping (`total_in`/`total_out`/`adler`/`data_type`/`msg`).
    zs: ZStream<CAllocator>,
    /// Raw pointer to the caller's `gz_header` registered via `inflateGetHeader`,
    /// or null if none. Only present when gzip framing is compiled in.
    #[cfg(feature = "gzip")]
    head: *mut gz_header,
}

impl InflateHandle {
    /// Wraps a freshly initialized idiomatic stream, tagging it as an inflate
    /// handle, with no registered header.
    fn new(zs: ZStream<CAllocator>) -> Self {
        Self {
            kind: HandleKind::INFLATE,
            zs,
            #[cfg(feature = "gzip")]
            head: ptr::null_mut(),
        }
    }
}

/// Borrows the [`InflateHandle`] behind the opaque `state` pointer, validating
/// the [`HandleKind`] tag first. Returns [`None`] when no handle is installed OR
/// the installed handle is not an inflate handle (cross-type misuse), so the
/// caller can return `Z_STREAM_ERROR` WITHOUT reinterpreting a wrong-type
/// allocation.
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by an FFI init shim
/// (so its leading tag is readable).
#[inline]
unsafe fn inflate_handle(strm: &mut z_stream) -> Option<&mut InflateHandle> {
    // SAFETY: delegated tag read; see `peek_handle_kind`.
    match unsafe { peek_handle_kind(strm) } {
        Some(kind) if kind == HandleKind::INFLATE => {
            // SAFETY: the tag confirms a live `InflateHandle`; the borrow is tied
            // to `strm`, so it cannot alias for its lifetime.
            Some(unsafe { &mut *(strm.state as *mut InflateHandle) })
        }
        _ => None,
    }
}

/// Reclaims the boxed [`InflateHandle`] from `state`, validating the
/// [`HandleKind`] tag first and nulling `state` on success. Returns [`None`] —
/// leaving `state` untouched — when the handle is absent or not an inflate
/// handle, so [`inflateEnd`] never drops a wrong-type box (FINDING-6).
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by an FFI init shim,
/// not already reclaimed.
#[inline]
unsafe fn inflate_take(strm: &mut z_stream) -> Option<Box<InflateHandle>> {
    // SAFETY: delegated tag read; see `peek_handle_kind`.
    match unsafe { peek_handle_kind(strm) } {
        Some(kind) if kind == HandleKind::INFLATE => {
            // SAFETY: the tag confirms a live `Box<InflateHandle>`; reconstitute
            // exactly once and null the field to prevent a double free.
            let boxed = unsafe { Box::from_raw(strm.state as *mut InflateHandle) };
            strm.state = ptr::null_mut();
            Some(boxed)
        }
        _ => None,
    }
}

/// The boxed `inflateBack` engine handle installed in [`z_stream::state`] by
/// [`inflateBackInit_`].
///
/// Unlike the streaming [`inflate`] path, `inflateBack` drives the raw engine
/// directly with a caller-owned window (which doubles as the output buffer), so
/// this handle owns the `Box<InflateState>` outright rather than an idiomatic
/// [`ZStream`]. The leading [`HandleKind`] tag ([`HandleKind::INFLATE_BACK`])
/// lets [`inflateBack`] and [`inflateBackEnd`] verify the handle *kind* before
/// reinterpreting the opaque `state` pointer, closing the C4 UB where an
/// untagged `Box<InflateState>` could be blindly reconstituted — or, worse, a
/// tagged deflate/inflate handle freed through the wrong layout. `#[repr(C)]`
/// guarantees `kind` sits at offset 0, matching every other tagged handle.
#[repr(C)]
struct InflateBackHandle {
    /// Discriminant tag; always [`HandleKind::INFLATE_BACK`]. MUST be first.
    kind: HandleKind,
    /// The engine state whose owned window doubles as the sliding output buffer.
    inner: Box<InflateState>,
}

impl InflateBackHandle {
    /// Wraps a freshly built back-inflate state, tagging it as an `inflateBack`
    /// handle.
    #[inline]
    fn new(inner: Box<InflateState>) -> Self {
        Self {
            kind: HandleKind::INFLATE_BACK,
            inner,
        }
    }
}

/// Borrows the [`InflateBackHandle`] behind the opaque `state` pointer,
/// validating the [`HandleKind`] tag first. Returns [`None`] when no handle is
/// installed OR the installed handle is not an `inflateBack` handle (cross-type
/// misuse), so the caller can return `Z_STREAM_ERROR` WITHOUT reinterpreting a
/// wrong-type allocation.
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by an FFI init shim
/// (so its leading tag is readable).
#[inline]
unsafe fn inflate_back_handle(strm: &mut z_stream) -> Option<&mut InflateBackHandle> {
    // SAFETY: delegated tag read; see `peek_handle_kind`.
    match unsafe { peek_handle_kind(strm) } {
        Some(kind) if kind == HandleKind::INFLATE_BACK => {
            // SAFETY: the tag confirms a live `InflateBackHandle`; the borrow is
            // tied to `strm`, so it cannot alias for its lifetime.
            Some(unsafe { &mut *(strm.state as *mut InflateBackHandle) })
        }
        _ => None,
    }
}

/// Reclaims the boxed [`InflateBackHandle`] from `state`, validating the
/// [`HandleKind`] tag first and nulling `state` on success. Returns [`None`] —
/// leaving `state` untouched — when the handle is absent or not an `inflateBack`
/// handle, so [`inflateBackEnd`] never drops a wrong-type box (C4).
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by an FFI init shim,
/// not already reclaimed.
#[inline]
unsafe fn inflate_back_take(strm: &mut z_stream) -> Option<Box<InflateBackHandle>> {
    // SAFETY: delegated tag read; see `peek_handle_kind`.
    match unsafe { peek_handle_kind(strm) } {
        Some(kind) if kind == HandleKind::INFLATE_BACK => {
            // SAFETY: the tag confirms a live `Box<InflateBackHandle>`;
            // reconstitute exactly once and null the field to prevent a double
            // free.
            let boxed = unsafe { Box::from_raw(strm.state as *mut InflateBackHandle) };
            strm.state = ptr::null_mut();
            Some(boxed)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// C callback adapters for `inflateBack`
// ---------------------------------------------------------------------------

/// Adapts zlib's C `in_func` to the engine's [`InFunc`] trait.
///
/// On the first call it yields whatever input was already buffered in the
/// stream (`next_in[..avail_in]`); thereafter it invokes the C callback, which
/// returns a byte count and points `*buf` at that many readable input bytes.
///
/// It also records the provenance of the most recent chunk it hands to the
/// engine (`last_ptr`/`last_len`), whether it has handed out any real input
/// (`handed_out`), whether the most recent pull ran dry (`dry`), and the
/// engine-reported unconsumed tail (`unconsumed`). After [`inflate_back`]
/// returns, the shim uses these to restore the C `z_stream`'s `next_in`/
/// `avail_in` exactly like C `infback.c`'s `inf_leave` (C3).
struct CInFunc<'a> {
    in_fn: unsafe extern "C" fn(*mut c_void, *mut *const c_uchar) -> c_uint,
    in_desc: *mut c_void,
    initial: &'a [u8],
    initial_done: bool,
    /// Base pointer of the most recent NON-EMPTY chunk handed to the engine
    /// (the initial buffer first, then successive callback buffers). Defaults to
    /// the initial buffer's base so a decode consuming only buffered input still
    /// reconstructs a correct `next_in`.
    last_ptr: *const c_uchar,
    /// Length of that most-recent non-empty chunk.
    last_len: usize,
    /// Set once any non-empty chunk has been handed to the engine.
    handed_out: bool,
    /// Set when the MOST RECENT pull yielded an empty slice (the callback ran
    /// dry). Mirrors C `PULL` returning 0, which sets `next = Z_NULL`/`have = 0`.
    dry: bool,
    /// Bytes of the last chunk left unconsumed at exit, reported by the engine
    /// via [`InFunc::set_unconsumed`]. Combined with `last_ptr`/`last_len` to
    /// reconstruct C's `next`/`have`.
    unconsumed: usize,
}

impl InFunc for CInFunc<'_> {
    fn next_input(&mut self) -> &[u8] {
        if !self.initial_done {
            self.initial_done = true;
            if !self.initial.is_empty() {
                // Record the initial stream buffer as the current chunk.
                self.last_ptr = self.initial.as_ptr();
                self.last_len = self.initial.len();
                self.handed_out = true;
                self.dry = false;
                return self.initial;
            }
        }
        let mut buf: *const c_uchar = ptr::null();
        // SAFETY: `in_fn` is a valid zlib `in_func` (checked non-null by the
        // `inflateBack` shim before this adapter is constructed). Per the zlib
        // callback contract it returns a count `n` and stores in `*buf` a pointer
        // to `n` readable bytes that remain valid at least until the next call —
        // which is exactly the lifetime over which the engine uses the slice.
        let n = unsafe { (self.in_fn)(self.in_desc, &mut buf) };
        if n == 0 || buf.is_null() {
            // Callback ran dry: mirror C `PULL` returning 0 (next = Z_NULL).
            self.dry = true;
            &[]
        } else {
            // Record this callback buffer as the current chunk.
            self.last_ptr = buf;
            self.last_len = n as usize;
            self.handed_out = true;
            self.dry = false;
            // SAFETY: the callback guaranteed `n` bytes are readable at `buf`.
            unsafe { slice::from_raw_parts(buf, n as usize) }
        }
    }

    fn set_unconsumed(&mut self, unconsumed: usize) {
        self.unconsumed = unconsumed;
    }
}

/// Adapts zlib's C `out_func` to the engine's [`OutFunc`] trait. The engine
/// hands it a slice of decompressed output; the callback returns `0` on success
/// (any nonzero value aborts `inflateBack`, mirroring zlib).
struct COutFunc {
    out_fn: unsafe extern "C" fn(*mut c_void, *mut c_uchar, c_uint) -> c_int,
    out_desc: *mut c_void,
}

impl OutFunc for COutFunc {
    fn write_output(&mut self, buf: &[u8]) -> Result<(), ()> {
        // SAFETY: `out_fn` is a valid zlib `out_func` (checked non-null by the
        // `inflateBack` shim). It reads `buf.len()` bytes from the supplied
        // pointer for the duration of the call and must not retain it; we pass a
        // pointer into the engine's live window slice. A zero return is success.
        let rc = unsafe {
            (self.out_fn)(
                self.out_desc,
                buf.as_ptr() as *mut c_uchar,
                buf.len() as c_uint,
            )
        };
        if rc == 0 { Ok(()) } else { Err(()) }
    }
}

// ---------------------------------------------------------------------------
// Shared init version check (identical to the deflate side)
// ---------------------------------------------------------------------------

/// Reproduces zlib's `inflateInit_`/`inflateInit2_` version guard: the caller's
/// version string must be non-null with a matching major version, and the
/// caller's reported `sizeof(z_stream)` must equal ours. Returns `true` when
/// compatible.
#[inline]
unsafe fn version_check(version: *const c_char, stream_size: c_int) -> bool {
    if version.is_null() {
        return false;
    }
    // SAFETY: `version` is non-null, so it points to at least the first byte of
    // the caller's NUL-terminated version C-string; we read only that one byte,
    // exactly as the C init macros do for ABI-compatibility checking.
    if unsafe { *version } != b'1' as c_char {
        return false;
    }
    stream_size == size_of::<z_stream>() as c_int
}

// ===========================================================================
// Phase 1 — initialization shims
//
// Only the real `_`-suffixed symbols are exported. The bare `inflateInit`,
// `inflateInit2`, and `inflateBackInit` names are C preprocessor macros (they
// inject the version string and `sizeof(z_stream)`), so they are deliberately
// NOT defined here.
// ===========================================================================

/// C `inflateInit2_` — initialize for decompression with an explicit
/// `windowBits`.
///
/// `windowBits` retains zlib's full overloading, forwarded unchanged to the
/// engine: `8..=15` selects a zlib (RFC 1950) wrapper, `-8..=-15` raw DEFLATE
/// (no wrapper), `24..=31` (i.e. `16 + 8..=15`) a gzip (RFC 1952) wrapper, and
/// `40..=47` (`32 + 8..=15`) zlib/gzip auto-detection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateInit2_(
    strm: z_streamp,
    window_bits: c_int,
    version: *const c_char,
    stream_size: c_int,
) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        // SAFETY: reads only the first byte of `version` (if non-null); see
        // `version_check`.
        if !unsafe { version_check(version, stream_size) } {
            return Z_VERSION_ERROR;
        }
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and, per the FFI contract, points to a valid
        // caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // Reject a half-present allocator pair before anything is allocated,
        // mirroring where C tests it: after the version and null-stream guards
        // and before the state `ZALLOC` (`inflate.c` L183-L199). C substitutes the
        // missing half in place; this crate cannot (`zcalloc`/`zcfree` are
        // unexported, and mixing a caller hook with the global allocator is UB),
        // and ignoring the supplied half would silently swallow the caller's
        // out-of-memory signal (AAP §0.6.3). `Z_STREAM_ERROR` is the code C's own
        // `inflateStateCheck` produces for such a stream (`inflate.c` L90-L91)
        // and the code its `Z_SOLO` init returns (`inflate.c` L184-L186). See
        // `CAllocator::is_half_present` for the documented divergence.
        // SAFETY: `sref` is a valid `&z_stream`; only its `Copy` allocator fields
        // are read, and the hook pointers are never dereferenced.
        if unsafe { CAllocator::from_stream(sref) }.is_half_present() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `sref` is a valid `z_stream`; this copies its `zalloc`/`zfree`/
        // `opaque` fields into an allocator wrapper (it does not read `state`).
        let mut zs = unsafe { zstream_with_caller_alloc(sref) };
        match crate::inflate::inflate_init2(&mut zs, window_bits) {
            Ok(_) => {
                let adler = zs.adler;
                // C's init path reaches `strm->adler` only through
                // `inflateResetKeep`'s `if (state->wrap)` guard (`inflate.c`
                // L108-L109), so a raw stream must leave the caller's field
                // alone. See `mirror_adler_if_wrapped`.
                let wrap = zs.inflate_state().map_or(0, |s| s.wrap);
                // Box the handle **fallibly**: `Box::new` would abort on
                // global-heap exhaustion where C reports `Z_MEM_ERROR`
                // (AAP §0.6.5). On failure `zs` drops here, releasing the state
                // reservation (and any window) through the caller's `zfree`, and
                // the caller's `z_stream` is left untouched.
                let Some(handle) = try_box(InflateHandle::new(zs)) else {
                    return Z_MEM_ERROR;
                };
                // SAFETY: transfers ownership of the box into the opaque
                // `state` slot; reclaimed and dropped by `inflateEnd`.
                sref.state = unsafe { state_ptr_from_box(handle) };
                sref.total_in = 0;
                sref.total_out = 0;
                sref.msg = ptr::null_mut();
                mirror_adler_if_wrapped(sref, wrap, adler);
                Z_OK
            }
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateInit_` — initialize for decompression with the default window
/// size. Equivalent to `inflateInit2_(strm, DEF_WBITS, …)` (`DEF_WBITS` == 15),
/// exactly as the C macro expands.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateInit_(
    strm: z_streamp,
    version: *const c_char,
    stream_size: c_int,
) -> c_int {
    // SAFETY: forwards the raw arguments unchanged to `inflateInit2_`, which
    // performs all validation.
    unsafe { inflateInit2_(strm, DEF_WBITS, version, stream_size) }
}

/// C `inflateBackInit_` — initialize for a raw-callback `inflateBack` decode.
///
/// `windowBits` must be in `8..=15` (raw DEFLATE only) and `window` must address
/// at least `1 << windowBits` bytes.
///
/// # The caller's window is used, not replaced
///
/// This is the one zlib entry point whose sliding window is supplied by the
/// caller. Reference zlib adopts the pointer verbatim — `state->window = window;`
/// (`infback.c` L60) — makes exactly **one** `zalloc` (the state, `infback.c`
/// L51), and frees only that state in `inflateBackEnd` (`infback.c` L572-L577).
/// This shim reproduces all three properties: the region is lent to the engine
/// through the boundary's borrowed-buffer bridge, so no second allocation is made,
/// no bytes are copied, the caller may legitimately place the buffer in static or
/// mapped memory, and teardown leaves it untouched (AAP §0.6.3, §0.6.5).
///
/// The state footprint is still charged to any caller-supplied `zalloc` captured
/// from the `z_stream`, which is exactly the single request C makes.
///
/// # Safety
///
/// In addition to the usual `z_stream` obligations, `window` must be valid for
/// reads and writes of `1 << windowBits` bytes, and must stay allocated and
/// un-aliased until [`inflateBackEnd`] destroys the state. These are the
/// obligations `zlib.h` already places on the argument (`zlib.h` L1682-L1699).
///
/// The region is zero-filled before use — see `crate::ffi::alloc`'s
/// `borrow_caller_window` for why that is required and why it is unobservable to
/// a correct caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateBackInit_(
    strm: z_streamp,
    window_bits: c_int,
    window: *mut c_uchar,
    version: *const c_char,
    stream_size: c_int,
) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        // SAFETY: reads only the first byte of `version` (if non-null).
        if !unsafe { version_check(version, stream_size) } {
            return Z_VERSION_ERROR;
        }
        if strm.is_null() || window.is_null() {
            return Z_STREAM_ERROR;
        }
        if !(8..=15).contains(&window_bits) {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // Capture any caller-supplied zalloc/zfree/opaque so the *state* footprint
        // is charged to the caller's allocator, matching the single
        // `ZALLOC(strm, 1, sizeof(struct inflate_state))` C makes (`infback.c`
        // L51). Null hooks leave the reservation unmade, exactly as a C caller with
        // `zalloc == Z_NULL` gets the built-in allocator.
        // SAFETY: `sref` is a valid `&z_stream`; `from_stream` only copies the
        // plain `Copy` allocator fields and never dereferences the hooks.
        let allocator = unsafe { CAllocator::from_stream(sref) };
        // A half-present pair is rejected here, in the same position C tests it —
        // after the version, null-stream, null-window and `windowBits` guards and
        // before the state `ZALLOC` (`infback.c` L30-L53). C substitutes the
        // missing half; this crate cannot, and ignoring the supplied half would
        // discard the caller's out-of-memory signal (AAP §0.6.3). See
        // `CAllocator::is_half_present` for the documented divergence.
        if allocator.is_half_present() {
            return Z_STREAM_ERROR;
        }

        // Lend the caller's buffer to the engine rather than allocating a
        // replacement: C adopts the pointer with `state->window = window;`
        // (`infback.c` L60) and `inflateBackEnd` never frees it.
        // SAFETY: `window` is non-null (checked above) and the caller's documented
        // contract on this entry point guarantees it is valid for reads and writes
        // of `1 << window_bits` bytes and stays valid, un-aliased, until
        // `inflateBackEnd` destroys the state built from it.
        let lent =
            unsafe { crate::ffi::alloc::borrow_caller_window(window, 1usize << window_bits) };
        let Some(lent) = lent else {
            // The only reachable causes are an exhausted global heap (the tiny box
            // holding the borrow) — `Z_MEM_ERROR`, as C reports for a failed
            // allocation — since the null and range checks above already ran.
            return Z_MEM_ERROR;
        };

        match crate::inflate::back::inflate_back_init_borrowed_window(&allocator, window_bits, lent)
        {
            Ok(state) => {
                // Tag the state as an `inflateBack` handle before installing it,
                // so `inflateBack`/`inflateBackEnd` can validate the kind and the
                // regular `inflateEnd` rejects it (C4: no more blind casts of an
                // untagged `Box<InflateState>`).
                // Fallible boxing: an exhausted Rust heap must surface as
                // `Z_MEM_ERROR`, which is what C's failed state `ZALLOC` returns
                // (`infback.c` L52-L53), not as an abort (AAP §0.6.5). The
                // dropped `state` releases the state reservation through the
                // caller's `zfree` and leaves the lent window untouched.
                let Some(handle) = try_box(InflateBackHandle::new(state)) else {
                    return Z_MEM_ERROR;
                };
                // SAFETY: transfers ownership of the `Box<InflateBackHandle>`
                // into the opaque `state` slot; reclaimed and dropped by
                // `inflateBackEnd` after tag validation.
                sref.state = unsafe { state_ptr_from_box(handle) };
                sref.total_in = 0;
                sref.total_out = 0;
                sref.msg = ptr::null_mut();
                // `strm->adler` is deliberately NOT touched here: `infback.c`
                // contains zero `strm->adler` assignments across its whole
                // lifetime (`inflateBackInit_`, `inflateBack`, `inflateBackEnd`),
                // because an `inflateBack` caller owns the check value itself.
                // C's `inflateBackInit_` writes only `strm->msg` (L36) and the
                // `state` slot, so publishing a `0` here — as this shim used to —
                // clobbered a field C leaves entirely alone.
                Z_OK
            }
            Err(rc) => rc.as_c_int(),
        }
    })
}

// ===========================================================================
// Phase 2 — core driver
// ===========================================================================

/// C `inflate` — decompress as much as possible.
///
/// Bridges the raw `z_stream` input/output windows to the safe engine, then
/// writes the results back into the raw stream. The observable semantics match
/// zlib: `Z_STREAM_END` at end of stream, `Z_NEED_DICT` when a preset
/// dictionary is required (with `strm.adler` set to the required checksum),
/// `Z_BUF_ERROR` on no progress, and `Z_DATA_ERROR` with `strm.msg` set on
/// corrupt input.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflate(strm: z_streamp, flush: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };

        // C `inflate` entry validation (`inflate.c` L474): reject a null
        // `next_out`, or a null `next_in` paired with a positive `avail_in`,
        // with `Z_STREAM_ERROR` — *before* bridging, so the programmer error is
        // surfaced rather than masked into an empty slice by
        // `input_slice`/`output_slice`.
        if !stream_buffers_valid(sref) {
            return Z_STREAM_ERROR;
        }

        // Borrow the input/output windows. These helpers return slices with
        // *detached* lifetimes that point at the caller's external buffers, not
        // into the `z_stream` struct, so they remain valid while we subsequently
        // take a `&mut` borrow of the struct for the state handle and cursor
        // updates.
        // SAFETY: honors `next_in`/`avail_in` and `next_out`/`avail_out`; the
        // caller guarantees those describe valid readable/writable regions.
        let input: &[u8] = unsafe { input_slice(sref) };
        // SAFETY: as above for the output window; the region is exclusively ours
        // for the duration of the call.
        let output: &mut [u8] = unsafe { output_slice(sref) };

        // Snapshot the caller's `adler` before `handle` takes a mutable borrow
        // of `sref` (see `seed_adler_mirror`).
        let caller_adler = sref.adler;
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>` installed by
        // `inflateInit2_`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };

        // Seed the engine's `adler` mirror from the caller's field so that a
        // call in which C assigns nothing (raw framing, or a wrapped stream that
        // fails in its header before `inflate.c` L550/L690) publishes the
        // caller's own value straight back. See `seed_adler_mirror`.
        seed_adler_mirror(&mut handle.zs, caller_adler);
        let outcome = crate::inflate::inflate(&mut handle.zs, input, output, flush);

        // Snapshot observable fields out of the handle before its borrow ends.
        let adler = handle.zs.adler;
        // All six of C's in-`inflate` `strm->adler` writes are unreachable for
        // `wrap == 0` (`inflate.c` L550/L690/L696/L705 sit on zlib/gzip header
        // paths; L1079/L1145 are guarded by `state->wrap & 4`), so a raw stream
        // must leave the caller's field alone. See `mirror_adler_if_wrapped`.
        let wrap = handle.zs.inflate_state().map_or(0, |s| s.wrap);
        let data_type = handle.zs.data_type;
        let msg = handle.zs.msg;

        // gzip header write-back: if the caller registered a `gz_header` via
        // `inflateGetHeader`, mirror whatever the engine has captured so far into
        // it, honoring the caller's `extra_max`/`name_max`/`comm_max` capacities.
        #[cfg(feature = "gzip")]
        {
            let head_ptr = handle.head;
            if !head_ptr.is_null() {
                if let Some(state) = handle.zs.inflate_state() {
                    if let Some(gh) = state.head.as_ref() {
                        // SAFETY: `head_ptr` is the caller's `gz_header`, still
                        // valid (registered via `inflateGetHeader`); the helper
                        // writes only within the recorded `*_max` capacities.
                        unsafe { write_gz_header_from_idiomatic(head_ptr, gh) };
                    }
                }
            }
        }

        // The `handle` borrow ends here (its last use was above); re-borrow the
        // raw stream to advance cursors and publish observable fields.
        // C's two `RESTORE()`-then-return-directly paths commit the cursors but
        // jump over `strm->total_in += in; strm->total_out += out;`
        // (`inflate.c` L1141-L1142). Snapshot the totals so they can be rolled
        // back for exactly those returns; see `InflateOutcome::commit_totals`.
        let (prev_total_in, prev_total_out) = (sref.total_in, sref.total_out);
        // SAFETY: `outcome.consumed <= input.len() == avail_in`, so advancing by
        // that amount keeps `next_in`/`avail_in`/`total_in` consistent.
        unsafe { advance_input(sref, outcome.consumed) };
        // SAFETY: `outcome.produced <= output.len() == avail_out`, as above.
        unsafe { advance_output(sref, outcome.produced) };
        if !outcome.commit_totals {
            sref.total_in = prev_total_in;
            sref.total_out = prev_total_out;
        }
        mirror_adler_if_wrapped(sref, wrap, adler);
        set_data_type(sref, data_type);
        set_msg(sref, msg_to_cstr(msg));

        outcome.code.as_c_int()
    })
}

// ===========================================================================
// Phase 3 — lifecycle: end / reset
// ===========================================================================

/// C `inflateEnd` — free all state associated with the stream. The boxed
/// `InflateHandle` is reclaimed and dropped, whose `Drop` frees the window and
/// tables (RAII replaces zlib's manual cleanup).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateEnd(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `sref` is a valid, uniquely-borrowed `z_stream`, so
        // `inflate_take` may inspect and clear its `state` slot. `inflate_take`
        // validates the handle's `HandleKind` tag BEFORE reconstituting the box:
        // it returns the `Box<InflateHandle>` only for a genuine inflate handle
        // (nulling `state` for exactly-once reclaim) and `None` for a cross-type
        // stream (e.g. one from `deflateInit*` or `inflateBackInit_`) — yielding
        // `Z_STREAM_ERROR` WITHOUT a layout-mismatched free, and matching C,
        // which returns `Z_STREAM_ERROR` for a non-inflate stream (FINDING-6).
        match unsafe { inflate_take(sref) } {
            Some(boxed) => {
                drop(boxed);
                // `strm->msg` is deliberately left ALONE. C's `inflateEnd`
                // (`inflate.c` L1155-L1165) frees the window and the state, nulls
                // `strm->state`, and returns `Z_OK` — it never assigns
                // `strm->msg`. Nulling it here broke the canonical zlib
                // diagnostic idiom used by the library's own samples, which call
                // `inflateEnd` and *then* report the error text (see
                // `test/example.c`'s `err`/`CHECK_ERR` usage and
                // `examples/zpipe.c`'s `zerr`), leaving such callers with a null
                // pointer where C hands them "invalid distance too far back",
                // "incorrect data check", and so on. `deflateEnd`
                // (`deflate.c` L1293-L1310) likewise never touches `msg`, and
                // this shim's `deflateEnd` correctly already does not.
                Z_OK
            }
            None => Z_STREAM_ERROR,
        }
    })
}

/// C `inflateReset` — reset the stream to a freshly-initialized state, keeping
/// the current `windowBits`/wrap configuration and allocations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateReset(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        let res = crate::inflate::inflate_reset(&mut handle.zs);
        let adler = handle.zs.adler;
        // C's whole reset family funnels into `inflateResetKeep`, whose
        // only `strm->adler` write is guarded by `if (state->wrap)`
        // (`inflate.c` L108-L109). See `mirror_adler_if_wrapped`.
        let wrap = handle.zs.inflate_state().map_or(0, |s| s.wrap);
        // `handle` borrow ends here.
        match res {
            Ok(_) => {
                sref.total_in = 0;
                sref.total_out = 0;
                sref.msg = ptr::null_mut();
                mirror_adler_if_wrapped(sref, wrap, adler);
                Z_OK
            }
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateReset2` — like [`inflateReset`] but also re-selects the wrap mode
/// and window size from a new `windowBits` (with the same overloading as
/// [`inflateInit2_`]). Invalid `windowBits` yields `Z_STREAM_ERROR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateReset2(strm: z_streamp, window_bits: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        let res = crate::inflate::inflate_reset2(&mut handle.zs, window_bits);
        let adler = handle.zs.adler;
        // C's whole reset family funnels into `inflateResetKeep`, whose
        // only `strm->adler` write is guarded by `if (state->wrap)`
        // (`inflate.c` L108-L109). See `mirror_adler_if_wrapped`.
        let wrap = handle.zs.inflate_state().map_or(0, |s| s.wrap);
        // `handle` borrow ends here.
        match res {
            Ok(_) => {
                sref.total_in = 0;
                sref.total_out = 0;
                sref.msg = ptr::null_mut();
                mirror_adler_if_wrapped(sref, wrap, adler);
                Z_OK
            }
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateResetKeep` — reset the stream while preserving already-processed
/// history (the sliding window contents), as used by `inflateReset`'s "keep"
/// variant. Observable byte totals are reset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateResetKeep(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        let res = crate::inflate::inflate_reset_keep(&mut handle.zs);
        let adler = handle.zs.adler;
        // C's whole reset family funnels into `inflateResetKeep`, whose
        // only `strm->adler` write is guarded by `if (state->wrap)`
        // (`inflate.c` L108-L109). See `mirror_adler_if_wrapped`.
        let wrap = handle.zs.inflate_state().map_or(0, |s| s.wrap);
        // `handle` borrow ends here.
        match res {
            Ok(_) => {
                sref.total_in = 0;
                sref.total_out = 0;
                sref.msg = ptr::null_mut();
                mirror_adler_if_wrapped(sref, wrap, adler);
                Z_OK
            }
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

// ===========================================================================
// Phase 4 — dictionary / sync / prime shims
// ===========================================================================

/// C `inflateSetDictionary` — supply a preset dictionary for decompression
/// (used after `inflate` reports `Z_NEED_DICT`). Returns `Z_DATA_ERROR` if the
/// dictionary's Adler-32 does not match the value the stream expects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateSetDictionary(
    strm: z_streamp,
    dictionary: *const Bytef,
    dict_length: uInt,
) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        let dict: &[u8] = if dictionary.is_null() || dict_length == 0 {
            &[]
        } else {
            // SAFETY: the caller guarantees `dict_length` readable bytes at
            // `dictionary` for the duration of this call.
            unsafe { slice::from_raw_parts(dictionary, dict_length as usize) }
        };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_set_dictionary(&mut handle.zs, dict) {
            Ok(rc) => rc.as_c_int(),
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateGetDictionary` — copy the sliding-window history (the current
/// dictionary) into the caller's buffer. If `dictionary` is null, only the
/// length is reported (via `dict_length`), enabling the usual query-then-fetch
/// pattern.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateGetDictionary(
    strm: z_streamp,
    dictionary: *mut Bytef,
    dict_length: *mut uInt,
) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        // How many bytes the engine would copy (the window history length).
        let whave = handle
            .zs
            .inflate_state()
            .map(|s| s.whave as usize)
            .unwrap_or(0);
        // Present the caller's buffer (assumed to hold at least `whave` bytes, as
        // the zlib contract requires) as a mutable slice, or an empty slice when
        // querying the length only.
        let dict_slice: &mut [u8] = if dictionary.is_null() {
            &mut []
        } else {
            // SAFETY: per the zlib contract the caller supplied a buffer of at
            // least `whave` bytes at `dictionary`.
            unsafe { slice::from_raw_parts_mut(dictionary, whave) }
        };
        let mut len: usize = 0;
        let res = crate::inflate::inflate_get_dictionary(&handle.zs, dict_slice, &mut len);
        if !dict_length.is_null() {
            // SAFETY: `dict_length` is non-null and points to a writable `uInt`.
            unsafe { *dict_length = len as uInt };
        }
        match res {
            Ok(rc) => rc.as_c_int(),
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateSync` — skip invalid compressed data until a possible full-flush
/// point is found, for error recovery. Consumes input from the raw stream and
/// advances its cursors accordingly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateSync(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // Reject a null `next_in` paired with a positive `avail_in` before the
        // input window is bridged. `inflateSync` has no output buffer, so only
        // the input half of the entry contract applies. C `inflateSync` would
        // instead dereference the null pointer (undefined behavior); returning
        // `Z_STREAM_ERROR` is the strictly safer drop-in behavior and matches
        // the "`next_in` non-null whenever `avail_in != 0`" rule.
        if !input_ptr_valid(sref) {
            return Z_STREAM_ERROR;
        }
        // SAFETY: detached-lifetime input window (see `inflate`); valid to hold
        // across the later `&mut` re-borrow for cursor advancement.
        let input: &[u8] = unsafe { input_slice(sref) };
        // Snapshot the caller's `adler` before `handle` takes a mutable borrow
        // of `sref` (see `seed_adler_mirror`).
        let caller_adler = sref.adler;
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        // C's `inflateSync` returns `Z_DATA_ERROR` *before* its closing
        // `inflateReset(strm)` when no sync point is found, leaving
        // `strm->adler` untouched; seed so that case round-trips the caller's
        // value. See `seed_adler_mirror`.
        seed_adler_mirror(&mut handle.zs, caller_adler);
        let (code, consumed) = crate::inflate::inflate_sync(&mut handle.zs, input);
        let adler = handle.zs.adler;
        // C's `inflateSync` finishes with `inflateReset(strm)`, so its only
        // `strm->adler` write is `inflateResetKeep`'s wrap-guarded one
        // (`inflate.c` L108-L109). See `mirror_adler_if_wrapped`.
        let wrap = handle.zs.inflate_state().map_or(0, |s| s.wrap);
        let msg = handle.zs.msg;
        // `handle` borrow ends here.
        // SAFETY: `consumed <= input.len() == avail_in`.
        unsafe { advance_input(sref, consumed) };
        mirror_adler_if_wrapped(sref, wrap, adler);
        set_msg(sref, msg_to_cstr(msg));
        code.as_c_int()
    })
}

/// C `inflateSyncPoint` — returns nonzero (`1`) if `inflate` is currently at a
/// point where a full flush was applied (a synchronization point), `0`
/// otherwise, or `Z_STREAM_ERROR` on an invalid stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateSyncPoint(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_sync_point(&handle.zs) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflatePrime` — insert `bits` bits of `value` into the inflate input
/// stream ahead of the next byte, or drop bits when `bits` is negative. Used to
/// prime the bit buffer when resuming mid-stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflatePrime(strm: z_streamp, bits: c_int, value: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_prime(&mut handle.zs, bits, value) {
            Ok(rc) => rc.as_c_int(),
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

// ===========================================================================
// Phase 4 (cont.) — copy / mark / validate / undermine / query shims
// ===========================================================================

/// C `inflateCopy` — deep-copy an active decompression stream (state, window,
/// and observable fields) from `source` into `dest`. The history window and the
/// state reservation are re-allocated through the **same** `zalloc` as the
/// source (AAP §0.6.5). Returns `Z_MEM_ERROR` if a copy allocation fails — in
/// which case `dest` is left entirely untouched — or `Z_STREAM_ERROR` on invalid
/// arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateCopy(dest: z_streamp, source: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if dest.is_null() || source.is_null() {
            return Z_STREAM_ERROR;
        }

        // Build the destination idiomatic stream carrying the source's allocator
        // hooks. This shared read completes before we take the `&mut` borrow of
        // `source` below, avoiding aliasing.
        // SAFETY: `source` is non-null and a valid `z_stream`; reads its alloc
        // fields only.
        let source_alloc = unsafe { CAllocator::from_stream(&*source) };
        // C reaches the same verdict through `inflateStateCheck(source)`, which
        // rejects a source whose `zalloc` or `zfree` is null (`inflate.c` L90-L91,
        // called from `inflateCopy` before its `ZALLOC(source, …)`). A stream this
        // crate initialized can only carry both hooks or neither, so this can fire
        // only if the fields were mutated after init; rejecting keeps the copy from
        // silently allocating out of the global heap while the caller believes
        // their hook owns the memory.
        if source_alloc.is_half_present() {
            return Z_STREAM_ERROR;
        }
        let mut dest_zs = ZStream::with_allocator(source_alloc);

        // SAFETY: `source` is non-null and a valid `z_stream`.
        let src = unsafe { &mut *source };
        // SAFETY: `source.state`, if non-null, is the `Box<InflateHandle>`.
        let src_handle = match unsafe { inflate_handle(src) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };

        let copy_res = crate::inflate::inflate_copy(&mut dest_zs, &src_handle.zs);
        #[cfg(feature = "gzip")]
        let src_head = src_handle.head;
        // `src_handle`/`src` borrows end here (last use above).

        match copy_res {
            Ok(_) => {
                // `handle` is mutated only under `gzip` (to carry the source's
                // gzip header); when `gzip` is disabled the `mut` is genuinely
                // unused, so suppress the lint in exactly that configuration.
                #[cfg_attr(not(feature = "gzip"), allow(unused_mut))]
                let mut handle = InflateHandle::new(dest_zs);
                #[cfg(feature = "gzip")]
                {
                    handle.head = src_head;
                }
                // Box the cloned handle **fallibly** before writing any field of
                // `dest`, so heap exhaustion yields `Z_MEM_ERROR` with `dest`
                // untouched — matching C's `ZFREE(copy); return Z_MEM_ERROR`
                // (`inflate.c` L1343-L1349). The dropped handle releases the
                // cloned window and state reservation through the caller's
                // `zfree`.
                let Some(boxed) = try_box(handle) else {
                    return Z_MEM_ERROR;
                };
                // SAFETY: `dest` is non-null and a valid `z_stream`.
                let dref = unsafe { &mut *dest };
                // SAFETY: transfers ownership of the cloned handle into
                // `dest.state`; reclaimed by `inflateEnd`.
                dref.state = unsafe { state_ptr_from_box(boxed) };

                // Mirror zlib's `zmemcpy(dest, source, sizeof(z_stream))` for the
                // observable fields (the `state` pointer was just set above).
                // SAFETY: `source` is a valid `z_stream`; the earlier `&mut`
                // borrow has ended, so this shared read does not alias.
                let sref = unsafe { &*source };
                dref.next_in = sref.next_in;
                dref.avail_in = sref.avail_in;
                dref.total_in = sref.total_in;
                dref.next_out = sref.next_out;
                dref.avail_out = sref.avail_out;
                dref.total_out = sref.total_out;
                dref.msg = sref.msg;
                dref.zalloc = sref.zalloc;
                dref.zfree = sref.zfree;
                dref.opaque = sref.opaque;
                dref.data_type = sref.data_type;
                dref.adler = sref.adler;
                dref.reserved = sref.reserved;
                Z_OK
            }
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateMark` — return a value describing the current decode location, for
/// random-access use. The high bits encode how many bytes were used from the
/// input, the low 16 bits how many bits beyond that. Returns the sentinel
/// `-(1 << 16)` when the stream state is unusable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateMark(strm: z_streamp) -> c_long {
    guard_long(INFLATE_MARK_ERR, move || {
        if strm.is_null() {
            return INFLATE_MARK_ERR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        match unsafe { inflate_handle(sref) } {
            Some(h) => crate::inflate::inflate_mark(&h.zs) as c_long,
            None => INFLATE_MARK_ERR,
        }
    })
}

/// C `inflateValidate` — enable (`check != 0`) or disable check-value
/// (CRC/Adler) verification for the stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateValidate(strm: z_streamp, check: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_validate(&mut handle.zs, check != 0) {
            Ok(rc) => rc.as_c_int(),
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateUndermine` — request tolerance of invalid check values. The
/// realized engine does not implement the (compile-time-gated) leniency, so it
/// mirrors zlib's default build by reporting `Z_DATA_ERROR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateUndermine(strm: z_streamp, subvert: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_undermine(&mut handle.zs, subvert) {
            Ok(rc) => rc.as_c_int(),
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateCodesUsed` — return the number of inflate decode-table entries in
/// use, primarily for diagnostics. Returns `(unsigned long)-1` (`c_ulong::MAX`)
/// when the stream state is unusable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateCodesUsed(strm: z_streamp) -> c_ulong {
    guard_ulong(c_ulong::MAX, move || {
        if strm.is_null() {
            return c_ulong::MAX;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        match unsafe { inflate_handle(sref) } {
            Some(h) => match crate::inflate::inflate_codes_used(&h.zs) {
                Some(n) => n as c_ulong,
                None => c_ulong::MAX,
            },
            None => c_ulong::MAX,
        }
    })
}

// ===========================================================================
// Phase 4 (cont.) — gzip header extraction
// ===========================================================================

/// C `inflateGetHeader` — register the caller's `gz_header` so that, while
/// inflating a gzip stream, the header fields (and optionally the extra field,
/// file name, and comment, bounded by the caller's `extra_max`/`name_max`/
/// `comm_max`) are captured into it.
///
/// The caller's `extra`/`name`/`comment` are **output** buffers here; we build a
/// fresh idiomatic [`GzHeader`] that pre-allocates capture vectors only for the
/// non-null buffers, register it with the engine, and remember the raw pointer
/// so [`inflate`] can mirror the captured fields back after each call.
///
/// Returns `Z_STREAM_ERROR` if the stream is not decoding gzip framing.
#[cfg(feature = "gzip")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateGetHeader(strm: z_streamp, head: gz_headerp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };

        // Construct the idiomatic header describing what to capture. We must not
        // use `gz_header_to_idiomatic` here: that reads `extra`/`name`/`comment`
        // as *inputs* (the `deflateSetHeader` direction), whereas here they are
        // caller output buffers. The engine captures a field only when its
        // idiomatic slot is pre-set to `Some(Vec)`, bounded by the `*_max`.
        let gh = if head.is_null() {
            GzHeader::default()
        } else {
            // SAFETY: `head` is non-null and a valid `gz_header`; we read its
            // buffer pointers (to test for null) and capacity fields.
            let raw = unsafe { &*head };
            GzHeader {
                extra: if raw.extra.is_null() {
                    None
                } else {
                    Some(alloc::vec::Vec::new())
                },
                name: if raw.name.is_null() {
                    None
                } else {
                    Some(alloc::vec::Vec::new())
                },
                comment: if raw.comment.is_null() {
                    None
                } else {
                    Some(alloc::vec::Vec::new())
                },
                // Seed the caller's own `extra_len` rather than defaulting it to
                // zero. C only assigns `head->extra_len` when the header actually
                // carries an `FEXTRA` field (`inflate.c` L596-L600); for a stream
                // without one it leaves the caller's field exactly as it found it
                // (`inflate.c` L605-L606 nulls `extra`, never `extra_len`).
                // Carrying the value in means the unconditional writeback in
                // `write_gz_header_from_idiomatic` reproduces both cases: a
                // declared `XLEN` overwrites it, and no `FEXTRA` leaves it alone.
                extra_len: raw.extra_len,
                extra_max: raw.extra_max,
                name_max: raw.name_max,
                comm_max: raw.comm_max,
                ..GzHeader::default()
            }
        };

        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_get_header(&mut handle.zs, gh) {
            Ok(_) => {
                handle.head = head;
                Z_OK
            }
            Err(e) => ReturnCode::from(e).as_c_int(),
        }
    })
}

/// C `inflateGetHeader` — gzip framing compiled out. Mirrors zlib's behavior of
/// rejecting the request for a non-gzip stream with `Z_STREAM_ERROR`.
#[cfg(not(feature = "gzip"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateGetHeader(strm: z_streamp, head: gz_headerp) -> c_int {
    let _ = head;
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        Z_STREAM_ERROR
    })
}

// ===========================================================================
// Phase 5 — inflateBack (raw-callback decode)
// ===========================================================================

/// C `inflateBack` — decompress a raw DEFLATE stream, pulling input via the
/// `in` callback and pushing output via the `out` callback, using the window
/// allocated by [`inflateBackInit_`].
///
/// The C callbacks are wrapped into `CInFunc`/`COutFunc` and handed to the
/// safe engine. Any already-buffered input in the stream (`next_in`/`avail_in`)
/// is presented to the engine first, then the `in` callback is invoked for more.
///
/// Returns `Z_STREAM_END` on success, `Z_DATA_ERROR`/`Z_BUF_ERROR`/
/// `Z_MEM_ERROR` on decode failures, or `Z_STREAM_ERROR` for invalid arguments
/// (including a null `in`/`out` callback).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateBack(
    strm: z_streamp,
    in_: in_func,
    in_desc: *mut c_void,
    out: out_func,
    out_desc: *mut c_void,
) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // Both callbacks are required; a null either way is a usage error.
        let in_fn = match in_ {
            Some(f) => f,
            None => return Z_STREAM_ERROR,
        };
        let out_fn = match out {
            Some(f) => f,
            None => return Z_STREAM_ERROR,
        };

        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // SAFETY: detached-lifetime input window (see `inflate`); it points at the
        // caller's external buffer, so it stays valid across the later `&mut`
        // re-borrow of the stream for cursor restoration.
        let initial: &[u8] = unsafe { input_slice(sref) };

        // C4: fetch the tagged `inflateBack` handle, validating the kind before
        // touching the engine state. A missing handle, or one owned by the
        // deflate/inflate engines, is rejected with `Z_STREAM_ERROR`.
        // SAFETY: `state`, if non-null, is a live tagged handle from an init shim.
        let handle = match unsafe { inflate_back_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        let state = &mut *handle.inner;

        let mut src = CInFunc {
            in_fn,
            in_desc,
            initial,
            initial_done: false,
            last_ptr: initial.as_ptr(),
            last_len: initial.len(),
            handed_out: false,
            dry: false,
            unconsumed: 0,
        };
        let mut sink = COutFunc { out_fn, out_desc };

        let code = crate::inflate::back::inflate_back(state, &mut src, &mut sink);
        // `state`/`handle` borrows of `sref` end here; `src` is a local that only
        // borrows the detached-lifetime `initial`, so it stays readable below.

        // C3: restore `next_in`/`avail_in` exactly like C `infback.c`'s `inf_leave`
        // (L561-L569 sets `strm->next_in = next; strm->avail_in = have;`), which
        // does NOT touch `total_in`/`total_out`. `next`/`have` track the LAST
        // buffer the engine pulled from — the initial stream buffer or a callback
        // buffer — at the unconsumed offset. A callback that ran dry leaves
        // `next = Z_NULL`/`have = 0` (the C `PULL`-returns-0 path).
        if src.dry {
            sref.next_in = ptr::null();
            sref.avail_in = 0;
        } else if src.handed_out {
            // `unconsumed <= last_len` by construction; `consumed` is the offset
            // of the unconsumed tail within the last chunk.
            let consumed = src.last_len - src.unconsumed;
            // SAFETY: `last_ptr` is the base of the last chunk of `last_len`
            // readable bytes; `consumed <= last_len`, so the offset is in-bounds
            // (one-past-the-end is permitted when fully consumed, matching C's
            // `next` pointer).
            sref.next_in = unsafe { src.last_ptr.add(consumed) };
            sref.avail_in = src.unconsumed as c_uint;
        }
        // else: the engine never pulled any input (e.g. it rejected an invalid
        // state before reading). C returns without loading `next`/`have`, so the
        // caller's cursors are left untouched.

        code.as_c_int()
    })
}

/// C `inflateBackEnd` — free the state allocated by [`inflateBackInit_`]. The
/// boxed `InflateState` is reclaimed and passed to the engine's terminator
/// (which validates it), then dropped (RAII frees the window).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflateBackEnd(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };
        // C4: validate the `INFLATE_BACK` tag before reclaiming. `inflate_back_take`
        // reconstitutes the `Box<InflateBackHandle>` ONLY when the leading tag
        // matches, so a stream owned by the other engines — a tagged
        // `DeflateHandle`/`InflateHandle` — or an already-freed handle is rejected
        // with `Z_STREAM_ERROR` and nothing is taken or dropped (no layout-
        // mismatched free).
        // SAFETY: `state`, if non-null, is a live tagged handle from an init shim.
        match unsafe { inflate_back_take(sref) } {
            Some(handle) => {
                // Hand the owned engine state to the terminator (which validates
                // it); RAII then frees the window when `handle`/`inner` drop.
                crate::inflate::back::inflate_back_end(handle.inner).as_c_int()
            }
            None => Z_STREAM_ERROR,
        }
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::ffi::CStr;

    /// A version string whose first byte is `'1'` (all `version_check` inspects).
    const VERSION: &CStr = c"1.3.2.1-motley";
    /// C `Z_NO_FLUSH`.
    const Z_NO_FLUSH: c_int = 0;
    /// C `Z_STREAM_END`.
    const Z_STREAM_END: c_int = ReturnCode::StreamEnd.as_c_int();

    /// The plaintext that every compressed test vector decodes to.
    const MSG: &[u8] = b"hello, hello, hello, world!";

    // zlib-wrapped (RFC 1950) stream for `MSG`.
    const ZLIB_STREAM: &[u8] = &[
        120, 156, 203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243, 139, 114, 82, 20, 1,
        133, 250, 9, 106,
    ];
    // Raw DEFLATE (RFC 1951) stream for `MSG`.
    const RAW_STREAM: &[u8] = &[
        203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243, 139, 114, 82, 20, 1,
    ];
    // gzip-wrapped (RFC 1952) stream for `MSG` (no name/comment).
    // Only the gzip/auto-detect tests consume this vector, so it is gated on
    // the `gzip` feature to stay dead-code-free under `--no-default-features`.
    #[cfg(feature = "gzip")]
    const GZIP_STREAM: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243,
        139, 114, 82, 20, 1, 131, 137, 31, 110, 27, 0, 0, 0,
    ];
    // gzip stream for `MSG` carrying FNAME="name.txt" and FCOMMENT="a comment".
    #[cfg(feature = "gzip")]
    const GZIP_NAMED: &[u8] = &[
        31, 139, 8, 24, 0, 0, 0, 0, 0, 3, 110, 97, 109, 101, 46, 116, 120, 116, 0, 97, 32, 99, 111,
        109, 109, 101, 110, 116, 0, 203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243, 139,
        114, 82, 20, 1, 131, 137, 31, 110, 27, 0, 0, 0,
    ];

    /// An all-zero/null `z_stream`, as a C caller would `memset` before init.
    fn zeroed_stream() -> z_stream {
        z_stream {
            next_in: ptr::null(),
            avail_in: 0,
            total_in: 0,
            next_out: ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: ptr::null_mut(),
            state: ptr::null_mut(),
            zalloc: None,
            zfree: None,
            opaque: ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        }
    }

    /// Initialize with `window_bits`, decompress `input` in one shot, and return
    /// `(return_code, decompressed_bytes)`.
    fn inflate_once(window_bits: c_int, input: &[u8]) -> (c_int, Vec<u8>) {
        let mut strm = zeroed_stream();
        let rc = unsafe {
            inflateInit2_(
                &mut strm,
                window_bits,
                VERSION.as_ptr(),
                size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(rc, Z_OK, "inflateInit2_ failed");

        let mut out = vec![0u8; 512];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
        let produced = out.len() - strm.avail_out as usize;
        out.truncate(produced);

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK, "inflateEnd failed");
        (rc, out)
    }

    #[test]
    fn round_trip_zlib() {
        let (rc, out) = inflate_once(15, ZLIB_STREAM);
        assert_eq!(rc, Z_STREAM_END);
        assert_eq!(out, MSG);
    }

    #[test]
    fn round_trip_raw() {
        // Raw DEFLATE selected by a negative windowBits.
        let (rc, out) = inflate_once(-15, RAW_STREAM);
        assert_eq!(rc, Z_STREAM_END);
        assert_eq!(out, MSG);
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn round_trip_gzip() {
        // gzip framing selected by 16 + windowBits.
        let (rc, out) = inflate_once(15 + 16, GZIP_STREAM);
        assert_eq!(rc, Z_STREAM_END);
        assert_eq!(out, MSG);
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn auto_detect_decodes_zlib_and_gzip() {
        // 32 + windowBits enables zlib/gzip auto-detection.
        let (rc_z, out_z) = inflate_once(15 + 32, ZLIB_STREAM);
        assert_eq!(rc_z, Z_STREAM_END);
        assert_eq!(out_z, MSG);

        let (rc_g, out_g) = inflate_once(15 + 32, GZIP_STREAM);
        assert_eq!(rc_g, Z_STREAM_END);
        assert_eq!(out_g, MSG);
    }

    #[test]
    fn version_and_size_mismatch_rejected() {
        let mut strm = zeroed_stream();
        // Wrong reported sizeof(z_stream).
        let rc = unsafe {
            inflateInit2_(
                &mut strm,
                15,
                VERSION.as_ptr(),
                (size_of::<z_stream>() - 1) as c_int,
            )
        };
        assert_eq!(rc, Z_VERSION_ERROR);

        // Null version string.
        let rc =
            unsafe { inflateInit2_(&mut strm, 15, ptr::null(), size_of::<z_stream>() as c_int) };
        assert_eq!(rc, Z_VERSION_ERROR);
    }

    #[test]
    fn null_stream_is_stream_error() {
        assert_eq!(
            unsafe { inflate(ptr::null_mut(), Z_NO_FLUSH) },
            Z_STREAM_ERROR
        );
        assert_eq!(unsafe { inflateEnd(ptr::null_mut()) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { inflateReset(ptr::null_mut()) }, Z_STREAM_ERROR);
    }

    #[test]
    fn inflate_rejects_invalid_raw_buffers() {
        // C `inflate` entry validation (`inflate.c` L474): a null `next_out`
        // (regardless of `avail_out`), or a positive `avail_in` paired with a
        // null `next_in`, is `Z_STREAM_ERROR`. An *initialized* stream is used
        // so the rejection is attributable to buffer validation, not the state
        // check.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        let mut out = vec![0u8; 64];

        // Null `next_out` with nonzero `avail_out` -> Z_STREAM_ERROR.
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = ptr::null_mut();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_ERROR);

        // Nonzero `avail_in` with null `next_in` -> Z_STREAM_ERROR.
        strm.next_in = ptr::null();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_ERROR);

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn inflate_sync_rejects_null_input_with_availability() {
        // `inflateSync` has no output buffer, so only the input half of the
        // entry contract applies: a positive `avail_in` with a null `next_in`
        // is `Z_STREAM_ERROR` (C would instead dereference the null pointer).
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        strm.next_in = ptr::null();
        strm.avail_in = 8;
        assert_eq!(unsafe { inflateSync(&mut strm) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn mark_and_codes_used_null_sentinels() {
        // inflateMark returns -(1 << 16) on a null/invalid stream.
        assert_eq!(unsafe { inflateMark(ptr::null_mut()) }, -(1 << 16));
        // inflateCodesUsed returns (unsigned long)-1 on a null/invalid stream.
        assert_eq!(unsafe { inflateCodesUsed(ptr::null_mut()) }, c_ulong::MAX);
    }

    #[test]
    fn reset_then_reinflate() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        // First decode.
        let mut out = vec![0u8; 512];
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);

        // Reset and decode the same stream again.
        assert_eq!(unsafe { inflateReset(&mut strm) }, Z_OK);
        assert_eq!(strm.total_out, 0);
        let mut out2 = vec![0u8; 512];
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out2.as_mut_ptr();
        strm.avail_out = out2.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
        let produced = out2.len() - strm.avail_out as usize;
        assert_eq!(&out2[..produced], MSG);

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    // -- inflateGetHeader ---------------------------------------------------

    #[cfg(feature = "gzip")]
    fn zeroed_gz_header() -> gz_header {
        gz_header {
            text: 0,
            time: 0,
            xflags: 0,
            os: 0,
            extra: ptr::null_mut(),
            extra_len: 0,
            extra_max: 0,
            name: ptr::null_mut(),
            name_max: 0,
            comment: ptr::null_mut(),
            comm_max: 0,
            hcrc: 0,
            done: 0,
        }
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_fills_name_and_comment() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15 + 16,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        let mut namebuf = [0u8; 64];
        let mut commbuf = [0u8; 64];
        let mut head = zeroed_gz_header();
        head.name = namebuf.as_mut_ptr();
        head.name_max = namebuf.len() as c_uint;
        head.comment = commbuf.as_mut_ptr();
        head.comm_max = commbuf.len() as c_uint;

        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut head) }, Z_OK);

        let mut out = vec![0u8; 256];
        strm.next_in = GZIP_NAMED.as_ptr();
        strm.avail_in = GZIP_NAMED.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);

        // The name/comment C strings were written NUL-terminated into the buffers.
        let name = CStr::from_bytes_until_nul(&namebuf).unwrap().to_bytes();
        let comment = CStr::from_bytes_until_nul(&commbuf).unwrap().to_bytes();
        assert_eq!(name, b"name.txt");
        assert_eq!(comment, b"a comment");

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    #[cfg(feature = "gzip")]
    /// Builds a real gzip member carrying an `FEXTRA` field of `extra`, produced
    /// by this crate's own encoder through `deflateSetHeader`, so the test
    /// exercises the write path as well as the read path.
    fn gzip_stream_with_extra(extra: &[u8], payload: &[u8]) -> Vec<u8> {
        use crate::constants::{Z_DEFAULT_STRATEGY, Z_DEFLATED, Z_FINISH};
        use crate::ffi::deflate::{deflate, deflateEnd, deflateInit2_, deflateSetHeader};

        let mut d = zeroed_stream();
        assert_eq!(
            unsafe {
                deflateInit2_(
                    &mut d,
                    6,
                    Z_DEFLATED,
                    31,
                    8,
                    Z_DEFAULT_STRATEGY,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        let mut extra_buf = extra.to_vec();
        let mut head = zeroed_gz_header();
        head.extra = extra_buf.as_mut_ptr();
        head.extra_len = extra_buf.len() as c_uint;
        assert_eq!(unsafe { deflateSetHeader(&mut d, &mut head) }, Z_OK);

        let mut out = vec![0u8; payload.len() + 256];
        d.next_in = payload.as_ptr();
        d.avail_in = payload.len() as c_uint;
        d.next_out = out.as_mut_ptr();
        d.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut d, Z_FINISH) }, Z_STREAM_END);
        let produced = out.len() - d.avail_out as usize;
        out.truncate(produced);
        assert_eq!(unsafe { deflateEnd(&mut d) }, Z_OK);
        out
    }

    /// QA-04 regression — the extra field's **declared** length must survive a
    /// truncating copy.
    ///
    /// `zlib.h` specifies that once `done` is true, `extra_len` holds the *actual*
    /// extra field length and `extra` holds that field "or that field truncated if
    /// `extra_max` is less than `extra_len`". C writes `head->extra_len` from the
    /// header's 16-bit `XLEN` (`inflate.c` L599-L600) and clamps only the copy
    /// (L614-L621), so `extra_len > extra_max` is the caller's sole truncation
    /// signal. Reporting the copied count instead would make truncation — silent
    /// data loss — undetectable.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_reports_declared_extra_len_when_the_copy_is_truncated() {
        const EXTRA: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let stream = gzip_stream_with_extra(EXTRA, b"payload for the truncation case");

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    31,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // A deliberately undersized buffer: 4 bytes for a 10-byte field.
        let mut extrabuf = [0u8; 4];
        let mut head = zeroed_gz_header();
        head.extra = extrabuf.as_mut_ptr();
        head.extra_max = extrabuf.len() as c_uint;
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut head) }, Z_OK);

        let mut out = vec![0u8; 256];
        strm.next_in = stream.as_ptr();
        strm.avail_in = stream.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);

        assert_eq!(head.done, 1, "the gzip header must be fully parsed");
        assert_eq!(
            head.extra_len, 10,
            "extra_len must report the DECLARED length (10), not the copied count (4)"
        );
        assert!(
            head.extra_len > head.extra_max,
            "extra_len > extra_max is the caller's only truncation signal"
        );
        assert_eq!(
            extrabuf,
            [1, 2, 3, 4],
            "the copy itself stays clamped to extra_max, byte-identically to C"
        );

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// The documented length-query pattern: leave `extra` null purely to learn the
    /// extra field's length. C writes `head->extra_len` in the `EXLEN` state
    /// (`inflate.c` L599-L600) gated on neither `extra`'s nullity nor `extra_max`,
    /// so a null buffer must still yield the real length.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_reports_extra_len_for_a_null_extra_length_query() {
        const EXTRA: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let stream = gzip_stream_with_extra(EXTRA, b"payload for the length query");

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    31,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // extra == NULL, extra_max == 0: a pure length query.
        let mut head = zeroed_gz_header();
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut head) }, Z_OK);

        let mut out = vec![0u8; 256];
        strm.next_in = stream.as_ptr();
        strm.avail_in = stream.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);

        assert_eq!(head.done, 1);
        assert_eq!(
            head.extra_len, 10,
            "a null extra buffer must still report the declared length"
        );

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// A stream with **no** `FEXTRA` field must leave the caller's `extra_len`
    /// exactly as it found it. C only assigns the field inside the
    /// `flags & 0x0400` branch (`inflate.c` L596-L600); the no-`FEXTRA` path nulls
    /// `head->extra` and never touches `extra_len` (L605-L606). Verified against
    /// reference C, which likewise preserves a poisoned value.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_leaves_extra_len_untouched_when_the_stream_has_no_extra_field() {
        const POISON: c_uint = 0xBEEF;

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    31,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        let mut namebuf = [0u8; 64];
        let mut extrabuf = [0u8; 32];
        let mut head = zeroed_gz_header();
        head.name = namebuf.as_mut_ptr();
        head.name_max = namebuf.len() as c_uint;
        // A real (non-null) extra buffer, so C's nulling of the pointer on the
        // no-`FEXTRA` branch is actually observable.
        head.extra = extrabuf.as_mut_ptr();
        head.extra_max = extrabuf.len() as c_uint;
        head.extra_len = POISON;
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut head) }, Z_OK);

        // GZIP_NAMED carries FNAME and FCOMMENT but no FEXTRA.
        let mut out = vec![0u8; 256];
        strm.next_in = GZIP_NAMED.as_ptr();
        strm.avail_in = GZIP_NAMED.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);

        assert_eq!(head.done, 1);
        assert_eq!(
            head.extra_len, POISON,
            "a stream without an FEXTRA field must not clobber the caller's extra_len"
        );
        assert!(
            head.extra.is_null(),
            "C nulls head->extra when the header declares no FEXTRA field \
             (`inflate.c` L605-L606), which is how a caller tells an absent \
             extra field from a present one"
        );

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    // -- inflateBack --------------------------------------------------------

    /// Input source for `inflateBack`: hands over `data` once, then signals EOF.
    struct BackIn {
        data: &'static [u8],
        given: bool,
    }

    unsafe extern "C" fn back_in(desc: *mut c_void, buf: *mut *const c_uchar) -> c_uint {
        // SAFETY: `desc` is the `*mut BackIn` handed to `inflateBack` below.
        let st = unsafe { &mut *(desc as *mut BackIn) };
        if st.given {
            return 0;
        }
        st.given = true;
        // SAFETY: `buf` is a valid out-pointer per the `in_func` contract.
        unsafe { *buf = st.data.as_ptr() };
        st.data.len() as c_uint
    }

    /// Output sink for `inflateBack`: accumulates all produced bytes.
    struct BackOut {
        collected: Vec<u8>,
    }

    unsafe extern "C" fn back_out(desc: *mut c_void, buf: *mut c_uchar, len: c_uint) -> c_int {
        // SAFETY: `desc` is the `*mut BackOut` handed to `inflateBack` below.
        let st = unsafe { &mut *(desc as *mut BackOut) };
        // SAFETY: `buf` points to `len` readable bytes per the `out_func` contract.
        let slice = unsafe { slice::from_raw_parts(buf, len as usize) };
        st.collected.extend_from_slice(slice);
        0
    }

    #[test]
    fn inflate_back_end_to_end() {
        let mut strm = zeroed_stream();
        let mut window = [0u8; 1 << 15];
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // All input arrives via the `in` callback (nothing pre-buffered).
        let mut in_state = BackIn {
            data: RAW_STREAM,
            given: false,
        };
        let mut out_state = BackOut {
            collected: Vec::new(),
        };

        let rc = unsafe {
            inflateBack(
                &mut strm,
                Some(back_in),
                &mut in_state as *mut BackIn as *mut c_void,
                Some(back_out),
                &mut out_state as *mut BackOut as *mut c_void,
            )
        };
        assert_eq!(rc, Z_STREAM_END);
        assert_eq!(out_state.collected, MSG);

        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn inflate_back_null_callbacks_rejected() {
        let mut strm = zeroed_stream();
        let mut window = [0u8; 1 << 15];
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        let rc = unsafe { inflateBack(&mut strm, None, ptr::null_mut(), None, ptr::null_mut()) };
        assert_eq!(rc, Z_STREAM_ERROR);
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    /// C3: `inflateBack` must restore `next_in`/`avail_in` to the unconsumed
    /// tail of the input (C `infback.c` `inf_leave`), leaving trailing bytes that
    /// are not part of the DEFLATE stream visible to the caller — and it must NOT
    /// touch `total_in`/`total_out`.
    #[test]
    fn inflate_back_preserves_unconsumed_trailing_input() {
        let mut strm = zeroed_stream();
        let mut window = [0u8; 1 << 15];
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // Pre-buffer the complete raw stream followed by trailing bytes that are
        // NOT part of the DEFLATE stream (as if the next record began there).
        const TRAILING: &[u8] = &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];
        let mut buf = Vec::with_capacity(RAW_STREAM.len() + TRAILING.len());
        buf.extend_from_slice(RAW_STREAM);
        buf.extend_from_slice(TRAILING);
        let base = buf.as_ptr();
        strm.next_in = base;
        strm.avail_in = buf.len() as c_uint;

        // The callback signals EOF; it must not be needed since the whole stream
        // is already buffered.
        let mut in_state = BackIn {
            data: &[],
            given: true,
        };
        let mut out_state = BackOut {
            collected: Vec::new(),
        };

        let rc = unsafe {
            inflateBack(
                &mut strm,
                Some(back_in),
                &mut in_state as *mut BackIn as *mut c_void,
                Some(back_out),
                &mut out_state as *mut BackOut as *mut c_void,
            )
        };
        assert_eq!(rc, Z_STREAM_END);
        assert_eq!(out_state.collected, MSG);

        // C `inf_leave` does NOT modify total_in/total_out (the old FFI bug bumped
        // total_in via `advance_input`).
        assert_eq!(
            strm.total_in, 0,
            "inflateBack must not modify total_in (C inf_leave)"
        );
        assert_eq!(
            strm.total_out, 0,
            "inflateBack must not modify total_out (C inf_leave)"
        );

        // next_in/avail_in partition the buffer, and the unconsumed tail contains
        // (at least) the trailing bytes — the old bug set avail_in=0.
        assert!(strm.avail_in > 0, "trailing bytes must remain available");
        let consumed = (strm.next_in as usize) - (base as usize);
        assert_eq!(
            consumed + strm.avail_in as usize,
            buf.len(),
            "next_in/avail_in must partition the input buffer"
        );
        // The engine consumes exactly the complete raw stream and leaves the
        // trailing bytes intact — byte-for-byte parity with C `inflateBack`.
        assert_eq!(
            consumed,
            RAW_STREAM.len(),
            "exactly the raw stream must be consumed"
        );
        assert_eq!(
            strm.avail_in as usize,
            TRAILING.len(),
            "avail_in must equal the trailing byte count"
        );
        // SAFETY: next_in/avail_in describe a live sub-slice of `buf`.
        let tail = unsafe { slice::from_raw_parts(strm.next_in, strm.avail_in as usize) };
        assert_eq!(
            tail, TRAILING,
            "the unconsumed tail must be exactly the trailing bytes"
        );

        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    /// C3: when the `in` callback runs dry, `inflateBack` must null `next_in` and
    /// zero `avail_in` (C `PULL` returning 0 sets `next = Z_NULL`, `have = 0`),
    /// even though input was initially buffered.
    #[test]
    fn inflate_back_dry_callback_nulls_cursor() {
        let mut strm = zeroed_stream();
        let mut window = [0u8; 1 << 15];
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // Pre-buffer a truncated (incomplete) stream so the engine must ask the
        // callback for more, which immediately reports EOF.
        let truncated = &RAW_STREAM[..3];
        strm.next_in = truncated.as_ptr();
        strm.avail_in = truncated.len() as c_uint;

        let mut in_state = BackIn {
            data: &[],
            given: true,
        };
        let mut out_state = BackOut {
            collected: Vec::new(),
        };

        let rc = unsafe {
            inflateBack(
                &mut strm,
                Some(back_in),
                &mut in_state as *mut BackIn as *mut c_void,
                Some(back_out),
                &mut out_state as *mut BackOut as *mut c_void,
            )
        };
        // Insufficient input → the callback ran dry → Z_BUF_ERROR.
        assert_eq!(rc, ReturnCode::BufError.as_c_int());
        // C `PULL` returning 0 sets next = Z_NULL / have = 0.
        assert!(
            strm.next_in.is_null(),
            "dry callback must null next_in (C PULL)"
        );
        assert_eq!(strm.avail_in, 0, "dry callback must zero avail_in");
        assert_eq!(strm.total_in, 0, "inflateBack must not modify total_in");

        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    /// F2 (inflate): `inflateBackInit_` must **use the caller's window**, not
    /// allocate a replacement.
    ///
    /// Reference zlib adopts the pointer verbatim (`state->window = window;`,
    /// `infback.c` L60). Three consequences are observable through the C ABI and
    /// all three are asserted here:
    ///
    /// 1. the decoder writes the decompressed data into the caller's buffer, so
    ///    after a successful decode the caller's own array holds the plaintext;
    /// 2. `inflateBackInit_` makes exactly **one** allocation through the caller's
    ///    `zalloc` — the state (`infback.c` L51) — and no window request; and
    /// 3. `inflateBackEnd` releases only that one region (`infback.c` L572-L577),
    ///    leaving the caller's buffer intact and still readable afterwards.
    #[test]
    fn back_init_lends_the_callers_window_and_never_frees_it() {
        use crate::ffi::alloc::test_hook::HookStats;

        let stats = HookStats::new();
        let hook = stats.hook();

        // A heap window, so a stray `zfree` or a double free would be caught by
        // the allocator rather than silently corrupting a stack frame.
        let mut window = vec![0xAAu8; 1 << 15];
        let window_ptr = window.as_mut_ptr();

        let mut strm = zeroed_stream();
        strm.zalloc = hook.zalloc();
        strm.zfree = hook.zfree();
        strm.opaque = hook.opaque();

        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window_ptr,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        assert_eq!(
            stats.allocs(),
            1,
            "inflateBackInit_ makes C's single state allocation and no window \
             request (`infback.c` L51, L60)"
        );
        assert_eq!(stats.frees(), 0);

        // Decode a real stream through the callbacks.
        let mut in_state = BackIn {
            data: RAW_STREAM,
            given: false,
        };
        let mut out_state = BackOut {
            collected: Vec::new(),
        };
        let rc = unsafe {
            inflateBack(
                &mut strm,
                Some(back_in),
                &mut in_state as *mut BackIn as *mut c_void,
                Some(back_out),
                &mut out_state as *mut BackOut as *mut c_void,
            )
        };
        assert_eq!(rc, Z_STREAM_END);
        assert_eq!(out_state.collected, MSG);
        assert_eq!(
            stats.allocs(),
            1,
            "decoding must not allocate: the window was already provided"
        );

        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
        assert_eq!(
            stats.frees(),
            1,
            "inflateBackEnd frees only the state, never the caller's window \
             (`infback.c` L572-L577)"
        );
        assert_eq!(
            stats.live_bytes(),
            0,
            "the caller's arena must be perfectly balanced"
        );

        // The caller still owns and can read its buffer, and it holds the decoded
        // bytes — proof the engine used this region rather than a private copy.
        assert_eq!(
            &window[..MSG.len()],
            MSG,
            "the decoder must write through the caller's window"
        );
    }

    /// A null window must be rejected before anything is allocated or installed,
    /// exactly as C rejects it (`infback.c` L33-L35) — and since the borrowed
    /// -window bridge dereferences that pointer, the check has to happen first.
    ///
    /// Nothing is installed, so `inflateBackEnd` then reports an uninitialized
    /// stream.
    #[test]
    fn back_init_rejects_a_null_window() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    ptr::null_mut(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_STREAM_ERROR,
            "a null window is C's Z_STREAM_ERROR (`infback.c` L33-L35)"
        );
        assert!(strm.state.is_null());
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_STREAM_ERROR);
    }

    /// A budget carried through the caller's `opaque` cookie so each test drives
    /// its own allocator with no shared global state — which is precisely what
    /// zlib's `opaque` field is for.
    struct Budget {
        /// Allocations still permitted before `zalloc` reports out-of-memory.
        remaining: core::sync::atomic::AtomicUsize,
    }

    /// Bytes reserved ahead of each payload so `zfree` can recover the size.
    const BUDGET_HDR: usize = 16;

    /// An honest allocator that really allocates and frees, but only while the
    /// budget in `opaque` lasts — then reports OOM as a C `zalloc` does under
    /// memory pressure.
    unsafe extern "C" fn budget_zalloc(
        opaque: *mut c_void,
        items: c_uint,
        size: c_uint,
    ) -> *mut c_void {
        // SAFETY: every stream in these tests sets `opaque` to a live `&Budget`
        // that outlives the stream, and zlib forwards the cookie unchanged.
        let budget = unsafe { &*(opaque as *const Budget) };
        if budget
            .remaining
            .fetch_update(
                core::sync::atomic::Ordering::SeqCst,
                core::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_err()
        {
            return ptr::null_mut();
        }

        let bytes = (items as usize) * (size as usize);
        let layout = alloc::alloc::Layout::from_size_align(BUDGET_HDR + bytes, BUDGET_HDR)
            .expect("test layout is valid");
        // SAFETY: `layout` has a non-zero size (`BUDGET_HDR` is 16).
        let base = unsafe { alloc::alloc::alloc(layout) };
        if base.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `base` addresses `BUDGET_HDR + bytes` writable, 16-byte aligned
        // bytes, so the header write is aligned and in bounds.
        unsafe {
            base.cast::<usize>().write(bytes);
            base.add(BUDGET_HDR).cast::<c_void>()
        }
    }

    unsafe extern "C" fn budget_zfree(_opaque: *mut c_void, address: *mut c_void) {
        if address.is_null() {
            return;
        }
        // SAFETY: `address` is a payload pointer from `budget_zalloc`, so its size
        // header sits `BUDGET_HDR` bytes below it and the block is live.
        unsafe {
            let base = address.cast::<u8>().sub(BUDGET_HDR);
            let bytes = base.cast::<usize>().read();
            let layout = alloc::alloc::Layout::from_size_align(BUDGET_HDR + bytes, BUDGET_HDR)
                .expect("test layout is valid");
            alloc::alloc::dealloc(base, layout);
        }
    }

    /// Installs the budgeted allocator on `strm`, pointing `opaque` at `budget`.
    fn attach_budget(strm: &mut z_stream, budget: &Budget) {
        strm.zalloc = Some(budget_zalloc);
        strm.zfree = Some(budget_zfree);
        strm.opaque = (budget as *const Budget).cast_mut().cast::<c_void>();
    }

    // =======================================================================
    // `strm->adler` / `strm->msg` / total-bookkeeping lifecycle parity
    //
    // C assigns `strm->adler` at exactly seven places in `inflate.c` (L109,
    // L550, L690, L696, L705, L1079, L1145) and at **zero** places in
    // `infback.c`. Every one of them is unreachable when `state->wrap == 0`, and
    // on a wrapped stream that fails inside its header none of them is reached
    // either. A C caller may therefore keep its own value in the field, and
    // these tests pin that contract at the FFI boundary.
    // =======================================================================

    /// A recognizable value no checksum this crate computes would produce.
    const POISON: c_ulong = 0xAAAA_5555;

    /// `inflateInit2_` on a raw stream must not publish anything into
    /// `strm->adler`: C reaches the field only through `inflateResetKeep`'s
    /// `if (state->wrap)` guard (`inflate.c` L108-L109).
    #[test]
    fn raw_init_and_resets_leave_the_callers_adler_untouched() {
        let mut strm = zeroed_stream();
        strm.adler = POISON;
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    -15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );
        assert_eq!(strm.adler, POISON, "raw inflateInit2_ clobbered adler");

        // Every reset entry point funnels into C's wrap-guarded write.
        for (name, rc) in [
            ("inflateReset", unsafe { inflateReset(&mut strm) }),
            ("inflateResetKeep", unsafe { inflateResetKeep(&mut strm) }),
            ("inflateReset2", unsafe { inflateReset2(&mut strm, -15) }),
        ] {
            assert_eq!(rc, Z_OK, "{name} failed");
            assert_eq!(strm.adler, POISON, "{name} clobbered adler on a raw stream");
        }

        // A full raw decode still must not touch it (`wrap & 4` is clear).
        let mut out = vec![0u8; 512];
        strm.next_in = RAW_STREAM.as_ptr();
        strm.avail_in = RAW_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(
            unsafe { inflate(&mut strm, crate::constants::Z_FINISH) },
            Z_STREAM_END
        );
        let produced = out.len() - strm.avail_out as usize;
        assert_eq!(&out[..produced], MSG, "raw decode produced wrong bytes");
        assert_eq!(strm.adler, POISON, "raw decode clobbered adler");
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// A **wrapped** stream that fails inside its header reaches `BAD` before C's
    /// L550/L690 writes, so the caller's value must survive there too. This is
    /// the case a `wrap != 0` gate alone would miss.
    #[test]
    fn a_wrapped_header_error_leaves_the_callers_adler_untouched() {
        // CM=8, CINFO=8 => a 16-bit window (> MAX_WBITS); FCHECK is valid, so the
        // decoder gets far enough to reject the window size specifically.
        const BAD_WINDOW: [u8; 2] = [0x88, 0x1C];

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );
        // Poison AFTER init, exactly as a caller reusing the field would.
        strm.adler = POISON;

        let mut out = vec![0u8; 64];
        strm.next_in = BAD_WINDOW.as_ptr();
        strm.avail_in = BAD_WINDOW.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            rc,
            ReturnCode::DataError.as_c_int(),
            "expected Z_DATA_ERROR"
        );
        assert_eq!(
            strm.adler, POISON,
            "a header error clobbered the caller's adler",
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// `inflateBackInit_` must leave `strm->adler` alone entirely: `infback.c`
    /// never assigns the field over the whole `inflateBack*` lifetime.
    #[test]
    fn back_init_leaves_the_callers_adler_untouched() {
        let mut window = vec![0u8; 1 << 15];
        let mut strm = zeroed_stream();
        strm.adler = POISON;
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );
        assert_eq!(
            strm.adler, POISON,
            "inflateBackInit_ wrote adler, but infback.c never does",
        );
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    /// The success paths must still publish the real check value: a zlib stream
    /// reports its Adler-32 (C L1145 with `wrap & 4` set) and a valid zlib header
    /// first seeds `adler32(0, Z_NULL, 0) == 1` (C L550).
    #[test]
    fn successful_zlib_decode_publishes_the_adler32() {
        let mut strm = zeroed_stream();
        strm.adler = POISON;
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );
        // A wrapped init *does* write the field (C L108-L109: `wrap & 1` == 1).
        assert_eq!(strm.adler, 1, "wrapped init must seed adler to wrap & 1");

        let mut out = vec![0u8; 512];
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(
            unsafe { inflate(&mut strm, crate::constants::Z_FINISH) },
            Z_STREAM_END
        );

        let expected = crate::checksum::adler32::adler32(1, MSG);
        assert_eq!(
            strm.adler as u32, expected,
            "zlib decode must publish the payload Adler-32",
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// `inflateEnd` must not touch `strm->msg`. C's `inflateEnd`
    /// (`inflate.c` L1155-L1165) frees the window and state and returns — the
    /// canonical zlib idiom of ending the stream and *then* reporting `strm.msg`
    /// (as `test/example.c` and `examples/zpipe.c` do) depends on it.
    #[test]
    fn inflate_end_preserves_the_error_message() {
        // Truncated/garbage stored-block header => "invalid stored block lengths".
        const BAD_BLOCK: [u8; 5] = [0x00, 0x01, 0x00, 0x01, 0x00];

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    -15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );
        let mut out = vec![0u8; 64];
        strm.next_in = BAD_BLOCK.as_ptr();
        strm.avail_in = BAD_BLOCK.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(
            unsafe { inflate(&mut strm, Z_NO_FLUSH) },
            ReturnCode::DataError.as_c_int(),
        );
        assert!(!strm.msg.is_null(), "inflate should have set a message");
        // SAFETY: `msg` is a non-null pointer to one of the engine's static
        // NUL-terminated diagnostic strings.
        let before = unsafe { CStr::from_ptr(strm.msg) };
        assert_eq!(before.to_bytes(), b"invalid stored block lengths");

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert!(
            !strm.msg.is_null(),
            "inflateEnd nulled strm.msg; C's inflateEnd never touches it",
        );
        // SAFETY: the diagnostic strings are `'static`, so the pointer stays
        // valid after the stream's state is freed — which is exactly why C
        // callers may read it here.
        let after = unsafe { CStr::from_ptr(strm.msg) };
        assert_eq!(
            after.to_bytes(),
            b"invalid stored block lengths",
            "the message must still be readable after inflateEnd",
        );
    }

    /// C's `case DICT` with no dictionary runs `RESTORE(); return Z_NEED_DICT;`
    /// (`inflate.c` L701-L703), committing the cursors but jumping over the
    /// `total_in`/`total_out` updates at L1141-L1142. Measured against reference
    /// C: `total_in == 0` at the `Z_NEED_DICT` return even though the header
    /// bytes were consumed.
    #[test]
    fn need_dict_advances_cursors_without_advancing_totals() {
        // zlib header with FDICT set (CM=8, CINFO=7, FLG has bit 5) followed by
        // the 4-byte big-endian DICTID. `0x78 0x3F` satisfies FCHECK.
        const FDICT_HEADER: [u8; 6] = [0x78, 0x3F, 0xDE, 0xAD, 0xBE, 0xEF];

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );
        let mut out = vec![0u8; 64];
        strm.next_in = FDICT_HEADER.as_ptr();
        strm.avail_in = FDICT_HEADER.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(rc, ReturnCode::NeedDict.as_c_int(), "expected Z_NEED_DICT");
        // Cursors advanced (C's `RESTORE()`)...
        assert_eq!(
            strm.avail_in, 0,
            "RESTORE() must commit avail_in at the Z_NEED_DICT return",
        );
        // ...but the totals did not (C jumps over L1141-L1142).
        assert_eq!(
            strm.total_in, 0,
            "total_in must stay 0: C returns before its total bookkeeping",
        );
        assert_eq!(strm.total_out, 0, "total_out must stay 0");
        // And the DICTID was published into adler (C L696).
        assert_eq!(
            strm.adler, 0xDEAD_BEEF,
            "the dictionary id must be published into adler",
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// When the lazy window allocation is refused, C reaches its
    /// `state->mode = MEM; return Z_MEM_ERROR;` *after* `RESTORE()`
    /// (`inflate.c` L1132 then L1136-L1137), so the caller keeps every byte
    /// decoded during the call while `total_out` stays behind.
    #[test]
    fn window_allocation_failure_still_delivers_the_decoded_output() {
        // Permit the state reservation but refuse the window that `updatewindow`
        // needs, which is the second allocation C performs.
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(1),
        };
        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK,
        );

        // Feed the whole stream but cap the output so the decoder must keep
        // history in the window and therefore has to allocate it.
        let mut out = vec![0u8; 8];
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            rc, Z_MEM_ERROR,
            "the refused window must surface Z_MEM_ERROR"
        );

        let delivered = out.len() - strm.avail_out as usize;
        assert!(
            delivered > 0,
            "C commits next_out/avail_out via RESTORE() before returning \
             Z_MEM_ERROR, so the already-decoded bytes must reach the caller",
        );
        assert_eq!(
            &out[..delivered],
            &MSG[..delivered],
            "the delivered prefix must be the real decoded output",
        );
        assert_eq!(
            strm.total_out, 0,
            "total_out must stay 0: C jumps over L1141-L1142 on this path",
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// `inflateInit2_` must be fallible **before** it mutates the caller's
    /// `z_stream`: when the allocator refuses the state reservation C makes at
    /// `inflate.c` L198, the entry point reports `Z_MEM_ERROR` and `strm->state`
    /// stays null.
    #[test]
    fn init_reports_mem_error_and_installs_no_state_when_allocator_refuses() {
        // Zero allocations permitted, so the state reservation is refused.
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(0),
        };

        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_MEM_ERROR,
            "a refused allocation must surface Z_MEM_ERROR, never an abort or a \
             silent global-heap substitution"
        );
        assert!(
            strm.state.is_null(),
            "a failed init must not install a state on the caller's z_stream"
        );
        // Nothing was installed, so the stream is still uninitialized.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_STREAM_ERROR);
    }

    /// The `inflateBack` twin of the test above. C's `inflateBackInit_` makes a
    /// single `ZALLOC` for the state (`infback.c` L51-L53); refusing it must
    /// yield `Z_MEM_ERROR` with nothing installed, and `inflateBackEnd` must then
    /// report an uninitialized stream.
    #[test]
    fn back_init_reports_mem_error_and_installs_no_state_when_allocator_refuses() {
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(0),
        };

        // A caller-supplied window, as the C ABI requires, kept alive for the
        // duration of the call.
        let mut window = vec![0u8; 1 << 15];
        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_MEM_ERROR,
            "a refused inflateBack allocation must surface Z_MEM_ERROR"
        );
        assert!(
            strm.state.is_null(),
            "a failed inflateBackInit_ must not install a state"
        );
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_STREAM_ERROR);
    }

    /// F-03 regression: when the caller's `zalloc` cannot satisfy the copy,
    /// `inflateCopy` must report `Z_MEM_ERROR` — matching C's
    /// `ZFREE(copy); return Z_MEM_ERROR` (`inflate.c` L1343-L1349) — rather than
    /// reporting success with a destination backed by the Rust global allocator
    /// (AAP §0.6.3, §0.6.5).
    #[test]
    fn copy_reports_mem_error_when_caller_allocator_is_exhausted() {
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(16),
        };

        let mut src = zeroed_stream();
        attach_budget(&mut src, &budget);
        assert_eq!(
            unsafe { inflateInit_(&mut src, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Decode the stream so the lazily allocated window exists too, then
        // starve the allocator before attempting the copy.
        let mut out = vec![0u8; 128];
        src.next_in = ZLIB_STREAM.as_ptr();
        src.avail_in = ZLIB_STREAM.len() as c_uint;
        src.next_out = out.as_mut_ptr();
        src.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut src, Z_NO_FLUSH) }, Z_STREAM_END);

        budget
            .remaining
            .store(0, core::sync::atomic::Ordering::SeqCst);

        let mut dst = zeroed_stream();
        attach_budget(&mut dst, &budget);
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut src) },
            ReturnCode::MemError.as_c_int(),
            "an exhausted caller allocator must fail the copy, not silently \
             substitute global-allocator storage"
        );
        assert!(
            dst.state.is_null(),
            "a failed copy must leave the destination without a state"
        );

        // The source is untouched and still terminates cleanly.
        assert!(!src.state.is_null());
        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
    }

    /// The same allocator with budget to spare copies successfully, proving the
    /// test above fails for want of memory and for no other reason.
    #[test]
    fn copy_succeeds_when_caller_allocator_has_budget() {
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };

        let mut src = zeroed_stream();
        attach_budget(&mut src, &budget);
        assert_eq!(
            unsafe { inflateInit_(&mut src, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let mut dst = zeroed_stream();
        attach_budget(&mut dst, &budget);
        assert_eq!(unsafe { inflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());

        assert_eq!(unsafe { inflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
    }

    /// C4: `inflateBackEnd` must reject a stream whose handle is NOT an
    /// `inflateBack` handle (here a regular `inflate` handle) with
    /// `Z_STREAM_ERROR`, WITHOUT freeing it (no layout-mismatched free); the
    /// correct terminator then still succeeds.
    #[test]
    fn inflate_back_end_rejects_inflate_handle() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        // Wrong terminator: the handle tag is INFLATE, not INFLATE_BACK.
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_STREAM_ERROR);
        assert!(
            !strm.state.is_null(),
            "handle must not be freed on mismatch"
        );
        // The genuine terminator still works.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// C4: `inflateEnd` must reject an `inflateBack` handle (tag INFLATE_BACK, not
    /// INFLATE) with `Z_STREAM_ERROR`, WITHOUT freeing it; `inflateBackEnd` then
    /// reclaims it correctly.
    #[test]
    fn inflate_end_rejects_inflate_back_handle() {
        let mut strm = zeroed_stream();
        let mut window = [0u8; 1 << 15];
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        // Wrong terminator: the handle tag is INFLATE_BACK, not INFLATE.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_STREAM_ERROR);
        assert!(
            !strm.state.is_null(),
            "handle must not be freed on mismatch"
        );
        // The genuine terminator still works.
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    /// F6 / M7 end-to-end: with the caller's arena exhausted part-way through
    /// `inflateCopy`, the C entry point reports `Z_MEM_ERROR`, leaves `dest`
    /// untouched, releases whatever it did allocate, and leaves `source` able to
    /// finish decoding — never completing the copy on the global allocator.
    #[test]
    fn copy_reports_mem_error_when_the_caller_arena_is_exhausted() {
        use crate::ffi::alloc::test_hook::HookStats;

        let stats = HookStats::new();
        let hook = stats.hook();

        let mut src = zeroed_stream();
        src.zalloc = hook.zalloc();
        src.zfree = hook.zfree();
        src.opaque = hook.opaque();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut src,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // Decode the first half so the history window and the dynamic code
        // tables are populated before the copy is attempted.
        let mut out = vec![0u8; MSG.len() * 2];
        let split = ZLIB_STREAM.len() / 2;
        src.next_in = ZLIB_STREAM.as_ptr();
        src.avail_in = split as c_uint;
        src.next_out = out.as_mut_ptr();
        src.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut src, Z_NO_FLUSH) }, Z_OK);

        let base_allocs = stats.allocs();
        assert!(
            base_allocs >= 2,
            "init and the first decode must both allocate through the caller's zalloc (got {base_allocs})"
        );
        assert_eq!(stats.frees(), 0);

        // Allow exactly one of the copy's two buffer allocations to succeed.
        stats.set_budget(1);
        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut src) },
            ReturnCode::MemError.as_c_int(),
            "an exhausted caller arena must surface Z_MEM_ERROR"
        );

        // `dest` is untouched — C allocates before writing anything to it.
        assert!(dst.state.is_null(), "a failed copy must not install state");
        assert_eq!(dst.total_out, 0);
        assert!(dst.zalloc.is_none() && dst.zfree.is_none());

        assert_eq!(
            stats.allocs() - base_allocs,
            1,
            "only the budgeted allocation may succeed"
        );
        assert_eq!(stats.ooms(), 1, "the second copy allocation reported OOM");
        assert_eq!(
            stats.frees(),
            1,
            "the partially built copy must be released through the caller's zfree"
        );

        // The source survived and still decodes the remainder correctly.
        stats.set_budget(usize::MAX);
        src.next_in = ZLIB_STREAM[split..].as_ptr();
        src.avail_in = (ZLIB_STREAM.len() - split) as c_uint;
        assert_eq!(unsafe { inflate(&mut src, Z_NO_FLUSH) }, Z_STREAM_END);
        assert_eq!(&out[..src.total_out as usize], MSG);

        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
        assert_eq!(
            stats.live_bytes(),
            0,
            "the caller's arena must be perfectly balanced"
        );
    }

    /// The success path: a mid-stream copy is allocated entirely from the
    /// caller's arena, decodes independently of the source even when the two are
    /// fed different remainders, and both streams balance on `inflateEnd`.
    #[test]
    fn copy_allocates_from_the_callers_arena_and_decodes_independently() {
        use crate::ffi::alloc::test_hook::HookStats;

        let stats = HookStats::new();
        let hook = stats.hook();

        let mut src = zeroed_stream();
        src.zalloc = hook.zalloc();
        src.zfree = hook.zfree();
        src.opaque = hook.opaque();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut src,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // Populate the window and the dynamic tables.
        let mut out_src = vec![0u8; MSG.len() * 2];
        let split = ZLIB_STREAM.len() / 2;
        src.next_in = ZLIB_STREAM.as_ptr();
        src.avail_in = split as c_uint;
        src.next_out = out_src.as_mut_ptr();
        src.avail_out = out_src.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut src, Z_NO_FLUSH) }, Z_OK);
        let base_allocs = stats.allocs();
        // Bytes already produced when the snapshot is taken; `inflateCopy`
        // mirrors this into `dest.total_out`.
        let produced = src.total_out;

        let mut dst = zeroed_stream();
        assert_eq!(unsafe { inflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(
            stats.allocs() > base_allocs,
            "the copy's buffers must come from the caller's zalloc"
        );
        assert_eq!(stats.ooms(), 0);
        assert!(dst.zalloc.is_some() && dst.zfree.is_some());
        assert_eq!(dst.opaque, src.opaque);
        assert_eq!(dst.total_out, produced, "C mirrors the whole z_stream");

        // Give the copy its own output buffer and finish it independently. It
        // resumes exactly where the snapshot was taken, so the bytes it emits are
        // the tail of the plaintext — reconstructed from the copy's *own* history
        // window and code tables.
        let mut out_dst = vec![0u8; MSG.len() * 2];
        dst.next_in = ZLIB_STREAM[split..].as_ptr();
        dst.avail_in = (ZLIB_STREAM.len() - split) as c_uint;
        dst.next_out = out_dst.as_mut_ptr();
        dst.avail_out = out_dst.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut dst, Z_NO_FLUSH) }, Z_STREAM_END);
        let tail = (dst.total_out - produced) as usize;
        assert_eq!(&out_dst[..tail], &MSG[produced as usize..]);

        // Finish the source too; both reproduce the same plaintext from their own
        // buffers, proving the window and code tables are not shared.
        src.next_in = ZLIB_STREAM[split..].as_ptr();
        src.avail_in = (ZLIB_STREAM.len() - split) as c_uint;
        assert_eq!(unsafe { inflate(&mut src, Z_NO_FLUSH) }, Z_STREAM_END);
        assert_eq!(&out_src[..src.total_out as usize], MSG);

        assert_eq!(unsafe { inflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
        assert_eq!(
            stats.live_bytes(),
            0,
            "every region must reach the caller's zfree"
        );
    }

    /// A `zalloc` that always refuses, used to prove the caller's out-of-memory
    /// signal is never bypassed. It allocates nothing, so a matching `zfree` is
    /// never needed.
    unsafe extern "C" fn always_fail_zalloc(
        _opaque: *mut c_void,
        _items: c_uint,
        _size: c_uint,
    ) -> *mut c_void {
        ptr::null_mut()
    }

    /// A `zfree` that must never be reached in the half-present tests.
    unsafe extern "C" fn never_called_zfree(_opaque: *mut c_void, _address: *mut c_void) {
        panic!("zfree must not be called: init rejected the half-present hook pair");
    }

    /// QA-03 regression — `inflateInit2_` must reject a caller who supplied
    /// exactly one half of the `zalloc`/`zfree` pair.
    ///
    /// C substitutes only the *missing* half (`inflate.c` L183-L196) and keeps the
    /// one the caller gave, so a supplied-but-failing `zalloc` is still called and
    /// still yields `Z_MEM_ERROR`. This crate cannot substitute per half
    /// (`zcalloc`/`zcfree` are unexported `local:` symbols, and pairing a caller
    /// hook with the global allocator is undefined behavior), and silently
    /// ignoring the supplied half would discard the caller's OOM signal — which
    /// AAP §0.6.3 forbids. `Z_STREAM_ERROR` is the code C's own
    /// `inflateStateCheck` yields for such a stream (`inflate.c` L90-L91) and the
    /// code its `Z_SOLO` init returns, so it is the faithful answer here
    /// (documented divergence, AAP §0.8.2).
    #[test]
    fn init_rejects_a_half_present_allocator_pair() {
        // zalloc only.
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_STREAM_ERROR,
            "a zalloc supplied without a zfree must be rejected, not silently \
             replaced by the global allocator"
        );
        assert!(
            strm.state.is_null(),
            "a rejected init must install no state"
        );

        // zfree only. `never_called_zfree` panics if it is ever invoked, proving
        // the rejection happens before any allocation is made or released.
        let mut strm = zeroed_stream();
        strm.zfree = Some(never_called_zfree);
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_STREAM_ERROR,
            "a zfree supplied without a zalloc must be rejected"
        );
        assert!(
            strm.state.is_null(),
            "a rejected init must install no state"
        );
    }

    /// The other half of the matrix: the two *valid* hook configurations must be
    /// unaffected. Neither half supplied is the overwhelmingly common case (a
    /// zeroed `z_stream`) and matches C's substitution of both built-ins; both
    /// halves supplied routes through the caller's allocator.
    #[test]
    fn init_accepts_both_hooks_and_neither_hook() {
        // Neither: global allocator, exactly as before.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a zeroed z_stream must still initialize"
        );
        assert!(!strm.state.is_null());
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);

        // Both: the caller's allocator backs the state.
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };
        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a complete hook pair with budget must initialize"
        );
        assert!(!strm.state.is_null());
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// The `inflateBack` twin: `infback.c` carries the identical prologue
    /// (L37-L50), so it must reach the identical verdict.
    #[test]
    fn back_init_rejects_a_half_present_allocator_pair() {
        let mut window = vec![0u8; 1 << 15];

        let cases: [(&str, crate::ffi::alloc_func, crate::ffi::free_func); 2] = [
            ("zalloc only", Some(always_fail_zalloc), None),
            ("zfree only", None, Some(never_called_zfree)),
        ];
        for (label, zalloc, zfree) in cases {
            let mut strm = zeroed_stream();
            strm.zalloc = zalloc;
            strm.zfree = zfree;
            assert_eq!(
                unsafe {
                    inflateBackInit_(
                        &mut strm,
                        15,
                        window.as_mut_ptr(),
                        VERSION.as_ptr(),
                        size_of::<z_stream>() as c_int,
                    )
                },
                Z_STREAM_ERROR,
                "inflateBackInit_ must reject a half-present hook pair ({label})"
            );
            assert!(
                strm.state.is_null(),
                "a rejected inflateBackInit_ must install no state ({label})"
            );
        }

        // Control: neither half still initializes, as it always has.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateBackInit_(
                    &mut strm,
                    15,
                    window.as_mut_ptr(),
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
    }

    /// `inflateCopy` reads the *source's* raw allocator fields, so it carries the
    /// same exposure. C rejects it through `inflateStateCheck(source)`
    /// (`inflate.c` L90-L91) before its `ZALLOC(source, …)`.
    #[test]
    fn copy_rejects_a_half_present_source_allocator() {
        let mut src = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut src, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Mutate the source into an invalid half-present pair after a successful
        // init, which is the only way this state is reachable.
        src.zalloc = Some(always_fail_zalloc);

        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut src) },
            Z_STREAM_ERROR,
            "a half-present source allocator must not yield a destination backed \
             by the global heap while the caller believes their hook owns it"
        );
        assert!(dst.state.is_null(), "a rejected copy must install no state");

        src.zalloc = None;
        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
    }
}
