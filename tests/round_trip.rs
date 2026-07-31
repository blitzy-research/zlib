//! Property-based compress → decompress round-trip conformance tests.
//!
//! This integration test is the *randomized* half of the `test/example.c`
//! conformance story (the *fixed-vector* half lives in `tests/regression.rs`).
//! It generalizes the fixed round-trips that `example.c` proves — `test_compress`
//! runs `compress()`/`uncompress()` on the anchor input `"hello, hello!"`;
//! `test_deflate`/`test_inflate` round-trip through the streaming engine while
//! *forcing `avail_in = avail_out = 1`*; `test_large_deflate`/`test_large_inflate`
//! round-trip a 20 000-byte buffer while switching levels and strategies — into
//! **properties over arbitrary inputs**.
//!
//! The single highest-value invariant asserted here is the universal identity
//!
//! ```text
//! uncompress(compress(x)) == x
//! ```
//!
//! held across:
//!
//! * every compression level (`Z_DEFAULT_COMPRESSION = -1` and `0..=9`),
//! * every strategy (`Z_DEFAULT_STRATEGY`, `Z_FILTERED`, `Z_HUFFMAN_ONLY`,
//!   `Z_RLE`, `Z_FIXED`),
//! * incremental streaming with arbitrarily small input/output chunk sizes
//!   (mirroring `example.c`'s one-byte buffers),
//! * every stream framing exposed through `windowBits` — zlib (RFC 1950),
//!   raw DEFLATE (RFC 1951), and gzip (RFC 1952),
//! * both the largest window (`windowBits` magnitude `15`, a 32 KiB history)
//!   and the smallest usable one (magnitude `9`, 512 bytes), in every framing,
//!   and
//! * every distinguishable `memLevel` — the smallest hash and symbol buffer
//!   (`1`), the default (`DEF_MEM_LEVEL = 8`), and the largest
//!   (`MAX_MEM_LEVEL = 9`).
//!
//! The window-size and `memLevel` axes are the two that the one-call API cannot
//! reach and that `example.c` never varies: it drives `deflateInit`/`inflateInit`
//! plus `deflateParams`, which fixes `windowBits = 15` and `memLevel = 8` for
//! every one of its cases. They are nonetheless two axes of the configuration
//! grid this migration is measured against, and they change real encoder
//! behaviour — `memLevel` sizes both the hash table (`hash_bits = memLevel + 7`)
//! and the symbol buffer (`lit_bufsize = 1 << (memLevel + 6)`, so `memLevel = 1`
//! forces a block boundary every 128 symbols), while a 512-byte window caps
//! every match distance and makes the history wrap constantly. Covering them
//! here turns them into an always-on gate that needs no C toolchain.
//!
//! Generators are bounded (via `Gen::new`) and, where explicit random data is
//! used, seeded (`StdRng::seed_from_u64`) so the suite is deterministic and
//! fast in CI. Everything binds strictly to the public `zlib_rs` API and
//! contains no `unsafe`.

// This is a pure black-box test over the safe public API; forbid `unsafe`
// outright so the "zero unsafe" contract is machine-checked for this file.
#![forbid(unsafe_code)]

use quickcheck::{Gen, QuickCheck, TestResult};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};

// Idiomatic public surface (curated crate-root re-exports from `src/lib.rs`).
use zlib_rs::{
    FlushMode, ReturnCode, Strategy, Z_BEST_COMPRESSION, Z_BEST_SPEED, Z_DEFAULT_COMPRESSION,
    Z_NO_COMPRESSION, ZStream, compress_bound, compress2, uncompress,
};
// Streaming engine entry points — public via `pub mod deflate` / `pub mod
// inflate` in `src/lib.rs`. The one-call API takes only a `level`, so strategy,
// chunked-streaming, and framing coverage is driven through these.
use zlib_rs::deflate::{deflate, deflate_end, deflate_init2};
use zlib_rs::inflate::{inflate, inflate_end, inflate_init2};
// Deflate initializer arguments that are not part of the curated root prelude.
use zlib_rs::constants::{DEF_MEM_LEVEL, MAX_MEM_LEVEL, Z_DEFLATED};

// ===========================================================================
// Shared fixtures
// ===========================================================================

/// Every valid zlib compression level: `Z_DEFAULT_COMPRESSION` (`-1`) plus the
/// explicit range `0..=9` (`Z_NO_COMPRESSION` .. `Z_BEST_COMPRESSION`).
const ALL_LEVELS: [i32; 11] = [
    Z_DEFAULT_COMPRESSION,
    Z_NO_COMPRESSION,
    2,
    3,
    4,
    5,
    6,
    7,
    8,
    Z_BEST_COMPRESSION,
    Z_BEST_SPEED,
];

/// All five deflate strategies.
const STRATEGIES: [Strategy; 5] = [
    Strategy::Default,
    Strategy::Filtered,
    Strategy::HuffmanOnly,
    Strategy::Rle,
    Strategy::Fixed,
];

/// `windowBits` selecting the zlib wrapper (RFC 1950).
const WBITS_ZLIB: i32 = 15;
/// `windowBits` selecting raw DEFLATE with no wrapper (RFC 1951).
const WBITS_RAW: i32 = -15;
/// `windowBits` selecting the gzip wrapper (RFC 1952): `16 + 15`.
#[cfg(feature = "gzip")]
const WBITS_GZIP: i32 = 31;

/// `windowBits` selecting the *smallest usable* zlib window: `2^9 = 512` bytes.
///
/// Nine — not eight — is the floor for a symmetric round-trip, and that is a
/// property of the encoder rather than an arbitrary choice here. `deflateInit2`
/// promotes a `windowBits` of `8` to `9` ("until 256-byte window bug fixed"),
/// so a zlib stream requested at `8` is *emitted* with a 9-bit window declared
/// in its CMF byte; an inflate initialized at `8` would then reject it as an
/// invalid window size. Raw `windowBits = -8` is rejected outright, because C
/// permits the 8-bit request only for the zlib wrapper. `9` is therefore the
/// smallest magnitude that behaves identically on both sides.
const WBITS_SMALL_ZLIB: i32 = 9;
/// `windowBits` selecting the smallest usable raw DEFLATE window (512 bytes).
const WBITS_SMALL_RAW: i32 = -9;
/// gzip framing over the smallest usable window: `16 + 9`.
#[cfg(feature = "gzip")]
const WBITS_SMALL_GZIP: i32 = 25;

/// Every `memLevel` that produces a distinguishable encoder configuration: the
/// smallest (`1`), the zlib default (`DEF_MEM_LEVEL = 8`), and the largest
/// (`MAX_MEM_LEVEL = 9`).
///
/// `memLevel` is not a tuning hint the encoder may ignore — it sizes two
/// structures outright. `hash_bits = memLevel + 7` fixes the match-finder hash
/// (256 entries at `1`, 65 536 at `9`), and `lit_bufsize = 1 << (memLevel + 6)`
/// fixes the symbol buffer (128 symbols at `1`, 32 768 at `9`), which is what
/// decides how often a block is closed and flushed. The two ends therefore
/// drive genuinely different code paths through `_tr_flush_block`, and both must
/// decode back to the original bytes.
const MEM_LEVELS: [i32; 3] = [1, DEF_MEM_LEVEL, MAX_MEM_LEVEL];

// ===========================================================================
// Round-trip helpers
// ===========================================================================

/// One-call round-trip: `compress2` then `uncompress`, returning `true` iff the
/// decompressed bytes are byte-identical to `data`.
///
/// This is the direct generalization of `example.c`'s `test_compress`. The
/// destination for compression is sized to [`compress_bound`] (the contractual
/// worst-case output size), and the destination for decompression is sized to
/// the *known* original length — exactly as a caller that recorded the
/// uncompressed size out of band would do.
fn one_call_round_trip(data: &[u8], level: i32) -> bool {
    let mut compressed = vec![0u8; compress_bound(data.len())];
    let produced = match compress2(&mut compressed, data, level) {
        Ok(n) => n,
        Err(_) => return false,
    };

    let mut restored = vec![0u8; data.len()];
    match uncompress(&mut restored, &compressed[..produced]) {
        Ok(n) => n == data.len() && restored.as_slice() == data,
        Err(_) => false,
    }
}

/// Streams `data` through the deflate engine at an explicit `mem_level`,
/// offering at most `in_chunk` input bytes and `out_chunk` output bytes per
/// `deflate()` call, and returns the complete compressed stream.
///
/// Small chunk sizes force many `Z_OK` continuations, exercising the resumable
/// state machine the same way `example.c`'s `avail_in = avail_out = 1` loop
/// does. `Z_FINISH` is issued once — and only once — the final input byte has
/// been offered.
///
/// The output chunk size is a *throughput* parameter here, never a correctness
/// one: the loop keeps calling `deflate()` until it reports `Z_STREAM_END`,
/// appending whatever each call produced, so a scratch buffer that fills is
/// simply drained and reused. That property is what lets the non-default
/// configurations this helper exists to serve be driven with a
/// [`compress_bound`]-sized buffer at all — C's tight `compressBound` formula
/// is only valid for `windowBits = 15` / `memLevel = 8`, and `deflateBound`
/// falls back to a *larger* conservative bound for anything else.
///
/// [`compress_stream`] is the `DEF_MEM_LEVEL` specialization of this function.
fn compress_stream_with_mem_level(
    data: &[u8],
    level: i32,
    strategy: Strategy,
    window_bits: i32,
    mem_level: i32,
    in_chunk: usize,
    out_chunk: usize,
) -> Vec<u8> {
    let mut strm = ZStream::new();
    assert_eq!(
        deflate_init2(
            &mut strm,
            level,
            Z_DEFLATED,
            window_bits,
            mem_level,
            strategy,
        ),
        Ok(ReturnCode::Ok),
        "deflate_init2 failed (level={level}, wbits={window_bits}, \
         memLevel={mem_level}, strategy={strategy:?})"
    );

    let in_chunk = in_chunk.max(1);
    let mut scratch = vec![0u8; out_chunk.max(1)];
    let mut compressed = Vec::new();
    let mut in_pos = 0usize;
    let no_flush = FlushMode::NoFlush.as_c_int();
    let finish = FlushMode::Finish.as_c_int();

    let mut guard = 0usize;
    loop {
        guard += 1;
        assert!(guard < 50_000_000, "compress_stream did not terminate");

        let win_end = (in_pos + in_chunk).min(data.len());
        let input = &data[in_pos..win_end];
        // Only the final window (the one that reaches the end of the input) is
        // flushed with Z_FINISH; earlier windows use Z_NO_FLUSH.
        let flush = if win_end == data.len() {
            finish
        } else {
            no_flush
        };

        let outcome = deflate(&mut strm, input, &mut scratch, flush);
        in_pos += outcome.consumed;
        compressed.extend_from_slice(&scratch[..outcome.produced]);

        match outcome.code {
            ReturnCode::StreamEnd => break,
            ReturnCode::Ok => {}
            ReturnCode::BufError => assert!(
                outcome.consumed != 0 || outcome.produced != 0,
                "deflate stalled with Z_BUF_ERROR (no progress)"
            ),
            other => panic!("unexpected deflate return code {other:?}"),
        }
    }

    // Mirrors C `deflateEnd`; RAII would also release the state, so the return
    // value is intentionally ignored (as C `compress2` does).
    let _ = deflate_end(&mut strm);
    compressed
}

/// Streams `data` through the deflate engine at the default `memLevel`
/// (`DEF_MEM_LEVEL = 8`) — the configuration every zlib one-call entry point
/// uses, and the only one `example.c` ever exercises.
///
/// A thin forwarder to [`compress_stream_with_mem_level`]: behaviour is
/// identical, byte for byte, to passing `DEF_MEM_LEVEL` explicitly. The
/// specialization exists so the many call sites that are *not* varying
/// `memLevel` stay readable.
fn compress_stream(
    data: &[u8],
    level: i32,
    strategy: Strategy,
    window_bits: i32,
    in_chunk: usize,
    out_chunk: usize,
) -> Vec<u8> {
    compress_stream_with_mem_level(
        data,
        level,
        strategy,
        window_bits,
        DEF_MEM_LEVEL,
        in_chunk,
        out_chunk,
    )
}

/// Streams a complete compressed `stream` through the inflate engine, offering
/// at most `in_chunk` input bytes and `out_chunk` output bytes per `inflate()`
/// call, and returns the fully decompressed bytes.
///
/// A stall (a non-`StreamEnd` return that consumed and produced nothing) is
/// treated as a hard failure: for a complete, valid stream the engine always
/// makes progress until it reports `Z_STREAM_END`.
fn decompress_stream(
    stream: &[u8],
    window_bits: i32,
    in_chunk: usize,
    out_chunk: usize,
) -> Vec<u8> {
    let mut strm = ZStream::new();
    assert_eq!(
        inflate_init2(&mut strm, window_bits),
        Ok(ReturnCode::Ok),
        "inflate_init2 failed (wbits={window_bits})"
    );

    let in_chunk = in_chunk.max(1);
    let mut scratch = vec![0u8; out_chunk.max(1)];
    let mut plain = Vec::new();
    let mut in_pos = 0usize;
    let no_flush = FlushMode::NoFlush.as_c_int();

    let mut guard = 0usize;
    loop {
        guard += 1;
        assert!(guard < 50_000_000, "decompress_stream did not terminate");

        let win_end = (in_pos + in_chunk).min(stream.len());
        let input = &stream[in_pos..win_end];

        let outcome = inflate(&mut strm, input, &mut scratch, no_flush);
        in_pos += outcome.consumed;
        plain.extend_from_slice(&scratch[..outcome.produced]);

        match outcome.code {
            ReturnCode::StreamEnd => break,
            ReturnCode::Ok | ReturnCode::BufError => assert!(
                outcome.consumed != 0 || outcome.produced != 0,
                "inflate stalled (code={:?}); stream may be truncated",
                outcome.code
            ),
            other => panic!("unexpected inflate return code {other:?}"),
        }
    }

    let _ = inflate_end(&mut strm);
    plain
}

/// Full streaming round-trip: compress `data` then decompress it back, using
/// the same framing on both sides. Returns the recovered bytes.
fn stream_round_trip(
    data: &[u8],
    level: i32,
    strategy: Strategy,
    window_bits: i32,
    in_chunk: usize,
    out_chunk: usize,
) -> Vec<u8> {
    let compressed = compress_stream(data, level, strategy, window_bits, in_chunk, out_chunk);
    decompress_stream(&compressed, window_bits, in_chunk, out_chunk)
}

/// Whole-buffer streaming round-trip, used where the variable under test is the
/// *encoder* (strategy or framing) rather than decode granularity.
///
/// Both directions run through the streaming engine — so raw DEFLATE and gzip
/// framing and every strategy are covered, none of which the one-call API
/// exposes — but each direction is driven with a single whole-size buffer.
/// Decompressing the complete stream in one pass keeps the decode on the very
/// same code path the proven one-call [`uncompress`] takes, isolating the
/// encoder variable under test from streaming-resume concerns.
///
/// Design note (decode granularity vs. the inflate fast path): a *multi-call*
/// inflate that offers a large (`>= 258`-byte) output window on a resumed call
/// re-enters the fast decode loop (`src/inflate/fast.rs`) carrying the bit
/// accumulator saved from the previous call, so `state.bits` may be 8 or more on
/// entry. That is ordinary rather than exceptional, and it is handled: C's
/// `inffast.c` header lists `state->bits < 8` among its entry assumptions, but
/// nothing in C enforces it and the C driver does not provide it — the slow
/// path's code lookups pull whole speculative bytes and then drop only the width
/// of the code actually decoded (`inflate.c` L924-L928, L940). This port's fast
/// loop therefore asserts the invariant it genuinely depends on, `bits <= 32`,
/// and clamps its byte-give-back epilogue to the bytes that call itself pulled,
/// so a resumed entry decodes byte-exactly whatever `bits` carries in.
///
/// Cross-call fast-path re-entry is covered where the invariant lives rather
/// than through this helper: `src/inflate/fast.rs`'s
/// `decodes_identically_when_entered_with_whole_buffered_bytes` and
/// `never_returns_input_bytes_it_did_not_pull` pin the carried-in bits and the
/// give-back clamp, and `inflate_coverage.rs`'s
/// `incrementally_delivered_input_decodes_byte_exactly` drives the 6-byte and
/// 258-byte entry-contract boundaries across raw, zlib, gzip and auto-detect
/// framing. Faithful to `example.c` — whose streaming tests use one-byte buffers
/// and whose bulk tests decode in one shot — the granularity stress here stays
/// in the small-window [`stream_round_trip`] callers, while strategy and framing
/// coverage decodes whole via this helper. The fast path is still exercised on
/// first entry by both this helper and the one-call [`uncompress`] tests.
fn round_trip_whole(data: &[u8], level: i32, strategy: Strategy, window_bits: i32) -> Vec<u8> {
    round_trip_whole_with_mem_level(data, level, strategy, window_bits, DEF_MEM_LEVEL)
}

/// Whole-buffer streaming round-trip at an explicit `mem_level`.
///
/// Carries the body [`round_trip_whole`] forwards to; every design note on that
/// function — in particular *why* the decode is driven whole rather than in
/// resumed chunks — applies here unchanged.
fn round_trip_whole_with_mem_level(
    data: &[u8],
    level: i32,
    strategy: Strategy,
    window_bits: i32,
    mem_level: i32,
) -> Vec<u8> {
    let bound = compress_bound(data.len()).max(64);
    let compressed = compress_stream_with_mem_level(
        data,
        level,
        strategy,
        window_bits,
        mem_level,
        data.len().max(1),
        bound,
    );
    // Whole compressed input + whole-size output => a single `inflate()` call
    // that reports `Z_STREAM_END`, exactly as one-call `uncompress` relies on.
    decompress_stream(
        &compressed,
        window_bits,
        compressed.len().max(1),
        data.len().max(1),
    )
}

/// Deterministic, effectively-incompressible bytes from a seeded PRNG.
///
/// A fixed `seed` keeps the suite reproducible while still exercising the
/// stored-block / low-compressibility paths that structured inputs never reach.
fn seeded_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut buf = vec![0u8; len];
    rng.fill_bytes(&mut buf);
    buf
}

/// A deterministic ~28 KiB corpus that mixes all three compressibility regimes
/// in one buffer: repetitive text (long back-references), a single long literal
/// run (RLE-friendly), and seeded incompressible bytes (stored-block territory).
///
/// It is deliberately far larger than a 512-byte window, so a round-trip at
/// `windowBits` magnitude `9` must wrap the history many times over, and far
/// larger than a `memLevel = 1` symbol buffer (128 symbols), so the encoder must
/// close and emit hundreds of blocks. Both are exactly the conditions the
/// window-size and `memLevel` properties below exist to exercise.
fn heterogeneous_corpus() -> Vec<u8> {
    let mut corpus =
        b"round-trip corpus: the quick brown fox jumps over the lazy dog. ".repeat(300);
    corpus.extend_from_slice(&vec![b'Q'; 4_000]);
    corpus.extend_from_slice(&seeded_random_bytes(0x0BAD_F00D, 4_000));
    corpus
}

// ===========================================================================
// Phase 2 — core one-call round-trip properties
// ===========================================================================

/// `uncompress(compress(x)) == x` for arbitrary `x` at the default level.
///
/// The foundational property: whatever the encoder does with an arbitrary byte
/// string, decompression must recover it exactly.
#[test]
fn qc_one_call_round_trip_default_level() {
    fn prop(data: Vec<u8>) -> bool {
        one_call_round_trip(&data, Z_DEFAULT_COMPRESSION)
    }
    QuickCheck::new()
        .tests(300)
        .rng(Gen::new(512))
        .quickcheck(prop as fn(Vec<u8>) -> bool);
}

/// The round-trip identity holds at *every* compression level.
///
/// The arbitrary integer is normalized into the valid level set
/// `{-1, 0, 1, ..., 9}` via `rem_euclid(11) - 1`, so every generated case tests
/// a real level. A defensive `TestResult::discard()` guards the (post-
/// normalization unreachable) out-of-range case rather than asserting on it.
#[test]
fn qc_one_call_round_trip_all_levels() {
    fn prop(data: Vec<u8>, raw_level: i32) -> TestResult {
        // rem_euclid(11) ∈ 0..=10  →  shifted by -1  →  -1..=9.
        let level = raw_level.rem_euclid(11) - 1;
        if level != Z_DEFAULT_COMPRESSION && !(0..=9).contains(&level) {
            return TestResult::discard();
        }
        TestResult::from_bool(one_call_round_trip(&data, level))
    }
    QuickCheck::new()
        .tests(250)
        .rng(Gen::new(400))
        .quickcheck(prop as fn(Vec<u8>, i32) -> TestResult);
}

/// The round-trip identity holds under *every* deflate strategy.
///
/// Strategy is not selectable through the one-call API, so this drives the
/// streaming engine (`deflate_init2` takes the strategy) and decompresses back
/// through the matching inflate path.
#[test]
fn qc_stream_round_trip_all_strategies() {
    fn prop(data: Vec<u8>, raw_idx: u8) -> bool {
        let strategy = STRATEGIES[raw_idx as usize % STRATEGIES.len()];
        let recovered = round_trip_whole(&data, Z_DEFAULT_COMPRESSION, strategy, WBITS_ZLIB);
        recovered.as_slice() == data
    }
    QuickCheck::new()
        .tests(150)
        .rng(Gen::new(400))
        .quickcheck(prop as fn(Vec<u8>, u8) -> bool);
}

// ===========================================================================
// Phase 3 — structured-input round-trips + explicit example.c anchors
// ===========================================================================

/// The `example.c` anchor input `"hello, hello!"` must round-trip at every
/// level through both the one-call API and the streaming engine (zlib + raw).
#[test]
fn hello_anchor_round_trips_every_level() {
    let hello: &[u8] = b"hello, hello!";
    for &level in &ALL_LEVELS {
        assert!(
            one_call_round_trip(hello, level),
            "one-call round-trip of the hello anchor failed at level {level}"
        );
        for &wbits in &[WBITS_ZLIB, WBITS_RAW] {
            // Byte-at-a-time streaming, mirroring example.c's forced 1-byte
            // buffers.
            let recovered = stream_round_trip(hello, level, Strategy::Default, wbits, 1, 1);
            assert_eq!(
                recovered.as_slice(),
                hello,
                "streaming round-trip of the hello anchor failed (level={level}, wbits={wbits})"
            );
        }
    }
}

/// Mirrors `example.c`'s `test_large_deflate`/`test_large_inflate`: a
/// 20 000-byte buffer (mostly zeros, so highly compressible) round-trips across
/// every level (one-call) and every strategy (streaming).
#[test]
fn large_zero_filled_buffer_round_trips() {
    let data = vec![0u8; 20_000];

    for &level in &ALL_LEVELS {
        assert!(
            one_call_round_trip(&data, level),
            "one-call round-trip of the 20000-byte buffer failed at level {level}"
        );
    }

    for &strategy in &STRATEGIES {
        let recovered = round_trip_whole(&data, Z_BEST_COMPRESSION, strategy, WBITS_ZLIB);
        assert_eq!(
            recovered.len(),
            data.len(),
            "length mismatch (strategy={strategy:?})"
        );
        assert_eq!(
            recovered.as_slice(),
            data.as_slice(),
            "streaming round-trip of the 20000-byte buffer failed (strategy={strategy:?})"
        );
    }
}

/// Structured inputs that stress distinct coder paths — long identical runs
/// (RLE-friendly), highly repetitive text, seeded incompressible data, an empty
/// input, and a mixture — must all round-trip identically.
#[test]
fn structured_inputs_round_trip() {
    let long_run = vec![b'A'; 30_000];
    let repetitive = b"hello, ".repeat(5_000);
    let incompressible = seeded_random_bytes(0x00C0_FFEE, 25_000);
    let empty: Vec<u8> = Vec::new();
    let mixed = {
        let mut m = Vec::new();
        m.extend_from_slice(&long_run[..1_000]);
        m.extend_from_slice(&repetitive[..1_000]);
        m.extend_from_slice(&incompressible[..1_000]);
        m
    };

    let cases: [&[u8]; 5] = [
        long_run.as_slice(),
        repetitive.as_slice(),
        incompressible.as_slice(),
        empty.as_slice(),
        mixed.as_slice(),
    ];

    for &data in &cases {
        for &level in &[
            Z_NO_COMPRESSION,
            Z_BEST_SPEED,
            Z_DEFAULT_COMPRESSION,
            Z_BEST_COMPRESSION,
        ] {
            assert!(
                one_call_round_trip(data, level),
                "one-call round-trip failed (len={}, level={level})",
                data.len()
            );
        }

        // Whole-buffer streaming pass validates the streaming engine on the same
        // inputs, isolated from decode-granularity effects.
        let whole = round_trip_whole(data, Z_DEFAULT_COMPRESSION, Strategy::Default, WBITS_ZLIB);
        assert_eq!(
            whole.as_slice(),
            data,
            "whole-buffer streaming round-trip failed (len={})",
            data.len()
        );

        // Small symmetric chunks additionally exercise the resumable state
        // machine on these structured inputs (output window < 258 keeps the
        // decode on the slow path, faithful to example.c's one-byte buffers).
        let chunked = stream_round_trip(
            data,
            Z_DEFAULT_COMPRESSION,
            Strategy::Default,
            WBITS_ZLIB,
            100,
            100,
        );
        assert_eq!(
            chunked.as_slice(),
            data,
            "chunked streaming round-trip failed (len={})",
            data.len()
        );
    }
}

/// Every strategy must round-trip a heterogeneous input (repetitive text, then
/// incompressible bytes, then a long run) identically through the streaming
/// engine with deliberately mismatched, non-power-of-two chunk sizes.
#[test]
fn all_strategies_round_trip_explicit() {
    let data = {
        let mut d = b"the quick brown fox ".repeat(300);
        d.extend_from_slice(&seeded_random_bytes(0x1234_5678, 2_000));
        d.resize(d.len() + 2_000, b'Z');
        d
    };

    for &strategy in &STRATEGIES {
        let recovered = round_trip_whole(&data, Z_DEFAULT_COMPRESSION, strategy, WBITS_ZLIB);
        assert_eq!(
            recovered.as_slice(),
            data.as_slice(),
            "strategy {strategy:?} failed to round-trip"
        );
    }
}

// ===========================================================================
// Phase 4 — streaming round-trip properties (incremental state machine)
// ===========================================================================

/// The streaming round-trip identity holds for arbitrary small chunk sizes.
///
/// The chunk size is normalized into `1..=64`; both the input and output
/// windows use it, maximally stressing the resumable deflate/inflate state
/// machine — the property-level analogue of `example.c`'s one-byte buffers.
#[test]
fn qc_stream_round_trip_small_chunks() {
    fn prop(data: Vec<u8>, raw_chunk: u16) -> bool {
        let chunk = 1 + (raw_chunk as usize % 64);
        let recovered = stream_round_trip(
            &data,
            Z_DEFAULT_COMPRESSION,
            Strategy::Default,
            WBITS_ZLIB,
            chunk,
            chunk,
        );
        recovered.as_slice() == data
    }
    QuickCheck::new()
        .tests(120)
        .rng(Gen::new(300))
        .quickcheck(prop as fn(Vec<u8>, u16) -> bool);
}

/// Explicit byte-at-a-time streaming (chunk size 1) — the direct analogue of
/// `example.c`'s `test_deflate`/`test_inflate` forcing `avail_in = avail_out =
/// 1` — across zlib and raw framing, including the empty input.
#[test]
fn streaming_byte_at_a_time_round_trips() {
    let inputs: [&[u8]; 3] = [
        b"hello, hello!",
        b"",
        b"aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd",
    ];
    for input in inputs {
        for &wbits in &[WBITS_ZLIB, WBITS_RAW] {
            let recovered =
                stream_round_trip(input, Z_DEFAULT_COMPRESSION, Strategy::Default, wbits, 1, 1);
            assert_eq!(
                recovered.as_slice(),
                input,
                "byte-at-a-time round-trip failed (wbits={wbits})"
            );
        }
    }
}

// ===========================================================================
// Phase 5 — framing round-trips (zlib / raw / gzip)
// ===========================================================================

/// The round-trip identity holds under both zlib (RFC 1950) and raw DEFLATE
/// (RFC 1951) framing for arbitrary inputs.
#[test]
fn qc_stream_round_trip_zlib_and_raw_framing() {
    fn prop(data: Vec<u8>) -> bool {
        let zlib = round_trip_whole(&data, 6, Strategy::Default, WBITS_ZLIB);
        let raw = round_trip_whole(&data, 6, Strategy::Default, WBITS_RAW);
        zlib.as_slice() == data && raw.as_slice() == data
    }
    QuickCheck::new()
        .tests(150)
        .rng(Gen::new(400))
        .quickcheck(prop as fn(Vec<u8>) -> bool);
}

/// Explicit zlib and raw framing round-trips across every level, on a
/// repetitive text input large enough to produce real back-references.
#[test]
fn framing_zlib_and_raw_round_trip_explicit() {
    let data = b"framing check: zlib (RFC 1950) and raw DEFLATE (RFC 1951). ".repeat(40);
    for &wbits in &[WBITS_ZLIB, WBITS_RAW] {
        for &level in &ALL_LEVELS {
            let recovered = round_trip_whole(&data, level, Strategy::Default, wbits);
            assert_eq!(
                recovered.as_slice(),
                data.as_slice(),
                "framing round-trip failed (wbits={wbits}, level={level})"
            );
        }
    }
}

/// gzip framing (RFC 1952) round-trips via `windowBits = 31` on both the
/// compress and decompress sides. Gated on the `gzip` feature (on by default);
/// a build without it omits gzip framing entirely.
#[cfg(feature = "gzip")]
#[test]
fn framing_gzip_round_trips() {
    let inputs: [Vec<u8>; 3] = [
        b"hello, hello!".to_vec(),
        Vec::new(),
        b"the quick brown fox jumps over the lazy dog. ".repeat(50),
    ];
    for data in &inputs {
        let recovered =
            round_trip_whole(data, Z_DEFAULT_COMPRESSION, Strategy::Default, WBITS_GZIP);
        assert_eq!(
            recovered.as_slice(),
            data.as_slice(),
            "gzip framing round-trip failed (len={})",
            data.len()
        );
    }
}

// ===========================================================================
// Phase 6 — memLevel and window-size round-trips
//
// The two encoder axes the one-call API cannot reach and `example.c` never
// varies. Both change real encoder structure rather than merely tuning it, and
// neither was covered by any always-on test before: the only in-repository
// coverage of them lived in the opt-in live-C-oracle harness, which a default
// `cargo test` (correctly) never builds.
// ===========================================================================

/// The round-trip identity holds at *every* `memLevel`, under *every* strategy.
///
/// `memLevel` sizes the match-finder hash (`hash_bits = memLevel + 7`) and the
/// symbol buffer (`lit_bufsize = 1 << (memLevel + 6)`), so the smallest value
/// closes a block every 128 symbols while the largest buffers 32 768 of them.
/// Both extremes must decode back to the original bytes for an arbitrary input.
#[test]
fn qc_mem_level_round_trip() {
    fn prop(data: Vec<u8>, raw_mem: u8, raw_strategy: u8) -> bool {
        let mem_level = MEM_LEVELS[raw_mem as usize % MEM_LEVELS.len()];
        let strategy = STRATEGIES[raw_strategy as usize % STRATEGIES.len()];
        let recovered = round_trip_whole_with_mem_level(
            &data,
            Z_DEFAULT_COMPRESSION,
            strategy,
            WBITS_ZLIB,
            mem_level,
        );
        recovered.as_slice() == data
    }
    QuickCheck::new()
        .tests(150)
        .rng(Gen::new(400))
        .quickcheck(prop as fn(Vec<u8>, u8, u8) -> bool);
}

/// Explicit `memLevel` grid on a corpus large enough to make the setting bite:
/// every `memLevel` against every level, every `memLevel` against every
/// strategy, and a byte-at-a-time pass at the smallest `memLevel`.
///
/// The corpus is ~28 KiB, so at `memLevel = 1` (a 128-symbol buffer) the encoder
/// must close and emit hundreds of consecutive blocks — the stress the property
/// above only reaches with its 400-byte generator bound.
#[test]
fn mem_level_round_trips_explicit() {
    let data = heterogeneous_corpus();

    for &mem_level in &MEM_LEVELS {
        for &level in &ALL_LEVELS {
            let recovered = round_trip_whole_with_mem_level(
                &data,
                level,
                Strategy::Default,
                WBITS_ZLIB,
                mem_level,
            );
            assert_eq!(
                recovered.as_slice(),
                data.as_slice(),
                "memLevel round-trip failed (memLevel={mem_level}, level={level})"
            );
        }

        for &strategy in &STRATEGIES {
            let recovered = round_trip_whole_with_mem_level(
                &data,
                Z_BEST_COMPRESSION,
                strategy,
                WBITS_ZLIB,
                mem_level,
            );
            assert_eq!(
                recovered.as_slice(),
                data.as_slice(),
                "memLevel round-trip failed (memLevel={mem_level}, strategy={strategy:?})"
            );
        }
    }

    // Byte-at-a-time through the smallest symbol buffer: block boundaries and
    // one-byte input/output windows interleave, which is where the resumable
    // state machine has the least room to hide a bug.
    const SMALLEST_MEM_LEVEL: i32 = 1;
    const ONE_BYTE: usize = 1;
    let small = b"aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd".repeat(40);
    for &wbits in &[WBITS_ZLIB, WBITS_RAW] {
        let compressed = compress_stream_with_mem_level(
            &small,
            Z_BEST_SPEED,
            Strategy::Default,
            wbits,
            SMALLEST_MEM_LEVEL,
            ONE_BYTE,
            ONE_BYTE,
        );
        let recovered = decompress_stream(&compressed, wbits, ONE_BYTE, ONE_BYTE);
        assert_eq!(
            recovered.as_slice(),
            small.as_slice(),
            "byte-at-a-time round-trip at memLevel=1 failed (wbits={wbits})"
        );
    }
}

/// The round-trip identity holds over the smallest usable window (512 bytes) in
/// both zlib and raw framing, for arbitrary inputs.
///
/// The generator is bounded at 1 500 bytes — deliberately *above* the 512-byte
/// window — so a meaningful share of the generated cases force the history to
/// wrap and every match distance to be clamped to the smaller window.
#[test]
fn qc_small_window_framings_round_trip() {
    fn prop(data: Vec<u8>) -> bool {
        let zlib = round_trip_whole(&data, 6, Strategy::Default, WBITS_SMALL_ZLIB);
        let raw = round_trip_whole(&data, 6, Strategy::Default, WBITS_SMALL_RAW);
        zlib.as_slice() == data && raw.as_slice() == data
    }
    QuickCheck::new()
        .tests(120)
        .rng(Gen::new(1_500))
        .quickcheck(prop as fn(Vec<u8>) -> bool);
}

/// Explicit smallest-window grid: `windowBits` magnitude `9` across every level,
/// across every strategy at the smallest `memLevel` (the tightest configuration
/// the encoder accepts), and byte-at-a-time.
///
/// Also pins the two boundary behaviours that make `9` the floor: a zlib
/// `windowBits` of `8` is silently promoted to `9` by `deflateInit2`, and a raw
/// `windowBits` of `-8` is rejected with `Z_STREAM_ERROR`.
#[test]
fn small_window_framings_round_trip_explicit() {
    let data = heterogeneous_corpus();

    for &wbits in &[WBITS_SMALL_ZLIB, WBITS_SMALL_RAW] {
        for &level in &ALL_LEVELS {
            let recovered = round_trip_whole(&data, level, Strategy::Default, wbits);
            assert_eq!(
                recovered.as_slice(),
                data.as_slice(),
                "small-window round-trip failed (wbits={wbits}, level={level})"
            );
        }

        // Smallest window *and* smallest hash/symbol buffer together.
        for &strategy in &STRATEGIES {
            let recovered =
                round_trip_whole_with_mem_level(&data, Z_BEST_COMPRESSION, strategy, wbits, 1);
            assert_eq!(
                recovered.as_slice(),
                data.as_slice(),
                "small-window round-trip failed (wbits={wbits}, strategy={strategy:?}, memLevel=1)"
            );
        }

        // Byte-at-a-time over the small window: the input is longer than the
        // 512-byte history, so the window wraps while the state machine is being
        // resumed on every single byte.
        let repetitive = b"small window, one byte at a time, over and over. ".repeat(20);
        let recovered = stream_round_trip(
            &repetitive,
            Z_DEFAULT_COMPRESSION,
            Strategy::Default,
            wbits,
            1,
            1,
        );
        assert_eq!(
            recovered.as_slice(),
            repetitive.as_slice(),
            "small-window byte-at-a-time round-trip failed (wbits={wbits})"
        );
    }

    // The 8-bit window boundary, and the reason `9` — not `8` — is the floor
    // used above. A zlib `windowBits` of 8 is silently promoted to 9 by
    // `deflateInit2`, so the CMF byte of the emitted stream declares a 9-bit
    // window. An inflate initialized at 8 would therefore reject its own
    // encoder's output ("invalid window size"), while any inflate window of 9 or
    // wider accepts it. That asymmetry is C's behaviour, not a port artifact, so
    // it is pinned here explicitly: compress once at 8, decode at both 9 and 15.
    let promoted = compress_stream(
        &data,
        Z_DEFAULT_COMPRESSION,
        Strategy::Default,
        8,
        data.len(),
        4_096,
    );
    for &decode_wbits in &[WBITS_SMALL_ZLIB, WBITS_ZLIB] {
        let recovered = decompress_stream(
            &promoted,
            decode_wbits,
            promoted.len().max(1),
            data.len().max(1),
        );
        assert_eq!(
            recovered.as_slice(),
            data.as_slice(),
            "zlib windowBits=8 (promoted to 9) failed to decode at windowBits={decode_wbits}"
        );
    }

    // Raw framing has no such promotion: C accepts `windowBits == 8` only for
    // the zlib wrapper, so `-8` must be refused rather than quietly widened.
    let mut strm = ZStream::new();
    assert!(
        deflate_init2(
            &mut strm,
            Z_DEFAULT_COMPRESSION,
            Z_DEFLATED,
            -8,
            DEF_MEM_LEVEL,
            Strategy::Default,
        )
        .is_err(),
        "raw windowBits=-8 must be rejected (C deflate.c: windowBits == 8 && wrap != 1)"
    );
    let _ = deflate_end(&mut strm);
}

// ===========================================================================
// Phase 7 — contract properties and degenerate inputs
// ===========================================================================

/// `compress_bound(n)` is a *sufficient* destination size for `compress2` of any
/// `n`-byte input at any level — asserted as a property rather than spot-checked.
///
/// This is the contract callers actually rely on when they size a buffer once
/// and compress into it, so it is checked in the same direction they use it: the
/// bound must never under-report (`bound >= n`, since incompressible input can
/// only grow), `compress2` into a bound-sized buffer must never fail with
/// `Z_BUF_ERROR`, and the bytes it produces must fit within the bound.
#[test]
fn qc_compress_bound_is_sufficient() {
    fn prop(data: Vec<u8>, raw_level: i32) -> TestResult {
        // Same normalization as `qc_one_call_round_trip_all_levels`: -1..=9.
        let level = raw_level.rem_euclid(11) - 1;
        if level != Z_DEFAULT_COMPRESSION && !(0..=9).contains(&level) {
            return TestResult::discard();
        }

        let bound = compress_bound(data.len());
        if bound < data.len() {
            return TestResult::failed();
        }

        let mut dest = vec![0u8; bound];
        match compress2(&mut dest, &data, level) {
            Ok(produced) => TestResult::from_bool(produced <= bound),
            Err(_) => TestResult::failed(),
        }
    }
    QuickCheck::new()
        .tests(250)
        .rng(Gen::new(512))
        .quickcheck(prop as fn(Vec<u8>, i32) -> TestResult);
}

/// The degenerate inputs — empty, and every interesting single byte — round-trip
/// under every level, every strategy, every framing, and every `memLevel`.
///
/// A zero-length input is the one case where the encoder emits a wrapper and an
/// empty final block and nothing else, and a one-byte input is the one case
/// where no match can exist at all, so both bypass the match finder entirely.
/// The generators reach these lengths only by chance; here they are pinned
/// deterministically across the whole configuration space.
#[test]
fn empty_and_single_byte_inputs_round_trip() {
    let edges: [&[u8]; 6] = [b"", b"\x00", b"\x01", b"\x7f", b"\x80", b"\xff"];

    for &data in &edges {
        // One-call API at every level.
        for &level in &ALL_LEVELS {
            assert!(
                one_call_round_trip(data, level),
                "one-call round-trip of a {}-byte input failed at level {level}",
                data.len()
            );
        }

        // Streaming, byte-at-a-time, across every strategy and both framings.
        for &strategy in &STRATEGIES {
            for &wbits in &[WBITS_ZLIB, WBITS_RAW] {
                let recovered =
                    stream_round_trip(data, Z_DEFAULT_COMPRESSION, strategy, wbits, 1, 1);
                assert_eq!(
                    recovered.as_slice(),
                    data,
                    "byte-at-a-time round-trip of a {}-byte input failed \
                     (strategy={strategy:?}, wbits={wbits})",
                    data.len()
                );
            }
        }

        // Small windows crossed with every memLevel: the degenerate inputs are
        // the cheapest place to sweep that product exhaustively.
        for &wbits in &[WBITS_SMALL_ZLIB, WBITS_SMALL_RAW] {
            for &mem_level in &MEM_LEVELS {
                let recovered = round_trip_whole_with_mem_level(
                    data,
                    Z_DEFAULT_COMPRESSION,
                    Strategy::Default,
                    wbits,
                    mem_level,
                );
                assert_eq!(
                    recovered.as_slice(),
                    data,
                    "small-window round-trip of a {}-byte input failed \
                     (wbits={wbits}, memLevel={mem_level})",
                    data.len()
                );
            }
        }
    }
}

/// gzip framing (RFC 1952) round-trips over the smallest usable window
/// (`windowBits = 16 + 9`) and at every `memLevel`, including the empty input.
///
/// gzip is the one framing whose trailer carries both a CRC-32 and an ISIZE
/// computed over the *uncompressed* stream, so a window or symbol-buffer size
/// that perturbed the decoded bytes would surface here as a checksum failure
/// rather than as a silent mismatch. Gated on the `gzip` feature (on by
/// default); a build without it omits gzip framing entirely.
#[cfg(feature = "gzip")]
#[test]
fn gzip_small_window_and_mem_level_round_trips() {
    let corpus = heterogeneous_corpus();
    let inputs: [&[u8]; 3] = [corpus.as_slice(), b"", b"hello, hello!"];

    for &data in &inputs {
        for &wbits in &[WBITS_GZIP, WBITS_SMALL_GZIP] {
            for &mem_level in &MEM_LEVELS {
                let recovered = round_trip_whole_with_mem_level(
                    data,
                    Z_DEFAULT_COMPRESSION,
                    Strategy::Default,
                    wbits,
                    mem_level,
                );
                assert_eq!(
                    recovered.as_slice(),
                    data,
                    "gzip round-trip failed (len={}, wbits={wbits}, memLevel={mem_level})",
                    data.len()
                );
            }
        }
    }
}
