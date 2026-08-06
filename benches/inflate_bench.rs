//! Criterion throughput benchmarks for the `zlib-rs` inflate (decompression) engine.
//!
//! Pre-compresses representative inputs with `compress2`, then measures
//! `uncompress` throughput. Throughput is reported over the *decompressed*
//! (original) size, which is the meaningful denominator for a decoder: the same
//! corpus compresses to a different length at every level, so normalising by the
//! compressed size would make two source levels that decode to identical output
//! incomparable. Decompression cost can vary with how the source was compressed,
//! so both the source level (1 / 6 / 9) and the input profile (text /
//! repetitive / incompressible) are varied. The larger, match-heavy inputs drive
//! the `inflate_fast` hot path, and the `repetitive` profile drives the
//! short-distance overlapping-copy path that neither of the others reaches.
//!
//! Two decoders are measured, because there are two. The three `uncompress`
//! groups drive `inflate.c`'s mode loop; `inflate_back_by_profile` drives the
//! separate `infback.c` decoder that `gzip`-style consumers use. A regression in
//! either one is invisible to the other.
//!
//! # Measured position, and why it is quoted this way
//!
//! Measured against a reference C zlib built from the retained in-tree `*.c`
//! baseline (`gcc -O2 -D_LARGEFILE64_SOURCE=1 -DHAVE_UNISTD_H`) by an out-of-tree
//! differential harness that links one C driver against each library in turn.
//! Method: five interleaved A/B rep pairs with the C/RS order reversed on even
//! reps, each timed phase at least 0.30 s, taking the MEDIAN of each side; payload
//! 64 KiB; source levels 1, 6 and 9. Each cell is the range across those three
//! source levels, RS as a percentage of C:
//!
//! | profile | `uncompress` | `inflateBack` |
//! |---|---|---|
//! | `text` | 141%-154% | 216%-312% |
//! | `repetitive` | 154%-160% | 248%-344% |
//! | `incompressible` | 101%-114% | 86%-102% |
//!
//! Decompression is at or above parity on every `uncompress` profile. The one cell
//! that dips below is `inflateBack` on incompressible input, and it is not a
//! decode-logic figure: an incompressible payload is emitted as STORED blocks, so
//! that case is a `memcpy` running at roughly 19 GiB/s on both sides and the 86%
//! to 102% spread is host and `memcpy` variance rather than anything this crate
//! decides. Every case that actually decodes Huffman symbols and copies matches —
//! which is what `inflate_fast` and the `infback` fast path exist for — runs
//! between 141% and 344% of C.
//!
//! Compression, measured the same way and tabulated in
//! `benches/deflate_bench.rs`, is 113%-161% of the same reference on compressible
//! input and 82%-94% on incompressible input. Nothing timed *here* is a
//! compression figure.
//!
//! Those percentages are measured, but they are NOT measured by this file: this
//! file links no C library and runs no reference implementation, so a run of this
//! suite cannot reproduce them — the differential harness described above is what
//! produces them, and `tests/c_oracle.rs` is the in-repository opt-in harness that
//! builds the reference C library for the byte-identity (not throughput) sweep.
//! They also carry a host caveat: they were taken on a shared 4-CPU quota under a
//! load average near 45, where two harnesses timing the *same* operation disagreed
//! by 10%-30% purely from warm-up ordering, and where a 0.12 s measurement window
//! swung by ±30%. That is precisely why the method above fixes five interleaved
//! reps at 0.30 s and quotes medians. Treat the ratios as the position on one host
//! and re-measure before quoting them elsewhere. Every number a run of *this* file
//! prints describes how fast this crate turns compressed bytes back into payload
//! bytes, which is what makes it useful for comparing the crate against itself
//! across levels, profiles, and commits.
//!
//! # Every case validates its own output before it is timed
//!
//! A benchmark that checks only `Result::is_ok()` can report a perfectly
//! plausible throughput figure for wrong bytes, because `uncompress` returning
//! `Ok` says nothing about *what* it wrote. Every case below therefore runs one
//! UNTIMED decode outside `b.iter` and asserts both the exact recovered length
//! and byte-for-byte equality with the original payload. Only then is the timed
//! closure registered, and Criterion never folds that check into a sample.
//!
//! This folder MEASURES; it does not AUTHORISE. Performance is a constraint on the
//! migration, not its objective, so no timing taken here is on its own a licence to
//! change anything under `src/` (AAP 0.8.3) — least of all the compression
//! heuristics: a faster match finder that emits different tokens is a regression,
//! not an improvement, no matter what the benchmark says. In particular
//! `deflate_to_vec` below calls `compress2` purely as benchmark *setup*, to obtain
//! a stream for the decoder to consume; it is not a byte-identity check.
//! Byte-identity is owned exclusively by `tests/interop.rs`, and the official zlib
//! test vectors by `tests/regression.rs`, `tests/round_trip.rs`,
//! `tests/inflate_coverage.rs`, and `tests/gzip_compat.rs` — never by a benchmark.
//!
//! # Measurement configuration: noise threshold
//!
//! Criterion's default `noise_threshold` of 1% is well below what this workload
//! reproduces run to run on an unpinned, non-isolated CI-class host, so an
//! unchanged binary earns "Performance has regressed" purely from scheduling noise.
//! [`NOISE_THRESHOLD`] is therefore set above the observed variation of these
//! cases. The policy is shared with `benches/deflate_bench.rs` and
//! `benches/checksum_bench.rs`: pick a threshold that exceeds the largest
//! run-to-run drift seen for the workload on an unchanged binary — 0.05 where that
//! drift stays within a few percent, and 0.10 for the noisier band these decode
//! cases and the incompressible compression cases occupy. A group setting always
//! wins over the corresponding CLI flag, so `--noise-threshold` cannot override it.
//!
//! Sampling mode is deliberately left at Criterion's `Auto` default. Every case
//! here decodes 64 KiB in well under a millisecond, so Linear sampling keeps a
//! workable iteration step and retains the regression-slope estimate, which is the
//! more accurate one at this magnitude; `deflate_bench.rs` explains why its own
//! millisecond-scale cases had to switch to `Flat` instead. For criterion's own CLI
//! and reporting behaviour, consult the pinned harness's documentation
//! (`criterion = "0.5.1"`).
//!
//! Registered in `Cargo.toml` as `[[bench]] name = "inflate_bench"` with
//! `harness = false`. No `[[bench]]` block carries a `path` key, so Cargo
//! auto-discovers the target by filename: renaming this file breaks the build
//! outright.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zlib_rs::constants::{Z_DEFLATED, Z_FINISH};
use zlib_rs::deflate::{deflate, deflate_end, deflate_init2};
use zlib_rs::inflate::{InFunc, OutFunc, inflate_back, inflate_back_end, inflate_back_init};
use zlib_rs::{ReturnCode, Strategy, ZStream, compress_bound, compress2, uncompress};

/// Payload size used by the inflate benchmarks (64 KiB).
const SIZE: usize = 64 * 1024;

/// Criterion noise threshold for every case in this file.
///
/// Ten percent, set above the 5.25% coefficient of variation measured for these
/// `uncompress` cases so that a verdict carries information instead of reporting
/// scheduling noise as a regression. It remains far below the magnitude of any
/// optimisation worth landing, so genuine movement is still flagged. See the
/// module header, and `benches/deflate_bench.rs` for the full derivation.
const NOISE_THRESHOLD: f64 = 0.10;

/// Deterministic xorshift64 generator for incompressible, high-entropy input.
fn xorshift_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state >> 24) as u8);
    }
    out
}

/// Compressible, text-like data built by repeating an ASCII sentence.
fn text_like_bytes(len: usize) -> Vec<u8> {
    const SAMPLE: &[u8] = b"The quick brown fox jumps over the lazy dog. ";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = SAMPLE.len().min(len - out.len());
        out.extend_from_slice(&SAMPLE[..take]);
    }
    out
}

/// Highly repetitive data: a four-byte cycle, so the encoder emits long matches
/// at a **very short distance** and the decoder spends nearly all of its time in
/// the LZ77 self-referential copy.
///
/// This profile exists because it is the one the other two cannot reach. `text`
/// matches at distance 45 and `incompressible` barely matches at all, so both
/// exercise the decoder's copy path only at distances where a bulk copy is
/// trivially correct. A four-byte cycle forces distance 4 — inside the region
/// where the source and destination of the copy overlap — which is exactly the
/// path that was running at 76% of reference C zlib while no benchmark could see
/// it. Leaving this profile out is what allowed that to go unobserved.
fn repetitive_bytes(len: usize) -> Vec<u8> {
    const SAMPLE: &[u8] = b"ABCD";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = SAMPLE.len().min(len - out.len());
        out.extend_from_slice(&SAMPLE[..take]);
    }
    out
}

/// Compress `data` at `level` and return exactly the produced compressed bytes.
fn deflate_to_vec(data: &[u8], level: i32) -> Vec<u8> {
    let mut buf = vec![0u8; compress_bound(data.len())];
    let n = compress2(&mut buf, data, level).expect("setup: compress2 failed");
    buf.truncate(n);
    buf
}

/// Decodes `compressed` into `dest` and asserts byte-for-byte recovery of
/// `original`. This is the untimed self-check every case runs before Criterion
/// takes a single sample.
///
/// Two properties are pinned, and each one closes a way a benchmark can time the
/// wrong work:
///
/// 1. `uncompress` must SUCCEED. `compressed` was produced by `deflate_to_vec`
///    from `original` and `dest` is sized to `original.len()`, so an error here
///    is a library defect rather than a benchmark-setup problem.
/// 2. The decode must report exactly `original.len()` bytes and they must equal
///    `original`. A short or corrupted decode that happens to return `Ok` is
///    rejected here instead of being reported as throughput — checking the
///    `Result` discriminant alone would let it through.
///
/// `dest` is the same buffer the timed closure reuses, so a successful check also
/// proves the buffer is correctly sized for the case about to be measured.
fn assert_decodes_exactly(dest: &mut [u8], compressed: &[u8], original: &[u8], case: &str) {
    let produced = match uncompress(dest, compressed) {
        Ok(produced) => produced,
        Err(code) => panic!("{case}: uncompress failed: {code:?}"),
    };
    assert_eq!(
        produced,
        original.len(),
        "{case}: decoded to {produced} bytes, expected {}",
        original.len()
    );
    assert_eq!(
        &dest[..produced],
        original,
        "{case}: did not round-trip byte for byte"
    );
}

/// Decompression throughput as a function of the source compression level.
fn bench_by_level(c: &mut Criterion) {
    let original = text_like_bytes(SIZE);
    let orig_len = original.len();

    let mut group = c.benchmark_group("inflate_by_level");
    group.throughput(Throughput::Bytes(orig_len as u64));
    // Raise the verdict threshold off Criterion's unusable 1% default; sampling
    // mode stays `Auto` because these cases are sub-millisecond (module header).
    group.noise_threshold(NOISE_THRESHOLD);
    for level in [1i32, 6, 9] {
        let compressed = deflate_to_vec(&original, level);
        group.bench_with_input(
            BenchmarkId::from_parameter(level),
            &compressed,
            |b, compressed| {
                let mut dest = vec![0u8; orig_len];
                // Untimed self-check: the decode must recover the exact original
                // bytes before a single sample is taken.
                assert_decodes_exactly(
                    &mut dest,
                    compressed,
                    &original,
                    &format!("inflate_by_level/source level {level}"),
                );
                b.iter(|| {
                    let produced = uncompress(&mut dest, compressed).expect("uncompress failed");
                    black_box(produced);
                });
            },
        );
    }
    group.finish();
}

/// Compress `data` at `level` with **raw** DEFLATE framing (no zlib header or
/// trailer), which is the only framing [`inflate_back`] accepts.
///
/// Benchmark setup only, exactly like [`deflate_to_vec`]: it produces a stream for
/// the decoder to consume and is not a byte-identity check.
fn raw_deflate_to_vec(data: &[u8], level: i32) -> Vec<u8> {
    let mut strm = ZStream::new();
    deflate_init2(&mut strm, level, Z_DEFLATED, -15, 8, Strategy::Default)
        .expect("setup: raw deflate_init2 must succeed");

    let mut out = vec![0u8; compress_bound(data.len()) + 128];
    let produced = {
        let outcome = deflate(&mut strm, data, &mut out, Z_FINISH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "setup: raw deflate(Z_FINISH) must finish in one pass"
        );
        assert_eq!(
            outcome.consumed,
            data.len(),
            "setup: raw deflate must consume the whole payload"
        );
        outcome.produced
    };
    deflate_end(&mut strm).expect("setup: deflate_end must succeed");
    out.truncate(produced);
    out
}

/// A single-chunk [`InFunc`] over one borrowed slice — the benchmark analogue of
/// `test/infcover.c`'s `pull` callback.
struct SliceIn<'a> {
    data: &'a [u8],
    served: bool,
}

impl<'a> InFunc for SliceIn<'a> {
    fn advance(&mut self) -> bool {
        if self.served {
            return false;
        }
        self.served = true;
        true
    }

    fn chunk(&self) -> &[u8] {
        if self.served { self.data } else { &[] }
    }
}

/// An [`OutFunc`] that only counts, so the timed loop measures decoding rather
/// than the cost of growing a `Vec`.
struct CountOut {
    written: usize,
}

impl OutFunc for CountOut {
    fn write_output(&mut self, buf: &[u8]) -> Result<(), ()> {
        self.written += buf.len();
        Ok(())
    }
}

/// An [`OutFunc`] that accumulates, used only by the untimed self-check.
struct VecOut {
    bytes: Vec<u8>,
}

impl OutFunc for VecOut {
    fn write_output(&mut self, buf: &[u8]) -> Result<(), ()> {
        self.bytes.extend_from_slice(buf);
        Ok(())
    }
}

/// `inflateBack` throughput, per input profile, over raw DEFLATE streams.
///
/// # Why this case exists
///
/// `inflateBack` is a *separate decoder* from [`uncompress`]: `infback.c` is its
/// own translation unit with its own decode loop, and it is what `gzip`-style
/// consumers use because it decodes into a caller-managed window with no
/// intermediate output buffer. Nothing in the rest of this file exercises it — the
/// three `uncompress` groups all drive `inflate.c`'s mode loop instead — so a
/// regression confined to `inflateBack` was invisible to the whole suite. One was:
/// the engine had no batched decode path at all and ran at 12%-19% of reference C
/// zlib on compressible input. This group is what makes that observable.
///
/// Throughput is reported over the *decompressed* size, matching the rest of the
/// file, and each case runs the same untimed byte-for-byte self-check before
/// Criterion takes a sample.
fn bench_inflate_back(c: &mut Criterion) {
    let profiles: [(&str, Vec<u8>); 3] = [
        ("text", text_like_bytes(SIZE)),
        ("repetitive", repetitive_bytes(SIZE)),
        ("incompressible", xorshift_bytes(SIZE, 0x1357_9BDF)),
    ];

    let mut group = c.benchmark_group("inflate_back_by_profile");
    // Same policy as the `uncompress` groups (module header).
    group.noise_threshold(NOISE_THRESHOLD);
    for (name, original) in &profiles {
        let orig_len = original.len();
        let compressed = raw_deflate_to_vec(original, 6);
        group.throughput(Throughput::Bytes(orig_len as u64));
        group.bench_with_input(
            BenchmarkId::new("level6", *name),
            &compressed,
            |b, compressed| {
                // Untimed self-check: `inflateBack` must recover the exact
                // original bytes before a single sample is taken. A decoder that
                // returns `StreamEnd` having written the wrong bytes would
                // otherwise be reported as throughput.
                {
                    let mut state = inflate_back_init(15).expect("inflate_back_init failed");
                    let mut src = SliceIn {
                        data: compressed,
                        served: false,
                    };
                    let mut sink = VecOut {
                        bytes: Vec::with_capacity(orig_len),
                    };
                    let outcome = inflate_back(&mut state, &mut src, &mut sink);
                    assert_eq!(
                        outcome.code,
                        ReturnCode::StreamEnd,
                        "inflate_back_by_profile/level6/{name}: inflate_back returned {:?}",
                        outcome.code
                    );
                    assert_eq!(
                        sink.bytes.len(),
                        orig_len,
                        "inflate_back_by_profile/level6/{name}: decoded to {} bytes, expected {orig_len}",
                        sink.bytes.len()
                    );
                    assert_eq!(
                        &sink.bytes[..],
                        &original[..],
                        "inflate_back_by_profile/level6/{name}: did not round-trip byte for byte"
                    );
                    inflate_back_end(state);
                }
                b.iter(|| {
                    // The window is part of the engine state, so a fresh state per
                    // sample is what C requires too (`inflateBackInit_` allocates
                    // the window; `inflateBack` consumes one whole stream).
                    let mut state = inflate_back_init(15).expect("inflate_back_init failed");
                    let mut src = SliceIn {
                        data: compressed,
                        served: false,
                    };
                    let mut sink = CountOut { written: 0 };
                    let outcome = inflate_back(&mut state, &mut src, &mut sink);
                    debug_assert_eq!(outcome.code, ReturnCode::StreamEnd);
                    let written = sink.written;
                    inflate_back_end(state);
                    black_box(written);
                });
            },
        );
    }
    group.finish();
}

/// Decompression throughput as a function of the input profile (source level 6).
fn bench_by_profile(c: &mut Criterion) {
    let profiles: [(&str, Vec<u8>); 3] = [
        ("text", text_like_bytes(SIZE)),
        ("repetitive", repetitive_bytes(SIZE)),
        ("incompressible", xorshift_bytes(SIZE, 0x1357_9BDF)),
    ];

    let mut group = c.benchmark_group("inflate_by_profile");
    // Same policy as `bench_by_level`: raise the verdict threshold off
    // Criterion's 1% default, leave sampling mode at `Auto` (module header).
    group.noise_threshold(NOISE_THRESHOLD);
    for (name, original) in &profiles {
        let orig_len = original.len();
        let compressed = deflate_to_vec(original, 6);
        group.throughput(Throughput::Bytes(orig_len as u64));
        group.bench_with_input(
            BenchmarkId::new("level6", *name),
            &compressed,
            |b, compressed| {
                let mut dest = vec![0u8; orig_len];
                // Same untimed self-check as `bench_by_level`.
                assert_decodes_exactly(
                    &mut dest,
                    compressed,
                    original,
                    &format!("inflate_by_profile/level6/{name}"),
                );
                b.iter(|| {
                    let produced = uncompress(&mut dest, compressed).expect("uncompress failed");
                    black_box(produced);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_by_level,
    bench_by_profile,
    bench_inflate_back
);
criterion_main!(benches);
