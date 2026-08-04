//! The **write** side of the gzip file-I/O layer (`gz*` writing API) — a
//! faithful, memory-safe Rust port of C `gzwrite.c`.
//!
//! This module implements the stdio-like `gz*` writing entry points
//! (`gzwrite`, `gzfwrite`, `gzputc`, `gzputs`, `gzprintf`, `gzflush`) plus the
//! internal compress pipeline (`gz_init`, `gz_comp`, `gz_zero`, `gz_write`)
//! that drives compression through the crate's DEFLATE engine
//! ([`crate::deflate`]). It operates entirely on the shared
//! [`GzState`](crate::gz::state::GzState) defined by the gz-layer state module,
//! and produces output that is **byte-identical** to reference zlib for the
//! same input, level, and strategy, framed as a valid gzip member (RFC 1952).
//!
//! # Relationship to C `gzwrite.c`
//!
//! The port preserves the C structure function-for-function (C source line
//! ranges are cited in comments), but replaces the raw `z_stream` cursor
//! quadruple (`next_in`/`avail_in`/`next_out`/`avail_out`) and the manual
//! `malloc`/`free` bookkeeping with safe Rust:
//!
//! * The idiomatic [`crate::deflate::deflate`] takes the input and output as
//!   **slices** per call and returns a [`DeflateOutcome`](crate::deflate::DeflateOutcome)
//!   (`consumed`/`produced`/`code`) — there is no persistent `next_out` cursor
//!   inside [`ZStream`](crate::stream::ZStream). This module therefore owns the
//!   buffering: the compressed output is flushed to the file after each
//!   `deflate` call, which is byte-identical to the C behaviour of accumulating
//!   in `state->out` before writing (the DEFLATE byte stream is independent of
//!   how the output is chunked).
//! * The C write path never uses the `gzFile_s.have` window field, so this port
//!   repurposes [`GzState::have`](crate::gz::state::GzState) to hold the count of
//!   **buffered, not-yet-compressed input bytes** (the C `strm.avail_in`). The
//!   buffered input always begins at `in_buf[0]` (the C `next_in == in`
//!   invariant), so pending input is exactly `in_buf[0..have]`.
//! * `deflateEnd`/`free`/`close` are subsumed by RAII: dropping the
//!   [`GzState`](crate::gz::state::GzState) releases the buffers, the engine
//!   state, and the OS file handle.
//!
//! # Safety
//!
//! This module contains **zero `unsafe`**. All buffer access is bounds-checked
//! slice indexing; the raw C-string / raw-pointer handling for the FFI `gz*`
//! entry points lives in `src/ffi/gz.rs`, never here. The public
//! [`std::io::Write`] implementation on [`GzState`](crate::gz::state::GzState)
//! offers an idiomatic Rust API alongside the C-compatible functions.

// Compile-enforce the folder unsafe-policy: this module must contain zero
// `unsafe` (all raw-pointer / C-string handling for the FFI `gz*` entry points
// lives in `src/ffi/gz.rs`). Mirrors the sibling deflate modules.
#![deny(unsafe_code)]

use std::io::{self, Write};

use crate::constants::{DEF_MEM_LEVEL, FlushMode, MAX_WBITS, Strategy, Z_DEFLATED, Z_FINISH};
use crate::deflate;
use crate::error::{ReturnCode, ZlibError};
use crate::gz::alloc_zeroed;
use crate::gz::state::{GzFile, GzMode, GzState};

/// Returns `true` if `state` is a live write stream with no *serious* pending
/// error, i.e. it is ready to accept a new write request.
///
/// Mirrors the C guard used at the top of every public write entry point:
/// `state->mode != GZ_WRITE || (state->err != Z_OK && !state->again)`
/// (`gzwrite.c` L263-L264). A [`ReturnCode::BufError`]-style soft error is not
/// possible on the write path, so the check reduces to "mode is
/// [`GzMode::Write`] and either there is no error or a non-blocking retry is
/// pending".
#[inline]
fn write_ready(state: &GzState) -> bool {
    state.mode == GzMode::Write && (state.err == ReturnCode::Ok || state.again)
}

/// Initializes state for writing a gzip file — port of C `gz_init`
/// (`gzwrite.c` L11-L57).
///
/// Allocates the input buffer at **double** the requested size (the second
/// half gives [`gzvprintf`] room to format before compressing), and — unless
/// the stream is in transparent/`direct` mode — allocates the output buffer and
/// initializes the DEFLATE engine for **gzip** framing (`windowBits =
/// MAX_WBITS + 16`, C L35). Initialization is marked complete by setting
/// [`GzState::size`](crate::gz::state::GzState) to the (non-zero) buffer size,
/// exactly as the C sentinel does.
///
/// # Errors
///
/// Returns [`ZlibError::MemError`] if the engine initialization fails (the C
/// code reports every `gz_init` failure as `Z_MEM_ERROR` / "out of memory",
/// C L38-L42); the state's error is set via
/// [`GzState::error`](crate::gz::state::GzState::error) as a side effect. That
/// includes a recorded `level` or `strategy` outside the range `deflateInit2`
/// accepts (`deflate.c` L436): C forwards those values unvalidated and reports
/// the engine's rejection here, and so does this port — the deferred failure is
/// how a caller's out-of-range `gzsetparams` argument surfaces.
///
/// Every failure path releases whatever this call had already allocated, exactly
/// as C does (`free(state->in)` before the `malloc(state->out)` failure return,
/// `free(state->out); free(state->in);` before the `deflateInit2` failure return
/// — C L19-L44). The allocation *order*, *count*, and reported code are
/// unchanged, so a caller still observes the failure at the same point in the
/// stream's life; only the retained memory is given back. Without the rollback a
/// stream whose recorded `level`/`strategy` can never be accepted would hold
/// `3 * want` bytes — the doubled input buffer plus the output buffer — for the
/// rest of its life, on every rejected write, even though `size` correctly stays
/// `0` and marks the stream uninitialized.
pub(crate) fn gz_init(state: &mut GzState) -> Result<(), ZlibError> {
    // Allocate the input buffer, double-sized for `gzprintf` (C L14-L19).
    //
    // `crate::gz::alloc_zeroed` rather than `vec![0u8; …]` because this length is
    // caller-influenced and can legitimately be enormous: `gzbuffer` accepts any
    // `want` up to `UINT_MAX >> 1` (matching C, which bounds the request and
    // nothing more), so `want << 1` here can reach ~4 GiB from a single
    // `gzbuffer(file, 0x7fff_ffff)` call. C answers an unsatisfiable request with
    // `Z_MEM_ERROR` / "out of memory" and leaves the stream usable; `vec!` would
    // abort the process instead, which a C caller cannot intercept. Nothing has
    // been allocated yet at this point, so — exactly like C L16-L19, whose first
    // failure return is not preceded by any `free` — there is nothing to roll
    // back.
    let Some(in_buf) = alloc_zeroed(state.want << 1) else {
        state.error(ReturnCode::MemError, Some("out of memory"));
        return Err(ZlibError::MemError);
    };
    state.in_buf = in_buf;

    // Only need an output buffer and a deflate engine when compressing
    // (C L22-L44); a `direct` stream writes straight to the file.
    if state.direct == 0 {
        // Allocate the output buffer (C L23-L29). C frees the input buffer
        // before reporting this one's failure (`free(state->in);` at C L25), so
        // the rollback runs first here too — the allocation order, the count, and
        // the reported code all stay identical to C.
        let Some(out_buf) = alloc_zeroed(state.want) else {
            gz_init_rollback(state);
            state.error(ReturnCode::MemError, Some("out of memory"));
            return Err(ZlibError::MemError);
        };
        state.out_buf = out_buf;

        // Set up for gzip compression. `MAX_WBITS + 16` selects gzip
        // header/trailer framing inside the engine (C L34-L36). The gzip
        // CRC-32 and ISIZE trailer are produced internally by the engine, so
        // this layer never touches `crate::checksum` directly.
        //
        // `state.strategy` is the raw C `int` recorded by `gzopen`'s mode
        // characters or by `gzsetparams`, neither of which range-checks it
        // (C `gzsetparams`, `gzwrite.c` L630-L663, simply records the value and
        // returns `Z_OK`). C hands that raw value straight to `deflateInit2`,
        // which rejects `strategy < 0 || strategy > Z_FIXED` (`deflate.c` L436),
        // and `gz_init` then reports the rejection as `Z_MEM_ERROR` /
        // "out of memory" (C L37-L43). The typed `Strategy` cannot represent an
        // out-of-range value, so the failed conversion *is* that rejection and
        // must be propagated identically — substituting a default here would
        // silently compress with a strategy the caller never asked for, making
        // the caller's mistake invisible in the return code, `gzerror`, and
        // `gzclose_w` alike. An out-of-range `level` fails the same way, being
        // rejected by `deflate_init2` below, so both parameters of one
        // `gzsetparams` call are handled identically.
        let Some(strategy) = Strategy::from_c_int(state.strategy) else {
            // C L37-L43 frees both buffers before reporting; see `gz_init_rollback`.
            gz_init_rollback(state);
            state.error(ReturnCode::MemError, Some("out of memory"));
            return Err(ZlibError::MemError);
        };
        if deflate::deflate_init2(
            &mut state.strm,
            state.level,
            Z_DEFLATED,
            MAX_WBITS + 16,
            DEF_MEM_LEVEL,
            strategy,
        )
        .is_err()
        {
            // C L37-L43: any init failure is surfaced as an out-of-memory error,
            // after `free(state->out); free(state->in);`.
            gz_init_rollback(state);
            state.error(ReturnCode::MemError, Some("out of memory"));
            return Err(ZlibError::MemError);
        }
    }

    // The compressed-output window (C L49-L55, `strm->avail_out = state->size;
    // strm->next_out = state->out; state->x.next = strm->next_out;`) is modelled
    // as `out_buf[0..size]` — the scratch area handed to `deflate` on each call —
    // plus the persistent cursor pair
    // [`GzState::out_start`](crate::gz::state::GzState) /
    // [`GzState::out_pending`](crate::gz::state::GzState), which locate and count
    // the bytes `deflate` produced that the OS has not accepted yet. C's cursor is
    // a pointer into `state->out`; the safe-index counterpart starts empty and
    // front-anchored here, exactly as C's starts equal to `strm->next_out`.
    state.out_pending = 0;
    state.out_start = 0;

    // Mark the state as initialized (C L46). This must stay the *last* statement:
    // a non-zero `size` is the sentinel every caller tests to decide whether the
    // buffers and engine already exist, so setting it earlier would advertise a
    // half-built state if any step above failed.
    state.size = state.want;
    Ok(())
}

/// Releases everything [`gz_init`] had allocated before it hit a failure — the
/// `free(state->out); free(state->in);` that precedes every C `gz_init` error
/// return (`gzwrite.c` L19-L44).
///
/// Called *before* [`GzState::error`](crate::gz::state::GzState::error) so the
/// recorded code and message — and therefore everything a caller can observe
/// through `gzerror`, the entry point's return value, or `gzclose_w` — are
/// untouched by the rollback. [`GzState::size`](crate::gz::state::GzState) is
/// still `0` at every one of those failure points, so the stream correctly
/// remains "uninitialized" and a later call re-runs [`gz_init`] from scratch,
/// exactly as it would in C.
///
/// The deflate engine needs no rollback: `deflate_init2` installs the engine
/// state on the stream only after it has been built successfully, so a failed
/// init leaves [`GzState::strm`](crate::gz::state::GzState) with no state object
/// to release (the diagnostic message it records on the stream is C-parity —
/// `deflate.c` L505-L514 — and is deliberately preserved).
#[inline]
fn gz_init_rollback(state: &mut GzState) {
    state.out_buf = Vec::new();
    state.in_buf = Vec::new();
}

/// The maximum number of bytes written to the file in a single `write` call —
/// the C `max = ((unsigned)-1 >> 2) + 1` cap (`gzwrite.c` L67), preserved so a
/// pathologically large buffer cannot overflow the platform's write count.
const WRITE_MAX: usize = (u32::MAX >> 2) as usize + 1;

/// How much input a compress-and-write step ingested, together with the error
/// (if any) that stopped it.
///
/// # Why the count must survive the error
///
/// Reference zlib reports partial progress out of a failed `gz_comp` through the
/// stream itself: after the call, `strm->avail_in` still describes the input the
/// engine has *not* taken, so `gz_write` recovers the ingested count with
/// `n -= strm->avail_in` and only then decides what to return
/// (`gzwrite.c` L239-L244). A plain `Result<usize, ZlibError>` cannot express
/// that — the `Err` arm carries no count — so a stall would report fewer bytes
/// accepted than `deflate` actually consumed. A caller obeying the documented
/// retry contract (resubmit from the returned offset) would then hand the same
/// bytes to the engine twice and duplicate them in the compressed stream.
///
/// Splitting the two answers apart makes the mistake unrepresentable: every
/// caller must look at `consumed` before it looks at `result`.
#[must_use]
struct CompOutcome {
    /// Number of bytes of the supplied input the engine ingested. Valid whether
    /// or not `result` is an error.
    consumed: usize,
    /// `Ok(())` if the step ran to completion, otherwise the error that stopped
    /// it (already recorded on the state via
    /// [`GzState::error`](crate::gz::state::GzState::error)).
    result: Result<(), ZlibError>,
}

impl CompOutcome {
    /// A step that ingested `consumed` bytes and completed successfully.
    #[inline]
    fn ok(consumed: usize) -> Self {
        Self {
            consumed,
            result: Ok(()),
        }
    }

    /// A step that ingested `consumed` bytes before failing with `err`.
    #[inline]
    fn err(consumed: usize, err: ZlibError) -> Self {
        Self {
            consumed,
            result: Err(err),
        }
    }
}

/// Hands `buf` to the operating system once, reporting how many bytes it
/// accepted — the single `write(2)` call shared by [`drain_pending`] and
/// [`write_direct`].
///
/// Taking `&mut GzFile` rather than `&mut GzState` is deliberate: both callers
/// pass a slice borrowed from a *different* field of the same state (the pending
/// output window, or the caller's input), and only a disjoint field borrow lets
/// that compile without copying the bytes somewhere else first.
///
/// # Test-only fault injection
///
/// A short write and a non-blocking stall are the two operating-system behaviours
/// the pending-output window exists to survive, yet neither can be provoked from a
/// regular file — and this module is `#![deny(unsafe_code)]`, so `pipe(2)`,
/// `socketpair(2)`, `fcntl(O_NONBLOCK)`, and `SO_SNDBUF` are all out of reach.
/// Under `cfg(test)` this indirection therefore consults a thread-local script
/// (see [`fault`]) that can make one write accept only part of its buffer, accept
/// nothing at all (`Ok(0)`), or fail with [`io::ErrorKind::WouldBlock`] or
/// [`io::ErrorKind::Interrupted`]. Those are the four outcomes an ordinary file
/// never produces; together with the accept-everything an ordinary file always
/// produces they cover every outcome a `write(2)` can report, deterministically
/// and portably, with no `unsafe`, no extra dependency, and no platform-specific
/// code. A scripted partial acceptance really does write that prefix to the file,
/// so end-to-end assertions still decode genuine output.
///
/// Outside `cfg(test)` the entire mechanism is absent: the function below is the
/// only definition compiled into the library, and it is the bare `write` call.
#[cfg(not(test))]
#[inline]
fn write_some(file: &mut GzFile, buf: &[u8]) -> io::Result<usize> {
    file.write(buf)
}

/// The `cfg(test)` counterpart of [`write_some`](fn@write_some): consults the
/// thread-local fault script first, then falls back to a real `write`.
///
/// See the non-test definition for why this seam exists.
#[cfg(test)]
fn write_some(file: &mut GzFile, buf: &[u8]) -> io::Result<usize> {
    match fault::next_step() {
        // No script installed, or it is exhausted: behave exactly as the
        // production definition does.
        None => file.write(buf),
        Some(fault::Step::WouldBlock) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        Some(fault::Step::Interrupted) => Err(io::Error::from(io::ErrorKind::Interrupted)),
        Some(fault::Step::Refuse) => Ok(0),
        Some(fault::Step::Accept(n)) => {
            let n = core::cmp::min(n, buf.len());
            // Deliver the accepted prefix for real, so the file on disk is exactly
            // what a destination behaving this way would have received.
            file.write_all(&buf[..n])?;
            Ok(n)
        }
    }
}

/// Deterministic write-fault injection for this module's unit tests.
///
/// The script is thread-local, so tests running concurrently under the default
/// test harness cannot influence one another, and [`Guard`](fault::Guard) clears
/// it on scope exit — including while unwinding from a failed assertion — so a
/// scripted fault can never leak into an unrelated test.
#[cfg(test)]
mod fault {
    use std::cell::{Cell, RefCell};

    /// One scripted answer for the next write performed by
    /// [`write_some`](super::write_some).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Step {
        /// Accept only this many bytes (clamped to the buffer length) — a short
        /// write, as a stream socket or pipe with partial room performs.
        Accept(usize),
        /// Fail with [`io::ErrorKind::WouldBlock`](std::io::ErrorKind::WouldBlock)
        /// — the `EAGAIN`/`EWOULDBLOCK` stall of a non-blocking descriptor.
        WouldBlock,
        /// Fail with
        /// [`io::ErrorKind::Interrupted`](std::io::ErrorKind::Interrupted) before
        /// any byte moves — the `EINTR` a signal delivers mid-`write(2)`. The loop
        /// must retry it, and retrying must not resubmit an accepted byte.
        Interrupted,
        /// Accept nothing at all (`Ok(0)`) while bytes remain — the `writ == 0`
        /// that C's single `+= writ` success arm folds into a re-issue of the
        /// identical request. Both loops must retry it in place: the cursor may
        /// neither advance nor rewind, so no byte is dropped and none is
        /// resubmitted.
        Refuse,
    }

    thread_local! {
        /// One-shot steps not yet consumed on this thread, stored in reverse so
        /// taking the next one is a cheap `pop`.
        static SCRIPT: RefCell<Vec<Step>> = const { RefCell::new(Vec::new()) };
        /// A standing per-write byte cap applied once [`SCRIPT`] is exhausted.
        static CAP: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Installs `steps` (consumed front-to-back), replacing any previous setup.
    ///
    /// The returned guard must be held for as long as the script should apply.
    /// Once the steps run out, writes behave normally again.
    pub(super) fn script(steps: &[Step]) -> Guard {
        CAP.with(|c| c.set(None));
        SCRIPT.with(|s| {
            let mut s = s.borrow_mut();
            s.clear();
            s.extend(steps.iter().rev().copied());
        });
        Guard
    }

    /// Caps *every* write at `n` bytes for as long as the returned guard lives —
    /// a destination that dribbles, however many writes that takes.
    pub(super) fn cap_every_write(n: usize) -> Guard {
        SCRIPT.with(|s| s.borrow_mut().clear());
        CAP.with(|c| c.set(Some(n)));
        Guard
    }

    /// Takes the next scripted step, the standing cap, or [`None`] when neither is
    /// installed.
    pub(super) fn next_step() -> Option<Step> {
        if let Some(step) = SCRIPT.with(|s| s.borrow_mut().pop()) {
            return Some(step);
        }
        CAP.with(Cell::get).map(Step::Accept)
    }

    /// How many one-shot steps have not been consumed yet.
    pub(super) fn remaining() -> usize {
        SCRIPT.with(|s| s.borrow().len())
    }

    /// Clears the installed script and cap when it goes out of scope.
    pub(super) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            SCRIPT.with(|s| s.borrow_mut().clear());
            CAP.with(|c| c.set(None));
        }
    }
}

/// Hands the pending window
/// `state.out_buf[state.out_start .. state.out_start + state.out_pending]` to the
/// operating system, keeping the unwritten remainder addressable — the inner
/// drain loop of C `gz_comp` (`gzwrite.c` L114-L128).
///
/// C advances a cursor that lives on the state
/// (`while (strm->next_out > state->x.next) { … state->x.next += writ; }`), so a
/// write that stops early leaves the remainder reachable for the next call. This
/// does the same thing with an index: a successful partial write advances
/// [`GzState::out_start`](crate::gz::state::GzState) by the amount accepted and
/// reduces [`GzState::out_pending`](crate::gz::state::GzState) by the same amount,
/// so on return the window still names exactly the bytes the OS has not taken.
///
/// # Why the cursor advances instead of compacting
///
/// Sliding the unwritten remainder back to `out_buf[0]` after every partial write
/// would make delivering an `N`-byte window cost `(N-1) + (N-2) + … + 1` byte
/// copies against a destination that accepts one byte at a time — quadratic work
/// an ordinary flow-controlled pipe or socket can provoke, and unlike C, whose
/// pointer bump copies nothing. Advancing an offset is `O(1)` per write and
/// `O(N)` overall, matching C exactly.
///
/// # What this deliberately does *not* do
///
/// It does **not** re-anchor the window at `out_buf[0]` when it empties. C splits
/// those two steps: its inner `while (strm->next_out > state->x.next)` loop only
/// advances `state->x.next`, and the buffer reclaim
/// `strm->avail_out = state->size; strm->next_out = state->out;
/// state->x.next = state->out;` is guarded by `if (strm->avail_out == 0)`
/// (`gzwrite.c` L125-L128). A drain provoked by a *flush* therefore leaves
/// `next_out` exactly where it was, and the following `deflate` appends after it
/// rather than restarting at the front. Reproducing that split is what keeps the
/// number of `deflate` calls, and the output slice each one receives, identical to
/// C; the reclaim itself is performed by [`gz_deflate_loop`] under C's guard.
///
/// [`std::io::Write::write_all`] is deliberately not used: it reports only
/// *whether* it failed, not how far it got, so a partial write followed by a
/// fault would lose the progress this window exists to preserve.
///
/// # A write that accepts nothing is retried in place
///
/// C has exactly one success arm — `state->x.next += writ` (`gzwrite.c` L124) —
/// so a `writ` of `0` advances nothing and the enclosing
/// `while (strm->next_out > state->x.next)` re-issues the identical request. This
/// loop reproduces that shape rather than special-casing `Ok(0)`: the cursor is
/// advanced by whatever was accepted, zero included, and the condition is
/// re-tested. Reporting a zero-byte acceptance as an error instead would be an
/// observable behaviour change C callers never see, and it is not one of the five
/// divergences `doc/technical-specifications.md` §0.8.2 sanctions.
///
/// The retry cannot become a live spin for the same reason C's does not: POSIX
/// permits `write(2)` to return `0` only for a zero-length request, and neither
/// this loop nor C's ever issues one — `state.out_pending > 0` bounds the request
/// below by one byte.
///
/// # Errors
///
/// * [`ZlibError::ErrNo`] on a non-blocking stall
///   ([`io::ErrorKind::WouldBlock`]), which additionally sets
///   [`GzState::again`](crate::gz::state::GzState) so the caller may retry;
/// * [`ZlibError::ErrNo`] on any other write failure.
///
/// On every error path the pending window is left intact, so a retry resumes at
/// the exact byte the OS stopped at.
fn drain_pending(state: &mut GzState) -> Result<(), ZlibError> {
    while state.out_pending > 0 {
        state.again = false;
        let start = state.out_start;
        let end = start + core::cmp::min(WRITE_MAX, state.out_pending);
        match write_some(&mut state.file, &state.out_buf[start..end]) {
            Ok(written) => {
                // Advance the cursor past what the OS took — C's
                // `state->x.next += writ` (C L124). No bytes move. A zero-byte
                // acceptance advances nothing, so the `while` re-issues the same
                // request; that is C's single success arm reproduced exactly, and
                // C has no `writ == 0` case of its own (see the section above).
                state.out_start += written;
                state.out_pending -= written;
            }
            // A signal interrupted the write before any byte moved; retry.
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                state.again = true;
                state.error(ReturnCode::ErrNo, Some("write error"));
                return Err(ZlibError::ErrNo);
            }
            Err(_) => {
                state.error(ReturnCode::ErrNo, Some("write error"));
                return Err(ZlibError::ErrNo);
            }
        }
    }

    // The window is empty and `out_start` has caught up to the produced-bytes
    // frontier — C's post-loop state `state->x.next == strm->next_out`. The
    // buffer reclaim that returns both to `out_buf[0]` is C's separate,
    // `avail_out == 0`-guarded step and belongs to the caller (see above).
    Ok(())
}

/// Writes `input` straight to the file with no compression — the transparent
/// (`direct`) branch of C `gz_comp` (`gzwrite.c` L74-L95).
///
/// Reports how many bytes reached the file, **including** on the error paths, so
/// a stalled transparent write does not under-report its progress and provoke a
/// duplicating retry (see [`CompOutcome`]). On a non-blocking stall
/// ([`io::ErrorKind::WouldBlock`]) it sets
/// [`GzState::again`](crate::gz::state::GzState) and reports
/// [`ReturnCode::ErrNo`]; any other write failure likewise reports
/// [`ReturnCode::ErrNo`]. A writer that accepts nothing (`Ok(0)`) while bytes
/// remain is **retried in place**, leaving `off` where it was, exactly as C's
/// `strm->next_in += writ` over `while (strm->avail_in)` does with a `writ` of
/// zero (`gzwrite.c` L76-L90); see [`drain_pending`] for why that cannot become a
/// live spin.
///
/// No pending-output window is needed here: the bytes still live in the caller's
/// `input` slice, so the accurate `consumed` count is all a retry requires.
fn write_direct(state: &mut GzState, input: &[u8]) -> CompOutcome {
    let mut off = 0usize;
    while off < input.len() {
        state.again = false;
        let end = off + core::cmp::min(WRITE_MAX, input.len() - off);
        match write_some(&mut state.file, &input[off..end]) {
            // C's single success arm: `strm->next_in += writ` (C L90). A `writ`
            // of zero advances nothing and `while (strm->avail_in)` re-issues the
            // same request, so a destination that accepts nothing is retried in
            // place rather than reported.
            Ok(written) => off += written,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                state.again = true;
                state.error(ReturnCode::ErrNo, Some("write error"));
                return CompOutcome::err(off, ZlibError::ErrNo);
            }
            Err(_) => {
                state.error(ReturnCode::ErrNo, Some("write error"));
                return CompOutcome::err(off, ZlibError::ErrNo);
            }
        }
    }
    CompOutcome::ok(off)
}

/// Runs `deflate` over `input`, writing every produced byte to the file, until
/// the engine has no more work for the requested `flush` — the compress loop of
/// C `gz_comp` (`gzwrite.c` L109-L142).
///
/// `input` must **not** alias any field of `state` (it is either an external
/// caller slice or the `in_buf` moved out with [`core::mem::take`]), which is
/// what lets the loop borrow [`GzState::strm`](crate::gz::state::GzState),
/// [`GzState::out_buf`](crate::gz::state::GzState), and
/// [`GzState::file`](crate::gz::state::GzState) as disjoint fields.
///
/// # When output reaches the operating system
///
/// Compressed bytes accumulate in `out_buf` and are handed to the file at exactly
/// C's two checkpoints, evaluated at the top of every iteration
/// (`gzwrite.c` L112-L128):
///
/// * the scratch area is **full** (`avail_out == 0`), or
/// * the caller asked for a **flush** — but while finishing, not until `deflate`
///   has reported `Z_STREAM_END`, so a `Z_FINISH` writes the final block and the
///   gzip trailer together rather than in pieces.
///
/// Draining after *every* `deflate` call instead would be observably different,
/// not merely chattier: a `Z_NO_FLUSH` write to a destination that cannot accept
/// bytes right now — a full non-blocking pipe or socket — would surface
/// `Z_ERRNO` from `gzwrite`, where C buffers the output, returns the full count,
/// and leaves `gzerror` reporting `Z_OK`. The DEFLATE byte stream is unaffected
/// either way; the *error* the application sees is not.
///
/// # Ordering invariant
///
/// `deflate` is only ever handed the still-unproduced tail of the scratch area,
/// `out_buf[out_start + out_pending .. size]`, so it can never overwrite a byte
/// the file has not received. The buffer is reclaimed back to `out_buf[0]` only
/// once it is both full and fully written — C's `if (strm->avail_out == 0)` arm —
/// which makes the sequence of output slices handed to the engine independent of
/// how the destination chunked its acceptance, and the compressed bytes therefore
/// byte-identical to reference zlib.
///
/// # Errors
///
/// * [`ZlibError::StreamError`] if `deflate` reports a corrupt stream
///   (C L136-L140) — fatal.
/// * [`ZlibError::ErrNo`] on a file write error (C L119-L124), including a
///   non-blocking stall (which also sets [`GzState::again`](crate::gz::state::GzState)).
///
/// Either way the returned [`CompOutcome::consumed`] is the true number of input
/// bytes the engine ingested, and any undelivered output remains pending in
/// [`GzState::out_pending`](crate::gz::state::GzState) for the next call.
fn gz_deflate_loop(state: &mut GzState, input: &[u8], flush: FlushMode) -> CompOutcome {
    let size = state.size;
    let flush_i32 = flush.as_c_int();
    let mut consumed = 0usize;
    // C seeds `ret = Z_OK` before the loop (`gzwrite.c` L109) purely so the
    // `flush == Z_FINISH` arm of the write checkpoint below can ask whether the
    // *previous* `deflate` already reported `Z_STREAM_END`.
    let mut code = ReturnCode::Ok;

    loop {
        // ---- C L112-L128: the write checkpoint. -------------------------------
        // Free space left in the scratch area — C's `strm->avail_out`. The
        // produced-bytes frontier (C's `next_out`) sits at
        // `out_start + out_pending`.
        let avail_out = size - (state.out_start + state.out_pending);
        if avail_out == 0
            || (flush != FlushMode::NoFlush
                && (flush != FlushMode::Finish || code == ReturnCode::StreamEnd))
        {
            // Deliver everything produced but not yet accepted. On a stall this
            // returns with the window intact so the retry resumes exactly where
            // the operating system stopped.
            if let Err(e) = drain_pending(state) {
                return CompOutcome::err(consumed, e);
            }
            // C L125-L128: reclaim the buffer, but only once it is full *and*
            // fully written. A drain triggered by a flush leaves the frontier
            // where it was, exactly as C leaves `next_out`.
            if avail_out == 0 {
                debug_assert_eq!(state.out_pending, 0, "drain_pending emptied the window");
                debug_assert_eq!(state.out_start, size, "the frontier reached the buffer end");
                state.out_start = 0;
            }
        }

        // ---- C L130-L134: compress into the free tail of the scratch area. ----
        // `input` is external and `strm`/`out_buf` are distinct `state` fields,
        // so these borrows are disjoint. The slice starts at the frontier, never
        // at `0`, so bytes the file has not yet accepted are never overwritten.
        let frontier = state.out_start + state.out_pending;
        let outcome = deflate::deflate(
            &mut state.strm,
            &input[consumed..],
            &mut state.out_buf[frontier..size],
            flush_i32,
        );

        // A corrupt stream is fatal (C L136-L140).
        if outcome.code == ReturnCode::StreamError {
            state.error(
                ReturnCode::StreamError,
                Some("internal error: deflate stream corrupt"),
            );
            return CompOutcome::err(consumed, ZlibError::StreamError);
        }

        consumed += outcome.consumed;
        code = outcome.code;
        // Advance the frontier by what this call produced — C's implicit
        // `next_out += have`. Nothing is written here; the checkpoint above
        // decides when the operating system sees these bytes.
        state.out_pending += outcome.produced;

        // ---- C L142: `} while (have);` ----------------------------------------
        // `have` is precisely the number of bytes the last `deflate` produced.
        // Producing nothing means the engine has no more work for this flush:
        // with a non-empty output slice (guaranteed above, since the checkpoint
        // reclaims whenever the area is full) `deflate` stops only when it has
        // taken all the input it can, so this cannot exit with work outstanding.
        if outcome.produced == 0 {
            debug_assert!(
                outcome.consumed == input.len() - (consumed - outcome.consumed)
                    || outcome.consumed == 0,
                "deflate produced nothing yet left input unconsumed"
            );
            break;
        }
    }

    CompOutcome::ok(consumed)
}

/// Compress-and-write step over an explicit `input` slice — the body of C
/// `gz_comp` (`gzwrite.c` L65-L148) minus the lazy buffer allocation (callers
/// guarantee the buffers are initialized). Returns the bytes consumed.
///
/// Handles the transparent (`direct`) branch, the pending-reset branch, the
/// compress loop, and the post-`Z_FINISH` reset arm. `input` must not alias any
/// `state` field (see [`gz_deflate_loop`]).
///
/// The ingested count is reported on both the success and the error paths (see
/// [`CompOutcome`]).
fn gz_comp_slice(state: &mut GzState, input: &[u8], flush: FlushMode) -> CompOutcome {
    // Write directly if requested (C L73-L96).
    if state.direct == 1 {
        return write_direct(state, input);
    }

    // Check for a pending reset (C L98-L108): after a `Z_FINISH` a new gzip
    // member must not be started until there is data to write and we are not
    // merely flushing.
    if state.reset {
        if input.is_empty() && flush == FlushMode::NoFlush {
            return CompOutcome::ok(0);
        }
        // Any output the previous member left stalled must reach the file before
        // the engine is reset for a new one, or the finished member would be
        // truncated mid-trailer.
        if let Err(e) = drain_pending(state) {
            return CompOutcome::err(0, e);
        }
        if deflate::deflate_reset(&mut state.strm).is_err() {
            state.error(
                ReturnCode::StreamError,
                Some("internal error: deflate stream corrupt"),
            );
            return CompOutcome::err(0, ZlibError::StreamError);
        }
        state.reset = false;
    }

    // Run deflate() until it produces no more output (C L110-L142).
    let outcome = gz_deflate_loop(state, input, flush);
    if outcome.result.is_err() {
        return outcome;
    }

    // If that completed a deflate stream, allow another to start (C L144-L146).
    if flush == FlushMode::Finish {
        state.reset = true;
    }

    outcome
}

/// Compress whatever is buffered in `in_buf[0..have]` and write it to the file
/// — the public face of C `gz_comp` (`gzwrite.c` L65-L148).
///
/// This is the shared workhorse consumed by [`gzflush`] (here), `gzsetparams`
/// (in `open.rs`), and `gzclose_w` (in `close.rs`), so it is `pub(crate)`. It
/// lazily allocates the buffers on first use, then delegates to
/// [`gz_comp_slice`] over the buffered input. The buffered input is moved out
/// with [`core::mem::take`] so the slice does not alias `state`; whatever the
/// engine ingested is then removed from the buffer.
///
/// # Buffer bookkeeping
///
/// [`GzState::have`](crate::gz::state::GzState) is reduced by the number of bytes
/// the engine actually took, and any remainder is compacted back to
/// `in_buf[0]`. On the ordinary success path everything is consumed and `have`
/// becomes `0`; after a non-blocking stall the untouched tail survives so the
/// retry submits it exactly once. Reference zlib gets this for free — its
/// `strm->next_in`/`avail_in` keep pointing into `state->in` across the call — so
/// this compaction is what reproduces C's behaviour with the cursor relocated
/// onto the state. Leaving `have` unchanged instead would resubmit already
/// ingested bytes on the retry and duplicate them in the compressed stream.
///
/// Compacting (rather than tracking a second offset) also preserves the
/// `next_in == in` invariant that [`gz_vacate`] and the [`gz_write`] /
/// [`gzputc`] / [`gzvprintf`] fast paths rely on: buffered input always begins at
/// `in_buf[0]`.
///
/// # Errors
///
/// Propagates the [`ZlibError`] from [`gz_init`] (memory) or [`gz_comp_slice`]
/// (a corrupt stream or a file write error); the state's error code is set as a
/// side effect.
///
/// # Reporting the ingested count
///
/// Callers that must credit partial progress before reacting to a failure — the
/// only one is [`gz_zero`], mirroring C's
/// `n -= strm->avail_in; state->x.pos += n; state->skip -= n;` *before*
/// `if (ret == -1) return -1;` (`gzwrite.c` L177-L181) — use
/// [`gz_comp_counted`] instead, which is this function without the discarded
/// count. The [`Result`](core::result::Result)-returning wrapper is kept because
/// every other call site genuinely has nothing to do with the count, and
/// widening them all would invite the mistake of ignoring it.
pub(crate) fn gz_comp(state: &mut GzState, flush: FlushMode) -> Result<(), ZlibError> {
    gz_comp_counted(state, flush).result
}

/// [`gz_comp`] that also reports how much of the buffered input the engine
/// ingested, valid on the error paths as well as on success.
///
/// See [`gz_comp`] for the behaviour and [`CompOutcome`] for why the count has to
/// survive the error. The reported count is the engine's true intake — the exact
/// analogue of C's `n - strm->avail_in` — and is deliberately *not* the clamped
/// value used below for the `in_buf` bookkeeping: a fatal error zeroes
/// [`GzState::have`](crate::gz::state::GzState) to signal that nothing is left to
/// hand back, which must not be mistaken for "nothing was taken".
fn gz_comp_counted(state: &mut GzState, flush: FlushMode) -> CompOutcome {
    // Allocate memory if this is the first time through (C L70-L71).
    if state.size == 0 {
        if let Err(e) = gz_init(state) {
            return CompOutcome::err(0, e);
        }
    }

    // Move the input buffer out so the borrow of its `[0..have]` slice does not
    // conflict with the `&mut state` handed to `gz_comp_slice`. `take` leaves a
    // cheap empty `Vec` behind, which is swapped back immediately afterwards.
    let buf = core::mem::take(&mut state.in_buf);
    let have = state.have;
    let outcome = gz_comp_slice(state, &buf[..have], flush);
    state.in_buf = buf;

    // Drop exactly the bytes the engine ingested, sliding any remainder to the
    // front so buffered input still starts at `in_buf[0]`.
    //
    // The clamp is against the *current* `have`, not the `have` snapshot taken
    // above, because a fatal (non-retryable) failure inside `gz_comp_slice`
    // already zeroed it through
    // [`GzState::error`](crate::gz::state::GzState::error) to mark that there is
    // nothing left to give back. Reading the live value keeps that decision
    // intact — computing `have - consumed` from the stale snapshot would
    // resurrect buffered input on a stream that has just been declared dead.
    // Where `have` was preserved (success, or a non-blocking stall, where
    // `again` suppresses the zeroing) the live value equals the snapshot, so the
    // arithmetic is unchanged.
    let consumed = core::cmp::min(outcome.consumed, state.have);
    if consumed > 0 {
        let end = state.have;
        state.in_buf.copy_within(consumed..end, 0);
        state.have = end - consumed;
    }

    outcome
}

/// Compress [`GzState::skip`](crate::gz::state::GzState) zero bytes to the
/// output — port of C `gz_zero` (`gzwrite.c` L154-L182).
///
/// Used to satisfy a forward seek on a write stream: the gap is realized by
/// compressing the requested number of zero bytes. Any buffered input is
/// flushed first, then zeros are fed in `size`-sized chunks.
///
/// # Crediting progress before propagating a failure
///
/// Each chunk's ingested count is added to
/// [`GzState::pos`](crate::gz::state::GzState) and subtracted from
/// [`GzState::skip`](crate::gz::state::GzState) **before** the chunk's result is
/// inspected, which is the order C uses verbatim (`gzwrite.c` L177-L181):
///
/// ```text
/// ret = gz_comp(state, Z_NO_FLUSH);
/// n -= strm->avail_in;
/// state->x.pos += n;
/// state->skip -= n;
/// if (ret == -1)
///     return -1;
/// ```
///
/// The order is what bounds the damage a non-blocking stall can do. `deflate` may
/// have ingested part of the chunk before the drain hit
/// `EAGAIN`/`EWOULDBLOCK`; if `skip` were left untouched (the shape `?` produces),
/// the retry would regenerate every zero the engine had already compressed —
/// emitting more zeros than the seek asked for, changing the output, and doing the
/// work twice. Crediting first means the retry re-stages only the part of the
/// chunk the engine did *not* take, exactly as reference zlib does.
///
/// # Errors
///
/// Propagates the [`ZlibError`] from [`gz_comp`] / [`gz_comp_counted`].
pub(crate) fn gz_zero(state: &mut GzState) -> Result<(), ZlibError> {
    // Consume whatever is left in the input buffer (C L160-L162).
    if state.have != 0 {
        gz_comp(state, FlushMode::NoFlush)?;
    }

    // Compress `skip` zero bytes (C L164-L181).
    let mut first = true;
    while state.skip > 0 {
        // n = min(size, skip), guarded against the size not fitting in the
        // 64-bit offset (C `GT_OFF`); `size` is a small buffer size and `skip`
        // is positive here, so a plain minimum suffices.
        let n = core::cmp::min(state.size as u64, state.skip as u64) as usize;

        // Only need to zero the buffer once, then it is reused (C L167-L170).
        if first {
            for b in state.in_buf[..n].iter_mut() {
                *b = 0;
            }
            first = false;
        }

        // Present the `n` zero bytes as the buffered input and compress them.
        state.have = n;
        let outcome = gz_comp_counted(state, FlushMode::NoFlush);

        // Credit what the engine actually took, then react to the result — C's
        // `n -= strm->avail_in; state->x.pos += n; state->skip -= n;` followed by
        // `if (ret == -1) return -1;`. `consumed` can never exceed `n`, so `skip`
        // cannot go negative and the loop still terminates.
        debug_assert!(
            outcome.consumed <= n,
            "the engine cannot take more than it was given"
        );
        state.pos += outcome.consumed as i64;
        state.skip -= outcome.consumed as i64;
        outcome.result?;

        // Termination guarantee. `skip` now shrinks by the ingested count rather
        // than by the whole chunk, so forward progress is no longer structural: a
        // step that reported success while ingesting nothing would re-stage the
        // same chunk forever. `deflate` cannot legitimately do that — it is handed
        // a non-empty scratch buffer (`want`, and therefore `size`, is at least
        // `8`) together with at least one input byte, so it must consume, produce,
        // or fail — which makes this unreachable in practice; reporting it as the
        // engine misbehaviour it would be is the only safe alternative to spinning.
        if outcome.consumed == 0 {
            state.error(
                ReturnCode::StreamError,
                Some("internal error: deflate stream corrupt"),
            );
            return Err(ZlibError::StreamError);
        }
    }

    Ok(())
}

/// Write `buf` to the file, returning the number of bytes consumed — port of
/// C `gz_write` (`gzwrite.c` L188-L252).
///
/// Small writes are copied into the input buffer and compressed once it fills
/// (amortizing the per-call engine overhead); a write at least as large as the
/// buffer first drains any buffered input, then feeds the caller's slice
/// directly to the engine in `u32::MAX`-sized chunks. Both paths produce
/// identical DEFLATE output.
///
/// If the returned value is less than `buf.len()`, an error occurred. For a
/// non-blocking stall the count of bytes accepted so far is returned (the
/// caller may retry); for any other error `0` is returned.
pub(crate) fn gz_write(state: &mut GzState, buf: &[u8]) -> usize {
    // If len is zero, avoid unnecessary operations (C L193-L194).
    if buf.is_empty() {
        return 0;
    }

    // Allocate memory if this is the first time through (C L197-L198).
    if state.size == 0 && gz_init(state).is_err() {
        return 0;
    }

    // Check for a seek request (C L201-L202).
    if state.skip != 0 && gz_zero(state).is_err() {
        return 0;
    }

    let put = buf.len();

    if buf.len() < state.size {
        // For small len, copy to the input buffer, compressing when full
        // (C L205-L226).
        let mut off = 0usize;
        loop {
            // Free region of the input buffer. Buffered input occupies
            // `in_buf[0..have]` (C `next_in == in`), so the next free byte is at
            // `have`. `saturating_sub` guards the (post-`gzprintf`) case where
            // `have` momentarily exceeds `size`.
            let have = state.have;
            let copy = core::cmp::min(state.size.saturating_sub(have), buf.len() - off);
            state.in_buf[have..have + copy].copy_from_slice(&buf[off..off + copy]);
            state.have += copy;
            state.pos += copy as i64;
            off += copy;

            if off == buf.len() {
                break;
            }

            // The input buffer is full — compress it (C L224-L225).
            if gz_comp(state, FlushMode::NoFlush).is_err() {
                // Report partial progress only on a non-blocking stall.
                return if state.again { off } else { 0 };
            }
        }
    } else {
        // Consume whatever is left in the input buffer (C L229-L230).
        if state.have != 0 && gz_comp(state, FlushMode::NoFlush).is_err() {
            return 0;
        }

        // Directly compress the user buffer to the file (C L233-L247). The
        // caller's slice is external, so feeding it to the engine involves no
        // extra copy and no aliasing with `state`.
        //
        // The ingested count is credited **before** the outcome is inspected,
        // exactly as C does (`n -= strm->avail_in; state->x.pos += n; len -= n;`
        // and only then `if (ret == -1) return state->again ? put - len : 0;`,
        // C L239-L246). Crediting it only on the success arm would under-report a
        // stalled write, and a caller retrying from the returned offset would
        // hand the engine bytes it had already compressed — silently duplicating
        // them in the output stream.
        let mut off = 0usize;
        while off < buf.len() {
            let n = core::cmp::min(u32::MAX as usize, buf.len() - off);
            let outcome = gz_comp_slice(state, &buf[off..off + n], FlushMode::NoFlush);
            state.pos += outcome.consumed as i64;
            off += outcome.consumed;
            if outcome.result.is_err() {
                // Report partial progress only on a non-blocking stall
                // (C `return state->again ? put - len : 0`).
                return if state.again { off } else { 0 };
            }
            if outcome.consumed < n {
                // A short consume without an error can only mean a non-blocking
                // stall that the engine absorbed without failing.
                return if state.again { off } else { 0 };
            }
        }
    }

    // Input was all buffered or compressed (C L251).
    put
}

/// Write `buf` to the gzip file — port of C `gzwrite` (`gzwrite.c` L255-L277).
///
/// Returns the number of bytes written, or `0` on error or if the stream is not
/// writable. Because the count is returned as an [`i32`], a length that does not
/// fit in a positive [`i32`] is rejected with [`ReturnCode::DataError`], exactly
/// as the C interface guards against its `unsigned`→`int` return.
pub fn gzwrite(state: &mut GzState, buf: &[u8]) -> i32 {
    // Check that we're writing and that there's no serious error (C L263-L266).
    if !write_ready(state) {
        return 0;
    }
    state.clear_error();

    // Since an int is returned, make sure the length fits in one (C L270-L273).
    if buf.len() > i32::MAX as usize {
        state.error(
            ReturnCode::DataError,
            Some("requested length does not fit in int"),
        );
        return 0;
    }

    // Write the bytes (the return value now fits in an int, C L276).
    gz_write(state, buf) as i32
}

/// Write `nitems` items of `size` bytes each to the gzip file — port of C
/// `gzfwrite` (`gzwrite.c` L280-L304).
///
/// `buf` must contain at least `size * nitems` bytes. This precondition is
/// enforced: if `buf` is smaller than the requested `size * nitems`, the request
/// is rejected with [`ReturnCode::StreamError`] and `0` is returned — the write
/// is **not** silently truncated to `buf.len()`, so a caller-side sizing mistake
/// surfaces as an error instead of being hidden (and the C-compatible request
/// contract the FFI layer relies on is preserved). Returns the number of full
/// items written; `0` on error or overflow. The `size * nitems` multiplication
/// is overflow-checked, matching the C guard that the request fits in a
/// `size_t`.
pub fn gzfwrite(state: &mut GzState, buf: &[u8], size: usize, nitems: usize) -> usize {
    // Check that we're writing and that there's no serious error (C L289-L292).
    if !write_ready(state) {
        return 0;
    }
    state.clear_error();

    // Compute the number of bytes to write, erroring on overflow (C L295-L299).
    let len = match nitems.checked_mul(size) {
        Some(len) => len,
        None => {
            state.error(
                ReturnCode::StreamError,
                Some("request does not fit in a size_t"),
            );
            return 0;
        }
    };

    // Write `len` bytes, returning the number of full items written (C L303).
    if len == 0 {
        return 0;
    }

    // Enforce the documented `buf` >= `size * nitems` precondition. C uses a raw
    // pointer and trusts the caller to have provided a large enough buffer; the
    // safe-slice API instead surfaces an undersized buffer as a stream error and
    // writes nothing, rather than silently clamping the write to `buf.len()`
    // (which would hide the caller's mistake and let the returned item count
    // diverge from the C `gz_write(state, buf, len) / size` contract).
    if buf.len() < len {
        state.error(
            ReturnCode::StreamError,
            Some("input buffer smaller than requested size * nitems"),
        );
        return 0;
    }

    gz_write(state, &buf[..len]) / size
}

/// Write one byte `c` (its low 8 bits) to the gzip file — port of C `gzputc`
/// (`gzwrite.c` L307-L347).
///
/// Returns the byte written (`c & 0xff`) on success, or `-1` on error. The fast
/// path appends straight into the input buffer when there is room, avoiding a
/// call into `gz_write`.
pub fn gzputc(state: &mut GzState, c: i32) -> i32 {
    // Check that we're writing and that there's no serious error (C L318-L321).
    if !write_ready(state) {
        return -1;
    }
    state.clear_error();

    // Check for a seek request (C L324-L325).
    if state.skip != 0 && gz_zero(state).is_err() {
        return -1;
    }

    // Try writing to the input buffer for speed (C L329-L340). `size == 0` means
    // the buffers are not initialized yet, in which case fall through.
    if state.size != 0 {
        let have = state.have;
        if have < state.size {
            state.in_buf[have] = c as u8;
            state.have += 1;
            state.pos += 1;
            return c & 0xff;
        }
    }

    // No room in the buffer (or not initialized) — use gz_write (C L343-L346).
    let byte = [c as u8];
    if gz_write(state, &byte) != 1 {
        return -1;
    }
    c & 0xff
}

/// Write the string `s` (its bytes) to the gzip file — port of C `gzputs`
/// (`gzwrite.c` L350-L372).
///
/// Returns the number of bytes written, or `-1` on error. Mirrors the C guard
/// that the string length fits in an [`i32`].
pub fn gzputs(state: &mut GzState, s: &str) -> i32 {
    // Check that we're writing and that there's no serious error (C L358-L361).
    if !write_ready(state) {
        return -1;
    }
    state.clear_error();

    // Length, with the "fits in an int" guard (C L364-L368).
    let len = s.len();
    if len > i32::MAX as usize {
        state.error(
            ReturnCode::StreamError,
            Some("string length does not fit in int"),
        );
        return -1;
    }

    // Write the string; a non-empty string that wrote nothing is an error
    // (C L369-L371).
    let put = gz_write(state, s.as_bytes());
    if len != 0 && put == 0 { -1 } else { put as i32 }
}

/// If the second half of the input buffer is occupied, write out the contents;
/// if input remains after a non-blocking stall, keep it at the front of the
/// buffer — port of C `gz_vacate` (`gzwrite.c` L382-L396).
///
/// Returns `true` if this did **not** free up the second half of the buffer
/// (i.e. more than `size` bytes are still buffered). The caller must inspect
/// [`GzState::err`](crate::gz::state::GzState) afterwards to detect a
/// [`gz_comp`] error. Because this port maintains the `next_in == in` invariant
/// (buffered input always starts at `in_buf[0]`), the C `memmove` that slides
/// leftover input to the front is a no-op and is omitted.
fn gz_vacate(state: &mut GzState) -> bool {
    // If the current contents fit within the first `size` bytes, the second
    // half is already free (C L389-L390): buffered input is `in_buf[0..have]`.
    if state.have <= state.size {
        return false;
    }

    // Otherwise write out what we can (C L391); the error is left on `state`
    // for the caller to observe.
    let _ = gz_comp(state, FlushMode::NoFlush);

    // If everything drained, the buffer is empty and starts at the front
    // (C L392-L395).
    if state.have == 0 {
        return false;
    }

    // Leftover remains (only on a non-blocking stall); it already starts at
    // `in_buf[0]`, so report whether it still exceeds `size` (C L396).
    state.have > state.size
}

/// A [`core::fmt::Write`] sink that renders **directly into a caller-supplied
/// byte region** and refuses to grow.
///
/// This is the port's stand-in for C's `vsnprintf(next, state->size, …)`
/// (`gzwrite.c` L467): the destination is the already-allocated free half of the
/// gzip input buffer, the capacity is fixed before formatting starts, and a
/// fragment that does not fit stops [`core::fmt::write`] immediately by
/// returning [`core::fmt::Error`] rather than reallocating. Formatting a
/// caller-controlled `format_args!` therefore performs **no allocation at all**
/// and cannot be driven past the configured buffer size, which is what keeps a
/// hostile or merely large format string from turning into an unbounded
/// allocation (and, with the global allocator's infallible `handle_alloc_error`,
/// a process abort) on an ordinary write path.
///
/// The written prefix is always valid: `write_str` copies a whole `&str`
/// fragment or none of it, so [`Self::len`] bytes of `buf` hold exactly the
/// concatenation of the accepted fragments.
struct BoundedWriter<'a> {
    /// The destination region. Its length **is** the capacity — writing stops at
    /// `buf.len()`, never beyond it.
    buf: &'a mut [u8],
    /// Bytes accepted so far; always `<= buf.len()`.
    len: usize,
    /// Set once a fragment was refused for lack of room. Once set it stays set,
    /// because [`core::fmt::write`] aborts on the first `Err`.
    overflowed: bool,
}

impl<'a> BoundedWriter<'a> {
    /// Wraps `buf` as an empty sink whose capacity is `buf.len()`.
    #[inline]
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflowed: false,
        }
    }
}

impl core::fmt::Write for BoundedWriter<'_> {
    #[inline]
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        // `self.len <= self.buf.len()` is an invariant, so this cannot underflow
        // and the comparison cannot overflow.
        if bytes.len() > self.buf.len() - self.len {
            self.overflowed = true;
            return Err(core::fmt::Error);
        }
        let end = self.len + bytes.len();
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// Formatted write to the gzip file — port of C `gzvprintf` (`gzwrite.c`
/// L420-L485, the `gz_vacate`-based variant).
///
/// Because Rust has no C variadics, the formatted text is supplied as
/// [`core::fmt::Arguments`] (produced by [`format_args!`]); the raw
/// `gzprintf(file, fmt, ...)` C entry point is reconstructed only in
/// `src/ffi/gz.rs`. The formatted bytes are staged in the free second half of
/// the double-sized input buffer and then compressed, exactly mirroring the C
/// `vsnprintf`-into-`next` sequence.
///
/// # Formatting is bounded and allocation-free
///
/// The text is rendered straight into `in_buf[have ..]` through the private
/// `BoundedWriter` sink, whose capacity is fixed at `size - 1` before formatting
/// begins. Nothing is allocated, and a caller-controlled format string cannot
/// grow the destination: the first fragment that does not fit ends
/// [`core::fmt::write`] and the call reports `0`, exactly as C reports `0` when
/// `vsnprintf` truncates. The `size - 1` bound is C's own effective capacity —
/// C reserves the final byte of the `size`-byte window as an overflow sentinel
/// (`next[state->size - 1] = 0` at C L453, re-tested at C L472), so the longest
/// result C can accept is `size - 1` bytes. Expressing that as the sink's
/// capacity replaces both the sentinel write and the `len >= size` re-test with
/// a bound the writer cannot cross.
///
/// Returns the number of formatted bytes written, `0` if the result did not fit
/// in the buffer, or a negative [`ReturnCode`] code on error.
pub fn gzvprintf(state: &mut GzState, args: core::fmt::Arguments<'_>) -> i32 {
    // Check that we're writing and that there's no serious error (C L436-L439).
    if !write_ready(state) {
        return ReturnCode::StreamError.as_c_int();
    }
    state.clear_error();

    // Make sure we have some buffer space (C L442-L443).
    if state.size == 0 && gz_init(state).is_err() {
        return state.err.as_c_int();
    }

    // Check for a seek request (C L446-L447).
    if state.skip != 0 && gz_zero(state).is_err() {
        return state.err.as_c_int();
    }

    // Ensure the second half of the input buffer is free to format into
    // (C L455). A stall that leaves the second half occupied is reported as a
    // retryable Z_BUF_ERROR (C L456-L468).
    let stalled = gz_vacate(state);
    if state.err != ReturnCode::Ok {
        if stalled && state.again {
            state.error(ReturnCode::BufError, Some("stalled write on gzprintf"));
        }
        if !state.again {
            return state.err.as_c_int();
        }
    }

    // Format into the free region. The buffered input occupies `in_buf[0..have]`
    // (`next_in == in`), so formatting begins at offset `have`; the buffer is
    // double-sized, so `size` bytes are available there whenever `gz_vacate`
    // freed the second half (C L450-L453).
    //
    // The capacity is C's effective one, `size - 1`, because C reserves the last
    // byte of the window as an overflow sentinel. It is additionally clamped to
    // what physically remains, which only bites in the one corner C gets wrong:
    // after a non-blocking stall left `have > size`, C still writes
    // `next[state->size - 1]`, i.e. up to `in[3 * size - 2]`, past the end of a
    // `2 * size` buffer. This port formats into whatever room is genuinely left
    // and reports `0` if the result no longer fits, instead of reproducing the
    // overrun.
    let start = state.have;
    let cap = state.size.saturating_sub(1).min(state.in_buf.len() - start);
    let (len, overflowed) = {
        let mut sink = BoundedWriter::new(&mut state.in_buf[start..start + cap]);
        let outcome = core::fmt::write(&mut sink, args);
        // A `Display`/`Debug` impl may itself fail; C sees that as a negative
        // `vsnprintf` return, which its `(unsigned)len >= state->size` test also
        // turns into `0`.
        (sink.len, sink.overflowed || outcome.is_err())
    };

    // Check that the result fits in the buffer (C L471-L473): C rejects a
    // zero-length result, a length that reached `size`, and a clobbered
    // sentinel. Here the length can never reach `size` — the sink stops at
    // `size - 1` — so the three collapse into "nothing formatted" or
    // "did not fit".
    if len == 0 || overflowed {
        return 0;
    }

    // Account for the bytes the sink already placed in `in_buf` and advance the
    // stream position (C L476-L477).
    state.have += len;
    state.pos += len as i64;

    // Write out the buffer if more than half is now occupied (C L497-L499).
    let _ = gz_vacate(state);
    if state.err != ReturnCode::Ok && !state.again {
        return state.err.as_c_int();
    }

    len as i32
}

/// Formatted write to the gzip file — the idiomatic wrapper over [`gzvprintf`]
/// standing in for C `gzprintf` (`gzwrite.c` L486-L494).
///
/// Rust callers pass the formatted text via [`format_args!`], e.g.
/// `gzprintf(state, format_args!("{}", value))`.
#[inline]
pub fn gzprintf(state: &mut GzState, args: core::fmt::Arguments<'_>) -> i32 {
    gzvprintf(state, args)
}

/// Flush pending output to the gzip file with the requested `flush` mode —
/// port of C `gzflush` (`gzwrite.c` L540-L560).
///
/// Returns [`ReturnCode::Ok`] (`0`) on success or the appropriate negative code
/// on error. `flush` must be in `0..=Z_FINISH`.
pub fn gzflush(state: &mut GzState, flush: i32) -> i32 {
    // Check that we're writing and that there's no serious error (C L546-L549).
    if !write_ready(state) {
        return ReturnCode::StreamError.as_c_int();
    }
    state.clear_error();

    // Check the flush parameter (C L552-L553).
    if !(0..=Z_FINISH).contains(&flush) {
        return ReturnCode::StreamError.as_c_int();
    }
    let flush_mode = FlushMode::from_c_int(flush).unwrap_or(FlushMode::NoFlush);

    // Check for a seek request (C L556-L557).
    if state.skip != 0 && gz_zero(state).is_err() {
        return state.err.as_c_int();
    }

    // Compress the remaining data with the requested flush (C L559-L560); the
    // resulting error state is reported via `state.err`.
    let _ = gz_comp(state, flush_mode);
    state.err.as_c_int()
}

/// Finalize a write stream by flushing with [`FlushMode::Finish`], returning the
/// resulting [`ReturnCode`].
///
/// This keeps the write-specific finalization logic in this module: it produces
/// the final block and the gzip CRC-32 + ISIZE trailer (a `Z_FINISH`), after
/// which the buffers, engine, and file are released by
/// [`GzState`](crate::gz::state::GzState)'s `Drop` — the RAII replacement for
/// the C `deflateEnd`/`free`/`close` tail of `gzclose_w` (`gzwrite.c`
/// L667-L700). It is exercised by this module's unit tests and reserved as the
/// write-side finalizer.
///
/// `#[allow(dead_code)]`: `gzclose_w` (in `close.rs`) does **not** call this
/// helper — it inlines a C-exact finalize because this function returns
/// `state.err` on a *successful* flush, which would let a pre-existing non-fatal
/// error (e.g. `Z_ERRNO`) override `gzclose_w`'s result even when the `Z_FINISH`
/// itself succeeded, diverging from C's "only a failed finalize sets `ret`"
/// contract. The function is therefore unused by the library target (only by
/// tests) but is deliberately retained.
#[allow(dead_code)]
pub(crate) fn finish_write(state: &mut GzState) -> ReturnCode {
    match gz_comp(state, FlushMode::Finish) {
        Ok(()) => state.err,
        Err(e) => e.as_return_code(),
    }
}

/// An idiomatic [`std::io::Write`] view of a write-mode gzip file, so Rust
/// callers can use `write!`, `writeln!`, and [`Write::write_all`] alongside the
/// C-compatible `gz*` functions.
///
/// [`Write::write`] delegates to `gz_write`; [`Write::flush`] delegates to
/// `gz_comp` with [`FlushMode::SyncFlush`], which flushes all pending output
/// to the underlying file and aligns to a byte boundary **without** ending the
/// gzip member (finishing is reserved for `gzclose_w`/`finish_write`).
/// [`ZlibError`] is mapped to an [`io::Error`].
impl Write for GzState {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !write_ready(self) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gzip stream is not open for writing",
            ));
        }
        self.clear_error();

        let n = gz_write(self, buf);
        if n == 0 {
            // A non-empty write that consumed nothing is an error; surface the
            // recorded stream error (or a generic one if none was set).
            return Err(match ZlibError::from_return_code(self.err) {
                Some(e) => io::Error::other(e),
                None => io::Error::new(io::ErrorKind::WriteZero, "gzip write made no progress"),
            });
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        gz_comp(self, FlushMode::SyncFlush).map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    //! Unit and round-trip tests for the gzip write side.
    //!
    //! The round-trip tests write data through the `gz*` API, finalize with
    //! [`finish_write`], then decompress the resulting file with the reference
    //! `flate2` decoder (its pure-Rust `miniz_oxide` backend), asserting the
    //! recovered bytes equal the original. This validates both that valid gzip
    //! (RFC 1952) framing is produced and that the output is decodable by an
    //! independent implementation. All buffer handling here is safe Rust —
    //! there is **zero `unsafe`** in this module.

    use super::*;

    use crate::gz::state::How;
    use crate::stream::ZStream;
    use std::fs::{File, OpenOptions};
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use flate2::read::GzDecoder;

    /// `Z_SYNC_FLUSH` numeric flush code (mirrors `FlushMode::SyncFlush`).
    const Z_SYNC_FLUSH: i32 = 2;

    /// A uniquely named output file inside a freshly created, caller-private
    /// directory, removed together with that directory when the guard drops.
    ///
    /// Dereferences to [`Path`], so it can be passed anywhere a `&Path` is
    /// expected.
    ///
    /// # Why not a bare path in the shared temporary directory
    ///
    /// Naming a file `…_<pid>_<counter>.gz` directly in [`std::env::temp_dir`] and
    /// opening it with a truncating `File::create` is both guessable and
    /// link-following: on a shared `/tmp` another user can pre-place — or race in
    /// — a symlink at that name and have the test write through it to a target of
    /// their choosing (CWE-377 insecure temporary file, CWE-59 link following,
    /// CWE-367 time-of-check/time-of-use). Manual `remove_file` calls make it
    /// worse rather than better, because a failing assertion unwinds straight past
    /// them and leaves the name in place for the next attempt.
    ///
    /// So each file gets its own directory created with `mkdir(2)` semantics:
    /// atomic, never following a symlink for the final component, and failing with
    /// [`io::ErrorKind::AlreadyExists`] instead of adopting whatever is already
    /// there. On Unix the mode is set to `0700` **at creation time**, leaving no
    /// window in which another user could enumerate it, let alone plant a name
    /// inside it. Files within it are opened with `create_new(true)`, which fails
    /// rather than following a link or truncating. [`Drop`] then removes the
    /// directory and its contents — on the unwinding path as well as the happy
    /// one.
    struct TempFile {
        /// The private directory; removed recursively on drop.
        dir: PathBuf,
        /// The primary output path inside [`Self::dir`].
        file: PathBuf,
    }

    impl TempFile {
        /// Creates a fresh private directory tagged with `tag` and names an output
        /// file inside it.
        ///
        /// # Panics
        ///
        /// If the directory cannot be created exclusively. A name collision is
        /// retried with the next counter value rather than reported.
        fn new(tag: &str) -> Self {
            static CTR: AtomicU32 = AtomicU32::new(0);
            let base = std::env::temp_dir();
            for _ in 0..64 {
                let n = CTR.fetch_add(1, Ordering::Relaxed);
                // The `blitzy_adhoc_test_` prefix keeps these out of any commit.
                let dir = base.join(format!(
                    "blitzy_adhoc_test_gzwrite_{tag}_{}_{n}",
                    std::process::id()
                ));
                match create_private_dir(&dir) {
                    Ok(()) => {
                        let file = dir.join("out.gz");
                        return Self { dir, file };
                    }
                    // Someone (or a previous run) already owns that name: never
                    // reuse it, just move on to the next candidate.
                    Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create private temp dir {}: {e}", dir.display()),
                }
            }
            panic!("no private temp directory could be created after 64 attempts");
        }

        /// A second path inside the *same* private directory, for the tests that
        /// need two destinations.
        fn sibling(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl std::ops::Deref for TempFile {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.file
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            // Best effort: a test that already failed must not fail again here.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Creates `dir` exclusively and restricted to the current user.
    ///
    /// `DirBuilder::mode` applies the permissions to the `mkdir(2)` call itself,
    /// so the directory is never momentarily group- or world-accessible the way a
    /// create-then-`set_permissions` sequence would leave it.
    #[cfg(unix)]
    fn create_private_dir(dir: &Path) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }

    /// Creates `dir` exclusively.
    ///
    /// Non-Unix targets get no explicit mode: Windows already gives each user a
    /// private `%TEMP%`, and there is no portable `0700` equivalent to apply.
    /// Exclusive creation — the part that defeats a pre-placed name — still holds.
    #[cfg(not(unix))]
    fn create_private_dir(dir: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new().create(dir)
    }

    /// Creates `path` exclusively: it must not already exist, no symlink at that
    /// name is followed, and nothing is truncated.
    fn create_new_file(path: &Path) -> File {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap_or_else(|e| panic!("exclusively create {}: {e}", path.display()))
    }

    /// Builds a fresh write-mode [`GzState`] backed by a real, truncated temp
    /// file. `size` is left `0` so the first write lazily runs [`gz_init`],
    /// exactly as a real `gzopen` would.
    fn new_write_state(
        path: &Path,
        want: usize,
        level: i32,
        strategy: i32,
        direct: i32,
    ) -> GzState {
        let file = create_new_file(path);
        GzState {
            have: 0,
            next: 0,
            pos: 0,
            mode: GzMode::Write,
            file: GzFile::new(file),
            path: path.as_os_str().as_encoded_bytes().to_vec(),
            size: 0,
            want,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            direct,
            how: How::Look,
            junk: 0,
            again: false,
            in_next: 0,
            in_avail: 0,
            start: 0,
            eof: false,
            past: false,
            level,
            strategy,
            reset: false,
            out_pending: 0,
            out_start: 0,
            skip: 0,
            err: ReturnCode::Ok,
            msg: None,
            msg_c: None,
            strm: ZStream::new(),
        }
    }

    /// Decompresses a complete gzip member with the reference `flate2` decoder.
    fn gunzip(compressed: &[u8]) -> Vec<u8> {
        let mut decoder = GzDecoder::new(compressed);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .expect("output is a valid gzip member");
        out
    }

    /// Reads the whole file at `path`.
    ///
    /// Deliberately does **not** remove anything: cleanup belongs to the
    /// [`TempFile`] guard, which runs even when an assertion below this call
    /// unwinds past it.
    fn read_output(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Replaces a state's destination with a handle every `write` will reject,
    /// simulating a destination that stops accepting bytes part-way through a
    /// drain.
    ///
    /// The handle is a **read-only** `File`, so `write(2)` fails with `EBADF`.
    /// That surfaces as an [`io::Error`] whose kind is neither
    /// [`io::ErrorKind::Interrupted`] nor [`io::ErrorKind::WouldBlock`], i.e. the
    /// generic failure arm of [`drain_pending`] / [`write_direct`] — reachable
    /// with no `unsafe`, no extra dependency, and no platform-specific flags
    /// (this module is `#![deny(unsafe_code)]`).
    fn wedge_destination(state: &mut GzState, path: &Path) {
        // `path` was created exclusively by `new_write_state` inside a private
        // 0700 directory, so reopening it by name cannot pick up a foreign file.
        let ro = OpenOptions::new()
            .read(true)
            .open(path)
            .unwrap_or_else(|e| panic!("reopen {} read-only: {e}", path.display()));
        state.file = GzFile::new(ro);
    }

    /// A deterministic, mildly compressible payload of `n` bytes.
    fn corpus(n: usize) -> Vec<u8> {
        let mut s: u32 = 0x1234_5678;
        (0..n)
            .map(|i| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                // Mix runs with pseudo-random bytes so `deflate` produces real
                // output rather than one tiny stored block.
                if i % 7 == 0 { b'A' } else { (s >> 16) as u8 }
            })
            .collect()
    }

    /// Output the destination declined stays pending, and a later successful drain
    /// delivers it — losing nothing.
    ///
    /// The invariant: a drain that stops early must not abandon
    /// `out_buf[off..produced]`, because the next `deflate` call writes over that
    /// region and the destination would then receive a spliced, undecodable
    /// DEFLATE stream. The check is end-to-end: after the destination recovers, the
    /// bytes it received must gunzip to exactly the prefix of the input that the
    /// stream position claims was accepted.
    #[test]
    fn output_the_destination_declined_is_retained_and_resumes_without_loss() {
        // Two destinations, one private directory: the wedged primary and a
        // sibling that stands in for the destination recovering.
        let wedged = TempFile::new("afdefect1");
        let good = wedged.sibling("recovered.gz");
        let data = corpus(65536);

        // `want = 64` keeps the scratch area small, so the first block the engine
        // emits overflows it. The payload has to be large enough to *reach* that
        // block: produced bytes leave the area only when it fills or a flush
        // arrives (`gzwrite.c` L112-L114), and a `Z_NO_FLUSH` `deflate` holds its
        // symbols until the symbol buffer is full — so a 4 KiB write to a
        // destination that rejects every `write(2)` still succeeds, in reference
        // zlib exactly as here. 64 KiB crosses the boundary with room to spare.
        let mut state = new_write_state(&wedged, 64, 6, 0, 0);
        wedge_destination(&mut state, &wedged);

        // `data.len() >= size`, so this takes the direct-to-engine large path.
        let n = gz_write(&mut state, &data);
        assert_eq!(n, 0, "a non-retryable write failure reports zero written");
        assert_eq!(state.err, ReturnCode::ErrNo, "the failure is recorded");
        assert!(!state.again, "EBADF is not a retryable stall");
        assert!(
            state.out_pending > 0,
            "compressed bytes the destination declined must remain pending, \
             not be silently dropped"
        );

        // How much input the engine ingested, per the position counter.
        let accepted = state.pos as usize;
        assert!(
            accepted > 0,
            "the engine ingested input before the destination failed"
        );

        // The destination becomes writable again; finish the member.
        state.file = GzFile::new(create_new_file(&good));
        state.clear_error();
        gz_comp(&mut state, FlushMode::Finish).expect("the resumed flush succeeds");
        assert_eq!(state.out_pending, 0, "everything pending was delivered");

        let bytes = read_output(&good);
        let recovered = gunzip(&bytes);
        assert_eq!(
            recovered.len(),
            accepted,
            "the delivered stream decodes to exactly the accepted byte count"
        );
        assert_eq!(
            recovered.as_slice(),
            &data[..accepted],
            "no compressed byte was lost, duplicated, or reordered"
        );
    }

    /// A failed compress-and-write step still reports how much input it ingested.
    ///
    /// The invariant: the ingested count must survive the error, because it is what
    /// advances the stream position. Discarding it would leave the position
    /// unmoved, and a caller retrying from the reported offset would resubmit bytes
    /// `deflate` had already compressed — duplicating them in the output.
    #[test]
    fn a_failed_write_still_accounts_for_the_ingested_input() {
        let wedged = TempFile::new("afdefect2");
        // Large enough that `deflate` emits a block and overflows the 64-byte
        // scratch area, which is the only thing that makes a `Z_NO_FLUSH` step
        // touch the destination at all (`gzwrite.c` L112-L114).
        let data = corpus(65536);

        let mut state = new_write_state(&wedged, 64, 6, 0, 0);
        wedge_destination(&mut state, &wedged);

        let n = gz_write(&mut state, &data);
        assert_eq!(n, 0, "a non-retryable failure reports zero written");
        assert!(
            state.pos > 0,
            "the ingested count must survive the error (C: `n -= strm->avail_in`)"
        );
        assert!(
            (state.pos as usize) <= data.len(),
            "the accounted count can never exceed the input"
        );
    }

    /// The transparent (`direct`) path must likewise account for the bytes that
    /// reached the destination before it failed, and must report the failure
    /// rather than abandoning the tail silently.
    #[test]
    fn transparent_writes_report_failure_and_account_for_progress() {
        let wedged = TempFile::new("afdefect2_direct");
        let data = corpus(2048);

        let mut state = new_write_state(&wedged, 64, 6, 0, 1);
        wedge_destination(&mut state, &wedged);

        let outcome = write_direct(&mut state, &data);
        assert!(
            outcome.result.is_err(),
            "the failure is reported, not hidden"
        );
        assert_eq!(state.err, ReturnCode::ErrNo);
        assert_eq!(
            outcome.consumed, 0,
            "nothing reached a destination that rejects every write"
        );
    }

    /// A fatal (non-retryable) failure marks the file as having nothing left to
    /// give back, and the input-buffer bookkeeping must not undo that.
    ///
    /// [`GzState::error`](crate::gz::state::GzState::error) zeroes `have` for a
    /// fatal error; [`gz_comp`]'s compaction therefore clamps against the live
    /// value rather than the snapshot it took before the call, so buffered input
    /// is never resurrected on a stream that has just been declared dead.
    ///
    /// The flush is `Z_FINISH` rather than `Z_NO_FLUSH` because that is what makes
    /// the destination be touched at all: produced bytes leave the scratch area
    /// only when the area fills or a flush arrives (`gzwrite.c` L112-L114), and
    /// 100 bytes of `Z_NO_FLUSH` input fill nothing. The bookkeeping under test is
    /// identical on either path — what fails is the write, not the flush code.
    #[test]
    fn a_fatal_write_error_does_not_resurrect_buffered_input() {
        let wedged = TempFile::new("afdefect2_haveclamp");

        let mut state = new_write_state(&wedged, 128, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");
        wedge_destination(&mut state, &wedged);

        // Stage buffered input the way the `gz_write` small path does.
        let data = corpus(100);
        state.in_buf[..data.len()].copy_from_slice(&data);
        state.have = data.len();

        assert!(gz_comp(&mut state, FlushMode::Finish).is_err());
        assert!(!state.again);
        assert_eq!(
            state.have, 0,
            "a fatal error leaves no buffered input to hand back"
        );
    }

    /// Buffered input is fully consumed on the ordinary success path, and the
    /// pending-output window is left empty.
    #[test]
    fn a_successful_comp_consumes_all_buffered_input() {
        let path = TempFile::new("afdefect2_success");

        let mut state = new_write_state(&path, 256, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");
        let data = corpus(200);
        state.in_buf[..data.len()].copy_from_slice(&data);
        state.have = data.len();

        gz_comp(&mut state, FlushMode::Finish).expect("flush succeeds");
        assert_eq!(state.have, 0, "every buffered byte was ingested");
        assert_eq!(state.out_pending, 0, "every produced byte was delivered");

        let bytes = read_output(&path);
        assert_eq!(gunzip(&bytes).as_slice(), data.as_slice());
    }

    /// A partial write must advance the pending-window cursor, not slide the
    /// remainder back to `out_buf[0]`.
    ///
    /// Compaction is what turns a run of short writes into quadratic copying
    /// (CWE-400): delivering an `N`-byte window one byte at a time would move
    /// `(N-1) + (N-2) + … + 1` bytes, where C's `state->x.next += writ`
    /// (`gzwrite.c` L124) moves none. The discriminating observation is the buffer
    /// itself — after the cursor advances, every byte of the window is still at the
    /// index `deflate` wrote it to.
    #[test]
    fn a_short_write_advances_the_cursor_instead_of_recopying_the_window() {
        let path = TempFile::new("cursor");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");

        // Stage a recognisable pending window by hand, exactly as a `deflate` call
        // that produced 16 bytes would leave it.
        let window: Vec<u8> = (0..16u8).collect();
        state.out_buf[..window.len()].copy_from_slice(&window);
        state.out_pending = window.len();
        assert_eq!(state.out_start, 0, "a fresh window is front-anchored");

        // The destination takes five bytes, then stalls.
        {
            let _fault = fault::script(&[fault::Step::Accept(5), fault::Step::WouldBlock]);
            assert!(
                drain_pending(&mut state).is_err(),
                "the scripted stall is reported"
            );
            assert_eq!(fault::remaining(), 0, "both scripted steps were exercised");
        }
        assert!(state.again, "a WouldBlock stall is retryable");
        assert_eq!(state.err, ReturnCode::ErrNo);
        assert_eq!(
            state.out_start, 5,
            "the cursor advanced past what the OS took"
        );
        assert_eq!(
            state.out_pending, 11,
            "the untaken remainder is still pending"
        );
        assert_eq!(
            &state.out_buf[..window.len()],
            window.as_slice(),
            "advancing a cursor must not move a single byte of the pending window"
        );

        // Retry against a destination that behaves: the remainder is delivered and
        // the cursor ends at the produced-bytes frontier. Reclaiming the area is
        // not the drain's job — C does that in the compress loop, and only once the
        // area is both full and fully delivered (`gzwrite.c` L125-L128).
        state.clear_error();
        drain_pending(&mut state).expect("the retry drains the remainder");
        assert_eq!(state.out_pending, 0, "the window is empty");
        assert_eq!(
            state.out_start,
            window.len(),
            "and the cursor sits at the frontier, exactly where C leaves `next_out`"
        );
        assert_eq!(
            read_output(&path),
            window,
            "every byte arrived exactly once, in order"
        );
    }

    /// A destination that accepts one byte per write must produce **byte-identical**
    /// output to one that accepts everything.
    ///
    /// This is the guarantee that makes the cursor safe to introduce at all: a
    /// drain moves the cursor and never the bytes, and the scratch area is
    /// reclaimed only once it is both full and fully delivered (`gzwrite.c`
    /// L125-L128). `deflate` therefore always receives the free tail
    /// `out_buf[frontier..size]`, whose extent is a function of the bytes already
    /// produced — never of how the destination chose to chunk its acceptance.
    #[test]
    fn short_writes_do_not_change_a_single_output_byte() {
        let data = corpus(4096);

        // Baseline: an ordinary destination that accepts every write in full.
        let plain = TempFile::new("dribble_baseline");
        {
            let mut state = new_write_state(&plain, 64, 6, 0, 0);
            assert_eq!(gz_write(&mut state, &data), data.len());
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let expected = read_output(&plain);

        // Same input, same settings, but one byte accepted per write.
        let dribble = TempFile::new("dribble");
        {
            let mut state = new_write_state(&dribble, 64, 6, 0, 0);
            let _fault = fault::cap_every_write(1);
            assert_eq!(
                gz_write(&mut state, &data),
                data.len(),
                "a dribbling destination still accepts the whole input"
            );
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
            assert_eq!(state.out_pending, 0, "nothing was left undelivered");
            assert!(
                state.out_start < state.size,
                "the compress loop never leaves the scratch area full: whenever the \
                 frontier reaches the end it drains and reclaims (`gzwrite.c` \
                 L125-L128) before asking `deflate` for more"
            );
        }
        let actual = read_output(&dribble);

        assert_eq!(
            actual, expected,
            "the compressed bytes must not depend on how the destination chunked \
             its writes"
        );
        assert_eq!(gunzip(&actual), data, "and the member still round-trips");
    }

    // ---------------------------------------------------------------------
    // Write-outcome branch coverage.
    //
    // An operating-system write reports five outcomes, and both output loops must
    // handle each one correctly: accept-everything, accept-a-prefix (`Ok(n)`),
    // accept-nothing (`Ok(0)`), `EINTR` ([`io::ErrorKind::Interrupted`]), and
    // `EAGAIN` ([`io::ErrorKind::WouldBlock`]). Only the first is exercised by an
    // ordinary file, so each of the others is driven through the [`fault`] seam
    // against the **production** loops themselves — `drain_pending` and
    // `write_direct` — rather than against a copy of them, so a divergence between
    // the tested code and the shipped code is impossible.
    //
    // The first three share one arm, because C shares it: `+= writ` advances by
    // whatever was accepted and the enclosing `while` re-tests, so `Ok(0)` is a
    // retry in place rather than a case of its own. The retry-in-place tests
    // therefore assert the *absence* of cursor movement across a refusal, which is
    // what a re-added special case would break.
    //
    // The retry flag and the pending-window bookkeeping are asserted alongside
    // the delivered bytes, because a loop can deliver the right bytes while still
    // leaving a caller unable to resume.
    // ---------------------------------------------------------------------

    /// Stages `window` as a hand-built pending output window on `state`, exactly
    /// as a `deflate` call that produced those bytes would leave it.
    ///
    /// Asserts that the state is a *fresh* one whose window has never been written
    /// through (`out_pending == 0` and `out_start == 0`), so a test that stages
    /// onto a partly delivered window fails here rather than misattributing the
    /// cause later. That pairing is deliberately **not** claimed as a loop
    /// invariant: after a completed drain the cursor legitimately sits at the
    /// produced-bytes frontier (see [`gz_deflate_loop`]).
    fn stage_pending_window(state: &mut GzState, window: &[u8]) {
        assert_eq!(state.out_pending, 0, "the window must start empty");
        assert_eq!(state.out_start, 0, "and therefore front-anchored");
        assert!(
            window.len() <= state.out_buf.len(),
            "the staged window must fit the scratch area"
        );
        state.out_buf[..window.len()].copy_from_slice(window);
        state.out_pending = window.len();
    }

    /// Drain branch 1 of 3 — `Interrupted`: the write is retried, and the retry
    /// resubmits only what the OS did not take.
    ///
    /// `EINTR` means no byte moved, so the correct response is to reissue the same
    /// window unchanged. Treating it as an error would abort a perfectly healthy
    /// stream on any signal; advancing the cursor for it would drop bytes.
    #[test]
    fn drain_pending_retries_an_interrupted_write_without_duplicating_bytes() {
        let path = TempFile::new("drain_eintr");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");

        let window: Vec<u8> = (0..16u8).collect();
        stage_pending_window(&mut state, &window);
        // Deliberately stale: the first attempt must clear it.
        state.again = true;

        {
            let _fault = fault::script(&[
                fault::Step::Interrupted,
                fault::Step::Accept(3),
                fault::Step::Interrupted,
                fault::Step::Accept(usize::MAX),
            ]);
            drain_pending(&mut state).expect("an interrupted write is retried, never reported");
            assert_eq!(fault::remaining(), 0, "every scripted step was exercised");
        }

        assert_eq!(state.out_pending, 0, "every pending byte was delivered");
        assert_eq!(
            state.out_start,
            window.len(),
            "and the cursor sits at the frontier — a drain moves the cursor, never \
             the bytes"
        );
        assert!(!state.again, "a completed drain leaves no retry pending");
        assert_eq!(state.err, ReturnCode::Ok, "an EINTR retry is not an error");
        assert_eq!(
            read_output(&path),
            window,
            "the retries must not duplicate, drop, or reorder a byte"
        );
    }

    /// Drain branch 2 of 3 — `WouldBlock`: every undelivered byte stays
    /// addressable through the cursor, and `again` is set so the caller may retry.
    ///
    /// This is the transition the pending window exists for. The companion test
    /// [`a_short_write_advances_the_cursor_instead_of_recopying_the_window`]
    /// asserts the cursor *arithmetic*; this one asserts the *bytes* — that
    /// `out_buf[out_start..out_start + out_pending]` names exactly the tail the OS
    /// declined, and that the destination received exactly the accepted prefix and
    /// nothing more.
    #[test]
    fn drain_pending_stall_keeps_the_undelivered_tail_addressable() {
        let path = TempFile::new("drain_eagain");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");

        let window: Vec<u8> = (0..16u8).collect();
        stage_pending_window(&mut state, &window);

        {
            let _fault = fault::script(&[fault::Step::Accept(3), fault::Step::WouldBlock]);
            assert_eq!(
                drain_pending(&mut state),
                Err(ZlibError::ErrNo),
                "a non-blocking stall is reported to the caller"
            );
            assert_eq!(fault::remaining(), 0, "both scripted steps were exercised");
        }

        assert!(state.again, "a stall is retryable, so `again` must be set");
        assert_eq!(state.err, ReturnCode::ErrNo, "a stall reports Z_ERRNO");
        assert_eq!(
            state.out_pending, 13,
            "the undelivered tail is still pending"
        );
        let start = state.out_start;
        assert_eq!(
            &state.out_buf[start..start + state.out_pending],
            &window[3..],
            "the cursor names exactly the bytes the destination declined"
        );
        assert_eq!(
            read_output(&path),
            window[..3].to_vec(),
            "only the accepted prefix reached the destination"
        );

        // The destination recovers: the retained tail is delivered once, in order.
        state.clear_error();
        drain_pending(&mut state).expect("the resumed drain succeeds");
        assert_eq!(state.out_pending, 0, "everything pending was delivered");
        assert_eq!(
            read_output(&path),
            window,
            "the retained bytes were delivered exactly once"
        );
    }

    /// Drain branch 3 of 3 — `Ok(0)`: the request is re-issued unchanged, because
    /// C's `state->x.next += writ` advances by zero and its
    /// `while (strm->next_out > state->x.next)` simply tests again
    /// (`gzwrite.c` L119-L124).
    ///
    /// The script is `Accept(2)`, `Refuse`, `WouldBlock`: three steps, so
    /// `remaining() == 0` afterwards is itself the proof that the loop performed a
    /// *third* write — it retried the refusal rather than reporting it. The stall
    /// that follows is only there to freeze the cursor where the refusal left it so
    /// the assertions can read it: `out_pending` is still 14 and the window still
    /// names `window[2..]`, so the refusal neither advanced the cursor (which would
    /// drop bytes) nor rewound it (which would resubmit them). The resumed drain
    /// then proves exactly-once delivery.
    #[test]
    fn drain_pending_retries_a_destination_that_accepts_nothing_in_place() {
        let path = TempFile::new("drain_zero");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");

        let window: Vec<u8> = (0..16u8).collect();
        stage_pending_window(&mut state, &window);

        {
            let _fault = fault::script(&[
                fault::Step::Accept(2),
                fault::Step::Refuse,
                fault::Step::WouldBlock,
            ]);
            assert_eq!(
                drain_pending(&mut state),
                Err(ZlibError::ErrNo),
                "the stall after the refusal is what is reported"
            );
            assert_eq!(
                fault::remaining(),
                0,
                "all three steps ran, so the refusal was retried rather than reported"
            );
        }

        assert!(state.again, "a stall is retryable, so `again` must be set");
        assert_eq!(state.err, ReturnCode::ErrNo, "a stall reports Z_ERRNO");
        assert_eq!(
            state.out_pending, 14,
            "the refusal advanced the cursor by zero, exactly as C's `+= writ` does"
        );
        let start = state.out_start;
        assert_eq!(
            &state.out_buf[start..start + state.out_pending],
            &window[2..],
            "so the window still names the undelivered tail, unrewound"
        );
        assert_eq!(
            read_output(&path),
            window[..2].to_vec(),
            "and a refused write moved no byte at all"
        );

        state.clear_error();
        drain_pending(&mut state).expect("the resumed drain succeeds");
        assert_eq!(state.out_pending, 0, "everything pending was delivered");
        assert_eq!(
            state.out_start,
            window.len(),
            "and the cursor sits at the frontier — a drain moves the cursor, never \
             the bytes"
        );
        assert_eq!(
            read_output(&path),
            window,
            "the retained bytes were delivered once, in order"
        );
    }

    /// A refusal in the middle of a drain is invisible to the caller: the drain
    /// still completes, reports success, and leaves no retry pending.
    ///
    /// This is the companion of
    /// [`drain_pending_retries_a_destination_that_accepts_nothing_in_place`] with
    /// nothing to freeze the cursor, so it asserts the end-to-end consequence
    /// rather than the intermediate state — a C caller draining through a
    /// destination that momentarily takes nothing observes `Z_OK`, not `Z_ERRNO`.
    #[test]
    fn a_refusal_inside_a_drain_is_invisible_to_the_caller() {
        let path = TempFile::new("drain_zero_transparent");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");

        let window: Vec<u8> = (0..16u8).collect();
        stage_pending_window(&mut state, &window);
        // Deliberately stale: the attempts that follow must clear it.
        state.again = true;

        {
            let _fault = fault::script(&[
                fault::Step::Accept(2),
                fault::Step::Refuse,
                fault::Step::Refuse,
                fault::Step::Accept(usize::MAX),
            ]);
            drain_pending(&mut state)
                .expect("a destination that takes nothing is retried, never reported");
            assert_eq!(fault::remaining(), 0, "every scripted step was exercised");
        }

        assert!(!state.again, "a completed drain leaves no retry pending");
        assert_eq!(
            state.err,
            ReturnCode::Ok,
            "and a retried refusal is not an error"
        );
        assert_eq!(state.out_pending, 0, "every pending byte was delivered");
        assert_eq!(
            read_output(&path),
            window,
            "the retries must not duplicate, drop, or reorder a byte"
        );
    }

    /// Transparent branch 1 of 3 — `Interrupted`: the copy is retried and every
    /// byte of the caller's slice reaches the file exactly once.
    #[test]
    fn write_direct_retries_an_interrupted_write_without_duplicating_bytes() {
        let path = TempFile::new("direct_eintr");
        let mut state = new_write_state(&path, 64, 6, 0, 1);
        let input = b"transparent-copy";
        // Deliberately stale: the first attempt must clear it.
        state.again = true;

        let outcome = {
            let _fault = fault::script(&[
                fault::Step::Accept(4),
                fault::Step::Interrupted,
                fault::Step::Accept(6),
                fault::Step::Accept(usize::MAX),
            ]);
            let outcome = write_direct(&mut state, input);
            assert_eq!(fault::remaining(), 0, "every scripted step was exercised");
            outcome
        };

        assert!(outcome.result.is_ok(), "an interrupted write is retried");
        assert_eq!(
            outcome.consumed,
            input.len(),
            "the full length is reported as written"
        );
        assert!(!state.again, "a completed copy leaves no retry pending");
        assert_eq!(state.err, ReturnCode::Ok, "an EINTR retry is not an error");
        assert_eq!(
            read_output(&path),
            input.to_vec(),
            "the retry must not duplicate, drop, or reorder a byte"
        );
    }

    /// Transparent branch 2 of 3 — `WouldBlock`: the true progress count is
    /// reported and `again` is set, so a retry resumes instead of duplicating.
    ///
    /// Under-reporting here is precisely what would make a caller resubmit bytes the
    /// file already holds, so the count matters as much as the error does.
    #[test]
    fn write_direct_stall_reports_true_progress_and_sets_again() {
        let path = TempFile::new("direct_eagain");
        let mut state = new_write_state(&path, 64, 6, 0, 1);
        let input = b"transparent-copy";

        let outcome = {
            let _fault = fault::script(&[fault::Step::Accept(4), fault::Step::WouldBlock]);
            let outcome = write_direct(&mut state, input);
            assert_eq!(fault::remaining(), 0, "both scripted steps were exercised");
            outcome
        };

        assert_eq!(
            outcome.result,
            Err(ZlibError::ErrNo),
            "a stalled transparent write reports Z_ERRNO"
        );
        assert!(state.again, "a stall is retryable, so `again` must be set");
        assert_eq!(
            outcome.consumed, 4,
            "progress is reported on the error path too"
        );
        assert_eq!(
            read_output(&path),
            input[..4].to_vec(),
            "exactly the reported prefix reached the destination"
        );
    }

    /// Transparent branch 3 of 3 — `Ok(0)`: the copy re-issues the identical
    /// request, because C's `strm->next_in += writ` advances by zero and its
    /// `while (strm->avail_in)` simply tests again (`gzwrite.c` L76-L90).
    ///
    /// The three-step script proves the retry the same way the drain test does:
    /// `remaining() == 0` can only hold if a third write ran. The trailing stall
    /// freezes `off` where the refusal left it, so `consumed == 5` shows the
    /// refusal neither credited a byte it never wrote nor discarded one it had.
    #[test]
    fn write_direct_retries_a_destination_that_accepts_nothing_in_place() {
        let path = TempFile::new("direct_zero");
        let mut state = new_write_state(&path, 64, 6, 0, 1);
        let input = b"transparent-copy";

        let outcome = {
            let _fault = fault::script(&[
                fault::Step::Accept(5),
                fault::Step::Refuse,
                fault::Step::WouldBlock,
            ]);
            let outcome = write_direct(&mut state, input);
            assert_eq!(
                fault::remaining(),
                0,
                "all three steps ran, so the refusal was retried rather than reported"
            );
            outcome
        };

        assert_eq!(
            outcome.result,
            Err(ZlibError::ErrNo),
            "the stall after the refusal is what is reported"
        );
        assert!(state.again, "a stall is retryable, so `again` must be set");
        assert_eq!(
            outcome.consumed, 5,
            "the refusal advanced the offset by zero, exactly as C's `+= writ` does"
        );
        assert_eq!(
            read_output(&path),
            input[..5].to_vec(),
            "and a refused write moved no byte at all"
        );

        // A resumed copy from the reported offset delivers the rest exactly once.
        state.clear_error();
        let rest = write_direct(&mut state, &input[outcome.consumed..]);
        assert!(rest.result.is_ok(), "the resumed copy succeeds");
        assert_eq!(
            outcome.consumed + rest.consumed,
            input.len(),
            "the two copies cover the input once"
        );
        assert_eq!(
            read_output(&path),
            input.to_vec(),
            "the tail was delivered once, in order"
        );
    }

    /// A refusal in the middle of a transparent copy is likewise invisible: the
    /// copy completes, credits the whole input, and reports success.
    #[test]
    fn a_refusal_inside_a_transparent_copy_is_invisible_to_the_caller() {
        let path = TempFile::new("direct_zero_transparent");
        let mut state = new_write_state(&path, 64, 6, 0, 1);
        let input = b"transparent-copy";
        // Deliberately stale: the attempts that follow must clear it.
        state.again = true;

        let outcome = {
            let _fault = fault::script(&[
                fault::Step::Accept(5),
                fault::Step::Refuse,
                fault::Step::Refuse,
                fault::Step::Accept(usize::MAX),
            ]);
            let outcome = write_direct(&mut state, input);
            assert_eq!(fault::remaining(), 0, "every scripted step was exercised");
            outcome
        };

        assert!(
            outcome.result.is_ok(),
            "a destination that takes nothing is retried, never reported"
        );
        assert_eq!(
            outcome.consumed,
            input.len(),
            "the full length is reported as written"
        );
        assert!(!state.again, "a completed copy leaves no retry pending");
        assert_eq!(
            state.err,
            ReturnCode::Ok,
            "and a retried refusal is not an error"
        );
        assert_eq!(
            read_output(&path),
            input.to_vec(),
            "the retries must not duplicate, drop, or reorder a byte"
        );
    }

    /// The retry flag is per-attempt state, not sticky history: the drain that
    /// stalls sets it, and the drain that recovers clears it.
    ///
    /// This is why both loops assign `again = false` at the top of every attempt
    /// rather than once on entry. A caller consulting the flag after a successful
    /// resume must see "no retry pending"; a flag left set from an earlier stall
    /// would advertise a stall that no longer exists.
    #[test]
    fn a_recovered_stall_clears_the_retry_flag() {
        let path = TempFile::new("flag_drain");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");

        let window: Vec<u8> = (0..16u8).collect();
        stage_pending_window(&mut state, &window);

        {
            let _fault = fault::script(&[fault::Step::Accept(3), fault::Step::WouldBlock]);
            drain_pending(&mut state).expect_err("the first drain stalls");
        }
        assert!(state.again, "the stall set the retry flag");

        // Same state, same window: the recovery must reset the flag.
        state.clear_error();
        drain_pending(&mut state).expect("the resumed drain succeeds");
        assert!(!state.again, "the recovery cleared the retry flag");
        assert_eq!(
            state.out_pending, 0,
            "and delivered everything still pending"
        );

        // The transparent path shares the contract, so it shares the check.
        let direct = TempFile::new("flag_direct");
        let mut state = new_write_state(&direct, 64, 6, 0, 1);
        let input = b"transparent-copy";

        let stalled = {
            let _fault = fault::script(&[fault::Step::Accept(4), fault::Step::WouldBlock]);
            write_direct(&mut state, input)
        };
        assert!(stalled.result.is_err(), "the first copy stalls");
        assert!(state.again, "the stall set the retry flag");

        state.clear_error();
        let rest = write_direct(&mut state, &input[stalled.consumed..]);
        assert!(rest.result.is_ok(), "the resumed copy succeeds");
        assert!(!state.again, "the recovery cleared the retry flag");
        assert_eq!(
            stalled.consumed + rest.consumed,
            input.len(),
            "and covered the input once"
        );
    }

    /// Nothing to move means no attempt at all: the destination is never touched
    /// and the retry flag keeps describing the attempt that last ran.
    ///
    /// Both loops clear the flag at the top of each *attempt*, not once on entry,
    /// so a zero-length drain or copy is inert. That is what stops an incidental
    /// no-op call — `gz_deflate_loop` drains after every `deflate`, most of which
    /// produce nothing — from issuing a pointless zero-byte write or from erasing a
    /// caller's still-valid retry state.
    #[test]
    fn an_empty_transfer_touches_neither_the_destination_nor_the_retry_flag() {
        let path = TempFile::new("empty_transfer");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        gz_init(&mut state).expect("gz_init");
        assert_eq!(state.out_pending, 0, "the window starts empty");
        state.again = true;

        // A script the loops must never reach: consuming it would prove a write
        // was attempted.
        let _fault = fault::script(&[fault::Step::Refuse]);

        drain_pending(&mut state).expect("an empty window drains trivially");
        assert_eq!(fault::remaining(), 1, "no write was attempted");
        assert!(state.again, "no attempt ran, so the flag is unchanged");
        assert_eq!(state.out_start, 0, "and the empty window stays anchored");

        let outcome = write_direct(&mut state, &[]);
        assert!(outcome.result.is_ok(), "an empty copy succeeds trivially");
        assert_eq!(outcome.consumed, 0, "and reports no progress");
        assert_eq!(fault::remaining(), 1, "still no write was attempted");
        assert!(
            state.again,
            "no attempt ran, so the flag is still unchanged"
        );
        assert!(
            read_output(&path).is_empty(),
            "the destination received nothing at all"
        );
    }

    /// A zero-fill that stalls part-way must credit the zeros it already
    /// compressed before it reports the stall.
    ///
    /// C does this explicitly — `n -= strm->avail_in; state->x.pos += n;
    /// state->skip -= n;` runs *before* `if (ret == -1) return -1;`
    /// (`gzwrite.c` L177-L181). Propagating first (what `?` does) would leave
    /// `skip` at its full value, so the retry would regenerate every zero the
    /// engine had already taken: more zeros than the seek asked for, different
    /// output bytes, and the work done twice (CWE-252, CWE-400).
    ///
    /// # Reaching a stall inside a `Z_NO_FLUSH` fill
    ///
    /// `gz_zero` only ever asks for `Z_NO_FLUSH`, so the destination is touched
    /// exactly when the scratch area fills (`gzwrite.c` L112-L114) — never merely
    /// because a `deflate` call produced something. Zeros produce nothing at all
    /// until a block boundary, so the fill has to run long enough to reach one.
    /// `Z_HUFFMAN_ONLY` makes that affordable: with string matching disabled every
    /// input byte becomes one literal symbol, so the symbol buffer fills after
    /// ~16 K zeros rather than the ~4 MB that level-6 matching would need, and the
    /// block emitted there overflows the 64-byte area many times over.
    #[test]
    fn a_stalled_zero_fill_credits_the_zeros_it_already_compressed() {
        let path = TempFile::new("zerostall");
        let mut state = new_write_state(&path, 64, 6, crate::constants::Z_HUFFMAN_ONLY, 0);
        gz_init(&mut state).expect("gz_init");
        let total: i64 = 20_000;
        state.skip = total;

        // Zeros accumulate as literal symbols until the symbol buffer fills; the
        // block emitted there overflows the 64-byte scratch area, and the drain
        // that overflow triggers is what stalls.
        {
            let _fault = fault::script(&[fault::Step::WouldBlock]);
            assert!(gz_zero(&mut state).is_err(), "the stall is reported");
            assert_eq!(fault::remaining(), 0, "the scripted stall was reached");
        }
        assert!(state.again, "a WouldBlock stall is retryable");
        assert_eq!(state.err, ReturnCode::ErrNo);
        assert!(
            state.out_pending > 0,
            "the output the destination declined is still pending"
        );

        let credited = state.pos;
        assert!(
            credited > 0,
            "the zeros deflate ingested before the stall must be credited"
        );
        assert_eq!(
            state.skip,
            total - credited,
            "the gap shrank by exactly the credited count"
        );
        assert_eq!(
            state.have, 0,
            "deflate took the whole staged chunk, so nothing is left to re-stage"
        );

        // Retry against a destination that behaves. Because the ingested prefix was
        // credited, the fill emits the remaining zeros and no others.
        state.clear_error();
        gz_zero(&mut state).expect("the retried fill completes");
        assert_eq!(state.skip, 0, "the whole gap was realized");
        assert_eq!(state.pos, total, "and counted exactly once");

        assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        assert_eq!(
            gunzip(&read_output(&path)),
            vec![0u8; total as usize],
            "exactly the requested number of zeros, none regenerated"
        );
    }

    /// The transparent (`direct`) zero-fill credits exactly the bytes that reached
    /// the destination, and leaves the untaken remainder of the chunk buffered.
    ///
    /// This is C's behaviour term for term: the direct branch of `gz_comp` leaves
    /// `strm->avail_in` describing what it could not write, `gz_zero` recovers the
    /// difference with `n -= strm->avail_in`, and the retry re-stages that
    /// remainder through the "consume whatever's left in the input buffer" step at
    /// the top of `gz_zero` (`gzwrite.c` L160-L162). The remainder is therefore
    /// re-sent but not re-credited — deliberately identical to reference zlib,
    /// which is what byte-for-byte compatibility requires.
    #[test]
    fn a_stalled_transparent_zero_fill_credits_only_what_reached_the_file() {
        let path = TempFile::new("zerostall_direct");
        let mut state = new_write_state(&path, 64, 6, 0, 1);
        gz_init(&mut state).expect("gz_init");
        let total: i64 = 200;
        state.skip = total;

        {
            let _fault = fault::script(&[fault::Step::Accept(20), fault::Step::WouldBlock]);
            assert!(gz_zero(&mut state).is_err(), "the stall is reported");
            assert_eq!(fault::remaining(), 0, "both scripted steps were exercised");
        }
        assert!(state.again, "a WouldBlock stall is retryable");
        assert_eq!(
            state.pos, 20,
            "exactly the bytes the destination took are credited"
        );
        assert_eq!(
            state.skip,
            total - 20,
            "and exactly those are removed from the gap"
        );
        assert_eq!(
            state.have, 44,
            "the untaken remainder of the staged chunk stays buffered for the retry"
        );
        assert_eq!(
            read_output(&path),
            vec![0u8; 20],
            "only the accepted prefix reached the file"
        );
    }

    #[test]
    fn write_ready_requires_write_mode_and_no_serious_error() {
        let path = TempFile::new("ready");
        let mut state = new_write_state(&path, 8192, 6, 0, 0);
        assert!(write_ready(&state), "fresh write stream is ready");

        state.mode = GzMode::Read;
        assert!(!write_ready(&state), "a read stream is not ready to write");

        state.mode = GzMode::Write;
        state.err = ReturnCode::DataError;
        assert!(!write_ready(&state), "a serious error blocks writing");

        state.again = true;
        assert!(
            write_ready(&state),
            "a pending non-blocking retry is writable"
        );
    }

    #[test]
    fn gz_init_allocates_buffers_and_engine_for_gzip() {
        let path = TempFile::new("init_gzip");
        let mut state = new_write_state(&path, 128, 6, 0, 0);
        gz_init(&mut state).expect("gz_init succeeds");

        // Input buffer is double-sized; output buffer is single-sized.
        assert_eq!(state.in_buf.len(), 256, "input buffer is want << 1");
        assert_eq!(state.out_buf.len(), 128, "output buffer is want");
        assert_eq!(state.size, 128, "size marks the buffers as initialized");
        assert!(state.strm.is_deflate(), "a deflate engine was initialized");
    }

    #[test]
    fn gz_init_direct_skips_output_buffer_and_engine() {
        let path = TempFile::new("init_direct");
        let mut state = new_write_state(&path, 128, 6, 0, 1);
        gz_init(&mut state).expect("gz_init succeeds");

        assert_eq!(
            state.in_buf.len(),
            256,
            "input buffer is still double-sized"
        );
        assert!(state.out_buf.is_empty(), "no output buffer in direct mode");
        assert_eq!(state.size, 128, "size still marks initialization");
        assert!(
            !state.strm.is_deflate(),
            "no deflate engine for a transparent stream"
        );
    }

    /// The **first** `gz_init` allocation — C's `malloc(state->want << 1)` — must
    /// report `Z_MEM_ERROR` / "out of memory" instead of aborting, and must leave
    /// the stream in the uninitialized state so a caller can lower `want` and
    /// retry.
    ///
    /// This is the abort the review found reachable from a legitimate API
    /// sequence: `gzbuffer` accepts any size up to `UINT_MAX >> 1` (exactly as C
    /// does — its only size check is the doubling-overflow test), so
    /// `gzbuffer(file, 0x7fff_ffff)` followed by a write asks for ~4 GiB here.
    ///
    /// `want = usize::MAX / 2` makes `want << 1` equal `usize::MAX - 1`, which
    /// exceeds the `isize::MAX` ceiling on any allocation and is therefore refused
    /// by arithmetic — deterministic on 32-bit and 64-bit, instant, and with no
    /// memory ever requested.
    #[test]
    fn gz_init_reports_mem_error_when_the_input_buffer_cannot_be_allocated() {
        let path = TempFile::new("init_oom_in");
        let mut state = new_write_state(&path, usize::MAX / 2, 6, 0, 0);

        assert_eq!(
            gz_init(&mut state),
            Err(ZlibError::MemError),
            "an unallocatable input buffer is C's `malloc` returning NULL"
        );
        assert_eq!(state.err, ReturnCode::MemError, "gzerror must report OOM");
        // `GzState::error` deliberately allocates nothing for `MemError` — a
        // detail string would need the very allocator that just failed — so the
        // "out of memory" text comes from `gzerror`'s static branch, which is also
        // where C's `gz_error` sources it (`gzlib.c`: `if (err == Z_MEM_ERROR) {
        // state->msg = (char *)"out of memory"; return; }`).
        assert!(
            state.msg.is_none(),
            "reporting OOM must not itself allocate a message"
        );
        assert_eq!(
            crate::gz::gzerror(&state, None),
            "out of memory",
            "C reports this exact message (gzwrite.c L17 via gz_error)"
        );
        assert_eq!(state.size, 0, "the stream must stay uninitialized");
        assert!(state.in_buf.is_empty(), "no input buffer was retained");
        assert!(state.out_buf.is_empty(), "no output buffer was allocated");
        assert!(
            !state.strm.is_deflate(),
            "the engine is never reached when the first allocation fails"
        );
    }

    /// When the **second** allocation fails — C's `malloc(state->want)` for the
    /// output buffer — C frees the input buffer first (`free(state->in);`,
    /// `gzwrite.c` L25) and only then reports. The port must release it too, or a
    /// stream that can never initialize would hold `2 * want` bytes for the rest
    /// of its life on every rejected write.
    ///
    /// `want = usize::MAX / 2 + 2` reaches that path deterministically and, just
    /// as importantly, makes the rollback *observable*. That value is
    /// `2^(BITS-1) + 1`, so:
    ///
    /// * `want << 1` truncates to `2` — exactly as C's unsigned shift does — so the
    ///   input buffer really is allocated, with a non-zero length; and
    /// * `want` itself exceeds the `isize::MAX` allocation ceiling, so the output
    ///   buffer is refused by arithmetic, with no memory ever requested.
    ///
    /// The non-zero input length is what gives the test its teeth: with
    /// `want << 1` wrapping to `0` instead, `in_buf` would be empty either way and
    /// the rollback assertion below would hold vacuously — it would pass against a
    /// build with the rollback deleted.
    #[test]
    fn gz_init_rolls_back_the_input_buffer_when_the_output_buffer_cannot_be_allocated() {
        let path = TempFile::new("init_oom_out");
        let want = usize::MAX / 2 + 2;
        // Pin the arithmetic the test depends on, so a future edit cannot quietly
        // make this case vacuous again.
        assert_eq!(
            want << 1,
            2,
            "the input buffer must be allocated, non-empty"
        );

        let mut state = new_write_state(&path, want, 6, 0, 0);

        assert_eq!(
            gz_init(&mut state),
            Err(ZlibError::MemError),
            "an unallocatable output buffer is C's second `malloc` returning NULL"
        );
        assert_eq!(state.err, ReturnCode::MemError);
        assert!(state.msg.is_none(), "reporting OOM must not allocate");
        assert_eq!(crate::gz::gzerror(&state, None), "out of memory");
        assert_eq!(state.size, 0, "the stream must stay uninitialized");
        assert!(
            state.in_buf.is_empty(),
            "the input buffer must be released, mirroring C's `free(state->in)`"
        );
        assert!(state.out_buf.is_empty(), "nothing was retained for output");
        assert!(!state.strm.is_deflate(), "the engine is never initialized");
    }

    /// A stream whose buffers could not be allocated must remain *usable*: the
    /// write reports failure through the documented channels and a caller that
    /// lowers `want` can initialize successfully afterwards. This is what makes
    /// the deferred-allocation design (C parity for `gzbuffer`) recoverable rather
    /// than merely non-fatal.
    #[test]
    fn a_stream_that_failed_to_initialize_can_be_retried_with_a_smaller_want() {
        let path = TempFile::new("init_oom_retry");
        let mut state = new_write_state(&path, usize::MAX / 2, 6, 0, 0);

        assert_eq!(gz_init(&mut state), Err(ZlibError::MemError));
        assert_eq!(state.err, ReturnCode::MemError);

        // The recovery a real caller performs: shrink the request and clear the
        // recorded error, exactly as `gzbuffer` + `gzclearerr` would.
        state.want = 128;
        state.clear_error();
        gz_init(&mut state).expect("a satisfiable request initializes normally");

        assert_eq!(state.in_buf.len(), 256, "input buffer is want << 1");
        assert_eq!(state.out_buf.len(), 128, "output buffer is want");
        assert_eq!(state.size, 128, "size marks the buffers as initialized");
        assert!(state.strm.is_deflate(), "the engine came up on the retry");
    }

    #[test]
    fn gz_init_rejects_out_of_range_strategy_like_c() {
        // C `gzsetparams` records `strategy` unvalidated (`gzwrite.c` L630-L663)
        // and `gz_init` forwards it to `deflateInit2`, which rejects
        // `strategy < 0 || strategy > Z_FIXED` (`deflate.c` L436); `gz_init`
        // reports that as `Z_MEM_ERROR` / "out of memory" (C L37-L43) and
        // returns -1. The value must never be silently replaced by a default.
        for bad in [5, 6, -1, i32::MIN, i32::MAX] {
            let path = TempFile::new("badstrategy");
            let mut state = new_write_state(&path, 128, 6, bad, 0);

            assert_eq!(
                gz_init(&mut state),
                Err(ZlibError::MemError),
                "strategy {bad} is out of range and must fail the deferred init"
            );
            assert_eq!(
                state.err,
                ReturnCode::MemError,
                "the failure is recorded on the state for gzerror/gzclose_w"
            );
            let mut code = 0;
            assert_eq!(
                crate::gz::open::gzerror(&state, Some(&mut code)),
                "out of memory",
                "C reports every gz_init failure as out of memory"
            );
            assert_eq!(
                code,
                ReturnCode::MemError.as_c_int(),
                "gzerror reports the numeric code a C caller sees"
            );
            assert_eq!(
                state.size, 0,
                "a failed init leaves the buffers unmarked, exactly as C does"
            );
            assert!(
                !state.strm.is_deflate(),
                "no engine is installed by a failed init"
            );
        }
    }

    #[test]
    fn gz_init_accepts_every_valid_strategy() {
        // The positive control for `gz_init_rejects_out_of_range_strategy_like_c`:
        // all five valid strategies (`Z_DEFAULT_STRATEGY`..=`Z_FIXED`) still
        // initialize the engine.
        for good in 0..=4 {
            let path = TempFile::new("goodstrategy");
            let mut state = new_write_state(&path, 128, 6, good, 0);

            gz_init(&mut state).expect("a valid strategy initializes the engine");
            assert_eq!(state.size, 128, "initialization completed");
            assert!(state.strm.is_deflate(), "a deflate engine was installed");
            assert_eq!(state.err, ReturnCode::Ok, "no error was recorded");
        }
    }

    /// A failed `gz_init` must not retain the buffers it had already allocated.
    ///
    /// C frees them on every failure return — `free(state->in)` before the output
    /// allocation's failure, `free(state->out); free(state->in);` before the
    /// `deflateInit2` failure (`gzwrite.c` L19-L44). Keeping them would strand
    /// `3 * want` bytes (the doubled input buffer plus the output buffer) for the
    /// rest of the stream's life on a configuration that can never succeed
    /// (CWE-401), and a caller is free to have asked for a very large `gzbuffer`.
    ///
    /// Both failure points are covered: the strategy conversion, which fails after
    /// *both* buffers exist, and `deflate_init2` itself via an out-of-range level.
    /// What a caller observes — the returned error, `state.err`, the `gzerror`
    /// text, and `size` still being `0` — is asserted unchanged by
    /// `gz_init_rejects_out_of_range_strategy_like_c` and its level counterpart.
    #[test]
    fn a_failed_gz_init_releases_the_buffers_it_allocated() {
        // `want` is deliberately large so a leak would be unmistakable: 64 KiB of
        // output buffer plus 128 KiB of input buffer per failed attempt.
        const WANT: usize = 1 << 16;

        for (label, level, strategy) in [
            ("out-of-range strategy", 6, 5),
            ("out-of-range level", 10, 0),
        ] {
            let path = TempFile::new("init_rollback");
            let mut state = new_write_state(&path, WANT, level, strategy, 0);

            assert_eq!(
                gz_init(&mut state),
                Err(ZlibError::MemError),
                "{label} must fail the deferred init"
            );
            assert_eq!(state.err, ReturnCode::MemError, "{label}: code preserved");
            assert_eq!(
                state.size, 0,
                "{label}: the stream is still marked uninitialized, exactly as in C"
            );
            assert!(
                state.in_buf.is_empty() && state.in_buf.capacity() == 0,
                "{label}: the input buffer was released (C `free(state->in)`)"
            );
            assert!(
                state.out_buf.is_empty() && state.out_buf.capacity() == 0,
                "{label}: the output buffer was released (C `free(state->out)`)"
            );
            assert!(
                !state.strm.is_deflate(),
                "{label}: no engine state is left installed"
            );

            // The rollback must leave the state genuinely re-initializable, the way
            // a C caller who fixes the parameter and writes again would expect.
            state.strategy = 0;
            state.level = 6;
            state.clear_error();
            gz_init(&mut state).expect("a corrected configuration initializes");
            assert_eq!(state.size, WANT, "{label}: the retry allocated cleanly");
            assert_eq!(state.in_buf.len(), WANT << 1);
            assert_eq!(state.out_buf.len(), WANT);
        }
    }

    #[test]
    fn write_with_out_of_range_strategy_reports_the_deferred_failure() {
        // The end-to-end shape a caller observes after `gzsetparams(level, 5)`
        // on a writer that has not performed any I/O yet: the first write fails
        // (returns 0), and the error is retrievable afterwards. Compare the
        // out-of-range *level* case below — both parameters behave identically.
        let path = TempFile::new("badstrategy_write");
        let mut state = new_write_state(&path, 128, 6, 5, 0);

        assert_eq!(
            gz_write(&mut state, b"payload written with an invalid strategy"),
            0,
            "the write makes no progress once the deferred init fails"
        );
        assert_eq!(state.err, ReturnCode::MemError, "the error is observable");
        let mut code = 0;
        assert_eq!(
            crate::gz::open::gzerror(&state, Some(&mut code)),
            "out of memory",
            "the failure is retrievable through gzerror"
        );
        assert_eq!(code, ReturnCode::MemError.as_c_int());
    }

    #[test]
    fn write_with_out_of_range_level_reports_the_deferred_failure() {
        // The symmetry proof: an out-of-range level is rejected by
        // `deflate_init2` itself, and reaches the caller through exactly the
        // same `gz_init` failure path as an out-of-range strategy.
        let path = TempFile::new("badlevel_write");
        let mut state = new_write_state(&path, 128, 10, 0, 0);

        assert_eq!(
            gz_write(&mut state, b"payload written with an invalid level"),
            0,
            "the write makes no progress once the deferred init fails"
        );
        assert_eq!(state.err, ReturnCode::MemError, "the error is observable");
        let mut code = 0;
        assert_eq!(
            crate::gz::open::gzerror(&state, Some(&mut code)),
            "out of memory",
            "the failure is retrievable through gzerror"
        );
        assert_eq!(code, ReturnCode::MemError.as_c_int());
    }

    #[test]
    fn gz_write_empty_is_noop() {
        let path = TempFile::new("empty");
        let mut state = new_write_state(&path, 8192, 6, 0, 0);
        assert_eq!(gz_write(&mut state, &[]), 0, "empty write consumes nothing");
        assert_eq!(state.size, 0, "an empty write does not trigger gz_init");
    }

    /// [`BoundedWriter`] accepts fragments while they fit and refuses the first
    /// one that does not, leaving the accepted prefix intact.
    ///
    /// This is the property that makes [`gzvprintf`] allocation-free and bounded
    /// (finding M6-06): the sink stands in for C's
    /// `vsnprintf(next, state->size, …)` (`gzwrite.c` L467) and cannot be driven
    /// past the region it was handed, so a caller-controlled format string can
    /// neither reallocate nor overrun.
    #[test]
    fn bounded_writer_stops_at_its_capacity_without_growing() {
        use core::fmt::Write as _;

        let mut region = [0u8; 5];
        {
            let mut sink = BoundedWriter::new(&mut region);
            assert!(
                sink.write_str("abc").is_ok(),
                "a fragment that fits is taken"
            );
            assert_eq!(sink.len, 3);
            assert!(!sink.overflowed, "nothing has been refused yet");
            assert!(
                sink.write_str("xyz").is_err(),
                "a fragment that does not fit is refused, ending core::fmt::write"
            );
            assert!(sink.overflowed, "the refusal is recorded");
            assert_eq!(sink.len, 3, "a refused fragment is not partially copied");
            assert!(
                sink.write_str("de").is_ok(),
                "the two bytes that do fit are still accepted"
            );
            assert_eq!(sink.len, 5, "the sink is now exactly full");
        }
        assert_eq!(
            &region, b"abcde",
            "only the accepted fragments were written"
        );
    }

    /// `gzprintf` reproduces C's effective capacity of `size - 1` and reports
    /// `0` — never a truncated write, never an allocation — when the formatted
    /// result does not fit (finding M6-06).
    ///
    /// The expected values are the ones a reference C zlib produces with
    /// `gzbuffer(file, 64)`, measured through the C ABI: `"hello"` → 5, a
    /// 63-character result → 63, a 64-character result → 0, a 200-character
    /// result → 0, and a single character → 1 (`gzwrite.c` L471-L473, where the
    /// reserved sentinel byte at `next[state->size - 1]` is what makes the
    /// greatest acceptable length `size - 1`).
    #[test]
    fn gzprintf_matches_c_effective_capacity_of_size_minus_one() {
        let path = TempFile::new("printf_cap");
        let s63 = "a".repeat(63);
        {
            let mut state = new_write_state(&path, 64, 6, 0, 0);

            assert_eq!(gzprintf(&mut state, format_args!("hello")), 5);
            let allocated = state.in_buf.len();
            assert_eq!(allocated, 128, "the input buffer is double-sized");

            assert_eq!(
                gzprintf(&mut state, format_args!("{s63}")),
                63,
                "63 bytes is the longest result C accepts for size = 64"
            );
            let s64 = "b".repeat(64);
            assert_eq!(
                gzprintf(&mut state, format_args!("{s64}")),
                0,
                "a result that reaches `size` is rejected outright"
            );
            let s200 = "c".repeat(200);
            assert_eq!(
                gzprintf(&mut state, format_args!("{s200}")),
                0,
                "a far longer result is rejected without growing anything"
            );
            assert_eq!(gzprintf(&mut state, format_args!("z")), 1);

            assert_eq!(
                state.in_buf.len(),
                allocated,
                "a rejected result never reallocated the preallocated buffer"
            );
            assert_eq!(
                state.err,
                ReturnCode::Ok,
                "an over-long result is reported as 0, not as an error"
            );
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        assert_eq!(
            gunzip(&read_output(&path)),
            format!("hello{s63}z").into_bytes(),
            "exactly the accepted results reached the member, in order"
        );
    }

    /// The formatted text is rendered *into* the preallocated gzip input buffer
    /// rather than into a temporary allocation, so it is already staged for
    /// compression the moment formatting ends (finding M6-06; C L450-L453
    /// formats at `state->in + avail_in`).
    #[test]
    fn gzprintf_renders_directly_into_the_preallocated_input_buffer() {
        let path = TempFile::new("printf_inplace");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        assert_eq!(gzprintf(&mut state, format_args!("{}+{}={}", 2, 3, 5)), 5);
        assert_eq!(state.have, 5, "the bytes are counted as buffered input");
        assert_eq!(
            &state.in_buf[..5],
            b"2+3=5".as_slice(),
            "and they are sitting in `in_buf` itself, not in a temporary"
        );
        assert_eq!(
            state.pos, 5,
            "the stream position advanced by exactly the formatted length"
        );
    }

    /// A `Display` implementation that itself fails leaves the call reporting
    /// `0`, mirroring C, whose negative `vsnprintf` return also fails the
    /// `(unsigned)len >= state->size` test (C L472). Nothing is accounted for,
    /// so the partial text the failing impl already emitted is ignored exactly
    /// as C ignores whatever `vsnprintf` left in `next`.
    #[test]
    fn gzprintf_reports_zero_when_formatting_itself_fails() {
        struct Fails;
        impl core::fmt::Display for Fails {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("partial")?;
                Err(core::fmt::Error)
            }
        }

        let path = TempFile::new("printf_fail");
        let mut state = new_write_state(&path, 64, 6, 0, 0);
        assert_eq!(gzprintf(&mut state, format_args!("{Fails}")), 0);
        assert_eq!(state.have, 0, "nothing was accounted for");
        assert_eq!(state.pos, 0, "and the stream position did not move");
    }

    #[test]
    fn roundtrip_small_gzwrite() {
        let path = TempFile::new("small");
        let data = b"The quick brown fox jumps over the lazy dog.";
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);
            assert_eq!(gzwrite(&mut state, data), data.len() as i32);
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert!(
            compressed.starts_with(&[0x1f, 0x8b, 0x08]),
            "gzip magic + deflate method are present"
        );
        assert_eq!(gunzip(&compressed), data, "round-trips to the original");
    }

    #[test]
    fn roundtrip_crosses_small_buffer() {
        let path = TempFile::new("large");
        // ~9 KiB of semi-repetitive data, far larger than the tiny buffer, so
        // both the small-write-buffering and large-write-direct-feed paths run.
        let mut data = Vec::new();
        for i in 0..1024u32 {
            data.extend_from_slice(format!("line number {i:05}\n").as_bytes());
        }
        {
            let mut state = new_write_state(&path, 64, 9, 0, 0);
            assert_eq!(gzwrite(&mut state, &data), data.len() as i32);
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert_eq!(gunzip(&compressed), data, "large input round-trips");
    }

    #[test]
    fn roundtrip_mixed_putc_puts_printf() {
        let path = TempFile::new("mixed");
        let mut expected = Vec::new();
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);

            assert_eq!(gzputc(&mut state, b'H' as i32), b'H' as i32);
            expected.push(b'H');

            assert_eq!(gzputs(&mut state, "ello, "), 6);
            expected.extend_from_slice(b"ello, ");

            let n = gzprintf(&mut state, format_args!("{}+{}={}", 2, 3, 5));
            assert_eq!(n, 5, "gzprintf reports the formatted length");
            expected.extend_from_slice(b"2+3=5");

            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert_eq!(gunzip(&compressed), expected, "mixed API round-trips");
    }

    #[test]
    fn gzputc_uses_input_buffer_fast_path() {
        let path = TempFile::new("putc");
        let mut state = new_write_state(&path, 8192, 6, 0, 0);

        // First putc lazily initializes (via gz_write) and buffers one byte.
        assert_eq!(gzputc(&mut state, 0x41), 0x41);
        assert_eq!(state.have, 1);
        assert_eq!(state.in_buf[0], 0x41);

        // Second putc takes the fast path (size != 0, room available).
        assert_eq!(gzputc(&mut state, 0x42), 0x42);
        assert_eq!(state.have, 2);
        assert_eq!(state.in_buf[1], 0x42);
        assert_eq!(state.pos, 2);
    }

    #[test]
    fn direct_mode_writes_transparently() {
        let path = TempFile::new("direct");
        let data = b"raw passthrough, no gzip framing here";
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 1);
            assert_eq!(gzwrite(&mut state, data), data.len() as i32);
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let written = read_output(&path);
        assert_eq!(written, data, "direct mode writes the bytes verbatim");
    }

    #[test]
    fn all_levels_and_strategies_roundtrip() {
        // Data with both structure and noise, exercising every strategy.
        let data: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        for level in [0, 1, 6, 9, -1] {
            for strategy in [0, 1, 2, 3, 4] {
                let path = TempFile::new("lvlstrat");
                {
                    let mut state = new_write_state(&path, 8192, level, strategy, 0);
                    assert_eq!(
                        gzwrite(&mut state, &data),
                        data.len() as i32,
                        "level {level} strategy {strategy} accepts all input"
                    );
                    assert_eq!(
                        finish_write(&mut state),
                        ReturnCode::Ok,
                        "level {level} strategy {strategy} finalizes cleanly"
                    );
                }
                let compressed = read_output(&path);
                assert_eq!(
                    gunzip(&compressed),
                    data,
                    "level {level} strategy {strategy} round-trips"
                );
            }
        }
    }

    #[test]
    fn gzflush_sync_then_continue() {
        let path = TempFile::new("flush");
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);
            assert_eq!(gzwrite(&mut state, b"first-"), 6);
            assert_eq!(
                gzflush(&mut state, Z_SYNC_FLUSH),
                0,
                "sync flush returns Z_OK"
            );
            assert_eq!(gzwrite(&mut state, b"second"), 6);
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert_eq!(
            gunzip(&compressed),
            b"first-second",
            "data spanning a sync flush round-trips"
        );
    }

    #[test]
    fn gzflush_rejects_out_of_range_flush() {
        let path = TempFile::new("flushbad");
        let mut state = new_write_state(&path, 8192, 6, 0, 0);
        assert_eq!(gzflush(&mut state, 99), ReturnCode::StreamError.as_c_int());
        assert_eq!(gzflush(&mut state, -1), ReturnCode::StreamError.as_c_int());
    }

    #[test]
    fn gzfwrite_returns_full_item_count() {
        let path = TempFile::new("fwrite");
        let buf = b"ABCDEFGHIJKL"; // 12 bytes = 3 items of 4
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);
            assert_eq!(gzfwrite(&mut state, buf, 4, 3), 3, "3 full items written");
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert_eq!(gunzip(&compressed), buf, "gzfwrite payload round-trips");
    }

    #[test]
    fn gzfwrite_overflow_is_rejected() {
        let path = TempFile::new("fwrite_ovf");
        let mut state = new_write_state(&path, 8192, 6, 0, 0);
        // size * nitems overflows usize -> rejected with a stream error.
        assert_eq!(gzfwrite(&mut state, b"x", usize::MAX, 2), 0);
        assert_eq!(state.err, ReturnCode::StreamError);
    }

    #[test]
    fn gzwrite_on_read_mode_returns_zero() {
        let path = TempFile::new("notwrite");
        let mut state = new_write_state(&path, 8192, 6, 0, 0);
        state.mode = GzMode::Read;
        assert_eq!(
            gzwrite(&mut state, b"data"),
            0,
            "a read stream rejects writes"
        );
    }

    #[test]
    fn io_write_trait_roundtrip() {
        let path = TempFile::new("iowrite");
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);
            state.write_all(b"hello ").expect("write_all succeeds");
            write!(state, "world {}", 42).expect("write! macro succeeds");
            state.flush().expect("io flush (sync) succeeds");
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert_eq!(
            gunzip(&compressed),
            b"hello world 42",
            "the std::io::Write path round-trips"
        );
    }

    #[test]
    fn finish_on_empty_stream_is_valid_empty_gzip() {
        let path = TempFile::new("emptyfin");
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);
            // No data at all, then finalize.
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert!(
            compressed.starts_with(&[0x1f, 0x8b, 0x08]),
            "an empty member still has a gzip header"
        );
        assert_eq!(gunzip(&compressed), b"", "empty member decodes to nothing");
    }

    #[test]
    fn gz_zero_compresses_skip_zero_bytes() {
        let path = TempFile::new("zero");
        {
            let mut state = new_write_state(&path, 8192, 6, 0, 0);
            gz_init(&mut state).expect("init");
            state.skip = 100;
            gz_zero(&mut state).expect("gz_zero succeeds");
            assert_eq!(state.skip, 0, "the whole gap was consumed");
            assert_eq!(state.pos, 100, "position advanced by the gap size");
            assert_eq!(finish_write(&mut state), ReturnCode::Ok);
        }
        let compressed = read_output(&path);
        assert_eq!(
            gunzip(&compressed),
            vec![0u8; 100],
            "a forward seek materializes as zero bytes"
        );
    }
}
