//! Criterion throughput benchmarks for the `zlib-rs` deflate (compression) engine.
//!
//! Exercises the one-call `compress2` API across all ten compression levels
//! (`0..=9`) and across several input profiles (compressible text,
//! incompressible high-entropy bytes, and highly repetitive data). Throughput is
//! reported over the *uncompressed* input size, which is the meaningful
//! denominator for an encoder: the compressed length is an *output* of the
//! measurement rather than a property of the workload, so normalising by it
//! would make two levels on the same input incomparable.
//!
//! Measured position, and the only throughput figures quoted anywhere in this
//! file: compression runs at approximately 85% of reference C zlib, while
//! decompression runs at 107%-127% of it (AAP 0.8.3, "Performance
//! Expectations"). Decompression is therefore at or above parity and
//! compression is the interesting side; its shortfall is concentrated in the
//! incompressible-input path, where the match finder does the most fruitless
//! work. Keeping that specific number honest is what `bench_incompressible_guard`
//! below exists for.
//!
//! This folder MEASURES; it does not AUTHORISE. Performance is a constraint on
//! the migration, not its objective, so no timing taken here is on its own a
//! licence to change anything under `src/` (AAP 0.8.3). Byte-identity against
//! reference zlib is owned exclusively by `tests/interop.rs`; the full
//! obligation is spelled out on `bench_incompressible_guard`.
//!
//! Strategy scope: `compress2` selects only the compression *level*. Strategy
//! selection (`Z_FILTERED`, `Z_HUFFMAN_ONLY`, `Z_RLE`, `Z_FIXED`) is reached
//! through the streaming API, and that surface is public and stable today —
//! `zlib_rs::deflate` exports `deflate_init2`, `deflate`, `deflate_params`, and
//! `deflate_end`, which `tests/interop.rs` already drives to build the raw and
//! gzip framings. Strategy sweeps are nonetheless deliberately OUT OF SCOPE for
//! this file: AAP 0.4.1.8 asks it for the ten-level sweep plus an
//! incompressible-input profile, and for nothing beyond that. The distinct data
//! profiles below already stress the match-finder behavior the strategies
//! target.
//!
//! Registered in `Cargo.toml` as `[[bench]] name = "deflate_bench"` with
//! `harness = false`. No `[[bench]]` block carries a `path` key, so Cargo
//! auto-discovers the target by filename: renaming this file breaks the build
//! outright.

// Both entry points come from the crate root, which is the only sanctioned
// import surface for a consumer of this crate (AAP 0.5.2). Deeper module paths
// also resolve for these two names, but they are an implementation detail rather
// than API and must not be relied on; the C-ABI shims are deliberately not
// re-exported at the root at all, and a benchmark never reaches for them.
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zlib_rs::{compress_bound, compress2};

/// Payload size used by the deflate benchmarks (64 KiB).
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

/// Highly repetitive data built from a short repeating cycle.
fn repetitive_bytes(len: usize) -> Vec<u8> {
    const CYCLE: &[u8] = b"ABCD";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = CYCLE.len().min(len - out.len());
        out.extend_from_slice(&CYCLE[..take]);
    }
    out
}

/// Compression throughput across all ten levels (`0..=9`) on compressible text.
fn bench_levels(c: &mut Criterion) {
    let data = text_like_bytes(SIZE);
    let bound = compress_bound(data.len());

    let mut group = c.benchmark_group("deflate_levels");
    group.throughput(Throughput::Bytes(data.len() as u64));
    for level in 0..=9 {
        group.bench_with_input(BenchmarkId::from_parameter(level), &level, |b, &level| {
            let mut dest = vec![0u8; bound];
            // Sanity: compression must succeed before we measure it.
            assert!(
                compress2(&mut dest, &data, level).is_ok(),
                "compress2 failed at level {level}"
            );
            b.iter(|| {
                let written = compress2(&mut dest, &data, level).expect("compress2 failed");
                black_box(written);
            });
        });
    }
    group.finish();
}

/// Compression throughput across input profiles at the default level (6).
fn bench_profiles(c: &mut Criterion) {
    const LEVEL: i32 = 6;
    let profiles: [(&str, Vec<u8>); 3] = [
        ("text", text_like_bytes(SIZE)),
        ("incompressible", xorshift_bytes(SIZE, 0xDEAD_BEEF)),
        ("repetitive", repetitive_bytes(SIZE)),
    ];

    let mut group = c.benchmark_group("deflate_profiles");
    for (name, data) in &profiles {
        let bound = compress_bound(data.len());
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::new("level6", *name), data, |b, data| {
            let mut dest = vec![0u8; bound];
            assert!(
                compress2(&mut dest, data, LEVEL).is_ok(),
                "compress2 failed for profile {name}"
            );
            b.iter(|| {
                let written = compress2(&mut dest, data, LEVEL).expect("compress2 failed");
                black_box(written);
            });
        });
    }
    group.finish();
}

/// Compression throughput on incompressible input — the migration's known worst
/// case, and this file's regression guard.
///
/// WHAT IT MEASURES. The high-entropy `xorshift_bytes` corpus at levels 1, 6,
/// and 9: least effort, the default, and most effort. Three levels rather than
/// ten deliberately — `deflate_levels` above already sweeps all ten, and these
/// are the same three source levels `benches/inflate_bench.rs` uses, so the two
/// files stay directly comparable. Level 6 overlaps
/// `deflate_profiles/level6/incompressible` on purpose: bracketing the default
/// means a change that helps one end of the level range at the other's expense
/// cannot hide behind a single data point. The seed and size are fixed constants
/// for the same reason the corpus generators take no `rand` dependency —
/// determinism is what makes a run-to-run comparison mean anything, and a
/// randomly seeded corpus would make this guard useless.
///
/// WHY IT EXISTS. Compression measures approximately 85% of reference C zlib
/// throughput, and that shortfall is concentrated precisely here, in the
/// incompressible path where the match finder does the most fruitless work
/// (AAP 0.8.3). This group is that gap's instrument: it exists so future tuning
/// is MEASURED rather than INFERRED, and so a reviewer weighing a tuning
/// proposal has one stable, self-describing bench id —
/// `deflate_incompressible_guard/<level>` — to point at and demand numbers for.
///
/// WHAT A FAVORABLE RESULT HERE DOES NOT BUY. It does not authorise a heuristic
/// change. A faster match finder that emits different tokens is a REGRESSION,
/// not an improvement, no matter what this benchmark says (AAP 0.8.3). The very
/// heuristics that cost throughput are the ones that determine the emitted
/// bytes: the chain-length halving at `good_match`, the `nice_match` early
/// break, and the `TOO_FAR` lazy-match filter. Any candidate speed-up must
/// therefore be validated against the byte-identity gate in `tests/interop.rs`,
/// which owns that property exclusively, BEFORE it is considered viable.
/// Permissible optimisation is limited to work that provably cannot change the
/// token stream: bounds-check elision, memory-access patterns, inlining, and
/// buffer-copy strategy. A number produced by this function is evidence, never
/// permission.
fn bench_incompressible_guard(c: &mut Criterion) {
    let data = xorshift_bytes(SIZE, 0xDEAD_BEEF);
    let bound = compress_bound(data.len());

    let mut group = c.benchmark_group("deflate_incompressible_guard");
    // Throughput over the *uncompressed* length, consistent with the groups
    // above: incompressible input barely shrinks, so the compressed size carries
    // no useful signal here.
    group.throughput(Throughput::Bytes(data.len() as u64));
    for level in [1i32, 6, 9] {
        group.bench_with_input(BenchmarkId::from_parameter(level), &level, |b, &level| {
            let mut dest = vec![0u8; bound];
            // Sanity: compression must succeed before we measure it, so the
            // guard can never report a fast failure as a fast encode.
            assert!(
                compress2(&mut dest, &data, level).is_ok(),
                "compress2 failed on incompressible input at level {level}"
            );
            b.iter(|| {
                let written = compress2(&mut dest, &data, level).expect("compress2 failed");
                black_box(written);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_levels,
    bench_profiles,
    bench_incompressible_guard
);
criterion_main!(benches);
