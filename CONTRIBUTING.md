# Contributing to `zlib-rs`

Thank you for considering a contribution. This document is the practical guide to
working in this repository: how to get set up, which gates a change must clear,
what the project will never accept a change to, and what a reviewable pull request
looks like here.

Read it before your first change. `zlib-rs` is a drop-in replacement for a library
that has been embedded in nearly everything for thirty years, and that imposes
constraints an ordinary Rust crate does not have — most notably that **compressed
output must stay byte-for-byte identical to reference C zlib**. Several things
which look like obvious improvements are, in this repository, regressions. They are
named explicitly below so nobody has to discover them in review.

## Contents

- [What this project is](#what-this-project-is)
- [How the rules in this document are sourced](#how-the-rules-in-this-document-are-sourced)
- [⚠ The retained C sources are the oracle](#-the-retained-c-sources-are-the-oracle)
- [Getting set up](#getting-set-up)
- [MSRV policy](#msrv-policy)
- [The blocking quality gates](#the-blocking-quality-gates)
- [⚠ Byte-identity: the defining acceptance criterion](#-byte-identity-the-defining-acceptance-criterion)
- [⚠ The `unsafe` boundary](#-the-unsafe-boundary)
- [⚠ The C ABI is a compile-time-guarded contract](#-the-c-abi-is-a-compile-time-guarded-contract)
- [No silent behaviour change](#no-silent-behaviour-change)
- [Five divergences that must be preserved, not fixed](#five-divergences-that-must-be-preserved-not-fixed)
- [Performance policy](#performance-policy)
- [Working in this repository](#working-in-this-repository)
- [Pre-submission checklist](#pre-submission-checklist)
- [Out of scope for contributions](#out-of-scope-for-contributions)
- [Reporting a vulnerability](#reporting-a-vulnerability)
- [Conduct](#conduct)
- [Where to look next](#where-to-look-next)

## What this project is

`zlib-rs` is a memory-safe, idiomatic Rust reimplementation of the zlib C
compression library, tracking upstream `1.3.2.1-motley` (`ZLIB_VERNUM 0x1321`). It
lives **in the same repository as the C baseline it replaces** — that co-location is
**preservation directive D-8**, and the C sources were deliberately retained rather
than deleted, for reasons that matter to every change you will make and that are
spelled out in [the oracle section](#-the-retained-c-sources-are-the-oracle).

Three properties are preserved absolutely, and every other consideration is
subordinate to them:

1. **Full DEFLATE format compatibility** — RFC 1951 DEFLATE, RFC 1950 zlib framing,
   RFC 1952 gzip framing.
2. **Exact C API equivalence** — the same symbols, the same signatures, the same
   `#[repr(C)]` layouts, the same numeric return codes.
3. **Behavioural fidelity to the reference implementation** — including behaviour a
   naive port would consider internal, such as allocation count and failure timing.

Know the scale of what you are touching:

| Quantity | Measured value | How it was measured |
|----------|----------------|---------------------|
| Rust modules under `src/` | **40 files**, **72,082 lines** (2026-08-04) | `find src -name '*.rs' -print0 \| xargs -0 wc -l` — the `total` row, with the file count from the same listing |
| Retained C baseline | **23,107 lines** across 26 root translation units and headers | `cat` of the 26 files piped to `wc -l` |
| Public C entry points the baseline declares | **119** `ZEXTERN` declarations in `zlib.h` | the retained header |
| Exported C symbols this crate emits | **95**, all type `T` | `nm -D --defined-only target/release/libzlib_rs.so` |
| Tests, default features | **959 passed / 0 failed / 0 ignored** | `cargo test --locked` |

Three companion documents carry things this one deliberately does not repeat:

- [`README.md`](README.md) — the user-facing overview: feature matrix, the drop-in
  ABI, usage examples, measured evidence, and the portability boundary.
- [`SECURITY.md`](SECURITY.md) — the vulnerability disclosure policy. **Use it
  instead of a public issue** for anything that looks like a memory-safety or
  correctness vulnerability.
- [`CHANGELOG.md`](CHANGELOG.md) — release history for the **Rust crate**. Note that
  the separate upstream [`ChangeLog`](ChangeLog) file (no extension) is the **C
  baseline's** history: it is retained unmodified and is not this crate's changelog.
  Do not add entries to it.

**License.** Contributions are made under the **zlib/libpng license**, the same
permissive license as upstream zlib — [`LICENSE`](LICENSE) carries the full text and
[`Cargo.toml`](Cargo.toml) declares `license = "Zlib"`. By opening a pull request you
confirm you have the right to contribute the code under that license.

## How the rules in this document are sourced

Everything below that reads like a project rule comes from one of exactly two
places, and the difference is worth keeping straight when you are arguing about a
review comment.

**1. The migration's four external constraints.** These are requirements imposed on
the project from outside it, quoted verbatim wherever they appear in this document:

> 1. "Output must be binary-compatible with zlib-produced streams."
> 2. "FFI layer must match the zlib C API signature exactly."
> 3. "Zero unsafe blocks in core compression logic."
> 4. "Must pass the official zlib test vectors."

**2. Ten plan-adopted engineering standards, `S1`–`S10`.** These are the migration
plan's *own* engineering commitments — this project's standards, adopted by the
plan, not handed down from anywhere else. They are cited by identifier throughout,
always labelled **plan-adopted**, and this document never presents them (or anything
else in it) as a user-specified rule. **No user-specified rules were provided for
this project**, so enterprise-standard best practice governs — and that absence is
emphatically not a licence to lower the bar. The same governance note appears in
[`deny.toml`](deny.toml), for the same reason.

The standards, in one place, so the inline citations are readable:

| ID | Plan-adopted standard | Where it bites in this document |
|----|-----------------------|---------------------------------|
| **S1** | Evidence over assertion | [PR expectations](#pull-requests) |
| **S2** | Unsafe containment by construction, not by convention | [The `unsafe` boundary](#-the-unsafe-boundary) |
| **S3** | Bit-exactness is a release gate, not an aspiration | [Byte-identity](#-byte-identity-the-defining-acceptance-criterion) |
| **S4** | The ABI is a compile-time-guarded contract | [The C ABI contract](#-the-c-abi-is-a-compile-time-guarded-contract) |
| **S5** | No silent behaviour change | [No silent behaviour change](#no-silent-behaviour-change) |
| **S6** | Supply-chain hygiene with concrete pinned versions | [Supply-chain gates](#supply-chain-gates) |
| **S7** | Reproducible toolchain | [MSRV policy](#msrv-policy) |
| **S8** | Platform claims require platform coverage | [Platform honesty](#platform-honesty) |
| **S9** | Documentation must be internally consistent | [Documentation](#documentation) |
| **S10** | Quality gates stay green and blocking | [The blocking quality gates](#the-blocking-quality-gates) |

Alongside them sit eight **preservation directives**, written `D-1` through `D-8`
with a hyphen. They are the concrete "do not change this" list, and each is
introduced where it applies. The hyphen matters: the *unhyphenated* identifiers
`D1`–`D12` are a different namespace entirely — they label the production-readiness
artifacts the plan called for, such as `D1` = [`deny.toml`](deny.toml),
`D4` = [`rust-toolchain.toml`](rust-toolchain.toml),
`D9` = [`tests/c_oracle.rs`](tests/c_oracle.rs), and `D6` = this file plus
[`SECURITY.md`](SECURITY.md).

## ⚠ The retained C sources are the oracle

**Preservation directive D-7: the C sources stay in the tree, unmodified.**

`*.c`, `*.h`, `test/*.c`, and [`zlib.map`](zlib.map) are the **cross-validation
oracle and the specification**. They are read, cited line by line in module
documentation headers, and compiled for cross-validation. **They are never edited.**
A pull request that modifies a C file will be rejected on sight, however small or
obviously-correct the edit looks.

The reason is not sentiment. Deleting or editing them would remove *"the only
independent cross-validation oracle and the official test vectors"* — and with them,
every mechanism by which byte-identity can be proven rather than asserted:

- [`tests/interop.rs`](tests/interop.rs) tier 1 compares against vectors baked from
  the genuine C encoder.
- [`tests/c_oracle.rs`](tests/c_oracle.rs) compiles those same `*.c` files at run
  time and diffs live output.
- The official zlib test vectors are not distributed as data files; they are embedded
  in `test/example.c`, `test/infcover.c`, and `test/minigzip.c`, whose assertions
  are what the Rust suite ports. That is how external constraint 4 — *"must pass the
  official zlib test vectors"* — is satisfied at all.
- Every ambiguity the RFCs leave open is resolved by reading the C implementation in
  the same tree.

Keeping them costs crate consumers nothing: [`Cargo.toml`](Cargo.toml)'s `exclude`
list keeps `*.c`, `*.h`, `*.in`, `*.map`, the C build descriptors, the legacy
platform trees, and the upstream C documentation out of the published `.crate`. The
`package-verify` job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
enforces that with `cargo package --list`.

If you need a reference build of C zlib, **copy the sources to a scratch directory
outside the working tree first** and build there. `tests/interop.rs` documents the
exact `gcc` invocation and `ar` line in its "Regenerating the vectors" section.
Neither the objects, the archive, nor any corpus dump is ever committed.

Three further areas are out of bounds for contribution, for a different reason —
they are not part of this library:

- **`contrib/**`** — nineteen subdirectories of third-party bindings and language
  ports (`ada`, `blast`, `delphi`, `dotzlib`, `infback9`, `minizip`, `puff`,
  `vstudio`, and the rest).
- **`examples/**`** — the C sample programs (`zpipe.c`, `gun.c`, `zran.c`, …). No
  Rust equivalents are wanted; producing them would be "tooling beyond the library
  itself", which is explicitly out of scope.
- **The legacy platform build trees** — `amiga/`, `msdos/`, `os400/`, `qnx/`,
  `watcom/`, `win32/`.

## Getting set up

No CMake, `./configure`, or `make` is required. A stock Cargo toolchain builds
everything.

```sh
git clone https://github.com/Blitzy-Sandbox/blitzy-zlib
cd blitzy-zlib

# --locked keeps the committed Cargo.lock authoritative, so your first build
# resolves the same 89 packages CI resolves instead of silently rewriting it.
# The bare form below runs on the MSRV compiler, because rust-toolchain.toml
# pins 1.85.0 — see the next section.
cargo build --locked
cargo test  --locked

# The same two gates the way CI runs them, on current stable.
RUSTUP_TOOLCHAIN=stable cargo build --locked
RUSTUP_TOOLCHAIN=stable cargo test  --locked
```

Both toolchains are supported and both are expected to pass; running the pair is the
cheapest way to catch an MSRV regression before you push.

### The toolchain pin, and the one thing it will surprise you with

[`rust-toolchain.toml`](rust-toolchain.toml) pins `channel = "1.85.0"` with the
`rustfmt` and `clippy` components (this is migration artifact `D4`). That is
deliberate: a contributor's local build then resolves to **the MSRV compiler**, so
MSRV breakage is found while you are writing the code rather than by the CI `msrv`
job after you have pushed.

The consequence is that a bare `cargo …` inside this repository is **not** current
stable. To run something the way CI runs it, name the toolchain:

```sh
# What the CI jobs do: an explicit stable override.
RUSTUP_TOOLCHAIN=stable cargo test --locked
cargo +stable clippy --locked --all-targets --all-features -- -D warnings

# What the MSRV job does.
RUSTUP_TOOLCHAIN=1.85.0 cargo build --locked
```

`rustup`'s precedence is why this works: a `RUSTUP_TOOLCHAIN` environment variable
and a `+toolchain` argument both outrank `rust-toolchain.toml`. CI installs its
toolchain with `dtolnay/rust-toolchain@…` and *also* sets `RUSTUP_TOOLCHAIN` at job
level, precisely so the repository pin cannot silently win — the `build-test`,
`lint`, `benches`, `msrv`, and nightly `fuzz` jobs each verify their resolved
toolchain in a dedicated step and fail loudly if the override was lost.

**Never edit `rust-toolchain.toml` to test on a different toolchain.** Use
`+stable` or `RUSTUP_TOOLCHAIN=`. Changing the pin changes the MSRV contract, which
is a four-file change described under [MSRV policy](#msrv-policy).

### Reference environment

Every measured figure in this document was observed on this environment:

| Tool | Version |
|------|---------|
| `rustc` / `cargo` (stable) | `1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)` / `1.97.1` |
| `rustfmt` | `1.9.0-stable` |
| `clippy` | `0.1.97` |
| `rustc` (MSRV) | `1.85.0 (4d91de4e4 2025-02-17)` |
| `rustc` (nightly, detached fuzz gates only) | `1.99.0-nightly (ad3d0bc14 2026-07-31, LLVM 22.1.8)` |
| `clippy` / `rustfmt` (nightly) | *not installed on this environment* — see the note below |
| `cargo-deny` / `cargo-audit` / `cargo-fuzz` | `0.20.2` / `0.22.2` / `0.13.2` |
| `mkdocs` / `mkdocs-material` / `mkdocs-techdocs-core` / `mkdocs-mermaid2-plugin` / `pymdown-extensions` | `1.6.1` / `9.7.6` / `1.7.0` / `1.2.3` / `10.21.3` |
| `gcc` (optional, C oracle only) | `15.2.0` |
| Host | `x86_64-unknown-linux-gnu` |

**No nightly `clippy` or `rustfmt` version is quoted, because none was observed.** A
dated nightly channel installs `cargo`, `rustc` and `rust-std` and nothing else, so on
this environment `cargo +nightly-2026-08-01 clippy --version` and `... fmt --version`
both report the component is not installed. The detached-fuzz format and lint gates in
[`fuzz.yml`](.github/workflows/fuzz.yml) are nevertheless real: its install step
requests `components: clippy, rustfmt`, so CI has them and this environment does not.
Reproduce those two gates locally with:

```sh
rustup component add --toolchain nightly-2026-08-01 clippy rustfmt
```

Every other row above was read back from the tool itself rather than transcribed, which
is the whole point of the table — a version nobody can reproduce is worse than an
absent one.

A C compiler is needed for exactly one optional thing — the live byte-identity sweep
described under [byte-identity](#tier-3--the-opt-in-live-c-oracle-sweep). Nothing
else in the build or the default test suite needs one, so the version above is
whatever happened to be installed rather than a requirement: the sweep asserts that
`zlib-rs` matches whatever the in-tree `*.c` sources compile to, which is a property
of those sources and not of the compiler that built them. Pin nothing here.

### Tools CI installs, which are not manifest dependencies

Three cargo tools and one Python toolchain are used by CI and are installed there
rather than declared as dependencies. Keep it that way; none of them may become a
manifest dependency, and none of them may be installed unpinned — an unpinned
install lets an upstream release change a gate's verdict without a commit.

```sh
# Supply-chain policy and advisory scanning (pinned versions, as CI pins them).
# `+stable` is REQUIRED here, not stylistic — see below.
cargo +stable install cargo-deny  --version 0.20.2 --locked
cargo +stable install cargo-audit --version 0.22.2 --locked

# Fuzzing. cargo-fuzz REQUIRES nightly — it needs the -Z sanitizer flags.
# Pinned to the same version fuzz.yml installs and then asserts by resolving
# `cargo fuzz --version`, so a local reproducer runs the same tool CI ran.
cargo +nightly-2026-08-01 install cargo-fuzz --version 0.13.2 --locked

# The published-documentation gate. CI's `docs` job resolves CPython 3.12.13 and
# installs the whole documentation closure - these three plus their 43 transitive
# dependencies - from a hash-pinned requirement set held INLINE in the `docs` job of
# `.github/workflows/ci.yml`, under `--require-hashes`, then runs
# `mkdocs build --strict`. To reproduce that exactly, copy the `REQS` heredoc out of
# that job into a file and install from it:
#   python -m pip install --require-hashes --only-binary=:all: -r <that-file>
# Naming the three below is the looser local equivalent - pip resolves the other
# forty-three at whatever versions the index serves today, which is fine for a
# local preview and is precisely what the lock exists to prevent in CI.
python -m pip install 'mkdocs==1.6.1' 'mkdocs-techdocs-core==1.7.0' \
  'mkdocs-mermaid2-plugin==1.2.3'
```

**Why `+stable` on the first two.** Both tools declare a `rust-version` **above this
repository's pin**, so a bare `cargo install` run from inside the checkout is resolved
by `rust-toolchain.toml` to 1.85.0 and fails during *resolution* — before a single
crate is compiled. Measured in this working tree:

```sh
cargo info cargo-deny@0.20.2  | grep rust-version   # rust-version: 1.88.0
cargo info cargo-audit@0.22.2 | grep rust-version   # rust-version: 1.88

# Bare form, from the repository root:
cargo install cargo-deny --version 0.20.2 --locked
# error: cannot install package `cargo-deny 0.20.2`, it requires rustc 1.88.0 or
#        newer, while the currently active rustc version is 1.85.0
# `cargo-deny 0.18.3` supports rustc 1.85.0

cargo install cargo-audit --version 0.22.2 --locked
# error: cannot install package `cargo-audit 0.22.2`, it requires rustc 1.88 or
#        newer, while the currently active rustc version is 1.85.0
# `cargo-audit 0.22.1` supports rustc 1.85
```

Note what cargo offers in each case: an *older* tool that does satisfy 1.85.0. Taking
that suggestion would silently downgrade the gate — [`deny.toml`](deny.toml) is
authored against cargo-deny **0.20.2** and CI pins that version at both of its call
sites — so the correct response is to name the toolchain, never to relax the
`--version` pin.

**Why `+nightly-2026-08-01` on the third.** `cargo-fuzz` needs nightly outright for its
`-Z` sanitizer and coverage flags, so the failure mode there is the same shape for a
different reason. The date is pinned rather than floating so a local reproducer runs
the toolchain CI ran.

`+stable` is rank 1 in `rustup`'s precedence order and therefore beats the
[`rust-toolchain.toml`](rust-toolchain.toml) pin (rank 4). Nothing about the pin needs
changing to install a tool; name the toolchain on the command instead. `--locked` here
pins the *tool's own* lockfile, and `--version` is what pins which release of the tool
you get.

**Two distinct mechanisms, and the workflows do not all use the same one.** Naming a
channel on `dtolnay/rust-toolchain`'s `with: toolchain:` input decides what gets
**installed**; a `+toolchain` prefix or a `RUSTUP_TOOLCHAIN` environment variable
decides what gets **run**. Every install step in all three workflows names its channel
explicitly — that part is uniform, and it has to be, because pinning the action to a
commit SHA means the `@<ref>` no longer selects a toolchain. What differs is the
selector, and the difference is deliberate:

| Workflow | Selector for the commands it runs | Measured |
|----------|-----------------------------------|----------|
| [`ci.yml`](.github/workflows/ci.yml) | Both: a `+stable` / `+1.85.0` prefix **and** a job-level `RUSTUP_TOOLCHAIN` | 27 prefixed commands, 12 job-level `env:` keys |
| [`audit.yml`](.github/workflows/audit.yml) | Both: a `+stable` prefix **and** a job-level `RUSTUP_TOOLCHAIN: stable` | 19 prefixed commands, 3 job-level `env:` keys |
| [`fuzz.yml`](.github/workflows/fuzz.yml) | **Only** the job-level `RUSTUP_TOOLCHAIN: nightly-2026-08-01`; every command is deliberately **unprefixed** | 0 prefixed commands, 1 job-level `env:` key |

`fuzz.yml` is the exception on purpose. The env var (rank 2) is inherited by the nested
`cargo build` invocations `cargo-fuzz` spawns, which a per-command prefix is not; and
because the job asserts the *ambient* resolution before building anything, a prefix on
each command would make that assertion vacuous for exactly the commands carrying it.
So when reading or reproducing a fuzz step, do not expect a `+nightly-2026-08-01` on
the command line — set it once for the shell instead:

```sh
RUSTUP_TOOLCHAIN=nightly-2026-08-01 cargo fuzz build
```

These are CI tools, not consumers of this library, so their compiler requirements have
no bearing on the crate's own MSRV contract. Build them with current stable (or the
pinned nightly) and leave the 1.85.0 floor to the library itself.

## MSRV policy

**Plan-adopted standard S7 — Reproducible toolchain.** The MSRV is **verified, not
assumed**. Both of these must pass, and the CI `msrv` job runs exactly them:

```sh
RUSTUP_TOOLCHAIN=1.85.0 cargo build --locked
RUSTUP_TOOLCHAIN=1.85.0 cargo check --locked --all-targets
```

The declared MSRV is **1.85.0**, and the pairing with the crate's edition is forced
rather than chosen: [`Cargo.toml`](Cargo.toml) declares `edition = "2024"`, and
**1.85.0 is precisely the release in which edition 2024 became available**. It is
therefore the tightest self-consistent MSRV an edition-2024 crate can declare —
lower is impossible, higher would be an unforced restriction on consumers.

### Raising the MSRV is a four-file change in one commit

The same fact is stated in four places. If they drift, three of them are lying:

| Where | Key |
|-------|-----|
| [`Cargo.toml`](Cargo.toml) | `rust-version = "1.85.0"` |
| [`rust-toolchain.toml`](rust-toolchain.toml) | `channel = "1.85.0"` |
| [`clippy.toml`](clippy.toml) | `msrv = "1.85.0"` |
| [`.github/workflows/ci.yml`](.github/workflows/ci.yml) | the `msrv` job's `RUSTUP_TOOLCHAIN` / `@<ref>` / `+<ref>` pins |

Move all four together, justify the bump in the pull request, and record it in
[`CHANGELOG.md`](CHANGELOG.md) — the MSRV is one of the four classes of change that
file requires an entry for, because a `libz` drop-in consumer cannot discover it any
other way.

Two traps make this worth stating explicitly rather than leaving to habit:

- **`clippy.toml`'s `msrv` overrides `Cargo.toml`'s `rust-version`** when both are
  present. Clippy does report the disagreement, but as a configuration-reader
  warning rather than a lint — so `-D warnings` does **not** promote it to an error
  and CI will not catch the drift for you.
- **Clippy only sees `Cargo.toml`'s `rust-version` because *cargo* forwards it.** Any
  clippy invocation that does not go through cargo — an editor's check-on-save wired
  straight to `clippy-driver`, a pre-commit hook, a one-off
  `clippy-driver src/deflate/slow.rs` — has no MSRV at all. The `clippy.toml` key is
  the only MSRV declaration that survives a cargo-less invocation, which is why it
  exists.

Edition bumps are a separate multi-file fact: [`Cargo.toml`](Cargo.toml)'s `edition`
and [`rustfmt.toml`](rustfmt.toml)'s `edition` / `style_edition` must move together.

## The blocking quality gates

**Plan-adopted standard S10 — Quality gates stay green and blocking.** All seven
gates below currently **exit 0** on this tree, and they must continue to. *No warning
is downgraded, no lint is `allow`-ed to make a change land, and no test is
`#[ignore]`d — the current count of ignored tests is zero and stays zero.*

Copy-pasteable, in the form CI runs them:

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

The last two are CI's `docs` job. Rustdoc warnings are **denied**, so a broken
intra-doc link fails the build rather than printing a note, and the published
MkDocs site is built with `--strict`, which turns a missing nav page or dangling
internal link into an error. CI pins that environment down to the artifact: CPython
`3.12.13`, and every package in the closure (`mkdocs 1.6.1`,
`mkdocs-techdocs-core 1.7.0`, `mkdocs-mermaid2-plugin 1.2.3` and their 43 transitive
dependencies) hash-pinned in the `REQS` heredoc inside the `docs` job of
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) and installed under
`--require-hashes`; copy that block into a file and install from it locally if you
want the identical verdict.

Three flags on those lines are load-bearing, not decorative:

- **`--all` on `cargo fmt`.** It selects **every package in the workspace** rather
  than only the one Cargo would pick by default. It is the form CI runs, and it keeps
  the gate honest if the workspace ever gains a member. Note that it deliberately does
  *not* reach [`fuzz/`](fuzz), which is a detached workspace with its own
  `[workspace]` table; running `cargo fmt` from inside `fuzz/` finds the same
  [`rustfmt.toml`](rustfmt.toml) by walking up. That workspace is not unchecked,
  though — [`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) gates it with
  its own nightly `fmt --check` and Clippy runs against
  `--manifest-path fuzz/Cargo.toml`; see [Fuzzing](#fuzzing).
- **`--all-features` on Clippy.** Without it Clippy never sees
  [`tests/c_oracle.rs`](tests/c_oracle.rs) (which carries `required-features`), the
  `inflate_strict` arms, or the `no_std` runtime block — precisely where a lint
  regression would hide longest, since no other job promotes warnings to errors.
- **`--locked`.** Both lockfiles are committed deliberately and must not be
  rewritten as a side effect of running a gate. It appears on five of the six lines
  and is **absent from `cargo fmt` on purpose**: `cargo-fmt` is a rustfmt wrapper, not
  a build command, and rejects the flag outright —
  `error: unexpected argument '--locked' found`. Do not add it there.

### Expected results

If your change did not touch behaviour, you should see exactly these numbers. A
different total is information, not noise — find out why before you push.

| Command | Expected result |
|---------|-----------------|
| `cargo test --locked` | **959 passed / 0 failed / 0 ignored** |
| `cargo test --locked --all-features` | **972 passed / 0 failed / 0 ignored** |
| `cargo test --locked --no-default-features` | **696 passed / 0 failed / 0 ignored** |
| `cargo test --locked --no-default-features --features no-std` | **696 passed / 0 failed / 0 ignored** |

The 959 decompose as **799** in-crate unit tests, **131** integration tests
(`checksum` 23, `gzip_compat` 17, `inflate_coverage` 29, `interop` 30, `regression`
13, `round_trip` 19), and **29** doctests — 28 runnable plus one `compile_fail`.
`--all-features` adds the **13** tests of the opt-in live C-oracle harness. Under
`--no-default-features` the total is **571** unit + **98** integration + **27**
doctests, and the `gzip_compat` suite correctly
reports 0 because the whole `gz*` file API is feature-gated off.

These are not merely expectations — CI enforces them. Each test row is captured
under `pipefail`, every `test result:` line is parsed, and the job fails if any row
reports a failure, reports an *ignored* test, or falls below a per-row lower bound.
`cargo test` exits 0 when tests are skipped, so a count check is the only thing that
notices a suite quietly shrinking.

**Preservation directive D-5: test coverage is preserved and only ever increased.**
*No test may be removed, weakened, or `#[ignore]`d to accommodate a change.* If a
capability cannot be exercised in a given build configuration, express that with a
feature gate or a run-time probe **that passes with a printed notice** — never with
`#[ignore]`. `tests/c_oracle.rs` is the worked example: `--all-features` enables it
even on a machine with no C compiler, so it probes at run time, prints a clearly
marked capability notice, and passes.

Four of those suites are ports of the official C drivers, and they are the reason
external constraint 4 is satisfied at all rather than merely claimed. Each one names
its own oracle in its module documentation header, so the mapping is checkable in the
source rather than only here:

| Rust suite | C oracle | What it carries over |
|------------|----------|----------------------|
| [`tests/regression.rs`](tests/regression.rs) | `test/example.c` | The reference exerciser's ten helper functions plus `main`'s version guard, as independent `#[test]`s — the **fixed-vector** half |
| [`tests/round_trip.rs`](tests/round_trip.rs) | `test/example.c` | The same conformance story generalised to properties over arbitrary inputs — the **randomised** half |
| [`tests/inflate_coverage.rs`](tests/inflate_coverage.rs) | `test/infcover.c` | The exhaustive malformed-stream decoder table, driving every inflate mode and error branch |
| [`tests/gzip_compat.rs`](tests/gzip_compat.rs) | `test/minigzip.c` | The library-exercising behaviour of the reference gzip client, through real files on disk |
| [`tests/checksum.rs`](tests/checksum.rs) | `adler32.c`, `crc32.c` | Known-answer vectors plus `*_combine` parity |
| [`tests/interop.rs`](tests/interop.rs) | reference C encoder | The two-tier byte-identity and wire-format gate described [below](#the-two-tier-interop-gate-and-why-the-tiers-must-not-be-conflated) |

These are load-bearing. Weakening one does not just lower a coverage number, it
withdraws the evidence for a constraint.

### Supply-chain gates

**Plan-adopted standard S6 — Supply-chain hygiene with concrete pinned versions.**
The governed closure is **102 packages**: 89 pinned by the root
[`Cargo.lock`](Cargo.lock) and 13 by [`fuzz/Cargo.lock`](fuzz/Cargo.lock), all from
crates.io except the fuzz workspace's single `path = ".."` self-reference.

```sh
# Root graph, against deny.toml (migration artifact D1). `--config` is explicit
# so the policy in force is never in doubt.
cargo deny --locked --config deny.toml check \
  -A unused-wrapper -A license-exception-not-encountered

# Detached fuzz workspace, against the SAME deny.toml. `--config` is MANDATORY
# here, not stylistic: cargo-deny resolves configuration from the target manifest's
# workspace root, and `fuzz/` holds no policy of its own -- so without the flag it
# finds nothing, falls back to built-in defaults, and reports `ok` over 13
# unreviewed packages. The three -A codes cover five entries that describe
# root-graph-only crates and so cannot match here; each stays at FULL severity on
# the root invocation above, where those entries are load-bearing.
cargo deny --locked --manifest-path fuzz/Cargo.toml --config deny.toml check \
  -A license-not-encountered -A unmatched-skip -A unnecessary-skip

# RustSec advisories over both lockfiles.
cargo audit --deny warnings
cargo audit --deny warnings --file fuzz/Cargo.lock
```

Two spellings above are deliberate rather than oversights, and both are worth knowing
before you "fix" them. `cargo audit` has **no** `--locked` flag — it reads a lockfile
directly, selected with `-f`/`--file`, so there is nothing for `--locked` to pin.
And neither command needs a `RUSTUP_TOOLCHAIN` override: both are standalone binaries
in `$CARGO_HOME/bin` that cargo dispatches to, so the repository's compiler pin does
not reach them. The pin matters only when a command **compiles** something — which is
why installing these tools does need `+stable` (above) while running them does not.

[`.github/workflows/audit.yml`](.github/workflows/audit.yml) — migration artifact
`D2` — runs all of those on push, pull request, a daily schedule, and manual
dispatch, in four jobs: `policy-integrity` (which asserts the single policy file has
not been hollowed out and that no second one has appeared), `cargo-audit`,
`cargo-deny`, and `cargo-deny-fuzz`. The one `deny` policy enforces `[advisories]`,
`[licenses]`, `[bans]`, and `[sources]` on **both** graphs, and its
`[bans]` list is where the project's scope boundaries are mechanically enforced:
`cc`, `bindgen`, `pkg-config`, `libz-sys`, `bzip2`, `lzma-sys`, `zstd`, and `brotli`
are all denied by name, with the reason recorded inline.

**Never silence a finding by widening a policy.** If `cargo deny` or `cargo audit`
objects to something your change introduced, the fix is in the change.

### Pinned tool configuration

Lint and format behaviour is pinned rather than inherited from whatever tool version
you happen to have installed — migration artifact `D11`, the pair of
[`clippy.toml`](clippy.toml) and [`rustfmt.toml`](rustfmt.toml). Both files carry
extensive rationale in their own comments; the operating rules are:

- **`clippy.toml` has exactly one key**, `msrv = "1.85.0"`. Do not add a threshold
  key to make a diagnostic go away — that is worse than a scoped `#[allow]`, because
  the finding becomes invisible and the suppression silently covers every future
  occurrence too. When Clippy is right, fix the code; when it is wrong about one
  specific site, put a narrowly scoped `#[expect(..)]` or `#[allow(..)]` **with a
  reason** at that site where a reviewer will see it. An unrecognised key is fatal:
  Clippy aborts rather than ignoring it, which breaks the gate for everyone.
- **`rustfmt.toml` pins only values the tree already satisfies.** It freezes the
  current style; it does not impose a new one. Because
  `cargo fmt --all -- --check` is blocking, a single non-default option would break
  the build for everybody until all 40 modules were rewritten. **Do not "improve" the
  style here.** The authoring procedure, if you ever must add a key, is: add one key,
  run the gate, and delete the key if the exit code is not 0.
- Neither file may be added to [`Cargo.toml`](Cargo.toml)'s `exclude` list. The lint
  and format contracts ship inside the published crate.

### The other CI jobs worth knowing about

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs **twelve** jobs — count
them with `grep -cE '^  [a-z0-9-]+:$' .github/workflows/ci.yml`. Beyond `build-test`,
`no-std-tests`, `lint`, `msrv`, and `benches`, **seven** more exist because each guards
something the seven gates above cannot see:

| Job | What it guards |
|-----|----------------|
| `docs` | Rustdoc with `RUSTDOCFLAGS: -D warnings` over `--all-features`, plus `mkdocs build --strict` in a version-pinned Python environment, so the published site and the API docs are gates rather than aspirations |
| `unsafe-boundary` | Three in-crate boundary tests **plus** a toolchain-independent shell scan, so the containment invariant holds even if the tests are weakened |
| `build-script-tests` | `build.rs` compiled as its own test binary — `cargo test` never compiles it — plus generated-table reproducibility and the big-endian (`s390x`) table selection |
| `c-abi-linkage` | The emitted `cdylib`/`staticlib` linked, dynamically linked, and `dlopen`'d by a real C consumer on four feature rows, with the exported set checked against `zlib.map` |
| `cross-targets` | Cross **type-check + Clippy only, nothing executed** for four triples: `aarch64`, 32-bit `i686`, big-endian `s390x`, and `x86_64-pc-windows-msvc` — the last being the *cross* Windows lane, distinct from `build-test`'s native `windows-latest` row, because neither `check` nor `clippy` links and so `--all-features` is reachable from Ubuntu (migration artifact `D3`) |
| `bare-metal-no-std` | Library build for `thumbv7em-none-eabihf`, the only configuration in which the freestanding runtime block compiles (migration artifact `D10`) |
| `package-verify` | `cargo package --list` against the `exclude` contract, then the packaging build, then the **unpacked archive's own `cargo test` run** (migration artifact `D12`) |

You can run the boundary gate locally:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib -- \
  tests::executable_unsafe_is_confined_to_the_designated_boundary \
  tests::unsafe_code_denial_has_exactly_two_scoped_carve_outs \
  tests::boundary_scanner_classifies_tokens_correctly
```


## ⚠ Byte-identity: the defining acceptance criterion

> **External constraint 1:** "Output must be binary-compatible with zlib-produced
> streams."

This project reads that constraint in its **strong** form, recorded as
**preservation directive D-1**: not merely that reference zlib can *decode* what
`zlib-rs` produces, but that the output **is the same bytes**. It is the strictest
available reading, it is the only one under which a drop-in replacement is genuinely
transparent to a caller that hashes, caches, or diffs compressed artifacts, and it is
the reading the in-tree tests already implement.

**Plan-adopted standard S3 — Bit-exactness is a release gate, not an aspiration.**

### The seven byte-identity-risk files

Any change touching one of these requires the byte-identity gate to be run **and its
output reported in the pull request**:

- [`src/deflate/state.rs`](src/deflate/state.rs)
- [`src/deflate/slow.rs`](src/deflate/slow.rs)
- [`src/deflate/fast.rs`](src/deflate/fast.rs)
- [`src/deflate/rle.rs`](src/deflate/rle.rs)
- [`src/deflate/stored.rs`](src/deflate/stored.rs)
- [`src/deflate/trees.rs`](src/deflate/trees.rs)
- [`src/deflate/strategy.rs`](src/deflate/strategy.rs)

Why these seven and not the decoder or the checksums? Because **compressed output is
a function of the match finder's decisions.** The DEFLATE format admits many valid
encodings of the same input; which one you emit is decided entirely by which match
the encoder chooses, and by how it costs out block types and breaks ties. The decoder
has no such freedom — a correct decoder produces the input, byte for byte, or it is
wrong.

### The eight decision points

These are the sites where "an obvious improvement" silently changes the output.
Each is a deliberate, verified transcription of the C original, and each must stay
that way:

1. **The hash function.** C's `UPDATE_HASH` is
   `h = ((h << hash_shift) ^ c) & hash_mask`, ported as an inherent method on the
   deflate state. Same operators, same order, same masking.
2. **The `hash_shift` derivation.** C computes an integer *ceiling* division,
   `(hash_bits + MIN_MATCH - 1) / MIN_MATCH`; Rust writes
   `hash_bits.div_ceil(MIN_MATCH)`. That is the idiomatic spelling of the same
   arithmetic and produces the same value for every input — it is not a
   simplification, and a plain `/` would not be equivalent.
3. **The chain insertion order in `insert_string`.** C's chained assignment
   `match_head = prev[str & w_mask] = head[ins_h]` is unrolled into separate
   statements, and the ordering is exact: **`head[]` is updated only *after* `prev[]`
   has been written.** Reorder those two lines and a *different* candidate becomes
   the first one `longest_match` examines, which changes the emitted token stream.
   This is the single most byte-identity-critical operation in the encoder.
4. **The four `longest_match` thresholds and its two early exits.** The thresholds
   are `max_chain_length`, `nice_match`, the **quartering** of the chain length when
   `prev_length >= good_match` — `chain_length >>= 2`, a shift of two, so the budget
   is divided by four and not by two (`deflate.c` L1424 →
   [`src/deflate/state.rs`](src/deflate/state.rs) L2349) — and the clamp of
   `nice_match` down to the available
   lookahead. The early exits are the two-byte prefilter (bail out unless
   `match[0..2] == scan[0..2]`) and the `len >= nice_match` break. All six are
   heuristics that trade compression ratio for speed, and all six are observable in
   the output.
5. **The lazy-match filter.** `deflate_slow` discards a match that is exactly
   `MIN_MATCH` long and reaches farther than `TOO_FAR`, where
   `TOO_FAR = 4096` (`src/deflate/state.rs`). The surrounding `prev_length`
   bookkeeping is preserved structurally too, including C's `prev_length -= 2`
   pre-decrement and the `max_insert = strstart + lookahead - MIN_MATCH` bound.
6. **Stored-versus-static-versus-dynamic block-type selection.** The costing
   formulas are transcribed exactly — `opt_lenb = (opt_len + 3 + 7) >> 3`,
   `static_lenb = (static_len + 3 + 7) >> 3`, the `Z_FIXED` branch, the
   `stored_len + 4 <= opt_lenb` forced-stored branch, and the `static_lenb ==
   opt_lenb` tie — with `wrapping_add` standing in for C's `ulg` wrap semantics.
7. **The Huffman tie-break: `depth[n] <= depth[m]`, not `<`.** C's `smaller` macro
   is `freq[n] < freq[m] || (freq[n] == freq[m] && depth[n] <= depth[m])`, and
   `src/deflate/trees.rs` reproduces it operator for operator. Change that `<=` to
   `<` and you get a **different but still perfectly valid** Huffman tree: the stream
   still round-trips, every decoder still accepts it, and byte-identity is broken.
   **A round-trip test cannot see this defect** — which is exactly why the baked
   oracle vectors exist.
8. **The ten-row `CONFIGURATION_TABLE`.** `good_length`, `max_lazy`, `nice_length`,
   and `max_chain` for each of levels 0–9, ported verbatim from the C
   `configuration_table`.

**Preservation directive D-3: `CONFIGURATION_TABLE` is verbatim by instruction.**
`src/deflate/strategy.rs` records it in its own header: *"altering any value would
make the compressed output diverge from reference zlib."* The `deflateTune` override
path must keep accepting caller-supplied replacements, and the defaults must keep
being those ten rows.

### Preservation directive D-2: constants must never be altered

These values are part of the ABI, the wire format, or both. Changing one changes
observable behaviour even when every line around it is correct, and
`src/constants.rs` carries the same instruction in its own header.

| Group | Values |
|-------|--------|
| Flush codes | `0..=6` |
| Return codes | `Z_OK 0`, `Z_STREAM_END 1`, `Z_NEED_DICT 2`, `Z_ERRNO −1`, `Z_STREAM_ERROR −2`, `Z_DATA_ERROR −3`, `Z_MEM_ERROR −4`, `Z_BUF_ERROR −5`, `Z_VERSION_ERROR −6` |
| Levels | `−1` (default, resolves to 6), `0`, `1`, `9` |
| Strategies | `0..=4` |
| Data types | `0..=2` |
| Method | `Z_DEFLATED 8` |
| Match bounds | `MIN_MATCH 3`, `MAX_MATCH 258` |
| Framing | `PRESET_DICT 0x20` |
| Encoder heuristic | `TOO_FAR 4096` |
| Decoder table arena | `ENOUGH 1444` = `ENOUGH_LENS 852` + `ENOUGH_DISTS 592` |
| Adler-32 | `BASE 65521`, `NMAX 5552` |
| CRC-32 | reflected polynomial `0xEDB88320` |
| `DeflateStatus` discriminants | `42` / `57` / `69` / `73` / `91` / `103` / `113` / `666` |
| `InflateMode` discriminants | 32 variants starting at the C sentinel `Head = 16180` |
| `GzMode` discriminants | `None = 0`, `Append = 1`, `Read = 7247`, `Write = 31153` |

The state discriminants are the least obvious entry in that table and the easiest to
"tidy". They are observable: `deflatePending`, `deflateSetHeader`, `inflateSync`, and
several error paths are state-dependent, so a caller that inspects behaviour at a
given point in a stream's life must observe the same behaviour as with C.
`DeflateStatus` is `#[repr(u16)]` for the sole reason that `Finish = 666` does not fit
in a `u8`.

### The two-tier interop gate, and why the tiers must not be conflated

[`tests/interop.rs`](tests/interop.rs) is deliberately bifurcated. Understanding the
split is what stops a well-meaning contributor from "consolidating" it.

**Tier 1 — strict byte-identity. Always on. The release gate.**

Comparison is against **deterministic oracle vectors baked from the genuine C
encoder** (via `deflateInit2` + `deflate(Z_FINISH)` + `deflateEnd`, method
`Z_DEFLATED`), spanning every compression level `-1..=9`, all five strategies, all
three `memLevel` values, and the zlib / raw / gzip / small-window framings. Because
the expected bytes are **precomputed constants**, this gate runs by default in CI
**with no C toolchain anywhere in sight**. That property is precious — protect it.

Three vector families make up **4,461 baked assertions**:

| Family | Rows | Encoding |
|--------|-----:|----------|
| `BI_VECTORS` + `BI_VECTORS_GZIP` | 225 + 75 = **300** | full literal hex — an exact-bytes proof |
| `BI_GRID` + `BI_GRID_GZIP` | 3,300 + 825 = **4,125** | `(length, CRC-32)` digest — what makes the wide grid affordable in source |
| `BI_EXTREMES` + `BI_EXTREMES_GZIP` | 24 + 12 = **36** | full literal hex, for the `memLevel` 1 / 9 corners |

Every expected value comes from the reference C encoder, **never from `zlib-rs`** —
regenerating a vector from this crate's own output would make the gate
self-referential and worthless. `tests/interop.rs` documents the exact regeneration
procedure, including why the destination buffer must be sized with
`compressBound(len) + 128` rather than `deflateBound`: `deflate_stored` consults
`avail_out`, so the wrong sizing bakes different level-0 bytes.

**Tier 2 — decode-compatibility. Always on. Not a byte-identity proof.**

Bidirectional interoperability against [`flate2`](https://crates.io/crates/flate2)
built with its **default pure-Rust `miniz_oxide` backend**, for every framing, level,
and strategy: `flate2` decodes what `zlib-rs` produced, and `zlib-rs` decodes what
`flate2` produced. Because `miniz_oxide` is a *different* encoder with different
match-finding heuristics, these tests prove RFC wire-format conformance but are
**explicitly not** treated as satisfying byte-identity — *that property is proven
exclusively by tier 1.* Do not "extend" tier 2 to assert equality of compressed
bytes; it would fail, and it would be asserting the wrong thing.

`flate2` and `miniz_oxide` are **dev-dependencies only**. They are independent decode
oracles and must never become runtime dependencies.

### Tier 3 — the opt-in live C-oracle sweep

[`tests/c_oracle.rs`](tests/c_oracle.rs) is the **live** form of the same sweep,
gated behind the `c-oracle` Cargo feature (migration artifact `D9`):

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --features c-oracle --test c_oracle -- --nocapture
```

It builds reference C zlib from the retained in-tree `*.c` sources by shelling out
through `std::process::Command`, then diffs live output across the full grid — **5
corpus shapes × 5 `windowBits` × 3 `memLevel`s × 10 levels × 5 strategies = 3,750
configurations**, `deflateBound` sizing included. On this tree it prints:

```text
c_oracle: smoke sweep 50/50 byte-identical against reference C zlib 1.3.2.1-motley
c_oracle: full grid 3750/3750 byte-identical against reference C zlib 1.3.2.1-motley
```

Two properties of this harness are load-bearing and must not be traded away:

- **It is strictly additive to tier 1**, never a replacement. Tier 1 stays the
  always-on release gate.
- **It introduces no build-dependency.** The `c-oracle` feature expands to `[]`, so
  `Cargo.lock` and `cargo metadata` are wholly unaffected by it. There is
  deliberately no `cc`, `bindgen`, or `pkg-config` dependency, no
  `[build-dependencies]` table, and no `links =` key anywhere in the manifest;
  `build.rs` is pure `std`. **A plain `cargo test` needs no C toolchain**, and that
  is not an accident to be optimised away.

Because `--all-features` is a row of the `build-test` matrix and `--all-features`
enables `c-oracle`, the harness probes for a usable C compiler at **run time** and,
finding none, prints a capability notice and **passes**. It must never fail for want
of a compiler, and it must never be `#[ignore]`d.


## ⚠ The `unsafe` boundary

> **External constraint 3:** "Zero unsafe blocks in core compression logic."

**Plan-adopted standard S2 — Unsafe containment by construction, not by convention**,
and **preservation directive D-6: the `unsafe` boundary is architectural.**

### Where `unsafe` may appear

`unsafe` is permitted in exactly two places:

1. **`src/ffi/**`** — the C ABI boundary. Reproducing the C ABI requires raw
   pointers, `extern "C"` entry points, and caller-supplied allocator hooks.
2. **The private `mod no_std_support` block in [`src/lib.rs`](src/lib.rs)** — a
   libc-backed global allocator over `malloc`/`calloc`/`realloc`/`free`/
   `posix_memalign`, an abort panic handler, and a `rust_eh_personality` definition,
   compiled only in a genuine non-test freestanding panic-abort build.

It is **forbidden** everywhere else, specifically in
[`src/deflate/**`](src/deflate), [`src/inflate/**`](src/inflate),
[`src/checksum/**`](src/checksum), [`src/gz/**`](src/gz),
[`src/util/**`](src/util), [`src/stream.rs`](src/stream.rs),
[`src/error.rs`](src/error.rs), [`src/constants.rs`](src/constants.rs), and
[`src/gz_header.rs`](src/gz_header.rs).

Measured on this tree, counting lines that carry an `unsafe` token once comments
have been removed. The method is worth stating exactly, because two defensible
readings of "excluding comments" disagree by two lines on `src/lib.rs`:

```sh
# The canonical count: drop whole-line `//` comments, THEN strip trailing
# `//` comments, then count the lines that still mention `unsafe`.
for f in src/ffi/*.rs src/lib.rs src/stream.rs; do
    printf '%6d  %s\n' \
      "$(grep -v '^[[:space:]]*//' "$f" | sed 's://.*::' | grep -cE '\bunsafe\b')" "$f"
done
cat src/ffi/*.rs | grep -v '^[[:space:]]*//' | sed 's://.*::' \
    | grep -cE '\bunsafe\b'                                              # 1497
cat src/{deflate,inflate,checksum,gz,util}/*.rs src/{error,constants,gz_header}.rs \
    | grep -v '^[[:space:]]*//' | sed 's://.*::' | grep -cE '\bunsafe\b'   # 0
```

Dropping only the whole-line comments and skipping the second step yields 49 for
[`src/lib.rs`](src/lib.rs) instead of 47. The two extra lines are not code: they
are the string literals `"// unsafe\n"` and `"//! unsafe\n"`, test fixtures that
feed the comment-stripping logic itself. The canonical form above is what
[`doc/technical-specifications.md` §0.6.2](doc/technical-specifications.md#062-unsafe-code-boundary)
reports, so the two documents report the same numbers.

| Location | Constructs | Nature |
|----------|-----------:|--------|
| `src/ffi/inflate.rs` | 478 | `extern "C"` entry points, pointer validation |
| `src/ffi/deflate.rs` | 333 | as above |
| `src/ffi/gz.rs` | 186 | gzip file API, C strings, descriptors |
| `src/ffi/util.rs` | 184 | one-call wrappers, checksums, version, compile flags |
| `src/ffi/types.rs` | 148 | ABI mirrors, hook aliases, handle tagging |
| `src/ffi/mod.rs` | 106 | wiring plus the ABI-drift guard |
| `src/ffi/alloc.rs` | 62 | the `zcalloc` / `zcfree` bridge |
| **`src/ffi/**` total** | **1,497** | the designated boundary |
| `src/lib.rs` | 47 | the freestanding runtime block described above (**22** of the 47, `mod no_std_support` at L207-L422) plus the **25** in the in-crate boundary tests that police it |
| `src/stream.rs` | 2 | **type aliases only** |
| All eight core module groups | **0** | — |

`src/stream.rs` is worth calling out because a naive `grep` finds two hits there.
Both are the type aliases `ZallocFn` and `ZfreeFn`, which merely *name* the C ABI
hook signatures the crate must interoperate with. `grep -c "unsafe {"` on that file
returns **0**: there is no executable `unsafe` block in it.

`src/lib.rs` is worth calling out for the opposite reason. A looser scan that drops
only *whole-line* comments reports **49** rather than 47, and the two extra lines are
neither code nor a discrepancy: they are the string-literal fixtures `"// unsafe\n"`
and `"//! unsafe\n"` inside the boundary test that checks the classifier ignores
comments. Use the canonical scan above — and note that the crate does not rely on
either count for enforcement. `#![deny(unsafe_code)]` and the in-crate boundary tests
do that; the table is evidence about shape, not the gate.

### How containment is enforced — four independent ways

It is not trusted, and it is not a review convention:

1. **`#![deny(unsafe_code)]` crate-wide** in `src/lib.rs`, with exactly **two**
   narrowly scoped `#[allow(unsafe_code)]` carve-outs — `pub mod ffi` and the private
   `mod no_std_support`. A stray `unsafe` block, `unsafe fn`, `unsafe impl`, or
   `unsafe extern` in a core module **fails the build outright**. `deny` rather than
   `forbid` is deliberate: `forbid` cannot be relaxed by an inner `allow`, which would
   make the two boundary carve-outs inexpressible.
2. **`#![warn(clippy::undocumented_unsafe_blocks)]`** alongside
   `#![warn(missing_docs)]`, both promoted to hard errors by the `-D warnings` lint
   gate. There are **513** `// SAFETY:` comments in `src/`, and every `unsafe` block in
   shipped code must carry one, immediately adjacent, where a reader will meet it.
3. **In-crate boundary tests** that re-derive the boundary from the source text —
   blanking comments and literals, classifying each `unsafe` token, and treating a
   bare `unsafe extern "C" fn(..)` *type* as declarative — then assert that executable
   `unsafe` appears only under `src/ffi/**` and inside `mod no_std_support`, and that
   exactly two carve-outs exist. `deny(unsafe_code)` alone cannot catch a smuggled
   *third* carve-out, because such code still compiles; these tests can.
4. **A toolchain-independent shell assertion** over the same invariants in CI's
   `unsafe-boundary` job, so the gate still bites if the tests themselves are
   weakened or deleted.

### When you need `unsafe` in a core module: restructure

That is the whole answer. The correct response to needing `unsafe` in the compression
or decompression core is to **change the design**, and two worked examples in the tree
show what that looks like:

- **Function-pointer dispatch became a tag enum.** C selects a block producer through
  a `compress_func` raw function-pointer table. `src/deflate/strategy.rs` replaces it
  with a `CompressFunc` **tag enum** carried in each row of `CONFIGURATION_TABLE`,
  which the driver resolves through an exhaustive `match`. Porting the pointer table
  verbatim *"would require `unsafe` and would forfeit the compiler's exhaustiveness
  checking"* — the tag enum keeps the whole compression core free of `unsafe` while
  preserving the original's exact selection behaviour.
- **Self-referential interior pointers became an offset plus a discriminant.** C's
  `state->next`, `lencode`, and `distcode` are interior pointers into
  `state->codes[]`, so a struct copy leaves them dangling and C's `inflateCopy` has to
  fix them up by hand. `src/inflate/state.rs` introduces
  `enum TableSource { Fixed, Dynamic }` plus integer offsets. That is precisely what
  makes a plain deep `Clone` **correct** for `inflateCopy` — a whole class of C
  undefined behaviour removed by construction rather than by convention.

If you genuinely believe a case requires `unsafe` in the core, open an issue and
describe the case before writing the code. It will not compile as written, and the
answer has always so far been a better data structure.

## ⚠ The C ABI is a compile-time-guarded contract

> **External constraint 2:** "FFI layer must match the zlib C API signature exactly."

**Plan-adopted standard S4 — The ABI is a compile-time-guarded contract**, and
**preservation directive D-4: public interfaces are preserved in full.**

A consumer replaces `libz` by *linking* against this crate. Everything about the
exported surface is therefore a compatibility obligation, not an implementation
detail.

### The symbol surface reconciles exactly

| Quantity | Value |
|----------|-------|
| `#[unsafe(no_mangle)]` attribute sites across the four shim modules | **98** (`ffi/deflate.rs` 17, `ffi/inflate.rs` 22, `ffi/util.rs` 25, `ffi/gz.rs` 34) |
| Distinct exported names they resolve to | **96** — two names (`gzdopen`, `inflateGetHeader`) have a `cfg`-paired pair of definitions |
| Platform-gated away on Linux | **1** — `gzopen_w`, `#[cfg(windows)]` in Rust, `#if defined(_WIN32) && !defined(Z_SOLO)` in `zlib.h` |
| `T` symbols in `libzlib_rs.so` | **95** — and nothing else; every exported symbol is a `T` |
| [`zlib.map`](zlib.map) `global:` names present | **54 / 54** |
| `zlib.map` `local:` names leaked | **0 / 10** |
| Emitted but undeclared | **none** |

96 declared − 1 platform-gated = **95 emitted**.
[`README.md`](README.md#exported-symbol-reconciliation) carries the full derivation,
including why 54 + 41 = 95 and why the ten `local:` names stay hidden.

### What you must not change

- **`#[repr(C)]` field order in [`src/ffi/types.rs`](src/ffi/types.rs) must match
  `zlib.h` exactly** — the **14-field** `z_stream` and the **13-field** `gz_header`
  (C's `struct z_stream_s` and `struct gz_header_s`), field for field, including
  padding behaviour.
- **The `gzFile_s { have, next, pos }` prefix.** C's `gzgetc` is a **macro** that
  dereferences those three fields directly at the call site. Reorder them and every C
  consumer that uses `gzgetc` reads garbage — with no compile error anywhere.
- **All five versioned init entry points must exist** — `deflateInit_`,
  `deflateInit2_`, `inflateInit_`, `inflateInit2_`, `inflateBackInit_` — because that
  is what the `zlib.h` macros expand to. The unsuffixed names a C programmer types
  are macros, not symbols.
- **`zlibVersion()` reports `"1.3.2.1-motley"` with `ZLIB_VERNUM 0x1321`.** The Cargo
  package version is deliberately **`1.3.2`**, because SemVer forbids the
  four-component motley string; the C API shim still reports the full upstream
  identity. Both are correct, and neither may be "unified" with the other.

### If you add or change an FFI entry point

[`src/ffi/mod.rs`](src/ffi/mod.rs) contains a `cfg(test)` guard that coerces each
exported function *item* to its exact `unsafe extern "C"` fn-pointer *type*, so any
drift in an argument, a return type, or a calling convention becomes a **compile
error** rather than something a C caller discovers at run time:

```rust
let _deflate: unsafe extern "C" fn(z_streamp, c_int) -> c_int = crate::ffi::deflate::deflate;
let _compress_bound: unsafe extern "C" fn(uLong) -> uLong = crate::ffi::util::compressBound;
```

That coverage is **exhaustive, not representative**: all **96** names are bound, a
strict superset of the 54 `zlib.map` globals, and a further test re-derives the export
list from the shim sources so that adding a symbol without adding its guard **fails
automatically**. Your obligations when touching this layer:

1. Add the fn-pointer coercion for any new symbol.
2. Record the change in [`CHANGELOG.md`](CHANGELOG.md) — the exported symbol surface
   is one of the four classes of change that file always records.
3. Do not remove a symbol. Even the two deliberately non-functional ones must keep
   being exported; see the next section.

## No silent behaviour change

**Plan-adopted standard S5 — No silent behaviour change.** *Structural improvement is
the objective; behavioural change is a defect.* The non-obvious cases are where this
actually gets decided:

- **Allocation count and failure timing.** A C program observes `Z_MEM_ERROR` at
  *specific points* in a stream's lifetime: some allocations happen at
  `inflateInit2_`, others are deferred until the window is first needed. A port that
  allocated everything up front would report `Z_MEM_ERROR` **earlier** than C under
  memory pressure, and one that deferred more would report it **later**. Either shift
  is an observable behaviour change even though not one compressed byte differs.
  `src/inflate/mod.rs` documents matching C's allocation count and failure timing
  explicitly; keep it that way.
- **Numeric error codes**, exactly as tabulated under
  [directive D-2](#preservation-directive-d-2-constants-must-never-be-altered).
- **State discriminants observable through state-dependent entry points** such as
  `deflatePending`.
- **`inflateEnd` and `deflateEnd` stay meaningful.** Ownership means the memory is
  already released, but the exported functions must still exist, still validate their
  argument, and still return the same code for the same input.
- **`inflateCopy` / `deflateCopy` must allocate the way the original did.** The copy
  is a deep `Clone` over the owned buffers, and those buffers may be
  caller-hook-backed — so the clone path must route through the same allocator hook,
  or a caller who supplied a custom arena would find the copy living in the global
  heap.
- **The allocator has-hook clause**, which is more subtle than it looks and is
  documented in full in [`src/stream.rs`](src/stream.rs). A hook is *active* only when
  **both** `zalloc` and `zfree` are present, because a region obtained from one must be
  released through the other. With neither half supplied the hook is inactive and the
  global allocator is used. With both present, every working buffer is carved from the
  caller's `zalloc` — and **an active `zalloc` that reports out-of-memory propagates as
  an allocation failure, with deliberately no global-allocator fallback**, which is what
  keeps a deliberately failing `zalloc` observable as `Z_MEM_ERROR` instead of silently
  bypassed. A pair with exactly one half supplied never becomes an active hook: C's
  three `*Init*_` prologues substitute the library's own built-in for the missing half
  first, and the FFI layer reproduces that substitution before constructing anything.

- **A shim may never invent a return code its C original cannot produce.** This is the
  rule that decides the awkward cases, so it is worth stating as a rule rather than
  leaving it to be re-derived. `deflateSetHeader` is the worked example: C's
  implementation stores the caller's `gz_header` **pointer** and can therefore only
  fail two ways — `Z_STREAM_ERROR` for a bad stream, wrong wrap mode, or bad state, or
  `Z_OK`. Its return set is exactly `{Z_OK, Z_STREAM_ERROR}` and contains no
  `Z_MEM_ERROR`. An earlier revision of the Rust shim deep-copied the header's
  `extra` / `name` / `comment` byte arrays into owned buffers, which added an
  allocation step C does not have — and an allocation step can fail. The shim now
  stores the caller's pointer exactly as C does (`deflate.c` L714-L719), copying
  nothing and allocating nothing, so the two-code return set holds structurally
  rather than by careful mapping. Had the copy stayed, its failure would have had to
  surface as `Z_STREAM_ERROR`, because a C caller switching on the documented return
  set would fall through an unexpected `Z_MEM_ERROR` into its generic-error path. An
  implementation detail the port added must never widen the contract the port
  inherited. A unit test pins the two-code set so the invariant is
  checked rather than remembered.

If a divergence is genuinely unavoidable, it is documented and — where the ABI can
carry the signal — advertised through `zlibCompileFlags`, never left implicit.

Two distinct classes exist, and conflating them is what makes a register go stale.
**Five divergences are visible to a C caller** and are enumerated in the next section;
they are frozen. A second, separate class — **internal conveniences that are strictly
invisible at the C ABI** — is enumerated in the section after it. A change that moves
an item from the second class into the first is a breaking change to the drop-in
contract and must be treated as one.

## Five divergences that must be preserved, not fixed

Each of these looks like an unfinished job. None is. Closing any one of them would
require a nightly compiler, break byte-identity, or bloat the published crate.

The same five, numbered in the same order, appear in
[`CHANGELOG.md`](CHANGELOG.md#the-five-divergences-a-c-caller-can-observe) and
[`SECURITY.md`](SECURITY.md#what-is-not-a-vulnerability).

1. **`gzprintf` / `gzvprintf` ship as ABI-compatible stubs returning
   `Z_STREAM_ERROR`.** Rendering a C `va_list` requires the nightly-only
   `c_variadic` language feature, which would break both the stable build and the
   MSRV contract. This is **not silent**: it is advertised through `zlibCompileFlags`
   **bit 27**, the bit C reserves for exactly this signal, so a caller can detect the
   limitation programmatically — which is precisely how a C zlib built without a
   secure `vsnprintf` behaves. The symbols must **not** be removed (that breaks
   linkage) and must **not** be made to appear functional. There is deliberately no
   `c-variadic` Cargo feature.
2. **`inflate_strict` defaults OFF.** Stricter inflate distance validation is
   available as an opt-in feature, but enabling it changes *which streams are
   accepted*, and therefore changes observable behaviour relative to a default-built
   reference zlib. Acceptance parity wins over stricter validation.
3. **The retained C baseline is excluded from the published crate** via
   [`Cargo.toml`](Cargo.toml)'s `exclude`. It is indispensable in-repository and dead
   weight in a `.crate`. Verification of that list belongs to migration artifact
   `D12`, and the `package-verify` CI job performs it.
4. **cdylib symbol *versioning* is not applied by default.** The symbol *set* is
   exactly right, but exported symbols carry no `@ZLIB_1.x` version tags the way a
   distribution `libz` does. `zlib.map` — 16 version nodes from `ZLIB_1.2.0` through
   `ZLIB_1.3.2` — is semantically authoritative for the exported/hidden partition, and
   an **opt-in** `build.rs` version-script wiring exists for GNU-ld targets
   (migration artifact `D8`). A drop-in replacement links successfully without it, and
   turning it on unconditionally carries linker-portability risk.
5. **`impl Drop for GzState` is intentionally empty of finishing logic**, so
   **`gzclose` / `gzclose_w` remain mandatory**. A destructor cannot surface a
   deferred compression or I/O error, and silently swallowing a failure to write a
   gzip member's final block and trailer during unwinding would be strictly worse than
   matching C's explicit-close contract. **Do not "improve" this into an
   auto-finishing destructor.** It is a documented, deliberate divergence from
   idiomatic Rust cleanup.

[`CHANGELOG.md`](CHANGELOG.md#known-limitations-and-documented-divergences) carries
the same list as release-facing text. If a divergence is ever added, removed, or
altered in scope, it must be recorded there.

**A zero-byte write acceptance is not on this list, and must not be added to it.**
C's two `gz_comp` write loops have exactly one success arm each —
`state->x.next += writ` and `strm->next_in += writ` (`gzwrite.c` L76-L90,
L112-L124) — so a `write(2)` returning `0` for a non-empty request advances no
cursor and the enclosing `while` re-issues the identical request. Both loops in
[`src/gz/write.rs`](src/gz/write.rs) reproduce that single-arm shape, which is why
`gzwrite`'s count and `gzerror`'s code match C's. Reporting the condition instead —
even as a *retryable* `Z_ERRNO` — is a behavioural change a C caller can observe,
so it belongs to neither class and was reverted. The retry is bounded for the same
reason C's is: POSIX permits a `0` return only for a zero-length request, and
neither loop ever issues one. Four unit tests pin the shape; if you find yourself
adding an `Ok(0)` arm to either loop, they will fail, and that is the intended
outcome.

## Internal divergences that are invisible at the C ABI

The five above are the whole list of divergences a C caller can *observe*. The port
also departs from C internally in the places below. None of them belongs in that list,
and the reason is uniform: each is either strictly stricter than C or strictly safer
than C, while leaving the return-code set, the struct layout, and the emitted bytes
untouched. They are recorded here so nobody has to guess whether an omission was an
oversight, and so that anyone who *changes* one can tell immediately whether they have
just promoted it into the observable list.

- **`deflateSetHeader` retains the caller's pointer, exactly as C does — this is
  no longer a divergence, and the entry is kept to record that.** C stores the
  caller's `gz_header` pointer and reads it only when the header is emitted, which
  makes the caller responsible for keeping that structure and its `extra` / `name` /
  `comment` buffers alive until then. An earlier revision of the shim deep-copied
  them at call time; that was reverted, because a snapshot ignores mutations the
  caller makes before emission — which changes the emitted gzip bytes and so breaks
  byte identity (AAP §0.8.1 D-1) — and because it introduced an allocation step in an
  entry point whose only documented outcomes are `Z_OK` and `Z_STREAM_ERROR`
  (`zlib.h` L854-L855). The shim now copies nothing and allocates nothing, so the
  lifetime contract, the return set and the wire bytes are all C's.
- **`HandleKind` / `HandleHeader` tag every opaque state allocation.** C's
  `z_stream.state` is an untyped pointer, so passing a deflate stream to `inflateEnd`
  reinterprets one struct as another — undefined behaviour that a C build cannot
  detect. The port stores a discriminant beside the state and rejects the mismatch with
  `Z_STREAM_ERROR`. C's own behaviour here is undefined rather than specified, so
  turning it into a defined error narrows undefined behaviour instead of changing
  defined behaviour.
- **Indexing is bounds-checked.** Where a C defect would read or write out of bounds
  and corrupt adjacent memory, the port panics. Since `panic = "abort"` is set in both
  profiles, that is an immediate abort rather than an unwind across the FFI boundary.
  This can only trigger on a path that is already a bug, so no correct program can
  observe it.
- **Allocation is fallible, with no global fallback.** Every working buffer is carved
  from the active allocator hook, and an active `zalloc` reporting out-of-memory
  propagates as an allocation failure rather than silently falling back to the global
  allocator. This is C's `ZALLOC` contract stated precisely, not a divergence from it —
  it is listed here only because a reader who knows the global allocator exists might
  reasonably expect a fallback that deliberately does not exist.


## Performance policy

**Performance is a constraint on this work, not its objective.** This is explicitly
not a performance refactor. No throughput target was ever set for it, and no
optimisation may be introduced at the cost of anything in
[byte-identity](#-byte-identity-the-defining-acceptance-criterion).

The recorded aggregate position against C zlib `1.3.2.1-motley` is **compression
≈ 85%** and **decompression 107–127%** of C throughput — so decompression is at or
above parity, and compression is the interesting side. Treat both as *attributed
context* rather than as properties this repository re-checks on demand: the Criterion
suite links no C library, and there is no in-tree **performance** oracle
(`tests/c_oracle.rs` is a *conformance* oracle for byte-identity, which is a different
job).

A per-profile comparison against a reference C build **inverted the intuitive reading
of the compression gap**, and it is worth knowing before you go hunting:

- **Incompressible input is the profile closest to C**, at roughly **82–86%**. The
  match finder fails *fast* there — `longest_match`'s two-byte prefilter rejects nearly
  every candidate before the comparison loop, and block-type selection then picks
  stored blocks because a dynamic tree cannot pay for itself — so both implementations
  do similar and rather little work per byte.
- **Compressible profiles are the furthest**, at roughly **58–64%**. That is where hash
  chains are genuinely walked, lazy matching is evaluated, and Huffman trees are built
  and emitted.
- **Decompression measured 104–125%** per profile, which *overlaps* the quoted 107–127% aggregate on 107–125% without containing it — parity holds throughout.

### The hard rule on optimisation

Any candidate compression speed-up **must clear the byte-identity gate before it is
considered viable**, because the very heuristics that cost throughput are the ones
that determine the output bytes: the chain-length **quartering** at `good_match`
(`chain_length >>= 2`), the `nice_match` early break, and the `TOO_FAR` lazy-match
filter.

> *A faster match finder that emits different tokens is a **regression**, not an
> improvement, no matter what the benchmark says.*

Permissible optimisation is limited to work that **provably cannot change the token
stream**: bounds-check elision, memory-access patterns, inlining, and buffer-copy
strategy. "Provably" means the byte-identity gate was run and reported, not that it
seemed safe.

### The benchmarks

Three Criterion harnesses, all driven by `cargo bench`:

```sh
RUSTUP_TOOLCHAIN=stable cargo bench --locked                     # all three
RUSTUP_TOOLCHAIN=stable cargo bench --locked --bench deflate_bench
RUSTUP_TOOLCHAIN=stable cargo bench --locked --no-run            # what CI checks
```

- [`benches/deflate_bench.rs`](benches/deflate_bench.rs) — all ten levels, with an
  explicit **incompressible-input guard** bracketed at levels 1, 6 and 9, so that
  the *slowest absolute* Rust
  workload is measured rather than assumed. Note what that profile is and is not. On
  this measuring host, `deflate_profiles` at level 6 reports roughly **35 MiB/s** for
  `incompressible` against **197 MiB/s** for `text` and **201 MiB/s** for
  `repetitive` — so it is the lowest bytes-per-second case by about 5.6×, and
  simultaneously the profile **closest** to C by ratio (~82–86%). It is *not* the
  C-relative worst case; that is the compressible profiles at ~58–64%. The harness
  links no C library, so it can produce the absolute column and never the ratio.
  Every case validates its own output before it is timed.
- [`benches/inflate_bench.rs`](benches/inflate_bench.rs) — pre-compress, then measure
  throughput over decompressed bytes.
- [`benches/checksum_bench.rs`](benches/checksum_bench.rs) — Adler-32 and CRC-32 across
  buffer sizes. It **prints the CRC-32 backend it actually measured**, which matters:
  the selected backend depends not only on the `simd` feature but on `crc32fast`'s own
  `std` feature, which a dev-dependency graph can silently unify on. To compare the two
  backends honestly, run the pair the README documents rather than trusting a single
  number.

The CI `benches` job compiles the harnesses (`cargo bench --no-run`) but does not time
them: a shared runner is not a measuring instrument. That folder measures; it does not
authorise.

## Working in this repository

### Branching and commits

- Work on a topic branch and open a pull request against the default branch.
- Keep each pull request to **one focused change**. A formatting sweep, a refactor, and
  a behaviour fix are three pull requests, not one — in a repository where byte-identity
  is the acceptance criterion, a mixed diff makes it impossible to tell which hunk moved
  a byte.
- Write commit messages that say *why*. The mechanical *what* is in the diff.

### Pull requests

**Plan-adopted standard S1 — Evidence over assertion.** Every claim about the system in
this repository's documentation carries a citation, and every behavioural claim is backed
by a command that was actually run. Hold your pull request to the same bar:

> *A change is not "done" because it compiles, it is done when the relevant gate has
> been observed to pass.*

Concretely, paste **the observed output of the gates you ran** into the pull request
description — the test totals, the exit codes, and, if you touched any of the seven
byte-identity-risk files, the byte-identity result. "Tests pass" is an assertion;
`959 passed / 0 failed / 0 ignored` is evidence.

### Import conventions

The module graph is the backbone of this port. Its layering is:

```text
error / constants  →  util  →  checksum  →  stream / gz_header  →  { deflate, inflate }  →  gz  →  ffi
```

That ordering is the architecture **and** an enforced invariant: in the shipped
library every `use crate::…` points at a strictly lower layer, with no upward and no
sideways edges. `deflate` and `inflate` are strict peers — neither names the other —
and two design decisions are what keep the graph one-way:

- `src/stream.rs` owns the engine state as an opaque `Box<dyn EngineState>`
  (`stream.rs:1599` declares the trait, `:1647` holds the boxed value) and so names
  neither `DeflateState` nor `InflateState`.
- The one-call façades are split rather than layered upward: the C driver loops stay
  at layer 3 behind the `OneCallDeflate` / `OneCallInflate` port traits
  (`src/util/compress.rs:145`, `src/util/uncompress.rs:67`), and the engine-owning
  entry points sit at layer 6 and implement them (`src/deflate/mod.rs:1770`,
  `src/inflate/mod.rs:2951`) — mirroring how `compress.c` and `uncompr.c` *include*
  `zlib.h` and drive the engine rather than being part of it.

```sh
# No upward `use` declaration exists in the shipped library. This prints nothing:
grep -nE '^\s*(pub )?use crate::(deflate|inflate)' \
     src/stream.rs src/util/compress.rs src/util/uncompress.rs
```

Keep the `^` anchor if you widen that grep. Dropping it adds matches from `//!`,
`///`, and `//` comments, which are intra-doc links rather than dependencies — the
same distinction the graph scan makes by blanking comments before it reads a `use`.

`the_module_graph_has_no_upward_edges` in `src/lib.rs` is the enforcer. It re-derives
the whole edge set from the source text of every file under `src/` — comments and
string literals blanked, brace-grouped `use` lists expanded — and scans twice: once
with `#[cfg(test)]` items removed, which is the library that is actually published
and must be *strictly* one-way, and once whole, so a test-only reference is checked
against `TEST_ONLY_CROSS_LAYER_EXCEPTIONS` rather than ignored. That list holds
exactly **three** entries, each with a recorded reason, and a fourth cannot land
unreviewed:

| Test-only exception | Why it exists |
|---------------------|---------------|
| `stream` → `ffi` | The has-hook contract can only be asserted against a real C `alloc_func`/`free_func` pair, and building one needs raw-pointer `unsafe`, which is permitted only under `src/ffi/**`. The counting hook therefore lives in `crate::ffi::alloc::test_hook` and is merely *driven* from the layer-5 tests. |
| `deflate` → `inflate` | Emitted-byte assertions decode with the crate's own inflate engine, which keeps the deflate unit tests free of `std` and of any third-party codec so they run in every feature configuration. |
| `inflate` → `deflate` | The mirror image: decoder tests need a producer, and using the crate's own encoder keeps them equally free of `std` and of third-party codecs. |

Those three are *test-only* references, not edges in the published library and not a
build or initialisation cycle: a Rust crate is one compilation unit, so intra-crate
module paths are resolved together and impose no ordering constraint — unlike C, where
the `#include` graph must be acyclic to compile. Four further guards keep the check
from passing vacuously: every `src/` file must classify into one of the ten declared
modules, the shipped graph must stay non-trivial (more than 40 inter-module edges),
`#[cfg(test)]` blanking must remove something but not everything, and every listed
exception must actually be exercised — so a stale exemption fails just as loudly as an
unlisted violation.
Note also that `unsafe` containment does not rest on the graph shape; it rests on
`#![deny(unsafe_code)]` and exactly two scoped carve-outs
(see [⚠ The `unsafe` boundary](#-the-unsafe-boundary)).

- **Prefer downward imports, and treat a new upward one as an architecture change.**
  Rust will not stop you: an upward `use` compiles silently, which is why the test
  exists. If you believe you need one, say so in the pull request and explain why the
  dependency cannot go the other way — and expect to restructure instead, as the
  `Box<dyn EngineState>` and port-trait decisions above both did. The clearest instance of deliberate cycle avoidance is
  `src/deflate/strategy.rs`, kept data-only with a single dependency on
  `crate::constants::Strategy` so that it reads as a layer below the block producers —
  all of which return a type it defines.
- **The C `#include "zutil.h"` idiom maps to the `crate::util` module.** Its header
  records the canonical form of that mapping, `use crate::{error::ZlibError, util::*};`,
  and the tree spells it out by name rather than by glob: `src/deflate/mod.rs` carries
  `use crate::util::OS_CODE;`, which an in-crate test in `src/lib.rs` asserts verbatim
  so that no module can quietly declare its own copy of the gzip OS byte. Outside
  `ffi`, only `deflate` and `inflate` reach into `util` at all — `deflate` for
  `OS_CODE` and the `OneCallDeflate` port, `inflate` for the `OneCallInflate` port — so
  import the items you use by name.
- **Integration tests, benches, and fuzz targets import only through the public
  surface**, for example:
  ```rust
  use zlib_rs::constants::{DEF_MEM_LEVEL, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH};
  use zlib_rs::deflate::{deflate, deflate_end, deflate_init2};
  use zlib_rs::{ReturnCode, Strategy, ZStream, compress2, compress_bound, uncompress};
  ```
- **FFI names are deliberately not re-exported at the crate root.** Anything that needs
  them imports from `zlib_rs::ffi`, so that glob-importing the crate root or the
  `prelude` can never pull raw-pointer entry points into scope.
- **`#[cfg(feature = "gzip")]` must gate every gzip-only import in a test**, or
  `--no-default-features` stops compiling.
- **No `use` of `flate2`, `quickcheck`, `rand`, or `criterion` may appear anywhere under
  `src/**`.** They are dev-only by contract, and `cargo deny`'s `[bans]` table plus the
  `no-std-tests` job are what make that stick.

### Module documentation convention

**Every module declares its C provenance in its own documentation header**, line by
line. `src/deflate/strategy.rs`, for instance, states that it is the safe-Rust
translation of the `block_state` enumeration, the `compress_func` typedef, and the
`config` struct plus `configuration_table`, each with the C line range it came from.

A new or substantially reworked module must do the same. Those citations are the
mechanism by which the port is audited against the oracle, which is also why
[`rustfmt.toml`](rustfmt.toml) forbids the comment-rewriting options: reflowing that
prose would corrupt exactly the citations a reviewer needs.

`#![warn(missing_docs)]` is promoted to an error by the lint gate, so every public item
needs a doc comment. Write it for someone holding `zlib.h` in the other hand.

### Documentation

**Plan-adopted standard S9 — Documentation must be internally consistent.** *A citation
that points at the wrong section is worse than no citation.* If you add or move a
cross-reference — an anchor link, a file path, a C line range — verify it still
resolves. Numbers in prose are held to the same standard as numbers in tests: if you
change something a documented figure measures, re-measure it and update the figure, and
never replace a measured value with an estimate.

The documentation gates make this enforceable rather than aspirational:
`RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features` turns a broken
intra-doc link into a build failure, and `mkdocs build --strict` does the same for a
missing nav page or a dangling link in the published site. Both run in CI's `docs` job.

### Platform honesty

**Plan-adopted standard S8 — Platform claims require platform coverage.** Do not claim
portability that CI has not exercised, and know where the line currently sits:

- **Natively executed:** `ubuntu-latest` across five feature rows — default,
  `--all-features`, `std,gzip,gz-io`, `std,simd`, and `--no-default-features`
  (**build-only**, since the `no-std-tests` job owns testing that configuration) — plus
  **`windows-latest` (x86_64)** and **`macos-latest` (aarch64)**, both running the full
  suite with default features, and each row asserting its own `rustc -vV` host triple
  and `runner.arch` so a mutable runner label cannot quietly change what was tested.
  The Windows row is the only place `OS_CODE = 10` and the `#[cfg(windows)]`-gated
  `gzopen_w` are ever compiled — and it **runs** them: a dedicated step executes
  `ffi::gz::tests::wide_path_open_round_trip` by name (UTF-16 path through `gzopen_w`,
  write, close, reopen, read back, `gzerror` state) and asserts exactly one test
  passed. The macOS row covers `OS_CODE = 19` and aarch64.
- **Cross type-checked and cross-linted, not natively run:** four triples —
  `aarch64-unknown-linux-gnu`, `i686-unknown-linux-gnu` (32-bit),
  `s390x-unknown-linux-gnu` (**big-endian**), and `x86_64-pc-windows-msvc` (32-bit
  `c_ulong`) — each with `cargo check --locked --all-targets --all-features` *and*
  `cargo clippy --locked --all-targets --all-features -- -D warnings`, so the
  `c_oracle` harness and the `inflate_strict` arms are type-checked rather than
  skipped and a target-conditional lint cannot hide. Read the Windows-MSVC entry as
  the *cross* lane, not a second count of the native `windows-latest` row above:
  that row is native and narrow (it **runs** the suite, with default features
  only), this one is cross and wide (every feature on, nothing executed), and a
  `cfg(windows)` diagnostic reachable only under `--all-features` had no gate at
  all before it existed. Neither `check` nor `clippy` links, which is why no MSVC
  linker or cross toolchain is needed to reach it from Ubuntu.
- **Built, not run:** the bare-metal `thumbv7em-none-eabihf` target, in both
  `--no-default-features` and `--features no-std` configurations.

So a 32-bit, big-endian, or bare-metal build is **compile-verified, not
runtime-verified**, and the documentation says so rather than rounding up. The two
mechanisms that make the distinction matter are concrete, and both resolve **at compile
time** from the target triple rather than probing at run time: `src/checksum/crc32.rs`
selects its braid tables with `cfg!(target_endian)` — an *expression* macro, so both arms
and both generated table sets stay compiled and type-checked while only the selected arm
reaches codegen — and `src/util/mod.rs` selects the gzip header's `OS_CODE` per platform
(**10** on Windows, **19** on non-Windows Apple, **3** otherwise). If your change touches
either, say which rows exercised it. The unit test
`both_endian_braids_match_byte_wise` in `src/checksum/crc32.rs` deliberately drives *both*
braid arms on whatever target runs the suite, so the arm your host does not take is still
covered.

### The feature matrix

The names and defaults in [`Cargo.toml`](Cargo.toml) are the authoritative contract, and
each feature maps onto a C preprocessor conditional so a consumer who knows how their C
zlib was configured can reproduce it:

| Feature | Default | Maps to | Notes |
|---------|:-------:|---------|-------|
| `std` | ✅ | presence of the C stdio / OS layer | forwards to `crc32fast?/std` through the **weak** `?` form |
| `gzip` | ✅ | `#ifdef GZIP` | gzip framing inside the engines |
| `gz-io` | ✅ | `#ifndef NO_GZCOMPRESS` | the `gz*` file API; implies `std` + `gzip` |
| `simd` | ✅ | *no C analogue* | SIMD CRC-32 via `crc32fast` |
| `no-std` | | `Z_SOLO` | core-only marker; the real switch is the `cfg_attr` in `src/lib.rs` |
| `inflate_strict` | | `INFLATE_STRICT` | **off by default** — see divergence 2 |
| `c-oracle` | | *no C analogue* | unlocks `tests/c_oracle.rs`; expands to `[]` |

Rules for touching this table: any change to a feature **name, default, or semantics**
must be recorded in [`CHANGELOG.md`](CHANGELOG.md), mirrored in
[`README.md`](README.md#feature-flags)'s table, and must keep
`cargo test --locked --no-default-features` green.

**`panic = "abort"` in both `[profile.release]` and `[profile.dev]` is required, not
stylistic.** A `--no-default-features` build emits the `cdylib`/`staticlib` without an
unwinding runtime, and on a stable toolchain the compiler then rejects unwinding panics
outright; `panic = "abort"` is the only stable mechanism that suppresses that
requirement, and Cargo cannot scope a panic strategy to one feature or crate-type. It is
also what makes the abort-style boundary guards in `src/ffi/**` correct under `no_std`,
since there is no unwinding to catch. Relatedly, `[profile.release]` deliberately carries
**no `lto` key** — rustc silently drops LTO for a unit that also emits an rlib, and a
declared optimisation that does nothing is worse than an honest absence. A test fails the
build if an `lto` key reappears.

### Fuzzing

Five `cargo-fuzz` / libFuzzer targets live in the **detached** [`fuzz/`](fuzz) workspace,
which has its own `[workspace]` table — so a root `cargo build`, `test`, `clippy`, or
`fmt` never touches it, and `cargo-fuzz` never enters the crate's dependency graph.
Run everything from the **repository root**; `cargo-fuzz` finds `fuzz/` by itself, and
the corpus and seed paths below are repository-relative:

```sh
cargo +nightly-2026-08-01 install cargo-fuzz --version 0.13.2 --locked
cargo +nightly-2026-08-01 fuzz build

# Pass the writable corpus FIRST and the committed seeds SECOND. libFuzzer treats
# the first corpus directory as writable and every later one as read-only input, so
# this ordering is a contract: reversing it would let a run rewrite tracked files.
mkdir -p fuzz/corpus/fuzz_inflate
cargo +nightly-2026-08-01 fuzz run fuzz_inflate fuzz/corpus/fuzz_inflate fuzz/seeds/fuzz_inflate \
  -- -max_total_time=120 -max_len=65536 -rss_limit_mb=2048
```

The explicit `+nightly-2026-08-01` on all three `cargo` lines is mandatory, not a
preference, and naming the *date* rather than the floating `nightly` channel is what makes
a local reproducer match CI: `cargo-fuzz` injects `-Z`
sanitizer and coverage flags, so on the repository's pinned 1.85.0 a bare `cargo fuzz
build` fails with `error: 1 nightly option were parsed` before any harness is produced.
`--locked` appears on the install line only — `cargo fuzz build` and `cargo fuzz run`
have no such flag (`cargo fuzz build --help` lists `-D`, `-O`, `-a`, `-v`,
`--no-default-features`, `--all-features`, `--features`, `-s`, and no lockfile option),
and the detached workspace's `fuzz/Cargo.lock` is committed and honoured regardless.

`fuzz/seeds/fuzz_inflate/` holds **11 committed seeds**, one per accepted compression
level, and they exist for a measurable reason: `fuzz_inflate`'s round-trip probe is
gated on a selector derived from the first eight input bytes, so an input either
always opens that gate or never does — each of the eleven opens it. Only
`fuzz_inflate` ships seeds today; for the other four targets, omit the second path.

The targets are `fuzz_inflate`, `fuzz_deflate_roundtrip`, `fuzz_gzip`, `fuzz_checksum`,
and `fuzz_ffi_roundtrip`. [`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml)
builds all five and runs each on pull requests and a weekly schedule
(`cron: '0 3 * * 1'`) with a per-target budget of **120 s on a pull request and 600 s
otherwise**. That workflow declares exactly one job and runs no `cargo-deny` of its own:
supply-chain policy over the fuzz graph is owned and enforced by `audit.yml`'s
`cargo-deny-fuzz` job, which aims the single reviewed [`deny.toml`](deny.toml) at it
on every push and pull request. Corpora are persisted between runs under per-target cache keys, the
committed seeds are supplied as read-only inputs on every run, and crash artifacts are
uploaded on failure. The fuzz crate builds with
`overflow-checks = true`, so an arithmetic overflow is a **finding**, not a wrap.

**That workflow is also the only quality gate the harnesses have.** Because `--all`
and `--all-targets` are workspace-scoped, the root `fmt` and Clippy gates cannot reach
a detached workspace, so `fuzz.yml` runs these four blocking steps before it builds or
fuzzes anything. Run the same four before touching a harness:

```sh
cargo +nightly-2026-08-01 fmt --manifest-path fuzz/Cargo.toml --all -- --check
cargo +nightly-2026-08-01 clippy --manifest-path fuzz/Cargo.toml --locked --all-targets -- -D warnings
cargo +nightly-2026-08-01 clippy --manifest-path fuzz/Cargo.toml --locked --all-targets \
  --no-default-features -- -D warnings
cargo +nightly-2026-08-01 build --manifest-path fuzz/Cargo.toml --locked --no-default-features
```

The `--no-default-features` pair is not a duplicate: `gzip` is a forwarding feature in
[`fuzz/Cargo.toml`](fuzz/Cargo.toml), and with it off each harness's
`#[cfg(feature = "gzip")]` legs compile out while every `fuzz_target!` — and therefore
every `main` — must still link. The build step asserts all five binaries are produced,
because a `fuzz_target!` that became conditional would otherwise silently stop
existing in a gzip-off build. `fuzz/Cargo.toml`'s `[lints.clippy]` table denies
`undocumented_unsafe_blocks` and `multiple_unsafe_ops_per_block`, and
`fuzz_ffi_roundtrip` is the one place a fuzzer crosses the `unsafe` C ABI boundary —
which is exactly why those lints need an enforcer rather than a declaration.

Campaign results — attributed, not re-measured here — record **1,674,289 executions
with 0 crashes and 0 crash artifacts** for a single sweep replicating this workflow's
exact invocation on the pinned `nightly-2026-08-01` at the pull-request budget of
120 s per target. That is a single-campaign total at a stated budget, not a cumulative
lifetime count: a fresh run is bounded by the budgets above, not by that total, and
the per-target breakdown lives in `doc/technical-specifications.md` §0.6.7.

**If a fuzz target finds a crash, treat it as a security report** and follow
[`SECURITY.md`](SECURITY.md) rather than attaching the reproducer to a public thread.

## Pre-submission checklist

Tick every line before you open the pull request.

- [ ] `RUSTUP_TOOLCHAIN=stable cargo fmt --all -- --check` — exit 0
- [ ] `RUSTUP_TOOLCHAIN=stable cargo clippy --locked --all-targets --all-features -- -D warnings` — exit 0
- [ ] `RUSTUP_TOOLCHAIN=stable cargo build --locked` — exit 0
- [ ] `RUSTUP_TOOLCHAIN=stable cargo test --locked` — **959 passed / 0 failed / 0
      ignored**, or higher with **zero** ignored
- [ ] `RUSTUP_TOOLCHAIN=stable cargo test --locked --no-default-features` —
      **696 passed**, same rule
- [ ] `RUSTUP_TOOLCHAIN=stable RUSTDOCFLAGS='-D warnings' cargo doc --locked
      --no-deps --all-features` — exit 0 with zero warnings
- [ ] `mkdocs build --strict --site-dir "$(mktemp -d)/site"` — exit 0 with **zero
      MkDocs strict diagnostics**: no `WARNING` and no `ERROR` from MkDocs, its
      plugins, or this project's content (CI's `docs` job runs both). Keep
      `--site-dir` pointed outside the checkout: `.gitignore` does not cover
      `site/`, so a bare build leaves 60 untracked files behind. The Material
      theme additionally prints one
      upstream advisory banner from its own maintainers about the forthcoming MkDocs
      2.0 — that is a vendor notice, not a build diagnostic; `--strict` does not fail
      on it and nothing in this repository can suppress it, so do not treat it as a
      regression
- [ ] `RUSTUP_TOOLCHAIN=1.85.0 cargo build --locked` and
      `RUSTUP_TOOLCHAIN=1.85.0 cargo check --locked --all-targets` — exit 0
      (MSRV is verified, not assumed)
- [ ] Both `cargo-deny` invocations clean, run exactly as
      [Supply-chain gates](#supply-chain-gates) prints them — **every** `-A` allowance
      included, because each covers entries that cannot match on the graph being
      checked and dropping one turns a shape artifact into a reported warning:
      `cargo deny --locked --config deny.toml check -A unused-wrapper -A license-exception-not-encountered`
      and
      `cargo deny --locked --manifest-path fuzz/Cargo.toml --config deny.toml check -A license-not-encountered -A unmatched-skip -A unnecessary-skip`
- [ ] `cargo audit --deny warnings` and
      `cargo audit --deny warnings --file fuzz/Cargo.lock` — both clean
- [ ] **No test removed, weakened, or `#[ignore]`d.** Ignored count is zero and stays zero
- [ ] If any of the [seven byte-identity-risk files](#the-seven-byte-identity-risk-files)
      was touched: the byte-identity gate was run **and its output is in the PR
      description**
- [ ] No new `unsafe` outside `src/ffi/**` and the `src/lib.rs` runtime block
- [ ] Every `unsafe` block carries an adjacent `// SAFETY:` justification
- [ ] No `*.c`, `*.h`, `test/*.c`, or `zlib.map` file modified
- [ ] [`CHANGELOG.md`](CHANGELOG.md) updated for any user-visible change — and always for
      a change to the MSRV, the feature set, the exported symbol surface, or a documented
      divergence
- [ ] Every documented figure your change invalidated has been **re-measured**, not
      estimated
- [ ] New or reworked modules carry a C-provenance documentation header

## Out of scope for contributions

Three exclusions are external to the project and are reproduced here verbatim:

> - "bzip2, lzma, or other compression formats"
> - "New compression algorithms"
> - "GUI or tooling beyond the library itself"

What follows from them:

- **The crate stays a library.** It emits `lib`, `cdylib`, and `staticlib` — the last two
  replacing CMake's `zlib SHARED` and `zlibstatic STATIC` targets. There is **no
  `[[bin]]` target, no CLI, and no GUI**, and none will be added.
- **No non-DEFLATE codecs.** No bzip2, lzma, zstd, or brotli support, and no novel
  algorithms. [`deny.toml`](deny.toml)'s `[bans]` table enforces this mechanically.
- **The runtime dependency closure stays exactly `cfg-if` plus optional `crc32fast`.** A
  memory-safety replacement for `libz` that dragged in a large transitive graph would
  trade one class of risk for another. `flate2` and `miniz_oxide` remain **dev-only
  oracles** and never become runtime dependencies, and no `cc`, `bindgen`, or
  `pkg-config` build-dependency may be introduced.
- **Behaviour changes are out of scope by default.** This is a structural and tech-stack
  migration; see [No silent behaviour change](#no-silent-behaviour-change).
- **This project has no UI, no component library, and no design system.** `zlib-rs` is a
  headless compression library with exactly two interfaces — the idiomatic Rust API and
  the C ABI — so no styling, markup, or design-token work applies.

Good first contributions, by contrast, look like: broadening malformed-stream coverage in
[`tests/inflate_coverage.rs`](tests/inflate_coverage.rs); adding a fuzz seed that reaches
a branch the corpus misses; sharpening a module's C-provenance documentation; or an
optimisation that provably cannot change the token stream and comes with the
byte-identity output to prove it.

## Reporting a vulnerability

**Do not open a public issue, discussion, or pull request for a suspected
vulnerability**, and do not attach a reproducing input to a public thread — that includes
findings that look probably-harmless, since whether an inflate defect is exploitable is
exactly the thing that is hard to judge from outside.

Use GitHub's private vulnerability reporting on this repository (**Security** tab →
**Report a vulnerability**). [`SECURITY.md`](SECURITY.md) has the full policy: what is in
scope, what to include, what happens next, and the fallback if private reporting is
unavailable to you.

## Conduct

Keep it straightforward: be respectful and assume good faith. Review comments address the
code, not the person. Disagreements are settled with evidence — a measurement, a citation
to the C oracle, or a failing test — which is both the fairest and the fastest way to
resolve a technical argument in a repository like this one. Maintainers may close or
moderate anything that makes the project a worse place to contribute.

## Where to look next

| Document | What it covers |
|----------|----------------|
| [`README.md`](README.md) | Overview, feature matrix, usage, the drop-in ABI, measured evidence, portability boundary |
| [`SECURITY.md`](SECURITY.md) | Vulnerability disclosure policy and the project's assurance posture |
| [`CHANGELOG.md`](CHANGELOG.md) | Release history for the Rust crate, plus the documented-divergence list |
| [`Cargo.toml`](Cargo.toml) | Crate identity, the feature contract, profiles, and the published-crate `exclude` list |
| [`rust-toolchain.toml`](rust-toolchain.toml) | The pinned toolchain (MSRV floor) |
| [`clippy.toml`](clippy.toml) / [`rustfmt.toml`](rustfmt.toml) | The lint and format contracts |
| [`deny.toml`](deny.toml) | The single supply-chain policy, governing the 89-package root graph and the 13-package detached fuzz graph |
| [`.github/workflows/ci.yml`](.github/workflows/ci.yml) | The twelve CI jobs, each documented in place |
| [`LICENSE`](LICENSE) | The zlib/libpng license |
| `doc/rfc1950.txt`, `doc/rfc1951.txt`, `doc/rfc1952.txt` | The normative wire-format specifications, retained in-tree |
| `zlib.h`, `deflate.c`, `trees.c`, `inflate.c`, … | The C oracle. Read freely; never edit |

A library that positions itself as a memory-safety replacement for a ubiquitous C
dependency has to earn that claim continuously. Thank you for helping it do so.
