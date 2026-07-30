//! Version and compile-flags reporting — a safe-Rust port of the public
//! reporting functions from the C `zutil.c` baseline of zlib
//! `1.3.2.1-motley`.
//!
//! This module reproduces the three side-effect-free, publicly exported
//! reporting entry points of the C library:
//!
//! * [`zlib_version`] ← C `zlibVersion` (`zutil.c` L27-29)
//! * [`zlib_compile_flags`] ← C `zlibCompileFlags` (`zutil.c` L31-121)
//! * [`z_error`] ← C `zError` (`zutil.c` L139-141)
//!
//! # Naming
//!
//! `src/lib.rs` re-exports the historical camelCase C names (`zlibVersion`,
//! `zlibCompileFlags`, `zError`) so Rust callers see the identifiers
//! documented in `zlib.h`. To keep the crate `clippy`-clean under the
//! `non_snake_case` lint, the *canonical* implementations use idiomatic
//! snake_case names, and each is paired with a thin `#[allow(non_snake_case)]`
//! camelCase wrapper that forwards to it.
//!
//! # Single source of truth
//!
//! This module deliberately **bridges** rather than **duplicates**:
//!
//! * the version string is owned by the crate root ([`crate::ZLIB_VERSION`]);
//! * the error-message table is owned by [`ReturnCode::message`](crate::error::ReturnCode::message), which
//!   already ports the C `z_errmsg` array.
//!
//! # `no_std`
//!
//! The module is `no_std`-clean: it uses only `core` (namely
//! [`core::mem::size_of`] and the [`core::ffi`] C-ABI integer types) together
//! with other crate modules. It contains no `unsafe` code.

use crate::error::ReturnCode;
use core::ffi::{c_long, c_uint, c_ulong, c_void};

/// Returns the zlib version string, exactly as the C `zlibVersion` function
/// does.
///
/// The returned string is the crate-wide single source of truth
/// [`crate::ZLIB_VERSION`] (`"1.3.2.1-motley"`); this function never introduces
/// a second string literal, guaranteeing the FFI `zlibVersion` shim and any
/// idiomatic Rust caller observe an identical value.
#[inline]
#[must_use]
pub fn zlib_version() -> &'static str {
    crate::ZLIB_VERSION
}

/// C-compatible camelCase alias for [`zlib_version`].
///
/// Provided so `src/lib.rs` can re-export the historical `zlibVersion` name
/// from `zlib.h`. It forwards verbatim to [`zlib_version`].
#[allow(non_snake_case)]
#[inline]
#[must_use]
pub fn zlibVersion() -> &'static str {
    zlib_version()
}

/// Converts a zlib return code into its human-readable message, exactly as the
/// C `zError` function does.
///
/// This reproduces the C `ERR_MSG(err)` macro semantics precisely by bridging
/// to [`ReturnCode::message`] instead of re-declaring the `z_errmsg` table:
///
/// * for the nine defined codes in `-6..=2`, [`ReturnCode::from_c_int`] yields
///   `Some(code)` and the matching `z_errmsg` string is returned (with
///   [`ReturnCode::Ok`] mapping to the empty string, just like `z_errmsg[2]`);
/// * for any out-of-range code, [`ReturnCode::from_c_int`] yields `None` and
///   the empty string is returned, matching the C macro's out-of-range index
///   (`z_errmsg[9] == ""`).
#[inline]
#[must_use]
pub fn z_error(err: i32) -> &'static str {
    ReturnCode::from_c_int(err)
        .map(|rc| rc.message())
        .unwrap_or("")
}

/// C-compatible camelCase alias for [`z_error`].
///
/// Provided so `src/lib.rs` can re-export the historical `zError` name from
/// `zlib.h`. It forwards verbatim to [`z_error`].
#[allow(non_snake_case)]
#[inline]
#[must_use]
pub fn zError(err: i32) -> &'static str {
    z_error(err)
}

/// Maps a C-ABI type size (in bytes) to the two-bit code the C
/// `zlibCompileFlags` bitfield uses to describe it: `2 -> 0`, `4 -> 1`,
/// `8 -> 2`, and any other width `-> 3`.
///
/// This mirrors the four `switch ((int)(sizeof(...)))` blocks in the C
/// `zlibCompileFlags` implementation, whose `case 2/4/8` arms add `0/1/2` and
/// whose `default` arm adds `3`.
#[inline]
fn size_code(n: usize) -> u32 {
    match n {
        2 => 0,
        4 => 1,
        8 => 2,
        _ => 3,
    }
}

/// Packs the four C-ABI type widths into bits 0-7 of the compile-flags word,
/// mirroring the four consecutive `switch (sizeof(...))` blocks of C
/// `zlibCompileFlags` (`zutil.c` L35-L58).
///
/// The widths are parameters rather than being read from
/// [`core::mem::size_of`] inside this function so that the slot assignment is
/// directly testable for ABI shapes other than the host's. On an LP64 host
/// `voidpf` and `z_off_t` are both 8 bytes, which would make a swap of those
/// two slots invisible to any test that could only observe the host widths —
/// yet that swap is a real defect on LLP64 (Windows x86_64), where C `long`
/// stays 32-bit while pointers are 64-bit.
///
/// Slot layout, matching C exactly:
/// * bits 0-1 — `uInt`
/// * bits 2-3 — `uLong`
/// * bits 4-5 — `voidpf`
/// * bits 6-7 — `z_off_t`
fn type_size_bits(uint: usize, ulong: usize, voidpf: usize, z_off_t: usize) -> u32 {
    size_code(uint) | (size_code(ulong) << 2) | (size_code(voidpf) << 4) | (size_code(z_off_t) << 6)
}

/// Returns a bitfield describing the compile-time configuration of the
/// library, mirroring the C `zlibCompileFlags` function.
///
/// The concrete numeric value is a faithful *reconstruction* for the Rust
/// build rather than a fixed magic number: the **bit layout matches the C
/// `zlibCompileFlags` layout exactly**, and the type-size bits are computed
/// from the platform's actual C-ABI type widths via [`core::mem::size_of`].
/// All defined bits fit in a `u32` (the highest documented bit is 27); the FFI
/// layer widens the result to `c_ulong`.
///
/// # Layout
///
/// | Bits  | Meaning                                              |
/// |-------|------------------------------------------------------|
/// | 0-1   | `sizeof(uInt)`   -> [`c_uint`] width                  |
/// | 2-3   | `sizeof(uLong)`  -> [`c_ulong`] width                 |
/// | 4-5   | `sizeof(voidpf)` -> data-pointer width                |
/// | 6-7   | `sizeof(z_off_t)` -> [`c_long`] width                 |
/// | 8     | `ZLIB_DEBUG` (mirrors `debug_assertions`)            |
/// | 16    | `NO_GZCOMPRESS` (set when the `gz-io` feature is off) |
/// | 17    | `NO_GZIP` (set when the `gzip` feature is off)        |
/// | 27    | `gzprintf()` returns an error (always set in this build)   |
///
/// Bit 27 mirrors C `zutil.c`'s `flags += 1L << 27`, which C sets in the
/// `NO_vsnprintf && !ZLIB_INSECURE` case — a build whose `gzprintf`/`gzvprintf`
/// exist as symbols but return `Z_STREAM_ERROR` because no secure `*printf` was
/// available (`zlib.h`: bit 27 "1 means gzprintf() returns an error"). Rendering
/// a C `va_list` from Rust requires the unstable (nightly-only) `c_variadic`
/// feature, so this crate always ships exactly that error-returning stub for
/// `gzprintf`/`gzvprintf` and therefore always sets this bit.
///
/// Every other bit the C function can set (9-15, 18-26, and 28-31) is `0` in
/// this build: there is no assembler variant, no Windows API, no runtime-built
/// fixed or CRC tables, no PKZIP bug workaround, and no `FASTEST` mode.
#[inline]
#[must_use]
pub fn zlib_compile_flags() -> u32 {
    let mut flags: u32 = 0;

    // Type-size bits (0-7): computed from the real C-ABI widths of the Rust
    // `core::ffi` types standing in for zlib's `uInt`, `uLong`, `voidpf`, and
    // `z_off_t`, mirroring the four `switch (sizeof(...))` blocks in C.
    flags |= type_size_bits(
        core::mem::size_of::<c_uint>(),
        core::mem::size_of::<c_ulong>(),
        core::mem::size_of::<*const c_void>(),
        core::mem::size_of::<c_long>(),
    );

    // bit 8: ZLIB_DEBUG. Mirror the Rust `debug_assertions` build profile so
    // the flag reflects whether debug checks are compiled in.
    if cfg!(debug_assertions) {
        flags |= 1 << 8;
    }

    // bit 16: NO_GZCOMPRESS. Set when the gz file-I/O layer is not built,
    // i.e. when the `gz-io` cargo feature is disabled.
    #[cfg(not(feature = "gz-io"))]
    {
        flags |= 1 << 16;
    }

    // bit 17: NO_GZIP. Set when gzip framing is not built, i.e. when the
    // `gzip` cargo feature is disabled.
    #[cfg(not(feature = "gzip"))]
    {
        flags |= 1 << 17;
    }

    // bit 27: gzprintf() returns an error. C sets `1L << 27` for a build with
    // no secure `vsnprintf`/`snprintf` (`NO_vsnprintf && !ZLIB_INSECURE`), i.e.
    // one whose `gzprintf`/`gzvprintf` are present as symbols but return
    // `Z_STREAM_ERROR`. A functional `gzprintf` in this crate would require
    // Rust's unstable (nightly-only) C-variadic support, so this crate always
    // ships the documented error-returning stubs and sets this bit to match C.
    flags |= 1 << 27;

    // Bits 9-15, 18-26, and 28-31 are unconditionally 0 in this build (see the
    // function's doc comment for the rationale).
    flags
}

/// C-compatible camelCase alias for [`zlib_compile_flags`].
///
/// Provided so `src/lib.rs` can re-export the historical `zlibCompileFlags`
/// name from `zlib.h`. It forwards verbatim to [`zlib_compile_flags`].
#[allow(non_snake_case)]
#[inline]
#[must_use]
pub fn zlibCompileFlags() -> u32 {
    zlib_compile_flags()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_exact_and_matches_crate_constant() {
        assert_eq!(zlib_version(), "1.3.2.1-motley");
        assert_eq!(zlib_version(), crate::ZLIB_VERSION);
    }

    #[test]
    fn z_error_matches_c_err_msg_table() {
        // The nine defined codes in -6..=2, byte-identical to the C `z_errmsg`
        // table addressed via the `ERR_MSG` macro.
        assert_eq!(z_error(2), "need dictionary"); // Z_NEED_DICT
        assert_eq!(z_error(1), "stream end"); // Z_STREAM_END
        assert_eq!(z_error(0), ""); // Z_OK
        assert_eq!(z_error(-1), "file error"); // Z_ERRNO
        assert_eq!(z_error(-2), "stream error"); // Z_STREAM_ERROR
        assert_eq!(z_error(-3), "data error"); // Z_DATA_ERROR
        assert_eq!(z_error(-4), "insufficient memory"); // Z_MEM_ERROR
        assert_eq!(z_error(-5), "buffer error"); // Z_BUF_ERROR
        assert_eq!(z_error(-6), "incompatible version"); // Z_VERSION_ERROR
    }

    #[test]
    fn z_error_out_of_range_is_empty() {
        // Matches the C `ERR_MSG` out-of-range index (`z_errmsg[9] == ""`).
        assert_eq!(z_error(3), "");
        assert_eq!(z_error(-7), "");
        assert_eq!(z_error(100), "");
        assert_eq!(z_error(i32::MIN), "");
        assert_eq!(z_error(i32::MAX), "");
    }

    #[test]
    fn z_error_bridges_return_code_message() {
        // Cross-check every in-range code against the `ReturnCode` source of
        // truth to prove this module bridges rather than duplicates.
        for err in -6..=2 {
            let expected = ReturnCode::from_c_int(err)
                .map(|rc| rc.message())
                .unwrap_or("");
            assert_eq!(z_error(err), expected);
        }
    }

    #[test]
    fn compile_flags_type_size_bits_are_consistent() {
        // Cross-platform: assert the type-size bits agree with `size_of` for
        // the current target rather than hard-coding one platform's value.
        let flags = zlib_compile_flags();
        assert_eq!(flags & 0b11, size_code(core::mem::size_of::<c_uint>()));
        assert_eq!(
            (flags >> 2) & 0b11,
            size_code(core::mem::size_of::<c_ulong>())
        );
        assert_eq!(
            (flags >> 4) & 0b11,
            size_code(core::mem::size_of::<*const c_void>())
        );
        assert_eq!(
            (flags >> 6) & 0b11,
            size_code(core::mem::size_of::<c_long>())
        );
    }

    #[test]
    fn compile_flags_option_bits_match_build() {
        let flags = zlib_compile_flags();
        // bit 8: ZLIB_DEBUG mirrors `debug_assertions`.
        assert_eq!((flags >> 8) & 1, u32::from(cfg!(debug_assertions)));
        // bit 16: NO_GZCOMPRESS is set iff the `gz-io` feature is disabled.
        assert_eq!((flags >> 16) & 1, u32::from(!cfg!(feature = "gz-io")));
        // bit 17: NO_GZIP is set iff the `gzip` feature is disabled.
        assert_eq!((flags >> 17) & 1, u32::from(!cfg!(feature = "gzip")));
        // bit 27: gzprintf-returns-error is always set — the crate ships the
        // error-returning `gzprintf`/`gzvprintf` stubs (C-variadics are nightly
        // only), matching a zlib built without a secure `*printf`.
        assert_eq!((flags >> 27) & 1, 1);
    }

    #[test]
    fn compile_flags_reserved_bits_are_zero() {
        // Every bit the C function can set but this build never does: bits
        // 9-15 (0xFE00) and 18-31 EXCEPT bit 27 (gzprintf status, checked in
        // `compile_flags_option_bits_match_build`). 0xFFFC_0000 with bit 27
        // (0x0800_0000) cleared is 0xF7FC_0000.
        const RESERVED: u32 = 0xF7FC_FE00;
        assert_eq!(zlib_compile_flags() & RESERVED, 0);
    }

    /// The C `zlibCompileFlags` width-to-code mapping, transcribed directly from
    /// the `switch` case labels in `zutil.c` L35-L58 rather than delegating to
    /// this module's [`size_code`].
    ///
    /// Writing the expectation out independently is the whole point: a test that
    /// asks [`size_code`] what it thinks the answer is would agree with a broken
    /// [`size_code`]. C uses `case 2 -> +0`, `case 4 -> +1`, `case 8 -> +2` and
    /// `default -> +3`, and each slot is shifted left by `2 * slot_index`.
    fn expected_size_code(width: usize) -> u32 {
        match width {
            2 => 0,
            4 => 1,
            8 => 2,
            _ => 3,
        }
    }

    #[test]
    fn compile_flags_low_byte_always_matches_the_active_target() {
        // The four C types whose widths occupy the low byte, in C's slot order:
        // uInt (bits 0-1), uLong (bits 2-3), voidpf (bits 4-5), z_off_t (bits
        // 6-7). `z_off_t` is `c_long` in this port.
        let uint = core::mem::size_of::<c_uint>();
        let ulong = core::mem::size_of::<c_ulong>();
        let ptr = core::mem::size_of::<*const c_void>();
        let off = core::mem::size_of::<c_long>();

        // Derive the expectation for whatever target is actually being compiled,
        // then assert it UNCONDITIONALLY. There is deliberately no `if` here: the
        // predecessor of this test hid its only assertion behind a runtime LP64
        // check and therefore passed vacuously on every non-LP64 target.
        let expected = expected_size_code(uint)
            | (expected_size_code(ulong) << 2)
            | (expected_size_code(ptr) << 4)
            | (expected_size_code(off) << 6);
        let actual = zlib_compile_flags() & 0xFF;
        assert_eq!(
            actual, expected,
            "low byte mismatch for widths uInt={uint} uLong={ulong} voidpf={ptr} z_off_t={off}"
        );

        // Assert each 2-bit slot in isolation as well, so a compensating error
        // across two slots cannot cancel out in the aggregate comparison. The
        // tally guards against a future edit dropping a slot from the table.
        let slots = [
            ("uInt", uint),
            ("uLong", ulong),
            ("voidpf", ptr),
            ("z_off_t", off),
        ];
        let mut checked = 0_usize;
        for (slot, (name, width)) in slots.into_iter().enumerate() {
            let shift = 2 * slot as u32;
            assert_eq!(
                (actual >> shift) & 0b11,
                expected_size_code(width),
                "slot {slot} ({name}, width {width}) occupies bits {shift}-{}",
                shift + 1
            );
            checked += 1;
        }
        assert_eq!(checked, 4, "all four type-size slots must be checked");

        // Additionally pin the documented literal for each ABI shape this crate
        // is built for, so the derivation above cannot drift as a whole. The
        // catch-all arm still asserts, so an unrecognised target is never
        // silently skipped.
        match (uint, ulong, ptr, off) {
            // LP64: Linux/macOS x86_64, aarch64, s390x. 1 + 2<<2 + 2<<4 + 2<<6.
            (4, 8, 8, 8) => assert_eq!(actual, 0xA9, "LP64 low byte"),
            // ILP32: i686, armv7 and other 32-bit targets. 1 + 1<<2 + 1<<4 + 1<<6.
            (4, 4, 4, 4) => assert_eq!(actual, 0x55, "ILP32 low byte"),
            // LLP64: Windows x86_64, where C `long` stays 32-bit. 1 + 1<<2 + 2<<4 + 1<<6.
            (4, 4, 8, 4) => assert_eq!(actual, 0x65, "LLP64 low byte"),
            other => {
                // Not a shape with a hard-coded literal, but the unconditional
                // assertion above has already checked this target exactly.
                assert_eq!(
                    actual, expected,
                    "unrecognised ABI shape {other:?}; derived low byte still applies"
                );
            }
        }
    }

    #[test]
    fn type_size_bits_packs_every_abi_shape() {
        // Each expected value below is computed by hand from `zutil.c` L35-L58
        // and written as a literal, so this test is an INDEPENDENT anchor rather
        // than a restatement of the implementation. Because the widths are
        // parameters, shapes the host does not use — ILP32, LLP64, and the
        // 2-byte and over-wide `default` arms — are all exercised here on every
        // target, including the slot swap that is invisible at LP64 widths.
        const CASES: [(usize, usize, usize, usize, u32, &str); 8] = [
            // uInt uLong voidpf z_off_t  expected  shape
            (4, 8, 8, 8, 0xA9, "LP64: 1 + 2<<2 + 2<<4 + 2<<6"),
            (4, 4, 4, 4, 0x55, "ILP32: 1 + 1<<2 + 1<<4 + 1<<6"),
            (4, 4, 8, 4, 0x65, "LLP64: 1 + 1<<2 + 2<<4 + 1<<6"),
            (
                4,
                8,
                8,
                4,
                0x69,
                "LP64 with 32-bit z_off_t: 1 + 2<<2 + 2<<4 + 1<<6",
            ),
            (2, 2, 2, 2, 0x00, "all 2-byte: every slot codes 0"),
            (2, 4, 4, 4, 0x54, "16-bit uInt: 0 + 1<<2 + 1<<4 + 1<<6"),
            (
                16,
                4,
                4,
                4,
                0x57,
                "over-wide uInt hits `default`: 3 + 1<<2 + 1<<4 + 1<<6",
            ),
            (8, 8, 8, 8, 0xAA, "all 8-byte: 2 + 2<<2 + 2<<4 + 2<<6"),
        ];

        for (uint, ulong, voidpf, z_off_t, expected, shape) in CASES {
            assert_eq!(
                type_size_bits(uint, ulong, voidpf, z_off_t),
                expected,
                "{shape} (widths {uint}/{ulong}/{voidpf}/{z_off_t})"
            );
        }

        // The packing must occupy only the low byte: no slot may bleed into the
        // option bits at 8 and above.
        for (uint, ulong, voidpf, z_off_t, _, shape) in CASES {
            assert_eq!(
                type_size_bits(uint, ulong, voidpf, z_off_t) & !0xFF,
                0,
                "{shape} must not set any bit above bit 7"
            );
        }

        // Each slot must be independently addressable: varying one width may
        // only ever change its own 2-bit field. This is what pins the shift
        // amounts and catches two slots being transposed.
        // The expected deltas are hard-coded rather than derived from `slot`, so
        // this check carries information independent of the packing it verifies.
        // Widening one slot from 4 to 8 flips its code from 1 to 2 -- both of its
        // bits -- giving 0b11 positioned at that slot: 0x03, 0x0C, 0x30, 0xC0.
        const BASE: (usize, usize, usize, usize) = (4, 4, 4, 4);
        const DELTAS: [u32; 4] = [0x03, 0x0C, 0x30, 0xC0];
        let base_bits = type_size_bits(BASE.0, BASE.1, BASE.2, BASE.3);
        assert_eq!(base_bits, 0x55, "the ILP32 base must pack to 0x55");
        for (slot, expected_delta) in DELTAS.into_iter().enumerate() {
            let mut w = BASE;
            match slot {
                0 => w.0 = 8,
                1 => w.1 = 8,
                2 => w.2 = 8,
                _ => w.3 = 8,
            }
            assert_eq!(
                type_size_bits(w.0, w.1, w.2, w.3) ^ base_bits,
                expected_delta,
                "widening only slot {slot} must alter only bits {}-{}",
                2 * slot,
                2 * slot + 1
            );
        }
    }

    #[test]
    fn the_low_byte_contract_cannot_be_silently_weakened() {
        // Two ways to break this contract are *observationally identical* on an
        // LP64 host and so cannot be caught by any runtime assertion here:
        //
        //  1. hard-coding `flags |= 0xA9` instead of computing the widths, which
        //     is correct on LP64 and wrong on ILP32 and LLP64; and
        //  2. re-hiding the low-byte assertion behind a runtime width check,
        //     which is exactly the vacuous-test defect this test set replaced.
        //
        // Both are therefore pinned structurally against this file's own source.
        let source = include_str!("version.rs");

        // (1) The production function must derive the low byte, not assert it.
        let producer = source
            .split_once("pub fn zlib_compile_flags()")
            .expect("zlib_compile_flags must exist")
            .1;
        let producer = producer
            .split_once("\n}\n")
            .expect("zlib_compile_flags must be delimited")
            .0;
        assert!(
            producer.contains("type_size_bits("),
            "zlib_compile_flags must obtain bits 0-7 from type_size_bits, so the \
             packing stays width-derived and testable for non-host ABI shapes"
        );
        for literal in ["0xA9", "0x55", "0x65", "169", "0xFF"] {
            assert!(
                !producer.contains(literal),
                "zlib_compile_flags must not hard-code the low byte ({literal}); \
                 that is correct only on one ABI shape"
            );
        }

        // (2) The active-target test must assert unconditionally. A `match` on
        // the ABI shape is fine (every arm asserts); an `if` is not, because it
        // is how an assertion gets skipped.
        let needle = "fn compile_flags_low_byte_always_matches_the_active_target";
        let body = source
            .split_once(needle)
            .expect("the active-target test must exist")
            .1;
        let body = body
            .split_once("\n    }\n")
            .expect("the active-target test must be delimited")
            .0;
        assert!(
            body.contains("let actual = zlib_compile_flags() & 0xFF;"),
            "the active-target test must read the real flags word directly"
        );
        for line in body.lines() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            assert!(
                !code.starts_with("if ") && !code.contains(" if "),
                "the active-target test must contain no conditional, or its \
                 assertions can pass vacuously on some target; found: {code}"
            );
        }
    }

    #[test]
    fn expected_size_code_matches_the_c_case_labels() {
        // Guards the independent mapping used above against the C source, and in
        // doing so also pins `size_code` from outside itself.
        assert_eq!(expected_size_code(2), 0, "zutil.c `case 2: break`");
        assert_eq!(expected_size_code(4), 1, "zutil.c `case 4: flags += 1`");
        assert_eq!(expected_size_code(8), 2, "zutil.c `case 8: flags += 2`");
        for odd in [0_usize, 1, 3, 5, 6, 7, 12, 16] {
            assert_eq!(
                expected_size_code(odd),
                3,
                "zutil.c `default: flags += 3` for width {odd}"
            );
        }
        // The independent transcription and the production mapping must agree on
        // every width the C switch can observe.
        for width in [0_usize, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16] {
            assert_eq!(
                expected_size_code(width),
                size_code(width),
                "size_code disagrees with the C case labels at width {width}"
            );
        }
    }

    #[test]
    fn size_code_maps_widths() {
        assert_eq!(size_code(2), 0);
        assert_eq!(size_code(4), 1);
        assert_eq!(size_code(8), 2);
        // Any other width falls through to the C `default` arm.
        assert_eq!(size_code(1), 3);
        assert_eq!(size_code(16), 3);
        assert_eq!(size_code(0), 3);
    }

    #[test]
    fn camelcase_aliases_delegate() {
        assert_eq!(zlibVersion(), zlib_version());
        assert_eq!(zlibCompileFlags(), zlib_compile_flags());
        for err in [-7, -6, -1, 0, 1, 2, 3, 100] {
            assert_eq!(zError(err), z_error(err));
        }
    }
}
