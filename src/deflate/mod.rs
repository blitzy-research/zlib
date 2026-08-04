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
    DEF_MEM_LEVEL, FlushMode, MAX_WBITS, Strategy, WrapMode, Z_BLOCK, Z_DEFAULT_COMPRESSION,
    Z_DEFLATED, Z_FINISH, Z_FULL_FLUSH, Z_NO_FLUSH, Z_PARTIAL_FLUSH, parse_window_bits,
};
use crate::error::{ReturnCode, ZlibError};
#[cfg(feature = "gzip")]
use crate::gz_header::{ForeignGzHeader, GzHeader, GzHeaderSlot, HeaderFields};
use crate::stream::{Allocator, ZStream};

use crate::deflate::state::{BUF_SIZE, DeflateStream, MIN_MATCH, NIL};
use crate::deflate::strategy::rank;
use crate::util::compress::{OneCallDeflate, OneCallStep, compress2_tracked_with};

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
/// `Some(dst)`, the most recent `len` window bytes are copied into `dst` and
/// every byte of `dst` beyond `len` is left untouched. Pass `None` to query only
/// the length — the idiomatic spelling of the `Z_NULL` destination C callers use
/// to size a buffer before fetching into it.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed.
///
/// # Panics
///
/// If `dictionary` is `Some(dst)` and `dst` is **shorter** than the length this
/// function would return. C cannot detect that case at all — it is handed a bare
/// `Bytef *` and writes `len` bytes through it, so an undersized buffer is silent
/// memory corruption. Rust is handed the destination's length, so the same
/// mistake is caught and reported instead. Callers that do not already know the
/// length must query it first (`None`) and pass a buffer of at least that size;
/// the C-ABI shim in `src/ffi/deflate.rs` does exactly that, so it can never
/// reach this panic.
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
            // Stated explicitly rather than left to the slice index below, so the
            // documented precondition names itself when it is violated. Slicing
            // `dst[..len]` would already panic, but only with a bare "range end
            // index out of range", which tells the caller nothing about which
            // contract they broke or how to satisfy it.
            assert!(
                dst.len() >= len,
                "deflate_get_dictionary destination holds {} bytes but the dictionary is \
                 {len}; query the length with `None` first",
                dst.len()
            );
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
    s.gzhead = match head {
        Some(h) => GzHeaderSlot::Owned(h),
        None => GzHeaderSlot::None,
    };
    Ok(ReturnCode::Ok)
}

/// Registers a **caller-owned** gzip header, the C ABI's counterpart of
/// [`deflate_set_header`].
///
/// Port of C `deflateSetHeader` (`deflate.c` L714-L719) on the path where the
/// header lives in the caller's memory. `present` mirrors `head != Z_NULL`: C
/// simply assigns the pointer, so a null one clears the header exactly as an
/// idiomatic [`None`] does.
///
/// No contents are stored — that is the whole point. The engine records only
/// *that* a foreign header exists and re-reads it through
/// [`GzHeaderSlot::Foreign`] on every entry point that needs it, so this call
/// allocates nothing and, like C, cannot fail for want of memory.
///
/// # Errors
///
/// [`ZlibError::StreamError`] if `strm` has no deflate state installed, or the
/// stream is not gzip-framed (`wrap != 2`) — both exactly as C.
#[cfg(feature = "gzip")]
pub(crate) fn deflate_set_header_foreign<A: Allocator>(
    strm: &mut ZStream<A>,
    present: bool,
) -> DeflateResult {
    let s = strm.deflate_state_mut().ok_or(ZlibError::StreamError)?;
    if s.wrap != 2 {
        return Err(ZlibError::StreamError);
    }
    s.gzhead = if present {
        GzHeaderSlot::Foreign
    } else {
        GzHeaderSlot::None
    };
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
    deflate_bound_z_lending(
        strm,
        source_len,
        #[cfg(feature = "gzip")]
        None,
    )
}

/// [`deflate_bound_z`], additionally lending a live caller-owned gzip header.
///
/// C's `deflateBound` reads `s->gzhead->extra_len`, `->name`, `->comment` and
/// `->hcrc` through the stored pointer (`deflate.c` L893-L907), so a stream whose
/// header lives in caller memory can only be bounded correctly if that memory is
/// reachable. The C ABI shim therefore lends the live header here exactly as it
/// does for `deflate` itself.
pub(crate) fn deflate_bound_z_lending<A: Allocator>(
    strm: &ZStream<A>,
    source_len: usize,
    #[cfg(feature = "gzip")] lent: Option<&ForeignGzHeader<'_>>,
) -> usize {
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
            // C reads the *live* header here too (`deflate.c` L893-L907), so the
            // bound must come from the same borrowed view emission uses.
            if let Some(h) = HeaderFields::resolve(&s.gzhead, lent) {
                if let Some(extra) = h.extra {
                    w += 2 + extra.len();
                }
                if let Some(name) = h.name {
                    w += name.len() + 1; // field bytes plus the NUL terminator
                }
                if let Some(comment) = h.comment {
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

/// [`deflate_bound`], additionally lending a live caller-owned gzip header.
///
/// See [`deflate_bound_z_lending`] for why the lend is required.
pub(crate) fn deflate_bound_lending<A: Allocator>(
    strm: &ZStream<A>,
    source_len: usize,
    #[cfg(feature = "gzip")] lent: Option<&ForeignGzHeader<'_>>,
) -> usize {
    deflate_bound_z_lending(
        strm,
        source_len,
        #[cfg(feature = "gzip")]
        lent,
    )
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
/// Emits the gzip member header (RFC 1952) from a **borrowed** view of whichever
/// header the stream has.
///
/// Port of the `GZIP`/`EXTRA`/`NAME`/`COMMENT`/`HCRC` phase ladder of C `deflate`
/// (`deflate.c` L1063-L1198). `head` is [`None`] exactly when C would see
/// `s->gzhead == Z_NULL`, in which case the default header is written
/// (`deflate.c` L1072-L1090).
///
/// # Why the header arrives borrowed
///
/// C never copies the caller's header: it re-reads through `s->gzhead` every time
/// it needs a field. Passing a borrowed [`HeaderFields`] reproduces that exactly
/// and has two consequences this function depends on. Caller mutations made after
/// registration but before emission are honored, which is required for
/// byte-identical output (AAP §0.8.1 D-1); and nothing is allocated here, which an
/// earlier implementation could not claim because it cloned `extra`, `name`, and
/// `comment` once per phase — and re-cloned them on every re-entry into a phase
/// that had been interrupted by a full pending buffer.
///
/// # Return value
///
/// [`Some`] means "return this code from `deflate` immediately", reproducing C's
/// early exits when the pending buffer could not be fully flushed. [`None`] means
/// the header is complete and compression may proceed.
///
/// The code is [`ReturnCode::Ok`] for every flush-stall exit. The one other
/// possibility is [`ReturnCode::StreamError`], returned when a caller shrinks a
/// *live* foreign `extra_len` below the progress this phase has already made —
/// the state C reaches through an unsigned underflow and an unbounded over-read.
/// See the re-entry guard in the `Extra` phase.
#[cfg(feature = "gzip")]
fn emit_gzip_header(
    s: &mut DeflateState,
    io: &mut IoContext,
    head: Option<&HeaderFields<'_>>,
) -> Option<ReturnCode> {
    if s.status == DeflateStatus::Gzip {
        // The gzip header checksum is a CRC-32 over the header bytes.
        io.adler = crc32(0, &[]);
        s.put_byte(31);
        s.put_byte(139);
        s.put_byte(8);

        // Snapshot the scalars into a `Copy` tuple so `s` can be mutated
        // freely below. The three byte payloads stay borrowed.
        let head_info = head.map(|h| {
            (
                h.text,
                h.hcrc,
                h.extra.is_some(),
                h.name.is_some(),
                h.comment.is_some(),
                h.time,
                h.os,
                h.extra.map_or(0usize, <[u8]>::len),
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
                    return Some(ReturnCode::Ok);
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
        // The bytes are read straight out of the borrowed view, so the
        // pending buffer (a field of `s`) can be written without cloning.
        if let Some(extra) = head.and_then(|h| h.extra) {
            let gz_hcrc = head.is_some_and(|h| h.hcrc);
            let extra_len = extra.len() & 0xffff;
            // Re-entry guard on the caller's *live* header length.
            //
            // `s.gzindex` records how many `extra` bytes this phase has already
            // copied; it survives across `deflate` calls because a full pending
            // buffer makes the phase return early (below) and resume here on the
            // next call. `extra_len`, by contrast, is re-derived from the caller's
            // struct on every call — that freshness is the whole point of
            // borrowing the header rather than copying it (see this function's
            // "Why the header arrives borrowed"). A caller is therefore free to
            // *shrink* `head->extra_len` between two calls and leave
            // `gzindex > extra_len`.
            //
            // C computes `ulg left = (s->gzhead->extra_len & 0xffff) - s->gzindex`
            // (`deflate.c` L1120) with no such guard: on an unsigned type the
            // subtraction wraps to a near-`ULONG_MAX` count, and the copy loop
            // then reads far past the end of the caller's buffer — an unbounded
            // over-read, so C has no defined behavior here to preserve. The
            // arithmetic is unreachable in every legitimate flow (`gzindex` is
            // zeroed in the `Gzip` phase before this one is entered, and is only
            // ever advanced by amounts summing to at most `extra_len`), so
            // rejecting it costs nothing and cannot perturb byte-identical output
            // (AAP §0.8.1 D-1). `Z_STREAM_ERROR` is the documented `deflate`
            // return for an inconsistent stream state, and the phase state is left
            // untouched so a caller that restores `extra_len` resumes correctly.
            if s.gzindex > extra_len {
                return Some(ReturnCode::StreamError);
            }
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
                    return Some(ReturnCode::Ok);
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
        if let Some(name) = head.and_then(|h| h.name) {
            let gz_hcrc = head.is_some_and(|h| h.hcrc);
            let mut beg = s.pending;
            loop {
                if s.pending == s.pending_buf_size {
                    hcrc_update(s, io, gz_hcrc, beg);
                    s.flush_pending(io);
                    if s.pending != 0 {
                        s.last_flush = -1;
                        return Some(ReturnCode::Ok);
                    }
                    beg = 0;
                }
                // C reads bytes (including the C-string NUL) until it hits
                // 0. The borrowed view excludes the NUL, so emit a 0 past
                // its end (`deflate.c` L1158: `val = s->gzhead->name[...]`).
                //
                // This form is also what makes the phase immune to the
                // shrinking-live-header hazard the `Extra` phase has to reject
                // explicitly: `name` is re-scanned to the caller's NUL on every
                // call, so a caller that moves its terminator earlier — even to
                // before the `gzindex` this loop already reached — simply falls
                // into the `else` arm, emits the terminating 0 and ends the
                // field. There is no subtraction to underflow and no index to
                // run out of range. C in the same situation reads
                // `name[gzindex]` from beyond the new NUL and keeps going until
                // it happens upon a zero byte, which is an unbounded over-read
                // with no defined behavior to preserve.
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
        if let Some(comment) = head.and_then(|h| h.comment) {
            let gz_hcrc = head.is_some_and(|h| h.hcrc);
            let mut beg = s.pending;
            loop {
                if s.pending == s.pending_buf_size {
                    hcrc_update(s, io, gz_hcrc, beg);
                    s.flush_pending(io);
                    if s.pending != 0 {
                        s.last_flush = -1;
                        return Some(ReturnCode::Ok);
                    }
                    beg = 0;
                }
                // Byte-at-a-time to the caller's NUL, exactly as in the `Name`
                // phase above (`deflate.c` L1180), and immune to a shrinking
                // live header for the same reason: see the note there.
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
        let gz_hcrc = head.is_some_and(|h| h.hcrc);
        if gz_hcrc {
            if s.pending + 2 > s.pending_buf_size {
                s.flush_pending(io);
                if s.pending != 0 {
                    s.last_flush = -1;
                    return Some(ReturnCode::Ok);
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
            return Some(ReturnCode::Ok);
        }
    }

    None
}

fn deflate_run(
    s: &mut DeflateState,
    io: &mut IoContext,
    flush: i32,
    #[cfg(feature = "gzip")] lent: Option<&ForeignGzHeader<'_>>,
) -> ReturnCode {
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
    // C keeps only a `gz_header *` and re-reads it here, at emission time
    // (`deflate.c` L1092-L1188). The slot is moved out of `s` for the duration so
    // that borrowing the header payload cannot conflict with the writes into
    // `s.pending_buf`; it is restored on every exit path. This is what lets the
    // emission code read borrowed slices instead of cloning `extra`, `name`, and
    // `comment` once per phase (AAP §0.6.5: allocation-count parity with C).
    #[cfg(feature = "gzip")]
    {
        let slot = core::mem::take(&mut s.gzhead);
        let early = emit_gzip_header(s, io, HeaderFields::resolve(&slot, lent).as_ref());
        s.gzhead = slot;
        if let Some(code) = early {
            return code;
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
    deflate_lending(
        strm,
        input,
        output,
        flush,
        #[cfg(feature = "gzip")]
        None,
    )
}

/// [`deflate`], additionally lending the engine a **live** caller-owned gzip
/// header for the duration of this one call.
///
/// This is the entry point the C ABI uses. C's `deflateSetHeader` stores only a
/// pointer, and `deflate` re-reads through it at emission time (`deflate.c` L717,
/// L1092-L1188), so the shim re-materializes `lent` from the caller's struct on
/// every call instead of copying it once at registration. `lent` is [`Some`]
/// exactly when the stream's slot is [`GzHeaderSlot::Foreign`] and the caller's
/// pointer is still live; the idiomatic [`deflate`] passes [`None`] because an
/// engine-owned header needs no lending.
#[must_use]
pub(crate) fn deflate_lending<A: Allocator>(
    strm: &mut ZStream<A>,
    input: &[u8],
    output: &mut [u8],
    flush: i32,
    #[cfg(feature = "gzip")] lent: Option<&ForeignGzHeader<'_>>,
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
        Some(s) => deflate_run(
            s,
            &mut io,
            flush,
            #[cfg(feature = "gzip")]
            lent,
        ),
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
/// The copy is allocator-preserving: **five allocations in total** — the
/// state-object reservation mirroring C's
/// `ZALLOC(strm, 1, sizeof(deflate_state))` (`deflate.c` L1335) plus the four
/// working buffers `window`, `prev`, `head` and `pending_buf` (`deflate.c`
/// L1342-L1345) — are re-allocated through the **same** `AllocHook` as the
/// source, so a caller who installed a custom arena does not find the copy
/// living in the global heap, and the caller observes the same request count C
/// issues (AAP §0.6.5). The symbol region is not a fifth working buffer: it is
/// overlaid inside `pending_buf` at offset `lit_bufsize`, exactly as C
/// re-derives `ds->sym_buf = ds->pending_buf + ds->lit_bufsize` after the copy
/// (`deflate.c` L1367), so it carries no request of its own.
///
/// Only the live region of each working buffer is duplicated, which is what C
/// copies (`deflate.c` L1353-L1368); see
/// [`DeflateState::try_clone`] for the region-by-region schedule.
///
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
// One-call façade — the engine-owning half of C `compress.c`
//
// C `compress.c` is a separate translation unit that `#include`s `zlib.h` and
// calls `deflateInit`/`deflate`/`deflateEnd`, so in the `#include` order it sits
// ABOVE the engine, not beside `zutil.h`. The Rust layering reflects that: the
// bit-exact sizing formula and the complete `compress2_z` driver loop live in
// `crate::util::compress` (layer 3, engine-free and generic over the
// `OneCallDeflate` port it declares), and the engine adapter plus the three
// C-named entry points live here (layer 6). That is what keeps the module graph
// one-way — layer 3 never names an engine (AAP §0.3.1, §0.4.2 B2) — while the
// crate root still re-exports `compress`, `compress2`, and `compress_bound`
// under exactly the names `zlib.h` publishes.
// ===========================================================================

/// The [`OneCallDeflate`] adapter: a private, self-contained stream driven by the
/// layer-3 `compress2_z` transcription.
///
/// It exists only for the duration of one `compress`/`compress2` call and holds
/// nothing beyond the stream itself, so the three trait methods are literally the
/// three C calls `compress2_z` makes.
struct OneCallDeflateEngine {
    /// The stream this call owns, initialised by [`OneCallDeflate::begin`].
    strm: ZStream,
}

impl OneCallDeflate for OneCallDeflateEngine {
    /// C `deflateInit(&stream, level)` (`compress.c` L40).
    ///
    /// `ZStream::new` installs the default (global) allocator, exactly as C
    /// `compress2_z` zeroes `zalloc`/`zfree`/`opaque` so `deflateInit` substitutes
    /// its own. An invalid `level` surfaces as [`ReturnCode::StreamError`] and an
    /// allocation failure as [`ReturnCode::MemError`].
    fn begin(level: i32) -> Result<Self, ReturnCode> {
        let mut strm = ZStream::new();
        deflate_init(&mut strm, level).map_err(ReturnCode::from)?;
        Ok(Self { strm })
    }

    /// C `deflate(&stream, flush)` (`compress.c` L58).
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: FlushMode) -> OneCallStep {
        let outcome = deflate(&mut self.strm, input, output, flush.as_c_int());
        OneCallStep {
            consumed: outcome.consumed,
            produced: outcome.produced,
            code: outcome.code,
        }
    }

    /// C `deflateEnd(&stream)` (`compress.c` L65).
    ///
    /// C ignores the return value, and so does this: the outcome of the call has
    /// already been decided. Dropping the stream afterwards is safe because
    /// `deflate_end` clears the state, so the `Drop` is a no-op (no double free).
    fn end(&mut self) {
        let _ = deflate_end(&mut self.strm);
    }
}

/// Compresses `source` into `dest` at the given compression `level`, returning
/// the number of bytes written to `dest`.
///
/// Faithful port of C `compress2_z` (`compress.c` L24-L66). `level` has the same
/// meaning as in `deflateInit`: `0` ([`Z_NO_COMPRESSION`]) through `9`
/// ([`Z_BEST_COMPRESSION`]), or `-1` ([`Z_DEFAULT_COMPRESSION`]) to request the
/// library default. `dest` must be at least
/// [`compress_bound(source.len())`](crate::util::compress_bound) bytes for the
/// call to be guaranteed to succeed.
///
/// Empty inputs are valid: an empty `source` still produces a complete zlib
/// stream (header, one empty block, and the Adler-32 trailer), so `dest` must
/// have room for at least those bytes. An empty `dest` therefore yields
/// [`ReturnCode::BufError`] — matching the C behavior — because not even the
/// two-byte header fits.
///
/// [`Z_NO_COMPRESSION`]: crate::constants::Z_NO_COMPRESSION
/// [`Z_BEST_COMPRESSION`]: crate::constants::Z_BEST_COMPRESSION
///
/// # Errors
///
/// * [`ReturnCode::StreamError`] — `level` is outside the valid set
///   (`-1` or `0..=9`); reported by the deflate initializer.
/// * [`ReturnCode::BufError`] — `dest` was too small to hold the complete
///   compressed stream.
/// * [`ReturnCode::MemError`] — the engine could not allocate its working
///   buffers, mirroring C `Z_MEM_ERROR`.
///
/// # Examples
///
/// ```
/// # use zlib_rs::{compress2, compress_bound, uncompress, Z_BEST_COMPRESSION};
/// let plain = b"one-call compression at the maximum level";
/// let mut zlib = vec![0u8; compress_bound(plain.len())];
/// let n = compress2(&mut zlib, plain, Z_BEST_COMPRESSION).unwrap();
///
/// let mut out = vec![0u8; plain.len()];
/// assert_eq!(uncompress(&mut out, &zlib[..n]).unwrap(), plain.len());
/// assert_eq!(&out[..], plain);
/// ```
pub fn compress2(dest: &mut [u8], source: &[u8], level: i32) -> Result<usize, ReturnCode> {
    // The produced-byte count is already carried by the `Ok` arm, so the
    // out-parameter is discarded here. The C-ABI shims call `compress2_tracked`
    // directly instead, because C publishes the produced length on its error
    // paths too (`compress.c` L63).
    let mut produced = 0usize;
    compress2_tracked(dest, source, level, &mut produced)
}

/// Compresses `source` into `dest` at `level`, reporting the produced byte count
/// through `produced` on **every** path that reaches the deflate loop.
///
/// This is the tracked core of [`compress2`] — identical logic, plus an
/// out-parameter that mirrors C `compress2_z`'s *unconditional*
/// `*destLen = (z_size_t)(stream.next_out - dest);` (`compress.c` L63). C runs
/// that assignment after the loop and before `deflateEnd`, so it reports a
/// partial length on the `Z_BUF_ERROR` path just as it does on success. The
/// C-ABI shims in `src/ffi/util.rs` need that count to reproduce the behavior
/// exactly; [`compress2`] keeps its `Result<usize, ReturnCode>` shape and simply
/// discards the out-parameter, since its `Ok` arm already carries the length.
///
/// # Errors
///
/// Identical to [`compress2`].
pub(crate) fn compress2_tracked(
    dest: &mut [u8],
    source: &[u8],
    level: i32,
    produced: &mut usize,
) -> Result<usize, ReturnCode> {
    compress2_tracked_with::<OneCallDeflateEngine>(dest, source, level, produced)
}

/// Compresses `source` into `dest` at the library default compression level,
/// returning the number of bytes written to `dest`.
///
/// Faithful port of C `compress_z` / `compress` (`compress.c` L77-L85): a
/// convenience wrapper that forwards to [`compress2`] with
/// [`Z_DEFAULT_COMPRESSION`]. As with [`compress2`], `dest` must be at least
/// [`compress_bound(source.len())`](crate::util::compress_bound) bytes to be
/// guaranteed sufficient.
///
/// # Errors
///
/// Returns the same errors as [`compress2`]: [`ReturnCode::BufError`] if `dest`
/// is too small (a bad `level` cannot occur here, as the level is fixed).
///
/// # Examples
///
/// ```
/// # use zlib_rs::{compress, compress_bound, uncompress};
/// let plain = b"the quick brown fox";
/// let mut zlib = vec![0u8; compress_bound(plain.len())];
/// let n = compress(&mut zlib, plain).unwrap();
///
/// let mut out = vec![0u8; plain.len()];
/// assert_eq!(uncompress(&mut out, &zlib[..n]).unwrap(), plain.len());
/// assert_eq!(&out[..], plain);
/// ```
pub fn compress(dest: &mut [u8], source: &[u8]) -> Result<usize, ReturnCode> {
    compress2(dest, source, Z_DEFAULT_COMPRESSION)
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

    /// C `deflateGetDictionary` (`deflate.c` L625-L641) answers two questions in
    /// one call: how many window bytes currently constitute the dictionary
    /// (`min(strstart + lookahead, w_size)`), and — when a destination is
    /// supplied — what those bytes are.
    ///
    /// This covers the four cases that have no other witness in the suite: no
    /// installed state, an empty window, a length-only query (`None`, the
    /// idiomatic spelling of C's `Z_NULL` destination), and a destination
    /// **larger** than the reported length. The last one matters because the
    /// engine must write exactly `len` bytes and no more: a port that filled the
    /// caller's whole slice would corrupt memory the caller still owns, and the
    /// contract this function documents ("`dst` is at least the returned length")
    /// permits a longer buffer, not merely an exact-sized one.
    ///
    /// The truncation and window-slide halves of the contract are asserted
    /// separately below, because both need a small window to reach.
    #[test]
    fn deflate_get_dictionary_reports_and_copies_the_live_window() {
        // Without an installed state there is no window to report on, exactly as
        // C's `deflateStateCheck` rejection produces `Z_STREAM_ERROR`.
        let bare: ZStream = ZStream::new();
        assert_eq!(
            deflate_get_dictionary(&bare, None),
            Err(ZlibError::StreamError),
            "no state means no dictionary"
        );
        let mut untouched = [0xa5u8; 4];
        assert_eq!(
            deflate_get_dictionary(&bare, Some(&mut untouched)),
            Err(ZlibError::StreamError)
        );
        assert_eq!(
            untouched, [0xa5u8; 4],
            "a rejected call must not write to the destination"
        );

        // A freshly initialized stream has `strstart == lookahead == 0`, so the
        // dictionary is empty and the `len != 0` guard must suppress the copy
        // entirely rather than writing a zero-length slice's worth of anything.
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        assert_eq!(deflate_get_dictionary(&strm, None), Ok(0));
        let mut sink = [0xa5u8; 8];
        assert_eq!(deflate_get_dictionary(&strm, Some(&mut sink)), Ok(0));
        assert_eq!(
            sink, [0xa5u8; 8],
            "an empty dictionary must not write a single byte"
        );

        // A dictionary shorter than the window comes back verbatim. `w_size` here
        // is 32 KiB, so nothing is truncated and nothing has slid.
        const DICT: &[u8] = b"the quick brown fox jumps over the lazy dog";
        deflate_set_dictionary(&mut strm, DICT).expect("a preset dictionary is accepted");
        assert_eq!(
            deflate_get_dictionary(&strm, None),
            Ok(DICT.len()),
            "the length-only query reports the dictionary length"
        );

        let mut exact = vec![0u8; DICT.len()];
        assert_eq!(
            deflate_get_dictionary(&strm, Some(&mut exact)),
            Ok(DICT.len())
        );
        assert_eq!(exact, DICT, "the exact bytes, in order");

        // A destination longer than the reported length receives exactly `len`
        // bytes; every byte past that stays as the caller left it.
        let mut oversized = vec![0x5au8; DICT.len() + 16];
        assert_eq!(
            deflate_get_dictionary(&strm, Some(&mut oversized)),
            Ok(DICT.len())
        );
        assert_eq!(&oversized[..DICT.len()], DICT);
        assert!(
            oversized[DICT.len()..].iter().all(|&b| b == 0x5a),
            "bytes beyond the dictionary length must be left untouched"
        );
    }

    /// An **undersized** destination is a caller contract violation, and the port
    /// turns it into a deterministic panic rather than the silent memory
    /// corruption C would produce.
    ///
    /// C `deflateGetDictionary` receives a bare `Bytef *` with no length and
    /// writes `min(strstart + lookahead, w_size)` bytes through it, so a caller
    /// who guessed the size too small overruns their own buffer with no
    /// diagnostic anywhere. The Rust signature carries the destination's length,
    /// so the same mistake is detectable — and this test pins that it is
    /// *detected* rather than papered over. A port that silently truncated to
    /// `dst.len()` would be worse than either: it would return a short
    /// dictionary while reporting the full length, and a caller passing it back
    /// to `deflateSetDictionary` would then produce a stream that decodes to the
    /// wrong bytes with no error at any step.
    #[test]
    #[should_panic(expected = "destination holds 42 bytes but the dictionary is 43")]
    fn deflate_get_dictionary_rejects_an_undersized_destination() {
        const DICT: &[u8] = b"the quick brown fox jumps over the lazy dog";
        let mut strm = init(6, MAX_WBITS, DEF_MEM_LEVEL, Strategy::Default);
        deflate_set_dictionary(&mut strm, DICT).expect("a preset dictionary is accepted");
        assert_eq!(deflate_get_dictionary(&strm, None), Ok(DICT.len()));

        // One byte short of the reported length.
        let mut too_small = vec![0u8; DICT.len() - 1];
        let _ = deflate_get_dictionary(&strm, Some(&mut too_small));
    }

    /// A preset dictionary longer than the window keeps only its **tail**: C
    /// advances the pointer (`dictionary += dictLength - s->w_size`) and clamps
    /// the length (`deflate.c` L596-L601), so the retained bytes are the most
    /// recent `w_size`, not the first `w_size`.
    ///
    /// `windowBits = 9` (a 512-byte window) makes the truncation reachable with a
    /// small fixture; the payload is a non-repeating ramp so keeping the wrong
    /// half is impossible to mistake for keeping the right one.
    #[test]
    fn deflate_get_dictionary_keeps_the_tail_of_an_oversized_dictionary() {
        const W_BITS: i32 = 9;
        let w_size = 1usize << W_BITS;
        let mut strm = init(6, W_BITS, DEF_MEM_LEVEL, Strategy::Default);
        assert_eq!(
            strm.deflate_state().expect("init installs a state").w_size,
            w_size
        );

        // Three times the window, so the truncation is unambiguous.
        let dict: Vec<u8> = (0..(w_size * 3) as u32).map(|i| (i % 251) as u8).collect();
        deflate_set_dictionary(&mut strm, &dict).expect("an oversized dictionary is accepted");

        assert_eq!(
            deflate_get_dictionary(&strm, None),
            Ok(w_size),
            "the dictionary is clamped to the window size"
        );
        let mut got = vec![0u8; w_size];
        assert_eq!(deflate_get_dictionary(&strm, Some(&mut got)), Ok(w_size));
        assert_eq!(
            got,
            dict[dict.len() - w_size..],
            "the retained bytes are the tail of the dictionary, not its head"
        );
    }

    /// After the window has slid, the dictionary is still the most recent
    /// `strstart + lookahead` bytes that passed through the encoder — and that
    /// count is deliberately **not** re-derived from how much input was fed.
    ///
    /// This is the case a naive port gets wrong twice over. First, `start =
    /// strstart + lookahead - len` indexes a window whose contents have been
    /// memmoved down by `w_size` one or more times, so an off-by-`w_size` here
    /// returns plausible but wrong history. Second, C `fill_window` performs
    /// `s->strstart -= wsize` on every slide (`deflate.c` L280-L288), which
    /// leaves `strstart` somewhere in `[MAX_DIST(s), w_size)` rather than pinned
    /// at the window size — so `deflateGetDictionary` reports **fewer** than
    /// `w_size` bytes even though the window physically retains more history.
    /// That is counter-intuitive and is exactly why it is asserted rather than
    /// assumed: a port that "helpfully" reported `w_size` here would diverge
    /// from C on every stream longer than one window.
    ///
    /// A 512-byte window and a 4000-byte payload force several slides (proven by
    /// [`DeflateState::slid`]), and the non-repeating ramp payload makes any
    /// offset error visible.
    #[test]
    fn deflate_get_dictionary_follows_the_window_across_slides() {
        const W_BITS: i32 = 9;
        let w_size = 1usize << W_BITS;
        let mut strm = init(6, W_BITS, DEF_MEM_LEVEL, Strategy::Default);

        let payload: Vec<u8> = (0..4000u32)
            .map(|i| (i.wrapping_mul(37) % 253) as u8)
            .collect();
        let mut out = vec![0u8; deflate_bound(&strm, payload.len()) + 64];
        let outcome = deflate(&mut strm, &payload, &mut out, Z_FINISH);
        assert_eq!(outcome.code, ReturnCode::StreamEnd);
        assert_eq!(outcome.consumed, payload.len(), "all input consumed");

        let expected_len = {
            let s = strm.deflate_state().expect("init installs a state");
            assert_eq!(s.lookahead, 0, "Z_FINISH drains the lookahead");
            assert!(
                s.slid,
                "the window must have slid for this test to mean anything"
            );
            assert!(
                s.strstart < payload.len(),
                "a slid window cannot hold the whole payload linearly (strstart {})",
                s.strstart
            );
            (s.strstart + s.lookahead).min(w_size)
        };
        assert!(
            expected_len > 0 && expected_len <= w_size,
            "the reported length stays inside the window (got {expected_len})"
        );

        assert_eq!(deflate_get_dictionary(&strm, None), Ok(expected_len));
        let mut got = vec![0u8; expected_len];
        assert_eq!(
            deflate_get_dictionary(&strm, Some(&mut got)),
            Ok(expected_len)
        );
        assert_eq!(
            got,
            payload[payload.len() - expected_len..],
            "the dictionary is the most recent strstart+lookahead input bytes"
        );
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

    // =======================================================================
    // One-call façade — round trips and error paths for `compress` / `compress2`
    //
    // Relocated here with the entry points themselves: `crate::util::compress`
    // is layer 3 and may not name an engine, so the tests that drive the real
    // engine belong in the layer that owns it (AAP §0.3.1, §0.4.2 B2). The
    // engine-free sizing formula and the scripted-engine driver tests stay in
    // `crate::util::compress`.
    //
    // These decompress with `flate2` (its pure-Rust `miniz_oxide` backend) to
    // prove the emitted stream is valid zlib. `std` is available under
    // `cfg(test)` even though the crate is `no_std`, so `Vec`, `vec!`, and
    // `std::io` may be used freely here.
    // =======================================================================

    use crate::util::compress_bound;
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    // ---------------------------------------------------------------------
    // Round-trip helpers
    // ---------------------------------------------------------------------

    /// Decompresses a complete zlib stream with `flate2`, returning the bytes.
    fn inflate_with_flate2(compressed: &[u8]) -> Vec<u8> {
        let mut decoder = ZlibDecoder::new(compressed);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .expect("compress2 must emit a valid zlib stream");
        out
    }

    /// Compresses `data` at `level` into a `compress_bound`-sized buffer, then
    /// verifies it decompresses back to `data`.
    fn assert_round_trip(level: i32, data: &[u8]) {
        let mut buf = vec![0u8; compress_bound(data.len())];
        let produced = compress2(&mut buf, data, level)
            .unwrap_or_else(|err| panic!("compress2 at level {level} failed: {err:?}"));
        let restored = inflate_with_flate2(&buf[..produced]);
        assert_eq!(restored, data, "round-trip mismatch at level {level}");
    }

    /// Deterministic, effectively-incompressible bytes (a simple LCG), so the
    /// tests need no `rand` dependency yet still exercise the stored-block path.
    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut state: u32 = 0x1234_5678;
        for byte in &mut out {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *byte = (state >> 24) as u8;
        }
        out
    }

    // ---------------------------------------------------------------------
    // compress2 — round-trip across representative inputs and every level
    // ---------------------------------------------------------------------

    #[test]
    fn compress2_round_trips_all_inputs_and_levels() {
        let empty: Vec<u8> = Vec::new();
        let small = b"hello, zlib-rs one-call compression!".to_vec();
        let compressible = vec![b'A'; 50_000]; // long run — compresses tiny
        let incompressible = pseudo_random(40_000); // ~stored size

        for &level in &[0i32, 1, 6, 9, Z_DEFAULT_COMPRESSION] {
            assert_round_trip(level, &empty);
            assert_round_trip(level, &small);
            assert_round_trip(level, &compressible);
            assert_round_trip(level, &incompressible);
        }
    }

    #[test]
    fn compress2_highly_compressible_shrinks() {
        // A 50 KiB single-byte run must compress to far fewer bytes at level 9.
        let data = vec![b'Q'; 50_000];
        let mut buf = vec![0u8; compress_bound(data.len())];
        let produced = compress2(&mut buf, &data, 9).expect("compress2 failed");
        assert!(produced < data.len() / 10, "expected strong compression");
        assert_eq!(inflate_with_flate2(&buf[..produced]), data);
    }

    #[test]
    fn compress2_empty_input_with_adequate_dest_succeeds() {
        // 16 >= compress_bound(0) == 13, so the header + empty block + trailer fit.
        let mut buf = [0u8; 16];
        let produced = compress2(&mut buf, &[], 6).expect("empty-input compress2 failed");
        assert!(
            produced > 0,
            "an empty input still emits header + block + trailer"
        );
        assert_eq!(inflate_with_flate2(&buf[..produced]), Vec::<u8>::new());
    }

    // ---------------------------------------------------------------------
    // Error paths
    // ---------------------------------------------------------------------

    #[test]
    fn compress2_too_small_dest_yields_buf_error() {
        let data = vec![b'Z'; 4096];
        // One byte cannot even hold the two-byte zlib header.
        let mut tiny = [0u8; 1];
        assert_eq!(compress2(&mut tiny, &data, 6), Err(ReturnCode::BufError));
    }

    #[test]
    fn compress2_invalid_level_yields_stream_error() {
        let data = b"some data to compress";
        let mut buf = [0u8; 64];
        // Above the valid 0..=9 range.
        assert_eq!(compress2(&mut buf, data, 42), Err(ReturnCode::StreamError));
        // Below the range and not the Z_DEFAULT_COMPRESSION (-1) sentinel.
        assert_eq!(compress2(&mut buf, data, -2), Err(ReturnCode::StreamError));
    }

    // ---------------------------------------------------------------------
    // compress2_tracked — the produced-count out-parameter
    // ---------------------------------------------------------------------

    #[test]
    fn compress2_tracked_reports_the_count_on_every_post_init_path() {
        let data = vec![b'Q'; 4096];

        // Success: the out-parameter agrees with the `Ok` payload.
        let mut roomy = vec![0u8; compress_bound(data.len())];
        let mut produced = usize::MAX;
        let n = compress2_tracked(&mut roomy, &data, 6, &mut produced).expect("ok");
        assert_eq!(produced, n, "the out-parameter matches the returned length");

        // Buffer error: C writes `next_out - dest` at `compress.c` L63 on this
        // path too, so the bytes that fit are reported — and they are a
        // byte-exact prefix of the complete stream.
        let mut short = [0u8; 12];
        let mut produced = usize::MAX;
        assert_eq!(
            compress2_tracked(&mut short, &data, 6, &mut produced),
            Err(ReturnCode::BufError)
        );
        assert_eq!(produced, short.len(), "the partial count is reported");
        assert_eq!(&short[..], &roomy[..short.len()]);

        // Invalid level: C zeroes the count at `compress.c` L36 before the
        // failing `deflateInit` (L42-L43), so zero is reported.
        let mut buf = [0u8; 64];
        let mut produced = usize::MAX;
        assert_eq!(
            compress2_tracked(&mut buf, &data, 42, &mut produced),
            Err(ReturnCode::StreamError)
        );
        assert_eq!(produced, 0, "a failed initializer reports zero bytes");
    }

    // ---------------------------------------------------------------------
    // compress — the default-level convenience wrapper
    // ---------------------------------------------------------------------

    #[test]
    fn compress_default_level_round_trips() {
        let data = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
        let mut buf = vec![0u8; compress_bound(data.len())];
        let produced = compress(&mut buf, &data).expect("compress failed");
        assert_eq!(inflate_with_flate2(&buf[..produced]), data);
    }

    #[test]
    fn compress_matches_compress2_default_level() {
        // `compress` must be exactly `compress2(.., Z_DEFAULT_COMPRESSION)`.
        let data = b"determinism check: compress == compress2(-1)".repeat(64);
        let mut buf_a = vec![0u8; compress_bound(data.len())];
        let mut buf_b = vec![0u8; compress_bound(data.len())];
        let na = compress(&mut buf_a, &data).unwrap();
        let nb = compress2(&mut buf_b, &data, Z_DEFAULT_COMPRESSION).unwrap();
        assert_eq!(na, nb);
        assert_eq!(buf_a[..na], buf_b[..nb], "identical bytes expected");
    }

    // =======================================================================
    // Shared `zutil.h` constants — engine-side cross-checks
    //
    // Relocated from `crate::util`'s own test module: `util` is layer 3 and may
    // not name an engine, not even under `cfg(test)` (AAP §0.3.1, §0.4.2 B2,
    // enforced by `the_module_graph_has_no_upward_edges` in `src/lib.rs`). The
    // assertions themselves are unchanged — they check that this module's
    // differently-typed aliases still agree with `util`'s canonical values, and
    // that the gzip header this module emits carries the one platform-selected
    // `OS_CODE`.
    // =======================================================================

    #[cfg(feature = "gzip")]
    use crate::util::OS_CODE;
    use crate::util::{
        DYN_TREES, MAX_MATCH as UTIL_MAX_MATCH, MIN_MATCH as UTIL_MIN_MATCH, STATIC_TREES,
        STORED_BLOCK,
    };

    /// The expected gzip OS byte for the target this test module is compiled for,
    /// derived from the same `cfg` predicates `OS_CODE` itself uses — but derived
    /// *here*, independently, so a mistake in a single `cfg` attribute cannot make
    /// both sides agree.
    ///
    /// Written as a `cfg`-selected constant rather than a runtime `if` so that
    /// **every** target gets exactly one value and the assertions below can never
    /// be vacuous: on a target where no arm applied the constant would not exist
    /// and this module would fail to compile.
    #[cfg(all(feature = "gzip", windows))]
    const EXPECTED_OS_CODE: u8 = 10;
    #[cfg(all(feature = "gzip", not(windows), target_vendor = "apple"))]
    const EXPECTED_OS_CODE: u8 = 19;
    #[cfg(all(feature = "gzip", not(windows), not(target_vendor = "apple")))]
    const EXPECTED_OS_CODE: u8 = 3;
    /// The differently-typed aliases the engines keep for arithmetic locality
    /// still agree with the canonical values above.
    ///
    /// Without this guard the two sets could drift silently: nothing in the
    /// compiler relates `crate::deflate::trees::STORED_BLOCK` (an `i32`, needed
    /// by `send_bits`) or `crate::deflate::state::MIN_MATCH` (a `usize`, needed
    /// for window indexing) to the `u8`/`usize` values published here.
    #[test]
    fn engine_aliases_agree_with_the_canonical_values() {
        use crate::deflate::state::{MAX_MATCH as ST_MAX, MIN_MATCH as ST_MIN};
        use crate::deflate::trees::{
            DYN_TREES as TR_DYN, STATIC_TREES as TR_STATIC, STORED_BLOCK as TR_STORED,
        };

        assert_eq!(TR_STORED, i32::from(STORED_BLOCK), "trees::STORED_BLOCK");
        assert_eq!(TR_STATIC, i32::from(STATIC_TREES), "trees::STATIC_TREES");
        assert_eq!(TR_DYN, i32::from(DYN_TREES), "trees::DYN_TREES");
        assert_eq!(ST_MIN, UTIL_MIN_MATCH, "state::MIN_MATCH");
        assert_eq!(ST_MAX, UTIL_MAX_MATCH, "state::MAX_MATCH");
    }

    /// The gzip header this crate emits carries the canonical [`OS_CODE`] — the
    /// one platform-selected definition — at RFC 1952 offset 9.
    ///
    /// This is the assertion that catches the defect the test exists for: if
    /// `crate::deflate` were to hard-code an `OS_CODE = 3` of its own, then on
    /// Windows or Apple it would emit `3` where reference zlib emits `10` or
    /// `19` — diverging from byte-identity on those targets while every
    /// Linux-only gate stayed green.
    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_header_emits_the_canonical_os_code() {
        const DATA: &[u8] = b"gzip header operating-system byte";

        let mut strm: ZStream = ZStream::new();
        // windowBits 31 == 15 + 16: gzip framing.
        deflate_init2(&mut strm, 6, Z_DEFLATED, 31, 8, Strategy::Default).expect("init");
        let mut out = alloc::vec![0u8; DATA.len() * 2 + 128];
        let r = deflate(&mut strm, DATA, &mut out, Z_FINISH);
        assert_eq!(r.code, ReturnCode::StreamEnd);
        out.truncate(r.produced);
        deflate_end(&mut strm).expect("end");

        // RFC 1952 §2.3: ID1 ID2 CM FLG MTIME[4] XFL OS -> OS is byte 9.
        assert!(out.len() > 9, "a gzip member has at least a 10-byte header");
        assert_eq!(out[0], 0x1f, "ID1");
        assert_eq!(out[1], 0x8b, "ID2");
        assert_eq!(out[2], 8, "CM == Z_DEFLATED");
        assert_eq!(
            out[9], OS_CODE,
            "the emitted OS byte must be the single canonical OS_CODE"
        );
        assert_eq!(
            out[9], EXPECTED_OS_CODE,
            "and it must be the platform value"
        );
    }

    /// A caller-supplied [`GzHeader`](crate::gz_header::GzHeader) overrides the
    /// OS byte, exactly as C writes `s->gzhead->os & 0xff` (`deflate.c` L1105)
    /// instead of `OS_CODE` (L1081) when a header is installed. The override path
    /// must therefore *not* be rewired to the canonical constant.
    #[cfg(feature = "gzip")]
    #[test]
    fn supplied_header_overrides_the_os_code() {
        use crate::gz_header::GzHeader;

        const DATA: &[u8] = b"explicit gzip header";
        const CUSTOM_OS: u8 = 7; // old Mac OS, zutil.h L149

        let mut strm: ZStream = ZStream::new();
        deflate_init2(&mut strm, 6, Z_DEFLATED, 31, 8, Strategy::Default).expect("init");
        let head = GzHeader {
            os: i32::from(CUSTOM_OS),
            ..GzHeader::default()
        };
        deflate_set_header(&mut strm, Some(head)).expect("set header");

        let mut out = alloc::vec![0u8; DATA.len() * 2 + 128];
        let r = deflate(&mut strm, DATA, &mut out, Z_FINISH);
        assert_eq!(r.code, ReturnCode::StreamEnd);
        out.truncate(r.produced);
        deflate_end(&mut strm).expect("end");

        assert_eq!(out[9], CUSTOM_OS, "a supplied header wins over OS_CODE");
    }
}
