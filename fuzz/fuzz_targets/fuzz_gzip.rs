#![no_main]
//! Gzip / zlib auto-detect inflate harness (safe streaming API).
//!
//! Drives arbitrary bytes through the crate's **safe** streaming inflate engine
//! ([`zlib_rs::inflate`]) with `windowBits = 47` (32 + 15 = auto-detect gzip
//! *or* zlib framing). This exercises the gzip header parser (magic, flags,
//! EXTRA/NAME/COMMENT/HCRC fields) and the RFC 1952 trailer on untrusted input.
//! The decoder must reject malformed streams with an error code and must never
//! panic, read out of bounds, or loop unbounded.
//!
//! # Why this target never reaches the crate's C ABI boundary
//!
//! The crate's `ffi` module is its C ABI boundary and the sole home of every
//! compiler-escape construct it contains; those symbols are deliberately not
//! re-exported at the crate root so that boundary stands alone. Reaching it from
//! here would drag a C-ABI dependency — and the escapes that come with it — into
//! a harness that has no business on that side of the boundary. So this target
//! drives the same decoder through the ordinary checked Rust surface that real
//! consumers use, which is also what makes its findings trustworthy: nothing it
//! reports can be an artefact of the harness reaching around the type system.
//! Exercising the C ABI is `fuzz_ffi_roundtrip`'s job, not this one's. (User
//! constraint 3 — no compiler-escape blocks in core compression logic — and AAP
//! §0.7.2 plan-adopted standard S2, containment by construction.)
//!
//! # Why the gzip paths are gated at RUN TIME, not by a compile-time predicate
//!
//! gzip (`24..=31`) and auto-detect (`40..=47`) `windowBits` are only accepted
//! when `zlib-rs` is built with its `gzip` feature: `inflate_reset2` masks the
//! request with `& 15` only when that feature is on, and without it the value
//! fails the `8..=15` bounds test — matching C built without `GUNZIP`.
//!
//! A `#[cfg]` feature predicate **cannot** express that condition here, and this
//! was measured rather than assumed. `fuzz/` is a separate, detached package
//! (`zlib-rs-fuzz`) that merely *depends on* `zlib-rs`, and Cargo defines a
//! `feature` predicate only for the crate it is compiling. Since `zlib-rs-fuzz`
//! declares no features of its own, a `gzip` predicate is never defined here:
//! `cargo build -v` passes zero feature flags to these binaries, so such an
//! attribute would always be **false** and would silently compile the gzip
//! coverage out of the gzip harness. Worse, `rustc`'s `unexpected_cfgs` lint
//! rejects the unknown value outright ("no expected values for `feature`"),
//! which the folder's blocking `clippy --all-targets -- -D warnings` gate turns
//! into a compile error. None of the five targets in this directory carries such
//! a predicate, for exactly this reason.
//!
//! So the gate lives where the information actually exists: [`drive`] reports
//! `None` when `inflate_init2` rejects a framing, which is precisely the
//! "gzip compiled out" signal, and the harness then falls back to the always
//! available zlib and raw framings. That keeps the mandatory libFuzzer entry
//! point — and therefore `main` — unconditional, so this `[[bin]]` links under
//! every feature combination, while the gzip coverage survives in a default
//! build. For the same reason the optional `inflate_get_header` probe is
//! omitted: that function *is* feature-gated inside the library, so merely
//! naming it would break a reduced-feature build.
//!
//! # Coverage
//!
//! Every RFC 1952 phase is reached from fuzzer-chosen bytes: the fixed header
//! (magic, CM, FLG, MTIME, XFL, OS), the optional EXTRA / NAME / COMMENT fields,
//! the optional header CRC, and the little-endian CRC-32 + ISIZE trailer —
//! plus truncated and corrupted variants of each, and a deterministic
//! well-formed member that pins the accept path.

use libfuzzer_sys::fuzz_target;
use zlib_rs::constants::Z_NO_FLUSH;
use zlib_rs::inflate::{inflate, inflate_end, inflate_init2};
use zlib_rs::{ReturnCode, ZStream, crc32};

// ===========================================================================
// windowBits framing selectors and work bounds
//
// These are ABI / wire-format constants: `constants::parse_window_bits` decodes
// the overloaded `windowBits` value, and altering any of them would change what
// this harness actually tests (AAP §0.8.1 directive D-2 — observe, never
// "correct").
// ===========================================================================

/// Auto-detect a zlib *or* gzip wrapper: windowBits 47 = 32 (auto-detect
/// header) + 15 (max window). This is the framing the harness has always used
/// and remains its primary case.
const WBITS_AUTO: i32 = 47;

/// gzip wrapper only (RFC 1952): `16 + 15`. Reaching the header parser without
/// the auto-detect probe in front of it is a distinct path through
/// `inflate_reset2`.
const WBITS_GZIP: i32 = 31;

/// zlib wrapper only (RFC 1950). Always available — never gated on `gzip`.
const WBITS_ZLIB: i32 = 15;

/// Raw DEFLATE (RFC 1951): no wrapper, no checksum. Always available.
const WBITS_RAW: i32 = -15;

/// The fixed output window, re-presented to the decoder on every pass. A fixed
/// ceiling (rather than a buffer sized from the input) is what stops a
/// decompression-bomb input from dominating the run, and it means no allocation
/// is ever sized from a fuzzer-supplied length.
const OUT_WINDOW: usize = 4096;

/// Maximum number of drain-loop passes per stream. See [`drive`].
const WORK_BUDGET: u32 = 4096;

// --- RFC 1952 §2.3.1 member layout -----------------------------------------

/// gzip magic byte 1 (`ID1`), fixed at 31 (`0x1f`).
const GZIP_ID1: u8 = 0x1f;
/// gzip magic byte 2 (`ID2`), fixed at 139 (`0x8b`).
const GZIP_ID2: u8 = 0x8b;
/// `CM` (compression method) 8 denotes DEFLATE — the only method gzip defines.
const CM_DEFLATE: u8 = 8;
/// `FLG` bit 1: a CRC-16 of the header follows the optional fields.
const FHCRC: u8 = 0x02;
/// `FLG` bit 2: an `XLEN`-prefixed extra field is present.
const FEXTRA: u8 = 0x04;
/// `FLG` bit 3: a NUL-terminated original file name is present.
const FNAME: u8 = 0x08;
/// `FLG` bit 4: a NUL-terminated file comment is present.
const FCOMMENT: u8 = 0x10;
/// The five `FLG` bits RFC 1952 defines (`FTEXT | FHCRC | FEXTRA | FNAME |
/// FCOMMENT`). Bits 5-7 are reserved and a conforming decoder must reject a
/// header that sets them; that rejection is covered by the unmodified-input
/// cases, so the synthesized headers mask down to the defined bits in order to
/// reach the *deeper* phases instead of being turned away at byte 3.
const FLG_DEFINED: u8 = 0x1f;

/// The canonical two-byte empty DEFLATE stream: `BFINAL = 1`, `BTYPE = 01`
/// (fixed Huffman), followed by the 7-zero-bit end-of-block symbol.
const EMPTY_DEFLATE: [u8; 2] = [0x03, 0x00];

/// Upper bound on a synthesized EXTRA / NAME / COMMENT field, in bytes.
///
/// Every field length is drawn from a *single* fuzz byte, so it is inherently
/// bounded by this value: steerable by the fuzzer, yet incapable of asking for
/// more memory than the (libFuzzer-capped) input already occupies. Nothing here
/// is ever sized from an unbounded fuzzer-supplied number.
const MAX_FIELD: usize = u8::MAX as usize;

// ===========================================================================
// Harness plumbing
// ===========================================================================

/// How the drain loop decides it is finished.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Drain {
    /// Stop as soon as the input has been fully consumed — the harness's
    /// original, preserved shape (reference zlib's `strm.avail_in == 0` test).
    /// This bounds the work a decompression bomb can demand, because output is
    /// only ever pulled while input remains.
    UntilInputDrained,
    /// Keep pumping until a terminal code or a stall. Used only for the
    /// deterministic well-formed member, which must be driven all the way to
    /// `Z_STREAM_END` for its accept-path assertion to mean anything.
    UntilTerminal,
}

/// What a completed [`drive`] observed.
struct Decoded {
    /// The terminal return code of the drain loop.
    code: ReturnCode,
    /// Total bytes written across every pass.
    produced: usize,
}

/// A non-panicking, non-over-reading cursor over the fuzzer's bytes.
///
/// Reads past the end yield zeros / empty slices rather than panicking, so the
/// synthesizers below can ask for whatever fields the chosen `FLG` implies
/// without first having to check that the input is long enough. Naturally short
/// reads are a feature, not a defect: they are exactly how the fuzzer reaches
/// the truncated-field paths in the gzip header parser.
struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    /// Wraps `data`, positioned at its first byte.
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Returns the next byte, or `0` once the input is exhausted.
    fn byte(&mut self) -> u8 {
        match self.data.get(self.pos) {
            Some(&b) => {
                self.pos = self.pos.saturating_add(1);
                b
            }
            None => 0,
        }
    }

    /// Returns up to `n` bytes, advancing past whatever was actually available.
    fn take(&mut self, n: usize) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        let end = start.saturating_add(n).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }

    /// Returns everything not yet read and leaves the cursor exhausted.
    fn rest(&mut self) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        self.pos = self.data.len();
        &self.data[start..]
    }
}

/// Asserts that `code` is one of the nine return codes zlib defines.
///
/// This is deliberately *not* an assertion about which code a given input
/// deserves: for arbitrary bytes a clean rejection is the correct answer, so
/// pinning a specific code would manufacture false crashes. What it does pin is
/// the shape of the answer, on two independent axes:
///
/// * the exhaustive `match` fails at **compile time** if the library ever grows
///   a tenth variant, forcing this harness to be taught about it; and
/// * the round-trip through the ABI integer fails at **run time** if a
///   discriminant ever stops agreeing with the `zlib.h` value it mirrors.
fn assert_legal_return_code(code: ReturnCode) {
    let expected = match code {
        ReturnCode::Ok => 0,
        ReturnCode::StreamEnd => 1,
        ReturnCode::NeedDict => 2,
        ReturnCode::ErrNo => -1,
        ReturnCode::StreamError => -2,
        ReturnCode::DataError => -3,
        ReturnCode::MemError => -4,
        ReturnCode::BufError => -5,
        ReturnCode::VersionError => -6,
    };
    assert_eq!(
        code.as_c_int(),
        expected,
        "ReturnCode::{code:?} no longer carries its zlib.h ABI value"
    );
    assert_eq!(
        ReturnCode::from_c_int(expected),
        Some(code),
        "ReturnCode round-trip through the ABI integer {expected} diverged"
    );
}

/// Decodes `stream` with the framing selected by `window_bits`.
///
/// Returns `None` when `inflate_init2` rejects the framing outright. That is the
/// harness's runtime feature gate: a `zlib-rs` built without `gzip` refuses the
/// gzip and auto-detect `windowBits` ranges, and the caller then falls back to
/// the always-available zlib / raw framings (see the module docs). It is also
/// the original harness's "return early when initialization fails" behaviour,
/// preserved.
///
/// Every exit path — clean end, error, stall, and exhausted work budget alike —
/// tears the stream down with an explicit `inflate_end`, mirroring the C
/// contract even though owning the state would release it anyway.
fn drive(stream: &[u8], window_bits: i32, drain: Drain) -> Option<Decoded> {
    let mut strm = ZStream::new();
    if inflate_init2(&mut strm, window_bits).is_err() {
        return None;
    }

    let mut out = [0u8; OUT_WINDOW];
    let mut in_pos = 0usize;
    let mut produced = 0usize;

    // Drain the decoder, refilling the fixed output window each pass. `guard`
    // caps total work so a decompression-bomb input cannot dominate the run
    // (libFuzzer's own timeout would otherwise flag it as noise).
    let mut guard: u32 = 0;
    let code = loop {
        let outcome = inflate(&mut strm, &stream[in_pos..], &mut out, Z_NO_FLUSH);
        assert_legal_return_code(outcome.code);

        // Progress can never exceed the buffers that were handed over. These are
        // library invariants rather than input-dependent expectations, so a
        // violation is a genuine finding — and asserting them keeps `in_pos`
        // provably within `stream`, so the re-slice above cannot go out of range.
        assert!(
            outcome.consumed <= stream.len().saturating_sub(in_pos),
            "inflate reported consuming more input than was supplied"
        );
        assert!(
            outcome.produced <= OUT_WINDOW,
            "inflate reported producing more output than the window allows"
        );

        in_pos = in_pos.saturating_add(outcome.consumed);
        produced = produced.saturating_add(outcome.produced);

        // Stop on Z_STREAM_END (1) or any error (< 0); only Z_OK (0) continues.
        // Under `UntilInputDrained` also stop once all input is consumed, which
        // is the original harness's `avail_in == 0` test.
        if outcome.code != ReturnCode::Ok
            || (drain == Drain::UntilInputDrained && in_pos >= stream.len())
        {
            break outcome.code;
        }

        // A pass that neither consumed nor produced anything cannot be followed
        // by a pass that does: the decoder is stalled (typically a truncated
        // stream). Breaking here is strictly better than leaning on libFuzzer's
        // timeout, because an unbounded harness loop is a harness bug, not a
        // library finding.
        if outcome.consumed == 0 && outcome.produced == 0 {
            break outcome.code;
        }

        guard = guard.saturating_add(1);
        if guard > WORK_BUDGET {
            break outcome.code;
        }
    };

    assert!(
        inflate_end(&mut strm).is_ok(),
        "inflate_end must succeed after a successful inflate_init2"
    );

    Some(Decoded { code, produced })
}

/// Drives `stream` under `window_bits` purely for coverage, discarding the
/// outcome.
///
/// The outcome is deliberately not asserted on. Every stream reaching this
/// function is arbitrary fuzzer input, for which a clean rejection — a
/// `DataError`, a `BufError`, or a stall — is the *correct* result, so demanding
/// any particular code would turn every corrupt input into a false crash and
/// make the target worthless. The invariants that *do* hold regardless of input
/// are still enforced inside [`drive`] on every pass: the code is one of the
/// nine zlib defines, progress never exceeds the buffers handed over, and the
/// explicit teardown succeeds. The one place a specific outcome is required is
/// the well-formed member built by [`valid_member`], whose validity the harness
/// controls end to end.
fn sweep(stream: &[u8], window_bits: i32) {
    let _ = drive(stream, window_bits, Drain::UntilInputDrained);
}

// ===========================================================================
// RFC 1952 member synthesis
// ===========================================================================

/// Appends the RFC 1952 trailer: the little-endian CRC-32 of the uncompressed
/// data followed by the little-endian ISIZE (its length mod 2^32).
fn push_trailer(member: &mut Vec<u8>, crc: u32, isize_mod32: u32) {
    member.extend_from_slice(&crc.to_le_bytes());
    member.extend_from_slice(&isize_mod32.to_le_bytes());
}

/// Appends a NUL-terminated header string built from up to [`MAX_FIELD`] fuzz
/// bytes, stripping embedded NULs so the terminator position stays under the
/// fuzzer's control via the length byte rather than the content.
fn push_cstr_field(member: &mut Vec<u8>, src: &mut Bytes<'_>) {
    let len = usize::from(src.byte());
    for &b in src.take(len) {
        if b != 0 {
            member.push(b);
        }
    }
    member.push(0);
}

/// Builds a gzip member whose header is *plausible* but whose every optional
/// field, payload, and trailer byte comes from the fuzzer.
///
/// Only the magic and `CM` are forced valid. Handing the decoder raw fuzz bytes
/// gets it rejected at byte 0 almost every time, which never reaches the
/// interesting code; pinning just those three bytes lets the fuzzer steer
/// straight into the EXTRA / NAME / COMMENT / HCRC phases and the trailer checks
/// while still choosing everything they contain. The unmodified input is driven
/// separately, so genuinely arbitrary streams stay covered too.
///
/// The `OS` byte is taken from the fuzzer like any other field. It is never
/// compared against a platform value: the library picks its own OS code at
/// compile time (one value on Windows, another on non-Windows Apple, a third
/// elsewhere), so asserting on it would bake a host assumption into the harness.
/// Proving portability is the CI matrix's job, not this file's — hence no
/// platform or endianness branch appears anywhere here.
fn synthesize_member(src: &mut Bytes<'_>) -> Vec<u8> {
    let control = src.byte();
    let flg = control & FLG_DEFINED;

    // Bounded by the input length plus a fixed header/trailer allowance, so the
    // capacity can never be dictated by an unbounded fuzzer-supplied number.
    let mut member = Vec::with_capacity(src.data.len().saturating_add(64));
    member.extend_from_slice(&[GZIP_ID1, GZIP_ID2, CM_DEFLATE, flg]);
    member.extend_from_slice(src.take(4)); // MTIME, little-endian.
    member.push(src.byte()); // XFL
    member.push(src.byte()); // OS

    if flg & FEXTRA != 0 {
        let want = usize::from(src.byte());
        let extra = src.take(want);
        // XLEN is declared from what was *requested* while only what was
        // *available* is appended, so a short read yields a deliberately
        // truncated EXTRA field — one of the phases most worth probing.
        let declared = u16::try_from(want).unwrap_or(u16::MAX);
        member.extend_from_slice(&declared.to_le_bytes());
        member.extend_from_slice(extra);
    }
    if flg & FNAME != 0 {
        push_cstr_field(&mut member, src);
    }
    if flg & FCOMMENT != 0 {
        push_cstr_field(&mut member, src);
    }
    if flg & FHCRC != 0 {
        // RFC 1952: the low 16 bits of the CRC-32 over the header bytes so far.
        // The top control bit decides whether to emit that value (exercising the
        // accept path) or a corrupted one (exercising the mismatch rejection).
        let crc16 = (crc32(0, &member) & 0xffff) as u16;
        let emitted = if control & 0x80 != 0 {
            crc16 ^ 0xffff
        } else {
            crc16
        };
        member.extend_from_slice(&emitted.to_le_bytes());
    }

    // Everything still unread becomes the DEFLATE payload, then a fuzzer-chosen
    // trailer so the CRC-32 and ISIZE verification paths are reachable.
    let trailer = src.take(8);
    member.extend_from_slice(src.rest());
    member.extend_from_slice(trailer);
    member
}

/// Builds a **well-formed** gzip member carrying an empty payload, with the
/// optional fields the fuzzer asks for and a correct header CRC when requested.
///
/// This is the accept-path anchor. Because every byte is under the harness's
/// control it cannot be rejected for a legitimate reason, so a decoder that
/// fails to reach `Z_STREAM_END` here has genuinely regressed — which makes it
/// the counterweight to all the negative cases above, and the reason the
/// EXTRA / NAME / COMMENT / HCRC phases are exercised on their success path and
/// not only on their rejection paths.
fn valid_member(src: &mut Bytes<'_>) -> Vec<u8> {
    let flg = src.byte() & FLG_DEFINED;

    let mut member = Vec::with_capacity(4 * MAX_FIELD);
    member.extend_from_slice(&[GZIP_ID1, GZIP_ID2, CM_DEFLATE, flg]);
    member.extend_from_slice(&0u32.to_le_bytes()); // MTIME: "no timestamp".
    member.push(0); // XFL
    member.push(src.byte()); // OS — fuzzer-chosen, never a platform constant.

    if flg & FEXTRA != 0 {
        let want = usize::from(src.byte());
        let extra = src.take(want);
        // Here XLEN must match the bytes actually present: this member is meant
        // to be accepted, so the field is self-consistent by construction even
        // when the input ran short.
        let declared = u16::try_from(extra.len()).unwrap_or(u16::MAX);
        member.extend_from_slice(&declared.to_le_bytes());
        member.extend_from_slice(extra);
    }
    if flg & FNAME != 0 {
        push_cstr_field(&mut member, src);
    }
    if flg & FCOMMENT != 0 {
        push_cstr_field(&mut member, src);
    }
    if flg & FHCRC != 0 {
        let crc16 = (crc32(0, &member) & 0xffff) as u16;
        member.extend_from_slice(&crc16.to_le_bytes());
    }

    member.extend_from_slice(&EMPTY_DEFLATE);
    // An empty payload's CRC-32 and ISIZE are both 0; the checksum is computed
    // rather than written as a literal so the trailer stays honest if the
    // payload above is ever changed.
    push_trailer(&mut member, crc32(0, b""), 0);
    member
}

// ===========================================================================
// Entry point
//
// The libFuzzer entry-point macro below — and therefore `main` — is
// UNCONDITIONAL. A whole-file inner `#![cfg(...)]` would compile the macro
// invocation away and leave this `[[bin]]` with no entry point, breaking the
// link and dropping the target from the `cargo fuzz list` sweep that drives CI.
// A test binary survives that treatment because one with zero tests still links;
// a libFuzzer binary does not. The only inner attribute in this file is
// therefore the mandatory `#![no_main]` on line 1.
// ===========================================================================

fuzz_target!(|data: &[u8]| {
    // 1. The unmodified input under auto-detect framing — the harness's original
    //    case, and the one that covers genuinely arbitrary streams, including
    //    reserved-FLG-bit and bad-CM rejections the synthesizers mask away.
    //    `None` means this build has gzip compiled out, in which case the zlib
    //    and raw sweeps below carry the coverage on their own.
    let gzip_available = drive(data, WBITS_AUTO, Drain::UntilInputDrained).is_some();

    // 2. Sweep the framings that never depend on the `gzip` feature. Feeding the
    //    same bytes through zlib and raw framing is what exercises the
    //    auto-detect discrimination against input that is and is not gzip.
    sweep(data, WBITS_ZLIB);
    sweep(data, WBITS_RAW);

    // 3. gzip-only framing: a different route through `inflate_reset2` than the
    //    auto-detect probe, and skipped automatically without the feature.
    sweep(data, WBITS_GZIP);

    if !gzip_available {
        return;
    }

    // 4. A synthesized member walks the fuzzer into the RFC 1952 header phases:
    //    EXTRA, NAME, COMMENT, the header CRC, and the CRC-32 / ISIZE trailer.
    //    Malformed and short fields are reached naturally, since a truncated
    //    input simply yields truncated fields.
    let mut src = Bytes::new(data);
    let member = synthesize_member(&mut src);
    sweep(&member, WBITS_GZIP);
    sweep(&member, WBITS_AUTO);

    // 5. An explicitly truncated copy, cut at a fuzzer-chosen offset, so every
    //    header phase gets to be interrupted part-way through. A 16-bit selector
    //    can address any offset in a member built from a 64 KiB input, and the
    //    `% (len + 1)` keeps it in range (the divisor is never zero because a
    //    synthesized member always carries at least its fixed 10-byte header).
    let cut_sel = usize::from(u16::from_le_bytes([
        data.first().copied().unwrap_or(0),
        data.get(1).copied().unwrap_or(0),
    ]));
    let cut = cut_sel % member.len().saturating_add(1);
    sweep(&member[..cut], WBITS_GZIP);

    // 6. The accept-path anchor: a well-formed member, driven to completion. It
    //    must decode cleanly to an empty payload. This is the only place a
    //    specific outcome is asserted, and it is sound precisely because the
    //    harness — not the fuzzer — decided every byte's validity.
    let mut anchor_src = Bytes::new(data);
    let good = valid_member(&mut anchor_src);
    if let Some(decoded) = drive(&good, WBITS_GZIP, Drain::UntilTerminal) {
        assert_eq!(
            decoded.code,
            ReturnCode::StreamEnd,
            "a well-formed gzip member was not accepted"
        );
        assert_eq!(
            decoded.produced, 0,
            "an empty gzip payload produced output bytes"
        );
    }
});
