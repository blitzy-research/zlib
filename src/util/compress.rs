//! One-call, buffer-to-buffer compression helpers — the safe-Rust port of the
//! C `compress.c` (zlib `1.3.2.1-motley`).
//!
//! This module owns the *whole* of `compress.c`'s logic:
//!
//! * [`compress_bound`] (and its C-named alias [`compressBound`]) — the
//!   bit-exact worst-case sizing formula (C `compressBound_z`, `compress.c`
//!   L91-L99), so callers can size the destination buffer.
//! * `compress2_tracked_with` — the complete `compress2_z` driver
//!   (`compress.c` L24-L66): the `u32::MAX` chunking loop, the
//!   `Z_NO_FLUSH`/`Z_FINISH` schedule, the unconditional produced-length
//!   publication, and the `Z_STREAM_END` → `Z_OK` remap.
//!
//! # Layering: why the public entry points live in `crate::deflate`
//!
//! This module is layer 3 of the seven-layer graph and the compression engine is
//! layer 6, so imports must run *downward only* (AAP §0.3.1, §0.4.2 B2). The
//! driver therefore names no engine at all: it is generic over the
//! `OneCallDeflate` port declared here, and the engine supplies the adapter.
//! The three C-named entry points — `compress`, `compress2`, and the
//! length-tracking `compress2_tracked` the FFI shims need — are consequently
//! defined one layer up, in [`crate::deflate`], which is also where the C
//! `compress.c` translation unit sits in the `#include` order (it includes
//! `zlib.h`, not `zutil.h`). They are re-exported unchanged both from the crate
//! root and from [`crate::util`] itself, so `zlib_rs::compress`,
//! `zlib_rs::compress2`, `zlib_rs::compress_bound` and their
//! `zlib_rs::util::…` spellings all resolve to exactly the names `zlib.h`
//! publishes (AAP §0.3.1). Re-exporting a *name* upward costs the layer graph
//! nothing — what §0.4.2 B2 forbids is a *code* dependency running upward, and
//! neither this module nor [`crate::util`] calls an engine or mentions an engine
//! type in any signature of its own.
//!
//! # Relationship to the C originals
//!
//! The C library splits each helper into a `size_t`-generic `_z` variant and a
//! `uLong` wrapper that merely narrows the width (`compress2_z`/`compress2`,
//! `compress_z`/`compress`, `compressBound_z`/`compressBound`). Rust's [`usize`]
//! unifies C's `size_t`/`uLong` split, so this module exposes a single
//! `usize`-based function per helper. The raw-pointer `extern "C"` shims that
//! reproduce the exact C signatures live separately in `src/ffi/util.rs` and are
//! not this module's concern.
//!
//! # Fidelity
//!
//! The compression loop in `compress2_tracked_with` is a faithful transcription
//! of C `compress2_z` (`compress.c` L24-L66): input and output are offered to the
//! engine in `u32::MAX`-sized chunks, [`crate::constants::FlushMode::NoFlush`] is used while input
//! remains and [`crate::constants::FlushMode::Finish`] once every byte has been handed over, and
//! the terminal `Z_STREAM_END` is remapped to success. Preserving this exact
//! multi-call engine sequence is what keeps the emitted stream byte-identical
//! to reference zlib. [`compress_bound`] reproduces the sizing formula bit-for-
//! bit — including the saturate-to-maximum overflow behavior — because callers
//! pre-allocate their output buffers against it (AAP §0.6.4, §0.8.1 directive D-1).
//!
//! # Safety and portability
//!
//! There is **zero `unsafe`** in this module and no dependency on `std`: it
//! operates purely over slices (`&[u8]` / `&mut [u8]`) and drives an abstract
//! engine through a safe trait. It is `no_std` + `alloc` compatible (the engine
//! performs its own allocation through the stream's allocator). Targets Rust 2024
//! edition, MSRV 1.85.0.

use crate::constants::FlushMode;
use crate::error::ReturnCode;

/// Returns an upper bound on the compressed size of `source_len` bytes.
///
/// This is the idiomatic, `usize`-based port of C `compressBound_z`
/// (`compress.c` L91-L99). The bound is computed as
///
/// ```text
/// source_len + (source_len >> 12) + (source_len >> 14) + (source_len >> 25) + 13
/// ```
///
/// which reserves enough room for the worst-case DEFLATE expansion (a stream of
/// incompressible data emitted as stored blocks) plus the zlib wrapper's header
/// and trailer. The result is **bit-exact** with reference zlib for every input,
/// so a caller that sizes its destination buffer to `compress_bound(n)` and then
/// calls [`compress`](crate::deflate::compress) / [`compress2`](crate::deflate::compress2)
/// on `n` bytes is guaranteed enough space
/// (AAP §0.6.4, §0.8.1 directive D-1).
///
/// # Overflow
///
/// The three right-shift terms can only shrink `source_len`, so the sole
/// overflow risk is the final accumulation near the top of the `usize` range. C
/// detects this with `bound < sourceLen ? (z_size_t)-1 : bound` (unsigned
/// wraparound yields the all-ones sentinel). This port reproduces that semantic
/// exactly: the additions are chained through [`usize::checked_add`], and any
/// overflow saturates the result to [`usize::MAX`].
///
/// # Examples
///
/// For an empty input the bound is exactly the `13`-byte overhead reserve, and
/// the top of the range saturates rather than wrapping:
///
/// ```text
/// compress_bound(0)          == 13
/// compress_bound(100_000)    == 100_043   // 100000 + 24 + 6 + 0 + 13
/// compress_bound(usize::MAX) == usize::MAX // overflow saturates
/// ```
#[must_use]
pub fn compress_bound(source_len: usize) -> usize {
    source_len
        .checked_add(source_len >> 12)
        .and_then(|bound| bound.checked_add(source_len >> 14))
        .and_then(|bound| bound.checked_add(source_len >> 25))
        .and_then(|bound| bound.checked_add(13))
        .unwrap_or(usize::MAX)
}

/// C-named alias for [`compress_bound`].
///
/// zlib publishes this helper as `compressBound`; the crate root re-exports this
/// camelCase spelling so C-familiar callers (and the FFI layer) can use the
/// exact name from `zlib.h`. It is a thin, allocation-free forward to
/// [`compress_bound`] and returns an identical value for every input.
#[allow(non_snake_case)]
#[must_use]
pub fn compressBound(source_len: usize) -> usize {
    compress_bound(source_len)
}

/// One step of an abstract compression engine, mirroring the three values a C
/// `deflate()` call publishes into the caller's `z_stream`.
///
/// `consumed` is C's advance of `next_in`/`avail_in`, `produced` is the advance
/// of `next_out`/`avail_out`, and `code` is the integer `deflate()` returned.
pub(crate) struct OneCallStep {
    /// Input bytes the engine took from the offered window.
    pub(crate) consumed: usize,
    /// Output bytes the engine wrote into the offered window.
    pub(crate) produced: usize,
    /// The zlib return code the step produced.
    pub(crate) code: ReturnCode,
}

/// The abstract streaming compressor that [`compress2_tracked_with`] drives.
///
/// This is the seam that keeps `compress.c`'s driver in layer 3 while the engine
/// stays in layer 6 (AAP §0.3.1, §0.4.2 B2): the driver depends on this trait,
/// and `crate::deflate` — one layer up — supplies the only implementation. The
/// three methods are exactly the three C calls the original makes, in order:
/// `deflateInit`, `deflate`, `deflateEnd`.
pub(crate) trait OneCallDeflate: Sized {
    /// C `deflateInit(&stream, level)` (`compress.c` L40-L43).
    ///
    /// # Errors
    ///
    /// Returns [`ReturnCode::StreamError`] for a `level` outside `-1 | 0..=9`
    /// and [`ReturnCode::MemError`] if the engine's working buffers cannot be
    /// allocated — the two codes C's `deflateInit` reports here.
    fn begin(level: i32) -> Result<Self, ReturnCode>;

    /// C `deflate(&stream, flush)` (`compress.c` L58).
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: FlushMode) -> OneCallStep;

    /// C `deflateEnd(&stream)` (`compress.c` L65). C discards the return value,
    /// so this reports nothing.
    fn end(&mut self);
}

/// Compresses `source` into `dest` at `level` using the engine `E`, reporting the
/// produced byte count through `produced` on **every** path that reaches the
/// deflate loop.
///
/// This is the complete transcription of C `compress2_z` (`compress.c` L24-L66)
/// and the shared core of `crate::deflate::compress`,
/// `crate::deflate::compress2`, and `crate::deflate::compress2_tracked`.
///
/// The `produced` out-parameter mirrors C's *unconditional*
/// `*destLen = (z_size_t)(stream.next_out - dest);` (`compress.c` L63). C runs
/// that assignment after the loop and before `deflateEnd`, so it reports a
/// partial length on the `Z_BUF_ERROR` path just as it does on success. The
/// C-ABI shims in `src/ffi/util.rs` need that count to reproduce the behavior
/// exactly; the `Result`-shaped wrappers simply discard it, since their `Ok` arm
/// already carries the length.
///
/// `produced` is a pure out-parameter: it is seeded to `0` before initialization,
/// matching C's `*destLen = 0;` (`compress.c` L36), so a failing `begin` leaves
/// it at zero exactly as C leaves `*destLen` at zero when `deflateInit` fails
/// (`compress.c` L42-L43).
///
/// # Errors
///
/// * [`ReturnCode::StreamError`] — `level` is outside the valid set
///   (`-1` or `0..=9`); reported by the engine's initializer.
/// * [`ReturnCode::BufError`] — `dest` was too small to hold the complete
///   compressed stream.
/// * [`ReturnCode::MemError`] — the engine could not allocate its working
///   buffers, mirroring C `Z_MEM_ERROR`.
pub(crate) fn compress2_tracked_with<E: OneCallDeflate>(
    dest: &mut [u8],
    source: &[u8],
    level: i32,
    produced: &mut usize,
) -> Result<usize, ReturnCode> {
    // C `compress2_z` zeroes the reported length before initializing the engine
    // (`compress.c` L36: `*destLen = 0;`), so a failed `deflateInit` returns with
    // a reported length of zero. Seed it identically.
    *produced = 0;

    // C `deflateInit(&stream, level)`. An invalid `level` is rejected here
    // (`Z_STREAM_ERROR`), as is an allocation failure (`Z_MEM_ERROR`).
    let mut engine = E::begin(level)?;

    // The engine's per-call I/O width is C `uInt` == `u32`, so — exactly as C
    // `compress2_z` does — the loop offers the input and output to `deflate` in
    // `u32::MAX`-sized chunks. Bookkeeping mirrors the C locals:
    //   * `source_len` / `left`      — input / output not yet offered
    //     (C `sourceLen` / `left`);
    //   * `avail_in` / `avail_out`   — the currently offered window sizes
    //     (C `stream.avail_in` / `stream.avail_out`);
    //   * `in_pos` / `out_pos`       — absolute bytes consumed / produced so far
    //     (C `stream.next_in - source` / `stream.next_out - dest`).
    let max = u32::MAX as usize;
    let mut source_len = source.len();
    let mut left = dest.len();
    let mut avail_in: usize = 0;
    let mut avail_out: usize = 0;
    let mut in_pos: usize = 0;
    let mut out_pos: usize = 0;

    let code = loop {
        // Refill the offered output window once the previous one is exhausted.
        if avail_out == 0 {
            avail_out = left.min(max);
            left -= avail_out;
        }
        // Refill the offered input window once the previous one is exhausted.
        if avail_in == 0 {
            avail_in = source_len.min(max);
            source_len -= avail_in;
        }

        // C: `deflate(&stream, sourceLen ? Z_NO_FLUSH : Z_FINISH)`. Once every
        // byte has been moved into the offered window (`source_len == 0`), the
        // engine is told this is the final input via `Z_FINISH`.
        let flush = if source_len != 0 {
            FlushMode::NoFlush
        } else {
            FlushMode::Finish
        };

        // Offer exactly the current windows; the engine advances by the returned
        // `consumed`/`produced` counts (it never exceeds the slice lengths).
        let outcome = engine.step(
            &source[in_pos..in_pos + avail_in],
            &mut dest[out_pos..out_pos + avail_out],
            flush,
        );

        // Advance the absolute cursors and shrink the offered windows by the
        // amounts actually processed. These subtractions cannot underflow:
        // `consumed <= avail_in` and `produced <= avail_out` by construction.
        in_pos += outcome.consumed;
        avail_in -= outcome.consumed;
        out_pos += outcome.produced;
        avail_out -= outcome.produced;

        // C loops `while (err == Z_OK)`; every other code is terminal. When the
        // output is exhausted mid-`Z_FINISH`, the engine returns `Z_BUF_ERROR`,
        // which both breaks the loop and becomes the reported error below.
        if outcome.code != ReturnCode::Ok {
            break outcome.code;
        }
    };

    // C publishes the produced byte count here — after the loop, before
    // `deflateEnd`, and on every path (`compress.c` L63:
    // `*destLen = (z_size_t)(stream.next_out - dest);`). Assigning outside the
    // success test below is what makes a `Z_BUF_ERROR` report the bytes that did
    // fit, exactly as the reference library does.
    *produced = out_pos;

    // Release the engine. RAII would already free the state, but calling
    // `deflateEnd` mirrors C and is safe: it clears the state, so the later
    // `Drop` is a no-op (no double free). C ignores this return value, and so do
    // we.
    engine.end();

    // C: `return err == Z_STREAM_END ? Z_OK : err`, with the produced-byte count
    // (`next_out - dest` == `out_pos`) reported to the caller on success.
    if code == ReturnCode::StreamEnd {
        Ok(out_pos)
    } else {
        Err(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::constants::Z_DEFAULT_COMPRESSION;

    // This module is layer 3, so these tests must not reach up to the real
    // layer-6 engine (AAP §0.3.1, §0.4.2 B2, enforced by
    // `the_module_graph_has_no_upward_edges` in `src/lib.rs`). The sizing formula
    // needs no engine at all, and the driver is exercised against a scripted
    // stand-in below, which pins the chunking and accounting behaviour far more
    // precisely than a round trip could. The real-engine round trips live with
    // the public entry points in `crate::deflate`.

    // ---------------------------------------------------------------------
    // compress_bound / compressBound  (Phase 1 — bit-exact sizing contract)
    // ---------------------------------------------------------------------

    #[test]
    fn compress_bound_zero_is_thirteen() {
        // Empty input still needs the fixed 13-byte overhead reserve.
        assert_eq!(compress_bound(0), 13);
    }

    #[test]
    fn compress_bound_mid_size_is_exact() {
        // 100000 + (100000>>12 = 24) + (100000>>14 = 6) + (100000>>25 = 0) + 13.
        assert_eq!(compress_bound(100_000), 100_043);
    }

    #[test]
    fn compress_bound_saturates_on_overflow() {
        // C returns (z_size_t)-1 when the sum wraps; the Rust port saturates.
        assert_eq!(compress_bound(usize::MAX), usize::MAX);
        // The neighborhood just below MAX also saturates (the +13 overflows).
        assert_eq!(compress_bound(usize::MAX - 5), usize::MAX);
    }

    #[test]
    fn compress_bound_is_monotonic_and_covers_source() {
        let mut prev = 0usize;
        for &n in &[0usize, 1, 13, 1024, 65_536, 100_000, 1_000_000, 16_000_000] {
            let bound = compress_bound(n);
            // Never smaller than the source plus the 13-byte reserve.
            assert!(bound >= n + 13, "bound {bound} must be >= {n} + 13");
            // Non-decreasing in the source length.
            assert!(bound >= prev, "bound {bound} must be >= previous {prev}");
            prev = bound;
        }
    }

    #[test]
    fn compress_bound_camelcase_alias_is_identical() {
        for &n in &[0usize, 1, 1000, 100_000, 1_000_000, usize::MAX] {
            assert_eq!(compressBound(n), compress_bound(n));
        }
    }

    // ---------------------------------------------------------------------
    // compress2_tracked_with  (the `compress2_z` driver, C `compress.c` L24-L66)
    //
    // A scripted stand-in engine lets every observable of the driver be asserted
    // exactly: how many bytes it offers per step, which flush it picks, when it
    // stops, and what it publishes through `produced` on each exit path.
    // ---------------------------------------------------------------------

    /// One recorded invocation of [`OneCallDeflate::step`].
    #[derive(Debug, PartialEq, Eq)]
    struct Offered {
        /// Length of the input window the driver offered.
        input: usize,
        /// Length of the output window the driver offered.
        output: usize,
        /// The flush the driver selected for this step.
        flush: FlushMode,
    }

    /// A stand-in engine that replays a scripted sequence of outcomes and records
    /// exactly what the driver offered it.
    struct ScriptedEngine {
        /// Remaining `(consumed, produced, code)` triples, front first.
        script: alloc::vec::Vec<(usize, usize, ReturnCode)>,
        /// What the driver offered on each step, in order.
        offered: alloc::vec::Vec<Offered>,
        /// Number of times [`OneCallDeflate::end`] was called.
        ended: usize,
    }

    // The script the next `ScriptedEngine::begin` should replay, the offers it
    // recorded, and how many times `end` ran. Thread-locals keep the fixture out
    // of the trait signature, which must stay identical to the three C calls it
    // models, and keep the tests independent when the harness runs them in
    // parallel.
    std::thread_local! {
        static SCRIPT: core::cell::RefCell<Option<alloc::vec::Vec<(usize, usize, ReturnCode)>>> =
            const { core::cell::RefCell::new(None) };
        static RECORD: core::cell::RefCell<alloc::vec::Vec<Offered>> =
            const { core::cell::RefCell::new(alloc::vec::Vec::new()) };
        static ENDED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    impl OneCallDeflate for ScriptedEngine {
        fn begin(level: i32) -> Result<Self, ReturnCode> {
            // Mirror the real engine's level validation so the driver's
            // "return before publishing anything" path is exercised for real.
            if level != Z_DEFAULT_COMPRESSION && !(0..=9).contains(&level) {
                return Err(ReturnCode::StreamError);
            }
            let script = SCRIPT
                .with(|s| s.borrow_mut().take())
                .expect("a script must be installed before driving the engine");
            RECORD.with(|r| r.borrow_mut().clear());
            ENDED.with(|e| e.set(0));
            Ok(Self {
                script,
                offered: alloc::vec::Vec::new(),
                ended: 0,
            })
        }

        fn step(&mut self, input: &[u8], output: &mut [u8], flush: FlushMode) -> OneCallStep {
            self.offered.push(Offered {
                input: input.len(),
                output: output.len(),
                flush,
            });
            let (consumed, produced, code) = self
                .script
                .first()
                .copied()
                .expect("the driver ran more steps than the script provides");
            self.script.remove(0);
            // Write recognisable bytes so the produced count is observable in the
            // destination buffer as well as in the reported length.
            for slot in output.iter_mut().take(produced) {
                *slot = 0xA5;
            }
            OneCallStep {
                consumed,
                produced,
                code,
            }
        }

        fn end(&mut self) {
            self.ended += 1;
            ENDED.with(|e| e.set(e.get() + 1));
            RECORD.with(|r| {
                r.borrow_mut()
                    .extend(self.offered.drain(..).collect::<alloc::vec::Vec<_>>())
            });
        }
    }

    /// Installs `script` and runs the driver over `source`/`dest` at `level`.
    fn drive(
        script: &[(usize, usize, ReturnCode)],
        dest: &mut [u8],
        source: &[u8],
        level: i32,
    ) -> (
        Result<usize, ReturnCode>,
        usize,
        alloc::vec::Vec<Offered>,
        usize,
    ) {
        SCRIPT.with(|s| *s.borrow_mut() = Some(script.to_vec()));
        RECORD.with(|r| r.borrow_mut().clear());
        ENDED.with(|e| e.set(0));
        let mut produced = usize::MAX;
        let result = compress2_tracked_with::<ScriptedEngine>(dest, source, level, &mut produced);
        let offered = RECORD.with(|r| core::mem::take(&mut *r.borrow_mut()));
        let ended = ENDED.with(core::cell::Cell::get);
        (result, produced, offered, ended)
    }

    #[test]
    fn driver_offers_whole_buffers_and_finishes_in_one_step() {
        // A single step that consumes everything and reports `Z_STREAM_END` is the
        // ordinary case: the driver must offer the whole input and the whole
        // output at once, pick `Z_FINISH` (all input already in the window), and
        // report the produced length.
        let source = [1u8; 40];
        let mut dest = [0u8; 90];
        let (result, produced, offered, ended) =
            drive(&[(40, 17, ReturnCode::StreamEnd)], &mut dest, &source, 6);

        assert_eq!(result, Ok(17));
        assert_eq!(produced, 17);
        assert_eq!(ended, 1, "`deflateEnd` runs exactly once");
        assert_eq!(
            offered,
            alloc::vec![Offered {
                input: 40,
                output: 90,
                flush: FlushMode::Finish
            }]
        );
        assert!(dest[..17].iter().all(|&b| b == 0xA5));
        assert!(dest[17..].iter().all(|&b| b == 0));
    }

    #[test]
    fn driver_refills_only_exhausted_windows_and_shrinks_the_offer() {
        // Two steps: the first consumes half the input and produces some output,
        // so the second must be offered exactly the REMAINDER of both windows —
        // C's `avail_in`/`avail_out` are decremented, not re-seeded.
        let source = [2u8; 30];
        let mut dest = [0u8; 50];
        let (result, produced, offered, _) = drive(
            &[(10, 4, ReturnCode::Ok), (20, 6, ReturnCode::StreamEnd)],
            &mut dest,
            &source,
            1,
        );

        assert_eq!(result, Ok(10));
        assert_eq!(produced, 10);
        assert_eq!(
            offered,
            alloc::vec![
                Offered {
                    input: 30,
                    output: 50,
                    flush: FlushMode::Finish
                },
                Offered {
                    input: 20,
                    output: 46,
                    flush: FlushMode::Finish
                }
            ]
        );
    }

    #[test]
    fn driver_publishes_the_partial_length_on_a_failing_path() {
        // C assigns `*destLen` after the loop and BEFORE `deflateEnd`, on every
        // post-init path (`compress.c` L63). A `Z_BUF_ERROR` must therefore still
        // report the bytes that did fit.
        let source = [3u8; 12];
        let mut dest = [0u8; 5];
        let (result, produced, _, ended) =
            drive(&[(12, 5, ReturnCode::BufError)], &mut dest, &source, 9);

        assert_eq!(result, Err(ReturnCode::BufError));
        assert_eq!(
            produced, 5,
            "the partial length is published, not discarded"
        );
        assert_eq!(ended, 1, "`deflateEnd` still runs on the failure path");
    }

    #[test]
    fn driver_seeds_the_length_to_zero_before_init() {
        // C `*destLen = 0;` precedes `deflateInit`, so a rejected level leaves the
        // reported length at zero rather than at whatever the caller passed in.
        let source = [4u8; 8];
        let mut dest = [0u8; 32];
        let mut produced = 777;
        assert_eq!(
            compress2_tracked_with::<ScriptedEngine>(&mut dest, &source, 42, &mut produced),
            Err(ReturnCode::StreamError)
        );
        assert_eq!(produced, 0);
    }

    #[test]
    fn driver_finishes_immediately_for_an_empty_source() {
        // An empty input still yields a complete stream: the driver offers a
        // zero-length input window and selects `Z_FINISH` on the very first step.
        let mut dest = [0u8; 16];
        let (result, produced, offered, _) =
            drive(&[(0, 8, ReturnCode::StreamEnd)], &mut dest, &[], 6);

        assert_eq!(result, Ok(8));
        assert_eq!(produced, 8);
        assert_eq!(
            offered,
            alloc::vec![Offered {
                input: 0,
                output: 16,
                flush: FlushMode::Finish
            }]
        );
    }
}
