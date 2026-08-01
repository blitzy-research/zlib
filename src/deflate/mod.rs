//! DEFLATE compression engine — the top-level `deflate` module.
//!
//! This module is a faithful, **100% safe** Rust port of the C `deflate()`
//! driver and every public `deflate*` API function from `deflate.c`, together
//! with the zlib (RFC 1950) and gzip (RFC 1952) stream header/trailer framing.
//! It is the root of the `src/deflate/` tree: it declares the engine
//! submodules, re-exports the public deflate API, and orchestrates the block
//! producers (`stored`, `fast`, `slow`, `rle`, `huff`) through the shared
//! [`DeflateState`] and [`IoContext`].
//!
//! # Byte-exact fidelity
//!
//! The observable output of this module is **byte-identical** to reference
//! zlib for the same input, level, and strategy. The header bytes (including
//! the zlib FCHECK adjustment and every gzip header field), the empty-block
//! flush markers, and the trailers (Adler-32 for a zlib wrapper, CRC-32 plus
//! ISIZE for a gzip wrapper) are reproduced exactly. Every early-return
//! checkpoint of the C driver — where a full output buffer causes an early
//! `Z_OK` return with `last_flush == -1` — is preserved so that streaming with
//! small output buffers behaves identically.
//!
//! # Safety and portability
//!
//! There is **zero `unsafe`** anywhere in this module (and the whole
//! `deflate/` tree): manual `zcalloc`/`zcfree` allocation is replaced by owned
//! [`alloc::vec::Vec`] buffers and `Box`-owned state, and buffer accesses use
//! bounds-checked slice indexing. The module is `no_std` + `alloc`. gzip
//! framing is gated behind the `gzip` cargo feature; the crate also compiles
//! and runs with gzip disabled (raw and zlib framing only). Targets Rust 2024
//! edition, MSRV 1.85.0.
//!
//! # Idiomatic wins over C
//!
//! * `Drop` (via the owned buffers inside [`DeflateState`]) subsumes
//!   `deflateEnd`'s manual reverse-order frees; [`deflate_end`] only reports the
//!   residual `Z_DATA_ERROR`-if-busy status.
//! * `DeflateState::try_clone` subsumes `deflateCopy`'s pointer fix-ups:
//!   [`deflate_copy`] is a deep copy with no manual re-basing. It is fallible
//!   rather than a [`Clone`] impl because every working buffer is re-allocated
//!   through the **same** caller `zalloc` as the source, so an exhausted arena
//!   surfaces as `Z_MEM_ERROR` instead of silently relocating the copy into the
//!   global heap (AAP §0.6.5).
//! * The integer state tags become the [`DeflateStatus`] enum with exhaustive
//!   `match`, and the C `configuration_table` of function pointers becomes the
//!   [`CompressFunc`] enum dispatched through a `match`.

// ---------------------------------------------------------------------------
// Submodule declarations.
//
// These are declared `pub mod` because this file *is* `deflate/mod.rs` and the
// block producers reference their siblings by absolute path (for example
// `use crate::deflate::state::DeflateState;` and `use crate::deflate::trees;`),
// and `src/stream.rs` reaches `crate::deflate::state::DeflateState`.
// ---------------------------------------------------------------------------
pub mod fast;
pub mod huff;
pub mod rle;
pub mod slow;
pub mod state;
pub mod stored;
pub mod strategy;
pub mod trees;

// Public re-exports: the state/status/IoContext types the rest of the crate
// consumes, and the strategy dispatch types.
pub use state::{DeflateState, DeflateStatus, IoContext};
pub use strategy::{BlockState, CONFIGURATION_TABLE, CompressFunc};

// ---------------------------------------------------------------------------
// Imports.
// ---------------------------------------------------------------------------
use crate::checksum::adler32;
#[cfg(feature = "gzip")]
use crate::checksum::crc32;
use crate::constants::{
    DEF_MEM_LEVEL, MAX_WBITS, Strategy, WrapMode, Z_BLOCK, Z_DEFAULT_COMPRESSION, Z_DEFLATED,
    Z_FINISH, Z_FULL_FLUSH, Z_NO_FLUSH, Z_PARTIAL_FLUSH, parse_window_bits,
};
use crate::error::{ReturnCode, ZlibError};
#[cfg(feature = "gzip")]
use crate::gz_header::GzHeader;
use crate::stream::{Allocator, ZStream};

use crate::deflate::state::{BUF_SIZE, MIN_MATCH, NIL};
use crate::deflate::strategy::rank;

// ---------------------------------------------------------------------------
// Shared `zutil.h` constants.
//
// These are NOT redefined here: `crate::util` is their single canonical home,
// so the values this module writes into a stream header cannot drift from the
// values the rest of the crate publishes. That matters most for `OS_CODE`,
// which is genuinely platform-dependent — C picks it through the `#ifdef`
// cascade at `zutil.h` L98-L189 (Windows 10, Apple 19, Unix 3) — so a local
// hard-coded copy would emit the wrong gzip OS byte on any non-Unix target and
// break byte-identity with reference zlib there.
// ---------------------------------------------------------------------------

/// `PRESET_DICT` (`zutil.h` L96): the preset-dictionary flag bit set in the
/// zlib header FLG byte when the stream was primed with a dictionary. Widened
/// to `u32` at the point of use because the FLG byte is assembled in `u32`
/// arithmetic.
const PRESET_DICT: u32 = crate::util::PRESET_DICT as u32;

/// `OS_CODE` (`zutil.h` L98-L189): the operating-system byte written into the
/// gzip header, re-exported from its canonical home so the value emitted here
/// is always the one selected for the active target.
#[cfg(feature = "gzip")]
use crate::util::OS_CODE;

/// The internal `Result` alias used throughout the deflate API surface: `Ok`
/// carries a success [`ReturnCode`] (typically [`ReturnCode::Ok`] or
/// [`ReturnCode::StreamEnd`]), while `Err` carries a [`ZlibError`]. The FFI
/// boundary flattens both arms back to the integer C return codes.
type DeflateResult = Result<ReturnCode, ZlibError>;

// ===========================================================================
// DeflateConfig — the Builder replacing the multi-argument deflateInit2_.
// ===========================================================================

/// Compression parameters for initializing a deflate stream.
///
/// This is the idiomatic Rust replacement for the positional
/// `deflateInit2_(level, method, windowBits, memLevel, strategy, …)` call. It
/// is a plain builder: start from [`DeflateConfig::default`] (or
/// [`DeflateConfig::new`]) and override individual fields with the chained
/// setters, then hand it to [`DeflateConfig::init`].
///
/// ```
/// use zlib_rs::deflate::DeflateConfig;
/// use zlib_rs::constants::Strategy;
/// let mut strm = zlib_rs::stream::ZStream::new();
/// DeflateConfig::new()
///     .level(6)
///     .strategy(Strategy::Default)
///     .init(&mut strm)
///     .unwrap();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeflateConfig {
    /// Compression level: `-1` ([`Z_DEFAULT_COMPRESSION`],
    /// resolved to `6`) or `0..=9`.
    pub level: i32,
    /// Compression method — must be [`Z_DEFLATED`] (`8`).
    pub method: i32,
    /// The base-2 logarithm of the window size, overloaded to also select the
    /// wrapper: `8..=15` = zlib, `-15..=-9` = raw, `24..=31` = gzip (which
    /// requires the `gzip` cargo feature).
    ///
    /// Raw `-8` is accepted by the shared `windowBits` decoder but then rejected
    /// by the C-parity guard `windowBits == 8 && wrap != 1` (`deflate.c`
    /// L434-L436), so the accepted raw range for compression is `-15..=-9` — one
    /// value narrower than inflate's `-15..=-8`. Auto-detect (`+32`) is
    /// inflate-only.
    pub window_bits: i32,
    /// The memory level (`1..=9`, default `8`) controlling the hash-table and
    /// symbol-buffer sizes.
    pub mem_level: i32,
    /// The compression [`Strategy`].
    pub strategy: Strategy,
}

impl Default for DeflateConfig {
    fn default() -> Self {
        Self {
            level: Z_DEFAULT_COMPRESSION,
            method: Z_DEFLATED,
            window_bits: MAX_WBITS,
            mem_level: DEF_MEM_LEVEL,
            strategy: Strategy::Default,
        }
    }
}

impl DeflateConfig {
    /// Creates a configuration with the zlib default settings (level `-1`,
    /// method DEFLATE, `windowBits` 15, `memLevel` 8, default strategy).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the compression level (`-1` or `0..=9`).
    #[must_use]
    pub fn level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }

    /// Sets the compression method (must be [`Z_DEFLATED`]).
    #[must_use]
    pub fn method(mut self, method: i32) -> Self {
        self.method = method;
        self
    }

    /// Sets the `windowBits` value (also selects zlib/raw/gzip framing).
    #[must_use]
    pub fn window_bits(mut self, window_bits: i32) -> Self {
        self.window_bits = window_bits;
        self
    }

    /// Sets the memory level (`1..=9`).
    #[must_use]
    pub fn mem_level(mut self, mem_level: i32) -> Self {
        self.mem_level = mem_level;
        self
    }

    /// Sets the compression [`Strategy`].
    #[must_use]
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Initializes `strm` for compression with these parameters.
    ///
    /// Equivalent to calling [`deflate_init2`] with the individual fields; see
    /// that function for the exact validation and error semantics.
    ///
    /// # Errors
    ///
    /// Returns [`ZlibError::StreamError`] for invalid parameters and
    /// [`ZlibError::MemError`] if state allocation fails.
    pub fn init<A: Allocator>(self, strm: &mut ZStream<A>) -> DeflateResult {
        deflate_init2(
            strm,
            self.level,
            self.method,
            self.window_bits,
            self.mem_level,
            self.strategy,
        )
    }
}

// ===========================================================================
// DeflateOutcome — return value of the streaming driver.
// ===========================================================================

/// The result of a streaming [`deflate`] (or [`deflate_params`]) call.
///
/// Because [`ZStream`] carries no `next_in`/`next_out`/`avail_*` cursor fields
/// (the input and output buffers are passed as slices per call), progress is
/// reported explicitly: `consumed` bytes were read from `input` and `produced`
/// bytes were written to `output`. The FFI boundary uses these to advance the
/// caller's `z_stream` cursors. `code` is the zlib return code for the call
/// (typically [`ReturnCode::Ok`], [`ReturnCode::StreamEnd`],
/// [`ReturnCode::BufError`], or [`ReturnCode::StreamError`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeflateOutcome {
    /// The zlib return code produced by the call.
    pub code: ReturnCode,
    /// Number of input bytes consumed from the supplied `input` slice.
    pub consumed: usize,
    /// Number of output bytes written to the supplied `output` slice.
    pub produced: usize,
}

// ===========================================================================
// Small helpers.
// ===========================================================================

/// Reproduces C `putShortMSB` (`deflate.c` L939): appends a 16-bit value to
/// the pending buffer in **most-significant-byte-first** order. Used for the
/// zlib header CMF/FLG word, the preset-dictionary Adler-32, and the zlib
/// Adler-32 trailer.
fn put_short_msb(s: &mut DeflateState, b: u16) {
    s.put_byte((b >> 8) as u8);
    s.put_byte((b & 0xff) as u8);
}

/// Reproduces the C `CLEAR_HASH(s)` macro (`deflate.c` L170): resets every
/// hash-head entry to [`NIL`]. Because `NIL == 0`, clearing every entry is
/// equivalent to the C form that writes `head[hash_size - 1] = NIL` and zeroes
/// the remainder. Also clears the `slid` flag. Invoked on `Z_FULL_FLUSH` and
/// by [`deflate_params`] when leaving level 0.
fn clear_hash(s: &mut DeflateState) {
    s.head.iter_mut().for_each(|h| *h = NIL);
    s.slid = false;
}

/// Reproduces the C `HCRC_UPDATE(beg)` macro (`deflate.c` L973): when the gzip
/// header CRC is requested, folds the pending-buffer bytes in `beg..pending`
/// into the running header CRC (`io.adler`). A no-op when `hcrc` is false or
/// no new bytes were appended since `beg`.
#[cfg(feature = "gzip")]
fn hcrc_update(s: &mut DeflateState, io: &mut IoContext, hcrc: bool, beg: usize) {
    if hcrc && s.pending > beg {
        io.adler = crc32(io.adler, &s.pending_buf[beg..s.pending]);
    }
}

// ===========================================================================
// Initialization — deflateInit2_ / deflateInit_.
// ===========================================================================

/// Initializes `strm` for compression with the full parameter set.
///
/// Port of C `deflateInit2_` (`deflate.c` L387-L533), minus the FFI-only
/// version/`stream_size` guard (that check belongs to the `ffi` layer). The
/// `windowBits` value is decoded into a wrapper selector and a positive window
/// size via [`parse_window_bits`]:
///
/// * `8..=15` → zlib wrapper (`wrap = 1`)
/// * `-15..=-8` → raw DEFLATE, no wrapper (`wrap = 0`)
/// * `24..=31` → gzip wrapper (`wrap = 2`, requires the `gzip` cargo feature)
/// * `40..=47` (auto-detect) is inflate-only and rejected here
///
/// The raw `-8` that the decoder accepts is then rejected downstream by C's
/// `windowBits == 8 && wrap != 1` guard (`deflate.c` L434-L436), so the range
/// this function actually accepts for raw DEFLATE is `-15..=-9`.
///
/// All remaining parameter validation (memory level, method, window range,
/// level range, and that `windowBits == 8 && wrap != 1` rejection), the
/// `level == -1 → 6` default resolution, and the 8-bit-window bump to 9 are
/// performed by [`DeflateState::new`]. On success the freshly constructed
/// state is installed and the stream is reset (mirroring the C
/// `return deflateReset(strm);`).
///
/// # Errors
///
/// * [`ZlibError::StreamError`] — invalid `windowBits`, a gzip request without
///   the `gzip` feature, an auto-detect request, or any parameter rejected by
///   [`DeflateState::new`].
/// * [`ZlibError::MemError`] — state allocation failed.
pub fn deflate_init2<A: Allocator>(
    strm: &mut ZStream<A>,
    level: i32,
    method: i32,
    window_bits: i32,
    mem_level: i32,
    strategy: Strategy,
) -> DeflateResult {
    strm.clear_msg();

    // Decode windowBits → (WrapMode, positive w_bits).
    let (wrap_mode, w_bits) = parse_window_bits(window_bits).ok_or(ZlibError::StreamError)?;

    // Map the wrapper to the C `wrap` integer. Gzip requires the `gzip`
    // feature; auto-detect is inflate-only and invalid for deflate.
    let wrap: i32 = match wrap_mode {
        WrapMode::Zlib => 1,
        WrapMode::Raw => 0,
        #[cfg(feature = "gzip")]
        WrapMode::Gzip => 2,
        #[cfg(not(feature = "gzip"))]
        WrapMode::Gzip => return Err(ZlibError::StreamError),
        WrapMode::Auto => return Err(ZlibError::StreamError),
    };

    // `DeflateState::new_in_with` performs the full deflateInit2_ validation,
    // resolves the default level, and runs the state-side reset + lm_init.
    // The stream's `Allocator` itself is threaded through — not merely its
    // `hook()` — so the state footprint and every working buffer are requested
    // from it, with the same `(items, size)` pairs C passes `zalloc`. A custom
    // Rust allocator therefore serves engine memory, and a caller-installed
    // `zalloc`/`zfree` still backs every buffer (AAP §0.6.3 has-hook clause,
    // §0.6.5).
    let state = match DeflateState::new_in_with_detail(
        strm.allocator(),
        level,
        method,
        w_bits as i32,
        mem_level,
        strategy,
        wrap,
    ) {
        Ok(state) => state,
        Err(err) => {
            // C records a diagnostic for exactly one of its two allocation
            // failure points: the working-buffer check at `deflate.c` L505-L514
            // does `strm->msg = ERR_MSG(Z_MEM_ERROR)` before returning, while the
            // state-object check at L440-L442 returns with `strm->msg` still
            // NULL. `ZStream::set_msg` is this crate's `ERR_MSG`, so the message
            // text ("insufficient memory") comes from the same table C uses.
            if err.sets_mem_message {
                strm.set_msg(err.code.as_return_code());
            }
            return Err(err.code);
        }
    };
    strm.set_deflate_state(state);

    // Finish exactly as C does: `return deflateReset(strm);`.
    deflate_reset(strm)
}

/// Initializes `strm` for compression at the given `level` using the zlib
/// defaults (method DEFLATE, `windowBits` 15, `memLevel` 8, default strategy).
///
/// Port of C `deflateInit_` (`deflate.c` L379-L385), which forwards to
/// `deflateInit2_` with those defaults.
///
/// # Errors
///
/// See [`deflate_init2`].
pub fn deflate_init<A: Allocator>(strm: &mut ZStream<A>, level: i32) -> DeflateResult {
    deflate_init2(
        strm,
        level,
        Z_DEFLATED,
        MAX_WBITS,
        DEF_MEM_LEVEL,
        Strategy::Default,
    )
}

// ===========================================================================
// Reset — deflateResetKeep / deflateReset.
// ===========================================================================

/// Resets the stream I/O counters, checksum seed, and header state while
/// keeping the allocated buffers and configuration.
///
/// Port of C `deflateResetKeep` (`deflate.c` L644-L677). The state-side reset
/// (status, pending, `last_flush = -2`, bit buffer) and the I/O-side reset
/// (`total_in`/`total_out = 0`, `adler` seeded) are delegated to
/// [`DeflateState::reset_keep`]; because that method deliberately does **not**
/// perform the tree-frequency initialization, this function invokes
/// `trees::_tr_init` afterwards. Finally the caller-visible stream fields are
/// written back and the message is cleared.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
pub fn deflate_reset_keep<A: Allocator>(strm: &mut ZStream<A>) -> DeflateResult {
    let seed;
    {
        let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;
        // A throwaway IoContext carries the counter/checksum reset; there are
        // no real buffers to process here.
        let mut empty: [u8; 0] = [];
        let mut io = IoContext::new(&[], &mut empty);
        s.reset_keep(&mut io);
        trees::_tr_init(s);
        seed = io.adler;
    }
    strm.total_in = 0;
    strm.total_out = 0;
    strm.adler = seed;
    strm.data_type = crate::constants::DataType::Unknown.as_c_int();
    strm.clear_msg();
    Ok(ReturnCode::Ok)
}

/// Fully resets the stream, ready to begin a new compression from scratch.
///
/// Port of C `deflateReset` (`deflate.c` L704-L712): a [`deflate_reset_keep`]
/// followed by [`DeflateState::lm_init`] to re-establish the match-finder
/// window bookkeeping and per-level tuning parameters.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
pub fn deflate_reset<A: Allocator>(strm: &mut ZStream<A>) -> DeflateResult {
    deflate_reset_keep(strm)?;
    let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;
    s.lm_init();
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// Dictionary — deflateSetDictionary / deflateGetDictionary.
// ===========================================================================

/// Initializes the compression dictionary from `dictionary`.
///
/// Port of C `deflateSetDictionary` (`deflate.c` L559-L622). May be called
/// only before any data has been processed: for a zlib wrapper the header must
/// not yet have been emitted (`status == Init`), and for any wrapper the
/// look-ahead must be empty; a gzip wrapper (`wrap == 2`) cannot take a
/// dictionary. For a zlib wrapper the dictionary bytes are folded into the
/// stream's running Adler-32 (over the *full* dictionary, before any
/// truncation). If the dictionary is at least a window in size, only its
/// trailing `w_size` bytes are retained. The bytes are then slid into the
/// window and every position is inserted into the hash chains, exactly as the
/// C `fill_window` + `INSERT_STRING` loop does.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if a dictionary cannot be set in the current
/// state, or if `strm` has no deflate state installed.
pub fn deflate_set_dictionary<A: Allocator>(
    strm: &mut ZStream<A>,
    dictionary: &[u8],
) -> DeflateResult {
    // Read the running checksum out first (Copy) so we can fold the dictionary
    // into it without holding a borrow of `strm` across the state work.
    let mut running_adler = strm.adler;
    {
        let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;
        let wrap = s.wrap;

        // Cannot set a dictionary for gzip, for a zlib stream whose header was
        // already written, or once any input has entered the window.
        if wrap == 2 || (wrap == 1 && s.status != DeflateStatus::Init) || s.lookahead != 0 {
            return Err(ZlibError::StreamError);
        }

        // For a zlib wrapper compute the Adler-32 of the (full) dictionary.
        if wrap == 1 {
            running_adler = adler32(running_adler, dictionary);
        }
        // Avoid re-checksumming while sliding the dictionary through read_buf.
        s.wrap = 0;

        // If the dictionary would fill the window, keep only its tail; when
        // there was no wrapper, the (already empty) history is reset per C.
        let mut dict = dictionary;
        if dict.len() >= s.w_size {
            if wrap == 0 {
                clear_hash(s);
                s.strstart = 0;
                s.block_start = 0;
                s.insert = 0;
            }
            dict = &dict[dict.len() - s.w_size..];
        }

        // Slide the dictionary into the window and insert every position into
        // the hash chains. The IoContext presents `dict` as input with an empty
        // output; `s.wrap == 0` here so read_buf leaves the checksum untouched.
        let mut empty: [u8; 0] = [];
        let mut io = IoContext::new(dict, &mut empty);
        s.fill_window(&mut io);
        while s.lookahead >= MIN_MATCH {
            let mut str_idx = s.strstart;
            let n = s.lookahead - (MIN_MATCH - 1);
            for _ in 0..n {
                s.insert_string(str_idx);
                str_idx += 1;
            }
            s.strstart = str_idx;
            s.lookahead = MIN_MATCH - 1;
            s.fill_window(&mut io);
        }
        s.strstart += s.lookahead;
        s.block_start = s.strstart as isize;
        s.insert = s.lookahead;
        s.lookahead = 0;
        s.match_length = MIN_MATCH - 1;
        s.prev_length = MIN_MATCH - 1;
        s.match_available = false;
        s.wrap = wrap; // restore the original wrapper
    }
    strm.adler = running_adler;
    Ok(ReturnCode::Ok)
}

/// Returns a copy of the compression dictionary currently in the window and
/// its length.
///
/// Port of C `deflateGetDictionary` (`deflate.c` L625-L641). The returned
/// length is `min(strstart + lookahead, w_size)`. When `dictionary` is
/// `Some(dst)`, the most recent `len` window bytes are copied into `dst`; the
/// caller must ensure `dst` is at least the returned length. Pass `None` to
/// query only the length.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
pub fn deflate_get_dictionary<A: Allocator>(
    strm: &ZStream<A>,
    dictionary: Option<&mut [u8]>,
) -> Result<usize, ZlibError> {
    let s = strm.deflate_state().ok_or(ZlibError::StreamError)?;
    let mut len = s.strstart + s.lookahead;
    if len > s.w_size {
        len = s.w_size;
    }
    if let Some(dst) = dictionary {
        if len != 0 {
            let start = s.strstart + s.lookahead - len;
            dst[..len].copy_from_slice(&s.window[start..start + len]);
        }
    }
    Ok(len)
}

// ===========================================================================
// gzip header — deflateSetHeader.
// ===========================================================================

/// Provides the gzip header fields to be written when the stream uses gzip
/// framing.
///
/// Port of C `deflateSetHeader` (`deflate.c` L714-L719). Requires a gzip
/// wrapper (`wrap == 2`); passing `None` restores the default (all-zero) gzip
/// header. Available only when the `gzip` feature is enabled.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if the stream is not a gzip stream or has no
/// deflate state installed.
#[cfg(feature = "gzip")]
pub fn deflate_set_header<A: Allocator>(
    strm: &mut ZStream<A>,
    head: Option<GzHeader>,
) -> DeflateResult {
    let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;
    if s.wrap != 2 {
        return Err(ZlibError::StreamError);
    }
    s.gzhead = head;
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// Introspection — deflatePending / deflateUsed.
// ===========================================================================

/// Returns the number of bytes and bits of output that have been generated but
/// not yet emitted: `(pending_bytes, pending_bits)`.
///
/// Port of C `deflatePending` (`deflate.c` L722-L734). The C truncation guard
/// (which returns `Z_BUF_ERROR` when the `size_t` pending count does not fit
/// the `unsigned` out-parameter) has no analogue at this `usize`-typed
/// boundary and is applied by the FFI layer if needed.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
pub fn deflate_pending<A: Allocator>(strm: &ZStream<A>) -> Result<(usize, i32), ZlibError> {
    let s = strm.deflate_state().ok_or(ZlibError::StreamError)?;
    Ok((s.pending, s.bi_valid))
}

/// Returns the number of bits used in the last byte written to the output.
///
/// Port of C `deflateUsed` (`deflate.c` L737-L742).
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
pub fn deflate_used<A: Allocator>(strm: &ZStream<A>) -> Result<i32, ZlibError> {
    let s = strm.deflate_state().ok_or(ZlibError::StreamError)?;
    Ok(s.bi_used)
}

// ===========================================================================
// deflatePrime.
// ===========================================================================

/// Inserts `bits` low-order bits of `value` into the pending output ahead of
/// the compressed data.
///
/// Port of C `deflatePrime` (`deflate.c` L745-L771). `bits` must be in
/// `0..=16`. The room check guards against the compressed data catching up to
/// the overlaid symbol region, and is C's check verbatim: C writes
/// `s->sym_buf < s->pending_out + ((Buf_size + 7) >> 3)` (`deflate.c` L757),
/// where `sym_buf == pending_buf + lit_bufsize` and `pending_out` is also a
/// pointer into `pending_buf`. Subtracting the common `pending_buf` base — which
/// this port has already done, since both are stored as indices (AAP §0.3.2 rule
/// T3) — leaves exactly `lit_bufsize < pending_out + ((Buf_size + 7) >> 3)`.
///
/// # Errors
///
/// * [`ZlibError::BufError`] — `bits` is out of range or there is insufficient
///   head-room.
/// * [`ZlibError::StreamError`] — `strm` has no deflate state installed.
pub fn deflate_prime<A: Allocator>(strm: &mut ZStream<A>, bits: i32, value: i32) -> DeflateResult {
    let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;

    if !(0..=16).contains(&bits) || s.lit_bufsize < s.pending_out + ((BUF_SIZE as usize + 7) >> 3) {
        return Err(ZlibError::BufError);
    }

    let mut bits = bits;
    let mut value = value;
    loop {
        let mut put = BUF_SIZE - s.bi_valid;
        if put > bits {
            put = bits;
        }
        // Extract the low `put` bits of `value` and OR them in at `bi_valid`.
        // The shift is computed in `u32` and truncated to `u16`, reproducing
        // the C `(ush)(...)` truncation with no overflow panic.
        let masked = (value & ((1 << put) - 1)) as u32;
        s.bi_buf |= (masked << s.bi_valid) as u16;
        s.bi_valid += put;
        s.tr_flush_bits();
        value >>= put;
        bits -= put;
        if bits == 0 {
            break;
        }
    }
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// deflateTune.
// ===========================================================================

/// Fine-tunes the internal match-finder parameters directly.
///
/// Port of C `deflateTune` (`deflate.c` L819-L830). For advanced use; the
/// values override those loaded from the configuration table for the current
/// level.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
pub fn deflate_tune<A: Allocator>(
    strm: &mut ZStream<A>,
    good_length: i32,
    max_lazy: i32,
    nice_length: i32,
    max_chain: i32,
) -> DeflateResult {
    let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;
    s.good_match = good_length as usize;
    s.max_lazy_match = max_lazy as usize;
    s.nice_match = nice_length;
    s.max_chain_length = max_chain as usize;
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// deflateBound / deflateBound_z.
// ===========================================================================

/// Returns an upper bound on the compressed size of `source_len` input bytes
/// for the current stream configuration.
///
/// Port of C `deflateBound_z` (`deflate.c` L856-L927), reproduced formula for
/// formula so that callers who pre-allocate output buffers observe an
/// identical bound. All overflow cases (`(z_size_t)-1` in C) are reproduced
/// with saturating arithmetic, which caps at [`usize::MAX`].
///
/// * Without an installed state, the larger of the fixed-block and
///   stored-block conservative bounds plus a full 18-byte wrapper is returned.
/// * For non-default window/hash sizes, one of the two conservative bounds is
///   returned (fixed when `w_bits <= hash_bits && level != 0`, else stored),
///   plus the wrapper length.
/// * For the default `windowBits == 15` / `memLevel == 8`, the tight bound
///   `source_len + (source_len >> 12) + (source_len >> 14) + (source_len >> 25)
///   + 7 + wraplen` is returned.
///
/// `wraplen` is `0` for raw, `6 + (strstart ? 4 : 0)` for zlib, and `18` plus
/// any user gzip-header fields for gzip.
#[must_use]
pub fn deflate_bound_z<A: Allocator>(strm: &ZStream<A>, source_len: usize) -> usize {
    // Upper bound for fixed blocks with 9-bit literals (~13% + a constant).
    let fixedlen = source_len
        .saturating_add(source_len >> 3)
        .saturating_add(source_len >> 8)
        .saturating_add(source_len >> 9)
        .saturating_add(4);

    // Upper bound for stored blocks with length 127 (~4% + a constant).
    let storelen = source_len
        .saturating_add(source_len >> 5)
        .saturating_add(source_len >> 7)
        .saturating_add(source_len >> 11)
        .saturating_add(7);

    // If we cannot read parameters, return the larger bound plus a wrapper.
    let s = match strm.deflate_state() {
        Some(s) => s,
        None => return fixedlen.max(storelen).saturating_add(18),
    };

    // Compute the wrapper length from the (sign-normalized) wrap mode.
    let wrap_abs = if s.wrap < 0 { -s.wrap } else { s.wrap };
    let wraplen: usize = match wrap_abs {
        0 => 0,                                       // raw deflate
        1 => 6 + if s.strstart != 0 { 4 } else { 0 }, // zlib wrapper
        #[cfg(feature = "gzip")]
        2 => {
            // gzip wrapper: fixed 18 plus any user-supplied header fields.
            let mut w = 18usize;
            if let Some(h) = s.gzhead.as_ref() {
                if let Some(extra) = h.extra.as_ref() {
                    w += 2 + extra.len();
                }
                if let Some(name) = h.name.as_ref() {
                    w += name.len() + 1; // field bytes plus the NUL terminator
                }
                if let Some(comment) = h.comment.as_ref() {
                    w += comment.len() + 1;
                }
                if h.hcrc {
                    w += 2;
                }
            }
            w
        }
        _ => 18, // for compiler happiness (matches the C default arm)
    };

    // If not the default parameters, return a conservative bound plus wrapper.
    if s.w_bits != 15 || s.hash_bits != 15 {
        let bound = if s.w_bits <= s.hash_bits && s.level != 0 {
            fixedlen
        } else {
            storelen
        };
        return bound.saturating_add(wraplen);
    }

    // Default settings: the tight bound. C writes `+ 13 - 6` (net `+ 7`).
    source_len
        .saturating_add(source_len >> 12)
        .saturating_add(source_len >> 14)
        .saturating_add(source_len >> 25)
        .saturating_add(7)
        .saturating_add(wraplen)
}

/// Returns an upper bound on the compressed size of `source_len` input bytes.
///
/// Port of C `deflateBound` (`deflate.c` L929-L932), the classic `uLong`-typed
/// entry point. It forwards to [`deflate_bound_z`]; at this `usize`-typed
/// boundary the two are identical. The C version additionally clamps the
/// result to the `uLong` range — that clamp is a property of the C integer
/// width and is applied by the FFI layer, not here.
#[must_use]
pub fn deflate_bound<A: Allocator>(strm: &ZStream<A>, source_len: usize) -> usize {
    deflate_bound_z(strm, source_len)
}

// ===========================================================================
// The deflate() driver.
// ===========================================================================

/// The core deflate state machine, operating directly on a [`DeflateState`]
/// and an [`IoContext`].
///
/// Statement-by-statement port of C `deflate()` (`deflate.c` L981-L1290),
/// excluding the outer state-presence and flush-range validation (performed by
/// the public [`deflate`] wrapper). It advances through the header stages
/// (zlib or gzip), dispatches the appropriate block producer, and — on
/// `Z_FINISH` — writes the trailer. Every early-return checkpoint of the C
/// driver is reproduced so that streaming with a small output buffer behaves
/// identically.
fn deflate_run(s: &mut DeflateState, io: &mut IoContext, flush: i32) -> ReturnCode {
    // The status/flush combination and output availability are checked first
    // (the null-pointer checks of C are unrepresentable with safe slices).
    if s.status == DeflateStatus::Finish && flush != Z_FINISH {
        return ReturnCode::StreamError;
    }
    if io.avail_out == 0 {
        return ReturnCode::BufError;
    }

    let old_flush = s.last_flush;
    s.last_flush = flush;

    // Flush as much pending output as possible.
    if s.pending != 0 {
        s.flush_pending(io);
        if io.avail_out == 0 {
            // Return OK (not BUF_ERROR) so the next call with more output space
            // is not mistaken for a useless repeat.
            s.last_flush = -1;
            return ReturnCode::Ok;
        }
    } else if io.avail_in == 0 && rank(flush) <= rank(old_flush) && flush != Z_FINISH {
        // Avoid duplicate consecutive flushes; keep returning STREAM_END rather
        // than BUF_ERROR for repeated Z_FINISH.
        return ReturnCode::BufError;
    }

    // No more input is permitted after the first Z_FINISH.
    if s.status == DeflateStatus::Finish && io.avail_in != 0 {
        return ReturnCode::BufError;
    }

    // ---------------------------- zlib header ----------------------------
    if s.status == DeflateStatus::Init && s.wrap == 0 {
        s.status = DeflateStatus::Busy;
    }
    if s.status == DeflateStatus::Init {
        // CMF/FLG: method + window size, then level flags and the preset bit.
        let mut header: u32 = (Z_DEFLATED as u32 + ((s.w_bits - 8) << 4)) << 8;
        let level_flags: u32 =
            if s.strategy.as_c_int() >= Strategy::HuffmanOnly.as_c_int() || s.level < 2 {
                0
            } else if s.level < 6 {
                1
            } else if s.level == 6 {
                2
            } else {
                3
            };
        header |= level_flags << 6;
        if s.strstart != 0 {
            header |= PRESET_DICT;
        }
        // FCHECK: make the 16-bit header a multiple of 31.
        header += 31 - (header % 31);
        put_short_msb(s, header as u16);

        // Emit the Adler-32 of the preset dictionary, if one was set.
        if s.strstart != 0 {
            put_short_msb(s, (io.adler >> 16) as u16);
            put_short_msb(s, (io.adler & 0xffff) as u16);
        }
        // The stream checksum now covers only the uncompressed data.
        io.adler = DeflateState::initial_adler(s.wrap);
        s.status = DeflateStatus::Busy;

        // Compression must start with an empty pending buffer.
        s.flush_pending(io);
        if s.pending != 0 {
            s.last_flush = -1;
            return ReturnCode::Ok;
        }
    }

    // ---------------------------- gzip header ----------------------------
    #[cfg(feature = "gzip")]
    {
        if s.status == DeflateStatus::Gzip {
            // The gzip header checksum is a CRC-32 over the header bytes.
            io.adler = crc32(0, &[]);
            s.put_byte(31);
            s.put_byte(139);
            s.put_byte(8);

            // Extract the header fields as owned/Copy values so `s` can be
            // mutated freely below without holding a borrow of `s.gzhead`.
            let head_info = s.gzhead.as_ref().map(|h| {
                (
                    h.text,
                    h.hcrc,
                    h.extra.is_some(),
                    h.name.is_some(),
                    h.comment.is_some(),
                    h.time,
                    h.os,
                    h.extra.as_ref().map_or(0usize, |e| e.len()),
                )
            });
            let xfl: u8 = if s.level == 9 {
                2
            } else if s.strategy.as_c_int() >= Strategy::HuffmanOnly.as_c_int() || s.level < 2 {
                4
            } else {
                0
            };

            match head_info {
                None => {
                    // Default header: zero MTIME/flags, XFL, then OS.
                    s.put_byte(0);
                    s.put_byte(0);
                    s.put_byte(0);
                    s.put_byte(0);
                    s.put_byte(0);
                    s.put_byte(xfl);
                    s.put_byte(OS_CODE);
                    s.status = DeflateStatus::Busy;
                    s.flush_pending(io);
                    if s.pending != 0 {
                        s.last_flush = -1;
                        return ReturnCode::Ok;
                    }
                }
                Some((text, hcrc, has_extra, has_name, has_comment, time, os, extra_len)) => {
                    let flag: u8 = (if text { 1 } else { 0 })
                        + (if hcrc { 2 } else { 0 })
                        + (if has_extra { 4 } else { 0 })
                        + (if has_name { 8 } else { 0 })
                        + (if has_comment { 16 } else { 0 });
                    s.put_byte(flag);
                    s.put_byte((time & 0xff) as u8);
                    s.put_byte(((time >> 8) & 0xff) as u8);
                    s.put_byte(((time >> 16) & 0xff) as u8);
                    s.put_byte(((time >> 24) & 0xff) as u8);
                    s.put_byte(xfl);
                    s.put_byte((os & 0xff) as u8);
                    if has_extra {
                        // The 2-byte little-endian XLEN (low 16 bits).
                        s.put_byte((extra_len & 0xff) as u8);
                        s.put_byte(((extra_len >> 8) & 0xff) as u8);
                    }
                    if hcrc {
                        io.adler = crc32(io.adler, &s.pending_buf[..s.pending]);
                    }
                    s.gzindex = 0;
                    s.status = DeflateStatus::Extra;
                }
            }
        }

        if s.status == DeflateStatus::Extra {
            // Clone the "extra" bytes so the pending buffer (a field of `s`) can
            // be mutated while we read the source data.
            if let Some(extra) = s.gzhead.as_ref().and_then(|h| h.extra.clone()) {
                let gz_hcrc = s.gzhead.as_ref().is_some_and(|h| h.hcrc);
                let extra_len = extra.len() & 0xffff;
                let mut beg = s.pending;
                let mut left = extra_len - s.gzindex;
                while s.pending + left > s.pending_buf_size {
                    let copy = s.pending_buf_size - s.pending;
                    s.pending_buf[s.pending..s.pending + copy]
                        .copy_from_slice(&extra[s.gzindex..s.gzindex + copy]);
                    s.pending = s.pending_buf_size;
                    hcrc_update(s, io, gz_hcrc, beg);
                    s.gzindex += copy;
                    s.flush_pending(io);
                    if s.pending != 0 {
                        s.last_flush = -1;
                        return ReturnCode::Ok;
                    }
                    beg = 0;
                    left -= copy;
                }
                s.pending_buf[s.pending..s.pending + left]
                    .copy_from_slice(&extra[s.gzindex..s.gzindex + left]);
                s.pending += left;
                hcrc_update(s, io, gz_hcrc, beg);
                s.gzindex = 0;
            }
            s.status = DeflateStatus::Name;
        }

        if s.status == DeflateStatus::Name {
            if let Some(name) = s.gzhead.as_ref().and_then(|h| h.name.clone()) {
                let gz_hcrc = s.gzhead.as_ref().is_some_and(|h| h.hcrc);
                let mut beg = s.pending;
                loop {
                    if s.pending == s.pending_buf_size {
                        hcrc_update(s, io, gz_hcrc, beg);
                        s.flush_pending(io);
                        if s.pending != 0 {
                            s.last_flush = -1;
                            return ReturnCode::Ok;
                        }
                        beg = 0;
                    }
                    // C reads bytes (including the C-string NUL) until it hits
                    // 0. Our stored name has no NUL, so emit a 0 past its end.
                    let val = if s.gzindex < name.len() {
                        name[s.gzindex]
                    } else {
                        0
                    };
                    s.gzindex += 1;
                    s.put_byte(val);
                    if val == 0 {
                        break;
                    }
                }
                hcrc_update(s, io, gz_hcrc, beg);
                s.gzindex = 0;
            }
            s.status = DeflateStatus::Comment;
        }

        if s.status == DeflateStatus::Comment {
            if let Some(comment) = s.gzhead.as_ref().and_then(|h| h.comment.clone()) {
                let gz_hcrc = s.gzhead.as_ref().is_some_and(|h| h.hcrc);
                let mut beg = s.pending;
                loop {
                    if s.pending == s.pending_buf_size {
                        hcrc_update(s, io, gz_hcrc, beg);
                        s.flush_pending(io);
                        if s.pending != 0 {
                            s.last_flush = -1;
                            return ReturnCode::Ok;
                        }
                        beg = 0;
                    }
                    let val = if s.gzindex < comment.len() {
                        comment[s.gzindex]
                    } else {
                        0
                    };
                    s.gzindex += 1;
                    s.put_byte(val);
                    if val == 0 {
                        break;
                    }
                }
                hcrc_update(s, io, gz_hcrc, beg);
            }
            s.status = DeflateStatus::Hcrc;
        }

        if s.status == DeflateStatus::Hcrc {
            let gz_hcrc = s.gzhead.as_ref().is_some_and(|h| h.hcrc);
            if gz_hcrc {
                if s.pending + 2 > s.pending_buf_size {
                    s.flush_pending(io);
                    if s.pending != 0 {
                        s.last_flush = -1;
                        return ReturnCode::Ok;
                    }
                }
                s.put_byte((io.adler & 0xff) as u8);
                s.put_byte(((io.adler >> 8) & 0xff) as u8);
                io.adler = crc32(0, &[]);
            }
            s.status = DeflateStatus::Busy;

            // Compression must start with an empty pending buffer.
            s.flush_pending(io);
            if s.pending != 0 {
                s.last_flush = -1;
                return ReturnCode::Ok;
            }
        }
    }

    // ------------------------- block production --------------------------
    if io.avail_in != 0
        || s.lookahead != 0
        || (flush != Z_NO_FLUSH && s.status != DeflateStatus::Finish)
    {
        // Dispatch precedence (deflate.c L1217-L1220): level 0 forces stored
        // (overriding strategy); then Huffman-only, then RLE, then the
        // per-level configured producer.
        let cf = if s.level == 0 {
            CompressFunc::Stored
        } else if s.strategy == Strategy::HuffmanOnly {
            CompressFunc::Huff
        } else if s.strategy == Strategy::Rle {
            CompressFunc::Rle
        } else {
            CONFIGURATION_TABLE[s.level as usize].func
        };
        let bstate = match cf {
            CompressFunc::Stored => stored::deflate_stored(s, io, flush),
            CompressFunc::Fast => fast::deflate_fast(s, io, flush),
            CompressFunc::Slow => slow::deflate_slow(s, io, flush),
            CompressFunc::Rle => rle::deflate_rle(s, io, flush),
            CompressFunc::Huff => huff::deflate_huff(s, io, flush),
        };

        if bstate == BlockState::FinishStarted || bstate == BlockState::FinishDone {
            s.status = DeflateStatus::Finish;
        }
        if bstate == BlockState::NeedMore || bstate == BlockState::FinishStarted {
            if io.avail_out == 0 {
                s.last_flush = -1; // avoid BUF_ERROR on the next call
            }
            // For a partial block with no output room, the next call reuses the
            // same flush parameter to complete it; no empty block is emitted
            // here, bounding tiny-buffer output to at most one empty block.
            return ReturnCode::Ok;
        }
        if bstate == BlockState::BlockDone {
            if flush == Z_PARTIAL_FLUSH {
                trees::_tr_align(s);
            } else if flush != Z_BLOCK {
                // FULL_FLUSH or SYNC_FLUSH: emit an empty stored block. For a
                // full flush inflate_sync() treats this as a resync marker.
                trees::_tr_stored_block(s, None, 0, false);
                if flush == Z_FULL_FLUSH {
                    clear_hash(s); // forget the match history
                    if s.lookahead == 0 {
                        s.strstart = 0;
                        s.block_start = 0;
                        s.insert = 0;
                    }
                }
            }
            s.flush_pending(io);
            if io.avail_out == 0 {
                s.last_flush = -1;
                return ReturnCode::Ok;
            }
        }
    }

    if flush != Z_FINISH {
        return ReturnCode::Ok;
    }
    if s.wrap <= 0 {
        return ReturnCode::StreamEnd;
    }

    // ------------------------------ trailer ------------------------------
    if s.wrap == 2 {
        // gzip: CRC-32 then ISIZE (total_in mod 2^32), both least-significant
        // byte first.
        s.put_byte((io.adler & 0xff) as u8);
        s.put_byte(((io.adler >> 8) & 0xff) as u8);
        s.put_byte(((io.adler >> 16) & 0xff) as u8);
        s.put_byte(((io.adler >> 24) & 0xff) as u8);
        s.put_byte((io.total_in & 0xff) as u8);
        s.put_byte(((io.total_in >> 8) & 0xff) as u8);
        s.put_byte(((io.total_in >> 16) & 0xff) as u8);
        s.put_byte(((io.total_in >> 24) & 0xff) as u8);
    } else {
        // zlib: Adler-32, most-significant 16 bits first.
        put_short_msb(s, (io.adler >> 16) as u16);
        put_short_msb(s, (io.adler & 0xffff) as u16);
    }
    s.flush_pending(io);
    // Negate wrap so the trailer is written only once.
    if s.wrap > 0 {
        s.wrap = -s.wrap;
    }
    if s.pending != 0 {
        ReturnCode::Ok
    } else {
        ReturnCode::StreamEnd
    }
}

/// Compresses `input` into `output`, advancing the stream by one call.
///
/// This is the streaming entry point, the idiomatic form of C `deflate()`.
/// Because [`ZStream`] carries no cursor fields, the input and output buffers
/// are supplied per call and progress is returned in a [`DeflateOutcome`]
/// (`consumed` input bytes, `produced` output bytes, and the zlib `code`). The
/// running totals, checksum, and detected data type are read from and written
/// back to `strm` on every return path.
///
/// `flush` is one of the raw flush constants
/// [`Z_NO_FLUSH`] ..=
/// [`Z_BLOCK`]; a value outside `0..=5` yields
/// [`ReturnCode::StreamError`].
#[must_use]
pub fn deflate<A: Allocator>(
    strm: &mut ZStream<A>,
    input: &[u8],
    output: &mut [u8],
    flush: i32,
) -> DeflateOutcome {
    // Flush range check (C: `flush > Z_BLOCK || flush < 0`).
    if !(0..=Z_BLOCK).contains(&flush) {
        return DeflateOutcome {
            code: ReturnCode::StreamError,
            consumed: 0,
            produced: 0,
        };
    }

    // Build the IoContext seeded from the stream's running totals + checksum.
    let mut io = IoContext::new(input, output);
    io.total_in = strm.total_in;
    io.total_out = strm.total_out;
    io.adler = strm.adler;

    let code = match strm.deflate_state_mut() {
        Some(s) => deflate_run(s, &mut io, flush),
        None => ReturnCode::StreamError,
    };

    // Write back stream-visible state on every path (mirrors z_stream fields).
    strm.total_in = io.total_in;
    strm.total_out = io.total_out;
    strm.adler = io.adler;
    if let Some(dt) = strm.deflate_state().map(|s| s.data_type.as_c_int()) {
        strm.data_type = dt;
    }

    DeflateOutcome {
        code,
        consumed: io.next_in,
        produced: io.next_out,
    }
}

// ===========================================================================
// deflateParams.
// ===========================================================================

/// Resolves a caller-supplied compression level the way C's `deflateParams`
/// does, returning [`None`] when the level is out of range.
///
/// Mirrors the non-`FASTEST` branch of `deflate.c`:
///
/// ```c
/// if (level == Z_DEFAULT_COMPRESSION) level = 6;
/// if (level < 0 || level > 9 || strategy < 0 || strategy > Z_FIXED)
///     return Z_STREAM_ERROR;
/// ```
///
/// The strategy half of that test is enforced by construction here — a
/// [`Strategy`] value cannot be out of range — so only the level needs
/// resolving. Resolution is idempotent for every accepted level, so passing an
/// already-resolved value through a second time is harmless.
fn params_resolve_level(level: i32) -> Option<i32> {
    let level = if level == Z_DEFAULT_COMPRESSION {
        6
    } else {
        level
    };
    (0..=9).contains(&level).then_some(level)
}

/// Reports whether a [`deflate_params`] call with these arguments would perform
/// C's internal `deflate(strm, Z_BLOCK)` pre-flush.
///
/// This is the single source of truth for that condition — [`deflate_params`]
/// itself calls it — and it exists because the C ABI boundary needs the answer
/// *before* it bridges the caller's raw `next_in`/`next_out` pointers into
/// slices. C validates those buffers only where it actually touches them,
/// namely inside the internal `deflate` call, and that call is made only when
/// the following all hold (`deflate.c`):
///
/// ```c
/// func = configuration_table[s->level].func;
/// if ((strategy != s->strategy || func != configuration_table[level].func) &&
///     s->last_flush != -2) { ... deflate(strm, Z_BLOCK) ... }
/// ```
///
/// So a level/strategy change that keeps the same block producer, or one made
/// before any `deflate` call has run (`last_flush == -2`, the value
/// `deflateReset` installs), is a pure bookkeeping update: reference zlib
/// returns `Z_OK` without reading or writing a single byte, and therefore
/// without caring whether `next_out` is null. Validating the buffers
/// unconditionally would reject those calls with `Z_STREAM_ERROR` and silently
/// drop the parameter change — an ABI-parity break, and a byte-identity break
/// for any stream whose level was raised mid-flight.
///
/// Returns `false` — never panics, never indexes out of bounds — when the level
/// is out of range or no deflate state is installed. Both cases make
/// [`deflate_params`] return [`ReturnCode::StreamError`] on its own, which is
/// exactly what C's `deflateStateCheck`/range test do before the pre-flush is
/// ever considered, so the buffers are irrelevant there too.
#[must_use]
pub fn deflate_params_flushes<A: Allocator>(
    strm: &ZStream<A>,
    level: i32,
    strategy: Strategy,
) -> bool {
    let Some(level) = params_resolve_level(level) else {
        return false;
    };
    let Some(s) = strm.deflate_state() else {
        return false;
    };
    // `last_flush == -2` is the sentinel `deflateReset` installs, meaning "no
    // `deflate` call has happened yet", so there is no block to flush.
    if s.last_flush == -2 {
        return false;
    }
    // `s.level` is always within `0..=9` for a live state (validated at init and
    // by this very function on every change), but read it fallibly so a
    // hypothetically corrupt level can never index out of bounds.
    let Some(cur) = CONFIGURATION_TABLE.get(s.level as usize) else {
        return false;
    };
    strategy != s.strategy || cur.func != CONFIGURATION_TABLE[level as usize].func
}

/// Dynamically updates the compression `level` and `strategy` mid-stream.
///
/// Port of C `deflateParams` (`deflate.c` L774-L816). When the change would
/// alter the active block producer and some data has already been processed
/// (`last_flush != -2`), the pending block is first flushed via an internal
/// [`deflate`] call with [`Z_BLOCK`] — hence the
/// `input`/`output` buffers are required here too. If input remains, or the
/// window still holds unflushed data after that flush, the switch cannot
/// complete and [`ReturnCode::BufError`] is returned. Switching *away* from
/// level 0 slides or clears the hash table as C does.
///
/// The returned [`DeflateOutcome`] carries any I/O performed by the internal
/// flush (`consumed`/`produced` are `0` when no flush was needed).
///
/// Callers that must know *in advance* whether this call will perform that
/// internal flush — the C ABI shim, which has to decide whether the raw
/// `next_in`/`next_out` pointers are even relevant — should ask
/// [`deflate_params_flushes`]; it evaluates the identical condition.
#[must_use]
pub fn deflate_params<A: Allocator>(
    strm: &mut ZStream<A>,
    input: &[u8],
    output: &mut [u8],
    level: i32,
    strategy: Strategy,
) -> DeflateOutcome {
    // Resolve default level and validate the range (strategy is validated by
    // construction of the `Strategy` enum).
    let Some(level) = params_resolve_level(level) else {
        return DeflateOutcome {
            code: ReturnCode::StreamError,
            consumed: 0,
            produced: 0,
        };
    };

    if strm.deflate_state().is_none() {
        return DeflateOutcome {
            code: ReturnCode::StreamError,
            consumed: 0,
            produced: 0,
        };
    }

    let mut consumed = 0usize;
    let mut produced = 0usize;

    // The pre-flush condition lives in ONE place (`deflate_params_flushes`) so
    // this engine path and the C ABI shim's buffer validation can never drift.
    if deflate_params_flushes(strm, level, strategy) {
        // Flush the current block before switching producers.
        let outcome = deflate(strm, input, output, Z_BLOCK);
        consumed = outcome.consumed;
        produced = outcome.produced;
        if outcome.code == ReturnCode::StreamError {
            return outcome;
        }
        let leftover_in = input.len() - consumed;
        let window_pending = match strm.deflate_state() {
            Some(s) => (s.strstart as isize - s.block_start) as usize + s.lookahead,
            None => {
                return DeflateOutcome {
                    code: ReturnCode::StreamError,
                    consumed,
                    produced,
                };
            }
        };
        if leftover_in != 0 || window_pending != 0 {
            return DeflateOutcome {
                code: ReturnCode::BufError,
                consumed,
                produced,
            };
        }
    }

    // Apply the change.
    let s = match strm.deflate_state_mut() {
        Some(s) => s,
        None => {
            return DeflateOutcome {
                code: ReturnCode::StreamError,
                consumed,
                produced,
            };
        }
    };
    if s.level != level {
        if s.level == 0 && s.matches != 0 {
            // Leaving level 0: reconcile the hash tables with the data that was
            // stored (matches == 1 → slide, otherwise clear).
            if s.matches == 1 {
                s.slide_hash();
            } else {
                clear_hash(s);
            }
            s.matches = 0;
        }
        s.level = level;
        let cfg = &CONFIGURATION_TABLE[level as usize];
        s.max_lazy_match = cfg.max_lazy as usize;
        s.good_match = cfg.good_length as usize;
        s.nice_match = cfg.nice_length as i32;
        s.max_chain_length = cfg.max_chain as usize;
    }
    s.strategy = strategy;

    DeflateOutcome {
        code: ReturnCode::Ok,
        consumed,
        produced,
    }
}

// ===========================================================================
// Teardown & copy — deflateEnd / deflateCopy.
// ===========================================================================

/// Frees the compression state associated with `strm`.
///
/// Port of C `deflateEnd` (`deflate.c` L1293-L1310). Thanks to RAII, dropping
/// the boxed [`DeflateState`] frees its owned buffers (window, hash tables,
/// pending/symbol buffers) automatically in reverse allocation order — the
/// explicit `TRY_FREE` sequence of C is unnecessary. The observable return
/// code is preserved: [`ZlibError::DataError`] if the stream was still busy
/// (freed prematurely), otherwise [`ReturnCode::Ok`].
///
/// # Errors
///
/// * [`ZlibError::StreamError`] — `strm` has no deflate state installed.
/// * [`ZlibError::DataError`] — the stream was mid-compression (`Busy`).
pub fn deflate_end<A: Allocator>(strm: &mut ZStream<A>) -> DeflateResult {
    let status = match strm.deflate_state() {
        Some(s) => s.status,
        None => return Err(ZlibError::StreamError),
    };
    // Dropping the state frees all owned buffers (RAII replaces TRY_FREE).
    strm.clear_state();
    if status == DeflateStatus::Busy {
        Err(ZlibError::DataError)
    } else {
        Ok(ReturnCode::Ok)
    }
}

/// Copies the compression state of `source` into `dest`, producing an
/// independent stream that continues identically.
///
/// Port of C `deflateCopy` (`deflate.c` L1317-L1377). Because [`DeflateState`]
/// holds its buffers as owned [`AllocBuffer`](crate::stream::AllocBuffer)s (and
/// its tree arrays are `Copy`), a deep copy is a single
/// [`DeflateState::try_clone`] with no manual pointer re-basing — the large
/// `zmemcpy`/pointer-fix-up body of C collapses away. The stream-level
/// bookkeeping (totals, checksum, data type, message) is copied to match C's
/// whole-`z_stream` copy.
///
/// The copy is allocator-preserving: all six buffers — the state-object
/// reservation mirroring C's `ZALLOC(strm, 1, sizeof(deflate_state))`
/// (`deflate.c` L1330-L1333) plus the five working buffers — are re-allocated
/// through the **same** `AllocHook` as the source, so a caller who installed a
/// custom arena does not find the copy living in the global heap, and the
/// caller observes the same request count C issues (AAP §0.6.5).
/// The copy is fallible for the same reason C's is: the state object and each
/// buffer are re-allocated through the allocator that backs them — via the
/// state's internal `try_copy` helper, which combines the field-by-field
/// [`DeflateState::try_clone`] with a fallible box allocation — so a
/// caller-supplied `zalloc` reporting out-of-memory aborts the copy with
/// [`ZlibError::MemError`] instead of silently producing a destination backed by
/// different storage (`deflate.c` L1348-L1350; AAP §0.6.3, §0.6.5). `dest` is
/// left untouched in that case, matching C's `deflateEnd(dest)` teardown before
/// it returns. An infallible [`Clone`] could not express that failure, which is
/// why neither the state nor its buffers implement it.
/// # Errors
///
/// [`ZlibError::StreamError`] if `source` has no deflate state installed, or
/// [`ZlibError::MemError`] when an active caller allocator reports
/// out-of-memory while copying a working buffer. In the latter case `dest` is
/// left completely untouched — C reaches the equivalent state by calling
/// `deflateEnd(dest)` before `return Z_MEM_ERROR`, which likewise leaves the
/// destination with no usable state.
pub fn deflate_copy<A: Allocator>(dest: &mut ZStream<A>, source: &ZStream<A>) -> DeflateResult {
    // Perform the whole deep copy BEFORE touching `dest`, so an active
    // allocator's OOM leaves the destination stream unmodified.
    let cloned = {
        let src = source.deflate_state().ok_or(ZlibError::StreamError)?;
        // C `deflateCopy` `zmemcpy`s the whole `z_stream`, so the destination
        // inherits the source's allocator; allocate the copy's buffers through
        // `dest`'s allocator, which the FFI shim has already made a duplicate of
        // the source's (AAP §0.6.3, §0.6.5).
        src.try_copy_in(dest.allocator())
            .ok_or(ZlibError::MemError)?
    };
    dest.total_in = source.total_in;
    dest.total_out = source.total_out;
    dest.adler = source.adler;
    dest.data_type = source.data_type;
    dest.msg = source.msg;
    dest.set_deflate_state(cloned);
    Ok(ReturnCode::Ok)
}

// ===========================================================================
// Unit tests.
//
// These pin the decision surfaces this module owns *exclusively* — the ones no
// sibling file covers: the zlib (RFC 1950) and gzip (RFC 1952) framing bytes,
// the wrapper-length and bound arithmetic, the bit-priming helpers, and the
// `level == 0` / strategy dispatch precedence of `deflate.c` L1217-L1220.
//
// Every expectation is transcribed independently from the C oracle
// (`deflate.c`) or derived from the RFCs — never from this module's own output.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// C `deflateBound_z`'s fixed-block bound (`deflate.c` L864-L868),
    /// transcribed independently of the implementation under test.
    fn c_fixedlen(source_len: usize) -> usize {
        source_len + (source_len >> 3) + (source_len >> 8) + (source_len >> 9) + 4
    }

    /// C `deflateBound_z`'s stored-block bound (`deflate.c` L871-L875).
    fn c_storelen(source_len: usize) -> usize {
        source_len + (source_len >> 5) + (source_len >> 7) + (source_len >> 11) + 7
    }

    /// C `deflateBound_z`'s tight default bound (`deflate.c` L925-L926). The C
    /// expression is written `+ 13 - 6 + wraplen`, kept verbatim here so the
    /// port's folded `+ 7` is checked against the original form.
    fn c_tight(source_len: usize, wraplen: usize) -> usize {
        source_len + (source_len >> 12) + (source_len >> 14) + (source_len >> 25) + 13 - 6 + wraplen
    }

    /// Initializes a deflate stream, panicking if the parameters are rejected.
    fn init(level: i32, window_bits: i32, mem_level: i32, strategy: Strategy) -> ZStream {
        let mut strm: ZStream = ZStream::new();
        deflate_init2(
            &mut strm,
            level,
            Z_DEFLATED,
            window_bits,
            mem_level,
            strategy,
        )
        .expect("deflate_init2 must accept these parameters");
        strm
    }

    /// Compresses `data` in a single [`deflate`] call with a bound-sized output
    /// buffer and returns the complete stream (header, payload, and trailer).
    fn compress_all(level: i32, window_bits: i32, strategy: Strategy, data: &[u8]) -> Vec<u8> {
        let mut strm = init(level, window_bits, DEF_MEM_LEVEL, strategy);
        let mut out = vec![0u8; deflate_bound(&strm, data.len()) + 64];
        let outcome = deflate(&mut strm, data, &mut out, Z_FINISH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "one deflate(Z_FINISH) into a bound-sized buffer must finish the stream"
        );
        assert_eq!(outcome.consumed, data.len(), "all input must be consumed");
        out.truncate(outcome.produced);
        deflate_end(&mut strm).expect("deflate_end must succeed after Z_STREAM_END");
        out
    }

    /// Decompresses a **complete** zlib stream with the crate's own inflate
    /// engine, asserting `Z_STREAM_END` and that the whole stream was consumed.
    ///
    /// Decoding in-crate keeps these tests free of `std` and of any third-party
    /// codec, so they run in every feature configuration.
    fn inflate_all(stream: &[u8], expect_len: usize) -> Vec<u8> {
        let mut strm: ZStream = ZStream::new();
        crate::inflate::inflate_init2(&mut strm, MAX_WBITS).expect("inflate_init2");
        let mut out = vec![0u8; expect_len + 64];
        let r = crate::inflate::inflate(&mut strm, stream, &mut out, Z_NO_FLUSH);
        assert_eq!(
            r.code,
            ReturnCode::StreamEnd,
            "the stream must decode fully"
        );
        assert_eq!(
            r.consumed,
            stream.len(),
            "Z_STREAM_END must leave no stream bytes unread"
        );
        out.truncate(r.produced);
        crate::inflate::inflate_end(&mut strm).expect("inflate_end");
        out
    }

    /// Decompresses an **unfinished** stream prefix, asserting `Z_OK` — the
    /// stream is not over — and returning everything the decoder could produce.
    fn inflate_prefix(prefix: &[u8], room: usize) -> Vec<u8> {
        let mut strm: ZStream = ZStream::new();
        crate::inflate::inflate_init2(&mut strm, MAX_WBITS).expect("inflate_init2");
        let mut out = vec![0u8; room + 64];
        let r = crate::inflate::inflate(&mut strm, prefix, &mut out, Z_NO_FLUSH);
        assert_eq!(
            r.code,
            ReturnCode::Ok,
            "an unfinished prefix must report Z_OK, not {:?}",
            r.code
        );
        out.truncate(r.produced);
        crate::inflate::inflate_end(&mut strm).expect("inflate_end");
        out
    }

    /// Compresses `data` in one call with `flush`, into a generously sized
    /// buffer, and returns `(emitted bytes, bi_valid after the call)`.
    ///
    /// `bi_valid` is the number of bits still held in the bit buffer, i.e. the
    /// direct observation of whether the emitted prefix is byte-aligned.
    fn flush_once(flush: i32, data: &[u8]) -> (Vec<u8>, i32) {
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        let mut out = vec![0u8; deflate_bound(&strm, data.len()) + 512];
        let outcome = deflate(&mut strm, data, &mut out, flush);
        assert_eq!(
            outcome.code,
            ReturnCode::Ok,
            "a non-final flush into a bound-sized buffer must return Z_OK"
        );
        assert_eq!(outcome.consumed, data.len(), "all input must be consumed");
        let bits = strm.deflate_state().expect("state").bi_valid;
        out.truncate(outcome.produced);
        // C `deflateEnd` reports Z_DATA_ERROR when it tears down a stream still
        // in BUSY_STATE, which is exactly where a non-final flush leaves it.
        assert_eq!(
            deflate_end(&mut strm),
            Err(ZlibError::DataError),
            "ending a stream that a non-final flush left BUSY must report \
             Z_DATA_ERROR"
        );
        (out, bits)
    }

    /// Without an installed state, C returns the larger of the two conservative
    /// bounds plus a full 18-byte wrapper (`deflate.c` L877-L878), and
    /// `deflateBound` forwards to `deflateBound_z` unchanged.
    #[test]
    fn deflate_bound_without_a_state_returns_the_larger_bound_plus_18() {
        let strm: ZStream = ZStream::new();
        for len in [0usize, 1, 3, 100, 1024, 200_000] {
            let expected = c_fixedlen(len).max(c_storelen(len)) + 18;
            assert_eq!(
                deflate_bound_z(&strm, len),
                expected,
                "no-state bound for len {len}"
            );
            assert_eq!(
                deflate_bound(&strm, len),
                expected,
                "deflate_bound must forward"
            );
        }
    }

    /// With the default `windowBits == 15` and `memLevel == 8`, C takes the
    /// tight bound and adds the wrapper length: 6 for zlib, 0 for raw
    /// (`deflate.c` L893-L896, L925-L926).
    #[test]
    fn deflate_bound_uses_the_tight_default_bound_plus_the_wrapper() {
        let zlib = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        let raw = init(6, -MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        for len in [0usize, 1, 1024, 200_000, 5_000_000] {
            assert_eq!(
                deflate_bound_z(&zlib, len),
                c_tight(len, 6),
                "zlib bound for len {len}"
            );
            assert_eq!(
                deflate_bound_z(&raw, len),
                c_tight(len, 0),
                "raw bound for len {len}"
            );
        }

        // The bound must actually bound: real output has to fit inside it.
        const DATA: &[u8] = b"the quick brown fox jumps over the lazy dog";
        let compressed = compress_all(6, MAX_WBITS, Strategy::Default, DATA);
        assert!(
            compressed.len() <= deflate_bound_z(&zlib, DATA.len()),
            "compressed length {} exceeded the bound",
            compressed.len()
        );
    }

    /// Non-default window or hash sizes take a conservative bound: fixed when
    /// `w_bits <= hash_bits && level != 0`, stored otherwise (`deflate.c`
    /// L917-L923). `hash_bits == memLevel + 7`, so `memLevel 8` gives 15.
    #[test]
    fn deflate_bound_uses_a_conservative_bound_for_non_default_parameters() {
        // windowBits 9: w_bits 9 <= hash_bits 15 and level != 0 -> fixed.
        let small_window = init(6, 9, DEF_MEM_LEVEL, Strategy::Default);
        // memLevel 1: hash_bits 8 < w_bits 15 -> stored.
        let small_hash = init(6, MAX_WBITS, 1, Strategy::Default);
        // level 0 disqualifies the fixed bound even though w_bits <= hash_bits.
        let level_zero = init(0, 9, DEF_MEM_LEVEL, Strategy::Default);
        for len in [0usize, 1024, 200_000] {
            let fixed = c_fixedlen(len) + 6;
            let stored = c_storelen(len) + 6;
            assert_eq!(
                deflate_bound_z(&small_window, len),
                fixed,
                "windowBits 9, len {len}"
            );
            assert_eq!(
                deflate_bound_z(&small_hash, len),
                stored,
                "memLevel 1, len {len}"
            );
            assert_eq!(
                deflate_bound_z(&level_zero, len),
                stored,
                "level 0, len {len}"
            );
        }
    }

    /// C returns `(z_size_t)-1` on overflow; this port saturates instead, which
    /// caps at [`usize::MAX`] rather than wrapping or panicking.
    #[test]
    fn deflate_bound_saturates_instead_of_overflowing() {
        let stateless: ZStream = ZStream::new();
        assert_eq!(deflate_bound_z(&stateless, usize::MAX), usize::MAX);
        let zlib = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        assert_eq!(deflate_bound_z(&zlib, usize::MAX), usize::MAX);
        let small_window = init(6, 9, DEF_MEM_LEVEL, Strategy::Default);
        assert_eq!(deflate_bound_z(&small_window, usize::MAX), usize::MAX);
    }

    /// The gzip wrapper length is 18 plus every supplied header field. C counts
    /// the name and comment with `do { wraplen++; } while (*str++);`
    /// (`deflate.c` L899-L906), which includes the NUL terminator — this port
    /// stores no NUL, so it must add `len() + 1`.
    #[cfg(feature = "gzip")]
    #[test]
    fn deflate_bound_gzip_wraplen_counts_every_header_field_and_its_nul() {
        let mut strm = init(6, MAX_WBITS + 16, DEF_MEM_LEVEL, Strategy::Default);
        for len in [0usize, 1024] {
            assert_eq!(
                deflate_bound_z(&strm, len),
                c_tight(len, 18),
                "bare gzip, len {len}"
            );
        }

        let mut head = GzHeader::new()
            .with_extra(vec![1u8, 2, 3])
            .with_name("name.txt")
            .with_comment("a comment");
        head.hcrc = true;
        deflate_set_header(&mut strm, Some(head)).expect("a gzip stream accepts a header");

        // 18 + (2 + 3 extra) + (8 name + 1 NUL) + (9 comment + 1 NUL) + 2 hcrc.
        let wraplen = 18 + (2 + 3) + (8 + 1) + (9 + 1) + 2;
        for len in [0usize, 1024] {
            assert_eq!(
                deflate_bound_z(&strm, len),
                c_tight(len, wraplen),
                "full gzip, len {len}"
            );
        }
    }

    /// C `CLEAR_HASH` (`deflate.c` L170-L175) writes `NIL` to the last head
    /// slot, `zmemzero`s the rest, and clears `slid`. Because `NIL == 0`,
    /// zeroing every entry is equivalent — this checks the post-condition.
    #[test]
    fn clear_hash_resets_every_head_entry_and_the_slid_flag() {
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        let s = strm.deflate_state_mut().expect("init installs a state");
        for (i, h) in s.head.iter_mut().enumerate() {
            *h = (i as u16) | 1; // deliberately non-NIL everywhere
        }
        s.slid = true;
        clear_hash(s);
        assert!(
            s.head.iter().all(|&h| h == NIL),
            "every head entry must be NIL"
        );
        assert!(!s.slid, "the slide flag must be cleared");
    }

    /// C `putShortMSB` (`deflate.c` L939-L942) appends a 16-bit value
    /// most-significant byte first; the zlib header word, the preset-dictionary
    /// Adler-32, and the zlib trailer all depend on that order.
    #[test]
    fn put_short_msb_writes_the_most_significant_byte_first() {
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        let s = strm.deflate_state_mut().expect("init installs a state");
        assert_eq!(
            s.pending, 0,
            "a freshly initialized state has no pending output"
        );
        put_short_msb(s, 0x1234);
        put_short_msb(s, 0xabcd);
        assert_eq!(s.pending, 4);
        assert_eq!(&s.pending_buf[..4], &[0x12, 0x34, 0xab, 0xcd]);
    }

    /// C `deflatePending` (`deflate.c` L722-L734) and `deflateUsed`
    /// (`deflate.c` L737-L742) both require an installed state and both report
    /// state that is otherwise invisible to callers.
    #[test]
    fn deflate_pending_and_deflate_used_require_a_state_and_start_at_zero() {
        let mut strm: ZStream = ZStream::new();
        assert_eq!(deflate_pending(&strm), Err(ZlibError::StreamError));
        assert_eq!(deflate_used(&strm), Err(ZlibError::StreamError));

        deflate_init2(
            &mut strm,
            6,
            Z_DEFLATED,
            MAX_WBITS,
            DEF_MEM_LEVEL,
            Strategy::Default,
        )
        .expect("deflate_init2 must accept the default parameters");
        assert_eq!(
            deflate_pending(&strm),
            Ok((0, 0)),
            "nothing is pending after init"
        );
        assert_eq!(
            deflate_used(&strm),
            Ok(0),
            "no output byte has been written yet"
        );

        deflate_end(&mut strm).expect("deflate_end before any compression returns Z_OK");
        assert_eq!(
            deflate_pending(&strm),
            Err(ZlibError::StreamError),
            "state is gone"
        );
    }

    /// C `deflatePrime` (`deflate.c` L745-L771) inserts bits low-order first,
    /// flushing whole bytes least-significant first through `bi_flush`, and
    /// rejects any width outside `0..=16` with `Z_BUF_ERROR`.
    #[test]
    fn deflate_prime_packs_bits_low_order_first_and_rejects_bad_widths() {
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);

        deflate_prime(&mut strm, 5, 0b1_0101).expect("5 bits fit the empty accumulator");
        assert_eq!(
            deflate_pending(&strm),
            Ok((0, 5)),
            "5 bits buffer without emitting a byte"
        );

        deflate_prime(&mut strm, 16, 0xffff).expect("16 more bits");
        // 21 bits total: 0b1_0101 then sixteen 1s. The low 16 bits of the
        // accumulator (0xfff5) are flushed least-significant byte first, and the
        // remaining 5 bits stay buffered.
        assert_eq!(deflate_pending(&strm), Ok((2, 5)));
        let s = strm.deflate_state().expect("init installs a state");
        assert_eq!(&s.pending_buf[..2], &[0xf5, 0xff]);

        assert_eq!(
            deflate_prime(&mut strm, 17, 0),
            Err(ZlibError::BufError),
            "17 bits is too wide"
        );
        assert_eq!(
            deflate_prime(&mut strm, -1, 0),
            Err(ZlibError::BufError),
            "negative width"
        );

        let mut bare: ZStream = ZStream::new();
        assert_eq!(deflate_prime(&mut bare, 8, 0), Err(ZlibError::StreamError));
    }

    /// C `deflateTune` (`deflate.c` L819-L830) overrides the four match-finder
    /// fields **on the state**. The configuration table is `const` and keeps the
    /// verbatim C values — level 6 is `{8, 16, 128, 128, deflate_slow}`
    /// (`deflate.c` L118).
    #[test]
    fn deflate_tune_overrides_state_fields_and_leaves_the_configuration_table_intact() {
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        {
            let s = strm.deflate_state().expect("init installs a state");
            assert_eq!(s.good_match, 8, "level 6 good_length");
            assert_eq!(s.max_lazy_match, 16, "level 6 max_lazy");
            assert_eq!(s.nice_match, 128, "level 6 nice_length");
            assert_eq!(s.max_chain_length, 128, "level 6 max_chain");
        }

        deflate_tune(&mut strm, 33, 133, 259, 4097).expect("tune accepts advanced overrides");
        let s = strm.deflate_state().expect("init installs a state");
        assert_eq!(s.good_match, 33);
        assert_eq!(s.max_lazy_match, 133);
        assert_eq!(s.nice_match, 259);
        assert_eq!(s.max_chain_length, 4097);

        let row = CONFIGURATION_TABLE[6];
        assert_eq!(row.good_length, 8, "the table must not be mutable state");
        assert_eq!(row.max_lazy, 16);
        assert_eq!(row.nice_length, 128);
        assert_eq!(row.max_chain, 128);
        assert_eq!(row.func, CompressFunc::Slow);
    }

    /// Each [`DeflateConfig`] setter is a pure single-field write, and
    /// `new()` matches `default()` — the C `deflateInit2_` defaults.
    #[test]
    fn deflate_config_builder_changes_exactly_one_field_per_setter() {
        let base = DeflateConfig::new();
        assert_eq!(base, DeflateConfig::default(), "new() must equal default()");
        assert_eq!(base.level, Z_DEFAULT_COMPRESSION);
        assert_eq!(base.method, Z_DEFLATED);
        assert_eq!(base.window_bits, MAX_WBITS);
        assert_eq!(base.mem_level, DEF_MEM_LEVEL);
        assert_eq!(base.strategy, Strategy::Default);

        // Validation belongs to `init`, not the setters: each one only records.
        assert_eq!(base.level(9), DeflateConfig { level: 9, ..base });
        assert_eq!(base.method(0), DeflateConfig { method: 0, ..base });
        assert_eq!(
            base.window_bits(-15),
            DeflateConfig {
                window_bits: -15,
                ..base
            }
        );
        assert_eq!(
            base.mem_level(1),
            DeflateConfig {
                mem_level: 1,
                ..base
            }
        );
        assert_eq!(
            base.strategy(Strategy::Rle),
            DeflateConfig {
                strategy: Strategy::Rle,
                ..base
            }
        );
    }

    /// The zlib (RFC 1950) header words are canonical and independently known:
    /// `78 01` for levels 0-1, `78 5e` for 2-5, `78 9c` for 6, `78 da` for 7-9
    /// at `windowBits == 15`. They follow from C's four-way `level_flags` ladder
    /// (`deflate.c` L1036-L1043) plus the FCHECK adjustment
    /// `header += 31 - (header % 31)` (`deflate.c` L1046).
    #[test]
    fn zlib_header_matches_the_canonical_cmf_flg_words() {
        const DATA: &[u8] = b"zlib header CMF/FLG word";
        for (level, cmf, flg) in [
            (0, 0x78u8, 0x01u8),
            (1, 0x78, 0x01),
            (2, 0x78, 0x5e),
            (5, 0x78, 0x5e),
            (6, 0x78, 0x9c),
            (7, 0x78, 0xda),
            (9, 0x78, 0xda),
        ] {
            let out = compress_all(level, MAX_WBITS, Strategy::Default, DATA);
            assert_eq!(out[0], cmf, "CMF at level {level}");
            assert_eq!(out[1], flg, "FLG at level {level}");
            let header = (u32::from(out[0]) << 8) | u32::from(out[1]);
            assert_eq!(
                header % 31,
                0,
                "the FCHECK residue must be 0 at level {level}"
            );
            assert_eq!(
                u32::from(out[1]) & PRESET_DICT,
                0,
                "PRESET_DICT must be clear without a dictionary at level {level}"
            );
        }
    }

    /// RFC 1950 §2.2 stores the Adler-32 trailer most-significant byte first; C
    /// writes it as two `putShortMSB` calls (`deflate.c` L1281-L1282). Raw
    /// framing carries neither header nor trailer, so the wrapper costs exactly
    /// six bytes over the identical payload.
    #[test]
    fn zlib_trailer_carries_the_adler32_most_significant_byte_first() {
        const DATA: &[u8] = b"Adler-32 trailer byte order: most significant first";
        let out = compress_all(6, MAX_WBITS, Strategy::Default, DATA);
        let expected = adler32(1, DATA);
        assert_eq!(
            &out[out.len() - 4..],
            &expected.to_be_bytes(),
            "big-endian Adler-32"
        );

        let raw = compress_all(6, -MAX_WBITS, Strategy::Default, DATA);
        assert_eq!(out.len() - raw.len(), 6, "2 header + 4 trailer bytes");
        assert_eq!(
            &out[2..out.len() - 4],
            raw.as_slice(),
            "the payload is framing-independent"
        );
    }

    /// RFC 1952 §2.3.1 stores CRC-32 then ISIZE least-significant byte first; C
    /// writes eight explicit bytes (`deflate.c` L1269-L1276). The bare gzip
    /// wrapper is a 10-byte header plus that 8-byte trailer.
    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_trailer_carries_crc32_then_isize_least_significant_byte_first() {
        const DATA: &[u8] = b"gzip trailer: CRC-32 then ISIZE, little-endian";
        let out = compress_all(6, MAX_WBITS + 16, Strategy::Default, DATA);
        assert_eq!(&out[..3], &[0x1f, 0x8b, 0x08], "gzip magic bytes and CM");

        let crc = crc32(0, DATA);
        let end = out.len();
        assert_eq!(
            &out[end - 8..end - 4],
            &crc.to_le_bytes(),
            "little-endian CRC-32"
        );
        assert_eq!(
            &out[end - 4..],
            &(DATA.len() as u32).to_le_bytes(),
            "little-endian ISIZE"
        );

        let raw = compress_all(6, -MAX_WBITS, Strategy::Default, DATA);
        assert_eq!(out.len() - raw.len(), 18, "10 header + 8 trailer bytes");
        assert_eq!(
            &out[10..end - 8],
            raw.as_slice(),
            "the payload is framing-independent"
        );
    }

    /// C tests `s->level == 0` **first** in its dispatch cascade (`deflate.c`
    /// L1217-L1220), so level 0 stores regardless of strategy. The expected
    /// bytes come from RFC 1951 §3.2.4: a final stored block is `01`, then LEN
    /// and its ones-complement, each little-endian, then the literal bytes.
    #[test]
    fn level_zero_emits_stored_blocks_for_every_strategy() {
        const DATA: &[u8] = b"stored block payload";
        let len = DATA.len() as u16;
        let mut expected = vec![0x01u8];
        expected.extend_from_slice(&len.to_le_bytes());
        expected.extend_from_slice(&(!len).to_le_bytes());
        expected.extend_from_slice(DATA);

        for strategy in [
            Strategy::Default,
            Strategy::Filtered,
            Strategy::HuffmanOnly,
            Strategy::Rle,
            Strategy::Fixed,
        ] {
            let out = compress_all(0, -MAX_WBITS, strategy, DATA);
            assert_eq!(
                out, expected,
                "level 0 must store regardless of {strategy:?}"
            );
        }
    }

    /// `Z_HUFFMAN_ONLY` and `Z_RLE` are tested before the per-level table
    /// (`deflate.c` L1218-L1220), so at any non-zero level they select their own
    /// producer and ignore the level's tuning row entirely — which makes their
    /// output level-independent. The table producer does use the window, so it
    /// compresses a repetitive input far better than a literals-only pass.
    #[test]
    fn strategy_overrides_the_per_level_producer_for_huffman_only_and_rle() {
        const RUNS: &[u8] = b"aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbaaaaaaaaaaaaaaaa";
        for strategy in [Strategy::HuffmanOnly, Strategy::Rle] {
            let first = compress_all(1, -MAX_WBITS, strategy, RUNS);
            for level in 2..=9 {
                assert_eq!(
                    compress_all(level, -MAX_WBITS, strategy, RUNS),
                    first,
                    "{strategy:?} must ignore the level {level} tuning row"
                );
            }
        }

        let mut repeated: Vec<u8> = Vec::with_capacity(512);
        while repeated.len() < 512 {
            repeated.extend_from_slice(b"abcdefghijklmnop");
        }
        let huff = compress_all(9, -MAX_WBITS, Strategy::HuffmanOnly, &repeated);
        let table = compress_all(9, -MAX_WBITS, Strategy::Default, &repeated);
        assert!(
            table.len() < huff.len(),
            "the table producer ({} bytes) must beat literals-only ({} bytes)",
            table.len(),
            huff.len()
        );
    }
    /// The pre-flush predicate reproduces C's condition exactly, and
    /// [`deflate_params`] agrees with it on every case.
    ///
    /// C reaches its internal `deflate(strm, Z_BLOCK)` only when
    /// `(strategy != s->strategy || configuration_table[s->level].func !=
    /// configuration_table[level].func) && s->last_flush != -2`. That condition
    /// is what tells the C ABI boundary whether the caller's raw
    /// `next_in`/`next_out` pointers are relevant at all, so it is pinned here
    /// independently of the code that consumes it.
    ///
    /// Every expectation below was cross-checked against a reference C zlib
    /// built from this repository's own `*.c` sources.
    #[test]
    fn the_pre_flush_predicate_matches_cs_condition() {
        // Fresh stream: `last_flush == -2`, so NOTHING ever flushes — not a level
        // change, not a strategy change, not both.
        let strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        assert_eq!(strm.deflate_state().expect("state").last_flush, -2);
        for level in [Z_DEFAULT_COMPRESSION, 0, 1, 4, 6, 9] {
            for strategy in [
                Strategy::Default,
                Strategy::Filtered,
                Strategy::HuffmanOnly,
                Strategy::Rle,
                Strategy::Fixed,
            ] {
                assert!(
                    !deflate_params_flushes(&strm, level, strategy),
                    "a fresh stream (last_flush == -2) never flushes, \
                     level={level} strategy={strategy:?}"
                );
            }
        }

        // Out-of-range level and a stream with no engine both answer `false`
        // without panicking or indexing out of bounds; `deflate_params` reports
        // `StreamError` for them on its own, exactly as C's range test does
        // before the flush is considered.
        for bad in [-2, 10, 42, i32::MIN, i32::MAX] {
            assert!(!deflate_params_flushes(&strm, bad, Strategy::Default));
        }
        let bare: ZStream = ZStream::new();
        assert!(!deflate_params_flushes(&bare, 9, Strategy::Default));

        // Mid-stream (`last_flush != -2`) the producer/strategy comparison
        // decides. Level 1..=3 select `deflate_fast`, 4..=9 select
        // `deflate_slow`, and level 0 selects `deflate_stored`, so a change
        // WITHIN a producer band is pure bookkeeping while a change ACROSS bands
        // must flush.
        let corpus: Vec<u8> = (0..4096u32).map(|i| b'a' + (i % 3) as u8).collect();
        let mut out = vec![0u8; 64 * 1024];
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        let outcome = deflate(&mut strm, &corpus, &mut out, Z_NO_FLUSH);
        assert_eq!(outcome.code, ReturnCode::Ok);
        assert_eq!(outcome.consumed, corpus.len(), "all input must be consumed");
        assert_ne!(strm.deflate_state().expect("state").last_flush, -2);
        // Every byte this stream emits is accumulated in `out` so the re-dispatch
        // can be proved not to corrupt it (see the decode at the end).
        let mut produced = outcome.produced;

        // Same producer band (6 -> 4..=9) and same strategy: no flush.
        for level in [4, 5, 6, 7, 8, 9, Z_DEFAULT_COMPRESSION] {
            assert!(
                !deflate_params_flushes(&strm, level, Strategy::Default),
                "level {level} keeps `deflate_slow`, so nothing needs flushing"
            );
        }
        // Different band: flush required.
        for level in [0, 1, 2, 3] {
            assert!(
                deflate_params_flushes(&strm, level, Strategy::Default),
                "level {level} switches the block producer, so a flush is required"
            );
        }
        // Strategy change alone: flush required, whatever the level.
        for strategy in [
            Strategy::Filtered,
            Strategy::HuffmanOnly,
            Strategy::Rle,
            Strategy::Fixed,
        ] {
            assert!(deflate_params_flushes(&strm, 6, strategy));
        }

        // And `deflate_params` agrees: a predicate-`false` call performs no I/O
        // even when the caller passes empty buffers, while a predicate-`true`
        // call is the one that consumes/produces.
        let mut empty_out: [u8; 0] = [];
        let quiet = deflate_params(&mut strm, &[], &mut empty_out, 9, Strategy::Default);
        assert_eq!(quiet.code, ReturnCode::Ok);
        assert_eq!((quiet.consumed, quiet.produced), (0, 0));
        assert_eq!(strm.deflate_state().expect("state").level, 9);

        // Now a real producer switch with room available: accepted, and — the
        // part that has to be demonstrated rather than asserted about state — the
        // finished stream still decodes to the original bytes. A re-dispatch that
        // mangled the block in flight would leave the level field looking correct
        // while producing an undecodable or wrong stream, so the switch is
        // followed here by an actual finish, an actual decode, and an actual
        // byte-for-byte comparison against `corpus`.
        let switch = deflate_params(&mut strm, &[], &mut out[produced..], 1, Strategy::Default);
        assert_eq!(switch.code, ReturnCode::Ok);
        assert_eq!(strm.deflate_state().expect("state").level, 1);
        produced += switch.produced;

        loop {
            let finish = deflate(&mut strm, &[], &mut out[produced..], Z_FINISH);
            produced += finish.produced;
            match finish.code {
                ReturnCode::StreamEnd => break,
                ReturnCode::Ok => assert!(
                    finish.produced != 0,
                    "the finish must make progress with {} bytes of room left",
                    out.len() - produced
                ),
                other => panic!("finishing after a re-dispatch returned {other:?}"),
            }
        }
        out.truncate(produced);
        deflate_end(&mut strm).expect("deflate_end must succeed after Z_STREAM_END");
        assert_eq!(
            inflate_all(&out, corpus.len()),
            corpus,
            "a stream whose level was switched mid-flight must still decode \
             byte-exactly"
        );
    }

    /// `Z_PARTIAL_FLUSH` honours the two clauses `zlib.h` states for it: every
    /// input byte seen so far becomes available to the decompressor, and the
    /// emitted output is **not** aligned to a byte boundary.
    ///
    /// `zlib.h` (L290-L305) specifies the mechanism as well: the current block is
    /// completed and followed by "an empty fixed codes block that is 10 bits
    /// long", which guarantees enough bytes are emitted for the decompressor to
    /// finish the real block. The port implements exactly that at [`deflate`]
    /// L1148-L1149 via [`trees::_tr_align`] — a 3-bit `STATIC_TREES` header plus
    /// the 7-bit static `END_BLOCK` code, then `bi_flush`, which writes out whole
    /// bytes only and therefore leaves the remainder in the bit buffer.
    ///
    /// Three flushes are run over identical input with identical parameters, so
    /// the data block they emit is identical and only the terminator differs:
    ///
    /// | flush | terminator appended | aligned? |
    /// |-------|--------------------|----------|
    /// | `Z_BLOCK` | none (block closed, bits withheld) | no |
    /// | `Z_PARTIAL_FLUSH` | empty **static** block, 10 bits | no |
    /// | `Z_SYNC_FLUSH` | `bi_windup` + empty **stored** block `00 00 ff ff` | yes |
    ///
    /// That makes the byte-count relationships exact rather than approximate: 10
    /// bits can flush at most two whole bytes, while a sync marker costs at least
    /// one alignment byte plus four marker bytes.
    ///
    /// One measured nuance, recorded so the strength of the decode assertion is
    /// not overstated: the terminator is deliberately *insurance*. Truncating the
    /// emitted prefix by its final byte still decodes this corpus in full, while
    /// truncating by two or more does not — which is the `zlib.h` clause "assures
    /// that enough bytes are output" behaving exactly as written.
    #[test]
    fn z_partial_flush_publishes_all_input_without_byte_aligning() {
        // Long enough for the match finder to emit a real block with both
        // literals and matches, and deterministic.
        let corpus: Vec<u8> = (0..600u32)
            .map(|i| b"partial flush behaviour "[(i as usize) % 24])
            .collect();

        let (block, block_bits) = flush_once(Z_BLOCK, &corpus);
        let (partial, partial_bits) = flush_once(Z_PARTIAL_FLUSH, &corpus);
        let (sync, sync_bits) = flush_once(crate::constants::Z_SYNC_FLUSH, &corpus);

        // ---- clause 2: alignment ----
        // `_tr_stored_block` calls `bi_windup`, so a sync flush always empties the
        // bit buffer. `_tr_align` calls `bi_flush`, which only writes out whole
        // bytes, so a partial flush cannot align unless the bit count happens to
        // land on a boundary — and for this corpus it does not.
        assert_eq!(
            sync_bits, 0,
            "Z_SYNC_FLUSH byte-aligns, so no bits may remain buffered"
        );
        assert_ne!(
            partial_bits, 0,
            "Z_PARTIAL_FLUSH must not byte-align: bits are expected to remain \
             buffered after the 10-bit empty static block"
        );
        // `bi_flush` empties a full 16-bit buffer outright and otherwise writes
        // exactly one byte, so from `block_bits` the 10-bit terminator lands on a
        // value this test can predict in closed form.
        let raw = block_bits + 10;
        let expected_bits = if raw == 16 { 0 } else { raw - 8 };
        assert_eq!(
            partial_bits, expected_bits,
            "the empty static block is exactly 10 bits wide, so from \
             {block_bits} buffered bits `bi_flush` must leave {expected_bits}"
        );

        // ---- the wire-level shape of each terminator ----
        assert!(
            partial.len() >= block.len() && partial.len() <= block.len() + 2,
            "a 10-bit terminator can flush at most two whole bytes: Z_BLOCK \
             emitted {} bytes, Z_PARTIAL_FLUSH emitted {}",
            block.len(),
            partial.len()
        );
        assert!(
            sync.len() >= block.len() + 4,
            "a sync marker costs alignment plus four bytes: Z_BLOCK emitted {} \
             bytes, Z_SYNC_FLUSH emitted {}",
            block.len(),
            sync.len()
        );
        assert!(
            partial.len() < sync.len(),
            "the 10-bit empty static block must be cheaper than a byte-aligned \
             sync marker ({} vs {} bytes)",
            partial.len(),
            sync.len()
        );
        assert_eq!(
            &sync[sync.len() - 4..],
            &[0x00, 0x00, 0xff, 0xff],
            "Z_SYNC_FLUSH must end with the empty stored block marker"
        );
        assert_ne!(
            &partial[partial.len() - 4..],
            &[0x00, 0x00, 0xff, 0xff],
            "Z_PARTIAL_FLUSH must not emit the byte-aligned sync marker"
        );

        // ---- clause 1: all input so far is available to the decompressor ----
        assert_eq!(
            inflate_prefix(&partial, corpus.len()),
            corpus,
            "every byte fed before Z_PARTIAL_FLUSH must be recoverable from the \
             bytes it emitted"
        );

        // ---- the stream survives the flush and still finishes correctly ----
        const TAIL: &[u8] = b"and the tail that follows the partial flush";
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        let mut out = vec![0u8; deflate_bound(&strm, corpus.len() + TAIL.len()) + 512];
        let first = deflate(&mut strm, &corpus, &mut out, Z_PARTIAL_FLUSH);
        assert_eq!(first.code, ReturnCode::Ok);
        assert_eq!(first.consumed, corpus.len());
        assert!(first.produced > 0, "the flush must publish bytes");
        let mut n = first.produced;
        let finish = deflate(&mut strm, TAIL, &mut out[n..], Z_FINISH);
        assert_eq!(
            finish.code,
            ReturnCode::StreamEnd,
            "the stream must finish normally after a partial flush"
        );
        assert_eq!(finish.consumed, TAIL.len());
        n += finish.produced;
        out.truncate(n);
        deflate_end(&mut strm).expect("deflate_end after Z_STREAM_END");

        let mut expected = corpus.clone();
        expected.extend_from_slice(TAIL);
        assert_eq!(
            inflate_all(&out, expected.len()),
            expected,
            "a stream containing a partial flush must decode byte-exactly"
        );
    }

    /// `params_resolve_level` mirrors C's level resolution and range test, and is
    /// idempotent for every accepted value (so passing an already-resolved level
    /// back through it cannot change the answer).
    #[test]
    fn params_level_resolution_matches_c() {
        assert_eq!(params_resolve_level(Z_DEFAULT_COMPRESSION), Some(6));
        for level in 0..=9 {
            assert_eq!(params_resolve_level(level), Some(level));
            assert_eq!(
                params_resolve_level(level).and_then(params_resolve_level),
                Some(level)
            );
        }
        for bad in [-2, -3, 10, 11, i32::MIN, i32::MAX] {
            assert_eq!(
                params_resolve_level(bad),
                None,
                "level {bad} is out of range"
            );
        }
    }

    /// A window slide that wraps `match_start` must still emit the true match
    /// distance.
    ///
    /// `fill_window` subtracts `w_size` from `match_start` unconditionally (C
    /// `deflate.c` L288), so a `match_start` left stale by an iteration that did
    /// not call `longest_match` wraps around. C recovers the correct distance
    /// anyway: `strstart` and `match_start` are reduced by the same amount on
    /// every slide, and `strstart - 1 - prev_match` (C `deflate.c` L2019) is
    /// evaluated in the same modular unsigned arithmetic, so the wrap cancels.
    /// Reproducing that requires `prev_match` to be as wide as `match_start` —
    /// C declares both `unsigned` (`deflate.h` L98 `IPos`, L167 `uInt`).
    ///
    /// This payload drives `deflate_slow` into exactly that state. With
    /// `windowBits = 9` the window is 512 bytes and `max_dist()` is 250, so
    /// planting one distinctive seven-byte pattern at offset 511 and another at
    /// offset 761 makes the match found at `strstart = 761` start at window
    /// position 511 — a distance of precisely `max_dist()`. The next position,
    /// 762, is the first that satisfies `strstart >= w_size + max_dist()`, so it
    /// triggers the initial slide, which wraps `match_start` to `511 - 512`
    /// before it is copied into `prev_match` and the deferred match is emitted.
    /// `Z_FILTERED` is required because it discards every match of five bytes or
    /// fewer, which is what leaves the seven-byte pattern as the only match in
    /// play.
    ///
    /// The expected output was measured with reference C zlib built from this
    /// repository's own `*.c` sources under `-fsanitize=address,undefined`: it
    /// compresses this payload to 943 bytes with CRC-32 `0xb573cc1b`, decodes it
    /// back byte-exactly, and reports no sanitizer diagnostic.
    #[test]
    fn a_wrapped_prev_match_still_emits_the_true_distance() {
        /// Distinct high bytes that the filler below can never produce, so the
        /// pattern occurs exactly twice and no competing match exists.
        const PATTERN: [u8; 7] = [0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7];
        /// Window position of the match the deferred emission refers to.
        const MATCH_AT: usize = 511;
        /// Position at which that match is found: `MATCH_AT + max_dist()`.
        const FOUND_AT: usize = 761;
        /// Compressed length produced by reference C zlib.
        const C_PRODUCED: usize = 943;
        /// CRC-32 of the compressed stream produced by reference C zlib.
        const C_CRC: u32 = 0xb573_cc1b;

        let mut payload = vec![0u8; 900];
        let mut x: u32 = 0x1234_5678;
        for slot in payload.iter_mut() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            // Confine the filler to 0x00..=0xEF so it cannot collide with
            // PATTERN, and keep it high-entropy so it forms no long match.
            *slot = ((x >> 16) % 240) as u8;
        }
        payload[MATCH_AT..MATCH_AT + PATTERN.len()].copy_from_slice(&PATTERN);
        payload[FOUND_AT..FOUND_AT + PATTERN.len()].copy_from_slice(&PATTERN);

        let mut strm = init(7, 9, 1, Strategy::Filtered);
        let mut out = vec![0u8; deflate_bound(&strm, payload.len()) + 64];
        let outcome = deflate(&mut strm, &payload, &mut out, Z_FINISH);
        assert_eq!(
            outcome.code,
            ReturnCode::StreamEnd,
            "the stream must finish rather than fault on the wrapped match_start"
        );
        assert_eq!(
            outcome.consumed,
            payload.len(),
            "all input must be consumed"
        );
        out.truncate(outcome.produced);
        deflate_end(&mut strm).expect("deflate_end must succeed after Z_STREAM_END");

        assert_eq!(
            out.len(),
            C_PRODUCED,
            "reference C zlib compresses this payload to {C_PRODUCED} bytes"
        );
        assert_eq!(
            crate::checksum::crc32(0, &out),
            C_CRC,
            "the compressed bytes must be identical to reference C zlib"
        );
        assert_eq!(
            inflate_all(&out, payload.len()),
            payload,
            "the emitted match distance must be valid, so the stream round-trips"
        );
    }
}
