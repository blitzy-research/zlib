#![no_main]
//! Inflate robustness harness.
//!
//! Feeds arbitrary, attacker-controlled bytes to the one-call zlib-framed
//! decoder [`zlib_rs::uncompress`]. Decoding untrusted input must reject
//! malformed streams with an error code and must NEVER panic, read out of
//! bounds, or over-allocate. Any panic here is a memory-safety or robustness
//! bug in the inflate state machine.
//!
//! The decoder is the only part of this library that faces hostile input, so
//! the coverage below is deliberately wider than a single one-call probe. Every
//! probe is black-box over the crate's public, checked API: the harness holds no
//! raw pointer, reaches no C-ABI shim, and reads no private engine state.
//!
//! # What is covered
//!
//! * **The one-call zlib-framed wrapper.** [`zlib_rs::uncompress`] against a
//!   fixed 64 KiB output ceiling — the shortest path from a raw byte slice to
//!   the decoder. Its return code is now checked for membership in the nine
//!   defined `Z_*` codes instead of being discarded, so a future change that
//!   returned an out-of-contract value is caught rather than passing silently.
//! * **All four `windowBits` framings, through the streaming engine.** The
//!   one-call wrapper is zlib-framed only; raw DEFLATE (RFC 1951), gzip
//!   (RFC 1952), and the zlib-or-gzip auto-detect mode are reachable
//!   exclusively through `inflate_init2` + `inflate` + `inflate_end`. Both ends
//!   of every documented range are exercised, so the overloaded `windowBits`
//!   contract (`8..=15`, `-8..=-15`, `24..=31`, `40..=47`) is swept rather than
//!   sampled at one point. The two gzip-dependent framings are guarded by a
//!   run-time capability query, [`gzip_framing_supported`], which keeps the
//!   harness correct against a `zlib-rs` built without gzip support while still
//!   exercising them in full whenever that support is present — see that
//!   function for why a feature `cfg` would be the wrong tool here, and would
//!   silently cost coverage.
//! * **Byte-at-a-time delivery.** A one-byte-in / one-byte-out drip is the
//!   harshest schedule the mode loop can be given, and is where a bit
//!   accumulator or cursor mistake shows up. `test/example.c` drives the C
//!   library the same way, for the same reason.
//! * **`inflateSync` error recovery.** After a `Z_DATA_ERROR` the engine is
//!   asked to resynchronise on the unconsumed remainder, which must either find
//!   a full flush marker or fail with a defined code — never fault on the
//!   arbitrary tail it is handed.
//! * **The `inflate_table` builder rejection paths.** A malformed dynamic
//!   Huffman header drives the decode-table builder into its over-subscribed,
//!   incomplete-set, and `ENOUGH`-exceeded exits. Those are reached by direct
//!   call as well as through byte streams, because — as zlib's own
//!   `test/infcover.c` observes — a byte stream cannot manifest a not-enough
//!   error at all, "since zlib insures that enough is always enough".
//! * **A round-trip self-check.** A stream this harness produced itself must
//!   decode back to the exact input bytes.
//!
//! # Two rules that keep the findings honest
//!
//! **No specific return code is ever asserted for arbitrary input.** Garbage
//! bytes may legitimately yield `Z_OK`, `Z_STREAM_END`, `Z_NEED_DICT`,
//! `Z_DATA_ERROR`, `Z_BUF_ERROR`, `Z_STREAM_ERROR`, or `Z_MEM_ERROR` depending
//! on what they happen to encode, so pinning any one value would turn every run
//! red for a harness reason rather than a library one. Membership in the closed
//! nine-code set is asserted instead — which is the property that actually
//! matters, because it is what a C caller's `switch` relies on. The single
//! exception is the round-trip leg, whose input this harness constructed and
//! which therefore has exactly one correct answer.
//!
//! **Every loop is explicitly bounded and every buffer is a fixed size.** A
//! hang is as much a finding as a crash, but an unbounded harness loop is a
//! *harness* defect, so each drain carries a work budget and a forward-progress
//! test. No allocation is ever sized from a length field in the input or from a
//! value read out of a decoded header: a decompression bomb must not be able to
//! make the harness itself the thing that fails, because that would exhaust the
//! run's memory budget and waste the finding.
//!
//! # Committed seed corpus
//!
//! Eleven seeds live in `fuzz/seeds/fuzz_inflate/`, and they exist for one
//! specific reason: the round-trip self-check is the only probe here that is
//! SAMPLED rather than run on every execution, and its gate — bits `16..=19` of
//! [`selector`] — is a pure function of the first eight input bytes. An input
//! therefore either always opens the gate or never does. Gate coverage is thus a
//! property of the corpus, not of luck, and a small authored corpus can miss it
//! outright: with a one-in-sixteen hit rate per file, a set of two dozen
//! hand-written streams has a real chance of containing none, in which case
//! [`level_from`] and [`probe_round_trip`] measure 0.00 % over a `-runs=0`
//! replay even though both are perfectly reachable.
//!
//! Each seed is named for the level it selects — one per accepted value,
//! `Z_DEFAULT_COMPRESSION` and `0..=9` — and every one satisfies
//! `(selector(seed) >> 16) & 0xF == 0`. A zero-mutation replay of the directory
//! therefore enters the probe eleven times and sweeps all eleven levels, which
//! is precisely what a coverage measurement over a seed corpus needs and what a
//! blind campaign cannot promise on its first executions.
//!
//! They are not gate tokens with junk attached. Every seed is a real,
//! decodable, single-member zlib stream emitted by a genuine DEFLATE encoder,
//! and the set deliberately spans stored blocks, fixed-Huffman blocks, and
//! dynamic-Huffman blocks with distance matches — so the same bytes are also
//! productive for [`uncompress`], for all four framing sweeps, and for the
//! byte-at-a-time drip, instead of failing at the first header check.
//!
//! To regenerate or extend the set: reimplement [`selector`] and [`level_from`]
//! (an xorshift64 over at most the first eight bytes, finished with a multiply
//! by `0x2545_F491_4F6C_DD1D`), then enumerate candidate encoder outputs and
//! keep the first whose selector both opens the gate and yields the wanted
//! level. Only those eight bytes matter, so the search converges in a few
//! thousand candidates; the trailing bytes stay free for whatever content the
//! other probes should see.
//!
//! `fuzz/seeds/` is a deliberately SEPARATE tree from cargo-fuzz's working
//! corpus. `fuzz/corpus/<target>/` is writable, is rewritten by
//! `cargo fuzz cmin`, and is gitignored precisely because libFuzzer fills it
//! with generated units; committing files there would put tracked data in a
//! directory that tooling is entitled to prune. `fuzz/seeds/` is read-only
//! input that nothing writes back to, so it survives minimisation and
//! `git clean` alike. Pass it explicitly — libFuzzer treats the FIRST corpus
//! directory as the writable one and the rest as read-only inputs:
//!
//! ```text
//! cargo +nightly fuzz run fuzz_inflate fuzz/corpus/fuzz_inflate fuzz/seeds/fuzz_inflate
//! cargo +nightly fuzz coverage fuzz_inflate fuzz/seeds/fuzz_inflate
//! ```
//!
//! A bounded campaign reaches the gate by mutation regardless, and that was
//! measured rather than reasoned: a 45-second run from a completely EMPTY corpus
//! executed 544,437 inputs and retained a 1,412-unit corpus of which 489 units
//! (34.6 %, far above the 1-in-16 base rate, because gate-opening inputs reach
//! extra coverage and are therefore kept) open the gate, spanning all eleven
//! levels. So these seeds are about DETERMINISM at run zero and about honest
//! coverage measurement — not about unblocking a path that would otherwise be
//! dead.
//!
//! # C provenance
//!
//! The engine under test is the Rust port of `inflate.c`, `inftrees.c`,
//! `inffast.c`, and their headers, and the rejection paths probed here are the
//! ones `test/infcover.c` was written to cover. Those C sources are retained
//! in-tree as the cross-validation oracle and are read-only.

use libfuzzer_sys::fuzz_target;
use std::sync::{Once, OnceLock};
use zlib_rs::constants::{Z_FINISH, Z_NO_FLUSH};
use zlib_rs::inflate::tables::{CodeType, InflateTableError};
use zlib_rs::inflate::{
    Code, ENOUGH, ENOUGH_DISTS, ENOUGH_LENS, MAXBITS, inflate, inflate_end, inflate_init2,
    inflate_sync, inflate_table,
};
use zlib_rs::{ReturnCode, ZStream, ZlibError, compress_bound, compress2, uncompress};

// ===========================================================================
// Harness geometry — every bound below is a FIXED constant
// ===========================================================================

/// Output ceiling shared by every decode probe: 64 KiB, fixed.
///
/// Decoding untrusted input must stay bounded, so no buffer in this harness is
/// sized from a length field in the input or from a value read out of a decoded
/// header. A decompression bomb therefore stops at this ceiling instead of
/// exhausting the fuzzing run's resident-memory budget.
const OUT_CEILING: usize = 1 << 16;

/// Iteration budget for a streaming drain.
///
/// Forward-progress detection already terminates every loop below; this is the
/// belt to that pair of braces. A hang is worth reporting, but it must be the
/// library hanging and never the harness.
const WORK_BUDGET: u32 = 4096;

/// Iteration budget for the byte-at-a-time drip probe.
///
/// The drip costs one engine call per byte, so it is capped well below
/// [`OUT_CEILING`]: the state-machine boundaries it exists to cross are all in
/// the header and the first block, and an uncapped drip would dominate the
/// per-target time budget without reaching anything new.
const DRIP_BUDGET: u32 = 512;

/// Payload ceiling for the round-trip self-check.
///
/// Bounded so that the one probe which *encodes* before it decodes cannot
/// dominate the execution rate; the decode legs still see the full input.
const ROUND_TRIP_MAX: usize = 1024;

/// Scratch size for the round-trip's compressed intermediate.
///
/// Deliberately generous and checked against [`compress_bound`] at run time,
/// rather than re-deriving zlib's worst-case sizing formula here — the formula
/// is the library's to own, and this harness only observes it.
const ROUND_TRIP_SCRATCH: usize = 2048;

// ===========================================================================
// `windowBits` framing selectors — the overloaded contract, both ends
//
// `parse_window_bits` resolves one overloaded integer into four framings. Each
// range is probed at BOTH ends so the boundaries, where an off-by-one would
// hide, are covered rather than sampled at a single convenient value.
// ===========================================================================

/// zlib wrapper (RFC 1950) at the maximum 32 KiB window — top of `8..=15`.
const WBITS_ZLIB: i32 = 15;

/// zlib wrapper at the smallest window the contract admits — bottom of `8..=15`.
const WBITS_ZLIB_MIN: i32 = 8;

/// Raw DEFLATE (RFC 1951), no wrapper and no checksum — top of `-8..=-15`.
const WBITS_RAW: i32 = -15;

/// Raw DEFLATE at the smallest window — bottom of `-8..=-15`.
///
/// The low end of the raw range is where the overloading is easiest to get
/// wrong: `-8` must resolve to raw framing with a 256-byte window, and must not
/// be confused with the zlib `8` that shares its magnitude.
const WBITS_RAW_MIN: i32 = -8;

/// gzip wrapper (RFC 1952) at the maximum window — top of `24..=31` (`16 + 15`).
///
/// Reached only when [`gzip_framing_supported`] reports that the linked library
/// accepts it; see that function for why the guard is a run-time capability
/// query rather than a `#[cfg(feature = ...)]`.
const WBITS_GZIP: i32 = 31;

/// gzip wrapper at the smallest window — bottom of `24..=31` (`16 + 8`).
const WBITS_GZIP_MIN: i32 = 24;

/// zlib-or-gzip auto-detect at the maximum window — top of `40..=47`
/// (`32 + 15`).
const WBITS_AUTO: i32 = 47;

/// Auto-detect at the smallest window — bottom of `40..=47` (`32 + 8`).
const WBITS_AUTO_MIN: i32 = 40;

// ===========================================================================
// Compile-time observation of the decoder's load-bearing constants
//
// These are wire-format and arena bounds, not tunables: understating `ENOUGH`
// overflows the decode table on adversarial input, and overstating it wastes
// memory on every stream. They are OBSERVED here, never altered — asserting
// them at compile time costs nothing per execution and turns a future drift
// into a build failure rather than a fuzzing session that quietly proves less
// than it claims.
// ===========================================================================

const _: () = assert!(
    MAXBITS == 15,
    "MAXBITS must stay at the DEFLATE code-length limit"
);
const _: () = assert!(ENOUGH_LENS == 852, "ENOUGH_LENS must match inftrees.h");
const _: () = assert!(ENOUGH_DISTS == 592, "ENOUGH_DISTS must match inftrees.h");
const _: () = assert!(
    ENOUGH == ENOUGH_LENS + ENOUGH_DISTS,
    "ENOUGH is the sum of the two halves"
);
const _: () = assert!(ENOUGH == 1444, "ENOUGH must match inftrees.h");

/// Runs the constructed table-builder vectors exactly once per process.
///
/// Their inputs are fixed rather than fuzzer-derived, so re-running them on
/// every execution would buy no coverage while measurably lowering the
/// execution rate. Once per process still fails the very first execution if a
/// regression is present, which is all a deterministic vector can offer.
static FIXED_VECTORS: Once = Once::new();

// ===========================================================================
// Outcome checking
// ===========================================================================

/// Asserts that `code` is a member of the closed set of nine zlib return codes
/// and that its ABI integer still equals the `zlib.h` value.
///
/// The `match` is deliberately EXHAUSTIVE with no wildcard arm, so adding a
/// variant to `ReturnCode` becomes a compile error right here — which is
/// precisely the signal this harness wants, because a new code reaching a C
/// caller's `switch` is an ABI change. Every arm is legal: for arbitrary input
/// each of the nine is a legitimate answer, so what is checked is that the
/// answer is *defined*, never that it is any particular value.
///
/// The two assertions are complementary. The first pins the `#[repr(i32)]`
/// discriminant against the literal `zlib.h` integer, catching a silent
/// renumbering. The second requires the value to round-trip back through
/// `from_c_int`, which is the same normalisation the C boundary performs, so a
/// code that could not survive the trip out to C and back is rejected.
fn assert_defined_code(code: ReturnCode, context: &'static str) {
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
    let actual = code.as_c_int();
    assert_eq!(
        actual, expected,
        "{context}: return code {code:?} no longer carries its zlib.h integer"
    );
    assert_eq!(
        ReturnCode::from_c_int(actual),
        Some(code),
        "{context}: return code {code:?} does not round-trip through its C integer"
    );
}

/// Reports whether the linked `zlib-rs` accepts the gzip and auto-detect
/// `windowBits` framings. Probed once and cached.
///
/// # Why this is a run-time query and not `#[cfg(feature = "gzip")]`
///
/// Cargo features are per-crate, and this harness lives in a **detached**
/// workspace: `fuzz/Cargo.toml` declares its own `[workspace]` table and no
/// `[features]` section at all. A `cfg(feature = "gzip")` written here would
/// therefore ask whether *this* crate — `zlib-rs-fuzz` — has a `gzip` feature.
/// It does not, and cannot be given one from this file, because that manifest is
/// owned elsewhere and must stay byte-unchanged.
///
/// Such a `cfg` is not merely redundant, it is actively harmful: being
/// permanently false, it would compile the gzip and auto-detect legs out of the
/// **default** build, silently deleting all coverage of the RFC 1952 header
/// parser — the single most attacker-exposed surface in the whole decoder, and a
/// loss that no gate would report because the target would still build, still
/// link, and still find nothing. `rustc` names the mistake directly, as an
/// `unexpected_cfg_value` diagnostic, precisely because Cargo tells it this
/// crate has no features to test. The gating idiom used by the integration
/// suite does not transfer here: those files are compiled as part of the root
/// package, which *does* declare the feature.
///
/// Asking the engine instead is both correct and strictly stronger:
///
/// * It is an exact capability query. The wrap request is masked with `& 15`
///   only in a gzip-enabled build, so without that support a `windowBits` of
///   `40..=47` fails the `8..=15` bounds test and yields `Z_STREAM_ERROR` — the
///   same answer a C zlib built without `GUNZIP` gives. What is measured is
///   therefore the behaviour that actually matters, not a proxy for it.
/// * It keeps every line of this harness compiled and linked in **every**
///   feature configuration, so the binary can never lose its entry point to a
///   conditional — the failure mode a whole-file attribute would cause, and one
///   that a per-item attribute only avoids by accident.
/// * It costs one initialisation for the life of the process. No window is
///   allocated by a failing or succeeding init, so the probe is cheap enough to
///   be invisible in the execution rate.
fn gzip_framing_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();

    *SUPPORTED.get_or_init(|| {
        let mut strm = ZStream::new();
        let code = defined_code(
            inflate_init2(&mut strm, WBITS_AUTO),
            "inflate_init2 (gzip capability probe)",
        );
        // Teardown runs either way: the C contract requires it whenever init
        // reported success, and it is a defined no-op (`Z_STREAM_ERROR`, no
        // state) when it did not.
        defined_code(
            inflate_end(&mut strm),
            "inflate_end (gzip capability probe)",
        );
        code == ReturnCode::Ok
    })
}

/// Flattens an inflate init / reset / teardown result into a bare
/// [`ReturnCode`] and asserts it is defined.
///
/// Mirrors C, where every one of these entry points returns a plain `int`, so
/// success and failure can be checked uniformly. Returning the code (rather
/// than discarding it) lets the one constructed-input caller assert a specific
/// value without a second helper.
fn defined_code(result: Result<ReturnCode, ZlibError>, context: &'static str) -> ReturnCode {
    let code = result.unwrap_or_else(ZlibError::as_return_code);
    assert_defined_code(code, context);
    code
}

// ===========================================================================
// Deterministic harness choices, derived from the input without consuming it
// ===========================================================================

/// Mixes a bounded prefix of the input into a 64-bit value used to choose
/// framings, compression levels, and code types.
///
/// `xorshift64*` with a fixed seed: fully deterministic, so any finding
/// reproduces from the saved input alone; allocation-free; and dependent on no
/// random-number crate, because this workspace has none and the fuzzer's own
/// bytes are the entropy source. Crucially this *reads* a prefix rather than
/// splitting a selector byte off the front, so the whole slice still reaches
/// every decoder unmodified — an inflate corpus is far more valuable intact.
/// The prefix is bounded so the mixer can never become the hot path.
///
/// # Bit budget
///
/// The result's bits are partitioned so that no two harness choices are drawn
/// from the same field. Overlapping fields would correlate the choices and
/// silently narrow what the sweep actually covers — for instance tying the
/// round-trip sampling gate to the framing endpoints, so that half the framing
/// combinations were never seen alongside a round trip:
///
/// | Bits | Choice |
/// |------|--------|
/// | `0..=3` | which end of each of the four `windowBits` ranges is used (one bit per framing) |
/// | `4..=7` | which framing the byte-at-a-time drip uses |
/// | `8..=15` | the round-trip compression level |
/// | `16..=19` | the round-trip sampling gate |
/// | `20..=23` | the table builder's code type |
fn selector(data: &[u8]) -> u64 {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for &byte in data.iter().take(8) {
        state ^= u64::from(byte);
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
    }
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Maps bits `8..=15` of a selector to a compression level in `-1..=9` for the
/// round-trip leg.
///
/// All eleven accepted values are reachable: `-1` is `Z_DEFAULT_COMPRESSION`,
/// `0` is `Z_NO_COMPRESSION` (stored blocks, a distinct decoder path worth
/// covering), and `9` is `Z_BEST_COMPRESSION`.
fn level_from(selector: u64) -> i32 {
    let index = ((selector >> 8) & 0xFF) % 11;
    // `index` is 0..=10 by construction, so the conversion cannot fail; the
    // fallback keeps the harness panic-free without an unchecked cast.
    i32::try_from(index).unwrap_or(0) - 1
}

// ===========================================================================
// The bounded streaming drain
// ===========================================================================

/// Drains `strm` over `input` into `out`, returning the terminal code together
/// with the input and output cursors reached.
///
/// This is the fuzz-target inversion of the loop the integration suite uses.
/// There, input is known-good, so an unexpected code is a test failure and the
/// loop panics on it. Here input is arbitrary, so a data error, a buffer error,
/// or a stall are all *correct* engine behaviour and each is a graceful exit.
///
/// The only panics are the two library invariants asserted inside the loop. An
/// engine that reports consuming more bytes than it was handed, or producing
/// more than the output room it was given, has already overstepped a buffer —
/// and detecting that is the entire point of pointing a fuzzer at a decoder.
/// Those two assertions hold for every possible input, so they can never fire
/// spuriously.
fn decode_bounded(
    strm: &mut ZStream,
    input: &[u8],
    out: &mut [u8],
    flush: i32,
) -> (ReturnCode, usize, usize) {
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut budget = WORK_BUDGET;

    loop {
        let avail_in = input.len() - in_pos;
        let avail_out = out.len() - out_pos;
        let outcome = inflate(strm, &input[in_pos..], &mut out[out_pos..], flush);

        assert!(
            outcome.consumed <= avail_in,
            "inflate reported consuming {} of {avail_in} available input bytes",
            outcome.consumed
        );
        assert!(
            outcome.produced <= avail_out,
            "inflate reported producing {} into {avail_out} bytes of output room",
            outcome.produced
        );
        assert_defined_code(outcome.code, "inflate");

        // Sound because of the two assertions above: neither cursor can pass the
        // length of its slice.
        in_pos += outcome.consumed;
        out_pos += outcome.produced;

        if outcome.code != ReturnCode::Ok {
            return (outcome.code, in_pos, out_pos);
        }

        // Neither cursor moved on a `Z_OK` pass, so repeating the call cannot
        // change anything: the input is truncated, or the fixed output ceiling
        // has been reached. That is a defined stall rather than a defect, and
        // `Z_BUF_ERROR` is exactly what zlib reports for "no progress is
        // possible", so it is what the harness reports too.
        if outcome.consumed == 0 && outcome.produced == 0 {
            return (ReturnCode::BufError, in_pos, out_pos);
        }

        // A countdown with `checked_sub` rather than a bare decrement: the fuzz
        // profile builds with `overflow-checks` on deliberately, so arithmetic
        // is kept explicitly in range instead of relying on a wrap.
        match budget.checked_sub(1) {
            Some(left) => budget = left,
            None => return (ReturnCode::BufError, in_pos, out_pos),
        }
    }
}

// ===========================================================================
// Probes
// ===========================================================================

/// Runs one framing probe end to end: init, bounded drain, `inflateSync`
/// recovery on a data error, and the mandatory teardown.
///
/// `out` is the caller's single fixed-size buffer, reused across framings so
/// that sweeping four of them costs one allocation rather than four.
fn probe_framing(data: &[u8], window_bits: i32, out: &mut [u8]) {
    let mut strm = ZStream::new();

    // Every value this harness passes is inside a documented range for the
    // feature set it was compiled with, so init is required to succeed. A
    // rejection here would itself be the finding, which is why it is asserted
    // rather than quietly skipped.
    assert_eq!(
        defined_code(inflate_init2(&mut strm, window_bits), "inflate_init2"),
        ReturnCode::Ok,
        "inflate_init2 rejected windowBits {window_bits}, which is inside a supported range"
    );

    let (code, in_pos, _produced) = decode_bounded(&mut strm, data, out, Z_NO_FLUSH);

    // A corrupt stream is the normal case here, and zlib's answer to it is
    // `inflateSync`: scan forward for a full flush marker and resume at the
    // block that follows. It must either find one or fail with a defined code.
    if code == ReturnCode::DataError {
        let tail = &data[in_pos..];
        let (sync_code, sync_consumed) = inflate_sync(&mut strm, tail);
        assert!(
            sync_consumed <= tail.len(),
            "inflate_sync reported consuming {sync_consumed} of {} available bytes",
            tail.len()
        );
        assert_defined_code(sync_code, "inflate_sync");

        // Resynchronisation reported success, so the engine claims decoding can
        // resume. Take it at its word: a second bounded drain over the
        // remainder must also terminate in a defined state.
        if sync_code == ReturnCode::Ok {
            let (resumed, _, _) =
                decode_bounded(&mut strm, &tail[sync_consumed..], out, Z_NO_FLUSH);
            assert_defined_code(resumed, "inflate after inflate_sync");
        }
    }

    // `inflate_end` is called unconditionally, exactly as a C caller must.
    defined_code(inflate_end(&mut strm), "inflate_end");
}

/// Feeds `data` one byte at a time into a one-byte output window.
///
/// Single-byte availability forces the mode loop to suspend and resume at every
/// header-field boundary, every bit-accumulator refill, and every window copy —
/// the schedule under which a cursor or accumulator mistake stops being
/// invisible. The step count is capped because the probe costs one engine call
/// per byte.
fn probe_drip(data: &[u8], window_bits: i32) {
    let mut strm = ZStream::new();
    assert_eq!(
        defined_code(
            inflate_init2(&mut strm, window_bits),
            "inflate_init2 (drip)"
        ),
        ReturnCode::Ok,
        "inflate_init2 rejected windowBits {window_bits}, which is inside a supported range"
    );

    let mut byte = [0u8; 1];
    let mut in_pos = 0usize;
    let mut budget = DRIP_BUDGET;

    while let Some(left) = budget.checked_sub(1) {
        budget = left;

        let end = (in_pos + 1).min(data.len());
        let avail_in = end - in_pos;
        let outcome = inflate(&mut strm, &data[in_pos..end], &mut byte, Z_NO_FLUSH);

        assert!(
            outcome.consumed <= avail_in,
            "inflate consumed {} of {avail_in} bytes in a 1-byte input window",
            outcome.consumed
        );
        assert!(
            outcome.produced <= byte.len(),
            "inflate produced {} bytes into a 1-byte output window",
            outcome.produced
        );
        assert_defined_code(outcome.code, "inflate (1-byte windows)");

        in_pos += outcome.consumed;

        // Anything other than `Z_OK`, and any pass that moved neither cursor,
        // ends the probe: both are defined outcomes for arbitrary input.
        if outcome.code != ReturnCode::Ok || (outcome.consumed == 0 && outcome.produced == 0) {
            break;
        }
    }

    defined_code(inflate_end(&mut strm), "inflate_end (drip)");
}

/// Encodes a bounded prefix of `data` and requires the decoder to recover it
/// byte for byte.
///
/// This is the ONE place a specific return code is asserted, and it is
/// legitimate precisely because the input is not arbitrary: the harness built
/// this stream itself, so `Z_STREAM_END` with an exact byte match is the only
/// correct answer. A mismatch is a correctness bug in the decoder rather than a
/// malformed-input rejection — the class of defect that a "does it panic?"
/// harness alone would never surface, because a wrong-but-plausible decode
/// panics nowhere.
///
/// Both buffers are fixed-size stack arrays, so this probe adds no heap traffic
/// and no input-derived sizing.
fn probe_round_trip(data: &[u8], level: i32) {
    let payload = &data[..data.len().min(ROUND_TRIP_MAX)];

    let mut scratch = [0u8; ROUND_TRIP_SCRATCH];
    assert!(
        compress_bound(payload.len()) <= scratch.len(),
        "round-trip scratch is smaller than zlib's own worst-case bound"
    );

    let compressed_len = match compress2(&mut scratch, payload, level) {
        Ok(len) => len,
        Err(code) => {
            // The encoder is not what this harness tests, so a refusal is
            // reported as a defined code and the probe simply ends. It is still
            // checked, because an out-of-contract value here would be a finding.
            assert_defined_code(code, "compress2");
            return;
        }
    };

    let mut strm = ZStream::new();
    assert_eq!(
        defined_code(
            inflate_init2(&mut strm, WBITS_ZLIB),
            "inflate_init2 (round trip)"
        ),
        ReturnCode::Ok,
        "inflate_init2 must accept the maximum zlib window"
    );

    let mut restored = [0u8; ROUND_TRIP_MAX];
    let (code, consumed, produced) = decode_bounded(
        &mut strm,
        &scratch[..compressed_len],
        &mut restored,
        Z_FINISH,
    );

    assert_eq!(
        code,
        ReturnCode::StreamEnd,
        "a self-produced level-{level} stream did not decode to Z_STREAM_END"
    );
    assert_eq!(
        consumed,
        compressed_len,
        "the decoder left {} bytes of a self-produced stream unread",
        compressed_len - consumed
    );
    assert_eq!(produced, payload.len(), "round-trip length mismatch");
    assert_eq!(
        &restored[..produced],
        payload,
        "round-trip content mismatch"
    );

    defined_code(inflate_end(&mut strm), "inflate_end (round trip)");
}

/// Drives the decode-table builder directly with fuzzer-derived code lengths.
///
/// Malformed dynamic-Huffman headers reach most of these rejections through a
/// byte stream, but not the arena bound: `test/infcover.c` reaches for
/// `inflate_table` itself for exactly that reason. Doing both means the harness
/// covers the builder's validation prologue from the outside *and* from the
/// inside.
///
/// Only LEGALITY is asserted, because for arbitrary lengths each of `Ok(())`,
/// `Err(Invalid)`, and `Err(Enough)` is a correct answer. The `match` is
/// exhaustive with no wildcard, so a new error variant becomes a compile error.
/// The accompanying out-parameter checks are true invariants of the port: the
/// builder writes `table_index` and `bits` only on its success path, so a
/// rejection must leave both exactly as they were passed, and a success must
/// leave the arena cursor inside the arena and the root-bit count inside
/// `1..=MAXBITS`.
fn probe_inflate_table(selector: u64, data: &[u8]) {
    // `MAXBITS + 1` code lengths: the same scale `test/infcover.c` uses, and
    // enough to reach every branch of the validation prologue.
    let mut lens = [0u16; MAXBITS + 1];
    for (slot, &byte) in lens.iter_mut().zip(data.iter()) {
        // Modulo 24, not 16, on purpose: roughly a third of the values land
        // above the DEFLATE limit, so the "code length exceeds MAXBITS"
        // rejection is reached as well as the arithmetic ones below it.
        *slot = u16::from(byte % 24);
    }

    // Bits 20..=23 of the selector, per its documented bit budget.
    let code_type = match ((selector >> 20) & 0xF) % 3 {
        0 => CodeType::Codes,
        1 => CodeType::Lens,
        _ => CodeType::Dists,
    };

    // `codes` is allowed to reach `lens.len()` and to exceed the alphabet size
    // of the chosen code type, which is what reaches the geometry rejections.
    let codes = usize::from(data.first().copied().unwrap_or(0)) % (lens.len() + 1);
    let requested_bits = usize::from(data.last().copied().unwrap_or(1)) % (MAXBITS + 1);

    let mut work = [0u16; MAXBITS + 1];
    // The arena the real decoder hands the builder. Sizing it at `ENOUGH` keeps
    // the harness clear of the one documented fault case — an arena large
    // enough to pass the free-entry guard yet still too small for the code
    // being built — so any fault reaching this call site is a genuine finding
    // and not a self-inflicted one.
    let mut table = [Code::default(); ENOUGH];
    let mut table_index = 0usize;
    let mut bits = requested_bits;

    let outcome = inflate_table(
        code_type,
        &lens,
        codes,
        &mut table,
        &mut table_index,
        &mut bits,
        &mut work,
    );

    match outcome {
        Ok(()) => {
            assert!(
                table_index <= table.len(),
                "the builder left its arena cursor at {table_index}, past the {} entries it owns",
                table.len()
            );
            assert!(
                (1..=MAXBITS).contains(&bits),
                "the builder reported {bits} root index bits, outside 1..={MAXBITS}"
            );
        }
        Err(InflateTableError::Invalid) => {
            assert_eq!(
                table_index, 0,
                "a rejected code set must publish no table entry"
            );
            assert_eq!(
                bits, requested_bits,
                "a rejected code set must leave the root-bit request untouched"
            );
        }
        Err(InflateTableError::Enough) => {
            assert_eq!(
                table_index, 0,
                "an arena-bound rejection must publish no table entry"
            );
            assert_eq!(
                bits, requested_bits,
                "an arena-bound rejection must leave the root-bit request untouched"
            );
        }
    }
}

/// The constructed table-builder vectors.
///
/// Each pins a SPECIFIC error variant, which is legitimate because these inputs
/// are built here rather than supplied by the fuzzer, so each has exactly one
/// correct answer. Together they cover both of C's non-zero `inflate_table`
/// returns and both halves of its `(type == CODES || max != 1)` disjunction.
fn probe_inflate_table_fixed_vectors() {
    // --- (a) the ENOUGH arena bound, C's `+1` -------------------------------
    // `test/infcover.c`'s `cover_trees` vector: lengths 1..=15 plus a second
    // length-15 code. Sixteen distance codes cannot fit the `ENOUGH_DISTS`
    // arena at either root-bit request — the large request overflows on the
    // root table, the small one on a sub-table — so both must report the bound
    // instead of building a table.
    let mut lens = [0u16; MAXBITS + 1];
    for (index, slot) in lens.iter_mut().enumerate().take(MAXBITS) {
        *slot = (index + 1) as u16;
    }
    lens[MAXBITS] = MAXBITS as u16;

    let mut work = [0u16; MAXBITS + 1];
    let mut dists_arena = [Code::default(); ENOUGH_DISTS];
    for root_bits in [MAXBITS, 1] {
        let mut table_index = 0usize;
        let mut bits = root_bits;
        assert_eq!(
            inflate_table(
                CodeType::Dists,
                &lens,
                lens.len(),
                &mut dists_arena,
                &mut table_index,
                &mut bits,
                &mut work,
            ),
            Err(InflateTableError::Enough),
            "sixteen distance codes must exceed the ENOUGH_DISTS arena at {root_bits} root bits"
        );
        assert_eq!(
            table_index, 0,
            "an arena-bound rejection publishes no entry"
        );
    }

    // --- (b) over-subscription and (c) an incomplete set, both C's `-1` -----
    // Three length-1 codes claim three of the two available one-bit slots, so
    // the Kraft sum goes negative. Three length-2 codes claim three of four
    // two-bit slots, so it stays positive with a maximum length above one.
    // Both are rejected for every code type.
    let over_subscribed = [1u16, 1, 1];
    let incomplete = [2u16, 2, 2];
    for code_type in [CodeType::Codes, CodeType::Lens, CodeType::Dists] {
        for (vector, description) in [
            (&over_subscribed, "over-subscribed"),
            (&incomplete, "incomplete"),
        ] {
            let mut work = [0u16; 3];
            let mut arena = [Code::default(); ENOUGH];
            let mut table_index = 0usize;
            let mut bits = 7usize;
            assert_eq!(
                inflate_table(
                    code_type,
                    vector,
                    vector.len(),
                    &mut arena,
                    &mut table_index,
                    &mut bits,
                    &mut work,
                ),
                Err(InflateTableError::Invalid),
                "{code_type:?}: an {description} code set must be rejected"
            );
            assert_eq!(table_index, 0, "{code_type:?}: no table entry is published");
            assert_eq!(bits, 7, "{code_type:?}: the root-bit request is untouched");
        }
    }

    // --- (d) incomplete with a maximum length of one -----------------------
    // A lone length-1 code is also incomplete, but its maximum length is one,
    // so the `max != 1` half of C's disjunction is false and the `type ==
    // CODES` half is what rejects it: the 19-symbol alphabet that encodes a
    // dynamic block's code lengths must be complete. The same vector is
    // deliberately ACCEPTED for the literal/length and distance alphabets,
    // because a dynamic block declaring exactly one distance code is legal and
    // "hardening" that into an error would reject streams reference zlib
    // decodes — an acceptance-parity break. Only the rejecting half is pinned
    // here; the accepting half is already covered by the integration suite.
    let lone = [1u16];
    let mut work = [0u16; 1];
    let mut arena = [Code::default(); ENOUGH];
    let mut table_index = 0usize;
    let mut bits = 7usize;
    assert_eq!(
        inflate_table(
            CodeType::Codes,
            &lone,
            lone.len(),
            &mut arena,
            &mut table_index,
            &mut bits,
            &mut work,
        ),
        Err(InflateTableError::Invalid),
        "a lone length-1 code is incomplete, and the code-length alphabet requires completeness"
    );
    assert_eq!(table_index, 0, "no table entry is published");
    assert_eq!(bits, 7, "the root-bit request is untouched");
}

fuzz_target!(|data: &[u8]| {
    // Deterministic harness choices, mixed from a bounded prefix. Nothing is
    // stripped from `data`, so every probe below still sees the whole slice.
    let sel = selector(data);

    // A fixed 64 KiB output ceiling: decoding untrusted input must stay bounded.
    // Every outcome (Ok, or a Data/Buf/Stream error) is acceptable — the only
    // failure this harness catches is a panic / UB inside the decoder. This one
    // buffer is reused by every decode probe below, so sweeping four framings
    // costs one allocation and never sizes memory from the input.
    let mut out = vec![0u8; OUT_CEILING];

    // --- The one-call zlib-framed wrapper ---------------------------------
    // Its result is checked for legal-code membership rather than discarded, so
    // an out-of-contract value cannot pass silently. No SPECIFIC code is
    // asserted: for arbitrary bytes every defined code is a legitimate answer.
    match uncompress(&mut out, data) {
        Ok(produced) => assert!(
            produced <= OUT_CEILING,
            "uncompress reported writing {produced} bytes into a {OUT_CEILING}-byte buffer"
        ),
        Err(code) => assert_defined_code(code, "uncompress"),
    }

    // --- Every framing, through the safe streaming engine ------------------
    // All four are swept on every execution; the selector chooses which END of
    // each documented range is used, so the boundaries get covered too.
    probe_framing(
        data,
        if sel & 1 == 0 {
            WBITS_ZLIB
        } else {
            WBITS_ZLIB_MIN
        },
        &mut out,
    );
    probe_framing(
        data,
        if sel & 2 == 0 {
            WBITS_RAW
        } else {
            WBITS_RAW_MIN
        },
        &mut out,
    );
    // The gzip framings are guarded per statement, never by a whole-file inner
    // attribute — that would compile away the harness entry point along with the
    // probes and leave a binary with no `main` to link. The guard is a run-time
    // capability query rather than a feature `cfg` because a `cfg` cannot see a
    // dependency's features from a detached workspace; see
    // `gzip_framing_supported` for the full reasoning.
    let gzip_supported = gzip_framing_supported();
    if gzip_supported {
        probe_framing(
            data,
            if sel & 4 == 0 {
                WBITS_GZIP
            } else {
                WBITS_GZIP_MIN
            },
            &mut out,
        );
        probe_framing(
            data,
            if sel & 8 == 0 {
                WBITS_AUTO
            } else {
                WBITS_AUTO_MIN
            },
            &mut out,
        );
    }

    // --- Byte-at-a-time delivery ------------------------------------------
    // A one-byte-at-a-time gzip header parse is a particularly good place to
    // look for a boundary mistake, so the gzip framings join the rotation
    // whenever the linked library accepts them.
    let drip_framings: &[i32] = if gzip_supported {
        &[WBITS_ZLIB, WBITS_RAW, WBITS_GZIP, WBITS_AUTO]
    } else {
        &[WBITS_ZLIB, WBITS_RAW]
    };
    // Bits 4..=7 of the selector, per its documented bit budget.
    let drip_pick = usize::try_from((sel >> 4) & 0xF).unwrap_or(0) % drip_framings.len();
    probe_drip(data, drip_framings[drip_pick]);

    // --- The decode-table builder -----------------------------------------
    FIXED_VECTORS.call_once(probe_inflate_table_fixed_vectors);
    probe_inflate_table(sel, data);

    // --- Round-trip self-check --------------------------------------------
    // Sampled on one execution in sixteen (bits 16..=19 of the selector), which
    // is the only probe here that is not run every time. It is the only one that
    // ENCODES before it decodes, so it pays for a full deflate state — by far
    // the most expensive thing in this harness — and the sampling is a measured
    // improvement rather than a saving of effort: running it every time yielded
    // 6,560 exec/s and 2,060 covered edges from a cold corpus over the same
    // wall time, while sampling yielded 12,970 exec/s and 2,398 edges. The extra
    // executions buy the fuzzer more of the decoder than the extra round trips
    // do, and at that rate the check still runs many thousands of times per
    // minute across every compression level. The gate is derived from the input,
    // so a finding still reproduces from the saved input alone.
    //
    // Because it is derived from the input — from the first eight bytes only —
    // an input either always opens this gate or never does, which makes reaching
    // it a property of the corpus rather than of the run length. That is why
    // eleven gate-opening seeds, one per accepted level, are committed under
    // `fuzz/seeds/fuzz_inflate/`; see "Committed seed corpus" in the module
    // documentation for how they are built and how to pass them.
    if (sel >> 16) & 0xF == 0 {
        probe_round_trip(data, level_from(sel));
    }
});
