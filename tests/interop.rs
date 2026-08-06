//! Byte-identity and wire-format cross-validation for `zlib-rs`.
//!
//! This integration test is the authoritative **byte-identity and wire-format
//! compatibility** gate for `zlib-rs`. Its mandate comes from four specific
//! places:
//!
//! * user **constraint 1** — "output must be binary-compatible with
//!   zlib-produced streams";
//! * **AAP §0.8.1 directive D-1** — compressed output must remain byte-for-byte
//!   identical to reference zlib, read in its strong form;
//! * **AAP §0.7.2 plan-adopted standard S3** — "bit-exactness is a release gate,
//!   not an aspiration", which is what makes tier 1 below always-on and
//!   C-toolchain-free;
//! * **AAP §0.6.4** (the eight decision points that determine the emitted bytes)
//!   and **§0.6.7** (official test-vector conformance), which supply the
//!   technical content the two tiers assert.
//!
//! It proves two distinct, complementary properties, in two tiers:
//!
//! # Testing strategy
//!
//! 1. **Strict byte-identity (always-on, release gate).** The crate's compressed
//!    output must be **byte-for-byte identical** to the reference C zlib
//!    (`1.3.2.1-motley`) for the same input, level, strategy, and framing — the
//!    defining acceptance criterion of the migration (user constraint 1; AAP
//!    §0.8.1 directive D-1). This is enforced by the [`byte_identity`] module
//!    below against **deterministic oracle vectors** baked from the genuine C
//!    encoder (via `deflateInit2` + `deflate(Z_FINISH)`). The vectors span every
//!    compression level (`-1..=9`), all five deflate strategies, all three
//!    `memLevel` values (1 / 8 / 9), and the zlib / raw / gzip / small-window
//!    framings. Because the reference bytes are precomputed constants, this gate
//!    runs **by default in CI with no C toolchain**.
//!
//! 2. **Decode-compatibility (always-on).** The crate's zlib (RFC 1950), raw
//!    DEFLATE (RFC 1951), and gzip (RFC 1952) streams must interoperate with an
//!    independently-authored codec — the dev-dependency [`flate2`], built with
//!    its **default `miniz_oxide` backend** (pure Rust, no C toolchain). Both
//!    directions are asserted for every framing (`flate2` decodes what `zlib-rs`
//!    produced, and `zlib-rs` decodes what `flate2` produced), across every
//!    level and strategy. Because `miniz_oxide` is a *different* encoder with
//!    different match-finding heuristics, these tests prove RFC wire-format
//!    conformance but are **not** treated as satisfying byte-identity; that
//!    property is proven exclusively by tier 1.
//!
//! All assertions are black-box over the public `zlib_rs` API. No FFI entry
//! point, raw pointer, or C-ABI shim is reached from this file, and no
//! compiler-escape construct appears anywhere in it — the whole gate is written
//! in ordinary checked Rust against the crate's safe surface (AAP §0.6.2).

// --- Public `zlib-rs` surface (crate-root re-exports + public engine modules) -
// One-call whole-buffer wrappers, the error/strategy enums, and the streaming
// stream handle are crate-root re-exports. The `deflate`/`inflate` streaming
// drivers and the flush/method constants live in the crate's public engine
// modules (`pub mod deflate`, `pub mod inflate`, `pub mod constants`); the raw
// and gzip framings are only reachable through that streaming API, since the
// one-call `compress`/`uncompress` wrappers are zlib-framed only.
use zlib_rs::constants::{DEF_MEM_LEVEL, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH};
use zlib_rs::deflate::{deflate, deflate_end, deflate_init2};
use zlib_rs::inflate::{inflate, inflate_end, inflate_init2};
use zlib_rs::{ReturnCode, Strategy, ZStream, compress_bound, compress2, crc32, uncompress};

// --- Reference implementation (`flate2`, default `miniz_oxide` backend) -------
use flate2::Compression;
#[cfg(feature = "gzip")]
use flate2::read::GzDecoder;
use flate2::read::{DeflateDecoder, ZlibDecoder};
#[cfg(feature = "gzip")]
use flate2::write::GzEncoder;
use flate2::write::{DeflateEncoder, ZlibEncoder};
use std::io::{Read, Write};

// ===========================================================================
// windowBits framing selectors (AAP §0.6.4 overloading contract) + sizes
// ===========================================================================

/// zlib wrapper (RFC 1950): 2-byte header + trailing Adler-32.
const WBITS_ZLIB: i32 = 15;
/// Raw DEFLATE (RFC 1951): no wrapper, no checksum.
const WBITS_RAW: i32 = -15;
/// zlib wrapper at the smallest permitted window: 512 bytes (`windowBits = 9`).
const WBITS_ZLIB_SMALL: i32 = 9;
/// Raw DEFLATE at the smallest permitted window: 512 bytes
/// (`windowBits = -9`).
///
/// The lower end of the raw range is where the overloading contract is easiest
/// to get wrong — `-9` must resolve to raw framing with a 512-byte window, not
/// be rejected and not be confused with zlib `9`.
const WBITS_RAW_SMALL: i32 = -9;
/// gzip wrapper (RFC 1952): gzip header + trailing CRC-32 and length.
#[cfg(feature = "gzip")]
const WBITS_GZIP: i32 = 31;
/// Auto-detect a zlib or gzip wrapper on inflate (`32 + 15`).
///
/// Auto-detection lives in the `40..=47` windowBits range, which the inflate
/// engine only accepts when gzip support is compiled in (`inflate_reset2`
/// masks the request with `& 15` under `#[cfg(feature = "gzip")]`; without it
/// the value fails the `8..=15` bounds test, matching C built without
/// `GUNZIP`). The constant — and the tests that use it — are therefore gated
/// on the `gzip` feature.
#[cfg(feature = "gzip")]
const WBITS_AUTO: i32 = 47;

/// Large-buffer size mirroring `test/example.c`'s `uncomprLen = 20000`.
const LARGE_LEN: usize = 20_000;

/// The canonical zlib exerciser literal from `test/example.c` (`hello[]`).
const HELLO: &[u8] = b"hello, hello!";

/// Every `zlib-rs` compression level: the ten explicit levels plus the
/// `Z_DEFAULT_COMPRESSION` sentinel (`-1`).
const ZLIB_RS_LEVELS: [i32; 11] = [-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

// ===========================================================================
// Test-data generators — a representative spread of compressibility profiles
// ===========================================================================

/// A run of `n` zero bytes: maximally compressible (mirrors the zero-filled
/// large-buffer cases in `test/example.c`'s `test_large_*`).
fn zeros(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

/// `n` bytes of repeated natural-language text: moderately compressible, with
/// abundant back-references for the match finder to exploit.
fn repetitive(n: usize) -> Vec<u8> {
    const PHRASE: &[u8] = b"The quick brown fox jumps over the lazy dog. ";
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let take = (n - out.len()).min(PHRASE.len());
        out.extend_from_slice(&PHRASE[..take]);
    }
    out
}

/// `n` bytes of deterministic high-entropy pseudo-random data: effectively
/// incompressible, exercising the stored-block fallback.
///
/// A self-contained `xorshift64*` generator with a fixed seed is used instead
/// of the `rand` crate so the corpus is perfectly reproducible and the test has
/// no dependency on any external RNG API surface.
fn incompressible(n: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        // xorshift64* — a well-distributed, fully deterministic sequence.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let mixed = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        out.push((mixed >> 24) as u8);
    }
    out
}

/// The representative input corpus shared by the interop tests: the canonical
/// literal, an empty buffer (streaming edge case), and the three
/// compressibility profiles at the `example.c` large-buffer size.
fn sample_inputs() -> Vec<Vec<u8>> {
    vec![
        HELLO.to_vec(),
        Vec::new(),
        zeros(LARGE_LEN),
        repetitive(LARGE_LEN / 2),
        incompressible(LARGE_LEN),
    ]
}

// ===========================================================================
// `zlib-rs` streaming helpers (black-box over the public API)
// ===========================================================================

/// Compress `data` with `zlib-rs` at `level`, `mem_level`, `strategy`, and the
/// framing selected by `window_bits`, returning the exact compressed stream.
///
/// Drives the public streaming `deflate` engine to completion with a single
/// `Z_FINISH` pass, growing the destination only in the unlikely event a pass
/// reports `Ok` with the buffer full. `deflate_end` is called explicitly to
/// mirror the C `deflateEnd` contract (the owned engine state would also be
/// released on drop).
///
/// # The destination size is part of the byte-identity contract
///
/// The initial capacity is `compress_bound(len) + 128` and it doubles only when
/// a pass fills it exactly. That is **not** an arbitrary implementation detail
/// that an oracle generator may vary: C's `deflate_stored` consults
/// `strm->avail_out` when it picks stored-block lengths (`deflate.c` L1689-L1748),
/// so level 0 output is a function of the destination size. Any reference
/// generator used to bake vectors for this file must reproduce this sizing
/// exactly — see the regeneration recipe on [`byte_identity`].
fn zlib_rs_deflate_full(
    data: &[u8],
    level: i32,
    window_bits: i32,
    mem_level: i32,
    strategy: Strategy,
) -> Vec<u8> {
    let mut strm = ZStream::new();
    deflate_init2(
        &mut strm,
        level,
        Z_DEFLATED,
        window_bits,
        mem_level,
        strategy,
    )
    .expect("deflate_init2 must succeed for valid parameters");

    // Size the destination with the zlib bound plus slack for a gzip
    // header/trailer; the loop still grows it if a pass needs more room.
    let mut output = vec![0u8; compress_bound(data.len()) + 128];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;

    loop {
        let outcome = deflate(&mut strm, &data[in_pos..], &mut output[out_pos..], Z_FINISH);
        in_pos += outcome.consumed;
        out_pos += outcome.produced;
        match outcome.code {
            ReturnCode::StreamEnd => break,
            ReturnCode::Ok => {
                if out_pos == output.len() {
                    // Ran out of room before finishing — grow and continue.
                    output.resize(output.len() * 2, 0);
                } else if outcome.consumed == 0 && outcome.produced == 0 {
                    panic!("zlib-rs deflate stalled with Z_FINISH before StreamEnd");
                }
            }
            other => panic!("zlib-rs deflate returned {other:?}"),
        }
    }

    deflate_end(&mut strm).expect("deflate_end must succeed");
    output.truncate(out_pos);
    output
}

/// Compress `data` with `zlib-rs` at the default memory level
/// (`DEF_MEM_LEVEL == 8`), delegating to [`zlib_rs_deflate_full`].
///
/// This is the shape the five-field tier-1 vector tables
/// ([`byte_identity::BI_VECTORS`] and [`byte_identity::BI_VECTORS_GZIP`]) are
/// baked against — they carry no `memLevel` column — so it is kept as a distinct
/// entry point rather than folded into its callers.
fn zlib_rs_deflate_strategy(
    data: &[u8],
    level: i32,
    window_bits: i32,
    strategy: Strategy,
) -> Vec<u8> {
    zlib_rs_deflate_full(data, level, window_bits, DEF_MEM_LEVEL, strategy)
}

/// Compress `data` with `zlib-rs` using the default strategy — the common case
/// for the decode-compatibility tests.
fn zlib_rs_deflate(data: &[u8], level: i32, window_bits: i32) -> Vec<u8> {
    zlib_rs_deflate_strategy(data, level, window_bits, Strategy::Default)
}

/// Decompress `data` with `zlib-rs` using the framing selected by
/// `window_bits`, returning the recovered bytes.
///
/// Drives the public streaming `inflate` engine, accumulating output into a
/// growable buffer via a reused fixed-size chunk, until `StreamEnd`. A stalled
/// pass (no progress without reaching the end) panics so a truncated or corrupt
/// stream fails the test loudly rather than hanging.
fn zlib_rs_inflate(data: &[u8], window_bits: i32) -> Vec<u8> {
    let mut strm = ZStream::new();
    inflate_init2(&mut strm, window_bits).expect("inflate_init2 must succeed");

    let mut output = Vec::new();
    let mut chunk = vec![0u8; 32 * 1024];
    let mut in_pos = 0usize;

    loop {
        let outcome = inflate(&mut strm, &data[in_pos..], &mut chunk, Z_NO_FLUSH);
        in_pos += outcome.consumed;
        output.extend_from_slice(&chunk[..outcome.produced]);
        match outcome.code {
            ReturnCode::StreamEnd => break,
            ReturnCode::Ok => {
                if outcome.consumed == 0 && outcome.produced == 0 {
                    panic!("zlib-rs inflate stalled before StreamEnd (truncated input?)");
                }
            }
            other => panic!("zlib-rs inflate returned {other:?}"),
        }
    }

    inflate_end(&mut strm).expect("inflate_end must succeed");
    output
}

/// Drive the streaming `inflate` engine over `data` and return the terminal
/// [`ReturnCode`] instead of the recovered bytes — the negative-path counterpart
/// to [`zlib_rs_inflate`].
///
/// Where [`zlib_rs_inflate`] panics on anything other than a clean `StreamEnd`,
/// this reports the outcome so a test can assert that a *rejection* is clean and
/// correctly typed. A rejected `inflate_init2` is reported as `StreamError`
/// (matching C, which returns `Z_STREAM_ERROR` for an unacceptable `windowBits`),
/// and a stalled pass as `BufError`, so the function always terminates.
#[cfg(feature = "gzip")]
fn zlib_rs_inflate_outcome(data: &[u8], window_bits: i32) -> ReturnCode {
    let mut strm = ZStream::new();
    if inflate_init2(&mut strm, window_bits).is_err() {
        return ReturnCode::StreamError;
    }

    let mut chunk = vec![0u8; 32 * 1024];
    let mut in_pos = 0usize;

    let code = loop {
        let outcome = inflate(&mut strm, &data[in_pos..], &mut chunk, Z_NO_FLUSH);
        in_pos += outcome.consumed;
        if outcome.code != ReturnCode::Ok {
            break outcome.code;
        }
        if outcome.consumed == 0 && outcome.produced == 0 {
            break ReturnCode::BufError;
        }
    };

    // `inflate_end` is called unconditionally to mirror the C contract; its own
    // result is irrelevant to the outcome under test.
    let _ = inflate_end(&mut strm);
    code
}

// ===========================================================================
// `flate2` (miniz_oxide) helpers — the independent reference codec
// ===========================================================================

/// Compress `data` to a zlib (RFC 1950) stream with `flate2` at `level`.
fn flate2_compress_zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data).expect("flate2 zlib write_all");
    encoder.finish().expect("flate2 zlib finish")
}

/// Compress `data` to a raw DEFLATE (RFC 1951) stream with `flate2` at `level`.
fn flate2_compress_raw(data: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data).expect("flate2 raw write_all");
    encoder.finish().expect("flate2 raw finish")
}

/// Decompress a `flate2`-encoded zlib (RFC 1950) stream.
fn flate2_decompress_zlib(data: &[u8]) -> Vec<u8> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .expect("flate2 zlib read_to_end");
    out
}

/// Decompress a `flate2`-encoded raw DEFLATE (RFC 1951) stream.
fn flate2_decompress_raw(data: &[u8]) -> Vec<u8> {
    let mut decoder = DeflateDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .expect("flate2 raw read_to_end");
    out
}

/// Compress `data` to a gzip (RFC 1952) stream with `flate2` at `level`.
#[cfg(feature = "gzip")]
fn flate2_compress_gzip(data: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data).expect("flate2 gzip write_all");
    encoder.finish().expect("flate2 gzip finish")
}

/// Decompress a `flate2`-encoded gzip (RFC 1952) stream.
#[cfg(feature = "gzip")]
fn flate2_decompress_gzip(data: &[u8]) -> Vec<u8> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .expect("flate2 gzip read_to_end");
    out
}

// ===========================================================================
// Tier 2 — decode-compatibility: `flate2` decodes what `zlib-rs` produced
// ===========================================================================

/// `flate2` must decode every zlib (RFC 1950) stream `zlib-rs` produces, at
/// every compression level and across the full input corpus.
#[test]
fn flate2_decodes_zlib_rs_zlib() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            let compressed = zlib_rs_deflate(&input, level, WBITS_ZLIB);
            let restored = flate2_decompress_zlib(&compressed);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "flate2 failed to decode zlib-rs zlib stream at level {level} (input len {})",
                input.len()
            );
        }
    }
}

/// `flate2` must decode every raw DEFLATE (RFC 1951) stream `zlib-rs` produces.
#[test]
fn flate2_decodes_zlib_rs_raw() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            let compressed = zlib_rs_deflate(&input, level, WBITS_RAW);
            let restored = flate2_decompress_raw(&compressed);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "flate2 failed to decode zlib-rs raw stream at level {level} (input len {})",
                input.len()
            );
        }
    }
}

/// `flate2` must decode every gzip (RFC 1952) stream `zlib-rs` produces,
/// validating `zlib-rs`'s gzip header/trailer emission.
#[cfg(feature = "gzip")]
#[test]
fn flate2_decodes_zlib_rs_gzip() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            let compressed = zlib_rs_deflate(&input, level, WBITS_GZIP);
            let restored = flate2_decompress_gzip(&compressed);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "flate2 failed to decode zlib-rs gzip stream at level {level} (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// Tier 2 — decode-compatibility: `zlib-rs` decodes what `flate2` produced
// ===========================================================================

/// `zlib-rs` must decode every zlib (RFC 1950) stream `flate2` produces.
#[test]
fn zlib_rs_decodes_flate2_zlib() {
    for input in sample_inputs() {
        for level in 0u32..=9 {
            let compressed = flate2_compress_zlib(&input, level);
            let restored = zlib_rs_inflate(&compressed, WBITS_ZLIB);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "zlib-rs failed to decode flate2 zlib stream at level {level} (input len {})",
                input.len()
            );
        }
    }
}

/// `zlib-rs` must decode every raw DEFLATE (RFC 1951) stream `flate2` produces.
#[test]
fn zlib_rs_decodes_flate2_raw() {
    for input in sample_inputs() {
        for level in 0u32..=9 {
            let compressed = flate2_compress_raw(&input, level);
            let restored = zlib_rs_inflate(&compressed, WBITS_RAW);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "zlib-rs failed to decode flate2 raw stream at level {level} (input len {})",
                input.len()
            );
        }
    }
}

/// `zlib-rs` must decode every gzip (RFC 1952) stream `flate2` produces,
/// validating `zlib-rs`'s gzip header/trailer parsing against an independent
/// gzip writer.
#[cfg(feature = "gzip")]
#[test]
fn zlib_rs_decodes_flate2_gzip() {
    for input in sample_inputs() {
        for level in 0u32..=9 {
            let compressed = flate2_compress_gzip(&input, level);
            let restored = zlib_rs_inflate(&compressed, WBITS_GZIP);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "zlib-rs failed to decode flate2 gzip stream at level {level} (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// Every deflate strategy produces a valid, interoperable wire format
// ===========================================================================

/// Each of the five deflate strategies (`Z_DEFAULT_STRATEGY`, `Z_FILTERED`,
/// `Z_HUFFMAN_ONLY`, `Z_RLE`, `Z_FIXED`) must emit a fully compliant DEFLATE
/// stream: `flate2` (an independent decoder) decodes it, and `zlib-rs`
/// round-trips its own output.
///
/// Per-strategy *byte-identity* against a reference is intentionally not
/// asserted here — `flate2`'s high-level API does not expose a strategy knob,
/// so there is no strategy-matched reference encoder under the default backend.
/// Decode-compatibility plus self round-trip is the robust, always-on proof
/// that every strategy's wire format is correct.
#[test]
fn all_strategies_wire_format_compatible() {
    let strategies = [
        Strategy::Default,
        Strategy::Filtered,
        Strategy::HuffmanOnly,
        Strategy::Rle,
        Strategy::Fixed,
    ];
    let inputs = [
        HELLO.to_vec(),
        repetitive(4096),
        zeros(4096),
        incompressible(4096),
    ];

    for strategy in strategies {
        for input in &inputs {
            // zlib framing at the default level, using this strategy.
            let compressed = zlib_rs_deflate_strategy(input, 6, WBITS_ZLIB, strategy);

            // An independent decoder must accept the stream.
            let via_flate2 = flate2_decompress_zlib(&compressed);
            assert_eq!(
                via_flate2.as_slice(),
                input.as_slice(),
                "flate2 failed to decode zlib-rs {strategy:?} stream (input len {})",
                input.len()
            );

            // And zlib-rs must decode its own strategy-specific output.
            let via_zlib_rs = zlib_rs_inflate(&compressed, WBITS_ZLIB);
            assert_eq!(
                via_zlib_rs.as_slice(),
                input.as_slice(),
                "zlib-rs failed to round-trip {strategy:?} stream (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// One-call crate-root re-exports (`compress2` / `compress_bound` / `uncompress`)
// ===========================================================================

/// Exercise the idiomatic one-call re-exports from the crate root and
/// cross-validate them with `flate2` in both directions. This binds directly to
/// `compress2`, `compress_bound`, and `uncompress` as re-exported by
/// `src/lib.rs`, confirming the `compressBound` sizing contract (AAP §0.6.4) is
/// sufficient for the emitted stream.
#[test]
fn one_call_reexports_cross_validate() {
    for input in sample_inputs() {
        // Compress with `compress2` at best level, sized via `compress_bound`.
        let mut compressed = vec![0u8; compress_bound(input.len())];
        let produced = compress2(&mut compressed, &input, 9).expect("compress2 must succeed");
        assert!(
            produced <= compressed.len(),
            "compress2 output ({produced}) overran compress_bound ({})",
            compressed.len()
        );
        compressed.truncate(produced);

        // The reference decoder accepts the one-call zlib stream.
        let via_flate2 = flate2_decompress_zlib(&compressed);
        assert_eq!(via_flate2.as_slice(), input.as_slice());

        // The one-call `uncompress` decodes a flate2-produced zlib stream. The
        // destination is sized to the known original length (at least one byte,
        // so an empty payload still has valid output room).
        let flate2_stream = flate2_compress_zlib(&input, 6);
        let mut restored = vec![0u8; input.len().max(1)];
        let written = uncompress(&mut restored, &flate2_stream).expect("uncompress must succeed");
        restored.truncate(written);
        assert_eq!(restored.as_slice(), input.as_slice());
    }
}

// ===========================================================================
// Auto-detect windowBits (40..=47) transparently accepts zlib and gzip wrappers
// ===========================================================================

/// `windowBits = 47` (auto-detect) must transparently decode a zlib (RFC 1950)
/// wrapper — the inflate-only auto-detection path of the overloading contract.
///
/// Auto-detect requires gzip framing support (the `40..=47` windowBits range
/// is only accepted when the `gzip` feature is enabled), so this test is
/// gated to match.
#[cfg(feature = "gzip")]
#[test]
fn auto_detect_accepts_zlib() {
    for input in sample_inputs() {
        let zlib_stream = zlib_rs_deflate(&input, 6, WBITS_ZLIB);
        let restored = zlib_rs_inflate(&zlib_stream, WBITS_AUTO);
        assert_eq!(
            restored.as_slice(),
            input.as_slice(),
            "auto-detect failed on a zlib stream (input len {})",
            input.len()
        );
    }
}

/// `windowBits = 47` (auto-detect) must also transparently decode a gzip
/// (RFC 1952) wrapper.
#[cfg(feature = "gzip")]
#[test]
fn auto_detect_accepts_gzip() {
    for input in sample_inputs() {
        let gzip_stream = zlib_rs_deflate(&input, 6, WBITS_GZIP);
        let restored = zlib_rs_inflate(&gzip_stream, WBITS_AUTO);
        assert_eq!(
            restored.as_slice(),
            input.as_slice(),
            "auto-detect failed on a gzip stream (input len {})",
            input.len()
        );
    }
}

// ===========================================================================
// `zlib-rs` self round-trip across framings and levels (streaming-API sanity)
// ===========================================================================

/// `zlib-rs` must round-trip its own zlib and raw output at every level — a
/// direct exercise of the streaming `deflate`/`inflate` drivers used by the
/// cross-decode tests.
#[test]
fn zlib_rs_self_roundtrip_zlib_and_raw() {
    for input in sample_inputs() {
        for &(window_bits, name) in &[(WBITS_ZLIB, "zlib"), (WBITS_RAW, "raw")] {
            for &level in &ZLIB_RS_LEVELS {
                let compressed = zlib_rs_deflate(&input, level, window_bits);
                let restored = zlib_rs_inflate(&compressed, window_bits);
                assert_eq!(
                    restored.as_slice(),
                    input.as_slice(),
                    "self round-trip failed: {name} framing, level {level} (input len {})",
                    input.len()
                );
            }
        }
    }
}

/// `zlib-rs` must round-trip its own gzip output at every level.
#[cfg(feature = "gzip")]
#[test]
fn zlib_rs_self_roundtrip_gzip() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            let compressed = zlib_rs_deflate(&input, level, WBITS_GZIP);
            let restored = zlib_rs_inflate(&compressed, WBITS_GZIP);
            assert_eq!(
                restored.as_slice(),
                input.as_slice(),
                "gzip self round-trip failed at level {level} (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// Tier 2 — small-window framings (`windowBits = ±9`), both directions
// ===========================================================================

/// A 512-byte-window stream must remain a fully compliant DEFLATE stream in both
/// framings: an independent decoder accepts it, and `zlib-rs` round-trips it.
///
/// `windowBits = ±9` is the smallest window the format permits and the corner of
/// the overloading contract with the least prior coverage — the tier-2 matrix
/// otherwise concentrates on `±15` and gzip. A small window changes only the
/// *encoder's* reachable match distances, so a conforming decoder needs no
/// special handling; if `flate2` were to reject one of these streams, the
/// divergence would be in `zlib-rs`'s emission. `zlib-rs`'s own decode is
/// asserted alongside so that a failure localises immediately to one side.
///
/// Decode-compatibility only: byte-identity for these framings is proven by the
/// `windowBits` 9 and −9 rows of [`byte_identity::BI_GRID`].
#[test]
fn small_window_framings_wire_format_compatible() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            // zlib framing, 512-byte window.
            let zlib_stream = zlib_rs_deflate(&input, level, WBITS_ZLIB_SMALL);
            assert_eq!(
                flate2_decompress_zlib(&zlib_stream).as_slice(),
                input.as_slice(),
                "flate2 failed to decode a zlib windowBits=9 stream at level {level} \
                 (input len {})",
                input.len()
            );
            assert_eq!(
                zlib_rs_inflate(&zlib_stream, WBITS_ZLIB_SMALL).as_slice(),
                input.as_slice(),
                "zlib-rs failed to round-trip a zlib windowBits=9 stream at level {level} \
                 (input len {})",
                input.len()
            );

            // Raw DEFLATE, 512-byte window.
            let raw_stream = zlib_rs_deflate(&input, level, WBITS_RAW_SMALL);
            assert_eq!(
                flate2_decompress_raw(&raw_stream).as_slice(),
                input.as_slice(),
                "flate2 failed to decode a raw windowBits=-9 stream at level {level} \
                 (input len {})",
                input.len()
            );
            assert_eq!(
                zlib_rs_inflate(&raw_stream, WBITS_RAW_SMALL).as_slice(),
                input.as_slice(),
                "zlib-rs failed to round-trip a raw windowBits=-9 stream at level {level} \
                 (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// The one-call wrapper is the streaming path at the default configuration
// ===========================================================================

/// `compress2` must emit **exactly** the bytes the streaming engine emits at the
/// default configuration — zlib framing, `MAX_WBITS`, `DEF_MEM_LEVEL`,
/// `Z_DEFAULT_STRATEGY` — for every level and across the full corpus.
///
/// C's `compress2` is `deflateInit` (i.e. `deflateInit2` with `MAX_WBITS`,
/// `DEF_MEM_LEVEL`, `Z_DEFAULT_STRATEGY`) followed by a `Z_FINISH` loop, so this
/// equality is a real behavioural requirement and not merely an internal
/// coincidence. Pinning it has a concrete payoff for the byte-identity gate:
/// because the one-call wrapper is byte-for-byte the streaming path at that
/// configuration, every `windowBits = 15` / `memLevel = 8` / strategy-0 oracle
/// row in [`byte_identity`] transitively proves `compress2` byte-identical to C
/// as well, without a single extra baked vector.
///
/// Note that the destination sizing differs deliberately between the two paths
/// (`compress_bound(len)` here versus `compress_bound(len) + 128` in
/// [`zlib_rs_deflate_full`]) and the outputs still agree, including at level 0
/// where `deflate_stored` consults `avail_out`: the bound is large enough in
/// both cases that no stored-block boundary is forced.
#[test]
fn one_call_matches_streaming_byte_for_byte() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            let mut one_call = vec![0u8; compress_bound(input.len())];
            let produced = compress2(&mut one_call, &input, level).expect("compress2 must succeed");
            one_call.truncate(produced);

            let streaming = zlib_rs_deflate(&input, level, WBITS_ZLIB);
            assert_eq!(
                one_call,
                streaming,
                "compress2 diverged from the streaming default configuration at level \
                 {level} (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// Auto-detect must reject a raw stream rather than mis-detect it
// ===========================================================================

/// `windowBits = 47` (auto-detect) must **refuse** a raw DEFLATE stream with
/// `Z_DATA_ERROR`.
///
/// Auto-detection is defined over the zlib and gzip wrappers only; raw DEFLATE
/// carries no header to detect. The negative case matters because a raw stream's
/// first byte is arbitrary, so a sloppy detector could read it as a plausible
/// CMF/FLG pair and start decoding garbage — silently producing wrong output
/// instead of an error. This asserts the failure is clean and typed for every
/// level and every corpus, completing the overloading contract alongside
/// [`auto_detect_accepts_zlib`] and [`auto_detect_accepts_gzip`].
///
/// Gated on the `gzip` feature, which is what makes the `40..=47` windowBits
/// range acceptable to `inflate_init2` at all.
#[cfg(feature = "gzip")]
#[test]
fn auto_detect_rejects_raw_stream() {
    for input in sample_inputs() {
        for &level in &ZLIB_RS_LEVELS {
            let raw_stream = zlib_rs_deflate(&input, level, WBITS_RAW);
            let outcome = zlib_rs_inflate_outcome(&raw_stream, WBITS_AUTO);
            assert_eq!(
                outcome,
                ReturnCode::DataError,
                "auto-detect must reject a raw DEFLATE stream with Z_DATA_ERROR, got \
                 {outcome:?} at level {level} (input len {})",
                input.len()
            );
        }
    }
}

// ===========================================================================
// Tier 1 — strict byte-identity vs GENUINE C zlib 1.3.2.1-motley
// (deterministic oracle vectors; always-on release gate for user constraint 1)
// ===========================================================================

/// Strict byte-for-byte equality of `zlib-rs`'s compressed output against the
/// genuine C zlib `1.3.2.1-motley` encoder — the defining acceptance criterion
/// of the migration (AAP §0.6.4 / §0.7.2 standard S3; user constraint 1).
///
/// The oracle vectors below were produced by the reference C library via
/// `deflateInit2` + `deflate(Z_FINISH)` + `deflateEnd` (method `Z_DEFLATED`),
/// mirroring [`zlib_rs_deflate_full`] exactly. Both the input corpora and the
/// expected output are baked into compact whitespace-delimited tables, so this
/// gate runs by default in CI with **no C toolchain** and no cross-language
/// input-reproduction risk.
///
/// # The two vector families
///
/// | family | corpora | axes | encoding |
/// |--------|---------|------|----------|
/// | [`BI_VECTORS`] (225 rows) + [`BI_VECTORS_GZIP`] (75 rows) | the five short hex corpora of [`BI_INPUTS`] (0 / 13 / 256 / 256 / 128 B) | levels `-1..=9`, all five strategies, `windowBits` 15 / −15 / 9 / 31, `memLevel = 8` | **full literal hex** |
/// | [`BI_GRID`] (3,300 rows) + [`BI_GRID_GZIP`] (825 rows) | the five 16 KiB shapes of [`bi_grid_inputs`] | levels `-1..=9`, all five strategies, `windowBits` 15 / −15 / 9 / −9 / 31, `memLevel` 1 / 8 / 9 | `(length, CRC-32)` **digest** |
/// | [`BI_EXTREMES`] (24 rows) + [`BI_EXTREMES_GZIP`] (12 rows) | the short [`BI_INPUTS`] corpora | the `memLevel` 1 / 9 corners the full-hex family never reached | **full literal hex** |
///
/// Together that is 4,461 baked assertions. The digest family is what makes the
/// wide grid affordable in source; the two full-hex families are what keep an
/// exact-bytes proof in the suite (see [`BI_GRID`] for the honesty note).
///
/// # Regenerating the vectors
///
/// Every expected value here comes from the reference C encoder — never from
/// `zlib-rs`, which would make the gate self-referential. Build the reference
/// **outside** the working tree (the in-tree `*.c` are read-only oracle sources,
/// AAP §0.8.1 directive D-7), copying them to a scratch directory first:
///
/// ```text
/// gcc -O2 -D_LARGEFILE64_SOURCE=1 -DHAVE_UNISTD_H -c \
///     adler32.c compress.c crc32.c deflate.c gzclose.c gzlib.c gzread.c \
///     gzwrite.c infback.c inffast.c inflate.c inftrees.c trees.c uncompr.c zutil.c
/// ar rcs libz_ref.a *.o          # ~132 KB, 0 errors; zlibVersion() == "1.3.2.1-motley"
/// ```
///
/// Then drive it from a throw-away C generator that mirrors
/// [`zlib_rs_deflate_full`] call for call: `deflateInit2(level, Z_DEFLATED,
/// windowBits, memLevel, strategy)`, a destination of `compressBound(len) + 128`
/// doubled only when a pass fills it, one `deflate(Z_FINISH)` loop, then
/// `deflateEnd`. The destination sizing is load-bearing, not incidental —
/// `deflate_stored` consults `avail_out` (`deflate.c` L1689-L1748), so a
/// generator that sized with `deflateBound` instead would bake different level-0
/// bytes. Feed the generator the corpora **as files written by a Rust program
/// running [`bi_grid_inputs`]**, so no corpus is ever reimplemented in C.
///
/// Two cross-checks are cheap and catch a faithless generator immediately: it
/// must reproduce all 300 existing full-hex rows exactly at `memLevel = 8`, and a
/// random sample of grid rows must be re-derivable by a separate
/// single-configuration driver. Neither the generator, the object files,
/// `libz_ref.a` nor the corpus dumps are ever committed.
///
/// # Relationship to `tests/c_oracle.rs`
///
/// `tests/c_oracle.rs` is the **live** form of the same sweep: it builds the
/// reference library at run time and diffs the streams directly. It is opt-in
/// (`--features c-oracle`) precisely so that `cargo test` needs no C compiler,
/// and it is strictly **additive** to this module — never a replacement. This
/// module is the always-on gate; that one is the on-demand breadth check.
mod byte_identity {
    use super::*;

    /// Decode an even-length lowercase hex string to bytes.
    fn unhex(s: &str) -> Vec<u8> {
        assert!(s.len() % 2 == 0, "odd-length hex string: {s:?}");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex byte"))
            .collect()
    }

    /// Map a strategy id (identical for the C `Z_*` values and the `zlib-rs`
    /// [`Strategy`] discriminants) to the enum variant.
    fn strat_from_id(id: u8) -> Strategy {
        match id {
            0 => Strategy::Default,
            1 => Strategy::Filtered,
            2 => Strategy::HuffmanOnly,
            3 => Strategy::Rle,
            4 => Strategy::Fixed,
            other => panic!("unknown strategy id {other}"),
        }
    }

    /// The five short input corpora of the full-hex vector family, one hex
    /// string per line; text after `#` is a descriptive comment and the
    /// empty-hex line is the empty input. Baked as hex rather than generated so
    /// there is no way for the Rust corpus and the corpus the reference encoder
    /// consumed to drift apart.
    const BI_INPUTS: &str = "\
      # empty buffer (streaming edge case)
    68656c6c6f2c2068656c6c6f21  # the canonical example.c literal
    54686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220  # repetitive natural-language text (256 B, long matches)
    000000000000000000000000000000000101010101010101010101010101010102020202020202020202020202020202030303030303030303030303030303030404040404040404040404040404040405050505050505050505050505050505060606060606060606060606060606060707070707070707070707070707070708080808080808080808080808080808090909090909090909090909090909090a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f  # byte-run pattern (256 B, RLE/short-match paths)
    9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186  # deterministic pseudo-random data (128 B, incompressible)
    ";

    /// Full literal reference bytes for the zlib / raw / small-window framings:
    /// 225 rows = 5 corpora × `windowBits` {15, −15, 9} × (11 levels at the
    /// default strategy + level 6 at the other four strategies), all at
    /// `memLevel = 8`.
    ///
    /// Line format: `<input-index> <level> <windowBits> <strategy-id>
    /// <hex-expected>`. These rows are the exact-bytes core of the gate and are
    /// never edited to accommodate a change — a mismatch means either the
    /// encoder or the generator is wrong (AAP §0.8.1 directive D-1).
    const BI_VECTORS: &str = "\
    0 -1 15 0 789c030000000001
    0 0 15 0 7801010000ffff00000001
    0 1 15 0 7801030000000001
    0 2 15 0 785e030000000001
    0 3 15 0 785e030000000001
    0 4 15 0 785e030000000001
    0 5 15 0 785e030000000001
    0 6 15 0 789c030000000001
    0 7 15 0 78da030000000001
    0 8 15 0 78da030000000001
    0 9 15 0 78da030000000001
    0 -1 -15 0 0300
    0 0 -15 0 010000ffff
    0 1 -15 0 0300
    0 2 -15 0 0300
    0 3 -15 0 0300
    0 4 -15 0 0300
    0 5 -15 0 0300
    0 6 -15 0 0300
    0 7 -15 0 0300
    0 8 -15 0 0300
    0 9 -15 0 0300
    0 -1 9 0 1895030000000001
    0 0 9 0 1819010000ffff00000001
    0 1 9 0 1819030000000001
    0 2 9 0 1857030000000001
    0 3 9 0 1857030000000001
    0 4 9 0 1857030000000001
    0 5 9 0 1857030000000001
    0 6 9 0 1895030000000001
    0 7 9 0 18d3030000000001
    0 8 9 0 18d3030000000001
    0 9 9 0 18d3030000000001
    0 6 15 1 789c030000000001
    0 6 15 2 7801030000000001
    0 6 15 3 7801030000000001
    0 6 15 4 7801030000000001
    0 6 -15 1 0300
    0 6 -15 2 0300
    0 6 -15 3 0300
    0 6 -15 4 0300
    0 6 9 1 1895030000000001
    0 6 9 2 1819030000000001
    0 6 9 3 1819030000000001
    0 6 9 4 1819030000000001
    1 -1 15 0 789ccb48cdc9c9d751c800518a0021700496
    1 0 15 0 7801010d00f2ff68656c6c6f2c2068656c6c6f2121700496
    1 1 15 0 7801cb48cdc9c9d751c800518a0021700496
    1 2 15 0 785ecb48cdc9c9d751c800518a0021700496
    1 3 15 0 785ecb48cdc9c9d751c800518a0021700496
    1 4 15 0 785ecb48cdc9c9d751c800518a0021700496
    1 5 15 0 785ecb48cdc9c9d751c800518a0021700496
    1 6 15 0 789ccb48cdc9c9d751c800518a0021700496
    1 7 15 0 78dacb48cdc9c9d751c800518a0021700496
    1 8 15 0 78dacb48cdc9c9d751c800518a0021700496
    1 9 15 0 78dacb48cdc9c9d751c800518a0021700496
    1 -1 -15 0 cb48cdc9c9d751c800518a00
    1 0 -15 0 010d00f2ff68656c6c6f2c2068656c6c6f21
    1 1 -15 0 cb48cdc9c9d751c800518a00
    1 2 -15 0 cb48cdc9c9d751c800518a00
    1 3 -15 0 cb48cdc9c9d751c800518a00
    1 4 -15 0 cb48cdc9c9d751c800518a00
    1 5 -15 0 cb48cdc9c9d751c800518a00
    1 6 -15 0 cb48cdc9c9d751c800518a00
    1 7 -15 0 cb48cdc9c9d751c800518a00
    1 8 -15 0 cb48cdc9c9d751c800518a00
    1 9 -15 0 cb48cdc9c9d751c800518a00
    1 -1 9 0 1895cb48cdc9c9d751c800518a0021700496
    1 0 9 0 1819010d00f2ff68656c6c6f2c2068656c6c6f2121700496
    1 1 9 0 1819cb48cdc9c9d751c800518a0021700496
    1 2 9 0 1857cb48cdc9c9d751c800518a0021700496
    1 3 9 0 1857cb48cdc9c9d751c800518a0021700496
    1 4 9 0 1857cb48cdc9c9d751c800518a0021700496
    1 5 9 0 1857cb48cdc9c9d751c800518a0021700496
    1 6 9 0 1895cb48cdc9c9d751c800518a0021700496
    1 7 9 0 18d3cb48cdc9c9d751c800518a0021700496
    1 8 9 0 18d3cb48cdc9c9d751c800518a0021700496
    1 9 9 0 18d3cb48cdc9c9d751c800518a0021700496
    1 6 15 1 789ccb48cdc9c9d751c848cdc9c957040021700496
    1 6 15 2 7801cb48cdc9c9d751c848cdc9c957040021700496
    1 6 15 3 7801cb48cdc9c9d751c848cdc9c957040021700496
    1 6 15 4 7801cb48cdc9c9d751c800518a0021700496
    1 6 -15 1 cb48cdc9c9d751c848cdc9c9570400
    1 6 -15 2 cb48cdc9c9d751c848cdc9c9570400
    1 6 -15 3 cb48cdc9c9d751c848cdc9c9570400
    1 6 -15 4 cb48cdc9c9d751c800518a00
    1 6 9 1 1895cb48cdc9c9d751c848cdc9c957040021700496
    1 6 9 2 1819cb48cdc9c9d751c848cdc9c957040021700496
    1 6 9 3 1819cb48cdc9c9d751c848cdc9c957040021700496
    1 6 9 4 1819cb48cdc9c9d751c800518a0021700496
    2 -1 15 0 789c0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 0 15 0 7801010001fffe54686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f7665722050fc5c22
    2 1 15 0 78010bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 2 15 0 785e0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 3 15 0 785e0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 4 15 0 785e0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 5 15 0 785e0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 6 15 0 789c0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 7 15 0 78da0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 8 15 0 78da0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 9 15 0 78da0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 -1 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 0 -15 0 010001fffe54686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220
    2 1 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 2 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 3 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 4 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 5 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 6 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 7 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 8 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 9 -15 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 -1 9 0 18950bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 0 9 0 1819010001fffe54686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f7665722050fc5c22
    2 1 9 0 18190bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 2 9 0 18570bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 3 9 0 18570bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 4 9 0 18570bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 5 9 0 18570bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 6 9 0 18950bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 7 9 0 18d30bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 8 9 0 18d30bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 9 9 0 18d30bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 6 15 1 789c0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228c94855c849acaa5448c94fd75308197e8a0150fc5c22
    2 6 15 2 780105c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60f50fc5c22
    2 6 15 3 780105c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60f50fc5c22
    2 6 15 4 78010bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 6 -15 1 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228c94855c849acaa5448c94fd75308197e8a01
    2 6 -15 2 05c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60f
    2 6 -15 3 05c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60f
    2 6 -15 4 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 6 9 1 18950bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228c94855c849acaa5448c94fd75308197e8a0150fc5c22
    2 6 9 2 181905c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60f50fc5c22
    2 6 9 3 181905c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60f50fc5c22
    2 6 9 4 18190bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    3 -1 15 0 789c5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 0 15 0 7801010001fffe000000000000000000000000000000000101010101010101010101010101010102020202020202020202020202020202030303030303030303030303030303030404040404040404040404040404040405050505050505050505050505050505060606060606060606060606060606060707070707070707070707070707070708080808080808080808080808080808090909090909090909090909090909090a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f70de0781
    3 1 15 0 78015dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 2 15 0 785e5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 3 15 0 785e5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 4 15 0 785e5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 5 15 0 785e5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 6 15 0 789c5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 7 15 0 78da5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 8 15 0 78da5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 9 15 0 78da5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 -1 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 0 -15 0 010001fffe000000000000000000000000000000000101010101010101010101010101010102020202020202020202020202020202030303030303030303030303030303030404040404040404040404040404040405050505050505050505050505050505060606060606060606060606060606060707070707070707070707070707070708080808080808080808080808080808090909090909090909090909090909090a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f
    3 1 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 2 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 3 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 4 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 5 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 6 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 7 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 8 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 9 -15 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 -1 9 0 18955dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 0 9 0 1819010001fffe000000000000000000000000000000000101010101010101010101010101010102020202020202020202020202020202030303030303030303030303030303030404040404040404040404040404040405050505050505050505050505050505060606060606060606060606060606060707070707070707070707070707070708080808080808080808080808080808090909090909090909090909090909090a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f70de0781
    3 1 9 0 18195dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 2 9 0 18575dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 3 9 0 18575dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 4 9 0 18575dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 5 9 0 18575dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 6 9 0 18955dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 7 9 0 18d35dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 8 9 0 18d35dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 9 9 0 18d35dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 6 15 1 789c5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 6 15 2 780105c187010020080020776effff3600000000000000004044444444444444242222222222222262666666666666661611111111111111515555555555555535333333333333337befbdf7de7befbdf7de73777777777777778f88888888888888c8ccccccccccccccacaaaaaaaaaaaaaaeaeeeeeeeeeeeeee9e99999999999999d9ddddddddddddddbdbbbbbbbbbbbbbbfb70de0781
    3 6 15 3 78015dc1b701c0200000207b37f9ff5b7720202221a3a0a2a163606261e3e0e2c38f0770de0781
    3 6 15 4 7801636040058c6880090d30a3011634c08a06d8d0003b1ae040039c68800b0d70a3011e34c08b06f8d0003f1a000070de0781
    3 6 -15 1 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 6 -15 2 05c187010020080020776effff3600000000000000004044444444444444242222222222222262666666666666661611111111111111515555555555555535333333333333337befbdf7de7befbdf7de73777777777777778f88888888888888c8ccccccccccccccacaaaaaaaaaaaaaaeaeeeeeeeeeeeeee9e99999999999999d9ddddddddddddddbdbbbbbbbbbbbbbbfb
    3 6 -15 3 5dc1b701c0200000207b37f9ff5b7720202221a3a0a2a163606261e3e0e2c38f07
    3 6 -15 4 636040058c6880090d30a3011634c08a06d8d0003b1ae040039c68800b0d70a3011e34c08b06f8d0003f1a0000
    3 6 9 1 18955dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 6 9 2 181905c187010020080020776effff3600000000000000004044444444444444242222222222222262666666666666661611111111111111515555555555555535333333333333337befbdf7de7befbdf7de73777777777777778f88888888888888c8ccccccccccccccacaaaaaaaaaaaaaaeaeeeeeeeeeeeeee9e99999999999999d9ddddddddddddddbdbbbbbbbbbbbbbbfb70de0781
    3 6 9 3 18195dc1b701c0200000207b37f9ff5b7720202221a3a0a2a163606261e3e0e2c38f0770de0781
    3 6 9 4 1819636040058c6880090d30a3011634c08a06d8d0003b1ae040039c68800b0d70a3011e34c08b06f8d0003f1a000070de0781
    4 -1 15 0 789c0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 0 15 0 78010180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 1 15 0 78010180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 2 15 0 785e0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 3 15 0 785e0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 4 15 0 785e0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 5 15 0 785e0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 15 0 789c0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 7 15 0 78da0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 8 15 0 78da0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 9 15 0 78da0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 -1 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 0 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 1 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 2 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 3 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 4 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 5 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 6 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 7 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 8 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 9 -15 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 -1 9 0 18950180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 0 9 0 18190180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 1 9 0 18190180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 2 9 0 18570180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 3 9 0 18570180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 4 9 0 18570180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 5 9 0 18570180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 9 0 18950180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 7 9 0 18d30180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 8 9 0 18d30180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 9 9 0 18d30180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 15 1 789c0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 15 2 78010180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 15 3 78010180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 15 4 78010180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 -15 1 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 6 -15 2 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 6 -15 3 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 6 -15 4 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 6 9 1 18950180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 9 2 18190180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 9 3 18190180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 6 9 4 18190180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    ";

    /// Full literal reference bytes for the gzip framing (`windowBits = 31`): 75
    /// rows = 5 corpora × the same 15 level/strategy combinations as
    /// [`BI_VECTORS`], at `memLevel = 8`.
    ///
    /// # The one host-dependent byte, and why it is still asserted exactly
    ///
    /// Byte 9 of a gzip member is the `OS` field, and zlib fills it from the
    /// compile-time `OS_CODE`: **3 on Unix-family targets, 10 on Windows, 19 on
    /// non-Windows Apple** (AAP §0.6.6). Every row below was baked on a Unix host
    /// and therefore carries `03`.
    ///
    /// That is a property of the *baking host*, not of the library: byte-identity
    /// (AAP §0.8.1 directive D-1) is defined against the reference C library built
    /// for the **same** target, and C's own `zutil.h` cascade selects 10 and 19 on
    /// Windows and Apple too, so emitting them there is parity rather than a
    /// regression. The rows are consequently *retargeted*, not weakened: before
    /// each comparison [`check_all`] rewrites byte 9 of the expected vector to
    /// [`HOST_GZIP_OS_CODE`] — the value reference zlib writes on the target being
    /// compiled, derived independently of the library by [`retarget_gzip_os`] —
    /// and then compares **every** byte, byte 9 included, with a bare
    /// `assert_eq!`. On a Unix host the rewrite is a no-op (`HOST_GZIP_OS_CODE ==
    /// 3`), so the 75 rows below are compared exactly as baked, bit for bit.
    ///
    /// Nothing is given up by this: unlike the platform-neutral gzip tables
    /// ([`BI_GRID_GZIP`], [`BI_EXTREMES_GZIP`]), which erase byte 9 through
    /// [`normalise_gzip_os`] and therefore assert nothing about it, this family
    /// keeps a positive assertion on that byte on every host. It is what makes the
    /// `windows-latest` and `macos-latest` rows of the `build-test` matrix
    /// (AAP gap D3) run this gate for real instead of failing on a fixture
    /// artifact. `the_gzip_vector_family_holds_for_every_platform_os_code`
    /// substitutes all three values from a single host, which shows the fixture
    /// helper retargets correctly and that no other byte of the stream moves with
    /// it; the Windows and Apple *behaviour* is established by those native rows
    /// actually running, not by the substitution.
    #[cfg(feature = "gzip")]
    const BI_VECTORS_GZIP: &str = "\
    0 -1 31 0 1f8b080000000000000303000000000000000000
    0 0 31 0 1f8b0800000000000403010000ffff0000000000000000
    0 1 31 0 1f8b080000000000040303000000000000000000
    0 2 31 0 1f8b080000000000000303000000000000000000
    0 3 31 0 1f8b080000000000000303000000000000000000
    0 4 31 0 1f8b080000000000000303000000000000000000
    0 5 31 0 1f8b080000000000000303000000000000000000
    0 6 31 0 1f8b080000000000000303000000000000000000
    0 7 31 0 1f8b080000000000000303000000000000000000
    0 8 31 0 1f8b080000000000000303000000000000000000
    0 9 31 0 1f8b080000000000020303000000000000000000
    0 6 31 1 1f8b080000000000000303000000000000000000
    0 6 31 2 1f8b080000000000040303000000000000000000
    0 6 31 3 1f8b080000000000040303000000000000000000
    0 6 31 4 1f8b080000000000040303000000000000000000
    1 -1 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 0 31 0 1f8b0800000000000403010d00f2ff68656c6c6f2c2068656c6c6f219bdc9ab30d000000
    1 1 31 0 1f8b0800000000000403cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 2 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 3 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 4 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 5 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 6 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 7 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 8 31 0 1f8b0800000000000003cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 9 31 0 1f8b0800000000000203cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 6 31 1 1f8b0800000000000003cb48cdc9c9d751c848cdc9c95704009bdc9ab30d000000
    1 6 31 2 1f8b0800000000000403cb48cdc9c9d751c848cdc9c95704009bdc9ab30d000000
    1 6 31 3 1f8b0800000000000403cb48cdc9c9d751c848cdc9c95704009bdc9ab30d000000
    1 6 31 4 1f8b0800000000000403cb48cdc9c9d751c800518a009bdc9ab30d000000
    2 -1 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 0 31 0 1f8b0800000000000403010001fffe54686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e2054686520717569636b2062726f776e20666f78206a756d7073206f76657220aff02d1b00010000
    2 1 31 0 1f8b08000000000004030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 2 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 3 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 4 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 5 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 6 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 7 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 8 31 0 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 9 31 0 1f8b08000000000002030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 6 31 1 1f8b08000000000000030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228c94855c849acaa5448c94fd75308197e8a01aff02d1b00010000
    2 6 31 2 1f8b080000000000040305c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60faff02d1b00010000
    2 6 31 3 1f8b080000000000040305c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60faff02d1b00010000
    2 6 31 4 1f8b08000000000004030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    3 -1 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 0 31 0 1f8b0800000000000403010001fffe000000000000000000000000000000000101010101010101010101010101010102020202020202020202020202020202030303030303030303030303030303030404040404040404040404040404040405050505050505050505050505050505060606060606060606060606060606060707070707070707070707070707070708080808080808080808080808080808090909090909090909090909090909090a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0fddd8be2200010000
    3 1 31 0 1f8b08000000000004035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 2 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 3 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 4 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 5 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 6 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 7 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 8 31 0 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 9 31 0 1f8b08000000000002035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 6 31 1 1f8b08000000000000035dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7eddd8be2200010000
    3 6 31 2 1f8b080000000000040305c187010020080020776effff3600000000000000004044444444444444242222222222222262666666666666661611111111111111515555555555555535333333333333337befbdf7de7befbdf7de73777777777777778f88888888888888c8ccccccccccccccacaaaaaaaaaaaaaaeaeeeeeeeeeeeeee9e99999999999999d9ddddddddddddddbdbbbbbbbbbbbbbbfbddd8be2200010000
    3 6 31 3 1f8b08000000000004035dc1b701c0200000207b37f9ff5b7720202221a3a0a2a163606261e3e0e2c38f07ddd8be2200010000
    3 6 31 4 1f8b0800000000000403636040058c6880090d30a3011634c08a06d8d0003b1ae040039c68800b0d70a3011e34c08b06f8d0003f1a0000ddd8be2200010000
    4 -1 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 0 31 0 1f8b08000000000004030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 1 31 0 1f8b08000000000004030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 2 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 3 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 4 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 5 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 6 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 7 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 8 31 0 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 9 31 0 1f8b08000000000002030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 6 31 1 1f8b08000000000000030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 6 31 2 1f8b08000000000004030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 6 31 3 1f8b08000000000004030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 6 31 4 1f8b08000000000004030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    ";

    /// Parse [`BI_INPUTS`] into the ordered input corpus (one hex string per
    /// line; text after `#` is a descriptive comment, and the empty-hex line is
    /// the empty input).
    fn parse_inputs() -> Vec<Vec<u8>> {
        BI_INPUTS
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| unhex(l.split('#').next().unwrap().trim()))
            .collect()
    }

    /// Assert every vector line in `table` reproduces the reference bytes
    /// exactly. Line format: `<input-index> <level> <windowBits> <strategy-id>
    /// <hex-expected>`.
    ///
    /// Every byte is compared with a bare `assert_eq!`. The only adjustment is
    /// [`retarget_gzip_os`], which rewrites the single legitimately host-dependent
    /// gzip `OS` byte of the *expected* vector to [`HOST_GZIP_OS_CODE`] so that the
    /// comparison is against what reference zlib emits **on this target** — a
    /// no-op on the Unix hosts the tables were baked on, and an exact
    /// per-platform expectation everywhere else. Non-gzip framings are untouched.
    fn check_all(table: &str) {
        let inputs = parse_inputs();
        let mut count = 0usize;
        for line in table.lines().filter(|l| !l.trim().is_empty()) {
            let mut f = line.split_whitespace();
            let idx: usize = f.next().unwrap().parse().unwrap();
            let level: i32 = f.next().unwrap().parse().unwrap();
            let wbits: i32 = f.next().unwrap().parse().unwrap();
            let strat: u8 = f.next().unwrap().parse().unwrap();
            let expected = retarget_gzip_os(unhex(f.next().unwrap()), wbits, HOST_GZIP_OS_CODE);
            assert!(
                f.next().is_none(),
                "unexpected trailing field in vector line: {line:?}"
            );
            let input = &inputs[idx];
            let produced = zlib_rs_deflate_strategy(input, level, wbits, strat_from_id(strat));
            assert_eq!(
                produced,
                expected,
                "byte-identity mismatch vs C zlib 1.3.2.1-motley: input #{idx} \
                 (len {}), level {level}, windowBits {wbits}, strategy {:?} \
                 (expected gzip OS byte for this target: {HOST_GZIP_OS_CODE})",
                input.len(),
                strat_from_id(strat),
            );
            count += 1;
        }
        assert!(count > 0, "no oracle vectors were parsed from the table");
    }

    /// zlib / raw / small-window framings across every level and strategy.
    #[test]
    fn matches_reference_zlib_plain_framings() {
        check_all(BI_VECTORS);
    }

    /// gzip framing (RFC 1952) across every level and strategy. Gated on the
    /// `gzip` feature, which is required to emit a gzip wrapper.
    #[cfg(feature = "gzip")]
    #[test]
    fn matches_reference_zlib_gzip_framing() {
        check_all(BI_VECTORS_GZIP);
    }

    // =======================================================================
    // The gzip `OS` byte across platforms — what makes the Windows and macOS
    // rows of the `build-test` matrix (AAP gap D3) able to run this gate.
    //
    // `OS_CODE` is the one compile-time platform choice that reaches the wire
    // (AAP §0.6.6), so it is the one axis the tier-1 tables cannot bake. The
    // three tests below make it testable from a single host: the retargeting
    // helper is exercised for every value the cascade can select, the whole
    // 75-row gzip family is replayed against every value, and the host value is
    // diffed against the library's own independent derivation. What they do NOT
    // do is execute the cascade for a foreign target — that is what the native
    // Windows and macOS rows are for.
    // =======================================================================

    /// [`retarget_gzip_os`] rewrites exactly one byte of a gzip-framed expectation
    /// and leaves every other byte, and every other framing, alone.
    ///
    /// Table-driven over all three values `zutil.h`'s cascade can select, so the
    /// Windows (10) and Apple (19) behaviour is checked on a Unix host rather than
    /// waiting for a runner that produces them.
    #[test]
    fn retargeting_the_gzip_os_byte_touches_only_that_byte() {
        // A gzip member long enough to have bytes on both sides of offset 9.
        let baked: Vec<u8> = (0u8..24).map(|i| if i == 9 { 0x03 } else { i }).collect();
        assert_eq!(baked[GZIP_OS_FIELD_OFFSET], NORMALISED_OS_CODE);

        for os_code in PLATFORM_OS_CODES {
            for wbits in [24, 25, 31] {
                let out = retarget_gzip_os(baked.clone(), wbits, os_code);
                assert_eq!(
                    out[GZIP_OS_FIELD_OFFSET], os_code,
                    "windowBits {wbits} selects gzip framing, so byte \
                     {GZIP_OS_FIELD_OFFSET} must become {os_code}"
                );
                assert_eq!(out.len(), baked.len(), "length must not change");
                for (offset, (&got, &want)) in out.iter().zip(baked.iter()).enumerate() {
                    if offset != GZIP_OS_FIELD_OFFSET {
                        assert_eq!(
                            got, want,
                            "byte {offset} must be untouched by OS retargeting"
                        );
                    }
                }
            }

            // Non-gzip framings carry no `OS` field, so nothing may change —
            // this is what keeps the 225 plain-framing rows of `BI_VECTORS`
            // out of the retargeting path entirely.
            for wbits in [-15, -9, 9, 15, 23, 32] {
                assert_eq!(
                    retarget_gzip_os(baked.clone(), wbits, os_code),
                    baked,
                    "windowBits {wbits} is not gzip framing and must pass through \
                     byte-for-byte"
                );
            }
        }

        // The Unix value must be a genuine no-op, which is what makes the
        // retargeting invisible on the hosts the tables were baked on.
        assert_eq!(
            retarget_gzip_os(baked.clone(), 31, NORMALISED_OS_CODE),
            baked,
            "retargeting to the baked value must be a no-op"
        );
    }

    /// The whole [`BI_VECTORS_GZIP`] family holds for **every** platform
    /// `OS_CODE`, not just this host's.
    ///
    /// The `OS` byte is the only part of a gzip member that depends on the build
    /// host — `src/deflate` writes it with a single `put_byte(OS_CODE)` and nothing
    /// else in the stream reads it — so substituting that byte in the produced
    /// stream models what a host with that `OS_CODE` would have emitted.
    /// Retargeting the expectation with the same value and then comparing all 75
    /// rows in full establishes two things from a Linux runner: that the fixture
    /// retargeting is correct, and that every other byte of every row is invariant
    /// under the substitution, for all three values the cascade can select. It does
    /// not execute the `cfg` cascade for a foreign target, so it is a companion to
    /// the native `windows-latest` / `macos-latest` rows rather than a substitute
    /// for them.
    ///
    /// It is deliberately a *superset* of [`matches_reference_zlib_gzip_framing`]
    /// rather than a replacement: that test still runs the unmodified produced
    /// stream against this host's expectation.
    #[cfg(feature = "gzip")]
    #[test]
    fn the_gzip_vector_family_holds_for_every_platform_os_code() {
        let inputs = parse_inputs();

        for os_code in PLATFORM_OS_CODES {
            let mut count = 0usize;
            for line in BI_VECTORS_GZIP.lines().filter(|l| !l.trim().is_empty()) {
                let mut f = line.split_whitespace();
                let idx: usize = f.next().unwrap().parse().unwrap();
                let level: i32 = f.next().unwrap().parse().unwrap();
                let wbits: i32 = f.next().unwrap().parse().unwrap();
                let strat: u8 = f.next().unwrap().parse().unwrap();
                let expected = retarget_gzip_os(unhex(f.next().unwrap()), wbits, os_code);
                assert!(
                    f.next().is_none(),
                    "unexpected trailing field in vector line: {line:?}"
                );
                assert!(
                    (24..=31).contains(&wbits),
                    "BI_VECTORS_GZIP must contain only gzip framings; found \
                     windowBits {wbits}"
                );

                let input = &inputs[idx];
                let mut produced =
                    zlib_rs_deflate_strategy(input, level, wbits, strat_from_id(strat));
                // Stand in for a host whose `OS_CODE` is `os_code`: the encoder
                // writes that byte and only that byte from the platform cascade.
                assert!(produced.len() > GZIP_OS_FIELD_OFFSET);
                produced[GZIP_OS_FIELD_OFFSET] = os_code;

                assert_eq!(
                    produced,
                    expected,
                    "byte-identity mismatch vs C zlib 1.3.2.1-motley on a host with \
                     OS_CODE {os_code}: input #{idx} (len {}), level {level}, \
                     windowBits {wbits}, strategy {:?}",
                    input.len(),
                    strat_from_id(strat),
                );
                count += 1;
            }
            assert_eq!(
                count, 75,
                "BI_VECTORS_GZIP must carry 75 rows; parsed {count} for OS_CODE \
                 {os_code}"
            );
        }
    }

    /// This file's independent `OS_CODE` derivation must agree with the library's.
    ///
    /// [`HOST_GZIP_OS_CODE`] is spelled from raw `#[cfg]` attributes so that an
    /// expectation is never computed from the constant it is testing. That
    /// independence is only worth having if the two derivations are also checked
    /// against each other: a divergence means one of the two cascades is wrong,
    /// and this names which host it happened on. On a Unix host it additionally
    /// pins the value the tier-1 gzip tables were baked with.
    #[test]
    fn the_host_gzip_os_code_agrees_with_the_library_cascade() {
        assert_eq!(
            HOST_GZIP_OS_CODE,
            zlib_rs::util::OS_CODE,
            "tests/interop.rs derives gzip OS byte {HOST_GZIP_OS_CODE} for this \
             target while zlib_rs::util::OS_CODE is {}; one of the two zutil.h \
             cascades is wrong",
            zlib_rs::util::OS_CODE
        );
        assert!(
            PLATFORM_OS_CODES.contains(&HOST_GZIP_OS_CODE),
            "the host OS byte {HOST_GZIP_OS_CODE} must be one of the three values \
             zutil.h's cascade can select: {PLATFORM_OS_CODES:?}"
        );
        #[cfg(all(not(windows), not(target_vendor = "apple")))]
        assert_eq!(
            HOST_GZIP_OS_CODE, NORMALISED_OS_CODE,
            "on a Unix-family host the tier-1 gzip tables must be compared exactly \
             as baked, with no retargeting at all"
        );
    }

    // =======================================================================
    // Wide grid — the 3,750-combination sweep of AAP §0.6.4 sweep 2
    //
    // Additive to the full-hex family above: nothing between `unhex` and
    // `matches_reference_zlib_gzip_framing` is touched by this block, so the 300
    // proven exact-byte assertions carry no risk from the grid expansion.
    // =======================================================================

    /// Byte count of every wide-grid corpus.
    ///
    /// 16 KiB is not an arbitrary "big enough": it is exactly the `memLevel = 8`
    /// symbol budget (`lit_bufsize == 1 << (memLevel + 6) == 16384`), and that is
    /// what makes the `memLevel` axis of the grid *observable at all*. Measured on
    /// this tree, an 8 KiB corpus leaves `memLevel` 8 and 9 emitting
    /// byte-identical output in every one of the 275 (framing, level, strategy)
    /// cells, because neither symbol budget is ever reached and so no block
    /// boundary moves. At 16 KiB the `memLevel` 8/9 pair diverges in 50-181 cells
    /// per corpus and the `memLevel` 1/8 pair in 68-250, while the 512-byte window
    /// of `windowBits = ±9` recycles 32 times over.
    const GRID_LEN: usize = 16384;

    /// The repeating unit of the natural-language grid corpus: 575 bytes of
    /// public-domain pangrams.
    ///
    /// Deliberately **longer than the 512-byte window** of `windowBits = ±9`.
    /// With `MAX_DIST == w_size - MIN_LOOKAHEAD == 512 - 262 == 250`, the
    /// paragraph-to-paragraph back-reference is unreachable at `±9` yet freely
    /// available at `±15`, so the `windowBits` axis changes the emitted token
    /// stream rather than merely the two-byte zlib header. (Measured: the
    /// header-free `-9` vs `-15` pair differs in 90 of 165 cells for this shape.)
    const GRID_PARAGRAPH: &[u8] = b"The quick brown fox jumps over the lazy dog. \
Pack my box with five dozen liquor jugs. How vexingly quick daft zebras jump! \
Sphinx of black quartz, judge my vow. Jackdaws love my big sphinx of quartz. \
The five boxing wizards jump quickly. Bright vixens jump; dozy fowl quack. \
Quick zephyrs blow, vexing daft Jim. Two driven jocks help fax my big quiz. \
Sixty zippers were quickly picked from the woven jute bag. Amazingly few \
discotheques provide jukeboxes. Heavy boxes perform quick waltzes and jigs. \
Whenever the lazy dog dozed, the quick brown fox vaulted the picket fence. ";

    /// `n` bytes of repeated [`GRID_PARAGRAPH`] — the natural-language shape,
    /// truncated mid-paragraph if `n` is not a whole multiple.
    fn grid_text(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let take = (n - out.len()).min(GRID_PARAGRAPH.len());
            out.extend_from_slice(&GRID_PARAGRAPH[..take]);
        }
        out
    }

    /// `n` bytes of the strictly periodic ramp `(i % 256)` — the byte-ramp shape.
    ///
    /// Its 256-byte period also exceeds the 250-byte `MAX_DIST` of a 512-byte
    /// window, which is why this shape is the strongest `windowBits`
    /// discriminator in the set (130 of 165 raw-pair cells differ).
    fn byte_ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i & 0xFF) as u8).collect()
    }

    /// `n` bytes alternating runs of one repeated byte with deterministic
    /// pseudo-random bytes — the mixed shape.
    ///
    /// This forces the match finder to switch repeatedly between the long-match
    /// and no-match regimes and gives `Z_RLE` real runs to find, while the run
    /// bytes recur at distances far beyond a 512-byte window. The generator is the
    /// same `xorshift64*` used by [`incompressible`] with a different hard-coded
    /// seed, so the shape is reproducible bit for bit with no RNG dependency.
    fn mixed_runs(n: usize) -> Vec<u8> {
        let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let mut out = Vec::with_capacity(n);
        let mut round = 0usize;
        while out.len() < n {
            let remaining = n - out.len();
            if round % 2 == 0 {
                // A run of a single byte value, its length and value both a pure
                // function of the round index.
                let run = (61 + round.wrapping_mul(37) % 300).min(remaining);
                let byte = (round.wrapping_mul(13) & 0xFF) as u8;
                out.resize(out.len() + run, byte);
            } else {
                let run = (29 + round.wrapping_mul(53) % 200).min(remaining);
                for _ in 0..run {
                    state ^= state >> 12;
                    state ^= state << 25;
                    state ^= state >> 27;
                    let mixed = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
                    out.push((mixed >> 24) as u8);
                }
            }
            round += 1;
        }
        out
    }

    /// The five wide-grid corpus shapes named by AAP §0.6.4 sweep 2, in the fixed
    /// order the `<input-index>` column of [`BI_GRID`] and [`BI_GRID_GZIP`] refers
    /// to. Each is [`GRID_LEN`] bytes.
    ///
    /// | index | shape | construction |
    /// |-------|-------|--------------|
    /// | 0 | constant | a single repeated byte ([`zeros`]) — maximally compressible; every match is the longest match |
    /// | 1 | pseudo-random / incompressible | fixed-seed `xorshift64*` ([`incompressible`]) — the stored-block fallback and the fruitless-search paths dominate |
    /// | 2 | natural-language text | [`grid_text`] over the 575-byte [`GRID_PARAGRAPH`] |
    /// | 3 | byte ramp | [`byte_ramp`], a 256-byte period |
    /// | 4 | mixed run + random | [`mixed_runs`] |
    ///
    /// Generation is pure arithmetic from hard-coded seeds — no RNG crate, no
    /// clock, no environment, no filesystem — so the corpora are bit-identical on
    /// every platform and every run. That determinism is exactly what allows a
    /// reference encoder written in another language to be fed these same bytes,
    /// and [`grid_corpora_match_the_oracle_inputs`] pins it against the digests of
    /// the files the oracle actually read.
    fn bi_grid_inputs() -> Vec<Vec<u8>> {
        vec![
            zeros(GRID_LEN),
            incompressible(GRID_LEN),
            grid_text(GRID_LEN),
            byte_ramp(GRID_LEN),
            mixed_runs(GRID_LEN),
        ]
    }

    /// `(length, CRC-32)` of each [`bi_grid_inputs`] corpus, measured on the
    /// **files the reference C generator actually read** while the grid tables
    /// were being baked.
    ///
    /// This is the hinge of the whole wide-grid chain of custody: the grid tables
    /// are only meaningful if the Rust corpus generator still produces exactly the
    /// bytes the C encoder was given. A future edit to [`GRID_PARAGRAPH`],
    /// [`mixed_runs`], or [`GRID_LEN`] that silently changed a corpus would
    /// otherwise surface as thousands of confusing digest mismatches; with this
    /// constant it surfaces as one precise failure naming the corpus.
    ///
    /// Cross-verified two ways when baked: an independently written reflected
    /// CRC-32 in the emitter, and the reference C library's own `crc32()` over the
    /// written files. Both agreed with the values below.
    const GRID_CORPUS_DIGESTS: [(usize, u32); 5] = [
        (16384, 0xAB54_D286), // 0 constant
        (16384, 0x8915_B133), // 1 incompressible
        (16384, 0xF478_9B35), // 2 natural-language text
        (16384, 0xE817_22F0), // 3 byte ramp
        (16384, 0x72A6_F1C0), // 4 mixed run + random
    ];

    /// Offset of the `OS` field in a gzip member header (RFC 1952 §2.3.1:
    /// `ID1 ID2 CM FLG MTIME[4] XFL OS`).
    const GZIP_OS_FIELD_OFFSET: usize = 9;

    /// The `OS_CODE` value the platform-neutral gzip tables are normalised to,
    /// and the value every baked full-hex gzip row carries: 3, "Unix".
    const NORMALISED_OS_CODE: u8 = 0x03;

    /// The three gzip `OS` byte values `zutil.h`'s cascade can select, in the
    /// cascade's own order. Used by the platform-simulation tests to exercise the
    /// Windows and Apple values from a host that is neither.
    const PLATFORM_OS_CODES: [u8; 3] = [10, 19, NORMALISED_OS_CODE];

    // -----------------------------------------------------------------------
    // The gzip `OS` byte reference zlib writes on the target being compiled.
    //
    // This is a deliberate second, INDEPENDENT derivation of the same cascade
    // `src/util/mod.rs` holds — spelled as raw `#[cfg]` attributes rather than
    // read from `zlib_rs::util::OS_CODE`, for exactly the reason that module
    // gives for its own `EXPECTED_OS_CODE` mirror: an expectation computed from
    // the constant under test cannot detect a wrong constant. Deriving it here
    // keeps `check_all`'s assertion on byte 9 a real assertion — a library that
    // emitted the wrong `OS_CODE` would still fail byte-identity — while making
    // the expectation correct on every host rather than on Unix only.
    // `the_host_gzip_os_code_agrees_with_the_library_cascade` diffs the two
    // derivations, so the pair cannot drift apart unnoticed.
    // -----------------------------------------------------------------------

    /// The gzip `OS` byte for a Windows target: 10 (`zutil.h` L156-L158).
    #[cfg(windows)]
    const HOST_GZIP_OS_CODE: u8 = 10;

    /// The gzip `OS` byte for a non-Windows Apple target: 19 (`zutil.h`
    /// L168-L170).
    #[cfg(all(not(windows), target_vendor = "apple"))]
    const HOST_GZIP_OS_CODE: u8 = 19;

    /// The gzip `OS` byte for every other target: the Unix default 3
    /// (`zutil.h` L187-L189) — the value the tables below were baked with.
    #[cfg(all(not(windows), not(target_vendor = "apple")))]
    const HOST_GZIP_OS_CODE: u8 = NORMALISED_OS_CODE;

    /// Rewrite the gzip `OS` byte of an *expected* full-hex reference vector from
    /// the [`NORMALISED_OS_CODE`] it was baked with to `os_code`, when
    /// `window_bits` selects gzip framing; return other framings untouched.
    ///
    /// This is the inverse in spirit of [`normalise_gzip_os`] and the opposite in
    /// effect. `normalise_gzip_os` *erases* byte 9 from a produced stream so a
    /// platform-neutral table can be compared; this *retargets* byte 9 of the
    /// expectation so the comparison stays exact — the produced stream is never
    /// touched, and the caller still asserts all of it, byte 9 included. That
    /// distinction is what lets the full-hex family keep a positive assertion on
    /// the `OS` byte on every host (AAP §0.8.1 directive D-5: coverage only ever
    /// increases).
    ///
    /// On a host whose `os_code` is already [`NORMALISED_OS_CODE`] this is a no-op
    /// by construction, so the Unix comparison is bit-for-bit what it was before
    /// the retargeting existed.
    ///
    /// # Panics
    ///
    /// If a gzip-framed expected vector does not carry [`NORMALISED_OS_CODE`] at
    /// [`GZIP_OS_FIELD_OFFSET`]. Silently overwriting an unexpected value would
    /// turn a mis-baked row — or a table accidentally regenerated on a non-Unix
    /// host — into a passing test on every platform, which is precisely the class
    /// of false green this helper must not create.
    fn retarget_gzip_os(mut expected: Vec<u8>, window_bits: i32, os_code: u8) -> Vec<u8> {
        if (24..=31).contains(&window_bits) && expected.len() > GZIP_OS_FIELD_OFFSET {
            assert_eq!(
                expected[GZIP_OS_FIELD_OFFSET], NORMALISED_OS_CODE,
                "a baked gzip reference vector must carry the Unix OS byte \
                 {NORMALISED_OS_CODE} at offset {GZIP_OS_FIELD_OFFSET}; found {} \
                 — the table was not baked on a Unix host, or the offset is wrong",
                expected[GZIP_OS_FIELD_OFFSET]
            );
            expected[GZIP_OS_FIELD_OFFSET] = os_code;
        }
        expected
    }

    /// Force the gzip `OS` byte of `stream` to [`NORMALISED_OS_CODE`] when
    /// `window_bits` selects gzip framing; return other framings untouched.
    ///
    /// `OS_CODE` is a compile-time platform choice — 3 on Unix-family targets, 10
    /// on Windows, 19 on non-Windows Apple (AAP §0.6.6) — so a gzip member is the
    /// one framing whose bytes legitimately differ between build hosts. Rather
    /// than gate the platform-neutral gzip assertions behind a platform predicate
    /// (which would silently delete their coverage everywhere else), this
    /// normalises that single header byte and asserts *everything else* exactly.
    ///
    /// The byte this gives up is not given up by the suite: the full-hex family
    /// [`BI_VECTORS_GZIP`] asserts it positively on **every** host by retargeting
    /// the expectation instead of erasing the observation — see
    /// [`retarget_gzip_os`].
    fn normalise_gzip_os(mut stream: Vec<u8>, window_bits: i32) -> Vec<u8> {
        if (24..=31).contains(&window_bits) && stream.len() > GZIP_OS_FIELD_OFFSET {
            stream[GZIP_OS_FIELD_OFFSET] = NORMALISED_OS_CODE;
        }
        stream
    }

    /// Rows each grid table carries per framing: 5 corpora × 3 `memLevel`s × 11
    /// levels × 5 strategies.
    const GRID_ROWS_PER_FRAMING: usize = 5 * 3 * 11 * 5;

    /// The four non-gzip framings [`BI_GRID`] covers: zlib and raw at both the
    /// 32 KiB and the 512-byte window.
    const GRID_PLAIN_FRAMINGS: [i32; 4] = [-15, -9, 9, 15];

    /// The non-gzip half of the wide grid, baked as `(length, CRC-32)` digests:
    /// 3,300 rows = 5 corpora × `windowBits` {15, −15, 9, −9} × `memLevel`
    /// {1, 8, 9} × levels {−1, 0..=9} × all five strategies. Together with
    /// [`BI_GRID_GZIP`]'s 825 `windowBits = 31` rows that is 4,125 configurations
    /// — the AAP's 3,750-combination grid plus the `Z_DEFAULT_COMPRESSION` (−1)
    /// sentinel carried as a superset.
    ///
    /// Line format: `<input-index> <level> <windowBits> <memLevel> <strategy-id>
    /// <output-length>:<crc32-hex>`, rows sorted by exactly that column order so a
    /// regeneration diff is readable.
    ///
    /// # What this table does and does not prove
    ///
    /// A `(length, CRC-32)` pair is a **digest, not the literal bytes**. This
    /// table proves that `zlib-rs` emits a stream of *the same length* with *the
    /// same CRC-32* as reference zlib for each configuration. That is exactly the
    /// comparison basis of AAP §0.6.4 sweep 1 — "comparing return code, output
    /// length, and CRC-32 of the compressed output" — and for a 32-bit checksum
    /// over a fixed length it is overwhelming, but it is not the absolute
    /// statement that the bytes are equal. The literal-bytes proof lives in the
    /// full-hex families: [`BI_VECTORS`] / [`BI_VECTORS_GZIP`] for the narrow
    /// `memLevel = 8` matrix and [`BI_EXTREMES`] / [`BI_EXTREMES_GZIP`] for the
    /// `memLevel` corners. The return code half of the sweep 1 comparison *is*
    /// absolute: [`zlib_rs_deflate_full`] panics on anything other than a clean
    /// `Z_STREAM_END`.
    ///
    /// Full hex for 4,125 rows over 16 KiB corpora would be megabytes of source;
    /// the digest encoding is what makes a grid this wide affordable in-tree.
    ///
    /// # Why the crate's own CRC-32 is not circular here
    ///
    /// The digests are verified with `zlib_rs::crc32`. That is sound rather than
    /// self-referential because CRC-32 correctness is pinned *independently of the
    /// deflate engine* by `tests/checksum.rs`, against fixed known-answer
    /// constants: `crc32(0, b"123456789") == 0xCBF4_3926`, the reflected
    /// polynomial `0xEDB88320` reconstructed bit by bit, and the empty-input
    /// identity. A deflate regression therefore cannot hide inside the checksum,
    /// and no dependency has to be added merely to hash test output.
    ///
    /// # Provenance
    ///
    /// Produced by the genuine C zlib `1.3.2.1-motley`, built from this
    /// repository's own retained `*.c` sources with
    /// `gcc -O2 -D_LARGEFILE64_SOURCE=1 -DHAVE_UNISTD_H`, driven exactly as
    /// [`zlib_rs_deflate_full`] drives the Rust engine. **Not one value in this
    /// table was obtained by running `zlib-rs`** — a table generated from the
    /// implementation under test would assert only that the implementation equals
    /// itself. See the regeneration recipe in this module's documentation.
    const BI_GRID: &str = "\
    0 -1 -15 1 0 33:fa6e1d63
    0 -1 -15 1 1 33:fa6e1d63
    0 -1 -15 1 2 3534:1701ede1
    0 -1 -15 1 3 32:798d0591
    0 -1 -15 1 4 108:e40f7be3
    0 -1 -15 8 0 33:fa6e1d63
    0 -1 -15 8 1 33:fa6e1d63
    0 -1 -15 8 2 2062:fa0ebeaa
    0 -1 -15 8 3 32:798d0591
    0 -1 -15 8 4 108:e40f7be3
    0 -1 -15 9 0 33:fa6e1d63
    0 -1 -15 9 1 33:fa6e1d63
    0 -1 -15 9 2 2060:9b0c6f86
    0 -1 -15 9 3 32:798d0591
    0 -1 -15 9 4 108:e40f7be3
    0 -1 -9 1 0 33:fa6e1d63
    0 -1 -9 1 1 33:fa6e1d63
    0 -1 -9 1 2 3534:1701ede1
    0 -1 -9 1 3 32:798d0591
    0 -1 -9 1 4 108:e40f7be3
    0 -1 -9 8 0 33:fa6e1d63
    0 -1 -9 8 1 33:fa6e1d63
    0 -1 -9 8 2 2062:fa0ebeaa
    0 -1 -9 8 3 32:798d0591
    0 -1 -9 8 4 108:e40f7be3
    0 -1 -9 9 0 33:fa6e1d63
    0 -1 -9 9 1 33:fa6e1d63
    0 -1 -9 9 2 2060:9b0c6f86
    0 -1 -9 9 3 32:798d0591
    0 -1 -9 9 4 108:e40f7be3
    0 -1 9 1 0 39:16288ae0
    0 -1 9 1 1 39:16288ae0
    0 -1 9 1 2 3540:1a57629a
    0 -1 9 1 3 38:a7621a42
    0 -1 9 1 4 114:a3dca8d9
    0 -1 9 8 0 39:16288ae0
    0 -1 9 8 1 39:16288ae0
    0 -1 9 8 2 2068:48701ad4
    0 -1 9 8 3 38:a7621a42
    0 -1 9 8 4 114:a3dca8d9
    0 -1 9 9 0 39:16288ae0
    0 -1 9 9 1 39:16288ae0
    0 -1 9 9 2 2066:da94d942
    0 -1 9 9 3 38:a7621a42
    0 -1 9 9 4 114:a3dca8d9
    0 -1 15 1 0 39:85766fe0
    0 -1 15 1 1 39:85766fe0
    0 -1 15 1 2 3540:ee1d7bc5
    0 -1 15 1 3 38:12b4cc6d
    0 -1 15 1 4 114:34c28ff3
    0 -1 15 8 0 39:85766fe0
    0 -1 15 8 1 39:85766fe0
    0 -1 15 8 2 2068:d571dfc2
    0 -1 15 8 3 38:12b4cc6d
    0 -1 15 8 4 114:34c28ff3
    0 -1 15 9 0 39:85766fe0
    0 -1 15 9 1 39:85766fe0
    0 -1 15 9 2 2066:bed9a725
    0 -1 15 9 3 38:12b4cc6d
    0 -1 15 9 4 114:34c28ff3
    0 0 -15 1 0 16389:171a159c
    0 0 -15 1 1 16389:171a159c
    0 0 -15 1 2 16389:171a159c
    0 0 -15 1 3 16389:171a159c
    0 0 -15 1 4 16389:171a159c
    0 0 -15 8 0 16389:171a159c
    0 0 -15 8 1 16389:171a159c
    0 0 -15 8 2 16389:171a159c
    0 0 -15 8 3 16389:171a159c
    0 0 -15 8 4 16389:171a159c
    0 0 -15 9 0 16389:171a159c
    0 0 -15 9 1 16389:171a159c
    0 0 -15 9 2 16389:171a159c
    0 0 -15 9 3 16389:171a159c
    0 0 -15 9 4 16389:171a159c
    0 0 -9 1 0 16389:171a159c
    0 0 -9 1 1 16389:171a159c
    0 0 -9 1 2 16389:171a159c
    0 0 -9 1 3 16389:171a159c
    0 0 -9 1 4 16389:171a159c
    0 0 -9 8 0 16389:171a159c
    0 0 -9 8 1 16389:171a159c
    0 0 -9 8 2 16389:171a159c
    0 0 -9 8 3 16389:171a159c
    0 0 -9 8 4 16389:171a159c
    0 0 -9 9 0 16389:171a159c
    0 0 -9 9 1 16389:171a159c
    0 0 -9 9 2 16389:171a159c
    0 0 -9 9 3 16389:171a159c
    0 0 -9 9 4 16389:171a159c
    0 0 9 1 0 16395:07cacd2e
    0 0 9 1 1 16395:07cacd2e
    0 0 9 1 2 16395:07cacd2e
    0 0 9 1 3 16395:07cacd2e
    0 0 9 1 4 16395:07cacd2e
    0 0 9 8 0 16395:07cacd2e
    0 0 9 8 1 16395:07cacd2e
    0 0 9 8 2 16395:07cacd2e
    0 0 9 8 3 16395:07cacd2e
    0 0 9 8 4 16395:07cacd2e
    0 0 9 9 0 16395:07cacd2e
    0 0 9 9 1 16395:07cacd2e
    0 0 9 9 2 16395:07cacd2e
    0 0 9 9 3 16395:07cacd2e
    0 0 9 9 4 16395:07cacd2e
    0 0 15 1 0 16395:74f47d21
    0 0 15 1 1 16395:74f47d21
    0 0 15 1 2 16395:74f47d21
    0 0 15 1 3 16395:74f47d21
    0 0 15 1 4 16395:74f47d21
    0 0 15 8 0 16395:74f47d21
    0 0 15 8 1 16395:74f47d21
    0 0 15 8 2 16395:74f47d21
    0 0 15 8 3 16395:74f47d21
    0 0 15 8 4 16395:74f47d21
    0 0 15 9 0 16395:74f47d21
    0 0 15 9 1 16395:74f47d21
    0 0 15 9 2 16395:74f47d21
    0 0 15 9 3 16395:74f47d21
    0 0 15 9 4 16395:74f47d21
    0 1 -15 1 0 89:d713a699
    0 1 -15 1 1 89:d713a699
    0 1 -15 1 2 3534:1701ede1
    0 1 -15 1 3 32:798d0591
    0 1 -15 1 4 163:fa139b57
    0 1 -15 8 0 89:d713a699
    0 1 -15 8 1 89:d713a699
    0 1 -15 8 2 2062:fa0ebeaa
    0 1 -15 8 3 32:798d0591
    0 1 -15 8 4 163:fa139b57
    0 1 -15 9 0 89:d713a699
    0 1 -15 9 1 89:d713a699
    0 1 -15 9 2 2060:9b0c6f86
    0 1 -15 9 3 32:798d0591
    0 1 -15 9 4 163:fa139b57
    0 1 -9 1 0 50:43151ffd
    0 1 -9 1 1 50:43151ffd
    0 1 -9 1 2 3534:1701ede1
    0 1 -9 1 3 32:798d0591
    0 1 -9 1 4 172:22889006
    0 1 -9 8 0 48:ede351df
    0 1 -9 8 1 48:ede351df
    0 1 -9 8 2 2062:fa0ebeaa
    0 1 -9 8 3 32:798d0591
    0 1 -9 8 4 171:106154cd
    0 1 -9 9 0 48:ede351df
    0 1 -9 9 1 48:ede351df
    0 1 -9 9 2 2060:9b0c6f86
    0 1 -9 9 3 32:798d0591
    0 1 -9 9 4 171:106154cd
    0 1 9 1 0 56:da651091
    0 1 9 1 1 56:da651091
    0 1 9 1 2 3540:1a57629a
    0 1 9 1 3 38:a7621a42
    0 1 9 1 4 178:6d9d5899
    0 1 9 8 0 54:25b9f357
    0 1 9 8 1 54:25b9f357
    0 1 9 8 2 2068:48701ad4
    0 1 9 8 3 38:a7621a42
    0 1 9 8 4 177:1536e6fa
    0 1 9 9 0 54:25b9f357
    0 1 9 9 1 54:25b9f357
    0 1 9 9 2 2066:da94d942
    0 1 9 9 3 38:a7621a42
    0 1 9 9 4 177:1536e6fa
    0 1 15 1 0 95:d195bd6c
    0 1 15 1 1 95:d195bd6c
    0 1 15 1 2 3540:ee1d7bc5
    0 1 15 1 3 38:12b4cc6d
    0 1 15 1 4 169:f995e724
    0 1 15 8 0 95:d195bd6c
    0 1 15 8 1 95:d195bd6c
    0 1 15 8 2 2068:d571dfc2
    0 1 15 8 3 38:12b4cc6d
    0 1 15 8 4 169:f995e724
    0 1 15 9 0 95:d195bd6c
    0 1 15 9 1 95:d195bd6c
    0 1 15 9 2 2066:bed9a725
    0 1 15 9 3 38:12b4cc6d
    0 1 15 9 4 169:f995e724
    0 2 -15 1 0 89:d713a699
    0 2 -15 1 1 89:d713a699
    0 2 -15 1 2 3534:1701ede1
    0 2 -15 1 3 32:798d0591
    0 2 -15 1 4 163:fa139b57
    0 2 -15 8 0 89:d713a699
    0 2 -15 8 1 89:d713a699
    0 2 -15 8 2 2062:fa0ebeaa
    0 2 -15 8 3 32:798d0591
    0 2 -15 8 4 163:fa139b57
    0 2 -15 9 0 89:d713a699
    0 2 -15 9 1 89:d713a699
    0 2 -15 9 2 2060:9b0c6f86
    0 2 -15 9 3 32:798d0591
    0 2 -15 9 4 163:fa139b57
    0 2 -9 1 0 50:43151ffd
    0 2 -9 1 1 50:43151ffd
    0 2 -9 1 2 3534:1701ede1
    0 2 -9 1 3 32:798d0591
    0 2 -9 1 4 172:22889006
    0 2 -9 8 0 48:ede351df
    0 2 -9 8 1 48:ede351df
    0 2 -9 8 2 2062:fa0ebeaa
    0 2 -9 8 3 32:798d0591
    0 2 -9 8 4 171:106154cd
    0 2 -9 9 0 48:ede351df
    0 2 -9 9 1 48:ede351df
    0 2 -9 9 2 2060:9b0c6f86
    0 2 -9 9 3 32:798d0591
    0 2 -9 9 4 171:106154cd
    0 2 9 1 0 56:30d1cd7d
    0 2 9 1 1 56:30d1cd7d
    0 2 9 1 2 3540:1a57629a
    0 2 9 1 3 38:a7621a42
    0 2 9 1 4 178:6d9d5899
    0 2 9 8 0 54:d493a489
    0 2 9 8 1 54:d493a489
    0 2 9 8 2 2068:48701ad4
    0 2 9 8 3 38:a7621a42
    0 2 9 8 4 177:1536e6fa
    0 2 9 9 0 54:d493a489
    0 2 9 9 1 54:d493a489
    0 2 9 9 2 2066:da94d942
    0 2 9 9 3 38:a7621a42
    0 2 9 9 4 177:1536e6fa
    0 2 15 1 0 95:36ce048a
    0 2 15 1 1 95:36ce048a
    0 2 15 1 2 3540:ee1d7bc5
    0 2 15 1 3 38:12b4cc6d
    0 2 15 1 4 169:f995e724
    0 2 15 8 0 95:36ce048a
    0 2 15 8 1 95:36ce048a
    0 2 15 8 2 2068:d571dfc2
    0 2 15 8 3 38:12b4cc6d
    0 2 15 8 4 169:f995e724
    0 2 15 9 0 95:36ce048a
    0 2 15 9 1 95:36ce048a
    0 2 15 9 2 2066:bed9a725
    0 2 15 9 3 38:12b4cc6d
    0 2 15 9 4 169:f995e724
    0 3 -15 1 0 89:d713a699
    0 3 -15 1 1 89:d713a699
    0 3 -15 1 2 3534:1701ede1
    0 3 -15 1 3 32:798d0591
    0 3 -15 1 4 163:fa139b57
    0 3 -15 8 0 89:d713a699
    0 3 -15 8 1 89:d713a699
    0 3 -15 8 2 2062:fa0ebeaa
    0 3 -15 8 3 32:798d0591
    0 3 -15 8 4 163:fa139b57
    0 3 -15 9 0 89:d713a699
    0 3 -15 9 1 89:d713a699
    0 3 -15 9 2 2060:9b0c6f86
    0 3 -15 9 3 32:798d0591
    0 3 -15 9 4 163:fa139b57
    0 3 -9 1 0 50:43151ffd
    0 3 -9 1 1 50:43151ffd
    0 3 -9 1 2 3534:1701ede1
    0 3 -9 1 3 32:798d0591
    0 3 -9 1 4 172:22889006
    0 3 -9 8 0 48:ede351df
    0 3 -9 8 1 48:ede351df
    0 3 -9 8 2 2062:fa0ebeaa
    0 3 -9 8 3 32:798d0591
    0 3 -9 8 4 171:106154cd
    0 3 -9 9 0 48:ede351df
    0 3 -9 9 1 48:ede351df
    0 3 -9 9 2 2060:9b0c6f86
    0 3 -9 9 3 32:798d0591
    0 3 -9 9 4 171:106154cd
    0 3 9 1 0 56:30d1cd7d
    0 3 9 1 1 56:30d1cd7d
    0 3 9 1 2 3540:1a57629a
    0 3 9 1 3 38:a7621a42
    0 3 9 1 4 178:6d9d5899
    0 3 9 8 0 54:d493a489
    0 3 9 8 1 54:d493a489
    0 3 9 8 2 2068:48701ad4
    0 3 9 8 3 38:a7621a42
    0 3 9 8 4 177:1536e6fa
    0 3 9 9 0 54:d493a489
    0 3 9 9 1 54:d493a489
    0 3 9 9 2 2066:da94d942
    0 3 9 9 3 38:a7621a42
    0 3 9 9 4 177:1536e6fa
    0 3 15 1 0 95:36ce048a
    0 3 15 1 1 95:36ce048a
    0 3 15 1 2 3540:ee1d7bc5
    0 3 15 1 3 38:12b4cc6d
    0 3 15 1 4 169:f995e724
    0 3 15 8 0 95:36ce048a
    0 3 15 8 1 95:36ce048a
    0 3 15 8 2 2068:d571dfc2
    0 3 15 8 3 38:12b4cc6d
    0 3 15 8 4 169:f995e724
    0 3 15 9 0 95:36ce048a
    0 3 15 9 1 95:36ce048a
    0 3 15 9 2 2066:bed9a725
    0 3 15 9 3 38:12b4cc6d
    0 3 15 9 4 169:f995e724
    0 4 -15 1 0 33:fa6e1d63
    0 4 -15 1 1 33:fa6e1d63
    0 4 -15 1 2 3534:1701ede1
    0 4 -15 1 3 32:798d0591
    0 4 -15 1 4 108:e40f7be3
    0 4 -15 8 0 33:fa6e1d63
    0 4 -15 8 1 33:fa6e1d63
    0 4 -15 8 2 2062:fa0ebeaa
    0 4 -15 8 3 32:798d0591
    0 4 -15 8 4 108:e40f7be3
    0 4 -15 9 0 33:fa6e1d63
    0 4 -15 9 1 33:fa6e1d63
    0 4 -15 9 2 2060:9b0c6f86
    0 4 -15 9 3 32:798d0591
    0 4 -15 9 4 108:e40f7be3
    0 4 -9 1 0 33:fa6e1d63
    0 4 -9 1 1 33:fa6e1d63
    0 4 -9 1 2 3534:1701ede1
    0 4 -9 1 3 32:798d0591
    0 4 -9 1 4 108:e40f7be3
    0 4 -9 8 0 33:fa6e1d63
    0 4 -9 8 1 33:fa6e1d63
    0 4 -9 8 2 2062:fa0ebeaa
    0 4 -9 8 3 32:798d0591
    0 4 -9 8 4 108:e40f7be3
    0 4 -9 9 0 33:fa6e1d63
    0 4 -9 9 1 33:fa6e1d63
    0 4 -9 9 2 2060:9b0c6f86
    0 4 -9 9 3 32:798d0591
    0 4 -9 9 4 108:e40f7be3
    0 4 9 1 0 39:c0d038bd
    0 4 9 1 1 39:c0d038bd
    0 4 9 1 2 3540:1a57629a
    0 4 9 1 3 38:a7621a42
    0 4 9 1 4 114:a3dca8d9
    0 4 9 8 0 39:c0d038bd
    0 4 9 8 1 39:c0d038bd
    0 4 9 8 2 2068:48701ad4
    0 4 9 8 3 38:a7621a42
    0 4 9 8 4 114:a3dca8d9
    0 4 9 9 0 39:c0d038bd
    0 4 9 9 1 39:c0d038bd
    0 4 9 9 2 2066:da94d942
    0 4 9 9 3 38:a7621a42
    0 4 9 9 4 114:a3dca8d9
    0 4 15 1 0 39:538eddbd
    0 4 15 1 1 39:538eddbd
    0 4 15 1 2 3540:ee1d7bc5
    0 4 15 1 3 38:12b4cc6d
    0 4 15 1 4 114:34c28ff3
    0 4 15 8 0 39:538eddbd
    0 4 15 8 1 39:538eddbd
    0 4 15 8 2 2068:d571dfc2
    0 4 15 8 3 38:12b4cc6d
    0 4 15 8 4 114:34c28ff3
    0 4 15 9 0 39:538eddbd
    0 4 15 9 1 39:538eddbd
    0 4 15 9 2 2066:bed9a725
    0 4 15 9 3 38:12b4cc6d
    0 4 15 9 4 114:34c28ff3
    0 5 -15 1 0 33:fa6e1d63
    0 5 -15 1 1 33:fa6e1d63
    0 5 -15 1 2 3534:1701ede1
    0 5 -15 1 3 32:798d0591
    0 5 -15 1 4 108:e40f7be3
    0 5 -15 8 0 33:fa6e1d63
    0 5 -15 8 1 33:fa6e1d63
    0 5 -15 8 2 2062:fa0ebeaa
    0 5 -15 8 3 32:798d0591
    0 5 -15 8 4 108:e40f7be3
    0 5 -15 9 0 33:fa6e1d63
    0 5 -15 9 1 33:fa6e1d63
    0 5 -15 9 2 2060:9b0c6f86
    0 5 -15 9 3 32:798d0591
    0 5 -15 9 4 108:e40f7be3
    0 5 -9 1 0 33:fa6e1d63
    0 5 -9 1 1 33:fa6e1d63
    0 5 -9 1 2 3534:1701ede1
    0 5 -9 1 3 32:798d0591
    0 5 -9 1 4 108:e40f7be3
    0 5 -9 8 0 33:fa6e1d63
    0 5 -9 8 1 33:fa6e1d63
    0 5 -9 8 2 2062:fa0ebeaa
    0 5 -9 8 3 32:798d0591
    0 5 -9 8 4 108:e40f7be3
    0 5 -9 9 0 33:fa6e1d63
    0 5 -9 9 1 33:fa6e1d63
    0 5 -9 9 2 2060:9b0c6f86
    0 5 -9 9 3 32:798d0591
    0 5 -9 9 4 108:e40f7be3
    0 5 9 1 0 39:c0d038bd
    0 5 9 1 1 39:c0d038bd
    0 5 9 1 2 3540:1a57629a
    0 5 9 1 3 38:a7621a42
    0 5 9 1 4 114:a3dca8d9
    0 5 9 8 0 39:c0d038bd
    0 5 9 8 1 39:c0d038bd
    0 5 9 8 2 2068:48701ad4
    0 5 9 8 3 38:a7621a42
    0 5 9 8 4 114:a3dca8d9
    0 5 9 9 0 39:c0d038bd
    0 5 9 9 1 39:c0d038bd
    0 5 9 9 2 2066:da94d942
    0 5 9 9 3 38:a7621a42
    0 5 9 9 4 114:a3dca8d9
    0 5 15 1 0 39:538eddbd
    0 5 15 1 1 39:538eddbd
    0 5 15 1 2 3540:ee1d7bc5
    0 5 15 1 3 38:12b4cc6d
    0 5 15 1 4 114:34c28ff3
    0 5 15 8 0 39:538eddbd
    0 5 15 8 1 39:538eddbd
    0 5 15 8 2 2068:d571dfc2
    0 5 15 8 3 38:12b4cc6d
    0 5 15 8 4 114:34c28ff3
    0 5 15 9 0 39:538eddbd
    0 5 15 9 1 39:538eddbd
    0 5 15 9 2 2066:bed9a725
    0 5 15 9 3 38:12b4cc6d
    0 5 15 9 4 114:34c28ff3
    0 6 -15 1 0 33:fa6e1d63
    0 6 -15 1 1 33:fa6e1d63
    0 6 -15 1 2 3534:1701ede1
    0 6 -15 1 3 32:798d0591
    0 6 -15 1 4 108:e40f7be3
    0 6 -15 8 0 33:fa6e1d63
    0 6 -15 8 1 33:fa6e1d63
    0 6 -15 8 2 2062:fa0ebeaa
    0 6 -15 8 3 32:798d0591
    0 6 -15 8 4 108:e40f7be3
    0 6 -15 9 0 33:fa6e1d63
    0 6 -15 9 1 33:fa6e1d63
    0 6 -15 9 2 2060:9b0c6f86
    0 6 -15 9 3 32:798d0591
    0 6 -15 9 4 108:e40f7be3
    0 6 -9 1 0 33:fa6e1d63
    0 6 -9 1 1 33:fa6e1d63
    0 6 -9 1 2 3534:1701ede1
    0 6 -9 1 3 32:798d0591
    0 6 -9 1 4 108:e40f7be3
    0 6 -9 8 0 33:fa6e1d63
    0 6 -9 8 1 33:fa6e1d63
    0 6 -9 8 2 2062:fa0ebeaa
    0 6 -9 8 3 32:798d0591
    0 6 -9 8 4 108:e40f7be3
    0 6 -9 9 0 33:fa6e1d63
    0 6 -9 9 1 33:fa6e1d63
    0 6 -9 9 2 2060:9b0c6f86
    0 6 -9 9 3 32:798d0591
    0 6 -9 9 4 108:e40f7be3
    0 6 9 1 0 39:16288ae0
    0 6 9 1 1 39:16288ae0
    0 6 9 1 2 3540:1a57629a
    0 6 9 1 3 38:a7621a42
    0 6 9 1 4 114:a3dca8d9
    0 6 9 8 0 39:16288ae0
    0 6 9 8 1 39:16288ae0
    0 6 9 8 2 2068:48701ad4
    0 6 9 8 3 38:a7621a42
    0 6 9 8 4 114:a3dca8d9
    0 6 9 9 0 39:16288ae0
    0 6 9 9 1 39:16288ae0
    0 6 9 9 2 2066:da94d942
    0 6 9 9 3 38:a7621a42
    0 6 9 9 4 114:a3dca8d9
    0 6 15 1 0 39:85766fe0
    0 6 15 1 1 39:85766fe0
    0 6 15 1 2 3540:ee1d7bc5
    0 6 15 1 3 38:12b4cc6d
    0 6 15 1 4 114:34c28ff3
    0 6 15 8 0 39:85766fe0
    0 6 15 8 1 39:85766fe0
    0 6 15 8 2 2068:d571dfc2
    0 6 15 8 3 38:12b4cc6d
    0 6 15 8 4 114:34c28ff3
    0 6 15 9 0 39:85766fe0
    0 6 15 9 1 39:85766fe0
    0 6 15 9 2 2066:bed9a725
    0 6 15 9 3 38:12b4cc6d
    0 6 15 9 4 114:34c28ff3
    0 7 -15 1 0 33:fa6e1d63
    0 7 -15 1 1 33:fa6e1d63
    0 7 -15 1 2 3534:1701ede1
    0 7 -15 1 3 32:798d0591
    0 7 -15 1 4 108:e40f7be3
    0 7 -15 8 0 33:fa6e1d63
    0 7 -15 8 1 33:fa6e1d63
    0 7 -15 8 2 2062:fa0ebeaa
    0 7 -15 8 3 32:798d0591
    0 7 -15 8 4 108:e40f7be3
    0 7 -15 9 0 33:fa6e1d63
    0 7 -15 9 1 33:fa6e1d63
    0 7 -15 9 2 2060:9b0c6f86
    0 7 -15 9 3 32:798d0591
    0 7 -15 9 4 108:e40f7be3
    0 7 -9 1 0 33:fa6e1d63
    0 7 -9 1 1 33:fa6e1d63
    0 7 -9 1 2 3534:1701ede1
    0 7 -9 1 3 32:798d0591
    0 7 -9 1 4 108:e40f7be3
    0 7 -9 8 0 33:fa6e1d63
    0 7 -9 8 1 33:fa6e1d63
    0 7 -9 8 2 2062:fa0ebeaa
    0 7 -9 8 3 32:798d0591
    0 7 -9 8 4 108:e40f7be3
    0 7 -9 9 0 33:fa6e1d63
    0 7 -9 9 1 33:fa6e1d63
    0 7 -9 9 2 2060:9b0c6f86
    0 7 -9 9 3 32:798d0591
    0 7 -9 9 4 108:e40f7be3
    0 7 9 1 0 39:36a04d59
    0 7 9 1 1 39:36a04d59
    0 7 9 1 2 3540:1a57629a
    0 7 9 1 3 38:a7621a42
    0 7 9 1 4 114:a3dca8d9
    0 7 9 8 0 39:36a04d59
    0 7 9 8 1 39:36a04d59
    0 7 9 8 2 2068:48701ad4
    0 7 9 8 3 38:a7621a42
    0 7 9 8 4 114:a3dca8d9
    0 7 9 9 0 39:36a04d59
    0 7 9 9 1 39:36a04d59
    0 7 9 9 2 2066:da94d942
    0 7 9 9 3 38:a7621a42
    0 7 9 9 4 114:a3dca8d9
    0 7 15 1 0 39:a5fea859
    0 7 15 1 1 39:a5fea859
    0 7 15 1 2 3540:ee1d7bc5
    0 7 15 1 3 38:12b4cc6d
    0 7 15 1 4 114:34c28ff3
    0 7 15 8 0 39:a5fea859
    0 7 15 8 1 39:a5fea859
    0 7 15 8 2 2068:d571dfc2
    0 7 15 8 3 38:12b4cc6d
    0 7 15 8 4 114:34c28ff3
    0 7 15 9 0 39:a5fea859
    0 7 15 9 1 39:a5fea859
    0 7 15 9 2 2066:bed9a725
    0 7 15 9 3 38:12b4cc6d
    0 7 15 9 4 114:34c28ff3
    0 8 -15 1 0 33:fa6e1d63
    0 8 -15 1 1 33:fa6e1d63
    0 8 -15 1 2 3534:1701ede1
    0 8 -15 1 3 32:798d0591
    0 8 -15 1 4 108:e40f7be3
    0 8 -15 8 0 33:fa6e1d63
    0 8 -15 8 1 33:fa6e1d63
    0 8 -15 8 2 2062:fa0ebeaa
    0 8 -15 8 3 32:798d0591
    0 8 -15 8 4 108:e40f7be3
    0 8 -15 9 0 33:fa6e1d63
    0 8 -15 9 1 33:fa6e1d63
    0 8 -15 9 2 2060:9b0c6f86
    0 8 -15 9 3 32:798d0591
    0 8 -15 9 4 108:e40f7be3
    0 8 -9 1 0 33:fa6e1d63
    0 8 -9 1 1 33:fa6e1d63
    0 8 -9 1 2 3534:1701ede1
    0 8 -9 1 3 32:798d0591
    0 8 -9 1 4 108:e40f7be3
    0 8 -9 8 0 33:fa6e1d63
    0 8 -9 8 1 33:fa6e1d63
    0 8 -9 8 2 2062:fa0ebeaa
    0 8 -9 8 3 32:798d0591
    0 8 -9 8 4 108:e40f7be3
    0 8 -9 9 0 33:fa6e1d63
    0 8 -9 9 1 33:fa6e1d63
    0 8 -9 9 2 2060:9b0c6f86
    0 8 -9 9 3 32:798d0591
    0 8 -9 9 4 108:e40f7be3
    0 8 9 1 0 39:36a04d59
    0 8 9 1 1 39:36a04d59
    0 8 9 1 2 3540:1a57629a
    0 8 9 1 3 38:a7621a42
    0 8 9 1 4 114:a3dca8d9
    0 8 9 8 0 39:36a04d59
    0 8 9 8 1 39:36a04d59
    0 8 9 8 2 2068:48701ad4
    0 8 9 8 3 38:a7621a42
    0 8 9 8 4 114:a3dca8d9
    0 8 9 9 0 39:36a04d59
    0 8 9 9 1 39:36a04d59
    0 8 9 9 2 2066:da94d942
    0 8 9 9 3 38:a7621a42
    0 8 9 9 4 114:a3dca8d9
    0 8 15 1 0 39:a5fea859
    0 8 15 1 1 39:a5fea859
    0 8 15 1 2 3540:ee1d7bc5
    0 8 15 1 3 38:12b4cc6d
    0 8 15 1 4 114:34c28ff3
    0 8 15 8 0 39:a5fea859
    0 8 15 8 1 39:a5fea859
    0 8 15 8 2 2068:d571dfc2
    0 8 15 8 3 38:12b4cc6d
    0 8 15 8 4 114:34c28ff3
    0 8 15 9 0 39:a5fea859
    0 8 15 9 1 39:a5fea859
    0 8 15 9 2 2066:bed9a725
    0 8 15 9 3 38:12b4cc6d
    0 8 15 9 4 114:34c28ff3
    0 9 -15 1 0 33:fa6e1d63
    0 9 -15 1 1 33:fa6e1d63
    0 9 -15 1 2 3534:1701ede1
    0 9 -15 1 3 32:798d0591
    0 9 -15 1 4 108:e40f7be3
    0 9 -15 8 0 33:fa6e1d63
    0 9 -15 8 1 33:fa6e1d63
    0 9 -15 8 2 2062:fa0ebeaa
    0 9 -15 8 3 32:798d0591
    0 9 -15 8 4 108:e40f7be3
    0 9 -15 9 0 33:fa6e1d63
    0 9 -15 9 1 33:fa6e1d63
    0 9 -15 9 2 2060:9b0c6f86
    0 9 -15 9 3 32:798d0591
    0 9 -15 9 4 108:e40f7be3
    0 9 -9 1 0 33:fa6e1d63
    0 9 -9 1 1 33:fa6e1d63
    0 9 -9 1 2 3534:1701ede1
    0 9 -9 1 3 32:798d0591
    0 9 -9 1 4 108:e40f7be3
    0 9 -9 8 0 33:fa6e1d63
    0 9 -9 8 1 33:fa6e1d63
    0 9 -9 8 2 2062:fa0ebeaa
    0 9 -9 8 3 32:798d0591
    0 9 -9 8 4 108:e40f7be3
    0 9 -9 9 0 33:fa6e1d63
    0 9 -9 9 1 33:fa6e1d63
    0 9 -9 9 2 2060:9b0c6f86
    0 9 -9 9 3 32:798d0591
    0 9 -9 9 4 108:e40f7be3
    0 9 9 1 0 39:36a04d59
    0 9 9 1 1 39:36a04d59
    0 9 9 1 2 3540:1a57629a
    0 9 9 1 3 38:a7621a42
    0 9 9 1 4 114:a3dca8d9
    0 9 9 8 0 39:36a04d59
    0 9 9 8 1 39:36a04d59
    0 9 9 8 2 2068:48701ad4
    0 9 9 8 3 38:a7621a42
    0 9 9 8 4 114:a3dca8d9
    0 9 9 9 0 39:36a04d59
    0 9 9 9 1 39:36a04d59
    0 9 9 9 2 2066:da94d942
    0 9 9 9 3 38:a7621a42
    0 9 9 9 4 114:a3dca8d9
    0 9 15 1 0 39:a5fea859
    0 9 15 1 1 39:a5fea859
    0 9 15 1 2 3540:ee1d7bc5
    0 9 15 1 3 38:12b4cc6d
    0 9 15 1 4 114:34c28ff3
    0 9 15 8 0 39:a5fea859
    0 9 15 8 1 39:a5fea859
    0 9 15 8 2 2068:d571dfc2
    0 9 15 8 3 38:12b4cc6d
    0 9 15 8 4 114:34c28ff3
    0 9 15 9 0 39:a5fea859
    0 9 15 9 1 39:a5fea859
    0 9 15 9 2 2066:bed9a725
    0 9 15 9 3 38:12b4cc6d
    0 9 15 9 4 114:34c28ff3
    1 -1 -15 1 0 17029:6fe7b18c
    1 -1 -15 1 1 17031:f2fae3e3
    1 -1 -15 1 2 17031:f2fae3e3
    1 -1 -15 1 3 17031:f2fae3e3
    1 -1 -15 1 4 17029:6fe7b18c
    1 -1 -15 8 0 16389:355b7629
    1 -1 -15 8 1 16391:c309f46e
    1 -1 -15 8 2 16391:c309f46e
    1 -1 -15 8 3 16391:c309f46e
    1 -1 -15 8 4 16389:355b7629
    1 -1 -15 9 0 16389:355b7629
    1 -1 -15 9 1 16389:355b7629
    1 -1 -15 9 2 16389:355b7629
    1 -1 -15 9 3 16389:355b7629
    1 -1 -15 9 4 16389:355b7629
    1 -1 -9 1 0 17031:f2fae3e3
    1 -1 -9 1 1 17031:f2fae3e3
    1 -1 -9 1 2 17031:f2fae3e3
    1 -1 -9 1 3 17031:f2fae3e3
    1 -1 -9 1 4 17031:f2fae3e3
    1 -1 -9 8 0 16420:77edc06e
    1 -1 -9 8 1 16420:77edc06e
    1 -1 -9 8 2 16420:77edc06e
    1 -1 -9 8 3 16420:77edc06e
    1 -1 -9 8 4 17287:6f3b7917
    1 -1 -9 9 0 16419:426921fa
    1 -1 -9 9 1 16419:426921fa
    1 -1 -9 9 2 16419:426921fa
    1 -1 -9 9 3 16419:426921fa
    1 -1 -9 9 4 17286:539b5413
    1 -1 9 1 0 17037:8201c56f
    1 -1 9 1 1 17037:8201c56f
    1 -1 9 1 2 17037:e52faf0a
    1 -1 9 1 3 17037:e52faf0a
    1 -1 9 1 4 17037:e52faf0a
    1 -1 9 8 0 16426:47ad15a9
    1 -1 9 8 1 16426:47ad15a9
    1 -1 9 8 2 16426:16408f08
    1 -1 9 8 3 16426:16408f08
    1 -1 9 8 4 17293:573576ab
    1 -1 9 9 0 16425:26abb81b
    1 -1 9 9 1 16425:26abb81b
    1 -1 9 9 2 16425:1531232a
    1 -1 9 9 3 16425:1531232a
    1 -1 9 9 4 17292:d052d692
    1 -1 15 1 0 17035:7ba5087f
    1 -1 15 1 1 17037:2a1ceff3
    1 -1 15 1 2 17037:5ebc7e03
    1 -1 15 1 3 17037:5ebc7e03
    1 -1 15 1 4 17035:39c4d7a4
    1 -1 15 8 0 16395:a72c786d
    1 -1 15 8 1 16397:4e93b510
    1 -1 15 8 2 16397:822fd0da
    1 -1 15 8 3 16397:822fd0da
    1 -1 15 8 4 16395:65ebee17
    1 -1 15 9 0 16395:a72c786d
    1 -1 15 9 1 16395:a72c786d
    1 -1 15 9 2 16395:65ebee17
    1 -1 15 9 3 16395:65ebee17
    1 -1 15 9 4 16395:65ebee17
    1 0 -15 1 0 16389:355b7629
    1 0 -15 1 1 16389:355b7629
    1 0 -15 1 2 16389:355b7629
    1 0 -15 1 3 16389:355b7629
    1 0 -15 1 4 16389:355b7629
    1 0 -15 8 0 16389:355b7629
    1 0 -15 8 1 16389:355b7629
    1 0 -15 8 2 16389:355b7629
    1 0 -15 8 3 16389:355b7629
    1 0 -15 8 4 16389:355b7629
    1 0 -15 9 0 16389:355b7629
    1 0 -15 9 1 16389:355b7629
    1 0 -15 9 2 16389:355b7629
    1 0 -15 9 3 16389:355b7629
    1 0 -15 9 4 16389:355b7629
    1 0 -9 1 0 16389:355b7629
    1 0 -9 1 1 16389:355b7629
    1 0 -9 1 2 16389:355b7629
    1 0 -9 1 3 16389:355b7629
    1 0 -9 1 4 16389:355b7629
    1 0 -9 8 0 16389:355b7629
    1 0 -9 8 1 16389:355b7629
    1 0 -9 8 2 16389:355b7629
    1 0 -9 8 3 16389:355b7629
    1 0 -9 8 4 16389:355b7629
    1 0 -9 9 0 16389:355b7629
    1 0 -9 9 1 16389:355b7629
    1 0 -9 9 2 16389:355b7629
    1 0 -9 9 3 16389:355b7629
    1 0 -9 9 4 16389:355b7629
    1 0 9 1 0 16395:16d55e18
    1 0 9 1 1 16395:16d55e18
    1 0 9 1 2 16395:16d55e18
    1 0 9 1 3 16395:16d55e18
    1 0 9 1 4 16395:16d55e18
    1 0 9 8 0 16395:16d55e18
    1 0 9 8 1 16395:16d55e18
    1 0 9 8 2 16395:16d55e18
    1 0 9 8 3 16395:16d55e18
    1 0 9 8 4 16395:16d55e18
    1 0 9 9 0 16395:16d55e18
    1 0 9 9 1 16395:16d55e18
    1 0 9 9 2 16395:16d55e18
    1 0 9 9 3 16395:16d55e18
    1 0 9 9 4 16395:16d55e18
    1 0 15 1 0 16395:65ebee17
    1 0 15 1 1 16395:65ebee17
    1 0 15 1 2 16395:65ebee17
    1 0 15 1 3 16395:65ebee17
    1 0 15 1 4 16395:65ebee17
    1 0 15 8 0 16395:65ebee17
    1 0 15 8 1 16395:65ebee17
    1 0 15 8 2 16395:65ebee17
    1 0 15 8 3 16395:65ebee17
    1 0 15 8 4 16395:65ebee17
    1 0 15 9 0 16395:65ebee17
    1 0 15 9 1 16395:65ebee17
    1 0 15 9 2 16395:65ebee17
    1 0 15 9 3 16395:65ebee17
    1 0 15 9 4 16395:65ebee17
    1 1 -15 1 0 17031:f2fae3e3
    1 1 -15 1 1 17031:f2fae3e3
    1 1 -15 1 2 17031:f2fae3e3
    1 1 -15 1 3 17031:f2fae3e3
    1 1 -15 1 4 17031:f2fae3e3
    1 1 -15 8 0 16389:355b7629
    1 1 -15 8 1 16389:355b7629
    1 1 -15 8 2 16391:c309f46e
    1 1 -15 8 3 16391:c309f46e
    1 1 -15 8 4 16389:355b7629
    1 1 -15 9 0 16389:355b7629
    1 1 -15 9 1 16389:355b7629
    1 1 -15 9 2 16389:355b7629
    1 1 -15 9 3 16389:355b7629
    1 1 -15 9 4 16389:355b7629
    1 1 -9 1 0 17031:f2fae3e3
    1 1 -9 1 1 17031:f2fae3e3
    1 1 -9 1 2 17031:f2fae3e3
    1 1 -9 1 3 17031:f2fae3e3
    1 1 -9 1 4 17031:f2fae3e3
    1 1 -9 8 0 16420:77edc06e
    1 1 -9 8 1 16420:77edc06e
    1 1 -9 8 2 16420:77edc06e
    1 1 -9 8 3 16420:77edc06e
    1 1 -9 8 4 17287:6f3b7917
    1 1 -9 9 0 16419:426921fa
    1 1 -9 9 1 16419:426921fa
    1 1 -9 9 2 16419:426921fa
    1 1 -9 9 3 16419:426921fa
    1 1 -9 9 4 17286:539b5413
    1 1 9 1 0 17037:e52faf0a
    1 1 9 1 1 17037:e52faf0a
    1 1 9 1 2 17037:e52faf0a
    1 1 9 1 3 17037:e52faf0a
    1 1 9 1 4 17037:e52faf0a
    1 1 9 8 0 16426:16408f08
    1 1 9 8 1 16426:16408f08
    1 1 9 8 2 16426:16408f08
    1 1 9 8 3 16426:16408f08
    1 1 9 8 4 17293:573576ab
    1 1 9 9 0 16425:1531232a
    1 1 9 9 1 16425:1531232a
    1 1 9 9 2 16425:1531232a
    1 1 9 9 3 16425:1531232a
    1 1 9 9 4 17292:d052d692
    1 1 15 1 0 17037:5ebc7e03
    1 1 15 1 1 17037:5ebc7e03
    1 1 15 1 2 17037:5ebc7e03
    1 1 15 1 3 17037:5ebc7e03
    1 1 15 1 4 17037:5ebc7e03
    1 1 15 8 0 16395:65ebee17
    1 1 15 8 1 16395:65ebee17
    1 1 15 8 2 16397:822fd0da
    1 1 15 8 3 16397:822fd0da
    1 1 15 8 4 16395:65ebee17
    1 1 15 9 0 16395:65ebee17
    1 1 15 9 1 16395:65ebee17
    1 1 15 9 2 16395:65ebee17
    1 1 15 9 3 16395:65ebee17
    1 1 15 9 4 16395:65ebee17
    1 2 -15 1 0 17029:4536e93b
    1 2 -15 1 1 17029:4536e93b
    1 2 -15 1 2 17031:f2fae3e3
    1 2 -15 1 3 17031:f2fae3e3
    1 2 -15 1 4 17029:4536e93b
    1 2 -15 8 0 16389:355b7629
    1 2 -15 8 1 16389:355b7629
    1 2 -15 8 2 16391:c309f46e
    1 2 -15 8 3 16391:c309f46e
    1 2 -15 8 4 16389:355b7629
    1 2 -15 9 0 16389:355b7629
    1 2 -15 9 1 16389:355b7629
    1 2 -15 9 2 16389:355b7629
    1 2 -15 9 3 16389:355b7629
    1 2 -15 9 4 16389:355b7629
    1 2 -9 1 0 17031:f2fae3e3
    1 2 -9 1 1 17031:f2fae3e3
    1 2 -9 1 2 17031:f2fae3e3
    1 2 -9 1 3 17031:f2fae3e3
    1 2 -9 1 4 17031:f2fae3e3
    1 2 -9 8 0 16420:77edc06e
    1 2 -9 8 1 16420:77edc06e
    1 2 -9 8 2 16420:77edc06e
    1 2 -9 8 3 16420:77edc06e
    1 2 -9 8 4 17287:6f3b7917
    1 2 -9 9 0 16419:426921fa
    1 2 -9 9 1 16419:426921fa
    1 2 -9 9 2 16419:426921fa
    1 2 -9 9 3 16419:426921fa
    1 2 -9 9 4 17286:539b5413
    1 2 9 1 0 17037:16c272c3
    1 2 9 1 1 17037:16c272c3
    1 2 9 1 2 17037:e52faf0a
    1 2 9 1 3 17037:e52faf0a
    1 2 9 1 4 17037:e52faf0a
    1 2 9 8 0 16426:4e9ab691
    1 2 9 8 1 16426:4e9ab691
    1 2 9 8 2 16426:16408f08
    1 2 9 8 3 16426:16408f08
    1 2 9 8 4 17293:573576ab
    1 2 9 9 0 16425:a744ab17
    1 2 9 9 1 16425:a744ab17
    1 2 9 9 2 16425:1531232a
    1 2 9 9 3 16425:1531232a
    1 2 9 9 4 17292:d052d692
    1 2 15 1 0 17035:3139d438
    1 2 15 1 1 17035:3139d438
    1 2 15 1 2 17037:5ebc7e03
    1 2 15 1 3 17037:5ebc7e03
    1 2 15 1 4 17035:9856e307
    1 2 15 8 0 16395:b4a1b348
    1 2 15 8 1 16395:b4a1b348
    1 2 15 8 2 16397:822fd0da
    1 2 15 8 3 16397:822fd0da
    1 2 15 8 4 16395:65ebee17
    1 2 15 9 0 16395:b4a1b348
    1 2 15 9 1 16395:b4a1b348
    1 2 15 9 2 16395:65ebee17
    1 2 15 9 3 16395:65ebee17
    1 2 15 9 4 16395:65ebee17
    1 3 -15 1 0 17029:5feec7af
    1 3 -15 1 1 17029:5feec7af
    1 3 -15 1 2 17031:f2fae3e3
    1 3 -15 1 3 17031:f2fae3e3
    1 3 -15 1 4 17029:5feec7af
    1 3 -15 8 0 16389:355b7629
    1 3 -15 8 1 16389:355b7629
    1 3 -15 8 2 16391:c309f46e
    1 3 -15 8 3 16391:c309f46e
    1 3 -15 8 4 16389:355b7629
    1 3 -15 9 0 16389:355b7629
    1 3 -15 9 1 16389:355b7629
    1 3 -15 9 2 16389:355b7629
    1 3 -15 9 3 16389:355b7629
    1 3 -15 9 4 16389:355b7629
    1 3 -9 1 0 17031:f2fae3e3
    1 3 -9 1 1 17031:f2fae3e3
    1 3 -9 1 2 17031:f2fae3e3
    1 3 -9 1 3 17031:f2fae3e3
    1 3 -9 1 4 17031:f2fae3e3
    1 3 -9 8 0 16420:77edc06e
    1 3 -9 8 1 16420:77edc06e
    1 3 -9 8 2 16420:77edc06e
    1 3 -9 8 3 16420:77edc06e
    1 3 -9 8 4 17287:6f3b7917
    1 3 -9 9 0 16419:426921fa
    1 3 -9 9 1 16419:426921fa
    1 3 -9 9 2 16419:426921fa
    1 3 -9 9 3 16419:426921fa
    1 3 -9 9 4 17286:539b5413
    1 3 9 1 0 17037:16c272c3
    1 3 9 1 1 17037:16c272c3
    1 3 9 1 2 17037:e52faf0a
    1 3 9 1 3 17037:e52faf0a
    1 3 9 1 4 17037:e52faf0a
    1 3 9 8 0 16426:4e9ab691
    1 3 9 8 1 16426:4e9ab691
    1 3 9 8 2 16426:16408f08
    1 3 9 8 3 16426:16408f08
    1 3 9 8 4 17293:573576ab
    1 3 9 9 0 16425:a744ab17
    1 3 9 9 1 16425:a744ab17
    1 3 9 9 2 16425:1531232a
    1 3 9 9 3 16425:1531232a
    1 3 9 9 4 17292:d052d692
    1 3 15 1 0 17035:854b7f4c
    1 3 15 1 1 17035:854b7f4c
    1 3 15 1 2 17037:5ebc7e03
    1 3 15 1 3 17037:5ebc7e03
    1 3 15 1 4 17035:2c244873
    1 3 15 8 0 16395:b4a1b348
    1 3 15 8 1 16395:b4a1b348
    1 3 15 8 2 16397:822fd0da
    1 3 15 8 3 16397:822fd0da
    1 3 15 8 4 16395:65ebee17
    1 3 15 9 0 16395:b4a1b348
    1 3 15 9 1 16395:b4a1b348
    1 3 15 9 2 16395:65ebee17
    1 3 15 9 3 16395:65ebee17
    1 3 15 9 4 16395:65ebee17
    1 4 -15 1 0 17029:9429a88b
    1 4 -15 1 1 17031:f2fae3e3
    1 4 -15 1 2 17031:f2fae3e3
    1 4 -15 1 3 17031:f2fae3e3
    1 4 -15 1 4 17029:9429a88b
    1 4 -15 8 0 16389:355b7629
    1 4 -15 8 1 16391:c309f46e
    1 4 -15 8 2 16391:c309f46e
    1 4 -15 8 3 16391:c309f46e
    1 4 -15 8 4 16389:355b7629
    1 4 -15 9 0 16389:355b7629
    1 4 -15 9 1 16389:355b7629
    1 4 -15 9 2 16389:355b7629
    1 4 -15 9 3 16389:355b7629
    1 4 -15 9 4 16389:355b7629
    1 4 -9 1 0 17031:f2fae3e3
    1 4 -9 1 1 17031:f2fae3e3
    1 4 -9 1 2 17031:f2fae3e3
    1 4 -9 1 3 17031:f2fae3e3
    1 4 -9 1 4 17031:f2fae3e3
    1 4 -9 8 0 16420:77edc06e
    1 4 -9 8 1 16420:77edc06e
    1 4 -9 8 2 16420:77edc06e
    1 4 -9 8 3 16420:77edc06e
    1 4 -9 8 4 17287:6f3b7917
    1 4 -9 9 0 16419:426921fa
    1 4 -9 9 1 16419:426921fa
    1 4 -9 9 2 16419:426921fa
    1 4 -9 9 3 16419:426921fa
    1 4 -9 9 4 17286:539b5413
    1 4 9 1 0 17037:16c272c3
    1 4 9 1 1 17037:16c272c3
    1 4 9 1 2 17037:e52faf0a
    1 4 9 1 3 17037:e52faf0a
    1 4 9 1 4 17037:e52faf0a
    1 4 9 8 0 16426:4e9ab691
    1 4 9 8 1 16426:4e9ab691
    1 4 9 8 2 16426:16408f08
    1 4 9 8 3 16426:16408f08
    1 4 9 8 4 17293:573576ab
    1 4 9 9 0 16425:a744ab17
    1 4 9 9 1 16425:a744ab17
    1 4 9 9 2 16425:1531232a
    1 4 9 9 3 16425:1531232a
    1 4 9 9 4 17292:d052d692
    1 4 15 1 0 17035:619d9189
    1 4 15 1 1 17037:bedf585f
    1 4 15 1 2 17037:5ebc7e03
    1 4 15 1 3 17037:5ebc7e03
    1 4 15 1 4 17035:c8f2a6b6
    1 4 15 8 0 16395:b4a1b348
    1 4 15 8 1 16397:aad66d42
    1 4 15 8 2 16397:822fd0da
    1 4 15 8 3 16397:822fd0da
    1 4 15 8 4 16395:65ebee17
    1 4 15 9 0 16395:b4a1b348
    1 4 15 9 1 16395:b4a1b348
    1 4 15 9 2 16395:65ebee17
    1 4 15 9 3 16395:65ebee17
    1 4 15 9 4 16395:65ebee17
    1 5 -15 1 0 17029:6fe7b18c
    1 5 -15 1 1 17031:f2fae3e3
    1 5 -15 1 2 17031:f2fae3e3
    1 5 -15 1 3 17031:f2fae3e3
    1 5 -15 1 4 17029:6fe7b18c
    1 5 -15 8 0 16389:355b7629
    1 5 -15 8 1 16391:c309f46e
    1 5 -15 8 2 16391:c309f46e
    1 5 -15 8 3 16391:c309f46e
    1 5 -15 8 4 16389:355b7629
    1 5 -15 9 0 16389:355b7629
    1 5 -15 9 1 16389:355b7629
    1 5 -15 9 2 16389:355b7629
    1 5 -15 9 3 16389:355b7629
    1 5 -15 9 4 16389:355b7629
    1 5 -9 1 0 17031:f2fae3e3
    1 5 -9 1 1 17031:f2fae3e3
    1 5 -9 1 2 17031:f2fae3e3
    1 5 -9 1 3 17031:f2fae3e3
    1 5 -9 1 4 17031:f2fae3e3
    1 5 -9 8 0 16420:77edc06e
    1 5 -9 8 1 16420:77edc06e
    1 5 -9 8 2 16420:77edc06e
    1 5 -9 8 3 16420:77edc06e
    1 5 -9 8 4 17287:6f3b7917
    1 5 -9 9 0 16419:426921fa
    1 5 -9 9 1 16419:426921fa
    1 5 -9 9 2 16419:426921fa
    1 5 -9 9 3 16419:426921fa
    1 5 -9 9 4 17286:539b5413
    1 5 9 1 0 17037:16c272c3
    1 5 9 1 1 17037:16c272c3
    1 5 9 1 2 17037:e52faf0a
    1 5 9 1 3 17037:e52faf0a
    1 5 9 1 4 17037:e52faf0a
    1 5 9 8 0 16426:4e9ab691
    1 5 9 8 1 16426:4e9ab691
    1 5 9 8 2 16426:16408f08
    1 5 9 8 3 16426:16408f08
    1 5 9 8 4 17293:573576ab
    1 5 9 9 0 16425:a744ab17
    1 5 9 9 1 16425:a744ab17
    1 5 9 9 2 16425:1531232a
    1 5 9 9 3 16425:1531232a
    1 5 9 9 4 17292:d052d692
    1 5 15 1 0 17035:90abe09b
    1 5 15 1 1 17037:bedf585f
    1 5 15 1 2 17037:5ebc7e03
    1 5 15 1 3 17037:5ebc7e03
    1 5 15 1 4 17035:39c4d7a4
    1 5 15 8 0 16395:b4a1b348
    1 5 15 8 1 16397:aad66d42
    1 5 15 8 2 16397:822fd0da
    1 5 15 8 3 16397:822fd0da
    1 5 15 8 4 16395:65ebee17
    1 5 15 9 0 16395:b4a1b348
    1 5 15 9 1 16395:b4a1b348
    1 5 15 9 2 16395:65ebee17
    1 5 15 9 3 16395:65ebee17
    1 5 15 9 4 16395:65ebee17
    1 6 -15 1 0 17029:6fe7b18c
    1 6 -15 1 1 17031:f2fae3e3
    1 6 -15 1 2 17031:f2fae3e3
    1 6 -15 1 3 17031:f2fae3e3
    1 6 -15 1 4 17029:6fe7b18c
    1 6 -15 8 0 16389:355b7629
    1 6 -15 8 1 16391:c309f46e
    1 6 -15 8 2 16391:c309f46e
    1 6 -15 8 3 16391:c309f46e
    1 6 -15 8 4 16389:355b7629
    1 6 -15 9 0 16389:355b7629
    1 6 -15 9 1 16389:355b7629
    1 6 -15 9 2 16389:355b7629
    1 6 -15 9 3 16389:355b7629
    1 6 -15 9 4 16389:355b7629
    1 6 -9 1 0 17031:f2fae3e3
    1 6 -9 1 1 17031:f2fae3e3
    1 6 -9 1 2 17031:f2fae3e3
    1 6 -9 1 3 17031:f2fae3e3
    1 6 -9 1 4 17031:f2fae3e3
    1 6 -9 8 0 16420:77edc06e
    1 6 -9 8 1 16420:77edc06e
    1 6 -9 8 2 16420:77edc06e
    1 6 -9 8 3 16420:77edc06e
    1 6 -9 8 4 17287:6f3b7917
    1 6 -9 9 0 16419:426921fa
    1 6 -9 9 1 16419:426921fa
    1 6 -9 9 2 16419:426921fa
    1 6 -9 9 3 16419:426921fa
    1 6 -9 9 4 17286:539b5413
    1 6 9 1 0 17037:8201c56f
    1 6 9 1 1 17037:8201c56f
    1 6 9 1 2 17037:e52faf0a
    1 6 9 1 3 17037:e52faf0a
    1 6 9 1 4 17037:e52faf0a
    1 6 9 8 0 16426:47ad15a9
    1 6 9 8 1 16426:47ad15a9
    1 6 9 8 2 16426:16408f08
    1 6 9 8 3 16426:16408f08
    1 6 9 8 4 17293:573576ab
    1 6 9 9 0 16425:26abb81b
    1 6 9 9 1 16425:26abb81b
    1 6 9 9 2 16425:1531232a
    1 6 9 9 3 16425:1531232a
    1 6 9 9 4 17292:d052d692
    1 6 15 1 0 17035:7ba5087f
    1 6 15 1 1 17037:2a1ceff3
    1 6 15 1 2 17037:5ebc7e03
    1 6 15 1 3 17037:5ebc7e03
    1 6 15 1 4 17035:39c4d7a4
    1 6 15 8 0 16395:a72c786d
    1 6 15 8 1 16397:4e93b510
    1 6 15 8 2 16397:822fd0da
    1 6 15 8 3 16397:822fd0da
    1 6 15 8 4 16395:65ebee17
    1 6 15 9 0 16395:a72c786d
    1 6 15 9 1 16395:a72c786d
    1 6 15 9 2 16395:65ebee17
    1 6 15 9 3 16395:65ebee17
    1 6 15 9 4 16395:65ebee17
    1 7 -15 1 0 17029:6fe7b18c
    1 7 -15 1 1 17031:f2fae3e3
    1 7 -15 1 2 17031:f2fae3e3
    1 7 -15 1 3 17031:f2fae3e3
    1 7 -15 1 4 17029:6fe7b18c
    1 7 -15 8 0 16389:355b7629
    1 7 -15 8 1 16391:c309f46e
    1 7 -15 8 2 16391:c309f46e
    1 7 -15 8 3 16391:c309f46e
    1 7 -15 8 4 16389:355b7629
    1 7 -15 9 0 16389:355b7629
    1 7 -15 9 1 16389:355b7629
    1 7 -15 9 2 16389:355b7629
    1 7 -15 9 3 16389:355b7629
    1 7 -15 9 4 16389:355b7629
    1 7 -9 1 0 17031:f2fae3e3
    1 7 -9 1 1 17031:f2fae3e3
    1 7 -9 1 2 17031:f2fae3e3
    1 7 -9 1 3 17031:f2fae3e3
    1 7 -9 1 4 17031:f2fae3e3
    1 7 -9 8 0 16420:77edc06e
    1 7 -9 8 1 16420:77edc06e
    1 7 -9 8 2 16420:77edc06e
    1 7 -9 8 3 16420:77edc06e
    1 7 -9 8 4 17287:6f3b7917
    1 7 -9 9 0 16419:426921fa
    1 7 -9 9 1 16419:426921fa
    1 7 -9 9 2 16419:426921fa
    1 7 -9 9 3 16419:426921fa
    1 7 -9 9 4 17286:539b5413
    1 7 9 1 0 17037:5c2e737d
    1 7 9 1 1 17037:5c2e737d
    1 7 9 1 2 17037:e52faf0a
    1 7 9 1 3 17037:e52faf0a
    1 7 9 1 4 17037:e52faf0a
    1 7 9 8 0 16426:82e35bd9
    1 7 9 8 1 16426:82e35bd9
    1 7 9 8 2 16426:16408f08
    1 7 9 8 3 16426:16408f08
    1 7 9 8 4 17293:573576ab
    1 7 9 9 0 16425:d2de76a3
    1 7 9 9 1 16425:d2de76a3
    1 7 9 9 2 16425:1531232a
    1 7 9 9 3 16425:1531232a
    1 7 9 9 4 17292:d052d692
    1 7 15 1 0 17035:374961f6
    1 7 15 1 1 17037:f43359e1
    1 7 15 1 2 17037:5ebc7e03
    1 7 15 1 3 17037:5ebc7e03
    1 7 15 1 4 17035:39c4d7a4
    1 7 15 8 0 16395:0a192b26
    1 7 15 8 1 16397:af7c8ea5
    1 7 15 8 2 16397:822fd0da
    1 7 15 8 3 16397:822fd0da
    1 7 15 8 4 16395:65ebee17
    1 7 15 9 0 16395:0a192b26
    1 7 15 9 1 16395:0a192b26
    1 7 15 9 2 16395:65ebee17
    1 7 15 9 3 16395:65ebee17
    1 7 15 9 4 16395:65ebee17
    1 8 -15 1 0 17029:6fe7b18c
    1 8 -15 1 1 17031:f2fae3e3
    1 8 -15 1 2 17031:f2fae3e3
    1 8 -15 1 3 17031:f2fae3e3
    1 8 -15 1 4 17029:6fe7b18c
    1 8 -15 8 0 16389:355b7629
    1 8 -15 8 1 16391:c309f46e
    1 8 -15 8 2 16391:c309f46e
    1 8 -15 8 3 16391:c309f46e
    1 8 -15 8 4 16389:355b7629
    1 8 -15 9 0 16389:355b7629
    1 8 -15 9 1 16389:355b7629
    1 8 -15 9 2 16389:355b7629
    1 8 -15 9 3 16389:355b7629
    1 8 -15 9 4 16389:355b7629
    1 8 -9 1 0 17031:f2fae3e3
    1 8 -9 1 1 17031:f2fae3e3
    1 8 -9 1 2 17031:f2fae3e3
    1 8 -9 1 3 17031:f2fae3e3
    1 8 -9 1 4 17031:f2fae3e3
    1 8 -9 8 0 16420:77edc06e
    1 8 -9 8 1 16420:77edc06e
    1 8 -9 8 2 16420:77edc06e
    1 8 -9 8 3 16420:77edc06e
    1 8 -9 8 4 17287:6f3b7917
    1 8 -9 9 0 16419:426921fa
    1 8 -9 9 1 16419:426921fa
    1 8 -9 9 2 16419:426921fa
    1 8 -9 9 3 16419:426921fa
    1 8 -9 9 4 17286:539b5413
    1 8 9 1 0 17037:5c2e737d
    1 8 9 1 1 17037:5c2e737d
    1 8 9 1 2 17037:e52faf0a
    1 8 9 1 3 17037:e52faf0a
    1 8 9 1 4 17037:e52faf0a
    1 8 9 8 0 16426:82e35bd9
    1 8 9 8 1 16426:82e35bd9
    1 8 9 8 2 16426:16408f08
    1 8 9 8 3 16426:16408f08
    1 8 9 8 4 17293:573576ab
    1 8 9 9 0 16425:d2de76a3
    1 8 9 9 1 16425:d2de76a3
    1 8 9 9 2 16425:1531232a
    1 8 9 9 3 16425:1531232a
    1 8 9 9 4 17292:d052d692
    1 8 15 1 0 17035:374961f6
    1 8 15 1 1 17037:f43359e1
    1 8 15 1 2 17037:5ebc7e03
    1 8 15 1 3 17037:5ebc7e03
    1 8 15 1 4 17035:39c4d7a4
    1 8 15 8 0 16395:0a192b26
    1 8 15 8 1 16397:af7c8ea5
    1 8 15 8 2 16397:822fd0da
    1 8 15 8 3 16397:822fd0da
    1 8 15 8 4 16395:65ebee17
    1 8 15 9 0 16395:0a192b26
    1 8 15 9 1 16395:0a192b26
    1 8 15 9 2 16395:65ebee17
    1 8 15 9 3 16395:65ebee17
    1 8 15 9 4 16395:65ebee17
    1 9 -15 1 0 17029:6fe7b18c
    1 9 -15 1 1 17031:f2fae3e3
    1 9 -15 1 2 17031:f2fae3e3
    1 9 -15 1 3 17031:f2fae3e3
    1 9 -15 1 4 17029:6fe7b18c
    1 9 -15 8 0 16389:355b7629
    1 9 -15 8 1 16391:c309f46e
    1 9 -15 8 2 16391:c309f46e
    1 9 -15 8 3 16391:c309f46e
    1 9 -15 8 4 16389:355b7629
    1 9 -15 9 0 16389:355b7629
    1 9 -15 9 1 16389:355b7629
    1 9 -15 9 2 16389:355b7629
    1 9 -15 9 3 16389:355b7629
    1 9 -15 9 4 16389:355b7629
    1 9 -9 1 0 17031:f2fae3e3
    1 9 -9 1 1 17031:f2fae3e3
    1 9 -9 1 2 17031:f2fae3e3
    1 9 -9 1 3 17031:f2fae3e3
    1 9 -9 1 4 17031:f2fae3e3
    1 9 -9 8 0 16420:77edc06e
    1 9 -9 8 1 16420:77edc06e
    1 9 -9 8 2 16420:77edc06e
    1 9 -9 8 3 16420:77edc06e
    1 9 -9 8 4 17287:6f3b7917
    1 9 -9 9 0 16419:426921fa
    1 9 -9 9 1 16419:426921fa
    1 9 -9 9 2 16419:426921fa
    1 9 -9 9 3 16419:426921fa
    1 9 -9 9 4 17286:539b5413
    1 9 9 1 0 17037:5c2e737d
    1 9 9 1 1 17037:5c2e737d
    1 9 9 1 2 17037:e52faf0a
    1 9 9 1 3 17037:e52faf0a
    1 9 9 1 4 17037:e52faf0a
    1 9 9 8 0 16426:82e35bd9
    1 9 9 8 1 16426:82e35bd9
    1 9 9 8 2 16426:16408f08
    1 9 9 8 3 16426:16408f08
    1 9 9 8 4 17293:573576ab
    1 9 9 9 0 16425:d2de76a3
    1 9 9 9 1 16425:d2de76a3
    1 9 9 9 2 16425:1531232a
    1 9 9 9 3 16425:1531232a
    1 9 9 9 4 17292:d052d692
    1 9 15 1 0 17035:374961f6
    1 9 15 1 1 17037:f43359e1
    1 9 15 1 2 17037:5ebc7e03
    1 9 15 1 3 17037:5ebc7e03
    1 9 15 1 4 17035:39c4d7a4
    1 9 15 8 0 16395:0a192b26
    1 9 15 8 1 16397:af7c8ea5
    1 9 15 8 2 16397:822fd0da
    1 9 15 8 3 16397:822fd0da
    1 9 15 8 4 16395:65ebee17
    1 9 15 9 0 16395:0a192b26
    1 9 15 9 1 16395:0a192b26
    1 9 15 9 2 16395:65ebee17
    1 9 15 9 3 16395:65ebee17
    1 9 15 9 4 16395:65ebee17
    2 -1 -15 1 0 490:7ef70261
    2 -1 -15 1 1 532:0796be41
    2 -1 -15 1 2 13160:b3ce8931
    2 -1 -15 1 3 13160:b3ce8931
    2 -1 -15 1 4 593:a99f06a0
    2 -1 -15 8 0 447:e0cd26be
    2 -1 -15 8 1 455:84c96125
    2 -1 -15 8 2 9699:8c0e1549
    2 -1 -15 8 3 9699:8c0e1549
    2 -1 -15 8 4 589:4eeea140
    2 -1 -15 9 0 447:e0cd26be
    2 -1 -15 9 1 455:84c96125
    2 -1 -15 9 2 9698:d3a4dfdf
    2 -1 -15 9 3 9698:d3a4dfdf
    2 -1 -15 9 4 589:4eeea140
    2 -1 -9 1 0 10636:0ac61f68
    2 -1 -9 1 1 11746:7fb58c21
    2 -1 -9 1 2 13160:b3ce8931
    2 -1 -9 1 3 13160:b3ce8931
    2 -1 -9 1 4 11987:dbe7f833
    2 -1 -9 8 0 8041:5c8f281b
    2 -1 -9 8 1 8514:46df861d
    2 -1 -9 8 2 9699:8c0e1549
    2 -1 -9 8 3 9699:8c0e1549
    2 -1 -9 8 4 11889:ca972a2f
    2 -1 -9 9 0 8041:5c8f281b
    2 -1 -9 9 1 8514:46df861d
    2 -1 -9 9 2 9698:d3a4dfdf
    2 -1 -9 9 3 9698:d3a4dfdf
    2 -1 -9 9 4 11889:ca972a2f
    2 -1 9 1 0 10642:da56df4b
    2 -1 9 1 1 11752:1b46bf23
    2 -1 9 1 2 13166:db2440ec
    2 -1 9 1 3 13166:db2440ec
    2 -1 9 1 4 11993:152d7584
    2 -1 9 8 0 8047:92602365
    2 -1 9 8 1 8520:a720c90a
    2 -1 9 8 2 9705:67cc6276
    2 -1 9 8 3 9705:67cc6276
    2 -1 9 8 4 11895:c887398f
    2 -1 9 9 0 8047:92602365
    2 -1 9 9 1 8520:a720c90a
    2 -1 9 9 2 9704:913f0e8d
    2 -1 9 9 3 9704:913f0e8d
    2 -1 9 9 4 11895:c887398f
    2 -1 15 1 0 496:93416d78
    2 -1 15 1 1 538:98f980aa
    2 -1 15 1 2 13166:eb5a9a85
    2 -1 15 1 3 13166:eb5a9a85
    2 -1 15 1 4 599:4dc8e9d7
    2 -1 15 8 0 453:af4afcc9
    2 -1 15 8 1 461:64ba1f9b
    2 -1 15 8 2 9705:b974f695
    2 -1 15 8 3 9705:b974f695
    2 -1 15 8 4 595:e9c1e408
    2 -1 15 9 0 453:af4afcc9
    2 -1 15 9 1 461:64ba1f9b
    2 -1 15 9 2 9704:92352860
    2 -1 15 9 3 9704:92352860
    2 -1 15 9 4 595:e9c1e408
    2 0 -15 1 0 16389:48365c2f
    2 0 -15 1 1 16389:48365c2f
    2 0 -15 1 2 16389:48365c2f
    2 0 -15 1 3 16389:48365c2f
    2 0 -15 1 4 16389:48365c2f
    2 0 -15 8 0 16389:48365c2f
    2 0 -15 8 1 16389:48365c2f
    2 0 -15 8 2 16389:48365c2f
    2 0 -15 8 3 16389:48365c2f
    2 0 -15 8 4 16389:48365c2f
    2 0 -15 9 0 16389:48365c2f
    2 0 -15 9 1 16389:48365c2f
    2 0 -15 9 2 16389:48365c2f
    2 0 -15 9 3 16389:48365c2f
    2 0 -15 9 4 16389:48365c2f
    2 0 -9 1 0 16389:48365c2f
    2 0 -9 1 1 16389:48365c2f
    2 0 -9 1 2 16389:48365c2f
    2 0 -9 1 3 16389:48365c2f
    2 0 -9 1 4 16389:48365c2f
    2 0 -9 8 0 16389:48365c2f
    2 0 -9 8 1 16389:48365c2f
    2 0 -9 8 2 16389:48365c2f
    2 0 -9 8 3 16389:48365c2f
    2 0 -9 8 4 16389:48365c2f
    2 0 -9 9 0 16389:48365c2f
    2 0 -9 9 1 16389:48365c2f
    2 0 -9 9 2 16389:48365c2f
    2 0 -9 9 3 16389:48365c2f
    2 0 -9 9 4 16389:48365c2f
    2 0 9 1 0 16395:ff79267f
    2 0 9 1 1 16395:ff79267f
    2 0 9 1 2 16395:ff79267f
    2 0 9 1 3 16395:ff79267f
    2 0 9 1 4 16395:ff79267f
    2 0 9 8 0 16395:ff79267f
    2 0 9 8 1 16395:ff79267f
    2 0 9 8 2 16395:ff79267f
    2 0 9 8 3 16395:ff79267f
    2 0 9 8 4 16395:ff79267f
    2 0 9 9 0 16395:ff79267f
    2 0 9 9 1 16395:ff79267f
    2 0 9 9 2 16395:ff79267f
    2 0 9 9 3 16395:ff79267f
    2 0 9 9 4 16395:ff79267f
    2 0 15 1 0 16395:8c479670
    2 0 15 1 1 16395:8c479670
    2 0 15 1 2 16395:8c479670
    2 0 15 1 3 16395:8c479670
    2 0 15 1 4 16395:8c479670
    2 0 15 8 0 16395:8c479670
    2 0 15 8 1 16395:8c479670
    2 0 15 8 2 16395:8c479670
    2 0 15 8 3 16395:8c479670
    2 0 15 8 4 16395:8c479670
    2 0 15 9 0 16395:8c479670
    2 0 15 9 1 16395:8c479670
    2 0 15 9 2 16395:8c479670
    2 0 15 9 3 16395:8c479670
    2 0 15 9 4 16395:8c479670
    2 1 -15 1 0 594:8a7b343d
    2 1 -15 1 1 594:8a7b343d
    2 1 -15 1 2 13160:b3ce8931
    2 1 -15 1 3 13160:b3ce8931
    2 1 -15 1 4 695:51f8d0ed
    2 1 -15 8 0 539:027b666c
    2 1 -15 8 1 539:027b666c
    2 1 -15 8 2 9699:8c0e1549
    2 1 -15 8 3 9699:8c0e1549
    2 1 -15 8 4 665:9611c6b5
    2 1 -15 9 0 539:027b666c
    2 1 -15 9 1 539:027b666c
    2 1 -15 9 2 9698:d3a4dfdf
    2 1 -15 9 3 9698:d3a4dfdf
    2 1 -15 9 4 665:9611c6b5
    2 1 -9 1 0 11075:dcd43b05
    2 1 -9 1 1 11075:dcd43b05
    2 1 -9 1 2 13160:b3ce8931
    2 1 -9 1 3 13160:b3ce8931
    2 1 -9 1 4 12491:5cc4d9ce
    2 1 -9 8 0 8314:b8b485ea
    2 1 -9 8 1 8314:b8b485ea
    2 1 -9 8 2 9699:8c0e1549
    2 1 -9 8 3 9699:8c0e1549
    2 1 -9 8 4 12390:4ddb5faf
    2 1 -9 9 0 8314:b8b485ea
    2 1 -9 9 1 8314:b8b485ea
    2 1 -9 9 2 9698:d3a4dfdf
    2 1 -9 9 3 9698:d3a4dfdf
    2 1 -9 9 4 12390:4ddb5faf
    2 1 9 1 0 11081:19082ccf
    2 1 9 1 1 11081:19082ccf
    2 1 9 1 2 13166:db2440ec
    2 1 9 1 3 13166:db2440ec
    2 1 9 1 4 12497:48e4dd29
    2 1 9 8 0 8320:9b05c553
    2 1 9 8 1 8320:9b05c553
    2 1 9 8 2 9705:67cc6276
    2 1 9 8 3 9705:67cc6276
    2 1 9 8 4 12396:39c5082a
    2 1 9 9 0 8320:9b05c553
    2 1 9 9 1 8320:9b05c553
    2 1 9 9 2 9704:913f0e8d
    2 1 9 9 3 9704:913f0e8d
    2 1 9 9 4 12396:39c5082a
    2 1 15 1 0 600:576704e1
    2 1 15 1 1 600:576704e1
    2 1 15 1 2 13166:eb5a9a85
    2 1 15 1 3 13166:eb5a9a85
    2 1 15 1 4 701:959d4e9a
    2 1 15 8 0 545:03f28ddb
    2 1 15 8 1 545:03f28ddb
    2 1 15 8 2 9705:b974f695
    2 1 15 8 3 9705:b974f695
    2 1 15 8 4 671:5bb92405
    2 1 15 9 0 545:03f28ddb
    2 1 15 9 1 545:03f28ddb
    2 1 15 9 2 9704:92352860
    2 1 15 9 3 9704:92352860
    2 1 15 9 4 671:5bb92405
    2 2 -15 1 0 557:36fea0d1
    2 2 -15 1 1 557:36fea0d1
    2 2 -15 1 2 13160:b3ce8931
    2 2 -15 1 3 13160:b3ce8931
    2 2 -15 1 4 654:4025fc72
    2 2 -15 8 0 518:5e566315
    2 2 -15 8 1 518:5e566315
    2 2 -15 8 2 9699:8c0e1549
    2 2 -15 8 3 9699:8c0e1549
    2 2 -15 8 4 637:fe76f89a
    2 2 -15 9 0 518:5e566315
    2 2 -15 9 1 518:5e566315
    2 2 -15 9 2 9698:d3a4dfdf
    2 2 -15 9 3 9698:d3a4dfdf
    2 2 -15 9 4 637:fe76f89a
    2 2 -9 1 0 11000:fa98ca33
    2 2 -9 1 1 11000:fa98ca33
    2 2 -9 1 2 13160:b3ce8931
    2 2 -9 1 3 13160:b3ce8931
    2 2 -9 1 4 12394:85f7ad3c
    2 2 -9 8 0 8255:dc2cd458
    2 2 -9 8 1 8255:dc2cd458
    2 2 -9 8 2 9699:8c0e1549
    2 2 -9 8 3 9699:8c0e1549
    2 2 -9 8 4 12293:07d83022
    2 2 -9 9 0 8255:dc2cd458
    2 2 -9 9 1 8255:dc2cd458
    2 2 -9 9 2 9698:d3a4dfdf
    2 2 -9 9 3 9698:d3a4dfdf
    2 2 -9 9 4 12293:07d83022
    2 2 9 1 0 11006:862470b5
    2 2 9 1 1 11006:862470b5
    2 2 9 1 2 13166:db2440ec
    2 2 9 1 3 13166:db2440ec
    2 2 9 1 4 12400:c5554b20
    2 2 9 8 0 8261:90147b77
    2 2 9 8 1 8261:90147b77
    2 2 9 8 2 9705:67cc6276
    2 2 9 8 3 9705:67cc6276
    2 2 9 8 4 12299:426c8374
    2 2 9 9 0 8261:90147b77
    2 2 9 9 1 8261:90147b77
    2 2 9 9 2 9704:913f0e8d
    2 2 9 9 3 9704:913f0e8d
    2 2 9 9 4 12299:426c8374
    2 2 15 1 0 563:98df6a4d
    2 2 15 1 1 563:98df6a4d
    2 2 15 1 2 13166:eb5a9a85
    2 2 15 1 3 13166:eb5a9a85
    2 2 15 1 4 660:b3d7c328
    2 2 15 8 0 524:9d97a3ea
    2 2 15 8 1 524:9d97a3ea
    2 2 15 8 2 9705:b974f695
    2 2 15 8 3 9705:b974f695
    2 2 15 8 4 643:c577ef21
    2 2 15 9 0 524:9d97a3ea
    2 2 15 9 1 524:9d97a3ea
    2 2 15 9 2 9704:92352860
    2 2 15 9 3 9704:92352860
    2 2 15 9 4 643:c577ef21
    2 3 -15 1 0 542:4d4b5ab1
    2 3 -15 1 1 542:4d4b5ab1
    2 3 -15 1 2 13160:b3ce8931
    2 3 -15 1 3 13160:b3ce8931
    2 3 -15 1 4 636:a177056c
    2 3 -15 8 0 510:736bf97d
    2 3 -15 8 1 510:736bf97d
    2 3 -15 8 2 9699:8c0e1549
    2 3 -15 8 3 9699:8c0e1549
    2 3 -15 8 4 632:50a88a0b
    2 3 -15 9 0 510:736bf97d
    2 3 -15 9 1 510:736bf97d
    2 3 -15 9 2 9698:d3a4dfdf
    2 3 -15 9 3 9698:d3a4dfdf
    2 3 -15 9 4 632:50a88a0b
    2 3 -9 1 0 10797:afea2dd5
    2 3 -9 1 1 10797:afea2dd5
    2 3 -9 1 2 13160:b3ce8931
    2 3 -9 1 3 13160:b3ce8931
    2 3 -9 1 4 12177:0f2a6a0a
    2 3 -9 8 0 8144:e1389d2e
    2 3 -9 8 1 8144:e1389d2e
    2 3 -9 8 2 9699:8c0e1549
    2 3 -9 8 3 9699:8c0e1549
    2 3 -9 8 4 12078:64dbf653
    2 3 -9 9 0 8144:e1389d2e
    2 3 -9 9 1 8144:e1389d2e
    2 3 -9 9 2 9698:d3a4dfdf
    2 3 -9 9 3 9698:d3a4dfdf
    2 3 -9 9 4 12078:64dbf653
    2 3 9 1 0 10803:baede2b2
    2 3 9 1 1 10803:baede2b2
    2 3 9 1 2 13166:db2440ec
    2 3 9 1 3 13166:db2440ec
    2 3 9 1 4 12183:94cbd1dc
    2 3 9 8 0 8150:a53b127b
    2 3 9 8 1 8150:a53b127b
    2 3 9 8 2 9705:67cc6276
    2 3 9 8 3 9705:67cc6276
    2 3 9 8 4 12084:46f53974
    2 3 9 9 0 8150:a53b127b
    2 3 9 9 1 8150:a53b127b
    2 3 9 9 2 9704:913f0e8d
    2 3 9 9 3 9704:913f0e8d
    2 3 9 9 4 12084:46f53974
    2 3 15 1 0 548:84a6d572
    2 3 15 1 1 548:84a6d572
    2 3 15 1 2 13166:eb5a9a85
    2 3 15 1 3 13166:eb5a9a85
    2 3 15 1 4 642:ecc50344
    2 3 15 8 0 516:9e4b0ef5
    2 3 15 8 1 516:9e4b0ef5
    2 3 15 8 2 9705:b974f695
    2 3 15 8 3 9705:b974f695
    2 3 15 8 4 638:652dd49d
    2 3 15 9 0 516:9e4b0ef5
    2 3 15 9 1 516:9e4b0ef5
    2 3 15 9 2 9704:92352860
    2 3 15 9 3 9704:92352860
    2 3 15 9 4 638:652dd49d
    2 4 -15 1 0 498:deeb3af6
    2 4 -15 1 1 541:66d3f93b
    2 4 -15 1 2 13160:b3ce8931
    2 4 -15 1 3 13160:b3ce8931
    2 4 -15 1 4 599:237ddf74
    2 4 -15 8 0 455:7b43737a
    2 4 -15 8 1 461:6ad7bff2
    2 4 -15 8 2 9699:8c0e1549
    2 4 -15 8 3 9699:8c0e1549
    2 4 -15 8 4 594:070b2e33
    2 4 -15 9 0 455:7b43737a
    2 4 -15 9 1 461:6ad7bff2
    2 4 -15 9 2 9698:d3a4dfdf
    2 4 -15 9 3 9698:d3a4dfdf
    2 4 -15 9 4 594:070b2e33
    2 4 -9 1 0 10636:0ac61f68
    2 4 -9 1 1 11746:7fb58c21
    2 4 -9 1 2 13160:b3ce8931
    2 4 -9 1 3 13160:b3ce8931
    2 4 -9 1 4 11987:dbe7f833
    2 4 -9 8 0 8041:5c8f281b
    2 4 -9 8 1 8514:46df861d
    2 4 -9 8 2 9699:8c0e1549
    2 4 -9 8 3 9699:8c0e1549
    2 4 -9 8 4 11889:ca972a2f
    2 4 -9 9 0 8041:5c8f281b
    2 4 -9 9 1 8514:46df861d
    2 4 -9 9 2 9698:d3a4dfdf
    2 4 -9 9 3 9698:d3a4dfdf
    2 4 -9 9 4 11889:ca972a2f
    2 4 9 1 0 10642:1f01abeb
    2 4 9 1 1 11752:53b73d43
    2 4 9 1 2 13166:db2440ec
    2 4 9 1 3 13166:db2440ec
    2 4 9 1 4 11993:152d7584
    2 4 9 8 0 8047:9a4080a2
    2 4 9 8 1 8520:33f20793
    2 4 9 8 2 9705:67cc6276
    2 4 9 8 3 9705:67cc6276
    2 4 9 8 4 11895:c887398f
    2 4 9 9 0 8047:9a4080a2
    2 4 9 9 1 8520:33f20793
    2 4 9 9 2 9704:913f0e8d
    2 4 9 9 3 9704:913f0e8d
    2 4 9 9 4 11895:c887398f
    2 4 15 1 0 504:e38be0dc
    2 4 15 1 1 547:ecab9876
    2 4 15 1 2 13166:eb5a9a85
    2 4 15 1 3 13166:eb5a9a85
    2 4 15 1 4 605:e8914d81
    2 4 15 8 0 461:37763386
    2 4 15 8 1 467:0c2942b1
    2 4 15 8 2 9705:b974f695
    2 4 15 8 3 9705:b974f695
    2 4 15 8 4 600:1a3fd31f
    2 4 15 9 0 461:37763386
    2 4 15 9 1 467:0c2942b1
    2 4 15 9 2 9704:92352860
    2 4 15 9 3 9704:92352860
    2 4 15 9 4 600:1a3fd31f
    2 5 -15 1 0 490:7ef70261
    2 5 -15 1 1 532:0796be41
    2 5 -15 1 2 13160:b3ce8931
    2 5 -15 1 3 13160:b3ce8931
    2 5 -15 1 4 593:a99f06a0
    2 5 -15 8 0 447:e0cd26be
    2 5 -15 8 1 455:84c96125
    2 5 -15 8 2 9699:8c0e1549
    2 5 -15 8 3 9699:8c0e1549
    2 5 -15 8 4 589:4eeea140
    2 5 -15 9 0 447:e0cd26be
    2 5 -15 9 1 455:84c96125
    2 5 -15 9 2 9698:d3a4dfdf
    2 5 -15 9 3 9698:d3a4dfdf
    2 5 -15 9 4 589:4eeea140
    2 5 -9 1 0 10636:0ac61f68
    2 5 -9 1 1 11746:7fb58c21
    2 5 -9 1 2 13160:b3ce8931
    2 5 -9 1 3 13160:b3ce8931
    2 5 -9 1 4 11987:dbe7f833
    2 5 -9 8 0 8041:5c8f281b
    2 5 -9 8 1 8514:46df861d
    2 5 -9 8 2 9699:8c0e1549
    2 5 -9 8 3 9699:8c0e1549
    2 5 -9 8 4 11889:ca972a2f
    2 5 -9 9 0 8041:5c8f281b
    2 5 -9 9 1 8514:46df861d
    2 5 -9 9 2 9698:d3a4dfdf
    2 5 -9 9 3 9698:d3a4dfdf
    2 5 -9 9 4 11889:ca972a2f
    2 5 9 1 0 10642:1f01abeb
    2 5 9 1 1 11752:53b73d43
    2 5 9 1 2 13166:db2440ec
    2 5 9 1 3 13166:db2440ec
    2 5 9 1 4 11993:152d7584
    2 5 9 8 0 8047:9a4080a2
    2 5 9 8 1 8520:33f20793
    2 5 9 8 2 9705:67cc6276
    2 5 9 8 3 9705:67cc6276
    2 5 9 8 4 11895:c887398f
    2 5 9 9 0 8047:9a4080a2
    2 5 9 9 1 8520:33f20793
    2 5 9 9 2 9704:913f0e8d
    2 5 9 9 3 9704:913f0e8d
    2 5 9 9 4 11895:c887398f
    2 5 15 1 0 496:4ac585e7
    2 5 15 1 1 538:ae1bb0a3
    2 5 15 1 2 13166:eb5a9a85
    2 5 15 1 3 13166:eb5a9a85
    2 5 15 1 4 599:4dc8e9d7
    2 5 15 8 0 453:6b39984a
    2 5 15 8 1 461:57d4c420
    2 5 15 8 2 9705:b974f695
    2 5 15 8 3 9705:b974f695
    2 5 15 8 4 595:e9c1e408
    2 5 15 9 0 453:6b39984a
    2 5 15 9 1 461:57d4c420
    2 5 15 9 2 9704:92352860
    2 5 15 9 3 9704:92352860
    2 5 15 9 4 595:e9c1e408
    2 6 -15 1 0 490:7ef70261
    2 6 -15 1 1 532:0796be41
    2 6 -15 1 2 13160:b3ce8931
    2 6 -15 1 3 13160:b3ce8931
    2 6 -15 1 4 593:a99f06a0
    2 6 -15 8 0 447:e0cd26be
    2 6 -15 8 1 455:84c96125
    2 6 -15 8 2 9699:8c0e1549
    2 6 -15 8 3 9699:8c0e1549
    2 6 -15 8 4 589:4eeea140
    2 6 -15 9 0 447:e0cd26be
    2 6 -15 9 1 455:84c96125
    2 6 -15 9 2 9698:d3a4dfdf
    2 6 -15 9 3 9698:d3a4dfdf
    2 6 -15 9 4 589:4eeea140
    2 6 -9 1 0 10636:0ac61f68
    2 6 -9 1 1 11746:7fb58c21
    2 6 -9 1 2 13160:b3ce8931
    2 6 -9 1 3 13160:b3ce8931
    2 6 -9 1 4 11987:dbe7f833
    2 6 -9 8 0 8041:5c8f281b
    2 6 -9 8 1 8514:46df861d
    2 6 -9 8 2 9699:8c0e1549
    2 6 -9 8 3 9699:8c0e1549
    2 6 -9 8 4 11889:ca972a2f
    2 6 -9 9 0 8041:5c8f281b
    2 6 -9 9 1 8514:46df861d
    2 6 -9 9 2 9698:d3a4dfdf
    2 6 -9 9 3 9698:d3a4dfdf
    2 6 -9 9 4 11889:ca972a2f
    2 6 9 1 0 10642:da56df4b
    2 6 9 1 1 11752:1b46bf23
    2 6 9 1 2 13166:db2440ec
    2 6 9 1 3 13166:db2440ec
    2 6 9 1 4 11993:152d7584
    2 6 9 8 0 8047:92602365
    2 6 9 8 1 8520:a720c90a
    2 6 9 8 2 9705:67cc6276
    2 6 9 8 3 9705:67cc6276
    2 6 9 8 4 11895:c887398f
    2 6 9 9 0 8047:92602365
    2 6 9 9 1 8520:a720c90a
    2 6 9 9 2 9704:913f0e8d
    2 6 9 9 3 9704:913f0e8d
    2 6 9 9 4 11895:c887398f
    2 6 15 1 0 496:93416d78
    2 6 15 1 1 538:98f980aa
    2 6 15 1 2 13166:eb5a9a85
    2 6 15 1 3 13166:eb5a9a85
    2 6 15 1 4 599:4dc8e9d7
    2 6 15 8 0 453:af4afcc9
    2 6 15 8 1 461:64ba1f9b
    2 6 15 8 2 9705:b974f695
    2 6 15 8 3 9705:b974f695
    2 6 15 8 4 595:e9c1e408
    2 6 15 9 0 453:af4afcc9
    2 6 15 9 1 461:64ba1f9b
    2 6 15 9 2 9704:92352860
    2 6 15 9 3 9704:92352860
    2 6 15 9 4 595:e9c1e408
    2 7 -15 1 0 490:7ef70261
    2 7 -15 1 1 532:0796be41
    2 7 -15 1 2 13160:b3ce8931
    2 7 -15 1 3 13160:b3ce8931
    2 7 -15 1 4 593:a99f06a0
    2 7 -15 8 0 447:e0cd26be
    2 7 -15 8 1 455:84c96125
    2 7 -15 8 2 9699:8c0e1549
    2 7 -15 8 3 9699:8c0e1549
    2 7 -15 8 4 589:4eeea140
    2 7 -15 9 0 447:e0cd26be
    2 7 -15 9 1 455:84c96125
    2 7 -15 9 2 9698:d3a4dfdf
    2 7 -15 9 3 9698:d3a4dfdf
    2 7 -15 9 4 589:4eeea140
    2 7 -9 1 0 10636:0ac61f68
    2 7 -9 1 1 11746:7fb58c21
    2 7 -9 1 2 13160:b3ce8931
    2 7 -9 1 3 13160:b3ce8931
    2 7 -9 1 4 11987:dbe7f833
    2 7 -9 8 0 8041:5c8f281b
    2 7 -9 8 1 8514:46df861d
    2 7 -9 8 2 9699:8c0e1549
    2 7 -9 8 3 9699:8c0e1549
    2 7 -9 8 4 11889:ca972a2f
    2 7 -9 9 0 8041:5c8f281b
    2 7 -9 9 1 8514:46df861d
    2 7 -9 9 2 9698:d3a4dfdf
    2 7 -9 9 3 9698:d3a4dfdf
    2 7 -9 9 4 11889:ca972a2f
    2 7 9 1 0 10642:fa000316
    2 7 9 1 1 11752:37542107
    2 7 9 1 2 13166:db2440ec
    2 7 9 1 3 13166:db2440ec
    2 7 9 1 4 11993:152d7584
    2 7 9 8 0 8047:90787e8d
    2 7 9 8 1 8520:7cad54e6
    2 7 9 8 2 9705:67cc6276
    2 7 9 8 3 9705:67cc6276
    2 7 9 8 4 11895:c887398f
    2 7 9 9 0 8047:90787e8d
    2 7 9 9 1 8520:7cad54e6
    2 7 9 9 2 9704:913f0e8d
    2 7 9 9 3 9704:913f0e8d
    2 7 9 9 4 11895:c887398f
    2 7 15 1 0 496:b6804487
    2 7 15 1 1 538:18e6e55b
    2 7 15 1 2 13166:eb5a9a85
    2 7 15 1 3 13166:eb5a9a85
    2 7 15 1 4 599:4dc8e9d7
    2 7 15 8 0 453:f6b5fc0a
    2 7 15 8 1 461:41545939
    2 7 15 8 2 9705:b974f695
    2 7 15 8 3 9705:b974f695
    2 7 15 8 4 595:e9c1e408
    2 7 15 9 0 453:f6b5fc0a
    2 7 15 9 1 461:41545939
    2 7 15 9 2 9704:92352860
    2 7 15 9 3 9704:92352860
    2 7 15 9 4 595:e9c1e408
    2 8 -15 1 0 490:7ef70261
    2 8 -15 1 1 532:0796be41
    2 8 -15 1 2 13160:b3ce8931
    2 8 -15 1 3 13160:b3ce8931
    2 8 -15 1 4 593:a99f06a0
    2 8 -15 8 0 447:e0cd26be
    2 8 -15 8 1 455:84c96125
    2 8 -15 8 2 9699:8c0e1549
    2 8 -15 8 3 9699:8c0e1549
    2 8 -15 8 4 589:4eeea140
    2 8 -15 9 0 447:e0cd26be
    2 8 -15 9 1 455:84c96125
    2 8 -15 9 2 9698:d3a4dfdf
    2 8 -15 9 3 9698:d3a4dfdf
    2 8 -15 9 4 589:4eeea140
    2 8 -9 1 0 10636:0ac61f68
    2 8 -9 1 1 11746:7fb58c21
    2 8 -9 1 2 13160:b3ce8931
    2 8 -9 1 3 13160:b3ce8931
    2 8 -9 1 4 11987:dbe7f833
    2 8 -9 8 0 8041:5c8f281b
    2 8 -9 8 1 8514:46df861d
    2 8 -9 8 2 9699:8c0e1549
    2 8 -9 8 3 9699:8c0e1549
    2 8 -9 8 4 11889:ca972a2f
    2 8 -9 9 0 8041:5c8f281b
    2 8 -9 9 1 8514:46df861d
    2 8 -9 9 2 9698:d3a4dfdf
    2 8 -9 9 3 9698:d3a4dfdf
    2 8 -9 9 4 11889:ca972a2f
    2 8 9 1 0 10642:fa000316
    2 8 9 1 1 11752:37542107
    2 8 9 1 2 13166:db2440ec
    2 8 9 1 3 13166:db2440ec
    2 8 9 1 4 11993:152d7584
    2 8 9 8 0 8047:90787e8d
    2 8 9 8 1 8520:7cad54e6
    2 8 9 8 2 9705:67cc6276
    2 8 9 8 3 9705:67cc6276
    2 8 9 8 4 11895:c887398f
    2 8 9 9 0 8047:90787e8d
    2 8 9 9 1 8520:7cad54e6
    2 8 9 9 2 9704:913f0e8d
    2 8 9 9 3 9704:913f0e8d
    2 8 9 9 4 11895:c887398f
    2 8 15 1 0 496:b6804487
    2 8 15 1 1 538:18e6e55b
    2 8 15 1 2 13166:eb5a9a85
    2 8 15 1 3 13166:eb5a9a85
    2 8 15 1 4 599:4dc8e9d7
    2 8 15 8 0 453:f6b5fc0a
    2 8 15 8 1 461:41545939
    2 8 15 8 2 9705:b974f695
    2 8 15 8 3 9705:b974f695
    2 8 15 8 4 595:e9c1e408
    2 8 15 9 0 453:f6b5fc0a
    2 8 15 9 1 461:41545939
    2 8 15 9 2 9704:92352860
    2 8 15 9 3 9704:92352860
    2 8 15 9 4 595:e9c1e408
    2 9 -15 1 0 490:7ef70261
    2 9 -15 1 1 532:0796be41
    2 9 -15 1 2 13160:b3ce8931
    2 9 -15 1 3 13160:b3ce8931
    2 9 -15 1 4 593:a99f06a0
    2 9 -15 8 0 447:e0cd26be
    2 9 -15 8 1 455:84c96125
    2 9 -15 8 2 9699:8c0e1549
    2 9 -15 8 3 9699:8c0e1549
    2 9 -15 8 4 589:4eeea140
    2 9 -15 9 0 447:e0cd26be
    2 9 -15 9 1 455:84c96125
    2 9 -15 9 2 9698:d3a4dfdf
    2 9 -15 9 3 9698:d3a4dfdf
    2 9 -15 9 4 589:4eeea140
    2 9 -9 1 0 10636:0ac61f68
    2 9 -9 1 1 11746:7fb58c21
    2 9 -9 1 2 13160:b3ce8931
    2 9 -9 1 3 13160:b3ce8931
    2 9 -9 1 4 11987:dbe7f833
    2 9 -9 8 0 8041:5c8f281b
    2 9 -9 8 1 8514:46df861d
    2 9 -9 8 2 9699:8c0e1549
    2 9 -9 8 3 9699:8c0e1549
    2 9 -9 8 4 11889:ca972a2f
    2 9 -9 9 0 8041:5c8f281b
    2 9 -9 9 1 8514:46df861d
    2 9 -9 9 2 9698:d3a4dfdf
    2 9 -9 9 3 9698:d3a4dfdf
    2 9 -9 9 4 11889:ca972a2f
    2 9 9 1 0 10642:fa000316
    2 9 9 1 1 11752:37542107
    2 9 9 1 2 13166:db2440ec
    2 9 9 1 3 13166:db2440ec
    2 9 9 1 4 11993:152d7584
    2 9 9 8 0 8047:90787e8d
    2 9 9 8 1 8520:7cad54e6
    2 9 9 8 2 9705:67cc6276
    2 9 9 8 3 9705:67cc6276
    2 9 9 8 4 11895:c887398f
    2 9 9 9 0 8047:90787e8d
    2 9 9 9 1 8520:7cad54e6
    2 9 9 9 2 9704:913f0e8d
    2 9 9 9 3 9704:913f0e8d
    2 9 9 9 4 11895:c887398f
    2 9 15 1 0 496:b6804487
    2 9 15 1 1 538:18e6e55b
    2 9 15 1 2 13166:eb5a9a85
    2 9 15 1 3 13166:eb5a9a85
    2 9 15 1 4 599:4dc8e9d7
    2 9 15 8 0 453:f6b5fc0a
    2 9 15 8 1 461:41545939
    2 9 15 8 2 9705:b974f695
    2 9 15 8 3 9705:b974f695
    2 9 15 8 4 595:e9c1e408
    2 9 15 9 0 453:f6b5fc0a
    2 9 15 9 1 461:41545939
    2 9 15 9 2 9704:92352860
    2 9 15 9 3 9704:92352860
    2 9 15 9 4 595:e9c1e408
    3 -1 -15 1 0 342:e6ea9a51
    3 -1 -15 1 1 342:e6ea9a51
    3 -1 -15 1 2 16951:5d8760f2
    3 -1 -15 1 3 16951:5d8760f2
    3 -1 -15 1 4 415:6c933758
    3 -1 -15 8 0 402:f9b82748
    3 -1 -15 8 1 402:f9b82748
    3 -1 -15 8 2 16391:b3b3effb
    3 -1 -15 8 3 16391:b3b3effb
    3 -1 -15 8 4 423:f8d8395a
    3 -1 -15 9 0 402:f9b82748
    3 -1 -15 9 1 402:f9b82748
    3 -1 -15 9 2 16389:5459e5ea
    3 -1 -15 9 3 16389:5459e5ea
    3 -1 -15 9 4 423:f8d8395a
    3 -1 -9 1 0 16951:5d8760f2
    3 -1 -9 1 1 16951:5d8760f2
    3 -1 -9 1 2 16951:5d8760f2
    3 -1 -9 1 3 16951:5d8760f2
    3 -1 -9 1 4 16951:5d8760f2
    3 -1 -9 8 0 16422:5bd1545c
    3 -1 -9 8 1 16422:5bd1545c
    3 -1 -9 8 2 16422:5bd1545c
    3 -1 -9 8 3 16422:5bd1545c
    3 -1 -9 8 4 17283:e806bfa4
    3 -1 -9 9 0 16420:3864aec7
    3 -1 -9 9 1 16420:3864aec7
    3 -1 -9 9 2 16420:3864aec7
    3 -1 -9 9 3 16420:3864aec7
    3 -1 -9 9 4 17282:55ce9339
    3 -1 9 1 0 16957:1cc8f95c
    3 -1 9 1 1 16957:1cc8f95c
    3 -1 9 1 2 16957:ce3cb7f3
    3 -1 9 1 3 16957:ce3cb7f3
    3 -1 9 1 4 16957:ce3cb7f3
    3 -1 9 8 0 16428:e5c6fb9f
    3 -1 9 8 1 16428:e5c6fb9f
    3 -1 9 8 2 16428:42005d80
    3 -1 9 8 3 16428:42005d80
    3 -1 9 8 4 17289:59ece51c
    3 -1 9 9 0 16426:fcd7cd0d
    3 -1 9 9 1 16426:fcd7cd0d
    3 -1 9 9 2 16426:ad3a57ac
    3 -1 9 9 3 16426:ad3a57ac
    3 -1 9 9 4 17288:a807478c
    3 -1 15 1 0 348:2544c670
    3 -1 15 1 1 348:2544c670
    3 -1 15 1 2 16957:55bc7d80
    3 -1 15 1 3 16957:55bc7d80
    3 -1 15 1 4 421:1a54baf8
    3 -1 15 8 0 408:efc5d88a
    3 -1 15 8 1 408:efc5d88a
    3 -1 15 8 2 16397:939fadd1
    3 -1 15 8 3 16397:939fadd1
    3 -1 15 8 4 429:fa8bde4e
    3 -1 15 9 0 408:efc5d88a
    3 -1 15 9 1 408:efc5d88a
    3 -1 15 9 2 16395:33b78d18
    3 -1 15 9 3 16395:33b78d18
    3 -1 15 9 4 429:fa8bde4e
    3 0 -15 1 0 16389:5459e5ea
    3 0 -15 1 1 16389:5459e5ea
    3 0 -15 1 2 16389:5459e5ea
    3 0 -15 1 3 16389:5459e5ea
    3 0 -15 1 4 16389:5459e5ea
    3 0 -15 8 0 16389:5459e5ea
    3 0 -15 8 1 16389:5459e5ea
    3 0 -15 8 2 16389:5459e5ea
    3 0 -15 8 3 16389:5459e5ea
    3 0 -15 8 4 16389:5459e5ea
    3 0 -15 9 0 16389:5459e5ea
    3 0 -15 9 1 16389:5459e5ea
    3 0 -15 9 2 16389:5459e5ea
    3 0 -15 9 3 16389:5459e5ea
    3 0 -15 9 4 16389:5459e5ea
    3 0 -9 1 0 16389:5459e5ea
    3 0 -9 1 1 16389:5459e5ea
    3 0 -9 1 2 16389:5459e5ea
    3 0 -9 1 3 16389:5459e5ea
    3 0 -9 1 4 16389:5459e5ea
    3 0 -9 8 0 16389:5459e5ea
    3 0 -9 8 1 16389:5459e5ea
    3 0 -9 8 2 16389:5459e5ea
    3 0 -9 8 3 16389:5459e5ea
    3 0 -9 8 4 16389:5459e5ea
    3 0 -9 9 0 16389:5459e5ea
    3 0 -9 9 1 16389:5459e5ea
    3 0 -9 9 2 16389:5459e5ea
    3 0 -9 9 3 16389:5459e5ea
    3 0 -9 9 4 16389:5459e5ea
    3 0 9 1 0 16395:40893d17
    3 0 9 1 1 16395:40893d17
    3 0 9 1 2 16395:40893d17
    3 0 9 1 3 16395:40893d17
    3 0 9 1 4 16395:40893d17
    3 0 9 8 0 16395:40893d17
    3 0 9 8 1 16395:40893d17
    3 0 9 8 2 16395:40893d17
    3 0 9 8 3 16395:40893d17
    3 0 9 8 4 16395:40893d17
    3 0 9 9 0 16395:40893d17
    3 0 9 9 1 16395:40893d17
    3 0 9 9 2 16395:40893d17
    3 0 9 9 3 16395:40893d17
    3 0 9 9 4 16395:40893d17
    3 0 15 1 0 16395:33b78d18
    3 0 15 1 1 16395:33b78d18
    3 0 15 1 2 16395:33b78d18
    3 0 15 1 3 16395:33b78d18
    3 0 15 1 4 16395:33b78d18
    3 0 15 8 0 16395:33b78d18
    3 0 15 8 1 16395:33b78d18
    3 0 15 8 2 16395:33b78d18
    3 0 15 8 3 16395:33b78d18
    3 0 15 8 4 16395:33b78d18
    3 0 15 9 0 16395:33b78d18
    3 0 15 9 1 16395:33b78d18
    3 0 15 9 2 16395:33b78d18
    3 0 15 9 3 16395:33b78d18
    3 0 15 9 4 16395:33b78d18
    3 1 -15 1 0 403:1b89b2d8
    3 1 -15 1 1 403:1b89b2d8
    3 1 -15 1 2 16951:5d8760f2
    3 1 -15 1 3 16951:5d8760f2
    3 1 -15 1 4 455:95449693
    3 1 -15 8 0 462:4f954641
    3 1 -15 8 1 462:4f954641
    3 1 -15 8 2 16391:b3b3effb
    3 1 -15 8 3 16391:b3b3effb
    3 1 -15 8 4 462:4f954641
    3 1 -15 9 0 462:4f954641
    3 1 -15 9 1 462:4f954641
    3 1 -15 9 2 16389:5459e5ea
    3 1 -15 9 3 16389:5459e5ea
    3 1 -15 9 4 462:4f954641
    3 1 -9 1 0 16951:5d8760f2
    3 1 -9 1 1 16951:5d8760f2
    3 1 -9 1 2 16951:5d8760f2
    3 1 -9 1 3 16951:5d8760f2
    3 1 -9 1 4 16951:5d8760f2
    3 1 -9 8 0 16422:5bd1545c
    3 1 -9 8 1 16422:5bd1545c
    3 1 -9 8 2 16422:5bd1545c
    3 1 -9 8 3 16422:5bd1545c
    3 1 -9 8 4 17283:e806bfa4
    3 1 -9 9 0 16420:3864aec7
    3 1 -9 9 1 16420:3864aec7
    3 1 -9 9 2 16420:3864aec7
    3 1 -9 9 3 16420:3864aec7
    3 1 -9 9 4 17282:55ce9339
    3 1 9 1 0 16957:ce3cb7f3
    3 1 9 1 1 16957:ce3cb7f3
    3 1 9 1 2 16957:ce3cb7f3
    3 1 9 1 3 16957:ce3cb7f3
    3 1 9 1 4 16957:ce3cb7f3
    3 1 9 8 0 16428:42005d80
    3 1 9 8 1 16428:42005d80
    3 1 9 8 2 16428:42005d80
    3 1 9 8 3 16428:42005d80
    3 1 9 8 4 17289:59ece51c
    3 1 9 9 0 16426:ad3a57ac
    3 1 9 9 1 16426:ad3a57ac
    3 1 9 9 2 16426:ad3a57ac
    3 1 9 9 3 16426:ad3a57ac
    3 1 9 9 4 17288:a807478c
    3 1 15 1 0 409:9866bd6d
    3 1 15 1 1 409:9866bd6d
    3 1 15 1 2 16957:55bc7d80
    3 1 15 1 3 16957:55bc7d80
    3 1 15 1 4 461:e624ca85
    3 1 15 8 0 468:a69fe1c7
    3 1 15 8 1 468:a69fe1c7
    3 1 15 8 2 16397:939fadd1
    3 1 15 8 3 16397:939fadd1
    3 1 15 8 4 468:a69fe1c7
    3 1 15 9 0 468:a69fe1c7
    3 1 15 9 1 468:a69fe1c7
    3 1 15 9 2 16395:33b78d18
    3 1 15 9 3 16395:33b78d18
    3 1 15 9 4 468:a69fe1c7
    3 2 -15 1 0 403:1b89b2d8
    3 2 -15 1 1 403:1b89b2d8
    3 2 -15 1 2 16951:5d8760f2
    3 2 -15 1 3 16951:5d8760f2
    3 2 -15 1 4 455:95449693
    3 2 -15 8 0 462:4f954641
    3 2 -15 8 1 462:4f954641
    3 2 -15 8 2 16391:b3b3effb
    3 2 -15 8 3 16391:b3b3effb
    3 2 -15 8 4 462:4f954641
    3 2 -15 9 0 462:4f954641
    3 2 -15 9 1 462:4f954641
    3 2 -15 9 2 16389:5459e5ea
    3 2 -15 9 3 16389:5459e5ea
    3 2 -15 9 4 462:4f954641
    3 2 -9 1 0 16951:5d8760f2
    3 2 -9 1 1 16951:5d8760f2
    3 2 -9 1 2 16951:5d8760f2
    3 2 -9 1 3 16951:5d8760f2
    3 2 -9 1 4 16951:5d8760f2
    3 2 -9 8 0 16422:5bd1545c
    3 2 -9 8 1 16422:5bd1545c
    3 2 -9 8 2 16422:5bd1545c
    3 2 -9 8 3 16422:5bd1545c
    3 2 -9 8 4 17283:e806bfa4
    3 2 -9 9 0 16420:3864aec7
    3 2 -9 9 1 16420:3864aec7
    3 2 -9 9 2 16420:3864aec7
    3 2 -9 9 3 16420:3864aec7
    3 2 -9 9 4 17282:55ce9339
    3 2 9 1 0 16957:55b3722f
    3 2 9 1 1 16957:55b3722f
    3 2 9 1 2 16957:ce3cb7f3
    3 2 9 1 3 16957:ce3cb7f3
    3 2 9 1 4 16957:ce3cb7f3
    3 2 9 8 0 16428:bd86bc01
    3 2 9 8 1 16428:bd86bc01
    3 2 9 8 2 16428:42005d80
    3 2 9 8 3 16428:42005d80
    3 2 9 8 4 17289:59ece51c
    3 2 9 9 0 16426:f5e06e35
    3 2 9 9 1 16426:f5e06e35
    3 2 9 9 2 16426:ad3a57ac
    3 2 9 9 3 16426:ad3a57ac
    3 2 9 9 4 17288:a807478c
    3 2 15 1 0 409:93333ebe
    3 2 15 1 1 409:93333ebe
    3 2 15 1 2 16957:55bc7d80
    3 2 15 1 3 16957:55bc7d80
    3 2 15 1 4 461:e624ca85
    3 2 15 8 0 468:d0e744ca
    3 2 15 8 1 468:d0e744ca
    3 2 15 8 2 16397:939fadd1
    3 2 15 8 3 16397:939fadd1
    3 2 15 8 4 468:a69fe1c7
    3 2 15 9 0 468:d0e744ca
    3 2 15 9 1 468:d0e744ca
    3 2 15 9 2 16395:33b78d18
    3 2 15 9 3 16395:33b78d18
    3 2 15 9 4 468:a69fe1c7
    3 3 -15 1 0 403:1b89b2d8
    3 3 -15 1 1 403:1b89b2d8
    3 3 -15 1 2 16951:5d8760f2
    3 3 -15 1 3 16951:5d8760f2
    3 3 -15 1 4 455:95449693
    3 3 -15 8 0 462:4f954641
    3 3 -15 8 1 462:4f954641
    3 3 -15 8 2 16391:b3b3effb
    3 3 -15 8 3 16391:b3b3effb
    3 3 -15 8 4 462:4f954641
    3 3 -15 9 0 462:4f954641
    3 3 -15 9 1 462:4f954641
    3 3 -15 9 2 16389:5459e5ea
    3 3 -15 9 3 16389:5459e5ea
    3 3 -15 9 4 462:4f954641
    3 3 -9 1 0 16951:5d8760f2
    3 3 -9 1 1 16951:5d8760f2
    3 3 -9 1 2 16951:5d8760f2
    3 3 -9 1 3 16951:5d8760f2
    3 3 -9 1 4 16951:5d8760f2
    3 3 -9 8 0 16422:5bd1545c
    3 3 -9 8 1 16422:5bd1545c
    3 3 -9 8 2 16422:5bd1545c
    3 3 -9 8 3 16422:5bd1545c
    3 3 -9 8 4 17283:e806bfa4
    3 3 -9 9 0 16420:3864aec7
    3 3 -9 9 1 16420:3864aec7
    3 3 -9 9 2 16420:3864aec7
    3 3 -9 9 3 16420:3864aec7
    3 3 -9 9 4 17282:55ce9339
    3 3 9 1 0 16957:55b3722f
    3 3 9 1 1 16957:55b3722f
    3 3 9 1 2 16957:ce3cb7f3
    3 3 9 1 3 16957:ce3cb7f3
    3 3 9 1 4 16957:ce3cb7f3
    3 3 9 8 0 16428:bd86bc01
    3 3 9 8 1 16428:bd86bc01
    3 3 9 8 2 16428:42005d80
    3 3 9 8 3 16428:42005d80
    3 3 9 8 4 17289:59ece51c
    3 3 9 9 0 16426:f5e06e35
    3 3 9 9 1 16426:f5e06e35
    3 3 9 9 2 16426:ad3a57ac
    3 3 9 9 3 16426:ad3a57ac
    3 3 9 9 4 17288:a807478c
    3 3 15 1 0 409:93333ebe
    3 3 15 1 1 409:93333ebe
    3 3 15 1 2 16957:55bc7d80
    3 3 15 1 3 16957:55bc7d80
    3 3 15 1 4 461:e624ca85
    3 3 15 8 0 468:d0e744ca
    3 3 15 8 1 468:d0e744ca
    3 3 15 8 2 16397:939fadd1
    3 3 15 8 3 16397:939fadd1
    3 3 15 8 4 468:a69fe1c7
    3 3 15 9 0 468:d0e744ca
    3 3 15 9 1 468:d0e744ca
    3 3 15 9 2 16395:33b78d18
    3 3 15 9 3 16395:33b78d18
    3 3 15 9 4 468:a69fe1c7
    3 4 -15 1 0 342:e6ea9a51
    3 4 -15 1 1 342:e6ea9a51
    3 4 -15 1 2 16951:5d8760f2
    3 4 -15 1 3 16951:5d8760f2
    3 4 -15 1 4 415:6c933758
    3 4 -15 8 0 402:f9b82748
    3 4 -15 8 1 402:f9b82748
    3 4 -15 8 2 16391:b3b3effb
    3 4 -15 8 3 16391:b3b3effb
    3 4 -15 8 4 423:f8d8395a
    3 4 -15 9 0 402:f9b82748
    3 4 -15 9 1 402:f9b82748
    3 4 -15 9 2 16389:5459e5ea
    3 4 -15 9 3 16389:5459e5ea
    3 4 -15 9 4 423:f8d8395a
    3 4 -9 1 0 16951:5d8760f2
    3 4 -9 1 1 16951:5d8760f2
    3 4 -9 1 2 16951:5d8760f2
    3 4 -9 1 3 16951:5d8760f2
    3 4 -9 1 4 16951:5d8760f2
    3 4 -9 8 0 16422:5bd1545c
    3 4 -9 8 1 16422:5bd1545c
    3 4 -9 8 2 16422:5bd1545c
    3 4 -9 8 3 16422:5bd1545c
    3 4 -9 8 4 17283:e806bfa4
    3 4 -9 9 0 16420:3864aec7
    3 4 -9 9 1 16420:3864aec7
    3 4 -9 9 2 16420:3864aec7
    3 4 -9 9 3 16420:3864aec7
    3 4 -9 9 4 17282:55ce9339
    3 4 9 1 0 16957:55b3722f
    3 4 9 1 1 16957:55b3722f
    3 4 9 1 2 16957:ce3cb7f3
    3 4 9 1 3 16957:ce3cb7f3
    3 4 9 1 4 16957:ce3cb7f3
    3 4 9 8 0 16428:bd86bc01
    3 4 9 8 1 16428:bd86bc01
    3 4 9 8 2 16428:42005d80
    3 4 9 8 3 16428:42005d80
    3 4 9 8 4 17289:59ece51c
    3 4 9 9 0 16426:f5e06e35
    3 4 9 9 1 16426:f5e06e35
    3 4 9 9 2 16426:ad3a57ac
    3 4 9 9 3 16426:ad3a57ac
    3 4 9 9 4 17288:a807478c
    3 4 15 1 0 348:ae685030
    3 4 15 1 1 348:ae685030
    3 4 15 1 2 16957:55bc7d80
    3 4 15 1 3 16957:55bc7d80
    3 4 15 1 4 421:1a54baf8
    3 4 15 8 0 408:ef8dbec0
    3 4 15 8 1 408:ef8dbec0
    3 4 15 8 2 16397:939fadd1
    3 4 15 8 3 16397:939fadd1
    3 4 15 8 4 429:fa8bde4e
    3 4 15 9 0 408:ef8dbec0
    3 4 15 9 1 408:ef8dbec0
    3 4 15 9 2 16395:33b78d18
    3 4 15 9 3 16395:33b78d18
    3 4 15 9 4 429:fa8bde4e
    3 5 -15 1 0 342:e6ea9a51
    3 5 -15 1 1 342:e6ea9a51
    3 5 -15 1 2 16951:5d8760f2
    3 5 -15 1 3 16951:5d8760f2
    3 5 -15 1 4 415:6c933758
    3 5 -15 8 0 402:f9b82748
    3 5 -15 8 1 402:f9b82748
    3 5 -15 8 2 16391:b3b3effb
    3 5 -15 8 3 16391:b3b3effb
    3 5 -15 8 4 423:f8d8395a
    3 5 -15 9 0 402:f9b82748
    3 5 -15 9 1 402:f9b82748
    3 5 -15 9 2 16389:5459e5ea
    3 5 -15 9 3 16389:5459e5ea
    3 5 -15 9 4 423:f8d8395a
    3 5 -9 1 0 16951:5d8760f2
    3 5 -9 1 1 16951:5d8760f2
    3 5 -9 1 2 16951:5d8760f2
    3 5 -9 1 3 16951:5d8760f2
    3 5 -9 1 4 16951:5d8760f2
    3 5 -9 8 0 16422:5bd1545c
    3 5 -9 8 1 16422:5bd1545c
    3 5 -9 8 2 16422:5bd1545c
    3 5 -9 8 3 16422:5bd1545c
    3 5 -9 8 4 17283:e806bfa4
    3 5 -9 9 0 16420:3864aec7
    3 5 -9 9 1 16420:3864aec7
    3 5 -9 9 2 16420:3864aec7
    3 5 -9 9 3 16420:3864aec7
    3 5 -9 9 4 17282:55ce9339
    3 5 9 1 0 16957:55b3722f
    3 5 9 1 1 16957:55b3722f
    3 5 9 1 2 16957:ce3cb7f3
    3 5 9 1 3 16957:ce3cb7f3
    3 5 9 1 4 16957:ce3cb7f3
    3 5 9 8 0 16428:bd86bc01
    3 5 9 8 1 16428:bd86bc01
    3 5 9 8 2 16428:42005d80
    3 5 9 8 3 16428:42005d80
    3 5 9 8 4 17289:59ece51c
    3 5 9 9 0 16426:f5e06e35
    3 5 9 9 1 16426:f5e06e35
    3 5 9 9 2 16426:ad3a57ac
    3 5 9 9 3 16426:ad3a57ac
    3 5 9 9 4 17288:a807478c
    3 5 15 1 0 348:ae685030
    3 5 15 1 1 348:ae685030
    3 5 15 1 2 16957:55bc7d80
    3 5 15 1 3 16957:55bc7d80
    3 5 15 1 4 421:1a54baf8
    3 5 15 8 0 408:ef8dbec0
    3 5 15 8 1 408:ef8dbec0
    3 5 15 8 2 16397:939fadd1
    3 5 15 8 3 16397:939fadd1
    3 5 15 8 4 429:fa8bde4e
    3 5 15 9 0 408:ef8dbec0
    3 5 15 9 1 408:ef8dbec0
    3 5 15 9 2 16395:33b78d18
    3 5 15 9 3 16395:33b78d18
    3 5 15 9 4 429:fa8bde4e
    3 6 -15 1 0 342:e6ea9a51
    3 6 -15 1 1 342:e6ea9a51
    3 6 -15 1 2 16951:5d8760f2
    3 6 -15 1 3 16951:5d8760f2
    3 6 -15 1 4 415:6c933758
    3 6 -15 8 0 402:f9b82748
    3 6 -15 8 1 402:f9b82748
    3 6 -15 8 2 16391:b3b3effb
    3 6 -15 8 3 16391:b3b3effb
    3 6 -15 8 4 423:f8d8395a
    3 6 -15 9 0 402:f9b82748
    3 6 -15 9 1 402:f9b82748
    3 6 -15 9 2 16389:5459e5ea
    3 6 -15 9 3 16389:5459e5ea
    3 6 -15 9 4 423:f8d8395a
    3 6 -9 1 0 16951:5d8760f2
    3 6 -9 1 1 16951:5d8760f2
    3 6 -9 1 2 16951:5d8760f2
    3 6 -9 1 3 16951:5d8760f2
    3 6 -9 1 4 16951:5d8760f2
    3 6 -9 8 0 16422:5bd1545c
    3 6 -9 8 1 16422:5bd1545c
    3 6 -9 8 2 16422:5bd1545c
    3 6 -9 8 3 16422:5bd1545c
    3 6 -9 8 4 17283:e806bfa4
    3 6 -9 9 0 16420:3864aec7
    3 6 -9 9 1 16420:3864aec7
    3 6 -9 9 2 16420:3864aec7
    3 6 -9 9 3 16420:3864aec7
    3 6 -9 9 4 17282:55ce9339
    3 6 9 1 0 16957:1cc8f95c
    3 6 9 1 1 16957:1cc8f95c
    3 6 9 1 2 16957:ce3cb7f3
    3 6 9 1 3 16957:ce3cb7f3
    3 6 9 1 4 16957:ce3cb7f3
    3 6 9 8 0 16428:e5c6fb9f
    3 6 9 8 1 16428:e5c6fb9f
    3 6 9 8 2 16428:42005d80
    3 6 9 8 3 16428:42005d80
    3 6 9 8 4 17289:59ece51c
    3 6 9 9 0 16426:fcd7cd0d
    3 6 9 9 1 16426:fcd7cd0d
    3 6 9 9 2 16426:ad3a57ac
    3 6 9 9 3 16426:ad3a57ac
    3 6 9 9 4 17288:a807478c
    3 6 15 1 0 348:2544c670
    3 6 15 1 1 348:2544c670
    3 6 15 1 2 16957:55bc7d80
    3 6 15 1 3 16957:55bc7d80
    3 6 15 1 4 421:1a54baf8
    3 6 15 8 0 408:efc5d88a
    3 6 15 8 1 408:efc5d88a
    3 6 15 8 2 16397:939fadd1
    3 6 15 8 3 16397:939fadd1
    3 6 15 8 4 429:fa8bde4e
    3 6 15 9 0 408:efc5d88a
    3 6 15 9 1 408:efc5d88a
    3 6 15 9 2 16395:33b78d18
    3 6 15 9 3 16395:33b78d18
    3 6 15 9 4 429:fa8bde4e
    3 7 -15 1 0 342:e6ea9a51
    3 7 -15 1 1 342:e6ea9a51
    3 7 -15 1 2 16951:5d8760f2
    3 7 -15 1 3 16951:5d8760f2
    3 7 -15 1 4 415:6c933758
    3 7 -15 8 0 402:f9b82748
    3 7 -15 8 1 402:f9b82748
    3 7 -15 8 2 16391:b3b3effb
    3 7 -15 8 3 16391:b3b3effb
    3 7 -15 8 4 423:f8d8395a
    3 7 -15 9 0 402:f9b82748
    3 7 -15 9 1 402:f9b82748
    3 7 -15 9 2 16389:5459e5ea
    3 7 -15 9 3 16389:5459e5ea
    3 7 -15 9 4 423:f8d8395a
    3 7 -9 1 0 16951:5d8760f2
    3 7 -9 1 1 16951:5d8760f2
    3 7 -9 1 2 16951:5d8760f2
    3 7 -9 1 3 16951:5d8760f2
    3 7 -9 1 4 16951:5d8760f2
    3 7 -9 8 0 16422:5bd1545c
    3 7 -9 8 1 16422:5bd1545c
    3 7 -9 8 2 16422:5bd1545c
    3 7 -9 8 3 16422:5bd1545c
    3 7 -9 8 4 17283:e806bfa4
    3 7 -9 9 0 16420:3864aec7
    3 7 -9 9 1 16420:3864aec7
    3 7 -9 9 2 16420:3864aec7
    3 7 -9 9 3 16420:3864aec7
    3 7 -9 9 4 17282:55ce9339
    3 7 9 1 0 16957:980a5d2b
    3 7 9 1 1 16957:980a5d2b
    3 7 9 1 2 16957:ce3cb7f3
    3 7 9 1 3 16957:ce3cb7f3
    3 7 9 1 4 16957:ce3cb7f3
    3 7 9 8 0 16428:5b9d2bb0
    3 7 9 8 1 16428:5b9d2bb0
    3 7 9 8 2 16428:42005d80
    3 7 9 8 3 16428:42005d80
    3 7 9 8 4 17289:59ece51c
    3 7 9 9 0 16426:3999837d
    3 7 9 9 1 16426:3999837d
    3 7 9 9 2 16426:ad3a57ac
    3 7 9 9 3 16426:ad3a57ac
    3 7 9 9 4 17288:a807478c
    3 7 15 1 0 348:cf7617fb
    3 7 15 1 1 348:cf7617fb
    3 7 15 1 2 16957:55bc7d80
    3 7 15 1 3 16957:55bc7d80
    3 7 15 1 4 421:1a54baf8
    3 7 15 8 0 408:7c7d363e
    3 7 15 8 1 408:7c7d363e
    3 7 15 8 2 16397:939fadd1
    3 7 15 8 3 16397:939fadd1
    3 7 15 8 4 429:fa8bde4e
    3 7 15 9 0 408:7c7d363e
    3 7 15 9 1 408:7c7d363e
    3 7 15 9 2 16395:33b78d18
    3 7 15 9 3 16395:33b78d18
    3 7 15 9 4 429:fa8bde4e
    3 8 -15 1 0 342:e6ea9a51
    3 8 -15 1 1 342:e6ea9a51
    3 8 -15 1 2 16951:5d8760f2
    3 8 -15 1 3 16951:5d8760f2
    3 8 -15 1 4 415:6c933758
    3 8 -15 8 0 402:f9b82748
    3 8 -15 8 1 402:f9b82748
    3 8 -15 8 2 16391:b3b3effb
    3 8 -15 8 3 16391:b3b3effb
    3 8 -15 8 4 423:f8d8395a
    3 8 -15 9 0 402:f9b82748
    3 8 -15 9 1 402:f9b82748
    3 8 -15 9 2 16389:5459e5ea
    3 8 -15 9 3 16389:5459e5ea
    3 8 -15 9 4 423:f8d8395a
    3 8 -9 1 0 16951:5d8760f2
    3 8 -9 1 1 16951:5d8760f2
    3 8 -9 1 2 16951:5d8760f2
    3 8 -9 1 3 16951:5d8760f2
    3 8 -9 1 4 16951:5d8760f2
    3 8 -9 8 0 16422:5bd1545c
    3 8 -9 8 1 16422:5bd1545c
    3 8 -9 8 2 16422:5bd1545c
    3 8 -9 8 3 16422:5bd1545c
    3 8 -9 8 4 17283:e806bfa4
    3 8 -9 9 0 16420:3864aec7
    3 8 -9 9 1 16420:3864aec7
    3 8 -9 9 2 16420:3864aec7
    3 8 -9 9 3 16420:3864aec7
    3 8 -9 9 4 17282:55ce9339
    3 8 9 1 0 16957:980a5d2b
    3 8 9 1 1 16957:980a5d2b
    3 8 9 1 2 16957:ce3cb7f3
    3 8 9 1 3 16957:ce3cb7f3
    3 8 9 1 4 16957:ce3cb7f3
    3 8 9 8 0 16428:5b9d2bb0
    3 8 9 8 1 16428:5b9d2bb0
    3 8 9 8 2 16428:42005d80
    3 8 9 8 3 16428:42005d80
    3 8 9 8 4 17289:59ece51c
    3 8 9 9 0 16426:3999837d
    3 8 9 9 1 16426:3999837d
    3 8 9 9 2 16426:ad3a57ac
    3 8 9 9 3 16426:ad3a57ac
    3 8 9 9 4 17288:a807478c
    3 8 15 1 0 348:cf7617fb
    3 8 15 1 1 348:cf7617fb
    3 8 15 1 2 16957:55bc7d80
    3 8 15 1 3 16957:55bc7d80
    3 8 15 1 4 421:1a54baf8
    3 8 15 8 0 408:7c7d363e
    3 8 15 8 1 408:7c7d363e
    3 8 15 8 2 16397:939fadd1
    3 8 15 8 3 16397:939fadd1
    3 8 15 8 4 429:fa8bde4e
    3 8 15 9 0 408:7c7d363e
    3 8 15 9 1 408:7c7d363e
    3 8 15 9 2 16395:33b78d18
    3 8 15 9 3 16395:33b78d18
    3 8 15 9 4 429:fa8bde4e
    3 9 -15 1 0 342:e6ea9a51
    3 9 -15 1 1 342:e6ea9a51
    3 9 -15 1 2 16951:5d8760f2
    3 9 -15 1 3 16951:5d8760f2
    3 9 -15 1 4 415:6c933758
    3 9 -15 8 0 402:f9b82748
    3 9 -15 8 1 402:f9b82748
    3 9 -15 8 2 16391:b3b3effb
    3 9 -15 8 3 16391:b3b3effb
    3 9 -15 8 4 423:f8d8395a
    3 9 -15 9 0 402:f9b82748
    3 9 -15 9 1 402:f9b82748
    3 9 -15 9 2 16389:5459e5ea
    3 9 -15 9 3 16389:5459e5ea
    3 9 -15 9 4 423:f8d8395a
    3 9 -9 1 0 16951:5d8760f2
    3 9 -9 1 1 16951:5d8760f2
    3 9 -9 1 2 16951:5d8760f2
    3 9 -9 1 3 16951:5d8760f2
    3 9 -9 1 4 16951:5d8760f2
    3 9 -9 8 0 16422:5bd1545c
    3 9 -9 8 1 16422:5bd1545c
    3 9 -9 8 2 16422:5bd1545c
    3 9 -9 8 3 16422:5bd1545c
    3 9 -9 8 4 17283:e806bfa4
    3 9 -9 9 0 16420:3864aec7
    3 9 -9 9 1 16420:3864aec7
    3 9 -9 9 2 16420:3864aec7
    3 9 -9 9 3 16420:3864aec7
    3 9 -9 9 4 17282:55ce9339
    3 9 9 1 0 16957:980a5d2b
    3 9 9 1 1 16957:980a5d2b
    3 9 9 1 2 16957:ce3cb7f3
    3 9 9 1 3 16957:ce3cb7f3
    3 9 9 1 4 16957:ce3cb7f3
    3 9 9 8 0 16428:5b9d2bb0
    3 9 9 8 1 16428:5b9d2bb0
    3 9 9 8 2 16428:42005d80
    3 9 9 8 3 16428:42005d80
    3 9 9 8 4 17289:59ece51c
    3 9 9 9 0 16426:3999837d
    3 9 9 9 1 16426:3999837d
    3 9 9 9 2 16426:ad3a57ac
    3 9 9 9 3 16426:ad3a57ac
    3 9 9 9 4 17288:a807478c
    3 9 15 1 0 348:cf7617fb
    3 9 15 1 1 348:cf7617fb
    3 9 15 1 2 16957:55bc7d80
    3 9 15 1 3 16957:55bc7d80
    3 9 15 1 4 421:1a54baf8
    3 9 15 8 0 408:7c7d363e
    3 9 15 8 1 408:7c7d363e
    3 9 15 8 2 16397:939fadd1
    3 9 15 8 3 16397:939fadd1
    3 9 15 8 4 429:fa8bde4e
    3 9 15 9 0 408:7c7d363e
    3 9 15 9 1 408:7c7d363e
    3 9 15 9 2 16395:33b78d18
    3 9 15 9 3 16395:33b78d18
    3 9 15 9 4 429:fa8bde4e
    4 -1 -15 1 0 6674:e370cb07
    4 -1 -15 1 1 6676:dbd0a539
    4 -1 -15 1 2 10563:d0738582
    4 -1 -15 1 3 6673:3132937e
    4 -1 -15 1 4 6674:e370cb07
    4 -1 -15 8 0 6353:ec502303
    4 -1 -15 8 1 6352:435663cc
    4 -1 -15 8 2 14525:5475a77d
    4 -1 -15 8 3 6352:6a525fa1
    4 -1 -15 8 4 6636:db988062
    4 -1 -15 9 0 6353:ec502303
    4 -1 -15 9 1 6352:435663cc
    4 -1 -15 9 2 14524:5bd30dd6
    4 -1 -15 9 3 6352:6a525fa1
    4 -1 -15 9 4 6636:db988062
    4 -1 -9 1 0 6674:e370cb07
    4 -1 -9 1 1 6676:dbd0a539
    4 -1 -9 1 2 10563:d0738582
    4 -1 -9 1 3 6673:3132937e
    4 -1 -9 1 4 6674:e370cb07
    4 -1 -9 8 0 6353:ec502303
    4 -1 -9 8 1 6352:435663cc
    4 -1 -9 8 2 14525:5475a77d
    4 -1 -9 8 3 6352:6a525fa1
    4 -1 -9 8 4 6636:db988062
    4 -1 -9 9 0 6353:ec502303
    4 -1 -9 9 1 6352:435663cc
    4 -1 -9 9 2 14524:5bd30dd6
    4 -1 -9 9 3 6352:6a525fa1
    4 -1 -9 9 4 6636:db988062
    4 -1 9 1 0 6680:2306e278
    4 -1 9 1 1 6682:303701ba
    4 -1 9 1 2 10569:0f93fd4d
    4 -1 9 1 3 6679:48c5cff5
    4 -1 9 1 4 6680:2e244c15
    4 -1 9 8 0 6359:a35319f4
    4 -1 9 8 1 6358:20786167
    4 -1 9 8 2 14531:97de20ff
    4 -1 9 8 3 6358:7fb0631f
    4 -1 9 8 4 6642:dd60ec5e
    4 -1 9 9 0 6359:a35319f4
    4 -1 9 9 1 6358:20786167
    4 -1 9 9 2 14530:a169ff6d
    4 -1 9 9 3 6358:7fb0631f
    4 -1 9 9 4 6642:dd60ec5e
    4 -1 15 1 0 6680:bedc53f9
    4 -1 15 1 1 6682:aec9b670
    4 -1 15 1 2 10569:92e75a30
    4 -1 15 1 3 6679:2e2a2ccf
    4 -1 15 1 4 6680:e84e7a44
    4 -1 15 8 0 6359:7478f754
    4 -1 15 8 1 6358:06442f86
    4 -1 15 8 2 14531:11876f20
    4 -1 15 8 3 6358:9e35ad6f
    4 -1 15 8 4 6642:fa455662
    4 -1 15 9 0 6359:7478f754
    4 -1 15 9 1 6358:06442f86
    4 -1 15 9 2 14530:2bf4f4bd
    4 -1 15 9 3 6358:9e35ad6f
    4 -1 15 9 4 6642:fa455662
    4 0 -15 1 0 16389:cee836da
    4 0 -15 1 1 16389:cee836da
    4 0 -15 1 2 16389:cee836da
    4 0 -15 1 3 16389:cee836da
    4 0 -15 1 4 16389:cee836da
    4 0 -15 8 0 16389:cee836da
    4 0 -15 8 1 16389:cee836da
    4 0 -15 8 2 16389:cee836da
    4 0 -15 8 3 16389:cee836da
    4 0 -15 8 4 16389:cee836da
    4 0 -15 9 0 16389:cee836da
    4 0 -15 9 1 16389:cee836da
    4 0 -15 9 2 16389:cee836da
    4 0 -15 9 3 16389:cee836da
    4 0 -15 9 4 16389:cee836da
    4 0 -9 1 0 16389:cee836da
    4 0 -9 1 1 16389:cee836da
    4 0 -9 1 2 16389:cee836da
    4 0 -9 1 3 16389:cee836da
    4 0 -9 1 4 16389:cee836da
    4 0 -9 8 0 16389:cee836da
    4 0 -9 8 1 16389:cee836da
    4 0 -9 8 2 16389:cee836da
    4 0 -9 8 3 16389:cee836da
    4 0 -9 8 4 16389:cee836da
    4 0 -9 9 0 16389:cee836da
    4 0 -9 9 1 16389:cee836da
    4 0 -9 9 2 16389:cee836da
    4 0 -9 9 3 16389:cee836da
    4 0 -9 9 4 16389:cee836da
    4 0 9 1 0 16395:a951fd73
    4 0 9 1 1 16395:a951fd73
    4 0 9 1 2 16395:a951fd73
    4 0 9 1 3 16395:a951fd73
    4 0 9 1 4 16395:a951fd73
    4 0 9 8 0 16395:a951fd73
    4 0 9 8 1 16395:a951fd73
    4 0 9 8 2 16395:a951fd73
    4 0 9 8 3 16395:a951fd73
    4 0 9 8 4 16395:a951fd73
    4 0 9 9 0 16395:a951fd73
    4 0 9 9 1 16395:a951fd73
    4 0 9 9 2 16395:a951fd73
    4 0 9 9 3 16395:a951fd73
    4 0 9 9 4 16395:a951fd73
    4 0 15 1 0 16395:da6f4d7c
    4 0 15 1 1 16395:da6f4d7c
    4 0 15 1 2 16395:da6f4d7c
    4 0 15 1 3 16395:da6f4d7c
    4 0 15 1 4 16395:da6f4d7c
    4 0 15 8 0 16395:da6f4d7c
    4 0 15 8 1 16395:da6f4d7c
    4 0 15 8 2 16395:da6f4d7c
    4 0 15 8 3 16395:da6f4d7c
    4 0 15 8 4 16395:da6f4d7c
    4 0 15 9 0 16395:da6f4d7c
    4 0 15 9 1 16395:da6f4d7c
    4 0 15 9 2 16395:da6f4d7c
    4 0 15 9 3 16395:da6f4d7c
    4 0 15 9 4 16395:da6f4d7c
    4 1 -15 1 0 6689:0506fae2
    4 1 -15 1 1 6689:0506fae2
    4 1 -15 1 2 10563:d0738582
    4 1 -15 1 3 6673:3132937e
    4 1 -15 1 4 6689:0506fae2
    4 1 -15 8 0 6373:32ff110e
    4 1 -15 8 1 6373:32ff110e
    4 1 -15 8 2 14525:5475a77d
    4 1 -15 8 3 6352:6a525fa1
    4 1 -15 8 4 6651:bb6c1e0d
    4 1 -15 9 0 6373:32ff110e
    4 1 -15 9 1 6373:32ff110e
    4 1 -15 9 2 14524:5bd30dd6
    4 1 -15 9 3 6352:6a525fa1
    4 1 -15 9 4 6651:bb6c1e0d
    4 1 -9 1 0 6686:34df9b2f
    4 1 -9 1 1 6686:34df9b2f
    4 1 -9 1 2 10563:d0738582
    4 1 -9 1 3 6673:3132937e
    4 1 -9 1 4 6686:34df9b2f
    4 1 -9 8 0 6369:5761d34b
    4 1 -9 8 1 6369:5761d34b
    4 1 -9 8 2 14525:5475a77d
    4 1 -9 8 3 6352:6a525fa1
    4 1 -9 8 4 6654:644a8d7d
    4 1 -9 9 0 6369:5761d34b
    4 1 -9 9 1 6369:5761d34b
    4 1 -9 9 2 14524:5bd30dd6
    4 1 -9 9 3 6352:6a525fa1
    4 1 -9 9 4 6654:644a8d7d
    4 1 9 1 0 6692:b35a9997
    4 1 9 1 1 6692:b35a9997
    4 1 9 1 2 10569:0f93fd4d
    4 1 9 1 3 6679:48c5cff5
    4 1 9 1 4 6692:b35a9997
    4 1 9 8 0 6375:4fef247d
    4 1 9 8 1 6375:4fef247d
    4 1 9 8 2 14531:97de20ff
    4 1 9 8 3 6358:7fb0631f
    4 1 9 8 4 6660:9bc894cd
    4 1 9 9 0 6375:4fef247d
    4 1 9 9 1 6375:4fef247d
    4 1 9 9 2 14530:a169ff6d
    4 1 9 9 3 6358:7fb0631f
    4 1 9 9 4 6660:9bc894cd
    4 1 15 1 0 6695:579c5412
    4 1 15 1 1 6695:579c5412
    4 1 15 1 2 10569:92e75a30
    4 1 15 1 3 6679:2e2a2ccf
    4 1 15 1 4 6695:579c5412
    4 1 15 8 0 6379:e827fb89
    4 1 15 8 1 6379:e827fb89
    4 1 15 8 2 14531:11876f20
    4 1 15 8 3 6358:9e35ad6f
    4 1 15 8 4 6657:31cab2a3
    4 1 15 9 0 6379:e827fb89
    4 1 15 9 1 6379:e827fb89
    4 1 15 9 2 14530:2bf4f4bd
    4 1 15 9 3 6358:9e35ad6f
    4 1 15 9 4 6657:31cab2a3
    4 2 -15 1 0 6689:0506fae2
    4 2 -15 1 1 6689:0506fae2
    4 2 -15 1 2 10563:d0738582
    4 2 -15 1 3 6673:3132937e
    4 2 -15 1 4 6689:0506fae2
    4 2 -15 8 0 6373:32ff110e
    4 2 -15 8 1 6373:32ff110e
    4 2 -15 8 2 14525:5475a77d
    4 2 -15 8 3 6352:6a525fa1
    4 2 -15 8 4 6651:bb6c1e0d
    4 2 -15 9 0 6373:32ff110e
    4 2 -15 9 1 6373:32ff110e
    4 2 -15 9 2 14524:5bd30dd6
    4 2 -15 9 3 6352:6a525fa1
    4 2 -15 9 4 6651:bb6c1e0d
    4 2 -9 1 0 6686:34df9b2f
    4 2 -9 1 1 6686:34df9b2f
    4 2 -9 1 2 10563:d0738582
    4 2 -9 1 3 6673:3132937e
    4 2 -9 1 4 6686:34df9b2f
    4 2 -9 8 0 6369:5761d34b
    4 2 -9 8 1 6369:5761d34b
    4 2 -9 8 2 14525:5475a77d
    4 2 -9 8 3 6352:6a525fa1
    4 2 -9 8 4 6654:644a8d7d
    4 2 -9 9 0 6369:5761d34b
    4 2 -9 9 1 6369:5761d34b
    4 2 -9 9 2 14524:5bd30dd6
    4 2 -9 9 3 6352:6a525fa1
    4 2 -9 9 4 6654:644a8d7d
    4 2 9 1 0 6692:c530e4df
    4 2 9 1 1 6692:c530e4df
    4 2 9 1 2 10569:0f93fd4d
    4 2 9 1 3 6679:48c5cff5
    4 2 9 1 4 6692:b35a9997
    4 2 9 8 0 6375:cf808d65
    4 2 9 8 1 6375:cf808d65
    4 2 9 8 2 14531:97de20ff
    4 2 9 8 3 6358:7fb0631f
    4 2 9 8 4 6660:9bc894cd
    4 2 9 9 0 6375:cf808d65
    4 2 9 9 1 6375:cf808d65
    4 2 9 9 2 14530:a169ff6d
    4 2 9 9 3 6358:7fb0631f
    4 2 9 9 4 6660:9bc894cd
    4 2 15 1 0 6695:d2b96070
    4 2 15 1 1 6695:d2b96070
    4 2 15 1 2 10569:92e75a30
    4 2 15 1 3 6679:2e2a2ccf
    4 2 15 1 4 6695:579c5412
    4 2 15 8 0 6379:addeceb5
    4 2 15 8 1 6379:addeceb5
    4 2 15 8 2 14531:11876f20
    4 2 15 8 3 6358:9e35ad6f
    4 2 15 8 4 6657:31cab2a3
    4 2 15 9 0 6379:addeceb5
    4 2 15 9 1 6379:addeceb5
    4 2 15 9 2 14530:2bf4f4bd
    4 2 15 9 3 6358:9e35ad6f
    4 2 15 9 4 6657:31cab2a3
    4 3 -15 1 0 6689:bf843ea2
    4 3 -15 1 1 6689:bf843ea2
    4 3 -15 1 2 10563:d0738582
    4 3 -15 1 3 6673:3132937e
    4 3 -15 1 4 6689:bf843ea2
    4 3 -15 8 0 6373:32ff110e
    4 3 -15 8 1 6373:32ff110e
    4 3 -15 8 2 14525:5475a77d
    4 3 -15 8 3 6352:6a525fa1
    4 3 -15 8 4 6651:bb6c1e0d
    4 3 -15 9 0 6373:32ff110e
    4 3 -15 9 1 6373:32ff110e
    4 3 -15 9 2 14524:5bd30dd6
    4 3 -15 9 3 6352:6a525fa1
    4 3 -15 9 4 6651:bb6c1e0d
    4 3 -9 1 0 6686:34df9b2f
    4 3 -9 1 1 6686:34df9b2f
    4 3 -9 1 2 10563:d0738582
    4 3 -9 1 3 6673:3132937e
    4 3 -9 1 4 6686:34df9b2f
    4 3 -9 8 0 6369:5761d34b
    4 3 -9 8 1 6369:5761d34b
    4 3 -9 8 2 14525:5475a77d
    4 3 -9 8 3 6352:6a525fa1
    4 3 -9 8 4 6654:644a8d7d
    4 3 -9 9 0 6369:5761d34b
    4 3 -9 9 1 6369:5761d34b
    4 3 -9 9 2 14524:5bd30dd6
    4 3 -9 9 3 6352:6a525fa1
    4 3 -9 9 4 6654:644a8d7d
    4 3 9 1 0 6692:c530e4df
    4 3 9 1 1 6692:c530e4df
    4 3 9 1 2 10569:0f93fd4d
    4 3 9 1 3 6679:48c5cff5
    4 3 9 1 4 6692:b35a9997
    4 3 9 8 0 6375:cf808d65
    4 3 9 8 1 6375:cf808d65
    4 3 9 8 2 14531:97de20ff
    4 3 9 8 3 6358:7fb0631f
    4 3 9 8 4 6660:9bc894cd
    4 3 9 9 0 6375:cf808d65
    4 3 9 9 1 6375:cf808d65
    4 3 9 9 2 14530:a169ff6d
    4 3 9 9 3 6358:7fb0631f
    4 3 9 9 4 6660:9bc894cd
    4 3 15 1 0 6695:fd0aa68a
    4 3 15 1 1 6695:fd0aa68a
    4 3 15 1 2 10569:92e75a30
    4 3 15 1 3 6679:2e2a2ccf
    4 3 15 1 4 6695:782f92e8
    4 3 15 8 0 6379:addeceb5
    4 3 15 8 1 6379:addeceb5
    4 3 15 8 2 14531:11876f20
    4 3 15 8 3 6358:9e35ad6f
    4 3 15 8 4 6657:31cab2a3
    4 3 15 9 0 6379:addeceb5
    4 3 15 9 1 6379:addeceb5
    4 3 15 9 2 14530:2bf4f4bd
    4 3 15 9 3 6358:9e35ad6f
    4 3 15 9 4 6657:31cab2a3
    4 4 -15 1 0 6674:e370cb07
    4 4 -15 1 1 6676:dbd0a539
    4 4 -15 1 2 10563:d0738582
    4 4 -15 1 3 6673:3132937e
    4 4 -15 1 4 6674:e370cb07
    4 4 -15 8 0 6353:ec502303
    4 4 -15 8 1 6352:435663cc
    4 4 -15 8 2 14525:5475a77d
    4 4 -15 8 3 6352:6a525fa1
    4 4 -15 8 4 6636:db988062
    4 4 -15 9 0 6353:ec502303
    4 4 -15 9 1 6352:435663cc
    4 4 -15 9 2 14524:5bd30dd6
    4 4 -15 9 3 6352:6a525fa1
    4 4 -15 9 4 6636:db988062
    4 4 -9 1 0 6674:e370cb07
    4 4 -9 1 1 6676:dbd0a539
    4 4 -9 1 2 10563:d0738582
    4 4 -9 1 3 6673:3132937e
    4 4 -9 1 4 6674:e370cb07
    4 4 -9 8 0 6353:ec502303
    4 4 -9 8 1 6352:435663cc
    4 4 -9 8 2 14525:5475a77d
    4 4 -9 8 3 6352:6a525fa1
    4 4 -9 8 4 6636:db988062
    4 4 -9 9 0 6353:ec502303
    4 4 -9 9 1 6352:435663cc
    4 4 -9 9 2 14524:5bd30dd6
    4 4 -9 9 3 6352:6a525fa1
    4 4 -9 9 4 6636:db988062
    4 4 9 1 0 6680:6417aa1e
    4 4 9 1 1 6682:7b974328
    4 4 9 1 2 10569:0f93fd4d
    4 4 9 1 3 6679:48c5cff5
    4 4 9 1 4 6680:2e244c15
    4 4 9 8 0 6359:411479d5
    4 4 9 8 1 6358:df6254ae
    4 4 9 8 2 14531:97de20ff
    4 4 9 8 3 6358:7fb0631f
    4 4 9 8 4 6642:dd60ec5e
    4 4 9 9 0 6359:411479d5
    4 4 9 9 1 6358:df6254ae
    4 4 9 9 2 14530:a169ff6d
    4 4 9 9 3 6358:7fb0631f
    4 4 9 9 4 6642:dd60ec5e
    4 4 15 1 0 6680:f9cd1b9f
    4 4 15 1 1 6682:e569f4e2
    4 4 15 1 2 10569:92e75a30
    4 4 15 1 3 6679:2e2a2ccf
    4 4 15 1 4 6680:e84e7a44
    4 4 15 8 0 6359:963f9775
    4 4 15 8 1 6358:f95e1a4f
    4 4 15 8 2 14531:11876f20
    4 4 15 8 3 6358:9e35ad6f
    4 4 15 8 4 6642:fa455662
    4 4 15 9 0 6359:963f9775
    4 4 15 9 1 6358:f95e1a4f
    4 4 15 9 2 14530:2bf4f4bd
    4 4 15 9 3 6358:9e35ad6f
    4 4 15 9 4 6642:fa455662
    4 5 -15 1 0 6674:e370cb07
    4 5 -15 1 1 6676:dbd0a539
    4 5 -15 1 2 10563:d0738582
    4 5 -15 1 3 6673:3132937e
    4 5 -15 1 4 6674:e370cb07
    4 5 -15 8 0 6353:ec502303
    4 5 -15 8 1 6352:435663cc
    4 5 -15 8 2 14525:5475a77d
    4 5 -15 8 3 6352:6a525fa1
    4 5 -15 8 4 6636:db988062
    4 5 -15 9 0 6353:ec502303
    4 5 -15 9 1 6352:435663cc
    4 5 -15 9 2 14524:5bd30dd6
    4 5 -15 9 3 6352:6a525fa1
    4 5 -15 9 4 6636:db988062
    4 5 -9 1 0 6674:e370cb07
    4 5 -9 1 1 6676:dbd0a539
    4 5 -9 1 2 10563:d0738582
    4 5 -9 1 3 6673:3132937e
    4 5 -9 1 4 6674:e370cb07
    4 5 -9 8 0 6353:ec502303
    4 5 -9 8 1 6352:435663cc
    4 5 -9 8 2 14525:5475a77d
    4 5 -9 8 3 6352:6a525fa1
    4 5 -9 8 4 6636:db988062
    4 5 -9 9 0 6353:ec502303
    4 5 -9 9 1 6352:435663cc
    4 5 -9 9 2 14524:5bd30dd6
    4 5 -9 9 3 6352:6a525fa1
    4 5 -9 9 4 6636:db988062
    4 5 9 1 0 6680:6417aa1e
    4 5 9 1 1 6682:7b974328
    4 5 9 1 2 10569:0f93fd4d
    4 5 9 1 3 6679:48c5cff5
    4 5 9 1 4 6680:2e244c15
    4 5 9 8 0 6359:411479d5
    4 5 9 8 1 6358:df6254ae
    4 5 9 8 2 14531:97de20ff
    4 5 9 8 3 6358:7fb0631f
    4 5 9 8 4 6642:dd60ec5e
    4 5 9 9 0 6359:411479d5
    4 5 9 9 1 6358:df6254ae
    4 5 9 9 2 14530:a169ff6d
    4 5 9 9 3 6358:7fb0631f
    4 5 9 9 4 6642:dd60ec5e
    4 5 15 1 0 6680:f9cd1b9f
    4 5 15 1 1 6682:e569f4e2
    4 5 15 1 2 10569:92e75a30
    4 5 15 1 3 6679:2e2a2ccf
    4 5 15 1 4 6680:e84e7a44
    4 5 15 8 0 6359:963f9775
    4 5 15 8 1 6358:f95e1a4f
    4 5 15 8 2 14531:11876f20
    4 5 15 8 3 6358:9e35ad6f
    4 5 15 8 4 6642:fa455662
    4 5 15 9 0 6359:963f9775
    4 5 15 9 1 6358:f95e1a4f
    4 5 15 9 2 14530:2bf4f4bd
    4 5 15 9 3 6358:9e35ad6f
    4 5 15 9 4 6642:fa455662
    4 6 -15 1 0 6674:e370cb07
    4 6 -15 1 1 6676:dbd0a539
    4 6 -15 1 2 10563:d0738582
    4 6 -15 1 3 6673:3132937e
    4 6 -15 1 4 6674:e370cb07
    4 6 -15 8 0 6353:ec502303
    4 6 -15 8 1 6352:435663cc
    4 6 -15 8 2 14525:5475a77d
    4 6 -15 8 3 6352:6a525fa1
    4 6 -15 8 4 6636:db988062
    4 6 -15 9 0 6353:ec502303
    4 6 -15 9 1 6352:435663cc
    4 6 -15 9 2 14524:5bd30dd6
    4 6 -15 9 3 6352:6a525fa1
    4 6 -15 9 4 6636:db988062
    4 6 -9 1 0 6674:e370cb07
    4 6 -9 1 1 6676:dbd0a539
    4 6 -9 1 2 10563:d0738582
    4 6 -9 1 3 6673:3132937e
    4 6 -9 1 4 6674:e370cb07
    4 6 -9 8 0 6353:ec502303
    4 6 -9 8 1 6352:435663cc
    4 6 -9 8 2 14525:5475a77d
    4 6 -9 8 3 6352:6a525fa1
    4 6 -9 8 4 6636:db988062
    4 6 -9 9 0 6353:ec502303
    4 6 -9 9 1 6352:435663cc
    4 6 -9 9 2 14524:5bd30dd6
    4 6 -9 9 3 6352:6a525fa1
    4 6 -9 9 4 6636:db988062
    4 6 9 1 0 6680:2306e278
    4 6 9 1 1 6682:303701ba
    4 6 9 1 2 10569:0f93fd4d
    4 6 9 1 3 6679:48c5cff5
    4 6 9 1 4 6680:2e244c15
    4 6 9 8 0 6359:a35319f4
    4 6 9 8 1 6358:20786167
    4 6 9 8 2 14531:97de20ff
    4 6 9 8 3 6358:7fb0631f
    4 6 9 8 4 6642:dd60ec5e
    4 6 9 9 0 6359:a35319f4
    4 6 9 9 1 6358:20786167
    4 6 9 9 2 14530:a169ff6d
    4 6 9 9 3 6358:7fb0631f
    4 6 9 9 4 6642:dd60ec5e
    4 6 15 1 0 6680:bedc53f9
    4 6 15 1 1 6682:aec9b670
    4 6 15 1 2 10569:92e75a30
    4 6 15 1 3 6679:2e2a2ccf
    4 6 15 1 4 6680:e84e7a44
    4 6 15 8 0 6359:7478f754
    4 6 15 8 1 6358:06442f86
    4 6 15 8 2 14531:11876f20
    4 6 15 8 3 6358:9e35ad6f
    4 6 15 8 4 6642:fa455662
    4 6 15 9 0 6359:7478f754
    4 6 15 9 1 6358:06442f86
    4 6 15 9 2 14530:2bf4f4bd
    4 6 15 9 3 6358:9e35ad6f
    4 6 15 9 4 6642:fa455662
    4 7 -15 1 0 6674:e370cb07
    4 7 -15 1 1 6676:dbd0a539
    4 7 -15 1 2 10563:d0738582
    4 7 -15 1 3 6673:3132937e
    4 7 -15 1 4 6674:e370cb07
    4 7 -15 8 0 6353:ec502303
    4 7 -15 8 1 6352:435663cc
    4 7 -15 8 2 14525:5475a77d
    4 7 -15 8 3 6352:6a525fa1
    4 7 -15 8 4 6636:db988062
    4 7 -15 9 0 6353:ec502303
    4 7 -15 9 1 6352:435663cc
    4 7 -15 9 2 14524:5bd30dd6
    4 7 -15 9 3 6352:6a525fa1
    4 7 -15 9 4 6636:db988062
    4 7 -9 1 0 6674:e370cb07
    4 7 -9 1 1 6676:dbd0a539
    4 7 -9 1 2 10563:d0738582
    4 7 -9 1 3 6673:3132937e
    4 7 -9 1 4 6674:e370cb07
    4 7 -9 8 0 6353:ec502303
    4 7 -9 8 1 6352:435663cc
    4 7 -9 8 2 14525:5475a77d
    4 7 -9 8 3 6352:6a525fa1
    4 7 -9 8 4 6636:db988062
    4 7 -9 9 0 6353:ec502303
    4 7 -9 9 1 6352:435663cc
    4 7 -9 9 2 14524:5bd30dd6
    4 7 -9 9 3 6352:6a525fa1
    4 7 -9 9 4 6636:db988062
    4 7 9 1 0 6680:c82f366e
    4 7 9 1 1 6682:ad114989
    4 7 9 1 2 10569:0f93fd4d
    4 7 9 1 3 6679:48c5cff5
    4 7 9 1 4 6680:2e244c15
    4 7 9 8 0 6359:c01be09b
    4 7 9 8 1 6358:6eedd77a
    4 7 9 8 2 14531:97de20ff
    4 7 9 8 3 6358:7fb0631f
    4 7 9 8 4 6642:dd60ec5e
    4 7 9 9 0 6359:c01be09b
    4 7 9 9 1 6358:6eedd77a
    4 7 9 9 2 14530:a169ff6d
    4 7 9 9 3 6358:7fb0631f
    4 7 9 9 4 6642:dd60ec5e
    4 7 15 1 0 6680:55f587ef
    4 7 15 1 1 6682:33effe43
    4 7 15 1 2 10569:92e75a30
    4 7 15 1 3 6679:2e2a2ccf
    4 7 15 1 4 6680:e84e7a44
    4 7 15 8 0 6359:17300e3b
    4 7 15 8 1 6358:48d1999b
    4 7 15 8 2 14531:11876f20
    4 7 15 8 3 6358:9e35ad6f
    4 7 15 8 4 6642:fa455662
    4 7 15 9 0 6359:17300e3b
    4 7 15 9 1 6358:48d1999b
    4 7 15 9 2 14530:2bf4f4bd
    4 7 15 9 3 6358:9e35ad6f
    4 7 15 9 4 6642:fa455662
    4 8 -15 1 0 6674:e370cb07
    4 8 -15 1 1 6676:dbd0a539
    4 8 -15 1 2 10563:d0738582
    4 8 -15 1 3 6673:3132937e
    4 8 -15 1 4 6674:e370cb07
    4 8 -15 8 0 6353:ec502303
    4 8 -15 8 1 6352:435663cc
    4 8 -15 8 2 14525:5475a77d
    4 8 -15 8 3 6352:6a525fa1
    4 8 -15 8 4 6636:db988062
    4 8 -15 9 0 6353:ec502303
    4 8 -15 9 1 6352:435663cc
    4 8 -15 9 2 14524:5bd30dd6
    4 8 -15 9 3 6352:6a525fa1
    4 8 -15 9 4 6636:db988062
    4 8 -9 1 0 6674:e370cb07
    4 8 -9 1 1 6676:dbd0a539
    4 8 -9 1 2 10563:d0738582
    4 8 -9 1 3 6673:3132937e
    4 8 -9 1 4 6674:e370cb07
    4 8 -9 8 0 6353:ec502303
    4 8 -9 8 1 6352:435663cc
    4 8 -9 8 2 14525:5475a77d
    4 8 -9 8 3 6352:6a525fa1
    4 8 -9 8 4 6636:db988062
    4 8 -9 9 0 6353:ec502303
    4 8 -9 9 1 6352:435663cc
    4 8 -9 9 2 14524:5bd30dd6
    4 8 -9 9 3 6352:6a525fa1
    4 8 -9 9 4 6636:db988062
    4 8 9 1 0 6680:c82f366e
    4 8 9 1 1 6682:ad114989
    4 8 9 1 2 10569:0f93fd4d
    4 8 9 1 3 6679:48c5cff5
    4 8 9 1 4 6680:2e244c15
    4 8 9 8 0 6359:c01be09b
    4 8 9 8 1 6358:6eedd77a
    4 8 9 8 2 14531:97de20ff
    4 8 9 8 3 6358:7fb0631f
    4 8 9 8 4 6642:dd60ec5e
    4 8 9 9 0 6359:c01be09b
    4 8 9 9 1 6358:6eedd77a
    4 8 9 9 2 14530:a169ff6d
    4 8 9 9 3 6358:7fb0631f
    4 8 9 9 4 6642:dd60ec5e
    4 8 15 1 0 6680:55f587ef
    4 8 15 1 1 6682:33effe43
    4 8 15 1 2 10569:92e75a30
    4 8 15 1 3 6679:2e2a2ccf
    4 8 15 1 4 6680:e84e7a44
    4 8 15 8 0 6359:17300e3b
    4 8 15 8 1 6358:48d1999b
    4 8 15 8 2 14531:11876f20
    4 8 15 8 3 6358:9e35ad6f
    4 8 15 8 4 6642:fa455662
    4 8 15 9 0 6359:17300e3b
    4 8 15 9 1 6358:48d1999b
    4 8 15 9 2 14530:2bf4f4bd
    4 8 15 9 3 6358:9e35ad6f
    4 8 15 9 4 6642:fa455662
    4 9 -15 1 0 6674:e370cb07
    4 9 -15 1 1 6676:dbd0a539
    4 9 -15 1 2 10563:d0738582
    4 9 -15 1 3 6673:3132937e
    4 9 -15 1 4 6674:e370cb07
    4 9 -15 8 0 6353:ec502303
    4 9 -15 8 1 6352:435663cc
    4 9 -15 8 2 14525:5475a77d
    4 9 -15 8 3 6352:6a525fa1
    4 9 -15 8 4 6636:db988062
    4 9 -15 9 0 6353:ec502303
    4 9 -15 9 1 6352:435663cc
    4 9 -15 9 2 14524:5bd30dd6
    4 9 -15 9 3 6352:6a525fa1
    4 9 -15 9 4 6636:db988062
    4 9 -9 1 0 6674:e370cb07
    4 9 -9 1 1 6676:dbd0a539
    4 9 -9 1 2 10563:d0738582
    4 9 -9 1 3 6673:3132937e
    4 9 -9 1 4 6674:e370cb07
    4 9 -9 8 0 6353:ec502303
    4 9 -9 8 1 6352:435663cc
    4 9 -9 8 2 14525:5475a77d
    4 9 -9 8 3 6352:6a525fa1
    4 9 -9 8 4 6636:db988062
    4 9 -9 9 0 6353:ec502303
    4 9 -9 9 1 6352:435663cc
    4 9 -9 9 2 14524:5bd30dd6
    4 9 -9 9 3 6352:6a525fa1
    4 9 -9 9 4 6636:db988062
    4 9 9 1 0 6680:c82f366e
    4 9 9 1 1 6682:ad114989
    4 9 9 1 2 10569:0f93fd4d
    4 9 9 1 3 6679:48c5cff5
    4 9 9 1 4 6680:2e244c15
    4 9 9 8 0 6359:c01be09b
    4 9 9 8 1 6358:6eedd77a
    4 9 9 8 2 14531:97de20ff
    4 9 9 8 3 6358:7fb0631f
    4 9 9 8 4 6642:dd60ec5e
    4 9 9 9 0 6359:c01be09b
    4 9 9 9 1 6358:6eedd77a
    4 9 9 9 2 14530:a169ff6d
    4 9 9 9 3 6358:7fb0631f
    4 9 9 9 4 6642:dd60ec5e
    4 9 15 1 0 6680:55f587ef
    4 9 15 1 1 6682:33effe43
    4 9 15 1 2 10569:92e75a30
    4 9 15 1 3 6679:2e2a2ccf
    4 9 15 1 4 6680:e84e7a44
    4 9 15 8 0 6359:17300e3b
    4 9 15 8 1 6358:48d1999b
    4 9 15 8 2 14531:11876f20
    4 9 15 8 3 6358:9e35ad6f
    4 9 15 8 4 6642:fa455662
    4 9 15 9 0 6359:17300e3b
    4 9 15 9 1 6358:48d1999b
    4 9 15 9 2 14530:2bf4f4bd
    4 9 15 9 3 6358:9e35ad6f
    4 9 15 9 4 6642:fa455662
    ";

    /// The gzip half of the wide grid: 825 rows at `windowBits = 31`, same axes,
    /// same digest encoding, same provenance and same caveats as [`BI_GRID`].
    ///
    /// Gated on the `gzip` feature because `deflate_init2` rejects a gzip request
    /// without it, exactly as a C zlib built without `GZIP` does. Byte 9 of every
    /// member was normalised to [`NORMALISED_OS_CODE`] before digesting, so unlike
    /// [`BI_VECTORS_GZIP`] these rows are platform-neutral — see
    /// [`normalise_gzip_os`].
    #[cfg(feature = "gzip")]
    const BI_GRID_GZIP: &str = "\
    0 -1 31 1 0 51:750957c2
    0 -1 31 1 1 51:750957c2
    0 -1 31 1 2 3552:0798d5f7
    0 -1 31 1 3 50:0a5e7c5b
    0 -1 31 1 4 126:39047c77
    0 -1 31 8 0 51:750957c2
    0 -1 31 8 1 51:750957c2
    0 -1 31 8 2 2080:714133dd
    0 -1 31 8 3 50:0a5e7c5b
    0 -1 31 8 4 126:39047c77
    0 -1 31 9 0 51:750957c2
    0 -1 31 9 1 51:750957c2
    0 -1 31 9 2 2078:ebbe3859
    0 -1 31 9 3 50:0a5e7c5b
    0 -1 31 9 4 126:39047c77
    0 0 31 1 0 16407:3b27e601
    0 0 31 1 1 16407:3b27e601
    0 0 31 1 2 16407:3b27e601
    0 0 31 1 3 16407:3b27e601
    0 0 31 1 4 16407:3b27e601
    0 0 31 8 0 16407:3b27e601
    0 0 31 8 1 16407:3b27e601
    0 0 31 8 2 16407:3b27e601
    0 0 31 8 3 16407:3b27e601
    0 0 31 8 4 16407:3b27e601
    0 0 31 9 0 16407:3b27e601
    0 0 31 9 1 16407:3b27e601
    0 0 31 9 2 16407:3b27e601
    0 0 31 9 3 16407:3b27e601
    0 0 31 9 4 16407:3b27e601
    0 1 31 1 0 107:475b2081
    0 1 31 1 1 107:475b2081
    0 1 31 1 2 3552:0798d5f7
    0 1 31 1 3 50:0a5e7c5b
    0 1 31 1 4 181:6be548f4
    0 1 31 8 0 107:475b2081
    0 1 31 8 1 107:475b2081
    0 1 31 8 2 2080:714133dd
    0 1 31 8 3 50:0a5e7c5b
    0 1 31 8 4 181:6be548f4
    0 1 31 9 0 107:475b2081
    0 1 31 9 1 107:475b2081
    0 1 31 9 2 2078:ebbe3859
    0 1 31 9 3 50:0a5e7c5b
    0 1 31 9 4 181:6be548f4
    0 2 31 1 0 107:242d2b0b
    0 2 31 1 1 107:242d2b0b
    0 2 31 1 2 3552:0798d5f7
    0 2 31 1 3 50:0a5e7c5b
    0 2 31 1 4 181:6be548f4
    0 2 31 8 0 107:242d2b0b
    0 2 31 8 1 107:242d2b0b
    0 2 31 8 2 2080:714133dd
    0 2 31 8 3 50:0a5e7c5b
    0 2 31 8 4 181:6be548f4
    0 2 31 9 0 107:242d2b0b
    0 2 31 9 1 107:242d2b0b
    0 2 31 9 2 2078:ebbe3859
    0 2 31 9 3 50:0a5e7c5b
    0 2 31 9 4 181:6be548f4
    0 3 31 1 0 107:242d2b0b
    0 3 31 1 1 107:242d2b0b
    0 3 31 1 2 3552:0798d5f7
    0 3 31 1 3 50:0a5e7c5b
    0 3 31 1 4 181:6be548f4
    0 3 31 8 0 107:242d2b0b
    0 3 31 8 1 107:242d2b0b
    0 3 31 8 2 2080:714133dd
    0 3 31 8 3 50:0a5e7c5b
    0 3 31 8 4 181:6be548f4
    0 3 31 9 0 107:242d2b0b
    0 3 31 9 1 107:242d2b0b
    0 3 31 9 2 2078:ebbe3859
    0 3 31 9 3 50:0a5e7c5b
    0 3 31 9 4 181:6be548f4
    0 4 31 1 0 51:750957c2
    0 4 31 1 1 51:750957c2
    0 4 31 1 2 3552:0798d5f7
    0 4 31 1 3 50:0a5e7c5b
    0 4 31 1 4 126:39047c77
    0 4 31 8 0 51:750957c2
    0 4 31 8 1 51:750957c2
    0 4 31 8 2 2080:714133dd
    0 4 31 8 3 50:0a5e7c5b
    0 4 31 8 4 126:39047c77
    0 4 31 9 0 51:750957c2
    0 4 31 9 1 51:750957c2
    0 4 31 9 2 2078:ebbe3859
    0 4 31 9 3 50:0a5e7c5b
    0 4 31 9 4 126:39047c77
    0 5 31 1 0 51:750957c2
    0 5 31 1 1 51:750957c2
    0 5 31 1 2 3552:0798d5f7
    0 5 31 1 3 50:0a5e7c5b
    0 5 31 1 4 126:39047c77
    0 5 31 8 0 51:750957c2
    0 5 31 8 1 51:750957c2
    0 5 31 8 2 2080:714133dd
    0 5 31 8 3 50:0a5e7c5b
    0 5 31 8 4 126:39047c77
    0 5 31 9 0 51:750957c2
    0 5 31 9 1 51:750957c2
    0 5 31 9 2 2078:ebbe3859
    0 5 31 9 3 50:0a5e7c5b
    0 5 31 9 4 126:39047c77
    0 6 31 1 0 51:750957c2
    0 6 31 1 1 51:750957c2
    0 6 31 1 2 3552:0798d5f7
    0 6 31 1 3 50:0a5e7c5b
    0 6 31 1 4 126:39047c77
    0 6 31 8 0 51:750957c2
    0 6 31 8 1 51:750957c2
    0 6 31 8 2 2080:714133dd
    0 6 31 8 3 50:0a5e7c5b
    0 6 31 8 4 126:39047c77
    0 6 31 9 0 51:750957c2
    0 6 31 9 1 51:750957c2
    0 6 31 9 2 2078:ebbe3859
    0 6 31 9 3 50:0a5e7c5b
    0 6 31 9 4 126:39047c77
    0 7 31 1 0 51:750957c2
    0 7 31 1 1 51:750957c2
    0 7 31 1 2 3552:0798d5f7
    0 7 31 1 3 50:0a5e7c5b
    0 7 31 1 4 126:39047c77
    0 7 31 8 0 51:750957c2
    0 7 31 8 1 51:750957c2
    0 7 31 8 2 2080:714133dd
    0 7 31 8 3 50:0a5e7c5b
    0 7 31 8 4 126:39047c77
    0 7 31 9 0 51:750957c2
    0 7 31 9 1 51:750957c2
    0 7 31 9 2 2078:ebbe3859
    0 7 31 9 3 50:0a5e7c5b
    0 7 31 9 4 126:39047c77
    0 8 31 1 0 51:750957c2
    0 8 31 1 1 51:750957c2
    0 8 31 1 2 3552:0798d5f7
    0 8 31 1 3 50:0a5e7c5b
    0 8 31 1 4 126:39047c77
    0 8 31 8 0 51:750957c2
    0 8 31 8 1 51:750957c2
    0 8 31 8 2 2080:714133dd
    0 8 31 8 3 50:0a5e7c5b
    0 8 31 8 4 126:39047c77
    0 8 31 9 0 51:750957c2
    0 8 31 9 1 51:750957c2
    0 8 31 9 2 2078:ebbe3859
    0 8 31 9 3 50:0a5e7c5b
    0 8 31 9 4 126:39047c77
    0 9 31 1 0 51:fb2eba34
    0 9 31 1 1 51:fb2eba34
    0 9 31 1 2 3552:90031bc9
    0 9 31 1 3 50:7a296abd
    0 9 31 1 4 126:9433f411
    0 9 31 8 0 51:fb2eba34
    0 9 31 8 1 51:fb2eba34
    0 9 31 8 2 2080:ce1cda84
    0 9 31 8 3 50:7a296abd
    0 9 31 8 4 126:9433f411
    0 9 31 9 0 51:fb2eba34
    0 9 31 9 1 51:fb2eba34
    0 9 31 9 2 2078:6bfcfe46
    0 9 31 9 3 50:7a296abd
    0 9 31 9 4 126:9433f411
    1 -1 31 1 0 17047:15ae2d34
    1 -1 31 1 1 17049:68696ce0
    1 -1 31 1 2 17049:d5d07c85
    1 -1 31 1 3 17049:d5d07c85
    1 -1 31 1 4 17047:2cf1c8b2
    1 -1 31 8 0 16407:6ef3df96
    1 -1 31 8 1 16409:57c04829
    1 -1 31 8 2 16409:36c58928
    1 -1 31 8 3 16409:36c58928
    1 -1 31 8 4 16407:3b27e601
    1 -1 31 9 0 16407:6ef3df96
    1 -1 31 9 1 16407:6ef3df96
    1 -1 31 9 2 16407:3b27e601
    1 -1 31 9 3 16407:3b27e601
    1 -1 31 9 4 16407:3b27e601
    1 0 31 1 0 16407:3b27e601
    1 0 31 1 1 16407:3b27e601
    1 0 31 1 2 16407:3b27e601
    1 0 31 1 3 16407:3b27e601
    1 0 31 1 4 16407:3b27e601
    1 0 31 8 0 16407:3b27e601
    1 0 31 8 1 16407:3b27e601
    1 0 31 8 2 16407:3b27e601
    1 0 31 8 3 16407:3b27e601
    1 0 31 8 4 16407:3b27e601
    1 0 31 9 0 16407:3b27e601
    1 0 31 9 1 16407:3b27e601
    1 0 31 9 2 16407:3b27e601
    1 0 31 9 3 16407:3b27e601
    1 0 31 9 4 16407:3b27e601
    1 1 31 1 0 17049:d5d07c85
    1 1 31 1 1 17049:d5d07c85
    1 1 31 1 2 17049:d5d07c85
    1 1 31 1 3 17049:d5d07c85
    1 1 31 1 4 17049:d5d07c85
    1 1 31 8 0 16407:3b27e601
    1 1 31 8 1 16407:3b27e601
    1 1 31 8 2 16409:36c58928
    1 1 31 8 3 16409:36c58928
    1 1 31 8 4 16407:3b27e601
    1 1 31 9 0 16407:3b27e601
    1 1 31 9 1 16407:3b27e601
    1 1 31 9 2 16407:3b27e601
    1 1 31 9 3 16407:3b27e601
    1 1 31 9 4 16407:3b27e601
    1 2 31 1 0 17047:8bb4ad75
    1 2 31 1 1 17047:8bb4ad75
    1 2 31 1 2 17049:d5d07c85
    1 2 31 1 3 17049:d5d07c85
    1 2 31 1 4 17047:b2eb48f3
    1 2 31 8 0 16407:6ef3df96
    1 2 31 8 1 16407:6ef3df96
    1 2 31 8 2 16409:36c58928
    1 2 31 8 3 16409:36c58928
    1 2 31 8 4 16407:3b27e601
    1 2 31 9 0 16407:6ef3df96
    1 2 31 9 1 16407:6ef3df96
    1 2 31 9 2 16407:3b27e601
    1 2 31 9 3 16407:3b27e601
    1 2 31 9 4 16407:3b27e601
    1 3 31 1 0 17047:6bc144de
    1 3 31 1 1 17047:6bc144de
    1 3 31 1 2 17049:d5d07c85
    1 3 31 1 3 17049:d5d07c85
    1 3 31 1 4 17047:529ea158
    1 3 31 8 0 16407:6ef3df96
    1 3 31 8 1 16407:6ef3df96
    1 3 31 8 2 16409:36c58928
    1 3 31 8 3 16409:36c58928
    1 3 31 8 4 16407:3b27e601
    1 3 31 9 0 16407:6ef3df96
    1 3 31 9 1 16407:6ef3df96
    1 3 31 9 2 16407:3b27e601
    1 3 31 9 3 16407:3b27e601
    1 3 31 9 4 16407:3b27e601
    1 4 31 1 0 17047:f92be3b8
    1 4 31 1 1 17049:68696ce0
    1 4 31 1 2 17049:d5d07c85
    1 4 31 1 3 17049:d5d07c85
    1 4 31 1 4 17047:c074063e
    1 4 31 8 0 16407:6ef3df96
    1 4 31 8 1 16409:57c04829
    1 4 31 8 2 16409:36c58928
    1 4 31 8 3 16409:36c58928
    1 4 31 8 4 16407:3b27e601
    1 4 31 9 0 16407:6ef3df96
    1 4 31 9 1 16407:6ef3df96
    1 4 31 9 2 16407:3b27e601
    1 4 31 9 3 16407:3b27e601
    1 4 31 9 4 16407:3b27e601
    1 5 31 1 0 17047:15ae2d34
    1 5 31 1 1 17049:68696ce0
    1 5 31 1 2 17049:d5d07c85
    1 5 31 1 3 17049:d5d07c85
    1 5 31 1 4 17047:2cf1c8b2
    1 5 31 8 0 16407:6ef3df96
    1 5 31 8 1 16409:57c04829
    1 5 31 8 2 16409:36c58928
    1 5 31 8 3 16409:36c58928
    1 5 31 8 4 16407:3b27e601
    1 5 31 9 0 16407:6ef3df96
    1 5 31 9 1 16407:6ef3df96
    1 5 31 9 2 16407:3b27e601
    1 5 31 9 3 16407:3b27e601
    1 5 31 9 4 16407:3b27e601
    1 6 31 1 0 17047:15ae2d34
    1 6 31 1 1 17049:68696ce0
    1 6 31 1 2 17049:d5d07c85
    1 6 31 1 3 17049:d5d07c85
    1 6 31 1 4 17047:2cf1c8b2
    1 6 31 8 0 16407:6ef3df96
    1 6 31 8 1 16409:57c04829
    1 6 31 8 2 16409:36c58928
    1 6 31 8 3 16409:36c58928
    1 6 31 8 4 16407:3b27e601
    1 6 31 9 0 16407:6ef3df96
    1 6 31 9 1 16407:6ef3df96
    1 6 31 9 2 16407:3b27e601
    1 6 31 9 3 16407:3b27e601
    1 6 31 9 4 16407:3b27e601
    1 7 31 1 0 17047:15ae2d34
    1 7 31 1 1 17049:68696ce0
    1 7 31 1 2 17049:d5d07c85
    1 7 31 1 3 17049:d5d07c85
    1 7 31 1 4 17047:2cf1c8b2
    1 7 31 8 0 16407:6ef3df96
    1 7 31 8 1 16409:57c04829
    1 7 31 8 2 16409:36c58928
    1 7 31 8 3 16409:36c58928
    1 7 31 8 4 16407:3b27e601
    1 7 31 9 0 16407:6ef3df96
    1 7 31 9 1 16407:6ef3df96
    1 7 31 9 2 16407:3b27e601
    1 7 31 9 3 16407:3b27e601
    1 7 31 9 4 16407:3b27e601
    1 8 31 1 0 17047:15ae2d34
    1 8 31 1 1 17049:68696ce0
    1 8 31 1 2 17049:d5d07c85
    1 8 31 1 3 17049:d5d07c85
    1 8 31 1 4 17047:2cf1c8b2
    1 8 31 8 0 16407:6ef3df96
    1 8 31 8 1 16409:57c04829
    1 8 31 8 2 16409:36c58928
    1 8 31 8 3 16409:36c58928
    1 8 31 8 4 16407:3b27e601
    1 8 31 9 0 16407:6ef3df96
    1 8 31 9 1 16407:6ef3df96
    1 8 31 9 2 16407:3b27e601
    1 8 31 9 3 16407:3b27e601
    1 8 31 9 4 16407:3b27e601
    1 9 31 1 0 17047:0901dff7
    1 9 31 1 1 17049:db0d67f2
    1 9 31 1 2 17049:db0d67f2
    1 9 31 1 3 17049:db0d67f2
    1 9 31 1 4 17047:0901dff7
    1 9 31 8 0 16407:a9a1407d
    1 9 31 8 1 16409:8afa2b89
    1 9 31 8 2 16409:8afa2b89
    1 9 31 8 3 16409:8afa2b89
    1 9 31 8 4 16407:a9a1407d
    1 9 31 9 0 16407:a9a1407d
    1 9 31 9 1 16407:a9a1407d
    1 9 31 9 2 16407:a9a1407d
    1 9 31 9 3 16407:a9a1407d
    1 9 31 9 4 16407:a9a1407d
    2 -1 31 1 0 508:565e2609
    2 -1 31 1 1 550:4147a63a
    2 -1 31 1 2 13178:1f5f3e71
    2 -1 31 1 3 13178:1f5f3e71
    2 -1 31 1 4 611:b87999f6
    2 -1 31 8 0 465:34e09086
    2 -1 31 8 1 473:4651b824
    2 -1 31 8 2 9717:b722e764
    2 -1 31 8 3 9717:b722e764
    2 -1 31 8 4 607:744c1956
    2 -1 31 9 0 465:34e09086
    2 -1 31 9 1 473:4651b824
    2 -1 31 9 2 9716:8b5ef8e2
    2 -1 31 9 3 9716:8b5ef8e2
    2 -1 31 9 4 607:744c1956
    2 0 31 1 0 16407:3b27e601
    2 0 31 1 1 16407:3b27e601
    2 0 31 1 2 16407:3b27e601
    2 0 31 1 3 16407:3b27e601
    2 0 31 1 4 16407:3b27e601
    2 0 31 8 0 16407:3b27e601
    2 0 31 8 1 16407:3b27e601
    2 0 31 8 2 16407:3b27e601
    2 0 31 8 3 16407:3b27e601
    2 0 31 8 4 16407:3b27e601
    2 0 31 9 0 16407:3b27e601
    2 0 31 9 1 16407:3b27e601
    2 0 31 9 2 16407:3b27e601
    2 0 31 9 3 16407:3b27e601
    2 0 31 9 4 16407:3b27e601
    2 1 31 1 0 612:0e216ed4
    2 1 31 1 1 612:0e216ed4
    2 1 31 1 2 13178:1f5f3e71
    2 1 31 1 3 13178:1f5f3e71
    2 1 31 1 4 713:45382f18
    2 1 31 8 0 557:63b2514c
    2 1 31 8 1 557:63b2514c
    2 1 31 8 2 9717:b722e764
    2 1 31 8 3 9717:b722e764
    2 1 31 8 4 683:4d169543
    2 1 31 9 0 557:63b2514c
    2 1 31 9 1 557:63b2514c
    2 1 31 9 2 9716:8b5ef8e2
    2 1 31 9 3 9716:8b5ef8e2
    2 1 31 9 4 683:4d169543
    2 2 31 1 0 575:5e736fe8
    2 2 31 1 1 575:5e736fe8
    2 2 31 1 2 13178:1f5f3e71
    2 2 31 1 3 13178:1f5f3e71
    2 2 31 1 4 672:914e27be
    2 2 31 8 0 536:e4d26cd5
    2 2 31 8 1 536:e4d26cd5
    2 2 31 8 2 9717:b722e764
    2 2 31 8 3 9717:b722e764
    2 2 31 8 4 655:2734fb2b
    2 2 31 9 0 536:e4d26cd5
    2 2 31 9 1 536:e4d26cd5
    2 2 31 9 2 9716:8b5ef8e2
    2 2 31 9 3 9716:8b5ef8e2
    2 2 31 9 4 655:2734fb2b
    2 3 31 1 0 560:23987360
    2 3 31 1 1 560:23987360
    2 3 31 1 2 13178:1f5f3e71
    2 3 31 1 3 13178:1f5f3e71
    2 3 31 1 4 654:5a48a411
    2 3 31 8 0 528:0e159488
    2 3 31 8 1 528:0e159488
    2 3 31 8 2 9717:b722e764
    2 3 31 8 3 9717:b722e764
    2 3 31 8 4 650:19efe799
    2 3 31 9 0 528:0e159488
    2 3 31 9 1 528:0e159488
    2 3 31 9 2 9716:8b5ef8e2
    2 3 31 9 3 9716:8b5ef8e2
    2 3 31 9 4 650:19efe799
    2 4 31 1 0 516:52c7b400
    2 4 31 1 1 559:c77b7716
    2 4 31 1 2 13178:1f5f3e71
    2 4 31 1 3 13178:1f5f3e71
    2 4 31 1 4 617:ea9bf3e6
    2 4 31 8 0 473:4f59249b
    2 4 31 8 1 479:8a8839c0
    2 4 31 8 2 9717:b722e764
    2 4 31 8 3 9717:b722e764
    2 4 31 8 4 612:9a51c684
    2 4 31 9 0 473:4f59249b
    2 4 31 9 1 479:8a8839c0
    2 4 31 9 2 9716:8b5ef8e2
    2 4 31 9 3 9716:8b5ef8e2
    2 4 31 9 4 612:9a51c684
    2 5 31 1 0 508:565e2609
    2 5 31 1 1 550:4147a63a
    2 5 31 1 2 13178:1f5f3e71
    2 5 31 1 3 13178:1f5f3e71
    2 5 31 1 4 611:b87999f6
    2 5 31 8 0 465:34e09086
    2 5 31 8 1 473:4651b824
    2 5 31 8 2 9717:b722e764
    2 5 31 8 3 9717:b722e764
    2 5 31 8 4 607:744c1956
    2 5 31 9 0 465:34e09086
    2 5 31 9 1 473:4651b824
    2 5 31 9 2 9716:8b5ef8e2
    2 5 31 9 3 9716:8b5ef8e2
    2 5 31 9 4 607:744c1956
    2 6 31 1 0 508:565e2609
    2 6 31 1 1 550:4147a63a
    2 6 31 1 2 13178:1f5f3e71
    2 6 31 1 3 13178:1f5f3e71
    2 6 31 1 4 611:b87999f6
    2 6 31 8 0 465:34e09086
    2 6 31 8 1 473:4651b824
    2 6 31 8 2 9717:b722e764
    2 6 31 8 3 9717:b722e764
    2 6 31 8 4 607:744c1956
    2 6 31 9 0 465:34e09086
    2 6 31 9 1 473:4651b824
    2 6 31 9 2 9716:8b5ef8e2
    2 6 31 9 3 9716:8b5ef8e2
    2 6 31 9 4 607:744c1956
    2 7 31 1 0 508:565e2609
    2 7 31 1 1 550:4147a63a
    2 7 31 1 2 13178:1f5f3e71
    2 7 31 1 3 13178:1f5f3e71
    2 7 31 1 4 611:b87999f6
    2 7 31 8 0 465:34e09086
    2 7 31 8 1 473:4651b824
    2 7 31 8 2 9717:b722e764
    2 7 31 8 3 9717:b722e764
    2 7 31 8 4 607:744c1956
    2 7 31 9 0 465:34e09086
    2 7 31 9 1 473:4651b824
    2 7 31 9 2 9716:8b5ef8e2
    2 7 31 9 3 9716:8b5ef8e2
    2 7 31 9 4 607:744c1956
    2 8 31 1 0 508:565e2609
    2 8 31 1 1 550:4147a63a
    2 8 31 1 2 13178:1f5f3e71
    2 8 31 1 3 13178:1f5f3e71
    2 8 31 1 4 611:b87999f6
    2 8 31 8 0 465:34e09086
    2 8 31 8 1 473:4651b824
    2 8 31 8 2 9717:b722e764
    2 8 31 8 3 9717:b722e764
    2 8 31 8 4 607:744c1956
    2 8 31 9 0 465:34e09086
    2 8 31 9 1 473:4651b824
    2 8 31 9 2 9716:8b5ef8e2
    2 8 31 9 3 9716:8b5ef8e2
    2 8 31 9 4 607:744c1956
    2 9 31 1 0 508:47d8e430
    2 9 31 1 1 550:3ff465ab
    2 9 31 1 2 13178:f51abcd7
    2 9 31 1 3 13178:f51abcd7
    2 9 31 1 4 611:81ba0835
    2 9 31 8 0 465:39577b50
    2 9 31 8 1 473:78dbd22e
    2 9 31 8 2 9717:39a1a30e
    2 9 31 8 3 9717:39a1a30e
    2 9 31 8 4 607:caee4d65
    2 9 31 9 0 465:39577b50
    2 9 31 9 1 473:78dbd22e
    2 9 31 9 2 9716:b6f56b7f
    2 9 31 9 3 9716:b6f56b7f
    2 9 31 9 4 607:caee4d65
    3 -1 31 1 0 360:8e8989ca
    3 -1 31 1 1 360:8e8989ca
    3 -1 31 1 2 16969:9071dd48
    3 -1 31 1 3 16969:9071dd48
    3 -1 31 1 4 433:0cd36d7d
    3 -1 31 8 0 420:66ffa1d2
    3 -1 31 8 1 420:66ffa1d2
    3 -1 31 8 2 16409:b56cf8d6
    3 -1 31 8 3 16409:b56cf8d6
    3 -1 31 8 4 441:ad35fa77
    3 -1 31 9 0 420:66ffa1d2
    3 -1 31 9 1 420:66ffa1d2
    3 -1 31 9 2 16407:3b27e601
    3 -1 31 9 3 16407:3b27e601
    3 -1 31 9 4 441:ad35fa77
    3 0 31 1 0 16407:3b27e601
    3 0 31 1 1 16407:3b27e601
    3 0 31 1 2 16407:3b27e601
    3 0 31 1 3 16407:3b27e601
    3 0 31 1 4 16407:3b27e601
    3 0 31 8 0 16407:3b27e601
    3 0 31 8 1 16407:3b27e601
    3 0 31 8 2 16407:3b27e601
    3 0 31 8 3 16407:3b27e601
    3 0 31 8 4 16407:3b27e601
    3 0 31 9 0 16407:3b27e601
    3 0 31 9 1 16407:3b27e601
    3 0 31 9 2 16407:3b27e601
    3 0 31 9 3 16407:3b27e601
    3 0 31 9 4 16407:3b27e601
    3 1 31 1 0 421:a738704c
    3 1 31 1 1 421:a738704c
    3 1 31 1 2 16969:9071dd48
    3 1 31 1 3 16969:9071dd48
    3 1 31 1 4 473:aeb28a5f
    3 1 31 8 0 480:f48d8ada
    3 1 31 8 1 480:f48d8ada
    3 1 31 8 2 16409:b56cf8d6
    3 1 31 8 3 16409:b56cf8d6
    3 1 31 8 4 480:f48d8ada
    3 1 31 9 0 480:f48d8ada
    3 1 31 9 1 480:f48d8ada
    3 1 31 9 2 16407:3b27e601
    3 1 31 9 3 16407:3b27e601
    3 1 31 9 4 480:f48d8ada
    3 2 31 1 0 421:fd8af1d7
    3 2 31 1 1 421:fd8af1d7
    3 2 31 1 2 16969:9071dd48
    3 2 31 1 3 16969:9071dd48
    3 2 31 1 4 473:aeb28a5f
    3 2 31 8 0 480:e286253d
    3 2 31 8 1 480:e286253d
    3 2 31 8 2 16409:b56cf8d6
    3 2 31 8 3 16409:b56cf8d6
    3 2 31 8 4 480:f48d8ada
    3 2 31 9 0 480:e286253d
    3 2 31 9 1 480:e286253d
    3 2 31 9 2 16407:3b27e601
    3 2 31 9 3 16407:3b27e601
    3 2 31 9 4 480:f48d8ada
    3 3 31 1 0 421:fd8af1d7
    3 3 31 1 1 421:fd8af1d7
    3 3 31 1 2 16969:9071dd48
    3 3 31 1 3 16969:9071dd48
    3 3 31 1 4 473:aeb28a5f
    3 3 31 8 0 480:e286253d
    3 3 31 8 1 480:e286253d
    3 3 31 8 2 16409:b56cf8d6
    3 3 31 8 3 16409:b56cf8d6
    3 3 31 8 4 480:f48d8ada
    3 3 31 9 0 480:e286253d
    3 3 31 9 1 480:e286253d
    3 3 31 9 2 16407:3b27e601
    3 3 31 9 3 16407:3b27e601
    3 3 31 9 4 480:f48d8ada
    3 4 31 1 0 360:8e8989ca
    3 4 31 1 1 360:8e8989ca
    3 4 31 1 2 16969:9071dd48
    3 4 31 1 3 16969:9071dd48
    3 4 31 1 4 433:0cd36d7d
    3 4 31 8 0 420:66ffa1d2
    3 4 31 8 1 420:66ffa1d2
    3 4 31 8 2 16409:b56cf8d6
    3 4 31 8 3 16409:b56cf8d6
    3 4 31 8 4 441:ad35fa77
    3 4 31 9 0 420:66ffa1d2
    3 4 31 9 1 420:66ffa1d2
    3 4 31 9 2 16407:3b27e601
    3 4 31 9 3 16407:3b27e601
    3 4 31 9 4 441:ad35fa77
    3 5 31 1 0 360:8e8989ca
    3 5 31 1 1 360:8e8989ca
    3 5 31 1 2 16969:9071dd48
    3 5 31 1 3 16969:9071dd48
    3 5 31 1 4 433:0cd36d7d
    3 5 31 8 0 420:66ffa1d2
    3 5 31 8 1 420:66ffa1d2
    3 5 31 8 2 16409:b56cf8d6
    3 5 31 8 3 16409:b56cf8d6
    3 5 31 8 4 441:ad35fa77
    3 5 31 9 0 420:66ffa1d2
    3 5 31 9 1 420:66ffa1d2
    3 5 31 9 2 16407:3b27e601
    3 5 31 9 3 16407:3b27e601
    3 5 31 9 4 441:ad35fa77
    3 6 31 1 0 360:8e8989ca
    3 6 31 1 1 360:8e8989ca
    3 6 31 1 2 16969:9071dd48
    3 6 31 1 3 16969:9071dd48
    3 6 31 1 4 433:0cd36d7d
    3 6 31 8 0 420:66ffa1d2
    3 6 31 8 1 420:66ffa1d2
    3 6 31 8 2 16409:b56cf8d6
    3 6 31 8 3 16409:b56cf8d6
    3 6 31 8 4 441:ad35fa77
    3 6 31 9 0 420:66ffa1d2
    3 6 31 9 1 420:66ffa1d2
    3 6 31 9 2 16407:3b27e601
    3 6 31 9 3 16407:3b27e601
    3 6 31 9 4 441:ad35fa77
    3 7 31 1 0 360:8e8989ca
    3 7 31 1 1 360:8e8989ca
    3 7 31 1 2 16969:9071dd48
    3 7 31 1 3 16969:9071dd48
    3 7 31 1 4 433:0cd36d7d
    3 7 31 8 0 420:66ffa1d2
    3 7 31 8 1 420:66ffa1d2
    3 7 31 8 2 16409:b56cf8d6
    3 7 31 8 3 16409:b56cf8d6
    3 7 31 8 4 441:ad35fa77
    3 7 31 9 0 420:66ffa1d2
    3 7 31 9 1 420:66ffa1d2
    3 7 31 9 2 16407:3b27e601
    3 7 31 9 3 16407:3b27e601
    3 7 31 9 4 441:ad35fa77
    3 8 31 1 0 360:8e8989ca
    3 8 31 1 1 360:8e8989ca
    3 8 31 1 2 16969:9071dd48
    3 8 31 1 3 16969:9071dd48
    3 8 31 1 4 433:0cd36d7d
    3 8 31 8 0 420:66ffa1d2
    3 8 31 8 1 420:66ffa1d2
    3 8 31 8 2 16409:b56cf8d6
    3 8 31 8 3 16409:b56cf8d6
    3 8 31 8 4 441:ad35fa77
    3 8 31 9 0 420:66ffa1d2
    3 8 31 9 1 420:66ffa1d2
    3 8 31 9 2 16407:3b27e601
    3 8 31 9 3 16407:3b27e601
    3 8 31 9 4 441:ad35fa77
    3 9 31 1 0 360:dfcfa84d
    3 9 31 1 1 360:dfcfa84d
    3 9 31 1 2 16969:84960185
    3 9 31 1 3 16969:84960185
    3 9 31 1 4 433:44181cac
    3 9 31 8 0 420:3d50e1ad
    3 9 31 8 1 420:3d50e1ad
    3 9 31 8 2 16409:09535a77
    3 9 31 8 3 16409:09535a77
    3 9 31 8 4 441:3e31aa95
    3 9 31 9 0 420:3d50e1ad
    3 9 31 9 1 420:3d50e1ad
    3 9 31 9 2 16407:a9a1407d
    3 9 31 9 3 16407:a9a1407d
    3 9 31 9 4 441:3e31aa95
    4 -1 31 1 0 6692:8060e709
    4 -1 31 1 1 6694:3ff7b0e2
    4 -1 31 1 2 10581:bfb300fe
    4 -1 31 1 3 6691:3c2cfeae
    4 -1 31 1 4 6692:5fa78022
    4 -1 31 8 0 6371:2b62441b
    4 -1 31 8 1 6370:3f1aca63
    4 -1 31 8 2 14543:ceeacfe6
    4 -1 31 8 3 6370:332be42d
    4 -1 31 8 4 6654:f63ee163
    4 -1 31 9 0 6371:2b62441b
    4 -1 31 9 1 6370:3f1aca63
    4 -1 31 9 2 14542:a2a14264
    4 -1 31 9 3 6370:332be42d
    4 -1 31 9 4 6654:f63ee163
    4 0 31 1 0 16407:3b27e601
    4 0 31 1 1 16407:3b27e601
    4 0 31 1 2 16407:3b27e601
    4 0 31 1 3 16407:3b27e601
    4 0 31 1 4 16407:3b27e601
    4 0 31 8 0 16407:3b27e601
    4 0 31 8 1 16407:3b27e601
    4 0 31 8 2 16407:3b27e601
    4 0 31 8 3 16407:3b27e601
    4 0 31 8 4 16407:3b27e601
    4 0 31 9 0 16407:3b27e601
    4 0 31 9 1 16407:3b27e601
    4 0 31 9 2 16407:3b27e601
    4 0 31 9 3 16407:3b27e601
    4 0 31 9 4 16407:3b27e601
    4 1 31 1 0 6707:3903476b
    4 1 31 1 1 6707:3903476b
    4 1 31 1 2 10581:bfb300fe
    4 1 31 1 3 6691:3c2cfeae
    4 1 31 1 4 6707:3903476b
    4 1 31 8 0 6391:21ed557a
    4 1 31 8 1 6391:21ed557a
    4 1 31 8 2 14543:ceeacfe6
    4 1 31 8 3 6370:332be42d
    4 1 31 8 4 6669:430d8685
    4 1 31 9 0 6391:21ed557a
    4 1 31 9 1 6391:21ed557a
    4 1 31 9 2 14542:a2a14264
    4 1 31 9 3 6370:332be42d
    4 1 31 9 4 6669:430d8685
    4 2 31 1 0 6707:c89ee29b
    4 2 31 1 1 6707:c89ee29b
    4 2 31 1 2 10581:bfb300fe
    4 2 31 1 3 6691:3c2cfeae
    4 2 31 1 4 6707:3903476b
    4 2 31 8 0 6391:bbc6d9e3
    4 2 31 8 1 6391:bbc6d9e3
    4 2 31 8 2 14543:ceeacfe6
    4 2 31 8 3 6370:332be42d
    4 2 31 8 4 6669:430d8685
    4 2 31 9 0 6391:bbc6d9e3
    4 2 31 9 1 6391:bbc6d9e3
    4 2 31 9 2 14542:a2a14264
    4 2 31 9 3 6370:332be42d
    4 2 31 9 4 6669:430d8685
    4 3 31 1 0 6707:d05e3e88
    4 3 31 1 1 6707:d05e3e88
    4 3 31 1 2 10581:bfb300fe
    4 3 31 1 3 6691:3c2cfeae
    4 3 31 1 4 6707:21c39b78
    4 3 31 8 0 6391:bbc6d9e3
    4 3 31 8 1 6391:bbc6d9e3
    4 3 31 8 2 14543:ceeacfe6
    4 3 31 8 3 6370:332be42d
    4 3 31 8 4 6669:430d8685
    4 3 31 9 0 6391:bbc6d9e3
    4 3 31 9 1 6391:bbc6d9e3
    4 3 31 9 2 14542:a2a14264
    4 3 31 9 3 6370:332be42d
    4 3 31 9 4 6669:430d8685
    4 4 31 1 0 6692:8060e709
    4 4 31 1 1 6694:3ff7b0e2
    4 4 31 1 2 10581:bfb300fe
    4 4 31 1 3 6691:3c2cfeae
    4 4 31 1 4 6692:5fa78022
    4 4 31 8 0 6371:2b62441b
    4 4 31 8 1 6370:3f1aca63
    4 4 31 8 2 14543:ceeacfe6
    4 4 31 8 3 6370:332be42d
    4 4 31 8 4 6654:f63ee163
    4 4 31 9 0 6371:2b62441b
    4 4 31 9 1 6370:3f1aca63
    4 4 31 9 2 14542:a2a14264
    4 4 31 9 3 6370:332be42d
    4 4 31 9 4 6654:f63ee163
    4 5 31 1 0 6692:8060e709
    4 5 31 1 1 6694:3ff7b0e2
    4 5 31 1 2 10581:bfb300fe
    4 5 31 1 3 6691:3c2cfeae
    4 5 31 1 4 6692:5fa78022
    4 5 31 8 0 6371:2b62441b
    4 5 31 8 1 6370:3f1aca63
    4 5 31 8 2 14543:ceeacfe6
    4 5 31 8 3 6370:332be42d
    4 5 31 8 4 6654:f63ee163
    4 5 31 9 0 6371:2b62441b
    4 5 31 9 1 6370:3f1aca63
    4 5 31 9 2 14542:a2a14264
    4 5 31 9 3 6370:332be42d
    4 5 31 9 4 6654:f63ee163
    4 6 31 1 0 6692:8060e709
    4 6 31 1 1 6694:3ff7b0e2
    4 6 31 1 2 10581:bfb300fe
    4 6 31 1 3 6691:3c2cfeae
    4 6 31 1 4 6692:5fa78022
    4 6 31 8 0 6371:2b62441b
    4 6 31 8 1 6370:3f1aca63
    4 6 31 8 2 14543:ceeacfe6
    4 6 31 8 3 6370:332be42d
    4 6 31 8 4 6654:f63ee163
    4 6 31 9 0 6371:2b62441b
    4 6 31 9 1 6370:3f1aca63
    4 6 31 9 2 14542:a2a14264
    4 6 31 9 3 6370:332be42d
    4 6 31 9 4 6654:f63ee163
    4 7 31 1 0 6692:8060e709
    4 7 31 1 1 6694:3ff7b0e2
    4 7 31 1 2 10581:bfb300fe
    4 7 31 1 3 6691:3c2cfeae
    4 7 31 1 4 6692:5fa78022
    4 7 31 8 0 6371:2b62441b
    4 7 31 8 1 6370:3f1aca63
    4 7 31 8 2 14543:ceeacfe6
    4 7 31 8 3 6370:332be42d
    4 7 31 8 4 6654:f63ee163
    4 7 31 9 0 6371:2b62441b
    4 7 31 9 1 6370:3f1aca63
    4 7 31 9 2 14542:a2a14264
    4 7 31 9 3 6370:332be42d
    4 7 31 9 4 6654:f63ee163
    4 8 31 1 0 6692:8060e709
    4 8 31 1 1 6694:3ff7b0e2
    4 8 31 1 2 10581:bfb300fe
    4 8 31 1 3 6691:3c2cfeae
    4 8 31 1 4 6692:5fa78022
    4 8 31 8 0 6371:2b62441b
    4 8 31 8 1 6370:3f1aca63
    4 8 31 8 2 14543:ceeacfe6
    4 8 31 8 3 6370:332be42d
    4 8 31 8 4 6654:f63ee163
    4 8 31 9 0 6371:2b62441b
    4 8 31 9 1 6370:3f1aca63
    4 8 31 9 2 14542:a2a14264
    4 8 31 9 3 6370:332be42d
    4 8 31 9 4 6654:f63ee163
    4 9 31 1 0 6692:023bd7bc
    4 9 31 1 1 6694:809c58e8
    4 9 31 1 2 10581:541e2749
    4 9 31 1 3 6691:c8606254
    4 9 31 1 4 6692:023bd7bc
    4 9 31 8 0 6371:dbbfc1f1
    4 9 31 8 1 6370:ed0c64f3
    4 9 31 8 2 14543:bdd676ec
    4 9 31 8 3 6370:9e6111dc
    4 9 31 8 4 6654:5db8c021
    4 9 31 9 0 6371:dbbfc1f1
    4 9 31 9 1 6370:ed0c64f3
    4 9 31 9 2 14542:420ecbe3
    4 9 31 9 3 6370:9e6111dc
    4 9 31 9 4 6654:5db8c021
    ";

    /// Full literal reference bytes at the `memLevel` corners, which the
    /// five-field tier-1 tables ([`BI_VECTORS`], [`BI_VECTORS_GZIP`]) cannot
    /// express because they carry no `memLevel` column: 24 rows covering
    /// `memLevel` 1 and 9, `windowBits` 15 / −15 / 9 / −9, levels 1 and 9, and
    /// strategies 0 / 2 / 3 over the short [`BI_INPUTS`] corpora 2, 3 and 4.
    ///
    /// Line format: `<input-index> <level> <windowBits> <memLevel> <strategy-id>
    /// <hex-expected>` — the six-field, `memLevel`-aware form, distinguishing it
    /// from [`BI_VECTORS`]'s five-field rows.
    ///
    /// These rows exist because [`BI_GRID`] proves only length and CRC-32. Short
    /// corpora keep the hex compact while still anchoring the *literal* output at
    /// a non-default `memLevel`. Six of the 24 differ from their `memLevel = 8`
    /// counterpart, all of them `memLevel = 1` rows: with `lit_bufsize` down to
    /// 128 symbols, a 128-byte incompressible input is split into two blocks
    /// instead of one, so these rows pin genuinely different block boundaries and
    /// not just a different header. Same provenance and same regeneration recipe
    /// as [`BI_GRID`].
    const BI_EXTREMES: &str = "\
    4 9 15 1 0 78da007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b030061334005
    4 9 15 9 0 78da0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 9 -15 1 0 007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b0300
    4 9 -15 9 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 9 9 1 0 18d3007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b030061334005
    4 9 9 9 0 18d30180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 9 -9 1 0 007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b0300
    4 9 -9 9 0 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    4 1 15 1 2 7801007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b030061334005
    4 1 15 9 2 78010180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f18661334005
    4 1 -9 1 2 007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b0300
    4 1 -9 9 2 0180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186
    2 1 9 1 0 18190bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 9 9 1 0 18d30bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 1 9 9 0 18190bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 9 9 9 0 18d30bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc50050fc5c22
    2 1 -9 1 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 9 -9 1 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 1 -9 9 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    2 9 -9 9 0 0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500
    3 9 9 1 0 18d35dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e70de0781
    3 9 9 1 3 18195dc1b701c0200000207b37f9ff5b7720202221a3a0a2a163606261e3e0e2c38f0770de0781
    3 9 -9 1 0 5dc1c901c010000030a56e65ff6dfb962484db838884171905150d1d03130b1f360e7e
    3 9 -9 1 3 5dc1b701c0200000207b37f9ff5b7720202221a3a0a2a163606261e3e0e2c38f07
    ";

    /// The gzip counterpart of [`BI_EXTREMES`]: 12 full-hex rows at
    /// `windowBits = 31`, `memLevel` 1 and 9, levels 1 and 9, strategies 0 and 2,
    /// over the short [`BI_INPUTS`] corpora 1, 2 and 4.
    ///
    /// Byte 9 was normalised to [`NORMALISED_OS_CODE`] when baked and is
    /// normalised again before comparison, so — unlike [`BI_VECTORS_GZIP`] —
    /// these rows assert exact bytes on every platform. See
    /// [`normalise_gzip_os`].
    #[cfg(feature = "gzip")]
    const BI_EXTREMES_GZIP: &str = "\
    4 1 31 1 0 1f8b0800000000000403007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b0300f920fde580000000
    4 9 31 1 0 1f8b0800000000000203007f0080ff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f16b0300f920fde580000000
    4 1 31 9 0 1f8b08000000000004030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    4 9 31 9 0 1f8b08000000000002030180007fff9af1008aa16d4009af18aed8b47fe55826740d5b18a0333d822483ef7c795f816b2f319474121bc4f695d4dc7a57581d900aed323378e83cbac64e8e8b46fd0606f7971cdeca95a6f736740589a2c13e1bbe5be5f84cff654babb7efbab0a330a2b3443f7b5e24e3066861e68dff1fee5b65f54dc2eb10869b795eeea503f186f920fde580000000
    2 9 31 1 0 1f8b08000000000002030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 9 31 1 2 1f8b080000000000020304c1810140201440c155de04a66981e427895f51617a772608a56d6e67ae3a4ebc3ec476e40bed52b98390ecf7b2e83a618250dae676e6aae3c4eb436c47bed02e953b08c97e2f8bae132608a56d6e67ae3a4ebc3ec476e40bed52b98390ec4f101c1800044201145ce54dd03416109f94fa9494a677373e56dd0d9313ee7a2c019bb52536edf81aaf82be92799c70cee363d5dd3039e1aec712b0595b62d38eaff12ae82b99c709e73c3e56dd0d9313ee7a2c019bb52536edf81aaf82bef2af480100aff02d1b00010000
    2 9 31 9 0 1f8b08000000000002030bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29840c3fc500aff02d1b00010000
    2 9 31 9 2 1f8b080000000000020305c1890180200c00b1556e02a761019f2a3e50a916d1e94d42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac61d85a3ff5e265d3a42148aafe3ce60fa64666d6c9ece0bad62dc5138faef65d2a52344a1f83aee0ca64f66d6c6e6e9bcd02ac60faff02d1b00010000
    1 1 31 1 0 1f8b0800000000000403cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 9 31 1 0 1f8b0800000000000203cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 1 31 9 0 1f8b0800000000000403cb48cdc9c9d751c800518a009bdc9ab30d000000
    1 9 31 9 0 1f8b0800000000000203cb48cdc9c9d751c800518a009bdc9ab30d000000
    ";

    /// Split a `<length>:<crc32-hex>` digest field into its two components.
    fn parse_digest(field: &str, line: &str) -> (usize, u32) {
        let Some((len, crc)) = field.split_once(':') else {
            panic!("digest field must be `<length>:<crc32-hex>` in grid line: {line:?}");
        };
        (
            len.parse().expect("decimal compressed length"),
            u32::from_str_radix(crc, 16).expect("lowercase hex CRC-32"),
        )
    }

    /// Assert every row of `table` whose `<windowBits>` column equals
    /// `window_bits` reproduces the reference digest, and return how many rows
    /// were checked.
    ///
    /// Mirrors [`check_all`]'s parsing discipline exactly — blank lines skipped,
    /// whitespace-delimited fields, a hard failure on a trailing field, and a
    /// final guard that the table was not silently empty — and additionally
    /// asserts **both** halves of the digest, so a length collision cannot mask a
    /// CRC divergence or vice versa.
    ///
    /// The framing filter exists for parallelism, not selectivity: the plain grid
    /// is 3,300 compressions of 16 KiB, which libtest runs far faster as four
    /// concurrent per-framing tests. Nothing is skipped —
    /// [`grid_table_is_structurally_complete`] proves that every row of the table
    /// belongs to one of the [`GRID_PLAIN_FRAMINGS`].
    fn check_grid(table: &str, window_bits: i32) -> usize {
        let inputs = bi_grid_inputs();
        let mut count = 0usize;
        for line in table.lines().filter(|l| !l.trim().is_empty()) {
            let mut f = line.split_whitespace();
            let idx: usize = f.next().unwrap().parse().unwrap();
            let level: i32 = f.next().unwrap().parse().unwrap();
            let wbits: i32 = f.next().unwrap().parse().unwrap();
            let mem_level: i32 = f.next().unwrap().parse().unwrap();
            let strat: u8 = f.next().unwrap().parse().unwrap();
            let (expected_len, expected_crc) = parse_digest(f.next().unwrap(), line);
            assert!(
                f.next().is_none(),
                "unexpected trailing field in grid line: {line:?}"
            );
            if wbits != window_bits {
                continue;
            }
            let input = &inputs[idx];
            let produced = normalise_gzip_os(
                zlib_rs_deflate_full(input, level, wbits, mem_level, strat_from_id(strat)),
                wbits,
            );
            assert_eq!(
                produced.len(),
                expected_len,
                "compressed-length divergence from C zlib 1.3.2.1-motley: corpus #{idx} \
                 (len {}), level {level}, windowBits {wbits}, memLevel {mem_level}, \
                 strategy {:?}",
                input.len(),
                strat_from_id(strat),
            );
            assert_eq!(
                crc32(0, &produced),
                expected_crc,
                "compressed-CRC-32 divergence from C zlib 1.3.2.1-motley: corpus #{idx} \
                 (len {}), level {level}, windowBits {wbits}, memLevel {mem_level}, \
                 strategy {:?}",
                input.len(),
                strat_from_id(strat),
            );
            count += 1;
        }
        assert!(
            count > 0,
            "no oracle vectors matched windowBits {window_bits} in the grid table"
        );
        count
    }

    /// Count the rows of `table` whose framing column equals `window_bits`,
    /// without compressing anything — the parse-only half of the structural audit.
    fn framing_row_count(table: &str, window_bits: i32) -> usize {
        table
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| {
                l.split_whitespace()
                    .nth(2)
                    .and_then(|w| w.parse::<i32>().ok())
                    == Some(window_bits)
            })
            .count()
    }

    /// Assert every row of a six-field, `memLevel`-aware full-hex table
    /// reproduces the reference bytes exactly.
    ///
    /// Line format: `<input-index> <level> <windowBits> <memLevel> <strategy-id>
    /// <hex-expected>`. Same discipline as [`check_all`], extended with the
    /// `memLevel` column and with gzip `OS`-byte normalisation
    /// ([`normalise_gzip_os`]) so the gzip rows assert exact bytes on every
    /// platform.
    fn check_exact_with_mem_level(table: &str) {
        let inputs = parse_inputs();
        let mut count = 0usize;
        for line in table.lines().filter(|l| !l.trim().is_empty()) {
            let mut f = line.split_whitespace();
            let idx: usize = f.next().unwrap().parse().unwrap();
            let level: i32 = f.next().unwrap().parse().unwrap();
            let wbits: i32 = f.next().unwrap().parse().unwrap();
            let mem_level: i32 = f.next().unwrap().parse().unwrap();
            let strat: u8 = f.next().unwrap().parse().unwrap();
            let expected = unhex(f.next().unwrap());
            assert!(
                f.next().is_none(),
                "unexpected trailing field in vector line: {line:?}"
            );
            let input = &inputs[idx];
            let produced = normalise_gzip_os(
                zlib_rs_deflate_full(input, level, wbits, mem_level, strat_from_id(strat)),
                wbits,
            );
            assert_eq!(
                produced,
                expected,
                "byte-identity mismatch vs C zlib 1.3.2.1-motley: input #{idx} \
                 (len {}), level {level}, windowBits {wbits}, memLevel {mem_level}, \
                 strategy {:?}",
                input.len(),
                strat_from_id(strat),
            );
            count += 1;
        }
        assert!(count > 0, "no oracle vectors were parsed from the table");
    }

    /// The wide-grid corpora must be bit-identical to the bytes the reference C
    /// encoder consumed when the grid tables were baked.
    ///
    /// Proves the chain of custody behind every one of the 4,125 grid rows: the
    /// tables assert nothing meaningful unless [`bi_grid_inputs`] still produces
    /// exactly the corpora recorded in [`GRID_CORPUS_DIGESTS`]. Cheap (five 16 KiB
    /// buffers) and it converts an otherwise baffling mass failure into a single
    /// named diagnostic.
    #[test]
    fn grid_corpora_match_the_oracle_inputs() {
        let corpora = bi_grid_inputs();
        assert_eq!(
            corpora.len(),
            GRID_CORPUS_DIGESTS.len(),
            "the grid corpus count is part of the table's `<input-index>` contract"
        );
        for (idx, (corpus, &(expected_len, expected_crc))) in
            corpora.iter().zip(GRID_CORPUS_DIGESTS.iter()).enumerate()
        {
            assert_eq!(
                corpus.len(),
                expected_len,
                "grid corpus #{idx} changed length; the baked grid digests no longer apply"
            );
            assert_eq!(
                crc32(0, corpus),
                expected_crc,
                "grid corpus #{idx} changed content; the baked grid digests no longer apply"
            );
        }
    }

    /// Structural audit of [`BI_GRID`]: the table must hold exactly
    /// `GRID_ROWS_PER_FRAMING` rows for each of the [`GRID_PLAIN_FRAMINGS`] and no
    /// rows outside them.
    ///
    /// This is what makes [`check_grid`]'s framing filter safe. Without it a row
    /// carrying a `windowBits` value no test asks for — a typo, or an axis added
    /// to the table but not to the tests — would be parsed, silently skipped, and
    /// never verified.
    #[test]
    fn grid_table_is_structurally_complete() {
        let total = BI_GRID.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(
            total,
            GRID_ROWS_PER_FRAMING * GRID_PLAIN_FRAMINGS.len(),
            "BI_GRID row count changed; the documented grid cardinality is stale"
        );
        let mut covered = 0usize;
        for framing in GRID_PLAIN_FRAMINGS {
            let rows = framing_row_count(BI_GRID, framing);
            assert_eq!(
                rows, GRID_ROWS_PER_FRAMING,
                "BI_GRID carries {rows} rows for windowBits {framing}, expected \
                 {GRID_ROWS_PER_FRAMING}"
            );
            covered += rows;
        }
        assert_eq!(
            covered, total,
            "BI_GRID contains rows whose windowBits is outside GRID_PLAIN_FRAMINGS, so no \
             test would ever check them"
        );
    }

    /// Structural audit of [`BI_GRID_GZIP`]: exactly `GRID_ROWS_PER_FRAMING` rows,
    /// all of them `windowBits = 31`.
    #[cfg(feature = "gzip")]
    #[test]
    fn grid_gzip_table_is_structurally_complete() {
        let total = BI_GRID_GZIP
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(
            total, GRID_ROWS_PER_FRAMING,
            "BI_GRID_GZIP row count changed; the documented grid cardinality is stale"
        );
        assert_eq!(
            framing_row_count(BI_GRID_GZIP, 31),
            total,
            "every BI_GRID_GZIP row must be windowBits 31"
        );
    }

    /// Wide grid, zlib framing at the full 32 KiB window (`windowBits = 15`): 825
    /// digest rows over 5 corpora × `memLevel` {1, 8, 9} × levels {−1, 0..=9} ×
    /// all five strategies, against the genuine C zlib `1.3.2.1-motley`.
    #[test]
    fn matches_reference_zlib_grid_zlib_window() {
        assert_eq!(check_grid(BI_GRID, 15), GRID_ROWS_PER_FRAMING);
    }

    /// Wide grid, raw DEFLATE at the full 32 KiB window (`windowBits = -15`): 825
    /// digest rows on the same axes. The most probative plain framing, because raw
    /// output carries no header or trailer — every divergence would be a pure
    /// match-finder or Huffman divergence.
    #[test]
    fn matches_reference_zlib_grid_raw_window() {
        assert_eq!(check_grid(BI_GRID, -15), GRID_ROWS_PER_FRAMING);
    }

    /// Wide grid, zlib framing at the 512-byte window (`windowBits = 9`): 825
    /// digest rows on the same axes, exercising `MAX_DIST == 250` match truncation
    /// and 32 window slides per corpus.
    #[test]
    fn matches_reference_zlib_grid_small_zlib_window() {
        assert_eq!(check_grid(BI_GRID, 9), GRID_ROWS_PER_FRAMING);
    }

    /// Wide grid, raw DEFLATE at the 512-byte window (`windowBits = -9`): 825
    /// digest rows on the same axes. The five-field tier-1 tables carry no
    /// `windowBits = -9` rows, so the small-raw grid is where this framing's
    /// byte-identity coverage lives.
    #[test]
    fn matches_reference_zlib_grid_small_raw_window() {
        assert_eq!(check_grid(BI_GRID, -9), GRID_ROWS_PER_FRAMING);
    }

    /// Wide grid, gzip framing (`windowBits = 31`): 825 digest rows on the same
    /// axes, with the platform-dependent `OS` byte normalised
    /// ([`normalise_gzip_os`]). Gated on the `gzip` feature, which is required to
    /// emit a gzip wrapper.
    #[cfg(feature = "gzip")]
    #[test]
    fn matches_reference_zlib_grid_gzip_window() {
        assert_eq!(check_grid(BI_GRID_GZIP, 31), GRID_ROWS_PER_FRAMING);
    }

    /// Exact literal bytes at the `memLevel` 1 and 9 corners for the zlib, raw and
    /// small-window framings: the 24 rows of [`BI_EXTREMES`], levels 1 and 9,
    /// strategies 0 / 2 / 3, against the genuine C zlib `1.3.2.1-motley`.
    #[test]
    fn matches_reference_zlib_mem_level_extremes() {
        check_exact_with_mem_level(BI_EXTREMES);
    }

    /// Exact literal bytes at the `memLevel` 1 and 9 corners for gzip framing: the
    /// 12 rows of [`BI_EXTREMES_GZIP`]. Gated on the `gzip` feature.
    #[cfg(feature = "gzip")]
    #[test]
    fn matches_reference_zlib_mem_level_extremes_gzip() {
        check_exact_with_mem_level(BI_EXTREMES_GZIP);
    }
}
