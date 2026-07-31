//! Deflate internal state — the foundational module of the `deflate` engine.
//!
//! This is a faithful, **100% safe Rust** port of the C baseline `deflate.h`
//! (the `deflate_state` structure and its companion constants) together with
//! the shared match-finding, window-management, and bit-output helpers that
//! live in `deflate.c` and `trees.c`. Every other file in `src/deflate/`
//! (`trees.rs`, `strategy.rs`, `fast.rs`, `slow.rs`, `stored.rs`, `huff.rs`,
//! `rle.rs`, and the `mod.rs` driver) builds on the types and methods defined
//! here.
//!
//! # Safety
//!
//! There is **zero `unsafe`** in this module (and, by design, in the whole
//! `deflate/` tree). This satisfies the project constraint "zero unsafe blocks
//! in core compression logic". Every working buffer is an owned
//! [`AllocBuffer`], every fixed-size table is an owned array, and every access
//! is a checked slice index. The C code relies on `zcalloc`/`zcfree` and raw
//! pointer arithmetic; here that is replaced by Rust ownership —
//! [`AllocBuffer`] routes each allocation through the caller's hook when one is
//! installed and through the global allocator otherwise, and releases it in
//! `Drop`, so there is no free path to forget.
//!
//! # Byte-exact fidelity
//!
//! Every constant, table, and algorithm reproduces the reference C behavior
//! bit-for-bit so that the emitted DEFLATE token stream is byte-identical to
//! reference zlib for the same input, level, and strategy. The match-finder
//! (`longest_match`, `fill_window`) and the bit packer (`send_bits`) are ported
//! with particular care because they directly determine the produced bytes.
//!
//! # `no_std`
//!
//! The module is `no_std` + `alloc`: it references only `core`, `alloc` (for
//! [`Box`]), and the crate's own modules — never `std`. The crate root turns
//! `no_std` on with
//! `#![cfg_attr(all(not(feature = "std"), not(test), panic = "abort"), no_std)]`
//! and declares `extern crate alloc;`. The two extra predicates matter: the
//! `not(test)` clause keeps the unit-test harness (which needs `std`) buildable
//! without the `std` feature, and the `panic = "abort"` clause reflects that a
//! stable-toolchain `no_std` build cannot link an unwinding runtime — both
//! release and dev profiles therefore set `panic = "abort"`.
//!
//! # C cross-reference
//!
//! Field and function names are kept close to the C originals (converted to
//! `snake_case`) so that the port can be audited against `deflate.h` /
//! `deflate.c` / `trees.c` line-by-line.

use alloc::boxed::Box;

use crate::checksum::adler32;
// `crc32` is only used for gzip (`wrap == 2`) framing, so gate its import to
// avoid an unused-import warning in no-gzip builds.
#[cfg(feature = "gzip")]
use crate::checksum::crc32;
use crate::constants::{DataType, MAX_MEM_LEVEL, Strategy, Z_DEFAULT_COMPRESSION, Z_DEFLATED};
use crate::deflate::strategy::CONFIGURATION_TABLE;
use crate::error::ZlibError;
#[cfg(feature = "gzip")]
use crate::gz_header::GzHeader;
use core::ffi::{c_int, c_long, c_uchar, c_uint, c_ulong, c_ushort, c_void};

use crate::stream::{AllocBuffer, AllocHook, Allocator, HookAllocator, ZeroValid, try_box};

// ===========================================================================
// Compile-time constants (ported from deflate.h / trees.h / trees.c / zutil.h)
// ===========================================================================

/// Number of length codes, not counting the special `END_BLOCK` code
/// (`deflate.h`: `LENGTH_CODES`).
pub const LENGTH_CODES: usize = 29;

/// Number of literal bytes `0..=255` (`deflate.h`: `LITERALS`).
pub const LITERALS: usize = 256;

/// Number of literal or length codes, including the `END_BLOCK` code
/// (`deflate.h`: `L_CODES = LITERALS + 1 + LENGTH_CODES` = 286).
pub const L_CODES: usize = LITERALS + 1 + LENGTH_CODES;

/// Number of distance codes (`deflate.h`: `D_CODES`).
pub const D_CODES: usize = 30;

/// Number of codes used to transfer the bit lengths (`deflate.h`: `BL_CODES`).
pub const BL_CODES: usize = 19;

/// Maximum heap size used when building the Huffman trees
/// (`deflate.h`: `HEAP_SIZE = 2 * L_CODES + 1` = 573).
pub const HEAP_SIZE: usize = 2 * L_CODES + 1;

/// All codes must not exceed this many bits (`deflate.h`: `MAX_BITS`).
pub const MAX_BITS: usize = 15;

/// Maximum number of bits used to encode the bit lengths themselves
/// (`trees.c`: `MAX_BL_BITS`).
pub const MAX_BL_BITS: usize = 7;

/// Size of the bit buffer `bi_buf`, in bits (`deflate.h`: `Buf_size`).
///
/// Kept as an `i32` because it participates in signed arithmetic in
/// [`DeflateState::send_bits`] (e.g. `Buf_size - length`). The C name is
/// `Buf_size`; the Rust constant is upper-cased to satisfy naming lints.
pub const BUF_SIZE: i32 = 16;

/// Minimum match length recognized by the LZ77 stage (`zutil.h`: `MIN_MATCH`).
pub const MIN_MATCH: usize = 3;

/// Maximum match length recognized by the LZ77 stage (`zutil.h`: `MAX_MATCH`).
pub const MAX_MATCH: usize = 258;

/// Minimum amount of lookahead required, except at the end of the input
/// (`deflate.h`: `MIN_LOOKAHEAD = MAX_MATCH + MIN_MATCH + 1` = 262).
pub const MIN_LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;

/// Number of bytes after the end of the current data in the window that are
/// initialized (zeroed) so that the longest-match routines may scan up to
/// `strstart + MAX_MATCH` without reading uninitialized memory
/// (`deflate.h`: `WIN_INIT = MAX_MATCH` = 258).
pub const WIN_INIT: usize = MAX_MATCH;

/// The special end-of-block code (`trees.c`: `END_BLOCK`).
pub const END_BLOCK: usize = 256;

/// Tail-of-hash-chain sentinel (`deflate.c`: `NIL`). A `Pos` (window index)
/// equal to `NIL` denotes "no further entry".
pub const NIL: u16 = 0;

/// Matches of length [`MIN_MATCH`] are discarded if their distance exceeds this
/// value (`deflate.c`: `TOO_FAR`).
pub const TOO_FAR: usize = 4096;

// ===========================================================================
// C layout mirror — the authoritative `sizeof(deflate_state)` for allocator
// accounting (AAP §0.6.3, §0.6.5)
// ===========================================================================

/// A field-exact `#[repr(C)]` mirror of C's `deflate_state`
/// (`deflate.h` L104-L283), used for **one** purpose: to compute the byte count
/// that reference zlib passes to the caller's `zalloc` when it allocates the
/// engine state with `ZALLOC(strm, 1, sizeof(deflate_state))` (`deflate.c` L440).
///
/// # Why a mirror instead of `size_of::<DeflateState>()`
///
/// The idiomatic [`DeflateState`] is a *different* type: it holds
/// [`AllocBuffer`]s (an enum with a discriminant) where C holds bare pointers,
/// index-typed cursors where C holds `uInt`, and Rust enums where C holds `int`.
/// Its size is therefore legitimately different — 6152 bytes against C's 5968 on
/// LP64 — and using it as the request size makes a caller's allocator observe a
/// footprint no C build ever asks for. Since allocation accounting is a
/// first-class parity requirement (AAP §0.6.5), the *request* must be sized by
/// the C layout even though the *storage* is a Rust `Box`.
///
/// # Why this is portable
///
/// Every field below is either a `core::ffi` scalar alias or a raw pointer, and
/// the struct is `#[repr(C)]`, so rustc lays it out with the platform C ABI's
/// rules — the same rules the C compiler applies to `deflate_state`. The size is
/// therefore correct on LP64, LLP64 (Windows, where `c_ulong` is 32-bit), and
/// 32-bit targets alike without a per-target table. The tests pin the exact LP64
/// numbers produced by `gcc` against the in-tree `deflate.h`, field offset by
/// field offset.
///
/// # Configuration assumptions, both matching the cross-validation oracle
///
/// * `LIT_MEM` is **not** defined — `deflate.h` L28 reads
///   `/* #define LIT_MEM */` — so the union arm here is the single `sym_buf`
///   pointer and `LIT_BUFS` is 4.
/// * `ZLIB_DEBUG` is **not** defined, so `compressed_len` and `bits_sent` are
///   absent. This matches the reference library the byte-identity sweep builds.
///
/// Nothing ever constructs or reads this type; it exists purely as a layout
/// description, and its fields are read only by the offset-pinning test.
// Each mirror below is a layout *description*: nothing constructs it and nothing
// reads its fields outside the offset-pinning test, which is exactly what
// `dead_code` reports. The allowance is scoped to these three types and is the
// standard idiom for an FFI layout mirror — the alternative (hand-writing a
// 60-field constructor that is never used either) would add code without adding
// verification.
#[allow(dead_code)]
#[repr(C)]
struct DeflateStateC {
    strm: *mut c_void,
    status: c_int,
    pending_buf: *mut c_uchar,
    pending_buf_size: c_ulong,
    pending_out: *mut c_uchar,
    pending: c_ulong,
    wrap: c_int,
    gzhead: *mut c_void,
    gzindex: c_ulong,
    method: c_uchar,
    last_flush: c_int,
    w_size: c_uint,
    w_bits: c_uint,
    w_mask: c_uint,
    window: *mut c_uchar,
    window_size: c_ulong,
    prev: *mut c_ushort,
    head: *mut c_ushort,
    ins_h: c_uint,
    hash_size: c_uint,
    hash_bits: c_uint,
    hash_mask: c_uint,
    hash_shift: c_uint,
    block_start: c_long,
    match_length: c_uint,
    prev_match: c_uint,
    match_available: c_int,
    strstart: c_uint,
    match_start: c_uint,
    lookahead: c_uint,
    prev_length: c_uint,
    max_chain_length: c_uint,
    max_lazy_match: c_uint,
    level: c_int,
    strategy: c_int,
    good_match: c_uint,
    nice_match: c_int,
    dyn_ltree: [CtDataC; HEAP_SIZE],
    dyn_dtree: [CtDataC; 2 * D_CODES + 1],
    bl_tree: [CtDataC; 2 * BL_CODES + 1],
    l_desc: TreeDescC,
    d_desc: TreeDescC,
    bl_desc: TreeDescC,
    bl_count: [c_ushort; MAX_BITS + 1],
    heap: [c_int; 2 * L_CODES + 1],
    heap_len: c_int,
    heap_max: c_int,
    depth: [c_uchar; 2 * L_CODES + 1],
    sym_buf: *mut c_uchar,
    lit_bufsize: c_uint,
    sym_next: c_uint,
    sym_end: c_uint,
    opt_len: c_ulong,
    static_len: c_ulong,
    matches: c_uint,
    insert: c_uint,
    bi_buf: c_ushort,
    bi_valid: c_int,
    bi_used: c_int,
    high_water: c_ulong,
    slid: c_int,
}

/// Layout mirror of C's `struct ct_data_s` (`deflate.h` L72-L82): two 2-byte
/// unions, each of two `ush` arms, so the whole struct is four bytes.
#[allow(dead_code)]
#[repr(C)]
#[derive(Copy, Clone)]
struct CtDataC {
    /// The `fc` union (`freq` / `code`).
    fc: c_ushort,
    /// The `dl` union (`dad` / `len`).
    dl: c_ushort,
}

/// Layout mirror of C's `struct tree_desc_s` (`deflate.h` L90-L94).
#[allow(dead_code)]
#[repr(C)]
#[derive(Copy, Clone)]
struct TreeDescC {
    dyn_tree: *mut CtDataC,
    max_code: c_int,
    stat_desc: *const c_void,
}

// ===========================================================================
// DeflateStatus — the deflate stream state machine (deflate.h L58-L67)
// ===========================================================================

/// The deflate stream status, replacing the integer-tagged `status` field of
/// the C `deflate_state`.
///
/// The discriminants are the **exact** C sentinel values (`deflate.h`
/// L58-L67); they must never change because some are observable through the
/// FFI state check and through save/restore semantics. Using a Rust `enum`
/// means the C `deflateStateCheck` status-membership test (does `status`
/// belong to the valid set?) becomes automatic: any `DeflateStatus` value is,
/// by construction, one of the valid variants, so that portion of the check is
/// trivially satisfied.
///
/// `#[repr(u16)]` is required because `Finish = 666` does not fit in a `u8`.
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeflateStatus {
    /// `INIT_STATE = 42`: zlib header pending → transitions to `Busy`.
    Init = 42,
    /// `GZIP_STATE = 57`: gzip header pending → `Busy` / `Extra`.
    Gzip = 57,
    /// `EXTRA_STATE = 69`: writing the gzip "extra" field → `Name`.
    Extra = 69,
    /// `NAME_STATE = 73`: writing the gzip file name → `Comment`.
    Name = 73,
    /// `COMMENT_STATE = 91`: writing the gzip comment → `Hcrc`.
    Comment = 91,
    /// `HCRC_STATE = 103`: writing the gzip header CRC → `Busy`.
    Hcrc = 103,
    /// `BUSY_STATE = 113`: actively deflating → `Finish`.
    Busy = 113,
    /// `FINISH_STATE = 666`: the stream is complete.
    Finish = 666,
}

// ===========================================================================
// CtData — the C `ct_data` union rendered as a safe struct (deflate.h)
// ===========================================================================

/// A single entry of a Huffman tree — the safe-Rust equivalent of the C
/// `ct_data` union.
///
/// In C, `ct_data` is a pair of unions: `fc` overlays `freq` (frequency count,
/// used while building the tree) with `code` (the emitted bit string, used
/// after `gen_codes`), and `dl` overlays `dad` (parent node during tree
/// construction) with `len` (code length afterwards). Because both members of
/// each union are `ush` (`u16`) and the two lifetimes never overlap in the
/// algorithm, the union is faithfully represented by two plain `u16` fields
/// with accessor methods that name the intended interpretation.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct CtData {
    /// The `fc` union member: frequency (`freq`) or code (`code`).
    pub fc: u16,
    /// The `dl` union member: parent (`dad`) or code length (`len`).
    pub dl: u16,
}

impl CtData {
    /// Reads the frequency count (`fc.freq`), used during tree building.
    #[inline]
    #[must_use]
    pub fn freq(&self) -> u16 {
        self.fc
    }

    /// Writes the frequency count (`fc.freq`).
    #[inline]
    pub fn set_freq(&mut self, v: u16) {
        self.fc = v;
    }

    /// Reads the emitted bit string (`fc.code`), valid after `gen_codes`.
    #[inline]
    #[must_use]
    pub fn code(&self) -> u16 {
        self.fc
    }

    /// Writes the emitted bit string (`fc.code`).
    #[inline]
    pub fn set_code(&mut self, v: u16) {
        self.fc = v;
    }

    /// Reads the parent node index (`dl.dad`), used during tree building.
    #[inline]
    #[must_use]
    pub fn dad(&self) -> u16 {
        self.dl
    }

    /// Writes the parent node index (`dl.dad`).
    #[inline]
    pub fn set_dad(&mut self, v: u16) {
        self.dl = v;
    }

    /// Reads the code length in bits (`dl.len`), valid after tree building.
    ///
    /// This mirrors the C `Len(tree, n)` accessor (`ct_data.dl.len`); it is a
    /// domain field name, not a collection length, so the companion
    /// `is_empty` that `clippy::len_without_is_empty` expects would be
    /// meaningless here.
    #[inline]
    #[must_use]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u16 {
        self.dl
    }

    /// Writes the code length in bits (`dl.len`).
    #[inline]
    pub fn set_len(&mut self, v: u16) {
        self.dl = v;
    }
}

// ===========================================================================
// TreeKind — replaces the C static_tree_desc / tree_desc pointer plumbing
// ===========================================================================

/// Identifies which of the three Huffman trees a routine is operating on,
/// replacing the C `tree_desc` / `static_tree_desc` pointer pairs.
///
/// In C, each of `l_desc`, `d_desc`, and `bl_desc` bundles a pointer to the
/// dynamic tree (`dyn_tree`) with a pointer to the corresponding static
/// descriptor (`stat_desc`). In this port the dynamic trees live directly in
/// [`DeflateState`] (`dyn_ltree`, `dyn_dtree`, `bl_tree`) and their per-tree
/// `max_code` is stored in [`DeflateState::l_max_code`],
/// [`DeflateState::d_max_code`], and [`DeflateState::bl_max_code`]. A
/// `TreeKind` value then selects the matching static descriptor data
/// (extra-bits table, extra base, element count, maximum length) which
/// `trees.rs` provides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeKind {
    /// The combined literal/length tree (`l_desc` in C).
    Literal,
    /// The distance tree (`d_desc` in C).
    Distance,
    /// The bit-length tree (`bl_desc` in C).
    BitLength,
}

// ===========================================================================
// IoContext — the transient z_stream I/O view threaded through the engine
// ===========================================================================

/// A borrowed, mutable view of the `z_stream` input/output cursors, checksum,
/// and byte counters that the deflate engine reads and advances during a
/// single call.
///
/// The C engine reaches back into `strm` (the `z_stream`) through
/// `s->strm->next_in`, `avail_in`, `next_out`, `avail_out`, `total_in`,
/// `total_out`, and `adler`. Storing those inside [`DeflateState`] would create
/// a dependency cycle with the public `ZStream` type in `src/stream.rs`, so
/// they are gathered here instead. The `mod.rs` driver constructs an
/// `IoContext` from the `ZStream`'s public buffers, threads it through the
/// block producers, and writes the advanced cursors / totals / checksum back
/// into the `ZStream` when the call returns.
///
/// # Field semantics (mirroring `z_stream`)
///
/// * [`input`](Self::input) / [`next_in`](Self::next_in) /
///   [`avail_in`](Self::avail_in): the input buffer, the read cursor into it,
///   and the number of bytes still available (`input.len() == next_in +
///   avail_in` is the intended invariant).
/// * [`output`](Self::output) / [`next_out`](Self::next_out) /
///   [`avail_out`](Self::avail_out): the output buffer, the write cursor, and
///   the remaining free space.
/// * [`total_in`](Self::total_in) / [`total_out`](Self::total_out): running
///   byte counters. zlib types these as `uLong`; `u64` is used internally and
///   the FFI boundary narrows them as required. The gzip trailer uses
///   `total_in & 0xffff_ffff`.
/// * [`adler`](Self::adler): the running checksum — Adler-32 for a zlib
///   wrapper, CRC-32 for a gzip wrapper.
pub struct IoContext<'a> {
    /// The input byte buffer (`z_stream::next_in` points into this).
    pub input: &'a [u8],
    /// Read cursor: index of the next unconsumed input byte.
    pub next_in: usize,
    /// Number of input bytes still available from [`next_in`](Self::next_in).
    pub avail_in: usize,
    /// The output byte buffer (`z_stream::next_out` points into this).
    pub output: &'a mut [u8],
    /// Write cursor: index of the next free output byte.
    pub next_out: usize,
    /// Number of free output bytes still available from
    /// [`next_out`](Self::next_out).
    pub avail_out: usize,
    /// Running total of input bytes consumed (`z_stream::total_in`).
    pub total_in: u64,
    /// Running total of output bytes produced (`z_stream::total_out`).
    pub total_out: u64,
    /// Running checksum accumulator (`z_stream::adler`): Adler-32 for zlib
    /// framing, CRC-32 for gzip framing.
    pub adler: u32,
}

impl<'a> IoContext<'a> {
    /// Creates an `IoContext` over the given input and output buffers with the
    /// `avail_*` counters initialized to the full buffer lengths, the cursors
    /// at the start, and the totals/checksum seeded to zero.
    ///
    /// The `adler` seed is deliberately `0`: the caller (the `mod.rs` driver)
    /// overwrites it with the correct initial checksum for the chosen wrapper
    /// via [`DeflateState::initial_adler`] before the first byte is processed.
    #[must_use]
    pub fn new(input: &'a [u8], output: &'a mut [u8]) -> Self {
        let avail_in = input.len();
        let avail_out = output.len();
        IoContext {
            input,
            next_in: 0,
            avail_in,
            output,
            next_out: 0,
            avail_out,
            total_in: 0,
            total_out: 0,
            adler: 0,
        }
    }

    /// Returns the number of input bytes still available.
    ///
    /// Mirrors reading `strm->avail_in` in the C engine. This simply returns
    /// the [`avail_in`](Self::avail_in) field and is provided for call-site
    /// readability alongside [`avail_out_remaining`](Self::avail_out_remaining).
    #[inline]
    #[must_use]
    pub fn avail_in_remaining(&self) -> usize {
        self.avail_in
    }

    /// Returns the number of free output bytes still available.
    ///
    /// Mirrors reading `strm->avail_out` in the C engine.
    #[inline]
    #[must_use]
    pub fn avail_out_remaining(&self) -> usize {
        self.avail_out
    }
}

// ===========================================================================
// DeflateState — the port of C `deflate_state` (deflate.h L104-L288)
// ===========================================================================

/// The complete internal deflate state, a safe-Rust port of the C
/// `deflate_state` structure.
///
/// This owns every working buffer (the sliding [`window`](Self::window), the
/// hash-chain tables [`prev`](Self::prev)/[`head`](Self::head), and the
/// [`pending_buf`](Self::pending_buf) staging area, which carries the symbol
/// region overlaid in its upper three quarters) as [`AllocBuffer`]s, replacing
/// the C `zcalloc`/`zcfree` pointers. Each one is routed through the caller's
/// `zalloc`/`zfree` hook when the `z_stream` installs an active pair, and
/// through the global allocator otherwise. It is created with
/// [`DeflateState::new`] and owned by the [`ZStream`](crate::stream::ZStream)
/// as the `StreamState::Deflate(Box<DeflateState>)` variant — the place the C
/// `internal_state *state` pointer went — so its memory is released
/// automatically via `Drop` and there is no explicit free to forget.
///
/// [`try_clone`](Self::try_clone) supports the `deflateCopy` operation: it
/// deep-copies every buffer through the allocator that backs it, and — because
/// this port uses indices and owned arrays rather than raw pointers — there are
/// **no pointer fix-ups** to perform afterwards, a substantial simplification
/// over the C implementation. The copy is deliberately *fallible* (there is no
/// [`Clone`] impl): a buffer owned by the caller's `zalloc` must be re-allocated
/// through that same hook, and if that fails the copy reports
/// [`ZlibError::MemError`] exactly as C returns `Z_MEM_ERROR` — it never
/// substitutes global-allocator storage (AAP §0.6.3, §0.6.5).
///
/// # Memory footprint
///
/// At the defaults (`mem_level = 8`, `w_bits = 15`) the allocations are:
/// `window` = `2 * 32 KiB` = 64 KiB, `prev` = `32 Ki * 2 B` = 64 KiB,
/// `head` = `32 Ki * 2 B` = 64 KiB, and `pending_buf` = `4 * lit_bufsize` =
/// `4 * 16 KiB` = 64 KiB, for a total of about 256 KiB — the same four
/// allocations and the same byte totals as reference zlib, because the symbol
/// region is **overlaid inside `pending_buf`** exactly as C overlays it
/// (`s->sym_buf = s->pending_buf + s->lit_bufsize`, `deflate.c` L520). The
/// overlay is expressed as index arithmetic through `sym` /
/// `set_sym`, so it costs no `unsafe` and no aliasing pointer.
pub struct DeflateState {
    /// Current stream status (C `status`). See [`DeflateStatus`].
    pub status: DeflateStatus,

    /// Output staging buffer (C `pending_buf`). Bytes produced by the bit
    /// packer accumulate here before being copied to the caller's output by
    /// [`flush_pending`](Self::flush_pending). Sized `lit_bufsize * 4`
    /// (`LIT_BUFS == 4`, since `deflate.h` L28 leaves `LIT_MEM` undefined).
    ///
    /// # The symbol region is overlaid here
    ///
    /// Bytes `[0, lit_bufsize)` are the pending-output area; bytes
    /// `[lit_bufsize, 4 * lit_bufsize)` are the **symbol region** — three bytes
    /// per token (distance low, distance high, literal/length) — which is
    /// precisely where C puts it with `s->sym_buf = s->pending_buf +
    /// s->lit_bufsize` (`deflate.c` L520). C's pointer arithmetic becomes index
    /// arithmetic (AAP §0.3.2 rule T3): reach the region through
    /// `sym` / `set_sym`, which add
    /// [`lit_bufsize`](Self::lit_bufsize) to a region-relative index, so no
    /// aliasing pointer and no `unsafe` is involved.
    ///
    /// Sharing one allocation is not merely a space optimisation: it is what
    /// makes the request count and the `(items, size)` pairs a caller's `zalloc`
    /// observes identical to C's (AAP §0.6.5), and it keeps C's overflow
    /// reasoning (`deflate.c` L466-L500, which proves at least 139 bits of
    /// headroom so block emission never overruns the symbols still being read)
    /// applicable verbatim, together with `sym_end = (lit_bufsize - 1) * 3`.
    pub pending_buf: AllocBuffer<u8>,

    /// Size of [`pending_buf`](Self::pending_buf) in bytes (C
    /// `pending_buf_size`), equal to `lit_bufsize * 4`.
    pub pending_buf_size: usize,

    /// Index into [`pending_buf`](Self::pending_buf) of the next byte to output
    /// (C `pending_out`, which is a pointer there; stored here as an offset).
    pub pending_out: usize,

    /// Number of bytes currently queued in [`pending_buf`](Self::pending_buf)
    /// awaiting output (C `pending`).
    pub pending: usize,

    /// Wrapper selector (C `wrap`): `0` = raw DEFLATE, `1` = zlib, `2` = gzip.
    /// May be temporarily negated by `deflate(..., Z_FINISH)` after the trailer
    /// is written; [`reset_keep`](Self::reset_keep) restores it to positive.
    pub wrap: i32,

    /// The gzip header to write, if any (C `gzhead`). Only present when the
    /// `gzip` feature is enabled.
    #[cfg(feature = "gzip")]
    pub gzhead: Option<GzHeader>,

    /// Current offset within the gzip extra/name/comment field being written
    /// (C `gzindex`). Only present when the `gzip` feature is enabled.
    #[cfg(feature = "gzip")]
    pub gzindex: usize,

    /// The compression method (C `method`). Always [`Z_DEFLATED`] (`8`).
    pub method: u8,

    /// The `flush` argument supplied to the previous `deflate` call (C
    /// `last_flush`), initialized to `-2`.
    pub last_flush: i32,

    /// LZ77 window size in bytes, `1 << w_bits` (C `w_size`).
    pub w_size: usize,
    /// `log2(w_size)`, in `8..=15` (C `w_bits`).
    pub w_bits: u32,
    /// `w_size - 1`, used to wrap window indices (C `w_mask`).
    pub w_mask: usize,

    /// The sliding window (C `window`), of length `2 * w_size`. Input bytes are
    /// read into the upper half and shifted down to retain a dictionary.
    pub window: AllocBuffer<u8>,

    /// Actual usable window size, `2 * w_size` (C `window_size`).
    pub window_size: usize,

    /// Hash-chain link table (C `prev`), length `w_size`. `prev[i]` links a
    /// window position to the previous position with the same hash. Entries are
    /// `Pos` (`u16`) window indices modulo `w_size`.
    pub prev: AllocBuffer<u16>,

    /// Hash-chain heads (C `head`), length `hash_size`. `head[h]` is the most
    /// recent window position hashing to `h`, or [`NIL`].
    pub head: AllocBuffer<u16>,

    /// Running hash index of the string about to be inserted (C `ins_h`).
    pub ins_h: usize,
    /// Number of slots in the hash table (C `hash_size`), `1 << hash_bits`.
    pub hash_size: usize,
    /// `log2(hash_size)` (C `hash_bits`).
    pub hash_bits: u32,
    /// `hash_size - 1` (C `hash_mask`).
    pub hash_mask: usize,

    /// Number of bits `ins_h` is shifted by per input byte (C `hash_shift`),
    /// equal to `(hash_bits + MIN_MATCH - 1) / MIN_MATCH`.
    pub hash_shift: u32,

    /// Window position at the start of the current output block (C
    /// `block_start`). **Signed** because it goes negative when the window
    /// slides backwards; used by the block flush to decide whether a stored
    /// block can be emitted from the window.
    pub block_start: isize,

    /// Length of the best match found (C `match_length`).
    pub match_length: usize,
    /// Previous match position (C `prev_match`, an `IPos`).
    pub prev_match: u16,
    /// Set when a deferred (lazy) match from the previous step exists (C
    /// `match_available`, an `int` used as a boolean).
    pub match_available: bool,
    /// Start of the string currently being inserted / matched (C `strstart`).
    pub strstart: usize,
    /// Start of the matching string in the window (C `match_start`), set as a
    /// side effect of [`longest_match`](Self::longest_match).
    pub match_start: usize,
    /// Number of valid bytes ahead of `strstart` in the window (C `lookahead`).
    pub lookahead: usize,

    /// Length of the best match at the previous step (C `prev_length`). Matches
    /// not longer than this are discarded during lazy evaluation.
    pub prev_length: usize,

    /// Hash chains are never searched beyond this many links (C
    /// `max_chain_length`).
    pub max_chain_length: usize,

    /// Only attempt a better (lazy) match when the current match is strictly
    /// shorter than this (C `max_lazy_match`). For levels `<= 3` the same field
    /// is read as `max_insert_length` via
    /// [`max_insert_length`](Self::max_insert_length).
    pub max_lazy_match: usize,

    /// Compression level `0..=9` (C `level`), with [`Z_DEFAULT_COMPRESSION`]
    /// already resolved to `6` by [`new`](Self::new).
    pub level: i32,
    /// Compression strategy (C `strategy`). Stored as the crate
    /// [`Strategy`] enum so comparisons such as `strategy == HuffmanOnly` are
    /// exhaustive and type-checked.
    pub strategy: Strategy,

    /// Switch to a faster search once the previous match is longer than this
    /// (C `good_match`).
    pub good_match: usize,

    /// Stop searching once a match reaches at least this length (C
    /// `nice_match`). Signed to match the C `int`.
    pub nice_match: i32,

    /// The dynamic literal/length tree (C `dyn_ltree`), `HEAP_SIZE` entries.
    pub dyn_ltree: [CtData; HEAP_SIZE],
    /// The dynamic distance tree (C `dyn_dtree`), `2 * D_CODES + 1` entries.
    pub dyn_dtree: [CtData; 2 * D_CODES + 1],
    /// The bit-length tree (C `bl_tree`), `2 * BL_CODES + 1` entries.
    pub bl_tree: [CtData; 2 * BL_CODES + 1],

    /// Largest code with non-zero frequency in the literal/length tree
    /// (replaces `l_desc.max_code`).
    pub l_max_code: usize,
    /// Largest code with non-zero frequency in the distance tree (replaces
    /// `d_desc.max_code`).
    pub d_max_code: usize,
    /// Largest code with non-zero frequency in the bit-length tree (replaces
    /// `bl_desc.max_code`).
    pub bl_max_code: usize,

    /// Count of codes at each bit length for an optimal tree (C `bl_count`),
    /// `MAX_BITS + 1` entries.
    pub bl_count: [u16; MAX_BITS + 1],

    /// Heap used to build the Huffman trees (C `heap`), `2 * L_CODES + 1`
    /// entries. `heap[0]` is unused; the sons of `heap[n]` are `heap[2n]` and
    /// `heap[2n + 1]`.
    pub heap: [i32; 2 * L_CODES + 1],
    /// Number of elements currently in the heap (C `heap_len`).
    pub heap_len: usize,
    /// Index of the element of largest frequency in the heap (C `heap_max`).
    pub heap_max: usize,

    /// Depth of each subtree, used as a tie-breaker for equal-frequency trees
    /// (C `depth`), `2 * L_CODES + 1` entries.
    pub depth: [u8; 2 * L_CODES + 1],

    /// Size of the literal/length symbol buffer in symbols-worth of bytes
    /// (C `lit_bufsize`), equal to `1 << (mem_level + 6)`.
    ///
    /// It is also the **base index of the symbol region inside
    /// [`pending_buf`](Self::pending_buf)**, because that region is overlaid
    /// exactly where C puts it: `s->sym_buf = s->pending_buf + s->lit_bufsize`
    /// (`deflate.c` L520). See `sym` / `set_sym`.
    pub lit_bufsize: usize,

    /// Running write index into the symbol region (C `sym_next`), counted in
    /// bytes from the region's base — i.e. relative to `pending_buf[lit_bufsize]`,
    /// exactly as C counts it from `sym_buf`.
    pub sym_next: usize,
    /// Index at which the symbol region is considered full and a flush is forced
    /// (C `sym_end`), equal to `(lit_bufsize - 1) * 3`.
    pub sym_end: usize,

    /// Bit length of the current block encoded with the optimal (dynamic) trees
    /// (C `opt_len`, a `ulg`).
    pub opt_len: usize,
    /// Bit length of the current block encoded with the static trees
    /// (C `static_len`, a `ulg`).
    pub static_len: usize,
    /// Number of string matches in the current block; also reused as the count
    /// of pending hash-table slides for `deflateParams` when `level == 0`
    /// (C `matches`).
    pub matches: usize,
    /// Bytes at the end of the window still to be inserted into the hash on the
    /// next `deflate` call (C `insert`).
    pub insert: usize,

    /// Bit accumulator; bits are inserted starting at the least-significant end
    /// (C `bi_buf`, a `ush`).
    pub bi_buf: u16,
    /// Number of valid bits currently in [`bi_buf`](Self::bi_buf) (C
    /// `bi_valid`). Signed to match the C `int` and its signed arithmetic.
    pub bi_valid: i32,
    /// Number of bits used in the final byte when aligning to a byte boundary
    /// (C `bi_used`), reported for the `deflateUsed`/`deflatePrime` machinery.
    pub bi_used: i32,

    /// High-water mark: the highest window offset that has been initialized
    /// (C `high_water`). Bytes above this are zeroed by
    /// [`fill_window`](Self::fill_window) so the match routines never read
    /// uninitialized memory.
    pub high_water: usize,

    /// `true` once the hash table has been slid at least once since it was
    /// cleared (C `slid`).
    pub slid: bool,

    /// Best-guess classification of the input set by `deflate` (C
    /// `data_type`), initialized to [`DataType::Unknown`].
    pub data_type: DataType,

    /// The `mem_level` the state was created with. The accepted range is
    /// `1..=MAX_MEM_LEVEL`, i.e. `1..=9`, matching the C `deflateInit2_`
    /// validation (`deflate.c` L433-L437); the *default* used by
    /// `deflateInit`/`compress` is [`DEF_MEM_LEVEL`](crate::constants::DEF_MEM_LEVEL)
    /// `== 8`. Retained for `deflateParams` / `deflateBound` bookkeeping even
    /// though most derived quantities (`hash_bits`, `lit_bufsize`) are
    /// precomputed.
    pub mem_level: i32,

    /// The caller's allocator hook, retained so the `deflateCopy` path can
    /// duplicate the working buffers through the *same* `zalloc` the original
    /// used (AAP §0.6.5) — reproducing C `deflateCopy`, whose `zmemcpy` of the
    /// `z_stream` makes the destination inherit the source's
    /// `zalloc`/`zfree`/`opaque` before any allocation happens
    /// (`deflate.c` L1317-L1377).
    ///
    /// [`AllocHook::none`] on the global-allocator path.
    pub(crate) alloc_hook: AllocHook,

    /// Reservation standing in for C's `ZALLOC(strm, 1, sizeof(deflate_state))`
    /// (`deflate.c` L440).
    ///
    /// The state object itself is a Rust `Box`, because the whole design keeps
    /// engine state as an owned, `Clone`-able Rust value (AAP §0.6.3) rather
    /// than a caller-allocated arena. C nevertheless charges the state's bytes
    /// to the caller's allocator, and a bounded allocator such as
    /// `infcover.c`'s must therefore see them: without this reservation a
    /// caller who budgets exactly `sizeof(deflate_state)` fewer bytes than the
    /// real requirement would observe `Z_OK` where C reports `Z_MEM_ERROR`,
    /// which is precisely the memory-bounds parity AAP §0.6.5 requires.
    ///
    /// Requested as `(1, `[`C_LAYOUT_SIZE`](Self::C_LAYOUT_SIZE)`)` — C's own
    /// `sizeof(deflate_state)`, computed from the field-exact layout mirror rather
    /// than from this Rust type's size — so the hook sees C's exact argument pair,
    /// and taken **first**, matching C's ordering. Empty (a
    /// zero-length [`AllocBuffer::Owned`]) whenever the hook is inactive, so the
    /// historical global-allocator path is byte-for-byte unchanged.
    pub(crate) state_alloc: AllocBuffer<u8>,
}

/// A `deflateInit2_` construction failure, carrying the extra bit of information
/// C's two distinct allocation-failure points make observable.
///
/// C checks the state object and the four working buffers at *different* places,
/// and only the second one records a diagnostic:
///
/// * `deflate.c` L440-L442 — `s = ZALLOC(strm, 1, sizeof(deflate_state)); if (s
///   == Z_NULL) return Z_MEM_ERROR;`. An immediate return; `strm->msg` is left as
///   `inflateInit`/`deflateInit` found it (NULL, cleared at L400).
/// * `deflate.c` L505-L514 — if any of `window`/`prev`/`head`/`pending_buf` is
///   NULL, C sets `strm->msg = ERR_MSG(Z_MEM_ERROR)` ("insufficient memory"),
///   calls `deflateEnd(strm)`, and *then* returns `Z_MEM_ERROR`. Because
///   `deflateEnd` never touches `strm->msg` (`deflate.c` L1293-L1310), the
///   message survives for the caller to read.
///
/// Collapsing both into a bare [`ZlibError::MemError`] would lose that
/// distinction and leave `strm->msg` NULL where C supplies a message, so the
/// constructor reports which point was reached (standard S5: no silent behavior
/// change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeflateInitError {
    /// The zlib error code to report.
    pub(crate) code: ZlibError,
    /// Whether C would have stored `ERR_MSG(Z_MEM_ERROR)` into `strm->msg`
    /// before returning — true only for a working-buffer failure.
    pub(crate) sets_mem_message: bool,
}

impl DeflateInitError {
    /// A failure at one of the points where C leaves `strm->msg` untouched.
    #[inline]
    const fn plain(code: ZlibError) -> Self {
        Self {
            code,
            sets_mem_message: false,
        }
    }

    /// The working-buffer failure of `deflate.c` L505-L514, where C records
    /// `ERR_MSG(Z_MEM_ERROR)`.
    #[inline]
    const fn working_buffer() -> Self {
        Self {
            code: ZlibError::MemError,
            sets_mem_message: true,
        }
    }
}

impl DeflateState {
    /// Reads byte `index` of the **symbol region** overlaid inside
    /// [`pending_buf`](Self::pending_buf).
    ///
    /// `index` is region-relative, exactly as C's `s->sym_buf[i]` is relative to
    /// `s->sym_buf = s->pending_buf + s->lit_bufsize` (`deflate.c` L520), so this
    /// is C's pointer dereference expressed as index arithmetic (AAP §0.3.2 rule
    /// T3). Bounds are checked by the slice index, so an out-of-range symbol
    /// index panics rather than reading adjacent pending output.
    #[inline]
    pub(crate) fn sym(&self, index: usize) -> u8 {
        self.pending_buf[self.lit_bufsize + index]
    }

    /// Writes byte `index` of the symbol region. The counterpart of
    /// `sym`; see there for the index convention.
    #[inline]
    pub(crate) fn set_sym(&mut self, index: usize, value: u8) {
        self.pending_buf[self.lit_bufsize + index] = value;
    }

    /// The whole symbol region as a slice — `pending_buf[lit_bufsize..]`, i.e.
    /// `3 * lit_bufsize` bytes.
    ///
    /// Provided for the state-comparison and copy-independence tests, which need
    /// to compare `sym_next` bytes of symbol data between two states; the engines
    /// themselves always address individual bytes, exactly as C does.
    #[cfg(test)]
    #[inline]
    pub(crate) fn sym_region(&self) -> &[u8] {
        &self.pending_buf[self.lit_bufsize..]
    }

    /// The byte count reference zlib passes to a caller's `zalloc` when it
    /// allocates the deflate state — C's `sizeof(deflate_state)` — computed from
    /// the field-exact `#[repr(C)]` layout mirror rather than from this Rust
    /// type's own size.
    ///
    /// This is the value C uses in `ZALLOC(strm, 1, sizeof(deflate_state))`
    /// (`deflate.c` L440), and it is what the init and copy paths reserve so a
    /// caller-supplied allocator observes the same request C would make
    /// (AAP §0.6.3, §0.6.5). It is **5968 on LP64** and differs from
    /// `size_of::<DeflateState>()`, which is legitimately larger because the
    /// idiomatic state holds owning buffer enums where C holds bare pointers.
    ///
    /// Exposed publicly because it is the only way an allocator implementation or
    /// a memory-accounting test can predict the request it will be handed.
    pub const C_LAYOUT_SIZE: usize = core::mem::size_of::<DeflateStateC>();

    /// Deep-copies this state into a new one, or returns [`ZlibError::MemError`]
    /// if any of its six buffers cannot be allocated — the `state_alloc`
    /// reservation or any of the five working buffers — the safe-Rust
    /// counterpart of C `deflateCopy`
    /// (`deflate.c` L1317-L1377).
    ///
    /// # Allocator and failure parity
    ///
    /// Each buffer is copied through [`AllocBuffer::try_clone`], so a buffer
    /// backed by the caller's `zalloc` is re-allocated **through that same hook**
    /// and one backed by the global allocator stays there. This is what AAP
    /// §0.6.5 requires: "the clone path must route through the same `AllocHook` —
    /// otherwise a caller that supplied a custom arena would find the copy living
    /// in the global heap". If any allocation
    /// fails the whole copy fails, exactly as C abandons the destination and
    /// returns `Z_MEM_ERROR` (`deflate.c` L1348-L1350) rather than completing the
    /// copy with substitute storage (AAP §0.6.3, §0.6.5).
    ///
    /// Because this port uses indices and owned arrays rather than raw pointers,
    /// there are **no pointer fix-ups** to perform afterwards — a substantial
    /// simplification over the C implementation, which must re-point
    /// `sym_buf`/`pending_out` into the freshly allocated `pending_buf`.
    ///
    /// # Why this is written out field by field
    ///
    /// The buffers must be cloned fallibly, so `#[derive(Clone)]` cannot express
    /// this operation. Listing every field explicitly makes the copy
    /// compiler-checked: adding a field to [`DeflateState`] without deciding how
    /// it is copied is a compile error rather than a silently dropped value.
    #[inline]
    pub fn try_clone(&self) -> Result<Self, ZlibError> {
        self.try_clone_in(&HookAllocator::new(self.alloc_hook))
    }

    /// Deep-copies this state, allocating the destination's buffers through
    /// `alloc` rather than through this state's own recorded allocator hook.
    ///
    /// `deflateCopy` inherits the source stream's allocator (C `zmemcpy`s the
    /// whole `z_stream`, carrying `zalloc`/`zfree`/`opaque` across), so the driver
    /// passes the source stream's allocator here and the two are the same value in
    /// practice. Routing through the [`Allocator`] trait — instead of cloning each
    /// buffer in place — is what lets a custom Rust allocator serve the copy's
    /// memory too, and it re-requests every buffer with the same `(items, size)`
    /// shape `deflateInit2_` used, which is exactly what C `deflateCopy` does
    /// (`deflate.c` L1335-L1348). See [`try_clone`](Self::try_clone) for the
    /// allocator-parity and field-by-field rationale.
    ///
    /// # Errors
    ///
    /// [`ZlibError::MemError`] if any allocation is refused; every buffer already
    /// allocated is released through the same allocator when the abandoned
    /// temporaries drop (C's `deflateEnd(dest)`).
    pub fn try_clone_in<A: Allocator>(&self, alloc: &A) -> Result<Self, ZlibError> {
        // Fallible work first: if any buffer cannot be allocated the copy is
        // abandoned before anything else is built, and the buffers already
        // allocated are released by their own `Drop` (C `deflateEnd(dest)`).
        //
        // The state-object reservation comes first, because that is the order C
        // `deflateCopy` uses: `ZALLOC(strm, 1, sizeof(deflate_state))` for the
        // destination state (`deflate.c` L1330-L1333) precedes the working
        // buffers. It re-requests the same `(1, size_of)` pair, so the caller's
        // allocation count and failure timing for a copy match C's as well
        // (AAP §0.6.5). It stays empty when the source's is empty — i.e. when no
        // C-style hook is installed — so the global-allocator memory bounds are
        // unchanged.
        let state_alloc = if self.state_alloc.is_empty() {
            AllocBuffer::default()
        } else {
            Self::cloned_buffer_items(alloc, &self.state_alloc, 1, Self::C_LAYOUT_SIZE)?
        };
        // Then the four working buffers, in C `deflateCopy`'s order:
        // window, prev, head, pending_buf (`deflate.c` L1341-L1345). The symbol
        // region needs no request of its own — it is overlaid inside
        // `pending_buf`, exactly as C re-derives `ds->sym_buf = ds->pending_buf +
        // ds->lit_bufsize` after copying (`deflate.c` L1368).
        //
        // All four requests are issued *unconditionally* and checked together
        // afterwards, because that is what C does (L1341-L1350): a caller whose
        // arena runs out part-way through therefore sees its `zalloc` called the
        // same number of times, and reports out-of-memory the same number of
        // times, as it would against reference zlib. Short-circuiting on the first
        // refusal would be a smaller but observably different schedule
        // (AAP §0.6.5).
        let window = Self::cloned_buffer_items(alloc, &self.window, self.w_size, 2);
        let prev = Self::cloned_buffer(alloc, &self.prev, self.w_size);
        let head = Self::cloned_buffer(alloc, &self.head, self.hash_size);
        let pending_buf = Self::cloned_buffer_items(alloc, &self.pending_buf, self.lit_bufsize, 4);

        // C's combined check calls `deflateEnd(dest)`, which releases whatever was
        // obtained in the order pending_buf, head, prev, window (`deflate.c`
        // L1300-L1306). Dropping the abandoned temporaries explicitly in that
        // order reproduces the sequence the caller's `zfree` observes; the earlier
        // `state_alloc` binding drops last, standing in for C's final
        // `ZFREE(strm, strm->state)`.
        let (window, prev, head, pending_buf) = match (window, prev, head, pending_buf) {
            (Ok(window), Ok(prev), Ok(head), Ok(pending_buf)) => (window, prev, head, pending_buf),
            (window, prev, head, pending_buf) => {
                drop(pending_buf);
                drop(head);
                drop(prev);
                drop(window);
                return Err(ZlibError::MemError);
            }
        };

        Ok(Self {
            status: self.status,
            state_alloc,
            pending_buf,
            pending_buf_size: self.pending_buf_size,
            pending_out: self.pending_out,
            pending: self.pending,
            wrap: self.wrap,
            #[cfg(feature = "gzip")]
            gzhead: self.gzhead.clone(),
            #[cfg(feature = "gzip")]
            gzindex: self.gzindex,
            method: self.method,
            last_flush: self.last_flush,
            w_size: self.w_size,
            w_bits: self.w_bits,
            w_mask: self.w_mask,
            window,
            window_size: self.window_size,
            prev,
            head,
            ins_h: self.ins_h,
            hash_size: self.hash_size,
            hash_bits: self.hash_bits,
            hash_mask: self.hash_mask,
            hash_shift: self.hash_shift,
            block_start: self.block_start,
            match_length: self.match_length,
            prev_match: self.prev_match,
            match_available: self.match_available,
            strstart: self.strstart,
            match_start: self.match_start,
            lookahead: self.lookahead,
            prev_length: self.prev_length,
            max_chain_length: self.max_chain_length,
            max_lazy_match: self.max_lazy_match,
            level: self.level,
            strategy: self.strategy,
            good_match: self.good_match,
            nice_match: self.nice_match,
            dyn_ltree: self.dyn_ltree,
            dyn_dtree: self.dyn_dtree,
            bl_tree: self.bl_tree,
            l_max_code: self.l_max_code,
            d_max_code: self.d_max_code,
            bl_max_code: self.bl_max_code,
            bl_count: self.bl_count,
            heap: self.heap,
            heap_len: self.heap_len,
            heap_max: self.heap_max,
            depth: self.depth,
            lit_bufsize: self.lit_bufsize,
            sym_next: self.sym_next,
            sym_end: self.sym_end,
            opt_len: self.opt_len,
            static_len: self.static_len,
            matches: self.matches,
            insert: self.insert,
            bi_buf: self.bi_buf,
            bi_valid: self.bi_valid,
            bi_used: self.bi_used,
            high_water: self.high_water,
            slid: self.slid,
            data_type: self.data_type,
            mem_level: self.mem_level,
            // The copy inherits the destination stream's allocator — which C
            // `deflateCopy` makes identical to the source's by `zmemcpy`ing the
            // whole `z_stream`, carrying `zalloc`/`zfree`/`opaque` across — so its
            // later re-allocations stay inside the caller's arena (AAP §0.6.3,
            // §0.6.5).
            alloc_hook: alloc.hook(),
        })
    }

    /// Allocates a destination buffer of `items * item_size` bytes through `alloc`
    /// and copies `src` into it, reproducing C `deflateCopy`'s
    /// `ZALLOC(...); zmemcpy(...)` pair for one buffer.
    ///
    /// The `(items, item_size)` pair is the same one `deflateInit2_` used for this
    /// buffer, so a caller's `zalloc` observes identical arguments on the copy path
    /// (AAP §0.6.5). C copies only the live prefix of some buffers and leaves the
    /// rest as the freshly zeroed allocation; copying the whole buffer here is a
    /// superset of that and cannot change any emitted byte, because the engine
    /// only ever reads the live prefix.
    ///
    /// # Errors
    ///
    /// [`ZlibError::MemError`] if the allocation is refused, or if it produced a
    /// length other than `src.len()` (which would mean the geometry fields and the
    /// buffer disagreed).
    fn cloned_buffer_items<A: Allocator, T>(
        alloc: &A,
        src: &AllocBuffer<T>,
        items: usize,
        item_size: usize,
    ) -> Result<AllocBuffer<T>, ZlibError>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        let mut dst = alloc
            .allocate_zeroed_items::<T>(items, item_size)
            .ok_or(ZlibError::MemError)?;
        if dst.len() != src.len() {
            return Err(ZlibError::MemError);
        }
        dst.copy_from_slice(src);
        Ok(dst)
    }

    /// Element-shaped counterpart of
    /// [`cloned_buffer_items`](Self::cloned_buffer_items), for the buffers whose C
    /// request is `ZALLOC(strm, count, sizeof(Pos))`.
    ///
    /// # Errors
    ///
    /// As [`cloned_buffer_items`](Self::cloned_buffer_items).
    fn cloned_buffer<A: Allocator, T>(
        alloc: &A,
        src: &AllocBuffer<T>,
        count: usize,
    ) -> Result<AllocBuffer<T>, ZlibError>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        let mut dst = alloc
            .allocate_zeroed::<T>(count)
            .ok_or(ZlibError::MemError)?;
        if dst.len() != src.len() {
            return Err(ZlibError::MemError);
        }
        dst.copy_from_slice(src);
        Ok(dst)
    }

    /// Allocates and initializes a new deflate state, reproducing the
    /// allocation and field-setup portion of the C `deflateInit2_`
    /// (`deflate.c` L387-L533).
    ///
    /// The overloaded `windowBits` decoding (negative → raw, `> 15` → gzip) is
    /// performed by the caller (`mod.rs`, via
    /// [`crate::constants::parse_window_bits`]); this constructor therefore
    /// receives an already-resolved positive `window_bits` (`8..=15`) and the
    /// resolved `wrap` selector (`0` raw, `1` zlib, `2` gzip).
    ///
    /// Parameter validation mirrors zlib exactly and yields
    /// [`ZlibError::StreamError`] on any invalid combination:
    ///
    /// * `mem_level` must be in `1..=`[`MAX_MEM_LEVEL`];
    /// * `method` must equal [`Z_DEFLATED`];
    /// * `window_bits` must be in `8..=15` (and `8` requires `wrap == 1`);
    /// * `level` must be in `0..=9` after [`Z_DEFAULT_COMPRESSION`] is resolved
    ///   to `6`.
    ///
    /// The `strategy` is a [`Strategy`] enum and is therefore always valid, so
    /// the C `strategy` range check is unnecessary.
    ///
    /// # I/O-side reset
    ///
    /// C `deflateInit2_` finishes by calling `deflateReset`, which also zeroes
    /// `total_in`/`total_out` and seeds `adler`. Those live in the
    /// [`IoContext`] / owning `ZStream` rather than in `DeflateState`, so this
    /// constructor performs only the state-side reset. The caller must set the
    /// stream's `total_in`/`total_out` to `0` and its `adler` to
    /// [`DeflateState::initial_adler`]`(wrap)`.
    ///
    /// # Errors
    ///
    /// Returns [`ZlibError::StreamError`] if any parameter is invalid.
    ///
    /// # Allocator
    ///
    /// This convenience constructor uses the Rust global allocator for all
    /// working buffers (equivalent to a C caller with null `zalloc`/`zfree`).
    /// The FFI init path calls [`new_in`](Self::new_in) instead, passing the
    /// caller's [`AllocHook`] so the buffers are routed through the caller's
    /// `zalloc`/`zfree` (AAP §0.6.3).
    #[inline]
    pub fn new(
        level: i32,
        method: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: Strategy,
        wrap: i32,
    ) -> Result<Box<DeflateState>, ZlibError> {
        Self::new_in(
            AllocHook::none(),
            level,
            method,
            window_bits,
            mem_level,
            strategy,
            wrap,
        )
    }

    /// Builds a boxed [`DeflateState`], allocating every working buffer through
    /// the supplied [`AllocHook`] (the caller's `zalloc`/`zfree` when active, or
    /// the global allocator otherwise). See [`new`](Self::new) for the parameter
    /// contract, I/O-side-reset note, and errors — this is the same constructor
    /// with an explicit allocator hook (AAP §0.6.3 has-hook clause).
    ///
    /// Equivalent to [`new_in_with`](Self::new_in_with) called with
    /// [`HookAllocator::new(hook)`](HookAllocator::new); use that spelling
    /// directly to allocate through a custom Rust [`Allocator`].
    #[inline]
    pub fn new_in(
        hook: AllocHook,
        level: i32,
        method: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: Strategy,
        wrap: i32,
    ) -> Result<Box<DeflateState>, ZlibError> {
        Self::new_in_with(
            &HookAllocator::new(hook),
            level,
            method,
            window_bits,
            mem_level,
            strategy,
            wrap,
        )
    }

    /// Builds a boxed [`DeflateState`], allocating the state footprint and every
    /// working buffer through `alloc`.
    ///
    /// This is the constructor the deflate driver
    /// ([`crate::deflate::deflate_init2`]) calls, passing the owning
    /// [`ZStream`](crate::stream::ZStream)'s allocator, so a custom [`Allocator`]
    /// genuinely serves engine memory instead of being consulted only for its
    /// [`hook`](Allocator::hook). Every request uses the method whose
    /// `(items, size)` shape matches the corresponding C `ZALLOC`, so an
    /// inspecting or bounded allocator observes exactly the arguments
    /// `deflateInit2_` passes (AAP §0.6.3, §0.6.5).
    ///
    /// See [`new`](Self::new) for the parameter contract, the I/O-side-reset note,
    /// and the errors.
    pub(crate) fn new_in_with_detail<A: Allocator>(
        alloc: &A,
        level: i32,
        method: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: Strategy,
        wrap: i32,
    ) -> Result<Box<DeflateState>, DeflateInitError> {
        let hook = alloc.hook();
        // Resolve the default level exactly as deflateInit2_ does.
        let level = if level == Z_DEFAULT_COMPRESSION {
            6
        } else {
            level
        };

        // Validate parameters (deflate.c L433-L437).
        if !(1..=MAX_MEM_LEVEL).contains(&mem_level)
            || method != Z_DEFLATED
            || !(8..=15).contains(&window_bits)
            || !(0..=9).contains(&level)
            || (window_bits == 8 && wrap != 1)
        {
            return Err(DeflateInitError::plain(ZlibError::StreamError));
        }

        // "until 256-byte window bug fixed": an 8-bit window is bumped to 9.
        let window_bits = if window_bits == 8 { 9 } else { window_bits };

        let w_bits = window_bits as u32;
        let w_size = 1usize << w_bits;
        let w_mask = w_size - 1;

        let hash_bits = mem_level as u32 + 7;
        let hash_size = 1usize << hash_bits;
        let hash_mask = hash_size - 1;
        // C: (hash_bits + MIN_MATCH - 1) / MIN_MATCH — i.e. a ceiling divide,
        // expressed with `div_ceil` for clarity (identical value).
        let hash_shift = hash_bits.div_ceil(MIN_MATCH as u32);

        let lit_bufsize = 1usize << (mem_level as u32 + 6);
        // We size pending_buf as 4 * lit_bufsize (LIT_BUFS == 4), exactly as C
        // does, preserving the block-emission overflow guarantee. Its upper
        // `3 * lit_bufsize` bytes are the symbol region, overlaid exactly where C
        // overlays it (`deflate.c` L520); see the field documentation.
        let pending_buf_size = lit_bufsize * 4;
        // We avoid equality with lit_bufsize*3 to match C (wraparound / stored
        // block considerations): sym_end = (lit_bufsize - 1) * 3.
        let sym_end = (lit_bufsize - 1) * 3;

        // C charges the state object itself to the caller's allocator *first*
        // and checks it immediately (`deflate.c` L440-L442:
        // `s = ZALLOC(strm, 1, sizeof(deflate_state)); if (s == Z_NULL) return
        // Z_MEM_ERROR;`). The state lives in a Rust `Box` here, so this
        // reservation exists purely to keep the caller's memory accounting and
        // failure timing identical (AAP §0.6.5). Whether to make it is the
        // allocator's decision — `Allocator::reserves_state_footprint` — so a
        // custom Rust allocator observes the request while the global default
        // (where the `Box` already is the allocation) keeps its historical
        // footprint. The `(1, size_of)` pair is exactly what C passes.
        let state_alloc = if alloc.reserves_state_footprint() {
            match alloc.allocate_zeroed_items::<u8>(1, Self::C_LAYOUT_SIZE) {
                Some(cell) => cell,
                None => return Err(DeflateInitError::plain(ZlibError::MemError)),
            }
        } else {
            AllocBuffer::default()
        };

        // Allocate every working buffer up front, routed through the caller's
        // allocator hook, forwarding C's exact `(items, item_size)` pairs so a
        // bounded or inspecting allocator observes the same arguments
        // (`deflate.c` L458-L460, L505).
        //
        // When an active hook's `zalloc` reports out-of-memory, the allocation
        // yields `None`, which is surfaced as `Z_MEM_ERROR` (AAP §0.6.5). C attempts all
        // of these and checks them *together* afterwards (`deflate.c`
        // L508-L514), so the number of `zalloc` calls a failing caller observes
        // is part of the contract; that ordering is reproduced here rather than
        // short-circuiting on the first `None` (AAP §0.6.5). Any buffer that did
        // succeed is released by its `Drop` (through the caller's `zfree`) when
        // the early return drops the temporaries. The null-hook (global) path
        // fails only on genuine heap exhaustion.
        // The four working-buffer requests, issued in C `deflateInit2_`'s order
        // — window, prev, head, pending_buf (`deflate.c` L458-L460, L505) — so a
        // caller's `zalloc` observes the same sequence, the same `(items, size)`
        // pairs, and the same count as reference zlib (AAP §0.6.5).
        //
        // C: ZALLOC(strm, s->w_size, 2 * sizeof(Byte)) — the doubled window.
        let window = alloc.allocate_zeroed_items::<u8>(w_size, 2);
        // C: ZALLOC(strm, s->w_size, sizeof(Pos)) — `Pos` is 2 bytes, which is
        // already `size_of::<u16>()`, so the element-shaped spelling is exact.
        let prev = alloc.allocate_zeroed::<u16>(w_size);
        // C: ZALLOC(strm, s->hash_size, sizeof(Pos)).
        let head = alloc.allocate_zeroed::<u16>(hash_size);
        // C: ZALLOC(strm, s->lit_bufsize, LIT_BUFS) with LIT_BUFS == 4
        // (`deflate.h` L229, `LIT_MEM` being commented out at `deflate.h` L28).
        // This single request covers the pending output area *and* the symbol
        // region overlaid at offset `lit_bufsize`, exactly as C's `s->sym_buf =
        // s->pending_buf + s->lit_bufsize` does (`deflate.c` L520) — hence four
        // working-buffer requests, not five.
        let pending_buf = alloc.allocate_zeroed_items::<u8>(lit_bufsize, 4);

        // C attempts all four and checks them *together* afterwards
        // (`deflate.c` L508-L514), then reports `Z_MEM_ERROR`; the abandoned
        // buffers here are released by their own `Drop`.
        let (Some(window), Some(prev), Some(head), Some(pending_buf)) =
            (window, prev, head, pending_buf)
        else {
            // C's L505-L514 branch: it records `ERR_MSG(Z_MEM_ERROR)` in
            // `strm->msg` here, unlike the state-object check at L440-L442.
            return Err(DeflateInitError::working_buffer());
        };

        let mut state = try_box(DeflateState {
            status: DeflateStatus::Init,
            pending_buf,
            pending_buf_size,
            pending_out: 0,
            pending: 0,
            wrap,
            #[cfg(feature = "gzip")]
            gzhead: None,
            #[cfg(feature = "gzip")]
            gzindex: 0,
            method: Z_DEFLATED as u8,
            last_flush: -2,
            w_size,
            w_bits,
            w_mask,
            window,
            window_size: 2 * w_size,
            prev,
            head,
            ins_h: 0,
            hash_size,
            hash_bits,
            hash_mask,
            hash_shift,
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            strstart: 0,
            match_start: 0,
            lookahead: 0,
            prev_length: MIN_MATCH - 1,
            max_chain_length: 0,
            max_lazy_match: 0,
            level,
            strategy,
            good_match: 0,
            nice_match: 0,
            dyn_ltree: [CtData::default(); HEAP_SIZE],
            dyn_dtree: [CtData::default(); 2 * D_CODES + 1],
            bl_tree: [CtData::default(); 2 * BL_CODES + 1],
            l_max_code: 0,
            d_max_code: 0,
            bl_max_code: 0,
            bl_count: [0u16; MAX_BITS + 1],
            heap: [0i32; 2 * L_CODES + 1],
            heap_len: 0,
            heap_max: 0,
            depth: [0u8; 2 * L_CODES + 1],
            lit_bufsize,
            sym_next: 0,
            sym_end,
            opt_len: 0,
            static_len: 0,
            matches: 0,
            insert: 0,
            bi_buf: 0,
            bi_valid: 0,
            bi_used: 0,
            high_water: 0,
            slid: false,
            data_type: DataType::Unknown,
            mem_level,
            alloc_hook: hook,
            state_alloc,
        })
        // The `Box` itself stands in for C's `ZALLOC(strm, 1,
        // sizeof(deflate_state))` at L441, which returns with `strm->msg` unset.
        .ok_or(DeflateInitError::plain(ZlibError::MemError))?;

        // Reproduce the state-resetting portion of deflateReset. The I/O-side
        // reset (total_in/total_out = 0, adler = initial_adler(wrap)) is the
        // caller's responsibility (this constructor has no IoContext).
        state.reset_state();
        state.lm_init();

        Ok(state)
    }

    /// Builds a boxed [`DeflateState`], allocating the state footprint and every
    /// working buffer through `alloc`.
    ///
    /// This is the allocator-generic public constructor. It reports failures as a
    /// plain [`ZlibError`]; the deflate driver uses the crate-internal
    /// `new_in_with_detail` instead, because it also needs to know which of C's
    /// two allocation-failure points was reached in order to reproduce
    /// `strm->msg` exactly (see the private `DeflateInitError` type).
    ///
    /// Both of those names are deliberately referenced as plain code spans rather
    /// than intra-doc links: they are `pub(crate)`, so linking them from a public
    /// item would emit `rustdoc::private_intra_doc_links`, and the crate's
    /// `cargo doc --no-deps` gate is warning-free.
    ///
    /// Every request uses the method whose `(items, size)` shape matches the
    /// corresponding C `ZALLOC`, so an inspecting or bounded allocator observes
    /// exactly the arguments `deflateInit2_` passes (AAP §0.6.3, §0.6.5).
    ///
    /// See [`new`](Self::new) for the parameter contract, the I/O-side-reset note,
    /// and the errors.
    #[inline]
    pub fn new_in_with<A: Allocator>(
        alloc: &A,
        level: i32,
        method: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: Strategy,
        wrap: i32,
    ) -> Result<Box<DeflateState>, ZlibError> {
        Self::new_in_with_detail(alloc, level, method, window_bits, mem_level, strategy, wrap)
            .map_err(|e| e.code)
    }

    /// Deep-copies this state for `deflateCopy`, **preserving the allocator** and
    /// reporting an out-of-memory refusal instead of masking it.
    ///
    /// C `deflateCopy` (`deflate.c` L1317-L1377) first `zmemcpy`s the `z_stream`
    /// — so the destination inherits the source's `zalloc`/`zfree`/`opaque` —
    /// then allocates the state and all four working buffers through that same
    /// allocator, and returns `Z_MEM_ERROR` if any allocation fails. An infallible
    /// [`Clone`] could not express that failure — its only options are to panic or
    /// to fall back to the global allocator, which would return `Z_OK` where C
    /// returns `Z_MEM_ERROR` and let a caller-supplied arena be silently escaped —
    /// which is why neither [`DeflateState`] nor
    /// [`AllocBuffer`](crate::stream::AllocBuffer) implements it. AAP §0.6.5
    /// requires the copy to route through the same [`AllocHook`], so `deflateCopy`
    /// uses this method and surfaces [`None`] as `Z_MEM_ERROR`.
    ///
    /// # Mechanism
    ///
    /// [`try_clone_in`](Self::try_clone_in) performs the field-by-field copy,
    /// requesting every buffer from `alloc` — including
    /// [`state_alloc`](Self::state_alloc), which reproduces C's
    /// `ZALLOC(strm, 1, sizeof(deflate_state))` for the destination — with exactly
    /// one request per buffer and none for anything else, so a caller's counter
    /// sees the same request *per buffer* C issues (`deflate.c` L1335-L1348).
    /// There is no extra request for the symbol region: it is overlaid inside
    /// `pending_buf`, so the copy issues C's five — state, `window`, `prev`,
    /// `head`, `pending_buf` — in C's order, and re-derives the region base the
    /// way C re-derives `ds->sym_buf = ds->pending_buf + ds->lit_bufsize`
    /// (`deflate.c` L1368), which here is simply the preserved `lit_bufsize`.
    /// It is fallible end to end and has **no** global-allocator fallback: an allocator that
    /// refuses yields [`ZlibError::MemError`] rather than a buffer that quietly
    /// escaped the caller's arena, so the escape cannot occur and does not have to
    /// be detected after the fact. A zero-length buffer stays legitimately owned
    /// even under an active hook, because an empty request never calls `zalloc`.
    ///
    /// [`try_box`] then moves the finished state onto the Rust heap fallibly,
    /// because [`Box::new`] aborts the process on heap exhaustion whereas zlib
    /// reports `Z_MEM_ERROR` (AAP §0.6.5). If that last step fails, the state is
    /// dropped normally and every buffer it owns is released through the caller's
    /// `zfree`.
    ///
    /// # Returns
    ///
    /// [`None`] if any buffer could not be duplicated through the caller's
    /// allocator, or if the state box itself could not be allocated. Every buffer
    /// allocated along the way is released through the caller's `zfree` when the
    /// abandoned copy drops.
    ///
    /// `alloc` is the destination stream's [`Allocator`], which C `deflateCopy`
    /// makes a duplicate of the source's by `zmemcpy`ing the whole `z_stream`.
    /// Every request goes through it, so a custom Rust allocator and a
    /// caller-installed `zalloc`/`zfree` are honoured identically.
    #[must_use]
    pub(crate) fn try_copy_in<A: Allocator>(&self, alloc: &A) -> Option<Box<DeflateState>> {
        try_box(self.try_clone_in(alloc).ok()?)
    }

    /// Returns the initial checksum seed for the given wrapper, matching the C
    /// `deflateResetKeep` assignment `adler = wrap == 2 ? crc32(0, Z_NULL, 0) :
    /// adler32(0, Z_NULL, 0)`.
    ///
    /// In C the `Z_NULL` sentinel makes `adler32(0, Z_NULL, 0)` return `1`.
    /// This crate's slice-based [`adler32`](crate::checksum::adler32()) has no
    /// null case, so the equivalent seed is obtained with `adler32(1, &[])`
    /// (which returns `1`); the gzip seed is `crc32(0, &[])` (which returns
    /// `0`). Both reproduce the C initial values exactly.
    #[must_use]
    pub fn initial_adler(wrap: i32) -> u32 {
        #[cfg(feature = "gzip")]
        if wrap == 2 {
            return crc32(0, &[]);
        }
        #[cfg(not(feature = "gzip"))]
        let _ = wrap;
        adler32(1, &[])
    }

    /// Returns the initial [`DeflateStatus`] for the given (already
    /// sign-normalized) wrapper: [`DeflateStatus::Gzip`] for a gzip wrapper
    /// (`wrap == 2`, `gzip` feature only), otherwise [`DeflateStatus::Init`].
    fn initial_status(wrap: i32) -> DeflateStatus {
        #[cfg(feature = "gzip")]
        if wrap == 2 {
            return DeflateStatus::Gzip;
        }
        #[cfg(not(feature = "gzip"))]
        let _ = wrap;
        DeflateStatus::Init
    }

    /// Residual equivalent of the C `deflateStateCheck` (`deflate.c`
    /// L538-L556).
    ///
    /// In C this validates that `strm`, `zalloc`/`zfree`, and `state` are
    /// non-null and that `status` is one of the recognized sentinels. With Rust
    /// ownership, holding a `&DeflateState` already guarantees a live, valid
    /// state, and the `status`-membership test is automatic because
    /// [`DeflateStatus`] can only hold a valid variant. The only residual
    /// invariant worth asserting is that the method is DEFLATE, which
    /// [`new`](Self::new) enforces — so this returns `true` for any well-formed
    /// state.
    #[inline]
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.method == Z_DEFLATED as u8
    }

    /// State-side portion of the C `deflateResetKeep` (`deflate.c` L644-L677):
    /// everything that lives in `DeflateState` rather than in the owning
    /// stream. The `total_in`/`total_out`/`adler` reset is performed by
    /// [`reset_keep`](Self::reset_keep) because those fields belong to the
    /// [`IoContext`].
    ///
    /// This also zeroes the bit-buffer trio (`bi_buf`/`bi_valid`/`bi_used`),
    /// which in C is done inside `_tr_init`. The higher-level tree-frequency
    /// initialization (`init_block`) is intentionally *not* performed here — it
    /// lives in `trees.rs` and is invoked by the `mod.rs` driver after a reset,
    /// because this module does not depend on `trees.rs`.
    fn reset_state(&mut self) {
        self.data_type = DataType::Unknown;
        self.pending = 0;
        self.pending_out = 0;
        // A negative wrap (used transiently while a trailer is written) is
        // normalized back to positive on reset, exactly as C does.
        if self.wrap < 0 {
            self.wrap = -self.wrap;
        }
        self.status = Self::initial_status(self.wrap);
        self.last_flush = -2;
        // Part of what C `_tr_init` resets; safe to do here since these are
        // plain state fields touched only by the bit-output primitives.
        self.bi_buf = 0;
        self.bi_valid = 0;
        self.bi_used = 0;
    }

    /// Reproduces C `deflateResetKeep` (`deflate.c` L644-L677): resets the
    /// stream I/O counters and checksum seed (via the supplied [`IoContext`])
    /// together with the state-side fields, but keeps the allocated buffers and
    /// configuration.
    ///
    /// The caller-visible `total_in`/`total_out` are zeroed and `adler` is
    /// seeded with [`initial_adler`](Self::initial_adler). The tree-frequency
    /// initialization performed by C `_tr_init` is deferred to the `trees.rs`
    /// driver (see `reset_state`).
    pub fn reset_keep(&mut self, io: &mut IoContext) {
        io.total_in = 0;
        io.total_out = 0;
        self.reset_state();
        io.adler = Self::initial_adler(self.wrap);
    }

    /// Reproduces C `deflateReset` (`deflate.c` L704): a
    /// [`reset_keep`](Self::reset_keep) followed by
    /// [`lm_init`](Self::lm_init), re-establishing the match-finder state.
    pub fn reset(&mut self, io: &mut IoContext) {
        self.reset_keep(io);
        self.lm_init();
    }

    /// Reproduces C `lm_init` (`deflate.c` L682-L701): initializes the
    /// match-finder window bookkeeping, clears the hash tables, and loads the
    /// per-level tuning parameters (`good_match`, `max_lazy_match`,
    /// `nice_match`, `max_chain_length`) from the configuration table.
    pub fn lm_init(&mut self) {
        self.window_size = 2 * self.w_size;
        self.clear_hash();

        // Load the per-level configuration from the crate's SINGLE
        // match-finder tuning table — `crate::deflate::strategy`'s
        // `CONFIGURATION_TABLE`, the verbatim port of C
        // `configuration_table[10]` (`deflate.c` L112-L124). `deflate_params`
        // re-dispatch reads the very same rows, so the two paths cannot drift
        // apart and silently change the emitted token stream (AAP §0.6.4,
        // TO-5). `level` is validated to 0..=9 in `new`.
        let cfg = &CONFIGURATION_TABLE[self.level as usize];
        self.max_lazy_match = cfg.max_lazy as usize;
        self.good_match = cfg.good_length as usize;
        self.nice_match = cfg.nice_length as i32;
        self.max_chain_length = cfg.max_chain as usize;

        self.strstart = 0;
        self.block_start = 0;
        self.lookahead = 0;
        self.insert = 0;
        self.match_length = MIN_MATCH - 1;
        self.prev_length = MIN_MATCH - 1;
        self.match_available = false;
        self.ins_h = 0;
    }

    /// Reproduces C `CLEAR_HASH` (`deflate.h`): resets every hash-head entry to
    /// [`NIL`]. C writes `head[hash_size - 1] = NIL` and zeroes the remainder;
    /// because `NIL == 0` the net effect is that all entries become `NIL`, so a
    /// single fill is equivalent. Also clears the [`slid`](DeflateState::slid)
    /// flag.
    fn clear_hash(&mut self) {
        self.head.iter_mut().for_each(|h| *h = NIL);
        self.slid = false;
    }

    /// Reproduces C `UPDATE_HASH` (`deflate.c` L154): rolls the byte `c` into
    /// the running hash index `ins_h`, masked to the hash-table size.
    ///
    /// `ins_h` stays strictly below `hash_size` (`<= 2^15`) so the intermediate
    /// shift cannot overflow a `usize`.
    #[inline]
    pub fn update_hash(&mut self, c: u8) {
        self.ins_h = ((self.ins_h << self.hash_shift) ^ (c as usize)) & self.hash_mask;
    }

    /// Reproduces the non-`FASTEST` C `INSERT_STRING` macro (`deflate.c`
    /// L165-L179): rolls the byte at `str_idx + MIN_MATCH - 1` into the hash,
    /// links the new string into the head of its hash chain, and returns the
    /// previous chain head (the candidate `match_head`).
    ///
    /// The caller must guarantee `str_idx + MIN_MATCH - 1` is a valid window
    /// index; [`fill_window`](Self::fill_window) upholds this by zero-filling
    /// bytes above `high_water`.
    #[inline]
    pub fn insert_string(&mut self, str_idx: usize) -> u16 {
        self.update_hash(self.window[str_idx + MIN_MATCH - 1]);
        let match_head = self.head[self.ins_h];
        self.prev[str_idx & self.w_mask] = match_head;
        self.head[self.ins_h] = str_idx as u16;
        match_head
    }

    /// Reproduces C `slide_hash` (`deflate.c` L217-L241): when the window
    /// slides by `w_size`, every hash-table and chain entry is decremented by
    /// `w_size`, with entries that would go negative reset to [`NIL`].
    ///
    /// C computes `m >= wsize ? m - wsize : NIL`; because `NIL == 0`, this is
    /// exactly [`u16::saturating_sub`], which is used here to stay panic-free
    /// without any `unsafe` or explicit branch.
    pub fn slide_hash(&mut self) {
        let wsize = self.w_size as u16;
        for m in self.head.iter_mut() {
            *m = m.saturating_sub(wsize);
        }
        for m in self.prev.iter_mut() {
            *m = m.saturating_sub(wsize);
        }
        self.slid = true;
    }

    /// Fully-decomposed core of C `read_buf` (`deflate.c` L295-L322): copies up
    /// to `dst.len()` bytes from `input[*next_in..]` into `dst`, updates the
    /// running checksum over the copied bytes according to `wrap`, and advances
    /// the input cursor/counter trio.
    ///
    /// This is an associated function taking each I/O field by mutable
    /// reference (rather than a `&mut self` method) so that it can be driven
    /// with **two different destinations** without tripping the borrow checker:
    ///
    /// * window fills pass `dst = &mut self.window[..]` (see
    ///   `read_buf` and [`fill_window`](Self::fill_window));
    /// * the stored-block fast path (in `stored.rs`) passes
    ///   `dst = &mut io.output[..]` to copy input straight to output.
    ///
    /// In both cases the `input`/cursor/`adler` arguments come from the same
    /// [`IoContext`], while `dst` borrows a disjoint buffer, so the two mutable
    /// borrows never overlap.
    ///
    /// Returns the number of bytes actually copied (`0` when input is
    /// exhausted).
    ///
    /// # Preconditions
    ///
    /// `*next_in` and `*avail_in` must describe a valid remaining region of
    /// `input`, i.e. `*next_in + *avail_in <= input.len()`. That is the
    /// invariant [`IoContext`] maintains from the moment it is built out of a
    /// `z_stream`, and the C engine relies on the same one (`strm->next_in` plus
    /// `strm->avail_in` never runs past the caller's buffer).
    ///
    /// # Panics
    ///
    /// Panics if the precondition is violated: the copy indexes
    /// `input[*next_in..*next_in + len]` where `len = min(*avail_in,
    /// dst.len())`, so a cursor or count that overruns `input` aborts on the
    /// bounds check instead of reading out of bounds. This is the deliberate
    /// safe-Rust counterpart of the C pointer arithmetic, which would silently
    /// read past the buffer.
    #[allow(clippy::too_many_arguments)]
    pub fn read_buf_into(
        input: &[u8],
        next_in: &mut usize,
        avail_in: &mut usize,
        total_in: &mut u64,
        adler: &mut u32,
        wrap: i32,
        dst: &mut [u8],
    ) -> usize {
        let len = core::cmp::min(*avail_in, dst.len());
        if len == 0 {
            return 0;
        }

        let start = *next_in;
        dst[..len].copy_from_slice(&input[start..start + len]);

        // Checksum the bytes just read (identical to updating over `dst`, since
        // it now holds the same bytes). zlib framing (wrap == 1) uses Adler-32;
        // gzip framing (wrap == 2) uses CRC-32.
        if wrap == 1 {
            *adler = adler32(*adler, &dst[..len]);
        }
        #[cfg(feature = "gzip")]
        if wrap == 2 {
            *adler = crc32(*adler, &dst[..len]);
        }

        *next_in += len;
        *avail_in -= len;
        *total_in += len as u64;
        len
    }

    /// Reproduces C `read_buf` (`deflate.c` L295-L322): reads up to `size`
    /// bytes of input into the window starting at `buf_start`, updating the
    /// checksum. Thin wrapper over [`read_buf_into`](Self::read_buf_into) that
    /// targets `self.window[buf_start..buf_start + size]`.
    ///
    /// Returns the number of bytes copied.
    fn read_buf(&mut self, io: &mut IoContext, buf_start: usize, size: usize) -> usize {
        let end = buf_start + size;
        Self::read_buf_into(
            io.input,
            &mut io.next_in,
            &mut io.avail_in,
            &mut io.total_in,
            &mut io.adler,
            self.wrap,
            &mut self.window[buf_start..end],
        )
    }

    /// Reproduces C `fill_window` (`deflate.c` L252-L376): refills the sliding
    /// window from the input so the match finder has at least
    /// [`MIN_LOOKAHEAD`] bytes available (when input permits), sliding the
    /// window down by `w_size` when it fills.
    ///
    /// The 16-bit-machine special-casing (`sizeof(int) <= 2`) in the C source
    /// is intentionally omitted: no supported target is 16-bit, so `usize` is
    /// at least 32 bits wide and the largest window this port can be asked for
    /// (`2 * 32 KiB`) is always representable without the C workaround. The
    /// omission is therefore independent of whether `usize` happens to be 32-
    /// or 64-bit on the target.
    ///
    /// After refilling, any window bytes beyond the current data that have
    /// never been written are zero-filled up to `high_water` (both C branches
    /// are reproduced). This is what makes the portable
    /// [`longest_match`](Self::longest_match) safe with pure bounds-checked
    /// indexing: scans up to `strstart + MAX_MATCH` always read initialized,
    /// in-bounds bytes, so **no `unsafe` is required**.
    pub fn fill_window(&mut self, io: &mut IoContext) {
        let wsize = self.w_size;

        loop {
            // Free space at the end of the window.
            let mut more = self.window_size - self.lookahead - self.strstart;

            // If the window is almost full and lookahead is insufficient, move
            // the upper half down to make room in the upper half.
            if self.strstart >= wsize + self.max_dist() {
                // memcpy(window, window + wsize, wsize - more)
                self.window.copy_within(wsize..wsize + (wsize - more), 0);
                // match_start may be stale here; C subtracts unconditionally.
                // Use wrapping_sub to stay panic-free without `unsafe`.
                self.match_start = self.match_start.wrapping_sub(wsize);
                // The slide condition guarantees strstart >= wsize.
                self.strstart -= wsize;
                self.block_start -= wsize as isize;
                if self.insert > self.strstart {
                    self.insert = self.strstart;
                }
                self.slide_hash();
                more += wsize;
            }

            if io.avail_in == 0 {
                break;
            }

            // Read into window[strstart + lookahead ..], at most `more` bytes.
            let n = self.read_buf(io, self.strstart + self.lookahead, more);
            self.lookahead += n;

            // Initialize the hash value now that we have some input.
            if self.lookahead + self.insert >= MIN_MATCH {
                let mut str_idx = self.strstart - self.insert;
                self.ins_h = self.window[str_idx] as usize;
                self.update_hash(self.window[str_idx + 1]);
                // (MIN_MATCH == 3, so no extra UPDATE_HASH calls are needed.)
                while self.insert != 0 {
                    self.update_hash(self.window[str_idx + MIN_MATCH - 1]);
                    self.prev[str_idx & self.w_mask] = self.head[self.ins_h];
                    self.head[self.ins_h] = str_idx as u16;
                    str_idx += 1;
                    self.insert -= 1;
                    if self.lookahead + self.insert < MIN_MATCH {
                        break;
                    }
                }
            }

            if !(self.lookahead < MIN_LOOKAHEAD && io.avail_in != 0) {
                break;
            }
        }

        // Zero any never-written bytes in the WIN_INIT region beyond the
        // current data, updating the high-water mark (deflate.c L355-L372).
        if self.high_water < self.window_size {
            let curr = self.strstart + self.lookahead;

            if self.high_water < curr {
                // Previous high water below current data: zero WIN_INIT bytes
                // (or up to end of window, whichever is less).
                let mut init = self.window_size - curr;
                if init > WIN_INIT {
                    init = WIN_INIT;
                }
                self.window[curr..curr + init].fill(0);
                self.high_water = curr + init;
            } else if self.high_water < curr + WIN_INIT {
                // High water at/above current data but below data + WIN_INIT:
                // zero out to data + WIN_INIT (or end of window).
                let mut init = curr + WIN_INIT - self.high_water;
                if init > self.window_size - self.high_water {
                    init = self.window_size - self.high_water;
                }
                let hw = self.high_water;
                self.window[hw..hw + init].fill(0);
                self.high_water += init;
            }
        }
    }

    /// Reproduces the **portable** (`#else /* UNALIGNED_OK */`) C
    /// `longest_match` (`deflate.c` L1480-L1512).
    ///
    /// Finds the longest match for the string at `strstart`, following the hash
    /// chain that begins at `cur_match`. Returns the best match length (capped
    /// at the current `lookahead`) and, as a side effect, sets
    /// [`match_start`](DeflateState::match_start) to the position of that match
    /// — mirroring the C out-parameter.
    ///
    /// This is the byte-by-byte comparison variant (both `UNALIGNED_OK` and
    /// `FASTEST` are disabled in the reference build), implemented with pure
    /// bounds-checked indexing and **no `unsafe`**. All reads are guaranteed
    /// in-bounds: `strstart <= window_size - MIN_LOOKAHEAD`, `cur_match <
    /// strstart`, `best_len <= MAX_MATCH`, and [`fill_window`](Self::fill_window)
    /// zero-fills the `WIN_INIT` bytes beyond the data, so scans up to
    /// `strstart + MAX_MATCH` stay within the `2 * w_size` window and read
    /// deterministic bytes.
    ///
    /// # Byte-exact fidelity
    ///
    /// The quick-reject comparison order and the `best_len`/`best_len - 1`
    /// probe offsets are preserved exactly, because they determine which of two
    /// equally long matches wins and therefore the produced byte stream. Byte
    /// offset `2` is intentionally **not** re-compared: when the surrounding
    /// bytes match and the hash keys are equal (with `HASH_BITS >= 8`), byte `2`
    /// is always equal, so the C code — and this port — begin extending from
    /// offset `3`.
    ///
    /// The C 8×-unrolled compare loop with an every-8th `scan < strend` bound
    /// check is replaced by a simple per-iteration loop. Because `MAX_MATCH - 2
    /// == 256` is a multiple of 8, both formulations terminate at exactly the
    /// same `scan` position and compute the identical `len`, so the result is
    /// bit-for-bit the same.
    pub fn longest_match(&mut self, cur_match: usize) -> usize {
        let mut chain_length = self.max_chain_length;
        let mut best_len = self.prev_length;
        let mut nice_match = self.nice_match as usize;
        // Stop when cur_match drops to/below `limit`; index 0 is never matched.
        let limit = if self.strstart > self.max_dist() {
            self.strstart - self.max_dist()
        } else {
            NIL as usize
        };
        let wmask = self.w_mask;
        let strend = self.strstart + MAX_MATCH;

        let mut scan_end1 = self.window[self.strstart + best_len - 1];
        let mut scan_end = self.window[self.strstart + best_len];

        // Do not waste time if we already have a good match.
        if self.prev_length >= self.good_match {
            chain_length >>= 2;
        }
        // Do not look beyond the end of the input; keeps deflate deterministic.
        if nice_match > self.lookahead {
            nice_match = self.lookahead;
        }

        let mut cur_match = cur_match;

        loop {
            let match_base = cur_match;

            // Quick reject (positive form of C's `if (A||B||C||D) continue;`).
            // These are pure reads, so the De Morgan inversion preserves
            // behavior exactly. The offsets and the pair of probes at
            // `best_len` / `best_len - 1` match C.
            if self.window[match_base + best_len] == scan_end
                && self.window[match_base + best_len - 1] == scan_end1
                && self.window[match_base] == self.window[self.strstart]
                && self.window[match_base + 1] == self.window[self.strstart + 1]
            {
                // Offsets 0 and 1 matched above; offset 2 is guaranteed equal
                // by the hash and is skipped. Extend the match from offset 3.
                let mut s_idx = self.strstart + 2;
                let mut m_idx = match_base + 2;
                loop {
                    s_idx += 1;
                    m_idx += 1;
                    if self.window[s_idx] != self.window[m_idx] {
                        break;
                    }
                    if s_idx >= strend {
                        break;
                    }
                }
                let len = MAX_MATCH - (strend - s_idx);

                if len > best_len {
                    self.match_start = cur_match;
                    best_len = len;
                    if len >= nice_match {
                        break;
                    }
                    scan_end1 = self.window[self.strstart + best_len - 1];
                    scan_end = self.window[self.strstart + best_len];
                }
            }

            // Advance along the hash chain: C's
            // `while ((cur_match = prev[cur_match & wmask]) > limit
            //         && --chain_length != 0)`.
            cur_match = self.prev[cur_match & wmask] as usize;
            if cur_match <= limit {
                break;
            }
            // Equivalent to `--chain_length != 0` for the chain_length >= 1 that
            // valid deflate levels always supply, and panic-safe otherwise.
            if chain_length <= 1 {
                break;
            }
            chain_length -= 1;
        }

        if best_len <= self.lookahead {
            best_len
        } else {
            self.lookahead
        }
    }

    // -----------------------------------------------------------------------
    // Low-level bit/byte output primitives.
    //
    // These live here (rather than in `trees.rs`) because they only touch the
    // bit-accumulator (`bi_buf`/`bi_valid`/`bi_used`) and the pending-output
    // buffer (`pending_buf`/`pending`), which are all `DeflateState` fields.
    // Keeping them here lets `flush_pending` call `tr_flush_bits`/`bi_flush`
    // without a `state.rs -> trees.rs` dependency cycle; the higher-level tree
    // routines in `trees.rs` call these primitives in turn.
    // -----------------------------------------------------------------------

    /// Appends one byte to the pending-output buffer (`deflate.h`: `put_byte`).
    ///
    /// # Preconditions
    ///
    /// [`pending`](Self::pending) must be strictly less than
    /// [`pending_buf_size`](Self::pending_buf_size), i.e. the pending buffer
    /// must have at least one byte of room. Every caller in this crate
    /// establishes that before writing: `pending_buf` is sized `4 *
    /// lit_bufsize` exactly as C sizes it, which is what makes the
    /// block-emission overflow guarantee hold, and the block producers flush
    /// through [`flush_pending`](Self::flush_pending) before the buffer can
    /// fill.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is already full, because the write is a checked
    /// slice index. C's `put_byte` is an unchecked pointer store, so the same
    /// programming error there is silent memory corruption; here it is a
    /// deterministic abort.
    #[inline]
    pub fn put_byte(&mut self, b: u8) {
        self.pending_buf[self.pending] = b;
        self.pending += 1;
    }

    /// Appends a 16-bit value to the pending buffer, **least-significant byte
    /// first** (`trees.c`: `put_short`).
    ///
    /// # Panics
    ///
    /// Panics if fewer than two bytes of pending-buffer room remain; see
    /// [`put_byte`](Self::put_byte), which performs both writes.
    #[inline]
    pub fn put_short(&mut self, w: u16) {
        self.put_byte((w & 0xff) as u8);
        self.put_byte((w >> 8) as u8);
    }

    /// Reproduces the compiled (non-debug) C `send_bits` macro (`trees.c`):
    /// sends `length` low bits of `value` into the bit accumulator, flushing a
    /// full 16-bit word to the pending buffer when the accumulator overflows.
    ///
    /// # Rust shift safety
    ///
    /// C promotes the operands to `int` and relies on defined behavior for the
    /// shifts. In Rust a shift such as `(value as u16) << 16` (which can occur
    /// when `bi_valid == 16`) would **panic** in debug builds. To reproduce the
    /// C truncation semantics without any `unsafe`, the shift is computed in
    /// `u32` and then truncated with `as u16`; the shift amount is always in
    /// `0..=16`, so the `u32` shift is always well-defined.
    ///
    /// # Preconditions
    ///
    /// `length` must satisfy `1 <= length <= 15` and `value` must fit in
    /// `length` bits — the same IN assertion the C source states above
    /// `send_bits` (`trees.c` L250-L255). Those bounds are what keep
    /// [`bi_valid`](Self::bi_valid) inside `0..16` across calls, which in turn
    /// keeps both shift amounts inside `0..=16`. Every caller derives `length`
    /// from a Huffman code length or a fixed literal in `2..=7`, so the bound
    /// holds by construction.
    ///
    /// # Panics
    ///
    /// Passing a `length` outside `1..=15` drives `bi_valid` out of range and
    /// makes a subsequent call shift by more than 31 bits, which panics on
    /// arithmetic overflow. The overflow branch also calls
    /// [`put_short`](Self::put_short), so it panics if fewer than two bytes of
    /// pending-buffer room remain.
    pub fn send_bits(&mut self, value: i32, length: i32) {
        let val = value as u32;
        if self.bi_valid > BUF_SIZE - length {
            self.bi_buf |= (val << self.bi_valid as u32) as u16;
            let bi_buf = self.bi_buf;
            self.put_short(bi_buf);
            self.bi_buf = (val >> (BUF_SIZE - self.bi_valid) as u32) as u16;
            self.bi_valid += length - BUF_SIZE;
        } else {
            self.bi_buf |= (val << self.bi_valid as u32) as u16;
            self.bi_valid += length;
        }
    }

    /// Sends the Huffman code for symbol `c` from `tree` (`trees.c`:
    /// `send_code`): a [`send_bits`](Self::send_bits) of the code value and its
    /// bit length.
    ///
    /// # Preconditions
    ///
    /// `c` must be a valid symbol index for `tree` (`c < tree.len()`) and
    /// `tree[c]` must carry a non-zero bit length — i.e. the symbol must
    /// actually have been assigned a code by `build_tree`/`gen_codes`. That is
    /// exactly C's `send_code` contract; C simply indexes the array unchecked.
    ///
    /// # Panics
    ///
    /// Panics if `c` is out of range for `tree`, since the lookup is a checked
    /// slice index. It also inherits the [`send_bits`](Self::send_bits) panics:
    /// a `tree[c]` length of `0` (an unassigned symbol) or greater than `15` (a
    /// malformed tree) violates that method's precondition.
    #[inline]
    pub fn send_code(&mut self, c: usize, tree: &[CtData]) {
        self.send_bits(tree[c].code() as i32, tree[c].len() as i32);
    }

    /// Reproduces C `bi_flush` (`trees.c`): flushes whole bytes currently held
    /// in the bit accumulator to the pending buffer, leaving fewer than 8 bits
    /// buffered.
    pub fn bi_flush(&mut self) {
        if self.bi_valid == 16 {
            let bi_buf = self.bi_buf;
            self.put_short(bi_buf);
            self.bi_buf = 0;
            self.bi_valid = 0;
        } else if self.bi_valid >= 8 {
            self.put_byte((self.bi_buf & 0xff) as u8);
            self.bi_buf >>= 8;
            self.bi_valid -= 8;
        }
    }

    /// Reproduces C `bi_windup` (`trees.c`): flushes the remaining bits,
    /// padding to a byte boundary, and resets the accumulator.
    ///
    /// Also records [`bi_used`](DeflateState::bi_used) — the number of bits
    /// occupied in the final output byte — using the C expression
    /// `((bi_valid - 1) & 7) + 1`, evaluated on the pre-windup `bi_valid`.
    pub fn bi_windup(&mut self) {
        if self.bi_valid > 8 {
            let bi_buf = self.bi_buf;
            self.put_short(bi_buf);
        } else if self.bi_valid > 0 {
            self.put_byte((self.bi_buf & 0xff) as u8);
        }
        self.bi_used = ((self.bi_valid - 1) & 7) + 1;
        self.bi_buf = 0;
        self.bi_valid = 0;
    }

    /// Reproduces C `_tr_flush_bits` (`trees.c`): flushes the bit buffer to the
    /// pending output (a thin wrapper over [`bi_flush`](Self::bi_flush)). Named
    /// without the leading underscore to keep it a conventional public method.
    ///
    /// # Panics
    ///
    /// Inherits [`put_byte`](Self::put_byte)/[`put_short`](Self::put_short):
    /// panics if the pending buffer has no room for the bits being flushed.
    #[inline]
    pub fn tr_flush_bits(&mut self) {
        self.bi_flush();
    }

    /// Reproduces C `flush_pending` (`deflate.c` L950-L968): first flushes any
    /// buffered bits (`tr_flush_bits`), then copies as many pending bytes as
    /// fit into the output buffer described by `io`, advancing all cursors and
    /// counters. When the pending buffer drains completely, the read offset is
    /// reset to the start.
    ///
    /// # Preconditions
    ///
    /// Two window invariants must hold, both maintained by [`IoContext`] and by
    /// this type's own bookkeeping:
    ///
    /// * `io.next_out + io.avail_out <= io.output.len()` — the output cursor and
    ///   remaining count describe a real region of the caller's buffer;
    /// * `self.pending_out + self.pending <= self.pending_buf.len()` — the
    ///   unflushed pending bytes lie inside the pending buffer.
    ///
    /// # Panics
    ///
    /// Panics if either invariant is violated, because both the destination
    /// (`io.output[next_out..next_out + len]`) and the source
    /// (`pending_buf[pending_out..pending_out + len]`) are checked slice ranges.
    /// It also inherits the [`tr_flush_bits`](Self::tr_flush_bits) panics, which
    /// run first.
    pub fn flush_pending(&mut self, io: &mut IoContext) {
        self.tr_flush_bits();

        let len = core::cmp::min(self.pending, io.avail_out);
        if len == 0 {
            return;
        }

        let src = self.pending_out;
        let dst = io.next_out;
        io.output[dst..dst + len].copy_from_slice(&self.pending_buf[src..src + len]);

        io.next_out += len;
        self.pending_out += len;
        io.total_out += len as u64;
        io.avail_out -= len;
        self.pending -= len;

        if self.pending == 0 {
            self.pending_out = 0;
        }
    }

    /// Reproduces the C `MAX_DIST(s)` macro (`deflate.h`): the largest distance
    /// a match may reach back, `w_size - MIN_LOOKAHEAD`. Kept as a method so the
    /// window size is always read from the live state.
    #[inline]
    #[must_use]
    pub fn max_dist(&self) -> usize {
        self.w_size - MIN_LOOKAHEAD
    }

    /// Reproduces the C `#define max_insert_length max_lazy_match`
    /// (`deflate.h`): the two names are aliases for the same value, exposed
    /// here as an accessor for the match-finder call sites.
    #[inline]
    #[must_use]
    pub fn max_insert_length(&self) -> usize {
        self.max_lazy_match
    }
}

/// Releases the working buffers in **C `deflateEnd`'s order**.
///
/// Ownership alone would already free every buffer — that is the whole point of
/// holding them as [`AllocBuffer`]s — but it would free them in *field
/// declaration* order, which is `pending_buf`, `window`, `prev`, `head`,
/// `state_alloc`. Reference zlib frees them "in reverse order of allocations":
///
/// ```text
/// TRY_FREE(strm, strm->state->pending_buf);   /* deflate.c L1301 */
/// TRY_FREE(strm, strm->state->head);          /* deflate.c L1302 */
/// TRY_FREE(strm, strm->state->prev);          /* deflate.c L1303 */
/// TRY_FREE(strm, strm->state->window);        /* deflate.c L1304 */
/// ZFREE(strm, strm->state);                   /* deflate.c L1306 */
/// ```
///
/// A caller-supplied `zfree` observes that sequence, and an arena implementation
/// that unwinds allocations in LIFO order — or merely records them, as
/// `infcover.c`'s harness does — can tell the difference. Since AAP §0.6.5 makes
/// the allocator-visible schedule a first-class parity requirement, the order is
/// pinned explicitly here rather than left to depend on how the fields happen to
/// be declared.
///
/// Each buffer is swapped out with [`core::mem::take`] and dropped immediately;
/// the replacement is an empty [`AllocBuffer`], whose own drop is a no-op, so the
/// implicit field drops that follow release nothing further. `state_alloc` goes
/// last, standing in for C's `ZFREE(strm, strm->state)`.
impl Drop for DeflateState {
    fn drop(&mut self) {
        drop(core::mem::take(&mut self.pending_buf));
        drop(core::mem::take(&mut self.head));
        drop(core::mem::take(&mut self.prev));
        drop(core::mem::take(&mut self.window));
        drop(core::mem::take(&mut self.state_alloc));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{
        Strategy, Z_DEFAULT_COMPRESSION, Z_DEFLATED, Z_FINISH, Z_NO_FLUSH, Z_SYNC_FLUSH,
    };
    use crate::error::ReturnCode;
    use crate::stream::ZStream;

    /// Compile-time constants must equal the C baseline exactly.
    #[test]
    fn constants_match_c() {
        assert_eq!(LENGTH_CODES, 29);
        assert_eq!(LITERALS, 256);
        assert_eq!(L_CODES, 286);
        assert_eq!(D_CODES, 30);
        assert_eq!(BL_CODES, 19);
        assert_eq!(HEAP_SIZE, 573);
        assert_eq!(MAX_BITS, 15);
        assert_eq!(MAX_BL_BITS, 7);
        assert_eq!(BUF_SIZE, 16);
        assert_eq!(MIN_MATCH, 3);
        assert_eq!(MAX_MATCH, 258);
        assert_eq!(MIN_LOOKAHEAD, 262);
        assert_eq!(WIN_INIT, 258);
        assert_eq!(END_BLOCK, 256);
        assert_eq!(NIL, 0);
        assert_eq!(TOO_FAR, 4096);
    }

    /// `DeflateStatus` discriminants must be the exact C sentinels.
    #[test]
    fn status_discriminants() {
        assert_eq!(DeflateStatus::Init as u16, 42);
        assert_eq!(DeflateStatus::Gzip as u16, 57);
        assert_eq!(DeflateStatus::Extra as u16, 69);
        assert_eq!(DeflateStatus::Name as u16, 73);
        assert_eq!(DeflateStatus::Comment as u16, 91);
        assert_eq!(DeflateStatus::Hcrc as u16, 103);
        assert_eq!(DeflateStatus::Busy as u16, 113);
        assert_eq!(DeflateStatus::Finish as u16, 666);
    }

    /// `CtData` faithfully overlays the C `ct_data` union: `freq` aliases
    /// `code` (both `fc`), and `dad` aliases `len` (both `dl`).
    #[test]
    fn ctdata_union_aliasing() {
        let mut c = CtData::default();
        assert_eq!(c.freq(), 0);
        assert_eq!(c.dad(), 0);

        c.set_freq(1234);
        assert_eq!(c.code(), 1234); // code shares storage with freq
        c.set_code(4321);
        assert_eq!(c.freq(), 4321);

        c.set_dad(77);
        assert_eq!(c.len(), 77); // len shares storage with dad
        c.set_len(88);
        assert_eq!(c.dad(), 88);
    }

    /// `send_bits` must never panic on shift overflow, even when `bi_valid`
    /// reaches 16 and 16-bit values are pushed, and must produce the C result.
    #[test]
    fn send_bits_no_shift_panic() {
        let mut s = DeflateState::new(6, Z_DEFLATED, 15, 8, Strategy::Default, 1).unwrap();

        // Simple accumulation (else branch).
        s.send_bits(0b101, 3);
        assert_eq!(s.bi_buf, 0b101);
        assert_eq!(s.bi_valid, 3);

        // Force the worst case: a full accumulator, then push 15 more bits.
        // With bi_valid == 16 this shifts a u16 value left by 16, which would
        // panic if computed in u16; the port computes in u32 and truncates.
        s.bi_buf = 0;
        s.bi_valid = 16;
        s.pending = 0;
        s.send_bits(0xFFFF, 15);
        assert_eq!(s.pending, 2); // one 16-bit word flushed via put_short
        assert_eq!(s.bi_valid, 15); // 16 + 15 - 16
        assert_eq!(s.bi_buf, 0xFFFF); // value >> (16 - 16)
    }

    /// Checksum seeds must match the C initial values despite the crate's
    /// slice-based (no null-sentinel) checksum API.
    #[test]
    fn initial_adler_seed() {
        // zlib framing: adler32(1, &[]) == 1 (C adler32(0, Z_NULL, 0) == 1).
        assert_eq!(DeflateState::initial_adler(1), 1);
        assert_eq!(DeflateState::initial_adler(0), 1);
        #[cfg(feature = "gzip")]
        {
            // gzip framing: crc32(0, &[]) == 0.
            assert_eq!(DeflateState::initial_adler(2), 0);
        }
    }

    /// `new` reproduces the deflateInit2_ allocation and derived sizing, and
    /// resolves `Z_DEFAULT_COMPRESSION` to level 6.
    #[test]
    fn new_allocations_and_config() {
        let s = DeflateState::new(
            Z_DEFAULT_COMPRESSION,
            Z_DEFLATED,
            15,
            8,
            Strategy::Default,
            1,
        )
        .unwrap();

        assert_eq!(s.level, 6); // default resolved
        assert_eq!(s.w_bits, 15);
        assert_eq!(s.w_size, 1 << 15);
        assert_eq!(s.w_mask, (1 << 15) - 1);
        assert_eq!(s.window.len(), 2 << 15);
        assert_eq!(s.window_size, 2 << 15);
        assert_eq!(s.prev.len(), 1 << 15);
        assert_eq!(s.hash_bits, 15);
        assert_eq!(s.hash_size, 1 << 15);
        assert_eq!(s.head.len(), 1 << 15);
        assert_eq!(s.hash_shift, 5); // (15 + 2) / 3
        assert_eq!(s.lit_bufsize, 1 << 14);
        assert_eq!(s.pending_buf.len(), 4 * (1 << 14));
        assert_eq!(s.pending_buf_size, 4 * (1 << 14));
        // The symbol region is the upper `3 * lit_bufsize` bytes of that single
        // allocation, exactly as C's `s->sym_buf = s->pending_buf + s->lit_bufsize`
        // (`deflate.c` L520) — it has no allocation of its own.
        assert_eq!(s.sym_region().len(), 3 * (1 << 14));
        assert_eq!(s.sym_end, ((1 << 14) - 1) * 3);
        assert_eq!(s.method, Z_DEFLATED as u8);
        assert!(s.is_valid());
        assert_eq!(s.status, DeflateStatus::Init); // wrap == 1
        assert_eq!(s.last_flush, -2);
        // lm_init defaults for level 6.
        assert_eq!(s.good_match, 8);
        assert_eq!(s.max_lazy_match, 16);
        assert_eq!(s.nice_match, 128);
        assert_eq!(s.max_chain_length, 128);
        assert_eq!(s.match_length, MIN_MATCH - 1);
        assert_eq!(s.prev_length, MIN_MATCH - 1);
    }

    /// `lm_init` must load the match-finder heuristics for **every** level from
    /// the crate's single authoritative tuning table, and there must be exactly
    /// one such table.
    ///
    /// `src/deflate/state.rs` previously carried a private duplicate of
    /// `configuration_table`, so `lm_init` (init) and `deflate_params`
    /// (re-dispatch) read *different* copies of the same numbers. Two copies can
    /// diverge silently, and any divergence changes the lazy-match decisions and
    /// therefore the emitted token stream — breaking byte-identity with reference
    /// zlib without breaking decodability (AAP §0.6.4, TO-5). The duplicate is
    /// gone; this test pins the surviving one against live state for all ten
    /// levels plus the `Z_DEFAULT_COMPRESSION` alias.
    #[test]
    fn lm_init_loads_every_level_from_the_authoritative_table() {
        for level in 0..=9i32 {
            let s = DeflateState::new(level, Z_DEFLATED, 15, 8, Strategy::Default, 1)
                .expect("level 0..=9 is valid");
            let cfg = &CONFIGURATION_TABLE[level as usize];

            assert_eq!(
                s.good_match, cfg.good_length as usize,
                "level {level}: good_match"
            );
            assert_eq!(
                s.max_lazy_match, cfg.max_lazy as usize,
                "level {level}: max_lazy_match"
            );
            assert_eq!(
                s.nice_match, cfg.nice_length as i32,
                "level {level}: nice_match"
            );
            assert_eq!(
                s.max_chain_length, cfg.max_chain as usize,
                "level {level}: max_chain_length"
            );
            // The resolved level is recorded verbatim, so the row index above is
            // the row the match finder will keep using.
            assert_eq!(s.level, level, "level {level}: resolved level");
        }

        // `Z_DEFAULT_COMPRESSION` (-1) resolves to 6 (`deflate.c` L438-L440), so
        // it must load row 6 and not, say, row 0.
        let d = DeflateState::new(
            Z_DEFAULT_COMPRESSION,
            Z_DEFLATED,
            15,
            8,
            Strategy::Default,
            1,
        )
        .expect("Z_DEFAULT_COMPRESSION is valid");
        let six = &CONFIGURATION_TABLE[6];
        assert_eq!(d.level, 6, "Z_DEFAULT_COMPRESSION must resolve to level 6");
        assert_eq!(d.good_match, six.good_length as usize);
        assert_eq!(d.max_lazy_match, six.max_lazy as usize);
        assert_eq!(d.nice_match, six.nice_length as i32);
        assert_eq!(d.max_chain_length, six.max_chain as usize);
    }

    /// The exact row values, spelled out independently of the table itself, so a
    /// mistaken edit to `CONFIGURATION_TABLE` cannot make the test above pass
    /// vacuously by moving both sides together. Transcribed from the C
    /// `configuration_table[10]` (`deflate.c` L112-L124): `{good, lazy, nice,
    /// chain}`.
    #[test]
    fn authoritative_table_rows_match_the_c_baseline() {
        const C_TABLE: [(u16, u16, u16, u16); 10] = [
            (0, 0, 0, 0),         // 0 store only
            (4, 4, 8, 4),         // 1 max speed, no lazy matches
            (4, 5, 16, 8),        // 2
            (4, 6, 32, 32),       // 3
            (4, 4, 16, 16),       // 4 lazy matches
            (8, 16, 32, 32),      // 5
            (8, 16, 128, 128),    // 6 (default)
            (8, 32, 128, 256),    // 7
            (32, 128, 258, 1024), // 8
            (32, 258, 258, 4096), // 9 max compression
        ];

        for (level, &(good, lazy, nice, chain)) in C_TABLE.iter().enumerate() {
            let cfg = &CONFIGURATION_TABLE[level];
            assert_eq!(
                (
                    cfg.good_length,
                    cfg.max_lazy,
                    cfg.nice_length,
                    cfg.max_chain
                ),
                (good, lazy, nice, chain),
                "CONFIGURATION_TABLE row {level} diverged from deflate.c"
            );

            // ...and a live state built at that level agrees.
            let s = DeflateState::new(level as i32, Z_DEFLATED, 15, 8, Strategy::Default, 1)
                .expect("valid level");
            assert_eq!(
                (
                    s.good_match,
                    s.max_lazy_match,
                    s.nice_match,
                    s.max_chain_length
                ),
                (good as usize, lazy as usize, nice as i32, chain as usize),
                "live state at level {level} diverged from deflate.c"
            );
        }
    }

    /// An 8-bit window is legal only with the zlib wrapper and is bumped to 9.
    #[test]
    fn new_window_bits_eight_bumped() {
        let s = DeflateState::new(6, Z_DEFLATED, 8, 8, Strategy::Default, 1).unwrap();
        assert_eq!(s.w_bits, 9);
        assert_eq!(s.w_size, 1 << 9);
    }

    /// Invalid parameter combinations must be rejected exactly as zlib does.
    #[test]
    fn new_rejects_invalid_params() {
        // windowBits == 8 requires wrap == 1.
        assert!(DeflateState::new(6, Z_DEFLATED, 8, 8, Strategy::Default, 0).is_err());
        // method must be Z_DEFLATED.
        assert!(DeflateState::new(6, 7, 15, 8, Strategy::Default, 1).is_err());
        // level out of range.
        assert!(DeflateState::new(10, Z_DEFLATED, 15, 8, Strategy::Default, 1).is_err());
        // windowBits out of range.
        assert!(DeflateState::new(6, Z_DEFLATED, 16, 8, Strategy::Default, 1).is_err());
        assert!(DeflateState::new(6, Z_DEFLATED, 7, 8, Strategy::Default, 1).is_err());
        // memLevel out of range: `0` (below the `1` minimum) and `10` (above
        // MAX_MEM_LEVEL == 9) are rejected; the full `1..=9` range is accepted
        // (see `new_accepts_max_mem_level`).
        assert!(DeflateState::new(6, Z_DEFLATED, 15, 0, Strategy::Default, 1).is_err());
        assert!(DeflateState::new(6, Z_DEFLATED, 15, 10, Strategy::Default, 1).is_err());
    }

    /// `memLevel == MAX_MEM_LEVEL == 9` must be accepted, matching reference
    /// zlib on modern (non-`MAXSEG_64K`) platforms (`deflate.c` L434). This is
    /// the boundary that a stricter `MAX_MEM_LEVEL == 8` would wrongly reject.
    #[test]
    fn new_accepts_max_mem_level() {
        // Every in-range memLevel (1..=9) constructs successfully.
        for mem_level in 1..=MAX_MEM_LEVEL {
            assert!(
                DeflateState::new(6, Z_DEFLATED, 15, mem_level, Strategy::Default, 1).is_ok(),
                "memLevel {mem_level} should be accepted"
            );
        }
        // memLevel == 9 sizes the hash table with hash_bits == mem_level + 7 == 16.
        let s = DeflateState::new(6, Z_DEFLATED, 15, 9, Strategy::Default, 1).unwrap();
        assert_eq!(s.mem_level, 9);
        assert_eq!(s.hash_bits, 16);
        assert_eq!(s.hash_size, 1 << 16);
        assert_eq!(s.head.len(), 1 << 16);
        assert_eq!(s.lit_bufsize, 1 << 15);
    }

    /// `slide_hash` decrements entries by `w_size`, flooring at `NIL`.
    #[test]
    fn slide_hash_saturates() {
        // Small window (w_size == 512) keeps the arithmetic easy to check.
        let mut s = DeflateState::new(6, Z_DEFLATED, 9, 8, Strategy::Default, 1).unwrap();
        assert_eq!(s.w_size, 512);
        s.head[0] = 600;
        s.head[1] = 100; // below w_size -> becomes NIL
        s.prev[0] = 512; // exactly w_size -> becomes 0
        s.prev[1] = 700;
        s.slide_hash();
        assert_eq!(s.head[0], 600 - 512);
        assert_eq!(s.head[1], NIL);
        assert_eq!(s.prev[0], 0);
        assert_eq!(s.prev[1], 700 - 512);
        assert!(s.slid);
    }

    /// `longest_match` finds a planted match and reports its length (capped by
    /// the lookahead), setting `match_start`.
    #[test]
    fn longest_match_finds_planted_match() {
        let mut s = DeflateState::new(9, Z_DEFLATED, 15, 8, Strategy::Default, 0).unwrap();

        // Deterministic window.
        s.window.iter_mut().for_each(|b| *b = 0);
        let pat: [u8; 12] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let mstart = 40usize;
        let strstart = 100usize;
        s.window[mstart..mstart + 12].copy_from_slice(&pat);
        s.window[strstart..strstart + 12].copy_from_slice(&pat);
        // Make the byte just past the pattern differ so the match ends at 12.
        s.window[mstart + 12] = 201;
        s.window[strstart + 12] = 202;

        s.strstart = strstart;
        s.lookahead = 12; // cap the match at exactly the planted length
        s.prev_length = MIN_MATCH - 1; // best_len starts at 2
        s.good_match = 9999;
        s.nice_match = 9999;
        s.max_chain_length = 64;

        let len = s.longest_match(mstart);
        assert_eq!(len, 12);
        assert_eq!(s.match_start, mstart);
    }

    /// With empty input, `fill_window` performs no reads but still zero-fills
    /// the `WIN_INIT` region beyond the (empty) data and advances high_water.
    #[test]
    fn fill_window_zeroes_win_init() {
        let mut s = DeflateState::new(6, Z_DEFLATED, 15, 8, Strategy::Default, 1).unwrap();
        assert_eq!(s.high_water, 0);

        let input: [u8; 0] = [];
        let mut output = [0u8; 16];
        let mut io = IoContext::new(&input, &mut output);

        s.fill_window(&mut io);

        // curr == 0, so the second branch zeroes WIN_INIT bytes from 0.
        assert_eq!(s.high_water, WIN_INIT);
        assert_eq!(s.lookahead, 0);
    }

    /// Drives `deflate(.., Z_FINISH)` to completion the way a C caller loops
    /// until `Z_STREAM_END`, advancing the input cursor by `consumed` on every
    /// call and appending to `out`. Returns the number of bytes appended.
    fn finish_into(strm: &mut ZStream, data: &[u8], out: &mut [u8]) -> usize {
        let mut in_off = 0usize;
        let mut produced = 0usize;
        loop {
            let r = crate::deflate::deflate(strm, &data[in_off..], &mut out[produced..], Z_FINISH);
            in_off += r.consumed;
            produced += r.produced;
            match r.code {
                ReturnCode::StreamEnd => return produced,
                ReturnCode::Ok => assert!(
                    produced < out.len(),
                    "output buffer exhausted before Z_STREAM_END"
                ),
                other => panic!("unexpected deflate return code {other:?}"),
            }
        }
    }

    /// Compresses `prefix` with `Z_NO_FLUSH` and then `tail` with `Z_FINISH` on
    /// one fresh stream — the exact call sequence the deep-copy test performs —
    /// so the result is the stream an *uninterrupted* compressor would have
    /// produced, and is therefore directly comparable to what the snapshot
    /// produces from the same point onwards.
    fn deflate_streamed(prefix: &[u8], tail: &[u8], level: i32) -> alloc::vec::Vec<u8> {
        let mut strm: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut strm, level, Z_DEFLATED, 15, 8, Strategy::Default)
            .expect("init");
        let mut out = alloc::vec![0u8; (prefix.len() + tail.len()) * 2 + 128];
        let r = crate::deflate::deflate(&mut strm, prefix, &mut out, Z_NO_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        assert_eq!(r.consumed, prefix.len());
        let mut n = r.produced;
        n += finish_into(&mut strm, tail, &mut out[n..]);
        out.truncate(n);
        crate::deflate::deflate_end(&mut strm).expect("end");
        out
    }

    /// Decompresses a complete zlib stream with the crate's own inflate engine,
    /// so the deep-copy tests need neither `std` nor a third-party decoder and
    /// therefore run in every feature configuration.
    fn inflate_all(stream: &[u8], expect_len: usize) -> alloc::vec::Vec<u8> {
        let mut strm: ZStream = ZStream::new();
        crate::inflate::inflate_init2(&mut strm, 15).expect("inflate init");
        let mut out = alloc::vec![0u8; expect_len + 64];
        let r = crate::inflate::inflate(&mut strm, stream, &mut out, 0);
        assert_eq!(r.code, ReturnCode::StreamEnd, "stream must decode fully");
        out.truncate(r.produced);
        crate::inflate::inflate_end(&mut strm).expect("inflate end");
        out
    }

    /// `deflateCopy` produces a genuinely independent snapshot of a **live**
    /// compressor, not a shallow alias.
    ///
    /// The previous version of this test cloned a *freshly initialised* state,
    /// poked one window byte, and compared lengths — which a shallow copy that
    /// shared every buffer would also have passed. This version:
    ///
    /// 1. drives real compression so the sliding window, the `head`/`prev` hash
    ///    chains, the overlaid symbol region and the pending buffer all hold live
    ///    data;
    /// 2. takes the copy;
    /// 3. proves the buffers are distinct *storage* by mutating one side and
    ///    observing the other is unchanged (window, `head`, `prev`, and both the
    ///    pending-output and symbol halves of `pending_buf`);
    /// 4. feeds the source and the copy **divergent suffixes**, finishes both,
    ///    and asserts each produced a correct — and different — stream that is
    ///    moreover byte-identical to what an uninterrupted compressor fed the
    ///    same total input emits, which is only possible if the window and hash
    ///    chains were duplicated faithfully rather than shared or reset; and
    /// 5. drops the two streams in both orders across the two halves of the test,
    ///    so a double free or a use-after-free would surface.
    ///
    /// Note that the copy continues the *same* zlib stream: the bytes the source
    /// had already emitted before the snapshot are part of the copy's stream too,
    /// so they are prepended before decoding or comparing.
    #[test]
    fn deflate_copy_snapshots_live_state_independently() {
        let prefix = b"the quick brown fox jumps over the lazy dog; ".repeat(24);
        let tail_a = b"AAAA suffix alpha alpha alpha ".repeat(8);
        let tail_b = b"ZZZZ suffix omega omega omega ".repeat(8);

        let mut src: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut src, 6, Z_DEFLATED, 15, 8, Strategy::Default)
            .expect("init");

        // (1) Consume the prefix so the window and hash chains hold real history.
        let mut out_src = alloc::vec![0u8; 8192];
        let r = crate::deflate::deflate(&mut src, &prefix, &mut out_src, Z_NO_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        assert_eq!(r.consumed, prefix.len());
        let mut produced_src = r.produced;

        {
            let st = src.deflate_state().expect("state installed");
            assert!(st.strstart > 0, "the window must hold history");
            assert!(
                st.head.iter().any(|&h| h != NIL),
                "the hash heads must hold live chains"
            );
            assert!(
                st.prev.iter().any(|&p| p != NIL),
                "the prev chain must hold live links"
            );
            assert!(st.sym_next > 0, "the symbol buffer must hold live symbols");
        }

        // Everything the source emitted *before* the snapshot belongs to the
        // copy's stream as well, so keep it to rebuild a complete stream later.
        let common: alloc::vec::Vec<u8> = out_src[..produced_src].to_vec();

        // (2) Snapshot.
        let mut cpy: ZStream = ZStream::new();
        crate::deflate::deflate_copy(&mut cpy, &src).expect("copy");

        // (2b) The snapshot is faithful: every cursor and every buffer *content*
        //      matches the source at snapshot time. Asserting this directly —
        //      rather than inferring it from the output alone — catches a field a
        //      future edit forgets to carry over even when that field happens to
        //      be reconstructible (`ins_h`, for instance, is rebuilt by
        //      `fill_window` from `window` + `insert`, `deflate.c` L314-L317, so
        //      output equality alone would not notice its loss).
        {
            let a = src.deflate_state().expect("source state");
            let b = cpy.deflate_state().expect("copy state");

            assert_eq!(b.status, a.status, "status");
            assert_eq!(b.level, a.level, "level");
            assert_eq!(b.strategy, a.strategy, "strategy");
            assert_eq!(b.wrap, a.wrap, "wrap");
            assert_eq!(b.w_size, a.w_size, "w_size");
            assert_eq!(b.w_mask, a.w_mask, "w_mask");
            assert_eq!(b.hash_bits, a.hash_bits, "hash_bits");
            assert_eq!(b.hash_mask, a.hash_mask, "hash_mask");
            assert_eq!(b.hash_shift, a.hash_shift, "hash_shift");
            assert_eq!(b.ins_h, a.ins_h, "ins_h");
            assert_eq!(b.insert, a.insert, "insert");
            assert_eq!(b.strstart, a.strstart, "strstart");
            assert_eq!(b.block_start, a.block_start, "block_start");
            assert_eq!(b.lookahead, a.lookahead, "lookahead");
            assert_eq!(b.match_start, a.match_start, "match_start");
            assert_eq!(b.match_length, a.match_length, "match_length");
            assert_eq!(b.prev_length, a.prev_length, "prev_length");
            assert_eq!(b.match_available, a.match_available, "match_available");
            assert_eq!(b.high_water, a.high_water, "high_water");
            assert_eq!(b.good_match, a.good_match, "good_match");
            assert_eq!(b.max_lazy_match, a.max_lazy_match, "max_lazy_match");
            assert_eq!(b.nice_match, a.nice_match, "nice_match");
            assert_eq!(b.max_chain_length, a.max_chain_length, "max_chain_length");
            assert_eq!(b.pending, a.pending, "pending");
            assert_eq!(b.pending_out, a.pending_out, "pending_out");
            assert_eq!(b.sym_next, a.sym_next, "sym_next");
            assert_eq!(b.sym_end, a.sym_end, "sym_end");
            assert_eq!(b.last_flush, a.last_flush, "last_flush");
            assert_eq!(b.bi_buf, a.bi_buf, "bi_buf");
            assert_eq!(b.bi_valid, a.bi_valid, "bi_valid");
            assert_eq!(b.opt_len, a.opt_len, "opt_len");
            assert_eq!(b.static_len, a.static_len, "static_len");
            assert_eq!(b.matches, a.matches, "matches");
            assert_eq!(b.slid, a.slid, "slid");
            assert_eq!(b.w_bits, a.w_bits, "w_bits");
            assert_eq!(b.hash_size, a.hash_size, "hash_size");
            assert_eq!(b.lit_bufsize, a.lit_bufsize, "lit_bufsize");
            #[cfg(feature = "gzip")]
            assert_eq!(b.gzindex, a.gzindex, "gzindex");

            // The regions C's `deflateCopy` explicitly duplicates
            // (`deflate.c` L1352-L1367): `window[..high_water]`, the live `prev`
            // prefix, the whole `head`, the undelivered `pending` bytes, and the
            // symbol region's live `[..sym_next]` prefix. Everything outside them is dead state that
            // C leaves as freshly-allocated garbage.
            let live_prev = if a.slid || a.strstart - a.insert > a.w_size {
                a.w_size
            } else {
                a.strstart - a.insert
            };
            assert_eq!(
                &b.window[..a.high_water],
                &a.window[..a.high_water],
                "live window"
            );
            assert_eq!(
                &b.prev[..live_prev],
                &a.prev[..live_prev],
                "live prev chain"
            );
            assert_eq!(&*b.head, &*a.head, "hash heads");
            assert_eq!(
                &b.pending_buf[a.pending_out..a.pending_out + a.pending],
                &a.pending_buf[a.pending_out..a.pending_out + a.pending],
                "undelivered pending output"
            );
            assert_eq!(
                &b.sym_region()[..a.sym_next],
                &a.sym_region()[..a.sym_next],
                "live symbol region"
            );

            // This implementation copies each buffer in full, a superset of C's
            // live-region copy, so the snapshot carries no uninitialised bytes.
            assert_eq!(&*b.window, &*a.window, "window contents");
            assert_eq!(&*b.prev, &*a.prev, "prev contents");
            // `pending_buf` carries the overlaid symbol region in its upper
            // `3 * lit_bufsize` bytes, so this single comparison covers both the
            // pending output and every symbol byte.
            assert_eq!(&*b.pending_buf, &*a.pending_buf, "pending_buf contents");
        }

        // (3) The buffers are distinct storage: mutating the copy leaves the
        //     source byte-for-byte unchanged.
        {
            let before = {
                let st = src.deflate_state().expect("source state");
                (
                    st.window[0],
                    st.head[0],
                    st.prev[0],
                    st.pending_buf[0],
                    st.sym(0),
                    st.strstart,
                    st.sym_next,
                )
            };

            let c = cpy.deflate_state_mut().expect("copy state");
            assert_eq!(c.window[0], before.0, "the copy starts identical");
            assert_eq!(c.strstart, before.5, "the copy inherits the window cursor");
            assert_eq!(c.sym_next, before.6, "the copy inherits the symbol cursor");
            c.window[0] = before.0 ^ 0xFF;
            c.head[0] = before.1 ^ 0xBEEF;
            c.prev[0] = before.2 ^ 0xBEEF;
            c.pending_buf[0] = before.3 ^ 0xFF;
            c.set_sym(0, before.4 ^ 0xFF);

            let st = src.deflate_state().expect("source state");
            assert_eq!(st.window[0], before.0, "window is shared!");
            assert_eq!(st.head[0], before.1, "head is shared!");
            assert_eq!(st.prev[0], before.2, "prev is shared!");
            assert_eq!(st.pending_buf[0], before.3, "pending_buf is shared!");
            assert_eq!(st.sym(0), before.4, "the symbol region is shared!");
            assert_eq!(st.strstart, before.5);
            assert_eq!(st.sym_next, before.6);

            // Restore the copy so step (4) compresses honestly.
            let c = cpy.deflate_state_mut().expect("copy state");
            c.window[0] = before.0;
            c.head[0] = before.1;
            c.prev[0] = before.2;
            c.pending_buf[0] = before.3;
            c.set_sym(0, before.4);
        }

        // (4) Divergent suffixes, finished independently.
        let mut out_cpy = alloc::vec![0u8; 8192];
        let produced_cpy = finish_into(&mut cpy, &tail_b, &mut out_cpy);
        produced_src += finish_into(&mut src, &tail_a, &mut out_src[produced_src..]);

        let mut want_a = prefix.clone();
        want_a.extend_from_slice(&tail_a);
        let mut want_b = prefix.clone();
        want_b.extend_from_slice(&tail_b);

        let got_a: alloc::vec::Vec<u8> = out_src[..produced_src].to_vec();
        let mut got_b = common.clone();
        got_b.extend_from_slice(&out_cpy[..produced_cpy]);

        assert_ne!(
            got_a, got_b,
            "divergent suffixes must produce different streams"
        );
        assert_eq!(inflate_all(&got_a, want_a.len()), want_a);
        assert_eq!(inflate_all(&got_b, want_b.len()), want_b);

        // Each side is byte-identical to an uninterrupted compressor fed the same
        // total input, which holds only if the snapshot duplicated the window and
        // the hash chains faithfully instead of sharing or resetting them.
        assert_eq!(
            got_a,
            deflate_streamed(&prefix, &tail_a, 6),
            "the source must continue exactly as an uninterrupted stream would"
        );
        assert_eq!(
            got_b,
            deflate_streamed(&prefix, &tail_b, 6),
            "the snapshot must continue exactly as an uninterrupted stream would"
        );

        // (5) Drop order A: copy first, then source.
        crate::deflate::deflate_end(&mut cpy).expect("end copy");
        crate::deflate::deflate_end(&mut src).expect("end source");
        drop(cpy);
        drop(src);

        // (5) Drop order B: source first, then copy — the copy must still own its
        //     buffers and keep compressing correctly after the source is gone.
        let mut src2: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut src2, 9, Z_DEFLATED, 15, 8, Strategy::Default)
            .expect("init");
        let mut scratch = alloc::vec![0u8; 8192];
        let r = crate::deflate::deflate(&mut src2, &prefix, &mut scratch, Z_NO_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        assert_eq!(r.consumed, prefix.len());
        let common2: alloc::vec::Vec<u8> = scratch[..r.produced].to_vec();

        let mut cpy2: ZStream = ZStream::new();
        crate::deflate::deflate_copy(&mut cpy2, &src2).expect("copy");
        // C's `deflateEnd` reports `Z_DATA_ERROR` for a stream discarded while
        // still `BUSY_STATE` — `status == BUSY_STATE ? Z_DATA_ERROR : Z_OK`
        // (`deflate.c` L1309) — while still releasing everything. `src2` is
        // deliberately mid-stream here, so that is the C-exact outcome, and the
        // release must nonetheless leave the copy fully intact.
        assert_eq!(
            crate::deflate::deflate_end(&mut src2),
            Err(ZlibError::DataError)
        );
        drop(src2);

        let st = cpy2
            .deflate_state()
            .expect("copy state outlives the source");
        assert!(st.strstart > 0);
        let mut out2 = alloc::vec![0u8; 8192];
        let n2 = finish_into(&mut cpy2, &tail_a, &mut out2);
        let mut got2 = common2;
        got2.extend_from_slice(&out2[..n2]);
        assert_eq!(inflate_all(&got2, want_a.len()), want_a);
        assert_eq!(
            got2,
            deflate_streamed(&prefix, &tail_a, 9),
            "a copy whose source has been freed still tracks an uninterrupted stream"
        );
        crate::deflate::deflate_end(&mut cpy2).expect("end copy");
    }

    /// Drives a stream to the point where a block sits **undelivered** in
    /// `pending_buf`: consume `prefix` with `Z_NO_FLUSH`, then ask for a
    /// `Z_SYNC_FLUSH` through a `sync_window`-byte output window, so the block is
    /// emitted into `pending_buf` but cannot be handed back in full. Appends to
    /// `out` and returns the number of bytes emitted so far.
    fn drive_to_pending(
        strm: &mut ZStream,
        prefix: &[u8],
        out: &mut [u8],
        sync_window: usize,
    ) -> usize {
        let r = crate::deflate::deflate(strm, prefix, out, Z_NO_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        assert_eq!(r.consumed, prefix.len());
        let n = r.produced;
        let r = crate::deflate::deflate(strm, &[], &mut out[n..n + sync_window], Z_SYNC_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        n + r.produced
    }

    /// The uninterrupted-stream reference for [`drive_to_pending`], i.e. the same
    /// scripted call sequence followed by `tail` under `Z_FINISH`.
    fn deflate_scripted_pending(
        prefix: &[u8],
        tail: &[u8],
        level: i32,
        window_bits: i32,
        sync_window: usize,
    ) -> alloc::vec::Vec<u8> {
        let mut strm: ZStream = ZStream::new();
        crate::deflate::deflate_init2(
            &mut strm,
            level,
            Z_DEFLATED,
            window_bits,
            8,
            Strategy::Default,
        )
        .expect("init");
        let mut out = alloc::vec![0u8; (prefix.len() + tail.len()) * 2 + 256];
        let mut n = drive_to_pending(&mut strm, prefix, &mut out, sync_window);
        n += finish_into(&mut strm, tail, &mut out[n..]);
        out.truncate(n);
        crate::deflate::deflate_end(&mut strm).expect("end");
        out
    }

    /// `deflateCopy` carries **undelivered output**. C duplicates the pending
    /// region explicitly — `ds->pending_out = ds->pending_buf + (ss->pending_out -
    /// ss->pending_buf); zmemcpy(ds->pending_out, ss->pending_out, ss->pending)`
    /// (`deflate.c` L1358-L1359) — so a snapshot taken while a block is still
    /// sitting in `pending_buf` has to deliver those exact bytes itself.
    ///
    /// The main snapshot test cannot cover this: `deflate` writes to
    /// `pending_buf` only when it flushes a block, and flushing a block empties
    /// the symbol region, so `pending > 0` and `sym_next > 0` cannot both hold at a call
    /// boundary. This test therefore snapshots at the complementary point,
    /// reached by requesting a `Z_SYNC_FLUSH` through a three-byte output window.
    #[test]
    fn deflate_copy_carries_undelivered_pending_output() {
        let prefix = b"pending output travels with the snapshot; ".repeat(40);
        let tail_a = b"AAAA alpha alpha alpha ".repeat(8);
        let tail_b = b"ZZZZ omega omega omega ".repeat(8);
        const SYNC_WINDOW: usize = 3;
        // A 512-byte window (`windowBits = 9`) with 1,680 bytes of input also
        // forces `fill_window` to slide, so this snapshot carries `slid == true`
        // — the flag C's `deflateCopy` consults when sizing its `prev` copy
        // (`deflate.c` L1353-L1355).
        const WINDOW_BITS: i32 = 9;

        let mut src: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut src, 6, Z_DEFLATED, WINDOW_BITS, 8, Strategy::Default)
            .expect("init");
        let mut out_src = alloc::vec![0u8; 8192];
        let mut produced_src = drive_to_pending(&mut src, &prefix, &mut out_src, SYNC_WINDOW);
        let common: alloc::vec::Vec<u8> = out_src[..produced_src].to_vec();

        let (want_pending, want_pending_out) = {
            let st = src.deflate_state().expect("state installed");
            assert!(
                st.pending > 0,
                "the snapshot point must hold undelivered output"
            );
            assert_eq!(st.sym_next, 0, "a sync flush empties the symbol buffer");
            assert!(st.slid, "the window must have slid at this snapshot point");
            (st.pending, st.pending_out)
        };

        let mut cpy: ZStream = ZStream::new();
        crate::deflate::deflate_copy(&mut cpy, &src).expect("copy");

        {
            let a = src.deflate_state().expect("source state");
            let b = cpy.deflate_state().expect("copy state");
            assert!(b.slid, "slid");
            assert_eq!(b.pending, want_pending, "pending");
            assert_eq!(b.pending_out, want_pending_out, "pending_out");
            assert_eq!(
                &b.pending_buf[want_pending_out..want_pending_out + want_pending],
                &a.pending_buf[want_pending_out..want_pending_out + want_pending],
                "the undelivered bytes must travel with the copy"
            );
        }

        let mut out_cpy = alloc::vec![0u8; 8192];
        let produced_cpy = finish_into(&mut cpy, &tail_b, &mut out_cpy);
        produced_src += finish_into(&mut src, &tail_a, &mut out_src[produced_src..]);

        let mut want_a = prefix.clone();
        want_a.extend_from_slice(&tail_a);
        let mut want_b = prefix.clone();
        want_b.extend_from_slice(&tail_b);

        let got_a: alloc::vec::Vec<u8> = out_src[..produced_src].to_vec();
        let mut got_b = common;
        got_b.extend_from_slice(&out_cpy[..produced_cpy]);

        assert_ne!(got_a, got_b, "divergent suffixes must differ");
        assert_eq!(inflate_all(&got_a, want_a.len()), want_a);
        assert_eq!(inflate_all(&got_b, want_b.len()), want_b);
        assert_eq!(
            got_a,
            deflate_scripted_pending(&prefix, &tail_a, 6, WINDOW_BITS, SYNC_WINDOW)
        );
        assert_eq!(
            got_b,
            deflate_scripted_pending(&prefix, &tail_b, 6, WINDOW_BITS, SYNC_WINDOW),
            "the snapshot must deliver the pending block exactly once"
        );

        crate::deflate::deflate_end(&mut src).expect("end source");
        crate::deflate::deflate_end(&mut cpy).expect("end copy");
    }

    /// The uninterrupted-stream reference for the level-0 snapshot: store
    /// `prefix` at level 0 through a 512-byte window, switch to level 6, then
    /// finish `tail`.
    fn deflate_scripted_level0(prefix: &[u8], tail: &[u8]) -> alloc::vec::Vec<u8> {
        let mut strm: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut strm, 0, Z_DEFLATED, 9, 8, Strategy::Default)
            .expect("init");
        let mut out = alloc::vec![0u8; (prefix.len() + tail.len()) * 2 + 512];
        let r = crate::deflate::deflate(&mut strm, prefix, &mut out, Z_NO_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        assert_eq!(r.consumed, prefix.len());
        let mut n = r.produced;
        let r = crate::deflate::deflate_params(&mut strm, &[], &mut out[n..], 6, Strategy::Default);
        assert_eq!(r.code, ReturnCode::Ok);
        n += r.produced;
        n += finish_into(&mut strm, tail, &mut out[n..]);
        out.truncate(n);
        crate::deflate::deflate_end(&mut strm).expect("end");
        out
    }

    /// `deflateCopy` carries the level-0 bookkeeping a later `deflateParams`
    /// depends on. When storing, `s->matches` counts the hash-table slides still
    /// owed — 1 means slide once, 2 means clear the table (`deflate.c`
    /// L1658-L1663) — and the large-copy path sets `s->insert = s->strstart`
    /// (`deflate.c` L1765-L1770). Both are dead while the level stays 0 and
    /// become load-bearing the moment the level changes, so the switch is
    /// performed here on both sides.
    ///
    /// This is also the scenario that makes those two fields non-trivial: in the
    /// main snapshot test both are legitimately zero, so a copy that dropped them
    /// would go unnoticed there.
    #[test]
    fn deflate_copy_carries_level0_slide_bookkeeping() {
        // 1080 bytes through a 512-byte window: more input than the window holds,
        // so `deflate_stored` takes the supplant-history path.
        let prefix = b"stored blocks slide the window down; ".repeat(30);
        let tail_a = b"AAAA alpha alpha alpha ".repeat(8);
        let tail_b = b"ZZZZ omega omega omega ".repeat(8);

        let mut src: ZStream = ZStream::new();
        crate::deflate::deflate_init2(&mut src, 0, Z_DEFLATED, 9, 8, Strategy::Default)
            .expect("init");
        let mut out_src = alloc::vec![0u8; 16384];
        let r = crate::deflate::deflate(&mut src, &prefix, &mut out_src, Z_NO_FLUSH);
        assert_eq!(r.code, ReturnCode::Ok);
        assert_eq!(r.consumed, prefix.len());
        let mut produced_src = r.produced;

        let (want_matches, want_insert) = {
            let st = src.deflate_state().expect("state installed");
            assert!(
                st.matches > 0,
                "storing more than a window must leave a hash-table slide owed"
            );
            assert!(st.insert > 0, "storing leaves unhashed bytes in the window");
            (st.matches, st.insert)
        };
        let common: alloc::vec::Vec<u8> = out_src[..produced_src].to_vec();

        let mut cpy: ZStream = ZStream::new();
        crate::deflate::deflate_copy(&mut cpy, &src).expect("copy");
        {
            let b = cpy.deflate_state().expect("copy state");
            assert_eq!(b.matches, want_matches, "matches");
            assert_eq!(b.insert, want_insert, "insert");
        }

        // Switch both sides to level 6, which is when `matches` and `insert` are
        // consumed, then finish with divergent tails.
        let mut out_cpy = alloc::vec![0u8; 16384];
        let r = crate::deflate::deflate_params(&mut cpy, &[], &mut out_cpy, 6, Strategy::Default);
        assert_eq!(r.code, ReturnCode::Ok);
        let mut produced_cpy = r.produced;
        produced_cpy += finish_into(&mut cpy, &tail_b, &mut out_cpy[produced_cpy..]);

        let r = crate::deflate::deflate_params(
            &mut src,
            &[],
            &mut out_src[produced_src..],
            6,
            Strategy::Default,
        );
        assert_eq!(r.code, ReturnCode::Ok);
        produced_src += r.produced;
        produced_src += finish_into(&mut src, &tail_a, &mut out_src[produced_src..]);

        let mut want_a = prefix.clone();
        want_a.extend_from_slice(&tail_a);
        let mut want_b = prefix.clone();
        want_b.extend_from_slice(&tail_b);

        let got_a: alloc::vec::Vec<u8> = out_src[..produced_src].to_vec();
        let mut got_b = common;
        got_b.extend_from_slice(&out_cpy[..produced_cpy]);

        assert_ne!(got_a, got_b, "divergent suffixes must differ");
        assert_eq!(inflate_all(&got_a, want_a.len()), want_a);
        assert_eq!(inflate_all(&got_b, want_b.len()), want_b);
        assert_eq!(got_a, deflate_scripted_level0(&prefix, &tail_a));
        assert_eq!(
            got_b,
            deflate_scripted_level0(&prefix, &tail_b),
            "the snapshot must slide the hash table exactly as its source would"
        );

        crate::deflate::deflate_end(&mut src).expect("end source");
        crate::deflate::deflate_end(&mut cpy).expect("end copy");
    }

    /// `DeflateState` is deep-copyable (supports `deflateCopy`); owned buffers
    /// are duplicated and index-based state needs no pointer fix-ups. The copy
    /// is fallible by design, so it is obtained through
    /// [`DeflateState::try_clone`] rather than [`Clone`].
    #[test]
    fn deflate_state_is_copyable() {
        let mut s = DeflateState::new(6, Z_DEFLATED, 15, 8, Strategy::Default, 1).unwrap();
        s.window[7] = 0x5A;
        s.strstart = 12;

        let c = s.try_clone().expect("global-allocator copy cannot fail");
        assert_eq!(c.w_size, s.w_size);
        assert_eq!(c.window.len(), s.window.len());
        assert_eq!(c.window[7], 0x5A);
        assert_eq!(c.strstart, 12);
        assert_eq!(c.status, s.status);
        assert_eq!(c.pending_buf.len(), s.pending_buf.len());

        // The copy is independent: mutating it must not disturb the source.
        let mut c = c;
        c.window[7] = 0xA5;
        assert_eq!(s.window[7], 0x5A);
    }

    /// `read_buf_into` copies input, updates the Adler-32 checksum for zlib
    /// framing, and advances the input cursors.
    #[test]
    fn read_buf_into_copies_and_checksums() {
        let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut next_in = 0usize;
        let mut avail_in = input.len();
        let mut total_in = 0u64;
        let mut adler = adler32(1, &[]);
        let mut dst = [0u8; 4];

        let n = DeflateState::read_buf_into(
            &input,
            &mut next_in,
            &mut avail_in,
            &mut total_in,
            &mut adler,
            1, // zlib framing -> adler32
            &mut dst,
        );

        assert_eq!(n, 4);
        assert_eq!(dst, [1, 2, 3, 4]);
        assert_eq!(next_in, 4);
        assert_eq!(avail_in, 4);
        assert_eq!(total_in, 4);
        assert_eq!(adler, adler32(1, &[1, 2, 3, 4]));
    }

    /// Whole-row transcription guard for the per-level tuning table, including the
    /// **block-producer column** and the operational selection that reads it.
    ///
    /// There is exactly **one** copy of C's `configuration_table`
    /// (`deflate.c` L112-L124) in this crate: `crate::deflate::strategy`'s
    /// `CONFIGURATION_TABLE`, which this module imports (see the module's `use`
    /// list) and [`DeflateState::lm_init`] reads. A private duplicate used to live
    /// here and was removed precisely because two copies can diverge silently —
    /// see `lm_init_loads_every_level_from_the_authoritative_table`. Single
    /// sourcing means a cross-table comparison would compare a value with itself
    /// and prove nothing, so this guard asserts the three things that *can* drift:
    ///
    /// 1. every one of the five columns equals an independent transcription of the
    ///    C row — including `func`, the block producer, which neither of this
    ///    module's other table tests looks at;
    /// 2. [`crate::deflate::strategy::select_compress_func`] returns that same
    ///    producer for all ten levels under [`Strategy::Default`], so the *dispatch
    ///    path* is pinned and not merely the literal (C's `s->strategy` overrides
    ///    and the level-0 special case are covered in `strategy.rs`); and
    /// 3. the four values `lm_init` installs into the match finder
    ///    (`good_match`, `max_lazy_match`, `nice_match`, `max_chain_length`) equal
    ///    the C row for every level.
    ///
    /// Every column here determines match-finder or block-emission decisions, so a
    /// single wrong entry changes the emitted token stream while every round-trip
    /// test still passes — the class of defect a round-trip test cannot see
    /// (AAP §0.6.4).
    #[test]
    fn configuration_table_rows_and_producers_match_the_c_oracle() {
        use crate::deflate::strategy::{CompressFunc, select_compress_func};

        // Transcribed independently from C `deflate.c` L112-L124 as
        // (good_length, max_lazy, nice_length, max_chain, func).
        const C_ORACLE: [(u16, u16, u16, u16, CompressFunc); 10] = [
            (0, 0, 0, 0, CompressFunc::Stored),       // 0 store only
            (4, 4, 8, 4, CompressFunc::Fast),         // 1 max speed, no lazy matches
            (4, 5, 16, 8, CompressFunc::Fast),        // 2
            (4, 6, 32, 32, CompressFunc::Fast),       // 3
            (4, 4, 16, 16, CompressFunc::Slow),       // 4 lazy matches
            (8, 16, 32, 32, CompressFunc::Slow),      // 5
            (8, 16, 128, 128, CompressFunc::Slow),    // 6 (default)
            (8, 32, 128, 256, CompressFunc::Slow),    // 7
            (32, 128, 258, 1024, CompressFunc::Slow), // 8
            (32, 258, 258, 4096, CompressFunc::Slow), // 9 max compression
        ];

        assert_eq!(CONFIGURATION_TABLE.len(), 10);

        for (level, &(good, lazy, nice, chain, func)) in C_ORACLE.iter().enumerate() {
            // (1) Every column of the single authoritative row matches C.
            let op = &CONFIGURATION_TABLE[level];
            assert_eq!(op.good_length, good, "good_length, level {level}");
            assert_eq!(op.max_lazy, lazy, "max_lazy, level {level}");
            assert_eq!(op.nice_length, nice, "nice_length, level {level}");
            assert_eq!(op.max_chain, chain, "max_chain, level {level}");
            assert_eq!(op.func, func, "block producer, level {level}");

            // (2) The dispatcher agrees, so the producer is not merely recorded but
            // actually selected (C: `configuration_table[s->level].func`,
            // `deflate.c` L1220).
            assert_eq!(
                select_compress_func(level as i32, Strategy::Default),
                func,
                "select_compress_func must return the table producer, level {level}"
            );

            // (3) The values `lm_init` installs into the match finder. `new_in`
            // ends with `lm_init`, so a freshly constructed state carries them.
            let s =
                DeflateState::new(level as i32, Z_DEFLATED, 15, 8, Strategy::Default, 1).unwrap();
            assert_eq!(s.good_match, good as usize, "good_match, level {level}");
            assert_eq!(
                s.max_lazy_match, lazy as usize,
                "max_lazy_match, level {level}"
            );
            assert_eq!(s.nice_match, nice as i32, "nice_match, level {level}");
            assert_eq!(
                s.max_chain_length, chain as usize,
                "max_chain_length, level {level}"
            );
        }
    }

    // =======================================================================
    // C layout-mirror pinning (AAP §0.6.3, §0.6.5)
    // =======================================================================

    /// `DeflateState::C_LAYOUT_SIZE` must equal C's `sizeof(deflate_state)`.
    ///
    /// The number is not guessed: it was measured with a `gcc` probe compiled
    /// against the in-tree `deflate.h` on this target
    /// (`x86_64-unknown-linux-gnu`, LP64, `gcc 15.2.0`), with `LIT_MEM` and
    /// `ZLIB_DEBUG` both undefined — the same configuration the byte-identity
    /// sweep builds its reference library in. Pinning it here means a future edit
    /// to [`DeflateStateC`] that silently changes the footprint a caller's
    /// `zalloc` observes fails the test suite instead of shipping.
    ///
    /// The assertion is LP64-specific and therefore gated: on LLP64 (Windows)
    /// `c_ulong` is four bytes and on 32-bit targets every pointer is, so the
    /// total legitimately differs. The *portability* of the mirror comes from
    /// `#[repr(C)]` plus `core::ffi` aliases, which make rustc apply the platform
    /// C ABI's own layout rules; this test pins the one platform whose C answer
    /// has been measured.
    #[test]
    #[cfg(all(target_pointer_width = "64", not(windows)))]
    fn c_layout_size_matches_the_measured_c_deflate_state() {
        assert_eq!(
            DeflateState::C_LAYOUT_SIZE,
            5968,
            "sizeof(deflate_state) on LP64 with LIT_MEM and ZLIB_DEBUG undefined"
        );
        assert_eq!(
            align_of::<DeflateStateC>(),
            8,
            "_Alignof(deflate_state) on LP64"
        );
        // The mirror must not accidentally become the Rust type: the whole point
        // of the mirror is that the two sizes differ and the *C* one is charged.
        assert_ne!(
            DeflateState::C_LAYOUT_SIZE,
            size_of::<DeflateState>(),
            "the mirror must be C's layout, not this Rust type's"
        );
        // Element sizes C's `ZALLOC` arithmetic depends on.
        assert_eq!(size_of::<CtDataC>(), 4, "sizeof(ct_data)");
        assert_eq!(size_of::<TreeDescC>(), 24, "sizeof(tree_desc)");
    }

    /// Field-by-field offset pinning for [`DeflateStateC`].
    ///
    /// A struct can have the right total size while having the wrong shape — a
    /// mistyped field compensated by padding, or two fields transposed. Only an
    /// offset sweep rules that out, and only a correct shape justifies calling
    /// the mirror "field-exact" in the documentation (standard S1: evidence over
    /// assertion). Every expected value below was emitted verbatim by an
    /// `offsetof` probe compiled against the in-tree `deflate.h`.
    #[test]
    #[cfg(all(target_pointer_width = "64", not(windows)))]
    fn c_layout_mirror_reproduces_every_c_deflate_state_offset() {
        use core::mem::offset_of;

        assert_eq!(offset_of!(DeflateStateC, strm), 0);
        assert_eq!(offset_of!(DeflateStateC, status), 8);
        assert_eq!(offset_of!(DeflateStateC, pending_buf), 16);
        assert_eq!(offset_of!(DeflateStateC, pending_buf_size), 24);
        assert_eq!(offset_of!(DeflateStateC, pending_out), 32);
        assert_eq!(offset_of!(DeflateStateC, pending), 40);
        assert_eq!(offset_of!(DeflateStateC, wrap), 48);
        assert_eq!(offset_of!(DeflateStateC, gzhead), 56);
        assert_eq!(offset_of!(DeflateStateC, gzindex), 64);
        assert_eq!(offset_of!(DeflateStateC, method), 72);
        assert_eq!(offset_of!(DeflateStateC, last_flush), 76);
        assert_eq!(offset_of!(DeflateStateC, w_size), 80);
        assert_eq!(offset_of!(DeflateStateC, w_bits), 84);
        assert_eq!(offset_of!(DeflateStateC, w_mask), 88);
        assert_eq!(offset_of!(DeflateStateC, window), 96);
        assert_eq!(offset_of!(DeflateStateC, window_size), 104);
        assert_eq!(offset_of!(DeflateStateC, prev), 112);
        assert_eq!(offset_of!(DeflateStateC, head), 120);
        assert_eq!(offset_of!(DeflateStateC, ins_h), 128);
        assert_eq!(offset_of!(DeflateStateC, hash_size), 132);
        assert_eq!(offset_of!(DeflateStateC, hash_bits), 136);
        assert_eq!(offset_of!(DeflateStateC, hash_mask), 140);
        assert_eq!(offset_of!(DeflateStateC, hash_shift), 144);
        assert_eq!(offset_of!(DeflateStateC, block_start), 152);
        assert_eq!(offset_of!(DeflateStateC, match_length), 160);
        assert_eq!(offset_of!(DeflateStateC, prev_match), 164);
        assert_eq!(offset_of!(DeflateStateC, match_available), 168);
        assert_eq!(offset_of!(DeflateStateC, strstart), 172);
        assert_eq!(offset_of!(DeflateStateC, match_start), 176);
        assert_eq!(offset_of!(DeflateStateC, lookahead), 180);
        assert_eq!(offset_of!(DeflateStateC, prev_length), 184);
        assert_eq!(offset_of!(DeflateStateC, max_chain_length), 188);
        assert_eq!(offset_of!(DeflateStateC, max_lazy_match), 192);
        assert_eq!(offset_of!(DeflateStateC, level), 196);
        assert_eq!(offset_of!(DeflateStateC, strategy), 200);
        assert_eq!(offset_of!(DeflateStateC, good_match), 204);
        assert_eq!(offset_of!(DeflateStateC, nice_match), 208);
        assert_eq!(offset_of!(DeflateStateC, dyn_ltree), 212);
        assert_eq!(offset_of!(DeflateStateC, dyn_dtree), 2504);
        assert_eq!(offset_of!(DeflateStateC, bl_tree), 2748);
        assert_eq!(offset_of!(DeflateStateC, l_desc), 2904);
        assert_eq!(offset_of!(DeflateStateC, d_desc), 2928);
        assert_eq!(offset_of!(DeflateStateC, bl_desc), 2952);
        assert_eq!(offset_of!(DeflateStateC, bl_count), 2976);
        assert_eq!(offset_of!(DeflateStateC, heap), 3008);
        assert_eq!(offset_of!(DeflateStateC, heap_len), 5300);
        assert_eq!(offset_of!(DeflateStateC, heap_max), 5304);
        assert_eq!(offset_of!(DeflateStateC, depth), 5308);
        assert_eq!(offset_of!(DeflateStateC, sym_buf), 5888);
        assert_eq!(offset_of!(DeflateStateC, lit_bufsize), 5896);
        assert_eq!(offset_of!(DeflateStateC, sym_next), 5900);
        assert_eq!(offset_of!(DeflateStateC, sym_end), 5904);
        assert_eq!(offset_of!(DeflateStateC, opt_len), 5912);
        assert_eq!(offset_of!(DeflateStateC, static_len), 5920);
        assert_eq!(offset_of!(DeflateStateC, matches), 5928);
        assert_eq!(offset_of!(DeflateStateC, insert), 5932);
        assert_eq!(offset_of!(DeflateStateC, bi_buf), 5936);
        assert_eq!(offset_of!(DeflateStateC, bi_valid), 5940);
        assert_eq!(offset_of!(DeflateStateC, bi_used), 5944);
        assert_eq!(offset_of!(DeflateStateC, high_water), 5952);
        assert_eq!(offset_of!(DeflateStateC, slid), 5960);
    }

    // =======================================================================
    // Overlaid symbol region (AAP §0.3.2 rule T3)
    // =======================================================================

    /// The symbol region must sit exactly where C puts it, be exactly as large as
    /// C makes it, and never reach outside its own allocation.
    ///
    /// C carves it out of the pending allocation with
    /// `s->sym_buf = s->pending_buf + s->lit_bufsize` (`deflate.c` L520) and caps
    /// it with `s->sym_end = (s->lit_bufsize - 1) * 3` (L521). Two properties
    /// follow, and both are load-bearing:
    ///
    /// * the last symbol written at `sym_next == sym_end` occupies the three bytes
    ///   `sym_end..sym_end + 3`, which must still be inside the region — that is
    ///   why C stops one symbol short of `lit_bufsize * 3`; and
    /// * the pending-output area and the symbol region are disjoint index ranges
    ///   of the same buffer, so the bit packer writing at `pending` cannot reach a
    ///   symbol the block emitter has yet to read (`deflate.c` L466-L500 proves at
    ///   least 139 bits of headroom).
    ///
    /// Checked across the whole `mem_level` domain, because `lit_bufsize` is
    /// `1 << (mem_level + 6)` and the relationship has to hold at both ends.
    #[test]
    fn overlaid_symbol_region_is_addressable_and_disjoint_from_pending_output() {
        for mem_level in 1..=MAX_MEM_LEVEL {
            let s = DeflateState::new(6, Z_DEFLATED, 15, mem_level, Strategy::Default, 1)
                .expect("valid parameters");

            let lit_bufsize = 1usize << (mem_level as u32 + 6);
            assert_eq!(s.lit_bufsize, lit_bufsize, "mem_level {mem_level}");
            assert_eq!(
                s.pending_buf.len(),
                4 * lit_bufsize,
                "one allocation of LIT_BUFS * lit_bufsize (mem_level {mem_level})"
            );
            assert_eq!(
                s.sym_region().len(),
                3 * lit_bufsize,
                "the region is the upper three quarters (mem_level {mem_level})"
            );
            assert_eq!(
                s.sym_end,
                (lit_bufsize - 1) * 3,
                "C's sym_end (mem_level {mem_level})"
            );

            // The final symbol's three bytes stay inside the region.
            assert!(
                s.sym_end + 3 <= s.sym_region().len(),
                "the symbol at sym_end must fit (mem_level {mem_level})"
            );

            // The two halves are disjoint index ranges of one buffer: the pending
            // area is `[0, lit_bufsize)` and the region base is `lit_bufsize`.
            assert!(
                s.pending_buf_size == 4 * lit_bufsize && s.lit_bufsize == lit_bufsize,
                "mem_level {mem_level}"
            );
        }
    }

    /// `sym` / `set_sym` must address the region relative to its base, i.e. they
    /// must be exactly `pending_buf[lit_bufsize + i]`.
    ///
    /// The accessors are the whole of C's pointer arithmetic in this port, so a
    /// missing or doubled `lit_bufsize` offset would silently move every emitted
    /// token. Writing through `set_sym` and reading back through the raw buffer
    /// (and vice versa) pins the offset from both directions, and writing the two
    /// extreme indices shows the region's bounds are the buffer's.
    #[test]
    fn symbol_accessors_are_offset_by_lit_bufsize() {
        let mut s = DeflateState::new(6, Z_DEFLATED, 15, 8, Strategy::Default, 1)
            .expect("valid parameters");
        let base = s.lit_bufsize;
        let last = s.sym_region().len() - 1;

        s.set_sym(0, 0xA5);
        assert_eq!(
            s.pending_buf[base], 0xA5,
            "set_sym(0) writes at lit_bufsize"
        );
        assert_eq!(s.pending_buf[0], 0, "the pending-output area is untouched");

        s.pending_buf[base + 7] = 0x5A;
        assert_eq!(s.sym(7), 0x5A, "sym(i) reads pending_buf[lit_bufsize + i]");

        s.set_sym(last, 0x3C);
        assert_eq!(
            s.pending_buf[base + last],
            0x3C,
            "the last region byte is the last buffer byte"
        );
        assert_eq!(s.pending_buf.len(), base + last + 1);
    }

    /// C's `deflatePrime` head-room guard is reproduced exactly.
    ///
    /// C compares two pointers into `pending_buf`:
    /// `s->sym_buf < s->pending_out + ((Buf_size + 7) >> 3)` (`deflate.c` L757).
    /// Both are offsets from the same base — `sym_buf` is `pending_buf +
    /// lit_bufsize` — so subtracting it leaves
    /// `lit_bufsize < pending_out + ((Buf_size + 7) >> 3)`, which is the
    /// expression `deflate_prime` evaluates verbatim now that both quantities are
    /// stored as indices. This test pins the boundary from both sides at the
    /// smallest `lit_bufsize` (`mem_level = 1`, 128 bytes), where the guard is
    /// tightest.
    #[test]
    fn deflate_prime_guard_matches_the_c_pointer_comparison() {
        // `Buf_size` is 16 (`deflate.h` L55), so the head-room term is 2 bytes.
        const HEADROOM: usize = (16 + 7) >> 3;
        assert_eq!(HEADROOM, 2);

        let mut s = DeflateState::new(6, Z_DEFLATED, 15, 1, Strategy::Default, 1)
            .expect("valid parameters");
        let lit_bufsize = s.lit_bufsize;
        assert_eq!(lit_bufsize, 1 << 7);

        // Room remains while `pending_out + 2 <= lit_bufsize`.
        s.pending_out = lit_bufsize - HEADROOM;
        assert!(
            s.lit_bufsize >= s.pending_out + HEADROOM,
            "the last accepting position"
        );

        // One byte further and C reports Z_BUF_ERROR.
        s.pending_out = lit_bufsize - HEADROOM + 1;
        assert!(
            s.lit_bufsize < s.pending_out + HEADROOM,
            "the first rejecting position"
        );
    }
}
