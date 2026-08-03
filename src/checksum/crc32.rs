//! CRC-32/IEEE checksum computation.
//!
//! This module is a faithful, memory-safe Rust port of the reference zlib
//! `crc32.c`. It computes the CRC-32 with the reflected polynomial
//! `0xEDB88320` (the IEEE 802.3 / gzip CRC) that gzip-format streams
//! (RFC 1952) carry as their trailing 4-byte integrity field, and it is also
//! exposed through the public API because it is useful to applications on its
//! own.
//!
//! The implementation contains **no `unsafe`** code. Everything that computes a
//! checksum depends only on `core`, so it is usable in `no_std` builds; the one
//! `std` touch point is the CPU-capability probe described below, which is
//! compiled only when the crate's `std` feature is on. Its output is
//! bit-identical to reference zlib for every input, which is the defining
//! acceptance criterion for the migration.
//!
//! # Computation paths
//!
//! Two interchangeable bulk-CRC paths exist. Both produce identical output for
//! every input, so a caller never observes which one ran — only throughput
//! changes — and [`crc32_backend`] reports the choice:
//!
//! * **`crc32fast` ([`Crc32Backend::Crc32Fast`])** delegates the hot loop to the
//!   `crc32fast` crate, which uses hardware-accelerated carry-less
//!   multiplication (x86 `pclmulqdq`) or the dedicated ARM CRC instructions.
//!   `crc32fast` follows the same external CRC convention as zlib (pre- and
//!   post-conditioning performed internally), so it is a bit-exact drop-in for
//!   the byte-wise algorithm. It is selected only when the `simd` Cargo feature
//!   is enabled **and** its hardware path is genuinely reachable — see
//!   "Reachability" below.
//! * **The braided scalar path** ([`Crc32Backend::Braid`]) is the *braided*,
//!   word-at-a-time algorithm ported from the `#ifdef W` fast path in
//!   `crc32.c`: `CRC_BRAID_N` (5) independent CRCs are advanced over interleaved
//!   `CRC_BRAID_W`-byte (8-byte) words and combined at the end of the final
//!   block, which breaks the serial dependency of the byte-wise loop. Little-
//!   and big-endian variants are both compiled and selected by
//!   `cfg!(target_endian = ...)`, and the classic reflected byte-wise table loop
//!   over `CRC_TABLE` still handles short inputs and the trailing bytes. It
//!   needs no external crate and is the guaranteed-correct baseline; the
//!   equivalence of the braided and byte-wise results is asserted by tests over
//!   every length and every word offset. It backs every build with the `simd`
//!   feature off, and it is the fallback whenever the accelerated path is not
//!   reachable — including a `no_std` build with `simd` on whose target neither
//!   detects nor compiles in the required CPU features.
//!
//! ## Reachability: why the `simd` feature is not by itself the selector
//!
//! `crc32fast` decides internally whether it may execute its accelerated code,
//! and it decides differently depending on whether **its own** `std` feature is
//! on: with it, `State::new` performs a RUN-TIME `is_x86_feature_detected!` /
//! `is_aarch64_feature_detected!` probe; without it, a COMPILE-TIME
//! `cfg!(target_feature = ...)` test. On a stock `x86_64` target only `sse2` is
//! baseline, so the compile-time test fails, `State::new` returns `None`, and
//! `crc32fast` silently falls back to its own software table — which is slower
//! than the braided path in this module. Selecting `crc32fast` unconditionally
//! from the `simd` feature alone therefore made enabling `simd` a *pessimization*
//! for the shipped library, while `cargo test`/`cargo bench` hid the defect
//! because their dev-dependency graph unified `crc32fast/std` on.
//!
//! Two changes close that gap and are load-bearing together:
//!
//! 1. `Cargo.toml` forwards the crate's own `std` feature to `crc32fast?/std`,
//!    so a std build of this crate gives `crc32fast` its run-time probe (and a
//!    `no_std` build still links it without `std`).
//! 2. This module tests reachability itself, mirroring `crc32fast`'s own gate
//!    conditions, and keeps the braid whenever the accelerated path would not be
//!    selected — including on architectures for which `crc32fast` has no
//!    specialized backend at all. That is what stops `simd` from silently
//!    substituting `crc32fast`'s software table for this module's braid.
//!
//! Which backend a given build actually runs is reported by [`crc32_backend`].
//! That reachability test is a run-time CPU probe on x86, x86-64 and AArch64, and
//! the bulk dispatcher consults it on every call, so its answer — which is
//! immutable for the lifetime of the process — is resolved once and cached
//! thereafter rather than re-probed per call.
//! Relative throughput is a property of the target and the CPU, so compare the
//! two feature rows with `benches/checksum_bench.rs` on the machine that matters
//! rather than assuming an ordering.
//!
//! Because both paths are bit-exact, this selection cannot change a single
//! emitted byte; only the instruction mix does.
//!
//! # Combining checksums
//!
//! The `crc32_combine*` family lets the CRC of a concatenation be computed from
//! the CRCs of the parts plus the length of the second part. These routines
//! are ported directly from `crc32.c` (they are *not* delegated to
//! `crc32fast`) so their results match reference zlib exactly and behave
//! identically in `no_std`. They rely on GF(2) polynomial arithmetic
//! (`multmodp` / `x2nmodp`) over the precomputed powers-of-x table `X2N_TABLE`.

// The CRC-32 lookup tables are generated at build time by `build.rs` (a safe
// Rust port of the C `make_crc_table()` routine) and written to
// `${OUT_DIR}/crc32_tables.rs`; the values are bit-identical to the checked-in
// C header `crc32.h`.
//
// NO `dead_code` allowance is granted, in ANY configuration, and that is a
// deliberate strengthening rather than an omission. The braided path below is
// now compiled unconditionally — with `simd` on it is the fallback taken
// whenever `crc32fast`'s hardware backend is unreachable — so all SEVEN
// generated artifacts are consumed in every feature row: `CRC_TABLE` and
// `X2N_TABLE` here, and `CRC_BRAID_N`, `CRC_BRAID_W`, `CRC_BIG_TABLE`,
// `CRC_BRAID_TABLE`, `CRC_BRAID_BIG_TABLE` in `braid`. The compiler therefore
// proves, in every configuration, that nothing `build.rs` emits has gone unused
// — which is exactly the producer/consumer drift signal an `allow(dead_code)`
// would have suppressed. `the_generated_contract_cannot_be_silently_weakened`
// pins that no suppression reappears.
mod tables {
    include!(concat!(env!("OUT_DIR"), "/crc32_tables.rs"));
}
use tables::{
    CRC_BIG_TABLE, CRC_BRAID_BIG_TABLE, CRC_BRAID_N, CRC_BRAID_TABLE, CRC_BRAID_W, CRC_TABLE,
    X2N_TABLE,
};

/// Compile-time contract with `build.rs`: the shape of every generated item.
///
/// Each of the seven promised names is named here with its promised type and
/// dimensions, so the build fails to compile — not merely to warn — if the
/// generator stops emitting one, renames it, changes its width, or changes a
/// dimension. The value-level anchors and the endian cross-relations live in
/// this module's test suite, which is where a wrong *number* is caught; this
/// block is what catches a wrong *schema*.
///
/// An anonymous `const _` is used rather than a named constant because the
/// block exists purely for its side effect on type checking and reachability:
/// a *named* constant would itself be unused, and a dead item's initializer
/// does not keep the items it mentions alive.
const _: () = {
    // The two scalar constants: braid width and depth, as declared by
    // `crc32.c`'s `braid()` parameters.
    assert!(CRC_BRAID_N == 5, "build.rs must emit CRC_BRAID_N == 5");
    assert!(CRC_BRAID_W == 8, "build.rs must emit CRC_BRAID_W == 8");

    // The five tables, pinned by length. A `const fn` taking a fixed-size array
    // reference makes the element type and the dimension part of the signature,
    // so a retyped or resized table cannot type-check.
    const fn u32_256(t: &[u32; 256]) -> usize {
        t.len()
    }
    const fn u32_32(t: &[u32; 32]) -> usize {
        t.len()
    }
    const fn u64_256(t: &[u64; 256]) -> usize {
        t.len()
    }
    const fn braid_u32(t: &[[u32; 256]; 8]) -> usize {
        t.len()
    }
    const fn braid_u64(t: &[[u64; 256]; 8]) -> usize {
        t.len()
    }

    assert!(u32_256(&CRC_TABLE) == 256);
    assert!(u32_32(&X2N_TABLE) == 32);
    assert!(u64_256(&CRC_BIG_TABLE) == 256);
    assert!(braid_u32(&CRC_BRAID_TABLE) == CRC_BRAID_W);
    assert!(braid_u64(&CRC_BRAID_BIG_TABLE) == CRC_BRAID_W);

    // The braid tables are indexed by byte value, so each row must be complete.
    assert!(CRC_BRAID_TABLE[0].len() == 256);
    assert!(CRC_BRAID_BIG_TABLE[0].len() == 256);
};

/// The byte-wise CRC entry a little-endian word load would consume.
///
/// `crc32.c` emits both a little-endian (`z_crc_t`) and a big-endian
/// (`z_word_t`, byte-swapped) form of every table so the braided word-at-a-time
/// path can load native words on either endianness.
///
/// Both forms are defined **unconditionally**, and only the final selection is
/// `cfg`-gated. That split is deliberate: an expression that appears solely
/// inside a `#[cfg(target_endian = "big")]` item is never compiled on a
/// little-endian host, so a wrong index or a wrong table in it would sit
/// undetected until someone built for a big-endian target. Defining both here
/// means the tests can assert the relationship *between* them on every target
/// (AAP §0.6.6, §0.7.2 standard S8) while still pinning the value this target
/// actually selects.
#[cfg(test)]
const LITTLE_CRC_ENTRY_1: u64 = CRC_TABLE[1] as u64;
/// Big-endian counterpart of [`LITTLE_CRC_ENTRY_1`].
#[cfg(test)]
const BIG_CRC_ENTRY_1: u64 = CRC_BIG_TABLE[1];

/// The first non-zero braid entry a little-endian word load would consume.
///
/// The big-endian braid sub-tables are stored in reverse position order (C
/// `big[w - 1 - k]` against `ltl[k]`), so the big-endian anchor is **not** the
/// same row index in the other table.
#[cfg(test)]
const LITTLE_BRAID_ENTRY_1: u64 = CRC_BRAID_TABLE[0][1] as u64;
/// Big-endian counterpart of [`LITTLE_BRAID_ENTRY_1`].
#[cfg(test)]
const BIG_BRAID_ENTRY_1: u64 = CRC_BRAID_BIG_TABLE[CRC_BRAID_W - 1][1];

/// The byte-wise CRC entry the active target's word endianness selects.
///
/// A target for which neither arm applied would fail to compile.
#[cfg(all(test, target_endian = "little"))]
const SELECTED_CRC_ENTRY_1: u64 = LITTLE_CRC_ENTRY_1;
/// Big-endian counterpart of [`SELECTED_CRC_ENTRY_1`].
#[cfg(all(test, target_endian = "big"))]
const SELECTED_CRC_ENTRY_1: u64 = BIG_CRC_ENTRY_1;

/// The first non-zero braid entry the active target's word endianness selects.
#[cfg(all(test, target_endian = "little"))]
const SELECTED_BRAID_ENTRY_1: u64 = LITTLE_BRAID_ENTRY_1;
/// Big-endian counterpart of [`SELECTED_BRAID_ENTRY_1`].
#[cfg(all(test, target_endian = "big"))]
const SELECTED_BRAID_ENTRY_1: u64 = BIG_BRAID_ENTRY_1;

/// The CRC-32 polynomial, reflected, with the `x^32` term implied.
///
/// This is `0xEDB88320`, the reflected form of the IEEE 802.3 CRC-32
/// polynomial (`POLY` in `crc32.c`). It drives the GF(2) reductions performed
/// by `multmodp`, which in turn back the checksum-combining routines.
const POLY: u32 = 0xedb8_8320;

/// Updates a running CRC-32 with the bytes in `buf` and returns the updated
/// CRC-32.
///
/// A CRC-32 value is in the range of a 32-bit unsigned integer. A fresh
/// checksum is started from `0`; passing an empty slice leaves the running
/// value unchanged, so `crc32(0, b"")` returns `0`.
///
/// Pre- and post-conditioning (one's complement) is performed inside this
/// function, so the application must **not** do it.
///
/// This mirrors the C `crc32(crc, buf, len)` entry point. The C contract of
/// returning the required initial value for a `NULL` buffer is handled at the
/// FFI boundary, where a null pointer can occur; in this idiomatic slice-based
/// API there is no null, so an empty slice simply returns `crc` unchanged.
///
/// # Examples
///
/// ```
/// # use zlib_rs::checksum::crc32::crc32;
/// let mut crc = crc32(0, b"");     // required initial value
/// crc = crc32(crc, b"123456789");  // fold in more data
/// assert_eq!(crc, 0xcbf4_3926);
/// ```
#[must_use]
pub fn crc32(crc: u32, buf: &[u8]) -> u32 {
    crc32_z(crc, buf)
}

/// Updates a running CRC-32 with the bytes in `buf` and returns the updated
/// CRC-32.
///
/// This is identical to [`crc32`] and exists to mirror the C `crc32_z` entry
/// point, which accepts a `size_t` length rather than an `unsigned int`
/// length. In this slice-based API both collapse to `&[u8]`, so [`crc32`]
/// simply delegates here. Both names are retained because the FFI layer
/// exposes them as distinct C symbols.
///
/// Pre- and post-conditioning (one's complement) is performed internally.
#[must_use]
pub fn crc32_z(crc: u32, buf: &[u8]) -> u32 {
    // Dispatch to whichever bulk implementation the active feature set selects.
    // Both implementations are bit-exact equivalents, so callers never observe
    // a difference; only throughput changes.
    crc32_bulk(crc, buf)
}

/// Which bulk CRC-32 implementation this build will actually execute.
///
/// Both variants compute the same value for every input — they are bit-exact
/// equivalents, and the whole test suite asserts that — so this type carries no
/// correctness meaning. It exists so that *performance* claims can be tied to
/// the code they describe: a benchmark, a CI job, or a bug report can record
/// which path it measured instead of inferring it from a feature flag, which is
/// precisely the inference that hid a CRC-32 pessimization in the shipped
/// library while `cargo bench` measured the accelerated path (see the module
/// documentation, "Reachability").
///
/// Obtain the value from [`crc32_backend`].
///
/// The enumeration is `#[non_exhaustive]`: adding a third backend must not be a
/// breaking change for a consumer that only prints or compares the value, which
/// is the only intended use.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Crc32Backend {
    /// The braided, word-at-a-time scalar port of `crc32.c`'s `#ifdef W` fast
    /// path, contained entirely in this module.
    ///
    /// Always the answer when the `simd` feature is off, and also the answer
    /// when `simd` is on but `crc32fast`'s hardware backend is unreachable on
    /// this target or CPU — which a `no_std` build reaches only when the
    /// required CPU features were not compiled in.
    Braid,
    /// The `crc32fast` crate's hardware-accelerated backend (x86 `pclmulqdq` or
    /// the AArch64 CRC instructions).
    ///
    /// Requires the `simd` feature *and* a target/CPU on which `crc32fast` will
    /// really select that backend.
    Crc32Fast,
}

/// Reports which bulk CRC-32 implementation [`crc32`] executes on this machine.
///
/// The answer is stable for the lifetime of the process: it depends only on the
/// compiled feature set, the target architecture, and immutable CPU capability
/// bits. It is not affected by the input length.
///
/// This is diagnostic information, never a correctness switch — both backends
/// return identical values for identical input.
///
/// # Cost
///
/// The answer is resolved at most once per process and cached thereafter, so
/// this is cheap enough to sit on the [`crc32`] hot path — which it does, since
/// the bulk dispatcher consults it on every bulk call. Only the first call can
/// run a CPU-feature probe; every later one reads a cached byte.
///
/// # Examples
///
/// ```
/// # use zlib_rs::checksum::crc32::{crc32, crc32_backend};
/// // Whichever backend is reported, the canonical check value is the same.
/// let backend = crc32_backend();
/// println!("CRC-32 backend: {backend:?}");
/// assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
/// ```
#[must_use]
pub fn crc32_backend() -> Crc32Backend {
    memoized_crc32_backend()
}

/// Resolves the backend from first principles: the compiled feature set, plus a
/// CPU-capability test on the targets that have one.
///
/// This is the whole decision, and it is deliberately kept separate from the
/// caching in [`memoized_crc32_backend`] so that a test can compare the cached
/// answer against a freshly computed one.
fn resolve_crc32_backend() -> Crc32Backend {
    // `cfg!` rather than `#[cfg]` so both arms type-check in every feature row.
    // With `simd` off the predicate is never reached at run time (`&&` short
    // circuits on a compile-time `false`), but it is still compiled, so it
    // cannot rot in the configuration that does not use it.
    if cfg!(feature = "simd") && accelerated_backend_is_reachable() {
        Crc32Backend::Crc32Fast
    } else {
        Crc32Backend::Braid
    }
}

// The cached form of `resolve_crc32_backend`.
//
// Caching is only worth anything where the resolver actually costs something,
// which is exactly the configurations whose `accelerated_backend_is_reachable`
// performs a RUN-TIME probe: `std` on x86, x86-64 or AArch64. Everywhere else the
// resolver is a chain of `cfg!` predicates that the optimizer folds to a
// constant, and routing it through an atomic would make it *slower*, so those
// configurations call the resolver directly.
cfg_if::cfg_if! {
    if #[cfg(all(
        feature = "std",
        any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")
    ))] {
        /// Returns the process-wide backend decision, probing the CPU at most
        /// once.
        ///
        /// Without this cache every bulk CRC-32 call re-ran the three
        /// `is_x86_feature_detected!` probes inside
        /// [`accelerated_backend_is_reachable`] — and then handed the work to
        /// `crc32fast`, which immediately repeated its own three probes in
        /// `Hasher::new_with_initial`. Those probes are individually cheap and
        /// internally cached by `std`, but they are not free, and this crate's
        /// half of the pair is pure overhead once the answer is known.
        ///
        /// # Why a plain relaxed load and store is sufficient
        ///
        /// The value being cached is *immutable for the lifetime of the
        /// process*: it is a function of the compiled feature set, the target
        /// architecture, and CPU capability bits that do not change while the
        /// program runs. Two threads racing here therefore compute the **same**
        /// answer, so the worst a race can do is perform the resolution more
        /// than once and store the identical byte more than once. No lost
        /// update is possible, nothing is published *through* this cell, and no
        /// other memory is ordered against it — so `Relaxed` is the correct
        /// ordering rather than merely a cheap one, and a compare-exchange loop
        /// would buy nothing. Restricting the cell to load/store also keeps it
        /// available on every target that has 8-bit atomics.
        ///
        /// Correctness does not depend on the cache at all: both backends are
        /// bit-exact equivalents, so even a hypothetically stale answer would
        /// return the same checksum — only the throughput would differ.
        fn memoized_crc32_backend() -> Crc32Backend {
            use core::sync::atomic::{AtomicU8, Ordering};

            /// Sentinel meaning "not resolved yet". Distinct from both encoded
            /// backends so the first caller can tell the cell is empty.
            const UNRESOLVED: u8 = 0;
            /// Encoded [`Crc32Backend::Braid`].
            const BRAID: u8 = 1;
            /// Encoded [`Crc32Backend::Crc32Fast`].
            const CRC32FAST: u8 = 2;

            static MEMO: AtomicU8 = AtomicU8::new(UNRESOLVED);

            match MEMO.load(Ordering::Relaxed) {
                BRAID => Crc32Backend::Braid,
                CRC32FAST => Crc32Backend::Crc32Fast,
                // `UNRESOLVED`, and — unreachable in practice, since nothing
                // else is ever stored — any other byte: resolve and publish.
                _ => {
                    let backend = resolve_crc32_backend();
                    MEMO.store(
                        match backend {
                            Crc32Backend::Braid => BRAID,
                            Crc32Backend::Crc32Fast => CRC32FAST,
                        },
                        Ordering::Relaxed,
                    );
                    backend
                }
            }
        }
    } else {
        /// Returns the process-wide backend decision.
        ///
        /// On this configuration [`accelerated_backend_is_reachable`] is a
        /// compile-time `cfg!` test, so the resolver is already constant-folded
        /// and there is nothing a cache could save.
        fn memoized_crc32_backend() -> Crc32Backend {
            resolve_crc32_backend()
        }
    }
}

// Whether `crc32fast`'s hardware backend can actually be selected here.
//
// These arms MIRROR `crc32fast` 1.5.0's own gates, which is the only way to know
// the answer: `crc32fast` reports neither the backend it chose nor the
// conditions it tested.
//
//   * `specialized::pclmulqdq::State::new` requires `pclmulqdq`, `sse2` and
//     `sse4.1`, probed at RUN TIME when `crc32fast/std` is on and at COMPILE
//     TIME (`cfg!(target_feature = ...)`) when it is off;
//   * `specialized::aarch64::State::new` requires `crc`, probed the same two
//     ways;
//   * every other architecture resolves to an uninhabited `State` whose `new`
//     always returns `None`, so no acceleration is possible.
//
// The crate's own `std` feature is what forwards to `crc32fast/std`
// (`Cargo.toml`), so gating the run-time arms on `feature = "std"` tests exactly
// the condition that decides which `State::new` body was compiled.
//
// Mirroring can drift in either direction: if a future `crc32fast` widened or
// narrowed its gate, this predicate could miss an acceleration that is available
// or claim one that is not taken. Either way the checksum stays correct, because
// the two paths are bit-exact equivalents; what a mismatch costs is a
// misreported `Crc32Backend` and a throughput difference. The `Cargo.lock` pin
// keeps the mirrored version fixed, and `crc32fast`'s AArch64 backend
// additionally needs its own `stable_arm_crc32_intrinsics` cfg, which its build
// script sets for every rustc from 1.80 onwards — always true at this crate's
// 1.85.0 MSRV.
//
// Only the x86_64 arms run on a natively-executed CI target; the AArch64 and
// other-architecture arms are cross-type-checked, so they compile against
// `crc32fast`'s source but are never measured. `CONTRIBUTING.md` ("Platform
// honesty") records which targets are executed and which are only checked.
cfg_if::cfg_if! {
    if #[cfg(all(feature = "std", any(target_arch = "x86", target_arch = "x86_64")))] {
        /// x86/x86-64 with `std`: run-time CPU probe, matching `crc32fast`'s
        /// `#[cfg(feature = "std")] State::new`.
        fn accelerated_backend_is_reachable() -> bool {
            std::arch::is_x86_feature_detected!("pclmulqdq")
                && std::arch::is_x86_feature_detected!("sse2")
                && std::arch::is_x86_feature_detected!("sse4.1")
        }
    } else if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
        /// x86/x86-64 without `std`: compile-time test, matching `crc32fast`'s
        /// `#[cfg(not(feature = "std"))] State::new`. False on a stock target,
        /// true when the caller built with `-C target-feature=+pclmulqdq,+sse4.1`.
        fn accelerated_backend_is_reachable() -> bool {
            cfg!(all(
                target_feature = "pclmulqdq",
                target_feature = "sse2",
                target_feature = "sse4.1"
            ))
        }
    } else if #[cfg(all(feature = "std", target_arch = "aarch64"))] {
        /// AArch64 with `std`: run-time probe for the CRC instructions.
        fn accelerated_backend_is_reachable() -> bool {
            std::arch::is_aarch64_feature_detected!("crc")
        }
    } else if #[cfg(target_arch = "aarch64")] {
        /// AArch64 without `std`: compile-time test for the CRC instructions.
        fn accelerated_backend_is_reachable() -> bool {
            cfg!(target_feature = "crc")
        }
    } else {
        /// Architectures for which `crc32fast` has no specialized backend: its
        /// `State::new` can only return `None`, so there is no acceleration to
        /// select and the braid is the only path available.
        fn accelerated_backend_is_reachable() -> bool {
            false
        }
    }
}

/// Bulk CRC-32 with the `simd` feature enabled: `crc32fast` when its hardware
/// backend is reachable, the braided scalar path otherwise.
///
/// `crc32fast` maintains the same external CRC convention as zlib:
/// `new_with_initial(crc)` seeds the running value, `update` folds in the bytes,
/// and `finalize` applies the trailing conditioning and returns the updated CRC.
/// Its output is identical to [`braid::crc32_bulk`] for every input, so the
/// branch below is a pure throughput decision.
#[cfg(feature = "simd")]
fn crc32_bulk(crc: u32, buf: &[u8]) -> u32 {
    match crc32_backend() {
        Crc32Backend::Crc32Fast => {
            let mut hasher = crc32fast::Hasher::new_with_initial(crc);
            hasher.update(buf);
            hasher.finalize()
        }
        Crc32Backend::Braid => braid::crc32_bulk(crc, buf),
    }
}

/// Scalar bulk CRC-32 (the only bulk path compiled when the `simd` feature is
/// disabled, whether or not the build is `no_std`).
///
/// Delegates to the braided, word-at-a-time implementation in [`braid`], which
/// is the port of the `#ifdef W` fast path in `crc32.c`. That routine falls back
/// to the byte-wise table loop for short inputs and for the trailing bytes, so
/// this path allocates nothing, needs no external crate, and is bit-identical to
/// reference zlib for every input.
#[cfg(not(feature = "simd"))]
fn crc32_bulk(crc: u32, buf: &[u8]) -> u32 {
    braid::crc32_bulk(crc, buf)
}

/// Braided, word-at-a-time CRC-32 — the safe-Rust port of the `#ifdef W` fast
/// path in `crc32.c` (`crc32_z`, plus its `crc_word`, `crc_word_big`, and
/// `byte_swap` helpers).
///
/// The braid computes `CRC_BRAID_N` independent CRCs over interleaved
/// `CRC_BRAID_W`-byte words and combines them at the end of the final block,
/// which breaks the serial dependency of the byte-wise loop.
///
/// # Endian selection is resolved at compile time
///
/// Both the little- and big-endian variants are *compiled* on every target —
/// [`cfg!`] is an expression macro, so both arms of the `if` are type-checked and
/// both generated table sets stay alive rather than becoming dead code — but the
/// arm that actually runs is fixed when the crate is built, because
/// `cfg!(target_endian = ...)` is a compile-time constant read from the target
/// triple.
///
/// This is a deliberate, output-preserving divergence from C. Reference zlib
/// probes endianness at *execution* time (`endian = 1; if (*(unsigned char
/// *)&endian)`, `crc32.c` L657-L662) and explains why: a bi-endian ARM core can
/// change endianness at run time, so a compile-time answer could be wrong for the
/// mode the process is actually running in. C's own comment notes that a compiler
/// which knows the endianness will optimize the check and the unused branch away,
/// which is precisely the code this port emits directly.
///
/// The divergence cannot change a checksum. Each variant is a self-consistent
/// algorithm over the same byte sequence — one loading words little-endian
/// against the reflected tables, the other big-endian against the byte-swapped
/// companions — so both return the same CRC for the same input. That is asserted,
/// not assumed: `both_endian_braids_match_byte_wise` drives `braid_le` *and*
/// `braid_be` against the byte-wise reference on whatever target runs the suite,
/// which is also what exercises `CRC_BIG_TABLE` and `CRC_BRAID_BIG_TABLE` on a
/// little-endian host where the selected path never reaches them.
///
/// Should a target ever require a genuine run-time probe, the replacement is
/// local to the `cfg!` in [`crc32_bulk`]: both branches already exist and are
/// already tested.
///
/// This module is compiled **unconditionally**. With the `simd` feature off it
/// is the bulk implementation; with `simd` on it is the fallback [`crc32_bulk`]
/// takes whenever `crc32fast`'s hardware backend is unreachable (see the module
/// documentation, "Reachability"), so it is live code in every feature row —
/// which is also why the generated tables need no `dead_code` allowance any
/// more, and why its equivalence tests run in every configuration.
mod braid {
    use super::tables::{
        CRC_BIG_TABLE, CRC_BRAID_BIG_TABLE, CRC_BRAID_N, CRC_BRAID_TABLE, CRC_BRAID_W, CRC_TABLE,
    };

    /// Number of interleaved braids (C `N`).
    const N: usize = CRC_BRAID_N;

    /// Bytes per CRC word (C `W`).
    const W: usize = CRC_BRAID_W;

    /// C's `byte_swap` is written for a 32- or 64-bit `z_word_t`; this port
    /// implements the 64-bit form, so the generated word size must agree.
    const _: () = assert!(W == 8, "this braid port assumes a 64-bit CRC word");

    /// Shortest input for which C enters the braided path
    /// (`len >= N * W + W - 1`, i.e. 47 for the generated `N = 5`, `W = 8`).
    const MIN_LEN: usize = N * W + W - 1;

    /// Folds one byte into a running, pre-conditioned CRC via the reflected
    /// byte-wise table — the innermost statement of every C byte loop.
    #[inline]
    fn fold_byte(crc: u32, byte: u8) -> u32 {
        (crc >> 8) ^ CRC_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize]
    }

    /// CRC of the `W` bytes in `data`, least-significant byte first, with no pre-
    /// or post-conditioning. Port of C `crc_word`; used to combine the braids.
    #[inline]
    fn crc_word(mut data: u64) -> u32 {
        for _ in 0..W {
            data = (data >> 8) ^ u64::from(CRC_TABLE[(data & 0xff) as usize]);
        }
        data as u32
    }

    /// Big-endian counterpart of [`crc_word`]. Port of C `crc_word_big`, which
    /// shifts the other way and indexes on the most-significant byte.
    #[inline]
    fn crc_word_big(mut data: u64) -> u64 {
        for _ in 0..W {
            data = (data << 8) ^ CRC_BIG_TABLE[((data >> ((W - 1) * 8)) & 0xff) as usize];
        }
        data
    }

    /// Reads braid `i`'s word out of `block` in little-endian byte order — the
    /// safe equivalent of C's `words[i]` native load on a little-endian target.
    #[inline]
    fn word_le(block: &[u8], i: usize) -> u64 {
        let mut bytes = [0u8; W];
        bytes.copy_from_slice(&block[i * W..(i + 1) * W]);
        u64::from_le_bytes(bytes)
    }

    /// Big-endian counterpart of [`word_le`].
    #[inline]
    fn word_be(block: &[u8], i: usize) -> u64 {
        let mut bytes = [0u8; W];
        bytes.copy_from_slice(&block[i * W..(i + 1) * W]);
        u64::from_be_bytes(bytes)
    }

    /// Little-endian braid over `blks` complete `N * W`-byte blocks.
    ///
    /// `crc` is already pre-conditioned and the return value stays
    /// pre-conditioned, matching C, where this runs inline on `crc`.
    fn braid_le(crc: u32, blocks: &[u8], blks: usize) -> u32 {
        debug_assert_eq!(blocks.len(), blks * N * W);
        debug_assert!(blks >= 1, "MIN_LEN guarantees at least one block");

        // C seeds braid 0 with the incoming CRC and the rest with zero.
        let mut crcs = [0u32; N];
        crcs[0] = crc;

        let mut blocks = blocks.chunks_exact(N * W);

        // The first `blks - 1` blocks advance each braid independently.
        for _ in 0..blks - 1 {
            let block = blocks.next().expect("blks blocks were split off");
            for (i, braid) in crcs.iter_mut().enumerate() {
                let word = u64::from(*braid) ^ word_le(block, i);
                let mut acc = CRC_BRAID_TABLE[0][(word & 0xff) as usize];
                for (k, table) in CRC_BRAID_TABLE.iter().enumerate().skip(1) {
                    acc ^= table[((word >> (k * 8)) & 0xff) as usize];
                }
                *braid = acc;
            }
        }

        // The last block combines the braids: each is folded into the running
        // value, which is why the accumulator joins the XOR from braid 1 on.
        let block = blocks.next().expect("the final block");
        let mut acc = crc_word(u64::from(crcs[0]) ^ word_le(block, 0));
        for (i, &braid) in crcs.iter().enumerate().skip(1) {
            acc = crc_word(u64::from(braid) ^ word_le(block, i) ^ u64::from(acc));
        }
        acc
    }

    /// Big-endian braid over `blks` complete `N * W`-byte blocks.
    ///
    /// Mirrors C's `else` branch: the braids are full words, braid 0 is seeded
    /// with `byte_swap(crc)`, the big tables are indexed on the
    /// most-significant byte, and the combined result is byte-swapped back.
    fn braid_be(crc: u32, blocks: &[u8], blks: usize) -> u32 {
        debug_assert_eq!(blocks.len(), blks * N * W);
        debug_assert!(blks >= 1, "MIN_LEN guarantees at least one block");

        let mut crcs = [0u64; N];
        crcs[0] = u64::from(crc).swap_bytes();

        let mut blocks = blocks.chunks_exact(N * W);

        for _ in 0..blks - 1 {
            let block = blocks.next().expect("blks blocks were split off");
            for (i, braid) in crcs.iter_mut().enumerate() {
                let word = *braid ^ word_be(block, i);
                let mut acc = CRC_BRAID_BIG_TABLE[0][(word & 0xff) as usize];
                for (k, table) in CRC_BRAID_BIG_TABLE.iter().enumerate().skip(1) {
                    acc ^= table[((word >> (k * 8)) & 0xff) as usize];
                }
                *braid = acc;
            }
        }

        let block = blocks.next().expect("the final block");
        let mut comb = crc_word_big(crcs[0] ^ word_be(block, 0));
        for (i, &braid) in crcs.iter().enumerate().skip(1) {
            comb = crc_word_big(braid ^ word_be(block, i) ^ comb);
        }
        comb.swap_bytes() as u32
    }

    /// Bulk CRC-32 over `buf`, updating `crc`.
    ///
    /// Pre- and post-conditioning (one's complement) happen here, as in C
    /// `crc32_z`. Long inputs take the braided path; the remainder is finished
    /// with C's eight-way-unrolled byte loop and then a plain byte loop.
    ///
    /// # Divergence from C: no word-alignment prologue
    ///
    /// C first folds bytes one at a time until `buf` is `W`-aligned, purely
    /// because it then casts the pointer to `z_word_t const *` and dereferences
    /// it — an unaligned load would be undefined behaviour there. This port
    /// reads each word with [`u64::from_le_bytes`]/[`u64::from_be_bytes`] over a
    /// byte slice, which has no alignment requirement, so the prologue has no
    /// purpose and is omitted. It cannot change the result: both forms compute
    /// the same CRC over the same byte sequence, and only the split between the
    /// braided and byte-wise portions moves. The equivalence is asserted
    /// directly by the tests below, which compare against the byte-wise
    /// reference for every length up to several blocks and at every offset
    /// within a word.
    pub fn crc32_bulk(crc: u32, buf: &[u8]) -> u32 {
        // Pre-condition: work on the one's complement of the incoming CRC.
        let mut c = !crc;
        let mut rest = buf;

        if rest.len() >= MIN_LEN {
            let blks = rest.len() / (N * W);
            let (blocks, tail) = rest.split_at(blks * N * W);
            // C decides this at run time; `cfg!` decides it at compile time
            // while still compiling — and therefore type-checking and keeping
            // alive — both branches and both generated table sets.
            c = if cfg!(target_endian = "little") {
                braid_le(c, blocks, blks)
            } else {
                braid_be(c, blocks, blks)
            };
            rest = tail;
        }

        // C finishes with an eight-way-unrolled byte loop, then single bytes.
        let mut octets = rest.chunks_exact(8);
        for octet in &mut octets {
            for &byte in octet {
                c = fold_byte(c, byte);
            }
        }
        for &byte in octets.remainder() {
            c = fold_byte(c, byte);
        }

        // Post-condition: undo the initial complement.
        !c
    }

    #[cfg(test)]
    mod tests {
        use super::{MIN_LEN, N, W, braid_be, braid_le, crc32_bulk, fold_byte};
        use alloc::vec::Vec;

        /// The unbraided byte-wise algorithm, kept as an independent reference:
        /// this is exactly the loop the module replaced, so agreement with it is
        /// the correctness criterion.
        fn reference(crc: u32, buf: &[u8]) -> u32 {
            let mut c = !crc;
            for &byte in buf {
                c = fold_byte(c, byte);
            }
            !c
        }

        /// Pseudo-random but deterministic bytes (a xorshift, so no dependency).
        fn corpus(len: usize) -> Vec<u8> {
            let mut state = 0x1234_5678_9abc_def0_u64;
            (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect()
        }

        /// Every length from empty to well past several braid blocks must agree
        /// with the byte-wise reference — covering the sub-threshold path, the
        /// threshold itself, partial blocks, and the unrolled tail.
        #[test]
        fn braided_matches_byte_wise_for_every_length() {
            let data = corpus(600);
            for len in 0..=600 {
                let slice = &data[..len];
                assert_eq!(
                    crc32_bulk(0, slice),
                    reference(0, slice),
                    "length {len} disagrees with the byte-wise reference"
                );
                // A non-zero seed exercises the braid-0 seeding path too.
                assert_eq!(
                    crc32_bulk(0xdead_beef, slice),
                    reference(0xdead_beef, slice),
                    "length {len} disagrees for a non-zero seed"
                );
            }
        }

        /// Starting the buffer at every offset within a word proves the omitted
        /// word-alignment prologue cannot change the result.
        #[test]
        fn every_word_offset_matches_byte_wise() {
            let data = corpus(512 + W);
            for offset in 0..=W {
                let slice = &data[offset..offset + 512];
                assert_eq!(
                    crc32_bulk(0, slice),
                    reference(0, slice),
                    "offset {offset} disagrees with the byte-wise reference"
                );
            }
        }

        /// **Both** braid implementations are checked here, on whatever target
        /// runs the suite. Each is a self-consistent algorithm over the byte
        /// sequence — one loading words little-endian with the reflected tables,
        /// the other big-endian with the big tables — so both must reproduce the
        /// reference regardless of host endianness. This is what exercises
        /// `CRC_BRAID_BIG_TABLE` and `CRC_BIG_TABLE` on a little-endian host,
        /// where the dispatched path never reaches them.
        #[test]
        fn both_endian_braids_match_byte_wise() {
            let data = corpus(8 * N * W);
            for blks in 1..=8 {
                let blocks = &data[..blks * N * W];
                let expected = reference(0, blocks);
                assert_eq!(braid_le(!0, blocks, blks), !expected, "little-endian braid");
                assert_eq!(braid_be(!0, blocks, blks), !expected, "big-endian braid");

                // Non-zero seed: both braids must thread it through identically.
                let seed = 0x0bad_f00d_u32;
                let expected = reference(seed, blocks);
                assert_eq!(braid_le(!seed, blocks, blks), !expected, "little-endian");
                assert_eq!(braid_be(!seed, blocks, blks), !expected, "big-endian");
            }
        }

        /// Splitting a buffer anywhere and folding the parts sequentially must
        /// equal one call over the whole buffer, which is the property every
        /// streaming caller depends on.
        #[test]
        fn split_updates_are_equivalent() {
            let data = corpus(300);
            let whole = crc32_bulk(0, &data);
            for split in 0..=data.len() {
                let first = crc32_bulk(0, &data[..split]);
                assert_eq!(crc32_bulk(first, &data[split..]), whole, "split at {split}");
            }
        }

        /// The canonical zlib known-answer vector, through the braided path and
        /// through a buffer long enough to enter it.
        #[test]
        fn known_answer_vectors() {
            assert_eq!(crc32_bulk(0, b"123456789"), 0xcbf4_3926);
            assert_eq!(crc32_bulk(0, b""), 0);

            // 47 bytes is exactly MIN_LEN: the shortest braided input.
            let data = corpus(MIN_LEN);
            assert_eq!(data.len(), 47);
            assert_eq!(crc32_bulk(0, &data), reference(0, &data));
        }
    }
}

/// Returns `a(x)` multiplied by `b(x)` modulo `p(x)`, where `p(x)` is the
/// reflected CRC polynomial.
///
/// This is a carry-less (GF(2)) multiply-and-reduce, ported directly from
/// `multmodp()` in `crc32.c`. For speed the C routine requires that `a` is not
/// zero; every call site here upholds that invariant, so the loop is
/// guaranteed to terminate at the lowest set bit of `a`. (A zero `a` would
/// never satisfy the break condition and would loop until `m` underflowed;
/// callers must therefore never pass `a == 0`.)
///
/// All intermediate values fit in 32 bits, so `u32` arithmetic reproduces the
/// C `uLong` computation exactly.
fn multmodp(a: u32, mut b: u32) -> u32 {
    // `m` walks a single set bit from bit 31 down toward the lowest set bit
    // of `a`.
    let mut m: u32 = 1 << 31;
    let mut p: u32 = 0;
    loop {
        if (a & m) != 0 {
            p ^= b;
            // Once the lowest set bit of `a` has been consumed, we are done.
            if (a & (m - 1)) == 0 {
                break;
            }
        }
        m >>= 1;
        // Multiply `b` by x modulo p(x): a right shift, reducing on carry-out.
        b = if (b & 1) != 0 {
            (b >> 1) ^ POLY
        } else {
            b >> 1
        };
    }
    p
}

/// Returns `x^(n * 2^k) modulo p(x)`.
///
/// Port of `x2nmodp()` in `crc32.c`, walking the precomputed `X2N_TABLE` of
/// squared powers of x. `n` is accepted as an `i64` to mirror the C
/// `z_off64_t` argument; callers guarantee it is non-negative (the public
/// combine entry points reject negative lengths before reaching here), so the
/// cast to `u64` performs a well-defined logical right shift over the bits of
/// `n`.
fn x2nmodp(n: i64, mut k: u32) -> u32 {
    let mut p: u32 = 1 << 31; // x^0 == 1, the identity element for multmodp.
    let mut n = n as u64;
    while n != 0 {
        if (n & 1) != 0 {
            p = multmodp(X2N_TABLE[(k & 31) as usize], p);
        }
        n >>= 1;
        k += 1;
    }
    p
}

/// Returns the operator corresponding to a second-sequence length of `len2`,
/// for repeated use with [`crc32_combine_op`].
///
/// `len2` must be non-negative; otherwise zero is returned. Computing the
/// operator once and reusing it across many [`crc32_combine_op`] calls is
/// faster than calling [`crc32_combine`] repeatedly with the same length.
///
/// A single `i64` length covers both the C `crc32_combine_gen` (`z_off_t`) and
/// `crc32_combine_gen64` (`z_off64_t`) entry points.
#[must_use]
pub fn crc32_combine_gen(len2: i64) -> u32 {
    // A negative length has no meaning; match reference zlib and return zero.
    if len2 < 0 {
        return 0;
    }
    x2nmodp(len2, 3)
}

/// Combines two CRC-32 values using a precomputed operator `op` in place of a
/// length.
///
/// Gives the same result as [`crc32_combine`], but takes `op` (produced by
/// [`crc32_combine_gen`]) instead of `len2`. This is faster than
/// [`crc32_combine`] when the same operator is applied more than once.
///
/// A zero operator is never produced for a valid length
/// (`crc32_combine_gen(0)` is `0x8000_0000`), so it is treated defensively as
/// an error and yields zero.
#[must_use]
pub fn crc32_combine_op(crc1: u32, crc2: u32, op: u32) -> u32 {
    if op == 0 {
        return 0;
    }
    multmodp(op, crc1) ^ crc2
}

/// Combines two CRC-32 check values into one.
///
/// Given two byte sequences `seq1` and `seq2` with lengths `len1` and `len2`
/// and CRC-32 values `crc1` and `crc2` respectively, this returns the CRC-32
/// of the concatenation `seq1 || seq2`, requiring only `crc1`, `crc2`, and
/// `len2` — the length of the second sequence.
///
/// `len2` must be non-negative; otherwise zero is returned. A single `i64`
/// length covers both the C `crc32_combine` (`z_off_t`) and `crc32_combine64`
/// (`z_off64_t`) entry points.
#[must_use]
pub fn crc32_combine(crc1: u32, crc2: u32, len2: i64) -> u32 {
    crc32_combine_op(crc1, crc2, crc32_combine_gen(len2))
}

/// Returns a reference to the 256-entry byte-wise CRC-32 lookup table.
///
/// This mirrors the C `get_crc_table()`, which returns `const z_crc_t *`. The
/// table is a pure mathematical constant derived from the reflected polynomial
/// `POLY` and is generated at build time; applications rarely need it, but it
/// is exposed for parity with the reference API (for example, assembly CRC
/// implementations and consumers that inspect the table directly).
#[must_use]
pub fn get_crc_table() -> &'static [u32; 256] {
    &CRC_TABLE
}

#[cfg(test)]
mod tests {
    use super::{
        BIG_BRAID_ENTRY_1, BIG_CRC_ENTRY_1, CRC_BIG_TABLE, CRC_BRAID_BIG_TABLE, CRC_BRAID_N,
        CRC_BRAID_TABLE, CRC_BRAID_W, CRC_TABLE, Crc32Backend, LITTLE_BRAID_ENTRY_1,
        LITTLE_CRC_ENTRY_1, SELECTED_BRAID_ENTRY_1, SELECTED_CRC_ENTRY_1, X2N_TABLE,
        accelerated_backend_is_reachable, braid, crc32, crc32_backend, crc32_combine,
        crc32_combine_gen, crc32_combine_op, crc32_z, get_crc_table, multmodp,
        resolve_crc32_backend,
    };

    // Every expected value below was produced by reference zlib (Python's
    // `zlib.crc32`), the canonical implementation this port must match
    // bit-for-bit. Because the `simd` and scalar paths are interchangeable,
    // these assertions validate whichever path the active feature set compiles.

    #[test]
    fn empty_slice_returns_input_unchanged() {
        // A fresh running CRC starts at 0; an empty update never changes it.
        assert_eq!(crc32(0, b""), 0);
        assert_eq!(crc32(0xdead_beef, b""), 0xdead_beef);
        assert_eq!(crc32_z(0, b""), 0);
    }

    #[test]
    fn known_reference_vectors() {
        // The canonical CRC-32/IEEE check value.
        assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(0, b"a"), 0xe8b7_be43);
        assert_eq!(crc32(0, b"abc"), 0x3524_41c2);
        assert_eq!(
            crc32(0, b"The quick brown fox jumps over the lazy dog"),
            0x414f_a339
        );
    }

    #[test]
    fn crc32_and_crc32_z_agree() {
        let data = b"The quick brown fox jumps over the lazy dog";
        for len in 0..=data.len() {
            assert_eq!(crc32(0, &data[..len]), crc32_z(0, &data[..len]));
        }
    }

    #[test]
    fn byte_at_a_time_matches_single_pass() {
        // Folding one byte at a time via running updates must equal one bulk
        // call — this exercises the incremental-update contract.
        let data = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut running = 0;
        for &byte in data {
            running = crc32(running, &[byte]);
        }
        assert_eq!(running, crc32(0, data));
    }

    #[test]
    fn split_update_matches_whole() {
        // For any split buf = a || b: crc32(crc32(0, a), b) == crc32(0, buf).
        let whole = b"The quick brown fox jumps over the lazy dog";
        for split in 0..=whole.len() {
            let (a, b) = whole.split_at(split);
            assert_eq!(crc32(crc32(0, a), b), crc32(0, whole));
        }
        assert_eq!(crc32(0, whole), 0x414f_a339);
    }

    #[test]
    fn large_buffer_matches_reference() {
        // 20_000 bytes exercises long inputs (and the block-based SIMD path).
        let mut big = [0u8; 20_000];
        for (i, byte) in big.iter_mut().enumerate() {
            *byte = ((i * 37 + 11) & 0xff) as u8;
        }
        assert_eq!(crc32(0, &big), 0x897e_9e86);
    }

    // -----------------------------------------------------------------------
    // Backend selection. These tests pin the *dispatch*, not the arithmetic: the
    // value tests above already prove both paths are bit-exact, so what has to be
    // guaranteed here is that the reported backend is the one that actually runs,
    // that it can never be `Crc32Fast` without the `simd` feature, and that
    // selecting it can never change a checksum.
    // -----------------------------------------------------------------------

    /// The reported backend must agree with the compiled configuration and with
    /// the reachability predicate — never merely with the feature flag, which is
    /// exactly the inference that let a pessimization ship unnoticed.
    #[test]
    fn the_reported_backend_matches_the_compiled_configuration() {
        let backend = crc32_backend();

        if cfg!(feature = "simd") {
            let expected = if accelerated_backend_is_reachable() {
                Crc32Backend::Crc32Fast
            } else {
                Crc32Backend::Braid
            };
            assert_eq!(
                backend, expected,
                "with `simd` on, the backend must follow the reachability probe"
            );
        } else {
            assert_eq!(
                backend,
                Crc32Backend::Braid,
                "without the `simd` feature there is no `crc32fast` in the graph, \
                 so the braid is the only possible backend"
            );
        }

        // Stable for the lifetime of the process: the probe reads immutable CPU
        // capability bits, so repeated calls cannot disagree.
        assert_eq!(backend, crc32_backend());
    }

    /// The cached answer must be the same answer, every time.
    ///
    /// [`crc32_backend`] is consulted on every bulk CRC-32 call, so its result is
    /// memoized rather than re-probed. That cache is only ever correct if it
    /// returns exactly what a fresh resolution returns, so this compares the two
    /// directly — including on the first call, before anything can have been
    /// cached — and then hammers the cached path to show it does not drift.
    #[test]
    fn the_reported_backend_is_memoized_without_changing_the_answer() {
        let fresh = resolve_crc32_backend();
        assert_eq!(
            crc32_backend(),
            fresh,
            "the cached backend must equal a freshly resolved one"
        );

        for _ in 0..1_000 {
            assert_eq!(
                crc32_backend(),
                fresh,
                "the cache must not drift across repeated reads"
            );
            assert_eq!(
                resolve_crc32_backend(),
                fresh,
                "resolution itself must be deterministic"
            );
        }

        // And the checksum is untouched by any of it: the canonical vector still
        // holds after a thousand dispatch decisions.
        assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
    }

    /// Concurrent first use must be safe and unanimous.
    ///
    /// The cache is a relaxed load/store with no compare-exchange, which is sound
    /// only because every racing thread computes the *same* immutable value — so
    /// the worst outcome is redundant work, never a wrong answer. This asserts
    /// that property directly: many threads race to populate the cache while also
    /// computing checksums, and all of them must agree with each other and with
    /// the reference value.
    #[test]
    fn concurrent_first_use_of_the_backend_cache_is_unanimous() {
        const THREADS: usize = 16;
        const ROUNDS: usize = 200;

        let handles: alloc::vec::Vec<_> = (0..THREADS)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut seen = Crc32Backend::Braid;
                    let mut agreed = true;
                    for round in 0..ROUNDS {
                        let backend = crc32_backend();
                        if round == 0 {
                            seen = backend;
                        } else if backend != seen {
                            agreed = false;
                        }
                        assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
                    }
                    (seen, agreed)
                })
            })
            .collect();

        let expected = resolve_crc32_backend();
        for handle in handles {
            let (seen, agreed) = handle.join().expect("worker must not panic");
            assert!(agreed, "a thread observed the cache changing under it");
            assert_eq!(
                seen, expected,
                "every thread must observe the same immutable backend decision"
            );
        }
    }

    /// Whichever backend is selected, it must agree with the braided reference
    /// over every length that spans the sub-threshold path, the braid threshold,
    /// whole blocks, and the unrolled tail — including a non-zero seed.
    ///
    /// With `simd` on and the hardware path reachable, this is the cross-check
    /// between `crc32fast` and this module's own implementation; with `simd` off
    /// (or unreachable) it compares the braid with itself, which is trivially true
    /// but keeps the test meaningful in every feature row without a `cfg`.
    #[test]
    fn the_selected_backend_agrees_with_the_braid_for_every_length() {
        let mut data = [0u8; 700];
        let mut state = 0x1234_5678_9abc_def0_u64;
        for byte in data.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 24) as u8;
        }

        for len in 0..=data.len() {
            let slice = &data[..len];
            assert_eq!(
                crc32(0, slice),
                braid::crc32_bulk(0, slice),
                "backend {:?} disagrees with the braid at length {len}",
                crc32_backend()
            );
            assert_eq!(
                crc32(0xdead_beef, slice),
                braid::crc32_bulk(0xdead_beef, slice),
                "backend {:?} disagrees with the braid at length {len} for a \
                 non-zero seed",
                crc32_backend()
            );
        }
    }

    /// The canonical vectors must hold for the braid specifically, not only for
    /// whichever backend the host selects. Without this, a CI host that happens to
    /// select `crc32fast` would leave the shipped-elsewhere braid unvalidated by
    /// the value tests above.
    #[test]
    fn the_braid_reproduces_the_canonical_vectors_independently() {
        assert_eq!(braid::crc32_bulk(0, b""), 0);
        assert_eq!(braid::crc32_bulk(0, b"123456789"), 0xcbf4_3926);
        assert_eq!(braid::crc32_bulk(0, b"a"), 0xe8b7_be43);
        assert_eq!(braid::crc32_bulk(0, b"abc"), 0x3524_41c2);
        assert_eq!(
            braid::crc32_bulk(0, b"The quick brown fox jumps over the lazy dog"),
            0x414f_a339
        );
    }

    #[test]
    fn get_crc_table_reference_entries() {
        // Spot-check the generated table against the known reflected values.
        let table = get_crc_table();
        assert_eq!(table[0], 0x0000_0000);
        assert_eq!(table[1], 0x7707_3096);
        assert_eq!(table[2], 0xee0e_612c);
        assert_eq!(table[255], 0x2d02_ef8d);
    }

    #[test]
    fn combine_matches_concatenation() {
        let whole = b"The quick brown fox jumps over the lazy dog";
        let (a, b) = whole.split_at(20);
        let c1 = crc32(0, a);
        let c2 = crc32(0, b);
        assert_eq!(c1, 0x88b0_75e2);
        assert_eq!(c2, 0x1878_6794);
        assert_eq!(crc32_combine(c1, c2, b.len() as i64), crc32(0, whole));
        assert_eq!(crc32_combine(c1, c2, b.len() as i64), 0x414f_a339);
    }

    #[test]
    fn combine_large_matches_concatenation() {
        let mut big = [0u8; 20_000];
        for (i, byte) in big.iter_mut().enumerate() {
            *byte = ((i * 37 + 11) & 0xff) as u8;
        }
        let (a, b) = big.split_at(8_000);
        let combined = crc32_combine(crc32(0, a), crc32(0, b), b.len() as i64);
        assert_eq!(combined, crc32(0, &big));
        assert_eq!(combined, 0x897e_9e86);
    }

    #[test]
    fn combine_op_matches_combine() {
        // A precomputed operator must give the same result as crc32_combine.
        let whole = b"The quick brown fox jumps over the lazy dog";
        let (a, b) = whole.split_at(20);
        let c1 = crc32(0, a);
        let c2 = crc32(0, b);
        let len2 = b.len() as i64;
        let op = crc32_combine_gen(len2);
        assert_eq!(crc32_combine_op(c1, c2, op), crc32_combine(c1, c2, len2));
    }

    #[test]
    fn combine_zero_length_is_xor() {
        // crc32_combine_gen(0) == 0x8000_0000 (the multmodp identity), so
        // combining with a zero-length second sequence is exactly c1 ^ c2.
        let c1 = 0x88b0_75e2;
        let c2 = 0x1878_6794;
        assert_eq!(crc32_combine_gen(0), 0x8000_0000);
        assert_eq!(crc32_combine(c1, c2, 0), c1 ^ c2);
        assert_eq!(crc32_combine(c1, c2, 0), 0x90c8_1276);
    }

    #[test]
    fn combine_negative_length_returns_zero() {
        // A negative length has no meaning; the generator and both combiners
        // defensively return zero (matches reference zlib).
        assert_eq!(crc32_combine_gen(-1), 0);
        assert_eq!(crc32_combine(0x88b0_75e2, 0x1878_6794, -1), 0);
        assert_eq!(crc32_combine(1, 2, -12_345), 0);
    }

    #[test]
    fn combine_op_zero_operator_returns_zero() {
        // A zero operator is never produced for a valid length, so it is
        // treated as an error and yields zero.
        assert_eq!(crc32_combine_op(0x1234_5678, 0x9abc_def0, 0), 0);
    }

    // -----------------------------------------------------------------------
    // Virtual 64-bit combine-length coverage.
    //
    // `crc32_combine_gen(len2)` is `x^(8 * len2) mod P`, computed by
    // `x2nmodp` as a walk over the bits of `len2` multiplying in successive
    // entries of the build-time `X2N_TABLE`. Every combine test above uses a
    // length below `2^15`, so it touches only the first few table entries: a
    // 32-bit truncation of the length, a wrong squaring step, or a mis-ordered
    // shift in the bit walk would pass all of them. The anchors below were
    // produced by `crc32_combine_gen64` and `crc32_combine64` in reference zlib
    // 1.3.2.1-motley, compiled from the C sources retained in this repository
    // with `-D_LARGEFILE64_SOURCE=1`.
    // -----------------------------------------------------------------------

    /// First of the two CRC-32 values every 64-bit anchor combines: the CRCs of
    /// the halves of "The quick brown fox jumps over the lazy dog" split at 20,
    /// themselves pinned by `combine_matches_concatenation` above.
    const CC1: u32 = 0x88b0_75e2;
    /// Second half of the anchor pair; see [`CC1`].
    const CC2: u32 = 0x1878_6794;

    /// `(len2, reference crc32_combine_gen64(len2), reference
    /// crc32_combine64(CC1, CC2, len2))` sampled across the whole non-negative
    /// `i64` domain: zero, one, the real 22/23-byte split lengths, either side of
    /// the Adler modulus (a length with no special meaning here, kept so both
    /// checksum files probe the same points), every power of two from `2^28` to
    /// `2^62`, either side of `2^32`, and both ends of `i64::MAX`.
    const CRC_COMBINE_ANCHORS: [(i64, u32, u32); 21] = [
        (0, 0x8000_0000, 0x90c8_1276),
        (1, 0x0080_0000, 0x56f4_54b5),
        (22, 0x9a11_d850, 0x9c81_dde8),
        (23, 0x6bf1_402c, 0x414f_a339),
        (65_520, 0x1383_5d16, 0x8131_0175),
        (65_521, 0xf4c7_360c, 0xcfec_fc1c),
        (65_522, 0x0942_8b1d, 0xfbcc_f81d),
        (131_041, 0xc621_80fe, 0xddd8_bdb0),
        (131_042, 0x5ac3_fe9b, 0x24be_239f),
        (1_i64 << 28, 0xc4e2_2c3c, 0x415a_eb5b),
        ((1_i64 << 29) - 1, 0xdf0b_66ea, 0x446f_e5f8),
        (1_i64 << 29, 0x4000_0000, 0x5c20_5d65),
        ((1_i64 << 29) + 1, 0x0040_0000, 0xd286_fd24),
        (1_i64 << 30, 0x2000_0000, 0xd7ec_f9cc),
        (1_i64 << 31, 0x0800_0000, 0x2b9d_4002),
        ((1_i64 << 32) - 1, 0x8000_0000, 0x90c8_1276),
        (1_i64 << 32, 0x0080_0000, 0x56f4_54b5),
        ((1_i64 << 32) + 1, 0x0000_8000, 0x545f_fbf9),
        (1_i64 << 62, 0x2000_0000, 0xd7ec_f9cc),
        (i64::MAX - 1, 0xe0c9_a615, 0x4157_0bdb),
        (i64::MAX, 0x6d3d_2d4d, 0xfe42_14f9),
    ];

    #[test]
    fn combine_gen_at_bit_boundaries_matches_reference() {
        for (len2, expected_gen, _) in CRC_COMBINE_ANCHORS {
            assert_eq!(
                crc32_combine_gen(len2),
                expected_gen,
                "crc32_combine_gen at len2={len2}"
            );
        }

        // The 29-bit exponent boundary and `i64::MAX`, spelled out: these are the
        // two points at which a truncated length or a wrong squaring step in the
        // `X2N_TABLE` walk becomes visible.
        assert_eq!(crc32_combine_gen(1_i64 << 29), 0x4000_0000);
        assert_eq!(crc32_combine_gen(i64::MAX), 0x6d3d_2d4d);

        // Non-vacuity: a 32-bit truncation would map `2^32` to zero and
        // `i64::MAX` to `u32::MAX`, and neither collides with the true operator.
        assert_ne!(crc32_combine_gen(1_i64 << 32), crc32_combine_gen(0));
        assert_ne!(
            crc32_combine_gen(i64::MAX),
            crc32_combine_gen(i64::from(u32::MAX)),
            "a 32-bit truncation of len2 must be observable"
        );
    }

    #[test]
    fn generated_operators_drive_combine_equivalently() {
        // The operator produced for each boundary length is then *used*, so a
        // defect in `crc32_combine_gen` cannot hide behind `crc32_combine`'s own
        // arithmetic, nor the reverse: all three entry points must agree with the
        // same reference value at every length.
        for (len2, expected_gen, expected_combined) in CRC_COMBINE_ANCHORS {
            let op = crc32_combine_gen(len2);
            assert_eq!(op, expected_gen, "operator at len2={len2}");
            assert_ne!(op, 0, "a valid length never yields the error operator");
            assert_eq!(
                crc32_combine_op(CC1, CC2, op),
                expected_combined,
                "crc32_combine_op at len2={len2}"
            );
            assert_eq!(
                crc32_combine(CC1, CC2, len2),
                expected_combined,
                "crc32_combine at len2={len2}"
            );

            // `combine_op` is affine in `crc2` — a single trailing XOR — so the
            // zero-`crc2` form plus an external XOR must reproduce the same
            // value. This pins the matrix application separately from the XOR.
            assert_eq!(
                crc32_combine_op(CC1, 0, op) ^ CC2,
                expected_combined,
                "combine_op must be affine in crc2 at len2={len2}"
            );
        }
    }

    #[test]
    fn combine_gen_is_multiplicative_in_the_length() {
        // Because the operator for `len` is `x^(8 * len)`, the operator for a sum
        // of lengths is the product of the operators. Verifying that by repeated
        // doubling walks every `X2N_TABLE` entry a 63-bit length can reach, which
        // no single known-answer vector can do: one wrong squaring step or one
        // mis-ordered shift breaks the chain at that exponent and nowhere else.
        let mut len: i64 = 1;
        let mut op = crc32_combine_gen(len);
        while len <= i64::MAX / 2 {
            let doubled = crc32_combine_gen(len * 2);
            assert_eq!(
                doubled,
                multmodp(op, op),
                "operator for 2*{len} must be the square of the operator for {len}"
            );
            len *= 2;
            op = doubled;
        }
        assert_eq!(len, 1_i64 << 62, "the doubling chain must reach 2^62");

        // `i64::MAX` is the all-ones length, so the product of the single-bit
        // operators must reproduce the reference anchor exactly. `gen(0)` is
        // `x^0`, the identity for `multmodp`.
        let mut product = crc32_combine_gen(0);
        for bit in 0..63 {
            product = multmodp(product, crc32_combine_gen(1_i64 << bit));
        }
        assert_eq!(product, crc32_combine_gen(i64::MAX));
        assert_eq!(product, 0x6d3d_2d4d);

        // Additivity on two arbitrary large unequal lengths whose sum does not
        // overflow, so the property is not an artifact of powers of two.
        let a = 0x0123_4567_89ab_cdef_i64;
        let b = 0x0fed_cba9_8765_4321_i64;
        assert_eq!(
            crc32_combine_gen(a + b),
            multmodp(crc32_combine_gen(a), crc32_combine_gen(b))
        );
    }

    #[test]
    fn combine_real_split_around_base_matches_whole() {
        // Real concatenated data at the same three lengths the Adler-32 suite
        // sweeps, so both checksums are cross-validated at identical split
        // points. Expected values are reference zlib over `((i * 131 + 7) &
        // 0xff)`, and the generated operator is exercised alongside
        // `crc32_combine` on every case.
        const L1: usize = 1_000;
        /// CRC-32 of the fixed 1_000-byte first sequence.
        const C1: u32 = 0x1ed5_7bb9;
        let cases: [(usize, u32, u32); 3] = [
            (65_520, 0x60f4_472c, 0x63e9_e6de),
            (65_521, 0x9dbd_e998, 0xce0d_6709),
            (65_522, 0xdff2_385a, 0x58a9_2b06),
        ];

        for (l2, expected_c2, expected_whole) in cases {
            let data: Vec<u8> = (0..L1 + l2).map(|i| ((i * 131 + 7) & 0xff) as u8).collect();
            let (first, second) = data.split_at(L1);
            let c1 = crc32(0, first);
            let c2 = crc32(0, second);
            let whole = crc32(0, &data);
            assert_eq!(c1, C1, "first-sequence CRC at l2={l2}");
            assert_eq!(c2, expected_c2, "second-sequence CRC at l2={l2}");
            assert_eq!(whole, expected_whole, "whole-sequence CRC at l2={l2}");
            assert_eq!(
                crc32_combine(c1, c2, l2 as i64),
                whole,
                "combine must equal the whole-sequence CRC at l2={l2}"
            );
            assert_eq!(
                crc32_combine_op(c1, c2, crc32_combine_gen(l2 as i64)),
                whole,
                "the generated operator must reproduce the whole-sequence CRC at l2={l2}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Generated-table contract (`build.rs` -> `${OUT_DIR}/crc32_tables.rs`).
    //
    // The `const _` block near the top of this file pins the *schema* — that all
    // seven promised items exist with the promised element types and dimensions —
    // at compile time. These tests pin the *values*: anchor entries, the
    // byte-swap relation that defines the big-endian companions, the
    // endian-selected anchors, and the link from the generated `X2N_TABLE` to the
    // public combine generator. Together they mean a producer change cannot reach
    // a release without either failing to compile or failing a test.
    // -----------------------------------------------------------------------

    /// The consumer's half of the generated contract must stay strong enough to
    /// detect producer drift.
    ///
    /// The value and dimension checks elsewhere in this module cannot protect
    /// *themselves*: an unconditional `#[allow(dead_code)]` on `mod tables` would
    /// mask five of the seven outputs going unused, and deleting an assertion from
    /// the `const _` schema block cannot be caught by the block it was deleted
    /// from. This test therefore reads this file's own source and pins the shape of
    /// the contract, so weakening it is a test failure rather than a silent loss of
    /// coverage.
    ///
    /// NO suppression is permitted on `mod tables`, in any form — not even a
    /// `cfg_attr`-conditional one. The braided word-at-a-time path is compiled in
    /// every feature row (with `simd` on it is the fallback taken when
    /// `crc32fast`'s hardware backend is unreachable), so all seven generated
    /// outputs are consumed in every configuration and the compiler itself proves
    /// nothing `build.rs` emits has gone unused. An allowance — conditional or
    /// not — would remove exactly that signal, so the attribute list above the
    /// declaration must be empty.
    #[test]
    fn the_generated_contract_cannot_be_silently_weakened() {
        const SRC: &str = include_str!("crc32.rs");
        const NAMES: [&str; 7] = [
            "CRC_BRAID_N",
            "CRC_BRAID_W",
            "CRC_TABLE",
            "X2N_TABLE",
            "CRC_BIG_TABLE",
            "CRC_BRAID_TABLE",
            "CRC_BRAID_BIG_TABLE",
        ];

        // No blanket suppression, at any scope. The needles are assembled at run
        // time so this test's own source cannot satisfy them.
        let inner_allow = format!("#!{}", "[allow(dead_code)]");
        assert!(
            !SRC.contains(&inner_allow),
            "an inner allow(dead_code) would suppress the whole module"
        );
        // `mod tables` must carry NO outer attribute at all. Checking
        // structurally — every attribute line above the declaration — rather than
        // matching one forbidden spelling, so `#[allow(dead_code)]`,
        // `#[allow(dead_code, unused)]`, and any `cfg_attr`-conditional variant
        // are all rejected.
        let lines: Vec<&str> = SRC.lines().collect();
        let decl = lines
            .iter()
            .position(|l| l.trim_end() == "mod tables {")
            .expect("the generated file must still be included through `mod tables`");
        let attrs: Vec<&str> = lines[..decl]
            .iter()
            .rev()
            .take_while(|l| l.trim_start().starts_with('#') || l.trim().is_empty())
            .filter(|l| l.trim_start().starts_with('#'))
            .copied()
            .collect();
        assert!(
            attrs.is_empty(),
            "`mod tables` must carry no attribute: every generated output is \
             consumed in every feature row, so any suppression — conditional or \
             not — would mask producer drift instead of reporting it. Found \
             {attrs:?}"
        );

        // The other half of the same contract: the braided path that consumes five
        // of the seven outputs must exist AND must be compiled unconditionally,
        // otherwise a feature row would exist in which those tables are dead and
        // the absent allowance above would stop being provable.
        let decl_braid = lines
            .iter()
            .position(|l| l.trim_end() == "mod braid {")
            .expect("the braided word-at-a-time path must exist to consume the braid tables");
        let braid_attrs: Vec<&str> = lines[..decl_braid]
            .iter()
            .rev()
            .take_while(|l| {
                let t = l.trim_start();
                t.starts_with('#') || t.starts_with("///") || l.trim().is_empty()
            })
            .filter(|l| l.trim_start().starts_with('#'))
            .copied()
            .collect();
        assert!(
            braid_attrs.is_empty(),
            "`mod braid` must be compiled unconditionally so the braid tables are \
             live in every feature row; found {braid_attrs:?}"
        );

        // Every promised name is imported, so a renamed or removed output is an
        // unresolved-import error rather than a silently unused table.
        let import_start = SRC
            .find("use tables::{")
            .expect("the generated outputs must be imported by name");
        let import_end = import_start
            + SRC[import_start..]
                .find("};")
                .expect("unterminated import list");
        let imports = &SRC[import_start..import_end];
        for name in NAMES {
            assert!(
                imports.contains(name),
                "{name} must appear in the `use tables::{{..}}` import list"
            );
        }

        // The compile-time schema block must still pin every name. Its first
        // occurrence is the declaration; later ones are this test's own literals.
        let block_start = SRC
            .find("const _: () = {")
            .expect("the compile-time schema contract must exist");
        let block_end = block_start
            + SRC[block_start..]
                .find("\n};")
                .expect("unterminated schema contract block");
        let block = &SRC[block_start..block_end];
        for name in NAMES {
            assert!(block.contains(name), "the schema contract must pin {name}");
        }
        assert!(
            block.matches("assert!(").count() >= 9,
            "the schema contract must retain an assertion per output plus the two \
             braid row-length checks, found {}",
            block.matches("assert!(").count()
        );
        // Exact-value pins, not inequalities: `CRC_BRAID_W >= 1` would accept any
        // depth and let the braid tables be resized underneath the consumer.
        assert!(block.contains("CRC_BRAID_N == 5"));
        assert!(block.contains("CRC_BRAID_W == 8"));
    }

    #[test]
    fn generated_tables_have_the_promised_dimensions_and_anchors() {
        // Two scalar constants: the braid geometry `crc32.c`'s `braid()` is
        // parameterised with (`N` streams over `W`-byte words).
        assert_eq!(CRC_BRAID_N, 5, "CRC_BRAID_N");
        assert_eq!(CRC_BRAID_W, 8, "CRC_BRAID_W");

        // Byte-wise table: the canonical reflected CRC-32 table. Entry `i` is the
        // CRC of the single byte `i`, so entry 0 is 0 and entry 1 is the
        // universally quoted `0x77073096`.
        assert_eq!(CRC_TABLE.len(), 256, "CRC_TABLE length");
        assert_eq!(CRC_TABLE[0], 0x0000_0000, "CRC_TABLE[0]");
        assert_eq!(CRC_TABLE[1], 0x7707_3096, "CRC_TABLE[1]");
        assert_eq!(CRC_TABLE[2], 0xee0e_612c, "CRC_TABLE[2]");
        assert_eq!(CRC_TABLE[3], 0x9909_51ba, "CRC_TABLE[3]");
        assert_eq!(CRC_TABLE[255], 0x2d02_ef8d, "CRC_TABLE[255]");

        // Powers-of-x table: entry 0 is x^1 and each entry is the previous one
        // squared, so the sequence starts 2^30, 2^29, 2^27, 2^23, 2^15 in the
        // reflected representation.
        assert_eq!(X2N_TABLE.len(), 32, "X2N_TABLE length");
        assert_eq!(X2N_TABLE[0], 0x4000_0000, "X2N_TABLE[0] == x^1");
        assert_eq!(X2N_TABLE[1], 0x2000_0000, "X2N_TABLE[1]");
        assert_eq!(X2N_TABLE[2], 0x0800_0000, "X2N_TABLE[2]");
        assert_eq!(X2N_TABLE[3], 0x0080_0000, "X2N_TABLE[3]");
        assert_eq!(X2N_TABLE[4], 0x0000_8000, "X2N_TABLE[4]");
        assert_eq!(X2N_TABLE[31], 0xc4e2_2c3c, "X2N_TABLE[31]");

        // Byte-swapped companion of the byte-wise table.
        assert_eq!(CRC_BIG_TABLE.len(), 256, "CRC_BIG_TABLE length");
        assert_eq!(CRC_BIG_TABLE[0], 0, "CRC_BIG_TABLE[0]");
        assert_eq!(CRC_BIG_TABLE[1], 0x9630_0777_0000_0000, "CRC_BIG_TABLE[1]");
        assert_eq!(
            CRC_BIG_TABLE[255], 0x8def_022d_0000_0000,
            "CRC_BIG_TABLE[255]"
        );

        // Braid tables: `CRC_BRAID_W` sub-tables of 256 entries each.
        assert_eq!(CRC_BRAID_TABLE.len(), CRC_BRAID_W, "braid table rows");
        assert_eq!(CRC_BRAID_BIG_TABLE.len(), CRC_BRAID_W, "big braid rows");
        for k in 0..CRC_BRAID_W {
            assert_eq!(CRC_BRAID_TABLE[k].len(), 256, "braid row {k} length");
            assert_eq!(CRC_BRAID_BIG_TABLE[k].len(), 256, "big braid row {k}");
            // Entry 0 of every sub-table is the zero contribution.
            assert_eq!(CRC_BRAID_TABLE[k][0], 0, "braid[{k}][0]");
            assert_eq!(CRC_BRAID_BIG_TABLE[k][0], 0, "big braid[{k}][0]");
        }
        assert_eq!(CRC_BRAID_TABLE[0][1], 0xaf44_9247, "braid[0][1]");
        assert_eq!(CRC_BRAID_TABLE[7][1], 0x36f2_90f3, "braid[7][1]");
        assert_eq!(CRC_BRAID_TABLE[7][255], 0xf437_7108, "braid[7][255]");
        assert_eq!(
            CRC_BRAID_BIG_TABLE[0][1], 0xf390_f236_0000_0000,
            "big braid[0][1]"
        );
        assert_eq!(
            CRC_BRAID_BIG_TABLE[7][1], 0x4792_44af_0000_0000,
            "big braid[7][1]"
        );
        assert_eq!(
            CRC_BRAID_BIG_TABLE[7][255], 0x6575_94e9_0000_0000,
            "big braid[7][255]"
        );
    }

    #[test]
    fn generated_big_endian_tables_are_byte_swapped_companions() {
        // `crc32.c`'s `make_crc_table()` stores `big[i] = byte_swap(crc_table[i])`
        // and `braid()` stores `big[w - 1 - k][i] = byte_swap(ltl[k][i])` — note
        // the *reversed* sub-table order, which is what lets the big-endian
        // word-at-a-time loop index the braid identically. Checking every entry
        // rather than a sample means a single mis-swapped or mis-placed word
        // cannot slip through.
        for i in 0..256 {
            assert_eq!(
                CRC_BIG_TABLE[i],
                u64::from(CRC_TABLE[i]).swap_bytes(),
                "CRC_BIG_TABLE[{i}] must be the byte-swapped CRC_TABLE entry"
            );
        }
        for k in 0..CRC_BRAID_W {
            for i in 0..256 {
                assert_eq!(
                    CRC_BRAID_BIG_TABLE[CRC_BRAID_W - 1 - k][i],
                    u64::from(CRC_BRAID_TABLE[k][i]).swap_bytes(),
                    "big braid[{}][{i}] must be the byte-swapped braid[{k}][{i}]",
                    CRC_BRAID_W - 1 - k
                );
            }
        }
    }

    #[test]
    fn endian_selected_generated_anchors_are_the_active_target_values() {
        // The expectations are `cfg`-selected exactly like the anchors they check,
        // so a target for which neither arm applied would fail to compile rather
        // than pass vacuously, and a little-endian host can never silently assert
        // a big-endian value.
        #[cfg(target_endian = "little")]
        {
            assert_eq!(
                SELECTED_CRC_ENTRY_1, 0x7707_3096,
                "a little-endian target selects the native-order CRC table"
            );
            assert_eq!(
                SELECTED_BRAID_ENTRY_1, 0xaf44_9247,
                "a little-endian target selects CRC_BRAID_TABLE row 0"
            );
        }
        #[cfg(target_endian = "big")]
        {
            assert_eq!(
                SELECTED_CRC_ENTRY_1, 0x9630_0777_0000_0000,
                "a big-endian target selects the byte-swapped CRC table"
            );
            assert_eq!(
                SELECTED_BRAID_ENTRY_1, 0x4792_44af_0000_0000,
                "a big-endian target selects the last CRC_BRAID_BIG_TABLE row"
            );
        }

        // Both endian forms are asserted on *every* target, not just the one that
        // matches the host. An anchor written only inside a
        // `#[cfg(target_endian = "big")]` block is never compiled on a
        // little-endian host, so a wrong index or a wrong table in it would stay
        // invisible until someone built for big-endian hardware. Checking the
        // relationship between the two forms instead makes both anchors live on
        // every host, and CI compiles the library for `s390x-unknown-linux-gnu`
        // (the `build-script-tests` job) so the big-endian `cfg` arms are
        // type-checked as well.
        //
        // What neither covers is *running* the braid loops on a big-endian target:
        // no CI job executes on such hardware, so the residual gap is execution,
        // not selection (AAP §0.6.6 residual risk, §0.7.2 standard S8).
        assert_eq!(
            LITTLE_CRC_ENTRY_1, 0x7707_3096,
            "the little-endian CRC anchor is CRC_TABLE[1]"
        );
        assert_eq!(
            BIG_CRC_ENTRY_1, 0x9630_0777_0000_0000,
            "the big-endian CRC anchor is CRC_BIG_TABLE[1]"
        );
        assert_eq!(
            BIG_CRC_ENTRY_1,
            LITTLE_CRC_ENTRY_1.swap_bytes(),
            "the two CRC anchors must be byte-swapped views of one value"
        );

        assert_eq!(
            LITTLE_BRAID_ENTRY_1, 0xaf44_9247,
            "the little-endian braid anchor is CRC_BRAID_TABLE[0][1]"
        );
        assert_eq!(
            BIG_BRAID_ENTRY_1, 0x4792_44af_0000_0000,
            "the big-endian braid anchor is CRC_BRAID_BIG_TABLE[CRC_BRAID_W - 1][1]"
        );
        assert_eq!(
            BIG_BRAID_ENTRY_1,
            LITTLE_BRAID_ENTRY_1.swap_bytes(),
            "the big-endian braid rows are stored in reverse position order, so \
             row CRC_BRAID_W - 1 is the byte-swapped view of row 0 — indexing the \
             same row number in both tables would be wrong"
        );

        // Which of the two forms the target selects is pinned by the `cfg` blocks
        // above; a further "it is one of the two" assertion would fold to `true`
        // at compile time and prove nothing, so it is deliberately absent.
    }

    #[test]
    fn generated_tables_back_the_public_entry_points() {
        // `get_crc_table()` hands out the generated byte-wise table itself, not a
        // copy, so the published table and the one the checksum loop reads cannot
        // diverge.
        assert!(
            core::ptr::eq(get_crc_table(), &CRC_TABLE),
            "get_crc_table must return the generated table"
        );

        // `x2nmodp(n, 3)` multiplies in `X2N_TABLE[(3 + bit) & 31]` for each set
        // bit of `n`, starting from the identity, so a single-bit length reduces
        // to exactly one table entry. Both anchors below were confirmed against
        // reference C zlib, which ties the generated table to observable API
        // behaviour instead of only to itself.
        assert_eq!(
            crc32_combine_gen(1),
            X2N_TABLE[3],
            "combine_gen(1) must be X2N_TABLE[3]"
        );
        assert_eq!(crc32_combine_gen(1), 0x0080_0000);
        assert_eq!(
            crc32_combine_gen(2),
            X2N_TABLE[4],
            "combine_gen(2) must be X2N_TABLE[4]"
        );
        assert_eq!(
            crc32_combine_gen(1_i64 << 28),
            X2N_TABLE[31],
            "combine_gen(2^28) must be X2N_TABLE[31]"
        );
        assert_eq!(crc32_combine_gen(1_i64 << 28), 0xc4e2_2c3c);

        // The byte-wise table drives the scalar checksum loop, so the canonical
        // single-byte CRCs must fall straight out of it.
        for byte in [0u8, 1, 2, 0x7f, 0xff] {
            // crc32(0, [b]) == CRC_TABLE[(0xffff_ffff ^ b) & 0xff] ^ (0xffff_ffff >> 8),
            // then complemented — the one-step form of the reflected update.
            let idx = usize::from(byte ^ 0xff);
            let expected = !(CRC_TABLE[idx] ^ 0x00ff_ffff);
            assert_eq!(
                crc32(0, &[byte]),
                expected,
                "single-byte CRC of {byte:#04x}"
            );
        }
    }
}
