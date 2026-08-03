//! Inflate error/edge-branch coverage — a Rust port of zlib's `test/infcover.c`.
//!
//! `test/infcover.c` is zlib's exhaustive *decoder* coverage harness. It feeds a
//! large table of hand-crafted, mostly-malformed DEFLATE / zlib / gzip byte
//! streams into the inflate engine and pins each one to an exact outcome,
//! driving the state machine through all 30+ inflate modes and every error
//! branch (AAP §0.6.1, §0.6.7). This file reproduces that harness against the
//! `zlib-rs` public API.
//!
//! ## Routing (idiomatic API first, FFI where required)
//!
//! Wherever a coverage operation is expressible through the safe, idiomatic
//! [`zlib_rs::inflate`] API it is used directly. A handful of `infcover.c`
//! scenarios exercise the raw C ABI (passing a NULL `z_stream`, supplying a
//! bogus version string) which the idiomatic API cannot express; those go
//! through the [`zlib_rs::ffi`] `extern "C"` shims. Every `unsafe` block is at
//! that FFI boundary and carries a `// SAFETY:` note (AAP §0.6.2).
//!
//! ## Direct `inflate_table` branch coverage (extending `cover_trees`)
//!
//! `infcover.c`'s `cover_trees` calls the table builder *directly*, because — as
//! its own comment states — that is the only way to "manifest not-enough errors,
//! since zlib insures that enough is always enough" through a byte stream. Having
//! established the technique, C then uses it for exactly one branch.
//! [`cover_trees`] ports that verbatim, and the `inflate_table_*` tests beside it
//! extend the same direct-call technique to every *remaining* exit of the
//! validation prologue at `inftrees.c` L121-L147:
//!
//! * over-subscription (`left < 0`, C `-1`) — [`inflate_table_rejects_over_subscribed`];
//! * an incomplete set (`left > 0`, C `-1`) for **both** halves of C's
//!   `(type == CODES || max != 1)` disjunction —
//!   [`inflate_table_rejects_incomplete_set`];
//! * the deliberate `max == 1` incomplete *acceptance* (C `0`) —
//!   [`inflate_table_allows_incomplete_single_distance_code`];
//! * the `max == 0` "no symbols to code at all" degenerate table (C `0` after two
//!   invalid-code markers) —
//!   [`inflate_table_all_zero_lengths_builds_invalid_marker`].
//!
//! [`enough_bounds_match_c`] pins the `ENOUGH_LENS`/`ENOUGH_DISTS`/`ENOUGH` arena
//! bounds that the guard `cover_trees` trips compares against (`inftrees.h`
//! L49-L51, AAP §0.6.6). Because an integration test is compiled as a separate
//! crate, these cases additionally prove the builder, its error discriminants and
//! its bounds are all reachable through this crate's *public* surface — the very
//! property `infcover.c` relies on when it reaches for `inflate_table` itself.
//!
//! ## Mid-decode `inflateCopy` — the offset-based table references
//!
//! C's `inflateCopy` must re-base three interior pointers (`state->next`,
//! `lencode`, `distcode`) that point *into* `state->codes[]`; the port stores
//! them as integer offsets plus a table-source discriminant instead, so a deep
//! clone is correct with no fix-up at all (AAP §0.6.3). The `inf()` driver below
//! copies a live stream on every iteration but releases the copy immediately, so
//! it exercises only that a copy can be taken and dropped.
//! [`inflate_copy_mid_decode_resumes_identically`] carries the stronger
//! guarantee: it copies a stream *after* its dynamic Huffman tables have been
//! built — observed through the public [`inflate_codes_used`], never by reading
//! private state — then finishes the decode through the original **and** the copy
//! and requires both to recover the input byte-for-byte. A port that copied the
//! state the way C `memcpy`s it, without re-basing, would leave the copy's table
//! references dangling and fail precisely here.
//!
//! ## Forced `Z_MEM_ERROR` (reproduced via the FFI allocator hooks)
//!
//! `infcover.c` forces `Z_MEM_ERROR` by installing a byte-capped allocator
//! through the `z_stream` `zalloc`/`zfree` hooks. `zlib-rs` allocation is
//! *fallible* at the FFI boundary (AAP §0.6.3): a caller hook that returns null
//! propagates to `MemError` with **no** global-allocator fallback. That harness
//! is reproduced here — see [`mem_limit_forces_mem_error`], which caps the byte
//! budget to force `Z_MEM_ERROR` on **both** inflate allocations that C routes
//! through the hook: the state struct reserved by `inflateInit2_` (a budget
//! below the state size fails the init) and the lazily-allocated window (a
//! budget covering the state but not the window fails the subsequent
//! `inflate`), exactly as in C.
//!
//! C reaches that same lazily-allocated window through a *third* entry point —
//! `inflateSetDictionary`, which loads the dictionary via `updatewindow` — and
//! pins `Z_MEM_ERROR` there too, then resumes from the failure by poking the
//! private `state->mode`.
//! [`set_dictionary_window_allocation_failure_is_a_mem_error`] ports that leg in
//! full: the refusal itself, the latched `MEM` mode the poke exists to undo, and
//! the accept-and-decode path on a stream that reaches `DICT` legitimately.
//!
//! ## Honestly-handled gap (never faked)
//!
//! One `infcover.c` behaviour reaches into private engine internals and is *not*
//! reachable from an integration test over the public API. It is handled openly
//! rather than by forging private access with `unsafe`:
//!
//! * **Forced inflateBack mode error** — `infcover.c` makes its `pull` callback
//!   poke `((inflate_state*)strm.state)->mode = SYNC`, an otherwise-impossible
//!   internal state, to force `Z_STREAM_ERROR`. The idiomatic [`InFunc`] trait
//!   only yields input bytes and cannot touch private state; equivalent coverage
//!   of that guard lives as a unit test in `src/inflate/back.rs`.
//!
//! ## Rust-native allocator accounting (the [`Allocator`] trait)
//!
//! `infcover.c` can only reach the allocator through the C `zalloc`/`zfree`
//! pointers, because that is the only allocation customization point C has. The
//! Rust port additionally exposes the safe [`zlib_rs::stream::Allocator`] trait,
//! and it must be a *real* customization point rather than a decorative one: a
//! downstream implementation has to observe every engine request and be able to
//! refuse it. That claim is pinned by
//! [`external_allocator_observes_every_deflate_request`],
//! [`external_allocator_refusal_fails_deflate_init`],
//! [`external_allocator_refusal_fails_inflate_init_then_window`], and
//! [`external_allocator_round_trip_is_byte_identical`], which drive the engines
//! through an allocator built **only** from this crate's public, safe surface —
//! no `unsafe`, no hook. Because this is an integration test it is compiled as a
//! separate crate, so those tests can only use what a real downstream user can.
//!
//! Gzip-framed cases are gated behind `#[cfg(feature = "gzip")]`; the file is
//! authored for the default (std + gzip) feature set.

use core::cell::{Cell, RefCell};
use core::ffi::{c_int, c_uint, c_void};
use core::ptr;

#[cfg(feature = "gzip")]
use zlib_rs::GzHeader;
use zlib_rs::checksum::adler32;
use zlib_rs::constants::{DEF_MEM_LEVEL, Strategy, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH, Z_TREES};
use zlib_rs::deflate::{DeflateState, deflate, deflate_end, deflate_init2, deflate_set_dictionary};
use zlib_rs::ffi::{
    inflate as ffi_inflate, inflateBack, inflateBackEnd, inflateBackInit_, inflateCopy, inflateEnd,
    inflateInit_, inflateInit2_, inflateSetDictionary, z_stream,
};
use zlib_rs::inflate::back::{
    BackMsg, InFunc, OutFunc, inflate_back, inflate_back_end, inflate_back_init,
};
#[cfg(feature = "gzip")]
use zlib_rs::inflate::inflate_get_header;
use zlib_rs::inflate::tables::{CodeType, InflateTableError};
use zlib_rs::inflate::{
    Code, ENOUGH, ENOUGH_DISTS, ENOUGH_LENS, InflateOutcome, MAXBITS, inflate, inflate_codes_used,
    inflate_copy, inflate_end, inflate_init, inflate_init2, inflate_mark, inflate_prime,
    inflate_reset, inflate_reset_keep, inflate_reset2, inflate_set_dictionary, inflate_sync,
    inflate_sync_point, inflate_table, inflate_undermine,
};
use zlib_rs::stream::{AllocBuffer, Allocator, ZeroValid};
use zlib_rs::{ReturnCode, ZStream, ZlibError};

// ===========================================================================
// Helpers
// ===========================================================================

/// Normalize an inflate result into a bare [`ReturnCode`] so success and error
/// variants compare uniformly against the exact expected code (mirrors C, where
/// every entry point returns an `int`).
fn rc(result: Result<ReturnCode, ZlibError>) -> ReturnCode {
    result.unwrap_or_else(ZlibError::as_return_code)
}

/// Liberal hex decoder — a port of `infcover.c`'s `h2b()`.
///
/// Consecutive hex-digit runs accumulate into a byte; any non-hex delimiter
/// (typically a space) terminates the current byte. As in the C original, a
/// value built from a *single* hex digit is biased by 240 before being emitted,
/// so space-separated single digits (e.g. `"0 0"`) decode to two `0x00` bytes
/// while `"00"` decodes to one. Pure safe Rust.
fn h2b(hex: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut val: u32 = 1;
    for byte in hex.bytes().chain(std::iter::once(0u8)) {
        match byte {
            b'0'..=b'9' => val = (val << 4) + u32::from(byte - b'0'),
            b'A'..=b'F' => val = (val << 4) + u32::from(byte - b'A' + 10),
            b'a'..=b'f' => val = (val << 4) + u32::from(byte - b'a' + 10),
            _ => {
                if val != 1 && val < 32 {
                    val += 240;
                }
            }
        }
        if val > 255 {
            out.push((val & 0xff) as u8);
            val = 1;
        }
        if byte == 0 {
            break;
        }
    }
    out
}

/// Build an all-zero [`z_stream`] for the FFI-boundary cases. Every field is a
/// valid zero: null pointers, `None` allocator hooks, zero counters. `z_stream`
/// is `#[repr(C)]` and has no `Default`, so it is constructed field-by-field.
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

/// [`InFunc`] that yields a byte slice exactly once, then signals EOF (empty
/// slice). Mirrors feeding the whole `next_in` buffer to `inflateBack` while the
/// C `pull(desc == Z_NULL)` supplies no additional input.
struct OneShot<'a> {
    data: &'a [u8],
    done: bool,
    /// The chunk made current by the most recent successful `advance`. The engine
    /// reads straight out of `data` through this window; nothing is copied.
    cur: &'a [u8],
}

impl<'a> OneShot<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            done: false,
            cur: &[],
        }
    }
}

impl InFunc for OneShot<'_> {
    fn advance(&mut self) -> bool {
        if self.done {
            false
        } else {
            self.done = true;
            self.cur = self.data;
            true
        }
    }

    fn chunk(&self) -> &[u8] {
        self.cur
    }
}

/// [`OutFunc`] that accepts and discards all output — the analogue of C
/// `push(desc == Z_NULL)`, which reports success.
struct SinkAccept;

impl OutFunc for SinkAccept {
    fn write_output(&mut self, _buf: &[u8]) -> Result<(), ()> {
        Ok(())
    }
}

/// [`OutFunc`] that always fails — the analogue of C `push(desc != Z_NULL)`,
/// which forces `inflateBack` to abort with `Z_BUF_ERROR`.
struct SinkReject;

impl OutFunc for SinkReject {
    fn write_output(&mut self, _buf: &[u8]) -> Result<(), ()> {
        Err(())
    }
}

/// C `in_func` that supplies no input, the analogue of `infcover.c`'s
/// `pull(desc == Z_NULL)` early return.
///
/// Used only where the callback must exist for the call to get past
/// `inflateBack`'s "both callbacks are required" guard, and must never actually
/// run — a state-validation rejection happens first.
unsafe extern "C" fn back_pull_none(_desc: *mut c_void, _buf: *mut *const u8) -> c_uint {
    0
}

/// C `out_func` that reports success without touching the buffer, the analogue of
/// `infcover.c`'s `push(desc == Z_NULL)`.
unsafe extern "C" fn back_push_ok(_desc: *mut c_void, _buf: *mut u8, _len: c_uint) -> c_int {
    0
}

// ===========================================================================
// Drivers
// ===========================================================================

/// Port of `infcover.c`'s `inf()` — the streaming-inflate driver.
///
/// Initializes inflate with `window_bits = win`, then feeds `h2b(hex)` into the
/// decoder in `step`-sized increments (`step == 0` means "all at once"),
/// re-presenting a fresh `len`-byte output buffer on every call. The first
/// `inflate()` return code must equal `err`; subsequent iterations are
/// "don't care" (matching C setting `err = 9`). Each live iteration also
/// round-trips an [`inflate_copy`], and the stream is finally reset with
/// `inflate_reset2(-8)` and released with `inflate_end`.
fn inf(hex: &str, what: &str, step: usize, win: i32, len: usize, err: ReturnCode) {
    let mut strm = ZStream::new();
    if let Err(e) = inflate_init2(&mut strm, win) {
        // C returns early when init fails; we pin the code so the bad-window
        // cases (which expect Z_STREAM_ERROR here) are still asserted.
        assert_eq!(e.as_return_code(), err, "{what}: init");
        return;
    }

    let mut out = vec![0u8; len];

    // `win == 47` selects gzip auto-detect. Register a gz_header so the optional
    // EXTRA/NAME/COMMENT/HCRC field processing is exercised, mirroring C's
    // inflateGetHeader() wired to the output buffer.
    #[cfg(feature = "gzip")]
    if win == 47 {
        let mut head = GzHeader::new();
        head.extra = Some(vec![0u8; len]);
        head.extra_max = len as u32;
        head.name = Some(vec![0u8; len]);
        head.name_max = len as u32;
        head.comment = Some(vec![0u8; len]);
        head.comm_max = len as u32;
        assert_eq!(
            rc(inflate_get_header(&mut strm, head)),
            ReturnCode::Ok,
            "{what}: get_header",
        );
    }

    let input = h2b(hex);
    let total = input.len();
    let step = if step == 0 || step > total {
        total
    } else {
        step
    };

    // `exposed` tracks how many input bytes have been made available so far
    // (C's growing avail_in); `next` is the consumed cursor (C's advancing
    // next_in). C's loop condition is `while (strm.avail_in)`.
    let mut exposed = core::cmp::min(step, total);
    let mut next = 0usize;
    let mut expect = Some(err);

    // Termination watchdog: these fixtures need only a few iterations. The bound
    // guarantees the loop halts even if a dependency regressed into a
    // no-progress state, without masking the meaningful first-iteration check.
    let cap = total.saturating_mul(1024) + 4096;
    for _ in 0..cap {
        let outcome = inflate(&mut strm, &input[next..exposed], &mut out, Z_NO_FLUSH);
        if let Some(expected) = expect {
            assert_eq!(outcome.code, expected, "{what}");
        }
        next += outcome.consumed;

        // Stop on any non-continuable code (matches C's `break`).
        if !matches!(
            outcome.code,
            ReturnCode::Ok | ReturnCode::BufError | ReturnCode::NeedDict
        ) {
            break;
        }

        if outcome.code == ReturnCode::NeedDict {
            // Reachable portion of C's NEED_DICT coverage. C additionally caps its
            // allocator to force `inflateSetDictionary` itself into Z_MEM_ERROR
            // and then poke-restores the private `state->mode = DICT` to resume;
            // that whole leg — including the state the poke papers over — is
            // pinned by `set_dictionary_window_allocation_failure_is_a_mem_error`,
            // because the cap has to be installed through the C ABI allocator
            // hooks that this idiomatic driver deliberately does not use.
            //
            // A dictionary whose Adler-32 mismatches the requested id → DataError.
            assert_eq!(
                rc(inflate_set_dictionary(&mut strm, &input[..1])),
                ReturnCode::DataError,
                "{what}: wrong dictionary",
            );
            // The correct (empty) dictionary — Adler-32("") == the id 1 → Ok.
            assert_eq!(
                rc(inflate_set_dictionary(&mut strm, &[])),
                ReturnCode::Ok,
                "{what}: empty dictionary",
            );
            // Resuming now makes no progress (len == 0 output here) → BufError.
            let resume = inflate(&mut strm, &input[next..exposed], &mut out, Z_NO_FLUSH);
            assert_eq!(
                resume.code,
                ReturnCode::BufError,
                "{what}: resume after dict",
            );
            next += resume.consumed;
        }

        // inflateCopy of a live stream must succeed; release the copy at once.
        let mut copy = ZStream::new();
        assert_eq!(
            rc(inflate_copy(&mut copy, &strm)),
            ReturnCode::Ok,
            "{what}: copy",
        );
        assert_eq!(
            rc(inflate_end(&mut copy)),
            ReturnCode::Ok,
            "{what}: copy end"
        );

        // Subsequent iterations are "don't care" (C sets err = 9).
        expect = None;

        // Expose the next chunk (C: strm.avail_in += min(step, have)).
        let remaining = total - exposed;
        exposed += core::cmp::min(remaining, step);

        // C loop condition: `while (strm.avail_in)`.
        if exposed - next == 0 {
            break;
        }
    }

    assert_eq!(
        rc(inflate_reset2(&mut strm, -8)),
        ReturnCode::Ok,
        "{what}: reset2",
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok, "{what}: end");
}

/// Port of `infcover.c`'s `try()` — raw-inflate a stream through BOTH the
/// streaming `inflate()` path and (when `err >= 0`) `inflateBack()`.
///
/// `err`: `1` expects `DataError` (with `strm.msg == id` on the inflate path),
/// `0` expects success, `-1` is an inflate-only trailer-mismatch case (gzip
/// framing, so no `inflateBack`).
fn try_stream(hex: &str, id: &str, err: i32) {
    let input = h2b(hex);
    let total = input.len();
    let size = (total << 3).max(1);

    // --- first with streaming inflate ---
    {
        let mut strm = ZStream::new();
        let win = if err < 0 { 47 } else { -15 };
        assert_eq!(
            rc(inflate_init2(&mut strm, win)),
            ReturnCode::Ok,
            "{id}: init",
        );

        let mut out = vec![0u8; size];
        let mut next = 0usize;
        let mut code = ReturnCode::Ok;

        let cap = total.saturating_mul(1024) + 4096;
        for _ in 0..cap {
            let outcome = inflate(&mut strm, &input[next..], &mut out, Z_TREES);
            // C: never Z_STREAM_ERROR / Z_MEM_ERROR on this path.
            assert_ne!(outcome.code, ReturnCode::StreamError, "{id}: stream error");
            assert_ne!(outcome.code, ReturnCode::MemError, "{id}: mem error");
            code = outcome.code;
            next += outcome.consumed;
            if code == ReturnCode::DataError || code == ReturnCode::NeedDict {
                break;
            }
            // C: `while (strm.avail_in || strm.avail_out == 0)`.
            let more_in = next < total;
            let out_full = outcome.produced == out.len();
            if !(more_in || out_full) {
                break;
            }
            if outcome.consumed == 0 && outcome.produced == 0 {
                break;
            }
        }

        if err != 0 {
            assert_eq!(
                code,
                ReturnCode::DataError,
                "{id}: expected DataError (inflate)",
            );
            assert_eq!(strm.msg, Some(id), "{id}: message (inflate)");
        }
        assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok, "{id}: end");
    }

    // --- then with inflateBack (raw only) ---
    if err >= 0 {
        let mut state = inflate_back_init(15).expect("inflate_back_init(15)");
        let mut src = OneShot::new(&input);
        let mut sink = SinkAccept;
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        // C: `assert(ret != Z_STREAM_ERROR)` (`infcover.c` L565).
        assert_ne!(
            outcome.code,
            ReturnCode::StreamError,
            "{id}: back stream error"
        );
        if err != 0 {
            assert_eq!(
                outcome.code,
                ReturnCode::DataError,
                "{id}: expected DataError (back)"
            );
            // C `infcover.c` L567: `assert(strcmp(id, strm.msg) == 0);` — the
            // diagnostic `inflateBack` leaves must be the very string this vector
            // is named after, byte for byte. `BackMsg` is how the engine reports
            // what C stores in `strm->msg`, so the official assertion applies
            // here unchanged.
            match outcome.msg {
                // The literal `strcmp` C performs: the diagnostic text must equal
                // the vector's id.
                BackMsg::Set(m) => assert_eq!(m, id, "{id}: message (back)"),
                other => panic!("{id}: expected a diagnostic, got {other:?}"),
            }
        } else {
            // A well-formed vector must leave the diagnostic cleared, which is
            // what C's `strm->msg = Z_NULL` at `infback.c` L214 does.
            assert_eq!(outcome.msg, BackMsg::Cleared, "{id}: message (back, ok)");
        }
        assert_eq!(inflate_back_end(state), ReturnCode::Ok, "{id}: back end");
    }
}

// ===========================================================================
// Coverage tests (one per infcover.c cover_* orchestrator)
// ===========================================================================

/// Port of `cover_support()` — init / prime / dictionary / reset paths.
#[test]
fn cover_support() {
    // init → prime → set-dictionary error → end.
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init(&mut strm)), ReturnCode::Ok);
    assert_eq!(rc(inflate_prime(&mut strm, 5, 31)), ReturnCode::Ok);
    assert_eq!(rc(inflate_prime(&mut strm, -1, 0)), ReturnCode::Ok);
    // Empty dictionary on a zlib stream not awaiting one → StreamError.
    assert_eq!(
        rc(inflate_set_dictionary(&mut strm, &[])),
        ReturnCode::StreamError,
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);

    // Window allocation / replacement / split update / fixed blocks / bad size.
    inf("63 0", "force window allocation", 0, -15, 1, ReturnCode::Ok);
    inf(
        "63 18 5",
        "force window replacement",
        0,
        -8,
        259,
        ReturnCode::Ok,
    );
    inf(
        "63 18 68 30 d0 0 0",
        "force split window update",
        4,
        -8,
        259,
        ReturnCode::Ok,
    );
    inf("3 0", "use fixed blocks", 0, -15, 1, ReturnCode::StreamEnd);
    inf("", "bad window size", 0, 1, 0, ReturnCode::StreamError);

    // Wrong version string → VersionError (version param exists only on the C ABI).
    let mut zs = zeroed_stream();
    // SAFETY: the bogus version ('!' != '1') fails version_check before the
    // stream is touched, so inflateInit_ returns Z_VERSION_ERROR without
    // installing or freeing any state. `zs` is a valid z_stream for the call.
    let ret = unsafe {
        inflateInit_(
            &mut zs,
            c"!".as_ptr(),
            core::mem::size_of::<z_stream>() as c_int,
        )
    };
    assert_eq!(ret, ReturnCode::VersionError.as_c_int());

    // Built-in memory routines: a plain init/end round-trip.
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init(&mut strm)), ReturnCode::Ok);
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

/// Port of `cover_wrap()` — NULL-parameter guards, header/trailer vectors, and
/// the miscellaneous API sequence.
#[test]
fn cover_wrap() {
    // NULL-parameter guards via the FFI C ABI (idiomatic API cannot pass null).
    // SAFETY: each shim checks for a null stream first and returns
    // Z_STREAM_ERROR without dereferencing it or the (absent) callbacks.
    unsafe {
        assert_eq!(
            ffi_inflate(ptr::null_mut(), 0),
            ReturnCode::StreamError.as_c_int(),
        );
        assert_eq!(
            inflateEnd(ptr::null_mut()),
            ReturnCode::StreamError.as_c_int(),
        );
        assert_eq!(
            inflateCopy(ptr::null_mut(), ptr::null_mut()),
            ReturnCode::StreamError.as_c_int(),
        );
    }

    // zlib / raw header + trailer cases (no gzip framing required).
    inf("77 85", "bad zlib method", 0, 15, 0, ReturnCode::DataError);
    inf(
        "8 99",
        "set window size from header",
        0,
        0,
        0,
        ReturnCode::Ok,
    );
    inf(
        "78 9c",
        "bad zlib window size",
        0,
        8,
        0,
        ReturnCode::DataError,
    );
    inf(
        "78 9c 63 0 0 0 1 0 1",
        "check adler32",
        0,
        15,
        1,
        ReturnCode::StreamEnd,
    );
    inf(
        "8 b8 0 0 0 1",
        "need dictionary",
        0,
        8,
        0,
        ReturnCode::NeedDict,
    );
    inf("78 9c 63 0", "compute adler32", 0, 15, 1, ReturnCode::Ok);

    // gzip / auto-detect header + trailer cases (require the gzip wrapper).
    #[cfg(feature = "gzip")]
    {
        inf(
            "1f 8b 0 0",
            "bad gzip method",
            0,
            31,
            0,
            ReturnCode::DataError,
        );
        inf(
            "1f 8b 8 80",
            "bad gzip flags",
            0,
            31,
            0,
            ReturnCode::DataError,
        );
        inf(
            "1f 8b 8 1e 0 0 0 0 0 0 1 0 0 0 0 0 0",
            "bad header crc",
            0,
            47,
            1,
            ReturnCode::DataError,
        );
        inf(
            "1f 8b 8 2 0 0 0 0 0 0 1d 26 3 0 0 0 0 0 0 0 0 0",
            "check gzip length",
            0,
            47,
            0,
            ReturnCode::StreamEnd,
        );
        inf(
            "78 90",
            "bad zlib header check",
            0,
            47,
            0,
            ReturnCode::DataError,
        );
    }

    // Miscellaneous API sequence. The C `mem_*` allocation-limit steps that force
    // Z_MEM_ERROR (the capped inflate() calls and inflateCopy) are consolidated
    // into the dedicated `mem_limit_forces_mem_error` test, which reproduces the
    // forced-failure via the FFI `zalloc`/`zfree` hooks (allocation is fallible
    // at the boundary — see the module docs). The remaining, publicly reachable
    // steps of this sequence are reproduced exactly here.
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, -8)), ReturnCode::Ok);
    // (C's 1-byte-limit Z_MEM_ERROR steps are covered by mem_limit_forces_mem_error.)
    let dict = [0u8; 257];
    assert_eq!(rc(inflate_set_dictionary(&mut strm, &dict)), ReturnCode::Ok);
    assert_eq!(rc(inflate_prime(&mut strm, 16, 0)), ReturnCode::Ok);
    // inflateSync on 0x80,0x00 finds no marker → DataError.
    let (sync1, _) = inflate_sync(&mut strm, &[0x80, 0x00]);
    assert_eq!(sync1, ReturnCode::DataError);
    // A subsequent inflate on the now-invalid stream → StreamError.
    let after = inflate(&mut strm, &[], &mut [0u8; 16], Z_NO_FLUSH);
    assert_eq!(after.code, ReturnCode::StreamError);
    // inflateSync on the empty-stored marker 00 00 ff ff → Ok.
    let (sync2, _) = inflate_sync(&mut strm, &[0x00, 0x00, 0xff, 0xff]);
    assert_eq!(sync2, ReturnCode::Ok);
    // inflateSyncPoint — return value unused (C casts to void).
    let _ = inflate_sync_point(&strm);
    // C's inflateCopy runs under a byte cap to force Z_MEM_ERROR; that forced
    // path is covered by mem_limit_forces_mem_error. Here the idiomatic API
    // installs no cap, so the copy succeeds.
    let mut copy = ZStream::new();
    assert_eq!(rc(inflate_copy(&mut copy, &strm)), ReturnCode::Ok);
    assert_eq!(rc(inflate_end(&mut copy)), ReturnCode::Ok);
    assert_eq!(rc(inflate_undermine(&mut strm, 1)), ReturnCode::DataError);
    // inflateMark — return value unused (C casts to void).
    let _ = inflate_mark(&strm);
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

/// Port of `cover_back()` — inflateBack bad-parameter guards and the
/// callback-driven decode paths.
#[test]
fn cover_back() {
    // Bad parameters via the FFI C ABI.
    // SAFETY: each call hits an early version/null guard and returns an error
    // code without dereferencing the null stream or invoking the callbacks.
    unsafe {
        // version NULL → Z_VERSION_ERROR (checked before the stream).
        assert_eq!(
            inflateBackInit_(ptr::null_mut(), 0, ptr::null_mut(), ptr::null(), 0),
            ReturnCode::VersionError.as_c_int(),
        );
        // valid version, NULL stream → Z_STREAM_ERROR.
        assert_eq!(
            inflateBackInit_(
                ptr::null_mut(),
                15,
                ptr::null_mut(),
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            ),
            ReturnCode::StreamError.as_c_int(),
        );
        assert_eq!(
            inflateBack(
                ptr::null_mut(),
                None,
                ptr::null_mut(),
                None,
                ptr::null_mut()
            ),
            ReturnCode::StreamError.as_c_int(),
        );
        assert_eq!(
            inflateBackEnd(ptr::null_mut()),
            ReturnCode::StreamError.as_c_int(),
        );
    }

    // Normal completion: a raw fixed empty block decodes to Z_STREAM_END.
    {
        let mut state = inflate_back_init(15).expect("inflate_back_init(15)");
        let mut src = OneShot::new(&[0x03, 0x00]);
        let mut sink = SinkAccept;
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        assert_eq!(outcome.msg, BackMsg::Cleared);
        assert_eq!(inflate_back_end(state), ReturnCode::Ok);
    }

    // Forced output error: the sink rejects, so inflateBack aborts with BufError.
    {
        let mut state = inflate_back_init(15).expect("inflate_back_init(15)");
        let mut src = OneShot::new(&[0x63, 0x00, 0x00]);
        let mut sink = SinkReject;
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::BufError);
        // C never assigns a diagnostic on the output-abort path.
        assert_eq!(outcome.msg, BackMsg::Cleared);
        assert_eq!(inflate_back_end(state), ReturnCode::Ok);
    }

    // Forced *mode* error (Z_STREAM_ERROR). C forces it by having its `pull`
    // callback poke `((inflate_state*)strm.state)->mode = SYNC` — "force an
    // otherwise impossible situation" (`infcover.c` L459) — and then asserts
    // `inflateBack` returns Z_STREAM_ERROR (`infcover.c` L496-L497).
    //
    // That exact poke is not expressible from here: the idiomatic `InFunc` trait
    // only yields input bytes and cannot touch private engine state, and forging
    // it with `unsafe` is expressly forbidden. The assertion is therefore made in
    // two complementary places, and BOTH exist:
    //
    //   * the unserviceable-mode arm itself is asserted by the unit test
    //     `inflate::back::tests::unsupported_mode_is_stream_error` in
    //     `src/inflate/back.rs`, which assigns SYNC (and every other mode that
    //     back-inflate cannot service) to a real initialized state and drives the
    //     private `drive()` machine directly, in safe Rust;
    //   * the publicly reachable half is asserted right here: a stream whose state
    //     belongs to a DIFFERENT engine is a state `inflateBack` cannot service,
    //     and the C ABI must reject it with Z_STREAM_ERROR rather than
    //     misinterpreting the handle.
    {
        let mut strm = zeroed_stream();
        // SAFETY: `strm` is a valid caller-owned `z_stream`; `c"1"` is a valid
        // version string whose first byte matches the library version, and the
        // reported size is the true `sizeof(z_stream)`.
        let init = unsafe {
            inflateInit2_(
                &mut strm,
                15,
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "inflateInit2_ must succeed"
        );

        // SAFETY: `strm` holds a live *inflate* handle, not an inflateBack one.
        // The shim validates the handle tag before touching engine state, so the
        // callbacks are never invoked and nothing is misinterpreted.
        let ret = unsafe {
            inflateBack(
                &mut strm,
                Some(back_pull_none),
                ptr::null_mut(),
                Some(back_push_ok),
                ptr::null_mut(),
            )
        };
        assert_eq!(
            ret,
            ReturnCode::StreamError.as_c_int(),
            "inflateBack over a state it cannot service must return Z_STREAM_ERROR",
        );

        // SAFETY: `strm` was initialized by `inflateInit2_` and is still live;
        // the rejected `inflateBack` call consumed nothing.
        assert_eq!(
            unsafe { inflateEnd(&mut strm) },
            ReturnCode::Ok.as_c_int(),
            "the inflate handle must survive the rejected inflateBack call",
        );
    }

    // Built-in memory routines: plain init/end.
    let state = inflate_back_init(15).expect("inflate_back_init(15)");
    assert_eq!(inflate_back_end(state), ReturnCode::Ok);
}

/// Port of `cover_inflate()` — DEFLATE data cases through inflate and inflateBack.
#[test]
fn cover_inflate() {
    try_stream("0 0 0 0 0", "invalid stored block lengths", 1);
    try_stream("3 0", "fixed", 0);
    try_stream("6", "invalid block type", 1);
    try_stream("1 1 0 fe ff 0", "stored", 0);
    try_stream("fc 0 0", "too many length or distance symbols", 1);
    try_stream("4 0 fe ff", "invalid code lengths set", 1);
    try_stream("4 0 24 49 0", "invalid bit length repeat", 1);
    try_stream("4 0 24 e9 ff ff", "invalid bit length repeat", 1);
    try_stream("4 0 24 e9 ff 6d", "invalid code -- missing end-of-block", 1);
    try_stream(
        "4 80 49 92 24 49 92 24 71 ff ff 93 11 0",
        "invalid literal/lengths set",
        1,
    );
    try_stream(
        "4 80 49 92 24 49 92 24 f b4 ff ff c3 84",
        "invalid distances set",
        1,
    );
    try_stream(
        "4 c0 81 8 0 0 0 0 20 7f eb b 0 0",
        "invalid literal/length code",
        1,
    );
    try_stream("2 7e ff ff", "invalid distance code", 1);
    try_stream(
        "c c0 81 0 0 0 0 0 90 ff 6b 4 0",
        "invalid distance too far back",
        1,
    );

    // Trailer mismatch — only surfaced through inflate() over gzip framing, so
    // inflate-only (err < 0) and gated on the gzip wrapper.
    #[cfg(feature = "gzip")]
    {
        try_stream(
            "1f 8b 8 0 0 0 0 0 0 0 3 0 0 0 0 1",
            "incorrect data check",
            -1,
        );
        try_stream(
            "1f 8b 8 0 0 0 0 0 0 0 3 0 0 0 0 0 0 0 0 1",
            "incorrect length check",
            -1,
        );
    }

    try_stream("5 c0 21 d 0 0 0 80 b0 fe 6d 2f 91 6c", "pull 17", 0);
    try_stream(
        "5 e0 81 91 24 cb b2 2c 49 e2 f 2e 8b 9a 47 56 9f fb fe ec d2 ff 1f",
        "long code",
        0,
    );
    try_stream("ed c0 1 1 0 0 0 40 20 ff 57 1b 42 2c 4f", "length extra", 0);
    try_stream(
        "ed cf c1 b1 2c 47 10 c4 30 fa 6f 35 1d 1 82 59 3d fb be 2e 2a fc f c",
        "long distance and extra",
        0,
    );
    try_stream(
        "ed c0 81 0 0 0 0 80 a0 fd a9 17 a9 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 6",
        "window end",
        0,
    );

    inf(
        "2 8 20 80 0 3 0",
        "inflate_fast TYPE return",
        0,
        -15,
        258,
        ReturnCode::StreamEnd,
    );
    inf("63 18 5 40 c 0", "window wrap", 3, -8, 300, ReturnCode::Ok);
}

/// Port of `cover_trees()` — force `inflate_table`'s "not enough" (`Enough`)
/// error, which the normal decode path never triggers.
///
/// Unlike `infcover.c` (which reaches a private `inflate_table()` via internal
/// headers), the `zlib-rs` table builder is part of the public API, so this
/// coverage runs directly here — no delegation or forged access required.
#[test]
fn cover_trees() {
    // lens = [1, 2, 3, ..., 15, 15] (16 code lengths).
    let mut lens = [0u16; 16];
    for (i, slot) in lens.iter_mut().enumerate().take(15) {
        *slot = (i + 1) as u16;
    }
    lens[15] = 15;

    let mut work = [0u16; 16];
    let mut table = [Code::default(); ENOUGH_DISTS];

    // First over-subscription attempt (root bits = 15) → Enough.
    let mut table_index = 0usize;
    let mut bits = 15usize;
    assert_eq!(
        inflate_table(
            CodeType::Dists,
            &lens,
            16,
            &mut table,
            &mut table_index,
            &mut bits,
            &mut work,
        ),
        Err(InflateTableError::Enough),
    );

    // Second attempt (root bits = 1) → Enough.
    let mut table_index = 0usize;
    let mut bits = 1usize;
    assert_eq!(
        inflate_table(
            CodeType::Dists,
            &lens,
            16,
            &mut table,
            &mut table_index,
            &mut bits,
            &mut work,
        ),
        Err(InflateTableError::Enough),
    );
}

/// Port of `cover_fast()` — the `inffast` decode inner loop and window copies.
#[test]
fn cover_fast() {
    inf(
        "e5 e0 81 ad 6d cb b2 2c c9 01 1e 59 63 ae 7d ee fb 4d fd b5 35 41 68 ff 7f 0f 0 0 0",
        "fast length extra bits",
        0,
        -8,
        258,
        ReturnCode::DataError,
    );
    inf(
        "25 fd 81 b5 6d 59 b6 6a 49 ea af 35 6 34 eb 8c b9 f6 b9 1e ef 67 49 50 fe ff ff 3f 0 0",
        "fast distance extra bits",
        0,
        -8,
        258,
        ReturnCode::DataError,
    );
    inf(
        "3 7e 0 0 0 0 0",
        "fast invalid distance code",
        0,
        -8,
        258,
        ReturnCode::DataError,
    );
    inf(
        "1b 7 0 0 0 0 0",
        "fast invalid literal/length code",
        0,
        -8,
        258,
        ReturnCode::DataError,
    );
    inf(
        "d c7 1 ae eb 38 c 4 41 a0 87 72 de df fb 1f b8 36 b1 38 5d ff ff 0",
        "fast 2nd level codes and too far back",
        0,
        -8,
        258,
        ReturnCode::DataError,
    );
    inf(
        "63 18 5 8c 10 8 0 0 0 0",
        "very common case",
        0,
        -8,
        259,
        ReturnCode::Ok,
    );
    inf(
        "63 60 60 18 c9 0 8 18 18 18 26 c0 28 0 29 0 0 0",
        "contiguous and wrap around window",
        6,
        -8,
        259,
        ReturnCode::Ok,
    );
    inf(
        "63 0 3 0 0 0 0 0",
        "copy direct from output",
        0,
        -8,
        259,
        ReturnCode::StreamEnd,
    );
}

// ===========================================================================
// Extended `inflate_table` branch coverage
//
// `cover_trees` above is the verbatim port of infcover.c's cover_trees, which
// reaches exactly one of `inflate_table`'s exits: the `Enough` table-arena
// overflow. C stops there because that is the only exit a byte stream cannot
// reach — "zlib insures that enough is always enough". The remaining exits of
// the validation prologue (inftrees.c L121-L147) ARE reachable from a malformed
// stream, but only through hundreds of lines of surrounding decoder, so a stream
// fixture localises a failure poorly and never pins the boundary itself. The
// tests below reuse C's own remedy — call the builder directly — for each one.
//
// Every case here is pure safe Rust: `inflate_table` is a safe public function,
// so none of these needs (or has) an `unsafe` block.
// ===========================================================================

/// Pin the decode-table arena bounds transcribed from `inftrees.h` L49-L51.
///
/// `ENOUGH_LENS` and `ENOUGH_DISTS` are the exhaustive-search results that
/// `inftrees.h` records — `enough 286 9 15` returns 852 for literal/length codes
/// and `enough 30 6 15` returns 592 for distance codes — and `ENOUGH` is their
/// sum. They size every decode-table arena in the decoder, and `inflate_table`
/// compares its running `used` count against them to decide whether to return
/// [`InflateTableError::Enough`], the exact error [`cover_trees`] forces.
///
/// AAP §0.6.6 records why the precise values are load-bearing: understating them
/// lets an adversarial stream overflow the arena, while overstating them wastes
/// memory on every stream. Preservation directive D-2 (AAP §0.8.1) therefore
/// forbids altering them in either direction, so they are asserted here rather
/// than assumed. `MAXBITS` is pinned alongside them because it is the DEFLATE
/// hard limit on a code length and bounds the length-count arrays the builder
/// indexes.
#[test]
fn enough_bounds_match_c() {
    assert_eq!(ENOUGH_LENS, 852, "inftrees.h L49: `enough 286 9 15`");
    assert_eq!(ENOUGH_DISTS, 592, "inftrees.h L50: `enough 30 6 15`");
    assert_eq!(ENOUGH, 1444, "inftrees.h L51: ENOUGH_LENS + ENOUGH_DISTS");
    assert_eq!(ENOUGH, ENOUGH_LENS + ENOUGH_DISTS);
    assert_eq!(MAXBITS, 15, "the DEFLATE hard limit on a code length");
}

/// An over-subscribed set of code lengths must be rejected — `inftrees.c`
/// L139-L143, where the Kraft accumulator goes negative and C returns `-1`.
///
/// Three symbols of length 1 claim three of the two available 1-bit codes, so the
/// accumulator goes negative on its very first iteration:
/// `left = (1 << 1) - count[1] = 2 - 3 = -1`. That check sits *before* C's
/// `type == CODES || max != 1` disjunction, so over-subscription is rejected
/// unconditionally — which is why all three code types are asserted here rather
/// than one representative.
///
/// [`InflateTableError::Invalid`] is this port's spelling of C's `-1` return, a
/// correspondence D-2 freezes. C returns without writing either output parameter,
/// so the caller's arena cursor and root-bit request must both come back
/// untouched; a port that scribbled a partial table before failing would be an
/// observable behaviour change even though the return code matched.
#[test]
fn inflate_table_rejects_over_subscribed() {
    let lens = [1u16, 1, 1];

    for code_type in [CodeType::Codes, CodeType::Lens, CodeType::Dists] {
        let mut work = [0u16; 3];
        let mut table = [Code::default(); ENOUGH];
        let mut table_index = 0usize;
        let mut bits = 7usize;

        assert_eq!(
            inflate_table(
                code_type,
                &lens,
                3,
                &mut table,
                &mut table_index,
                &mut bits,
                &mut work,
            ),
            Err(InflateTableError::Invalid),
            "{code_type:?}: three length-1 codes over-subscribe the 1-bit space",
        );
        assert_eq!(table_index, 0, "{code_type:?}: no table entry is emitted");
        assert_eq!(bits, 7, "{code_type:?}: the root-bit request is untouched");
    }
}

/// An incomplete set of code lengths must be rejected — `inftrees.c` L146-L147,
/// `left > 0 && (type == CODES || max != 1)` returning C's `-1`.
///
/// Both halves of C's disjunction are covered, because they reject for different
/// reasons and a port could plausibly honour one and drop the other:
///
/// 1. **`max != 1`.** Three symbols of length 2 leave one 2-bit code unassigned.
///    The accumulator runs `len = 1` → `left = 2`, `len = 2` → `left = 4 - 3 = 1`
///    and thereafter only doubles, so it ends positive with `max == 2`. Rejected
///    for every code type.
/// 2. **`type == CODES`.** A lone length-1 code is also incomplete
///    (`left = 2 - 1 = 1`) but has `max == 1`, so the second half of the
///    disjunction is false and the *first* half is what rejects it: the 19-symbol
///    code-length alphabet that encodes a dynamic block's code lengths must be
///    complete. The identical length set is *accepted* for `Lens`/`Dists` — see
///    [`inflate_table_allows_incomplete_single_distance_code`] — so this is the
///    branch that distinguishes the two alphabets, and it is the whole reason C
///    writes a disjunction instead of a single test.
#[test]
fn inflate_table_rejects_incomplete_set() {
    // (1) Incomplete with max != 1 — rejected for every code type.
    let lens = [2u16, 2, 2];
    for code_type in [CodeType::Codes, CodeType::Lens, CodeType::Dists] {
        let mut work = [0u16; 3];
        let mut table = [Code::default(); ENOUGH];
        let mut table_index = 0usize;
        let mut bits = 7usize;

        assert_eq!(
            inflate_table(
                code_type,
                &lens,
                3,
                &mut table,
                &mut table_index,
                &mut bits,
                &mut work,
            ),
            Err(InflateTableError::Invalid),
            "{code_type:?}: three length-2 codes leave the 2-bit space incomplete",
        );
        assert_eq!(table_index, 0, "{code_type:?}: no table entry is emitted");
        assert_eq!(bits, 7, "{code_type:?}: the root-bit request is untouched");
    }

    // (2) Incomplete with max == 1, where `type == CODES` short-circuits the
    //     `max != 1` test: the code-length alphabet must be complete.
    let lens = [1u16];
    let mut work = [0u16; 1];
    let mut table = [Code::default(); ENOUGH];
    let mut table_index = 0usize;
    let mut bits = 7usize;

    assert_eq!(
        inflate_table(
            CodeType::Codes,
            &lens,
            1,
            &mut table,
            &mut table_index,
            &mut bits,
            &mut work,
        ),
        Err(InflateTableError::Invalid),
        "a lone length-1 code is incomplete, and CODES requires completeness",
    );
    assert_eq!(table_index, 0, "no table entry is emitted");
    assert_eq!(bits, 7, "the root-bit request is untouched");
}

/// A single length-1 code is incomplete yet **accepted** for the literal/length
/// and distance alphabets — `inftrees.c` L146, with the `max != 1` half of
/// `(type == CODES || max != 1)` evaluating false.
///
/// This `Ok` is deliberate and load-bearing, not a missing validation. Reference
/// zlib accepts a dynamic block that declares exactly one distance code, and the
/// disjunction is written the way it is precisely to let `max == 1` through for
/// `LENS`/`DISTS` while still rejecting it for `CODES`. "Hardening" it into an
/// error would reject streams a default-built reference zlib decodes — an
/// acceptance-parity break, which is a behaviour change rather than an
/// improvement.
///
/// Everything the builder writes on this path is pinned:
///
/// * `bits` comes back as `1`, because `root` is clamped down to `max`
///   (`if (root > max) root = max`) and reported through `*bits` at
///   `inftrees.c` L308 — the caller asked for 7;
/// * `table_index` advances by exactly `used == 1 << root == 2`
///   (`inftrees.c` L307);
/// * entry 0 decodes symbol 0 of the requested alphabet — a literal for `Lens`,
///   and `dbase[0] == 1` with `dext[0] == 16` extra bits for `Dists`;
/// * entry 1 is the trailing invalid-code marker C fills in for the unused half
///   of the 1-bit code space (`inftrees.c` L297-L304, `op == 64`).
#[test]
fn inflate_table_allows_incomplete_single_distance_code() {
    // count[1] == 1 with count[0] == 2: one live code of length 1, so max == 1
    // and the Kraft accumulator ends at left == 1 > 0.
    let lens = [1u16, 0, 0];
    let invalid_marker = Code {
        op: 64,
        bits: 1,
        val: 0,
    };

    for (code_type, expected_first) in [
        // Symbol 0 of the literal/length alphabet is literal 0, so op == 0.
        (
            CodeType::Lens,
            Code {
                op: 0,
                bits: 1,
                val: 0,
            },
        ),
        // Symbol 0 of the distance alphabet is dbase[0] == 1 with dext[0] == 16.
        (
            CodeType::Dists,
            Code {
                op: 16,
                bits: 1,
                val: 1,
            },
        ),
    ] {
        let mut work = [0u16; 3];
        let mut table = [Code::default(); ENOUGH];
        let mut table_index = 0usize;
        let mut bits = 7usize;

        assert_eq!(
            inflate_table(
                code_type,
                &lens,
                3,
                &mut table,
                &mut table_index,
                &mut bits,
                &mut work,
            ),
            Ok(()),
            "{code_type:?}: an incomplete single length-1 code is accepted",
        );
        assert_eq!(bits, 1, "{code_type:?}: root is clamped down to max == 1");
        assert_eq!(table_index, 2, "{code_type:?}: used == 1 << root == 2");
        assert_eq!(table[0], expected_first, "{code_type:?}: symbol 0 entry");
        assert_eq!(
            table[1], invalid_marker,
            "{code_type:?}: trailing invalid-code marker",
        );
    }
}

/// All-zero code lengths build a two-entry invalid table and return `Ok` —
/// `inftrees.c` L126-L134, the "no symbols to code at all" branch.
///
/// With every length zero there is no maximum length, so no code can be built at
/// all. C does **not** report an error here: it writes two copies of the
/// invalid-code marker (`op == 64`), sets `*bits = 1`, and returns `0` under the
/// comment "no symbols, but wait for decoding to report error". The deferred
/// error is the point — the marker makes the *decoder* fail on the first symbol
/// it tries to read, which keeps the error's reporting position identical to C's.
/// Reporting it early from the builder instead would move an observable error
/// point, exactly the kind of change AAP §0.6.5's failure-timing parity rules
/// out.
///
/// The branch precedes every type-dependent test, so all three code types are
/// asserted. A second pass then starts from a non-zero `table_index` to pin C's
/// `*(*table)++` semantics: the markers land at the caller's cursor rather than at
/// the start of the arena, the cursor advances by exactly two, and nothing before
/// it is disturbed.
#[test]
fn inflate_table_all_zero_lengths_builds_invalid_marker() {
    let lens = [0u16; 4];
    let invalid_marker = Code {
        op: 64,
        bits: 1,
        val: 0,
    };

    for code_type in [CodeType::Codes, CodeType::Lens, CodeType::Dists] {
        let mut work = [0u16; 4];
        let mut table = [Code::default(); ENOUGH];
        let mut table_index = 0usize;
        let mut bits = 7usize;

        assert_eq!(
            inflate_table(
                code_type,
                &lens,
                4,
                &mut table,
                &mut table_index,
                &mut bits,
                &mut work,
            ),
            Ok(()),
            "{code_type:?}: no symbols defers the error to the decoder",
        );
        assert_eq!(
            table[0], invalid_marker,
            "{code_type:?}: first invalid-code marker",
        );
        assert_eq!(
            table[1], invalid_marker,
            "{code_type:?}: second invalid-code marker",
        );
        assert_eq!(table_index, 2, "{code_type:?}: exactly two entries written");
        assert_eq!(
            bits, 1,
            "{code_type:?}: the degenerate table has 1 root bit"
        );
    }

    // The same branch from a non-zero cursor. C writes through `*(*table)++`, an
    // advancing caller-owned pointer, so an appended degenerate table must land
    // at the cursor and leave every earlier entry alone.
    const BASE: usize = 5;
    let mut work = [0u16; 4];
    let mut table = [Code::default(); ENOUGH];
    let mut table_index = BASE;
    let mut bits = 7usize;

    assert_eq!(
        inflate_table(
            CodeType::Dists,
            &lens,
            4,
            &mut table,
            &mut table_index,
            &mut bits,
            &mut work,
        ),
        Ok(()),
        "an appended degenerate table is built at the caller's cursor",
    );
    assert_eq!(
        table_index,
        BASE + 2,
        "the cursor advances by two from its base",
    );
    assert_eq!(table[BASE], invalid_marker);
    assert_eq!(table[BASE + 1], invalid_marker);
    assert_eq!(bits, 1);
    assert!(
        table[..BASE].iter().all(|entry| *entry == Code::default()),
        "entries before the cursor are untouched",
    );
}

/// A stream copied mid-decode must finish identically to the original — the
/// integration-level proof that offsets replaced C's interior table pointers.
///
/// C's `inflateCopy` (`inflate.c` L1328-L1367) copies the state and then re-bases
/// `state->next`, `lencode` and `distcode`, every one of which points *into*
/// `state->codes[]`; without that fix-up the copy's table references would still
/// address the *original's* arena. This port stores them as integer offsets plus
/// a table-source discriminant, so a deep clone is correct with no fix-up at all
/// (AAP §0.6.3) — and this test is what makes that claim falsifiable instead of
/// merely asserted.
///
/// The copy is taken only once the decoder is genuinely inside a dynamic block:
/// [`inflate_codes_used`] (the port of C `inflateCodesUsed`) reports the arena
/// cursor, so a non-zero value means dynamic Huffman tables have been built into
/// `codes[]` and the live table references are offsets into it. That is a
/// *public* observation — nothing here reads private state, the same discipline
/// the module header records for the forced-`inflateBack`-mode gap.
///
/// The `inf()` driver already round-trips an `inflate_copy` on every iteration,
/// but releases the copy immediately without resuming it. Resuming through both
/// halves is the part a broken deep copy would survive there and fail here.
#[test]
fn inflate_copy_mid_decode_resumes_identically() {
    // Mixed-entropy payload: repeated phrases give the match finder long
    // distances to encode while the interleaved counter keeps the literal
    // alphabet wide, so the encoder emits dynamic Huffman blocks with a real
    // distance code and the decoder must build both tables into its arena.
    let mut payload = Vec::with_capacity(64_000);
    for i in 0..1_500u32 {
        payload.extend_from_slice(b"the quick brown fox jumps over the lazy dog; ");
        payload.extend_from_slice(&i.to_le_bytes());
    }

    // Compress with this crate's own encoder (zlib framing, default strategy).
    let mut def = ZStream::new();
    assert_eq!(
        rc(deflate_init2(
            &mut def,
            6,
            Z_DEFLATED,
            15,
            DEF_MEM_LEVEL,
            Strategy::Default,
        )),
        ReturnCode::Ok,
    );
    let mut compressed = vec![0u8; payload.len() + 1024];
    let packed = deflate(&mut def, &payload, &mut compressed, Z_FINISH);
    assert_eq!(packed.code, ReturnCode::StreamEnd, "deflate must finish");
    assert_eq!(packed.consumed, payload.len());
    compressed.truncate(packed.produced);
    assert_eq!(rc(deflate_end(&mut def)), ReturnCode::Ok);

    // Decode a prefix through a deliberately small output window so the decoder
    // stops *inside* the first dynamic block, with its tables already built.
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);

    let mut consumed = 0usize;
    let mut original_out = Vec::with_capacity(payload.len());
    let mut window = [0u8; 64];
    while original_out.len() < 4_096 {
        let outcome = inflate(&mut strm, &compressed[consumed..], &mut window, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::Ok,
            "the prefix decode must stay continuable",
        );
        assert!(outcome.produced > 0, "the prefix decode must make progress");
        consumed += outcome.consumed;
        original_out.extend_from_slice(&window[..outcome.produced]);
    }

    // Dynamic tables are live: the arena cursor has advanced, so `lencode` and
    // `distcode` are offsets into this state's own `codes[]` rather than the
    // module-static fixed tables.
    let used_at_copy = inflate_codes_used(&strm).expect("a live stream reports codes used");
    assert!(
        used_at_copy > 0,
        "the decoder must be inside a dynamic block at the copy point",
    );

    // A match copy is in flight at this point, so the snapshot also carries
    // partially-emitted length/distance state rather than sitting on a clean
    // symbol boundary. `inflate_mark` reports it (`back` in the high bits,
    // `was - length` in the low 16), and it must survive the clone unchanged.
    let mark_at_copy = inflate_mark(&strm);
    assert_ne!(
        mark_at_copy, 0,
        "the copy point must carry in-flight decode state, not a clean boundary",
    );

    // Snapshot the decoder. C would have to re-base three interior pointers here.
    let mut copy = ZStream::new();
    assert_eq!(rc(inflate_copy(&mut copy, &strm)), ReturnCode::Ok);
    assert_eq!(
        inflate_codes_used(&copy),
        Some(used_at_copy),
        "the copy inherits the source's arena cursor",
    );
    assert_eq!(
        inflate_mark(&copy),
        mark_at_copy,
        "the copy inherits the in-flight match state",
    );
    // C finishes `inflateCopy` with `zmemcpy(dest, source, sizeof(z_stream))`,
    // which carries the observable stream bookkeeping across as well.
    assert_eq!(copy.total_in, strm.total_in, "the copy inherits total_in");
    assert_eq!(
        copy.total_out, strm.total_out,
        "the copy inherits total_out"
    );
    assert_eq!(
        copy.adler, strm.adler,
        "the copy inherits the running Adler-32"
    );
    assert_eq!(
        copy.data_type, strm.data_type,
        "the copy inherits the data-type bits",
    );

    // Finish the decode independently through both halves, from the same point in
    // the input, and require byte-identical recovery of the whole payload.
    let mut copy_out = original_out.clone();
    let remaining = &compressed[consumed..];
    for (stream, sink, which) in [
        (&mut strm, &mut original_out, "original"),
        (&mut copy, &mut copy_out, "copy"),
    ] {
        let mut tail = vec![0u8; payload.len()];
        let outcome = inflate(stream, remaining, &mut tail, Z_FINISH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "{which}: the resumed decode must reach the end of the stream",
        );
        sink.extend_from_slice(&tail[..outcome.produced]);
        assert_eq!(&**sink, &payload[..], "{which}: lossless recovery");
        assert_eq!(rc(inflate_end(stream)), ReturnCode::Ok, "{which}: end");
    }
    assert_eq!(
        original_out, copy_out,
        "the copy must decode byte-identically to the original",
    );
}

// ===========================================================================
// Forced Z_MEM_ERROR — a byte-capped allocator via the z_stream hooks
// (faithful port of infcover.c's `mem_*` forced-allocation-failure steps)
// ===========================================================================

/// A byte-capped allocator, the Rust analogue of `infcover.c`'s `mem_*` harness.
///
/// It hands out memory from the global allocator until a fixed byte budget is
/// exhausted, after which it returns null — exactly the failure `infcover.c`
/// injects through the `z_stream` `zalloc`/`zfree` hooks to drive the inflate
/// engine into `Z_MEM_ERROR`. `zlib-rs` allocation is *fallible* at the FFI
/// boundary (AAP §0.6.3): a caller hook that returns null propagates to
/// [`ReturnCode::MemError`] with **no** global-allocator fallback, so this hook
/// forces the error just as in C.
struct MemCap {
    /// Remaining byte budget. Any single request larger than this fails.
    budget: core::cell::Cell<usize>,
}

/// Every allocation is prefixed with a `usize` header recording its total size
/// so [`cap_free`] can reconstruct the [`Layout`](core::alloc::Layout).
const CAP_HEADER: usize = core::mem::size_of::<usize>();

/// `zalloc` hook: allocate `items * size` bytes when within budget, else null.
unsafe extern "C" fn cap_alloc(opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
    // SAFETY: `opaque` is the `&MemCap` installed on the stream below, which
    // outlives every inflate call in the test.
    let cap = unsafe { &*(opaque as *const MemCap) };
    let bytes = (items as usize).saturating_mul(size as usize);
    if bytes == 0 || bytes > cap.budget.get() {
        return ptr::null_mut();
    }
    let total = bytes + CAP_HEADER;
    let layout = core::alloc::Layout::from_size_align(total, CAP_HEADER).expect("valid layout");
    // SAFETY: `layout` has non-zero size.
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `raw` owns `total` bytes aligned for `usize`; record the size in
    // the header and hand back the pointer just past it.
    unsafe { *(raw as *mut usize) = total };
    cap.budget.set(cap.budget.get() - bytes);
    // SAFETY: the returned pointer lies one header inside the allocation.
    unsafe { raw.add(CAP_HEADER) as *mut c_void }
}

/// `zfree` hook: reconstruct the layout from the header, refund the budget, and
/// release the block.
unsafe extern "C" fn cap_free(opaque: *mut c_void, address: *mut c_void) {
    if address.is_null() {
        return;
    }
    // SAFETY: `address` was returned by `cap_alloc`, so its `usize` size header
    // sits in the `CAP_HEADER` bytes immediately before it; stepping back by
    // `CAP_HEADER` stays within that same allocation.
    let raw = unsafe { (address as *mut u8).sub(CAP_HEADER) };
    // SAFETY: `raw` points at the `usize` size header written by `cap_alloc`.
    let total = unsafe { *(raw as *const usize) };
    let layout = core::alloc::Layout::from_size_align(total, CAP_HEADER).expect("valid layout");
    // SAFETY: `opaque` is the live `&MemCap` for the stream.
    let cap = unsafe { &*(opaque as *const MemCap) };
    cap.budget.set(cap.budget.get() + (total - CAP_HEADER));
    // SAFETY: `raw`/`layout` match the original allocation from `cap_alloc`.
    unsafe { std::alloc::dealloc(raw, layout) };
}

/// Port of the forced-`Z_MEM_ERROR` steps of `infcover.c`'s `mem_*` harness.
///
/// Reference zlib routes **both** inflate allocations through the caller's
/// `zalloc`: `inflateInit2_` allocates the state struct
/// (`ZALLOC(strm, 1, sizeof(struct inflate_state))`) up front, and the sliding
/// window is allocated lazily on first use. `infcover.c` drives each into
/// `Z_MEM_ERROR` by capping the allocator, so this test pins both:
///
/// 1. **State allocation fails at init.** With a budget below the state size,
///    `inflateInit2_` cannot obtain the state reservation and returns
///    `Z_MEM_ERROR` — matching C, and validating that the inflate *state*
///    allocation is routed through the caller's hook. A state allocation that
///    bypassed the hook would wrongly succeed here.
/// 2. **Window allocation fails after init.** With a budget large enough for the
///    state but below the state + window size, `inflateInit2_` succeeds and the
///    subsequent `inflate` fails when it tries to grow the lazily-allocated
///    window — the outcome `infcover.c` pins under its tight allocation limit.
#[test]
fn mem_limit_forces_mem_error() {
    // Bytes the inflate state reservation charges the caller's `zalloc` at
    // `inflateInit2_`. Reference zlib passes `sizeof(struct inflate_state)`; this
    // port passes `size_of::<InflateState>()` because the region it gets back is
    // where the state actually lives, and this port's state is legitimately larger
    // than C's — see `external_allocator_observes_every_deflate_request` for the
    // full argument. Only the request *count* and the failure *timing* are fixed
    // by AAP §0.6.5, and both still match C exactly.
    const STATE_SIZE: usize = core::mem::size_of::<zlib_rs::inflate::InflateState>();
    // The raw 8-bit inflate window (`1 << 8`) allocated lazily by `inflate`.
    const WINDOW_8: usize = 1 << 8;

    // --- Scenario 1: budget below the state size => init fails ---------------
    {
        let cap = MemCap {
            // Far below STATE_SIZE, so the state reservation itself cannot be
            // obtained and `inflateInit2_` fails — exactly as C's state `ZALLOC`
            // fails under a tight limit.
            budget: core::cell::Cell::new(200),
        };

        let mut strm = zeroed_stream();
        strm.zalloc = Some(cap_alloc);
        strm.zfree = Some(cap_free);
        strm.opaque = (&cap as *const MemCap) as *mut c_void;

        // SAFETY: `strm` is a valid, caller-owned `z_stream` with a live capped
        // allocator installed; `c"1"` is a valid version whose first byte
        // matches the library version, and the reported size is the true
        // `sizeof(z_stream)`.
        let init = unsafe {
            inflateInit2_(
                &mut strm,
                -8,
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(
            init,
            ReturnCode::MemError.as_c_int(),
            "a budget below the state size must fail inflateInit2_ with Z_MEM_ERROR \
             (the inflate state allocation is routed through the caller's hook)",
        );
        // Init failed, so no state was installed and there is nothing to free.
    }

    // --- Scenario 2: budget fits the state but not the window ----------------
    {
        let cap = MemCap {
            // Enough for the state reservation, but the remaining `WINDOW_8 - 1`
            // bytes are one short of the window, so the lazy window allocation
            // fails during `inflate`.
            budget: core::cell::Cell::new(STATE_SIZE + WINDOW_8 - 1),
        };

        let mut strm = zeroed_stream();
        strm.zalloc = Some(cap_alloc);
        strm.zfree = Some(cap_free);
        strm.opaque = (&cap as *const MemCap) as *mut c_void;

        // SAFETY: as in scenario 1 — a valid caller-owned `z_stream` with a live
        // capped allocator, a matching version byte, and the true stream size.
        let init = unsafe {
            inflateInit2_(
                &mut strm,
                -8,
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "a budget covering the state (but not the window) must let inflateInit2_ succeed",
        );

        // A minimal raw-DEFLATE fragment that drives the engine to grow its
        // window (mirrors the `\x63\x00` feed in infcover.c's mem coverage).
        let input = [0x63u8, 0x00];
        let mut out = [0u8; 1];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        // SAFETY: `strm` holds a valid inflate state; `next_in`/`next_out` point
        // at the live local buffers with matching `avail_*` counts.
        let ret = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            ret,
            ReturnCode::MemError.as_c_int(),
            "a budget below the state + window size must force Z_MEM_ERROR on the window",
        );

        // SAFETY: `strm` was initialized by `inflateInit2_`; `inflateEnd`
        // reclaims the state box (allocated globally) and refunds the capped
        // state reservation through `zfree`.
        let end = unsafe { inflateEnd(&mut strm) };
        assert_eq!(end, ReturnCode::Ok.as_c_int(), "inflateEnd must succeed");
    }
}

/// Compresses `payload` with a preset dictionary at `windowBits = 15`, returning
/// the zlib stream (whose header carries `FDICT` and the dictionary id) together
/// with that id — C `test_dict_deflate`'s `dictId = c_stream.adler`.
fn compress_with_dictionary(payload: &[u8], dictionary: &[u8]) -> (Vec<u8>, u32) {
    let mut strm = ZStream::new();
    assert_eq!(
        rc(deflate_init2(
            &mut strm,
            6,
            Z_DEFLATED,
            15,
            DEF_MEM_LEVEL,
            Strategy::Default,
        )),
        ReturnCode::Ok,
        "deflate_init2 for the dictionary fixture",
    );
    assert_eq!(
        rc(deflate_set_dictionary(&mut strm, dictionary)),
        ReturnCode::Ok,
        "deflate_set_dictionary must be accepted before any input",
    );
    // C reads the dictionary id out of `adler` immediately after the call.
    let dict_id = strm.adler;

    let mut out = vec![0u8; payload.len() + payload.len() / 2 + 1_024];
    let outcome = deflate(&mut strm, payload, &mut out, Z_FINISH);
    assert_eq!(
        outcome.code,
        ReturnCode::StreamEnd,
        "the dictionary fixture must finish in one call",
    );
    assert_eq!(outcome.consumed, payload.len());
    out.truncate(outcome.produced);
    assert_eq!(rc(deflate_end(&mut strm)), ReturnCode::Ok);
    (out, dict_id)
}

/// Port of the forced-`Z_MEM_ERROR` step inside `infcover.c`'s `NEED_DICT`
/// branch (`infcover.c` L323-L332) — the third allocation site C drives into
/// `Z_MEM_ERROR`, and the only one reached through `inflateSetDictionary`.
///
/// When `inflate` asks for a dictionary, C tightens the limit to a single byte
/// and then installs one:
///
/// ```text
///     ret = inflateSetDictionary(&strm, in, 1);   assert(ret == Z_DATA_ERROR);
///     mem_limit(&strm, 1);
///     ret = inflateSetDictionary(&strm, out, 0);  assert(ret == Z_MEM_ERROR);
///     mem_limit(&strm, 0);
///     ((struct inflate_state *)strm.state)->mode = DICT;
///     ret = inflateSetDictionary(&strm, out, 0);  assert(ret == Z_OK);
///     ret = inflate(&strm, Z_NO_FLUSH);           assert(ret == Z_BUF_ERROR);
/// ```
///
/// `inflateSetDictionary` loads the dictionary through `updatewindow`, which
/// allocates the sliding window whenever it is still null (`inflate.c`
/// L259-L263), so a budget covering the state but not the window turns the
/// dictionary load itself into `Z_MEM_ERROR` and latches `state->mode = MEM`
/// (C L1211-L1214). Neither [`mem_limit_forces_mem_error`] (the state
/// reservation and the *decode-path* window) nor
/// [`window_allocation_failure_keeps_the_bytes_but_not_the_totals`] (the
/// `inf_leave` window) passes through that call, so it is pinned here.
///
/// The one step C takes that an integration test cannot is the `mode = DICT`
/// poke on L330. It exists only because C's own preceding failure latched
/// `mode = MEM`, and private state is not writable from here — so what the poke
/// papers over is asserted instead: after the refusal the mode really is `MEM`,
/// which is why a repeat `inflateSetDictionary` is rejected with
/// `Z_STREAM_ERROR` and a resumed `inflate` reports `Z_MEM_ERROR` (C
/// `case MEM: return Z_MEM_ERROR;`). C's L331-L332 success leg is then
/// reproduced on a stream that reaches `DICT` legitimately, once through the C
/// ABI on C's own fixture and once through the idiomatic API on a real
/// dictionary-compressed payload that must decode byte-exactly.
#[test]
fn set_dictionary_window_allocation_failure_is_a_mem_error() {
    // Reserved through the caller's `zalloc` by `inflateInit2_`, sized by this
    // port's own state rather than C's (see `mem_limit_forces_mem_error`).
    const STATE_SIZE: usize = core::mem::size_of::<zlib_rs::inflate::InflateState>();
    // `windowBits = 8` => the 256-byte window `updatewindow` allocates lazily.
    const WINDOW_8: usize = 1 << 8;
    // C's `inf("8 b8 0 0 0 1", "need dictionary", 0, 8, 0, Z_NEED_DICT)` fixture:
    // CMF `0x08` (CM 8, CINFO 0 => a 256-byte window), FLG `0xb8` (FDICT set,
    // and `0x08b8 % 31 == 0`), then the big-endian dictionary id `1` — which is
    // `adler32` of the *empty* dictionary, so C's zero-length `out` is the
    // correct dictionary for this stream.
    const NEED_DICT_ID_ONE: [u8; 6] = [0x08, 0xb8, 0x00, 0x00, 0x00, 0x01];

    // --- C ABI: the tight budget refuses the dictionary window ---------------
    {
        let cap = MemCap {
            // Enough for the state reservation, one byte short of the window, so
            // the allocation inside `updatewindow` is the request that fails —
            // C's `mem_limit(&strm, 1)`.
            budget: Cell::new(STATE_SIZE + WINDOW_8 - 1),
        };

        let mut strm = zeroed_stream();
        strm.zalloc = Some(cap_alloc);
        strm.zfree = Some(cap_free);
        strm.opaque = (&cap as *const MemCap) as *mut c_void;

        // SAFETY: `strm` is a valid, caller-owned `z_stream` with a live capped
        // allocator installed; `c"1"` is a valid version whose first byte matches
        // the library version, and the reported size is the true
        // `sizeof(z_stream)`.
        let init = unsafe {
            inflateInit2_(
                &mut strm,
                8,
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "a budget covering the state must let inflateInit2_ succeed",
        );

        let mut out = [0u8; 1];
        strm.next_in = NEED_DICT_ID_ONE.as_ptr();
        strm.avail_in = NEED_DICT_ID_ONE.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        // SAFETY: `strm` holds a valid inflate state and its cursors describe the
        // live local buffers with matching `avail_*` counts.
        let ret = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            ret,
            ReturnCode::NeedDict.as_c_int(),
            "the FDICT header must stop the decode with Z_NEED_DICT",
        );
        assert_eq!(
            strm.adler, 1,
            "the requested dictionary id reaches the caller through adler",
        );

        // C L324-L325: a one-byte dictionary whose Adler-32 is not the requested
        // id. The identifier is checked *before* `updatewindow` runs, so even
        // under the tight budget this is Z_DATA_ERROR and not Z_MEM_ERROR — a
        // port that allocated first would report the wrong code here.
        // SAFETY: `strm` holds a valid inflate state and the fixture provides at
        // least the one byte named by the length argument.
        let wrong = unsafe { inflateSetDictionary(&mut strm, NEED_DICT_ID_ONE.as_ptr(), 1) };
        assert_eq!(
            wrong,
            ReturnCode::DataError.as_c_int(),
            "a mismatched dictionary id must be rejected before any allocation",
        );

        // C L326-L329: the *correct* (empty) dictionary now passes the id check
        // and reaches `updatewindow`, whose window allocation the budget refuses.
        // SAFETY: `strm` holds a valid inflate state; a zero length reads nothing
        // through the pointer, exactly as C's `inflateSetDictionary(&strm, out, 0)`.
        let capped = unsafe { inflateSetDictionary(&mut strm, out.as_ptr(), 0) };
        assert_eq!(
            capped,
            ReturnCode::MemError.as_c_int(),
            "a refused dictionary window must surface Z_MEM_ERROR (inflate.c L1211-L1214)",
        );

        // The failure latched `mode = MEM`, which is precisely why C has to poke
        // `mode = DICT` before retrying: the stream is no longer awaiting a
        // dictionary, so the guard `wrap != 0 && mode != DICT` rejects a repeat.
        // SAFETY: as above — a valid inflate state and a zero-length dictionary.
        let again = unsafe { inflateSetDictionary(&mut strm, out.as_ptr(), 0) };
        assert_eq!(
            again,
            ReturnCode::StreamError.as_c_int(),
            "once mode == MEM the stream is not awaiting a dictionary any more",
        );

        // C `case MEM: return Z_MEM_ERROR;` — a resumed decode reports the latched
        // failure and advances nothing.
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: `strm` holds a valid inflate state; `avail_in` is 0 and
        // `next_out` points at the live local buffer.
        let resumed = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            resumed,
            ReturnCode::MemError.as_c_int(),
            "a stream latched in MEM mode must keep reporting Z_MEM_ERROR",
        );
        assert_eq!(
            strm.avail_out,
            out.len() as c_uint,
            "the MEM arm returns without the RESTORE() epilogue, so nothing advances",
        );

        // SAFETY: `strm` was initialized by `inflateInit2_` and still owns its
        // state; `inflateEnd` refunds the capped state reservation through `zfree`.
        let end = unsafe { inflateEnd(&mut strm) };
        assert_eq!(end, ReturnCode::Ok.as_c_int(), "inflateEnd must succeed");
    }

    // --- C ABI: C L330-L332, on a stream that reaches DICT legitimately ------
    {
        let cap = MemCap {
            // C's `mem_limit(&strm, 0)`: the window now fits.
            budget: Cell::new(STATE_SIZE + WINDOW_8),
        };

        let mut strm = zeroed_stream();
        strm.zalloc = Some(cap_alloc);
        strm.zfree = Some(cap_free);
        strm.opaque = (&cap as *const MemCap) as *mut c_void;

        // SAFETY: as in the first scenario — a valid caller-owned `z_stream` with
        // a live capped allocator, a matching version byte, and the true size.
        let init = unsafe {
            inflateInit2_(
                &mut strm,
                8,
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "inflateInit2_ must succeed"
        );

        let mut out = [0u8; 1];
        strm.next_in = NEED_DICT_ID_ONE.as_ptr();
        strm.avail_in = NEED_DICT_ID_ONE.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;

        // SAFETY: valid inflate state, cursors describing the live buffers.
        let ret = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(ret, ReturnCode::NeedDict.as_c_int(), "Z_NEED_DICT");

        // C L331: with memory available the same empty dictionary is accepted.
        // SAFETY: valid inflate state; a zero length reads nothing.
        let loaded = unsafe { inflateSetDictionary(&mut strm, out.as_ptr(), 0) };
        assert_eq!(
            loaded,
            ReturnCode::Ok.as_c_int(),
            "the correct dictionary must be accepted once the window fits",
        );

        // C L332: the fixture carries no compressed payload and `avail_in` is
        // spent, so the resumed decode makes no progress => Z_BUF_ERROR.
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: valid inflate state; `avail_in` is 0 and `next_out` is live.
        let resumed = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            resumed,
            ReturnCode::BufError.as_c_int(),
            "no input and no progress after the dictionary load => Z_BUF_ERROR",
        );

        // SAFETY: `strm` was initialized by `inflateInit2_` and still owns its
        // state; the window and state reservations are refunded through `zfree`.
        let end = unsafe { inflateEnd(&mut strm) };
        assert_eq!(end, ReturnCode::Ok.as_c_int(), "inflateEnd must succeed");
    }

    // --- Idiomatic API: the same refusal, then a real dictionary decode ------
    {
        // A dictionary the payload genuinely re-uses, so the decode really
        // depends on it rather than merely tolerating it.
        const DICTIONARY: &[u8] = b"the quick brown fox jumps over the lazy dog";
        const PAYLOAD: &[u8] =
            b"the quick brown fox jumps over the lazy dog, and the lazy dog naps on";
        let (compressed, dict_id) = compress_with_dictionary(PAYLOAD, DICTIONARY);
        let state_size = core::mem::size_of::<zlib_rs::inflate::InflateState>();
        // `windowBits = 15` => a 32 KiB window, requested as C's
        // `ZALLOC(strm, 1U << wbits, sizeof(unsigned char))`.
        let window_15 = 1usize << 15;

        // Budget between the state and the window: the dictionary load is refused.
        let mut strm =
            ZStream::with_allocator(ExternalAllocator::with_budget(state_size + window_15 - 1));
        assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
        let mut out = vec![0u8; PAYLOAD.len() + 64];
        let header = inflate(&mut strm, &compressed, &mut out, Z_NO_FLUSH);
        assert_eq!(
            header.code,
            ReturnCode::NeedDict,
            "an FDICT stream must ask for its dictionary",
        );
        assert_eq!(
            strm.adler, dict_id,
            "the encoder's dictionary id must be the one the decoder requests",
        );
        assert_eq!(
            inflate_set_dictionary(&mut strm, DICTIONARY),
            Err(ZlibError::MemError),
            "a refused dictionary window must surface MemError through the safe API too",
        );
        assert!(
            strm.allocator().count_of(window_15, 1) >= 1,
            "the dictionary load must request the window through the allocator",
        );
        assert_eq!(
            inflate_set_dictionary(&mut strm, DICTIONARY),
            Err(ZlibError::StreamError),
            "the latched MEM mode is no longer awaiting a dictionary",
        );
        let resumed = inflate(
            &mut strm,
            &compressed[header.consumed..],
            &mut out,
            Z_NO_FLUSH,
        );
        assert_eq!(
            resumed.code,
            ReturnCode::MemError,
            "the resumed decode reports the latched allocation failure",
        );
        assert_eq!(resumed.consumed, 0, "the MEM arm consumes nothing");
        assert_eq!(resumed.produced, 0, "the MEM arm produces nothing");
        assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);

        // With memory available the very same stream decodes byte-exactly.
        let mut strm = ZStream::with_allocator(ExternalAllocator::unlimited());
        assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
        let header = inflate(&mut strm, &compressed, &mut out, Z_NO_FLUSH);
        assert_eq!(header.code, ReturnCode::NeedDict);
        assert_eq!(header.produced, 0, "the header alone produces no output");
        assert_eq!(
            rc(inflate_set_dictionary(&mut strm, DICTIONARY)),
            ReturnCode::Ok,
            "the matching dictionary must be accepted",
        );
        let rest = inflate(
            &mut strm,
            &compressed[header.consumed..],
            &mut out,
            Z_FINISH,
        );
        assert_eq!(
            rest.code,
            ReturnCode::StreamEnd,
            "the decode must complete once the dictionary is installed",
        );
        assert_eq!(
            &out[..rest.produced],
            PAYLOAD,
            "a dictionary-compressed payload must be recovered byte-for-byte",
        );
        assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
    }
}

/// `inflateCopy` must allocate the destination through the **source stream's**
/// allocator and report `Z_MEM_ERROR` when that allocator refuses — it must never
/// quietly relocate the copy into the Rust global heap.
///
/// Reference zlib allocates the destination state (and window) with
/// `ZALLOC(source, ...)` and returns `Z_MEM_ERROR` on failure, freeing whatever it
/// already obtained (`inflate.c` L1340-L1350). A caller who installed a bounded
/// allocator therefore observes the copy failing once its budget is spent. This
/// test pins both directions with the same capped allocator used above:
///
/// 1. **Budget for two states** — the copy succeeds and both streams can be
///    torn down through the caller's `zfree`.
/// 2. **Budget for one state** — the copy fails with `Z_MEM_ERROR` and installs
///    nothing on the destination.
#[test]
fn inflate_copy_honors_the_caller_allocator_budget() {
    const STATE_SIZE: usize = core::mem::size_of::<zlib_rs::inflate::InflateState>();

    /// Initializes `strm` as a raw 8-bit-window inflate stream on `cap`.
    ///
    /// # Safety
    ///
    /// `strm` must be a valid, caller-owned `z_stream` and `cap` must outlive
    /// every inflate call made on `strm`.
    unsafe fn init_on(strm: &mut z_stream, cap: &MemCap) -> c_int {
        strm.zalloc = Some(cap_alloc);
        strm.zfree = Some(cap_free);
        strm.opaque = (cap as *const MemCap) as *mut c_void;
        // SAFETY: the caller guarantees `strm` is a valid `z_stream` with a live
        // capped allocator; `c"1"` matches the library's major version byte and
        // the reported size is the true `sizeof(z_stream)`.
        unsafe {
            inflateInit2_(
                strm,
                -8,
                c"1".as_ptr(),
                core::mem::size_of::<z_stream>() as c_int,
            )
        }
    }

    // --- Scenario 1: budget for two states => the copy succeeds ---------------
    {
        let cap = MemCap {
            budget: core::cell::Cell::new(2 * STATE_SIZE),
        };
        let mut src = zeroed_stream();
        // SAFETY: `src` is a live local `z_stream` and `cap` outlives it.
        let src_rc = unsafe { init_on(&mut src, &cap) };
        assert_eq!(
            src_rc,
            ReturnCode::Ok.as_c_int(),
            "the source stream must initialize within budget"
        );

        let mut dst = zeroed_stream();
        // SAFETY: both pointers are live, caller-owned `z_stream`s; `src` holds a
        // valid inflate state and `dst` holds none yet.
        let rc = unsafe { inflateCopy(&mut dst, &mut src) };
        assert_eq!(
            rc,
            ReturnCode::Ok.as_c_int(),
            "a budget covering a second state must let inflateCopy succeed"
        );
        assert!(!dst.state.is_null(), "a successful copy installs a state");
        assert!(dst.zalloc.is_some(), "the copy inherits the allocator");

        // SAFETY: both streams hold states installed by the calls above.
        let dst_end_rc = unsafe { inflateEnd(&mut dst) };
        assert_eq!(
            dst_end_rc,
            ReturnCode::Ok.as_c_int(),
            "the copy must tear down cleanly"
        );
        // SAFETY: as above.
        assert_eq!(unsafe { inflateEnd(&mut src) }, ReturnCode::Ok.as_c_int());
    }

    // --- Scenario 2: budget for one state => the copy reports Z_MEM_ERROR -----
    {
        let cap = MemCap {
            // Exactly one state: after the source initializes, nothing is left,
            // so the destination's state reservation cannot be obtained.
            budget: core::cell::Cell::new(STATE_SIZE),
        };
        let mut src = zeroed_stream();
        // SAFETY: `src` is a live local `z_stream` and `cap` outlives it.
        let src_rc = unsafe { init_on(&mut src, &cap) };
        assert_eq!(
            src_rc,
            ReturnCode::Ok.as_c_int(),
            "the source stream must still initialize within budget"
        );

        let mut dst = zeroed_stream();
        // SAFETY: both pointers are live, caller-owned `z_stream`s.
        let rc = unsafe { inflateCopy(&mut dst, &mut src) };
        assert_eq!(
            rc,
            ReturnCode::MemError.as_c_int(),
            "an exhausted caller allocator must make inflateCopy return Z_MEM_ERROR \
             instead of relocating the copy into the global heap",
        );
        assert!(
            dst.state.is_null(),
            "a failed copy must not install a state on the destination"
        );

        // SAFETY: `src` still holds the state installed by `init_on`.
        assert_eq!(unsafe { inflateEnd(&mut src) }, ReturnCode::Ok.as_c_int());
    }
}

// ===========================================================================
// Rust-native allocator accounting
//
// The C harness above reaches the allocator through `z_stream.zalloc`/`zfree`,
// which is the only customization point C offers. The Rust port adds the safe
// `Allocator` trait, and these tests pin that it is a genuine customization
// point: an implementation written with nothing but this crate's public, safe
// surface observes every engine request — including the engine-state footprint
// C charges with `ZALLOC(strm, 1, sizeof(deflate_state))` (`deflate.c` L440) —
// and a refusal becomes `Z_MEM_ERROR` instead of a silent fall back to the Rust
// global heap.
//
// Being an integration test, this file is a separate crate, so nothing here can
// use an item a downstream user cannot. In particular no `AllocHook` is built:
// supplying raw storage requires the `unsafe` FFI constructor, whereas deciding
// and accounting — which is what a bounded allocator exists to do — is fully
// expressible in safe code.
// ===========================================================================

/// A downstream-style [`Allocator`] that records and budgets every request.
///
/// It overrides only the two allocation methods and leaves
/// [`Allocator::hook`] at its inactive default, which is exactly how a
/// Rust-native allocator is meant to be written. Storage still comes from the
/// public [`AllocBuffer::try_zeroed_items`] constructor — a global-allocator
/// region, because handing the engines foreign memory is an `unsafe` operation
/// on stable Rust — but *whether* each request is served, and with what shape,
/// is entirely this type's decision.
struct ExternalAllocator {
    /// Remaining byte budget. [`usize::MAX`] means effectively unlimited.
    budget: Cell<usize>,
    /// Every `(items, item_size)` pair the engines asked for, in call order,
    /// including the pair that was refused.
    requests: RefCell<Vec<(usize, usize)>>,
}

impl ExternalAllocator {
    /// An allocator that serves every request.
    fn unlimited() -> Self {
        Self::with_budget(usize::MAX)
    }

    /// An allocator that refuses any request which would exceed `bytes` in
    /// total, mirroring `infcover.c`'s capped `mem_limit` harness.
    fn with_budget(bytes: usize) -> Self {
        Self {
            budget: Cell::new(bytes),
            requests: RefCell::new(Vec::new()),
        }
    }

    /// Records the request and reports whether the budget can absorb it. A
    /// refusal deducts nothing, so a later smaller request can still succeed —
    /// the same semantics as a real bounded arena.
    fn charge(&self, items: usize, item_size: usize) -> bool {
        self.requests.borrow_mut().push((items, item_size));
        let Some(bytes) = items.checked_mul(item_size) else {
            return false;
        };
        let left = self.budget.get();
        if bytes > left {
            return false;
        }
        self.budget.set(left - bytes);
        true
    }

    /// The recorded request list.
    fn requests(&self) -> Vec<(usize, usize)> {
        self.requests.borrow().clone()
    }

    /// How many times `(items, item_size)` was requested.
    fn count_of(&self, items: usize, item_size: usize) -> usize {
        self.requests
            .borrow()
            .iter()
            .filter(|pair| **pair == (items, item_size))
            .count()
    }
}

impl Allocator for ExternalAllocator {
    fn allocate_zeroed<T>(&self, count: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        // C's element-shaped `ZALLOC(strm, n, sizeof(Pos))` split.
        self.allocate_zeroed_items(count, core::mem::size_of::<T>())
    }

    fn allocate_zeroed_items<T>(&self, items: usize, item_size: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        if !self.charge(items, item_size) {
            return None;
        }
        AllocBuffer::try_zeroed_items(items, item_size, self.hook())
    }
}

/// Every deflate buffer — and the engine-state footprint — must be requested
/// through the [`Allocator`] trait, with the same `(items, item_size)` shapes C
/// passes to `zalloc`.
///
/// This is the property the trait exists for: before it held, an external
/// implementation was consulted for nothing and the engine quietly used the Rust
/// global heap.
#[test]
fn external_allocator_observes_every_deflate_request() {
    // `deflateInit2_(level=6, Z_DEFLATED, windowBits=15, memLevel=8, default)`.
    const W_BITS: i32 = 15;
    let w_size = 1usize << W_BITS;
    let lit_bufsize = 1usize << (DEF_MEM_LEVEL as u32 + 6);
    // `hash_bits = memLevel + 7` (`deflate.c` L452).
    let hash_size = 1usize << (DEF_MEM_LEVEL as u32 + 7);

    let alloc = ExternalAllocator::unlimited();
    let mut strm = ZStream::with_allocator(alloc);
    assert_eq!(
        rc(deflate_init2(
            &mut strm,
            6,
            Z_DEFLATED,
            W_BITS,
            DEF_MEM_LEVEL,
            Strategy::Default,
        )),
        ReturnCode::Ok,
        "an unlimited external allocator must satisfy deflateInit2"
    );

    let requests = strm.allocator().requests();

    // C charges the state object first and checks it immediately
    // (`deflate.c` L440-L442), so the very first request an external allocator
    // sees is the one-item engine-state pair.
    //
    // The `size` argument is `size_of::<DeflateState>()` rather than C's
    // `sizeof(deflate_state)` (`DeflateState::C_LAYOUT_SIZE`, 5968 on LP64)
    // because the region this request secures *is* where the state lives
    // (AAP §0.6.3 has-hook clause), and this port's state is legitimately larger
    // than C's — a block of C's `sizeof` could not hold it, so holding the state
    // in the caller's memory and advertising C's byte count are mutually
    // exclusive. AAP §0.6.5 fixes the allocation **count** and the **failure
    // timing**, both of which still match; no zlib contract lets a caller assert
    // a particular `size` argument, and C's own value moves with `LIT_MEM` and
    // pointer width.
    assert_eq!(
        requests.first().copied(),
        Some((1, core::mem::size_of::<DeflateState>())),
        "the engine-state footprint must be the first request, as in C, and it must \
         be the one-item region the state itself occupies"
    );

    // The doubled sliding window, the `prev` chain, and the `head` hash table
    // are all two-byte-element requests; C passes `(w_size, 2 * sizeof(Byte))`,
    // `(w_size, sizeof(Pos))`, and `(hash_size, sizeof(Pos))` (`deflate.c`
    // L458-L460). With windowBits 15 and memLevel 8 all three are (32768, 2).
    assert_eq!(w_size, hash_size, "windowBits 15 / memLevel 8 sizing");
    assert!(
        strm.allocator().count_of(w_size, 2) >= 3,
        "window, prev, and head must each be requested through the allocator; saw {requests:?}"
    );

    // The pending buffer: C's `ZALLOC(strm, lit_bufsize, LIT_BUFS)` with
    // `LIT_BUFS == 4` (`deflate.c` L505, `deflate.h` L28 leaves `LIT_MEM`
    // undefined).
    assert!(
        strm.allocator().count_of(lit_bufsize, 4) >= 1,
        "the pending buffer must be requested through the allocator; saw {requests:?}"
    );

    // Exactly five requests, matching C `deflateInit2_` — the state plus
    // `window`, `prev`, `head` and the *single* `pending_buf` that carries the
    // overlaid symbol region (`deflate.c` L440-L520). A sixth request would mean
    // the symbol buffer was split out into an allocation of its own.
    assert_eq!(
        requests,
        vec![
            (1, core::mem::size_of::<DeflateState>()),
            (w_size, 2),
            (w_size, 2),
            (hash_size, 2),
            (lit_bufsize, 4),
        ],
        "deflateInit2 must present C's exact five-request schedule"
    );

    // Nothing may have been taken from the global heap behind the allocator's
    // back: the byte total it was charged must cover the whole documented
    // footprint (AAP §0.6.3 enumerates state + doubled window + prev + head +
    // the pending/symbol buffer).
    let charged: usize = requests
        .iter()
        .map(|(items, size)| items * size)
        .sum::<usize>();
    let minimum = core::mem::size_of::<DeflateState>()
        + 2 * w_size
        + 2 * w_size
        + 2 * hash_size
        + 4 * lit_bufsize;
    assert!(
        charged >= minimum,
        "the allocator was charged {charged} bytes but the deflate footprint is at least {minimum}"
    );

    assert_eq!(rc(deflate_end(&mut strm)), ReturnCode::Ok);
}

/// A refused state reservation must fail `deflateInit2` with `Z_MEM_ERROR`, and
/// must fail it *immediately* — C checks its state `ZALLOC` before requesting any
/// working buffer (`deflate.c` L440-L442).
#[test]
fn external_allocator_refusal_fails_deflate_init() {
    let state_size = core::mem::size_of::<DeflateState>();
    let alloc = ExternalAllocator::with_budget(state_size - 1);
    let mut strm = ZStream::with_allocator(alloc);

    assert_eq!(
        deflate_init2(
            &mut strm,
            6,
            Z_DEFLATED,
            15,
            DEF_MEM_LEVEL,
            Strategy::Default
        ),
        Err(ZlibError::MemError),
        "an external allocator that refuses the state reservation must fail init \
         with Z_MEM_ERROR rather than silently using the global heap"
    );
    assert_eq!(
        strm.allocator().requests(),
        vec![(1, state_size)],
        "init must stop at the first refused request, exactly as C does"
    );
}

/// The inflate side of the same contract, and the Rust-native twin of
/// [`mem_limit_forces_mem_error`]: the state reservation is charged at
/// `inflateInit2` and the window lazily during `inflate`, so a budget between
/// the two makes init succeed and the first `inflate` fail.
#[test]
fn external_allocator_refusal_fails_inflate_init_then_window() {
    let state_size = core::mem::size_of::<zlib_rs::inflate::InflateState>();
    // The raw 8-bit window (`1 << 8`) `inflate` grows on first use.
    let window_8 = 1usize << 8;
    // A minimal raw-DEFLATE fragment that drives the engine to grow its window
    // (the same feed `mem_limit_forces_mem_error` uses).
    let input = [0x63u8, 0x00];

    // --- Budget below the state size => init fails ---------------------------
    {
        let mut strm = ZStream::with_allocator(ExternalAllocator::with_budget(state_size - 1));
        assert_eq!(
            inflate_init2(&mut strm, -8),
            Err(ZlibError::MemError),
            "a refused state reservation must fail inflate_init2"
        );
        assert_eq!(
            strm.allocator().requests(),
            vec![(1, state_size)],
            "the state reservation is the only request an init failure makes"
        );
    }

    // --- Budget for the state but not the window => `inflate` fails ----------
    {
        let mut strm =
            ZStream::with_allocator(ExternalAllocator::with_budget(state_size + window_8 - 1));
        assert_eq!(
            rc(inflate_init2(&mut strm, -8)),
            ReturnCode::Ok,
            "a budget covering the state must let inflate_init2 succeed"
        );
        let mut out = [0u8; 1];
        let outcome = inflate(&mut strm, &input, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::MemError,
            "a refused window must surface Z_MEM_ERROR, not a global-heap fallback"
        );
        assert!(
            strm.allocator().count_of(window_8, 1) >= 1,
            "the window must be requested through the allocator as C's \
             ZALLOC(strm, 1U << wbits, sizeof(unsigned char))"
        );
        assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
    }

    // --- Unlimited => the stream decodes normally ---------------------------
    {
        let mut strm = ZStream::with_allocator(ExternalAllocator::unlimited());
        assert_eq!(rc(inflate_init2(&mut strm, -8)), ReturnCode::Ok);
        let mut out = [0u8; 1];
        let outcome = inflate(&mut strm, &input, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::Ok,
            "an unlimited external allocator must not disturb decoding"
        );
        assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
    }
}

/// Serving engine memory through an external allocator must not change a single
/// emitted byte: compressed output stays byte-identical to the default
/// allocator's, and the stream round-trips (User Constraint 1 / AAP §0.6.4).
#[test]
fn external_allocator_round_trip_is_byte_identical() {
    // Mixed-entropy payload: a repeated run plus a deterministic ramp, so both
    // the match finder and the literal path are exercised.
    let mut payload = Vec::with_capacity(40_000);
    for i in 0..40_000usize {
        payload.push(if i % 97 < 40 {
            b'A' + (i % 7) as u8
        } else {
            (i * 31 % 251) as u8
        });
    }

    fn compress_with<A: Allocator>(alloc: A, payload: &[u8]) -> Vec<u8> {
        let mut strm = ZStream::with_allocator(alloc);
        assert_eq!(
            rc(deflate_init2(
                &mut strm,
                6,
                Z_DEFLATED,
                15,
                DEF_MEM_LEVEL,
                Strategy::Default,
            )),
            ReturnCode::Ok
        );
        let mut out = vec![0u8; payload.len() + 1024];
        let outcome = deflate(&mut strm, payload, &mut out, Z_FINISH);
        assert_eq!(outcome.code, ReturnCode::StreamEnd, "deflate must finish");
        assert_eq!(outcome.consumed, payload.len());
        out.truncate(outcome.produced);
        assert_eq!(rc(deflate_end(&mut strm)), ReturnCode::Ok);
        out
    }

    let external = compress_with(ExternalAllocator::unlimited(), &payload);
    let default = compress_with(zlib_rs::DefaultAllocator, &payload);
    assert_eq!(
        external, default,
        "the allocator must not influence a single compressed byte"
    );

    // And the external-allocator output decodes back to the original through an
    // external allocator too.
    let mut strm = ZStream::with_allocator(ExternalAllocator::unlimited());
    assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
    let mut decoded = vec![0u8; payload.len()];
    let outcome = inflate(&mut strm, &external, &mut decoded, Z_FINISH);
    assert_eq!(outcome.code, ReturnCode::StreamEnd);
    assert_eq!(outcome.produced, payload.len());
    assert_eq!(decoded, payload, "round trip must be lossless");
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

// ===========================================================================
// Incrementally delivered input — the `inflate_fast` entry contract
//
// `inffast.c`'s header comment lists five entry assumptions, four of which the
// C driver genuinely guarantees (`mode == LEN`, six input bytes, 258 output
// bytes, `start >= avail_out`). The fifth, `state->bits < 8`, is prose only: C
// neither enforces nor provides it. Its own slow-path code lookups pull whole
// *speculative* bytes — `for (;;) { here = lencode[BITS(lenbits)]; if
// (here.bits <= bits) break; PULLBYTE(); }` (`inflate.c` L924-L928, and
// identically at L976-L980 for the distance code) — then drop only the width of
// the code actually decoded (`inflate.c` L940 / L992). A short code decoded
// after a speculative pull leaves a whole byte buffered, and the very next
// `case LEN` re-enters `inflate_fast` as soon as `have >= 6 && left >= 258`
// (`inflate.c` L914-L922) — with `bits >= 8`. The same holds across an
// `inflate()` call boundary, because the driver's `inf_leave` epilogue does not
// normalize `bits` the way `inflate_fast`'s does.
//
// That makes the condition ordinary rather than exceptional for any caller that
// feeds input in small increments — the canonical `zpipe.c` streaming pattern —
// and reference zlib decodes such streams byte-exactly. The unit tests beside
// `inflate_fast` pin the routine's own behaviour under a carried-in byte; this
// test pins the property end to end through the public streaming API, over the
// chunk sizes and framings that provoke it.
// ===========================================================================

/// Streams `compressed` through inflate `in_chunk` input bytes at a time into an
/// `out_chunk`-byte output buffer — the `zpipe.c` pattern — and returns the final
/// return code together with everything produced.
///
/// Presenting only `in_chunk` bytes per call is what forces the decoder to
/// suspend mid-symbol and resume with bits already buffered.
fn stream_inflate(
    compressed: &[u8],
    window_bits: i32,
    in_chunk: usize,
    out_chunk: usize,
) -> (ReturnCode, Vec<u8>) {
    let mut strm = ZStream::new();
    assert_eq!(
        rc(inflate_init2(&mut strm, window_bits)),
        ReturnCode::Ok,
        "inflate_init2({window_bits})",
    );

    let mut out = Vec::new();
    let mut window = vec![0u8; out_chunk];
    let mut consumed = 0usize;
    let mut code = ReturnCode::Ok;

    // Bounded so a no-progress regression halts instead of hanging, while leaving
    // ample room for the smallest chunk sizes exercised here.
    let cap = compressed.len() * 64 + 4_096;
    for _ in 0..cap {
        let end = core::cmp::min(consumed + in_chunk, compressed.len());
        let outcome = inflate(
            &mut strm,
            &compressed[consumed..end],
            &mut window,
            Z_NO_FLUSH,
        );
        consumed += outcome.consumed;
        out.extend_from_slice(&window[..outcome.produced]);
        code = outcome.code;
        if code == ReturnCode::StreamEnd {
            break;
        }
        assert_eq!(
            code,
            ReturnCode::Ok,
            "a well-formed stream must stay continuable (wb={window_bits}, \
             in_chunk={in_chunk}, out_chunk={out_chunk})",
        );
        // No progress and no input left to give: the loop would spin forever.
        if outcome.consumed == 0 && outcome.produced == 0 && consumed == compressed.len() {
            break;
        }
    }

    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
    (code, out)
}

/// Compresses `payload` with this crate's encoder at `window_bits`.
fn compress_at(payload: &[u8], window_bits: i32) -> Vec<u8> {
    let mut strm = ZStream::new();
    assert_eq!(
        rc(deflate_init2(
            &mut strm,
            6,
            Z_DEFLATED,
            window_bits,
            DEF_MEM_LEVEL,
            Strategy::Default,
        )),
        ReturnCode::Ok,
        "deflate_init2({window_bits})",
    );
    let mut out = vec![0u8; payload.len() + payload.len() / 2 + 1_024];
    let outcome = deflate(&mut strm, payload, &mut out, Z_FINISH);
    assert_eq!(outcome.code, ReturnCode::StreamEnd, "deflate must finish");
    assert_eq!(outcome.consumed, payload.len());
    out.truncate(outcome.produced);
    assert_eq!(rc(deflate_end(&mut strm)), ReturnCode::Ok);
    out
}

/// Incrementally delivered input must decode byte-exactly at every framing.
///
/// Each `(in_chunk, out_chunk)` pair is chosen to sit on a boundary of the fast
/// loop's entry contract: `in_chunk == 6` is exactly `INFLATE_FAST_MIN_INPUT`, so
/// the decoder alternates between the slow path and one fast-loop entry per
/// refill and re-enters it with whatever the slow path left buffered;
/// `out_chunk == 258` is exactly `INFLATE_FAST_MIN_OUTPUT`, the smallest buffer
/// for which the fast loop runs at all. `in_chunk == 32` with a large
/// `out_chunk` is the ordinary `zpipe.c` shape.
///
/// The assertion is byte-exact recovery plus `Z_STREAM_END`, across raw, zlib,
/// gzip and auto-detect framings. The stakes on these combinations are high: a
/// fast loop that asserted C's prose-only `bits < 8` entry claim, or that handed
/// back bytes it never pulled, would fail a `debug_assert!` under
/// `panic = "abort"` (set for **both** profiles, `Cargo.toml` L173-L205), and
/// across the C ABI that is an unrecoverable `SIGABRT` rather than an error
/// return.
#[test]
fn incrementally_delivered_input_decodes_byte_exactly() {
    // Mixed entropy: repeated phrases give the encoder long back-references while
    // the counter and the pseudo-random tail keep the literal alphabet wide, so
    // the stream carries dynamic Huffman blocks with codes of many different
    // widths — which is what makes a speculative byte pull leave a whole byte
    // buffered. Several sizes, including one just over the 258-byte minimum.
    let payloads: [Vec<u8>; 4] = [
        {
            let mut v = Vec::new();
            for i in 0..4u32 {
                v.extend_from_slice(b"the quick brown fox jumps over the lazy dog; ");
                v.extend_from_slice(&i.to_le_bytes());
            }
            v
        },
        {
            let mut v = Vec::new();
            for i in 0..60u32 {
                v.extend_from_slice(b"deflate/inflate parity, byte for byte. ");
                v.extend_from_slice(&i.to_be_bytes());
            }
            v
        },
        {
            // A deterministic pseudo-random tail after compressible text: forces a
            // mix of stored/static/dynamic blocks and wide literal coverage.
            let mut v = Vec::new();
            let mut seed: u32 = 0x1234_5678;
            for i in 0..500u32 {
                v.extend_from_slice(b"zlib-rs ");
                v.extend_from_slice(&i.to_le_bytes());
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                v.push((seed >> 16) as u8);
                v.push((seed >> 8) as u8);
            }
            v
        },
        {
            let mut v = Vec::new();
            let mut seed: u32 = 0x9E37_79B9;
            for _ in 0..20_000 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                v.push((seed >> 24) as u8);
            }
            // Long runs after incompressible data: distance codes reach far back.
            for i in 0..400u32 {
                v.extend_from_slice(b"tail-run-tail-run-tail-run ");
                v.extend_from_slice(&i.to_le_bytes());
            }
            v
        },
    ];

    // (inflate window_bits, deflate window_bits). 47 is inflate-only auto-detect,
    // so its producer is the gzip wrapper it must detect.
    #[cfg(feature = "gzip")]
    const FRAMINGS: [(i32, i32); 4] = [(15, 15), (-15, -15), (31, 31), (47, 31)];
    #[cfg(not(feature = "gzip"))]
    const FRAMINGS: [(i32, i32); 2] = [(15, 15), (-15, -15)];

    for payload in &payloads {
        assert!(
            payload.len() >= 150,
            "fixtures must exceed the 150-byte floor"
        );
        for (inflate_bits, deflate_bits) in FRAMINGS {
            let compressed = compress_at(payload, deflate_bits);
            for in_chunk in [6usize, 8, 32] {
                for out_chunk in [258usize, 4_096] {
                    let (code, decoded) =
                        stream_inflate(&compressed, inflate_bits, in_chunk, out_chunk);
                    assert_eq!(
                        code,
                        ReturnCode::StreamEnd,
                        "wb={inflate_bits} in_chunk={in_chunk} out_chunk={out_chunk}: \
                         the stream must reach Z_STREAM_END",
                    );
                    assert_eq!(
                        decoded.len(),
                        payload.len(),
                        "wb={inflate_bits} in_chunk={in_chunk} out_chunk={out_chunk}: \
                         decoded length must match",
                    );
                    assert!(
                        decoded == *payload,
                        "wb={inflate_bits} in_chunk={in_chunk} out_chunk={out_chunk}: \
                         decoded bytes must match the payload exactly",
                    );
                }
            }
        }
    }
}

// ===========================================================================
// INFLATE_STRICT — the `dmax` guard must cover BOTH decode paths
//
// C compiles the maximum-distance check from a single `INFLATE_STRICT` macro
// into two places: the fast loop (`inffast.c` L156-L162) and the slow path's
// `case DISTEXT` (`inflate.c` L1010-L1015). Those are the only two functional
// `#ifdef INFLATE_STRICT` sites in the whole C library — `infback.c` has none of
// its own, it merely sets `dmax = 32768`, and `inflate.h` L91 just declares the
// field.
//
// Which path decodes any given symbol is decided purely by how much input and
// output the caller happens to supply: the fast loop runs only when `have >= 6
// && left >= 258` (`inflate.c` L914-L922). Porting one guard and not the other
// therefore makes *acceptance of a stream* depend on the caller's buffer sizes —
// the same bytes accepted with a 64-byte output buffer and rejected with a
// 4096-byte one. These tests pin both paths to the same verdict, and pin the
// default build to reference zlib's (accept), per AAP §0.8.2 Divergence 2.
// ===========================================================================

/// Builds a zlib stream whose declared window is smaller than a back-reference it
/// actually contains, together with the payload it encodes.
///
/// The body is produced with **raw** deflate at the full 32 KiB window, so the
/// encoder is free to emit a distance of `distance`; it is then wrapped in a zlib
/// header whose `CINFO` field declares a window of `1 << (cinfo + 8)` bytes.
/// C computes `state->dmax = 1U << (BITS(4) + 8)` in `case HEAD`, so `cinfo == 0`
/// declares `dmax == 256`. The stream is perfectly well-formed DEFLATE — only the
/// *declared* window is too small — which is exactly the case `INFLATE_STRICT`
/// exists to reject and a default build accepts.
fn dmax_violating_stream(cinfo: u8, distance: usize) -> (Vec<u8>, Vec<u8>) {
    // A pseudo-random prefix of exactly `distance` bytes has no internal repeats
    // to match against, so when the prefix's first `tail` bytes are repeated the
    // only back-reference available to the encoder is at distance `distance`.
    const TAIL: usize = 300;
    let mut payload = Vec::with_capacity(distance + TAIL);
    let mut seed: u32 = 99;
    for _ in 0..distance {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        payload.push((seed >> 16) as u8);
    }
    payload.extend_from_within(..TAIL);

    // Raw body at level 9 / full window.
    let body = compress_at(&payload, -15);

    // Two-byte zlib header: CM = 8 (deflate), CINFO = `cinfo`, then FCHECK chosen
    // so the big-endian 16-bit header is a multiple of 31 (RFC 1950 §2.2).
    let cmf = 0x08u32 | ((cinfo as u32) << 4);
    let mut hdr = cmf << 8;
    hdr += 31 - (hdr % 31);

    let mut stream = Vec::with_capacity(body.len() + 6);
    stream.push((hdr >> 8) as u8);
    stream.push((hdr & 0xff) as u8);
    stream.extend_from_slice(&body);
    // Big-endian Adler-32 trailer over the uncompressed data (RFC 1950 §2.2).
    // `1` is the required initial value. (C spells this
    // `adler32(adler32(0L, Z_NULL, 0), ...)`, where the `Z_NULL` call *returns*
    // 1; that null sentinel lives at the FFI boundary, so the idiomatic API is
    // seeded with the literal `1`.)
    let adler = adler32(1, &payload);
    stream.extend_from_slice(&adler.to_be_bytes());

    (stream, payload)
}

/// Streams `compressed` through inflate and reports the outcome *without*
/// asserting success, so an expected data error can be inspected.
///
/// `in_chunk == 0` presents all remaining input on every call. Returns the final
/// return code, `strm.msg`, and the number of bytes produced before stopping.
fn stream_inflate_tolerant(
    compressed: &[u8],
    window_bits: i32,
    in_chunk: usize,
    out_chunk: usize,
) -> (ReturnCode, Option<&'static str>, usize) {
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, window_bits)), ReturnCode::Ok);

    let mut window = vec![0u8; out_chunk];
    let mut consumed = 0usize;
    let mut produced = 0usize;
    let mut code = ReturnCode::Ok;

    let cap = compressed.len() * 64 + 4_096;
    for _ in 0..cap {
        let end = if in_chunk == 0 {
            compressed.len()
        } else {
            core::cmp::min(consumed + in_chunk, compressed.len())
        };
        let outcome = inflate(
            &mut strm,
            &compressed[consumed..end],
            &mut window,
            Z_NO_FLUSH,
        );
        consumed += outcome.consumed;
        produced += outcome.produced;
        code = outcome.code;
        if code != ReturnCode::Ok {
            break;
        }
        if outcome.consumed == 0 && outcome.produced == 0 && consumed == compressed.len() {
            break;
        }
    }

    let msg = strm.msg;
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
    (code, msg, produced)
}

/// The `dmax` verdict must not depend on the caller's buffer sizes.
///
/// `out_chunk` spans both sides of `INFLATE_FAST_MIN_OUTPUT` (258) and `in_chunk`
/// both sides of `INFLATE_FAST_MIN_INPUT` (6), so the sweep decodes the offending
/// distance through the slow path in some rows and the fast loop in others. Every
/// row must reach the *same* verdict:
///
/// * `inflate_strict` on — `Z_DATA_ERROR` with C's exact
///   `"invalid distance too far back"` message, stopping partway through;
/// * feature off (the default) — a complete, byte-exact decode, because reference
///   zlib built without `INFLATE_STRICT` accepts this stream and a default build
///   must accept exactly what reference zlib accepts.
///
/// Both verdicts must hold at every buffer granularity. Without the slow-path
/// `dmax` guard a strict build would accept the stream at every
/// `out_chunk <= 259` and at `out_chunk == 4096` for every small `in_chunk`,
/// rejecting it only when the fast loop happened to decode the offending
/// symbol — which is exactly the buffer-size dependence this test forbids.
#[test]
fn strict_dmax_verdict_is_independent_of_buffer_sizes() {
    // CINFO = 0 declares a 256-byte window; the body carries a distance of 556.
    let (stream, payload) = dmax_violating_stream(0, 556);
    assert_eq!(
        &stream[..2],
        &[0x08, 0x1d],
        "the fixture's zlib header must declare CM=8, CINFO=0",
    );

    // Sanity, independent of the feature: the body itself is valid DEFLATE. Raw
    // framing never consults `dmax` (it is fixed at 32768 by `inflateInit2`), so
    // this recovers the payload in both feature configurations and proves the
    // fixture is well-formed rather than corrupt.
    let raw_body = &stream[2..stream.len() - 4];
    let (raw_code, raw_out) = stream_inflate(raw_body, -15, raw_body.len(), payload.len() + 16);
    assert_eq!(
        raw_code,
        ReturnCode::StreamEnd,
        "the raw body must be valid"
    );
    assert_eq!(raw_out, payload, "the raw body must decode to the payload");

    let strict = cfg!(feature = "inflate_strict");

    for out_chunk in [16usize, 64, 128, 256, 257, 258, 259, 512, 4_096] {
        for in_chunk in [0usize, 1, 2, 5, 6, 7, 64] {
            let (code, msg, produced) = stream_inflate_tolerant(&stream, 15, in_chunk, out_chunk);
            let row = format!("in_chunk={in_chunk} out_chunk={out_chunk}");
            if strict {
                assert_eq!(
                    code,
                    ReturnCode::DataError,
                    "{row}: a strict build must reject a distance beyond dmax",
                );
                assert_eq!(
                    msg,
                    Some("invalid distance too far back"),
                    "{row}: message must match C exactly",
                );
                assert!(
                    produced > 0 && produced < payload.len(),
                    "{row}: the decode must stop at the offending distance, \
                     produced {produced} of {}",
                    payload.len(),
                );
            } else {
                assert_eq!(
                    code,
                    ReturnCode::StreamEnd,
                    "{row}: a default build must accept what reference zlib accepts",
                );
                assert_eq!(
                    produced,
                    payload.len(),
                    "{row}: the whole payload must be recovered",
                );
            }
        }
    }
}

/// A distance *within* the declared window must be accepted even by a strict
/// build, at every buffer size — the guard must reject too-far distances without
/// also rejecting legitimate ones.
#[test]
fn strict_dmax_accepts_distances_within_the_declared_window() {
    // CINFO = 7 declares a 32768-byte window (`1 << (7 + 8)`), which comfortably
    // covers the 556-byte distance the body carries.
    let (stream, payload) = dmax_violating_stream(7, 556);
    assert_eq!(
        stream[0] & 0x0f,
        0x08,
        "the fixture must still declare CM=8 (deflate)",
    );
    assert_eq!(stream[0] >> 4, 7, "the fixture must declare CINFO=7");

    for out_chunk in [16usize, 64, 258, 259, 4_096] {
        for in_chunk in [0usize, 1, 6, 64] {
            let (code, msg, produced) = stream_inflate_tolerant(&stream, 15, in_chunk, out_chunk);
            let row = format!("in_chunk={in_chunk} out_chunk={out_chunk}");
            assert_eq!(
                code,
                ReturnCode::StreamEnd,
                "{row}: a distance within the declared window must be accepted \
                 (msg={msg:?})",
            );
            assert_eq!(produced, payload.len(), "{row}: full payload recovered");
        }
    }
}

// ===========================================================================
// `strm.adler` and total-bookkeeping lifecycle, at the idiomatic-API level
//
// C assigns `strm->adler` at exactly seven places in `inflate.c` (L109, L550,
// L690, L696, L705, L1079, L1145) and at zero places in `infback.c`. Every one
// is unreachable when `state->wrap == 0`, and a wrapped stream that fails inside
// its header reaches none of them either. `ZStream::adler` is the engine-side
// mirror of that field, so the same contract must hold here (AAP §0.8.1 D-4,
// standard S5).
// ===========================================================================

/// A raw (`windowBits < 0`) stream must never have `adler` written: C reaches the
/// field only through `inflateResetKeep`'s `if (state->wrap)` guard
/// (`inflate.c` L108-L109) and its `wrap & 4` guarded folds (L1079, L1145).
#[test]
fn raw_inflate_never_writes_the_adler_mirror() {
    const POISON: u32 = 0xAAAA_5555;

    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, -15)), ReturnCode::Ok);
    strm.adler = POISON;

    // A reset must not disturb it either (all reset entry points funnel into C's
    // wrap-guarded write).
    assert_eq!(rc(inflate_reset(&mut strm)), ReturnCode::Ok);
    assert_eq!(
        strm.adler, POISON,
        "inflate_reset wrote adler on a raw stream"
    );
    assert_eq!(rc(inflate_reset_keep(&mut strm)), ReturnCode::Ok);
    assert_eq!(
        strm.adler, POISON,
        "inflate_reset_keep wrote adler on a raw stream"
    );

    // A full raw decode must not disturb it: `wrap & 4` is clear.
    let payload: Vec<u8> = (0..4_096u32).map(|i| (i % 251) as u8).collect();
    let raw = compress_at(&payload, -15);
    let mut out = vec![0u8; payload.len() + 64];
    let outcome = inflate(&mut strm, &raw, &mut out, Z_FINISH);
    assert_eq!(outcome.code, ReturnCode::StreamEnd);
    assert_eq!(&out[..outcome.produced], &payload[..]);
    assert_eq!(strm.adler, POISON, "a raw decode wrote the adler mirror");
    // An ordinary return runs C's full epilogue, so the running byte totals
    // advance together with the reported progress (`inflate.c` L1139-L1142).
    assert_eq!(
        strm.total_in, outcome.consumed as u64,
        "an ordinary return must commit total_in"
    );
    assert_eq!(
        strm.total_out, outcome.produced as u64,
        "an ordinary return must commit total_out"
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

/// A **wrapped** stream that fails inside its header reaches `BAD` before C's
/// L550/L690 writes, so the mirror must be left alone there too. This is the
/// case a `wrap != 0` check alone would miss.
#[test]
fn a_wrapped_header_error_does_not_write_the_adler_mirror() {
    const POISON: u32 = 0xAAAA_5555;
    // CM=8, CINFO=8 => a 16-bit window (above MAX_WBITS); FCHECK is valid so the
    // decoder rejects the window size specifically.
    const BAD_WINDOW: [u8; 2] = [0x88, 0x1C];

    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
    // A wrapped init *does* write it (C L108-L109: `wrap & 1`).
    assert_eq!(strm.adler, 1, "a wrapped init must seed adler to wrap & 1");
    strm.adler = POISON;

    let mut out = vec![0u8; 64];
    let outcome = inflate(&mut strm, &BAD_WINDOW, &mut out, Z_NO_FLUSH);
    assert_eq!(outcome.code, ReturnCode::DataError);
    assert_eq!(strm.msg, Some("invalid window size"));
    assert_eq!(
        strm.adler, POISON,
        "a header error wrote the adler mirror; C reaches none of its seven \
         assignments on this path"
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

/// C's `case DICT` with no dictionary runs `RESTORE(); return Z_NEED_DICT;`
/// (`inflate.c` L701-L703), committing the cursors but jumping over the
/// `total_in`/`total_out` updates at L1141-L1142. The DICTID is still published
/// into `adler` (C L696) so the caller can pick the right dictionary.
///
/// The quirk is observable in exactly one place — the C `z_stream`'s running
/// totals — so it is asserted where a C caller sees it: through the `extern "C"`
/// [`ffi_inflate`], on a stream initialized by [`inflateInit2_`]. The idiomatic
/// half of the test pins the same contract on [`ZStream`]'s own counters, which
/// the early return likewise bypasses.
#[test]
fn need_dict_leaves_the_running_byte_totals_behind() {
    // zlib header with FDICT set, then the 4-byte big-endian dictionary id.
    const FDICT_HEADER: [u8; 6] = [0x78, 0x3F, 0xDE, 0xAD, 0xBE, 0xEF];

    // --- idiomatic API: ZStream's own totals must not advance ----------------
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
    let mut out = vec![0u8; 64];
    let outcome = inflate(&mut strm, &FDICT_HEADER, &mut out, Z_NO_FLUSH);

    assert_eq!(outcome.code, ReturnCode::NeedDict);
    assert_eq!(
        outcome.consumed,
        FDICT_HEADER.len(),
        "C's RESTORE() commits the input cursor before returning Z_NEED_DICT"
    );
    assert_eq!(
        strm.total_in, 0,
        "C returns before its total_in bookkeeping on the Z_NEED_DICT path"
    );
    assert_eq!(
        strm.total_out, 0,
        "C returns before its total_out bookkeeping on the Z_NEED_DICT path"
    );
    assert_eq!(
        strm.adler, 0xDEAD_BEEF,
        "the requested dictionary id must be published into adler (C L696)"
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);

    // --- C ABI: the z_stream mirror a C caller actually inspects -------------
    let mut cstrm = zeroed_stream();
    // SAFETY: `cstrm` is a valid caller-owned `z_stream`; `c"1"` is a valid
    // version string whose first byte matches the library version, and the
    // reported size is the true `sizeof(z_stream)`.
    let init = unsafe {
        inflateInit2_(
            &mut cstrm,
            15,
            c"1".as_ptr(),
            core::mem::size_of::<z_stream>() as c_int,
        )
    };
    assert_eq!(
        init,
        ReturnCode::Ok.as_c_int(),
        "inflateInit2_ must succeed"
    );

    let mut cout = [0u8; 64];
    cstrm.next_in = FDICT_HEADER.as_ptr();
    cstrm.avail_in = FDICT_HEADER.len() as c_uint;
    cstrm.next_out = cout.as_mut_ptr();
    cstrm.avail_out = cout.len() as c_uint;

    // SAFETY: `cstrm` holds a valid inflate state and its cursors describe the
    // live local buffers with matching `avail_*` counts.
    let ret = unsafe { ffi_inflate(&mut cstrm, Z_NO_FLUSH) };
    assert_eq!(
        ret,
        ReturnCode::NeedDict.as_c_int(),
        "an FDICT header with no dictionary supplied must return Z_NEED_DICT"
    );
    assert_eq!(
        cstrm.avail_in, 0,
        "RESTORE() commits avail_in before the direct return (inflate.c L701-L703)"
    );
    assert_eq!(
        cstrm.next_in,
        // SAFETY: the header was fully consumed, so one-past-the-end of the
        // fixture is the correct in-bounds-or-end cursor value.
        unsafe { FDICT_HEADER.as_ptr().add(FDICT_HEADER.len()) },
        "RESTORE() commits next_in before the direct return"
    );
    assert_eq!(
        cstrm.total_in, 0,
        "C jumps over `strm->total_in += in` (inflate.c L1141) on this path"
    );
    assert_eq!(
        cstrm.total_out, 0,
        "C jumps over `strm->total_out += out` (inflate.c L1142) on this path"
    );
    assert_eq!(
        cstrm.adler, 0xDEAD_BEEF,
        "the requested dictionary id must reach the C caller's adler field"
    );

    // SAFETY: `cstrm` was initialized by `inflateInit2_` and still owns its state.
    assert_eq!(
        unsafe { inflateEnd(&mut cstrm) },
        ReturnCode::Ok.as_c_int(),
        "inflateEnd must succeed"
    );
}

/// The second of C's two `RESTORE()`-then-return-directly paths: the `inf_leave`
/// `updatewindow` failure runs `RESTORE()` (`inflate.c` L1132), so the cursors
/// keep every byte just decoded, and then `state->mode = MEM; return
/// Z_MEM_ERROR;` (L1136-L1137) jumps over the total bookkeeping at L1141-L1142.
///
/// Reference C, linked into the same differential harness as this test's fixture,
/// reports `rc = Z_MEM_ERROR` with `600` bytes delivered through
/// `next_out`/`avail_out` and `584` through `next_in`/`avail_in`, while
/// `total_in` and `total_out` both stay at `0`. A port that committed the totals
/// here — or that discarded the delivered bytes — would diverge observably.
#[test]
fn window_allocation_failure_keeps_the_bytes_but_not_the_totals() {
    const STATE_SIZE: usize = core::mem::size_of::<zlib_rs::inflate::InflateState>();
    // `windowBits = -9` => a 512-byte raw window, allocated lazily by `inflate`.
    const WINDOW_9: usize = 1 << 9;

    // A payload whose second half repeats its first half, so the decode really
    // emits back-references and the engine really needs a window at inf_leave.
    let mut payload: Vec<u8> = (0..600u32).map(|i| ((i * 7 + 3) % 251) as u8).collect();
    for i in 300..600 {
        payload[i] = payload[i - 300];
    }
    let stream = compress_at(&payload, -9);

    let cap = MemCap {
        // Enough for the state reservation, one byte short of the window.
        budget: core::cell::Cell::new(STATE_SIZE + WINDOW_9 - 1),
    };
    let mut strm = zeroed_stream();
    strm.zalloc = Some(cap_alloc);
    strm.zfree = Some(cap_free);
    strm.opaque = (&cap as *const MemCap) as *mut c_void;

    // SAFETY: `strm` is a valid caller-owned `z_stream` with a live capped
    // allocator installed; `c"1"`'s first byte matches the library version and
    // the reported size is the true `sizeof(z_stream)`.
    let init = unsafe {
        inflateInit2_(
            &mut strm,
            -9,
            c"1".as_ptr(),
            core::mem::size_of::<z_stream>() as c_int,
        )
    };
    assert_eq!(
        init,
        ReturnCode::Ok.as_c_int(),
        "a budget covering the state must let inflateInit2_ succeed",
    );

    let mut out = vec![0u8; 2_048];
    strm.next_in = stream.as_ptr();
    strm.avail_in = stream.len() as c_uint;
    strm.next_out = out.as_mut_ptr();
    strm.avail_out = out.len() as c_uint;

    // SAFETY: `strm` holds a valid inflate state and its cursors describe the live
    // local buffers with matching `avail_*` counts.
    let ret = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
    assert_eq!(
        ret,
        ReturnCode::MemError.as_c_int(),
        "the refused window allocation must surface as Z_MEM_ERROR",
    );
    assert_eq!(
        out.len() - strm.avail_out as usize,
        payload.len(),
        "RESTORE() at inflate.c L1132 commits the output cursor, so every decoded \
         byte stays delivered",
    );
    assert_eq!(
        stream.len() - strm.avail_in as usize,
        stream.len(),
        "RESTORE() likewise commits the input cursor",
    );
    assert_eq!(
        &out[..payload.len()],
        &payload[..],
        "the delivered bytes must be the real decoded payload",
    );
    assert_eq!(
        strm.total_in, 0,
        "C jumps over `strm->total_in += in` (inflate.c L1141) on this path",
    );
    assert_eq!(
        strm.total_out, 0,
        "C jumps over `strm->total_out += out` (inflate.c L1142) on this path",
    );

    // SAFETY: `strm` was initialized by `inflateInit2_` and still owns its state.
    assert_eq!(
        unsafe { inflateEnd(&mut strm) },
        ReturnCode::Ok.as_c_int(),
        "inflateEnd must succeed",
    );
}

/// The public [`InflateOutcome`] must stay exactly `{code, consumed, produced}`.
///
/// It is a root-visible public type that mirrors the deflate engine's
/// `DeflateOutcome` field-for-field, so any added field is a breaking change for
/// downstream exhaustive struct literals and destructuring patterns. This test is
/// compiled as a *separate crate* against the public API, so it fails to compile
/// — loudly, at the exact spot — if a field is ever added or renamed. Anything
/// the FFI boundary needs beyond these three values must travel in a
/// crate-private wrapper instead (the `commit_totals` flag does).
#[test]
fn inflate_outcome_keeps_its_three_field_public_shape() {
    // Exhaustive struct literal: a fourth public field breaks this line.
    let outcome = InflateOutcome {
        code: ReturnCode::StreamEnd,
        consumed: 7,
        produced: 11,
    };
    // Exhaustive destructuring (no `..` rest pattern): the same guarantee from
    // the read side, which is how downstream code most often observes the type.
    let InflateOutcome {
        code,
        consumed,
        produced,
    } = outcome;
    assert_eq!(code, ReturnCode::StreamEnd);
    assert_eq!(consumed, 7);
    assert_eq!(produced, 11);

    // The engine must return exactly this type, so a real call is assignable to
    // an exhaustively-built value with no conversion.
    let payload: Vec<u8> = (0..1_024u32).map(|i| (i % 251) as u8).collect();
    let stream = compress_at(&payload, 15);
    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
    let mut out = vec![0u8; payload.len() + 64];
    let real: InflateOutcome = inflate(&mut strm, &stream, &mut out, Z_FINISH);
    assert_eq!(
        real,
        InflateOutcome {
            code: ReturnCode::StreamEnd,
            consumed: stream.len(),
            produced: payload.len(),
        },
        "a full decode must be describable by an exhaustive three-field literal"
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

/// A successful zlib decode must still publish the payload's Adler-32 through
/// C's `wrap & 4` guarded fold (`inflate.c` L1145).
#[test]
fn successful_zlib_decode_publishes_the_payload_adler32() {
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 241) as u8).collect();
    let stream = compress_at(&payload, 15);

    let mut strm = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut strm, 15)), ReturnCode::Ok);
    let mut out = vec![0u8; payload.len() + 64];
    let outcome = inflate(&mut strm, &stream, &mut out, Z_FINISH);
    assert_eq!(outcome.code, ReturnCode::StreamEnd);
    assert_eq!(&out[..outcome.produced], &payload[..]);
    assert_eq!(
        strm.adler,
        adler32(1, &payload),
        "the published adler must be the payload's Adler-32"
    );
    assert_eq!(rc(inflate_end(&mut strm)), ReturnCode::Ok);
}

/// The gzip metadata parsed by `inflate` must be retrievable by an **external**
/// safe-Rust consumer, using nothing but the public API.
///
/// This is the read-side counterpart of `deflate_set_header`. C's
/// `inflateGetHeader` (`inflate.c` L1219-L1230) records a *borrowed*
/// `gz_headerp`, so a C caller simply reads its own struct afterwards; this port
/// takes ownership of the [`GzHeader`] instead, which is what removes the
/// dangling-pointer hazard (AAP §0.6.3) but also means a retrieval route must
/// exist or the parsed fields are unreachable. `inflate_header` (borrow) and
/// `inflate_take_header` (transfer ownership) are that route.
///
/// Because this file is compiled as a separate crate, everything asserted here
/// is reachable by a real downstream user — a `pub(crate)` accessor would not
/// compile.
#[cfg(feature = "gzip")]
#[test]
fn gzip_header_metadata_is_retrievable_by_a_safe_rust_consumer() {
    use zlib_rs::deflate::deflate_set_header;
    use zlib_rs::inflate::{inflate_header, inflate_take_header};

    let payload: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();

    // --- write side: emit a gzip member carrying every optional field --------
    let member = {
        let mut enc = ZStream::new();
        assert_eq!(
            rc(deflate_init2(
                &mut enc,
                6,
                Z_DEFLATED,
                16 + 15,
                DEF_MEM_LEVEL,
                Strategy::Default,
            )),
            ReturnCode::Ok,
        );
        let mut head = GzHeader::new()
            .with_text(true)
            .with_time(0x1234_5678)
            .with_os(3)
            .with_extra(vec![1u8, 2, 3, 4, 5])
            .with_name(b"metadata.bin")
            .with_comment(b"written by the interop test");
        head.hcrc = true; // emit and verify the optional CRC-16
        assert_eq!(rc(deflate_set_header(&mut enc, Some(head))), ReturnCode::Ok);
        let mut out = vec![0u8; payload.len() + payload.len() / 2 + 1_024];
        let outcome = deflate(&mut enc, &payload, &mut out, Z_FINISH);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        out.truncate(outcome.produced);
        assert_eq!(rc(deflate_end(&mut enc)), ReturnCode::Ok);
        out
    };
    assert_eq!(&member[..2], &[0x1f, 0x8b], "a gzip member was produced");

    // --- read side: register capture buffers, decode, then retrieve ----------
    let mut want = GzHeader::new();
    want.extra = Some(Vec::new());
    want.extra_max = 64;
    want.name = Some(Vec::new());
    want.name_max = 64;
    want.comment = Some(Vec::new());
    want.comm_max = 64;

    let mut dec = ZStream::new();
    assert_eq!(rc(inflate_init2(&mut dec, 16 + 15)), ReturnCode::Ok);
    assert_eq!(rc(inflate_get_header(&mut dec, want)), ReturnCode::Ok);

    // Reachable immediately after registration, with `done` cleared.
    assert!(
        !inflate_header(&dec)
            .expect("the registration is visible before decoding")
            .done,
        "inflate_get_header clears `done`",
    );

    let mut out = vec![0u8; payload.len() + 64];
    let outcome = inflate(&mut dec, &member, &mut out, Z_FINISH);
    assert_eq!(
        outcome.code,
        ReturnCode::StreamEnd,
        "decode failed: {:?}",
        dec.msg
    );
    assert_eq!(
        &out[..outcome.produced],
        &payload[..],
        "payload round-trips"
    );

    // Every field is now readable through the borrowing accessor.
    let head = inflate_header(&dec).expect("the header is reachable after decoding");
    assert!(head.done, "`done` marks the whole header consumed");
    assert!(head.text, "the TEXT flag survives the round trip");
    assert_eq!(head.time, 0x1234_5678, "MTIME survives the round trip");
    assert_eq!(head.os, 3, "the OS byte survives the round trip");
    assert_eq!(head.extra.as_deref(), Some(&[1u8, 2, 3, 4, 5][..]));
    // The declared 16-bit `XLEN` is deliberately absent from `GzHeader`: it is
    // wire-level decoder metadata published only into a C caller's `gz_header`.
    // An idiomatic caller reads the number of bytes actually captured, which is
    // the whole field here because `extra_max` exceeded `XLEN`.
    assert_eq!(
        head.extra.as_ref().map_or(0, Vec::len),
        5,
        "the whole declared extra field was captured"
    );
    assert_eq!(head.name.as_deref(), Some(&b"metadata.bin"[..]));
    assert_eq!(
        head.comment.as_deref(),
        Some(&b"written by the interop test"[..])
    );
    assert!(
        head.hcrc,
        "FHCRC was set, so the CRC-16 was present and checked"
    );

    // ...and can be taken out to outlive the stream. Ownership transfer clears
    // the registration, mirroring C's `state->head = Z_NULL`.
    let owned = inflate_take_header(&mut dec).expect("ownership is returned");
    assert!(
        inflate_header(&dec).is_none(),
        "the registration is cleared"
    );
    assert!(
        inflate_take_header(&mut dec).is_none(),
        "a second take yields nothing"
    );
    assert_eq!(rc(inflate_end(&mut dec)), ReturnCode::Ok);
    drop(dec);
    assert_eq!(
        owned.name.as_deref(),
        Some(&b"metadata.bin"[..]),
        "the retrieved metadata outlives the stream it came from"
    );
}
