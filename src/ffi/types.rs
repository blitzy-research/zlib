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
use alloc::collections::TryReserveError;
use alloc::vec::Vec;

#[cfg(feature = "gzip")]
use crate::gz_header::{ForeignByteSink, ForeignGzHeader, ForeignGzHeaderSink};
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
///
/// # A freshly opened handle reads deterministically here
///
/// On a handle straight out of `gzopen`, [`next`](Self::next) is null and
/// [`have`](Self::have) is zero. Reference C leaves `next` **uninitialized** in
/// that state: `gzlib.c` allocates its state with plain `malloc`, never `calloc`,
/// and the field is only ever assigned on the first fetch inside `gzread.c`. What
/// a caller reads before that first fetch is therefore whatever the allocator
/// happened to hand back — this was measured as null on glibc, whose fresh pages
/// are zeroed, and as garbage on the Windows heap under the same driver.
///
/// The C `gzgetc` macro is unaffected, because it tests `have` first and `have` is
/// genuinely zero, so it takes the function path. The exposure is to a caller that
/// inspects the public prefix directly, which the struct's very existence invites.
/// This crate initializes the whole prefix, so such a caller reads a defined value
/// on every platform. Like the null-argument and use-after-close cases, this is a
/// difference in *C's* favour being removed, not a compatibility risk: no
/// conforming use can observe it.
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
/// # How the engine *state* is charged
///
/// The working buffers are not the only thing the hook is billed for. The
/// `deflate_state`/`inflate_state` charge is made by `EngineReservation` in
/// `src/stream.rs`, whose arm is selected by one allocator predicate,
/// [`reserves_state_footprint`](Allocator::reserves_state_footprint):
///
/// | `reserves_state_footprint()` | Arm | What the allocator is asked for |
/// |---|---|---|
/// | `false` | `Global` | nothing — the state is a plain `Box<E>` on the Rust global heap |
/// | `true` | `Charged` | C's own `(1, sizeof(deflate_state))` / `(1, sizeof(struct inflate_state))` |
///
/// [`CAllocator`] reports `reserves_state_footprint() == hook().is_active()`
/// (see its [`Allocator`] impl), so which row a C caller reaches is fixed by
/// whether a hook is installed:
///
/// * **Null hooks — or a pair made up solely of the crate's built-in
///   substitutes, which is what a hookless caller ends up publishing —** take
///   the `Global` arm. The state and every buffer stay on the Rust global heap,
///   keeping a hookless caller's allocation count and footprint byte-for-byte
///   what they have always been (AAP §0.6.5).
/// * **An installed hook** takes the `Charged` arm, so the caller's `zalloc` is
///   billed for the state where C makes its
///   `ZALLOC(strm, 1, sizeof(deflate_state))` (`deflate.c` L440) — with C's own
///   `items` **and** C's own `size`, so an arena sized from C's header serves it
///   exactly as it serves reference zlib — and the region is handed back through
///   their `zfree` after the working buffers, C's teardown order.
///
/// The charged region is held, unread, purely as the caller's accounting token;
/// the state value itself sits beside it on the global heap. That split is
/// forced rather than chosen: a block of C's `sizeof` cannot hold this port's
/// legitimately larger state, so billing C's byte count and hosting the state in
/// the same block are mutually exclusive. Requesting this port's own
/// `size_of::<DeflateState>()` instead made a conforming allocator sized for
/// C refuse, turning a successful `deflateInit2_` into `Z_MEM_ERROR` — a real
/// drop-in defect. Everything the caller's allocator can observe (request count,
/// argument pair, sequence position, failure timing, release order) is C-exact;
/// only the address the state occupies differs, and no zlib contract exposes it.
///
/// What remains on the global heap in *both* arms is the small owning handle:
/// the `Box` holding `EngineBox`, which is what lets the stream's state slot stay
/// a single non-generic boxed trait object. That is one small allocation per
/// stream, requested fallibly, so a refusal surfaces as `Z_MEM_ERROR` and never
/// aborts; it never re-requests the caller's region, so C's allocation *count* is
/// unaffected by it.
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
/// # Binding time — a documented divergence from C
///
/// A hook is bound to the buffers it allocates **at initialization time only**.
/// `deflateInit*` / `inflateInit*` / `inflateBackInit*` read the stream's
/// `zalloc`/`zfree`/`opaque` triple once; every buffer those calls create — and
/// every buffer created later on the same stream, such as inflate's deferred
/// window — uses that captured hook. Writing new values into `strm.zalloc` or
/// `strm.zfree` after a successful init has **no effect**.
///
/// Reference C re-reads the fields on every `ZALLOC`, so a hook installed after
/// init does serve later allocations there. That difference is deliberate and is
/// *sounder* than matching C, not merely cheaper: C's own accounting shows it
/// handing a caller-supplied `zfree` a pointer that came from C's internal
/// `zcalloc`, because the stream state predates the hook. This crate's
/// [`AllocBuffer`] always frees through the hook that
/// allocated it, so no such mismatch is representable. `zlib.h` documents these
/// fields as init inputs and never sanctions changing them on a live stream, so no
/// conforming caller is affected — see the fuller discussion in the
/// [`crate::stream`] module documentation.
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
/// [`HandleKind`] at offset 0 followed by the owning stream's address, both
/// readable regardless of the concrete handle type behind the opaque `state`
/// pointer.
///
/// `owner` is the Rust counterpart of C's `s->strm` back-pointer, which
/// `deflateInit2_` sets (`deflate.c` L444), `deflateCopy` re-points at the
/// destination (`deflate.c` L1340), `inflateInit2_` sets (`inflate.c` L203), and
/// `deflateStateCheck` / `inflateStateCheck` then compare against the incoming
/// `strm`. Without it a *same-kind* handle can be reached through a **different**
/// `z_stream` — the exact situation a caller creates by copying the 14-field
/// struct — and both copies then claim ownership of one allocation, so whichever
/// `*End` runs second frees it again or reads it after free. The tag alone
/// cannot detect that, because both structs carry the same tag.
///
/// Every concrete handle declares these two fields first, in this order, with
/// these types, and `#[repr(C)]` fixes their offsets — which is what makes it
/// sound to read the prefix through this type before the concrete handle type is
/// known. [`TaggedHandle`] is the contract that keeps them in step.
#[repr(C)]
struct HandleHeader {
    kind: HandleKind,
    owner: *const z_stream,
}

/// The contract every boxed FFI engine handle satisfies so that installation and
/// validation can be written once instead of once per engine.
///
/// An implementor must declare, as its first two fields and in this order, a
/// `kind: HandleKind` and an `owner: *const z_stream`, and must be `#[repr(C)]`
/// so those fields sit at the offsets [`HandleHeader`] reads. In exchange it
/// gets [`install_handle`] (which cannot forget to bind the owner) and
/// [`handle_prefix_valid`] (the shared clauses of C's state check).
pub(crate) trait TaggedHandle: Sized {
    /// The discriminant this handle type always carries at offset 0.
    const KIND: HandleKind;

    /// Records the address of the `z_stream` this handle is installed into.
    fn set_owner(&mut self, owner: *const z_stream);

    /// Whether the engine state behind this handle carries a status/mode value
    /// reference zlib's state check accepts.
    ///
    /// This is the engine-specific clause of C's predicate:
    /// `deflate.c` L538-L556 enumerates the eight legal `status` values, and
    /// `inflate.c` L88-L97 range-checks `mode` against `HEAD..=SYNC`.
    fn engine_status_is_c_valid(&self) -> bool;
}

/// Installs a freshly built tagged handle into [`z_stream::state`], binding it to
/// `strm` as its owner.
///
/// This is the single place a handle becomes reachable through a `z_stream`, and
/// it performs C's `s->strm = strm` (`deflate.c` L444, `inflate.c` L203) in the
/// same statement pair that publishes the pointer — so the owner can never be
/// left unset by an initializer that forgets it.
///
/// # Safety
///
/// `strm.state` must not already hold a live handle: the previous handle would be
/// leaked. Every caller is an `*Init*` shim that has just verified the field is
/// null or has already reclaimed what was there.
#[inline]
pub(crate) unsafe fn install_handle<T: TaggedHandle>(strm: &mut z_stream, mut handle: Box<T>) {
    handle.set_owner(strm as *const z_stream);
    // SAFETY: ownership of the box is transferred to the raw `state` field, from
    // which exactly one `*End`/reclaim path reconstitutes it.
    strm.state = unsafe { state_ptr_from_box(handle) };
}

/// The *identity* clauses shared by every engine's C state check: an installed
/// handle, the expected engine kind, and a matching owner.
///
/// This is the composable core the three engine-specific predicates are built
/// from, because reference zlib does **not** apply the same clauses to all three
/// engines — `inflateBack` deliberately checks less than `inflate` (see
/// [`inflate_back_state_check`]). Splitting the clauses here lets each engine
/// reproduce its own C predicate exactly instead of being flattened onto the
/// strictest one.
///
/// Returns `Some(&T)` only when the handle behind [`z_stream::state`] may be
/// reinterpreted as `T` and belongs to this very stream. **Nothing outside `strm`
/// and its own state handle is touched** — no auxiliary caller pointer is read,
/// dereferenced, or turned into a slice — which is the property that lets a shim
/// run this *before* bridging a `dictionary`, `head`, or buffer pointer that may
/// be stale. C rejects such a call without touching those pointers, so Rust must
/// not form a reference to them either: constructing a slice over invalid memory
/// is undefined behavior even when the slice is never read.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by
/// [`install_handle`], so its `#[repr(C)]` prefix is readable.
#[inline]
#[must_use]
pub(crate) unsafe fn handle_owner_valid<T: TaggedHandle>(strm: &z_stream) -> Option<&T> {
    // Clause 1 (`strm == Z_NULL`) is discharged by the caller: holding a
    // `&z_stream` at all means the pointer was non-null.
    //
    // Clause `s == Z_NULL`, plus the Rust-only kind tag, which is strictly
    // stronger than anything C can check and prevents reinterpreting a different
    // engine's allocation.
    let (kind, owner) =
        // SAFETY: delegated prefix read; see `peek_handle_prefix`.
        unsafe { peek_handle_prefix(strm) }?;
    if kind != T::KIND {
        return None;
    }

    // Clause `s->strm != strm` — the owner check (`deflate.c` L544,
    // `inflate.c` L94).
    if !core::ptr::eq(owner, strm) {
        return None;
    }

    // SAFETY: the tag confirms the allocation really is a live `T`, and the
    // returned shared borrow is tied to `strm`, which owns it.
    Some(unsafe { &*(strm.state as *const T) })
}

/// The full symmetric predicate shared by `deflateStateCheck` (`deflate.c`
/// L538-L556) and `inflateStateCheck` (`inflate.c` L88-L97): a live allocator
/// pair, the identity clauses of [`handle_owner_valid`], and an engine
/// status/mode value C would accept.
///
/// Inherits [`handle_owner_valid`]'s guarantee that nothing outside `strm` and
/// its own handle is touched.
///
/// # Safety
///
/// A non-null [`z_stream::state`] must point at a live handle installed by
/// [`install_handle`], so its `#[repr(C)]` prefix is readable.
#[inline]
#[must_use]
pub(crate) unsafe fn handle_prefix_valid<T: TaggedHandle>(strm: &z_stream) -> bool {
    // Clause `strm->zalloc == 0 || strm->zfree == 0`. Both halves are published
    // by `init_allocator_prologue`, so a successfully initialized stream always
    // has them; a caller who zeroes one afterwards is telling the library its
    // allocator is gone, and C refuses rather than calling through a null hook.
    if strm.zalloc.is_none() || strm.zfree.is_none() {
        return false;
    }

    // SAFETY: delegated; the identity clauses read only the handle prefix.
    unsafe { handle_owner_valid::<T>(strm) }.is_some_and(TaggedHandle::engine_status_is_c_valid)
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
    /// Address of the `z_stream` this handle is installed into — C's `s->strm`
    /// (`deflate.c` L444). MUST be the second field. Written by
    /// [`install_handle`]; null until then.
    owner: *const z_stream,
    /// The idiomatic deflate stream this handle owns.
    pub zs: ZStream<CAllocator>,
    /// The caller's live `gz_header`, as handed to `deflateSetHeader` — C's
    /// `s->gzhead` (`deflate.c` L717). Null when no header is registered.
    ///
    /// Only the *pointer* is kept, never a copy: every field is re-read from the
    /// caller's struct each time the engine needs it, so mutations made after
    /// registration but before emission are honored exactly as in C
    /// (`deflate.c` L893-L907, L1092-L1188). Holding a raw pointer requires no
    /// `unsafe`; every field access is confined to `read_gz_header_source`, which
    /// reads through `addr_of!` projections and never forms a reference over the
    /// caller's struct or its payload buffers.
    #[cfg(feature = "gzip")]
    pub(crate) head: *mut gz_header,
}

impl DeflateHandle {
    /// Wraps a freshly built deflate stream, tagging it as a deflate handle.
    ///
    /// The owner is left null: it is bound by `install_handle` at the moment
    /// the handle becomes reachable through a `z_stream`, so a handle that has
    /// not been installed can never pass `deflate_state_check`.
    #[inline]
    #[must_use]
    pub fn new(zs: ZStream<CAllocator>) -> Self {
        Self {
            kind: HandleKind::DEFLATE,
            owner: ptr::null(),
            zs,
            #[cfg(feature = "gzip")]
            head: ptr::null_mut(),
        }
    }
}

impl TaggedHandle for DeflateHandle {
    const KIND: HandleKind = HandleKind::DEFLATE;

    #[inline]
    fn set_owner(&mut self, owner: *const z_stream) {
        self.owner = owner;
    }

    #[inline]
    fn engine_status_is_c_valid(&self) -> bool {
        use crate::deflate::state::DeflateStream;
        self.zs
            .deflate_state()
            .is_some_and(|st| st.status.is_c_valid())
    }
}

/// C's `deflateStateCheck` (`deflate.c` L538-L556), reproduced clause for clause.
///
/// Returns `true` when reference zlib would have accepted `strm` — i.e. when C's
/// `deflateStateCheck` would have returned 0. Every `deflate*` shim that reaches
/// the engine runs this first, and, critically, **runs it before bridging any
/// auxiliary caller pointer**: C rejects an invalid stream without reading the
/// `dictionary`, `head`, `pending`, or buffer pointers passed alongside it, so a
/// stale non-null pointer must not be turned into a Rust slice or reference
/// either. Forming a reference to invalid memory is undefined behavior even if
/// the reference is never read.
///
/// # Safety
///
/// `strm` must be a valid `z_stream`, and a non-null [`z_stream::state`] must
/// point at a live handle installed by [`install_handle`].
#[inline]
#[must_use]
pub(crate) unsafe fn deflate_state_check(strm: &z_stream) -> bool {
    // SAFETY: delegated; every clause is discharged there.
    unsafe { handle_prefix_valid::<DeflateHandle>(strm) }
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
    // SAFETY: delegated prefix read; see `peek_handle_prefix`.
    unsafe { peek_handle_prefix(strm) }.map(|(kind, _)| kind)
}

/// Reads the `#[repr(C)]` prefix of the installed state handle as
/// `(kind, owner)`, or [`None`] if no handle is installed.
///
/// # Safety
///
/// If [`z_stream::state`] is non-null it must point at a live handle whose first
/// two fields are a [`HandleKind`] and a `*const z_stream`. Every FFI init shim
/// installs such a handle through [`install_handle`], so the prefix is always
/// present and both fields are simply read and compared — no enum-validity or
/// pointer-validity requirement applies to either.
#[inline]
#[must_use]
pub(crate) unsafe fn peek_handle_prefix(strm: &z_stream) -> Option<(HandleKind, *const z_stream)> {
    if strm.state.is_null() {
        None
    } else {
        // SAFETY: `state` is non-null and points at a live handle allocation
        // whose `#[repr(C)]` prefix is a `HandleHeader`; reading a `u64` and a
        // raw pointer out of it is valid and carries no validity requirement on
        // the values themselves.
        let header = unsafe { &*(strm.state as *const HandleHeader) };
        Some((header.kind, header.owner))
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
    // SAFETY: delegated; runs every clause of C's `deflateStateCheck` and reads
    // nothing outside `strm` and its own handle.
    if !unsafe { deflate_state_check(strm) } {
        return None;
    }
    // SAFETY: the check confirmed the tag and the owner, so the allocation really
    // is a live `DeflateHandle`; the borrow is tied to `strm`, so it cannot alias
    // for its lifetime.
    let handle = unsafe { &mut *(strm.state as *mut DeflateHandle) };
    Some(&mut handle.zs)
}

/// Borrows the whole installed [`DeflateHandle`], not just its stream, after
/// running every clause of C's `deflateStateCheck`.
///
/// [`deflate_state`] is the right accessor for shims that need only the engine;
/// this one additionally exposes [`DeflateHandle::head`], the caller's live
/// `gz_header` pointer, which the `deflate` and `deflateBound` shims must re-read
/// on every call to reproduce C's lazy header reads.
///
/// # Safety
///
/// Same contract as [`deflate_state`]: a non-null [`z_stream::state`] must point
/// at a live handle installed by an FFI init shim.
#[inline]
pub(crate) unsafe fn deflate_handle(strm: &mut z_stream) -> Option<&mut DeflateHandle> {
    // SAFETY: delegated; reads nothing outside `strm` and its own handle.
    if !unsafe { deflate_state_check(strm) } {
        return None;
    }
    // SAFETY: the check confirmed the tag and the owner, so the allocation really
    // is a live `DeflateHandle`; the borrow is tied to `strm` and cannot alias.
    Some(unsafe { &mut *(strm.state as *mut DeflateHandle) })
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
    // SAFETY: delegated; runs every clause of C's `deflateStateCheck`, including
    // the owner comparison. That clause is what makes reclaim safe: a caller who
    // copied the 14-field `z_stream` holds a second struct pointing at the SAME
    // handle, and freeing through the copy would leave the original with a
    // dangling `state`. C returns `Z_STREAM_ERROR` for the copy (`deflate.c`
    // L538-L556 via `s->strm != strm`) and this reproduces that refusal, so the
    // allocation is released exactly once, through its owner.
    if !unsafe { deflate_state_check(strm) } {
        return None;
    }
    // SAFETY: the check confirmed a live, owner-matched `Box<DeflateHandle>`;
    // reconstitute it exactly once and null the field to prevent a double free.
    let boxed = unsafe { Box::from_raw(strm.state as *mut DeflateHandle) };
    strm.state = ptr::null_mut();
    Some(boxed)
}

// --- Cross-width scalar bridging -------------------------------------------

/// Narrow a [`uLong`] (C `unsigned long`) to the `u32` in which zlib actually
/// carries a checksum value or a gzip header timestamp.
///
/// C `unsigned long` is 64-bit on LP64 targets (e.g. 64-bit Linux) and 32-bit on
/// both LLP64 targets (e.g. 64-bit Windows) and ILP32 targets (e.g. `i686`), yet
/// every value crossing this boundary —
/// an Adler-32, a CRC-32, a `crc32_combine` operator, a `gz_header::time` — is
/// defined by the format as exactly 32 bits. So the `as` cast is a genuine
/// truncation on LP64 and the identity on LLP64 and ILP32, and it is correct on
/// all three:
/// reference zlib relies on the same property, its own `uLong` checksums never
/// carrying more than 32 significant bits.
///
/// Being the identity on LLP64 and ILP32 is precisely what makes
/// [`clippy::unnecessary_cast`] fire on those targets while staying silent on LP64
/// — a lint
/// that is right about the expression and wrong about the program, and one that
/// a Linux-only CI lane never surfaces. Confining the conversion, and with it the
/// single localized exemption, to one function keeps every checksum and header
/// shim portable and lint-clean on all three target families without scattering
/// `#[allow]` across eighteen call sites. This mirrors `off_to_i64` in
/// `src/ffi/util.rs`, which solves the same problem in the widening direction for
/// `z_off_t`.
///
/// [`clippy::unnecessary_cast`]: https://rust-lang.github.io/rust-clippy/master/index.html#unnecessary_cast
#[inline]
#[must_use]
#[allow(clippy::unnecessary_cast)]
pub(crate) fn ulong_to_u32(value: uLong) -> u32 {
    value as u32
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
    // `consumed` is always an engine-reported count over the very slice built from
    // `avail_in`, so it cannot exceed it and the subtraction cannot underflow. That
    // makes this categorically different from `rewind_input`, whose addend is
    // unbounded by the field it is added to. The assertion pins the invariant in
    // debug and test builds without costing anything in release.
    debug_assert!(
        consumed <= strm.avail_in as usize,
        "advance_input must never consume more than avail_in"
    );
    // SAFETY: `consumed <= avail_in`, so the offset stays within the input
    // buffer the caller guaranteed for `next_in`.
    strm.next_in = unsafe { strm.next_in.add(consumed) };
    strm.avail_in -= consumed as c_uint;
    strm.total_in = strm.total_in.wrapping_add(consumed as c_ulong);
}

/// Un-consumes `rewound` input bytes: moves [`z_stream::next_in`] *backwards* and
/// grows [`z_stream::avail_in`].
///
/// This is the exact inverse of [`advance_input`] and exists for one reason: C's
/// `inflate_fast` epilogue does `in -= bits >> 3` (`inffast.c` L291) without any
/// lower bound, so a call that entered with whole bytes already buffered hands
/// bytes back that an *earlier* call consumed. `strm->next_in` then points before
/// where this call started, `avail_in` exceeds the value the caller passed in, and
/// `total_in` decreases. Reference zlib relies on the caller presenting one
/// contiguous buffer and never dereferences the rewound bytes.
///
/// This deliberately leaves [`z_stream::total_in`] alone. C does not adjust the
/// total per operation: it derives one per-call delta as `in -= strm->avail_in`
/// in **`uInt`** arithmetic and then adds that to the `uLong` total
/// (`inflate.c` L1139-L1141). When a rewind exceeds the bytes the call consumed
/// the delta wraps at 2³² *before* the widening, so reference C reports
/// `total_in == 4294967295` — not `-1` widened to 64 bits — for a one-byte rewind
/// at `total_in == 0`. That value is part of the observable contract
/// (AAP §0.8.1 D-4), so the caller reproduces the 32-bit delta itself rather than
/// composing two independent 64-bit adjustments.
///
/// # Safety
///
/// `rewound` must not exceed the number of bytes preceding `next_in` *within the
/// same allocation*, and those bytes must still be valid for reads — that is, the
/// caller must not have moved or freed the buffer region it already fed. This is
/// the same obligation reference zlib imposes. The resulting pointer is formed
/// with [`pointer::wrapping_sub`](primitive@pointer) and is never dereferenced
/// here.
#[inline]
pub(crate) unsafe fn rewind_input(strm: &mut z_stream, rewound: usize) {
    if rewound == 0 {
        return;
    }
    // `wrapping_sub` rather than `sub`: the offset is only guaranteed in-bounds by
    // the caller's contiguous-buffer contract, and this crate never dereferences
    // the result — it is published to the caller, which owns the memory.
    //
    // `wrapping_add` rather than `+`: C recomputes the count in `unsigned`
    // arithmetic, which wraps. `inffast.c` L295 is
    // `strm->avail_in = (unsigned)(in < last ? 5 + (last - in) : 5 - (in - last));`
    // — a `uInt` expression with no overflow check — so a caller who presents
    // `avail_in` near `u32::MAX` and triggers a buffered-byte rewind observes the
    // wrapped value in reference zlib. A checked `+` would instead panic, which
    // under this crate's `panic = "abort"` profiles (`Cargo.toml`) terminates the
    // host process: a denial of service where C returns a defined, if surprising,
    // count. Both fields are computed and then committed together, so no
    // intermediate state where the cursor moved but the count did not is ever
    // observable.
    let next_in = strm.next_in.wrapping_sub(rewound);
    let avail_in = strm.avail_in.wrapping_add(rewound as c_uint);
    strm.next_in = next_in;
    strm.avail_in = avail_in;
}

/// Republishes [`z_stream::total_in`] using C's per-call input delta.
///
/// Reference zlib never adjusts the running total incrementally. It snapshots
/// `in = strm->avail_in` on entry and, on the way out, collapses the whole call
/// into one delta before widening it (`inflate.c` L1139-L1141):
///
/// ```text
/// in -= strm->avail_in;      /* uInt  — wraps at 2^32 */
/// strm->total_in += in;      /* uLong — the 32-bit delta is zero-extended */
/// ```
///
/// The wrap matters. When `inflate_fast`'s unbounded give-back (`inffast.c` L291)
/// hands back more bytes than the call pulled, `avail_in` ends up *larger* than it
/// started, the `uInt` delta wraps to just under 2³², and that value — not a
/// 64-bit `-1` — is what a C caller observes. Composing an `advance_input` add with
/// a `rewind_input` subtract in 64-bit arithmetic would instead publish
/// `u64::MAX`, so the shim reconstructs C's delta explicitly whenever a rewind is
/// in play.
///
/// `base` is `total_in` as it stood before the call consumed anything.
#[inline]
pub(crate) fn republish_total_in(
    strm: &mut z_stream,
    base: c_ulong,
    consumed: usize,
    rewound: usize,
) {
    let delta = (consumed as u32).wrapping_sub(rewound as u32);
    strm.total_in = base.wrapping_add(c_ulong::from(delta));
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
    // As in `advance_input`: `produced` is an engine-reported count over the slice
    // built from `avail_out`, so it is bounded by the field it is subtracted from.
    debug_assert!(
        produced <= strm.avail_out as usize,
        "advance_output must never produce more than avail_out"
    );
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

/// Copies a slice into a freshly allocated `Vec<u8>`, reporting an exhausted
/// allocator instead of aborting.
///
/// `slice::to_vec` allocates infallibly: on failure it calls
/// `handle_alloc_error`, which aborts the process. Every use below copies a
/// **caller-sized** buffer — a `gz_header`'s `extra` field is sized by the
/// caller's `extra_len`, and `name`/`comment` by wherever the caller's NUL
/// happens to be — so the size is not bounded by anything this crate controls.
/// Reference zlib never aborts for such an input, and neither may this shim
/// (AAP §0.6.5): the failure has to travel back as a return code.
///
/// **Which** return code is the caller's decision, and it is decided by the C
/// contract of the entry point the caller implements — never by this helper.
/// Reference zlib performs no allocation at `deflateSetHeader`, so it has no
/// out-of-memory answer to copy there: `zlib.h` L854-L855 gives that function
/// exactly `Z_OK` and `Z_STREAM_ERROR`, and its shim folds an exhausted
/// allocator into the latter rather than inventing a third code.
///
/// [`Vec::try_reserve_exact`] provides the fallible allocation; the subsequent
/// `extend_from_slice` cannot fail because the exact capacity is already present.
fn try_copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, TryReserveError> {
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(out)
}

/// Reads a NUL-terminated C string starting at `ptr` into an owned byte vector,
/// **excluding** the terminating NUL, or reports an exhausted allocator.
///
/// # Errors
///
/// Returns [`TryReserveError`] when the copy cannot be allocated. The length is
/// determined by the caller's data — the scan runs to the caller's NUL — so this
/// is a genuine, reachable condition and not a theoretical one.
///
/// # Safety
///
/// `ptr` must be non-null and point at a NUL-terminated sequence of bytes that
/// stays valid for the duration of the read.
unsafe fn cstr_bytes(ptr: *const c_uchar) -> Result<Vec<u8>, TryReserveError> {
    let mut len = 0usize;
    // SAFETY: per the contract, `ptr` points at a NUL-terminated string, so
    // every `ptr.add(len)` up to and including the terminator is readable.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: bytes `ptr[0..len]` precede the NUL and are therefore readable.
    try_copy_bytes(unsafe { slice::from_raw_parts(ptr, len) })
}

/// Returns the length of a NUL-terminated C string, excluding the terminator,
/// **without forming a slice over it**.
///
/// The allocation-free counterpart of [`cstr_bytes`], and the read-side mirror of
/// [`CRawHeaderSink`]. It exists because C's `deflateSetHeader` copies nothing:
/// `deflate` re-reads `s->gzhead->name[...]` straight out of caller memory at
/// emission time (`deflate.c` L1158), so the shim must be able to *describe* the
/// caller's buffer without committing to a reference over it. Being infallible is
/// the point — C's registration cannot fail for want of memory, so neither may
/// this path (AAP §0.6.5).
///
/// Returning a length rather than a `&[u8]` is what lets the caller decide
/// *whether* a reference may be formed at all: a field that overlaps the output
/// window must be staged instead (see [`CGzHeaderSource`]).
///
/// # Safety
///
/// `ptr` must be non-null and point at a NUL-terminated sequence of bytes that
/// stays valid for the duration of the scan, and **no mutable reference may be
/// live over the scanned bytes**. The scan reads through the raw pointer, so it
/// creates no reference of its own, but a read through an unrelated pointer is
/// still a foreign access that would invalidate a `&mut` covering the same
/// region — which is why the boundary performs every scan before it bridges the
/// caller's output window.
#[cfg(feature = "gzip")]
unsafe fn cstr_len(ptr: *const c_uchar) -> usize {
    let mut len = 0usize;
    // SAFETY: per the contract, `ptr` points at a NUL-terminated string, so
    // every `ptr.add(len)` up to and including the terminator is readable, and no
    // conflicting reference is live over them.
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    len
}

/// Whether the half-open address ranges `[a_start, a_end)` and `[b_start, b_end)`
/// share at least one byte.
///
/// Plain integer comparison is deliberate. Addresses that may belong to different
/// allocations cannot be compared as *pointers* in any defined way, but comparing
/// them as `usize` is total and exact — and "do these two byte ranges touch" is
/// precisely the question the boundary must answer before deciding whether a
/// reference may be formed over a caller-supplied buffer.
///
/// Both empty-range cases answer `false`, because a range spanning no bytes cannot
/// share a byte with anything: an empty *field* (a non-null `head->extra` with
/// `extra_len == 0`, or a `name` that is just its NUL) occupies nothing, and an
/// empty *window* (`avail_out == 0`, which the engine sees as `&mut []`) covers
/// nothing. Adjacency — one range ending exactly where the other begins — is
/// likewise not an overlap.
#[cfg(feature = "gzip")]
#[inline]
fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < a_end && b_start < b_end && a_start < b_end && b_start < a_end
}

/// Reads a live caller-owned [`gz_header`] into a [`CGzHeaderSource`], or
/// [`None`] when `head` is null.
///
/// This is the read side of the C ABI's gzip-header contract and the reason
/// `deflateSetHeader` needs no allocation at all. C stores the caller's pointer
/// and re-reads every field lazily when the header is emitted (`deflate.c` L717,
/// L893-L907, L1092-L1188); this function reproduces that by re-describing the
/// caller's own buffers once per engine call.
///
/// Nothing is copied and nothing is allocated, so unlike
/// [`gz_header_to_idiomatic`] this cannot fail — which is required, because
/// reference zlib's `deflateSetHeader` returns only `Z_OK` or `Z_STREAM_ERROR`
/// and can never report `Z_MEM_ERROR`.
///
/// # Why a descriptor and not a [`ForeignGzHeader`]
///
/// The engine consumes borrowed slices, but the boundary must not *decide* to form
/// them until it knows whether they would overlap a window the engine holds
/// mutably: `zlib.h` L843-L847 lets a caller point `head->name` straight at
/// `strm->next_out`, and a shared borrow over bytes covered by a live `&mut` is
/// undefined behaviour regardless of how carefully it is bounded. This function
/// therefore stops one step short of the borrow, and
/// [`CGzHeaderSource::borrow`]/[`CGzHeaderSource::stage_into`] complete it.
///
/// # Safety
///
/// `head` must be null, or point at a valid `gz_header` that stays valid for the
/// duration of the call, whose `extra` (when non-null) is readable for `extra_len`
/// bytes and whose `name`/`comment` (when non-null) are NUL-terminated — exactly
/// the contract `zlib.h` L843-L847 places on the caller. No mutable reference may
/// be live over the `name`/`comment` bytes, which are scanned for their terminator
/// here; the boundary satisfies this by calling before it bridges the caller's
/// output window.
#[cfg(feature = "gzip")]
pub(crate) unsafe fn read_gz_header_source(head: *const gz_header) -> Option<CGzHeaderSource> {
    if head.is_null() {
        return None;
    }
    // Every field is projected with `addr_of!` and read through the resulting raw
    // pointer. Forming `&*head` instead would create a shared reference over the
    // caller's whole `gz_header`, which is both unnecessary and — if the caller
    // placed that struct inside a region the boundary also references — a foreign
    // access. `addr_of!` never materializes a reference, so the question cannot
    // arise.
    //
    // SAFETY: `head` is non-null and, per the contract, points at a valid
    // `gz_header` for the duration of this call, so every field projection is
    // in-bounds, aligned and initialized.
    let (text, time, os, hcrc, extra_ptr, extra_len, name_ptr, comment_ptr) = unsafe {
        (
            ptr::addr_of!((*head).text).read(),
            ptr::addr_of!((*head).time).read(),
            ptr::addr_of!((*head).os).read(),
            ptr::addr_of!((*head).hcrc).read(),
            ptr::addr_of!((*head).extra).read(),
            ptr::addr_of!((*head).extra_len).read(),
            ptr::addr_of!((*head).name).read(),
            ptr::addr_of!((*head).comment).read(),
        )
    };

    // The two NUL scans are hoisted out of the struct literal so each carries its
    // own safety justification: `clippy::undocumented_unsafe_blocks` requires the
    // comment on the line immediately preceding the block, and a single comment
    // above a struct literal does not cover a second block further down it.
    //
    // SAFETY: a non-null `name` is a NUL-terminated C string that stays valid for
    // the duration of this call (`zlib.h` L843-L847), and this function runs before
    // the boundary bridges the caller's output window, so no mutable reference is
    // live over the scanned bytes.
    let name_len = unsafe { cstr_len_of(name_ptr) };
    // SAFETY: identical to `name` above — a non-null `comment` is a NUL-terminated
    // C string valid for this call, scanned before any output window is bridged.
    let comment_len = unsafe { cstr_len_of(comment_ptr) };

    Some(CGzHeaderSource {
        text: text != 0,
        // C writes the low four bytes of `head->time` (`deflate.c` L1098-L1101),
        // so a 64-bit `uLong` is truncated exactly as C truncates it. The
        // narrowing goes through `ulong_to_u32` because `uLong` is `c_ulong`,
        // whose width is target-dependent: the cast genuinely truncates on LP64
        // (x86_64/aarch64/s390x Linux) and is the identity on ILP32 (`i686`) and
        // LLP64 (Windows), where a bare `as u32` trips
        // `clippy::unnecessary_cast` — a diagnostic CI's cross-lint rows raise
        // and a Linux-x86_64-only lane never would.
        time: ulong_to_u32(time),
        os,
        hcrc: hcrc != 0,
        // C bounds the field by `head->extra_len & 0xffff` when it emits XLEN
        // (`deflate.c` L1120), but the *descriptor* records `extra_len` verbatim:
        // the engine applies the mask itself, and `extra_len & 0xffff` is not
        // recoverable from a pre-masked length (`0x1_0005 & 0xffff == 5`, whereas
        // masking `0x1_0000` first yields `0`). `zlib.h` L845-L846 makes those
        // bytes readable the caller's responsibility.
        //
        // A non-null pointer with a zero length stays `Some`, because presence —
        // not length — is what sets FLG bit 4 (`s->gzhead->extra != Z_NULL`), so a
        // caller registering an empty `extra` gets an empty extra field emitted.
        extra: CRawHeaderSource::describe(extra_ptr, extra_len as usize),
        // `name` and `comment` are NUL-terminated rather than length-prefixed, so
        // their extents come from the scans performed above.
        name: CRawHeaderSource::describe(name_ptr, name_len),
        comment: CRawHeaderSource::describe(comment_ptr, comment_len),
    })
}

/// [`cstr_len`] lifted over a possibly-null pointer, yielding `0` for null.
///
/// Keeps [`read_gz_header_source`] free of a nested `if` per field: the length is
/// irrelevant when the pointer is null, because [`CRawHeaderSource::describe`]
/// discards it.
///
/// # Safety
///
/// Same contract as [`cstr_len`] when `ptr` is non-null.
#[cfg(feature = "gzip")]
unsafe fn cstr_len_of(ptr: *const c_uchar) -> usize {
    if ptr.is_null() {
        return 0;
    }
    // SAFETY: `ptr` is non-null and the caller upholds `cstr_len`'s contract.
    unsafe { cstr_len(ptr) }
}

/// One gzip-header payload field belonging to a C caller, addressed as a bare
/// pointer plus a length — the **read**-side counterpart of [`CRawHeaderSink`].
///
/// # Why not a slice
///
/// C's `deflate` reads the header's three payloads through three independent
/// caller pointers — `head->extra`, `head->name`, `head->comment` — and `zlib.h`
/// L843-L847 imposes **no disjointness** between them or with `strm->next_out`. A
/// caller may legally set `head->name = strm->next_out`. Materializing such a
/// field as `&[u8]` while the engine holds the output window as `&mut [u8]` is
/// undefined behaviour: both references cover the same bytes, the shared read
/// invalidates the unique tag, and the engine's next write through it is invalid.
/// Bounds checking cannot help — the violation is committed when the second
/// reference is *created*.
///
/// Splitting *describing* a field from *reading* it is what makes the legal
/// overlap expressible. The boundary measures the overlap first, then chooses
/// between borrowing in place (disjoint — the allocation-free zero-copy path,
/// which is what C does) and staging a copy before the output window is bridged
/// (overlapping).
///
/// # Invariants
///
/// * `ptr` is non-null and addresses at least `len` readable bytes for as long as
///   this value lives.
/// * `len` is the caller's *live* field length: `head->extra_len` verbatim for
///   `extra`, and the distance to the terminating NUL for `name`/`comment`.
#[cfg(feature = "gzip")]
#[derive(Clone, Copy)]
pub(crate) struct CRawHeaderSource {
    /// The caller's buffer pointer, non-null and valid for `len` readable bytes.
    ptr: *const c_uchar,
    /// The caller's live length for this field, in bytes.
    len: usize,
}

#[cfg(feature = "gzip")]
impl CRawHeaderSource {
    /// Describes `ptr`/`len` as a field, or [`None`] when `ptr` is null.
    ///
    /// Null is the only rejection: unlike the sink side, a zero *length* is a
    /// meaningful, emittable field (see [`read_gz_header_source`]).
    #[inline]
    fn describe(ptr: *const c_uchar, len: usize) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, len })
        }
    }

    /// The half-open address range `[start, end)` this field occupies, as plain
    /// integers, with a saturating end so a caller-declared length that would wrap
    /// the address space cannot panic.
    #[inline]
    fn range(self) -> (usize, usize) {
        let start = self.ptr as usize;
        (start, start.saturating_add(self.len))
    }

    /// Whether this field shares a byte with the half-open range `[start, end)`.
    #[inline]
    fn intersects(self, start: usize, end: usize) -> bool {
        let (f_start, f_end) = self.range();
        ranges_overlap(f_start, f_end, start, end)
    }

    /// Materializes the borrow the engine consumes.
    ///
    /// # Safety
    ///
    /// The field's bytes must be readable for `len` and remain valid and unmutated
    /// for `'a`, and **no mutable reference may be live over them** for `'a`. The
    /// second clause is the one that matters here and the reason this is not simply
    /// done at construction: the caller must have established disjointness from
    /// every window the engine holds (see [`CGzHeaderSource::intersects`]).
    #[inline]
    unsafe fn borrow<'a>(self) -> &'a [u8] {
        // SAFETY: per the contract the region is readable for `len` and no
        // conflicting reference exists, which is exactly `from_raw_parts`'
        // precondition. `c_uchar` needs no alignment.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Copies the field into `dst`, which is cleared first.
    ///
    /// Used only when the field overlaps a window the engine will hold, so that the
    /// engine reads the staged copy instead of the caller's memory.
    ///
    /// # Safety
    ///
    /// As [`borrow`](Self::borrow): the copy reads the caller's bytes, so it must
    /// run while no mutable reference is live over them — that is, *before* the
    /// boundary bridges the output window.
    unsafe fn stage(self, dst: &mut Vec<u8>) -> Result<(), TryReserveError> {
        dst.clear();
        dst.try_reserve_exact(self.len)?;
        // SAFETY: the caller upholds `borrow`'s contract, and the borrow ends
        // inside this statement — it never coexists with the engine's windows.
        dst.extend_from_slice(unsafe { self.borrow() });
        Ok(())
    }
}

/// Owned storage for a staged copy of the three header payloads.
///
/// Lives in the FFI shim's own frame for the duration of one call. Only the
/// overlapping path fills it; the universal disjoint path leaves all three vectors
/// empty and never allocates, which is what keeps `deflate`'s header handling as
/// allocation-free as C's (AAP §0.6.5).
#[cfg(feature = "gzip")]
#[derive(Debug, Default)]
pub(crate) struct CGzHeaderStage {
    /// Staged `head->extra`.
    extra: Vec<u8>,
    /// Staged `head->name`, excluding its NUL.
    name: Vec<u8>,
    /// Staged `head->comment`, excluding its NUL.
    comment: Vec<u8>,
}

/// A live caller-owned [`gz_header`] read as scalars plus three raw field
/// descriptors — the read-side counterpart of [`CGzHeaderSinks`].
///
/// This is what `deflateSetHeader`'s stored pointer becomes at every point C would
/// re-read it: inside `deflate` at emission time (`deflate.c` L1092-L1188) and
/// inside `deflateBound` (`deflate.c` L893-L907). Re-reading per call is what makes
/// a mutation performed after registration but before emission observable in the
/// output bytes, exactly as in C, and is required for byte identity
/// (AAP §0.8.1 D-1).
#[cfg(feature = "gzip")]
pub(crate) struct CGzHeaderSource {
    /// C `head->text`.
    text: bool,
    /// C `head->time`, truncated to the four bytes C writes.
    time: u32,
    /// C `head->os`.
    os: c_int,
    /// C `head->hcrc`.
    hcrc: bool,
    /// C `head->extra`, length `head->extra_len` verbatim.
    extra: Option<CRawHeaderSource>,
    /// C `head->name`, length up to but excluding its NUL.
    name: Option<CRawHeaderSource>,
    /// C `head->comment`, length up to but excluding its NUL.
    comment: Option<CRawHeaderSource>,
}

#[cfg(feature = "gzip")]
impl CGzHeaderSource {
    /// Whether any present payload field shares a byte with the half-open address
    /// range `[start, end)`.
    ///
    /// The boundary asks this about the caller's output window. An intersection
    /// means the engine would hold a `&mut [u8]` over bytes a header borrow also
    /// covers, so the fields must be staged; a disjoint answer — the universal
    /// case — keeps the zero-copy path.
    pub(crate) fn intersects(&self, start: usize, end: usize) -> bool {
        [self.extra, self.name, self.comment]
            .into_iter()
            .flatten()
            .any(|field| field.intersects(start, end))
    }

    /// The zero-copy view: the engine reads the caller's own memory, as C does.
    ///
    /// # Safety
    ///
    /// Every present field must satisfy [`CRawHeaderSource::borrow`]'s contract for
    /// `'a`. In particular the caller must have established, via
    /// [`intersects`](Self::intersects), that no field overlaps a window held as
    /// `&mut` for `'a`.
    pub(crate) unsafe fn borrow<'a>(&self) -> ForeignGzHeader<'a> {
        ForeignGzHeader {
            text: self.text,
            time: self.time,
            os: self.os,
            hcrc: self.hcrc,
            // SAFETY: delegated to this function's contract, field by field.
            extra: self.extra.map(|f| unsafe { f.borrow() }),
            // SAFETY: as above.
            name: self.name.map(|f| unsafe { f.borrow() }),
            // SAFETY: as above.
            comment: self.comment.map(|f| unsafe { f.borrow() }),
        }
    }

    /// The staged view: the payloads are copied into `stage` and the engine reads
    /// the copies.
    ///
    /// Field *presence* is preserved exactly — a present-but-empty field stays
    /// present — because presence is what drives the FLG bits, and a staged length
    /// equals the caller's length, so `extra_len & 0xffff`, the emitted XLEN and
    /// `deflateBound`'s wrapper term are all unchanged.
    ///
    /// # Safety
    ///
    /// Must be called while no mutable reference is live over any field's bytes —
    /// that is, before the boundary bridges the caller's output window. The
    /// resulting view borrows `stage`, not the caller's memory, so it may then be
    /// held across the engine call.
    pub(crate) unsafe fn stage_into<'a>(
        &self,
        stage: &'a mut CGzHeaderStage,
    ) -> Result<ForeignGzHeader<'a>, TryReserveError> {
        let CGzHeaderStage {
            extra,
            name,
            comment,
        } = stage;
        if let Some(field) = self.extra {
            // SAFETY: delegated to this function's contract.
            unsafe { field.stage(extra) }?;
        }
        if let Some(field) = self.name {
            // SAFETY: delegated to this function's contract.
            unsafe { field.stage(name) }?;
        }
        if let Some(field) = self.comment {
            // SAFETY: delegated to this function's contract.
            unsafe { field.stage(comment) }?;
        }
        Ok(ForeignGzHeader {
            text: self.text,
            time: self.time,
            os: self.os,
            hcrc: self.hcrc,
            extra: self.extra.map(|_| &extra[..]),
            name: self.name.map(|_| &name[..]),
            comment: self.comment.map(|_| &comment[..]),
        })
    }
}

/// One gzip-header payload buffer belonging to a C caller, addressed as a bare
/// pointer plus a capacity.
///
/// # Why not a slice
///
/// C's `inflate` writes the header's three payloads through three independent
/// caller pointers — `head->extra`, `head->name`, `head->comment` — and the API
/// requires **no disjointness** between them or with `strm->next_out`
/// (`inflate.c` L614-L621, L639-L642, L661-L664). Materializing them as three
/// `&mut [u8]` is undefined behaviour the instant any two overlap, and the
/// violation is committed when the references are *created*: a bounds-checked
/// write is already too late. Keeping the raw pointer and performing one isolated
/// access per store reproduces C exactly, including its overlap behaviour, and
/// removes the hazard entirely.
///
/// # Invariants
///
/// * `ptr` is non-null and addresses at least `cap` bytes writable for as long as
///   this value lives.
/// * `cap` is the caller's *live* `extra_max`/`name_max`/`comm_max`, read when
///   this descriptor was built.
/// * The bytes need not be initialized: nothing here ever reads them, and no
///   reference is ever formed over the region.
#[cfg(feature = "gzip")]
pub(crate) struct CRawHeaderSink {
    /// The caller's buffer pointer, non-null and valid for `cap` writable bytes.
    ptr: *mut c_uchar,
    /// The caller's live capacity for this field, in bytes.
    cap: usize,
}

#[cfg(feature = "gzip")]
impl CRawHeaderSink {
    /// The half-open address range `[start, end)` this buffer occupies, as plain
    /// integers.
    ///
    /// Used by the boundary to decide whether the buffer overlaps the caller's
    /// input or output window. Integer arithmetic is deliberate: comparing
    /// addresses that may belong to different allocations is meaningless as
    /// pointer arithmetic but perfectly well-defined on `usize`, and the
    /// saturating end keeps a caller-declared capacity that would wrap the address
    /// space from panicking.
    fn range(&self) -> (usize, usize) {
        let start = self.ptr as usize;
        (start, start.saturating_add(self.cap))
    }
}

#[cfg(feature = "gzip")]
impl ForeignByteSink for CRawHeaderSink {
    #[inline]
    fn capacity(&self) -> usize {
        self.cap
    }

    #[inline]
    fn store_byte(&mut self, index: usize, byte: u8) -> bool {
        if index >= self.cap {
            return false;
        }
        // SAFETY: `index < cap` and, per this type's invariants, `ptr` addresses
        // `cap` writable bytes, so `ptr.add(index)` is inside the caller's buffer
        // and aligned (`c_uchar` has alignment 1). No reference over the region
        // exists — that is the whole point of the descriptor — so this write
        // cannot invalidate one, and it is exactly C's
        // `head->name[state->length++] = byte`.
        unsafe { self.ptr.add(index).write(byte) };
        true
    }

    #[inline]
    fn store_bytes(&mut self, offset: usize, src: &[u8]) -> usize {
        if offset >= self.cap {
            return 0;
        }
        let room = self.cap - offset;
        let n = if src.len() > room { room } else { src.len() };
        // SAFETY: `offset + n <= cap`, so the whole destination lies inside the
        // caller's buffer, which the invariants make writable for `cap` bytes;
        // `src` is a live slice of at least `n` readable bytes. The two ranges are
        // disjoint: `src` is the decoder's input window, and the boundary routes
        // any call whose header buffers overlap that window through a staged copy
        // (see `inflate`), so `src` never aliases the destination here. `u8` needs
        // no alignment.
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(offset), n) };
        n
    }
}

/// The three caller-owned gzip-header payload buffers, owned as raw descriptors
/// for the duration of one FFI call.
///
/// This is the *owner*; [`view`](Self::view) hands the decoder the borrowed
/// [`ForeignGzHeaderSink`] it programs against. Splitting owner from view lets a
/// single set of descriptors serve several consecutive engine calls within one
/// `inflate` invocation — which the header/output overlap path needs — while the
/// declared `XLEN` the decoder maintains persists across them in the view.
#[cfg(feature = "gzip")]
pub(crate) struct CGzHeaderSinks {
    /// `head->extra`, bounded by the live `extra_max`; `None` when absent.
    extra: Option<CRawHeaderSink>,
    /// `head->name`, bounded by the live `name_max`; `None` when absent.
    name: Option<CRawHeaderSink>,
    /// `head->comment`, bounded by the live `comm_max`; `None` when absent.
    comment: Option<CRawHeaderSink>,
    /// The caller's `head->extra_len` as read when the descriptors were built.
    extra_len: u32,
}

#[cfg(feature = "gzip")]
impl CGzHeaderSinks {
    /// Lends the decoder a borrowed view over these descriptors.
    ///
    /// The view is what `inflate` passes to the engine; it carries the declared
    /// `XLEN` by value because the engine assigns it (`inflate.c` L599-L600) and
    /// then derives the extra field's write offset from it (L616-L617).
    pub(crate) fn view(&mut self) -> ForeignGzHeaderSink<'_> {
        ForeignGzHeaderSink {
            extra_len: self.extra_len,
            extra: self
                .extra
                .as_mut()
                .map(|sink| sink as &mut dyn ForeignByteSink),
            name: self
                .name
                .as_mut()
                .map(|sink| sink as &mut dyn ForeignByteSink),
            comment: self
                .comment
                .as_mut()
                .map(|sink| sink as &mut dyn ForeignByteSink),
        }
    }

    /// Whether any present payload buffer intersects the half-open address range
    /// `[start, end)`.
    ///
    /// The boundary asks this about the caller's input and output windows. An
    /// intersection means a header store and a decoder access would touch the same
    /// bytes while a Rust reference exists over one of them, so the call must be
    /// split into header and data phases; a disjoint answer — the universal case —
    /// keeps the single-pass path.
    pub(crate) fn intersects(&self, start: usize, end: usize) -> bool {
        [
            self.extra.as_ref(),
            self.name.as_ref(),
            self.comment.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|sink| {
            let (f_start, f_end) = sink.range();
            ranges_overlap(f_start, f_end, start, end)
        })
    }
}

/// Borrows a live caller-owned [`gz_header`]'s **output buffers** as a
/// [`CGzHeaderSinks`], or [`None`] when `head` is null.
///
/// This is the write direction's counterpart of [`read_gz_header_source`], and it
/// must be called immediately before each engine call rather than once at
/// registration: C re-reads `head->extra`/`name`/`comment` and their
/// `extra_max`/`name_max`/`comm_max` capacities on **every stored byte**
/// (`inflate.c` L614-L621, L639-L642, L661-L664), so a caller may install,
/// replace, resize or withdraw a sink at any point before the bytes arrive and
/// see it honored. Re-materializing per call reproduces that: a caller cannot
/// mutate its header *during* a synchronous call, so per-call and per-byte
/// freshness observe the same bytes.
///
/// Nothing is copied and nothing is allocated — the descriptors address the
/// caller's own storage, which is exactly why the header path can no longer report
/// a `Z_MEM_ERROR` C never reports.
///
/// Every field of `*head` is read through a raw projection rather than a
/// `&mut gz_header`: a caller is free to place a payload buffer *inside* the
/// header struct itself, and a reference covering the struct would then alias the
/// descriptor built from it.
///
/// # Safety
///
/// `head` must be null, or point at a valid `gz_header` that stays valid and
/// unaliased for as long as the returned value lives. When `extra`/`name`/
/// `comment` are non-null, each must address at least `extra_max`/`name_max`/
/// `comm_max` **writable** bytes — the contract `zlib.h` L1076-L1085 places on an
/// `inflateGetHeader` caller.
#[cfg(feature = "gzip")]
pub(crate) unsafe fn borrow_gz_header_sink(head: *mut gz_header) -> Option<CGzHeaderSinks> {
    if head.is_null() {
        return None;
    }
    // Each pointer and capacity is read *now* through a raw field projection, so a
    // mid-decode change takes effect on the next call — C's per-byte re-read, at
    // call granularity — and no reference spanning the struct is ever created.
    // SAFETY: `head` is non-null and, per the contract, points at a valid
    // `gz_header`; `addr_of!` forms field pointers without materializing a
    // reference to the whole struct, and each field is a plain `Copy` scalar or
    // pointer that is initialized in any valid `gz_header`.
    let (extra_ptr, name_ptr, comment_ptr, extra_len, extra_max, name_max, comm_max) = unsafe {
        (
            ptr::addr_of!((*head).extra).read(),
            ptr::addr_of!((*head).name).read(),
            ptr::addr_of!((*head).comment).read(),
            ptr::addr_of!((*head).extra_len).read(),
            ptr::addr_of!((*head).extra_max).read() as usize,
            ptr::addr_of!((*head).name_max).read() as usize,
            ptr::addr_of!((*head).comm_max).read() as usize,
        )
    };
    /// Builds a descriptor for one payload, or `None` for C's "field absent"
    /// encodings: a null pointer, or a capacity of zero that admits no byte.
    fn describe(ptr: *mut c_uchar, cap: usize) -> Option<CRawHeaderSink> {
        if ptr.is_null() || cap == 0 {
            None
        } else {
            Some(CRawHeaderSink { ptr, cap })
        }
    }
    Some(CGzHeaderSinks {
        extra: describe(extra_ptr, extra_max),
        name: describe(name_ptr, name_max),
        comment: describe(comment_ptr, comm_max),
        // C derives the extra field's write offset from the caller's own
        // `extra_len` (`inflate.c` L616-L617), so it belongs to the live view.
        extra_len,
    })
}

/// Converts a raw [`gz_header`] (as passed to `deflateSetHeader`) into the
/// idiomatic [`GzHeader`], or `Ok(None)` when `head` is null.
///
/// The [`extra`](gz_header::extra) field is copied using its
/// [`extra_len`](gz_header::extra_len); [`name`](gz_header::name) and
/// [`comment`](gz_header::comment) are read as NUL-terminated C strings (the
/// terminator is dropped). C `int` booleans map to Rust [`bool`].
///
/// # Errors
///
/// Returns [`TryReserveError`] if any of the three copies cannot be allocated.
/// All three are sized entirely by the caller — `extra` by its `extra_len`, the
/// two strings by their NUL positions — so an exhausted allocator here is a
/// reachable outcome of a well-formed call, and one that must travel back as a
/// return code rather than abort the process.
///
/// Callers must map it to whichever failure code the C contract of *their* entry
/// point permits — **not** unconditionally to `Z_MEM_ERROR`. The sole production
/// caller is `deflateSetHeader`, where reference zlib allocates nothing and
/// `zlib.h` L854-L855 documents exactly two outcomes, so that shim reports the
/// exhaustion as `Z_STREAM_ERROR`; returning `Z_MEM_ERROR` there would be a code
/// C cannot produce. See `deflateSetHeader`.
///
/// Nothing is retained on the error path: the partially built copies are dropped
/// as this function returns, and the stream is left exactly as it was — matching
/// C's `deflateSetHeader`, which validates and only then takes ownership of the
/// caller's pointers.
///
/// # Safety
///
/// `head` must be null or point at a valid [`gz_header`]. When its `extra`
/// pointer is non-null it must be readable for `extra_len` bytes; when `name`/
/// `comment` are non-null they must be NUL-terminated.
pub unsafe fn gz_header_to_idiomatic(
    head: *const gz_header,
) -> Result<Option<GzHeader>, TryReserveError> {
    if head.is_null() {
        return Ok(None);
    }
    // SAFETY: `head` is non-null and, per the contract, points at a valid
    // `gz_header`.
    let h = unsafe { &*head };

    let extra = if h.extra.is_null() {
        None
    } else {
        // SAFETY: a non-null `extra` is readable for `extra_len` bytes
        // (deflateSetHeader contract).
        let raw = unsafe { slice::from_raw_parts(h.extra, h.extra_len as usize) };
        Some(try_copy_bytes(raw)?)
    };
    let name = if h.name.is_null() {
        None
    } else {
        // SAFETY: a non-null `name` is a NUL-terminated C string.
        Some(unsafe { cstr_bytes(h.name) }?)
    };
    let comment = if h.comment.is_null() {
        None
    } else {
        // SAFETY: a non-null `comment` is a NUL-terminated C string.
        Some(unsafe { cstr_bytes(h.comment) }?)
    };

    Ok(Some(GzHeader {
        text: h.text != 0,
        time: ulong_to_u32(h.time),
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
    }))
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

    // Every access below is a *raw field projection*: a `&mut gz_header` spanning
    // the struct is deliberately never formed. A caller may legally point
    // `extra`/`name`/`comment` at storage that overlaps a sibling payload buffer,
    // the decoder's output window, or even the header struct itself, and a
    // reference covering the struct would then alias the very bytes this function
    // writes through those pointers. Reference zlib performs exactly these
    // independent accesses, so reproducing them literally is both faithful and
    // sound.
    //
    // SAFETY: this applies to every access in the block below. `head` is non-null
    // and, per this function's contract, points at a valid `gz_header` exclusively
    // available to this call, so `addr_of!`/`addr_of_mut!` yield field pointers
    // that are in bounds, aligned, and (for reads) initialized in any valid
    // `gz_header`. Each payload write is separately bounded by the capacity read
    // from that same struct: `copy_tail_bounded` clamps to it, and the two NUL
    // stores are guarded by a strict `len < cap` test, so no write leaves the
    // caller's declared region.
    unsafe {
        // --- scalars: each written only by the state that assigns it in C -----
        if published.text {
            ptr::addr_of_mut!((*head).text).write(c_int::from(src.text));
        }
        if published.time {
            ptr::addr_of_mut!((*head).time).write(src.time as c_ulong);
        }
        if published.os {
            // C assigns `xflags` and `os` together under one guard (`inflate.c`
            // L586-L589), so one flag covers both.
            ptr::addr_of_mut!((*head).xflags).write(src.xflags);
            ptr::addr_of_mut!((*head).os).write(src.os);
        }
        if published.hcrc {
            ptr::addr_of_mut!((*head).hcrc).write(c_int::from(src.hcrc));
        }
        if let Some(done) = published.done {
            // The tri-state reaches the caller verbatim: `-1` for "not a gzip
            // header", `1` for "header complete".
            ptr::addr_of_mut!((*head).done).write(done.as_c_int() as c_int);
        }

        // --- extra -----------------------------------------------------------
        if let Some(declared_xlen) = published.extra_len {
            // The stream's declared 16-bit `XLEN`, published unconditionally and
            // *unclamped* — gated on neither `extra`'s nullity nor `extra_max`'s
            // size (`inflate.c` L599-L600), while the copy below is separately
            // clamped (L614-L621). Reproducing both halves is what makes
            // `extra_len > extra_max` a usable truncation signal per `zlib.h`, and
            // what lets a caller pass a null `extra` purely to learn the length.
            ptr::addr_of_mut!((*head).extra_len).write(declared_xlen as c_uint);
        }
        if published.extra_null {
            // C's no-`FEXTRA` branch: `state->head->extra = Z_NULL` (`inflate.c`
            // L605-L606). That assignment is how a C caller distinguishes "the
            // header declared no extra field" from "it declared one"; leaving a
            // stale non-null pointer would misreport an absent field as present.
            ptr::addr_of_mut!((*head).extra).write(ptr::null_mut());
        }
        if published.extra_stored != 0 {
            if let Some(extra) = &src.extra {
                // Re-read after the possible nulling above, so C's ordering — the
                // `Z_NULL` assignment first, the clamped copy second — is preserved
                // and a nulled field copies nothing.
                let dst = ptr::addr_of!((*head).extra).read();
                let cap = ptr::addr_of!((*head).extra_max).read() as usize;
                copy_tail_bounded(dst, extra, published.extra_stored, cap);
            }
        }

        // --- name ------------------------------------------------------------
        if published.name_null {
            // C's no-`FNAME` branch: `state->head->name = Z_NULL` (`inflate.c`
            // L650-L651).
            ptr::addr_of_mut!((*head).name).write(ptr::null_mut());
        }
        if let Some(name) = &src.name {
            let dst = ptr::addr_of!((*head).name).read();
            let cap = ptr::addr_of!((*head).name_max).read() as usize;
            if published.name_stored != 0 {
                copy_tail_bounded(dst, name, published.name_stored, cap);
            }
            if published.name_terminated && !dst.is_null() && name.len() < cap {
                // C stores the NUL at `head->name[state->length]`, and
                // `state->length` is exactly the number of content bytes captured
                // so far; `name.len() < cap` puts it inside the buffer.
                dst.add(name.len()).write(0);
            }
        }

        // --- comment ---------------------------------------------------------
        if published.comment_null {
            // C's no-`FCOMMENT` branch: `state->head->comment = Z_NULL`
            // (`inflate.c` L672-L673).
            ptr::addr_of_mut!((*head).comment).write(ptr::null_mut());
        }
        if let Some(comment) = &src.comment {
            let dst = ptr::addr_of!((*head).comment).read();
            let cap = ptr::addr_of!((*head).comm_max).read() as usize;
            if published.comment_stored != 0 {
                copy_tail_bounded(dst, comment, published.comment_stored, cap);
            }
            if published.comment_terminated && !dst.is_null() && comment.len() < cap {
                dst.add(comment.len()).write(0);
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
//
// THIS IS THE SINGLE HOME FOR EVERY BOUNDARY GUARD. One guard exists per return
// width the exported C surface actually uses — `c_int`, `c_ulong`, `c_long`,
// `z_size_t`, `z_off64_t`, `*mut T`, `*const T`, and `void` — so the set of
// widths the boundary covers is auditable by reading this one section. A shim
// file must never define its own guard or open a bare `catch_unwind`: a
// duplicate drifts from the `#[cfg(feature = "std")]` pair below the moment one
// copy is edited, and a bare `catch_unwind` at a call site is invisible to that
// audit. If a new entry point returns a width not listed above, add it HERE.

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

/// Runs `f`, returning its `c_long` result, or `default` if it panics.
///
/// The `c_long` width is needed by `inflateMark`, whose C signature returns
/// `long` (`zlib.h`) and whose documented failure value is `-1 << 16`.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_long(
    default: c_long,
    f: impl FnOnce() -> c_long + core::panic::UnwindSafe,
) -> c_long {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_long(
    _default: c_long,
    f: impl FnOnce() -> c_long + core::panic::UnwindSafe,
) -> c_long {
    f()
}

/// Runs `f`, returning its [`z_size_t`] result, or `default` if it panics.
///
/// The `size_t` width is needed by `gzfread` / `gzfwrite`, whose C signatures
/// return `z_size_t` and whose documented failure value is `0`.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_size(
    default: z_size_t,
    f: impl FnOnce() -> z_size_t + core::panic::UnwindSafe,
) -> z_size_t {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_size(
    _default: z_size_t,
    f: impl FnOnce() -> z_size_t + core::panic::UnwindSafe,
) -> z_size_t {
    f()
}

/// Runs `f`, returning its `*const T` result, or `default` if it panics.
///
/// Distinct from [`guard_ptr`], which covers `*mut T`: `gzerror` returns
/// `const char *` and must not be forced through a mutable pointer type.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_const_ptr<T>(
    default: *const T,
    f: impl FnOnce() -> *const T + core::panic::UnwindSafe,
) -> *const T {
    std::panic::catch_unwind(f).unwrap_or(default)
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_const_ptr<T>(
    default: *const T,
    f: impl FnOnce() -> *const T + core::panic::UnwindSafe,
) -> *const T {
    let _ = default;
    f()
}

/// Runs `f` and swallows a panic — the guard for the `void`-returning shims.
///
/// `gzclearerr` is the only C entry point in this library that returns nothing,
/// so there is no value to substitute; the obligation is purely that the panic
/// must not cross the ABI. Having it here rather than as an inline
/// `catch_unwind` at the call site keeps every boundary guard in one place, so
/// the set of return widths the boundary covers is auditable by reading one
/// section of one file.
#[cfg(feature = "std")]
#[allow(dead_code)]
pub(crate) fn guard_void(f: impl FnOnce() + core::panic::UnwindSafe) {
    let _ = std::panic::catch_unwind(f);
}

/// `no_std` fallback: runs `f` directly (no unwinding to catch).
#[cfg(not(feature = "std"))]
#[allow(dead_code)]
pub(crate) fn guard_void(f: impl FnOnce() + core::panic::UnwindSafe) {
    f();
}

// --- Shared test utility for the panic guards ------------------------------
//
// Declared at module scope (not inside `mod tests`) and `pub(crate)` so the
// sibling shim files' test modules reach the same serialization primitive. The
// guards themselves all live in this module, but each shim file tests the ones IT
// uses from its own test module, and without a single shared lock two test modules
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
    /// the shared `(kind, owner)` prefix must sit at identical offsets in
    /// [`HandleHeader`] and in every tagged handle.
    ///
    /// [`peek_handle_prefix`] reads both fields through the C-layout
    /// [`HandleHeader`] *before* any handle is reinterpreted as its concrete
    /// type, so a magic collision — or a width/offset drift in either field —
    /// would silently defeat the cross-engine `End` guard and the owner check,
    /// re-opening both the layout-mismatched deallocation and the
    /// double-reclaim-through-a-copied-`z_stream` hazard that the prefix exists to
    /// prevent.
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

        // The prefix is two fields wide, not one: `kind` then `owner`. Asserting
        // the *offsets* rather than the total size keeps this correct on targets
        // where a `*const` is narrower than the `u64` tag and tail padding is
        // therefore inserted.
        assert_eq!(offset_of!(HandleHeader, kind), 0);
        assert_eq!(
            offset_of!(HandleHeader, owner),
            size_of::<HandleKind>(),
            "`owner` must follow `kind` immediately; a gap here would make \
             `peek_handle_prefix` read the wrong bytes"
        );

        // The prefix must lead every tagged handle at exactly those offsets — that
        // is precisely what lets `peek_handle_prefix` read `(kind, owner)` without
        // knowing the concrete handle type behind the opaque `state` pointer.
        // `InflateHandle`/`InflateBackHandle` are private to `ffi::inflate` and are
        // asserted the same way in that module's own tests.
        assert_eq!(offset_of!(DeflateHandle, kind), 0);
        assert_eq!(
            offset_of!(DeflateHandle, owner),
            offset_of!(HandleHeader, owner)
        );
        assert!(size_of::<DeflateHandle>() >= size_of::<HandleHeader>());
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

    /// `deflate_state`/`deflate_take` gate on the *whole* of C's
    /// `deflateStateCheck` (`deflate.c` L538-L556), clause by clause: the
    /// allocator pair, the installed handle, the engine kind, the owning
    /// `z_stream`, and the status ladder.
    ///
    /// Each clause is toggled in isolation and then restored, so a clause that
    /// silently stopped being enforced fails here rather than surfacing as a
    /// double free or a cross-engine `Layout` mismatch in a consumer.
    #[test]
    fn deflate_state_check_enforces_every_c_clause() {
        // No handle installed: every accessor reports absence without touching
        // the null pointer.
        let mut strm = zeroed_stream();
        // SAFETY: `state` is null, so no dereference occurs.
        assert!(unsafe { peek_handle_kind(&strm) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { deflate_state(&mut strm) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { deflate_take(&mut strm) }.is_none());

        // Build a genuine engine behind a genuine caller hook, so clause 5 (the
        // status ladder) has a real `DeflateStatus` to inspect and the stream's
        // advertised allocator pair matches the one the buffers came from.
        // `stats` is declared first so it outlives every allocation made through
        // its hook.
        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let hook = stats.hook();
        let calloc = CAllocator {
            zalloc: hook.zalloc(),
            zfree: hook.zfree(),
            opaque: hook.opaque(),
        };
        let mut zs = ZStream::with_allocator(calloc);
        crate::deflate::deflate_init2(&mut zs, 6, 8, 15, 8, crate::constants::Strategy::Default)
            .expect("a default deflate init must succeed");
        strm.zalloc = calloc.zalloc;
        strm.zfree = calloc.zfree;
        strm.opaque = calloc.opaque;

        // Installing binds the owner, which is what every clause-4b check below
        // compares against. `strm` must not be moved from here on.
        // SAFETY: `state` is null, so nothing is leaked, and the box is reclaimed
        // exactly once by the `deflate_take` at the end.
        unsafe { install_handle(&mut strm, Box::new(DeflateHandle::new(zs))) };
        // SAFETY: `state` holds the live `Box<DeflateHandle>` installed above,
        // whose `#[repr(C)]` prefix is readable.
        assert_eq!(
            unsafe { peek_handle_kind(&strm) },
            Some(HandleKind::DEFLATE)
        );
        // SAFETY: as above; every clause holds, so the borrow is handed out.
        assert!(
            unsafe { deflate_state(&mut strm) }.is_some(),
            "a correctly installed, owner-bound, initialized handle must pass"
        );

        // --- Clause 2: `strm->zalloc == 0 || strm->zfree == 0` ---------------
        for (label, clear_zalloc) in [("zalloc", true), ("zfree", false)] {
            let saved_alloc = strm.zalloc;
            let saved_free = strm.zfree;
            if clear_zalloc {
                strm.zalloc = None;
            } else {
                strm.zfree = None;
            }
            // SAFETY: `state` still holds the live handle; only `strm`'s own
            // `Copy` hook fields were changed.
            assert!(
                !unsafe { deflate_state_check(&strm) },
                "C rejects a stream whose {label} hook has been cleared"
            );
            // SAFETY: as above; a rejected take must not reconstitute the box.
            assert!(unsafe { deflate_take(&mut strm) }.is_none());
            assert!(!strm.state.is_null());
            strm.zalloc = saved_alloc;
            strm.zfree = saved_free;
        }

        // --- Clause 4a: the Rust-only engine-kind tag -----------------------
        // Overwrite the tag with a foreign engine's magic, emulating a caller
        // that passes an inflate-initialized stream to `deflateEnd`.
        // SAFETY: `state` points at a live, C-layout `DeflateHandle` whose prefix
        // is a `HandleHeader`, so this writes exactly that field and no other.
        unsafe {
            (*(strm.state as *mut HandleHeader)).kind = HandleKind::INFLATE;
        }
        // SAFETY: `state` still points at the live handle allocation, so the
        // prefix remains readable.
        assert!(unsafe { deflate_state(&mut strm) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { deflate_take(&mut strm) }.is_none());
        assert!(
            !strm.state.is_null(),
            "a rejected take must leave `state` installed, never dropping a \
             wrong-type box"
        );
        // SAFETY: as for the corrupting write above — same allocation, same field.
        unsafe {
            (*(strm.state as *mut HandleHeader)).kind = HandleKind::DEFLATE;
        }

        // --- Clause 4b: `s->strm != strm`, the owner check ------------------
        // This is the transplantation guard. A caller who `memcpy`s the 14-field
        // `z_stream` gets a second struct naming the SAME handle; C refuses every
        // call through the copy, and so must this. Emulating it by rewriting the
        // recorded owner is equivalent to — and far safer than — building a real
        // copy, because a real copy would leave a second struct able to double
        // free if the guard regressed.
        let foreign = 0xDEAD_BEEF_usize as *const z_stream;
        // SAFETY: same allocation, same `#[repr(C)]` prefix; `owner` is a raw
        // pointer field that is only ever compared, never dereferenced.
        unsafe {
            (*(strm.state as *mut HandleHeader)).owner = foreign;
        }
        // SAFETY: `state` still points at the live handle allocation.
        assert!(
            !unsafe { deflate_state_check(&strm) },
            "a handle owned by a different z_stream must be refused"
        );
        // SAFETY: as above.
        assert!(unsafe { deflate_state(&mut strm) }.is_none());
        // SAFETY: as above — and this is the assertion the ownership tag exists
        // for: a wrong-owner reclaim must NOT reconstitute the box.
        assert!(unsafe { deflate_take(&mut strm) }.is_none());
        assert!(
            !strm.state.is_null(),
            "a wrong-owner take must leave `state` installed so the real owner \
             can still reclaim it exactly once"
        );
        // SAFETY: as above; restore the true owner.
        unsafe {
            (*(strm.state as *mut HandleHeader)).owner = &raw const strm;
        }

        // --- All clauses restored: the true owner reclaims exactly once ------
        // SAFETY: every clause holds again, so the live `Box<DeflateHandle>` is
        // reconstituted exactly once here.
        assert!(unsafe { deflate_take(&mut strm) }.is_some());
        assert!(
            strm.state.is_null(),
            "a successful take must null `state` to prevent a double free"
        );
        assert_eq!(
            stats.live_bytes(),
            0,
            "reclaiming through the owner must release every hook-backed region"
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

    /// Geometry regression: the FFI init path must present the caller's `zalloc`
    /// with the same argument *shape* — in the same order, and the same number of
    /// times — that C `deflateInit2_` passes.
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
    /// (AAP §0.3.2 rule T3).
    ///
    /// All five requests reproduce C's `(items, size)` pairs exactly, the state's
    /// included: it is `(1, `[`DeflateState::C_LAYOUT_SIZE`]`)`, C's own
    /// `sizeof(deflate_state)`, so an allocator sized from C's header serves this
    /// port exactly as it serves reference zlib (AAP §0.6.5). Because a block of
    /// C's `sizeof` cannot hold this port's legitimately larger state value, the
    /// charge and the value are separate objects — the caller's region is held
    /// unread until its matching `zfree` and the state sits beside it on the global
    /// heap — which is what lets the observable pair, count, order, failure timing
    /// and release order all stay C-exact.
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
        // The C-parity init path — the one the FFI `deflateInit2_` shim drives —
        // charges the state to the hook as well as the four working buffers.
        let state = DeflateState::new_in_with_detail(
            &HookAllocator::new(hook),
            6,
            Z_DEFLATED,
            15,
            8,
            Strategy::Default,
            1,
        )
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
        let state = DeflateState::new_in_with_detail(
            &HookAllocator::new(hook),
            6,
            Z_DEFLATED,
            9,
            4,
            Strategy::Default,
            1,
        )
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
            // (C's own `sizeof`; see the init-geometry test for why the state
            // charge and the state value are separate objects)
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
        assert!(
            copy.pending_buf.is_foreign() && copy.is_charged(),
            "the copy's buffers live in the caller's arena and its state footprint \
             was charged to the caller's `zalloc`"
        );
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
            let state = DeflateState::new_in_with_detail(
                &HookAllocator::new(hook),
                6,
                Z_DEFLATED,
                15,
                8,
                Strategy::Default,
                1,
            )
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

        // --- inflate: the state itself, then the lazily grown window ---------
        NALLOC.store(0, Ordering::SeqCst);
        NFREE.store(0, Ordering::SeqCst);
        for cell in &FREED {
            cell.store(usize::MAX, Ordering::SeqCst);
        }
        {
            // Take the two hook-backed regions in C's order: the state itself
            // (`inflate.c` L198) and then the window (L261). The reservation is what
            // the real `inflate_init2` takes, and filling it puts the state *in* the
            // caller's region rather than beside it.
            let reservation =
                crate::stream::EngineReservation::<InflateState>::take(&HookAllocator::new(hook))
                    .expect("healthy hook serves the state");
            let mut state = reservation
                .fill(InflateState::build_in(hook, 0, 15))
                .expect("filling a secured region cannot fail");
            state.window =
                AllocBuffer::try_zeroed(1 << 15, hook).expect("healthy hook serves the window");
            assert_eq!(NALLOC.load(Ordering::SeqCst), 2);
            drop(state);
        }
        assert_eq!(NFREE.load(Ordering::SeqCst), 2);
        let order: [usize; 2] = core::array::from_fn(|i| FREED[i].load(Ordering::SeqCst));
        assert_eq!(
            order,
            // 0 = the state, 1 = window.
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
        let state = DeflateState::new_in_with_detail(
            &HookAllocator::new(hook),
            6,
            Z_DEFLATED,
            9,
            1,
            Strategy::Default,
            1,
        )
        .expect("healthy hook initializes the state");
        assert!(state.window.is_foreign() && state.is_charged());

        {
            let copy = state
                .try_copy_in(&HookAllocator::new(hook))
                .expect("healthy hook duplicates the state and its buffers");
            assert!(
                copy.window.is_foreign()
                    && copy.prev.is_foreign()
                    && copy.head.is_foreign()
                    && copy.pending_buf.is_foreign()
                    && copy.is_charged(),
                "every buffer of the copy must stay in the caller's arena, and the \
                 copy's state footprint must be charged to the caller's `zalloc`"
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
        let idi = unsafe { gz_header_to_idiomatic(&src) }
            .expect("the three copies are tiny and cannot exhaust the allocator")
            .expect("non-null header");
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
        assert!(
            unsafe { gz_header_to_idiomatic(ptr::null()) }
                .expect("the null case allocates nothing and cannot fail")
                .is_none()
        );
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
    /// represent (`inflate.c` L522-L523).
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
    /// (`inflate.c` L605-L606, L650-L651, L672-L673) without touching the buffer
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
    // Every fallible shim body in `src/ffi/**` runs inside one of the eight
    // guards above, because a Rust panic unwinding across the C ABI is
    // undefined behavior. The guards are eight *separate* implementations, one
    // per C return width, and each is the last line of defense for the shims
    // that use it, so each is exercised directly rather than by analogy:
    //
    // | Guard             | C return type          | Representative shims                        |
    // |-------------------|------------------------|---------------------------------------------|
    // | `guard_int`       | `int`                  | `deflate`, `inflate`, `gzread`, `gzclose`    |
    // | `guard_ulong`     | `uLong`                | `adler32`, `crc32`, `compressBound`          |
    // | `guard_long`      | `long`                 | `inflateMark`                               |
    // | `guard_size`      | `z_size_t`             | `gzfread`, `gzfwrite`, `compressBound_z`     |
    // | `guard_ptr`       | `T *`                  | `gzgets`, `gzopen`                          |
    // | `guard_const_ptr` | `const T *`            | `zError`, `zlibVersion`, `gzerror`           |
    // | `guard_off`       | `z_off_t`/`z_off64_t`  | `gzseek`, `gztell`, `gzoffset`              |
    // | `guard_void`      | `void`                 | `gzclearerr`                                |
    //
    // `ffi::types` is the single home for all eight; no shim file may define its
    // own guard or open a bare `catch_unwind`, which
    // `every_boundary_guard_lives_in_this_module` asserts structurally.
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

    /// [`guard_long`] passes a [`c_long`] through unchanged — including the `-1`
    /// sentinel `inflateMark` returns for an absent state — and substitutes its
    /// exact default when the body panics.
    ///
    /// The signedness matters for the same reason it does in [`guard_off`]:
    /// `inflateMark`'s documented failure value is `-1 << 16`, so a guard that
    /// routed the value through an unsigned type would hand the caller a huge
    /// positive mark.
    #[cfg(feature = "std")]
    #[test]
    fn guard_long_catches_panic_and_passes_value() {
        assert_eq!(guard_long(-1, || 0), 0);
        assert_eq!(guard_long(0, || -1), -1);
        assert_eq!(guard_long(0, || -(1 << 16)), -(1 << 16));
        assert_eq!(guard_long(0, || c_long::MIN), c_long::MIN);
        assert_eq!(guard_long(0, || c_long::MAX), c_long::MAX);

        let caught = with_silenced_panic_hook(|| guard_long(-1, || panic!("boundary panic")));
        assert_eq!(caught, -1);

        let marked =
            with_silenced_panic_hook(|| guard_long(-(1 << 16), || panic!("boundary panic")));
        assert_eq!(marked, -(1 << 16));
    }

    /// [`guard_size`] passes a [`z_size_t`] through unchanged and substitutes its
    /// exact default when the body panics.
    ///
    /// The `_z` family (`gzfread`, `gzfwrite`, `compressBound_z`) reports failure
    /// as `0`, which is also a perfectly ordinary success value, so both must
    /// survive the guard distinctly.
    #[cfg(feature = "std")]
    #[test]
    fn guard_size_catches_panic_and_passes_value() {
        assert_eq!(guard_size(1, || 0), 0);
        assert_eq!(guard_size(0, || 1), 1);
        assert_eq!(guard_size(0, || z_size_t::MAX), z_size_t::MAX);

        let caught = with_silenced_panic_hook(|| guard_size(0, || panic!("boundary panic")));
        assert_eq!(caught, 0);

        let nonzero = with_silenced_panic_hook(|| guard_size(0xFEED, || panic!("boundary panic")));
        assert_eq!(nonzero, 0xFEED);
    }

    /// [`guard_const_ptr`] passes a `*const T` through by **identity** and
    /// substitutes its default when the body panics.
    ///
    /// This is the guard behind the shims that hand a C caller a pointer into
    /// static storage — `zError`, `zlibVersion`, `gzerror` — where returning a
    /// re-derived pointer instead of the exact one produced would be a silent
    /// corruption. Exercised at two pointee types so the generic parameter is
    /// proven live rather than accidentally monomorphic.
    #[cfg(feature = "std")]
    #[test]
    fn guard_const_ptr_catches_panic_and_passes_value() {
        let value: c_int = 42;
        let live: *const c_int = &raw const value;

        assert_eq!(guard_const_ptr(ptr::null(), || live), live);

        let nulled = with_silenced_panic_hook(|| {
            guard_const_ptr(ptr::null::<c_int>(), || panic!("boundary"))
        });
        assert!(nulled.is_null());

        let fallback = with_silenced_panic_hook(|| guard_const_ptr(live, || panic!("boundary")));
        assert_eq!(fallback, live);
        assert_eq!(value, 42, "the guard moves pointers, never the pointee");

        // `c_char` is the pointee that actually matters: it is what `zError`,
        // `zlibVersion` and `gzerror` return.
        let text = c"boundary";
        let text_ptr: *const c_char = text.as_ptr();
        assert_eq!(guard_const_ptr(ptr::null(), || text_ptr), text_ptr);
        let text_nulled = with_silenced_panic_hook(|| {
            guard_const_ptr(ptr::null::<c_char>(), || panic!("boundary"))
        });
        assert!(text_nulled.is_null());
    }

    /// [`guard_void`] runs its body and swallows a panic without a value to
    /// substitute.
    ///
    /// `gzclearerr` returns `void`, so there is no sentinel a C caller could
    /// observe — which makes the guard's only obligations to (a) actually invoke
    /// the body and (b) never let an unwind reach the C frame. Both are asserted:
    /// the side effect proves the body ran, and the surviving assertion after the
    /// panicking call proves the unwind was contained.
    #[cfg(feature = "std")]
    #[test]
    fn guard_void_runs_its_body_and_contains_a_panic() {
        let mut ran = false;
        guard_void(core::panic::AssertUnwindSafe(|| ran = true));
        assert!(ran, "guard_void must invoke its body");

        let mut reached = false;
        with_silenced_panic_hook(|| {
            guard_void(core::panic::AssertUnwindSafe(|| {
                reached = true;
                panic!("boundary panic");
            }));
        });
        assert!(
            reached,
            "the body must have started before the panic was contained"
        );
    }

    /// `ffi::types` is the **only** module that may define a boundary guard or
    /// open a bare `catch_unwind`.
    ///
    /// Before this was centralized, `ffi::inflate` carried its own `guard_long`,
    /// `ffi::gz` carried its own `guard_size` and `guard_const_ptr`, and
    /// `gzclearerr` opened a bare `std::panic::catch_unwind` with no guard at all.
    /// Four independent implementations of one safety-critical primitive is four
    /// places for the `no_std` arm, the `AssertUnwindSafe` boundary, or the
    /// default-substitution semantics to drift apart, and the drift would be
    /// invisible: each copy passes its own tests.
    ///
    /// The invariant is therefore asserted structurally, over the source itself.
    /// Comments are stripped first, so the prose above cannot satisfy it.
    #[test]
    fn every_boundary_guard_lives_in_this_module() {
        /// Every `src/ffi/**` module that must contain no guard definition.
        const SHIM_FILES: [&str; 6] = [
            "src/ffi/mod.rs",
            "src/ffi/deflate.rs",
            "src/ffi/inflate.rs",
            "src/ffi/gz.rs",
            "src/ffi/util.rs",
            "src/ffi/alloc.rs",
        ];

        /// The canonical guard names. Each must be defined here, twice — once for
        /// `std` and once for `no_std`.
        const CANONICAL: [&str; 8] = [
            "guard_int",
            "guard_ulong",
            "guard_long",
            "guard_size",
            "guard_ptr",
            "guard_const_ptr",
            "guard_off",
            "guard_void",
        ];

        let strip = |text: &str| -> std::string::String {
            text.lines()
                .map(|line| match line.find("//") {
                    Some(at) => &line[..at],
                    None => line,
                })
                .collect::<std::vec::Vec<_>>()
                .join("\n")
        };
        let read = |relative: &str| -> std::string::String {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
            strip(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|err| panic!("{} must be readable: {err}", path.display())),
            )
        };

        // Every canonical guard is defined here, in both feature arms.
        let home = read("src/ffi/types.rs");
        for name in CANONICAL {
            let needle = std::format!("fn {name}");
            let count = home.matches(needle.as_str()).count();
            assert!(
                count >= 2,
                "`{name}` must be defined in src/ffi/types.rs for both the `std` \
                 and `no_std` arms; found {count} definition(s)"
            );
        }

        // No shim file defines one, and none opens a bare `catch_unwind`.
        let mut strays: std::vec::Vec<std::string::String> = std::vec::Vec::new();
        for relative in SHIM_FILES {
            let text = read(relative);
            for name in CANONICAL {
                if text.contains(&std::format!("fn {name}(")) {
                    strays.push(std::format!(
                        "{relative} defines `{name}`; the single home is \
                         src/ffi/types.rs"
                    ));
                }
                if text.contains(&std::format!("fn {name}<")) {
                    strays.push(std::format!(
                        "{relative} defines generic `{name}`; the single home is \
                         src/ffi/types.rs"
                    ));
                }
            }
            if text.contains("catch_unwind") {
                strays.push(std::format!(
                    "{relative} opens a bare `catch_unwind`; route the body \
                     through the guard for its C return width instead"
                ));
            }
        }
        assert!(
            strays.is_empty(),
            "boundary guards must live only in src/ffi/types.rs:\n{}",
            strays.join("\n")
        );
    }

    /// Pass-through holds for **both** implementations of all **eight** guards.
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

        assert_eq!(guard_long(-1, || 0), 0);
        assert_eq!(guard_long(0, || c_long::MIN), c_long::MIN);
        assert_eq!(guard_long(0, || c_long::MAX), c_long::MAX);

        assert_eq!(guard_size(1, || 0), 0);
        assert_eq!(guard_size(0, || z_size_t::MAX), z_size_t::MAX);

        assert_eq!(guard_off(-1, || 2_147_483_648), 2_147_483_648);
        assert_eq!(guard_off(0, || z_off64_t::MIN), z_off64_t::MIN);

        let mut value: c_int = 42;
        let live: *mut c_int = &raw mut value;
        assert_eq!(guard_ptr(ptr::null_mut(), || live), live);
        assert_eq!(value, 42, "the guard moves pointers, never the pointee");

        let frozen: *const c_int = &raw const value;
        assert_eq!(guard_const_ptr(ptr::null(), || frozen), frozen);
        assert_eq!(value, 42);

        // `guard_void` has no value to pass through, so the property it must hold
        // is that the body actually runs: a guard that swallowed its closure
        // would silently turn `gzclearerr` into a no-op.
        let mut ran = false;
        guard_void(core::panic::AssertUnwindSafe(|| ran = true));
        assert!(ran, "guard_void must invoke its body in every feature row");
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
        // SAFETY: `strm` is a freshly built, live, exclusively-owned `z_stream` and
        // the prologue only reads and writes its plain `Copy` fields — including the
        // `zalloc`/`zfree`/`opaque` fields set just above, which are a function
        // pointer and a raw cookie the prologue never calls or dereferences.
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
        // SAFETY: `strm` is a freshly built, live, exclusively-owned `z_stream` and
        // the prologue only reads and writes its plain `Copy` fields — including the
        // `zalloc`/`zfree`/`opaque` fields set just above, which are a function
        // pointer and a raw cookie the prologue never calls or dereferences.
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
        // SAFETY: `strm` is a freshly built, live, exclusively-owned `z_stream` and
        // the prologue only reads and writes its plain `Copy` fields — including the
        // `zalloc`/`zfree`/`opaque` fields set just above, which are a function
        // pointer and a raw cookie the prologue never calls or dereferences.
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

    /// `rewind_input` must reproduce C's `uInt` wrap instead of overflowing.
    ///
    /// C recomputes the count in `unsigned` arithmetic with no overflow check
    /// (`inffast.c` L295), so a caller presenting `avail_in` at or near
    /// `u32::MAX` and triggering a buffered-byte rewind observes a wrapped value.
    /// A checked `+` would instead panic — and under this crate's `panic =
    /// "abort"` profiles that terminates the host process, turning a defined C
    /// result into a denial of service. The cursor and the count must also be
    /// committed together, so no state where one moved and the other did not is
    /// observable.
    #[test]
    fn rewind_input_wraps_at_u32_max_like_c() {
        // A real buffer so `next_in` has honest provenance; `rewind_input` never
        // dereferences it, but the pointer arithmetic should stay in-bounds.
        let buf = [0u8; 8];
        let base = buf.as_ptr();

        // Exactly at the boundary: one rewound byte must wrap the count to 0.
        let mut strm = zeroed_stream();
        strm.next_in = unsafe { base.add(4) };
        strm.avail_in = c_uint::MAX;
        // SAFETY: one byte precedes `next_in` inside `buf`, which is still live.
        unsafe { rewind_input(&mut strm, 1) };
        assert_eq!(
            strm.avail_in, 0,
            "u32::MAX + 1 must wrap to 0 exactly as C's uInt arithmetic does"
        );
        assert_eq!(
            strm.next_in,
            unsafe { base.add(3) },
            "the cursor must move back by the rewound count"
        );

        // Past the boundary: the wrap is modulo 2^32, not a saturation.
        let mut over = zeroed_stream();
        over.next_in = unsafe { base.add(4) };
        over.avail_in = c_uint::MAX - 1;
        // SAFETY: three bytes precede `next_in` inside the live `buf`.
        unsafe { rewind_input(&mut over, 3) };
        assert_eq!(
            over.avail_in, 1,
            "(u32::MAX - 1) + 3 must wrap to 1, matching C's uInt overflow"
        );
        assert_eq!(over.next_in, unsafe { base.add(1) });

        // A zero rewind is a no-op on both fields (the early return).
        let mut none = zeroed_stream();
        none.next_in = unsafe { base.add(4) };
        none.avail_in = c_uint::MAX;
        // SAFETY: a zero rewind touches nothing.
        unsafe { rewind_input(&mut none, 0) };
        assert_eq!(none.avail_in, c_uint::MAX);
        assert_eq!(none.next_in, unsafe { base.add(4) });

        // The ordinary case is unchanged: no wrap, plain addition.
        let mut plain = zeroed_stream();
        plain.next_in = unsafe { base.add(4) };
        plain.avail_in = 10;
        // SAFETY: two bytes precede `next_in` inside the live `buf`.
        unsafe { rewind_input(&mut plain, 2) };
        assert_eq!(plain.avail_in, 12);
        assert_eq!(plain.next_in, unsafe { base.add(2) });
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

    // -- CRawHeaderSink / borrow_gz_header_sink -----------------------------

    /// A `gz_header` whose three payload buffers are carved out of one backing
    /// allocation at the given offsets, so overlap can be constructed exactly.
    #[cfg(feature = "gzip")]
    fn header_over(
        backing: &mut [u8],
        extra: Option<(usize, usize)>,
        name: Option<(usize, usize)>,
        comment: Option<(usize, usize)>,
    ) -> gz_header {
        let base = backing.as_mut_ptr();
        // SAFETY: every caller below keeps `offset + cap` inside `backing`.
        let at = |slot: Option<(usize, usize)>| match slot {
            Some((offset, cap)) => (unsafe { base.add(offset) }, cap as c_uint),
            None => (ptr::null_mut(), 0),
        };
        let (extra_ptr, extra_max) = at(extra);
        let (name_ptr, name_max) = at(name);
        let (comment_ptr, comm_max) = at(comment);
        gz_header {
            text: 0,
            time: 0,
            xflags: 0,
            os: 0,
            extra: extra_ptr,
            extra_len: 0,
            extra_max,
            name: name_ptr,
            name_max,
            comment: comment_ptr,
            comm_max,
            hcrc: 0,
            done: 0,
        }
    }

    /// A null `gz_header` yields no descriptors at all — C's "no header
    /// registered" state.
    #[test]
    #[cfg(feature = "gzip")]
    fn borrowing_a_null_header_sink_yields_nothing() {
        assert!(unsafe { borrow_gz_header_sink(ptr::null_mut()) }.is_none());
    }

    /// C's two "field absent" encodings — a null pointer, and a capacity that
    /// admits no byte — must both produce an absent descriptor, because the
    /// decoder's store predicate is `Option`-plus-bounds.
    #[test]
    #[cfg(feature = "gzip")]
    fn an_absent_or_zero_capacity_field_yields_no_descriptor() {
        let mut backing = [0u8; 16];
        // `name` present with capacity 0, `comment` absent, `extra` present.
        let mut head = header_over(&mut backing, Some((0, 4)), Some((8, 0)), None);
        head.extra_len = 7;
        let mut sinks = unsafe { borrow_gz_header_sink(&mut head) }.expect("head is non-null");
        let view = sinks.view();
        assert_eq!(
            view.extra_len, 7,
            "the live declared XLEN travels in the view"
        );
        assert!(
            view.extra.is_some(),
            "a non-null field with capacity is present"
        );
        assert!(view.name.is_none(), "a zero capacity is C's absent field");
        assert!(view.comment.is_none(), "a null pointer is C's absent field");
    }

    /// Each descriptor's stores are bounded by its own capacity and refuse — never
    /// clamp — an out-of-range target, exactly as C's guards drop such bytes.
    #[test]
    #[cfg(feature = "gzip")]
    fn a_raw_descriptor_refuses_out_of_range_stores() {
        let mut backing = [0xAAu8; 16];
        let mut head = header_over(&mut backing, Some((0, 4)), Some((8, 2)), None);
        let mut sinks = unsafe { borrow_gz_header_sink(&mut head) }.expect("head is non-null");
        let mut view = sinks.view();

        assert_eq!(view.store_extra(0, b"ABCDEF"), 4, "clamped to extra_max");
        assert_eq!(
            view.store_extra(4, b"Z"),
            0,
            "an offset at the capacity stores nothing"
        );
        assert_eq!(
            view.store_extra(usize::MAX, b"Z"),
            0,
            "and neither does a wild one"
        );
        assert!(view.store_name(1, b'!'));
        assert!(!view.store_name(2, b'?'), "clamped to name_max");
        assert!(!view.store_name(usize::MAX, b'?'));

        assert_eq!(&backing[..4], b"ABCD");
        assert!(
            backing[4..8].iter().all(|&b| b == 0xAA),
            "nothing may be written between the two fields"
        );
        assert_eq!(&backing[8..10], &[0xAA, b'!']);
        assert!(backing[10..].iter().all(|&b| b == 0xAA));
    }

    /// Overlapping payload buffers behave as in C: the writes are independent, so
    /// the last one to land owns the shared bytes.
    ///
    /// This configuration is undefined behaviour to express as `&mut [u8]` and is
    /// the reason the descriptors exist.
    #[test]
    #[cfg(feature = "gzip")]
    fn overlapping_raw_descriptors_write_independently() {
        let mut backing = [0xAAu8; 8];
        // All three fields address the same 8 bytes.
        let mut head = header_over(&mut backing, Some((0, 8)), Some((0, 8)), Some((0, 8)));
        let mut sinks = unsafe { borrow_gz_header_sink(&mut head) }.expect("head is non-null");
        let mut view = sinks.view();

        assert_eq!(view.store_extra(0, b"1234"), 4);
        assert!(view.store_name(0, b'n'));
        assert!(view.store_comment(1, b'c'));

        assert_eq!(&backing[..4], b"nc34", "each store lands independently");
        assert!(backing[4..].iter().all(|&b| b == 0xAA));
    }

    /// The window-intersection test is a half-open range overlap over plain
    /// addresses: adjacency is not overlap, and an empty range never overlaps.
    #[test]
    #[cfg(feature = "gzip")]
    fn descriptor_intersection_is_half_open_range_overlap() {
        let mut backing = [0u8; 32];
        let mut head = header_over(&mut backing, Some((8, 4)), None, None);
        let sinks = unsafe { borrow_gz_header_sink(&mut head) }.expect("head is non-null");

        let base = backing.as_ptr() as usize;
        let field = (base + 8, base + 12);
        assert!(
            sinks.intersects(field.0, field.1),
            "the field overlaps itself"
        );
        assert!(
            sinks.intersects(base, base + 32),
            "an enclosing window overlaps"
        );
        assert!(
            sinks.intersects(base + 11, base + 20),
            "a partial tail overlaps"
        );
        assert!(sinks.intersects(base, base + 9), "a partial head overlaps");
        assert!(
            !sinks.intersects(base, base + 8),
            "a window ending where the field begins is adjacent, not overlapping"
        );
        assert!(
            !sinks.intersects(base + 12, base + 32),
            "a window beginning where the field ends is adjacent, not overlapping"
        );
        assert!(
            !sinks.intersects(field.0, field.0),
            "an empty window never overlaps"
        );
    }

    /// A `extra_max` large enough that `ptr + cap` may wrap the address space — a
    /// `c_uint` capacity is up to 4 GiB, which a 32-bit target cannot always add to
    /// a real address — must neither panic nor mis-answer the intersection test.
    ///
    /// The comparison is over the *declared* capacity, because that is the region C
    /// is entitled to write, and the range end saturates rather than overflowing.
    #[test]
    #[cfg(feature = "gzip")]
    fn a_capacity_that_could_wrap_the_address_space_saturates() {
        let mut backing = [0u8; 4];
        let mut head = header_over(&mut backing, Some((0, 4)), None, None);
        head.extra_max = c_uint::MAX;
        let sinks = unsafe { borrow_gz_header_sink(&mut head) }.expect("head is non-null");

        let base = backing.as_ptr() as usize;
        assert!(
            sinks.intersects(base.saturating_add(4096), base.saturating_add(8192)),
            "a window inside the declared capacity must overlap, even though it is \
             far past the four bytes actually backing it"
        );
        assert!(
            !sinks.intersects(base.wrapping_sub(64), base),
            "a window ending at the field's first byte is still adjacent, not \
             overlapping"
        );
        // The extreme window: the only requirement is a defined answer, arrived at
        // without overflowing the saturating end.
        let _ = sinks.intersects(usize::MAX - 1, usize::MAX);
    }
    // -- CRawHeaderSource / read_gz_header_source ---------------------------

    /// A `gz_header` presenting `extra`/`name`/`comment` as **read** fields carved
    /// out of one backing allocation, so overlap can be constructed exactly.
    ///
    /// `extra` takes an explicit declared length so the `> 0xffff` case is
    /// reachable; `name`/`comment` are located by their offset and terminated by
    /// whatever NUL the backing bytes already contain.
    #[cfg(feature = "gzip")]
    fn source_header(
        backing: &mut [u8],
        extra: Option<(usize, c_uint)>,
        name: Option<usize>,
        comment: Option<usize>,
    ) -> gz_header {
        let base = backing.as_mut_ptr();
        // SAFETY: every caller below keeps the offsets inside `backing`.
        let at = |slot: Option<usize>| match slot {
            Some(offset) => unsafe { base.add(offset) },
            None => ptr::null_mut(),
        };
        let (extra_ptr, extra_len) = match extra {
            Some((offset, len)) => (at(Some(offset)), len),
            None => (ptr::null_mut(), 0),
        };
        gz_header {
            text: 1,
            time: 0x0102_0304,
            xflags: 0,
            os: 3,
            extra: extra_ptr,
            extra_len,
            extra_max: 0,
            name: at(name),
            name_max: 0,
            comment: at(comment),
            comm_max: 0,
            hcrc: 1,
            done: 0,
        }
    }

    /// A null `head` describes nothing, exactly as C's `gzhead == Z_NULL` means "no
    /// registered header".
    #[test]
    #[cfg(feature = "gzip")]
    fn reading_a_null_header_source_yields_nothing() {
        // SAFETY: a null `head` is the documented "no header" input.
        assert!(unsafe { read_gz_header_source(ptr::null()) }.is_none());
    }

    /// Null distinguishes "absent" from "empty": a non-null pointer with a zero
    /// length is a *present* field, because C keys the FLG bits off the pointer
    /// (`deflate.c` L1106-L1108), never off the length.
    #[test]
    #[cfg(feature = "gzip")]
    fn a_null_field_is_absent_but_a_zero_length_field_is_present() {
        let mut backing = *b"\0padding";
        let mut head = source_header(&mut backing, Some((0, 0)), Some(0), None);
        // SAFETY: `head` is a live, valid `gz_header`; `name` points at a NUL.
        let src = unsafe { read_gz_header_source(&head) }.expect("head is non-null");
        // SAFETY: nothing else references `backing` for the duration of the borrow.
        let view = unsafe { src.borrow() };
        assert_eq!(view.extra, Some(&[][..]), "extra_len 0 is a present field");
        assert_eq!(
            view.name,
            Some(&[][..]),
            "an immediate NUL is a present field"
        );
        assert_eq!(view.comment, None, "a null comment is absent");
        assert!(view.text, "scalars come through");
        assert!(view.hcrc);
        assert_eq!(view.time, 0x0102_0304);
        assert_eq!(view.os, 3);

        head.extra = ptr::null_mut();
        // SAFETY: `head` is still the live, valid `gz_header` built above; clearing
        // `extra` only makes that field absent, and `name` still points at a NUL
        // inside `backing`.
        let src = unsafe { read_gz_header_source(&head) }.expect("head is non-null");
        // SAFETY: `src` borrows `backing`, which lives to the end of this test and is
        // not referenced anywhere else for the duration of the borrow.
        assert_eq!(unsafe { src.borrow() }.extra, None);
    }

    /// `name`/`comment` lengths come from a NUL scan; `extra`'s comes from
    /// `extra_len` **verbatim**, un-masked.
    ///
    /// The mask matters: C emits `extra_len & 0xffff` (`deflate.c` L1120), and that
    /// value is not recoverable from a pre-masked length — `0x1_0005 & 0xffff` is
    /// `5`, whereas masking `0x1_0000` first yields `0`. Recording the raw length and
    /// letting the engine mask is the only faithful arrangement.
    #[test]
    #[cfg(feature = "gzip")]
    fn field_lengths_are_the_callers_own() {
        let mut backing = std::vec![0x41u8; 0x1_0005];
        backing[0x1_0004] = 0;
        let head = source_header(&mut backing, Some((0, 0x1_0005)), Some(0), None);
        // SAFETY: `head` is valid; `extra` is readable for `extra_len` and `name`
        // reaches a NUL inside `backing`.
        let src = unsafe { read_gz_header_source(&head) }.expect("head is non-null");
        // SAFETY: nothing else references `backing` here.
        let view = unsafe { src.borrow() };
        assert_eq!(
            view.extra.map(<[u8]>::len),
            Some(0x1_0005),
            "extra_len must be recorded verbatim so the engine can mask it"
        );
        assert_eq!(
            view.extra.map(|e| e.len() & 0xffff),
            Some(5),
            "and masking it must still give C's XLEN"
        );
        assert_eq!(
            view.name.map(<[u8]>::len),
            Some(0x1_0004),
            "name runs to its NUL, which the view excludes"
        );
    }

    /// Field intersection is half-open range overlap, with both empty cases
    /// answering "no".
    #[test]
    #[cfg(feature = "gzip")]
    fn source_intersection_is_half_open_range_overlap() {
        let mut backing = [0x41u8; 32];
        backing[19] = 0;
        // `extra` = [8, 12); `name` starts at 16 and runs to the NUL at 19.
        let head = source_header(&mut backing, Some((8, 4)), Some(16), None);
        let base = backing.as_ptr() as usize;
        // SAFETY: `head` is valid and its fields lie inside `backing`.
        let src = unsafe { read_gz_header_source(&head) }.expect("head is non-null");

        assert!(
            src.intersects(base + 8, base + 12),
            "the field overlaps itself"
        );
        assert!(
            src.intersects(base, base + 32),
            "an enclosing window overlaps"
        );
        assert!(
            src.intersects(base + 11, base + 20),
            "a partial tail overlaps"
        );
        assert!(
            src.intersects(base + 18, base + 32),
            "the name overlaps too"
        );
        assert!(
            !src.intersects(base, base + 8),
            "a window ending where extra begins is adjacent, not overlapping"
        );
        assert!(
            !src.intersects(base + 12, base + 16),
            "the gap between the two fields overlaps neither"
        );
        assert!(
            !src.intersects(base + 9, base + 9),
            "an empty window never overlaps"
        );

        // An empty *field* occupies no bytes and so overlaps nothing, however the
        // window is placed around it.
        let empty = source_header(&mut backing, Some((8, 0)), None, None);
        // SAFETY: `empty` is a live, valid `gz_header` whose only present field is
        // `extra`, declared at offset 8 of `backing` with length 0 — a zero-length
        // read at an in-bounds address, which needs no readable byte at all.
        let src = unsafe { read_gz_header_source(&empty) }.expect("head is non-null");
        assert!(
            !src.intersects(base, base + 32),
            "a present-but-empty field cannot share a byte with anything"
        );
    }

    /// Staging copies every present field, preserves presence and length exactly,
    /// and leaves the view borrowing the stage rather than the caller.
    #[test]
    #[cfg(feature = "gzip")]
    fn staging_copies_every_present_field_and_preserves_presence() {
        let mut backing = *b"EXTRAname\0comment\0";
        let head = source_header(&mut backing, Some((0, 5)), Some(5), Some(10));
        // SAFETY: `head` is valid and every field lies inside `backing`.
        let src = unsafe { read_gz_header_source(&head) }.expect("head is non-null");
        let mut stage = CGzHeaderStage::default();
        // SAFETY: nothing references `backing` mutably here.
        let view = unsafe { src.stage_into(&mut stage) }.expect("the copy must fit");

        assert_eq!(view.extra, Some(&b"EXTRA"[..]));
        assert_eq!(view.name, Some(&b"name"[..]));
        assert_eq!(view.comment, Some(&b"comment"[..]));
        assert!(view.text && view.hcrc);
        assert_eq!(view.time, 0x0102_0304);
        assert_eq!(view.os, 3);

        // The view must not alias the caller's storage: overwriting `backing` after
        // the copy cannot change what the engine will read.
        let staged_extra = view.extra.expect("present");
        let staged_addr = staged_extra.as_ptr() as usize;
        let caller_addr = backing.as_ptr() as usize;
        assert!(
            !(caller_addr..caller_addr + backing.len()).contains(&staged_addr),
            "a staged field must live in the stage, not in the caller's buffer"
        );
    }

    /// An absent field stages nothing and stays absent, so the FLG bits a staged
    /// header produces are the same ones the zero-copy view produces.
    #[test]
    #[cfg(feature = "gzip")]
    fn staging_an_absent_field_keeps_it_absent() {
        let mut backing = *b"only-extra";
        let head = source_header(&mut backing, Some((0, 4)), None, None);
        // SAFETY: `head` is valid and `extra` lies inside `backing`.
        let src = unsafe { read_gz_header_source(&head) }.expect("head is non-null");
        let mut stage = CGzHeaderStage::default();
        // SAFETY: nothing references `backing` mutably here.
        let staged = unsafe { src.stage_into(&mut stage) }.expect("the copy must fit");
        // SAFETY: as above.
        let direct = unsafe { src.borrow() };
        assert_eq!(staged.extra, direct.extra, "same bytes");
        assert_eq!(staged.name, None);
        assert_eq!(staged.comment, None);
        assert_eq!(
            staged.name, direct.name,
            "same presence as the zero-copy view"
        );
        assert_eq!(staged.comment, direct.comment);
    }
}
