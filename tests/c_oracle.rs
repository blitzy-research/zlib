//! Live C-oracle byte-identity conformance harness (opt-in: `--features c-oracle`).
//!
//! This harness makes the full byte-identity sweep against a **locally built
//! reference C zlib** reproducible *inside this repository*, instead of only in
//! an ad-hoc environment. It compiles the retained C baseline that lives in the
//! repository root (`adler32.c`, `crc32.c`, `deflate.c`, `trees.c`, `zutil.c`,
//! …) into a throwaway reference library, links a tiny driver against it, and
//! then compares — byte for byte — the compressed stream that reference zlib
//! produces against the stream `zlib-rs` produces for the identical input and
//! the identical `(windowBits, memLevel, level, strategy)` configuration.
//!
//! # Relationship to the always-on gate
//!
//! This harness is **strictly additive**. Tier 1 of `tests/interop.rs` — strict
//! byte-identity against deterministic vectors baked from the genuine C encoder
//! — remains the always-on release gate that runs by default with **no C
//! toolchain**, and nothing here replaces or weakens it. What this file adds is
//! *breadth*: tier 1 bakes a few hundred constant vectors, whereas this harness
//! sweeps the full configuration grid live.
//!
//! # Why it is feature-gated
//!
//! The crate's defining test-suite property is that `cargo test` needs no C
//! compiler. So this target is declared explicitly in `Cargo.toml` with
//! `required-features = ["c-oracle"]`: without that feature the file is not
//! even compiled, and the default suite is byte-for-byte the same suite as
//! before this target existed. The `c-oracle` feature deliberately expands to
//! `[]` — it adds no dependency of any kind, so `Cargo.lock` and
//! `cargo metadata` are completely unaffected.
//!
//! The reference library is built by shelling out with
//! [`std::process::Command`] — never through the `cc` crate. There is no
//! `[build-dependencies]` table, no `cc`/`bindgen`/`pkg-config` dependency and
//! no `links =` key anywhere in the manifest, and `build.rs` stays pure `std`.
//! That property is load-bearing and this harness preserves it.
//!
//! # Behaviour when no C compiler is present
//!
//! `--all-features` is a row of the `build-test` matrix in
//! `.github/workflows/ci.yml`, and `--all-features` enables `c-oracle`. The
//! harness therefore probes for a usable C compiler **at run time** and, when
//! none is available (or the reference build cannot be produced), prints a
//! capability notice and **passes**. It never fails for an environmental
//! reason, and it is never `#[ignore]`d — the suite's ignored-test count is
//! zero and stays zero. Once the oracle *does* build and run, any divergence is
//! a hard assertion failure.
//!
//! Run the sweep explicitly with:
//!
//! ```text
//! cargo test --features c-oracle --test c_oracle -- --nocapture
//! ```
//!
//! # Sweep size
//!
//! The grid is the full `5 windowBits x 3 memLevels x 10 levels x 5 strategies`
//! product over five corpus shapes — 3,750 configurations — and the per-corpus
//! byte count is the one knob, settable through
//! `ZLIB_RS_C_ORACLE_CORPUS_BYTES`. The default of
//! [`DEFAULT_CORPUS_BYTES`] keeps the whole sweep inside roughly a minute in the
//! unoptimized test profile. Raising it to `200000` reproduces the widest sweep
//! this port has been validated against and additionally exercises the
//! sliding-window *slide* at `windowBits = 15` (which needs more than the 64 KiB
//! doubled window); both `49152` and `200000` have been observed at
//! 3,750/3,750 byte-identical.
//!
//! # Invariants this harness itself must honour
//!
//! * The retained C sources are **read only**. Objects, the driver binary and
//!   every intermediate file are written to a private directory under the
//!   system temporary directory; the compiler is invoked with that directory as
//!   its working directory and with absolute source paths, so no build artifact
//!   can ever land in the repository tree. The (few hundred KiB) work directory
//!   is deliberately left behind for post-mortem inspection, while the one large
//!   artifact — the reference vector stream — is deleted as soon as it has been
//!   consumed.
//! * All assertions are black-box over the public `zlib_rs` API, and the file
//!   contains zero `unsafe`.

// --- Public `zlib-rs` surface (crate-root re-exports + public engine modules) -
use zlib_rs::checksum::{adler32, crc32};
use zlib_rs::constants::{Z_DEFLATED, Z_FINISH};
use zlib_rs::deflate::{deflate, deflate_end, deflate_init2};
use zlib_rs::{ReturnCode, Strategy, ZStream, compress_bound, zlib_version};

use std::fmt::Write as _;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

// ===========================================================================
// The configuration grid (AAP §0.6.4 sweep 2)
// ===========================================================================

/// `windowBits` framings: zlib (15), raw (−15), gzip (31), and the small
/// 512-byte-window zlib/raw pair (9 / −9). The gzip row is dropped when the
/// `gzip` feature is off, because `deflate_init2` then rejects a gzip request
/// exactly as a C zlib built without `GZIP` does.
fn window_bits_grid() -> Vec<i32> {
    let mut grid = vec![15, -15];
    if cfg!(feature = "gzip") {
        grid.push(31);
    }
    grid.push(9);
    grid.push(-9);
    grid
}

/// `memLevel` values: the minimum, the default, and the maximum.
const MEM_LEVELS: [i32; 3] = [1, 8, 9];

/// The ten explicit compression levels. `Z_DEFAULT_COMPRESSION` (−1) is checked
/// separately by [`default_compression_sentinel_matches_reference`] so that this
/// grid keeps its documented cardinality.
const LEVELS: [i32; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];

/// All five deflate strategies (`Z_DEFAULT_STRATEGY` … `Z_FIXED`).
const STRATEGIES: [i32; 5] = [0, 1, 2, 3, 4];

/// Default corpus size in bytes: 1.5 × the 32 KiB window of `windowBits = 15`,
/// so window recycling and the sliding-window slide are exercised, while the
/// 512-byte-window rows (`±9`) wrap many times over.
///
/// Override with `ZLIB_RS_C_ORACLE_CORPUS_BYTES` to widen or narrow the sweep;
/// the integration tests are compiled unoptimized, so the default is a
/// deliberate balance between coverage and wall-clock time.
const DEFAULT_CORPUS_BYTES: usize = 49_152;

/// Environment variable overriding [`DEFAULT_CORPUS_BYTES`].
const CORPUS_BYTES_ENV: &str = "ZLIB_RS_C_ORACLE_CORPUS_BYTES";

/// Magic prefix of the oracle vector stream, guarding against a stale file.
const VECTOR_MAGIC: &[u8; 8] = b"ZRORCL02";

/// The C translation units of the retained baseline that the reference library
/// is built from. The gzip *file* API (`gz*.c`) is deliberately omitted: this
/// harness compares in-memory `deflate` output and needs no stdio layer.
const REFERENCE_SOURCES: [&str; 11] = [
    "adler32.c",
    "crc32.c",
    "deflate.c",
    "trees.c",
    "zutil.c",
    "compress.c",
    "uncompr.c",
    "inflate.c",
    "inffast.c",
    "inftrees.c",
    "infback.c",
];

// ===========================================================================
// The reference driver, compiled and run out-of-tree
// ===========================================================================

/// Source of the reference driver.
///
/// Two sub-commands:
///
/// * `info` — prints the reference library's identity vectors as `key=value`
///   lines, so the oracle being compared against can itself be validated.
/// * `sweep <out> <wbits> <memlevels> <levels> <strategies> <corpus>…` — walks
///   the cartesian product of the four comma-separated lists for every corpus
///   file and appends one record per combination to `<out>`.
///
/// The grid lists are passed in from Rust rather than hard-coded here so that
/// there is exactly one source of truth for the sweep shape. Record layout is
/// `corpus:i32, windowBits:i32, memLevel:i32, level:i32, strategy:i32,
/// length:u32, length bytes`, written in the host's native byte order — the
/// producer is a C program compiled for the very same target that reads it.
const DRIVER_SOURCE: &str = r#"/* Reference-zlib oracle driver for tests/c_oracle.rs.
 *
 * Generated at test run time into a private temporary directory and linked
 * against the retained C baseline. Never compiled into the Rust crate.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "zlib.h"

#define MAX_LIST 64

static int parse_list(const char *text, int *out) {
    int count = 0;
    const char *p = text;
    while (*p != '\0') {
        char *end = NULL;
        long value = strtol(p, &end, 10);
        if (end == p) {
            fprintf(stderr, "oracle: malformed grid list \"%s\"\n", text);
            return -1;
        }
        if (count == MAX_LIST) {
            fprintf(stderr, "oracle: grid list too long\n");
            return -1;
        }
        out[count++] = (int)value;
        p = end;
        if (*p == ',') {
            p++;
        }
    }
    return count;
}

static unsigned char *slurp(const char *path, long *size) {
    FILE *f = fopen(path, "rb");
    unsigned char *buf;
    long n;
    if (f == NULL) {
        fprintf(stderr, "oracle: cannot open %s\n", path);
        return NULL;
    }
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); return NULL; }
    n = ftell(f);
    if (n < 0 || fseek(f, 0, SEEK_SET) != 0) { fclose(f); return NULL; }
    buf = (unsigned char *)malloc((size_t)n > 0 ? (size_t)n : 1);
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

static int cmd_info(void) {
    printf("version=%s\n", zlibVersion());
    printf("vernum=%u\n", (unsigned)ZLIB_VERNUM);
    printf("crc32=%lu\n", (unsigned long)crc32(0L, (const Bytef *)"123456789", 9));
    printf("adler32=%lu\n", (unsigned long)adler32(1L, (const Bytef *)"123456789", 9));
    printf("compress_bound_9=%lu\n", (unsigned long)compressBound(9));
    return 0;
}

static int emit(FILE *out, int corpus, int wbits, int memlevel, int level,
                int strategy, const unsigned char *data, long n) {
    z_stream s;
    unsigned char *cbuf;
    uLong cap;
    unsigned int clen;
    int header[5];
    int rc;

    memset(&s, 0, sizeof s);
    rc = deflateInit2(&s, level, Z_DEFLATED, wbits, memlevel, strategy);
    if (rc != Z_OK) {
        fprintf(stderr, "oracle: deflateInit2(level=%d, wbits=%d, mem=%d, strat=%d) = %d\n",
                level, wbits, memlevel, strategy, rc);
        return -1;
    }
    cap = deflateBound(&s, (uLong)n) + 128;
    cbuf = (unsigned char *)malloc((size_t)cap);
    if (cbuf == NULL) { deflateEnd(&s); return -1; }
    s.next_in = (Bytef *)data;
    s.avail_in = (uInt)n;
    s.next_out = cbuf;
    s.avail_out = (uInt)cap;
    rc = deflate(&s, Z_FINISH);
    if (rc != Z_STREAM_END) {
        fprintf(stderr, "oracle: deflate(Z_FINISH) = %d (level=%d, wbits=%d, mem=%d, strat=%d)\n",
                rc, level, wbits, memlevel, strategy);
        free(cbuf);
        deflateEnd(&s);
        return -1;
    }
    clen = (unsigned int)(cap - s.avail_out);
    deflateEnd(&s);

    header[0] = corpus;
    header[1] = wbits;
    header[2] = memlevel;
    header[3] = level;
    header[4] = strategy;
    if (fwrite(header, sizeof(int), 5, out) != 5 ||
        fwrite(&clen, sizeof clen, 1, out) != 1 ||
        (clen > 0 && fwrite(cbuf, 1, (size_t)clen, out) != (size_t)clen)) {
        fprintf(stderr, "oracle: write failed\n");
        free(cbuf);
        return -1;
    }
    free(cbuf);
    return 0;
}

static int cmd_sweep(int argc, char **argv) {
    int wbits[MAX_LIST], memlevels[MAX_LIST], levels[MAX_LIST], strategies[MAX_LIST];
    int nw, nm, nl, ns;
    int corpus;
    FILE *out;

    if (argc < 8) {
        fprintf(stderr, "usage: oracle sweep <out> <wbits> <mem> <levels> <strategies> <corpus>...\n");
        return 2;
    }
    nw = parse_list(argv[3], wbits);
    nm = parse_list(argv[4], memlevels);
    nl = parse_list(argv[5], levels);
    ns = parse_list(argv[6], strategies);
    if (nw <= 0 || nm <= 0 || nl <= 0 || ns <= 0) {
        return 2;
    }
    out = fopen(argv[2], "wb");
    if (out == NULL) {
        fprintf(stderr, "oracle: cannot create %s\n", argv[2]);
        return 2;
    }
    if (fwrite("ZRORCL02", 1, 8, out) != 8) {
        fclose(out);
        return 2;
    }
    for (corpus = 7; corpus < argc; corpus++) {
        long n = 0;
        unsigned char *data = slurp(argv[corpus], &n);
        int wi, mi, li, si;
        if (data == NULL) { fclose(out); return 2; }
        for (wi = 0; wi < nw; wi++) {
            for (mi = 0; mi < nm; mi++) {
                for (li = 0; li < nl; li++) {
                    for (si = 0; si < ns; si++) {
                        if (emit(out, corpus - 7, wbits[wi], memlevels[mi], levels[li],
                                 strategies[si], data, n) != 0) {
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
        fprintf(stderr, "oracle: close failed\n");
        return 2;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && strcmp(argv[1], "info") == 0) {
        return cmd_info();
    }
    if (argc >= 2 && strcmp(argv[1], "sweep") == 0) {
        return cmd_sweep(argc, argv);
    }
    fprintf(stderr, "usage: oracle info | oracle sweep ...\n");
    return 2;
}
"#;

// ===========================================================================
// Deterministic corpora — the five shapes of AAP §0.6.4 sweep 2
// ===========================================================================

/// A `xorshift64*` generator, so every corpus is bit-for-bit reproducible on
/// any machine without depending on the `rand` dev-dependency (which must not
/// leak into a conformance oracle).
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

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
/// path the reference driver reads them from.
struct Corpus {
    name: &'static str,
    data: Vec<u8>,
    path: PathBuf,
}

/// Builds the five corpus shapes at `len` bytes each.
fn corpus_shapes(len: usize) -> Vec<(&'static str, Vec<u8>)> {
    // 1. Constant run: maximally compressible; every match is the longest match.
    let constant = vec![0xA5u8; len];

    // 2. High-entropy pseudo-random: effectively incompressible, so the
    //    stored-block fallback and the fruitless-search paths dominate.
    let mut rng = XorShift64::new(0x9E37_79B9_7F4A_7C15);
    let incompressible: Vec<u8> = (0..len).map(|_| rng.next_byte()).collect();

    // 3. Natural-language text: abundant mid-distance back-references.
    const PHRASE: &[u8] = b"The quick brown fox jumps over the lazy dog. \
Pack my box with five dozen liquor jugs. ";
    let mut text = Vec::with_capacity(len);
    while text.len() < len {
        let take = (len - text.len()).min(PHRASE.len());
        text.extend_from_slice(&PHRASE[..take]);
    }

    // 4. Byte ramp: a strictly periodic pattern with a 256-byte period.
    let ramp: Vec<u8> = (0..len).map(|i| (i & 0xFF) as u8).collect();

    // 5. Alternating runs and random bytes: forces the match finder to switch
    //    between long-match and no-match regimes, and exercises `Z_RLE`.
    let mut noise = XorShift64::new(0xDEAD_BEEF_CAFE_F00D);
    let mut mixed = Vec::with_capacity(len);
    let mut round = 0usize;
    while mixed.len() < len {
        let remaining = len - mixed.len();
        if round % 2 == 0 {
            let run = (61 + round.wrapping_mul(37) % 300).min(remaining);
            let byte = (round.wrapping_mul(13) & 0xFF) as u8;
            mixed.extend(std::iter::repeat_n(byte, run));
        } else {
            let run = (29 + round.wrapping_mul(53) % 200).min(remaining);
            mixed.extend((0..run).map(|_| noise.next_byte()));
        }
        round += 1;
    }

    vec![
        ("constant", constant),
        ("incompressible", incompressible),
        ("text", text),
        ("ramp", ramp),
        ("mixed", mixed),
    ]
}

// ===========================================================================
// `zlib-rs` side: compress exactly one configuration, black-box
// ===========================================================================

/// Compresses `data` with the public streaming `deflate` engine at `level`,
/// `strategy` and the framing selected by `window_bits`, with an explicit
/// `mem_level`, returning the exact compressed stream.
///
/// Mirrors what the reference driver does: a single `Z_FINISH` pass, then
/// `deflateEnd`. The destination is sized with the public `compress_bound` plus
/// slack for a gzip header/trailer and grown only if a pass reports `Ok` with
/// the buffer already full.
fn zlib_rs_deflate(
    data: &[u8],
    level: i32,
    window_bits: i32,
    mem_level: i32,
    strategy: Strategy,
) -> Vec<u8> {
    let mut strm = ZStream::new();
    deflate_init2(
        &mut strm,
        level,
        Z_DEFLATED,
        window_bits,
        mem_level,
        strategy,
    )
    .unwrap_or_else(|err| {
        panic!(
            "deflate_init2(level={level}, windowBits={window_bits}, memLevel={mem_level}, \
             strategy={strategy:?}) must succeed for a valid configuration, got {err:?}"
        )
    });

    let mut output = vec![0u8; compress_bound(data.len()) + 128];
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;

    loop {
        let outcome = deflate(&mut strm, &data[in_pos..], &mut output[out_pos..], Z_FINISH);
        in_pos += outcome.consumed;
        out_pos += outcome.produced;
        match outcome.code {
            ReturnCode::StreamEnd => break,
            ReturnCode::Ok => {
                if out_pos == output.len() {
                    output.resize(output.len() * 2, 0);
                } else if outcome.consumed == 0 && outcome.produced == 0 {
                    panic!("zlib-rs deflate stalled with Z_FINISH before StreamEnd");
                }
            }
            other => panic!("zlib-rs deflate returned {other:?}"),
        }
    }

    deflate_end(&mut strm).expect("deflate_end must succeed");
    output.truncate(out_pos);
    output
}

// ===========================================================================
// Building the reference oracle out-of-tree
// ===========================================================================

/// A built reference oracle: the driver binary plus the corpora both sides use.
struct Oracle {
    work: PathBuf,
    driver: PathBuf,
    corpora: Vec<Corpus>,
}

/// The process-wide oracle, built at most once however many tests ask for it.
static ORACLE: OnceLock<Option<Oracle>> = OnceLock::new();

/// Returns the built oracle, or [`None`] when this machine cannot produce one.
///
/// [`None`] is not a failure: it means "no usable C toolchain here", which is an
/// expected condition in a Rust-only CI row. The reason is printed once so the
/// skip is never silent.
fn oracle() -> Option<&'static Oracle> {
    ORACLE.get_or_init(build_oracle).as_ref()
}

/// Repository root — the directory holding `Cargo.toml` and the retained C
/// baseline. Cargo sets this for every integration test.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Corpus length for this run, honouring [`CORPUS_BYTES_ENV`].
fn corpus_len() -> usize {
    match std::env::var(CORPUS_BYTES_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n >= 1024 => n,
            _ => panic!("{CORPUS_BYTES_ENV} must be an integer >= 1024, got {raw:?}"),
        },
        Err(_) => DEFAULT_CORPUS_BYTES,
    }
}

/// Prints the capability notice explaining why the live sweep was skipped.
fn notice(reason: &str) {
    println!(
        "\n[c_oracle] SKIPPED live C-oracle sweep: {reason}\n\
         [c_oracle] This is not a failure. Byte-identity remains gated by the always-on,\n\
         [c_oracle] toolchain-free tier-1 vectors in tests/interop.rs. Install a C compiler\n\
         [c_oracle] (or set CC) to run the live sweep against the retained C baseline.\n"
    );
}

/// Candidate C compilers, in priority order: `$CC` first, then the usual names.
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

/// Returns the first candidate that answers `--version`, or [`None`].
fn probe_compiler() -> Option<String> {
    compiler_candidates().into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    })
}

/// Creates the private working directory for this run under the system
/// temporary directory. Nothing is ever written inside the repository.
fn make_work_dir() -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("zlib-rs-c-oracle-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Renders a captured process failure into a diagnosable one-line reason.
fn describe_failure(what: &str, output: &std::process::Output) -> String {
    let mut reason = format!("{what} failed with {}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
    if !tail.is_empty() {
        reason.push_str(" — ");
        for (i, line) in tail.iter().rev().enumerate() {
            if i > 0 {
                reason.push_str(" | ");
            }
            reason.push_str(line.trim());
        }
    }
    reason
}

/// Probes for a compiler, builds the reference C library and the driver, and
/// materializes the corpora. Any environmental problem yields [`None`] plus a
/// printed notice rather than a failure.
fn build_oracle() -> Option<Oracle> {
    let Some(cc) = probe_compiler() else {
        notice("no usable C compiler found (tried $CC, cc, gcc, clang)");
        return None;
    };

    let work = match make_work_dir() {
        Ok(dir) => dir,
        Err(err) => {
            notice(&format!(
                "could not create a temporary work directory: {err}"
            ));
            return None;
        }
    };

    let root = repo_root();
    let missing: Vec<&str> = REFERENCE_SOURCES
        .iter()
        .copied()
        .filter(|src| !root.join(src).is_file())
        .collect();
    if !missing.is_empty() {
        notice(&format!(
            "the retained C baseline is incomplete (missing: {})",
            missing.join(", ")
        ));
        return None;
    }

    // Compile the reference translation units. The compiler runs *inside* the
    // temporary directory with absolute source paths, so every `.o` lands there
    // and the repository tree is left untouched.
    let mut compile = Command::new(&cc);
    compile
        .current_dir(&work)
        .args([
            "-O2",
            "-DNDEBUG",
            "-D_LARGEFILE64_SOURCE=1",
            "-DHAVE_UNISTD_H",
        ])
        .arg(format!("-I{}", root.display()))
        .arg("-c");
    for src in REFERENCE_SOURCES {
        compile.arg(root.join(src));
    }
    match compile.output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            notice(&describe_failure(
                "compiling the reference C baseline",
                &out,
            ));
            return None;
        }
        Err(err) => {
            notice(&format!("could not run the C compiler {cc:?}: {err}"));
            return None;
        }
    }

    // Emit and link the driver.
    let driver_src = work.join("oracle_driver.c");
    if let Err(err) = fs::write(&driver_src, DRIVER_SOURCE) {
        notice(&format!("could not write the reference driver: {err}"));
        return None;
    }
    let driver = work.join("oracle_driver");
    let objects: Vec<PathBuf> = REFERENCE_SOURCES
        .iter()
        .map(|src| work.join(Path::new(src).with_extension("o")))
        .collect();
    let mut link = Command::new(&cc);
    link.current_dir(&work)
        .args(["-O2", "-DNDEBUG"])
        .arg(format!("-I{}", root.display()))
        .arg("-o")
        .arg(&driver)
        .arg(&driver_src)
        .args(&objects);
    match link.output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            notice(&describe_failure(
                "linking the reference oracle driver",
                &out,
            ));
            return None;
        }
        Err(err) => {
            notice(&format!(
                "could not link the reference oracle driver: {err}"
            ));
            return None;
        }
    }

    // Materialize the corpora for the driver to read.
    let len = corpus_len();
    let mut corpora = Vec::new();
    for (name, data) in corpus_shapes(len) {
        let path = work.join(format!("corpus_{name}.bin"));
        if let Err(err) = fs::write(&path, &data) {
            notice(&format!("could not write the {name} corpus: {err}"));
            return None;
        }
        corpora.push(Corpus { name, data, path });
    }

    println!(
        "[c_oracle] reference C zlib built with {cc:?} from {} in-tree translation units; \
         corpora = {} shapes x {len} bytes; work dir = {}",
        REFERENCE_SOURCES.len(),
        corpora.len(),
        work.display()
    );

    Some(Oracle {
        work,
        driver,
        corpora,
    })
}

// ===========================================================================
// Reading the oracle vector stream
// ===========================================================================

/// One `(configuration, compressed bytes)` record produced by the driver.
struct Record {
    corpus: usize,
    window_bits: i32,
    mem_level: i32,
    level: i32,
    strategy: i32,
    compressed: Vec<u8>,
}

/// Reads `buf` fully, returning `false` on a clean end of stream.
fn read_record_bytes(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated oracle record",
                ));
            }
            n => filled += n,
        }
    }
    Ok(true)
}

/// Reads the next record, or [`None`] at end of stream.
fn next_record(reader: &mut impl Read) -> std::io::Result<Option<Record>> {
    let mut header = [0u8; 24];
    if !read_record_bytes(reader, &mut header)? {
        return Ok(None);
    }
    let field =
        |i: usize| i32::from_ne_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let length = u32::from_ne_bytes([header[20], header[21], header[22], header[23]]) as usize;
    let mut compressed = vec![0u8; length];
    if length > 0 && !read_record_bytes(reader, &mut compressed)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "oracle record payload missing",
        ));
    }
    Ok(Some(Record {
        corpus: field(0).max(0) as usize,
        window_bits: field(4),
        mem_level: field(8),
        level: field(12),
        strategy: field(16),
        compressed,
    }))
}

/// Maps a raw strategy integer onto the public [`Strategy`] enum.
fn strategy_of(raw: i32) -> Strategy {
    Strategy::from_c_int(raw).unwrap_or_else(|| panic!("strategy {raw} is not a zlib strategy"))
}

/// Renders the first divergence between two streams as a diagnosable report.
fn divergence_report(record: &Record, corpus: &str, ours: &[u8]) -> String {
    let mut report = String::new();
    let _ = write!(
        report,
        "compressed output diverged from reference C zlib\n  \
         corpus     : {corpus}\n  \
         windowBits : {}\n  memLevel   : {}\n  level      : {}\n  strategy   : {}\n  \
         reference  : {} bytes\n  zlib-rs    : {} bytes",
        record.window_bits,
        record.mem_level,
        record.level,
        record.strategy,
        record.compressed.len(),
        ours.len()
    );
    if let Some(at) = record.compressed.iter().zip(ours).position(|(a, b)| a != b) {
        let from = at.saturating_sub(8);
        let to = (at + 8).min(record.compressed.len().min(ours.len()));
        let _ = write!(
            report,
            "\n  first diff : offset {at} (reference {:#04x} vs zlib-rs {:#04x})\
             \n  reference[{from}..{to}] = {:02x?}\n  zlib-rs  [{from}..{to}] = {:02x?}",
            record.compressed[at],
            ours[at],
            &record.compressed[from..to],
            &ours[from..to]
        );
    } else {
        let _ = write!(
            report,
            "\n  first diff : none in the common prefix — the streams differ in length only"
        );
    }
    report
}

// ===========================================================================
// Tests
// ===========================================================================

/// Validates the oracle itself: the locally built reference library must be the
/// zlib the crate claims parity with, and its canonical vectors must match the
/// values `zlib-rs` computes.
///
/// Comparing against an oracle without first proving *what* the oracle is would
/// make every later assertion unfalsifiable.
#[test]
fn reference_library_identity_matches_zlib_rs() {
    let Some(oracle) = oracle() else { return };

    let output = Command::new(&oracle.driver)
        .arg("info")
        .current_dir(&oracle.work)
        .output()
        .expect("the reference driver must be executable");
    assert!(
        output.status.success(),
        "reference driver `info` failed: {}",
        describe_failure("`oracle info`", &output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("reference driver did not report {key}: {stdout:?}"))
            .trim()
            .to_owned()
    };
    let number = |key: &str| -> u64 {
        value(key)
            .parse::<u64>()
            .unwrap_or_else(|err| panic!("reference {key} is not a number: {err}"))
    };

    assert_eq!(
        value("version"),
        zlib_version(),
        "the reference C library and zlib-rs must report the same version string"
    );
    assert_eq!(number("vernum"), 0x1321, "reference ZLIB_VERNUM");
    assert_eq!(
        number("crc32") as u32,
        crc32(0, b"123456789"),
        "crc32(\"123456789\") must agree"
    );
    assert_eq!(
        number("adler32") as u32,
        adler32(1, b"123456789"),
        "adler32(\"123456789\") must agree"
    );
    assert_eq!(
        number("compress_bound_9") as usize,
        compress_bound(9),
        "compressBound(9) must agree"
    );
}

/// The headline gate: for every corpus shape × `windowBits` × `memLevel` ×
/// level × strategy, the compressed stream `zlib-rs` emits must be **byte-for-byte
/// identical** to the stream reference C zlib emits.
///
/// This is the in-repository, reproducible form of the migration's defining
/// acceptance criterion (AAP §0.6.4 / §0.8.1 D-1): the eight match-finder
/// decision points — hash function, `hash_shift` derivation, chain insertion
/// order, the `longest_match` thresholds and early exits, the lazy-match
/// `TOO_FAR` filter, block-type selection, the Huffman `<=` tie-break and the
/// verbatim `CONFIGURATION_TABLE` — are exactly what this sweep would catch a
/// regression in.
#[test]
fn compressed_output_is_byte_identical_to_reference_c_zlib() {
    let Some(oracle) = oracle() else { return };

    let window_bits = window_bits_grid();
    let expected = window_bits.len() * MEM_LEVELS.len() * LEVELS.len() * STRATEGIES.len();
    let expected_total = expected * oracle.corpora.len();

    let join = |values: &[i32]| -> String {
        values
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };

    let vectors = oracle.work.join("oracle_vectors.bin");
    let mut sweep = Command::new(&oracle.driver);
    sweep
        .current_dir(&oracle.work)
        .arg("sweep")
        .arg(&vectors)
        .arg(join(&window_bits))
        .arg(join(&MEM_LEVELS))
        .arg(join(&LEVELS))
        .arg(join(&STRATEGIES));
    for corpus in &oracle.corpora {
        sweep.arg(&corpus.path);
    }
    let produced = sweep
        .output()
        .expect("the reference driver must be executable");
    assert!(
        produced.status.success(),
        "the reference sweep failed: {}",
        describe_failure("`oracle sweep`", &produced)
    );

    let file = fs::File::open(&vectors).expect("the reference sweep must produce a vector stream");
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut magic = [0u8; 8];
    assert!(
        read_record_bytes(&mut reader, &mut magic).expect("reading the vector stream magic"),
        "the reference vector stream is empty"
    );
    assert_eq!(&magic, VECTOR_MAGIC, "unexpected vector stream magic");

    let mut checked = 0usize;
    while let Some(record) = next_record(&mut reader).expect("reading a reference vector") {
        let corpus = oracle
            .corpora
            .get(record.corpus)
            .unwrap_or_else(|| panic!("reference reported unknown corpus {}", record.corpus));
        let ours = zlib_rs_deflate(
            &corpus.data,
            record.level,
            record.window_bits,
            record.mem_level,
            strategy_of(record.strategy),
        );
        assert!(
            ours == record.compressed,
            "{}",
            divergence_report(&record, corpus.name, &ours)
        );
        checked += 1;
    }

    // The vector file is the largest artifact this harness creates; drop it as
    // soon as it has been consumed.
    let _ = fs::remove_file(&vectors);

    assert_eq!(
        checked,
        expected_total,
        "the sweep must cover the whole grid: {} corpora x {} windowBits x {} memLevels \
         x {} levels x {} strategies",
        oracle.corpora.len(),
        window_bits.len(),
        MEM_LEVELS.len(),
        LEVELS.len(),
        STRATEGIES.len()
    );
    println!(
        "[c_oracle] {checked}/{expected_total} configurations byte-identical to reference C zlib"
    );
}

/// `Z_DEFAULT_COMPRESSION` (−1) must resolve to level 6 exactly as C does, for
/// every framing and strategy.
///
/// The sentinel is checked here rather than inside the main grid so that grid
/// keeps its documented cardinality, and it is compared against the reference's
/// own level-6 output rather than against a baked constant.
#[test]
fn default_compression_sentinel_matches_reference() {
    let Some(oracle) = oracle() else { return };

    let window_bits = window_bits_grid();
    let vectors = oracle.work.join("oracle_default_level.bin");
    let mut sweep = Command::new(&oracle.driver);
    sweep
        .current_dir(&oracle.work)
        .arg("sweep")
        .arg(&vectors)
        .arg(
            window_bits
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .arg("8")
        .arg("6")
        .arg(
            STRATEGIES
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    // One representative corpus is enough: the point is the level-resolution
    // rule, which the full grid already covers at an explicit level 6.
    let corpus = oracle
        .corpora
        .iter()
        .find(|c| c.name == "mixed")
        .expect("the mixed corpus must exist");
    sweep.arg(&corpus.path);

    let produced = sweep
        .output()
        .expect("the reference driver must be executable");
    assert!(
        produced.status.success(),
        "the reference level-6 sweep failed: {}",
        describe_failure("`oracle sweep`", &produced)
    );

    let file = fs::File::open(&vectors).expect("the reference sweep must produce a vector stream");
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut magic = [0u8; 8];
    assert!(
        read_record_bytes(&mut reader, &mut magic).expect("reading the vector stream magic"),
        "the reference vector stream is empty"
    );
    assert_eq!(&magic, VECTOR_MAGIC, "unexpected vector stream magic");

    let mut checked = 0usize;
    while let Some(record) = next_record(&mut reader).expect("reading a reference vector") {
        assert_eq!(record.level, 6, "the driver was asked for level 6 only");
        let ours = zlib_rs_deflate(
            &corpus.data,
            -1,
            record.window_bits,
            record.mem_level,
            strategy_of(record.strategy),
        );
        assert!(
            ours == record.compressed,
            "Z_DEFAULT_COMPRESSION must be indistinguishable from level 6\n{}",
            divergence_report(&record, corpus.name, &ours)
        );
        checked += 1;
    }
    let _ = fs::remove_file(&vectors);

    assert_eq!(
        checked,
        window_bits.len() * STRATEGIES.len(),
        "every framing x strategy pair must be checked"
    );
}
