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
//! Measured position — external evidence recorded in the plan, not a figure this
//! harness produces, and the only C-relative throughput figures quoted anywhere
//! in this file: compression runs at approximately 85% of reference C zlib, while
//! decompression runs at 107%-127% of it (AAP 0.8.3, "Performance
//! Expectations"). Decompression is therefore at or above parity and compression
//! is the interesting side.
//!
//! Those two numbers are AAP-recorded historical context. They are aggregates,
//! they were not produced by this harness, and — importantly — they do NOT
//! localise the compression shortfall. An earlier revision of this header
//! asserted that the shortfall was "concentrated in the incompressible-input
//! path, where the match finder does the most fruitless work". That claim was
//! plausible but wrong, and a per-profile measurement against a reference C build
//! inverted it: the incompressible profile is the CLOSEST to C, at roughly
//! 82%-86% of its throughput, while the compressible profiles are the FURTHEST,
//! at roughly 58%-64%. The reasoning behind the old claim had the mechanism
//! backwards. On incompressible input the match finder fails FAST rather than
//! working hard: `longest_match`'s two-byte prefilter rejects almost every
//! candidate before the comparison loop is entered, and `_tr_flush_block` then
//! selects stored blocks because the dynamic tree cannot pay for itself, so both
//! implementations end up doing similar and rather little work per byte. It is on
//! compressible input — where hash chains are genuinely walked, lazy matching is
//! evaluated, and Huffman trees are built and emitted — that the gap opens up.
//! (Decompression's measured 104%-125% does bracket the quoted 107%-127%, so that
//! figure survives contact with measurement.)
//!
//! Treat every C-relative percentage in this file as provisional. This harness
//! links no C library and cannot produce one (see the paragraph below), and the
//! repository has no automated in-tree performance oracle — that is an
//! acknowledged gap (AAP 0.10.1 D9, which covers a *conformance* oracle for
//! byte-identity; no performance counterpart exists even in plan). Until such a
//! harness exists, a C-relative claim made here is a claim this repository cannot
//! re-check on demand, which is exactly why the numbers above are attributed
//! rather than asserted, and why `bench_incompressible_guard` below is justified
//! by what it makes falsifiable rather than by a percentage.
//!
//! What this file itself measures is `zlib-rs` alone: it links no C library and
//! runs no reference implementation, so every number it prints is a Rust-only
//! level-and-profile throughput figure, useful for comparing this crate against
//! itself across levels, inputs and commits. The C-relative percentages above
//! come from AAP 0.8.3; they are not produced by a run of this harness.
//!
//! This folder MEASURES; it does not AUTHORISE. Performance is a constraint on
//! the migration, not its objective, so no timing taken here is on its own a
//! licence to change anything under `src/` (AAP 0.8.3). Byte-identity against
//! reference zlib is owned exclusively by `tests/interop.rs`; the full
//! obligation is spelled out on `bench_incompressible_guard`.
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
//! Per AAP 0.8.3 no throughput target was ever specified for this migration and
//! this is explicitly not a performance refactor, so the percentages above are
//! evidence about where the code stands rather than a goal to optimise toward.
//! A candidate speed-up is viable only if it provably cannot change the token
//! stream — bounds-check elision, memory-access patterns, inlining, and
//! buffer-copy strategy — and only after clearing the byte-identity gate, which
//! is owned exclusively by the tier-1 oracle vectors in `tests/interop.rs`.
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
//! # Measurement configuration: sampling mode and noise threshold
//!
//! Two Criterion defaults are wrong for this file's workload, and both are
//! overridden explicitly with the derivation recorded here so the numbers can be
//! re-derived rather than guessed at when the workload or the host changes.
//!
//! ## Sampling mode — why the ~1.8 ms cases use `Flat`
//!
//! Criterion's default `SamplingMode::Auto` selects Linear for every case here.
//! Linear derives its per-sample iteration step as
//! `d = ceil(measurement_time / met / total_runs).max(1)`, where
//! `total_runs = n * (n + 1) / 2` = 5050 for the default 100 samples and `met` is
//! the mean execution time observed during warm-up
//! (criterion-0.5.1 `src/lib.rs`, `ActualSamplingMode::iteration_counts`). It
//! prints `Warning: Unable to complete 100 samples in 5.0s` on exactly one
//! condition — `d == 1` — and it then runs for `5050 * met` no matter what the
//! budget said. The incompressible 64 KiB cases measure `met` at roughly 1.8 ms,
//! so `d = ceil(5e9 / 1.8e6 / 5050) = ceil(0.55) = 1`: the warning fires and the
//! case overruns its nominal 5 s budget to about 9.1 s.
//!
//! Raising `measurement_time` is the wrong remedy. Reaching `d == 2` requires
//! `measurement_time > 5050 * met`, i.e. beyond 9.09 s, and the case then
//! actually runs `5050 * 2 * met` ~ 18.2 s — double what it costs today — while
//! the warning-free window is only `(9.09 s, 18.18 s]`. That window spans a
//! factor of exactly 2 in `met`, so a host merely 32% slower than this one falls
//! back to `d == 1` and the warning returns. On a 4-CPU quota where contention
//! has been measured to move these timings by far more than 32%, that is not a
//! fix, it is a coin flip.
//!
//! `SamplingMode::Flat` is Criterion's own first suggestion in the warning text
//! and is the correct answer for a millisecond-scale iteration. Flat holds the
//! iteration count constant at `ceil((measurement_time / n) / met).max(1)` and
//! warns only when that value is 1 — that is, only once `met` reaches
//! `measurement_time / n` = 50 ms. At `met` ~ 1.8 ms Flat takes 28 iterations per
//! sample, finishes *inside* its 5 s budget at about 5.04 s (so it is also
//! faster than the Linear overrun it replaces), keeps all 100 samples, and holds
//! a ~27.8x margin before the warning could return.
//!
//! What Flat gives up is only the slope estimate: criterion computes
//! `estimates.slope` for Linear alone (`src/analysis/mod.rs`), so the reported
//! interval becomes the mean rather than the regression slope. That trade is free
//! at this magnitude. The slope exists to cancel constant per-sample timer
//! overhead of order tens of nanoseconds, which is ~0.003% of a 1.8 ms iteration;
//! and mean, median, MAD, standard deviation and the entire change-detection
//! comparison are computed identically either way. Flat's uniform samples are in
//! fact the better-conditioned design here, since Linear's samples range from 1
//! to 100 iterations and so differ in relative noise by two orders of magnitude.
//!
//! The sub-millisecond cases keep `Auto`: they satisfy `d >= 1` comfortably, they
//! do not warn, and for them the slope genuinely is the more accurate estimator.
//!
//! ## Noise threshold — why 1% is unusable on this host
//!
//! Criterion's default `noise_threshold` is 0.01, and the verdict rule is a plain
//! comparison of the change confidence interval against it: "regressed" iff both
//! bounds exceed `+noise`, "improved" iff both fall below `-noise`, otherwise
//! "within noise" (`src/report.rs`, `compare_to_threshold`). One percent is far
//! below what this workload reproduces run to run. Measured on the CI-class host
//! that motivated this configuration: coefficient of variation 1.49% for
//! `deflate_profiles/level6/text`, 1.58% and 2.08% for incompressible levels 1
//! and 9, and a spread across five isolated repeats of 3.30% at level 1 and 7.81%
//! at level 6. Two consecutive runs of a bit-identical binary reported
//! "+3.76% regressed" and then "-2.66%"; under deliberate CPU contention the same
//! unchanged binary reported +120% to +136%.
//!
//! `NOISE_THRESHOLD_FAST` and `NOISE_THRESHOLD_SLOW` below are therefore set
//! above the measured spread of the cases they apply to. This buys signal rather
//! than blindness: a change larger than the threshold is still reported, and both
//! values sit far below the magnitude of any optimisation that would be worth
//! taking. A verdict remains only a hint — this host has no frequency pinning and
//! no CPU isolation, so a single-run verdict inside the noise band must be
//! discounted, and a real before/after claim needs repeated runs with
//! `--save-baseline` / `--baseline` rather than one incidental comparison.
//!
//! Note that `--noise-threshold` on the command line CANNOT override these
//! values. `to_complete` resolves every field with `unwrap_or(defaults.field)`
//! (`src/benchmark.rs`), so a value set on the group always wins over the
//! process-level default the CLI flag feeds. The same call is made once per
//! registered case inside `run_bench` (`src/benchmark_group.rs`), which is what
//! makes the per-case configuration in `bench_profiles` below work at all.
//!
//! # Operating this harness
//!
//! The pinned harness (`criterion = "0.5.1"`, AAP 0.5.1) has several behaviours
//! that can silently invalidate a measurement or a CI gate and that cannot be
//! fixed from this repository — among them: a filter that matches nothing exits 0
//! having measured nothing; invoking the bench binary by path without `--bench`
//! runs in Test mode and collects no data; an unwritable `CRITERION_HOME` still
//! exits 0; invalid numeric arguments such as `--sample-size 9` abort with status
//! 101; `--help` is unavailable while `--version` prints no version; and
//! `Gnuplot not found, using plotters backend` is expected and harmless.
//! `benches/checksum_bench.rs` holds the authoritative list with the exact
//! assertion sites, exit codes, and CI mitigations. Read it before wiring any of
//! these benchmarks into an automated gate.
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
/// silently fall back to Criterion's defaults and the warning F-3 removed would
/// return.
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
        // case (`src/benchmark_group.rs`) — so a setting applied here binds only
        // to the case registered on the next statement. That is what makes a
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
