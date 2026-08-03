# blitzy-zlib

`zlib-rs` is a memory-safe, idiomatic Rust rewrite of the zlib 1.3.2.1 compression library with a C-compatible
FFI drop-in layer. It reimplements DEFLATE, zlib, and gzip (RFC 1951 / RFC 1950 / RFC 1952) so that its
compressed output is **byte-identical** to reference C zlib — the same bytes, not merely a stream that zlib
happens to decode.

The rewrite lives in the **same repository** as the C baseline it replaces. Those C sources are deliberately
retained in-tree as the cross-validation oracle and as the source of the official test vectors, and are kept out
of the published crate through the `exclude` list in `Cargo.toml`.

## At a glance

| | |
| --- | --- |
| **Baseline** | zlib `1.3.2.1-motley`, `ZLIB_VERNUM 0x1321`. `zlibVersion()` reports that full four-component string; the Cargo package version is `1.3.2`, because SemVer admits no fourth component. |
| **Memory safety** | `unsafe` is confined to the `ffi` boundary. All eight core module groups — `deflate`, `inflate`, `checksum`, `gz`, `util`, `error`, `constants`, `gz_header` — measure **zero** executable `unsafe`. |
| **Structure** | **40** modules in a **seven-layer** tree mirroring the C `#include` layering: `error` / `constants` → `util` → `checksum` → `stream` / `gz_header` → `{deflate, inflate}` → `gz` → `ffi`. `stream` / `gz_header` and `deflate` / `inflate` are strict peers. The graph is **acyclic**: every `use crate::…` in the shipped library points at a strictly lower layer — no upward edges, no same-layer edges — and the `the_module_graph_has_no_upward_edges` test re-derives the whole edge set from the source text and fails on any reference that does not, so the layering is enforced rather than asserted ([details](technical-specifications.md#031-refactored-structure-planning)). It replaces **26** C translation units and headers, **23,107** lines of C exposing **119** `ZEXTERN` entry points. |
| **Byte-identity** | Proven at **50/50** and **3,750/3,750** configurations against a reference C zlib built from this repository's own C sources. |
| **C ABI** | **95** exported symbols; **54/54** of `zlib.map`'s `global:` symbols present and **0/10** of its `local:` symbols leaked. |
| **Formats** | All **10** compression levels, **5** strategies, and **7** flush modes. |
| **Feature flags** | Seven: `std`, `gzip`, `gz-io`, and `simd` (the default set), plus `no-std`, `inflate_strict`, and the opt-in `c-oracle` harness gate. |
| **Toolchain** | Edition **2024**, MSRV **1.85.0** — verified on both 1.85.0 and stable 1.97.1. |
| **Dependencies** | A **two**-crate runtime closure: `cfg-if 1.0.4` plus optional `crc32fast 1.5.0`. |
| **Performance** | A constraint respected, not the objective: compression ≈ **85%** of C throughput, decompression **107–127%**. The heuristics that cost throughput also decide which bytes are emitted, so a faster match finder emitting different tokens is a regression. |

## What it provides

- **Memory-safe by construction.** Rust ownership replaces all **22** `ZALLOC` / `ZFREE` call sites of the C
  baseline (`deflate.c` 11, `inflate.c` 9, `infback.c` 2) with owned buffers, so no free path exists to forget.
  Containment is a compile error rather than a convention: the crate root carries `#![deny(unsafe_code)]` with
  exactly two narrowly scoped `#[allow(unsafe_code)]` carve-outs — the `ffi` module and a private `no_std`
  runtime block. One exception is deliberate: a gzip handle's `Drop` does **not** finish the stream, so
  `gzclose` / `gzclose_w` remain mandatory (see below).
- **Complete DEFLATE, zlib, and gzip.** An RFC 1951 encoder and decoder, with RFC 1950 and RFC 1952 framing and
  the overloaded `windowBits` contract — raw `-8..-15`, zlib `8..15`, gzip `+16`, auto-detect `+32` — resolved in
  one place. Coverage spans all 10 levels, 5 strategies, and 7 flush modes, subject to **five deliberate,
  documented divergences**; it is parity qualified by that list, never unqualified parity.
- **C ABI drop-in.** Emits `lib`, `cdylib`, and `staticlib`. The C entry points are
  `#[unsafe(no_mangle)] extern "C"` shims over the safe core, with field-exact `#[repr(C)]` mirrors of the
  **14**-field `z_stream` and **13**-field `gz_header`, plus the `gzFile_s` `{ have, next, pos }` prefix that C's
  `gzgetc` *macro* dereferences directly. Every symbol declared for the platform is exported; the one entry
  absent from a Linux build is `gzopen_w`, `#[cfg(windows)]`-gated exactly as C gates it. `ffi` is declared
  unconditionally, so the artifacts always present the full zlib C symbol table for linkage.
- **`no_std`.** A core-only build via `--no-default-features` (optionally with the `no-std` feature) omits the
  gzip file-I/O layer, which fundamentally requires `std::fs` / `std::io`. That row is a blocking CI gate.
- **Both naming conventions.** Every zlib-style camelCase entry point has an idiomatic snake_case twin —
  `compressBound` / `compress_bound`, `zlibVersion` / `zlib_version`, `zError` / `z_error`. FFI names are
  deliberately *not* re-exported at the crate root, so `ffi` stands alone as the C ABI surface, and the
  `prelude` carries types only — glob-importing it cannot pull raw-pointer entry points into scope.

## How byte-identity is proven

Two tiers, deliberately kept distinct:

- **Tier 1 — strict byte-identity.** **4,461** assertions baked from the genuine C encoder, spanning every
  level `-1..=9`, all five strategies, the zlib, raw, gzip, and small-window framings, and `memLevel` 1 / 8 / 9.
  They are baked in two encodings, because exact bytes and wide coverage pull against each other in source form:
  - **336 full literal hex** rows — the exact-bytes core. `BI_VECTORS` (225) and `BI_VECTORS_GZIP` (75) cover the
    five short corpora at `memLevel = 8`; `BI_EXTREMES` (24) and `BI_EXTREMES_GZIP` (12) add the `memLevel` 1 / 9
    corners that the `memLevel = 8` family never reaches.
  - **4,125 `(length, CRC-32)` digest** rows — `BI_GRID` (3,300) and `BI_GRID_GZIP` (825) over five 16 KiB
    shapes. 16 KiB is not an arbitrary "big enough": it is exactly the `memLevel = 8` symbol budget
    (`lit_bufsize == 1 << (memLevel + 6) == 16384`), which is what makes the `memLevel` axis observable at all.

  Because every reference byte is a precomputed constant, this gate runs by default **with no C toolchain**. Its
  exhaustive closure is the **50/50** and **3,750/3,750** live sweeps — 5 corpus shapes × 5 `windowBits` ×
  3 `memLevel`s × 10 levels × 5 strategies — against a reference C zlib, reproducible in-repository through the
  opt-in `c-oracle` harness.

- **Tier 2 — decode compatibility.** Round-trips in both directions against `flate2`'s default pure-Rust
  `miniz_oxide` backend. That is a *different* encoder with different match-finding heuristics, so tier 2 proves
  RFC wire-format conformance and is **not** treated as satisfying byte-identity; that property is proven
  exclusively by tier 1.

The official zlib test vectors are operationalised as Rust ports of the three C drivers: `test/example.c` becomes
the fixed-vector and randomized round-trip suites, `test/infcover.c` the decoder-coverage suite, and
`test/minigzip.c` the `gz*` client suite. Every row — default features, `--all-features`, and
`--no-default-features` — reports **0 failed and 0 ignored**. Absolute test counts are deliberately not published
here: they move with every test added, while that invariant does not.

## Documented divergences

Six are **visible to a C caller**, each deliberate and none a defect:

1. `gzprintf` / `gzvprintf` ship as ABI-compatible stubs returning `Z_STREAM_ERROR`, because rendering a C
   `va_list` needs the nightly-only `c_variadic` feature. This is advertised programmatically through
   `zlibCompileFlags` **bit 27**, exactly as a C zlib built without a secure `vsnprintf` behaves. The idiomatic
   Rust `gzprintf` formats fully.
2. `inflate_strict` defaults **off**, preserving acceptance parity with a default-built reference zlib.
3. The retained C baseline is excluded from the published crate.
4. Exported symbols carry no `@ZLIB_1.x` version tags by default, although the symbol *set* is exactly right;
   applying the version script is opt-in.
5. A gzip handle's `Drop` is intentionally empty of finishing logic, so `gzclose` / `gzclose_w` remain mandatory
   — a destructor cannot surface a deferred compression or I/O error.
6. A gzip destination that accepts **nothing** (`write(2)` returning `0` for a non-empty request) is reported as a
   *retryable* `Z_ERRNO` instead of being retried forever, as C's `while (strm->next_out > state->x.next)` loop
   does. The cursor, the buffered input, and the caller's true partial progress are all retained, so a retry
   resumes exactly where C would have; POSIX permits that return only for a zero-length write, which this loop
   never issues, so the case is as unreachable in practice as C's spin.

A second class exists and is deliberately kept separate: internal departures from C that are **invisible at the
C ABI**, because each is strictly stricter or strictly safer than C while leaving the return-code set, the struct
layout, and the emitted bytes untouched. Opaque state carries a kind tag, so handing a deflate
stream to `inflateEnd` is a defined `Z_STREAM_ERROR` rather than C's undefined reinterpretation. Indexing is
bounds-checked, so a path that would corrupt memory in C aborts instead. Allocation is fallible with no global
fallback, which is C's `ZALLOC` contract stated precisely. Full reasoning lives in the repository's
`CONTRIBUTING.md`, under *Internal divergences that are invisible at the C ABI*.

## Verified platforms

Platform claims deserve platform coverage, so here is the boundary drawn honestly. The test suite is **natively
executed** on `ubuntu-latest` across five feature rows, on `windows-latest` (x86_64 — the only place `gzopen_w`
is compiled and `OS_CODE` is 10), and on `macos-latest` (aarch64, where `OS_CODE` is 19). On the Windows row
`gzopen_w` is not merely compiled: `ffi::gz::tests::wide_path_open_round_trip` opens a UTF-16 path through it,
writes, closes, reopens, and reads the payload back, and a dedicated step runs that test by name so the coverage
cannot decay into a signature check. Each row also asserts its own `rustc -vV` host triple and `runner.arch`. The
aarch64, 32-bit `i686`, and big-endian `s390x` Linux triples are **cross type-checked** (with `--all-features`,
so the `c_oracle` harness and `inflate_strict` arms are included), and the bare-metal `thumbv7em-none-eabihf`
target is **built** in both `no_std` configurations. A 32-bit, big-endian, or bare-metal build is therefore
compile-verified rather than runtime-verified, and this page does not claim otherwise.

## Where to go next

| Page | What it covers |
| --- | --- |
| [Project Guide](project-guide.md) | Setup, build, test, lint, and benchmark commands, the module architecture, and troubleshooting. |
| [Technical Specifications](technical-specifications.md) | The migration specification: target design and layering, the file-by-file transformation mapping, the design patterns applied, the `unsafe`-boundary and memory-ownership analysis, and the dependency inventory. |

`README.md` in the repository root is the crate-facing overview — installation, the idiomatic Rust and C drop-in
usage guides, the feature-flag table, and the exported-symbol reconciliation. `CHANGELOG.md` records the crate's
release history, while `CONTRIBUTING.md` and `SECURITY.md` cover the contribution workflow and the vulnerability
disclosure policy. The repository is at <https://github.com/Blitzy-Sandbox/blitzy-zlib>.

The normative format specifications ship alongside this page as `rfc1950.txt`, `rfc1951.txt`, and `rfc1952.txt`,
with `algorithm.txt` and `txtvsbin.txt` covering the deflate internals and the text/binary heuristic. Canonical
copies: [RFC 1950](https://datatracker.ietf.org/doc/html/rfc1950),
[RFC 1951](https://datatracker.ietf.org/doc/html/rfc1951),
[RFC 1952](https://datatracker.ietf.org/doc/html/rfc1952).
