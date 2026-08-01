//! `extern "C"` shims for the zlib **deflate** (compression) API.
//!
//! This module is the crate's designated `unsafe` boundary for compression.
//! Each function reproduces the exact C signature of its zlib counterpart so
//! the emitted `cdylib`/`staticlib` is a drop-in replacement for the reference
//! library. The shims validate raw C inputs, bridge the `#[repr(C)]`
//! [`z_stream`] to the fully **safe** engine in [`crate::deflate`], own the
//! boxed idiomatic state through the opaque `z_stream.state` handle, and
//! re-materialize zlib's integer return codes at the boundary.
//!
//! ## Safe-core / unsafe-boundary split
//!
//! The compression engine (`src/deflate/**`) contains **zero** `unsafe`. All
//! raw-pointer handling lives here: every `unsafe` block carries a `// SAFETY:`
//! justification, and every fallible body is wrapped in
//! `crate::ffi::types::guard_int` / `crate::ffi::types::guard_ulong`
//! (`catch_unwind`) so that a Rust panic can never unwind across the C ABI.
//!
//! ## State-handle model
//!
//! `z_stream.state` stores a `Box<DeflateHandle>` — a `#[repr(C)]` struct whose
//! first field is a [`HandleKind`] discriminant at offset 0 and whose `zs` field
//! is the idiomatic [`ZStream<CAllocator>`](ZStream) the engine actually drives.
//! Initialization builds that stream (honoring any caller-supplied
//! `zalloc`/`zfree`), wraps it in a `DeflateHandle` tagged
//! [`HandleKind::DEFLATE`], and installs the box via [`state_ptr_from_box`].
//!
//! Every subsequent call goes through a **tag-validating** accessor rather than
//! a blind pointer cast: [`deflate_state`] reads the [`HandleKind`] through the
//! shared header prefix, confirms it is `DEFLATE`, and only then reborrows
//! `handle.zs`; [`deflate_take`] does the same before reconstituting the
//! `Box<DeflateHandle>`. `deflateEnd` reclaims the box that way and drops it,
//! letting RAII run the engine cleanup that C performed manually in
//! `deflateEnd`. `deflateCopy` deep-clones the engine into a fresh tagged box.
//!
//! Ownership is exactly-once: installation moves the box into `state`, and
//! [`deflate_take`] nulls `state` as it reclaims it, so a second `deflateEnd`
//! finds nothing to free. A stream carrying another engine's handle fails the
//! tag check, leaves `state` untouched, and yields `Z_STREAM_ERROR` instead of
//! deallocating with a mismatched `Layout`.
//!
//! ## Defined rejections — deliberately part of the caller contract
//!
//! A C consumer cannot be trusted to be well-formed. Reference zlib already
//! answers *some* malformed calls with a defined error — that is what
//! `deflateStateCheck` is for — and this boundary deliberately extends the same
//! treatment to the rest, so that every case below is a diagnosable error
//! instead of undefined behavior. The following inputs are therefore
//! **explicitly within** the contract of every non-initializing shim in this
//! module: each is detected and rejected with `Z_STREAM_ERROR` (`Z_MEM_ERROR` is
//! never fabricated for them), nothing is dereferenced beyond the plain `Copy`
//! fields of the `z_stream` itself, and no allocation is created, freed, or
//! reinterpreted.
//!
//! * **A null `z_streamp`.** Tested first in every body, before any field read.
//! * **A zeroed or never-initialized `z_stream`.** `state` is null, which
//!   [`peek_handle_kind`] reports as "no handle" without loading through it.
//! * **A stream whose `*End` already ran.** Teardown nulls `state`, so this is
//!   indistinguishable from the previous case: a second `deflateEnd` is a
//!   defined `Z_STREAM_ERROR`, never a double free.
//! * **A stream carrying another engine's handle** — for example `deflateEnd`
//!   on an `inflateInit2_`-initialized stream, or the converse. The
//!   [`HandleKind`] tag at offset 0 is compared before the pointer is
//!   reinterpreted, so a cross-engine terminator can neither drop with a
//!   mismatched `Layout` nor observe another engine's fields. This one is a
//!   deliberate hardening **beyond** the C contract: reference zlib's
//!   `deflateEnd` would cast the opaque `state` blindly, so portable C code must
//!   still never do it — but against this implementation it is defined.
//! * **Inconsistent buffer descriptors** on the entry points that take them: a
//!   null `next_out`, or a null `next_in` paired with a non-zero `avail_in`.
//!   These fail `stream_buffers_valid`/`input_ptr_valid` and are rejected
//!   *before* any slice is formed.
//!
//! What remains the caller's obligation is only this: when a buffer pointer is
//! non-null and its `avail_*` is non-zero, that many bytes must really be
//! readable (input) or writable (output) for the duration of the call, and the
//! two regions must not overlap. A pointer that is merely *unused* on the path
//! taken need not be valid — [`deflateParams`] documents the one case where
//! that distinction is observable.

// zlib's public symbols are camelCase C identifiers; `#[unsafe(no_mangle)]`
// exempts them from `non_snake_case`, but this keeps the module warning-free
// regardless of how the exported names are spelled.
#![allow(non_snake_case)]

use core::ffi::{c_char, c_int, c_uint};
use core::{ptr, slice};

use crate::constants::{DEF_MEM_LEVEL, MAX_WBITS, Strategy, Z_DEFAULT_STRATEGY, Z_DEFLATED};
use crate::deflate as engine;
use crate::error::{ReturnCode, ZlibError};
use crate::ffi::alloc::try_box;
use crate::ffi::types::*;
use crate::stream::ZStream;

// ===========================================================================
// Integer return codes (materialized locally from `ReturnCode`).
//
// `constants.rs` intentionally does not expose bare `Z_OK`/`Z_STREAM_ERROR`
// integer aliases, so the boundary defines exactly the ones it emits using the
// `const fn` `ReturnCode::as_c_int`.
// ===========================================================================

/// `Z_OK` — successful completion.
const Z_OK: c_int = ReturnCode::Ok.as_c_int();
/// `Z_STREAM_ERROR` — inconsistent stream state or invalid parameters.
const Z_STREAM_ERROR: c_int = ReturnCode::StreamError.as_c_int();
/// `Z_BUF_ERROR` — no progress was possible (e.g. output buffer too small).
const Z_BUF_ERROR: c_int = ReturnCode::BufError.as_c_int();
/// `Z_VERSION_ERROR` — header/library version or `z_stream` size mismatch.
const Z_VERSION_ERROR: c_int = ReturnCode::VersionError.as_c_int();
/// `Z_MEM_ERROR` — an allocation (engine buffer or opaque handle) was refused.
const Z_MEM_ERROR: c_int = ReturnCode::MemError.as_c_int();

// ===========================================================================
// Boundary helpers
// ===========================================================================

/// Collapse an engine `Result<ReturnCode, ZlibError>` into the C integer code.
#[inline]
fn code_of(result: Result<ReturnCode, ZlibError>) -> c_int {
    match result {
        Ok(rc) => rc.as_c_int(),
        Err(err) => err.as_return_code().as_c_int(),
    }
}

/// Map an idiomatic stream's `msg` (`Option<&'static str>`) to a NUL-terminated
/// C string pointer, or `NULL` when there is no message.
///
/// The compression engine keeps `msg` clear on the deflate path, so this
/// resolves to `NULL` in practice; the mapping is retained so the raw
/// `z_stream.msg` faithfully mirrors any message the engine may set.
#[inline]
fn msg_ptr(zs: &ZStream<CAllocator>) -> *const c_char {
    match zs.msg {
        None => ptr::null(),
        Some(text) => cmsg(text),
    }
}

/// Translate a known zlib message string into a `'static` NUL-terminated C
/// pointer. Unknown strings map to `NULL` (never fabricates a dangling buffer).
#[inline]
fn cmsg(text: &str) -> *const c_char {
    match text {
        "need dictionary" => c"need dictionary".as_ptr(),
        "stream end" => c"stream end".as_ptr(),
        "file error" => c"file error".as_ptr(),
        "stream error" => c"stream error".as_ptr(),
        "data error" => c"data error".as_ptr(),
        "insufficient memory" => c"insufficient memory".as_ptr(),
        "buffer error" => c"buffer error".as_ptr(),
        "incompatible version" => c"incompatible version".as_ptr(),
        _ => ptr::null(),
    }
}

// ===========================================================================
// Phase 1 — Initialization shims
//
// The linkable symbols are the `_`-suffixed forms carrying `(version,
// stream_size)`. The bare `deflateInit`/`deflateInit2` names are C header
// macros and are deliberately NOT defined here.
// ===========================================================================

/// `int deflateInit2_(z_streamp strm, int level, int method, int windowBits,`
/// `int memLevel, int strategy, const char *version, int stream_size)`
///
/// Full-control initializer. Reproduces the C version guard
/// (`deflate.c` L392-396): a `NULL` version, a leading version byte other than
/// `'1'`, or a `stream_size` that disagrees with `sizeof(z_stream)` yields
/// `Z_VERSION_ERROR`.
///
/// # Safety
///
/// `strm` must be null or a valid, properly aligned pointer to a `z_stream`
/// that the caller owns exclusively for the duration of the call (typically a
/// zero-initialized stream). `version` must be null or point to a
/// NUL-terminated C string. On success the crate installs an owned state handle
/// into `strm.state` that must be released with [`deflateEnd`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateInit2_(
    strm: z_streamp,
    level: c_int,
    method: c_int,
    window_bits: c_int,
    mem_level: c_int,
    strategy: c_int,
    version: *const c_char,
    stream_size: c_int,
) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        // Reproduce C `deflateInit2_`'s version guard (deflate.c L392-396): a
        // null `version`, a leading byte other than the major-version digit
        // `'1'`, or a `z_stream` size mismatch each yield `Z_VERSION_ERROR`.
        if version.is_null() {
            return Z_VERSION_ERROR;
        }
        // SAFETY: `version` is non-null (checked immediately above); the caller
        // guarantees it addresses a readable NUL-terminated C string, so its
        // first byte is valid to read.
        let version_major = unsafe { *version };
        if version_major != b'1' as c_char
            || stream_size != core::mem::size_of::<z_stream>() as c_int
        {
            return Z_VERSION_ERROR;
        }
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and, per the zlib contract, points at a
        // valid `z_stream` uniquely owned by the caller for this call.
        let s = unsafe { &mut *strm };

        // Run C's allocator prologue here, in exactly the position C runs it:
        // straight after the version and null-stream guards and *before* the
        // level/method/`windowBits`/`memLevel`/strategy validation
        // (`deflate.c` L399-L414). It clears `strm->msg` unconditionally — so a
        // stale diagnostic cannot survive into a failed init, including one that
        // fails in the parameter validation below — and substitutes the crate's
        // built-in for a missing half of the `zalloc`/`zfree` pair, clearing
        // `opaque` only when it is `zalloc` that was defaulted. The caller's own
        // half is kept, so a deliberately failing `zalloc` still reports
        // `Z_MEM_ERROR` rather than being bypassed.
        // SAFETY: `s` is a valid, exclusively-owned `&mut z_stream`; only its
        // plain `Copy` `msg`/allocator fields are read and written, and no hook
        // pointer is dereferenced.
        let allocator = unsafe { init_allocator_prologue(s) };

        // The engine takes a validated `Strategy` enum; an out-of-range value
        // is rejected exactly as C's `deflateInit2_` rejects it.
        let Some(strategy) = Strategy::from_c_int(strategy) else {
            return Z_STREAM_ERROR;
        };

        // Build the idiomatic stream on the post-prologue allocator triple: the
        // caller's `zalloc`/`zfree`/`opaque` where supplied, the crate's built-in
        // where substituted, and the global allocator when the caller supplied
        // neither half (AAP §0.6.3).
        let mut zs = ZStream::with_allocator(allocator);

        if let Err(err) =
            engine::deflate_init2(&mut zs, level, method, window_bits, mem_level, strategy)
        {
            // Publish the engine's diagnostic before bailing out. C's
            // working-buffer failure stores `ERR_MSG(Z_MEM_ERROR)` into
            // `strm->msg` and its own internal `deflateEnd` leaves the field
            // alone (`deflate.c` L505-L514, L1293-L1310), so the message is still
            // readable by the caller after `deflateInit2_` returns `Z_MEM_ERROR`.
            // Returning without this write left `strm->msg` NULL where C supplies
            // "insufficient memory". `msg_ptr` yields null when the engine set no
            // message, which is exactly C's state-object-failure behavior.
            set_msg(s, msg_ptr(&zs));
            return err.as_return_code().as_c_int();
        }

        // Snapshot the engine's initial observable state before the stream is
        // moved into its box.
        let adler = zs.adler;
        let data_type = zs.data_type;
        let msg = msg_ptr(&zs);

        // Box the handle **fallibly** before touching `s`. `Box::new` aborts the
        // process when the Rust global heap cannot hold the handle, which would
        // turn a recoverable condition into process death after the engine's own
        // allocations already succeeded; C reports every failed allocation as
        // `Z_MEM_ERROR` (AAP §0.6.5). On failure `zs` drops here, releasing every
        // working buffer through the caller's `zfree`, and the caller's `z_stream`
        // is left exactly as it was found.
        let Some(handle) = try_box(DeflateHandle::new(zs)) else {
            return Z_MEM_ERROR;
        };

        // Install the boxed state. C overwrites `strm->state` unconditionally;
        // callers must not re-init without `deflateEnd` (documented contract).
        // SAFETY: transfers ownership of the tagged `Box<DeflateHandle>` into the
        // opaque `state` handle; it is reclaimed exactly once by `deflateEnd`,
        // whose `deflate_take` validates the `HandleKind::DEFLATE` tag first and
        // nulls `state` on success.
        s.state = unsafe { state_ptr_from_box(handle) };
        s.total_in = 0;
        s.total_out = 0;
        set_msg(s, msg);
        set_data_type(s, data_type);
        set_adler(s, adler);
        Z_OK
    })
}

/// `int deflateInit_(z_streamp strm, int level, const char *version,`
/// `int stream_size)`
///
/// Convenience initializer; delegates to [`deflateInit2_`] with zlib's defaults
/// (`Z_DEFLATED`, `MAX_WBITS`, `DEF_MEM_LEVEL`, `Z_DEFAULT_STRATEGY`), exactly
/// as the C macro in `deflate.c` L379 does.
///
/// # Safety
///
/// Same contract as [`deflateInit2_`]: `strm` must be null or a valid,
/// exclusively-owned `z_stream`, and `version` must be null or a NUL-terminated
/// C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateInit_(
    strm: z_streamp,
    level: c_int,
    version: *const c_char,
    stream_size: c_int,
) -> c_int {
    // SAFETY: forwards the raw inputs unchanged to `deflateInit2_`, which
    // performs the null/version validation before any dereference.
    unsafe {
        deflateInit2_(
            strm,
            level,
            Z_DEFLATED,
            MAX_WBITS,
            DEF_MEM_LEVEL,
            Z_DEFAULT_STRATEGY,
            version,
            stream_size,
        )
    }
}

// ===========================================================================
// Phase 2 — Core driver
// ===========================================================================

/// `int deflate(z_streamp strm, int flush)`
///
/// Advances the compression state machine. All seven flush modes
/// (`Z_NO_FLUSH` … `Z_TREES`) are validated by the engine exactly as C does
/// (`flush < 0 || flush > Z_BLOCK` is rejected with `Z_STREAM_ERROR`). Returns
/// `Z_STREAM_END` once a `Z_FINISH` request completes and `Z_BUF_ERROR` when no
/// forward progress is possible. Input consumption and output production are
/// written back onto the raw `z_stream`, along with `adler`, `data_type`, and
/// `msg`.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` previously initialized by
/// `deflateInit*` (its `state` handle unmodified by the caller). When
/// `avail_in` / `avail_out` are non-zero, `next_in` / `next_out` must address
/// that many valid readable / writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflate(strm: z_streamp, flush: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and points at a valid, uniquely-owned
        // `z_stream` for the duration of this call.
        let s = unsafe { &mut *strm };

        // C `deflate` entry validation (`deflate.c` L981-L1010): a null
        // `next_out`, or a positive `avail_in` paired with a null `next_in`, is
        // a `Z_STREAM_ERROR`. This must run *before* the buffers are bridged so
        // the programmer error is surfaced rather than silently masked into an
        // empty slice by `input_slice`/`output_slice`. (The remaining C guards
        // — finish-state-with-wrong-flush and the `avail_out == 0` buffer error
        // — are already enforced inside the engine's `deflate` driver.)
        if !stream_buffers_valid(s) {
            return Z_STREAM_ERROR;
        }

        // Bridge the raw C buffers to slices. These carry detached lifetimes and
        // alias the caller's external buffers (never the `z_stream` struct), so
        // the subsequent `&mut` reborrow of `s` for `deflate_state` is sound.
        // SAFETY: `next_in`/`avail_in` describe a readable input region.
        let input = unsafe { input_slice(s) };
        // SAFETY: `next_out`/`avail_out` describe a writable region disjoint
        // from `input`, per the zlib API contract.
        let output = unsafe { output_slice(s) };

        // Run the engine against the boxed state, then release the borrow before
        // writing observable fields back onto the raw stream.
        let (code, consumed, produced, adler, data_type, msg) = {
            // SAFETY: `state`, when non-null, was installed by `deflateInit*` as
            // a `Box<DeflateHandle>`; `deflate_state` confirms the
            // `HandleKind::DEFLATE` tag before reborrowing `handle.zs`, and
            // returns `None` for any other engine's handle.
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            let outcome = engine::deflate(zs, input, output, flush);
            (
                outcome.code.as_c_int(),
                outcome.consumed,
                outcome.produced,
                zs.adler,
                zs.data_type,
                msg_ptr(zs),
            )
        };

        // SAFETY: `consumed <= input.len() == avail_in` (engine invariant).
        unsafe { advance_input(s, consumed) };
        // SAFETY: `produced <= output.len() == avail_out` (engine invariant).
        unsafe { advance_output(s, produced) };
        set_adler(s, adler);
        set_data_type(s, data_type);
        set_msg(s, msg);
        code
    })
}

// ===========================================================================
// Phase 3 — Lifecycle: end / reset
// ===========================================================================

/// `int deflateEnd(z_streamp strm)`
///
/// Reclaims the boxed engine state and drops it (RAII replaces C's manual
/// `zcfree` cleanup). Mirrors C's return contract: `Z_DATA_ERROR` if the stream
/// was freed while still in a busy (non-finished) state, otherwise `Z_OK`.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`. On
/// return the `state` handle has been reclaimed and `strm.state` is null, so it
/// must not be reused without re-initialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateEnd(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        // SAFETY: `deflate_take` validates the handle's `HandleKind` tag BEFORE
        // reconstituting the box, so it returns the `Box<DeflateHandle>` only for
        // a genuine deflate handle (nulling `s.state` for exactly-once reclaim)
        // and `None` for a cross-type stream (e.g. one from `inflateInit*`) —
        // yielding `Z_STREAM_ERROR` here WITHOUT a layout-mismatched free
        // It matches C's `deflateEnd`, which rejects a non-deflate
        // stream with `Z_STREAM_ERROR`.
        let Some(mut boxed) = (unsafe { deflate_take(s) }) else {
            return Z_STREAM_ERROR;
        };
        // The engine computes the `Z_DATA_ERROR`-if-busy result and clears its
        // inner state; dropping `boxed` afterward frees the `ZStream`.
        code_of(engine::deflate_end(&mut boxed.zs))
    })
}

/// `int deflateReset(z_streamp strm)`
///
/// Resets the stream to the just-initialized state, preserving the compression
/// parameters. Observable counters, `adler`, `data_type`, and `msg` are
/// re-seeded on the raw stream.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`
/// (its `state` handle unmodified by the caller).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateReset(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        let (code, adler, data_type, msg) = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            let code = code_of(engine::deflate_reset(zs));
            (code, zs.adler, zs.data_type, msg_ptr(zs))
        };
        s.total_in = 0;
        s.total_out = 0;
        set_adler(s, adler);
        set_data_type(s, data_type);
        set_msg(s, msg);
        code
    })
}

/// `int deflateResetKeep(z_streamp strm)` (`ZLIB_1.2.5.2`)
///
/// Like [`deflateReset`] but keeps allocations and does not re-emit the wrapper
/// header. Mirrors the same observable fields C's `deflateResetKeep` touches.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`
/// (its `state` handle unmodified by the caller).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateResetKeep(strm: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        let (code, adler, data_type, msg) = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            let code = code_of(engine::deflate_reset_keep(zs));
            (code, zs.adler, zs.data_type, msg_ptr(zs))
        };
        s.total_in = 0;
        s.total_out = 0;
        set_adler(s, adler);
        set_data_type(s, data_type);
        set_msg(s, msg);
        code
    })
}

// ===========================================================================
// Phase 4 — Parameter / tuning shims
// ===========================================================================

/// `int deflateParams(z_streamp strm, int level, int strategy)`
///
/// Dynamically changes the compression level and/or strategy. This may need to
/// flush pending output, so the input/output buffers are bridged around the
/// engine call; a partial flush surfaces as `Z_BUF_ERROR` just as in C.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
///
/// The caller's buffers are read and written **only when the change requires a
/// pre-flush** — that is, only when it switches the block producer or the
/// strategy *and* the stream has already started (C's
/// `s->last_flush != -2`). The buffer obligations therefore apply only to that
/// case: `next_out` must be non-null and address `avail_out` writable bytes,
/// `next_in` must address `avail_in` readable bytes whenever `avail_in` is
/// non-zero, and the two regions must be disjoint. Both are validated on entry
/// and `Z_STREAM_ERROR` is returned if either is inconsistent.
///
/// Every other call — including the common "raise the level immediately after
/// `deflateInit2`, before any `deflate`" case — is pure bookkeeping. This shim
/// then neither forms a slice over, nor computes an offset from, `next_in` or
/// `next_out`, so those fields may hold anything at all (a stale pointer, a
/// region far smaller than `avail_*` claims, or null) exactly as they may in C,
/// which likewise returns `Z_OK` there without touching a byte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateParams(strm: z_streamp, level: c_int, strategy: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };

        // An out-of-range strategy is rejected exactly as C rejects it.
        let Some(strategy) = Strategy::from_c_int(strategy) else {
            return Z_STREAM_ERROR;
        };

        // `deflateParams` may flush pending output through an internal
        // `deflate(strm, Z_BLOCK)` (see `engine::deflate_params`), and that is
        // precisely — and ONLY — where C validates the raw buffers. Because the
        // engine call receives already-bridged slices, with a null-and-nonempty
        // buffer masked to empty by `input_slice`/`output_slice`, that masked
        // case would otherwise degrade to `Z_BUF_ERROR` instead of C's
        // `Z_STREAM_ERROR`. So surface the same entry check eagerly — but ONLY
        // when the pre-flush will actually run.
        //
        // C reaches the internal `deflate` call only when the change switches
        // the block producer (or the strategy) AND the stream has already
        // started (`s->last_flush != -2`). Otherwise `deflateParams` is pure
        // bookkeeping: reference zlib returns `Z_OK` without touching a single
        // byte, and therefore without caring whether `next_out` is null — which
        // is exactly what a caller does when it raises the level between
        // `deflateInit2` and the first `deflate` call, with the buffers not yet
        // wired up. Validating unconditionally rejected those calls with
        // `Z_STREAM_ERROR` and silently dropped the parameter change, breaking
        // both ABI parity and byte identity (a stream compressed after a
        // dropped level change diverges from reference zlib's output).
        //
        // `engine::deflate_params_flushes` is the single source of truth for
        // that condition — `engine::deflate_params` itself calls it — so the
        // check here cannot drift from the behavior it is guarding. It also
        // reports `false` when no deflate handle is installed or the level is
        // out of range, both of which the engine already answers with
        // `Z_STREAM_ERROR` on its own (mirroring C's `deflateStateCheck` and
        // range test, which likewise run before the buffers matter).
        let flushes = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            engine::deflate_params_flushes(zs, level, strategy)
        };
        if flushes && !stream_buffers_valid(s) {
            return Z_STREAM_ERROR;
        }

        // Bridge the caller's buffers ONLY when the pre-flush will actually run.
        //
        // On the no-pre-flush path C never dereferences `next_in`/`next_out`, so
        // it never requires either to address `avail_in`/`avail_out` live bytes —
        // and a caller raising the level between `deflateInit2` and its first
        // `deflate` may legitimately still have a stale, undersized, or not-yet-
        // wired region recorded there. Constructing a Rust slice is itself the
        // load-bearing assertion: `&[u8]`/`&mut [u8]` require the *whole*
        // declared region to be one live allocation from the moment the
        // reference exists, even when not a single byte is read or written. So
        // building them unconditionally turned a call reference zlib answers
        // with `Z_OK` into undefined behavior. Empty slices assert nothing about
        // the caller's pointers, and `engine::deflate_params` provably never
        // touches either argument off the pre-flush path — the lone forward to
        // `deflate(strm, input, output, Z_BLOCK)` sits inside its
        // `if deflate_params_flushes(..)` branch (`src/deflate/mod.rs`).
        let (input, output): (&[u8], &mut [u8]) = if flushes {
            // SAFETY: `stream_buffers_valid` above confirmed `next_out` is
            // non-null and that `next_in` is non-null whenever `avail_in > 0`;
            // the caller's contract makes the input region readable for
            // `avail_in` bytes for the duration of the call.
            let input = unsafe { input_slice(s) };
            // SAFETY: as above for the output region — writable for `avail_out`
            // bytes and, per the same contract, disjoint from `input`.
            let output = unsafe { output_slice(s) };
            (input, output)
        } else {
            // Statically promoted empty slices: no caller memory is named, so
            // nothing whatsoever is asserted about `next_in`/`next_out`.
            (&[], &mut [])
        };

        let (code, consumed, produced, adler, data_type, msg) = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            let outcome = engine::deflate_params(zs, input, output, level, strategy);
            (
                outcome.code.as_c_int(),
                outcome.consumed,
                outcome.produced,
                zs.adler,
                zs.data_type,
                msg_ptr(zs),
            )
        };

        if flushes {
            // SAFETY: `consumed <= avail_in` (engine invariant) and the
            // pre-flush path validated the pointers, so the advanced cursor
            // stays inside the caller's input buffer.
            unsafe { advance_input(s, consumed) };
            // SAFETY: `produced <= avail_out` (engine invariant), as above for
            // the output buffer.
            unsafe { advance_output(s, produced) };
        } else {
            // Nothing was bridged, so nothing can have moved. Leaving the
            // cursors alone is what C does: on this path `deflateParams` is pure
            // bookkeeping and writes no `z_stream` buffer field at all, so the
            // shim must not compute an offset from a pointer the caller never
            // promised was live.
            debug_assert_eq!(
                consumed, 0,
                "deflateParams consumed input without a pre-flush"
            );
            debug_assert_eq!(
                produced, 0,
                "deflateParams produced output without a pre-flush"
            );
        }
        set_adler(s, adler);
        set_data_type(s, data_type);
        set_msg(s, msg);
        code
    })
}

/// `int deflateTune(z_streamp strm, int good_length, int max_lazy,`
/// `int nice_length, int max_chain)` (`ZLIB_1.2.2.3`)
///
/// Fine-tunes the internal match-finding thresholds for advanced use.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateTune(
    strm: z_streamp,
    good_length: c_int,
    max_lazy: c_int,
    nice_length: c_int,
    max_chain: c_int,
) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        // SAFETY: installed engine state (see `deflate`).
        let Some(zs) = (unsafe { deflate_state(s) }) else {
            return Z_STREAM_ERROR;
        };
        code_of(engine::deflate_tune(
            zs,
            good_length,
            max_lazy,
            nice_length,
            max_chain,
        ))
    })
}

/// `uLong deflateBound(z_streamp strm, uLong sourceLen)`
///
/// Returns an upper bound on the compressed size of `sourceLen` input bytes.
/// Must never panic, even for a `NULL` stream or a stream without installed
/// state; in those cases the conservative worst-case bound (matching C's
/// uninitialized-stream path) is returned.
///
/// # Safety
///
/// `strm` must be null or a valid, properly aligned pointer to a `z_stream`
/// (initialized or freshly zeroed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateBound(strm: z_streamp, source_len: uLong) -> uLong {
    // Conservative bound computed from a throwaway stateless stream; used both
    // as the panic-guard default and when no engine state is installed.
    let stateless = engine::deflate_bound(&ZStream::new(), source_len as usize) as uLong;
    guard_ulong(stateless, || {
        if !strm.is_null() {
            // SAFETY: `strm` is non-null and valid for this call.
            let s = unsafe { &mut *strm };
            // SAFETY: `state`, when non-null, is a `Box<DeflateHandle>`;
            // `deflate_state` validates the `HandleKind::DEFLATE` tag before
            // reborrowing `handle.zs`.
            if let Some(zs) = unsafe { deflate_state(s) } {
                return engine::deflate_bound(zs, source_len as usize) as uLong;
            }
        }
        stateless
    })
}

/// `z_size_t deflateBound_z(z_streamp strm, z_size_t sourceLen)`
/// (`ZLIB_1.3.2`)
///
/// `size_t`-typed variant of [`deflateBound`]. The underlying computation is
/// pure saturating arithmetic and cannot panic, so no `catch_unwind` guard is
/// required.
///
/// # Safety
///
/// `strm` must be null or a valid, properly aligned pointer to a `z_stream`
/// (initialized or freshly zeroed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateBound_z(strm: z_streamp, source_len: z_size_t) -> z_size_t {
    if !strm.is_null() {
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        // SAFETY: `state`, when non-null, is a `Box<DeflateHandle>`;
        // `deflate_state` validates the `HandleKind::DEFLATE` tag before
        // reborrowing `handle.zs`.
        if let Some(zs) = unsafe { deflate_state(s) } {
            return engine::deflate_bound_z(zs, source_len);
        }
    }
    engine::deflate_bound_z(&ZStream::new(), source_len)
}

/// `int deflatePending(z_streamp strm, unsigned *pending, int *bits)`
/// (`ZLIB_1.2.5.1`)
///
/// Reports the number of bytes and bits of pending output not yet emitted.
/// Either output pointer may be `NULL`. Mirrors C's ordering (writes `*bits`
/// first) and its truncation guard: if the pending byte count does not fit in a
/// C `unsigned`, `*pending` is set to `UINT_MAX` and `Z_BUF_ERROR` is returned.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
/// `pending` and `bits` must each be null or a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflatePending(
    strm: z_streamp,
    pending: *mut c_uint,
    bits: *mut c_int,
) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        let (pending_val, bits_val) = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            match engine::deflate_pending(zs) {
                Ok(pair) => pair,
                Err(err) => return err.as_return_code().as_c_int(),
            }
        };

        // C writes `*bits` before `*pending`.
        if !bits.is_null() {
            // SAFETY: caller passed a valid, writable `*mut c_int`.
            unsafe { *bits = bits_val };
        }
        if !pending.is_null() {
            let truncated = pending_val as c_uint;
            if truncated as usize != pending_val {
                // SAFETY: caller passed a valid, writable `*mut c_uint`.
                unsafe { *pending = c_uint::MAX };
                return Z_BUF_ERROR;
            }
            // SAFETY: caller passed a valid, writable `*mut c_uint`.
            unsafe { *pending = truncated };
        }
        Z_OK
    })
}

/// `int deflateUsed(z_streamp strm, int *bits)` (`ZLIB_1.3.1.2`)
///
/// Reports the number of bits (0-8) of the last output byte that are still in
/// use. The output pointer may be `NULL`.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
/// `bits` must be null or a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateUsed(strm: z_streamp, bits: *mut c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        let bits_val = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            match engine::deflate_used(zs) {
                Ok(v) => v,
                Err(err) => return err.as_return_code().as_c_int(),
            }
        };
        if !bits.is_null() {
            // SAFETY: caller passed a valid, writable `*mut c_int`.
            unsafe { *bits = bits_val };
        }
        Z_OK
    })
}

/// `int deflatePrime(z_streamp strm, int bits, int value)` (`ZLIB_1.2.0.8`)
///
/// Inserts `bits` bits of `value` into the output bit stream ahead of the next
/// compressed data — used to align or splice streams.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflatePrime(strm: z_streamp, bits: c_int, value: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        // SAFETY: installed engine state (see `deflate`).
        let Some(zs) = (unsafe { deflate_state(s) }) else {
            return Z_STREAM_ERROR;
        };
        code_of(engine::deflate_prime(zs, bits, value))
    })
}

// ===========================================================================
// Phase 5 — Dictionary & header shims
// ===========================================================================

/// `int deflateSetDictionary(z_streamp strm, const Bytef *dictionary,`
/// `uInt dictLength)`
///
/// Initializes the compression dictionary from the given byte sequence. The
/// dictionary is folded into the running Adler-32, which is written back to the
/// raw stream.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
/// `dictionary` must be null or address at least `dict_length` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateSetDictionary(
    strm: z_streamp,
    dictionary: *const Bytef,
    dict_length: uInt,
) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };

        // A null dictionary is a usage error: reference C's
        // `deflateSetDictionary` returns `Z_STREAM_ERROR` for a `Z_NULL`
        // dictionary regardless of length, rather than silently
        // treating it as empty — which would mask a caller bug, since the
        // intended dictionary would never actually be set. Match C exactly.
        if dictionary.is_null() {
            return Z_STREAM_ERROR;
        }
        // A non-null dictionary of length 0 is a valid empty dictionary; view
        // `dict_length` bytes at `dictionary` otherwise.
        let dict: &[u8] = if dict_length == 0 {
            &[]
        } else {
            // SAFETY: the caller guarantees `dict_length` readable bytes at
            // `dictionary` (`Bytef` is `u8`).
            unsafe { slice::from_raw_parts(dictionary, dict_length as usize) }
        };

        let (code, adler) = {
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            let code = code_of(engine::deflate_set_dictionary(zs, dict));
            (code, zs.adler)
        };
        set_adler(s, adler);
        code
    })
}

/// `int deflateGetDictionary(z_streamp strm, Bytef *dictionary,`
/// `uInt *dictLength)` (`ZLIB_1.2.9`)
///
/// Retrieves the sliding-window contents currently used as the dictionary.
/// Both output pointers may be `NULL`: passing a null `dictionary` performs a
/// length-only query. Implemented with the engine's two-call pattern (query the
/// length, then copy into a caller buffer of that length).
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
/// If `dictionary` is non-null it must address at least the returned dictionary
/// length in writable bytes (never more than 32 KiB). `dict_length` must be
/// null or a valid, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateGetDictionary(
    strm: z_streamp,
    dictionary: *mut Bytef,
    dict_length: *mut uInt,
) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and valid for this call.
        let s = unsafe { &mut *strm };
        // SAFETY: installed engine state (see `deflate`).
        let Some(zs) = (unsafe { deflate_state(s) }) else {
            return Z_STREAM_ERROR;
        };

        // First, query the available dictionary length.
        let len = match engine::deflate_get_dictionary(zs, None) {
            Ok(l) => l,
            Err(err) => return err.as_return_code().as_c_int(),
        };

        // Then copy the bytes out if the caller supplied a destination buffer.
        if !dictionary.is_null() && len != 0 {
            // SAFETY: the zlib contract requires `dictionary` to address at
            // least `len` writable bytes (the window is at most 32 KiB).
            let dst = unsafe { slice::from_raw_parts_mut(dictionary, len) };
            if let Err(err) = engine::deflate_get_dictionary(zs, Some(dst)) {
                return err.as_return_code().as_c_int();
            }
        }
        if !dict_length.is_null() {
            // SAFETY: caller passed a valid, writable `*mut uInt`.
            unsafe { *dict_length = len as uInt };
        }
        Z_OK
    })
}

/// `int deflateSetHeader(z_streamp strm, gz_headerp head)` (`ZLIB_1.2.2`)
///
/// Supplies the gzip header for a gzip-wrapped stream. Returns `Z_STREAM_ERROR`
/// unless the stream is in gzip mode (`wrap == 2`), mirroring C. C stores the
/// caller's pointer; this shim deep-copies the header fields into an owned
/// [`GzHeader`](crate::gz_header::GzHeader), so no dangling pointer can be read during later `deflate`
/// calls. A `NULL` header clears any previously set header (restoring the
/// default), exactly as passing `Z_NULL` does in C.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
/// `head` must be null or point to a valid `gz_header` whose field-declared
/// buffers remain valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateSetHeader(strm: z_streamp, head: gz_headerp) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }

        #[cfg(feature = "gzip")]
        {
            // SAFETY: `strm` is non-null and valid for this call.
            let s = unsafe { &mut *strm };
            // Validate installed state before touching `head` (C order:
            // `deflateStateCheck` precedes the `wrap` test).
            // SAFETY: installed engine state (see `deflate`).
            let Some(zs) = (unsafe { deflate_state(s) }) else {
                return Z_STREAM_ERROR;
            };
            // SAFETY: `head` is null or points at a valid `gz_header` whose
            // field-declared buffers remain valid for this call.
            let header = unsafe { gz_header_to_idiomatic(head) };
            code_of(engine::deflate_set_header(zs, header))
        }
        #[cfg(not(feature = "gzip"))]
        {
            // Without gzip framing no stream can ever be `wrap == 2`, so C's
            // contract (`wrap != 2 -> Z_STREAM_ERROR`) is unconditional here.
            let _ = head;
            Z_STREAM_ERROR
        }
    })
}

// ===========================================================================
// Phase 6 — Copy
// ===========================================================================

/// `int deflateCopy(z_streamp dest, z_streamp source)`
///
/// Duplicates a compression stream, including its sliding window, pending
/// buffer, and hash tables (the engine's allocator-preserving
/// `DeflateState::try_clone` performs the deep copy, re-allocating every working
/// buffer through the **same** `zalloc` as the source — AAP §0.6.5). All
/// observable `z_stream` fields are mirrored from `source` into `dest`, matching
/// C's full-struct copy. Returns `Z_MEM_ERROR` if a copy allocation fails, in
/// which case `dest` is left entirely untouched.
///
/// # Safety
///
/// `dest` and `source` must each be null or a valid, exclusively-owned
/// `z_stream`, and must not alias one another. `source` must have been
/// initialized by `deflateInit*`. On success `dest` receives its own owned
/// state handle that must be released with [`deflateEnd`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflateCopy(dest: z_streamp, source: z_streamp) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if dest.is_null() || source.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: both pointers are non-null and, per the zlib contract, refer
        // to distinct (non-aliasing) valid streams for this call.
        let d = unsafe { &mut *dest };
        // SAFETY: see above; `source` is distinct from `dest`.
        let src = unsafe { &mut *source };

        // Deep-copy the source engine into a fresh stream carrying the same
        // allocator, then install it into `dest`.
        let new_zs = {
            // SAFETY: `source.state`, when non-null, is a
            // `Box<DeflateHandle>`; `deflate_state` confirms the
            // `HandleKind::DEFLATE` tag before reborrowing `handle.zs`.
            let Some(src_zs) = (unsafe { deflate_state(src) }) else {
                return Z_STREAM_ERROR;
            };
            let mut new_zs = ZStream::with_allocator(*src_zs.allocator());
            if let Err(err) = engine::deflate_copy(&mut new_zs, src_zs) {
                return err.as_return_code().as_c_int();
            }
            new_zs
        };

        // Box the cloned handle **fallibly** before any field of `dest` is
        // written, so global-heap exhaustion is reported as `Z_MEM_ERROR` with
        // `dest` untouched — the state C leaves behind when it calls
        // `deflateEnd(dest)` and returns `Z_MEM_ERROR` (AAP §0.6.5). The dropped
        // `new_zs` releases the freshly cloned buffers through the caller's
        // `zfree`.
        let Some(handle) = try_box(DeflateHandle::new(new_zs)) else {
            return Z_MEM_ERROR;
        };

        // Install the cloned state into `dest`.
        // SAFETY: transfers ownership of the tagged `Box<DeflateHandle>` into
        // `dest.state`; reclaimed exactly once by `deflateEnd`, whose
        // `deflate_take` validates the `HandleKind::DEFLATE` tag and nulls
        // `dest.state` on success.
        d.state = unsafe { state_ptr_from_box(handle) };

        // Mirror the full observable `z_stream` (C copies the whole struct).
        d.next_in = src.next_in;
        d.avail_in = src.avail_in;
        d.total_in = src.total_in;
        d.next_out = src.next_out;
        d.avail_out = src.avail_out;
        d.total_out = src.total_out;
        d.msg = src.msg;
        d.data_type = src.data_type;
        d.adler = src.adler;
        d.zalloc = src.zalloc;
        d.zfree = src.zfree;
        d.opaque = src.opaque;
        d.reserved = src.reserved;
        Z_OK
    })
}

// ===========================================================================
// Phase 8 — Tests
//
// Exercised with the default feature set (`std` + `gzip` + `simd`), so the std
// prelude and the `flate2` dev-dependency (pure-Rust `miniz_oxide` backend) are
// available for byte-identity interop checks.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{Z_DEFAULT_COMPRESSION, Z_FINISH, Z_NO_FLUSH};
    use crate::ffi::alloc::test_hook::BuiltinHookStats;
    use core::ffi::c_void;
    use core::mem::size_of;
    use std::io::Read;
    use std::vec::Vec;

    /// `Z_STREAM_END` — only referenced by the tests.
    const Z_STREAM_END: c_int = ReturnCode::StreamEnd.as_c_int();

    /// A valid version pointer (only the leading byte is checked by zlib).
    fn ver() -> *const c_char {
        c"1.3.2.1-motley".as_ptr()
    }

    /// A `memset(0)` `z_stream`, matching how a C caller presents a fresh stream.
    fn zeroed_stream() -> z_stream {
        // SAFETY: `z_stream` is a `#[repr(C)]` aggregate of raw pointers, nullable
        // `Option<fn>` callbacks, and integers; the all-zero bit pattern is the
        // valid "uninitialized" state (null pointers, `None` callbacks, zero
        // counters) that C code produces with `memset`.
        unsafe { core::mem::zeroed() }
    }

    /// zlib-decompress `data` (RFC 1950 wrapper) and return the bytes.
    fn zlib_inflate(data: &[u8]) -> Vec<u8> {
        let mut dec = flate2::read::ZlibDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).expect("zlib inflate failed");
        out
    }

    #[test]
    fn init_compress_end_roundtrip() {
        let mut strm = zeroed_stream();
        let rc = unsafe {
            deflateInit_(
                &mut strm,
                Z_DEFAULT_COMPRESSION,
                ver(),
                size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(rc, Z_OK);

        let input = b"Hello, hello, hello! zlib-rs FFI deflate round-trip. ".repeat(8);
        let mut output = std::vec![0u8; 1024];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = output.len() as c_uint;

        loop {
            let rc = unsafe { deflate(&mut strm, Z_FINISH) };
            if rc == Z_STREAM_END {
                break;
            }
            assert_eq!(rc, Z_OK, "unexpected deflate return code");
            assert!(strm.avail_out > 0, "ran out of output space unexpectedly");
        }

        let produced = strm.total_out as usize;
        assert!(produced > 0, "expected some compressed output");
        assert_eq!(strm.total_in as usize, input.len(), "all input consumed");

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert!(
            strm.state.is_null(),
            "state must be reclaimed by deflateEnd"
        );

        assert_eq!(zlib_inflate(&output[..produced]), input);
    }

    #[test]
    fn streaming_small_output_buffer() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 9, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let input = b"streaming with a tiny output buffer forces many deflate calls. ".repeat(20);
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;

        let mut compressed = Vec::new();
        let mut chunk = [0u8; 16];
        loop {
            strm.next_out = chunk.as_mut_ptr();
            strm.avail_out = chunk.len() as c_uint;
            let rc = unsafe { deflate(&mut strm, Z_FINISH) };
            let produced = chunk.len() - strm.avail_out as usize;
            compressed.extend_from_slice(&chunk[..produced]);
            if rc == Z_STREAM_END {
                break;
            }
            assert_eq!(rc, Z_OK, "unexpected return code during streaming");
        }
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        assert_eq!(zlib_inflate(&compressed), input);
    }

    #[test]
    fn version_mismatch_rejected() {
        let mut strm = zeroed_stream();
        // Wrong leading version byte.
        assert_eq!(
            unsafe {
                deflateInit_(
                    &mut strm,
                    6,
                    c"9.9".as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_VERSION_ERROR
        );
        // Null version pointer.
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ptr::null(), size_of::<z_stream>() as c_int) },
            Z_VERSION_ERROR
        );
        // Wrong stream size.
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int - 1) },
            Z_VERSION_ERROR
        );
        assert!(strm.state.is_null(), "no state after a rejected init");
    }

    #[test]
    fn null_and_stateless_streams_are_stream_error() {
        assert_eq!(
            unsafe { deflate(ptr::null_mut(), Z_NO_FLUSH) },
            Z_STREAM_ERROR
        );
        assert_eq!(unsafe { deflateEnd(ptr::null_mut()) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { deflateReset(ptr::null_mut()) }, Z_STREAM_ERROR);

        // Non-null stream but no installed state.
        let mut strm = zeroed_stream();
        assert_eq!(unsafe { deflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_STREAM_ERROR);
    }

    #[test]
    fn deflate_rejects_invalid_raw_buffers() {
        // C `deflate` entry validation (`deflate.c` L981-L1010): a null
        // `next_out`, or a positive `avail_in` paired with a null `next_in`, is
        // `Z_STREAM_ERROR`. An *initialized* stream is used so the rejection is
        // attributable to buffer validation rather than the state check.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let input = b"payload for null-buffer validation";
        let mut output = std::vec![0u8; 128];

        // Null `next_out` with nonzero `avail_out` -> Z_STREAM_ERROR (C rejects a
        // null output pointer unconditionally).
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = ptr::null_mut();
        strm.avail_out = output.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_ERROR);

        // Nonzero `avail_in` with null `next_in` -> Z_STREAM_ERROR.
        strm.next_in = ptr::null();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = output.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_ERROR);

        // A fully valid buffer pair is accepted (proves the guard is not
        // over-broad): the guard lets a normal call through.
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = output.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_FINISH) }, Z_STREAM_END);

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
    }

    /// `deflateParams` validates the raw buffers **only when it actually
    /// flushes** — exactly where reference C does.
    ///
    /// C reaches its internal `deflate(strm, Z_BLOCK)` (the only place it looks
    /// at `next_in`/`next_out`) solely when the requested change switches the
    /// block producer or the strategy AND the stream has already started
    /// (`s->last_flush != -2`). Every other `deflateParams` call is pure
    /// bookkeeping and returns `Z_OK` without reading or writing a byte — so a
    /// null `next_out` is irrelevant to it. That is precisely what a caller does
    /// when it raises the level between `deflateInit2` and the first `deflate`,
    /// with the buffers not yet wired up.
    ///
    /// Validating unconditionally rejected those calls with `Z_STREAM_ERROR` and
    /// silently dropped the parameter change, which is both an ABI-parity break
    /// and a **byte-identity** break: the stream then compresses at the old
    /// level and diverges from reference zlib's output. Every expectation below
    /// was measured against a reference C zlib built from this repository's own
    /// `*.c` sources.
    #[test]
    fn deflate_params_validates_raw_buffers_only_when_it_flushes() {
        let input = b"payload";

        // --- No pre-flush needed: Z_OK even with a null `next_out` -----------
        // Immediately after `deflateInit_` the stream has `last_flush == -2`, so
        // C skips the internal flush entirely. Reference C: rc = Z_OK.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = ptr::null_mut();
        strm.avail_out = 128;
        assert_eq!(
            unsafe { deflateParams(&mut strm, 9, Z_DEFAULT_STRATEGY) },
            Z_OK,
            "a parameter change that needs no flush must succeed, as in C — the \
             raw buffers are never touched"
        );

        // ...and the change must have actually been APPLIED. Finish this stream
        // and compare with one initialized at level 9 from the start: dropping
        // the change would compress at level 6 and emit different bytes.
        let corpus: Vec<u8> = (0..20_000u32)
            .map(|i| b'a' + (i % 3) as u8)
            .collect::<Vec<u8>>();
        let mut promoted = std::vec![0u8; 64 * 1024];
        strm.next_in = corpus.as_ptr();
        strm.avail_in = corpus.len() as c_uint;
        strm.next_out = promoted.as_mut_ptr();
        strm.avail_out = promoted.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        let promoted_len = strm.total_out as usize;
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        let mut native = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut native, 9, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let mut native_out = std::vec![0u8; 64 * 1024];
        native.next_in = corpus.as_ptr();
        native.avail_in = corpus.len() as c_uint;
        native.next_out = native_out.as_mut_ptr();
        native.avail_out = native_out.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut native, Z_FINISH) }, Z_STREAM_END);
        let native_len = native.total_out as usize;
        assert_eq!(unsafe { deflateEnd(&mut native) }, Z_OK);

        assert_eq!(
            &promoted[..promoted_len],
            &native_out[..native_len],
            "the accepted parameter change must take effect: promoting to level 9 \
             before the first `deflate` must emit exactly what level 9 emits"
        );
        assert_eq!(
            corpus,
            zlib_inflate(&promoted[..promoted_len]),
            "the promoted stream must still decode to the original bytes"
        );

        // --- Pre-flush required + invalid `next_out`: Z_STREAM_ERROR ---------
        // Level 1 uses `deflate_fast` and level 9 uses `deflate_slow`, so this is
        // a producer switch; one real `deflate` call first makes
        // `last_flush != -2`. C's internal `deflate(strm, Z_BLOCK)` then rejects
        // the null output pointer and `deflateParams` propagates it.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 1, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let mut output = std::vec![0u8; 64 * 1024];
        strm.next_in = corpus.as_ptr();
        strm.avail_in = 4096;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = output.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_NO_FLUSH) }, Z_OK);

        let saved_next_out = strm.next_out;
        let saved_avail_out = strm.avail_out;
        strm.next_out = ptr::null_mut();
        strm.avail_out = 128;
        assert_eq!(
            unsafe { deflateParams(&mut strm, 9, Z_DEFAULT_STRATEGY) },
            Z_STREAM_ERROR,
            "a parameter change that MUST flush still rejects an invalid output \
             buffer, exactly as C's internal `deflate` does"
        );

        // --- Pre-flush required + valid buffers: accepted --------------------
        // Same change, buffers restored: the guard must not be over-broad.
        strm.next_out = saved_next_out;
        strm.avail_out = saved_avail_out;
        assert_eq!(
            unsafe { deflateParams(&mut strm, 9, Z_DEFAULT_STRATEGY) },
            Z_OK,
            "with room available the flush completes and the change is applied"
        );
        // Finish the stream so the mid-flight change is proven end to end, then
        // close cleanly (`Z_OK`, because the state is no longer `Busy`).
        strm.next_in = unsafe { corpus.as_ptr().add(4096) };
        strm.avail_in = (corpus.len() - 4096) as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        let switched_len = strm.total_out as usize;
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            corpus,
            zlib_inflate(&output[..switched_len]),
            "a stream whose level changed mid-flight must still decode exactly"
        );

        // --- Pre-flush required but no output room: Z_BUF_ERROR --------------
        // The flush cannot complete, so C reports `Z_BUF_ERROR` (not
        // `Z_STREAM_ERROR`): the pointers are valid, there is simply no space.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        strm.next_in = corpus.as_ptr();
        strm.avail_in = 4096;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = 8;
        let _ = unsafe { deflate(&mut strm, Z_NO_FLUSH) };
        strm.avail_out = 0;
        assert_eq!(
            unsafe { deflateParams(&mut strm, 1, Z_DEFAULT_STRATEGY) },
            Z_BUF_ERROR,
            "an incomplete flush is Z_BUF_ERROR, never Z_STREAM_ERROR"
        );
        // Ending a stream that is still `Busy` is C's documented `Z_DATA_ERROR`
        // ("the stream was freed prematurely"), not `Z_OK`.
        assert_eq!(
            unsafe { deflateEnd(&mut strm) },
            ReturnCode::DataError.as_c_int()
        );

        // --- Out-of-range arguments still win over everything ---------------
        // C validates the level/strategy range BEFORE it considers a flush, so a
        // null `next_out` cannot change the answer.
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        strm.next_out = ptr::null_mut();
        strm.avail_out = 128;
        assert_eq!(
            unsafe { deflateParams(&mut strm, 10, Z_DEFAULT_STRATEGY) },
            Z_STREAM_ERROR
        );
        assert_eq!(
            unsafe { deflateParams(&mut strm, -2, Z_DEFAULT_STRATEGY) },
            Z_STREAM_ERROR
        );
        assert_eq!(unsafe { deflateParams(&mut strm, 6, 5) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        // A stream with no engine installed is `Z_STREAM_ERROR`, and so is null.
        let mut bare = zeroed_stream();
        assert_eq!(
            unsafe { deflateParams(&mut bare, 6, Z_DEFAULT_STRATEGY) },
            Z_STREAM_ERROR
        );
        assert_eq!(
            unsafe { deflateParams(ptr::null_mut(), 6, Z_DEFAULT_STRATEGY) },
            Z_STREAM_ERROR
        );
    }

    /// The no-pre-flush `deflateParams` path must never form a Rust slice over —
    /// or compute an offset from — a region the caller has not promised is live.
    ///
    /// This is the sharper companion to the null-pointer coverage above. A null
    /// `next_out` is the *easy* case: `input_slice`/`output_slice` mask a null
    /// pointer down to an empty slice, and under strict provenance a zero-offset
    /// `ptr::add` needs no allocation, so a null-only test passes even when the
    /// slices are built unconditionally. What no masking can rescue is a
    /// **non-null but undersized** region: `slice::from_raw_parts_mut(p, 128)`
    /// over a one-byte allocation is undefined behavior the instant the
    /// reference exists, whether or not a single byte is read or written.
    ///
    /// Reference C is entirely unbothered by this input. With `last_flush == -2`
    /// no pre-flush runs, so `deflateParams` never dereferences `next_in` or
    /// `next_out`; it returns `Z_OK`, applies the change, and leaves the caller's
    /// cursors and totals exactly as they were. A drop-in replacement must reach
    /// the same answer *without naming the caller's memory at all* — which is
    /// what the `flushes`-gated bridge in `deflateParams` guarantees.
    ///
    /// Under `cargo +nightly miri test` this is the executable proof of that
    /// guarantee; under an ordinary build it still pins the whole observable
    /// contract: `Z_OK`, the change applied, cursors and totals untouched, and
    /// the one honest byte of each buffer left alone.
    #[test]
    fn deflate_params_without_a_preflush_never_touches_an_undersized_buffer() {
        /// Sentinel written into the single honest output byte; a shim that
        /// wrote through the overstated region would disturb it.
        const SENTINEL: u8 = 0xA5;

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Deliberately dishonest descriptors: one live byte each, with
        // `avail_in`/`avail_out` claiming far more. Nothing here is UB by
        // itself — a `z_stream` is a plain `#[repr(C)]` struct of scalars — and
        // C tolerates it precisely because it never looks.
        let honest_in = *b"z";
        let mut honest_out = [SENTINEL];
        strm.next_in = honest_in.as_ptr();
        strm.avail_in = 64;
        strm.next_out = honest_out.as_mut_ptr();
        strm.avail_out = 128;

        assert_eq!(
            unsafe { deflateParams(&mut strm, 9, Z_DEFAULT_STRATEGY) },
            Z_OK,
            "a parameter change needing no pre-flush must succeed without \
             regard to the buffers, exactly as reference C does"
        );

        // Cursors, counts, and totals must be untouched: the engine cannot have
        // consumed or produced anything, so the shim must not have advanced
        // anything either.
        assert_eq!(strm.next_in, honest_in.as_ptr(), "next_in must not move");
        assert_eq!(strm.avail_in, 64, "avail_in must not change");
        assert_eq!(
            strm.next_out,
            honest_out.as_mut_ptr(),
            "next_out must not move"
        );
        assert_eq!(strm.avail_out, 128, "avail_out must not change");
        assert_eq!(strm.total_in, 0, "total_in must stay at zero");
        assert_eq!(strm.total_out, 0, "total_out must stay at zero");
        assert_eq!(
            honest_out[0], SENTINEL,
            "the one live output byte must be untouched"
        );

        // The same must hold for a strategy-only change, and for a request that
        // resolves to the level already in force (`Z_DEFAULT_COMPRESSION` == 6).
        // `1` is `Z_FILTERED`; the raw integer is what a C caller passes.
        assert_eq!(
            unsafe { deflateParams(&mut strm, 9, 1) },
            Z_OK,
            "a strategy change is likewise pure bookkeeping here"
        );
        assert_eq!(
            unsafe { deflateParams(&mut strm, Z_DEFAULT_COMPRESSION, Z_DEFAULT_STRATEGY) },
            Z_OK,
            "Z_DEFAULT_COMPRESSION resolves to 6 and still needs no flush"
        );
        assert_eq!(strm.total_out, 0, "still nothing produced");
        assert_eq!(
            honest_out[0], SENTINEL,
            "the one live output byte is still untouched"
        );

        // Now wire up honest buffers and prove the *last* accepted change really
        // took effect, so the fix cannot have been "ignore the call entirely":
        // the stream must emit exactly what a level-6 / default-strategy stream
        // emits from the start.
        let corpus: Vec<u8> = (0..20_000u32).map(|i| b'a' + (i % 5) as u8).collect();
        let mut produced = std::vec![0u8; 64 * 1024];
        strm.next_in = corpus.as_ptr();
        strm.avail_in = corpus.len() as c_uint;
        strm.next_out = produced.as_mut_ptr();
        strm.avail_out = produced.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        let produced_len = strm.total_out as usize;
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        let mut native = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut native, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let mut native_out = std::vec![0u8; 64 * 1024];
        native.next_in = corpus.as_ptr();
        native.avail_in = corpus.len() as c_uint;
        native.next_out = native_out.as_mut_ptr();
        native.avail_out = native_out.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut native, Z_FINISH) }, Z_STREAM_END);
        let native_len = native.total_out as usize;
        assert_eq!(unsafe { deflateEnd(&mut native) }, Z_OK);

        assert_eq!(
            &produced[..produced_len],
            &native_out[..native_len],
            "the last accepted parameter change must be the one in force"
        );
        assert_eq!(
            corpus,
            zlib_inflate(&produced[..produced_len]),
            "the stream must still decode to the original bytes"
        );
    }

    #[test]
    fn bound_without_state_is_finite() {
        let b = unsafe { deflateBound(ptr::null_mut(), 1024) };
        assert!(b >= 1024, "bound must cover the source length");
        let bz = unsafe { deflateBound_z(ptr::null_mut(), 1024) };
        assert!(bz >= 1024);

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let b2 = unsafe { deflateBound(&mut strm, 1024) };
        assert!(b2 >= 1024);
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn pending_and_used_accept_null_pointers() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Both output pointers null: still succeeds.
        assert_eq!(
            unsafe { deflatePending(&mut strm, ptr::null_mut(), ptr::null_mut()) },
            Z_OK
        );
        let mut pending: c_uint = 0xDEAD;
        let mut bits: c_int = 99;
        assert_eq!(
            unsafe { deflatePending(&mut strm, &mut pending, &mut bits) },
            Z_OK
        );
        assert!((0..8).contains(&bits), "pending bit count out of range");

        assert_eq!(unsafe { deflateUsed(&mut strm, ptr::null_mut()) }, Z_OK);
        let mut used: c_int = -1;
        assert_eq!(unsafe { deflateUsed(&mut strm, &mut used) }, Z_OK);
        assert!((0..=8).contains(&used), "used bit count out of range");

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn reset_clears_counters() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let input = b"reset test payload ".repeat(4);
        let mut out = std::vec![0u8; 512];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        loop {
            let rc = unsafe { deflate(&mut strm, Z_FINISH) };
            if rc == Z_STREAM_END {
                break;
            }
            assert_eq!(rc, Z_OK);
        }
        assert!(strm.total_out > 0);

        assert_eq!(unsafe { deflateReset(&mut strm) }, Z_OK);
        assert_eq!(strm.total_in, 0);
        assert_eq!(strm.total_out, 0);
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn set_dictionary_updates_adler() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let dict = b"the quick brown fox jumps over the lazy dog";
        let rc = unsafe { deflateSetDictionary(&mut strm, dict.as_ptr(), dict.len() as c_uint) };
        assert_eq!(rc, Z_OK);
        // For a zlib-wrapped stream the dictionary is folded into the Adler-32,
        // which the shim writes back onto the raw stream.
        assert_ne!(strm.adler, 1, "dictionary should change the Adler-32 seed");
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
    }

    #[test]
    fn copy_produces_independent_finishable_stream() {
        let mut src = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Copy a fresh stream, then finish the copy on its own buffers.
        let mut dst = zeroed_stream();
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());
        assert!(!src.state.is_null());

        let input = b"independent copy finishes correctly ".repeat(4);
        let mut out = std::vec![0u8; 1024];
        dst.next_in = input.as_ptr();
        dst.avail_in = input.len() as c_uint;
        dst.next_out = out.as_mut_ptr();
        dst.avail_out = out.len() as c_uint;
        loop {
            let rc = unsafe { deflate(&mut dst, Z_FINISH) };
            if rc == Z_STREAM_END {
                break;
            }
            assert_eq!(rc, Z_OK);
        }
        let n = dst.total_out as usize;
        assert!(n > 0);
        assert_eq!(zlib_inflate(&out[..n]), input);

        // Both streams free independently (no shared state / double free).
        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
    }

    /// A budget carried through the caller's `opaque` cookie, so each test drives
    /// its own allocator without sharing global state — exactly what zlib's
    /// `opaque` field exists for.
    struct Budget {
        /// Allocations still permitted before `zalloc` starts reporting OOM.
        remaining: core::sync::atomic::AtomicUsize,
    }

    /// Bytes reserved ahead of each payload so `zfree` can recover the size.
    const BUDGET_HDR: usize = 16;

    /// An honest allocator that really allocates and frees, but only until the
    /// budget in `opaque` is exhausted — after which it reports out-of-memory the
    /// way a C `zalloc` does under pressure.
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
        let layout = std::alloc::Layout::from_size_align(BUDGET_HDR + bytes, BUDGET_HDR)
            .expect("test layout is valid");
        // SAFETY: `layout` has a non-zero size (`BUDGET_HDR` is 16).
        let base = unsafe { std::alloc::alloc(layout) };
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
            let layout = std::alloc::Layout::from_size_align(BUDGET_HDR + bytes, BUDGET_HDR)
                .expect("test layout is valid");
            std::alloc::dealloc(base, layout);
        }
    }

    /// Installs the budgeted allocator on `strm`, pointing `opaque` at `budget`.
    fn attach_budget(strm: &mut z_stream, budget: &Budget) {
        strm.zalloc = Some(budget_zalloc);
        strm.zfree = Some(budget_zfree);
        strm.opaque = (budget as *const Budget).cast_mut().cast::<c_void>();
    }

    // =======================================================================
    // `strm->msg` lifecycle parity
    //
    // C writes `strm->msg` in `deflate.c` at only three places: L400 (cleared by
    // `deflateInit2_`), L511 (`ERR_MSG(Z_MEM_ERROR)` on a working-buffer
    // failure) and L652 (cleared by `deflateResetKeep`). `deflateEnd`
    // (L1293-L1310) never touches it — which is precisely what makes L511's
    // message readable, since L512 calls `deflateEnd` before returning.
    // =======================================================================

    /// C's L505-L514 branch stores `ERR_MSG(Z_MEM_ERROR)` before returning, and
    /// its own internal `deflateEnd` call leaves the field alone, so the caller
    /// can read "insufficient memory" after `deflateInit2_` fails.
    #[test]
    fn init_publishes_insufficient_memory_when_a_working_buffer_is_refused() {
        // C issues five requests: the state object, then window/prev/head/
        // pending_buf. Permitting 1..=4 lets the state succeed and starves a
        // working buffer, which is C's message-recording path.
        for permitted in 1..=4 {
            let budget = Budget {
                remaining: core::sync::atomic::AtomicUsize::new(permitted),
            };
            let mut strm = zeroed_stream();
            attach_budget(&mut strm, &budget);
            let rc = unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) };
            assert_eq!(rc, Z_MEM_ERROR, "permitted={permitted}");
            assert!(
                strm.state.is_null(),
                "permitted={permitted}: no state may be installed"
            );
            assert!(
                !strm.msg.is_null(),
                "permitted={permitted}: C sets strm->msg at deflate.c L511"
            );
            // SAFETY: `msg` points at one of the `'static` `z_errmsg` strings.
            let msg = unsafe { core::ffi::CStr::from_ptr(strm.msg) };
            assert_eq!(
                msg.to_bytes(),
                b"insufficient memory",
                "permitted={permitted}: must be ERR_MSG(Z_MEM_ERROR)"
            );
        }
    }

    /// The *other* failure point — the state object at `deflate.c` L440-L442 —
    /// returns immediately and leaves `strm->msg` NULL. The distinction is
    /// observable, so it must be reproduced rather than collapsed.
    #[test]
    fn init_leaves_msg_null_when_the_state_reservation_is_refused() {
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(0),
        };
        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_MEM_ERROR,
        );
        assert!(
            strm.msg.is_null(),
            "C returns at L441 before reaching the ERR_MSG at L511, so msg stays NULL"
        );
    }

    /// `deflateEnd` must not clear `strm->msg`: C's `deflateEnd`
    /// (`deflate.c` L1293-L1310) frees the buffers and the state, nulls
    /// `strm->state`, and returns — nothing else. A caller may therefore report
    /// the diagnostic *after* tearing the stream down.
    #[test]
    fn deflate_end_preserves_the_error_message() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK,
        );
        // Stand in for a diagnostic the caller is holding across teardown, exactly
        // as C's own `deflateInit2_` does at L511 before calling `deflateEnd`.
        let marker = c"insufficient memory";
        strm.msg = marker.as_ptr().cast_mut();
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            strm.msg,
            marker.as_ptr().cast_mut(),
            "deflateEnd must leave strm->msg exactly as it found it",
        );
    }

    /// Every handle installation must be fallible **before** the caller's
    /// `z_stream` is mutated. When the caller's allocator refuses the very first
    /// request — the state reservation C makes and checks immediately at
    /// `deflate.c` L440-L442 — `deflateInit2_` must report `Z_MEM_ERROR` and
    /// leave `strm->state` null, so the caller's struct is exactly as it was
    /// found and a retry or a `deflateEnd` behaves as on an uninitialized stream.
    #[test]
    fn init_reports_mem_error_and_installs_no_state_when_allocator_refuses() {
        // Zero allocations permitted: the state reservation itself is refused.
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(0),
        };

        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_MEM_ERROR,
            "a refused allocation must surface Z_MEM_ERROR, never an abort or a \
             silent global-heap substitution"
        );
        assert!(
            strm.state.is_null(),
            "a failed init must not install a state on the caller's z_stream"
        );
        // Nothing was installed, so the stream is still uninitialized and
        // `deflateEnd` reports that exactly as C's `deflateStateCheck` does.
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_STREAM_ERROR);
    }

    /// Regression guard: when the caller's `zalloc` cannot satisfy the copy,
    /// `deflateCopy` must report `Z_MEM_ERROR` — matching C `deflate.c`
    /// L1348-L1350 — and must **not** report success with a destination whose
    /// buffers came from the Rust global allocator instead of the caller's arena
    /// (AAP §0.6.3, §0.6.5).
    #[test]
    fn copy_reports_mem_error_when_caller_allocator_is_exhausted() {
        // Enough allocations to initialize one stream, none to spare for a copy.
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(16),
        };

        let mut src = zeroed_stream();
        attach_budget(&mut src, &budget);
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Starve the allocator, then attempt the copy.
        budget
            .remaining
            .store(0, core::sync::atomic::Ordering::SeqCst);

        let mut dst = zeroed_stream();
        attach_budget(&mut dst, &budget);
        assert_eq!(
            unsafe { deflateCopy(&mut dst, &mut src) },
            ReturnCode::MemError.as_c_int(),
            "an exhausted caller allocator must fail the copy, not silently \
             substitute global-allocator storage"
        );
        assert!(
            dst.state.is_null(),
            "a failed copy must leave the destination without a state"
        );

        // The source is untouched and still finishes normally.
        assert!(!src.state.is_null());
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
    }

    /// The same allocator, with budget to spare, copies successfully — proving
    /// the test above fails for want of memory and not for any other reason.
    #[test]
    fn copy_succeeds_when_caller_allocator_has_budget() {
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };

        let mut src = zeroed_stream();
        attach_budget(&mut src, &budget);
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let mut dst = zeroed_stream();
        attach_budget(&mut dst, &budget);
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());

        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
    }

    #[test]
    fn set_header_on_zlib_stream_is_stream_error() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        // A zlib-wrapped stream is `wrap == 1`, not gzip, so setting a gzip
        // header must fail with `Z_STREAM_ERROR` (matches C for either feature
        // configuration).
        assert_eq!(
            unsafe { deflateSetHeader(&mut strm, ptr::null_mut()) },
            Z_STREAM_ERROR
        );
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_roundtrip_with_header() {
        let mut strm = zeroed_stream();
        // windowBits 31 selects the gzip wrapper with a 15-bit window.
        let rc = unsafe {
            deflateInit2_(
                &mut strm,
                6,
                Z_DEFLATED,
                31,
                DEF_MEM_LEVEL,
                Z_DEFAULT_STRATEGY,
                ver(),
                size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(rc, Z_OK);
        // Setting a (default) gzip header succeeds for a gzip stream.
        assert_eq!(
            unsafe { deflateSetHeader(&mut strm, ptr::null_mut()) },
            Z_OK
        );

        let input = b"gzip framed round trip via the FFI shims ".repeat(6);
        let mut out = std::vec![0u8; 1024];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        loop {
            let rc = unsafe { deflate(&mut strm, Z_FINISH) };
            if rc == Z_STREAM_END {
                break;
            }
            assert_eq!(rc, Z_OK);
        }
        let n = strm.total_out as usize;
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        // Decode with a gzip reader to confirm RFC 1952 framing.
        let mut dec = flate2::read::GzDecoder::new(&out[..n]);
        let mut restored = Vec::new();
        dec.read_to_end(&mut restored).expect("gzip inflate failed");
        assert_eq!(restored, input);
    }

    /// End-to-end out-of-memory propagation: when the caller's arena is exhausted part-way through
    /// `deflateCopy`, the C entry point must report `Z_MEM_ERROR`, leave `dest`
    /// untouched, release every buffer it did manage to allocate, and leave
    /// `source` fully usable — never silently completing the copy on the Rust
    /// global allocator.
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
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let init_allocs = stats.allocs();
        assert_eq!(
            init_allocs, 5,
            "deflate init makes exactly C's five requests through the caller's zalloc: \
             the state reservation mirroring `ZALLOC(strm, 1, sizeof(deflate_state))` \
             (`deflate.c` L440-L442), then `window`, `prev`, `head` and the single \
             `pending_buf` that carries the overlaid symbol region (L458-L505)"
        );
        assert_eq!(stats.frees(), 0);

        // Give the source real history so the copy is a meaningful deep copy.
        let input = b"exhausted arena must surface Z_MEM_ERROR ".repeat(8);
        let mut out = std::vec![0u8; 4096];
        src.next_in = input.as_ptr();
        src.avail_in = input.len() as c_uint;
        src.next_out = out.as_mut_ptr();
        src.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut src, Z_NO_FLUSH) }, Z_OK);
        assert_eq!(
            stats.allocs(),
            init_allocs,
            "compression itself must not allocate"
        );

        // Allow exactly two of the copy's five allocations to succeed: the
        // destination state reservation and `window`. C then still *asks* for
        // `prev`, `head` and `pending_buf` before checking, which is what the
        // `ooms()` assertion below pins.
        stats.set_budget(2);
        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { deflateCopy(&mut dst, &mut src) },
            ReturnCode::MemError.as_c_int(),
            "an exhausted caller arena must surface Z_MEM_ERROR"
        );

        // `dest` is untouched: no state installed, no mirrored bookkeeping.
        assert!(
            dst.state.is_null(),
            "a failed copy must not install state into dest"
        );
        assert_eq!(dst.total_in, 0);
        assert_eq!(dst.total_out, 0);
        assert!(dst.zalloc.is_none() && dst.zfree.is_none());

        // The partial copy was released through the caller's `zfree`, and no
        // allocation escaped to the global allocator.
        assert_eq!(
            stats.allocs() - init_allocs,
            2,
            "only the budgeted allocations may succeed"
        );
        assert_eq!(
            stats.ooms(),
            3,
            "C `deflateCopy` issues all four working-buffer `ZALLOC`s and checks \
             them together (`deflate.c` L1341-L1350), so the three that cannot be \
             served each report out-of-memory to the caller's hook"
        );
        assert_eq!(
            stats.frees(),
            2,
            "every buffer the partial copy obtained must be released"
        );

        // The source survived untouched and still produces a correct stream.
        stats.set_budget(usize::MAX);
        loop {
            let rc = unsafe { deflate(&mut src, Z_FINISH) };
            if rc == Z_STREAM_END {
                break;
            }
            assert_eq!(rc, Z_OK);
        }
        let n = src.total_out as usize;
        assert_eq!(zlib_inflate(&out[..n]), input);

        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
        assert_eq!(
            stats.frees(),
            init_allocs + 2,
            "deflateEnd releases the source's buffers through the caller's zfree"
        );
        assert_eq!(
            stats.live_bytes(),
            0,
            "the caller's arena must be perfectly balanced"
        );
    }

    /// The success path of the same setup: with an unbounded caller arena the
    /// copy is allocated entirely from the caller's `zalloc` (never the global
    /// allocator), is independent of the source, and both streams balance.
    #[test]
    fn copy_allocates_from_the_callers_arena_and_stays_independent() {
        use crate::ffi::alloc::test_hook::HookStats;

        let stats = HookStats::new();
        let hook = stats.hook();

        let mut src = zeroed_stream();
        src.zalloc = hook.zalloc();
        src.zfree = hook.zfree();
        src.opaque = hook.opaque();
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        assert_eq!(
            stats.allocs(),
            5,
            "C's five `deflateInit2_` requests exactly: the state reservation plus \
             `window`, `prev`, `head` and the single `pending_buf` holding the \
             overlaid symbol region (`deflate.c` L440-L520)"
        );

        let mut dst = zeroed_stream();
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);
        assert_eq!(
            stats.allocs(),
            10,
            "the copy repeats the same five requests through the caller's zalloc, \
             matching C `deflateCopy` (`deflate.c` L1335-L1372)"
        );
        assert_eq!(stats.ooms(), 0);
        // `deflateCopy` mirrors the allocator triple so `dest` is self-sufficient.
        assert!(dst.zalloc.is_some() && dst.zfree.is_some());
        assert_eq!(dst.opaque, src.opaque);

        // Feed the two streams different data; each must produce its own correct
        // output, proving the buffers are not shared.
        let a = b"stream A payload ".repeat(6);
        let b = b"stream B has different bytes entirely ".repeat(6);
        let mut out_a = std::vec![0u8; 2048];
        let mut out_b = std::vec![0u8; 2048];

        src.next_in = a.as_ptr();
        src.avail_in = a.len() as c_uint;
        src.next_out = out_a.as_mut_ptr();
        src.avail_out = out_a.len() as c_uint;
        dst.next_in = b.as_ptr();
        dst.avail_in = b.len() as c_uint;
        dst.next_out = out_b.as_mut_ptr();
        dst.avail_out = out_b.len() as c_uint;

        while unsafe { deflate(&mut src, Z_FINISH) } != Z_STREAM_END {}
        while unsafe { deflate(&mut dst, Z_FINISH) } != Z_STREAM_END {}

        assert_eq!(zlib_inflate(&out_a[..src.total_out as usize]), a);
        assert_eq!(zlib_inflate(&out_b[..dst.total_out as usize]), b);

        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
        assert_eq!(stats.frees(), 10, "every region reaches the caller's zfree");
        assert_eq!(stats.live_bytes(), 0);
    }

    /// A `zalloc` that always refuses, proving the caller's out-of-memory signal
    /// is never bypassed. It allocates nothing, so no matching `zfree` is needed.
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

    /// CQ-1 regression — `deflateInit2_` must *complete* a half-present
    /// `zalloc`/`zfree` pair exactly as C does, not reject it.
    ///
    /// C's prologue substitutes only the **missing** half — `zcalloc` for a null
    /// `zalloc` (also clearing `opaque`), `zcfree` for a null `zfree` (which
    /// leaves `opaque` alone) — and then proceeds (`deflate.c` L399-L414). This
    /// case pins both halves of that behavior:
    ///
    /// * `zalloc` only, deliberately failing: the caller's hook **is** consulted,
    ///   so the observable code is `Z_MEM_ERROR` (their out-of-memory signal), the
    ///   missing `zfree` was filled in, and `opaque` survived untouched.
    /// * `zfree` only: the built-in `zalloc` was filled in so initialization
    ///   **succeeds**, `opaque` was cleared (that is C's `zalloc` branch), and the
    ///   caller's `zfree` really does release every region at `deflateEnd`.
    #[test]
    fn init_completes_a_half_present_allocator_pair_exactly_as_c_does() {
        // --- zalloc only, and it refuses ------------------------------------
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        strm.opaque = opaque_sentinel();
        strm.msg = c"stale".as_ptr().cast_mut();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_MEM_ERROR,
            "the caller's zalloc must be consulted, so their out-of-memory signal \
             surfaces as Z_MEM_ERROR exactly as it does in C"
        );
        assert!(
            strm.zfree.is_some(),
            "the missing zfree half must be substituted in place (`deflate.c` L408-L413)"
        );
        assert!(strm.zalloc.is_some(), "the caller's own half must be kept");
        assert_eq!(
            strm.opaque,
            opaque_sentinel(),
            "C clears `opaque` only on the zalloc branch (`deflate.c` L405-L406)"
        );
        assert!(strm.state.is_null(), "a failed init must install no state");

        // --- zfree only: initialization succeeds ----------------------------
        let stats = BuiltinHookStats::new();
        let mut strm = zeroed_stream();
        strm.zfree = Some(stats.zfree_fn());
        strm.opaque = opaque_sentinel();
        strm.msg = c"stale".as_ptr().cast_mut();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a caller who supplied only zfree gets the built-in zalloc and a \
             working stream, exactly as in C"
        );
        assert!(
            strm.zalloc.is_some(),
            "the missing zalloc half must be substituted in place (`deflate.c` L400-L407)"
        );
        assert!(
            strm.zfree.is_some(),
            "the caller's own half must survive the substitution untouched"
        );
        assert!(strm.msg.is_null(), "`strm->msg` is cleared unconditionally");
        assert!(
            strm.opaque.is_null(),
            "C clears `opaque` on the zalloc branch, because the cookie belonged \
             to the allocator being replaced (`deflate.c` L405-L406)"
        );
        assert!(!strm.state.is_null());

        // The caller's own `zfree` — invoked with the cleared, null `opaque`, just
        // as C invokes it — is what releases the engine's five regions.
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            stats.allocs(),
            0,
            "the caller supplied no zalloc, so none of their allocations ran"
        );
        assert_eq!(
            stats.frees(),
            5,
            "the caller's zfree releases C's five `deflateInit2_` regions: the \
             state reservation plus `window`, `prev`, `head` and `pending_buf` \
             (`deflate.c` L440-L520)"
        );
    }

    /// The prologue runs in exactly C's position: after the version and
    /// null-stream guards, before the level/method/`windowBits`/`memLevel`/
    /// strategy validation (`deflate.c` L392-L432).
    #[test]
    fn the_allocator_prologue_is_ordered_exactly_as_c_orders_it() {
        // A bad strategy *and* a half hook. C validates parameters after the
        // prologue, so the answer is `Z_STREAM_ERROR` — but the prologue has
        // already run, which the substituted `zfree` and the cleared `msg` prove.
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        strm.msg = c"stale".as_ptr().cast_mut();
        assert_eq!(
            unsafe {
                deflateInit2_(
                    &mut strm,
                    6,
                    Z_DEFLATED,
                    MAX_WBITS,
                    DEF_MEM_LEVEL,
                    99,
                    ver(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_STREAM_ERROR
        );
        assert!(strm.state.is_null());
        assert!(
            strm.zfree.is_some(),
            "the prologue precedes parameter validation, so the pair was completed \
             even though the init then failed"
        );
        assert!(
            strm.msg.is_null(),
            "`strm->msg = Z_NULL` is unconditional and precedes every later return"
        );

        // A bad version *and* a half hook: the version guard wins, and it precedes
        // the prologue, so neither the hooks nor `msg` may be touched at all.
        let stale = c"stale".as_ptr().cast_mut();
        let mut strm = zeroed_stream();
        strm.zalloc = Some(always_fail_zalloc);
        strm.msg = stale;
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ptr::null(), size_of::<z_stream>() as c_int) },
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
    }

    /// The two *complete* configurations must be untouched: neither half supplied
    /// (the common zeroed `z_stream`) and both halves supplied (routed through the
    /// caller's allocator).
    ///
    /// A wholly absent pair is substituted per half, exactly as C substitutes
    /// `zcalloc`/`zcfree` (`deflate.c` L400-L414), so the caller's `z_stream`
    /// publishes two non-null halves afterwards. That publication is pure ABI
    /// shape: [`crate::ffi::types::CAllocator::is_builtin_pair`] recognizes the
    /// crate's own substitutes and reports *no* hook, so the engine keeps using
    /// the global-allocator path and a hookless caller's allocation count and
    /// engine-state footprint stay byte-for-byte what they have always been
    /// (AAP §0.6.5).
    #[test]
    fn init_accepts_both_hooks_and_neither_hook() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a zeroed z_stream must still initialize"
        );
        assert!(!strm.state.is_null());
        assert!(
            strm.zalloc.is_some() && strm.zfree.is_some(),
            "C substitutes each missing half unconditionally (`deflate.c` \
             L400-L414), so a hookless caller's stream publishes a complete pair"
        );
        assert!(
            crate::ffi::types::publishes_builtin_alloc_pair(&strm),
            "the published pair must be the crate's own built-ins — the \
             counterpart of C's zcalloc/zcfree — not a caller hook"
        );
        assert!(strm.opaque.is_null(), "a zeroed cookie stays zeroed");
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };
        let mut strm = zeroed_stream();
        attach_budget(&mut strm, &budget);
        let cookie = strm.opaque;
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK,
            "a complete hook pair with budget must initialize"
        );
        assert!(!strm.state.is_null());
        assert!(
            core::ptr::eq(strm.opaque, cookie),
            "a complete pair is left exactly as the caller supplied it"
        );
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert!(
            budget.remaining.load(core::sync::atomic::Ordering::SeqCst) < 64,
            "a complete pair must actually route the working buffers through the \
             caller's zalloc, not through the built-in"
        );
    }
}
