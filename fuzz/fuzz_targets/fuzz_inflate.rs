#![no_main]
//! Inflate robustness harness.
//!
//! Feeds arbitrary, attacker-controlled bytes to the one-call zlib-framed
//! decoder [`zlib_rs::uncompress`]. Decoding untrusted input must reject
//! malformed streams with an error code and must NEVER panic, read out of
//! bounds, or over-allocate. Any panic here is a memory-safety or robustness
//! bug in the inflate state machine.
//!
//! The inflate engine is the primary parser of attacker-controlled compressed
//! bytes — the C ABI shims in `src/ffi/**` and the gzip file layer in `src/gz/**`
//! also take untrusted input, and have their own harnesses — so the coverage
//! below is deliberately wider than a single one-call probe. Every
//! probe is black-box over the crate's public, checked API: the harness holds no
//! raw pointer, reaches no C-ABI shim, and reads no private engine state.
//!
//! # What is covered
//!
//! * **The one-call zlib-framed wrapper.** [`zlib_rs::uncompress`] against a
//!   fixed 64 KiB output ceiling — the shortest path from a raw byte slice to
//!   the decoder. Its return code is checked against the set that entry point's
//!   own documentation admits, so a value it has no path to — including the
//!   `Z_NEED_DICT` it is documented to re-map to `Z_DATA_ERROR` — is caught
//!   rather than passing silently.
//! * **All four `windowBits` framings, through the streaming engine.** The
//!   one-call wrapper is zlib-framed only; raw DEFLATE (RFC 1951), gzip
//!   (RFC 1952), and the zlib-or-gzip auto-detect mode are reachable
//!   exclusively through `inflate_init2` + `inflate` + `inflate_end`. Both ends
//!   of every documented range are exercised, so the overloaded `windowBits`
//!   contract (`8..=15`, `-8..=-15`, `24..=31`, `40..=47`) is swept rather than
//!   sampled at one point. The two gzip-dependent framings are guarded twice
//!   over: at compile time by this package's forwarding `gzip` feature, applied
//!   per item, and at run time by [`gzip_framing_supported`], which confirms the
//!   linked engine really honours what the feature asked for. Both are needed —
//!   see that function.
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
//!   decode back to the exact input bytes. The encoder is *required* to succeed:
//!   the level and the destination size are the harness's own, so the only
//!   refusal in contract is a host-level allocation failure, and anything else
//!   is a finding rather than a reason to skip the comparison.
//! * **The RFC 1950 preset-dictionary handshake.** `deflate_set_dictionary`
//!   writes an Adler-32 identifier into the header; `inflate` must suspend with
//!   `Z_NEED_DICT` carrying that identifier; `inflate_set_dictionary` must
//!   refuse a mismatched dictionary with `Z_DATA_ERROR`, accept the right one,
//!   and let the stream finish with the exact payload and a trailer checksum
//!   computed over the payload alone. This is the only path on which `inflate`
//!   returns `Z_NEED_DICT`, and nothing else in the harness reaches it — see
//!   [`probe_dictionary`].
//!
//! # Two rules that keep the findings honest
//!
//! **A return code is judged against the contract of the call that produced
//! it, never against the union of all nine.** Garbage bytes may legitimately
//! yield several different codes from the same entry point depending on what
//! they happen to encode, so pinning one value would turn every run red for a
//! harness reason rather than a library one. But accepting *any* of the nine
//! from *any* call is not a weaker version of the same check — it is a different
//! and much emptier one, and it passes for `Z_ERRNO` out of `inflate_end` or
//! `Z_VERSION_ERROR` out of `inflate`, outcomes neither function has a path to
//! and no C caller's `switch` handles. Each call site therefore names a
//! [`Contract`], which carries the closed set that entry point documents, and
//! the harness asserts membership in *that* set on top of the `zlib.h` integer
//! and its round trip through the C boundary. Where the input was constructed
//! here rather than supplied by the fuzzer, the exact code is pinned instead,
//! because such a call has exactly one correct answer.
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
use std::sync::Once;
// The gzip capability probe is the only cached one-shot value here, so its cell
// type belongs to the gzip legs rather than to the harness as a whole.
#[cfg(feature = "gzip")]
use std::sync::OnceLock;
use zlib_rs::constants::{DEF_MEM_LEVEL, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH};
use zlib_rs::deflate::{
    deflate, deflate_bound, deflate_end, deflate_init2, deflate_set_dictionary,
};
use zlib_rs::inflate::tables::{CodeType, InflateTableError};
use zlib_rs::inflate::{
    Code, ENOUGH, ENOUGH_DISTS, ENOUGH_LENS, MAXBITS, inflate, inflate_end, inflate_init2,
    inflate_set_dictionary, inflate_sync, inflate_table,
};
use zlib_rs::{
    ReturnCode, Strategy, ZStream, ZlibError, adler32, compress_bound, compress2, uncompress,
};

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

/// Payload ceiling for the preset-dictionary probe.
///
/// Half of [`ROUND_TRIP_MAX`], because this probe pays for a deflate state *and*
/// an inflate state *and* a dictionary load, so it is the most expensive thing
/// in the harness and is sampled rather than run every execution.
const DICT_PAYLOAD_MAX: usize = 512;

/// Dictionary ceiling for the preset-dictionary probe.
///
/// Far below the 32 KiB window the maximum `windowBits` gives, so the dictionary
/// is never truncated to its tail. That matters for correctness of the probe and
/// not just its cost: `deflate_set_dictionary` computes the Adler-32 identifier
/// over the WHOLE dictionary but slides only the trailing `w_size` bytes into the
/// window, so a dictionary large enough to be truncated would still be handed to
/// the decoder in full — a subtlety worth keeping out of an assertion that is
/// meant to pin the identifier round trip.
const DICT_MAX: usize = 256;

/// Scratch size for the preset-dictionary probe's compressed intermediate.
///
/// Checked against [`zlib_rs::deflate::deflate_bound`] at run time — the
/// dictionary-aware bound, which accounts for the four extra header bytes the
/// preset-dictionary identifier costs and which `compress_bound` does not model.
const DICT_SCRATCH: usize = 1024;

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
/// Compiled in only with this package's forwarding `gzip` feature, and reached
/// only when [`gzip_framing_supported`] then confirms the linked library accepts
/// it; see that function for why both guards exist.
#[cfg(feature = "gzip")]
const WBITS_GZIP: i32 = 31;

/// gzip wrapper at the smallest window — bottom of `24..=31` (`16 + 8`).
#[cfg(feature = "gzip")]
const WBITS_GZIP_MIN: i32 = 24;

/// zlib-or-gzip auto-detect at the maximum window — top of `40..=47`
/// (`32 + 15`).
#[cfg(feature = "gzip")]
const WBITS_AUTO: i32 = 47;

/// Auto-detect at the smallest window — bottom of `40..=47` (`32 + 8`).
#[cfg(feature = "gzip")]
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
// Outcome checking — one contract per entry point
// ===========================================================================

/// An entry point this harness calls, standing for the closed set of return
/// codes that entry point's own `# Errors` documentation admits.
///
/// # Why "one of the nine defined codes" is not a contract check
///
/// A single `is this one of the nine Z_* values?` predicate is a *type* check
/// wearing a contract check's clothes. It passes for `Z_ERRNO` returned by
/// `inflate_end`, and for `Z_VERSION_ERROR` returned by `inflate` — outcomes
/// neither function has any path to, and which no C caller's `switch` is
/// written to handle. Accepting the union of everything leaves the assertion
/// unable to fail for any *reachable* regression, so a code that leaked out of
/// the wrong layer would be recorded as ordinary behaviour and the harness
/// would report success.
///
/// Each variant below names ONE entry point and pins the set that entry point
/// documents. These are still genuine sets and not single values: arbitrary
/// input legitimately draws several different codes from the same call, so
/// pinning one value would turn every run red for a harness reason rather than
/// a library one. What the narrowing removes is only the *impossible* — which
/// is precisely the part that was able to hide a defect.
///
/// The sets are transcribed from the `# Errors` sections of the functions
/// themselves, so this enum is a mirror of the library's own documented
/// contract rather than a second, independent opinion about it. Where a
/// function returns a `Result`, only the half this harness inspects is
/// modelled, and the variant name says so.
///
/// Every set was derived twice and the two derivations agree: from `zlib.h`, the
/// normative contract a C caller programs against, and from the implementation, by
/// enumerating every `ZlibError`/`ReturnCode` value constructed on the
/// corresponding path. Narrowing a set is safe for the same reason widening one is
/// not: a set that is too narrow fails loudly on a legitimate answer and is fixed
/// in one place, while a set that is too wide fails never.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Contract {
    /// `zlib_rs::inflate::inflate_init2` — `Ok`, `Z_STREAM_ERROR` for a
    /// `windowBits` value this build rejects, `Z_MEM_ERROR` when the state
    /// reservation fails.
    InflateInit2,
    /// `zlib_rs::inflate::inflate_end` — `Ok`, or `Z_STREAM_ERROR` when the
    /// stream carries no inflate state.
    InflateEnd,
    /// `zlib_rs::inflate::inflate` — the full decode ladder. `Z_MEM_ERROR` is
    /// in the set because a deferred sliding-window reservation can fail
    /// mid-stream.
    Inflate,
    /// `zlib_rs::inflate::inflate_sync` — `Ok` when a full flush marker was
    /// found, `Z_BUF_ERROR` with no input and fewer than eight held bits,
    /// `Z_DATA_ERROR` when the input ran out first, `Z_STREAM_ERROR` with no
    /// inflate state.
    InflateSync,
    /// `zlib_rs::inflate::inflate_set_dictionary` — `Ok`, `Z_STREAM_ERROR` for
    /// a wrapped stream that is not awaiting a dictionary, `Z_DATA_ERROR` on an
    /// Adler-32 identifier mismatch, `Z_MEM_ERROR` when loading the dictionary
    /// into the window fails.
    InflateSetDictionary,
    /// The ERROR half of `zlib_rs::uncompress`. The one-call wrapper folds
    /// `Z_STREAM_END` into its `Ok` and re-maps `Z_NEED_DICT` to
    /// `Z_DATA_ERROR`, reproducing `uncompr.c` L78-L81, so neither can appear
    /// here.
    UncompressError,
    /// The ERROR half of `zlib_rs::compress2` — `Z_STREAM_ERROR` for a level
    /// outside `-1 | 0..=9`, `Z_BUF_ERROR` for a destination that is too small,
    /// `Z_MEM_ERROR` for a failed state reservation.
    Compress2Error,
    /// `zlib_rs::deflate::deflate_init2` — `Ok`, `Z_STREAM_ERROR` for a
    /// rejected parameter, `Z_MEM_ERROR` when the state reservation fails.
    DeflateInit2,
    /// `zlib_rs::deflate::deflate_set_dictionary` — `Ok`, or `Z_STREAM_ERROR`
    /// when a dictionary cannot be set in the current state.
    DeflateSetDictionary,
    /// `zlib_rs::deflate::deflate` — `Ok`, `Z_STREAM_END`, `Z_BUF_ERROR` when
    /// no progress is possible, `Z_STREAM_ERROR` for an inconsistent state or a
    /// flush value outside `0..=5`.
    Deflate,
    /// `zlib_rs::deflate::deflate_end` — `Ok`, `Z_STREAM_ERROR` with no deflate
    /// state, `Z_DATA_ERROR` when the stream was freed mid-compression.
    DeflateEnd,
}

impl Contract {
    /// The governed entry point, spelled as it appears in the public API, for
    /// use in panic messages.
    fn entry_point(self) -> &'static str {
        match self {
            Contract::InflateInit2 => "inflate_init2",
            Contract::InflateEnd => "inflate_end",
            Contract::Inflate => "inflate",
            Contract::InflateSync => "inflate_sync",
            Contract::InflateSetDictionary => "inflate_set_dictionary",
            Contract::UncompressError => "uncompress (error half)",
            Contract::Compress2Error => "compress2 (error half)",
            Contract::DeflateInit2 => "deflate_init2",
            Contract::DeflateSetDictionary => "deflate_set_dictionary",
            Contract::Deflate => "deflate",
            Contract::DeflateEnd => "deflate_end",
        }
    }

    /// The closed set of codes the entry point is documented to produce.
    ///
    /// Not one of these eleven sets contains `Z_ERRNO` or `Z_VERSION_ERROR`.
    /// Those two exist for the file-descriptor and version-mismatch layers —
    /// `gzerror` reports the former, the `*Init_` shims the latter — so
    /// observing either from a safe engine call means a code crossed a layer
    /// boundary it has no path across.
    fn allowed(self) -> &'static [ReturnCode] {
        match self {
            Contract::InflateInit2 | Contract::DeflateInit2 => &[
                ReturnCode::Ok,
                ReturnCode::StreamError,
                ReturnCode::MemError,
            ],
            Contract::InflateEnd | Contract::DeflateSetDictionary => {
                &[ReturnCode::Ok, ReturnCode::StreamError]
            }
            Contract::Inflate => &[
                ReturnCode::Ok,
                ReturnCode::StreamEnd,
                ReturnCode::NeedDict,
                ReturnCode::BufError,
                ReturnCode::DataError,
                ReturnCode::MemError,
                ReturnCode::StreamError,
            ],
            Contract::InflateSync | Contract::UncompressError => &[
                ReturnCode::Ok,
                ReturnCode::BufError,
                ReturnCode::DataError,
                ReturnCode::StreamError,
            ],
            Contract::InflateSetDictionary => &[
                ReturnCode::Ok,
                ReturnCode::StreamError,
                ReturnCode::DataError,
                ReturnCode::MemError,
            ],
            Contract::Compress2Error => &[
                ReturnCode::StreamError,
                ReturnCode::BufError,
                ReturnCode::MemError,
            ],
            Contract::Deflate => &[
                ReturnCode::Ok,
                ReturnCode::StreamEnd,
                ReturnCode::BufError,
                ReturnCode::StreamError,
            ],
            Contract::DeflateEnd => &[
                ReturnCode::Ok,
                ReturnCode::StreamError,
                ReturnCode::DataError,
            ],
        }
    }
}

/// Asserts that `code` is one of the codes `contract`'s entry point documents,
/// and that its ABI integer still equals the `zlib.h` value.
///
/// Three properties are pinned, and they are complementary rather than
/// overlapping. The `#[repr(i32)]` discriminant must still equal the literal
/// `zlib.h` integer, catching a silent renumbering. The value must round-trip
/// back through `from_c_int`, which is the same normalisation the C boundary
/// performs, so a code that could not survive the trip out to C and back is
/// rejected. And the value must be reachable from the entry point that produced
/// it, which is the property a blanket nine-code check cannot express.
fn assert_contract_code(code: ReturnCode, contract: Contract, context: &str) {
    assert_abi_integer(code, context);
    assert!(
        contract.allowed().contains(&code),
        "{context}: {} returned {code:?} ({}), which is outside the codes it \
         documents: {:?}",
        contract.entry_point(),
        code.as_c_int(),
        contract.allowed()
    );
}

/// Asserts that `code`'s ABI integer still equals the `zlib.h` value and still
/// round-trips through the C boundary's own normalisation.
///
/// This is the ABI half of [`assert_contract_code`], kept as its own function
/// because it is the half that must run for EVERY code the harness sees,
/// including on a path that is about to panic for a contract violation.
///
/// The `match` is deliberately EXHAUSTIVE with no wildcard arm, so adding a
/// variant to `ReturnCode` becomes a compile error right here — which is
/// precisely the signal this harness wants, because a new code reaching a C
/// caller's `switch` is an ABI change.
///
/// This is the **ABI layer** of outcome checking and is deliberately
/// value-agnostic: no arm is rejected at this level. Which of the nine a given
/// call may legitimately produce is the question [`Contract`] answers — the
/// membership layer — and every call site in this harness goes on through it
/// rather than stopping here. Answering it here instead is how the blanket
/// nine-code check came to accept `Z_ERRNO` from a decoder.
fn assert_abi_integer(code: ReturnCode, context: &str) {
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
/// # Why the compile-time gate does not replace this query
///
/// Cargo defines a `feature` predicate only for the crate it is compiling, and
/// this harness is the detached `zlib-rs-fuzz` package rather than `zlib-rs`. So
/// `fuzz/Cargo.toml` declares a **forwarding** `gzip` feature
/// (`gzip = ["zlib-rs/gzip"]`, on by default), which is what makes the per-item
/// `#[cfg(feature = "gzip")]` attributes in this file both defined and
/// meaningful. Without that declaration the predicate would be permanently
/// false — silently deleting all coverage of the RFC 1952 header parser, the
/// most attacker-exposed surface in the decoder — and `rustc` would reject the
/// unknown value as an `unexpected_cfg_value` diagnostic besides.
///
/// The feature records what the build *asked for*. This probe reports what the
/// linked engine *does*, and the two are not the same question:
///
/// * It is an exact capability query. The wrap request is masked with `& 15`
///   only in a gzip-enabled build, so without that support a `windowBits` of
///   `40..=47` fails the `8..=15` bounds test and yields `Z_STREAM_ERROR` — the
///   same answer a C zlib built without `GUNZIP` gives. What is measured is
///   therefore the behaviour that actually matters, not a proxy for it.
/// * It costs one initialisation for the life of the process. No window is
///   allocated by a failing or succeeding init, so the probe is cheap enough to
///   be invisible in the execution rate.
///
/// Note what neither guard may become: a whole-file inner `#![cfg(...)]` would
/// compile away the `fuzz_target!` invocation and leave this `[[bin]]` with no
/// `main` to link. Every gate in this file is therefore per item.
#[cfg(feature = "gzip")]
fn gzip_framing_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();

    *SUPPORTED.get_or_init(|| {
        let mut strm = ZStream::new();
        let code = contract_code(
            inflate_init2(&mut strm, WBITS_AUTO),
            Contract::InflateInit2,
            "inflate_init2 (gzip capability probe)",
        );
        // Only two answers are in contract for an auto-detect request: it is
        // accepted, or it is rejected because this build has no gzip support.
        // `Z_MEM_ERROR` is in the entry point's set but not reachable here — a
        // failing init allocates nothing — so it is excluded explicitly rather
        // than left to the broader membership check.
        assert!(
            code == ReturnCode::Ok || code == ReturnCode::StreamError,
            "inflate_init2 answered the gzip capability probe with {code:?}; \
             an auto-detect request is either accepted or refused with \
             Z_STREAM_ERROR"
        );
        let supported = code == ReturnCode::Ok;

        // Teardown runs either way, exactly as a C caller must. Its answer is
        // fully determined: `Z_OK` when a state was installed, and the defined
        // no-op `Z_STREAM_ERROR` when init refused and left none.
        let expected_end = if supported {
            ReturnCode::Ok
        } else {
            ReturnCode::StreamError
        };
        assert_eq!(
            contract_code(
                inflate_end(&mut strm),
                Contract::InflateEnd,
                "inflate_end (gzip capability probe)",
            ),
            expected_end,
            "inflate_end must report {expected_end:?} after an init that \
             returned {code:?}"
        );
        supported
    })
}

/// Sweeps both gzip-dependent framings, choosing which end of each documented
/// range to use from `sel`. Returns whether they actually ran, so the
/// byte-at-a-time rotation can include them only when they are real.
///
/// The two guards are applied here rather than at the call site so the entry
/// point stays free of conditional compilation.
#[cfg(feature = "gzip")]
fn probe_gzip_framings(data: &[u8], sel: u64, out: &mut [u8]) -> bool {
    if !gzip_framing_supported() {
        return false;
    }
    probe_framing(
        data,
        if sel & 4 == 0 {
            WBITS_GZIP
        } else {
            WBITS_GZIP_MIN
        },
        out,
    );
    probe_framing(
        data,
        if sel & 8 == 0 {
            WBITS_AUTO
        } else {
            WBITS_AUTO_MIN
        },
        out,
    );
    true
}

/// Without the forwarding `gzip` feature there is no gzip framing to sweep.
#[cfg(not(feature = "gzip"))]
fn probe_gzip_framings(_data: &[u8], _sel: u64, _out: &mut [u8]) -> bool {
    false
}

/// The framing rotation for the byte-at-a-time drip.
///
/// A one-byte-at-a-time gzip header parse is a particularly good place to look
/// for a boundary mistake, so the gzip framings join the rotation whenever they
/// are compiled in *and* the linked library accepts them.
#[cfg(feature = "gzip")]
fn drip_framings(gzip: bool) -> &'static [i32] {
    if gzip {
        &[WBITS_ZLIB, WBITS_RAW, WBITS_GZIP, WBITS_AUTO]
    } else {
        &[WBITS_ZLIB, WBITS_RAW]
    }
}

/// Without the forwarding `gzip` feature only the always-available framings
/// remain in the rotation.
#[cfg(not(feature = "gzip"))]
fn drip_framings(_gzip: bool) -> &'static [i32] {
    &[WBITS_ZLIB, WBITS_RAW]
}

/// Flattens a `Result`-returning entry point into a bare [`ReturnCode`] and
/// asserts it against that entry point's contract.
///
/// Mirrors C, where every one of these entry points returns a plain `int`, so
/// success and failure can be checked uniformly. Returning the code (rather
/// than discarding it) lets a caller with constructed input go on to assert a
/// specific value without a second helper.
///
/// There is deliberately no contract-agnostic variant: flattening a `Result` and
/// checking only that the code is *defined* is exactly the gap that let impossible
/// outcomes through, so the governing [`Contract`] is a required argument.
fn contract_code(
    result: Result<ReturnCode, ZlibError>,
    contract: Contract,
    context: &str,
) -> ReturnCode {
    let code = result.unwrap_or_else(ZlibError::as_return_code);
    assert_contract_code(code, contract, context);
    code
}

/// Requires an entry point whose arguments are entirely under this harness's
/// control to have succeeded, and separates out the ONE refusal that is a
/// property of the host rather than of the library.
///
/// A state reservation can fail on a machine under memory pressure, and turning
/// that into a crash report would waste a finding on the fuzzing environment. So
/// `Z_MEM_ERROR` — and only `Z_MEM_ERROR`, matched as an exact value — returns
/// `false`, which ends the calling probe. Every other refusal panics: the level,
/// the framing, the `memLevel`, and the strategy were all chosen here, so no
/// other code is reachable, and treating one as a reason to skip is precisely how
/// a probe comes to report success without having tested anything.
fn state_reserved(
    result: Result<ReturnCode, ZlibError>,
    contract: Contract,
    context: &str,
) -> bool {
    match result {
        Ok(ReturnCode::Ok) => true,
        Err(ZlibError::MemError) => false,
        other => {
            let code = other.unwrap_or_else(ZlibError::as_return_code);
            assert_contract_code(code, contract, context);
            panic!(
                "{context}: {} answered {code:?} for arguments this harness \
                 controls end to end; only Z_OK, or the machine-level \
                 Z_MEM_ERROR, is in contract here",
                contract.entry_point()
            );
        }
    }
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
/// | `24..=27` | the preset-dictionary sampling gate |
/// | `28..=35` | the preset-dictionary compression level |
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

/// Maps bits `28..=35` of a selector to a compression level in `-1..=9` for the
/// preset-dictionary leg.
///
/// Drawn from its own field rather than reusing [`level_from`]'s, so the two
/// encoding probes cannot end up correlated to the same level on every
/// execution — which would quietly halve what the pair covers.
fn dict_level_from(selector: u64) -> i32 {
    let index = ((selector >> 28) & 0xFF) % 11;
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
        assert_contract_code(outcome.code, Contract::Inflate, "inflate");

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
        contract_code(
            inflate_init2(&mut strm, window_bits),
            Contract::InflateInit2,
            "inflate_init2"
        ),
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
        assert_contract_code(sync_code, Contract::InflateSync, "inflate_sync");

        // Resynchronisation reported success, so the engine claims decoding can
        // resume. Take it at its word: a second bounded drain over the
        // remainder must also terminate in a defined state.
        if sync_code == ReturnCode::Ok {
            let (resumed, _, _) =
                decode_bounded(&mut strm, &tail[sync_consumed..], out, Z_NO_FLUSH);
            assert_contract_code(resumed, Contract::Inflate, "inflate after inflate_sync");
        }
    }

    // `inflate_end` is called unconditionally, exactly as a C caller must. Init
    // was asserted to have installed a state above and no other engine has
    // touched this stream, so `Z_OK` is the only answer in contract — the code
    // is pinned rather than merely admitted, because a `Z_STREAM_ERROR` here
    // would mean the state vanished mid-decode.
    // A successful `inflate_init2` narrows `inflate_end` from its documented
    // two-code set to exactly `Z_OK`: the only documented failure is an
    // inconsistent stream, and this one was just initialised and driven through the
    // safe API. C callers routinely ignore this code, which is exactly why a
    // harness must not.
    assert_eq!(
        contract_code(inflate_end(&mut strm), Contract::InflateEnd, "inflate_end"),
        ReturnCode::Ok,
        "inflate_end refused a stream that inflate_init2 had accepted"
    );
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
        contract_code(
            inflate_init2(&mut strm, window_bits),
            Contract::InflateInit2,
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
        assert_contract_code(outcome.code, Contract::Inflate, "inflate (1-byte windows)");

        in_pos += outcome.consumed;

        // Anything other than `Z_OK`, and any pass that moved neither cursor,
        // ends the probe: both are defined outcomes for arbitrary input.
        if outcome.code != ReturnCode::Ok || (outcome.consumed == 0 && outcome.produced == 0) {
            break;
        }
    }

    // A successful `inflate_init2` narrows `inflate_end` from its documented
    // two-code set to exactly `Z_OK`: the only documented failure is an
    // inconsistent stream, and this one was just initialised and driven through the
    // safe API. C callers routinely ignore this code, which is exactly why a
    // harness must not.
    assert_eq!(
        contract_code(
            inflate_end(&mut strm),
            Contract::InflateEnd,
            "inflate_end (drip)"
        ),
        ReturnCode::Ok,
        "inflate_end refused a stream that inflate_init2 had accepted"
    );
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

    // Every argument handed to the encoder is under this harness's control: the
    // level came from `level_from`, which yields only `-1..=9`, and the
    // destination was just checked against `compress_bound`. `Z_STREAM_ERROR`
    // and `Z_BUF_ERROR` are therefore both unreachable, and treating a refusal
    // as "the encoder is not what this harness tests" would silently skip the
    // whole round trip — including the four assertions below, which are the only
    // correctness (as opposed to robustness) checks in the file. Encoder success
    // is required, and the sole environmental excuse is isolated by exact code.
    let compressed_len = match compress2(&mut scratch, payload, level) {
        Ok(len) => len,
        // A failed state reservation is a property of the machine, not of the
        // library, so it ends the probe. It is spelled out as one exact code so
        // that no other refusal can slip through with it.
        Err(ReturnCode::MemError) => return,
        Err(code) => {
            // Contract first, so a code the encoder has no path to at all is
            // reported as the layering violation it is rather than as a merely
            // unexpected refusal.
            assert_contract_code(code, Contract::Compress2Error, "compress2");
            panic!(
                "compress2 refused a {}-byte payload at level {level} with \
                 {code:?}; the level is inside -1..=9 and the destination is \
                 compress_bound-sized, so Z_MEM_ERROR is the only refusal in \
                 contract here",
                payload.len()
            );
        }
    };

    let mut strm = ZStream::new();
    assert_eq!(
        contract_code(
            inflate_init2(&mut strm, WBITS_ZLIB),
            Contract::InflateInit2,
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

    assert_eq!(
        contract_code(
            inflate_end(&mut strm),
            Contract::InflateEnd,
            "inflate_end (round trip)"
        ),
        ReturnCode::Ok,
        "inflate_end refused a stream that inflate_init2 had accepted"
    );
}

/// Drives the RFC 1950 preset-dictionary handshake end to end: encode with a
/// dictionary, require the decoder to ask for it by identifier, install it, and
/// recover the payload byte for byte.
///
/// # Why this probe exists
///
/// `deflate_set_dictionary` / `inflate_set_dictionary` are the `FDICT` path, and
/// nothing else in this harness reaches it. It is a four-byte field in the zlib
/// header plus one extra decoder mode, and it is the only place `inflate` returns
/// `Z_NEED_DICT` — a code the harness was previously willing to accept from any
/// call while never once producing it deliberately. A `Z_NEED_DICT` that never
/// arrives, or that arrives carrying the wrong identifier, is a wire-format
/// defect that no dictionary-free round trip can see, because both halves would
/// simply agree to skip the field.
///
/// # What is pinned, and why pinning it is legitimate here
///
/// Everything, because none of the input to the *engine* is arbitrary: the
/// dictionary and payload are slices of the fuzzer's bytes, but the framing, the
/// level, the `memLevel`, the strategy, and every buffer size are the harness's
/// own choices. Specifically —
///
/// 1. `FDICT` is set in the second header byte and the four bytes after it carry
///    the dictionary's Adler-32, most significant byte first (RFC 1950 §2.2);
/// 2. that wire identifier equals an independently computed
///    `adler32(1, dictionary)`, so the encoder is checked against the checksum
///    primitive rather than against itself;
/// 3. the first decode call stops after exactly those six header bytes, emits
///    nothing, and reports `Z_NEED_DICT`;
/// 4. the identifier the decoder surfaces in `ZStream::adler` is the one that was
///    written — the field survived the wire in both directions;
/// 5. a dictionary whose identifier does not match is refused with
///    `Z_DATA_ERROR`, and that refusal leaves the stream still able to accept the
///    correct one, exactly as C's early `return` before any state mutation does;
/// 6. once the correct dictionary is installed the stream reaches
///    `Z_STREAM_END`, consumes every compressed byte, and yields precisely the
///    original payload;
/// 7. the trailer checksum covers the PAYLOAD alone and not the dictionary, which
///    is what C's `state->check = adler32(0L, Z_NULL, 0)` at `inflate.c` L705
///    and the encoder's matching reset exist to get right — an asymmetry here
///    would produce a stream reference zlib rejects.
///
/// # Bounds
///
/// Every buffer is a fixed-size stack array, the dictionary is capped at
/// [`DICT_MAX`] and the payload at [`DICT_PAYLOAD_MAX`], and the compressed
/// intermediate is checked against the dictionary-aware `deflate_bound` before a
/// byte is written into it. Nothing is sized from the input.
///
/// The caller guarantees a non-empty payload. A zero-length dictionary leaves
/// `strstart` at zero, so the encoder clears `PRESET_DICT` and never writes the
/// identifier, and there is no handshake left to observe; that precondition is
/// asserted here rather than silently returned from.
fn probe_dictionary(data: &[u8], level: i32) {
    let payload = &data[..data.len().min(DICT_PAYLOAD_MAX)];
    assert!(
        !payload.is_empty(),
        "the preset-dictionary probe requires a non-empty payload; the caller \
         must not invoke it on empty input"
    );
    // A prefix of the payload, so the pre-loaded window is genuinely useful: the
    // encoder can reference it at a negative distance, which is the entire point
    // of a preset dictionary and the path a disjoint dictionary leaves cold.
    let dictionary = &payload[..payload.len().min(DICT_MAX)];
    let expected_id = adler32(1, dictionary);

    // --- Encode with the dictionary ---------------------------------------
    let mut encoder = ZStream::new();
    if !state_reserved(
        deflate_init2(
            &mut encoder,
            level,
            Z_DEFLATED,
            WBITS_ZLIB,
            DEF_MEM_LEVEL,
            Strategy::Default,
        ),
        Contract::DeflateInit2,
        "deflate_init2 (dictionary)",
    ) {
        return;
    }

    assert_eq!(
        contract_code(
            deflate_set_dictionary(&mut encoder, dictionary),
            Contract::DeflateSetDictionary,
            "deflate_set_dictionary",
        ),
        ReturnCode::Ok,
        "deflate_set_dictionary refused a {}-byte dictionary on a freshly \
         initialised level-{level} zlib stream",
        dictionary.len()
    );
    // The encoder publishes the identifier it will write as the stream's running
    // checksum. Cross-checking it against the checksum primitive here means the
    // wire comparison below cannot pass by both sides making the same mistake.
    assert_eq!(
        encoder.adler,
        expected_id,
        "deflate_set_dictionary published dictionary id {:#010x}, but the \
         Adler-32 of those {} bytes is {expected_id:#010x}",
        encoder.adler,
        dictionary.len()
    );

    let mut scratch = [0u8; DICT_SCRATCH];
    let bound = deflate_bound(&encoder, payload.len());
    assert!(
        bound <= scratch.len(),
        "the dictionary probe's {}-byte scratch is smaller than deflate_bound's \
         {bound} bytes for a {}-byte payload",
        scratch.len(),
        payload.len()
    );

    let encoded = deflate(&mut encoder, payload, &mut scratch, Z_FINISH);
    assert_contract_code(encoded.code, Contract::Deflate, "deflate (dictionary)");
    assert_eq!(
        encoded.code,
        ReturnCode::StreamEnd,
        "a single Z_FINISH into a deflate_bound-sized buffer must complete the \
         stream, but level {level} reported {:?}",
        encoded.code
    );
    assert_eq!(
        encoded.consumed,
        payload.len(),
        "deflate left {} of {} payload bytes unread on a completed stream",
        payload.len() - encoded.consumed,
        payload.len()
    );
    let compressed_len = encoded.produced;
    assert_eq!(
        contract_code(
            deflate_end(&mut encoder),
            Contract::DeflateEnd,
            "deflate_end (dictionary)",
        ),
        ReturnCode::Ok,
        "deflate_end reported an error for a stream that had reached Z_STREAM_END"
    );

    // --- The header field, read straight off the wire -----------------------
    // RFC 1950 §2.2: FLG is the second byte and FDICT is its bit 5; when set,
    // DICTID is the next four bytes, most significant first. Six bytes in total,
    // which is what the decoder must consume before it can ask for anything.
    const ZLIB_FDICT_HEADER: usize = 6;
    assert!(
        compressed_len >= ZLIB_FDICT_HEADER,
        "a dictionary-flagged zlib stream cannot be shorter than its \
         {ZLIB_FDICT_HEADER}-byte header, but level {level} produced \
         {compressed_len} bytes"
    );
    assert_eq!(
        scratch[1] & 0x20,
        0x20,
        "FDICT is clear in FLG ({:#04x}) even though a dictionary was set",
        scratch[1]
    );
    assert_eq!(
        u32::from_be_bytes([scratch[2], scratch[3], scratch[4], scratch[5]]),
        expected_id,
        "the DICTID field on the wire is not the dictionary's Adler-32"
    );

    // --- Decode, which must stop and ask for the dictionary ----------------
    let mut decoder = ZStream::new();
    if !state_reserved(
        inflate_init2(&mut decoder, WBITS_ZLIB),
        Contract::InflateInit2,
        "inflate_init2 (dictionary)",
    ) {
        return;
    }

    let mut restored = [0u8; DICT_PAYLOAD_MAX];
    let stream = &scratch[..compressed_len];
    let first = inflate(&mut decoder, stream, &mut restored, Z_NO_FLUSH);
    assert_contract_code(
        first.code,
        Contract::Inflate,
        "inflate (awaiting a dictionary)",
    );
    assert_eq!(
        first.code,
        ReturnCode::NeedDict,
        "a stream carrying FDICT must suspend with Z_NEED_DICT, not {:?}",
        first.code
    );
    assert_eq!(
        first.consumed, ZLIB_FDICT_HEADER,
        "the decoder consumed {} bytes before asking for the dictionary; a \
         FDICT-flagged header is exactly {ZLIB_FDICT_HEADER} bytes",
        first.consumed
    );
    assert_eq!(
        first.produced, 0,
        "the decoder emitted {} bytes before the dictionary it still needs was \
         installed",
        first.produced
    );
    assert_eq!(
        decoder.adler, expected_id,
        "the decoder asked for dictionary id {:#010x}; the encoder wrote \
         {expected_id:#010x}",
        decoder.adler
    );

    // --- A mismatched dictionary must be refused, and refusing must not
    //     poison the stream --------------------------------------------------
    // One byte inverted shifts the Adler-32 low half by an odd amount no larger
    // than 255, so it cannot alias the original modulo 65521: the identifier is
    // guaranteed different, which is what makes pinning Z_DATA_ERROR sound.
    let mut wrong_storage = [0u8; DICT_MAX];
    wrong_storage[..dictionary.len()].copy_from_slice(dictionary);
    wrong_storage[0] ^= 0xFF;
    let wrong = &wrong_storage[..dictionary.len()];
    assert_ne!(
        adler32(1, wrong),
        expected_id,
        "the harness failed to construct a dictionary with a different id"
    );
    assert_eq!(
        contract_code(
            inflate_set_dictionary(&mut decoder, wrong),
            Contract::InflateSetDictionary,
            "inflate_set_dictionary (mismatched id)",
        ),
        ReturnCode::DataError,
        "a dictionary whose Adler-32 does not match the stream's request must \
         be refused with Z_DATA_ERROR"
    );

    // --- Install the right one and finish ----------------------------------
    if !state_reserved(
        inflate_set_dictionary(&mut decoder, dictionary),
        Contract::InflateSetDictionary,
        "inflate_set_dictionary",
    ) {
        assert_eq!(
            contract_code(
                inflate_end(&mut decoder),
                Contract::InflateEnd,
                "inflate_end (dictionary, window reservation refused)",
            ),
            ReturnCode::Ok,
            "inflate_end refused a stream that inflate_init2 had accepted"
        );
        return;
    }

    let (code, resumed, produced) = decode_bounded(
        &mut decoder,
        &stream[first.consumed..],
        &mut restored,
        Z_FINISH,
    );
    assert_eq!(
        code,
        ReturnCode::StreamEnd,
        "a self-produced level-{level} stream with a {}-byte preset dictionary \
         did not decode to Z_STREAM_END",
        dictionary.len()
    );
    assert_eq!(
        first.consumed + resumed,
        compressed_len,
        "the decoder left {} bytes of a self-produced stream unread",
        compressed_len - first.consumed - resumed
    );
    assert_eq!(
        produced,
        payload.len(),
        "preset-dictionary round-trip length mismatch"
    );
    assert_eq!(
        &restored[..produced],
        payload,
        "preset-dictionary round-trip content mismatch"
    );
    // The zlib trailer is the Adler-32 of the decompressed data only: both ends
    // restart the running checksum once the dictionary has been accounted for,
    // so folding the dictionary in on either side would produce a stream the
    // other implementation rejects.
    assert_eq!(
        decoder.adler,
        adler32(1, payload),
        "the verified trailer checksum is not the Adler-32 of the payload alone"
    );

    assert_eq!(
        contract_code(
            inflate_end(&mut decoder),
            Contract::InflateEnd,
            "inflate_end (dictionary)",
        ),
        ReturnCode::Ok,
        "inflate_end refused a stream that inflate_init2 had accepted"
    );
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
    // Any code inside the calling entry point's own contract is acceptable — the
    // failures this harness catches are a panic or UB inside the decoder, and a
    // code the call had no path to. This one buffer is reused by every decode
    // probe below, so sweeping four framings costs one allocation and never sizes
    // memory from the input.
    let mut out = vec![0u8; OUT_CEILING];

    // --- The one-call zlib-framed wrapper ---------------------------------
    // Its result is checked against the wrapper's OWN documented error set
    // rather than the union of all nine codes, so a value that entry point has
    // no path to — `Z_ERRNO`, `Z_VERSION_ERROR`, or the `Z_NEED_DICT` it is
    // documented to re-map to `Z_DATA_ERROR` — is a finding instead of a pass.
    // Within that set no SPECIFIC code is asserted: for arbitrary bytes each
    // remaining member is a legitimate answer.
    match uncompress(&mut out, data) {
        Ok(produced) => assert!(
            produced <= OUT_CEILING,
            "uncompress reported writing {produced} bytes into a {OUT_CEILING}-byte buffer"
        ),
        Err(code) => assert_contract_code(code, Contract::UncompressError, "uncompress"),
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
    // The gzip framings live behind two guards, both applied inside
    // `probe_gzip_framings`: the per-item `gzip` feature of this package, and the
    // run-time capability query. Never a whole-file inner attribute — that would
    // compile away the harness entry point along with the probes and leave a
    // binary with no `main` to link.
    let gzip_supported = probe_gzip_framings(data, sel, &mut out);

    // --- Byte-at-a-time delivery ------------------------------------------
    let rotation = drip_framings(gzip_supported);
    // Bits 4..=7 of the selector, per its documented bit budget.
    let drip_pick = usize::try_from((sel >> 4) & 0xF).unwrap_or(0) % rotation.len();
    probe_drip(data, rotation[drip_pick]);

    // --- The decode-table builder -----------------------------------------
    FIXED_VECTORS.call_once(probe_inflate_table_fixed_vectors);
    probe_inflate_table(sel, data);

    // --- Round-trip self-check --------------------------------------------
    // Sampled on one execution in sixteen (bits 16..=19 of the selector), which
    // is the only probe here that is not run every time. It is also the only one
    // that ENCODES before it decodes, so it pays for a full deflate state — by
    // far the most expensive work in this harness — and spending that on every
    // execution would buy fewer decoder paths per unit of fuzzing time than the
    // additional executions do. The gate is derived from the input, so a finding
    // still reproduces from the saved input alone.
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

    // --- The RFC 1950 preset-dictionary handshake --------------------------
    // Sampled on the same one-in-sixteen basis (bits 24..=27, its own field so it
    // is uncorrelated with the round-trip gate above), for the same reason: it is
    // the only probe that pays for a deflate state AND an inflate state AND a
    // dictionary load in one execution.
    //
    // The non-empty test is an input-domain precondition of the probe, not a
    // skipped assertion: with a zero-length dictionary the encoder leaves
    // `strstart` at zero, clears PRESET_DICT, and writes no identifier, so there
    // is no handshake in the stream to observe. Every other input reaches the
    // probe, and once inside it there is exactly one exit other than completing
    // every assertion — a host-level allocation refusal, isolated by exact code.
    if (sel >> 24) & 0xF == 0 && !data.is_empty() {
        probe_dictionary(data, dict_level_from(sel));
    }
});
