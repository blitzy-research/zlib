//! Live C-oracle byte-identity conformance sweep — opt-in via `--features c-oracle`.
//!
//! This harness builds **reference C zlib from the retained baseline that lives
//! in this repository** and diffs its compressed output, byte for byte, against
//! the output `zlib-rs` produces for the identical input and the identical
//! `(windowBits, memLevel, level, strategy)` configuration. It is the
//! in-repository, reproducible form of the migration's defining acceptance
//! criterion, and it closes gap **D9** of the Agent Action Plan's register
//! (AAP §0.10.1), authorised by the transformation row at AAP §0.4.1.8.
//!
//! # Its relationship to the always-on gate — strictly additive
//!
//! The always-on, toolchain-free counterpart of this gate is the **tier-1 baked
//! vector set in `tests/interop.rs`**: a few hundred deterministic vectors
//! precomputed from the genuine C encoder, which therefore run by default in CI
//! with no C compiler anywhere in sight. That is the release gate, and nothing
//! in this file replaces, weakens or duplicates it — the plan-adopted
//! engineering standard **S3** (AAP §0.7.2) and the design constraint at AAP
//! §0.6.7 both require this harness to be *additive*. What it adds is
//! **breadth**: tier 1 bakes a fixed sample, whereas this harness sweeps the
//! entire configuration grid live against a freshly compiled oracle.
//!
//! # Why it is feature-gated, and why that gate is load-bearing
//!
//! The crate's defining test-suite property is that `cargo test` needs no C
//! toolchain. So this target is declared explicitly in `Cargo.toml` with
//! `required-features = ["c-oracle"]`: without that feature Cargo does not even
//! compile this file, and the default suite is byte-for-byte the suite that
//! existed before this target did (plan-adopted standard **S5**, AAP §0.7.2).
//! The `c-oracle` feature deliberately expands to `[]` — it adds **no
//! dependency of any kind**, so `Cargo.lock` and `cargo metadata` are wholly
//! unaffected.
//!
//! Consistent with that, the reference library is built by shelling out through
//! [`std::process::Command`]. There is no `cc`/`bindgen`/`pkg-config`
//! dependency, no `[build-dependencies]` table and no `links =` key anywhere in
//! the manifest, and `build.rs` stays pure `std`. That property is load-bearing
//! and this harness preserves it exactly.
//!
//! # Behaviour when no C compiler is present
//!
//! `--all-features` is a row of the `build-test` matrix in
//! `.github/workflows/ci.yml`, and `--all-features` enables `c-oracle`. The same
//! situation arises for any contributor or downstream packager who runs
//! `cargo test --all-features` on a machine without a C compiler. The harness
//! therefore probes for a usable compiler **at run time** and, when none exists
//! — or when the retained C baseline is absent, as it legitimately is inside a
//! packaged `.crate` (AAP §0.8.2, third divergence) — prints a clearly marked
//! capability notice and **passes**.
//!
//! It is never `#[ignore]`d: plan-adopted standard **S10** (AAP §0.7.2) fixes
//! the suite's ignored-test count at zero, so a run-time capability probe is the
//! only admissible mechanism. The line is drawn deliberately: a compiler that is
//! *absent* is an environmental fact and skips, whereas a compiler that is
//! *present but cannot build the in-tree C sources* is a genuine defect and
//! **fails**, with the captured `stderr` attached.
//!
//! # How to run it
//!
//! ```text
//! cargo test --features c-oracle --test c_oracle -- --nocapture
//! ```
//!
//! # The build recipe, and what it produces
//!
//! Per configuration the harness copies the [`REFERENCE_SOURCES`] translation
//! units and the [`REFERENCE_HEADERS`] they include into a private directory
//! under the system temporary directory, then runs
//!
//! ```text
//! <cc> -O2 -D_LARGEFILE64_SOURCE=1 -DHAVE_UNISTD_H -c <15 .c files>
//! ar rcs libz_ref.a <15 .o files>
//! <cc> -O2 -I. -o oracle_driver oracle_driver.c libz_ref.a
//! ```
//!
//! which was measured in this environment at `libz_ref.a` = 135,206 bytes with
//! zero warnings under `gcc 15.2.0` (AAP §0.6.4 records 130 KB under
//! `gcc 13.3.0`; the delta is the compiler version). When `ar` is unavailable —
//! on an MSVC toolchain the archiver is `lib.exe`, not `ar` — the harness falls
//! back to linking the object files straight into the driver, which is
//! equivalent for the sweep's purposes.
//!
//! # Why it is a subprocess oracle rather than an FFI link
//!
//! Nothing here is linked into the test binary and no C function is called from
//! Rust. Every byte that crosses the language boundary crosses it as a file.
//! That buys two properties that an FFI design cannot:
//!
//! * **Zero `unsafe`.** There is no `extern "C"` block, no raw pointer and no
//!   use of `zlib_rs::ffi`; the file declares `#![forbid(unsafe_code)]` so the
//!   compiler enforces it. (`tests/inflate_coverage.rs` is the suite's only
//!   sanctioned FFI consumer, because it must exercise the C ABI's
//!   null-pointer and version-string validation. This harness has no such need.)
//! * **No cross-language corpus-reproduction risk.** Rust generates every
//!   corpus and writes it to a file that the C driver `fread`s, so both sides
//!   provably compress *the same bytes*. Had each side generated "the same"
//!   corpus from its own PRNG, a one-bit divergence would masquerade as a
//!   byte-identity failure — the worst available false positive.
//!
//! # A property this harness has that baked vectors cannot
//!
//! The gzip vectors baked into `tests/interop.rs` embed the `OS_CODE` byte at
//! offset 9 of the gzip header, whose value is a compile-time platform choice
//! (AAP §0.6.6: 10 on Windows, 19 on non-Windows Apple, 3 otherwise). Because
//! this harness compiles the C reference **on the very platform the Rust crate
//! is being compiled for**, each side derives `OS_CODE` from its own platform
//! logic. If C's `zutil.h` and Rust's `src/util/mod.rs` ever disagreed on some
//! platform, this sweep would catch it and a baked vector could not. That is
//! value beyond mere reproducibility, and it is the same portability concern
//! that motivates the cross-platform CI matrix gap D3.
//!
//! # Invariants this harness itself must honour
//!
//! * **The retained C sources are read-only.** Preservation directive **D-7**
//!   (AAP §0.8.1) keeps `*.c`, `*.h`, `test/*.c` and `zlib.map` unmodified: they
//!   are the oracle and the specification. They are therefore *copied out* and
//!   every compiler, archiver and driver invocation runs with the temporary
//!   directory as its working directory, so no object file, archive, binary,
//!   corpus or result blob can land in the repository tree. The directory is
//!   removed by a [`Drop`] guard when the test finishes.
//! * **Never adjust an expectation to make a comparison pass.** Preservation
//!   directive **D-2** (AAP §0.8.1) forbids altering a constant, and a mismatch
//!   here is a real byte-identity defect in the encoder that must be reported,
//!   not papered over. Nothing in this file is derived from `zlib-rs` itself;
//!   deriving the "expected" value from the implementation under test would make
//!   the gate self-referential and worthless.
//! * **It measures bytes, never time.** Performance is a constraint on the
//!   migration and not its objective (AAP §0.8.3), so there is no timing
//!   assertion anywhere.
//! * **It is hermetic and deterministic.** Every corpus comes from a fixed-seed
//!   generator written inline, the grid is fixed, and the only environment
//!   variable consulted is `CC`.

#![forbid(unsafe_code)]

// --- Public `zlib-rs` surface: crate-root re-exports plus the public engine ---
// Integration tests see only the public API. The one-call wrappers, the error
// and strategy enums and the stream handle are crate-root re-exports; the
// streaming `deflate` driver, `deflate_bound` and the flush/method constants
// live in the public `deflate` and `constants` modules. `zlib_rs::ffi` is
// deliberately not used (see the module header).
use zlib_rs::checksum::{adler32, crc32};
use zlib_rs::constants::{DEF_MEM_LEVEL, Z_DEFLATED, Z_FINISH};
use zlib_rs::deflate::{deflate, deflate_bound, deflate_end, deflate_init2};
use zlib_rs::{ReturnCode, Strategy, ZLIB_VERNUM, ZStream, compress_bound, zlib_version};

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ===========================================================================
// The retained C baseline: exactly what gets compiled
// ===========================================================================

/// The fifteen C translation units of the retained baseline, compiled verbatim
/// into the reference archive.
///
/// All fifteen are built even though the driver only exercises `deflate`: that
/// is the recipe AAP §0.6.4 measured, and it keeps the archive faithful to the
/// documented artifact. The gzip *file* API (`gzclose.c`, `gzlib.c`,
/// `gzread.c`, `gzwrite.c`) contributes no symbol this sweep calls, but
/// dropping it would change the archive and silently diverge from that recipe.
const REFERENCE_SOURCES: [&str; 15] = [
    "adler32.c",
    "compress.c",
    "crc32.c",
    "deflate.c",
    "gzclose.c",
    "gzlib.c",
    "gzread.c",
    "gzwrite.c",
    "infback.c",
    "inffast.c",
    "inflate.c",
    "inftrees.c",
    "trees.c",
    "uncompr.c",
    "zutil.c",
];

/// The eleven headers the [`REFERENCE_SOURCES`] include, copied alongside them
/// so the reference build resolves every `#include "…"` from the temporary
/// directory and never reaches back into the repository.
const REFERENCE_HEADERS: [&str; 11] = [
    "crc32.h",
    "deflate.h",
    "gzguts.h",
    "inffast.h",
    "inffixed.h",
    "inflate.h",
    "inftrees.h",
    "trees.h",
    "zconf.h",
    "zlib.h",
    "zutil.h",
];

// ===========================================================================
// The configuration grid (AAP §0.6.4, sweep 2)
// ===========================================================================

/// The ten explicit compression levels.
///
/// `Z_DEFAULT_COMPRESSION` (−1) is deliberately not a grid row: it is checked
/// separately, against the reference's own level-6 output, by the sentinel pass
/// inside the smoke sweep. Keeping it out preserves the grid's documented
/// cardinality.
const LEVELS: [i32; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

/// `memLevel` values: the minimum, the default and the maximum.
const MEM_LEVELS: [i32; 3] = [1, 8, 9];

/// All five deflate strategies, as the C `Z_*` integers: `Z_DEFAULT_STRATEGY`,
/// `Z_FILTERED`, `Z_HUFFMAN_ONLY`, `Z_RLE`, `Z_FIXED`.
const STRATEGIES: [i32; 5] = [0, 1, 2, 3, 4];

/// The `windowBits` framings swept by the full grid.
///
/// Five values per AAP §0.6.4: zlib (15), raw (−15), gzip (31), and the
/// 512-byte-window zlib/raw pair (9 / −9). `windowBits = 8` is intentionally
/// absent — it is silently promoted to 9 during deflate state construction, so
/// it would only add a duplicate of the 9 row.
///
/// The gzip row is dropped when the `gzip` feature is off, because
/// `deflate_init2` then rejects a gzip request exactly as a C zlib built without
/// `GZIP` does. `c-oracle` does not imply `gzip`, so
/// `--no-default-features --features c-oracle` is a legitimate configuration and
/// must still compile and pass — with a correspondingly smaller, honestly
/// reported grid.
fn window_bits_grid() -> Vec<i32> {
    let mut grid = vec![15, -15];
    if cfg!(feature = "gzip") {
        grid.push(31);
    }
    grid.extend_from_slice(&[9, -9]);
    grid
}

/// Bytes per corpus in the full grid.
///
/// Sized from a measurement, not a guess. Sweeping the reference C encoder over
/// the five shapes at 4 KiB, 8 KiB, 16 KiB, 32 KiB and 48 KiB showed that below
/// 16 KiB the `memLevel` and small-window axes are **degenerate** — at 8 KiB the
/// structured `mixed` shape produced identical output for `windowBits` 15 and 9
/// once the two-byte zlib header is discounted, and the natural-language shape
/// produced identical output for `memLevel` 1 and 9. A grid built on corpora
/// that small would be largely duplicated rows proving nothing about the two
/// axes it exists to cover. 16 KiB is the smallest size at which the `mixed`
/// shape discriminates *both* axes, and [`grid_discrimination_is_real`] asserts
/// that it still does rather than trusting this comment.
const GRID_CORPUS_BYTES: usize = 16 * 1024;

/// Bytes in the smoke sweep's single corpus, matching the 200,000-byte
/// mixed-entropy corpus of AAP §0.6.4 sweep 1 exactly.
const SMOKE_CORPUS_BYTES: usize = 200_000;

/// Slack added to `deflateBound` when sizing the destination, on both sides.
///
/// The value itself is unimportant; that the two sides use the **same** value is
/// not. C's `deflate_stored` consults `strm->avail_out` when it chooses stored
/// block lengths, so level-0 output is a function of the destination size — the
/// sizing is part of the byte-identity contract, not an allocation detail.
const OUTPUT_SLACK: usize = 64;

/// C's `Z_STREAM_END`. Named locally so the assertion on the recorded reference
/// return code reads like the C check it mirrors.
const Z_STREAM_END_C: i32 = 1;

/// The upstream version identity the retained baseline must report, proving the
/// oracle is the in-tree reference and not a system `libz` picked up by
/// accident.
const REFERENCE_VERSION: &str = "1.3.2.1-motley";

/// One point of the sweep: which corpus, and the four deflate parameters.
///
/// Carried as a struct rather than five loose arguments so the producer keeps a
/// single parameter, and so a failure diagnostic can render the whole
/// configuration from one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Combo {
    /// Index into the sweep's corpus list.
    corpus: usize,
    /// The overloaded `windowBits` selector: raw, zlib, or gzip framing.
    window_bits: i32,
    /// `memLevel`, sizing the hash table and the symbol buffer.
    mem_level: i32,
    /// Compression level.
    level: i32,
    /// Strategy, as the C `Z_*` integer.
    strategy_id: i32,
}

impl Combo {
    /// Renders the configuration the way the C driver's own diagnostics do.
    fn describe(&self, corpus_name: &str) -> String {
        format!(
            "corpus={corpus_name} windowBits={} memLevel={} level={} strategy={} ({})",
            self.window_bits,
            self.mem_level,
            self.level,
            self.strategy_id,
            strategy_name(self.strategy_id),
        )
    }
}

/// Maps a strategy integer onto the public [`Strategy`] enum by an explicit
/// `match`, mirroring the equivalent helper in `tests/interop.rs`.
///
/// Deliberately not a cast or a transmute into the enum: the mapping is part of
/// the C-to-Rust correspondence being tested, so it is spelled out.
fn strategy_from_id(id: i32) -> Strategy {
    match id {
        0 => Strategy::Default,
        1 => Strategy::Filtered,
        2 => Strategy::HuffmanOnly,
        3 => Strategy::Rle,
        4 => Strategy::Fixed,
        other => panic!("{other} is not a zlib deflate strategy"),
    }
}

/// The C spelling of a strategy id, for diagnostics.
fn strategy_name(id: i32) -> &'static str {
    match id {
        0 => "Z_DEFAULT_STRATEGY",
        1 => "Z_FILTERED",
        2 => "Z_HUFFMAN_ONLY",
        3 => "Z_RLE",
        4 => "Z_FIXED",
        _ => "unknown",
    }
}

// ===========================================================================
// Deterministic corpora — the five shapes of AAP §0.6.4, sweep 2
// ===========================================================================

/// A `xorshift64*` generator with hard-coded constants.
///
/// Written inline rather than taken from the `rand` dev-dependency on purpose: a
/// conformance oracle's inputs must stay byte-stable forever, and a corpus
/// derived from an external RNG would change the moment that crate changed its
/// algorithm. The multiplier and the shift triple below are fixed for
/// reproducibility and must never be tuned.
struct XorShift64(u64);

impl XorShift64 {
    /// Seeds the generator. Any non-zero seed is valid; the seeds used here are
    /// literals so the corpora are reproducible on every machine.
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Returns the next byte of the sequence.
    fn next_byte(&mut self) -> u8 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u8
    }
}

/// One corpus: its shape name, the bytes both implementations compress, and the
/// path the reference driver reads them back from.
struct Corpus {
    /// Shape name, used in diagnostics and in the corpus file name.
    name: &'static str,
    /// The exact bytes written to disk for the C driver and fed to `zlib-rs`.
    data: Vec<u8>,
    /// Absolute path of the materialised corpus inside the work directory.
    path: PathBuf,
}

/// A constant run: maximally compressible, every match is the longest possible
/// match. Exercises the RLE and long-match paths.
fn shape_constant(len: usize) -> Vec<u8> {
    vec![0xA5; len]
}

/// High-entropy pseudo-random bytes: effectively incompressible, so the
/// stored-block fallback and the fruitless-search paths dominate. This is also
/// the profile where compression throughput is furthest from the C baseline
/// (AAP §0.8.3) — a fact this harness neither measures nor cares about, since it
/// compares bytes and never time.
fn shape_incompressible(len: usize) -> Vec<u8> {
    let mut rng = XorShift64::new(0x9E37_79B9_7F4A_7C15);
    (0..len).map(|_| rng.next_byte()).collect()
}

/// Repeated natural-language text: the typical literal/length-code mix, with
/// abundant mid-distance back-references, and the shape that drives the
/// text/binary data-type detection.
fn shape_text(len: usize) -> Vec<u8> {
    const PHRASE: &[u8] = b"The quick brown fox jumps over the lazy dog. \
Pack my box with five dozen liquor jugs. How vexingly quick daft zebras jump! ";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let take = (len - out.len()).min(PHRASE.len());
        out.extend_from_slice(&PHRASE[..take]);
    }
    out
}

/// A strictly periodic byte ramp with a 256-byte period: the structure
/// `Z_FILTERED` is designed for.
fn shape_ramp(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i & 0xFF) as u8).collect()
}

/// Alternating runs and pseudo-random blocks: forces the match finder to switch
/// repeatedly between the long-match and no-match regimes, which is what
/// exercises block-boundary placement and the stored/static/dynamic block-type
/// selection in `_tr_flush_block`.
fn shape_mixed(len: usize) -> Vec<u8> {
    let mut rng = XorShift64::new(0xDEAD_BEEF_CAFE_F00D);
    let mut out = Vec::with_capacity(len);
    let mut round = 0usize;
    while out.len() < len {
        let remaining = len - out.len();
        if round % 2 == 0 {
            let run = (61 + round.wrapping_mul(37) % 300).min(remaining);
            let byte = (round.wrapping_mul(13) & 0xFF) as u8;
            out.extend(std::iter::repeat_n(byte, run));
        } else {
            let run = (29 + round.wrapping_mul(53) % 200).min(remaining);
            out.extend((0..run).map(|_| rng.next_byte()));
        }
        round += 1;
    }
    out
}

/// The name of the structured shape used by the discrimination self-check.
///
/// Deliberately *not* `constant`: with every match maximal, `memLevel` genuinely
/// cannot change that shape's output at any size, so it is the one legitimate
/// exception to the discrimination requirement rather than evidence against it.
const DISCRIMINATING_SHAPE: &str = "mixed";

/// Builds the five corpus shapes at `len` bytes each, in the fixed order the
/// sweep's corpus indices refer to.
fn corpus_shapes(len: usize) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("constant", shape_constant(len)),
        ("incompressible", shape_incompressible(len)),
        ("text", shape_text(len)),
        ("ramp", shape_ramp(len)),
        (DISCRIMINATING_SHAPE, shape_mixed(len)),
    ]
}

/// The smoke sweep's single corpus: one mixed-entropy buffer, reproducing the
/// 200,000-byte corpus of AAP §0.6.4 sweep 1.
fn smoke_corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![("mixed-entropy", shape_mixed(SMOKE_CORPUS_BYTES))]
}

// ===========================================================================
// The private work directory (std only — there is no `tempfile` dependency)
// ===========================================================================

/// A uniquely named work directory, removed when the guard drops.
///
/// Everything the reference build produces — copied sources, object files, the
/// archive, the driver binary, the corpora and the result blob — lives here and
/// nowhere else, which is how preservation directive **D-7** (AAP §0.8.1) is
/// honoured mechanically rather than aspirationally.
///
/// Cleanup by [`Drop`] is exactly right here, and it is worth noting why that is
/// not in tension with `impl Drop for GzState` being deliberately *empty* of
/// finishing logic (AAP §0.8.2): a destructor cannot surface a deferred I/O
/// error, so the gzip layer must not attempt one — but failing to delete a
/// temporary directory is not an error anybody needs to hear about, so
/// best-effort removal in a destructor is precisely the correct design.
struct WorkDir {
    /// Absolute path of the directory.
    path: PathBuf,
}

impl WorkDir {
    /// Creates a fresh directory tagged with `tag`.
    ///
    /// Uniqueness comes from the process id, a per-process monotonic counter and
    /// a nanosecond timestamp together, because Cargo runs the test functions of
    /// one binary on parallel threads and each builds its own oracle.
    fn new(tag: &str) -> std::io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zlib_rs_c_oracle_{tag}_{}_{nanos}_{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Returns the path of `name` inside this directory.
    fn join<P: AsRef<Path>>(&self, name: P) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        // Best-effort: a cleanup failure must never mask a test result, and
        // leaving a few hundred KiB behind is not worth failing a conformance
        // gate over.
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ===========================================================================
// Capability probing — skip on an absent tool, fail on a broken build
// ===========================================================================

/// A usable C toolchain: the compiler, its version banner, and the archiver when
/// one is available.
struct Toolchain {
    /// The command that answered `--version`.
    cc: String,
    /// First line of its `--version` output, printed as evidence.
    version: String,
    /// The archiver, when `ar` answered `--version`; [`None`] selects the
    /// direct-object-link fallback.
    ar: Option<String>,
}

/// Candidate C compilers in priority order: `$CC` when set and non-empty, then
/// the conventional names. `CC` is the only environment variable this harness
/// reads.
fn compiler_candidates() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(cc) = std::env::var("CC") {
        let cc = cc.trim().to_owned();
        if !cc.is_empty() {
            candidates.push(cc);
        }
    }
    for name in ["cc", "gcc", "clang"] {
        if !candidates.iter().any(|c| c == name) {
            candidates.push(name.to_owned());
        }
    }
    candidates
}

/// Returns the first line of `name --version`, or [`None`] when the tool cannot
/// be spawned or reports failure.
///
/// The banner is captured rather than inherited so it never pollutes the test
/// log; only the one line this returns is printed, and only as evidence of which
/// toolchain produced the oracle.
///
/// This probe runs *before* the work directory exists — it is what decides
/// whether there is anything to build at all — so it anchors itself to the
/// system temporary directory instead. The rule that no spawn in this file ever
/// inherits the repository as its working directory therefore holds without
/// exception, which is what preservation directive **D-7** (AAP §0.8.1)
/// requires: even a compiler wrapper that wrote a stray log file on `--version`
/// could not touch the tree.
fn probe_tool(name: &str) -> Option<String> {
    let out = Command::new(name)
        .current_dir(std::env::temp_dir())
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let banner = String::from_utf8_lossy(&out.stdout);
    Some(
        banner
            .lines()
            .next()
            .unwrap_or("(no version banner)")
            .trim()
            .to_owned(),
    )
}

/// Probes for a compiler and, independently, for `ar`.
///
/// A missing `ar` is not a reason to skip: the archive step exists to mirror the
/// recipe AAP §0.6.4 measured, and linking the objects straight into the driver
/// is equivalent for this sweep. A missing *compiler* is a reason to skip.
fn probe_toolchain() -> Option<Toolchain> {
    let (cc, version) = compiler_candidates()
        .into_iter()
        .find_map(|candidate| probe_tool(&candidate).map(|version| (candidate, version)))?;
    let ar = probe_tool("ar").map(|_| "ar".to_owned());
    Some(Toolchain { cc, version, ar })
}

/// Prints the capability notice explaining why the live sweep did not run.
///
/// Unmistakable in a CI log, and unambiguous that it is not a failure. A
/// run-time notice is the mechanism precisely because plan-adopted standard
/// **S10** (AAP §0.7.2) keeps the suite's ignored-test count at zero, so
/// `#[ignore]` is not available.
fn skip_notice(reason: &str) {
    println!(
        "\nc_oracle: SKIPPED — {reason}.\n\
         c_oracle: This harness is opt-in and additive; tier 1 of tests/interop.rs is the\n\
         c_oracle: always-on, toolchain-free byte-identity gate and is unaffected.\n\
         c_oracle: Install a C compiler (or set CC) to run the live sweep against the\n\
         c_oracle: retained C baseline in the repository root.\n"
    );
}

/// Renders a captured process failure into a diagnosable multi-line report.
///
/// Used for the hard-failure path: when a compiler is present but the reference
/// build fails, the captured `stderr` is what makes the failure actionable.
fn describe_failure(what: &str, output: &Output) -> String {
    let mut report = format!("{what} failed with {}", output.status);
    for (label, stream) in [("stderr", &output.stderr), ("stdout", &output.stdout)] {
        let text = String::from_utf8_lossy(stream);
        let tail: Vec<&str> = text.lines().rev().take(20).collect();
        if !tail.is_empty() {
            let _ = write!(report, "\n  --- {label} (last {} lines) ---", tail.len());
            for line in tail.iter().rev() {
                let _ = write!(report, "\n  {}", line.trim_end());
            }
        }
    }
    report
}

/// Runs `command`, panicking with the captured output when it cannot be spawned
/// or reports failure.
///
/// This is the deliberate line between the two failure modes. A tool that is
/// *absent* was already handled by [`probe_toolchain`] and skipped; reaching this
/// function means the toolchain answered `--version` and then could not do its
/// job, which is a genuine problem worth surfacing loudly rather than hiding
/// behind a skip notice.
fn run_required(command: &mut Command, what: &str) -> Output {
    match command.output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => panic!("{}", describe_failure(what, &output)),
        Err(err) => panic!(
            "{what} could not be spawned: {err}. The toolchain answered `--version` a moment \
             ago, so this is not a plain 'no C compiler' condition."
        ),
    }
}

// ===========================================================================
// The reference driver, emitted as C and compiled out-of-tree
// ===========================================================================

/// The reference oracle driver, written into the work directory as
/// `oracle_driver.c` and linked against the freshly built reference zlib.
///
/// It has two sub-commands:
///
/// * `info` — prints the reference library's identity vectors as `key=value`
///   lines, so the oracle can itself be validated before anything is compared
///   against it.
/// * `sweep <params>` — reads the parameter file, walks the grid and writes the
///   result blob.
///
/// The grid is passed in from Rust through a parameter file rather than
/// hard-coded here, so there is exactly one source of truth for the sweep's
/// shape and the two sides cannot drift apart. See [`write_params`] for the
/// file format and [`Record`] for the blob layout; both are documented on the C
/// side too, immediately below.
const DRIVER_SOURCE: &str = r#"/* Reference-zlib oracle driver for tests/c_oracle.rs of the zlib-rs crate.
 *
 * Emitted at test run time into a private temporary directory and linked
 * against the retained C baseline compiled in that same directory. This file is
 * never part of the repository and is never compiled into the Rust crate.
 *
 * Sub-commands
 * ------------
 *   oracle_driver info
 *       Prints identity vectors of the linked reference library, one
 *       `key=value` per line, so the caller can prove what oracle it is about
 *       to trust.
 *
 *   oracle_driver sweep <params-file>
 *       Reads the grid from <params-file> and writes one record per
 *       combination to the output file named there.
 *
 * Parameter file format (one `key=value` per line; `#` starts a comment and
 * blank lines are ignored; axis values are comma-separated; `corpus=` repeats,
 * and the order of those lines defines the corpus index used in the records):
 *
 *   out=<output path>
 *   slack=<bytes added to deflateBound when sizing the destination>
 *   windowbits=<list>
 *   memlevels=<list>
 *   levels=<list>
 *   strategies=<list>
 *   corpus=<path>            (repeated)
 *
 * Result blob layout. Every scalar is LITTLE-ENDIAN and is assembled byte by
 * byte below; a struct is deliberately never written directly, because struct
 * padding is not part of any interchange contract.
 *
 *   header, 12 bytes
 *     0   magic            "ZRCO"    4 bytes
 *     4   format version   u32 le    (= 1)
 *     8   record count     u32 le
 *
 *   record, 32-byte header then the payload
 *     0   corpus index     u32 le
 *     4   windowBits       i32 le
 *     8   memLevel         i32 le
 *     12  level            i32 le
 *     16  strategy         i32 le
 *     20  deflate() return i32 le    (recorded, NOT asserted here)
 *     24  deflateBound     u32 le
 *     28  length           u32 le
 *     32  compressed bytes `length` bytes
 *
 * The configuration is echoed into every record so the reader can verify the
 * two sides walked the grid in the same order record by record, instead of only
 * checking the aggregate count. Records appear in this nested-loop order, which
 * the Rust side reproduces exactly:
 *
 *     for corpus { for windowBits { for memLevel { for level { for strategy } } } }
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "zlib.h"

#define MAX_AXIS 64
#define MAX_CORPORA 32
#define MAX_LINE 4096
#define FORMAT_VERSION 1

/* --- little-endian scalar writers ------------------------------------------ */

static int put_u32(FILE *out, unsigned long v) {
    unsigned char b[4];
    b[0] = (unsigned char)(v & 0xFFUL);
    b[1] = (unsigned char)((v >> 8) & 0xFFUL);
    b[2] = (unsigned char)((v >> 16) & 0xFFUL);
    b[3] = (unsigned char)((v >> 24) & 0xFFUL);
    return fwrite(b, 1, 4, out) == 4 ? 0 : -1;
}

/* Two's-complement little-endian, so the reader recovers negative windowBits
 * and negative return codes exactly. */
static int put_i32(FILE *out, int v) {
    return put_u32(out, (unsigned long)((unsigned int)v));
}

/* --- parameter file ------------------------------------------------------- */

struct params {
    char out[MAX_LINE];
    long slack;
    int wbits[MAX_AXIS];
    int nwbits;
    int memlevels[MAX_AXIS];
    int nmemlevels;
    int levels[MAX_AXIS];
    int nlevels;
    int strategies[MAX_AXIS];
    int nstrategies;
    char corpora[MAX_CORPORA][MAX_LINE];
    int ncorpora;
};

static int parse_list(const char *text, int *out, int *count) {
    const char *p = text;
    int n = 0;
    while (*p != '\0') {
        char *end = NULL;
        long value;
        if (n == MAX_AXIS) {
            fprintf(stderr, "oracle: axis list longer than %d entries\n", MAX_AXIS);
            return -1;
        }
        value = strtol(p, &end, 10);
        if (end == p) {
            fprintf(stderr, "oracle: malformed axis list \"%s\"\n", text);
            return -1;
        }
        out[n++] = (int)value;
        p = end;
        while (*p == ',' || *p == ' ') {
            p++;
        }
    }
    if (n == 0) {
        fprintf(stderr, "oracle: empty axis list\n");
        return -1;
    }
    *count = n;
    return 0;
}

static int read_params(const char *path, struct params *p) {
    FILE *f = fopen(path, "r");
    char line[MAX_LINE];
    if (f == NULL) {
        fprintf(stderr, "oracle: cannot open parameter file %s\n", path);
        return -1;
    }
    memset(p, 0, sizeof *p);
    p->slack = 64;
    while (fgets(line, (int)sizeof line, f) != NULL) {
        char *eq;
        char *key;
        char *value;
        size_t len = strlen(line);
        while (len > 0 && (line[len - 1] == '\n' || line[len - 1] == '\r')) {
            line[--len] = '\0';
        }
        if (len == 0 || line[0] == '#') {
            continue;
        }
        eq = strchr(line, '=');
        if (eq == NULL) {
            fprintf(stderr, "oracle: parameter line without '=': %s\n", line);
            fclose(f);
            return -1;
        }
        *eq = '\0';
        key = line;
        value = eq + 1;
        if (strcmp(key, "out") == 0) {
            strncpy(p->out, value, sizeof p->out - 1);
        } else if (strcmp(key, "slack") == 0) {
            p->slack = strtol(value, NULL, 10);
        } else if (strcmp(key, "windowbits") == 0) {
            if (parse_list(value, p->wbits, &p->nwbits) != 0) { fclose(f); return -1; }
        } else if (strcmp(key, "memlevels") == 0) {
            if (parse_list(value, p->memlevels, &p->nmemlevels) != 0) { fclose(f); return -1; }
        } else if (strcmp(key, "levels") == 0) {
            if (parse_list(value, p->levels, &p->nlevels) != 0) { fclose(f); return -1; }
        } else if (strcmp(key, "strategies") == 0) {
            if (parse_list(value, p->strategies, &p->nstrategies) != 0) { fclose(f); return -1; }
        } else if (strcmp(key, "corpus") == 0) {
            if (p->ncorpora == MAX_CORPORA) {
                fprintf(stderr, "oracle: more than %d corpora\n", MAX_CORPORA);
                fclose(f);
                return -1;
            }
            strncpy(p->corpora[p->ncorpora], value, MAX_LINE - 1);
            p->ncorpora++;
        } else {
            fprintf(stderr, "oracle: unknown parameter key \"%s\"\n", key);
            fclose(f);
            return -1;
        }
    }
    fclose(f);
    if (p->out[0] == '\0' || p->ncorpora == 0 || p->nwbits == 0 || p->nmemlevels == 0 ||
        p->nlevels == 0 || p->nstrategies == 0) {
        fprintf(stderr, "oracle: parameter file %s is incomplete\n", path);
        return -1;
    }
    if (p->slack < 0) {
        fprintf(stderr, "oracle: negative slack\n");
        return -1;
    }
    return 0;
}

/* --- corpus loading ------------------------------------------------------- */

static unsigned char *slurp(const char *path, long *size) {
    FILE *f = fopen(path, "rb");
    unsigned char *buf;
    long n;
    if (f == NULL) {
        fprintf(stderr, "oracle: cannot open corpus %s\n", path);
        return NULL;
    }
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return NULL; }
    n = ftell(f);
    if (n < 0 || fseek(f, 0, SEEK_SET) != 0) { fclose(f); return NULL; }
    buf = (unsigned char *)malloc((size_t)(n > 0 ? n : 1));
    if (buf == NULL) { fclose(f); return NULL; }
    if (n > 0 && fread(buf, 1, (size_t)n, f) != (size_t)n) {
        fprintf(stderr, "oracle: short read on %s\n", path);
        free(buf);
        fclose(f);
        return NULL;
    }
    fclose(f);
    *size = n;
    return buf;
}

/* --- identity vectors ----------------------------------------------------- */

static int cmd_info(void) {
    printf("version=%s\n", zlibVersion());
    printf("vernum=%lu\n", (unsigned long)ZLIB_VERNUM);
    printf("crc32=%lu\n", (unsigned long)crc32(0L, (const Bytef *)"123456789", 9));
    printf("adler32=%lu\n", (unsigned long)adler32(1L, (const Bytef *)"123456789", 9));
    printf("compress_bound_9=%lu\n", (unsigned long)compressBound(9));
    return 0;
}

/* --- one grid point ------------------------------------------------------- */

static int emit(FILE *out, const struct params *p, int corpus, int wbits, int memlevel,
                int level, int strategy, const unsigned char *data, long n) {
    z_stream s;
    unsigned char *buf;
    uLong bound;
    uLong cap;
    unsigned long produced;
    int rc;
    int rc_end;

    memset(&s, 0, sizeof s);
    rc = deflateInit2(&s, level, Z_DEFLATED, wbits, memlevel, strategy);
    if (rc != Z_OK) {
        fprintf(stderr,
                "oracle: deflateInit2(level=%d, windowBits=%d, memLevel=%d, strategy=%d) = %d\n",
                level, wbits, memlevel, strategy, rc);
        return -1;
    }

    /* deflateBound() is called AFTER deflateInit2() because it consults the
     * initialised state's wrapper mode and window configuration. The Rust side
     * sizes its destination with the same bound and the same slack: C's
     * deflate_stored() reads strm->avail_out when it chooses stored block
     * lengths, so at level 0 the destination size is part of the emitted
     * output, not merely an allocation detail. */
    bound = deflateBound(&s, (uLong)n);
    cap = bound + (uLong)p->slack;
    if (cap > (uLong)(uInt)-1) {
        fprintf(stderr, "oracle: destination bound %lu exceeds uInt\n", (unsigned long)cap);
        deflateEnd(&s);
        return -1;
    }
    buf = (unsigned char *)malloc((size_t)cap);
    if (buf == NULL) {
        fprintf(stderr, "oracle: out of memory for %lu bytes\n", (unsigned long)cap);
        deflateEnd(&s);
        return -1;
    }

    s.next_in = (Bytef *)data;
    s.avail_in = (uInt)n;
    s.next_out = buf;
    s.avail_out = (uInt)cap;

    /* A single Z_FINISH pass, which deflateBound sizing guarantees completes.
     * No header is set and no dictionary is supplied, so both sides derive the
     * gzip mtime (0), the XFL byte from the level and the OS byte from their own
     * OS_CODE. The return code is RECORDED rather than asserted here: the Rust
     * side asserts that C reported Z_STREAM_END, that zlib-rs reported
     * StreamEnd, and that the two agree. */
    rc = deflate(&s, Z_FINISH);
    produced = (unsigned long)(cap - s.avail_out);
    rc_end = deflateEnd(&s);
    if (rc_end != Z_OK) {
        fprintf(stderr, "oracle: deflateEnd = %d\n", rc_end);
        free(buf);
        return -1;
    }

    if (put_u32(out, (unsigned long)corpus) != 0 ||
        put_i32(out, wbits) != 0 ||
        put_i32(out, memlevel) != 0 ||
        put_i32(out, level) != 0 ||
        put_i32(out, strategy) != 0 ||
        put_i32(out, rc) != 0 ||
        put_u32(out, (unsigned long)bound) != 0 ||
        put_u32(out, produced) != 0 ||
        (produced > 0 && fwrite(buf, 1, (size_t)produced, out) != (size_t)produced)) {
        fprintf(stderr, "oracle: write failed\n");
        free(buf);
        return -1;
    }
    free(buf);
    return 0;
}

/* --- the sweep ------------------------------------------------------------ */

static int cmd_sweep(const char *params_path) {
    struct params p;
    FILE *out;
    unsigned long total;
    int ci;

    if (read_params(params_path, &p) != 0) {
        return 2;
    }
    total = (unsigned long)p.ncorpora * (unsigned long)p.nwbits *
            (unsigned long)p.nmemlevels * (unsigned long)p.nlevels *
            (unsigned long)p.nstrategies;

    out = fopen(p.out, "wb");
    if (out == NULL) {
        fprintf(stderr, "oracle: cannot create %s\n", p.out);
        return 2;
    }
    if (fwrite("ZRCO", 1, 4, out) != 4 || put_u32(out, FORMAT_VERSION) != 0 ||
        put_u32(out, total) != 0) {
        fprintf(stderr, "oracle: cannot write the blob header\n");
        fclose(out);
        return 2;
    }

    /* The one authoritative loop order, mirrored on the Rust side. */
    for (ci = 0; ci < p.ncorpora; ci++) {
        long n = 0;
        unsigned char *data = slurp(p.corpora[ci], &n);
        int wi;
        if (data == NULL) {
            fclose(out);
            return 2;
        }
        for (wi = 0; wi < p.nwbits; wi++) {
            int mi;
            for (mi = 0; mi < p.nmemlevels; mi++) {
                int li;
                for (li = 0; li < p.nlevels; li++) {
                    int si;
                    for (si = 0; si < p.nstrategies; si++) {
                        if (emit(out, &p, ci, p.wbits[wi], p.memlevels[mi], p.levels[li],
                                 p.strategies[si], data, n) != 0) {
                            free(data);
                            fclose(out);
                            return 2;
                        }
                    }
                }
            }
        }
        free(data);
    }

    if (fclose(out) != 0) {
        fprintf(stderr, "oracle: closing %s failed\n", p.out);
        return 2;
    }
    printf("records=%lu\n", total);
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "info") == 0) {
        return cmd_info();
    }
    if (argc == 3 && strcmp(argv[1], "sweep") == 0) {
        return cmd_sweep(argv[2]);
    }
    fprintf(stderr, "usage: oracle_driver info | oracle_driver sweep <params-file>\n");
    return 2;
}
"#;

// ===========================================================================
// Sweep axes — one source of truth for the grid, shared with the C driver
// ===========================================================================

/// The four parameter axes of a sweep, in the order the loops nest.
struct Axes {
    /// The `windowBits` framings.
    window_bits: Vec<i32>,
    /// The `memLevel` values.
    mem_levels: Vec<i32>,
    /// The compression levels.
    levels: Vec<i32>,
    /// The strategies, as C `Z_*` integers.
    strategies: Vec<i32>,
}

impl Axes {
    /// The full grid of AAP §0.6.4 sweep 2.
    fn full_grid() -> Self {
        Self {
            window_bits: window_bits_grid(),
            mem_levels: MEM_LEVELS.to_vec(),
            levels: LEVELS.to_vec(),
            strategies: STRATEGIES.to_vec(),
        }
    }

    /// The smoke sweep of AAP §0.6.4 sweep 1: the zlib framing at the default
    /// memory level, across every level and strategy.
    fn smoke() -> Self {
        Self {
            window_bits: vec![15],
            mem_levels: vec![DEF_MEM_LEVEL],
            levels: LEVELS.to_vec(),
            strategies: STRATEGIES.to_vec(),
        }
    }

    /// Combinations per corpus.
    fn per_corpus(&self) -> usize {
        self.window_bits.len() * self.mem_levels.len() * self.levels.len() * self.strategies.len()
    }

    /// Every combination for `corpora` corpora, in **the** authoritative order:
    /// corpus, then `windowBits`, then `memLevel`, then level, then strategy.
    ///
    /// The C driver nests its loops identically, and every record echoes its own
    /// configuration so the correspondence is verified record by record rather
    /// than merely in aggregate.
    fn combos(&self, corpora: usize) -> Vec<Combo> {
        let mut combos = Vec::with_capacity(self.per_corpus() * corpora);
        for corpus in 0..corpora {
            for &window_bits in &self.window_bits {
                for &mem_level in &self.mem_levels {
                    for &level in &self.levels {
                        for &strategy_id in &self.strategies {
                            combos.push(Combo {
                                corpus,
                                window_bits,
                                mem_level,
                                level,
                                strategy_id,
                            });
                        }
                    }
                }
            }
        }
        combos
    }

    /// Human-readable shape of the grid, for the count assertion's message.
    fn shape(&self, corpora: usize) -> String {
        format!(
            "{corpora} corpora x {} windowBits x {} memLevels x {} levels x {} strategies",
            self.window_bits.len(),
            self.mem_levels.len(),
            self.levels.len(),
            self.strategies.len()
        )
    }

    /// Renders an axis as the comma-separated list the parameter file uses.
    fn list(values: &[i32]) -> String {
        values
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

// ===========================================================================
// Building the reference oracle, entirely out of tree
// ===========================================================================

/// A built reference oracle, owning the work directory that holds it.
///
/// Dropping the oracle drops its [`WorkDir`], which removes the copied sources,
/// the objects, the archive, the driver, the corpora and any result blob.
struct Oracle {
    /// The private directory every artifact lives in.
    work: WorkDir,
    /// The linked driver binary.
    driver: PathBuf,
    /// The corpora, in the index order the records refer to.
    corpora: Vec<Corpus>,
    /// The toolchain that built it, reported as evidence.
    toolchain: Toolchain,
}

/// Repository root: the directory holding `Cargo.toml` and the retained C
/// baseline. Cargo sets this at compile time for every integration test, which
/// makes it independent of how the test binary was launched.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Builds a reference oracle over `shapes`, or returns [`None`] after printing a
/// capability notice when this machine cannot produce one.
///
/// [`None`] means an *environmental* condition: no C compiler, or no retained C
/// baseline (which is the legitimate state inside a packaged `.crate`, since the
/// C sources are excluded from the published crate). A compiler that is present
/// but fails to build the baseline panics instead — see [`run_required`].
fn build_oracle(tag: &str, shapes: Vec<(&'static str, Vec<u8>)>) -> Option<Oracle> {
    let Some(toolchain) = probe_toolchain() else {
        skip_notice("no working C compiler found (tried $CC, cc, gcc, clang)");
        return None;
    };

    // The retained C baseline is excluded from the published crate, so its
    // absence is expected there and must degrade exactly like a missing
    // compiler.
    let root = repo_root();
    let missing: Vec<&str> = REFERENCE_SOURCES
        .iter()
        .chain(REFERENCE_HEADERS.iter())
        .copied()
        .filter(|name| !root.join(name).is_file())
        .collect();
    if !missing.is_empty() {
        skip_notice(&format!(
            "the retained C baseline is not present in {} (missing {} of {} files, \
             starting with {}). That is expected inside a packaged crate, where the C \
             sources are deliberately excluded",
            root.display(),
            missing.len(),
            REFERENCE_SOURCES.len() + REFERENCE_HEADERS.len(),
            missing[0]
        ));
        return None;
    }

    let work = match WorkDir::new(tag) {
        Ok(work) => work,
        Err(err) => {
            skip_notice(&format!(
                "could not create a temporary work directory: {err}"
            ));
            return None;
        }
    };

    // Copy the baseline out of the tree. The originals are only ever *read*:
    // preservation directive D-7 keeps them byte-identical, and copying is how
    // that is guaranteed mechanically rather than by discipline.
    for name in REFERENCE_SOURCES.iter().chain(REFERENCE_HEADERS.iter()) {
        if let Err(err) = fs::copy(root.join(name), work.join(name)) {
            skip_notice(&format!(
                "could not copy the retained C baseline file {name} into {}: {err}",
                work.path.display()
            ));
            return None;
        }
    }
    if let Err(err) = fs::write(work.join("oracle_driver.c"), DRIVER_SOURCE) {
        skip_notice(&format!("could not write the reference driver: {err}"));
        return None;
    }

    // Compile the fifteen translation units. `.current_dir(&work.path)` is the
    // single most important line for D-7 compliance: without it the compiler
    // would drop its `.o` files into the process working directory, which during
    // `cargo test` is the repository root. Every spawn below sets it, without
    // exception, and the sources are named relatively so nothing resolves back
    // into the tree. (The one spawn that predates this directory — the
    // `--version` probe in `probe_tool` — anchors itself to the system temporary
    // directory for the same reason, so no spawn anywhere in this file ever
    // inherits the repository as its working directory.)
    let mut compile = Command::new(&toolchain.cc);
    compile
        .current_dir(&work.path)
        .args([
            "-O2",
            "-D_LARGEFILE64_SOURCE=1",
            "-DHAVE_UNISTD_H",
            "-I.",
            "-c",
        ])
        .args(REFERENCE_SOURCES);
    run_required(&mut compile, "compiling the retained C baseline");

    let objects: Vec<String> = REFERENCE_SOURCES
        .iter()
        .map(|src| {
            Path::new(src)
                .with_extension("o")
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    // Prefer the archive path, because `libz_ref.a` is the artifact AAP §0.6.4
    // measured and reporting its size is useful evidence. When `ar` is
    // unavailable — on an MSVC toolchain the archiver is `lib.exe` — fall back to
    // linking the objects straight into the driver, which is equivalent for this
    // sweep's purposes.
    let archive = "libz_ref.a";
    let archived = toolchain.ar.as_ref().is_some_and(|ar| {
        let mut command = Command::new(ar);
        command
            .current_dir(&work.path)
            .arg("rcs")
            .arg(archive)
            .args(&objects);
        matches!(command.output(), Ok(out) if out.status.success())
    });

    let mut link = Command::new(&toolchain.cc);
    link.current_dir(&work.path)
        .args(["-O2", "-I.", "-o", "oracle_driver", "oracle_driver.c"]);
    if archived {
        link.arg(archive);
    } else {
        link.args(&objects);
    }
    run_required(&mut link, "linking the reference oracle driver");

    // Materialise the corpora for the driver to read back, so both sides
    // provably compress the very same bytes.
    let mut corpora = Vec::with_capacity(shapes.len());
    for (name, data) in shapes {
        let path = work.join(format!("corpus_{name}.bin"));
        if let Err(err) = fs::write(&path, &data) {
            skip_notice(&format!("could not write the {name} corpus: {err}"));
            return None;
        }
        corpora.push(Corpus { name, data, path });
    }

    let linkage = if archived {
        match fs::metadata(work.join(archive)) {
            Ok(meta) => format!("{archive} = {} bytes", meta.len()),
            Err(_) => archive.to_owned(),
        }
    } else {
        "direct object link (no usable `ar`)".to_owned()
    };
    println!(
        "c_oracle: reference C zlib built with {} [{}] from {} in-tree translation units \
         and {} headers; {linkage}; corpora = {} x {} bytes",
        toolchain.cc,
        toolchain.version,
        REFERENCE_SOURCES.len(),
        REFERENCE_HEADERS.len(),
        corpora.len(),
        corpora.first().map_or(0, |c| c.data.len()),
    );

    let driver = work.join("oracle_driver");
    Some(Oracle {
        work,
        driver,
        corpora,
        toolchain,
    })
}

// ===========================================================================
// Reading the reference blob back — bounds-checked, and free of `unsafe`
// ===========================================================================

/// Magic prefix of the reference blob, guarding against a stale or foreign file.
const BLOB_MAGIC: &[u8; 4] = b"ZRCO";

/// The blob format version this reader understands. Bumped in lockstep with
/// `FORMAT_VERSION` in [`DRIVER_SOURCE`].
const BLOB_FORMAT_VERSION: u32 = 1;

/// Defensive ceiling on the record count read from the blob header, so a
/// corrupted length cannot ask for an enormous allocation.
const MAX_RECORDS: usize = 1 << 20;

/// One record read back from the reference blob.
struct Record {
    /// The configuration the reference driver echoed back.
    combo: Combo,
    /// What C's `deflate()` returned. Recorded, never asserted on the C side.
    return_code: i32,
    /// What C's `deflateBound()` returned for this configuration.
    bound: usize,
    /// The exact compressed stream reference zlib produced.
    compressed: Vec<u8>,
}

/// A bounds-checked forward reader over the blob.
///
/// Every scalar is decoded with [`u32::from_le_bytes`] / [`i32::from_le_bytes`]
/// from a slice obtained through [`slice::get`], so a truncated blob produces a
/// descriptive error instead of a panic — and no `transmute`, pointer cast or
/// `unsafe` block appears anywhere.
struct BlobReader<'a> {
    /// The whole blob.
    bytes: &'a [u8],
    /// Current offset.
    at: usize,
}

impl<'a> BlobReader<'a> {
    /// Wraps `bytes` at offset zero.
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Consumes and returns the next `n` bytes.
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| "reference blob offset overflowed".to_owned())?;
        let slice = self.bytes.get(self.at..end).ok_or_else(|| {
            format!(
                "the reference blob is truncated: wanted {n} bytes at offset {}, but only {} \
                 of {} remain",
                self.at,
                self.bytes.len().saturating_sub(self.at),
                self.bytes.len()
            )
        })?;
        self.at = end;
        Ok(slice)
    }

    /// Consumes a little-endian `u32`.
    fn read_u32(&mut self) -> Result<u32, String> {
        let quad: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| "a four-byte slice was not four bytes".to_owned())?;
        Ok(u32::from_le_bytes(quad))
    }

    /// Consumes a little-endian, two's-complement `i32`.
    fn read_i32(&mut self) -> Result<i32, String> {
        let quad: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| "a four-byte slice was not four bytes".to_owned())?;
        Ok(i32::from_le_bytes(quad))
    }

    /// Bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }
}

/// Parses the whole blob into records, in the order the driver wrote them.
fn parse_blob(bytes: &[u8]) -> Result<Vec<Record>, String> {
    let mut reader = BlobReader::new(bytes);

    let magic = reader.take(BLOB_MAGIC.len())?;
    if magic != BLOB_MAGIC {
        return Err(format!(
            "unexpected reference blob magic {magic:02x?} (expected {BLOB_MAGIC:02x?}) — the \
             file is stale or was not written by the emitted driver"
        ));
    }
    let version = reader.read_u32()?;
    if version != BLOB_FORMAT_VERSION {
        return Err(format!(
            "reference blob format version {version}, but this reader understands \
             {BLOB_FORMAT_VERSION}"
        ));
    }
    let count = reader.read_u32()? as usize;
    if count > MAX_RECORDS {
        return Err(format!(
            "the reference blob claims {count} records, above the {MAX_RECORDS} sanity ceiling"
        ));
    }

    let mut records = Vec::with_capacity(count);
    for index in 0..count {
        let annotate = |err: String| format!("record {index} of {count}: {err}");
        let corpus = reader.read_u32().map_err(annotate)? as usize;
        let window_bits = reader.read_i32().map_err(annotate)?;
        let mem_level = reader.read_i32().map_err(annotate)?;
        let level = reader.read_i32().map_err(annotate)?;
        let strategy_id = reader.read_i32().map_err(annotate)?;
        let return_code = reader.read_i32().map_err(annotate)?;
        let bound = reader.read_u32().map_err(annotate)? as usize;
        let length = reader.read_u32().map_err(annotate)? as usize;
        let compressed = reader.take(length).map_err(annotate)?.to_vec();
        records.push(Record {
            combo: Combo {
                corpus,
                window_bits,
                mem_level,
                level,
                strategy_id,
            },
            return_code,
            bound,
            compressed,
        });
    }

    if reader.remaining() != 0 {
        return Err(format!(
            "the reference blob has {} trailing bytes after {count} records",
            reader.remaining()
        ));
    }
    Ok(records)
}

// ===========================================================================
// The `zlib-rs` side: compress exactly one configuration, black-box
// ===========================================================================

/// The outcome of one `zlib-rs` compression.
struct DeflateRun {
    /// The final return code of the `Z_FINISH` pass.
    code: ReturnCode,
    /// What `deflate_bound` reported, so bound parity with C is checkable.
    bound: usize,
    /// The exact compressed stream.
    bytes: Vec<u8>,
}

/// Compresses `data` with the public streaming engine under `combo`.
///
/// Mirrors the reference driver step for step: `deflate_init2`, then
/// `deflate_bound` **after** init, a destination of `bound + OUTPUT_SLACK`, a
/// single `Z_FINISH` pass, then `deflate_end`. No header and no dictionary are
/// set, so both sides derive the gzip mtime, the XFL byte and the OS byte the
/// same way.
///
/// The identical sizing is deliberate and load-bearing rather than incidental:
/// C's `deflate_stored` consults `avail_out` when it picks stored block lengths,
/// so level-0 output is a function of the destination size. A single pass is
/// guaranteed by the `deflate_bound` contract, and this producer therefore
/// treats a non-final `Ok` as a hard error rather than growing the buffer —
/// growing would silently change `avail_out` mid-stream and could change the
/// emitted bytes, which is exactly the class of difference this harness exists
/// to detect.
fn zlib_rs_deflate(data: &[u8], combo: Combo, corpus_name: &str) -> DeflateRun {
    let mut strm = ZStream::new();
    deflate_init2(
        &mut strm,
        combo.level,
        Z_DEFLATED,
        combo.window_bits,
        combo.mem_level,
        strategy_from_id(combo.strategy_id),
    )
    .unwrap_or_else(|err| {
        panic!(
            "deflate_init2 must succeed for the valid configuration {}, got {err:?}",
            combo.describe(corpus_name)
        )
    });

    let bound = deflate_bound(&strm, data.len());
    let mut output = vec![0u8; bound + OUTPUT_SLACK];
    let mut produced = 0usize;

    let outcome = deflate(&mut strm, data, &mut output, Z_FINISH);
    produced += outcome.produced;
    let code = outcome.code;
    assert_eq!(
        outcome.consumed,
        data.len(),
        "a single Z_FINISH pass sized with deflate_bound ({bound} + {OUTPUT_SLACK} bytes) must \
         consume the whole {} byte input for {}, but consumed {}",
        data.len(),
        combo.describe(corpus_name),
        outcome.consumed
    );
    assert_eq!(
        code,
        ReturnCode::StreamEnd,
        "a single Z_FINISH pass sized with deflate_bound ({bound} + {OUTPUT_SLACK} bytes) must \
         reach StreamEnd for {}, but returned {code:?} after producing {produced} bytes. The \
         reference driver relies on the same guarantee, so the two sides would no longer agree \
         on avail_out — and level-0 stored-block segmentation depends on it",
        combo.describe(corpus_name)
    );

    deflate_end(&mut strm).expect("deflate_end must succeed after a completed stream");
    output.truncate(produced);
    DeflateRun {
        code,
        bound,
        bytes: output,
    }
}

// ===========================================================================
// Driving the oracle: identity validation, then one sweep
// ===========================================================================

impl Oracle {
    /// Proves *what* the oracle is before anything is compared against it.
    ///
    /// Comparing against an unidentified oracle would make every later assertion
    /// unfalsifiable — in particular, a system `libz` accidentally linked in
    /// place of the retained baseline would still "pass" a byte-identity sweep
    /// while proving nothing about the in-tree specification. The version string
    /// is the guard that catches exactly that, and the canonical checksum and
    /// bound vectors confirm the archive is functional.
    fn assert_identity(&self) {
        let mut info = Command::new(&self.driver);
        info.current_dir(&self.work.path).arg("info");
        let output = run_required(&mut info, "running `oracle_driver info`");

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let value = |key: &str| -> String {
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key}=")))
                .unwrap_or_else(|| {
                    panic!("the reference driver did not report `{key}`; it printed {stdout:?}")
                })
                .trim()
                .to_owned()
        };
        let number = |key: &str| -> u64 {
            value(key)
                .parse::<u64>()
                .unwrap_or_else(|err| panic!("the reference `{key}` is not a number: {err}"))
        };

        let version = value("version");
        assert_eq!(
            version, REFERENCE_VERSION,
            "the locally built oracle reports version {version:?}, not the retained baseline's \
             {REFERENCE_VERSION:?} — a system libz was linked instead of the in-tree sources, so \
             this sweep would prove nothing"
        );
        assert_eq!(
            version,
            zlib_version(),
            "reference C zlib and zlib-rs must report the same version string"
        );
        assert_eq!(
            u32::try_from(number("vernum")).expect("ZLIB_VERNUM fits in u32"),
            ZLIB_VERNUM,
            "reference ZLIB_VERNUM must equal the crate's"
        );
        assert_eq!(
            u32::try_from(number("crc32")).expect("a CRC-32 fits in u32"),
            crc32(0, b"123456789"),
            "crc32(\"123456789\") must agree"
        );
        assert_eq!(
            u32::try_from(number("adler32")).expect("an Adler-32 fits in u32"),
            adler32(1, b"123456789"),
            "adler32(\"123456789\") must agree"
        );
        assert_eq!(
            usize::try_from(number("compress_bound_9")).expect("a small bound fits in usize"),
            compress_bound(9),
            "compressBound(9) must agree"
        );

        println!(
            "c_oracle: oracle identity confirmed — zlibVersion()={version:?} \
             ZLIB_VERNUM={ZLIB_VERNUM:#06x} crc32(\"123456789\")={:#010x} \
             adler32(\"123456789\")={:#010x} compressBound(9)={}",
            crc32(0, b"123456789"),
            adler32(1, b"123456789"),
            compress_bound(9),
        );
    }

    /// Runs one sweep over `axes` and returns the parsed, alignment-verified
    /// records.
    ///
    /// `tag` distinguishes this sweep's parameter and blob files inside the work
    /// directory. The blob is deleted as soon as it has been read: it is by far
    /// the largest artifact the harness produces, and nothing needs it again.
    fn sweep(&self, tag: &str, axes: &Axes) -> Vec<Record> {
        let blob_path = self.work.join(format!("results_{tag}.bin"));
        let params_path = self.work.join(format!("sweep_{tag}.params"));

        // The parameter file is the single source of truth for the grid: writing
        // the axes once and letting the driver read them removes any possibility
        // of the two sides sweeping different grids or nesting their loops
        // differently. The format is documented in DRIVER_SOURCE.
        let mut params = String::new();
        params.push_str("# zlib-rs c_oracle sweep parameters -- format 1.\n");
        params.push_str("# One key=value per line; '#' starts a comment; lists are comma-\n");
        params.push_str("# separated. The order of the `corpus` lines defines the corpus\n");
        params.push_str("# index echoed in every record.\n");
        let _ = writeln!(params, "out={}", blob_path.display());
        let _ = writeln!(params, "slack={OUTPUT_SLACK}");
        let _ = writeln!(params, "windowbits={}", Axes::list(&axes.window_bits));
        let _ = writeln!(params, "memlevels={}", Axes::list(&axes.mem_levels));
        let _ = writeln!(params, "levels={}", Axes::list(&axes.levels));
        let _ = writeln!(params, "strategies={}", Axes::list(&axes.strategies));
        for corpus in &self.corpora {
            let _ = writeln!(params, "corpus={}", corpus.path.display());
        }
        fs::write(&params_path, &params).unwrap_or_else(|err| {
            panic!(
                "could not write the sweep parameter file {}: {err}",
                params_path.display()
            )
        });

        let mut sweep = Command::new(&self.driver);
        sweep
            .current_dir(&self.work.path)
            .arg("sweep")
            .arg(&params_path);
        let output = run_required(&mut sweep, "running the reference `oracle_driver sweep`");

        let bytes = fs::read(&blob_path).unwrap_or_else(|err| {
            panic!(
                "the reference sweep reported success but its blob {} is unreadable: {err}",
                blob_path.display()
            )
        });
        let records = parse_blob(&bytes).unwrap_or_else(|err| panic!("{err}"));
        let _ = fs::remove_file(&blob_path);

        // Three independent alignment checks. A count mismatch would mean the two
        // sides walked different grids, and silently comparing misaligned records
        // would produce a meaningless "byte-identity failure" instead of a
        // diagnosable one.
        let expected = axes.combos(self.corpora.len());
        let reported = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("records="))
            .and_then(|n| n.trim().parse::<usize>().ok())
            .unwrap_or_else(|| {
                panic!("the reference sweep did not report its record count on stdout")
            });
        assert_eq!(
            reported,
            expected.len(),
            "the reference driver swept {reported} configurations but the grid is {} ({})",
            expected.len(),
            axes.shape(self.corpora.len())
        );
        assert_eq!(
            records.len(),
            expected.len(),
            "the reference blob holds {} records but the grid is {} ({})",
            records.len(),
            expected.len(),
            axes.shape(self.corpora.len())
        );
        for (index, (record, want)) in records.iter().zip(&expected).enumerate() {
            assert_eq!(
                &record.combo, want,
                "reference record {index} carries a different configuration than the Rust side \
                 expects at that position — the two nested-loop orders have diverged"
            );
        }

        records
    }
}

// ===========================================================================
// Comparison and diagnostics
// ===========================================================================

/// The eight match-finder decision points a byte-identity divergence localises
/// to, appended to every mismatch report so whoever hits one knows where to look
/// (AAP §0.6.4).
const DECISION_POINTS: &str = "\
  A divergence localises to one of the eight decision points that determine the
  emitted token stream (AAP §0.6.4). In `src/deflate/`, check:
    (a) the hash function          — state.rs `update_hash` vs C `UPDATE_HASH`
    (b) the `hash_shift` derivation — state.rs `hash_bits.div_ceil(MIN_MATCH)`
    (c) the chain insertion order   — state.rs `insert_string`; `head[]` must be
                                      written only AFTER `prev[]`
    (d) the `longest_match` thresholds and both early exits — `max_chain_length`,
        `nice_match`, the `good_match` chain halving, the lookahead clamp
    (e) the lazy-match filter       — slow.rs `TOO_FAR` (4096) and the
                                      `prev_length -= 2` pre-decrement
    (f) block-type selection        — trees.rs stored/static/dynamic formulas
    (g) the Huffman tie-break       — trees.rs `smaller` must use `<=`, not `<`
    (h) the tuning table            — strategy.rs `CONFIGURATION_TABLE`
  Preservation directive D-2 (AAP §0.8.1) applies: do NOT adjust a constant, a
  threshold or this expectation to make the comparison pass.";

/// Renders `bytes[from..to]` as spaced lowercase hex.
fn hex_window(bytes: &[u8], from: usize, to: usize) -> String {
    let to = to.min(bytes.len());
    if from >= to {
        return "(out of range)".to_owned();
    }
    bytes[from..to]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders the first divergence between the reference stream and ours as a
/// report a human can act on.
///
/// A bare `assert_eq!` on two multi-kilobyte `Vec<u8>`s produces an unreadable
/// wall of output; this names the configuration, both lengths, the offset of the
/// first differing byte and a 16-byte hex window around it from each side.
fn divergence_report(record: &Record, corpus_name: &str, ours: &[u8]) -> String {
    let mut report = String::from(
        "compressed output diverged from reference C zlib — byte-identity is the migration's \
         defining acceptance criterion (AAP §0.8.1, directive D-1)\n",
    );
    let _ = writeln!(
        report,
        "  configuration : {}",
        record.combo.describe(corpus_name)
    );
    let _ = writeln!(
        report,
        "  reference     : {} bytes",
        record.compressed.len()
    );
    let _ = writeln!(report, "  zlib-rs       : {} bytes", ours.len());

    match record
        .compressed
        .iter()
        .zip(ours.iter())
        .position(|(a, b)| a != b)
    {
        Some(at) => {
            let from = at.saturating_sub(8);
            let to = at + 8;
            let _ = writeln!(
                report,
                "  first diff    : offset {at} (reference {:#04x} vs zlib-rs {:#04x})",
                record.compressed[at], ours[at]
            );
            let _ = writeln!(
                report,
                "  reference[{from}..] : {}",
                hex_window(&record.compressed, from, to)
            );
            let _ = writeln!(
                report,
                "  zlib-rs  [{from}..] : {}",
                hex_window(ours, from, to)
            );
        }
        None => {
            let common = record.compressed.len().min(ours.len());
            let _ = writeln!(
                report,
                "  first diff    : none in the common {common}-byte prefix — one stream is a \
                 truncation of the other"
            );
        }
    }
    let _ = write!(report, "{DECISION_POINTS}");
    report
}

/// Compares every record against `zlib-rs`, returning how many were checked.
///
/// Asserts, per configuration: the reference reported `Z_STREAM_END`, `zlib-rs`
/// reported [`ReturnCode::StreamEnd`], the two `deflateBound` values agree, and
/// the compressed streams are byte-for-byte equal.
fn compare_all(oracle: &Oracle, records: &[Record]) -> usize {
    let mut checked = 0usize;
    for record in records {
        let corpus = oracle.corpora.get(record.combo.corpus).unwrap_or_else(|| {
            panic!(
                "the reference reported corpus index {} but only {} corpora exist",
                record.combo.corpus,
                oracle.corpora.len()
            )
        });
        let described = record.combo.describe(corpus.name);

        assert_eq!(
            record.return_code, Z_STREAM_END_C,
            "reference C deflate(Z_FINISH) returned {} rather than Z_STREAM_END for {described}",
            record.return_code
        );

        let run = zlib_rs_deflate(&corpus.data, record.combo, corpus.name);
        assert_eq!(
            run.code,
            ReturnCode::StreamEnd,
            "zlib-rs deflate returned {:?} rather than StreamEnd for {described}, while \
             reference C zlib returned Z_STREAM_END",
            run.code
        );
        assert_eq!(
            run.bound, record.bound,
            "deflate_bound disagreed with C deflateBound for {described}. The two sides then \
             size their destinations differently, and because C's deflate_stored consults \
             avail_out when choosing stored block lengths, level-0 output would legitimately \
             differ for a reason that is not a match-finder defect"
        );
        assert!(
            run.bytes == record.compressed,
            "{}",
            divergence_report(record, corpus.name, &run.bytes)
        );
        checked += 1;
    }
    checked
}

/// Finds the record for `combo`, panicking when the grid does not contain it.
fn find_record(records: &[Record], combo: Combo) -> &Record {
    records
        .iter()
        .find(|record| record.combo == combo)
        .unwrap_or_else(|| panic!("the sweep contains no record for {combo:?}"))
}

/// Asserts that the corpora actually make the `memLevel` and small-window axes
/// discriminate, so the grid is not silently vacuous.
///
/// This guard exists because of a measurement, not a hunch. Sweeping the
/// reference C encoder over these five shapes at 4 KiB and 8 KiB showed
/// `memLevel` 1, 8 and 9 producing byte-identical output for the structured
/// shapes, and `windowBits` 15 and 9 differing only in the two-byte zlib header.
/// A grid built on corpora that small would be two-thirds duplicated rows and
/// would prove nothing about the two axes it exists to cover — so if these
/// assertions ever fail, the corpora have become too small or too uniform and
/// that is a real defect in this harness.
///
/// The shape used is deliberately [`DISCRIMINATING_SHAPE`] rather than
/// `constant`: with every match maximal, `memLevel` genuinely cannot change the
/// constant shape's output at any size, which is a legitimate exception rather
/// than evidence against the check.
fn assert_grid_discriminates(oracle: &Oracle, records: &[Record]) {
    /// Level and strategy at which the axes are probed.
    const PROBE_LEVEL: i32 = 6;
    /// `Z_DEFAULT_STRATEGY`.
    const PROBE_STRATEGY: i32 = 0;

    let corpus = oracle
        .corpora
        .iter()
        .position(|c| c.name == DISCRIMINATING_SHAPE)
        .unwrap_or_else(|| panic!("the `{DISCRIMINATING_SHAPE}` corpus shape must be present"));
    let at = |window_bits: i32, mem_level: i32| Combo {
        corpus,
        window_bits,
        mem_level,
        level: PROBE_LEVEL,
        strategy_id: PROBE_STRATEGY,
    };

    let smallest_mem = find_record(records, at(15, 1));
    let largest_mem = find_record(records, at(15, 9));
    assert!(
        smallest_mem.compressed != largest_mem.compressed,
        "the grid is vacuous on the memLevel axis: the `{DISCRIMINATING_SHAPE}` corpus \
         ({} bytes) produced identical reference output at memLevel 1 and memLevel 9 \
         ({} vs {} bytes). Two thirds of the grid would then be duplicated rows. Raise \
         GRID_CORPUS_BYTES or use a corpus with more structure.",
        oracle.corpora[corpus].data.len(),
        smallest_mem.compressed.len(),
        largest_mem.compressed.len()
    );

    let wide_window = find_record(records, at(15, DEF_MEM_LEVEL));
    let narrow_window = find_record(records, at(9, DEF_MEM_LEVEL));
    // Compare the deflate payload, not the wrapper: the two-byte zlib header
    // encodes the window size in its CINFO nibble and so always differs, which
    // would make a whole-stream comparison pass without the *payload* differing
    // at all.
    const ZLIB_HEADER_LEN: usize = 2;
    assert!(
        wide_window.compressed.len() > ZLIB_HEADER_LEN
            && narrow_window.compressed.len() > ZLIB_HEADER_LEN,
        "both zlib-framed reference streams must be longer than the two-byte header"
    );
    assert!(
        wide_window.compressed[ZLIB_HEADER_LEN..] != narrow_window.compressed[ZLIB_HEADER_LEN..],
        "the grid is vacuous on the windowBits axis: the `{DISCRIMINATING_SHAPE}` corpus \
         ({} bytes) produced an identical deflate payload at windowBits 15 and 9, differing \
         only in the two-byte zlib header. Raise GRID_CORPUS_BYTES so the 512-byte window \
         actually constrains the match finder.",
        oracle.corpora[corpus].data.len()
    );

    println!(
        "c_oracle: discrimination self-check passed on the `{DISCRIMINATING_SHAPE}` corpus \
         ({} bytes): memLevel 1 vs 9 -> {} vs {} bytes; windowBits 15 vs 9 payloads differ",
        oracle.corpora[corpus].data.len(),
        smallest_mem.compressed.len(),
        largest_mem.compressed.len()
    );
}

/// Asserts that `Z_DEFAULT_COMPRESSION` (−1) is indistinguishable from level 6,
/// returning how many configurations were checked.
///
/// Compared against the *reference's own* level-6 records, never against a baked
/// constant and never against `zlib-rs` itself, so the check stays falsifiable.
/// It reuses records the sweep already produced, so it costs the oracle nothing.
fn assert_default_level_sentinel(oracle: &Oracle, records: &[Record]) -> usize {
    let mut checked = 0usize;
    for record in records.iter().filter(|record| record.combo.level == 6) {
        let corpus = &oracle.corpora[record.combo.corpus];
        let sentinel = Combo {
            level: -1,
            ..record.combo
        };
        let run = zlib_rs_deflate(&corpus.data, sentinel, corpus.name);
        assert!(
            run.bytes == record.compressed,
            "Z_DEFAULT_COMPRESSION must resolve to level 6 exactly as C does\n{}",
            divergence_report(record, corpus.name, &run.bytes)
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the sweep must contain level-6 records for the Z_DEFAULT_COMPRESSION sentinel to \
         compare against"
    );
    checked
}

// ===========================================================================
// The two gates
// ===========================================================================

/// The fast path: AAP §0.6.4 sweep 1, reproduced exactly.
///
/// One 200,000-byte mixed-entropy corpus at the zlib framing and the default
/// memory level, across all ten levels and all five strategies — fifty
/// configurations, each compared on the reference return code, the
/// `deflateBound` value, the output length and every output byte. The AAP
/// records this sweep at 50/50 byte-identical; this test is how that claim
/// becomes reproducible inside the repository rather than only in an ad-hoc
/// environment.
///
/// It also carries the `Z_DEFAULT_COMPRESSION` sentinel at no cost to the
/// oracle, by comparing `zlib-rs` at level −1 against the reference's own
/// level-6 records.
#[test]
fn c_oracle_smoke_sweep_matches_reference_zlib() {
    // No compiler, or no retained C baseline: print the notice and pass.
    // `#[ignore]` is not an option — plan-adopted standard S10 (AAP §0.7.2) keeps
    // the suite's ignored-test count at zero.
    let Some(oracle) = build_oracle("smoke", smoke_corpus()) else {
        return;
    };
    oracle.assert_identity();

    let axes = Axes::smoke();
    let expected = axes.per_corpus() * oracle.corpora.len();
    assert_eq!(
        expected,
        50,
        "AAP §0.6.4 sweep 1 is one corpus x 10 levels x 5 strategies = 50 configurations, but \
         this harness would sweep {expected} ({})",
        axes.shape(oracle.corpora.len())
    );

    let records = oracle.sweep("smoke", &axes);
    let checked = compare_all(&oracle, &records);

    // Guard against a silently empty sweep masquerading as a pass.
    assert_eq!(
        checked,
        expected,
        "every configuration of the smoke grid must have been compared ({})",
        axes.shape(oracle.corpora.len())
    );

    let sentinel = assert_default_level_sentinel(&oracle, &records);

    println!(
        "c_oracle: smoke sweep {checked}/{expected} byte-identical against reference C zlib \
         {REFERENCE_VERSION} ({} bytes per corpus, built with {})",
        SMOKE_CORPUS_BYTES, oracle.toolchain.cc
    );
    println!(
        "c_oracle: Z_DEFAULT_COMPRESSION resolved to level 6 in {sentinel}/{sentinel} \
         configurations, matching reference C zlib byte for byte"
    );
}

/// The headline gate: AAP §0.6.4 sweep 2, reproduced in full.
///
/// Every corpus shape x `windowBits` x `memLevel` x level x strategy — with the
/// default feature set that is 5 x 5 x 3 x 10 x 5 = **3,750** configurations,
/// the figure the AAP reports at 3,750/3,750 byte-identical. Without the `gzip`
/// feature the `windowBits = 31` framing is not available on the Rust side (as
/// in a C zlib built without `GZIP`), so the grid is legitimately 3,000; the
/// count is computed from the axis tables and the real number is printed, never
/// the headline one.
///
/// Each configuration is compared on the reference return code, the
/// `deflateBound` value, the output length and every output byte. This is the
/// in-repository form of preservation directive **D-1** (AAP §0.8.1), and it is
/// the gate that a change to any of `src/deflate/{state,slow,fast,rle,stored,
/// trees,strategy}.rs` must clear (plan-adopted standard **S3**, AAP §0.7.2).
#[test]
fn c_oracle_full_grid_matches_reference_zlib() {
    let Some(oracle) = build_oracle("grid", corpus_shapes(GRID_CORPUS_BYTES)) else {
        return;
    };
    oracle.assert_identity();

    let axes = Axes::full_grid();
    let expected = axes.per_corpus() * oracle.corpora.len();

    let records = oracle.sweep("grid", &axes);

    // Prove the grid is not vacuous before trusting that it passed.
    assert_grid_discriminates(&oracle, &records);

    let checked = compare_all(&oracle, &records);
    assert_eq!(
        checked,
        expected,
        "every configuration of the full grid must have been compared ({})",
        axes.shape(oracle.corpora.len())
    );

    println!(
        "c_oracle: full grid {checked}/{expected} byte-identical against reference C zlib \
         {REFERENCE_VERSION}"
    );
    println!(
        "c_oracle: grid = {} at {GRID_CORPUS_BYTES} bytes per corpus, built with {}",
        axes.shape(oracle.corpora.len()),
        oracle.toolchain.cc
    );
    if !cfg!(feature = "gzip") {
        println!(
            "c_oracle: note — the `gzip` feature is off, so the windowBits = 31 framing is \
             excluded and the grid is {expected} rather than the 3750 of the default feature set"
        );
    }
}
