//! Raw-callback back-inflate (`inflateBack` / `inflateBackInit` /
//! `inflateBackEnd`) — safe Rust port of `infback.c`. Uses caller callbacks for
//! I/O and a caller/owned window; raw DEFLATE only.
//!
//! Unlike the main `inflate()` driver (which pushes decoded bytes into a
//! caller-supplied `next_out` buffer), [`inflate_back`] pulls its input and
//! pushes its output through **caller-supplied callbacks** ([`InFunc`] /
//! [`OutFunc`]), decoding a single *raw* DEFLATE stream (RFC 1951 — no zlib or
//! gzip wrapper) using a single sliding window that doubles as the output
//! buffer. This is the high-throughput entry point used by consumers such as
//! `gzip -d`'s core loop.
//!
//! The decoded output is **byte-identical** to reference zlib for the same
//! input (AAP §0.6.4, §0.8.1 directive D-1): the DEFLATE bit reader, the block dispatch, the
//! dynamic Huffman table construction (shared with the main driver via
//! [`crate::inflate::tables::inflate_table`]), the fixed tables
//! ([`crate::inflate::fixed`]), and — crucially — the overlapping LZ77 window
//! copy are reproduced exactly.
//!
//! # Unsafe policy
//!
//! This module contains **zero** `unsafe` (AAP §0.6.2). All window and input
//! access uses bounds-checked slice indexing. The raw C function-pointer
//! callbacks (`in_func` / `out_func` in `zlib.h`) are *not* modelled here;
//! wrapping those into the safe [`InFunc`] / [`OutFunc`] traits is the job of
//! the FFI boundary (`src/ffi/inflate.rs`).
//!
//! # The two decode paths, and why the batched one is a specialization
//!
//! Reference `infback.c` L422-L428 hands the symbol loop to the shared
//! `inflate_fast` hot loop whenever at least six input bytes and 258 output
//! bytes are available, and uses the single-symbol loop (`infback.c` L430-L543)
//! only when they are not. **Both** paths are ported here — `back_fast` is the
//! batched one, `do_len` the per-symbol one — and the same
//! `have >= 6 && left >= 258` test selects between them, so the crossover point
//! is C's.
//!
//! [`crate::inflate::fast::inflate_fast`] itself cannot be *reused*, because it
//! is written for the main driver, where the output buffer and the history
//! window are **distinct** allocations: it takes the output as `&mut [u8]` and
//! the window as `&[u8]`, two separate borrows. In back-inflate the window *is*
//! the output buffer, so `back_fast` is the single-buffer specialization of
//! the same algorithm. That specialization is markedly *simpler* than the
//! general routine rather than harder, because two invariants collapse:
//!
//! * `wnext` is `0` for the entire session — `infback.c` L60 sets it and nothing
//!   in the file ever moves it — so only the "very common case" arm of
//!   `inffast.c` L197-L206 is reachable. The two window-wrap arms are dead code
//!   in back-inflate.
//! * `beg` — the earliest output byte a back-reference may reach — is the window
//!   base. `infback.c` L425 calls `inflate_fast(strm, state->wsize)` with
//!   `avail_out == left`, and `put + left == wsize` always holds, so C's
//!   `beg = out - (start - avail_out)` evaluates to `put - put`. C's "max
//!   distance in output", `out - beg`, is therefore just `put`.
//!
//! What the shared buffer *does* demand is that every copy be a single move
//! inside a single slice, including the ones that read bytes the same copy is
//! writing. `forward_copy` is that primitive: it reproduces C's forward
//! `do { *put++ = *from++; } while (--n);` for **any** source/destination
//! relationship, block-at-a-time instead of byte-at-a-time. Both paths route
//! every window and match copy through it, so the batched path, the per-symbol
//! path, and C agree byte-for-byte by construction rather than by coincidence.
//!
//! # `no_std`
//!
//! The implementation is `no_std` + `alloc`: it uses [`alloc::boxed::Box`] for
//! the state and never references `std`. Nothing else is heap-allocated — the
//! decode path holds no owned buffer at all, matching `infback.c`, which
//! allocates only in `inflateBackInit_`. The I/O callbacks are expressed as
//! traits so `no_std` callers can supply their own.

use alloc::boxed::Box;

use crate::constants::MAX_WBITS;
use crate::error::ReturnCode;
use crate::inflate::fixed::{DISTFIX, LENFIX};
use crate::inflate::state::{InflateMode, InflateState, TableSource};
use crate::inflate::tables::{Code, CodeType, inflate_table};
use crate::stream::{
    AllocBuffer, AllocHook, Allocator, BoxedEngine, EngineReservation, HookAllocator, try_box,
};

/// Permutation of the 19 code-length code lengths, as read from a dynamic
/// block header. Transcribed verbatim from `infback.c` L205-L206 (identical to
/// the `order[]` used by the main `inflate()` driver).
const ORDER: [u16; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Minimum permitted `windowBits` for `inflateBack` (raw DEFLATE only).
///
/// Reference `infback.c` L34 rejects anything below `8`.
const MIN_WBITS: i32 = 8;

/// Returns a mask selecting the low `n` bits (`(1 << n) - 1`), i.e. the C
/// `BITS(n)` mask.
///
/// `n` must be strictly less than 32; every caller in this module passes a
/// value derived from a DEFLATE code length (`<= 15`) or a small literal
/// header width, so the shift never overflows.
#[inline]
const fn low_mask(n: u32) -> u32 {
    (1u32 << n) - 1
}

/// Input bytes that must be available before the batched decode path is
/// entered.
///
/// C `infback.c` L424 spells this `have >= 6`. Six is what lets
/// [`back_fast`] refill the accumulator with unchecked-in-C reads for one whole
/// literal/length + distance pair without re-testing the input bound, which is
/// the entire reason the batched path is faster than the per-symbol one.
const BACK_FAST_MIN_INPUT: usize = 6;

/// Free window bytes that must be available before the batched decode path is
/// entered.
///
/// C `infback.c` L424 spells this `left >= 258`; 258 is the longest match a
/// single DEFLATE length code can encode, so one iteration can never overrun the
/// window.
const BACK_FAST_MIN_OUTPUT: usize = 258;

/// Copies `n` bytes inside `buf` from `src` to `dst` in **increasing byte
/// order**, reproducing C's `do { *put++ = *from++; } while (--n);` for any
/// relationship between `src` and `dst`.
///
/// This is the one copy primitive of this module. Back-inflate decodes into the
/// sliding window itself, so a match copy, a ring-wrapped window read and an
/// RLE run are all moves *within a single slice* — and two of those three cases
/// read bytes that the very same copy is writing. Getting that right, and doing
/// it block-at-a-time rather than byte-at-a-time, is what makes the batched path
/// worthwhile at all.
///
/// # Why one `copy_within` is not enough
///
/// [`slice::copy_within`] has `memmove` semantics: it behaves as if the source
/// were snapshotted first, so it always reproduces the *pre-existing* bytes.
/// That is right for two of the three cases and wrong for the third:
///
/// * `src == dst` — the C loop copies every byte onto itself. A no-op.
/// * `src > dst` — the write cursor trails the read cursor. At step `i` the C
///   loop reads `src + i`, which is not written until step `src + i - dst > i`,
///   so every read sees an original byte. That is exactly `memmove`, so a single
///   [`slice::copy_within`] is byte-for-byte equivalent no matter how far the
///   two ranges overlap. This is the ring-wrap arm (`from = put + copy` in
///   `infback.c` L529, `from = window + wsize - op` in `inffast.c` L199).
/// * `src < dst` — a back-reference. Once `dist = dst - src` is smaller than
///   `n`, byte `dst + dist` must read the byte just written at `dst`: the run is
///   *periodic* with period `dist` and `memmove` would emit the wrong bytes.
///
/// # The periodic arm
///
/// The `src < dst` arm uses the same strategy as
/// `crate::inflate::fast`'s `copy_within_output`, whose doc comment carries the
/// full argument; in brief:
///
/// * `dist == 1` collapses to `fill` (a `memset`) — the RLE case.
/// * otherwise, with `copied` bytes already written, the bytes in
///   `buf[src .. dst + copied]` are all final and number `dist + copied`, so
///   copying `chunk = min(dist + copied, n - copied)` bytes from `src` to
///   `dst + copied` reads only final bytes (the ranges are disjoint, so
///   `copy_within` is exact) and lands on the correct periodic continuation
///   because `copied` is a multiple of `dist` on every pass that is not the last
///   one. The block size doubles each pass, so at most `log2(n / dist) + 1`
///   moves are issued.
///
/// # Panics
///
/// Panics if `src + n` or `dst + n` exceeds `buf.len()`; both callers derive
/// their indices from the window geometry, which bounds every copy by `wsize`.
#[inline]
fn forward_copy(buf: &mut [u8], dst: usize, src: usize, n: usize) {
    if n == 0 || src == dst {
        // Copying a byte onto itself is what the C loop does here, so there is
        // nothing to move. The `n == 0` guard also keeps `fill`/`copy_within`
        // off an empty range.
        return;
    }

    if src > dst {
        // Reads run ahead of writes at every step, so `memmove` semantics are
        // exactly the C byte loop even when the ranges overlap.
        buf.copy_within(src..src + n, dst);
        return;
    }

    let dist = dst - src;
    if dist == 1 {
        // RLE run: every byte equals `buf[src]`.
        let b = buf[src];
        buf[dst..dst + n].fill(b);
    } else {
        // One pass when `dist >= n` (disjoint ranges); geometric growth
        // otherwise.
        let mut copied = 0usize;
        while copied < n {
            let chunk = core::cmp::min(dist + copied, n - copied);
            buf.copy_within(src..src + chunk, dst + copied);
            copied += chunk;
        }
    }
}

/// Provides input bytes to [`inflate_back`], replacing the C `in_func`
/// callback.
///
/// The C prototype is
/// `typedef unsigned (*in_func)(void FAR *, z_const unsigned char FAR * FAR *)`:
/// it hands back a pointer to a run of input bytes and the count available,
/// returning `0` to signal end-of-input (or an input error).
///
/// The safe Rust equivalent splits the C callback into two halves so that the
/// engine can read *directly out of the provider's own memory* rather than
/// copying each chunk into a buffer of its own:
///
/// * [`advance`](InFunc::advance) performs the C call — it makes the next chunk
///   current and reports whether one was obtained. `false` signals end-of-input
///   (or an input error) exactly as a `0` count does in C, in which case
///   [`inflate_back`] stops and returns [`ReturnCode::BufError`].
/// * [`chunk`](InFunc::chunk) borrows the chunk made current by the most recent
///   successful `advance`.
///
/// Splitting them is what makes the borrow legal: a single
/// `fn next(&mut self) -> &[u8]` ties the returned slice to a `&mut self`
/// borrow, so the engine could not hold the slice while calling any other
/// method. With the pair, the engine holds only a `&self` borrow for the length
/// of one expression, and the whole decoder therefore performs **zero
/// decode-time allocation** — matching `infback.c`, which allocates nothing
/// once `inflateBackInit_` has returned.
///
/// A current chunk is fully consumed before `advance` is called again, mirroring
/// the C contract that "the application must not change the provided input until
/// `in()` is called again". Before the first `advance`, and after one that
/// returned `false`, `chunk` must report a slice whose length still equals the
/// number of bytes the engine has consumed from it — returning the previous
/// chunk, or an empty slice, both satisfy that.
pub trait InFunc {
    /// Makes the next chunk of input current, returning `false` at
    /// end-of-input.
    ///
    /// Returning `true` while [`chunk`](InFunc::chunk) is empty is treated as
    /// end-of-input too, so an implementation cannot stall the decoder.
    fn advance(&mut self) -> bool;

    /// Borrows the chunk made current by the most recent successful
    /// [`advance`](InFunc::advance).
    fn chunk(&self) -> &[u8];

    /// Reports, once at the end of [`inflate_back`], how many bytes of the most
    /// recently yielded chunk were left unconsumed by the engine.
    ///
    /// This mirrors C `infback.c`'s `inf_leave` writing `strm->avail_in = have`
    /// (and `next` at the unconsumed offset): an FFI adapter overriding this can
    /// restore the C `z_stream`'s `next_in`/`avail_in` cursors to point at the
    /// unconsumed tail of the last provider buffer. The default is a no-op, so
    /// pure-Rust callers that do not track raw cursors are unaffected.
    fn set_unconsumed(&mut self, _unconsumed: usize) {}
}

/// What a finished [`inflate_back`] call leaves in the caller's diagnostic slot,
/// i.e. the fate of C's `strm->msg`.
///
/// C keeps the diagnostic on the `z_stream`, which this engine does not have, so
/// the three reachable outcomes are reported explicitly instead. The distinction
/// is not cosmetic: `infback.c` returns `Z_STREAM_ERROR` for an uninitialised
/// stream at L209-L210, which is *before* the `strm->msg = Z_NULL` at L214, so a
/// rejected call must leave whatever diagnostic the caller already had in place.
/// Collapsing that into "always clear" would erase a message C preserves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackMsg {
    /// The call was rejected before C reaches `strm->msg = Z_NULL`
    /// (`infback.c` L209-L210): any existing diagnostic must be left alone.
    Untouched,
    /// C cleared `strm->msg` (`infback.c` L214) and no error site assigned a new
    /// one — the outcome for a clean decode and for a `Z_BUF_ERROR` exit alike,
    /// neither of which sets a diagnostic in C.
    Cleared,
    /// C cleared `strm->msg` and then assigned this diagnostic at one of the
    /// twelve `infback.c` error sites. The string is byte-identical to C's.
    Set(&'static str),
}

/// The complete result of one [`inflate_back`] call: the C return code together
/// with the fate of `strm->msg`.
///
/// Returned as a value rather than written through a stream because
/// [`inflate_back`] takes only an [`InflateState`]; an FFI shim publishes both
/// halves onto the caller's `z_stream`, and a pure-Rust caller can ignore
/// [`msg`](BackOutcome::msg) entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackOutcome {
    /// The code C's `inflateBack` would have returned.
    pub code: ReturnCode,
    /// What C would have left in `strm->msg`.
    pub msg: BackMsg,
}

/// Consumes output bytes produced by [`inflate_back`], replacing the C
/// `out_func` callback.
///
/// The C prototype is
/// `typedef int (*out_func)(void FAR *, unsigned char FAR *, unsigned)`: it is
/// handed a pointer and a length and returns `0` on success or non-zero to
/// abort.
///
/// The safe Rust equivalent receives the produced bytes as a slice and returns
/// `Ok(())` on success or `Err(())` to abort (the non-zero C return). On
/// `Err(())`, [`inflate_back`] stops and returns [`ReturnCode::BufError`],
/// matching the C behavior when `out()` fails.
///
/// The unit error type (`Result<(), ()>`) is intentional: it mirrors the C
/// `out_func` contract exactly (zero for success, non-zero to abort) without
/// inventing a richer error the callback cannot supply. Hence
/// `clippy::result_unit_err` is allowed here.
#[allow(clippy::result_unit_err)]
pub trait OutFunc {
    /// Consumes `buf`; returns `Err(())` to abort decoding.
    fn write_output(&mut self, buf: &[u8]) -> Result<(), ()>;
}

/// The mutable working registers of a single [`inflate_back`] call.
///
/// This bundles the parts of the C `inflateBack` local state that live in
/// registers between the C `LOAD`/`RESTORE` macros — the bit accumulator
/// (`hold`/`bits`), the current input chunk and cursor (`next`/`have`), and the
/// output-window cursor (`put`/`left`) — together with the two I/O callbacks.
///
/// The current input chunk is **not** copied: it is read straight out of the
/// provider through [`InFunc::chunk`], so this type owns no input buffer and the
/// decode path performs no allocation whatsoever — exactly like `infback.c`,
/// which allocates only in `inflateBackInit_`. Only a cursor is kept; the
/// two-method [`InFunc`] split is what keeps that borrow legal in safe Rust.
///
/// It also carries the pending diagnostic. C writes `strm->msg` in place at each
/// error site; there is no `z_stream` here, so the string is accumulated on this
/// per-call value and reported through [`BackOutcome`]. Keeping it per-call
/// rather than on [`InflateState`] makes a stale diagnostic unrepresentable.
struct BackCtx<'a, I: InFunc, O: OutFunc> {
    /// Read cursor into [`InFunc::chunk`]; `chunk().len() - next` is the C
    /// `have`.
    next: usize,
    /// The diagnostic C would have stored in `strm->msg`, set alongside every
    /// transition to [`InflateMode::Bad`] and [`None`] until one occurs.
    msg: Option<&'static str>,
    /// Input bit accumulator (C `hold`; only the low 32 bits are ever used).
    hold: u32,
    /// Number of valid bits currently in [`hold`](BackCtx::hold) (C `bits`).
    bits: u32,
    /// Write cursor into the window/output buffer (offset of C `put` from the
    /// window base).
    put: usize,
    /// Remaining free space in the window from [`put`](BackCtx::put) to the end
    /// (C `left`). The invariant `put + left == wsize` always holds.
    left: usize,
    /// Window size in bytes (`1 << windowBits`); a cached copy of
    /// [`InflateState::wsize`].
    wsize: usize,
    /// Input provider (C `in` / `in_desc`).
    src: &'a mut I,
    /// Output sink (C `out` / `out_desc`).
    sink: &'a mut O,
}

impl<I: InFunc, O: OutFunc> BackCtx<'_, I, O> {
    /// C `PULL()`: assure at least one input byte is available, refilling from
    /// the [`InFunc`] when the current chunk is exhausted.
    ///
    /// Returns `Err(ReturnCode::BufError)` when the provider yields no more
    /// input, matching the C macro's `goto inf_leave` with `ret = Z_BUF_ERROR`.
    #[inline]
    fn pull(&mut self) -> Result<(), ReturnCode> {
        if self.next == self.src.chunk().len() {
            // `advance` performs C's `in(in_desc, &next)` call; an empty result
            // is end-of-input either way, so a provider that answers `true`
            // while offering nothing cannot stall the decoder.
            if !self.src.advance() || self.src.chunk().is_empty() {
                return Err(ReturnCode::BufError);
            }
            self.next = 0;
        }
        Ok(())
    }

    /// C `have`: the number of bytes left unconsumed in the current chunk.
    ///
    /// Saturating rather than plain subtraction because a provider whose
    /// `advance` returned `false` is permitted to shrink or empty its reported
    /// chunk; the cursor then sits at or past the end and C's `have` is `0`.
    #[inline]
    fn have(&self) -> usize {
        self.src.chunk().len().saturating_sub(self.next)
    }

    /// C's `strm->msg = (char *)"…"; state->mode = BAD;` pair, kept together so
    /// no error site can set one without the other.
    #[inline]
    fn bad(&mut self, state: &mut InflateState, msg: &'static str) {
        state.mode = InflateMode::Bad;
        self.msg = Some(msg);
    }

    /// C `PULLBYTE()`: pull one input byte into the bit accumulator.
    #[inline]
    fn pull_byte(&mut self) -> Result<(), ReturnCode> {
        self.pull()?;
        // The low `bits` positions are occupied; the new byte's 8 bits sit
        // above them, so `|` is exactly the C `+=` here (no carry, no overflow:
        // `bits < 32` at every call site, so the shift is well-defined).
        let byte = self.src.chunk()[self.next];
        self.hold |= u32::from(byte) << self.bits;
        self.next += 1;
        self.bits += 8;
        Ok(())
    }

    /// C `NEEDBITS(n)`: assure at least `n` bits are in the accumulator.
    #[inline]
    fn need_bits(&mut self, n: u32) -> Result<(), ReturnCode> {
        while self.bits < n {
            self.pull_byte()?;
        }
        Ok(())
    }

    /// C `BITS(n)`: return the low `n` bits of the accumulator.
    #[inline]
    fn bits_val(&self, n: u32) -> u32 {
        self.hold & low_mask(n)
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

    /// C `ROOM()`: assure some output space by flushing the window when it is
    /// full.
    ///
    /// When the window is full ([`left`](BackCtx::left) is `0`) the whole window
    /// is handed to the [`OutFunc`], `whave` is set to the full window size, and
    /// the cursor is reset to the window base. Returns
    /// `Err(ReturnCode::BufError)` if the sink aborts, matching the C macro.
    #[inline]
    fn room(&mut self, window: &mut [u8], whave: &mut u32) -> Result<(), ReturnCode> {
        if self.left == 0 {
            self.put = 0;
            self.left = self.wsize;
            *whave = self.wsize as u32;
            if self.sink.write_output(&window[..self.wsize]).is_err() {
                return Err(ReturnCode::BufError);
            }
        }
        Ok(())
    }
}

impl<I: InFunc, O: OutFunc> BackCtx<'_, I, O> {
    /// Decode one Huffman symbol using a two-level decode table, returning the
    /// resolved leaf [`Code`].
    ///
    /// This is the shared decode used for both literal/length codes and
    /// distance codes in the per-symbol path (a faithful port of the two nested
    /// loops in `infback.c` L432-L447 and L486-L501). `table` is the code table
    /// (either a module-static fixed table or a sub-slice of
    /// [`InflateState::codes`]) and `root_bits` is its root index width
    /// (`state.lenbits` or `state.distbits`).
    ///
    /// The first-level loop assures just enough bits for the root code without
    /// over-reading; a second level is walked only for a table-link entry
    /// (`op != 0 && op & 0xf0 == 0`). Bits for the resolved code (and any parent
    /// table-link bits) are consumed before returning, exactly as the C
    /// `DROPBITS` sequence does.
    #[inline]
    fn decode_symbol(&mut self, table: &[Code], root_bits: u32) -> Result<Code, ReturnCode> {
        // First-level lookup: pull bytes until the indexed code fits in `bits`.
        let mut here: Code;
        loop {
            here = table[self.bits_val(root_bits) as usize];
            if u32::from(here.bits) <= self.bits {
                break;
            }
            self.pull_byte()?;
        }

        // Second-level lookup for a table-link entry (op is a nonzero index
        // width with no high-nibble flags set). A literal (op == 0) or a
        // length/distance/EOB/invalid leaf (op & 0xf0 != 0) is already resolved.
        if here.op != 0 && (here.op & 0xf0) == 0 {
            let last = here;
            loop {
                let idx = last.val as usize
                    + (self.bits_val(u32::from(last.bits) + u32::from(last.op)) >> last.bits)
                        as usize;
                here = table[idx];
                if u32::from(last.bits) + u32::from(here.bits) <= self.bits {
                    break;
                }
                self.pull_byte()?;
            }
            self.drop_bits(u32::from(last.bits));
        }
        self.drop_bits(u32::from(here.bits));
        Ok(here)
    }
}

/// Initialize an [`InflateState`] for raw-callback back-inflate, allocating the
/// owned sliding window/output buffer.
///
/// Port of `inflateBackInit_` (`infback.c` L25-L64). `inflateBack` supports
/// **raw** DEFLATE only, so `window_bits` must be in `8..=15`
/// (`MIN_WBITS`..=[`MAX_WBITS`]); any other value
/// yields [`ReturnCode::StreamError`], matching the C `Z_STREAM_ERROR` return.
///
/// On success the returned boxed state owns a window of `1 << window_bits`
/// bytes (the AAP §0.6.3 owned-buffer model — the C API's caller-supplied
/// `window` argument is replaced by this owned allocation), with `dmax` set to
/// `32768`, `sane` enabled, and the window-geometry fields initialized. The
/// state is created with `wrap = 0` (raw framing). The block-decode `mode` is
/// intentionally *not* set here; [`inflate_back`] sets it to
/// [`InflateMode::Type`] on entry, exactly as the C code does.
///
/// # Errors
///
/// Returns [`ReturnCode::StreamError`] if `window_bits` is outside `8..=15`.
///
/// # Examples
///
/// ```
/// # use zlib_rs::inflate::back::inflate_back_init;
/// let state = inflate_back_init(15).expect("15 is a valid windowBits");
/// assert_eq!(state.wsize, 1 << 15);
/// assert!(inflate_back_init(7).is_err());
/// assert!(inflate_back_init(16).is_err());
/// ```
#[inline]
pub fn inflate_back_init(window_bits: i32) -> Result<Box<InflateState>, ReturnCode> {
    inflate_back_init_in(AllocHook::none(), window_bits)
}

/// Same as [`inflate_back_init`] but threads an explicit [`AllocHook`] so the
/// owned back-inflate window is allocated through the caller's `zalloc`/`zfree`
/// when one was installed via the FFI `z_stream` (AAP §0.6.3 has-hook clause).
/// In back-inflate the window *is* the output buffer, so unlike
/// the streaming `inflate` path this allocation is eager (performed here rather
/// than lazily in `updatewindow`); routing it through the hook means the
/// output buffer honors a caller-supplied allocator exactly as reference zlib's
/// `ZALLOC(strm, ...)` does.
///
/// # Errors
///
/// * [`ReturnCode::StreamError`] — `window_bits` is outside `8..=15`.
/// * [`ReturnCode::MemError`] — the state or the window could not be allocated.
#[inline]
pub fn inflate_back_init_in(
    hook: AllocHook,
    window_bits: i32,
) -> Result<Box<InflateState>, ReturnCode> {
    inflate_back_init_with(&HookAllocator::new(hook), window_bits)
}

/// Same as [`inflate_back_init_in`] but takes an [`Allocator`] rather than a bare
/// [`AllocHook`], so the state footprint and the window are requested from it.
///
/// This is the spelling any caller with a custom Rust allocator should use: a
/// bare hook only ever selects between the caller's `zalloc` and the global
/// allocator, whereas an [`Allocator`] can serve the memory itself (AAP §0.6.3).
///
/// # Allocator schedule
///
/// Exactly **one** request to `alloc`, immediately after the `window_bits` range
/// check: the window, `(1 << window_bits, 1)`. That is the same *count* reference
/// zlib makes and at the same point in the sequence — C's one request is for the
/// state (`ZALLOC(strm, 1, sizeof(struct inflate_state))`, `infback.c` L51) and
/// its window comes from the caller, whereas here the window is owned and the
/// state is placed on the Rust heap. A bounded allocator therefore refuses at the
/// same position either way.
///
/// Substituting the window for the state is this Rust-native convenience
/// constructor's documented divergence: its signature hands back an owning
/// [`Box`], which cannot address a caller's region. The FFI `inflateBackInit_`
/// shim has no such constraint and does it C's way — the ABI caller's buffer is
/// lent rather than allocated, and the state itself is charged to the caller's
/// `zalloc` — through the crate-private `inflate_back_init_borrowed_window`.
///
/// # Errors
///
/// As [`inflate_back_init_in`].
pub fn inflate_back_init_with<A: Allocator>(
    alloc: &A,
    window_bits: i32,
) -> Result<Box<InflateState>, ReturnCode> {
    // C L33-L35: reject windowBits outside the raw range 8..=15. Checked before
    // any allocation, exactly as C does.
    if !(MIN_WBITS..=MAX_WBITS).contains(&window_bits) {
        return Err(ReturnCode::StreamError);
    }
    // In back-inflate the window *is* the output buffer, so unlike the streaming
    // `inflate` path this allocation is eager. Routing it through `alloc` means it
    // honors a caller-supplied allocator rather than silently using the global
    // heap (AAP §0.6.3 has-hook clause).
    let window = alloc
        .allocate_zeroed::<u8>(1usize << window_bits)
        .ok_or(ReturnCode::MemError)?;
    // Boxing is fallible so global-heap exhaustion becomes `Z_MEM_ERROR` — the
    // code C returns when its state `ZALLOC` fails (`infback.c` L52-L53) — rather
    // than an abort.
    try_box(build_back_state(alloc.hook(), window_bits, || {
        Some(window)
    })?)
    .ok_or(ReturnCode::MemError)
}

/// Builds a back-inflate state around an **already-provided** window buffer,
/// making the single state-footprint request C makes and no other.
///
/// This is the entry point the FFI `inflateBackInit_` shim uses. Reference zlib's
/// `inflateBackInit_` allocates exactly one region — the state — and then adopts
/// the caller's buffer verbatim:
///
/// ```text
/// state = ZALLOC(strm, 1, sizeof(struct inflate_state));  /* infback.c L51 */
/// if (state == Z_NULL) return Z_MEM_ERROR;
/// ...
/// state->window = window;                                 /* infback.c L59 */
/// ```
///
/// Passing the caller's region in as `window` (wrapped by the boundary's borrowed
/// -buffer bridge, whose `Drop` is a no-op) reproduces that exactly: one
/// allocator request, no copy, and `inflateBackEnd` leaves the caller's memory
/// alone just as `infback.c` L572-L577 does.
///
/// `window` must hold at least `1 << window_bits` bytes; a shorter buffer is
/// rejected with [`ReturnCode::StreamError`] rather than silently truncating the
/// history the decoder may reach back into.
///
/// # Errors
///
/// * [`ReturnCode::StreamError`] — `window_bits` is outside `8..=15`, or `window`
///   is smaller than `1 << window_bits`.
/// * [`ReturnCode::MemError`] — the state allocation was refused, or the global
///   heap could not hold the state on the no-hook path.
pub(crate) fn inflate_back_init_borrowed_window<A, F>(
    alloc: &A,
    window_bits: i32,
    lend_window: F,
) -> Result<BoxedEngine<InflateState>, ReturnCode>
where
    A: Allocator,
    F: FnOnce() -> Option<AllocBuffer<u8>>,
{
    // C L33-L35: reject windowBits outside the raw range 8..=15. Before any
    // allocation, exactly as C does.
    if !(MIN_WBITS..=MAX_WBITS).contains(&window_bits) {
        return Err(ReturnCode::StreamError);
    }

    // C L51-L53: the state is charged to the caller's allocator and checked
    // immediately. This reservation *is* that request, carrying C's own
    // `(1, sizeof(struct inflate_state))` pair, and the region is handed back
    // through their `zfree` at `inflateBackEnd` (AAP §0.6.5). Whether to charge at
    // all is the allocator's decision
    // (`Allocator::reserves_state_footprint`); the global default declines,
    // because there the `Box` already is the allocation.
    //
    // It is taken before `lend_window` runs, so a refused request returns having
    // touched nothing at all — precisely what C does: the `ZALLOC` at L51-L53
    // fails and returns `Z_MEM_ERROR` before L60's `state->window = window;` ever
    // runs. Reservation and construction are two steps because the charge has to
    // happen before the state value exists.
    let reservation = EngineReservation::<InflateState>::take(alloc).ok_or(ReturnCode::MemError)?;

    let state = build_back_state(alloc.hook(), window_bits, lend_window)?;

    // Filling boxes the state through a checked global allocation, so heap
    // exhaustion becomes `Z_MEM_ERROR` — the code C returns when its state
    // `ZALLOC` fails (`infback.c` L52-L53) — rather than an abort. The caller's
    // charge was already secured above and is never re-requested here.
    reservation.fill(state).ok_or(ReturnCode::MemError)
}

/// Builds the back-inflate state **value**, with the window adopted but no
/// placement decision made.
///
/// Split out of [`inflate_back_init_borrowed_window`] so the hook-backed FFI path
/// and the owning Rust-native path
/// ([`inflate_back_init_with`]) share one definition of what an
/// `inflateBack` state *is*, while differing — as their signatures force them to —
/// in where it ends up living.
///
/// `lend_window` is a closure rather than a value so a caller that must charge an
/// allocator first can return without ever naming the window, which is what lets
/// [`inflate_back_init_borrowed_window`] leave an ABI caller's buffer untouched on
/// the path C leaves untouched.
///
/// # Errors
///
/// * [`ReturnCode::StreamError`] — `window_bits` is outside `8..=15`, or the lent
///   buffer is shorter than `1 << window_bits`.
/// * [`ReturnCode::MemError`] — `lend_window` reported failure.
fn build_back_state<F>(
    hook: AllocHook,
    window_bits: i32,
    lend_window: F,
) -> Result<InflateState, ReturnCode>
where
    F: FnOnce() -> Option<AllocBuffer<u8>>,
{
    // C L33-L35, re-asserted here because this is also reached directly: reject
    // windowBits outside the raw range 8..=15.
    if !(MIN_WBITS..=MAX_WBITS).contains(&window_bits) {
        return Err(ReturnCode::StreamError);
    }
    let wsize = 1usize << window_bits;

    let window = lend_window().ok_or(ReturnCode::MemError)?;
    if window.len() < wsize {
        return Err(ReturnCode::StreamError);
    }

    // Raw stream: wrap = 0. `build_in` already sets dmax = 32768, sane = true, and
    // records `hook` so any later window handling routes through it.
    let mut state = InflateState::build_in(hook, 0, window_bits as u32);

    // C L56-L62: window geometry, then adopt the window.
    state.dmax = 32768;
    state.wbits = window_bits as u32;
    state.wsize = wsize as u32;
    state.whave = 0;
    state.wnext = 0;
    state.sane = true;
    state.window = window;

    Ok(state)
}

/// Handle the `TYPE` state: read a block header and dispatch on the block type.
///
/// Port of `infback.c` L228-L260. Reads the final-block flag and the 2-bit
/// block type, then transitions to [`InflateMode::Stored`], the fixed-code
/// [`InflateMode::Len`] path (configuring the module-static fixed tables, as
/// C's `inflate_fixed` does: `lenbits = 9`, `distbits = 5`),
/// [`InflateMode::Table`] (dynamic codes), or [`InflateMode::Bad`] for the
/// reserved block type `3`. When the previous block was final, byte-aligns and
/// transitions to [`InflateMode::Done`].
///
/// Returns `Err(ReturnCode::BufError)` only if the input callback is exhausted
/// while reading the header.
fn do_type<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &mut InflateState,
) -> Result<(), ReturnCode> {
    if state.last {
        // C L230-L233: at the last block, discard padding to a byte boundary
        // and finish.
        ctx.byte_align();
        state.mode = InflateMode::Done;
        return Ok(());
    }

    ctx.need_bits(3)?;
    state.last = ctx.bits_val(1) != 0;
    ctx.drop_bits(1);
    match ctx.bits_val(2) {
        0 => state.mode = InflateMode::Stored,
        1 => {
            // Fixed Huffman block: point at the module-static fixed tables with
            // their canonical root widths (C `inflate_fixed`).
            state.lentable = TableSource::Fixed;
            state.lenbits = 9;
            state.disttable = TableSource::Fixed;
            state.distbits = 5;
            state.mode = InflateMode::Len;
        }
        2 => state.mode = InflateMode::Table,
        // C L255-L257: reserved block type 3 -> "invalid block type".
        _ => ctx.bad(state, "invalid block type"),
    }
    // C L259: consume the type bits in all cases (including BAD).
    ctx.drop_bits(2);
    Ok(())
}

/// Handle the `STORED` state: copy an uncompressed (stored) block straight from
/// input to the window/output.
///
/// Port of `infback.c` L262-L292. Byte-aligns, reads the 16-bit length and its
/// one's-complement check (transitioning to [`InflateMode::Bad`] on mismatch),
/// then streams `length` bytes through the input ([`BackCtx::pull`]) and output
/// ([`BackCtx::room`]) callbacks. Returns to [`InflateMode::Type`] on
/// completion.
///
/// # Errors
///
/// Returns `Err(ReturnCode::BufError)` if the input or output callback fails
/// mid-copy.
fn do_stored<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &mut InflateState,
) -> Result<(), ReturnCode> {
    // C L264-L266: align to a byte boundary and read LEN/NLEN (32 bits).
    ctx.byte_align();
    ctx.need_bits(32)?;
    if (ctx.hold & 0xffff) != ((ctx.hold >> 16) ^ 0xffff) {
        // C L266-L269: LEN and ~NLEN disagree.
        ctx.bad(state, "invalid stored block lengths");
        return Ok(());
    }
    let mut length = ctx.hold & 0xffff;
    ctx.init_bits();

    // C L277-L289: copy `length` stored bytes input -> window/output.
    while length != 0 {
        let mut copy = length as usize;
        ctx.pull()?;
        ctx.room(&mut state.window, &mut state.whave)?;
        let have = ctx.have();
        if copy > have {
            copy = have;
        }
        if copy > ctx.left {
            copy = ctx.left;
        }
        state.window[ctx.put..ctx.put + copy]
            .copy_from_slice(&ctx.src.chunk()[ctx.next..ctx.next + copy]);
        ctx.next += copy;
        ctx.left -= copy;
        ctx.put += copy;
        length -= copy as u32;
    }

    state.mode = InflateMode::Type;
    Ok(())
}

/// Handle the `TABLE` state: read a dynamic block's Huffman code descriptor and
/// build the literal/length and distance decode tables.
///
/// Port of `infback.c` L294-L419. Reads the `HLIT`/`HDIST`/`HCLEN` counts,
/// builds the code-length code table, decodes the literal/length and distance
/// code lengths (including the `16`/`17`/`18` repeat codes), and finally builds
/// the two decode tables via [`inflate_table`] into
/// [`InflateState::codes`]. On success transitions to [`InflateMode::Len`];
/// every malformed-input branch transitions to [`InflateMode::Bad`].
///
/// The code-length codes are decoded with an inline **single-level** lookup
/// that defers `DROPBITS` until after the repeat's extra bits have been
/// requested — this differs from the two-level [`BackCtx::decode_symbol`] used
/// for literal/length and distance symbols, and mirrors the C control flow
/// exactly.
///
/// # Errors
///
/// Returns `Err(ReturnCode::BufError)` if the input callback is exhausted while
/// reading the descriptor or code lengths.
fn do_table<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &mut InflateState,
) -> Result<(), ReturnCode> {
    // C L296-L302: read HLIT (nlen), HDIST (ndist) and HCLEN (ncode).
    ctx.need_bits(14)?;
    let nlen = (ctx.bits_val(5) + 257) as usize;
    ctx.drop_bits(5);
    let ndist = (ctx.bits_val(5) + 1) as usize;
    ctx.drop_bits(5);
    let ncode = (ctx.bits_val(4) + 4) as usize;
    ctx.drop_bits(4);

    // C L303-L309: bound the symbol counts.
    if nlen > 286 || ndist > 30 {
        ctx.bad(state, "too many length or distance symbols");
        return Ok(());
    }

    // C L313-L321: read the `ncode` code-length code lengths in `ORDER`
    // permutation, zeroing the remainder of the 19-entry alphabet.
    for &ord in ORDER.iter().take(ncode) {
        ctx.need_bits(3)?;
        state.lens[ord as usize] = ctx.bits_val(3) as u16;
        ctx.drop_bits(3);
    }
    for &ord in ORDER.iter().skip(ncode) {
        state.lens[ord as usize] = 0;
    }

    // C L322-L331: build the code-length code table at codes[0..].
    let mut code_index = 0usize;
    let mut code_bits: usize = 7;
    if inflate_table(
        CodeType::Codes,
        &state.lens,
        19,
        &mut state.codes,
        &mut code_index,
        &mut code_bits,
        &mut state.work,
    )
    .is_err()
    {
        ctx.bad(state, "invalid code lengths set");
        return Ok(());
    }

    // C L334-L383: decode the nlen + ndist literal/length and distance code
    // lengths, expanding the 16/17/18 repeat codes.
    let total = nlen + ndist;
    let mut n = 0usize;
    while n < total {
        // Inline single-level decode of one code-length code (defers DROPBITS).
        let mut here: Code;
        loop {
            let idx = ctx.bits_val(code_bits as u32) as usize;
            here = state.codes[idx];
            if u32::from(here.bits) <= ctx.bits {
                break;
            }
            ctx.pull_byte()?;
        }

        if here.val < 16 {
            // C L342-L345: a literal code length.
            ctx.drop_bits(u32::from(here.bits));
            state.lens[n] = here.val;
            n += 1;
        } else {
            // C L346-L382: a repeat code (16/17/18).
            let repeat_val: u16;
            let mut copy: usize;
            if here.val == 16 {
                // Repeat the previous length 3..6 times.
                ctx.need_bits(u32::from(here.bits) + 2)?;
                ctx.drop_bits(u32::from(here.bits));
                if n == 0 {
                    // C L350-L354: nothing to repeat.
                    ctx.bad(state, "invalid bit length repeat");
                    return Ok(());
                }
                repeat_val = state.lens[n - 1];
                copy = 3 + ctx.bits_val(2) as usize;
                ctx.drop_bits(2);
            } else if here.val == 17 {
                // Repeat a zero length 3..10 times.
                ctx.need_bits(u32::from(here.bits) + 3)?;
                ctx.drop_bits(u32::from(here.bits));
                repeat_val = 0;
                copy = 3 + ctx.bits_val(3) as usize;
                ctx.drop_bits(3);
            } else {
                // Repeat a zero length 11..138 times.
                ctx.need_bits(u32::from(here.bits) + 7)?;
                ctx.drop_bits(u32::from(here.bits));
                repeat_val = 0;
                copy = 11 + ctx.bits_val(7) as usize;
                ctx.drop_bits(7);
            }
            // C L374-L379: a repeat must not overrun the declared code counts.
            if n + copy > total {
                ctx.bad(state, "invalid bit length repeat");
                return Ok(());
            }
            while copy != 0 {
                state.lens[n] = repeat_val;
                n += 1;
                copy -= 1;
            }
        }
    }

    // C L388-L394: a valid block must define an end-of-block code.
    if state.lens[256] == 0 {
        ctx.bad(state, "invalid code -- missing end-of-block");
        return Ok(());
    }

    // C L399-L408: build the literal/length table at codes[0..]. Do not change
    // the root width (9) without revisiting the ENOUGH sizing in tables.rs.
    state.lencode = 0;
    state.lentable = TableSource::Dynamic;
    let mut table_index = 0usize;
    let mut lenbits: usize = 9;
    if inflate_table(
        CodeType::Lens,
        &state.lens,
        nlen,
        &mut state.codes,
        &mut table_index,
        &mut lenbits,
        &mut state.work,
    )
    .is_err()
    {
        ctx.bad(state, "invalid literal/lengths set");
        return Ok(());
    }
    state.lenbits = lenbits as u32;

    // C L409-L417: build the distance table immediately after the length table
    // (root width 6). `table_index` is now the arena cursor past the length
    // table, so it becomes the distance table's base offset.
    state.distcode = table_index;
    state.disttable = TableSource::Dynamic;
    let mut distbits: usize = 6;
    if inflate_table(
        CodeType::Dists,
        &state.lens[nlen..],
        ndist,
        &mut state.codes,
        &mut table_index,
        &mut distbits,
        &mut state.work,
    )
    .is_err()
    {
        ctx.bad(state, "invalid distances set");
        return Ok(());
    }
    state.distbits = distbits as u32;

    // C L418-L420: tables built; fall through to symbol decoding.
    state.mode = InflateMode::Len;
    Ok(())
}

/// Decode literal/length and distance symbols in one batch, returning when
/// fewer than six input bytes or 258 free window bytes remain, at end-of-block,
/// or on a malformed code.
///
/// Port of `inflate_fast` (`inffast.c` L50-L305) specialized for back-inflate,
/// entered from [`do_len`] under exactly C's `have >= 6 && left >= 258` test
/// (`infback.c` L422-L428). See the module header for why this is a
/// specialization of [`crate::inflate::fast::inflate_fast`] rather than a call
/// to it, and for the two invariants (`wnext == 0`, `beg == window base`) that
/// make the single-buffer form simpler than the general one.
///
/// # What it does *not* do
///
/// It never flushes. C's `inflate_fast` has no access to `infback.c`'s `ROOM()`
/// macro either: it is called only when 258 bytes are already free and it stops
/// while at least 257 still are, leaving every flush to the per-symbol path. It
/// likewise never touches `whave`, which only `ROOM()` advances.
///
/// # State transitions
///
/// Leaves `state.mode` at [`InflateMode::Len`] when it simply runs out of input
/// or window space (the caller re-dispatches and the per-symbol path picks up
/// where this left off), sets it to [`InflateMode::Type`] on end-of-block, and to
/// [`InflateMode::Bad`] — together with C's exact diagnostic on
/// [`BackCtx::msg`] — for an invalid code or an out-of-range distance. There is
/// no error return: a callback is never invoked from here, so every outcome is a
/// mode.
///
/// # Returning unused input bytes
///
/// C L295-L300 ends with `len = bits >> 3; in -= len; bits -= len << 3;`, handing
/// back whole bytes that are still sitting in the accumulator so the per-symbol
/// path can re-read them. `in_idx` is an index into the *current provider chunk*,
/// so "before the chunk" is not expressible; the rewind is therefore clamped at
/// index zero and any bits that cannot be given back stay in `hold`, which keeps
/// the bitstream synchronised either way. The clamp can only bite when the
/// accumulator straddles a chunk boundary — for a single-chunk provider (every
/// FFI caller of `inflateBack`, and every one-shot Rust caller) `in_idx` counts
/// from the buffer start and the rewind is byte-for-byte C's.
fn back_fast<I: InFunc, O: OutFunc>(ctx: &mut BackCtx<'_, I, O>, state: &mut InflateState) {
    // ---- entry contract (mirrors C's implicit one at infback.c L424) --------
    debug_assert_eq!(
        state.mode,
        InflateMode::Len,
        "back_fast entry: state.mode must be Len"
    );
    debug_assert!(
        ctx.have() >= BACK_FAST_MIN_INPUT,
        "back_fast entry: at least {BACK_FAST_MIN_INPUT} input bytes required"
    );
    debug_assert!(
        ctx.left >= BACK_FAST_MIN_OUTPUT,
        "back_fast entry: at least {BACK_FAST_MIN_OUTPUT} free window bytes required"
    );
    debug_assert_eq!(
        ctx.put + ctx.left,
        ctx.wsize,
        "back_fast entry: the put/left window invariant must hold"
    );
    debug_assert_eq!(
        state.wnext, 0,
        "back_fast entry: back-inflate never advances wnext (infback.c L60)"
    );
    debug_assert!(
        state.sane,
        "back_fast entry: back-inflate never clears sane (infback.c L62)"
    );
    debug_assert!(
        ctx.bits <= 32,
        "back_fast entry: bits must fit the u32 hold"
    );

    // ---- LOAD: pull everything the loop touches into locals (C L77-L97) -----
    let wsize = ctx.wsize;
    let whave = state.whave as usize;
    let lmask = low_mask(state.lenbits);
    let dmask = low_mask(state.distbits);
    // Consulted only by the optional INFLATE_STRICT check, so it is read under
    // the same gate to avoid a dead load in a default build.
    #[cfg(feature = "inflate_strict")]
    let dmax = state.dmax as usize;

    let mut hold = ctx.hold;
    let mut bits = ctx.bits;
    let mut put = ctx.put;
    let mut in_idx = ctx.next;

    // The two decode tables and the window are reached through *disjoint field*
    // borrows of `state`: the tables read `state.codes` (or a module-static fixed
    // table) while `window` mutably borrows `state.window`. `state.mode` is
    // therefore only written after both borrows have ended, below.
    let lcode: &[Code] = match state.lentable {
        TableSource::Fixed => &LENFIX[..],
        TableSource::Dynamic => &state.codes[state.lencode..],
    };
    let dcode: &[Code] = match state.disttable {
        TableSource::Fixed => &DISTFIX[..],
        TableSource::Dynamic => &state.codes[state.distcode..],
    };
    let window: &mut [u8] = &mut state.window;

    // The provider's chunk is borrowed for the length of the loop, exactly as
    // the per-symbol path borrows it one expression at a time.
    let input: &[u8] = ctx.src.chunk();

    // Loop bounds. C `last = in + (have - 5)` and `end = out + (left - 257)`:
    // while `in_idx < last_safe_in` at least six input bytes remain, and while
    // `put < end_safe` at least 258 free window bytes remain. `end_safe` is
    // derived from `wsize`, not from `window.len()`, because the window buffer is
    // permitted to be longer than the logical window.
    let last_safe_in = input.len() - (BACK_FAST_MIN_INPUT - 1);
    let end_safe = wsize - (BACK_FAST_MIN_OUTPUT - 1);

    // Applied to `state.mode` / `ctx.msg` after the loop. `None` means "leave the
    // mode alone", which is C's behavior when the loop merely runs out of input
    // or output space.
    let mut final_mode: Option<InflateMode> = None;
    let mut error_msg: Option<&'static str> = None;

    // ---- main decode loop (C `do { ... } while (in < last && out < end)`) ----
    'outer: loop {
        // Refill to at least 15 bits (C L100-L105). The loop bound guarantees
        // both reads are in range.
        if bits < 15 {
            hold += u32::from(input[in_idx]) << bits;
            in_idx += 1;
            bits += 8;
            hold += u32::from(input[in_idx]) << bits;
            in_idx += 1;
            bits += 8;
        }

        let mut here: Code = lcode[(hold & lmask) as usize];
        let mut len: usize = 0;
        let mut go_dodist = false;

        // C label `dolen` (L107-L280).
        'dolen: loop {
            let code_bits = u32::from(here.bits);
            hold >>= code_bits;
            bits -= code_bits;

            let op = u32::from(here.op);
            if op == 0 {
                // Literal byte straight into the window (C L112-L117).
                window[put] = here.val as u8;
                put += 1;
                break 'dolen;
            } else if op & 16 != 0 {
                // Length base plus extra bits (C L118-L131).
                len = here.val as usize;
                let extra = op & 15;
                if extra != 0 {
                    if bits < extra {
                        hold += u32::from(input[in_idx]) << bits;
                        in_idx += 1;
                        bits += 8;
                    }
                    len += (hold & low_mask(extra)) as usize;
                    hold >>= extra;
                    bits -= extra;
                }
                // Refill for the distance code that must follow (C L133-L138).
                if bits < 15 {
                    hold += u32::from(input[in_idx]) << bits;
                    in_idx += 1;
                    bits += 8;
                    hold += u32::from(input[in_idx]) << bits;
                    in_idx += 1;
                    bits += 8;
                }
                here = dcode[(hold & dmask) as usize];
                go_dodist = true;
                break 'dolen;
            } else if op & 64 == 0 {
                // Second-level length table (C L270-L273).
                here = lcode[here.val as usize + (hold & low_mask(op)) as usize];
                continue 'dolen;
            } else if op & 32 != 0 {
                // End of block (C L274-L277).
                final_mode = Some(InflateMode::Type);
                break 'outer;
            } else {
                // Invalid literal/length code (C L278-L281).
                error_msg = Some("invalid literal/length code");
                final_mode = Some(InflateMode::Bad);
                break 'outer;
            }
        }

        if go_dodist {
            // C label `dodist` (L139-L268).
            'dodist: loop {
                let code_bits = u32::from(here.bits);
                hold >>= code_bits;
                bits -= code_bits;

                let op = u32::from(here.op);
                if op & 16 != 0 {
                    // Distance base plus extra bits (C L144-L155).
                    let mut dist = here.val as usize;
                    let extra = op & 15;
                    if bits < extra {
                        hold += u32::from(input[in_idx]) << bits;
                        in_idx += 1;
                        bits += 8;
                        if bits < extra {
                            hold += u32::from(input[in_idx]) << bits;
                            in_idx += 1;
                            bits += 8;
                        }
                    }
                    dist += (hold & low_mask(extra)) as usize;

                    // C `#ifdef INFLATE_STRICT` (L156-L163) — off by default, so
                    // a byte-exact default build does not compile this in.
                    #[cfg(feature = "inflate_strict")]
                    {
                        if dist > dmax {
                            error_msg = Some("invalid distance too far back");
                            final_mode = Some(InflateMode::Bad);
                            break 'outer;
                        }
                    }

                    hold >>= extra;
                    bits -= extra;

                    // C L167-L168: `op = out - beg`, the largest distance the
                    // freshly written output alone can satisfy. `beg` is the
                    // window base here, so this is simply `put`.
                    if dist > put {
                        // The reference reaches back into the history that the
                        // window already flushed.
                        let dist_back = dist - put;

                        // C L170-L177. `sane` is deliberately not consulted:
                        // back-inflate never clears it (asserted on entry), and
                        // the `!sane` zero-fill arm is C's
                        // INFLATE_ALLOW_INVALID_DISTANCE_TOOFAR_ARRR path, which
                        // this crate does not port. Rejecting unconditionally is
                        // also what keeps `wsize - dist_back` below in range:
                        // passing the test implies `dist_back <= whave <= wsize`.
                        if dist_back > whave {
                            error_msg = Some("invalid distance too far back");
                            final_mode = Some(InflateMode::Bad);
                            break 'outer;
                        }

                        // C L197-L206, the `wnext == 0` "very common case" — the
                        // only reachable arm in back-inflate. Valid history ends
                        // at `wsize`, so the reference starts `dist_back` bytes
                        // before the window end.
                        let wpos = wsize - dist_back;
                        if dist_back < len {
                            // Some from the window tail, the rest from what this
                            // very copy is producing (C `from = out - dist`).
                            forward_copy(window, put, wpos, dist_back);
                            put += dist_back;
                            len -= dist_back;
                            let src = put - dist;
                            debug_assert_eq!(
                                src, 0,
                                "the window-tail remainder always resumes at the window base"
                            );
                            forward_copy(window, put, src, len);
                            put += len;
                        } else {
                            forward_copy(window, put, wpos, len);
                            put += len;
                        }
                    } else {
                        // Wholly inside the current fill (C L246-L258). This is
                        // the arm that overlaps whenever `dist < len` — the
                        // essence of LZ77 — which `forward_copy` reproduces.
                        let src = put - dist;
                        forward_copy(window, put, src, len);
                        put += len;
                    }
                    break 'dodist;
                } else if op & 64 == 0 {
                    // Second-level distance table (C L260-L263).
                    here = dcode[here.val as usize + (hold & low_mask(op)) as usize];
                    continue 'dodist;
                } else {
                    // Invalid distance code (C L264-L267).
                    error_msg = Some("invalid distance code");
                    final_mode = Some(InflateMode::Bad);
                    break 'outer;
                }
            }
        }

        // C `} while (in < last && out < end);`.
        if !(in_idx < last_safe_in && put < end_safe) {
            break 'outer;
        }
    }

    // ---- return unused bytes still in the accumulator (C L295-L300) ---------
    let owed = (bits >> 3) as usize;
    let give = owed.min(in_idx);
    in_idx -= give;
    bits -= (give as u32) << 3;
    // A completely full accumulator (`bits == 32`) makes `1u32 << bits`
    // overflow, so the all-ones mask is produced without shifting.
    hold &= 1u32.checked_shl(bits).unwrap_or(0).wrapping_sub(1);

    // ---- RESTORE: C's LOAD() at infback.c L427 ------------------------------
    ctx.hold = hold;
    ctx.bits = bits;
    ctx.next = in_idx;
    ctx.put = put;
    ctx.left = wsize - put;
    if let Some(msg) = error_msg {
        ctx.msg = Some(msg);
    }
    if let Some(mode) = final_mode {
        state.mode = mode;
    }
}

/// Handle the `LEN` state: decode one literal/length symbol and, for a length
/// code, its distance and the resulting match copy.
///
/// Port of `infback.c` L422-L543. C's L423-L428 hands whole batches of symbols
/// to `inflate_fast` whenever six input and 258 free window bytes are available;
/// that test is the first thing this function performs, and [`back_fast`] is the
/// batched path it selects. Everything below it is the single-symbol path C falls
/// back to (L430-L543), which is also the only path that can flush the window: a
/// literal is emitted to the window/output; an end-of-block returns to
/// [`InflateMode::Type`]; a length/distance pair copies the match from the window
/// with correct ring-wrap and overlapping-copy (RLE) semantics. Malformed codes
/// transition to [`InflateMode::Bad`].
///
/// # Errors
///
/// Returns `Err(ReturnCode::BufError)` if the input or output callback fails
/// while decoding or emitting.
fn do_len<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &mut InflateState,
) -> Result<(), ReturnCode> {
    // C L423-L428: use the batched path while there is enough input to decode a
    // whole symbol pair without re-checking, and enough window space for the
    // longest possible match. C returns to the mode dispatch afterwards
    // (`break` out of the `switch`), which is what returning here does: the mode
    // is unchanged unless the batch ended the block or found a bad code.
    if ctx.have() >= BACK_FAST_MIN_INPUT && ctx.left >= BACK_FAST_MIN_OUTPUT {
        back_fast(ctx, state);
        return Ok(());
    }

    // C L432-L448: decode a literal/length code (two-level lookup).
    let lenbits = state.lenbits;
    let here = {
        let len_table: &[Code] = match state.lentable {
            TableSource::Fixed => &LENFIX[..],
            TableSource::Dynamic => &state.codes[state.lencode..],
        };
        ctx.decode_symbol(len_table, lenbits)?
    };

    // C L450-L459: a literal byte.
    if here.op == 0 {
        ctx.room(&mut state.window, &mut state.whave)?;
        state.window[ctx.put] = here.val as u8;
        ctx.put += 1;
        ctx.left -= 1;
        state.mode = InflateMode::Len;
        return Ok(());
    }

    // C L462-L466: end of block.
    if here.op & 32 != 0 {
        state.mode = InflateMode::Type;
        return Ok(());
    }

    // C L469-L473: invalid literal/length code.
    if here.op & 64 != 0 {
        ctx.bad(state, "invalid literal/length code");
        return Ok(());
    }

    // C L476-L482: length base plus any extra bits.
    let mut length = here.val as usize;
    let extra = u32::from(here.op) & 15;
    if extra != 0 {
        ctx.need_bits(extra)?;
        length += ctx.bits_val(extra) as usize;
        ctx.drop_bits(extra);
    }

    // C L485-L506: decode a distance code (two-level lookup).
    let distbits = state.distbits;
    let dhere = {
        let dist_table: &[Code] = match state.disttable {
            TableSource::Fixed => &DISTFIX[..],
            TableSource::Dynamic => &state.codes[state.distcode..],
        };
        ctx.decode_symbol(dist_table, distbits)?
    };
    if dhere.op & 64 != 0 {
        // C L502-L505: invalid distance code.
        ctx.bad(state, "invalid distance code");
        return Ok(());
    }
    let mut offset = dhere.val as usize;

    // C L509-L515: distance base plus any extra bits.
    let dextra = u32::from(dhere.op) & 15;
    if dextra != 0 {
        ctx.need_bits(dextra)?;
        offset += ctx.bits_val(dextra) as usize;
        ctx.drop_bits(dextra);
    }

    // C L516-L521: reject a distance that reaches past the available history.
    // NOTE: `infback` does *not* consult `state.sane` here (unlike the main
    // `inflate` driver); this follows the C source verbatim.
    let reach = if state.whave < ctx.wsize as u32 {
        ctx.left
    } else {
        0
    };
    if offset > ctx.wsize - reach {
        ctx.bad(state, "invalid distance too far back");
        return Ok(());
    }

    // C L524-L542: copy the match from the window to the output. The window is
    // a ring, so a copy may wrap; overlapping copies (offset < length) are
    // performed byte-by-byte to reproduce the LZ77/RLE semantics exactly.
    loop {
        ctx.room(&mut state.window, &mut state.whave)?;
        let mut copy = ctx.wsize - offset;
        let from;
        if copy < ctx.left {
            // The source wraps around to the tail of the window.
            from = ctx.put + copy;
            copy = ctx.left - copy;
        } else {
            // The source is a direct back-reference within the current fill.
            from = ctx.put - offset;
            copy = ctx.left;
        }
        if copy > length {
            copy = length;
        }
        length -= copy;
        ctx.left -= copy;
        // In increasing byte order so an overlapping copy (offset < run length)
        // propagates like C's `*put++ = *from++` (RLE). `forward_copy` moves
        // whole blocks while emitting exactly those bytes; a bare `copy_within`
        // would be wrong for the back-reference arm.
        forward_copy(&mut state.window, ctx.put, from, copy);
        ctx.put += copy;
        if length == 0 {
            break;
        }
    }

    state.mode = InflateMode::Len;
    Ok(())
}

/// Flush any output still buffered in the window at loop exit (C `inf_leave`,
/// `infback.c` L561-L569).
///
/// When the window is partially filled (`left < wsize`) the pending bytes
/// `window[0..wsize - left]` are handed to the [`OutFunc`]. If that final write
/// fails and decoding had otherwise succeeded ([`ReturnCode::StreamEnd`]), the
/// result is downgraded to [`ReturnCode::BufError`], matching the C behavior.
fn inf_leave<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &InflateState,
    ret: ReturnCode,
) -> ReturnCode {
    if ctx.left < ctx.wsize {
        let n = ctx.wsize - ctx.left;
        if ctx.sink.write_output(&state.window[..n]).is_err() && ret == ReturnCode::StreamEnd {
            return ReturnCode::BufError;
        }
    }
    ret
}

/// Decode a single **raw** DEFLATE stream, pulling input from `in_func` and
/// pushing output to `out_func` through the state's sliding/output window.
///
/// That window is *owned* by the state for anything built by
/// [`inflate_back_init`] and its allocator-aware siblings; it is backed by
/// caller-supplied storage only for states produced by the crate-private
/// borrowed-window constructor, which exists solely for the FFI bridge (C's
/// `inflateBackInit_` takes the window from its caller).
///
/// Port of `inflateBack` (`infback.c` L191-L570). The `state` must have been
/// produced by [`inflate_back_init`]. Decoding runs to completion in this one
/// call: the [`InFunc`] is invoked whenever more input is needed and the
/// [`OutFunc`] whenever the window fills or at end-of-stream. Only raw DEFLATE
/// (RFC 1951) is handled — there is no zlib or gzip wrapper.
///
/// # Returns
///
/// A [`BackOutcome`] carrying both halves of what C leaves behind: the return
/// code in [`code`](BackOutcome::code) and the fate of `strm->msg` in
/// [`msg`](BackOutcome::msg). The codes are
///
/// - [`ReturnCode::StreamEnd`] on a successfully decoded final block
///   ([`BackMsg::Cleared`]).
/// - [`ReturnCode::DataError`] on a DEFLATE format error (an invalid block
///   type, bad stored-block lengths, a malformed dynamic header, an invalid
///   code, or a distance that reaches too far back) — always
///   [`BackMsg::Set`] with C's exact diagnostic.
/// - [`ReturnCode::BufError`] if the input callback runs dry or the output
///   callback aborts ([`BackMsg::Cleared`]: C assigns no diagnostic on this
///   path).
/// - [`ReturnCode::StreamError`] if `state` is not a valid, window-sized
///   back-inflate state ([`BackMsg::Untouched`], because C returns before it
///   clears the field).
///
/// # Examples
///
/// ```
/// # use zlib_rs::inflate::back::{
/// #     inflate_back, inflate_back_init, BackMsg, InFunc, OutFunc,
/// # };
/// # use zlib_rs::error::ReturnCode;
/// // Raw DEFLATE for the empty stream: a final fixed block containing only the
/// // end-of-block code.
/// struct OneShot<'a> {
///     pending: Option<&'a [u8]>,
///     current: &'a [u8],
/// }
/// impl<'a> InFunc for OneShot<'a> {
///     fn advance(&mut self) -> bool {
///         match self.pending.take() {
///             Some(chunk) => {
///                 self.current = chunk;
///                 true
///             }
///             None => false,
///         }
///     }
///     fn chunk(&self) -> &[u8] {
///         self.current
///     }
/// }
/// struct Collect(Vec<u8>);
/// impl OutFunc for Collect {
///     fn write_output(&mut self, buf: &[u8]) -> Result<(), ()> {
///         self.0.extend_from_slice(buf);
///         Ok(())
///     }
/// }
/// let mut state = inflate_back_init(15).unwrap();
/// let data = [0x03u8, 0x00];
/// let mut src = OneShot { pending: Some(&data), current: &[] };
/// let mut sink = Collect(Vec::new());
/// let outcome = inflate_back(&mut state, &mut src, &mut sink);
/// assert_eq!(outcome.code, ReturnCode::StreamEnd);
/// assert_eq!(outcome.msg, BackMsg::Cleared);
/// assert!(sink.0.is_empty());
/// ```
pub fn inflate_back<I: InFunc, O: OutFunc>(
    state: &mut InflateState,
    in_func: &mut I,
    out_func: &mut O,
) -> BackOutcome {
    // C L208-L211: the state must be initialized (via inflate_back_init), which
    // means a valid mode and an allocated, correctly sized window. This returns
    // *before* C's `strm->msg = Z_NULL` at L214, so the caller's existing
    // diagnostic is deliberately left alone.
    let wsize = state.wsize as usize;
    if !state.is_valid() || wsize == 0 || state.window.len() < wsize {
        return BackOutcome {
            code: ReturnCode::StreamError,
            msg: BackMsg::Untouched,
        };
    }

    // C L213-L223: reset per-call state and load the registers. C L214 clears
    // `strm->msg` here, which `BackCtx::msg` starting at `None` reproduces: a
    // decode that sets no diagnostic reports `BackMsg::Cleared` below.
    state.mode = InflateMode::Type;
    state.last = false;
    state.whave = 0;

    let mut ctx = BackCtx {
        next: 0,
        msg: None,
        hold: 0,
        bits: 0,
        put: 0,
        left: wsize,
        wsize,
        src: in_func,
        sink: out_func,
    };

    // C L225-L560: run the block/symbol state machine.
    let ret: ReturnCode = drive(&mut ctx, state);

    // Report the unconsumed tail of the last provider chunk (C `have`) so an FFI
    // adapter can restore `next_in`/`avail_in` exactly like C `inf_leave`
    // (infback.c L561-L569 sets `strm->avail_in = have`). This is the number of
    // bytes pulled but not yet consumed from that chunk.
    let unconsumed = ctx.have();
    ctx.src.set_unconsumed(unconsumed);

    // C L561-L569: flush the tail of the window and return.
    let code = inf_leave(&mut ctx, state, ret);
    BackOutcome {
        code,
        // C never re-clears `strm->msg` in `inf_leave`, so whatever an error site
        // stored is what the caller observes; otherwise the L214 clear stands.
        msg: match ctx.msg {
            Some(m) => BackMsg::Set(m),
            None => BackMsg::Cleared,
        },
    }
}

/// Runs the back-inflate block/symbol state machine to completion
/// (`infback.c` L225-L560), returning the code C would carry into `inf_leave`.
///
/// Each handler returns `Err(code)` to leave immediately (a failed callback), or
/// `Ok(())` after updating `state.mode` — possibly to `Done` or `Bad`.
///
/// # Why this is a separate function
///
/// The final `_` arm is C's "impossible" mode arm: `infback.c`'s `switch` has no
/// `default`, so a mode outside `{TYPE, STORED, TABLE, LEN, DONE, BAD}` falls
/// out of the loop and returns `Z_STREAM_ERROR` (`infback.c` L553-L559, `ret`
/// still holding its `Z_STREAM_ERROR` initialisation). `test/infcover.c`
/// deliberately reaches it: its `pull` callback receives the `z_stream` as its
/// descriptor and pokes `((struct inflate_state *)strm->state)->mode = SYNC`
/// mid-decode (`infcover.c` L459), then asserts `inflateBack` returns
/// `Z_STREAM_ERROR` (`infcover.c` L496-L497).
///
/// That poke is not expressible over this port's API by design — [`InFunc`]
/// yields input bytes and cannot reach private engine state, and forging it with
/// `unsafe` is forbidden in this module (AAP §0.6.2). Splitting the loop out is
/// what makes the arm reachable from a *safe* test instead: the module's own
/// `unsupported_mode_is_stream_error` test builds a real initialised state,
/// assigns an otherwise-impossible mode, and drives the machine directly, which
/// is the same observation the C driver makes by different means.
fn drive<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &mut InflateState,
) -> ReturnCode {
    loop {
        let step = match state.mode {
            InflateMode::Type => do_type(ctx, state),
            InflateMode::Stored => do_stored(ctx, state),
            InflateMode::Table => do_table(ctx, state),
            InflateMode::Len => do_len(ctx, state),
            // C L545-L548: DONE.
            InflateMode::Done => break ReturnCode::StreamEnd,
            // C L550-L551: BAD (a format error; `strm->msg` was set in C).
            InflateMode::Bad => break ReturnCode::DataError,
            // No other mode is reachable in back-inflate.
            _ => break ReturnCode::StreamError,
        };
        if let Err(code) = step {
            break code;
        }
    }
}

/// Finish a back-inflate session (C `inflateBackEnd`, `infback.c` L562-L579).
///
/// In C this frees the internal state; in Rust the owned window and state are
/// released when the [`Box`] is dropped at the end of this function, so this
/// wrapper exists for API/FFI parity. It validates the state and returns
/// [`ReturnCode::Ok`], or [`ReturnCode::StreamError`] if the state is not a
/// valid inflate state.
///
/// Callers using the idiomatic API may simply drop the [`Box<InflateState>`]
/// instead of calling this function; the effect is identical.
pub fn inflate_back_end(state: Box<InflateState>) -> ReturnCode {
    let code = back_end_validate(&state);
    // `state` and its owned window are freed here, subsuming the C `ZFREE`.
    code
}

/// [`inflate_back_end`] for a state that the FFI boundary placed through the
/// caller's allocator.
///
/// The C `inflateBackEnd` shim owns its state as a placed engine rather than a
/// plain [`Box`], because reference zlib's single `ZALLOC` at `infback.c` L51 is
/// charged to the caller and the region it returned is where the state actually
/// lives. Dropping the argument hands that region back through the caller's
/// `zfree`, which is the one free `infback.c` L572-L577 performs; the lent window
/// is deliberately left alone.
pub(crate) fn inflate_back_end_engine(state: BoxedEngine<InflateState>) -> ReturnCode {
    let code = back_end_validate(&state);
    // Dropping `state` releases its storage — through the caller's `zfree` when
    // the caller's `zalloc` supplied it.
    code
}

/// The state check both `inflateBackEnd` spellings perform, factored out so the
/// two cannot drift apart.
///
/// C `inflateBackEnd` refuses a stream with no state or no `zfree`
/// (`infback.c` L572-L573); the missing-state half is discharged by holding the
/// value at all, and the allocator half is enforced at the FFI boundary, which is
/// the only place a `zfree` pointer exists. What is left to check here is that the
/// state really is a usable inflate state.
fn back_end_validate(state: &InflateState) -> ReturnCode {
    if state.is_valid() {
        ReturnCode::Ok
    } else {
        ReturnCode::StreamError
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    // The batched-path fixtures below build their own raw DEFLATE streams with
    // the crate's own encoder, which is the `inflate -> deflate` test-only edge
    // enumerated in `lib.rs`'s `TEST_ONLY_CROSS_LAYER_EXCEPTIONS`: it is the only
    // way to build a fixture larger than a baked constant without `std` or a
    // third-party codec.
    use crate::constants::{Strategy, Z_FINISH};

    // Reference raw-DEFLATE (RFC 1951) test vectors, produced by Python's
    // `zlib` (the reference C implementation) with a negative `wbits` (raw
    // framing). Decoding these and comparing against the known plaintext is a
    // byte-exact interoperability check against reference zlib.

    /// Stored (uncompressed) block: `"Hello, stored DEFLATE world!"`.
    const STORED_VEC: [u8; 33] = [
        0x01, 0x1c, 0x00, 0xe3, 0xff, 0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20, 0x73, 0x74, 0x6f,
        0x72, 0x65, 0x64, 0x20, 0x44, 0x45, 0x46, 0x4c, 0x41, 0x54, 0x45, 0x20, 0x77, 0x6f, 0x72,
        0x6c, 0x64, 0x21,
    ];
    /// Fixed-Huffman block: `"abababababcdcdcd"`.
    const FIXED_VEC: [u8; 10] = [0x4b, 0x4c, 0x4a, 0x84, 0xc2, 0xe4, 0x14, 0x10, 0x04, 0x00];
    /// Dynamic-Huffman block: a 175-byte pangram passage.
    const DYNAMIC_VEC: [u8; 79] = [
        0xb5, 0xcb, 0xc7, 0x01, 0x80, 0x20, 0x10, 0x05, 0xd1, 0x56, 0x7e, 0x05, 0xd4, 0xe2, 0xc1,
        0x06, 0x40, 0x49, 0x06, 0x56, 0xb2, 0x50, 0xbd, 0xdb, 0x84, 0xe7, 0x79, 0xb3, 0x3a, 0x8d,
        0x58, 0xfd, 0x76, 0x42, 0x25, 0xea, 0x01, 0x86, 0x5e, 0x1c, 0xf5, 0x7e, 0x32, 0xa8, 0xe9,
        0x84, 0xc2, 0xf9, 0x92, 0x73, 0x60, 0x27, 0x2b, 0xb0, 0xfe, 0x86, 0x17, 0xc9, 0xee, 0x1e,
        0x50, 0x8c, 0xba, 0x2f, 0x0e, 0xc6, 0x37, 0xcd, 0x69, 0xea, 0x80, 0xcb, 0xc7, 0x4a, 0x89,
        0x5f, 0x9b, 0xc5, 0x07,
    ];
    /// 2048 bytes of `"0123456789abcdef"` repeated, decoded with a 512-byte
    /// (`windowBits = 9`) window so it forces multiple `ROOM` flushes and
    /// cross-wrap back-references.
    const WRAP_VEC: [u8; 34] = [
        0x33, 0x30, 0x34, 0x32, 0x36, 0x31, 0x35, 0x33, 0xb7, 0xb0, 0x4c, 0x4c, 0x4a, 0x4e, 0x49,
        0x4d, 0x33, 0x18, 0xe5, 0x8f, 0xf2, 0x47, 0xf9, 0xa3, 0xfc, 0x51, 0xfe, 0x28, 0x7f, 0x94,
        0x3f, 0xec, 0xf9, 0x00,
    ];
    /// The empty stream: a final fixed block with only an end-of-block code.
    const EMPTY_VEC: [u8; 2] = [0x03, 0x00];

    /// Input provider that yields the entire buffer in a single chunk, then
    /// signals end-of-input.
    struct SliceIn<'a> {
        data: &'a [u8],
        done: bool,
        /// The chunk made current by the most recent successful `advance`.
        cur: &'a [u8],
    }
    impl InFunc for SliceIn<'_> {
        fn advance(&mut self) -> bool {
            if self.done {
                false
            } else {
                self.done = true;
                self.cur = self.data;
                true
            }
        }
        fn chunk(&self) -> &[u8] {
            self.cur
        }
    }

    /// Input provider that yields one byte at a time, exercising the `PULL`
    /// refill path across many callback invocations.
    struct ChunkyIn<'a> {
        data: &'a [u8],
        pos: usize,
        /// Window over the single byte made current by the last `advance`.
        cur: &'a [u8],
        /// Number of times `advance` has been invoked, so a test can assert the
        /// engine pulls exactly as often as C's `in()` is called.
        advances: usize,
    }
    impl InFunc for ChunkyIn<'_> {
        fn advance(&mut self) -> bool {
            self.advances += 1;
            if self.pos >= self.data.len() {
                return false;
            }
            self.cur = &self.data[self.pos..self.pos + 1];
            self.pos += 1;
            true
        }
        fn chunk(&self) -> &[u8] {
            self.cur
        }
    }

    /// Output sink that accumulates every emitted byte.
    struct VecOut {
        data: Vec<u8>,
    }
    impl OutFunc for VecOut {
        fn write_output(&mut self, buf: &[u8]) -> Result<(), ()> {
            self.data.extend_from_slice(buf);
            Ok(())
        }
    }

    /// Output sink that aborts on the first write.
    struct AlwaysErrOut;
    impl OutFunc for AlwaysErrOut {
        fn write_output(&mut self, _buf: &[u8]) -> Result<(), ()> {
            Err(())
        }
    }

    /// Decode `data` in one shot with the given window size, returning the
    /// return code and the collected output.
    fn run(window_bits: i32, data: &[u8]) -> (ReturnCode, Vec<u8>) {
        run_full(window_bits, data).0
    }

    /// As [`run`], but also returns the diagnostic C would have left in
    /// `strm->msg`, so a test can assert the exact `infback.c` string.
    fn run_full(window_bits: i32, data: &[u8]) -> ((ReturnCode, Vec<u8>), BackMsg) {
        let mut state = inflate_back_init(window_bits).expect("valid windowBits");
        let mut src = SliceIn {
            data,
            done: false,
            cur: &[],
        };
        let mut sink = VecOut { data: Vec::new() };
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        ((outcome.code, sink.data), outcome.msg)
    }

    /// The 175-byte plaintext behind [`DYNAMIC_VEC`].
    fn dynamic_plaintext() -> Vec<u8> {
        let base = b"The quick brown fox jumps over the lazy dog. ";
        let mut expected: Vec<u8> = Vec::new();
        for _ in 0..3 {
            expected.extend_from_slice(base);
        }
        expected.extend_from_slice(b"Pack my box with five dozen liquor jugs.");
        expected
    }

    #[test]
    fn order_matches_c() {
        // Verbatim `order[]` permutation from infback.c L205-L206.
        assert_eq!(
            ORDER,
            [
                16u16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15
            ]
        );
    }

    #[test]
    fn init_sets_raw_geometry() {
        let state = inflate_back_init(15).expect("15 is valid");
        assert_eq!(state.dmax, 32768);
        assert!(state.sane);
        assert_eq!(state.wbits, 15);
        assert_eq!(state.wsize, 1 << 15);
        assert_eq!(state.whave, 0);
        assert_eq!(state.wnext, 0);
        assert_eq!(state.window.len(), 1 << 15);
    }

    #[test]
    fn window_bits_out_of_range_is_stream_error() {
        // Raw inflateBack accepts only 8..=15. (`InflateState` does not derive
        // `Debug`, so match on the result rather than calling `unwrap_err`.)
        assert!(matches!(inflate_back_init(7), Err(ReturnCode::StreamError)));
        assert!(matches!(
            inflate_back_init(16),
            Err(ReturnCode::StreamError)
        ));
        assert!(matches!(inflate_back_init(0), Err(ReturnCode::StreamError)));
        assert!(matches!(
            inflate_back_init(-15),
            Err(ReturnCode::StreamError)
        ));
        assert!(inflate_back_init(8).is_ok());
        assert!(inflate_back_init(15).is_ok());
    }

    #[test]
    fn stored_block_round_trips() {
        let (rc, out) = run(15, &STORED_VEC);
        assert_eq!(rc, ReturnCode::StreamEnd);
        assert_eq!(out, b"Hello, stored DEFLATE world!");
    }

    #[test]
    fn fixed_block_round_trips() {
        let (rc, out) = run(15, &FIXED_VEC);
        assert_eq!(rc, ReturnCode::StreamEnd);
        assert_eq!(out, b"abababababcdcdcd");
    }

    #[test]
    fn dynamic_block_round_trips() {
        let (rc, out) = run(15, &DYNAMIC_VEC);
        assert_eq!(rc, ReturnCode::StreamEnd);
        assert_eq!(out, dynamic_plaintext());
    }

    #[test]
    fn empty_stream_round_trips() {
        let (rc, out) = run(15, &EMPTY_VEC);
        assert_eq!(rc, ReturnCode::StreamEnd);
        assert!(out.is_empty());
    }

    #[test]
    fn window_wrap_and_room_flush() {
        // 2048 bytes decoded through a 512-byte window: forces ROOM() to flush
        // the full window several times and resolves back-references that wrap
        // around the ring.
        let (rc, out) = run(9, &WRAP_VEC);
        assert_eq!(rc, ReturnCode::StreamEnd);
        let expected: Vec<u8> = b"0123456789abcdef"
            .iter()
            .copied()
            .cycle()
            .take(2048)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn chunky_input_refills_across_callbacks() {
        // Feed the dynamic stream one byte per callback to exercise PULL refill.
        let mut state = inflate_back_init(15).unwrap();
        let mut src = ChunkyIn {
            data: &DYNAMIC_VEC,
            pos: 0,
            cur: &[],
            advances: 0,
        };
        let mut sink = VecOut { data: Vec::new() };
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        assert_eq!(outcome.msg, BackMsg::Cleared);
        assert_eq!(sink.data, dynamic_plaintext());
        // C invokes `in()` only when a byte is actually needed, so a stream whose
        // final block ends on its last byte produces exactly one call per byte and
        // never a trailing dry probe. Measured directly against reference C at the
        // ABI (`calls == i` for every well-formed vector), this identity holds only
        // because the engine consumes the provider's chunks in place: a buffering
        // engine that read ahead would pull a different number of times.
        assert_eq!(
            src.advances,
            DYNAMIC_VEC.len(),
            "exactly one advance per byte, with no read-ahead and no dry probe",
        );
        assert_eq!(src.pos, DYNAMIC_VEC.len(), "every byte must be pulled once");
    }

    #[test]
    fn invalid_block_type_is_data_error() {
        // A single byte: BFINAL = 1, BTYPE = 11 (reserved) -> data error.
        let (rc, out) = run(15, &[0x07]);
        assert_eq!(rc, ReturnCode::DataError);
        assert!(out.is_empty());
    }

    #[test]
    fn bad_stored_length_check_is_data_error() {
        // Stored block whose NLEN is not the one's-complement of LEN.
        // byte0 = 0x01 (BFINAL=1, stored); LEN=0x0004; NLEN=0x0000 (wrong).
        let bad = [0x01u8, 0x04, 0x00, 0x00, 0x00];
        let (rc, _out) = run(15, &bad);
        assert_eq!(rc, ReturnCode::DataError);
    }

    #[test]
    fn truncated_input_is_buf_error() {
        // Only the stored header's first byte: the decoder needs more input
        // than the callback can supply.
        let (rc, _out) = run(15, &STORED_VEC[..1]);
        assert_eq!(rc, ReturnCode::BufError);
    }

    #[test]
    fn output_abort_is_buf_error() {
        // Decoding succeeds internally, but the final flush is refused by the
        // sink; a successful decode is downgraded to BufError (C inf_leave).
        let mut state = inflate_back_init(15).unwrap();
        let mut src = SliceIn {
            data: &FIXED_VEC,
            done: false,
            cur: &[],
        };
        let mut sink = AlwaysErrOut;
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::BufError);
        // C assigns no diagnostic on the failed-flush path; the L214 clear stands.
        assert_eq!(outcome.msg, BackMsg::Cleared);
    }

    /// Port of `test/infcover.c`'s forced-mode assertion (`infcover.c` L459 and
    /// L496-L497): a mode the back-inflate machine cannot service yields
    /// `Z_STREAM_ERROR`.
    ///
    /// The C driver reaches this by having its `pull` callback poke
    /// `((struct inflate_state *)strm->state)->mode = SYNC` through the stream it
    /// receives as the input descriptor — "force an otherwise impossible
    /// situation", as the C comment puts it. This port's [`InFunc`] cannot do
    /// that (it yields bytes and never sees engine state), and forging it with
    /// `unsafe` is forbidden here, so the same observation is made directly on
    /// [`drive`]: build a real initialised state, assign the impossible mode, and
    /// run the machine.
    ///
    /// `Sync` is asserted first because it is exactly the mode C forces. The
    /// remaining modes are the ones a *wrapper* decode passes through and
    /// back-inflate never can (it is raw-only); every one of them must land in
    /// the same arm, so a future `match` that grew a wrong handler for one of
    /// them cannot pass this test.
    #[test]
    fn unsupported_mode_is_stream_error() {
        for mode in [
            InflateMode::Sync,
            InflateMode::Head,
            InflateMode::Dict,
            InflateMode::Mem,
            InflateMode::Check,
            InflateMode::Length,
        ] {
            let mut state = inflate_back_init(15).expect("inflate_back_init(15)");
            let data = FIXED_VEC;
            let mut src = SliceIn {
                data: &data,
                done: false,
                cur: &[],
            };
            let mut sink = VecOut { data: Vec::new() };
            let wsize = state.wsize as usize;
            let mut ctx = BackCtx {
                next: 0,
                msg: None,
                hold: 0,
                bits: 0,
                put: 0,
                left: wsize,
                wsize,
                src: &mut src,
                sink: &mut sink,
            };

            state.mode = mode;
            assert_eq!(
                drive(&mut ctx, &mut state),
                ReturnCode::StreamError,
                "mode {mode:?} is not serviceable by inflate_back and must \
                 return Z_STREAM_ERROR, as infcover.c asserts for SYNC",
            );
            // C leaves the forced mode in place; nothing consumed it.
            assert_eq!(state.mode, mode, "drive must not rewrite an unusable mode");
            assert!(
                sink.data.is_empty(),
                "no output may be produced from an unusable mode",
            );
        }
    }

    #[test]
    fn end_subsumes_drop() {
        // inflate_back_end validates and returns Ok; the box (and its window)
        // is freed on return.
        let state = inflate_back_init(15).unwrap();
        assert_eq!(inflate_back_end(state), ReturnCode::Ok);
    }

    /// `inflate_back_init_borrowed_window` charges C's *single*
    /// state allocation and adopts the supplied window without requesting one.
    ///
    /// This is the property that makes the FFI `inflateBackInit_` shim match
    /// `infback.c` exactly: one `ZALLOC` for the state (L51), then
    /// `state->window = window;` (L60). A recording allocator therefore sees one
    /// request, shaped `(1, sizeof(struct inflate_state))` — C's own pair, taken
    /// from the `#[repr(C)]` layout mirror — so an arena sized from C's header
    /// serves it exactly as it serves reference zlib (AAP §0.6.5).
    #[test]
    fn borrowed_window_init_makes_only_the_state_request() {
        use core::cell::RefCell;

        use crate::stream::{AllocBuffer, Allocator, ZeroValid};

        /// Records every `(items, item_size)` pair, serving each from the global
        /// allocator. `reserves_state_footprint` is left at its `true` default,
        /// which is what a bounded C-style allocator behaves like.
        struct Recorder {
            seen: RefCell<Vec<(usize, usize)>>,
        }

        impl Allocator for Recorder {
            fn allocate_zeroed_items<T>(
                &self,
                items: usize,
                item_size: usize,
            ) -> Option<AllocBuffer<T>>
            where
                T: Copy + Default + ZeroValid + 'static,
            {
                self.seen.borrow_mut().push((items, item_size));
                AllocBuffer::try_zeroed_items(items, item_size, self.hook())
            }
        }

        let alloc = Recorder {
            seen: RefCell::new(Vec::new()),
        };
        // A window the caller "owns" — here an ordinary owned buffer, which stands
        // in for the region the FFI shim lends through `borrow_caller_window`.
        let window = AllocBuffer::<u8>::try_zeroed(1 << 15, AllocHook::none())
            .expect("global allocator serves a 32 KiB window");

        let state = inflate_back_init_borrowed_window(&alloc, 15, || Some(window))
            .expect("a healthy allocator initializes the state");
        assert_eq!(state.wsize, 1 << 15);
        assert_eq!(state.wbits, 15);
        assert_eq!(state.dmax, 32768);
        assert!(state.sane);
        assert_eq!(state.whave, 0);
        assert_eq!(state.wnext, 0);
        assert_eq!(state.window.len(), 1 << 15);

        assert_eq!(
            alloc.seen.into_inner(),
            vec![(1, InflateState::C_LAYOUT_SIZE)],
            "exactly C's single state request, with C's own `sizeof(struct inflate_state)`"
        );
    }

    /// A window shorter than `1 << window_bits` is rejected rather than silently
    /// truncating the history the decoder may reach back into.
    ///
    /// C cannot detect this — `inflateBackInit_` receives a bare pointer — but
    /// this port knows the lent region's length, so the check costs nothing and
    /// turns what would be out-of-bounds access in C into `Z_STREAM_ERROR`.
    #[test]
    fn borrowed_window_init_rejects_a_short_window() {
        use crate::stream::AllocBuffer;

        let short = AllocBuffer::<u8>::try_zeroed((1 << 15) - 1, AllocHook::none())
            .expect("global allocator serves the buffer");
        assert!(matches!(
            inflate_back_init_borrowed_window(&HookAllocator::new(AllocHook::none()), 15, || {
                Some(short)
            }),
            Err(ReturnCode::StreamError)
        ));

        // Out-of-range windowBits is still rejected first, before the length test.
        let ok = AllocBuffer::<u8>::try_zeroed(1 << 15, AllocHook::none())
            .expect("global allocator serves the buffer");
        assert!(matches!(
            inflate_back_init_borrowed_window(&HookAllocator::new(AllocHook::none()), 16, || {
                Some(ok)
            }),
            Err(ReturnCode::StreamError)
        ));
    }

    /// The engine must read input **through** the provider on every byte rather
    /// than copying each chunk once into a buffer of its own.
    ///
    /// The observable that separates the two strategies is how often
    /// [`InFunc::chunk`] is consulted. A copying engine calls it exactly once per
    /// successful `advance` — it needs the bytes only to memcpy them — so for a
    /// single-chunk provider the count would be `1`. A lending engine consults it
    /// on every `PULL`, `PULLBYTE` and stored-block copy, so the count scales with
    /// the input. This is fully within the C callback contract: nothing mutates
    /// the chunk, it is merely observed.
    #[test]
    fn the_decoder_reads_through_the_provider_instead_of_copying() {
        use core::cell::Cell;

        /// One-chunk provider that counts how often the engine looks at the chunk.
        struct CountingIn<'a> {
            data: &'a [u8],
            done: bool,
            cur: &'a [u8],
            chunk_calls: Cell<usize>,
        }
        impl InFunc for CountingIn<'_> {
            fn advance(&mut self) -> bool {
                if self.done {
                    return false;
                }
                self.done = true;
                self.cur = self.data;
                true
            }
            fn chunk(&self) -> &[u8] {
                self.chunk_calls.set(self.chunk_calls.get() + 1);
                self.cur
            }
        }

        let mut state = inflate_back_init(15).unwrap();
        let mut src = CountingIn {
            data: &DYNAMIC_VEC,
            done: false,
            cur: &[],
            chunk_calls: Cell::new(0),
        };
        let mut sink = VecOut { data: Vec::new() };
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        assert_eq!(sink.data, dynamic_plaintext());

        let calls = src.chunk_calls.get();
        assert!(
            calls >= DYNAMIC_VEC.len(),
            "a lending decoder consults the provider at least once per input byte; \
             {calls} call(s) for {} byte(s) means the chunk was copied instead",
            DYNAMIC_VEC.len()
        );
    }

    /// A stored block must be copied straight out of the provider's memory, which
    /// is the one place the old shape used `Vec` as a *source* slice rather than
    /// just as a refill buffer.
    #[test]
    fn a_stored_block_is_copied_out_of_provider_memory() {
        use core::cell::Cell;

        struct CountingIn<'a> {
            data: &'a [u8],
            done: bool,
            cur: &'a [u8],
            chunk_calls: Cell<usize>,
            advances: usize,
        }
        impl InFunc for CountingIn<'_> {
            fn advance(&mut self) -> bool {
                self.advances += 1;
                if self.done {
                    return false;
                }
                self.done = true;
                self.cur = self.data;
                true
            }
            fn chunk(&self) -> &[u8] {
                self.chunk_calls.set(self.chunk_calls.get() + 1);
                self.cur
            }
        }

        let mut state = inflate_back_init(15).unwrap();
        let mut src = CountingIn {
            data: &STORED_VEC,
            done: false,
            cur: &[],
            chunk_calls: Cell::new(0),
            advances: 0,
        };
        let mut sink = VecOut { data: Vec::new() };
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        assert_eq!(outcome.msg, BackMsg::Cleared);
        assert_eq!(&sink.data[..], b"Hello, stored DEFLATE world!");
        // Measured: the lending decoder observes this 33-byte chunk 15 times (the
        // header bit-pulls plus the bulk copy). A decoder that copied each chunk
        // once would observe it a fixed handful of times regardless of length, so
        // both a concrete floor and an advance-relative floor are asserted.
        let calls = src.chunk_calls.get();
        assert!(
            calls >= 8,
            "the stored-block copy must read the provider's slice directly; \
             {calls} observation(s) indicates the chunk was buffered instead"
        );
        assert!(
            calls > 3 * src.advances,
            "observations must scale with the bytes consumed, not with the number \
             of chunks ({calls} observation(s) across {} advance(s))",
            src.advances
        );
    }

    /// Every one of the twelve `infback.c` error sites must report C's exact
    /// diagnostic string, and a clean or short stream must report the cleared
    /// field C leaves at `infback.c` L214.
    ///
    /// The vectors and their expected texts are `test/infcover.c` L584-L598, the
    /// official table this crate treats as the decoder's conformance oracle.
    #[test]
    fn every_error_site_reports_cs_exact_diagnostic() {
        // (raw DEFLATE bytes, the exact `strm->msg` C would publish)
        let cases: [(&[u8], &str); 12] = [
            (
                &[0x00, 0x00, 0x00, 0x00, 0x00],
                "invalid stored block lengths",
            ),
            (&[0x06], "invalid block type"),
            (&[0xfc, 0x00, 0x00], "too many length or distance symbols"),
            (&[0x04, 0x00, 0xfe, 0xff], "invalid code lengths set"),
            (&[0x04, 0x00, 0x24, 0x49, 0x00], "invalid bit length repeat"),
            (
                &[0x04, 0x00, 0x24, 0xe9, 0xff, 0xff],
                "invalid bit length repeat",
            ),
            (
                &[0x04, 0x00, 0x24, 0xe9, 0xff, 0x6d],
                "invalid code -- missing end-of-block",
            ),
            (
                &[
                    0x04, 0x80, 0x49, 0x92, 0x24, 0x49, 0x92, 0x24, 0x71, 0xff, 0xff, 0x93, 0x11,
                    0x00,
                ],
                "invalid literal/lengths set",
            ),
            (
                &[
                    0x04, 0x80, 0x49, 0x92, 0x24, 0x49, 0x92, 0x24, 0x0f, 0xb4, 0xff, 0xff, 0xc3,
                    0x84,
                ],
                "invalid distances set",
            ),
            (
                &[
                    0x04, 0xc0, 0x81, 0x08, 0x00, 0x00, 0x00, 0x00, 0x20, 0x7f, 0xeb, 0x0b, 0x00,
                    0x00,
                ],
                "invalid literal/length code",
            ),
            (&[0x02, 0x7e, 0xff, 0xff], "invalid distance code"),
            (
                &[
                    0x0c, 0xc0, 0x81, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0xff, 0x6b, 0x04, 0x00,
                ],
                "invalid distance too far back",
            ),
        ];
        for (bytes, expected) in cases {
            let ((code, _out), msg) = run_full(15, bytes);
            assert_eq!(
                code,
                ReturnCode::DataError,
                "vector for {expected:?} must be a data error"
            );
            assert_eq!(
                msg,
                BackMsg::Set(expected),
                "vector for {expected:?} reported the wrong diagnostic"
            );
        }

        // The two well-formed vectors from the same table leave the field cleared.
        for ok in [
            &[0x03u8, 0x00][..],
            &[0x01, 0x01, 0x00, 0xfe, 0xff, 0x00][..],
        ] {
            let ((code, _), msg) = run_full(15, ok);
            assert_eq!(code, ReturnCode::StreamEnd);
            assert_eq!(msg, BackMsg::Cleared);
        }

        // A truncated stream exits `Z_BUF_ERROR` without C assigning a diagnostic,
        // so the L214 clear is what the caller observes — not a stale message.
        let ((code, _), msg) = run_full(15, &[0x04]);
        assert_eq!(code, ReturnCode::BufError);
        assert_eq!(msg, BackMsg::Cleared);
    }

    /// A state `inflate_back` refuses must leave the diagnostic field alone,
    /// because C returns at `infback.c` L209-L210 — *before* the
    /// `strm->msg = Z_NULL` at L214.
    #[test]
    fn a_refused_call_leaves_the_diagnostic_untouched() {
        let mut state = inflate_back_init(15).expect("15 is valid");
        // Zero the window geometry so the entry check fails, standing in for C's
        // `strm->state == Z_NULL`.
        state.wsize = 0;
        let mut src = SliceIn {
            data: &FIXED_VEC,
            done: false,
            cur: &[],
        };
        let mut sink = VecOut { data: Vec::new() };
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        assert_eq!(outcome.code, ReturnCode::StreamError);
        assert_eq!(
            outcome.msg,
            BackMsg::Untouched,
            "a rejected call must not clear a diagnostic C preserves"
        );
        assert!(sink.data.is_empty(), "no output from a refused call");
    }

    /// `inflateBackInit_`'s allocation ordering: C's single state `ZALLOC`
    /// (`infback.c` L51-L53) runs *before* `state->window = window;` (L59), so a
    /// refused state request must return `Z_MEM_ERROR` without the caller's window
    /// ever being named, let alone written.
    #[test]
    fn a_refused_state_request_never_reaches_the_window() {
        use crate::stream::AllocBuffer;
        use core::cell::Cell;

        /// Allocator that refuses everything, so the state request is the first
        /// and only thing that happens.
        struct Broke;
        impl Allocator for Broke {
            fn hook(&self) -> AllocHook {
                AllocHook::none()
            }
            fn reserves_state_footprint(&self) -> bool {
                true
            }
            fn allocate_zeroed_items<T: Copy + Default + crate::stream::ZeroValid>(
                &self,
                _items: usize,
                _item_size: usize,
            ) -> Option<AllocBuffer<T>> {
                None
            }
        }

        let lent = Cell::new(false);
        let result = inflate_back_init_borrowed_window(&Broke, 15, || {
            lent.set(true);
            AllocBuffer::<u8>::try_zeroed(1 << 15, AllocHook::none())
        });
        // `InflateState` is deliberately not `Debug` (it holds caller memory), so
        // match rather than unwrap.
        match result {
            Err(e) => assert_eq!(e, ReturnCode::MemError, "C returns Z_MEM_ERROR here"),
            Ok(_) => panic!("a refusing allocator must not initialize the state"),
        }
        assert!(
            !lent.get(),
            "the caller's window must not be borrowed at all once the state \
             request has failed -- C never reaches infback.c L59"
        );
    }

    // =====================================================================
    // Batched decode path (`back_fast`)
    //
    // C selects between two decoders at `infback.c` L423-L428 purely on how
    // much input and window space is available, and both must emit the same
    // bytes. The tests below attack that from three directions:
    //
    //  1. `forward_copy` against a literal model of C's byte loop, exhaustively
    //     over every source/destination relationship.
    //  2. Differentially: the same stream decoded with providers whose chunks
    //     are too small for the batched path to ever be entered must produce
    //     byte-identical output to a one-shot decode that uses it constantly.
    //  3. White-box: `back_fast` called directly, plus each of its three error
    //     arms driven by a hand-built bitstream.
    // =====================================================================

    /// A literal transcription of C's copy loop
    /// (`do { *put++ = *from++; } while (--n);`), used as the model
    /// [`forward_copy`] must match. Deliberately naive: one byte at a time, in
    /// increasing order, re-reading whatever the previous iteration wrote.
    fn byte_loop_model(buf: &mut [u8], dst: usize, src: usize, n: usize) {
        for k in 0..n {
            let b = buf[src + k];
            buf[dst + k] = b;
        }
    }

    /// Deterministic filler so a copy test can tell every position apart
    /// without pulling in a random number generator.
    fn ramp(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8) ^ 0x5a).collect()
    }

    #[test]
    fn forward_copy_matches_the_c_byte_loop_in_both_directions() {
        // `src > dst` (the ring-wrap arm), `src == dst` (a distance of exactly
        // one whole window) and `src < dst` (a back-reference) are three
        // genuinely different regimes, and every one of them can overlap the
        // destination. Sweep all three together.
        const LEN: usize = 1024;
        let base = ramp(LEN);
        let lengths = [
            0usize, 1, 2, 3, 4, 5, 7, 8, 15, 16, 31, 63, 64, 127, 200, 257, 258,
        ];
        for src in 0..48usize {
            for dst in 0..48usize {
                for n in lengths {
                    // Keep both ranges inside the buffer; the real callers are
                    // bounded by the window geometry the same way.
                    if src + n > LEN || dst + n > LEN {
                        continue;
                    }
                    let mut got = base.clone();
                    let mut want = base.clone();
                    forward_copy(&mut got, dst, src, n);
                    byte_loop_model(&mut want, dst, src, n);
                    assert_eq!(
                        got, want,
                        "forward_copy(dst={dst}, src={src}, n={n}) diverged from the C byte loop"
                    );
                }
            }
        }
    }

    #[test]
    fn forward_copy_matches_the_c_byte_loop_for_every_short_distance() {
        // The periodic arm is the one that must self-observe, so sweep every
        // distance a DEFLATE match can use against every length it can carry.
        const LEN: usize = 40_000;
        let base = ramp(LEN);
        for dist in 1..=64usize {
            for n in 0..=258usize {
                let src = 1_000usize;
                let dst = src + dist;
                let mut got = base.clone();
                let mut want = base.clone();
                forward_copy(&mut got, dst, src, n);
                byte_loop_model(&mut want, dst, src, n);
                assert_eq!(
                    got, want,
                    "forward_copy diverged for dist={dist}, n={n} (periodic arm)"
                );
            }
        }
        // A distance of exactly one window (`src == dst`) is a no-op in C, and
        // the largest distance DEFLATE can encode must behave too.
        for n in [0usize, 1, 3, 258] {
            let mut got = base.clone();
            let mut want = base.clone();
            forward_copy(&mut got, 5_000, 5_000, n);
            byte_loop_model(&mut want, 5_000, 5_000, n);
            assert_eq!(got, want, "src == dst must copy bytes onto themselves");

            let mut got = base.clone();
            let mut want = base.clone();
            forward_copy(&mut got, 1_000, 1_000 + 32_768 - 32_768, n);
            byte_loop_model(&mut want, 1_000, 1_000, n);
            assert_eq!(got, want, "degenerate zero distance");
        }
    }

    /// Input provider that yields fixed-size chunks, so a test can choose
    /// whether [`back_fast`]'s `have >= 6` entry test can ever be satisfied.
    struct ChunkedIn<'a> {
        data: &'a [u8],
        pos: usize,
        size: usize,
        cur: &'a [u8],
        /// Total bytes the engine reported as *unconsumed* on the final chunk.
        unconsumed: usize,
    }
    impl InFunc for ChunkedIn<'_> {
        fn advance(&mut self) -> bool {
            if self.pos >= self.data.len() {
                return false;
            }
            let end = core::cmp::min(self.pos + self.size, self.data.len());
            self.cur = &self.data[self.pos..end];
            self.pos = end;
            true
        }
        fn chunk(&self) -> &[u8] {
            self.cur
        }
        fn set_unconsumed(&mut self, unconsumed: usize) {
            self.unconsumed = unconsumed;
        }
    }

    /// Decodes `data` with a provider that hands out `size`-byte chunks,
    /// returning the code, the output, and how many input bytes were consumed
    /// in total.
    fn run_chunked(window_bits: i32, data: &[u8], size: usize) -> (ReturnCode, Vec<u8>, usize) {
        let mut state = inflate_back_init(window_bits).expect("valid windowBits");
        let mut src = ChunkedIn {
            data,
            pos: 0,
            size,
            cur: &[],
            unconsumed: 0,
        };
        let mut sink = VecOut { data: Vec::new() };
        let outcome = inflate_back(&mut state, &mut src, &mut sink);
        let consumed = src.pos - src.unconsumed;
        (outcome.code, sink.data, consumed)
    }

    /// Compresses `data` as a **raw** DEFLATE stream (`infback.c` decodes raw
    /// streams only) using the crate's own encoder, so a fixture can be far
    /// larger than a baked-in constant vector.
    ///
    /// The decoder must be opened with the same `window_bits`: the encoder's
    /// window bounds the largest distance it emits, and pairing a 15-bit encoder
    /// with a 9-bit decoder is a legitimately invalid combination whose
    /// `Z_DATA_ERROR` would look like a decoder defect.
    fn raw_deflate(data: &[u8], window_bits: i32, level: i32, strategy: Strategy) -> Vec<u8> {
        use crate::constants::Z_DEFLATED;
        use crate::stream::ZStream;

        let mut strm = ZStream::new();
        crate::deflate::deflate_init2(&mut strm, level, Z_DEFLATED, -window_bits, 8, strategy)
            .expect("raw deflate init");
        let mut out = alloc::vec![0u8; data.len() + data.len() / 2 + 1024];
        let r = crate::deflate::deflate(&mut strm, data, &mut out, Z_FINISH);
        assert_eq!(
            r.code,
            ReturnCode::StreamEnd,
            "the fixture must fit one call"
        );
        assert_eq!(r.consumed, data.len());
        out.truncate(r.produced);
        crate::deflate::deflate_end(&mut strm).expect("raw deflate end");
        out
    }

    /// Cheap deterministic pseudo-random bytes (xorshift32) — the crate has no
    /// runtime random dependency and the tests must not add one.
    fn noise(len: usize, seed: u32) -> Vec<u8> {
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect()
    }

    /// The four corpus shapes that between them reach every copy arm:
    /// ordinary short/medium back-references, `dist == 1` runs, distances at or
    /// beyond a small window, and mostly-literal data.
    fn corpus(kind: &str, len: usize) -> Vec<u8> {
        let v = corpus_body(kind, len);
        // A fixture that silently came out empty would make every comparison
        // below vacuously true, so its size is checked rather than assumed.
        assert_eq!(v.len(), len, "corpus {kind} must be exactly {len} bytes");
        v
    }

    fn corpus_body(kind: &str, len: usize) -> Vec<u8> {
        match kind {
            "text" => {
                let base: &[u8] = b"the quick brown fox jumps over the lazy dog -- \
                                    pack my box with five dozen liquor jugs; ";
                base.iter().copied().cycle().take(len).collect()
            }
            "run" => {
                // Long identical runs (dist == 1, length 258) interleaved with
                // short literal islands so blocks do not degenerate to stored.
                let mut v = Vec::with_capacity(len);
                let mut b = 0u8;
                while v.len() < len {
                    let run = 300 + (usize::from(b) % 700);
                    for _ in 0..run.min(len - v.len()) {
                        v.push(b);
                    }
                    if v.len() < len {
                        v.push(b ^ 0xff);
                    }
                    b = b.wrapping_add(37);
                }
                v
            }
            "period" => {
                // Period deliberately close to a 512-byte window so matches land
                // on both sides of the `dist > put` split, including
                // `dist == wsize`.
                let block = noise(509, 0x1234_5678);
                block.iter().copied().cycle().take(len).collect()
            }
            "noisy" => noise(len, 0x9e37_79b9),
            other => panic!("unknown corpus {other}"),
        }
    }

    /// Decodes one stream every way a provider can hand it over and asserts they
    /// all agree with `expected`.
    ///
    /// Chunk sizes below six make `have >= 6` unsatisfiable, so the batched path
    /// is never entered and the per-symbol path decodes the whole stream; larger
    /// ones enter it repeatedly, including right at a chunk boundary where the
    /// accumulator rewind is clamped. Equality across all of them is the
    /// property that matters: it proves the two paths are byte-identical on real
    /// data rather than merely both "plausible".
    fn decode_every_way(window_bits: i32, stream: &[u8], expected: &[u8], what: &str) {
        let (rc, out) = run(window_bits, stream);
        assert_eq!(rc, ReturnCode::StreamEnd, "{what}: one-shot decode");
        assert_eq!(out, expected, "{what}: one-shot decode (batched path)");

        for size in [1usize, 5, 7, 64, 4096] {
            let (rc, out, consumed) = run_chunked(window_bits, stream, size);
            assert_eq!(rc, ReturnCode::StreamEnd, "{what}: chunk size {size}");
            assert_eq!(out, expected, "{what}: chunk size {size}");
            assert!(
                consumed <= stream.len(),
                "{what}: chunk size {size} claims {consumed} of {} bytes consumed",
                stream.len()
            );
        }
    }

    #[test]
    fn both_decode_paths_agree_on_every_corpus_shape() {
        for kind in ["text", "run", "period", "noisy"] {
            let data = corpus(kind, if kind == "noisy" { 8 * 1024 } else { 32 * 1024 });
            for wb in [9i32, 12, 15] {
                for (level, strategy) in [
                    (1, Strategy::Default),
                    (6, Strategy::Default),
                    (9, Strategy::Default),
                    (6, Strategy::Fixed),
                    (6, Strategy::Rle),
                    (6, Strategy::HuffmanOnly),
                ] {
                    let stream = raw_deflate(&data, wb, level, strategy);
                    let what = alloc::format!("{kind} wb={wb} level={level} {strategy:?}");
                    decode_every_way(wb, &stream, &data, &what);
                }
            }
        }
    }

    #[test]
    fn both_decode_paths_agree_when_the_window_wraps_many_times() {
        // A 512-byte window with 256 KiB of highly repetitive input flushes the
        // window hundreds of times, so `dist > put` (read from the window tail)
        // and `dist == wsize` are both hit constantly inside the batched path.
        let data = corpus("period", 256 * 1024);
        let stream = raw_deflate(&data, 9, 9, Strategy::Default);
        decode_every_way(9, &stream, &data, "period wb=9 level=9 many wraps");
    }

    #[test]
    fn a_truncated_stream_is_a_buf_error_on_both_paths() {
        let data = corpus("text", 32 * 1024);
        let stream = raw_deflate(&data, 15, 6, Strategy::Default);
        let cut = &stream[..stream.len() * 3 / 4];
        // One-shot: the batched path consumes most of it, then the per-symbol
        // path runs dry.
        let (rc, _out) = run(15, cut);
        assert_eq!(rc, ReturnCode::BufError, "one-shot truncated");
        // Chunked below the batched threshold: per-symbol path only.
        let (rc, _out, _consumed) = run_chunked(15, cut, 5);
        assert_eq!(rc, ReturnCode::BufError, "chunked truncated");
    }

    #[test]
    fn an_aborting_sink_is_a_buf_error_even_when_the_batched_path_runs() {
        // 256 KiB through a 512-byte window: the batched path decodes, and the
        // first `ROOM()` flush in the per-symbol path meets the refusing sink.
        let data = corpus("text", 256 * 1024);
        let stream = raw_deflate(&data, 9, 6, Strategy::Default);
        let mut state = inflate_back_init(9).unwrap();
        let mut src = SliceIn {
            data: &stream,
            done: false,
            cur: &[],
        };
        let outcome = inflate_back(&mut state, &mut src, &mut AlwaysErrOut);
        assert_eq!(outcome.code, ReturnCode::BufError);
        assert_eq!(outcome.msg, BackMsg::Cleared, "C sets no diagnostic here");
    }

    /// Least-significant-bit-first bit writer, i.e. DEFLATE's packing order, so
    /// a test can hand-build a bitstream that reaches an arm no valid encoder
    /// emits.
    struct BitWriter {
        out: Vec<u8>,
        acc: u32,
        nbits: u32,
    }
    impl BitWriter {
        fn new() -> Self {
            BitWriter {
                out: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }
        fn bit(&mut self, b: u32) {
            self.acc |= b << self.nbits;
            self.nbits += 1;
            if self.nbits == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.nbits = 0;
            }
        }
        /// Header fields and extra bits: least-significant bit first.
        fn bits(&mut self, val: u32, n: u32) {
            for i in 0..n {
                self.bit((val >> i) & 1);
            }
        }
        /// A Huffman code: most-significant bit first (RFC 1951 §3.1.1).
        fn code(&mut self, code: u32, n: u32) {
            for i in (0..n).rev() {
                self.bit((code >> i) & 1);
            }
        }
        /// Finishes the stream and pads it so at least
        /// [`BACK_FAST_MIN_INPUT`] bytes are always available at the first `LEN`
        /// state — which is what forces the *batched* path to be the one that
        /// meets the malformed code.
        fn finish_padded(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                self.out.push(self.acc as u8);
            }
            self.out.extend_from_slice(&[0u8; 2 * BACK_FAST_MIN_INPUT]);
            self.out
        }
    }

    /// The fixed literal/length code and its bit length (RFC 1951 §3.2.6).
    fn fixed_litlen(sym: u32) -> (u32, u32) {
        match sym {
            0..=143 => (0x30 + sym, 8),
            144..=255 => (0x190 + sym - 144, 9),
            256..=279 => (sym - 256, 7),
            _ => (0xc0 + sym - 280, 8),
        }
    }

    /// Opens a final fixed-Huffman block and writes `text` as literals.
    fn fixed_block_prefix(text: &[u8]) -> BitWriter {
        let mut w = BitWriter::new();
        w.bits(1, 1); // BFINAL
        w.bits(1, 2); // BTYPE = 01, fixed Huffman
        for b in text {
            let (c, n) = fixed_litlen(u32::from(*b));
            w.code(c, n);
        }
        w
    }

    #[test]
    fn the_batched_path_reports_an_invalid_literal_length_code() {
        // Symbols 286/287 exist in the fixed tree but have no meaning:
        // `inftrees.c` gives them `lext` 68 and 193, both with bit 64 set, which
        // is the invalid marker the decoder tests.
        let mut w = fixed_block_prefix(b"batched ");
        let (c, n) = fixed_litlen(286);
        w.code(c, n);
        let data = w.finish_padded();

        let ((rc, out), msg) = run_full(15, &data);
        assert_eq!(rc, ReturnCode::DataError);
        assert_eq!(msg, BackMsg::Set("invalid literal/length code"));
        assert_eq!(
            out, b"batched ",
            "literals decoded before the bad code are still flushed"
        );
    }

    #[test]
    fn the_batched_path_reports_an_invalid_distance_code() {
        // Distance symbols 30/31 carry `dext` 64 — the same invalid marker.
        let mut w = fixed_block_prefix(b"batched ");
        let (c, n) = fixed_litlen(257); // length 3, no extra bits
        w.code(c, n);
        w.code(30, 5); // fixed distance codes are all five bits
        let data = w.finish_padded();

        let ((rc, out), msg) = run_full(15, &data);
        assert_eq!(rc, ReturnCode::DataError);
        assert_eq!(msg, BackMsg::Set("invalid distance code"));
        assert_eq!(out, b"batched ");
    }

    #[test]
    fn the_batched_path_reports_a_distance_that_reaches_too_far_back() {
        // First symbol of the stream is a match: nothing has been written and
        // `whave` is zero, so any distance at all reaches past the history.
        let mut w = fixed_block_prefix(b"");
        let (c, n) = fixed_litlen(257); // length 3
        w.code(c, n);
        w.code(0, 5); // distance code 0 => distance 1
        let data = w.finish_padded();

        let ((rc, out), msg) = run_full(15, &data);
        assert_eq!(rc, ReturnCode::DataError);
        assert_eq!(msg, BackMsg::Set("invalid distance too far back"));
        assert!(out.is_empty());
    }

    #[test]
    fn the_batched_path_is_entered_and_produces_the_same_bytes_as_the_machine() {
        // White-box: drive the block header by hand so `back_fast` can be called
        // directly, proving the batched path really is reached (a coverage claim
        // the differential tests can only imply) and that it leaves the context
        // invariants intact for the state machine to carry on from.
        let data = corpus("text", 16 * 1024);
        let stream = raw_deflate(&data, 15, 6, Strategy::Fixed);

        let mut state = inflate_back_init(15).unwrap();
        state.mode = InflateMode::Type;
        state.last = false;
        state.whave = 0;
        let wsize = state.wsize as usize;
        let mut src = SliceIn {
            data: &stream,
            done: false,
            cur: &[],
        };
        let mut sink = VecOut { data: Vec::new() };
        let mut ctx = BackCtx {
            next: 0,
            msg: None,
            hold: 0,
            bits: 0,
            put: 0,
            left: wsize,
            wsize,
            src: &mut src,
            sink: &mut sink,
        };

        // C `case TYPE` (infback.c L227-L295): consume the block header.
        do_type(&mut ctx, &mut state).expect("block header");
        assert_eq!(
            state.mode,
            InflateMode::Len,
            "the fixture's first block must be a Huffman block"
        );
        assert!(
            ctx.have() >= BACK_FAST_MIN_INPUT && ctx.left >= BACK_FAST_MIN_OUTPUT,
            "the fixture must satisfy C's `have >= 6 && left >= 258`"
        );

        back_fast(&mut ctx, &mut state);
        assert!(ctx.put > 0, "the batched path must have produced output");
        assert_eq!(
            ctx.put + ctx.left,
            ctx.wsize,
            "the put/left window invariant must survive the batch"
        );
        assert_eq!(ctx.msg, None, "a valid stream sets no diagnostic");
        assert!(
            ctx.left >= BACK_FAST_MIN_OUTPUT - 1,
            "the batch must stop while at least 257 window bytes remain"
        );

        // Hand the rest to the ordinary machine and check the whole payload.
        let ret = drive(&mut ctx, &mut state);
        assert_eq!(ret, ReturnCode::StreamEnd);
        let code = inf_leave(&mut ctx, &state, ret);
        assert_eq!(code, ReturnCode::StreamEnd);
        assert_eq!(sink.data, data);
    }

    #[test]
    fn the_batched_path_consumes_exactly_as_much_input_as_the_per_symbol_path() {
        // Trailing bytes after the final block must be left unconsumed, and both
        // decoders must agree on where the stream ended. This is what the C
        // epilogue's `in -= bits >> 3` exists for; a rewind that over- or
        // under-returns would show up here as a different consumed count.
        let data = corpus("text", 32 * 1024);
        let mut stream = raw_deflate(&data, 15, 6, Strategy::Default);
        let body = stream.len();
        stream.extend_from_slice(b"TRAILING BYTES THAT ARE NOT PART OF THE STREAM");

        let (rc, out, consumed_batched) = run_chunked(15, &stream, stream.len());
        assert_eq!(rc, ReturnCode::StreamEnd);
        assert_eq!(out, data);
        let (rc, out, consumed_per_symbol) = run_chunked(15, &stream, 5);
        assert_eq!(rc, ReturnCode::StreamEnd);
        assert_eq!(out, data);

        assert_eq!(
            consumed_batched, consumed_per_symbol,
            "the two paths must stop on the same input byte"
        );
        assert!(
            consumed_batched <= body,
            "consumed {consumed_batched} bytes but the DEFLATE stream is only {body}"
        );
    }
}
