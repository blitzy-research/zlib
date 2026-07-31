//! # `zlib-rs` — a safe-Rust reimplementation of zlib
//!
//! `zlib-rs` is an idiomatic, memory-safe Rust port of zlib `1.3.2.1-motley`
//! ([`ZLIB_VERNUM`] `0x1321`). It reproduces the DEFLATE (RFC 1951), zlib
//! (RFC 1950), and gzip (RFC 1952) stream formats with **byte-for-byte** output
//! compatibility with the reference C library, and exposes a C-compatible FFI so
//! the emitted `cdylib`/`staticlib` (`libzlib_rs.so` / `libzlib_rs.a`) can drop
//! in for the system `libz`.
//!
//! ## Safe core, unsafe boundary
//!
//! The crate is organized as a **safe core** wrapped by a thin **unsafe
//! boundary**. Every compression, decompression, checksum, and one-call engine
//! is written in fully safe Rust — the deflate engine ([`deflate`]) contains
//! **zero** `unsafe` — while all C-ABI raw-pointer handling is confined to the
//! [`ffi`] module (and, at most, the inflate fast-path decode loop). The manual
//! `zcalloc`/`zcfree` memory management of the C sources is replaced by Rust
//! ownership: a [`ZStream`] owns its boxed engine state and releases it in
//! [`Drop`], so the "uninitialized-state pointer" hazards of the C API become
//! unrepresentable.
//!
//! ## Architecture
//!
//! The crate mirrors the six functional layers of the C baseline as modules,
//! adds a public-API-types layer, and isolates the C ABI behind [`ffi`]:
//!
//! | Module        | Ported from (C)                           | Responsibility                              |
//! |---------------|-------------------------------------------|---------------------------------------------|
//! | [`error`]     | `zutil.h`                                 | [`ReturnCode`] / [`ZlibError`] error model  |
//! | [`constants`] | `zlib.h`                                  | flush modes, levels, strategies, wrap modes |
//! | [`stream`]    | `zlib.h`                                  | idiomatic [`ZStream`] + [`Allocator`]       |
//! | [`gz_header`] | `zlib.h`                                  | idiomatic [`GzHeader`]                      |
//! | [`checksum`]  | `adler32.c`, `crc32.c`                    | Adler-32 and CRC-32                         |
//! | [`deflate`]   | `deflate.c`, `trees.c`                    | compression engine (zero `unsafe`)          |
//! | [`inflate`]   | `inflate.c`, `inffast.c`, `infback.c`, …  | decompression engine incl. `inflateBack`    |
//! | [`util`]      | `compress.c`, `uncompr.c`, `zutil.c`      | one-call wrappers and version reporting     |
//! | `gz`          | `gzlib.c`, `gzread.c`, `gzwrite.c`, …     | gzip file-I/O (Cargo feature `gz-io`)       |
//! | [`ffi`]       | `zlib.h`, `zconf.h`, `zlib.map`           | `extern "C"` drop-in boundary               |
//!
//! ## `no_std`
//!
//! The crate is `#![no_std]` + [`alloc`] whenever the `std` feature is disabled,
//! so the compression core can be embedded in freestanding environments. The
//! `std` feature (enabled by default) links the standard library for the gz
//! file-I/O layer ([`std::fs`]/[`std::io`]) and for `catch_unwind` panic guards
//! at the FFI boundary. Core modules use [`alloc`] types ([`alloc::boxed::Box`],
//! [`alloc::vec::Vec`], [`alloc::string::String`]) rather than their `std`
//! re-exports, so the whole crate participates in `no_std` builds.
//!
//! ## Feature flags
//!
//! | Feature          | Default | Effect                                                       |
//! |------------------|:-------:|--------------------------------------------------------------|
//! | `std`            |   yes   | Standard-library build (file I/O, `catch_unwind` guards).    |
//! | `gzip`           |   yes   | gzip framing within deflate/inflate (mirrors C `#ifdef GZIP`). |
//! | `gz-io`          |   yes   | gzip file-I/O `gz*` API; implies `std` + `gzip`.             |
//! | `simd`           |   yes   | SIMD-accelerated CRC-32 via the `crc32fast` crate.          |
//! | `inflate_strict` |   no    | Stricter inflate distance validation (mirrors C `INFLATE_STRICT`). |
//!
//! Building with `--no-default-features` yields the `no_std` compression and
//! decompression core without the gz file-I/O layer (the `gz` module is gated
//! behind `gz-io`, which implies `std`).
//!
//! ## Quick start
//!
//! ```
//! use zlib_rs::{compress2, compress_bound, uncompress, Z_BEST_COMPRESSION};
//!
//! let source = b"the quick brown fox jumps over the lazy dog";
//!
//! // Size the destination with the same bound formula the C library uses.
//! let mut compressed = vec![0u8; compress_bound(source.len())];
//! let produced = compress2(&mut compressed, source, Z_BEST_COMPRESSION).unwrap();
//! compressed.truncate(produced);
//!
//! // Decompress into a buffer sized to the known original length.
//! let mut restored = vec![0u8; source.len()];
//! let written = uncompress(&mut restored, &compressed).unwrap();
//! restored.truncate(written);
//!
//! assert_eq!(&restored[..], &source[..]);
//! ```

// The core engines are `no_std` + `alloc`; the standard library is linked only
// under the `std` feature (the default). Because the FFI layer and several
// engines allocate (`Box`/`Vec`), `alloc` is imported unconditionally below.
//
// `no_std` is applied only for a genuine freestanding build, identified by the
// conjunction of three stable predicates:
//   * `not(feature = "std")` — the default `std` feature is off.
//   * `panic = "abort"`      — the crate is compiled with the abort panic
//     strategy. On the STABLE toolchain (MSRV 1.85.0, no `-Z build-std`) a
//     `#![no_std]` crate CANNOT be codegen'd with `panic = "unwind"`: the
//     compiler rejects it with "unwinding panics are not supported without
//     std". `[profile.dev]`/`[profile.release]` set `panic = "abort"` precisely
//     so the no_std `cdylib`/`staticlib` link (`cargo build
//     --no-default-features`). But `cargo test` forces `panic = "unwind"`
//     (libtest needs `catch_unwind`) AND still builds every `crate-type` of the
//     lib target — including the `cdylib`/`staticlib`. Gating `no_std` on
//     `panic = "abort"` therefore keeps the crate `std`-linked under
//     `cargo test --no-default-features` (so those artifacts and the integration
//     tests / doctests build), while `cargo build --no-default-features` (abort)
//     still yields the real freestanding artifact. The `feature = "std"` code
//     gates stay OFF regardless, so `cargo test --no-default-features` genuinely
//     exercises the `no_std`-configured code paths. `cfg(panic = ...)` is stable
//     since Rust 1.60. See the `no_std_support` gate below (identical
//     predicate) and the `src/util/compress.rs` test module.
//   * `not(test)`            — belt-and-braces exclusion of the unit-test
//     harness (which links `std`); redundant given `panic = "abort"` on stable,
//     but documents intent and guards custom profiles.
#![cfg_attr(all(not(feature = "std"), not(test), panic = "abort"), no_std)]
// Every public item in the crate must be documented (AAP: "document all public
// items"). This is a `warn`, never a `deny`, so it can never break the build.
#![warn(missing_docs)]
// Every `unsafe` block in SHIPPED crate code must carry an immediately-adjacent
// `// SAFETY:` justification (AAP §0.7.2 standard S2 / User Constraint 3). Like
// `missing_docs` this is a `warn` (never a `deny`) so it cannot break a plain
// build, but the CI `-D warnings` gate promotes it to an error for the
// production library target, preventing recurrence of the
// undocumented-`unsafe` finding across the FFI boundary. The lint is relaxed
// to `allow` under `cfg(test)` so it governs only the shipped
// `cdylib`/`staticlib`/`rlib` (whose `unsafe` lives in `src/ffi/**`), not the
// crate's inline `#[cfg(test)]` unit tests.
#![warn(clippy::undocumented_unsafe_blocks)]
#![cfg_attr(test, allow(clippy::undocumented_unsafe_blocks))]
// `unsafe` is DENIED crate-wide, converting the migration's unsafe-containment
// strategy (AAP §0.3.2 pattern C7 / §0.6.2 / §0.7.2 standard S2, satisfying User
// Constraint 3 "zero unsafe blocks in core compression logic") from an
// architectural convention plus a lint-assisted review check into a HARD COMPILE
// ERROR. A stray `unsafe` block, `unsafe fn`, `unsafe impl`, or `unsafe extern`
// anywhere in `src/deflate/**`, `src/inflate/**`, `src/checksum/**`,
// `src/gz/**`, `src/util/**`, `src/stream.rs`, `src/error.rs`,
// `src/constants.rs`, or `src/gz_header.rs` now fails the build outright rather
// than surviving until code review.
//
// Exactly TWO carve-outs exist, and both are the designated boundaries the AAP
// names:
//   1. `mod no_std_support` below — the private `libc`-backed global allocator
//      and abort panic handler that a freestanding `cdylib`/`staticlib` must
//      supply; and
//   2. `pub mod ffi` — the C ABI drop-in surface.
// Each carries a narrowly scoped `#[allow(unsafe_code)]` at its declaration, so
// the permission is granted per-module rather than crate-wide.
//
// `deny` (not `forbid`) is deliberate: `forbid` cannot be relaxed by an inner
// `allow`, which would make the two boundary carve-outs impossible to express.
#![deny(unsafe_code)]

// `Box`, `Vec`, and `String` are provided by `alloc` in both `std` and `no_std`
// builds. Importing the crate here makes those types available crate-wide
// without pulling in the whole standard library.
extern crate alloc;

// ===========================================================================
// `no_std` runtime support — global allocator + panic handler
//
// A `#![no_std]` crate that still allocates (this one uses `Box`/`Vec`/`String`
// via `alloc`) and is emitted as a `cdylib`/`staticlib` must SUPPLY its own
// `#[global_allocator]` and `#[panic_handler]`: the standard library normally
// provides both, and without `std` the final `cdylib`/`staticlib` link step
// fails with "no global memory allocator found" and "`#[panic_handler]`
// function required, but not found" (see QA finding on the `no-std` build).
//
// These items are compiled ONLY for a genuine freestanding library build —
// `#[cfg(all(not(feature = "std"), not(test), panic = "abort"))]`, the SAME
// predicate that applies `#![no_std]` above (see the detailed rationale there):
//   * `not(feature = "std")`  — under the default (`std`) build the standard
//     library already provides the global allocator and panic handler, and
//     redefining them here would be a duplicate-lang-item error. The whole std
//     surface therefore stays byte-for-byte unchanged.
//   * `panic = "abort"`       — under `cargo test` the crate is `std`-linked
//     (panic = "unwind"; see the crate attribute above), so `std` already
//     supplies the allocator and panic handler; defining them here would then
//     collide. This predicate keeps these items in lockstep with `#![no_std]`.
//   * `not(test)`             — the unit-test harness links `libtest`, which
//     pulls in `std`; excluding the items from test builds avoids clashing with
//     the std-provided ones.
//
// The allocator is a thin, faithful port of the standard library's Unix
// `System` allocator: the platform `malloc`/`calloc`/`realloc`/`free` satisfy
// the common (≤ `MIN_ALIGN`) case, and `posix_memalign` covers over-aligned
// requests. Routing through the C runtime's `malloc` family is exactly what a
// C consumer of the emitted `libzlib_rs.{so,a}` expects, and AAP §0.5.2
// explicitly permits the platform `libc` already linked by any hosted artifact
// (it introduces no *additional* C dependency and keeps the shipped Rust graph
// pure). Per-`z_stream` `zalloc`/`zfree` caller hooks remain a separate concern
// handled inside the FFI layer; this global allocator is only the fallback the
// `alloc` crate requires.
//
// SAFETY: this module is crate-root C-ABI plumbing, analogous to the FFI
// boundary carve-out in AAP §0.6.2 — it is NOT part of the compression core
// (`src/deflate/**` remains 100% `unsafe`-free, honoring the "zero unsafe in
// core compression logic" rule). Every `unsafe` operation is justified inline.
#[cfg(all(not(feature = "std"), not(test), panic = "abort"))]
// Carve-out 1 of 2 for the crate-wide `#![deny(unsafe_code)]` above. This module
// is the freestanding runtime-support plumbing a `no_std` `cdylib`/`staticlib`
// must supply (`libc` allocator + abort panic handler); it is NOT part of the
// compression core, and the scope of this allow is exactly this module.
#[allow(unsafe_code)]
mod no_std_support {
    use core::alloc::{GlobalAlloc, Layout};
    use core::ffi::c_void;

    // Minimum alignment guaranteed by the platform `malloc` family: 16 bytes on
    // 64-bit targets, 8 on 32-bit. This mirrors the constant the standard
    // library's Unix allocator uses.
    #[cfg(target_pointer_width = "64")]
    const MIN_ALIGN: usize = 16;
    #[cfg(not(target_pointer_width = "64"))]
    const MIN_ALIGN: usize = 8;

    // The C runtime allocation primitives. These are resolved against the
    // platform `libc` that every hosted `cdylib`/`staticlib` links against, so
    // declaring them here adds no new dependency to the shipped Rust graph.
    unsafe extern "C" {
        fn malloc(size: usize) -> *mut c_void;
        fn calloc(nmemb: usize, size: usize) -> *mut c_void;
        fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
        fn posix_memalign(memptr: *mut *mut c_void, align: usize, size: usize) -> i32;
        fn abort() -> !;
    }

    /// A `GlobalAlloc` implementation backed by the platform `libc` allocator.
    ///
    /// This is a faithful port of the standard library's Unix `System`
    /// allocator so that a `no_std` build behaves identically to the default
    /// `std` build with respect to heap allocation.
    struct LibcAllocator;

    /// Allocate an over-aligned block via `posix_memalign`.
    ///
    /// # Safety
    /// The caller must free the returned pointer (when non-null) with `free`,
    /// and must not request a zero-sized layout (the `GlobalAlloc` contract
    /// already guarantees `layout.size() > 0`).
    unsafe fn aligned_malloc(layout: Layout) -> *mut u8 {
        // `posix_memalign` requires the alignment to be a power of two and a
        // multiple of `size_of::<*mut c_void>()`; clamp up to that minimum.
        let align = layout.align().max(core::mem::size_of::<usize>());
        let mut out: *mut c_void = core::ptr::null_mut();
        // SAFETY: `out` is a valid pointer to a `*mut c_void` slot; `align` is a
        // power-of-two multiple of the pointer size as required.
        let ret = unsafe { posix_memalign(&mut out, align, layout.size()) };
        if ret != 0 {
            core::ptr::null_mut()
        } else {
            out as *mut u8
        }
    }

    // SAFETY: `LibcAllocator` forwards every request to the C runtime allocator,
    // which upholds the `GlobalAlloc` contract (returns suitably aligned blocks
    // or null, and `free`/`realloc` operate on pointers it previously handed
    // out). Over-aligned requests are routed through `posix_memalign`, whose
    // allocations are also freeable with `free`.
    unsafe impl GlobalAlloc for LibcAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if layout.align() <= MIN_ALIGN && layout.align() <= layout.size() {
                // SAFETY: `malloc` returns a `MIN_ALIGN`-aligned block that
                // satisfies this layout, or null on failure.
                unsafe { malloc(layout.size()) as *mut u8 }
            } else {
                // SAFETY: over-aligned path; `aligned_malloc` honors the layout.
                unsafe { aligned_malloc(layout) }
            }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if layout.align() <= MIN_ALIGN && layout.align() <= layout.size() {
                // SAFETY: `calloc` returns zeroed, `MIN_ALIGN`-aligned memory.
                unsafe { calloc(layout.size(), 1) as *mut u8 }
            } else {
                // SAFETY: allocate over-aligned, then zero it explicitly.
                let ptr = unsafe { aligned_malloc(layout) };
                if !ptr.is_null() {
                    // SAFETY: `ptr` points to `layout.size()` writable bytes.
                    unsafe { core::ptr::write_bytes(ptr, 0, layout.size()) };
                }
                ptr
            }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            // SAFETY: `ptr` was returned by `alloc`/`alloc_zeroed`/`realloc`
            // above (all backed by the `malloc` family), so `free` is valid.
            unsafe { free(ptr as *mut c_void) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if layout.align() <= MIN_ALIGN && layout.align() <= new_size {
                // SAFETY: the original block came from the `malloc` family with
                // `MIN_ALIGN` alignment, so `realloc` preserves the layout.
                unsafe { realloc(ptr as *mut c_void, new_size) as *mut u8 }
            } else {
                // Over-aligned: `realloc` cannot preserve the alignment, so
                // allocate a fresh aligned block, copy, and free the old one.
                // SAFETY: `new_size` and `layout.align()` form a valid layout.
                let new_layout =
                    unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
                // SAFETY: delegates to the checked `alloc` above.
                let new_ptr = unsafe { self.alloc(new_layout) };
                if !new_ptr.is_null() {
                    let copy = core::cmp::min(layout.size(), new_size);
                    // SAFETY: both regions are valid for `copy` bytes and do not
                    // overlap (distinct allocations).
                    unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy) };
                    // SAFETY: `ptr` is a live allocation from this allocator.
                    unsafe { free(ptr as *mut c_void) };
                }
                new_ptr
            }
        }
    }

    #[global_allocator]
    static GLOBAL: LibcAllocator = LibcAllocator;

    // In a `no_std` build there is no unwinding runtime (the crate is compiled
    // with `panic = "abort"`; see `Cargo.toml`), so the panic handler simply
    // terminates the process via the C runtime's `abort()`. This matches the
    // `panic = "abort"` strategy and upholds the crate-wide invariant that a
    // Rust panic never unwinds across the C ABI.
    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo) -> ! {
        // SAFETY: `abort` is the libc process-termination routine; it never
        // returns and has no preconditions.
        unsafe { abort() }
    }
}

// ===========================================================================
// Version macros (ported from `zlib.h` L44-L49)
//
// These mirror the C `ZLIB_VERSION`/`ZLIB_VERNUM` family exactly and are the
// crate-wide single source of truth. `util::version::zlib_version()` (and the
// FFI `zlibVersion` shim) return `ZLIB_VERSION`, so this string must stay
// byte-identical to the C `ZLIB_VERSION` macro for compatibility.
// ===========================================================================

/// zlib version string, mirroring the C `ZLIB_VERSION` macro.
///
/// This is the authoritative version reported by [`util::version::zlib_version`]
/// (and its FFI shim `zlibVersion`); it must remain byte-identical to the C
/// `ZLIB_VERSION` macro for drop-in compatibility.
pub const ZLIB_VERSION: &str = "1.3.2.1-motley";

/// zlib version number, mirroring the C `ZLIB_VERNUM` macro (`0x1321`).
///
/// The nibbles encode the version as `0xMNRS` (major, minor, revision,
/// sub-revision): `0x1321` is `1.3.2` sub-revision `1`.
pub const ZLIB_VERNUM: u32 = 0x1321;

/// zlib major version, mirroring the C `ZLIB_VER_MAJOR` macro.
pub const ZLIB_VER_MAJOR: u32 = 1;

/// zlib minor version, mirroring the C `ZLIB_VER_MINOR` macro.
pub const ZLIB_VER_MINOR: u32 = 3;

/// zlib revision, mirroring the C `ZLIB_VER_REVISION` macro.
pub const ZLIB_VER_REVISION: u32 = 2;

/// zlib sub-revision, mirroring the C `ZLIB_VER_SUBREVISION` macro.
pub const ZLIB_VER_SUBREVISION: u32 = 1;

// ===========================================================================
// Module declarations — the six C layers + public-API-types + FFI boundary
//
// `lib.rs` only DECLARES these modules; each is implemented in its own file.
// The graph mirrors the C `#include` layering exactly (AAP §0.3.1) with no
// extra top-level modules.
// ===========================================================================

pub mod checksum;
pub mod constants;
pub mod deflate;
pub mod error;
pub mod gz_header;
pub mod inflate;
pub mod stream;
pub mod util;

// The gzip file-I/O layer fundamentally requires the standard library
// (`std::fs`/`std::io`), so it is compiled only when the `gz-io` feature is
// enabled (which implies `std` + `gzip`). This matches the feature gating in
// `src/gz/**` and `src/ffi/gz.rs`; a bare `no_std` build omits it entirely.
#[cfg(feature = "gz-io")]
pub mod gz;

// The FFI drop-in boundary is declared UNCONDITIONALLY so the emitted
// `cdylib`/`staticlib` always presents the full zlib C symbol table for
// linkage. Any part of `ffi` that needs `std` is gated internally; the module
// declaration itself is never feature-gated.
//
// Carve-out 2 of 2 for the crate-wide `#![deny(unsafe_code)]` above: this is the
// designated unsafe boundary (AAP §0.6.2). Reproducing the C ABI requires raw
// pointers, `extern "C"` entry points, and caller-supplied allocator hooks, and
// every such block carries a `// SAFETY:` justification. The scope of this allow
// is exactly the `ffi` module tree — it does not leak into the safe core.
#[allow(unsafe_code)]
pub mod ffi;

// ===========================================================================
// Idiomatic public API — curated crate-root re-exports
//
// These mirror the surface `zlib.h` exposes so that `use zlib_rs::{…}` is
// ergonomic. The FFI symbols are intentionally NOT re-exported here: the `ffi`
// module stands alone as the C ABI surface.
// ===========================================================================

// The error model: the exhaustive return-code enum and its error twin.
pub use error::{ReturnCode, ZlibError};

// Public-API enums plus the four compression-level constants callers most often
// reference when configuring the engine.
pub use constants::{
    DataType, FlushMode, Method, Strategy, WrapMode, Z_BEST_COMPRESSION, Z_BEST_SPEED,
    Z_DEFAULT_COMPRESSION, Z_NO_COMPRESSION,
};

// The idiomatic stream and its allocator abstraction (mirrors `z_stream`).
pub use stream::{Allocator, DefaultAllocator, ZStream};

// The idiomatic gzip header (mirrors `gz_header`).
pub use gz_header::GzHeader;

// One-call, whole-buffer wrappers (ported from `compress.c`/`uncompr.c`). Both
// the idiomatic snake_case names and the zlib-style `compressBound` alias are
// surfaced.
pub use util::{compress, compress_bound, compress2, compressBound, uncompress, uncompress2};

// Version, compile-flag, and error-string reporting (ported from `zutil.c`),
// each paired with its zlib-style camelCase alias.
pub use util::{z_error, zError, zlib_compile_flags, zlib_version, zlibCompileFlags, zlibVersion};

// Checksums (ported from `adler32.c`/`crc32.c`): the running checksum functions
// and their stream-combining companions.
pub use checksum::{adler32, adler32_combine, crc32, crc32_combine};

// ===========================================================================
// Prelude — grouped re-exports for `use zlib_rs::prelude::*;`
// ===========================================================================

/// Common imports for working with `zlib-rs`.
///
/// Bring the most frequently used types, traits, and enums into scope with a
/// single glob import:
///
/// ```
/// use zlib_rs::prelude::*;
///
/// // The re-exported types are now in scope.
/// let _stream: ZStream = ZStream::new();
/// let _header: GzHeader = GzHeader::new();
/// ```
///
/// The prelude intentionally excludes the free functions (`compress`,
/// `adler32`, …) and the C-ABI [`crate::ffi`] surface to avoid polluting the
/// caller's namespace; reach those through their crate paths
/// (`zlib_rs::compress`, `zlib_rs::ffi::…`).
pub mod prelude {
    pub use crate::constants::{DataType, FlushMode, Method, Strategy, WrapMode};
    pub use crate::error::{ReturnCode, ZlibError};
    pub use crate::gz_header::GzHeader;
    pub use crate::stream::{Allocator, DefaultAllocator, ZStream};
}

// ===========================================================================
// Sanity tests — version constants and re-export resolution
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The version constants must equal the values from `zlib.h` L44-L49.
    #[test]
    fn version_constants_match_zlib_h() {
        assert_eq!(ZLIB_VERSION, "1.3.2.1-motley");
        assert_eq!(ZLIB_VERNUM, 0x1321);
        assert_eq!(ZLIB_VER_MAJOR, 1);
        assert_eq!(ZLIB_VER_MINOR, 3);
        assert_eq!(ZLIB_VER_REVISION, 2);
        assert_eq!(ZLIB_VER_SUBREVISION, 1);
    }

    /// The version reporting functions re-exported at the crate root must agree
    /// with the authoritative [`ZLIB_VERSION`] constant.
    #[test]
    fn version_functions_agree_with_constant() {
        assert_eq!(zlib_version(), ZLIB_VERSION);
        assert_eq!(zlibVersion(), ZLIB_VERSION);
    }

    /// The value-namespace re-exports (functions and constants) must resolve
    /// through the crate root and behave as their zlib counterparts do on
    /// trivial, algorithm-guaranteed inputs.
    #[test]
    fn value_reexports_resolve_and_work() {
        // Compression-level constants carry their canonical zlib values.
        assert_eq!(Z_NO_COMPRESSION, 0);
        assert_eq!(Z_BEST_SPEED, 1);
        assert_eq!(Z_BEST_COMPRESSION, 9);
        assert_eq!(Z_DEFAULT_COMPRESSION, -1);

        // The `compress_bound` alias and its snake_case form are the same fn.
        assert_eq!(compress_bound(0), compressBound(0));

        // Adler-32 seeded with 1 and CRC-32 seeded with 0 are unchanged by an
        // empty input — a property that holds for every conforming impl.
        assert_eq!(adler32(1, b""), 1);
        assert_eq!(crc32(0, b""), 0);

        // The combine companions resolve and are identity over a zero-length
        // second stream.
        assert_eq!(
            adler32_combine(adler32(1, b"abc"), 1, 0),
            adler32(1, b"abc")
        );
        assert_eq!(crc32_combine(crc32(0, b"abc"), 0, 0), crc32(0, b"abc"));

        // The compile-flags/error-string reporters resolve through the root.
        let _ = zlib_compile_flags();
        let _ = zlibCompileFlags();
        assert_eq!(z_error(0), zError(0));
    }

    /// The type-namespace re-exports (enums, structs, and the [`Allocator`]
    /// trait) must resolve through both the crate root and the [`prelude`].
    #[test]
    fn type_reexports_resolve() {
        // A no-bound generic proves each path names a real type.
        fn assert_type<T>() {}
        assert_type::<ReturnCode>();
        assert_type::<ZlibError>();
        assert_type::<FlushMode>();
        assert_type::<Strategy>();
        assert_type::<WrapMode>();
        assert_type::<DataType>();
        assert_type::<Method>();
        assert_type::<DefaultAllocator>();
        assert_type::<ZStream>();
        assert_type::<GzHeader>();

        // The same items must also resolve through the prelude.
        assert_type::<prelude::ReturnCode>();
        assert_type::<prelude::ZStream>();
        assert_type::<prelude::GzHeader>();

        // The `Allocator` trait is re-exported and usable as a bound, and the
        // default allocator satisfies it.
        fn needs_allocator<A: Allocator>() {}
        needs_allocator::<DefaultAllocator>();
    }

    // -----------------------------------------------------------------------
    // Unsafe-containment boundary (F3 remediation)
    //
    // `#![deny(unsafe_code)]` at the crate root already makes a stray `unsafe` a
    // compile error. The two `#[allow(unsafe_code)]` carve-outs, however, are
    // ordinary attributes: a future edit could widen one, copy it onto a third
    // module, or replace it with a crate-level `#![allow(unsafe_code)]`, and the
    // crate would still compile. The check below independently re-derives the
    // boundary from the source text, so any of those changes fails the suite.
    //
    // It classifies each `unsafe` token, ignoring comments and string/char
    // literals, and treats a bare `unsafe extern "C" fn(..)` *type* (the
    // `ZallocFn`/`ZfreeFn` hook aliases in `src/stream.rs`) as declarative
    // rather than executable — which is exactly the distinction AAP §0.6.2
    // draws when it records the safe core as containing zero `unsafe`.
    // -----------------------------------------------------------------------

    /// Replaces every comment, string literal, raw string literal, and character
    /// literal with spaces, preserving byte offsets so ranges computed on the
    /// result also address the original text.
    ///
    /// Keeping offsets stable is what lets the caller locate the
    /// `mod no_std_support { .. }` block and test membership of an `unsafe`
    /// token against it.
    fn blank_comments_and_literals(src: &str) -> String {
        let b = src.as_bytes();
        let mut out = alloc::vec![b' '; b.len()];
        let mut i = 0usize;
        while i < b.len() {
            match b[i] {
                // Line comment.
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                // Block comment (nesting is legal in Rust).
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    let mut depth = 1usize;
                    i += 2;
                    while i < b.len() && depth > 0 {
                        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                            depth += 1;
                            i += 2;
                        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                // Raw string: `r"..."`, `r#"..."#`, `br##"..."##`, ...
                b'r' | b'b'
                    if {
                        let mut j = i;
                        if b[j] == b'b' {
                            j += 1;
                        }
                        if b.get(j) == Some(&b'r') {
                            j += 1;
                            while b.get(j) == Some(&b'#') {
                                j += 1;
                            }
                            b.get(j) == Some(&b'"')
                        } else {
                            false
                        }
                    } =>
                {
                    let mut j = i;
                    if b[j] == b'b' {
                        j += 1;
                    }
                    j += 1; // `r`
                    let mut hashes = 0usize;
                    while b.get(j) == Some(&b'#') {
                        hashes += 1;
                        j += 1;
                    }
                    j += 1; // opening quote
                    // Scan to the closing quote followed by `hashes` `#`s.
                    while j < b.len() {
                        if b[j] == b'"' {
                            let mut k = j + 1;
                            let mut seen = 0usize;
                            while seen < hashes && b.get(k) == Some(&b'#') {
                                seen += 1;
                                k += 1;
                            }
                            if seen == hashes {
                                j = k;
                                break;
                            }
                        }
                        j += 1;
                    }
                    i = j;
                }
                // Ordinary (possibly byte) string literal.
                b'"' => {
                    i += 1;
                    while i < b.len() {
                        match b[i] {
                            b'\\' => i += 2,
                            b'"' => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                }
                // Character literal — but `'` also starts a lifetime, which must
                // be preserved (blanking it would fuse neighbouring tokens).
                b'\'' => {
                    let is_char = if b.get(i + 1) == Some(&b'\\') {
                        true
                    } else {
                        b.get(i + 2) == Some(&b'\'')
                    };
                    if is_char {
                        i += 1;
                        while i < b.len() {
                            match b[i] {
                                b'\\' => i += 2,
                                b'\'' => {
                                    i += 1;
                                    break;
                                }
                                _ => i += 1,
                            }
                        }
                    } else {
                        // A lifetime: keep it verbatim.
                        out[i] = b[i];
                        i += 1;
                    }
                }
                // Ordinary code byte: keep.
                _ => {
                    out[i] = b[i];
                    i += 1;
                }
            }
        }
        alloc::string::String::from_utf8_lossy(&out).into_owned()
    }

    /// Whether the text immediately following an `unsafe` keyword makes it a
    /// mere *function-pointer type* (`unsafe extern "C" fn(..)` /
    /// `unsafe fn(..)`) rather than executable `unsafe`.
    fn is_fn_pointer_type(rest: &str) -> bool {
        let mut t = rest.trim_start();
        if let Some(r) = t.strip_prefix("extern") {
            t = r.trim_start();
        }
        match t.strip_prefix("fn") {
            Some(r) => r.trim_start().starts_with('('),
            None => false,
        }
    }

    /// Byte offsets of every executable `unsafe` token in already-blanked source.
    fn executable_unsafe_offsets(blanked: &str) -> alloc::vec::Vec<usize> {
        let bytes = blanked.as_bytes();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut hits = alloc::vec::Vec::new();
        let mut from = 0usize;
        while let Some(rel) = blanked[from..].find("unsafe") {
            let at = from + rel;
            from = at + "unsafe".len();
            // Whole-word only: `unsafe_code` and `my_unsafe` must not match.
            if at > 0 && ident(bytes[at - 1]) {
                continue;
            }
            if bytes.get(from).is_some_and(|&c| ident(c)) {
                continue;
            }
            if !is_fn_pointer_type(&blanked[from..]) {
                hits.push(at);
            }
        }
        hits
    }

    /// Every `.rs` file under `src/`, as `(relative path, contents)`.
    fn crate_sources() -> alloc::vec::Vec<(std::string::String, std::string::String)> {
        fn walk(
            dir: &std::path::Path,
            root: &std::path::Path,
            out: &mut alloc::vec::Vec<(std::string::String, std::string::String)>,
        ) {
            let mut entries: alloc::vec::Vec<_> = std::fs::read_dir(dir)
                .expect("src/ must be readable")
                .map(|e| e.expect("directory entry").path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let rel = path
                        .strip_prefix(root)
                        .expect("path is under the manifest dir")
                        .to_string_lossy()
                        .replace('\\', "/");
                    let text = std::fs::read_to_string(&path).expect("source file is UTF-8");
                    out.push((rel, text));
                }
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = alloc::vec::Vec::new();
        walk(&root.join("src"), root, &mut out);
        assert!(
            out.len() > 30,
            "expected the full src/ tree, found only {} files",
            out.len()
        );
        out
    }

    /// The byte range of the `mod no_std_support { .. }` block in `src/lib.rs` —
    /// the crate root's single permitted `unsafe` region.
    fn no_std_support_range(blanked: &str) -> core::ops::Range<usize> {
        let start = blanked
            .find("mod no_std_support")
            .expect("the no_std runtime-support module must exist in src/lib.rs");
        let open = start
            + blanked[start..]
                .find('{')
                .expect("the module must have a body");
        let mut depth = 0usize;
        for (i, c) in blanked[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return start..open + i + 1;
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces while delimiting mod no_std_support");
    }

    /// Executable `unsafe` exists **only** inside `src/ffi/**` and the crate
    /// root's `no_std_support` runtime block — User Constraint 3 / AAP §0.6.2 /
    /// §0.7.2 standard S2.
    #[test]
    fn executable_unsafe_is_confined_to_the_designated_boundary() {
        let mut ffi_hits = 0usize;
        let mut runtime_hits = 0usize;

        for (rel, text) in crate_sources() {
            let blanked = blank_comments_and_literals(&text);
            let hits = executable_unsafe_offsets(&blanked);

            if rel.starts_with("src/ffi/") {
                ffi_hits += hits.len();
                continue;
            }

            if rel == "src/lib.rs" {
                let allowed = no_std_support_range(&blanked);
                let stray: alloc::vec::Vec<usize> = hits
                    .iter()
                    .copied()
                    .filter(|at| !allowed.contains(at))
                    .collect();
                assert!(
                    stray.is_empty(),
                    "src/lib.rs has executable `unsafe` outside `mod no_std_support` \
                     at byte offsets {stray:?}"
                );
                runtime_hits += hits.len();
                continue;
            }

            assert!(
                hits.is_empty(),
                "{rel} contains executable `unsafe` at byte offsets {:?}; the safe \
                 core must contain none (User Constraint 3). Restructure instead, \
                 or move the raw-pointer work into `src/ffi/**`.",
                hits
            );
        }

        // The boundary must not be empty, otherwise the test would pass
        // vacuously if the classifier ever stopped matching anything.
        assert!(
            ffi_hits > 100,
            "expected the FFI boundary to hold the crate's `unsafe`, found {ffi_hits}"
        );
        assert!(
            runtime_hits > 0,
            "expected the no_std runtime block to hold `unsafe`, found {runtime_hits}"
        );
    }

    /// The enforcement attributes themselves are pinned: the crate root denies
    /// `unsafe_code`, nothing re-enables it crate- or module-wide, and exactly two
    /// narrowly scoped carve-outs exist — both in `src/lib.rs`.
    #[test]
    fn unsafe_code_denial_has_exactly_two_scoped_carve_outs() {
        let mut total_allows = 0usize;

        for (rel, text) in crate_sources() {
            let blanked = blank_comments_and_literals(&text);
            let allows = blanked.matches("#[allow(unsafe_code)]").count();
            let inner_allows = blanked.matches("#![allow(unsafe_code)]").count();

            assert_eq!(
                inner_allows, 0,
                "{rel} re-enables `unsafe_code` for a whole module or crate; only \
                 narrowly scoped `#[allow(unsafe_code)]` on the two designated \
                 boundaries is permitted"
            );

            if rel == "src/lib.rs" {
                assert!(
                    blanked.contains("#![deny(unsafe_code)]"),
                    "the crate root must deny `unsafe_code`"
                );
                assert!(
                    !blanked.contains("#![forbid(unsafe_code)]"),
                    "`forbid` cannot be relaxed by the two boundary carve-outs"
                );
            } else {
                assert_eq!(
                    allows, 0,
                    "{rel} carries an `#[allow(unsafe_code)]`; the only permitted \
                     carve-outs are `mod no_std_support` and `pub mod ffi`, both in \
                     src/lib.rs"
                );
            }
            total_allows += allows;
        }

        assert_eq!(
            total_allows, 2,
            "exactly two `#[allow(unsafe_code)]` carve-outs must exist \
             (mod no_std_support and pub mod ffi)"
        );
    }

    /// The predicate that both applies `#![no_std]` to the crate and gates the
    /// runtime-support module. The two MUST stay in lockstep: the module supplies
    /// the `#[global_allocator]`/`#[panic_handler]` that only a genuinely
    /// freestanding build lacks, so a wider predicate is a duplicate-lang-item
    /// error and a narrower one silently drops the items the link needs.
    const RUNTIME_GATE: &str = "all(not(feature = \"std\"), not(test), panic = \"abort\")";

    /// Each `#[allow(unsafe_code)]` carve-out is attached to its designated
    /// boundary — and to nothing else.
    ///
    /// [`unsafe_code_denial_has_exactly_two_scoped_carve_outs`] *counts* the
    /// carve-outs; it cannot see what they are attached to. Moving one onto a core
    /// module keeps the count at two, keeps the crate compiling, and keeps the
    /// boundary scan green for exactly as long as that module happens to contain
    /// no `unsafe` — so the permission would widen with nothing to reveal it.
    ///
    /// The same walk pins two further properties nothing else observes:
    ///
    /// * `pub mod ffi;` carries **no** `cfg`. Gating it would shrink the emitted
    ///   `cdylib`/`staticlib` symbol table below the 54 `zlib.map` globals a
    ///   drop-in consumer links against, while every default-feature gate stayed
    ///   green.
    /// * `mod no_std_support`'s gate is character-for-character the crate's own
    ///   `no_std` predicate. If the two ever diverge, the freestanding build
    ///   either redefines lang items `std` already provides or loses the
    ///   allocator and panic handler it must supply — and, because the module is
    ///   compiled by `--no-default-features` alone, a default `cargo build`
    ///   cannot see either outcome.
    #[test]
    fn each_carve_out_is_attached_to_its_designated_boundary() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text =
            std::fs::read_to_string(root.join("src/lib.rs")).expect("src/lib.rs must be readable");
        let lines: alloc::vec::Vec<&str> = text.lines().collect();

        // Each carve-out, as (guarded item, other attributes in its group).
        let mut guarded: alloc::vec::Vec<(&str, alloc::vec::Vec<&str>)> = alloc::vec::Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if line.trim() != "#[allow(unsafe_code)]" {
                continue;
            }

            // An attribute group is a run of attributes and comments bounded by
            // blank lines, so collect siblings in both directions and stop at the
            // first blank line or non-attribute, non-comment line.
            let mut attrs: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
            let mut j = i;
            while j > 0 {
                j -= 1;
                let above = lines[j].trim();
                if above.starts_with("//") {
                    continue;
                }
                if above.starts_with("#[") {
                    attrs.push(above);
                    continue;
                }
                break;
            }

            let mut item = None;
            for below in lines.iter().skip(i + 1) {
                let below = below.trim();
                if below.is_empty() || below.starts_with("//") {
                    continue;
                }
                if below.starts_with("#[") {
                    attrs.push(below);
                    continue;
                }
                item = Some(below);
                break;
            }

            guarded.push((
                item.expect("an `#[allow(unsafe_code)]` must guard a following item"),
                attrs,
            ));
        }

        let runtime_cfg = alloc::format!("#[cfg({RUNTIME_GATE})]");
        assert_eq!(
            guarded,
            alloc::vec![
                ("mod no_std_support {", alloc::vec![runtime_cfg.as_str()]),
                ("pub mod ffi;", alloc::vec::Vec::new()),
            ],
            "the two `unsafe_code` carve-outs must guard exactly `mod \
             no_std_support` (gated on the crate's own `no_std` predicate) and an \
             UNCONDITIONAL `pub mod ffi;` — nothing else, and neither with an \
             extra `cfg`"
        );

        // The module gate and the crate's `no_std` gate are the same predicate.
        assert!(
            text.contains(&alloc::format!("#![cfg_attr({RUNTIME_GATE}, no_std)]")),
            "the crate `no_std` gate must use the same predicate as the \
             runtime-support module it keeps in lockstep"
        );
    }

    /// Every CI job whose gate is meaningful only on a particular toolchain,
    /// paired with the channel it must resolve to.
    ///
    /// `rust-toolchain.toml` pins this repository to the MSRV floor, and that
    /// repository pin OUTRANKS the `rustup default` set by
    /// `dtolnay/rust-toolchain`. Without a higher-priority override every one
    /// of these jobs silently runs on 1.85.0 instead of its intended channel —
    /// the stable rows would still pass while quietly not covering stable, and
    /// `cargo fuzz build` would fail outright because cargo-fuzz needs nightly.
    const TOOLCHAIN_JOBS: [(&str, &str, &str); 8] = [
        (".github/workflows/ci.yml", "build-test", "stable"),
        (".github/workflows/ci.yml", "no-std-tests", "stable"),
        (".github/workflows/ci.yml", "lint", "stable"),
        (".github/workflows/ci.yml", "msrv", "1.85.0"),
        (".github/workflows/ci.yml", "benches", "stable"),
        (".github/workflows/ci.yml", "build-script-tests", "stable"),
        (".github/workflows/ci.yml", "unsafe-boundary", "stable"),
        (".github/workflows/fuzz.yml", "cargo-fuzz", "nightly"),
    ];

    /// Returns the text of one job block from a workflow file.
    ///
    /// Jobs are the only two-space-indented mapping keys under `jobs:`, so a
    /// block runs from its own key line to the next such key (or end of file).
    fn workflow_job_block(workflow: &str, job: &str) -> std::string::String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join(workflow))
            .unwrap_or_else(|e| panic!("{workflow} must be readable: {e}"));
        let header = alloc::format!("\n  {job}:\n");
        let start = text
            .find(&header)
            .unwrap_or_else(|| panic!("{workflow} must define the `{job}` job"))
            + header.len();
        let rest = &text[start..];
        // The block ends at the next two-space-indented mapping key, i.e. the
        // next sibling job.
        let end = rest
            .char_indices()
            .filter(|&(_, c)| c == '\n')
            .map(|(i, _)| i + 1)
            .find(|&i| {
                let line = rest[i..].split('\n').next().unwrap_or_default();
                line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':')
            })
            .map_or(rest.len(), |i| i - 1);
        rest[..end].to_string()
    }

    #[test]
    fn every_toolchain_specific_ci_job_pins_and_asserts_its_toolchain() {
        for (workflow, job, channel) in TOOLCHAIN_JOBS {
            let block = workflow_job_block(workflow, job);

            // 1. A rank-2 `RUSTUP_TOOLCHAIN` override, which beats the rank-4
            //    repository pin. Without it the job runs on the MSRV floor.
            let needle = alloc::format!("RUSTUP_TOOLCHAIN: {channel}");
            assert!(
                block.contains(&needle),
                "{workflow} job `{job}` must set `{needle}`, otherwise \
                 rust-toolchain.toml silently forces it onto the MSRV floor"
            );

            // 2. A step that proves the override actually took effect, so losing
            //    it fails the job loudly instead of downgrading the gate.
            assert!(
                block.contains("rustc --version --verbose"),
                "{workflow} job `{job}` must assert its resolved toolchain with \
                 `rustc --version --verbose`"
            );
            assert!(
                block.contains("rustup show active-toolchain"),
                "{workflow} job `{job}` must read `rustup show active-toolchain` \
                 to identify the resolved channel"
            );
            let expected_arm = alloc::format!("{channel}-*)");
            assert!(
                block.contains(&expected_arm),
                "{workflow} job `{job}` must accept only `{expected_arm}`"
            );

            // 3. The assertion must actually fail the job. A `case` arm that
            //    merely prints is indistinguishable from no check at all, and it
            //    is latent: the mismatch branch is not taken while the override
            //    is present, so nothing else would ever reveal it.
            assert!(
                block.contains("exit 1"),
                "{workflow} job `{job}`'s toolchain assertion must `exit 1` on \
                 mismatch, or it cannot fail the job"
            );
        }
    }

    #[test]
    fn the_toolchain_pin_documents_no_clippy_failure_that_does_not_exist() {
        // F2 also covered stale commentary: `rust-toolchain.toml` claimed the
        // `-D warnings` clippy gate FAILS on the pinned floor's clippy. It was
        // measured passing (exit 0, zero diagnostics) on clippy 0.1.85 and
        // 0.1.97 alike, and neither construct the claim named still exists.
        // Guard against the claim being reinstated.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let pin = std::fs::read_to_string(root.join("rust-toolchain.toml"))
            .expect("rust-toolchain.toml must be readable");
        assert!(
            !pin.contains("FAILS on the"),
            "rust-toolchain.toml must not assert a clippy failure on the pinned \
             floor: the exact gate was measured passing on clippy 0.1.85"
        );
        assert!(
            pin.contains("clippy 0.1.85 (4d91de4e48 2025-02-17)  — exit 0"),
            "rust-toolchain.toml must record the measured floor clippy result"
        );
        // And the pin itself must still be the MSRV floor, not a moving channel.
        assert!(
            pin.contains("channel = \"1.85.0\""),
            "the toolchain pin must remain the MSRV floor 1.85.0"
        );
    }

    /// `OS_CODE` — the gzip-header operating-system byte — is declared in exactly
    /// one module, and the gzip emission path reads that one declaration.
    ///
    /// This has to be a *source-level* guard because no value assertion can catch
    /// the defect it protects against. `OS_CODE` is `3` on Linux, so a module that
    /// re-declares its own `const OS_CODE: u8 = 3` agrees with the canonical
    /// constant on every currently exercised CI target while silently emitting `3`
    /// where reference zlib emits `10` on Windows (`zutil.h` L156-L158) or `19` on
    /// Apple (L168-L170). Counting declarations catches that on every platform.
    #[test]
    fn os_code_is_declared_in_exactly_one_module() {
        let mut declaring: alloc::vec::Vec<(std::string::String, usize)> = alloc::vec::Vec::new();
        let mut saw_util = false;
        let mut saw_deflate = false;

        for (rel, text) in crate_sources() {
            let blanked = blank_comments_and_literals(&text);
            let declarations = blanked.matches("const OS_CODE").count();
            if declarations > 0 {
                declaring.push((rel.clone(), declarations));
            }

            if rel == "src/util/mod.rs" {
                saw_util = true;
                // The canonical home holds the full `#ifdef` cascade: one arm per
                // platform class, mutually exclusive by construction.
                assert_eq!(
                    declarations, 3,
                    "src/util/mod.rs must hold exactly the three-arm OS_CODE cascade"
                );

                // Each declaration must carry its own `cfg`. Scanned on the raw
                // text (the blanked copy erases the `"apple"` literal) by walking
                // back from every declaration over its doc comment to the
                // attribute immediately above it.
                // Capturing the literal alongside the `cfg` is what makes the
                // per-platform claims checkable from a single host: a wrong value in
                // an arm that this target does not compile is otherwise invisible.
                let lines: alloc::vec::Vec<&str> = text.lines().collect();
                let mut arms: alloc::vec::Vec<(&str, &str)> = alloc::vec::Vec::new();
                for (i, line) in lines.iter().enumerate() {
                    if !line.contains("const OS_CODE") {
                        continue;
                    }
                    let value = line
                        .split_once('=')
                        .and_then(|(_, rhs)| rhs.split_once(';'))
                        .map(|(value, _)| value.trim())
                        .expect("an OS_CODE declaration is `... = <literal>;`");
                    let mut j = i;
                    while j > 0 {
                        j -= 1;
                        let above = lines[j].trim();
                        if above.is_empty() || above.starts_with("///") {
                            continue;
                        }
                        arms.push((above, value));
                        break;
                    }
                }
                arms.sort_unstable();
                assert_eq!(
                    arms,
                    alloc::vec![
                        (
                            "#[cfg(all(not(windows), not(target_vendor = \"apple\")))]",
                            "3"
                        ),
                        ("#[cfg(all(not(windows), target_vendor = \"apple\"))]", "19"),
                        ("#[cfg(windows)]", "10"),
                    ],
                    "the OS_CODE cascade must be exactly `10` on Windows (zutil.h \
                     L156-L158), `19` on Apple (L168-L170) and the `3` Unix fallback \
                     (L187-L189), each guarded by its own mutually exclusive `cfg` so \
                     that precisely one arm compiles for any target"
                );
            }

            if rel == "src/deflate/mod.rs" {
                saw_deflate = true;
                assert!(
                    blanked.contains("use crate::util::OS_CODE;"),
                    "the gzip header emission path must import the one canonical \
                     OS_CODE from crate::util instead of declaring its own"
                );
            }
        }

        assert!(saw_util && saw_deflate, "both owning files must be scanned");
        assert_eq!(
            declaring,
            alloc::vec![(std::string::String::from("src/util/mod.rs"), 3usize)],
            "OS_CODE must be declared only by src/util/mod.rs; a second declaration \
             is a platform-divergence bug that Linux-only gates cannot observe"
        );
    }

    /// The comment/literal blanking and token classification the boundary check
    /// relies on are themselves tested, so a silent classifier regression cannot
    /// turn the boundary check into a no-op.
    #[test]
    fn boundary_scanner_classifies_tokens_correctly() {
        // Comments and literals never contribute a hit.
        for src in [
            "// unsafe\n",
            "/* unsafe */",
            "/* a /* unsafe */ b */",
            "//! unsafe\n",
            "let s = \"unsafe\";",
            "let s = r#\"unsafe\"#;",
            "let s = br##\"unsafe\"##;",
        ] {
            let blanked = blank_comments_and_literals(src);
            assert!(
                executable_unsafe_offsets(&blanked).is_empty(),
                "false positive for {src:?}"
            );
        }

        // A lifetime is preserved rather than blanked, and does not create a hit.
        let blanked = blank_comments_and_literals("fn f<'a>(x: &'a u8) {}");
        assert!(blanked.contains("'a"));
        assert!(executable_unsafe_offsets(&blanked).is_empty());

        // Identifiers merely containing the word do not match.
        for src in [
            "#![deny(unsafe_code)]",
            "let unsafely = 1;",
            "let x_unsafe = 1;",
        ] {
            let blanked = blank_comments_and_literals(src);
            assert!(
                executable_unsafe_offsets(&blanked).is_empty(),
                "false positive for {src:?}"
            );
        }

        // Function-pointer *types* are declarative, not executable.
        for src in [
            "pub type F = unsafe extern \"C\" fn(*mut u8) -> i32;",
            "pub type G = unsafe fn(u8);",
        ] {
            let blanked = blank_comments_and_literals(src);
            assert!(
                executable_unsafe_offsets(&blanked).is_empty(),
                "fn-pointer type wrongly flagged: {src:?}"
            );
        }

        // Every executable form IS flagged.
        for src in [
            "let x = unsafe { 1 };",
            "unsafe fn f() {}",
            "unsafe impl Send for T {}",
            "unsafe trait T {}",
            "unsafe extern \"C\" { fn f(); }",
            "unsafe extern { fn f(); }",
            "pub unsafe extern \"C\" fn f() {}",
        ] {
            let blanked = blank_comments_and_literals(src);
            assert_eq!(
                executable_unsafe_offsets(&blanked).len(),
                1,
                "executable unsafe missed: {src:?}"
            );
        }
    }
}
