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
//! [`inflate_copy_mid_decode_resumes_identically`] closes the remaining half of
//! that contract: it copies a stream *after* its dynamic Huffman tables have been
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
use zlib_rs::constants::{DEF_MEM_LEVEL, Strategy, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH, Z_TREES};
use zlib_rs::deflate::{DeflateState, deflate, deflate_end, deflate_init2};
use zlib_rs::ffi::{
    inflate as ffi_inflate, inflateBack, inflateBackEnd, inflateBackInit_, inflateCopy, inflateEnd,
    inflateInit_, inflateInit2_, z_stream,
};
use zlib_rs::inflate::back::{InFunc, OutFunc, inflate_back, inflate_back_end, inflate_back_init};
#[cfg(feature = "gzip")]
use zlib_rs::inflate::inflate_get_header;
use zlib_rs::inflate::tables::{CodeType, InflateTableError};
use zlib_rs::inflate::{
    Code, ENOUGH, ENOUGH_DISTS, ENOUGH_LENS, MAXBITS, inflate, inflate_codes_used, inflate_copy,
    inflate_end, inflate_init, inflate_init2, inflate_mark, inflate_prime, inflate_reset2,
    inflate_set_dictionary, inflate_sync, inflate_sync_point, inflate_table, inflate_undermine,
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
}

impl<'a> OneShot<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, done: false }
    }
}

impl InFunc for OneShot<'_> {
    fn next_input(&mut self) -> &[u8] {
        if self.done {
            &[]
        } else {
            self.done = true;
            self.data
        }
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
            // Reachable portion of C's NEED_DICT coverage. (C additionally forces
            // Z_MEM_ERROR under an allocation limit and then poke-restores the
            // private `state->mode = DICT`; forcing `Z_MEM_ERROR` is covered
            // separately by `mem_limit_forces_mem_error`, but the private-state
            // poke that resumes decoding from it is not expressible over the
            // public API, so only the publicly reachable steps are reproduced.)
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
        let code = inflate_back(&mut state, &mut src, &mut sink);
        // C: `assert(ret != Z_STREAM_ERROR)`.
        assert_ne!(code, ReturnCode::StreamError, "{id}: back stream error");
        if err != 0 {
            // inflateBack exposes no z_stream, hence no msg to compare (C compares
            // strm.msg only because inflateBack borrows the same z_stream; the
            // idiomatic API returns just the code).
            assert_eq!(
                code,
                ReturnCode::DataError,
                "{id}: expected DataError (back)"
            );
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
        assert_eq!(
            inflate_back(&mut state, &mut src, &mut sink),
            ReturnCode::StreamEnd,
        );
        assert_eq!(inflate_back_end(state), ReturnCode::Ok);
    }

    // Forced output error: the sink rejects, so inflateBack aborts with BufError.
    {
        let mut state = inflate_back_init(15).expect("inflate_back_init(15)");
        let mut src = OneShot::new(&[0x63, 0x00, 0x00]);
        let mut sink = SinkReject;
        assert_eq!(
            inflate_back(&mut state, &mut src, &mut sink),
            ReturnCode::BufError,
        );
        assert_eq!(inflate_back_end(state), ReturnCode::Ok);
    }

    // Forced *mode* error (Z_STREAM_ERROR): NOT reproducible here. C forces it by
    // having its `pull` callback poke `((inflate_state*)strm.state)->mode = SYNC`,
    // an otherwise-impossible internal state. The idiomatic `InFunc` trait only
    // yields input bytes and cannot touch private engine state, and forging it
    // with `unsafe` is expressly forbidden. Equivalent coverage of the bad-state
    // guard in `inflate_back` is provided as a `#[cfg(test)]` unit test in
    // `src/inflate/back.rs` (owned by the `src/inflate/` module).

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
///    allocation is routed through the caller's hook (previously it bypassed
///    the hook and wrongly succeeded).
/// 2. **Window allocation fails after init.** With a budget large enough for the
///    state but below the state + window size, `inflateInit2_` succeeds and the
///    subsequent `inflate` fails when it tries to grow the lazily-allocated
///    window — the outcome `infcover.c` pins under its tight allocation limit.
#[test]
fn mem_limit_forces_mem_error() {
    // Size of the inflate state, which reference zlib (and now zlib-rs) reserves
    // through the caller's `zalloc` at `inflateInit2_`.
    const STATE_SIZE: usize = zlib_rs::inflate::InflateState::C_LAYOUT_SIZE;
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
    const STATE_SIZE: usize = zlib_rs::inflate::InflateState::C_LAYOUT_SIZE;

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
    // sees is the `(1, sizeof(deflate_state))` pair.
    assert_eq!(
        requests.first().copied(),
        Some((1, DeflateState::C_LAYOUT_SIZE)),
        "the engine-state footprint must be the first request, as in C, and it must \
         carry C's own `sizeof(deflate_state)` rather than this Rust type's size"
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
    // the symbol buffer had been split out again.
    assert_eq!(
        requests,
        vec![
            (1, DeflateState::C_LAYOUT_SIZE),
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
    let minimum =
        DeflateState::C_LAYOUT_SIZE + 2 * w_size + 2 * w_size + 2 * hash_size + 4 * lit_bufsize;
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
    let state_size = DeflateState::C_LAYOUT_SIZE;
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
    let state_size = zlib_rs::inflate::InflateState::C_LAYOUT_SIZE;
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
