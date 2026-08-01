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
//! Both paths store a **type-tagged** handle in `z_stream.state`: a
//! `#[repr(C)]` struct whose first field is a [`HandleKind`] discriminant at
//! offset 0. Each accessor reads that tag through the shared header prefix and
//! confirms it before reinterpreting the opaque pointer, so a stream initialized
//! by one engine can never be reconstituted — or dropped — as another engine's
//! box.
//!
//! * Regular inflate path: `inflateInit_`/`inflateInit2_` install a
//!   `Box<InflateHandle>` tagged `HandleKind::INFLATE`, holding an idiomatic
//!   [`ZStream`] plus the raw pointer to the caller's registered `gz_header`.
//!   `inflate_handle` reborrows it and `inflate_take` reclaims it, each
//!   validating the tag first; [`inflateEnd`] drops the reclaimed box, so RAII
//!   replaces the manual `inflateEnd` free.
//! * `inflateBack` path: [`inflateBackInit_`] installs a
//!   `Box<InflateBackHandle>` tagged `HandleKind::INFLATE_BACK`, whose `inner`
//!   field owns the `Box<InflateState>` that the
//!   [`crate::inflate::back`] engine drives (its owned window doubles as the
//!   output buffer). `inflate_back_handle` reborrows it and `inflate_back_take`
//!   reclaims it, again tag-first.
//!
//! Ownership is exactly-once in both directions: installation transfers the box
//! into `state`, and the `take` helpers null `state` as they reclaim it, so a
//! second `End` call finds nothing to free. A tag mismatch leaves `state`
//! untouched and yields `Z_STREAM_ERROR` rather than performing a
//! layout-mismatched deallocation.
//!
//! # Defined rejections — deliberately part of the caller contract
//!
//! The uniform contract above is what a *well-formed* C caller owes this
//! boundary. A C consumer, however, cannot be assumed well-formed. Reference
//! zlib already answers *some* malformed calls with a defined error — its
//! `inflateStateCheck` and the `inflate` entry test at `inflate.c` L474 — and
//! this boundary deliberately extends the same treatment to the rest, so every
//! case below is a diagnosable error instead of undefined behavior. The
//! following inputs are therefore **explicitly inside** the contract of every
//! non-initializing shim here: each is detected and rejected with
//! `Z_STREAM_ERROR` (never a fabricated `Z_MEM_ERROR`, never a panic across the
//! ABI), nothing is loaded through the offending pointer, and no allocation is
//! created, freed, or reinterpreted.
//!
//! * **A null `z_streamp`.** Tested before any field is read.
//! * **A zeroed or never-initialized `z_stream`.** Its `state` is null, which
//!   [`peek_handle_kind`] reports as "no handle" without dereferencing it.
//! * **A stream whose `*End` already ran.** Teardown nulls `state`, making this
//!   indistinguishable from the previous case, which is exactly why a second
//!   `inflateEnd` or `inflateBackEnd` is a defined `Z_STREAM_ERROR` and never a
//!   double free.
//! * **A stream carrying another engine's handle** — `inflateEnd` on a
//!   `deflateInit2_` stream, `inflateBackEnd` on a plain inflate stream, or any
//!   other crossing. The [`HandleKind`] tag at offset 0 is compared before the
//!   opaque pointer is reinterpreted, so a cross-engine terminator can neither
//!   deallocate with a mismatched `Layout` nor read another engine's fields.
//!   This is a deliberate hardening **beyond** the C contract: reference zlib's
//!   `inflateEnd` would cast the opaque `state` blindly, so portable C code must
//!   still never do it — but against this implementation it is defined.
//! * **Inconsistent buffer descriptors:** a null `next_out`, or a null `next_in`
//!   paired with a non-zero `avail_in`. These fail `stream_buffers_valid` /
//!   `input_ptr_valid` and are rejected *before* any slice is constructed.
//! * **A null `in_func`/`out_func` passed to [`inflateBack`]**, and a null
//!   `window` passed to [`inflateBackInit_`] — both checked, never called or
//!   dereferenced.
//!
//! What stays the caller's obligation is only this: when a pointer is non-null
//! and the call is one that actually uses it, the region it names must really be
//! readable/writable for the length declared alongside it and must not overlap
//! the other region, for the duration of the call. A pointer that the taken path
//! never uses need not be valid — [`inflateBackInit_`] documents the one case
//! where that distinction is observable.
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
    gz_headerp, in_func, init_allocator_prologue, input_ptr_valid, input_slice, out_func,
    output_slice, peek_handle_kind, set_adler, set_data_type, set_msg, state_ptr_from_box,
    stream_buffers_valid, uInt, z_stream, z_streamp,
};
use crate::inflate::back::{InFunc, OutFunc};
use crate::inflate::state::InflateState;
use crate::stream::ZStream;

// `gz_header` (the `#[repr(C)]` mirror) and the header write-back helper are
// only referenced by the gzip-only `inflateGetHeader` path and the header
// write-back inside `inflate`, so gate their imports to avoid unused warnings.
#[cfg(feature = "gzip")]
use crate::ffi::types::{gz_header, publish_gz_header};
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
/// `Box::from_raw` cast would cause on cross-type misuse. `#[repr(C)]`
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
/// handle, so [`inflateEnd`] never drops a wrong-type box.
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
/// reinterpreting the opaque `state` pointer, closing the undefined behavior where an
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
/// handle, so [`inflateBackEnd`] never drops a wrong-type box.
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
/// `avail_in` exactly like C `infback.c`'s `inf_leave`.
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
/// engine (`inflate.c` L136-L161):
///
/// * `8..=15` — zlib (RFC 1950) wrapper;
/// * `-15..=-8` — raw DEFLATE, no wrapper. Unlike the deflate side, which
///   rejects raw `-8` via its `windowBits == 8 && wrap != 1` guard, inflate
///   accepts the full `-15..=-8` span;
/// * `0` — zlib wrapper, with the window size taken from the header's CINFO
///   field instead of the argument (C: `wrap = 5`, `wbits = 0`);
/// * `24..=31` (`16 + 8..=15`) — gzip (RFC 1952) wrapper;
/// * `40..=47` (`32 + 8..=15`) — zlib/gzip auto-detection.
///
/// The two gzip-bearing forms require the `gzip` cargo feature; without it they
/// are rejected with `Z_STREAM_ERROR`.
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
        // Run C's allocator prologue here, in exactly the position C runs it:
        // after the version and null-stream guards and *before* the state
        // `ZALLOC` (`inflate.c` L182-L199). It clears `strm->msg` unconditionally
        // and substitutes the crate's built-in for a missing half of the
        // `zalloc`/`zfree` pair, clearing `opaque` only when it is `zalloc` that
        // was defaulted. The caller's own half is kept, so a deliberately failing
        // `zalloc` still reports `Z_MEM_ERROR` rather than being bypassed.
        // SAFETY: `sref` is a valid, exclusively-owned `&mut z_stream`; only its
        // plain `Copy` `msg`/allocator fields are read and written, and no hook
        // pointer is dereferenced.
        let allocator = unsafe { init_allocator_prologue(sref) };
        // Build the idiomatic stream on the post-prologue allocator triple: the
        // caller's `zalloc`/`zfree`/`opaque` where supplied, the crate's built-in
        // where substituted, and the global allocator when the caller supplied
        // neither half (AAP §0.6.3).
        let mut zs = ZStream::with_allocator(allocator);
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
/// Those obligations bind **only on the accepting path**, and that is a
/// deliberate part of the contract rather than an accident of the
/// implementation. The argument guards run in a fixed order — version pair,
/// then `strm`/`window` null, then the `8..=15` `windowBits` bound — and the
/// region is first named only after all three have passed:
///
/// * A **null `window`** is a defined, rejected argument (`Z_STREAM_ERROR`). The
///   pointer is compared, never dereferenced.
/// * A **`windowBits` outside `8..=15`** is rejected before the window is
///   touched, so the size obligation does not apply to such a call. Probing the
///   rejected range with a buffer smaller than `1 << windowBits` — or with no
///   buffer at all — is therefore sound and within contract.
///
/// Only a call that will be *accepted* must actually supply `1 << windowBits`
/// live, un-aliased bytes that outlive the state.
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
        // Run C's allocator prologue here, in exactly the position C runs it:
        // after the version, null-stream, null-window and `windowBits` guards and
        // *before* the state `ZALLOC` (`infback.c` L36-L53). It clears
        // `strm->msg` unconditionally and substitutes the crate's built-in for a
        // missing half of the `zalloc`/`zfree` pair, clearing `opaque` only when
        // it is `zalloc` that was defaulted.
        //
        // The resulting triple charges the *state* footprint to the caller's
        // allocator, matching the single
        // `ZALLOC(strm, 1, sizeof(struct inflate_state))` C makes (`infback.c`
        // L51). A caller who supplied *neither* half gets both built-ins
        // published into their `z_stream` (as C publishes `zcalloc`/`zcfree`) but
        // no active hook, so the reservation stays unmade and the built-in
        // allocator serves the state — see `CAllocator::is_builtin_pair`.
        // SAFETY: `sref` is a valid, exclusively-owned `&mut z_stream`; only its
        // plain `Copy` `msg`/allocator fields are read and written, and no hook
        // pointer is dereferenced.
        let allocator = unsafe { init_allocator_prologue(sref) };

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
                // regular `inflateEnd` rejects it — no blind cast of an untagged
                // `Box<InflateState>` is ever performed.
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
                // `state` slot, so publishing a `0` here would clobber a field C
                // leaves entirely alone.
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
        // The crate-private tracked form additionally reports whether the C
        // `total_in`/`total_out` mirrors may advance; the public
        // `crate::inflate::inflate` discards that flag (see
        // `TrackedInflateOutcome`).
        let tracked = crate::inflate::inflate_tracked(&mut handle.zs, input, output, flush);
        let outcome = tracked.outcome;

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
        // `inflateGetHeader`, publish exactly the assignments this call performed,
        // honoring the caller's `extra_max`/`name_max`/`comm_max` capacities.
        //
        // Publishing the *schedule* rather than the whole owned header is
        // load-bearing. C writes each field inside its own parser state, directly
        // into the caller's struct, so a caller polling between `inflate` calls
        // sees only what the stream has delivered. Mirroring the owned value
        // wholesale would instead zero every scalar the decode has not reached
        // yet, NUL-terminate a half-received name, and flatten the tri-state
        // `done` to `0` — all observable divergences (AAP §0.8.1 D-4, standard
        // S5). See `publish_gz_header`.
        #[cfg(feature = "gzip")]
        {
            let head_ptr = handle.head;
            if !head_ptr.is_null() {
                if let Some(state) = handle.zs.inflate_state() {
                    if let Some(gh) = state.head.as_ref() {
                        // SAFETY: `head_ptr` is the caller's `gz_header`, still
                        // valid (registered via `inflateGetHeader`); the helper
                        // writes only within the recorded `*_max` capacities.
                        unsafe { publish_gz_header(head_ptr, gh, &tracked.header) };
                    }
                }
            }
        }

        // The `handle` borrow ends here (its last use was above); re-borrow the
        // raw stream to advance cursors and publish observable fields.
        // C's two `RESTORE()`-then-return-directly paths commit the cursors but
        // jump over `strm->total_in += in; strm->total_out += out;`
        // (`inflate.c` L1141-L1142). Snapshot the totals so they can be rolled
        // back for exactly those returns; see
        // `TrackedInflateOutcome::commit_totals`.
        let (prev_total_in, prev_total_out) = (sref.total_in, sref.total_out);
        // SAFETY: `outcome.consumed <= input.len() == avail_in`, so advancing by
        // that amount keeps `next_in`/`avail_in`/`total_in` consistent.
        unsafe { advance_input(sref, outcome.consumed) };
        // SAFETY: `outcome.produced <= output.len() == avail_out`, as above.
        unsafe { advance_output(sref, outcome.produced) };
        if !tracked.commit_totals {
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
        // which returns `Z_STREAM_ERROR` for a non-inflate stream.
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
///
/// C reads `dictLength` bytes from `dictionary` unconditionally, so a null
/// pointer or a bogus length is undefined behavior on the caller. This shim
/// instead maps a null `dictionary` **or** a zero `dict_length` to an empty
/// slice, which the engine treats as an empty dictionary. That is strictly more
/// defensive than C: it turns one class of caller UB into a well-defined
/// `Z_DATA_ERROR` (the Adler-32 of an empty dictionary will not match a stream
/// that asked for one) while behaving identically for every well-formed call.
/// Non-null pointers with a positive length are read exactly as C reads them.
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
        // crate initialized always carries two non-null halves, so this can fire
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
/// Registration has one caller-visible side effect, reproduced from
/// `inflate.c` L1228-L1229 (`state->head = head; head->done = 0;`): on success
/// the caller's `head->done` is cleared to `0`, so a `gz_header` reused across
/// streams cannot report a stale completion. C performs that store
/// unconditionally and therefore requires a non-null `head`; this shim skips it
/// for a null `head`, which is the only difference.
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
                // The caller's `extra_len` needs no seeding: C assigns
                // `head->extra_len` only when the header actually carries an
                // `FEXTRA` field (`inflate.c` L596-L600), and for a stream without
                // one it leaves the caller's field exactly as it found it
                // (`inflate.c` L605-L606 nulls `extra`, never `extra_len`).
                // `publish_gz_header` writes the field only when the decoder
                // reports a declared `XLEN`, so both cases fall out for free.
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
                // Clear `head->done` at *registration*, synchronously, exactly as
                // C does: `state->head = head; head->done = 0;`
                // (`inflate.c` L1228-L1229). This is observable and load-bearing —
                // `done` is the caller's "the header is fully decoded" flag, which
                // `zlib.h` L1076-L1085 documents as "zero until the header is
                // completed, at which time head->done is set to one", and which a
                // caller polls between `inflate` calls. Re-registering a
                // `gz_header` that a previous stream had already completed would
                // otherwise leave a stale `done == 1` visible from the instant of
                // registration until the new stream's header actually finished, so
                // a polling caller could read the *old* stream's fields and
                // believe they described the new one.
                //
                // C dereferences `head` unconditionally here, so a null `head` is
                // a caller error it does not survive; this shim already tolerates
                // one (capturing into a default `GzHeader` above), so the write is
                // guarded to keep that tolerance rather than reintroducing the
                // crash.
                if !head.is_null() {
                    // SAFETY: `head` is non-null and, per this entry point's FFI
                    // contract, points at a valid caller-owned `gz_header` that
                    // stays valid until the decode completes. `done` is a plain
                    // `c_int` field, so this is an ordinary aligned scalar write
                    // to a field the caller has explicitly handed over for the
                    // library to publish into.
                    unsafe { (*head).done = 0 };
                }
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
/// Returns exactly the four codes `zlib.h` L1194-L1199 documents for
/// `inflateBack` — `Z_MEM_ERROR` is **not** among them, because the window and
/// state were already allocated by [`inflateBackInit_`] and this call allocates
/// nothing:
///
/// * `Z_STREAM_END` — the raw DEFLATE stream completed successfully;
/// * `Z_DATA_ERROR` — the stream was malformed, with `strm->msg` set to the
///   reason;
/// * `Z_BUF_ERROR` — the `in` callback reported no input available, or the `out`
///   callback reported a write failure;
/// * `Z_STREAM_ERROR` — the stream was not properly initialized, i.e. a null
///   `strm`, a `state` that is not an `inflateBackInit_` handle, or a null
///   `in`/`out` callback.
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

        // Fetch the tagged `inflateBack` handle, validating the kind before
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

        // Restore `next_in`/`avail_in` exactly like C `infback.c`'s `inf_leave`
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
        // Validate the `INFLATE_BACK` tag before reclaiming. `inflate_back_take`
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
    use crate::ffi::alloc::test_hook::BuiltinHookStats;
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

    /// An all-zero `gz_header`, the state a caller's fresh struct is in.
    ///
    /// Deliberately **not** feature-gated: the `#[cfg(not(feature = "gzip"))]`
    /// rejection test needs it too, and `gz_header` is an unconditional
    /// `#[repr(C)]` ABI mirror (`src/ffi/types.rs`), so it is nameable in every
    /// configuration.
    fn zeroed_gz_header() -> crate::ffi::types::gz_header {
        crate::ffi::types::gz_header {
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

    /// CQ-2 regression — `inflateGetHeader` must clear `head->done`
    /// **synchronously, at registration**, exactly as C's
    /// `state->head = head; head->done = 0;` does (`inflate.c` L1228-L1229).
    ///
    /// The failure mode this pins is precise: `zlib.h` L1076-L1085 specifies
    /// `done` as zero until the header completes and one afterwards, and a caller
    /// polls it between `inflate` calls. A `gz_header` reused from a previous,
    /// completed decode still carries `done == 1`; if registration does not clear
    /// it, the flag reads "complete" from the instant of registration, so a
    /// polling caller would accept the *previous* stream's fields as the new
    /// stream's. The assertion is therefore made before any `inflate` call, with a
    /// deliberately non-zero pre-seeded sentinel — a check made only after a
    /// successful decode would pass even with the write missing.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_clears_done_at_registration() {
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

        // Pre-seed `done` the way a reused header arrives: C's tri-state uses `1`
        // for "complete" and `-1` for "was a raw zlib stream", so try both, plus
        // an arbitrary non-zero value no caller would produce.
        for seeded in [1, -1, 0x7F] {
            let mut head = zeroed_gz_header();
            head.done = seeded;
            assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut head) }, Z_OK);
            assert_eq!(
                head.done, 0,
                "registration must clear `done` immediately (seeded {seeded}), before \
                 any inflate call — `inflate.c` L1228-L1229"
            );
        }

        // The same must hold for a header that also requests field capture, and
        // `done` must still be `0` after a *partial* decode that has not yet
        // finished the header.
        let mut namebuf = [0u8; 64];
        let mut head = zeroed_gz_header();
        head.done = 1;
        head.name = namebuf.as_mut_ptr();
        head.name_max = namebuf.len() as c_uint;
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut head) }, Z_OK);
        assert_eq!(head.done, 0, "a capturing header is cleared too");

        // Feed only the first two bytes (the `1f 8b` magic): the header cannot be
        // complete yet, so `done` must still read zero.
        let mut out = vec![0u8; 256];
        strm.next_in = GZIP_NAMED.as_ptr();
        strm.avail_in = 2;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_OK);
        assert_eq!(head.done, 0, "an incomplete header must not report done");

        // Finishing the stream sets it to one, which is the other half of the
        // contract and proves the clear did not break completion reporting.
        strm.next_in = GZIP_NAMED[2..].as_ptr();
        strm.avail_in = (GZIP_NAMED.len() - 2) as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
        assert_eq!(
            head.done, 1,
            "a completed gzip header must report done == 1"
        );

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// The other side of the `done` contract, asserted with gzip framing compiled
    /// **in**: a registration C refuses must leave the caller's header completely
    /// alone, `done` included.
    ///
    /// C reaches `state->head = head; head->done = 0;` only after
    /// `(state->wrap & 2) == 0` has been ruled out (`inflate.c` L1225-L1229), so a
    /// *raw* stream — `windowBits = -15`, which carries no gzip header at all —
    /// returns `Z_STREAM_ERROR` before the store. This is the case
    /// [`get_header_without_gzip_rejects_and_leaves_done_untouched`] cannot cover,
    /// because that test only exists when the `gzip` feature is off; without this
    /// one the default build would have no guard at all against the `done` write
    /// escaping onto the rejection path.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_rejection_leaves_the_callers_done_flag_untouched() {
        let mut head = zeroed_gz_header();
        head.done = 1; // as if left over from a previous, completed stream

        let mut raw = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut raw,
                    -15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        assert_eq!(
            unsafe { inflateGetHeader(&mut raw, &mut head) },
            Z_STREAM_ERROR,
            "a raw stream carries no gzip header, so registration is refused"
        );
        assert_eq!(
            head.done, 1,
            "a rejected registration must not touch the caller's header"
        );
        assert_eq!(unsafe { inflateEnd(&mut raw) }, Z_OK);

        // A zlib-wrapped stream is refused for the same reason (`wrap == 1`), and
        // must likewise leave the header untouched.
        let mut zlib = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut zlib, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        assert_eq!(
            unsafe { inflateGetHeader(&mut zlib, &mut head) },
            Z_STREAM_ERROR,
            "a zlib-wrapped stream carries no gzip header either"
        );
        assert_eq!(
            head.done, 1,
            "a rejected registration must not touch the caller's header"
        );
        assert_eq!(unsafe { inflateEnd(&mut zlib) }, Z_OK);
    }

    /// A null `head` is tolerated rather than dereferenced. C would crash here
    /// (`head->done = 0` is unconditional in `inflate.c` L1229), so this shim is
    /// strictly more forgiving; the point of the case is that adding the `done`
    /// write did not reintroduce the crash.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_tolerates_a_null_header_pointer() {
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
        assert_eq!(
            unsafe { inflateGetHeader(&mut strm, ptr::null_mut()) },
            Z_OK,
            "a null gz_header must be accepted, not dereferenced"
        );

        // The stream still decodes correctly with nothing captured.
        let mut out = vec![0u8; 256];
        strm.next_in = GZIP_NAMED.as_ptr();
        strm.avail_in = GZIP_NAMED.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// With gzip framing compiled out, `inflateGetHeader` rejects every request
    /// with `Z_STREAM_ERROR` and therefore must not touch the caller's header at
    /// all — including `done`, since C reaches its `head->done = 0` only after the
    /// `(state->wrap & 2)` test passes (`inflate.c` L1225-L1229).
    #[test]
    #[cfg(not(feature = "gzip"))]
    fn get_header_without_gzip_rejects_and_leaves_done_untouched() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let mut head = zeroed_gz_header();
        head.done = 1;
        assert_eq!(
            unsafe { inflateGetHeader(&mut strm, &mut head) },
            Z_STREAM_ERROR
        );
        assert_eq!(
            head.done, 1,
            "a rejected registration must leave the caller's header untouched"
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
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

    /// Regression guard — the extra field's **declared** length must survive a
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

    /// The null-buffer length query: leave `extra` null purely to learn the extra
    /// field's length. C writes `head->extra_len` in the `EXLEN` state
    /// (`inflate.c` L599-L600) gated on neither `extra`'s nullity nor `extra_max`,
    /// so a null buffer still yields the real length. `zlib.h` documents only the
    /// `extra_len > extra_max` truncation signal, making this a de-facto behavior
    /// that the port must nonetheless match.
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

    // -- inflateGetHeader: the INCREMENTAL publication schedule -------------

    /// The poison byte every capture buffer is pre-filled with, so any write the
    /// library was not authorized to make is visible.
    #[cfg(feature = "gzip")]
    const HDR_POISON: u8 = 0x7e;

    /// Which optional gzip header fields [`gzip_stream_with_fields`] should emit.
    #[cfg(feature = "gzip")]
    #[derive(Clone, Copy)]
    struct HeaderFields {
        extra: bool,
        name: bool,
        comment: bool,
        hcrc: bool,
    }

    #[cfg(feature = "gzip")]
    impl HeaderFields {
        /// Every optional field present.
        const ALL: Self = Self {
            extra: true,
            name: true,
            comment: true,
            hcrc: true,
        };
        /// No optional field present — a bare `FLG == 0` gzip member.
        const NONE: Self = Self {
            extra: false,
            name: false,
            comment: false,
            hcrc: false,
        };
    }

    /// The `FEXTRA` payload every fixture in this section declares.
    #[cfg(feature = "gzip")]
    const HDR_EXTRA: &[u8] = b"ABCDEF";
    /// The `FNAME` every fixture declares — nine content bytes, so `name_max`
    /// values of 9 and 10 straddle C's "the NUL is a counted byte" rule.
    #[cfg(feature = "gzip")]
    const HDR_NAME: &[u8] = b"hello.txt";
    /// The `FCOMMENT` every fixture declares.
    #[cfg(feature = "gzip")]
    const HDR_COMMENT: &[u8] = b"note";
    /// The `MTIME` every fixture declares, chosen so all four bytes differ.
    #[cfg(feature = "gzip")]
    const HDR_TIME: c_ulong = 0x1122_3344;

    /// Builds a gzip member carrying exactly the header fields `fields` names,
    /// using this crate's own encoder through `deflateSetHeader`.
    ///
    /// Mirrors the `build_stream_flags` fixture of the differential C harness this
    /// section's expectations were measured against, so the Rust assertions and
    /// the reference-C observations describe the same bytes.
    #[cfg(feature = "gzip")]
    fn gzip_stream_with_fields(fields: HeaderFields) -> Vec<u8> {
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

        let mut extra = HDR_EXTRA.to_vec();
        let mut name = HDR_NAME.to_vec();
        name.push(0);
        let mut comment = HDR_COMMENT.to_vec();
        comment.push(0);

        let mut head = zeroed_gz_header();
        head.text = 1;
        head.time = HDR_TIME;
        head.os = 3;
        if fields.extra {
            head.extra = extra.as_mut_ptr();
            head.extra_len = HDR_EXTRA.len() as c_uint;
        }
        if fields.name {
            head.name = name.as_mut_ptr();
        }
        if fields.comment {
            head.comment = comment.as_mut_ptr();
        }
        head.hcrc = c_int::from(fields.hcrc);
        assert_eq!(unsafe { deflateSetHeader(&mut d, &mut head) }, Z_OK);

        let payload: Vec<u8> = (0..64u8).map(|i| b'a' + (i % 26)).collect();
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

    /// A caller's `gz_header` with every scalar poisoned and capture buffers
    /// pre-filled with [`HDR_POISON`], the shape the differential C harness uses.
    ///
    /// A capacity of `0` hands over a null pointer, which is how a C caller
    /// declines to capture a field.
    #[cfg(feature = "gzip")]
    struct PoisonedHeader {
        head: crate::ffi::types::gz_header,
        extra: Vec<u8>,
        name: Vec<u8>,
        comment: Vec<u8>,
    }

    #[cfg(feature = "gzip")]
    impl PoisonedHeader {
        /// Poison value for `text`, `xflags`, `os` and `hcrc` — a value no gzip
        /// header can legitimately produce for any of them.
        const SCALAR: c_int = 9;
        /// Poison value for `time`.
        const TIME: c_ulong = 0xDEAD_BEEF;
        /// Poison value for `extra_len`.
        const XLEN: c_uint = 12345;

        fn new(extra_max: usize, name_max: usize, comm_max: usize) -> Self {
            let mut this = Self {
                head: zeroed_gz_header(),
                extra: vec![HDR_POISON; extra_max],
                name: vec![HDR_POISON; name_max],
                comment: vec![HDR_POISON; comm_max],
            };
            this.head.text = Self::SCALAR;
            this.head.time = Self::TIME;
            this.head.xflags = Self::SCALAR;
            this.head.os = Self::SCALAR;
            this.head.hcrc = Self::SCALAR;
            this.head.done = Self::SCALAR;
            this.head.extra_len = Self::XLEN;
            this.head.extra_max = extra_max as c_uint;
            this.head.name_max = name_max as c_uint;
            this.head.comm_max = comm_max as c_uint;
            // Pointers are taken after the vectors are in their final place.
            if extra_max != 0 {
                this.head.extra = this.extra.as_mut_ptr();
            }
            if name_max != 0 {
                this.head.name = this.name.as_mut_ptr();
            }
            if comm_max != 0 {
                this.head.comment = this.comment.as_mut_ptr();
            }
            this
        }
    }

    /// Registers a poisoned header on a `window_bits`-framed stream, feeds
    /// `stream` `step` bytes per `inflate` call, and invokes `observe` after every
    /// call — the Rust equivalent of the differential harness's `drive`.
    ///
    /// Stops as soon as `done` reports completion (`1`), because the publication
    /// schedule is fully observed by then. A `done == -1` ("this stream carries no
    /// gzip header") case keeps running to the end of the stream so callers can
    /// also assert the final return code — matching the differential C harness,
    /// whose loop is likewise gated on `done == 1`.
    #[cfg(feature = "gzip")]
    fn drive_header<F: FnMut(&PoisonedHeader)>(
        window_bits: c_int,
        step: usize,
        stream: &[u8],
        hdr: &mut PoisonedHeader,
        mut observe: F,
    ) -> c_int {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    window_bits,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut hdr.head) }, Z_OK);
        assert_eq!(hdr.head.done, 0, "registration clears `done` to 0");

        let mut out = vec![0u8; stream.len() * 8 + 512];
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        let mut off = 0usize;
        let mut rc = Z_OK;
        while off < stream.len() && rc == Z_OK {
            let n = core::cmp::min(step, stream.len() - off);
            strm.next_in = stream[off..].as_ptr();
            strm.avail_in = n as c_uint;
            off += n;
            rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
            observe(hdr);
            if hdr.head.done == 1 {
                break;
            }
        }
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        rc
    }

    /// Each gzip header scalar is published **only** by the parser state that
    /// assigns it in C, never eagerly.
    ///
    /// C writes into the caller's `gz_header` from inside `FLAGS` (`text`), `TIME`
    /// (`time`), `OS` (`xflags` and `os`, one statement pair under one guard) and
    /// `HCRC` (`hcrc`, `done`) — `inflate.c` L523-L524, L531-L532, L539-L542,
    /// L686-L689. A caller polling between calls therefore sees its own values in
    /// every field the stream has not reached yet, which is exactly what makes a
    /// sentinel-based "has this arrived?" test work in C.
    ///
    /// Feeding the header one byte per call and poisoning every scalar pins the
    /// ordering: at the moment `text` first changes, `time`/`xflags`/`os`/`hcrc`
    /// must still hold poison, and so on down the chain. The expectations were
    /// measured against reference C zlib built from this repository's own
    /// `inflate.c`, which yields exactly this staircase.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_publishes_each_scalar_only_when_its_own_state_runs() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);
        let mut hdr = PoisonedHeader::new(16, 16, 16);

        let mut saw_text = false;
        let mut saw_time = false;
        let mut saw_os = false;
        let rc = drive_header(31, 1, &stream, &mut hdr, |h| {
            let head = &h.head;
            if !saw_text && head.text != PoisonedHeader::SCALAR {
                saw_text = true;
                assert_eq!(head.text, 1, "FLAGS publishes the real FTEXT bit");
                assert_eq!(
                    head.time,
                    PoisonedHeader::TIME,
                    "`time` belongs to the TIME state, which has not run yet"
                );
                assert_eq!(head.xflags, PoisonedHeader::SCALAR, "OS has not run yet");
                assert_eq!(head.os, PoisonedHeader::SCALAR, "OS has not run yet");
                assert_eq!(head.hcrc, PoisonedHeader::SCALAR, "HCRC has not run yet");
            }
            if !saw_time && head.time != PoisonedHeader::TIME {
                saw_time = true;
                assert!(saw_text, "TIME runs after FLAGS");
                assert_eq!(head.time, HDR_TIME);
                assert_eq!(head.xflags, PoisonedHeader::SCALAR, "OS has not run yet");
                assert_eq!(head.os, PoisonedHeader::SCALAR, "OS has not run yet");
                assert_eq!(head.hcrc, PoisonedHeader::SCALAR, "HCRC has not run yet");
            }
            if !saw_os && head.os != PoisonedHeader::SCALAR {
                saw_os = true;
                assert!(saw_time, "OS runs after TIME");
                assert_eq!(head.os, 3);
                assert_eq!(head.xflags, 0, "C assigns xflags and os together");
                assert_eq!(head.hcrc, PoisonedHeader::SCALAR, "HCRC has not run yet");
            }
        });

        assert_eq!(rc, Z_OK);
        assert!(saw_text && saw_time && saw_os, "every state must have run");
        // HCRC is last: it publishes `hcrc` and completes the header.
        assert_eq!(hdr.head.hcrc, 1);
        assert_eq!(hdr.head.done, 1);
        assert_eq!(hdr.head.extra_len, HDR_EXTRA.len() as c_uint);
        assert_eq!(&hdr.extra[..HDR_EXTRA.len()], HDR_EXTRA);
        assert_eq!(
            &hdr.extra[HDR_EXTRA.len()..],
            &[HDR_POISON; 10],
            "the copy stops at the captured length; the tail stays the caller's"
        );
        // Nine content bytes plus C's counted NUL, then untouched poison.
        assert_eq!(&hdr.name[..HDR_NAME.len()], HDR_NAME);
        assert_eq!(hdr.name[HDR_NAME.len()], 0);
        assert!(
            hdr.name[HDR_NAME.len() + 1..]
                .iter()
                .all(|&b| b == HDR_POISON)
        );
        assert_eq!(&hdr.comment[..HDR_COMMENT.len()], HDR_COMMENT);
        assert_eq!(hdr.comment[HDR_COMMENT.len()], 0);
    }

    /// A stream that turns out **not** to carry a gzip header reports C's
    /// `done == -1`, which a [`bool`] cannot express.
    ///
    /// With auto-detect framing (`windowBits = 47`) the decoder does not know
    /// which wrapper it has until the first two bytes arrive. When they are not
    /// `1f 8b`, C's `HEAD` state runs `if (state->head != Z_NULL)
    /// state->head->done = -1;` (`inflate.c` L505-L506) and proceeds as zlib.
    /// That `-1` is the only way a caller can distinguish "there is no gzip header
    /// to wait for" from "the gzip header has not arrived yet"; collapsing it to
    /// `0` leaves such a caller polling forever.
    ///
    /// Nothing else may be touched: no gzip header exists, so every scalar keeps
    /// the caller's poison and no capture buffer is written — in particular no
    /// premature NUL, which a bulk mirror of an empty owned header would emit.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_reports_done_minus_one_for_a_zlib_stream_under_auto_detect() {
        // Both delivery granularities: byte-at-a-time (the decision is made on
        // the second byte) and the whole stream in one call.
        for step in [1usize, usize::MAX] {
            let mut hdr = PoisonedHeader::new(16, 16, 16);
            let rc = drive_header(47, step, ZLIB_STREAM, &mut hdr, |_| {});
            assert_eq!(
                rc, Z_STREAM_END,
                "the zlib stream still decodes (step {step})"
            );
            assert_eq!(
                hdr.head.done, -1,
                "a non-gzip stream must report done == -1 (step {step})"
            );
            assert_eq!(hdr.head.text, PoisonedHeader::SCALAR);
            assert_eq!(hdr.head.time, PoisonedHeader::TIME);
            assert_eq!(hdr.head.xflags, PoisonedHeader::SCALAR);
            assert_eq!(hdr.head.os, PoisonedHeader::SCALAR);
            assert_eq!(hdr.head.hcrc, PoisonedHeader::SCALAR);
            assert_eq!(hdr.head.extra_len, PoisonedHeader::XLEN);
            assert!(
                hdr.extra.iter().all(|&b| b == HDR_POISON)
                    && hdr.name.iter().all(|&b| b == HDR_POISON)
                    && hdr.comment.iter().all(|&b| b == HDR_POISON),
                "no gzip header exists, so no capture buffer may be written — \
                 not even a terminating NUL (step {step})"
            );
            assert!(
                !hdr.head.extra.is_null()
                    && !hdr.head.name.is_null()
                    && !hdr.head.comment.is_null(),
                "the no-field nulling belongs to EXLEN/NAME/COMMENT, which a \
                 non-gzip stream never reaches (step {step})"
            );
        }
    }

    /// A name or comment still arriving stays **unterminated**.
    ///
    /// C stores the field's NUL only when it actually decodes that byte
    /// (`inflate.c` L632-L637 / L654-L659), so a caller polling mid-field sees the
    /// bytes delivered so far followed by its own memory. A boundary that mirrors
    /// an owned `Vec` as a C string instead writes a terminator after every call,
    /// which reads as "the name is complete" while more bytes are still coming.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_leaves_a_partially_received_name_unterminated() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);
        let mut hdr = PoisonedHeader::new(16, 16, 16);

        let mut saw_partial_name = false;
        let mut saw_partial_comment = false;
        drive_header(31, 1, &stream, &mut hdr, |h| {
            // Number of leading bytes that are no longer poison == bytes stored.
            let stored = |buf: &[u8]| buf.iter().take_while(|&&b| b != HDR_POISON).count();
            let n = stored(&h.name);
            if n > 0 && n < HDR_NAME.len() {
                saw_partial_name = true;
                assert_eq!(&h.name[..n], &HDR_NAME[..n]);
                assert_eq!(
                    h.name[n], HDR_POISON,
                    "the byte after a partially received name must still be the \
                     caller's — C has not decoded the NUL yet"
                );
            }
            let c = stored(&h.comment);
            if c > 0 && c < HDR_COMMENT.len() {
                saw_partial_comment = true;
                assert_eq!(&h.comment[..c], &HDR_COMMENT[..c]);
                assert_eq!(h.comment[c], HDR_POISON, "no premature comment NUL");
            }
        });

        assert!(
            saw_partial_name && saw_partial_comment,
            "one byte per call must expose at least one partial state per field"
        );
        // Once complete, both carry C's counted NUL.
        assert_eq!(hdr.name[HDR_NAME.len()], 0);
        assert_eq!(hdr.comment[HDR_COMMENT.len()], 0);
    }

    /// C counts the terminating NUL against `name_max`/`comm_max` like any other
    /// decoded byte, so a name that *exactly* fills the buffer is left
    /// unterminated.
    ///
    /// `inflate.c` L632-L637 stores under `state->length < head->name_max` and
    /// increments `state->length` for the NUL too. With a nine-byte name:
    /// `name_max == 9` stores nine content bytes and drops the NUL; `name_max ==
    /// 10` stores the NUL as the tenth byte; `name_max == 5` truncates the content
    /// and never reaches the NUL. All three were confirmed against reference C.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_applies_cs_counted_nul_accounting_to_name_max() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);

        // Exact fit: nine content bytes, no room left for the counted NUL.
        let mut hdr = PoisonedHeader::new(8, HDR_NAME.len(), 8);
        drive_header(31, usize::MAX, &stream, &mut hdr, |_| {});
        assert_eq!(hdr.head.done, 1);
        assert_eq!(
            hdr.name, HDR_NAME,
            "a name that exactly fills name_max is NOT NUL-terminated, exactly \
             as in C — the terminator is one of the counted bytes"
        );

        // One more byte of capacity: the NUL now fits and is stored.
        let mut hdr = PoisonedHeader::new(8, HDR_NAME.len() + 1, 8);
        drive_header(31, usize::MAX, &stream, &mut hdr, |_| {});
        assert_eq!(hdr.head.done, 1);
        assert_eq!(&hdr.name[..HDR_NAME.len()], HDR_NAME);
        assert_eq!(hdr.name[HDR_NAME.len()], 0, "the counted NUL now fits");

        // Truncating capacity: content is cut and the NUL is never reached.
        let mut hdr = PoisonedHeader::new(4, 5, 3);
        drive_header(31, 3, &stream, &mut hdr, |_| {});
        assert_eq!(hdr.head.done, 1);
        assert_eq!(hdr.name, &HDR_NAME[..5], "content truncated, no terminator");
        assert_eq!(hdr.comment, &HDR_COMMENT[..3], "same rule for the comment");
        assert_eq!(
            hdr.extra,
            &HDR_EXTRA[..4],
            "the extra copy is clamped to extra_max while extra_len reports XLEN"
        );
        assert_eq!(hdr.head.extra_len, HDR_EXTRA.len() as c_uint);
    }

    /// An **absent** optional field nulls the caller's pointer and leaves its
    /// buffer untouched.
    ///
    /// C assigns `head->extra = Z_NULL` (`inflate.c` L605-L606), `head->name =
    /// Z_NULL` (L643-L644) and `head->comment = Z_NULL` (L665-L666) on the
    /// respective "flag not set" branches. That store is how a C caller tells "the
    /// header declared no such field" from "it declared one"; leaving a stale
    /// non-null pointer misreports an absent field as present. The buffer itself
    /// is never written, so the caller's bytes must survive intact.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_nulls_the_pointer_of_every_absent_field() {
        // One case per field, plus the bare header that omits all three.
        let cases: [(HeaderFields, bool, bool, bool); 4] = [
            (
                HeaderFields {
                    extra: false,
                    ..HeaderFields::ALL
                },
                true,
                false,
                false,
            ),
            (
                HeaderFields {
                    name: false,
                    ..HeaderFields::ALL
                },
                false,
                true,
                false,
            ),
            (
                HeaderFields {
                    comment: false,
                    ..HeaderFields::ALL
                },
                false,
                false,
                true,
            ),
            (HeaderFields::NONE, true, true, true),
        ];

        for (fields, extra_absent, name_absent, comment_absent) in cases {
            let stream = gzip_stream_with_fields(fields);
            let mut hdr = PoisonedHeader::new(16, 16, 16);
            let rc = drive_header(31, 1, &stream, &mut hdr, |_| {});
            assert_eq!(rc, Z_OK);
            assert_eq!(hdr.head.done, 1, "the header still completes");

            assert_eq!(
                hdr.head.extra.is_null(),
                extra_absent,
                "extra pointer nulled iff the header declared no FEXTRA"
            );
            assert_eq!(
                hdr.head.name.is_null(),
                name_absent,
                "name pointer nulled iff the header declared no FNAME"
            );
            assert_eq!(
                hdr.head.comment.is_null(),
                comment_absent,
                "comment pointer nulled iff the header declared no FCOMMENT"
            );

            if extra_absent {
                assert!(hdr.extra.iter().all(|&b| b == HDR_POISON));
                assert_eq!(
                    hdr.head.extra_len,
                    PoisonedHeader::XLEN,
                    "no FEXTRA means EXLEN never assigns extra_len either"
                );
            } else {
                assert_eq!(hdr.head.extra_len, HDR_EXTRA.len() as c_uint);
            }
            if name_absent {
                assert!(
                    hdr.name.iter().all(|&b| b == HDR_POISON),
                    "an absent name must not write a single byte, NUL included"
                );
            }
            if comment_absent {
                assert!(hdr.comment.iter().all(|&b| b == HDR_POISON));
            }
            // The HCRC state always publishes `hcrc` and `done`, even when the
            // FHCRC bit is clear (`inflate.c` L686-L689 is outside the
            // `flags & 0x0200` guard).
            assert_eq!(hdr.head.hcrc, c_int::from(fields.hcrc));
        }
    }

    /// The same schedule holds when the FHCRC bit is clear: the
    /// `HCRC` state's `head->hcrc = (flags >> 9) & 1; head->done = 1;` pair sits
    /// *outside* the `flags & 0x0200` guard (`inflate.c` L686-L689), so `hcrc` is
    /// published as `0` rather than left poisoned.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_publishes_a_zero_hcrc_when_the_stream_carries_no_header_crc() {
        let stream = gzip_stream_with_fields(HeaderFields {
            hcrc: false,
            ..HeaderFields::ALL
        });
        let mut hdr = PoisonedHeader::new(16, 16, 16);
        drive_header(31, 1, &stream, &mut hdr, |_| {});
        assert_eq!(hdr.head.done, 1);
        assert_eq!(
            hdr.head.hcrc, 0,
            "HCRC publishes the FHCRC bit unconditionally, so a stream without \
             one reports 0 instead of keeping the caller's value"
        );
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

    /// `inflateBack` must restore `next_in`/`avail_in` to the unconsumed
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

    /// When the `in` callback runs dry, `inflateBack` must null `next_in` and
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

    /// `inflateBackInit_` must **use the caller's window**, not
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

    /// Regression guard: when the caller's `zalloc` cannot satisfy the copy,
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

    /// `inflateBackEnd` must reject a stream whose handle is NOT an
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

    /// `inflateEnd` must reject an `inflateBack` handle (tag INFLATE_BACK, not
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

    /// End-to-end out-of-memory propagation: with the caller's arena exhausted part-way through
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

    /// A non-null sentinel cookie, used to prove exactly which prologue branch
    /// clears `z_stream.opaque`. Never dereferenced.
    fn opaque_sentinel() -> *mut c_void {
        core::ptr::without_provenance_mut::<c_void>(0x5EED_C0DE)
    }

    /// CQ-1 regression — `inflateInit2_` must *complete* a half-present
    /// `zalloc`/`zfree` pair exactly as C does, not reject it.
    ///
    /// C's prologue substitutes only the **missing** half — `zcalloc` for a null
    /// `zalloc` (also clearing `opaque`), `zcfree` for a null `zfree` (which
    /// leaves `opaque` alone) — and then proceeds (`inflate.c` L182-L199):
    ///
    /// * `zalloc` only, deliberately failing: the caller's hook **is** consulted,
    ///   so the observable code is `Z_MEM_ERROR`, the missing `zfree` was filled
    ///   in, and `opaque` survived untouched.
    /// * `zfree` only: the built-in `zalloc` was filled in so initialization
    ///   **succeeds**, `opaque` was cleared (that is C's `zalloc` branch), and the
    ///   caller's `zfree` really does release the state at `inflateEnd`.
    #[test]
    fn init_completes_a_half_present_allocator_pair_exactly_as_c_does() {
        // --- zalloc only, and it refuses ------------------------------------
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        strm.opaque = opaque_sentinel();
        strm.msg = c"stale".as_ptr().cast_mut();
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_MEM_ERROR,
            "the caller's zalloc must be consulted, so their out-of-memory signal \
             surfaces as Z_MEM_ERROR exactly as it does in C"
        );
        assert!(
            strm.zfree.is_some(),
            "the missing zfree half must be substituted in place (`inflate.c` L191-L196)"
        );
        assert!(strm.zalloc.is_some(), "the caller's own half must be kept");
        assert_eq!(
            strm.opaque,
            opaque_sentinel(),
            "C clears `opaque` only on the zalloc branch (`inflate.c` L186-L189)"
        );
        assert!(strm.state.is_null(), "a failed init must install no state");

        // --- zfree only: initialization succeeds ----------------------------
        let stats = BuiltinHookStats::new();
        let mut strm = zeroed_stream();
        strm.zfree = Some(stats.zfree_fn());
        strm.opaque = opaque_sentinel();
        strm.msg = c"stale".as_ptr().cast_mut();
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a caller who supplied only zfree gets the built-in zalloc and a \
             working stream, exactly as in C"
        );
        assert!(
            strm.zalloc.is_some(),
            "the missing zalloc half must be substituted in place (`inflate.c` L183-L190)"
        );
        assert!(strm.msg.is_null(), "`strm->msg` is cleared unconditionally");
        assert!(
            strm.opaque.is_null(),
            "C clears `opaque` on the zalloc branch, because the cookie belonged \
             to the allocator being replaced (`inflate.c` L186-L189)"
        );
        assert!(!strm.state.is_null());

        // The caller's own `zfree` — invoked with the cleared, null `opaque`, just
        // as C invokes it — is what releases the state reservation. C's
        // `inflateInit2_` makes exactly one allocation (`inflate.c` L197-L199);
        // the window is deferred to the first `inflate` call.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            stats.allocs(),
            0,
            "the caller supplied no zalloc, so none of their allocations ran"
        );
        assert_eq!(
            stats.frees(),
            1,
            "the caller's zfree releases C's single `inflateInit2_` allocation, \
             the state reservation (`inflate.c` L197-L199)"
        );
    }

    /// The prologue runs in exactly C's position: after the version and
    /// null-stream guards, and before the state allocation and the `windowBits`
    /// validation (`inflate.c` L173-L214).
    #[test]
    fn the_allocator_prologue_is_ordered_exactly_as_c_orders_it() {
        // A bad version *and* a half hook: the version guard precedes the
        // prologue, so neither the hooks nor `msg` may be touched at all.
        let stale = c"stale".as_ptr().cast_mut();
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        strm.msg = stale;
        assert_eq!(
            unsafe { inflateInit_(&mut strm, ptr::null(), size_of::<z_stream>() as c_int) },
            Z_VERSION_ERROR,
            "the version guard must still outrank the allocator prologue"
        );
        assert!(strm.state.is_null());
        assert!(
            strm.zfree.is_none(),
            "C returns Z_VERSION_ERROR before the prologue, so no substitution occurs"
        );
        assert_eq!(
            strm.msg, stale,
            "C returns Z_VERSION_ERROR before `strm->msg = Z_NULL`"
        );

        // An out-of-range `windowBits` with a *working* half hook: the prologue
        // completes the pair, the state reservation is served through the caller's
        // `zalloc`, and only then is the parameter rejected — because C validates
        // `windowBits` inside `inflateReset2`, *after* its state `ZALLOC`
        // (`inflate.c` L197-L213). The substituted half then releases the state,
        // so the counting `zalloc` sees one call and the counting side sees no
        // free at all.
        let stats = BuiltinHookStats::new();
        let mut strm = zeroed_stream();
        strm.zalloc = Some(stats.zalloc_fn());
        strm.msg = stale;
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    99,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_STREAM_ERROR
        );
        assert!(strm.state.is_null());
        assert!(
            strm.zfree.is_some(),
            "the prologue precedes the state allocation, so the pair was completed \
             even though the init then failed"
        );
        assert!(
            strm.msg.is_null(),
            "`strm->msg = Z_NULL` is unconditional and precedes every later return"
        );
        assert_eq!(
            stats.allocs(),
            1,
            "C reserves the state through the caller's zalloc before validating \
             `windowBits` (`inflate.c` L197-L213)"
        );
        assert_eq!(
            stats.frees(),
            0,
            "the substituted built-in zfree released the state, not the caller's \
             counting half"
        );

        // An out-of-range `windowBits` *and* a refusing `zalloc`: because C's
        // state `ZALLOC` precedes the `windowBits` check, the allocator's verdict
        // is what the caller observes — `Z_MEM_ERROR`, not `Z_STREAM_ERROR`.
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    99,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_MEM_ERROR,
            "C's `ZALLOC` precedes `inflateReset2`'s windowBits validation \
             (`inflate.c` L197-L213), so out-of-memory outranks the bad parameter"
        );
        assert!(strm.state.is_null());
    }

    /// The other half of the matrix: the two *complete* hook configurations must
    /// be unaffected. Neither half supplied is the overwhelmingly common case (a
    /// zeroed `z_stream`); both halves supplied routes through the caller's
    /// allocator.
    ///
    /// A wholly absent pair is substituted per half, exactly as C substitutes
    /// `zcalloc`/`zcfree` (`inflate.c` L183-L196), so the caller's `z_stream`
    /// publishes two non-null halves afterwards. That publication is pure ABI
    /// shape: [`crate::ffi::types::CAllocator::is_builtin_pair`] recognizes the
    /// crate's own substitutes and reports *no* hook, so the engine keeps using
    /// the global-allocator path and a hookless caller's allocation count and
    /// engine-state footprint stay byte-for-byte what they have always been
    /// (AAP §0.6.5).
    #[test]
    fn init_accepts_both_hooks_and_neither_hook() {
        // Neither: both halves substituted in place, global allocator still used.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a zeroed z_stream must still initialize"
        );
        assert!(!strm.state.is_null());
        assert!(
            strm.zalloc.is_some() && strm.zfree.is_some(),
            "C substitutes each missing half unconditionally (`inflate.c` \
             L183-L196), so a hookless caller's stream publishes a complete pair"
        );
        assert!(
            crate::ffi::types::publishes_builtin_alloc_pair(&strm),
            "the published pair must be the crate's own built-ins — the \
             counterpart of C's zcalloc/zcfree — not a caller hook"
        );
        assert!(strm.opaque.is_null(), "a zeroed cookie stays zeroed");
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);

        // Both: the caller's allocator backs the state.
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };
        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        let cookie = strm.opaque;
        assert_eq!(
            unsafe { inflateInit_(&mut strm, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a complete hook pair with budget must initialize"
        );
        assert!(!strm.state.is_null());
        assert!(
            core::ptr::eq(strm.opaque, cookie),
            "a complete pair is left exactly as the caller supplied it"
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert!(
            budget.remaining.load(core::sync::atomic::Ordering::SeqCst) < 64,
            "a complete pair must actually route the working buffers through the \
             caller's zalloc, not through the built-in"
        );
    }

    /// The `inflateBack` twin: `infback.c` carries the identical prologue
    /// (L36-L50), so it must reach the identical verdict — completion, not
    /// rejection.
    #[test]
    fn back_init_completes_a_half_present_allocator_pair_exactly_as_c_does() {
        let mut window = vec![0u8; 1 << 15];

        // --- zalloc only, and it refuses ------------------------------------
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        strm.opaque = opaque_sentinel();
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
            "the caller's zalloc must be consulted, so their out-of-memory signal \
             surfaces as Z_MEM_ERROR exactly as it does in C"
        );
        assert!(
            strm.zfree.is_some(),
            "the missing zfree half must be substituted in place (`infback.c` L45-L50)"
        );
        assert_eq!(
            strm.opaque,
            opaque_sentinel(),
            "C clears `opaque` only on the zalloc branch (`infback.c` L40-L43)"
        );
        assert!(
            strm.state.is_null(),
            "a failed inflateBackInit_ must install no state"
        );

        // --- zfree only: initialization succeeds ----------------------------
        let stats = BuiltinHookStats::new();
        let mut strm = zeroed_stream();
        strm.zfree = Some(stats.zfree_fn());
        strm.opaque = opaque_sentinel();
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
            "a caller who supplied only zfree gets the built-in zalloc and a \
             working inflateBack state, exactly as in C"
        );
        assert!(
            strm.zalloc.is_some(),
            "the missing zalloc half must be substituted in place (`infback.c` L37-L44)"
        );
        assert!(
            strm.opaque.is_null(),
            "C clears `opaque` on the zalloc branch (`infback.c` L40-L43)"
        );
        assert!(!strm.state.is_null());

        // C's `inflateBackInit_` makes exactly one allocation — the state
        // (`infback.c` L51) — because the window belongs to the caller and is
        // adopted, never allocated or freed.
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
        assert_eq!(
            stats.allocs(),
            0,
            "the caller supplied no zalloc, so none of their allocations ran"
        );
        assert_eq!(
            stats.frees(),
            1,
            "the caller's zfree releases the single state reservation and nothing \
             else: the lent window is never freed (`infback.c` L572-L577)"
        );

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

    /// `inflateCopy` reads the *source's* raw allocator fields, exactly as C does
    /// through `inflateStateCheck(source)` (`inflate.c` L90-L91) followed by
    /// `ZALLOC(source, …)` (L1310-L1329), and then hands the destination the
    /// source's whole allocator triple through
    /// `zmemcpy(dest, source, sizeof(z_stream))` (L1355). Both of the *valid*
    /// hook configurations are asserted:
    ///
    /// * a source initialized with **no** hooks at all: the prologue substituted
    ///   the crate's built-ins for both halves (see
    ///   [`crate::ffi::types::init_allocator_prologue`]), the source is still a
    ///   valid stream, the copy succeeds, and the destination inherits the same
    ///   built-in triple — which is what selects the global allocator for it too,
    ///   because [`crate::ffi::types::CAllocator::is_builtin_pair`] reports no
    ///   hook for it;
    /// * a source with a **complete** caller-supplied pair: the copy succeeds and
    ///   the destination inherits both halves *and* the cookie verbatim, so its
    ///   buffers are charged to the caller's arena rather than the global heap.
    ///
    /// The invalid configuration — a half-present pair — is the subject of
    /// [`copy_rejects_a_half_present_source_allocator`].
    #[test]
    fn copy_honors_the_source_allocator_fields_exactly_as_c_does() {
        // --- a wholly unhooked source ---------------------------------------
        let mut src = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut src, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        assert!(
            crate::ffi::types::publishes_builtin_alloc_pair(&src),
            "both halves are substituted with the crate's built-ins, which report \
             no hook, so a hookless caller's footprint is unchanged (AAP §0.6.5)"
        );

        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut src) },
            Z_OK,
            "a stream this crate initialized always passes inflateStateCheck's \
             hook clause"
        );
        assert!(!dst.state.is_null());
        assert!(
            crate::ffi::types::publishes_builtin_alloc_pair(&dst) && dst.opaque.is_null(),
            "C's zmemcpy hands the destination the source's allocator triple \
             verbatim, substituted built-ins included"
        );
        assert_eq!(unsafe { inflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);

        // --- a source with a complete caller-supplied pair -------------------
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(256),
        };
        let mut src = zeroed_stream();
        attach_budget(&mut src, &budget);
        let cookie = src.opaque;
        assert_eq!(
            unsafe { inflateInit_(&mut src, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let mut dst = zeroed_stream();
        assert_eq!(unsafe { inflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());
        assert!(
            dst.zalloc.is_some() && dst.zfree.is_some(),
            "a complete pair is inherited by the destination"
        );
        assert!(
            core::ptr::eq(dst.opaque, cookie),
            "the cookie travels with the hooks, so the destination's regions are \
             charged to the same arena"
        );
        assert_eq!(unsafe { inflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
    }

    /// `inflateCopy` reads the *source's* raw allocator fields, so it carries the
    /// same exposure as the `*Init*_` shims. C rejects a half-present pair through
    /// `inflateStateCheck(source)` (`inflate.c` L90-L91) before its
    /// `ZALLOC(source, …)`, and so must this shim — otherwise the destination
    /// would be backed by the global heap while the caller believes their hook
    /// owns it.
    #[test]
    fn copy_rejects_a_half_present_source_allocator() {
        let mut src = zeroed_stream();
        assert_eq!(
            unsafe { inflateInit_(&mut src, VERSION.as_ptr(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Mutate the source into an invalid half-present pair after a successful
        // init, which is the only way this state is reachable: the prologue
        // always leaves two non-null halves behind, so the mutation has to null
        // one of them. Both directions are exercised.
        let published_zalloc = src.zalloc;
        let published_zfree = src.zfree;

        src.zfree = None;
        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut src) },
            Z_STREAM_ERROR,
            "a source whose zfree was nulled after init must be rejected, exactly \
             as `inflateStateCheck` (`inflate.c` L90-L91) rejects it"
        );
        assert!(dst.state.is_null(), "a rejected copy must install no state");
        src.zfree = published_zfree;

        src.zalloc = None;
        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut src) },
            Z_STREAM_ERROR,
            "a half-present source allocator must not yield a destination backed \
             by the global heap while the caller believes their hook owns it"
        );
        assert!(dst.state.is_null(), "a rejected copy must install no state");
        src.zalloc = published_zalloc;

        assert_eq!(unsafe { inflateEnd(&mut src) }, Z_OK);
    }
}
