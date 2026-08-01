//! `#[repr(C)]` ABI mirror of zlib's public structs plus the raw ↔ idiomatic
//! conversion glue for the C drop-in boundary.
//!
//! This is the **foundational** module of the [`crate::ffi`] layer. Every other
//! FFI shim (`util.rs`, `deflate.rs`, `inflate.rs`, `gz.rs`) and the module root
//! (`mod.rs`) builds on the types, aliases, allocator bridge, and conversion
//! helpers defined here. The module deliberately contains **no**
//! `#[no_mangle] extern "C"` exported functions — those live in the sibling shim
//! files; here we provide only:
//!
//! * **Scalar type aliases** ([`Bytef`], [`uInt`], [`uLong`], …) mirroring
//!   `zconf.h`, so the shims can spell C types precisely.
//! * **C function-pointer typedefs** ([`alloc_func`], [`free_func`], [`in_func`],
//!   [`out_func`]) modelled as null-pointer-optimized `Option<unsafe extern "C"
//!   fn(…)>`.
//! * **`#[repr(C)]` mirror structs** — [`z_stream`] (`zlib.h` L90-L110),
//!   [`gz_header`] (`zlib.h` L118-L133), and [`gzFile_s`] (`zlib.h` L1956-L1960)
//!   — whose field order and widths match `zlib.h` **exactly** so the emitted
//!   `cdylib`/`staticlib` is byte-layout-compatible with `libz`.
//! * **The caller-allocator bridge** [`CAllocator`], which lets a C caller's
//!   `zalloc`/`zfree`/`opaque` triple ride inside the crate's own
//!   [`Allocator`] abstraction.
//! * **Raw ↔ idiomatic conversion helpers** that the deflate/inflate/util shims
//!   use to move between the raw [`z_stream`]/[`gz_header`] and the idiomatic
//!   [`ZStream`]/[`GzHeader`].
//! * **Panic guards** (`guard_int`, …) so a shim body can never unwind across
//!   the C boundary.
//!
//! # Unsafe boundary
//!
//! `src/ffi/**` is the crate's designated `unsafe` boundary (AAP §0.6.2 /
//! §0.7.2 standard S2): the idiomatic core (`src/deflate/**`, `src/stream.rs`, …) is fully
//! safe, and every `unsafe` operation here carries a `// SAFETY:` justification,
//! while every public `unsafe fn` documents its contract in a `# Safety`
//! section.
//!
//! # C integer widths
//!
//! The C integer aliases are taken from [`core::ffi`] (`c_int`, `c_uint`,
//! `c_ulong`, …) rather than `std::os::raw`. The two are the *same* platform-
//! accurate types — `std::os::raw` merely re-exports `core::ffi` — but
//! [`core::ffi`] keeps this module `no_std`-clean. This matters because C
//! `unsigned long` is 64-bit on LP64 targets and 32-bit on Windows/ILP32, so the
//! mirror must never hard-code `u32`/`u64` where the C type is `unsigned long`.

// The types below deliberately mirror zlib's C identifiers verbatim
// (`z_stream`, `uInt`, `alloc_func`, `gzFile`, `z_off64_t`, …) so both the
// emitted symbols and the developer-facing type names match `zlib.h` /
// `zconf.h` exactly — that verbatim spelling is the entire point of an
// ABI-mirror module. The idiomatic upper-camel-case convention is therefore
// intentionally waived for this module only (it does not leak to the rest of
// the crate).
#![allow(non_camel_case_types)]

// The C ABI integer aliases. `core::ffi` provides the exact platform widths
// (identical to `std::os::raw`) while remaining usable under `no_std`.
use core::ffi::{c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};
use core::{ptr, slice};

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::gz_header::{GzHeader, HeaderPublication};
use crate::stream::{AllocBuffer, AllocHook, Allocator, ZStream, ZeroValid};

// ===========================================================================
// Phase 2 — C scalar type aliases (mirror `zconf.h`)
// ===========================================================================

/// C `Bytef` — a byte (`Byte` = `unsigned char`), the element type of every
/// input/output buffer in the zlib API (`zconf.h`).
pub type Bytef = c_uchar;

/// C `uInt` — `unsigned int` (`zconf.h`). Used for `avail_in`/`avail_out` and
/// most length fields.
pub type uInt = c_uint;

/// C `uLong` — `unsigned long` (`zconf.h`). 64-bit on LP64, 32-bit on
/// Windows/ILP32. Used for `total_in`/`total_out`/`adler`.
pub type uLong = c_ulong;

/// C `uLongf` — a `FAR`-qualified `uLong` (`zconf.h`); identical to [`uLong`] on
/// modern flat-memory targets.
pub type uLongf = c_ulong;

/// C `voidpf` — `void *` (`zconf.h`), the generic mutable pointer zlib uses for
/// allocator buffers and the `opaque` cookie.
pub type voidpf = *mut c_void;

/// C `voidpc` — `const void *` (`zconf.h`).
pub type voidpc = *const c_void;

/// C `voidp` — `void *` (`zconf.h`).
pub type voidp = *mut c_void;

/// C `z_crc_t` — the 32-bit unsigned type used for CRC-32 values (`zconf.h`,
/// `Z_U4`).
pub type z_crc_t = c_uint;

/// C `z_size_t` — `size_t` (`zconf.h`); the element/most-recent length type for
/// the `*_size_t` one-call helpers.
pub type z_size_t = usize;

/// C `z_off_t` — `off_t` (`zconf.h`); the file-offset type for the gz seek API.
/// Maps to `long` on the common Unix configuration.
pub type z_off_t = c_long;

/// C `z_off64_t` — the 64-bit file-offset type (`zconf.h`), used by the `*64`
/// gz entry points and by [`gzFile_s::pos`]. Always 64 bits wide.
pub type z_off64_t = i64;

// ===========================================================================
// Phase 3 — Opaque `internal_state` handle
// ===========================================================================

/// FFI-opaque stand-in for the C `struct internal_state` (`zlib.h` L88), the
/// type of the [`z_stream::state`] field.
///
/// C never constructs or inspects this type — it only ever holds a pointer to
/// it. The Rust FFI stores `Box::into_raw(handle) as *mut internal_state` here
/// (see [`state_ptr_from_box`]), where `handle` is always one of the three
/// **type-tagged** engine handles — [`DeflateHandle`], the `inflate*`-owned
/// inflate handle, or the `inflateBack*`-owned handle — each of which carries a
/// [`HandleKind`] discriminant at offset 0. Reclamation therefore goes through a
/// tag-validating helper such as [`deflate_take`] rather than a blind
/// `Box::from_raw`, which is what makes a cross-engine `End` call return
/// `Z_STREAM_ERROR` instead of deallocating with a mismatched `Layout`. The
/// empty, private field makes the type both zero-sized and impossible to
/// construct outside this module, exactly matching the "not visible by
/// applications" contract in `zlib.h`.
#[repr(C)]
pub struct internal_state {
    /// Zero-sized private marker: `internal_state` is opaque and never built in
    /// Rust, only pointed at.
    _private: [u8; 0],
}

// ===========================================================================
// Phase 4 — C function-pointer typedefs
// ===========================================================================
//
// Each is an `Option<unsafe extern "C" fn(…)>`. `Option<fn>` is the ABI-correct
// representation of a *nullable* C function pointer: the null-pointer
// optimization guarantees `Option<extern fn>` has the same size and layout as
// the bare function pointer, with `None` encoded as a null address (validated
// by the tests below). Callers may legitimately leave `zalloc`/`zfree` null.

/// C `alloc_func` — `voidpf (*)(voidpf opaque, uInt items, uInt size)`
/// (`zlib.h` L85). The custom allocation hook a caller may install on
/// [`z_stream::zalloc`].
pub type alloc_func =
    Option<unsafe extern "C" fn(opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void>;

/// C `free_func` — `void (*)(voidpf opaque, voidpf address)` (`zlib.h` L86).
/// The custom deallocation hook a caller may install on [`z_stream::zfree`].
pub type free_func = Option<unsafe extern "C" fn(opaque: *mut c_void, address: *mut c_void)>;

/// C `in_func` — `unsigned (*)(void *, z_const unsigned char **)`
/// (`zlib.h` L1134). The input callback for `inflateBack`.
pub type in_func =
    Option<unsafe extern "C" fn(in_desc: *mut c_void, buf: *mut *const c_uchar) -> c_uint>;

/// C `out_func` — `int (*)(void *, unsigned char *, unsigned)` (`zlib.h`
/// L1136). The output callback for `inflateBack`.
pub type out_func =
    Option<unsafe extern "C" fn(out_desc: *mut c_void, buf: *mut c_uchar, len: c_uint) -> c_int>;

// ===========================================================================
// Phase 5 — `#[repr(C)] z_stream`
// ===========================================================================

/// `#[repr(C)]` mirror of the C `z_stream` (`zlib.h` L90-L110).
///
/// LAYOUT: the field order and widths below are **byte-identical** to `zlib.h`
/// and MUST NOT be reordered — the emitted `cdylib`/`staticlib` presents this
/// exact layout to C consumers. A compile-time guard (below) and unit tests
/// assert the offsets. The idiomatic [`ZStream`] is a
/// separate, non-`repr(C)` type; all conversion between the two lives in this
/// module.
///
/// The C `msg` field is `z_const char *`; the mirror stores it as
/// `*mut c_char`. Dropping `const` is ABI-irrelevant (both are a single machine
/// pointer) and lets the shims assign the crate's static, NUL-terminated error
/// strings without a cast.
#[repr(C)]
pub struct z_stream {
    /// Next input byte (C `z_const Bytef *next_in`).
    pub next_in: *const c_uchar,
    /// Number of bytes available at [`next_in`](Self::next_in) (C `uInt`).
    pub avail_in: c_uint,
    /// Total number of input bytes read so far (C `uLong`).
    pub total_in: c_ulong,
    /// Next output byte will go here (C `Bytef *next_out`).
    pub next_out: *mut c_uchar,
    /// Remaining free space at [`next_out`](Self::next_out) (C `uInt`).
    pub avail_out: c_uint,
    /// Total number of bytes output so far (C `uLong`).
    pub total_out: c_ulong,
    /// Last error message, or null when there is no error (C
    /// `z_const char *msg`). Points at a crate-owned static string; never freed
    /// by C.
    pub msg: *mut c_char,
    /// Opaque internal engine state (C `struct internal_state FAR *state`).
    pub state: *mut internal_state,
    /// Caller-supplied allocation hook, or `None` (C `alloc_func zalloc`).
    pub zalloc: alloc_func,
    /// Caller-supplied deallocation hook, or `None` (C `free_func zfree`).
    pub zfree: free_func,
    /// Private cookie passed to [`zalloc`](Self::zalloc)/[`zfree`](Self::zfree)
    /// (C `voidpf opaque`).
    pub opaque: *mut c_void,
    /// Best guess about the data type (C `int data_type`): binary/text for
    /// deflate, or the decode state for inflate.
    pub data_type: c_int,
    /// Adler-32 or CRC-32 of the uncompressed data (C `uLong adler`).
    pub adler: c_ulong,
    /// Reserved for future use (C `uLong reserved`); always `0`.
    pub reserved: c_ulong,
}

/// C `z_streamp` — `z_stream *` (`zlib.h` L112). The handle type every public
/// streaming entry point receives.
pub type z_streamp = *mut z_stream;

// ===========================================================================
// Phase 6 — `#[repr(C)] gz_header`
// ===========================================================================

/// `#[repr(C)]` mirror of the C `gz_header` (`zlib.h` L118-L133), the gzip
/// header exchanged with `deflateSetHeader`/`inflateGetHeader`.
///
/// LAYOUT: field order/widths are byte-identical to `zlib.h`. The idiomatic,
/// owned [`GzHeader`] is the safe counterpart;
/// [`gz_header_to_idiomatic`] and [`write_gz_header_from_idiomatic`] convert
/// between the two.
#[repr(C)]
pub struct gz_header {
    /// `true` (non-zero) if the data is believed to be text (C `int text`).
    pub text: c_int,
    /// Modification time (C `uLong time`).
    pub time: c_ulong,
    /// Extra flags — not used when writing (C `int xflags`).
    pub xflags: c_int,
    /// Operating system (C `int os`).
    pub os: c_int,
    /// Pointer to the extra field, or null (C `Bytef *extra`).
    pub extra: *mut c_uchar,
    /// Extra-field length, valid when [`extra`](Self::extra) is non-null
    /// (C `uInt extra_len`).
    pub extra_len: c_uint,
    /// Capacity at [`extra`](Self::extra), used only when reading
    /// (C `uInt extra_max`).
    pub extra_max: c_uint,
    /// Pointer to the zero-terminated file name, or null (C `Bytef *name`).
    pub name: *mut c_uchar,
    /// Capacity at [`name`](Self::name), used only when reading
    /// (C `uInt name_max`).
    pub name_max: c_uint,
    /// Pointer to the zero-terminated comment, or null (C `Bytef *comment`).
    pub comment: *mut c_uchar,
    /// Capacity at [`comment`](Self::comment), used only when reading
    /// (C `uInt comm_max`).
    pub comm_max: c_uint,
    /// `true` (non-zero) if a header CRC is/will be present (C `int hcrc`).
    pub hcrc: c_int,
    /// `true` when done reading the gzip header (C `int done`); the C field is
    /// tri-state (`1` = done, `-1` = raw zlib stream, `0` = still reading).
    pub done: c_int,
}

/// C `gz_headerp` — `gz_header *` (`zlib.h`).
pub type gz_headerp = *mut gz_header;

// ===========================================================================
// Phase 7 — `#[repr(C)] gzFile_s`
// ===========================================================================

/// `#[repr(C)]` mirror of the exposed C `struct gzFile_s` (`zlib.h`
/// L1956-L1960): `{ unsigned have; unsigned char *next; z_off64_t pos; }`.
///
/// zlib exposes just this abbreviated prefix so the C `gzgetc(g)` *macro* can
/// read `have`/`next`/`pos` directly through the [`gzFile`] pointer.
///
/// # Live prefix
///
/// The crate's idiomatic gz state (`crate::gz::GzState`) is **not** `#[repr(C)]`,
/// so `gz.rs` boxes its opaque handle as a `#[repr(C)]` `GzHandle` whose FIRST
/// field is a live instance of this struct. The `gzFile` pointer therefore
/// begins with a real `{ have, next, pos }` prefix at offset 0 that the C
/// `gzgetc(g)` macro can read and advance directly; `gz.rs` reconciles the
/// prefix with the idiomatic cursor on entry to every shim and re-syncs it on
/// exit, giving true macro fast-path parity. `gzgetc`/`gzgetc_` remain exported
/// as real functions for callers that take the function pointer.
#[repr(C)]
pub struct gzFile_s {
    /// Bytes currently available in [`next`](Self::next) (C `unsigned have`).
    pub have: c_uint,
    /// Pointer to the next available byte (C `unsigned char *next`).
    pub next: *mut c_uchar,
    /// Current position in the uncompressed stream (C `z_off64_t pos`).
    pub pos: i64,
}

/// C `gzFile` — `struct gzFile_s *` (`zlib.h`). The opaque handle returned by
/// `gzopen`/`gzdopen`.
pub type gzFile = *mut gzFile_s;

// ===========================================================================
// Phase 8 — Caller-allocator bridge: `CAllocator`
// ===========================================================================

/// Bridges a C caller's `zalloc`/`zfree`/`opaque` triple into the crate's
/// [`Allocator`] abstraction (AAP §0.6.3).
///
/// The FFI layer always instantiates its streams as
/// [`ZStream<CAllocator>`](crate::stream::ZStream), giving the deflate/inflate
/// shims a *single* monomorphized handle type to box into
/// [`z_stream::state`] regardless of whether the caller supplied hooks.
///
/// # Hook routing (AAP §0.6.3 "has-hook clause")
///
/// The crate's [`Allocator`] trait returns an
/// [`AllocBuffer`] — a smart owned region that is
/// **either** a global-allocator [`Vec`] **or** a foreign region carved from a
/// caller's `zalloc` and released through their `zfree` on [`Drop`]. This
/// resolves the historical limitation (a `Vec` cannot be soundly reclaimed
/// through a foreign `zfree` on stable Rust, which lacks a stable
/// `allocator_api`): the foreign case is a dedicated variant that owns its raw
/// pointer and hook, so its `Drop` calls the matching `zfree` — never the
/// global allocator.
///
/// [`CAllocator`] therefore forwards its captured triple as an
/// [`AllocHook`] via [`Allocator::hook`], and its
/// [`allocate_zeroed`](Allocator::allocate_zeroed) routes every working buffer
/// (window, `pending_buf`, hash tables) through the caller's `zalloc` when
/// **both** `zalloc` and `zfree` are supplied. When both are null it uses the
/// global allocator, exactly matching AAP §0.6.3's "otherwise `std::alloc` is
/// used" clause; an *active* hook that reports out-of-memory is reported as an
/// allocation failure and never falls back. The **ABI guarantee** is
/// unconditional: supplying `zalloc`/`zfree` routes allocation through them and
/// never breaks the stream, and leaving them null works identically to before.
///
/// The boxed engine *state* handle itself is still allocated through the global
/// allocator (Rust `Box`); it is the working *buffers* — the bulk of a stream's
/// footprint and precisely what AAP §0.6.3 enumerates — that honor the hook.
#[derive(Clone, Copy)]
pub struct CAllocator {
    /// The caller's allocation hook, or `None` (mirrors [`z_stream::zalloc`]).
    pub zalloc: alloc_func,
    /// The caller's deallocation hook, or `None` (mirrors [`z_stream::zfree`]).
    pub zfree: free_func,
    /// The caller's private cookie (mirrors [`z_stream::opaque`]).
    pub opaque: *mut c_void,
}

impl CAllocator {
    /// Captures the `zalloc`/`zfree`/`opaque` triple from a raw [`z_stream`].
    ///
    /// # Safety
    ///
    /// `strm` must reference a validly-initialized [`z_stream`]. Only plain
    /// `Copy` fields are read (no dereference of the hook pointers occurs
    /// here), so the requirement is simply that the reference itself is valid.
    #[inline]
    #[must_use]
    pub unsafe fn from_stream(strm: &z_stream) -> Self {
        // SAFETY: `strm` is a valid `&z_stream`; `zalloc`/`zfree`/`opaque` are
        // plain `Copy` fields (function pointers / a raw cookie pointer) and are
        // merely copied out, never dereferenced.
        Self {
            zalloc: strm.zalloc,
            zfree: strm.zfree,
            opaque: strm.opaque,
        }
    }

    /// Whether exactly one half of the `zalloc`/`zfree` pair is present.
    ///
    /// Such a pair is unusable, and the C library agrees: `inflateStateCheck`
    /// (`inflate.c` L90-L91) and `deflateStateCheck` (`deflate.c` L540-L541)
    /// both classify a stream whose `zalloc` **or** `zfree` is null as invalid,
    /// so every entry point after initialization answers `Z_STREAM_ERROR`.
    ///
    /// C's three `*Init*_` prologues avoid ever tripping that check by
    /// substituting the *missing half* in place — `zcalloc` for a null `zalloc`,
    /// `zcfree` for a null `zfree` (`inflate.c` L183-L196, `deflate.c`
    /// L400-L414, `infback.c` L37-L50). [`init_allocator_prologue`] reproduces
    /// that substitution byte-for-byte and per half, so a stream this crate
    /// initialized **always** carries two non-null halves and this predicate is
    /// `false` for it.
    ///
    /// The predicate therefore has exactly one job: detecting a stream whose
    /// allocator fields were mutated *after* initialization, which is the same
    /// condition C's `*StateCheck` functions reject. `inflateCopy` uses it for
    /// that purpose, because C reaches its own `Z_STREAM_ERROR` there through
    /// `inflateStateCheck(source)` before its `ZALLOC(source, …)`. Rejecting
    /// keeps the copy from silently allocating out of the global heap while the
    /// caller believes their hook owns the memory — the "caller buffers are used
    /// only when both halves are supplied" half of AAP §0.6.3's has-hook clause.
    #[inline]
    #[must_use]
    pub(crate) const fn is_half_present(&self) -> bool {
        self.zalloc.is_some() != self.zfree.is_some()
    }

    /// Whether **both** captured halves are this crate's own built-in
    /// substitutes rather than anything the caller supplied.
    ///
    /// [`init_allocator_prologue`] fills in every missing half with
    /// [`default_zalloc`](crate::ffi::alloc::default_zalloc) /
    /// [`default_zfree`](crate::ffi::alloc::default_zfree), exactly as C fills
    /// in `zcalloc`/`zcfree` (`deflate.c` L400-L414, `inflate.c` L183-L196,
    /// `infback.c` L37-L50). When the caller supplied *neither* half, the pair a
    /// stream ends up publishing is therefore entirely the library's own default
    /// allocator — the counterpart of C's `zcalloc`/`zcfree`, not a caller hook.
    ///
    /// [`Allocator::hook`] consults this predicate so such a stream reports **no
    /// hook** and keeps using the crate's default (global-allocator) path. That
    /// is what AAP §0.6.5 requires: the engine-state footprint is charged to the
    /// caller only when they actually installed an allocator, so a hookless C
    /// caller's allocation count and footprint stay byte-for-byte what they have
    /// always been. A *half*-present pair is deliberately **not** matched here:
    /// the caller's own half is real and must be honored, so such a stream keeps
    /// an active hook (a deliberately failing caller `zalloc` still surfaces as
    /// `Z_MEM_ERROR` rather than being bypassed), exactly as in C.
    ///
    /// The built-ins are `pub(crate)` and are never exported (they mirror
    /// `zlib.map` `local:` entries), so a caller cannot supply either address
    /// and this predicate cannot mistake a genuine hook for a substitute.
    #[inline]
    #[must_use]
    pub(crate) fn is_builtin_pair(&self) -> bool {
        match (self.zalloc, self.zfree) {
            (Some(zalloc), Some(zfree)) => {
                core::ptr::fn_addr_eq(
                    zalloc,
                    crate::ffi::alloc::default_zalloc
                        as unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void,
                ) && core::ptr::fn_addr_eq(
                    zfree,
                    crate::ffi::alloc::default_zfree
                        as unsafe extern "C" fn(*mut c_void, *mut c_void),
                )
            }
            _ => false,
        }
    }
}

/// Reproduces C's `*Init*_` allocator prologue on a caller's [`z_stream`] and
/// returns the resulting [`CAllocator`].
///
/// Crate-private: this is init-sequence plumbing for the five versioned `*Init*_`
/// shims in [`crate::ffi::deflate`] and [`crate::ffi::inflate`], and it mutates a
/// caller's `z_stream` in place. It is deliberately **not** part of the public
/// surface — an external caller with a raw `z_stream` wants a full
/// `deflateInit2_`/`inflateInit2_`, never this prologue on its own, and exposing
/// it would publish an `unsafe fn` whose only correct use site is inside the
/// initializers themselves.
///
/// All three C initializers — `deflateInit2_` (`deflate.c` L399-L414),
/// `inflateInit2_` (`inflate.c` L182-L196) and `inflateBackInit_`
/// (`infback.c` L36-L50) — carry the identical prologue, and they run it *after*
/// the version/`stream_size` guard and the null-argument guards but *before* any
/// parameter validation and before the state `ZALLOC`. This function is that
/// prologue, in that order:
///
/// 1. `strm->msg = Z_NULL` — cleared unconditionally, "in case we return an
///    error", so a stale diagnostic from a previous call cannot survive into a
///    failed initialization. This happens **before** the allocator is inspected.
/// 2. If `strm->zalloc` is null, install `crate::ffi::alloc::default_zalloc` and
///    clear `strm->opaque`. C clears `opaque` on exactly this branch and nowhere
///    else (`deflate.c` L405-L406), because the cookie belonged to the allocator
///    that is being replaced; a caller who supplied only `zfree` keeps *their*
///    `zfree` but must not have a stale cookie handed to the built-in `zalloc`.
/// 3. If `strm->zfree` is null, install `crate::ffi::alloc::default_zfree`.
///    `opaque` is **not** touched on this branch — again exactly as C does it
///    (`deflate.c` L408-L413).
///
/// The two built-ins are the crate's counterparts of C's `zcalloc`/`zcfree`, are
/// `malloc`/`free`-backed just as C's are, and — like the `zlib.map` `local:`
/// entries they mirror — are never exported. Substituting per half is what makes
/// a partially-supplied pair behave exactly as it does in C: the caller's own
/// half is honored (so a deliberately failing `zalloc` still produces
/// `Z_MEM_ERROR` rather than being bypassed), the missing half is filled in, and
/// initialization proceeds.
///
/// # Every missing half is substituted, including a wholly absent pair
///
/// The two `if` tests are independent in C and are independent here: a caller
/// who supplied neither half gets **both** built-ins published into their
/// `z_stream`, precisely as `deflate.c` L400-L414 publishes both `zcalloc` and
/// `zcfree`. After any successful initialization — and after a failed one that
/// got past the version guard — a C caller inspecting `strm->zalloc` and
/// `strm->zfree` therefore sees the same non-null shape reference zlib leaves
/// behind, so nothing observable through the ABI differs.
///
/// Publishing the built-ins does **not** change which allocator the engine
/// actually uses. [`CAllocator::is_builtin_pair`] recognizes a pair that consists
/// solely of this crate's own substitutes, and [`Allocator::hook`] answers "no
/// hook" for it, so such a stream keeps the crate's default global-allocator
/// path. That preserves AAP §0.6.5's requirement that the engine-state footprint
/// is charged to the caller only when they actually installed an allocator: a
/// hookless caller's allocation count and footprint stay byte-for-byte what they
/// have always been. See [`Allocator::reserves_state_footprint`], whose entire
/// purpose is that requirement.
///
/// A *half*-present pair behaves differently and must: the caller's own half is
/// real, so the completed pair stays an **active** hook and every buffer is
/// charged to it. A deliberately failing caller `zalloc` therefore still
/// surfaces as `Z_MEM_ERROR` rather than being bypassed, exactly as in C.
///
/// # Safety
///
/// `strm` must reference a validly-initialized [`z_stream`] that the caller owns
/// exclusively for the duration of the call. Only plain `Copy` fields are read
/// and written (`msg`, `zalloc`, `zfree`, `opaque`); no hook pointer is
/// dereferenced here.
#[inline]
pub(crate) unsafe fn init_allocator_prologue(strm: &mut z_stream) -> CAllocator {
    // Step 1 — `strm->msg = Z_NULL;` (deflate.c L399, inflate.c L182,
    // infback.c L36). Unconditional, and ahead of the allocator inspection, so
    // every subsequent `return` in the initializer reports a clean `msg`.
    strm.msg = core::ptr::null_mut();

    // Step 2 — `if (strm->zalloc == 0) { strm->zalloc = zcalloc; strm->opaque = 0; }`
    // (deflate.c L400-L407, inflate.c L183-L190, infback.c L37-L44).
    //
    // Unconditional per half, exactly as C writes it: a caller who supplied
    // neither half gets both built-ins published, so the `z_stream` a C caller
    // inspects afterwards carries the same non-null shape reference zlib leaves
    // behind. Which allocator the engine *uses* is decided separately by
    // `CAllocator::is_builtin_pair` / `Allocator::hook`, so a hookless caller's
    // allocation count and footprint stay unchanged (AAP §0.6.5).
    if strm.zalloc.is_none() {
        strm.zalloc = Some(crate::ffi::alloc::default_zalloc);
        strm.opaque = core::ptr::null_mut();
    }

    // Step 3 — `if (strm->zfree == 0) strm->zfree = zcfree;` (deflate.c
    // L408-L413, inflate.c L191-L196, infback.c L45-L50). Note that C does NOT
    // touch `opaque` on this branch.
    if strm.zfree.is_none() {
        strm.zfree = Some(crate::ffi::alloc::default_zfree);
    }

    // SAFETY: `strm` is a valid `&z_stream`; `from_stream` only copies the plain
    // `Copy` allocator fields out and never dereferences a hook pointer.
    unsafe { CAllocator::from_stream(strm) }
}

/// Whether `strm`'s published `zalloc`/`zfree` pair consists solely of this
/// crate's built-in substitutes — the shape [`init_allocator_prologue`] leaves
/// behind for a caller who supplied neither half, mirroring the `zcalloc`/`zcfree`
/// pair reference zlib publishes (`deflate.c` L400-L414).
///
/// Test-only: it lets the FFI regression tests assert C-shaped publication
/// without hard-coding raw function addresses.
#[cfg(test)]
#[inline]
#[must_use]
pub(crate) fn publishes_builtin_alloc_pair(strm: &z_stream) -> bool {
    CAllocator {
        zalloc: strm.zalloc,
        zfree: strm.zfree,
        opaque: strm.opaque,
    }
    .is_builtin_pair()
}

impl Allocator for CAllocator {
    /// Allocates a zero-initialized buffer of `count` elements, **routing
    /// through the caller's `zalloc` when supplied** (AAP §0.6.3, "has-hook
    /// clause").
    ///
    /// The returned [`AllocBuffer`] is a [`Foreign`](AllocBuffer::Foreign)
    /// region carved from the caller's `zalloc` (and released through their
    /// `zfree` on drop) whenever this [`CAllocator`] carries an active
    /// [`hook`](Allocator::hook); with **both** hooks null — or with a pair made
    /// up solely of the crate's own built-in substitutes, which is what a
    /// hookless caller ends up publishing — or for an empty request, it uses a
    /// global-allocator [`Vec`](AllocBuffer::Owned), matching AAP §0.6.3's
    /// "otherwise `std::alloc` is used" clause and C's substitution of
    /// `zcalloc`/`zcfree` for a wholly absent pair (`deflate.c` L400-L414).
    ///
    /// A *half*-present pair never reaches this method from a C entry point:
    /// `init_allocator_prologue` has already completed it by substituting the
    /// crate's built-in for the missing half, exactly as C's `*Init*_` prologues
    /// substitute `zcalloc`/`zcfree` (`deflate.c` L400-L414, `inflate.c`
    /// L183-L196, `infback.c` L37-L50). The caller's own half is therefore
    /// honored — a deliberately failing `zalloc` still surfaces as `Z_MEM_ERROR`
    /// rather than being bypassed. Soundness is preserved because
    /// the returned buffer knows its own backing store and frees it the matching
    /// way on [`Drop`] — the historical unsoundness of dropping a foreign-backed
    /// `Vec` through the global allocator cannot occur.
    ///
    /// When an **active** hook's `zalloc` reports out-of-memory (or the request is
    /// unrepresentable, or the returned region is unusably aligned) this returns
    /// [`None`] and the caller surfaces `Z_MEM_ERROR`. There is deliberately no
    /// global-allocator fallback on that path.
    #[inline]
    fn allocate_zeroed<T>(&self, count: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        AllocBuffer::try_zeroed(count, self.hook())
    }

    /// Exposes the caller's `zalloc`/`zfree`/`opaque` triple as an
    /// [`AllocHook`], so buffers this allocator produces (directly, or lazily on
    /// a state it initializes) are backed by the caller's allocator.
    ///
    /// A pair consisting *solely* of this crate's own built-in substitutes —
    /// which is what `init_allocator_prologue` publishes when the caller
    /// supplied neither half, mirroring C's `zcalloc`/`zcfree` — is **not** a
    /// caller hook and yields [`AllocHook::none`], selecting the crate's default
    /// global-allocator path. See `CAllocator::is_builtin_pair`: this is what
    /// keeps a hookless C caller's allocation count and engine-state footprint
    /// byte-for-byte unchanged (AAP §0.6.5) even though the raw `z_stream` fields
    /// are populated exactly as C populates them.
    #[inline]
    fn hook(&self) -> AllocHook {
        if self.is_builtin_pair() {
            AllocHook::none()
        } else {
            AllocHook::new(self.zalloc, self.zfree, self.opaque)
        }
    }

    /// The engine-state footprint is charged to the caller only when they
    /// actually installed a hook, exactly as C charges
    /// `ZALLOC(strm, 1, sizeof(deflate_state))` to `strm->zalloc`
    /// (`deflate.c` L440). With null hooks this returns `false`, keeping the
    /// footprint of a hookless C caller byte-for-byte what it has always been
    /// (AAP §0.6.5).
    #[inline]
    fn reserves_state_footprint(&self) -> bool {
        self.hook().is_active()
    }

    // `deallocate` uses the trait default: dropping the `AllocBuffer` routes to
    // the correct deallocator — the caller's `zfree` for a `Foreign` region, or
    // the global allocator for an `Owned` fallback.
    //
    // `allocate_zeroed_items` also uses the trait default, which forwards C's
    // `(items, size)` pair to `hook()` verbatim.
}

/// Builds an [`AllocHook`] from a raw C `zalloc`/`zfree`/`opaque` triple.
///
/// This is the sanctioned public path for constructing an **active** allocator
/// hook — one that routes engine buffers through foreign C function pointers.
/// `AllocHook::new` is deliberately crate-private: the
/// obligations below cannot be checked by the compiler, and `src/stream.rs`
/// carries `#![deny(unsafe_code)]`, so the constructor that imposes them belongs
/// at the FFI boundary where `unsafe` is permitted (AAP §0.6.2).
///
/// A Rust-native [`Allocator`] implementation does **not** need this: it should
/// override [`Allocator::allocate_zeroed`] /
/// [`Allocator::allocate_zeroed_items`], which the engines call for every
/// working buffer and for each engine-state footprint. Reach for this function
/// only when the storage genuinely lives behind a C `alloc_func`/`free_func`
/// pair — for example when embedding this crate underneath an existing C
/// allocator, or when re-implementing the drop-in boundary.
///
/// Passing `None` for either half yields an *inactive* hook
/// ([`AllocHook::is_active`] is `false`), which selects the global-allocator
/// path. That mirrors the AAP's has-hook policy: caller storage is used only when
/// **both** halves are present (AAP §0.6.3).
///
/// # Safety
///
/// The caller must guarantee, for the entire lifetime of every buffer allocated
/// through the returned hook, that:
///
/// 1. `zalloc(opaque, items, size)` either returns null or returns a pointer to
///    at least `items * size` writable bytes that no other code accesses;
/// 2. `zfree(opaque, address)` releases exactly a region previously returned by
///    that `zalloc` with the same `opaque`, and is safe to call once per region;
/// 3. neither hook unwinds into Rust — both are `extern "C"`, so unwinding across
///    the boundary is undefined behavior; and
/// 4. `opaque` remains valid for both hooks for as long as any buffer allocated
///    through this hook is alive.
///
/// These are precisely the obligations `zlib.h` L85-L86 already places on
/// `alloc_func`/`free_func`. Everything that *can* be verified mechanically —
/// null returns, unrepresentable sizes, and misalignment — is checked by the
/// crate: a misaligned region is handed straight back to `zfree` and the request
/// is reported as an allocation failure, so a merely *unhelpful* hook produces
/// `Z_MEM_ERROR` rather than undefined behavior.
///
/// # Examples
///
/// ```
/// use core::ffi::{c_uint, c_void};
/// use zlib_rs::stream::{Allocator, AllocHook};
/// use zlib_rs::ffi::types::alloc_hook_from_parts;
///
/// unsafe extern "C" fn my_alloc(_opaque: *mut c_void, _items: c_uint, _size: c_uint)
///     -> *mut c_void { core::ptr::null_mut() }
/// unsafe extern "C" fn my_free(_opaque: *mut c_void, _address: *mut c_void) {}
///
/// // SAFETY: `my_alloc` only ever reports out-of-memory (it returns null), and
/// // `my_free` is a no-op that is never handed a region, so clauses 1-4 hold.
/// let hook = unsafe {
///     alloc_hook_from_parts(Some(my_alloc), Some(my_free), core::ptr::null_mut())
/// };
/// assert!(hook.is_active());
///
/// // An allocator that forwards to it reports the hook's out-of-memory verbatim.
/// struct Forwarding(AllocHook);
/// impl Allocator for Forwarding {
///     fn hook(&self) -> AllocHook { self.0 }
/// }
/// assert!(Forwarding(hook).allocate_zeroed::<u8>(64).is_none());
/// ```
#[inline]
#[must_use]
pub const unsafe fn alloc_hook_from_parts(
    zalloc: alloc_func,
    zfree: free_func,
    opaque: *mut c_void,
) -> AllocHook {
    AllocHook::new(zalloc, zfree, opaque)
}

/// Builds an idiomatic [`ZStream<CAllocator>`](crate::stream::ZStream) whose
/// allocator carries the caller's `zalloc`/`zfree`/`opaque` triple.
///
/// It produces the one monomorphized handle type the boundary boxes into
/// [`z_stream::state`], honoring caller hooks when the pair is complete and
/// global-allocating when neither half is present (see [`CAllocator`]).
///
/// # Use `init_allocator_prologue` in an `*Init*_` shim
///
/// This constructor reads the triple exactly as it finds it, so it is the right
/// tool only where that triple is already known-complete — for example when it
/// has been cloned out of an initialized handle. The `deflateInit2_`,
/// `inflateInit2_`, and `inflateBackInit_` shims must additionally reproduce C's
/// initialization prologue (clear `msg`, substitute a missing half of the pair),
/// so they call `init_allocator_prologue` and hand its result to
/// [`ZStream::with_allocator`] instead.
///
/// # Safety
///
/// `strm` must reference a validly-initialized [`z_stream`] (see
/// [`CAllocator::from_stream`]).
#[inline]
#[must_use]
pub unsafe fn zstream_with_caller_alloc(strm: &z_stream) -> ZStream<CAllocator> {
    // SAFETY: forwarded to the caller: `strm` is a valid `&z_stream`.
    let allocator = unsafe { CAllocator::from_stream(strm) };
    ZStream::with_allocator(allocator)
}

// ===========================================================================
// Phase 9 — Conversion glue (raw ↔ idiomatic)
// ===========================================================================
//
// These helpers are consumed by the sibling shim files (`deflate.rs`,
// `inflate.rs`, `util.rs`, `gz.rs`). They move between the raw `z_stream`/
// `gz_header` and the idiomatic engine state / `GzHeader`. Each raw-pointer
// dereference carries a `// SAFETY:` justification, and each public `unsafe fn`
// documents its contract in a `# Safety` section.

// --- Opaque state-handle helpers -------------------------------------------

/// Consumes a boxed engine handle and returns it as the opaque
/// [`z_stream::state`] pointer.
///
/// In production `T` is always one of the three **type-tagged** engine handles —
/// [`DeflateHandle`] (installed by the `deflate*` shims), the inflate handle
/// (installed by `inflateInit*`), or the `inflateBack` handle (installed by
/// `inflateBackInit_`) — each a `#[repr(C)]` struct carrying a [`HandleKind`] at
/// offset 0. Ownership is transferred to the C `state` field; the box must later
/// be reclaimed exactly once to avoid a leak, and that reclamation goes through
/// the matching tag-validating helper ([`deflate_take`] for a deflate stream,
/// its inflate counterparts for the two decode paths) rather than a blind
/// `Box::from_raw`.
///
/// # Safety
///
/// The returned pointer must be treated as owning: it must be reclaimed exactly
/// once — via the tag-validating `*_take` helper for its handle kind, or an
/// equivalent [`state_take`]/`Box::from_raw` **with the same `T`** — and must not
/// be aliased by another owner. Reconstituting it as a different `T` would
/// deallocate with a `Layout` that does not match the allocation, which is why
/// the tag check exists.
#[inline]
#[must_use]
pub unsafe fn state_ptr_from_box<T>(b: Box<T>) -> *mut internal_state {
    // `Box::into_raw` is a safe operation; the pointer cast reinterprets the
    // handle as the opaque C `state` type without any dereference.
    Box::into_raw(b) as *mut internal_state
}

/// Borrows the boxed engine handle stored in [`z_stream::state`], or [`None`]
/// if no engine is installed.
///
/// This is the raw, **untagged** primitive: it performs no [`HandleKind`] check.
/// The exported shims never call it directly on a caller-supplied stream — they
/// go through the tag-validating accessors ([`deflate_state`] and its inflate
/// counterparts), which read the tag through the shared C-layout header prefix
/// before reinterpreting the pointer.
///
/// # Safety
///
/// If [`z_stream::state`] is non-null it must point at a live `Box<T>` that was
/// installed by [`state_ptr_from_box`] with the *same* `T`, and must not be
/// mutably aliased for the returned borrow's lifetime (tied to `strm`).
#[inline]
pub unsafe fn state_ref<T>(strm: &mut z_stream) -> Option<&mut T> {
    if strm.state.is_null() {
        None
    } else {
        // SAFETY: `state` is non-null and, per the contract, points at a live,
        // uniquely-owned `T` installed via `state_ptr_from_box::<T>`. The
        // returned borrow is tied to the `&'a mut z_stream`, so it cannot alias.
        Some(unsafe { &mut *(strm.state as *mut T) })
    }
}

/// Reclaims the boxed engine handle from [`z_stream::state`], leaving the field
/// null, or returns [`None`] if no engine is installed.
///
/// Dropping the returned box runs the engine's RAII teardown — the replacement
/// for the C `deflateEnd`/`inflateEnd` free path.
///
/// This is the raw, **untagged** primitive: it performs no [`HandleKind`] check,
/// so the exported `End` shims use the tag-validating helpers ([`deflate_take`]
/// and its inflate counterparts) instead. Those confirm the tag *before*
/// reconstituting the box, which is what turns a cross-engine `End` call into a
/// `Z_STREAM_ERROR` rather than a layout-mismatched deallocation.
///
/// # Safety
///
/// If [`z_stream::state`] is non-null it must point at a live `Box<T>` (same
/// `T`) installed by [`state_ptr_from_box`], and must not have been reclaimed
/// already (no double free).
#[inline]
pub unsafe fn state_take<T>(strm: &mut z_stream) -> Option<Box<T>> {
    if strm.state.is_null() {
        None
    } else {
        // SAFETY: `state` is non-null and owns a `Box<T>` installed via
        // `state_ptr_from_box::<T>`; reconstituting the box transfers ownership
        // back to Rust exactly once. Null the field to prevent a double free.
        let b = unsafe { Box::from_raw(strm.state as *mut T) };
        strm.state = ptr::null_mut();
        Some(b)
    }
}

// --- Type-tagged state handles ---------------------------------------------
//
// The C `deflateEnd`/`inflateEnd` contract lets a caller (incorrectly) pass a
// stream that was initialized by the *other* engine. The deflate and inflate
// handles are `Box`es of DIFFERENT concrete types — hence different sizes — so a
// blind `Box::from_raw(state as *mut T)` inside the wrong `End` shim would
// reconstitute (and drop) a `Box<T>` whose `Layout` does not match the original
// allocation. That violates the `GlobalAlloc::dealloc` contract (the dealloc
// `Layout` must equal the alloc `Layout`). This is undefined behavior. It may
// appear to work against glibc's size-agnostic `free`, but it can corrupt an
// allocator that relies on the size passed to deallocation (jemalloc, mimalloc,
// and Rust's own sized-dealloc path) — and, being UB, it carries no guarantee on
// any allocator. Tagging each handle with a discriminant at offset 0 and
// validating it BEFORE reconstituting the box makes the mismatch detectable, so
// the shim returns `Z_STREAM_ERROR` without ever dropping a wrong-type box.

/// Discriminant magic stored as the FIRST field (offset 0) of every boxed FFI
/// engine handle installed in [`z_stream::state`].
///
/// Stored as a plain `u64` (via `#[repr(transparent)]`) so the tag can be read
/// through `HandleHeader` with no enum-validity concern: every bit pattern is
/// a valid `u64`, and only the published magics compare equal. The values have
/// their high bits set so they can never collide with the small integer that
/// leads a bare engine state, providing defense-in-depth even though every FFI
/// init shim installs a tagged `#[repr(C)]` handle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct HandleKind(u64);

impl HandleKind {
    /// Tag identifying a boxed [`DeflateHandle`] (a `deflate*`-owned stream).
    pub const DEFLATE: HandleKind = HandleKind(0xDEF1_A7E5_0DEF_0001);
    /// Tag identifying a boxed inflate handle (an `inflate*`-owned stream).
    pub const INFLATE: HandleKind = HandleKind(0x14F1_A7E5_0114_0002);
    /// Tag identifying a boxed `inflateBack` handle (an `inflateBack*`-owned
    /// stream). Distinct from [`INFLATE`](Self::INFLATE) so the regular
    /// `inflateEnd`/`inflateBackEnd` terminators can reject cross-type misuse
    /// (freeing an `inflateBack` state as a plain inflate handle, or vice
    /// versa, would be a layout-mismatched free — the same undefined behavior a
    /// blind cast of the opaque `state` pointer would cause).
    pub const INFLATE_BACK: HandleKind = HandleKind(0x14F1_A7E5_BAC6_0003);
}

/// The common `#[repr(C)]` prefix shared by every tagged handle: a
/// [`HandleKind`] at offset 0, readable regardless of the concrete handle type
/// behind the opaque `state` pointer.
#[repr(C)]
struct HandleHeader {
    kind: HandleKind,
}

/// The boxed deflate engine handle installed in [`z_stream::state`] by the
/// `deflate*` FFI shims.
///
/// Wrapping [`ZStream<CAllocator>`](crate::stream::ZStream) behind a
/// [`HandleKind`] tag lets [`deflate_take`]/[`deflate_state`] confirm the handle
/// really is a deflate handle before reinterpreting the opaque pointer,
/// preventing the layout-mismatched deallocation that a blind cast would cause
/// on cross-type `End` misuse. `#[repr(C)]` guarantees `kind` is at
/// offset 0.
#[repr(C)]
pub struct DeflateHandle {
    /// Discriminant tag; always [`HandleKind::DEFLATE`]. MUST be the first field.
    kind: HandleKind,
    /// The idiomatic deflate stream this handle owns.
    pub zs: ZStream<CAllocator>,
}

impl DeflateHandle {
    /// Wraps a freshly built deflate stream, tagging it as a deflate handle.
    #[inline]
    #[must_use]
    pub fn new(zs: ZStream<CAllocator>) -> Self {
        Self {
            kind: HandleKind::DEFLATE,
            zs,
        }
    }
}

/// Reads the [`HandleKind`] tag at offset 0 of the installed state handle, or
/// [`None`] if no handle is installed.
///
/// # Safety
///
/// If [`z_stream::state`] is non-null it must point at a live boxed handle whose
/// first field is a [`HandleKind`]. Every FFI init shim — `deflateInit*`,
/// `inflateInit*`, and `inflateBackInit_` — boxes a `#[repr(C)]` type with a
/// leading `HandleKind`, so the tag is always present and simply read/compared
/// (no enum-validity requirement).
#[inline]
#[must_use]
pub unsafe fn peek_handle_kind(strm: &z_stream) -> Option<HandleKind> {
    if strm.state.is_null() {
        None
    } else {
        // SAFETY: `state` is non-null and points at a live handle allocation of
        // at least 8 bytes; reading the leading `HandleKind` (a `u64`) is valid
        // for any such allocation and carries no enum-validity requirement.
        Some(unsafe { (*(strm.state as *const HandleHeader)).kind })
    }
}

/// Borrows the deflate engine behind the state handle, validating the handle
/// kind first. Returns [`None`] when no handle is installed OR the installed
/// handle is not a deflate handle, letting the caller return `Z_STREAM_ERROR`
/// WITHOUT reinterpreting a wrong-type allocation.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by an
/// FFI init shim (so its leading tag is readable).
#[inline]
pub unsafe fn deflate_state(strm: &mut z_stream) -> Option<&mut ZStream<CAllocator>> {
    // SAFETY: delegated tag read; see `peek_handle_kind`.
    match unsafe { peek_handle_kind(strm) } {
        Some(kind) if kind == HandleKind::DEFLATE => {
            // SAFETY: the tag confirms a live `DeflateHandle`; the borrow is tied
            // to `strm`, so it cannot alias for its lifetime.
            let handle = unsafe { &mut *(strm.state as *mut DeflateHandle) };
            Some(&mut handle.zs)
        }
        _ => None,
    }
}

/// Reclaims the boxed [`DeflateHandle`] from the state field, validating the
/// kind first and nulling `state` on success. Returns [`None`] — leaving
/// `state` untouched — when the handle is absent or not a deflate handle, so
/// `deflateEnd` never drops a wrong-type box.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by an
/// FFI init shim, not already reclaimed.
#[inline]
pub unsafe fn deflate_take(strm: &mut z_stream) -> Option<Box<DeflateHandle>> {
    // SAFETY: delegated tag read; see `peek_handle_kind`.
    match unsafe { peek_handle_kind(strm) } {
        Some(kind) if kind == HandleKind::DEFLATE => {
            // SAFETY: the tag confirms a live `Box<DeflateHandle>`; reconstitute
            // exactly once and null the field to prevent a double free.
            let boxed = unsafe { Box::from_raw(strm.state as *mut DeflateHandle) };
            strm.state = ptr::null_mut();
            Some(boxed)
        }
        _ => None,
    }
}

// --- Input/output slice bridging -------------------------------------------

/// Views the pending input as a slice of [`z_stream::avail_in`] bytes at
/// [`z_stream::next_in`], or an empty slice when the buffer is null/empty.
///
/// # Safety
///
/// When [`z_stream::next_in`] is non-null and [`z_stream::avail_in`] is
/// non-zero, the caller guarantees `avail_in` bytes are readable at `next_in`
/// and remain valid and unmodified for the returned borrow's lifetime `'a`.
#[inline]
#[must_use]
pub unsafe fn input_slice<'a>(strm: &z_stream) -> &'a [u8] {
    if strm.next_in.is_null() || strm.avail_in == 0 {
        &[]
    } else {
        // SAFETY: per the contract, `avail_in` bytes are readable at the
        // non-null `next_in`. `u8` and `c_uchar` share a layout.
        unsafe { slice::from_raw_parts(strm.next_in, strm.avail_in as usize) }
    }
}

/// Views the free output space as a mutable slice of [`z_stream::avail_out`]
/// bytes at [`z_stream::next_out`], or an empty slice when null/empty.
///
/// # Safety
///
/// When [`z_stream::next_out`] is non-null and [`z_stream::avail_out`] is
/// non-zero, the caller guarantees `avail_out` bytes are writable at `next_out`
/// and that this output region is disjoint from the input region for the
/// returned borrow's lifetime `'a`.
#[inline]
#[must_use]
// The returned `&mut [u8]` aliases the caller's external output buffer
// (`next_out`), which is disjoint from the `z_stream` struct itself; producing
// it from `&z_stream` is sound and lets a shim obtain input and output slices
// from one shared borrow. The `mut_from_ref` heuristic cannot see this.
#[allow(clippy::mut_from_ref)]
pub unsafe fn output_slice<'a>(strm: &z_stream) -> &'a mut [u8] {
    if strm.next_out.is_null() || strm.avail_out == 0 {
        &mut []
    } else {
        // SAFETY: per the contract, `avail_out` bytes are writable at the
        // non-null `next_out` and disjoint from the input region.
        unsafe { slice::from_raw_parts_mut(strm.next_out, strm.avail_out as usize) }
    }
}

/// Returns `true` when the input pointer is consistent with `avail_in`: a
/// positive `avail_in` requires a non-null `next_in` (a null `next_in` is only
/// permitted when `avail_in == 0`).
///
/// This is the input half of the entry-validation contract, factored out for
/// the call sites that constrain only the input buffer — e.g. C `inflateSync`
/// (`inflate.c`), which validates `next_in`/`avail_in` but has no output buffer
/// to check.
///
/// It is a *pure* pointer/length consistency check performed **before** any
/// bridging via [`input_slice`]: that primitive deliberately tolerates a
/// null-and-empty input by yielding an empty slice, so the shims must reject
/// the `avail_in != 0 && next_in == NULL` programmer error explicitly here
/// rather than let it be silently masked into an empty read.
#[inline]
#[must_use]
pub fn input_ptr_valid(strm: &z_stream) -> bool {
    strm.avail_in == 0 || !strm.next_in.is_null()
}

/// Returns `true` when the stream's I/O buffers satisfy the entry-validation
/// contract shared by C `deflate` and `inflate`: `next_out` must be non-null,
/// and a positive `avail_in` requires a non-null `next_in`.
///
/// This mirrors the guards at the top of C `deflate` (`deflate.c` L981-L1010)
/// and C `inflate` (`inflate.c` L474). A null `next_out` is rejected
/// **unconditionally** — matching C, whose `next_out == Z_NULL` check carries
/// no `avail_out` qualifier — because [`output_slice`] would otherwise mask a
/// null-and-empty output buffer into an empty slice. Either violation is a
/// programmer error the shims report as `Z_STREAM_ERROR` *before* any buffer is
/// dereferenced.
#[inline]
#[must_use]
pub fn stream_buffers_valid(strm: &z_stream) -> bool {
    !strm.next_out.is_null() && input_ptr_valid(strm)
}

/// Advances the input cursor after `consumed` bytes were read: bumps
/// [`z_stream::next_in`], decrements [`z_stream::avail_in`], and accumulates
/// [`z_stream::total_in`].
///
/// # Safety
///
/// `consumed` must not exceed [`z_stream::avail_in`], and the resulting
/// `next_in` must stay within the caller's input buffer.
#[inline]
pub unsafe fn advance_input(strm: &mut z_stream, consumed: usize) {
    // SAFETY: `consumed <= avail_in`, so the offset stays within the input
    // buffer the caller guaranteed for `next_in`.
    strm.next_in = unsafe { strm.next_in.add(consumed) };
    strm.avail_in -= consumed as c_uint;
    strm.total_in = strm.total_in.wrapping_add(consumed as c_ulong);
}

/// Advances the output cursor after `produced` bytes were written: bumps
/// [`z_stream::next_out`], decrements [`z_stream::avail_out`], and accumulates
/// [`z_stream::total_out`].
///
/// # Safety
///
/// `produced` must not exceed [`z_stream::avail_out`], and the resulting
/// `next_out` must stay within the caller's output buffer.
#[inline]
pub unsafe fn advance_output(strm: &mut z_stream, produced: usize) {
    // SAFETY: `produced <= avail_out`, so the offset stays within the output
    // buffer the caller guaranteed for `next_out`.
    strm.next_out = unsafe { strm.next_out.add(produced) };
    strm.avail_out -= produced as c_uint;
    strm.total_out = strm.total_out.wrapping_add(produced as c_ulong);
}

/// Writes the running checksum into [`z_stream::adler`].
#[inline]
pub fn set_adler(strm: &mut z_stream, adler: u32) {
    strm.adler = adler as c_ulong;
}

/// Writes the data-type guess / inflate decode state into
/// [`z_stream::data_type`].
#[inline]
pub fn set_data_type(strm: &mut z_stream, dt: i32) {
    strm.data_type = dt as c_int;
}

/// Points [`z_stream::msg`] at a static, NUL-terminated diagnostic string (or
/// null to clear it).
///
/// The message is a crate-owned `&'static` C string; C must never free it.
#[inline]
pub fn set_msg(strm: &mut z_stream, msg: *const c_char) {
    strm.msg = msg as *mut c_char;
}

// --- `gz_header` conversion ------------------------------------------------

/// Reads a NUL-terminated C string starting at `ptr` into an owned byte vector,
/// **excluding** the terminating NUL.
///
/// # Safety
///
/// `ptr` must be non-null and point at a NUL-terminated sequence of bytes that
/// stays valid for the duration of the read.
unsafe fn cstr_bytes(ptr: *const c_uchar) -> Vec<u8> {
    let mut len = 0usize;
    // SAFETY: per the contract, `ptr` points at a NUL-terminated string, so
    // every `ptr.add(len)` up to and including the terminator is readable.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: bytes `ptr[0..len]` precede the NUL and are therefore readable.
    unsafe { slice::from_raw_parts(ptr, len) }.to_vec()
}

/// Converts a raw [`gz_header`] (as passed to `deflateSetHeader`) into the
/// idiomatic [`GzHeader`], or [`None`] when `head`
/// is null.
///
/// The [`extra`](gz_header::extra) field is copied using its
/// [`extra_len`](gz_header::extra_len); [`name`](gz_header::name) and
/// [`comment`](gz_header::comment) are read as NUL-terminated C strings (the
/// terminator is dropped). C `int` booleans map to Rust [`bool`].
///
/// # Safety
///
/// `head` must be null or point at a valid [`gz_header`]. When its `extra`
/// pointer is non-null it must be readable for `extra_len` bytes; when `name`/
/// `comment` are non-null they must be NUL-terminated.
#[must_use]
pub unsafe fn gz_header_to_idiomatic(head: *const gz_header) -> Option<GzHeader> {
    if head.is_null() {
        return None;
    }
    // SAFETY: `head` is non-null and, per the contract, points at a valid
    // `gz_header`.
    let h = unsafe { &*head };

    let extra = if h.extra.is_null() {
        None
    } else {
        // SAFETY: a non-null `extra` is readable for `extra_len` bytes
        // (deflateSetHeader contract).
        Some(unsafe { slice::from_raw_parts(h.extra, h.extra_len as usize) }.to_vec())
    };
    let name = if h.name.is_null() {
        None
    } else {
        // SAFETY: a non-null `name` is a NUL-terminated C string.
        Some(unsafe { cstr_bytes(h.name) })
    };
    let comment = if h.comment.is_null() {
        None
    } else {
        // SAFETY: a non-null `comment` is a NUL-terminated C string.
        Some(unsafe { cstr_bytes(h.comment) })
    };

    Some(GzHeader {
        text: h.text != 0,
        time: h.time as u32,
        // `xflags`/`os` are C `int` (== `i32`), assigned directly.
        xflags: h.xflags,
        os: h.os,
        extra,
        name,
        comment,
        hcrc: h.hcrc != 0,
        // C tri-state `done`: only `1` means "header fully read".
        done: h.done == 1,
        // `c_uint` is `u32` on every Rust target, so these assign directly. The
        // caller's `extra_len` is *not* carried into a field of `GzHeader` — it is
        // consumed above to size the `extra` copy, which is its only role in this
        // direction, and on the read-back path the decoder reports the stream's
        // declared `XLEN` through `HeaderPublication::extra_len` instead.
        extra_max: h.extra_max,
        name_max: h.name_max,
        comm_max: h.comm_max,
    })
}

/// Writes an idiomatic [`GzHeader`] back into a caller-provided raw
/// [`gz_header`] (as used by `inflateGetHeader`), honoring the caller's
/// `extra_max`/`name_max`/`comm_max` capacities and never overrunning the
/// caller's buffers.
///
/// This is the **bulk** publisher, for callers holding a header that is already
/// complete and no record of how it was parsed. It publishes every field C
/// assigns, copies each captured buffer from offset `0`, and NUL-terminates
/// `name`/`comment` when the capacity leaves room — exactly what reference zlib
/// has done by the time it sets `head->done = 1`.
///
/// One field is deliberately left alone: the extra field's **declared** `XLEN`
/// (C `head->extra_len`). It is a decoder observation that cannot be recovered
/// from a finished [`GzHeader`] — `extra.len()` is the *captured* count, which is
/// smaller precisely in the truncation case the field exists to report — so
/// inventing a value here would destroy the caller's `extra_len > extra_max`
/// truncation signal. The decoder path publishes the real `XLEN` through
/// `publish_gz_header` instead.
///
/// While a stream is still being decoded, use `publish_gz_header`: C assigns
/// each field inside its own parser state, and a bulk write would zero scalars
/// the stream has not reached and terminate a half-received name.
///
/// # Safety
///
/// `head` must be null or point at a valid [`gz_header`]. When its `extra`/
/// `name`/`comment` pointers are non-null, each must address at least
/// `extra_max`/`name_max`/`comm_max` writable bytes respectively.
pub unsafe fn write_gz_header_from_idiomatic(head: *mut gz_header, src: &GzHeader) {
    // SAFETY: forwards this function's own contract unchanged.
    unsafe { publish_gz_header(head, src, &HeaderPublication::for_completed_header(src)) }
}

/// Publishes into a caller-provided raw [`gz_header`] exactly the assignments
/// `published` records — reference zlib's *incremental* gzip-header schedule.
///
/// # Why the schedule matters
///
/// C never mirrors a header wholesale. Each field is written inside the parser
/// state that decodes it, straight into the caller's struct and buffers, so a
/// caller polling between `inflate` calls sees only what the stream has actually
/// delivered and keeps its own values everywhere else. Reproducing that requires
/// three things a bulk copy cannot do:
///
/// * **Unreached scalars stay untouched.** `text`/`time`/`xflags`/`os`/`hcrc` are
///   written only once their own state has run (`inflate.c` `FLAGS`/`TIME`/`OS`/
///   `HCRC`). Writing them eagerly would overwrite a caller's sentinels with
///   zeros before the header carried any value at all.
/// * **`done` is tri-state.** `-1` ("this stream has no gzip header", assigned in
///   the `HEAD` non-gzip branch) is not expressible as a [`bool`], so it arrives
///   through `HeaderDone`. `0` is written by `inflateGetHeader` at registration
///   and is never re-published here.
/// * **The name/comment NUL is a decoded byte, not a formatting flourish.** C
///   stores the field's terminating NUL only when it actually reads it *and* it
///   fits within `name_max`/`comm_max` — a name that exactly fills the buffer is
///   left unterminated, and a name still arriving has no terminator yet.
///
/// Buffer bytes are written at the offset C wrote them: the record carries how
/// many bytes this call appended, and the owned `Vec`'s new length gives the end,
/// so the destination offset is `len - stored`. Every write is additionally
/// clamped to the caller's declared capacity, so a hand-built [`GzHeader`] whose
/// vectors exceed `*_max` truncates instead of overrunning.
///
/// # Safety
///
/// `head` must be null or point at a valid [`gz_header`]. When its `extra`/
/// `name`/`comment` pointers are non-null, each must address at least
/// `extra_max`/`name_max`/`comm_max` writable bytes respectively.
pub(crate) unsafe fn publish_gz_header(
    head: *mut gz_header,
    src: &GzHeader,
    published: &HeaderPublication,
) {
    if head.is_null() {
        return;
    }
    // SAFETY: `head` is non-null and, per the contract, points at a valid,
    // uniquely-borrowed `gz_header` whose buffers are sized by its `*_max`
    // fields.
    let h = unsafe { &mut *head };

    // --- scalars: each written only by the state that assigns it in C ---------
    if published.text {
        h.text = c_int::from(src.text);
    }
    if published.time {
        h.time = src.time as c_ulong;
    }
    if published.os {
        // C assigns `xflags` and `os` together under one guard (`inflate.c`
        // L539-L542), so one flag covers both.
        h.xflags = src.xflags;
        h.os = src.os;
    }
    if published.hcrc {
        h.hcrc = c_int::from(src.hcrc);
    }
    if let Some(done) = published.done {
        // The tri-state reaches the caller verbatim: `-1` for "not a gzip header",
        // `1` for "header complete".
        h.done = done.as_c_int() as c_int;
    }

    // --- extra ---------------------------------------------------------------
    if let Some(declared_xlen) = published.extra_len {
        // The stream's declared 16-bit `XLEN`, published unconditionally and
        // *unclamped* — gated on neither `extra`'s nullity nor `extra_max`'s size
        // (`inflate.c` L599-L600), while the copy below is separately clamped
        // (L614-L621). Reproducing both halves is what makes
        // `extra_len > extra_max` a usable truncation signal per `zlib.h`, and
        // what lets a caller pass a null `extra` purely to learn the length.
        h.extra_len = declared_xlen as c_uint;
    }
    if published.extra_null {
        // C's no-`FEXTRA` branch: `state->head->extra = Z_NULL` (`inflate.c`
        // L605-L606). That assignment is how a C caller distinguishes "the header
        // declared no extra field" from "it declared one"; leaving a stale
        // non-null pointer would misreport an absent field as present.
        h.extra = ptr::null_mut();
    }
    if published.extra_stored != 0 {
        if let Some(extra) = &src.extra {
            // SAFETY: bounded by `extra_max`; see `copy_tail_bounded`.
            unsafe {
                copy_tail_bounded(h.extra, extra, published.extra_stored, h.extra_max as usize);
            }
        }
    }

    // --- name ----------------------------------------------------------------
    if published.name_null {
        // C's no-`FNAME` branch: `state->head->name = Z_NULL` (`inflate.c`
        // L643-L644).
        h.name = ptr::null_mut();
    }
    if let Some(name) = &src.name {
        let cap = h.name_max as usize;
        if published.name_stored != 0 {
            // SAFETY: bounded by `name_max`; see `copy_tail_bounded`.
            unsafe {
                copy_tail_bounded(h.name, name, published.name_stored, cap);
            }
        }
        if published.name_terminated && !h.name.is_null() && name.len() < cap {
            // SAFETY: `h.name` has `name_max` writable bytes and
            // `name.len() < cap`, so this byte is inside the buffer. C stores the
            // NUL at `head->name[state->length]`, and `state->length` is exactly
            // the number of content bytes captured so far.
            unsafe {
                *h.name.add(name.len()) = 0;
            }
        }
    }

    // --- comment -------------------------------------------------------------
    if published.comment_null {
        // C's no-`FCOMMENT` branch: `state->head->comment = Z_NULL` (`inflate.c`
        // L665-L666).
        h.comment = ptr::null_mut();
    }
    if let Some(comment) = &src.comment {
        let cap = h.comm_max as usize;
        if published.comment_stored != 0 {
            // SAFETY: bounded by `comm_max`; see `copy_tail_bounded`.
            unsafe {
                copy_tail_bounded(h.comment, comment, published.comment_stored, cap);
            }
        }
        if published.comment_terminated && !h.comment.is_null() && comment.len() < cap {
            // SAFETY: `h.comment` has `comm_max` writable bytes and
            // `comment.len() < cap`, so this byte is inside the buffer.
            unsafe {
                *h.comment.add(comment.len()) = 0;
            }
        }
    }
}

/// Copies the last `stored` bytes of `src` into `dst` at the offset they occupy
/// in `src`, clamped to `cap` writable bytes; a null `dst` or a zero `cap` is a
/// no-op.
///
/// This is the byte-placement rule C uses for `head->extra`, `head->name`, and
/// `head->comment`: it writes each decoded byte at the running index it was
/// captured at, so a field delivered across several `inflate` calls lands
/// contiguously without any call rewriting an earlier call's bytes.
///
/// # Safety
///
/// `dst` must be null or point at a buffer of at least `cap` writable bytes.
unsafe fn copy_tail_bounded(dst: *mut c_uchar, src: &[u8], stored: usize, cap: usize) {
    if dst.is_null() || cap == 0 {
        return;
    }
    // `stored` can never exceed `src.len()` for a record the decoder produced;
    // `saturating_sub` keeps a hand-built record in bounds instead of wrapping.
    let offset = src.len().saturating_sub(stored);
    if offset >= cap {
        return;
    }
    let n = core::cmp::min(src.len() - offset, cap - offset);
    // SAFETY: `offset < cap` and `offset + n <= cap`, so the destination range
    // `dst[offset..offset + n]` lies inside the caller's `cap` writable bytes;
    // `src[offset..offset + n]` is in bounds because `offset + n <= src.len()`;
    // and the two buffers are distinct (the source is this crate's owned `Vec`).
    unsafe {
        ptr::copy_nonoverlapping(src.as_ptr().add(offset), dst.add(offset), n);
    }
}

// --- Panic guards ----------------------------------------------------------
//
// A Rust panic must never unwind across the C ABI (that is undefined behavior).
// Every fallible shim body wraps its logic in one of these guards, which run
// the closure under `catch_unwind` (when `std` is available) and substitute a
// safe default return value if it panics. Under `no_std` (where `catch_unwind`
// is unavailable and builds typically use `panic = "abort"`) the closure runs
// directly. The helpers are `pub(crate)` and consumed by the sibling shim
// files, hence `#[allow(dead_code)]` for standalone compilation of this module.

/// Runs `f`, returning its `c_int` result, or `default` if it panics.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_int(
    default: c_int,
    f: impl FnOnce() -> c_int + core::panic::UnwindSafe,
) -> c_int {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_int(
    _default: c_int,
    f: impl FnOnce() -> c_int + core::panic::UnwindSafe,
) -> c_int {
    f()
}

/// Runs `f`, returning its `c_ulong` result, or `default` if it panics.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_ulong(
    default: c_ulong,
    f: impl FnOnce() -> c_ulong + core::panic::UnwindSafe,
) -> c_ulong {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_ulong(
    _default: c_ulong,
    f: impl FnOnce() -> c_ulong + core::panic::UnwindSafe,
) -> c_ulong {
    f()
}

/// Runs `f`, returning its `*mut T` result, or `default` if it panics.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_ptr<T>(
    default: *mut T,
    f: impl FnOnce() -> *mut T + core::panic::UnwindSafe,
) -> *mut T {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_ptr<T>(
    _default: *mut T,
    f: impl FnOnce() -> *mut T + core::panic::UnwindSafe,
) -> *mut T {
    f()
}

/// Runs `f`, returning its [`z_off64_t`] result, or `default` if it panics.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_off(
    default: z_off64_t,
    f: impl FnOnce() -> z_off64_t + core::panic::UnwindSafe,
) -> z_off64_t {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_off(
    _default: z_off64_t,
    f: impl FnOnce() -> z_off64_t + core::panic::UnwindSafe,
) -> z_off64_t {
    f()
}

// --- Shared test utility for the panic guards ------------------------------
//
// Declared at module scope (not inside `mod tests`) and `pub(crate)` so the
// sibling shim files' test modules — `src/ffi/gz.rs` has two guards of its own
// for the `z_size_t` / `*const T` widths this module does not cover — reach the
// same serialization primitive. Without a single shared lock, two test modules
// swapping the process-global panic hook concurrently can interleave.

/// Serializes every test that installs a scoped panic hook.
///
/// `std::panic::take_hook` / `set_hook` mutate **process-global** state and the
/// test harness runs unit tests on several threads, so two tests swapping the
/// hook concurrently can interleave — leaving the noisy default hook installed
/// for a deliberate panic, or restoring a hook the other test had taken. This
/// lock makes each take/run/restore sequence atomic with respect to the others.
#[cfg(all(test, feature = "std"))]
static PANIC_HOOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `f` with the default panic hook silenced and the previous hook restored
/// afterwards, serialized through `PANIC_HOOK_LOCK`.
///
/// Every panic-guard test provokes a real panic inside a guard; without this the
/// default hook would print a backtrace per deliberate panic and make a passing
/// run read like a failing one. Poisoning is tolerated
/// ([`std::sync::PoisonError::into_inner`]) because these tests exist to provoke
/// panics: a poisoned lock must not cascade into unrelated failures. `f` itself
/// is not expected to unwind — the guard under test catches the panic and
/// returns its default — so the restore is a plain sequential statement rather
/// than a drop guard.
#[cfg(all(test, feature = "std"))]
pub(crate) fn with_silenced_panic_hook<R>(f: impl FnOnce() -> R) -> R {
    let _lock = PANIC_HOOK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let out = f();
    std::panic::set_hook(prev);
    out
}

// ===========================================================================
// Phase 10 — Compile-time ABI layout guards
// ===========================================================================
//
// These `const` assertions fail the build if the mirror structs ever lose their
// expected shape. Field-offset ordering is checked exhaustively in the tests
// below via `core::mem::offset_of!`.

const _: () = {
    // The mirror structs must be non-zero-sized aggregates.
    assert!(core::mem::size_of::<z_stream>() > 0);
    assert!(core::mem::size_of::<gz_header>() > 0);

    // `z_stream` begins with `next_in`, a pointer, so it must be pointer-aligned.
    assert!(core::mem::align_of::<z_stream>() == core::mem::align_of::<*const c_void>());

    // `gzFile_s` = { u32 have; ptr next; i64 pos } — at least a pointer plus the
    // 8-byte `pos` beyond `next`'s offset.
    assert!(core::mem::size_of::<gzFile_s>() >= core::mem::size_of::<*mut c_uchar>() + 8);

    // A nullable C function pointer must stay pointer-sized (null-pointer
    // optimization), or the `#[repr(C)]` structs above would not match `libz`.
    assert!(core::mem::size_of::<alloc_func>() == core::mem::size_of::<*const c_void>());
    assert!(core::mem::size_of::<free_func>() == core::mem::size_of::<*const c_void>());
};

// Exact numeric ABI layout guard for the primary supported target family:
// 64-bit **LP64** (Linux/macOS on `x86_64`/`aarch64`), where `c_ulong` (C
// `uLong`) is 8 bytes. On this ABI the mirror structs must match `libz`'s
// `z_stream`/`gz_header`/`gzFile_s` byte for byte, so every field offset and the
// total size is pinned below; any drift fails the build.
//
// Windows x64 (LLP64, `c_ulong` = 4) and 32-bit targets carry a different
// `uLong` width and therefore different offsets, so they are intentionally
// excluded from these *exact* checks — the ABI-portable ordering/size tests in
// the test module still apply to every target.
#[cfg(all(target_pointer_width = "64", not(windows)))]
const _: () = {
    use core::mem::{offset_of, size_of};

    // `z_stream` — 14 fields, 112 bytes total on LP64.
    assert!(offset_of!(z_stream, next_in) == 0);
    assert!(offset_of!(z_stream, avail_in) == 8);
    assert!(offset_of!(z_stream, total_in) == 16);
    assert!(offset_of!(z_stream, next_out) == 24);
    assert!(offset_of!(z_stream, avail_out) == 32);
    assert!(offset_of!(z_stream, total_out) == 40);
    assert!(offset_of!(z_stream, msg) == 48);
    assert!(offset_of!(z_stream, state) == 56);
    assert!(offset_of!(z_stream, zalloc) == 64);
    assert!(offset_of!(z_stream, zfree) == 72);
    assert!(offset_of!(z_stream, opaque) == 80);
    assert!(offset_of!(z_stream, data_type) == 88);
    assert!(offset_of!(z_stream, adler) == 96);
    assert!(offset_of!(z_stream, reserved) == 104);
    assert!(size_of::<z_stream>() == 112);

    // `gz_header` — 13 fields, 80 bytes total on LP64.
    assert!(offset_of!(gz_header, text) == 0);
    assert!(offset_of!(gz_header, time) == 8);
    assert!(offset_of!(gz_header, xflags) == 16);
    assert!(offset_of!(gz_header, os) == 20);
    assert!(offset_of!(gz_header, extra) == 24);
    assert!(offset_of!(gz_header, extra_len) == 32);
    assert!(offset_of!(gz_header, extra_max) == 36);
    assert!(offset_of!(gz_header, name) == 40);
    assert!(offset_of!(gz_header, name_max) == 48);
    assert!(offset_of!(gz_header, comment) == 56);
    assert!(offset_of!(gz_header, comm_max) == 64);
    assert!(offset_of!(gz_header, hcrc) == 68);
    assert!(offset_of!(gz_header, done) == 72);
    assert!(size_of::<gz_header>() == 80);

    // `gzFile_s` — `{ have, next, pos }`, 24 bytes total on LP64. This is the
    // live prefix the C `gzgetc(g)` macro dereferences, so its exact shape
    // matters for drop-in macro consumers.
    assert!(offset_of!(gzFile_s, have) == 0);
    assert!(offset_of!(gzFile_s, next) == 8);
    assert!(offset_of!(gzFile_s, pos) == 16);
    assert!(size_of::<gzFile_s>() == 24);
};

// ===========================================================================
// Phase 11 — Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{MaybeUninit, align_of, offset_of, size_of};

    use crate::stream::HookAllocator;

    /// The four C function-pointer typedefs must be pointer-sized, confirming
    /// the null-pointer optimization the `#[repr(C)]` structs rely on.
    #[test]
    fn fn_pointer_typedefs_are_pointer_sized() {
        let ptr = size_of::<*const c_void>();
        assert_eq!(size_of::<alloc_func>(), ptr);
        assert_eq!(size_of::<free_func>(), ptr);
        assert_eq!(size_of::<in_func>(), ptr);
        assert_eq!(size_of::<out_func>(), ptr);
        // `None` for these Options is the null address.
        let a: alloc_func = None;
        assert!(a.is_none());
    }

    /// `z_stream` field offsets must start at 0 and strictly increase in the
    /// declaration order the C ABI mandates.
    #[test]
    fn z_stream_field_offsets_are_ordered() {
        let offsets = [
            offset_of!(z_stream, next_in),
            offset_of!(z_stream, avail_in),
            offset_of!(z_stream, total_in),
            offset_of!(z_stream, next_out),
            offset_of!(z_stream, avail_out),
            offset_of!(z_stream, total_out),
            offset_of!(z_stream, msg),
            offset_of!(z_stream, state),
            offset_of!(z_stream, zalloc),
            offset_of!(z_stream, zfree),
            offset_of!(z_stream, opaque),
            offset_of!(z_stream, data_type),
            offset_of!(z_stream, adler),
            offset_of!(z_stream, reserved),
        ];
        assert_eq!(offsets[0], 0, "next_in must be the first field");
        for pair in offsets.windows(2) {
            assert!(
                pair[1] > pair[0],
                "z_stream fields must be laid out in declaration order"
            );
        }
        // Pointer-aligned aggregate.
        assert_eq!(align_of::<z_stream>(), align_of::<*const c_void>());
    }

    /// `gz_header` field offsets must start at 0 and strictly increase.
    #[test]
    fn gz_header_field_offsets_are_ordered() {
        let offsets = [
            offset_of!(gz_header, text),
            offset_of!(gz_header, time),
            offset_of!(gz_header, xflags),
            offset_of!(gz_header, os),
            offset_of!(gz_header, extra),
            offset_of!(gz_header, extra_len),
            offset_of!(gz_header, extra_max),
            offset_of!(gz_header, name),
            offset_of!(gz_header, name_max),
            offset_of!(gz_header, comment),
            offset_of!(gz_header, comm_max),
            offset_of!(gz_header, hcrc),
            offset_of!(gz_header, done),
        ];
        assert_eq!(offsets[0], 0, "text must be the first field");
        for pair in offsets.windows(2) {
            assert!(pair[1] > pair[0]);
        }
    }

    /// `gzFile_s` exposes `{ have, next, pos }` with `have` first, as the C
    /// `gzgetc` macro requires.
    #[test]
    fn gz_file_s_layout() {
        assert_eq!(offset_of!(gzFile_s, have), 0);
        assert!(offset_of!(gzFile_s, next) >= size_of::<c_uint>());
        assert!(offset_of!(gzFile_s, pos) > offset_of!(gzFile_s, next));
        // Must hold at least a pointer plus the 8-byte `pos`.
        assert!(size_of::<gzFile_s>() >= size_of::<*mut c_uchar>() + 8);
    }

    /// **Exact** numeric field offsets and total sizes on the supported
    /// 64-bit LP64 ABI (Linux/macOS on `x86_64`/`aarch64`), pinning the mirror
    /// structs to `libz`'s byte-for-byte layout. The compile-time `const _`
    /// guard in the module enforces the identical values at build time; this
    /// test surfaces them as visible coverage. Excluded on Windows LLP64
    /// (`c_ulong` = 4) and 32-bit targets, whose `uLong` width shifts offsets.
    #[cfg(all(target_pointer_width = "64", not(windows)))]
    #[test]
    fn abi_exact_field_offsets_lp64() {
        // z_stream — 112 bytes.
        assert_eq!(offset_of!(z_stream, next_in), 0);
        assert_eq!(offset_of!(z_stream, avail_in), 8);
        assert_eq!(offset_of!(z_stream, total_in), 16);
        assert_eq!(offset_of!(z_stream, next_out), 24);
        assert_eq!(offset_of!(z_stream, avail_out), 32);
        assert_eq!(offset_of!(z_stream, total_out), 40);
        assert_eq!(offset_of!(z_stream, msg), 48);
        assert_eq!(offset_of!(z_stream, state), 56);
        assert_eq!(offset_of!(z_stream, zalloc), 64);
        assert_eq!(offset_of!(z_stream, zfree), 72);
        assert_eq!(offset_of!(z_stream, opaque), 80);
        assert_eq!(offset_of!(z_stream, data_type), 88);
        assert_eq!(offset_of!(z_stream, adler), 96);
        assert_eq!(offset_of!(z_stream, reserved), 104);
        assert_eq!(size_of::<z_stream>(), 112);

        // gz_header — 80 bytes.
        assert_eq!(offset_of!(gz_header, text), 0);
        assert_eq!(offset_of!(gz_header, time), 8);
        assert_eq!(offset_of!(gz_header, xflags), 16);
        assert_eq!(offset_of!(gz_header, os), 20);
        assert_eq!(offset_of!(gz_header, extra), 24);
        assert_eq!(offset_of!(gz_header, extra_len), 32);
        assert_eq!(offset_of!(gz_header, extra_max), 36);
        assert_eq!(offset_of!(gz_header, name), 40);
        assert_eq!(offset_of!(gz_header, name_max), 48);
        assert_eq!(offset_of!(gz_header, comment), 56);
        assert_eq!(offset_of!(gz_header, comm_max), 64);
        assert_eq!(offset_of!(gz_header, hcrc), 68);
        assert_eq!(offset_of!(gz_header, done), 72);
        assert_eq!(size_of::<gz_header>(), 80);

        // gzFile_s — 24 bytes; the live `gzgetc` macro prefix.
        assert_eq!(offset_of!(gzFile_s, have), 0);
        assert_eq!(offset_of!(gzFile_s, next), 8);
        assert_eq!(offset_of!(gzFile_s, pos), 16);
        assert_eq!(size_of::<gzFile_s>(), 24);
    }

    /// `CAllocator` charges the engine-state footprint only when the caller
    /// actually installed a hook, so a C caller who left `zalloc`/`zfree` null
    /// keeps exactly the footprint they have always had (AAP §0.6.5).
    #[test]
    fn callocator_reserves_the_state_footprint_only_with_an_active_hook() {
        let null_hooks = CAllocator {
            zalloc: None,
            zfree: None,
            opaque: ptr::null_mut(),
        };
        assert!(
            !null_hooks.reserves_state_footprint(),
            "a hookless C caller must not be charged an extra state-sized region"
        );

        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let hook = stats.hook();
        let active = CAllocator {
            zalloc: hook.zalloc(),
            zfree: hook.zfree(),
            opaque: hook.opaque(),
        };
        assert!(
            active.reserves_state_footprint(),
            "an installed hook must be charged for the state, as C's ZALLOC is"
        );
    }

    /// The three published [`HandleKind`] magics must be pairwise distinct, and
    /// the tag must stay a `u64` sitting at offset 0 of every tagged handle.
    ///
    /// [`peek_handle_kind`] reads the discriminant through the C-layout
    /// [`HandleHeader`] prefix *before* any handle is reinterpreted, so a magic
    /// collision — or a width/offset drift — would silently defeat the
    /// cross-engine `End` guard and re-open the layout-mismatched deallocation
    /// that tagging exists to prevent.
    #[test]
    fn handle_kind_magics_are_distinct_and_u64_shaped() {
        // Pairwise distinctness across all three engines.
        assert_ne!(HandleKind::DEFLATE, HandleKind::INFLATE);
        assert_ne!(HandleKind::INFLATE, HandleKind::INFLATE_BACK);
        assert_ne!(HandleKind::DEFLATE, HandleKind::INFLATE_BACK);

        // `#[repr(transparent)]` over a `u64`: identical size and alignment, so
        // reading the tag through the `HandleHeader` prefix is exact.
        assert_eq!(size_of::<HandleKind>(), size_of::<u64>());
        assert_eq!(align_of::<HandleKind>(), align_of::<u64>());
        assert_eq!(size_of::<HandleHeader>(), size_of::<u64>());

        // The tag must lead the shared header AND every tagged handle — that is
        // precisely what lets `peek_handle_kind` read it without knowing the
        // concrete handle type behind the opaque `state` pointer.
        assert_eq!(offset_of!(HandleHeader, kind), 0);
        assert_eq!(offset_of!(DeflateHandle, kind), 0);
    }

    /// The three published [`HandleKind`] magics must be *these exact* `u64`
    /// values.
    ///
    /// Distinctness alone is not enough. Three magics that all drifted together —
    /// a copy-paste while adding a fourth engine, a mechanical renumbering — stay
    /// pairwise distinct and stay `u64`-shaped, so
    /// [`handle_kind_magics_are_distinct_and_u64_shaped`] keeps passing while the
    /// tag values themselves have silently changed. Nothing else in the crate
    /// notices, because every producer and consumer reads the same constants:
    /// the drift is *internally self-consistent*.
    ///
    /// It is still observable. A handle installed by one build of this library and
    /// terminated by another — a `cdylib` swapped underneath a running consumer,
    /// or a `staticlib` linked alongside a differently-versioned copy — would fail
    /// its tag check and report `Z_STREAM_ERROR` from a perfectly valid `*End`.
    /// Pinning the literals makes any such change a deliberate, visible edit.
    ///
    /// The high-bit property the [`HandleKind`] documentation claims is asserted
    /// alongside them: each magic exceeds `u32::MAX`, so it can never collide with
    /// the small integer that leads a bare, untagged engine state.
    #[test]
    fn handle_kind_magics_are_the_published_literals() {
        assert_eq!(
            HandleKind::DEFLATE.0,
            0xDEF1_A7E5_0DEF_0001,
            "the DEFLATE handle tag is part of the compiled ABI and must not drift"
        );
        assert_eq!(
            HandleKind::INFLATE.0,
            0x14F1_A7E5_0114_0002,
            "the INFLATE handle tag is part of the compiled ABI and must not drift"
        );
        assert_eq!(
            HandleKind::INFLATE_BACK.0,
            0x14F1_A7E5_BAC6_0003,
            "the INFLATE_BACK handle tag is part of the compiled ABI and must not \
             drift"
        );

        for (name, kind) in [
            ("DEFLATE", HandleKind::DEFLATE),
            ("INFLATE", HandleKind::INFLATE),
            ("INFLATE_BACK", HandleKind::INFLATE_BACK),
        ] {
            assert!(
                kind.0 > u64::from(u32::MAX),
                "the {name} tag must keep its high bits set so it cannot collide \
                 with the small leading integer of an untagged engine state"
            );
        }
    }

    /// The tag gates `deflate_state`/`deflate_take`: a foreign engine's handle is
    /// rejected WITHOUT reconstituting a wrong-type box, and `state` is left
    /// intact so nothing is ever dropped through a mismatched `Layout`.
    #[test]
    fn handle_tag_gates_deflate_state_and_take() {
        // No handle installed: every accessor reports absence without touching
        // the null pointer.
        let mut strm = zeroed_stream();
        // SAFETY: `state` is null, so no dereference occurs.
        assert!(unsafe { peek_handle_kind(&strm) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { deflate_state(&mut strm) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { deflate_take(&mut strm) }.is_none());

        // Install a genuine, correctly tagged deflate handle.
        let handle = Box::new(DeflateHandle::new(ZStream::with_allocator(CAllocator {
            zalloc: None,
            zfree: None,
            opaque: ptr::null_mut(),
        })));
        // SAFETY: transfers ownership of a freshly boxed handle into `state`;
        // it is reclaimed exactly once by the `deflate_take` at the end.
        strm.state = unsafe { state_ptr_from_box(handle) };
        // SAFETY: `state` holds the live `Box<DeflateHandle>` installed above,
        // whose first field is the `HandleKind` tag.
        assert_eq!(
            unsafe { peek_handle_kind(&strm) },
            Some(HandleKind::DEFLATE)
        );
        // SAFETY: as above; the tag confirms a `DeflateHandle`.
        assert!(unsafe { deflate_state(&mut strm) }.is_some());

        // Overwrite the tag with a foreign engine's magic, emulating a caller
        // that passes an inflate-initialized stream to `deflateEnd`.
        // SAFETY: `state` points at a live, C-layout `DeflateHandle` whose first
        // field is a `HandleKind`, and `HandleHeader` is a C-layout struct with
        // the same leading field, so this writes exactly that field and no other.
        unsafe {
            (*(strm.state as *mut HandleHeader)).kind = HandleKind::INFLATE;
        }
        // SAFETY: `state` still points at the live handle allocation, so the tag
        // remains readable.
        assert!(unsafe { deflate_state(&mut strm) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { deflate_take(&mut strm) }.is_none());
        assert!(
            !strm.state.is_null(),
            "a rejected take must leave `state` installed, never dropping a \
             wrong-type box"
        );

        // Restore the correct tag and reclaim the box, so the handle is freed
        // through its real type and the test leaks nothing.
        // SAFETY: as for the corrupting write above — same allocation, same
        // leading field.
        unsafe {
            (*(strm.state as *mut HandleHeader)).kind = HandleKind::DEFLATE;
        }
        // SAFETY: the tag again confirms the live `Box<DeflateHandle>`, which is
        // reconstituted exactly once here.
        assert!(unsafe { deflate_take(&mut strm) }.is_some());
        assert!(
            strm.state.is_null(),
            "a successful take must null `state` to prevent a double free"
        );
    }

    /// With null hooks, `CAllocator` falls back to the global allocator and
    /// produces zeroed buffers that round-trip through `deallocate` without UB.
    #[test]
    fn callocator_null_hooks_use_global_allocator() {
        let alloc = CAllocator {
            zalloc: None,
            zfree: None,
            opaque: ptr::null_mut(),
        };

        let bytes: AllocBuffer<u8> = alloc
            .allocate_zeroed(8)
            .expect("global allocation is infallible");
        assert_eq!(bytes.len(), 8);
        assert!(bytes.iter().all(|&b| b == 0));
        alloc.deallocate(bytes);

        let words: AllocBuffer<u32> = alloc
            .allocate_zeroed(4)
            .expect("global allocation is infallible");
        assert_eq!(&words[..], &[0u32; 4][..]);
        alloc.deallocate(words);

        // A zero-length request yields an empty buffer.
        let empty: AllocBuffer<u16> = alloc
            .allocate_zeroed(0)
            .expect("global allocation is infallible");
        assert!(empty.is_empty());
        alloc.deallocate(empty);
    }

    /// With active hooks, `CAllocator::allocate_zeroed` routes through the
    /// caller's `zalloc` (a `Foreign` buffer, zero-filled and usable as a
    /// slice) and releases it through the caller's `zfree` on drop — the
    /// has-hook clause (AAP §0.6.3). Invocation counts are recorded via a global
    /// (`static`) counter because the C hooks receive only the `opaque` cookie.
    #[test]
    fn callocator_active_hooks_are_invoked_and_balanced() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        static ALLOCS: AtomicUsize = AtomicUsize::new(0);
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn zalloc(
            _opaque: *mut c_void,
            items: c_uint,
            size: c_uint,
        ) -> *mut c_void {
            ALLOCS.fetch_add(1, Ordering::SeqCst);
            let bytes = (items as usize) * (size as usize);
            // Delegate the actual bytes to the Rust global allocator; a
            // `Vec<u8>` we forget and later reclaim in `zfree`.
            let mut v = alloc::vec![0u8; bytes.max(1)];
            let p = v.as_mut_ptr();
            core::mem::forget(v);
            p.cast()
        }
        unsafe extern "C" fn zfree(_opaque: *mut c_void, address: *mut c_void) {
            FREES.fetch_add(1, Ordering::SeqCst);
            // We cannot recover the exact length here, so this test backing
            // store intentionally leaks the bytes (the process ends promptly);
            // the assertion of interest is that `zfree` is invoked exactly once
            // per `zalloc`. This keeps the test allocator trivially sound.
            let _ = address;
        }

        let alloc = CAllocator {
            zalloc: Some(zalloc),
            zfree: Some(zfree),
            opaque: ptr::null_mut(),
        };
        assert!(alloc.hook().is_active());

        {
            let buf: AllocBuffer<u16> = alloc
                .allocate_zeroed(32)
                .expect("active-hook zalloc returns non-null");
            assert_eq!(buf.len(), 32);
            // Foreign region is zero-filled and usable as a slice.
            assert!(buf.iter().all(|&w| w == 0));
        } // <- drop routes through the caller's `zfree`

        assert_eq!(ALLOCS.load(Ordering::SeqCst), 1, "zalloc must be invoked");
        assert_eq!(FREES.load(Ordering::SeqCst), 1, "zfree must balance zalloc");
    }

    /// Regression guard: an **active** allocator hook whose `zalloc` reports
    /// out-of-memory (returns `NULL`) must make `allocate_zeroed` yield
    /// [`None`] — surfaced as `Z_MEM_ERROR` at the engine init paths — rather
    /// than silently falling back to the Rust global allocator. A zero-count
    /// request still succeeds through the owned fast path (the hook is never
    /// consulted).
    #[test]
    fn callocator_active_hook_oom_returns_none_no_global_fallback() {
        unsafe extern "C" fn oom_zalloc(
            _opaque: *mut c_void,
            _items: c_uint,
            _size: c_uint,
        ) -> *mut c_void {
            // Report out-of-memory unconditionally.
            ptr::null_mut()
        }
        unsafe extern "C" fn noop_zfree(_opaque: *mut c_void, _address: *mut c_void) {}

        let alloc = CAllocator {
            zalloc: Some(oom_zalloc),
            zfree: Some(noop_zfree),
            opaque: ptr::null_mut(),
        };
        assert!(alloc.hook().is_active());

        // A non-empty request through the OOM hook must fail with `None`; there
        // is deliberately NO global-allocator fallback.
        let buf: Option<AllocBuffer<u8>> = alloc.allocate_zeroed(64);
        assert!(
            buf.is_none(),
            "active-hook OOM must yield None, not a global-allocator Vec"
        );

        // The zero-count fast path never consults the hook and still succeeds.
        let empty: AllocBuffer<u8> = alloc
            .allocate_zeroed(0)
            .expect("empty request uses the owned fast path");
        assert!(empty.is_empty());
    }

    /// Out-of-memory propagation: a real engine init (`DeflateState::new_in`) driven by an
    /// active OOM hook surfaces [`ZlibError::MemError`] (→ `Z_MEM_ERROR`) instead
    /// of panicking or silently succeeding on the global allocator. This proves
    /// the `AllocBuffer::try_zeroed(..).ok_or(ZlibError::MemError)?` chain in
    /// `new_in` is wired end to end.
    #[test]
    fn deflate_new_in_surfaces_mem_error_on_active_hook_oom() {
        use crate::constants::{Strategy, Z_DEFLATED};
        use crate::deflate::state::DeflateState;
        use crate::error::ZlibError;

        unsafe extern "C" fn oom_zalloc(
            _opaque: *mut c_void,
            _items: c_uint,
            _size: c_uint,
        ) -> *mut c_void {
            ptr::null_mut()
        }
        unsafe extern "C" fn noop_zfree(_opaque: *mut c_void, _address: *mut c_void) {}

        let hook = AllocHook::new(Some(oom_zalloc), Some(noop_zfree), ptr::null_mut());
        assert!(hook.is_active());

        // Valid parameters (level 6, deflate method, 15-bit window, mem level 8,
        // default strategy, zlib wrap): the ONLY reason this can fail is the OOM
        // hook, so a `MemError` proves the out-of-memory propagation path.
        let result = DeflateState::new_in(hook, 6, Z_DEFLATED, 15, 8, Strategy::Default, 1);
        assert!(
            matches!(result, Err(ZlibError::MemError)),
            "active-hook OOM at init must surface Z_MEM_ERROR, got {:?}",
            result.as_ref().map(|_| "Ok(state)")
        );
    }

    /// A test-only `zalloc`/`zfree` pair backed by the Rust global allocator.
    ///
    /// Each block is prefixed with a `usize` size header (the same technique
    /// `tests/inflate_coverage.rs` uses) so `zfree` can reconstruct the layout
    /// and actually release the memory instead of leaking. The header size is
    /// also the alignment, which keeps every returned pointer `usize`-aligned —
    /// enough for the `u8`/`u16` element types the engines request, for the `u32`
    /// buffers the tests below allocate, and for the 4-byte-aligned probe type
    /// used below.
    const HOOK_HEADER: usize = size_of::<usize>();

    /// Allocates `bytes` usable bytes through the global allocator with a size
    /// header, returning the pointer just past the header (or null).
    fn hook_backing_alloc(bytes: usize) -> *mut c_void {
        if bytes == 0 {
            return ptr::null_mut();
        }
        let total = bytes + HOOK_HEADER;
        let Ok(layout) = core::alloc::Layout::from_size_align(total, HOOK_HEADER) else {
            return ptr::null_mut();
        };
        // SAFETY: `layout` has a non-zero size, which is `alloc`'s requirement.
        let raw = unsafe { alloc::alloc::alloc(layout) };
        if raw.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `raw` owns `total` >= `HOOK_HEADER` bytes aligned for `usize`,
        // so writing the header at offset 0 is in bounds and well aligned.
        unsafe { *(raw as *mut usize) = total };
        // SAFETY: the usable region starts one header inside the same allocation.
        unsafe { raw.add(HOOK_HEADER) as *mut c_void }
    }

    /// Releases a block produced by [`hook_backing_alloc`].
    fn hook_backing_free(address: *mut c_void) {
        if address.is_null() {
            return;
        }
        // SAFETY: `address` came from `hook_backing_alloc`, so its `usize` size
        // header sits in the `HOOK_HEADER` bytes immediately before it; stepping
        // back stays inside that same allocation.
        let raw = unsafe { (address as *mut u8).sub(HOOK_HEADER) };
        // SAFETY: `raw` points at the header written by `hook_backing_alloc`.
        let total = unsafe { *(raw as *const usize) };
        let layout =
            core::alloc::Layout::from_size_align(total, HOOK_HEADER).expect("header is valid");
        // SAFETY: `raw`/`layout` are exactly the pointer and layout the matching
        // `hook_backing_alloc` allocated.
        unsafe { alloc::alloc::dealloc(raw, layout) };
    }

    /// Soundness regression: a foreign region must be initialized with
    /// `T::default()` for **every** element, not with a raw zero fill.
    ///
    /// `Copy + Default` does **not** promise that an all-zero bit pattern is an
    /// inhabited — let alone the default — value of `T`, so writing zeros and
    /// then exposing the region as `&[T]` would be unsound for any such type and
    /// silently wrong for one whose `Default` is non-zero.
    ///
    /// Two independent remedies are in force and this test covers both.
    ///
    /// 1. The allocation path is initialized by *writing* `T::default()` values.
    ///    That is [`fill_default`], exercised here for a `Copy + Default` element
    ///    type whose default is `0xDEAD_BEEF` over a region pre-poisoned with
    ///    `0xAAAA_AAAA`. The poison is what makes the assertion meaningful: a
    ///    `write_bytes(0)` implementation would produce `Probe(0)`, and an
    ///    implementation that forgot to initialize at all would leave
    ///    `Probe(0xAAAA_AAAA)`.
    /// 2. The element-type set reaching that path is additionally sealed by
    ///    `ZeroValid`, so a type like `Probe` cannot be requested through
    ///    `AllocBuffer::try_zeroed` at all — the `compile_fail` doctest on
    ///    `crate::stream::ZeroValid` pins that. The engines' own element types are
    ///    checked below end-to-end through a hook that hands back a region
    ///    pre-filled with `0xAA` bytes, so a missing initialization would be
    ///    visible there too.
    #[test]
    fn foreign_buffer_initializes_elements_to_default_not_zero() {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        struct Probe(u32);

        impl Default for Probe {
            fn default() -> Self {
                Probe(0xDEAD_BEEF)
            }
        }

        unsafe extern "C" fn dirty_zalloc(
            _opaque: *mut c_void,
            items: c_uint,
            size: c_uint,
        ) -> *mut c_void {
            let bytes = (items as usize) * (size as usize);
            let p = hook_backing_alloc(bytes);
            if p.is_null() {
                return p;
            }
            // Poison the region so an uninitialized or zero-filled result is
            // distinguishable from a correctly `Default`-initialized one.
            // SAFETY: `p` owns `bytes` writable bytes.
            unsafe { ptr::write_bytes(p as *mut u8, 0xAA, bytes) };
            p
        }
        unsafe extern "C" fn plain_zfree(_opaque: *mut c_void, address: *mut c_void) {
            hook_backing_free(address);
        }

        // (1) The initializer writes values, not bytes — over a poisoned region.
        let mut slots = [MaybeUninit::new(Probe(0xAAAA_AAAA)); 8];
        crate::ffi::alloc::fill_default(&mut slots);
        for slot in &slots {
            // SAFETY: `fill_default` wrote a valid `Probe` into every slot above,
            // and `Probe` is `Copy`, so reading one out leaves the slot intact.
            let value = unsafe { slot.assume_init() };
            assert_eq!(
                value,
                Probe::default(),
                "every element must be initialized to T::default()"
            );
            assert_ne!(value, Probe(0), "a byte-zeroing fill must not pass");
            assert_ne!(value, Probe(0xAAAA_AAAA), "the poison must be overwritten");
        }

        // (2) End-to-end through the safe entry point, for the sealed element
        // types the engines actually request: they keep exact zero-fill parity
        // with C `zcalloc` because their `Default` *is* the all-zero pattern, and
        // the `0xAA` poison would be visible if the fill were skipped.
        let hook = AllocHook::new(Some(dirty_zalloc), Some(plain_zfree), ptr::null_mut());
        assert!(hook.is_active());

        let bytes: AllocBuffer<u8> =
            AllocBuffer::try_zeroed(24, hook).expect("healthy hook allocates");
        assert!(
            bytes.is_foreign(),
            "an active hook must produce a Foreign arm"
        );
        assert_eq!(bytes.len(), 24);
        assert!(bytes.iter().all(|&b| b == 0));
        let words: AllocBuffer<u16> =
            AllocBuffer::try_zeroed(12, hook).expect("healthy hook allocates");
        assert!(words.iter().all(|&w| w == 0));
        let longs: AllocBuffer<u32> =
            AllocBuffer::try_zeroed(6, hook).expect("healthy hook allocates");
        assert!(longs.iter().all(|&l| l == 0));
    }

    /// Geometry regression: `DeflateState::new_in` must present the caller's
    /// `zalloc` with the same `(items, size)` argument pairs — in the same order,
    /// and the same number of times — that C `deflateInit2_` passes.
    ///
    /// A bounded or inspecting allocator (`infcover.c`'s is the canonical
    /// example) legitimately reads both arguments and counts the calls, so
    /// flattening every request to `(byte_count, 1)`, reordering them, or issuing
    /// one more than C does is an observable ABI difference even when the total
    /// byte count is unchanged (AAP §0.6.5).
    ///
    /// C's sequence is exactly five requests:
    ///
    /// 1. the state, `ZALLOC(strm, 1, sizeof(deflate_state))` (`deflate.c` L440),
    /// 2. `window`, `ZALLOC(strm, s->w_size, 2 * sizeof(Byte))` (L458),
    /// 3. `prev`, `ZALLOC(strm, s->w_size, sizeof(Pos))` (L459),
    /// 4. `head`, `ZALLOC(strm, s->hash_size, sizeof(Pos))` (L460),
    /// 5. `pending_buf`, `ZALLOC(strm, s->lit_bufsize, LIT_BUFS)` (L505).
    ///
    /// There is no sixth request: C carves the symbol buffer out of the pending
    /// allocation with `s->sym_buf = s->pending_buf + s->lit_bufsize` (L520), and
    /// this port reproduces that as index arithmetic inside one owned buffer
    /// (AAP §0.3.2 rule T3). The state's byte count comes from
    /// [`DeflateState::C_LAYOUT_SIZE`] — the field-exact `#[repr(C)]` layout
    /// mirror of C's `deflate_state` — not from this Rust type's own `size_of`,
    /// which differs (AAP §0.6.3).
    #[test]
    fn deflate_new_in_presents_c_zalloc_geometry() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::constants::{Strategy, Z_DEFLATED};
        use crate::deflate::state::DeflateState;

        /// Recorded `(items, size)` pairs, in call order. Sized generously so an
        /// unexpected extra allocation shows up as a count mismatch rather than
        /// an overflow.
        const CAP: usize = 12;
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        static ITEMS: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
        static SIZES: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];

        unsafe extern "C" fn recording_zalloc(
            _opaque: *mut c_void,
            items: c_uint,
            size: c_uint,
        ) -> *mut c_void {
            let i = COUNT.fetch_add(1, Ordering::SeqCst);
            if i < CAP {
                ITEMS[i].store(items as usize, Ordering::SeqCst);
                SIZES[i].store(size as usize, Ordering::SeqCst);
            }
            hook_backing_alloc((items as usize) * (size as usize))
        }
        unsafe extern "C" fn recording_zfree(_opaque: *mut c_void, address: *mut c_void) {
            hook_backing_free(address);
        }

        let hook = AllocHook::new(
            Some(recording_zalloc),
            Some(recording_zfree),
            ptr::null_mut(),
        );

        // Reference parameters: level 6, 15-bit window, mem level 8 — the zlib
        // defaults, and the configuration `deflateInit_` produces.
        let state = DeflateState::new_in(hook, 6, Z_DEFLATED, 15, 8, Strategy::Default, 1)
            .expect("healthy hook initializes the state");

        let w_size = 1usize << 15;
        let hash_size = 1usize << (8 + 7);
        let lit_bufsize = 1usize << (8 + 6);
        let expected: [(usize, usize); 5] = [
            // C: ZALLOC(strm, 1, sizeof(deflate_state))   -- deflate.c L440
            (1, DeflateState::C_LAYOUT_SIZE),
            // C: ZALLOC(strm, s->w_size, 2 * sizeof(Byte)) -- deflate.c L458
            (w_size, 2),
            // C: ZALLOC(strm, s->w_size, sizeof(Pos))      -- deflate.c L459
            (w_size, 2),
            // C: ZALLOC(strm, s->hash_size, sizeof(Pos))   -- deflate.c L460
            (hash_size, 2),
            // C: ZALLOC(strm, s->lit_bufsize, LIT_BUFS)    -- deflate.c L505
            (lit_bufsize, 4),
        ];

        let observed = COUNT.load(Ordering::SeqCst);
        assert_eq!(
            observed,
            expected.len(),
            "deflateInit2_ must make exactly {} hook allocations",
            expected.len()
        );
        for (i, &(items, size)) in expected.iter().enumerate() {
            assert_eq!(
                (
                    ITEMS[i].load(Ordering::SeqCst),
                    SIZES[i].load(Ordering::SeqCst)
                ),
                (items, size),
                "hook allocation #{i} must be ({items}, {size}) as C passes it"
            );
        }

        // Sanity: the byte totals still match the buffer geometry. The single
        // `pending_buf` request covers both the pending output (its lower
        // `lit_bufsize` bytes) and the overlaid symbol region (the upper
        // `3 * lit_bufsize`), which is why four rather than seven appears here.
        assert_eq!(state.pending_buf.len(), lit_bufsize * 4);
        assert_eq!(state.window.len(), 2 * w_size);
        assert_eq!(state.prev.len(), w_size);
        assert_eq!(state.head.len(), hash_size);
    }

    /// Regression guard: `deflateCopy` must present the caller's `zalloc` with C's
    /// **copy** schedule — the destination state, then `window`, `prev`, `head`
    /// and `pending_buf` (`deflate.c` L1335-L1345) — with the same `(items, size)`
    /// pairs `deflateInit2_` used, and with no request for the overlaid symbol
    /// region.
    #[test]
    fn deflate_copy_presents_c_zalloc_geometry() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::constants::{Strategy, Z_DEFLATED};
        use crate::deflate::state::DeflateState;

        /// Recorded `(items, size)` pairs, in call order, for the copy only.
        const CAP: usize = 12;
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        static ITEMS: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
        static SIZES: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
        /// Set once init is done, so only the copy's requests are recorded.
        static RECORDING: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);

        unsafe extern "C" fn copy_zalloc(
            _opaque: *mut c_void,
            items: c_uint,
            size: c_uint,
        ) -> *mut c_void {
            if RECORDING.load(Ordering::SeqCst) {
                let i = COUNT.fetch_add(1, Ordering::SeqCst);
                if i < CAP {
                    ITEMS[i].store(items as usize, Ordering::SeqCst);
                    SIZES[i].store(size as usize, Ordering::SeqCst);
                }
            }
            hook_backing_alloc((items as usize) * (size as usize))
        }
        unsafe extern "C" fn copy_zfree(_opaque: *mut c_void, address: *mut c_void) {
            hook_backing_free(address);
        }

        let hook = AllocHook::new(Some(copy_zalloc), Some(copy_zfree), ptr::null_mut());

        // A small geometry keeps the test cheap while keeping every request pair
        // distinct enough to be meaningful: windowBits 9 => w_size 512,
        // memLevel 4 => hash_size 2048, lit_bufsize 1024.
        let state = DeflateState::new_in(hook, 6, Z_DEFLATED, 9, 4, Strategy::Default, 1)
            .expect("healthy hook initializes the state");

        RECORDING.store(true, Ordering::SeqCst);
        let copy = state
            .try_copy_in(&HookAllocator::new(hook))
            .expect("healthy hook duplicates the state");
        RECORDING.store(false, Ordering::SeqCst);

        let w_size = 1usize << 9;
        let hash_size = 1usize << (4 + 7);
        let lit_bufsize = 1usize << (4 + 6);
        let expected: [(usize, usize); 5] = [
            // C: ZALLOC(dest, 1, sizeof(deflate_state))     -- deflate.c L1335
            (1, DeflateState::C_LAYOUT_SIZE),
            // C: ZALLOC(dest, ds->w_size, 2 * sizeof(Byte)) -- deflate.c L1341
            (w_size, 2),
            // C: ZALLOC(dest, ds->w_size, sizeof(Pos))      -- deflate.c L1342
            (w_size, 2),
            // C: ZALLOC(dest, ds->hash_size, sizeof(Pos))   -- deflate.c L1343
            (hash_size, 2),
            // C: ZALLOC(dest, ds->lit_bufsize, LIT_BUFS)    -- deflate.c L1344
            (lit_bufsize, 4),
        ];

        assert_eq!(
            COUNT.load(Ordering::SeqCst),
            expected.len(),
            "deflateCopy must make exactly C's five requests"
        );
        for (i, &(items, size)) in expected.iter().enumerate() {
            assert_eq!(
                (
                    ITEMS[i].load(Ordering::SeqCst),
                    SIZES[i].load(Ordering::SeqCst)
                ),
                (items, size),
                "copy allocation #{i} must be ({items}, {size}) as C passes it"
            );
        }

        // The copy really is a copy: same geometry, same live bytes, distinct
        // storage, and every region inside the caller's arena.
        assert_eq!(copy.lit_bufsize, state.lit_bufsize);
        assert_eq!(copy.pending_buf.len(), state.pending_buf.len());
        assert_eq!(&copy.window[..], &state.window[..]);
        assert!(copy.pending_buf.is_foreign() && copy.state_alloc.is_foreign());
    }

    /// Regression guard: the engines must release their buffers in the **reverse of
    /// C's allocation order**, which is the order C's own teardown uses.
    ///
    /// `deflateEnd` is explicit about it — the source carries the comment
    /// "Deallocate in reverse order of allocations" — and the sequence is
    /// `pending_buf`, `head`, `prev`, `window`, then the state
    /// (`deflate.c` L1300-L1306). `inflateEnd` frees the window then the state
    /// (`inflate.c` L1160-L1161).
    ///
    /// The four deflate working buffers can share a byte size, so size alone
    /// cannot identify them. This test therefore records the *address* each
    /// allocation returned and, on each `zfree`, reports which allocation index
    /// that address belonged to — an exact identification independent of geometry.
    #[test]
    fn engine_teardown_frees_in_c_reverse_allocation_order() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::constants::{Strategy, Z_DEFLATED};
        use crate::deflate::state::DeflateState;
        use crate::inflate::state::InflateState;

        /// Capacity for the recorded allocation/free sequences. Generous so an
        /// unexpected extra event shows up as a length mismatch, not an overflow.
        const CAP: usize = 12;
        /// Addresses returned by `zalloc`, in allocation order.
        static ADDRS: [AtomicUsize; CAP] = [const { AtomicUsize::new(0) }; CAP];
        /// Number of allocations recorded.
        static NALLOC: AtomicUsize = AtomicUsize::new(0);
        /// Allocation indices, in the order their regions were freed.
        static FREED: [AtomicUsize; CAP] = [const { AtomicUsize::new(usize::MAX) }; CAP];
        /// Number of frees recorded.
        static NFREE: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn tracking_zalloc(
            _opaque: *mut c_void,
            items: c_uint,
            size: c_uint,
        ) -> *mut c_void {
            let block = hook_backing_alloc((items as usize) * (size as usize));
            let i = NALLOC.fetch_add(1, Ordering::SeqCst);
            if i < CAP {
                ADDRS[i].store(block as usize, Ordering::SeqCst);
            }
            block
        }
        unsafe extern "C" fn tracking_zfree(_opaque: *mut c_void, address: *mut c_void) {
            let want = address as usize;
            let n = NALLOC.load(Ordering::SeqCst).min(CAP);
            // Resolve the freed address back to the allocation index that
            // produced it. Address identity is the only usable key here: the
            // four deflate working buffers can share a byte size, so the
            // `(items, size)` pair cannot tell them apart.
            let which = ADDRS
                .iter()
                .take(n)
                .position(|slot| slot.load(Ordering::SeqCst) == want)
                .unwrap_or(usize::MAX);
            let f = NFREE.fetch_add(1, Ordering::SeqCst);
            if f < CAP {
                FREED[f].store(which, Ordering::SeqCst);
            }
            hook_backing_free(address);
        }

        let hook = AllocHook::new(Some(tracking_zalloc), Some(tracking_zfree), ptr::null_mut());

        // --- deflate: five allocations, freed 4, 3, 2, 1, 0 ------------------
        {
            let state = DeflateState::new_in(hook, 6, Z_DEFLATED, 15, 8, Strategy::Default, 1)
                .expect("healthy hook initializes the state");
            assert_eq!(
                NALLOC.load(Ordering::SeqCst),
                5,
                "C's deflateInit2_ schedule: state, window, prev, head, pending_buf"
            );
            assert_eq!(NFREE.load(Ordering::SeqCst), 0, "init frees nothing");
            drop(state);
        }
        let n = NFREE.load(Ordering::SeqCst);
        assert_eq!(n, 5, "every region must reach the caller's zfree");
        let order: [usize; 5] = core::array::from_fn(|i| FREED[i].load(Ordering::SeqCst));
        assert_eq!(
            order,
            // Allocation indices were 0 = state, 1 = window, 2 = prev,
            // 3 = head, 4 = pending_buf.
            [4, 3, 2, 1, 0],
            "deflateEnd frees pending_buf, head, prev, window, state \
             (`deflate.c` L1300-L1306)"
        );

        // --- inflate: state reservation then the lazily grown window ---------
        NALLOC.store(0, Ordering::SeqCst);
        NFREE.store(0, Ordering::SeqCst);
        for cell in &FREED {
            cell.store(usize::MAX, Ordering::SeqCst);
        }
        {
            let mut state = InflateState::try_new_in(hook, 0, 15).expect("state boxes");
            // Install the two hook-backed regions in C's order: the state
            // reservation (`inflate.c` L198) and then the window (L261).
            state.state_alloc = AllocBuffer::try_zeroed_items(1, InflateState::C_LAYOUT_SIZE, hook)
                .expect("healthy hook reserves the state footprint");
            state.window =
                AllocBuffer::try_zeroed(1 << 15, hook).expect("healthy hook serves the window");
            assert_eq!(NALLOC.load(Ordering::SeqCst), 2);
            drop(state);
        }
        assert_eq!(NFREE.load(Ordering::SeqCst), 2);
        let order: [usize; 2] = core::array::from_fn(|i| FREED[i].load(Ordering::SeqCst));
        assert_eq!(
            order,
            // 0 = state reservation, 1 = window.
            [1, 0],
            "inflateEnd frees the window then the state \
             (`inflate.c` L1160-L1161)"
        );
    }

    /// Regression guard: `deflateCopy` must duplicate the state through the *same*
    /// allocator and report an out-of-memory refusal, never silently relocate the
    /// copy into the Rust global heap.
    ///
    /// C `deflateCopy` allocates the destination state and buffers through the
    /// source stream's `zalloc` and returns `Z_MEM_ERROR` if any of them fails
    /// (`deflate.c` L1317-L1377). The infallible `AllocBuffer: Clone` cannot
    /// express that, so the copy path uses `DeflateState::try_copy`. This test
    /// initializes a state through a healthy hook, checks the copy stays inside
    /// the caller's arena, then makes the hook refuse and requires the copy to
    /// fail rather than escape (AAP §0.6.5).
    #[test]
    fn deflate_try_copy_preserves_allocator_and_reports_oom() {
        use core::sync::atomic::{AtomicBool, Ordering};

        use crate::constants::{Strategy, Z_DEFLATED};
        use crate::deflate::state::DeflateState;

        static REFUSE: AtomicBool = AtomicBool::new(false);

        unsafe extern "C" fn flaky_zalloc(
            _opaque: *mut c_void,
            items: c_uint,
            size: c_uint,
        ) -> *mut c_void {
            if REFUSE.load(Ordering::SeqCst) {
                return ptr::null_mut();
            }
            hook_backing_alloc((items as usize) * (size as usize))
        }
        unsafe extern "C" fn flaky_zfree(_opaque: *mut c_void, address: *mut c_void) {
            hook_backing_free(address);
        }

        let hook = AllocHook::new(Some(flaky_zalloc), Some(flaky_zfree), ptr::null_mut());

        // Small geometry (9-bit window, mem level 1) keeps the test cheap.
        let state = DeflateState::new_in(hook, 6, Z_DEFLATED, 9, 1, Strategy::Default, 1)
            .expect("healthy hook initializes the state");
        assert!(state.window.is_foreign());

        {
            let copy = state
                .try_copy_in(&HookAllocator::new(hook))
                .expect("healthy hook duplicates the state and its buffers");
            assert!(
                copy.window.is_foreign()
                    && copy.prev.is_foreign()
                    && copy.head.is_foreign()
                    && copy.pending_buf.is_foreign()
                    && copy.state_alloc.is_foreign(),
                "every buffer of the copy must stay in the caller's arena"
            );
            assert_eq!(copy.window.len(), state.window.len());
            assert_eq!(&copy.window[..], &state.window[..]);
        } // <- the copy's buffers are released through the caller's `zfree`

        // Now make the caller's allocator refuse: the copy must fail instead of
        // returning a state whose buffers escaped into the global heap.
        REFUSE.store(true, Ordering::SeqCst);
        assert!(
            state.try_copy_in(&HookAllocator::new(hook)).is_none(),
            "an allocator refusal during deflateCopy must surface as Z_MEM_ERROR"
        );
        REFUSE.store(false, Ordering::SeqCst);
    }

    /// `zstream_with_caller_alloc` produces a `ZStream<CAllocator>` carrying the
    /// caller's hooks/cookie.
    #[test]
    fn zstream_with_caller_alloc_carries_hooks() {
        let cookie = 0xABCD_usize as *mut c_void;
        let strm = z_stream {
            next_in: ptr::null(),
            avail_in: 0,
            total_in: 0,
            next_out: ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: ptr::null_mut(),
            state: ptr::null_mut(),
            zalloc: None,
            zfree: None,
            opaque: cookie,
            data_type: 0,
            adler: 0,
            reserved: 0,
        };
        // SAFETY: `strm` is a fully-initialized local `z_stream`.
        let z = unsafe { zstream_with_caller_alloc(&strm) };
        assert_eq!(z.allocator().opaque, cookie);
        assert!(!z.has_state());
    }

    /// The state-handle helpers round-trip a boxed value through the opaque
    /// `state` pointer.
    #[test]
    fn state_handle_round_trip() {
        let mut strm = zeroed_stream();
        assert!(strm.state.is_null());

        // Install a boxed value.
        let boxed = Box::new(1234u64);
        // SAFETY: transfers ownership of a freshly boxed value into `state`.
        strm.state = unsafe { state_ptr_from_box(boxed) };
        assert!(!strm.state.is_null());

        // Borrow it back.
        // SAFETY: `state` holds a live `Box<u64>` installed just above.
        let borrowed: Option<&mut u64> = unsafe { state_ref::<u64>(&mut strm) };
        assert_eq!(borrowed.copied(), Some(1234));

        // Reclaim it; the field is nulled and the box drops here.
        // SAFETY: `state` still holds the same live `Box<u64>`.
        let taken: Option<Box<u64>> = unsafe { state_take::<u64>(&mut strm) };
        assert_eq!(taken.as_deref().copied(), Some(1234));
        assert!(strm.state.is_null());

        // A second take yields `None`.
        // SAFETY: `state` is now null.
        let again = unsafe { state_take::<u64>(&mut strm) };
        assert!(again.is_none());
    }

    /// Input/output slice bridging returns empty slices for null/zero cursors
    /// and correct views otherwise.
    #[test]
    fn slice_bridging_and_cursor_advance() {
        let input = [10u8, 20, 30, 40];
        let mut output = [0u8; 4];

        let mut strm = zeroed_stream();
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = output.as_mut_ptr();
        strm.avail_out = output.len() as c_uint;

        // SAFETY: cursors point at the live local buffers with matching lengths.
        let inp = unsafe { input_slice(&strm) };
        assert_eq!(inp, &[10, 20, 30, 40]);
        // SAFETY: as above; the output buffer is disjoint from the input.
        let out = unsafe { output_slice(&strm) };
        out.copy_from_slice(&[1, 2, 3, 4]);

        // Advance both cursors by 2.
        // SAFETY: 2 <= avail_in and 2 <= avail_out.
        unsafe {
            advance_input(&mut strm, 2);
            advance_output(&mut strm, 2);
        }
        assert_eq!(strm.avail_in, 2);
        assert_eq!(strm.total_in, 2);
        assert_eq!(strm.avail_out, 2);
        assert_eq!(strm.total_out, 2);
        assert_eq!(output, [1, 2, 3, 4]);

        // Null/zero cursors yield empty slices.
        let empty = zeroed_stream();
        // SAFETY: both cursors are null.
        assert!(unsafe { input_slice(&empty) }.is_empty());
        // SAFETY: both cursors are null.
        assert!(unsafe { output_slice(&empty) }.is_empty());
    }

    /// Setters write the raw fields with the correct C widths.
    #[test]
    fn field_setters() {
        let mut strm = zeroed_stream();
        let adler: u32 = 0xDEAD_BEEF;
        set_adler(&mut strm, adler);
        set_data_type(&mut strm, 1);
        assert_eq!(strm.adler, c_ulong::from(adler));
        assert_eq!(strm.data_type, 1);

        let msg = c"boom";
        set_msg(&mut strm, msg.as_ptr());
        assert_eq!(strm.msg as *const c_char, msg.as_ptr());
    }

    /// A `gz_header` converts to `GzHeader` and back with field equivalence for
    /// the scalars and the `extra`/`name`/`comment` byte vectors.
    #[test]
    fn gz_header_round_trip() {
        let mut extra_src = [1u8, 2, 3];
        let mut name_src = *b"file.txt\0";
        let mut comment_src = *b"cmt\0";

        let src = gz_header {
            text: 1,
            time: 0x1234_5678,
            xflags: 7,
            os: 3,
            extra: extra_src.as_mut_ptr(),
            extra_len: 3,
            extra_max: 0,
            name: name_src.as_mut_ptr(),
            name_max: 0,
            comment: comment_src.as_mut_ptr(),
            comm_max: 0,
            hcrc: 1,
            done: 0,
        };

        // SAFETY: `src` is fully initialized; `extra` is readable for
        // `extra_len` bytes and `name`/`comment` are NUL-terminated.
        let idi = unsafe { gz_header_to_idiomatic(&src) }.expect("non-null header");
        assert!(idi.text);
        assert_eq!(idi.time, 0x1234_5678);
        assert_eq!(idi.xflags, 7);
        assert_eq!(idi.os, 3);
        assert_eq!(idi.extra.as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!(idi.name.as_deref(), Some(&b"file.txt"[..]));
        assert_eq!(idi.comment.as_deref(), Some(&b"cmt"[..]));
        assert!(idi.hcrc);

        // Write back into caller-owned buffers.
        let mut extra_dst = [0u8; 8];
        let mut name_dst = [0u8; 16];
        let mut comment_dst = [0u8; 16];
        let mut out = gz_header {
            text: 0,
            time: 0,
            xflags: 0,
            os: 0,
            extra: extra_dst.as_mut_ptr(),
            extra_len: 0,
            extra_max: extra_dst.len() as c_uint,
            name: name_dst.as_mut_ptr(),
            name_max: name_dst.len() as c_uint,
            comment: comment_dst.as_mut_ptr(),
            comm_max: comment_dst.len() as c_uint,
            hcrc: 0,
            done: 0,
        };
        // SAFETY: `out`'s buffers are sized by its `*_max` fields.
        unsafe { write_gz_header_from_idiomatic(&mut out, &idi) };

        assert_eq!(out.text, 1);
        assert_eq!(out.time, 0x1234_5678 as c_ulong);
        assert_eq!(out.xflags, 7);
        assert_eq!(out.os, 3);
        assert_eq!(out.hcrc, 1);
        assert_eq!(out.extra_len, 3);
        assert_eq!(&extra_dst[..3], &[1, 2, 3]);
        assert_eq!(&name_dst[..8], b"file.txt");
        assert_eq!(name_dst[8], 0, "name must be NUL-terminated");
        assert_eq!(&comment_dst[..3], b"cmt");
        assert_eq!(comment_dst[3], 0, "comment must be NUL-terminated");
    }

    /// A null `gz_header` pointer converts to `None`, and writing back through a
    /// null pointer is a no-op.
    #[test]
    fn gz_header_null_is_none() {
        // SAFETY: the null case is handled without dereferencing.
        assert!(unsafe { gz_header_to_idiomatic(ptr::null()) }.is_none());
        let hdr = GzHeader::new();
        // SAFETY: writing through null is an explicit no-op.
        unsafe { write_gz_header_from_idiomatic(ptr::null_mut(), &hdr) };
    }

    /// `write_gz_header_from_idiomatic` truncates to the caller's capacities and
    /// never overruns.
    #[test]
    fn write_gz_header_truncates_to_capacity() {
        let src = GzHeader::new()
            .with_name(b"toolongname".to_vec())
            .with_extra(alloc::vec![9u8; 10]);

        let mut name_dst = [0xAAu8; 4];
        let mut extra_dst = [0xAAu8; 4];
        let mut out = gz_header {
            text: 0,
            time: 0,
            xflags: 0,
            os: 0,
            extra: extra_dst.as_mut_ptr(),
            extra_len: 0,
            extra_max: extra_dst.len() as c_uint,
            name: name_dst.as_mut_ptr(),
            name_max: name_dst.len() as c_uint,
            comment: ptr::null_mut(),
            comm_max: 0,
            hcrc: 0,
            done: 0,
        };
        // SAFETY: buffers are sized by the `*_max` fields (4 bytes each).
        unsafe { write_gz_header_from_idiomatic(&mut out, &src) };

        // name: filled to the full 4-byte capacity with content and NO
        // terminator. This mirrors C `inflateGetHeader`, which truncates the
        // name to exactly `name_max` *data* bytes and only stores the NUL when
        // the name (plus its terminator) fits within the capacity.
        assert_eq!(&name_dst[..4], b"tool");
        // extra: filled to capacity (no room for a terminator; extra is binary).
        assert_eq!(extra_dst, [9, 9, 9, 9]);
        // extra_len reports the full source length even though the copy was cut.
        assert_eq!(out.extra_len, 10);
    }

    /// A poisoned `gz_header` whose buffers are pre-filled with a sentinel, for
    /// asserting exactly which bytes [`publish_gz_header`] wrote.
    #[cfg(feature = "gzip")]
    struct PoisonedRawHeader {
        head: gz_header,
        extra: alloc::vec::Vec<u8>,
        name: alloc::vec::Vec<u8>,
        comment: alloc::vec::Vec<u8>,
    }

    #[cfg(feature = "gzip")]
    impl PoisonedRawHeader {
        const POISON: u8 = 0x7e;
        const SCALAR: c_int = 9;
        const TIME: c_ulong = 0xDEAD_BEEF;
        const XLEN: c_uint = 12345;

        fn new(cap: usize) -> Self {
            let mut this = Self {
                head: gz_header {
                    text: Self::SCALAR,
                    time: Self::TIME,
                    xflags: Self::SCALAR,
                    os: Self::SCALAR,
                    extra: ptr::null_mut(),
                    extra_len: Self::XLEN,
                    extra_max: cap as c_uint,
                    name: ptr::null_mut(),
                    name_max: cap as c_uint,
                    comment: ptr::null_mut(),
                    comm_max: cap as c_uint,
                    hcrc: Self::SCALAR,
                    done: Self::SCALAR,
                },
                extra: alloc::vec![Self::POISON; cap],
                name: alloc::vec![Self::POISON; cap],
                comment: alloc::vec![Self::POISON; cap],
            };
            this.head.extra = this.extra.as_mut_ptr();
            this.head.name = this.name.as_mut_ptr();
            this.head.comment = this.comment.as_mut_ptr();
            this
        }
    }

    /// An empty [`HeaderPublication`] authorizes **no** write, so a call that
    /// reached no gzip header state leaves the caller's struct and every capture
    /// buffer exactly as it found them.
    ///
    /// This is the property a bulk mirror cannot have, and it is what makes C's
    /// sentinel-based "has this field arrived yet?" idiom work: C assigns each
    /// field inside its own parser state, so anything the stream has not reached
    /// still holds the caller's value.
    #[test]
    #[cfg(feature = "gzip")]
    fn publish_gz_header_writes_nothing_for_an_empty_publication() {
        let mut raw = PoisonedRawHeader::new(8);
        let src = GzHeader::new()
            .with_name(b"nm".to_vec())
            .with_comment(b"cm".to_vec())
            .with_extra(alloc::vec![1u8, 2]);

        // SAFETY: `raw.head`'s buffers are sized by its `*_max` fields.
        unsafe {
            publish_gz_header(&mut raw.head, &src, &HeaderPublication::default());
        }

        assert_eq!(raw.head.text, PoisonedRawHeader::SCALAR);
        assert_eq!(raw.head.time, PoisonedRawHeader::TIME);
        assert_eq!(raw.head.xflags, PoisonedRawHeader::SCALAR);
        assert_eq!(raw.head.os, PoisonedRawHeader::SCALAR);
        assert_eq!(raw.head.hcrc, PoisonedRawHeader::SCALAR);
        assert_eq!(raw.head.done, PoisonedRawHeader::SCALAR);
        assert_eq!(raw.head.extra_len, PoisonedRawHeader::XLEN);
        assert!(!raw.head.extra.is_null() && !raw.head.name.is_null());
        assert!(!raw.head.comment.is_null());
        assert!(
            raw.extra.iter().all(|&b| b == PoisonedRawHeader::POISON)
                && raw.name.iter().all(|&b| b == PoisonedRawHeader::POISON)
                && raw.comment.iter().all(|&b| b == PoisonedRawHeader::POISON),
            "an empty publication must not touch a single buffer byte"
        );
    }

    /// Buffer bytes are written at the offset C wrote them, so a field delivered
    /// across several `inflate` calls lands contiguously and no call rewrites an
    /// earlier call's bytes.
    ///
    /// C's `EXTRA` state copies to `head->extra + (extra_len - state->length)`
    /// (`inflate.c` L614-L621) — the count already consumed. The record carries
    /// how many bytes *this* call appended, and the owned `Vec`'s new length gives
    /// the end, so the destination offset is `len - stored`. Feeding the same
    /// growing `Vec` in two instalments and asserting the untouched tail proves the
    /// arithmetic.
    #[test]
    #[cfg(feature = "gzip")]
    fn publish_gz_header_writes_each_instalment_at_cs_offset() {
        let mut raw = PoisonedRawHeader::new(8);

        // Call 1: two bytes of extra and two of the name arrive.
        let mut src = GzHeader::new()
            .with_extra(alloc::vec![0xA1u8, 0xA2])
            .with_name(b"ab".to_vec());
        let first = HeaderPublication {
            extra_stored: 2,
            name_stored: 2,
            ..HeaderPublication::default()
        };
        // SAFETY: buffers are sized by the `*_max` fields.
        unsafe { publish_gz_header(&mut raw.head, &src, &first) };
        assert_eq!(&raw.extra[..2], &[0xA1, 0xA2]);
        assert!(
            raw.extra[2..]
                .iter()
                .all(|&b| b == PoisonedRawHeader::POISON)
        );
        assert_eq!(&raw.name[..2], b"ab");
        assert_eq!(
            raw.name[2],
            PoisonedRawHeader::POISON,
            "the name is still arriving, so it carries no terminator yet"
        );

        // Call 2: one more extra byte and two more name bytes, then the NUL.
        src.extra.as_mut().unwrap().push(0xA3);
        src.name.as_mut().unwrap().extend_from_slice(b"cd");
        let second = HeaderPublication {
            extra_stored: 1,
            name_stored: 2,
            name_terminated: true,
            ..HeaderPublication::default()
        };
        // SAFETY: buffers are sized by the `*_max` fields.
        unsafe { publish_gz_header(&mut raw.head, &src, &second) };
        assert_eq!(
            &raw.extra[..3],
            &[0xA1, 0xA2, 0xA3],
            "the second instalment appended rather than restarting at offset 0"
        );
        assert!(
            raw.extra[3..]
                .iter()
                .all(|&b| b == PoisonedRawHeader::POISON)
        );
        assert_eq!(&raw.name[..4], b"abcd");
        assert_eq!(raw.name[4], 0, "the decoded NUL is now stored");
        assert!(
            raw.name[5..]
                .iter()
                .all(|&b| b == PoisonedRawHeader::POISON)
        );
    }

    /// The tri-state `done` reaches the C caller verbatim, including the `-1` that
    /// says "this stream carries no gzip header" and which a Rust [`bool`] cannot
    /// represent (`inflate.c` L505-L506).
    #[test]
    #[cfg(feature = "gzip")]
    fn publish_gz_header_publishes_the_tri_state_done_verbatim() {
        let src = GzHeader::new();
        for (recorded, expected) in [
            (crate::gz_header::HeaderDone::NotGzip, -1),
            (crate::gz_header::HeaderDone::Pending, 0),
            (crate::gz_header::HeaderDone::Complete, 1),
        ] {
            let mut raw = PoisonedRawHeader::new(4);
            let published = HeaderPublication {
                done: Some(recorded),
                ..HeaderPublication::default()
            };
            // SAFETY: buffers are sized by the `*_max` fields.
            unsafe { publish_gz_header(&mut raw.head, &src, &published) };
            assert_eq!(raw.head.done, expected, "done must survive as {expected}");
            // Nothing else was authorized, so nothing else moved.
            assert_eq!(raw.head.text, PoisonedRawHeader::SCALAR);
            assert_eq!(raw.head.hcrc, PoisonedRawHeader::SCALAR);
        }
    }

    /// The `*_null` flags publish C's `Z_NULL` assignments for an absent field
    /// (`inflate.c` L605-L606, L643-L644, L665-L666) without touching the buffer
    /// the caller handed over.
    #[test]
    #[cfg(feature = "gzip")]
    fn publish_gz_header_nulls_only_the_pointers_the_record_names() {
        let src = GzHeader::new();
        let mut raw = PoisonedRawHeader::new(4);
        let published = HeaderPublication {
            extra_null: true,
            comment_null: true,
            ..HeaderPublication::default()
        };
        // SAFETY: buffers are sized by the `*_max` fields.
        unsafe { publish_gz_header(&mut raw.head, &src, &published) };

        assert!(raw.head.extra.is_null(), "EXLEN's no-FEXTRA branch");
        assert!(raw.head.comment.is_null(), "COMMENT's no-FCOMMENT branch");
        assert!(
            !raw.head.name.is_null(),
            "an unrecorded field's pointer must survive"
        );
        assert!(
            raw.extra.iter().all(|&b| b == PoisonedRawHeader::POISON)
                && raw.comment.iter().all(|&b| b == PoisonedRawHeader::POISON),
            "nulling a pointer must not write into the buffer it pointed at"
        );
        assert_eq!(
            raw.head.extra_len,
            PoisonedRawHeader::XLEN,
            "C's no-FEXTRA branch nulls `extra` and never touches `extra_len`"
        );
    }

    /// The declared `XLEN` is published **unclamped**, from the record rather than
    /// from `extra.len()`.
    ///
    /// C assigns `head->extra_len = (unsigned)hold` in `EXLEN` (`inflate.c`
    /// L599-L600) gated on neither `extra`'s nullity nor `extra_max`'s size, while
    /// the copy is separately clamped. That asymmetry is the entire truncation
    /// contract: `extra_len > extra_max` is a C caller's only signal that bytes
    /// were dropped, so reporting the captured count instead would make silent
    /// data loss undetectable.
    #[test]
    #[cfg(feature = "gzip")]
    fn publish_gz_header_reports_the_declared_xlen_unclamped() {
        // A two-byte capacity capturing two of a declared ten bytes.
        let src = GzHeader::new().with_extra(alloc::vec![0xB1u8, 0xB2]);
        let mut raw = PoisonedRawHeader::new(2);
        let published = HeaderPublication {
            extra_len: Some(10),
            extra_stored: 2,
            ..HeaderPublication::default()
        };
        // SAFETY: buffers are sized by the `*_max` fields.
        unsafe { publish_gz_header(&mut raw.head, &src, &published) };

        assert_eq!(
            raw.head.extra_len, 10,
            "the DECLARED length, not the copied 2"
        );
        assert!(raw.head.extra_len > raw.head.extra_max);
        assert_eq!(raw.extra, alloc::vec![0xB1u8, 0xB2]);

        // A pure length query: no buffer at all, yet the length still lands.
        let mut head = gz_header {
            text: 0,
            time: 0,
            xflags: 0,
            os: 0,
            extra: ptr::null_mut(),
            extra_len: 0,
            extra_max: 0,
            name: ptr::null_mut(),
            name_max: 0,
            comment: ptr::null_mut(),
            comm_max: 0,
            hcrc: 0,
            done: 0,
        };
        // SAFETY: every buffer pointer is null, which the publisher handles.
        unsafe {
            publish_gz_header(
                &mut head,
                &GzHeader::new(),
                &HeaderPublication {
                    extra_len: Some(10),
                    ..HeaderPublication::default()
                },
            );
        }
        assert_eq!(head.extra_len, 10);
    }

    // -- panic-guard tests --------------------------------------------------
    //
    // Every fallible shim body in `src/ffi/**` runs inside one of the four
    // guards above, because a Rust panic unwinding across the C ABI is
    // undefined behavior. The guards are four *separate* implementations, one
    // per C return width, and each is the last line of defense for the shims
    // that use it, so each is exercised directly rather than by analogy:
    //
    // | Guard         | C return type          | Representative shims                       |
    // |---------------|------------------------|--------------------------------------------|
    // | `guard_int`   | `int`                  | `deflate`, `inflate`, `gzread`, `gzclose`   |
    // | `guard_ulong` | `uLong`                | `adler32`, `crc32`, `compressBound`         |
    // | `guard_ptr`   | `T *`                  | `gzgets`, `gzopen`                          |
    // | `guard_off`   | `z_off_t`/`z_off64_t`  | `gzseek`, `gztell`, `gzoffset`              |
    //
    // Two properties matter for every one of them: the success value must pass
    // through *bit-exactly* (a guard that clamped or re-derived it would corrupt
    // an ordinary result), and a panic must yield *the caller's* default, since
    // that substituted value is precisely what the C caller observes as the
    // function's error sentinel.

    /// The panic guard substitutes the default value when the body panics and
    /// passes the value through otherwise.
    #[cfg(feature = "std")]
    #[test]
    fn guard_int_catches_panic_and_passes_value() {
        assert_eq!(guard_int(-2, || 7), 7);

        // The default panic hook is silenced (and the swap serialized against
        // the sibling guard tests) so the test log stays clean.
        let caught = with_silenced_panic_hook(|| guard_int(-2, || panic!("boundary panic")));
        assert_eq!(caught, -2);
    }

    /// [`guard_ulong`] passes a `c_ulong` through unchanged and substitutes its
    /// exact default when the body panics.
    ///
    /// This guard backs the shims whose C contract has **no error sentinel** —
    /// `adler32`, `crc32`, `compressBound`, `deflateBound` all return a plain
    /// `uLong` — so the substituted default *is* the value a C caller observes
    /// and it must be reproduced bit-exactly rather than coerced to zero.
    #[cfg(feature = "std")]
    #[test]
    fn guard_ulong_catches_panic_and_passes_value() {
        // Pass-through, including both extremes of the platform width.
        assert_eq!(guard_ulong(0, || 0), 0);
        assert_eq!(guard_ulong(0, || 1), 1);
        assert_eq!(guard_ulong(1, || c_ulong::MAX), c_ulong::MAX);

        // A panic substitutes the caller's default exactly — not zero, and not
        // the closure's would-be value.
        let caught =
            with_silenced_panic_hook(|| guard_ulong(0xDEAD_BEEF, || panic!("boundary panic")));
        assert_eq!(caught, 0xDEAD_BEEF);

        // A zero default is substituted just as faithfully (the `adler32` /
        // `crc32` shims pass `0`).
        let zeroed = with_silenced_panic_hook(|| guard_ulong(0, || panic!("boundary panic")));
        assert_eq!(zeroed, 0);
    }

    /// [`guard_ptr`] passes a pointer through by **identity** and substitutes
    /// its default when the body panics.
    ///
    /// The generic parameter is exercised at two distinct pointee types so the
    /// guard is proven generic rather than accidentally monomorphic, and both
    /// the C `NULL` sentinel (used by `gzgets` / `gzopen`) and a non-null
    /// default are checked — the latter proving the guard returns *the caller's*
    /// default rather than hard-coding `NULL`.
    #[cfg(feature = "std")]
    #[test]
    fn guard_ptr_catches_panic_and_passes_value() {
        let mut value: c_int = 42;
        let live: *mut c_int = &raw mut value;

        // Pass-through preserves pointer identity, not merely non-nullness.
        assert_eq!(guard_ptr(ptr::null_mut(), || live), live);

        // A panic yields the NULL sentinel the pointer-returning shims use.
        let nulled =
            with_silenced_panic_hook(|| guard_ptr(ptr::null_mut::<c_int>(), || panic!("boundary")));
        assert!(nulled.is_null());

        // ... and a non-null default is substituted just as faithfully.
        let fallback = with_silenced_panic_hook(|| guard_ptr(live, || panic!("boundary")));
        assert_eq!(fallback, live);

        // The pointee is untouched throughout: the guard moves pointers, never
        // the memory behind them.
        assert_eq!(value, 42);

        // A second pointee type proves the generic parameter is live. `Bytef`
        // is the element type of every zlib buffer, so it is the one that
        // matters for the byte-pointer shims.
        let mut bytes: [Bytef; 2] = [7, 8];
        let byte_ptr: *mut Bytef = bytes.as_mut_ptr();
        assert_eq!(guard_ptr(ptr::null_mut(), || byte_ptr), byte_ptr);
        let byte_nulled =
            with_silenced_panic_hook(|| guard_ptr(ptr::null_mut::<Bytef>(), || panic!("boundary")));
        assert!(byte_nulled.is_null());
        assert_eq!(bytes, [7, 8]);
    }

    /// [`guard_off`] passes a [`z_off64_t`] through unchanged — including the
    /// negative `-1` sentinel of the `gzseek` / `gztell` / `gzoffset` family and
    /// both signed 64-bit extremes — and substitutes its exact default when the
    /// body panics.
    ///
    /// The signedness is the point: a guard that widened or saturated through an
    /// unsigned type would turn a legitimate `-1` result into a huge positive
    /// offset, so `-1` is asserted as a *pass-through* value as well as a
    /// default.
    #[cfg(feature = "std")]
    #[test]
    fn guard_off_catches_panic_and_passes_value() {
        // Pass-through, including the C error sentinel and both extremes.
        assert_eq!(guard_off(-1, || 0), 0);
        assert_eq!(guard_off(-1, || -1), -1);
        // A value beyond 32-bit range: `z_off64_t` is 64-bit on every target,
        // so this must survive unnarrowed.
        assert_eq!(guard_off(-1, || 2_147_483_648), 2_147_483_648);
        assert_eq!(guard_off(0, || z_off64_t::MAX), z_off64_t::MAX);
        assert_eq!(guard_off(0, || z_off64_t::MIN), z_off64_t::MIN);

        // A panic substitutes the `-1` sentinel the gz position family uses.
        let caught = with_silenced_panic_hook(|| guard_off(-1, || panic!("boundary panic")));
        assert_eq!(caught, -1);

        // ... and any other caller-chosen default, bit-exactly.
        let extreme =
            with_silenced_panic_hook(|| guard_off(z_off64_t::MIN, || panic!("boundary panic")));
        assert_eq!(extreme, z_off64_t::MIN);
    }

    /// Pass-through holds for **both** implementations of every guard.
    ///
    /// Each guard exists twice: a `catch_unwind` form under `std` and a
    /// direct-call form under `no_std`, where there is no unwinding to catch.
    /// The panic-substitution tests can only run against the `std` form, so this
    /// deliberately **ungated** test pins the property the two forms share — a
    /// non-panicking body's value reaches the caller bit-exactly and the
    /// `default` argument is ignored on the success path — and is therefore the
    /// only *runtime* coverage the `no_std` forms receive (in the
    /// `--no-default-features` rows, where `std` is off).
    ///
    /// Every `default` below is deliberately distinct from the value returned,
    /// so a guard that substituted its default unconditionally would be caught.
    #[test]
    fn guards_pass_non_panicking_values_through_in_every_feature_row() {
        assert_eq!(guard_int(-2, || 7), 7);
        assert_eq!(guard_int(0, || c_int::MIN), c_int::MIN);

        assert_eq!(guard_ulong(0, || 0xFEED), 0xFEED);
        assert_eq!(guard_ulong(1, || c_ulong::MAX), c_ulong::MAX);

        assert_eq!(guard_off(-1, || 2_147_483_648), 2_147_483_648);
        assert_eq!(guard_off(0, || z_off64_t::MIN), z_off64_t::MIN);

        let mut value: c_int = 42;
        let live: *mut c_int = &raw mut value;
        assert_eq!(guard_ptr(ptr::null_mut(), || live), live);
        assert_eq!(value, 42, "the guard moves pointers, never the pointee");
    }

    /// `init_allocator_prologue` must reproduce C's allocator prologue for all
    /// **four** states of the `zalloc`/`zfree` pair.
    ///
    /// C's text (`deflate.c` L399-L414, `inflate.c` L182-L196, `infback.c`
    /// L36-L50) is three independent statements, and the two `if`s do not
    /// consult each other:
    ///
    /// ```text
    /// strm->msg = Z_NULL;
    /// if (strm->zalloc == 0) { strm->zalloc = zcalloc; strm->opaque = 0; }
    /// if (strm->zfree  == 0)   strm->zfree  = zcfree;
    /// ```
    ///
    /// So every missing half is filled in — including *both* of them — and
    /// `opaque` is cleared on the `zalloc` branch and nowhere else. This test
    /// pins each cell of that matrix, plus the AAP §0.6.5 footprint rule: a
    /// caller who supplied nothing must still end up with the crate's default
    /// (global-allocator) path, which is what `CAllocator::is_builtin_pair`
    /// decides.
    #[test]
    fn prologue_substitutes_every_missing_allocator_half_exactly_as_c_does() {
        let cookie = ptr::without_provenance_mut::<c_void>(0xC0FF_EE00);
        let stale = c"stale".as_ptr().cast_mut();

        // A genuine caller pair. Never invoked: this test only observes which
        // halves the prologue publishes and how they are classified.
        extern "C" fn caller_zalloc(
            _opaque: *mut c_void,
            _items: c_uint,
            _size: c_uint,
        ) -> *mut c_void {
            ptr::null_mut()
        }
        extern "C" fn caller_zfree(_opaque: *mut c_void, _address: *mut c_void) {}

        // --- neither half supplied (the zeroed `z_stream`) -------------------
        let mut strm = zeroed_stream();
        strm.msg = stale;
        // SAFETY: `strm` is a live, exclusively-owned `z_stream`; the prologue
        // only reads and writes its plain `Copy` fields.
        let alloc = unsafe { init_allocator_prologue(&mut strm) };
        assert!(strm.msg.is_null(), "`strm->msg = Z_NULL` is unconditional");
        assert!(
            strm.zalloc.is_some() && strm.zfree.is_some(),
            "C fills in BOTH missing halves, so the caller's stream publishes a \
             complete pair (`deflate.c` L400-L414)"
        );
        assert!(
            publishes_builtin_alloc_pair(&strm),
            "the published pair must be this crate's own built-ins"
        );
        assert!(
            alloc.is_builtin_pair(),
            "a pair made solely of the crate's substitutes is not a caller hook"
        );
        assert!(
            !alloc.hook().is_active(),
            "a hookless caller must keep the crate's default global-allocator \
             path (AAP §0.6.5)"
        );
        assert!(
            !alloc.reserves_state_footprint(),
            "a hookless caller's engine-state footprint must stay exactly what \
             it has always been (AAP §0.6.5)"
        );
        assert!(!alloc.is_half_present());

        // --- `zalloc` only: the caller's half is honored, `zfree` filled in ---
        let mut strm = zeroed_stream();
        strm.zalloc = Some(caller_zalloc);
        strm.opaque = cookie;
        // SAFETY: as above.
        let alloc = unsafe { init_allocator_prologue(&mut strm) };
        assert!(strm.zfree.is_some(), "the missing `zfree` is substituted");
        assert!(
            ptr::eq(strm.opaque, cookie),
            "C does NOT touch `opaque` on the `zfree` branch (`deflate.c` \
             L408-L413)"
        );
        assert!(
            alloc.hook().is_active(),
            "one caller-supplied half makes the completed pair a live hook, so a \
             failing caller `zalloc` still surfaces as Z_MEM_ERROR"
        );
        assert!(!alloc.is_half_present());

        // --- `zfree` only: `zalloc` filled in AND `opaque` cleared -----------
        let mut strm = zeroed_stream();
        strm.zfree = Some(caller_zfree);
        strm.opaque = cookie;
        // SAFETY: as above.
        let alloc = unsafe { init_allocator_prologue(&mut strm) };
        assert!(strm.zalloc.is_some(), "the missing `zalloc` is substituted");
        assert!(
            strm.opaque.is_null(),
            "C clears `opaque` on exactly the `zalloc` branch, because the cookie \
             belonged to the allocator being replaced (`deflate.c` L405-L406)"
        );
        assert!(alloc.hook().is_active());
        assert!(!alloc.is_half_present());

        // --- both halves supplied: nothing is substituted at all -------------
        let mut strm = zeroed_stream();
        strm.zalloc = Some(caller_zalloc);
        strm.zfree = Some(caller_zfree);
        strm.opaque = cookie;
        // SAFETY: as above.
        let alloc = unsafe { init_allocator_prologue(&mut strm) };
        assert!(
            ptr::eq(strm.opaque, cookie),
            "a complete pair is left exactly as the caller supplied it"
        );
        assert!(
            alloc.hook().is_active() && !alloc.is_builtin_pair(),
            "a complete caller pair backs every buffer"
        );
        assert!(!alloc.is_half_present());
    }

    /// `is_builtin_pair` must recognize the crate's own substitutes and must not
    /// mistake anything else for them.
    ///
    /// The predicate rests on `core::ptr::fn_addr_eq`, so this is also the guard
    /// that would catch a toolchain or codegen arrangement under which the
    /// comparison stopped identifying the built-ins — the failure mode would
    /// otherwise be silent (a hookless caller's buffers moving from the global
    /// allocator to `malloc`, changing the footprint AAP §0.6.5 pins).
    #[test]
    fn is_builtin_pair_identifies_only_the_crate_substitutes() {
        let builtin = CAllocator {
            zalloc: Some(crate::ffi::alloc::default_zalloc),
            zfree: Some(crate::ffi::alloc::default_zfree),
            opaque: ptr::null_mut(),
        };
        assert!(
            builtin.is_builtin_pair(),
            "fn_addr_eq must identify the crate's own built-in allocator halves"
        );

        let absent = CAllocator {
            zalloc: None,
            zfree: None,
            opaque: ptr::null_mut(),
        };
        assert!(
            !absent.is_builtin_pair(),
            "an absent pair is not a built-in pair (it is already hookless)"
        );

        // A caller pair that reports out-of-memory: never invoked here, only its
        // address is compared.
        extern "C" fn caller_zalloc(
            _opaque: *mut c_void,
            _items: c_uint,
            _size: c_uint,
        ) -> *mut c_void {
            ptr::null_mut()
        }
        extern "C" fn caller_zfree(_opaque: *mut c_void, _address: *mut c_void) {}

        let foreign = CAllocator {
            zalloc: Some(caller_zalloc),
            zfree: Some(caller_zfree),
            opaque: ptr::null_mut(),
        };
        assert!(
            !foreign.is_builtin_pair(),
            "a genuine caller pair must never be mistaken for the substitutes"
        );
        assert!(foreign.hook().is_active());
        assert!(foreign.reserves_state_footprint());

        // Mixed pairs — one caller half, one substitute — are live hooks.
        let mixed = CAllocator {
            zalloc: Some(caller_zalloc),
            zfree: Some(crate::ffi::alloc::default_zfree),
            opaque: ptr::null_mut(),
        };
        assert!(!mixed.is_builtin_pair());
        assert!(mixed.hook().is_active());
    }

    // -- test helpers -------------------------------------------------------

    /// Builds a fully-zeroed `z_stream` for tests (the state a C caller would
    /// `memset` before `deflateInit`/`inflateInit`).
    fn zeroed_stream() -> z_stream {
        z_stream {
            next_in: ptr::null(),
            avail_in: 0,
            total_in: 0,
            next_out: ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: ptr::null_mut(),
            state: ptr::null_mut(),
            zalloc: None,
            zfree: None,
            opaque: ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        }
    }
}
