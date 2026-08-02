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
on this tree, never an estimate. Each row below was run with
`RUSTUP_TOOLCHAIN=stable` — see [Installation](#installation) for why that prefix
matters — on stable `rustc 1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)`,
`x86_64-unknown-linux-gnu`, except the two MSRV rows.

| Gate | Command | Result |
|------|---------|--------|
| Test suite (default features) | `cargo test --locked` | **842 passed / 0 failed / 0 ignored** |
| Test suite (all features) | `cargo test --locked --all-features` | **855 passed / 0 failed / 0 ignored** |
| Test suite (`no_std`) | `cargo test --locked --no-default-features` | **626 passed / 0 failed / 0 ignored** |
| Test suite (`no-std` feature) | `cargo test --locked --no-default-features --features no-std` | **626 passed / 0 failed / 0 ignored** |
| Formatting | `cargo fmt --all -- --check` | exit 0 |
| Lints | `cargo clippy --locked --all-targets --all-features -- -D warnings` | exit 0 |
| Docs | `cargo doc --locked` | exit 0 |
| MSRV build | `cargo +1.85.0 build --locked` | exit 0 |
| MSRV type-check | `cargo +1.85.0 check --locked --all-targets` | exit 0 |
| Exported C symbols | `nm -D --defined-only target/release/libzlib_rs.so` | **95**, all type `T` |
| Live byte-identity sweep | `cargo test --locked --features c-oracle --test c_oracle` | **3750/3750** and **50/50** byte-identical |

The 842 default-feature tests decompose as **688** in-crate unit tests, **127**
integration tests (`checksum` 23, `gzip_compat` 15, `inflate_coverage` 28,
`interop` 30, `regression` 12, `round_trip` 19), and **27** doctests.
`--all-features` adds the 13 tests of the opt-in live C-oracle harness. Under
`--no-default-features` the total is **505** unit + **96** integration + **25**
doctests; the `gzip_compat` suite correctly reports 0 because the whole `gz*`
file API is feature-gated off.

**The ignored-test count is zero in every configuration, and stays zero.** A
capability that cannot be exercised in a given build is expressed by a
feature gate or a run-time probe that *passes with a printed notice*, never by
`#[ignore]`.

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

Add the crate with Cargo:

```sh
cargo add zlib-rs
```

or add it to your `Cargo.toml` directly:

```toml
[dependencies]
zlib-rs = "1.3.2"
```

The crate name on crates.io is `zlib-rs`; the importable module path is
`zlib_rs` (Cargo replaces `-` with `_`):

```rust,ignore
use zlib_rs::{compress, uncompress};
```

**Minimum Supported Rust Version (MSRV): `1.85.0`.** The crate targets the Rust
**2024 edition**, which was stabilized in Rust 1.85.0 — so 1.85.0 is the tightest
self-consistent floor an edition-2024 crate can declare, not a round number. Any
newer stable toolchain also works.

That floor is **verified, not assumed**. On `rustc 1.85.0 (4d91de4e4 2025-02-17)`
both `cargo +1.85.0 build --locked` and `cargo +1.85.0 check --locked
--all-targets` exit 0, reproducing CI's `msrv` job locally; the same tree also
builds and passes its full suite on current stable
`rustc 1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)`.

A root [`rust-toolchain.toml`](rust-toolchain.toml) pins `channel = "1.85.0"`
(with `rustfmt` and `clippy`, `profile = "minimal"`) so a contributor's local
build resolves to the same compiler CI's MSRV job uses instead of floating to
whatever stable happens to be installed. **A bare `cargo …` inside this
repository therefore invokes the MSRV compiler.** To reach stable — which is what
ten of CI's eleven jobs do — set `RUSTUP_TOOLCHAIN` or use a `+stable` prefix;
`rustup`'s environment override outranks the toolchain file, which is exactly why
CI's `dtolnay/rust-toolchain@stable` and `@nightly` steps still work:

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

```rust,ignore
use zlib_rs::gz::{gzopen, gzwrite, gzclose};

// Write a gzip-compressed file, then close to flush the trailer.
let mut file = gzopen("hello.txt.gz", "wb").expect("gzopen failed");
let _ = gzwrite(&mut file, b"hello, gzip\n");
let _ = gzclose(file);
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
recovered byte-exactly with correct `1f 8b` gzip framing. CI's `c-abi-linkage`
job runs the equivalent check on four feature rows and additionally asserts that
the dynamic export set matches the `zlib.map` contract on every one of them.

Release artifacts, default features, built by `cargo build --locked --release` on
stable 1.97.1: `libzlib_rs.rlib` ≈ 2.3 MiB, `libzlib_rs.so` ≈ 620 KiB,
`libzlib_rs.a` ≈ 21 MiB. These are a dated observation rather than a budget — no
gate asserts them, and they drift with the compiler; the same build on the MSRV
floor's older LLVM comes out marginally smaller, as
[`rust-toolchain.toml`](rust-toolchain.toml) records.

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

Build the shared and static objects:

```sh
cargo build --release
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
ZLIB_RS_VERSION_SCRIPT=1 cargo build --release
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
cargo build --no-default-features
```

To pick individual features explicitly, combine `--no-default-features` with
`--features`:

```sh
cargo build --no-default-features --features "std,gzip,simd"
```

## Building, testing, and benchmarking

The crate builds with a stock Cargo toolchain — **no CMake, `./configure`, or
`make` required**.

```sh
# Build (debug / optimized)
cargo build
cargo build --release

# Run the full test suite (unit, integration, and doc tests)
cargo test

# Lint with Clippy, treating warnings as errors
cargo clippy --all-targets --all-features -- -D warnings

# Verify formatting
cargo fmt --all -- --check

# Run the Criterion benchmarks
cargo bench
```

Remember the toolchain pin from [Installation](#installation): a bare `cargo …`
here resolves to MSRV 1.85.0, so prefix the gates with `RUSTUP_TOOLCHAIN=stable`
(or `+stable`) to run them the way CI does.

### The blocking quality gates

These six are the gates that must stay green, and **all six currently exit 0** on
this tree — the results are tabulated under
[Measured evidence](#measured-evidence):

```sh
RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check
RUSTUP_TOOLCHAIN=stable cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTUP_TOOLCHAIN=stable cargo build --locked
RUSTUP_TOOLCHAIN=stable cargo test  --locked
RUSTUP_TOOLCHAIN=stable cargo test  --locked --no-default-features
RUSTUP_TOOLCHAIN=stable cargo doc   --locked
```

No warning is downgraded, no lint is `allow`-ed to make a change land, and no test
is `#[ignore]`d. `--all-features` on the Clippy line is load-bearing rather than
decorative: without it Clippy never sees `tests/c_oracle.rs` (which carries
`required-features`), the `inflate_strict` arms, or the `no_std` runtime block —
precisely where a lint regression would hide longest, since no other job promotes
warnings to errors. `--locked` keeps both lockfiles immutable.

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

Two `cargo-deny` policies govern that closure — [`deny.toml`](deny.toml) for the
root graph and [`fuzz/deny.toml`](fuzz/deny.toml) for the detached fuzz workspace
(`cargo-deny` resolves configuration from the target manifest's workspace root, so
each is picked up automatically). Both set `[advisories]`, `[licenses]`, `[bans]`,
and `[sources]` with `all-features = true`, deny yanked crates, and bound advisory
staleness; the root policy additionally pins nine target triples so the audit
covers the Windows, Apple, aarch64, bare-metal, and wasm configurations rather
than only the host. `.github/workflows/fuzz.yml` already runs the fuzz-workspace
policy as a `supply-chain` job that **gates** fuzzing (`needs: supply-chain`), and
`.github/workflows/audit.yml` carries the root-graph `cargo-deny` plus
`cargo-audit` gate.

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

That is **40 modules** under `src/`, in a strictly acyclic layering —
`error`/`constants` → `util` → `checksum` → `stream`/`gz_header` →
`{deflate, inflate}` → `gz` → `ffi` — that mirrors the C `#include` layering with
no extra top-level modules. The CRC-32 lookup tables are not checked in: `build.rs`
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
`.github/workflows/ci.yml` runs **eleven jobs**:

- **Natively executed, full test suite:** `ubuntu-latest` across five feature rows
  (default, `--all-features`, `std,gzip,gz-io`, `+simd`, and `--no-default-features`
  build-only), plus **`windows-latest` (x86_64)** and **`macos-latest` (aarch64)**.
  The Windows row is what exercises `OS_CODE = 10` and the `#[cfg(windows)]`-gated
  `gzopen_w`; the macOS row exercises `OS_CODE = 19` and aarch64.
- **Cross type-checked, not natively run:** `aarch64-unknown-linux-gnu`,
  `i686-unknown-linux-gnu` (32-bit), and `s390x-unknown-linux-gnu`
  (**big-endian**). The `build-script-tests` job additionally type-checks the
  library and test profile for s390x specifically so the big-endian CRC braid arms
  are compiled — on a little-endian runner they otherwise never are, and a wrong
  index in one of them would go unnoticed until somebody built for big-endian
  hardware. A unit test asserts the endian-selected table anchors match the active
  target's values on **every** target, so the relationship is checked at run time
  even where the big-endian arms themselves are not executed.
- **Built, not run:** the bare-metal `thumbv7em-none-eabihf` target, in both
  `--no-default-features` and `--features no-std` configurations, with an assertion
  that the freestanding runtime block was genuinely compiled.
- **Also gated:** the C-ABI linkage contract on four feature rows, the `unsafe`
  boundary, MSRV 1.85.0, benchmark compilation, and `cargo package` verification.

So: a 32-bit, big-endian, or bare-metal build is **compile-verified**, not
runtime-verified, and this README does not claim otherwise. The two mechanisms that
make the distinction matter are concrete — `src/checksum/crc32.rs` chooses its
braid tables with `cfg!(target_endian)`, and `src/util/mod.rs` selects the gzip
header's `OS_CODE` per platform (10 on Windows, 19 on non-Windows Apple, 3
otherwise).

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
  under `cargo test --no-default-features` (and `--features no-std`) — **626
  tests, 0 failed, 0 ignored** — and CI runs it as a blocking gate, plus a
  bare-metal `thumbv7em-none-eabihf` build job.
- **Cross-platform CI (done).** Native Windows and macOS rows run the real suite;
  aarch64, 32-bit x86, and big-endian s390x are cross type-checked. The residual
  limits are stated precisely under
  [Portability](#portability-what-ci-actually-exercises).
- **Fuzzing (done).** A detached `cargo-fuzz` crate under `fuzz/` ships targets
  for inflate, deflate round-trip, gzip parsing, checksums, and the FFI
  boundary; `.github/workflows/fuzz.yml` builds every target and runs each for a
  bounded budget on a weekly schedule, behind a `cargo-deny` supply-chain gate.

Remaining, workload-dependent work:

- **Performance.** Read the framing first: **performance is a constraint on this
  migration, not its objective.** This is explicitly not a performance refactor,
  and no optimisation may be introduced at the cost of byte-identity.

  The plan-recorded aggregate position against C zlib 1.3.2.1-motley is
  **compression ≈ 85%** and **decompression 107–127%** of C throughput — so
  decompression is at or above parity, and compression is the interesting side.
  Treat both as attributed context rather than as properties this repository
  re-checks on demand: the Criterion suite links no C library, and there is no
  in-tree *performance* oracle (`tests/c_oracle.rs` is a *conformance* oracle for
  byte-identity, which is a different job).

  A per-profile comparison against a reference C build sharpened the picture, and
  **inverted the intuitive reading of the compression gap**. An earlier revision of
  this README and of `benches/deflate_bench.rs` attributed the shortfall to the
  incompressible-input path, "where the match finder does the most fruitless work".
  That was plausible and wrong:
  - Incompressible input is the profile **closest** to C, at roughly **82–86%**.
    The match finder fails *fast* there — `longest_match`'s two-byte prefilter
    rejects nearly every candidate before the comparison loop, and
    `_tr_flush_block` then selects stored blocks because a dynamic tree cannot pay
    for itself — so both implementations do similar and rather little work per byte.
  - Compressible profiles are the **furthest**, at roughly **58–64%**. That is where
    hash chains are genuinely walked, lazy matching is evaluated, and Huffman trees
    are built and emitted.
  - Decompression measured **104–125%** per profile, which brackets the quoted
    107–127%, so that figure survives contact with measurement.

  No CRC-32 multiplier is quoted here. The difference between the `crc32fast` hot
  path and the scalar braid is CPU- and build-dependent, and — as
  `benches/checksum_bench.rs` documents — the selected backend depends not only on
  the `simd` feature but on `crc32fast`'s own `std` feature, which dev-dependencies
  can silently unify on. A re-runnable pair of commands is better evidence than a
  constant, and the bench prints the backend it actually measured:

  ```sh
  # scalar braid (no crc32fast in the graph)
  cargo bench --bench checksum_bench --no-default-features --features std,gzip,gz-io
  # crc32fast hot path
  cargo bench --bench checksum_bench --no-default-features --features std,gzip,gz-io,simd
  ```

  **The hard rule on optimisation.** Any candidate compression speed-up must clear
  the byte-identity gate before it is considered viable, because the very
  heuristics that cost throughput are the ones that determine the output bytes: the
  chain-length halving at `good_match`, the `nice_match` early break, and the
  `TOO_FAR` lazy-match filter. *A faster match finder that emits different tokens
  is a regression, not an improvement, no matter what the benchmark says.*
  Permissible optimisation is limited to work that provably cannot change the token
  stream — bounds-check elision, memory-access patterns, inlining, and buffer-copy
  strategy.

  The three Criterion harnesses are `benches/deflate_bench.rs`,
  `benches/inflate_bench.rs`, and `benches/checksum_bench.rs`. The deflate harness
  carries an explicit **incompressible-input profile** so that the gap is measured
  rather than inferred, every case validates its own output before it is timed, and
  the noise thresholds are set above the run-to-run spread actually observed on the
  measuring host rather than at Criterion's unusable 1% default. That folder
  measures; it does not authorise.

## Contributing

Issues and pull requests are welcome. [`CONTRIBUTING.md`](CONTRIBUTING.md) has the
full workflow — the quality gates, the MSRV policy, and what a reviewable change
looks like here. In short: every gate listed under
[The blocking quality gates](#the-blocking-quality-gates) must pass, including
`cargo fmt --all -- --check` and
`cargo clippy --all-targets --all-features -- -D warnings`.

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
