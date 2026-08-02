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
// production library target, so every `unsafe` block crossing the FFI boundary
// carries its justification where a reader will meet it. The lint is relaxed to
// `allow` under `cfg(test)` so it governs only the shipped
// `cdylib`/`staticlib`/`rlib` (whose `unsafe` lives in `src/ffi/**` and in the
// private `no_std_support` block below), not the crate's inline `#[cfg(test)]`
// unit tests.
#![warn(clippy::undocumented_unsafe_blocks)]
#![cfg_attr(test, allow(clippy::undocumented_unsafe_blocks))]
// `unsafe` is DENIED crate-wide, converting the migration's unsafe-containment
// strategy (AAP §0.3.2 pattern C7 / §0.6.2 / §0.7.2 standard S2, satisfying User
// Constraint 3 "zero unsafe blocks in core compression logic") from an
// architectural convention plus a lint-assisted review check into a HARD COMPILE
// ERROR. A stray `unsafe` block, `unsafe fn`, `unsafe impl`, or `unsafe extern`
// anywhere in `src/deflate/**`, `src/inflate/**`, `src/checksum/**`,
// `src/gz/**`, `src/util/**`, `src/stream.rs`, `src/error.rs`,
// `src/constants.rs`, or `src/gz_header.rs` fails the build outright.
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
// `no_std` runtime support — global allocator + panic handler + personality
//
// A `#![no_std]` crate that still allocates (this one uses `Box`/`Vec`/`String`
// via `alloc`) and is emitted as a `cdylib`/`staticlib` must SUPPLY its own
// `#[global_allocator]` and `#[panic_handler]`: the standard library normally
// provides both, and without `std` the final `cdylib`/`staticlib` link step
// fails with "no global memory allocator found" and "`#[panic_handler]`
// function required, but not found".
//
// It must also supply `rust_eh_personality`, for the same reason one step later
// in the pipeline: the two items above satisfy *rustc*, but the pre-compiled
// sysroot `core`/`alloc` objects still carry LSDA references to the unwinder's
// personality symbol, so the artifact compiles and yet is unlinkable and
// unloadable by any C consumer. The rationale is documented in full at that
// item's definition, at the end of this module.
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

    // The third freestanding runtime item, and the same kind of obligation as the
    // allocator and panic handler above: a symbol `std` would have supplied.
    //
    // The sysroot `core`/`alloc` rlibs this crate links against are distributed
    // pre-compiled with `panic = "unwind"`, so their objects carry LSDA
    // (language-specific data area) references to the unwinder's personality
    // routine — materialized as a `.data.DW.ref.rust_eh_personality` word that
    // *names* the symbol even in code that can never unwind. `std` defines
    // `rust_eh_personality`; a freestanding `cdylib`/`staticlib` does not link
    // `std`, so without this definition nothing resolves it and EVERY std-off
    // artifact is unlinkable and unloadable:
    //
    //   ld: libzlib_rs.a(alloc-*.rcgu.o):(.data.DW.ref.rust_eh_personality+0x0):
    //       undefined reference to `rust_eh_personality'
    //   ld: libzlib_rs.so: undefined reference to `rust_eh_personality'
    //   dlopen: libzlib_rs.so: undefined symbol: rust_eh_personality
    //
    // It is therefore gated by the identical predicate, so a `std` build (which
    // already has the symbol) never sees a duplicate definition.
    //
    // The routine is unreachable by construction: this configuration is compiled
    // `panic = "abort"` (`Cargo.toml` sets it for both profiles, which AAP §0.5.3
    // records as required for a stable-toolchain `no_std` `cdylib`/`staticlib`),
    // so no Rust frame ever unwinds and the personality routine is never entered
    // — the references above are pure link-time data. Only the SYMBOL is needed,
    // which is why this is a plain `#[unsafe(no_mangle)] extern "C"` function
    // rather than the nightly-only `#[lang = "eh_personality"]` item: it keeps the
    // freestanding build on the declared MSRV (stable 1.85.0). The body aborts
    // rather than returning so that if some future configuration ever did route an
    // unwind here, the process terminates deterministically instead of resuming
    // with an unwind context this crate cannot honor — the same "never unwind
    // across the C ABI" invariant the panic handler upholds.
    //
    // ---------------------------------------------------------------------
    // Symbol VISIBILITY: needed by the linker, never exported to consumers.
    //
    // `#[unsafe(no_mangle)]` gives the definition below an unmangled name *and*
    // default ELF visibility, so a freestanding `cdylib` would publish it in
    // `.dynsym` as `FUNC GLOBAL DEFAULT`. That is wrong twice over:
    //
    //   * it widens the drop-in `libz` surface. A distribution `libz.so.1`
    //     exports the 54 `zlib.map` globals and nothing else; the default-feature
    //     build of this crate emits exactly 95 dynamic `T` symbols, and a
    //     freestanding build must emit the same 95 rather than 96. The symbol set
    //     is part of the ABI contract, so it must not vary with a Cargo feature.
    //   * it exposes an *aborting* routine to ELF symbol interposition. Loaded
    //     into a global symbol scope (`RTLD_GLOBAL`, or as a `DT_NEEDED` of the
    //     main object) alongside another Rust shared object, this definition can
    //     win the process-wide lookup for that object's personality references
    //     and turn its unwinds into an abort.
    //
    // The reference this definition exists to satisfy is resolved at *static*
    // link time, within the artifact, so internal linkage is sufficient: hiding
    // the symbol keeps every std-off `cdylib`/`staticlib` linkable and loadable
    // while removing it from the dynamic table entirely.
    //
    // An assembler visibility directive is used because no stable, sufficiently
    // narrow attribute exists at the declared MSRV (1.85.0): `#[no_mangle]` has
    // no visibility modifier, `#[unsafe(export_name)]` renames without changing
    // visibility, and `-C default-visibility=hidden` both postdates the MSRV
    // (stabilized in 1.86) and would apply to every symbol in the crate,
    // including the 95 that must stay exported. Emitting the directive next to
    // the definition keeps the two impossible to separate.
    //
    // Platform coverage is deliberate and explicit. ELF is the format the
    // concern above is stated in, and is the only one measured here. Mach-O's
    // equivalent is `.private_extern` on the underscore-prefixed name; that arm
    // is provided on the same reasoning but is NOT verified in this environment,
    // and is documented as such rather than presented as tested. Non-Unix
    // targets get no directive: COFF has no visibility concept — a DLL's export
    // table is opt-in — and bare-metal ELF targets emit only a `staticlib`, which
    // has no dynamic symbol table and therefore no interposition surface.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    core::arch::global_asm!(".hidden rust_eh_personality");

    #[cfg(all(unix, target_vendor = "apple"))]
    core::arch::global_asm!(".private_extern _rust_eh_personality");

    #[unsafe(no_mangle)]
    extern "C" fn rust_eh_personality() {
        // SAFETY: `abort` is the libc process-termination routine declared above;
        // it never returns and has no preconditions.
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

// The IDIOMATIC gzip file-I/O layer fundamentally requires the standard library
// (`std::fs`/`std::io`), so it is compiled only when the `gz-io` feature is
// enabled (which implies `std` + `gzip`); a bare `no_std` build omits it
// entirely. This gate governs the Rust-facing API only. The C-facing `gz*`
// symbols in `src/ffi/gz.rs` are deliberately NOT gated this way — see the
// `pub mod ffi` note below.
#[cfg(feature = "gz-io")]
pub mod gz;

// The FFI drop-in boundary is declared UNCONDITIONALLY so the emitted
// `cdylib`/`staticlib` always presents the full zlib C symbol table for
// linkage. This holds for the WHOLE table in EVERY feature configuration, not
// just for the core engines: `src/ffi/mod.rs` declares all four shim submodules
// (`types`, `util`, `deflate`, `inflate`, `gz`) unconditionally, and each of the
// 34 `gz*` entry points has exactly one definition whose *body* — never the
// exported item — is gated on `gz-io`. Without the feature those symbols still
// resolve and return their documented failure sentinel, so a C consumer links
// against one ABI regardless of how the crate was configured (AAP §0.3.1,
// §0.8.1 D-4). Only `std`-dependent internals inside `ffi` are gated; the module
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
    // Unsafe-containment boundary
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
    ///   green. This check owns only the *root* declaration; an unconditional
    ///   `pub mod ffi;` is necessary but not sufficient, because a `cfg` on any
    ///   child shim module would shrink the table just as effectively. That
    ///   second half is pinned inside the boundary itself, by
    ///   `crate::ffi`'s `no_exported_gz_symbol_is_feature_gated` and
    ///   `every_exported_c_symbol_resolves_to_a_live_address` — the latter
    ///   resolving all 95 exports to live addresses once per feature row, so no
    ///   configuration can claim completeness without demonstrating it.
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

    /// The freestanding runtime block supplies **all three** items a std-off
    /// `cdylib`/`staticlib` needs — not just the two `rustc` itself demands.
    ///
    /// `#[global_allocator]` and `#[panic_handler]` are compiler-enforced: omit
    /// either and `cargo build --no-default-features` fails outright with "no
    /// global memory allocator found" / "`#[panic_handler]` function required".
    /// `rust_eh_personality` is **not**. The pre-compiled sysroot `core`/`alloc`
    /// objects reference it from their LSDA (a `.data.DW.ref.rust_eh_personality`
    /// word), so omitting it still compiles, still reports success, and still
    /// emits `libzlib_rs.{a,so}` — the artifacts are simply unlinkable and
    /// unloadable:
    ///
    /// ```text
    /// ld: libzlib_rs.a(alloc-*.rcgu.o):(.data.DW.ref.rust_eh_personality+0x0):
    ///     undefined reference to `rust_eh_personality'
    /// ld: libzlib_rs.so: undefined reference to `rust_eh_personality'
    /// dlopen: libzlib_rs.so: undefined symbol: rust_eh_personality
    /// ```
    ///
    /// No `cargo build`, `cargo test`, `cargo clippy`, or `cargo fmt` invocation
    /// on any feature row can observe that — only a C consumer can — which is
    /// precisely why the item is pinned here, in the Rust suite that always runs.
    ///
    /// Two further properties nothing else observes are pinned with it:
    ///
    /// * each item appears exactly **once** in `src/lib.rs` and lies **inside**
    ///   `mod no_std_support`, so it is compiled only for a genuinely
    ///   freestanding build and can never collide with the `std`-provided one;
    /// * the personality routine carries `#[unsafe(no_mangle)]`. It is reached by
    ///   the *linker*, by name; without that attribute rustc mangles the symbol,
    ///   the definition silently stops resolving the references it exists to
    ///   satisfy, and every Rust-side gate stays green.
    #[test]
    fn the_freestanding_runtime_block_supplies_every_required_item() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text =
            std::fs::read_to_string(root.join("src/lib.rs")).expect("src/lib.rs must be readable");
        let blanked = blank_comments_and_literals(&text);
        let runtime = no_std_support_range(&blanked);

        for marker in [
            "#[global_allocator]",
            "#[panic_handler]",
            "fn rust_eh_personality",
        ] {
            let hits: alloc::vec::Vec<usize> =
                blanked.match_indices(marker).map(|(at, _)| at).collect();
            assert_eq!(
                hits.len(),
                1,
                "`{marker}` must appear exactly once in src/lib.rs, found {}",
                hits.len()
            );
            assert!(
                runtime.contains(&hits[0]),
                "`{marker}` must live inside `mod no_std_support` so it is compiled ONLY \
                 for a freestanding build; outside that gate it collides with the \
                 std-provided definition"
            );
        }

        // The personality routine is resolved by the linker, by name.
        let lines: alloc::vec::Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| {
                l.trim_start()
                    .starts_with("extern \"C\" fn rust_eh_personality()")
            })
            .expect(
                "the personality routine must be a plain `extern \"C\" fn \
                 rust_eh_personality()` — only the SYMBOL is required, so it must NOT \
                 depend on the nightly-only `#[lang = \"eh_personality\"]` item",
            );

        let mut i = at;
        let attr = loop {
            assert!(i > 0, "the personality routine must carry an attribute");
            i -= 1;
            let above = lines[i].trim();
            if above.is_empty() || above.starts_with("//") {
                continue;
            }
            break above;
        };
        assert_eq!(
            attr, "#[unsafe(no_mangle)]",
            "the personality routine must be `#[unsafe(no_mangle)]`; a mangled symbol \
             does not resolve the sysroot LSDA references, and nothing on the Rust side \
             would notice"
        );
    }

    /// The complete set of unmangled symbols this crate defines **outside**
    /// `src/ffi/**`, each paired with the assembler directive that must keep it
    /// out of the dynamic symbol table.
    ///
    /// The emitted symbol surface is part of the C ABI contract: a distribution
    /// `libz.so.1` publishes the 54 `zlib.map` globals and nothing else, and this
    /// crate's `cdylib` publishes exactly 95 dynamic `T` symbols — the 96 names
    /// its `#[unsafe(no_mangle)]` shims declare, minus the `#[cfg(windows)]`-gated
    /// `gzopen_w`. That count must NOT vary with a Cargo feature.
    ///
    /// `crate::ffi`'s own inventory cannot enforce this, because it scans only
    /// `src/ffi/{module}.rs`. Anything unmangled declared anywhere else is
    /// invisible to it and reaches `.dynsym` unnoticed on the feature rows that
    /// compile it — which is exactly what happened to the freestanding
    /// personality routine: every Rust-side gate stayed green while the std-off
    /// `cdylib` exported 96 symbols instead of 95, publishing an *aborting*
    /// routine that ELF interposition could select for another Rust shared
    /// object's unwinds.
    ///
    /// Each entry is `(relative path, symbol, hiding directive)`. A new unmangled
    /// symbol outside `src/ffi/**` fails the test below until it is either given
    /// internal linkage and listed here, or moved into a shim module where the
    /// FFI inventory governs it.
    const NON_FFI_UNMANGLED_SYMBOLS: [(&str, &str, &str); 1] = [(
        "src/lib.rs",
        "rust_eh_personality",
        ".hidden rust_eh_personality",
    )];

    /// No unmangled symbol outside `src/ffi/**` escapes into the dynamic symbol
    /// table, so the exported surface is identical on every feature row.
    ///
    /// This is the companion to `crate::ffi`'s inventory, covering precisely the
    /// blind spot that inventory has by construction (see
    /// [`NON_FFI_UNMANGLED_SYMBOLS`]). Two independent properties are pinned:
    ///
    /// 1. **The set is closed.** Scanning the whole `src/` tree for name-fixing
    ///    attributes — `#[unsafe(no_mangle)]`, bare `#[no_mangle]`, and
    ///    `export_name`, since each one publishes a chosen, unmangled name —
    ///    yields exactly the listed entries and nothing more.
    /// 2. **Each one is hidden, next to its own definition.** The directive must
    ///    appear in the same file and, for the crate root, inside
    ///    `mod no_std_support`, so it is compiled under exactly the predicate that
    ///    compiles the definition. A directive that drifted out of that gate would
    ///    either hide nothing or apply to a symbol this build does not define.
    ///
    /// The check is deliberately source-based rather than an `nm` sweep of a built
    /// artifact: it then runs on every feature row of every `cargo test`
    /// invocation, including `--no-default-features`, with no build-order
    /// dependency, no external tool, and no conditional skip. The linked artifacts
    /// are still what proves the mechanism works — a std-off release `cdylib`
    /// measures 95 dynamic `T` symbols with `rust_eh_personality` present as
    /// `FUNC LOCAL` and absent from `.dynsym`, while `DW.ref.rust_eh_personality`
    /// still resolves — and this test is what keeps the source in the shape that
    /// produced that measurement.
    #[test]
    fn no_unmangled_symbol_outside_the_ffi_shims_reaches_the_dynamic_table() {
        // Attributes that fix an unmangled, externally visible name. `no_mangle`
        // is matched as a bare token so `#[unsafe(no_mangle)]` and the older
        // `#[no_mangle]` spelling are both caught.
        const NAME_FIXING: [&str; 2] = ["no_mangle", "export_name"];

        let mut found: alloc::vec::Vec<(std::string::String, std::string::String)> =
            alloc::vec::Vec::new();

        for (path, text) in crate_sources() {
            if path.starts_with("src/ffi/") {
                continue;
            }
            // Comments and string literals are blanked first: this file discusses
            // `#[unsafe(no_mangle)]` at length in prose, and those mentions are
            // not declarations.
            let blanked = blank_comments_and_literals(&text);
            let lines: alloc::vec::Vec<&str> = blanked.lines().collect();

            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if !trimmed.starts_with("#[") {
                    continue;
                }
                if !NAME_FIXING.iter().any(|attr| trimmed.contains(attr)) {
                    continue;
                }
                // The declaration is the next line that is neither blank nor a
                // further attribute.
                let signature = lines[index + 1..]
                    .iter()
                    .map(|next| next.trim())
                    .find(|next| !next.is_empty() && !next.starts_with("#["))
                    .unwrap_or_else(|| {
                        panic!("{path} line {}: `{trimmed}` guards nothing", index + 1)
                    });
                let name = signature
                    .split_once(" fn ")
                    .map(|(_, rest)| rest)
                    .or_else(|| signature.split_once("static ").map(|(_, rest)| rest))
                    .unwrap_or_else(|| {
                        panic!(
                            "{path} line {}: `{trimmed}` must guard an `fn` or `static`, \
                             found `{signature}`",
                            index + 1
                        )
                    })
                    .trim_start()
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or_default()
                    .to_string();
                assert!(
                    !name.is_empty(),
                    "{path} line {}: could not read the exported symbol name",
                    index + 1
                );
                found.push((path.clone(), name));
            }
        }

        let mut expected: alloc::vec::Vec<(std::string::String, std::string::String)> =
            NON_FFI_UNMANGLED_SYMBOLS
                .iter()
                .map(|&(path, symbol, _)| (path.to_string(), symbol.to_string()))
                .collect();
        found.sort();
        expected.sort();
        assert_eq!(
            found, expected,
            "the set of unmangled symbols declared outside `src/ffi/**` changed. Every such \
             symbol is published by the `cdylib` unless it is given internal linkage, which \
             would make the exported surface differ between feature rows and widen the \
             drop-in `libz` ABI. Either move the symbol into an `src/ffi/` shim module, where \
             the FFI inventory governs it, or hide it and add it to NON_FFI_UNMANGLED_SYMBOLS."
        );

        // Property 2: each symbol is hidden, by a directive compiled under exactly
        // the predicate that compiles its definition.
        for (path, symbol, directive) in NON_FFI_UNMANGLED_SYMBOLS {
            let text = crate_sources()
                .into_iter()
                .find_map(|(candidate, text)| (candidate == path).then_some(text))
                .unwrap_or_else(|| panic!("{path} must exist"));
            // The directive is searched for in the RAW text: it is an assembler
            // string, so blanking literals would erase the very thing being
            // located. `blank_comments_and_literals` replaces bytes in place and
            // preserves length, so an offset found in the raw text is directly
            // comparable to a range derived from the blanked text — which is what
            // lets the gate membership below be checked without the surrounding
            // prose (which discusses this directive) producing a false match.
            let at = text.find(directive).unwrap_or_else(|| {
                panic!(
                    "{path} must carry `{directive}` so `{symbol}` gets internal linkage and \
                     stays out of `.dynsym`; without it the std-off `cdylib` exports it as \
                     FUNC GLOBAL DEFAULT and ELF interposition can select it"
                )
            });
            let statement = text[..at]
                .rfind("global_asm!")
                .map(|from| &text[from..at])
                .unwrap_or("");
            assert!(
                statement.trim_start_matches("global_asm!").starts_with('('),
                "`{directive}` in {path} must be emitted through `global_asm!`, not merely \
                 mentioned"
            );
            let blanked = blank_comments_and_literals(&text);
            if path == "src/lib.rs" {
                let runtime = no_std_support_range(&blanked);
                assert!(
                    runtime.contains(&at),
                    "`{directive}` must live inside `mod no_std_support`, alongside the \
                     definition of `{symbol}`, so it is compiled under exactly the predicate \
                     that compiles it — outside that gate it either hides nothing or names a \
                     symbol this build does not define"
                );
            }
        }
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
    const TOOLCHAIN_JOBS: [(&str, &str, &str); 9] = [
        (".github/workflows/ci.yml", "build-test", "stable"),
        (".github/workflows/ci.yml", "no-std-tests", "stable"),
        (".github/workflows/ci.yml", "lint", "stable"),
        (".github/workflows/ci.yml", "msrv", "1.85.0"),
        (".github/workflows/ci.yml", "benches", "stable"),
        (".github/workflows/ci.yml", "build-script-tests", "stable"),
        (".github/workflows/ci.yml", "unsafe-boundary", "stable"),
        (".github/workflows/ci.yml", "c-abi-linkage", "stable"),
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
        // `rust-toolchain.toml` must not claim that the `-D warnings` clippy
        // gate fails on the pinned floor's clippy. The gate is measured passing
        // (exit 0, zero diagnostics) on clippy 0.1.85 and 0.1.97 alike, so such
        // a claim would be false and would invite someone to "fix" a gate that
        // works. This guards the file against acquiring it.
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

    /// Returns the body of one top-level table from the repository manifest,
    /// i.e. the lines after `[<table>]` up to the next `[`-introduced header.
    ///
    /// Deliberately reads `Cargo.toml` rather than a baked copy of its values,
    /// because the property under test *is* what the manifest says. It works in a
    /// packaged crate as well as in the working tree: `cargo package` normalizes
    /// the manifest but keeps both `[lib]` and `[profile.release]`, and
    /// `CARGO_MANIFEST_DIR` resolves to the unpacked crate root there.
    fn manifest_table(table: &str) -> alloc::string::String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text =
            std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml must be readable");
        let header = alloc::format!("[{table}]");
        let start = text
            .lines()
            .position(|line| line.trim() == header)
            .unwrap_or_else(|| panic!("Cargo.toml must declare a `{header}` table"))
            + 1;
        text.lines()
            .skip(start)
            .take_while(|line| !line.trim_start().starts_with('['))
            .collect::<alloc::vec::Vec<_>>()
            .join("\n")
    }

    /// `[profile.release]` must NOT declare an `lto` key while `[lib]
    /// crate-type` still emits an rlib, because such a declaration cannot take
    /// effect and Cargo will not say so.
    ///
    /// Cargo builds `lib` + `cdylib` + `staticlib` from one rustc invocation, and
    /// rustc cannot run LTO for a unit that also emits an rlib, so Cargo silently
    /// drops any LTO-*enabling* value. Measured by reading the `--crate-name
    /// zlib_rs` command line out of `cargo build --release --verbose` while
    /// overriding the profile through `CARGO_PROFILE_RELEASE_LTO`: `true`, `"fat"`,
    /// `"thin"` and `false` all produce **no** `-C lto` flag, and only `"off"` is
    /// forwarded (as `-C lto=off`). A `lto = true` line therefore reads as a
    /// request that never happens — and a declared optimization that silently does
    /// nothing is worse than an honest absence, because it invites performance
    /// reasoning from a flag that is not there.
    ///
    /// This is a *manifest-level* guard on purpose. The defect is invisible to
    /// every build, test and lint gate: the crate compiles, the artifacts emit and
    /// nothing warns. Only the rustc command line shows it, and no job reads that.
    ///
    /// The `crate-type` half is what keeps the guard honest rather than dogmatic:
    /// if the rlib is ever dropped from the triple, fat LTO becomes available and
    /// this assertion is the thing that says so instead of silently forbidding a
    /// setting that would by then be legitimate.
    #[test]
    fn the_release_profile_declares_no_inert_lto_setting() {
        let lib = manifest_table("lib");
        let crate_types = lib
            .lines()
            .skip_while(|line| !line.trim_start().starts_with("crate-type"))
            .take_while(|line| !line.contains(']') || line.trim_start().starts_with("crate-type"))
            .collect::<alloc::vec::Vec<_>>()
            .join(" ");
        assert!(
            crate_types.contains("crate-type"),
            "[lib] must declare `crate-type`"
        );
        for expected in ["\"lib\"", "\"cdylib\"", "\"staticlib\""] {
            assert!(
                crate_types.contains(expected),
                "[lib] crate-type must keep {expected}; the three-artifact triple \
                 is a requirement of the migration. Found: {crate_types}"
            );
        }

        let release = manifest_table("profile.release");
        let declares_lto = release
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with("lto") && line[3..].trim_start().starts_with('='));
        assert!(
            !declares_lto,
            "[profile.release] declares an `lto` key while [lib] crate-type still \
             emits an rlib ({crate_types}). Cargo cannot forward an LTO-enabling \
             value for a unit that also emits an rlib, so the declaration is inert \
             and no other gate can see that. Remove the key, or remove `\"lib\"` \
             from crate-type first — and if you remove `\"lib\"`, relax this test \
             deliberately rather than by accident."
        );

        // The settings that DO reach rustc must still be declared, so removing the
        // inert key cannot be mistaken for abandoning release optimization.
        for expected in ["opt-level = 3", "codegen-units = 1", "panic = \"abort\""] {
            assert!(
                release.contains(expected),
                "[profile.release] must keep `{expected}`"
            );
        }
    }

    /// The CI `lint` job must run the clippy gate the repository documents,
    /// `--all-features` included.
    ///
    /// Without `--all-features` clippy silently skips every feature-gated unit:
    /// `tests/c_oracle.rs` is not linted at all (it carries `required-features =
    /// ["c-oracle"]`), and neither are the `inflate_strict` arms or the `no_std`
    /// runtime block. `lint` is the only job in the workflow that promotes
    /// warnings to errors, so a lint regression in any of them would have nowhere
    /// else to surface. `.cargo/config.toml` states this exact gate twice, which
    /// is what made the narrower CI spelling a documentation drift as well as a
    /// coverage gap.
    #[test]
    fn the_lint_job_runs_the_documented_clippy_gate() {
        let block = workflow_job_block(".github/workflows/ci.yml", "lint");
        assert!(
            block.contains("clippy --all-targets --all-features -- -D warnings"),
            "the ci.yml `lint` job must run `cargo clippy --all-targets \
             --all-features -- -D warnings`; without `--all-features` the \
             feature-gated surface (tests/c_oracle.rs, the inflate_strict arms, \
             the no_std runtime block) is never linted anywhere"
        );
        assert!(
            block.contains("fmt --all -- --check"),
            "the ci.yml `lint` job must also run `cargo fmt --all -- --check`"
        );

        // The gate the workflow runs and the gate the repository documents must be
        // the same string, or one of them is lying.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo_config = std::fs::read_to_string(root.join(".cargo/config.toml"))
            .expect(".cargo/config.toml must be readable");
        assert!(
            cargo_config.contains("clippy --all-targets --all-features -- -D warnings"),
            ".cargo/config.toml must keep documenting the same clippy gate the \
             `lint` job runs"
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

                // The cascade must be spelled with `cfg_if!`, the crate's declared
                // stand-in for the C `#if`/`#elif`/`#else` nests (AAP §0.5.1,
                // §0.5.3), and must be backed by the independent `const`
                // cross-check that a bare `cargo check --target <triple>` proves.
                assert!(
                    text.contains("cfg_if::cfg_if! {"),
                    "the OS_CODE cascade must be written as a cfg_if! chain, whose \
                     arms are mutually exclusive by construction because the macro \
                     negates every preceding predicate"
                );
                assert!(
                    text.contains("const _: () = assert!("),
                    "the cascade must keep its const-evaluated cross-check, which \
                     is what makes the Windows and Apple values provable from a \
                     host that cannot execute them"
                );

                // Each declaration must sit under its own arm of that chain.
                // Scanned on the raw text (the blanked copy erases the `"apple"`
                // literal) by walking back from every declaration over its doc
                // comment to the arm header immediately above it.
                // Capturing the literal alongside the arm is what makes the
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
                // Deliberately NOT sorted: in a `cfg_if!` chain the first matching
                // arm wins, so source order is load-bearing and the fallback must
                // come last. Asserting the ordered sequence therefore pins the
                // cascade itself, which per-arm value checks alone cannot do.
                assert_eq!(
                    arms,
                    alloc::vec![
                        ("if #[cfg(windows)] {", "10"),
                        ("} else if #[cfg(target_vendor = \"apple\")] {", "19"),
                        ("} else {", "3"),
                    ],
                    "the OS_CODE cascade must be exactly `10` on Windows (zutil.h \
                     L156-L158), then `19` on Apple (L168-L170), then the \
                     unconditional `3` Unix fallback (L187-L189) — in that order, so \
                     that precisely one arm compiles for any target and no target is \
                     left without one"
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
