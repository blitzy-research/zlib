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
//! Measured position, and the only throughput figures quoted anywhere in this
//! file: decompression runs at 107%-127% of reference C zlib, so it is at or
//! above parity, while compression runs at approximately 85% of it (AAP 0.8.3,
//! "Performance Expectations"). Compression is therefore the interesting side,
//! and it is measured separately in `benches/deflate_bench.rs`; nothing timed
//! here is a compression figure.
//!
//! This folder MEASURES; it does not AUTHORISE. Performance is a constraint on
//! the migration, not its objective, so no timing taken here is on its own a
//! licence to change anything under `src/` (AAP 0.8.3). In particular
//! `deflate_to_vec` below calls `compress2` purely as benchmark *setup*, to
//! obtain a stream for the decoder to consume: it is not a byte-identity check,
//! and byte-identity against reference zlib is owned exclusively by
//! `tests/interop.rs`.
//!
//! Registered in `Cargo.toml` as `[[bench]] name = "inflate_bench"` with
//! `harness = false`. No `[[bench]]` block carries a `path` key, so Cargo
//! auto-discovers the target by filename: renaming this file breaks the build
//! outright.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zlib_rs::{compress_bound, compress2, uncompress};

/// Payload size used by the inflate benchmarks (64 KiB).
const SIZE: usize = 64 * 1024;

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

/// Decompression throughput as a function of the source compression level.
fn bench_by_level(c: &mut Criterion) {
    let original = text_like_bytes(SIZE);
    let orig_len = original.len();

    let mut group = c.benchmark_group("inflate_by_level");
    group.throughput(Throughput::Bytes(orig_len as u64));
    for level in [1i32, 6, 9] {
        let compressed = deflate_to_vec(&original, level);
        group.bench_with_input(
            BenchmarkId::from_parameter(level),
            &compressed,
            |b, compressed| {
                let mut dest = vec![0u8; orig_len];
                assert!(
                    uncompress(&mut dest, compressed).is_ok(),
                    "uncompress failed for source level {level}"
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
    for (name, original) in &profiles {
        let orig_len = original.len();
        let compressed = deflate_to_vec(original, 6);
        group.throughput(Throughput::Bytes(orig_len as u64));
        group.bench_with_input(
            BenchmarkId::new("level6", *name),
            &compressed,
            |b, compressed| {
                let mut dest = vec![0u8; orig_len];
                assert!(
                    uncompress(&mut dest, compressed).is_ok(),
                    "uncompress failed for profile {name}"
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
