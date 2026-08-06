//! Inflate state model: [`InflateMode`] (C `inflate_mode`) and [`InflateState`]
//! (C `struct inflate_state`) — a safe Rust port of `inflate.h`.
//!
//! This module defines the **data model** of the DEFLATE decompression state
//! machine. It contains no decoding logic; it merely holds the fields that must
//! persist between `inflate()` calls. The driver ([`crate::inflate`] `mod.rs`),
//! the fast decode loop (`fast.rs`), and the raw-callback back-inflate
//! (`back.rs`) all operate on an [`InflateState`], and the public streaming
//! type [`crate::stream::ZStream`] owns one as the boxed
//! [`EngineState`](crate::stream) in its engine slot.
//!
//! # Relationship to the C model
//!
//! The layout mirrors `struct inflate_state` from `inflate.h` field-for-field,
//! with three idiomatic transformations that make the whole decoder (outside
//! `fast.rs`) expressible in **safe** Rust:
//!
//! * **No back-pointer.** The C struct stores `z_streamp strm` — a pointer back
//!   to the owning stream. Ownership is inverted here: the stream owns the state
//!   as the boxed engine in its engine slot (per AAP §0.6.3), which is where the
//!   C `internal_state *state` pointer went, so the back-pointer is omitted
//!   entirely rather than modelled as a raw pointer.
//! * **Owned window.** The C `unsigned char FAR *window` (manually
//!   `ZALLOC`/`ZFREE`-managed) becomes an owned
//!   [`AllocBuffer<u8>`](crate::stream::AllocBuffer), which routes the
//!   allocation through the caller's `zalloc`/`zfree` hook when the `z_stream`
//!   installs an active pair and through the global allocator otherwise.
//!   Dropping the [`InflateState`] frees the window automatically, so RAII
//!   subsumes the `ZFREE(state->window)` performed by C `inflateEnd`
//!   (AAP §0.3.2).
//! * **Table offsets, not self-referential pointers.** The C `lencode` /
//!   `distcode` are `code const FAR *` pointers that may point *into* the
//!   state's own `codes[]` arena (dynamic Huffman blocks) or into the
//!   module-static fixed tables (`inffixed.h`). A self-referential borrow into
//!   an owned array cannot be expressed in safe Rust, so those pointers become
//!   `usize` offsets into [`codes`](InflateState::codes) paired with a
//!   [`TableSource`] discriminator. See [`TableSource`] and
//!   [`InflateState::lencode_slice`] for the resolution mechanism.
//!
//! # Safety
//!
//! This module contains **zero `unsafe`**, and that is *enforced*, not merely
//! asserted (AAP §0.6.2 / §0.7.2 standard S2, satisfying User Constraint 3
//! "zero unsafe blocks in core compression logic"). Three independent mechanisms
//! hold the line:
//!
//! 1. `src/lib.rs` carries a crate-wide `#![deny(unsafe_code)]`, so an `unsafe`
//!    block, `unsafe fn`, `unsafe impl`, `unsafe trait`, or `unsafe extern` block
//!    anywhere in this file is a **compile error**. Exactly two narrowly scoped
//!    `#[allow(unsafe_code)]` carve-outs exist — `mod no_std_support` (the
//!    freestanding `libc` allocator plus abort panic handler) and `pub mod ffi`
//!    (the C ABI boundary) — and neither covers the inflate layer.
//! 2. The crate-root boundary tests
//!    (`executable_unsafe_is_confined_to_the_designated_boundary` and
//!    `unsafe_code_denial_has_exactly_two_scoped_carve_outs`) re-derive the
//!    boundary from the source text, so smuggling in a *third* carve-out —
//!    which would still compile — fails the test suite.
//! 3. The `unsafe-boundary` CI job repeats the same assertions in a
//!    toolchain-independent shell check, so the gate still bites if the tests are
//!    weakened or removed.
//!
//! The module is also `no_std`-compatible: it references only `core`, `alloc`,
//! and the crate's own safe modules.

use alloc::boxed::Box;
use core::any::Any;

use crate::gz_header::GzHeader;
use crate::inflate::tables::{Code, ENOUGH};
use core::ffi::{c_int, c_uchar, c_uint, c_ulong, c_ushort, c_void};

use crate::stream::{
    AllocBuffer, AllocHook, Allocator, BoxedEngine, EngineBox, EngineKind, EngineState, try_box,
};

/// The possible inflate modes maintained between `inflate()` calls.
///
/// This is the safe Rust equivalent of the C `inflate_mode` enum (`inflate.h`).
/// The discriminants begin at the distinctive sentinel value `16180` (`HEAD`)
/// and increment sequentially; the C code uses that non-zero base as a cheap
/// "is this a validly initialized state?" check in `inflateStateCheck`. The
/// exact numeric values are preserved so that any debugging tooling or memory
/// dump matches reference zlib, and so the state occupies the same conceptual
/// range (`16180..=16211`).
///
/// The C `switch`/`goto` fall-through dispatch over these modes is reshaped in
/// the driver into a loop-plus-`match`; Rust's exhaustiveness checking then
/// guarantees no mode is silently unhandled (AAP §0.6.1).
///
/// # State transitions (from `inflate.h`)
///
/// ```text
/// Header:  HEAD -> (gzip) FLAGS..HCRC -> TYPE
///               -> (zlib) DICTID -> DICT -> TYPE  |  TYPE
///               -> (raw)  TYPEDO
/// Blocks:  TYPE -> TYPEDO -> STORED | TABLE | LEN_ | CHECK
///          STORED -> COPY_ -> COPY -> TYPE
///          TABLE  -> LENLENS -> CODELENS -> LEN_ -> LEN
/// Codes:   LEN -> LENEXT | LIT | TYPE ;  LENEXT -> DIST -> DISTEXT -> MATCH -> LEN
/// Trailer: CHECK -> LENGTH -> DONE
/// ```
///
/// (Most modes may additionally transition to [`Bad`](InflateMode::Bad) or
/// [`Mem`](InflateMode::Mem) on error.)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u16)] // values start at 16180; u16 suffices (max SYNC == 16180 + 31 == 16211)
pub enum InflateMode {
    /// i: waiting for the magic header (zlib CMF+FLG or gzip magic). The
    /// sentinel start value (`16180`) of the C `inflate_mode` enum.
    Head = 16180,
    /// i: waiting for method and flags (gzip).
    Flags,
    /// i: waiting for the modification time (gzip).
    Time,
    /// i: waiting for the extra flags and operating system (gzip).
    Os,
    /// i: waiting for the extra-field length (gzip).
    ExLen,
    /// i: waiting for the extra-field bytes (gzip).
    Extra,
    /// i: waiting for the end of the file name (gzip).
    Name,
    /// i: waiting for the end of the comment (gzip).
    Comment,
    /// i: waiting for the header CRC (gzip).
    Hcrc,
    /// i: waiting for the dictionary check value.
    DictId,
    /// Waiting for an `inflateSetDictionary()` call.
    Dict,
    /// i: waiting for type bits, including the last-block flag bit.
    Type,
    /// i: same as [`Type`](InflateMode::Type), but skips the check so
    /// `inflate()` can exit on a new block boundary.
    TypeDo,
    /// i: waiting for the stored-block size (length and its one's complement).
    Stored,
    /// i/o: same as [`Copy`](InflateMode::Copy) below, but only the first time
    /// in. Corresponds to the C mode `COPY_` (the trailing underscore denotes
    /// the "first time in" variant).
    CopyUnderscore,
    /// i/o: waiting for input or output space to copy a stored block.
    Copy,
    /// i: waiting for the dynamic-block table lengths.
    Table,
    /// i: waiting for the code-length code lengths.
    LenLens,
    /// i: waiting for the length/literal and distance code lengths.
    CodeLens,
    /// i: same as [`Len`](InflateMode::Len) below, but only the first time in.
    /// Corresponds to the C mode `LEN_` (the trailing underscore denotes the
    /// "first time in" variant).
    LenUnderscore,
    /// i: waiting for a length/literal/end-of-block code.
    Len,
    /// i: waiting for the length extra bits.
    LenExt,
    /// i: waiting for a distance code.
    Dist,
    /// i: waiting for the distance extra bits.
    DistExt,
    /// o: waiting for output space to copy a matched string.
    Match,
    /// o: waiting for output space to write a literal.
    Lit,
    /// i: waiting for the 32-bit check value.
    Check,
    /// i: waiting for the 32-bit length (gzip trailer).
    Length,
    /// Finished the trailing check — done; remains here until reset.
    Done,
    /// Got a data error — remains here until reset.
    Bad,
    /// Got an `inflate()` memory error — remains here until reset.
    Mem,
    /// Looking for synchronization bytes to restart `inflate()`.
    Sync,
}

impl InflateMode {
    /// Whether this mode is one C's `inflateStateCheck` accepts.
    ///
    /// C stores the mode in a plain `inflate_mode` field reached through an
    /// opaque `internal_state *`, so a transplanted or uninitialized struct can
    /// present garbage. It therefore range-checks the field (`inflate.c`
    /// L88-L97):
    ///
    /// ```text
    /// state->mode < HEAD || state->mode > SYNC
    /// ```
    ///
    /// `HEAD` and `SYNC` are the first and last of the 32 modes, so C's test
    /// admits exactly the declared set. Here the field is this typed enum with
    /// those same 32 variants, so an out-of-range value is unrepresentable and
    /// the predicate holds for every variant. It is written as an exhaustive
    /// `match` rather than a bare `true` so that C's clause appears where a
    /// reader of the FFI state check expects it, and so that adding a
    /// Rust-specific mode outside C's `HEAD..=SYNC` span cannot silently widen
    /// the set of streams the C ABI admits — the exhaustiveness check would
    /// force it to be classified here.
    #[must_use]
    pub(crate) const fn is_c_valid(self) -> bool {
        match self {
            InflateMode::Head
            | InflateMode::Flags
            | InflateMode::Time
            | InflateMode::Os
            | InflateMode::ExLen
            | InflateMode::Extra
            | InflateMode::Name
            | InflateMode::Comment
            | InflateMode::Hcrc
            | InflateMode::DictId
            | InflateMode::Dict
            | InflateMode::Type
            | InflateMode::TypeDo
            | InflateMode::Stored
            | InflateMode::CopyUnderscore
            | InflateMode::Copy
            | InflateMode::Table
            | InflateMode::LenLens
            | InflateMode::CodeLens
            | InflateMode::LenUnderscore
            | InflateMode::Len
            | InflateMode::LenExt
            | InflateMode::Dist
            | InflateMode::DistExt
            | InflateMode::Match
            | InflateMode::Lit
            | InflateMode::Check
            | InflateMode::Length
            | InflateMode::Done
            | InflateMode::Bad
            | InflateMode::Mem
            | InflateMode::Sync => true,
        }
    }

    /// Whether this mode is one of the gzip/zlib **header** parser states — the
    /// span in which `inflate` may store bytes into a caller-registered
    /// `gz_header`, and in which it can never produce output.
    ///
    /// C's header states run `HEAD` through `HCRC` (`inflate.h`): `HEAD` reads the
    /// magic, `FLAGS`/`TIME`/`OS`/`EXLEN` the fixed gzip fields, `EXTRA`/`NAME`/
    /// `COMMENT` the three variable-length payloads, and `HCRC` the optional header
    /// CRC. Every store into `head->extra`/`name`/`comment` happens inside that
    /// span (`inflate.c` L614-L621, L639-L642, L661-L664), and none of those states
    /// writes a decompressed byte.
    ///
    /// `DICTID`/`DICT` are excluded even though they precede the first block: they
    /// belong to zlib's preset-dictionary handshake, store nothing into a
    /// `gz_header`, and `DICT` has its own early return.
    ///
    /// The FFI boundary uses this to bound the header phase of a call whose header
    /// buffers overlap the caller's input or output window, where the two phases
    /// must be separated in time rather than merely bounds-checked. It is an
    /// exhaustive `match` so that a newly added mode must be classified here
    /// instead of silently defaulting to "not a header state".
    ///
    /// Gated on `gzip` because that overlap can only arise for a registered
    /// `gz_header`, so the sole caller — `inflate_split_over_header` in
    /// `src/ffi/inflate.rs` — carries the same gate. Without the gate this method
    /// is dead code in every `gzip`-off configuration, which the `--no-default-
    /// features`, `no-std` and `std,simd` rows each report as a `dead_code`
    /// warning; matching the caller's gate removes the warning at its cause
    /// instead of silencing it.
    #[cfg(feature = "gzip")]
    #[must_use]
    pub(crate) const fn is_header_phase(self) -> bool {
        match self {
            InflateMode::Head
            | InflateMode::Flags
            | InflateMode::Time
            | InflateMode::Os
            | InflateMode::ExLen
            | InflateMode::Extra
            | InflateMode::Name
            | InflateMode::Comment
            | InflateMode::Hcrc => true,
            InflateMode::DictId
            | InflateMode::Dict
            | InflateMode::Type
            | InflateMode::TypeDo
            | InflateMode::Stored
            | InflateMode::CopyUnderscore
            | InflateMode::Copy
            | InflateMode::Table
            | InflateMode::LenLens
            | InflateMode::CodeLens
            | InflateMode::LenUnderscore
            | InflateMode::Len
            | InflateMode::LenExt
            | InflateMode::Dist
            | InflateMode::DistExt
            | InflateMode::Match
            | InflateMode::Lit
            | InflateMode::Check
            | InflateMode::Length
            | InflateMode::Done
            | InflateMode::Bad
            | InflateMode::Mem
            | InflateMode::Sync => false,
        }
    }
}

impl Default for InflateMode {
    /// The mode a freshly initialized or reset inflate state starts in.
    ///
    /// Reference zlib sets `state->mode = HEAD` in `inflateResetKeep`; this
    /// mirrors that reset value.
    #[inline]
    fn default() -> Self {
        InflateMode::Head
    }
}

/// Identifies which table arena the active decode table lives in.
///
/// The C `struct inflate_state` stores `lencode` / `distcode` as
/// `code const FAR *` pointers. Those pointers may reference **either**:
///
/// * the state's own [`codes`](InflateState::codes) arena — for the Huffman
///   tables built from a *dynamic* DEFLATE block; or
/// * the module-static *fixed* tables (`inffixed.h` → `crate::inflate::fixed`'s
///   `LENFIX` / `DISTFIX`) — used for a *fixed* DEFLATE block.
///
/// Reference zlib distinguishes the two cases purely by pointer value
/// (`inflateCopy` checks whether the pointer falls inside `codes[]`). A
/// self-referential borrow cannot be held safely in Rust, so this discriminator
/// records the choice explicitly. It is paired with the `usize` offsets
/// [`InflateState::lencode`] / [`InflateState::distcode`], which are meaningful
/// only in the [`Dynamic`](TableSource::Dynamic) case, and it is resolved to a
/// concrete slice by [`InflateState::lencode_slice`] /
/// [`InflateState::distcode_slice`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum TableSource {
    /// The active table is one of the module-static fixed tables
    /// (`crate::inflate::fixed::LENFIX` / `DISTFIX`).
    Fixed,
    /// The active table lives in [`InflateState::codes`], starting at the
    /// matching offset ([`InflateState::lencode`] or
    /// [`InflateState::distcode`]).
    ///
    /// This is the default because a freshly reset state points
    /// `lencode`/`distcode` at the start of its own `codes[]` arena, exactly as
    /// C's `inflateResetKeep` does (`lencode = distcode = next = codes`).
    #[default]
    Dynamic,
}

/// State maintained between `inflate()` calls.
///
/// This is the safe Rust port of the C `struct inflate_state` (`inflate.h`).
/// It occupies roughly 7 KB (dominated by the [`codes`](InflateState::codes)
/// decode-table arena and the [`lens`](InflateState::lens) /
/// [`work`](InflateState::work) scratch arrays), not counting the sliding
/// [`window`](InflateState::window), which is allocated on demand and can grow
/// to 32 KB.
///
/// Because the struct is large, it is normally kept behind a [`Box`] (see
/// [`InflateState::new`]); the owning [`crate::stream::ZStream`] holds it as the
/// boxed `EngineState` in its engine slot, and the typed accessors that
/// recover it are the `InflateStream` extension methods defined below.
///
/// # Ownership and cleanup
///
/// All buffers are owned: the [`window`](InflateState::window) is an
/// [`AllocBuffer<u8>`](crate::stream::AllocBuffer) — so a caller-installed
/// `zalloc`/`zfree` pair backs it when one is active, and the global allocator
/// does otherwise — and the decode tables are inline arrays. No manual teardown
/// is required: dropping the `Box<InflateState>` frees the window
/// automatically, which is precisely how Rust ownership subsumes the
/// `ZFREE(state->window)` that C `inflateEnd` performs (AAP §0.3.2). No explicit
/// [`Drop`] implementation is needed.
///
/// # Field width notes
///
/// * [`hold`](InflateState::hold) / [`bits`](InflateState::bits) are [`u32`]
///   even though C declares `hold` as `unsigned long`. The inflate algorithm
///   never needs more than 32 accumulated bits (`NEEDBITS(32)` is the maximum,
///   reached only for the stored-block length/complement), and the final
///   `PULLBYTE` before that limit shifts by at most 24, so a [`u32`]
///   accumulator holds every value the algorithm produces without truncation.
///   All `NEEDBITS`/`DROPBITS` arithmetic in `mod.rs` / `back.rs` must therefore
///   also use [`u32`].
/// * [`total`](InflateState::total) is deliberately widened to [`u64`]. C
///   declares it `unsigned long total; /* protected copy of output count */`
///   (`inflate.h` L93), whose width is platform-dependent — 32 bits on Windows
///   LLP64, 64 bits on LP64 Unix. Fixing it at [`u64`] makes the counter behave
///   identically on every target, and it costs nothing observably because the
///   only place the value is compared against the wire is the gzip `ISIZE`
///   check, which masks it first (C `hold != (state->total & 0xffffffff)`,
///   `inflate.c` L1100) — a mask this port reproduces.
/// * [`back`](InflateState::back) is signed ([`i32`]) because it holds the
///   sentinel `-1` ("no unprocessed length/literal code yet").
pub struct InflateState {
    // --- state / framing ---------------------------------------------------
    /// Current inflate mode (the C `mode` field driving the state machine).
    pub mode: InflateMode,
    /// `true` while processing the last block of the stream (C `int last`).
    pub last: bool,
    /// Wrapper selector: bit 0 = expect a zlib header/trailer, bit 1 = expect a
    /// gzip header/trailer, bit 2 = validate the check value (C `int wrap`).
    pub wrap: i32,
    /// `true` once a preset dictionary has been supplied (C `int havedict`).
    pub havedict: bool,
    /// gzip header method and flags: `0` for a zlib stream, or `-1` for a raw
    /// stream / before any header has been seen (C `int flags`).
    pub flags: i32,
    /// zlib-header maximum back-reference distance, used by the `INFLATE_STRICT`
    /// distance check (C `unsigned dmax`). Defaults to `32768` (`1 << 15`).
    pub dmax: u32,
    /// Protected copy of the running check value (Adler-32 for zlib, CRC-32 for
    /// gzip). Kept separate from the stream's `adler` field (C
    /// `unsigned long check`).
    pub check: u32,
    /// Protected copy of the total number of output bytes produced (C
    /// `unsigned long total`; widened to [`u64`] here).
    pub total: u64,
    /// Optional gzip header being filled in when the caller requested one via
    /// `inflateGetHeader` (C `gz_headerp head`; [`None`] mirrors `Z_NULL`).
    ///
    /// On the C ABI path this carries only the header's *scalars*; the
    /// `extra`/`name`/`comment` payloads are written straight into the caller's
    /// own buffers through a per-call
    /// [`ForeignGzHeaderSink`](crate::gz_header::ForeignGzHeaderSink) instead of
    /// being accumulated here. See the `head_foreign` field, which is present
    /// only when the `gzip` feature is enabled.
    pub head: Option<GzHeader>,
    /// Whether the registered header's payload buffers live in *caller* memory,
    /// as they do for every `inflateGetHeader` call made through the C ABI.
    ///
    /// C has no counterpart because it only ever has this case: `state->head`
    /// *is* the caller's struct. The flag distinguishes it from the idiomatic
    /// Rust API, which lends the decoder an owned [`GzHeader`] to fill. When set,
    /// the `EXTRA`/`NAME`/`COMMENT` states store through the lent sink — which
    /// re-reads the caller's live pointers and capacities on every call and
    /// allocates nothing — rather than growing owned vectors.
    ///
    /// Cleared by [`reset_keep`](Self::reset_keep) alongside `head`, mirroring
    /// C's `state->head = Z_NULL` (`inflate.c` L115).
    #[cfg(feature = "gzip")]
    pub head_foreign: bool,

    // --- sliding window ----------------------------------------------------
    /// Base-2 logarithm of the requested window size (C `unsigned wbits`).
    pub wbits: u32,
    /// Window size in bytes, or `0` when the window is not yet in use (C
    /// `unsigned wsize`). The `0` sentinel mirrors C's "window not initialized"
    /// state.
    pub wsize: u32,
    /// Number of valid bytes currently stored in the window (C
    /// `unsigned whave`).
    pub whave: u32,
    /// Write index (next position to write) within the circular window (C
    /// `unsigned wnext`).
    pub wnext: u32,
    /// Owned sliding window, allocated on demand.
    ///
    /// Starts empty ([`AllocBuffer::default`]) and is sized to `1 << wbits` on
    /// first use by the driver's `updatewindow`, mirroring C's lazy `ZALLOC` of
    /// `state->window`. When a caller installed `zalloc`/`zfree` through the FFI
    /// `z_stream`, the window is routed through those hooks via the
    /// [`alloc_hook`](InflateState::alloc_hook) stored at construction time
    /// (AAP §0.6.3 has-hook clause); otherwise it is a plain global allocation.
    /// Being owned, it is freed automatically on drop — through the caller's
    /// `zfree` for a hook-backed buffer, or the global allocator otherwise —
    /// subsuming C's `ZFREE(state->window)`.
    pub window: AllocBuffer<u8>,

    // --- bit accumulator ---------------------------------------------------
    /// Input bit accumulator; only the low 32 bits are ever used (C
    /// `unsigned long hold`, narrowed to [`u32`] — see the type note on
    /// [`InflateState`]).
    pub hold: u32,
    /// Number of valid bits currently held in [`hold`](InflateState::hold) (C
    /// `unsigned bits`).
    pub bits: u32,

    /// Whole input bytes the fast path handed back that lie *behind* the input
    /// slice it was given — the part of C's `in -= (bits >> 3)` (`inffast.c`
    /// L291) that an index into a slice cannot express.
    ///
    /// C rewinds a raw pointer, so when it enters `inflate_fast` with whole bytes
    /// already buffered (which `inflatePrime` alone guarantees) it may move
    /// `strm->next_in` to before where the call started, un-consuming bytes an
    /// earlier call took. It never dereferences them, so this is sound in C and
    /// observable only through `next_in`, `avail_in`, `total_in` and `data_type`.
    ///
    /// Whether those bytes exist is a property of the *caller's* memory, not of
    /// the decoder: a C caller owns one contiguous buffer and can honour the
    /// rewind, while a caller handing over an independent `&[u8]` per call cannot.
    /// The fast path therefore records the debt here instead of acting on it, and
    /// the corresponding bits stay in [`Self::hold`] until something settles it.
    /// The crate-internal `inflate_take_input_history_rewind` is that settlement,
    /// used by the C ABI boundary.
    ///
    /// Purely per-call: zeroed on entry to every decode call, so it is never read
    /// stale.
    pub rewound: u32,

    // --- string / stored-block copy ---------------------------------------
    /// Literal byte value, or the length of data still to copy (C
    /// `unsigned length`).
    pub length: u32,
    /// Distance back into the window from which to copy a matched string (C
    /// `unsigned offset`).
    pub offset: u32,

    // --- table / code decoding --------------------------------------------
    /// Number of extra bits still needed for the current length or distance
    /// code (C `unsigned extra`).
    pub extra: u32,
    /// Offset into [`codes`](InflateState::codes) of the starting
    /// literal/length decode table, valid when
    /// [`lentable`](InflateState::lentable) is [`TableSource::Dynamic`] (C
    /// `code const FAR *lencode`, represented as a `usize` offset).
    pub lencode: usize,
    /// Offset into [`codes`](InflateState::codes) of the starting distance
    /// decode table, valid when [`disttable`](InflateState::disttable) is
    /// [`TableSource::Dynamic`] (C `code const FAR *distcode`, represented as a
    /// `usize` offset).
    pub distcode: usize,
    /// Which arena the active literal/length table lives in (fixed vs dynamic).
    /// See [`TableSource`] and [`InflateState::lencode_slice`].
    pub lentable: TableSource,
    /// Which arena the active distance table lives in (fixed vs dynamic). See
    /// [`TableSource`] and [`InflateState::distcode_slice`].
    pub disttable: TableSource,
    /// Root-table index bits for the literal/length table (C
    /// `unsigned lenbits`).
    pub lenbits: u32,
    /// Root-table index bits for the distance table (C `unsigned distbits`).
    pub distbits: u32,

    // --- dynamic table building -------------------------------------------
    /// Number of code-length code lengths (C `unsigned ncode`).
    pub ncode: u32,
    /// Number of literal/length code lengths (C `unsigned nlen`).
    pub nlen: u32,
    /// Number of distance code lengths (C `unsigned ndist`).
    pub ndist: u32,
    /// Number of code lengths already read into [`lens`](InflateState::lens) (C
    /// `unsigned have`).
    pub have: u32,
    /// Next available slot in [`codes`](InflateState::codes) while building
    /// tables (C `code FAR *next`, represented as a `usize` offset). This is the
    /// cursor advanced by `crate::inflate::tables::inflate_table`.
    pub next: usize,
    /// Temporary storage for code lengths (C `unsigned short lens[320]`).
    pub lens: [u16; 320],
    /// Scratch work area used while building the decode tables (C
    /// `unsigned short work[288]`).
    pub work: [u16; 288],

    // --- decode-table arena ------------------------------------------------
    /// Space for the dynamically built literal/length and distance decode
    /// tables (C `code codes[ENOUGH]`).
    ///
    /// Sized to [`ENOUGH`] (`1444` = `ENOUGH_LENS` `852` + `ENOUGH_DISTS`
    /// `592`), exactly matching the reference C footprint and the table-overflow
    /// guard in `inflate_table`.
    pub codes: [Code; ENOUGH],

    // --- error recovery / validation --------------------------------------
    /// When `false`, permit an otherwise-invalid "distance too far back"
    /// reference (used by `inflateSync` recovery). `true` in normal operation
    /// (C `int sane`).
    pub sane: bool,
    /// Number of bits back of the last unprocessed length/literal code; `-1`
    /// when there is none. Must stay signed (C `int back`).
    pub back: i32,
    /// Initial length of the match currently being processed (C
    /// `unsigned was`).
    pub was: u32,

    /// Allocator hook threaded from the owning stream at construction time.
    ///
    /// The C `struct inflate_state` has no such field because the manual
    /// `ZALLOC`/`ZFREE` calls read `strm->zalloc`/`strm->zfree` directly. Here
    /// the state owns its [`window`](InflateState::window) rather than holding a
    /// back-pointer to the stream, so the hook is captured once (by
    /// [`new_in`](InflateState::new_in)) and consulted whenever the window is
    /// lazily (re)allocated. It is [`AllocHook::none`] under the Rust global
    /// allocator, or the caller's `zalloc`/`zfree`/`opaque` when one was
    /// installed through the FFI `z_stream` (AAP §0.6.3 has-hook clause).
    pub alloc_hook: AllocHook,
}

// ===========================================================================
// C layout mirror — the authoritative `sizeof(struct inflate_state)` for
// allocator accounting (AAP §0.6.3, §0.6.5)
// ===========================================================================

/// A field-exact `#[repr(C)]` mirror of C's `struct inflate_state`
/// (`inflate.h` L82-L125), used for **one** purpose: to compute the byte count
/// reference zlib passes to a caller's `zalloc` in
/// `ZALLOC(strm, 1, sizeof(struct inflate_state))` (`inflate.c` L198).
///
/// `size_of::<InflateState>()` is *not* that number — it is 7272 against C's
/// 7160 on LP64 — because the idiomatic state holds an owning
/// [`AllocBuffer`] where C holds a bare `unsigned char *`, a
/// [`TableSource`] discriminant plus offsets where C holds three interior
/// `code *` pointers, and Rust enums where C holds `int`s. The two numbers
/// therefore cannot be one value, and the request has to carry **C's**: an
/// allocator sized from C's header must serve this port exactly as it serves
/// reference zlib (AAP §0.6.5). This mirror is what makes that number available,
/// and [`InflateState::C_LAYOUT_SIZE`] is what the init and copy paths charge. The
/// consequence is that the caller's region cannot *host* the larger Rust state, so
/// it is held purely as the accounting token C's `zfree` receives back, and the
/// state value sits beside it on the global heap.
///
/// Every field is a `core::ffi` scalar alias or a raw pointer and the struct is
/// `#[repr(C)]`, so rustc applies the platform C ABI's layout rules — the same
/// ones the C compiler applies — making the size correct on LP64, on Windows
/// LLP64 (where `c_ulong` is 32-bit), and on 32-bit targets without a per-target
/// table. The tests pin the exact LP64 offsets `gcc` reports for the in-tree
/// `inflate.h`, field by field.
///
/// Nothing constructs this type and nothing reads its fields outside that test.
#[allow(dead_code)]
#[repr(C)]
struct InflateStateC {
    strm: *mut c_void,
    /// C's `inflate_mode` enumeration, which the compiler represents as an `int`.
    mode: c_int,
    last: c_int,
    wrap: c_int,
    havedict: c_int,
    flags: c_int,
    dmax: c_uint,
    check: c_ulong,
    total: c_ulong,
    head: *mut c_void,
    wbits: c_uint,
    wsize: c_uint,
    whave: c_uint,
    wnext: c_uint,
    window: *mut c_uchar,
    hold: c_ulong,
    bits: c_uint,
    length: c_uint,
    offset: c_uint,
    extra: c_uint,
    lencode: *const CodeC,
    distcode: *const CodeC,
    lenbits: c_uint,
    distbits: c_uint,
    ncode: c_uint,
    nlen: c_uint,
    ndist: c_uint,
    have: c_uint,
    next: *mut CodeC,
    lens: [c_ushort; 320],
    work: [c_ushort; 288],
    codes: [CodeC; ENOUGH],
    sane: c_int,
    back: c_int,
    was: c_uint,
}

/// Layout mirror of C's `code` struct (`inftrees.h` L24-L28): two `unsigned
/// char`s followed by an `unsigned short`, four bytes in total.
#[allow(dead_code)]
#[repr(C)]
#[derive(Copy, Clone)]
struct CodeC {
    op: c_uchar,
    bits: c_uchar,
    val: c_ushort,
}

/// Default `dmax` value: the maximum back-reference distance for a 32 KiB
/// window (`1 << 15`). Reference zlib initializes `state->dmax = 32768U` in
/// `inflateResetKeep`.
const DMAX_DEFAULT: u32 = 32768;

// ===========================================================================
// Stream integration — the typed view of a `ZStream`'s decompression engine
// ===========================================================================

/// Lets a [`ZStream`](crate::stream::ZStream) own an `InflateState` without
/// naming it.
///
/// `crate::stream` is layer 5 and this module is layer 6, so the naming has to
/// run in this direction: the stream stores `Box<dyn EngineState>` and this impl
/// is what makes an `InflateState` installable (AAP §0.3.1, §0.4.2 B2). The
/// [`Any`] accessors are the MSRV-1.85 spelling of
/// `&dyn EngineState -> &dyn Any` upcasting, which only became available in
/// Rust 1.86.
/// The `size` argument reference zlib passes when it charges a caller's `zalloc`
/// for this engine's state: C's `sizeof(struct inflate_state)` (`inflate.c` L198,
/// `infback.c` L51), not this port's own `size_of::<InflateState>()`.
///
/// Forwarding the `#[repr(C)]` mirror's size is what makes an allocator sized from
/// C's header — one that serves `(1, 7160)` and refuses anything larger —
/// initialize here exactly as it does against reference zlib (AAP §0.6.5). See
/// [`C_LAYOUT_SIZE`](InflateState::C_LAYOUT_SIZE).
impl crate::stream::EngineFootprint for InflateState {
    const C_STATE_SIZE: usize = Self::C_LAYOUT_SIZE;
}

impl EngineState for InflateState {
    #[inline]
    fn engine_kind(&self) -> EngineKind {
        EngineKind::Inflate
    }

    #[inline]
    fn as_any(&self) -> &dyn Any {
        self
    }

    #[inline]
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    #[inline]
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// The typed, layer-6 view of a stream's installed decompression engine.
///
/// These are the spellings the inflate driver and the FFI shims use
/// (`strm.inflate_state_mut()`, `strm.set_inflate_state(state)`); they forward to
/// the engine-agnostic generic accessors on
/// [`ZStream`](crate::stream::ZStream). Keeping them here rather than on
/// `ZStream` itself is what removes the `stream -> inflate` upward edge while
/// leaving every call site unchanged: the concrete engine type is named only in
/// the layer that defines it.
///
/// Bring the trait into scope with
/// `use crate::inflate::state::InflateStream;` to use the methods.
pub(crate) trait InflateStream {
    /// The allocator this stream was built with.
    type Alloc: Allocator;

    /// Borrows the installed decompression engine, or [`None`] if the stream
    /// holds no state or a compression engine.
    fn inflate_state(&self) -> Option<&InflateState>;

    /// Mutably borrows the installed decompression engine, or [`None`]. The hot
    /// path for `inflate()`.
    fn inflate_state_mut(&mut self) -> Option<&mut InflateState>;

    /// Installs a decompression engine, replacing (and thereby freeing) any
    /// engine previously held — the RAII replacement for the C `inflateEnd` that
    /// would otherwise be required first.
    ///
    /// The engine arrives already placed — on the Rust heap, or in the region a
    /// caller's `zalloc` handed back — so installing it allocates nothing.
    fn set_inflate_state(&mut self, state: BoxedEngine<InflateState>);

    /// Mutably borrows the installed decompression engine **together with** a
    /// shared borrow of the stream's allocator, or [`None`].
    ///
    /// Exists so paths that mutate the decoder *and* need to allocate — the lazy
    /// window allocation in `updatewindow`, reached from `inflate` and
    /// `inflateSetDictionary` — can route their allocation through the
    /// [`Allocator`] trait rather than bypassing it (AAP §0.6.3).
    fn inflate_state_and_allocator(&mut self) -> Option<(&mut InflateState, &Self::Alloc)>;

    /// Removes and returns the installed decompression engine.
    ///
    /// Returns [`None`] — **leaving any installed engine in place** — when the
    /// stream holds no engine or holds a *compression* engine. That is exactly
    /// C `inflateStateCheck` rejecting a stream it must not touch (`inflate.c`
    /// L88-L97): the caller's `z_stream` is left as it was found.
    ///
    /// `inflate()` uses this to decouple the borrow of the engine from the borrow
    /// of the surrounding stream fields (`msg`, `total_in`, `adler`), then puts
    /// the engine back on every exit path.
    fn take_inflate_state(&mut self) -> Option<BoxedEngine<InflateState>>;
}

impl<A: Allocator> InflateStream for crate::stream::ZStream<A> {
    type Alloc = A;

    #[inline]
    fn inflate_state(&self) -> Option<&InflateState> {
        self.engine_state::<InflateState>()
    }

    #[inline]
    fn inflate_state_mut(&mut self) -> Option<&mut InflateState> {
        self.engine_state_mut::<InflateState>()
    }

    #[inline]
    fn set_inflate_state(&mut self, state: BoxedEngine<InflateState>) {
        self.set_engine_state(state);
    }

    #[inline]
    fn inflate_state_and_allocator(&mut self) -> Option<(&mut InflateState, &A)> {
        self.engine_state_and_allocator::<InflateState>()
    }

    #[inline]
    fn take_inflate_state(&mut self) -> Option<BoxedEngine<InflateState>> {
        // Look before leaping: a compression engine must be left exactly where it
        // is, so only remove the engine once it is known to be a decompressor.
        if self.engine_kind() != Some(EngineKind::Inflate) {
            return None;
        }
        let engine = self.take_engine_state()?;
        // The kind check above already established the concrete type, so the
        // downcast cannot fail; `ok()` keeps the path total rather than panicking.
        // The owning view stays the placement wrapper — a state that lives in a
        // caller's region cannot be moved out of it without allocating elsewhere —
        // so this hands back the wrapper itself and allocates nothing.
        engine.into_any().downcast::<EngineBox<InflateState>>().ok()
    }
}

impl InflateState {
    /// The byte count reference zlib passes to a caller's `zalloc` when it
    /// allocates the inflate state — C's `sizeof(struct inflate_state)` —
    /// computed from the field-exact `#[repr(C)]` layout mirror rather than from
    /// this Rust type's own size.
    ///
    /// This is the value C uses in
    /// `ZALLOC(strm, 1, sizeof(struct inflate_state))` (`inflate.c` L198). It is
    /// **7160 on LP64**, whereas `size_of::<InflateState>()` is legitimately
    /// larger — this port's state carries the same information in Rust-native
    /// shapes (owned buffers, `Option`s, offsets) and does not have to be
    /// byte-compatible, only behaviourally so.
    ///
    /// **This is exactly the size the init paths request**, forwarded to
    /// the crate-internal `EngineFootprint::C_STATE_SIZE`
    /// so that a caller's `zalloc` sees C's `(1, sizeof(struct inflate_state))`
    /// pair and an arena sized from C's header serves it exactly as it serves
    /// reference zlib (AAP §0.6.5). Because a block of C's smaller `sizeof` cannot
    /// hold this port's state, the charge and the state value are deliberately
    /// separate: the caller's region is held — unread — until its matching
    /// `zfree`, and the state itself lives beside it on the global heap. Every
    /// property a zlib caller can observe (request count, argument pair, sequence
    /// position, failure timing, release order) is preserved.
    ///
    /// Exposed publicly because it is the ABI mirror's own size, and the only way
    /// an allocator implementation or a memory-accounting test can state what
    /// reference zlib asks for.
    pub const C_LAYOUT_SIZE: usize = core::mem::size_of::<InflateStateC>();

    /// Creates a new, boxed inflate state for the given wrapper mode and window
    /// size.
    ///
    /// `wrap` is the already-decoded wrapper selector (bit 0 = zlib, bit 1 =
    /// gzip, bit 2 = validate check value) and `wbits` is the base-2 logarithm
    /// of the window size. Both are computed by
    /// [`crate::inflate::inflate_reset2`], which decodes the caller's overloaded
    /// `windowBits` argument itself — `wrap = (windowBits >> 4) + 5` for
    /// non-negative values, `wrap = 0` with `wbits = -windowBits` for raw
    /// streams — exactly as C `inflateReset2` does (`inflate.c`), and which
    /// accepts `wbits == 0` to mean "take the window size from the zlib header".
    /// The deflate side has its own decoder,
    /// [`crate::constants::parse_window_bits`]; the two are intentionally
    /// separate because the accepted domains differ (deflate rejects `0` and
    /// applies the special 8-bit-window rule). The returned state reproduces the
    /// field values reference zlib leaves after
    /// `inflateInit2_` + `inflateReset2` + `inflateResetKeep`:
    ///
    /// * [`mode`](InflateState::mode) = [`InflateMode::Head`],
    /// * [`flags`](InflateState::flags) = `-1` (no header seen yet),
    /// * [`dmax`](InflateState::dmax) = `32768`,
    /// * [`sane`](InflateState::sane) = `true`, [`back`](InflateState::back) =
    ///   `-1`,
    /// * the decode-table cursors ([`lencode`](InflateState::lencode),
    ///   [`distcode`](InflateState::distcode), [`next`](InflateState::next)) at
    ///   offset `0` with [`TableSource::Dynamic`] tables,
    /// * and an **empty** [`window`](InflateState::window) — the allocation is
    ///   deferred until the driver's `updatewindow` first needs it, matching C's
    ///   lazy `ZALLOC`.
    ///
    /// The state is returned already boxed because it is large (~7 KB): keeping
    /// it behind a [`Box`] matches the placed engine the owning
    /// [`crate::stream::ZStream`] holds, and avoids moving the arrays around by
    /// value.
    ///
    /// # Allocator
    ///
    /// This convenience constructor records [`AllocHook::none`], so the window
    /// (when the driver later allocates it) uses the Rust global allocator —
    /// equivalent to a C caller with null `zalloc`/`zfree`.
    ///
    /// It is **not** the constructor the FFI uses. Every path that has to report
    /// an allocation failure rather than abort goes through
    /// [`try_new_in`](Self::try_new_in); see there for the complete list of
    /// callers and the exact allocation schedule each one performs.
    #[inline]
    #[must_use]
    pub fn new(wrap: i32, wbits: u32) -> Box<InflateState> {
        Self::new_in(AllocHook::none(), wrap, wbits)
    }

    /// Builds a boxed [`InflateState`], recording the supplied [`AllocHook`] so
    /// the lazily-allocated [`window`](InflateState::window) is routed through
    /// the caller's `zalloc`/`zfree` when active, or the global allocator
    /// otherwise. See [`new`](Self::new) for the full field-initialization
    /// contract; this is the same constructor with an explicit allocator hook
    /// (AAP §0.6.3 has-hook clause).
    ///
    /// The window is *not* allocated here — it is sized on demand by the
    /// driver's `updatewindow`, which consults the stored
    /// [`alloc_hook`](InflateState::alloc_hook) at that time. Nor is the state
    /// itself charged to `hook`: this spelling places it on the Rust heap. See
    /// [`try_new_in`](Self::try_new_in) for the paths that place it in the
    /// caller's own memory instead, and when.
    ///
    /// # Panics
    ///
    /// This spelling boxes the state with [`Box::new`], so it **aborts the
    /// process** if the Rust global heap cannot hold the roughly 7 KB state. It
    /// exists only for Rust-native callers that treat allocation failure as fatal;
    /// nothing that has to return a zlib error code may use it. Use
    /// [`try_new_in`](Self::try_new_in) instead.
    #[must_use]
    pub fn new_in(hook: AllocHook, wrap: i32, wbits: u32) -> Box<InflateState> {
        Box::new(Self::build(hook, wrap, wbits))
    }

    /// Fallible counterpart of [`new_in`](Self::new_in): boxes the state through
    /// a checked global allocation, yielding [`None`] instead of aborting when
    /// the heap cannot satisfy it.
    ///
    /// C `inflateInit2_` reports a failed state allocation as `Z_MEM_ERROR`
    /// (`inflate.c` L198-L200), so every init path must be able to surface it.
    /// The field-initialization contract is identical to [`new`](Self::new).
    ///
    /// # Placement
    ///
    /// This spelling always boxes on the Rust global heap, so it makes exactly
    /// **one** allocation and no caller-hook request. It is the Rust-native
    /// constructor.
    ///
    /// The paths that must reproduce C's caller-visible allocation schedule do
    /// **not** use it: they take a `crate::stream::EngineReservation` at C's
    /// position in the sequence — charging the caller's `zalloc` C's own
    /// `(1, `[`C_LAYOUT_SIZE`](Self::C_LAYOUT_SIZE)`)` pair, released again through
    /// their `zfree` after the window (AAP §0.6.5) — and build the state with
    /// `build_in`:
    ///
    /// | Caller | Caller-hook requests, in order |
    /// |--------|--------------------------------|
    /// | [`crate::inflate::inflate_init2`] (reached from FFI `inflateInit2_`) | one for the state itself; the window comes later, lazily, from `updatewindow` — C's schedule exactly (`inflate.c` L198, L261) |
    /// | `crate::inflate::back::inflate_back_init_borrowed_window` (reached from FFI `inflateBackInit_`) | one for the state and nothing else — the window is the ABI caller's own buffer, lent rather than allocated (`infback.c` L51, L59) |
    /// | [`crate::inflate::inflate_copy`] | one for the destination state (`inflate.c` L1340), then one for its window when the source had one (L1346) |
    ///
    /// Each of those paths charges the hook only when its
    /// [`Allocator::reserves_state_footprint`] says to, so the global-allocator
    /// path keeps its historical footprint: there the `Box` already *is* that
    /// allocation, and charging a second region would double every stream's fixed
    /// overhead (AAP §0.6.5).
    #[must_use]
    pub fn try_new_in(hook: AllocHook, wrap: i32, wbits: u32) -> Option<Box<InflateState>> {
        try_box(Self::build(hook, wrap, wbits))
    }

    /// Builds the initial state **unboxed**, for a caller that has already
    /// reserved its final home.
    ///
    /// This is the entry point the C-parity init paths use: they take a
    /// `crate::stream::EngineReservation` where C issues
    /// `ZALLOC(strm, 1, sizeof(struct inflate_state))`, call this to produce the
    /// value, and then attach the charge to the finished state. Splitting
    /// reservation from construction is unavoidable, because the reservation has
    /// to be charged *before* the state exists in order to fail where C fails.
    ///
    /// The field-initialization contract is identical to [`new`](Self::new).
    #[must_use]
    pub(crate) fn build_in(hook: AllocHook, wrap: i32, wbits: u32) -> InflateState {
        Self::build(hook, wrap, wbits)
    }

    /// Builds the unboxed initial state shared by [`new_in`](Self::new_in) and
    /// [`try_new_in`](Self::try_new_in), so the field-initialization contract
    /// exists in exactly one place.
    fn build(hook: AllocHook, wrap: i32, wbits: u32) -> InflateState {
        InflateState {
            // reset-managed fields (see `reset_keep`)
            mode: InflateMode::Head,
            last: false,
            havedict: false,
            flags: -1,
            dmax: DMAX_DEFAULT,
            total: 0,
            head: None,
            #[cfg(feature = "gzip")]
            head_foreign: false,
            hold: 0,
            bits: 0,
            rewound: 0,
            lencode: 0,
            distcode: 0,
            lentable: TableSource::Dynamic,
            disttable: TableSource::Dynamic,
            next: 0,
            sane: true,
            back: -1,
            // caller-provided framing / window geometry
            wrap,
            wbits,
            // kept / lazily-sized buffers and per-block scratch
            check: 0,
            wsize: 0,
            whave: 0,
            wnext: 0,
            window: AllocBuffer::default(),
            length: 0,
            offset: 0,
            extra: 0,
            lenbits: 0,
            distbits: 0,
            ncode: 0,
            nlen: 0,
            ndist: 0,
            have: 0,
            lens: [0; 320],
            work: [0; 288],
            codes: [Code::default(); ENOUGH],
            was: 0,
            alloc_hook: hook,
        }
    }

    /// Resets the per-stream decoding fields while **keeping** the sliding
    /// window allocation and its contents.
    ///
    /// This is the state-level portion of the C `inflateResetKeep` routine: it
    /// restores [`mode`](InflateState::mode) to [`InflateMode::Head`] and clears
    /// the block/bit-buffer/table state so decoding can restart, but it leaves
    /// [`wrap`](InflateState::wrap), [`wbits`](InflateState::wbits), and the
    /// [`window`](InflateState::window) (plus [`wsize`](InflateState::wsize) /
    /// [`whave`](InflateState::whave) / [`wnext`](InflateState::wnext))
    /// untouched. The stream-level counterparts (`total_in`/`total_out`, `msg`,
    /// `adler`) are reset by the driver in `mod.rs`, which calls this helper.
    pub fn reset_keep(&mut self) {
        self.total = 0;
        self.mode = InflateMode::Head;
        self.last = false;
        self.havedict = false;
        self.flags = -1;
        self.dmax = DMAX_DEFAULT;
        self.head = None;
        // C's `state->head = Z_NULL` (`inflate.c` L115) drops the registration
        // itself, so the ownership marker must go with it: a stale `true` here
        // would let a later call consult a sink the caller is entitled to have
        // freed the moment the reset returned.
        #[cfg(feature = "gzip")]
        {
            self.head_foreign = false;
        }
        self.hold = 0;
        self.bits = 0;
        // The fast path's un-honoured input debt is per-call, so a reset must
        // not carry one into the next decode.
        self.rewound = 0;
        // `lencode = distcode = next = codes` (offset 0, dynamic arena) in C.
        self.lencode = 0;
        self.distcode = 0;
        self.next = 0;
        self.lentable = TableSource::Dynamic;
        self.disttable = TableSource::Dynamic;
        self.sane = true;
        self.back = -1;
    }

    /// Resets the state and additionally invalidates the sliding window
    /// contents.
    ///
    /// This mirrors the state-level portion of the C `inflateReset` routine,
    /// which zeroes [`wsize`](InflateState::wsize),
    /// [`whave`](InflateState::whave), and [`wnext`](InflateState::wnext) — so
    /// the previously decoded data is no longer used as history — and then
    /// performs the [`reset_keep`](InflateState::reset_keep) reset. The window
    /// *allocation* is retained (only its logical contents are dropped),
    /// matching C, which keeps `state->window` allocated.
    pub fn reset(&mut self) {
        self.wsize = 0;
        self.whave = 0;
        self.wnext = 0;
        self.reset_keep();
    }

    /// Clears the input bit accumulator.
    ///
    /// Sets [`hold`](InflateState::hold) and [`bits`](InflateState::bits) back
    /// to zero, mirroring the C `INITBITS()` macro used by the driver after a
    /// value has been fully consumed from the bit buffer.
    #[inline]
    pub fn clear_bitbuffer(&mut self) {
        self.hold = 0;
        self.bits = 0;
    }

    /// Returns whether this state is in a valid inflate mode.
    ///
    /// This mirrors the mode-range portion of the C `inflateStateCheck`
    /// predicate, which treats a state as valid when
    /// `HEAD <= mode <= SYNC`. Because [`InflateMode`] is a closed enum that can
    /// only ever hold one of its defined variants, Rust's type system already
    /// guarantees the mode lies in `[Head, Sync]` — so this always returns
    /// `true`. It is retained for parity with the C API and as an explicit,
    /// self-documenting hook the driver (`mod.rs`) can call where the C code
    /// invoked `inflateStateCheck`.
    #[must_use]
    #[inline]
    pub fn is_valid(&self) -> bool {
        let mode = self.mode as u16;
        (InflateMode::Head as u16..=InflateMode::Sync as u16).contains(&mode)
    }

    /// Resolves the active literal/length decode table to a slice.
    ///
    /// Pass the module-static fixed literal/length table
    /// (`&crate::inflate::fixed::LENFIX[..]`) as `fixed`. It is returned
    /// verbatim when [`lentable`](InflateState::lentable) is
    /// [`TableSource::Fixed`]; otherwise the sub-slice of
    /// [`codes`](InflateState::codes) beginning at
    /// [`lencode`](InflateState::lencode) is returned.
    ///
    /// Taking `fixed` as an argument (rather than importing the fixed tables)
    /// keeps this data-model module free of a dependency on the fixed-table
    /// module and free of any `unsafe` self-referential borrow: the returned
    /// slice borrows from either `self` or `fixed`, both tied to the `'a`
    /// lifetime.
    ///
    /// # Preconditions
    ///
    /// When [`lentable`](InflateState::lentable) is [`TableSource::Dynamic`],
    /// [`lencode`](InflateState::lencode) must be a valid offset into
    /// [`codes`](InflateState::codes), i.e. `lencode <= codes.len()`
    /// (which is [`ENOUGH`]). Both fields are
    /// public, so this cannot be enforced by the type system; the invariant is
    /// maintained by the only writers — `reset_keep`, which sets the offset to
    /// `0`, and the driver's table-building step, which advances
    /// [`next`](InflateState::next) strictly within the `ENOUGH`-sized arena
    /// because [`crate::inflate::tables::inflate_table`] refuses to overflow it.
    /// This is the port of C's rule that `state->lencode` always points inside
    /// `state->codes[]`; the C version keeps a raw pointer and has no way to
    /// check it at all.
    ///
    /// # Panics
    ///
    /// Panics if the precondition is violated, because the [`TableSource::Dynamic`]
    /// arm is a checked slice range. That is the deliberate trade: a corrupted
    /// offset aborts here rather than handing the decoder a table built from
    /// out-of-bounds memory, which is what the equivalent C pointer would do.
    #[must_use]
    #[inline]
    pub fn lencode_slice<'a>(&'a self, fixed: &'a [Code]) -> &'a [Code] {
        match self.lentable {
            TableSource::Fixed => fixed,
            TableSource::Dynamic => &self.codes[self.lencode..],
        }
    }

    /// Resolves the active distance decode table to a slice.
    ///
    /// The distance counterpart of [`lencode_slice`](InflateState::lencode_slice):
    /// pass the module-static fixed distance table
    /// (`&crate::inflate::fixed::DISTFIX[..]`) as `fixed`. It is returned when
    /// [`disttable`](InflateState::disttable) is [`TableSource::Fixed`];
    /// otherwise the sub-slice of [`codes`](InflateState::codes) beginning at
    /// [`distcode`](InflateState::distcode) is returned.
    ///
    /// # Preconditions
    ///
    /// The same rule as [`lencode_slice`](InflateState::lencode_slice), applied
    /// to [`distcode`](InflateState::distcode): in [`TableSource::Dynamic`] mode
    /// it must satisfy `distcode <= codes.len()`.
    ///
    /// # Panics
    ///
    /// Panics if that precondition is violated — the [`TableSource::Dynamic`] arm
    /// is a checked slice range.
    #[must_use]
    #[inline]
    pub fn distcode_slice<'a>(&'a self, fixed: &'a [Code]) -> &'a [Code] {
        match self.disttable {
            TableSource::Fixed => fixed,
            TableSource::Dynamic => &self.codes[self.distcode..],
        }
    }
}

/// Releases the window in **C `inflateEnd`'s order**, ahead of the state itself.
///
/// Ownership alone already frees the window — that is what makes C's explicit
/// `ZFREE` unnecessary — but the caller's charge for the state footprint is
/// released by the crate-internal `EngineBox` wrapper that holds it, which runs
/// *after* this impl. Reference zlib's order is fixed by its source:
///
/// ```text
/// if (state->window != Z_NULL) ZFREE(strm, state->window);  /* inflate.c L1160 */
/// ZFREE(strm, strm->state);                                 /* inflate.c L1161 */
/// ```
///
/// so the window must reach `zfree` before the state does. Dropping the window
/// explicitly here — rather than letting the implicit field drops do it — is what
/// pins that, because field-declaration order is an incidental property of how
/// this struct happens to be written, and AAP §0.6.5 makes the allocator-visible
/// schedule a first-class parity requirement. This matches how
/// [`crate::deflate::DeflateState`] pins C `deflateEnd`'s five-step order.
///
/// The window is swapped out with [`core::mem::take`] and dropped immediately;
/// the replacement is an empty [`AllocBuffer`] whose own drop is a no-op, so the
/// implicit field drops that follow release nothing further.
///
/// For an `inflateBack` state the window is the ABI caller's own lent region,
/// whose `Drop` is deliberately empty, so nothing at all reaches `zfree` here —
/// the one free `inflateBackEnd` performs is of the state itself
/// (`infback.c` L572-L577).
impl Drop for InflateState {
    fn drop(&mut self) {
        drop(core::mem::take(&mut self.window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an owned window of `n` zero bytes as an [`AllocBuffer`] (the
    /// [`window`](InflateState::window) field type) using the global allocator,
    /// so the helper is valid in both `no_std` + `alloc` and `std` test builds
    /// and can be assigned directly to `state.window`.
    fn make_window(n: usize) -> AllocBuffer<u8> {
        // The null hook always yields an owned global-allocator buffer, so
        // `try_zeroed` never returns `None` here.
        AllocBuffer::try_zeroed(n, AllocHook::none()).expect("global allocation is infallible")
    }

    #[test]
    fn head_sentinel_value() {
        // The C `inflate_mode` enum starts at HEAD = 16180.
        assert_eq!(InflateMode::Head as u16, 16180);
    }

    #[test]
    fn sync_value_and_span() {
        // SYNC is the last mode; 32 contiguous variants => span of 31 from HEAD.
        assert_eq!(InflateMode::Sync as u16, 16211);
        assert_eq!(InflateMode::Sync as u16 - InflateMode::Head as u16, 31);
    }

    /// Every one of the 32 variants of `inflate.h`'s `inflate_mode`, in
    /// declaration order. Shared by the mode tests so a single list is the one
    /// place a new variant has to be registered — and [`mode_ordinal`] makes
    /// forgetting to register it a compile error.
    const ALL_MODES: [InflateMode; 32] = [
        InflateMode::Head,
        InflateMode::Flags,
        InflateMode::Time,
        InflateMode::Os,
        InflateMode::ExLen,
        InflateMode::Extra,
        InflateMode::Name,
        InflateMode::Comment,
        InflateMode::Hcrc,
        InflateMode::DictId,
        InflateMode::Dict,
        InflateMode::Type,
        InflateMode::TypeDo,
        InflateMode::Stored,
        InflateMode::CopyUnderscore,
        InflateMode::Copy,
        InflateMode::Table,
        InflateMode::LenLens,
        InflateMode::CodeLens,
        InflateMode::LenUnderscore,
        InflateMode::Len,
        InflateMode::LenExt,
        InflateMode::Dist,
        InflateMode::DistExt,
        InflateMode::Match,
        InflateMode::Lit,
        InflateMode::Check,
        InflateMode::Length,
        InflateMode::Done,
        InflateMode::Bad,
        InflateMode::Mem,
        InflateMode::Sync,
    ];

    /// The position each mode occupies in [`ALL_MODES`].
    ///
    /// The `match` deliberately has **no wildcard arm**: adding a variant to
    /// [`InflateMode`] makes this function fail to compile until the variant is
    /// classified, and [`every_mode_is_enumerated_exactly_once`] then requires it
    /// to appear in [`ALL_MODES`] at the matching index. Together they are the
    /// count/match guard that stops a new mode from escaping the mode tests.
    fn mode_ordinal(mode: InflateMode) -> usize {
        match mode {
            InflateMode::Head => 0,
            InflateMode::Flags => 1,
            InflateMode::Time => 2,
            InflateMode::Os => 3,
            InflateMode::ExLen => 4,
            InflateMode::Extra => 5,
            InflateMode::Name => 6,
            InflateMode::Comment => 7,
            InflateMode::Hcrc => 8,
            InflateMode::DictId => 9,
            InflateMode::Dict => 10,
            InflateMode::Type => 11,
            InflateMode::TypeDo => 12,
            InflateMode::Stored => 13,
            InflateMode::CopyUnderscore => 14,
            InflateMode::Copy => 15,
            InflateMode::Table => 16,
            InflateMode::LenLens => 17,
            InflateMode::CodeLens => 18,
            InflateMode::LenUnderscore => 19,
            InflateMode::Len => 20,
            InflateMode::LenExt => 21,
            InflateMode::Dist => 22,
            InflateMode::DistExt => 23,
            InflateMode::Match => 24,
            InflateMode::Lit => 25,
            InflateMode::Check => 26,
            InflateMode::Length => 27,
            InflateMode::Done => 28,
            InflateMode::Bad => 29,
            InflateMode::Mem => 30,
            InflateMode::Sync => 31,
        }
    }

    /// [`ALL_MODES`] lists every mode exactly once, in order, with nothing
    /// missing and nothing repeated.
    #[test]
    fn every_mode_is_enumerated_exactly_once() {
        // Ordering and uniqueness, cross-checked against the wildcard-free match.
        for (i, mode) in ALL_MODES.iter().enumerate() {
            assert_eq!(
                mode_ordinal(*mode),
                i,
                "ALL_MODES[{i}] is out of order or duplicated"
            );
        }
        // Count, tied to the discriminant span rather than to a literal, so the
        // array cannot drift away from the enum: 16211 - 16180 + 1 == 32.
        assert_eq!(
            ALL_MODES.len(),
            (InflateMode::Sync as u16 - InflateMode::Head as u16 + 1) as usize,
            "ALL_MODES must cover the entire discriminant span"
        );
    }

    #[test]
    fn all_32_variants_are_contiguous() {
        // Every variant, in the exact C order, must be exactly one more than the
        // previous, and there must be exactly 32 of them.
        assert_eq!(ALL_MODES.len(), 32);
        for (i, mode) in ALL_MODES.iter().enumerate() {
            assert_eq!(*mode as u16, 16180 + i as u16);
        }
    }

    #[test]
    fn mode_default_is_head() {
        assert_eq!(InflateMode::default(), InflateMode::Head);
    }

    #[test]
    fn table_source_default_is_dynamic() {
        assert_eq!(TableSource::default(), TableSource::Dynamic);
    }

    #[test]
    fn codes_arena_is_enough_1444() {
        let state = InflateState::new(0, 15);
        assert_eq!(ENOUGH, 1444);
        assert_eq!(state.codes.len(), ENOUGH);
        assert_eq!(state.codes.len(), 1444);
    }

    #[test]
    fn scratch_arrays_sized_exactly() {
        let state = InflateState::new(0, 15);
        assert_eq!(state.lens.len(), 320);
        assert_eq!(state.work.len(), 288);
    }

    #[test]
    fn new_reproduces_c_initial_state() {
        let state = InflateState::new(1, 15);
        assert_eq!(state.mode, InflateMode::Head);
        assert!(state.sane);
        assert_eq!(state.back, -1);
        assert!(state.window.is_empty());
        assert_eq!(state.wrap, 1);
        assert_eq!(state.wbits, 15);
        assert_eq!(state.flags, -1);
        assert_eq!(state.dmax, 32768);
        assert_eq!(state.total, 0);
        assert!(state.head.is_none());
        assert!(!state.last);
        assert!(!state.havedict);
        assert_eq!(state.hold, 0);
        assert_eq!(state.bits, 0);
        assert_eq!(state.lencode, 0);
        assert_eq!(state.distcode, 0);
        assert_eq!(state.next, 0);
        assert_eq!(state.lentable, TableSource::Dynamic);
        assert_eq!(state.disttable, TableSource::Dynamic);
        assert_eq!(state.wsize, 0);
        assert_eq!(state.whave, 0);
        assert_eq!(state.wnext, 0);
        assert_eq!(state.codes[0], Code::default());
    }

    #[test]
    fn is_valid_is_true_for_every_mode() {
        // `is_valid` is C's `inflateStateCheck` range test (`HEAD <= mode <=
        // SYNC`), so it must hold for every mode the closed enum can hold —
        // including the terminal `Bad`/`Mem`/`Sync` states. Driving the whole of
        // `ALL_MODES` (guarded by `every_mode_is_enumerated_exactly_once`) means
        // a newly added mode is covered here automatically.
        let mut state = InflateState::new(0, 15);
        for mode in ALL_MODES {
            state.mode = mode;
            assert!(state.is_valid(), "is_valid must hold for {mode:?}");
        }
    }

    #[test]
    fn clear_bitbuffer_zeroes_accumulator() {
        let mut state = InflateState::new(0, 15);
        state.hold = 0xDEAD_BEEF;
        state.bits = 24;
        state.clear_bitbuffer();
        assert_eq!(state.hold, 0);
        assert_eq!(state.bits, 0);
    }

    #[test]
    fn reset_keep_restores_reset_fields_but_keeps_window() {
        let mut state = InflateState::new(1, 15);
        // Simulate arbitrary mid-stream mutation.
        state.mode = InflateMode::Len;
        state.last = true;
        state.havedict = true;
        state.flags = 0;
        state.hold = 4321;
        state.bits = 15;
        state.back = 7;
        state.sane = false;
        state.lencode = 42;
        state.distcode = 99;
        state.next = 100;
        state.lentable = TableSource::Fixed;
        state.disttable = TableSource::Fixed;
        state.total = 999;
        state.dmax = 4096;
        // Pretend the window is in use.
        state.window = make_window(8);
        state.wsize = 8;
        state.whave = 4;
        state.wnext = 2;

        state.reset_keep();

        // Reset-managed fields are restored to their initial values.
        assert_eq!(state.mode, InflateMode::Head);
        assert!(!state.last);
        assert!(!state.havedict);
        assert_eq!(state.flags, -1);
        assert_eq!(state.hold, 0);
        assert_eq!(state.bits, 0);
        assert_eq!(state.back, -1);
        assert!(state.sane);
        assert_eq!(state.lencode, 0);
        assert_eq!(state.distcode, 0);
        assert_eq!(state.next, 0);
        assert_eq!(state.lentable, TableSource::Dynamic);
        assert_eq!(state.disttable, TableSource::Dynamic);
        assert_eq!(state.total, 0);
        assert_eq!(state.dmax, 32768);

        // The window allocation and geometry are kept (this is "reset *keep*").
        assert_eq!(state.window.len(), 8);
        assert_eq!(state.wsize, 8);
        assert_eq!(state.whave, 4);
        assert_eq!(state.wnext, 2);
    }

    #[test]
    fn reset_invalidates_window_geometry_but_keeps_allocation() {
        let mut state = InflateState::new(1, 15);
        state.window = make_window(8);
        state.wsize = 8;
        state.whave = 4;
        state.wnext = 2;
        state.mode = InflateMode::Len;

        state.reset();

        // Window geometry is zeroed (history invalidated) ...
        assert_eq!(state.wsize, 0);
        assert_eq!(state.whave, 0);
        assert_eq!(state.wnext, 0);
        // ... but the allocation is retained, matching C keeping state->window.
        assert_eq!(state.window.len(), 8);
        // ... and the reset_keep fields are applied too.
        assert_eq!(state.mode, InflateMode::Head);
        assert_eq!(state.back, -1);
    }

    #[test]
    fn lencode_slice_dynamic_indexes_into_codes() {
        let mut state = InflateState::new(0, 15);
        state.lentable = TableSource::Dynamic;
        state.lencode = 3;
        state.codes[3] = Code {
            op: 1,
            bits: 2,
            val: 7,
        };
        let fixed = [Code::default()];
        let slice = state.lencode_slice(&fixed);
        assert_eq!(
            slice[0],
            Code {
                op: 1,
                bits: 2,
                val: 7
            }
        );
        assert_eq!(slice.len(), ENOUGH - 3);
    }

    #[test]
    fn lencode_slice_fixed_returns_the_fixed_table() {
        let mut state = InflateState::new(0, 15);
        state.lentable = TableSource::Fixed;
        let fixed = [
            Code {
                op: 9,
                bits: 9,
                val: 99,
            },
            Code::default(),
        ];
        let slice = state.lencode_slice(&fixed);
        assert_eq!(slice.len(), 2);
        assert_eq!(
            slice[0],
            Code {
                op: 9,
                bits: 9,
                val: 99
            }
        );
    }

    #[test]
    fn distcode_slice_dynamic_and_fixed() {
        let mut state = InflateState::new(0, 15);
        state.disttable = TableSource::Dynamic;
        state.distcode = 5;
        state.codes[5] = Code {
            op: 4,
            bits: 5,
            val: 6,
        };
        let fixed = [Code {
            op: 1,
            bits: 1,
            val: 1,
        }];
        assert_eq!(
            state.distcode_slice(&fixed)[0],
            Code {
                op: 4,
                bits: 5,
                val: 6
            }
        );

        state.disttable = TableSource::Fixed;
        assert_eq!(
            state.distcode_slice(&fixed)[0],
            Code {
                op: 1,
                bits: 1,
                val: 1
            }
        );
    }

    #[test]
    fn state_is_boxable_matching_stream_model() {
        // `new` already yields a `Box<InflateState>`; inspect it before moving it
        // into the engine slot `src/stream.rs` actually holds — the place C keeps
        // its `internal_state *state` pointer.
        let state: Box<InflateState> = InflateState::new(2, 15);
        assert_eq!(state.mode, InflateMode::Head);
        assert_eq!(state.wrap, 2);

        let mut strm: crate::stream::ZStream = crate::stream::ZStream::new();
        // The slot holds a *placed* engine, because the FFI init path puts the state
        // in the caller's own `zalloc` region; `try_owned` is the global-heap arm of
        // that same wrapper, which is what a Rust-native `Box` becomes.
        strm.set_inflate_state(
            EngineBox::try_owned(*state).expect("the test heap holds an engine"),
        );
        assert!(strm.is_inflate());
        assert!(!strm.is_deflate());
        assert!(strm.has_state());
        // The typed view resolves, and the slot reports this direction. (The
        // wrong-direction downcast is asserted in `src/stream.rs`, which owns the
        // slot; `inflate` must not name `deflate` — they are strict peers.)
        assert_eq!(strm.inflate_state().map(|s| s.wrap), Some(2));
        assert!(!strm.is_deflate());

        // Taking it back hands over the same concrete box and empties the slot.
        let taken = strm.take_inflate_state().expect("an inflate engine");
        assert_eq!(taken.mode, InflateMode::Head);
        assert!(!strm.has_state());
        assert!(strm.take_inflate_state().is_none());
    }

    // =======================================================================
    // C layout-mirror pinning (AAP §0.6.3, §0.6.5)
    // =======================================================================

    /// `InflateState::C_LAYOUT_SIZE` must equal C's
    /// `sizeof(struct inflate_state)`.
    ///
    /// Measured with a `gcc` probe against the in-tree `inflate.h` and
    /// `inftrees.h` on this target (`x86_64-unknown-linux-gnu`, LP64,
    /// `gcc 15.2.0`). Gated to LP64 for the same reason as the deflate twin: on
    /// LLP64 `c_ulong` is four bytes and on 32-bit targets pointers are, so the
    /// total legitimately differs while the mirror stays correct by virtue of
    /// `#[repr(C)]` plus `core::ffi` aliases.
    #[test]
    #[cfg(all(target_pointer_width = "64", not(windows)))]
    fn c_layout_size_matches_the_measured_c_inflate_state() {
        assert_eq!(
            InflateState::C_LAYOUT_SIZE,
            7160,
            "sizeof(struct inflate_state) on LP64"
        );
        assert_eq!(
            align_of::<InflateStateC>(),
            8,
            "_Alignof(struct inflate_state) on LP64"
        );
        assert_ne!(
            InflateState::C_LAYOUT_SIZE,
            size_of::<InflateState>(),
            "the mirror must be C's layout, not this Rust type's"
        );
        assert_eq!(size_of::<CodeC>(), 4, "sizeof(code)");
        // `ENOUGH` bounds the `codes` arena; a wrong value here would move every
        // trailing offset and silently mis-report C's `sizeof`.
        assert_eq!(ENOUGH, 1444, "ENOUGH_LENS + ENOUGH_DISTS");
    }

    /// Field-by-field offset pinning for [`InflateStateC`], emitted verbatim by
    /// an `offsetof` probe compiled against the in-tree headers. See the deflate
    /// twin for why total size alone is insufficient evidence.
    #[test]
    #[cfg(all(target_pointer_width = "64", not(windows)))]
    fn c_layout_mirror_reproduces_every_c_inflate_state_offset() {
        use core::mem::offset_of;

        assert_eq!(offset_of!(InflateStateC, strm), 0);
        assert_eq!(offset_of!(InflateStateC, mode), 8);
        assert_eq!(offset_of!(InflateStateC, last), 12);
        assert_eq!(offset_of!(InflateStateC, wrap), 16);
        assert_eq!(offset_of!(InflateStateC, havedict), 20);
        assert_eq!(offset_of!(InflateStateC, flags), 24);
        assert_eq!(offset_of!(InflateStateC, dmax), 28);
        assert_eq!(offset_of!(InflateStateC, check), 32);
        assert_eq!(offset_of!(InflateStateC, total), 40);
        assert_eq!(offset_of!(InflateStateC, head), 48);
        assert_eq!(offset_of!(InflateStateC, wbits), 56);
        assert_eq!(offset_of!(InflateStateC, wsize), 60);
        assert_eq!(offset_of!(InflateStateC, whave), 64);
        assert_eq!(offset_of!(InflateStateC, wnext), 68);
        assert_eq!(offset_of!(InflateStateC, window), 72);
        assert_eq!(offset_of!(InflateStateC, hold), 80);
        assert_eq!(offset_of!(InflateStateC, bits), 88);
        assert_eq!(offset_of!(InflateStateC, length), 92);
        assert_eq!(offset_of!(InflateStateC, offset), 96);
        assert_eq!(offset_of!(InflateStateC, extra), 100);
        assert_eq!(offset_of!(InflateStateC, lencode), 104);
        assert_eq!(offset_of!(InflateStateC, distcode), 112);
        assert_eq!(offset_of!(InflateStateC, lenbits), 120);
        assert_eq!(offset_of!(InflateStateC, distbits), 124);
        assert_eq!(offset_of!(InflateStateC, ncode), 128);
        assert_eq!(offset_of!(InflateStateC, nlen), 132);
        assert_eq!(offset_of!(InflateStateC, ndist), 136);
        assert_eq!(offset_of!(InflateStateC, have), 140);
        assert_eq!(offset_of!(InflateStateC, next), 144);
        assert_eq!(offset_of!(InflateStateC, lens), 152);
        assert_eq!(offset_of!(InflateStateC, work), 792);
        assert_eq!(offset_of!(InflateStateC, codes), 1368);
        assert_eq!(offset_of!(InflateStateC, sane), 7144);
        assert_eq!(offset_of!(InflateStateC, back), 7148);
        assert_eq!(offset_of!(InflateStateC, was), 7152);
    }
}
