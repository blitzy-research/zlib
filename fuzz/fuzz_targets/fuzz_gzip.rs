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
//! The crate's `ffi` module is its C ABI boundary and the home of the crate's
//! `unsafe` operations, alongside the private freestanding `no_std_support`
//! runtime block in `src/lib.rs`; the FFI symbols are deliberately not
//! re-exported at the crate root so that boundary stands alone. Reaching it from
//! here would drag a C-ABI dependency — and the `unsafe` blocks that come with it
//! — into a harness that has no business on that side of the boundary. So this
//! target drives the same decoder through the ordinary checked Rust surface that
//! real consumers use, which is also what makes its findings trustworthy: nothing
//! it reports can be an artefact of the harness reaching around the type system.
//! Exercising the C ABI is `fuzz_ffi_roundtrip`'s job, not this one's. (User
//! constraint 3 — "zero unsafe blocks in core compression logic" — and AAP
//! §0.7.2 plan-adopted standard S2, containment by construction.)
//!
//! # How the gzip paths are gated
//!
//! gzip (`24..=31`) and auto-detect (`40..=47`) `windowBits` are only accepted
//! when `zlib-rs` is built with its `gzip` feature: `inflate_reset2` masks the
//! request with `& 15` only when that feature is on, and without it the value
//! fails the `8..=15` bounds test — matching C built without `GUNZIP`.
//!
//! Cargo defines a `feature` predicate only for the crate it is compiling, and
//! that crate here is the detached `zlib-rs-fuzz` package, not `zlib-rs`. So a
//! bare `cfg(feature = "gzip")` would be undefined in this file — permanently
//! false, and rejected outright by `rustc`'s `unexpected_cfgs` lint, which the
//! folder's blocking `clippy --all-targets -- -D warnings` gate turns into an
//! error. `fuzz/Cargo.toml` therefore declares a **forwarding** `gzip` feature
//! (`gzip = ["zlib-rs/gzip"]`, enabled by default) so the predicate is both
//! defined and truthful: whenever a gated item compiles, the library capability
//! it depends on is guaranteed enabled rather than merely inherited from
//! `zlib-rs`'s own defaults.
//!
//! The gate is applied **per item** — on the gzip `windowBits` selectors, the
//! RFC 1952 layout constants, the member synthesizers, and the gzip coverage
//! legs — never as a whole-file inner attribute.
//!
//! Capability is settled **once, up front**, and kept strictly separate from the
//! outcome of any individual call. That separation is the whole point. Letting
//! each [`drive`] call treat "init refused" as "gzip must be compiled out"
//! conflates a build-configuration fact with a run-time result, and the
//! conflation is silent in both directions: a genuine defect that made
//! `inflate_init2` reject a framing it must accept would be read as a missing
//! feature and skipped, and so would a transient allocation failure. So from then
//! on every init is REQUIRED to succeed for the framings the build supports.
//!
//! The capability is read from the library, from the one place it is actually
//! published: [`gzip_supported`] tests bit 17 (`NO_GZIP`) of
//! [`zlib_rs::zlib_compile_flags`], which the library sets exactly when its
//! `gzip` feature is off. That is a *derivation*, not an inference — and
//! [`assert_capability_agrees`] then requires the engine to behave the way the
//! word advertises, so an `inflate_init2` that rejects gzip framing in a build
//! whose flags claim gzip, or accepts it in one that does not, is itself a
//! finding rather than a silently skipped probe. Inferring the capability from
//! "did `inflate_init2` happen to fail" cannot tell those two situations apart, so
//! a genuine regression in the gzip path would disable the very coverage that would
//! have caught it.
//!
//! `inflate_get_header` is reached the same way. It is feature-gated *inside* the
//! library, so it is named only from items carrying the same per-item gate — which
//! is precisely why the gate is per item rather than per file.
//!
//! That keeps the mandatory libFuzzer entry point — and therefore `main` —
//! unconditional, so this `[[bin]]` links under every feature combination, while
//! the gzip coverage survives in a default build.
//!
//! # Coverage
//!
//! Every RFC 1952 phase is reached from fuzzer-chosen bytes: the fixed header
//! (magic, CM, FLG, MTIME, XFL, OS), the optional EXTRA / NAME / COMMENT fields,
//! the optional header CRC, and the little-endian CRC-32 + ISIZE trailer — plus
//! truncated and corrupted variants of each. They are *reachable* because the
//! synthesizers below build well-formed prefixes around the fuzzer's bytes instead
//! of relying on arbitrary input to stumble into a valid header; which phases a
//! given execution actually enters still depends on the input.
//!
//! Two accept-path anchors sit opposite all of that, because a harness made only
//! of rejections proves nothing about the code that accepts:
//!
//! * [`valid_member`] pins the **header** accept path with an empty payload; and
//! * [`probe_valid_member_with_metadata`] pins the **payload** accept path with a
//!   non-empty member produced by the crate's own gzip encoder, asserting the
//!   decoded bytes, the wire-level CRC-32 and ISIZE, and the metadata read back
//!   through `inflateGetHeader`. An empty payload cannot do that: the CRC-32 of
//!   nothing is the initial value, so a check that is never reached is
//!   indistinguishable from one that passes.
//!
//! A third probe, [`probe_gz_lifecycle`], is **sampled** rather than run on every
//! input: it drives the safe `gz*` file API end to end (`gzopen` / `gzwrite` /
//! `gzclose_w`, then `gzopen` / `gzread` / `gzclose_r`) on a temp path built
//! entirely from non-input components, which is the buffered layer real consumers
//! use and the one place the deferred finish behind `gzclose_w` is exercised.
use libfuzzer_sys::fuzz_target;
use zlib_rs::constants::Z_NO_FLUSH;
use zlib_rs::inflate::{inflate, inflate_end, inflate_init2};
use zlib_rs::{ReturnCode, ZStream};

// Everything below belongs to the gzip legs rather than to the harness as a
// whole, so each import carries the same per-item gate the legs do. `gz-io` is
// not named: the path dependency in `fuzz/Cargo.toml` keeps `zlib-rs`'s own
// default features, so the library items these reach are always compiled, and
// gating on the forwarding `gzip` feature is what keeps the reduced-feature row
// free of unused-import diagnostics.
#[cfg(feature = "gzip")]
use zlib_rs::constants::{DEF_MEM_LEVEL, Z_DEFLATED, Z_FINISH};
#[cfg(feature = "gzip")]
use zlib_rs::deflate::{deflate, deflate_bound, deflate_end, deflate_init2, deflate_set_header};
#[cfg(feature = "gzip")]
use zlib_rs::gz::{gzclose_r, gzclose_w, gzopen, gzread, gzwrite};
#[cfg(feature = "gzip")]
use zlib_rs::inflate::{inflate_get_header, inflate_header};
// `ZlibError` and `zlib_compile_flags` are needed by `contract_code` and
// `gzip_supported`, both of which the UNGATED `drive`/`sweep` path calls, so they
// are imported unconditionally. Both live in layers the library declares without a
// feature gate (`error` and `util`), so this is available in every build.
#[cfg(feature = "gzip")]
use zlib_rs::{GzHeader, Strategy, crc32};
use zlib_rs::{ZlibError, zlib_compile_flags};

// ===========================================================================
// windowBits framing selectors and work bounds
//
// These are ABI / wire-format constants: `constants::parse_window_bits` decodes
// the overloaded `windowBits` value, and altering any of them would change what
// this harness actually tests (AAP §0.8.1 directive D-2 — observe, never
// "correct").
// ===========================================================================

/// Auto-detect a zlib *or* gzip wrapper: windowBits 47 = 32 (auto-detect
/// header) + 15 (max window). This is the harness's primary framing: it is the
/// one that accepts genuinely arbitrary bytes under either wrapper.
#[cfg(feature = "gzip")]
const WBITS_AUTO: i32 = 47;

/// gzip wrapper only (RFC 1952): `16 + 15`. Reaching the header parser without
/// the auto-detect probe in front of it is a distinct path through
/// `inflate_reset2`.
#[cfg(feature = "gzip")]
const WBITS_GZIP: i32 = 31;

/// zlib wrapper only (RFC 1950). Always available — never gated on `gzip`.
const WBITS_ZLIB: i32 = 15;

/// Ceiling on the candidate names [`TempGzWorkspace::new`] will try.
///
/// The loop only ever advances to the next name — it never deletes an occupied
/// candidate — so it needs a bound, and this one is generous: each candidate
/// already carries the process id, a monotonic counter and a sanitized clone
/// index, so a collision means another process is actively planting names rather
/// than that the name space is crowded.
#[cfg(feature = "gzip")]
const TEMP_DIR_ATTEMPTS: u32 = 64;

/// Raw DEFLATE (RFC 1951): no wrapper, no checksum. Always available.
const WBITS_RAW: i32 = -15;

/// The fixed output window, re-presented to the decoder on every pass. A fixed
/// ceiling (rather than a buffer sized from the input) is what stops a
/// decompression-bomb input from dominating the run, and it means no allocation
/// is ever sized from a fuzzer-supplied length.
const OUT_WINDOW: usize = 4096;

/// Maximum number of drain-loop passes per stream. See [`drive`].
const WORK_BUDGET: u32 = 4096;

/// The most output one [`drive`] call can ever report.
///
/// A pass writes at most one [`OUT_WINDOW`], and [`WORK_BUDGET`] caps the passes
/// — compared *after* the increment, so one further pass runs before the break.
/// Asserting against this in [`sweep`] is what turns the bounded-work design
/// from a claim in a comment into a checked invariant.
const MAX_PRODUCED: usize = (WORK_BUDGET as usize + 1) * OUT_WINDOW;

// --- RFC 1952 §2.3.1 member layout -----------------------------------------
//
// Every constant in this block describes the gzip member layout and is reached
// only from the gzip legs, so each is gated on the forwarding `gzip` feature.

/// gzip magic byte 1 (`ID1`), fixed at 31 (`0x1f`).
#[cfg(feature = "gzip")]
const GZIP_ID1: u8 = 0x1f;
/// gzip magic byte 2 (`ID2`), fixed at 139 (`0x8b`).
#[cfg(feature = "gzip")]
const GZIP_ID2: u8 = 0x8b;
/// `CM` (compression method) 8 denotes DEFLATE — the only method gzip defines.
#[cfg(feature = "gzip")]
const CM_DEFLATE: u8 = 8;
/// `FLG` bit 1: a CRC-16 of the header follows the optional fields.
#[cfg(feature = "gzip")]
const FHCRC: u8 = 0x02;
/// `FLG` bit 2: an `XLEN`-prefixed extra field is present.
#[cfg(feature = "gzip")]
const FEXTRA: u8 = 0x04;
/// `FLG` bit 3: a NUL-terminated original file name is present.
#[cfg(feature = "gzip")]
const FNAME: u8 = 0x08;
/// `FLG` bit 4: a NUL-terminated file comment is present.
#[cfg(feature = "gzip")]
const FCOMMENT: u8 = 0x10;
/// The five `FLG` bits RFC 1952 defines (`FTEXT | FHCRC | FEXTRA | FNAME |
/// FCOMMENT`). Bits 5-7 are reserved and a conforming decoder must reject a
/// header that sets them; that rejection is covered by the unmodified-input
/// cases, so the synthesized headers mask down to the defined bits in order to
/// reach the *deeper* phases instead of being turned away at byte 3.
#[cfg(feature = "gzip")]
const FLG_DEFINED: u8 = 0x1f;

/// The canonical two-byte empty DEFLATE stream: `BFINAL = 1`, `BTYPE = 01`
/// (fixed Huffman), followed by the 7-zero-bit end-of-block symbol.
#[cfg(feature = "gzip")]
const EMPTY_DEFLATE: [u8; 2] = [0x03, 0x00];

/// Upper bound on a synthesized EXTRA / NAME / COMMENT field, in bytes.
///
/// Every field length is drawn from a *single* fuzz byte, so it is inherently
/// bounded by this value: steerable by the fuzzer, yet incapable of asking for
/// more memory than the (libFuzzer-capped) input already occupies. Nothing here
/// is ever sized from an unbounded fuzzer-supplied number.
#[cfg(feature = "gzip")]
const MAX_FIELD: usize = u8::MAX as usize;

/// Upper bound on each header field the non-empty accept-path anchor installs.
///
/// Kept well below [`MAX_FIELD`] so the anchor stays cheap enough to run on every
/// iteration; the negative synthesizers above are where field-length extremes are
/// explored.
#[cfg(feature = "gzip")]
const MAX_ANCHOR_FIELD: usize = 48;

/// Upper bound on the anchor's payload. Small enough that a full encode plus
/// decode plus metadata comparison does not dominate `exec/s`, large enough that
/// the payload spans several DEFLATE symbols rather than a single literal.
#[cfg(feature = "gzip")]
const MAX_ANCHOR_PAYLOAD: usize = 256;

/// Filler written into the anchor's decode buffer, so a short write cannot pass a
/// content comparison by accident.
#[cfg(feature = "gzip")]
const POISON: u8 = 0xA5;

/// One in this many inputs runs the `gz*` file-I/O lifecycle probe.
///
/// File I/O is orders of magnitude slower than an in-memory decode, so it is
/// sampled rather than run every iteration: the paths it covers
/// (`gzopen`/`gzwrite`/`gzclose_w`/`gzread`/`gzclose_r`) are deterministic given
/// the payload, so density buys nothing while cost is paid on every input.
#[cfg(feature = "gzip")]
const GZ_LIFECYCLE_SAMPLE: u8 = 64;

// ===========================================================================
// Harness plumbing
// ===========================================================================

/// How the drain loop decides it is finished.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Drain {
    /// Stop as soon as the input has been fully consumed — reference zlib's
    /// `strm.avail_in == 0` test. This bounds the work a decompression bomb can
    /// demand, because output is only ever pulled while input remains.
    UntilInputDrained,
    /// Keep pumping until a terminal code or a stall. Used only for the
    /// deterministic well-formed gzip member, which must be driven all the way
    /// to `Z_STREAM_END` for its accept-path assertion to mean anything — hence
    /// the gate, matching the leg that is its only constructor.
    #[cfg(feature = "gzip")]
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
///
/// The member synthesizers are its only users, so it is gated with them.
#[cfg(feature = "gzip")]
struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

#[cfg(feature = "gzip")]
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

/// Asserts `code` still carries the `zlib.h` ABI integer its variant mirrors.
///
/// This is the **ABI layer** of outcome checking and is deliberately *not* an
/// assertion about which code a given input deserves — that is [`Contract`]'s job,
/// and every call site in this harness goes through
/// [`assert_contract_code`]/[`contract_code`] rather than stopping here. What this
/// function pins is the shape of the answer, on two independent axes:
///
/// * the exhaustive `match` fails at **compile time** if the library ever grows
///   a tenth variant, forcing this harness to be taught about it; and
/// * the round-trip through the ABI integer fails at **run time** if a
///   discriminant ever stops agreeing with the `zlib.h` value it mirrors.
fn assert_abi_integer(code: ReturnCode) {
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

/// The entry points this harness calls, each carrying the exact set of codes its
/// own documentation permits.
///
/// Accepting all nine codes everywhere would let a `Z_VERSION_ERROR` out of
/// `inflate`, or a `Z_NEED_DICT` out of `inflate_end`, pass unremarked even though
/// neither entry point can produce them and either would be a serious ABI
/// regression. Each set below was read from the
/// `# Errors` section of the function it names, so this is a contract check
/// rather than a guess, and it stays orthogonal to the "arbitrary bytes may be
/// rejected" rule: the sets say which answers are *possible*, never which answer
/// a given input deserves.
#[derive(Copy, Clone)]
enum Contract {
    /// `inflate_init2`: accepts the framing, refuses it, or cannot allocate.
    InflateInit2,
    /// `inflate`: the full decoder surface, including the `Z_NEED_DICT` a zlib
    /// stream with `FDICT` set legitimately produces under the zlib framing.
    Inflate,
    /// `inflate_end`: succeeds, or reports that there was no state to end.
    InflateEnd,
    /// `inflate_get_header`: succeeds, or refuses a stream that is not
    /// gzip-capable.
    ///
    /// Gated with the legs that construct it: only the gzip probes call the
    /// header and encoder entry points, so in a `--no-default-features` build
    /// these variants would be unconstructed and the folder's blocking
    /// `clippy -- -D warnings` gate would reject them.
    #[cfg(feature = "gzip")]
    InflateGetHeader,
    /// `deflate_init2`: as `inflate_init2`.
    #[cfg(feature = "gzip")]
    DeflateInit2,
    /// `deflate_set_header`: succeeds, or refuses a non-gzip stream.
    #[cfg(feature = "gzip")]
    DeflateSetHeader,
    /// `deflate`: the encoder surface. It has no data-error path, because the
    /// input is bytes rather than a stream to be validated.
    #[cfg(feature = "gzip")]
    Deflate,
    /// `deflate_end`: succeeds, refuses a stateless stream, or reports
    /// `Z_DATA_ERROR` for a stream torn down while still busy.
    #[cfg(feature = "gzip")]
    DeflateEnd,
}

impl Contract {
    /// The C entry point, for panic messages.
    fn entry_point(self) -> &'static str {
        match self {
            Contract::InflateInit2 => "inflate_init2",
            Contract::Inflate => "inflate",
            Contract::InflateEnd => "inflate_end",
            #[cfg(feature = "gzip")]
            Contract::InflateGetHeader => "inflate_get_header",
            #[cfg(feature = "gzip")]
            Contract::DeflateInit2 => "deflate_init2",
            #[cfg(feature = "gzip")]
            Contract::DeflateSetHeader => "deflate_set_header",
            #[cfg(feature = "gzip")]
            Contract::Deflate => "deflate",
            #[cfg(feature = "gzip")]
            Contract::DeflateEnd => "deflate_end",
        }
    }

    /// The codes this entry point documents. Nothing outside the set is a
    /// legitimate answer, whatever the input.
    fn allowed(self) -> &'static [ReturnCode] {
        // `inflate_init2` and `deflate_init2` share a set, as do
        // `inflate_get_header` and `deflate_set_header`, but the arms are written
        // out separately rather than as or-patterns: an attribute cannot sit on one
        // alternative of an or-pattern, and the encoder-side variants only exist in
        // a `gzip` build.
        match self {
            Contract::InflateInit2 => &[
                ReturnCode::Ok,
                ReturnCode::StreamError,
                ReturnCode::MemError,
            ],
            #[cfg(feature = "gzip")]
            Contract::DeflateInit2 => &[
                ReturnCode::Ok,
                ReturnCode::StreamError,
                ReturnCode::MemError,
            ],
            Contract::Inflate => &[
                ReturnCode::Ok,
                ReturnCode::StreamEnd,
                ReturnCode::NeedDict,
                ReturnCode::BufError,
                ReturnCode::DataError,
                ReturnCode::MemError,
                ReturnCode::StreamError,
            ],
            Contract::InflateEnd => &[ReturnCode::Ok, ReturnCode::StreamError],
            #[cfg(feature = "gzip")]
            Contract::InflateGetHeader => &[ReturnCode::Ok, ReturnCode::StreamError],
            #[cfg(feature = "gzip")]
            Contract::DeflateSetHeader => &[ReturnCode::Ok, ReturnCode::StreamError],
            #[cfg(feature = "gzip")]
            Contract::Deflate => &[
                ReturnCode::Ok,
                ReturnCode::StreamEnd,
                ReturnCode::BufError,
                ReturnCode::StreamError,
            ],
            #[cfg(feature = "gzip")]
            Contract::DeflateEnd => &[
                ReturnCode::Ok,
                ReturnCode::StreamError,
                ReturnCode::DataError,
            ],
        }
    }
}

/// Asserts `code` is inside `contract`'s documented set, after pinning its ABI
/// integer.
fn assert_contract_code(code: ReturnCode, contract: Contract, context: &str) {
    assert_abi_integer(code);
    assert!(
        contract.allowed().contains(&code),
        "{context}: {} returned {code:?} ({}), which is outside the codes it \
         documents: {:?}",
        contract.entry_point(),
        code.as_c_int(),
        contract.allowed()
    );
}

/// Collapses a `Result<ReturnCode, ZlibError>` to its code and asserts the
/// contract, so both halves of the safe API are checked identically.
fn contract_code(
    result: Result<ReturnCode, ZlibError>,
    contract: Contract,
    context: &str,
) -> ReturnCode {
    let code = result.unwrap_or_else(ZlibError::as_return_code);
    assert_contract_code(code, contract, context);
    code
}

/// True when this build has gzip framing compiled in, derived from the one place
/// the library publishes the answer: bit 17 (`NO_GZIP`) of `zlibCompileFlags()`,
/// which is set exactly when the `gzip` feature is off.
fn gzip_supported() -> bool {
    zlib_compile_flags() & (1 << 17) == 0
}

/// Requires the engine's behaviour to match the capability the compile-flags word
/// advertises, in both directions.
///
/// gzip-only (`31`) and auto-detect (`47`) `windowBits` must be accepted when
/// [`gzip_supported`] is true and refused with `Z_STREAM_ERROR` when it is false;
/// the zlib framing must be accepted either way. A disagreement means either the
/// flags word or `inflate_reset2`'s bounds test is wrong, and both are ABI
/// surfaces a consumer relies on to decide what it may ask for.
#[cfg(feature = "gzip")]
fn assert_capability_agrees() {
    let gzip = gzip_supported();
    for (bits, needs_gzip) in [
        (WBITS_AUTO, true),
        (WBITS_GZIP, true),
        (WBITS_ZLIB, false),
        (WBITS_RAW, false),
    ] {
        let mut strm = ZStream::new();
        let code = contract_code(
            inflate_init2(&mut strm, bits),
            Contract::InflateInit2,
            "capability probe",
        );
        // A refused state reservation is a host condition, not a disagreement.
        if code == ReturnCode::MemError {
            continue;
        }
        let expected = if needs_gzip && !gzip {
            ReturnCode::StreamError
        } else {
            ReturnCode::Ok
        };
        assert_eq!(
            code,
            expected,
            "inflate_init2(windowBits = {bits}) answered {code:?} but \
             zlibCompileFlags() bit 17 says gzip is {}",
            if gzip { "COMPILED IN" } else { "COMPILED OUT" }
        );
        if code == ReturnCode::Ok {
            assert_eq!(
                contract_code(
                    inflate_end(&mut strm),
                    Contract::InflateEnd,
                    "capability probe"
                ),
                ReturnCode::Ok,
                "inflate_end must report Z_OK for a stream inflate_init2 accepted"
            );
        }
    }
}

/// Decodes `stream` with the framing selected by `window_bits`.
///
/// # Callers must pass a framing this build supports
///
/// `window_bits` must be either one of the always-available zlib / raw values or,
/// when [`gzip_supported`] reports `true`, a gzip / auto-detect value. Under that
/// precondition `inflate_init2` is REQUIRED to return `Z_OK`: a rejection is a
/// finding, not a feature gate, and this function panics with the framing and the
/// code.
///
/// # Return value and panic policy
///
/// The **one** exception is a refused state reservation, isolated by exact code
/// (`Z_MEM_ERROR`) and reported as `None`, because libFuzzer runs under an
/// `-rss_limit_mb` ceiling and a host allocation failure is not a library defect.
///
/// This replaces the previous "return `None` whenever `inflate_init2` fails" gate,
/// which could not distinguish "gzip is compiled out" from "the gzip path
/// regressed" and silently switched off every probe below it in the second case:
/// had `inflate_init2` started rejecting a framing it must accept, every caller
/// would have skipped its assertions and the target would have stayed green.
///
/// Every exit path — clean end, error, stall, and exhausted work budget alike —
/// tears the stream down with an explicit `inflate_end` whose code is checked
/// against its contract, mirroring C even though owning the state would release
/// it anyway.
fn drive(stream: &[u8], window_bits: i32, drain: Drain) -> Option<Decoded> {
    let mut strm = ZStream::new();
    match contract_code(
        inflate_init2(&mut strm, window_bits),
        Contract::InflateInit2,
        "drive",
    ) {
        ReturnCode::Ok => {}
        ReturnCode::MemError => return None,
        other => panic!(
            "inflate_init2(windowBits = {window_bits}) answered {other:?} for a \
             framing this build advertises as available (gzip {}); only \
             Z_MEM_ERROR is in contract here",
            if gzip_supported() { "on" } else { "off" }
        ),
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
        assert_contract_code(outcome.code, Contract::Inflate, "drive");

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
        // is reference zlib's `avail_in == 0` test.
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

    // A successful `inflate_init2` narrows `inflate_end` from its documented
    // two-code set to exactly `Z_OK`: the only documented failure is an
    // inconsistent stream, and this one was just initialised and driven through
    // the safe API. C callers routinely ignore this code, which is why a harness
    // must not.
    assert_eq!(
        contract_code(inflate_end(&mut strm), Contract::InflateEnd, "drive"),
        ReturnCode::Ok,
        "inflate_end must report exactly Z_OK after a successful inflate_init2"
    );

    Some(Decoded { code, produced })
}

/// Drives `stream` under `window_bits` purely for coverage.
///
/// *Which* code came back is deliberately not asserted. Every stream reaching
/// this function is arbitrary fuzzer input, for which a clean rejection — a
/// `DataError`, a `BufError`, or a stall — is the *correct* result, so demanding
/// any particular code would turn every corrupt input into a false crash and
/// make the target worthless. The one place a specific outcome is required is the
/// well-formed member built by [`valid_member`], whose validity the harness
/// controls end to end.
///
/// Discarding the decode code is NOT the same as discarding the call, and the
/// difference matters: if [`drive`] reported a swallowed init failure for this
/// function to throw away, a decoder that refused `windowBits = 15` — a value it
/// must always accept — would produce no signal at all.
///
/// What *is* checked here holds for every input: total output never exceeds
/// [`MAX_PRODUCED`], which is the bounded-work design proving itself rather than
/// asserting itself in a comment. The remaining per-pass invariants — a return
/// code drawn from the nine zlib defines and inside the calling entry point's own
/// documented set, progress never exceeding the buffers handed over, and a
/// successful explicit teardown — are enforced inside [`drive`].
fn sweep(stream: &[u8], window_bits: i32) {
    if let Some(decoded) = drive(stream, window_bits, Drain::UntilInputDrained) {
        assert!(
            decoded.produced <= MAX_PRODUCED,
            "a drain ending in {:?} produced {} bytes, over the {MAX_PRODUCED}-byte ceiling",
            decoded.code,
            decoded.produced
        );
    }
}

// ===========================================================================
// RFC 1952 member synthesis
// ===========================================================================

/// Appends the RFC 1952 trailer: the little-endian CRC-32 of the uncompressed
/// data followed by the little-endian ISIZE (its length mod 2^32).
#[cfg(feature = "gzip")]
fn push_trailer(member: &mut Vec<u8>, crc: u32, isize_mod32: u32) {
    member.extend_from_slice(&crc.to_le_bytes());
    member.extend_from_slice(&isize_mod32.to_le_bytes());
}

/// Appends a NUL-terminated header string built from up to [`MAX_FIELD`] fuzz
/// bytes, stripping embedded NULs so the terminator position stays under the
/// fuzzer's control via the length byte rather than the content.
#[cfg(feature = "gzip")]
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
#[cfg(feature = "gzip")]
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
#[cfg(feature = "gzip")]
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
// The non-empty accept-path anchor: payload bytes, trailer, and metadata
// ===========================================================================

/// Drives a **non-empty** gzip member all the way round — encoder to decoder —
/// and asserts every value RFC 1952 makes verifiable.
///
/// # Why an empty payload was not enough
///
/// [`valid_member`] carries [`EMPTY_DEFLATE`], so the only accept-path facts it
/// can establish are "the header parsed" and "zero bytes came out". That leaves
/// the parts of the member a real consumer depends on completely unasserted: the
/// payload bytes themselves, the little-endian CRC-32 the decoder checks them
/// against, the ISIZE it checks the length against, and the header metadata a
/// caller reads back through `inflateGetHeader`. An empty payload cannot
/// distinguish a working CRC-32 check from one that is never reached, because the
/// checksum of nothing is the initial value.
///
/// # What is asserted
///
/// The member is produced by the crate's own gzip-framed encoder with a header
/// installed through `deflateSetHeader`, so every byte is under the harness's
/// control and a rejection cannot be legitimate:
///
/// * the encoder reaches `Z_STREAM_END` in one `Z_FINISH` pass inside
///   `deflate_bound`, and `deflate_end` answers exactly `Z_OK`;
/// * the member opens with the gzip magic and `CM = 8`;
/// * its **last eight bytes** are exactly `crc32(payload)` then `payload.len()`,
///   both little-endian — checked at the wire level, independently of whatever
///   the decoder later concludes;
/// * the decoder reaches `Z_STREAM_END`, produces exactly `payload.len()` bytes,
///   and those bytes equal the payload; and
/// * the metadata read back through `inflateGetHeader` / `inflate_header` matches
///   what was installed: `name`, `comment`, `extra`, `time`, `os`, `hcrc`, and
///   `done`.
///
/// `extra_max` / `name_max` / `comm_max` are set generously so the copies are
/// never clamped, which is what makes an exact comparison the right assertion.
#[cfg(feature = "gzip")]
fn probe_valid_member_with_metadata(src: &mut Bytes<'_>) {
    // Bounded by a fixed cap, never by an unbounded fuzzer-supplied number.
    let control = src.byte();
    let level = i32::from(src.byte() % 10);
    let name = ascii_field(src, "anchor-name");
    let comment = ascii_field(src, "anchor-comment");
    let extra = src.take(MAX_ANCHOR_FIELD).to_vec();
    let time = u32::from_le_bytes([src.byte(), src.byte(), src.byte(), src.byte()]);
    let os = i32::from(src.byte());
    let payload = anchor_payload(src);

    let installed = GzHeader::new()
        .with_text(control & 0x01 != 0)
        .with_time(time)
        .with_os(os)
        .with_name(name.clone())
        .with_comment(comment.clone())
        .with_extra(extra.clone());
    let hcrc = control & 0x02 != 0;
    let mut installed = installed;
    installed.hcrc = hcrc;

    // ---- encode ----
    let mut strm = ZStream::new();
    match contract_code(
        deflate_init2(
            &mut strm,
            level,
            Z_DEFLATED,
            WBITS_GZIP,
            DEF_MEM_LEVEL,
            Strategy::Default,
        ),
        Contract::DeflateInit2,
        "anchor encode",
    ) {
        ReturnCode::Ok => {}
        // Host memory pressure only; see `drive`.
        ReturnCode::MemError => return,
        other => panic!(
            "deflate_init2 refused gzip framing at level {level} with {other:?}; \
             windowBits 31 and memLevel 8 are in range whenever gzip is compiled \
             in, so Z_MEM_ERROR is the only refusal in contract here"
        ),
    }
    assert_eq!(
        contract_code(
            deflate_set_header(&mut strm, Some(installed)),
            Contract::DeflateSetHeader,
            "anchor encode"
        ),
        ReturnCode::Ok,
        "deflate_set_header must accept a header on a gzip-framed stream"
    );

    // The header fields ride in the output ahead of the payload, so the bound
    // gets their exact byte cost added to it rather than a guess.
    let header_cost = name
        .len()
        .saturating_add(comment.len())
        .saturating_add(extra.len())
        .saturating_add(32);
    let mut member = vec![0u8; deflate_bound(&strm, payload.len()).saturating_add(header_cost)];
    let outcome = deflate(&mut strm, &payload, &mut member, Z_FINISH);
    assert_contract_code(outcome.code, Contract::Deflate, "anchor encode");
    assert_eq!(
        outcome.code,
        ReturnCode::StreamEnd,
        "one deflate(Z_FINISH) into a bound-sized buffer must finish the member"
    );
    assert_eq!(
        outcome.consumed,
        payload.len(),
        "all input must be consumed"
    );
    member.truncate(outcome.produced);
    assert_eq!(
        contract_code(
            deflate_end(&mut strm),
            Contract::DeflateEnd,
            "anchor encode"
        ),
        ReturnCode::Ok,
        "deflate_end must report Z_OK after Z_STREAM_END"
    );

    // ---- wire-level checks, independent of the decoder ----
    assert!(
        member.len() >= 18,
        "a gzip member carrying {} payload bytes cannot be {} bytes long",
        payload.len(),
        member.len()
    );
    assert_eq!(
        &member[..3],
        &[GZIP_ID1, GZIP_ID2, CM_DEFLATE],
        "the encoder must open the member with the gzip magic and CM = 8"
    );
    let mut expected_trailer = Vec::with_capacity(8);
    push_trailer(
        &mut expected_trailer,
        crc32(0, &payload),
        payload.len() as u32,
    );
    assert_eq!(
        &member[member.len() - 8..],
        &expected_trailer[..],
        "the trailer must be the little-endian CRC-32 of the payload followed by \
         its length mod 2^32 ({} payload bytes)",
        payload.len()
    );

    // ---- decode, with the header captured ----
    let mut back = ZStream::new();
    match contract_code(
        inflate_init2(&mut back, WBITS_GZIP),
        Contract::InflateInit2,
        "anchor decode",
    ) {
        ReturnCode::Ok => {}
        ReturnCode::MemError => return,
        other => panic!("inflate_init2(31) answered {other:?} with gzip compiled in"),
    }
    let sink = GzHeader::new()
        .with_extra(Vec::new())
        .with_name(Vec::new())
        .with_comment(Vec::new());
    let mut sink = sink;
    // Generous ceilings so nothing is clamped and an exact comparison is right.
    sink.extra_max = MAX_ANCHOR_FIELD as u32 + 64;
    sink.name_max = MAX_ANCHOR_FIELD as u32 + 64;
    sink.comm_max = MAX_ANCHOR_FIELD as u32 + 64;
    assert_eq!(
        contract_code(
            inflate_get_header(&mut back, sink),
            Contract::InflateGetHeader,
            "anchor decode"
        ),
        ReturnCode::Ok,
        "inflate_get_header must accept a sink on a gzip-framed stream"
    );

    let mut restored = vec![POISON; payload.len().saturating_add(1)];
    let mut in_pos = 0usize;
    let mut produced = 0usize;
    let mut passes = 0u32;
    let code = loop {
        passes = passes.saturating_add(1);
        assert!(
            passes <= WORK_BUDGET,
            "decoding a self-produced {}-byte member took more than \
             {WORK_BUDGET} passes",
            member.len()
        );
        let outcome = inflate(
            &mut back,
            &member[in_pos..],
            &mut restored[produced..],
            Z_NO_FLUSH,
        );
        assert_contract_code(outcome.code, Contract::Inflate, "anchor decode");
        in_pos = in_pos.saturating_add(outcome.consumed);
        produced = produced.saturating_add(outcome.produced);
        match outcome.code {
            ReturnCode::Ok => assert!(
                outcome.consumed != 0 || outcome.produced != 0,
                "the decoder stalled on a self-produced member"
            ),
            other => break other,
        }
    };
    assert_eq!(
        code,
        ReturnCode::StreamEnd,
        "a self-produced gzip member must decode to Z_STREAM_END, not {code:?}"
    );
    assert_eq!(
        in_pos,
        member.len(),
        "Z_STREAM_END must leave no member bytes unread"
    );
    assert_eq!(
        produced,
        payload.len(),
        "the decoder produced {produced} bytes for a {}-byte payload",
        payload.len()
    );
    assert_eq!(
        &restored[..produced],
        &payload[..],
        "the decoded payload must equal the encoded payload byte for byte"
    );

    // ---- metadata round-trip ----
    let parsed = inflate_header(&back).expect("the registered header must still be borrowable");
    assert!(
        parsed.done,
        "the header parse must be marked complete once the member has finished"
    );
    assert_eq!(
        parsed.name.as_deref(),
        Some(&name[..]),
        "the NAME field must survive the round trip"
    );
    assert_eq!(
        parsed.comment.as_deref(),
        Some(&comment[..]),
        "the COMMENT field must survive the round trip"
    );
    assert_eq!(
        parsed.extra.as_deref(),
        Some(&extra[..]),
        "the EXTRA field must survive the round trip"
    );
    assert_eq!(parsed.time, time, "MTIME must survive the round trip");
    assert_eq!(
        parsed.os,
        os & 0xff,
        "the encoder writes the low byte of the installed OS code verbatim"
    );
    assert_eq!(
        parsed.hcrc, hcrc,
        "the FHCRC flag must be reported exactly as it was requested"
    );
    assert_eq!(
        contract_code(
            inflate_end(&mut back),
            Contract::InflateEnd,
            "anchor decode"
        ),
        ReturnCode::Ok,
        "inflate_end must report Z_OK after Z_STREAM_END"
    );
}

/// Builds a NUL-free header string of at most [`MAX_ANCHOR_FIELD`] bytes from
/// fuzz bytes, falling back to `fallback` when the input has run out.
///
/// NULs are stripped rather than rejected because the gzip `NAME`/`COMMENT`
/// fields are NUL-terminated on the wire, so an embedded NUL would truncate the
/// field and make the round-trip comparison compare something the encoder never
/// promised to keep.
#[cfg(feature = "gzip")]
fn ascii_field(src: &mut Bytes<'_>, fallback: &str) -> Vec<u8> {
    let want = usize::from(src.byte()) % (MAX_ANCHOR_FIELD + 1);
    let field: Vec<u8> = src.take(want).iter().copied().filter(|&b| b != 0).collect();
    if field.is_empty() {
        fallback.as_bytes().to_vec()
    } else {
        field
    }
}

// ===========================================================================
// The safe `gz*` file-I/O lifecycle probe
// ===========================================================================

/// Writes a payload through the safe `gz*` API, reads it back, and asserts exact
/// recovery — the whole `gzopen` / `gzwrite` / `gzclose_w` / `gzopen` / `gzread` /
/// `gzclose_r` lifecycle in one pass.
///
/// # Why this belongs in the gzip harness
///
/// Every other probe in this file drives the streaming engine directly, so the
/// `gz*` layer — its 8 KiB buffering, its `How::Look` sniffing, and the deferred
/// finish that makes `gzclose_w` mandatory — is never reached. That layer is the
/// interface most consumers of a gzip library actually use.
///
/// # Path safety
///
/// The path contains **no** input-derived component — a fuzzer-chosen path
/// fragment is a directory-traversal primitive and this harness must not become
/// one — and it is not merely unpredictable but *unshared*: [`TempGzWorkspace`]
/// creates a fresh, owner-only directory with create-new semantics and puts the
/// payload inside it, so nothing that another user could have planted in the
/// shared temp directory is ever opened, followed, or truncated.
///
/// Both handles are closed explicitly (a `GzState` destructor deliberately
/// performs no finishing work, precisely so a deferred write error cannot be
/// swallowed), and the workspace is removed on every exit path: by the guard's
/// `Drop` on the ordinary and unwinding routes, and by an explicit
/// [`TempGzWorkspace::cleanup`] immediately before each `panic!`, because
/// libFuzzer's panic hook aborts rather than unwinding and would otherwise skip
/// every destructor.
///
/// A `gzclose_w` that cannot flush is asserted rather than tolerated: the payload
/// is bounded and the directory was created empty a moment earlier, so a failure
/// here is a genuine finding in the write path.
#[cfg(feature = "gzip")]
fn probe_gz_lifecycle(payload: &[u8]) {
    // A workspace the environment refuses is not a library finding; the in-memory
    // probes carry the coverage on their own in that case.
    let Some(temp) = TempGzWorkspace::new() else {
        return;
    };

    // ---- write leg ----
    // `"wbx"` adds `O_EXCL` (`src/gz/open.rs` maps the `'x'` flag to
    // `create_new(true)`), so even inside a directory that did not exist a moment
    // ago the file is never opened through a pre-existing name.
    let mut out = match gzopen(temp.path(), "wbx") {
        Ok(state) => state,
        // Same environmental tolerance as above — an exhausted descriptor table or
        // a full filesystem is not a finding about the library. It is *only* that:
        // every library outcome after this point is asserted, never skipped.
        Err(_) => return,
    };
    let written = gzwrite(&mut out, payload);
    let write_ok = written == payload.len() as i32;
    let closed = gzclose_w(out);
    if !write_ok || closed != ReturnCode::Ok.as_c_int() {
        temp.cleanup();
        panic!(
            "gz write leg failed for a {}-byte payload: gzwrite returned \
             {written}, gzclose_w returned {closed}",
            payload.len()
        );
    }

    // ---- read leg ----
    let read_back = (|| -> Result<Vec<u8>, String> {
        let mut inp = gzopen(temp.path(), "rb").map_err(|e| format!("gzopen for read: {e:?}"))?;
        let mut got = Vec::with_capacity(payload.len());
        let mut chunk = [0u8; 512];
        loop {
            let n = gzread(&mut inp, &mut chunk);
            if n < 0 {
                let code = gzclose_r(inp);
                return Err(format!("gzread returned {n} (gzclose_r {code})"));
            }
            if n == 0 {
                break;
            }
            got.extend_from_slice(&chunk[..n as usize]);
            // Reported rather than asserted, so the reader is still closed and the
            // workspace still removed on this route: a bare `assert!` here would
            // abort under libFuzzer holding an open `gzFile` and leaving the
            // directory behind.
            if got.len() > payload.len() {
                let code = gzclose_r(inp);
                return Err(format!(
                    "gzread produced {} bytes, more than the {} written (gzclose_r {code})",
                    got.len(),
                    payload.len()
                ));
            }
        }
        let code = gzclose_r(inp);
        if code != ReturnCode::Ok.as_c_int() {
            return Err(format!("gzclose_r returned {code}"));
        }
        Ok(got)
    })();

    // Remove the workspace before asserting, so an abort-on-panic cannot leak it.
    temp.cleanup();
    let got = read_back.unwrap_or_else(|e| panic!("gz read leg failed: {e}"));
    assert_eq!(
        got.len(),
        payload.len(),
        "the gz round trip recovered {} of {} bytes",
        got.len(),
        payload.len()
    );
    assert_eq!(
        got, payload,
        "the gz round trip must recover the written bytes exactly"
    );
}

/// Sanitizes `raw` into a single filesystem component that cannot escape its
/// parent directory.
///
/// Only ASCII alphanumerics, `_` and `-` survive, capped at 32 characters, and an
/// input that filters down to nothing becomes `x`. Every traversal and separator
/// form — `..`, `/`, `\`, a drive prefix — is therefore collapsed to something
/// that can only ever name a child of the directory it is joined to. The only
/// value this is applied to is the `CLONE_INDEX` environment variable, which
/// parallel clones of this repository use to stay distinct inside one shared
/// `/tmp`; no fuzz input reaches it.
#[cfg(feature = "gzip")]
fn safe_path_component(raw: &str) -> String {
    let filtered: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect();
    if filtered.is_empty() {
        "x".to_owned()
    } else {
        filtered
    }
}

/// Creates `path` as a new, private directory, failing if anything is already
/// there.
///
/// Non-recursive on purpose: unlike `create_dir_all` this reports
/// [`std::io::ErrorKind::AlreadyExists`] when the name is taken — including when
/// it is taken by a symlink someone else planted — which is what lets
/// [`TempGzWorkspace::new`] move to the next candidate instead of following the
/// link or deleting it. On Unix the `0o700` mode is handed to `mkdir(2)` itself,
/// so the directory is never even briefly group- or world-accessible and there is
/// no `set_permissions` window to race. Both properties describe the moment of
/// creation; on other targets the mode is the platform default.
#[cfg(feature = "gzip")]
fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

/// An owned private directory holding one temporary `.gz` payload, both removed
/// when the guard is dropped.
///
/// # Why the directory carries the uniqueness
///
/// The predecessor of this type was a single filename in the shared temp
/// directory — `zlib_rs_fuzz_gzip_<pid>_<seq>.gz` — opened for create-and-
/// truncate. Every component of that name is derivable by anyone on the host, and
/// a create-and-truncate open follows a symlink: a name planted ahead of the
/// harness redirects the write to whatever the attacker chose (CWE-59), and the
/// use of a predictable name in a world-writable directory is CWE-377 in its own
/// right.
///
/// Moving the uniqueness up one level fixes both. The directory name combines a
/// fixed prefix, a [`safe_path_component`]-sanitized `CLONE_INDEX`, the process
/// id, a monotonic counter and a retry ordinal, which keeps it distinct across
/// libFuzzer's workers, across concurrent runs, and across sibling clones sharing
/// one `/tmp`; and it is created with create-new semantics, so an occupied
/// candidate — file, directory or symlink — is *skipped*, never followed and never
/// deleted. Nothing pre-existing is ever removed.
///
/// The payload name is then predictable only *within* a directory that did not
/// exist a moment earlier and, on Unix, was owner-only from `mkdir(2)` onwards.
/// The file itself is still opened with `O_EXCL` (`gzopen(…, "wbx")`) so a name
/// that somehow is taken fails loudly rather than being truncated. These are
/// creation-time properties: the guard holds paths rather than open handles, so it
/// makes no claim about the directory still being the same object later.
#[cfg(feature = "gzip")]
struct TempGzWorkspace {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

#[cfg(feature = "gzip")]
impl TempGzWorkspace {
    /// Creates the private directory and returns the guard, or [`None`] when the
    /// environment will not provide one.
    ///
    /// Returning [`None`] rather than panicking is deliberate: a read-only or full
    /// temp directory is a property of the host, and a fuzz target that aborted on
    /// it would report an environment problem as a library crash. The retry loop is
    /// bounded at [`TEMP_DIR_ATTEMPTS`] so a pathological environment cannot spin.
    fn new() -> Option<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let clone = safe_path_component(&std::env::var("CLONE_INDEX").unwrap_or_default());
        let pid = std::process::id();
        let base = std::env::temp_dir();

        // Retry only ever advances the candidate name; it never deletes.
        for attempt in 0..TEMP_DIR_ATTEMPTS {
            let candidate = base.join(format!("zlib_rs_fuzz_gz_{clone}_{pid}_{seq}_{attempt}"));
            match create_private_dir(&candidate) {
                Ok(()) => {
                    let path = candidate.join("payload.gz");
                    return Some(Self {
                        dir: candidate,
                        path,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// The payload path inside the private directory.
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Removes the payload and the directory, best effort.
    ///
    /// Idempotent, and called both from [`Drop`] and explicitly before every
    /// `panic!` on this path — libFuzzer's panic hook aborts rather than unwinding,
    /// so a destructor alone would not run on the failing route.
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(feature = "gzip")]
impl Drop for TempGzWorkspace {
    fn drop(&mut self) {
        // The recursive removal rests on `dir` not having existed before `new`
        // created it with create-new semantics, so the removal set starts from a
        // path this guard brought into existence rather than one it adopted, and on
        // Unix from one no other user could enter. Best effort on every route out,
        // including an unwinding one: failing to clean up must never mask the
        // original failure.
        self.cleanup();
    }
}

/// Builds a bounded, always **non-empty** payload for the anchor.
///
/// A fixed fallback keeps the anchor meaningful on a short or empty input, which
/// is exactly when the fuzzer has nothing to contribute: the whole point of this
/// probe is that the CRC-32 and ISIZE checks are reached on every iteration.
#[cfg(feature = "gzip")]
fn anchor_payload(src: &mut Bytes<'_>) -> Vec<u8> {
    let taken = src.take(MAX_ANCHOR_PAYLOAD);
    if taken.is_empty() {
        b"a non-empty gzip payload for the accept-path anchor".to_vec()
    } else {
        taken.to_vec()
    }
}

// ===========================================================================
// Entry point
//
// Every leg that needs gzip or auto-detect framing lives in one gated function
// with a no-op counterpart, so the gate sits on gzip-specific code only and the
// entry point below stays free of conditional compilation.
// ===========================================================================

/// Runs every gzip-framing leg over `data`.
#[cfg(feature = "gzip")]
fn gzip_coverage(data: &[u8]) {
    // 1. The capability is DERIVED from the library's own compile-flags word, and
    //    the engine is then required to agree with it in both directions. Doing
    //    this first means every gzip probe below runs on the strength of a
    //    published capability rather than on whether an earlier call happened to
    //    succeed — so a regression in the gzip path is a panic here instead of a
    //    silent loss of gzip coverage. Inferring it from "did `inflate_init2`
    //    happen to fail" cannot tell "gzip is compiled out" apart from "the gzip
    //    path regressed", and in the second case it disables the very coverage
    //    that would have caught the regression.
    assert_capability_agrees();
    let gzip_available = gzip_supported();

    if !gzip_available {
        return;
    }

    // 2. The unmodified input under both gzip-capable framings — the primary
    //    case, and the one that covers genuinely arbitrary streams, including the
    //    reserved-FLG-bit and bad-CM rejections the synthesizers mask away.
    //    gzip-only framing is a different route through `inflate_reset2` than the
    //    auto-detect probe.
    sweep(data, WBITS_AUTO);
    sweep(data, WBITS_GZIP);

    // 3. A synthesized member walks the fuzzer into the RFC 1952 header phases:
    //    EXTRA, NAME, COMMENT, the header CRC, and the CRC-32 / ISIZE trailer.
    //    Malformed and short fields are reached naturally, since a truncated
    //    input simply yields truncated fields.
    let mut src = Bytes::new(data);
    let member = synthesize_member(&mut src);
    sweep(&member, WBITS_GZIP);
    sweep(&member, WBITS_AUTO);

    // 4. An explicitly truncated copy, cut at a fuzzer-chosen offset, so every
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

    // 5. The empty-payload accept-path anchor: a well-formed member, driven to
    //    completion. It must decode cleanly to zero bytes. Sound precisely
    //    because the harness — not the fuzzer — decided every byte's validity.
    //
    //    The `None` arm is genuine heap exhaustion and nothing else — it is NOT a
    //    capability escape, since gzip support was settled in step 1 and reaching
    //    here means the build has it. When a swallowed init failure could also
    //    land here it skipped the entire accept path, leaving the target with no
    //    positive evidence that the decoder accepts anything at all, which is the
    //    one thing a reject-everything decoder would pass.
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

    // 6. The NON-EMPTY accept-path anchor. The empty member above cannot reach the
    //    payload, the CRC-32, the ISIZE, or the header metadata — the checksum of
    //    nothing is the initial value, so an unreached check looks identical to a
    //    passing one. This probe encodes a real payload with a real installed
    //    header and asserts all four.
    let mut meta_src = Bytes::new(data);
    probe_valid_member_with_metadata(&mut meta_src);

    // 7. The safe `gz*` file lifecycle, sampled. Nothing above reaches the
    //    buffered file layer that most gzip consumers actually use, and its
    //    deferred finish is exactly the kind of path an in-memory harness cannot
    //    see. Sampled because file I/O is orders of magnitude slower than a
    //    decode; the selector is a fuzz byte so the fuzzer can steer into it.
    if data.first().copied().unwrap_or(0) % GZ_LIFECYCLE_SAMPLE == 0 {
        let mut gz_src = Bytes::new(data);
        let payload = anchor_payload(&mut gz_src);
        probe_gz_lifecycle(&payload);
    }
}

/// Without the forwarding `gzip` feature there is no gzip framing to exercise,
/// so the legs collapse to nothing and the zlib / raw sweeps stand alone.
#[cfg(not(feature = "gzip"))]
fn gzip_coverage(_data: &[u8]) {}

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
    // The framings that never depend on the `gzip` feature. Feeding the same
    // bytes through zlib and raw framing is what exercises the auto-detect
    // discrimination against input that is and is not gzip.
    sweep(data, WBITS_ZLIB);
    sweep(data, WBITS_RAW);

    // Everything that needs gzip or auto-detect framing.
    gzip_coverage(data);
});
