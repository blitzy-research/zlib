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
//! Per the migration's unsafe-isolation strategy (AAP §0.6.2 and §0.7.2
//! standard S2), executable `unsafe` is confined to two places: this `ffi` tree,
//! and the private no-`std` runtime-support block in `src/lib.rs` (the
//! libc-backed global allocator and the abort panic handler, which only a true
//! non-test `no_std` build compiles). Nothing else in the crate needs it. The
//! compression, decompression, checksum, gzip-I/O, and one-call engines —
//! [`crate::deflate`], [`crate::inflate`], [`crate::checksum`], `crate::gz`, and
//! [`crate::util`] — contain the actual logic and are written in fully safe Rust
//! (the deflate engine has **zero** `unsafe`). Every `unsafe` operation in the sibling shim
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
//! | [`gz`](mod@gz) | `gzlib.c`, `gzread.c`, `gzwrite.c`, `gzclose.c`, `gzguts.h`  | `gz*` file-I/O shims (always linked; functional with `gz-io`) |
//!
//! ## Feature gating
//!
//! **Every one of the four shim submodules is compiled unconditionally, so the
//! emitted `cdylib`/`staticlib` presents the complete zlib C symbol table in
//! every Cargo feature configuration** (AAP §0.3.1, §0.8.1 D-4). A C consumer
//! links against one ABI, not against a per-feature subset: a missing
//! `gzbuffer` would be a link failure, which is a strictly worse outcome than a
//! symbol that resolves and reports an error.
//!
//! The `gz` submodule maps zlib's gzip file-I/O layer, which fundamentally
//! requires the standard library for filesystem access. Feature gating for it is
//! therefore applied **inside each function body**, never to the `mod`
//! declaration or to the `#[unsafe(no_mangle)]` items:
//!
//! * With `gz-io` enabled (implied by the default feature set, and itself
//!   implying `std` and `gzip`), every `gz*` shim delegates to `crate::gz` and
//!   behaves exactly as the C original.
//! * Without `gz-io`, every `gz*` shim still exists as an exported symbol with
//!   its exact C signature and returns that entry point's documented failure
//!   sentinel — `NULL` for the pointer-returning `gzopen`/`gzopen64`/`gzdopen`/
//!   `gzgets`/`gzerror` family, `-1` for `gzread`/`gzgetc`/`gzungetc`/`gzseek`/
//!   `gztell`/`gzoffset` and friends, `0` for `gzwrite`/`gzeof`/`gzdirect` and
//!   the `z_size_t`-returning `gzfread`/`gzfwrite`, `Z_STREAM_ERROR` for
//!   `gzsetparams`/`gzflush`/`gzclose`/`gzclose_r`/`gzclose_w`, and a no-op for
//!   the `void`-returning `gzclearerr`. This is the same shape as the
//!   `gzprintf`/`gzvprintf` concession below: the symbol resolves, and the
//!   failure is observable through the return value rather than at link time.
//!
//! Only the *helpers* that genuinely need `std` — the `GzHandle`/`GzBorrow`
//! ownership types, the boxing and close plumbing, the platform `close(2)`
//! binding and the path conversion — carry `#[cfg(feature = "gz-io")]`, so a
//! `--no-default-features` build compiles no unreachable machinery while still
//! emitting all 34 `gz*` symbols. Every core
//! (`deflate`/`inflate`/`checksum`/one-call/version) symbol is likewise always
//! present: [`types`], [`util`], [`deflate`](mod@deflate), and
//! [`inflate`](mod@inflate) have never been gated.
//!
//! The per-configuration proof lives in this module's own test suite: the
//! signature guards are un-gated (so they only compile if the symbol exists in
//! the row under test), `every_exported_c_symbol_resolves_to_a_live_address`
//! takes the address of all 96 exported entry points, and
//! `no_exported_gz_symbol_is_feature_gated` fails if a future edit re-applies a
//! `cfg` to a `#[unsafe(no_mangle)]` site.
//!
//! `gzprintf`/`gzvprintf` are always exported as
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
//! shims are exported *unversioned* by default, which satisfies static linking,
//! ordinary dynamic linking, `-lz` substitution, and `LD_PRELOAD` injection.
//! That default is the deliberate divergence recorded in AAP §0.8.2
//! (Divergence 4) and ranked **Low** as gap D8 in §0.10.1: the exported symbol
//! *set* is already exactly right and only the version tags are absent, so the
//! AAP defers applying them behind the cross-platform CI matrix (gap D3) and
//! directs that the divergence be kept rather than "fixed". The mapping below is
//! therefore recorded verbatim for two consumers: a downstream packaging step
//! that generates its own version script, and this crate's **own opt-in** —
//! `ZLIB_RS_VERSION_SCRIPT=1` makes `build.rs` derive a script from `zlib.map`
//! and apply it to the `cdylib` alone. See the "Optional cdylib symbol
//! versioning" section of `build.rs` for the mechanism, the target clauses it
//! requires, and its inert behaviour everywhere else.
//!
//! Both builds export the *same* symbol set and differ only in version
//! metadata. Measured on `x86_64-unknown-linux-gnu`, release `libzlib_rs.so`:
//!
//! | Inspection                          | Default | `ZLIB_RS_VERSION_SCRIPT=1` |
//! |-------------------------------------|--------:|---------------------------:|
//! | exported `T` symbols                |      95 |                         95 |
//! | `zlib.map` `global:` names present  |   54/54 |                      54/54 |
//! | `zlib.map` `local:` names leaked    |    0/10 |                       0/10 |
//! | symbols tagged `@@ZLIB_x.y.z`       |       0 |                         54 |
//! | `.gnu.version_d` definitions        |       0 |       17 (BASE + 16 nodes) |
//!
//! The default's only consumer-visible cost is cosmetic and confined to one
//! substitution form. Installing the unversioned artifact **as** `libz.so.1`,
//! for a program that was linked against a versioned distribution `libz`, makes
//! glibc's loader print `no version information available` once per distinct
//! `ZLIB_x.y.z` node that program requires — measured 0, 4, and 9 lines for
//! consumers requiring 0, 4, and 9 nodes — after which the program runs
//! correctly and every functional check passes, identically to the versioned
//! build. `LD_PRELOAD` injection prints no such line in either build, because
//! the versioned `libz.so.1` stays mapped to satisfy the version lookups while
//! the preloaded object takes resolution precedence. With the opt-in on, the
//! soname-substitution count is 0 and a consumer linked directly against the
//! artifact records the same `ZLIB_*` requirements it would record against a
//! distribution `libz`.
//!
//! **Unversioned base symbols** (predate the versioning scheme introduced in
//! zlib 1.2.0; they are *not* listed in any `zlib.map` node and are exported
//! without a version tag — the 41 names below are exactly the 41 untagged
//! exports measured in the opt-in build, so they stay unversioned-global there
//! too, which is how a distribution `libz.so.1` built from the same script
//! behaves): `deflate`, `inflate`, `deflateInit_`, `deflateInit2_`,
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
// `types` is the foundational module every shim builds on. All four shim
// modules — `util`, `deflate`, `inflate`, and `gz` — are compiled
// unconditionally so the emitted `cdylib`/`staticlib` presents the complete
// zlib C symbol table in every feature configuration (AAP §0.3.1, §0.8.1 D-4).
// `gz` applies its `gz-io` gating inside each function body, never to the `mod`
// declaration or to an exported item (see the module-level docs above).

pub mod deflate;
pub mod inflate;
pub mod types;
pub mod util;

// The caller-allocator (`zalloc`/`zfree`) buffer bridge. This is internal
// plumbing (no `extern "C"` symbols), not part of the public C surface, so it is
// `pub(crate)` rather than `pub` and is intentionally NOT glob-re-exported below.
// It is the sanctioned home of the raw allocator-hook `unsafe`, which the safe
// core (`src/stream.rs`) delegates to via `AllocHook::try_alloc_zeroed`.
pub(crate) mod alloc;

// The gzip file-I/O shims. Declared unconditionally: each of the 34 exported
// `gz*` entry points has exactly one definition whose *body* is gated on
// `gz-io`, so the symbol resolves in every configuration and returns its
// documented failure sentinel when the feature is off.
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
pub use gz::*;
pub use inflate::*;
pub use types::*;
pub use util::*;

// ===========================================================================
// Compile-time symbol-presence guards
// ===========================================================================

#[cfg(test)]
mod tests {
    //! Exhaustive signature guards.
    //!
    //! Each guard binds the address of an `extern "C"` shim to a function
    //! pointer of the exact expected C signature. These are pure type-checks —
    //! no shim is ever invoked — so the module has no runtime effect but fails
    //! to compile if a sibling shim's signature ever drifts, catching an ABI
    //! regression at build time. Coercing a function *item* to a function
    //! *pointer* does not require an `unsafe` block, so this module upholds the
    //! "no `unsafe` in `mod.rs`" rule.
    //!
    //! Coverage is **exhaustive, not representative**: every one of the 96
    //! distinct `#[unsafe(no_mangle)]` names the sibling shim modules export is
    //! bound below, which is a strict superset of the 54 `zlib.map` `global:`
    //! symbols transcribed in the module documentation above. A guard that
    //! covered only a sample would let an unguarded argument, return, callback,
    //! or calling-convention change compile and reach C callers while every
    //! existing guard still passed — the exact silent-drift failure this module
    //! exists to make impossible.
    //!
    //! Exhaustiveness is not left to inspection either. The three inventory
    //! tests at the end of this module re-derive the authoritative symbol sets
    //! at test time — the `global:`/`local:` partition from `zlib.map`, and the
    //! export list from the anchored `#[unsafe(no_mangle)]` attributes in the
    //! four sibling shim sources — and diff them against the set of names
    //! actually bound here. Adding an export without adding its guard therefore
    //! fails a test automatically, rather than waiting to be noticed.
    //!
    //! The guards are split by symbol family rather than gathered into one
    //! function so that no individual test trips `clippy::too_many_lines`; the
    //! project's quality gates run `-D warnings` and no lint may be silenced to
    //! make a change land.

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
    #[test]
    fn default_gzprintf_stub_symbols_are_present() {
        use crate::ffi::types::gzFile;
        use core::ffi::{c_char, c_int, c_void};

        let _gzprintf: extern "C" fn(gzFile, *const c_char) -> c_int = crate::ffi::gz::gzprintf;
        let _gzvprintf: extern "C" fn(gzFile, *const c_char, *mut c_void) -> c_int =
            crate::ffi::gz::gzvprintf;
        let _ = (_gzprintf, _gzvprintf);
    }

    // -- deflate family (`src/ffi/deflate.rs`, 17 exported names) ------------
    //
    // `deflate` itself is bound by `core_symbols_have_expected_c_signatures`
    // above; the three guards below cover the remaining sixteen.

    /// The `deflate*` lifecycle entry points: the two versioned initializers
    /// that `zlib.h`'s `deflateInit`/`deflateInit2` macros expand to, teardown,
    /// both reset flavors, and the deep-copy constructor.
    #[test]
    fn deflate_lifecycle_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_char, c_int};

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

    /// The `deflate*` tuning and bit-accounting entry points. `deflatePending`
    /// and `deflateUsed` write through caller pointers, so their pointer
    /// mutability and integer widths are part of the ABI.
    #[test]
    fn deflate_tuning_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_int, c_uint};

        let _deflate_params: unsafe extern "C" fn(z_streamp, c_int, c_int) -> c_int =
            crate::ffi::deflate::deflateParams;
        let _deflate_tune: unsafe extern "C" fn(z_streamp, c_int, c_int, c_int, c_int) -> c_int =
            crate::ffi::deflate::deflateTune;
        let _deflate_prime: unsafe extern "C" fn(z_streamp, c_int, c_int) -> c_int =
            crate::ffi::deflate::deflatePrime;
        let _deflate_pending: unsafe extern "C" fn(z_streamp, *mut c_uint, *mut c_int) -> c_int =
            crate::ffi::deflate::deflatePending;
        let _deflate_used: unsafe extern "C" fn(z_streamp, *mut c_int) -> c_int =
            crate::ffi::deflate::deflateUsed;

        let _ = (
            _deflate_params,
            _deflate_tune,
            _deflate_prime,
            _deflate_pending,
            _deflate_used,
        );
    }

    /// The `deflate*` output-bound, dictionary, and gzip-header entry points.
    /// `deflateBound` and `deflateBound_z` differ only in their integer width —
    /// `uLong` versus `z_size_t` — which is exactly the kind of drift a guard
    /// must catch, because the two compile interchangeably on LP64.
    #[test]
    fn deflate_bound_and_dictionary_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, gz_headerp, uInt, uLong, z_size_t, z_streamp};
        use core::ffi::c_int;

        let _deflate_bound: unsafe extern "C" fn(z_streamp, uLong) -> uLong =
            crate::ffi::deflate::deflateBound;
        let _deflate_bound_z: unsafe extern "C" fn(z_streamp, z_size_t) -> z_size_t =
            crate::ffi::deflate::deflateBound_z;
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
            _deflate_bound,
            _deflate_bound_z,
            _deflate_set_dictionary,
            _deflate_get_dictionary,
            _deflate_set_header,
        );
    }

    // -- inflate family (`src/ffi/inflate.rs`, 21 exported names) ------------
    //
    // `inflate` itself is bound by `core_symbols_have_expected_c_signatures`
    // above; the four guards below cover the remaining twenty.

    /// The `inflate*` lifecycle entry points: both versioned initializers, one
    /// teardown, the three reset flavors, and the deep-copy constructor whose
    /// soundness rests on the offset-based table references in
    /// `crate::inflate::state` rather than on C's interior pointers.
    #[test]
    fn inflate_lifecycle_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_char, c_int};

        let _inflate_init2_: unsafe extern "C" fn(z_streamp, c_int, *const c_char, c_int) -> c_int =
            crate::ffi::inflate::inflateInit2_;
        let _inflate_init_: unsafe extern "C" fn(z_streamp, *const c_char, c_int) -> c_int =
            crate::ffi::inflate::inflateInit_;
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

    /// The `inflate*` dictionary and gzip-header entry points.
    ///
    /// This guard is deliberately **ungated**. `inflateGetHeader` has two
    /// mutually exclusive definitions in `src/ffi/inflate.rs` — one under
    /// `#[cfg(feature = "gzip")]` and one under `#[cfg(not(feature = "gzip"))]`
    /// — so the *symbol* exists under every feature configuration even though
    /// each *definition* is conditional. An unconditional guard is therefore
    /// both correct and strictly stronger: it proves the signature is identical
    /// across both arms.
    #[test]
    fn inflate_dictionary_and_header_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, gz_headerp, uInt, z_streamp};
        use core::ffi::c_int;

        let _inflate_set_dictionary: unsafe extern "C" fn(z_streamp, *const Bytef, uInt) -> c_int =
            crate::ffi::inflate::inflateSetDictionary;
        let _inflate_get_dictionary: unsafe extern "C" fn(
            z_streamp,
            *mut Bytef,
            *mut uInt,
        ) -> c_int = crate::ffi::inflate::inflateGetDictionary;
        let _inflate_get_header: unsafe extern "C" fn(z_streamp, gz_headerp) -> c_int =
            crate::ffi::inflate::inflateGetHeader;

        let _ = (
            _inflate_set_dictionary,
            _inflate_get_dictionary,
            _inflate_get_header,
        );
    }

    /// The `inflate*` stream-control and introspection entry points. Note the
    /// three distinct return types a naive port would collapse into `c_int`:
    /// `inflateMark` returns `c_long` and `inflateCodesUsed` returns `c_ulong`.
    #[test]
    fn inflate_stream_control_symbols_have_expected_c_signatures() {
        use crate::ffi::types::z_streamp;
        use core::ffi::{c_int, c_long, c_ulong};

        let _inflate_sync: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateSync;
        let _inflate_sync_point: unsafe extern "C" fn(z_streamp) -> c_int =
            crate::ffi::inflate::inflateSyncPoint;
        let _inflate_prime: unsafe extern "C" fn(z_streamp, c_int, c_int) -> c_int =
            crate::ffi::inflate::inflatePrime;
        let _inflate_mark: unsafe extern "C" fn(z_streamp) -> c_long =
            crate::ffi::inflate::inflateMark;
        let _inflate_validate: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::inflate::inflateValidate;
        let _inflate_undermine: unsafe extern "C" fn(z_streamp, c_int) -> c_int =
            crate::ffi::inflate::inflateUndermine;
        let _inflate_codes_used: unsafe extern "C" fn(z_streamp) -> c_ulong =
            crate::ffi::inflate::inflateCodesUsed;

        let _ = (
            _inflate_sync,
            _inflate_sync_point,
            _inflate_prime,
            _inflate_mark,
            _inflate_validate,
            _inflate_undermine,
            _inflate_codes_used,
        );
    }

    /// The callback-driven `inflateBack*` decoder (`infback.c`). The two C
    /// callback typedefs are the highest-risk part of the whole ABI surface: a
    /// drift in `in_func`/`out_func` would have the decoder call a caller's
    /// function with the wrong argument list, which no amount of runtime symbol
    /// inspection can detect.
    #[test]
    fn inflate_back_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{in_func, out_func, z_streamp};
        use core::ffi::{c_char, c_int, c_uchar, c_void};

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

    // -- util family (`src/ffi/util.rs`, 25 exported names) ------------------
    //
    // `compress`, `compressBound`, `adler32`, and `crc32` are bound by
    // `core_symbols_have_expected_c_signatures` above; the five guards below
    // cover the remaining twenty-one, including all six motley `_z`
    // size_t-suffixed variants of node `ZLIB_1.3.2`.

    /// The one-call compression wrappers (`compress.c`). Each `uLong` entry
    /// point has a `_z` twin that takes `z_size_t` instead; the pair is only
    /// distinguishable by type, so binding both is the only way to prove neither
    /// silently adopted the other's width.
    #[test]
    fn one_call_compress_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uLong, uLongf, z_size_t};
        use core::ffi::c_int;

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

        let _ = (_compress2, _compress2_z, _compress_z, _compress_bound_z);
    }

    /// The one-call decompression wrappers (`uncompr.c`). `uncompress2` and
    /// `uncompress2_z` take `source_len` **by pointer** because they write the
    /// consumed length back; `uncompress`/`uncompress_z` take it by value.
    #[test]
    fn one_call_uncompress_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uLong, uLongf, z_size_t};
        use core::ffi::c_int;

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

        let _ = (_uncompress, _uncompress_z, _uncompress2, _uncompress2_z);
    }

    /// The Adler-32 entry points (`adler32.c`). The `_combine` pair differs only
    /// in its length parameter — `z_off_t` (C `long`) versus `z_off64_t` (always
    /// 64-bit) — which coincide on LP64 and diverge on 32-bit Windows.
    #[test]
    fn adler32_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uLong, z_off_t, z_off64_t, z_size_t};

        let _adler32_z: unsafe extern "C" fn(uLong, *const Bytef, z_size_t) -> uLong =
            crate::ffi::util::adler32_z;
        let _adler32_combine: unsafe extern "C" fn(uLong, uLong, z_off_t) -> uLong =
            crate::ffi::util::adler32_combine;
        let _adler32_combine64: unsafe extern "C" fn(uLong, uLong, z_off64_t) -> uLong =
            crate::ffi::util::adler32_combine64;

        let _ = (_adler32_z, _adler32_combine, _adler32_combine64);
    }

    /// The CRC-32 entry points (`crc32.c`), including the three
    /// `crc32_combine_*` operators of node `ZLIB_1.2.12` and the table accessor
    /// whose `*const z_crc_t` return type C callers index directly.
    #[test]
    fn crc32_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{Bytef, uLong, z_crc_t, z_off_t, z_off64_t, z_size_t};

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
        let _get_crc_table: unsafe extern "C" fn() -> *const z_crc_t =
            crate::ffi::util::get_crc_table;

        let _ = (
            _crc32_z,
            _crc32_combine,
            _crc32_combine64,
            _crc32_combine_gen,
            _crc32_combine_gen64,
            _crc32_combine_op,
            _get_crc_table,
        );
    }

    /// The version, error-string, and compile-flags entry points (`zutil.c`).
    /// `zlibCompileFlags` returns `uLong` per `zlib.h`, not the `u32` the safe
    /// layer computes — the widening happens inside the shim.
    #[test]
    fn version_symbols_have_expected_c_signatures() {
        use crate::ffi::types::uLong;
        use core::ffi::{c_char, c_int};

        let _zlib_version: unsafe extern "C" fn() -> *const c_char = crate::ffi::util::zlibVersion;
        let _z_error: unsafe extern "C" fn(c_int) -> *const c_char = crate::ffi::util::zError;
        let _zlib_compile_flags: unsafe extern "C" fn() -> uLong =
            crate::ffi::util::zlibCompileFlags;

        let _ = (_zlib_version, _z_error, _zlib_compile_flags);
    }

    // -- gz family (`src/ffi/gz.rs`, 33 exported names) ----------------------
    //
    // `gzopen`, `gzprintf`, and `gzvprintf` are bound by the two gz guards
    // above; the six guards below cover the remaining thirty. **None of them is
    // feature-gated**, and that is deliberate: `src/ffi/gz.rs` declares exactly
    // one definition per exported name in every configuration and gates only
    // the function *bodies* on `gz-io` (AAP §0.3.1, §0.8.1 D-4). Because these
    // guards coerce the function *items* to `unsafe extern "C"` fn-pointer
    // *types*, they only compile if the symbol genuinely exists — so leaving
    // them un-gated is what makes a `--no-default-features` build prove its own
    // symbol completeness rather than merely skipping the check.

    /// The `gz*` open and configuration entry points.
    ///
    /// `gzdopen` is deliberately **not** `unix`-gated here: it has two mutually
    /// exclusive definitions in `src/ffi/gz.rs` (`#[cfg(unix)]` and
    /// `#[cfg(not(unix))]`) with identical signatures, so the symbol exists on
    /// every platform and an unconditional binding proves both arms agree.
    #[test]
    fn gz_open_and_config_symbols_have_expected_c_signatures() {
        use crate::ffi::types::gzFile;
        use core::ffi::{c_char, c_int, c_uint};

        let _gzopen64: unsafe extern "C" fn(*const c_char, *const c_char) -> gzFile =
            crate::ffi::gz::gzopen64;
        let _gzdopen: unsafe extern "C" fn(c_int, *const c_char) -> gzFile =
            crate::ffi::gz::gzdopen;
        let _gzbuffer: unsafe extern "C" fn(gzFile, c_uint) -> c_int = crate::ffi::gz::gzbuffer;
        let _gzsetparams: unsafe extern "C" fn(gzFile, c_int, c_int) -> c_int =
            crate::ffi::gz::gzsetparams;

        let _ = (_gzopen64, _gzdopen, _gzbuffer, _gzsetparams);
    }

    /// The `gz*` read entry points.
    ///
    /// Three signatures here are individually easy to get wrong and impossible
    /// to detect at run time: `gzfread` takes its `gzFile` **last** (it mirrors
    /// C's `fread` argument order and returns `z_size_t`, not `c_int`);
    /// `gzungetc` takes the character **first**; and `gzgetc_` is a separate
    /// exported symbol from `gzgetc`, because `zlib.h` defines `gzgetc` as a
    /// macro and keeps `gzgetc_` for backward compatibility.
    #[test]
    fn gz_read_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{gzFile, voidp, z_size_t};
        use core::ffi::{c_char, c_int, c_uint};

        let _gzread: unsafe extern "C" fn(gzFile, voidp, c_uint) -> c_int = crate::ffi::gz::gzread;
        let _gzfread: unsafe extern "C" fn(voidp, z_size_t, z_size_t, gzFile) -> z_size_t =
            crate::ffi::gz::gzfread;
        let _gzgetc: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzgetc;
        let _gzgetc_: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzgetc_;
        let _gzgets: unsafe extern "C" fn(gzFile, *mut c_char, c_int) -> *mut c_char =
            crate::ffi::gz::gzgets;
        let _gzungetc: unsafe extern "C" fn(c_int, gzFile) -> c_int = crate::ffi::gz::gzungetc;

        let _ = (_gzread, _gzfread, _gzgetc, _gzgetc_, _gzgets, _gzungetc);
    }

    /// The `gz*` write entry points. `gzfwrite` mirrors C's `fwrite` argument
    /// order — buffer, size, nitems, file — and returns `z_size_t`.
    #[test]
    fn gz_write_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{gzFile, voidpc, z_size_t};
        use core::ffi::{c_char, c_int, c_uint};

        let _gzwrite: unsafe extern "C" fn(gzFile, voidpc, c_uint) -> c_int =
            crate::ffi::gz::gzwrite;
        let _gzfwrite: unsafe extern "C" fn(voidpc, z_size_t, z_size_t, gzFile) -> z_size_t =
            crate::ffi::gz::gzfwrite;
        let _gzputc: unsafe extern "C" fn(gzFile, c_int) -> c_int = crate::ffi::gz::gzputc;
        let _gzputs: unsafe extern "C" fn(gzFile, *const c_char) -> c_int = crate::ffi::gz::gzputs;
        let _gzflush: unsafe extern "C" fn(gzFile, c_int) -> c_int = crate::ffi::gz::gzflush;

        let _ = (_gzwrite, _gzfwrite, _gzputc, _gzputs, _gzflush);
    }

    /// The `gz*` seek and position entry points. Each 32-bit-offset entry point
    /// has a 64-bit twin, and the two differ only in `z_off_t` (C `long`) versus
    /// `z_off64_t` (always 64-bit) — indistinguishable on LP64, divergent on
    /// 32-bit Windows.
    #[test]
    fn gz_seek_and_position_symbols_have_expected_c_signatures() {
        use crate::ffi::types::{gzFile, z_off_t, z_off64_t};
        use core::ffi::c_int;

        let _gzseek: unsafe extern "C" fn(gzFile, z_off_t, c_int) -> z_off_t =
            crate::ffi::gz::gzseek;
        let _gzseek64: unsafe extern "C" fn(gzFile, z_off64_t, c_int) -> z_off64_t =
            crate::ffi::gz::gzseek64;
        let _gzrewind: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzrewind;
        let _gztell: unsafe extern "C" fn(gzFile) -> z_off_t = crate::ffi::gz::gztell;
        let _gztell64: unsafe extern "C" fn(gzFile) -> z_off64_t = crate::ffi::gz::gztell64;
        let _gzoffset: unsafe extern "C" fn(gzFile) -> z_off_t = crate::ffi::gz::gzoffset;
        let _gzoffset64: unsafe extern "C" fn(gzFile) -> z_off64_t = crate::ffi::gz::gzoffset64;

        let _ = (
            _gzseek,
            _gzseek64,
            _gzrewind,
            _gztell,
            _gztell64,
            _gzoffset,
            _gzoffset64,
        );
    }

    /// The `gz*` status, error, and close entry points. `gzclearerr` is the one
    /// exported symbol in the whole C surface that returns **nothing**, so its
    /// function-pointer type carries no `->` clause at all.
    #[test]
    fn gz_status_error_and_close_symbols_have_expected_c_signatures() {
        use crate::ffi::types::gzFile;
        use core::ffi::{c_char, c_int};

        let _gzeof: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzeof;
        let _gzdirect: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzdirect;
        let _gzerror: unsafe extern "C" fn(gzFile, *mut c_int) -> *const c_char =
            crate::ffi::gz::gzerror;
        let _gzclearerr: unsafe extern "C" fn(gzFile) = crate::ffi::gz::gzclearerr;
        let _gzclose: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzclose;
        let _gzclose_r: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzclose_r;
        let _gzclose_w: unsafe extern "C" fn(gzFile) -> c_int = crate::ffi::gz::gzclose_w;

        let _ = (
            _gzeof,
            _gzdirect,
            _gzerror,
            _gzclearerr,
            _gzclose,
            _gzclose_r,
            _gzclose_w,
        );
    }

    /// `gzopen_w`, the wide-character open. Unlike `gzdopen`, this symbol has
    /// **no** non-Windows counterpart — `zlib.h` declares it under
    /// `#if defined(_WIN32) && !defined(Z_SOLO)` and `src/ffi/gz.rs` mirrors that
    /// with `#[cfg(windows)]` — so the guard must carry the `windows` predicate
    /// too, or a non-Windows build fails to compile.
    #[cfg(windows)]
    #[test]
    fn gz_wide_open_symbol_has_expected_c_signature() {
        use crate::ffi::types::gzFile;
        use core::ffi::c_char;

        let _gzopen_w: unsafe extern "C" fn(*const u16, *const c_char) -> gzFile =
            crate::ffi::gz::gzopen_w;

        let _ = _gzopen_w;
    }

    // =======================================================================
    // Guarded-name inventory — makes an omitted guard fail automatically
    // =======================================================================
    //
    // The signature guards above are exhaustive, but exhaustiveness that depends
    // on someone remembering to extend a list decays on the first commit that
    // adds an export. The three tests below close that loop by re-deriving the
    // authoritative symbol sets from their sources at test time and diffing them
    // against the names actually bound above:
    //
    //   * `zlib.map` — the linker version script, authoritative for the
    //     `global:` (exported) / `local:` (hidden) partition.
    //   * the anchored `#[unsafe(no_mangle)]` attributes in the four sibling
    //     shim sources — authoritative for what the object file exports.
    //   * this file's own guard bodies — the set that is compile-time checked.
    //
    // Reading the sources at test time (rather than baking a list into a
    // constant) is what makes the check self-maintaining, and it mirrors the
    // established pattern in `src/lib.rs`, whose unsafe-boundary scanner walks
    // `src/**` the same way. All three tests are `#[cfg(test)]`-only: the crate
    // root applies `#![no_std]` only for a genuine freestanding build
    // (`not(test)` is part of its predicate), so `std::fs` is available here in
    // every feature configuration, including `--no-default-features`.
    //
    // Note on `zlib.map` availability: it is `REFERENCE`-only and is EXCLUDED
    // from the published crate, exactly like the retained C baseline — the
    // `package-verify` job in `.github/workflows/ci.yml` asserts that no `*.map`
    // is ever packaged, so its absence there is structural rather than
    // incidental. From the repository working tree `CARGO_MANIFEST_DIR` resolves
    // to the checkout root and the file is present, and the two tests that read
    // it assert in full. Inside an unpacked `.crate` it cannot be present, so
    // they print `skip_notice` and pass instead of panicking, which is what keeps
    // the published crate `cargo test`-able for a downstream consumer or distro
    // packager. That is the same degradation `tests/c_oracle.rs` performs for the
    // same reason, and the condition is deliberately narrow: only "the file does
    // not exist" skips (see `repo_file_if_present`), while a file that exists and
    // cannot be read, or reads but does not parse, still fails hard.

    /// The `#[unsafe(no_mangle)]`-bearing shim modules, and the number of
    /// attribute *sites* each one is expected to carry.
    ///
    /// `inflate.rs` declares 22 sites for 21 distinct names and `gz.rs` declares
    /// 34 for 33, because each file contains one pair of mutually exclusive
    /// `cfg` definitions of a single symbol (`inflateGetHeader` on the `gzip`
    /// feature, `gzdopen` on `unix`). 17 + 22 + 25 + 34 = 98 sites collapse to
    /// 96 distinct exported names.
    const SHIM_MODULES: [(&str, usize); 4] =
        [("deflate", 17), ("inflate", 22), ("util", 25), ("gz", 34)];

    /// Reads a file from the repository root, panicking with the path on error.
    ///
    /// Used for the four shim sources, which are `src/**` and therefore always
    /// present — in the working tree and inside a packaged `.crate` alike — so
    /// any failure to read them really is a defect.
    fn repo_file(relative: &str) -> std::string::String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display()))
    }

    /// Reads a `REFERENCE`-only file that is excluded from the published crate,
    /// yielding `None` **only** when it does not exist.
    ///
    /// The narrowness is the whole point, and it mirrors the skip contract of
    /// `tests/c_oracle.rs`: "not packaged" is a capability condition and is the
    /// single tolerated outcome, whereas a file that exists but cannot be read —
    /// a permission error, a directory in its place, an I/O failure — means the
    /// check *was* possible and something is wrong, so it still panics. Parsing
    /// is likewise never softened: a present-but-malformed script fails the
    /// caller's assertions as loudly as it always did.
    fn repo_file_if_present(relative: &str) -> Option<std::string::String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => panic!("{} exists but could not be read: {err}", path.display()),
        }
    }

    /// Prints why a `REFERENCE`-file-dependent check could not run.
    ///
    /// Emitted instead of panicking so that `cargo test` inside an unpacked
    /// `.crate` reports zero failures. `#[ignore]` is deliberately not used: the
    /// crate's own gate keeps the ignored-test count at zero (AAP §0.7.2 S10),
    /// and an ignored test would stay silently skipped in the repository too,
    /// where the check must and does run in full.
    fn skip_notice(relative: &str) {
        std::println!(
            "\nffi: SKIPPED — {relative} is not present in this build tree.\n\
             ffi: It is a REFERENCE-only artifact excluded from the published crate\n\
             ffi: (see [Cargo.toml:exclude]; the `package-verify` CI job asserts no\n\
             ffi: *.map is ever packaged), so this check is unrunnable here by design.\n\
             ffi: The compile-time signature guards themselves are unaffected — they\n\
             ffi: are ordinary code in this module and are checked by the compiler on\n\
             ffi: every build. The full 54-global / 10-local reconciliation runs from\n\
             ffi: the repository working tree, where the script is present.\n"
        );
    }

    /// Strips `//`-introduced comments (including `///` and `//!` doc comments)
    /// from every line, so a symbol name mentioned in prose can never be
    /// mistaken for code.
    ///
    /// Safe for these files: the only string literals they contain are short ABI
    /// and feature names (`"C"`, `"gz-io"`, …), none of which contains `//`.
    fn strip_line_comments(source: &str) -> std::string::String {
        source
            .lines()
            .map(|line| match line.find("//") {
                Some(at) => &line[..at],
                None => line,
            })
            .collect::<std::vec::Vec<_>>()
            .join("\n")
    }

    /// Splits `zlib.map` into its `global:` and `local:` symbol names.
    ///
    /// The script's shape is one `NODE { … } PARENT;` block per version. Within
    /// a block, symbols are `global` until a `local:` label appears, and each
    /// block starts over at `global` (only the first and last nodes carry an
    /// explicit `global:` label). The `_*` catch-all is a wildcard pattern, not a
    /// symbol, so it is skipped.
    ///
    /// Returns `None` only when the script is absent, i.e. inside an unpacked
    /// `.crate`; see `repo_file_if_present` for why nothing else is tolerated.
    fn zlib_map_partition() -> Option<(
        std::vec::Vec<std::string::String>,
        std::vec::Vec<std::string::String>,
    )> {
        let text = repo_file_if_present("zlib.map")?;
        let mut globals = std::vec::Vec::new();
        let mut locals = std::vec::Vec::new();
        let mut exported = true;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.ends_with('{') {
                exported = true;
                continue;
            }
            match line {
                "global:" => {
                    exported = true;
                    continue;
                }
                "local:" => {
                    exported = false;
                    continue;
                }
                _ => {}
            }
            if line.starts_with('}') {
                continue;
            }
            if let Some(name) = line.strip_suffix(';') {
                let name = name.trim();
                if name.is_empty() || name == "_*" {
                    continue;
                }
                if exported {
                    globals.push(name.to_string());
                } else {
                    locals.push(name.to_string());
                }
            }
        }

        Some((globals, locals))
    }

    /// Every C symbol the crate exports, derived from the anchored
    /// `#[unsafe(no_mangle)]` attributes in the four shim sources.
    ///
    /// Returns `(distinct names, total attribute sites)`. The attribute is
    /// always followed by the `pub [unsafe] extern "C" fn <name>(` line,
    /// possibly with further attributes in between, so the scan skips attribute
    /// lines and then reads the identifier after `fn`.
    fn exported_symbol_inventory() -> (std::vec::Vec<std::string::String>, usize) {
        let mut names: std::vec::Vec<std::string::String> = std::vec::Vec::new();
        let mut sites = 0usize;

        for (module, expected_sites) in SHIM_MODULES {
            let source = strip_line_comments(&repo_file(&std::format!("src/ffi/{module}.rs")));
            let lines: std::vec::Vec<&str> = source.lines().collect();
            let mut module_sites = 0usize;

            for (index, line) in lines.iter().enumerate() {
                if line.trim() != "#[unsafe(no_mangle)]" {
                    continue;
                }
                module_sites += 1;
                let name = lines[index + 1..]
                    .iter()
                    .map(|next| next.trim())
                    .find(|next| !next.starts_with("#["))
                    .and_then(|signature| signature.split_once(" fn "))
                    .map(|(_, rest)| {
                        rest.trim_start()
                            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or_default()
                            .to_string()
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "src/ffi/{module}.rs line {} carries #[unsafe(no_mangle)] but no \
                             `extern \"C\" fn <name>` follows it",
                            index + 1
                        )
                    });
                assert!(
                    !name.is_empty(),
                    "src/ffi/{module}.rs line {}: could not read the exported symbol name",
                    index + 1
                );
                if !names.contains(&name) {
                    names.push(name);
                }
            }

            assert_eq!(
                module_sites, expected_sites,
                "src/ffi/{module}.rs declares {module_sites} `#[unsafe(no_mangle)]` sites, \
                 expected {expected_sites}. If an export was added or removed, update \
                 SHIM_MODULES and add or remove the matching signature guard in this module."
            );
            sites += module_sites;
        }

        (names, sites)
    }

    /// Every symbol name bound by a signature guard in this module, read back
    /// out of this file's own source.
    ///
    /// A guard's right-hand side is always the fully qualified path
    /// `crate::ffi::<shim module>::<symbol>`, which is what makes the guarded set
    /// mechanically recoverable. `crate::ffi::types::…` import paths are ignored
    /// because `types` exports no C symbol.
    fn guarded_symbol_names() -> std::vec::Vec<std::string::String> {
        let source = strip_line_comments(&repo_file("src/ffi/mod.rs"));
        let mut names: std::vec::Vec<std::string::String> = std::vec::Vec::new();

        for (module, _) in SHIM_MODULES {
            let needle = std::format!("crate::ffi::{module}::");
            for (_, tail) in source
                .match_indices(&needle)
                .map(|(at, _)| (at, &source[at + needle.len()..]))
            {
                let name: std::string::String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() && !names.contains(&name) {
                    names.push(name);
                }
            }
        }

        names
    }

    /// Every `zlib.map` `global:` symbol must have an exact compile-time
    /// signature guard in this module.
    ///
    /// The 54 globals are the symbols a distribution `libz.so.1` version-tags,
    /// so they are the narrowest set a drop-in replacement must get exactly
    /// right (preservation directive D-4). This test proves each one is bound;
    /// the sibling test below proves the wider 96-name export surface is too.
    ///
    /// Skips — printing `skip_notice`, never `#[ignore]` — when `zlib.map` is
    /// absent, which happens only inside an unpacked `.crate`.
    #[test]
    fn every_zlib_map_global_symbol_has_a_compile_time_signature_guard() {
        let Some((globals, _)) = zlib_map_partition() else {
            skip_notice("zlib.map");
            return;
        };
        assert_eq!(
            globals.len(),
            54,
            "zlib.map must declare 54 `global:` symbols; found {}",
            globals.len()
        );

        let guarded = guarded_symbol_names();
        let missing: std::vec::Vec<&std::string::String> =
            globals.iter().filter(|g| !guarded.contains(g)).collect();
        assert!(
            missing.is_empty(),
            "{} of the 54 `zlib.map` global symbols have no signature guard in \
             src/ffi/mod.rs: {missing:?}",
            missing.len()
        );
    }

    /// Every exported C symbol — all 96, a strict superset of the 54 `zlib.map`
    /// globals — must have an exact compile-time signature guard, and every
    /// guard must name a symbol that is actually exported.
    ///
    /// Equality in both directions is what makes this self-maintaining: adding
    /// an `#[unsafe(no_mangle)]` shim without a guard fails here, and so does a
    /// guard whose path was mistyped or whose symbol was deleted.
    #[test]
    fn every_exported_c_symbol_has_a_compile_time_signature_guard() {
        let (exported, sites) = exported_symbol_inventory();
        assert_eq!(
            sites, 98,
            "expected 98 `#[unsafe(no_mangle)]` attribute sites across the four shim \
             modules; found {sites}"
        );
        assert_eq!(
            exported.len(),
            96,
            "expected 96 distinct exported symbol names (98 sites minus the two \
             mutually exclusive cfg pairs); found {}",
            exported.len()
        );

        let guarded = guarded_symbol_names();

        let unguarded: std::vec::Vec<&std::string::String> =
            exported.iter().filter(|e| !guarded.contains(e)).collect();
        assert!(
            unguarded.is_empty(),
            "{} exported C symbols have no signature guard in src/ffi/mod.rs: \
             {unguarded:?}. Every `#[unsafe(no_mangle)]` shim needs an exact \
             fn-pointer coercion here, or its signature can drift silently.",
            unguarded.len()
        );

        let unknown: std::vec::Vec<&std::string::String> =
            guarded.iter().filter(|g| !exported.contains(g)).collect();
        assert!(
            unknown.is_empty(),
            "src/ffi/mod.rs guards {} name(s) that no shim exports: {unknown:?}",
            unknown.len()
        );
    }

    /// None of the `zlib.map` `local:` symbols may be exported, and none may
    /// appear in a guard.
    ///
    /// These ten are internal to the C sources; the Rust ports keep their
    /// equivalents private and deliberately do not apply `#[unsafe(no_mangle)]`.
    /// `zcalloc`/`zcfree` in particular have no Rust symbol at all — the
    /// allocator bridge in `crate::ffi::alloc` carries no `#[unsafe(no_mangle)]`.
    ///
    /// Skips — printing `skip_notice`, never `#[ignore]` — when `zlib.map` is
    /// absent, which happens only inside an unpacked `.crate`.
    #[test]
    fn no_zlib_map_local_symbol_is_exported_or_guarded() {
        let Some((_, locals)) = zlib_map_partition() else {
            skip_notice("zlib.map");
            return;
        };
        assert_eq!(
            locals.len(),
            10,
            "zlib.map must declare 10 named `local:` symbols; found {}",
            locals.len()
        );

        let (exported, _) = exported_symbol_inventory();
        let leaked: std::vec::Vec<&std::string::String> =
            locals.iter().filter(|l| exported.contains(l)).collect();
        assert!(
            leaked.is_empty(),
            "{} `zlib.map` local symbol(s) are exported: {leaked:?}",
            leaked.len()
        );

        let guarded = guarded_symbol_names();
        let guarded_locals: std::vec::Vec<&std::string::String> =
            locals.iter().filter(|l| guarded.contains(l)).collect();
        assert!(
            guarded_locals.is_empty(),
            "{} `zlib.map` local symbol(s) appear in a signature guard: \
             {guarded_locals:?}",
            guarded_locals.len()
        );
    }

    /// The `zlib.map` skip may only fire in a tree where the retained C baseline
    /// is absent — i.e. inside an unpacked `.crate`.
    ///
    /// This is what stops the skip from silently swallowing coverage in the
    /// repository. `zlib.map` and the C sources are excluded from the published
    /// crate by the same `[Cargo.toml:exclude]` contract and are therefore present
    /// or absent together. Rather than walk the whole baseline, this check samples
    /// representative sentinels: if any sentinel is present, the script must be
    /// here too, and the two reconciliation tests above must have asserted in full
    /// rather than printed a notice.
    #[test]
    fn the_zlib_map_skip_can_only_happen_where_the_whole_c_baseline_is_absent() {
        // The sentinels — three of the retained C translation units plus the API
        // header. Each is matched by an `exclude` pattern (`*.c` / `*.h`), so
        // `cargo package` drops all of them together with `zlib.map` (`*.map`);
        // sampling is therefore enough to tell a repository tree from a packaged
        // one.
        const C_BASELINE: [&str; 4] = ["deflate.c", "inflate.c", "trees.c", "zlib.h"];

        let baseline_present: std::vec::Vec<&str> = C_BASELINE
            .into_iter()
            .filter(|relative| repo_file_if_present(relative).is_some())
            .collect();

        if baseline_present.is_empty() {
            skip_notice("the retained C baseline");
            return;
        }

        assert!(
            zlib_map_partition().is_some(),
            "the retained C baseline is present ({baseline_present:?}) but zlib.map is \
             not, so the two reconciliation tests skipped instead of asserting. \
             zlib.map is REFERENCE-only and must never be deleted from the working \
             tree: it is the authoritative global:/local: partition (preservation \
             directive D-7)."
        );
    }

    /// `repo_file_if_present` tolerates exactly one condition: the file does not
    /// exist.
    ///
    /// The positive half pins that a file which *is* there still reads, so the
    /// helper cannot degrade into "always `None`" and quietly disable both
    /// reconciliation tests.
    #[test]
    fn the_reference_file_skip_condition_is_narrow() {
        assert!(
            repo_file_if_present("zlib.map.this-path-is-deliberately-absent").is_none(),
            "a genuinely missing REFERENCE file must yield None"
        );
        assert!(
            repo_file_if_present("src/ffi/mod.rs")
                .is_some_and(|text| text.contains("fn repo_file_if_present")),
            "a present file must still be read in full"
        );
    }

    /// A path that exists but cannot be read as text is a defect, not a
    /// capability condition, so it must still panic.
    ///
    /// `src` is a directory: `read_to_string` fails on it with an error whose
    /// kind is never `NotFound` (`IsADirectory` on Linux, a permission or
    /// generic error elsewhere), which is precisely the class this asserts is
    /// still fatal.
    #[test]
    #[should_panic(expected = "exists but could not be read")]
    fn a_reference_path_that_exists_but_cannot_be_read_still_fails_hard() {
        let _ = repo_file_if_present("src");
    }

    /// The free-function surface `crate::ffi::types` publishes, pinned by name.
    ///
    /// `types.rs` is glob-re-exported by every shim module (`use
    /// crate::ffi::types::*;`), so anything `pub` there becomes part of the
    /// crate's public API the moment it is written. That is fine for the ABI
    /// mirrors and the conversion helpers a re-implementor genuinely needs, and
    /// wrong for init-sequence plumbing that mutates a caller's `z_stream` in
    /// place and has exactly one correct call site — inside the five versioned
    /// `*Init*_` shims. `init_allocator_prologue` is that plumbing and is
    /// `pub(crate)`; this test is what keeps it, and anything like it, from
    /// drifting back into the public surface unnoticed.
    ///
    /// Only *free functions* are inventoried (`^pub … fn`), because they are the
    /// items a glob import pulls into a consumer's namespace unqualified. The
    /// `#[repr(C)]` mirrors, type aliases and inherent methods are pinned by the
    /// ABI guards above and by the layout assertions in `crate::ffi::types`.
    #[test]
    fn ffi_types_publishes_exactly_the_intended_free_functions() {
        /// The sanctioned public free functions of `crate::ffi::types`.
        ///
        /// Adding a name here is a deliberate public-API decision; removing one
        /// is a breaking change. Neither may happen by accident.
        const EXPECTED: [&str; 19] = [
            "advance_input",
            "advance_output",
            "alloc_hook_from_parts",
            "deflate_state",
            "deflate_take",
            "gz_header_to_idiomatic",
            "input_ptr_valid",
            "input_slice",
            "output_slice",
            "peek_handle_kind",
            "set_adler",
            "set_data_type",
            "set_msg",
            "state_ptr_from_box",
            "state_ref",
            "state_take",
            "stream_buffers_valid",
            "write_gz_header_from_idiomatic",
            "zstream_with_caller_alloc",
        ];

        let source = strip_line_comments(&repo_file("src/ffi/types.rs"));
        let mut found: std::vec::Vec<std::string::String> = std::vec::Vec::new();

        for line in source.lines() {
            // Column 0 only: an indented `pub fn` is an inherent method, and a
            // `pub(crate) fn` is not part of the public surface.
            if !line.starts_with("pub ") {
                continue;
            }
            let Some(after_fn) = line.split(" fn ").nth(1) else {
                continue;
            };
            let name: std::string::String = after_fn
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                found.push(name);
            }
        }
        found.sort();
        found.dedup();

        let mut expected: std::vec::Vec<std::string::String> =
            EXPECTED.iter().map(|s| s.to_string()).collect();
        expected.sort();

        assert_eq!(
            found, expected,
            "the public free-function surface of src/ffi/types.rs changed. Added \
             names must be a deliberate public-API decision; init-sequence \
             plumbing such as `init_allocator_prologue` must stay `pub(crate)`."
        );
        assert!(
            !found.iter().any(|n| n == "init_allocator_prologue"),
            "`init_allocator_prologue` mutates a caller's z_stream and has one \
             correct call site; it must remain `pub(crate)`."
        );
    }

    /// Every `deflate*`/`inflate*` shim validates the stream **before** it
    /// touches any auxiliary caller pointer.
    ///
    /// Reference zlib establishes this order for every entry point that takes
    /// something alongside the stream: `deflateStateCheck(strm) || dictionary ==
    /// Z_NULL` is a single expression (`deflate.c` L602-L603), `inflateSync`
    /// opens with `inflateStateCheck` before it reaches `strm->next_in`
    /// (`inflate.c` L1349-L1351), `inflateGetHeader` writes `head->done` only
    /// after both of its guards pass (`inflate.c` L1219-L1230), and
    /// `deflate`/`inflate` put the state clause first in their entry test
    /// (`deflate.c` L981-L1010, `inflate.c` L474).
    ///
    /// In C, getting that order wrong reads a stale pointer. In Rust it is worse:
    /// `slice::from_raw_parts` over a stale pointer, or `&*head` on a dangling
    /// header, is undefined behavior *at the moment the reference is created*,
    /// even if it is never read and even though the shim is about to return
    /// `Z_STREAM_ERROR`. A behavioral test cannot catch that — the wrong order
    /// still returns the right code — so the ordering is asserted structurally,
    /// here, the same way the crate asserts its layer graph.
    ///
    /// Both files are scanned per exported shim. A shim that mentions any
    /// auxiliary-access marker must mention a state-validation marker *earlier* in
    /// its body. Comments are stripped first, so the prose above cannot satisfy
    /// its own assertion.
    #[test]
    fn state_validation_precedes_every_auxiliary_pointer_access() {
        /// Markers for "the installed engine state has now been validated".
        ///
        /// Every one of these runs the whole of the relevant C predicate and
        /// borrows nothing outside the `z_stream` and its own handle.
        const VALIDATED: [&str; 9] = [
            "deflate_state_check(",
            "deflate_state(",
            "deflate_take(",
            "inflate_state_check(",
            "inflate_handle(",
            "inflate_take(",
            "inflate_back_state_check(",
            "inflate_back_handle(",
            "inflate_back_take(",
        ];

        /// Markers for "a pointer that is not the `z_stream` itself is now being
        /// turned into a reference, a slice, or written through".
        const AUXILIARY: [&str; 13] = [
            "slice::from_raw_parts",
            "input_slice(",
            "output_slice(",
            "&*head",
            "&mut *head",
            "&*dest",
            "&mut *dest",
            "&*source",
            "&mut *source",
            "unsafe { *bits",
            "unsafe { *pending",
            "unsafe { *dict_length",
            "unsafe { *value",
        ];

        let mut inspected = 0usize;
        let mut with_auxiliary = 0usize;
        let mut violations: std::vec::Vec<std::string::String> = std::vec::Vec::new();

        for relative in ["src/ffi/deflate.rs", "src/ffi/inflate.rs"] {
            let source = strip_line_comments(&repo_file(relative));
            // Split on the exported-shim boundary. Everything before the first
            // `pub unsafe extern "C" fn` is module-level helper code, which is
            // covered by the shims that call it.
            let marker = "pub unsafe extern \"C\" fn ";
            let mut starts: std::vec::Vec<usize> = std::vec::Vec::new();
            let mut from = 0usize;
            while let Some(rel) = source[from..].find(marker) {
                let at = from + rel;
                // Column 0 only: a nested or indented occurrence is not a shim.
                if at == 0 || source.as_bytes()[at - 1] == b'\n' {
                    starts.push(at);
                }
                from = at + marker.len();
            }
            assert!(
                starts.len() >= 15,
                "{relative}: expected at least 15 exported shims, found {}",
                starts.len()
            );

            for (i, &start) in starts.iter().enumerate() {
                let end = starts.get(i + 1).copied().unwrap_or(source.len());
                let body = &source[start..end];
                let name: std::string::String = body[marker.len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                inspected += 1;

                let earliest_aux = AUXILIARY.iter().filter_map(|m| body.find(m)).min();
                let Some(aux_at) = earliest_aux else {
                    continue;
                };
                with_auxiliary += 1;

                let earliest_check = VALIDATED.iter().filter_map(|m| body.find(m)).min();
                match earliest_check {
                    Some(check_at) if check_at < aux_at => {}
                    _ => {
                        let offender = AUXILIARY
                            .iter()
                            .find(|m| body.find(*m) == Some(aux_at))
                            .copied()
                            .unwrap_or("<unknown>");
                        violations.push(std::format!(
                            "{relative}: `{name}` reaches `{offender}` before any \
                             state validation"
                        ));
                    }
                }
            }
        }

        // Anti-vacuity: the scan must actually have seen the shims and must
        // actually have classified a substantial number of them as taking an
        // auxiliary pointer. A refactor that renamed the markers out from under
        // this test would otherwise pass by finding nothing.
        assert!(
            inspected >= 35,
            "expected at least 35 exported deflate/inflate shims, inspected {inspected}"
        );
        assert!(
            with_auxiliary >= 12,
            "expected at least 12 shims to take an auxiliary pointer, found \
             {with_auxiliary} — the AUXILIARY markers have probably drifted"
        );
        assert!(
            violations.is_empty(),
            "state validation must precede every auxiliary pointer access \
             (AAP §0.6.2; C: deflate.c L538-L556, inflate.c L88-L97, infback.c \
             L208-L219):\n{}",
            violations.join("\n")
        );
    }

    /// No caller-owned gzip-header field is ever reached through a Rust reference.
    ///
    /// # What this pins
    ///
    /// A C `gz_header`'s `extra`, `name` and `comment` are three independent caller
    /// pointers, and `inflateGetHeader`'s contract (`zlib.h` L1076-L1085) places **no
    /// disjointness requirement** on them — nor between them and `strm->next_in`/
    /// `next_out`. Reference zlib is unbothered: its stores are plain indexed writes
    /// through each pointer (`inflate.c` L614-L621, L632-L637, L654-L659). Rust is
    /// not: two `&mut [u8]` over overlapping ranges are undefined behaviour *at the
    /// moment they are created*, before any bounds test runs, and so is a
    /// `&mut gz_header` spanning a struct that a payload buffer lives inside.
    ///
    /// The boundary therefore reaches every header field through raw
    /// pointer-plus-capacity descriptors (`CRawHeaderSink`) and raw field
    /// projections. That property is invisible to a behavioural test — the wrong
    /// code produces the right bytes and the right return code, and only a
    /// Miri-style aliasing model would object — so it is asserted structurally,
    /// exactly as the crate asserts its layer graph and its validation ordering.
    ///
    /// The scan is deliberately narrow: it covers the two header helpers, and it
    /// requires each to be found, so renaming one out from under the test fails
    /// rather than silently passing.
    #[test]
    #[cfg(feature = "gzip")]
    fn no_gz_header_field_is_reached_through_a_reference() {
        /// Constructs that would place a Rust reference over a caller-owned header
        /// field or over the struct that may contain one.
        const FORBIDDEN: [&str; 4] = [
            "slice::from_raw_parts_mut",
            "slice::from_raw_parts",
            "&mut *head",
            "&*head",
        ];

        let source = strip_line_comments(&repo_file("src/ffi/types.rs"));
        // Each helper's body runs from its `fn` keyword to the start of the next
        // top-level item, which in this file is always a column-0 `///`, `#[` or
        // `pub`/`fn`/`impl`/`struct` line. Bounding on the next column-0 `fn ` or
        // `pub ` is sufficient and keeps the parser trivial.
        for helper in [
            "pub(crate) unsafe fn borrow_gz_header_sink(",
            "pub(crate) unsafe fn publish_gz_header(",
            "pub(crate) unsafe fn read_gz_header_source(",
            "unsafe fn cstr_len(",
        ] {
            let at = source.find(helper).unwrap_or_else(|| {
                panic!(
                    "{helper} must exist in src/ffi/types.rs — if it was renamed, \
                     retarget this guard rather than deleting it"
                )
            });
            let rest = &source[at + helper.len()..];
            let end = rest
                .find("\npub ")
                .into_iter()
                .chain(rest.find("\nfn "))
                .chain(rest.find("\nimpl "))
                .min()
                .unwrap_or(rest.len());
            let body = &rest[..end];
            for marker in FORBIDDEN {
                assert!(
                    !body.contains(marker),
                    "{helper} reaches a caller-owned gzip-header field through \
                     `{marker}`; the C API allows `extra`/`name`/`comment`/`next_out` \
                     to overlap, so every access must be a raw, individually bounded \
                     one (AAP §0.6.2, standard S2)"
                );
            }
        }

        // Anti-vacuity: the raw descriptor type the helpers must use has to exist,
        // and its stores have to be raw writes.
        let sink = source
            .find("impl ForeignByteSink for CRawHeaderSink")
            .expect("the raw header-sink descriptor must implement ForeignByteSink");
        let sink_body = &source[sink..];
        assert!(
            sink_body.contains("self.ptr.add(index).write(byte)"),
            "CRawHeaderSink::store_byte must be a single raw write"
        );
        assert!(
            sink_body.contains("ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(offset), n)"),
            "CRawHeaderSink::store_bytes must be a single bounded raw copy"
        );

        // Anti-vacuity for the read direction: the fields must actually be reached
        // through `addr_of!` projections, not merely not-reached through a reference.
        let read = source
            .find("pub(crate) unsafe fn read_gz_header_source(")
            .expect("the raw header-source reader must exist");
        let read_body = &source[read..];
        for projection in [
            "ptr::addr_of!((*head).extra).read()",
            "ptr::addr_of!((*head).name).read()",
            "ptr::addr_of!((*head).comment).read()",
            "ptr::addr_of!((*head).extra_len).read()",
        ] {
            assert!(
                read_body.contains(projection),
                "read_gz_header_source must read each field with `{projection}`,                  which forms no reference over the caller's struct"
            );
        }

        // The load-bearing ordering. A shared borrow of a header payload is sound
        // only while no `&mut` covers the same bytes, and `zlib.h` L843-L847 lets a
        // caller point `head->extra`/`name`/`comment` straight into `next_out`. The
        // `deflate` shim therefore MUST read and stage the header before
        // `output_slice` bridges the output window. Nothing about the emitted bytes
        // reveals a violation of this — the wrong order still produces the right
        // output — so, like the aliasing rule above, it is asserted structurally.
        let deflate_src = strip_line_comments(&repo_file("src/ffi/deflate.rs"));
        let shim = deflate_src
            .find("\npub unsafe extern \"C\" fn deflate(")
            .expect("the deflate shim must exist");
        let after = &deflate_src[shim..];
        let shim_end = after[1..]
            .find("\npub unsafe extern \"C\" fn ")
            .map_or(after.len(), |i| i + 1);
        let shim_body = &after[..shim_end];
        let read_at = shim_body
            .find("read_gz_header_source(")
            .expect("the deflate shim must read the live header through the raw reader");
        let stage_at = shim_body
            .find("stage_into(")
            .expect("the deflate shim must stage an overlapping header");
        let out_at = shim_body
            .find("output_slice(")
            .expect("the deflate shim must bridge the output window");
        assert!(
            read_at < out_at && stage_at < out_at,
            "the deflate shim must read (offset {read_at}) and stage (offset \
             {stage_at}) the caller's gzip header before `output_slice` (offset \
             {out_at}) bridges the output window; reading afterwards places a shared \
             borrow over bytes a live `&mut [u8]` may cover, which `zlib.h` \
             L843-L847 explicitly permits a caller to arrange (AAP §0.6.2, \
             standard S2)"
        );
    }

    /// Every exported C symbol resolves to a live, distinct code address **in
    /// the configuration under test**.
    ///
    /// This is the per-feature-row artifact check. The signature guards above
    /// prove a symbol's *type* is right; they are also, by themselves, a
    /// sufficient existence proof only because they are un-gated — coercing
    /// `crate::ffi::gz::gzbuffer` to a fn pointer simply does not compile if
    /// that item was cfg'd away. This test makes the existence claim explicit
    /// and total: it names all 95 non-Windows exports at once, takes each
    /// address, and requires every one to be a real, non-null, unique function.
    ///
    /// Why that matters: the emitted `cdylib`/`staticlib` must present the
    /// complete zlib C symbol table in *every* Cargo feature configuration (AAP
    /// §0.3.1, §0.8.1 D-4). A C consumer links against one ABI, so a
    /// `--no-default-features` build that quietly dropped the 15 gzip
    /// `zlib.map` globals (`gzbuffer`, `gzclearerr`, `gzclose_r`, `gzclose_w`,
    /// `gzdirect`, `gzfread`, `gzfwrite`, `gzgetc_`, `gzoffset`, `gzoffset64`,
    /// `gzopen64`, `gzseek64`, `gztell64`, `gzungetc`, `gzvprintf`) would be an
    /// unlinkable artifact, not a smaller one. Because `cargo test` runs this
    /// module once per feature row, each row carries its own proof.
    ///
    /// Distinctness is asserted as well as non-nullness: two names collapsing to
    /// one address would mean a shim had been aliased to another (for example by
    /// a copy-paste that pointed `gzclose_r` at `gzclose_w`), which no signature
    /// guard can catch when the signatures happen to match. The one sanctioned
    /// exception is a pair of ABI *width twins* — `X` and its `X_z` or `X64`
    /// variant — whose bodies are identical on a target where the two width types
    /// coincide, and which an optimized build is therefore free to fold onto a
    /// single address; see the check itself for why that is ABI-neutral, and why
    /// the comparison is confined to the rows that actually compile the gated
    /// bodies rather than the shared `Z_STREAM_ERROR` stub.
    #[test]
    fn every_exported_c_symbol_resolves_to_a_live_address() {
        /// Builds `(name, address)` pairs from `module::symbol` paths.
        ///
        /// `stringify!` keeps the printed name and the resolved item in lockstep,
        /// so a table entry can never disagree with the symbol it measures.
        macro_rules! export_addresses {
            ($( $module:ident :: $symbol:ident ),* $(,)?) => {
                std::vec![ $( (
                    std::stringify!($symbol),
                    crate::ffi::$module::$symbol as *const ()
                ) ),* ]
            };
        }

        // `gzopen_w` exists only on Windows, exactly as `zlib.h` gates it with
        // `#if defined(_WIN32) && !defined(Z_SOLO)`. Building it as a separate
        // (possibly empty) tail keeps the main table immutable and needs no
        // `cfg` inside the macro invocation.
        let windows_only: std::vec::Vec<(&str, *const ())> = {
            #[cfg(windows)]
            {
                std::vec![("gzopen_w", crate::ffi::gz::gzopen_w as *const ())]
            }
            #[cfg(not(windows))]
            {
                std::vec::Vec::new()
            }
        };

        let table: std::vec::Vec<(&str, *const ())> = export_addresses![
            // deflate.rs — 17 names
            deflate::deflateInit2_,
            deflate::deflateInit_,
            deflate::deflate,
            deflate::deflateEnd,
            deflate::deflateReset,
            deflate::deflateResetKeep,
            deflate::deflateParams,
            deflate::deflateTune,
            deflate::deflateBound,
            deflate::deflateBound_z,
            deflate::deflatePending,
            deflate::deflateUsed,
            deflate::deflatePrime,
            deflate::deflateSetDictionary,
            deflate::deflateGetDictionary,
            deflate::deflateSetHeader,
            deflate::deflateCopy,
            // inflate.rs — 21 names
            inflate::inflateInit2_,
            inflate::inflateInit_,
            inflate::inflateBackInit_,
            inflate::inflate,
            inflate::inflateEnd,
            inflate::inflateReset,
            inflate::inflateReset2,
            inflate::inflateResetKeep,
            inflate::inflateSetDictionary,
            inflate::inflateGetDictionary,
            inflate::inflateSync,
            inflate::inflateSyncPoint,
            inflate::inflatePrime,
            inflate::inflateCopy,
            inflate::inflateMark,
            inflate::inflateValidate,
            inflate::inflateUndermine,
            inflate::inflateCodesUsed,
            inflate::inflateGetHeader,
            inflate::inflateBack,
            inflate::inflateBackEnd,
            // util.rs — 25 names
            util::compress2,
            util::compress2_z,
            util::compress,
            util::compress_z,
            util::compressBound,
            util::compressBound_z,
            util::uncompress2,
            util::uncompress2_z,
            util::uncompress,
            util::uncompress_z,
            util::adler32,
            util::adler32_z,
            util::adler32_combine,
            util::adler32_combine64,
            util::crc32,
            util::crc32_z,
            util::crc32_combine,
            util::crc32_combine64,
            util::crc32_combine_gen,
            util::crc32_combine_gen64,
            util::crc32_combine_op,
            util::get_crc_table,
            util::zlibVersion,
            util::zError,
            util::zlibCompileFlags,
            // gz.rs — 32 names
            gz::gzopen,
            gz::gzopen64,
            gz::gzdopen,
            gz::gzbuffer,
            gz::gzsetparams,
            gz::gzread,
            gz::gzfread,
            gz::gzgetc,
            gz::gzgetc_,
            gz::gzgets,
            gz::gzungetc,
            gz::gzwrite,
            gz::gzfwrite,
            gz::gzputc,
            gz::gzputs,
            gz::gzflush,
            gz::gzvprintf,
            gz::gzprintf,
            gz::gzseek,
            gz::gzseek64,
            gz::gzrewind,
            gz::gztell,
            gz::gztell64,
            gz::gzoffset,
            gz::gzoffset64,
            gz::gzeof,
            gz::gzdirect,
            gz::gzerror,
            gz::gzclearerr,
            gz::gzclose,
            gz::gzclose_r,
            gz::gzclose_w
        ]
        .into_iter()
        .chain(windows_only)
        .collect();

        let expected = if cfg!(windows) { 96 } else { 95 };
        assert_eq!(
            table.len(),
            expected,
            "the address table must cover every exported C symbol for this \
             target ({expected} expected)"
        );

        for (name, address) in &table {
            assert!(
                !address.is_null(),
                "exported symbol `{name}` resolved to a null address"
            );
        }

        // Distinctness, up to the identical-code folding an optimized build
        // legitimately performs.
        //
        // `[profile.release]` sets `codegen-units = 1`, so the whole crate is
        // optimized as a single unit and LLVM's function merging collapses two
        // exports whose machine code is byte-identical onto one address. That is
        // reachable only for the ABI *width twins* — the motley `_z` exports
        // (`compress_z`, `compress2_z`, `compressBound_z`, `deflateBound_z`,
        // `uncompress_z`, `uncompress2_z`, `adler32_z`, `crc32_z`) and the
        // large-file `*64` exports — because on an LP64 target `uLong`,
        // `z_size_t`, `z_off_t` and `z_off64_t` are all the same machine type,
        // which leaves a twin pair with identical bodies. A C zlib linked with
        // `--icf=all` folds exactly the same pairs, and nothing in the ABI is
        // weakened: each name still resolves and each is still callable through
        // its own declared signature, which the coercion guards above prove
        // independently. No zlib contract lets a caller compare function
        // addresses.
        //
        // Any OTHER pair sharing an address is the aliasing defect this check
        // exists to catch — a shim pointed at the wrong implementation, say
        // `gzclose_r` at `gzclose_w` — so only twins are tolerated, and every
        // collision is collected before reporting so one run names them all.
        //
        // The comparison is meaningful only where the bodies are genuinely
        // different, so it runs on the rows that compile them. `gzip` and `gz-io`
        // gate the function *bodies*, never the `#[unsafe(no_mangle)]` items (see
        // the `no exported gz* symbol may be feature-gated` test below), which is
        // what keeps the artifact's symbol table complete in every row — and it
        // means a row with those features off compiles the whole `gz*` family plus
        // `deflateSetHeader`/`inflateGetHeader` down to one shared
        // `Z_STREAM_ERROR` stub. Folding those together is the intended
        // consequence of that design, not aliasing, so address distinctness
        // carries no information there. Completeness, non-nullness and the
        // signature coercions above still run in every row, which is what makes
        // each row prove its own symbol surface.
        fn is_width_twin(a: &str, b: &str) -> bool {
            let folds_onto = |base: &str, twin: &str| {
                twin.strip_suffix("_z").is_some_and(|stem| stem == base)
                    || twin.strip_suffix("64").is_some_and(|stem| stem == base)
            };
            folds_onto(a, b) || folds_onto(b, a)
        }

        if cfg!(all(feature = "gzip", feature = "gz-io")) {
            let mut seen: std::vec::Vec<(*const (), &str)> = std::vec::Vec::new();
            let mut aliased: std::vec::Vec<(&str, &str)> = std::vec::Vec::new();
            for (name, address) in &table {
                if let Some((_, other)) = seen.iter().find(|(seen_at, _)| seen_at == address) {
                    if !is_width_twin(name, other) {
                        aliased.push((name, other));
                    }
                    continue;
                }
                seen.push((*address, name));
            }
            assert!(
                aliased.is_empty(),
                "exported symbols share one address without being ABI width twins, so a \
                 shim is aliased to another implementation: {aliased:?}"
            );
        }

        // The table is complete with respect to the source of truth: every
        // `#[unsafe(no_mangle)]` site in the four shim modules, minus the one
        // name this target legitimately does not build.
        let (mut declared, _) = exported_symbol_inventory();
        if !cfg!(windows) {
            declared.retain(|name| name != "gzopen_w");
        }
        declared.sort();

        let mut measured: std::vec::Vec<std::string::String> =
            table.iter().map(|(name, _)| name.to_string()).collect();
        measured.sort();

        assert_eq!(
            measured, declared,
            "the address table drifted from the `#[unsafe(no_mangle)]` inventory; \
             every declared export must be measured here so each feature row \
             proves its own symbol completeness"
        );
    }

    /// No exported `gz*` symbol may be feature-gated.
    ///
    /// `src/ffi/gz.rs` keeps exactly one definition per exported name and gates
    /// only the function *bodies* on `gz-io`, so the symbol table is identical in
    /// every feature row. Re-applying a `cfg` to a `#[unsafe(no_mangle)]` item —
    /// or restoring the module-level gate on `pub mod gz;` — would silently shrink
    /// the artifact back to 63 symbols and 39 of the 54 `zlib.map` globals. That
    /// regression is invisible to a default-features test run, so it is pinned
    /// here at the source level, where it is visible in every row.
    ///
    /// Platform gates are the one sanctioned exception: `gzdopen` has two
    /// mutually exclusive `unix` / `not(unix)` arms and `gzopen_w` is
    /// Windows-only, matching `zlib.h`. Both are allow-listed by predicate, so a
    /// feature predicate can never slip in under their cover.
    #[test]
    fn no_exported_gz_symbol_is_feature_gated() {
        let source = strip_line_comments(&repo_file("src/ffi/gz.rs"));
        let lines: std::vec::Vec<&str> = source.lines().collect();

        assert!(
            !source.contains("#![cfg("),
            "src/ffi/gz.rs must not carry a module-level `#![cfg(…)]`: the whole \
             file has to compile in every feature row so all 34 gz* symbols are \
             emitted"
        );

        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[unsafe(no_mangle)]" {
                continue;
            }

            // Attributes may sit on either side of `#[unsafe(no_mangle)]`.
            let mut attrs: std::vec::Vec<&str> = std::vec::Vec::new();
            let mut above = index;
            while above > 0 && lines[above - 1].trim().starts_with("#[") {
                above -= 1;
                attrs.push(lines[above].trim());
            }
            for below in &lines[index + 1..] {
                let below = below.trim();
                if !below.starts_with("#[") {
                    break;
                }
                attrs.push(below);
            }

            for attr in attrs {
                if !attr.contains("cfg") {
                    continue;
                }
                assert!(
                    !attr.contains("feature"),
                    "src/ffi/gz.rs line {}: an exported symbol carries the feature \
                     gate `{attr}`. Gate the function BODY instead so the symbol \
                     still links in a --no-default-features build.",
                    index + 1
                );
                assert!(
                    attr.contains("unix") || attr.contains("windows"),
                    "src/ffi/gz.rs line {}: unexpected gate `{attr}` on an exported \
                     symbol; only the platform gates zlib.h itself applies are \
                     allowed here.",
                    index + 1
                );
            }
        }

        // The `mod` declaration and the re-export must both be unconditional.
        let root = strip_line_comments(&repo_file("src/ffi/mod.rs"));
        for (index, line) in root.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed != "pub mod gz;" && trimmed != "pub use gz::*;" {
                continue;
            }
            let previous = root.lines().nth(index.wrapping_sub(1)).unwrap_or("").trim();
            assert!(
                !previous.contains("#[cfg("),
                "src/ffi/mod.rs: `{trimmed}` must be unconditional, but it is \
                 preceded by `{previous}`"
            );
        }
    }
}
