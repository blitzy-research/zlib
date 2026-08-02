//! Criterion throughput benchmarks for the `zlib-rs` inflate (decompression) engine.
//!
//! Pre-compresses representative inputs with `compress2`, then measures
//! `uncompress` throughput. Throughput is reported over the *decompressed*
//! (original) size, which is the meaningful denominator for a decoder: the same
//! corpus compresses to a different length at every level, so normalising by the
//! compressed size would make two source levels that decode to identical output
//! incomparable. Decompression cost can vary with how the source was compressed,
//! so both the source level (1 / 6 / 9) and the input profile (text vs.
//! incompressible) are varied. The larger, match-heavy inputs drive the
//! `inflate_fast` hot path.
//!
//! Measured position, recorded as external context rather than produced here:
//! decompression runs at 107%-127% of reference C zlib, so it is at or above
//! parity, while compression runs at approximately 85% of it (AAP 0.8.3,
//! "Performance Expectations"). Compression is measured separately in
//! `benches/deflate_bench.rs`; nothing timed here is a compression figure. A
//! per-profile comparison against a reference C build put these decode cases at
//! 104%-125%, which brackets the quoted range.
//!
//! This file links no C library and runs no reference implementation, and the
//! repository has no automated in-tree performance oracle that could re-check the
//! percentages above on demand — treat them as attributed context rather than as a
//! property this suite verifies. Every number a run prints describes how fast this
//! crate turns compressed bytes back into payload bytes, useful for comparing the
//! crate against itself across levels, profiles, and commits.
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
use zlib_rs::{compress_bound, compress2, uncompress};

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

/// Decompression throughput as a function of the input profile (source level 6).
fn bench_by_profile(c: &mut Criterion) {
    let profiles: [(&str, Vec<u8>); 2] = [
        ("text", text_like_bytes(SIZE)),
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

criterion_group!(benches, bench_by_level, bench_by_profile);
criterion_main!(benches);
