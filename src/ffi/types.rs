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

use crate::gz_header::GzHeader;
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
    /// that substitution byte-for-byte, so a stream this crate initialized
    /// **always** carries a complete pair and this predicate is `false` for it.
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
}

/// Reproduces C's `*Init*_` allocator prologue on a caller's [`z_stream`] and
/// returns the resulting [`CAllocator`].
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
/// # A wholly absent pair is left absent
///
/// The substitution is confined to the *half*-present case. When the caller
/// supplied **neither** half, both fields are left null, which selects the
/// crate's global-allocator path — the built-in allocator for this port, exactly
/// as `zcalloc`/`zcfree` are for C. That is required by AAP §0.6.5: charging the
/// engine-state footprint to the caller happens only when they actually installed
/// a hook, so that a hookless C caller's footprint stays byte-for-byte what it
/// has always been. See [`Allocator::reserves_state_footprint`], whose entire
/// purpose is that requirement. Substituting the built-ins here would make every
/// hookless stream pay a real `sizeof(deflate_state)` reservation it does not pay
/// today, purely for accounting symmetry with a hook the caller never supplied.
///
/// One consequence is observable and is recorded here rather than glossed over:
/// after a *hookless* initialization, reference C leaves `strm->zalloc` and
/// `strm->zfree` holding its own built-ins, whereas this crate leaves both null.
/// It affects nothing else — every return code, every `msg`/`opaque` mutation,
/// every allocation count and every byte of output is identical, and the null
/// fields are internally consistent (nothing in this crate rejects a stream for
/// carrying them, so `deflateCopy`/`inflateCopy` and every other entry point
/// behave the same). `zlib.h` documents these three fields as *inputs* the
/// application initializes before the call (L140, L235, L385, L551, L863, L1117)
/// and never as outputs the library publishes, so no documented contract depends
/// on the difference. This is a consequence of the AAP §0.6.5 footprint
/// requirement, not an independently authorized divergence.
///
/// # Safety
///
/// `strm` must reference a validly-initialized [`z_stream`] that the caller owns
/// exclusively for the duration of the call. Only plain `Copy` fields are read
/// and written (`msg`, `zalloc`, `zfree`, `opaque`); no hook pointer is
/// dereferenced here.
#[inline]
pub unsafe fn init_allocator_prologue(strm: &mut z_stream) -> CAllocator {
    // Step 1 — `strm->msg = Z_NULL;` (deflate.c L399, inflate.c L182,
    // infback.c L36). Unconditional, and ahead of the allocator inspection, so
    // every subsequent `return` in the initializer reports a clean `msg`.
    strm.msg = core::ptr::null_mut();

    // Whether the caller supplied exactly one half. Captured *before* either
    // branch runs, so step 3 cannot observe the pointer step 2 just installed.
    let complete_the_pair = strm.zalloc.is_some() != strm.zfree.is_some();

    // Step 2 — `if (strm->zalloc == 0) { strm->zalloc = zcalloc; strm->opaque = 0; }`
    // (deflate.c L400-L407, inflate.c L183-L190, infback.c L37-L44).
    //
    // The `complete_the_pair` conjunct is the one place this differs from C's
    // literal text, and it is deliberate: it confines the substitution to the
    // half-present case and leaves a WHOLLY absent pair absent, which selects
    // this crate's global-allocator path — the built-in allocator here, exactly
    // as `zcalloc`/`zcfree` are C's, and the case AAP §0.6.3 and §0.6.5 govern
    // (see this function's doc comment). The behavior a C caller can observe for
    // a half-present pair is identical either way; for a wholly absent pair the
    // conjunct is what keeps a hookless caller's allocation count and footprint
    // unchanged.
    if complete_the_pair && strm.zalloc.is_none() {
        strm.zalloc = Some(crate::ffi::alloc::default_zalloc);
        strm.opaque = core::ptr::null_mut();
    }

    // Step 3 — `if (strm->zfree == 0) strm->zfree = zcfree;` (deflate.c
    // L408-L413, inflate.c L191-L196, infback.c L45-L50). Note that C does NOT
    // touch `opaque` on this branch.
    if complete_the_pair && strm.zfree.is_none() {
        strm.zfree = Some(crate::ffi::alloc::default_zfree);
    }

    // SAFETY: `strm` is a valid `&z_stream`; `from_stream` only copies the plain
    // `Copy` allocator fields out and never dereferences a hook pointer.
    unsafe { CAllocator::from_stream(strm) }
}

impl Allocator for CAllocator {
    /// Allocates a zero-initialized buffer of `count` elements, **routing
    /// through the caller's `zalloc` when supplied** (AAP §0.6.3, "has-hook
    /// clause").
    ///
    /// The returned [`AllocBuffer`] is a [`Foreign`](AllocBuffer::Foreign)
    /// region carved from the caller's `zalloc` (and released through their
    /// `zfree` on drop) whenever this [`CAllocator`] carries an active
    /// [`hook`](Allocator::hook); with **both** hooks null, or for an empty
    /// request, it uses a global-allocator [`Vec`](AllocBuffer::Owned), matching
    /// AAP §0.6.3's "otherwise `std::alloc` is used" clause and C's substitution
    /// of `zcalloc`/`zcfree` for a wholly absent pair (`deflate.c` L400-L414).
    ///
    /// A *half*-present pair never reaches this method from a C entry point:
    /// [`init_allocator_prologue`] has already completed it by substituting the
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
    #[inline]
    fn hook(&self) -> AllocHook {
        AllocHook::new(self.zalloc, self.zfree, self.opaque)
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
/// # Use [`init_allocator_prologue`] in an `*Init*_` shim
///
/// This constructor reads the triple exactly as it finds it, so it is the right
/// tool only where that triple is already known-complete — for example when it
/// has been cloned out of an initialized handle. The `deflateInit2_`,
/// `inflateInit2_`, and `inflateBackInit_` shims must additionally reproduce C's
/// initialization prologue (clear `msg`, substitute a missing half of the pair),
/// so they call [`init_allocator_prologue`] and hand its result to
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
/// init shim now installs a tagged `#[repr(C)]` handle.
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

/// Copies at most `cap` content bytes of `src` to `dst` and NUL-terminates when
/// room remains, mirroring the C `inflateGetHeader` name/comment truncation
/// (the NUL is stored only if the content did not fill the whole capacity).
///
/// # Safety
///
/// `dst` must be non-null and point at a buffer of at least `cap` writable
/// bytes.
unsafe fn write_cstr_bounded(dst: *mut c_uchar, src: &[u8], cap: usize) {
    if cap == 0 {
        return;
    }
    let n = core::cmp::min(src.len(), cap);
    // SAFETY: `dst` has `cap` writable bytes and `n <= cap`, so the copy stays
    // in bounds; `src` and `dst` are distinct buffers.
    unsafe {
        ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
    }
    if n < cap {
        // SAFETY: `n < cap`, so `dst.add(n)` addresses a writable byte within
        // the buffer.
        unsafe {
            *dst.add(n) = 0;
        }
    }
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
        // `c_uint` is `u32` on every Rust target, so these assign directly.
        // Carrying `extra_len` across preserves a value the caller may already
        // have set: on the `inflateGetHeader` path the engine overwrites it from
        // the stream's declared XLEN only when the header actually carries an
        // `FEXTRA` field, so a stream without one must leave the caller's value
        // intact (`inflate.c` L596-L606).
        extra_len: h.extra_len,
        extra_max: h.extra_max,
        name_max: h.name_max,
        comm_max: h.comm_max,
    })
}

/// Writes an idiomatic [`GzHeader`] back into a
/// caller-provided raw [`gz_header`] (as used by `inflateGetHeader`), honoring
/// the caller's `extra_max`/`name_max`/`comm_max` capacities and never
/// overrunning the caller's buffers.
///
/// Scalar fields (`text`, `time`, `xflags`, `os`, `hcrc`, `done`) are always
/// written, as is `extra_len`, which receives the extra field's **declared**
/// length even when the copy into `extra` is truncated to `extra_max` — and even
/// when `extra` is null. This matches C, where `inflate` writes `head->extra_len`
/// from the header's `XLEN` independently of the clamped copy (`inflate.c`
/// L599-L600 vs L614-L621). `zlib.h` specifies the truncation signal
/// (`extra_len > extra_max`); the null-`extra` length query is not spelled out
/// there but falls out of that same unconditional write, so reference zlib
/// permits it de facto and this port must too.
///
/// # Safety
///
/// `head` must be null or point at a valid [`gz_header`]. When its `extra`/
/// `name`/`comment` pointers are non-null, each must address at least
/// `extra_max`/`name_max`/`comm_max` writable bytes respectively.
pub unsafe fn write_gz_header_from_idiomatic(head: *mut gz_header, src: &GzHeader) {
    if head.is_null() {
        return;
    }
    // SAFETY: `head` is non-null and, per the contract, points at a valid,
    // uniquely-borrowed `gz_header` whose buffers are sized by its `*_max`
    // fields.
    let h = unsafe { &mut *head };

    h.text = c_int::from(src.text);
    h.time = src.time as c_ulong;
    // `xflags`/`os` are C `int` (== `i32`), assigned directly.
    h.xflags = src.xflags;
    h.os = src.os;
    h.hcrc = c_int::from(src.hcrc);
    h.done = c_int::from(src.done);

    // Publish the **declared** extra-field length, unconditionally — not the
    // number of bytes that fit. C writes `head->extra_len` from the stream's
    // 16-bit `XLEN` in the `EXLEN` state (`inflate.c` L599-L600), gated on
    // neither `extra`'s nullity nor `extra_max`'s size, and separately clamps the
    // copy (`inflate.c` L614-L621). Reproducing both halves is what makes
    // `extra_len > extra_max` a usable truncation signal per `zlib.h`, and what
    // lets a caller pass a null `extra` purely to learn the length and get the
    // real value instead of zero — de-facto reference-zlib behavior that falls
    // out of the same unconditional write. When the stream
    // carried no `FEXTRA` field the engine left this at whatever the caller
    // supplied, so writing it back is then a no-op.
    h.extra_len = src.extra_len as c_uint;
    if let Some(extra) = &src.extra {
        if !h.extra.is_null() {
            let cap = h.extra_max as usize;
            let n = core::cmp::min(cap, extra.len());
            // SAFETY: `h.extra` has `extra_max` writable bytes and `n <= cap`.
            unsafe {
                ptr::copy_nonoverlapping(extra.as_ptr(), h.extra, n);
            }
        }
    } else {
        // No extra field: null the caller's pointer, as C does with
        // `state->head->extra = Z_NULL` on the no-`FEXTRA` branch (`inflate.c`
        // L605-L606). That assignment is how a C caller distinguishes "the header
        // declared no extra field" from "it declared one"; leaving a stale
        // non-null pointer would misreport an absent field as present.
        //
        // Both ways of reaching `None` want exactly this. If the caller supplied
        // no buffer, `inflateGetHeader` set the slot to `None` and the pointer is
        // already null, so this is a no-op. If the stream carried no `FEXTRA`,
        // the engine set it to `None` and nulling is precisely C's behavior. A
        // stream whose header has not yet reached the `EXLEN` state — including a
        // raw zlib stream, where `done` is reported as `-1` — keeps the slot at
        // `Some(empty)`, so nothing is nulled prematurely, matching C, which
        // likewise only assigns once it reaches that state.
        h.extra = ptr::null_mut();
    }
    if let Some(name) = &src.name {
        // Nested `if` rather than an `if let ... && ...` chain, which is
        // unstable before Rust 1.88 (this crate's MSRV is 1.85).
        if !h.name.is_null() {
            // SAFETY: `h.name` has `name_max` writable bytes.
            unsafe {
                write_cstr_bounded(h.name, name, h.name_max as usize);
            }
        }
    }
    if let Some(comment) = &src.comment {
        // Nested `if` rather than an `if let ... && ...` chain, which is
        // unstable before Rust 1.88 (this crate's MSRV is 1.85).
        if !h.comment.is_null() {
            // SAFETY: `h.comment` has `comm_max` writable bytes.
            unsafe {
                write_cstr_bounded(h.comment, comment, h.comm_max as usize);
            }
        }
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
    /// enough for the `u8`/`u16`/`u32` element types the engines request and for
    /// the 4-byte-aligned probe type used below.
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
