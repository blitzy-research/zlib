//! One-call, buffer-to-buffer decompression helpers — the safe-Rust port of C
//! `uncompr.c` (AAP §0.4.1, source `uncompr.c`).
//!
//! This module owns the whole of `uncompr.c`'s decode-driving logic as
//! `uncompress2_with`: the `inflateInit` / `inflate` / `inflateEnd` sequence,
//! the consumed/produced accounting C publishes on every post-init path, and the
//! return-code folding of `uncompr.c` L78-L81.
//!
//! # Layering: why the public entry points live in `crate::inflate`
//!
//! This module is layer 3 of the seven-layer graph and the decompression engine
//! is layer 6, so imports must run *downward only* (AAP §0.3.1, §0.4.2 B2). The
//! driver therefore names no engine at all: it is generic over the
//! `OneCallInflate` port declared here, and the engine supplies the adapter.
//! The two C-named entry points — `uncompress` and `uncompress2` — are
//! consequently defined one layer up, in [`crate::inflate`], which is also where
//! the C `uncompr.c` translation unit sits in the `#include` order (it includes
//! `zlib.h`, not `zutil.h`). Both are re-exported unchanged from the crate root,
//! so `zlib_rs::uncompress` and `zlib_rs::uncompress2` are exactly the names
//! `zlib.h` publishes.
//!
//! # Relationship to the C original
//!
//! C `uncompr.c` exposes four symbols: the size-generic `uncompress2_z`
//! (the real implementation), the `uLong`-typed `uncompress2`, and the
//! `sourceLen`-by-value shims `uncompress_z` / `uncompress`. In safe Rust a
//! slice already carries its own length, so the `size_t`-vs-`uLong` distinction
//! collapses: there is a single size-generic implementation
//! (`uncompress2_with`) and a single convenience wrapper
//! (`crate::inflate::uncompress`). The raw-pointer, `z_size_t`/`uLong` FFI shims
//! that reproduce the exact C ABI live in `src/ffi/util.rs` and are **not** part
//! of this module.
//!
//! # Safety and portability
//!
//! * **Zero `unsafe`.** Everything here operates over safe slices; the pointer
//!   validation and the `next_out`-cannot-be-`NULL` scratch trick from the C
//!   original (`uncompr.c` L42-L43) are FFI-boundary concerns and are
//!   unnecessary in the slice-based API — an empty `&mut []` output is already a
//!   valid, non-null buffer.
//! * **`no_std` friendly.** The module depends only on `core` (through the
//!   crate's shared types) and never references `std`.
//!
//! # Byte-exact parity
//!
//! Decompressed output is byte-identical to reference zlib for the same input
//! (AAP §0.6.4 / §0.8.1 directive D-1); the subtle return-code mapping of `uncompr.c`
//! (L78-L81) — folding `Z_NEED_DICT` and "all input consumed but stream not
//! finished" into `Z_DATA_ERROR` — is reproduced exactly in
//! `uncompress2_with`.

use crate::constants::Z_NO_FLUSH;
use crate::error::ReturnCode;
use crate::util::compress::OneCallStep;

/// The abstract streaming decompressor that [`uncompress2_with`] drives.
///
/// This is the seam that keeps `uncompr.c`'s driver in layer 3 while the engine
/// stays in layer 6 (AAP §0.3.1, §0.4.2 B2): the driver depends on this trait,
/// and `crate::inflate` — one layer up — supplies the only implementation. The
/// three methods are exactly the three C calls `uncompress2_z` makes, in order:
/// `inflateInit`, `inflate`, `inflateEnd`.
pub(crate) trait OneCallInflate: Sized {
    /// C `inflateInit(&stream)` (`uncompr.c` L50), i.e. the default
    /// `windowBits` of 15.
    ///
    /// # Errors
    ///
    /// Returns [`ReturnCode::MemError`] when the engine state cannot be
    /// allocated — the only failure C's `inflateInit` reports here.
    fn begin() -> Result<Self, ReturnCode>;

    /// C `inflate(&stream, Z_NO_FLUSH)` (`uncompr.c` L58).
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> OneCallStep;

    /// C `inflateEnd(&stream)` (`uncompr.c` L76). C discards the return value,
    /// so this reports nothing.
    fn end(&mut self);
}

/// Decompresses the whole zlib stream in `source` into `dest` using the engine
/// `E`, reporting the number of source bytes consumed.
///
/// This is the complete transcription of C `uncompress2_z` (`uncompr.c`
/// L29-L90) and the shared core of `crate::inflate::uncompress` and
/// `crate::inflate::uncompress2`. It initializes a fresh engine with the default
/// `windowBits` (15, i.e. C `inflateInit`), feeds the entire input through it,
/// and tears the engine down before returning.
///
/// # Parameters
///
/// * `dest` — the output buffer. It must be large enough to hold the *entire*
///   decompressed payload; its length is the available output capacity. (As
///   with C zlib, the uncompressed size must have been recorded by the
///   compressor and communicated out of band.)
/// * `source` — the compressed input.
/// * `source_len` — an **in/out** parameter:
///   * **On entry** it caps how many bytes of `source` may be read. The number
///     of input bytes actually made available to the engine is
///     `source.len().min(*source_len)`, so passing `source.len()` (or any larger
///     value) simply means "use the whole slice". This mirrors the C contract
///     where `*sourceLen` is the length of the `source` buffer.
///   * **On exit** it is overwritten with the number of source bytes actually
///     consumed (C `*sourceLen -= len`). After a successful call,
///     `source[*source_len..]` is the first unused input byte onward — exactly
///     C's "`source + *sourceLen` points to the first unused input byte".
/// * `dest_len` — an **out** parameter written with the number of output bytes
///   actually produced (C `*destLen -= left`, i.e. `total_out`). Reproducing C
///   `uncompress2_z`, this is written on **every** path that reaches the decode
///   loop — success **and** failure — so a caller observing `Z_BUF_ERROR` (or
///   any error) still learns how many bytes were written before the failure.
///
/// Both out-parameters are written on every path that reaches the decode loop
/// (success *and* failure); they are left untouched only if engine
/// initialization itself fails, matching C's early `return err;` after a failed
/// `inflateInit`.
///
/// # Returns
///
/// * `Ok(produced)` — the stream was fully decompressed; `produced` bytes were
///   written to the front of `dest`.
/// * `Err(code)` — decompression failed; `code` follows the exact `uncompr.c`
///   L78-L81 mapping (see *Errors*).
///
/// # Errors
///
/// Reproduces `uncompr.c` L78-L81 verbatim:
///
/// * [`ReturnCode::DataError`] — the input was corrupt, **or** it was an
///   incomplete / truncated zlib stream (all input was consumed yet the stream
///   never reached its end), **or** a preset dictionary was required
///   (`Z_NEED_DICT`, which the one-call path cannot supply and therefore treats
///   as a data error).
/// * [`ReturnCode::BufError`] — `dest` was too small to hold the decompressed
///   data while input still remained to be read.
/// * [`ReturnCode::MemError`] — the engine could not allocate its working state.
/// * Any other code surfaced unchanged by the underlying engine.
pub(crate) fn uncompress2_with<E: OneCallInflate>(
    dest: &mut [u8],
    source: &[u8],
    source_len: &mut usize,
    dest_len: &mut usize,
) -> Result<usize, ReturnCode> {
    // Amount of input the engine is allowed to see. Honor the caller-declared
    // length (C `*sourceLen`) but never read past the end of the slice.
    let available = source.len().min(*source_len);

    // C `inflateInit(&stream)` with the default window size. On failure we return
    // immediately WITHOUT touching `*source_len`, matching C `uncompress2_z`
    // which does `return err;` before it computes the consumed/produced
    // accounting.
    let mut engine = E::begin()?;

    // Drive the engine. Because the idiomatic engine carries no
    // `next_in`/`next_out` cursor fields and the driver consumes/produces whole
    // slices per call, the C `uInt`-sized chunking loop (`avail_in`/`avail_out`
    // capped at `(uInt)-1`) collapses to: hand the engine the *remaining* input
    // and output, add the reported progress to our running cursors, and repeat
    // while the engine keeps returning [`ReturnCode::Ok`] — exactly C's
    // `do { … } while (err == Z_OK);` (`uncompr.c` L57-L67).
    let mut in_pos: usize = 0; // total input consumed  (C: original sourceLen - len)
    let mut out_pos: usize = 0; // total output produced (C: original destLen  - left)
    let code = loop {
        let outcome = engine.step(&source[in_pos..available], &mut dest[out_pos..], Z_NO_FLUSH);
        in_pos += outcome.consumed;
        out_pos += outcome.produced;

        // Any non-`Ok` code terminates the loop (C's `while (err == Z_OK)`).
        if outcome.code != ReturnCode::Ok {
            break outcome.code;
        }

        // Defensive termination: a conforming engine never reports `Ok` while
        // making no progress — it returns `BufError` when it is stalled for
        // want of input or output. Guard against a hang regardless, folding a
        // stall into the same `BufError` C would observe on its next iteration
        // (an empty `avail_in`/`avail_out` refill yields `Z_BUF_ERROR`).
        if outcome.consumed == 0 && outcome.produced == 0 {
            break ReturnCode::BufError;
        }
    };

    // C `inflateEnd(&stream)`. Discarding the result is intentional: our return
    // value has already been decided, and dropping the engine afterwards is safe
    // because its state has been released (RAII then sees an empty stream — no
    // double free).
    engine.end();

    // C accounting (`uncompr.c` L69-L74): after the loop, `len` is the number of
    // input bytes NOT consumed and `*sourceLen` becomes the number that WAS.
    // Both counts are published on every post-init path (success AND failure),
    // exactly as C `uncompress2_z` computes `*sourceLen -= len` / `*destLen -=
    // left` before `return err;`, so a caller that hit `Z_BUF_ERROR` still sees
    // how many output bytes were produced before the buffer ran out.
    let leftover_in = available - in_pos;
    *source_len = in_pos;
    *dest_len = out_pos;

    // Return mapping — reproduces `uncompr.c` L78-L81 exactly:
    //   Z_STREAM_END                              -> Ok(produced)
    //   Z_NEED_DICT                               -> Z_DATA_ERROR
    //   Z_BUF_ERROR with every input byte consumed-> Z_DATA_ERROR (truncated)
    //   otherwise                                 -> the engine's code unchanged
    // A `Z_BUF_ERROR` with input still pending (`leftover_in > 0`) means the
    // output buffer was too small, and falls through to `Err(BufError)`.
    match code {
        ReturnCode::StreamEnd => Ok(out_pos),
        ReturnCode::NeedDict => Err(ReturnCode::DataError),
        ReturnCode::BufError if leftover_in == 0 => Err(ReturnCode::DataError),
        other => Err(other),
    }
}
