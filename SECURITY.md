# Security Policy

`zlib-rs` is a memory-safe Rust reimplementation of [zlib](https://zlib.net/) that
presents the C `libz` ABI as a drop-in replacement. Memory safety *is* the value
proposition, and one of the most widely deployed C libraries in existence is what
it proposes to replace. That raises the bar for this document twice over: the
disclosure process has to be a real one, and the assurance claims have to be
things that were actually measured rather than things that sound reassuring.

So this policy states what has been verified, names the command that verified it,
and is equally explicit about the coverage that does **not** exist yet. A security
policy that overstates its assurances is worse than no policy at all.

Every figure below was observed on this tree with the command quoted beside it,
on stable `rustc 1.97.1 (8bab26f4f 2026-07-14, LLVM 22.1.6)`,
`x86_64-unknown-linux-gnu`, unless it is explicitly marked **attributed** — in
which case it is inherited from the migration's own records and is *not*
re-measured here.

---

## Scope

### What this policy covers

- The Rust crate **`zlib-rs`** — every module under [`src/`](src), including the C
  ABI boundary in [`src/ffi/`](src/ffi).
- The build script [`build.rs`](build.rs), which regenerates the CRC-32 lookup
  tables into `OUT_DIR` at build time.
- All three shipped artifacts: the Rust `lib` (rlib), the `cdylib`
  (`libzlib_rs.so` — the `libz` drop-in), and the `staticlib` (`libzlib_rs.a`).
- The crate's dependency closure and its supply-chain policy — see
  [Supply chain and dependencies](#supply-chain-and-dependencies).

### What this policy does not cover

- **The retained C sources** (`*.c`, `*.h`, `test/*.c`, [`zlib.map`](zlib.map)).
  They are kept in-tree deliberately, as the cross-validation oracle, the
  behavioural specification, and the source of the official test vectors, and they
  are **never modified**. They are also excluded from the published crate through
  the `exclude` list in [`Cargo.toml`](Cargo.toml), so **no C code is compiled
  into any shipped artifact** and their presence in the repository is not an
  exposure. A vulnerability in **upstream zlib itself** belongs to the
  [upstream project](https://github.com/madler/zlib), not here.

  One important carve-in: a report that **this crate fails to reproduce an
  upstream fix**, or diverges from upstream behaviour in a way that has security
  impact, is squarely in scope. The C tree being the oracle is exactly why such a
  divergence is a defect here.
- `contrib/**`, `examples/**`, and the legacy platform trees `amiga/`, `msdos/`,
  `os400/`, `qnx/`, `watcom/`, `win32/` — third-party bindings, C sample
  programs, and legacy build descriptors. None of them is part of this library or
  of the published crate.
- The C build and packaging descriptors (`CMakeLists.txt`, `Makefile.in`,
  `configure`, `BUILD.bazel`, and friends), retained for C consumers who integrate
  against the upstream layout.

### Version identity — please cite both

There are two version identities — the Cargo one and the C API one — and a report
is much easier to act on when it names both:

| Identity | Value | Source of truth |
|----------|-------|-----------------|
| Cargo package version | `1.3.2` | [`Cargo.toml`](Cargo.toml) `[package] version` |
| C API `zlibVersion()` | `"1.3.2.1-motley"` | [`src/util/version.rs`](src/util/version.rs) |
| C API `ZLIB_VERNUM` | `0x1321` | [`src/lib.rs`](src/lib.rs) |

They differ because Cargo requires SemVer and upstream zlib's four-component
`1.3.2.1-motley` string is not valid SemVer. The C shim still reports the full
upstream identity, verified live through the ABI:
`zlibVersion()="1.3.2.1-motley" ZLIB_VERNUM=0x1321`.

---

## Supported versions

| Version line | Status | Security fixes |
|--------------|--------|----------------|
| `1.3.x` (current, package version `1.3.2`) | **Experimental** — active migration | Yes |
| Anything earlier | Does not exist | n/a |

Two things are worth saying plainly rather than implying otherwise:

- **The declared lifecycle is `experimental`**, as recorded in
  [`catalog-info.yaml`](catalog-info.yaml). This is not a long-term-support
  posture, there is no back-port branch, and the API surface may still shift while
  parity work continues. Fixes land on the current line.
- **There is no prior release**, so there is no matrix of maintained older
  versions and no retroactive guarantee about unreleased history. See
  [`CHANGELOG.md`](CHANGELOG.md) for what the initial entry establishes.

**Toolchain support.** The crate declares `rust-version = "1.85.0"` (the release
that stabilised edition 2024) and pins it for local builds through
[`rust-toolchain.toml`](rust-toolchain.toml). Both the MSRV floor and current
stable are exercised: `cargo +1.85.0 build --locked` and
`cargo +1.85.0 check --locked --all-targets --all-features` both exit 0, and CI
runs them as a blocking `msrv` job — the `--all-features` making the floor a claim
about every feature the manifest declares, not just the default set. **A security fix will not silently raise the MSRV** — an MSRV
change is a documented, `CHANGELOG.md`-recorded change like any other.

---

## Reporting a vulnerability

### Please do not open a public issue or pull request

Do not file a public issue, a discussion, or a PR for a suspected vulnerability,
and please do not attach a reproducing input to a public thread. That includes
"probably harmless" findings — whether an inflate defect is exploitable is
precisely the thing that is hard to judge from the outside.

### Preferred channel — GitHub private vulnerability reporting

Use GitHub's private vulnerability reporting on this repository,
[`Blitzy-Sandbox/blitzy-zlib`](https://github.com/Blitzy-Sandbox/blitzy-zlib):

> **Security** tab → **Report a vulnerability**

That opens a private advisory thread visible only to you and the maintainers, and
it is where a fix and a published advisory are coordinated from.

**If that form is not available to you** — private reporting is a per-repository
setting and this document cannot assert on your behalf that it is enabled — then
open an ordinary issue containing **no technical detail whatsoever**: say only
that you have a security report and how a maintainer can reach you privately, and
wait for a private thread to be opened before sending anything substantive. There
is deliberately **no email address or PGP key published here**, because publishing
a contact this project cannot commit to monitoring would be worse than pointing
you at the mechanism that genuinely exists.

### What to include

Compression bugs are configuration-sensitive, so a report that pins the
configuration down is dramatically faster to act on. Please include as much of
this as you have:

1. **Versions** — the crate version and the string `zlibVersion()` returns.
2. **Feature set** — which Cargo features were enabled. `std`, `gzip`, `gz-io`,
   and `simd` are on by default; `no-std`, `inflate_strict`, and `c-oracle` are
   opt-in (see [Feature flags](README.md#feature-flags)).
3. **How you consume the library** — the Rust API, the `cdylib`, or the
   `staticlib`; and if you are substituting it for a system `libz`, say so
   (`-lz`, `LD_PRELOAD`, or installed as `libz.so.1`).
4. **Target triple, endianness, and pointer width.** These are not boilerplate
   here: [`src/checksum/crc32.rs`](src/checksum/crc32.rs) selects its CRC braid
   tables with `cfg!(target_endian)`, and [`src/util/mod.rs`](src/util/mod.rs)
   selects the gzip header's `OS_CODE` per platform (10 on Windows, 19 on
   non-Windows Apple, 3 otherwise) — so a defect can be genuinely
   platform-specific. Both are decided **at compile time** from the triple you
   built for, not probed at run time, which is why the triple itself is the
   evidence we need rather than the machine you happened to run on.
5. **The four encoder axes**, when compression is involved: `windowBits` (raw
   `-8..-15`, zlib `8..15`, gzip `+16`, auto-detect `+32`), `level`, `strategy`,
   and `memLevel`.
6. **A minimal reproducing input**, as raw bytes or base64 rather than as prose —
   a description of a byte sequence is not a byte sequence. If it came out of a
   fuzzer, name the target (`fuzz_inflate`, `fuzz_deflate_roundtrip`, `fuzz_gzip`,
   `fuzz_checksum`, `fuzz_ffi_roundtrip`) and attach the artifact.
7. **Which surface reproduces it** — the Rust API, the C ABI, or both. A defect
   that only reproduces through the C ABI points at
   [`src/ffi/`](src/ffi); one that reproduces through safe Rust is more serious,
   because the core is `unsafe`-free by construction.
8. **Impact as you see it** — crash, hang, memory unsafety, incorrect output,
   information disclosure — and the call sequence that gets there.

### What happens next

These are **targets this project holds itself to, on a best-effort basis. They are
not a service-level agreement**, and there is no funded on-call rotation behind
them:

| Step | Best-effort target |
|------|--------------------|
| Acknowledge the report | within 5 business days |
| Initial triage and a severity assessment | within 10 business days |
| Fix, or a written explanation of why it is not a vulnerability | tracked in the advisory thread until closed |

- **Coordinated disclosure is preferred.** Please give the fix a chance to land
  before publishing. If you have a disclosure deadline, say so in your first
  message and it will be worked to rather than argued with.
- **You will be credited** in the advisory and in [`CHANGELOG.md`](CHANGELOG.md)
  unless you ask not to be.
- **Every fix gets two records**: a `Security` entry in
  [`CHANGELOG.md`](CHANGELOG.md), and — where the impact warrants it — a published
  GitHub Security Advisory, plus a RustSec advisory request so that
  `cargo audit` users are told.
- If a report turns out to describe one of the
  [documented divergences](#what-is-not-a-vulnerability), you will get that
  explanation with a pointer to where the behaviour is specified, not a silent
  close.

---

## What counts as a vulnerability here

This project's four binding constraints are that compressed output must be
binary-compatible with zlib-produced streams, that the FFI layer must match the
zlib C API signatures exactly, that the compression core must contain zero
`unsafe`, and that the official zlib test vectors must pass. The classes below are
what a violation of those constraints looks like in practice.

### 1. Memory unsafety reachable without writing `unsafe` yourself

Out-of-bounds reads or writes, use-after-free, double free, reads of
uninitialised memory, or a data race — reached either from safe Rust or from a
**correct** C ABI call sequence.

Worth being precise about where such a bug can live: the compression and
decompression core measures **zero** executable `unsafe` and a stray block there
would not compile (see [Assurance posture](#assurance-posture--what-has-actually-been-verified)).
So a genuine memory-safety finding is almost certainly either in
[`src/ffi/`](src/ffi) — the boundary that unavoidably handles raw pointers — or a
soundness bug in the interface those modules present to safe code. Both are in
scope, and both are the highest-severity class here.

### 2. Soundness bugs at the C ABI boundary

Mishandled null or misaligned pointers, unvalidated caller-supplied lengths,
incorrect assumptions about caller buffer aliasing, or an `End`/terminator call on
a handle belonging to a different engine.

That last case is guarded *by construction*: every boxed FFI engine handle carries
a `#[repr(transparent)]` `HandleKind` discriminant as its first field, read and
validated through a `HandleHeader` prefix **before** the handle is ever
reconstituted as a `Box`
([`src/ffi/types.rs`](src/ffi/types.rs)) — because reconstituting a box whose
`Layout` does not match the original allocation is undefined behaviour even when
it appears to work. A way to bypass that tag, or any other route to a
layout-mismatched free, is a reportable defect.

### 3. Panics reachable through the C ABI

Both `[profile.release]` and `[profile.dev]` set `panic = "abort"`. This is
required rather than stylistic — a stable-toolchain `no_std` `cdylib`/`staticlib`
cannot link an unwinding runtime — and it is also what upholds the invariant that
a Rust panic never unwinds across the C ABI, which would be undefined behaviour.

The consequence for a C consumer is direct: **a reachable panic aborts the host
process**, which is a denial of service. Reachable-panic reports are therefore in
scope. The boundary already converts the foreseeable cases into ordinary `Z_*`
error codes through the eight-strong `guard_int` / `guard_ulong` / `guard_ptr` /
`guard_off` / `guard_long` / `guard_size` / `guard_const_ptr` / `guard_void`
family in [`src/ffi/types.rs`](src/ffi/types.rs), each defined twice so the `std`
and `no_std` builds guard identically; those guards exist to make
boundary failures deterministic, so a panic that escapes them is exactly the kind
of finding this section wants.

### 4. Resource exhaustion on adversarial input

Unbounded allocation, non-terminating loops, or superlinear behaviour driven by a
crafted stream — the decompression side especially, since it consumes untrusted
data by definition.

One bound deserves naming. The decoder's Huffman table arena is sized by
`ENOUGH = ENOUGH_LENS + ENOUGH_DISTS = 852 + 592 = 1444`
([`src/inflate/tables.rs`](src/inflate/tables.rs)); understating it would mean
table overflow on adversarial input. A demonstration that the arena bound can be
exceeded, or that table construction can be driven past its limits, is **high
severity**.

### 5. Stream-acceptance and correctness failures with security impact

- **Accepting a malformed stream that reference zlib rejects.** This is how a
  decoder becomes a parser-differential, and it is in scope.
- **Rejecting a stream reference zlib accepts** — a correctness and availability
  defect.
- **A checksum mismatch** in Adler-32 or CRC-32, or a `*_combine` result that
  disagrees with reference zlib.
- **Compressed output that differs from reference zlib**, byte for byte, for the
  same input and the same `(level, strategy, windowBits, memLevel)`.

That last item needs its framing stated honestly. Byte-identity with reference
zlib is *this project's defining acceptance criterion*, so a byte-level divergence
is always treated as a real defect — but it is usually a **functional** defect
rather than a vulnerability, since both outputs decode correctly. Report it
through the same channel; expect it to be triaged as a conformance bug unless
there is security impact.

### 6. Allocator-hook contract deviations

The C `zalloc`/`zfree` hook semantics are part of the observable contract, and two
clauses in particular must hold: caller-supplied buffers are used **only when both
`zalloc` and `zfree` are present** ([`src/stream.rs`](src/stream.rs)), and a null
allocator must propagate as an **allocation failure** rather than silently falling
back to the global allocator. A deviation — including a copy path such as
`deflateCopy`/`inflateCopy` that routes around a caller-supplied arena — is a
reportable behavioural defect, because a caller who supplied an arena for
isolation reasons would be silently losing that isolation.

### 7. Supply-chain issues

Anything in the governed dependency closure: a vulnerable, unmaintained, yanked,
or license-incompatible package, or a policy gap that let one through. See
[Supply chain and dependencies](#supply-chain-and-dependencies).

### Severity, roughly

| Severity | Typical finding |
|----------|-----------------|
| **Critical** | Memory unsafety reachable from safe Rust, or from a correct C call sequence, with a plausible path to code execution or disclosure |
| **High** | Any other memory-safety or soundness violation at the boundary; exceeding the `ENOUGH` table bound; accepting a malformed stream that reference zlib rejects |
| **Medium** | Reachable panic (host-process abort) through the C ABI; unbounded allocation or a hang on crafted input; an ABI signature or `#[repr(C)]` layout mismatch against `zlib.h` |
| **Low** | Allocator-hook contract deviation; checksum or acceptance divergence with no security impact; a dev-only supply-chain advisory |

Severity is assigned per report, with reasoning, in the advisory thread. If you
disagree with an assessment, say so there.

---

## What is not a vulnerability

The following are **deliberate, documented design decisions that are preserved on
purpose**, not defects awaiting a fix. They are listed here so nobody spends
effort rediscovering them.

### The five divergences a C caller can observe

The same five, numbered in the same order, appear in
[`CHANGELOG.md`](CHANGELOG.md#the-five-divergences-a-c-caller-can-observe) under
*Known limitations and documented divergences* and in
[`CONTRIBUTING.md`](CONTRIBUTING.md#five-divergences-that-must-be-preserved-not-fixed).
There is no sixth.

1. **`gzprintf` / `gzvprintf` return `Z_STREAM_ERROR`.** Rendering a C `va_list`
   needs the nightly-only `c_variadic` language feature, which would break the
   crate's stable build and its MSRV contract. Both symbols are still exported
   with the correct signatures — removing them would break linkage — and the
   limitation is **programmatically detectable rather than silent**:
   `zlibCompileFlags` sets **bit 27**, exactly as a C zlib built without a secure
   `vsnprintf` does ([`src/util/version.rs`](src/util/version.rs)). This ships the
   documented no-`vsnprintf` zlib build variant. The *idiomatic Rust* `gzprintf`,
   which takes `core::fmt::Arguments` instead of a `va_list`, formats fully.
2. **`inflate_strict` is off by default.** Enabling it changes which streams are
   accepted, so the default build deliberately matches a default-built reference
   zlib. It is therefore *not* a defect that this crate accepts a stream reference
   zlib also accepts, however lenient that pair of decisions looks in isolation. A
   divergence **from** reference zlib, in either direction, is — see class 5 above.
3. **The retained C sources are not compiled into the shipped artifact.** They are
   the oracle and the specification, they are excluded from the published crate,
   and their presence in the repository is not an exposure.
4. **Exported symbols carry no `@ZLIB_x.y.z` version tags by default.** The symbol
   *set* is exactly right — **95** emitted symbols, all of type `T`, with **54/54**
   [`zlib.map`](zlib.map) `global:` names present and **0/10** `local:` names
   leaked — and only the version *tags* are absent. Static linking, ordinary
   dynamic linking, `-lz` substitution, and `LD_PRELOAD` are all unaffected.
   Opting in with `ZLIB_RS_VERSION_SCRIPT=1` makes [`build.rs`](build.rs) derive a
   version script from `zlib.map` and apply it to the `cdylib`. It is off by
   default because a version script is a GNU-ld/ELF-only construct and no CI row
   sets the variable, so the opt-in path carries linker-portability risk the matrix
   does not yet retire. Full detail:
   [Symbol versioning](README.md#symbol-versioning).
5. **`gzclose` / `gzclose_w` are mandatory.** `GzState`'s `Drop`
   ([`src/gz/state.rs`](src/gz/state.rs)) releases every buffer but is
   intentionally empty of *finishing* logic, because a destructor cannot surface a
   deferred compression or I/O error — silently swallowing a failed write of a
   member's final block and trailer during unwinding would be strictly worse than
   matching C's explicit-close contract. Dropping a writer without closing it
   leaves an unfinished gzip member on disk. That is the documented contract, and
   it must not be "improved" into an auto-finishing destructor.

### General limitations, which are not compatibility divergences

- **Performance is not a security property here.** Measured against C, compression
  runs at **113–161%** on the compressible profiles and **82–94%** on incompressible
  input, while decompression is at or above parity throughout (`uncompress`
  **101–160%**, `inflateBack` **216–344%** on input that decompresses well). This
  supersedes an earlier "aggregate ≈ 85%, per-profile 58–64% compressible / 82–86%
  incompressible" reading, which was wrong in both magnitude and ordering. The one
  place the crate is slower is per-stream *initialisation* at level 1 with a
  non-default `memLevel` (70–74% at 64 KiB), which is owned-buffer zero-filling.
  A performance report is welcome as an ordinary issue. It is a
  vulnerability only if the slowdown is *input-triggered and superlinear*, which
  makes it class 4 above. Any proposed speed-up must clear the byte-identity gate
  first: the heuristics that cost throughput are the same ones that determine the
  output bytes, so a faster match finder that emits different tokens is a
  regression, not an improvement.
- **A `write(2)` that accepts zero bytes is retried in place**, because that is
  what C does: its two `gz_comp` write loops advance by `writ` and re-test, so a
  zero-byte acceptance re-issues the identical request (`gzwrite.c` L76-L90,
  L112-L124). This is not an unbounded spin, and it is not a divergence: POSIX
  permits a `0` return only for a zero-length request, and neither loop ever issues
  one — `avail_in`/`out_pending` bound every request below by one byte. A
  destination that could genuinely accept nothing forever reports `EAGAIN` or
  `EWOULDBLOCK`, which **is** handled as a retryable `Z_ERRNO` with the caller's
  cursor and buffered input preserved.
- **An accepted `inflateBackInit_` zero-fills the caller's window**, where C's
  adoption is a bare `state->window = window;` (`infback.c` L59) that writes nothing.
  This is a **hardening measure, not a behavioural change**, and it is listed here
  rather than among the five observable divergences above because no conforming
  caller can detect it. It exists because the decoder addresses the window through
  slices, and a `&[u8]` / `&mut [u8]` over abstract-uninitialized bytes is undefined
  behaviour *even when nothing reads it* — CWE-457 (use of uninitialized variable)
  and CWE-908 (use of uninitialized resource), tracked as **SEC-FFI-01**. Three
  properties bound it. It is the **last act of the accepting path**, so every
  refusing path — rejected argument, an allocator that turns C's single state
  request down, an exhausted Rust heap — and `inflateBackEnd`, which frees only the
  state (`infback.c` L572-L577), all leave the caller's buffer byte-for-byte
  untouched exactly as C leaves it. `inflateBack` treats the window purely as its
  **output** buffer (`put = state->window; left = state->wsize;`, `infback.c`
  L222-L223, with `state->whave = 0`), and zlib offers no way to seed `inflateBack`
  history, so the fill is unobservable on the accepting path. And it **adds no
  failure mode**, because it runs only once every fallible step has already
  succeeded. Reporting it as a vulnerability is therefore out of scope; reporting a
  path on which it *fails* to run before the first slice is formed is class 1 above.
- **Several out-of-contract calls are refused here that reference C accepts or
  crashes on, and that difference *is* observable at the ABI.** Measured with one C
  probe compiled twice — once against `target/release/libzlib_rs.a` and once
  against an archive built from the retained in-tree `*.c` sources, with each case
  run in a forked child so a crash in the reference build is reported as a signal
  instead of taking the harness down — `inflateBackEnd` on a `deflateInit_` or an
  `inflateInit_` handle returns `Z_STREAM_ERROR` here and `Z_OK` there, where C
  reinterprets one state struct as another and releases it through the wrong path;
  and `gzprintf(file, NULL)` returns `Z_STREAM_ERROR` here where the reference
  build raises **SIGSEGV**. The one-call wrappers agree in both libraries:
  `compress(NULL, ...)` and `uncompress(..., NULL, ...)` each return
  `Z_STREAM_ERROR`. None of this is a compatibility divergence, because every one
  of those calls requires usage `zlib.h` does not sanction — an untyped `state`
  handed to the wrong engine, or a null pointer where a format string is documented
  — and in each case a defined refusal replaces undefined behaviour, which
  narrows what a program can do rather than changing anything the header promises.
  This is why the register above is scoped to what a **conforming** caller can
  observe, and why the phrase "invisible at the C ABI" is deliberately not used for
  this class anywhere in the repository: it would overclaim. The reportable
  direction is the mirror image — a call that `zlib.h` *defines* and that this
  crate aborts on is class 3 above, and one it answers differently than reference C
  is class 5.

---

## Supply chain and dependencies

**The runtime closure is two crates.** `cfg-if 1.0.4`, plus the optional
`crc32fast 1.5.0` behind the `simd` feature (itself pure Rust, itself depending
only on `cfg-if`). Everything else in the lockfile — `criterion`, `flate2`,
`quickcheck`, `rand` — is **dev-only by contract** and never appears under
`src/`. That minimality is a security decision, not an aesthetic one: a
memory-safety replacement for `libz` that dragged in a large transitive graph
would trade one class of risk for another.

**No C toolchain is required to build or test the crate.** [`build.rs`](build.rs)
is pure `std` — no external crates, no `[build-dependencies]` table, no `links =`
key, and zero `unsafe`. `flate2` resolves to its default pure-Rust `miniz_oxide`
backend, so even the test graph needs no compiler. The only place a C-compiler
driver chain enters any dependency graph is the **detached** `fuzz/` workspace
(`cc`, `jobserver`, `shlex`, `find-msvc-tools`), which the root build never
touches; and the opt-in `c-oracle` test feature shells out to a system compiler
through `std::process::Command` rather than adding a build-dependency, so it too
leaves `Cargo.lock` untouched.

**The governed closure is 102 packages** — **89** pinned by
[`Cargo.lock`](Cargo.lock) and **13** by [`fuzz/Cargo.lock`](fuzz/Cargo.lock), all
from crates.io except the fuzz workspace's single `path = ".."` self-reference.
Both lockfiles are committed deliberately, because the crate ships
`cdylib`/`staticlib` distributables and reproducible offline builds need exact
resolved versions.

**The gate.** `cargo-deny` governs that closure through **one reviewed policy**:
[`deny.toml`](deny.toml), covering the 89 root packages and the 13 fuzz-only
packages alike. One file is deliberate — a second policy would be a second
rulebook, so a boundary would be stated twice, the two statements could drift, and
an auditor reading one graph's rules could be reading rules that do not govern the
other. `cargo-deny` resolves one graph per invocation, so there are **two commands
and one rulebook**, and each command names the file explicitly with `--config`
because `cargo-deny` otherwise resolves configuration from the *target* manifest's
workspace root and a discovery miss falls back to built-in defaults — a fallback
that looks exactly like a pass. The fuzz command is precisely that hazard, since
`fuzz/` is a detached workspace holding no policy of its own:

```sh
# Root graph: 89 packages.
cargo deny --locked --config deny.toml check \
  -A unused-wrapper -A license-exception-not-encountered

# Detached fuzz workspace: 13 packages, against the same policy.
cargo deny --locked --manifest-path fuzz/Cargo.toml --config deny.toml check \
  -A license-not-encountered -A unmatched-skip -A unnecessary-skip
```

Those are the invocations verbatim, `-A` allowances included, as
[`.github/workflows/audit.yml`](.github/workflows/audit.yml) runs them and as
[`CONTRIBUTING.md`](CONTRIBUTING.md) documents them. Reproducing the gate means
reproducing the flags. Measured on this tree, both invocations report
`0 errors, 0 warnings`.

The five allowances are the entire cost of one policy spanning two graphs, and each
is downgraded **only on the graph where the entry it covers cannot match**. On the
root invocation, `unused-wrapper` and `license-exception-not-encountered` name the
two entries the policy retains as latent defence in depth for `cc` and
`libfuzzer-sys`, which are fuzz-graph-only. On the fuzz invocation,
`license-not-encountered` names the root-only `Unicode-3.0` allowance, and
`unmatched-skip` / `unnecessary-skip` name the four duplicate-major pins, all of
which are root-graph crates. Every one of the five stays at **full** severity on the
other invocation, where the entry is load-bearing — so no code is waived on a graph
where it could report something real, and nothing is waived in both places at once.

The policy declares `[advisories]`, `[licenses]`, `[bans]`, and `[sources]`,
resolves with `all-features = true`, denies yanked crates, and bounds
advisory-database staleness at `maximum-db-staleness = "P7D"` **on the offline
path**. Read that bound narrowly: in cargo-deny 0.20.2 the duration is carried only
by the `Fetch::Disallow` variant, which is constructed solely for an `--offline`
run, so a fetching run never evaluates it. What keeps a *fetching* run honest is
that the fetch is fallible and unhandled — an unreachable database is a hard error,
so an audit that cannot obtain a current database produces no verdict rather than a
falsely clean one. Measured on this tree: online against a nonexistent `db-urls`
repository exits 1 with `failed to fetch advisory database`; `--offline` against a
clone backdated 30 days exits 1 with `repository is stale`; online against that same
backdated clone exits 0, because the fetch refreshes it before the bound could be
read. Seven days is therefore a tightening of cargo-deny's ninety-day *offline*
default, not the mechanism behind the daily online scan. It carries an
**empty `ignore` list** — no advisory is waived — leaves `[graph] targets` **empty**
so every crate is evaluated for every platform (naming triples *prunes* the graph:
measured, nine triples reduced coverage from 89 crates to 86, silently dropping the
`spirv`-only and `uefi`-only leaves from licence and ban review), and names `cc`,
`bindgen`, `pkg-config`, `libz-sys`, and the bzip2/lzma/zstd/brotli families in
`[bans] deny` so the zero-C-dependency and single-codec properties cannot erode by
accident. The two concessions the fuzz graph needs are **scoped rather than
relaxed**, which is exactly what lets one file govern both graphs: the `cc` ban
carries `wrappers = ["libfuzzer-sys"]`, so `cc` is admitted only as that crate's
build dependency and every *other* path to a C toolchain is still an error, and
`libfuzzer-sys`'s mandatory NCSA term is granted through a crate-scoped
`[[licenses.exceptions]]` entry rather than added to the global allow list. One
boundary, written once, correct on both graphs.
[`.github/workflows/audit.yml`](.github/workflows/audit.yml) is the single owner of
that gate. It runs `cargo-audit` over **both** lockfiles and evaluates **both
graphs against that one policy** — the root graph in its `cargo-deny` job and the
detached fuzz graph in its `cargo-deny-fuzz` job — on every push and pull request
and on a daily schedule (`cron: '0 5 * * *'`), and asserts that neither lockfile was
rewritten. A fourth job, `policy-integrity`, parses the policy and fails if it is
missing, if any load-bearing key has drifted from the value reviewed here, or if a
**second** policy file has appeared anywhere in the tree — because that would mean a
graph had quietly acquired its own rulebook.

[`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) deliberately declares **no**
`cargo-deny` job of its own, so fuzzing is not gated on a policy verdict inside its own
run. That is a real trade and is recorded rather than glossed: what replaces the gate is
breadth of coverage — `cargo-deny-fuzz` evaluates the policy on every push and pull
request, which is far more often than the weekly fuzzing schedule fires, so the fuzz
graph is checked strictly more, not less.

**The finding that motivated all of that.** `rand 0.9.4` is the direct
dev-dependency, pinned at or above the patched range for **RUSTSEC-2026-0097**.
But `rand 0.10.2` also sits in the graph, reached transitively through
`quickcheck 1.1.0` — so the mitigation expressed against the direct requirement
did not, on its own, govern the transitive line. Both lines are in fact above the
advisory's patched range, which is why the policy waives nothing; and because the
advisory reaches only a **development** dependency, it does **not** affect
consumers of the published crate — a dev-dependency is not part of a downstream
build graph. The gate exists precisely to catch this class of drift: a policy
written against a single assumed `rand` version would be wrong on contact.

**Duplicate majors are a hard error, with exactly four named exemptions.**
[`deny.toml`](deny.toml)'s `[bans]` table sets `multiple-versions = "deny"` — *not*
`warn` — together with `multiple-versions-include-dev = true`. Including the dev
graph is load-bearing: with that key unset, `multiple-versions = "deny"` reports
`bans ok` anyway, because every duplication in this project is dev-only. `warn`
would have been the weaker choice on the other axis: it accepts *every* duplicate,
including one introduced tomorrow by an unrelated dependency bump, and buries it in
output nobody re-reads.

Four duplications are legitimate, dev-dependency-only, and absent from all three
shipped artifacts (`lib`, `cdylib`, `staticlib` contain only `cfg-if` and optionally
`crc32fast`). They are exempted one at a time by exact-version `skip` entries:

| `skip` entry | Why |
|--------------|-----|
| `rand@0.10.2` | Dev-only; transitive via `quickcheck 1.1.0`, while `rand 0.9.4` is the direct dev-dependency. Both are above the RUSTSEC-2026-0097 patched range |
| `rand_core@0.10.1` | Dev-only; required by both `rand 0.10.2` and `getrandom 0.4.3`, each reached only through `quickcheck 1.1.0` |
| `getrandom@0.4.3` | Dev-only; required by `rand 0.10.2` on the same path. Note the direction: in the 0.10 chain `getrandom` follows *`rand`*, and it is `getrandom` that depends on `rand_core` — not the reverse |
| `r-efi@6.0.0` | Dev-only; the UEFI random backend of `getrandom 0.4.3`, while `r-efi 5.3.0` follows `getrandom 0.3.4`. Unreachable on every supported platform — `getrandom` gates it on the custom `cfg` `getrandom_backend = "efi_rng"` — and its LGPL term is an `OR`-disjunct already satisfied permissively |

Each entry names the **transitive** copy rather than the version this project
declares, on two counts: the directly-declared `rand 0.9.4` stays under normal
scrutiny, and because a `skip` matches an exact version, a future `quickcheck` bump
leaves the entry unmatched and `cargo-deny` reports `warning[unmatched-skip]` —
turning dependency drift into a visible prompt to re-review rather than a silent
widening of the exemption. `skip-tree` is deliberately empty, because it would
suppress an entire transitive subtree.

**`r-efi` is the fourth entry as a direct consequence of `[graph] targets = []`.**
With no triple filter, both `r-efi` 5.3.0 and 6.0.0 stay in the graph and are
correctly reported as a fourth duplicate major, so they must be waived by name like
the other three. An explicit triple list would instead have *pruned* them — and
pruning is the weaker outcome, because a package nobody evaluates is not a package
nobody ships. Empty `targets` is the deliberate choice: all 89 root packages are
governed, and the waiver is recorded in writing rather than hidden by a filter.

Measured on this tree, using the exact invocation CI runs:

```sh
cargo deny --locked --config deny.toml -L info check bans \
  -A unused-wrapper -A license-exception-not-encountered
# bans ok: 0 errors, 0 warnings, 5 notes
```

The notes are the acknowledged skips plus the `cc` wrapper scope; zero warnings and
zero errors is the expected steady state, and **any fifth duplicate major is a hard
error that fails the build** until somebody either removes it or approves it in
writing. The skip list is additionally asserted as an exact four-entry set by the
`policy-integrity` job in
[`.github/workflows/audit.yml`](.github/workflows/audit.yml), so silently adding a
fifth waiver fails CI just as loudly as the duplicate itself would.

---

## Assurance posture — what has actually been verified

### `unsafe` is contained as a compile error, not as a convention

[`src/lib.rs`](src/lib.rs) carries a crate-wide `#![deny(unsafe_code)]` with
**exactly two** narrowly scoped `#[allow(unsafe_code)]` carve-outs: `pub mod ffi`,
the C ABI surface, and a private `mod no_std_support` holding the libc-backed
`#[global_allocator]`, `#[panic_handler]`, and personality symbol that a
freestanding `cdylib`/`staticlib` must supply. `deny` rather than `forbid` is
deliberate — `forbid` cannot be relaxed by an inner `allow`, which would make
those two boundary carve-outs inexpressible.

Measured on this tree with a comment-excluded token scan. Both carve-outs are
listed, so the table accounts for every file in the crate that is permitted to
contain `unsafe` at all:

| Location | Unsafe-bearing code lines |
|----------|---------------------------|
| `src/ffi/` (carve-out 1 — the designated boundary) | **1,662** — `inflate.rs` 538, `deflate.rs` 373, `gz.rs` 212, `types.rs` 188, `util.rs` 184, `mod.rs` 113, `alloc.rs` 54 |
| `src/lib.rs` (carve-out 2 — the freestanding runtime block, plus the boundary tests that police it) | **51**, split **22 / 29**. The **22** sit inside the private `mod no_std_support` (L207–L422): the libc-backed `#[global_allocator]`, the `#[panic_handler]`, and the personality symbol. The other **29** are all inside `#[cfg(test)] mod tests` — the boundary scanner's own parsing logic, its assertion messages, and the deliberately adversarial corpus it is fed. **No executable `unsafe` exists anywhere else in the file**, and an in-crate test asserts precisely that rather than trusting it |
| `src/deflate/`, `src/inflate/`, `src/checksum/`, `src/gz/`, `src/util/`, `src/error.rs`, `src/constants.rs`, `src/gz_header.rs` | **0** |
| `src/stream.rs` | **2**, and both are `type` aliases only — `ZallocFn` and `ZfreeFn` merely *name* the C hook signatures the crate interoperates with. `grep -c "unsafe {"` on that file returns **0**, and the module carries its own `#![deny(unsafe_code)]` |

Reproduce every figure above with the same scan that produced it — one line per
file, whole-line comments discarded:

```bash
for f in src/ffi/*.rs src/lib.rs src/stream.rs; do
  printf '%-22s %s\n' "$f" \
    "$(grep -v -E '^[[:space:]]*(//|/\*|\*)' "$f" | grep -c '\bunsafe\b')"
done
```

One reconciliation is worth stating explicitly, because a slightly different
scan yields a slightly different number for `src/lib.rs` and neither is wrong.
A stricter variant that *also* discards text following a trailing `//` reports
**47** instead of 49. The two lines that drop out are the string literals
`"// unsafe\n"` and `"//! unsafe\n"` inside the boundary test's corpus, which
exist for the sole purpose of proving that the scanner ignores commented-out
`unsafe`. The table quotes the whole-line-comment variant throughout so that
`src/ffi/`, `src/lib.rs`, and `src/stream.rs` are all measured by one identical
method; a mixed methodology would make the rows incomparable.

Every `unsafe` block that does exist is justified in place: **592** `// SAFETY:`
comments across `src/`, with `#![warn(clippy::undocumented_unsafe_blocks)]` and
`#![warn(missing_docs)]` promoted to hard errors by the `-D warnings` lint gate.
Containment is checked four independent ways — the `deny` attribute, that lint
gate, in-crate boundary tests that re-derive the boundary from the source text and
assert that exactly two carve-outs exist, and a toolchain-independent shell
assertion in CI's `unsafe-boundary` job. The attribute alone cannot catch a
smuggled *third* carve-out, because such code still compiles; the tests can.
Mechanism detail: [How the `unsafe` boundary is enforced](README.md#how-the-unsafe-boundary-is-enforced).

### Ownership replaced every manual free

All **22** `ZALLOC`/`ZFREE` call sites in the C baseline — `deflate.c` 11,
`inflate.c` 9, `infback.c` 2 — are replaced by owned buffers behind a single
allocator abstraction, so there is no free path left to forget. C's
self-referential interior table pointers (`state->next`, `lencode`, `distcode`,
all pointing into `state->codes[]`) became an integer offset plus a
`TableSource { Fixed, Dynamic }` discriminant
([`src/inflate/state.rs`](src/inflate/state.rs)), which is what makes a deep
`Clone` sound for `inflateCopy` where a C `memcpy` of the struct would leave
dangling pointers. Whole classes of C defect — use-after-free, double free, buffer
overrun, unhandled state transition — are removed by construction rather than by
review.

### The ABI is a compile-time-checked contract

A `cfg(test)` guard in [`src/ffi/mod.rs`](src/ffi/mod.rs) coerces every exported
function *item* to its exact `unsafe extern "C"` fn-pointer *type*, binding all
**96** exported names — exhaustive, not a representative sample — so a change to an
argument, a return type, or a calling convention is a **compile error** rather
than something a C caller discovers at run time. The emitted surface reconciles
exactly: `nm -D --defined-only target/release/libzlib_rs.so` reports **95**
symbols, all of type `T` (96 declared names minus the `#[cfg(windows)]`-gated
`gzopen_w`), with 54/54 `zlib.map` globals present and 0/10 locals leaked. Full
derivation: [Exported symbol reconciliation](README.md#exported-symbol-reconciliation).

### Tests

| Command | Result |
|---------|--------|
| `cargo test --locked` | **1039 passed / 0 failed / 0 ignored** (867 unit, 143 integration, 29 doctests) |
| `cargo test --locked --all-features` | **1052 passed / 0 failed / 0 ignored** (adds the 13 live C-oracle tests) |
| `cargo test --locked --no-default-features` | **737 passed / 0 failed / 0 ignored** (600 unit, 110 integration, 27 doctests) |

The **ignored-test count is zero in every configuration and stays zero**. A
capability that cannot be exercised in a given build is expressed by a feature
gate or by a run-time probe that *passes with a printed notice*, never by
`#[ignore]`.

The suite includes Rust ports of all three official C drivers, which is how "must
pass the official zlib test vectors" is operationalised: `test/example.c` →
[`tests/regression.rs`](tests/regression.rs) (fixed vectors) and
[`tests/round_trip.rs`](tests/round_trip.rs) (the `quickcheck` randomised half);
`test/infcover.c` → [`tests/inflate_coverage.rs`](tests/inflate_coverage.rs), the
exhaustive malformed-stream decoder coverage table — the most security-relevant
suite in the tree; and `test/minigzip.c` →
[`tests/gzip_compat.rs`](tests/gzip_compat.rs). Checksum known-answer vectors live
in [`tests/checksum.rs`](tests/checksum.rs).

### Byte-identity against reference C zlib

- **Always-on, no C toolchain required.** Tier 1 of
  [`tests/interop.rs`](tests/interop.rs) carries **4,461 baked byte-identity
  assertions** derived from the genuine C encoder. Because the reference bytes are
  precomputed constants, this gate runs by default, everywhere.
- **Live, reproducible in-repository.** The opt-in
  [`tests/c_oracle.rs`](tests/c_oracle.rs) harness (`--features c-oracle`) builds
  reference C zlib from the 15 retained in-tree translation units and 11 headers
  and diffs live output. Observed on this tree: **50/50** on the smoke sweep
  (200,000-byte corpus) and **3,750/3,750** byte-identical on the full grid —
  5 corpus shapes × 5 `windowBits` × 3 `memLevel`s × 10 levels × 5 strategies —
  against a `libz_ref.a` built with the system `cc` (observed:
  `cc (Ubuntu 15.2.0-4ubuntu4) 15.2.0`). The same run re-confirmed the canonical
  vectors through the C ABI: `crc32("123456789") = 0xcbf43926`,
  `adler32("123456789") = 0x091e01de`, `compressBound(9) = 22`.

### Fuzzing

Five `cargo-fuzz` / libFuzzer targets — `fuzz_inflate`, `fuzz_deflate_roundtrip`,
`fuzz_gzip`, `fuzz_checksum`, `fuzz_ffi_roundtrip` — live in a detached `fuzz/`
workspace that the root build never pulls in.
[`.github/workflows/fuzz.yml`](.github/workflows/fuzz.yml) builds every target and
runs each on a weekly schedule (`cron: '0 3 * * 1'`) and on pull requests, with a
per-target budget of **120 s on a pull request and 600 s otherwise**, at
`-max_len=65536 -rss_limit_mb=2048`. Supply-chain policy over the fuzz graph is not
enforced by that workflow: the root `deny.toml` is aimed at that graph by
`audit.yml`'s `cargo-deny-fuzz` job on every push and pull request.
Each target's corpus is persisted between runs, and crash artifacts are uploaded
on failure. The fuzz crate builds with `overflow-checks = true`, so an arithmetic
overflow is a finding rather than a wrap.

Campaign results record **716,617 executions with 0 crashes and 0 crash
artifacts**, every target exiting 0, for a single sweep that replicated this
workflow's exact invocation on the pinned `nightly-2026-08-01` at a budget of
60 s per target (`fuzz_inflate` 400,054 · `fuzz_gzip` 124,945 ·
`fuzz_deflate_roundtrip` 116,425 · `fuzz_checksum` 40,505 · `fuzz_ffi_roundtrip`
34,688). That is a single-campaign total at a stated budget, not a cumulative
lifetime count, and it moves with the budget, the corpus and the host. What is
durable is the mechanism: the cached per-target corpus means coverage accumulates
across runs instead of restarting each week.
`doc/technical-specifications.md` §0.6.7 records the same figure with its full
invocation.

**This supersedes a previously published total, for a reason that matters to
anyone reading a fuzzing claim as a security assurance.** The earlier figure was
recorded while `fuzz_ffi_roundtrip` leaked 32 bytes per hook-backed engine
placement; under LeakSanitizer that target aborted deterministically at exit 77.
A memory leak is neither a "crash" nor a crash *artifact*, so "0 crashes and 0
crash artifacts" remained literally true of a run that was in fact failing — and
because the run loop used `set -e`, the abort at the third of five targets left
`fuzz_gzip` and `fuzz_inflate` with **zero** budget, so the published total never
represented five targets. The leak is fixed, the loop now budgets every target
regardless of an earlier failure, and a target-count guard fails the job on a
short campaign. Treat "0 crashes" as insufficient on its own: require that every
target exited 0 and that the target count is asserted.

### Honest limitations

Everything above describes coverage that exists; this section describes coverage
that does not. `.github/workflows/ci.yml` runs **fourteen jobs**, and the boundary
sits here:

- **Natively executed, full suite:** `ubuntu-latest` across five feature rows,
  plus **`windows-latest` (x86_64)** and **`macos-latest` (aarch64)**. The Windows
  row is the only place `OS_CODE = 10` and the `#[cfg(windows)]`-gated `gzopen_w`
  are compiled *and* run: it executes
  `ffi::gz::tests::wide_path_open_round_trip`, which opens a UTF-16 path through
  `gzopen_w`, writes, closes, reopens, reads the payload back, and asserts both the
  recovered bytes and the `gzerror` state — and a dedicated Windows-only step runs
  that test by name and asserts exactly one test passed, so the claim cannot decay
  into a compile-only check. The macOS row is the only place that reaches
  `OS_CODE = 19` and `O_NONBLOCK`'s BSD value. Every row additionally asserts its
  own `rustc -vV` host triple and `runner.arch`, so a mutable runner label that
  changes architecture underneath us fails the job instead of quietly invalidating
  this paragraph.
- **Cross type-checked and cross-linted:** four triples —
  `aarch64-unknown-linux-gnu`, `i686-unknown-linux-gnu` (32-bit `usize`),
  `s390x-unknown-linux-gnu` (**big-endian**), and `x86_64-pc-windows-msvc`
  (32-bit `c_ulong`) — are cross type-checked *and* cross-linted with
  `cargo check --locked --all-targets --all-features` and
  `cargo clippy --locked --all-targets --all-features -- -D warnings`, the
  `--all-features` spelling being what pulls the `c_oracle` harness and the
  `inflate_strict` arms into the check rather than skipping them. The
  Windows-MSVC entry here is the *cross* lane and does not double-count the
  native `windows-latest` row above: that row executes the suite on Windows with
  the default feature set, while this one reaches the whole `cfg(windows)`
  surface with every feature on and executes nothing. Neither subsumes the other,
  which is why both exist.
- **Executed under emulation, not on the hardware:** the `cross-run` job runs the
  suite on `aarch64-unknown-linux-gnu`, on 32-bit `i686-unknown-linux-gnu` and on
  **big-endian** `s390x-unknown-linux-gnu` through `qemu-user`, over the default row
  and both std-off rows, so the **big-endian CRC braid arms are run, not merely
  compiled** — on s390x `crc32fast` offers no accelerated backend, which makes the
  scalar braid the arm that serves every bulk call even on the SIMD-enabled row.
  The i686 row is what executes the 32-bit `usize` arms; the five layout tests
  gated on `target_pointer_width = "64"` correctly do not run there, which is why
  that row reports a smaller test count than the 64-bit rows rather than a failure.
  Each row asserts its declared endianness **and** pointer width against
  `rustc --print cfg`, and a dedicated step runs the four endian-critical CRC tests
  by name and asserts exactly four passed. This is **emulation, not IBM Z, Intel or
  ARM hardware**, and that distinction is deliberate: `qemu-user` reproduces the ISA
  and the byte order but not the machine. A unit test asserts that the
  endian-selected table anchors match the active target's values on every target, so
  the relationship is checked at run time wherever the suite runs at all.
- **Bare metal, executed under emulation:** `bare-metal-no-std` builds the
  `thumbv7em-none-eabihf` library in both `--no-default-features` and
  `--features no-std` configurations, with an `nm` assertion that the freestanding
  runtime block — the libc-backed allocator, the abort panic handler, the
  personality shim — was genuinely compiled and is crate-owned. `bare-metal-run`
  then links that staticlib into firmware and **executes it on a no-OS Cortex-M4
  under `qemu-system-arm`**, which is the only configuration in which that block
  can run at all: `cargo test` sets `test` and forces `panic = "unwind"`, failing
  two of the three terms in its own `cfg`, so no hosted test reaches it. The
  security-relevant assertion is the fallible one — the job drains the heap and
  requires `deflateInit2` to return `Z_MEM_ERROR` rather than abort, because on a
  device with no OOM killer an allocator that aborts under pressure is a
  denial-of-service primitive. **This is emulation, not real embedded hardware**:
  QEMU reproduces the ISA, the memory map and the absence of an OS, but not a
  device's timing, memory controller or peripheral behaviour, so validation on
  real silicon remains genuinely outstanding.
- **Human code review across the full Rust surface — 80,286 lines across 40 files
  under `src/`, measured on 2026-08-04 with
  `find src -name '*.rs' -print0 | xargs -0 wc -l` — is outstanding**, and it is
  the highest-severity remaining hardening item
  precisely because it cannot be automated away. Everything above is machine
  evidence; none of it substitutes for a reviewer.
- **No third-party security audit, penetration test, certification, or CVE
  history exists for this crate.** Nothing in this document should be read as
  claiming one.
- **The declared lifecycle is `experimental`.** Please weigh that before putting
  the artifact in front of untrusted input in production.

The same boundary is drawn, job by job, in
[Portability: what CI actually exercises](README.md#portability-what-ci-actually-exercises).

---

## Related documents

| Document | What it covers |
|----------|----------------|
| [`README.md`](README.md) | Overview, feature matrix, measured evidence, the drop-in ABI, and the portability boundary |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Contribution workflow, the blocking quality gates, and the MSRV policy |
| [`CHANGELOG.md`](CHANGELOG.md) | Release history for the Rust crate; `Security` entries record every fix |
| [`deny.toml`](deny.toml) | The single supply-chain policy, governing the 89-package root graph and the 13-package detached fuzz graph |
| [`Cargo.toml`](Cargo.toml) | Crate identity, the feature contract, profiles, and the published-crate `exclude` list |
| [`LICENSE`](LICENSE) | The zlib/libpng license, carried forward from upstream |

The separate upstream `ChangeLog` file (no extension) is the **C baseline's**
history and is retained unmodified; it is not this crate's release history.

Thank you for reporting responsibly. A library that positions itself as a
memory-safety replacement for a ubiquitous C dependency has to earn that claim
continuously, and a careful report is the most useful thing anyone outside the
project can contribute to it.
