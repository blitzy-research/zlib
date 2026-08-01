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
//!   the crate's own dependency graph:
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
//! The distinction is CRC-32-only. `src/checksum/crc32.rs` dispatches its private
//! `crc32_bulk` helper to `crc32fast::Hasher` or to the braided, word-at-a-time
//! scalar table loop; both are bit-identical to reference zlib, so only
//! throughput changes. `src/checksum/adler32.rs` has no `simd` code path at all,
//! so the `adler32` group is expected to report the same figures under both rows
//! — a divergence there would indicate measurement noise, not a different
//! implementation.
//!
//! # The feature flag is not the whole selector — so the backend is PRINTED
//!
//! `simd` only puts `crc32fast` in the graph. Whether `crc32fast` then executes
//! its carry-less-multiply code depends on **its own** `std` feature, because
//! that is what chooses between a run-time `is_x86_feature_detected!` probe and a
//! compile-time `cfg!(target_feature = ...)` test that is false on a stock
//! `x86_64` target. Two consequences follow, and both are the reason this file
//! now reports the backend instead of naming it in a comment:
//!
//! 1. `cargo bench` and `cargo test` pull in dev-dependencies, and `flate2` asks
//!    for `crc32fast/default` (= its `std`). Cargo feature unification then turns
//!    that on for the WHOLE bench build — even though nothing in the crate's own
//!    graph requested it. A benchmark can therefore time a configuration no
//!    consumer of `cargo build --release` ever has, over-reporting CRC-32
//!    throughput several-fold.
//! 2. That divergence is closed at the manifest level: the crate's `std` feature
//!    forwards to `crc32fast?/std`, so the effective `crc32fast` configuration is
//!    now identical in the bench, test, and release builds. `src/checksum/crc32.rs`
//!    additionally probes reachability itself and keeps its own braid whenever the
//!    accelerated path would not be selected, so `simd` can never be slower than
//!    omitting it.
//!
//! Feature wiring is invisible in a benchmark log, so the guard below prints
//! `crc32_backend()` and asserts it is consistent with the compiled feature set.
//! Every CRC-32 figure this file produces is therefore self-describing, and a
//! figure quoted anywhere else (`README.md`, a release note, an issue) must carry
//! that line with it. To confirm the shipped artifact agrees, compare against a
//! `cargo build --release` library measured through the C ABI — the bench binary
//! and the artifact are built by different commands, and only the printed backend
//! makes the comparison auditable.
//!
//! # Operating this harness: upstream criterion 0.5.1 behaviours to know
//!
//! These are properties of the pinned harness (`criterion = "0.5.1"`, AAP 0.5.1),
//! not of this crate, and none of them can be fixed here. They are recorded
//! because each one can silently invalidate a measurement or a CI gate. This is
//! the authoritative list; `benches/deflate_bench.rs` and
//! `benches/inflate_bench.rs` point here.
//!
//! - **Running the binary directly without `--bench` measures nothing.**
//!   criterion decides its mode as
//!   `match (bench_flag, test_flag) { (true, true) => test, (true, false) =>
//!   benchmark, (false, _) => test }` (`src/lib.rs`), so the *absence* of
//!   `--bench` forces Test mode: every case executes exactly once, prints
//!   `Testing <id>` / `Success`, writes no `estimates.json`, and exits 0. `cargo
//!   bench` supplies `--bench` and `cargo test --benches` deliberately does not.
//!   Anything invoking `target/release/deps/<bench>-<hash>` by path — a profiler,
//!   a `perf` wrapper, a hand-rolled CI step — must pass `--bench` explicitly or
//!   it silently collects no data at all.
//! - **A filter that matches nothing exits 0 having measured nothing.** A typo
//!   (`-- crc32_typo`), an over-narrow `--exact`, or `--ignored` all produce an
//!   empty run with status 0 and no "0 benchmarks matched" notice. Measured: a
//!   non-matching filter in benchmark mode wrote 0 `new/estimates.json` files and
//!   still exited 0. A CI regression gate must therefore pin ids with `--exact`
//!   *and* assert that the expected number of `new/estimates.json` files was
//!   produced; a green run is not by itself evidence that anything ran. Note that
//!   `--exact` needs the *full* id: `--exact crc32/1024` yields one estimates
//!   file, while `--exact crc32` — a group name rather than an id — matches
//!   nothing and yields zero.
//! - **An unwritable criterion home still exits 0.** Point `CRITERION_HOME` at a
//!   path that cannot hold a directory and criterion prints one
//!   `Criterion.rs ERROR: error: Failed to access file …` line per artifact it
//!   fails to write (measured: six lines for a single id, covering the directory,
//!   `tukey.json`, `sample.json`, and the rest), reports timings normally, and
//!   exits 0. A job that persists reports must check that the tree it expected
//!   actually exists rather than trusting the status code.
//! - **Invalid numeric CLI arguments abort inside the harness.**
//!   `--sample-size 9` trips `assertion failed: num_size >= 10` at
//!   `criterion-0.5.1/src/lib.rs:1096`, and `--warm-up-time 0` and
//!   `--measurement-time 0` trip `assertion failed: dur.as_nanos() > 0` at
//!   `:1101` and `:1107`. These are unconditional `assert!`s, not
//!   `debug_assert!`s, so they fire in the release-profile bench binary too, and
//!   the process exits 101. That is an argument-validation limitation upstream,
//!   not a defect in the benchmarks; pass values inside the documented ranges.
//!   The exit code is 101 rather than a SIGABRT 134 because Cargo forces the
//!   `unwind` panic strategy for bench targets even though both profiles set
//!   `panic = "abort"`, so these surface as ordinary panics.
//! - **`--help` is unavailable and `--version` prints no version.** criterion
//!   depends on `clap` with `default-features = false`, so `-h`/`--help` is
//!   rejected with status 2. `-V`/`--version` is declared `hide(true)` with
//!   `num_args(0)` and is then never read, so it is a no-op that leaves the run
//!   to proceed in whatever mode the remaining flags imply: a full measurement
//!   under `cargo bench -- --version`, or a Test-mode pass when the binary is
//!   invoked directly. Either way no version string is emitted. Consult
//!   criterion's documentation for the option list rather than the binary.
//! - **`Gnuplot not found, using plotters backend` is expected.** `gnuplot` is
//!   not a dependency of this project; `plotters` is pinned in `Cargo.lock`,
//!   generates the complete report tree including the SVGs, and the notice is
//!   purely informational. Forcing the issue with `--plotting-backend gnuplot`
//!   panics at `criterion-0.5.1/src/lib.rs:503` with "Gnuplot plotting backend
//!   was requested, but gnuplot is not available" and exits 101. Use `--noplot` if
//!   the intent is to skip plotting: it exits 0, writes no SVGs, and still records
//!   `estimates.json`.
//! - **Group settings in these files outrank the CLI.** criterion resolves a
//!   case's configuration as "group setting, else Criterion/CLI setting", so a
//!   `--measurement-time`, `--sample-size`, or `--noise-threshold` passed on the
//!   command line is ignored for any group that sets it here. Change the constant
//!   in the file if the policy itself is wrong.
//!
//! Registered in `Cargo.toml` as `[[bench]] name = "checksum_bench"` with
//! `harness = false`, so this file supplies its own entry point via
//! `criterion_group!` / `criterion_main!` (not the libtest harness). No
//! `[[bench]]` block carries a `path` key, so Cargo auto-discovers the target by
//! filename: renaming this file breaks the build outright.

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zlib_rs::checksum::{Crc32Backend, crc32_backend};
use zlib_rs::{adler32, crc32};

/// Buffer sizes exercised by every checksum benchmark: 1 KiB, 16 KiB, 256 KiB.
const SIZES: [usize; 3] = [1 << 10, 1 << 14, 1 << 18];

/// Noise threshold for the checksum groups (see `benches/deflate_bench.rs` for
/// the full derivation, which applies to all three bench files).
///
/// criterion's 1% default is below the run-to-run spread of a shared,
/// non-frequency-pinned CI container, so it labels ordinary host noise
/// "Performance has regressed". These groups are the most stable in the suite
/// (measured run-to-run median spread under 0.3%), but the threshold is what
/// makes a *verdict* trustworthy rather than the measurement, and a real CRC-32
/// regression — a backend silently reverting to the software table, which is what
/// `crc32_backend()` above now makes visible — is several hundred percent, not
/// five. The measured change percentage is always printed regardless, so raising
/// the threshold suppresses false verdicts without hiding data.
const NOISE_THRESHOLD: f64 = 0.05;

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

/// Prints, and cross-checks, which CRC-32 backend this binary will time.
///
/// A benchmark log that does not say which implementation it measured cannot be
/// compared with anything, and for CRC-32 the answer is not implied by the
/// feature flags: `crc32fast` selects its accelerated code from its OWN `std`
/// feature, which dev-dependency feature unification can turn on for a bench
/// build (see the module documentation). Printing the library's own answer makes
/// every figure below self-describing, and the assertion makes an impossible
/// combination — `Crc32Fast` without the `simd` feature — a hard failure rather
/// than a misleading number.
fn report_crc32_backend() -> Crc32Backend {
    let backend = crc32_backend();
    println!(
        "checksum_bench: crc32 backend = {backend:?} (crate feature `simd` \
         {}). Compare any figure below only against a run reporting the same \
         backend; the shipped artifact's backend is whatever `cargo build \
         --release` reports through the same accessor.",
        if cfg!(feature = "simd") {
            "enabled"
        } else {
            "disabled"
        }
    );
    assert!(
        cfg!(feature = "simd") || backend == Crc32Backend::Braid,
        "without the `simd` feature the only possible backend is the braided \
         scalar path, but the library reported {backend:?}"
    );
    backend
}

fn bench_adler32(c: &mut Criterion) {
    // Correctness guard using the canonical zlib Adler-32 identity vector.
    assert_eq!(adler32(1, b""), 1, "adler32 identity vector");

    let mut group = c.benchmark_group("adler32");
    group.noise_threshold(NOISE_THRESHOLD);
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
    // Correctness guard using the canonical "123456789" CRC-32 check value. It
    // holds for either backend, which is exactly why the backend must be printed:
    // the value proves correctness, not which code produced it.
    assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926, "crc32 check vector");
    report_crc32_backend();

    let mut group = c.benchmark_group("crc32");
    group.noise_threshold(NOISE_THRESHOLD);
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
