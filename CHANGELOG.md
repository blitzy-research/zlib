# Changelog

All notable changes to **`zlib-rs`** — the Rust crate built in this repository —
are recorded in this file.

## This file is not zlib's history

The repository carries **two** history documents, and conflating them is the one
mistake this preamble exists to prevent:

| File | Covers | Edited here? |
|------|--------|--------------|
| **`CHANGELOG.md`** (this file) | the Rust crate `zlib-rs` | yes — every crate-visible change lands here |
| [**`ChangeLog`**](ChangeLog) (no extension) | the upstream **C zlib** library, 83 version blocks back to 0.3 | **never** |

The C sources remain in this repository on purpose — they are the
cross-validation oracle and the source of the official test vectors that prove
byte-identical output — and `ChangeLog` is part of that retained baseline. It is
read, cited, and preserved verbatim; it is not this crate's release history and
it is not amended when the Rust crate changes. See
[Project layout](README.md#project-layout) in the README for why the C tree
stays.

## Conventions

- **Format:** [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/).
- **Versioning:** [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html)
  — see [Two version identities](#two-version-identities) for the one place this
  needs care.
- **Ordering:** newest version first, terse factual bullets — the ordering and
  tone the upstream [`ChangeLog`](ChangeLog) has used across every one of its
  version blocks, carried over here as format precedent.
- **Only sections with content appear.** An empty `Fixed`, `Removed`, or
  `Deprecated` heading is noise, so none is emitted until there is something to
  put under it.
- **Two non-standard sections are used deliberately.** `Compatibility` carries
  the acceptance evidence, and `Known limitations and documented divergences`
  carries the permanent, intentional departures from a literal C port. A drop-in
  replacement for a C library has both, and neither fits under any of Keep a
  Changelog's six standard headings.
- **Evidence, not assertion.** Every figure below was observed by running the
  command quoted beside it against this tree — with the single exception of the
  throughput ratios under
  [Known limitations](#known-limitations-and-documented-divergences), which are
  explicitly labelled as attributed rather than re-measured, and say why. Nothing
  here is an estimate, a projection, or a round number chosen for readability.
- **Security:** fixes are recorded under `Security` here **and** announced
  through the process in [`SECURITY.md`](SECURITY.md). Report vulnerabilities
  there, never in a public issue.

## Two version identities

`zlib-rs` reports **two different version strings on purpose**, and both are
correct:

| Identity | Value | Source of truth |
|----------|-------|-----------------|
| Cargo package version | `1.3.2` | [`Cargo.toml`](Cargo.toml) `[package] version` |
| C API `zlibVersion()` / `zlib_version()` | `"1.3.2.1-motley"` | `ZLIB_VERSION` in [`src/lib.rs`](src/lib.rs), returned by [`src/util/version.rs`](src/util/version.rs) |
| C API `ZLIB_VERNUM` | `0x1321` | `ZLIB_VERNUM` in [`src/lib.rs`](src/lib.rs) |
| Component macros | `ZLIB_VER_MAJOR 1`, `ZLIB_VER_MINOR 3`, `ZLIB_VER_REVISION 2`, `ZLIB_VER_SUBREVISION 1` | [`src/lib.rs`](src/lib.rs) |

Upstream zlib's four-component `1.3.2.1-motley` is not valid Semantic
Versioning, so Cargo cannot carry it and the package version is tracked as
`1.3.2`. The C shim is under the opposite obligation: a C consumer's
`deflateInit_` / `inflateInit_` version check and any `ZLIB_VERNUM` comparison
must see exactly what a real zlib `1.3.2.1-motley` reports, or the drop-in stops
being a drop-in.

**This split is deliberate and permanent.** SemVer governs the crate version;
ABI compatibility governs the reported C version. "Fixing" either one to match
the other would be a defect, not a cleanup.

Verified live through a C program linked against the emitted artifacts, both
statically and dynamically:

```text
ver=1.3.2.1-motley vernum=0x1321 crc=cbf43926 adler=091e01de compress=0 uncompress=0 bound=22
```

---

## 1.3.2 — unreleased

First release of the Rust crate. The version number is the one declared in
[`Cargo.toml`](Cargo.toml); **the date is deliberately absent** because the crate
has not been published and no release tag exists yet. A guessed date would be
the one kind of error this file cannot tolerate. The heading gains its date at
publication, and a new `Unreleased` section opens above it — see
[Maintaining this file](#maintaining-this-file).

This entry describes a complete C-to-Rust reimplementation of zlib
`1.3.2.1-motley`. Because there is no previous release of `zlib-rs`, `Changed`
below is measured against **the C implementation this crate replaces**, and
`Security` records the properties the initial release establishes rather than a
vulnerability it repairs. There is no `Fixed` section: with no prior release,
every entry in one would have to be invented.

### Added

- **Memory-safe Rust reimplementation of zlib `1.3.2.1-motley`.** **40 modules**
  under [`src/`](src) replace the C baseline's **26 root translation units and
  headers — 23,107 lines of C exposing 119 `ZEXTERN` entry points**. Every C
  translation unit has a named Rust owner; no C source is a runtime dependency
  of the crate.
- **DEFLATE encoder (RFC 1951)** with the full block-producer set — `stored`,
  `fast`, `slow`, `rle`, and `huff` — plus dynamic Huffman construction
  (`build_tree`, `pqdownheap`, `gen_bitlen`, `gen_codes`, `scan_tree`,
  `send_tree`, `build_bl_tree`, `compress_block`) and stored/static/dynamic block
  selection.
- **DEFLATE decoder (RFC 1951)** — the 32-state mode loop, the `inflate_fast`
  hot path, the `inflate_table` builder, the fixed Huffman tables, streaming
  `inflateSync` error recovery, and the callback-driven `inflateBack` decoder.
- **zlib framing (RFC 1950)** — CMF/FLG/FCHECK header, preset-dictionary
  Adler-32, big-endian Adler trailer — and **gzip framing (RFC 1952)** — the
  fixed/extra/name/comment/header-CRC phase walk with little-endian CRC-32 and
  ISIZE. The overloaded `windowBits` contract (raw `-8..-15`, zlib `8..15`, gzip
  `+16`, auto-detect `+32`) is resolved in **one** place, `constants.rs`, so the
  four framings cannot drift apart.
- **Compression levels `-1` and `0..=9`** with `-1` resolving to 6, all five
  strategies, the ten-row per-level tuning table ported verbatim, plus
  `deflateParams` re-dispatch and the `deflateTune` override. All seven flush
  modes, preset dictionaries, and the exact `compressBound` sizing formula.
- **C ABI compatibility layer** emitting `lib`, `cdylib`, and `staticlib` from a
  single package. **95 exported symbols, every one of type `T`** — reconciled
  exactly: 98 `#[unsafe(no_mangle)]` attribute sites resolve to **96 distinct
  names** (`gzdopen` and `inflateGetHeader` are each declared twice under
  mutually exclusive `cfg`s), minus the `#[cfg(windows)]`-gated `gzopen_w`, which
  is correctly absent on Linux. Nothing is emitted that is not declared. Against
  [`zlib.map`](zlib.map): **54/54** `global:` symbols present, **0/10** `local:`
  symbols leaked, across all 16 version nodes from `ZLIB_1.2.0` to `ZLIB_1.3.2`.
- **Field-exact `#[repr(C)]` ABI mirrors** of the 14-field `z_stream` and the
  13-field `gz_header`, the `gzFile_s { have, next, pos }` prefix that C's
  `gzgetc` **macro** dereferences directly, and all five versioned init entry
  points (`deflateInit_`, `deflateInit2_`, `inflateInit_`, `inflateInit2_`,
  `inflateBackInit_`) — which is what the `zlib.h` convenience macros expand to.
  The motley `_z` size_t-suffixed exports (`compressBound_z`, `deflateBound_z`,
  `compress_z`, `compress2_z`, `uncompress_z`, `uncompress2_z`) are present.
- **The complete `gz*` file-I/O family** — `gzopen`, `gzdopen`, `gzbuffer`,
  `gzread`, `gzfread`, `gzwrite`, `gzfwrite`, `gzgets`, `gzputs`, `gzgetc`,
  `gzputc`, `gzungetc`, `gzseek`, `gztell`, `gzflush`, `gzsetparams`, `gzeof`,
  `gzdirect`, `gzerror`, `gzclearerr`, `gzclose` / `gzclose_r` / `gzclose_w`, and
  their 64-bit variants — behind the `gz-io` feature.
- **Adler-32 and CRC-32** with `adler32_combine` / `crc32_combine` and their
  64-bit forms. The CRC-32 lookup tables are **generated at build time** by
  [`build.rs`](build.rs) in pure safe `std` Rust with **no build-dependencies**,
  replacing 9,446 checked-in lines of pre-generated C tables with a verifiable
  algorithm. Optional SIMD acceleration through the `simd` feature.
- **`no_std` support.** With `std` off the crate is `#![no_std]` + `alloc` and
  supplies its own freestanding runtime: a private libc-backed
  `#[global_allocator]`, a `#[panic_handler]`, and a personality symbol, all
  confined to one private module.
- **Seven Cargo features** replacing the C preprocessor configuration, each with
  an explicit C provenance — `std`, `gzip` (`#ifdef GZIP`), `gz-io`
  (`#ifndef NO_GZCOMPRESS`), and `simd` are on by default; `no-std` (`Z_SOLO`),
  `inflate_strict` (`INFLATE_STRICT`), and `c-oracle` are opt-in. The default set
  is exactly `["std", "gzip", "gz-io", "simd"]`. The full table lives under
  [Feature flags](README.md#feature-flags).
- **Test suite ported from the official C drivers.** `test/example.c` →
  [`tests/regression.rs`](tests/regression.rs) (fixed vectors) and
  [`tests/round_trip.rs`](tests/round_trip.rs) (the `quickcheck` randomized
  half); `test/infcover.c` →
  [`tests/inflate_coverage.rs`](tests/inflate_coverage.rs); `test/minigzip.c` →
  [`tests/gzip_compat.rs`](tests/gzip_compat.rs); checksum known-answer vectors
  in [`tests/checksum.rs`](tests/checksum.rs); and the two-tier byte-identity and
  wire-format gate in [`tests/interop.rs`](tests/interop.rs).
  **956 tests pass** by default — 797 in-crate unit tests, 130 integration tests
  (`checksum` 23, `gzip_compat` 17, `inflate_coverage` 29, `interop` 30,
  `regression` 12, `round_trip` 19), and 29 doctests (28 runnable plus one
  `compile_fail`) — with **0 failed and 0 ignored**. `--no-default-features`
  passes **695** (571 unit + 97 integration + 27 doctests) and `--all-features`
  passes **969**. CI parses every
  `test result:` line and fails on any failure, on any *ignored* test, or on a
  count below a per-row lower bound, because `cargo test` exits 0 when tests are
  skipped.
- **Opt-in live C-oracle harness** [`tests/c_oracle.rs`](tests/c_oracle.rs),
  gated behind the `c-oracle` feature, which builds reference C zlib from the
  retained in-tree sources during the test run and diffs live compressed output.
  It expands to `[]` and shells out through `std::process::Command`, so it adds
  **no dependency of any kind** and both lockfiles are untouched by it.
- **Five `cargo-fuzz` / libFuzzer targets** — inflate, deflate round-trip, gzip
  parsing, checksums, and the FFI boundary — in a **detached** `fuzz/` workspace
  that never enters the root build graph.
  [`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) builds every target
  and runs each for a bounded budget on a weekly schedule. That workflow builds and
  fuzzes only: the fuzz graph's `cargo-deny` policy is enforced by
  [`.github/workflows/audit.yml`](.github/workflows/audit.yml), the single home of
  the supply-chain gate, which evaluates it on every push and pull request.
  Detachment means the root `cargo fmt --all`
  and `cargo clippy --all-targets` gates cannot see that workspace, so the same
  workflow runs a nightly `fmt --check` and Clippy with `-D warnings` against
  `fuzz/Cargo.toml` at default features and again at `--no-default-features`, plus
  a `--no-default-features` build that asserts all five harness binaries still
  link. The 11 committed seeds under `fuzz/seeds/fuzz_inflate/` — one per accepted
  compression level — are handed to libFuzzer as read-only corpus inputs after the
  writable cached corpus, so the deterministic seed sweep runs even on a cold
  cache, and the job fails if a seed directory is empty or no longer names a real
  target.
- **Three Criterion benchmark harnesses** —
  [`benches/deflate_bench.rs`](benches/deflate_bench.rs) (all ten levels, with an
  explicit incompressible-input profile),
  [`benches/inflate_bench.rs`](benches/inflate_bench.rs), and
  [`benches/checksum_bench.rs`](benches/checksum_bench.rs), which prints the
  CRC-32 backend it actually measured.
- **Repository hygiene shipped with this release:**
  [`rust-toolchain.toml`](rust-toolchain.toml) pinning the toolchain to the MSRV,
  [`clippy.toml`](clippy.toml) and [`rustfmt.toml`](rustfmt.toml) pinning lint
  and format behaviour, [`deny.toml`](deny.toml) and
  [`fuzz/deny.toml`](fuzz/deny.toml) as the `cargo-deny` policies over the
  governed dependency closure — one reviewed policy per graph, the root graph and
  the detached fuzz graph — [`.cargo/config.toml`](.cargo/config.toml),
  [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and this
  file.

### Changed

Measured against the C implementation this crate replaces. Every item removes a
C construct that would otherwise have forced `unsafe` or manual memory
management, and every one preserves observable behaviour exactly.

- **Manual memory management replaced by Rust ownership.** All **22**
  `ZALLOC`/`ZFREE` call sites in the C baseline — `deflate.c` 11, `inflate.c` 9,
  `infback.c` 2, and zero in `gz*.c`, `inftrees.c`, and `zutil.c` — became owned
  buffers behind a single allocator abstraction. There is **no free path left to
  forget**: the deflate state, the doubled sliding window, the `prev` chain array,
  the `head` hash array, the pending/symbol buffer, the inflate history window,
  and the `inflateBack` window are all owned values whose lifetimes the compiler
  tracks.
- **Caller-supplied allocation hooks preserved, not discarded.** A C consumer's
  `zalloc`/`zfree` pair and its `opaque` pointer are unified with global
  allocation behind one owning type, keeping C's exact contract: caller buffers
  are used only when **both** hooks are set, and a null `zalloc` propagates as an
  allocation failure rather than silently falling back to the global heap.
  `deflateCopy` / `inflateCopy` route their deep copies through the same hook, so
  a caller who supplied a custom arena does not find the copy living elsewhere.
- **Self-referential interior pointers replaced by offsets.** C's `state->next`,
  `lencode`, and `distcode` point *into* `state->codes[]`, which is why a C struct
  copy leaves them dangling and `inflateCopy` has to repair them by hand. They
  became an integer offset plus a `TableSource { Fixed, Dynamic }` discriminant —
  the change that makes a plain deep `Clone` correct for `inflateCopy`, and the
  single most important ownership decision in the decoder.
- **Function-pointer dispatch replaced by a tag enum.** C's `compress_func` table
  became a `CompressFunc` tag carried in each row of the configuration table and
  resolved by an exhaustive `match` in
  [`src/deflate/strategy.rs`](src/deflate/strategy.rs). Porting the pointer table
  verbatim would have required `unsafe` and forfeited exhaustiveness checking; the
  enum keeps the whole compression core `unsafe`-free while preserving the exact
  selection behaviour.
- **Flat translation units restructured into a seven-layer module tree** mirroring
  the C `#include` layering with no extra top-level modules:
  `error`/`constants` → `util` → `checksum` → `stream`/`gz_header` →
  `{deflate, inflate}` → `gz` → `ffi`. `deflate` and `inflate` are strict peers,
  and `unsafe` may cross the `ffi` boundary in one direction only. The layering is
  the architecture rather than a claim of acyclicity: four `use` sites point
  upward — `src/stream.rs` naming the two engine states that `StreamState` owns
  (C's `z_stream.state`, which C keeps opaque), and `src/util/compress.rs` /
  `src/util/uncompress.rs` calling the engines exactly as `compress.c` and
  `uncompr.c` call `deflate()` and `inflate()` — so `deflate`↔`stream`,
  `inflate`↔`stream`, and `deflate`↔`util` reference each other. A Rust crate is a
  single compilation unit, so those are module references, not a build cycle.
- **Integer mode and status fields became `#[repr]`-tagged enums with C-exact
  discriminants,** because these values are observable through the ABI:
  `DeflateStatus` `Init = 42`, `Gzip = 57`, `Extra = 69`, `Name = 73`,
  `Comment = 91`, `Hcrc = 103`, `Busy = 113`, `Finish = 666`; `InflateMode`
  starting at the C sentinel `Head = 16180` with 32 variants; `GzMode`
  `None = 0`, `Append = 1`, `Read = 7247`, `Write = 31153`. `deflatePending`,
  `inflateSync`, and every state-dependent error path therefore behave
  identically.
- **C macros became named Rust items, never textual substitution.**
  `UPDATE_HASH` and `INSERT_STRING` are inherent methods on the deflate state;
  the `NEEDBITS`/`DROPBITS`/`BITS`/`PULLBYTE` family became inline primitives on
  a private borrowed-I/O type whose cursors the borrow checker cannot let outrun
  their slices; `ERR_RETURN` became `Err(ZlibError)` propagated with `?`;
  `Assert`/`Tracev` became `debug_assert!` or nothing.
- **Every zlib-style camelCase entry point now has an idiomatic snake_case
  twin** — `compressBound`/`compress_bound`, `zlibVersion`/`zlib_version`,
  `zError`/`z_error`, `zlibCompileFlags`/`zlib_compile_flags` — so API
  equivalence and Rust idiom coexist without either being compromised. FFI names
  are deliberately **not** re-exported at the crate root; `zlib_rs::ffi` stands
  alone as the C ABI surface.
- **The build system changed completely.** CMake and autotools are replaced by
  Cargo: the `cdylib` + `staticlib` pair supersedes the `zlib SHARED` and
  `zlibstatic STATIC` targets, and no CMake, `./configure`, or `make` step is
  required. The C build descriptors are retained for drop-in consumers but compile
  nothing into the crate.
- **The retained C baseline is now excluded from the published package.** The
  `*.c`, `*.h`, `zlib.map`, legacy platform directories, C build descriptors, and
  upstream C documentation stay in the repository as the oracle and are filtered
  out of the `.crate` by [`Cargo.toml`](Cargo.toml)'s `exclude` list. A CI job
  enforces that contract on `cargo package --locked --list` — asserting that no
  forbidden pattern matches and that every required one is present — and then
  builds the packaged crate from its own contents and runs its test suite, which
  a file listing alone cannot prove.

### Security

No vulnerability is fixed by this entry — there is no prior release. These are
the security properties the initial release establishes.

- **Zero `unsafe` in the compression and decompression core, enforced as a
  compile error.** [`src/lib.rs`](src/lib.rs) carries a crate-wide
  `#![deny(unsafe_code)]` with **exactly two** narrowly scoped
  `#[allow(unsafe_code)]` carve-outs: `pub mod ffi`, the C ABI surface, and a
  private module holding the freestanding `no_std` runtime. Every other module —
  `deflate`, `inflate`, `checksum`, `gz`, `util`, `stream.rs`, `error.rs`,
  `constants.rs`, `gz_header.rs` — measures **zero** executable `unsafe`, and a
  stray block there fails the build outright. (`src/stream.rs` is worth calling
  out: it carries exactly two `unsafe` tokens in code, and both are **type
  aliases only** — `ZallocFn` and `ZfreeFn`, which merely *name* the C hook
  signatures the crate must interoperate with. `grep -c "unsafe {"` on that file
  returns 0, and the module carries its own `#![deny(unsafe_code)]`.)
- **Every `unsafe` block that does exist is justified in place.** **513
  `// SAFETY:` comments** across `src/`, with
  `#![warn(clippy::undocumented_unsafe_blocks)]` and `#![warn(missing_docs)]`
  promoted to hard errors by the `-D warnings` lint gate. Containment is checked
  four independent ways — the `deny` attribute, the lint gate, in-crate boundary
  tests that re-derive the boundary from the source text and assert that exactly
  two carve-outs exist, and a toolchain-independent shell assertion in CI's
  `unsafe-boundary` job. `deny` rather than `forbid` is deliberate: `forbid`
  cannot be relaxed by an inner `allow`, which would make the two boundary
  carve-outs inexpressible.
- **Signature drift at the ABI is a compile error.** A `cfg(test)` guard coerces
  every exported function *item* to its exact `unsafe extern "C"` fn-pointer
  *type*, binding all 96 exported names, so a changed signature fails compilation
  instead of surfacing at a C call site.
- **Whole classes of C defect removed by construction** rather than by review:
  use-after-free, double-free, buffer overruns, and unhandled state transitions.
  Opaque handles are tagged with a `#[repr(transparent)]` discriminant so a
  cross-engine `End` call cannot reconstruct a `Box` with the wrong layout, and
  `panic = "abort"` in both profiles upholds the invariant that a Rust panic never
  unwinds across the C ABI.
- **The runtime dependency closure is two crates:** `cfg-if 1.0.4` plus the
  optional `crc32fast 1.5.0` (itself pure Rust, itself depending only on
  `cfg-if`). **No C toolchain is required to build or test the crate**, and
  `criterion`, `flate2`, `quickcheck`, and `rand` are dev-only by contract and
  never appear under `src/`.
- **Supply-chain gates over the governed closure of 102 packages** — 89 pinned by
  [`Cargo.lock`](Cargo.lock) and 13 by [`fuzz/Cargo.lock`](fuzz/Cargo.lock), both
  committed deliberately because the crate ships `cdylib`/`staticlib`
  distributables. [`deny.toml`](deny.toml) over the root graph and
  [`fuzz/deny.toml`](fuzz/deny.toml) over the fuzz graph — each named at its call
  site with an explicit `--config` — declare `[advisories]`,
  `[licenses]`, `[bans]`, and `[sources]`, resolves the graph with
  `all-features = true`, denies yanked crates (`yanked = "deny"`), and bounds
  advisory-database staleness (`maximum-db-staleness = "P7D"`). Its
  `[graph] targets` list is deliberately empty so every crate is audited for every
  platform rather than only those a target list happens to reach, and its
  `[bans] deny` list keeps
  `cc`, `bindgen`, `pkg-config`, `libz-sys`, and the bzip2/lzma/zstd/brotli
  families out of the graph by name.
  Both policies hold duplicate major versions to the same standard
  (`[bans] multiple-versions = "deny"` with
  `multiple-versions-include-dev = true` — the second key is load-bearing, since
  every duplication here is dev-only and the check would otherwise report `bans ok`
  regardless), so an unreviewed duplicate fails the build instead of printing a
  warning that nothing acts on; the root graph's **four** known dev-only
  duplications — `rand@0.10.2`, `rand_core@0.10.1`, `getrandom@0.4.3`, and
  `r-efi@6.0.0`, the chain reached through `quickcheck 1.1.0` — are acknowledged
  individually with exact-version `skip` entries that expire on the next bump, and
  `skip-tree` is deliberately empty in both files because a subtree waiver would
  silently widen as the tree changes. `r-efi@6.0.0` is the fourth precisely
  *because* `[graph] targets` is empty: the nine-triple list `deny.toml` used to
  carry pruned it out of view, which was a coverage hole rather than a refinement,
  and acknowledging it explicitly is what closing that hole costs. The two things
  the fuzz graph legitimately needs are scoped rather than waived: `cc` stays in
  `[bans] deny` but carries `wrappers = ["libfuzzer-sys"]`, and NCSA is granted
  crate-scoped rather than globally — load-bearing in `fuzz/deny.toml`, where those
  crates live, and kept verbatim in `deny.toml`, where they do not, as latent
  defence in depth. Run as documented in [`CONTRIBUTING.md`](CONTRIBUTING.md), both
  invocations report `advisories ok, bans ok, licenses ok, sources ok`: the root
  command line carries `-A unused-wrapper -A license-exception-not-encountered` for
  exactly those two latent entries, the fuzz command line carries no allowance at
  all, and both codes stay at full severity on the graph where they mean something.
  [`.github/workflows/audit.yml`](.github/workflows/audit.yml) runs the gate on
  push, pull request, a daily schedule, and manual dispatch as four independent
  blocking jobs — `policy-integrity` (both policy files, `deny.toml` and
  `fuzz/deny.toml`, still exist, still declare every governed table, still hold
  every load-bearing key at its reviewed value, and still agree on every shared
  key, so section-level erosion cannot pass vacuously), `cargo-audit` (both
  lockfiles),
  `cargo-deny` (all four root categories), and `cargo-deny-fuzz` (the detached
  fuzz graph). None declares `needs:`, so one failing category cannot mask
  another's verdict, and those four jobs are the only place either tool runs:
  [`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) builds and fuzzes the
  harnesses and declares no policy job, so the gate is not duplicated across two
  workflows that could drift apart. The consequence is stated rather than
  glossed — a job in another workflow cannot act as a `needs:` predecessor, so a
  policy verdict no longer sequences ahead of a fuzzing campaign within one run;
  it blocks the same pull request as a sibling check, on a broader set of
  triggers than a fuzz-workflow job could see. Every invocation passes
  `--locked`, so a verdict describes the committed pins rather than a graph
  resolved on the runner; every job then asserts that neither lockfile moved;
  and both tools are version-pinned (`cargo-deny` 0.20.2, `cargo-audit` 0.22.2)
  with the resolved version asserted rather than merely logged, because
  `--locked` pins a tool's own lockfile and not which release of the tool is
  installed. Neither tool is ever a manifest dependency.
- **RUSTSEC-2026-0097 affects a development dependency only** (`rand`) and does
  **not** reach consumers of the published crate — a dev-dependency is not part of
  a downstream build graph. The direct requirement is pinned at or above the
  patched `0.9.4`. Note that two `rand` majors coexist in the *development* graph
  (`0.9.4` directly, plus a newer major reached transitively through
  `quickcheck`), which is precisely the drift the `cargo-deny` policy exists to
  keep bounded.
- **Disclosure:** follow [`SECURITY.md`](SECURITY.md), which also states the
  assurance posture and its honest limitations. Do not open a public issue for a
  vulnerability.

### Compatibility

- **Byte-identical DEFLATE output against reference zlib** — not merely
  decodable by it. For a given input, level, strategy, `windowBits`, and
  `memLevel`, the compressed bytes are the same bytes.
  - **Tier 1 — always on, no C toolchain.** [`tests/interop.rs`](tests/interop.rs)
    carries **4,461 baked assertions** whose expected values were produced by the
    genuine C `1.3.2.1-motley` encoder via `deflateInit2` + `deflate(Z_FINISH)` +
    `deflateEnd`: 225 + 75 full-hex vector rows, 3,300 + 825 `(length, CRC-32)`
    digest rows across the wide grid, and 24 + 12 full-hex `memLevel` corner rows —
    336 rows of exact-bytes proof in total. Because the reference values are
    compiled-in constants, this gate runs by default in CI with no C compiler
    anywhere in sight.
  - **Tier 2 — decode compatibility, also always on.** Both directions against
    `flate2` on its pure-Rust `miniz_oxide` backend, across every framing, level,
    and strategy. `miniz_oxide` is a *different* encoder with different
    match-finding heuristics, so these tests prove RFC wire-format conformance and
    are explicitly **not** treated as satisfying byte-identity — that property is
    proven exclusively by tier 1.
  - **Live sweep — additive, never a replacement.**
    [`tests/c_oracle.rs`](tests/c_oracle.rs) builds reference C zlib from the
    retained in-tree sources during the test run and returned **3750/3750** and
    **50/50 byte-identical**, across a grid of 5 corpus shapes × 5 `windowBits` ×
    3 `memLevel`s × 10 levels × 5 strategies, with `deflateBound`-based
    destination sizing included because C's `deflate_stored` consults `avail_out`.
    `Z_DEFAULT_COMPRESSION` resolved to level 6 in 5/5 configurations, matching
    the reference byte for byte.
- **Decompression accepts any valid stream** — zlib, raw DEFLATE, or gzip —
  including output from other implementations.
- **Live C-ABI drop-in verification**, statically and dynamically linked:

  ```text
  ver=1.3.2.1-motley vernum=0x1321 crc=cbf43926 adler=091e01de compress=0 uncompress=0 bound=22
  gzip: deflate=1 inflate=1 clen=1017 olen=50000 identical=yes magic=1f 8b
  ```

  A 50,000-byte payload round-trips byte-exactly through the streaming API at
  `windowBits = 31` with correct `1f 8b` gzip framing, in **both** link modes.
- **`LD_PRELOAD` substitution verified on an unmodified binary.** A C program
  compiled and linked against the *system* `libz` reports `impl=1.3.1`; the same
  binary, re-run under `LD_PRELOAD=…/libzlib_rs.so`, reports
  `impl=1.3.2.1-motley` and returns identical results
  (`crc=cbf43926 compress=0 uncompress=0 recovered=yes`) — no recompilation, no
  relink, no source change. That is the drop-in property demonstrated end to end
  rather than inferred from the symbol table.
- **MSRV `1.85.0`, verified rather than assumed.** On
  `rustc 1.85.0 (4d91de4e4 2025-02-17)` both `cargo build --locked` and
  `cargo check --locked --all-targets` exit 0; the same tree builds and passes its
  full suite on stable `rustc 1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)`. Edition
  **2024** — 1.85.0 is precisely the release in which edition 2024 became
  available, making it the tightest self-consistent floor an edition-2024 crate
  can declare rather than a round number. **A change to the MSRV is a deliberate,
  documented change and must be recorded in this file.**
- **All six blocking quality gates pass** on this tree. Each was run with
  `RUSTUP_TOOLCHAIN=stable`, because [`rust-toolchain.toml`](rust-toolchain.toml)
  pins this repository to the MSRV floor and a bare `cargo …` therefore invokes
  the 1.85.0 compiler:

  | Gate | Result |
  |------|--------|
  | `cargo fmt --all -- --check` | exit 0 |
  | `cargo clippy --locked --all-targets --all-features -- -D warnings` | exit 0 |
  | `cargo build --locked` | exit 0 |
  | `cargo test --locked` | **956 passed / 0 failed / 0 ignored** |
  | `cargo test --locked --no-default-features` | **695 passed / 0 failed / 0 ignored** |
  | `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features` | exit 0, 0 warnings |
  | `mkdocs build --strict` | exit 0, 0 strict diagnostics |

  On the documentation row, "0 strict diagnostics" means no `WARNING` and no
  `ERROR` from MkDocs, its plugins, or this project's content. The Material theme
  additionally prints one advisory banner of its own — an upstream notice from the
  Material for MkDocs maintainers about the forthcoming MkDocs 2.0. It is a vendor
  notice rather than a build diagnostic: `--strict` does not fail on it, and nothing
  in this repository can suppress it.

  The ignored-test count is **zero in every configuration and stays zero**. A
  capability that cannot be exercised in a given build is expressed by a feature
  gate or by a run-time probe that passes with a printed notice — never by
  `#[ignore]`.
- **Release artifacts** from `cargo build --locked --release` under **default**
  features on `x86_64-unknown-linux-gnu` with `rustc 1.97.1 (8bab26f4f 2026-07-14)`,
  into the repository's default `target/release/`: `libzlib_rs.rlib` 2,791,948 bytes ·
  `libzlib_rs.so` 660,944 · `libzlib_rs.a` 22,441,620, observed on **2026-08-03**.
  Read those as a dated, environment-specific snapshot of one build — not a
  reproducible invariant and not a size budget: no gate asserts them, and they move
  with the compiler, the feature row, the profile, and any change to the crate's own
  sources or doc metadata. The `.rlib` is the least durable of the three, because an
  rlib embeds absolute build paths and therefore changes size when the checkout
  directory or `CARGO_TARGET_DIR` changes without a single line of source changing.
  Reproduce them for your own build with
  `stat -c '%s' target/release/libzlib_rs.{rlib,so,a}`. All three share one output
  path, so the last feature row built wins — rebuild with the intended features
  immediately before linking a C consumer.

### Known limitations and documented divergences

All of the following are **deliberate and preserved**, not pending fixes
(AAP §0.8.2). Each is listed with the reason it must stay.

The list above is exactly the set of divergences a **C caller can observe**. Internal departures that are invisible at the C ABI are tracked separately in `CONTRIBUTING.md` rather than here, because each is strictly stricter or strictly safer than C while leaving the return-code set, the struct layout, and the emitted bytes untouched: `deflateSetHeader` deep-copies the header instead of retaining the caller's pointer and still reports through C's exact `{Z_OK, Z_STREAM_ERROR}` return set; opaque state is kind-tagged so a cross-engine `End` is a defined error rather than C's undefined reinterpretation; indexing is bounds-checked; and allocation is fallible with no global fallback.

- **`gzprintf` / `gzvprintf` return `Z_STREAM_ERROR`.** Consuming a C `va_list`
  requires the nightly-only `c_variadic` language feature, which would break the
  crate's stable build and its MSRV contract. Both symbols are still exported with
  the correct signatures — removing them would break linkage — and the limitation
  is **programmatically detectable**: `zlibCompileFlags` sets **bit 27**, exactly
  as a C zlib built without a secure `vsnprintf` does. Measured through the C ABI:
  `zlibCompileFlags() = 0x080000a9` (bit 27 set) and `gzprintf(NULL, "x") = -2`,
  which is `Z_STREAM_ERROR`. The idiomatic Rust `gzprintf`, which takes
  `core::fmt::Arguments` instead of a `va_list`, formats fully.
- **`inflate_strict` defaults to OFF.** Enabling it changes which streams are
  accepted, so the default build deliberately matches a default-built reference
  zlib. Byte-exactness and acceptance parity take precedence over stricter
  validation; enable the feature only to reject out-of-window distances early.
- **`gzclose` / `gzclose_w` remain mandatory.** The gzip state's `Drop` is
  intentionally empty of finishing logic, because a destructor cannot surface a
  deferred compression or I/O error — silently swallowing a failed write of a
  member's final block and trailer during unwinding would be strictly worse than
  matching C's explicit-close contract. This is a documented departure from
  idiomatic Rust cleanup and must not be "improved" into an auto-finishing
  destructor.
- **Exported symbols carry no `@ZLIB_x.y.z` version tags by default.** The symbol
  *set* is exactly right (95 emitted, 54/54 `zlib.map` globals, 0/10 locals
  leaked); only the version *tags* are absent, and static linking, ordinary
  dynamic linking, `-lz` substitution, and `LD_PRELOAD` are all unaffected.
  Opting in with `ZLIB_RS_VERSION_SCRIPT=1` makes [`build.rs`](build.rs) derive a
  version script from [`zlib.map`](zlib.map) and apply it to the `cdylib`,
  yielding the same 95 symbols with 54 tagged and all 16 `ZLIB_*` nodes declared.
  It is off by default because a version script is a GNU-ld/ELF-only construct and
  no CI row sets the variable, so the opt-in path carries linker-portability risk
  the matrix does not yet retire.
- **A gzip destination that accepts nothing yields a *retryable* `Z_ERRNO`
  instead of an endless retry.** C's inner drain loop leaves its output cursor
  unchanged when `write(2)` returns `0` for a non-empty request and simply tries
  again forever, which is an unbounded spin inside the library (CWE-835). Both
  Rust output loops report the condition instead and mark it retryable, so the
  output cursor and the buffered input are retained, the stream is not declared
  dead, and `gzwrite` reports its true partial count rather than `0` — a retry
  therefore reaches exactly the outcome C's spin would have. POSIX allows a `0`
  return only for a zero-length write, which neither loop ever issues, so the
  case is as unreachable in practice as C's retry.
- **The retained C baseline is excluded from the published crate.** It is
  indispensable in-repository — oracle, specification, and the source of the
  official test vectors — and dead weight in a `crates.io` package.
- **Platform coverage, stated honestly.** CI runs **twelve jobs**. Windows
  (x86_64) and macOS (aarch64) execute the real suite natively, which is what
  exercises `OS_CODE = 10`, `OS_CODE = 19`, and the `#[cfg(windows)]`-gated
  `gzopen_w` — the Windows row both compiles that symbol and **executes**
  `ffi::gz::tests::wide_path_open_round_trip` (a UTF-16 open/write/close/reopen/
  read round trip), with a dedicated step running that test by name so the
  coverage cannot silently decay into a compile-only check. Each row also asserts
  its own `rustc -vV` host triple and `runner.arch`, so a runner label that
  changes architecture fails the job rather than weakening the claim.
  `aarch64`, 32-bit `i686`, and **big-endian** `s390x` are **cross
  type-checked, not natively run**, and the bare-metal `thumbv7em-none-eabihf`
  target is **built, not run**. So a 32-bit, big-endian, or bare-metal build is
  compile-verified rather than runtime-verified, and `no_std` has been validated on
  a hosted target rather than on real embedded hardware. The declared lifecycle is
  **experimental** while parity work continues; see
  [Portability](README.md#portability-what-ci-actually-exercises) for the exact
  boundary.
- **Performance is a constraint on this work, not its objective.** This is
  explicitly not a performance refactor.

  *The ratios in this bullet are **attributed**, not re-measured here.* They come
  from the migration's own benchmarking against a reference C build and are the
  one place this file quotes a figure it does not reproduce on demand, because the
  Criterion suite links no C library and there is no in-tree *performance* oracle
  — [`tests/c_oracle.rs`](tests/c_oracle.rs) is a *conformance* oracle for
  byte-identity, which is a different job.

  The aggregate position against C zlib `1.3.2.1-motley` is **compression ≈ 85%**
  and **decompression 107–127%** of C throughput, so decompression is at or above
  parity. Per-profile comparison refined the compression picture and inverted the
  intuitive reading: incompressible input is the profile *closest* to C at roughly
  82–86% (the match finder fails fast there — the two-byte prefilter rejects
  nearly every candidate, and stored blocks get selected because a dynamic tree
  cannot pay for itself), while compressible profiles are the *furthest* at
  roughly 58–64% (hash chains genuinely walked, lazy matching evaluated, Huffman
  trees built and emitted). Per-profile decompression measured 104–125%, which
  *overlaps* the quoted 107–127% aggregate on 107–125% without containing it,
  so parity holds throughout. **The hard rule:** any candidate compression speed-up
  must clear the byte-identity gate before it is viable, because the very
  heuristics that cost throughput are the ones that determine the output bytes —
  the chain-length **quartering** at `good_match` (`chain_length >>= 2`, which
  divides by four rather than two), the `nice_match` early break, and the
  `TOO_FAR` lazy-match filter. *A faster match finder that emits different tokens
  is a regression, not an improvement, no matter what the benchmark says.*
  Permissible optimisation is limited to work that provably cannot change the
  token stream: bounds-check elision, memory-access patterns, inlining, and
  buffer-copy strategy.

---

## Maintaining this file

- **Newest version first.** One section per released version, with its release
  date, under a heading of the form `## X.Y.Z — YYYY-MM-DD`. Accumulate work in
  progress under an `## Unreleased` heading at the top and rename it at release.
- **Never invent a figure or a date.** Every number here is a value observed by
  running the command quoted beside it. If a value cannot be measured, state that
  instead of estimating it, and if a date is not yet knowable, leave the version
  unreleased. Superseding a stale measurement with a fresh one is expected; adding
  an unverified one is not.
- **Four classes of change must always be recorded here,** because they are the
  things a `libz` drop-in consumer cannot discover any other way:
  1. the **MSRV**,
  2. the **feature set** — names, defaults, or semantics,
  3. the **exported symbol surface** — additions, removals, or signature changes,
  4. any **documented divergence**, whether added, removed, or altered in scope.
- **Security fixes get two records:** a `Security` entry in this file *and* an
  advisory published through the process in [`SECURITY.md`](SECURITY.md).
- **Behavioural change is a defect unless it is deliberate and documented.**
  Structural improvement is the objective. That includes the non-obvious cases —
  allocation count and failure timing, numeric error codes, state discriminants
  observable through state-dependent entry points, and the mandatory-`gzclose`
  contract. If a divergence is unavoidable, document it under
  [Known limitations and documented divergences](#known-limitations-and-documented-divergences)
  and, where the ABI can carry the signal, advertise it through
  `zlibCompileFlags` rather than leaving it implicit.
- **The upstream [`ChangeLog`](ChangeLog) is never edited.** C-baseline history
  belongs there; Rust-crate history belongs here.
- See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the quality gates a change must
  clear before it is eligible for an entry in this file.
