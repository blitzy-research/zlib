# zlib-rs

> A memory-safe, idiomatic Rust rewrite of **zlib 1.3.2.1** with byte-identical
> DEFLATE output and a C-compatible FFI drop-in layer.

`zlib-rs` is a from-scratch Rust port of the venerable [zlib](https://zlib.net/)
compression library. It implements the DEFLATE ([RFC 1951](https://datatracker.ietf.org/doc/html/rfc1951)),
zlib ([RFC 1950](https://datatracker.ietf.org/doc/html/rfc1950)), and gzip
([RFC 1952](https://datatracker.ietf.org/doc/html/rfc1952)) formats, produces
streams that are **byte-for-byte compatible** with the reference C
implementation, and ships as both an idiomatic Rust crate and a drop-in
replacement for the C `libz` shared/static library.

---

## Overview

zlib is one of the most widely deployed pieces of software in the world. This
crate reimplements it in safe Rust so that the same wire format can be produced
and consumed without the manual memory management of the original C code.

- **Full wire-format compatibility.** For a given input, compression level,
  strategy, `windowBits`, and `memLevel`, the compressed output is **byte-for-byte
  identical** to reference zlib — not merely decodable by it. This is validated
  **by default** (no C toolchain required) in `tests/interop.rs`, whose tier-1
  gate carries **4,461 baked byte-identity assertions** derived from the genuine
  C zlib 1.3.2.1-motley encoder. The same property is reproducible *live*
  in-repository through the opt-in `tests/c_oracle.rs` harness, which builds the
  retained C baseline and diffs its output: **3,750/3,750** and **50/50
  byte-identical** on the runs recorded below. Decompression accepts any valid
  zlib, raw-DEFLATE, or gzip stream, including those produced by other
  implementations.
- **Memory safety by construction.** Manual `zcalloc`/`zcfree` allocation is
  replaced by Rust ownership, borrowing, and `Drop`-based cleanup. The
  compression core (`src/deflate/`) contains **zero `unsafe`**.
- **`unsafe` isolated to the boundary — as a compile error, not a convention.**
  `src/lib.rs` carries a crate-wide `#![deny(unsafe_code)]` with exactly **two**
  narrowly scoped `#[allow(unsafe_code)]` carve-outs: `pub mod ffi`, the C ABI
  drop-in surface, and a private `mod no_std_support` holding the libc-backed
  `#[global_allocator]`, `#[panic_handler]`, and personality symbol that a
  freestanding `cdylib`/`staticlib` must supply. Every other module — `deflate`,
  `inflate`, `checksum`, `gz`, `util`, `stream.rs`, `error.rs`, `constants.rs`,
  `gz_header.rs` — measures **zero** executable `unsafe`, and a stray block there
  fails the build outright. Every `unsafe` block that does exist carries a
  `// SAFETY:` justification.
- **Zero C dependency in the shipped artifact.** The library links no C code: its
  entire runtime closure is `cfg-if 1.0.4` plus the optional `crc32fast 1.5.0`
  (itself pure Rust, and itself depending only on `cfg-if`), so there is no C
  toolchain in the build. `criterion`, `flate2`, `quickcheck`, and `rand` are
  dev-only **by contract** and never appear under `src/`.
- **Two APIs, one implementation.** Use the ergonomic Rust API for new code, or
  drop the emitted `cdylib`/`staticlib` in place of `libz` for existing C
  consumers.

## Status

**Experimental — active migration.** The crate is being brought up as a faithful
Rust reimplementation and its API surface may still shift while parity work
continues. It tracks upstream `zlib 1.3.2.1-motley` (`ZLIB_VERNUM 0x1321`).

Compression/decompression correctness and byte-identical format compatibility
are the primary acceptance criteria and are validated by default. Byte-identity,
the `no_std` test suite, and `cargo-fuzz` targets are all now in place;
performance has been measured and is reported in the [Roadmap](#roadmap).

### Measured evidence

Every figure in this README is a value observed by running the command beside it
on this tree, never an estimate. That now includes the throughput ratios *relative
to C zlib* under [Roadmap](#roadmap), which earlier revisions of this file had to
label as *attributed* because nothing in the repository re-derived them. They are
measured — but they are **not** measured by `cargo bench`, and the distinction
matters enough to state before the numbers appear:

- **Absolute Rust throughput** is reproducible here with
  `RUSTUP_TOOLCHAIN=stable cargo bench --locked`. The Criterion suite links no C
  library, so it cannot express a ratio at all; it compares this crate against
  itself across levels, profiles, tuning parameters and commits.
- **C-relative ratios** come from an out-of-tree *differential* harness: one C
  driver compiled twice, once against a reference `libz` built from the retained
  in-tree `*.c` baseline with
  `gcc -O2 -D_LARGEFILE64_SOURCE=1 -DHAVE_UNISTD_H` and once against this crate's
  `staticlib`, with the two sides interleaved pair-by-pair. Method for every ratio
  quoted below: **five interleaved A/B rep pairs**, C/RS order reversed on even
  reps, each timed phase **≥ 0.30 s**, reporting the **median** (and, where the
  spread matters, the best) of each side. The reference archive measured
  135,206 B, and the compiler was **`gcc 15.2.0`** (`Ubuntu 15.2.0-4ubuntu4`) —
  worth naming because the migration plan's own record of that build cites
  `gcc 13.3.0`, and a different C compiler is the single largest uncontrolled
  variable in any C-relative ratio. Any C99 compiler that can consume the retained
  root `zlib.h` reproduces the harness; the *numbers* it produces are that
  compiler's, not this document's.
- **`tests/c_oracle.rs`** is a *conformance* oracle for byte-identity, not a
  performance one, and is unchanged by any of the above.

One host caveat applies to every absolute number and, to a lesser degree, to every
ratio: the measuring host is a shared four-CPU quota running under a load average
near 45, with `perf_event_paranoid=2` and no `perf`/`valgrind` available. Absolute
MiB/s figures are therefore biased downward, and two harnesses timing the *same*
operation were observed to disagree by 10–30% purely from warm-up ordering, with a
0.12 s measurement window swinging ±30%. That is exactly why the method above fixes
five interleaved reps at ≥ 0.30 s and quotes medians rather than single runs. Treat
the **ordering and the ratio** as the durable claims and the absolute throughput as
host-specific.

Each row below was run with `RUSTUP_TOOLCHAIN=stable` — see
[Installation](#installation) for why that prefix matters — on stable
`rustc 1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)`, `x86_64-unknown-linux-gnu`,
except the two MSRV rows.

| Gate | Command | Result |
|------|---------|--------|
| Test suite (default features) | `cargo test --locked` | **1039 passed / 0 failed / 0 ignored** |
| Test suite (all features) | `cargo test --locked --all-features` | **1052 passed / 0 failed / 0 ignored** |
| Test suite (`no_std`) | `cargo test --locked --no-default-features` | **737 passed / 0 failed / 0 ignored** |
| Test suite (`no-std` feature) | `cargo test --locked --no-default-features --features no-std` | **737 passed / 0 failed / 0 ignored** |
| Formatting | `cargo fmt --all -- --check` | exit 0 |
| Lints | `cargo clippy --locked --all-targets --all-features -- -D warnings` | exit 0 |
| API docs | `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features` | exit 0, 0 warnings |
| Published docs | `mkdocs build --strict --site-dir "$(mktemp -d)/site"` | exit 0, 0 strict diagnostics (see note) |
| MSRV build | `cargo +1.85.0 build --locked` | exit 0 |
| MSRV type-check | `cargo +1.85.0 check --locked --all-targets --all-features` | exit 0, 0 warnings |
| Exported C symbols | `nm -D --defined-only target/release/libzlib_rs.so` | **95**, all type `T` |
| Packaged crate | `cargo package --locked --list` | **76** files; the unpacked archive re-runs its own suite at 1039 |
| Live byte-identity sweep | `cargo test --locked --features c-oracle --test c_oracle` | **3750/3750** and **50/50** byte-identical |

The 1039 default-feature tests decompose as **867** in-crate unit tests, **143**
integration tests (`checksum` 23, `gzip_compat` 17, `inflate_coverage` 30,
`interop` 30, `regression` 13, `round_trip` 19), and **29** doctests (28
runnable plus one `compile_fail`).
`--all-features` adds the 13 tests of the opt-in live C-oracle harness. Under
`--no-default-features` the total is **600** unit + **110** integration + **27**
doctests; the `gzip_compat` suite correctly reports 0 because the whole `gz*`
file API is feature-gated off. The two remaining CI feature rows measure **1039**
(`std,gzip,gz-io`) and **745** (`std,simd`).

CI does not merely run these commands — it parses every `test result:` line and
fails the job on any failure, on any *ignored* test, or on a count below a
per-row lower bound, because `cargo test` exits 0 when tests are skipped. A
separate step rejects any real `#[ignore]` or `#[cfg_attr(…, ignore)]` attribute
anywhere in `src`, `tests`, `benches`, `build.rs`, or `fuzz/fuzz_targets`.

**The ignored-test count is zero in every configuration, and stays zero.** A
capability that cannot be exercised in a given build is expressed by a
feature gate or a run-time probe that *passes with a printed notice*, never by
`#[ignore]`.

One note on the published-docs row: `mkdocs build --strict` exits 0 and emits
**zero MkDocs strict diagnostics** — no `WARNING` and no `ERROR` from MkDocs, its
plugins, or this project's content. The Material theme does print one banner of its
own, an upstream advisory from the Material for MkDocs team about the forthcoming
MkDocs 2.0. That banner is a vendor notice rather than a build diagnostic: it is
independent of this repository's content, `--strict` does not fail on it, and no
change here can suppress it.

## Highlights

- DEFLATE encoder with every match-finding strategy (`stored`, `fast`, `slow`,
  `rle`, `huffman-only`) and all ten compression levels.
- Inflate decoder with the raw-callback `inflateBack` path and streaming
  `inflateSync` error recovery.
- zlib (RFC 1950) and gzip (RFC 1952) wrappers around raw DEFLATE (RFC 1951),
  with the full `windowBits` overloading contract (zlib / raw / gzip /
  auto-detect).
- Adler-32 and CRC-32 checksums, including the `adler32_combine` /
  `crc32_combine` operations; the CRC-32 hot path is SIMD-accelerated via
  [`crc32fast`](https://crates.io/crates/crc32fast).
- All seven flush modes, preset-dictionary support, and the exact
  `compressBound` sizing formula.
- A C ABI (`#[unsafe(no_mangle)] extern "C"`) exposing the exact zlib symbol table
  with `#[repr(C)]` mirrors of the 14-field `z_stream` and the 13-field
  `gz_header`.

## Installation

> [!IMPORTANT]
> **This crate is not published on crates.io, and the name `zlib-rs` there belongs to
> an unrelated project.** Do **not** run `cargo add zlib-rs` and do **not** write
> `zlib-rs = "1.3.2"` in a manifest — neither will get you this code. Depend on it by
> Git URL or by path, as shown below.

Two facts underlie that, and three commands establish them against the live registry:

```sh
cargo search zlib-rs --limit 1
#   zlib-rs = "0.6.7"    # A memory-safe zlib implementation written in rust
cargo info zlib-rs@0.6.7 | grep repository
#   repository: https://github.com/trifectatechfoundation/zlib-rs
cargo info zlib-rs@1.3.2
#   error: could not find `zlib-rs@1.3.2` in registry
#          `https://github.com/rust-lang/crates.io-index`
```

The published `zlib-rs` 0.6.7 is an unrelated, independently developed crate from the
Trifecta Tech Foundation — a separate memory-safe zlib effort, and not this code.
Worse, the version requirement `"1.3.2"` cannot be satisfied by it at all, so the
mistake surfaces either as a confusing resolution failure or, if the requirement is
loosened, as a silent substitution of somebody else's implementation. This is textbook
dependency confusion, and the only reliable defence is to name the source explicitly.

The version in [`Cargo.toml`](Cargo.toml) is `1.3.2` because it tracks the C zlib
release this crate reimplements — it is **not** a crates.io coordinate.
[`CHANGELOG.md`](CHANGELOG.md) records that release as `1.3.2 — unreleased`,
deliberately without a date, for the same reason.

Depend on it by **source**, not by name. Point at the repository:

```toml
[dependencies]
zlib-rs = { git = "https://github.com/Blitzy-Sandbox/blitzy-zlib" }
```

Pin the revision for a reproducible build — recommended, and required if you rely
on byte-identical output, since the match-finder heuristics that guarantee it are
the code a floating branch would move underneath you:

```toml
[dependencies]
zlib-rs = { git = "https://github.com/Blitzy-Sandbox/blitzy-zlib", rev = "<40-char commit SHA>" }
```

Or vendor the tree and use a path dependency:

```toml
[dependencies]
zlib-rs = { path = "../blitzy-zlib" }
```

Either form supports the usual feature selection, for example
`default-features = false, features = ["std", "gzip"]` — see
[Feature flags](#feature-flags).

In every form the package name stays `zlib-rs` and the importable module path is
`zlib_rs`, because Cargo replaces `-` with `_`:

```rust,ignore
use zlib_rs::{compress, uncompress};
```

**C consumers do not use Cargo at all.** The drop-in replacement is the emitted
`libzlib_rs.so` / `libzlib_rs.a`, linked against the retained `zlib.h`; see
[Usage — C drop-in (FFI)](#usage--c-drop-in-ffi) for the build and link recipe. Nothing on crates.io is involved in that path.

**On the name itself.** Renaming the package to sidestep the collision is
deliberately *not* proposed: the name is load-bearing across this repository's
plan and documentation, and while the crate is unpublished a rename would trade a
documented, closed-form quirk for much larger churn. The collision has one further
consequence worth knowing about — advisory scanners match by *name plus semver
range* and consult neither the source nor the publisher, so this package is
evaluated against the other project's advisory history. That is analysed in full,
including what to do if a future advisory ever collides, in the
`THE zlib-rs NAME COLLISION` section of [`deny.toml`](deny.toml).

**Minimum Supported Rust Version (MSRV): `1.85.0`.** The crate targets the Rust
**2024 edition**, which was stabilized in Rust 1.85.0 — so 1.85.0 is the tightest
self-consistent floor an edition-2024 crate can declare, not a round number. Any
newer stable toolchain also works.

That floor is **verified, not assumed**. On `rustc 1.85.0 (4d91de4e4 2025-02-17)`
both `cargo +1.85.0 build --locked` and `cargo +1.85.0 check --locked
--all-targets --all-features` exit 0, reproducing CI's `msrv` job locally — and
because the second command carries `--all-features`, the floor is proven for the
optional rows (`inflate_strict`, `c-oracle`) too, not only for the default set.
The same tree also builds and passes its full suite on current stable
`rustc 1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)`.

A root [`rust-toolchain.toml`](rust-toolchain.toml) pins `channel = "1.85.0"`
(with `rustfmt` and `clippy`, `profile = "minimal"`) so a contributor's local
build resolves to the same compiler CI's MSRV job uses instead of floating to
whatever stable happens to be installed. **A bare `cargo …` inside this
repository therefore invokes the MSRV compiler.** To reach stable — which is what
thirteen of CI's fourteen jobs do — set `RUSTUP_TOOLCHAIN` or use a `+stable` prefix;
`rustup`'s environment override outranks the toolchain file, which is exactly why
CI's `dtolnay/rust-toolchain` steps still select the channel they ask for. Those
steps are pinned to a commit SHA and name their channel with an explicit
`toolchain:` input rather than through an `@stable`/`@nightly` ref:

```sh
# MSRV (the repository default)
cargo build --locked

# Stable, as CI runs it
RUSTUP_TOOLCHAIN=stable cargo test --locked
cargo +stable test --locked
```

## Usage — idiomatic Rust API

The public API is re-exported from the crate root (`zlib_rs::…`). The examples
below are illustrative; they mirror the crate's intended surface while the
migration stabilizes.

### One-shot compression

`compress` and `uncompress` operate on caller-provided buffers and return the
number of bytes written, mirroring the classic zlib one-call helpers. Size the
destination buffer with `compress_bound`.

```rust,ignore
use zlib_rs::{compress, uncompress, compress_bound};

let source = b"the quick brown fox jumps over the lazy dog";

// Upper bound on the compressed size, then compress into an owned buffer.
let mut packed = vec![0u8; compress_bound(source.len())];
let n = compress(&mut packed, source).expect("compress failed");
packed.truncate(n);

// Round-trip back to the original bytes.
let mut restored = vec![0u8; source.len()];
let m = uncompress(&mut restored, &packed).expect("uncompress failed");
restored.truncate(m);

assert_eq!(restored, source);
```

Choose an explicit compression level (0–9, or `Z_DEFAULT_COMPRESSION`) with
`compress2`:

```rust,ignore
use zlib_rs::{compress2, compress_bound, Z_BEST_COMPRESSION};

let source = b"payload to squeeze as hard as possible";
let mut packed = vec![0u8; compress_bound(source.len())];
let n = compress2(&mut packed, source, Z_BEST_COMPRESSION).expect("compress2 failed");
packed.truncate(n);
```

### Streaming deflate / inflate

For incremental work, build a configuration and drive a `ZStream`. The
`DeflateConfig` builder replaces the multi-argument `deflateInit2_` call, and
cleanup is automatic via `Drop` (no explicit `deflateEnd` required, though it is
available for parity).

```rust,ignore
use zlib_rs::{ZStream, FlushMode};
use zlib_rs::deflate::{DeflateConfig, deflate, deflate_end};

// Configure: level 6, default method / window / memory / strategy.
let mut strm = ZStream::new();
DeflateConfig::new()
    .level(6)
    .init(&mut strm)
    .expect("deflateInit failed");

// `ZStream` carries no cursor fields, so each `deflate` call is handed the
// input and output slices plus a raw flush code; drive it in a loop until the
// returned `DeflateOutcome` reports `ReturnCode::StreamEnd`:
//
//     loop {
//         let outcome = deflate(&mut strm, input, output, FlushMode::Finish.as_c_int());
//         // advance the buffers by `outcome.consumed` / `outcome.produced` …
//         if outcome.code == ReturnCode::StreamEnd { break; }
//     }

deflate_end(&mut strm).ok();
```

The inflate side mirrors this with `inflate_init` / `inflate` / `inflate_end`,
and `inflate_init2` accepts a `windowBits` value to select zlib, raw, gzip, or
auto-detect framing. All seven flush modes (`Z_NO_FLUSH` … `Z_TREES`) are
honored.

### Checksums

Adler-32 and CRC-32 follow the zlib seeding convention: start Adler-32 from `1`
and CRC-32 from `0`, then fold in successive slices.

```rust,ignore
use zlib_rs::{adler32, crc32};

let data = b"the quick brown fox";

// Running Adler-32 (seed 1) and CRC-32 (seed 0).
let a = adler32(1, data);
let c = crc32(0, data);

// Chunked updates produce the same result as a single call.
let (head, tail) = data.split_at(9);
let a_chunked = adler32(adler32(1, head), tail);
assert_eq!(a, a_chunked);
let _ = c;
```

Use `adler32_combine` / `crc32_combine` to merge the checksums of two
concatenated segments without rescanning the data.

### gzip file I/O

The gzip layer offers a `.gz`-aware file API layered over `std::fs` and
`std::io`, closely following zlib's `gz*` functions. It is available with the
default `gz-io` feature.

The `gz*` family reports failure through return values rather than panics, so
every result has to be inspected — a discarded return code silently loses a
short write or an unflushed trailer:

```rust,ignore
use zlib_rs::ReturnCode;
use zlib_rs::gz::{gzclose, gzopen, gzwrite};

let payload: &[u8] = b"hello, gzip\n";

let mut file = gzopen("hello.txt.gz", "wb").expect("gzopen failed");

// `gzwrite` returns the number of bytes written, or 0 on error. A short write
// is a failure, so it is compared against the full payload length.
let written = gzwrite(&mut file, payload);
assert_eq!(
    written,
    payload.len() as i32,
    "gzwrite wrote {written} of {} bytes",
    payload.len(),
);

// `gzclose` is mandatory rather than optional: `GzState`'s `Drop` is
// deliberately empty of finishing logic, because a destructor cannot surface a
// deferred compression or I/O error. Dropping a writer releases its buffers and
// file descriptor but leaves the final deflate block and the gzip trailer
// unwritten, so the close must happen explicitly *and* its code must be checked.
let rc = gzclose(file);
assert_eq!(
    rc,
    ReturnCode::Ok.as_c_int(),
    "gzclose failed: {:?}",
    ReturnCode::from_c_int(rc),
);
```

## Usage — C drop-in (FFI)

For existing C/C++ consumers, `zlib-rs` emits `cdylib` and `staticlib`
artifacts that expose the **exact zlib C API**. The FFI layer in `src/ffi/`
provides `#[unsafe(no_mangle)] extern "C"` shims for every public zlib prototype
and `#[repr(C)]` mirror structs for `z_stream` and `gz_header`, reproducing the C
field order and type widths so the emitted object can substitute for `libz`
without recompiling downstream code.

What "exact" means here is enumerated rather than asserted. The mirrors reproduce
the **14-field** `z_stream_s` and the **13-field** `gz_header_s` field for field,
and `gzFile_s` keeps its `{ have, next, pos }` prefix because C's `gzgetc` is a
*macro* that dereferences it directly. All five versioned initialiser entry points
exist — `deflateInit_`, `deflateInit2_`, `inflateInit_`, `inflateInit2_`,
`inflateBackInit_` — because that is what the `zlib.h` macros expand to.
`zlibVersion()` returns `"1.3.2.1-motley"` and `ZLIB_VERNUM` is `0x1321`.

### Exported symbol reconciliation

The symbol table is not approximately right; it reconciles exactly, and the
arithmetic is checked by tests in `src/ffi/mod.rs` rather than by inspection:

| Quantity | Value |
|----------|-------|
| `#[unsafe(no_mangle)]` attribute sites across the four shim modules | **98** (`ffi/deflate.rs` 17, `ffi/inflate.rs` 22, `ffi/util.rs` 25, `ffi/gz.rs` 34) |
| Distinct exported names they resolve to | **96** — two names (`gzdopen`, `inflateGetHeader`) have a `cfg`-paired pair of definitions |
| Platform-gated away on Linux | **1** — `gzopen_w`, `#[cfg(windows)]` in Rust and `#if defined(_WIN32) && !defined(Z_SOLO)` in `zlib.h` |
| `T` symbols in `libzlib_rs.so` (`nm -D --defined-only`) | **95** — and *nothing else*: every exported symbol is a `T` |
| `zlib.map` `global:` names present | **54 / 54** |
| `zlib.map` `local:` names leaked | **0 / 10** |
| Emitted but undeclared | **none** |

96 declared − 1 platform-gated = **95 emitted**. The ten `local:` names —
`deflate_copyright`, `inflate_copyright`, `inflate_fast`, `inflate_fixed`,
`inflate_table`, `zcalloc`, `zcfree`, `z_errmsg`, `gz_error`, `gz_intmax` — are all
correctly hidden. The other 41 emitted symbols are the classic entry points that
predate `ZLIB_1.2.0` (`deflate`, `inflate`, `crc32`, `adler32`, `compress`,
`gzopen`, `zlibVersion`, the four `*Init*_` shims, and the rest). `zlib.map` does
not list them in any node because it enumerates only what each release *added*
from `ZLIB_1.2.0` onward; the older base API is public but unversioned, and the
script's sole wildcard — the `_*` under `ZLIB_1.2.0`'s `local:` — cannot hide it
because none of the 41 begins with an underscore. Every one of the 41 is a
`ZEXTERN` declaration in the retained upstream `zlib.h`. 54 + 41 = 95.

A `cfg(test)` guard in `src/ffi/mod.rs` coerces each exported function *item* to
its exact `unsafe extern "C"` fn-pointer *type*, so any drift in an argument,
return type, or calling convention becomes a **compile error** rather than
something a C caller discovers at run time. That coverage is **exhaustive, not
representative**: all **96** names are bound, a strict superset of the 54
`zlib.map` globals, and a test re-derives the export list from the shim sources so
adding a symbol without adding its guard fails automatically.

### Verified drop-in behaviour

Both link modes were exercised with a small C consumer compiled against the
retained upstream `zlib.h`, unmodified:

```text
# statically linked against target/release/libzlib_rs.a
ver=1.3.2.1-motley crc=cbf43926 adler=091e01de compress=0 uncompress=0 bound=22
vernum=0x1321 roundtrip=exact

# dynamically linked against target/release/libzlib_rs.so
gzip magic=1f 8b  compressed=30918  restored=50000  inflate_rc=1  bytes=byte-exact
```

The dynamic run is a 50,000-byte streaming round trip at `windowBits = 31`,
recovered byte-exactly with correct `1f 8b` gzip framing. Every field above is a
fixed expectation except `compressed=`, which is a property of that run's input
bytes: re-running with different 50,000 bytes changes the compressed length and
nothing else. CI's `c-abi-linkage`
job runs the equivalent check on four feature rows and additionally asserts that
the dynamic export set matches the `zlib.map` contract on every one of them.

Release artifact sizes, observed on **2026-08-05** with **stable 1.97.1**, the
**default** feature set, `cargo build --locked --release`, into this repository's
default `target/release/` (no `CARGO_TARGET_DIR` override): `libzlib_rs.rlib`
≈ 2.8 MiB (2,977,116 B), `libzlib_rs.so` ≈ 539 KiB (551,760 B), `libzlib_rs.a`
≈ 21.4 MiB (22,468,092 B). The `.so` figure reflects `strip = "symbols"` in
`[profile.release]`; the `.a` is unaffected, because Cargo's `strip` reaches only
linked outputs and a static archive is not linked.

Read those as a dated, environment-specific observation rather than a budget or an
invariant. No gate asserts them; they move with the compiler, the feature row, and
the profile — the same build on the MSRV floor's older LLVM comes out marginally
smaller, as [`rust-toolchain.toml`](rust-toolchain.toml) records. The `.rlib` figure
is the least durable of the three, because an rlib embeds absolute build paths and
therefore changes size when the checkout directory or `CARGO_TARGET_DIR` changes,
with no change to a single line of source. Exact byte counts are deliberately not
published here: reproduce them locally with
`stat -c '%s' target/release/libzlib_rs.{rlib,so,a}` if you need them.

> **`gzclose` stays mandatory.** `GzState`'s `Drop` frees every buffer and tears
> down the engine, but is **intentionally empty of *finishing* logic**: a
> destructor cannot report a deferred compression or I/O error, so the final
> `deflate(…, Z_FINISH)` and the gzip trailer are emitted only by an explicit
> `gzclose`/`gzclose_w`, exactly as reference zlib does. Dropping a writer
> without closing it therefore leaves an unfinished member on disk. This is a
> deliberate, documented divergence from idiomatic Rust cleanup — pinned from
> both sides by tests — and not a defect to be "fixed" into an auto-finishing
> destructor.

> **Variadic `gzprintf`/`gzvprintf`.** Both symbols are **always exported** so
> the artifact presents the complete zlib symbol table. Genuine C-variadic
> formatting depends on the unstable `c_variadic` language feature (nightly
> only), which would break the crate's stable build and its MSRV contract
> (AAP §0.7.2) and its zero-C-dependency guarantee (AAP §0.5.2). The FFI layer
> therefore ships the **documented no-`vsnprintf` zlib build variant**: the two
> shims return `Z_STREAM_ERROR` rather than formatting — exactly as a zlib built
> without a secure `*printf` does — and `zlibCompileFlags` reports this by
> setting bit 27. Every other public prototype is fully implemented, and the
> idiomatic Rust `gzprintf` (which takes `core::fmt::Arguments` instead of a
> C `va_list`) formats fully.

> **The allocator is bound at init, not re-read per allocation.** A stream's
> `zalloc`/`zfree`/`opaque` triple is captured by `deflateInit*`/`inflateInit*`/
> `inflateBackInit*`; writing new values into those fields on a *live* stream has
> no effect, including on inflate's window, which is deferred until a block needs
> it. Reference C re-reads the fields inside its `ZALLOC` macro, so install a
> counting `zalloc` after `inflateInit2_` and C records one hook allocation where
> this crate records none. `zlib.h` documents the triple as an input to the init
> call and never sanctions changing it afterwards, so no conforming caller is
> affected — and matching C here would be **less** sound, not merely more work: in
> that same measurement C's ledger shows **one** allocation taken from the caller's
> `zalloc` against **two** pointers handed back to the caller's `zfree`, because
> the stream state came from C's internal `zcalloc` before the hooks existed. C
> thus mixes two allocators inside one stream and passes a foreign pointer to a
> caller-supplied deallocator. Here every buffer frees through the hook that
> allocated it, so **one buffer, one allocator** holds by construction rather than
> by convention. The decompressed payload is byte-identical either way.

Build the shared and static objects:

```sh
RUSTUP_TOOLCHAIN=stable cargo build --locked --release
# target/release/libzlib_rs.so   (cdylib — the drop-in libz replacement)
# target/release/libzlib_rs.a    (staticlib — for static linking)
```

Because the exported symbol table matches zlib's, the shared object can be
injected in place of the system library — for example via `LD_PRELOAD` — so a
C program that calls `deflate`, `inflate`, `crc32`, `gzopen`, and friends runs
against the Rust implementation unchanged:

```sh
LD_PRELOAD=./target/release/libzlib_rs.so ./your_existing_c_program
```

Return codes (`Z_OK = 0` … `Z_VERSION_ERROR = -6`), buffer bounds, and streaming
semantics are surfaced at the boundary exactly as C callers expect, even though
the internal implementation uses Rust `Result` types.

### Symbol versioning

The exported symbol **set** matches zlib's exactly, as the reconciliation table
above shows — but the symbols carry **no `@ZLIB_x.y.z` version tags by default**.
That is a deliberate, documented divergence (AAP §0.8.2 Divergence 4, ranked Low
as gap D8), and it leaves static linking, ordinary dynamic linking, `-lz`
substitution, and the `LD_PRELOAD` form above completely unaffected — none of them
prints anything extra.

The one form that notices is installing the artifact *as* `libz.so.1` for a
program that was linked against a versioned distribution `libz`: glibc's loader
then prints `no version information available` once per distinct `ZLIB_x.y.z`
node that program requires (measured 0, 4, and 9 lines for consumers requiring
0, 4, and 9 nodes) before the program runs — correctly, with identical results
either way. Opting in silences it:

```sh
ZLIB_RS_VERSION_SCRIPT=1 RUSTUP_TOOLCHAIN=stable cargo build --locked --release
```

`build.rs` then derives a version script from `zlib.map` and applies it to the
`cdylib` only, yielding the same 95 exported symbols with 54 of them tagged and
all 16 `ZLIB_*` nodes declared. `1`, `true`, `yes`, and `on` all switch it on and
`0`, `false`, `no`, `off`, and an empty value switch it off (case-insensitively,
after trimming); any other value fails the build rather than silently guessing,
so a typo cannot quietly leave it disabled. It is off by default because a version
script is a GNU-ld/ELF-only construct and **no CI row sets the variable**, so the
opt-in path itself carries linker-portability risk that the matrix does not yet
retire — the honest reason to leave it off rather than to advertise it as enabled.
On any target or toolchain where one of its clauses does not hold, the build prints
a `cargo:warning` naming that clause and links unversioned rather than failing. The
mechanism, the target clauses, and the full measurements are documented in the
"Optional cdylib symbol versioning" section of `build.rs`.

## Feature flags

Feature flags map the C preprocessor conditionals (`GZIP`, `NO_GZCOMPRESS`,
`Z_SOLO`, …) onto Cargo features. The names and defaults below are the
authoritative contract and are kept in sync with `Cargo.toml`.

| Feature          | Default | Description |
|------------------|:-------:|-------------|
| `std`            | ✅ | Standard-library build (I/O, allocation, formatting). |
| `gzip`           | ✅ | gzip framing within the deflate/inflate engines (maps C `#ifdef GZIP`). |
| `gz-io`          | ✅ | gzip **file** I/O layer — the `gz*` functions (maps C `#ifndef NO_GZCOMPRESS`); implies `std` + `gzip`. |
| `no-std`         |    | Core-only, bare-metal build with no gz file I/O (maps C `Z_SOLO`). |
| `simd`           | ✅ | SIMD-accelerated CRC-32 via [`crc32fast`](https://crates.io/crates/crc32fast). |
| `inflate_strict` |    | Stricter inflate distance validation (maps the C `INFLATE_STRICT` switch). Off by default so the default build stays byte-exact with reference zlib; enable only to reject out-of-window distances early. |
| `c-oracle`       |    | Unlocks the opt-in live byte-identity harness `tests/c_oracle.rs`, which shells out to a system C compiler to build reference zlib from the retained in-tree `*.c` sources and diffs live compressed output. Expands to `[]`, so it adds **no dependency** — `Cargo.lock` and `cargo metadata` are untouched. |

The default feature set is `std`, `gzip`, `gz-io`, and `simd`. The non-default
features `inflate_strict` and `c-oracle` are optional opt-ins beyond that core
set; neither is required for a complete drop-in ABI.

`c-oracle` is worth a note because it is the one feature that reaches outside the
Rust toolchain. The harness is declared in `Cargo.toml` with
`required-features = ["c-oracle"]`, so without the feature Cargo does not even
compile the file and the default suite behaves exactly as if the target were
absent — which is what preserves the crate's most valuable testing property: **a
plain `cargo test` needs no C toolchain**, because tier 1 of `tests/interop.rs`
is always-on and made of precomputed constants. There is no `cc`, `bindgen`, or
`pkg-config` build-dependency, no `[build-dependencies]` table, and no `links =`
key anywhere in the manifest. `--all-features` does enable `c-oracle`; on a
machine with no usable C compiler the harness probes at **run time**, prints a
clearly marked capability notice, and **passes** — it is never `#[ignore]`d.

To build a core-only, no-standard-library configuration, disable the defaults:

```sh
RUSTUP_TOOLCHAIN=stable cargo build --locked --no-default-features
```

To pick individual features explicitly, combine `--no-default-features` with
`--features`:

```sh
RUSTUP_TOOLCHAIN=stable cargo build --locked --no-default-features --features "std,gzip,simd"
```

## Building, testing, and benchmarking

The crate builds with a stock Cargo toolchain — **no CMake, `./configure`, or
`make` required**.

```sh
# Build (debug / optimized)
RUSTUP_TOOLCHAIN=stable cargo build --locked
RUSTUP_TOOLCHAIN=stable cargo build --locked --release

# Run the full test suite (unit, integration, and doc tests)
RUSTUP_TOOLCHAIN=stable cargo test --locked

# Lint with Clippy, treating warnings as errors
RUSTUP_TOOLCHAIN=stable cargo clippy --locked --all-targets --all-features -- -D warnings

# Verify formatting (cargo-fmt is a rustfmt wrapper and rejects --locked)
RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check

# Run the Criterion benchmarks
RUSTUP_TOOLCHAIN=stable cargo bench --locked
```

Both parts of every line above are deliberate. `RUSTUP_TOOLCHAIN=stable` is needed
because of the toolchain pin described under [Installation](#installation) — a bare
`cargo …` here resolves to MSRV 1.85.0, which is a supported configuration but not
the one CI's gates run on. `--locked` keeps the two committed lockfiles
authoritative, so running a gate can never rewrite them as a side effect. The one
line without `--locked` is the formatting check, because `cargo-fmt` is a rustfmt
wrapper rather than a build command and rejects the flag outright.

### The blocking quality gates

These seven are the gates that must stay green, and **all seven currently exit 0**
on this tree — the results are tabulated under
[Measured evidence](#measured-evidence):

```sh
RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check
RUSTUP_TOOLCHAIN=stable cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTUP_TOOLCHAIN=stable cargo build --locked
RUSTUP_TOOLCHAIN=stable cargo test  --locked
RUSTUP_TOOLCHAIN=stable cargo test  --locked --no-default-features
RUSTUP_TOOLCHAIN=stable RUSTDOCFLAGS='-D warnings' \
  cargo doc --locked --no-deps --all-features
mkdocs build --strict --site-dir "$(mktemp -d)/site"
```

The last two are what CI's `docs` job runs. Rustdoc warnings are **denied**, not
merely printed, so a broken intra-doc link or a malformed doc attribute fails the
build; and the published MkDocs site is built with `--strict`, which promotes a
missing nav page or a dangling internal link to an error. `--site-dir` sends the
output outside the checkout, exactly as CI does (`--site-dir "$RUNNER_TEMP/site"`):
MkDocs otherwise defaults `site_dir` to `<repo>/site`, so a bare build drops 60
generated files into the working tree and dirties `git status`. `/site/` is in
`.gitignore` as a backstop, but the flag is the primary fix because it leaves the
checkout untouched rather than merely unstaged. The MkDocs environment
is version-pinned in CI (Python 3.12, `mkdocs 1.6.1`, `mkdocs-techdocs-core
1.7.0`, `mkdocs-mermaid2-plugin 1.2.3`) so the gate cannot change verdict because
an upstream release shifted underneath it.

No warning is downgraded, no lint is `allow`-ed to make a change land, and no test
is `#[ignore]`d. `--all-features` on the Clippy line is load-bearing rather than
decorative: without it Clippy never sees `tests/c_oracle.rs` (which carries
`required-features`), the `inflate_strict` arms, or the `no_std` runtime block —
precisely where a lint regression would hide longest, since no other job promotes
warnings to errors. `--locked` keeps both lockfiles immutable; it appears on five of
the six lines and is absent from `cargo fmt` because `cargo-fmt` is a rustfmt wrapper
rather than a build command and rejects the flag with
`error: unexpected argument '--locked' found`.

Lint and format behaviour is pinned rather than inherited from whatever tool
version a contributor happens to have: [`clippy.toml`](clippy.toml) sets
`msrv = "1.85.0"` and [`rustfmt.toml`](rustfmt.toml) fixes `edition` and
`style_edition` to 2024 along with width, indentation, and import-ordering.

To run the opt-in live byte-identity sweep against a freshly built reference C
zlib (needs a C compiler; see [Feature flags](#feature-flags)):

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --features c-oracle --test c_oracle -- --nocapture
```

### Supply chain

The governed dependency closure is **102 packages** — 89 pinned by the root
[`Cargo.lock`](Cargo.lock) and 13 by [`fuzz/Cargo.lock`](fuzz/Cargo.lock), all
from crates.io except the fuzz workspace's single `path = ".."` self-reference.
Both lockfiles are committed deliberately, because the crate ships
`cdylib`/`staticlib` distributables and reproducible offline builds need exact
resolved versions.

**One** `cargo-deny` policy governs that closure: [`deny.toml`](deny.toml), over
the 89 root packages and the 13 fuzz-only packages alike. `cargo-deny` resolves one
graph per invocation, so there are two commands and one rulebook, and each names the
policy explicitly — because `cargo-deny` otherwise resolves configuration from the
*target* manifest's workspace root and a discovery miss falls back to built-in
defaults, silently, and a silent fallback looks exactly like a pass. The fuzz
command is precisely that hazard, since `fuzz/` is a detached workspace holding no
policy of its own:

```sh
# Root graph: 89 packages.
cargo deny --locked --config deny.toml check \
  -A unused-wrapper -A license-exception-not-encountered

# Detached fuzz workspace: 13 packages, against the same policy.
cargo deny --locked --manifest-path fuzz/Cargo.toml --config deny.toml check \
  -A license-not-encountered -A unmatched-skip -A unnecessary-skip
```

Both command lines are reproduced exactly as
[`.github/workflows/audit.yml`](.github/workflows/audit.yml) runs them, `-A`
allowances included, and both report `0 errors, 0 warnings` on this tree. The five
allowances are the entire cost of one policy spanning two graphs, and each is
downgraded **only on the graph where the entry it covers cannot match** — so no code
is waived where it could report something real, and every one of the five stays at
full severity on the other invocation.

The policy sets `[advisories]`, `[licenses]`, `[bans]`, and `[sources]` with
`all-features = true`, denies yanked crates, and bounds advisory staleness. Its
`[graph] targets` list is deliberately **empty**, which is what makes all 89 root
crates reachable by the licence and ban checks: naming triples *prunes* the graph,
and nine triples were measured to cut coverage to 86, silently excluding the
`spirv`-only and `uefi`-only leaves. Duplicate major versions are held to
`[bans] multiple-versions = "deny"` together with
`multiple-versions-include-dev = true` — the second key is load-bearing, because
every duplication in this project is dev-only and the check would otherwise report
`bans ok` regardless — and the four acknowledged duplicates are pinned to exact
`crate@version` `skip` entries so an *unreviewed* duplicate fails the build rather
than merely printing a warning. The two things the fuzz graph legitimately needs —
a C-toolchain build dependency and an NCSA-licensed `libfuzzer-sys` — are **scoped,
not waived**, which is exactly what lets one file govern both graphs: `cc` stays
banned but carries `wrappers = ["libfuzzer-sys"]`, so it is admitted only as that
crate's build dependency, and NCSA is granted crate-scoped rather than globally. On
the root graph neither crate is present, so those two entries are latent defence in
depth and the root invocation allows exactly their two "unused configuration" codes;
on the fuzz graph they are load-bearing and run at full severity. Both invocations
report `0 errors, 0 warnings` across all four checks.

[`.github/workflows/audit.yml`](.github/workflows/audit.yml) runs the gate on
push, pull request, a daily schedule, and manual dispatch, as four independent
blocking jobs — none declares `needs:`, so one failing category can never mask
another's verdict:

| Job | What it proves |
| --- | --- |
| `policy-integrity` | `deny.toml` is present, still declares every governed table, still holds every load-bearing key at its reviewed value, and is still the **only** cargo-deny policy in the tree. Guards against a deleted policy, against section-level erosion (which would otherwise pass vacuously), and against a second policy appearing and quietly giving one graph its own rulebook. |
| `cargo-audit` | No known advisory affects either lockfile — the root graph and the detached fuzz graph are both scanned. |
| `cargo-deny` | The root graph satisfies all four categories against `deny.toml`: licences, advisories, bans, sources. |
| `cargo-deny-fuzz` | The detached fuzz graph satisfies the same four categories against the same `deny.toml`. |

Those four jobs are the **only** place either tool runs.
[`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) builds and fuzzes the
harnesses and nothing else — it declares no `cargo-deny` job — so the
supply-chain *gate* has exactly one home and the tool pin, the `--locked`
discipline and the lockfile assertion cannot drift between two copies of it. (Two
policy *files* are not two gates: they are one gate reading the right rulebook per
graph, and `policy-integrity` asserts they never disagree.) The
trade-off is recorded rather than glossed: because a job in another workflow
cannot be a `needs:` predecessor, a policy verdict can no longer sequence *ahead
of* a fuzzing campaign within one run. It still blocks the same pull request, as
a sibling check, and `cargo-deny-fuzz` sees a strictly broader set of triggers
(push, pull request, daily, manual) than a fuzz-workflow job could (`fuzz.yml`
has no `push` trigger at all).

Every invocation passes `--locked`, so a verdict always describes the pins that
are actually committed rather than a graph resolved on the runner, and each job
asserts afterwards that neither lockfile moved. The tools themselves are
version-pinned (`cargo-deny` 0.20.2, `cargo-audit` 0.22.2) and the resolved
version is asserted rather than merely logged, because `--locked` pins a tool's
own lockfile and not which release of the tool gets installed. Neither tool is
ever a manifest dependency.

Offline builds work from a warmed Cargo cache. The development dependencies
([`criterion`](https://crates.io/crates/criterion),
[`flate2`](https://crates.io/crates/flate2),
[`quickcheck`](https://crates.io/crates/quickcheck), and
[`rand`](https://crates.io/crates/rand)) are used only for benchmarking and
compatibility testing; `flate2` uses its pure-Rust `miniz_oxide` backend so the
test graph needs no C toolchain either.

## Project layout

The crate mirrors the six functional layers of the C baseline as Rust modules,
adds a public-API-types layer, and isolates the C ABI in a dedicated FFI
boundary:

```text
src/
├── lib.rs            crate root, module declarations, public re-exports
├── error.rs          ZlibError enum + ReturnCode
├── constants.rs      flush modes, levels, strategies, windowBits contract
├── stream.rs         idiomatic ZStream<A: Allocator>
├── gz_header.rs      GzHeader
├── deflate/          compression engine (zero unsafe): mod, state, trees,
│                     strategy, fast, slow, stored, huff, rle
├── inflate/          decompression engine: mod, state, fast, tables, fixed, back
├── checksum/         adler32, crc32 (SIMD hot path), combine ops
├── gz/               gzip file I/O: mod, state, open, read, write, close
├── util/             one-call wrappers, version reporting
└── ffi/              extern "C" drop-in boundary: mod, types, alloc,
                      deflate, inflate, gz, util
```

That is **40 modules** under `src/`, layered as
`error`/`constants` → `util` → `checksum` → `stream`/`gz_header` →
`{deflate, inflate}` → `gz` → `ffi`, mirroring the C `#include` layering with no
extra top-level modules.

**The layering is acyclic, and a test enforces it.** Every `use crate::…` in the
shipped library points at a *strictly lower* layer. There are no upward edges and
no same-layer edges, so `stream`/`gz_header` and `deflate`/`inflate` are strict
peers that never reference each other, and the module graph contains no cycle at
all.

Nothing in the compiler enforces that — a Rust crate is one compilation unit, so an
upward `use` compiles perfectly and the layering would erode silently. The
invariant is therefore mechanical: `the_module_graph_has_no_upward_edges` in
`src/lib.rs` re-derives the entire edge set from the source text of every file
under `src/` and fails on any reference that does not point downward, naming the
file, the line and both layer numbers.

```sh
# The enforced invariant, and the analysis it runs:
cargo test --lib the_module_graph_has_no_upward_edges
```

The check is deliberately two-tiered, because the shipped library and the test
configuration are different compilation units:

- **Shipped code** — every file with its `#[cfg(test)]` items removed — must be
  strictly one-way. This is the graph that is built, linked and published, and it
  is what lets each layer be read and reviewed without the layers above it.
- **`#[cfg(test)]` code** may hold exceptions, but only the three enumerated in
  `TEST_ONLY_CROSS_LAYER_EXCEPTIONS`, each with its rationale on record: `stream`
  drives the counting `alloc_func`/`free_func` hook that lives in
  `crate::ffi::alloc::test_hook`, because building a real C hook pair needs
  raw-pointer `unsafe` and that is permitted only under `src/ffi/**`; and the two
  engines decode each other's output, because an in-crate round trip is the only
  way to assert emitted bytes without `std` or a third-party codec. The list is an
  allow-list rather than a blanket exemption, so a *new* test-only inversion fails
  the test until it is justified — and a stale entry fails it too.

Two design decisions are what make the shipped graph one-way, and both mirror C
rather than working around it:

- [`src/stream.rs`](src/stream.rs) owns the engine state as an opaque
  `Box<dyn EngineState>` and names neither `DeflateState` nor `InflateState`. The
  typed views live *above* it, in `crate::deflate::state::DeflateStream` and
  `crate::inflate::state::InflateStream`, so the layer-5 handle keeps C's ownership
  semantics — the thing that makes `deflateEnd` unforgettable — while remaining
  exactly as incurious about the engines as C's opaque forward declaration
  `struct internal_state FAR *state`.
- The one-call façades are split the way the C `#include` graph splits them.
  `compress.c` and `uncompr.c` *include* `zlib.h` and *drive* the engines, so they
  sit above the engine rather than beside `zutil.h`: the bit-exact `compressBound`
  formula and the complete C driver loops stay in `src/util/` behind the
  `OneCallDeflate`/`OneCallInflate` port traits, while the engine-owning adapters
  and the public `compress`/`compress2`/`uncompress`/`uncompress2` entry points
  live in `src/deflate/mod.rs` and `src/inflate/mod.rs`. `zutil.h`'s `OS_CODE` and
  `PRESET_DICT` remain in `src/util/mod.rs`, and `src/deflate/mod.rs` reading them
  is now an ordinary downward edge.

`unsafe` containment does not rest on the graph shape in any case: it is enforced
by `#![deny(unsafe_code)]` plus exactly two scoped carve-outs, as described under
[How the `unsafe` boundary is enforced](#how-the-unsafe-boundary-is-enforced).

The CRC-32 lookup tables are not checked in: `build.rs`
regenerates them at build time in pure safe `std` Rust with no build-dependencies,
replacing 9,446 lines of pre-generated C tables with a verifiable algorithm whose
output CI asserts is byte-reproducible across two independent builds.

The **26 C translation units and headers (23,107 lines, 119 `ZEXTERN`
declarations)** remain in the repository root **unmodified, on purpose**. They are
the cross-validation oracle and the source of the official test vectors — the
`tests/c_oracle.rs` harness compiles them, and the tier-1 vectors in
`tests/interop.rs` were baked from them — so deleting them would destroy the only
mechanism by which byte-identity can be independently proven. They are not dead
code awaiting removal, and they are not shipped either: `Cargo.toml`'s `exclude`
list keeps every `*.c`, `*.h`, legacy build descriptor, and platform directory out
of the published crate, and CI's `package-verify` job diffs
`cargo package --list` against that contract, then builds and **runs the packaged
crate's own test suite**.

## Compatibility and RFCs

The data formats implemented by this crate are described by the following
Requests for Comments, whose text is also vendored under `doc/` for reference:

- **[RFC 1950](https://datatracker.ietf.org/doc/html/rfc1950)** — ZLIB Compressed
  Data Format (`doc/rfc1950.txt`).
- **[RFC 1951](https://datatracker.ietf.org/doc/html/rfc1951)** — DEFLATE
  Compressed Data Format (`doc/rfc1951.txt`).
- **[RFC 1952](https://datatracker.ietf.org/doc/html/rfc1952)** — GZIP File
  Format (`doc/rfc1952.txt`).

Byte-identical output against reference zlib is validated **by default** by
`tests/interop.rs`, which is deliberately built in two tiers that prove two
different things:

**Tier 1 — strict byte-identity. Always on, no C toolchain.** The crate's
compressed output must be byte-for-byte identical to the reference C zlib
`1.3.2.1-motley` for the same input, level, strategy, `windowBits`, and
`memLevel`. The oracle values were produced by the genuine C encoder via
`deflateInit2` + `deflate(Z_FINISH)` + `deflateEnd` and are baked into the source
as constants, which is what lets this gate run **by default in CI with no C
compiler anywhere in sight**. It carries **4,461 assertions** across six tables:

| Family | Rows | Corpora | Axes | Encoding |
|--------|-----:|---------|------|----------|
| `BI_VECTORS` + `BI_VECTORS_GZIP` | 225 + 75 | five short corpora (0 / 13 / 256 / 256 / 128 B) | levels `-1..=9` × 5 strategies × `windowBits` 15 / −15 / 9 / 31, `memLevel = 8` | full literal hex |
| `BI_GRID` + `BI_GRID_GZIP` | 3,300 + 825 | five 16 KiB shapes | levels `-1..=9` × 5 strategies × `windowBits` 15 / −15 / 9 / −9 / 31 × `memLevel` 1 / 8 / 9 | `(length, CRC-32)` digest |
| `BI_EXTREMES` + `BI_EXTREMES_GZIP` | 24 + 12 | the short corpora | the `memLevel` 1 / 9 corners the full-hex family never reaches | full literal hex |

The digest family is what makes a grid that wide affordable in source; the two
full-hex families keep an exact-bytes proof — 336 rows of it — in the suite.

**Tier 2 — decode-compatibility. Also always on.** Both directions of interop
against [`flate2`](https://crates.io/crates/flate2) on its default, pure-Rust
`miniz_oxide` backend, across every framing, level, and strategy. Because
`miniz_oxide` is a *different* encoder with different match-finding heuristics,
these tests prove RFC wire-format conformance but are **not** treated as
satisfying byte-identity: that property is proven exclusively by tier 1.

**The live C-oracle sweep — additive, never a replacement.** Tier 1 bakes a fixed
sample; `tests/c_oracle.rs` sweeps the whole configuration grid *live* against a
reference library compiled from this repository's own retained `*.c` sources
during the test run. Observed on this tree:

```text
c_oracle: oracle identity confirmed — zlibVersion()="1.3.2.1-motley" ZLIB_VERNUM=0x1321
          crc32("123456789")=0xcbf43926 adler32("123456789")=0x091e01de compressBound(9)=22
c_oracle: full grid 3750/3750 byte-identical against reference C zlib 1.3.2.1-motley
c_oracle: grid = 5 corpora x 5 windowBits x 3 memLevels x 10 levels x 5 strategies
          at 16384 bytes per corpus
c_oracle: smoke sweep 50/50 byte-identical (200000 bytes per corpus)
c_oracle: Z_DEFAULT_COMPRESSION resolved to level 6 in 5/5 configurations,
          matching reference C zlib byte for byte
```

The five corpus shapes are constant bytes, incompressible pseudo-random data,
natural-language text, a byte ramp, and a mixed run-plus-random buffer;
`deflateBound`-based destination sizing is part of the comparison, because C's
`deflate_stored` consults `avail_out` and so level-0 output is a function of the
destination size. The harness also self-checks that the grid is not degenerate,
asserting that its corpora really do discriminate the `memLevel` and `windowBits`
axes rather than filling the sweep with duplicate rows.

The zlib home page — with the canonical specifications and FAQ — is
<https://zlib.net/>.

## Memory safety and design

Every C idiom is mapped to a safe-Rust equivalent: integer-tagged state machines
become exhaustively-matched `enum`s, function-pointer dispatch tables become enum
strategy dispatch, opaque `internal_state *` pointers become owned `Box`ed state,
and manual `zcalloc`/`zcfree` allocation becomes ownership plus `Drop`. This
removes entire classes of defects — use-after-free, double-free, buffer overruns,
and unhandled state transitions — without changing a single output byte.

The design deliberately keeps the compression core free of `unsafe` and pushes
the unavoidable raw-pointer work (opaque handle round-tripping, slice
construction from C pointers, C-string handling) to the `src/ffi/` boundary,
where it is auditable and each block is annotated with a `// SAFETY:` comment.

### How the `unsafe` boundary is enforced

Containment here is structural, and it is checked four independent ways rather
than trusted:

1. **`#![deny(unsafe_code)]` crate-wide** in `src/lib.rs`, with exactly **two**
   narrowly scoped `#[allow(unsafe_code)]` carve-outs — `pub mod ffi` and the
   private `mod no_std_support`. A stray `unsafe` block, `unsafe fn`, `unsafe
   impl`, or `unsafe extern` in `src/deflate/**`, `src/inflate/**`,
   `src/checksum/**`, `src/gz/**`, `src/util/**`, `src/stream.rs`, `src/error.rs`,
   `src/constants.rs`, or `src/gz_header.rs` fails the build outright. `deny`
   rather than `forbid` is deliberate: `forbid` cannot be relaxed by an inner
   `allow`, which would make the two boundary carve-outs inexpressible.
2. **`#![warn(clippy::undocumented_unsafe_blocks)]`** alongside
   `#![warn(missing_docs)]`, both promoted to hard errors by the `-D warnings`
   lint gate, so every `unsafe` block in shipped code carries an adjacent
   `// SAFETY:` justification where a reader will meet it.
3. **In-crate boundary tests** that re-derive the boundary from the source text —
   blanking comments and literals, classifying each `unsafe` token, and treating a
   bare `unsafe extern "C" fn(..)` *type* as declarative — then assert that
   executable `unsafe` appears only under `src/ffi/**` and inside
   `mod no_std_support`, and that exactly two carve-outs exist. `deny(unsafe_code)`
   alone cannot catch a smuggled *third* carve-out, because such code still
   compiles; these tests can.
4. **A toolchain-independent shell assertion** over the same invariants in CI's
   `unsafe-boundary` job, so the gate still bites if the tests themselves are
   weakened or deleted.

`src/stream.rs` is worth calling out because a naive `grep` for `unsafe` finds two
hits there. Both are **type aliases only** — `ZallocFn` and `ZfreeFn`, which merely
*name* the C ABI hook signatures the crate must interoperate with;
`grep -c "unsafe {"` on that file returns **0**.

The same discipline covers the ABI itself: the `cfg(test)` fn-pointer coercion
guard described under
[Exported symbol reconciliation](#exported-symbol-reconciliation) binds all **96**
exported names, so a signature change is a compile error rather than a surprise at
a C call site.

### Portability: what CI actually exercises

Platform claims deserve platform coverage, so here is the boundary, drawn honestly.
`.github/workflows/ci.yml` runs **fourteen jobs**:

- **Natively executed, full test suite:** `ubuntu-latest` across five feature rows
  (default, `--all-features`, `std,gzip,gz-io`, `std,simd`, and `--no-default-features`
  build-only), plus **`windows-latest` (x86_64)** and **`macos-latest` (aarch64)**.
  Every row asserts its own host triple via `rustc -vV` and its own `runner.arch`,
  so a runner label that silently changes architecture fails the job instead of
  quietly weakening the claim. The Windows row is what exercises `OS_CODE = 10`
  and the `#[cfg(windows)]`-gated `gzopen_w`: it not only compiles that symbol but
  **executes** `ffi::gz::tests::wide_path_open_round_trip`, which opens a
  UTF-16 path through `gzopen_w`, writes, closes, reopens, reads back, and asserts
  both the recovered bytes and the `gzerror` state — and a dedicated Windows-only
  step runs that test **by name** and asserts exactly one test passed, so the
  coverage cannot regress into a mere compile check. The macOS row exercises
  `OS_CODE = 19` and aarch64.
- **Cross type-checked and cross-linted:** four triples in the `cross-targets`
  job — `aarch64-unknown-linux-gnu`, `i686-unknown-linux-gnu` (32-bit),
  `s390x-unknown-linux-gnu` (**big-endian**), and `x86_64-pc-windows-msvc`. Each
  gets both `cargo check` and `cargo clippy -D warnings` with `--all-targets
  --all-features`; neither command links, which is why no cross linker, emulator
  or MSVC toolchain is needed here. The Windows-MSVC row is a *cross* lane and is
  not the same claim as the native `windows-latest` row above: that one is native
  and narrow (one OS, one feature selection, and it **runs** the suite), this one
  is cross and wide (every feature on, nothing executed). Both are kept because a
  regression has to evade both — `c_ulong` is 32-bit on MSVC and 64-bit on LP64,
  so a `uLong as u32` cast is a real truncation on Linux and an identity cast
  there, and `unnecessary_cast` fires on exactly one of the two. The first three
  triples are additionally **executed** by `cross-run`, described below; this job
  is what covers them at `--all-features`, which `cross-run` cannot execute
  because the live C-oracle tests want a host-native C compiler. The
  `build-script-tests` job additionally type-checks the
  library and test profile for s390x specifically so the big-endian CRC braid arms
  are compiled — on a little-endian runner they otherwise never are, and a wrong
  index in one of them would go unnoticed until somebody built for big-endian
  hardware. A unit test asserts the endian-selected table anchors match the active
  target's values on **every** target, so the relationship is checked at run time
  wherever the suite runs at all.
- **Cross EXECUTED under emulation:** the `cross-run` job runs the test suite on
  `aarch64-unknown-linux-gnu`, on 32-bit `i686-unknown-linux-gnu` and on
  **big-endian** `s390x-unknown-linux-gnu` through `qemu-user`, across the default
  row and both std-off rows (`--no-default-features` and
  `--no-default-features --features no-std`). Each matrix row declares **both** its
  endianness and its pointer width and asserts them against `rustc --print cfg`, so
  a triple swapped in without those fields being updated fails the job rather than
  reporting a little-endian run as big-endian coverage or a 64-bit run as 32-bit
  coverage; a dedicated step then runs the four endian-critical CRC tests **by
  name** and asserts exactly four passed, so this coverage cannot decay into a
  mere compile check. The job resolves its emulator binary by trying
  `qemu-<arch>-static` and then `qemu-<arch>`, because the statically linked
  emulators are packaged differently across Ubuntu releases — on 25.10
  `qemu-user-static` is a *pure virtual* package and the binaries ship inside
  `qemu-user` without the suffix, so a job that hard-codes one spelling breaks on
  a runner-image bump. `--all-features` is deliberately **excluded** from these
  rows: `tests/c_oracle.rs` classifies a toolchain probe by the `ErrorKind` of a
  failed `Command` spawn, and under user-mode emulation "command absent" and
  "command present but broken" both surface as a child exiting 127, so four
  self-tests of that classifier cannot distinguish the two. Weakening a
  deliberately loud classifier to suit an emulator would be the wrong trade.
- **Bare metal, EXECUTED under emulation:** `bare-metal-no-std` builds the
  `thumbv7em-none-eabihf` library in both `--no-default-features` and
  `--features no-std` configurations and asserts from the archive's symbol table
  that the freestanding runtime block was genuinely compiled and is crate-owned.
  `bare-metal-run` then links that same staticlib into a firmware image and
  **runs it on a no-OS Cortex-M4 under `qemu-system-arm`**, which is what turns
  the libc-backed `GlobalAlloc`, the 32-bit `MIN_ALIGN = 8` arm and the
  `#[panic_handler]` from linked code into executed code. No hosted test can reach
  that block — `cargo test` sets `test` and forces `panic = "unwind"`, failing two
  of the three terms in its own `cfg`. The row that matters most drains the heap
  and asserts `deflateInit2` returns `Z_MEM_ERROR` rather than aborting: on a
  device with no OOM killer and no swap, that is the difference between a
  recoverable error and a dead board. Both feature rows run, and they assert
  **opposite** things about `windowBits = 31` — accepted with `gzip` on, rejected
  with `Z_STREAM_ERROR` when it is off, matching a C zlib compiled without
  `GZIP` (`deflate.c` lines 428-433) — so the feature gate itself is covered
  rather than merely present.
- **Also gated:** the C-ABI linkage contract on four feature rows, the `unsafe`
  boundary, MSRV 1.85.0, benchmark compilation, `cargo package` verification, and a
  blocking `docs` job that runs rustdoc with `RUSTDOCFLAGS: -D warnings` and builds
  the published MkDocs site with `--strict` in a version-pinned Python environment.

So: **big-endian, 32-bit and bare-metal are all executed** — under emulation
rather than on IBM Z, i686 or Cortex-M silicon, which is a real distinction and is
stated as such. Emulation reproduces the instruction set, the endianness, the
pointer width and the absence of an operating system; it does not reproduce a
device, its timing or its memory controller, so real-hardware validation remains
genuinely open and this README does not claim otherwise. The two mechanisms that
make the endianness and platform distinctions
matter are concrete, and both are resolved **at compile time** from the target
triple rather than probed at run time — `src/checksum/crc32.rs` chooses its braid
tables with `cfg!(target_endian)` (both arms are compiled and type-checked; only
the selected one reaches codegen), and `src/util/mod.rs` selects the gzip header's
`OS_CODE` per platform (10 on Windows, 19 on non-Windows Apple, 3 otherwise).
Reference C zlib probes endianness at execution time instead; that divergence is
documented and cannot change a checksum.

Two of those three `OS_CODE` arms are the ones a compile-time cascade most easily
gets wrong, because nothing on the build host exercises them. Both have now been
observed in an emitted gzip member rather than inferred from the source: the
Windows row writes `os_byte = 0x0a` (10), and the bare-metal row — a target that
is neither Windows nor Apple, and has no operating system at all — writes
`os_byte = 0x03`, taking the `#ifndef OS_CODE` fallback. Reference C selects 10
under Windows too, because mingw-w64 defines `WIN32`/`_WIN32` and `zutil.h`
branches on them, so the values agree rather than merely both being defensible.
The Apple arm (19) is reached by CI's native `macos-latest` row.

The big-endian row is the one worth dwelling on, because it was for a long time the
project's largest unexercised claim. `crc32fast` has no accelerated backend on
s390x, so even the SIMD-enabled default row selects the scalar braid there: the
`CRC_BIG_TABLE` and `CRC_BRAID_BIG_TABLE` arms are not merely compiled on that
row, they are the arms that serve every bulk call. Running that row is therefore
what turns "the big-endian tables are byte-swapped companions of the little-endian
ones" from a type-checked assertion into an executed one.

## Roadmap

Parity work is complete for the core codecs and validated by default. The gates
that were previously tracked as open items are now closed:

- **Byte-identity (done).** Validated by default in `tests/interop.rs` by
  **4,461 baked assertions** derived from the genuine C zlib 1.3.2.1-motley
  encoder — no opt-in feature and no C toolchain required. The same property is
  reproducible live and in-repository through the opt-in `tests/c_oracle.rs`
  harness, which returned **3,750/3,750** and **50/50 byte-identical** against a
  reference library compiled from the retained in-tree C sources. See
  [Compatibility and RFCs](#compatibility-and-rfcs) for the full grid.
- **`no_std` test coverage (done).** The full test suite compiles and passes
  under `cargo test --locked --no-default-features` (and `--features no-std`) —
  **737 tests, 0 failed, 0 ignored** in both rows — and CI runs both as blocking
  gates, plus a bare-metal `thumbv7em-none-eabihf` build job.
- **Cross-platform CI (done).** Native Windows and macOS rows run the real suite;
  four further triples — aarch64, 32-bit x86, big-endian s390x, and
  Windows-MSVC — are cross type-checked and cross-linted without being executed.
  The residual limits are stated precisely under
  [Portability](#portability-what-ci-actually-exercises).
- **Fuzzing (done).** A detached `cargo-fuzz` crate under `fuzz/` ships targets
  for inflate, deflate round-trip, gzip parsing, checksums, and the FFI
  boundary; `.github/workflows/fuzz.yml` builds every target and runs each for a
  bounded budget on a weekly schedule. That workflow builds and fuzzes only; the
  fuzz graph's `cargo-deny` policy is enforced by `audit.yml`, on every push and
  pull request.
  Because the workspace is detached, the root `cargo fmt --all` and
  `cargo clippy --all-targets` gates cannot reach it — so that workflow also runs
  a nightly `fmt --check` and Clippy with `-D warnings` against
  `fuzz/Cargo.toml`, at default features and again at `--no-default-features`,
  plus a `--no-default-features` build asserting all five harness binaries still
  link. The 11 committed seeds in `fuzz/seeds/fuzz_inflate/` are passed to
  libFuzzer as read-only corpus inputs after the writable cached corpus, so the
  deterministic seed sweep runs even on a cold cache.

Remaining, workload-dependent work:

- **Performance.** Read the framing first: **performance is a constraint on this
  migration, not its objective.** This is explicitly not a performance refactor,
  and no optimisation may be introduced at the cost of byte-identity.

  The position against C zlib 1.3.2.1-motley is **measured**, per profile and per
  level, by the differential harness described under
  [Measured evidence](#measured-evidence) — median of five interleaved A/B rep
  pairs at ≥ 0.30 s each, 64 KiB payload, reference C built from the retained
  in-tree `*.c` baseline with `gcc -O2 -D_LARGEFILE64_SOURCE=1 -DHAVE_UNISTD_H`.
  Every cell is RS as a percentage of C; higher is faster than C.

  **Compression** — one-call `compress2`:

  | profile | level 1 | level 6 | level 9 |
  |---------|---------|---------|---------|
  | `text` | 113% | 161% | 157% |
  | `repetitive` | 146% | 131% | 138% |
  | `incompressible` | 90% | 82% | 92% |

  **Decompression** — one-call `uncompress`, and separately the `inflateBack`
  decoder, each cell being the range across source levels 1, 6 and 9:

  | profile | `uncompress` | `inflateBack` |
  |---------|--------------|---------------|
  | `text` | 141–154% | 216–312% |
  | `repetitive` | 154–160% | 248–344% |
  | `incompressible` | 101–114% | 86–102% |

  Three things in those tables replace claims earlier revisions of this README made,
  and each earlier claim is named here rather than quietly dropped:

  - **The compressible profiles are now above C, not below it.** An earlier revision
    quoted an aggregate "compression ≈ 85%" and localised the shortfall to the
    compressible profiles at "roughly 58–64%", with incompressible input the
    *closest* to C at "roughly 82–86%". Both halves of that are retired: nothing
    measures in the 58–64% band, and the localisation is the other way round.
  - **The one profile still below C is `incompressible`, at 82–94%** across levels.
    Incompressible input pays for one *failed* match-finder search per literal and
    has no long match to amortise it over — `longest_match`'s two-byte prefilter
    rejects nearly every candidate before the comparison loop, and `_tr_flush_block`
    then selects stored blocks because a dynamic tree cannot pay for itself — so
    throughput there is set almost entirely by per-literal bookkeeping. It is
    therefore both the slowest workload in absolute terms (≈ 39 MiB/s against
    ≈ 309 MiB/s for `text` and ≈ 316 MiB/s for `repetitive` at the same level,
    a factor of roughly eight) and the only C-relative deficit that remains.
  - **Decompression is at or above parity on every `uncompress` profile**, which was
    the load-bearing half of the old claim and is now true without qualification.
    Earlier revisions quoted a "104–125% per profile" range that did not include
    `inflateBack` at all; measured on its own, `inflateBack` runs 216–344% of C on
    input that actually decodes Huffman symbols. Its one sub-parity cell —
    `incompressible` at 86–102% — is not a decode-logic figure: an incompressible
    payload is emitted as *stored* blocks, so that case is a `memcpy` running at
    roughly 19 GiB/s on both sides and the spread is host and `memcpy` variance.

  Two narrower measured positions are worth recording because they were previously
  unmeasured and each turned out to hide a real regression:

  - **Short-distance LZ77 copies.** `inflate` on streams whose matches are dominated
    by one distance runs at 132–433% of C across distances 1, 3, 4, 8, 16, 32 and
    128 — the worst of those being 132% at distance 8. No Criterion case reached
    this path before the `repetitive` profile was added to
    `benches/inflate_bench.rs`.
  - **Caller-supplied `zalloc`/`zfree` hooks.** A stream driven through caller hooks
    now runs at 98–102% of the same stream on the global allocator (reference C
    measures 93–96% on the same comparison), and at 159–162% of C's own hook path.
    Allocation counts are identical to C in all thirteen measured lifecycle
    families, and `zalloc`/`zfree` remain balanced with zero live bytes at teardown.
  - **Non-default `memLevel`.** At level 6 the tuning sweep runs 108–158% of C
    across `memLevel` 1, 8 and 9. The residual deficit is at **level 1 with a
    non-default `memLevel`** — 70–74% of C on a 64 KiB payload — and it is a
    per-stream *initialisation* cost, not a compression cost: this crate's owned
    buffers are zero-filled when they are created (a safe `Vec` always is, and
    `alloc_zeroed` would require an `unsafe` carve-out outside the FFI boundary,
    which `#![deny(unsafe_code)]` forbids) whereas C's `ZALLOC` is a plain `malloc`.
    It amortises away with payload size: the same case reaches 87% at 1 MiB, and the
    `memLevel = 9` variant reaches 149%.

  No CRC-32 multiplier is quoted here. The difference between the `crc32fast` hot
  path and the scalar braid is CPU- and build-dependent, and — as
  `benches/checksum_bench.rs` documents — the selected backend depends not only on
  the `simd` feature but on `crc32fast`'s own `std` feature, which dev-dependencies
  can silently unify on. A re-runnable pair of commands is better evidence than a
  constant, and the bench prints the backend it actually measured:

  ```sh
  # scalar braid (no crc32fast in the graph)
  RUSTUP_TOOLCHAIN=stable cargo bench --locked --bench checksum_bench \
      --no-default-features --features std,gzip,gz-io
  # simd-enabled build (inspect the printed crc32_backend value)
  RUSTUP_TOOLCHAIN=stable cargo bench --locked --bench checksum_bench \
      --no-default-features --features std,gzip,gz-io,simd
  ```

  **The hard rule on optimisation.** Any candidate compression speed-up must clear
  the byte-identity gate before it is considered viable, because the very
  heuristics that cost throughput are the ones that determine the output bytes: the
  chain-length **quartering** at `good_match` (`chain_length >>= 2`, not a halving),
  the `nice_match` early break, and the
  `TOO_FAR` lazy-match filter. *A faster match finder that emits different tokens
  is a regression, not an improvement, no matter what the benchmark says.*
  Permissible optimisation is limited to work that provably cannot change the token
  stream — bounds-check elision, memory-access patterns, inlining, and buffer-copy
  strategy.

  The three Criterion harnesses are `benches/deflate_bench.rs` (621 lines),
  `benches/inflate_bench.rs` (460), and `benches/checksum_bench.rs` (186) — the
  file names, since the plan's own inventory once listed them as
  `compress.rs`/`decompress.rs`/`checksum.rs`, which never existed. They cover, and
  the coverage list is deliberately explicit because every gap in it once hid a
  regression:

  - `deflate_levels/<0..9>` — all ten levels on text.
  - `deflate_profiles/level6/{text,repetitive,incompressible}` — the three shapes.
  - `deflate_incompressible_guard/{1,6,9}` — the **incompressible-input guard**, so
    that the slowest *absolute* Rust workload is measured rather than assumed. Per
    the tables above that profile is now also the only C-relative deficit, so the
    guard brackets both readings at once. Note that this harness links no C library
    and therefore prints no ratio of its own.
  - `deflate_tuning/mem_level/{1,8,9}`, `deflate_tuning/window_bits/{-15,9,15,31}`,
    and `deflate_tuning/strategy/{default,filtered,huffman_only,rle,fixed}` — added
    because `compress2` hard-codes `windowBits = 15`, `memLevel = 8` and the default
    strategy, so *every* pre-existing case measured exactly one point of that space.
    The `memLevel` dimension was holding a real regression at the time it was added.
  - `inflate_by_level/{1,6,9}` and
    `inflate_by_profile/level6/{text,repetitive,incompressible}` — the `repetitive`
    profile being what drives the short-distance overlapping-copy path.
  - `inflate_back_by_profile/level6/{text,repetitive,incompressible}` — the
    `infback.c` decoder, which no case reached before and which was consequently
    free to regress unobserved.
  - `adler32/<size>` and `crc32/<size>`.

  Every case validates its own output before it is timed — one untimed
  round-trip asserting exact length and byte-for-byte equality, which Criterion
  never folds into a sample — and the noise thresholds are set above the
  run-to-run spread actually observed on the measuring host rather than at
  Criterion's unusable 1% default. Even so, 8 of the Criterion ids on this host are
  noise-dominated (worst observed: `crc32/16384` at a 138.6% coefficient of
  variation, and `deflate_levels/6` at R² = 0.32), which is the same shared-host
  caveat recorded under [Measured evidence](#measured-evidence) and another reason
  the C-relative claims are taken from interleaved medians rather than from
  Criterion. That folder measures; it does not authorise.

## Contributing

Issues and pull requests are welcome. [`CONTRIBUTING.md`](CONTRIBUTING.md) has the
full workflow — the quality gates, the MSRV policy, and what a reviewable change
looks like here. In short: every gate listed under
[The blocking quality gates](#the-blocking-quality-gates) must pass, including
`RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check` and
`RUSTUP_TOOLCHAIN=stable cargo clippy --locked --all-targets --all-features -- -D warnings`
— run them in exactly that form, for the reasons given there.

Two rules are worth stating up front because they are not negotiable:

- **New `unsafe` outside the FFI boundary is not accepted.** It will not even
  compile — `#![deny(unsafe_code)]` is crate-wide, and the correct response to
  needing `unsafe` in a core module is to restructure, as `src/deflate/strategy.rs`
  did when it replaced C's `compress_func` pointer table with a tag enum resolved
  by an exhaustive `match`.
- **Any `unsafe` that is genuinely required must carry a `// SAFETY:`
  justification**, immediately adjacent to the block, and the lint gate enforces it.

To report a vulnerability, follow [`SECURITY.md`](SECURITY.md) rather than opening a
public issue. Release history for the Rust crate lives in
[`CHANGELOG.md`](CHANGELOG.md); the separate upstream `ChangeLog` file is the **C
baseline's** history and is retained unchanged alongside the C sources.

## License

`zlib-rs` is distributed under the **Zlib** license — the same permissive license
as upstream zlib. See the [`LICENSE`](LICENSE) file for the full text.

```text
(C) 1995-2026 Jean-loup Gailly and Mark Adler

This software is provided 'as-is', without any express or implied warranty. In
no event will the authors be held liable for any damages arising from the use of
this software.

Permission is granted to anyone to use this software for any purpose, including
commercial applications, and to alter it and redistribute it freely, subject to
the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim
   that you wrote the original software. If you use this software in a product,
   an acknowledgment in the product documentation would be appreciated but is
   not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.
```

## Acknowledgments

The DEFLATE format used by zlib was defined by **Phil Katz**. The DEFLATE and
zlib specifications were written by **L. Peter Deutsch**. The original zlib
library was written by **Jean-loup Gailly** (compression) and **Mark Adler**
(decompression); this crate is a Rust reimplementation of their work and would
not exist without it. Thanks also to the many contributors who reported problems
and suggested improvements to zlib over the decades.
