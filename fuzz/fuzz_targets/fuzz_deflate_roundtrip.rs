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
//! Those are exactly the axes on which byte-identity against reference zlib was
//! proven, so keeping them reachable from the first few input bytes lets the
//! fuzzer steer straight into the interesting configurations from a cold, empty
//! corpus.
//!
//! # What this harness does and does not prove
//!
//! It proves **losslessness**: every configuration must decode back to the exact
//! input bytes. It deliberately makes **no** assertion about the compressed
//! bytes themselves and bakes in no expected-output vector — byte-identity
//! against reference zlib is proven separately by the always-on oracle vectors
//! in `tests/interop.rs`, and a fuzz finding here never authorises a change to a
//! match-finder heuristic. Compressed output legitimately differs between the
//! whole-buffer and chunked legs below, because C's `deflate_stored` consults
//! `avail_out` when it sizes stored blocks (`deflate.c` L1689-L1748); that is
//! not a defect. For the same reason no gzip framing byte is inspected: byte 10
//! of a gzip header is `OS_CODE`, a compile-time platform choice.
//!
//! # Panic policy: a panic must mean the LIBRARY broke, never the harness
//!
//! Unlike a test, a fuzz target is fed arbitrary bytes, so a rejection or a
//! stalled pass can be a legitimate outcome. Every fuzzer-derived parameter is
//! therefore clamped into the exact domain C `deflateInit2_` accepts *before*
//! the call, and the drain loops bail out gracefully on the outcomes C
//! documents as legitimate (see [`deflate_stream`]). What remains is asserted
//! hard: a complete stream this harness produced itself must decode, and it must
//! decode to the original bytes. Every loop is explicitly bounded, because a
//! hang is as much a finding as a crash — but an unbounded *harness* loop would
//! be a harness bug.

use libfuzzer_sys::fuzz_target;

// Only the public, safe `zlib_rs` surface is used: the crate-root re-exports for
// the one-call wrappers and the shared types, plus the public engine modules for
// the streaming drivers and the flush / method / window / memory constants. The
// library's C-ABI boundary module is deliberately never reached from here, which
// is what keeps this harness entirely within safe Rust.
use zlib_rs::constants::{
    DEF_MEM_LEVEL, GZIP_WRAP_OFFSET, MAX_MEM_LEVEL, MAX_WBITS, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH,
};
use zlib_rs::deflate::{deflate, deflate_bound, deflate_end, deflate_init2, deflate_params};
use zlib_rs::inflate::{inflate, inflate_end, inflate_init2};
use zlib_rs::{
    ReturnCode, Strategy, ZStream, ZlibError, compress_bound, compress2, uncompress,
    zlib_compile_flags,
};

// ===========================================================================
// Harness limits
// ===========================================================================

/// Number of configuration bytes consumed from the front of the fuzz input; the
/// remainder is the payload. Kept small and fixed so a cold corpus reaches every
/// configuration immediately.
const HEADER_LEN: usize = 8;

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
/// inflate engine accepts only when gzip framing is built in — hence it is used
/// only behind [`gzip_supported`].
const WBITS_AUTO: i32 = MAX_WBITS + 32;

/// Whether the linked `zlib-rs` was built with gzip framing.
///
/// `gzip` is a feature of the **`zlib-rs`** crate, not of this harness crate, so
/// a `#[cfg(feature = "gzip")]` written here could never observe it: `cargo`
/// resolves `feature` against this crate's own manifest, which declares none, and
/// rustc says exactly that through its `unexpected_cfgs` lint. The library
/// instead advertises the answer through the very mechanism a C consumer uses —
/// `zlibCompileFlags` bit 17 is `NO_GZIP`, set precisely when gzip framing is
/// absent — so reading it is both correct and idiomatic from outside the crate.
/// The gzip rows are therefore selected at run time rather than compiled away,
/// which keeps a single binary correct under either feature set.
///
/// This is also why the file carries no whole-file inner `#![cfg(...)]`: such an
/// attribute would compile away the entry-point macro invocation at the bottom of
/// this file, and with it `main`, leaving a `[[bin]]` that cannot link.
fn gzip_supported() -> bool {
    /// `zlibCompileFlags` bit 17, "no gzip framing in this build".
    const NO_GZIP: u32 = 1 << 17;
    zlib_compile_flags() & NO_GZIP == 0
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
}

impl Config {
    /// Splits `data` into a fixed-size configuration header and the payload.
    ///
    /// Every byte is read through `Option`, so a short or empty input still
    /// yields a usable configuration: an absent level byte keeps the original
    /// harness behaviour of level 6 over an empty payload, and the remaining
    /// fallbacks are the zlib / max-window / `DEF_MEM_LEVEL` defaults.
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

/// The original one-call, zlib-framed round trip, preserved as the always-run
/// baseline leg: `compress_bound` sizing, then `compress2` followed by
/// `uncompress`, asserting exact recovery.
fn one_call_round_trip(level: i32, payload: &[u8]) {
    // `compress_bound` is the exact zlib worst-case sizing, so this never errors
    // for lack of room.
    let mut compressed = vec![0u8; compress_bound(payload.len())];
    let n = match compress2(&mut compressed, payload, level) {
        Ok(n) => n,
        Err(_) => return,
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
/// `Some` is a stream driven to `Z_STREAM_END`; `None` means the pass could not
/// be completed for a reason C documents as legitimate, so there is nothing to
/// decode. Each such bail-out is annotated at its site. Everything else panics,
/// because the parameters were clamped into the accepted domain before the call
/// and the output buffer is grown on demand: a hard error under those conditions
/// is a library defect, not an artefact of arbitrary input.
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
    // at all. The chunked and re-dispatched legs emit extra block boundaries
    // that the single-pass bound does not cover, so they start from a generous
    // multiple instead; both grow on demand below, and neither size is derived
    // from a fuzzer-controlled multiplier.
    let bound = deflate_bound(&strm, payload.len());
    let capacity = if single_pass {
        bound
    } else {
        bound
            .saturating_add(compress_bound(payload.len()))
            .saturating_add(payload.len())
    };
    let mut output = vec![POISON; capacity.clamp(1, OUTPUT_GROWTH_LIMIT)];

    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut passes = 0u32;
    let mut params_pending = params.is_some();

    let stream = loop {
        passes = passes.saturating_add(1);
        if passes > MAX_PASSES {
            // Work budget for this input is spent. Bounding the loop here is
            // what keeps a hang a *library* finding rather than a harness bug.
            break None;
        }

        let in_end = match input_chunk {
            Some(chunk) => payload.len().min(in_pos.saturating_add(chunk)),
            None => payload.len(),
        };

        // Re-dispatch once, mid-stream, after at least one chunk has been
        // compressed, so the switch happens with a block already in flight.
        if params_pending && in_pos > 0 {
            params_pending = false;
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
                    ReturnCode::Ok => continue,
                    // C `deflateParams` documents Z_BUF_ERROR when the pending
                    // pre-flush did not fit in the output buffer.
                    ReturnCode::BufError => break None,
                    other => panic!(
                        "deflate_params rejected an in-range re-dispatch to \
                         level {level} / {strategy:?}: {other:?} ({config:?})"
                    ),
                }
            }
        }

        let flush = if in_end == payload.len() {
            Z_FINISH
        } else {
            Z_NO_FLUSH
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
                break Some(output);
            }
            ReturnCode::Ok => {
                if out_pos == output.len() {
                    // Ran out of room before finishing: grow, up to the ceiling.
                    if output.len() >= OUTPUT_GROWTH_LIMIT {
                        break None;
                    }
                    let grown = output.len().saturating_mul(2).clamp(1, OUTPUT_GROWTH_LIMIT);
                    output.resize(grown, POISON);
                } else if outcome.consumed == 0 && outcome.produced == 0 {
                    // A pass that neither consumes nor produces is legitimate
                    // here: C `deflate_stored` (level 0) returns before copying
                    // anything when the remaining output cannot even hold a
                    // stored-block header, and `deflate` then reports Z_OK
                    // because `avail_out` is non-zero.
                    break None;
                }
            }
            other => {
                panic!("deflate returned {other:?} for an in-range configuration ({config:?})")
            }
        }
    };

    // Mirror the C `deflateEnd` contract on every path, error paths included.
    // Ownership would release the engine buffers anyway (including when a panic
    // above unwinds), so this is about the return code, not the memory.
    let end = deflate_end(&mut strm);
    if let Some(stream) = stream {
        // A stream driven to Z_STREAM_END has left BUSY_STATE, so C
        // `deflateEnd` returns Z_OK. Its Z_DATA_ERROR arm applies to a premature
        // end, which is exactly the bail-out paths above, hence the guard.
        assert!(
            end.is_ok(),
            "deflate_end failed after a completed stream: {end:?} ({config:?})"
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
        return Some(stream);
    }
    None
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
    assert!(
        end.is_ok(),
        "inflate_end failed after a completed stream: {end:?} (leg {leg}, {config:?})"
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
    if let Some(stream) = deflate_stream(&config, payload, None, None) {
        assert_round_trip(&config, payload, &stream, "single-pass");
    }

    // Leg 3: the same configuration under small-chunk input pressure, which
    // drives the engine's partial-progress paths, optionally switching level and
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
