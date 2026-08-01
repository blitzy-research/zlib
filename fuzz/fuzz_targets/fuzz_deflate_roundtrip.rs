#![no_main]
//! Deflate -> inflate round-trip harness.
//!
//! For an arbitrary payload and a fuzzer-chosen compression level, compressing
//! with [`zlib_rs::compress2`] and then decompressing with
//! [`zlib_rs::uncompress`] must reproduce the input byte-for-byte (RFC 1950 /
//! 1951 losslessness). A mismatch or a decode failure on our own output is a
//! correctness bug; a panic is a robustness bug.
//!
//! # The four-axis sweep
//!
//! The one-call `compress2` / `uncompress` pair is inherently zlib-framed and
//! varies only the level, so it cannot reach the raw or gzip wrappers, the four
//! non-default strategies, or any non-default window / memory configuration. The
//! harness therefore additionally drives the **streaming** engine
//! (`deflate_init2` / `deflate` / `deflate_end` paired with `inflate_init2` /
//! `inflate` / `inflate_end`) over all four axes, every one of them selected
//! from a fixed-size header at the front of the fuzz input:
//!
//! | axis          | swept domain                                              |
//! |---------------|-----------------------------------------------------------|
//! | `level`       | `-1` (`Z_DEFAULT_COMPRESSION`) and `0..=9`                 |
//! | `strategy`    | `Z_DEFAULT_STRATEGY` / `FILTERED` / `HUFFMAN_ONLY` / `RLE` / `FIXED` |
//! | `windowBits`  | raw `-15..=-9`, zlib `8..=15`, gzip `25..=31` (when built in) |
//! | `memLevel`    | `1..=MAX_MEM_LEVEL`, `DEF_MEM_LEVEL` as the baseline        |
//!
//! Those are the same axes the migration's byte-identity conformance grid is
//! swept over, so keeping them reachable from the first few input bytes lets the
//! fuzzer steer straight into the interesting configurations from a cold, empty
//! corpus.
//!
//! # The flush schedule
//!
//! A fifth dimension crosses the grid above: the chunked leg rotates its
//! non-final passes through **every flush code `deflate` accepts** —
//! `Z_NO_FLUSH`, `Z_PARTIAL_FLUSH`, `Z_SYNC_FLUSH`, `Z_FULL_FLUSH`, and
//! `Z_BLOCK` — from a fuzzer-chosen starting offset, with `Z_FINISH` on the last
//! pass. `Z_TREES` is excluded because it is inflate-only; `deflate` documents
//! its domain as `Z_NO_FLUSH ..= Z_BLOCK` and answers `Z_STREAM_ERROR` outside
//! `0..=5`, so sending it would sweep a parameter rejection rather than a
//! compression path.
//!
//! This matters because each of those codes changes the emitted stream in a
//! different way — an empty fixed block, a byte-aligned empty stored block, a
//! window reset, or up to seven deliberately withheld bits — and each interacts
//! with the `last_flush`/`rank` bookkeeping that decides whether a repeated call
//! is a useful continuation or a duplicate. Sending only `Z_NO_FLUSH` and
//! `Z_FINISH`, as this harness previously did, left all of that unreached. The
//! rotation advances only after a flush has *completed*, because zlib requires a
//! call that returns with `avail_out == 0` to be repeated with the same flush
//! value; see [`Config::non_final_flush`].
//!
//! # What this harness does and does not prove
//!
//! Each execution asserts **losslessness** for the configuration it selected:
//! the compressed bytes must decode back to the exact input. It exercises the
//! parameter dimensions listed above; it does not enumerate them, and a sampled
//! run of a randomized harness is not an exhaustive comparison. It deliberately
//! makes **no** assertion about the compressed bytes themselves and bakes in no
//! expected-output vector — byte-identity against reference zlib is established
//! by the always-on baked oracle vectors in `tests/interop.rs` and by the
//! opt-in live sweep in `tests/c_oracle.rs`, and a fuzz finding here never
//! authorises a change to a match-finder heuristic. Compressed output
//! legitimately differs between the
//! whole-buffer and chunked legs below, because C's `deflate_stored` consults
//! `avail_out` when it sizes stored blocks (`deflate.c` L1689-L1748); that is
//! not a defect. For the same reason no gzip framing byte is inspected: byte 10
//! of a gzip header is `OS_CODE`, a compile-time platform choice.
//!
//! # Panic policy: a panic must mean the LIBRARY broke, never the harness
//!
//! Unlike a test, a fuzz target is fed arbitrary bytes, so a rejection can be a
//! legitimate outcome. Every fuzzer-derived parameter is therefore clamped into
//! the exact domain C `deflateInit2_` accepts *before* the call. What that
//! clamping buys is the right to be strict afterwards, and this harness now takes
//! it: because every parameter is in range and every output buffer is grown on
//! demand, a producing leg either drives its stream to `Z_STREAM_END` or panics.
//!
//! The **one** exit that is not a completed stream is a refused state
//! reservation, isolated by exact code (`Z_MEM_ERROR`), because libFuzzer runs
//! under an `-rss_limit_mb` ceiling and a host allocation failure is not a
//! library defect. Everything a caller previously could not distinguish from it —
//! a spent pass budget, a recoverable `Z_BUF_ERROR` from `deflate_params`, the
//! output-growth ceiling, a pass that made no progress — is now either recovered
//! from or asserted, so those cases can no longer silently skip the round-trip
//! comparison that is the whole point of the leg.
//!
//! Every loop is still explicitly bounded, because a hang is as much a finding as
//! a crash — but the bound is now an assertion rather than a quiet exit, since an
//! unbounded *harness* loop would be a harness bug while a library that cannot
//! finish inside a generous budget is a library bug.

use libfuzzer_sys::fuzz_target;

// Only the public, safe `zlib_rs` surface is used: the crate-root re-exports for
// the one-call wrappers and the shared types, plus the public engine modules for
// the streaming drivers and the flush / method / window / memory constants. The
// library's C-ABI boundary module is deliberately never reached from here, which
// is what keeps this harness entirely within safe Rust.
use zlib_rs::constants::{
    DEF_MEM_LEVEL, GZIP_WRAP_OFFSET, MAX_MEM_LEVEL, MAX_WBITS, Z_BLOCK, Z_DEFLATED, Z_FINISH,
    Z_FULL_FLUSH, Z_NO_FLUSH, Z_PARTIAL_FLUSH, Z_SYNC_FLUSH,
};
use zlib_rs::deflate::{deflate, deflate_bound, deflate_end, deflate_init2, deflate_params};
use zlib_rs::inflate::{inflate, inflate_end, inflate_init2};
use zlib_rs::{ReturnCode, Strategy, ZStream, ZlibError, compress_bound, compress2, uncompress};
// The compile-flags word is read for one purpose only — confirming the linked
// build really has gzip framing — so the import belongs to the gzip leg.
#[cfg(feature = "gzip")]
use zlib_rs::zlib_compile_flags;

// ===========================================================================
// Harness limits
// ===========================================================================

/// Number of configuration bytes consumed from the front of the fuzz input; the
/// remainder is the payload. Kept small and fixed so a cold corpus reaches every
/// configuration immediately.
const HEADER_LEN: usize = 9;

/// The flush codes a **non-final** deflate pass may legally use, rotated through
/// by [`Config::non_final_flush`].
///
/// [`Z_FINISH`] is absent because it terminates the stream and is applied
/// unconditionally to the last pass. [`zlib_rs::constants::Z_TREES`] is absent
/// for a different and more important reason: it is **inflate-only**. `deflate`
/// documents its flush domain as `Z_NO_FLUSH ..= Z_BLOCK` and answers
/// `Z_STREAM_ERROR` for anything outside `0..=5`, so including it would sweep a
/// parameter rejection rather than a compression path — and would then have to be
/// explained away in the panic policy instead of simply not being sent.
///
/// Every code that IS here changes the shape of the emitted stream:
/// `Z_PARTIAL_FLUSH` closes the block and appends a 10-bit empty *fixed* block,
/// `Z_SYNC_FLUSH` closes it and appends an empty *stored* block byte-aligned to
/// `00 00 ff ff`, `Z_FULL_FLUSH` does that and additionally resets the window so
/// decoding can restart from the marker, and `Z_BLOCK` closes the block while
/// deliberately withholding up to seven bits. The round trip must survive all of
/// them, in any order, which is what this harness now checks and previously did
/// not: only `Z_NO_FLUSH` and `Z_FINISH` were ever sent.
const NON_FINAL_FLUSHES: [i32; 5] = [
    Z_NO_FLUSH,
    Z_PARTIAL_FLUSH,
    Z_SYNC_FLUSH,
    Z_FULL_FLUSH,
    Z_BLOCK,
];

/// Consecutive passes a drain loop tolerates without forward progress before the
/// stall is reported as a finding.
///
/// A pass that neither consumes nor produces is answered by growing the output
/// buffer, and growth doubles, so reaching the 4 MiB
/// [`OUTPUT_GROWTH_LIMIT`] from any starting capacity takes at most about twenty
/// doublings. This budget is comfortably above that, so exhausting it means the
/// engine is stuck for a reason more room cannot fix — which is exactly the
/// condition the harness used to answer by returning `None` and skipping every
/// assertion that followed.
const MAX_STALLED_PASSES: u32 = 32;

/// Per-input-byte output allowance added to the chunked leg's starting capacity.
///
/// The chunked leg now emits a flush marker on most passes and a pass offers at
/// least one input byte, so the worst case is roughly one marker per byte. A
/// `Z_SYNC_FLUSH`/`Z_FULL_FLUSH` marker is an empty stored block — three bits of
/// header, filler to the byte boundary, then `00 00 ff ff` — so eight bytes per
/// input byte bounds it with room to spare. This only avoids the common case of
/// having to grow; the growth loop below remains the correctness mechanism and
/// the allowance is never derived from a fuzzer-controlled multiplier.
const FLUSH_MARKER_ALLOWANCE: usize = 8;

/// Hard cap on engine passes in any single drain loop, matching the `guard`
/// budget the sibling gzip harness uses. It bounds total work so a
/// pathological input cannot dominate the run; libFuzzer's own timeout would
/// otherwise flag it as noise.
const MAX_PASSES: u32 = 4096;

/// Ceiling on how far a drain loop may grow its output buffer. libFuzzer is run
/// with `-max_len=65536` and `-rss_limit_mb=2048`, so 4 MiB is ample for any
/// payload the fuzzer can supply and is never derived from a fuzzer-controlled
/// factor.
const OUTPUT_GROWTH_LIMIT: usize = 4 << 20;

/// Payload ceiling for the byte-at-a-time chunked leg. Small-chunk streaming
/// costs one engine pass per chunk, so restricting it to short payloads keeps
/// `exec/s` high enough for the fuzzer to keep finding new coverage while still
/// exercising the partial-progress paths on every short input.
const CHUNKED_PAYLOAD_LIMIT: usize = 2048;

/// Largest input chunk the chunked leg feeds per pass; the smallest is 1 byte.
const MAX_INPUT_CHUNK: usize = 64;

/// Filler written into every output buffer before it is handed to the engine, so
/// a short write cannot pass a content comparison by accident.
const POISON: u8 = 0xA5;

// ===========================================================================
// windowBits domains (the overloaded zlib `windowBits` contract)
// ===========================================================================

/// Smallest `windowBits` `deflateInit2` accepts — and **only** with zlib
/// framing: C `deflate.c` L436 rejects `(windowBits == 8 && wrap != 1)`, so raw
/// `-8` and gzip `24` are legitimate `Z_STREAM_ERROR`s rather than findings, and
/// are excluded from the swept domains below.
const MIN_WBITS_ZLIB: i32 = 8;

/// Smallest window the raw and gzip framings accept, for the reason above.
const MIN_WBITS_WRAPPED: i32 = 9;

/// Smallest `memLevel` `deflateInit2` accepts (C `deflate.c` L434 rejects
/// `memLevel < 1`).
const MIN_MEM_LEVEL: i32 = 1;

/// `windowBits` selecting inflate's auto-detect framing (`32 + 15`).
///
/// Auto-detection is **inflate-only and invalid for deflate**, so it appears
/// exclusively on the decode leg, and it lives in the `40..=47` range that the
/// inflate engine accepts only when gzip framing is built in — hence every use
/// sits behind [`gzip_supported`], which is itself gated on this package's
/// forwarding `gzip` feature.
const WBITS_AUTO: i32 = MAX_WBITS + 32;

/// Whether this build both asked for gzip framing and got it.
///
/// The gzip rows are gated twice, because the two guards answer different
/// questions.
///
/// *Compile time.* Cargo resolves a `feature` predicate against the crate being
/// compiled, and that crate here is the detached `zlib-rs-fuzz` package rather
/// than `zlib-rs`. `fuzz/Cargo.toml` therefore declares a **forwarding** `gzip`
/// feature (`gzip = ["zlib-rs/gzip"]`, on by default), which is what makes the
/// `#[cfg(feature = "gzip")]` on this function defined and truthful. Without that
/// declaration the predicate would be permanently false and rustc would reject
/// the unknown value through its `unexpected_cfgs` lint besides.
///
/// *Run time.* The library advertises the answer through the very mechanism a C
/// consumer uses — `zlibCompileFlags` bit 17 is `NO_GZIP`, set precisely when
/// gzip framing is absent — so reading it confirms from outside the crate that
/// the linked engine honours what the feature requested. The gzip rows are
/// selected rather than assumed, which keeps one binary correct under either
/// feature set.
///
/// Note what neither guard may become: a whole-file inner `#![cfg(...)]` would
/// compile away the entry-point macro invocation at the bottom of this file, and
/// with it `main`, leaving a `[[bin]]` that cannot link. Every gate here is per
/// item.
#[cfg(feature = "gzip")]
fn gzip_supported() -> bool {
    /// `zlibCompileFlags` bit 17, "no gzip framing in this build".
    const NO_GZIP: u32 = 1 << 17;
    zlib_compile_flags() & NO_GZIP == 0
}

/// Without the forwarding `gzip` feature the harness never asks for gzip
/// framing, so it drops out of every rotation below by construction.
#[cfg(not(feature = "gzip"))]
fn gzip_supported() -> bool {
    false
}

// ===========================================================================
// Configuration derived from the fuzz input
// ===========================================================================

/// The stream framing family a configuration encodes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// Raw DEFLATE (RFC 1951): no wrapper, no checksum.
    Raw,
    /// zlib wrapper (RFC 1950): 2-byte header plus a trailing Adler-32.
    Zlib,
    /// gzip wrapper (RFC 1952): gzip header plus a trailing CRC-32 and length.
    Gzip,
}

/// One fully-resolved point of the level x strategy x `windowBits` x `memLevel`
/// grid, plus the streaming knobs the chunked leg uses. Every field is already
/// clamped into the domain the engine accepts.
#[derive(Clone, Copy, Debug)]
struct Config {
    /// Compression level: `-1` (`Z_DEFAULT_COMPRESSION`, which the engine
    /// resolves internally to 6) or `0..=9`.
    level: i32,
    /// One of the five deflate strategies.
    strategy: Strategy,
    /// `windowBits` handed to `deflate_init2`. Its value also identifies the
    /// framing family: negative is raw, `8..=15` zlib, `25..=31` gzip.
    deflate_window_bits: i32,
    /// `windowBits` handed to `inflate_init2`, derived so the decoder's window
    /// is never smaller than the encoder's.
    inflate_window_bits: i32,
    /// `memLevel` handed to `deflate_init2`, in `1..=MAX_MEM_LEVEL`.
    mem_level: i32,
    /// Input bytes offered per pass in the chunked leg, in `1..=MAX_INPUT_CHUNK`.
    input_chunk: usize,
    /// Whether the chunked leg re-dispatches level / strategy mid-stream.
    redispatch: bool,
    /// Level for that mid-stream `deflate_params` re-dispatch.
    params_level: i32,
    /// Strategy for that mid-stream `deflate_params` re-dispatch.
    params_strategy: Strategy,
    /// Rotation offset into [`NON_FINAL_FLUSHES`] for the chunked leg's flush
    /// schedule, so every starting point in the cycle is reachable.
    flush_pick: usize,
}

impl Config {
    /// The flush code a non-final pass uses at schedule position `step`.
    ///
    /// Rotating rather than fixing one code per input means a single execution
    /// sees several different flush shapes in one stream — the interleaving is
    /// what exercises the `last_flush` / `rank` bookkeeping in
    /// `deflate_run`, which a constant flush cannot reach.
    ///
    /// `step` is advanced by the caller only after a flush has **completed**.
    /// zlib requires that a `deflate` call which returns with `avail_out == 0` be
    /// repeated with the *same* flush value until it returns with room to spare
    /// (`zlib.h`: "this function must be called again with the same value of the
    /// flush parameter and more output space"), so keying the schedule to the raw
    /// pass counter would violate the contract and turn a harness mistake into a
    /// library-looking finding.
    fn non_final_flush(&self, step: usize) -> i32 {
        NON_FINAL_FLUSHES[(self.flush_pick.wrapping_add(step)) % NON_FINAL_FLUSHES.len()]
    }
    /// Splits `data` into a fixed-size configuration header and the payload.
    ///
    /// Every byte is read through `Option`, so a short or empty input still
    /// yields a usable configuration: an absent level byte selects level 6 over an
    /// empty payload, and the remaining fallbacks are the zlib / max-window /
    /// `DEF_MEM_LEVEL` defaults.
    fn from_input(data: &[u8]) -> (Self, &[u8]) {
        let (header, payload) = data.split_at(HEADER_LEN.min(data.len()));
        let byte = |index: usize| header.get(index).copied();

        // Level in [-1, 9] (-1 == Z_DEFAULT_COMPRESSION, 0 == Z_NO_COMPRESSION,
        // ..., 9 == Z_BEST_COMPRESSION). `-1` resolving to 6 inside the engine
        // is a wire-format-visible constant this harness observes and never
        // changes.
        let level = match byte(0) {
            Some(first) => i32::from(first % 11) - 1,
            None => 6,
        };

        let strategy = strategy_from_id(byte(1).unwrap_or(0));
        let framing = framing_from_id(byte(2).unwrap_or(1));
        let deflate_window_bits = encode_window_bits(framing, byte(3).unwrap_or(0));
        let inflate_window_bits =
            decode_window_bits(framing, deflate_window_bits, byte(5).unwrap_or(0));

        // memLevel in [1, MAX_MEM_LEVEL]; an absent byte keeps the zlib default.
        let mem_level = match byte(4) {
            Some(raw) => MIN_MEM_LEVEL + i32::from(raw) % (MAX_MEM_LEVEL - MIN_MEM_LEVEL + 1),
            None => DEF_MEM_LEVEL,
        };

        // Chunk size in [1, MAX_INPUT_CHUNK]; `+ 1` keeps it non-zero so a pass
        // always offers at least one byte and can never spin.
        let input_chunk = usize::from(byte(6).unwrap_or(0)) % MAX_INPUT_CHUNK + 1;

        // The low bit enables the mid-stream re-dispatch; the remaining bits
        // choose its level and strategy.
        let redispatch = byte(7).is_some_and(|raw| raw & 1 == 1);
        let params_level = match byte(7) {
            Some(raw) => i32::from((raw >> 1) % 11) - 1,
            None => 6,
        };
        let params_strategy = strategy_from_id(byte(7).unwrap_or(0) >> 4);

        // Where the non-final flush rotation starts. Its own header byte rather
        // than spare bits of another field: sharing a byte would correlate the
        // flush schedule with a framing or memory choice and silently narrow what
        // the sweep covers. An absent byte starts at `Z_NO_FLUSH`, preserving the
        // original single-flush behaviour for a zero-length input.
        let flush_pick = usize::from(byte(8).unwrap_or(0)) % NON_FINAL_FLUSHES.len();

        let config = Self {
            level,
            strategy,
            deflate_window_bits,
            inflate_window_bits,
            mem_level,
            input_chunk,
            redispatch,
            params_level,
            params_strategy,
            flush_pick,
        };
        (config, payload)
    }
}

/// Maps a fuzzer byte onto one of the five deflate strategies with a total
/// `match`, so no cast or transmute of untrusted input is ever needed.
fn strategy_from_id(id: u8) -> Strategy {
    let strategy = match id % 5 {
        0 => Strategy::Default,
        1 => Strategy::Filtered,
        2 => Strategy::HuffmanOnly,
        3 => Strategy::Rle,
        _ => Strategy::Fixed,
    };
    // The arms are ordered by the `Z_*` strategy values, so the id doubles as
    // the C integer. Cross-checking against the crate's own conversion keeps the
    // mapping from silently drifting away from those values.
    debug_assert_eq!(Strategy::from_c_int(i32::from(id % 5)), Some(strategy));
    strategy
}

/// Selects a framing family the current build can actually encode with.
///
/// Raw and zlib are always available; gzip joins the rotation only when the
/// library was built with it, because `deflate_init2` correctly rejects a gzip
/// `windowBits` otherwise. Narrowing the modulus rather than branching later
/// keeps every path below framing-agnostic.
fn framing_from_id(id: u8) -> Framing {
    let total = if gzip_supported() { 3 } else { 2 };
    match usize::from(id) % total {
        0 => Framing::Raw,
        1 => Framing::Zlib,
        _ => Framing::Gzip,
    }
}

/// Picks a `windowBits` for `deflate_init2` inside the exact domain the engine
/// accepts for `family`: raw `-15..=-9`, zlib `8..=15`, gzip `25..=31`.
///
/// The lower bounds are not symmetric, and that is deliberate: only zlib framing
/// may request the 256-byte window (C `deflate.c` L436), so raw `-8` and gzip
/// `24` are excluded rather than swept and then explained away.
///
/// The window is counted *down* from [`MAX_WBITS`] so that `id == 0` selects the
/// 32 KiB window in every family — the same value C `deflateInit_` hands to
/// `deflateInit2_`. A single default byte therefore means "the zlib default
/// window" whichever framing was drawn, instead of landing mid-range for two of
/// the three.
fn encode_window_bits(family: Framing, id: u8) -> i32 {
    let min_bits = match family {
        Framing::Zlib => MIN_WBITS_ZLIB,
        Framing::Raw | Framing::Gzip => MIN_WBITS_WRAPPED,
    };
    let bits = MAX_WBITS - i32::from(id) % (MAX_WBITS - min_bits + 1);
    match family {
        Framing::Raw => -bits,
        Framing::Zlib => bits,
        Framing::Gzip => GZIP_WRAP_OFFSET + bits,
    }
}

/// Returns the window size, in bits, the **encoder** will actually use for
/// `window_bits`.
///
/// Mirrors C `deflateInit2_`: the sign is stripped for raw framing, the gzip
/// offset is subtracted for gzip framing, and a request for 8 is promoted to 9
/// (`deflate.c` L439, "until 256-byte window bug fixed"). That promotion is why
/// the decode window has to be derived rather than copied — a zlib stream
/// written with `windowBits = 8` records a 9-bit window in its CMF byte, and
/// decoding it with `windowBits = 8` is a legitimate `Z_DATA_ERROR`
/// ("invalid window size", C `inflate.c` L542).
fn encoder_window(window_bits: i32) -> i32 {
    let bits = if window_bits < 0 {
        -window_bits
    } else if window_bits > MAX_WBITS {
        window_bits - GZIP_WRAP_OFFSET
    } else {
        window_bits
    };
    if bits == MIN_WBITS_ZLIB {
        MIN_WBITS_WRAPPED
    } else {
        bits
    }
}

/// Picks a `windowBits` for `inflate_init2` that can read a stream produced with
/// `deflate_window_bits`, varying the decode framing across a few equivalent
/// choices.
///
/// The invariant every variant preserves is that the decoder's window is **at
/// least** the encoder's. A smaller decode window is not merely untidy: a back
/// reference beyond it is reported as "invalid distance too far back", which
/// would surface as a spurious finding on exactly those payloads whose match
/// distances happen to be long.
fn decode_window_bits(family: Framing, deflate_window_bits: i32, variant: u8) -> i32 {
    let effective = encoder_window(deflate_window_bits);
    match family {
        // Raw streams carry no header, so auto-detect has nothing to find and
        // `windowBits = 0` has no header to read the window from.
        Framing::Raw => match variant % 2 {
            0 => -effective,
            _ => -MAX_WBITS,
        },
        Framing::Zlib => match variant % 4 {
            0 => effective,
            1 => MAX_WBITS,
            // `0` is accepted by inflate only: it means "take the window size
            // from the stream's zlib header".
            2 => 0,
            // Auto-detect also decodes a plain zlib stream, but only a build
            // with gzip framing accepts the 40..=47 range at all.
            _ if gzip_supported() => WBITS_AUTO,
            _ => MAX_WBITS,
        },
        // Reached only when `gzip_supported()` held while the family was
        // selected, so every arm here is a framing this build accepts.
        Framing::Gzip => match variant % 3 {
            0 => effective + GZIP_WRAP_OFFSET,
            1 => MAX_WBITS + GZIP_WRAP_OFFSET,
            _ => WBITS_AUTO,
        },
    }
}

// ===========================================================================
// Round-trip legs
// ===========================================================================

/// The one-call, zlib-framed round trip that runs on every execution as the
/// baseline leg: `compress_bound` sizing, then `compress2` followed by
/// `uncompress`, asserting exact recovery.
///
/// # Why only `Z_MEM_ERROR` is tolerated
///
/// Every input to `compress2` here is already known-good: `level` came from
/// [`Config::from_input`], which maps a fuzzer byte onto `-1..=9` and therefore
/// cannot produce an out-of-range level, and the destination is sized by
/// `compress_bound`, which is zlib's own exact worst case — so `Z_BUF_ERROR` is
/// unreachable. That leaves heap exhaustion as the single legitimate failure,
/// and it is an environment condition under the fuzzer's `-rss_limit_mb`
/// ceiling rather than a defect. Discarding any other error here would let a
/// build in which *every* `compress2` call failed still run this leg to
/// completion without a single assertion firing.
fn one_call_round_trip(level: i32, payload: &[u8]) {
    // `compress_bound` is the exact zlib worst-case sizing, so this never errors
    // for lack of room.
    let mut compressed = vec![0u8; compress_bound(payload.len())];
    // Encoder success is REQUIRED, not hoped for. Both refusals `compress2`
    // documents are unreachable here: the level came from `Config::from_input`,
    // which yields only `-1..=9`, and the destination is exactly
    // `compress_bound`-sized. Returning on an arbitrary error would skip the two
    // assertions below, which are the entire content of this leg — the harness
    // would report a pass having compared nothing.
    let n = match compress2(&mut compressed, payload, level) {
        Ok(n) => n,
        // The one refusal that is a property of the host rather than the library,
        // isolated by exact code so no other error can travel with it. libFuzzer
        // runs with `-rss_limit_mb`, so a refused state reservation is a real
        // possibility and reporting it as a crash would waste the finding.
        //
        // Note the error type: the one-call wrappers report a bare `ReturnCode`,
        // whereas `deflate_init2` below reports a `ZlibError`.
        Err(ReturnCode::MemError) => return,
        Err(other) => panic!(
            "compress2 refused a {}-byte payload at level {level} with {other:?}; \
             the level is inside -1..=9 and the destination is the exact \
             {}-byte compress_bound, so Z_MEM_ERROR is the only refusal in \
             contract here",
            payload.len(),
            compressed.len()
        ),
    };

    // Decompress into a buffer sized to the exact original length. `max(1)`
    // avoids a zero-length output buffer on the empty-payload edge.
    let mut restored = vec![0u8; payload.len().max(1)];
    match uncompress(&mut restored, &compressed[..n]) {
        Ok(m) => {
            assert_eq!(m, payload.len(), "round-trip length mismatch");
            assert_eq!(&restored[..m], payload, "round-trip content mismatch");
        }
        Err(e) => panic!("round-trip decode of self-produced stream failed: {e:?}"),
    }
}

/// Doubles `output` toward `ceiling` so the next `deflate` pass has more room,
/// panicking if the ceiling has already been reached.
///
/// Growing rather than bailing out is what keeps the round-trip assertion
/// reachable: an encoder starved of output space has not failed, it has simply
/// been given too little room, and the harness owns that. Reaching the ceiling is
/// a different matter — the ceiling is never below the engine's own
/// `deflate_bound` for this payload, so needing more than that much room to
/// finish is a library defect and must be reported as one.
///
/// `output.len()` is always at least `1` (the constructor uses `capacity.max(1)`)
/// and always strictly below `ceiling` when this returns, so the doubling is
/// guaranteed to make progress and the caller's loop cannot spin.
fn grow_output(output: &mut Vec<u8>, ceiling: usize, out_pos: usize, why: &str, config: &Config) {
    assert!(
        output.len() < ceiling,
        "deflate could not reach Z_STREAM_END within the {ceiling}-byte output \
         ceiling ({why}; {out_pos} bytes produced so far) ({config:?})"
    );
    let grown = output.len().saturating_mul(2).min(ceiling);
    debug_assert!(grown > output.len(), "growth must make progress");
    output.resize(grown, POISON);
}

/// Compresses `payload` through the streaming engine at the full four-axis
/// configuration in `config`, returning the complete stream.
///
/// `input_chunk` limits how much input a single pass may consume (`None` feeds
/// everything at once, the shape `deflate_bound` is specified for). `params`,
/// when set, re-dispatches level and strategy mid-stream through
/// `deflate_params` exactly once.
///
/// # Return value and panic policy
///
/// This function returns a stream driven to `Z_STREAM_END` or it panics. There is
/// **exactly one** `return None` in its body — a refused state reservation, which
/// is a property of the host under libFuzzer's `-rss_limit_mb` ceiling and not of
/// the library. That single exit is the whole meaning of the `Option`.
///
/// That narrowness is the point. Each parameter is clamped into its accepted
/// domain by [`Config::from_input`] *before* the call and the output buffer is
/// grown on demand up to a ceiling that is never below the engine's own
/// `deflate_bound`, so once initialization succeeds this leg is driving the
/// encoder entirely within its documented contract — and an encoder that cannot
/// compress its own valid configuration is a library defect however arbitrary the
/// payload bytes were.
///
/// It previously had four more `None` exits: a spent pass budget, a `Z_BUF_ERROR`
/// from `deflate_params`, the output-growth ceiling, and a pass that made no
/// progress. Every one of them was indistinguishable to the caller from the
/// allocation case, and the caller answered all five by skipping
/// `assert_round_trip` entirely — so a configuration that could not be driven to
/// completion was recorded as a pass having compared nothing. A build in which
/// every `deflate` call stalled forever would have burned its whole pass budget on
/// every execution and still reported success. They are now handled properly
/// instead of reported identically:
///
/// * `Z_BUF_ERROR` from `deflate_params` is **recoverable and retried**. `zlib.h`
///   says so directly — the parameters are left unchanged and the call may be
///   repeated with more output space — so the harness grows the buffer and repeats
///   it rather than abandoning the stream.
/// * A pass that neither consumes nor produces while output room remains is the
///   `deflate_stored` shape C documents (`deflate.c` L1689-L1748): it wants more
///   room than the tail of the buffer offers. That is also answered by growing,
///   and only a run of [`MAX_STALLED_PASSES`] consecutive stalls — which more room
///   provably cannot fix — is reported.
/// * The pass budget and the output ceiling are now assertions. Both are sized far
///   above anything a `-max_len=65536` input can legitimately need, so reaching
///   either is a finding rather than a reason to stop looking.
fn deflate_stream(
    config: &Config,
    payload: &[u8],
    input_chunk: Option<usize>,
    params: Option<(i32, Strategy)>,
) -> Option<Vec<u8>> {
    let single_pass = input_chunk.is_none() && params.is_none();

    let mut strm = ZStream::new();
    match deflate_init2(
        &mut strm,
        config.level,
        Z_DEFLATED,
        config.deflate_window_bits,
        config.mem_level,
        config.strategy,
    ) {
        Ok(_) => {}
        // Heap exhaustion is an environment condition under the fuzzer's
        // `-rss_limit_mb` ceiling, not a defect.
        Err(ZlibError::MemError) => return None,
        Err(other) => {
            panic!("deflate_init2 rejected an in-range configuration: {other:?} ({config:?})")
        }
    }

    // `deflate_bound` is the engine's own worst-case sizing for a single-pass
    // Z_FINISH deflate, wrapper included, so the single-pass leg needs no slack
    // at all. The chunked and re-dispatched legs emit extra block boundaries and,
    // now, a flush marker on most passes, none of which the single-pass bound
    // covers — so they start from a generous multiple instead. Both grow on
    // demand below, and no size is derived from a fuzzer-controlled multiplier.
    let bound = deflate_bound(&strm, payload.len());
    let capacity = if single_pass {
        bound
    } else {
        bound
            .saturating_add(compress_bound(payload.len()))
            .saturating_add(payload.len())
            .saturating_add(payload.len().saturating_mul(FLUSH_MARKER_ALLOWANCE))
    };

    // The growth ceiling is `OUTPUT_GROWTH_LIMIT` *or the starting capacity if
    // that is already larger*, never the smaller of the two. Clamping the
    // starting buffer down to the limit would hand the engine less room than
    // `deflate_bound` says it needs and then blame it for not finishing — a
    // harness bug wearing a library bug's clothes. `-max_len=65536` keeps the
    // payload far below the limit today, so this is about the invariant holding
    // if that budget is ever raised, not about current behaviour.
    let ceiling = capacity.max(OUTPUT_GROWTH_LIMIT);
    let mut output = vec![POISON; capacity.max(1)];

    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut passes = 0u32;
    let mut stalled_passes = 0u32;
    let mut params_pending = params.is_some();
    // Position in the non-final flush rotation, advanced only after a flush has
    // completed (see `Config::non_final_flush`).
    let mut flush_step = 0usize;
    // Set to the flush value of a pass that returned with `avail_out == 0`, which
    // zlib requires be repeated with that same value until it completes.
    let mut incomplete_flush: Option<i32> = None;

    let stream = loop {
        passes = passes.saturating_add(1);
        // The work budget is an assertion, not an exit — a hang detector rather
        // than an escape hatch. Each pass either offers at least one input byte,
        // grows the output buffer (a doubling, so at most about twenty times), or
        // completes a flush, and the chunked leg is capped at
        // `CHUNKED_PAYLOAD_LIMIT` bytes, so roughly half this budget is the true
        // ceiling for a `-max_len=65536` input. Bounding the loop is what keeps a
        // hang a *library* finding rather than a harness bug; failing the bound is
        // itself the finding.
        assert!(
            passes <= MAX_PASSES,
            "deflate exceeded {MAX_PASSES} passes on a {}-byte payload without \
             reaching Z_STREAM_END (in_pos {in_pos}, out_pos {out_pos}, output \
             capacity {}, {config:?})",
            payload.len(),
            output.len()
        );

        // Every engine call below requires at least one byte of output room:
        // `deflate` answers `Z_BUF_ERROR` for `avail_out == 0` on entry, and
        // `deflate_params`' internal `deflate(Z_BLOCK)` does the same. Restoring
        // the invariant once, here, keeps it in a single place instead of on every
        // path that can advance `out_pos` to the end of the buffer — which is how
        // a `Z_BUF_ERROR` that looks like a library defect gets manufactured.
        if out_pos == output.len() {
            grow_output(
                &mut output,
                ceiling,
                out_pos,
                "the drain loop needed room before the next engine call",
                config,
            );
        }

        let in_end = match input_chunk {
            Some(chunk) => payload.len().min(in_pos.saturating_add(chunk)),
            None => payload.len(),
        };

        // Re-dispatch once, genuinely mid-stream: after at least one chunk has
        // been compressed, so a block is already in flight, but while input still
        // remains and no flush is outstanding.
        //
        // Both of the extra conditions are contract requirements, not tidiness.
        // `in_pos < payload.len()` keeps the switch strictly before `Z_FINISH` is
        // ever sent: `deflate_params` performs an internal `deflate(Z_BLOCK)`, and
        // `deflate` answers `Z_STREAM_ERROR` for any flush other than `Z_FINISH`
        // once the stream has entered `FINISH_STATE` — so re-dispatching after the
        // finish began would manufacture a `Z_STREAM_ERROR` and report the
        // harness's own sequencing mistake as a library defect. It also guarantees
        // the slice handed to `deflate_params` is non-empty, which is the shape a
        // mid-stream re-dispatch has. `incomplete_flush.is_none()` respects the
        // rule that a flush which returned with `avail_out == 0` must be repeated
        // before anything else is asked of the stream.
        //
        // If the re-dispatch's own internal flush happens to consume the rest of
        // the input, the switch simply does not occur for that input; it is an
        // extra coverage axis, never an oracle, so every round-trip assertion
        // below still runs in full.
        if params_pending && in_pos > 0 && in_pos < payload.len() && incomplete_flush.is_none() {
            if let Some((level, strategy)) = params {
                let outcome = deflate_params(
                    &mut strm,
                    &payload[in_pos..in_end],
                    &mut output[out_pos..],
                    level,
                    strategy,
                );
                in_pos = in_pos.saturating_add(outcome.consumed);
                out_pos = out_pos.saturating_add(outcome.produced);
                match outcome.code {
                    ReturnCode::Ok => {
                        params_pending = false;
                        continue;
                    }
                    // C `deflateParams` documents `Z_BUF_ERROR` when the internal
                    // `deflate(Z_BLOCK)` pre-flush could not drain the pending
                    // block into the output buffer. `zlib.h` also documents the
                    // recovery: the stream is untouched and the new parameters were
                    // not applied, so the call may simply be repeated with more
                    // output space. Growing and retrying (`params_pending` stays
                    // set, so the retry actually happens) keeps the stream alive so
                    // its round-trip assertions still run, where abandoning the leg
                    // silently dropped both the re-dispatch coverage and the
                    // round-trip assertion. Growth is bounded by `ceiling` and the
                    // whole loop by `MAX_PASSES`, so this cannot spin.
                    ReturnCode::BufError => {
                        grow_output(
                            &mut output,
                            ceiling,
                            out_pos,
                            "deflate_params could not flush the pending block",
                            config,
                        );
                        continue;
                    }
                    other => panic!(
                        "deflate_params rejected an in-range re-dispatch to \
                         level {level} / {strategy:?}: {other:?} ({config:?})"
                    ),
                }
            }
            params_pending = false;
        }

        // The last pass finishes the stream; every earlier one draws from the
        // rotation, except while a previous flush is still incomplete.
        let flush = match incomplete_flush {
            Some(pending) => pending,
            None if in_end == payload.len() => Z_FINISH,
            None => config.non_final_flush(flush_step),
        };
        let outcome = deflate(
            &mut strm,
            &payload[in_pos..in_end],
            &mut output[out_pos..],
            flush,
        );
        in_pos = in_pos.saturating_add(outcome.consumed);
        out_pos = out_pos.saturating_add(outcome.produced);

        match outcome.code {
            ReturnCode::StreamEnd => {
                output.truncate(out_pos);
                break output;
            }
            ReturnCode::Ok => {
                // `avail_out == 0` on return means the flush did not finish, so
                // the next call must repeat this exact flush value with more room.
                let out_full = out_pos == output.len();
                let stalled = outcome.consumed == 0 && outcome.produced == 0;

                if stalled {
                    stalled_passes = stalled_passes.saturating_add(1);
                    // A stall while output room remains is the `deflate_stored`
                    // shape C documents: level 0 returns before copying anything
                    // when the remaining room cannot hold even a stored-block
                    // header, and `deflate` still reports Z_OK. The engine wants
                    // more room than the tail of the buffer offers, so growing
                    // answers it. A RUN of stalls survives every growth up to the
                    // ceiling, so more room demonstrably is not the problem and the
                    // engine is stuck — itself the finding, not a reason to abandon
                    // the leg and skip the round-trip assertion.
                    assert!(
                        stalled_passes <= MAX_STALLED_PASSES,
                        "deflate made no progress on {MAX_STALLED_PASSES} \
                         consecutive passes with flush {flush} (in_pos {in_pos} of \
                         {}, out_pos {out_pos} of {}, {config:?})",
                        payload.len(),
                        output.len()
                    );
                } else {
                    stalled_passes = 0;
                }

                if out_full {
                    incomplete_flush = Some(flush);
                } else {
                    incomplete_flush = None;
                    if flush != Z_FINISH {
                        // A completed non-final flush advances the rotation, so
                        // one stream sees several flush shapes.
                        flush_step = flush_step.wrapping_add(1);
                    }
                }

                if out_full || stalled {
                    grow_output(
                        &mut output,
                        ceiling,
                        out_pos,
                        if out_full {
                            "the destination filled before Z_STREAM_END"
                        } else {
                            "a pass consumed and produced nothing"
                        },
                        config,
                    );
                }
            }
            other => {
                panic!("deflate returned {other:?} for an in-range configuration ({config:?})")
            }
        }
    };

    // Mirror the C `deflateEnd` contract, exactly as a C caller must. Ownership
    // would release the engine buffers anyway (including when a panic above
    // unwinds), so this is about the return code, not the memory.
    let end = deflate_end(&mut strm);
    // The loop leaves only one way out — `Z_STREAM_END` — so this is
    // unconditional. A stream driven to Z_STREAM_END has left BUSY_STATE, so C
    // `deflateEnd` returns Z_OK; its Z_DATA_ERROR arm applies to a premature end,
    // and no path now ends prematurely without panicking first. Anything but Z_OK
    // here is therefore a defect.
    assert_eq!(
        end,
        Ok(ReturnCode::Ok),
        "deflate_end must report Z_OK after a completed stream, not {end:?} ({config:?})"
    );
    if single_pass {
        // The documented contract of `deflateBound`: an upper bound on a
        // single-pass Z_FINISH deflate of this many input bytes.
        assert!(
            stream.len() <= bound,
            "single-pass deflate produced {} bytes, above the {bound}-byte \
             deflate_bound for {} input bytes ({config:?})",
            stream.len(),
            payload.len(),
        );
    }
    Some(stream)
}

/// Decompresses `stream` — which this harness produced itself — and asserts
/// byte-for-byte recovery of `payload`.
///
/// Every non-terminal outcome is a finding here, precisely because the input is
/// a complete, self-produced stream rather than arbitrary bytes: a decode
/// failure, a stall, or a byte mismatch all mean the library broke. `leg` names
/// the producing leg so a crash report identifies it immediately.
fn assert_round_trip(config: &Config, payload: &[u8], stream: &[u8], leg: &str) {
    let mut strm = ZStream::new();
    match inflate_init2(&mut strm, config.inflate_window_bits) {
        Ok(_) => {}
        // Heap exhaustion is an environment condition, as on the encode leg.
        Err(ZlibError::MemError) => return,
        Err(other) => panic!(
            "inflate_init2 rejected the derived decode framing: {other:?} (leg {leg}, {config:?})"
        ),
    }

    // One spare byte beyond the payload: the decoder needs room to reach
    // Z_STREAM_END on its final pass, and a decoder that emits *more* than the
    // payload is then caught by the length assertion below instead of being
    // masked as a full-buffer Z_BUF_ERROR. Poisoned so a short write cannot
    // satisfy the content assertion by accident.
    let mut restored = vec![POISON; payload.len().saturating_add(1)];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut passes = 0u32;

    loop {
        passes = passes.saturating_add(1);
        assert!(
            passes <= MAX_PASSES,
            "inflate exceeded {MAX_PASSES} passes on a self-produced stream \
             (leg {leg}, {config:?})"
        );

        let outcome = inflate(
            &mut strm,
            &stream[in_pos..],
            &mut restored[out_pos..],
            Z_NO_FLUSH,
        );
        in_pos = in_pos.saturating_add(outcome.consumed);
        out_pos = out_pos.saturating_add(outcome.produced);

        match outcome.code {
            ReturnCode::StreamEnd => break,
            ReturnCode::Ok => assert!(
                outcome.consumed != 0 || outcome.produced != 0,
                "inflate stalled before Z_STREAM_END on a self-produced stream \
                 (leg {leg}, {config:?})"
            ),
            other => panic!(
                "round-trip decode of self-produced stream failed: {other:?} \
                 (leg {leg}, {config:?})"
            ),
        }
    }

    let end = inflate_end(&mut strm);
    // Reaching Z_STREAM_END must mean the WHOLE stream was read. Checking only the
    // decoded length and content leaves a stream that the decoder finished early
    // indistinguishable from one it read to the end: an encoder that appended
    // bytes past the trailer, or a decoder that stopped short of it, would
    // reproduce the payload perfectly and pass unnoticed. That matters most for
    // the raw framing, which has no trailer to run out of, and for the flush
    // markers the chunked leg now emits, whose bytes must all be consumed.
    assert_eq!(
        in_pos,
        stream.len(),
        "inflate reported Z_STREAM_END with {} of {} stream bytes unread \
         (leg {leg}, {config:?})",
        stream.len() - in_pos,
        stream.len()
    );
    assert_eq!(
        out_pos,
        payload.len(),
        "round-trip length mismatch (leg {leg}, {config:?})"
    );
    assert_eq!(
        &restored[..out_pos],
        payload,
        "round-trip content mismatch (leg {leg}, {config:?})"
    );
    assert_eq!(
        end,
        Ok(ReturnCode::Ok),
        "inflate_end must report Z_OK after a completed stream, not {end:?} \
         (leg {leg}, {config:?})"
    );
}

fuzz_target!(|data: &[u8]| {
    // The configuration comes from a short fixed header so the whole grid is
    // reachable from the very first bytes of a cold corpus; the rest is payload.
    let (config, payload) = Config::from_input(data);

    // Leg 1: the one-call, zlib-framed round trip, varying the level only.
    one_call_round_trip(config.level, payload);

    // Leg 2: the four-axis sweep as a single-pass Z_FINISH deflate — the shape
    // `deflate_bound` is specified for, so the bound is asserted there too.
    //
    // `deflate_stream` now yields `None` for exactly one reason: the host refused
    // a state reservation. So this is not a skip of the comparison below — every
    // other outcome either produced a completed stream or already panicked inside.
    if let Some(stream) = deflate_stream(&config, payload, None, None) {
        assert_round_trip(&config, payload, &stream, "single-pass");
    }

    // Leg 3: the same configuration under small-chunk input pressure and the full
    // non-final flush rotation, which together drive the engine's
    // partial-progress and flush-marker paths, optionally switching level and
    // strategy mid-stream. Restricted to short payloads to keep `exec/s` high.
    if payload.len() <= CHUNKED_PAYLOAD_LIMIT {
        let params = config
            .redispatch
            .then_some((config.params_level, config.params_strategy));
        if let Some(stream) = deflate_stream(&config, payload, Some(config.input_chunk), params) {
            assert_round_trip(&config, payload, &stream, "chunked");
        }
    }
});
