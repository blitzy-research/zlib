//! Utility layer for the `zlib-rs` crate.
//!
//! This module is the root of the utility layer and is the idiomatic-Rust
//! counterpart to the C header `zutil.h`. In the C sources, `zutil.h` is the
//! shared internal interface pulled in by nearly every translation unit via
//! `#include "zutil.h"`; the Rust convention that replaces that include is
//! `use crate::{error::ZlibError, util::*};`, where `crate::util` is exactly
//! this module.
//!
//! The concrete logic lives in three sibling submodules, all re-exported here:
//!
//! - One-call, whole-buffer compression wrappers ported from C `compress.c`
//!   (`compress`, `compress2`, `compress_bound`), in [`compress`](mod@compress).
//! - One-call, whole-buffer decompression wrappers ported from C `uncompr.c`
//!   (`uncompress`, `uncompress2`), in [`uncompress`](mod@uncompress).
//! - Version, compile-flag, and error-string reporting ported from C
//!   `zutil.c` (`zlib_version`, `zlib_compile_flags`, `z_error`), in
//!   [`version`].
//!
//! In addition, this module is the canonical home for the small set of shared
//! internal constants that `zutil.h` owned (the block-type tags, the LZ77
//! match-length bounds, and the preset-dictionary flag) together with the
//! gzip-header operating-system identifier byte [`OS_CODE`].
//!
//! # `zmem*` helper mapping
//!
//! The C library defines `zmemcpy`, `zmemcmp`, and `zmemzero` as thin wrappers
//! over `memcpy`, `memcmp`, and `memset`. In safe Rust these have no standalone
//! function form; each collapses into an inline slice operation performed at
//! the call site:
//!
//! - `zmemcpy(dst, src, n)` becomes `dst[..n].copy_from_slice(&src[..n])`
//! - `zmemcmp(a, b, n)` becomes `a[..n] == b[..n]` (or `a[..n].cmp(&b[..n])`
//!   when an ordering result is required)
//! - `zmemzero(b, n)` becomes `b[..n].fill(0)`
//!
//! These operations are emitted directly by their consumers (the deflate and
//! inflate engines), so no `zmem*` functions are defined in this module by
//! design.
//!
//! # Safety and environment
//!
//! Every item in this module is safe Rust; the layer contains no `unsafe`
//! blocks. No exported item references `std`, so the layer participates cleanly
//! in `no_std` builds; the `#[cfg(test)]` module at the bottom of this file uses
//! `alloc::vec!` for scratch output buffers, which `no_std` builds also provide
//! through the crate-root `extern crate alloc`. Raw-pointer and `extern "C"`
//! variants of the wrappers live in `crate::ffi`, not here.

/// One-call, whole-buffer compression wrappers ported from C `compress.c`.
pub mod compress;
/// One-call, whole-buffer decompression wrappers ported from C `uncompr.c`.
pub mod uncompress;
/// Version, compile-flag, and error-string reporting ported from C `zutil.c`.
pub mod version;

// ---------------------------------------------------------------------------
// Shared internal constants (ported from `zutil.h`).
//
// `src/constants.rs` deliberately does not define these; this module is their
// canonical public home. Engine modules may keep their own differently-typed
// aliases where the arithmetic demands it - `crate::deflate::trees` needs `i32`
// block-type tags for `send_bits`, and `crate::deflate::state` needs `usize`
// match bounds for indexing - and the tests at the bottom of this file assert
// that every such alias still agrees with the canonical value here.
//
// `OS_CODE` is the one constant no module may re-declare: it is genuinely
// platform-dependent, so a hard-coded copy would emit the wrong gzip OS byte on
// some target. `crate::deflate` imports it from here (see the note above
// `PRESET_DICT` in `src/deflate/mod.rs`).
// ---------------------------------------------------------------------------

/// Block-type tag for a *stored* (uncompressed) DEFLATE block.
///
/// Mirrors `STORED_BLOCK` in C `zutil.h` (line 87).
pub const STORED_BLOCK: u8 = 0;

/// Block-type tag for a block compressed with the *static* Huffman trees.
///
/// Mirrors `STATIC_TREES` in C `zutil.h` (line 88).
pub const STATIC_TREES: u8 = 1;

/// Block-type tag for a block compressed with *dynamic* Huffman trees.
///
/// Mirrors `DYN_TREES` in C `zutil.h` (line 89).
pub const DYN_TREES: u8 = 2;

/// Minimum LZ77 match length DEFLATE encodes as a length/distance pair.
///
/// Mirrors `MIN_MATCH` in C `zutil.h` (line 92). Typed as `usize` because it
/// participates in buffer indexing and match-length arithmetic.
pub const MIN_MATCH: usize = 3;

/// Maximum LZ77 match length DEFLATE encodes as a length/distance pair.
///
/// Mirrors `MAX_MATCH` in C `zutil.h` (line 93). Typed as `usize` because it
/// participates in buffer indexing and match-length arithmetic.
pub const MAX_MATCH: usize = 258;

/// Preset-dictionary flag bit (`FDICT`) in the zlib header `FLG` byte.
///
/// Mirrors `PRESET_DICT` in C `zutil.h` (line 96).
pub const PRESET_DICT: u8 = 0x20;

// ---------------------------------------------------------------------------
// Platform `OS_CODE` selection (ported from `zutil.h` lines 98-189).
//
// C selects the RFC 1952 operating-system identifier through a long `#ifdef`
// cascade; here it collapses to a single compile-time constant chosen with
// `cfg` attributes. Only targets that have a stable Rust `cfg` predicate are
// distinguished (Windows and Apple); every other target - including Linux and
// the historical codes C enumerated (Amiga = 1, VMS = 2, ATARI = 5, OS/2 = 6,
// old Mac OS = 7, RISC OS = 13, BeOS = 16, OS/400 = 18, MS-DOS = 0) -
// collapses to the Unix default of 3, matching C's `#ifndef OS_CODE` fallback.
// Exactly one variant below is compiled for any given target.
//
// WHAT CONSUMES THIS CONSTANT: the gzip encoder. `crate::deflate` imports
// `OS_CODE` from here and writes it into the RFC 1952 header as
// `put_byte(s, OS_CODE)`, which is exactly what `deflate.c` does with the
// `zutil.h` macro. That is why no module may re-declare it: a private hard-coded
// `u8 = 3` copy under this name anywhere else would shadow the cascade below, so
// a Windows or Apple build would emit `3` where the reference C library emits
// `10` or `19`. `src/lib.rs`'s `os_code_is_declared_in_exactly_one_module`
// counts the declarations across the tree to keep that from happening.
//
// This does NOT put byte-identity at risk, and the reasoning matters because the
// opposite conclusion is easy to reach. Byte identity (AAP §0.8.1 directive D-1)
// is defined against the reference C library built for the SAME target, and C's
// own `zutil.h` selects the same platform value through the same cascade. So
// consuming this constant is what PRESERVES parity; hard-coding one value on
// every target is what breaks it. Compressed output has never been comparable
// across targets, because reference zlib's is not either. See the `os` field
// documentation on [`crate::gz_header::GzHeader`] for the full
// default/custom-header contract, including how a caller-supplied
// [`crate::gz_header::GzHeader`] overrides this default.
//
// HOW THE CASCADE IS SPELLED: through `cfg_if::cfg_if!`, the crate's declared
// stand-in for the C `#if`/`#elif`/`#else` preprocessor nests (AAP §0.5.1,
// §0.5.3). This is the structure `zutil.h` L98-L189 actually has - a single
// chain in which the first matching arm wins - and writing it as a chain keeps
// that property mechanical rather than manual: `cfg_if!` derives each arm's
// predicate by negating every arm above it, so the arms cannot overlap and
// cannot leave a gap. The expansion is predicate-for-predicate identical to the
// hand-written attributes it replaces (`windows`;
// `all(not(windows), target_vendor = "apple")`;
// `all(not(windows), not(target_vendor = "apple"))`), so the selected value is
// unchanged on every target.
//
// The mirror cascade in this module's tests (`EXPECTED_OS_CODE`) is deliberately
// left as raw `#[cfg]` attributes. It exists to derive the same answer a second,
// independent way, and routing both through the same macro would forfeit exactly
// that independence.
// ---------------------------------------------------------------------------

cfg_if::cfg_if! {
    if #[cfg(windows)] {
        /// The `zutil.h` operating-system identifier byte for the current target.
        ///
        /// On Windows this is `10`, mirroring the C `WIN32 && !__CYGWIN__` branch
        /// of `zutil.h` (lines 156-158). Cygwin reports the Unix code and is
        /// excluded by Rust's `cfg(windows)` predicate.
        ///
        /// Written into the gzip header by [`crate::deflate`]; see the module
        /// note above for why that preserves byte-identity rather than
        /// threatening it.
        pub const OS_CODE: u8 = 10;
    } else if #[cfg(target_vendor = "apple")] {
        /// The `zutil.h` operating-system identifier byte for the current target.
        ///
        /// On Apple platforms this is `19`, mirroring the C `__APPLE__` branch of
        /// `zutil.h` (lines 168-170). Windows is already claimed by the arm
        /// above, which is what makes the bare `target_vendor` predicate here
        /// equivalent to the C cascade's ordering.
        ///
        /// Written into the gzip header by [`crate::deflate`]; see the module
        /// note above for why that preserves byte-identity rather than
        /// threatening it.
        pub const OS_CODE: u8 = 19;
    } else {
        /// The `zutil.h` operating-system identifier byte for the current target.
        ///
        /// This is the Unix default of `3`, mirroring the C `#ifndef OS_CODE`
        /// fallback in `zutil.h` (lines 187-189). It applies to Linux and every
        /// other target that lacks a more specific stable `cfg` predicate.
        ///
        /// Written into the gzip header by [`crate::deflate`]; this is the arm
        /// every CI job compiles, which is why the shadowing bug described in the
        /// module note above was invisible on Linux.
        pub const OS_CODE: u8 = 3;
    }
}

// A compile-time cross-check of the cascade above, evaluated by `const` folding
// on every build of the library. It restates the selection as a `cfg!` chain -
// an expression form, independent of the item-attribute form `cfg_if!` emits -
// so a mistake in one spelling cannot be masked by the same mistake in the
// other. Being a `const` assertion rather than a `#[test]`, it is proved by a
// bare `cargo check --target <triple>` for a target this host cannot execute,
// which is what makes the Windows (`10`) and Apple (`19`) arms verifiable here
// and not merely inspectable.
const _: () = assert!(
    OS_CODE
        == if cfg!(windows) {
            10
        } else if cfg!(target_vendor = "apple") {
            19
        } else {
            3
        },
    "OS_CODE must equal the byte zutil.h's cascade selects for this target"
);

// ---------------------------------------------------------------------------
// Public API re-exports.
//
// `src/lib.rs` re-exports these from the crate root so that both
// `crate::util::<item>` and the top-level crate path resolve. They are NOT in
// `crate::prelude`, which deliberately carries only types (the public enums,
// `ReturnCode`/`ZlibError`, `GzHeader`, `ZStream` and the allocator traits) so
// that a glob import cannot pull free functions into scope. Both the idiomatic
// snake_case names and the camelCase C-parity aliases defined by the child
// modules are surfaced. The `compress`/`uncompress` module names (type
// namespace) and the re-exported functions of the same spelling (value
// namespace) coexist without conflict.
// ---------------------------------------------------------------------------

/// One-call compression entry points: `compress` and `compress2`, plus the
/// output-bound helper `compress_bound` and its C-parity alias `compressBound`.
pub use compress::{compress, compress_bound, compress2, compressBound};

// Crate-internal only: the tracked variant of `compress2` that also reports the
// produced byte count on the error paths, as C `compress2_z` does
// (`compress.c` L63). The C-ABI shims in `src/ffi/util.rs` need it to publish a
// partial `*destLen`; it is deliberately not part of the public Rust surface,
// whose `compress2` already carries the count in its `Ok` arm.
pub(crate) use compress::compress2_tracked;

/// One-call decompression entry points: `uncompress` and `uncompress2`.
pub use uncompress::{uncompress, uncompress2};

/// Version, compile-flag, and error-string reporting entry points, each paired
/// with its camelCase C-parity alias (`zlibVersion`, `zlibCompileFlags`,
/// `zError`).
pub use version::{
    z_error, zError, zlib_compile_flags, zlib_version, zlibCompileFlags, zlibVersion,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared `zutil.h` constants, asserted against their C values.
    ///
    /// These are wire-format and ABI values: preservation directive D-2 forbids
    /// altering them, and a changed value would silently corrupt output rather
    /// than fail to compile, so each is pinned to the literal from the C header.
    #[test]
    fn shared_constants_match_the_c_header() {
        // zutil.h L87-L89 — DEFLATE block-type tags.
        assert_eq!(STORED_BLOCK, 0, "STORED_BLOCK (zutil.h L87)");
        assert_eq!(STATIC_TREES, 1, "STATIC_TREES (zutil.h L88)");
        assert_eq!(DYN_TREES, 2, "DYN_TREES (zutil.h L89)");
        // zutil.h L92-L93 — LZ77 match-length bounds.
        assert_eq!(MIN_MATCH, 3, "MIN_MATCH (zutil.h L92)");
        assert_eq!(MAX_MATCH, 258, "MAX_MATCH (zutil.h L93)");
        // zutil.h L96 — FDICT flag bit in the zlib header FLG byte.
        assert_eq!(PRESET_DICT, 0x20, "PRESET_DICT (zutil.h L96)");

        // The tags are three consecutive values, which is what lets `trees.rs`
        // emit a block type as `(tag << 1) + last` in three bits.
        assert_eq!(STATIC_TREES, STORED_BLOCK + 1);
        assert_eq!(DYN_TREES, STATIC_TREES + 1);
        // 258 == 3 + 255: the maximum match is representable as the minimum plus
        // a full byte of extension, which is why the length codes cover it.
        assert_eq!(MAX_MATCH - MIN_MATCH, 255);
    }

    /// The differently-typed aliases the engines keep for arithmetic locality
    /// still agree with the canonical values above.
    ///
    /// Without this guard the two sets could drift silently: nothing in the
    /// compiler relates `crate::deflate::trees::STORED_BLOCK` (an `i32`, needed
    /// by `send_bits`) or `crate::deflate::state::MIN_MATCH` (a `usize`, needed
    /// for window indexing) to the `u8`/`usize` values published here.
    #[test]
    fn engine_aliases_agree_with_the_canonical_values() {
        use crate::deflate::state::{MAX_MATCH as ST_MAX, MIN_MATCH as ST_MIN};
        use crate::deflate::trees::{
            DYN_TREES as TR_DYN, STATIC_TREES as TR_STATIC, STORED_BLOCK as TR_STORED,
        };

        assert_eq!(TR_STORED, i32::from(STORED_BLOCK), "trees::STORED_BLOCK");
        assert_eq!(TR_STATIC, i32::from(STATIC_TREES), "trees::STATIC_TREES");
        assert_eq!(TR_DYN, i32::from(DYN_TREES), "trees::DYN_TREES");
        assert_eq!(ST_MIN, MIN_MATCH, "state::MIN_MATCH");
        assert_eq!(ST_MAX, MAX_MATCH, "state::MAX_MATCH");
    }

    /// The expected gzip OS byte for the target this test is compiled for,
    /// derived from the same `cfg` predicates `OS_CODE` itself uses.
    ///
    /// Written as a `cfg`-selected constant rather than a runtime `if` so that
    /// **every** target gets exactly one value and the assertions below can never
    /// be vacuous: on a target where no arm applied the constant would not exist
    /// and the test would fail to compile.
    #[cfg(windows)]
    const EXPECTED_OS_CODE: u8 = 10;
    #[cfg(all(not(windows), target_vendor = "apple"))]
    const EXPECTED_OS_CODE: u8 = 19;
    #[cfg(all(not(windows), not(target_vendor = "apple")))]
    const EXPECTED_OS_CODE: u8 = 3;

    /// `OS_CODE` is the platform value C's `#ifdef` cascade would have selected:
    /// `10` on Windows (`zutil.h` L156-L158), `19` on Apple (L168-L170), and the
    /// `3` "assume Unix" fallback otherwise (L187-L189).
    #[test]
    fn os_code_is_the_platform_value() {
        assert_eq!(OS_CODE, EXPECTED_OS_CODE);

        // Cross-check against the cfg cascade a second, independent way, so a
        // mistake in a single `cfg` attribute cannot make both sides agree.
        if cfg!(windows) {
            assert_eq!(OS_CODE, 10, "Windows uses the C WIN32 code");
        } else if cfg!(target_vendor = "apple") {
            assert_eq!(OS_CODE, 19, "Apple targets use the C __APPLE__ code");
        } else {
            assert_eq!(OS_CODE, 3, "every other target assumes Unix");
        }

        // Whatever the target, the byte must be one C can produce.
        assert!(
            [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 13, 16, 18, 19].contains(&OS_CODE),
            "OS_CODE {OS_CODE} is not one of the codes zutil.h defines"
        );
    }

    /// The gzip header this crate emits carries the canonical [`OS_CODE`] — the
    /// one platform-selected definition — at RFC 1952 offset 9.
    ///
    /// This is the assertion that catches the defect the test exists for: if
    /// `crate::deflate` were to hard-code an `OS_CODE = 3` of its own, then on
    /// Windows or Apple it would emit `3` where reference zlib emits `10` or
    /// `19` — diverging from byte-identity on those targets while every
    /// Linux-only gate stayed green.
    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_header_emits_the_canonical_os_code() {
        use crate::constants::{Strategy, Z_DEFLATED, Z_FINISH};
        use crate::error::ReturnCode;
        use crate::stream::ZStream;

        const DATA: &[u8] = b"gzip header operating-system byte";

        let mut strm: ZStream = ZStream::new();
        // windowBits 31 == 15 + 16: gzip framing.
        crate::deflate::deflate_init2(&mut strm, 6, Z_DEFLATED, 31, 8, Strategy::Default)
            .expect("init");
        let mut out = alloc::vec![0u8; DATA.len() * 2 + 128];
        let r = crate::deflate::deflate(&mut strm, DATA, &mut out, Z_FINISH);
        assert_eq!(r.code, ReturnCode::StreamEnd);
        out.truncate(r.produced);
        crate::deflate::deflate_end(&mut strm).expect("end");

        // RFC 1952 §2.3: ID1 ID2 CM FLG MTIME[4] XFL OS -> OS is byte 9.
        assert!(out.len() > 9, "a gzip member has at least a 10-byte header");
        assert_eq!(out[0], 0x1f, "ID1");
        assert_eq!(out[1], 0x8b, "ID2");
        assert_eq!(out[2], 8, "CM == Z_DEFLATED");
        assert_eq!(
            out[9], OS_CODE,
            "the emitted OS byte must be the single canonical OS_CODE"
        );
        assert_eq!(
            out[9], EXPECTED_OS_CODE,
            "and it must be the platform value"
        );
    }

    /// A caller-supplied [`GzHeader`](crate::gz_header::GzHeader) overrides the
    /// OS byte, exactly as C writes `s->gzhead->os & 0xff` (`deflate.c` L1105)
    /// instead of `OS_CODE` (L1081) when a header is installed. The override path
    /// must therefore *not* be rewired to the canonical constant.
    #[cfg(feature = "gzip")]
    #[test]
    fn supplied_header_overrides_the_os_code() {
        use crate::constants::{Strategy, Z_DEFLATED, Z_FINISH};
        use crate::error::ReturnCode;
        use crate::gz_header::GzHeader;
        use crate::stream::ZStream;

        const DATA: &[u8] = b"explicit gzip header";
        const CUSTOM_OS: u8 = 7; // old Mac OS, zutil.h L149

        let mut strm: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut strm, 6, Z_DEFLATED, 31, 8, Strategy::Default)
            .expect("init");
        let head = GzHeader {
            os: i32::from(CUSTOM_OS),
            ..GzHeader::default()
        };
        crate::deflate::deflate_set_header(&mut strm, Some(head)).expect("set header");

        let mut out = alloc::vec![0u8; DATA.len() * 2 + 128];
        let r = crate::deflate::deflate(&mut strm, DATA, &mut out, Z_FINISH);
        assert_eq!(r.code, ReturnCode::StreamEnd);
        out.truncate(r.produced);
        crate::deflate::deflate_end(&mut strm).expect("end");

        assert_eq!(out[9], CUSTOM_OS, "a supplied header wins over OS_CODE");
    }
}
