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
    Bytef, CAllocator, HandleKind, TaggedHandle, advance_input, advance_output, guard_int,
    guard_long, guard_ulong, gz_headerp, handle_owner_valid, handle_prefix_valid, in_func,
    init_allocator_prologue, input_ptr_valid, input_slice, install_handle, out_func, output_slice,
    peek_handle_kind, republish_total_in, rewind_input, set_adler, set_data_type, set_msg,
    stream_buffers_valid, uInt, ulong_to_u32, z_stream, z_streamp,
};
use crate::inflate::back::{InFunc, OutFunc};
use crate::inflate::state::{InflateState, InflateStream};
use crate::stream::{BoxedEngine, ZStream};

// `gz_header` (the `#[repr(C)]` mirror) and the header write-back helper are
// only referenced by the gzip-only `inflateGetHeader` path and the header
// write-back inside `inflate`, so gate their imports to avoid unused warnings.
#[cfg(feature = "gzip")]
use crate::ffi::types::{borrow_gz_header_sink, gz_header, publish_gz_header};

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
    zs.adler = ulong_to_u32(caller_adler);
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
    /// Address of the `z_stream` this handle is installed into — C's
    /// `state->strm` (`inflate.c` L203). MUST be the second field. Written by
    /// [`install_handle`]; null until then.
    owner: *const z_stream,
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
    ///
    /// The owner is left null: it is bound by [`install_handle`] at the moment
    /// the handle becomes reachable through a `z_stream`, so a handle that has
    /// not been installed can never pass [`inflate_state_check`].
    fn new(zs: ZStream<CAllocator>) -> Self {
        Self {
            kind: HandleKind::INFLATE,
            owner: ptr::null(),
            zs,
            #[cfg(feature = "gzip")]
            head: ptr::null_mut(),
        }
    }
}

impl TaggedHandle for InflateHandle {
    const KIND: HandleKind = HandleKind::INFLATE;

    #[inline]
    fn set_owner(&mut self, owner: *const z_stream) {
        self.owner = owner;
    }

    #[inline]
    fn engine_status_is_c_valid(&self) -> bool {
        self.zs
            .inflate_state()
            .is_some_and(|st| st.mode.is_c_valid())
    }
}

/// C's `inflateStateCheck` (`inflate.c` L88-L97), reproduced clause for clause.
///
/// Returns `true` when reference zlib would have accepted `strm` — i.e. when C's
/// `inflateStateCheck` would have returned 0: a non-null stream, both allocator
/// hooks live, an installed state, `state->strm == strm`, and
/// `HEAD <= state->mode <= SYNC`.
///
/// Every `inflate*` shim that reaches the engine runs this first, and, critically,
/// **runs it before bridging any auxiliary caller pointer**: C rejects an invalid
/// stream without reading the `dictionary`, `head`, or window pointers passed
/// alongside it, so a stale non-null pointer must not be turned into a Rust slice
/// or reference either.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by
/// [`install_handle`].
#[inline]
#[must_use]
unsafe fn inflate_state_check(strm: &z_stream) -> bool {
    // SAFETY: delegated to the canonical clause helper in `ffi::types`; every
    // clause is discharged there and nothing outside `strm` and its own handle is
    // read.
    unsafe { handle_prefix_valid::<InflateHandle>(strm) }
}

/// Borrows the [`InflateHandle`] behind the opaque `state` pointer after running
/// the whole of C's [`inflate_state_check`]. Returns [`None`] — so the caller can
/// return `Z_STREAM_ERROR` WITHOUT reinterpreting a wrong-type allocation — when
/// no handle is installed, the allocator pair is not live, the handle belongs to
/// a different engine or a different `z_stream`, or the mode is outside
/// `HEAD..=SYNC`.
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by
/// [`install_handle`].
#[inline]
unsafe fn inflate_handle(strm: &mut z_stream) -> Option<&mut InflateHandle> {
    // SAFETY: delegated; runs every clause of C's `inflateStateCheck` and reads
    // nothing outside `strm` and its own handle.
    if !unsafe { inflate_state_check(strm) } {
        return None;
    }
    // SAFETY: the check confirmed the tag and the owner, so the allocation really
    // is a live `InflateHandle`; the borrow is tied to `strm`, so it cannot alias
    // for its lifetime.
    Some(unsafe { &mut *(strm.state as *mut InflateHandle) })
}

/// Reclaims the boxed [`InflateHandle`] from `state` after running the whole of
/// C's [`inflate_state_check`], nulling `state` on success. Returns [`None`] —
/// leaving `state` untouched — when the check fails, so [`inflateEnd`] never
/// drops a wrong-type box and never frees a handle through a stream that does not
/// own it.
///
/// The owner clause is what makes reclaim sound against transplantation: a caller
/// who copies the 14-field `z_stream` holds a second struct pointing at the SAME
/// handle, and freeing through the copy would leave the original with a dangling
/// `state`. C returns `Z_STREAM_ERROR` for the copy (`inflate.c` L95,
/// `state->strm != strm`) and this reproduces that refusal, so the allocation is
/// released exactly once, through its owner.
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by
/// [`install_handle`], not already reclaimed.
#[inline]
unsafe fn inflate_take(strm: &mut z_stream) -> Option<Box<InflateHandle>> {
    // SAFETY: delegated; runs every clause of C's `inflateStateCheck`.
    if !unsafe { inflate_state_check(strm) } {
        return None;
    }
    // SAFETY: the check confirmed a live, owner-matched `Box<InflateHandle>`;
    // reconstitute it exactly once and null the field to prevent a double free.
    let boxed = unsafe { Box::from_raw(strm.state as *mut InflateHandle) };
    strm.state = ptr::null_mut();
    Some(boxed)
}

/// The boxed `inflateBack` engine handle installed in [`z_stream::state`] by
/// [`inflateBackInit_`].
///
/// Unlike the streaming [`inflate`] path, `inflateBack` drives the raw engine
/// directly with a caller-owned window (which doubles as the output buffer), so
/// this handle owns the placed engine state outright rather than an idiomatic
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
    /// Address of the `z_stream` this handle was installed into. MUST be the
    /// second field so the shared `#[repr(C)]` prefix stays uniform across all
    /// three handle kinds.
    ///
    /// Recorded by [`install_handle`], but — unlike the deflate and inflate
    /// handles — **not** consulted by [`inflate_back_state_check`]. Reference
    /// zlib never assigns `state->strm` anywhere in `infback.c`, so there is no
    /// owner for the decode path to compare against; see that function for the
    /// full reasoning and for the one place the field *is* enforced.
    owner: *const z_stream,
    /// The engine state whose window doubles as the sliding output buffer.
    ///
    /// Held as a placed engine, not a plain [`Box`]: reference zlib charges the
    /// state to the caller's `zalloc` (`infback.c` L51) and this shim does the
    /// same, so for a caller with an active hook these bytes live in the caller's
    /// own region and go back through their `zfree` when the handle drops.
    inner: BoxedEngine<InflateState>,
}

impl InflateBackHandle {
    /// Wraps a freshly built back-inflate state, tagging it as an `inflateBack`
    /// handle.
    ///
    /// The owner is left null until [`install_handle`] binds it.
    #[inline]
    fn new(inner: BoxedEngine<InflateState>) -> Self {
        Self {
            kind: HandleKind::INFLATE_BACK,
            owner: ptr::null(),
            inner,
        }
    }
}

impl TaggedHandle for InflateBackHandle {
    const KIND: HandleKind = HandleKind::INFLATE_BACK;

    #[inline]
    fn set_owner(&mut self, owner: *const z_stream) {
        self.owner = owner;
    }

    #[inline]
    fn engine_status_is_c_valid(&self) -> bool {
        // `infback.c` L208-L219 range-checks nothing: `inflateBack` overwrites
        // `state->mode = TYPE` on entry (`infback.c` L221), so whatever mode the
        // previous call left behind is irrelevant and C never rejects on it.
        // Reporting `true` unconditionally is therefore the faithful answer, and
        // it is only ever consulted if some future caller routes this handle
        // through the symmetric `handle_prefix_valid` predicate.
        true
    }
}

/// The entry validation reference zlib performs in `inflateBack`
/// (`infback.c` L208-L219), reproduced clause for clause.
///
/// C checks **strictly less** here than in `inflateStateCheck`, and that
/// difference is deliberate rather than an oversight to be tidied up:
///
/// * `strm == Z_NULL || strm->state == Z_NULL` — the only two clauses C applies.
///   Discharged by holding a `&z_stream` and by the tag read below.
/// * **No allocator clause.** `inflate`/`deflate` reject a stream whose `zalloc`
///   or `zfree` has been cleared; `inflateBack` does not, because it allocates
///   nothing during decode — the window is the caller's.
/// * **No mode clause.** `inflateBack` assigns `state->mode = TYPE` on entry
///   (`infback.c` L221), so the incoming mode cannot make the call invalid.
/// * **No owner clause.** `infback.c` never assigns `state->strm` at all, so
///   reference zlib has no owner recorded to compare against. Imposing one would
///   reject a `z_stream` the caller has *moved* since `inflateBackInit_` — which
///   C accepts on this path — and that is a behavior change on an input C handles
///   defined, so it is not imposed.
///
/// The one clause added beyond C is the Rust-only [`HandleKind`] tag, which is
/// strictly stronger than anything C can express and is what prevents
/// reinterpreting a deflate or inflate allocation through this handle's layout.
///
/// The owner *is* enforced on the reclaim path — see
/// [`inflate_back_end_state_check`] — because there, and only there, tolerating a
/// foreign stream would mean freeing one allocation twice.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by
/// [`install_handle`].
#[inline]
#[must_use]
unsafe fn inflate_back_state_check(strm: &z_stream) -> bool {
    // SAFETY: delegated prefix read; see `peek_handle_kind`.
    let kind = unsafe { peek_handle_kind(strm) };
    kind == Some(HandleKind::INFLATE_BACK)
}

/// The entry validation reference zlib performs in `inflateBackEnd`
/// (`infback.c` L573), plus the owner clause that reclaim soundness requires.
///
/// C's predicate is `strm == Z_NULL || strm->state == Z_NULL ||
/// strm->zfree == (free_func)0` — note `zfree` but *not* `zalloc`, since the only
/// thing left to do is free.
///
/// Beyond that this adds the [`HandleKind`] tag and the owner comparison. The
/// owner is the deliberate divergence, and it is confined to inputs on which
/// reference zlib is already undefined: a caller who *copies* the 14-field
/// `z_stream` holds two structs naming one handle, and C — having no owner to
/// check — frees it through both, a double free. Refusing the copy with
/// `Z_STREAM_ERROR` releases the allocation exactly once, through the stream it
/// was installed into. The cost is that a *moved* stream leaks rather than frees
/// and reports `Z_STREAM_ERROR`; a leak is memory-safe and observable, a double
/// free is neither, and no other zlib engine tolerates a moved stream either
/// (`deflate.c` L546, `inflate.c` L95), so no portable caller can be relying on
/// it.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by
/// [`install_handle`].
#[inline]
#[must_use]
unsafe fn inflate_back_end_state_check(strm: &z_stream) -> bool {
    if strm.zfree.is_none() {
        return false;
    }
    // SAFETY: delegated; the identity clauses read only the handle prefix.
    unsafe { handle_owner_valid::<InflateBackHandle>(strm) }.is_some()
}

/// Borrows the [`InflateBackHandle`] behind the opaque `state` pointer after
/// running [`inflate_back_state_check`] — reference zlib's `inflateBack` entry
/// validation. Returns [`None`] when no handle is installed OR the installed
/// handle is not an `inflateBack` handle (cross-type misuse), so the caller can
/// return `Z_STREAM_ERROR` WITHOUT reinterpreting a wrong-type allocation.
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by
/// [`install_handle`].
#[inline]
unsafe fn inflate_back_handle(strm: &mut z_stream) -> Option<&mut InflateBackHandle> {
    // SAFETY: delegated; runs C's `infback.c` L208-L219 clauses plus the kind tag,
    // and reads nothing outside `strm` and its own handle.
    if !unsafe { inflate_back_state_check(strm) } {
        return None;
    }
    // SAFETY: the tag confirms a live `InflateBackHandle`; the borrow is tied to
    // `strm`, so it cannot alias for its lifetime.
    Some(unsafe { &mut *(strm.state as *mut InflateBackHandle) })
}

/// Reclaims the boxed [`InflateBackHandle`] from `state` after running
/// [`inflate_back_end_state_check`] — reference zlib's `inflateBackEnd`
/// validation (`infback.c` L573) plus the owner clause — nulling `state` on
/// success.
///
/// Returns [`None`], leaving `state` untouched, when the check fails, so
/// [`inflateBackEnd`] never drops a wrong-type box, never frees while the
/// caller's `zfree` hook is absent, and never frees one allocation through two
/// streams.
///
/// # Safety
///
/// A non-null `state` must point at a live handle installed by
/// [`install_handle`], not already reclaimed.
#[inline]
unsafe fn inflate_back_take(strm: &mut z_stream) -> Option<Box<InflateBackHandle>> {
    // SAFETY: delegated; runs C's `infback.c` L573 clauses plus kind and owner.
    if !unsafe { inflate_back_end_state_check(strm) } {
        return None;
    }
    // SAFETY: the check confirmed a live, owner-matched `Box<InflateBackHandle>`;
    // reconstitute exactly once and null the field to prevent a double free.
    let boxed = unsafe { Box::from_raw(strm.state as *mut InflateBackHandle) };
    strm.state = ptr::null_mut();
    Some(boxed)
}

// ---------------------------------------------------------------------------
// C callback adapters for `inflateBack`
// ---------------------------------------------------------------------------

/// Adapts zlib's C `in_func` to the engine's [`InFunc`] trait.
///
/// On the first advance it yields whatever input was already buffered in the
/// stream (`next_in[..avail_in]`); thereafter it invokes the C callback, which
/// returns a byte count and points `*buf` at that many readable input bytes.
///
/// The chunk is never copied. `advance` only records the provenance of the region
/// the callback published, and `chunk` re-forms a slice over it on demand, so the
/// engine decodes straight out of the caller's memory — the zero-allocation
/// property `infback.c` has and a growing owned buffer would not.
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
    fn advance(&mut self) -> bool {
        if !self.initial_done {
            self.initial_done = true;
            if !self.initial.is_empty() {
                // Record the initial stream buffer as the current chunk.
                self.last_ptr = self.initial.as_ptr();
                self.last_len = self.initial.len();
                self.handed_out = true;
                self.dry = false;
                return true;
            }
        }
        let mut buf: *const c_uchar = ptr::null();
        // SAFETY: `in_fn` is a valid zlib `in_func` (checked non-null by the
        // `inflateBack` shim before this adapter is constructed). Per the zlib
        // callback contract it returns a count `n` and stores in `*buf` a pointer
        // to `n` readable bytes that remain valid at least until the next call —
        // which is exactly the window over which `chunk` re-forms the slice.
        let n = unsafe { (self.in_fn)(self.in_desc, &mut buf) };
        if n == 0 || buf.is_null() {
            // Callback ran dry: mirror C `PULL` returning 0 (next = Z_NULL). The
            // recorded provenance is left in place so `chunk` keeps reporting the
            // previous, fully consumed region and C's `have` stays `0`.
            self.dry = true;
            false
        } else {
            // Record this callback buffer as the current chunk.
            self.last_ptr = buf;
            self.last_len = n as usize;
            self.handed_out = true;
            self.dry = false;
            true
        }
    }

    fn chunk(&self) -> &[u8] {
        if !self.handed_out {
            // Nothing has been published yet. `last_ptr`/`last_len` are
            // pre-seeded with the initial buffer so the exit-cursor arithmetic
            // works even for a decode that pulls nothing, but that region has not
            // been handed to the engine, so reporting it here would let the
            // decoder consume the same bytes twice.
            return &[];
        }
        // SAFETY: `handed_out` means the most recent successful `advance` recorded
        // `last_len` readable bytes at `last_ptr` — either the caller's
        // `next_in[..avail_in]` window or a region the C `in_func` published.
        // Either stays valid until the next callback invocation, and `advance` is
        // the only thing that invokes it, so no slice formed here outlives its
        // region. `last_len` came from a `c_uint`, so it cannot exceed
        // `isize::MAX`.
        unsafe { slice::from_raw_parts(self.last_ptr, self.last_len) }
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
                // SAFETY: transfers ownership of the box into the opaque `state`
                // slot and binds the handle's owner to this stream — C's
                // `state->strm = strm` (`inflate.c` L203). `state` was reclaimed
                // (or was never populated) before this point, so nothing leaks.
                // Reclaimed and dropped by `inflateEnd`.
                unsafe { install_handle(sref, handle) };
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
/// The region is adopted, never written — see `crate::ffi::alloc`'s
/// `borrow_caller_window`. A caller that pre-fills its window and then makes a
/// call this function refuses (a rejected argument, or an allocator that turns
/// the state request down) finds the buffer byte-for-byte unchanged, exactly as a
/// C build leaves it.
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
        //
        // The borrow is handed over as a *closure* so the engine can run C's
        // ordering: the state `ZALLOC` (`infback.c` L51-L53) first, and only on
        // its success the L60 adoption. A refused state request therefore returns
        // `Z_MEM_ERROR` without this closure ever running, leaving the caller's
        // window as untouched as C leaves it. The `1 << window_bits` length is
        // computed here — not taken from the caller — because C derives
        // `state->wsize` the same way and never inspects the buffer's real size.
        let lend = move || {
            // SAFETY: `window` is non-null (checked above) and the caller's
            // documented contract on this entry point guarantees it is valid for
            // reads and writes of `1 << window_bits` bytes and stays valid,
            // un-aliased, until `inflateBackEnd` destroys the state built from it.
            // The bytes need not be initialized; nothing here writes or reads them.
            unsafe { crate::ffi::alloc::borrow_caller_window(window, 1usize << window_bits) }
        };

        match crate::inflate::back::inflate_back_init_borrowed_window(&allocator, window_bits, lend)
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
                // `inflateBackEnd` after tag validation. The owner is recorded
                // here for the reclaim-path check even though `infback.c` itself
                // never assigns `state->strm` — see `inflate_back_state_check`.
                unsafe { install_handle(sref, handle) };
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
///
/// # Safety
///
/// `strm` may be null, which is rejected with `Z_STREAM_ERROR` before any
/// dereference. When non-null it must be a valid, exclusively-owned `z_stream`
/// previously initialized by `inflateInit*`, with its `state` handle unmodified
/// by the caller. A handle belonging to a different engine is *not* undefined
/// behavior either: `inflate_handle` checks the `HandleKind` tag before the state
/// is reborrowed, and a mismatch yields `Z_STREAM_ERROR`.
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
/// **Registered gzip header.** If a `gz_header` was registered through
/// [`inflateGetHeader`], it must still be live and writable for this call, and
/// its `extra` / `name` / `comment` buffers must address at least the
/// `extra_max` / `name_max` / `comm_max` bytes they advertise, since this call
/// publishes into them.
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
/// `inflate`'s entry guard (`inflate.c` L474) and exists precisely so the
/// programmer error surfaces as an error code instead of being silently masked
/// into an empty slice. The complementary shape `avail_in == 0` with
/// `next_in == NULL` is legal: nothing is read. Note that it is not thereby
/// productive — unlike `deflate`, which has an RFC 1950 header of its own to
/// publish, a raw `inflate` with an empty input window has nothing to do and
/// reports `Z_BUF_ERROR`.
///
/// **Unwinding.** The body runs inside a `guard_int` panic guard, so a Rust
/// panic can never unwind across this `extern "C"` boundary. With `std` the
/// guard is a `catch_unwind` that substitutes `Z_STREAM_ERROR`; without it the
/// closure runs directly, because a `no_std` build has no unwinding runtime to
/// catch and the crate sets `panic = "abort"` in both profiles, so a panic
/// aborts the process rather than crossing the boundary. Either way the caller
/// never observes a foreign unwind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inflate(strm: z_streamp, flush: c_int) -> c_int {
    guard_int(Z_STREAM_ERROR, move || {
        if strm.is_null() {
            return Z_STREAM_ERROR;
        }
        // SAFETY: `strm` is non-null and a valid caller-owned `z_stream`.
        let sref = unsafe { &mut *strm };

        // C `inflate` validates the *stream* first: `inflateStateCheck(strm) ||
        // strm->next_out == Z_NULL || (strm->next_in == Z_NULL && avail_in != 0)`
        // is a single expression whose first operand is the state clause
        // (`inflate.c` L474). Running it here, rather than after the slices are
        // formed, is also a soundness requirement: on an unvalidated stream
        // `next_in`/`next_out` may be stale, and constructing a slice over a stale
        // pointer is undefined behavior even when the slice is never read.
        // SAFETY: `state`, when non-null, was installed by `inflateInit*` via
        // `install_handle`; the check reads nothing outside `sref` and its handle.
        if !unsafe { inflate_state_check(sref) } {
            return Z_STREAM_ERROR;
        }

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
        // Lend the decoder the caller's own gzip-header output buffers for the
        // duration of this call. C stores every decoded header byte straight into
        // them, re-reading `head->extra`/`name`/`comment` and their `*_max`
        // capacities as it goes (`inflate.c` L614-L621, L632-L637, L654-L659);
        // re-materializing the view here — rather than snapshotting it back at
        // `inflateGetHeader` time — is what makes a sink installed, resized or
        // withdrawn after registration behave as it does in C, and what keeps the
        // header path free of the allocation (and therefore of the `Z_MEM_ERROR`)
        // that C does not have.
        //
        // The pointer is read out of the *validated* handle, so the header is only
        // dereferenced once the state check has passed, and the slices alias the
        // caller's buffers rather than the stream, so they coexist with the
        // `&mut handle.zs` borrow below.
        #[cfg(feature = "gzip")]
        let head_ptr = handle.head;
        // The sink is scoped to the engine call alone: the mutable slices it holds
        // alias the caller's header buffers, and the write-back below reaches the
        // same struct through `head_ptr`, so the borrow must be provably finished
        // before that happens.
        let tracked = {
            // SAFETY: `head_ptr` is either null or the `gz_header` the caller
            // registered through `inflateGetHeader` and undertook to keep valid,
            // with `extra`/`name`/`comment` writable for their declared `*_max`.
            #[cfg(feature = "gzip")]
            let mut sink = unsafe { borrow_gz_header_sink(head_ptr) };
            crate::inflate::inflate_tracked_lending(
                &mut handle.zs,
                input,
                output,
                flush,
                #[cfg(feature = "gzip")]
                sink.as_mut(),
            )
        };
        let outcome = tracked.outcome;

        // C's `inflate_fast` gives back `bits >> 3` whole input bytes by rewinding a
        // raw pointer (`inffast.c` L290-L294), which may move `next_in` to before
        // where this call began when the accumulator held whole bytes on entry. A
        // `&[u8]` index cannot express that, so the engine defers the out-of-slice
        // part; settle it here, where the caller's one contiguous buffer makes it
        // meaningful. This must precede the `data_type` snapshot below, because the
        // settlement reduces `bits` and `data_type`'s low seven bits *are* `bits`.
        let rewound = crate::inflate::inflate_take_input_history_rewind(&mut handle.zs);

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
        if rewound != 0 {
            // The deferred half of C's give-back, applied as the inverse of the
            // advance above: `next_in` ends at `entry + consumed - rewound` and
            // `avail_in` at `entry - consumed + rewound`.
            // SAFETY: `rewound` counts bytes the decoder itself read from this
            // stream on an earlier call, so they precede `next_in` inside the
            // caller's buffer — the same contiguity reference zlib requires. The
            // pointer is not dereferenced.
            unsafe { rewind_input(sref, rewound) };
            // `total_in` cannot be maintained by composing the two adjustments
            // above: C collapses the call into a single `uInt` delta and only then
            // widens it, so a give-back larger than the bytes consumed wraps at
            // 2^32 rather than 2^64. Rebuild that delta from the pre-call total.
            republish_total_in(sref, prev_total_in, outcome.consumed, rewound);
        }
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
        // Mirrored from the engine rather than written as a literal so the two
        // layers cannot drift: `inflate_reset_keep` performs C's
        // `strm->data_type = 0` (`inflate.c` L107) on the owned stream, and this
        // is the value the caller must observe.
        let data_type = handle.zs.data_type;
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
                // C's `inflateResetKeep` clears the caller-visible decode-state
                // word unconditionally, on the same line as `total_in`/`msg`
                // (`inflate.c` L105-L107), and every reset variant funnels
                // through it (`inflateReset` L125-L134, `inflateReset2`
                // L136-L177). Leaving the previous value in place published a
                // stale `data_type` — `128` (mode == TYPE) after a clean reset,
                // or the residue of an aborted stream — to a caller that C
                // guarantees sees `0`. Only the success path writes: both
                // `inflateStateCheck` and `inflateReset2`'s `windowBits`
                // validation return before any field is touched.
                set_data_type(sref, data_type);
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
        // Mirrored from the engine rather than written as a literal so the two
        // layers cannot drift: `inflate_reset_keep` performs C's
        // `strm->data_type = 0` (`inflate.c` L107) on the owned stream, and this
        // is the value the caller must observe.
        let data_type = handle.zs.data_type;
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
                // C's `inflateResetKeep` clears the caller-visible decode-state
                // word unconditionally, on the same line as `total_in`/`msg`
                // (`inflate.c` L105-L107), and every reset variant funnels
                // through it (`inflateReset` L125-L134, `inflateReset2`
                // L136-L177). Leaving the previous value in place published a
                // stale `data_type` — `128` (mode == TYPE) after a clean reset,
                // or the residue of an aborted stream — to a caller that C
                // guarantees sees `0`. Only the success path writes: both
                // `inflateStateCheck` and `inflateReset2`'s `windowBits`
                // validation return before any field is touched.
                set_data_type(sref, data_type);
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
        // Mirrored from the engine rather than written as a literal so the two
        // layers cannot drift: `inflate_reset_keep` performs C's
        // `strm->data_type = 0` (`inflate.c` L107) on the owned stream, and this
        // is the value the caller must observe.
        let data_type = handle.zs.data_type;
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
                // C's `inflateResetKeep` clears the caller-visible decode-state
                // word unconditionally, on the same line as `total_in`/`msg`
                // (`inflate.c` L105-L107), and every reset variant funnels
                // through it (`inflateReset` L125-L134, `inflateReset2`
                // L136-L177). Leaving the previous value in place published a
                // stale `data_type` — `128` (mode == TYPE) after a clean reset,
                // or the residue of an aborted stream — to a caller that C
                // guarantees sees `0`. Only the success path writes: both
                // `inflateStateCheck` and `inflateReset2`'s `windowBits`
                // validation return before any field is touched.
                set_data_type(sref, data_type);
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

        // C runs `inflateStateCheck(strm)` before it looks at `dictionary` at all
        // (`inflate.c` L1256-L1262); the dictionary bytes are read only afterwards,
        // and only when the state permits. Validating first is what makes the
        // `slice::from_raw_parts` below sound: a stale pointer paired with an
        // invalid stream must be refused without a slice ever being constructed
        // over it, because constructing one is undefined behavior even if it is
        // never read.
        // SAFETY: `state`, when non-null, was installed via `install_handle`; the
        // check reads nothing outside `sref` and its own handle.
        if !unsafe { inflate_state_check(sref) } {
            return Z_STREAM_ERROR;
        }

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

        // C runs `inflateStateCheck(strm)` as the very first statement of
        // `inflateSync` (`inflate.c` L1349-L1351) and only then reaches
        // `syncsearch(..., strm->next_in, ...)`. Validating before the input
        // window is bridged reproduces that order and keeps a stale `next_in` on
        // an invalid stream from being turned into a slice.
        // SAFETY: `state`, when non-null, was installed via `install_handle`; the
        // check reads nothing outside `sref` and its own handle.
        if !unsafe { inflate_state_check(sref) } {
            return Z_STREAM_ERROR;
        }

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

        // C evaluates `inflateStateCheck(source) || dest == Z_NULL` as one
        // expression (`inflate.c` L1381), so the *source* is fully validated
        // before anything else is read — including its own allocator fields — and
        // before `dest` is used at all. Running the whole predicate first means an
        // invalid source is refused before a reference to either stream is formed
        // for any other purpose.
        // SAFETY: `source` is non-null and valid; the check reads only `source`
        // and its own handle prefix and leaves no borrow behind.
        if !unsafe { inflate_state_check(&*source) } {
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
                // `dest.state` and re-points its owner at `dest`, reproducing
                // C's `ds->strm = dest` in `deflateCopy` (`deflate.c` L1340) —
                // the clone belongs to the destination, not to the source, so
                // `inflateEnd(dest)` reclaims it and `inflateEnd(source)` cannot.
                // Reclaimed by `inflateEnd`.
                unsafe { install_handle(dref, boxed) };

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
/// The caller's `extra`/`name`/`comment` are **output** buffers here, and they
/// stay in the caller's memory: this shim records only the registration plus the
/// raw pointer, and [`inflate`] lends the decoder a borrowed view of those buffers
/// on every call. Nothing about the header is read at registration time, matching
/// C — which likewise consults `extra`/`name`/`comment` and their `*_max`
/// capacities only later, live, from the parser (`inflate.c` L614-L621, L632-L637,
/// L654-L659). A caller may therefore install or resize a sink *after* registering,
/// exactly as it can in C, and the header path allocates nothing, so it cannot
/// report the `Z_MEM_ERROR` C has no way to return.
///
/// The header's decoded scalars are mirrored back by [`inflate`] through C's
/// incremental publication schedule, so a caller polling between calls sees only
/// what the stream has actually delivered.
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

        // C's `inflateGetHeader` runs `inflateStateCheck(strm)` and the
        // `(state->wrap & 2) == 0` test *before* it touches `head` at all — the
        // first write to the caller's struct is `head->done = 0` after both
        // clauses pass (`inflate.c` L1219-L1230). Reading `head->extra`/`name`/
        // `comment`/`*_max` below therefore must not happen until the stream has
        // been validated: a caller who passes a stale header alongside an
        // uninitialized stream gets C's `Z_STREAM_ERROR` without this shim
        // dereferencing the header.
        // SAFETY: `state`, when non-null, was installed via `install_handle`; the
        // check reads nothing outside `sref` and its own handle.
        if !unsafe { inflate_state_check(sref) } {
            return Z_STREAM_ERROR;
        }

        // Record only *that* a caller-owned header is registered. Nothing about
        // it is read here — not a buffer pointer, not a capacity — because C reads
        // nothing either: `inflateGetHeader` stores the pointer and clears `done`,
        // full stop (`inflate.c` L1228-L1229), and every field is consulted later,
        // live, by the parser.
        //
        // Snapshotting the pointers and `*_max` values at this instant was a real
        // divergence: a caller that registers a header and only afterwards installs
        // its `extra`/`name`/`comment` buffers — which C honors — would have had
        // those fields silently skipped, and the owned capture vectors introduced an
        // allocation, hence a `Z_MEM_ERROR`, on a path C cannot fail on. The
        // per-call `borrow_gz_header_sink` view in `inflate` replaces both.
        //
        // SAFETY: `state`, if non-null, is the `Box<InflateHandle>`.
        let handle = match unsafe { inflate_handle(sref) } {
            Some(h) => h,
            None => return Z_STREAM_ERROR,
        };
        match crate::inflate::inflate_get_header_foreign(&mut handle.zs, !head.is_null()) {
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
                // one (registering it as "no header", exactly as C's assignment of
                // a null pointer would), so the write is guarded to keep that
                // tolerance rather than reintroducing the crash.
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

        // C's `inflateBack` validates `strm` and `strm->state` (plus both
        // callbacks) as its first act (`infback.c` L208-L219) and only afterwards
        // copies `strm->next_in`/`avail_in` into its local `next`/`have`
        // (`infback.c` L226-L231). Validating before the window is bridged
        // reproduces that order and keeps a stale `next_in` on a stream that was
        // never `inflateBackInit_`-ed from being turned into a slice.
        // SAFETY: `state`, when non-null, is a live tagged handle installed via
        // `install_handle`; the check reads only the handle prefix.
        if !unsafe { inflate_back_state_check(sref) } {
            return Z_STREAM_ERROR;
        }

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
        let state = &mut **handle.inner;

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

        let outcome = crate::inflate::back::inflate_back(state, &mut src, &mut sink);
        // `state`/`handle` borrows of `sref` end here; `src` is a local that only
        // borrows the detached-lifetime `initial`, so it stays readable below.

        // Publish `strm->msg`. C clears it at `infback.c` L214 — after the
        // valid-state check at L209-L210, which is why a rejected call (handled
        // above, before this point) must leave the caller's existing diagnostic
        // alone — and each of the twelve error sites then stores its own literal.
        // `BackMsg` carries which of those three happened; `msg_to_cstr` maps the
        // engine's `&'static str` onto the matching `c"…"` literal.
        match outcome.msg {
            crate::inflate::back::BackMsg::Untouched => {}
            crate::inflate::back::BackMsg::Cleared => set_msg(sref, ptr::null()),
            crate::inflate::back::BackMsg::Set(m) => set_msg(sref, msg_to_cstr(Some(m))),
        }

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

        outcome.code.as_c_int()
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
                crate::inflate::back::inflate_back_end_engine(handle.inner).as_c_int()
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
    /// C `Z_FINISH`.
    const Z_FINISH: c_int = crate::constants::Z_FINISH;
    /// C `Z_BLOCK` — stop at the next deflate block boundary.
    const Z_BLOCK: c_int = crate::constants::Z_BLOCK;
    /// C `Z_DATA_ERROR`.
    const Z_DATA_ERROR: c_int = ReturnCode::DataError.as_c_int();
    /// C `Z_BUF_ERROR`.
    const Z_BUF_ERROR: c_int = ReturnCode::BufError.as_c_int();

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

    // -- inflateGetDictionary / inflatePrime / inflateValidate / -----------
    // -- inflateUndermine: C-boundary behaviour ---------------------------

    /// Installs a deflate handle in `strm` so a cross-engine rejection can be
    /// exercised. The caller must pass the same `strm` to `deflateEnd`.
    ///
    /// The `HandleKind` tag makes every inflate shim reject this stream. C would
    /// reinterpret the pointer as an `inflate_state` and misread it; the tag check
    /// is deliberate hardening beyond C (module header, "cross-engine handle").
    ///
    /// It initializes IN PLACE, through `&mut z_stream`, rather than returning an
    /// initialized stream by value. `deflateInit_` records the owning stream's
    /// address in its handle and every later entry point re-checks it - C's
    /// `s->strm != strm` clause (`deflate.c` L546, `inflate.c` L95), ported by
    /// `handle_owner_valid`. Returning the stream by value MOVES it to a different
    /// address, so the handle would point at the caller's slot while recording the
    /// helper's, and `deflateEnd` would answer `Z_STREAM_ERROR` and leak the
    /// engine - exactly as reference zlib would. Initializing at the final address
    /// keeps the test about the cross-engine tag rather than about a moved stream.
    fn install_a_deflate_handle(strm: &mut z_stream) {
        assert_eq!(
            unsafe {
                crate::ffi::deflate::deflateInit_(
                    strm,
                    6,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
    }

    /// Re-encodes `src`'s DEFLATE bitstream `k` bits later, returning a buffer
    /// one byte longer whose low `k` bits are zero padding.
    ///
    /// DEFLATE packs bits least-significant-first, so bit `j` of `src` lives at
    /// `src[j / 8] >> (j % 8)`. The result satisfies `out` bit `j + k` == `src`
    /// bit `j`, which is exactly the shape a caller resuming at a non-byte
    /// boundary has to hand to `inflatePrime`.
    ///
    /// # Panics
    ///
    /// If `k` is not in `1..8` — a whole-byte shift needs no priming at all.
    fn shift_stream_later_by_bits(src: &[u8], k: u32) -> Vec<u8> {
        assert!((1..8).contains(&k), "k must be a partial-byte shift");
        let mut out = vec![0u8; src.len() + 1];
        for (i, &byte) in src.iter().enumerate() {
            out[i] |= byte << k;
            out[i + 1] |= byte >> (8 - k);
        }
        out
    }

    /// `int inflateGetDictionary(z_streamp, Bytef *, uInt *)` at the C boundary
    /// (`ZLIB_1.2.9`).
    ///
    /// The shim sizes the caller's buffer from the engine's `whave`, so the
    /// null/non-null combinations of the two output pointers are the whole
    /// contract surface and all four are exercised. The bytes are cross-checked
    /// against [`crate::inflate::inflate_get_dictionary`] read through the same
    /// handle, so the shim cannot pass by copying the right *number* of the wrong
    /// bytes.
    ///
    /// History only exists once a call has *returned* mid-stream: C folds produced
    /// bytes into the check value at `CHECK` and resets its progress counter
    /// (`out = left`, `inflate.c` L1121), so a stream decoded entirely inside one
    /// call never touches the window. The decode below is therefore deliberately
    /// output-starved rather than run to `Z_STREAM_END`.
    #[test]
    fn inflate_get_dictionary_at_the_c_boundary() {
        assert_eq!(
            unsafe { inflateGetDictionary(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        let mut bare = zeroed_stream();
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { inflateGetDictionary(&mut bare, ptr::null_mut(), &mut len) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );
        assert_eq!(
            len, 0xdead,
            "a rejected call must not write the length out-parameter"
        );

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

        // Before any decode the window is empty, so every combination reports 0
        // and copies nothing.
        let mut buf = [0xa5u8; 64];
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, buf.as_mut_ptr(), &mut len) },
            Z_OK
        );
        assert_eq!(len, 0, "a fresh stream has no history");
        assert!(
            buf.iter().all(|&b| b == 0xa5),
            "an empty history writes no bytes"
        );
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, ptr::null_mut(), ptr::null_mut()) },
            Z_OK,
            "both output pointers NULL is legal"
        );

        // Decode an output-starved prefix so the call returns mid-stream and the
        // engine flushes what it produced into the history window.
        const PREFIX: usize = 12;
        let mut sink = [0u8; PREFIX];
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = sink.as_mut_ptr();
        strm.avail_out = PREFIX as c_uint;
        assert_eq!(
            unsafe { inflate(&mut strm, Z_NO_FLUSH) },
            Z_OK,
            "an output-starved call returns Z_OK, not Z_STREAM_END"
        );
        assert_eq!(strm.avail_out, 0, "the whole sink was filled");
        assert_eq!(&sink[..], &MSG[..PREFIX]);

        // (a) Length-only query: `dictionary == NULL`.
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, ptr::null_mut(), &mut len) },
            Z_OK
        );
        assert_eq!(
            len as usize, PREFIX,
            "the history is exactly what was produced"
        );

        // (b) Both pointers: exact bytes and exact length, nothing beyond.
        let mut buf = [0xa5u8; 64];
        let mut len: uInt = 0;
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, buf.as_mut_ptr(), &mut len) },
            Z_OK
        );
        assert_eq!(len as usize, PREFIX);
        assert_eq!(
            &buf[..PREFIX],
            &MSG[..PREFIX],
            "the history is the decoded prefix, in order"
        );
        assert!(
            buf[PREFIX..].iter().all(|&b| b == 0xa5),
            "exactly `whave` bytes are written and no more"
        );

        // (c) Buffer only: a NULL `dictLength` must not be dereferenced, and the
        //     copy must still happen.
        let mut buf2 = [0x5au8; 64];
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, buf2.as_mut_ptr(), ptr::null_mut()) },
            Z_OK,
            "a NULL dictLength is legal, not an error"
        );
        assert_eq!(&buf2[..PREFIX], &MSG[..PREFIX]);
        assert!(buf2[PREFIX..].iter().all(|&b| b == 0x5a));

        // Cross-check the bytes against the safe core through the same handle.
        {
            let handle = unsafe { inflate_handle(&mut strm) }.expect("an inflate handle");
            let mut core_bytes = vec![0u8; PREFIX];
            let mut core_len: usize = 0;
            assert_eq!(
                crate::inflate::inflate_get_dictionary(&handle.zs, &mut core_bytes, &mut core_len),
                Ok(ReturnCode::Ok)
            );
            assert_eq!(core_len, PREFIX, "core and shim agree on the length");
            assert_eq!(
                &buf[..PREFIX],
                &core_bytes[..],
                "the shim copied exactly what the safe core reports"
            );
        }

        // The stream is still usable: the query is non-destructive.
        let mut rest = vec![0u8; 512];
        strm.next_out = rest.as_mut_ptr();
        strm.avail_out = rest.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
        let produced = rest.len() - strm.avail_out as usize;
        assert_eq!(&rest[..produced], &MSG[PREFIX..]);

        // A deflate handle is rejected by the tag check rather than misread.
        let mut def = zeroed_stream();
        install_a_deflate_handle(&mut def);
        let mut len: uInt = 0xdead;
        assert_eq!(
            unsafe { inflateGetDictionary(&mut def, ptr::null_mut(), &mut len) },
            Z_STREAM_ERROR,
            "a deflate handle is not an inflate handle"
        );
        assert_eq!(len, 0xdead);
        assert_eq!(unsafe { crate::ffi::deflate::deflateEnd(&mut def) }, Z_OK);

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, ptr::null_mut(), ptr::null_mut()) },
            Z_STREAM_ERROR,
            "inflateEnd nulls `state`, so a later query is rejected"
        );
    }

    /// `inflateGetDictionary` must return a **wrapped** history in chronological
    /// order at the C boundary, not in raw window-buffer order.
    ///
    /// Once more bytes have been produced than the window holds, the engine's
    /// circular buffer has `wnext != 0` and the oldest surviving byte sits at
    /// `window[wnext]`, not at `window[0]`. C therefore emits `window[wnext..whave]`
    /// followed by `window[..wnext]` (`inflate.c` L1301-L1313). A shim that handed
    /// the window back verbatim would return the right *count* of bytes in the
    /// wrong *order* — decodable-looking, silently corrupt as a preset dictionary,
    /// and invisible to a length-only assertion. This is the one defect the report
    /// names for this entry point, so it gets its own vector.
    ///
    /// A 512-byte window (`windowBits = 9`) is used on both sides so the wrap is
    /// reached quickly; the encoder must match, or the decoder would see distances
    /// beyond its window.
    #[test]
    fn inflate_get_dictionary_at_the_c_boundary_wraps_in_chronological_order() {
        use crate::constants::{Z_DEFAULT_STRATEGY, Z_DEFLATED, Z_FINISH};
        use crate::ffi::deflate::{deflate, deflateEnd, deflateInit2_};

        // Low-order-byte noise: compressible enough to stream, varied enough that
        // an out-of-order window would not accidentally compare equal.
        let payload: Vec<u8> = (0..1500u32)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i >> 3) % 251) as u8)
            .collect();

        // Encode at windowBits 9 so the decoder's 512-byte window suffices.
        let mut d = zeroed_stream();
        assert_eq!(
            unsafe {
                deflateInit2_(
                    &mut d,
                    6,
                    Z_DEFLATED,
                    9,
                    8,
                    Z_DEFAULT_STRATEGY,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        let mut encoded = vec![0u8; payload.len() + 256];
        d.next_in = payload.as_ptr();
        d.avail_in = payload.len() as c_uint;
        d.next_out = encoded.as_mut_ptr();
        d.avail_out = encoded.len() as c_uint;
        assert_eq!(unsafe { deflate(&mut d, Z_FINISH) }, Z_STREAM_END);
        let produced = encoded.len() - d.avail_out as usize;
        encoded.truncate(produced);
        assert_eq!(unsafe { deflateEnd(&mut d) }, Z_OK);

        let mut strm = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut strm,
                    9,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );

        // Decode in small output-starved slices and stop the instant the window
        // has both filled AND wrapped. Stopping mid-stream is mandatory: a stream
        // finished inside a single call leaves the window untouched.
        const SLICE: usize = 64;
        let mut decoded: Vec<u8> = Vec::new();
        let mut cursor = 0usize;
        let mut wrapped = None;
        strm.next_in = encoded.as_ptr();
        strm.avail_in = encoded.len() as c_uint;
        for _ in 0..(payload.len() / SLICE + 8) {
            let mut chunk = [0u8; SLICE];
            strm.next_out = chunk.as_mut_ptr();
            strm.avail_out = SLICE as c_uint;
            let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
            let n = SLICE - strm.avail_out as usize;
            decoded.extend_from_slice(&chunk[..n]);
            cursor += n;

            let (whave, wnext, wsize) = {
                let handle = unsafe { inflate_handle(&mut strm) }.expect("an inflate handle");
                let s = handle.zs.inflate_state().expect("engine state");
                (s.whave as usize, s.wnext as usize, s.wsize as usize)
            };
            if whave == wsize && wsize != 0 && wnext != 0 {
                wrapped = Some((whave, wnext, wsize));
                break;
            }
            assert_ne!(
                rc, Z_STREAM_END,
                "the stream ended before the window wrapped; enlarge the payload"
            );
        }

        let (whave, wnext, wsize) = wrapped.expect("the window must fill and wrap");
        assert_eq!(wsize, 512, "windowBits 9 gives a 512-byte window");
        assert_eq!(whave, wsize, "the window is full");
        assert!(wnext > 0 && wnext < wsize, "the write cursor has wrapped");
        assert!(
            cursor > wsize,
            "more bytes were produced than the window holds"
        );

        // Query through the C entry point.
        let mut dict = vec![0xa5u8; whave + 16];
        let mut len: uInt = 0;
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, dict.as_mut_ptr(), &mut len) },
            Z_OK
        );
        assert_eq!(
            len as usize, whave,
            "a full window reports its whole length"
        );
        assert!(
            dict[whave..].iter().all(|&b| b == 0xa5),
            "exactly `whave` bytes are written and no more"
        );

        // The history must be the most recent `whave` decoded bytes, in order.
        assert_eq!(
            &dict[..whave],
            &decoded[decoded.len() - whave..],
            "the wrapped history is returned oldest-to-newest"
        );

        // Independent cross-check of the splice itself, so the assertion above
        // cannot be satisfied by a coincidentally-aligned window.
        {
            let handle = unsafe { inflate_handle(&mut strm) }.expect("an inflate handle");
            let s = handle.zs.inflate_state().expect("engine state");
            let mut spliced = Vec::with_capacity(whave);
            spliced.extend_from_slice(&s.window[wnext..whave]);
            spliced.extend_from_slice(&s.window[..wnext]);
            assert_eq!(
                &dict[..whave],
                &spliced[..],
                "the shim emits window[wnext..whave] then window[..wnext]"
            );
            assert_ne!(
                &dict[..whave],
                &s.window[..whave],
                "a wrapped window is NOT raw buffer order, so the test is meaningful"
            );
        }

        // The query is non-destructive: the rest of the stream still decodes.
        let mut tail = vec![0u8; payload.len()];
        strm.next_out = tail.as_mut_ptr();
        strm.avail_out = tail.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        let n = tail.len() - strm.avail_out as usize;
        decoded.extend_from_slice(&tail[..n]);
        assert_eq!(decoded, payload, "the full payload round-trips");

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// `int inflatePrime(z_streamp, int, int)` at the C boundary
    /// (`ZLIB_1.2.2.4`).
    ///
    /// Every documented outcome of C `inflatePrime` (`inflate.c` L219-L236) is
    /// asserted with its exact code — note that an over-wide request is
    /// `Z_STREAM_ERROR` here, **not** the `Z_BUF_ERROR` its `deflatePrime`
    /// counterpart returns, and the two must not be conflated.
    ///
    /// The success path is proved functionally rather than by its return code: a
    /// raw stream is re-emitted three bits later, and only a correctly primed
    /// accumulator reconstructs the original bitstream. The unprimed control
    /// decode is asserted to *fail* to produce the message, so the test cannot
    /// pass with `inflatePrime` stubbed out to `Z_OK`.
    #[test]
    fn inflate_prime_at_the_c_boundary() {
        assert_eq!(
            unsafe { inflatePrime(ptr::null_mut(), 8, 0) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        let mut bare = zeroed_stream();
        assert_eq!(
            unsafe { inflatePrime(&mut bare, 8, 0) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );

        const K: u32 = 3;
        let shifted = shift_stream_later_by_bits(RAW_STREAM, K);

        // Control: the shifted stream without priming must NOT yield the message.
        // Feeding `shifted[1..]` drops the bits stranded in `shifted[0]`.
        let (rc, out) = {
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
                Z_OK
            );
            let mut out = vec![0u8; 512];
            strm.next_in = shifted[1..].as_ptr();
            strm.avail_in = (shifted.len() - 1) as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
            let produced = out.len() - strm.avail_out as usize;
            out.truncate(produced);
            assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
            (rc, out)
        };
        assert!(
            rc != Z_STREAM_END || out != MSG,
            "without priming the bit-shifted stream must not decode to the message"
        );

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
            Z_OK
        );

        // A zero-width prime is legal and a no-op (C's loop exits immediately).
        assert_eq!(
            unsafe { inflatePrime(&mut strm, 0, 0) },
            Z_OK,
            "a 0-bit prime is accepted"
        );

        // Over-wide and accumulator-overflowing requests are Z_STREAM_ERROR.
        assert_eq!(
            unsafe { inflatePrime(&mut strm, 17, 0) },
            Z_STREAM_ERROR,
            "17 bits exceeds the 16-bit limit"
        );
        assert_eq!(unsafe { inflatePrime(&mut strm, 16, 0xffff) }, Z_OK);
        assert_eq!(unsafe { inflatePrime(&mut strm, 16, 0xffff) }, Z_OK);
        assert_eq!(
            unsafe { inflatePrime(&mut strm, 16, 0) },
            Z_STREAM_ERROR,
            "a 48-bit accumulator would overflow the 32-bit hold"
        );

        // A negative width flushes the accumulator, which is what makes the
        // over-filled state above recoverable.
        assert_eq!(
            unsafe { inflatePrime(&mut strm, -1, 0) },
            Z_OK,
            "a negative width flushes the bit buffer"
        );
        {
            let handle = unsafe { inflate_handle(&mut strm) }.expect("an inflate handle");
            let s = handle.zs.inflate_state().expect("engine state");
            assert_eq!((s.hold, s.bits), (0, 0), "the accumulator is empty again");
        }

        // Now the real use: inject the 8 - K high bits stranded in `shifted[0]`
        // and decode from `shifted[1]`, reconstructing the original bitstream.
        assert_eq!(
            unsafe { inflatePrime(&mut strm, (8 - K) as c_int, c_int::from(shifted[0] >> K)) },
            Z_OK
        );
        let mut out = vec![0u8; 512];
        strm.next_in = shifted[1..].as_ptr();
        strm.avail_in = (shifted.len() - 1) as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(
            unsafe { inflate(&mut strm, Z_NO_FLUSH) },
            Z_STREAM_END,
            "a primed accumulator decodes the bit-shifted stream"
        );
        let produced = out.len() - strm.avail_out as usize;
        assert_eq!(
            &out[..produced],
            MSG,
            "priming reconstructs the original bitstream exactly"
        );

        // A deflate handle is rejected by the tag check.
        let mut def = zeroed_stream();
        install_a_deflate_handle(&mut def);
        assert_eq!(
            unsafe { inflatePrime(&mut def, 8, 0) },
            Z_STREAM_ERROR,
            "a deflate handle is not an inflate handle"
        );
        assert_eq!(unsafe { crate::ffi::deflate::deflateEnd(&mut def) }, Z_OK);

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            unsafe { inflatePrime(&mut strm, 8, 0) },
            Z_STREAM_ERROR,
            "inflateEnd nulls `state`, so a later prime is rejected"
        );
    }

    /// `int inflateValidate(z_streamp, int)` at the C boundary (`ZLIB_1.2.9`).
    ///
    /// The only externally visible effect of this call is whether a stream with a
    /// broken trailer is accepted, so that is what is asserted — in both
    /// directions, on the *same* corrupted vector: rejected with `Z_DATA_ERROR`
    /// by default, accepted with `Z_STREAM_END` and the correct plaintext once
    /// validation is switched off, and rejected again once it is switched back on.
    /// A shim that returned `Z_OK` without touching `wrap` would pass a
    /// return-code-only test and fail this one.
    ///
    /// The `state.wrap != 0` guard in C (`inflate.c` L1385-L1395) means enabling
    /// validation on a *raw* stream is a legal no-op, which is asserted too.
    #[test]
    fn inflate_validate_at_the_c_boundary() {
        assert_eq!(
            unsafe { inflateValidate(ptr::null_mut(), 1) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        let mut bare = zeroed_stream();
        assert_eq!(
            unsafe { inflateValidate(&mut bare, 1) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );

        // A zlib stream whose big-endian Adler-32 trailer has one flipped byte.
        let mut corrupt = ZLIB_STREAM.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;

        let decode = |check: Option<c_int>| -> (c_int, Vec<u8>) {
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
            if let Some(flag) = check {
                assert_eq!(
                    unsafe { inflateValidate(&mut strm, flag) },
                    Z_OK,
                    "inflateValidate is accepted on an initialized zlib stream"
                );
            }
            let mut out = vec![0u8; 512];
            strm.next_in = corrupt.as_ptr();
            strm.avail_in = corrupt.len() as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            let rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
            let produced = out.len() - strm.avail_out as usize;
            out.truncate(produced);
            assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
            (rc, out)
        };

        // Default: the trailer is verified, so the stream is rejected.
        let (rc, out) = decode(None);
        assert_eq!(
            rc, Z_DATA_ERROR,
            "a corrupted trailer is rejected by default"
        );
        assert_eq!(
            out, MSG,
            "the payload still decoded; only the trailer check failed"
        );

        // Validation off: the same bytes are accepted.
        let (rc, out) = decode(Some(0));
        assert_eq!(
            rc, Z_STREAM_END,
            "with validation off the corrupted trailer is accepted"
        );
        assert_eq!(out, MSG);

        // Validation explicitly on: rejected again, proving the toggle is live in
        // both directions rather than a one-way latch.
        let (rc, _) = decode(Some(1));
        assert_eq!(
            rc, Z_DATA_ERROR,
            "re-enabling validation restores the rejection"
        );

        // Toggling mid-stream, before the trailer is reached, still takes effect.
        {
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
            let mut head = [0u8; 8];
            strm.next_in = corrupt.as_ptr();
            strm.avail_in = corrupt.len() as c_uint;
            strm.next_out = head.as_mut_ptr();
            strm.avail_out = head.len() as c_uint;
            assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_OK);
            assert_eq!(unsafe { inflateValidate(&mut strm, 0) }, Z_OK);
            let mut rest = vec![0u8; 512];
            strm.next_out = rest.as_mut_ptr();
            strm.avail_out = rest.len() as c_uint;
            assert_eq!(
                unsafe { inflate(&mut strm, Z_NO_FLUSH) },
                Z_STREAM_END,
                "disabling validation mid-stream suppresses the trailer check"
            );
            assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        }

        // Raw streams have no wrapper, so enabling validation is a legal no-op
        // and decoding is unaffected.
        {
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
                Z_OK
            );
            assert_eq!(
                unsafe { inflateValidate(&mut strm, 1) },
                Z_OK,
                "enabling validation on a raw stream is accepted"
            );
            {
                let handle = unsafe { inflate_handle(&mut strm) }.expect("an inflate handle");
                let s = handle.zs.inflate_state().expect("engine state");
                assert_eq!(
                    s.wrap & 4,
                    0,
                    "a raw stream has no wrapper, so the check bit stays clear"
                );
            }
            let mut out = vec![0u8; 512];
            strm.next_in = RAW_STREAM.as_ptr();
            strm.avail_in = RAW_STREAM.len() as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
            let produced = out.len() - strm.avail_out as usize;
            assert_eq!(&out[..produced], MSG);
            assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        }

        // A deflate handle is rejected by the tag check.
        let mut def = zeroed_stream();
        install_a_deflate_handle(&mut def);
        assert_eq!(
            unsafe { inflateValidate(&mut def, 1) },
            Z_STREAM_ERROR,
            "a deflate handle is not an inflate handle"
        );
        assert_eq!(unsafe { crate::ffi::deflate::deflateEnd(&mut def) }, Z_OK);

        // After teardown the stream is stateless again.
        let mut spent = zeroed_stream();
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut spent,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        assert_eq!(unsafe { inflateEnd(&mut spent) }, Z_OK);
        assert_eq!(
            unsafe { inflateValidate(&mut spent, 1) },
            Z_STREAM_ERROR,
            "inflateEnd nulls `state`, so a later toggle is rejected"
        );
    }

    /// `int inflateUndermine(z_streamp, int)` at the C boundary
    /// (`ZLIB_1.2.3.4`).
    ///
    /// The realized engine, like a default-configured reference zlib, is built
    /// without `INFLATE_ALLOW_INVALID_DISTANCE_TOOFAR_ARRR`, so C's
    /// `inflateUndermine` (`inflate.c` L1370-L1383) forces `sane` back on and
    /// reports `Z_DATA_ERROR` to signal the leniency is unavailable. Both halves
    /// are asserted, for both `subvert` polarities: the exact code, and — through
    /// the handle — that `sane` really is left `true` rather than merely reported
    /// as such. Reporting success here would silently promise a decoder leniency
    /// the engine does not have.
    #[test]
    fn inflate_undermine_at_the_c_boundary() {
        assert_eq!(
            unsafe { inflateUndermine(ptr::null_mut(), 1) },
            Z_STREAM_ERROR,
            "a NULL z_streamp is Z_STREAM_ERROR"
        );

        let mut bare = zeroed_stream();
        assert_eq!(
            unsafe { inflateUndermine(&mut bare, 1) },
            Z_STREAM_ERROR,
            "an uninitialized z_stream is Z_STREAM_ERROR"
        );

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

        // Both polarities report Z_DATA_ERROR and both leave `sane` on.
        for subvert in [1, 0, -1] {
            assert_eq!(
                unsafe { inflateUndermine(&mut strm, subvert) },
                Z_DATA_ERROR,
                "undermining is unavailable in this build, whatever `subvert` is"
            );
            let handle = unsafe { inflate_handle(&mut strm) }.expect("an inflate handle");
            let s = handle.zs.inflate_state().expect("engine state");
            assert!(
                s.sane,
                "`sane` is forced back on, so distance checks stay enforced"
            );
        }

        // The rejection is advisory: the stream is untouched and still decodes.
        let mut out = vec![0u8; 512];
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
        let produced = out.len() - strm.avail_out as usize;
        assert_eq!(
            &out[..produced],
            MSG,
            "a refused undermine leaves the stream fully usable"
        );

        // A deflate handle is rejected before the engine is reached, so the code
        // is Z_STREAM_ERROR rather than Z_DATA_ERROR — the two are distinguished.
        let mut def = zeroed_stream();
        install_a_deflate_handle(&mut def);
        assert_eq!(
            unsafe { inflateUndermine(&mut def, 1) },
            Z_STREAM_ERROR,
            "a deflate handle is not an inflate handle"
        );
        assert_eq!(unsafe { crate::ffi::deflate::deflateEnd(&mut def) }, Z_OK);

        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert_eq!(
            unsafe { inflateUndermine(&mut strm, 1) },
            Z_STREAM_ERROR,
            "inflateEnd nulls `state`, so a later call is Z_STREAM_ERROR"
        );
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
            ulong_to_u32(strm.adler),
            expected,
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

    // =======================================================================
    // Owner-bound handles (S7-02) and validation-before-borrow (S7-01)
    // =======================================================================

    /// All three tagged handles share the same `#[repr(C)]` `(kind, owner)`
    /// prefix, at the same offsets, so `peek_handle_prefix` may read it through
    /// `HandleHeader` before the concrete handle type is known.
    ///
    /// `ffi::types` asserts this for `DeflateHandle`, which is the only handle
    /// declared there; the other two live here and are private to this module, so
    /// they are asserted here. A drift in either would let `handle_owner_valid`
    /// compare the wrong bytes and silently defeat both the kind tag and the owner
    /// check.
    #[test]
    fn every_tagged_handle_shares_the_kind_owner_prefix_layout() {
        use core::mem::offset_of;

        assert_eq!(offset_of!(InflateHandle, kind), 0);
        assert_eq!(offset_of!(InflateBackHandle, kind), 0);
        assert_eq!(
            offset_of!(InflateHandle, owner),
            size_of::<HandleKind>(),
            "`owner` must immediately follow `kind`, as it does in `HandleHeader`"
        );
        assert_eq!(
            offset_of!(InflateBackHandle, owner),
            size_of::<HandleKind>(),
            "`owner` must immediately follow `kind`, as it does in `HandleHeader`"
        );

        // The trait constants must agree with the tags the constructors write, or
        // a handle would fail its own kind check.
        assert_eq!(<InflateHandle as TaggedHandle>::KIND, HandleKind::INFLATE);
        assert_eq!(
            <InflateBackHandle as TaggedHandle>::KIND,
            HandleKind::INFLATE_BACK
        );
    }

    /// A byte-copied `z_stream` must not be able to drive or reclaim the
    /// original's inflate state.
    ///
    /// This is C's `state->strm != strm` clause (`inflate.c` L95). Without it the
    /// copy could reclaim and free the handle, leaving the original's `state`
    /// dangling — a use-after-free reachable from ordinary C usage. Reference zlib
    /// returns `Z_STREAM_ERROR` for the copy and `Z_OK` for the owner.
    #[test]
    fn a_byte_copied_z_stream_can_neither_drive_nor_reclaim_the_inflate_state() {
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
        assert!(!strm.state.is_null());

        // `z_stream copy = strm;` — the plain struct copy a C caller can write.
        // SAFETY: `z_stream` is a `#[repr(C)]` aggregate of `Copy` scalars and raw
        // pointers with no `Drop`, so a bitwise read is a valid duplicate.
        let mut copy = unsafe { core::ptr::read(&raw const strm) };
        assert!(core::ptr::eq(copy.state, strm.state));

        let mut out = vec![0u8; 512];
        copy.next_in = ZLIB_STREAM.as_ptr();
        copy.avail_in = ZLIB_STREAM.len() as c_uint;
        copy.next_out = out.as_mut_ptr();
        copy.avail_out = out.len() as c_uint;
        assert_eq!(
            unsafe { inflate(&mut copy, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "C's inflateStateCheck rejects a stream that does not own its state"
        );
        assert_eq!(unsafe { inflateReset(&mut copy) }, Z_STREAM_ERROR);
        assert_eq!(unsafe { inflateSync(&mut copy) }, Z_STREAM_ERROR);
        assert_eq!(
            unsafe { inflateEnd(&mut copy) },
            Z_STREAM_ERROR,
            "a wrong-owner inflateEnd must refuse rather than free"
        );
        assert!(
            !copy.state.is_null(),
            "a refused reclaim must leave the handle installed for its real owner"
        );

        // The owner is untouched and still decodes correctly.
        strm.next_in = ZLIB_STREAM.as_ptr();
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);
        let produced = out.len() - strm.avail_out as usize;
        assert_eq!(&out[..produced], MSG);
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
        assert!(strm.state.is_null());
    }

    /// A byte-copied `z_stream` must not be able to reclaim an `inflateBack`
    /// state either.
    ///
    /// `infback.c` records no owner, so C double-frees here (`inflateBackEnd`
    /// L573 checks only `strm`, `strm->state` and `zfree`). This port refuses the
    /// copy instead — the sole deliberate divergence in
    /// `inflate_back_end_state_check`, confined to an input on which reference
    /// zlib is undefined. The *decode* path deliberately keeps C's weaker
    /// predicate, which this test also pins.
    #[test]
    fn a_byte_copied_z_stream_cannot_reclaim_the_inflate_back_state() {
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

        // SAFETY: see `a_byte_copied_z_stream_can_neither_drive_nor_reclaim_the_inflate_state`.
        let mut copy = unsafe { core::ptr::read(&raw const strm) };

        // The decode path keeps C's weaker `infback.c` L208-L219 predicate, which
        // has no owner clause — so the copy is accepted here, exactly as C accepts
        // it. Driving it with a dry input callback is a complete, side-effect-free
        // way to observe that: it reaches the engine and reports a data error
        // rather than `Z_STREAM_ERROR`.
        let mut in_state = BackIn {
            data: &[],
            given: false,
        };
        let mut out_state = BackOut {
            collected: Vec::new(),
        };
        let rc = unsafe {
            inflateBack(
                &mut copy,
                Some(back_in),
                &raw mut in_state as *mut c_void,
                Some(back_out),
                &raw mut out_state as *mut c_void,
            )
        };
        assert_ne!(
            rc, Z_STREAM_ERROR,
            "inflateBack keeps C's weaker entry predicate, which has no owner \
             clause (infback.c never assigns state->strm)"
        );

        // The reclaim path does add the owner clause, because tolerating a
        // foreign stream there means freeing one allocation twice.
        assert_eq!(
            unsafe { inflateBackEnd(&mut copy) },
            Z_STREAM_ERROR,
            "a wrong-owner inflateBackEnd must refuse rather than double free"
        );
        assert!(!copy.state.is_null());

        // The owner still reclaims exactly once.
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
        assert!(strm.state.is_null());
    }

    /// `inflateBackEnd` refuses to reclaim while the caller's `zfree` hook is
    /// absent, exactly as C does.
    ///
    /// `infback.c` L573 tests `strm->zfree == (free_func)0` — and notably *not*
    /// `zalloc`, since the only remaining work is to free. Freeing anyway would
    /// release, through the global allocator, memory the caller's own hook owns.
    #[test]
    fn inflate_back_end_refuses_while_zfree_is_absent() {
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
        let published_zfree = strm.zfree;
        assert!(
            published_zfree.is_some(),
            "the init prologue must publish a zfree half"
        );

        strm.zfree = None;
        assert_eq!(
            unsafe { inflateBackEnd(&mut strm) },
            Z_STREAM_ERROR,
            "C's inflateBackEnd refuses when zfree is null (infback.c L573)"
        );
        assert!(
            !strm.state.is_null(),
            "a refused reclaim must leave the state installed"
        );

        // `zalloc` is deliberately NOT part of C's predicate here: clearing it
        // alone must still allow the reclaim to proceed.
        strm.zfree = published_zfree;
        strm.zalloc = None;
        assert_eq!(
            unsafe { inflateBackEnd(&mut strm) },
            Z_OK,
            "infback.c L573 checks zfree only, never zalloc"
        );
        assert!(strm.state.is_null());
    }

    /// `inflateCopy` re-points the clone's owner at `dest`, so each stream
    /// reclaims its own state and neither can free the other's.
    #[test]
    fn copy_binds_the_clone_to_dest_so_each_stream_reclaims_its_own_state() {
        let mut src = zeroed_stream();
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
        let mut dst = zeroed_stream();
        assert_eq!(unsafe { inflateCopy(&mut dst, &mut src) }, Z_OK);
        assert!(!dst.state.is_null());
        assert!(
            !core::ptr::eq(dst.state, src.state),
            "inflateCopy must install a distinct handle"
        );

        // Each stream decodes independently through its own owner-bound handle.
        for (label, strm) in [("clone", &mut dst), ("source", &mut src)] {
            let mut out = vec![0u8; 512];
            strm.next_in = ZLIB_STREAM.as_ptr();
            strm.avail_in = ZLIB_STREAM.len() as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            assert_eq!(
                unsafe { inflate(strm, Z_NO_FLUSH) },
                Z_STREAM_END,
                "the {label} must decode through its own owner-bound handle"
            );
            let produced = out.len() - strm.avail_out as usize;
            assert_eq!(&out[..produced], MSG, "{label} output");
        }

        assert_eq!(unsafe { inflateEnd(&mut dst) }, Z_OK);
        assert!(dst.state.is_null());
        assert_eq!(
            unsafe { inflateEnd(&mut src) },
            Z_OK,
            "ending the clone must not disturb the source's handle"
        );
        assert!(src.state.is_null());
    }

    /// A stateless stream is refused by every auxiliary-pointer entry point
    /// *without* the auxiliary pointer being bridged (S7-01).
    ///
    /// The pointers below are non-null and deliberately far smaller than the
    /// lengths claimed, so a shim that bridged them before validating would be
    /// constructing a slice or reference over memory it has no right to — which is
    /// undefined behavior at the moment of creation, regardless of the code the
    /// shim then returns.
    #[test]
    fn a_stateless_stream_is_refused_without_its_auxiliary_pointers_being_used() {
        let probe = [0xA5u8; 1];
        let probe_ptr = probe.as_ptr();

        let mut strm = zeroed_stream();
        assert!(strm.state.is_null());

        assert_eq!(
            unsafe { inflateSetDictionary(&mut strm, probe_ptr, 4096) },
            Z_STREAM_ERROR,
            "C validates the stream before it reads one dictionary byte"
        );
        assert_eq!(
            unsafe { inflateGetDictionary(&mut strm, probe_ptr.cast_mut(), ptr::null_mut()) },
            Z_STREAM_ERROR
        );

        strm.next_in = probe_ptr;
        strm.avail_in = 4096;
        strm.next_out = probe_ptr.cast_mut();
        strm.avail_out = 4096;
        assert_eq!(
            unsafe { inflate(&mut strm, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "C validates the stream before it bridges next_in/next_out"
        );
        assert_eq!(
            unsafe { inflateSync(&mut strm) },
            Z_STREAM_ERROR,
            "C validates the stream before it reaches strm->next_in"
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_STREAM_ERROR);

        // `inflateBack` must reject a stream that was never `inflateBackInit_`-ed
        // before it bridges the input window.
        let mut in_state = BackIn {
            data: &[],
            given: false,
        };
        let mut out_state = BackOut {
            collected: Vec::new(),
        };
        assert_eq!(
            unsafe {
                inflateBack(
                    &mut strm,
                    Some(back_in),
                    &raw mut in_state as *mut c_void,
                    Some(back_out),
                    &raw mut out_state as *mut c_void,
                )
            },
            Z_STREAM_ERROR,
            "infback.c L208-L219 rejects a null state before loading next/have"
        );

        // `inflateCopy` from a stateless source must not touch `dest`.
        let mut dst = zeroed_stream();
        dst.total_out = 0x5EED;
        assert_eq!(
            unsafe { inflateCopy(&mut dst, &mut strm) },
            Z_STREAM_ERROR,
            "C evaluates inflateStateCheck(source) before it looks at dest"
        );
        assert_eq!(
            dst.total_out, 0x5EED,
            "a refused inflateCopy must leave dest byte-for-byte untouched"
        );
        assert!(dst.state.is_null());
    }

    /// `inflateGetHeader` refuses an unvalidated stream without dereferencing the
    /// caller's `gz_header`.
    ///
    /// C writes `head->done = 0` only after `inflateStateCheck` and the
    /// `(state->wrap & 2)` test have both passed (`inflate.c` L1219-L1230), so a
    /// caller who pairs a stale header with an uninitialized stream must see
    /// `Z_STREAM_ERROR` and an untouched header.
    #[cfg(feature = "gzip")]
    #[test]
    fn get_header_refuses_a_stateless_stream_without_touching_the_header() {
        let mut strm = zeroed_stream();
        let mut head = zeroed_gz_header();
        head.done = 0x5EED;
        head.extra_max = 0;
        assert_eq!(
            unsafe { inflateGetHeader(&mut strm, &mut head) },
            Z_STREAM_ERROR,
            "C validates the stream before it writes head->done"
        );
        assert_eq!(
            head.done, 0x5EED,
            "a refused registration must leave the caller's header untouched"
        );
    }
    // -----------------------------------------------------------------------
    // Live borrowed gzip header sinks (`inflateGetHeader`)
    //
    // C's `inflateGetHeader` stores the caller's pointer and clears `done`
    // (`inflate.c` L1228-L1229) — nothing else. Every one of `extra`, `name`,
    // `comment`, `extra_max`, `name_max` and `comm_max` is then re-read from the
    // caller's struct on each stored byte (`inflate.c` L614-L621, L632-L637,
    // L654-L659), so a sink installed, resized or withdrawn after registration
    // takes effect, and the decoder allocates nothing for the header at all.
    // -----------------------------------------------------------------------

    /// A gzip stream carrying `FEXTRA`/`FNAME`/`FCOMMENT`, decoded with the
    /// caller's sinks installed **after** `inflateGetHeader` returned.
    ///
    /// A registration-time snapshot of the pointers records three null sinks and
    /// three zero capacities and therefore captures nothing; reference C fills all
    /// three fields, because it looks at the caller's struct only while parsing.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_honors_sinks_installed_after_registration() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);

        // Registered with no sinks whatsoever: null pointers, zero capacities.
        let mut hdr = PoisonedHeader::new(0, 0, 0);
        assert!(hdr.head.extra.is_null() && hdr.head.name.is_null());

        let mut extra = vec![HDR_POISON; 16];
        let mut name = vec![HDR_POISON; 16];
        let mut comment = vec![HDR_POISON; 16];

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
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut hdr.head) }, Z_OK);

        // Only *now* does the caller publish its buffers — after registration and
        // before a single byte has been parsed.
        hdr.head.extra = extra.as_mut_ptr();
        hdr.head.extra_max = extra.len() as c_uint;
        hdr.head.name = name.as_mut_ptr();
        hdr.head.name_max = name.len() as c_uint;
        hdr.head.comment = comment.as_mut_ptr();
        hdr.head.comm_max = comment.len() as c_uint;

        let mut out = vec![0u8; stream.len() * 8 + 512];
        strm.next_in = stream.as_ptr();
        strm.avail_in = stream.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);

        assert_eq!(hdr.head.done, 1, "the header completed");
        assert_eq!(
            hdr.head.extra_len as usize,
            HDR_EXTRA.len(),
            "the declared XLEN is published regardless of the sink"
        );
        assert_eq!(
            &extra[..HDR_EXTRA.len()],
            HDR_EXTRA,
            "a sink installed after registration must still receive the extra field"
        );
        assert_eq!(&name[..HDR_NAME.len()], HDR_NAME, "…and the name");
        assert_eq!(name[HDR_NAME.len()], 0, "…NUL-terminated, as C stores it");
        assert_eq!(
            &comment[..HDR_COMMENT.len()],
            HDR_COMMENT,
            "…and the comment"
        );
        assert_eq!(comment[HDR_COMMENT.len()], 0, "…NUL-terminated");
    }

    /// A capacity reduced **after** registration truncates against the new value.
    ///
    /// C's store guard is `len < state->head->extra_max`, re-read from the
    /// caller's struct on every pass (`inflate.c` L616-L618), so shrinking
    /// `extra_max` mid-decode is honored. Feeding one byte per call gives the
    /// caller a window in which to change it.
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_truncates_against_the_live_capacity() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);
        // Registered with room for the whole extra field…
        let mut hdr = PoisonedHeader::new(16, 16, 16);

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
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut hdr.head) }, Z_OK);

        let mut out = vec![0u8; stream.len() * 8 + 512];
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        let mut shrunk = false;
        let mut off = 0usize;
        let mut rc = Z_OK;
        while off < stream.len() && rc == Z_OK {
            // …shrunk to 3 the instant the declared XLEN becomes visible, which is
            // before any extra byte has been stored.
            if !shrunk && hdr.head.extra_len as usize == HDR_EXTRA.len() {
                hdr.head.extra_max = 3;
                shrunk = true;
            }
            strm.next_in = stream[off..].as_ptr();
            strm.avail_in = 1;
            off += 1;
            rc = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
            if hdr.head.done == 1 {
                break;
            }
        }
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);

        assert!(shrunk, "the declared XLEN must become visible mid-decode");
        assert_eq!(hdr.head.done, 1, "the header completed");
        assert_eq!(
            hdr.head.extra_len as usize,
            HDR_EXTRA.len(),
            "the declared length still reports the full field"
        );
        assert_eq!(
            &hdr.extra[..3],
            &HDR_EXTRA[..3],
            "exactly the live capacity is filled"
        );
        assert_eq!(
            &hdr.extra[3..],
            &vec![HDR_POISON; hdr.extra.len() - 3][..],
            "nothing may be written beyond the live capacity"
        );
    }

    /// The decoder accumulates **nothing** of its own for the header: every byte
    /// is written straight into the caller's buffers.
    ///
    /// The engine-side header carries only the scalars; its `extra`/`name`/
    /// `comment` slots — the sole allocating parts of the old capture model —
    /// stay `None` for the whole decode. That is what removes the `Z_MEM_ERROR`
    /// this path could once report while C cannot (AAP §0.6.5).
    #[test]
    #[cfg(feature = "gzip")]
    fn get_header_accumulates_nothing_in_the_engines_owned_vectors() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);
        // Capacities far larger than the fields, so an accumulating
        // implementation would definitely have grown its vectors.
        let mut hdr = PoisonedHeader::new(4096, 4096, 4096);

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
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut hdr.head) }, Z_OK);

        let mut out = vec![0u8; stream.len() * 8 + 512];
        strm.next_in = stream.as_ptr();
        strm.avail_in = stream.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_FINISH) }, Z_STREAM_END);

        // The caller's buffers hold the fields…
        assert_eq!(hdr.head.done, 1);
        assert_eq!(&hdr.name[..HDR_NAME.len()], HDR_NAME);
        assert_eq!(&hdr.comment[..HDR_COMMENT.len()], HDR_COMMENT);
        assert_eq!(&hdr.extra[..HDR_EXTRA.len()], HDR_EXTRA);

        // …while the engine grew nothing.
        // SAFETY: `strm` is a live, validated inflate stream owned by this test.
        let handle = unsafe { inflate_handle(&mut strm) }.expect("inflate handle");
        let state = handle.zs.inflate_state().expect("inflate state");
        assert!(state.head_foreign, "the C ABI registers a foreign header");
        let gh = state.head.as_ref().expect("a header is registered");
        assert!(
            gh.extra.is_none() && gh.name.is_none() && gh.comment.is_none(),
            "no header payload may be accumulated engine-side"
        );
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// `inflateReset` drops the registration, exactly as C's `state->head =
    /// Z_NULL` in `inflateResetKeep` does (`inflate.c` L115).
    ///
    /// This is load-bearing for safety as well as fidelity: once the registration
    /// is gone the caller is entitled to free its `gz_header`, so no later call
    /// may consult it.
    #[test]
    #[cfg(feature = "gzip")]
    fn inflate_reset_drops_the_foreign_header_registration() {
        let stream = gzip_stream_with_fields(HeaderFields::ALL);
        let mut hdr = PoisonedHeader::new(16, 16, 16);

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
        assert_eq!(unsafe { inflateGetHeader(&mut strm, &mut hdr.head) }, Z_OK);
        assert_eq!(unsafe { inflateReset(&mut strm) }, Z_OK);

        // SAFETY: `strm` is a live, validated inflate stream owned by this test.
        {
            let handle = unsafe { inflate_handle(&mut strm) }.expect("inflate handle");
            let state = handle.zs.inflate_state().expect("inflate state");
            assert!(state.head.is_none(), "reset clears the registration");
            assert!(
                !state.head_foreign,
                "reset must clear the foreign-ownership marker with it"
            );
        }

        let mut out = vec![0u8; stream.len() * 8 + 512];
        strm.next_in = stream.as_ptr();
        strm.avail_in = stream.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        assert_eq!(unsafe { inflate(&mut strm, Z_FINISH) }, Z_STREAM_END);
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);

        assert_eq!(
            hdr.head.done, 0,
            "a de-registered header must not be written to"
        );
        assert_eq!(
            hdr.head.extra_len,
            PoisonedHeader::XLEN,
            "…nor may its declared length be republished"
        );
        assert!(
            hdr.name.iter().all(|&b| b == HDR_POISON),
            "…nor may its buffers be touched"
        );
    }

    // -----------------------------------------------------------------------
    // Reset publication (`inflate.c` L100-L134) and the `inflate_fast`
    // give-back (`inffast.c` L282-L299) as a C caller observes them.
    // -----------------------------------------------------------------------

    /// Every reset variant publishes C's cleared `data_type` into the *caller's*
    /// `z_stream`.
    ///
    /// `inflate.c` L107 clears `strm->data_type` inside `inflateResetKeep`, in the
    /// same group of assignments as `total_in`, `total_out` and `msg`, and all
    /// three public reset entry points funnel through it (`inflateReset`
    /// L125-L134, `inflateReset2` L136-L177). The engine clearing its own
    /// `ZStream` is not sufficient: unless the shim mirrors the value out, the
    /// caller keeps reading the decode-state word left over from the previous
    /// stream — `128` (mode == TYPE) after a clean stop.
    ///
    /// The `assert_ne!` before each reset is a deliberate non-vacuity guard: it
    /// fails if the fixture ever stops leaving something to clear.
    #[test]
    fn every_reset_variant_publishes_the_cleared_data_type() {
        type Reset = fn(&mut z_stream) -> c_int;
        fn via_reset(s: &mut z_stream) -> c_int {
            // SAFETY: `s` is a live inflate stream owned by this test.
            unsafe { inflateReset(s) }
        }
        fn via_reset2(s: &mut z_stream) -> c_int {
            // SAFETY: as above; 15 keeps the existing zlib wrap and window size.
            unsafe { inflateReset2(s, 15) }
        }
        fn via_reset_keep(s: &mut z_stream) -> c_int {
            // SAFETY: as above.
            unsafe { inflateResetKeep(s) }
        }
        let variants: [(&str, Reset); 3] = [
            ("inflateReset", via_reset),
            ("inflateReset2", via_reset2),
            ("inflateResetKeep", via_reset_keep),
        ];

        for (label, reset) in variants {
            let mut strm = zeroed_stream();
            assert_eq!(
                // SAFETY: `strm` is a valid zeroed `z_stream` owned by this test.
                unsafe {
                    inflateInit2_(
                        &mut strm,
                        15,
                        VERSION.as_ptr(),
                        size_of::<z_stream>() as c_int,
                    )
                },
                Z_OK,
                "{label}: init"
            );

            let mut out = vec![0u8; 512];
            strm.next_in = ZLIB_STREAM.as_ptr();
            strm.avail_in = ZLIB_STREAM.len() as c_uint;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as c_uint;
            // `Z_BLOCK` leaves at the block boundary with mode == TYPE, so C's
            // formula (`inflate.c` L1147-L1149) sets bit 128.
            // SAFETY: both cursors point at live buffers sized by the fields above.
            let rc = unsafe { inflate(&mut strm, Z_BLOCK) };
            assert_eq!(rc, Z_OK, "{label}: Z_BLOCK stops at the block boundary");
            assert_ne!(
                strm.data_type, 0,
                "{label}: the fixture must leave a nonzero data_type to clear"
            );

            assert_eq!(reset(&mut strm), Z_OK, "{label}: reset");
            assert_eq!(
                strm.data_type, 0,
                "{label}: C clears the caller's data_type (inflate.c L107)"
            );
            assert_eq!(strm.total_in, 0, "{label}: ...alongside total_in");
            assert_eq!(strm.total_out, 0, "{label}: ...and total_out");
            assert!(strm.msg.is_null(), "{label}: ...and msg");

            // SAFETY: `strm` still owns its state; this is the single reclaim.
            assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK, "{label}: end");
        }
    }

    /// A reset after a *failed* decode clears the published `data_type` too.
    ///
    /// The failure path is the interesting one: C's `inf_leave` publishes
    /// `data_type` from the aborted state, so the caller is left holding a bit
    /// count that no longer describes anything. `inflateResetKeep` clears it
    /// together with `msg` (`inflate.c` L106-L107), which is what lets the
    /// canonical "reset and retry" idiom start from a clean slate.
    #[test]
    fn a_reset_after_a_failed_decode_clears_the_published_data_type_and_msg() {
        // Corrupt FLG so CMF*256+FLG is no longer a multiple of 31: C reports
        // "incorrect header check" with 16 bits still buffered.
        let mut corrupt = ZLIB_STREAM.to_vec();
        corrupt[1] = corrupt[1].wrapping_add(1);

        let mut strm = zeroed_stream();
        assert_eq!(
            // SAFETY: `strm` is a valid zeroed `z_stream` owned by this test.
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
        let mut out = vec![0u8; 512];
        strm.next_in = corrupt.as_ptr();
        strm.avail_in = corrupt.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: both cursors point at live buffers sized by the fields above.
        assert_eq!(
            unsafe { inflate(&mut strm, Z_NO_FLUSH) },
            ReturnCode::DataError.as_c_int(),
            "a bad FCHECK must be rejected"
        );
        assert!(!strm.msg.is_null(), "C publishes a diagnostic here");
        assert_ne!(
            strm.data_type, 0,
            "the aborted state must leave a nonzero data_type to clear"
        );

        // SAFETY: `strm` is a live inflate stream owned by this test.
        assert_eq!(unsafe { inflateReset(&mut strm) }, Z_OK);
        assert_eq!(
            strm.data_type, 0,
            "the reset must clear the error residue, not preserve it"
        );
        assert!(strm.msg.is_null(), "...and the diagnostic with it");

        // SAFETY: `strm` still owns its state; this is the single reclaim.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// Bytes buffered by `inflatePrime` are handed back exactly as C hands them
    /// back — including past the start of the slice the caller supplied.
    ///
    /// `inffast.c` L290-L294 returns whole buffered bytes with no lower bound:
    /// `len = bits >> 3; in -= len; bits -= len << 3;`. The comment justifying it
    /// ("on entry, bits < 8, so in won't go too far back") does not hold when the
    /// caller primed whole bytes, so `strm->next_in` legitimately ends up *before*
    /// the pointer it was handed, `avail_in` ends up larger than it was given, and
    /// the `uInt` per-call delta wraps `total_in` at 2^32.
    ///
    /// The fixture primes the entire three-byte block plus one trailing byte, so
    /// `inflate_fast` decodes wholly out of the accumulator and pulls nothing from
    /// the slice — leaving a byte of debt that no in-slice index can absorb. Every
    /// number below was read off reference zlib built from this repository's own C
    /// sources; `data_type == 198` is `6` bits buffered `+ 64` (last block)
    /// `+ 128` (mode == TYPE) per `inflate.c` L1147-L1149.
    #[test]
    fn primed_bytes_are_given_back_exactly_as_c_gives_them_back() {
        use crate::ffi::deflate::{deflate, deflateEnd, deflateInit2_};

        // A raw fixed-Huffman block holding the single literal 'Q'.
        let mut blk = [0u8; 64];
        let payload = *b"Q";
        let mut d = zeroed_stream();
        assert_eq!(
            // SAFETY: `d` is a valid zeroed `z_stream` owned by this test.
            unsafe {
                deflateInit2_(
                    &mut d,
                    6,
                    crate::constants::Z_DEFLATED,
                    -15,
                    8,
                    crate::constants::Z_FIXED,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        d.next_in = payload.as_ptr();
        d.avail_in = payload.len() as c_uint;
        d.next_out = blk.as_mut_ptr();
        d.avail_out = blk.len() as c_uint;
        // SAFETY: both cursors point at live buffers sized by the fields above.
        let drc = unsafe { deflate(&mut d, Z_FINISH) };
        let blen = blk.len() - d.avail_out as usize;
        // SAFETY: `d` still owns its state; this is the single reclaim.
        assert_eq!(unsafe { deflateEnd(&mut d) }, Z_OK);
        assert_eq!(drc, Z_STREAM_END, "the whole block must be emitted");
        assert_eq!(blen, 3, "the fixture depends on a three-byte fixed block");

        // The caller's contiguous buffer: the block, then trailing padding. The
        // slice handed to `inflate` starts after every primed byte, so one primed
        // byte lies past the end of the block.
        let mut whole = Vec::with_capacity(blen + 16);
        whole.extend_from_slice(&blk[..blen]);
        whole.extend_from_slice(&[0u8; 16]);
        let pre = blen + 1; // 32 bits — exactly `inflatePrime`'s ceiling.

        let mut strm = zeroed_stream();
        assert_eq!(
            // SAFETY: `strm` is a valid zeroed `z_stream` owned by this test.
            unsafe {
                inflateInit2_(
                    &mut strm,
                    -15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        for (k, &byte) in whole[..pre].iter().enumerate() {
            assert_eq!(
                // SAFETY: `strm` is a live inflate stream owned by this test.
                unsafe { inflatePrime(&mut strm, 8, c_int::from(byte)) },
                Z_OK,
                "priming byte {k}"
            );
        }

        let mut out = vec![0u8; 1024];
        // SAFETY: `pre <= whole.len()`, so the offset is in bounds (one past the
        // end would also be sound, and is not reached here).
        let base = unsafe { whole.as_ptr().add(pre) };
        let avail = whole.len() - pre;
        strm.next_in = base;
        strm.avail_in = avail as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: both cursors point at live buffers sized by the fields above.
        let rc = unsafe { inflate(&mut strm, Z_BLOCK) };
        assert_eq!(rc, Z_OK, "Z_BLOCK stops at the boundary after the block");

        let produced = out.len() - strm.avail_out as usize;
        assert_eq!(&out[..produced], b"Q", "the literal must decode");
        assert_eq!(strm.total_out, 1, "one byte produced");

        let delta = (strm.next_in as isize) - (base as isize);
        assert_eq!(
            delta, -1,
            "the returned byte must move next_in behind the slice, as C does"
        );
        assert_eq!(
            strm.avail_in as usize,
            avail + 1,
            "...and grow avail_in past the value the caller supplied"
        );
        assert_eq!(
            strm.total_in,
            c_ulong::from(u32::MAX),
            "C forms the per-call delta in uInt before widening it, so the give-back \
             wraps total_in at 2^32 — not at 2^64"
        );
        assert_eq!(
            strm.data_type, 198,
            "6 bits buffered + 64 (last) + 128 (mode == TYPE), per inflate.c L1147"
        );

        // SAFETY: `strm` still owns its state; this is the single reclaim.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }

    /// A stream that pulls its own input keeps `total_in` monotonic — the
    /// give-back settlement must not perturb the ordinary path.
    ///
    /// This is the counterpart to
    /// [`primed_bytes_are_given_back_exactly_as_c_gives_them_back`]: with nothing
    /// primed, the debt is always zero and every published field must match a
    /// plain accumulation.
    #[test]
    fn an_unprimed_decode_publishes_a_plain_input_accumulation() {
        let mut strm = zeroed_stream();
        assert_eq!(
            // SAFETY: `strm` is a valid zeroed `z_stream` owned by this test.
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
        let mut out = vec![0u8; 512];
        let base = ZLIB_STREAM.as_ptr();
        strm.next_in = base;
        strm.avail_in = ZLIB_STREAM.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: both cursors point at live buffers sized by the fields above.
        assert_eq!(unsafe { inflate(&mut strm, Z_NO_FLUSH) }, Z_STREAM_END);

        let delta = (strm.next_in as isize) - (base as isize);
        assert_eq!(
            delta as usize,
            ZLIB_STREAM.len(),
            "the whole stream is consumed"
        );
        assert_eq!(strm.avail_in, 0, "and nothing is handed back");
        assert_eq!(
            strm.total_in,
            ZLIB_STREAM.len() as c_ulong,
            "total_in is a plain accumulation when no byte was primed"
        );
        assert_eq!(&out[..strm.total_out as usize], MSG);

        // SAFETY: `strm` still owns its state; this is the single reclaim.
        assert_eq!(unsafe { inflateEnd(&mut strm) }, Z_OK);
    }
    /// `inflateBack` must publish C's exact diagnostic into `strm.msg` for every
    /// error site, and must clear the field on the paths where C clears it
    /// (M4-01).
    ///
    /// The field is planted with a sentinel first, so a shim that simply never
    /// wrote it — the pre-remediation behaviour — fails on every row rather than
    /// passing vacuously on the ones whose expected value happens to be null.
    #[test]
    fn inflate_back_publishes_cs_exact_diagnostic() {
        /// `(raw DEFLATE, expected return code, expected `strm.msg`)` — the error
        /// rows are `test/infcover.c` L584-L598 and the C strings are
        /// `infback.c` L256-L518.
        const CASES: &[(&[u8], c_int, Option<&str>)] = &[
            (
                &[0x00, 0x00, 0x00, 0x00, 0x00],
                Z_DATA_ERROR,
                Some("invalid stored block lengths"),
            ),
            (&[0x06], Z_DATA_ERROR, Some("invalid block type")),
            (
                &[0xfc, 0x00, 0x00],
                Z_DATA_ERROR,
                Some("too many length or distance symbols"),
            ),
            (
                &[0x04, 0x00, 0xfe, 0xff],
                Z_DATA_ERROR,
                Some("invalid code lengths set"),
            ),
            (
                &[0x04, 0x00, 0x24, 0x49, 0x00],
                Z_DATA_ERROR,
                Some("invalid bit length repeat"),
            ),
            (
                &[0x04, 0x00, 0x24, 0xe9, 0xff, 0x6d],
                Z_DATA_ERROR,
                Some("invalid code -- missing end-of-block"),
            ),
            (
                &[0x02, 0x7e, 0xff, 0xff],
                Z_DATA_ERROR,
                Some("invalid distance code"),
            ),
            (
                &[
                    0x0c, 0xc0, 0x81, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0xff, 0x6b, 0x04, 0x00,
                ],
                Z_DATA_ERROR,
                Some("invalid distance too far back"),
            ),
            // Well-formed: C clears `strm->msg` at `infback.c` L214 and assigns
            // nothing, so the planted sentinel must be gone.
            (&[0x03, 0x00], Z_STREAM_END, None),
            (&[0x01, 0x01, 0x00, 0xfe, 0xff, 0x00], Z_STREAM_END, None),
            // Truncated: `Z_BUF_ERROR` carries no diagnostic in C either.
            (&[0x04], Z_BUF_ERROR, None),
        ];

        const PLANTED: &CStr = c"planted sentinel";

        for (bytes, want_code, want_msg) in CASES {
            let mut window = [0u8; 1 << 15];
            let mut strm = zeroed_stream();
            // SAFETY: `strm` and `window` are live locals of the right size.
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
            strm.msg = PLANTED.as_ptr() as *mut c_char;
            strm.next_in = bytes.as_ptr();
            strm.avail_in = bytes.len() as c_uint;
            let mut sink = BackOut {
                collected: Vec::new(),
            };
            // A provider that immediately reports dry: everything is pre-loaded at
            // `next_in`, exactly as `infcover.c`'s `pull(desc == Z_NULL)` does.
            let mut dry = BackIn {
                data: &[],
                given: true,
            };
            // SAFETY: both callbacks match the zlib `in_func`/`out_func` ABI and
            // each descriptor points at the matching live local.
            let code = unsafe {
                inflateBack(
                    &mut strm,
                    Some(back_in),
                    (&mut dry) as *mut BackIn as *mut c_void,
                    Some(back_out),
                    (&mut sink) as *mut BackOut as *mut c_void,
                )
            };
            assert_eq!(code, *want_code, "return code for {want_msg:?}");
            match want_msg {
                Some(expected) => {
                    assert!(!strm.msg.is_null(), "{expected:?} must publish a message");
                    // SAFETY: a non-null `msg` is one of the crate's `'static`
                    // `c"…"` literals, hence NUL-terminated and readable.
                    let got = unsafe { CStr::from_ptr(strm.msg) };
                    assert_eq!(
                        got.to_str().expect("diagnostics are ASCII"),
                        *expected,
                        "diagnostic text"
                    );
                }
                None => assert!(
                    strm.msg.is_null(),
                    "C clears strm->msg here, so the planted sentinel must be gone"
                ),
            }
            // SAFETY: `strm` still owns the handle installed above.
            assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
        }
    }

    /// A rejected `inflateBack` must leave `strm.msg` exactly as the caller left
    /// it, because C's `strm->state == Z_NULL` return at `infback.c` L209-L210
    /// happens *before* the `strm->msg = Z_NULL` at L214 (M4-01).
    #[test]
    fn a_rejected_inflate_back_leaves_the_callers_message_alone() {
        const PLANTED: &CStr = c"survives rejection";

        // Never `inflateBackInit_`-ed: the stand-in for C's null `strm->state`.
        let mut strm = zeroed_stream();
        strm.msg = PLANTED.as_ptr() as *mut c_char;
        let mut sink = BackOut {
            collected: Vec::new(),
        };
        let mut dry = BackIn {
            data: &[],
            given: true,
        };
        // SAFETY: `strm` is a live zeroed stream; the callbacks and descriptors are
        // valid. The shim rejects on the null `state` before touching anything.
        let code = unsafe {
            inflateBack(
                &mut strm,
                Some(back_in),
                (&mut dry) as *mut BackIn as *mut c_void,
                Some(back_out),
                (&mut sink) as *mut BackOut as *mut c_void,
            )
        };
        assert_eq!(code, Z_STREAM_ERROR);
        assert_eq!(
            strm.msg as *const c_char,
            PLANTED.as_ptr(),
            "a rejected call must not clear a diagnostic C preserves"
        );

        // The same clause with a WRONG-kind handle: an `inflateInit2_` stream is
        // not an `inflateBack` handle, and the rejection is equally silent.
        let mut other = zeroed_stream();
        // SAFETY: standard init of a live stream.
        assert_eq!(
            unsafe {
                inflateInit2_(
                    &mut other,
                    15,
                    VERSION.as_ptr(),
                    size_of::<z_stream>() as c_int,
                )
            },
            Z_OK
        );
        other.msg = PLANTED.as_ptr() as *mut c_char;
        // SAFETY: `other` holds a live INFLATE handle, not an INFLATE_BACK one.
        let code = unsafe {
            inflateBack(
                &mut other,
                Some(back_in),
                (&mut dry) as *mut BackIn as *mut c_void,
                Some(back_out),
                (&mut sink) as *mut BackOut as *mut c_void,
            )
        };
        assert_eq!(code, Z_STREAM_ERROR);
        assert_eq!(
            other.msg as *const c_char,
            PLANTED.as_ptr(),
            "a cross-kind rejection must not clear the diagnostic either"
        );
        // SAFETY: reclaim the inflate handle exactly once.
        assert_eq!(unsafe { inflateEnd(&mut other) }, Z_OK);
    }

    /// `inflateBackInit_` must leave the caller's window byte-for-byte unchanged,
    /// on both the accepting and the refusing path (M6-09).
    ///
    /// C's `infback.c` L60 is a bare `state->window = window;`: it stores the
    /// pointer and writes nothing. And because the single state `ZALLOC` at
    /// L51-L53 runs *first*, a refused allocation returns `Z_MEM_ERROR` having
    /// never reached the window at all — so a caller that pre-fills the buffer and
    /// then hits an exhausted allocator finds its own bytes intact.
    #[test]
    fn back_init_never_writes_the_callers_window() {
        use crate::ffi::alloc::test_hook::HookStats;

        const FILL: u8 = 0xC3;

        // --- accepting path -------------------------------------------------
        let mut window = vec![FILL; 1 << 15];
        let mut strm = zeroed_stream();
        // SAFETY: `strm` and the window are live and correctly sized.
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
        assert!(
            window.iter().all(|&b| b == FILL),
            "a successful inflateBackInit_ adopts the window without writing it"
        );
        // SAFETY: single reclaim of the handle just installed.
        assert_eq!(unsafe { inflateBackEnd(&mut strm) }, Z_OK);
        assert!(
            window.iter().all(|&b| b == FILL),
            "inflateBackEnd frees only the state (infback.c L572-L577)"
        );

        // --- refusing path: C's single ZALLOC fails before L60 ---------------
        let stats = HookStats::with_budget(0);
        let hook = stats.hook();
        let mut window2 = vec![FILL; 1 << 15];
        let mut strm2 = zeroed_stream();
        strm2.zalloc = hook.zalloc();
        strm2.zfree = hook.zfree();
        strm2.opaque = hook.opaque();
        // SAFETY: `strm2` is live; the hook pair is the test allocator's.
        let code = unsafe {
            inflateBackInit_(
                &mut strm2,
                15,
                window2.as_mut_ptr(),
                VERSION.as_ptr(),
                size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(
            code, Z_MEM_ERROR,
            "a refused state request is C's Z_MEM_ERROR (infback.c L52-L53)"
        );
        assert_eq!(
            stats.ooms(),
            1,
            "exactly C's one state request was attempted, and it was refused"
        );
        assert_eq!(
            stats.allocs(),
            0,
            "nothing may be allocated once the state request fails -- C makes no \
             second request (infback.c has exactly one ZALLOC)"
        );
        assert!(
            window2.iter().all(|&b| b == FILL),
            "a failing allocation must not mutate memory C leaves untouched"
        );
        assert!(
            strm2.state.is_null(),
            "no handle may be installed on the failing path"
        );
    }
}
