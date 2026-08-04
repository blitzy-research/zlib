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
// items"). The level is `warn` rather than `deny` so that a plain `cargo build`
// stays usable while an item is being written, but the CI lint gate runs
// `cargo clippy --locked --all-targets --all-features -- -D warnings`, which
// promotes this lint to an error. An undocumented public item therefore cannot
// land, even though it does not break a local build.
#![warn(missing_docs)]
// Every `unsafe` block in SHIPPED crate code must carry an immediately-adjacent
// `// SAFETY:` justification (AAP §0.7.2 standard S2, serving User Constraint 3).
// The level is `warn` for the same reason as `missing_docs` above, and the same
// `-D warnings` CI gate promotes it to an error, so an unjustified `unsafe` block
// cannot reach the shipped library. The next line relaxes the lint to `allow`
// under `cfg(test)`, which scopes it to the emitted
// `cdylib`/`staticlib`/`rlib` — whose `unsafe` lives in `src/ffi/**` and in the
// private `no_std_support` block below — rather than to the crate's inline
// `#[cfg(test)]` unit tests.
#![warn(clippy::undocumented_unsafe_blocks)]
#![cfg_attr(test, allow(clippy::undocumented_unsafe_blocks))]
// `unsafe` is DENIED crate-wide, which makes the migration's unsafe-containment
// boundary (AAP §0.3.2 pattern C7 / §0.6.2 / §0.7.2 standard S2, serving User
// Constraint 3 "zero unsafe blocks in core compression logic") a HARD COMPILE
// ERROR rather than a convention a reader has to police. A stray `unsafe` block,
// `unsafe fn`, `unsafe impl`, or `unsafe extern` anywhere in
// `src/deflate/**`, `src/inflate/**`, `src/checksum/**`,
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
// extra top-level modules, and it is ACYCLIC: every `use crate::…` in the
// shipped library points at a strictly lower layer of
//
//     error / constants -> util -> checksum -> stream / gz_header
//                       -> {deflate, inflate} -> gz -> ffi
//
// so `stream`/`gz_header` and `deflate`/`inflate` are strict peers that never
// name each other. Nothing in the compiler enforces that — a Rust crate is one
// compilation unit, so an upward `use` would compile silently — which is why
// `tests::the_module_graph_has_no_upward_edges` re-derives the whole edge set
// from the source text and fails on any reference that does not point downward
// (AAP §0.4.2 B2). Two decisions keep it one-way: `stream` owns the engine
// state as an opaque `Box<dyn EngineState>` and names neither engine type, and
// the one-call façades are split into layer-3 C driver logic behind the
// `OneCallDeflate`/`OneCallInflate` port traits plus layer-6 engine-owning
// entry points, mirroring how `compress.c` and `uncompr.c` include `zlib.h`
// and drive the engine rather than sitting beside `zutil.h`.
//
// The one exception, enumerated in that test's `PUBLIC_REEXPORT_EXCEPTIONS`, is
// that `util` re-exports those layer-6 entry points back down with a `pub use`,
// because AAP §0.3.1 publishes them as `util::{compress, compress2, uncompress,
// uncompress2}` and dropping a public path is a source-breaking change. A
// re-export moves a *name*, not a dependency: no layer-3 code calls them, no
// layer-3 signature mentions them, and deleting the two `pub use` lines leaves
// `util` compiling byte-for-byte the same.
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
// surfaced. `compress.c` and `uncompr.c` are C translation units that include
// `zlib.h` and drive the engines, so their engine-owning halves are *defined*
// with the engines (`deflate` / `inflate`) while the engine-free sizing formula
// stays in `util`. All six are additionally re-exported from `util` itself, so
// both `zlib_rs::compress2` and `zlib_rs::util::compress2` resolve — those are
// the paths AAP §0.3.1 publishes, and a `pub use` costs the layer graph nothing
// because it re-exports a name rather than creating a code dependency
// (AAP §0.4.2 B2).
pub use deflate::{compress, compress2};
pub use inflate::{uncompress, uncompress2};
pub use util::{compress_bound, compressBound};

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

    /// Every allocation on the hook-backed allocation path is **fallible**.
    ///
    /// zlib answers heap exhaustion with `Z_MEM_ERROR`; it never aborts. The two
    /// modules that own hook-backed memory must therefore reach the global
    /// allocator only through the fallible `try_box`, never through `Box::new`,
    /// whose failure path is `handle_alloc_error` and hence process abort:
    ///
    /// * `src/stream.rs` holds the ownership model — `AllocBuffer`, `EngineBox`
    ///   and `EngineReservation`;
    /// * `src/ffi/alloc.rs` supplies its `ForeignBuffer` implementation, the only
    ///   code that touches the region a caller's `zalloc` returned.
    ///
    /// Preserving C's allocation *count* means the small owning handles this port
    /// needs cannot become extra `zalloc` requests, so they come from the global
    /// heap — which is exactly why their failure has to be reportable (AAP
    /// §0.6.5). `EngineReservation::fill` is the concrete case: it boxes the engine
    /// state through `try_box` while still holding the caller's charge, so an
    /// exhausted global heap surfaces as `Z_MEM_ERROR` and the charge goes back
    /// through the caller's `zfree` instead of leaking.
    ///
    /// One occurrence is sanctioned — the zero-sized fast path *inside* `try_box`
    /// itself, where the allocator is provably never consulted.
    #[test]
    fn hook_backed_placement_never_uses_infallible_box_new() {
        let sources = crate_sources();
        let mut scanned = 0usize;

        for (rel, text) in &sources {
            let is_model = rel.as_str() == "src/stream.rs";
            let is_boundary = rel.as_str() == "src/ffi/alloc.rs";
            if !is_model && !is_boundary {
                continue;
            }
            scanned += 1;

            let shipped = blank_cfg_test_items(&blank_comments_and_literals(text));
            let hits = shipped.matches("Box::new").count();
            let expected = usize::from(is_boundary);
            assert_eq!(
                hits, expected,
                "{rel} has {hits} shipped `Box::new` occurrence(s) but must have \
                 {expected}. Hook-backed placement allocates only through \
                 `try_box`: `Box::new` aborts the process on heap exhaustion, \
                 whereas zlib reports it as Z_MEM_ERROR (AAP §0.6.5)."
            );
        }
        assert_eq!(
            scanned, 2,
            "both placement modules must have been scanned; the paths in this test \
             are stale if they were renamed"
        );

        // Pin the one sanctioned occurrence in place, so it cannot be joined by
        // another that merely inherits its exemption.
        let boundary = sources
            .iter()
            .find(|(rel, _)| rel.as_str() == "src/ffi/alloc.rs")
            .map(|(_, text)| blank_cfg_test_items(&blank_comments_and_literals(text)))
            .expect("src/ffi/alloc.rs is part of the crate");
        let at = boundary
            .find("Box::new")
            .expect("the sanctioned occurrence must still be present");
        let opens = boundary[..at]
            .rfind("fn try_box")
            .expect("the sanctioned `Box::new` must sit inside `try_box` itself");
        assert!(
            boundary[opens..at].contains("layout.size() == 0"),
            "the sanctioned `Box::new` must be guarded by the zero-size test that \
             makes it allocation-free"
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

    // -----------------------------------------------------------------------
    // Layer architecture — the acyclic seven-layer module graph (AAP §0.3.1,
    // §0.4.2 B2)
    //
    // The module-declaration comment above claims the graph "mirrors the C
    // `#include` layering exactly ... with no extra top-level modules", and the
    // AAP fixes the ordering as
    //
    //     error / constants -> util -> checksum -> stream / gz_header
    //                       -> {deflate, inflate} -> gz -> ffi
    //
    // where every arrow means "the right side may name the left side". Nothing in
    // the compiler enforces that: a Rust crate is one compilation unit, so an
    // upward `use` compiles perfectly and the layering silently degrades into a
    // cycle. The check below re-derives the whole edge set from the source text
    // and fails on any reference that does not point strictly downward, which is
    // what turns the documented architecture into an enforced one.
    //
    // The scan is deliberately two-tiered, because the shipped library and the
    // `#[cfg(test)]` configuration are different compilation units with different
    // obligations:
    //
    //   * Shipped code — every `.rs` file with its `#[cfg(test)]` items removed —
    //     must be *strictly* one-way. No upward edge and no same-layer edge, so
    //     the library that is actually built, linked, and published has an
    //     acyclic graph in which each layer can be read, reviewed, and reasoned
    //     about without the layers above it. This is the claim AAP §0.3.1 makes
    //     and the only tier the published artifacts depend on.
    //
    //   * `#[cfg(test)]` code may hold a small, *enumerated* set of exceptions
    //     (`TEST_ONLY_CROSS_LAYER_EXCEPTIONS`). Two facts make a blanket ban
    //     counter-productive there: a caller-hook test needs a real C
    //     `alloc_func`/`free_func` pair, which requires `unsafe` and therefore
    //     may only be built inside `src/ffi/**`; and an engine round-trip test
    //     needs the inverse engine, which is the only way to verify emitted bytes
    //     without `std` or a third-party codec. Neither can place an edge in the
    //     shipped graph. The exceptions are an allow-list rather than a blanket
    //     exemption precisely so a *new* test-only inversion fails this test
    //     until it is justified here, and so a stale entry fails it too.
    // -----------------------------------------------------------------------

    /// The ten top-level modules of the crate and their AAP §0.3.1 layer numbers.
    ///
    /// `stream` and `gz_header` share layer 5, and `deflate` and `inflate` share
    /// layer 6 — they are strict peers, so neither may name the other either.
    /// A reference is legal only when it points at a *strictly lower* number.
    const MODULE_LAYERS: [(&str, u8); 10] = [
        ("error", 1),
        ("constants", 2),
        ("util", 3),
        ("checksum", 4),
        ("stream", 5),
        ("gz_header", 5),
        ("deflate", 6),
        ("inflate", 6),
        ("gz", 7),
        ("ffi", 8),
    ];

    /// The complete set of `(from, to, why)` module references that shipped code
    /// may hold **only** in the form `pub use crate::<to>::…;` — a re-export of a
    /// *name*, never an import that creates a code dependency.
    ///
    /// The distinction is the whole content of the exemption, and it is exact. An
    /// arrow in AAP §0.4.2 B2 means "the right side may *depend on* the left
    /// side": the ordering exists so that each layer can be compiled, read, and
    /// reasoned about without the layers above it. A `pub use` does none of that.
    /// It publishes an item under a second path and adds no call, no field, no
    /// trait bound, and no type in any signature the lower layer owns; delete
    /// every re-export listed here and the lower layer compiles unchanged.
    ///
    /// The exemption is needed because AAP §0.3.1 and §0.4.2 B2 constrain the same
    /// two names from opposite directions. §0.3.1 publishes the one-call surface as
    /// `util::{compress, compress_bound, compress2, compressBound, uncompress,
    /// uncompress2}`, so `zlib_rs::util::compress2` is a promised public path and
    /// removing it is a source-breaking change for a downstream `use`. §0.4.2 B2
    /// forbids the utility layer from *driving* an engine, which is why the
    /// concrete entry points are defined in [`crate::deflate`] and
    /// [`crate::inflate`] — they must name a concrete engine adapter, and layer 3
    /// may not. Re-exporting the finished functions back down to the promised path
    /// satisfies both readings with no code dependency in either direction.
    ///
    /// Scope is deliberately minimal, and three separate conditions must all hold
    /// for a reference to be exempt: the `(from, to)` pair appears below, the text
    /// at the reference is literally `pub use crate::<to>::`, and — as with
    /// [`TEST_ONLY_CROSS_LAYER_EXCEPTIONS`] — the entry is actually exercised, so a
    /// stale one fails this test as loudly as an unlisted violation. A plain `use`,
    /// a call, a type mention, or a second reference anywhere else in the file is
    /// an ordinary violation and still fails.
    const PUBLIC_REEXPORT_EXCEPTIONS: [(&str, &str, &str); 2] = [
        (
            "util",
            "deflate",
            "AAP §0.3.1 publishes the one-call compression entry points at \
             util::{compress, compress2}, but §0.4.2 B2 forbids layer 3 from driving \
             an engine, so they are defined in crate::deflate and re-exported back \
             down to the promised path; src/util/** never calls them",
        ),
        (
            "util",
            "inflate",
            "AAP §0.3.1 publishes the one-call decompression entry points at \
             util::{uncompress, uncompress2}, defined in crate::inflate for the same \
             reason and re-exported back down on the same terms",
        ),
    ];

    /// Whether the `crate::…` reference at byte offset `at` in `shipped` is a
    /// `pub use` re-export rather than a dependency-creating reference.
    ///
    /// Walks backwards over whitespace only — the blanker turns comments into
    /// spaces and preserves offsets, so a `crate::deflate` mentioned in a doc
    /// comment has already been erased — and requires the immediately preceding
    /// non-whitespace text to be exactly the keywords `pub use`. Any other
    /// context, including a bare `use crate::deflate::…`, a call, or a type
    /// position, is therefore *not* a re-export and stays a violation.
    fn is_pub_use_reexport(shipped: &str, at: usize) -> bool {
        let head = &shipped[..at];
        head.trim_end().ends_with("pub use")
    }

    /// The complete set of `(from, to, why)` module references that are permitted
    /// **only** inside `#[cfg(test)]` code, and never in the shipped library.
    ///
    /// Every entry must be exercised — a stale one fails
    /// [`the_module_graph_has_no_upward_edges`](fn@the_module_graph_has_no_upward_edges)
    /// just as loudly as an unlisted one — so this list cannot drift away from
    /// the code it describes.
    const TEST_ONLY_CROSS_LAYER_EXCEPTIONS: [(&str, &str, &str); 3] = [
        (
            "stream",
            "ffi",
            "the has-hook contract (AAP §0.6.3) can only be asserted against a real C \
             alloc_func/free_func pair, and building one needs raw-pointer `unsafe`, \
             which is permitted only under src/ffi/** (AAP §0.6.2); the counting hook \
             therefore lives in crate::ffi::alloc::test_hook and is merely driven from \
             the layer-5 tests",
        ),
        (
            "deflate",
            "inflate",
            "emitted-byte assertions decode with the crate's own inflate engine, which \
             keeps the deflate unit tests free of `std` and of any third-party codec so \
             they run in every feature configuration",
        ),
        (
            "inflate",
            "deflate",
            "decoder fixtures are produced by the crate's own deflate engine for the \
             same reason, rather than by baking a second copy of the reference vectors",
        ),
    ];

    /// The top-level module a `src/`-relative path belongs to, or [`None`] for
    /// `src/lib.rs` itself.
    ///
    /// `src/lib.rs` is the crate root — the API curator that declares every
    /// module and therefore legitimately names all of them — so it is excluded
    /// from the edge scan. Every other file resolves to exactly one module:
    /// `src/stream.rs` to `stream`, `src/deflate/state.rs` to `deflate`.
    fn owning_module(rel: &str) -> Option<&'static str> {
        let tail = rel.strip_prefix("src/")?;
        if tail == "lib.rs" {
            return None;
        }
        let name = match tail.split_once('/') {
            Some((dir, _)) => dir,
            None => tail.strip_suffix(".rs")?,
        };
        MODULE_LAYERS
            .iter()
            .find(|(m, _)| *m == name)
            .map(|(m, _)| *m)
    }

    /// The layer number of a top-level module.
    fn layer_of(module: &str) -> u8 {
        MODULE_LAYERS
            .iter()
            .find(|(m, _)| *m == module)
            .map(|(_, l)| *l)
            .unwrap_or_else(|| panic!("{module} is not one of the crate's top-level modules"))
    }

    /// Every `crate::<module>` reference in `blanked`, as
    /// `(byte offset, 1-based line, module)`.
    ///
    /// The input must already have had comments and string literals blanked, so a
    /// doc comment that *mentions* `[`crate::deflate`]` — `src/stream.rs` has
    /// several — is not mistaken for a dependency. A path such as
    /// `crate::compress2` names a crate-root re-export rather than a module and is
    /// skipped, because the root sits above every layer.
    ///
    /// The byte offset is returned alongside the line so a caller can ask whether
    /// the reference survives [`blank_cfg_test_items`] and therefore whether it
    /// belongs to the shipped library or only to the test configuration.
    fn crate_path_edges(blanked: &str) -> alloc::vec::Vec<(usize, usize, &'static str)> {
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let bytes = blanked.as_bytes();
        let mut edges = alloc::vec::Vec::new();
        let mut from = 0usize;
        while let Some(rel) = blanked[from..].find("crate::") {
            let at = from + rel;
            from = at + "crate::".len();
            // Whole-word `crate` only: `my_crate::x` is a different crate.
            if at > 0 && ident(bytes[at - 1]) {
                continue;
            }
            let rest = &blanked[from..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let head = &rest[..end];
            if let Some((module, _)) = MODULE_LAYERS.iter().find(|(m, _)| *m == head) {
                let line = blanked[..at].bytes().filter(|&c| c == b'\n').count() + 1;
                edges.push((at, line, *module));
            }
            from += end;
        }
        edges
    }

    /// Blanks every `#[cfg(test)]`-gated item, leaving exactly the text the
    /// shipped library is compiled from.
    ///
    /// Offsets and line numbers are preserved — each removed byte becomes a space
    /// and newlines are kept — so an offset taken from the input still addresses
    /// the same source position in the output, which is what lets one
    /// [`crate_path_edges`] scan be classified against both texts.
    ///
    /// An attribute counts as test-gating when its predicate names `test` as a
    /// whole word and does not negate it. That covers every form the crate
    /// actually uses — `#[cfg(test)]`, `#[cfg(all(test, feature = "std"))]` and
    /// `#[cfg(all(test, target_endian = "big"))]` — while leaving the
    /// `#[cfg(not(test))]` and `#[cfg(all(not(feature = "std"), not(test), ...))]`
    /// production items in place. The gated item ends at the first `;` or the
    /// first brace-balanced `{ ... }` after the attribute, which is the shape of
    /// every gated item here: a `mod`, a `fn`, a `use`, or a `const`.
    fn blank_cfg_test_items(blanked: &str) -> std::string::String {
        let bytes = blanked.as_bytes();
        let mut out = bytes.to_vec();
        // Whole-word search, so neither `latest` nor `test_hook` counts as `test`.
        let names_test = |attr: &str| {
            let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
            let a = attr.as_bytes();
            let mut i = 0usize;
            while let Some(rel) = attr[i..].find("test") {
                let at = i + rel;
                i = at + "test".len();
                let before_ok = at == 0 || !ident(a[at - 1]);
                let after_ok = i >= a.len() || !ident(a[i]);
                if before_ok && after_ok {
                    return true;
                }
            }
            false
        };
        let mut from = 0usize;
        while let Some(rel) = blanked[from..].find("#[cfg(") {
            let at = from + rel;
            // End of the attribute: the `]` closing the opening `#[`.
            let mut depth = 0i32;
            let mut i = at + 1;
            let mut attr_end = None;
            while i < bytes.len() {
                match bytes[i] {
                    b'[' | b'(' => depth += 1,
                    b']' | b')' => {
                        depth -= 1;
                        if depth == 0 {
                            attr_end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let Some(attr_end) = attr_end else { break };
            from = attr_end + 1;
            let attr = &blanked[at..=attr_end];
            if !names_test(attr) || attr.contains("not(test)") {
                continue;
            }
            // End of the gated item: the first `;`, or the first balanced `{...}`.
            let mut j = attr_end + 1;
            let mut item_end = None;
            while j < bytes.len() {
                match bytes[j] {
                    b';' => {
                        item_end = Some(j);
                        break;
                    }
                    b'{' => {
                        let mut d = 0i32;
                        let mut k = j;
                        while k < bytes.len() {
                            match bytes[k] {
                                b'{' => d += 1,
                                b'}' => {
                                    d -= 1;
                                    if d == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            k += 1;
                        }
                        item_end = Some(k.min(bytes.len().saturating_sub(1)));
                        break;
                    }
                    _ => {}
                }
                j += 1;
            }
            let Some(item_end) = item_end else { break };
            for b in &mut out[at..=item_end] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
            from = item_end + 1;
        }
        std::string::String::from_utf8(out)
            .expect("only ASCII bytes are overwritten, so the text stays valid UTF-8")
    }

    /// The `inflateBack` decode path must contain no owned buffer and perform no
    /// allocation, exactly as `infback.c` allocates only inside
    /// `inflateBackInit_` (its one `ZALLOC`, L51) and nothing thereafter.
    ///
    /// Copying each provider chunk into an infallibly growing `Vec` would satisfy
    /// every behavioural test while turning a caller-driven input pattern into
    /// caller-driven heap pressure and, on the `no_std` `cdylib`, into `malloc`
    /// traffic during decode. The two-method [`crate::inflate::InFunc`] split is
    /// what makes such a buffer unnecessary; the guard is mechanical because a
    /// behavioural test cannot see an allocation that merely *could* happen on a
    /// larger input.
    ///
    /// Scope is the shipped body of `src/inflate/back.rs` only: `#[cfg(test)]`
    /// items legitimately build `Vec`s to hold expected output, and the single
    /// `Box` that owns the state is C's own allocation.
    #[test]
    fn the_back_inflate_decode_path_owns_no_buffer() {
        let sources = crate_sources();
        let (_, text) = sources
            .iter()
            .find(|(rel, _)| rel == "src/inflate/back.rs")
            .expect("src/inflate/back.rs must be part of the crate");

        // Comments and string literals are blanked so prose describing the old
        // shape cannot trip the scan, then `#[cfg(test)]` items are removed so only
        // the shipped decoder is examined.
        let shipped = blank_cfg_test_items(&blank_comments_and_literals(text));

        // Every construct that would put an owned, growable buffer on the decode
        // path. `Box` is deliberately absent from this list: it is how the state
        // itself is held, matching C's single `ZALLOC`.
        const ALLOCATING: [&str; 8] = [
            "Vec<",
            "Vec::",
            "vec!",
            ".to_vec()",
            "extend_from_slice",
            "with_capacity",
            "String",
            ".reserve(",
        ];
        let mut found = alloc::vec::Vec::new();
        for (n, line) in shipped.lines().enumerate() {
            for needle in ALLOCATING {
                if line.contains(needle) {
                    found.push(alloc::format!(
                        "src/inflate/back.rs:{}: `{needle}` on the decode path",
                        n + 1
                    ));
                }
            }
        }
        assert!(
            found.is_empty(),
            "the inflateBack decode path must allocate nothing (infback.c allocates \
             only in inflateBackInit_); found {} occurrence(s):\n{}",
            found.len(),
            found.join("\n")
        );

        // Non-vacuity: the scan must actually have seen the decoder, not an empty
        // string produced by an over-eager blanker.
        assert!(
            shipped.contains("fn pull(&mut self)") && shipped.contains("fn inflate_back<"),
            "the blanked source no longer contains the decoder; the scan would \
             pass vacuously"
        );
        // And the cursor-only `BackCtx` must still read through the provider
        // rather than from a field of its own.
        assert!(
            shipped.contains("self.src.chunk()"),
            "the decoder must read input through InFunc::chunk, not from an owned \
             buffer"
        );
    }

    /// **No `use` in the shipped library creates an upward, or sideways, code
    /// dependency in the layer graph — and every exception is one of five
    /// enumerated ones.**
    ///
    /// This is the mechanical form of AAP §0.4.2 B2. It scans every `.rs` file
    /// under `src/` twice: once with the `#[cfg(test)]` items removed, which is
    /// the library that is actually built and published and must be *strictly*
    /// one-way; and once whole, so that a test-only reference is checked against
    /// [`TEST_ONLY_CROSS_LAYER_EXCEPTIONS`] rather than ignored. Every violation
    /// is reported at once with its file, line, tier and both layer numbers, so a
    /// regression is diagnosable without re-running the analysis by hand.
    ///
    /// Shipped code holds exactly two exemptions, both in
    /// [`PUBLIC_REEXPORT_EXCEPTIONS`] and both of the form `pub use
    /// crate::<higher>::…;` — a re-export publishing a name at the second path
    /// AAP §0.3.1 promises, which creates no call, no field, no bound and no
    /// signature and therefore no dependency. `#[cfg(test)]` code holds three more,
    /// in [`TEST_ONLY_CROSS_LAYER_EXCEPTIONS`].
    ///
    /// Four structural facts are asserted alongside it, each closing a way the
    /// check could pass while measuring nothing:
    ///
    /// 1. every `src/` file classifies into one of the ten declared modules, so a
    ///    new top-level module cannot slip in unscanned;
    /// 2. the shipped graph is non-trivial (> 40 inter-module edges), so a
    ///    blanking bug that erases the source cannot masquerade as compliance;
    /// 3. `#[cfg(test)]` blanking removed something but not everything, so the
    ///    two tiers are genuinely different texts;
    /// 4. every entry in *both* exception lists is actually exercised, so a stale
    ///    exemption fails just as loudly as an unlisted violation.
    #[test]
    fn the_module_graph_has_no_upward_edges() {
        let sources = crate_sources();

        // Guard 1: every `src/` file must classify into a known module, so the
        // scan cannot be silently incomplete.
        let mut classified = 0usize;
        for (rel, _) in &sources {
            if rel == "src/lib.rs" {
                continue;
            }
            assert!(
                owning_module(rel).is_some(),
                "{rel} does not belong to any of the {} declared top-level modules; \
                 add it to MODULE_LAYERS with its AAP §0.3.1 layer",
                MODULE_LAYERS.len()
            );
            classified += 1;
        }
        assert!(
            classified > 30,
            "expected the whole module tree, classified only {classified} files"
        );

        // Guard 2: collect the edge set, tier by tier.
        let mut violations = alloc::vec::Vec::new();
        let mut shipped_edges = 0usize;
        let mut test_edges = 0usize;
        let mut exceptions_used = [0usize; TEST_ONLY_CROSS_LAYER_EXCEPTIONS.len()];
        let mut reexports_used = [0usize; PUBLIC_REEXPORT_EXCEPTIONS.len()];
        for (rel, text) in &sources {
            let Some(from) = owning_module(rel) else {
                continue;
            };
            let blanked = blank_comments_and_literals(text);
            let shipped = blank_cfg_test_items(&blanked);
            let shipped_bytes = shipped.as_bytes();
            for (at, line, to) in crate_path_edges(&blanked) {
                if to == from {
                    continue; // an intra-module path is not a graph edge
                }
                // The blanker preserves offsets, so the reference belongs to the
                // shipped library exactly when its first byte survived.
                let in_shipped = shipped_bytes[at] == b'c';
                let (lf, lt) = (layer_of(from), layer_of(to));
                if in_shipped {
                    shipped_edges += 1;
                    if lt >= lf {
                        // A `pub use crate::<to>::…;` publishes a name at a second
                        // path and creates no dependency, so an enumerated one is
                        // exempt; everything else is a violation.
                        match PUBLIC_REEXPORT_EXCEPTIONS
                            .iter()
                            .position(|(f, t, _)| *f == from && *t == to)
                            .filter(|_| is_pub_use_reexport(&shipped, at))
                        {
                            Some(idx) => reexports_used[idx] += 1,
                            None => {
                                let direction = if lt == lf { "SAME-LAYER" } else { "UPWARD" };
                                violations.push(alloc::format!(
                                    "{rel}:{line}: SHIPPED {direction} \
                                     {from}(layer {lf}) -> {to}(layer {lt})"
                                ));
                            }
                        }
                    }
                    continue;
                }
                test_edges += 1;
                if lt < lf {
                    continue; // a downward reference needs no exemption
                }
                match TEST_ONLY_CROSS_LAYER_EXCEPTIONS
                    .iter()
                    .position(|(f, t, _)| *f == from && *t == to)
                {
                    Some(idx) => exceptions_used[idx] += 1,
                    None => violations.push(alloc::format!(
                        "{rel}:{line}: CFG(TEST) {from}(layer {lf}) -> {to}(layer {lt}) \
                         is not in TEST_ONLY_CROSS_LAYER_EXCEPTIONS — either point it \
                         downward or justify it there"
                    )),
                }
            }
        }

        // Guard 3: the scan must actually have seen the shipped graph, and the two
        // tiers must be genuinely distinct texts.
        assert!(
            shipped_edges > 40,
            "only {shipped_edges} shipped inter-module edges found — the scan is not \
             seeing the graph"
        );
        assert!(
            test_edges > 0,
            "no #[cfg(test)] inter-module edge was seen at all, so blank_cfg_test_items \
             is removing more than the test configuration"
        );

        assert!(
            violations.is_empty(),
            "the seven-layer module graph must be strictly one-way (AAP §0.3.1, \
             §0.4.2 B2); found {} violation(s):\n{}",
            violations.len(),
            violations.join("\n")
        );

        // Guard 4: no stale exemption in either list. An entry that stops being
        // needed must be deleted, or the list stops describing the code.
        for (idx, (from, to, why)) in TEST_ONLY_CROSS_LAYER_EXCEPTIONS.iter().enumerate() {
            assert!(
                exceptions_used[idx] > 0,
                "TEST_ONLY_CROSS_LAYER_EXCEPTIONS still exempts {from} -> {to} but no \
                 #[cfg(test)] code needs it any more; delete the entry (rationale on \
                 record: {why})"
            );
        }
        for (idx, (from, to, why)) in PUBLIC_REEXPORT_EXCEPTIONS.iter().enumerate() {
            assert!(
                reexports_used[idx] > 0,
                "PUBLIC_REEXPORT_EXCEPTIONS still exempts a {from} -> {to} re-export but \
                 no `pub use crate::{to}::…;` remains in src/{from}/**; delete the entry \
                 (rationale on record: {why})"
            );
        }
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
    const TOOLCHAIN_JOBS: [(&str, &str, &str); 10] = [
        (".github/workflows/ci.yml", "build-test", "stable"),
        (".github/workflows/ci.yml", "no-std-tests", "stable"),
        (".github/workflows/ci.yml", "lint", "stable"),
        // rustdoc's diagnostic set moves with the compiler, so `-D warnings` on
        // the MSRV floor's rustdoc is a different gate from the one this project
        // documents as blocking.
        (".github/workflows/ci.yml", "docs", "stable"),
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
        // Counts the jobs clause 4 below actually inspected, so that clause cannot
        // quietly stop matching and assert nothing.
        let mut lints = 0usize;
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

            // 4. A job that LINTS needs more than a channel: `cargo fmt` and
            //    `cargo clippy` live in components the install step provisions
            //    only when it names them. Reading non-comment lines only, because
            //    these files narrate each other's commands in prose — `fuzz.yml`'s
            //    header quotes `ci.yml`'s `cargo fmt --all -- --check` while
            //    running neither.
            let mut commands = std::string::String::new();
            for line in block.lines() {
                if line.trim_start().starts_with('#') {
                    continue;
                }
                commands.push_str(line);
                commands.push('\n');
            }
            if !commands.contains("cargo fmt") && !commands.contains("cargo clippy") {
                continue;
            }
            lints += 1;
            // `(component, the probe that proves it resolved, the shim rustup
            // names when it did not)`.
            for (component, probe, shim) in [
                ("rustfmt", "cargo fmt --version", "cargo-fmt"),
                ("clippy", "cargo clippy --version", "cargo-clippy"),
            ] {
                assert!(
                    commands
                        .lines()
                        .filter_map(|line| line.trim().strip_prefix("components:"))
                        .any(|named| named.split(',').any(|one| one.trim() == component)),
                    "{workflow} job `{job}` runs cargo fmt/clippy, so its toolchain \
                     step must name `{component}` in `components:`; without it the \
                     subcommand has no `{shim}` to dispatch to, and the gate fails \
                     on a missing component instead of on the sources it exists to \
                     read"
                );
                assert!(
                    commands.contains(probe),
                    "{workflow} job `{job}` must run `{probe}` in its toolchain \
                     verification step, so a dropped `components:` entry is named \
                     right there instead of surfacing minutes later as \
                     `'{shim}' is not installed`"
                );
            }
        }
        assert_eq!(
            lints, 2,
            "expected exactly two linting jobs among the toolchain-pinned ones \
             (ci.yml `lint` and fuzz.yml `cargo-fuzz`), saw {lints} — either a job \
             started linting without being held to this contract (name the \
             components, probe them, raise this count) or the scan stopped matching \
             and clause 4 now asserts nothing"
        );
    }

    /// Every `ci.yml` job must name the commit it is judging, before it judges it.
    ///
    /// A green verdict is only evidence if the log says which commit it describes.
    /// A retained build, test, lint, MSRV, symbol or oracle log that names no
    /// commit cannot be attributed to the tree it was meant to prove, and it is
    /// indistinguishable from one produced against a different clone entirely.
    /// Prose cannot close that gap, because the defect is the absence of an
    /// identifier in the log itself. So the remedy is structural: every job emits
    /// its commit identity as its FIRST act after checkout, which makes every line
    /// that follows self-describing.
    ///
    /// ORDER IS THE WHOLE POINT, and is asserted rather than assumed. Provenance
    /// printed at the END of a job is worthless precisely when it matters most —
    /// a job that dies in its third step produces a log with a failure and no
    /// commit. Hence the check is positional: checkout first, provenance second,
    /// gates afterwards.
    ///
    /// SCOPED TO `ci.yml` DELIBERATELY. These twelve jobs are the ones whose
    /// output a reader attributes to a commit — the build, test, lint, MSRV,
    /// symbol and oracle gates. `audit.yml` and `fuzz.yml` number their steps in
    /// prose comments (`# 2.`, `# 3.`, …), so inserting a step there would mean
    /// renumbering commentary unrelated to this contract — churn that buys no
    /// additional attributability for the gates named.
    ///
    /// The job list is read from the file, not hard-coded, so a newly added job
    /// cannot escape the requirement by not being mentioned here.
    #[test]
    fn every_ci_job_records_the_commit_it_is_judging() {
        const WORKFLOW: &str = ".github/workflows/ci.yml";
        let jobs = workflow_job_names(WORKFLOW);
        assert_eq!(
            jobs.len(),
            12,
            "expected 12 `ci.yml` jobs to hold to the provenance contract, found \
             {}: {jobs:?}",
            jobs.len()
        );

        for job in &jobs {
            let block = workflow_job_block(WORKFLOW, job);

            // 1. Positional: checkout, then provenance, then everything else.
            //    Reading the step names in order is what makes this an ordering
            //    assertion rather than a mere presence check.
            //
            //    INDENTATION IS THE DISCRIMINATOR, exactly as it is for the job
            //    keys themselves. Steps are the FOUR-space-indented sequence
            //    items; `build-test`'s `matrix.include:` rows are also `- name:`
            //    entries but sit deeper, so trimming the leading whitespace would
            //    read a matrix row as the job's first step.
            let steps: alloc::vec::Vec<&str> = block
                .lines()
                .filter_map(|l| l.strip_prefix("    - name: "))
                .collect();
            assert_eq!(
                steps.first().copied(),
                Some("Checkout repository"),
                "`ci.yml` job `{job}` must begin by checking out the repository"
            );
            assert_eq!(
                steps.get(1).copied(),
                Some("Record the commit under test"),
                "`ci.yml` job `{job}` must record the commit under test \
                 IMMEDIATELY after checkout, so a job that fails early still \
                 leaves an attributable log; found {:?}",
                steps.get(1)
            );

            // 2. All three witnesses. They answer different questions — what the
            //    event named, what the checkout produced, and what the files
            //    actually contain — and any one alone leaves a gap the review
            //    already walked through.
            //
            //    EACH IS MATCHED AS A WHOLE `echo` STATEMENT, not as a bare token.
            //    `${{ github.sha }}` also appears in the step's own `case` arm, so
            //    searching for the token alone stays satisfied after the line that
            //    PRINTS it is deleted — the witness would then be compared against
            //    but never recorded, which is precisely the unattributable log
            //    this contract exists to prevent. (Verified by mutation: the token
            //    form lets a removed `event sha` line through.)
            for needle in [
                "head=\"$(git rev-parse HEAD)\"",
                "echo \"event sha : ${{ github.sha }}\"",
                "echo \"head sha  : $head\"",
                "echo \"tree hash : $(git rev-parse 'HEAD^{tree}')\"",
                "echo \"ref       : ${{ github.ref }}\"",
            ] {
                assert!(
                    block.contains(needle),
                    "`ci.yml` job `{job}`'s provenance step must emit `{needle}`"
                );
            }

            // 3. The mismatch check must actually fail the job. A branch that only
            //    prints is indistinguishable from no check at all, and it is
            //    latent: the arm is not taken while the checkout is correct, so
            //    nothing else would ever reveal that it does not bite.
            //
            //    THE DIAGNOSTIC AND THE `exit 1` ARE ASSERTED AS ONE STRING, not
            //    as two independent `contains` calls. Every job here already runs
            //    some other `exit 1` — the toolchain assertion, at least — so a
            //    free-standing search for `exit 1` is satisfied by a completely
            //    different branch and would pass with the provenance failure
            //    deleted. Requiring them adjacent is what binds the failure to
            //    this arm. (Verified by mutation: splitting the two lets a
            //    print-only provenance branch through.)
            assert!(
                block.contains("nobody asked about.\"; exit 1 ;;"),
                "`ci.yml` job `{job}`'s provenance step must reject a HEAD that is \
                 neither the event sha nor the pull-request head sha, and must \
                 `exit 1` in that same branch — otherwise the check cannot fail \
                 the job"
            );
        }
    }

    /// Workflow files whose supply-chain invariants the tests below enforce.
    ///
    /// Every entry is held to three properties: each `uses:` names a full commit
    /// SHA, each `actions/checkout` drops its credentials, and each
    /// `dtolnay/rust-toolchain` states its channel explicitly. Enumerating the
    /// files here rather than scanning the directory keeps the contract
    /// reviewable, and is also what makes the census self-maintaining: the counts
    /// derive from the files themselves, so a hand-written tally cannot go stale.
    ///
    /// The list is exhaustive over `.github/workflows/`, and
    /// [`every_workflow_file_is_covered_by_the_supply_chain_contract`] proves it,
    /// so a newly added workflow cannot escape the contract by simply not being
    /// mentioned here.
    const SUPPLY_CHAIN_WORKFLOWS: [&str; 3] = [
        ".github/workflows/audit.yml",
        ".github/workflows/ci.yml",
        ".github/workflows/fuzz.yml",
    ];

    /// The action census, per workflow: `(workflow, checkout, toolchain, cache,
    /// upload-artifact)`.
    ///
    /// `audit.yml`'s header states these totals in prose, where nothing holds them
    /// to the files they describe. Asserting the numbers here means the prose
    /// cannot drift without a red test, and it also catches the quieter direction
    /// of drift: an action added to a job that nobody reviewed as an action
    /// change.
    const ACTION_CENSUS: [(&str, usize, usize, usize, usize); 3] = [
        (".github/workflows/audit.yml", 4, 3, 0, 0),
        (".github/workflows/ci.yml", 12, 12, 0, 1),
        (".github/workflows/fuzz.yml", 1, 1, 5, 1),
    ];

    /// Every tool a workflow installs into CI, with the version it is pinned to.
    ///
    /// These are executables that run with full access to the checked-out tree, so
    /// they are dependencies in every sense that matters even though no manifest
    /// mentions them. `cargo-fuzz` is listed for the same reason as the other two:
    /// installed without a version, `cargo install` builds and executes whatever
    /// crates.io serves at that moment.
    const CI_INSTALLED_TOOLS: [(&str, &str); 3] = [
        ("cargo-audit", "0.22.2"),
        ("cargo-deny", "0.20.2"),
        ("cargo-fuzz", "0.13.2"),
    ];

    /// First-party Cargo subcommands that RESOLVE the dependency graph and accept
    /// `--locked`.
    ///
    /// `fmt` is deliberately absent: it accepts no such flag. So is `fuzz` — a
    /// third-party subcommand that forwards no lockfile flag, which is why
    /// `fuzz.yml` brackets it with a `cargo metadata --locked` preflight and a
    /// lockfile assertion instead of a flag it cannot pass.
    const RESOLVING_SUBCOMMANDS: [&str; 12] = [
        "build", "test", "check", "clippy", "bench", "doc", "package", "metadata", "tree", "rustc",
        "run", "install",
    ];

    /// Reads a workflow file relative to the repository root.
    fn read_workflow(workflow: &str) -> alloc::string::String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(root.join(workflow))
            .unwrap_or_else(|e| panic!("{workflow} must be readable: {e}"))
    }

    /// Every `cargo` subcommand invoked on one shell line, tolerating a
    /// `+channel` prefix (`cargo +stable test …`) and several commands chained on
    /// one line (`cargo build && cargo test`).
    ///
    /// Version and help queries — `cargo clippy --version`, `cargo fmt --help` —
    /// are excluded, because they resolve nothing and are used throughout these
    /// workflows to record the tool versions a job actually ran on. The test is
    /// deliberately narrow: only a query flag in the FIRST argument position
    /// disqualifies an invocation, so `cargo install <crate> --version <v>` is
    /// still held to the `--locked` requirement.
    fn cargo_subcommands(line: &str) -> alloc::vec::Vec<&str> {
        let mut out = alloc::vec::Vec::new();
        for (at, _) in line.match_indices("cargo ") {
            // `cargo` must sit in command position, not inside another word such
            // as `CARGO_HOME` or a path like `.cargo/config.toml`.
            if at > 0
                && !matches!(
                    line.as_bytes()[at - 1],
                    b' ' | b'\t' | b'|' | b'(' | b'&' | b';' | b'`'
                )
            {
                continue;
            }
            // A command name quoted inside `echo`/`printf` is log text, not an
            // invocation: these workflows narrate the command they are about to
            // run, and narration resolves nothing. The segment is delimited by the
            // nearest preceding shell separator so `echo x && cargo build` still
            // sees the real invocation.
            let head = &line[..at];
            let segment = head
                .rfind(['|', '&', ';', '(', '`'])
                .map_or(head, |cut| &head[cut + 1..]);
            if matches!(
                segment
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches('"'),
                "echo" | "printf"
            ) {
                continue;
            }
            let mut tokens = line[at + "cargo ".len()..].split_whitespace();
            let Some(first) = tokens.next() else { continue };
            let subcommand = if first.starts_with('+') {
                tokens.next()
            } else {
                Some(first)
            };
            let Some(subcommand) = subcommand else {
                continue;
            };
            if matches!(tokens.next(), Some("--version" | "-V" | "--help" | "-h")) {
                continue;
            }
            out.push(subcommand);
        }
        out
    }

    /// Names of every job declared in a workflow.
    ///
    /// Jobs are the only two-space-indented mapping keys under `jobs:` — the same
    /// rule [`workflow_job_block`] relies on. Comment lines are skipped, because a
    /// comment shaped `  # something:` would otherwise read as a job key.
    fn workflow_job_names(workflow: &str) -> alloc::vec::Vec<alloc::string::String> {
        let text = read_workflow(workflow);
        let mut names = alloc::vec::Vec::new();
        let mut in_jobs = false;
        for line in text.lines() {
            if line.starts_with("jobs:") {
                in_jobs = true;
                continue;
            }
            if !in_jobs || line.trim_start().starts_with('#') {
                continue;
            }
            if line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':')
            {
                names.push(line.trim().trim_end_matches(':').into());
            }
        }
        names
    }

    /// Every third-party action must be pinned to a full commit SHA, every
    /// checkout must drop its credentials, and every toolchain install must name
    /// its channel.
    ///
    /// A tag or branch ref (`@v4`, `@stable`) is MUTABLE: whoever can move it —
    /// the action's maintainers, or anyone who compromises that account — runs
    /// arbitrary code inside this repository's jobs, with whatever the job token
    /// grants. `ci.yml` runs on `pull_request`, so it already executes
    /// fork-controlled source; the actions themselves must not be a second
    /// unreviewed input. A SHA is content-addressed and cannot be moved.
    ///
    /// `actions/checkout` writes the job token into `.git/config` unless told not
    /// to. No job here performs an authenticated git operation, so persisting it
    /// only leaves a credential for a later step to find.
    ///
    /// The `toolchain:` requirement is a direct consequence of SHA-pinning
    /// `dtolnay/rust-toolchain`: that action derives its default channel from
    /// `github.action_ref`, so a pin to a commit — which is not a channel name —
    /// leaves the input unset, and the pinned revision declares it `required`.
    /// Every call site must therefore state its channel explicitly.
    #[test]
    fn every_workflow_action_is_pinned_to_an_immutable_commit_sha() {
        for workflow in SUPPLY_CHAIN_WORKFLOWS {
            let text = read_workflow(workflow);
            let lines: alloc::vec::Vec<&str> = text.lines().collect();
            let mut checkouts = 0usize;
            let mut toolchains = 0usize;
            let mut caches = 0usize;
            let mut uploads = 0usize;

            for (index, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    continue;
                }
                let Some(rest) = trimmed.strip_prefix("uses: ") else {
                    continue;
                };
                let number = index + 1;
                let spec = rest.trim();
                let (reference, annotation) = match spec.split_once(" #") {
                    Some((reference, annotation)) => (reference.trim(), annotation.trim()),
                    None => (spec, ""),
                };
                let (action, sha) = reference
                    .split_once('@')
                    .unwrap_or_else(|| panic!("{workflow}:{number}: `{spec}` names no ref"));
                assert!(
                    sha.len() == 40
                        && sha
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                    "{workflow}:{number}: `{action}` resolves through `{sha}`, which is not a \
                     full 40-character lowercase commit SHA. Tag and branch refs are mutable, \
                     so pinning to one delegates code execution in this workflow to whoever \
                     can move it."
                );
                assert!(
                    !annotation.is_empty(),
                    "{workflow}:{number}: the pin for `{action}` must carry a trailing \
                     `# <version>` comment, otherwise no reviewer can tell which release the \
                     SHA denotes or when it was last reviewed"
                );

                // The remainder of this step: `uses:` steps carry only `name`,
                // `if` and `with`, so the scan ends at the next step's `- `.
                let mut body = alloc::string::String::new();
                for follower in lines[index + 1..]
                    .iter()
                    .take_while(|line| !line.trim_start().starts_with("- "))
                {
                    body.push_str(follower);
                    body.push('\n');
                }

                if action.ends_with("actions/checkout") {
                    checkouts += 1;
                    assert!(
                        body.contains("persist-credentials: false"),
                        "{workflow}:{number}: this `actions/checkout` must set \
                         `persist-credentials: false`. The default writes the job token into \
                         `.git/config`, where every later step - including anything a \
                         dependency's build script runs - can read it."
                    );
                }
                if action.ends_with("actions/cache") {
                    caches += 1;
                }
                if action.ends_with("actions/upload-artifact") {
                    uploads += 1;
                }
                if action.contains("rust-toolchain") {
                    toolchains += 1;
                    assert!(
                        body.lines()
                            .any(|line| line.trim_start().starts_with("toolchain:")),
                        "{workflow}:{number}: a SHA-pinned `dtolnay/rust-toolchain` must pass \
                         an explicit `toolchain:` input. Pinned to a commit there is no \
                         `github.action_ref` channel to fall back on, and the input is \
                         declared `required`."
                    );
                }
            }

            // A file with no matches would satisfy every assertion above
            // vacuously, so the census is asserted exactly rather than assumed.
            let (_, expected_checkouts, expected_toolchains, expected_caches, expected_uploads) =
                ACTION_CENSUS
                    .into_iter()
                    .find(|&(file, ..)| file == workflow)
                    .unwrap_or_else(|| panic!("{workflow} must appear in ACTION_CENSUS"));
            assert_eq!(
                (checkouts, toolchains, caches, uploads),
                (
                    expected_checkouts,
                    expected_toolchains,
                    expected_caches,
                    expected_uploads
                ),
                "{workflow} declares (checkout, toolchain, cache, upload-artifact) = \
                 ({checkouts}, {toolchains}, {caches}, {uploads}), but the census records \
                 ({expected_checkouts}, {expected_toolchains}, {expected_caches}, \
                 {expected_uploads}). Update ACTION_CENSUS and the totals in audit.yml's \
                 ACTION PINNING section together - an action added without that review is \
                 an unreviewed code-execution site."
            );
        }
    }

    /// The census must cover every workflow file that exists, not merely every
    /// file someone remembered to list.
    ///
    /// Without this, the contract above is opt-in: a new workflow that pinned
    /// nothing and persisted its credentials would pass simply by being absent
    /// from [`SUPPLY_CHAIN_WORKFLOWS`].
    #[test]
    fn every_workflow_file_is_covered_by_the_supply_chain_contract() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found: alloc::vec::Vec<alloc::string::String> =
            std::fs::read_dir(root.join(".github/workflows"))
                .expect(".github/workflows must be readable")
                .map(|entry| entry.expect("directory entry must be readable").file_name())
                .map(|name| alloc::format!(".github/workflows/{}", name.to_string_lossy()))
                .filter(|name| name.ends_with(".yml") || name.ends_with(".yaml"))
                .collect();
        found.sort();
        let mut covered: alloc::vec::Vec<alloc::string::String> =
            SUPPLY_CHAIN_WORKFLOWS.iter().map(|&f| f.into()).collect();
        covered.sort();
        assert_eq!(
            found, covered,
            "every workflow under .github/workflows must be listed in \
             SUPPLY_CHAIN_WORKFLOWS (and in ACTION_CENSUS), or the SHA-pinning, \
             credential-dropping and `--locked` contracts silently do not apply to it"
        );
        let mut census: alloc::vec::Vec<alloc::string::String> = ACTION_CENSUS
            .iter()
            .map(|&(file, ..)| file.into())
            .collect();
        census.sort();
        assert_eq!(
            found, census,
            "ACTION_CENSUS must have one row per workflow file"
        );
    }

    /// Every tool this repository installs into CI is version-pinned, fetched
    /// against a verified checksum, built from an audited dependency graph, and
    /// asserted after installation.
    ///
    /// A CI-installed tool runs with full access to the checked-out tree and to
    /// whatever the job token grants, so an unpinned `cargo install <tool>`
    /// executes whatever the registry serves at that moment. Two further holes are
    /// less obvious and are both closed here. Fetching the crate by hand bypasses
    /// the checksum verification `cargo install` performs internally, so the
    /// download is checked against the sparse index explicitly. And `--locked`
    /// builds the tool's OWN published lockfile, which for two of these three
    /// tools was measured to contain advisory-affected dependencies - so the
    /// graph is refreshed with reviewed `--precise` overrides and then audited,
    /// rather than merely pinned.
    #[test]
    fn every_ci_installed_tool_is_pinned_verified_and_audited() {
        let mut installs_seen = 0usize;
        for workflow in SUPPLY_CHAIN_WORKFLOWS {
            let text = read_workflow(workflow);

            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') || !trimmed.contains("cargo") {
                    continue;
                }
                if !cargo_subcommands(trimmed).contains(&"install") {
                    continue;
                }
                installs_seen += 1;
                assert!(
                    trimmed.contains("--path") && trimmed.contains("--locked"),
                    "{workflow}:{}: a CI tool install must build from a local, \
                     checksum-verified, dependency-refreshed tree (`--path … --locked`). \
                     A registry install cannot be audited before it is built: {trimmed}",
                    index + 1
                );
            }

            for (tool, version) in CI_INSTALLED_TOOLS {
                // Only the files that actually install the tool are held to the
                // rest of the contract.
                if !text.contains(&alloc::format!("name={tool}")) {
                    continue;
                }
                for needle in [
                    alloc::format!("version={version}"),
                    alloc::format!("expected='{version}'"),
                ] {
                    assert!(
                        text.contains(&needle),
                        "{workflow} installs {tool} but does not contain `{needle}`: the \
                         version must be pinned AND asserted after installation, or a \
                         cached or pre-installed binary silently replaces it"
                    );
                }
                assert!(
                    text.contains("index.crates.io") && text.contains("sha256sum -c"),
                    "{workflow} installs {tool} from a hand-fetched tarball, so it must \
                     verify that download against the sparse index checksum - Cargo's own \
                     verification does not apply to a `curl` download"
                );
                assert!(
                    text.contains("check advisories") || text.contains("audit --deny warnings"),
                    "{workflow} installs {tool} but never audits a tool dependency graph. \
                     Two of the three pinned tools ship advisory-affected published \
                     lockfiles, so the graph must be audited, not merely pinned"
                );
            }
        }
        assert!(
            installs_seen >= CI_INSTALLED_TOOLS.len(),
            "expected at least {} CI tool installs across the workflows, found \
             {installs_seen} - has an install step been removed or renamed?",
            CI_INSTALLED_TOOLS.len()
        );
    }

    /// Every workflow command that resolves the dependency graph must pass
    /// `--locked`, and every job that runs one must prove the lockfile survived.
    ///
    /// Without `--locked` Cargo may re-resolve and REWRITE `Cargo.lock`, after
    /// which the job builds, tests or lints a dependency set no commit contains.
    /// That is not a theoretical concern for this crate: it emits `cdylib` and
    /// `staticlib` artifacts and commits its lockfile precisely so the shipped
    /// dependency closure is the reviewed one.
    ///
    /// The flag alone is not sufficient evidence, because a single command that
    /// loses it leaves no trace in the log. Hence the second half: any job that
    /// resolves must also assert `git status --porcelain Cargo.lock` is empty.
    #[test]
    fn every_dependency_resolving_workflow_command_is_locked() {
        for workflow in SUPPLY_CHAIN_WORKFLOWS {
            let text = read_workflow(workflow);
            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    continue;
                }
                for subcommand in cargo_subcommands(trimmed) {
                    if !RESOLVING_SUBCOMMANDS.contains(&subcommand) {
                        continue;
                    }
                    assert!(
                        trimmed.contains("--locked"),
                        "{workflow}:{}: `cargo {subcommand}` resolves the dependency graph \
                         and must pass `--locked`, or it may rewrite Cargo.lock and evaluate \
                         this gate against a resolution no commit contains: {trimmed}",
                        index + 1
                    );
                }
            }

            for job in workflow_job_names(workflow) {
                let block = workflow_job_block(workflow, &job);
                let resolves = block.lines().any(|line| {
                    let trimmed = line.trim_start();
                    !trimmed.starts_with('#')
                        && cargo_subcommands(trimmed)
                            .iter()
                            .any(|subcommand| RESOLVING_SUBCOMMANDS.contains(subcommand))
                });
                if resolves {
                    assert!(
                        block.contains("git status --porcelain Cargo.lock"),
                        "{workflow} job `{job}` resolves the dependency graph, so it must \
                         assert `Cargo.lock` is unchanged before it finishes. A rewritten \
                         lockfile is the one build-input change that leaves no other trace."
                    );
                }
            }
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
    /// `--locked` and `--all-features` included. The rustdoc half of the same
    /// contract lives in the dedicated `docs` job and is asserted there.
    ///
    /// Without `--all-features` clippy silently skips feature-gated units:
    /// `tests/c_oracle.rs` is not linted at all (it carries `required-features =
    /// ["c-oracle"]`), and neither are the `inflate_strict` arms. `lint` is the
    /// only job in the workflow that promotes warnings to errors, so a lint
    /// regression in either would have nowhere else to surface.
    ///
    /// What `--all-features` cannot cover is the freestanding `no_std_support`
    /// block: enabling every feature turns `std` on, and that block's predicate
    /// requires `not(feature = "std")`. The std-off library builds are what
    /// compile it — see the `no_std library (build only)` matrix row and the
    /// `bare-metal-no-std` job.
    ///
    /// Without `--locked` the gate is reproducible only by luck: an ordinary
    /// `cargo clippy` is free to re-resolve the graph and rewrite `Cargo.lock`,
    /// so the tree that gets linted need not be the tree the lockfile pins — and
    /// a lockfile the run itself mutated is exactly what
    /// [`Self::the_release_profile_declares_no_inert_lto_setting`]'s sibling
    /// gates and the `package-verify` job assume has not happened. Every
    /// graph-resolving cargo invocation in `ci.yml` therefore carries `--locked`;
    /// `cargo fmt` is the sole exception, because it resolves no dependencies.
    ///
    /// `.cargo/config.toml` documents this exact gate as well, and the assertion
    /// is made against it too: a spelling that drifts in either place is a
    /// documentation defect as well as a coverage gap.
    ///
    /// The rustdoc gate is asserted too, in the separate `docs` job that owns it:
    /// `#![warn(missing_docs)]` and every intra-doc link in the public API are
    /// enforced only when rustdoc actually runs with warnings promoted to errors,
    /// and `cargo doc` alone prints the same diagnostics and still exits 0 — so
    /// `RUSTDOCFLAGS: -D warnings` is what makes it gate at all.
    ///
    /// Finally, `-D warnings` is also what gives `[lints.rust.unexpected_cfgs]`
    /// in `Cargo.toml` its teeth, so that table's `level` and `check-cfg` are
    /// asserted here as well. The crate matches on one `target_os` value the
    /// MSRV compiler does not know (`cygwin`), and teaching the lint that single
    /// value is deliberately different from switching the lint off: the same
    /// lint is what rejected `target_arch = "hppa"` and `"alpha"` — neither of
    /// which exists — while the platform-constant cascades were being written.
    /// A future edit to `allow`, or to `values(any())`, would keep every gate
    /// green while making a misspelled predicate compile into an arm that can
    /// never be taken, so it must fail a test instead.
    #[test]
    fn the_lint_job_runs_the_documented_clippy_gate() {
        const CLIPPY_GATE: &str = "clippy --locked --all-targets --all-features -- -D warnings";
        let block = workflow_job_block(".github/workflows/ci.yml", "lint");
        assert!(
            block.contains(CLIPPY_GATE),
            "the ci.yml `lint` job must run `cargo {CLIPPY_GATE}`; without \
             `--all-features` the feature-gated surface (tests/c_oracle.rs and \
             the inflate_strict arms) is never linted anywhere; the std-off \
             rows are what cover the no_std runtime block, which this row \
             cannot reach because `--all-features` turns `std` on. Without \
             `--locked` the linted graph need not be the pinned one"
        );
        assert!(
            block.contains("fmt --all -- --check"),
            "the ci.yml `lint` job must also run `cargo fmt --all -- --check`"
        );

        // A resolving job must also PROVE it honoured the committed resolution: a
        // rewritten lockfile is the one build-input change that leaves no other
        // trace in the log.
        assert!(
            block.contains("git status --porcelain Cargo.lock"),
            "the ci.yml `lint` job must assert `Cargo.lock` is unchanged at job \
             end, so a command that lost its `--locked` flag cannot pass silently"
        );

        // The gate the workflow runs and the gate the repository documents must be
        // the same string, or one of them is lying.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo_config = std::fs::read_to_string(root.join(".cargo/config.toml"))
            .expect(".cargo/config.toml must be readable");
        assert!(
            cargo_config.contains(CLIPPY_GATE),
            ".cargo/config.toml must keep documenting the same clippy gate the \
             `lint` job runs, `--locked` included"
        );
        // The rustdoc gate lives in the dedicated `docs` job (which also builds the
        // MkDocs site in strict mode), so it is asserted against that job's block.
        // `RUSTDOCFLAGS: -D warnings` is what makes it blocking; `cargo doc` alone
        // would print the same diagnostics and still exit 0.
        let docs = workflow_job_block(".github/workflows/ci.yml", "docs");
        assert!(
            docs.contains("doc --locked --no-deps"),
            "the ci.yml `docs` job must run `cargo doc --locked --no-deps`, or \
             `#![warn(missing_docs)]` and every intra-doc link in the public API \
             are enforced nowhere in CI"
        );
        assert!(
            docs.contains("RUSTDOCFLAGS: -D warnings"),
            "the ci.yml `docs` job's rustdoc step must set \
             `RUSTDOCFLAGS: -D warnings`; without it `cargo doc` prints its \
             warnings and still exits 0, so the gate would not gate"
        );

        // `[lints.rust.unexpected_cfgs]` is only a GATE because of the `-D
        // warnings` asserted above: at `level = "warn"` a bogus `cfg` value is a
        // warning locally and a hard error under this job. Downgrading it to
        // `allow` — or widening `check-cfg` into a blanket permit — would keep
        // every gate green while silently restoring the class of defect this
        // lint exists to catch, so both halves are asserted here rather than
        // trusted to review.
        let lints = manifest_table("lints.rust.unexpected_cfgs");
        assert!(
            lints.contains("level = \"warn\""),
            "`[lints.rust.unexpected_cfgs]` must stay at `level = \"warn\"`, \
             which the `-D warnings` clippy gate promotes to an error. At \
             `allow` a misspelled `target_os`/`target_arch` value compiles \
             silently and its arm is simply never taken - the exact failure \
             mode that caught `target_arch = \"hppa\"` and `\"alpha\"`, two \
             values that do not exist, during the platform-constant work. \
             Found instead: {lints:?}"
        );
        // The allowance must stay an ENUMERATION of real values. `check-cfg`
        // accepts `cfg(target_os, values(any()))`, which would permit every
        // spelling including the misspellings, so the enumerated form is the
        // whole point of keeping the lint on.
        assert!(
            lints.contains("values(\"cygwin\")"),
            "`check-cfg` must keep enumerating exactly the extra `target_os` \
             values this crate matches on but the MSRV rustc does not know - \
             today only `cygwin`, which `src/gz/open.rs` and `src/ffi/gz.rs` \
             place in the newlib `O_NONBLOCK` and POSIX `FD_CLOEXEC`/fcntl \
             families. Found instead: {lints:?}"
        );
        assert!(
            !lints.contains("any()"),
            "`check-cfg` must not use `values(any())`: that permits every \
             spelling, including the misspellings the lint exists to reject, \
             which is indistinguishable from switching the lint off. \
             Found: {lints:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Conditional-ignore scanning (AAP directive D-5)
    //
    // `ci.yml` asserts that no test in this repository is disabled. The plain
    // `#[ignore]` half of that gate is a single anchored grep and is sound. The
    // `#[cfg_attr(<pred>, ignore)]` half cannot be, and the regex that used to
    // stand in for it — `#\[[[:space:]]*cfg_attr\([^)]*[^a-z_]ignore` — missed
    // both of the forms most likely to be written by accident:
    //
    //   * a NESTED predicate. `[^)]*` cannot cross a `)`, so in
    //     `cfg_attr(all(unix, target_pointer_width = "64"), ignore)` the
    //     character class stops at the `)` that closes `all(..)` and never
    //     reaches the `ignore`.
    //   * a MULTILINE attribute. `grep` matches within one line, and rustfmt
    //     wraps a long `cfg_attr` across several.
    //
    // Either form disables a test under one configuration while the summary-line
    // `ignored=0` check stays green, because a test that is not compiled is not
    // reported as ignored. So the gate has to understand Rust's nesting, which a
    // line-oriented regex cannot, and that is what the scanner below does:
    // comments and literals are blanked first (so the many prose mentions of
    // `#[ignore]` in this repository are invisible to it), then each attribute is
    // read as a BALANCED bracket group that may span any number of lines.
    // -----------------------------------------------------------------------

    /// The trees the conditional-ignore scan covers.
    ///
    /// Kept identical to the `roots` list in `ci.yml`'s "Assert no test is
    /// disabled with #[ignore]" step, and asserted so by
    /// `the_ignore_scan_covers_the_same_roots_as_ci` below: a root added to one
    /// and not the other is a silently unscanned tree. That name is not an
    /// intra-doc link because a `#[cfg(test)]` item is not in the documented
    /// graph, so linking it would be an unresolved reference under
    /// `--document-private-items`.
    const IGNORE_SCAN_ROOTS: [&str; 5] =
        ["src", "tests", "benches", "build.rs", "fuzz/fuzz_targets"];

    /// The roots that `[Cargo.toml] exclude` keeps out of the published archive.
    ///
    /// The fuzz tree is a DETACHED workspace and is deliberately excluded, so it is
    /// present in a git checkout and absent from a packaged crate. The scan must
    /// therefore treat its absence as expected when — and only when — it is running
    /// inside an unpacked archive, without weakening the staleness check that makes
    /// a genuinely wrong root name fail loudly in the repository.
    const IGNORE_SCAN_ROOTS_ABSENT_WHEN_PACKAGED: [&str; 1] = ["fuzz/fuzz_targets"];

    /// True when the crate is being tested from an unpacked `.crate` archive rather
    /// than from a git checkout.
    ///
    /// `Cargo.toml.orig` is written by `cargo package` and exists nowhere else, which
    /// makes it an exact witness: it is not a heuristic about which files happen to
    /// be missing, so it cannot mask a real omission.
    fn running_from_packaged_crate(root: &std::path::Path) -> bool {
        root.join("Cargo.toml.orig").is_file()
    }

    /// True when `bytes[at..]` begins with `word` as a standalone identifier —
    /// neither preceded nor followed by an identifier byte.
    ///
    /// This is what keeps `ignore_without_reason`, `should_ignore` and
    /// `crate::ignored` from being mistaken for the `ignore` attribute.
    fn is_word_at(bytes: &[u8], at: usize, word: &[u8]) -> bool {
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        if !bytes[at..].starts_with(word) {
            return false;
        }
        if at > 0 && ident(bytes[at - 1]) {
            return false;
        }
        !bytes.get(at + word.len()).copied().is_some_and(ident)
    }

    /// Index just past the bracket/paren/brace group that opens at `open`, or
    /// `None` if it is never closed.
    ///
    /// Counts all three bracket kinds so a `cfg_attr(all(..), ignore)` predicate
    /// and an attribute body containing a block both close at the right place.
    /// Newlines are ordinary bytes here, which is precisely how a multiline
    /// attribute becomes visible to this scan and invisible to a `grep`.
    fn balanced_end(bytes: &[u8], open: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut i = open;
        while i < bytes.len() {
            match bytes[i] {
                b'[' | b'(' | b'{' => depth += 1,
                b']' | b')' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Every `#[ignore]` and `#[cfg_attr(.., ignore)]` attribute in `text`, as
    /// `(1-based line, form)` where `form` is `"ignore"` or `"cfg_attr"`.
    ///
    /// Operates on the comment- and literal-blanked text, so a mention inside a
    /// doc comment or a string is not a hit — which matters because this
    /// repository documents "no ignored tests" as a property in several files and
    /// carries this scanner's own fixtures in `src/lib.rs`.
    fn conditional_ignore_hits(text: &str) -> alloc::vec::Vec<(usize, &'static str)> {
        let blanked = blank_comments_and_literals(text);
        let b = blanked.as_bytes();
        let mut hits = alloc::vec::Vec::new();
        let mut i = 0usize;
        while i < b.len() {
            if b[i] != b'#' {
                i += 1;
                continue;
            }
            // `#[..]` or the inner-attribute form `#![..]`.
            let mut j = i + 1;
            if b.get(j) == Some(&b'!') {
                j += 1;
            }
            if b.get(j) != Some(&b'[') {
                i += 1;
                continue;
            }
            let Some(end) = balanced_end(b, j) else {
                break;
            };
            // The attribute body, without its brackets.
            let body_start = j + 1;
            let body = &b[body_start..end - 1];
            let first = body
                .iter()
                .position(|c| !c.is_ascii_whitespace())
                .unwrap_or(body.len());
            let line = 1 + blanked[..i].bytes().filter(|c| *c == b'\n').count();
            if is_word_at(body, first, b"ignore") {
                // `#[ignore]`, `#[ignore = "reason"]`.
                hits.push((line, "ignore"));
            } else if is_word_at(body, first, b"cfg_attr") {
                // `#[cfg_attr(<predicate>, <attrs..>)]`. Search the whole paren
                // group: a standalone `ignore` token anywhere inside it is an
                // ignore applied under some configuration, and the predicate
                // cannot contain that token itself — a `cfg` predicate is built
                // from `all`/`any`/`not`/`feature`/`target_*` keys and string
                // values, and the values were blanked before this scan.
                if let Some(paren) = body[first..].iter().position(|c| *c == b'(') {
                    let open = first + paren;
                    if let Some(close) = balanced_end(body, open) {
                        let group = &body[open..close];
                        if (0..group.len()).any(|k| is_word_at(group, k, b"ignore")) {
                            hits.push((line, "cfg_attr"));
                        }
                    }
                }
            }
            i = end;
        }
        hits
    }

    /// The scanner must catch all three shapes of disabled test, including the two
    /// the previous `grep` could not see.
    ///
    /// Every fixture is assembled with [`concat!`] rather than written as a single
    /// literal. That is not stylistic: `ci.yml` greps this very tree for a line
    /// whose first non-whitespace is `#[ignore`, so a fixture spelled out in full
    /// would match the repository's own gate and turn it permanently red. Splitting
    /// the token means the matching text exists only at run time.
    #[test]
    fn the_conditional_ignore_scanner_catches_simple_nested_and_multiline_forms() {
        let plain = concat!("#[", "ignore]\nfn t() {}\n");
        let plain_reason = concat!("#[", "ignore = \"flaky\"]\nfn t() {}\n");
        let spaced = concat!("#[ ", "ignore ]\nfn t() {}\n");
        let simple_cfg = concat!("#[cfg_", "attr(unix, ignore)]\nfn t() {}\n");
        // The nested predicate: the old regex's `[^)]*` stopped at the `)` that
        // closes `all(..)` and never reached the `ignore`.
        let nested = concat!(
            "#[cfg_",
            "attr(all(unix, target_pointer_width = \"64\"), ignore)]\nfn t() {}\n"
        );
        let doubly_nested = concat!(
            "#[cfg_",
            "attr(any(all(unix, not(target_os = \"macos\")), windows), ignore)]\nfn t() {}\n"
        );
        // The multiline form: rustfmt wraps a long predicate, and `grep` matches
        // within a single line.
        let multiline = concat!(
            "#[cfg_",
            "attr(\n    all(unix, target_pointer_width = \"64\"),\n    ignore\n)]\nfn t() {}\n"
        );
        for (label, src) in [
            ("plain", plain),
            ("plain with reason", plain_reason),
            ("spaced", spaced),
            ("simple cfg_attr", simple_cfg),
            ("nested cfg_attr", nested),
            ("doubly nested cfg_attr", doubly_nested),
            ("multiline cfg_attr", multiline),
        ] {
            let hits = conditional_ignore_hits(src);
            assert_eq!(
                hits.len(),
                1,
                "the {label} form must be caught exactly once, got {hits:?} for:\n{src}"
            );
        }

        // And it must NOT fire on the things this repository legitimately
        // contains, or the gate would be unusable.
        let benign = [
            (
                "prose in a doc comment",
                "/// This suite has no #[ignore] tests.\nfn t() {}\n",
            ),
            (
                "prose in a line comment",
                "// never add #[ignore] here\nfn t() {}\n",
            ),
            (
                "prose in a block comment",
                "/* a #[cfg_attr(unix, ignore)] would be a defect */\nfn t() {}\n",
            ),
            (
                "a string literal",
                "let pattern = \"#[cfg_attr(unix, ignore)]\";\n",
            ),
            (
                "a longer identifier",
                "#[allow(clippy::ignored_unit_patterns)]\nfn t() {}\n",
            ),
            (
                "an unrelated cfg_attr",
                "#[cfg_attr(docsrs, doc(cfg(feature = \"gzip\")))]\nfn t() {}\n",
            ),
            (
                "an ordinary cfg",
                "#[cfg(all(unix, feature = \"gz-io\"))]\nfn t() {}\n",
            ),
            (
                "a feature literally named ignore",
                "#[cfg_attr(feature = \"ignore\", doc = \"x\")]\nfn t() {}\n",
            ),
        ];
        for (label, src) in benign {
            let hits = conditional_ignore_hits(src);
            assert!(
                hits.is_empty(),
                "the scanner must not fire on {label}, got {hits:?} for:\n{src}"
            );
        }

        // The line number is reported from the attribute, so a failure names the
        // right place even for a multiline form.
        let offset = concat!("fn a() {}\nfn b() {}\n#[", "ignore]\nfn c() {}\n");
        assert_eq!(conditional_ignore_hits(offset), alloc::vec![(3, "ignore")]);
    }

    /// No test anywhere in the repository is disabled, by either form.
    ///
    /// This is the in-crate half of `ci.yml`'s D-5 gate, and it is the half that
    /// understands Rust: the workflow keeps the cheap anchored grep for plain
    /// `#[ignore]` (no compilation, one pass) and defers the `cfg_attr` form to
    /// this test, which it runs by name so the coverage is legible in the log.
    #[test]
    fn no_test_in_the_repository_is_disabled_with_ignore() {
        fn walk(dir: &std::path::Path, out: &mut alloc::vec::Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            let mut paths: alloc::vec::Vec<_> =
                entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = alloc::vec::Vec::new();
        for entry in IGNORE_SCAN_ROOTS {
            let path = root.join(entry);
            if path.is_dir() {
                walk(&path, &mut files);
            } else if path.is_file() {
                files.push(path);
            } else if IGNORE_SCAN_ROOTS_ABSENT_WHEN_PACKAGED.contains(&entry)
                && running_from_packaged_crate(root)
            {
                // Excluded from the published archive by design; nothing to scan.
                // Still asserted in a checkout, where the tree does exist.
            } else {
                panic!("ignore-scan root {entry} does not exist; IGNORE_SCAN_ROOTS is stale");
            }
        }
        // A scan that found nothing to scan proves nothing.
        assert!(
            files.len() > 40,
            "expected the whole source tree, found only {} files",
            files.len()
        );

        let mut offenders = alloc::vec::Vec::new();
        for path in &files {
            let text = std::fs::read_to_string(path).expect("source file is UTF-8");
            for (line, form) in conditional_ignore_hits(&text) {
                let rel = path.strip_prefix(root).unwrap_or(path);
                offenders.push(std::format!("{}:{line} ({form})", rel.display()));
            }
        }
        assert!(
            offenders.is_empty(),
            "AAP directive D-5 requires zero ignored tests, but {} attribute(s) \
             disable one: {:?}",
            offenders.len(),
            offenders
        );
    }

    /// The scan roots in this crate and in `ci.yml` must be the same set.
    ///
    /// The workflow's grep and this crate's scanner are two halves of one gate; a
    /// root added to only one of them is a tree that looks covered and is not.
    #[test]
    fn the_ignore_scan_covers_the_same_roots_as_ci() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
            .expect("ci.yml must be readable");
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("roots="))
            .expect("ci.yml must declare the ignore-scan roots as `roots=...`");
        let declared = line
            .trim_start_matches("roots=")
            .trim_matches('\'')
            .split_whitespace()
            .collect::<alloc::vec::Vec<_>>();
        assert_eq!(
            declared,
            IGNORE_SCAN_ROOTS.as_slice(),
            "ci.yml's ignore-scan roots and IGNORE_SCAN_ROOTS must match exactly"
        );
    }

    /// Join the shell string concatenations that `ci.yml` wraps across lines.
    ///
    /// A step writes `echo "... below the floor"\` then `" of $VAR - ..."`, which the
    /// shell concatenates into one message. Collapsing `"\` + newline + indent + `"`
    /// reconstructs the text the operator actually sees, so an assertion about wording
    /// is not accidentally an assertion about where rustfmt-style wrapping fell.
    fn join_shell_continuations(block: &str) -> alloc::string::String {
        let mut out = alloc::string::String::with_capacity(block.len());
        let mut rest = block;
        while let Some(at) = rest.find("\"\\\n") {
            out.push_str(&rest[..at]);
            // Skip the closing quote, the backslash, the newline, the indent and the
            // reopening quote of the next fragment.
            let after = &rest[at + 3..];
            let trimmed = after.trim_start_matches([' ', '\t']);
            rest = trimmed.strip_prefix('"').unwrap_or(trimmed);
        }
        out.push_str(rest);
        out
    }

    /// Every CI test-count floor is declared once and used consistently.
    ///
    /// This is the mechanical form of the rule that a floor's DECLARATION, the value
    /// it LOGS, and the value it ENFORCES must be the same number. Spelled as three
    /// separate literals they can disagree, and the disagreement is silent in the
    /// worst possible direction: the log reads reassuring while the comparison
    /// accepts fewer tests than it advertises, which is worse than carrying no floor
    /// at all.
    ///
    /// Each floor therefore routes through one variable — `matrix.min_tests` for
    /// the `build-test` rows, and job-scoped `STD_OFF_FLOOR` / `PACKAGED_FLOOR` for
    /// the other two jobs — so the three uses cannot disagree. This test asserts that
    /// property directly: it fails if any count-enforcing comparison comes back as a
    /// bare numeric literal, which is the only way the drift can recur.
    #[test]
    fn the_ci_test_floors_are_internally_consistent() {
        let text = read_workflow(".github/workflows/ci.yml");

        // 1. Every floor comparison must read a variable, never a literal. The shape
        //    is `[ "$2" -ge <operand> ]`, the assertion being over <operand>.
        let mut literal_floors = alloc::vec::Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let Some(rest) = line.split_once("-ge ").map(|(_, r)| r) else {
                continue;
            };
            if !line.contains("\"$2\"") {
                continue;
            }
            let operand: alloc::string::String = rest
                .trim_start()
                .trim_start_matches('"')
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != ']')
                .collect();
            if operand.chars().all(|c| c.is_ascii_digit()) {
                literal_floors.push(std::format!("ci.yml:{}: -ge {operand}", lineno + 1));
            }
        }
        assert!(
            literal_floors.is_empty(),
            "a test-count floor is enforced against a bare literal instead of the \
             variable that is also logged, which is exactly how the log and the gate \
             drifted apart before: {literal_floors:?}"
        );

        // 2. Each job that declares a floor variable must USE it in the logged
        //    summary, the comparison and the failure message of every counting step.
        for (job, var) in [
            ("no-std-tests", "STD_OFF_FLOOR"),
            ("package-verify", "PACKAGED_FLOOR"),
        ] {
            let block = workflow_job_block(".github/workflows/ci.yml", job);
            let declared = block
                .lines()
                .find_map(|l| l.trim().strip_prefix(&std::format!("{var}:")))
                .map(|v| v.trim().to_string())
                .unwrap_or_else(|| std::panic!("job `{job}` must declare {var} at job scope"));
            assert!(
                declared.chars().all(|c| c.is_ascii_digit()) && !declared.is_empty(),
                "job `{job}`: {var} must be a plain integer, got {declared:?}"
            );
            let logs = block.matches(&std::format!("floor=${var}")).count();
            let enforces = block.matches(&std::format!("-ge \"${var}\"")).count();
            assert!(
                logs > 0 && logs == enforces,
                "job `{job}`: {var} is logged {logs} time(s) but enforced {enforces} \
                 time(s); every counting step must do both so the log cannot promise \
                 a floor the gate does not apply"
            );
            // The failure message must quote the same variable, or a red row would
            // name a number nobody can trace back to the declaration. The block is
            // normalised first: these messages are shell string concatenations split
            // across continuation lines (`... floor"\` / `" of $VAR ...`), so the
            // phrase is only contiguous once the continuations are joined. Matching
            // the raw text would assert about line wrapping rather than about wording.
            let joined = join_shell_continuations(&block);
            let in_message = joined.matches(&std::format!("floor of ${var}")).count();
            assert_eq!(
                in_message, enforces,
                "job `{job}`: {var} appears in {in_message} failure message(s) but is \
                 enforced {enforces} time(s); the message must cite the same source"
            );
        }

        // 3. The `build-test` rows: every row declares a floor, and the counting steps
        //    reference `matrix.min_tests` rather than any literal.
        let block = workflow_job_block(".github/workflows/ci.yml", "build-test");
        let declared: alloc::vec::Vec<u32> = block
            .lines()
            .filter_map(|l| l.trim().strip_prefix("min_tests:"))
            .map(|v| v.trim().parse().expect("min_tests must be an integer"))
            .collect();
        assert_eq!(
            declared.len(),
            7,
            "expected one min_tests per build-test row, got {declared:?}"
        );
        // Exactly one row runs no test step and therefore carries a zero floor.
        assert_eq!(
            declared.iter().filter(|v| **v == 0).count(),
            1,
            "exactly one build-test row (the no_std library row) may carry a zero \
             floor; got {declared:?}"
        );
        assert!(
            declared.iter().filter(|v| **v > 0).all(|v| *v >= 600),
            "a non-zero build-test floor is implausibly low, which would make the \
             row's count assertion vacuous: {declared:?}"
        );

        // 4. Ordering that must hold by construction: the all-features row exercises a
        //    superset of the default row, and every std-off floor is below even the
        //    LOWEST row floor, because `gz-io` being off removes a whole integration
        //    target plus several cfg-gated unit tests.
        let lowest_row_floor = *declared.iter().filter(|v| **v > 0).min().expect("a floor");
        let max_floor = *declared.iter().max().expect("a floor");
        assert!(
            max_floor >= lowest_row_floor,
            "the all-features floor must be at least the lowest row floor"
        );
        let std_off: u32 = workflow_job_block(".github/workflows/ci.yml", "no-std-tests")
            .lines()
            .find_map(|l| l.trim().strip_prefix("STD_OFF_FLOOR:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("STD_OFF_FLOOR");
        assert!(
            std_off < lowest_row_floor,
            "the std-off floor ({std_off}) must be below the lowest non-zero \
             build-test row floor ({lowest_row_floor}): turning `gz-io` off removes \
             the tests/gzip_compat.rs target and several cfg-gated unit tests, so an \
             equal or higher value would mean the std-off rows are not actually \
             running with std off"
        );
    }
    /// Every cargo invocation in `ci.yml` that resolves the dependency graph must
    /// pass `--locked`.
    ///
    /// `--locked` is what makes a CI result attributable to the committed
    /// `Cargo.lock`. Without it cargo may re-resolve and rewrite the lockfile
    /// mid-run, so a green build can be green against a graph nobody reviewed and
    /// nobody can reproduce — and a later step that reads `Cargo.lock` (the
    /// failure-only upload, `package-verify`, the audit workflow's policy jobs)
    /// then reads a file this run invented.
    ///
    /// `cargo fmt` is deliberately exempt: it parses source text and touches no
    /// dependency graph, so `--locked` would be noise. So is any invocation
    /// carrying `--version`, which is a toolchain probe (`cargo --version`,
    /// `cargo fmt --version`, `cargo clippy --version`) rather than a build — note
    /// that the probe has to be recognised by its *argument*, because
    /// `cargo clippy --version` presents `clippy` as its subcommand. Everything
    /// else — `build`, `test`, `check`, `clippy`, `bench`, `doc`, `package` — is
    /// required to carry it.
    ///
    /// The scan resolves each line into shell command segments before looking for
    /// `cargo`, because the word also appears inside quoted prose such as
    /// `echo "--- cargo package --list (…) ---"`. Splitting on the separators after
    /// which a new command begins, and then requiring `cargo` to be the segment's
    /// *command word*, keeps a mention from being mistaken for an invocation while
    /// still catching a real invocation nested in a command substitution.
    #[test]
    fn every_graph_resolving_ci_command_is_locked() {
        const EXEMPT_SUBCOMMANDS: [&str; 1] = ["fmt"];
        const RESOLVING: [&str; 7] = [
            "build", "test", "check", "clippy", "bench", "doc", "package",
        ];
        const SEPARATORS: [&str; 6] = ["&&", "||", "|", ";", "$(", ")"];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
            .expect("ci.yml must be readable");

        // Environment assignments (`RUSTUP_TOOLCHAIN=stable cargo …`) and shell
        // noise stand in front of the command word without being the command.
        let is_command_prefix = |word: &&str| {
            matches!(*word, "!" | "sudo" | "env" | "time" | "then" | "do")
                || word.split_once('=').is_some_and(|(name, _)| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                })
        };

        let mut checked = 0usize;
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            // Comments describe commands; they are prose, not invocations.
            if trimmed.starts_with('#') {
                continue;
            }
            let mut segments = std::string::String::from(trimmed);
            for separator in SEPARATORS {
                segments = segments.replace(separator, "\n");
            }
            for segment in segments.split('\n') {
                let segment = segment.trim();
                let segment = segment.strip_prefix("run:").unwrap_or(segment).trim();
                let mut words = segment
                    .split_whitespace()
                    .map(|word| word.trim_matches(['"', '\'']))
                    .skip_while(is_command_prefix);
                if words.next() != Some("cargo") {
                    continue;
                }
                // A `--version` argument makes the invocation a toolchain probe: it
                // prints a version string and exits without reading `Cargo.toml`,
                // so there is no dependency graph to lock.
                if segment.split_whitespace().any(|word| word == "--version") {
                    continue;
                }
                // The subcommand is the first argument that is not a `+toolchain`
                // shorthand.
                let Some(first) = words.next() else { continue };
                let subcommand = if first.starts_with('+') {
                    match words.next() {
                        Some(next) => next,
                        None => continue,
                    }
                } else {
                    first
                };
                if EXEMPT_SUBCOMMANDS.contains(&subcommand) || !RESOLVING.contains(&subcommand) {
                    continue;
                }
                checked += 1;
                assert!(
                    segment.contains("--locked"),
                    "ci.yml:{} runs `cargo {subcommand}` without `--locked`: {trimmed}",
                    index + 1
                );
            }
        }
        assert!(
            checked >= 20,
            "expected at least 20 graph-resolving ci.yml commands to be inspected \
             (24 exist today), saw {checked} — the scan stopped matching, so it is \
             no longer a gate"
        );
    }

    /// A `gzdopen` descriptor must never be wrapped in a [`std::fs::File`] unless
    /// it has been **proven** open, and a Windows CRT descriptor must never be
    /// wrapped at all.
    ///
    /// # Why this is a source-text guard
    ///
    /// The platform matrix reaches Windows; it does not reach this property.
    /// `ci.yml`'s `build-test` matrix carries native `windows-latest` and
    /// `macos-latest` rows alongside its five `ubuntu-latest` ones, and
    /// `cross-targets` type-checks `x86_64-pc-windows-msvc`, so a
    /// `#[cfg(windows)]` body is compiled by CI and, on the native row, executed.
    /// What no execution can observe is double ownership itself. A second owner of
    /// a lent CRT handle misbehaves only at the second close, which either reports
    /// `EBADF` into a value nobody inspects or — once the descriptor table has
    /// recycled the slot — closes an unrelated file belonging to another part of
    /// the process. Neither outcome fails an assertion, so a passing suite is not
    /// evidence either way.
    ///
    /// This guard is therefore supplementary rather than a substitute: the native
    /// `windows-latest` row runs the suite against a compiled `#[cfg(windows)]`
    /// body, and pinning the *shape* in text covers the one property that run
    /// cannot fail on. Unlike either, the text check also holds where the arm is
    /// never compiled at all — every Linux job but `cross-targets`, and any Linux
    /// developer checkout.
    ///
    /// What is pinned:
    ///
    /// * `_get_osfhandle` appears nowhere. It only **lends** the `HANDLE` the CRT
    ///   descriptor-table slot owns, so building an owning `File` from its result
    ///   creates a second owner and a later double close (CWE-664, CWE-672).
    /// * `from_raw_handle` appears nowhere, for the same reason — and because
    ///   `OwnedHandle` excludes `INVALID_HANDLE_VALUE`, so the `-1` an unopened CRT
    ///   descriptor reports is an immediately invalid value.
    /// * The Windows owner does its I/O through the CRT family reference zlib
    ///   compiles to: `_read`, `_write`, `_lseeki64`, `_close`.
    /// * Exactly one non-test `from_raw_fd` call site survives, it lives in
    ///   `adopt_descriptor`, and `descriptor_is_open` is consulted *before* it.
    ///   `File::from_raw_fd` requires an open descriptor and `gzdopen` must accept
    ///   one that is not (`zlib.h` L1422-L1426), so the proof has to come first.
    #[test]
    fn a_gzdopen_descriptor_is_never_wrapped_without_proof() {
        let mut saw = false;

        for (rel, text) in crate_sources() {
            if rel != "src/ffi/gz.rs" {
                continue;
            }
            saw = true;
            let blanked = blank_comments_and_literals(&text);
            // The boundary is the test *module*, not the first `#[cfg(test)]`: this
            // file gates a close-failure seam under `#[cfg(test)]` several hundred
            // lines before `adopt_descriptor`, so cutting at that attribute would
            // silently exclude the very code this guard exists to inspect — and the
            // guard would pass by finding nothing.
            let non_test = &blanked[..blanked.find("\nmod tests {").unwrap_or(blanked.len())];
            assert!(
                non_test.len() > 100_000,
                "the pre-test region of src/ffi/gz.rs looks truncated ({} bytes), so \
                 this guard would be vacuous",
                non_test.len()
            );
            assert!(
                non_test.contains("unsafe fn adopt_descriptor("),
                "the inspected region must contain `adopt_descriptor`, or every \
                 assertion below is vacuous"
            );

            // The borrowed-handle trap, in both of its spellings.
            for banned in ["_get_osfhandle", "from_raw_handle", "FromRawHandle"] {
                assert!(
                    !blanked.contains(banned),
                    "src/ffi/gz.rs must not name `{banned}`: the Windows CRT only \
                     LENDS the HANDLE behind an int descriptor, so wrapping it in an \
                     owning File double-owns it and the second close is a double \
                     close"
                );
            }

            // The Windows owner must reach the CRT directly, exactly as C does.
            // `#[link_name = "..."]` string literals survive comment blanking only
            // as their quotes, so the raw text is searched for these.
            for crt in ["\"_read\"", "\"_write\"", "\"_lseeki64\"", "\"_close\""] {
                assert!(
                    text.contains(crt),
                    "src/ffi/gz.rs must declare the CRT entry point {crt}: owning the \
                     CRT descriptor is what removes the second owner, and these are \
                     the calls reference zlib compiles to on Windows (`gzlib.c` L11 \
                     defines LSEEK as _lseeki64)"
                );
            }

            // Exactly one non-test `from_raw_fd` call, inside `adopt_descriptor`,
            // after the proof.
            let calls: alloc::vec::Vec<usize> = non_test
                .match_indices("from_raw_fd(")
                .map(|(at, _)| at)
                .collect();
            assert_eq!(
                calls.len(),
                1,
                "expected exactly one non-test `from_raw_fd` call site in \
                 src/ffi/gz.rs (the proven-open arm of `adopt_descriptor`), found {}",
                calls.len()
            );
            let call = calls[0];

            let owner_fn = non_test[..call]
                .rfind("unsafe fn adopt_descriptor(")
                .expect(
                    "the only non-test `from_raw_fd` call must live in \
                     `adopt_descriptor`, which is the one place that owns the \
                     caller's descriptor",
                );
            let proof = non_test[owner_fn..call].find("descriptor_is_open(").expect(
                "`adopt_descriptor` must consult `descriptor_is_open` BEFORE \
                     `from_raw_fd`: the latter requires an open descriptor, and \
                     `gzdopen` is required to accept one that is not (zlib.h \
                     L1422-L1426)",
            );
            assert!(
                proof < call - owner_fn,
                "the descriptor proof must precede the `from_raw_fd` that relies on it"
            );

            // And the raw owners must not smuggle a `File` in by another route.
            for owner in [
                "impl crate::gz::RawFileIo for DeadDescriptor",
                "impl crate::gz::RawFileIo for CrtDescriptor",
            ] {
                let at = non_test
                    .find(owner)
                    .unwrap_or_else(|| panic!("src/ffi/gz.rs must define `{owner}`"));
                let body = &non_test[at..];
                let end = body.find("\nimpl ").unwrap_or(body.len());
                assert!(
                    !body[..end].contains("std::fs::File"),
                    "`{owner}` must do its I/O on the raw descriptor, never by \
                     constructing a File from it"
                );
            }
        }

        assert!(saw, "src/ffi/gz.rs must be present in the source walk");
    }

    /// `OS_CODE` — the gzip-header operating-system byte — is declared in exactly
    /// one module, and the gzip emission path reads that one declaration.
    ///
    /// This has to be a *source-level* guard because no value assertion can catch
    /// the defect it protects against. `OS_CODE` is `3` on Linux, so a module that
    /// re-declares its own `const OS_CODE: u8 = 3` still agrees with the canonical
    /// constant wherever the comparison itself runs, while silently emitting `3`
    /// where reference zlib emits `10` on Windows (`zutil.h` L156-L158) or `19` on
    /// Apple (L168-L170). A value check performed on one host cannot detect that
    /// divergence, because that host does not build the target the divergence
    /// appears on. Counting declarations does, from any host, whichever platforms
    /// CI happens to run.
    ///
    /// The same test carries a second, related contract: the platform
    /// descriptor-flag and `fcntl` command cascades must degrade to "do nothing" on
    /// an unrecognised platform, never to a value borrowed from a neighbouring one.
    ///
    /// # Why this is a source-text test
    ///
    /// The property at stake lives in the arms this host cannot compile. The
    /// terminal arm of each cascade is, by definition, selected only on a platform
    /// nobody enumerated — so no host can evaluate it, no `cargo check --target`
    /// can reach it, and a unit test can only observe the arm its own target
    /// picked. Reading the cascade's own text is the only way to prove, from here,
    /// that the fallback is `None` rather than a guess.
    ///
    /// It also pins the two orderings that make the tables safe: the platforms
    /// that renumber `fcntl`'s commands must be matched *before* the broad POSIX
    /// arm (otherwise they would inherit POSIX numbers), and no `fcntl` call may
    /// name its command with a literal (otherwise the table is bypassed).
    #[test]
    fn platform_flag_cascades_never_guess() {
        let mut saw_open = false;
        let mut saw_ffi = false;

        for (rel, text) in crate_sources() {
            let blanked = blank_comments_and_literals(&text);

            if rel == "src/gz/open.rs" {
                saw_open = true;

                // The replaced defect: a two-branch cascade whose `else` answered
                // `0o0004` for every non-Linux unix. That value must not reappear
                // in the module's own code.
                //
                // Scoped to the pre-`#[cfg(test)]` region on purpose: the test
                // module legitimately names the old value in order to assert that
                // no enumerated platform is answered with it, and forbidding it
                // there would forbid the very regression test that proves it gone.
                let non_test = &blanked[..blanked.find("#[cfg(test)]").unwrap_or(blanked.len())];
                assert!(
                    !non_test.contains("0o0004"),
                    "src/gz/open.rs must not reintroduce the `0o0004` blanket \
                     fallback; on MIPS/SPARC Linux, Solaris, illumos, Haiku, QNX, \
                     GNU/Hurd, NuttX, Cygwin and Redox that bit is a different, \
                     real flag, so the old code set the wrong one rather than \
                     failing to set O_NONBLOCK"
                );

                // Both flag cascades must terminate in `None`.
                for name in [
                    "const O_NONBLOCK: Option<i32>",
                    "const FD_CLOEXEC: Option<i32>",
                ] {
                    let start = blanked
                        .find(name)
                        .unwrap_or_else(|| panic!("{rel} must declare `{name}`"));
                    let body = &blanked[start..];
                    let end = body
                        .find("\n};")
                        .unwrap_or_else(|| panic!("`{name}` must be a `;`-terminated cascade"));
                    let cascade = &body[..end];
                    let tail = cascade
                        .rfind("} else {")
                        .unwrap_or_else(|| panic!("`{name}` must have a terminal `else` arm"));
                    let terminal = cascade[tail..].trim();
                    assert!(
                        terminal.contains("None"),
                        "the terminal arm of `{name}` must be `None` so an \
                         unenumerated platform gets no flag at all, but it reads: \
                         {terminal:?}"
                    );
                    // Every non-terminal arm must yield a *named* constant, never
                    // an inline literal: an inline literal is unverifiable from a
                    // host that does not compile that arm, which is exactly how
                    // the original wrong values survived review.
                    let arms = cascade.matches("Some(").count();
                    assert!(
                        arms >= 2,
                        "`{name}` must enumerate at least two platform classes"
                    );
                    for digit in ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'] {
                        let inline = alloc::format!("Some({digit}");
                        assert!(
                            !cascade.contains(&inline),
                            "`{name}` must select a named constant per arm, not the \
                             inline literal `{inline}..`, so that the \
                             `const _: () = {{ .. }}` block can prove every \
                             platform's value from any host"
                        );
                    }
                }
            }

            if rel == "src/ffi/gz.rs" {
                saw_ffi = true;

                // The command cascade must terminate in the all-`None` table.
                let start = blanked
                    .find("const FCNTL: FcntlCmds")
                    .expect("src/ffi/gz.rs must declare the FCNTL selection");
                let body = &blanked[start..];
                let end = body
                    .find("\n};")
                    .expect("the FCNTL cascade must be `;`-terminated");
                let cascade = &body[..end];
                let tail = cascade
                    .rfind("} else {")
                    .expect("the FCNTL cascade must have a terminal `else` arm");
                assert!(
                    cascade[tail..].contains("FCNTL_UNKNOWN"),
                    "the terminal arm of the FCNTL cascade must be FCNTL_UNKNOWN, \
                     not FCNTL_POSIX: a platform nobody enumerated must be sent no \
                     fcntl command rather than POSIX's numbers"
                );

                // Ordering: every platform that renumbers the commands must be
                // matched before the broad POSIX arm, or `cfg!`'s first-match-wins
                // would hand it POSIX numbers.
                let posix_at = cascade
                    .find("FCNTL_POSIX")
                    .expect("the cascade must have a POSIX arm");
                for (divergent, why) in [
                    (
                        "FCNTL_HAIKU",
                        "Haiku's F_DUPFD is the POSIX F_GETFD, so a POSIX-numbered \
                         call there duplicates and leaks the descriptor",
                    ),
                    (
                        "FCNTL_NUTTX",
                        "NuttX renumbers F_GETFL/F_SETFL and defines no F_SETFD",
                    ),
                ] {
                    let at = cascade
                        .find(divergent)
                        .unwrap_or_else(|| panic!("the cascade must have a {divergent} arm"));
                    assert!(
                        at < posix_at,
                        "{divergent} (offset {at}) must be matched before \
                         FCNTL_POSIX (offset {posix_at}): {why}"
                    );
                }

                // No `fcntl` call may name its command with a literal. Every call
                // must pass a value bound out of the table, which is what makes the
                // per-platform numbering effective and guarantees a command is
                // never issued on a platform that does not define it.
                let mut inspected = 0usize;
                let mut from = 0usize;
                while let Some(at) = blanked[from..].find("fcntl(") {
                    let abs = from + at;
                    from = abs + "fcntl(".len();
                    // Require a word boundary before the name, so an identifier
                    // that merely *ends* in `fcntl` — such as the test function
                    // `a_partially_known_platform_issues_no_fcntl` — is not
                    // mistaken for a call.
                    let preceded_by_word_char = abs
                        .checked_sub(1)
                        .and_then(|i| blanked.as_bytes().get(i))
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
                    if preceded_by_word_char {
                        continue;
                    }
                    // Skip the `extern "C"` declaration itself, whose parameter
                    // list legitimately names `cmd: c_int`.
                    let args = &blanked[from..];
                    let close = match args.find(')') {
                        Some(c) => c,
                        None => continue,
                    };
                    let arglist = &args[..close];
                    if arglist.contains("c_int") {
                        continue;
                    }
                    inspected += 1;
                    let second = arglist.split(',').nth(1).unwrap_or("").trim();
                    assert!(
                        !second.is_empty(),
                        "an fcntl call must pass a command: {arglist:?}"
                    );
                    assert!(
                        !second.chars().next().is_some_and(|c| c.is_ascii_digit()),
                        "the fcntl command at offset {abs} is the literal \
                         {second:?}; it must come from the FCNTL table instead, so \
                         that Haiku and NuttX cannot be sent POSIX numbers"
                    );

                    // `fcntl` is variadic, so its arity is part of the command's
                    // contract rather than of its type: `F_GETFD`/`F_GETFL` take
                    // no third argument and `F_SETFD`/`F_SETFL` take exactly one
                    // `int`. Passing the wrong number is undefined behaviour that
                    // no signature can catch, so it is checked here against the
                    // table binding the call names.
                    let arity = arglist.split(',').count();
                    if second.starts_with("get_") {
                        assert_eq!(
                            arity, 2,
                            "the fcntl call at offset {abs} names the getter \
                             {second:?}, which takes no third argument, but passes \
                             {arity} arguments: {arglist:?}"
                        );
                    } else if second.starts_with("set_") {
                        assert_eq!(
                            arity, 3,
                            "the fcntl call at offset {abs} names the setter \
                             {second:?}, which requires exactly one `int` third \
                             argument, but passes {arity} arguments: {arglist:?} — \
                             a variadic call missing its argument reads whatever \
                             happens to be in the argument register"
                        );
                    } else {
                        panic!(
                            "the fcntl command at offset {abs} is {second:?}, which \
                             is neither a `get_*` nor a `set_*` binding from the \
                             FCNTL table; its required arity is therefore unknown \
                             and cannot be checked"
                        );
                    }
                }
                assert!(
                    inspected >= 4,
                    "at least the four reconciliation/probe fcntl call sites must \
                     be inspected, found {inspected} — a zero or low count would \
                     make this assertion vacuous"
                );
            }
        }

        assert!(
            saw_open && saw_ffi,
            "both files owning a platform cascade must be scanned"
        );
    }

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
