//! Criterion throughput benchmarks for the `zlib-rs` checksum functions.
//!
//! Measures Adler-32 and CRC-32 throughput across a range of buffer sizes.
//!
//! Selecting the CRC-32 code path: `simd` is a member of the crate's `default`
//! feature set (`default = ["std", "gzip", "gz-io", "simd"]`), so a plain
//! `cargo bench` already measures the `crc32fast`-backed SIMD hot path, and
//! adding `--features simd` on top of the defaults resolves to the identical
//! feature set — it rebuilds the same binary and compares nothing. Reaching the
//! scalar fallback therefore requires `--no-default-features`. The two commands
//! below are the "gzip + gz-io" and "simd" rows of the `build-test` matrix in
//! `.github/workflows/ci.yml`: they differ in exactly that one feature, and both
//! are built and tested on every push.
//!
//! - scalar fallback — the braided, word-at-a-time port, with no `crc32fast` in
//!   the dependency graph:
//!   `cargo bench --bench checksum_bench --no-default-features --features std,gzip,gz-io`
//! - SIMD-accelerated — the `crc32fast` hot path:
//!   `cargo bench --bench checksum_bench --no-default-features --features std,gzip,gz-io,simd`
//!
//! Run them in that order and criterion reports the second against the first,
//! because the group ids below are stable. Both paths return bit-identical
//! checksums for every input, so only the timing differs; no figure for that
//! difference is quoted anywhere in this file, because it is CPU- and
//! build-dependent and the re-runnable commands above are the evidence rather
//! than a constant baked into a comment (plan-adopted standard S1, AAP 0.7.2).
//! A benchmark result is likewise never on its own a licence to change `src/`
//! (AAP 0.8.3): this folder measures, it does not authorise.
//!
//! Registered in `Cargo.toml` as `[[bench]] name = "checksum_bench"` with
//! `harness = false`, so this file supplies its own entry point via
//! `criterion_group!` / `criterion_main!` (not the libtest harness). No
//! `[[bench]]` block carries a `path` key, so Cargo auto-discovers the target by
//! filename: renaming this file breaks the build outright.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zlib_rs::{adler32, crc32};

/// Buffer sizes exercised by every checksum benchmark: 1 KiB, 16 KiB, 256 KiB.
const SIZES: [usize; 3] = [1 << 10, 1 << 14, 1 << 18];

/// Deterministic, dependency-free pseudo-random byte generator (xorshift64).
/// Produces high-entropy (incompressible-looking) data so the checksum loop is
/// exercised on realistic input without pulling in the `rand` crate.
fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
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

fn bench_adler32(c: &mut Criterion) {
    // Correctness guard using the canonical zlib Adler-32 identity vector.
    assert_eq!(adler32(1, b""), 1, "adler32 identity vector");

    let mut group = c.benchmark_group("adler32");
    for &size in &SIZES {
        let data = pseudo_random_bytes(size, 0x51ED_5EED);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter(|| black_box(adler32(1, black_box(data.as_slice()))));
        });
    }
    group.finish();
}

fn bench_crc32(c: &mut Criterion) {
    // Correctness guard using the canonical "123456789" CRC-32 check value.
    assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926, "crc32 check vector");

    let mut group = c.benchmark_group("crc32");
    for &size in &SIZES {
        let data = pseudo_random_bytes(size, 0xC0FF_EE00);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter(|| black_box(crc32(0, black_box(data.as_slice()))));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_adler32, bench_crc32);
criterion_main!(benches);
