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
//! # Relationship to `inflate_fast`
//!
//! Reference `infback.c` calls the shared `inflate_fast` hot loop when at least
//! six input and 258 output bytes are available. That routine
//! ([`crate::inflate::fast::inflate_fast`]) is written for the main driver,
//! where the output buffer and the history window are **distinct** allocations.
//! In back-inflate the window *is* the output buffer, so feeding it as both the
//! `&mut` output and the `&` history would require aliasing a single `Vec<u8>`
//! mutably and immutably at once — impossible in safe Rust, and a per-call
//! window clone would cost more than it saves. This port therefore always uses
//! the equivalent single-symbol decode path (a faithful port of `infback.c`
//! L446-L543). The emitted bytes are identical either way; the fast path is a
//! pure throughput optimization, not a correctness requirement.
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
/// state->window = window;                                 /* infback.c L60 */
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

/// Handle the `LEN` state: decode one literal/length symbol and, for a length
/// code, its distance and the resulting match copy.
///
/// Port of `infback.c` L422-L543, using the single-symbol decode path only (see
/// the module-level note on `inflate_fast`). A literal is emitted to the
/// window/output; an end-of-block returns to [`InflateMode::Type`]; a
/// length/distance pair copies the match from the window with correct ring-wrap
/// and overlapping-copy (RLE) semantics. Malformed codes transition to
/// [`InflateMode::Bad`].
///
/// # Errors
///
/// Returns `Err(ReturnCode::BufError)` if the input or output callback fails
/// while decoding or emitting.
fn do_len<I: InFunc, O: OutFunc>(
    ctx: &mut BackCtx<'_, I, O>,
    state: &mut InflateState,
) -> Result<(), ReturnCode> {
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
        // Byte-by-byte in increasing order so an overlapping copy (offset <
        // run length) propagates like C's `*put++ = *from++` (RLE); `memmove`
        // / `copy_within` semantics would be wrong here.
        for k in 0..copy {
            let byte = state.window[from + k];
            state.window[ctx.put + k] = byte;
        }
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
    /// than copying each chunk once into a buffer of its own (M4-02).
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
    /// diagnostic string (M4-01), and a clean or short stream must report the
    /// cleared field C leaves at `infback.c` L214.
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
    /// `strm->msg = Z_NULL` at L214 (M4-01).
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
    /// (`infback.c` L51-L53) runs *before* `state->window = window;` (L60), so a
    /// refused state request must return `Z_MEM_ERROR` without the caller's window
    /// ever being named, let alone written (M6-09).
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
             request has failed -- C never reaches infback.c L60"
        );
    }
}
