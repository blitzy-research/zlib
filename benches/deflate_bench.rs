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
//! Measured position, recorded as external context rather than produced here:
//! compression runs at approximately 85% of reference C zlib and decompression at
//! 107%-127% of it (AAP 0.8.3, "Performance Expectations"). Those aggregates do
//! not localise the compression shortfall, and a per-profile measurement against a
//! reference C build places it opposite to where the aggregate invites one to look:
//! the incompressible profile is the CLOSEST to C at roughly 82%-86%, while
//! compressible profiles are the FURTHEST at roughly 58%-64%. On incompressible
//! input the match finder fails FAST rather than working hard — `longest_match`'s
//! two-byte prefilter rejects almost every candidate before the comparison loop,
//! and `_tr_flush_block` then selects stored blocks because the dynamic tree cannot
//! pay for itself — so both implementations do similar and rather little work per
//! byte. The gap opens on compressible input, where hash chains are genuinely
//! walked, lazy matching is evaluated, and Huffman trees are built and emitted.
//!
//! Treat every C-relative percentage in this file as provisional. This harness
//! links no C library, runs no reference implementation, and the repository has no
//! automated in-tree performance oracle, so a C-relative claim made here cannot be
//! re-checked on demand — which is why the numbers above are attributed rather than
//! asserted. Every figure a run of this file prints is a Rust-only
//! level-and-profile throughput number, useful for comparing this crate against
//! itself across levels, inputs and commits.
//!
//! This folder MEASURES; it does not AUTHORISE. Performance is a constraint on the
//! migration, not its objective, so no timing taken here is on its own a licence to
//! change anything under `src/` (AAP 0.8.3). A candidate speed-up is viable only if
//! it provably cannot change the token stream — bounds-check elision,
//! memory-access patterns, inlining, and buffer-copy strategy — and only after
//! clearing the byte-identity gate, which is owned exclusively by the tier-1
//! oracle vectors in `tests/interop.rs`. The full obligation is spelled out on
//! `bench_incompressible_guard`.
//!
//! # Every case validates its own output before it is timed
//!
//! A benchmark that checks only `Result::is_ok()` can report a perfectly
//! plausible throughput figure for wrong bytes, because `compress2` returning
//! `Ok` says nothing about *what* it wrote. Every case below therefore runs one
//! UNTIMED compression outside `b.iter`, truncates the destination to the
//! produced length, decodes exactly those bytes with `uncompress`, and asserts
//! both the exact recovered length and byte-for-byte equality with the input.
//! Only then is the timed closure registered. The check costs one compression
//! plus one decompression per case and Criterion never folds it into a sample.
//!
//! Strategy scope: `compress2` selects only the compression *level*. Strategy
//! selection (`Z_FILTERED`, `Z_HUFFMAN_ONLY`, `Z_RLE`, `Z_FIXED`) is reached
//! through the streaming API and is deliberately OUT OF SCOPE here — AAP 0.4.1.8
//! asks this file for the ten-level sweep plus an incompressible-input profile and
//! nothing beyond that, and the distinct data profiles below already stress the
//! match-finder behaviour the strategies target.
//!
//! # Measurement configuration: sampling mode and noise threshold
//!
//! Two Criterion defaults are wrong for this workload and are overridden below.
//!
//! **Sampling mode.** The millisecond-scale cases (the incompressible 64 KiB
//! profiles) use `SamplingMode::Flat`. Criterion's default `Auto` picks Linear,
//! whose per-sample iteration step collapses to 1 at that magnitude; the run then
//! costs `total_runs * met` regardless of the stated budget and emits
//! `Warning: Unable to complete 100 samples`. Raising `measurement_time` does not
//! fix it — the warning-free window spans only a factor of 2 in the per-iteration
//! time, so a modestly slower host falls straight back out of it. Flat holds the
//! iteration count constant, finishes inside the budget, and keeps all 100
//! samples. The only thing it gives up is the regression slope, which exists to
//! cancel timer overhead of order tens of nanoseconds and is therefore irrelevant
//! against a millisecond iteration; every other statistic and the entire
//! change-detection comparison are computed identically. The sub-millisecond cases
//! keep `Auto`, where the slope genuinely is the better estimator.
//!
//! **Noise threshold.** Criterion's default of 1% is below what this workload
//! reproduces run to run on an unpinned, non-isolated CI-class host, where two
//! runs of a bit-identical binary can differ by several percent and CPU contention
//! by far more. `NOISE_THRESHOLD_FAST` and `NOISE_THRESHOLD_SLOW` below are
//! therefore set above the observed spread of the cases they apply to — high
//! enough to stop reporting noise as a verdict, and far below the magnitude of any
//! optimisation worth taking. A verdict is still only a hint: a real before/after
//! claim needs repeated runs with `--save-baseline` / `--baseline`.
//!
//! A group setting always wins over the corresponding Criterion CLI flag, so
//! `--sample-size`, `--measurement-time` and `--noise-threshold` cannot override
//! the values configured below; change the constants if the policy is wrong. For
//! criterion's own CLI and reporting behaviour, consult the pinned harness's
//! documentation (`criterion = "0.5.1"`).
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
use criterion::{
    BenchmarkId, Criterion, SamplingMode, Throughput, black_box, criterion_group, criterion_main,
};
use zlib_rs::{compress_bound, compress2, uncompress};

/// Payload size used by the deflate benchmarks (64 KiB).
const SIZE: usize = 64 * 1024;

/// Name of the high-entropy profile in [`bench_profiles`].
///
/// Held as a constant because it is used twice — once to build the profile list
/// and once to decide that profile's measurement policy — and the two uses must
/// not be allowed to drift apart. If they did, the long-running case would
/// silently fall back to Criterion's defaults and reinstate its "Unable to
/// complete 100 samples" warning.
const INCOMPRESSIBLE_PROFILE: &str = "incompressible";

/// Noise threshold for the sub-millisecond cases: every `deflate_levels/*` id
/// and the `text` and `repetitive` members of `deflate_profiles`.
///
/// Five percent, chosen to sit above the 1.49%-2.08% coefficient of variation
/// measured for these cases with headroom for a moderately loaded host, and
/// matching the value `benches/checksum_bench.rs` uses for the same reason. See
/// the module header for the full derivation and for why Criterion's 0.01
/// default is unusable here.
const NOISE_THRESHOLD_FAST: f64 = 0.05;

/// Noise threshold for the ~1.8 ms incompressible cases:
/// `deflate_incompressible_guard/*` and `deflate_profiles/level6/incompressible`.
///
/// Ten percent, chosen to sit above the worst spread actually reproduced on an
/// unchanged binary for these ids — 7.81% across five isolated repeats at level
/// 6, against 3.30% at level 1 — so that a verdict from this group means
/// something. It is still far below the magnitude of any optimisation worth
/// landing, so real movement is not masked. See the module header.
const NOISE_THRESHOLD_SLOW: f64 = 0.10;

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

/// Compresses `data` at `level` into `dest`, then decodes the produced bytes and
/// asserts byte-for-byte recovery. This is the untimed self-check every case
/// runs before Criterion takes a single sample.
///
/// Three properties are pinned, and each one closes a way a benchmark can time
/// the wrong work:
///
/// 1. `compress2` must SUCCEED. `dest` is sized by `compress_bound`, which is
///    zlib's own worst-case bound, and `level` is always in range, so an error
///    here is a library defect rather than a benchmark-setup problem.
/// 2. Only the `written` prefix of `dest` is decoded. Handing `uncompress` the
///    whole buffer would let leftover scratch bytes stand in for real output.
/// 3. The decode must return exactly `data.len()` bytes and they must equal
///    `data`. A truncated or corrupted stream that happens to decode to
///    *something* is rejected here rather than being reported as throughput.
///
/// `dest` is the same buffer the timed closure reuses, so a successful check
/// also proves the buffer is large enough for the level about to be measured.
fn assert_compresses_exactly(dest: &mut [u8], data: &[u8], level: i32, case: &str) {
    let written = match compress2(dest, data, level) {
        Ok(written) => written,
        Err(code) => panic!("{case}: compress2 failed at level {level}: {code:?}"),
    };

    let mut restored = vec![0u8; data.len()];
    let produced = match uncompress(&mut restored, &dest[..written]) {
        Ok(produced) => produced,
        Err(code) => panic!(
            "{case}: the {written}-byte stream compress2 produced at level {level} \
             did not decode: {code:?}"
        ),
    };

    assert_eq!(
        produced,
        data.len(),
        "{case}: level {level} decoded to {produced} bytes, expected {}",
        data.len()
    );
    assert_eq!(
        &restored[..produced],
        data,
        "{case}: level {level} did not round-trip byte for byte"
    );
}

/// Compression throughput across all ten levels (`0..=9`) on compressible text.
fn bench_levels(c: &mut Criterion) {
    let data = text_like_bytes(SIZE);
    let bound = compress_bound(data.len());

    let mut group = c.benchmark_group("deflate_levels");
    group.throughput(Throughput::Bytes(data.len() as u64));
    // Every case in this group is sub-millisecond on 64 KiB of text, so Linear
    // sampling reaches d >= 1 without warning and its slope estimate is the more
    // accurate one: sampling mode is deliberately left at the `Auto` default.
    // Only the noise threshold is raised off Criterion's unusable 1% (module
    // header, "Noise threshold").
    group.noise_threshold(NOISE_THRESHOLD_FAST);
    for level in 0..=9 {
        group.bench_with_input(BenchmarkId::from_parameter(level), &level, |b, &level| {
            let mut dest = vec![0u8; bound];
            // Untimed self-check: the stream must succeed AND decode back to the
            // exact input before a single sample is taken.
            assert_compresses_exactly(&mut dest, &data, level, "deflate_levels");
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
        (INCOMPRESSIBLE_PROFILE, xorshift_bytes(SIZE, 0xDEAD_BEEF)),
        ("repetitive", repetitive_bytes(SIZE)),
    ];

    let mut group = c.benchmark_group("deflate_profiles");
    for (name, data) in &profiles {
        let bound = compress_bound(data.len());
        group.throughput(Throughput::Bytes(data.len() as u64));

        // Per-case measurement policy. This group is heterogeneous: `text` and
        // `repetitive` compress in well under a millisecond, while
        // `incompressible` sits at ~1.8 ms and is the one case in this group that
        // triggered Criterion's "Unable to complete 100 samples" warning and
        // carried the wider run-to-run spread.
        //
        // Criterion snapshots the group configuration when each case is
        // REGISTERED — `run_bench` calls `partial_config.to_complete(..)` per
        // case, in criterion 0.5.1's own `src/benchmark_group.rs` rather than any
        // file in this repository — so a setting applied here binds only to the
        // case registered on the next statement. That is what makes a
        // per-case policy possible, but it also means the settings PERSIST into
        // the following iteration: `incompressible` is registered second, so
        // leaving the fast arm implicit would silently leak Flat sampling and the
        // wider threshold into `repetitive`. Both arms are therefore explicit.
        let long_running = *name == INCOMPRESSIBLE_PROFILE;
        group.sampling_mode(if long_running {
            SamplingMode::Flat
        } else {
            SamplingMode::Auto
        });
        group.noise_threshold(if long_running {
            NOISE_THRESHOLD_SLOW
        } else {
            NOISE_THRESHOLD_FAST
        });

        group.bench_with_input(BenchmarkId::new("level6", *name), data, |b, data| {
            let mut dest = vec![0u8; bound];
            // Same untimed self-check as `bench_levels`: succeed, then decode
            // back to the exact profile bytes.
            assert_compresses_exactly(&mut dest, data, LEVEL, name);
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
/// throughput in aggregate (AAP 0.8.3). This group does NOT exist because the
/// shortfall is concentrated here — a per-profile measurement showed the
/// opposite, with incompressible input the closest to C at roughly 82%-86% and
/// the compressible profiles the furthest at roughly 58%-64% (module header,
/// "Measured position"). It exists because incompressible input is the WORST-CASE
/// WORKLOAD in absolute terms: it is the slowest thing this encoder does per
/// input byte, measured at roughly 34 MiB/s against roughly 170 MiB/s for the
/// text and repetitive profiles at the same level — a factor of five — so it is
/// where a tuning change has the most room to help and the most room to silently
/// break something. Bracketing it at levels 1, 6, and 9 gives future
/// tuning a MEASURED rather than INFERRED basis, and gives a reviewer weighing a
/// proposal three stable, self-describing bench ids —
/// `deflate_incompressible_guard/<level>` — to point at and demand numbers for.
/// Note that the numbers it produces are Rust-only: this file links no C library,
/// so the group tracks this crate against itself across commits, not against
/// zlib.
///
/// WHAT A FAVORABLE RESULT HERE DOES NOT BUY. It does not authorise a heuristic
/// change. A faster match finder that emits different tokens is a REGRESSION,
/// not an improvement, no matter what this benchmark says (AAP 0.8.3). The very
/// heuristics that cost throughput are the ones that determine the emitted
/// bytes: the chain-length quartering at `good_match` (`chain_length >>= 2`, at
/// `deflate.c` L1424 and `src/deflate/state.rs`), the `nice_match` early break,
/// and the `TOO_FAR` lazy-match filter. Any candidate speed-up must
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
    // Every id in this group runs at ~1.8 ms per iteration, which is precisely
    // the regime where Linear sampling computes d == 1, prints "Unable to
    // complete 100 samples in 5.0s", and then overruns to ~9.1 s anyway. Flat
    // keeps all 100 samples, finishes inside the 5 s budget at ~28 iterations per
    // sample, and would not warn until a single iteration reached 50 ms. The only
    // statistic forfeited is the regression slope, which is worth nothing at this
    // magnitude. Full derivation in the module header.
    group.sampling_mode(SamplingMode::Flat);
    // This guard's whole purpose is to make tuning claims falsifiable, so its
    // verdicts have to mean something: the threshold sits above the 7.81% spread
    // reproduced here on an unchanged binary rather than at Criterion's 1%.
    group.noise_threshold(NOISE_THRESHOLD_SLOW);
    for level in [1i32, 6, 9] {
        group.bench_with_input(BenchmarkId::from_parameter(level), &level, |b, &level| {
            let mut dest = vec![0u8; bound];
            // Incompressible input is exactly where a silently wrong encoder
            // would look fastest, so the untimed decode-and-compare matters most
            // in this group.
            assert_compresses_exactly(&mut dest, &data, level, "deflate_incompressible_guard");
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
