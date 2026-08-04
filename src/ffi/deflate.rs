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
        // opaque `state` handle and binds the handle's owner to this stream — C's
        // `s->strm = strm` (`deflate.c` L444). It is reclaimed exactly once by
        // `deflateEnd`, whose `deflate_take` runs the whole of
        // `deflateStateCheck` — tag, owner, allocator pair and status — before
        // reconstituting the box, and nulls `state` on success.
        unsafe { install_handle(s, handle) };
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
/// `strm` may be null, which is rejected with `Z_STREAM_ERROR` before any
/// dereference. When non-null it must be a valid, exclusively-owned `z_stream`
/// previously initialized by `deflateInit*`, with its `state` handle unmodified
/// by the caller. A handle belonging to a different engine is *not* undefined
/// behavior either: the `HandleKind` tag is checked before the state is
/// reborrowed, and a mismatch yields `Z_STREAM_ERROR`.
///
/// **Buffer pointers and counts.** Each `next_*` pointer paired with a non-zero
/// `avail_*` count must address that many valid, live bytes — readable for
/// `next_in`, writable for `next_out` — and must be correctly aligned for `u8`
/// (that is, any non-null address). Overstating an `avail_*` count relative to
/// the real allocation, or passing a dangling or freed pointer, is undefined
/// behavior; the C API carries no length information with which to detect it,
/// so this obligation cannot be discharged by the callee.
///
/// **Aliasing.** The input region `next_in[..avail_in]` and the output region
/// `next_out[..avail_out]` must not overlap each other, and neither may overlap
/// the `*strm` struct itself. This is a hard requirement rather than a
/// convention: the two regions are bridged to a `&[u8]` and a `&mut [u8]` that
/// are live simultaneously, so an overlap would create a shared and a unique
/// reference to the same bytes. For the duration of the call the caller must
/// also not access `*strm` or either buffer from another thread, since `*strm`
/// is reborrowed as `&mut`.
///
/// **Pre-dereference validation — the two defined rejection cases.** Two
/// pointer/count combinations that this contract would otherwise appear to
/// forbid are in fact *defined, rejected* configurations rather than undefined
/// behavior, and callers (including the fuzz harness's
/// `probe_buffer_validation`) may rely on that:
///
/// - `next_out == NULL`, rejected **unconditionally** with `Z_STREAM_ERROR` —
///   with no `avail_out == 0` qualifier, matching C, whose `next_out == Z_NULL`
///   test likewise carries none.
/// - `avail_in != 0` paired with `next_in == NULL`, rejected with
///   `Z_STREAM_ERROR`.
///
/// Both are tested by [`stream_buffers_valid`] at the top of the body, *before*
/// `input_slice`/`output_slice` bridge anything, so neither pointer is
/// dereferenced and no slice is ever formed from a null base. This mirrors C
/// `deflate`'s entry guards (`deflate.c` L981-L1010) and exists precisely so the
/// programmer error surfaces as an error code instead of being silently masked
/// into an empty slice. The complementary shape `avail_in == 0` with
/// `next_in == NULL` is simply legal: nothing is read.
///
/// **Unwinding.** The body runs inside a `guard_int` panic guard, so a Rust
/// panic can never unwind across this `extern "C"` boundary. With `std` the
/// guard is a `catch_unwind` that substitutes `Z_STREAM_ERROR`; without it the
/// closure runs directly, because a `no_std` build has no unwinding runtime to
/// catch and the crate sets `panic = "abort"` in both profiles, so a panic
/// aborts the process rather than crossing the boundary. Either way the caller
/// never observes a foreign unwind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deflate(strm: z_streamp, flush: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and points at a valid, uniquely-owned
        // `z_stream` for the duration of this call.
        let s = unsafe { &mut *strm };

        // C `deflate` validates the *stream* before it looks at anything else:
        // `if (deflateStateCheck(strm) || flush > Z_BLOCK || flush < 0) return
        // Z_STREAM_ERROR;` precedes the `next_out`/`next_in` null tests
        // (`deflate.c` L981-L1010). Running it first here is both C-faithful and
        // a soundness requirement: `next_in`/`next_out` on an unvalidated stream
        // may be stale, and turning a stale pointer into a slice is undefined
        // behavior even when the slice is never read. The check borrows nothing
        // outside `s` and its own handle.
        // SAFETY: `state`, when non-null, was installed by `deflateInit*` via
        // `install_handle`, so its `#[repr(C)]` prefix is readable.
        if !unsafe { deflate_state_check(s) } {
            return Z_STREAM_ERROR;
        }

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
            let Some(handle) = (unsafe { deflate_handle(s) }) else {
                return Z_STREAM_ERROR;
            };
            // C re-reads the caller's `gz_header` here, at emission time, through
            // the pointer `deflateSetHeader` stored (`deflate.c` L1092-L1188).
            // Re-borrowing it on every call is what makes a mutation performed
            // after registration but before emission observable in the output
            // bytes, exactly as in C.
            //
            // SAFETY: `handle.head` is null, or the `gz_header` the caller passed
            // to `deflateSetHeader` and undertook to keep valid until emission
            // completes (`zlib.h` L843-L847). The borrow lives only for this call.
            #[cfg(feature = "gzip")]
            let lent = unsafe { borrow_gz_header(handle.head) };
            let zs = &mut handle.zs;
            let outcome = engine::deflate_lending(
                zs,
                input,
                output,
                flush,
                #[cfg(feature = "gzip")]
                lent.as_ref(),
            );
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
            if let Some(handle) = unsafe { deflate_handle(s) } {
                // C's `deflateBound` reads the live header too (`deflate.c`
                // L893-L907).
                // SAFETY: as in `deflate`; see `borrow_gz_header`.
                #[cfg(feature = "gzip")]
                let lent = unsafe { borrow_gz_header(handle.head) };
                return engine::deflate_bound_lending(
                    &handle.zs,
                    source_len as usize,
                    #[cfg(feature = "gzip")]
                    lent.as_ref(),
                ) as uLong;
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
        if let Some(handle) = unsafe { deflate_handle(s) } {
            // C's `deflateBound` reads the live header too (`deflate.c` L893-L907).
            // SAFETY: as in `deflate`; see `borrow_gz_header`.
            #[cfg(feature = "gzip")]
            let lent = unsafe { borrow_gz_header(handle.head) };
            return engine::deflate_bound_z_lending(
                &handle.zs,
                source_len,
                #[cfg(feature = "gzip")]
                lent.as_ref(),
            );
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

        // C evaluates `deflateStateCheck(strm) || dictionary == Z_NULL` as one
        // expression, so the stream is validated *first* — before the dictionary
        // pointer is looked at at all, and long before it is read. Reproducing
        // that order is a soundness requirement here, not a stylistic one:
        // `slice::from_raw_parts` below would otherwise be handed a pointer whose
        // provenance C never vouched for, and constructing a slice over invalid
        // memory is undefined behavior even if the slice is never read.
        // SAFETY: `state`, when non-null, was installed by `deflateInit*` via
        // `install_handle`; the check reads nothing outside `s` and its handle.
        if !unsafe { deflate_state_check(s) } {
            return Z_STREAM_ERROR;
        }

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
/// unless the stream is in gzip mode (`wrap == 2`), mirroring C; otherwise
/// `Z_OK`. Those are the only two outcomes, exactly as `zlib.h` L854-L855
/// documents — nothing is copied and nothing is allocated, so no memory error is
/// reachable. A `NULL` header clears any previously set header (restoring the
/// default), exactly as assigning `Z_NULL` does in C.
///
/// # The header stays the caller's, and stays live
///
/// C does exactly two things here: validate, then `s->gzhead = head`
/// (`deflate.c` L714-L719). Every field is then re-read **lazily**, when the
/// header is finally sized by `deflateBound` (`deflate.c` L893-L907) or emitted
/// by `deflate` (`deflate.c` L1092-L1188). This shim reproduces that: it records
/// only the pointer, and the engine borrows the caller's fields per call.
///
/// The consequence is caller-visible and load-bearing: a mutation made after
/// registration but before emission **is** reflected in the compressed bytes. A
/// deep copy taken here would instead emit the registration-time snapshot, which
/// is a different gzip stream for a legitimate call sequence and therefore a
/// byte-identity divergence (AAP §0.8.1 D-1), and would additionally introduce a
/// `Z_MEM_ERROR` on an entry point C cannot fail.
///
/// The registration survives `deflateReset`: C clears `gzhead` only in
/// `deflateInit2_` (`deflate.c` L448). It also travels to a `deflateCopy` clone,
/// because C's struct-wide `zmemcpy` duplicates the pointer (`deflate.c` L1339),
/// so both streams read the caller's one header.
///
/// # Safety
///
/// `strm` must be null or a valid `z_stream` initialized by `deflateInit*`.
/// `head` must be null or point to a valid `gz_header`. Because only the pointer
/// is retained, that header — and the `extra`/`name`/`comment` buffers it names —
/// must stay valid until the header has been emitted or the registration has been
/// replaced, which is the same obligation C places on the caller.
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
            let Some(handle) = (unsafe { deflate_handle(s) }) else {
                return Z_STREAM_ERROR;
            };
            // C does exactly two things here: validate, then `s->gzhead = head`
            // (`deflate.c` L714-L719). It copies nothing, reads nothing through
            // `head`, and allocates nothing — so neither does this shim.
            //
            // The header is therefore *not* converted to an owned `GzHeader`. A
            // deep copy would introduce two divergences that reference zlib
            // cannot exhibit: a `Z_MEM_ERROR` from an entry point whose only
            // documented outcomes are `Z_OK` and `Z_STREAM_ERROR` (`zlib.h`
            // L854-L855), and a stale snapshot that ignores mutations the caller
            // makes before the header is emitted — which changes the emitted gzip
            // bytes and so breaks byte identity (AAP §0.8.1 D-1).
            //
            // Ordering note: the engine slot is set *before* the pointer is
            // stored, so that a stream which fails the `wrap != 2` test keeps
            // whatever header it already had, exactly as C leaves `s->gzhead`
            // untouched when it returns `Z_STREAM_ERROR`.
            let rc = code_of(engine::deflate_set_header_foreign(
                &mut handle.zs,
                !head.is_null(),
            ));
            if rc == Z_OK {
                handle.head = head;
            }
            rc
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
/// buffer through the **same** `zalloc` as the source — AAP §0.6.5).
///
/// Every application-visible `z_stream` field is mirrored from `source` into
/// `dest` *before* the first allocation, because that is where C does it:
/// `zmemcpy(dest, source, sizeof(z_stream))` at `deflate.c` L1333 precedes the
/// destination-state `ZALLOC` at L1335. The ordering is observable — on
/// `Z_MEM_ERROR` C leaves `dest` carrying the source's cursors, totals, `msg`,
/// allocator triple, `data_type` and `adler` rather than leaving it untouched —
/// so this shim reproduces it. Returns `Z_MEM_ERROR` if a copy allocation fails,
/// with `dest.state` left as the caller had it (see the note at the mirror below).
///
/// `Z_STREAM_ERROR` is returned when either pointer is null, when `source`
/// carries no deflate state, or when `source`'s caller-visible `zalloc`/`zfree`
/// pair is only half present — the conditions C reaches through
/// `deflateStateCheck(source) || dest == Z_NULL` (`deflate.c` L1327-L1329,
/// L540-L541) before it touches `dest` or allocates anything.
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

        // C evaluates `deflateStateCheck(source) || dest == Z_NULL` as one
        // expression (`deflate.c` L1327-L1329), so the *source* stream is fully
        // validated before anything else is read — including its own allocator
        // fields — and before `dest` is touched at all. Running the whole
        // predicate first means an invalid source is rejected before a reference
        // to either stream is formed for any other purpose.
        // SAFETY: `source` is non-null and valid; the check reads only `source`
        // and its own handle prefix, and creates no lasting borrow.
        if !unsafe { deflate_state_check(&*source) } {
            return Z_STREAM_ERROR;
        }

        // Capture the source's **caller-visible** `zalloc`/`zfree`/`opaque`
        // triple. C charges every clone allocation to exactly this triple: it
        // `zmemcpy`s the whole `z_stream` from `source` into `dest` and then
        // allocates through `ZALLOC(dest, …)` (`deflate.c` L1332-L1341), so the
        // pair the caller can currently see — not whatever pair was captured at
        // initialization — owns the cloned buffers. The read yields a plain
        // `Copy` value, so it completes here and no shared borrow survives into
        // the `&mut` borrows taken below.
        // SAFETY: `source` is non-null and a valid `z_stream`; only the plain
        // `Copy` allocator fields are copied out, and no hook pointer is
        // dereferenced.
        let source_alloc = unsafe { CAllocator::from_stream(&*source) };

        // C rejects a source whose `zalloc` **or** `zfree` is null through
        // `deflateStateCheck(source)` (`deflate.c` L540-L541), evaluated in
        // `deflateCopy` before the first `ZALLOC(dest, …)` (`deflate.c`
        // L1327-L1329). `init_allocator_prologue` substitutes the crate's
        // built-in for every missing half, so a stream this crate initialized
        // always publishes two non-null halves and this can only fire when the
        // fields were mutated after initialization. Rejecting here — before
        // `dest` is written and before anything is allocated — keeps the copy
        // from silently allocating out of the global heap while the caller
        // believes their own hook owns the memory.
        if source_alloc.is_half_present() {
            return Z_STREAM_ERROR;
        }

        // SAFETY: both pointers are non-null and, per the zlib contract, refer
        // to distinct (non-aliasing) valid streams for this call.
        let d = unsafe { &mut *dest };
        // SAFETY: see above; `source` is distinct from `dest`.
        let src = unsafe { &mut *source };

        // Mirror the whole caller-visible `z_stream` **before** anything is
        // allocated, reproducing C's `zmemcpy(dest, source, sizeof(z_stream))`
        // (`deflate.c` L1333), which precedes the destination-state `ZALLOC` at
        // L1335. Doing it here rather than after a successful clone is what makes
        // the `Z_MEM_ERROR` path match: reference zlib leaves `dest` holding the
        // source's cursors, totals, `msg`, allocator triple, `data_type` and
        // `adler`, and a caller inspecting `dest` after a refused copy sees them.
        //
        // The one field C's struct copy carries that is deliberately **not**
        // mirrored is the opaque `state` pointer. C's memcpy leaves
        // `dest->state == source->state` until L1337 overwrites it, so on the
        // failure path C hands the caller a pointer into *another* stream's state.
        // That is not reproduced here, for three reasons that together make it
        // unobservable through the zlib contract: `zlib.h` declares the field "not
        // visible by applications"; C's own `deflateStateCheck` rejects the alias
        // through its `s->strm != strm` clause, exactly as this port's owner-bound
        // handle header does, so `deflateEnd(dest)` returns `Z_STREAM_ERROR` in
        // both implementations (measured, not assumed); and publishing a live
        // pointer to a state this stream does not own would defeat the owner
        // binding that makes cross-stream reclamation impossible by construction.
        // On the success path `install_handle` below writes `dest.state` itself,
        // which is C's L1337.
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

        // Deep-copy the source engine into a fresh stream carrying the same
        // allocator, then install it into `dest`.
        //
        // The registered `gz_header` pointer travels with the copy. C's
        // `zmemcpy(ds, ss, sizeof(deflate_state))` (`deflate.c` L1345) duplicates
        // the whole state struct, and `gzhead` is one of its members, so the
        // clone ends up pointing at the *same* caller-owned header as the
        // original — it is never deep-copied and never cleared. Carrying the raw
        // pointer across reproduces that exactly: both streams read the caller's
        // one live header, and a mutation the caller makes afterwards is seen by
        // both. Only the pointer is copied here; nothing is dereferenced.
        #[cfg(feature = "gzip")]
        let src_head;
        let new_zs = {
            // SAFETY: `source.state`, when non-null, is a
            // `Box<DeflateHandle>`; `deflate_handle` runs the whole of
            // `deflateStateCheck` — confirming the `HandleKind::DEFLATE` tag and
            // the owner — before reborrowing the handle.
            let Some(src_handle) = (unsafe { deflate_handle(src) }) else {
                return Z_STREAM_ERROR;
            };
            #[cfg(feature = "gzip")]
            {
                src_head = src_handle.head;
            }
            let mut new_zs = ZStream::with_allocator(source_alloc);
            if let Err(err) = engine::deflate_copy(&mut new_zs, &src_handle.zs) {
                return err.as_return_code().as_c_int();
            }
            new_zs
        };

        // Box the cloned handle **fallibly** before `dest.state` is written, so
        // global-heap exhaustion is reported as `Z_MEM_ERROR` rather than an abort
        // — the outcome C reaches when its destination `ZALLOC` fails and it calls
        // `deflateEnd(dest)` (AAP §0.6.5). `dest` keeps the mirrored fields written
        // above, exactly as C's earlier struct copy leaves them, and its `state` is
        // never touched. The dropped `new_zs` releases the freshly cloned buffers
        // through the caller's `zfree`.
        let Some(handle) = try_box(DeflateHandle::new(new_zs)) else {
            return Z_MEM_ERROR;
        };

        // Carry the registered header pointer onto the clone, matching C's
        // struct-wide `zmemcpy` (see the note above the copy). The engine-side
        // `GzHeaderSlot` was already duplicated by `deflate_copy`, which
        // propagates the `Foreign` marker; this restores the raw pointer that
        // marker refers to.
        #[cfg(feature = "gzip")]
        let handle = {
            let mut handle = handle;
            handle.head = src_head;
            handle
        };

        // Install the cloned state into `dest`.
        // SAFETY: transfers ownership of the tagged `Box<DeflateHandle>` into
        // `dest.state` and re-points the clone's owner at `dest`, reproducing C's
        // `ds->strm = dest` (`deflate.c` L1340). That re-pointing is what makes
        // the two streams independent: `deflateEnd(dest)` reclaims the clone and
        // `deflateEnd(source)` reclaims only the original, so neither can free the
        // other's state. Reclaimed exactly once by `deflateEnd`, whose
        // `deflate_take` runs the whole of `deflateStateCheck` first.
        unsafe { install_handle(d, handle) };

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
    #[cfg(feature = "gzip")]
    use core::ffi::c_uchar;
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

    /// Reads the four match-finder thresholds out of an initialized stream's
    /// engine state, through the same tag-validating accessor the shims use.
    ///
    /// # Panics
    ///
    /// If `strm` carries no deflate handle.
    fn tune_fields(strm: &mut z_stream) -> (usize, usize, c_int, usize) {
        // The engine accessors live on this trait rather than on `ZStream`, which
        // is what keeps the `stream` layer free of an upward edge into `deflate`
        // (AAP §0.3.1, §0.4.2); it has to be in scope to call them.
        use crate::deflate::state::DeflateStream;

        // SAFETY: the caller passes an initialized `z_stream`; `deflate_state`
        // compares the `HandleKind` tag before reinterpreting `state`, so a
        // stateless or foreign-handle stream yields `None` rather than a misread.
        let zs = unsafe { deflate_state(strm) }.expect("an initialized stream has a handle");
        let s = zs.deflate_state().expect("the handle carries engine state");
        (
            s.good_match,
            s.max_lazy_match,
            s.nice_match,
            s.max_chain_length,
        )
    }

    /// `int deflateTune(z_streamp, int, int, int, int)` at the C boundary
    /// (`ZLIB_1.2.2.3`).
    ///
    /// C `deflateTune` (`deflate.c` L819-L830) performs the state check and then
    /// writes all four values verbatim — it validates nothing else and cannot
    /// fail for any other reason. Both halves are asserted here: every rejection
    /// path returns exactly `Z_STREAM_ERROR`, and an accepted call is observed
    /// **on the state** rather than merely by its return code, because a shim that
    /// returned `Z_OK` while dropping the values on the floor would look identical
    /// from the outside. `deflateTune` sits directly on the byte-identity surface
    /// (AAP §0.6.4 (d)), so "it returned `Z_OK`" is not evidence that it worked.
    #[test]
    fn deflate_tune_at_the_c_boundary() {
        // A null stream is rejected before any field is read.
        assert_eq!(
            unsafe { deflateTune(ptr::null_mut(), 8, 16, 128, 128) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        // A zeroed, never-initialized stream: `state` is null, which the handle
        // accessor reports as "no handle" without ever loading through it.
        let mut bare = zeroed_stream();
        assert_eq!(
            unsafe { deflateTune(&mut bare, 8, 16, 128, 128) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Level 6 loads `configuration_table[6] == {8, 16, 128, 128}`
        // (`deflate.c` L121). Establishing the pre-state is what makes the
        // override below meaningful rather than a coincidence.
        assert_eq!(
            tune_fields(&mut strm),
            (8, 16, 128, 128),
            "level 6 starts from its configuration-table row"
        );

        assert_eq!(
            unsafe { deflateTune(&mut strm, 33, 133, 259, 4097) },
            Z_OK,
            "an initialized stream accepts advanced overrides"
        );
        assert_eq!(
            tune_fields(&mut strm),
            (33, 133, 259, 4097),
            "all four thresholds are written through to the state"
        );

        // C imposes no range check whatsoever, so zeros are accepted and stored
        // verbatim. A shim that "helpfully" rejected or clamped them would diverge
        // from reference zlib on a call reference zlib accepts.
        assert_eq!(unsafe { deflateTune(&mut strm, 0, 0, 0, 0) }, Z_OK);
        assert_eq!(
            tune_fields(&mut strm),
            (0, 0, 0, 0),
            "C validates nothing here; the values are stored as given"
        );

        // The stream still works after being tuned: the thresholds change which
        // matches the finder accepts, never whether the output is a valid stream.
        let corpus = b"tuned deflate stream, tuned deflate stream, tuned. ".repeat(20);
        let mut out = std::vec![0u8; 64 * 1024];
        strm.next_in = corpus.as_ptr();
        strm.avail_in = corpus.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        let produced = strm.total_out as usize;
        assert_eq!(
            zlib_inflate(&out[..produced]),
            corpus,
            "a tuned stream still round-trips"
        );

        // A stream carrying the OTHER engine's handle is rejected by the tag
        // check rather than misread as a deflate state.
        let mut inf = zeroed_stream();
        assert_eq!(
            unsafe {
                crate::ffi::inflate::inflateInit_(&mut inf, ver(), size_of::<z_stream>() as c_int)
            },
            Z_OK
        );
        assert_eq!(
            unsafe { deflateTune(&mut inf, 8, 16, 128, 128) },
            Z_STREAM_ERROR,
            "an inflate handle is not a deflate handle"
        );
        assert_eq!(unsafe { crate::ffi::inflate::inflateEnd(&mut inf) }, Z_OK);

        // After teardown the stream is stateless again, so tuning is rejected.
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            unsafe { deflateTune(&mut strm, 8, 16, 128, 128) },
            Z_STREAM_ERROR,
            "deflateEnd nulls `state`, so a later tune is rejected"
        );
    }

    /// `int deflatePrime(z_streamp, int, int)` at the C boundary
    /// (`ZLIB_1.2.0.8`).
    ///
    /// Three distinct outcomes are reachable and all three are asserted with
    /// their exact codes: `Z_STREAM_ERROR` for a null or stateless stream,
    /// `Z_BUF_ERROR` for a width outside `0..=16` (`deflate.c` L756-L758), and
    /// `Z_OK` with the bits actually buffered. The success case is observed
    /// through `deflatePending`, which is the only C-visible witness of the bit
    /// accumulator: a shim that returned `Z_OK` without priming anything would
    /// otherwise be indistinguishable, and `deflatePrime` exists precisely so a
    /// caller can splice streams at a known bit offset.
    #[test]
    fn deflate_prime_at_the_c_boundary() {
        assert_eq!(
            unsafe { deflatePrime(ptr::null_mut(), 8, 0) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        let mut bare = zeroed_stream();
        assert_eq!(
            unsafe { deflatePrime(&mut bare, 8, 0) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        let pending = |strm: &mut z_stream| -> (c_uint, c_int) {
            let mut bytes: c_uint = 0xdead;
            let mut bits: c_int = -1;
            assert_eq!(
                unsafe { deflatePending(strm, &mut bytes, &mut bits) },
                Z_OK,
                "deflatePending must succeed on an initialized stream"
            );
            (bytes, bits)
        };

        assert_eq!(
            pending(&mut strm),
            (0, 0),
            "a fresh stream has nothing pending"
        );

        // Five bits fit the empty accumulator without completing a byte.
        assert_eq!(unsafe { deflatePrime(&mut strm, 5, 0b1_0101) }, Z_OK);
        assert_eq!(
            pending(&mut strm),
            (0, 5),
            "the primed bits are buffered, not emitted"
        );

        // Sixteen more: the low two bytes flush least-significant first and five
        // bits stay buffered (21 bits total). The exact bytes are asserted by
        // `deflate::tests::deflate_prime_packs_bits_low_order_first_and_rejects_bad_widths`;
        // what matters here is that the C entry point drives the same engine.
        assert_eq!(unsafe { deflatePrime(&mut strm, 16, 0xffff) }, Z_OK);
        assert_eq!(pending(&mut strm), (2, 5));

        // A zero-width prime is legal and a no-op, matching C's loop, which exits
        // immediately when `bits == 0`.
        assert_eq!(unsafe { deflatePrime(&mut strm, 0, 0) }, Z_OK);
        assert_eq!(pending(&mut strm), (2, 5), "a 0-bit prime changes nothing");

        // Out-of-range widths are `Z_BUF_ERROR`, NOT `Z_STREAM_ERROR`: the
        // distinction is observable and C makes it.
        assert_eq!(
            unsafe { deflatePrime(&mut strm, 17, 0) },
            Z_BUF_ERROR,
            "17 bits exceeds the 16-bit limit"
        );
        assert_eq!(
            unsafe { deflatePrime(&mut strm, -1, 0) },
            Z_BUF_ERROR,
            "a negative width is rejected"
        );
        assert_eq!(
            pending(&mut strm),
            (2, 5),
            "a rejected prime must not disturb the accumulator"
        );

        // A cross-engine handle is rejected by the tag check.
        let mut inf = zeroed_stream();
        assert_eq!(
            unsafe {
                crate::ffi::inflate::inflateInit_(&mut inf, ver(), size_of::<z_stream>() as c_int)
            },
            Z_OK
        );
        assert_eq!(
            unsafe { deflatePrime(&mut inf, 8, 0) },
            Z_STREAM_ERROR,
            "an inflate handle is not a deflate handle"
        );
        assert_eq!(unsafe { crate::ffi::inflate::inflateEnd(&mut inf) }, Z_OK);

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            unsafe { deflatePrime(&mut strm, 8, 0) },
            Z_STREAM_ERROR,
            "deflateEnd nulls `state`, so a later prime is rejected"
        );
    }

    /// `int deflateGetDictionary(z_streamp, Bytef *, uInt *)` at the C boundary
    /// (`ZLIB_1.2.9`).
    ///
    /// This shim is the only one in the module that implements the engine's
    /// two-call pattern — query the length with `None`, then copy into a slice of
    /// exactly that length — and it must honour C's rule that **either** output
    /// pointer may be `NULL` independently. All four combinations are exercised,
    /// and the bytes it produces are cross-checked against
    /// [`engine::deflate_get_dictionary`] read through the same handle, so the
    /// shim cannot pass by copying the right count of the wrong bytes.
    #[test]
    fn deflate_get_dictionary_at_the_c_boundary() {
        const DICT: &[u8] = b"the quick brown fox jumps over the lazy dog";

        assert_eq!(
            unsafe { deflateGetDictionary(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        let mut bare = zeroed_stream();
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { deflateGetDictionary(&mut bare, ptr::null_mut(), &mut len) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );
        assert_eq!(
            len, 0xdead,
            "a rejected call must not write the length out-parameter"
        );

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Before any dictionary or input: length 0, and a supplied buffer is left
        // untouched because the engine's `len != 0` guard suppresses the copy.
        let mut buf = [0xa5u8; 64];
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { deflateGetDictionary(&mut strm, buf.as_mut_ptr(), &mut len) },
            Z_OK
        );
        assert_eq!(len, 0, "a fresh stream has an empty dictionary");
        assert!(
            buf.iter().all(|&b| b == 0xa5),
            "an empty dictionary writes no bytes"
        );

        assert_eq!(
            unsafe { deflateSetDictionary(&mut strm, DICT.as_ptr(), DICT.len() as uInt) },
            Z_OK
        );

        // (a) Length-only query: `dictionary == NULL`, `dictLength` non-null.
        //     This is the sizing half of the C idiom, and the buffer must stay
        //     untouched because it was never passed.
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { deflateGetDictionary(&mut strm, ptr::null_mut(), &mut len) },
            Z_OK
        );
        assert_eq!(len, DICT.len() as uInt, "the length-only query reports it");
        assert!(buf.iter().all(|&b| b == 0xa5));

        // (b) Both output pointers non-null: exact bytes AND exact length.
        let mut len: uInt = 0;
        assert_eq!(
            unsafe { deflateGetDictionary(&mut strm, buf.as_mut_ptr(), &mut len) },
            Z_OK
        );
        assert_eq!(len, DICT.len() as uInt);
        assert_eq!(&buf[..DICT.len()], DICT, "the exact dictionary bytes");
        assert!(
            buf[DICT.len()..].iter().all(|&b| b == 0xa5),
            "exactly `len` bytes are written and no more"
        );

        // (c) Buffer only: `dictLength == NULL` must not be dereferenced, and the
        //     copy must still happen.
        let mut buf2 = [0x5au8; 64];
        assert_eq!(
            unsafe { deflateGetDictionary(&mut strm, buf2.as_mut_ptr(), ptr::null_mut()) },
            Z_OK,
            "a NULL dictLength is legal, not an error"
        );
        assert_eq!(&buf2[..DICT.len()], DICT);
        assert!(buf2[DICT.len()..].iter().all(|&b| b == 0x5a));

        // (d) Neither output pointer: a pure "is this stream usable" probe.
        assert_eq!(
            unsafe { deflateGetDictionary(&mut strm, ptr::null_mut(), ptr::null_mut()) },
            Z_OK,
            "both output pointers NULL is legal"
        );

        // Cross-check against the safe core reached through the same handle: the
        // shim must not merely produce the right NUMBER of bytes.
        {
            let zs = unsafe { deflate_state(&mut strm) }.expect("handle");
            let core_len = engine::deflate_get_dictionary(zs, None).expect("core length query");
            assert_eq!(core_len, DICT.len(), "core and shim agree on the length");
            let mut core_bytes = std::vec![0u8; core_len];
            assert_eq!(
                engine::deflate_get_dictionary(zs, Some(&mut core_bytes)),
                Ok(core_len)
            );
            assert_eq!(
                &buf[..core_len],
                &core_bytes[..],
                "the shim copied exactly what the safe core reports"
            );
        }

        // A cross-engine handle is rejected by the tag check.
        let mut inf = zeroed_stream();
        assert_eq!(
            unsafe {
                crate::ffi::inflate::inflateInit_(&mut inf, ver(), size_of::<z_stream>() as c_int)
            },
            Z_OK
        );
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { deflateGetDictionary(&mut inf, ptr::null_mut(), &mut len) },
            Z_STREAM_ERROR,
            "an inflate handle is not a deflate handle"
        );
        assert_eq!(len, 0xdead);
        assert_eq!(unsafe { crate::ffi::inflate::inflateEnd(&mut inf) }, Z_OK);

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            unsafe { deflateGetDictionary(&mut strm, ptr::null_mut(), ptr::null_mut()) },
            Z_STREAM_ERROR,
            "deflateEnd nulls `state`, so a later query is rejected"
        );
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

    /// Which half of the source's published allocator pair a
    /// [`copy_rejects_half_present_source`] scenario knocks out after init.
    #[derive(Copy, Clone)]
    enum ClearedHalf {
        /// Clear `source->zalloc`, leaving `source->zfree` in place.
        Zalloc,
        /// Clear `source->zfree`, leaving `source->zalloc` in place.
        Zfree,
    }

    /// Shared body for the two half-present-source rejection tests.
    ///
    /// C's `deflateCopy` reaches `Z_STREAM_ERROR` through
    /// `deflateStateCheck(source)`, which classifies a stream whose `zalloc`
    /// **or** `zfree` is null as invalid (`deflate.c` L540-L541), and it reaches
    /// that verdict *before* `zmemcpy(dest, source, sizeof(z_stream))` and before
    /// the first `ZALLOC(dest, …)` (`deflate.c` L1327-L1341). Because
    /// `init_allocator_prologue` substitutes the crate's built-in for every
    /// missing half, the only route to a half-present pair is post-init surgery
    /// on the caller's `z_stream` — and that must never be allowed to route the
    /// clone through the global heap while the caller believes their own hook
    /// owns the memory.
    fn copy_rejects_half_present_source(cleared: ClearedHalf) {
        let budget = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };

        let mut src = zeroed_stream();
        attach_budget(&mut src, &budget);
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );

        // Post-init surgery: knock out exactly one half of the published pair.
        let saved_zalloc = src.zalloc;
        let saved_zfree = src.zfree;
        match cleared {
            ClearedHalf::Zalloc => src.zalloc = None,
            ClearedHalf::Zfree => src.zfree = None,
        }

        let mut dst = zeroed_stream();
        attach_budget(&mut dst, &budget);
        let budget_before = budget.remaining.load(core::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            unsafe { deflateCopy(&mut dst, &mut src) },
            Z_STREAM_ERROR,
            "a half-present source allocator pair must be rejected exactly as C's \
             deflateStateCheck does"
        );

        // `dest` must be *entirely* untouched, because C evaluates the check
        // before its full-struct copy: not one observable field is written.
        assert!(dst.state.is_null(), "no state may be installed");
        assert!(dst.next_in.is_null());
        assert_eq!(dst.avail_in, 0);
        assert_eq!(dst.total_in, 0);
        assert!(dst.next_out.is_null());
        assert_eq!(dst.avail_out, 0);
        assert_eq!(dst.total_out, 0);
        assert!(dst.msg.is_null());
        assert_eq!(dst.data_type, 0);
        assert_eq!(dst.adler, 0);
        assert_eq!(dst.reserved, 0);
        assert_eq!(
            budget.remaining.load(core::sync::atomic::Ordering::SeqCst),
            budget_before,
            "the rejection must precede every allocation"
        );

        // Restoring the missing half leaves the source fully usable: the copy now
        // succeeds and both streams tear down cleanly.
        src.zalloc = saved_zalloc;
        src.zfree = saved_zfree;
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());
        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
    }

    /// A `source` whose `zalloc` was cleared after initialization must be
    /// rejected with `Z_STREAM_ERROR`, leaving `dest` untouched.
    #[test]
    fn copy_rejects_a_source_whose_zalloc_was_cleared_after_init() {
        copy_rejects_half_present_source(ClearedHalf::Zalloc);
    }

    /// The `zfree` half is checked too: C's `deflateStateCheck` tests both, so a
    /// guard that only looked at `zalloc` would still diverge.
    #[test]
    fn copy_rejects_a_source_whose_zfree_was_cleared_after_init() {
        copy_rejects_half_present_source(ClearedHalf::Zfree);
    }

    /// A caller who supplied **neither** half is still a valid copy source.
    ///
    /// C's `deflateStateCheck` rejects only a *null* half, and this crate's
    /// `init_allocator_prologue` has already published its built-in substitutes
    /// into the caller's `z_stream` (mirroring `deflate.c` L400-L414), so both
    /// halves are non-null after any successful init. The new guard must
    /// therefore never fire for a hookless caller — the historical
    /// global-allocator path stays exactly as it was.
    #[test]
    fn copy_succeeds_for_a_hookless_source() {
        let mut src = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        assert!(
            src.zalloc.is_some() && src.zfree.is_some(),
            "init publishes both built-in substitutes (deflate.c L400-L414)"
        );

        let mut dst = zeroed_stream();
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());
        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
    }

    /// The clone is charged to the pair `source` publishes **at the time of the
    /// copy**, which is what C does by `zmemcpy`ing the whole `z_stream` into
    /// `dest` and then allocating through `ZALLOC(dest, …)` (`deflate.c`
    /// L1332-L1341). Swapping in a *different* live arena after init — both
    /// halves present, so the guard does not fire — must therefore make the
    /// clone draw from the new arena, and exhausting that new arena must surface
    /// `Z_MEM_ERROR` even though the arena captured at init still has budget.
    #[test]
    fn copy_charges_the_clone_to_the_sources_current_allocator_pair() {
        let init_arena = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(64),
        };
        let empty_arena = Budget {
            remaining: core::sync::atomic::AtomicUsize::new(0),
        };

        let mut src = zeroed_stream();
        attach_budget(&mut src, &init_arena);
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let init_remaining = init_arena
            .remaining
            .load(core::sync::atomic::Ordering::SeqCst);
        assert!(
            init_remaining > 0,
            "the init arena must retain budget so the assertion below is meaningful"
        );

        // Republish an exhausted arena on the source. Both halves stay non-null,
        // so this is not the half-present case — it is the "which pair pays"
        // question, and C's answer is the pair `source` currently publishes.
        attach_budget(&mut src, &empty_arena);

        let mut dst = zeroed_stream();
        attach_budget(&mut dst, &empty_arena);
        assert_eq!(
            unsafe { deflateCopy(&mut dst, &mut src) },
            Z_MEM_ERROR,
            "the clone must draw from the arena the source publishes now, not the \
             one captured at init"
        );
        assert!(dst.state.is_null());
        assert_eq!(
            init_arena
                .remaining
                .load(core::sync::atomic::Ordering::SeqCst),
            init_remaining,
            "the init arena must not have funded the clone"
        );

        // Hand the source's own arena back so teardown releases the init-time
        // buffers through the allocator that produced them.
        attach_budget(&mut src, &init_arena);
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

    /// `deflateSetHeader`'s observable return set is **exactly** `zlib.h`'s two
    /// codes — `Z_OK` and `Z_STREAM_ERROR` — across every reachable stream shape.
    ///
    /// `zlib.h` L854-L855 documents only those two, and `deflate.c` L714-L719
    /// produces only those two because it allocates nothing. This shim allocates
    /// nothing either — it records the caller's `gz_header` pointer and the engine
    /// re-reads every field lazily, when the header is sized or emitted — so there
    /// is no allocation failure here to fold into a third code, and `Z_MEM_ERROR`
    /// (which reference zlib cannot produce from this function) can never appear.
    /// This test is the standing guard on that contract: it sweeps the reachable
    /// argument space and fails if any configuration ever answers a third code.
    #[test]
    fn set_header_return_set_is_exactly_ok_or_stream_error() {
        // Assert a single observed code against the whole permitted set.
        fn assert_permitted(rc: c_int, case: &str) {
            assert!(
                rc == Z_OK || rc == Z_STREAM_ERROR,
                "deflateSetHeader({case}) returned {rc}, which is outside the \
                 zlib.h L854-L855 contract {{Z_OK, Z_STREAM_ERROR}}"
            );
            assert_ne!(
                rc, Z_MEM_ERROR,
                "deflateSetHeader({case}) returned Z_MEM_ERROR, a code reference \
                 zlib cannot produce from this function"
            );
        }

        // 1. A null stream pointer.
        assert_permitted(
            unsafe { deflateSetHeader(ptr::null_mut(), ptr::null_mut()) },
            "null strm",
        );

        // 2. A zeroed, never-initialized stream: no installed engine state.
        let mut raw = zeroed_stream();
        assert_permitted(
            unsafe { deflateSetHeader(&mut raw, ptr::null_mut()) },
            "uninitialized strm",
        );

        // 3. Every `windowBits` framing zlib offers, with and without a
        //    populated header. Only the gzip framing (`wrap == 2`) may answer
        //    `Z_OK`; the rest must answer `Z_STREAM_ERROR`. Either way the code
        //    has to be one of the two.
        for window_bits in [15, -15, 31, 9, -9] {
            let mut strm = zeroed_stream();
            let init_rc = unsafe {
                deflateInit2_(
                    &mut strm,
                    6,
                    Z_DEFLATED,
                    window_bits,
                    DEF_MEM_LEVEL,
                    Z_DEFAULT_STRATEGY,
                    ver(),
                    size_of::<z_stream>() as c_int,
                )
            };
            if init_rc != Z_OK {
                // A framing this build cannot construct cannot be exercised
                // further. The only such case is the gzip framing in a build
                // without the `gzip` feature, where no stream can ever reach
                // `wrap == 2` at all.
                assert_eq!(
                    window_bits, 31,
                    "only the gzip framing may be unconstructible; windowBits=\
                     {window_bits} returned {init_rc}"
                );
                continue;
            }

            let null_rc = unsafe { deflateSetHeader(&mut strm, ptr::null_mut()) };
            assert_permitted(
                null_rc,
                &std::format!("windowBits={window_bits}, NULL head"),
            );

            let mut extra = std::vec![0x11u8, 0x22, 0x33];
            let mut name = std::vec![b'n', b'a', b'm', b'e', 0];
            let mut comment = std::vec![b'c', 0];
            let mut head = crate::ffi::types::gz_header {
                text: 0,
                time: 0,
                xflags: 0,
                os: 3,
                extra: extra.as_mut_ptr(),
                extra_len: extra.len() as c_uint,
                extra_max: 0,
                name: name.as_mut_ptr(),
                name_max: 0,
                comment: comment.as_mut_ptr(),
                comm_max: 0,
                hcrc: 0,
                done: 0,
            };
            // SAFETY: `strm` is a live deflate stream and every field-declared
            // buffer above outlives this call.
            let full_rc = unsafe { deflateSetHeader(&mut strm, &mut head) };
            assert_permitted(
                full_rc,
                &std::format!("windowBits={window_bits}, populated head"),
            );
            // The gzip framing is the only one C accepts, and both header shapes
            // must agree with each other on that verdict.
            assert_eq!(
                null_rc, full_rc,
                "windowBits={window_bits}: a NULL and a populated header must \
                 reach the same verdict"
            );
            assert_eq!(
                full_rc,
                if window_bits == 31 && cfg!(feature = "gzip") {
                    Z_OK
                } else {
                    Z_STREAM_ERROR
                },
                "windowBits={window_bits}: only the gzip framing sets a header"
            );

            assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        }
    }
    /// `deflateSetHeader` with a **populated** header must return `Z_OK` and every
    /// caller-owned field must reach the wire, byte for byte.
    ///
    /// This pins the *success* path: every other `deflateSetHeader` test passes
    /// `NULL`, so without this one the whole header ladder — flags, MTIME, OS,
    /// XLEN, extra, name, comment — could stop being emitted with nothing to catch
    /// it.
    ///
    /// The header is read from the caller's own storage at emission time, as in C
    /// (`deflate.c` L714-L719 stores only the pointer; L1092-L1188 read the
    /// fields). Accordingly the source buffers here stay alive and unmodified
    /// across the `deflate` call, and are only scribbled over and dropped
    /// afterwards — the obligation `zlib.h` L843-L847 places on a C caller. The
    /// scribble is still meaningful: it proves the emitted bytes were produced
    /// during the call rather than lazily afterwards.
    #[cfg(feature = "gzip")]
    #[test]
    fn set_header_emits_every_populated_field_from_the_callers_storage() {
        let mut strm = zeroed_stream();
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

        let compressed = {
            // Scoped so every caller-owned buffer is dropped before `deflate`.
            let mut extra = std::vec![0xA5u8, 0x5A, 0x11, 0x22];
            let mut name = std::vec![
                b'p', b'a', b'y', b'l', b'o', b'a', b'd', b'.', b'b', b'i', b'n', 0
            ];
            let mut comment = std::vec![b'a', b' ', b'c', b'o', b'm', b'm', b'e', b'n', b't', 0];

            let mut head = crate::ffi::types::gz_header {
                text: 1,
                time: 0x5EED_1234,
                xflags: 0,
                os: 3,
                extra: extra.as_mut_ptr(),
                extra_len: extra.len() as c_uint,
                extra_max: 0,
                name: name.as_mut_ptr(),
                name_max: 0,
                comment: comment.as_mut_ptr(),
                comm_max: 0,
                hcrc: 0,
                done: 0,
            };

            // SAFETY: `strm` is a live gzip-wrapped deflate stream and `head`'s
            // buffers are valid for the duration of the call.
            assert_eq!(
                unsafe { deflateSetHeader(&mut strm, &mut head) },
                Z_OK,
                "a populated header on a gzip stream must be accepted"
            );

            let input = b"borrowed gzip header fields".repeat(4);
            let mut out = std::vec![0u8; 512];
            strm.next_in = input.as_ptr();
            strm.avail_in = input.len() as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            // SAFETY: both cursors point at live buffers sized by the fields set
            // above.
            assert_eq!(unsafe { deflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
            let produced = out.len() - strm.avail_out as usize;
            out.truncate(produced);

            // The header has been emitted; scribbling over the source buffers and
            // dropping them now proves the bytes were produced during the
            // `deflate` call above and not read again afterwards.
            extra.fill(0);
            name.fill(0);
            comment.fill(0);
            out
        };

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        // The emitted member must carry FEXTRA|FNAME|FCOMMENT and the exact bytes.
        assert_eq!(&compressed[..2], &[0x1f, 0x8b], "gzip magic");
        let flg = compressed[3];
        assert_ne!(flg & 0x04, 0, "FEXTRA must be advertised");
        assert_ne!(flg & 0x08, 0, "FNAME must be advertised");
        assert_ne!(flg & 0x10, 0, "FCOMMENT must be advertised");
        assert_ne!(flg & 0x01, 0, "FTEXT must be advertised for text: 1");

        // Header layout: 10 fixed bytes, then XLEN (2, little-endian) + extra.
        assert_eq!(&compressed[4..8], &0x5EED_1234u32.to_le_bytes(), "MTIME");
        assert_eq!(compressed[9], 3, "OS byte is the caller's `os`");
        assert_eq!(&compressed[10..12], &4u16.to_le_bytes(), "XLEN");
        assert_eq!(
            &compressed[12..16],
            &[0xA5, 0x5A, 0x11, 0x22],
            "extra bytes"
        );
        let rest = &compressed[16..];
        let name_end = rest
            .iter()
            .position(|&b| b == 0)
            .expect("NUL-terminated name");
        assert_eq!(&rest[..name_end], b"payload.bin", "FNAME");
        let after_name = &rest[name_end + 1..];
        let cmt_end = after_name
            .iter()
            .position(|&b| b == 0)
            .expect("NUL-terminated comment");
        assert_eq!(&after_name[..cmt_end], b"a comment", "FCOMMENT");
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

    /// End-to-end out-of-memory propagation through **initialization**: when the
    /// caller's arena is exhausted on the last of the five regions
    /// `deflateInit2_` requests, the C entry point must report `Z_MEM_ERROR`,
    /// hand every region it did manage to allocate back to the caller's `zfree`,
    /// and leave nothing outstanding — never silently completing the init on the
    /// Rust global allocator. A caller arena that runs out on the *last* working
    /// buffer sees its regions handed back in C's documented teardown order, not
    /// in Rust field order.
    ///
    /// The test drives `deflateInit_`, which forwards to `deflateInit2_` with the
    /// documented defaults (`deflate.c` L379-L384). That path charges the arena
    /// five times: the state (`deflate.c` L440), then `window`, `prev` and `head`
    /// issued unconditionally (`deflate.c` L458-L460), then `pending_buf`
    /// (`deflate.c` L505). C checks all four working buffers **together** rather
    /// than short-circuiting after the first failure (`deflate.c` L508-L509),
    /// and on failure sets `FINISH_STATE`, sets `msg`, and calls `deflateEnd`
    /// before returning `Z_MEM_ERROR` (`deflate.c` L510-L513). `deflateEnd` frees
    /// `pending_buf`, `head`, `prev`, `window` and finally the state —
    /// "deallocate in reverse order of allocations" (`deflate.c` L1300-L1306).
    /// Request numbers make that `[3, 2, 1, 0]`, the state's `0`th region last.
    ///
    /// This is a real divergence this port had: dropping the buffers as a tuple
    /// pattern released them left-to-right (`window, prev, head, pending_buf`,
    /// i.e. `[1, 2, 3, 0]`), which a caller arena that coalesces or asserts on
    /// release order can observe. Measured against a reference C zlib built from
    /// the in-tree sources before being fixed.
    #[test]
    fn init_releases_working_buffers_in_c_reverse_order_when_the_arena_runs_out() {
        use crate::ffi::alloc::test_hook::HookStats;

        // Four successes — state, `window`, `prev`, `head` — then refusal, so the
        // fifth request (`pending_buf`) is the one that fails.
        let stats = HookStats::with_budget(4);
        let hook = stats.hook();

        let mut strm = zeroed_stream();
        strm.zalloc = hook.zalloc();
        strm.zfree = hook.zfree();
        strm.opaque = hook.opaque();

        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            ReturnCode::MemError.as_c_int(),
            "the fifth request is refused, so init must report Z_MEM_ERROR"
        );

        assert_eq!(
            stats.allocs(),
            4,
            "C asks for the state and all four working buffers; four are served"
        );
        assert_eq!(
            stats.ooms(),
            1,
            "exactly the `pending_buf` request is refused; C does not short-circuit \
             the earlier ones (`deflate.c` L507-L513 checks them together)"
        );
        assert_eq!(
            stats.free_order(),
            std::vec![3, 2, 1, 0],
            "C's `deflateEnd` releases `pending_buf` (refused, so absent), then \
             `head`, `prev`, `window`, and the state region last — request numbers \
             [3, 2, 1, 0] (`deflate.c` L1300-L1306)"
        );
        assert_eq!(
            stats.live_bytes(),
            0,
            "a failed init must leave nothing outstanding in the caller's arena"
        );
    }

    /// `deflateCopy` publishes the source `z_stream` into `dest` **before** it
    /// makes its first allocation, so even a copy refused on its very first
    /// request leaves `dest` carrying the source's bookkeeping.
    ///
    /// C's `zmemcpy(dest, source, sizeof(z_stream))` is at `deflate.c` L1333 and
    /// the destination `ZALLOC` at L1335 — the mirror strictly precedes the
    /// charge. This port originally mirrored *after* installing the handle, so a
    /// refused copy left `dest` untouched; that divergence was measured against a
    /// reference C zlib built from the in-tree sources.
    ///
    /// Budgeting the arena to zero for the copy makes the ordering the *only*
    /// thing under test: nothing is allocated at all, so any mirrored field
    /// observed in `dest` can only have been written before the first request.
    #[test]
    fn copy_mirrors_the_stream_before_its_first_allocation() {
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

        // Give the source history so the mirrored counters are non-zero and the
        // assertions below cannot pass against a still-zeroed `dest`.
        let input = b"mirror before allocate ".repeat(16);
        let mut out = std::vec![0u8; 4096];
        src.next_in = input.as_ptr();
        src.avail_in = input.len() as c_uint;
        src.next_out = out.as_mut_ptr();
        src.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut src, Z_NO_FLUSH) }, Z_OK);
        assert!(
            src.total_in > 0,
            "the source must have consumed input for this test to be meaningful"
        );

        let served = stats.allocs();
        stats.set_budget(0);

        let mut dst = zeroed_stream();
        assert_eq!(
            unsafe { deflateCopy(&mut dst, &mut src) },
            ReturnCode::MemError.as_c_int()
        );
        assert_eq!(
            stats.allocs(),
            served,
            "the copy must not have been served a single region"
        );
        assert_eq!(stats.ooms(), 1, "exactly the first copy request is refused");

        // Every application-visible field is already present in `dest`.
        assert_eq!(dst.total_in, src.total_in);
        assert_eq!(dst.total_out, src.total_out);
        assert_eq!(dst.next_in, src.next_in);
        assert_eq!(dst.avail_in, src.avail_in);
        assert_eq!(dst.next_out, src.next_out);
        assert_eq!(dst.avail_out, src.avail_out);
        assert_eq!(dst.adler, src.adler);
        assert_eq!(dst.data_type, src.data_type);
        assert!(dst.zalloc.is_some() && dst.zfree.is_some());

        // The source is mid-stream (`BUSY_STATE`) because `deflate(Z_NO_FLUSH)`
        // left output pending, and C's `deflateEnd` reports that as
        // `Z_DATA_ERROR` while still releasing every region:
        // `return status == BUSY_STATE ? Z_DATA_ERROR : Z_OK;` (`deflate.c`
        // `deflateEnd`). The teardown itself is unconditional, which the balance
        // assertion below confirms.
        assert_eq!(
            unsafe { deflateEnd(&mut src) },
            ReturnCode::DataError.as_c_int(),
            "tearing down a stream that still has pending output is Z_DATA_ERROR"
        );
        assert_eq!(stats.live_bytes(), 0);
    }

    /// The caller's `zalloc` is the real home of the engine state, not merely the
    /// payer for a same-sized reservation, and the whole schedule matches C.
    ///
    /// This is the C-ABI end-to-end statement of the hook-backed-ownership
    /// requirement (AAP §0.6.5). It pins four things at once:
    ///
    /// * the **count** — five requests for `deflateInit2_`, exactly C's
    ///   `ZALLOC(1, sizeof(deflate_state))` plus `window`, `prev`, `head` and the
    ///   single overlaid `pending_buf` (`deflate.c` L440, L458-L460, L505);
    /// * the **residency** — outstanding bytes after init equal the state's own
    ///   footprint *plus* the working buffers, which is only possible if the state
    ///   value itself lives in the arena. A reservation charged and then abandoned
    ///   would show the same byte count, so the companion assertions in
    ///   `crate::ffi::types` additionally check the buffer and state handles report
    ///   foreign backing;
    /// * the **release order** — `[4, 3, 2, 1, 0]`, C's reverse-of-allocation
    ///   teardown with the state last (`deflate.c` L1300-L1306);
    /// * the **balance** — zero outstanding bytes afterwards, so nothing leaked
    ///   into the arena and nothing was released twice.
    #[test]
    fn the_caller_arena_owns_the_state_and_sees_c_s_whole_schedule() {
        use crate::deflate::state::DeflateState;
        use crate::ffi::alloc::test_hook::HookStats;

        let stats = HookStats::new();
        let hook = stats.hook();

        let mut strm = zeroed_stream();
        strm.zalloc = hook.zalloc();
        strm.zfree = hook.zfree();
        strm.opaque = hook.opaque();

        assert_eq!(
            unsafe {
                deflateInit2_(
                    &mut strm,
                    6,
                    Z_DEFLATED,
                    15,
                    8,
                    Z_DEFAULT_STRATEGY,
                    ver(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        assert_eq!(
            stats.allocs(),
            5,
            "C's five `ZALLOC`s, no more and no fewer"
        );
        assert_eq!(stats.ooms(), 0);
        assert_eq!(stats.frees(), 0, "a successful init releases nothing");

        // `windowBits = 15`, `memLevel = 8` gives `w_size = 32768` and
        // `hash_size = 32768`, so C's four working buffers are
        // `window` 2*32768, `prev` 2*32768, `head` 2*32768 and `pending_buf`
        // `lit_bufsize * LIT_BUFS` = 16384 * 4 — 262144 bytes in total, a figure
        // measured to be identical in reference C.
        const WORKING: usize = 262_144;
        assert_eq!(
            stats.live_bytes(),
            size_of::<DeflateState>() + WORKING,
            "the arena must hold the state value itself as well as every working \
             buffer; a hook charged only for the buffers, or charged for the state \
             and then handed a global `Box` instead, would not add up"
        );

        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            stats.free_order(),
            std::vec![4, 3, 2, 1, 0],
            "`deflateEnd` releases `pending_buf`, `head`, `prev`, `window`, state"
        );
        assert_eq!(stats.frees(), 5, "every region is handed back exactly once");
        assert_eq!(stats.live_bytes(), 0, "the arena is balanced");
    }

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

        // `dest` carries the mirrored `z_stream` but no state, which is precisely
        // what reference zlib leaves behind: its `zmemcpy(dest, source,
        // sizeof(z_stream))` runs at `deflate.c` L1333, *before* the destination
        // `ZALLOC` at L1335, so a refused copy still publishes the source's
        // cursors, totals, `msg`, allocator triple, `data_type` and `adler`. This
        // was measured against a reference C zlib built from the in-tree sources,
        // not inferred.
        assert_eq!(
            dst.total_in, src.total_in,
            "C mirrors total_in before it allocates"
        );
        assert_eq!(
            dst.total_out, src.total_out,
            "C mirrors total_out before it allocates"
        );
        assert_eq!(
            dst.next_in, src.next_in,
            "C mirrors next_in before it allocates"
        );
        assert_eq!(
            dst.avail_in, src.avail_in,
            "C mirrors avail_in before it allocates"
        );
        assert_eq!(dst.adler, src.adler, "C mirrors adler before it allocates");
        assert_eq!(
            dst.data_type, src.data_type,
            "C mirrors data_type before it allocates"
        );
        assert!(
            dst.zalloc.is_some() && dst.zfree.is_some(),
            "C mirrors the allocator triple before it allocates, so a refused copy \
             leaves dest carrying the source's hook"
        );

        // The one field C's struct copy carries that this port deliberately does
        // not is the opaque `state` pointer: C leaves `dest->state ==
        // source->state` on this path. `zlib.h` L100 declares that field "not
        // visible by applications"; C's own `deflateStateCheck` rejects the alias
        // through `s->strm != strm` exactly as this port's owner-bound handle
        // header does, so `deflateEnd(dest)` answers `Z_STREAM_ERROR` in both; and
        // publishing a live pointer to a state this stream does not own would
        // defeat that owner binding. So the alias is not reproduced, and the
        // contract-visible outcome is asserted instead.
        assert!(
            dst.state.is_null(),
            "a failed copy must not install state into dest"
        );
        assert_eq!(
            unsafe { deflateEnd(&mut dst) },
            Z_STREAM_ERROR,
            "dest holds no state it owns, so its teardown is refused — the same answer \
             reference C gives for its aliased pointer"
        );

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
             them together (`deflate.c` L1342-L1350), so the three that cannot be \
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

    // =======================================================================
    // Owner-bound handles (S7-02) and validation-before-borrow (S7-01)
    // =======================================================================

    /// A byte-copied `z_stream` must not be able to drive or reclaim the
    /// original's state.
    ///
    /// This is C's `s->strm != strm` clause (`deflate.c` L544), and it is the
    /// clause that makes reclaim sound. A caller who writes `z_stream copy = strm;`
    /// holds a second 14-field struct whose `state` names the SAME handle.
    /// Reference zlib refuses every call through the copy — measured against a
    /// reference build from this repository's own C sources: `deflateEnd(&copy)`
    /// returns `-2` and the subsequent `deflateEnd(&strm)` returns `0`.
    ///
    /// Without the owner clause the Rust port inverted that: the copy reclaimed
    /// and freed the box, and the original was then left reading freed memory —
    /// a genuine use-after-free reachable from safe C usage. This test pins the
    /// fix, and asserts the *order* too: the original must still be able to
    /// finish its work afterwards.
    #[test]
    fn a_byte_copied_z_stream_can_neither_drive_nor_reclaim_the_originals_state() {
        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut strm, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        assert!(!strm.state.is_null());

        // `z_stream copy = strm;` — a plain struct copy, exactly what C does.
        // SAFETY: `z_stream` is a `#[repr(C)]` aggregate of `Copy` scalars and raw
        // pointers, so a bitwise read is a valid duplicate. Nothing is dropped:
        // `z_stream` owns nothing and has no `Drop` impl.
        let mut copy = unsafe { core::ptr::read(&raw const strm) };
        assert!(
            core::ptr::eq(copy.state, strm.state),
            "the copy must name the same handle — that is the hazard being tested"
        );

        // Every stateful entry point must refuse the copy.
        let mut out = [0u8; 64];
        copy.next_out = out.as_mut_ptr();
        copy.avail_out = out.len() as c_uint;
        assert_eq!(
            unsafe { deflate(&mut copy, Z_FINISH) },
            Z_STREAM_ERROR,
            "C's deflateStateCheck rejects a stream that does not own its state"
        );
        assert_eq!(unsafe { deflateReset(&mut copy) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { deflateParams(&mut copy, 1, 0) }, Z_STREAM_ERROR);

        // The reclaim path is the one that would otherwise free the allocation
        // through a stream that does not own it.
        assert_eq!(
            unsafe { deflateEnd(&mut copy) },
            Z_STREAM_ERROR,
            "a wrong-owner deflateEnd must refuse, matching reference C's -2"
        );
        assert!(
            !copy.state.is_null(),
            "a refused reclaim must leave the handle installed for its real owner"
        );

        // The true owner is untouched and still fully functional.
        let source = b"owner-bound handles keep the original stream usable";
        let mut dest = [0u8; 256];
        strm.next_in = source.as_ptr();
        strm.avail_in = source.len() as c_uint;
        strm.next_out = dest.as_mut_ptr();
        strm.avail_out = dest.len() as c_uint;
        assert_eq!(
            unsafe { deflate(&mut strm, Z_FINISH) },
            crate::error::ReturnCode::StreamEnd.as_c_int(),
            "the owning stream must still compress normally"
        );
        let produced = dest.len() - strm.avail_out as usize;
        assert_eq!(
            unsafe { deflateEnd(&mut strm) },
            Z_OK,
            "the owner reclaims exactly once, matching reference C's 0"
        );
        assert!(strm.state.is_null());
        assert_eq!(
            zlib_inflate(&dest[..produced]),
            source,
            "the stream the copy could not touch produced a valid zlib member"
        );
    }

    /// `deflateCopy` re-points the clone's owner at `dest`, so each stream
    /// reclaims its own state and neither can free the other's.
    ///
    /// C does this explicitly: `ds->strm = dest;` (`deflate.c` L1340) immediately
    /// after the `zmemcpy` that would otherwise have left the clone claiming the
    /// source as its owner. Getting it wrong is not a cosmetic bug — the clone
    /// would be unusable through `dest` and, worse, `deflateEnd(&source)` would
    /// be the only way to free it while `source` still owns its own handle.
    #[test]
    fn copy_binds_the_clone_to_dest_so_each_stream_reclaims_its_own_state() {
        let mut src = zeroed_stream();
        assert_eq!(
            unsafe { deflateInit_(&mut src, 6, ver(), size_of::<z_stream>() as c_int) },
            Z_OK
        );
        let mut dst = zeroed_stream();
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());
        assert!(
            !core::ptr::eq(dst.state, src.state),
            "deflateCopy must install a distinct handle, not share the source's"
        );

        // Both streams are independently drivable, which is only true if the
        // clone's owner really is `dst`.
        for (label, strm) in [("clone", &mut dst), ("source", &mut src)] {
            let source = b"each stream owns its own handle";
            let mut out = [0u8; 256];
            strm.next_in = source.as_ptr();
            strm.avail_in = source.len() as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            assert_eq!(
                unsafe { deflate(strm, Z_FINISH) },
                crate::error::ReturnCode::StreamEnd.as_c_int(),
                "the {label} must compress through its own owner-bound handle"
            );
            let produced = out.len() - strm.avail_out as usize;
            assert_eq!(zlib_inflate(&out[..produced]), source, "{label} output");
        }

        // Each reclaims exactly once, in either order.
        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        assert!(dst.state.is_null());
        assert_eq!(
            unsafe { deflateEnd(&mut src) },
            Z_OK,
            "ending the clone must not have disturbed the source's handle"
        );
        assert!(src.state.is_null());
    }

    /// A stateless stream is refused by every auxiliary-pointer entry point
    /// *without* the auxiliary pointer being bridged (S7-01).
    ///
    /// C reaches its verdict from `deflateStateCheck(strm)` alone and never reads
    /// the `dictionary`, `head`, `next_in` or `next_out` it was handed
    /// (`deflate.c` L567-L568, L981-L1010). The pointers below are deliberately
    /// non-null and deliberately not backed by the sizes claimed, so any shim that
    /// bridged them before validating would be constructing a slice or reference
    /// over memory it has no right to. The structural companion to this test —
    /// `state_validation_precedes_every_auxiliary_pointer_access` — is what
    /// detects a regression in the *order*; this one pins the observable contract.
    #[test]
    fn a_stateless_stream_is_refused_without_its_auxiliary_pointers_being_used() {
        // One real byte, but every call below claims far more than one.
        let probe = [0xA5u8; 1];
        let probe_ptr = probe.as_ptr();

        let mut strm = zeroed_stream();
        assert!(strm.state.is_null());

        assert_eq!(
            unsafe { deflateSetDictionary(&mut strm, probe_ptr, 4096) },
            Z_STREAM_ERROR,
            "C validates the stream before it reads one dictionary byte"
        );

        strm.next_in = probe_ptr;
        strm.avail_in = 4096;
        strm.next_out = probe.as_ptr().cast_mut();
        strm.avail_out = 4096;
        assert_eq!(
            unsafe { deflate(&mut strm, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "C validates the stream before it bridges next_in/next_out"
        );
        assert_eq!(
            unsafe { deflateEnd(&mut strm) },
            Z_STREAM_ERROR,
            "there is nothing to reclaim"
        );

        // `deflateCopy` from a stateless source must not touch `dest` at all.
        let mut dst = zeroed_stream();
        dst.total_in = 0x5EED;
        assert_eq!(
            unsafe { deflateCopy(&mut dst, &mut strm) },
            Z_STREAM_ERROR,
            "C evaluates deflateStateCheck(source) before it looks at dest"
        );
        assert_eq!(
            dst.total_in, 0x5EED,
            "a refused deflateCopy must leave dest byte-for-byte untouched"
        );
        assert!(dst.state.is_null());
    }
    // -----------------------------------------------------------------------
    // Live borrowed gzip header (`deflateSetHeader`)
    //
    // C stores the caller's `gz_header` *pointer* and copies nothing
    // (`deflate.c` L714-L719), re-reading every field lazily when the header is
    // finally emitted (`deflate.c` L893-L907 for `deflateBound`, L1092-L1188 for
    // the emission ladder). Anything the caller changes between registration and
    // emission therefore lands in the compressed bytes. A shim that deep-copied
    // the header at registration produced *different gzip bytes* for a
    // legitimate call sequence, and could additionally fail with `Z_MEM_ERROR`
    // on a call C documents as returning only `Z_OK`/`Z_STREAM_ERROR`
    // (`zlib.h` L854-L855).
    //
    // The header is driven through a raw pointer into a leaked `Box` throughout,
    // rather than through a Rust local: that is how a C caller reaches it, it
    // keeps one provenance for the whole registration lifetime, and it means the
    // post-registration mutations are genuine writes the optimizer cannot
    // discard.
    // -----------------------------------------------------------------------

    /// Initializes `strm` **in place** for gzip framing (`windowBits = 31`).
    ///
    /// In place is mandatory: `deflateStateCheck` compares `s->strm` against the
    /// stream address (`deflate.c` L540-L541), so a stream initialized in one
    /// location and then moved is refused — by C and by this crate alike.
    #[cfg(feature = "gzip")]
    fn init_gzip(strm: &mut z_stream) {
        let rc = unsafe {
            deflateInit2_(
                strm,
                6,
                Z_DEFLATED,
                31,
                8,
                Z_DEFAULT_STRATEGY,
                ver(),
                size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(rc, Z_OK, "gzip deflateInit2_ must succeed");
    }

    /// A zeroed `gz_header` on the heap, reached only through the returned raw
    /// pointer — the shape a C caller presents.
    #[cfg(feature = "gzip")]
    fn raw_header() -> *mut gz_header {
        // SAFETY: `gz_header` is a `#[repr(C)]` aggregate of raw pointers and
        // integers, for which the all-zero bit pattern is the valid "no fields"
        // state a C caller produces with `memset`.
        Box::into_raw(Box::new(unsafe { core::mem::zeroed::<gz_header>() }))
    }

    /// Releases a header obtained from [`raw_header`].
    #[cfg(feature = "gzip")]
    fn free_header(head: *mut gz_header) {
        // SAFETY: `head` came from `Box::into_raw` in `raw_header` and is
        // reclaimed exactly once.
        drop(unsafe { Box::from_raw(head) });
    }

    /// Compresses `input` to completion, returning the emitted bytes.
    #[cfg(feature = "gzip")]
    fn finish(strm: &mut z_stream, input: &[u8]) -> Vec<u8> {
        let mut out = std::vec![0u8; 4096];
        strm.next_in = input.as_ptr().cast_mut();
        strm.avail_in = input.len() as uInt;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as uInt;
        let rc = unsafe { deflate(strm, Z_FINISH) };
        assert_eq!(rc, Z_STREAM_END, "the whole payload must fit");
        let produced = out.len() - strm.avail_out as usize;
        out.truncate(produced);
        out
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn set_header_emits_the_callers_live_fields_not_a_registration_snapshot() {
        let payload = b"live gzip header payload".repeat(4);

        // Two full field sets. The first is registered; the second replaces it
        // *after* registration but *before* the header is emitted.
        let extra_a: [c_uchar; 4] = [1, 2, 3, 4];
        let extra_b: [c_uchar; 6] = [9, 8, 7, 6, 5, 4];
        let name_a = c"aaa";
        let name_b = c"bbbbbb";
        let comm_a = c"ccc";
        let comm_b = c"dddddd";

        let mut strm = zeroed_stream();
        init_gzip(&mut strm);
        let head = raw_header();
        // SAFETY: `head` is a live, uniquely-owned `gz_header`.
        unsafe {
            (*head).time = 0x1111_1111;
            (*head).os = 3;
            (*head).extra = extra_a.as_ptr().cast_mut();
            (*head).extra_len = extra_a.len() as uInt;
            (*head).name = name_a.as_ptr().cast::<c_uchar>().cast_mut();
            (*head).comment = comm_a.as_ptr().cast::<c_uchar>().cast_mut();
        }

        assert_eq!(unsafe { deflateSetHeader(&mut strm, head) }, Z_OK);
        let bound_before = unsafe { deflateBound(&mut strm, payload.len() as uLong) };

        // Mutate every field the emitter reads.
        // SAFETY: as above; the registration holds only the pointer.
        unsafe {
            (*head).text = 1;
            (*head).time = 0x2222_2222;
            (*head).os = 7;
            (*head).extra = extra_b.as_ptr().cast_mut();
            (*head).extra_len = extra_b.len() as uInt;
            (*head).name = name_b.as_ptr().cast::<c_uchar>().cast_mut();
            (*head).comment = comm_b.as_ptr().cast::<c_uchar>().cast_mut();
            (*head).hcrc = 1;
        }

        // `deflateBound` reads `gzhead` too (`deflate.c` L893-L907), so the
        // larger field set must enlarge the bound: +2 extra bytes, +3 name,
        // +3 comment, +2 for the header CRC.
        let bound_after = unsafe { deflateBound(&mut strm, payload.len() as uLong) };
        assert_eq!(
            bound_after - bound_before,
            10,
            "deflateBound must size the live header, not the registered snapshot"
        );

        let gz = finish(&mut strm, &payload);
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        free_header(head);

        // FLG: FTEXT|FHCRC|FEXTRA|FNAME|FCOMMENT, all five from the *new* values.
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 0x08], "gzip magic and method");
        assert_eq!(gz[3], 0x1f, "FLG must reflect the mutated text/hcrc bits");
        assert_eq!(&gz[4..8], &[0x22, 0x22, 0x22, 0x22], "mutated MTIME");
        assert_eq!(gz[9], 7, "mutated OS");
        assert_eq!(&gz[10..12], &[6, 0], "XLEN of the mutated extra field");
        assert_eq!(&gz[12..18], &extra_b, "mutated extra bytes");
        assert_eq!(&gz[18..25], b"bbbbbb\0", "mutated NAME");
        assert_eq!(&gz[25..32], b"dddddd\0", "mutated COMMENT");
        // None of the registration-time values may appear anywhere.
        assert!(
            !gz.windows(4).any(|w| w == [0x11, 0x11, 0x11, 0x11]),
            "no field from the registration snapshot may be emitted"
        );
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn set_header_never_reports_a_memory_error() {
        // C's `deflateSetHeader` copies nothing and so returns only `Z_OK` or
        // `Z_STREAM_ERROR` (`zlib.h` L854-L855). Header sizes that would dwarf
        // any registration-time copy must therefore still succeed, repeatedly.
        let mut big_name = std::vec![b'x'; 4096];
        *big_name.last_mut().expect("non-empty") = 0;
        let big_extra = std::vec![0x5au8; 8192];

        let mut strm = zeroed_stream();
        init_gzip(&mut strm);
        let head = raw_header();
        // SAFETY: `head` is a live, uniquely-owned `gz_header`.
        unsafe {
            (*head).name = big_name.as_ptr().cast_mut();
            (*head).comment = big_name.as_ptr().cast_mut();
            (*head).extra = big_extra.as_ptr().cast_mut();
            (*head).extra_len = big_extra.len() as uInt;
        }
        for _ in 0..64 {
            assert_eq!(
                unsafe { deflateSetHeader(&mut strm, head) },
                Z_OK,
                "registration is a pointer store; it cannot run out of memory"
            );
        }
        // A null header clears the registration, as C's plain assignment does.
        assert_eq!(
            unsafe { deflateSetHeader(&mut strm, ptr::null_mut()) },
            Z_OK
        );
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);

        // `wrap != 2` is still refused, and refusing must not disturb anything.
        let mut raw = zeroed_stream();
        assert_eq!(
            unsafe {
                deflateInit2_(
                    &mut raw,
                    6,
                    Z_DEFLATED,
                    -15,
                    8,
                    Z_DEFAULT_STRATEGY,
                    ver(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        assert_eq!(
            unsafe { deflateSetHeader(&mut raw, head) },
            Z_STREAM_ERROR,
            "C rejects deflateSetHeader on a non-gzip stream (deflate.c L715-L716)"
        );
        assert_eq!(unsafe { deflateEnd(&mut raw) }, Z_OK);
        free_header(head);
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn reset_keeps_the_registered_header_exactly_as_c_does() {
        // C clears `s->gzhead` only in `deflateInit2_` (`deflate.c` L448); no
        // reset path touches it, so the same header is emitted again after a
        // `deflateReset`.
        let payload = b"reset keeps the header".repeat(3);
        let name = c"keep";

        let mut strm = zeroed_stream();
        init_gzip(&mut strm);
        let head = raw_header();
        // SAFETY: `head` is a live, uniquely-owned `gz_header`.
        unsafe {
            (*head).time = 0x3333_3333;
            (*head).os = 3;
            (*head).name = name.as_ptr().cast::<c_uchar>().cast_mut();
        }
        assert_eq!(unsafe { deflateSetHeader(&mut strm, head) }, Z_OK);

        let first = finish(&mut strm, &payload);
        assert_eq!(unsafe { deflateReset(&mut strm) }, Z_OK);
        let second = finish(&mut strm, &payload);
        assert_eq!(unsafe { deflateEnd(&mut strm) }, Z_OK);
        free_header(head);

        assert_eq!(
            first, second,
            "deflateReset must not drop the registered gzip header"
        );
        assert_eq!(&second[10..15], b"keep\0", "the name is emitted again");
    }

    #[test]
    #[cfg(feature = "gzip")]
    fn copy_carries_the_registered_header_pointer_to_the_clone() {
        // C's `zmemcpy(ds, ss, sizeof(deflate_state))` (`deflate.c` L1339) copies
        // the `gzhead` member, so the clone reads the same caller-owned header.
        let payload = b"copy carries the header".repeat(3);
        let name = c"copy";

        let mut src = zeroed_stream();
        init_gzip(&mut src);
        let head = raw_header();
        // SAFETY: `head` is a live, uniquely-owned `gz_header`.
        unsafe {
            (*head).time = 0x4444_4444;
            (*head).os = 3;
            (*head).name = name.as_ptr().cast::<c_uchar>().cast_mut();
        }
        assert_eq!(unsafe { deflateSetHeader(&mut src, head) }, Z_OK);

        let mut dst = zeroed_stream();
        assert_eq!(unsafe { deflateCopy(&mut dst, &mut src) }, Z_OK);

        let from_src = finish(&mut src, &payload);
        let from_dst = finish(&mut dst, &payload);
        assert_eq!(unsafe { deflateEnd(&mut src) }, Z_OK);
        assert_eq!(unsafe { deflateEnd(&mut dst) }, Z_OK);
        free_header(head);

        assert_eq!(
            from_src, from_dst,
            "a deflateCopy clone must emit the same gzip header as its source"
        );
        assert_eq!(&from_dst[10..15], b"copy\0", "the clone emits the name");
    }
}
