//! `deflate_rle` — run-length-only compression (the `Z_RLE` strategy).
//!
//! Faithful, **100% safe Rust** port of the C `deflate_rle` block producer
//! (`deflate.c` L2084-L2149 of **zlib 1.3.2.1-motley**). It is selected by
//! [`Strategy::Rle`](crate::constants::Strategy::Rle) (`Z_RLE`) and limits
//! match distances to *exactly one*: it finds runs of the immediately
//! preceding byte without ever consulting the hash table or the general
//! longest-match search. Every recognised run is emitted as a
//! distance-1 length/literal pair, so the produced DEFLATE token stream — and
//! therefore the compressed bytes — is **byte-identical to reference zlib**.
//!
//! # Why RLE needs no hash table
//!
//! Because the only candidate match is the byte at distance one, `deflate_rle`
//! never calls [`DeflateState::longest_match`](crate::deflate::state::DeflateState::longest_match)
//! and never touches the `head`/`prev` hash chains. It only needs a previous
//! byte (`strstart > 0`) and at least [`MIN_MATCH`] bytes of lookahead; the
//! window must hold [`MAX_MATCH`] additional bytes (or be at end-of-input),
//! which is why the loop refills whenever `lookahead <= MAX_MATCH`.
//!
//! # Safety
//!
//! There is **zero `unsafe`** in this module, satisfying the project constraint
//! "zero unsafe blocks in core compression logic". Every window access is a
//! checked slice index; the C pointer arithmetic (`scan = window + strstart -
//! 1`, `*++scan`, `strend - scan`) is reproduced with plain `usize` indices.
//!
//! # `no_std`
//!
//! The module is `no_std` + `alloc`: it performs no allocation of its own and
//! uses only `core` facilities plus the crate's own deflate types. All working
//! buffers live in [`DeflateState`].
//!
//! # C cross-reference
//!
//! Function and variable names track the C originals (converted to
//! `snake_case`) so the port can be audited against `deflate.c` line-by-line.
//! The two `FLUSH_BLOCK` macros (`deflate.c` L1637-L1649) are
//! reproduced by the private `flush_block` helper, and the run-scan bound
//! (`deflate.c` L2103-L2120) is reproduced by `rle_match_length` with its
//! off-by-one carefully preserved (see that function's documentation).

use crate::constants::{Z_FINISH, Z_NO_FLUSH};
use crate::deflate::state::{DeflateState, IoContext, MAX_MATCH, MIN_MATCH};
use crate::deflate::strategy::BlockState;
use crate::deflate::trees;

/// Flush the current block, mirroring the C `FLUSH_BLOCK`/`FLUSH_BLOCK_ONLY`
/// macros (`deflate.c` L1637-L1649).
///
/// This performs the `FLUSH_BLOCK_ONLY` work — hand the current block to
/// [`trees::_tr_flush_block`], advance `block_start`, and drain the pending
/// buffer to the output via [`DeflateState::flush_pending`] — and then applies
/// the `FLUSH_BLOCK` early-exit test: if the output buffer is now full, the
/// producer must return to its caller.
///
/// # Return value
///
/// * `None` — output space remains; the caller may continue its main loop.
/// * `Some(state)` — the output buffer filled (`avail_out == 0`). `state` is
///   [`BlockState::FinishStarted`] when `last` is set (the final block has been
///   started but still needs draining) and [`BlockState::NeedMore`] otherwise.
///
/// `buf` passed to [`trees::_tr_flush_block`] is the block's start offset in the
/// window, or `None` when `block_start` has gone negative (the window slid past
/// the block start), reproducing the C `s->block_start >= 0L ? &window[...] :
/// Z_NULL` selection. `stored_len` is `strstart - block_start`, the number of
/// input bytes represented by the block.
fn flush_block(s: &mut DeflateState, io: &mut IoContext, last: bool) -> Option<BlockState> {
    // --- FLUSH_BLOCK_ONLY (deflate.c L1637-L1644) ---------------------------
    // block_start is signed: after a window slide it can be negative, in which
    // case no stored (uncompressed) representation of the block is available.
    let buf = if s.block_start >= 0 {
        Some(s.block_start as usize)
    } else {
        None
    };
    // (long)strstart - block_start, cast to an unsigned length. `strstart` is
    // always >= `block_start` (block_start is set to strstart at every flush
    // and both slide together in fill_window), so this never goes negative.
    let stored_len = (s.strstart as isize - s.block_start) as usize;

    trees::_tr_flush_block(s, buf, stored_len, last);
    s.block_start = s.strstart as isize;
    s.flush_pending(io);

    // --- FLUSH_BLOCK early exit (deflate.c L1647-L1648) ---------------------
    if io.avail_out == 0 {
        Some(if last {
            BlockState::FinishStarted
        } else {
            BlockState::NeedMore
        })
    } else {
        None
    }
}

/// Computes the run-length (distance-1) match length for the string at
/// `strstart`, reproducing the C run scan in `deflate_rle`
/// (`deflate.c` L2103-L2120) exactly.
///
/// A run is recognised only when there is a preceding byte (`strstart > 0`), at
/// least [`MIN_MATCH`] bytes of `lookahead`, and the three bytes at
/// `strstart..strstart + 3` all equal the previous byte `window[strstart - 1]`.
/// When recognised, the run is extended up to [`MAX_MATCH`] bytes and then
/// clamped to `lookahead`; otherwise `0` is returned, and the caller emits a
/// literal.
///
/// # The off-by-one that must be preserved
///
/// The C code sets its scan pointer to `window + strstart - 1`, reads `prev`
/// there, then evaluates `prev == *++scan` three times — which inspects
/// `window[strstart]`, `window[strstart + 1]`, and `window[strstart + 2]`,
/// leaving `scan` at index `strstart + 2`. The run-extension `do { } while
/// (prev == *++scan ... 8x ... && scan < strend)` *pre-increments before every
/// comparison*, so the first byte actually tested for the run is
/// `window[strstart + 3]`, and the `scan < strend` bound is checked only once
/// per eight comparisons. Because `MAX_MATCH - 2 == 256` is an exact multiple
/// of eight, `scan` lands *precisely* on `strend` for a maximal run and
/// otherwise stops on the first byte that differs from `prev`. The final
/// length is `MAX_MATCH - (strend - scan)`.
///
/// This port therefore starts its scan at `strstart + 3` and advances with the
/// pre-check `while scan < strend && window[scan] == prev`, which yields the
/// **identical** terminal `scan` (and never reads `window[strend]`), hence the
/// identical `match_length`. A naive `while scan + 1 < strend && window[scan +
/// 1] == prev` starting at `strstart + 2` would be off by one and would report
/// `2` for a 3-byte run — misclassifying a genuine match as literals and
/// diverging from zlib — so that form is deliberately avoided.
///
/// # Panics
///
/// Does not panic under the engine's invariants: `deflate_rle` only calls this
/// after [`DeflateState::fill_window`], which zero-fills the window for at
/// least `strstart + MAX_MATCH` bytes, so every index below is in bounds. All
/// accesses are bounds-checked regardless, so a violated invariant would panic
/// safely rather than invoke undefined behaviour.
fn rle_match_length(window: &[u8], strstart: usize, lookahead: usize) -> usize {
    // Gate: a run requires a previous byte and MIN_MATCH lookahead
    // (deflate.c L2104).
    if lookahead < MIN_MATCH || strstart == 0 {
        return 0;
    }

    // The byte at distance one that the run would repeat (deflate.c L2105-L2106).
    let prev = window[strstart - 1];

    // Require the first three bytes to repeat `prev` before scanning any
    // further (deflate.c L2107). Rust `&&` short-circuits exactly like C `&&`.
    if prev != window[strstart] || prev != window[strstart + 1] || prev != window[strstart + 2] {
        return 0;
    }

    // Extend the run. `strend` is the exclusive upper bound (`window + strstart
    // + MAX_MATCH` in C); `scan` starts at the first byte past the three-byte
    // gate. See the "off-by-one" section above for why this matches C exactly.
    let strend = strstart + MAX_MATCH;
    let mut scan = strstart + 3;
    while scan < strend && window[scan] == prev {
        scan += 1;
    }

    // deflate.c L2115: match_length = MAX_MATCH - (strend - scan). `scan` never
    // exceeds `strend`, so the inner subtraction cannot underflow.
    let mut len = MAX_MATCH - (strend - scan);

    // deflate.c L2116-L2117: never claim more than the available lookahead.
    if len > lookahead {
        len = lookahead;
    }
    len
}

/// Compress the current input using run-length encoding only (`Z_RLE`).
///
/// Direct port of C `deflate_rle` (`deflate.c` L2084-L2149). Distances are
/// restricted to one, so this recognises runs of the previous byte and emits
/// them as distance-1 matches (and non-repeating bytes as literals). The output
/// is byte-identical to reference zlib for the same input.
///
/// # Parameters
///
/// * `s` — the mutable [`DeflateState`]; its window, symbol buffer, and
///   bit-output state are advanced in place.
/// * `io` — the [`IoContext`] describing the caller's input/output buffers;
///   input is pulled into the window via [`DeflateState::fill_window`] and
///   compressed output is drained via [`DeflateState::flush_pending`].
/// * `flush` — the active flush mode (a `Z_*` code such as
///   [`Z_NO_FLUSH`] or
///   [`Z_FINISH`]); it controls whether the routine
///   may pause for more input and whether the final block is emitted.
///
/// # Returns
///
/// A [`BlockState`] reporting how far the block advanced:
/// [`NeedMore`](BlockState::NeedMore) when more input/output is required,
/// [`BlockDone`](BlockState::BlockDone) when a non-final block was completed,
/// and [`FinishStarted`](BlockState::FinishStarted) /
/// [`FinishDone`](BlockState::FinishDone) for the `Z_FINISH` termination cases.
pub fn deflate_rle(s: &mut DeflateState, io: &mut IoContext, flush: i32) -> BlockState {
    loop {
        // Make sure we always have enough lookahead, except at end of input:
        // MAX_MATCH bytes for the longest run, plus one for the C unrolled loop
        // (deflate.c L2091-L2100).
        if s.lookahead <= MAX_MATCH {
            s.fill_window(io);
            if s.lookahead <= MAX_MATCH && flush == Z_NO_FLUSH {
                return BlockState::NeedMore;
            }
            if s.lookahead == 0 {
                break; // flush the current block
            }
        }

        // See how many times the previous byte repeats (deflate.c L2102-L2121).
        // `rle_match_length` returns 0 when no run of at least MIN_MATCH bytes
        // begins at `strstart`, matching the C `s->match_length = 0` default.
        s.match_length = rle_match_length(&s.window, s.strstart, s.lookahead);

        // Emit a match for a run of MIN_MATCH or longer, else a literal
        // (deflate.c L2123-L2138). `bflush` is set when the symbol buffer
        // filled and the block must be flushed.
        let bflush;
        if s.match_length >= MIN_MATCH {
            // For RLE the distance is always 1; the symbol buffer carries the
            // length code as `match_length - MIN_MATCH` (always in 0..=255).
            let len_code = (s.match_length - MIN_MATCH) as u8;
            bflush = trees::_tr_tally_dist(s, 1, len_code);

            s.lookahead -= s.match_length;
            s.strstart += s.match_length;
            s.match_length = 0;
        } else {
            // No run: output a single literal byte.
            let literal = s.window[s.strstart];
            bflush = trees::_tr_tally_lit(s, literal);
            s.lookahead -= 1;
            s.strstart += 1;
        }

        // Flush the block when the symbol buffer filled. The flush is invoked
        // only when `bflush` is set (preserving the C short-circuit), and the
        // outcome is inspected with a single `if let` — this deliberately
        // avoids an `if cond && let ...` chain, which would raise the crate's
        // minimum supported Rust version above the required 1.85.0.
        let flushed = if bflush {
            flush_block(s, io, false)
        } else {
            None
        };
        if let Some(state) = flushed {
            return state;
        }
    }

    // End of input: nothing remains to be inserted into the (unused) hash
    // (deflate.c L2140).
    s.insert = 0;

    if flush == Z_FINISH {
        // Emit the final block (deflate.c L2141-L2144). If the output buffer
        // filled, finishing has only *started*; otherwise the stream is done.
        if flush_block(s, io, true).is_some() {
            return BlockState::FinishStarted;
        }
        return BlockState::FinishDone;
    }

    // Flush a trailing partial block if any symbols are still buffered
    // (deflate.c L2146-L2147). Structured as a single `if let` (see the
    // matching note in the main loop) to stay within MSRV 1.85.0.
    let flushed = if s.sym_next != 0 {
        flush_block(s, io, false)
    } else {
        None
    };
    if let Some(state) = flushed {
        return state;
    }

    BlockState::BlockDone
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::Strategy;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Builds a test window in which the byte before `strstart` is `prev`, the
    /// `run` bytes at `strstart..strstart + run` are all `prev`, and every other
    /// byte is `filler`. The window is oversized to `strstart + MAX_MATCH + 8`
    /// so all scans stay in bounds (mirroring `fill_window`'s zero-fill).
    /// `filler` must differ from `prev` to terminate the run.
    fn window_with_run(strstart: usize, prev: u8, run: usize, filler: u8) -> Vec<u8> {
        assert_ne!(prev, filler, "filler must differ from prev to end the run");
        let size = strstart + MAX_MATCH + 8;
        let mut w = vec![filler; size];
        w[strstart - 1] = prev;
        for byte in w.iter_mut().skip(strstart).take(run) {
            *byte = prev;
        }
        w
    }

    #[test]
    fn no_run_when_strstart_is_zero() {
        // No previous byte exists at strstart == 0, so no run can start.
        let w = vec![0x42u8; 300];
        assert_eq!(rle_match_length(&w, 0, 300), 0);
    }

    #[test]
    fn no_run_when_lookahead_below_min_match() {
        let w = window_with_run(10, 0xAA, 5, 0x55);
        assert_eq!(rle_match_length(&w, 10, MIN_MATCH - 1), 0);
    }

    #[test]
    fn no_run_when_first_byte_differs() {
        // prev = 0xAA but window[strstart] = 0x55 → the three-byte gate fails.
        let mut w = vec![0x55u8; 300];
        w[9] = 0xAA; // prev; w[10] stays 0x55 != prev
        assert_eq!(rle_match_length(&w, 10, 200), 0);
    }

    #[test]
    fn run_of_exactly_three_is_a_match_not_literals() {
        // The off-by-one guard: a 3-byte run MUST yield match_length == 3
        // (>= MIN_MATCH), never 2 (which would wrongly emit literals and
        // diverge from zlib).
        let w = window_with_run(10, 0xAA, 3, 0x55);
        assert_eq!(rle_match_length(&w, 10, 200), 3);
    }

    #[test]
    fn run_of_four() {
        let w = window_with_run(10, 0xAA, 4, 0x55);
        assert_eq!(rle_match_length(&w, 10, 200), 4);
    }

    #[test]
    fn run_of_five() {
        let w = window_with_run(10, 0xAA, 5, 0x55);
        assert_eq!(rle_match_length(&w, 10, 200), 5);
    }

    #[test]
    fn run_of_one_hundred() {
        let w = window_with_run(10, 0xAA, 100, 0x55);
        assert_eq!(rle_match_length(&w, 10, 200), 100);
    }

    #[test]
    fn run_of_two_hundred_fifty_seven() {
        let w = window_with_run(10, 0xAA, 257, 0x55);
        assert_eq!(rle_match_length(&w, 10, 300), 257);
    }

    #[test]
    fn run_of_two_hundred_fifty_eight_hits_max_match() {
        let w = window_with_run(10, 0xAA, 258, 0x55);
        assert_eq!(rle_match_length(&w, 10, 300), MAX_MATCH);
    }

    #[test]
    fn run_longer_than_max_match_is_capped_to_max_match() {
        // 300 identical bytes with ample lookahead → capped at MAX_MATCH (258).
        let w = window_with_run(10, 0xAA, 300, 0x55);
        assert_eq!(rle_match_length(&w, 10, 300), MAX_MATCH);
    }

    #[test]
    fn match_length_capped_by_lookahead() {
        // A 50-byte run but only 20 bytes of lookahead → clamp to 20.
        let w = window_with_run(10, 0xAA, 50, 0x55);
        assert_eq!(rle_match_length(&w, 10, 20), 20);
    }

    #[test]
    fn zero_prev_with_zero_fill_is_capped_by_lookahead() {
        // prev == 0 with an all-zero window (mirrors fill_window's WIN_INIT
        // zero-fill). The run would reach MAX_MATCH, but lookahead clamps it —
        // exactly as the C code caps `match_length` to `lookahead`.
        let w = vec![0u8; 10 + MAX_MATCH + 8];
        assert_eq!(rle_match_length(&w, 10, 3), 3);
        assert_eq!(rle_match_length(&w, 10, 7), 7);
    }

    #[test]
    fn deflate_rle_compresses_long_run_end_to_end() {
        // Raw DEFLATE stream (wrap = 0), RLE strategy, default level 6.
        let mut s =
            DeflateState::new(6, 8, 15, 8, Strategy::Rle, 0).expect("valid deflate parameters");
        // Initialise the tree/block bookkeeping (C `_tr_init`, normally invoked
        // by the mod.rs driver after a reset).
        trees::_tr_init(&mut s);

        let input = vec![b'A'; 2000];
        let mut output = vec![0u8; 4096];
        let mut io = IoContext::new(&input, &mut output);

        let state = deflate_rle(&mut s, &mut io, Z_FINISH);

        assert_eq!(state, BlockState::FinishDone);
        // A 2000-byte single-byte run must compress dramatically.
        assert!(io.next_out > 0, "expected some compressed output");
        assert!(
            io.next_out < input.len() / 4,
            "expected strong compression, got {} bytes for {} input bytes",
            io.next_out,
            input.len()
        );
        // All input was consumed and accounted for.
        assert_eq!(io.avail_in, 0);
        assert_eq!(io.total_in as usize, input.len());
    }

    #[test]
    fn deflate_rle_handles_mixed_and_nonrepeating_data() {
        // Runs interspersed with distinct bytes exercise both the match and
        // literal emission paths within a single block.
        let mut input = Vec::new();
        input.extend_from_slice(&[b'x'; 300]);
        input.extend_from_slice(b"abcdefg");
        input.extend_from_slice(&[b'y'; 5]);
        input.extend_from_slice(b"z");

        let mut s = DeflateState::new(6, 8, 15, 8, Strategy::Rle, 0).unwrap();
        trees::_tr_init(&mut s);
        let mut output = vec![0u8; 8192];
        let mut io = IoContext::new(&input, &mut output);

        let state = deflate_rle(&mut s, &mut io, Z_FINISH);

        assert_eq!(state, BlockState::FinishDone);
        assert!(io.next_out > 0);
        assert_eq!(io.avail_in, 0);
        assert_eq!(io.total_in as usize, input.len());
    }

    #[test]
    fn deflate_rle_empty_input_finishes_cleanly() {
        // No input at all: the routine must still terminate the stream.
        let mut s = DeflateState::new(6, 8, 15, 8, Strategy::Rle, 0).unwrap();
        trees::_tr_init(&mut s);
        let input: [u8; 0] = [];
        let mut output = vec![0u8; 64];
        let mut io = IoContext::new(&input, &mut output);

        let state = deflate_rle(&mut s, &mut io, Z_FINISH);

        assert_eq!(state, BlockState::FinishDone);
        // An empty final block is still emitted (BFINAL + alignment).
        assert!(io.next_out > 0);
    }

    #[test]
    fn run_of_exactly_two_is_not_a_match() {
        // The three-byte gate (`deflate.c` L2107) seen from the other side: a
        // two-byte run must score 0, not 2, so the caller emits literals. This
        // is the complement of `run_of_exactly_three_is_a_match_not_literals`
        // and together they pin the gate from both directions.
        let w = window_with_run(10, 0xAA, 2, 0x55);
        assert_eq!(rle_match_length(&w, 10, 200), 0);
    }

    #[test]
    fn every_run_length_scans_exactly_and_never_off_by_one() {
        // Exhaustive regression pin for the off-by-one documented on
        // `rle_match_length`. The C scan checks its `scan < strend` bound only
        // once per eight comparisons, so every one of the 32 eight-byte group
        // boundaries between `strstart + 2` and `strend` is a place a mistaken
        // bound could hide. Sweeping *every* run length from 0 through 400
        // crosses all of them, including the exact-`strend` landing at 258.
        //
        // The expectation is derived independently of the implementation's
        // `MAX_MATCH - (strend - scan)` arithmetic: a run shorter than
        // MIN_MATCH scores 0, and any other run scores itself capped at
        // MAX_MATCH. A single misplaced index would break at least one length.
        for run in 0..=400usize {
            let w = window_with_run(10, 0xAA, run, 0x55);
            let expected = if run < MIN_MATCH {
                0
            } else if run > MAX_MATCH {
                MAX_MATCH
            } else {
                run
            };
            assert_eq!(
                rle_match_length(&w, 10, 512),
                expected,
                "run of {run} bytes scanned incorrectly"
            );
        }
    }

    #[test]
    fn lookahead_clamp_holds_for_every_lookahead() {
        // Companion sweep for the `match_length > lookahead` clamp
        // (`deflate.c` L2116-L2117), which is applied *after* the length
        // formula. With a 300-byte run present, the unclamped length is always
        // MAX_MATCH, so the result must be `min(MAX_MATCH, lookahead)` for
        // every lookahead at or above MIN_MATCH, and 0 below it.
        let w = window_with_run(10, 0xAA, 300, 0x55);
        for lookahead in 0..=300usize {
            let expected = if lookahead < MIN_MATCH {
                0
            } else {
                lookahead.min(MAX_MATCH)
            };
            assert_eq!(
                rle_match_length(&w, 10, lookahead),
                expected,
                "clamp wrong for lookahead {lookahead}"
            );
        }
    }

    #[test]
    fn length_code_of_a_maximal_run_is_exactly_u8_max() {
        // `deflate_rle` narrows the length to `u8` as `match_length -
        // MIN_MATCH`. Because the emit branch requires `match_length >=
        // MIN_MATCH` and the scan caps at MAX_MATCH, the code spans 0..=255
        // exactly — `258 - 3 == 255` leaves zero slack above `u8::MAX`, so the
        // cast is lossless by construction. This pins both ends of that range.
        let maximal = window_with_run(10, 0xAA, MAX_MATCH, 0x55);
        assert_eq!(rle_match_length(&maximal, 10, 512), MAX_MATCH);
        assert_eq!((MAX_MATCH - MIN_MATCH) as u8, u8::MAX);

        let minimal = window_with_run(10, 0xAA, MIN_MATCH, 0x55);
        assert_eq!(rle_match_length(&minimal, 10, 512), MIN_MATCH);
        assert_eq!((MIN_MATCH - MIN_MATCH) as u8, 0);
    }

    #[test]
    fn deflate_rle_emits_only_distance_one_matches() {
        // The defining property of Z_RLE, asserted directly on the symbol
        // buffer rather than inferred from the compressed size: every match
        // `deflate_rle` records must carry distance exactly 1.
        //
        // `_tr_tally_dist`/`_tr_tally_lit` write three bytes per symbol —
        // distance low, distance high, then the length code (matches) or the
        // literal byte (literals) — so a triple with a zero distance is a
        // literal and any non-zero distance is a match. Flushing is avoided by
        // driving the producer with Z_NO_FLUSH, which returns `NeedMore` once
        // the input is exhausted and therefore leaves `sym_next` intact.
        let mut s = DeflateState::new(6, 8, 15, 8, Strategy::Rle, 0).unwrap();
        trees::_tr_init(&mut s);

        let input = vec![b'Q'; 2000];
        let mut output = vec![0u8; 8192];
        let mut io = IoContext::new(&input, &mut output);

        let state = deflate_rle(&mut s, &mut io, Z_NO_FLUSH);

        // Input exhausted with the symbol buffer far from full, so no flush ran
        // and the tallied symbols are still available for inspection.
        assert_eq!(state, BlockState::NeedMore);
        assert!(s.sym_next > 0, "expected tallied symbols");
        assert_eq!(s.sym_next % 3, 0, "symbol buffer holds 3-byte triples");

        let mut matches = 0usize;
        for triple in (0..s.sym_next).step_by(3) {
            let dist = u16::from(s.sym(triple)) | (u16::from(s.sym(triple + 1)) << 8);
            if dist != 0 {
                assert_eq!(dist, 1, "Z_RLE emitted a match at distance {dist}");
                // The length code is `match_length - MIN_MATCH`; a run this
                // long always saturates the scan at MAX_MATCH.
                assert_eq!(s.sym(triple + 2), (MAX_MATCH - MIN_MATCH) as u8);
                matches += 1;
            }
        }
        assert!(matches > 0, "expected at least one distance-1 match");
    }
}
