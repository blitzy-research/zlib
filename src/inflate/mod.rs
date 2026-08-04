//! DEFLATE decompression engine — a safe Rust port of zlib's `inflate.c`.
//!
//! This module is the root of the `src/inflate/` tree. It declares the engine
//! submodules, re-exports the public inflate surface, and — most importantly —
//! contains the central [`inflate`] mode-loop state machine together with the
//! full inflate public API (init / reset / dictionary / sync / copy / prime /
//! mark / header / validate and friends).
//!
//! # Byte-exact fidelity
//!
//! The decoded output and the error/recovery behaviour of this module are
//! **byte-identical** to reference zlib for the same compressed input — that is
//! the defining acceptance criterion (AAP §0.6.1, §0.6.4, §0.8.1 directive D-1). Every check,
//! every diagnostic message string, and the exact order of bit reads are
//! preserved verbatim from `inflate.c`. In particular:
//!
//! * The `data_type` formula reported on return (`inflate.c` L1147-L1149) is
//!   reproduced exactly.
//! * The zlib Adler-32 trailer and the gzip CRC-32 + ISIZE trailers are checked
//!   with the same byte order and the same "incorrect data/length check"
//!   diagnostics.
//! * `inflateSync` scans for the `00 00 FF FF` marker and restarts decoding
//!   with the identical state fix-up C performs.
//!
//! # Idiomatic wins over C
//!
//! * The C `switch (state->mode)` with `goto` fall-through becomes an
//!   exhaustive `match` over [`InflateMode`] inside a `'inf_leave` loop; the
//!   compiler statically guarantees no mode is left unhandled (AAP §0.6.1). C
//!   `break` (re-enter the `for(;;)`) becomes `continue 'inf_leave`, and C
//!   `goto inf_leave` becomes `break 'inf_leave`.
//! * [`inflate_copy`] is a straightforward deep clone: because the state uses
//!   `usize` table offsets plus a fixed/dynamic discriminator instead of
//!   self-referential raw pointers, the C pointer-fix-up dance in `inflateCopy`
//!   is unnecessary — a genuine memory-safety win.
//! * Dropping the boxed [`InflateState`] frees the sliding window automatically,
//!   so [`inflate_end`] merely detaches the state (RAII subsumes the
//!   `ZFREE(window)` that C `inflateEnd` performs).
//!
//! # Safety and portability
//!
//! There is **zero `unsafe`** anywhere in this file, and the same holds for
//! every other module under `src/inflate/**` — including [`fast`], the hot
//! decode loop. All raw-pointer and `extern "C"` work lives at the FFI boundary
//! in `src/ffi/**` (AAP §0.6.2 / §0.7.2 standard S2), so the decoder is written
//! entirely in safe, bounds-checked Rust. The module is `no_std` + `alloc`
//! and targets the Rust 2024 edition (MSRV 1.85.0). All gzip-framing code is
//! gated behind the `gzip` cargo feature; with gzip disabled, [`inflate`]
//! still fully handles zlib and raw DEFLATE streams.

// ---------------------------------------------------------------------------
// Submodule declarations.
//
// Declared `pub mod` because this file *is* `inflate/mod.rs`: the sibling
// modules reach each other by absolute path (e.g.
// `use crate::inflate::state::InflateState;`), and `src/stream.rs`,
// `src/util/uncompress.rs`, and `src/ffi/inflate.rs` reach into this tree.
// ---------------------------------------------------------------------------
pub mod back;
pub mod fast;
pub mod fixed;
pub mod state;
pub mod tables;

// Public re-exports: the state model and the table-builder surface the rest of
// the crate consumes, plus the raw-callback back-inflate API.
pub use back::{
    BackMsg, BackOutcome, InFunc, OutFunc, inflate_back, inflate_back_end, inflate_back_init,
};
pub use state::{InflateMode, InflateState};
pub use tables::{Code, ENOUGH, ENOUGH_DISTS, ENOUGH_LENS, MAXBITS, inflate_table};

// ---------------------------------------------------------------------------
// Imports.
// ---------------------------------------------------------------------------

use crate::checksum::adler32;
#[cfg(feature = "gzip")]
use crate::checksum::crc32;
use crate::constants::{DEF_WBITS, MAX_WBITS, Z_BLOCK, Z_DEFLATED, Z_FINISH, Z_TREES};
use crate::error::{ReturnCode, ZlibError};
#[cfg(feature = "gzip")]
use crate::gz_header::ForeignGzHeaderSink;
#[cfg(feature = "gzip")]
use crate::gz_header::GzHeader;
#[cfg(feature = "gzip")]
use crate::gz_header::HeaderDone;
use crate::gz_header::HeaderPublication;
use crate::stream::{AllocBuffer, Allocator, BoxedEngine, EngineReservation, ZStream, ZeroValid};

use crate::inflate::fast::inflate_fast;
use crate::inflate::fixed::{DISTFIX, LENFIX};
use crate::inflate::state::{InflateStream, TableSource};
use crate::inflate::tables::CodeType;
use crate::util::compress::OneCallStep;
use crate::util::uncompress::{OneCallInflate, uncompress2_with};

// ---------------------------------------------------------------------------
// Result / outcome types.
// ---------------------------------------------------------------------------

/// The internal `Result` alias used across the inflate init/reset/dictionary
/// API: `Ok` carries a success [`ReturnCode`] (typically [`ReturnCode::Ok`],
/// occasionally [`ReturnCode::NeedDict`]), while `Err` carries a [`ZlibError`].
/// The FFI boundary flattens both arms back to the integer C return codes.
pub type InflateResult = Result<ReturnCode, ZlibError>;

/// The result of a streaming [`inflate`] call.
///
/// Because [`ZStream`] carries no `next_in`/`next_out`/`avail_*` cursor fields
/// (input and output are passed as slices per call), progress is reported
/// explicitly: `consumed` bytes were read from `input` and `produced` bytes were
/// written to `output`. The FFI boundary uses these to advance the caller's
/// `z_stream` cursors. This mirrors the deflate engine's `DeflateOutcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InflateOutcome {
    /// The zlib return code produced by the call. Typically
    /// [`ReturnCode::Ok`], [`ReturnCode::StreamEnd`], [`ReturnCode::NeedDict`],
    /// [`ReturnCode::BufError`], [`ReturnCode::DataError`], or
    /// [`ReturnCode::StreamError`].
    pub code: ReturnCode,
    /// Number of input bytes consumed from the supplied `input` slice.
    pub consumed: usize,
    /// Number of output bytes written to the supplied `output` slice.
    pub produced: usize,
}

/// An [`InflateOutcome`] paired with the C-mirror total-commit flag that only
/// the FFI boundary needs — deliberately **crate-private**.
///
/// The flag is *not* a field of [`InflateOutcome`]: that type is part of this
/// crate's public API and mirrors the deflate engine's `DeflateOutcome`
/// field-for-field, so adding a field to it would break every downstream
/// exhaustive struct literal and destructuring pattern. Callers of the public
/// [`inflate`] need `code`/`consumed`/`produced` and nothing more; the C
/// `z_stream` mirror is an FFI concern, so it travels in this wrapper returned by
/// the crate-private [`inflate_tracked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrackedInflateOutcome {
    /// The public outcome, exactly as [`inflate`] returns it.
    pub(crate) outcome: InflateOutcome,
    /// Whether `outcome.consumed`/`outcome.produced` should also be added to a C
    /// `z_stream`'s `total_in`/`total_out` mirrors.
    ///
    /// Normally `true`: C's `inflate` epilogue advances the cursors and the
    /// running totals together (`inflate.c` L1139-L1142).
    ///
    /// It is `false` on the two paths where C executes its `RESTORE()` macro —
    /// committing `next_in`/`avail_in`/`next_out`/`avail_out` — and then returns
    /// *directly*, jumping over the `strm->total_in += in; strm->total_out += out;`
    /// bookkeeping at L1141-L1142:
    ///
    /// * `case DICT` with `havedict == 0`: `RESTORE(); return Z_NEED_DICT;`
    ///   (`inflate.c` L701-L703).
    /// * the `inf_leave` `updatewindow` failure: `RESTORE()` at L1132, then
    ///   `state->mode = MEM; return Z_MEM_ERROR;` at L1136-L1137.
    ///
    /// On those paths a C caller therefore receives the bytes and the advanced
    /// cursors while `total_in`/`total_out` stay behind — permanently, since the
    /// per-call `in`/`out` counters are recomputed from `avail_*` on the next
    /// entry. Reproducing that quirk is required for observable parity (AAP §0.8.1
    /// D-4, standard S5); measured against reference C, a preset-dictionary stream
    /// reports `total_in == 0` at its `Z_NEED_DICT` return and finishes at
    /// `total_in == 34` for a 40-byte stream.
    ///
    /// This concerns only the C `z_stream` mirror maintained by the FFI boundary.
    /// [`ZStream`]'s own `total_in`/`total_out` are already correct on both paths
    /// (the early returns bypass the epilogue that updates them), and pure-Rust
    /// callers that track their own byte counts from `consumed`/`produced` should
    /// keep counting normally — the bytes really were transferred.
    pub(crate) commit_totals: bool,
    /// Which of C's individual `state->head->…` assignments this call performed,
    /// so the FFI boundary can reproduce reference zlib's incremental gzip-header
    /// publication schedule instead of bulk-mirroring the owned header. See
    /// [`HeaderPublication`].
    pub(crate) header: HeaderPublication,
}

impl TrackedInflateOutcome {
    /// The ordinary epilogue result: C advances the cursors and the totals
    /// together (`inflate.c` L1139-L1142).
    #[inline]
    fn committed(
        code: ReturnCode,
        consumed: usize,
        produced: usize,
        header: HeaderPublication,
    ) -> Self {
        Self {
            outcome: InflateOutcome {
                code,
                consumed,
                produced,
            },
            commit_totals: true,
            header,
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

/// Reproduces the C `ZSWAP32` macro (`zutil.h` L258): a 32-bit byte swap used to
/// read the big-endian Adler-32 in a zlib trailer / dictionary id. Rust's
/// [`u32::swap_bytes`] is exactly this operation.
#[inline]
fn zswap32(q: u32) -> u32 {
    q.swap_bytes()
}

/// The Adler-32 checksum of the empty input (`adler32(0, Z_NULL, 0) == 1`).
///
/// Reference zlib obtains a fresh Adler-32 accumulator by calling
/// `adler32(0L, Z_NULL, 0)`; the idiomatic Rust [`adler32`] returns its input
/// unchanged for an empty slice, so the initializer is simply this literal `1`
/// (see the checksum-init idiom in the module port notes / AAP §0.6.4). Using
/// the literal avoids the incorrect `adler32(0, &[])`, which would yield `0`.
const ADLER32_INIT: u32 = 1;

/// The CRC-32 checksum of the empty input (`crc32(0L, Z_NULL, 0) == 0`).
///
/// The gzip counterpart of [`ADLER32_INIT`]: a fresh CRC-32 accumulator is the
/// literal `0`.
#[cfg(feature = "gzip")]
const CRC32_INIT: u32 = 0;

/// The running-check dispatch (C `UPDATE_CHECK` macro, `inflate.c` L301-L306):
/// folds `buf` into the running check value, choosing CRC-32 for a gzip stream
/// (`state.flags != 0`) or Adler-32 otherwise.
///
/// This is only ever invoked when the validate bit (`wrap & 4`) is set, which
/// never happens for a raw stream — so the `flags != 0` test cleanly selects
/// CRC-32 for gzip and Adler-32 for zlib.
#[cfg(feature = "gzip")]
#[inline]
fn update_check(flags: i32, check: u32, buf: &[u8]) -> u32 {
    if flags != 0 {
        crc32(check, buf)
    } else {
        adler32(check, buf)
    }
}

/// The running-check dispatch when gzip support is compiled out: always
/// Adler-32 (C `#else` branch of `UPDATE_CHECK`).
#[cfg(not(feature = "gzip"))]
#[inline]
fn update_check(_flags: i32, check: u32, buf: &[u8]) -> u32 {
    adler32(check, buf)
}

/// Folds the two low little-endian bytes of `word` into the gzip header CRC —
/// the Rust port of the C `CRC2` macro (`inflate.c` L143-L149).
///
/// Used while parsing a gzip header (`FLAGS`/`OS`/`EXLEN`/`HCRC`) to accumulate
/// the header's own CRC-16 (the low 16 bits of a running CRC-32), which is later
/// verified against the `FHCRC` trailer.
#[cfg(feature = "gzip")]
#[inline]
fn crc2(check: &mut u32, word: u32) {
    let hbuf = [word as u8, (word >> 8) as u8];
    *check = crc32(*check, &hbuf);
}

/// Folds the four little-endian bytes of `word` into the gzip header CRC — the
/// Rust port of the C `CRC4` macro (`inflate.c` L151-L160), used for the 32-bit
/// modification-time field in `TIME`.
#[cfg(feature = "gzip")]
#[inline]
fn crc4(check: &mut u32, word: u32) {
    let hbuf = [
        word as u8,
        (word >> 8) as u8,
        (word >> 16) as u8,
        (word >> 24) as u8,
    ];
    *check = crc32(*check, &hbuf);
}

/// The permutation of code-length code indices used while reading a dynamic
/// block's code-length code lengths (C `order[19]`, `inflate.c` L491-L492).
const ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

// ===========================================================================
// InflateIo — the input/output cursor + bit-accumulator bundle.
//
// This is the slice-based analogue of the C `next`/`have`/`put`/`left`/`hold`/
// `bits` locals threaded through `inflate()`. Keeping them in one struct with
// small `#[inline]` methods reproduces the C `LOAD`/`RESTORE`/`NEEDBITS`/…
// macro group without any `unsafe`, and mirrors the `BackCtx` design used by
// the sibling `back.rs`.
// ===========================================================================

/// Bit-accumulator + I/O cursor bundle threaded through [`inflate`].
struct InflateIo<'a> {
    /// The available input bytes (C `next` points into this; `avail_in` is its
    /// length).
    input: &'a [u8],
    /// Read cursor into [`input`](InflateIo::input); equal to the number of
    /// input bytes consumed so far this call (C `next - base`; `have` is
    /// `input.len() - next`).
    next: usize,
    /// The output buffer (C `put` points into this; `avail_out` is its length).
    output: &'a mut [u8],
    /// Write cursor into [`output`](InflateIo::output); equal to the number of
    /// output bytes produced so far this call (C `put - base`; `left` is
    /// `output.len() - put`).
    put: usize,
    /// Input bit accumulator (C `hold`; only the low 32 bits are ever used).
    hold: u32,
    /// Number of valid bits currently held in [`hold`](InflateIo::hold) (C
    /// `bits`).
    bits: u32,
}

impl InflateIo<'_> {
    /// Number of input bytes still available (C `have`).
    #[inline]
    fn have(&self) -> usize {
        self.input.len() - self.next
    }

    /// Number of output bytes of free space remaining (C `left`).
    #[inline]
    fn left(&self) -> usize {
        self.output.len() - self.put
    }

    /// C `PULLBYTE()`: pull one input byte into the bit accumulator.
    ///
    /// Returns [`None`] when no input is available — the caller translates that
    /// into `break 'inf_leave` (the C macro's `goto inf_leave`). The new byte's
    /// eight bits sit above the `bits` already present, so `|=` is exactly the C
    /// `+=` (no carry: every call site has `bits < 32`, and the paths that reach
    /// `NEEDBITS(32)` are byte-aligned, so the shift is at most 24).
    #[inline]
    fn pull_byte(&mut self) -> Option<()> {
        if self.have() == 0 {
            return None;
        }
        self.hold |= u32::from(self.input[self.next]) << self.bits;
        self.next += 1;
        self.bits += 8;
        Some(())
    }

    /// C `NEEDBITS(n)`: assure at least `n` bits are in the accumulator, pulling
    /// input bytes as needed. Returns [`None`] if input is exhausted first.
    #[inline]
    fn need_bits(&mut self, n: u32) -> Option<()> {
        while self.bits < n {
            self.pull_byte()?;
        }
        Some(())
    }

    /// C `BITS(n)`: return the low `n` bits of the accumulator (`n <= 16`).
    #[inline]
    fn bits_val(&self, n: u32) -> u32 {
        self.hold & ((1u32 << n) - 1)
    }

    /// C `DROPBITS(n)`: remove `n` bits from the accumulator.
    #[inline]
    fn drop_bits(&mut self, n: u32) {
        self.hold >>= n;
        self.bits -= n;
    }

    /// C `BYTEBITS()`: drop 0-7 bits to reach a byte boundary.
    #[inline]
    fn byte_align(&mut self) {
        let rem = self.bits & 7;
        self.hold >>= rem;
        self.bits -= rem;
    }

    /// C `INITBITS()`: clear the bit accumulator.
    #[inline]
    fn init_bits(&mut self) {
        self.hold = 0;
        self.bits = 0;
    }
}

// ===========================================================================
// Private helpers.
// ===========================================================================

/// Installs the fixed (static) Huffman decode tables, mirroring the
/// non-`BUILDFIXED` branch of C `fixedtables` (`inflate.c` L216-L260) which
/// simply points the decoder at the pre-generated `lenfix`/`distfix` tables.
///
/// Rather than copying the tables, the state records that the fixed tables are
/// in use ([`TableSource::Fixed`]) with the canonical root bit-lengths (`9` for
/// literals/lengths, `5` for distances); [`InflateState::lencode_slice`] and
/// [`InflateState::distcode_slice`] then resolve to [`LENFIX`]/[`DISTFIX`].
#[inline]
fn fixedtables(state: &mut InflateState) {
    state.lentable = TableSource::Fixed;
    state.lenbits = 9;
    state.disttable = TableSource::Fixed;
    state.distbits = 5;
}

/// Copies the most-recent `copy` output bytes into the circular sliding window,
/// lazily allocating the window on first use — a faithful port of C
/// `updatewindow` (`inflate.c` L252-L296).
///
/// `output[..end]` is the output produced during the current [`inflate`] call;
/// the bytes `output[end - copy .. end]` are the ones to fold into the window.
/// The wrap-around copy math is reproduced exactly, because `fast::inflate_fast`
/// relies on identical `wsize`/`whave`/`wnext` bookkeeping when it copies match
/// bytes out of the window.
///
/// The window is an owned [`AllocBuffer<u8>`]: when the owning stream carries a
/// caller-supplied `zalloc`/`zfree` (installed through the FFI `z_stream`), the
/// window is allocated through those hooks via the state's stored
/// [`alloc_hook`](InflateState::alloc_hook) (AAP §0.6.3);
/// otherwise it uses the Rust global allocator.
///
/// # Errors
///
/// Returns [`ZlibError::MemError`] when the lazy window allocation is routed
/// through an active caller hook whose `zalloc` reports out-of-memory. This is
/// the faithful port of C `updatewindow` returning `1` on `ZALLOC` failure,
/// which the callers translate into the `MEM` mode / `Z_MEM_ERROR`. The
/// global-allocator path is infallible (it aborts on OOM per Rust convention),
/// so this only fails for a caller-installed bounded allocator.
fn updatewindow<A: Allocator>(
    state: &mut InflateState,
    alloc: &A,
    output: &[u8],
    end: usize,
    mut copy: usize,
) -> Result<(), ZlibError> {
    // If it hasn't been done already, allocate space for the window — through the
    // owning stream's `Allocator`, so a custom Rust allocator serves it and a
    // caller-installed `zalloc` still backs it. C requests
    // `ZALLOC(strm, 1U << state->wbits, sizeof(unsigned char))` (`inflate.c`
    // L261), which is the element-shaped split, so `allocate_zeroed` forwards the
    // same argument pair. A refusal propagates as `Z_MEM_ERROR` rather than
    // falling back to the global allocator (AAP §0.6.3 has-hook clause).
    if state.window.is_empty() {
        state.window = alloc
            .allocate_zeroed::<u8>(1usize << state.wbits)
            .ok_or(ZlibError::MemError)?;
    }

    // If the window is not in use yet, initialise its geometry.
    let wsize = 1usize << state.wbits;
    if state.wsize == 0 {
        state.wsize = wsize as u32;
        state.wnext = 0;
        state.whave = 0;
    }
    let wsize = state.wsize as usize;

    // Copy `wsize` or fewer output bytes into the circular window.
    if copy >= wsize {
        // The last `wsize` bytes fill the whole window.
        state.window[..wsize].copy_from_slice(&output[end - wsize..end]);
        state.wnext = 0;
        state.whave = wsize as u32;
    } else {
        let wnext = state.wnext as usize;
        let mut dist = wsize - wnext;
        if dist > copy {
            dist = copy;
        }
        // First span: from `end - copy` into the window at `wnext`.
        state.window[wnext..wnext + dist].copy_from_slice(&output[end - copy..end - copy + dist]);
        copy -= dist;
        if copy > 0 {
            // Wrap-around span: remaining bytes go to the front of the window.
            state.window[..copy].copy_from_slice(&output[end - copy..end]);
            state.wnext = copy as u32;
            state.whave = wsize as u32;
        } else {
            let mut next = wnext + dist;
            if next == wsize {
                next = 0;
            }
            state.wnext = next as u32;
            if (state.whave as usize) < wsize {
                state.whave += dist as u32;
            }
        }
    }

    Ok(())
}

/// Scans `buf` for the 4-byte flush marker `00 00 FF FF`, a faithful port of C
/// `syncsearch` (`inflate.c` L1244-L1262).
///
/// `have` carries the number of pattern bytes matched so far (`0..=4`) across
/// calls; it is updated in place. The return value is the number of bytes of
/// `buf` consumed. When `*have` reaches `4` the full marker has been found.
fn syncsearch(have: &mut u32, buf: &[u8]) -> usize {
    let mut got = *have;
    let mut next = 0usize;
    let len = buf.len();
    while next < len && got < 4 {
        let want: u8 = if got < 2 { 0 } else { 0xff };
        if buf[next] == want {
            got += 1;
        } else if buf[next] != 0 {
            got = 0;
        } else {
            got = 4 - got;
        }
        next += 1;
    }
    *have = got;
    next
}

// ===========================================================================
// Reset / initialisation API.
// ===========================================================================

/// Resets the stream for a fresh decode **without** discarding the sliding
/// window contents — the Rust port of C `inflateResetKeep` (`inflate.c`
/// L100-L123).
///
/// Clears the total counters, `msg`, and `data_type`; restores the running
/// `adler` mirror to the wrapper's initial value (`wrap & 1`); and delegates the
/// per-stream decode-state reset to [`InflateState::reset_keep`]. The window
/// geometry ([`wsize`](InflateState::wsize)/[`whave`](InflateState::whave)/
/// [`wnext`](InflateState::wnext)) is intentionally left intact so history can
/// be reused (e.g. after [`inflate_sync`]).
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state installed.
pub fn inflate_reset_keep<A: Allocator>(strm: &mut ZStream<A>) -> InflateResult {
    let wrap = {
        let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
        state.reset_keep();
        state.wrap
    };
    strm.total_in = 0;
    strm.total_out = 0;
    strm.msg = None;
    strm.data_type = 0;
    // "to support ill-conceived Java test suite": expose the wrapper's initial
    // Adler value (1 for zlib/auto, 0 for gzip/raw) even before any data.
    if wrap != 0 {
        strm.adler = (wrap & 1) as u32;
    }
    Ok(ReturnCode::Ok)
}

/// Resets the stream for a fresh decode, additionally invalidating the sliding
/// window contents — the Rust port of C `inflateReset` (`inflate.c` L125-L134).
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state installed.
pub fn inflate_reset<A: Allocator>(strm: &mut ZStream<A>) -> InflateResult {
    {
        let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
        state.wsize = 0;
        state.whave = 0;
        state.wnext = 0;
    }
    inflate_reset_keep(strm)
}

/// Reconfigures the wrapper and window size, then resets — the Rust port of C
/// `inflateReset2` (`inflate.c` L136-L171).
///
/// `window_bits` is overloaded exactly as in zlib:
///
/// * `8..=15` — zlib (RFC 1950) wrapper, `wrap = 5` (check + validate).
/// * `-15..=-8` — raw DEFLATE, `wrap = 0` (no wrapper, no check).
/// * `24..=31` — gzip (RFC 1952) wrapper (`= 16 + windowBits`), `wrap = 6`.
/// * `40..=47` — automatic zlib/gzip detection (`= 32 + windowBits`),
///   `wrap = 7`.
/// * `0` — permitted; the effective window size is taken from the zlib header.
///
/// The `wrap` encoding is `(windowBits >> 4) + 5` for the non-negative cases:
/// bit 0 selects the zlib Adler check, bit 1 selects gzip, and bit 2 enables
/// check validation. When gzip support is compiled out, the `& 15` masking of
/// the gzip/auto ranges does not occur, so those requests fail the `8..=15`
/// bounds test — matching C built without `GUNZIP`.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] for an invalid `window_bits` or if `strm`
/// has no inflate state.
pub fn inflate_reset2<A: Allocator>(strm: &mut ZStream<A>, window_bits: i32) -> InflateResult {
    {
        let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;

        // Extract the wrap request from the windowBits parameter.
        let wrap;
        let mut wb = window_bits;
        if wb < 0 {
            if wb < -15 {
                return Err(ZlibError::StreamError);
            }
            wrap = 0;
            wb = -wb;
        } else {
            wrap = (wb >> 4) + 5;
            #[cfg(feature = "gzip")]
            if wb < 48 {
                wb &= 15;
            }
        }

        // Validate the window size (0 is allowed: "use the zlib header size").
        if wb != 0 && !(8..=MAX_WBITS).contains(&wb) {
            return Err(ZlibError::StreamError);
        }

        // Free the window if the size changed, so it is re-sized on next use.
        // Assigning an empty `AllocBuffer` drops the previous one, which routes
        // through the caller's `zfree` when the window was hook-backed
        // (AAP §0.6.3). The stored `alloc_hook` is left intact so
        // the re-allocation on next use goes through the same allocator.
        if !state.window.is_empty() && state.wbits != wb as u32 {
            state.window = AllocBuffer::default();
        }

        state.wrap = wrap;
        state.wbits = wb as u32;
    }
    inflate_reset(strm)
}

/// Allocates and initialises the inflate state for `strm` with the given
/// `window_bits` — the Rust port of C `inflateInit2_` (`inflate.c` L173-L212).
///
/// The version/`stream_size` compatibility check that C performs is a
/// concern of the FFI boundary (where a real `z_stream` layout exists); this
/// idiomatic entry point installs a fresh [`InflateState`] and delegates to
/// [`inflate_reset2`].
///
/// # Allocation and the caller's hook
/// C `inflateInit2_` allocates the state struct through the caller's
/// allocator (`ZALLOC(strm, 1, sizeof(struct inflate_state))`) before any
/// window is needed, so a null/failing `zalloc` fails the init with
/// `Z_MEM_ERROR`. This path reproduces that literally: when a hook is active it
/// asks the hook for the state's storage *first* and the state is then built
/// **inside the region the hook returned**, so a caller-supplied arena really
/// holds the `inflate_state` and gets it back through `zfree`
/// (AAP §0.6.3 has-hook clause). A hook whose `zalloc` reports out-of-memory
/// therefore surfaces [`ZlibError::MemError`] here — matching C's allocation
/// count (one at init for a single-shot inflate) and its failure timing. Under
/// the global allocator no hook request is made and the state is boxed as
/// before, keeping the crate's ~7 KB inflate memory-bounds parity (AAP §0.6.5).
/// The `inflateBack` init path does not go through here, so its single
/// (state-only) allocation is unaffected.
///
/// # Errors
/// Returns [`ZlibError::MemError`] when an active caller hook's `zalloc` reports
/// out-of-memory for the state, and propagates [`ZlibError::StreamError`] from
/// [`inflate_reset2`] for an invalid `window_bits`; the partially-installed state
/// is torn down on error.
pub fn inflate_init2<A: Allocator>(strm: &mut ZStream<A>, window_bits: i32) -> InflateResult {
    strm.msg = None;
    // Charge the state to the caller's allocator *first*, exactly where C does
    // (`inflate.c` L198-L200: `state = ZALLOC(strm, 1, sizeof(struct
    // inflate_state)); if (state == Z_NULL) return Z_MEM_ERROR;`), with C's own
    // argument pair `(1, InflateState::C_LAYOUT_SIZE)`, so an arena sized from C's
    // header serves this request exactly as it serves reference zlib's; the region
    // is handed back through their `zfree` after the window (AAP §0.6.5).
    //
    // Whether to charge at all is the allocator's decision
    // (`Allocator::reserves_state_footprint`): the global default declines,
    // because there the `Box` already *is* the allocation and a second region
    // would change the ~7 KB inflate memory-bounds parity (AAP §0.6.5); a custom
    // Rust allocator and an active C hook both take it. Reservation and
    // construction are two steps because the charge has to happen before the
    // state value exists, which is the only way to fail where C fails.
    let reservation =
        EngineReservation::<InflateState>::take(strm.allocator()).ok_or(ZlibError::MemError)?;
    // `build_in` sets mode = Head so the state passes reset2's inflate-state
    // guard; wrap/wbits are (re)assigned by inflate_reset2 below. The caller's
    // allocator hook (the `zalloc`/`zfree` installed via the FFI `z_stream`, or
    // a no-op under the global allocator) is threaded in so the lazily-allocated
    // window is later routed through it (AAP §0.6.3).
    let hook = strm.allocator().hook();
    // Filling boxes the state through a checked allocation, so heap exhaustion
    // becomes `Z_MEM_ERROR` rather than an abort; the caller's charge was already
    // secured above and is never re-requested here, so C's request count holds.
    let state = reservation
        .fill(InflateState::build_in(hook, 0, 0))
        .ok_or(ZlibError::MemError)?;
    strm.set_inflate_state(state);
    match inflate_reset2(strm, window_bits) {
        Ok(rc) => Ok(rc),
        Err(e) => {
            // Mirror C freeing the state and nulling strm->state on failure. The
            // state's storage drops with it, returning through the caller's
            // `zfree` when the hook supplied it.
            strm.clear_state();
            Err(e)
        }
    }
}

/// Allocates and initialises the inflate state using the default window size
/// ([`DEF_WBITS`] = 15) — the Rust port of C `inflateInit_` (`inflate.c`
/// L214-L217).
///
/// # Errors
/// See [`inflate_init2`].
pub fn inflate_init<A: Allocator>(strm: &mut ZStream<A>) -> InflateResult {
    inflate_init2(strm, DEF_WBITS)
}

/// Injects `bits` bits of `value` into the input bit accumulator — the Rust
/// port of C `inflatePrime` (`inflate.c` L219-L236).
///
/// This is used to resume decoding mid-byte (e.g. after [`inflate_copy`] or when
/// bits were pre-read by a caller). A negative `bits` flushes the accumulator; a
/// `bits` of `0` is a no-op. `bits` must be `<= 16` and must not overflow the
/// 32-bit accumulator.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state, if
/// `bits > 16`, or if adding `bits` would exceed the 32-bit accumulator.
pub fn inflate_prime<A: Allocator>(strm: &mut ZStream<A>, bits: i32, value: i32) -> InflateResult {
    let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
    if bits == 0 {
        return Ok(ReturnCode::Ok);
    }
    if bits < 0 {
        state.hold = 0;
        state.bits = 0;
        return Ok(ReturnCode::Ok);
    }
    if bits > 16 || state.bits + bits as u32 > 32 {
        return Err(ZlibError::StreamError);
    }
    let masked = (value as u32) & ((1u32 << bits) - 1);
    state.hold += masked << state.bits;
    state.bits += bits as u32;
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// inflate() — the central mode-loop state machine.
// ===========================================================================

/// Decompresses as much data as possible — the Rust port of C `inflate`
/// (`inflate.c` L474-L1152), the heart of the decoder.
///
/// This consumes bytes from `input` and produces bytes into `output`, advancing
/// the internal state machine ([`InflateMode`]) until it runs out of input, runs
/// out of output space, reaches a requested block boundary, hits the end of the
/// stream, or encounters an error. The number of bytes actually read and written
/// is reported in the returned [`InflateOutcome`] (reference zlib mutates the
/// `z_stream` cursors instead; the FFI boundary maps between the two).
///
/// # Flush modes
/// `flush` is one of the zlib flush constants; only [`Z_BLOCK`]
/// (stop at block boundaries), [`Z_TREES`] (also stop after the block header),
/// and [`Z_FINISH`] (expect the whole stream) alter behaviour here — every other
/// value is treated like `Z_NO_FLUSH`.
///
/// # Return codes
/// * [`ReturnCode::Ok`] — progress was made; call again with more room/input.
/// * [`ReturnCode::StreamEnd`] — the end of the compressed stream was reached.
/// * [`ReturnCode::NeedDict`] — a preset dictionary is required (its Adler-32 id
///   is placed in [`ZStream::adler`]); supply it via [`inflate_set_dictionary`].
/// * [`ReturnCode::BufError`] — no progress was possible (needs more input or
///   output space).
/// * [`ReturnCode::DataError`] — the input is corrupt; [`ZStream::msg`] describes
///   the fault with the same wording reference zlib uses.
/// * [`ReturnCode::StreamError`] — the stream state is inconsistent (e.g. no
///   inflate state installed).
///
/// # Byte-exact fidelity
/// Every state transition, bounds test, checksum fold, and diagnostic string is
/// reproduced verbatim from `inflate.c`; the reported [`ZStream::data_type`]
/// uses the exact C formula (L1147-L1149). The slow path here and the fast path
/// in [`fast::inflate_fast`] are both free of `unsafe`, and together decode
/// byte-identically to reference zlib.
pub fn inflate<A: Allocator>(
    strm: &mut ZStream<A>,
    input: &[u8],
    output: &mut [u8],
    flush: i32,
) -> InflateOutcome {
    inflate_tracked(strm, input, output, flush).outcome
}

/// [`inflate_tracked`] without a lent header sink — the shape every idiomatic
/// caller uses.
///
/// The idiomatic API lends the decoder an owned [`GzHeader`] through
/// [`inflate_get_header`], so there is no caller-owned buffer to write into and
/// the sink is always absent here. The C ABI calls
/// [`inflate_tracked_lending`] directly.
#[inline]
pub(crate) fn inflate_tracked<A: Allocator>(
    strm: &mut ZStream<A>,
    input: &[u8],
    output: &mut [u8],
    flush: i32,
) -> TrackedInflateOutcome {
    inflate_tracked_lending(
        strm,
        input,
        output,
        flush,
        #[cfg(feature = "gzip")]
        None,
    )
}

/// The full implementation of [`inflate`], additionally reporting whether the C
/// `z_stream` `total_in`/`total_out` mirrors may be advanced.
///
/// This is the crate-private entry point the FFI boundary calls; every public
/// caller goes through [`inflate`], which discards the extra flag. Keeping the
/// flag out of [`InflateOutcome`] preserves that type's public
/// `{code, consumed, produced}` shape (see [`TrackedInflateOutcome`]).
#[allow(clippy::too_many_lines)]
pub(crate) fn inflate_tracked_lending<A: Allocator>(
    strm: &mut ZStream<A>,
    input: &[u8],
    output: &mut [u8],
    flush: i32,
    #[cfg(feature = "gzip")] mut sink: Option<&mut ForeignGzHeaderSink<'_>>,
) -> TrackedInflateOutcome {
    // ---- guard: an inflate state must be installed (C `inflateStateCheck`) ---
    //
    // Take the boxed state *out* of `strm` for the duration of the call. This
    // decouples the borrows: `strm` stays fully available for `strm.msg`,
    // `strm.total_*`, and `strm.adler`, while the owned `state` is mutated
    // freely. Every exit path reinstalls the state via `set_inflate_state`.
    // `take_inflate_state` removes the engine only when it really is a
    // decompressor: an installed *compression* engine is left exactly where it
    // was found, which is C `inflateStateCheck` refusing a stream it must not
    // disturb (`inflate.c` L88-L97).
    let Some(mut placed) = strm.take_inflate_state() else {
        return TrackedInflateOutcome::committed(
            ReturnCode::StreamError,
            0,
            0,
            HeaderPublication::default(),
        );
    };

    // Reborrow the placed engine as a plain `&mut InflateState`. The engine may
    // live in a caller-supplied region rather than on the Rust heap, so the value
    // taken above is a placement wrapper; going through it on every access would
    // borrow the whole wrapper and defeat the disjoint field borrows this loop
    // relies on (the decode tables are read while `codes`/`work` are written).
    // One reborrow here restores them, and each exit path hands `placed` back.
    let state: &mut InflateState = &mut placed;

    // C: "if (state->mode == TYPE) state->mode = TYPEDO;  /* skip check */".
    // On re-entry at a block boundary, advance past the Z_BLOCK/Z_TREES early
    // exit so a fresh call makes progress rather than immediately returning.
    if state.mode == InflateMode::Type {
        state.mode = InflateMode::TypeDo;
    }

    // The fast path's input debt is strictly per-call output: zero it now so a
    // value left by an earlier call can never be mistaken for this one's.
    state.rewound = 0;

    // LOAD(): pull the bit accumulator into the local I/O context.
    let mut io = InflateIo {
        input,
        next: 0,
        output,
        put: 0,
        hold: state.hold,
        bits: state.bits,
    };

    // C snapshots `in = have; out = left;` at entry. `outck` is the running
    // "output checkpoint" (C's reused `out`): the free space at the last point
    // the running check was folded. It starts at the full output length and is
    // reset after the trailer check folds the final data bytes.
    let mut outck = io.left();
    let mut ret = ReturnCode::Ok;
    // Records which `state->head->…` assignments this call performs, so the FFI
    // boundary can replay exactly C's incremental publication schedule. With the
    // `gzip` feature off every gzip-header state is compiled out, so nothing ever
    // records anything and the empty record is published (a no-op).
    #[cfg_attr(not(feature = "gzip"), allow(unused_mut))]
    let mut header_pub = HeaderPublication::default();

    'inf_leave: loop {
        match state.mode {
            // ---------------------------------------------------------------
            // Stream header.
            // ---------------------------------------------------------------
            InflateMode::Head => {
                if state.wrap == 0 {
                    state.mode = InflateMode::TypeDo;
                    continue 'inf_leave;
                }
                if io.need_bits(16).is_none() {
                    break 'inf_leave;
                }
                #[cfg(feature = "gzip")]
                {
                    // gzip magic (0x1f, 0x8b) with gzip decoding enabled.
                    if (state.wrap & 2) != 0 && io.hold == 0x8b1f {
                        if state.wbits == 0 {
                            state.wbits = 15;
                        }
                        state.check = CRC32_INIT;
                        crc2(&mut state.check, io.hold);
                        io.init_bits();
                        state.mode = InflateMode::Flags;
                        continue 'inf_leave;
                    }
                    // C `if (state->head != Z_NULL) state->head->done = -1;`
                    // (`inflate.c` L505-L506): the stream carries no gzip header,
                    // so a registered header is marked "not gzip" rather than
                    // merely "not finished". The idiomatic `done` is a `bool` and
                    // stays `false`; the `-1` travels to the C caller through the
                    // publication record.
                    if state.head.is_some() {
                        header_pub.done = Some(HeaderDone::NotGzip);
                    }
                }
                // zlib header validation. The `wrap & 1` guard is present only
                // when gzip support exists (C wraps it in `#ifdef GUNZIP`).
                let header_bad = {
                    #[cfg(feature = "gzip")]
                    {
                        (state.wrap & 1) == 0 || ((io.bits_val(8) << 8) + (io.hold >> 8)) % 31 != 0
                    }
                    #[cfg(not(feature = "gzip"))]
                    {
                        ((io.bits_val(8) << 8) + (io.hold >> 8)) % 31 != 0
                    }
                };
                if header_bad {
                    strm.msg = Some("incorrect header check");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                if io.bits_val(4) != Z_DEFLATED as u32 {
                    strm.msg = Some("unknown compression method");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                io.drop_bits(4);
                let len = io.bits_val(4) + 8;
                if state.wbits == 0 {
                    state.wbits = len;
                }
                if len > 15 || len > state.wbits {
                    strm.msg = Some("invalid window size");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.dmax = 1u32 << len;
                state.flags = 0; // indicate zlib header
                // C `strm->adler = state->check = adler32(0L, Z_NULL, 0)`
                // (`inflate.c` L550): the caller-visible `adler` mirror is
                // published here, not left to the epilogue. Each of C's seven
                // `strm->adler` assignments is individually placed and guarded,
                // and reproducing them one-for-one is what lets the epilogue
                // carry C's own `(wrap & 4) && out` guard instead of writing
                // unconditionally (AAP §0.6.2 ABI field fidelity).
                state.check = ADLER32_INIT;
                strm.adler = state.check;
                state.mode = if (io.hold & 0x200) != 0 {
                    InflateMode::DictId
                } else {
                    InflateMode::Type
                };
                io.init_bits();
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // gzip header fields (RFC 1952). All gated on the `gzip` feature.
            // ---------------------------------------------------------------
            #[cfg(feature = "gzip")]
            InflateMode::Flags => {
                if io.need_bits(16).is_none() {
                    break 'inf_leave;
                }
                state.flags = io.hold as i32;
                if (state.flags & 0xff) != Z_DEFLATED {
                    strm.msg = Some("unknown compression method");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                if (state.flags & 0xe000) != 0 {
                    strm.msg = Some("unknown header flags set");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                if let Some(head) = state.head.as_mut() {
                    head.text = ((io.hold >> 8) & 1) != 0;
                    header_pub.text = true;
                }
                if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                    crc2(&mut state.check, io.hold);
                }
                io.init_bits();
                state.mode = InflateMode::Time;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::Time => {
                if io.need_bits(32).is_none() {
                    break 'inf_leave;
                }
                if let Some(head) = state.head.as_mut() {
                    head.time = io.hold;
                    header_pub.time = true;
                }
                if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                    crc4(&mut state.check, io.hold);
                }
                io.init_bits();
                state.mode = InflateMode::Os;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::Os => {
                if io.need_bits(16).is_none() {
                    break 'inf_leave;
                }
                if let Some(head) = state.head.as_mut() {
                    head.xflags = (io.hold & 0xff) as i32;
                    head.os = (io.hold >> 8) as i32;
                    header_pub.os = true;
                }
                if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                    crc2(&mut state.check, io.hold);
                }
                io.init_bits();
                state.mode = InflateMode::ExLen;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::ExLen => {
                if (state.flags & 0x0400) != 0 {
                    if io.need_bits(16).is_none() {
                        break 'inf_leave;
                    }
                    state.length = io.hold;
                    // Record the **declared** XLEN, matching C's
                    // `head->extra_len = (unsigned)hold` (`inflate.c` L599-L600).
                    // Note what C does *not* condition this on: it is written
                    // whenever a header is installed, irrespective of whether
                    // `extra` is non-null or how small `extra_max` is. That is
                    // deliberate and is the whole truncation contract of
                    // `inflateGetHeader` — the copy below is clamped to
                    // `extra_max` (`inflate.c` L614-L621) while this field keeps
                    // the true length, so `extra_len > extra_max` is the caller's
                    // only signal that bytes were dropped. The same unconditional
                    // write is also what lets a caller supply no `extra` buffer
                    // at all purely to learn the length — de-facto reference-zlib
                    // behavior rather than a `zlib.h`-documented pattern.
                    //
                    // It is a wire-level quantity with no idiomatic counterpart
                    // (`extra.len()` already reports what was captured), so it
                    // travels in the publication record and is published straight
                    // into the C caller's `extra_len` instead of occupying a field
                    // of the public `GzHeader`.
                    if state.head.is_some() {
                        header_pub.extra_len = Some(io.hold);
                        // C's `state->head->extra_len = (unsigned)hold` lands in
                        // the caller's struct *immediately*, and the `EXTRA` state
                        // derives its write offset from that very field — often in
                        // the same pass, via the fallthrough below. The lent view
                        // must therefore see the value now; the boundary publishes
                        // the identical value into the caller's `extra_len` when
                        // this call returns, so the next call's freshly
                        // materialized view reads it back from exactly where C
                        // keeps it.
                        if let Some(sk) = sink.as_deref_mut() {
                            sk.extra_len = io.hold;
                        }
                    }
                    if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                        crc2(&mut state.check, io.hold);
                    }
                    io.init_bits();
                } else if let Some(head) = state.head.as_mut() {
                    head.extra = None;
                    header_pub.extra_null = true;
                }
                state.mode = InflateMode::Extra;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::Extra => {
                if (state.flags & 0x0400) != 0 {
                    let mut copy = state.length as usize;
                    if copy > io.have() {
                        copy = io.have();
                    }
                    if copy > 0 {
                        // Store into the caller's `extra` buffer up to its cap.
                        // While fewer than `extra_max` bytes have been stored,
                        // the Vec's length equals the C `len = extra_len -
                        // length` write offset, so appending matches C exactly.
                        //
                        // The growth is fallible. C writes straight into the
                        // caller's fixed buffer and cannot fail here at all, but
                        // this port accumulates into an owned `Vec` first, and
                        // that `Vec` is bounded only by the caller's `extra_max`
                        // — a `c_uint`, so up to 4 GiB. An infallible
                        // `extend_from_slice` would turn an exhausted allocator
                        // into a process abort on a path C cannot fail on, so the
                        // request is reserved first and exhaustion is reported as
                        // `Z_MEM_ERROR` instead (AAP §0.6.5).
                        let mut capture_oom = false;
                        if state.head_foreign {
                            // C ABI path: the bytes go straight into the
                            // caller's buffer, re-reading its live pointer,
                            // `extra_len` and `extra_max` on every pass, and
                            // allocating nothing — so `capture_oom` can never be
                            // set and no `Z_MEM_ERROR` exists here, exactly as in
                            // C (`inflate.c` L610-L621).
                            //
                            // The write offset is C's `len = head->extra_len -
                            // state->length`, computed in the same wrapping
                            // `unsigned` arithmetic: a caller who shrinks
                            // `extra_len` below the remaining count makes C's
                            // subtraction wrap to a huge value that fails the
                            // `len < head->extra_max` guard, storing nothing.
                            // `store_extra` reaches the same conclusion by
                            // rejecting an out-of-range offset.
                            if state.head.is_some() {
                                if let Some(sk) = sink.as_deref_mut() {
                                    let offset = sk.extra_len.wrapping_sub(state.length) as usize;
                                    sk.store_extra(offset, &io.input[io.next..io.next + copy]);
                                }
                            }
                        } else if let Some(head) = state.head.as_mut() {
                            let extra_max = head.extra_max as usize;
                            if let Some(extra) = head.extra.as_mut() {
                                if extra.len() < extra_max {
                                    let room = extra_max - extra.len();
                                    let n = core::cmp::min(copy, room);
                                    if extra.try_reserve(n).is_err() {
                                        capture_oom = true;
                                    } else {
                                        extra.extend_from_slice(&io.input[io.next..io.next + n]);
                                        // C wrote those `n` bytes at `head->extra
                                        // + (extra_len - length)`, which is
                                        // exactly the Vec offset they landed at;
                                        // the boundary recovers it as
                                        // `extra.len() - stored`.
                                        header_pub.extra_stored += n;
                                    }
                                }
                            }
                        }
                        if capture_oom {
                            // Enter the permanent `MEM` state and let the
                            // `InflateMode::Mem` arm perform C's
                            // `case MEM: return Z_MEM_ERROR;`. The input cursor,
                            // `state.length`, and the header CRC are all still
                            // unadvanced, which is exactly what C's
                            // return-without-`RESTORE()` leaves behind.
                            state.mode = InflateMode::Mem;
                            continue 'inf_leave;
                        }
                        if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                            state.check = crc32(state.check, &io.input[io.next..io.next + copy]);
                        }
                        io.next += copy;
                        state.length -= copy as u32;
                    }
                    if state.length != 0 {
                        break 'inf_leave;
                    }
                }
                state.length = 0;
                state.mode = InflateMode::Name;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::Name => {
                if (state.flags & 0x0800) != 0 {
                    if io.have() == 0 {
                        break 'inf_leave;
                    }
                    let mut copy = 0usize;
                    let mut last_byte: u8;
                    // Set when the caller-bounded `name` buffer cannot grow; see
                    // the `Extra` arm for why this growth has to be fallible
                    // (`name_max` is a `c_uint`, and C allocates nothing here).
                    let mut capture_oom = false;
                    loop {
                        last_byte = io.input[io.next + copy];
                        copy += 1;
                        // C stores every byte it reads — the terminating NUL
                        // included — into `head->name[state->length++]` while
                        // `state->length < head->name_max` (`inflate.c`
                        // L632-L637). The owned `Vec` keeps only the content bytes
                        // (this type documents "no trailing NUL"), so `Vec::len()`
                        // *is* C's `state->length` and the terminator is recorded
                        // as a flag for the boundary to write. A name that exactly
                        // fills `name_max` therefore stays unterminated, as in C.
                        if state.head_foreign {
                            // C ABI path: `head->name[state->length++] = byte`
                            // straight into the caller's buffer, bounded by the
                            // live `name_max`, with the index advancing only on a
                            // store (`inflate.c` L632-L637). The terminating NUL
                            // is one of those bytes, so a name that exactly fills
                            // the buffer stays unterminated — no separate
                            // `name_terminated` publication is needed, and
                            // nothing can allocate.
                            if state.head.is_some() {
                                if let Some(sk) = sink.as_deref_mut() {
                                    if sk.store_name(state.length as usize, last_byte) {
                                        state.length += 1;
                                    }
                                }
                            }
                        } else if let Some(head) = state.head.as_mut() {
                            let name_max = head.name_max as usize;
                            if let Some(name) = head.name.as_mut() {
                                if name.len() < name_max {
                                    if last_byte == 0 {
                                        header_pub.name_terminated = true;
                                    } else if name.try_reserve(1).is_err() {
                                        capture_oom = true;
                                    } else {
                                        name.push(last_byte);
                                        header_pub.name_stored += 1;
                                    }
                                }
                            }
                        }
                        if capture_oom || last_byte == 0 || copy >= io.have() {
                            break;
                        }
                    }
                    if capture_oom {
                        // C's `case MEM: return Z_MEM_ERROR;` via the terminal
                        // arm, with the input cursor and header CRC unadvanced.
                        state.mode = InflateMode::Mem;
                        continue 'inf_leave;
                    }
                    if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                        state.check = crc32(state.check, &io.input[io.next..io.next + copy]);
                    }
                    io.next += copy;
                    if last_byte != 0 {
                        break 'inf_leave;
                    }
                } else if let Some(head) = state.head.as_mut() {
                    head.name = None;
                    header_pub.name_null = true;
                }
                state.length = 0;
                state.mode = InflateMode::Comment;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::Comment => {
                if (state.flags & 0x1000) != 0 {
                    if io.have() == 0 {
                        break 'inf_leave;
                    }
                    let mut copy = 0usize;
                    let mut last_byte: u8;
                    // Same fallible-growth reasoning as `Extra`/`Name` above.
                    let mut capture_oom = false;
                    loop {
                        last_byte = io.input[io.next + copy];
                        copy += 1;
                        // Same accounting as `NAME` above: the terminating NUL is
                        // one of C's counted bytes against `comm_max`
                        // (`inflate.c` L654-L659), so it is recorded as a flag
                        // rather than pushed into the content `Vec`.
                        if state.head_foreign {
                            // C ABI path: `head->comment[state->length++]`
                            // straight into the caller's buffer, bounded by the
                            // live `comm_max` (`inflate.c` L654-L659). Same
                            // accounting as `NAME` above; allocation-free.
                            if state.head.is_some() {
                                if let Some(sk) = sink.as_deref_mut() {
                                    if sk.store_comment(state.length as usize, last_byte) {
                                        state.length += 1;
                                    }
                                }
                            }
                        } else if let Some(head) = state.head.as_mut() {
                            let comm_max = head.comm_max as usize;
                            if let Some(comment) = head.comment.as_mut() {
                                if comment.len() < comm_max {
                                    if last_byte == 0 {
                                        header_pub.comment_terminated = true;
                                    } else if comment.try_reserve(1).is_err() {
                                        capture_oom = true;
                                    } else {
                                        comment.push(last_byte);
                                        header_pub.comment_stored += 1;
                                    }
                                }
                            }
                        }
                        if capture_oom || last_byte == 0 || copy >= io.have() {
                            break;
                        }
                    }
                    if capture_oom {
                        // C's `case MEM: return Z_MEM_ERROR;` via the terminal
                        // arm, with the input cursor and header CRC unadvanced.
                        state.mode = InflateMode::Mem;
                        continue 'inf_leave;
                    }
                    if (state.flags & 0x0200) != 0 && (state.wrap & 4) != 0 {
                        state.check = crc32(state.check, &io.input[io.next..io.next + copy]);
                    }
                    io.next += copy;
                    if last_byte != 0 {
                        break 'inf_leave;
                    }
                } else if let Some(head) = state.head.as_mut() {
                    head.comment = None;
                    header_pub.comment_null = true;
                }
                state.mode = InflateMode::Hcrc;
                continue 'inf_leave;
            }
            #[cfg(feature = "gzip")]
            InflateMode::Hcrc => {
                if (state.flags & 0x0200) != 0 {
                    if io.need_bits(16).is_none() {
                        break 'inf_leave;
                    }
                    if (state.wrap & 4) != 0 && io.hold != (state.check & 0xffff) {
                        strm.msg = Some("header crc mismatch");
                        state.mode = InflateMode::Bad;
                        continue 'inf_leave;
                    }
                    io.init_bits();
                }
                let flags = state.flags;
                if let Some(head) = state.head.as_mut() {
                    head.hcrc = ((flags >> 9) & 1) != 0;
                    head.done = true;
                    header_pub.hcrc = true;
                    header_pub.done = Some(HeaderDone::Complete);
                }
                // C `strm->adler = state->check = crc32(0L, Z_NULL, 0)`
                // (`inflate.c` L690): the gzip header is complete, so the CRC-32
                // over the *payload* starts fresh and is mirrored to the caller.
                state.check = CRC32_INIT;
                strm.adler = state.check;
                state.mode = InflateMode::Type;
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // When gzip support is compiled out, the header-field modes above
            // are never entered (HEAD cannot transition into them). This
            // combined arm preserves `match` exhaustiveness without a wildcard.
            // ---------------------------------------------------------------
            #[cfg(not(feature = "gzip"))]
            InflateMode::Flags
            | InflateMode::Time
            | InflateMode::Os
            | InflateMode::ExLen
            | InflateMode::Extra
            | InflateMode::Name
            | InflateMode::Comment
            | InflateMode::Hcrc => {
                strm.msg = Some("unknown compression method");
                state.mode = InflateMode::Bad;
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // Preset dictionary (zlib FDICT).
            // ---------------------------------------------------------------
            InflateMode::DictId => {
                if io.need_bits(32).is_none() {
                    break 'inf_leave;
                }
                // C `strm->adler = state->check = ZSWAP32(hold)` (`inflate.c`
                // L696): publish the requested dictionary's Adler-32 id so a
                // caller that receives `Z_NEED_DICT` can select the right
                // dictionary from `strm->adler`.
                state.check = zswap32(io.hold);
                strm.adler = state.check;
                io.init_bits();
                state.mode = InflateMode::Dict;
                continue 'inf_leave;
            }
            InflateMode::Dict => {
                if !state.havedict {
                    // C: `RESTORE(); return Z_NEED_DICT;` — no epilogue, and
                    // notably **no** `strm->adler` assignment in this arm
                    // (`inflate.c` L700-L704). The dictionary id the caller needs
                    // was already published by the `DictId` arm above (C
                    // L696), which falls straight through to here on the first
                    // pass. Re-entering this arm on a later call — a caller that
                    // got `Z_NEED_DICT` and called `inflate` again without
                    // supplying a dictionary — must therefore leave the field
                    // exactly as the caller left it, which is what C does.
                    state.hold = io.hold;
                    state.bits = io.bits;
                    let consumed = io.next;
                    let produced = io.put;
                    strm.set_inflate_state(placed);
                    return TrackedInflateOutcome {
                        outcome: InflateOutcome {
                            code: ReturnCode::NeedDict,
                            consumed,
                            produced,
                        },
                        header: header_pub,
                        // C runs `RESTORE()` and returns *directly* here
                        // (`inflate.c` L701-L703), jumping over the
                        // `strm->total_in += in; strm->total_out += out;`
                        // bookkeeping at L1141-L1142. The caller therefore sees
                        // the advanced cursors but unchanged totals.
                        commit_totals: false,
                    };
                }
                // C `strm->adler = state->check = adler32(0L, Z_NULL, 0)`
                // (`inflate.c` L705): the dictionary has been accepted, so the
                // running Adler-32 restarts over the decompressed data.
                state.check = ADLER32_INIT;
                strm.adler = state.check;
                state.mode = InflateMode::Type;
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // Block header dispatch.
            // ---------------------------------------------------------------
            InflateMode::Type => {
                if flush == Z_BLOCK || flush == Z_TREES {
                    break 'inf_leave;
                }
                state.mode = InflateMode::TypeDo;
                continue 'inf_leave;
            }
            InflateMode::TypeDo => {
                if state.last {
                    io.byte_align();
                    state.mode = InflateMode::Check;
                    continue 'inf_leave;
                }
                if io.need_bits(3).is_none() {
                    break 'inf_leave;
                }
                state.last = io.bits_val(1) != 0;
                io.drop_bits(1);
                match io.bits_val(2) {
                    0 => {
                        // stored (uncompressed) block
                        state.mode = InflateMode::Stored;
                    }
                    1 => {
                        // fixed Huffman block
                        fixedtables(state);
                        state.mode = InflateMode::LenUnderscore;
                        if flush == Z_TREES {
                            io.drop_bits(2);
                            break 'inf_leave;
                        }
                    }
                    2 => {
                        // dynamic Huffman block
                        state.mode = InflateMode::Table;
                    }
                    _ => {
                        strm.msg = Some("invalid block type");
                        state.mode = InflateMode::Bad;
                    }
                }
                io.drop_bits(2);
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // Stored (uncompressed) block.
            // ---------------------------------------------------------------
            InflateMode::Stored => {
                io.byte_align();
                if io.need_bits(32).is_none() {
                    break 'inf_leave;
                }
                if (io.hold & 0xffff) != ((io.hold >> 16) ^ 0xffff) {
                    strm.msg = Some("invalid stored block lengths");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.length = io.hold & 0xffff;
                io.init_bits();
                state.mode = InflateMode::CopyUnderscore;
                if flush == Z_TREES {
                    break 'inf_leave;
                }
                continue 'inf_leave;
            }
            InflateMode::CopyUnderscore => {
                state.mode = InflateMode::Copy;
                continue 'inf_leave;
            }
            InflateMode::Copy => {
                let mut copy = state.length as usize;
                if copy > 0 {
                    if copy > io.have() {
                        copy = io.have();
                    }
                    if copy > io.left() {
                        copy = io.left();
                    }
                    if copy == 0 {
                        break 'inf_leave;
                    }
                    let dst = io.put;
                    let src = io.next;
                    io.output[dst..dst + copy].copy_from_slice(&io.input[src..src + copy]);
                    io.next += copy;
                    io.put += copy;
                    state.length -= copy as u32;
                    continue 'inf_leave;
                }
                state.mode = InflateMode::Type;
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // Dynamic block: read the code-length code lengths, then the
            // literal/length and distance code lengths, then build the tables.
            // ---------------------------------------------------------------
            InflateMode::Table => {
                if io.need_bits(14).is_none() {
                    break 'inf_leave;
                }
                state.nlen = io.bits_val(5) + 257;
                io.drop_bits(5);
                state.ndist = io.bits_val(5) + 1;
                io.drop_bits(5);
                state.ncode = io.bits_val(4) + 4;
                io.drop_bits(4);
                if state.nlen > 286 || state.ndist > 30 {
                    strm.msg = Some("too many length or distance symbols");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.have = 0;
                state.mode = InflateMode::LenLens;
                continue 'inf_leave;
            }
            InflateMode::LenLens => {
                while state.have < state.ncode {
                    if io.need_bits(3).is_none() {
                        break 'inf_leave;
                    }
                    let idx = ORDER[state.have as usize];
                    state.lens[idx] = io.bits_val(3) as u16;
                    state.have += 1;
                    io.drop_bits(3);
                }
                while state.have < 19 {
                    let idx = ORDER[state.have as usize];
                    state.lens[idx] = 0;
                    state.have += 1;
                }
                state.next = 0;
                state.lencode = 0;
                state.distcode = 0;
                state.lentable = TableSource::Dynamic;
                state.disttable = TableSource::Dynamic;
                let mut lb: usize = 7;
                let res = inflate_table(
                    CodeType::Codes,
                    &state.lens,
                    19,
                    &mut state.codes,
                    &mut state.next,
                    &mut lb,
                    &mut state.work,
                );
                state.lenbits = lb as u32;
                if res.is_err() {
                    strm.msg = Some("invalid code lengths set");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.have = 0;
                state.mode = InflateMode::CodeLens;
                continue 'inf_leave;
            }
            InflateMode::CodeLens => {
                let total = (state.nlen + state.ndist) as usize;
                while (state.have as usize) < total {
                    // Decode one code-length code. The immutable `lencode`
                    // borrow lives only inside this block; `io` mutations are
                    // fine because `io` is disjoint from `state`.
                    let here: Code = {
                        let lc = state.lencode_slice(&LENFIX);
                        let mut h = lc[io.bits_val(state.lenbits) as usize];
                        while (h.bits as u32) > io.bits {
                            if io.pull_byte().is_none() {
                                break 'inf_leave;
                            }
                            h = lc[io.bits_val(state.lenbits) as usize];
                        }
                        h
                    };
                    if here.val < 16 {
                        io.drop_bits(here.bits as u32);
                        state.lens[state.have as usize] = here.val;
                        state.have += 1;
                    } else {
                        let len: u16;
                        let mut copy: u32;
                        if here.val == 16 {
                            if io.need_bits(here.bits as u32 + 2).is_none() {
                                break 'inf_leave;
                            }
                            io.drop_bits(here.bits as u32);
                            if state.have == 0 {
                                strm.msg = Some("invalid bit length repeat");
                                state.mode = InflateMode::Bad;
                                continue 'inf_leave;
                            }
                            len = state.lens[state.have as usize - 1];
                            copy = 3 + io.bits_val(2);
                            io.drop_bits(2);
                        } else if here.val == 17 {
                            if io.need_bits(here.bits as u32 + 3).is_none() {
                                break 'inf_leave;
                            }
                            io.drop_bits(here.bits as u32);
                            len = 0;
                            copy = 3 + io.bits_val(3);
                            io.drop_bits(3);
                        } else {
                            if io.need_bits(here.bits as u32 + 7).is_none() {
                                break 'inf_leave;
                            }
                            io.drop_bits(here.bits as u32);
                            len = 0;
                            copy = 11 + io.bits_val(7);
                            io.drop_bits(7);
                        }
                        if state.have as usize + copy as usize > total {
                            strm.msg = Some("invalid bit length repeat");
                            state.mode = InflateMode::Bad;
                            continue 'inf_leave;
                        }
                        while copy > 0 {
                            state.lens[state.have as usize] = len;
                            state.have += 1;
                            copy -= 1;
                        }
                    }
                }

                // Handle error breaks above (unreachable once BAD is set, since
                // those `continue 'inf_leave` re-dispatch to the BAD arm).
                if state.mode == InflateMode::Bad {
                    continue 'inf_leave;
                }

                // Check for an end-of-block code (length symbol 256).
                if state.lens[256] == 0 {
                    strm.msg = Some("invalid code -- missing end-of-block");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }

                // Build the literal/length decode table. `lenbits` is the
                // requested root size in/out; a temporary `usize` bridges the
                // `u32` state field and the `&mut usize` table API.
                state.next = 0;
                state.lencode = 0;
                state.lentable = TableSource::Dynamic;
                let nlen = state.nlen as usize;
                let mut lb: usize = 9;
                let res = inflate_table(
                    CodeType::Lens,
                    &state.lens,
                    nlen,
                    &mut state.codes,
                    &mut state.next,
                    &mut lb,
                    &mut state.work,
                );
                state.lenbits = lb as u32;
                if res.is_err() {
                    strm.msg = Some("invalid literal/lengths set");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }

                // Build the distance decode table, continuing from where the
                // literal/length table left off in the shared `codes` arena.
                state.distcode = state.next;
                state.disttable = TableSource::Dynamic;
                let ndist = state.ndist as usize;
                let mut db: usize = 6;
                let res = inflate_table(
                    CodeType::Dists,
                    &state.lens[nlen..],
                    ndist,
                    &mut state.codes,
                    &mut state.next,
                    &mut db,
                    &mut state.work,
                );
                state.distbits = db as u32;
                if res.is_err() {
                    strm.msg = Some("invalid distances set");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.mode = InflateMode::LenUnderscore;
                if flush == Z_TREES {
                    break 'inf_leave;
                }
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // Symbol decode + copy loop.
            // ---------------------------------------------------------------
            InflateMode::LenUnderscore => {
                state.mode = InflateMode::Len;
                continue 'inf_leave;
            }
            InflateMode::Len => {
                // Fast path: with at least 6 input bytes and 258 output bytes
                // available, decode in bulk in `fast::inflate_fast`, which — like
                // the rest of the inflate layer — contains no `unsafe`.
                if io.have() >= 6 && io.left() >= 258 {
                    // RESTORE the bit accumulator so the fast path can read it.
                    state.hold = io.hold;
                    state.bits = io.bits;
                    let msg = inflate_fast(
                        state,
                        io.input,
                        &mut io.next,
                        io.output,
                        &mut io.put,
                        outck,
                        &LENFIX,
                        &DISTFIX,
                    );
                    // LOAD the accumulator back out of the state.
                    io.hold = state.hold;
                    io.bits = state.bits;
                    if let Some(m) = msg {
                        strm.msg = Some(m);
                    }
                    if state.mode == InflateMode::Type {
                        state.back = -1;
                    }
                    continue 'inf_leave;
                }

                // Slow path: decode one length code, following one level of
                // sub-table indirection if the root entry is a link. The
                // immutable `lencode` borrow is confined to the block; `state`
                // mutations (e.g. `state.back`) happen only after it ends.
                state.back = 0;
                let (here, back_add): (Code, i32) = {
                    let lc = state.lencode_slice(&LENFIX);
                    let mut h = lc[io.bits_val(state.lenbits) as usize];
                    while (h.bits as u32) > io.bits {
                        if io.pull_byte().is_none() {
                            break 'inf_leave;
                        }
                        h = lc[io.bits_val(state.lenbits) as usize];
                    }
                    let mut badd = 0i32;
                    if h.op != 0 && (h.op & 0xf0) == 0 {
                        let last = h;
                        let mut idx = last.val as usize
                            + ((io.bits_val(last.bits as u32 + last.op as u32) >> last.bits)
                                as usize);
                        h = lc[idx];
                        while (last.bits as u32 + h.bits as u32) > io.bits {
                            if io.pull_byte().is_none() {
                                break 'inf_leave;
                            }
                            idx = last.val as usize
                                + ((io.bits_val(last.bits as u32 + last.op as u32) >> last.bits)
                                    as usize);
                            h = lc[idx];
                        }
                        io.drop_bits(last.bits as u32);
                        badd = last.bits as i32;
                    }
                    (h, badd)
                };
                state.back += back_add;
                io.drop_bits(here.bits as u32);
                state.back += here.bits as i32;
                state.length = here.val as u32;
                if here.op == 0 {
                    // Literal byte.
                    state.mode = InflateMode::Lit;
                    continue 'inf_leave;
                }
                if (here.op & 32) != 0 {
                    // End of block.
                    state.back = -1;
                    state.mode = InflateMode::Type;
                    continue 'inf_leave;
                }
                if (here.op & 64) != 0 {
                    strm.msg = Some("invalid literal/length code");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.extra = (here.op & 15) as u32;
                state.mode = InflateMode::LenExt;
                continue 'inf_leave;
            }
            InflateMode::LenExt => {
                if state.extra != 0 {
                    let extra = state.extra;
                    if io.need_bits(extra).is_none() {
                        break 'inf_leave;
                    }
                    state.length += io.bits_val(extra);
                    io.drop_bits(extra);
                    state.back += extra as i32;
                }
                state.was = state.length;
                state.mode = InflateMode::Dist;
                continue 'inf_leave;
            }
            InflateMode::Dist => {
                let (here, back_add): (Code, i32) = {
                    let dc = state.distcode_slice(&DISTFIX);
                    let mut h = dc[io.bits_val(state.distbits) as usize];
                    while (h.bits as u32) > io.bits {
                        if io.pull_byte().is_none() {
                            break 'inf_leave;
                        }
                        h = dc[io.bits_val(state.distbits) as usize];
                    }
                    let mut badd = 0i32;
                    if (h.op & 0xf0) == 0 {
                        let last = h;
                        let mut idx = last.val as usize
                            + ((io.bits_val(last.bits as u32 + last.op as u32) >> last.bits)
                                as usize);
                        h = dc[idx];
                        while (last.bits as u32 + h.bits as u32) > io.bits {
                            if io.pull_byte().is_none() {
                                break 'inf_leave;
                            }
                            idx = last.val as usize
                                + ((io.bits_val(last.bits as u32 + last.op as u32) >> last.bits)
                                    as usize);
                            h = dc[idx];
                        }
                        io.drop_bits(last.bits as u32);
                        badd = last.bits as i32;
                    }
                    (h, badd)
                };
                state.back += back_add;
                io.drop_bits(here.bits as u32);
                state.back += here.bits as i32;
                if (here.op & 64) != 0 {
                    strm.msg = Some("invalid distance code");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.offset = here.val as u32;
                state.extra = (here.op & 15) as u32;
                state.mode = InflateMode::DistExt;
                continue 'inf_leave;
            }
            InflateMode::DistExt => {
                if state.extra != 0 {
                    let extra = state.extra;
                    if io.need_bits(extra).is_none() {
                        break 'inf_leave;
                    }
                    state.offset += io.bits_val(extra);
                    io.drop_bits(extra);
                    state.back += extra as i32;
                }
                // C `#ifdef INFLATE_STRICT` (`inflate.c` L1010-L1015): reject a
                // distance that exceeds the maximum the zlib header's window size
                // permits. This is the *slow path's* half of the check; the fast
                // loop carries the other half (`inffast.c` L156-L162, ported at
                // `crate::inflate::fast`). C compiles both from the same macro, so
                // they must be gated together — enforcing it on only one path
                // would make acceptance depend on how much output buffer the
                // caller happened to supply, which is precisely the kind of
                // configuration-dependent divergence the strict feature exists to
                // rule out. Off by default, so a default build stays byte-exact
                // and accepts exactly what reference zlib accepts
                // (AAP §0.8.2 Divergence 2).
                #[cfg(feature = "inflate_strict")]
                if state.offset > state.dmax {
                    strm.msg = Some("invalid distance too far back");
                    state.mode = InflateMode::Bad;
                    continue 'inf_leave;
                }
                state.mode = InflateMode::Match;
                continue 'inf_leave;
            }
            InflateMode::Match => {
                if io.left() == 0 {
                    break 'inf_leave;
                }
                // Bytes already produced this call (C `copy = out - left`).
                let mut copy = outck - io.left();
                let from_output: bool;
                let from_base: usize;
                if state.offset as usize > copy {
                    // Distance reaches into the sliding window (earlier calls).
                    copy = state.offset as usize - copy;
                    if copy > state.whave as usize && state.sane {
                        strm.msg = Some("invalid distance too far back");
                        state.mode = InflateMode::Bad;
                        continue 'inf_leave;
                    }
                    if copy > state.wnext as usize {
                        copy -= state.wnext as usize;
                        from_base = state.wsize as usize - copy;
                    } else {
                        from_base = state.wnext as usize - copy;
                    }
                    if copy > state.length as usize {
                        copy = state.length as usize;
                    }
                    from_output = false;
                } else {
                    // Distance reaches into output already produced this call.
                    from_base = io.put - state.offset as usize;
                    copy = state.length as usize;
                    from_output = true;
                }
                if copy > io.left() {
                    copy = io.left();
                }
                state.length -= copy as u32;
                // Byte-by-byte copy: the read cursor is always strictly behind
                // the write cursor, so overlapping LZ77 copies work correctly.
                for k in 0..copy {
                    let b = if from_output {
                        io.output[from_base + k]
                    } else {
                        state.window[from_base + k]
                    };
                    let p = io.put;
                    io.output[p] = b;
                    io.put += 1;
                }
                if state.length == 0 {
                    state.mode = InflateMode::Len;
                }
                continue 'inf_leave;
            }
            InflateMode::Lit => {
                if io.left() == 0 {
                    break 'inf_leave;
                }
                let p = io.put;
                io.output[p] = state.length as u8;
                io.put += 1;
                state.mode = InflateMode::Len;
                continue 'inf_leave;
            }

            // ---------------------------------------------------------------
            // Trailer verification.
            // ---------------------------------------------------------------
            InflateMode::Check => {
                if state.wrap != 0 {
                    if io.need_bits(32).is_none() {
                        break 'inf_leave;
                    }
                    let produced = outck - io.left();
                    state.total += produced as u64;
                    if (state.wrap & 4) != 0 && produced != 0 {
                        let start = io.put - produced;
                        state.check =
                            update_check(state.flags, state.check, &io.output[start..io.put]);
                        // C mirrors the freshly-folded check into `strm->adler`
                        // inside this very guard (`inflate.c` L1078-L1080) —
                        // `if ((state->wrap & 4) && out) strm->adler =
                        // state->check = UPDATE_CHECK(...)`.
                        strm.adler = state.check;
                    }
                    outck = io.left();
                    // Compare the stored trailer with the computed check. For a
                    // gzip stream the CRC-32 is little-endian; for zlib the
                    // Adler-32 is big-endian, hence the `zswap32`.
                    let mismatch = {
                        #[cfg(feature = "gzip")]
                        {
                            (state.wrap & 4) != 0
                                && (if state.flags != 0 {
                                    io.hold
                                } else {
                                    zswap32(io.hold)
                                }) != state.check
                        }
                        #[cfg(not(feature = "gzip"))]
                        {
                            (state.wrap & 4) != 0 && zswap32(io.hold) != state.check
                        }
                    };
                    if mismatch {
                        strm.msg = Some("incorrect data check");
                        state.mode = InflateMode::Bad;
                        continue 'inf_leave;
                    }
                    io.init_bits();
                }
                state.mode = InflateMode::Length;
                continue 'inf_leave;
            }
            InflateMode::Length => {
                // The gzip ISIZE (uncompressed length mod 2^32) check. For zlib
                // and raw streams `state.flags` is `0`, so this is a no-op that
                // simply advances to `Done` — hence the arm is not gzip-gated.
                if state.wrap != 0 && state.flags != 0 {
                    if io.need_bits(32).is_none() {
                        break 'inf_leave;
                    }
                    if (state.wrap & 4) != 0 && io.hold as u64 != (state.total & 0xffff_ffff) {
                        strm.msg = Some("incorrect length check");
                        state.mode = InflateMode::Bad;
                        continue 'inf_leave;
                    }
                    io.init_bits();
                }
                state.mode = InflateMode::Done;
                continue 'inf_leave;
            }
            InflateMode::Done => {
                ret = ReturnCode::StreamEnd;
                break 'inf_leave;
            }
            InflateMode::Bad => {
                ret = ReturnCode::DataError;
                break 'inf_leave;
            }
            InflateMode::Mem => {
                // C `case MEM: return Z_MEM_ERROR;` — an immediate return with
                // no epilogue. The stream cursors are left unadvanced, matching
                // C skipping `RESTORE()`.
                //
                // Reached two ways, both of them live. Within a single call, the
                // gzip header-capture arms (`Extra`/`Name`/`Comment`) jump here
                // when a caller-bounded `extra`/`name`/`comment` buffer cannot
                // grow. Across calls, `MEM` is permanent: a window allocation
                // that fails records it on the way out, so every later
                // `inflate` on that stream re-enters here and keeps returning
                // `Z_MEM_ERROR` - exactly the C behavior, since C's
                // `state->mode` is equally sticky. That window allocation is
                // fallible whenever it is served by a caller-supplied hook or a
                // custom `Allocator` that refuses it, both at `inf_leave` below
                // and inside [`inflate_set_dictionary`].
                strm.set_inflate_state(placed);
                return TrackedInflateOutcome::committed(ReturnCode::MemError, 0, 0, header_pub);
            }
            InflateMode::Sync => {
                // C `case SYNC: default: return Z_STREAM_ERROR;`.
                strm.set_inflate_state(placed);
                return TrackedInflateOutcome::committed(ReturnCode::StreamError, 0, 0, header_pub);
            }
        }
    }

    // =======================================================================
    // inf_leave: restore the accumulator, update the window, and account for
    // progress before returning (C `inflate.c` L1131-L1152).
    // =======================================================================

    // RESTORE(): write the bit accumulator back to the state.
    state.hold = io.hold;
    state.bits = io.bits;

    let produced_since_ck = outck - io.left();

    let mode_u = state.mode as u16;
    // Update the sliding window with freshly produced output when the window is
    // already in use, or while more output is still expected (and this is not a
    // short read at the very end of a `Z_FINISH` request). Mirrors the C
    // condition at L1133-L1136 (using the enum discriminants for `< BAD` /
    // `< CHECK`, since `InflateMode` is ordered like the C `mode` values).
    let needs_window_update = state.wsize != 0
        || (produced_since_ck != 0
            && mode_u < InflateMode::Bad as u16
            && (mode_u < InflateMode::Check as u16 || flush != Z_FINISH));
    // `&&` short-circuits, so `updatewindow` runs (with its window side effect)
    // exactly when the condition holds; the body runs only on an OOM failure.
    if needs_window_update
        && updatewindow(
            state,
            strm.allocator(),
            &io.output[..],
            io.put,
            produced_since_ck,
        )
        .is_err()
    {
        // C `inf_leave`: `state->mode = MEM; return Z_MEM_ERROR;` — the lazy
        // window allocation (routed through the caller's `zalloc`) reported OOM.
        // Enter the permanent `MEM` error state and return `Z_MEM_ERROR` with no
        // committed progress, matching both the C control flow and the
        // `InflateMode::Mem` arm above.
        state.mode = InflateMode::Mem;
        strm.set_inflate_state(placed);
        return TrackedInflateOutcome {
            header: header_pub,
            outcome: InflateOutcome {
                code: ReturnCode::MemError,
                // C reaches this return *after* `RESTORE()` (`inflate.c` L1132),
                // so `next_in`/`avail_in`/`next_out`/`avail_out` are already
                // committed and the caller keeps every byte decoded during this
                // call — the window allocation failed, not the decode. Reporting
                // `0`/`0` here used to silently discard that output.
                consumed: io.next,
                produced: io.put,
            },
            // ...but C's `state->mode = MEM; return Z_MEM_ERROR;` at L1136-L1137
            // jumps over the total bookkeeping at L1141-L1142, so the totals must
            // NOT advance. Measured against reference C: a failing window
            // allocation mid-stream yields `total_out == 0` with 600 bytes
            // already delivered through `next_out`/`avail_out`.
            commit_totals: false,
        };
    }

    let consumed = io.next;
    let produced = io.put;

    state.total += produced_since_ck as u64;

    // Fold the output produced since the last checkpoint into the running
    // check (the pre-checkpoint bytes were folded in the CHECK arm).
    if (state.wrap & 4) != 0 && produced_since_ck != 0 {
        let start = io.put - produced_since_ck;
        state.check = update_check(state.flags, state.check, &io.output[start..io.put]);
        // C publishes the check mirror *inside* this guard (`inflate.c`
        // L1144-L1146: `if ((state->wrap & 4) && out) strm->adler = state->check
        // = UPDATE_CHECK(...)`). The write used to sit below, unguarded, which
        // clobbered `strm->adler` on exactly the paths where C leaves it alone —
        // raw framing (`wrap == 0`, where C's `inflateResetKeep` deliberately
        // skips the field per its "ill-conceived Java test suite" comment at
        // `inflate.c` L108-L109) and early-error paths that produce no output.
        strm.adler = state.check;
    }

    // data_type: the exact C formula (inflate.c L1147-L1149) — low 7 bits are
    // the bit-buffer occupancy, plus flags for last-block / at-block-boundary /
    // at-start-of-block.
    strm.data_type = state.bits as i32
        + if state.last { 64 } else { 0 }
        + if state.mode == InflateMode::Type {
            128
        } else {
            0
        }
        + if state.mode == InflateMode::LenUnderscore || state.mode == InflateMode::CopyUnderscore {
            256
        } else {
            0
        };

    // Downgrade Z_OK to Z_BUF_ERROR when the call was unable to make progress
    // (no bytes in or out) or a Z_FINISH could not complete.
    if ((consumed == 0 && produced == 0) || flush == Z_FINISH) && ret == ReturnCode::Ok {
        ret = ReturnCode::BufError;
    }

    // Commit progress counters and the running check mirror, then reinstall the
    // state so subsequent calls resume where this one left off.
    strm.total_in += consumed as u64;
    strm.total_out += produced as u64;
    strm.set_inflate_state(placed);

    // The normal epilogue path: C advances the cursors and the totals together
    // (`inflate.c` L1139-L1142).
    TrackedInflateOutcome::committed(ret, consumed, produced, header_pub)
}

// ===========================================================================
// Dictionary / header / sync / copy / diagnostics API.
// ===========================================================================

/// Returns the sliding-window history as a preset dictionary — the Rust port of
/// C `inflateGetDictionary` (`inflate.c` L1167-L1185).
///
/// Up to [`InflateState::whave`] bytes of decoded history are copied into
/// `dictionary` in chronological order (oldest first), and the true length is
/// written to `dict_length`. Pass an empty (or too-small) `dictionary` slice to
/// query only the length — the copy is skipped when `dictionary` cannot hold the
/// full history, mirroring how C callers pass `Z_NULL` to size their buffer.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state.
pub fn inflate_get_dictionary<A: Allocator>(
    strm: &ZStream<A>,
    dictionary: &mut [u8],
    dict_length: &mut usize,
) -> InflateResult {
    let state = strm.inflate_state().ok_or(ZlibError::StreamError)?;
    let whave = state.whave as usize;
    let wnext = state.wnext as usize;
    if whave != 0 && dictionary.len() >= whave {
        // Older bytes first (`window[wnext..whave]`), then the wrapped-around
        // newer bytes (`window[..wnext]`).
        let head_len = whave - wnext;
        dictionary[..head_len].copy_from_slice(&state.window[wnext..whave]);
        dictionary[head_len..whave].copy_from_slice(&state.window[..wnext]);
    }
    *dict_length = whave;
    Ok(ReturnCode::Ok)
}

/// Installs a preset dictionary into the sliding window — the Rust port of C
/// `inflateSetDictionary` (`inflate.c` L1187-L1217).
///
/// Valid only for a raw stream (before any data) or immediately after
/// [`inflate`] returned [`ReturnCode::NeedDict`] (state [`InflateMode::Dict`]).
/// In the latter case the dictionary's Adler-32 must match the id the stream
/// requested, or [`ZlibError::DataError`] is returned.
///
/// # Errors
/// * [`ZlibError::StreamError`] — no inflate state, or a wrapped stream not
///   awaiting a dictionary. A stream whose mode was already latched to
///   [`InflateMode::Mem`] by a failed load reports this on a retry, which is why
///   C's `infcover.c` has to restore `mode = DICT` before trying again.
/// * [`ZlibError::DataError`] — the dictionary's Adler-32 id does not match.
/// * [`ZlibError::MemError`] — the sliding window is not allocated yet and the
///   stream's allocator refused it, matching C's `updatewindow` failure clause
///   (`inflate.c` L1211-L1214). The mode is latched to [`InflateMode::Mem`].
pub fn inflate_set_dictionary<A: Allocator>(
    strm: &mut ZStream<A>,
    dictionary: &[u8],
) -> InflateResult {
    // Borrow the decoder and the allocator together: `updatewindow` mutates the
    // state *and* may allocate the window, and both live in distinct `ZStream`
    // fields (AAP §0.6.3).
    let (state, alloc) = strm
        .inflate_state_and_allocator()
        .ok_or(ZlibError::StreamError)?;
    // A dictionary is only accepted for a raw stream, or when explicitly needed.
    if state.wrap != 0 && state.mode != InflateMode::Dict {
        return Err(ZlibError::StreamError);
    }
    // Verify the dictionary identifier when one was requested.
    if state.mode == InflateMode::Dict {
        let dictid = adler32(ADLER32_INIT, dictionary);
        if dictid != state.check {
            return Err(ZlibError::DataError);
        }
    }
    // Load the dictionary into the window (amending existing history). C treats
    // `updatewindow` failure as `Z_MEM_ERROR` (setting `mode = MEM`); reproduce
    // that when the window allocation is routed through a caller hook that
    // reports OOM.
    let dict_len = dictionary.len();
    if updatewindow(state, alloc, dictionary, dict_len, dict_len).is_err() {
        state.mode = InflateMode::Mem;
        return Err(ZlibError::MemError);
    }
    state.havedict = true;
    Ok(ReturnCode::Ok)
}

/// Registers a [`GzHeader`] to receive the gzip header fields parsed by
/// [`inflate`] — the Rust port of C `inflateGetHeader` (`inflate.c`
/// L1219-L1230). Only meaningful for a gzip-capable stream (`wrap & 2`).
///
/// Ownership of `head` is transferred to the decoder: its `extra`/`name`/
/// `comment` capacities and buffers (if `Some`) bound what is captured while the
/// gzip header modes run, and `done` is cleared.
///
/// This is the *idiomatic* registration. The C ABI does not use it: a
/// `gz_headerp` names buffers in the caller's own memory that C writes through
/// live, so `src/ffi/inflate.rs` registers the borrowed form instead and lends the
/// decoder a per-call view of those buffers.
///
/// # Retrieving the parsed fields
///
/// Because ownership moves in, the caller does **not** retain a handle the way a
/// C caller does. Read the parsed fields back with [`inflate_header`] (borrow) or
/// [`inflate_take_header`] (transfer ownership back out). Do so before an
/// `inflate_reset*`, which clears the registration exactly as C's
/// `state->head = Z_NULL` does (`inflate.c` L115).
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state or the
/// stream is not gzip-capable.
#[cfg(feature = "gzip")]
pub fn inflate_get_header<A: Allocator>(strm: &mut ZStream<A>, head: GzHeader) -> InflateResult {
    let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
    if (state.wrap & 2) == 0 {
        return Err(ZlibError::StreamError);
    }
    let mut head = head;
    head.done = false;
    state.head = Some(head);
    // The idiomatic API lends the decoder an *owned* header, so payload bytes are
    // accumulated in its vectors rather than written through to caller memory.
    state.head_foreign = false;
    Ok(ReturnCode::Ok)
}

/// Registers a **caller-owned** gzip header for [`inflate`] to fill, the C ABI's
/// counterpart of [`inflate_get_header`].
///
/// Port of C `inflateGetHeader` (`inflate.c` L1219-L1230) on the path where the
/// header — and every one of its output buffers — lives in the caller's memory.
/// `present` mirrors `head != Z_NULL`.
///
/// Only the *registration* is recorded here: no pointer, no capacity, and no
/// content. The decoder writes each decoded byte through the
/// [`ForeignGzHeaderSink`] the boundary lends it on every [`inflate`] call, which
/// is re-materialized from the caller's live struct each time. That is what makes
/// a sink installed (or resized, or withdrawn) *after* registration take effect,
/// as it does in C, and what keeps the header path allocation-free — C allocates
/// nothing here, so no `Z_MEM_ERROR` may arise from it.
///
/// The header's decoded *scalars* still land in an engine-side [`GzHeader`], from
/// which the boundary replays C's incremental publication schedule; only the
/// variable-length payloads bypass it.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state or the
/// stream is not gzip-capable — both exactly as C.
#[cfg(feature = "gzip")]
pub(crate) fn inflate_get_header_foreign<A: Allocator>(
    strm: &mut ZStream<A>,
    present: bool,
) -> InflateResult {
    let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
    if (state.wrap & 2) == 0 {
        return Err(ZlibError::StreamError);
    }
    if present {
        // Scalars only: every payload slot stays `None`, so nothing is ever
        // appended and nothing is ever allocated.
        state.head = Some(GzHeader::default());
        state.head_foreign = true;
    } else {
        // C's assignment of a null `head` simply clears the registration.
        state.head = None;
        state.head_foreign = false;
    }
    Ok(ReturnCode::Ok)
}

/// Settles the input debt the fast path could not pay, and reports how many
/// whole bytes the caller must rewind its own input cursor by.
///
/// # Why this exists
///
/// C's `inflate_fast` ends with `len = bits >> 3; in -= len; bits -= len << 3;`
/// (`inffast.c` L290-L294): a single unconditional operation on a raw pointer.
/// When it enters holding whole bytes — which `inflatePrime` alone guarantees —
/// `in` legitimately moves to before `strm->next_in`, un-consuming bytes an
/// earlier call took. C never dereferences them, so the only trace is in
/// `next_in`, `avail_in`, `total_in` and `data_type`.
///
/// An index into a `&[u8]` cannot go before that slice, so
/// [`crate::inflate::fast::inflate_fast`] pays the part that lands inside the
/// slice and records the rest in [`InflateState::rewound`], leaving those bits in
/// `hold`. Deferring rather than discarding them is deliberate: a caller that
/// hands over an independent slice per call could never re-feed the bytes the
/// bits came from, and dropping them would desynchronise the bitstream.
///
/// This function is the settlement, for callers whose input really is one
/// contiguous buffer — that is, the C ABI. It drops the deferred bits and returns
/// the byte count, so that the two halves of C's `in -= len; bits -= len << 3;`
/// pair still commit together. [`ZStream::data_type`] is re-derived from the
/// reduced `bits` with C's exact formula (`inflate.c` L1147-L1149), because the
/// decode epilogue computed it before this settlement.
///
/// Returns `0` — leaving the stream untouched — when there is no debt, which is
/// every ordinary call.
pub(crate) fn inflate_take_input_history_rewind<A: Allocator>(strm: &mut ZStream<A>) -> usize {
    let Some(state) = strm.inflate_state_mut() else {
        return 0;
    };
    let rewound = core::mem::take(&mut state.rewound);
    if rewound == 0 {
        return 0;
    }
    // C's `bits -= len << 3` for the deferred half. The fast path only ever defers
    // whole bytes it actually had, so this cannot underflow; `saturating_sub` keeps
    // that guarantee local rather than trusting it from a distance.
    state.bits = state.bits.saturating_sub(rewound << 3);
    // C's `hold &= (1U << bits) - 1`. `bits == 32` would overflow the shift, so the
    // all-ones mask is produced without shifting (as in the fast path itself).
    state.hold &= 1u32.checked_shl(state.bits).unwrap_or(0).wrapping_sub(1);
    let (bits, last, mode) = (state.bits, state.last, state.mode);
    // Re-derive C's `strm->data_type` (`inflate.c` L1147-L1149) from the reduced
    // accumulator: the decode epilogue already published a value computed from the
    // pre-settlement `bits`, and the low seven bits of `data_type` *are* `bits`.
    strm.data_type = bits as i32
        + if last { 64 } else { 0 }
        + if mode == InflateMode::Type { 128 } else { 0 }
        + if mode == InflateMode::LenUnderscore || mode == InflateMode::CopyUnderscore {
            256
        } else {
            0
        };
    rewound as usize
}

/// Borrows the [`GzHeader`] previously registered with [`inflate_get_header`],
/// or [`None`] if `strm` holds no inflate state or no header was registered.
///
/// # Why this exists
///
/// C's `inflateGetHeader` (`inflate.c` L1219-L1230) stores a *borrowed*
/// `gz_headerp`, so a C caller keeps its own handle and simply reads its own
/// struct once [`inflate`] has run. This port deliberately cannot do that:
/// ownership of the [`GzHeader`] moves into the decoder so that the parsed
/// `extra` / `name` / `comment` byte buffers are owned by the same value that
/// bounds them, which is precisely what removes the dangling-pointer hazard C
/// carries here (AAP §0.6.3). This accessor is the safe-Rust counterpart of
/// C's "read the struct you handed in": it hands the borrow back once the
/// header modes have run, so the parsed metadata is reachable from safe Rust
/// without exposing the decoder's internals. The C ABI is unaffected — the FFI
/// shim in `src/ffi/inflate.rs` keeps writing the fields back through the
/// caller's `gz_headerp` exactly as before.
///
/// # Timing
///
/// Fields are populated progressively as [`inflate`] walks the gzip header
/// modes, in the same order and from the same bytes C uses: `text`, `time`,
/// `xflags` / `os`, `extra_len` and `extra`, `name`, `comment`, then `hcrc`.
/// [`GzHeader::done`] is set to `true` only after the whole header (including
/// the optional CRC-16) has been consumed, so `done` is the signal that every
/// field is final — exactly C's contract. A truncated or rejected header leaves
/// `done` clear, and a non-gzip stream leaves the registration untouched.
///
/// # Lifetime of the registration
///
/// The registration does **not** survive a reset: `inflate_reset*` clears it,
/// mirroring C's `state->head = Z_NULL` (`inflate.c` L115). Because this port
/// owns the header rather than borrowing it, a reset *drops* the parsed
/// metadata instead of leaving it in caller-owned storage — so retrieve it (via
/// this function or [`inflate_take_header`]) before resetting the stream.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "gzip")] {
/// use zlib_rs::constants::{Strategy, Z_DEFLATED, Z_FINISH};
/// use zlib_rs::deflate::{deflate, deflate_init2, deflate_set_header};
/// use zlib_rs::inflate::{inflate, inflate_get_header, inflate_header, inflate_init2};
/// use zlib_rs::stream::ZStream;
/// use zlib_rs::{GzHeader, ReturnCode};
///
/// // Produce a gzip member carrying a file name (windowBits 31 = gzip wrapper).
/// let mut enc = ZStream::new();
/// deflate_init2(&mut enc, 6, Z_DEFLATED, 31, 8, Strategy::Default).unwrap();
/// deflate_set_header(&mut enc, Some(GzHeader::new().with_name(b"a.bin"))).unwrap();
/// let mut compressed = [0u8; 128];
/// let out = deflate(&mut enc, b"abc", &mut compressed, Z_FINISH);
/// assert_eq!(out.code, ReturnCode::StreamEnd);
/// let compressed = &compressed[..out.produced];
///
/// // Register a header to receive the parsed fields: an empty buffer plus the
/// // capacity that bounds it, exactly as a C caller supplies `name`/`name_max`.
/// let mut want = GzHeader::new();
/// want.name = Some(Vec::new());
/// want.name_max = 64;
///
/// let mut dec = ZStream::new();
/// inflate_init2(&mut dec, 31).unwrap();
/// inflate_get_header(&mut dec, want).unwrap();
/// // The registration is reachable even before `inflate` runs.
/// assert!(inflate_header(&dec).is_some());
///
/// let mut plain = [0u8; 16];
/// let got = inflate(&mut dec, compressed, &mut plain, Z_FINISH);
/// assert_eq!(got.code, ReturnCode::StreamEnd);
/// assert_eq!(&plain[..got.produced], b"abc");
///
/// // The gzip metadata is now readable from safe Rust.
/// let head = inflate_header(&dec).expect("the registered header");
/// assert!(head.done, "the whole gzip header was consumed");
/// assert_eq!(head.name.as_deref(), Some(&b"a.bin"[..]));
/// # }
/// ```
#[cfg(feature = "gzip")]
#[must_use]
pub fn inflate_header<A: Allocator>(strm: &ZStream<A>) -> Option<&GzHeader> {
    strm.inflate_state()?.head.as_ref()
}

/// Takes back ownership of the [`GzHeader`] previously registered with
/// [`inflate_get_header`], leaving the decoder with no header registered — or
/// [`None`] if `strm` holds no inflate state or no header was registered.
///
/// Use this when the parsed metadata must outlive the stream, or must survive an
/// `inflate_reset*` (which otherwise drops the registration, mirroring C's
/// `state->head = Z_NULL` at `inflate.c` L115). Use [`inflate_header`] instead
/// to inspect the fields while leaving the registration in place.
///
/// # Effect on subsequent decoding
///
/// Afterwards the decoder behaves exactly as if [`inflate_get_header`] had never
/// been called: the remaining gzip header bytes are still parsed and validated,
/// and the header CRC-16 is still verified when `FHCRC` is set, but nothing is
/// recorded — which is precisely what C does for a `Z_NULL` `state->head`.
/// Decoded output is therefore unchanged, preserving byte-exact fidelity
/// (AAP §0.8.1 directive D-1).
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "gzip")] {
/// use zlib_rs::GzHeader;
/// use zlib_rs::inflate::{inflate_get_header, inflate_header, inflate_init2, inflate_take_header};
/// use zlib_rs::stream::ZStream;
///
/// let mut strm = ZStream::new();
/// inflate_init2(&mut strm, 31).unwrap();
/// assert!(inflate_take_header(&mut strm).is_none(), "nothing registered yet");
///
/// inflate_get_header(&mut strm, GzHeader::new().with_name(b"data.bin")).unwrap();
/// let owned = inflate_take_header(&mut strm).expect("ownership returned");
/// assert!(!owned.done, "`inflate_get_header` clears `done`");
/// assert_eq!(owned.name.as_deref(), Some(&b"data.bin"[..]));
///
/// // The registration is gone, so the decoder records nothing further.
/// assert!(inflate_header(&strm).is_none());
/// assert!(inflate_take_header(&mut strm).is_none());
/// # }
/// ```
#[cfg(feature = "gzip")]
#[must_use]
pub fn inflate_take_header<A: Allocator>(strm: &mut ZStream<A>) -> Option<GzHeader> {
    strm.inflate_state_mut()?.head.take()
}

/// Scans `input` for the DEFLATE flush marker (`00 00 FF FF`) and, when found,
/// restarts decoding at the following block — the Rust port of C `inflateSync`
/// (`inflate.c` L1264-L1310), used for error recovery.
///
/// Returns the [`ReturnCode`] together with the number of `input` bytes
/// consumed (the FFI shim advances `next_in`/`avail_in` by that amount). The
/// error-recovery behaviour is byte-for-byte identical to reference zlib: it
/// first drains any bits still held in the accumulator, then searches the
/// supplied input, and on a full match resets to [`InflateMode::Type`] while
/// preserving the running byte totals.
///
/// Possible codes: [`ReturnCode::Ok`] (marker found, decoding can resume),
/// [`ReturnCode::BufError`] (no input and fewer than 8 held bits — call again
/// with more input), [`ReturnCode::DataError`] (input exhausted without a full
/// marker), or [`ReturnCode::StreamError`] (no inflate state).
pub fn inflate_sync<A: Allocator>(strm: &mut ZStream<A>, input: &[u8]) -> (ReturnCode, usize) {
    // check parameters
    let state_bits = match strm.inflate_state() {
        Some(s) => s.bits,
        None => return (ReturnCode::StreamError, 0),
    };
    if input.is_empty() && state_bits < 8 {
        return (ReturnCode::BufError, 0);
    }

    // If this is the first sync call, prime the search from the bit buffer.
    {
        let state = strm
            .inflate_state_mut()
            .expect("inflate state present (checked above)");
        if state.mode != InflateMode::Sync {
            state.mode = InflateMode::Sync;
            let rem = state.bits & 7;
            state.hold >>= rem;
            state.bits -= rem;
            let mut buf = [0u8; 4];
            let mut len = 0usize;
            while state.bits >= 8 {
                buf[len] = state.hold as u8;
                len += 1;
                state.hold >>= 8;
                state.bits -= 8;
            }
            state.have = 0;
            let mut have = state.have;
            syncsearch(&mut have, &buf[..len]);
            state.have = have;
        }
    }

    // Search the supplied input.
    let consumed = {
        let state = strm
            .inflate_state_mut()
            .expect("inflate state present (checked above)");
        let mut have = state.have;
        let n = syncsearch(&mut have, input);
        state.have = have;
        n
    };
    strm.total_in += consumed as u64;

    // No full marker yet.
    if strm
        .inflate_state()
        .expect("inflate state present (checked above)")
        .have
        != 4
    {
        return (ReturnCode::DataError, consumed);
    }

    // Marker found: reset to resume decoding on the next block, preserving the
    // running totals and header status.
    let flags = strm
        .inflate_state()
        .expect("inflate state present (checked above)")
        .flags;
    {
        let state = strm
            .inflate_state_mut()
            .expect("inflate state present (checked above)");
        if state.flags == -1 {
            state.wrap = 0; // no header seen yet: treat as raw
        } else {
            state.wrap &= !4; // computing a check value now is pointless
        }
    }
    let in_total = strm.total_in;
    let out_total = strm.total_out;
    // Cannot fail: the inflate state is present.
    let _ = inflate_reset(strm);
    strm.total_in = in_total;
    strm.total_out = out_total;
    {
        let state = strm
            .inflate_state_mut()
            .expect("inflate state present (checked above)");
        state.flags = flags;
        state.mode = InflateMode::Type;
    }
    (ReturnCode::Ok, consumed)
}

/// Reports whether the decoder is paused exactly at a stored-block boundary —
/// the Rust port of C `inflateSyncPoint` (`inflate.c` L1320-L1327).
///
/// Used by protocols such as PPP that emit `Z_SYNC_FLUSH`/`Z_FULL_FLUSH` empty
/// stored blocks and then verify the decoder is waiting for the length bytes.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state; otherwise
/// `Ok(true)` at a sync point and `Ok(false)` elsewhere.
pub fn inflate_sync_point<A: Allocator>(strm: &ZStream<A>) -> Result<bool, ZlibError> {
    let state = strm.inflate_state().ok_or(ZlibError::StreamError)?;
    Ok(state.mode == InflateMode::Stored && state.bits == 0)
}

/// Duplicates one owned buffer through `alloc` — the `ZALLOC` + `zmemcpy` pair C
/// `inflateCopy` performs for the window
/// (`ZALLOC(strm, 1U << wbits, sizeof(unsigned char))`, `inflate.c` L1346).
///
/// It is what makes the deep copy behind [`inflate_copy`] allocator-faithful:
/// the destination buffer comes from the same allocator that backs the source,
/// with one request, so a caller-supplied arena serves the copy as well as the
/// original (AAP §0.6.3, §0.6.5).
///
/// The rest of the clone needs no such help. Because the state records the active
/// decode tables as `usize` offsets into [`InflateState::codes`] plus a
/// fixed/dynamic discriminator ([`TableSource`]) — rather than self-referential
/// raw pointers — copying the fields verbatim already yields a correct,
/// independent clone, and the pointer fix-up dance C `inflateCopy` performs has no
/// counterpart here. (`InflateState` deliberately does not derive [`Clone`], so
/// the copy is written out explicitly.)
///
/// # Errors
///
/// [`ZlibError::MemError`] if the allocation is refused, or if it produced a
/// length other than `src.len()` (which would mean the geometry fields and the
/// buffer disagreed).
fn clone_buffer<A: Allocator, T>(
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

/// C `inflateCopy` allocates the destination state and its window through the
/// source stream's own `zalloc` and returns `Z_MEM_ERROR` if either fails
/// (`inflate.c` L1340-L1350, including the `ZFREE(copy)` that releases the state
/// when the window allocation fails). The destination state is therefore charged
/// to `alloc` first — becoming the copy's real home, so a caller-supplied arena
/// owns the copy — and the window is re-allocated through the same allocator,
/// each with one request, in C's order. On the global-allocator path the state is
/// boxed fallibly instead, so heap exhaustion is reported rather than aborting.
///
/// A caller-supplied `zalloc` reporting out-of-memory fails the copy with
/// [`ZlibError::MemError`] — exactly what C does — instead of completing it with
/// global-allocator storage. There is no global-allocator fallback, so a caller
/// who installed a bounded arena observes the failure instead of silently
/// receiving a copy in the global heap (AAP §0.6.3, §0.6.5); an infallible
/// [`Clone`] would return `Z_OK` while quietly escaping that arena, which is the
/// behavior AAP §0.6.5 forbids — and is why neither `AllocBuffer` nor the engine
/// states implement it.
fn try_clone_inflate_state<A: Allocator>(
    s: &InflateState,
    alloc: &A,
) -> Result<BoxedEngine<InflateState>, ZlibError> {
    // Fallible work first, so a failure abandons the copy before any state is
    // built; the partial clone is released by its own `Drop` (C's `ZFREE(copy)`).
    // C's order: the destination state first (`inflate.c` L1340), then the window
    // (L1346). Taking the reservation before the window clone is what makes a
    // bounded destination allocator refuse at C's request rather than a later one.
    let reservation = EngineReservation::<InflateState>::take(alloc).ok_or(ZlibError::MemError)?;
    // The window is requested from `alloc` with the same `(items, size)` split C
    // uses — `(1 << wbits, 1)` — rather than duplicated in place, so a custom Rust
    // allocator serves the copy as well as the original (AAP §0.6.3, §0.6.5). An
    // empty source buffer stays empty: C has nothing to copy either when the
    // window was never allocated.
    let window = if s.window.is_empty() {
        AllocBuffer::default()
    } else {
        clone_buffer(alloc, &s.window, 1usize << s.wbits)?
    };

    // Moving the finished value into the reserved region puts the copy in the
    // caller's own memory when a hook is active, and boxes it fallibly otherwise.
    reservation
        .fill(InflateState {
            mode: s.mode,
            last: s.last,
            wrap: s.wrap,
            havedict: s.havedict,
            flags: s.flags,
            dmax: s.dmax,
            check: s.check,
            total: s.total,
            head: s.head.clone(),
            // C's `zmemcpy(copy, state, sizeof(struct inflate_state))`
            // (`inflate.c` L1272-L1273) duplicates `head` verbatim, so the clone
            // reads the *same* caller-owned buffers; the marker travels with it.
            #[cfg(feature = "gzip")]
            head_foreign: s.head_foreign,
            wbits: s.wbits,
            wsize: s.wsize,
            whave: s.whave,
            wnext: s.wnext,
            window,
            hold: s.hold,
            bits: s.bits,
            rewound: s.rewound,
            length: s.length,
            offset: s.offset,
            extra: s.extra,
            lencode: s.lencode,
            distcode: s.distcode,
            lentable: s.lentable,
            disttable: s.disttable,
            lenbits: s.lenbits,
            distbits: s.distbits,
            ncode: s.ncode,
            nlen: s.nlen,
            ndist: s.ndist,
            have: s.have,
            next: s.next,
            lens: s.lens,
            work: s.work,
            codes: s.codes,
            sane: s.sane,
            back: s.back,
            was: s.was,
            // Carry the caller's allocator hook into the clone so the copied
            // window (already re-allocated through the same hook) and any future
            // re-allocation stay routed through the caller's `zalloc`/`zfree`
            // (AAP §0.6.3).
            alloc_hook: s.alloc_hook,
        })
        .ok_or(ZlibError::MemError)
}

/// Copies a complete inflate stream — the Rust port of C `inflateCopy`
/// (`inflate.c` L1328-L1367).
///
/// The `source` stream's bookkeeping fields and a deep clone of its decode state
/// (including the sliding window and any gzip header) are installed into `dest`,
/// so the two streams thereafter decode independently. This is typically used to
/// snapshot a decoder mid-stream (e.g. for random access).
///
/// Unlike C — which must re-base the `lencode`/`distcode`/`next` pointers into
/// the copied `codes` array — no fix-up is required here: those are `usize`
/// offsets, so `try_clone_inflate_state` produces a correct clone directly.
///
/// # Errors
/// * [`ZlibError::StreamError`] — `source` has no inflate state.
/// * [`ZlibError::MemError`] — the copy's window or state reservation could not
///   be allocated through the allocator backing the source's, mirroring C's
///   `ZFREE(copy); return Z_MEM_ERROR` (`inflate.c` L1343-L1349). `dest` is left
///   untouched in that case, because the copy is allocated in full before
///   anything is written to the destination.
pub fn inflate_copy<A: Allocator>(dest: &mut ZStream<A>, source: &ZStream<A>) -> InflateResult {
    let state = source.inflate_state().ok_or(ZlibError::StreamError)?;
    // Allocate the copy FIRST and bail out before touching `dest`, exactly as C
    // does — so an OOM leaves the destination stream unmodified.
    // C `inflateCopy` allocates through the *source* stream's `zalloc`
    // (`inflate.c` L1340-L1346); the FFI shim gives `dest` a duplicate of that
    // allocator, so requesting from `dest` is the same allocator either way.
    let copy = try_clone_inflate_state(state, dest.allocator())?;
    // Mirror C `zmemcpy(dest, source, sizeof(z_stream))` for the observable
    // stream bookkeeping (the allocator and I/O cursors are the caller's).
    dest.total_in = source.total_in;
    dest.total_out = source.total_out;
    dest.adler = source.adler;
    dest.data_type = source.data_type;
    dest.msg = source.msg;
    dest.set_inflate_state(copy);
    Ok(ReturnCode::Ok)
}

/// Toggles acceptance of invalid distances-too-far-back — the Rust port of C
/// `inflateUndermine` (`inflate.c` L1370-L1383).
///
/// The `INFLATE_ALLOW_INVALID_DISTANCE_TOOFAR_ARRR` build option is not enabled
/// (matching reference zlib defaults), so this always forces `sane` back on and
/// reports [`ZlibError::DataError`] to signal the feature is unavailable.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state, otherwise
/// always [`ZlibError::DataError`].
pub fn inflate_undermine<A: Allocator>(strm: &mut ZStream<A>, _subvert: i32) -> InflateResult {
    let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
    state.sane = true;
    Err(ZlibError::DataError)
}

/// Enables or disables trailer-checksum validation — the Rust port of C
/// `inflateValidate` (`inflate.c` L1385-L1395).
///
/// When `check` is `true` and the stream has a wrapper, the validate bit
/// (`wrap & 4`) is set so [`inflate`] verifies the Adler-32/CRC-32 trailer;
/// otherwise validation is turned off.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state.
pub fn inflate_validate<A: Allocator>(strm: &mut ZStream<A>, check: bool) -> InflateResult {
    let state = strm.inflate_state_mut().ok_or(ZlibError::StreamError)?;
    if check && state.wrap != 0 {
        state.wrap |= 4;
    } else {
        state.wrap &= !4;
    }
    Ok(ReturnCode::Ok)
}

/// Returns a diagnostic "mark" of decode progress — the Rust port of C
/// `inflateMark` (`inflate.c` L1397-L1406).
///
/// The result packs the number of bits back into the input at the current
/// literal/length code (high bits) and the number of bytes still to copy for an
/// in-progress match (low 16 bits). A value of `-(1 << 16)` indicates an invalid
/// stream state. Applications use this to locate the last full flush point.
#[must_use]
pub fn inflate_mark<A: Allocator>(strm: &ZStream<A>) -> i64 {
    let state = match strm.inflate_state() {
        Some(s) => s,
        None => return -(1i64 << 16),
    };
    let offset = match state.mode {
        InflateMode::Copy => state.length as i64,
        InflateMode::Match => state.was as i64 - state.length as i64,
        _ => 0,
    };
    ((state.back as i64) << 16) + offset
}

/// Returns the number of decode-table entries used so far — the Rust port of C
/// `inflateCodesUsed` (`inflate.c` L1408-L1413).
///
/// This equals [`InflateState::next`] (the C `state->next - state->codes`, since
/// `next` is already an index into [`InflateState::codes`]). Returns [`None`]
/// for an invalid stream state (the C `(unsigned long)-1` sentinel).
#[must_use]
pub fn inflate_codes_used<A: Allocator>(strm: &ZStream<A>) -> Option<usize> {
    strm.inflate_state().map(|s| s.next)
}

/// Releases the inflate state — the Rust port of C `inflateEnd` (`inflate.c`
/// L1155-L1165).
///
/// Dropping the boxed [`InflateState`] automatically frees the sliding window
/// (RAII subsumes the C `ZFREE(window)`), so this simply detaches the state from
/// `strm`.
///
/// # Errors
/// Returns [`ZlibError::StreamError`] if `strm` has no inflate state.
pub fn inflate_end<A: Allocator>(strm: &mut ZStream<A>) -> InflateResult {
    if !strm.is_inflate() {
        return Err(ZlibError::StreamError);
    }
    strm.clear_state();
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// One-call façade — the engine-owning half of C `uncompr.c`
//
// C `uncompr.c` is a separate translation unit that `#include`s `zlib.h` and
// calls `inflateInit`/`inflate`/`inflateEnd`, so in the `#include` order it sits
// ABOVE the engine, not beside `zutil.h`. The Rust layering reflects that: the
// complete `uncompress2_z` driver — the decode loop, the consumed/produced
// accounting, and the `uncompr.c` L78-L81 return-code folding — lives in
// `crate::util::uncompress` (layer 3, engine-free and generic over the
// `OneCallInflate` port it declares), and the engine adapter plus the two
// C-named entry points live here (layer 6). That is what keeps the module graph
// one-way — layer 3 never names an engine (AAP §0.3.1, §0.4.2 B2) — while the
// crate root still re-exports `uncompress` and `uncompress2` under exactly the
// names `zlib.h` publishes.
// ===========================================================================

/// The [`OneCallInflate`] adapter: a private, self-contained stream driven by the
/// layer-3 `uncompress2_z` transcription.
///
/// It exists only for the duration of one `uncompress`/`uncompress2` call and
/// holds nothing beyond the stream itself, so the three trait methods are
/// literally the three C calls `uncompress2_z` makes.
struct OneCallInflateEngine {
    /// The stream this call owns, initialised by [`OneCallInflate::begin`].
    strm: ZStream,
}

impl OneCallInflate for OneCallInflateEngine {
    /// C `inflateInit(&stream)` (`uncompr.c` L50).
    ///
    /// `ZStream::new` installs the default (global) allocator, exactly as C
    /// `uncompress2_z` zeroes `zalloc`/`zfree`/`opaque` so `inflateInit`
    /// substitutes its own, and `inflate_init` defaults `windowBits` to
    /// `DEF_WBITS` (15).
    fn begin() -> Result<Self, ReturnCode> {
        let mut strm = ZStream::new();
        match inflate_init(&mut strm) {
            Ok(_) => Ok(Self { strm }),
            Err(err) => Err(err.as_return_code()),
        }
    }

    /// C `inflate(&stream, Z_NO_FLUSH)` (`uncompr.c` L58).
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> OneCallStep {
        let outcome = inflate(&mut self.strm, input, output, flush);
        OneCallStep {
            consumed: outcome.consumed,
            produced: outcome.produced,
            code: outcome.code,
        }
    }

    /// C `inflateEnd(&stream)` (`uncompr.c` L76).
    ///
    /// C ignores the return value, and so does this: the outcome of the call has
    /// already been decided. Dropping the stream afterwards is safe because
    /// `inflate_end` clears the state, so the `Drop` is a no-op (no double free).
    fn end(&mut self) {
        let _ = inflate_end(&mut self.strm);
    }
}

/// Decompresses the whole zlib stream in `source` into `dest`, reporting the
/// number of source bytes consumed.
///
/// This is the idiomatic port of C `uncompress2` / `uncompress2_z`
/// (`uncompr.c` L29-L90); see
/// `crate::util::uncompress::uncompress2_with` for the
/// parameter contract, which this function forwards verbatim.
///
/// # Errors
///
/// Reproduces `uncompr.c` L78-L81 verbatim: [`ReturnCode::DataError`] for
/// corrupt, truncated, or dictionary-requiring input, [`ReturnCode::BufError`]
/// when `dest` is too small while input remains, [`ReturnCode::MemError`] on
/// allocation failure, and any other engine code unchanged.
///
/// # Examples
///
/// ```
/// # use zlib_rs::{compress, compress_bound, uncompress2};
/// // Build a valid zlib stream; `plain_len` is its decompressed size.
/// let plain = b"the quick brown fox";
/// let mut zlib = vec![0u8; compress_bound(plain.len())];
/// let m = compress(&mut zlib, plain).unwrap();
/// zlib.truncate(m);
/// let plain_len = plain.len();
///
/// let mut out = vec![0u8; plain_len];
/// let mut consumed = zlib.len();
/// let mut produced = out.len();
/// let n = uncompress2(&mut out, &zlib, &mut consumed, &mut produced).unwrap();
/// assert_eq!(n, plain_len);
/// assert_eq!(produced, plain_len);   // output count reported on all paths
/// assert_eq!(consumed, zlib.len());  // whole stream read
/// assert_eq!(&out[..], plain);
/// ```
pub fn uncompress2(
    dest: &mut [u8],
    source: &[u8],
    source_len: &mut usize,
    dest_len: &mut usize,
) -> Result<usize, ReturnCode> {
    uncompress2_with::<OneCallInflateEngine>(dest, source, source_len, dest_len)
}

/// Decompresses the whole zlib stream in `source` into `dest`.
///
/// This is the classic convenience wrapper — the port of C `uncompress`
/// (`uncompr.c` L92-L101) — for callers that do not care how many input bytes
/// were consumed. It treats the *entire* `source` slice as the available input
/// and forwards to [`uncompress2`], discarding the consumed-length report.
///
/// # Returns
///
/// * `Ok(produced)` — `produced` bytes were written to the front of `dest`.
/// * `Err(code)` — see [`uncompress2`] for the exact error mapping.
///
/// # Errors
///
/// Identical to [`uncompress2`]: [`ReturnCode::DataError`] for corrupt or
/// incomplete input (or a needed preset dictionary), [`ReturnCode::BufError`]
/// when `dest` is too small, and [`ReturnCode::MemError`] on allocation failure.
///
/// # Examples
///
/// ```
/// # use zlib_rs::{compress, compress_bound, uncompress};
/// let plain = b"the quick brown fox";
/// let mut zlib = vec![0u8; compress_bound(plain.len())];
/// let m = compress(&mut zlib, plain).unwrap();
/// zlib.truncate(m);
/// let plain_len = plain.len();
///
/// let mut out = vec![0u8; plain_len];
/// let n = uncompress(&mut out, &zlib).unwrap();
/// assert_eq!(n, plain_len);
/// assert_eq!(&out[..], plain);
/// ```
pub fn uncompress(dest: &mut [u8], source: &[u8]) -> Result<usize, ReturnCode> {
    // C `uncompress` seeds a local `used = sourceLen` and calls `uncompress2`,
    // then throws the updated `used` away. The produced-count out-parameter is
    // likewise discarded here (the `Ok(produced)` return still carries it).
    let mut used = source.len();
    let mut produced = dest.len();
    uncompress2(dest, source, &mut used, &mut produced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::Z_NO_FLUSH;
    use alloc::vec::Vec;

    /// The uncompressed reference message. It repeats "hello, " so the decoder
    /// exercises the length/distance/match copy path (back-references into the
    /// window), not just literal output.
    const MSG: &[u8] = b"hello, hello, hello, world!";

    /// `MSG` compressed as a zlib stream (RFC 1950), level 6.
    const ZLIB_STREAM: &[u8] = &[
        120, 156, 203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243, 139, 114, 82, 20, 1,
        133, 250, 9, 106,
    ];

    /// `MSG` compressed as raw DEFLATE (RFC 1951), level 6, `windowBits = -15`.
    const RAW_STREAM: &[u8] = &[
        203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243, 139, 114, 82, 20, 1,
    ];

    /// `MSG` compressed as a gzip stream (RFC 1952), level 6,
    /// `windowBits = 16 + 15`.
    #[cfg(feature = "gzip")]
    const GZIP_STREAM: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 203, 72, 205, 201, 201, 215, 81, 200, 64, 161, 202, 243,
        139, 114, 82, 20, 1, 131, 137, 31, 110, 27, 0, 0, 0,
    ];

    /// Fully decodes `input` (given `window_bits`) in a single call with a
    /// generous output buffer, asserting the message round-trips exactly and the
    /// stream ends cleanly.
    fn decode_ok(input: &[u8], window_bits: i32) {
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, window_bits), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let outcome = inflate(&mut strm, input, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "expected Z_STREAM_END, got {:?} (msg {:?})",
            outcome.code,
            strm.msg
        );
        assert_eq!(outcome.consumed, input.len(), "all input consumed");
        assert_eq!(&out[..outcome.produced], MSG, "decoded bytes match input");
        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));
    }

    #[test]
    fn round_trip_zlib() {
        decode_ok(ZLIB_STREAM, 15);
    }

    #[test]
    fn round_trip_raw() {
        decode_ok(RAW_STREAM, -15);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn round_trip_gzip() {
        decode_ok(GZIP_STREAM, 16 + 15);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn round_trip_auto_detect() {
        // windowBits 32 + 15 auto-detects a zlib *or* gzip wrapper.
        decode_ok(ZLIB_STREAM, 32 + 15);
        decode_ok(GZIP_STREAM, 32 + 15);
    }

    #[test]
    fn decode_with_tight_output_buffer() {
        // A 4-byte output buffer forces many `Z_OK` returns, exercising the
        // resumable window-copy / continuation paths across calls.
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        let mut decoded = Vec::new();
        let mut in_pos = 0usize;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 10_000, "decode did not terminate");
            let mut out = [0u8; 4];
            let outcome = inflate(&mut strm, &ZLIB_STREAM[in_pos..], &mut out, Z_NO_FLUSH);
            in_pos += outcome.consumed;
            decoded.extend_from_slice(&out[..outcome.produced]);
            match outcome.code {
                ReturnCode::StreamEnd => break,
                ReturnCode::Ok | ReturnCode::BufError => {
                    assert!(
                        outcome.consumed != 0 || outcome.produced != 0,
                        "stalled with code {:?}",
                        outcome.code
                    );
                }
                other => panic!("unexpected code {other:?}"),
            }
        }
        assert_eq!(decoded.as_slice(), MSG);
    }

    #[test]
    fn inflate_without_state_is_stream_error() {
        let mut strm = ZStream::new();
        let mut out = [0u8; 16];
        let outcome = inflate(&mut strm, &[], &mut out, Z_NO_FLUSH);
        assert_eq!(outcome.code, ReturnCode::StreamError);
    }

    #[test]
    fn corrupt_body_is_data_error() {
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        // Valid zlib header (0x78 0x9c) followed by an invalid block type run.
        let bad = [0x78u8, 0x9c, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let mut out = [0u8; 64];
        let outcome = inflate(&mut strm, &bad, &mut out, Z_NO_FLUSH);
        assert_eq!(outcome.code, ReturnCode::DataError);
        assert!(strm.msg.is_some(), "a diagnostic message must be set");
    }

    #[test]
    fn bad_header_check_is_data_error() {
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        // CMF = 0x78 is a valid deflate method (CM = 8) with a 32 KiB window,
        // but FLG = 0x00 makes the 16-bit header value 0x7800, and
        // 0x7800 % 31 == 30 (non-zero), so the zlib header checksum fails.
        // Reference zlib reports "incorrect header check" for exactly this
        // input (verified against the C library). Contrast with 0x0000, which
        // *passes* the %31 check and instead trips the compression-method test.
        let bad = [0x78u8, 0x00];
        let mut out = [0u8; 16];
        let outcome = inflate(&mut strm, &bad, &mut out, Z_NO_FLUSH);
        assert_eq!(outcome.code, ReturnCode::DataError);
        assert_eq!(strm.msg, Some("incorrect header check"));
    }

    #[test]
    fn invalid_window_bits_rejected() {
        let mut strm = ZStream::new();
        // 7 is below the minimum window size of 8.
        assert_eq!(inflate_init2(&mut strm, 7), Err(ZlibError::StreamError));
    }

    #[test]
    fn reset_reinitialises_for_a_fresh_member() {
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let first = inflate(&mut strm, ZLIB_STREAM, &mut out, Z_NO_FLUSH);
        assert_eq!(first.code, ReturnCode::StreamEnd);
        // After a reset the same decoder decodes the member again from scratch.
        assert_eq!(inflate_reset(&mut strm), Ok(ReturnCode::Ok));
        let second = inflate(&mut strm, ZLIB_STREAM, &mut out, Z_NO_FLUSH);
        assert_eq!(second.code, ReturnCode::StreamEnd);
        assert_eq!(&out[..second.produced], MSG);
    }

    /// The gzip rows of the `adler` matrix below, or an empty vector when the
    /// `gzip` feature is off.
    ///
    /// `(windowBits, stream, checksum the decode epilogue must publish)`:
    /// gzip framing is seeded with `crc32(0, NULL, 0) == 0` and publishes the
    /// CRC-32, while automatic detection resolves to `wrap = 7`, so `wrap & 1`
    /// seeds `1` even when the member turns out to be gzip.
    fn gzip_adler_cases() -> alloc::vec::Vec<(i32, &'static [u8], Option<u32>)> {
        #[cfg(feature = "gzip")]
        {
            alloc::vec![
                (31, GZIP_STREAM, Some(crc32(0, MSG))),
                (47, GZIP_STREAM, Some(crc32(0, MSG))),
                (47, ZLIB_STREAM, Some(adler32(1, MSG))),
            ]
        }
        #[cfg(not(feature = "gzip"))]
        {
            alloc::vec::Vec::new()
        }
    }

    /// The observable `z_stream.adler` value must match reference zlib at every
    /// point in a decoder's lifetime, for **every** wrapper — including raw
    /// framing, where C never assigns the field at all.
    ///
    /// C's contract, reproduced here in full:
    ///
    /// * The caller arrives with a `memset`-zeroed `z_stream`, so `adler` is `0`
    ///   before `inflateInit2`. [`ZStream::new`] therefore seeds `0`, not `1`.
    /// * `inflateResetKeep` publishes the wrapper's initial checksum, but only
    ///   under `if (state->wrap)` (`inflate.c` L108-L109): `1` for zlib and
    ///   automatic detection (`wrap & 1 == 1`), `0` for gzip (`wrap == 6`), and
    ///   **nothing** for raw (`wrap == 0`), which leaves the caller's zero in
    ///   place forever.
    /// * The decode epilogue publishes the running check under
    ///   `if ((state->wrap & 4) && out)` (`inflate.c` L1144-L1146), and raw
    ///   framing has bit 2 clear as well, so a raw decode never touches it
    ///   either.
    ///
    /// Regression guard: seeding the constructor with `1` made the safe API
    /// report `1` for raw streams where both [`crate::ffi`] and reference zlib
    /// report `0` — a silent divergence in an observable public field (AAP S5),
    /// invisible to every round-trip test because the decoded bytes are correct.
    #[test]
    fn adler_mirror_matches_c_for_every_wrapper() {
        // A stream with no engine installed reports the zeroed C default.
        assert_eq!(
            ZStream::new().adler,
            0,
            "a fresh stream must match a memset-zeroed C z_stream"
        );

        // (windowBits, stream, checksum the epilogue must publish once decoded)
        // `None` means "the field is never written, so it stays at 0".
        let mut cases: alloc::vec::Vec<(i32, &[u8], Option<u32>)> = alloc::vec![
            // Raw: no checksum exists, and C assigns nothing on any path.
            (-15, RAW_STREAM, None),
            // A smaller raw window still decodes this stream (all distances are
            // well under 512 bytes) and must behave identically.
            (-9, RAW_STREAM, None),
            // zlib: seeded with adler32(0, NULL, 0) == 1, published as the
            // Adler-32 of the output.
            (15, ZLIB_STREAM, Some(adler32(1, MSG))),
        ];
        // The gzip rows live in a helper so this extension is unconditional —
        // it yields an empty vector without the `gzip` feature. Pushing them
        // inside a `#[cfg]` block instead would leave `cases` provably unmutated
        // on a `--no-default-features` build and trip `unused_mut`.
        cases.extend(gzip_adler_cases());

        for (window_bits, stream, decoded_check) in cases {
            // `wrap` is `(windowBits >> 4) + 5` for non-negative windowBits and
            // `0` for raw, so the seed is exactly `wrap & 1`.
            let seed = if window_bits < 0 {
                0
            } else {
                ((window_bits >> 4) + 5) as u32 & 1
            };

            let mut strm = ZStream::new();
            assert_eq!(inflate_init2(&mut strm, window_bits), Ok(ReturnCode::Ok));
            assert_eq!(
                strm.adler, seed,
                "windowBits {window_bits}: adler after init must be `wrap & 1`"
            );

            let mut out = alloc::vec![0u8; 256];
            let outcome = inflate(&mut strm, stream, &mut out, Z_NO_FLUSH);
            assert_eq!(
                outcome.code,
                ReturnCode::StreamEnd,
                "windowBits {window_bits}: expected Z_STREAM_END"
            );
            assert_eq!(&out[..outcome.produced], MSG);
            assert_eq!(
                strm.adler,
                decoded_check.unwrap_or(0),
                "windowBits {window_bits}: adler after a full decode"
            );

            // Both resets restore the seed (or, for raw, leave the zero alone).
            assert_eq!(inflate_reset(&mut strm), Ok(ReturnCode::Ok));
            assert_eq!(
                strm.adler, seed,
                "windowBits {window_bits}: adler after inflate_reset"
            );
            assert_eq!(inflate_reset_keep(&mut strm), Ok(ReturnCode::Ok));
            assert_eq!(
                strm.adler, seed,
                "windowBits {window_bits}: adler after inflate_reset_keep"
            );
            assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));
        }
    }

    /// A raw decode that errors out, or produces no output at all, must still
    /// leave `adler` at the caller's zero — the paths where an unguarded
    /// epilogue write would have leaked an internal `state.check` value.
    #[test]
    fn raw_framing_never_publishes_a_check_value() {
        // Truncated input: the decoder consumes bytes and returns Z_OK without
        // reaching the end of the block.
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, -15), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let partial = inflate(&mut strm, &RAW_STREAM[..4], &mut out, Z_NO_FLUSH);
        assert_eq!(partial.code, ReturnCode::Ok, "a truncated raw block stalls");
        assert_eq!(strm.adler, 0, "a partial raw decode publishes nothing");
        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));

        // A zero-length output buffer produces nothing at all.
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, -15), Ok(ReturnCode::Ok));
        let mut none: [u8; 0] = [];
        let _ = inflate(&mut strm, RAW_STREAM, &mut none, Z_NO_FLUSH);
        assert_eq!(strm.adler, 0, "no output means no check to publish");
        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));

        // Corrupt input: the error path must not leak a partial check either.
        let mut corrupt = RAW_STREAM.to_vec();
        corrupt[2] ^= 0xFF;
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, -15), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let _ = inflate(&mut strm, &corrupt, &mut out, Z_NO_FLUSH);
        assert_eq!(strm.adler, 0, "a failed raw decode publishes nothing");
        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));
    }

    #[test]
    fn mark_is_sentinel_without_state() {
        let strm = ZStream::new();
        assert_eq!(inflate_mark(&strm), -(1i64 << 16));
    }

    #[test]
    fn codes_used_is_none_without_state() {
        let strm = ZStream::new();
        assert_eq!(inflate_codes_used(&strm), None);
    }

    #[test]
    fn prime_and_flush_bits() {
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        // Inject 5 bits, then confirm a negative count flushes them back out.
        assert_eq!(inflate_prime(&mut strm, 5, 0b1_0101), Ok(ReturnCode::Ok));
        assert_eq!(inflate_prime(&mut strm, -1, 0), Ok(ReturnCode::Ok));
        // Over-wide bit counts are rejected.
        assert_eq!(inflate_prime(&mut strm, 17, 0), Err(ZlibError::StreamError));
    }

    /// C `inflateGetDictionary` (`inflate.c` L1167-L1185) always reports the
    /// history length and copies the history only when the caller's buffer can
    /// hold all of it — which is what makes the C two-call idiom (query with
    /// `Z_NULL`, allocate, fetch) work.
    ///
    /// Four cases with no other witness in the suite: no installed state, a
    /// stream that has decoded nothing, a short un-wrapped history, and a buffer
    /// too small for the history (length still reported, nothing copied). The
    /// wrapped-history case needs a small window and is asserted separately.
    #[test]
    fn inflate_get_dictionary_reports_the_length_and_copies_only_when_it_fits() {
        // Without an installed state there is nothing to report; the length
        // out-parameter must be left exactly as the caller set it, because C
        // returns before it writes `*dictLength`.
        let bare = ZStream::new();
        let mut sink = [0xa5u8; 8];
        let mut len = usize::MAX;
        assert_eq!(
            inflate_get_dictionary(&bare, &mut sink, &mut len),
            Err(ZlibError::StreamError)
        );
        assert_eq!(len, usize::MAX, "a rejected call writes no length");
        assert_eq!(sink, [0xa5u8; 8], "a rejected call writes no bytes");

        // A stream that has decoded nothing has an empty history: the length is
        // reported as 0 and the `whave != 0` guard suppresses the copy.
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        let mut len = 7usize;
        assert_eq!(
            inflate_get_dictionary(&strm, &mut sink, &mut len),
            Ok(ReturnCode::Ok)
        );
        assert_eq!(len, 0, "nothing decoded yet means no history");
        assert_eq!(sink, [0xa5u8; 8], "an empty history writes no bytes");

        // Decode PART of the reference message: a 20-byte output buffer stops the
        // call for output space, so it returns `Z_OK` with the window updated and
        // the history contiguous (`whave == wnext == 20`, far below the 32 KiB
        // window). Stopping short is deliberate - see the completed-stream case
        // at the end of this test for why finishing would report NO history.
        const PART: usize = 20;
        assert!(PART < MSG.len());
        let mut out = alloc::vec![0u8; PART];
        let outcome = inflate(&mut strm, ZLIB_STREAM, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::Ok,
            "the call must stop for output space, not end"
        );
        assert_eq!(outcome.produced, PART);
        assert_eq!(&out[..], &MSG[..PART]);
        {
            let s = strm.inflate_state().expect("init installs a state");
            assert_eq!(s.whave as usize, PART, "history length");
            assert_eq!(
                s.wnext as usize, PART,
                "the circular cursor has not wrapped"
            );
        }

        // A buffer exactly the size of the history receives all of it.
        let mut exact = alloc::vec![0u8; PART];
        let mut len = 0usize;
        assert_eq!(
            inflate_get_dictionary(&strm, &mut exact, &mut len),
            Ok(ReturnCode::Ok)
        );
        assert_eq!(len, PART);
        assert_eq!(exact, MSG[..PART], "the decoded bytes come back verbatim");

        // A larger buffer receives exactly `whave` bytes; the tail is untouched.
        let mut oversized = alloc::vec![0x5au8; PART + 16];
        let mut len = 0usize;
        assert_eq!(
            inflate_get_dictionary(&strm, &mut oversized, &mut len),
            Ok(ReturnCode::Ok)
        );
        assert_eq!(len, PART);
        assert_eq!(&oversized[..PART], &MSG[..PART]);
        assert!(
            oversized[PART..].iter().all(|&b| b == 0x5a),
            "bytes beyond the history length must be left untouched"
        );

        // A buffer one byte too small: the length is still reported (this is the
        // sizing half of the two-call idiom) and NOT a single byte is copied.
        let mut small = alloc::vec![0xa5u8; PART - 1];
        let mut len = 0usize;
        assert_eq!(
            inflate_get_dictionary(&strm, &mut small, &mut len),
            Ok(ReturnCode::Ok)
        );
        assert_eq!(
            len, PART,
            "the length is reported even when it does not fit"
        );
        assert!(
            small.iter().all(|&b| b == 0xa5),
            "a too-small buffer must be left entirely untouched"
        );

        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));

        // The counter-intuitive C behaviour, pinned deliberately: a stream small
        // enough to decode COMPLETELY in one call reports NO history at all.
        // C folds the produced bytes into the check value in the `CHECK` state and
        // then resets its progress counter (`out = left`, `inflate.c` L1121), so
        // `inf_leave`'s window-update condition (`state->wsize ||
        // (out != strm->avail_out && ...)`, L1133-L1136) is false on both clauses:
        // nothing was produced *since the fold*, and the window was never
        // allocated. A port that updated the window unconditionally would report a
        // dictionary here where reference zlib reports none.
        let mut whole = ZStream::new();
        assert_eq!(inflate_init2(&mut whole, 15), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let outcome = inflate(&mut whole, ZLIB_STREAM, &mut out, Z_NO_FLUSH);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        assert_eq!(&out[..outcome.produced], MSG);
        let mut sink = [0xa5u8; 8];
        let mut len = 7usize;
        assert_eq!(
            inflate_get_dictionary(&whole, &mut sink, &mut len),
            Ok(ReturnCode::Ok)
        );
        assert_eq!(
            len, 0,
            "a stream decoded entirely within one call leaves the window untouched"
        );
        assert_eq!(sink, [0xa5u8; 8]);
        assert_eq!(inflate_end(&mut whole), Ok(ReturnCode::Ok));
    }

    /// Once the circular history window has wrapped, the dictionary must come
    /// back in **chronological** order: the older tail of the buffer
    /// (`window[wnext..whave]`) first, then the newer head (`window[..wnext]`).
    ///
    /// This is the single defect this API is most likely to carry, and it is
    /// invisible to a round-trip test: the two halves are both present, so a
    /// port that emitted them in buffer order rather than chronological order
    /// returns exactly the right bytes in exactly the wrong sequence, and every
    /// decode still succeeds.
    ///
    /// Reaching the case needs a small window (`windowBits = 9`, 512 bytes) so it
    /// fills quickly, more decoded output than the window, the output delivered
    /// in **small chunks**, and the query taken **mid-stream**. Every obvious
    /// shortcut skips the wrap entirely: a single call producing at least `wsize`
    /// bytes takes C's `copy >= wsize` branch, which resets `wnext` to 0; and the
    /// call that reaches `CHECK` updates the window with `copy == 0`, because C
    /// folds the produced bytes into the check value and then resets its progress
    /// counter (`out = left`, `inflate.c` L1121), so driving to `Z_STREAM_END`
    /// leaves the history one chunk behind the decoded output. Stopping the moment
    /// the wrap is observed keeps the expected bytes exactly derivable from what
    /// was decoded, and both `whave == wsize` and `wnext != 0` are asserted so the
    /// test cannot silently stop exercising the wrap.
    #[test]
    fn inflate_get_dictionary_returns_a_wrapped_history_in_chronological_order() {
        const W_BITS: i32 = 9;
        const CHUNK: usize = 100;
        let w_size = 1usize << W_BITS;

        // A non-repeating ramp: any misordering is unmistakable. Comfortably more
        // than two windows, so the wrap is reached well before the stream ends.
        let payload: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(29) % 251) as u8)
            .collect();
        let comp = zlib_compress_wb(&payload, W_BITS);

        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, W_BITS), Ok(ReturnCode::Ok));

        // Feed the whole compressed stream but take the output in fixed chunks, so
        // every call updates the window with fewer than `wsize` bytes, and stop as
        // soon as the cursor has wrapped.
        let mut decoded = Vec::new();
        let mut in_off = 0usize;
        let mut buf = alloc::vec![0u8; CHUNK];
        let (whave, wnext) = loop {
            let r = inflate(&mut strm, &comp[in_off..], &mut buf, Z_NO_FLUSH);
            in_off += r.consumed;
            decoded.extend_from_slice(&buf[..r.produced]);
            let (whave, wnext) = {
                let s = strm.inflate_state().expect("init installs a state");
                (s.whave as usize, s.wnext as usize)
            };
            if whave == w_size && wnext != 0 {
                break (whave, wnext);
            }
            assert_eq!(
                r.code,
                ReturnCode::Ok,
                "the window must wrap before the stream ends (whave {whave}, wnext {wnext})"
            );
        };
        assert_eq!(
            decoded,
            payload[..decoded.len()],
            "the decoded prefix round-trips"
        );
        assert!(
            decoded.len() > w_size,
            "more than one window must have passed through (decoded {})",
            decoded.len()
        );
        assert!(
            wnext < whave,
            "the wrapped cursor stays inside the history (wnext {wnext}, whave {whave})"
        );

        let mut got = alloc::vec![0u8; whave];
        let mut len = 0usize;
        assert_eq!(
            inflate_get_dictionary(&strm, &mut got, &mut len),
            Ok(ReturnCode::Ok)
        );
        assert_eq!(len, whave);
        assert_eq!(
            got,
            decoded[decoded.len() - whave..],
            "a wrapped history must be returned oldest byte first"
        );

        // The independent statement of the same requirement, in terms of the raw
        // buffer: the two halves, spliced at `wnext`, are what was returned.
        {
            let s = strm.inflate_state().expect("init installs a state");
            let mut spliced = Vec::with_capacity(whave);
            spliced.extend_from_slice(&s.window[wnext..whave]);
            spliced.extend_from_slice(&s.window[..wnext]);
            assert_eq!(got, spliced, "window[wnext..whave] then window[..wnext]");
        }

        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));
    }

    /// C `inflateValidate` (`inflate.c` L1385-L1395) toggles bit 2 of `wrap`,
    /// which is the bit every trailer comparison in the decoder is guarded on.
    ///
    /// Both halves are asserted: the bit arithmetic (`wrap |= 4` only for a
    /// wrapped stream, `wrap &= ~4` otherwise, with the wrapper bits preserved
    /// either way) and the behaviour it buys — a zlib stream whose Adler-32
    /// trailer has been corrupted is rejected with validation on and accepted,
    /// with the payload intact, with validation off. Asserting only the bit
    /// would be satisfied by an implementation nothing reads.
    #[test]
    fn inflate_validate_toggles_the_check_bit_and_governs_trailer_verification() {
        // No installed state: rejected before anything is touched.
        let mut bare = ZStream::new();
        assert_eq!(
            inflate_validate(&mut bare, true),
            Err(ZlibError::StreamError)
        );
        assert_eq!(
            inflate_validate(&mut bare, false),
            Err(ZlibError::StreamError)
        );

        // A zlib stream starts at `wrap == 5`: bit 0 selects the RFC 1950
        // wrapper, bit 2 enables trailer validation (`inflate.c` L246-L253).
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 15), Ok(ReturnCode::Ok));
        let wrap = |s: &ZStream| s.inflate_state().expect("state").wrap;
        assert_eq!(wrap(&strm), 5, "zlib framing validates by default");

        assert_eq!(inflate_validate(&mut strm, false), Ok(ReturnCode::Ok));
        assert_eq!(
            wrap(&strm),
            1,
            "turning validation off clears exactly bit 2, keeping the wrapper"
        );
        // Idempotent: clearing an already-clear bit changes nothing.
        assert_eq!(inflate_validate(&mut strm, false), Ok(ReturnCode::Ok));
        assert_eq!(wrap(&strm), 1);

        assert_eq!(inflate_validate(&mut strm, true), Ok(ReturnCode::Ok));
        assert_eq!(wrap(&strm), 5, "turning it back on restores exactly bit 2");
        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));

        // A raw stream has `wrap == 0`, so there is no check value to validate and
        // `check && state->wrap` is false: enabling validation must be a no-op
        // rather than setting a bit that would make the decoder look for a
        // trailer that does not exist.
        let mut raw = ZStream::new();
        assert_eq!(inflate_init2(&mut raw, -15), Ok(ReturnCode::Ok));
        assert_eq!(wrap(&raw), 0, "raw framing carries no wrapper");
        assert_eq!(inflate_validate(&mut raw, true), Ok(ReturnCode::Ok));
        assert_eq!(
            wrap(&raw),
            0,
            "a raw stream must not acquire the validate bit"
        );
        assert_eq!(inflate_end(&mut raw), Ok(ReturnCode::Ok));

        // ---- the behaviour the bit buys ----------------------------------
        // Corrupt only the final byte of the Adler-32 trailer, leaving the
        // compressed body and the header byte-exact.
        let mut corrupt = ZLIB_STREAM.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;

        // Validation on (the default): Z_DATA_ERROR from the trailer comparison.
        let mut checked = ZStream::new();
        assert_eq!(inflate_init2(&mut checked, 15), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let outcome = inflate(&mut checked, &corrupt, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::DataError,
            "a corrupted Adler-32 trailer must be rejected while validation is on"
        );
        assert_eq!(inflate_end(&mut checked), Ok(ReturnCode::Ok));

        // Validation off: the same bytes decode to Z_STREAM_END and the payload is
        // recovered exactly. This is the whole point of the API.
        let mut unchecked = ZStream::new();
        assert_eq!(inflate_init2(&mut unchecked, 15), Ok(ReturnCode::Ok));
        assert_eq!(inflate_validate(&mut unchecked, false), Ok(ReturnCode::Ok));
        let mut out = alloc::vec![0u8; 256];
        let outcome = inflate(&mut unchecked, &corrupt, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "with validation off the corrupted trailer is accepted"
        );
        assert_eq!(
            &out[..outcome.produced],
            MSG,
            "the payload is still recovered exactly"
        );
        assert_eq!(inflate_end(&mut unchecked), Ok(ReturnCode::Ok));
    }

    /// A payload that forces the decoder onto its realistic paths: the repeated
    /// phrases guarantee length/distance back-references into the history window,
    /// while the varied vocabulary makes a **dynamic** Huffman block the cheapest
    /// encoding, so `inflate` builds real dynamic code tables in `codes[]` rather
    /// than pointing at the module-static fixed ones.
    fn varied_payload() -> Vec<u8> {
        const WORDS: [&[u8]; 8] = [
            b"alpha ",
            b"bravo ",
            b"charlie ",
            b"delta ",
            b"echo ",
            b"foxtrot ",
            b"golf ",
            b"hotel ",
        ];
        let mut v = Vec::new();
        let mut i = 0usize;
        while v.len() < 6000 {
            v.extend_from_slice(WORDS[(i * 7 + 3) % 8]);
            if i % 11 == 0 {
                v.extend_from_slice(b"the quick brown fox jumps over the lazy dog; ");
            }
            i += 1;
        }
        v
    }

    /// Compresses `data` as a zlib stream at level 6 with the crate's own
    /// encoder, whose output is byte-identical to reference zlib.
    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        zlib_compress_wb(data, 15)
    }

    /// Compresses `data` as a zlib stream at level 6 with the given
    /// `window_bits`.
    ///
    /// The encoder's window size bounds the largest match distance it will emit,
    /// so a decoder opened with the **same** `window_bits` can never meet a
    /// distance its window cannot satisfy. That matters for the small-window
    /// history tests below: compressing at 15 and decoding at 9 would be a
    /// legitimately invalid pairing, and the resulting `Z_DATA_ERROR` would look
    /// like a decoder defect rather than a fixture mistake.
    fn zlib_compress_wb(data: &[u8], window_bits: i32) -> Vec<u8> {
        use crate::constants::{Strategy, Z_DEFLATED};
        let mut strm = ZStream::new();
        crate::deflate::deflate_init2(&mut strm, 6, Z_DEFLATED, window_bits, 8, Strategy::Default)
            .expect("deflate init");
        let mut out = alloc::vec![0u8; data.len() * 2 + 128];
        let r = crate::deflate::deflate(&mut strm, data, &mut out, Z_FINISH);
        assert_eq!(r.code, ReturnCode::StreamEnd);
        assert_eq!(r.consumed, data.len());
        out.truncate(r.produced);
        crate::deflate::deflate_end(&mut strm).expect("deflate end");
        out
    }

    /// Drives `strm` to `Z_STREAM_END`, offering at most `in_chunk` input bytes
    /// and at most `out_chunk` output bytes per call, and returns everything
    /// produced.
    ///
    /// Two streams that share a history can be driven with completely different
    /// `in_chunk`/`out_chunk` values, which is how the copy test diverges the call
    /// pattern of the original and its snapshot.
    fn resume_to_end(
        strm: &mut ZStream,
        input: &[u8],
        in_chunk: usize,
        out_chunk: usize,
    ) -> Vec<u8> {
        let mut got = Vec::new();
        let mut in_off = 0usize;
        let mut buf = alloc::vec![0u8; out_chunk];
        loop {
            let end_in = (in_off + in_chunk).min(input.len());
            let r = inflate(strm, &input[in_off..end_in], &mut buf, Z_NO_FLUSH);
            in_off += r.consumed;
            got.extend_from_slice(&buf[..r.produced]);
            match r.code {
                ReturnCode::StreamEnd => return got,
                ReturnCode::Ok => assert!(
                    r.consumed > 0 || r.produced > 0 || in_off < input.len(),
                    "inflate made no progress with input exhausted (truncated stream?)"
                ),
                other => panic!("unexpected inflate return code {other:?}"),
            }
        }
    }

    /// Decodes `comp` until `stop` output bytes exist, leaving the stream **live**
    /// mid-block with a populated history window and populated dynamic code
    /// tables. Returns the input offset at which decoding must resume.
    fn drive_to_midstream(strm: &mut ZStream, comp: &[u8], stop: usize) -> usize {
        let mut out = alloc::vec![0u8; stop];
        let r = inflate(strm, comp, &mut out, Z_NO_FLUSH);
        assert_eq!(
            r.code,
            ReturnCode::Ok,
            "must stop for output space, not end"
        );
        assert_eq!(r.produced, stop, "output buffer should be filled exactly");
        r.consumed
    }

    /// `inflateCopy` produces a genuinely independent snapshot of a **live**
    /// decoder — one whose history window and dynamic Huffman tables are already
    /// populated — not a shallow alias.
    ///
    /// Copying at a trivial point — a few bytes into a fixed-table stream — and
    /// then checking only that a state was installed and that `total_out` matched
    /// would be satisfied by a shallow copy that shared every buffer. The snapshot
    /// is therefore taken mid-stream and the independence asserted directly:
    ///
    /// 1. decodes a dynamic-Huffman stream part-way, so `codes[]` holds real
    ///    tables, `lencode`/`distcode` are non-trivial offsets into that arena
    ///    with [`TableSource::Dynamic`] selected, and the sliding window holds
    ///    history (`wsize`/`whave` non-zero);
    /// 2. takes the copy and asserts field-for-field fidelity, including the
    ///    offsets and table discriminants that replace C's interior pointers
    ///    (AAP §0.6.3) — the very fields C's `inflateCopy` has to re-base by hand;
    /// 3. mutates the copy's window **and** its live decode table and observes
    ///    that the source is byte-for-byte unchanged, proving distinct storage;
    /// 4. resumes the two with **divergent chunking** — the source in one call,
    ///    the copy in small input and output slices — observes that their progress
    ///    genuinely diverges, and asserts both reconstruct the payload exactly;
    ///    and
    /// 5. drops each side while the other still has work outstanding, in **both
    ///    orders**, proving neither state owns the other's buffers.
    #[test]
    fn copy_produces_independent_streams() {
        let payload = varied_payload();
        let comp = zlib_compress(&payload);
        let stop = 2000usize;

        let mut src = ZStream::new();
        assert_eq!(inflate_init2(&mut src, 15), Ok(ReturnCode::Ok));
        let resume = drive_to_midstream(&mut src, &comp, stop);

        // (1) The snapshot point is realistic: dynamic tables and live history.
        {
            let st = src.inflate_state().expect("state installed");
            assert_eq!(
                st.lentable,
                TableSource::Dynamic,
                "the length table must be a dynamic one built into codes[]"
            );
            assert_eq!(st.disttable, TableSource::Dynamic, "distance table dynamic");
            assert!(st.wsize > 0, "the window must be allocated");
            assert!(st.whave > 0, "the window must hold history");
            assert!(
                st.distcode > 0,
                "the distance table must sit past the length table in codes[]"
            );
            assert_ne!(
                st.codes[st.lencode],
                Code::default(),
                "codes[] must hold a built table"
            );
            assert!(st.lenbits > 0 && st.distbits > 0, "table root bits are set");
        }

        let mut cpy = ZStream::new();
        assert_eq!(inflate_copy(&mut cpy, &src), Ok(ReturnCode::Ok));
        assert!(cpy.is_inflate(), "destination received a cloned state");
        assert_eq!(cpy.total_out, src.total_out);
        assert_eq!(cpy.total_in, src.total_in);
        assert_eq!(cpy.adler, src.adler);

        // (2) Field-for-field fidelity, asserted directly rather than inferred
        //     from the decoded output, so a field a future edit forgets to carry
        //     over is caught even when it happens to be reconstructible.
        {
            let a = src.inflate_state().expect("source state");
            let b = cpy.inflate_state().expect("copy state");

            assert_eq!(b.mode, a.mode, "mode");
            assert_eq!(b.last, a.last, "last");
            assert_eq!(b.wrap, a.wrap, "wrap");
            assert_eq!(b.havedict, a.havedict, "havedict");
            assert_eq!(b.flags, a.flags, "flags");
            assert_eq!(b.dmax, a.dmax, "dmax");
            assert_eq!(b.check, a.check, "check");
            assert_eq!(b.total, a.total, "total");
            assert_eq!(b.wbits, a.wbits, "wbits");
            assert_eq!(b.wsize, a.wsize, "wsize");
            assert_eq!(b.whave, a.whave, "whave");
            assert_eq!(b.wnext, a.wnext, "wnext");
            assert_eq!(b.hold, a.hold, "hold");
            assert_eq!(b.bits, a.bits, "bits");
            assert_eq!(b.length, a.length, "length");
            assert_eq!(b.offset, a.offset, "offset");
            assert_eq!(b.extra, a.extra, "extra");
            // The offset-plus-discriminant pair that replaces C's self-referential
            // `lencode`/`distcode`/`next` interior pointers (AAP §0.6.3): getting
            // these wrong is exactly the defect C must hand-patch after its
            // `zmemcpy`, and is what makes a deep clone sound here.
            // `lencode` is structurally 0 in this port — the length table always
            // begins at the base of the `codes` arena, mirroring C's
            // `state->lencode = state->next` while `next == codes` (`inflate.c`
            // L811, L889) — so this assertion is a guard against a future arena
            // layout change rather than a live discriminator. `distcode` below is
            // the non-trivial offset, and it is asserted too.
            assert_eq!(b.lencode, a.lencode, "lencode offset");
            assert_eq!(b.distcode, a.distcode, "distcode offset");
            assert_eq!(b.lentable, a.lentable, "lentable source");
            assert_eq!(b.disttable, a.disttable, "disttable source");
            assert_eq!(b.lenbits, a.lenbits, "lenbits");
            assert_eq!(b.distbits, a.distbits, "distbits");
            assert_eq!(b.ncode, a.ncode, "ncode");
            assert_eq!(b.nlen, a.nlen, "nlen");
            assert_eq!(b.ndist, a.ndist, "ndist");
            assert_eq!(b.have, a.have, "have");
            assert_eq!(b.next, a.next, "next");
            assert_eq!(b.sane, a.sane, "sane");
            assert_eq!(b.back, a.back, "back");
            assert_eq!(b.was, a.was, "was");
            assert_eq!(b.lens, a.lens, "lens");
            assert_eq!(b.work, a.work, "work");
            assert_eq!(b.codes, a.codes, "codes arena");
            assert_eq!(&*b.window, &*a.window, "window contents");
        }

        // (3) Distinct storage: mutating the copy's window and its live decode
        //     table leaves the source untouched.
        {
            let (w0, code, hold) = {
                let a = src.inflate_state().expect("source state");
                (a.window[0], a.codes[a.lencode], a.hold)
            };

            let b = cpy.inflate_state_mut().expect("copy state");
            let lencode = b.lencode;
            b.window[0] = w0 ^ 0xFF;
            b.codes[lencode].val = code.val ^ 0xBEEF;
            b.hold = hold ^ 0xDEAD_BEEF;

            let a = src.inflate_state().expect("source state");
            assert_eq!(a.window[0], w0, "window is shared!");
            assert_eq!(a.codes[a.lencode], code, "codes arena is shared!");
            assert_eq!(a.hold, hold, "bit accumulator is shared!");

            // Restore so step (4) decodes honestly.
            let b = cpy.inflate_state_mut().expect("copy state");
            b.window[0] = w0;
            b.codes[lencode] = code;
            b.hold = hold;
        }

        // (4) Divergent chunking: the source finishes in one call; the copy is fed
        //     11 input bytes at a time into a 13-byte output window. Their
        //     progress therefore diverges, yet both must reconstruct the payload.
        let tail = &payload[stop..];
        let got_src = resume_to_end(&mut src, &comp[resume..], comp.len(), payload.len());
        assert_eq!(
            got_src, tail,
            "the source must decode its remainder exactly"
        );

        let mid_out = cpy.total_out;
        assert_ne!(
            mid_out, src.total_out,
            "the copy must not have advanced with the source"
        );
        assert_eq!(
            mid_out as usize, stop,
            "the copy is still parked at the snapshot point"
        );

        let got_cpy = resume_to_end(&mut cpy, &comp[resume..], 11, 13);
        assert_eq!(
            got_cpy, tail,
            "the copy must decode the same remainder under a different call pattern"
        );
        assert_eq!(
            cpy.total_out, src.total_out,
            "both consumed the whole stream"
        );
        assert_eq!(cpy.adler, src.adler, "both verified the same Adler-32");

        assert_eq!(inflate_end(&mut cpy), Ok(ReturnCode::Ok));
        assert_eq!(inflate_end(&mut src), Ok(ReturnCode::Ok));
        drop(cpy);
        drop(src);

        // (5a) Drop order A — release the SOURCE while the copy still has the
        //      whole remainder outstanding; the copy must finish correctly.
        {
            let mut a = ZStream::new();
            assert_eq!(inflate_init2(&mut a, 15), Ok(ReturnCode::Ok));
            let resume_a = drive_to_midstream(&mut a, &comp, stop);
            let mut b = ZStream::new();
            assert_eq!(inflate_copy(&mut b, &a), Ok(ReturnCode::Ok));
            assert_eq!(inflate_end(&mut a), Ok(ReturnCode::Ok));
            drop(a);
            let got = resume_to_end(&mut b, &comp[resume_a..], 29, 37);
            assert_eq!(got, tail, "the copy outlives its source intact");
            assert_eq!(inflate_end(&mut b), Ok(ReturnCode::Ok));
        }

        // (5b) Drop order B — release the COPY while the source still has the
        //      whole remainder outstanding; the source must finish correctly.
        {
            let mut a = ZStream::new();
            assert_eq!(inflate_init2(&mut a, 15), Ok(ReturnCode::Ok));
            let resume_a = drive_to_midstream(&mut a, &comp, stop);
            let mut b = ZStream::new();
            assert_eq!(inflate_copy(&mut b, &a), Ok(ReturnCode::Ok));
            assert_eq!(inflate_end(&mut b), Ok(ReturnCode::Ok));
            drop(b);
            let got = resume_to_end(&mut a, &comp[resume_a..], 5, 7);
            assert_eq!(got, tail, "the source survives the copy being released");
            assert_eq!(inflate_end(&mut a), Ok(ReturnCode::Ok));
        }
    }

    #[test]
    fn set_dictionary_rejects_bad_state() {
        // No inflate state installed → Z_STREAM_ERROR.
        let mut strm = ZStream::new();
        assert_eq!(
            inflate_set_dictionary(&mut strm, b"dict"),
            Err(ZlibError::StreamError)
        );
    }

    /// Compresses `data` as a gzip member carrying every optional header field
    /// (`FEXTRA`, `FNAME`, `FCOMMENT`, `FHCRC`) using the crate's own encoder,
    /// whose output is byte-identical to reference zlib. Returns the member.
    #[cfg(feature = "gzip")]
    fn gzip_compress_with_full_header(data: &[u8]) -> Vec<u8> {
        use crate::constants::{Strategy, Z_DEFLATED};
        let mut strm = ZStream::new();
        crate::deflate::deflate_init2(&mut strm, 6, Z_DEFLATED, 16 + 15, 8, Strategy::Default)
            .expect("deflate init");
        let mut head = GzHeader::new()
            .with_text(true)
            .with_time(0x5EED_C0DE)
            .with_os(3)
            .with_extra(alloc::vec![0xDE, 0xAD, 0xBE, 0xEF])
            .with_name(b"payload.bin")
            .with_comment(b"a comment");
        // `hcrc` has no builder; setting it makes the encoder emit the optional
        // CRC-16, which the decoder then verifies.
        head.hcrc = true;
        crate::deflate::deflate_set_header(&mut strm, Some(head)).expect("set header");
        let mut out = alloc::vec![0u8; data.len() * 2 + 256];
        let r = crate::deflate::deflate(&mut strm, data, &mut out, Z_FINISH);
        assert_eq!(r.code, ReturnCode::StreamEnd);
        assert_eq!(r.consumed, data.len());
        out.truncate(r.produced);
        crate::deflate::deflate_end(&mut strm).expect("deflate end");
        out
    }

    /// Builds the read-side registration a C caller would supply: empty capture
    /// buffers plus the capacities that bound them.
    #[cfg(feature = "gzip")]
    fn capture_header() -> GzHeader {
        let mut head = GzHeader::new();
        head.extra = Some(Vec::new());
        head.extra_max = 64;
        head.name = Some(Vec::new());
        head.name_max = 64;
        head.comment = Some(Vec::new());
        head.comm_max = 64;
        head
    }

    /// Every gzip header field parsed by `inflate` must be reachable from safe
    /// Rust through [`inflate_header`]. Before this accessor existed the parsed
    /// metadata was structurally unreachable outside the FFI shim, even though
    /// the write side round-tripped fully.
    #[cfg(feature = "gzip")]
    #[test]
    fn inflate_header_exposes_every_parsed_field() {
        let member = gzip_compress_with_full_header(MSG);

        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 16 + 15), Ok(ReturnCode::Ok));
        assert_eq!(
            inflate_get_header(&mut strm, capture_header()),
            Ok(ReturnCode::Ok)
        );

        // Registered but not yet decoded: reachable, and `done` is cleared.
        let pending = inflate_header(&strm).expect("the registration is visible");
        assert!(!pending.done, "`inflate_get_header` clears `done`");

        let mut out = alloc::vec![0u8; 256];
        let outcome = inflate(&mut strm, &member, &mut out, Z_NO_FLUSH);
        assert_eq!(outcome.code, ReturnCode::StreamEnd, "msg {:?}", strm.msg);
        assert_eq!(&out[..outcome.produced], MSG);

        let head = inflate_header(&strm).expect("the header is reachable after decoding");
        assert!(head.done, "`done` marks the whole header consumed");
        assert!(head.text, "the TEXT flag round-trips");
        assert_eq!(head.time, 0x5EED_C0DE, "MTIME round-trips");
        assert_eq!(head.os, 3, "the OS byte round-trips");
        assert_eq!(head.extra.as_deref(), Some(&[0xDEu8, 0xAD, 0xBE, 0xEF][..]));
        // The stream's declared 16-bit `XLEN` is deliberately not a field of
        // `GzHeader`: it is wire-level parser metadata carried in the
        // crate-private `HeaderPublication::extra_len` and published only into a
        // C caller's `gz_header` (see the module documentation of
        // `crate::gz_header`). The idiomatic count of captured bytes is
        // `extra.len()`, and here — with `extra_max` larger than `XLEN` — nothing
        // was clamped, so the two agree.
        assert_eq!(
            head.extra.as_ref().map_or(0, alloc::vec::Vec::len),
            4,
            "the whole declared extra field was captured"
        );
        assert_eq!(head.name.as_deref(), Some(&b"payload.bin"[..]));
        assert_eq!(head.comment.as_deref(), Some(&b"a comment"[..]));
        assert!(
            head.hcrc,
            "FHCRC was set, so the CRC-16 was present and checked"
        );

        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));
    }

    /// [`inflate_take_header`] must transfer ownership out and leave the decoder
    /// with no registration, matching C's `state->head = Z_NULL` semantics.
    #[cfg(feature = "gzip")]
    #[test]
    fn inflate_take_header_transfers_ownership_and_clears_the_registration() {
        let member = gzip_compress_with_full_header(MSG);

        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 16 + 15), Ok(ReturnCode::Ok));
        assert_eq!(
            inflate_get_header(&mut strm, capture_header()),
            Ok(ReturnCode::Ok)
        );
        let mut out = alloc::vec![0u8; 256];
        assert_eq!(
            inflate(&mut strm, &member, &mut out, Z_NO_FLUSH).code,
            ReturnCode::StreamEnd
        );

        let owned = inflate_take_header(&mut strm).expect("ownership is returned");
        assert!(owned.done);
        assert_eq!(owned.name.as_deref(), Some(&b"payload.bin"[..]));
        assert_eq!(owned.comment.as_deref(), Some(&b"a comment"[..]));

        // The registration is gone; both accessors now report nothing.
        assert!(
            inflate_header(&strm).is_none(),
            "the registration is cleared"
        );
        assert!(
            inflate_take_header(&mut strm).is_none(),
            "a second take yields nothing"
        );

        // The owned value outlives the stream it came from.
        assert_eq!(inflate_end(&mut strm), Ok(ReturnCode::Ok));
        drop(strm);
        assert_eq!(owned.name.as_deref(), Some(&b"payload.bin"[..]));
    }

    /// Taking the header must not perturb decoding: the remaining header bytes
    /// are still parsed and the CRC-16 still verified, they are simply recorded
    /// nowhere — exactly what C does for a `Z_NULL` `state->head`.
    #[cfg(feature = "gzip")]
    #[test]
    fn taking_the_header_midstream_leaves_decoding_byte_exact() {
        let member = gzip_compress_with_full_header(MSG);

        // Baseline: decode with no registration at all.
        let mut plain = ZStream::new();
        assert_eq!(inflate_init2(&mut plain, 16 + 15), Ok(ReturnCode::Ok));
        let mut expected = alloc::vec![0u8; 256];
        let base = inflate(&mut plain, &member, &mut expected, Z_NO_FLUSH);
        assert_eq!(base.code, ReturnCode::StreamEnd);
        expected.truncate(base.produced);

        // Register, then immediately revoke the registration before decoding.
        let mut strm = ZStream::new();
        assert_eq!(inflate_init2(&mut strm, 16 + 15), Ok(ReturnCode::Ok));
        assert_eq!(
            inflate_get_header(&mut strm, capture_header()),
            Ok(ReturnCode::Ok)
        );
        assert!(inflate_take_header(&mut strm).is_some());
        let mut out = alloc::vec![0u8; 256];
        let outcome = inflate(&mut strm, &member, &mut out, Z_NO_FLUSH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "an FHCRC header still validates with no registration (msg {:?})",
            strm.msg
        );
        assert_eq!(outcome.consumed, base.consumed);
        assert_eq!(&out[..outcome.produced], expected.as_slice());
        assert!(inflate_header(&strm).is_none(), "nothing was recorded");
    }

    /// Both accessors must report [`None`] for every state in which no gzip
    /// header registration can exist.
    #[cfg(feature = "gzip")]
    #[test]
    fn header_accessors_are_none_without_a_registration() {
        use crate::constants::{Strategy, Z_DEFLATED};

        // (a) No state installed at all.
        let mut bare = ZStream::new();
        assert!(inflate_header(&bare).is_none());
        assert!(inflate_take_header(&mut bare).is_none());

        // (b) A deflate state, not an inflate state.
        let mut enc = ZStream::new();
        crate::deflate::deflate_init2(&mut enc, 6, Z_DEFLATED, 16 + 15, 8, Strategy::Default)
            .expect("deflate init");
        assert!(inflate_header(&enc).is_none());
        assert!(inflate_take_header(&mut enc).is_none());
        crate::deflate::deflate_end(&mut enc).expect("deflate end");

        // (c) An inflate state with nothing registered.
        let mut dec = ZStream::new();
        assert_eq!(inflate_init2(&mut dec, 16 + 15), Ok(ReturnCode::Ok));
        assert!(inflate_header(&dec).is_none());
        assert!(inflate_take_header(&mut dec).is_none());

        // (d) A registration dropped by a reset, mirroring C's
        //     `state->head = Z_NULL` in `inflateResetKeep` (`inflate.c` L115).
        assert_eq!(
            inflate_get_header(&mut dec, capture_header()),
            Ok(ReturnCode::Ok)
        );
        assert!(inflate_header(&dec).is_some());
        assert_eq!(inflate_reset(&mut dec), Ok(ReturnCode::Ok));
        assert!(
            inflate_header(&dec).is_none(),
            "a reset clears the registration exactly as C does"
        );
        assert_eq!(inflate_end(&mut dec), Ok(ReturnCode::Ok));

        // (e) A raw stream cannot register a header in the first place.
        let mut raw = ZStream::new();
        assert_eq!(inflate_init2(&mut raw, -15), Ok(ReturnCode::Ok));
        assert_eq!(
            inflate_get_header(&mut raw, capture_header()),
            Err(ZlibError::StreamError)
        );
        assert!(inflate_header(&raw).is_none());
        assert_eq!(inflate_end(&mut raw), Ok(ReturnCode::Ok));
    }

    // =======================================================================
    // One-call façade — `uncompress` / `uncompress2` against fixed C vectors
    //
    // Relocated here with the entry points themselves: `crate::util::uncompress`
    // is layer 3 and may not name an engine, so the tests that drive the real
    // engine belong in the layer that owns it (AAP §0.3.1, §0.4.2 B2).
    //
    // The streams below were produced by reference zlib (level 6). Using fixed
    // vectors keeps the tests self-contained — they depend on neither the
    // compression side nor any external crate — while still exercising the real
    // decoder end-to-end.
    // =======================================================================

    // -----------------------------------------------------------------------
    // Deterministic zlib streams produced by reference zlib (level 6). Using
    // fixed vectors keeps the tests self-contained — they neither depend on the
    // sibling `compress` module nor on any external crate — while still
    // exercising the real `crate::inflate` engine end-to-end.
    // -----------------------------------------------------------------------

    /// `zlib.compress(b"hello, world")`.
    const HELLO_PLAIN: &[u8] = b"hello, world";
    const HELLO_ZLIB: [u8; 20] = [
        0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0xd7, 0x51, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01,
        0x00, 0x1d, 0x54, 0x04, 0x89,
    ];

    /// `zlib.compress(b"A" * 300)` — highly compressible.
    const BIG_ZLIB: [u8; 13] = [
        0x78, 0x9c, 0x73, 0x74, 0x1c, 0x05, 0xc4, 0x02, 0x00, 0xcb, 0x9e, 0x4c, 0x2d,
    ];
    const BIG_PLAIN_LEN: usize = 300;

    /// `zlib.compress(b"")` — the empty payload (8-byte stream).
    const EMPTY_ZLIB: [u8; 8] = [0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01];

    /// `zlib.compress(bytes(0..64))`.
    const MIXED_ZLIB: [u8; 72] = [
        0x78, 0x9c, 0x63, 0x60, 0x64, 0x62, 0x66, 0x61, 0x65, 0x63, 0xe7, 0xe0, 0xe4, 0xe2, 0xe6,
        0xe1, 0xe5, 0xe3, 0x17, 0x10, 0x14, 0x12, 0x16, 0x11, 0x15, 0x13, 0x97, 0x90, 0x94, 0x92,
        0x96, 0x91, 0x95, 0x93, 0x57, 0x50, 0x54, 0x52, 0x56, 0x51, 0x55, 0x53, 0xd7, 0xd0, 0xd4,
        0xd2, 0xd6, 0xd1, 0xd5, 0xd3, 0x37, 0x30, 0x34, 0x32, 0x36, 0x31, 0x35, 0x33, 0xb7, 0xb0,
        0xb4, 0xb2, 0xb6, 0xb1, 0xb5, 0xb3, 0x07, 0x00, 0xaa, 0xe0, 0x07, 0xe1,
    ];

    /// `zlib.compress(<256 pseudo-random bytes>)` — incompressible, so the
    /// compressed form is *larger* than a small output buffer and filling that
    /// buffer leaves input unconsumed.
    const INCOMP_ZLIB: [u8; 267] = [
        0x78, 0x9c, 0x01, 0x00, 0x01, 0xff, 0xfe, 0xe1, 0x3b, 0x03, 0x2e, 0x11, 0x2a, 0x32, 0xb5,
        0x79, 0x08, 0x0f, 0x08, 0xb1, 0xf7, 0xed, 0x4c, 0x2e, 0x5d, 0x3a, 0x07, 0xf9, 0x7f, 0x21,
        0xee, 0x23, 0x2d, 0x17, 0x8a, 0x20, 0x9a, 0xf6, 0xb5, 0x88, 0x7f, 0x66, 0xe8, 0x09, 0x24,
        0x02, 0xaa, 0x49, 0xf2, 0xc1, 0x55, 0x1b, 0x27, 0xfe, 0x53, 0x26, 0x6e, 0x49, 0x0d, 0xb1,
        0x38, 0x48, 0x9c, 0xe8, 0x14, 0xd5, 0x8d, 0x14, 0x5a, 0x8b, 0x4f, 0x99, 0x4f, 0xed, 0x15,
        0xc5, 0xb2, 0xfd, 0xae, 0xef, 0xf3, 0x17, 0xf1, 0x57, 0xe1, 0xe0, 0x97, 0x8c, 0x3f, 0x5f,
        0xd5, 0xdf, 0x3d, 0x34, 0xf8, 0xc0, 0x82, 0x62, 0xb0, 0x37, 0x50, 0x89, 0x4f, 0xa5, 0xe4,
        0x24, 0x28, 0xca, 0x6d, 0x18, 0x92, 0x13, 0x70, 0x2c, 0xa2, 0x9c, 0xeb, 0x21, 0x83, 0x25,
        0xda, 0x67, 0x33, 0xcb, 0x63, 0xeb, 0x78, 0xb8, 0x69, 0xd7, 0x59, 0x68, 0x9a, 0x1e, 0xb4,
        0x4e, 0xff, 0xf1, 0xaa, 0x47, 0x43, 0x18, 0x54, 0x4a, 0x23, 0xa6, 0x57, 0x00, 0x1f, 0x2c,
        0x4b, 0x6f, 0x14, 0xdd, 0xc8, 0xa6, 0x6a, 0xc3, 0x8f, 0x9b, 0xd8, 0xa3, 0x4d, 0x2f, 0x85,
        0x8e, 0xd2, 0xcc, 0x8d, 0x3a, 0xc0, 0x8c, 0x6d, 0x98, 0xcb, 0x1a, 0xb2, 0xe1, 0x77, 0xfb,
        0x54, 0xc2, 0x9d, 0x01, 0x25, 0xf5, 0xca, 0x98, 0xdb, 0xf5, 0x5f, 0xcd, 0xf4, 0x50, 0x90,
        0xbd, 0xb1, 0x69, 0x56, 0xea, 0xf2, 0x0e, 0xef, 0x35, 0x0d, 0xbb, 0xf3, 0x21, 0x47, 0xa9,
        0xb2, 0x94, 0x98, 0xa9, 0x96, 0x63, 0x8e, 0x25, 0x68, 0xad, 0xab, 0xa4, 0xea, 0x88, 0x2b,
        0x3d, 0x7d, 0x83, 0xbe, 0x46, 0x0e, 0xca, 0x13, 0x16, 0x6a, 0x4f, 0xa0, 0xb5, 0xde, 0x23,
        0x9c, 0x85, 0xf8, 0x70, 0xb2, 0x2a, 0x09, 0xa9, 0x75, 0x53, 0xf4, 0xff, 0x47, 0x22, 0x4a,
        0x7c, 0x54, 0xc9, 0xa7, 0x42, 0xe4, 0x14, 0xbe, 0xeb, 0xaa, 0x7e, 0x17,
    ];
    const INCOMP_PLAIN: [u8; 256] = [
        0xe1, 0x3b, 0x03, 0x2e, 0x11, 0x2a, 0x32, 0xb5, 0x79, 0x08, 0x0f, 0x08, 0xb1, 0xf7, 0xed,
        0x4c, 0x2e, 0x5d, 0x3a, 0x07, 0xf9, 0x7f, 0x21, 0xee, 0x23, 0x2d, 0x17, 0x8a, 0x20, 0x9a,
        0xf6, 0xb5, 0x88, 0x7f, 0x66, 0xe8, 0x09, 0x24, 0x02, 0xaa, 0x49, 0xf2, 0xc1, 0x55, 0x1b,
        0x27, 0xfe, 0x53, 0x26, 0x6e, 0x49, 0x0d, 0xb1, 0x38, 0x48, 0x9c, 0xe8, 0x14, 0xd5, 0x8d,
        0x14, 0x5a, 0x8b, 0x4f, 0x99, 0x4f, 0xed, 0x15, 0xc5, 0xb2, 0xfd, 0xae, 0xef, 0xf3, 0x17,
        0xf1, 0x57, 0xe1, 0xe0, 0x97, 0x8c, 0x3f, 0x5f, 0xd5, 0xdf, 0x3d, 0x34, 0xf8, 0xc0, 0x82,
        0x62, 0xb0, 0x37, 0x50, 0x89, 0x4f, 0xa5, 0xe4, 0x24, 0x28, 0xca, 0x6d, 0x18, 0x92, 0x13,
        0x70, 0x2c, 0xa2, 0x9c, 0xeb, 0x21, 0x83, 0x25, 0xda, 0x67, 0x33, 0xcb, 0x63, 0xeb, 0x78,
        0xb8, 0x69, 0xd7, 0x59, 0x68, 0x9a, 0x1e, 0xb4, 0x4e, 0xff, 0xf1, 0xaa, 0x47, 0x43, 0x18,
        0x54, 0x4a, 0x23, 0xa6, 0x57, 0x00, 0x1f, 0x2c, 0x4b, 0x6f, 0x14, 0xdd, 0xc8, 0xa6, 0x6a,
        0xc3, 0x8f, 0x9b, 0xd8, 0xa3, 0x4d, 0x2f, 0x85, 0x8e, 0xd2, 0xcc, 0x8d, 0x3a, 0xc0, 0x8c,
        0x6d, 0x98, 0xcb, 0x1a, 0xb2, 0xe1, 0x77, 0xfb, 0x54, 0xc2, 0x9d, 0x01, 0x25, 0xf5, 0xca,
        0x98, 0xdb, 0xf5, 0x5f, 0xcd, 0xf4, 0x50, 0x90, 0xbd, 0xb1, 0x69, 0x56, 0xea, 0xf2, 0x0e,
        0xef, 0x35, 0x0d, 0xbb, 0xf3, 0x21, 0x47, 0xa9, 0xb2, 0x94, 0x98, 0xa9, 0x96, 0x63, 0x8e,
        0x25, 0x68, 0xad, 0xab, 0xa4, 0xea, 0x88, 0x2b, 0x3d, 0x7d, 0x83, 0xbe, 0x46, 0x0e, 0xca,
        0x13, 0x16, 0x6a, 0x4f, 0xa0, 0xb5, 0xde, 0x23, 0x9c, 0x85, 0xf8, 0x70, 0xb2, 0x2a, 0x09,
        0xa9, 0x75, 0x53, 0xf4, 0xff, 0x47, 0x22, 0x4a, 0x7c, 0x54, 0xc9, 0xa7, 0x42, 0xe4, 0x14,
        0xbe,
    ];

    #[test]
    fn roundtrip_small() {
        let mut out = [0u8; 32];
        let n = uncompress(&mut out, &HELLO_ZLIB).expect("valid stream decompresses");
        assert_eq!(n, HELLO_PLAIN.len());
        assert_eq!(&out[..n], HELLO_PLAIN);
    }

    #[test]
    fn roundtrip_empty() {
        // An empty payload decodes to zero bytes.
        let mut out = [0u8; 8];
        let n = uncompress(&mut out, &EMPTY_ZLIB).expect("empty stream decompresses");
        assert_eq!(n, 0);
    }

    #[test]
    fn roundtrip_empty_into_zero_length_dest() {
        // The C `next_out == NULL` scratch trick is unnecessary in safe Rust:
        // an empty `&mut []` output is a valid, non-null buffer, and an empty
        // payload needs no output space at all.
        let mut out: [u8; 0] = [];
        let n = uncompress(&mut out, &EMPTY_ZLIB).expect("empty stream, empty dest");
        assert_eq!(n, 0);
    }

    #[test]
    fn roundtrip_highly_compressible() {
        let mut out = [0u8; BIG_PLAIN_LEN];
        let n = uncompress(&mut out, &BIG_ZLIB).expect("valid stream decompresses");
        assert_eq!(n, BIG_PLAIN_LEN);
        assert!(out.iter().all(|&b| b == b'A'), "all bytes are 'A'");
    }

    #[test]
    fn roundtrip_mixed() {
        let mut expected = [0u8; 64];
        for (i, b) in expected.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut out = [0u8; 64];
        let n = uncompress(&mut out, &MIXED_ZLIB).expect("valid stream decompresses");
        assert_eq!(n, expected.len());
        assert_eq!(&out[..n], &expected[..]);
    }

    #[test]
    fn roundtrip_incompressible() {
        let mut out = [0u8; 256];
        let n = uncompress(&mut out, &INCOMP_ZLIB).expect("valid stream decompresses");
        assert_eq!(n, INCOMP_PLAIN.len());
        assert_eq!(&out[..n], &INCOMP_PLAIN[..]);
    }

    #[test]
    fn uncompress2_reports_consumed_length() {
        let mut out = [0u8; 32];
        let mut consumed = HELLO_ZLIB.len();
        let mut produced = out.len();
        let n = uncompress2(&mut out, &HELLO_ZLIB, &mut consumed, &mut produced).expect("ok");
        assert_eq!(n, HELLO_PLAIN.len());
        assert_eq!(consumed, HELLO_ZLIB.len(), "the entire stream is consumed");
        assert_eq!(
            produced,
            HELLO_PLAIN.len(),
            "the produced-count out-parameter matches the return value"
        );
    }

    #[test]
    fn uncompress2_consumes_only_the_stream_with_trailing_bytes() {
        // A valid stream followed by trailing junk: only the stream bytes are
        // consumed, and `*source_len` reports exactly that count.
        let mut buf = [0u8; 40];
        buf[..HELLO_ZLIB.len()].copy_from_slice(&HELLO_ZLIB);
        let mut out = [0u8; 32];
        let mut consumed = buf.len();
        let mut produced = out.len();
        let n = uncompress2(&mut out, &buf, &mut consumed, &mut produced).expect("ok");
        assert_eq!(n, HELLO_PLAIN.len());
        assert_eq!(
            consumed,
            HELLO_ZLIB.len(),
            "trailing bytes are not consumed"
        );
        assert_eq!(produced, HELLO_PLAIN.len(), "produced count is reported");
        assert_eq!(&out[..n], HELLO_PLAIN);
    }

    #[test]
    fn truncated_input_is_data_error() {
        // Feed only a prefix that cuts into the deflate data. The engine
        // consumes all of it yet never reaches stream end (leftover_in == 0),
        // which `uncompr.c` L80-81 maps to Z_DATA_ERROR.
        let mut out = [0u8; BIG_PLAIN_LEN];
        let err = uncompress(&mut out, &BIG_ZLIB[..7]).unwrap_err();
        assert_eq!(err, ReturnCode::DataError);
    }

    #[test]
    fn dest_too_small_is_buf_error() {
        // Full, valid, incompressible input but a far-too-small output buffer:
        // output fills while input still remains (leftover_in > 0), which falls
        // through to Z_BUF_ERROR.
        let mut out = [0u8; 64];
        let err = uncompress(&mut out, &INCOMP_ZLIB).unwrap_err();
        assert_eq!(err, ReturnCode::BufError);
    }

    #[test]
    fn corrupt_input_is_data_error() {
        let mut corrupt = HELLO_ZLIB; // `[u8; 20]` is `Copy`.
        corrupt[8] ^= 0xff; // Damage a deflate-data byte.
        let mut out = [0u8; 32];
        let err = uncompress(&mut out, &corrupt).unwrap_err();
        assert_eq!(err, ReturnCode::DataError);
    }

    #[test]
    fn zero_declared_source_len_is_data_error() {
        // Declaring zero available input means nothing can be decoded; with all
        // (zero) input "consumed" this is the truncated-stream branch.
        let mut out = [0u8; 32];
        let mut consumed = 0usize;
        let mut produced = out.len();
        let err = uncompress2(&mut out, &HELLO_ZLIB, &mut consumed, &mut produced).unwrap_err();
        assert_eq!(err, ReturnCode::DataError);
        assert_eq!(consumed, 0);
        assert_eq!(produced, 0, "no output is produced on this error path");
    }

    #[test]
    fn source_len_cap_is_honored() {
        // Capping the declared input below the stream length prevents the
        // engine from ever finishing, and the cap is never exceeded.
        let mut out = [0u8; 32];
        let mut consumed = 6usize; // Fewer than the 20-byte stream.
        let mut produced = out.len();
        let result = uncompress2(&mut out, &HELLO_ZLIB, &mut consumed, &mut produced);
        assert!(result.is_err(), "a capped, incomplete stream cannot finish");
        assert!(consumed <= 6, "never read past the declared cap");
        assert!(
            produced <= out.len(),
            "produced count stays within the buffer"
        );
    }
}
