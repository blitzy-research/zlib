//! # `ffi` — the C-compatible drop-in boundary of `zlib-rs`
//!
//! This module is the crate's **foreign-function-interface (FFI) boundary**: the
//! single place where the idiomatic, memory-safe Rust engines are exposed to C
//! callers with **byte-for-byte zlib C ABI fidelity**. Compiled into the
//! `cdylib`/`staticlib` artifacts (see the crate's `[lib] crate-type`), it turns
//! `zlib-rs` into a drop-in replacement for the reference `libz`: a C program can
//! link against the emitted object, or an operator can inject it via an
//! `LD_PRELOAD`-style substitution, and every documented `zlib.h` entry point
//! resolves to a safe-Rust implementation without any source changes on the
//! caller's side.
//!
//! ## What lives here
//!
//! * `#[unsafe(no_mangle)] extern "C"` **shims** — one per public `zlib.h`
//!   prototype — that validate raw C inputs, bridge to the safe engines, and
//!   re-materialize zlib's integer return codes at the boundary. They are defined
//!   in the submodules [`util`], [`deflate`](mod@deflate), [`inflate`](mod@inflate), and `gz`.
//! * `#[repr(C)]` **struct mirrors** ([`z_stream`], [`gz_header`], [`gzFile_s`])
//!   whose field order and widths match `zlib.h` exactly, the C scalar aliases
//!   ([`Bytef`], [`uInt`], [`uLong`], …), the C function-pointer typedefs
//!   ([`alloc_func`], [`free_func`], [`in_func`], [`out_func`]), and the
//!   caller-allocator bridge [`CAllocator`]. These are defined in [`types`] and
//!   re-exported here (see also the raw ↔ idiomatic conversion helpers in
//!   [`types`], which turn a raw [`z_stream`]/[`gz_header`] into the idiomatic
//!   [`ZStream`](crate::stream::ZStream)/[`GzHeader`](crate::gz_header::GzHeader)).
//!
//! ## Safe core, unsafe boundary
//!
//! Per the migration's unsafe-isolation strategy (AAP §0.6.2 / §0.7.2 standard S2), **all**
//! `unsafe` in the crate is confined to this `ffi` tree. The compression,
//! decompression, checksum, gzip-I/O, and one-call engines — [`crate::deflate`],
//! [`crate::inflate`], [`crate::checksum`], `crate::gz`, and [`crate::util`] —
//! contain the actual logic and are written in fully safe Rust (the deflate
//! engine has **zero** `unsafe`). Every `unsafe` operation in the sibling shim
//! files carries a `// SAFETY:` justification, and every fallible shim body is
//! wrapped in a `catch_unwind` guard so a Rust panic can never unwind across the
//! `extern "C"` boundary into C.
//!
//! This root file itself contains **no** `unsafe`, **no** `#[unsafe(no_mangle)]`,
//! and **no** business logic: it only declares the submodules, re-exports their
//! public surface, and documents the exported symbol table. The
//! `#[unsafe(no_mangle)]` shims are emitted into the object file as exported
//! symbols by virtue of being compiled into the crate — *independent* of any
//! re-export. The `pub use` statements below therefore exist to present a single,
//! centralized Rust API surface (`zlib_rs::ffi::<symbol>`) and to document the
//! ABI; they do **not** change symbol emission.
//!
//! ## Submodule map
//!
//! | Submodule   | C sources                                                     | Responsibility                                             |
//! |-------------|---------------------------------------------------------------|------------------------------------------------------------|
//! | [`types`]   | `zlib.h`, `zconf.h`                                            | `#[repr(C)]` ABI mirrors, scalar aliases, allocator bridge |
//! | [`util`]    | `compress.c`, `uncompr.c`, `adler32.c`, `crc32.c`, `zutil.c`   | one-call, checksum, and version/error shims                |
//! | [`deflate`](mod@deflate) | `deflate.c`, `zlib.h`                                          | `deflate*` compression shims                               |
//! | [`inflate`](mod@inflate) | `inflate.c`, `infback.c`, `zlib.h`                             | `inflate*` / `inflateBack*` decompression shims            |
//! | `gz`        | `gzlib.c`, `gzread.c`, `gzwrite.c`, `gzclose.c`, `gzguts.h`    | `gz*` file-I/O shims (Cargo feature `gz-io`)               |
//!
//! ## Feature gating
//!
//! The `gz` submodule maps zlib's gzip file-I/O layer, which fundamentally
//! requires the standard library for filesystem access. It is therefore compiled
//! only when the `gz-io` Cargo feature is enabled (which implies `std` and
//! `gzip`), matching the gating in `crate::gz` and the `#[cfg(feature =
//! "gz-io")] pub mod gz;` declaration in `src/lib.rs`. The `#[cfg(feature =
//! "gz-io")]` attribute on the `pub mod gz;` declaration below is the single
//! gate for `src/ffi/gz.rs`: that file deliberately does **not** repeat it as a
//! module-level `#![cfg(…)]`, because an inner `cfg` duplicating the one on the
//! `mod` declaration is reported as a `clippy::duplicated_attributes` error by
//! the Clippy shipped with the pinned MSRV toolchain (`rust-toolchain.toml`).
//! A build **without** `gz-io` still links the complete set of core
//! (`deflate`/`inflate`/`checksum`/one-call/version) symbols: [`types`],
//! [`util`], [`deflate`](mod@deflate), and [`inflate`](mod@inflate) are always compiled so the core zlib
//! symbol table is always present for linkage.
//!
//! Within the `gz-io` layer, `gzprintf`/`gzvprintf` are always exported as
//! ABI-compatible **error-returning stubs** that yield `Z_STREAM_ERROR`:
//! rendering a C `va_list` from Rust requires the unstable (nightly-only)
//! `c_variadic` feature, so — precisely as zlib documents for a build without
//! secure `*printf` (and reflected by `zlibCompileFlags` bit 27; see
//! [`crate::util::version`]) — the symbols exist but return an error. They
//! always resolve, so a C caller's link never fails.
//!
//! ## `zlib.map` symbol-versioning contract
//!
//! zlib ships a linker version script (`zlib.map`) that assigns each exported
//! symbol to an ELF version node so consumers link against a specific
//! `libz.so.1` symbol version. Reproducing symbol versioning on the emitted
//! `cdylib` is **optional** for a functional drop-in — the `#[unsafe(no_mangle)]`
//! shims are exported *unversioned* by default, which satisfies ordinary linking
//! and `LD_PRELOAD` injection. The mapping is recorded here verbatim so a
//! downstream packaging step can generate a version script and achieve strict
//! `libz.so.1` symbol-versioning parity if required.
//!
//! **Unversioned base symbols** (predate the versioning scheme introduced in
//! zlib 1.2.0; they are *not* listed in any `zlib.map` node and are exported
//! without a version tag): `deflate`, `inflate`, `deflateInit_`, `deflateInit2_`,
//! `inflateInit_`, `inflateInit2_`, `deflateEnd`, `inflateEnd`, `deflateReset`,
//! `inflateReset`, `deflateParams`, `deflateSetDictionary`,
//! `inflateSetDictionary`, `deflateCopy`, `compress`, `compress2`, `uncompress`,
//! `adler32`, `crc32`, `get_crc_table`, `gzopen`, `gzdopen`, `gzread`, `gzwrite`,
//! `gzprintf`, `gzputs`, `gzgets`, `gzputc`, `gzgetc`, `gzflush`, `gzseek`,
//! `gzrewind`, `gztell`, `gzeof`, `gzclose`, `gzerror`, `gzsetparams`,
//! `zlibVersion`, `zError`, `inflateSync`, `inflateSyncPoint`.
//!
//! **Versioned nodes** (exactly as enumerated in the repository's `zlib.map`;
//! each node inherits the symbols of the node it extends):
//!
//! * `ZLIB_1.2.0`: `compressBound`, `deflateBound`, `inflateBack`,
//!   `inflateBackEnd`, `inflateBackInit_`, `inflateCopy`.
//! * `ZLIB_1.2.0.2`: `gzclearerr`, `gzungetc`, `zlibCompileFlags`.
//! * `ZLIB_1.2.0.8`: `deflatePrime`.
//! * `ZLIB_1.2.2`: `adler32_combine`, `crc32_combine`, `deflateSetHeader`,
//!   `inflateGetHeader`.
//! * `ZLIB_1.2.2.3`: `deflateTune`, `gzdirect`.
//! * `ZLIB_1.2.2.4`: `inflatePrime`.
//! * `ZLIB_1.2.3.3`: `adler32_combine64`, `crc32_combine64`, `gzopen64`,
//!   `gzseek64`, `gztell64`, `inflateUndermine`.
//! * `ZLIB_1.2.3.4`: `inflateReset2`, `inflateMark`.
//! * `ZLIB_1.2.3.5`: `gzbuffer`, `gzoffset`, `gzoffset64`, `gzclose_r`,
//!   `gzclose_w`.
//! * `ZLIB_1.2.5.1`: `deflatePending`.
//! * `ZLIB_1.2.5.2`: `deflateResetKeep`, `gzgetc_`, `inflateResetKeep`.
//! * `ZLIB_1.2.7.1`: `inflateGetDictionary`, `gzvprintf`.
//! * `ZLIB_1.2.9`: `inflateCodesUsed`, `inflateValidate`, `uncompress2`,
//!   `gzfread`, `gzfwrite`, `deflateGetDictionary`, `adler32_z`, `crc32_z`.
//! * `ZLIB_1.2.12`: `crc32_combine_gen`, `crc32_combine_gen64`,
//!   `crc32_combine_op`.
//! * `ZLIB_1.3.1.2`: `deflateUsed`.
//! * `ZLIB_1.3.2`: `compressBound_z`, `deflateBound_z`, `compress_z`,
//!   `compress2_z`, `uncompress_z`, `uncompress2_z`.
//!
//! **`local:` symbols — never exported** (internal to the C sources; the Rust
//! ports keep these private and deliberately do **not** apply
//! `#[unsafe(no_mangle)]` to their equivalents): `deflate_copyright`,
//! `inflate_copyright`, `inflate_fast`, `inflate_table`, `zcalloc`, `zcfree`,
//! `z_errmsg`, `gz_error`, `gz_intmax`, `inflate_fixed`, plus the catch-all
//! `_*` pattern that hides any remaining underscore-prefixed symbols.

// ===========================================================================
// Submodule declarations
// ===========================================================================
//
// `types` is the foundational module every shim builds on; `util`, `deflate`,
// and `inflate` are always compiled so the core zlib symbol table is always
// present for linkage. `gz` is feature-gated (see the module-level docs above).

pub mod deflate;
pub mod inflate;
pub mod types;
pub mod util;

// The caller-allocator (`zalloc`/`zfree`) buffer bridge. This is internal
// plumbing (no `extern "C"` symbols), not part of the public C surface, so it is
// `pub(crate)` rather than `pub` and is intentionally NOT glob-re-exported below.
// It is the sanctioned home of the raw allocator-hook `unsafe`, which the safe
// core (`src/stream.rs`) delegates to via `AllocHook::try_alloc_zeroed` (M6).
pub(crate) mod alloc;

#[cfg(feature = "gz-io")]
pub mod gz;

// ===========================================================================
// Re-exports — centralized Rust-visible symbol surface
// ===========================================================================
//
// These globs surface the shim functions, the `#[repr(C)]` mirrors, and the C
// type aliases under a single `zlib_rs::ffi::*` path for idiomatic Rust
// consumers and for documentation. They do NOT affect `#[unsafe(no_mangle)]`
// symbol emission (which happens from each defining submodule regardless).
//
// The re-exported surfaces are disjoint by construction — `types` exports the
// `#[repr(C)]` structs, scalar/pointer aliases, and conversion helpers, while
// each shim submodule exports only uniquely named `extern "C"` functions — so
// no glob collision or ambiguous re-export arises. (The `deflate`/`inflate`
// module names live in the type namespace while the identically named `deflate`
// / `inflate` shim functions live in the value namespace, so both coexist.)

pub use deflate::*;
pub use inflate::*;
pub use types::*;
pub use util::*;

#[cfg(feature = "gz-io")]
pub use gz::*;

// ===========================================================================
// Compile-time symbol-presence guards
// ===========================================================================

#[cfg(test)]
mod tests {
    //! Lightweight signature guards.
    //!
    //! Each guard binds the address of a representative `extern "C"` shim to a
    //! function pointer of the exact expected C signature. These are pure
    //! type-checks — no shim is ever invoked — so the module has no runtime
    //! effect but fails to compile if a sibling shim's signature ever drifts,
    //! catching an ABI regression at build time. Coercing a function *item* to a
    //! function *pointer* does not require an `unsafe` block, so this module
    //! upholds the "no `unsafe` in `mod.rs`" rule.

    #[test]
    fn core_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uInt, uLong, uLongf, z_streamp};
        use core::ffi::c_int;

        // Compression / decompression drivers.
        let _deflate: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::deflate::deflate;
        let _inflate: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::inflate::inflate;

        // One-call helpers and the pre-allocation bound.
        let _compress: unsafe extern "C" fn(*mut Bytef, *mut uLongf, *const Bytef, uLong) -> c_int =
            crate::ffi::util::compress;
        let _compress_bound: unsafe extern "C" fn(uLong) -> uLong = crate::ffi::util::compressBound;

        // Checksums.
        let _adler32: unsafe extern "C" fn(uLong, *const Bytef, uInt) -> uLong =
            crate::ffi::util::adler32;
        let _crc32: unsafe extern "C" fn(uLong, *const Bytef, uInt) -> uLong =
            crate::ffi::util::crc32;

        // Bind each to a shared reference so the guards are observed (and thus
        // fully type-checked) without triggering `unused_variables`.
        let _ = (
            _deflate,
            _inflate,
            _compress,
            _compress_bound,
            _adler32,
            _crc32,
        );
    }

    #[cfg(feature = "gz-io")]
    #[test]
    fn gz_symbols_have_expected_c_signatures() {
        use crate::ffi::types::gzFile;
        use core::ffi::c_char;

        let _gzopen: unsafe extern "C" fn(*const c_char, *const c_char) -> gzFile =
            crate::ffi::gz::gzopen;
        let _ = _gzopen;
    }

    // `gzprintf`/`gzvprintf` are exported as error-returning stubs with these
    // fixed signatures. This guard fails to compile if either symbol is dropped
    // or its fixed leading parameters drift, catching the "missing
    // gzprintf/gzvprintf" regression at build time.
    #[cfg(feature = "gz-io")]
    #[test]
    fn default_gzprintf_stub_symbols_are_present() {
        use crate::ffi::types::gzFile;
        use core::ffi::{c_char, c_int, c_void};

        let _gzprintf: extern "C" fn(gzFile, *const c_char) -> c_int = crate::ffi::gz::gzprintf;
        let _gzvprintf: extern "C" fn(gzFile, *const c_char, *mut c_void) -> c_int =
            crate::ffi::gz::gzvprintf;
        let _ = (_gzprintf, _gzvprintf);
    }

    #[test]
    fn deflate_init_and_lifecycle_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_char, c_int};

        // Versioned initializers — what the `zlib.h` `deflateInit`/`deflateInit2`
        // macros expand to.
        let _deflate_init2_: unsafe extern "C" fn(
            z_streamp,
            c_int,
            c_int,
            c_int,
            c_int,
            c_int,
            *const c_char,
            c_int,
        ) -> c_int = crate::ffi::deflate::deflateInit2_;
        let _deflate_init_: unsafe extern "C" fn(z_streamp, c_int, *const c_char, c_int) -> c_int =
            crate::ffi::deflate::deflateInit_;

        // Lifecycle: teardown, the two resets, and the deep copy.
        let _deflate_end: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::deflate::deflateEnd;
        let _deflate_reset: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::deflate::deflateReset;
        let _deflate_reset_keep: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::deflate::deflateResetKeep;
        let _deflate_copy: unsafe extern "C" fn(z_streamp, z_streamp) -> c_int =
            crate::ffi::deflate::deflateCopy;

        let _ = (
            _deflate_init2_,
            _deflate_init_,
            _deflate_end,
            _deflate_reset,
            _deflate_reset_keep,
            _deflate_copy,
        );
    }

    #[test]
    fn deflate_tuning_and_bound_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{uLong, z_size_t, z_streamp};
        use core::ffi::{c_int, c_uint};

        // Level / strategy re-dispatch and the match-finder override.
        let _deflate_params: unsafe extern "C" fn(z_streamp, c_int, c_int) -> c_int =
            crate::ffi::deflate::deflateParams;
        let _deflate_tune: unsafe extern "C" fn(z_streamp, c_int, c_int, c_int, c_int) -> c_int =
            crate::ffi::deflate::deflateTune;
        let _deflate_prime: unsafe extern "C" fn(z_streamp, c_int, c_int) -> c_int =
            crate::ffi::deflate::deflatePrime;

        // Pending-output introspection.
        let _deflate_pending: unsafe extern "C" fn(z_streamp, *mut c_uint, *mut c_int) -> c_int =
            crate::ffi::deflate::deflatePending;
        let _deflate_used: unsafe extern "C" fn(z_streamp, *mut c_int) -> c_int =
            crate::ffi::deflate::deflateUsed;

        // Pre-allocation bounds — the `uLong` entry point and the motley
        // `size_t` variant.
        let _deflate_bound: unsafe extern "C" fn(z_streamp, uLong) -> uLong =
            crate::ffi::deflate::deflateBound;
        let _deflate_bound_z: unsafe extern "C" fn(z_streamp, z_size_t) -> z_size_t =
            crate::ffi::deflate::deflateBound_z;

        let _ = (
            _deflate_params,
            _deflate_tune,
            _deflate_prime,
            _deflate_pending,
            _deflate_used,
            _deflate_bound,
            _deflate_bound_z,
        );
    }

    #[test]
    fn deflate_dictionary_and_header_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, gz_headerp, uInt, z_streamp};
        use core::ffi::c_int;

        // Preset dictionaries and the gzip header setter.
        let _deflate_set_dictionary: unsafe extern "C" fn(z_streamp, *const Bytef, uInt) -> c_int =
            crate::ffi::deflate::deflateSetDictionary;
        let _deflate_get_dictionary: unsafe extern "C" fn(
            z_streamp,
            *mut Bytef,
            *mut uInt,
        ) -> c_int = crate::ffi::deflate::deflateGetDictionary;
        let _deflate_set_header: unsafe extern "C" fn(z_streamp, gz_headerp) -> c_int =
            crate::ffi::deflate::deflateSetHeader;

        let _ = (
            _deflate_set_dictionary,
            _deflate_get_dictionary,
            _deflate_set_header,
        );
    }

    #[test]
    fn inflate_init_and_lifecycle_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_char, c_int};

        // Versioned initializers — what the `zlib.h` `inflateInit`/`inflateInit2`
        // macros expand to.
        let _inflate_init2_: unsafe extern "C" fn(z_streamp, c_int, *const c_char, c_int) -> c_int =
            crate::ffi::inflate::inflateInit2_;
        let _inflate_init_: unsafe extern "C" fn(z_streamp, *const c_char, c_int) -> c_int =
            crate::ffi::inflate::inflateInit_;

        // Lifecycle: teardown, the three resets, and the deep copy.
        let _inflate_end: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateEnd;
        let _inflate_reset: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateReset;
        let _inflate_reset2: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::inflate::inflateReset2;
        let _inflate_reset_keep: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateResetKeep;
        let _inflate_copy: unsafe extern "C" fn(z_streamp, z_streamp) -> c_int =
            crate::ffi::inflate::inflateCopy;

        let _ = (
            _inflate_init2_,
            _inflate_init_,
            _inflate_end,
            _inflate_reset,
            _inflate_reset2,
            _inflate_reset_keep,
            _inflate_copy,
        );
    }

    #[test]
    fn inflate_dictionary_sync_and_prime_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uInt, z_streamp};
        use core::ffi::c_int;

        // Preset dictionaries.
        let _inflate_set_dictionary: unsafe extern "C" fn(z_streamp, *const Bytef, uInt) -> c_int =
            crate::ffi::inflate::inflateSetDictionary;
        let _inflate_get_dictionary: unsafe extern "C" fn(
            z_streamp,
            *mut Bytef,
            *mut uInt,
        ) -> c_int = crate::ffi::inflate::inflateGetDictionary;

        // Resynchronization and bit-level priming.
        let _inflate_sync: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateSync;
        let _inflate_sync_point: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateSyncPoint;
        let _inflate_prime: unsafe extern "C" fn(z_streamp, c_int, c_int) -> c_int =
            crate::ffi::inflate::inflatePrime;

        let _ = (
            _inflate_set_dictionary,
            _inflate_get_dictionary,
            _inflate_sync,
            _inflate_sync_point,
            _inflate_prime,
        );
    }

    #[test]
    fn inflate_introspection_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_int, c_long, c_ulong};

        // Decoder introspection. Note the non-`c_int` returns: `inflateMark`
        // yields `c_long` and `inflateCodesUsed` yields `c_ulong`.
        let _inflate_mark: unsafe extern "C" fn(z_streamp) -> c_long =
            crate::ffi::inflate::inflateMark;
        let _inflate_validate: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::inflate::inflateValidate;
        let _inflate_undermine: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::inflate::inflateUndermine;
        let _inflate_codes_used: unsafe extern "C" fn(z_streamp) -> c_ulong =
            crate::ffi::inflate::inflateCodesUsed;

        let _ = (
            _inflate_mark,
            _inflate_validate,
            _inflate_undermine,
            _inflate_codes_used,
        );
    }

    // `inflateGetHeader` has two mutually exclusive definitions — one under
    // `#[cfg(feature = "gzip")]` and one under `#[cfg(not(feature = "gzip"))]` —
    // with identical signatures, so exactly one symbol is exported in every
    // feature configuration. This guard is therefore deliberately UNGATED: it
    // proves the signature is stable across BOTH arms, which a gated guard could
    // not do.
    #[test]
    fn inflate_get_header_symbol_has_expected_c_signature() {
        use crate::ffi::types::{gz_headerp, z_streamp};
        use core::ffi::c_int;

        let _inflate_get_header: unsafe extern "C" fn(z_streamp, gz_headerp) -> c_int =
            crate::ffi::inflate::inflateGetHeader;

        let _ = _inflate_get_header;
    }

    #[test]
    fn inflate_back_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{in_func, out_func, z_streamp};
        use core::ffi::{c_char, c_int, c_uchar, c_void};

        // The callback-driven decoder: `in_func`/`out_func` are the C
        // function-pointer typedefs, each paired with an opaque descriptor.
        let _inflate_back_init_: unsafe extern "C" fn(
            z_streamp,
            c_int,
            *mut c_uchar,
            *const c_char,
            c_int,
        ) -> c_int = crate::ffi::inflate::inflateBackInit_;
        let _inflate_back: unsafe extern "C" fn(
            z_streamp,
            in_func,
            *mut c_void,
            out_func,
            *mut c_void,
        ) -> c_int = crate::ffi::inflate::inflateBack;
        let _inflate_back_end: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateBackEnd;

        let _ = (_inflate_back_init_, _inflate_back, _inflate_back_end);
    }

    #[test]
    fn one_call_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uLong, uLongf, z_size_t};
        use core::ffi::c_int;

        // Compression helpers: the `uLong` entry points and their motley
        // `size_t` twins.
        let _compress2: unsafe extern "C" fn(
            *mut Bytef,
            *mut uLongf,
            *const Bytef,
            uLong,
            c_int,
        ) -> c_int = crate::ffi::util::compress2;
        let _compress2_z: unsafe extern "C" fn(
            *mut Bytef,
            *mut z_size_t,
            *const Bytef,
            z_size_t,
            c_int,
        ) -> c_int = crate::ffi::util::compress2_z;
        let _compress_z: unsafe extern "C" fn(
            *mut Bytef,
            *mut z_size_t,
            *const Bytef,
            z_size_t,
        ) -> c_int = crate::ffi::util::compress_z;
        let _compress_bound_z: unsafe extern "C" fn(z_size_t) -> z_size_t =
            crate::ffi::util::compressBound_z;

        // Decompression helpers. `uncompress2` takes `source_len` by pointer so
        // it can write back the number of bytes actually consumed.
        let _uncompress: unsafe extern "C" fn(
            *mut Bytef,
            *mut uLongf,
            *const Bytef,
            uLong,
        ) -> c_int = crate::ffi::util::uncompress;
        let _uncompress_z: unsafe extern "C" fn(
            *mut Bytef,
            *mut z_size_t,
            *const Bytef,
            z_size_t,
        ) -> c_int = crate::ffi::util::uncompress_z;
        let _uncompress2: unsafe extern "C" fn(
            *mut Bytef,
            *mut uLongf,
            *const Bytef,
            *mut uLong,
        ) -> c_int = crate::ffi::util::uncompress2;
        let _uncompress2_z: unsafe extern "C" fn(
            *mut Bytef,
            *mut z_size_t,
            *const Bytef,
            *mut z_size_t,
        ) -> c_int = crate::ffi::util::uncompress2_z;

        let _ = (
            _compress2,
            _compress2_z,
            _compress_z,
            _compress_bound_z,
            _uncompress,
            _uncompress_z,
            _uncompress2,
            _uncompress2_z,
        );
    }

    #[test]
    fn checksum_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uLong, z_crc_t, z_off_t, z_off64_t, z_size_t};

        // Adler-32: the `size_t` variant and the two combine widths.
        let _adler32_z: unsafe extern "C" fn(uLong, *const Bytef, z_size_t) -> uLong =
            crate::ffi::util::adler32_z;
        let _adler32_combine: unsafe extern "C" fn(uLong, uLong, z_off_t) -> uLong =
            crate::ffi::util::adler32_combine;
        let _adler32_combine64: unsafe extern "C" fn(uLong, uLong, z_off64_t) -> uLong =
            crate::ffi::util::adler32_combine64;

        // CRC-32: the `size_t` variant, the two combine widths, the operator
        // generators, and the operator application.
        let _crc32_z: unsafe extern "C" fn(uLong, *const Bytef, z_size_t) -> uLong =
            crate::ffi::util::crc32_z;
        let _crc32_combine: unsafe extern "C" fn(uLong, uLong, z_off_t) -> uLong =
            crate::ffi::util::crc32_combine;
        let _crc32_combine64: unsafe extern "C" fn(uLong, uLong, z_off64_t) -> uLong =
            crate::ffi::util::crc32_combine64;
        let _crc32_combine_gen: unsafe extern "C" fn(z_off_t) -> uLong =
            crate::ffi::util::crc32_combine_gen;
        let _crc32_combine_gen64: unsafe extern "C" fn(z_off64_t) -> uLong =
            crate::ffi::util::crc32_combine_gen64;
        let _crc32_combine_op: unsafe extern "C" fn(uLong, uLong, uLong) -> uLong =
            crate::ffi::util::crc32_combine_op;

        // The CRC table accessor.
        let _get_crc_table: unsafe extern "C" fn() -> *const z_crc_t =
            crate::ffi::util::get_crc_table;

        let _ = (
            _adler32_z,
            _adler32_combine,
            _adler32_combine64,
            _crc32_z,
            _crc32_combine,
            _crc32_combine64,
            _crc32_combine_gen,
            _crc32_combine_gen64,
            _crc32_combine_op,
            _get_crc_table,
        );
    }

    #[test]
    fn version_symbols_have_expected_c_signatures() {
        use crate::ffi::types::uLong;
        use core::ffi::{c_char, c_int};

        // Version identity, error-string mapping, and the compile-flags word.
        let _zlib_version: unsafe extern "C" fn() -> *const c_char = crate::ffi::util::zlibVersion;
        let _z_error: unsafe extern "C" fn(c_int) -> *const c_char = crate::ffi::util::zError;
        let _zlib_compile_flags: unsafe extern "C" fn() -> uLong =
            crate::ffi::util::zlibCompileFlags;

        let _ = (_zlib_version, _z_error, _zlib_compile_flags);
    }

    #[cfg(feature = "gz-io")]
    #[test]
    fn gz_open_and_config_symbols_have_expected_c_signatures() {
        use crate::ffi::types::gzFile;
        use core::ffi::{c_char, c_int, c_uint};

        // Openers. `gzdopen` has two mutually exclusive definitions (`unix` and
        // `not(unix)`) with identical signatures, so this binding is deliberately
        // NOT `unix`-gated: the symbol exists on every platform.
        let _gzopen64: unsafe extern "C" fn(*const c_char, *const c_char) -> gzFile =
            crate::ffi::gz::gzopen64;
        let _gzdopen: unsafe extern "C" fn(c_int, *const c_char) -> gzFile =
            crate::ffi::gz::gzdopen;

        // Buffer sizing and mid-stream parameter changes.
        let _gzbuffer: unsafe extern "C" fn(gzFile, c_uint) -> c_int = crate::ffi::gz::gzbuffer;
        let _gzsetparams: unsafe extern "C" fn(gzFile, c_int, c_int) -> c_int =
            crate::ffi::gz::gzsetparams;

        let _ = (_gzopen64, _gzdopen, _gzbuffer, _gzsetparams);
    }

    #[cfg(feature = "gz-io")]
    #[test]
    fn gz_read_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{gzFile, voidp, z_size_t};
        use core::ffi::{c_char, c_int, c_uint};

        // Bulk reads. `gzfread` returns `z_size_t`, not `c_int`.
        let _gzread: unsafe extern "C" fn(gzFile, voidp, c_uint) -> c_int = crate::ffi::gz::gzread;
        let _gzfread: unsafe extern "C" fn(voidp, z_size_t, z_size_t, gzFile) -> z_size_t =
            crate::ffi::gz::gzfread;
        let _gzgets: unsafe extern "C" fn(gzFile, *mut c_char, c_int) -> *mut c_char =
            crate::ffi::gz::gzgets;

        // `gzgetc` and `gzgetc_` are two distinct exported symbols: `zlib.h`
        // defines `gzgetc` as a macro over the `gzFile_s` prefix and keeps
        // `gzgetc_` as the callable backward-compatibility function.
        let _gzgetc: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzgetc;
        let _gzgetc_: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzgetc_;

        // `gzungetc` takes the character FIRST, then the file — matching C's
        // `int gzungetc(int c, gzFile file)`.
        let _gzungetc: unsafe extern "C" fn(c_int, gzFile) -> c_int = crate::ffi::gz::gzungetc;

        let _ = (_gzread, _gzfread, _gzgets, _gzgetc, _gzgetc_, _gzungetc);
    }

    #[cfg(feature = "gz-io")]
    #[test]
    fn gz_write_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{gzFile, voidpc, z_size_t};
        use core::ffi::{c_char, c_int, c_uint};

        // Bulk writes. `gzfwrite` returns `z_size_t`, not `c_int`.
        let _gzwrite: unsafe extern "C" fn(gzFile, voidpc, c_uint) -> c_int =
            crate::ffi::gz::gzwrite;
        let _gzfwrite: unsafe extern "C" fn(voidpc, z_size_t, z_size_t, gzFile) -> z_size_t =
            crate::ffi::gz::gzfwrite;

        // Single-character / string writes and the explicit flush.
        let _gzputc: unsafe extern "C" fn(gzFile, c_int) -> c_int = crate::ffi::gz::gzputc;
        let _gzputs: unsafe extern "C" fn(gzFile, *const c_char) -> c_int = crate::ffi::gz::gzputs;
        let _gzflush: unsafe extern "C" fn(gzFile, c_int) -> c_int = crate::ffi::gz::gzflush;

        let _ = (_gzwrite, _gzfwrite, _gzputc, _gzputs, _gzflush);
    }

    #[cfg(feature = "gz-io")]
    #[test]
    fn gz_seek_and_position_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{gzFile, z_off_t, z_off64_t};
        use core::ffi::c_int;

        // Seeking, in both the native and the explicit 64-bit offset widths.
        let _gzseek: unsafe extern "C" fn(gzFile, z_off_t, c_int) -> z_off_t =
            crate::ffi::gz::gzseek;
        let _gzseek64: unsafe extern "C" fn(gzFile, z_off64_t, c_int) -> z_off64_t =
            crate::ffi::gz::gzseek64;
        let _gzrewind: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzrewind;

        // Position reporting, likewise in both offset widths.
        let _gztell: unsafe extern "C" fn(gzFile) -> z_off_t = crate::ffi::gz::gztell;
        let _gztell64: unsafe extern "C" fn(gzFile) -> z_off64_t = crate::ffi::gz::gztell64;
        let _gzoffset: unsafe extern "C" fn(gzFile) -> z_off_t = crate::ffi::gz::gzoffset;
        let _gzoffset64: unsafe extern "C" fn(gzFile) -> z_off64_t = crate::ffi::gz::gzoffset64;

        // Stream-state predicates.
        let _gzeof: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzeof;
        let _gzdirect: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzdirect;

        let _ = (
            _gzseek,
            _gzseek64,
            _gzrewind,
            _gztell,
            _gztell64,
            _gzoffset,
            _gzoffset64,
            _gzeof,
            _gzdirect,
        );
    }

    #[cfg(feature = "gz-io")]
    #[test]
    fn gz_error_and_close_symbols_have_expected_c_signatures() {
        use crate::ffi::types::gzFile;
        use core::ffi::{c_char, c_int};

        // Error reporting. `gzclearerr` returns nothing, so its function-pointer
        // type carries no return clause at all.
        let _gzerror: unsafe extern "C" fn(gzFile, *mut c_int) -> *const c_char =
            crate::ffi::gz::gzerror;
        let _gzclearerr: unsafe extern "C" fn(gzFile) = crate::ffi::gz::gzclearerr;

        // The close dispatcher and the two direction-specific closers, which stay
        // mandatory because `GzState`'s `Drop` cannot surface a deferred I/O error.
        let _gzclose: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzclose;
        let _gzclose_r: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzclose_r;
        let _gzclose_w: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzclose_w;

        let _ = (_gzerror, _gzclearerr, _gzclose, _gzclose_r, _gzclose_w);
    }

    // `gzopen_w` takes a UTF-16 `wchar_t` path and exists on Windows only, so
    // this guard carries a `windows` predicate in addition to the `gz-io`
    // feature gate. C applies the same restriction to the prototype itself with
    // `#if defined(_WIN32) && !defined(Z_SOLO)`, which is why the symbol is
    // legitimately absent from the 95 exported on Linux.
    #[cfg(all(feature = "gz-io", windows))]
    #[test]
    fn gz_wide_open_symbol_has_expected_c_signature() {
        use crate::ffi::types::gzFile;
        use core::ffi::c_char;

        let _gzopen_w: unsafe extern "C" fn(*const u16, *const c_char) -> gzFile =
            crate::ffi::gz::gzopen_w;

        let _ = _gzopen_w;
    }
}
