//! Open, configure, and position operations for the gzip file-I/O layer
//! (`gz*` API) of the `zlib-rs` crate.
//!
//! This module is the safe-Rust port of C **`gzlib.c`** (the direction-agnostic
//! core shared by both the read and write paths), and — following the AAP's
//! function-family reorganisation — it additionally hosts two entry points
//! whose C bodies live elsewhere but which logically belong to the "open /
//! configure" family:
//!
//! * [`gzsetparams`] — the C body is in `gzwrite.c`; it is placed here because
//!   it is a configuration entry point. It drives the shared
//!   [`gz_comp`](crate::gz::write::gz_comp) writer and
//!   [`deflate_params`](crate::deflate::deflate_params).
//! * [`gzdirect`] — the C body is in `gzread.c`; it is placed here because it is
//!   a query entry point. It drives the shared
//!   [`gz_look`](crate::gz::read::gz_look) look-ahead.
//!
//! # Responsibilities
//!
//! * Opening a gzip file by path ([`gzopen`] / [`gzopen64`]) or from an already
//!   open [`File`] ([`gzdopen`]), including the full mode-string grammar.
//! * Buffer sizing ([`gzbuffer`]) and mid-stream parameter changes
//!   ([`gzsetparams`]).
//! * Seeking and telling ([`gzrewind`], [`gzseek`]/[`gzseek64`],
//!   [`gztell`]/[`gztell64`], [`gzoffset`]/[`gzoffset64`]).
//! * End-of-file and transparency queries ([`gzeof`], [`gzdirect`]).
//! * Error accessors ([`gzerror`], [`gzclearerr`]).
//! * The internal helpers [`gz_reset`], [`gz_open`], [`gz_intmax`], and
//!   [`gt_off`].
//!
//! # Idiomatic vs. C boundary
//!
//! The idiomatic openers accept `impl AsRef<Path>` or an owned [`File`]; the
//! raw `int fd` / `*const c_char` handling required by the C `gzopen`/`gzdopen`
//! prototypes lives exclusively in `src/ffi/gz.rs`. Likewise, the C
//! `z_off_t` / `z_off64_t` split is a boundary concern: here every offset is a
//! 64-bit [`i64`], the `*64` functions are the real implementations, and the
//! non-`*64` variants delegate to them (the FFI layer re-materialises the
//! narrower platform `z_off_t` with the overflow-to-`-1` contract).
//!
//! # Safety
//!
//! This module contains **zero `unsafe`**, enforced by the crate-inner
//! `#![deny(unsafe_code)]` below. The cursor into the output buffer is modelled
//! as a `usize` index (see [`GzState`]), so every buffer access elsewhere in the
//! gz layer is bounds-checked slice indexing.

// Compile-enforce the gz-layer unsafe policy: this file must contain zero
// `unsafe`. All raw-fd / raw-pointer / C-string handling for the FFI `gz*`
// entry points lives in `src/ffi/gz.rs`. Mirrors the sibling gz modules.
#![deny(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::Path;

use crate::constants::{
    FlushMode, Strategy, Z_DEFAULT_COMPRESSION, Z_DEFAULT_STRATEGY, Z_FILTERED, Z_FIXED,
    Z_HUFFMAN_ONLY, Z_RLE,
};
use crate::deflate;
use crate::error::ReturnCode;
use crate::gz::GZBUFSIZE;
use crate::gz::read::gz_look;
use crate::gz::state::{GzFile, GzMode, GzState, How};
use crate::gz::write::{gz_comp, gz_zero};

/// The `O_NONBLOCK` open flag, applied on unix when the mode string requests it
/// with `'N'`.
///
/// The Rust standard library does not re-export the platform `O_*` constants
/// and this crate deliberately takes no `libc` dependency, so the value is
/// spelled out per POSIX platform: `O_NONBLOCK` is `0o4000` on Linux/Android
/// and `0o0004` on the BSD-derived unices (including macOS). It is fed to
/// [`std::os::unix::fs::OpenOptionsExt::custom_flags`], which takes a plain
/// [`i32`].
#[cfg(unix)]
const O_NONBLOCK: i32 = if cfg!(any(target_os = "linux", target_os = "android")) {
    0o4000
} else {
    0o0004
};

// ===========================================================================
// Mode-string parsing (gzlib.c L150-197) — pure, no I/O.
// ===========================================================================

/// The result of parsing a `gz*` mode string, factored out of [`gz_open`] so it
/// can be unit-tested without touching the file system.
///
/// The fields correspond to the pieces of state the C `gz_open` derives from
/// the mode string before it opens the descriptor (`gzlib.c` L150-197).
#[derive(Debug)]
struct ParsedMode {
    /// The requested open mode: [`GzMode::Read`], [`GzMode::Write`], or the
    /// transient [`GzMode::Append`] (which [`gz_open`] converts to
    /// [`GzMode::Write`] after seeking to end-of-file).
    mode: GzMode,
    /// The compression level (`0`..=`9`, or [`Z_DEFAULT_COMPRESSION`]).
    level: i32,
    /// The compression strategy (see [`Strategy`]).
    strategy: i32,
    /// The tri-state transparency flag: while reading, `1` = auto-detect and
    /// `-1` = force gzip-only; while writing, `0` = gzip and `1` = transparent.
    direct: i32,
    /// `true` if the `'x'` flag requested an exclusive create (`O_EXCL`).
    exclusive: bool,
    /// `true` if the `'N'` flag requested a non-blocking open (`O_NONBLOCK`).
    #[cfg(unix)]
    nonblock: bool,
}

/// Parses a `gz*` mode string into a [`ParsedMode`], reproducing the C
/// `gz_open` grammar and validation exactly (`gzlib.c` L150-197).
///
/// The recognised flags are:
///
/// * `'0'..='9'` — set the compression level.
/// * `'r'` / `'w'` / `'a'` — open for reading / writing / appending.
/// * `'+'` — **rejected**: reading and writing at once is not supported.
/// * `'b'` — ignored (the stream is always binary).
/// * `'e'` — request close-on-exec (`O_CLOEXEC`). A Rust [`File`] is already
///   close-on-exec by default, so this flag needs no explicit handling.
/// * `'x'` — exclusive create (`O_EXCL`).
/// * `'f'` / `'h'` / `'R'` / `'F'` — strategy filtered / Huffman-only / RLE /
///   fixed.
/// * `'G'` — force gzip-only (`direct = -1`); the last `'G'`/`'T'` wins.
/// * `'T'` — request transparent (`direct = 1`); the last `'G'`/`'T'` wins.
/// * `'N'` — non-blocking open (`O_NONBLOCK`, unix only).
/// * anything else — ignored, exactly as the C code does.
///
/// # Errors
///
/// Returns [`ReturnCode::StreamError`] (the analogue of the C "return `NULL`")
/// when:
///
/// * the string contains `'+'`;
/// * no `'r'`/`'w'`/`'a'` is present;
/// * a transparent read is forced (`'T'` while reading); or
/// * `'G'` is given while writing or appending.
fn parse_mode(mode: &str) -> Result<ParsedMode, ReturnCode> {
    // Defaults mirror the C initialisation (gzlib.c L150-155).
    let mut gz_mode = GzMode::None;
    let mut level = Z_DEFAULT_COMPRESSION;
    let mut strategy = Z_DEFAULT_STRATEGY;
    let mut direct = 0i32;
    let mut exclusive = false;
    #[cfg(unix)]
    let mut nonblock = false;

    // Interpret the mode string (gzlib.c L156-171).
    for ch in mode.chars() {
        match ch {
            c @ '0'..='9' => level = c as i32 - '0' as i32,
            'r' => gz_mode = GzMode::Read,
            'w' => gz_mode = GzMode::Write,
            'a' => gz_mode = GzMode::Append,
            // Can't read and write at the same time (gzlib.c L120).
            '+' => return Err(ReturnCode::StreamError),
            // Binary is implied; nothing to do.
            'b' => {}
            // `O_CLOEXEC`: a Rust `File` is close-on-exec by default, so honour
            // the request implicitly with no extra work.
            'e' => {}
            // `O_EXCL` (exclusive create).
            'x' => exclusive = true,
            'f' => strategy = Z_FILTERED,
            'h' => strategy = Z_HUFFMAN_ONLY,
            'R' => strategy = Z_RLE,
            'F' => strategy = Z_FIXED,
            'G' => direct = -1,
            'T' => direct = 1,
            // `O_NONBLOCK` (unix only); applied when the descriptor is opened.
            'N' => {
                #[cfg(unix)]
                {
                    nonblock = true;
                }
            }
            // Unknown flags are ignored, exactly as the C code does.
            _ => {}
        }
    }

    // Must provide an "r", "w", or "a" (gzlib.c L173-177).
    if gz_mode == GzMode::None {
        return Err(ReturnCode::StreamError);
    }

    // Resolve the tri-state `direct` flag (gzlib.c L179-197). The last `'G'` or
    // `'T'` already won during the parse loop; here we validate the combination
    // against the chosen mode.
    if gz_mode == GzMode::Read {
        if direct == 1 {
            // A `'T'` was given: can't force a transparent read.
            return Err(ReturnCode::StreamError);
        }
        if direct == 0 {
            // Default when reading is auto-detect of gzip vs. transparent —
            // start with a transparent assumption in case of an empty file.
            direct = 1;
        }
        // `direct == -1` (from `'G'`) is left as gzip-only.
    } else if direct == -1 {
        // `'G'` has no meaning when writing or appending.
        return Err(ReturnCode::StreamError);
    }
    // Net: reading -> direct in {1 auto-detect, -1 gzip-only};
    //      writing -> direct in {0 gzip, 1 transparent}.

    Ok(ParsedMode {
        mode: gz_mode,
        level,
        strategy,
        direct,
        exclusive,
        #[cfg(unix)]
        nonblock,
    })
}

/// Validates a `gz*` mode string *without* opening or adopting anything — the
/// pre-flight half of [`parse_mode`].
///
/// This exists for one reason: C `gz_open` performs every mode-grammar rejection
/// **before** it stores the caller's descriptor in `state->fd`
/// (`gzlib.c` L150-L197 precede L263), so a rejected `gzdopen` leaves the
/// caller's descriptor open and reusable. The C-ABI `gzdopen` shim in
/// `src/ffi/gz.rs` must therefore decide whether the mode is acceptable *before*
/// it wraps the raw `fd` in a [`File`], because a [`File`] closes its descriptor
/// on drop. Calling this first reproduces C's contract exactly; [`gz_open`]
/// re-parses the same string afterwards, which is pure (no I/O, no allocation)
/// and therefore free of side effects.
///
/// # Errors
///
/// Exactly the errors [`parse_mode`] reports: [`ReturnCode::StreamError`] when
/// the string contains `'+'`, carries no `'r'`/`'w'`/`'a'`, forces a transparent
/// read (`'T'` while reading), or applies `'G'` while writing or appending.
pub(crate) fn validate_mode(mode: &str) -> Result<(), ReturnCode> {
    parse_mode(mode).map(|_| ())
}

// ===========================================================================
// Internal helpers: gz_reset (gzlib.c L68-89) and gz_open (gzlib.c L120-287).
// ===========================================================================

/// Resets a [`GzState`] to the "just opened, nothing buffered" condition — the
/// port of the internal C `gz_reset` (`gzlib.c` L68-89).
///
/// This clears the output window, the pending-seek amount, and the error state,
/// and reinitialises the per-direction bookkeeping. It deliberately does **not**
/// touch the rewind anchor [`start`](GzState::start), the negotiated buffer size
/// [`want`](GzState::want), or the identity/configuration fields, so it is safe
/// to call both from [`gz_open`] (once, at the end of open) and from
/// [`gzrewind`] (to restart an already-open reader).
///
/// The `mode` is only ever [`GzMode::Read`] or [`GzMode::Write`] here: the
/// transient [`GzMode::Append`] is converted to [`GzMode::Write`] by [`gz_open`]
/// before this is first called, and [`GzMode::None`] never reaches a reset.
fn gz_reset(state: &mut GzState) {
    // No output data is available.
    state.have = 0;

    if state.mode == GzMode::Read {
        // For reading: not at EOF, have not read past EOF, and look for a gzip
        // header on the next read. `junk == -1` marks "at the start" for the
        // trailing-junk classifier.
        state.eof = false;
        state.past = false;
        state.how = How::Look;
        state.junk = -1;
    } else {
        // For writing: no `deflateReset` is pending, and no compressed output is
        // waiting to be handed to the OS. C re-seeds the equivalent output cursor
        // in `gz_init` (`state->x.next = strm->next_out`, `gzwrite.c` L49-L55);
        // clearing both halves here leaves the pending window
        // `out_buf[out_start .. out_start + out_pending]` empty and re-anchored at
        // the front for a stream that is starting over.
        state.reset = false;
        state.out_pending = 0;
        state.out_start = 0;
    }

    // Shared: no non-blocking retry pending, no seek request pending, no error,
    // no uncompressed data delivered yet, and no compressed input buffered.
    state.again = false;
    state.skip = 0;
    state.clear_error();
    state.pos = 0;
    // The idiomatic `ZStream` carries no `avail_in`/`next_in`; the gz layer keeps
    // the unconsumed-input window on the state itself, so reset both halves (the
    // analogue of the C `state->strm.avail_in = 0;`).
    state.in_avail = 0;
    state.in_next = 0;
}

/// The shared file opener — the port of the internal C `gz_open`
/// (`gzlib.c` L120-287), which backs both `gzopen`/`gzopen64` and `gzdopen`.
///
/// `file` selects the two C entry points:
///
/// * `file == None` opens `path` from scratch (the `gzopen` case).
/// * `file == Some(f)` adopts an already-open descriptor `f` (the `gzdopen`
///   case); `path` then carries only the synthetic `<fd:N>` name used for error
///   messages.
///
/// The returned `Box<GzState>` is the safe analogue of the C opaque `gzFile`
/// pointer; the caller (or the FFI layer) is responsible for eventually closing
/// it via the `gzclose*` family. On any failure the state is dropped and the
/// corresponding [`ReturnCode`] is returned (the C code returns `NULL`, which the
/// FFI layer maps to a null `gzFile`).
///
/// # Errors
///
/// * The mode string is invalid (see [`parse_mode`]) — [`ReturnCode::StreamError`].
/// * Either of the two allocations C performs before opening anything — the state
///   itself and the retained path name — cannot be satisfied —
///   [`ReturnCode::MemError`]. Both are attempted *before* the file is opened, so
///   this failure creates, truncates or claims nothing (see the allocation-order
///   note in the body).
/// * The underlying open fails — [`ReturnCode::ErrNo`] (the C errno path).
fn gz_open(path: &Path, file: Option<File>, mode: &str) -> Result<Box<GzState>, ReturnCode> {
    // Parse and validate the mode string before touching the file system
    // (gzlib.c L150-197). An invalid mode short-circuits to an error, mirroring
    // the C "return NULL".
    let parsed = match parse_mode(mode) {
        Ok(parsed) => parsed,
        Err(code) => return Err(abandon_adopted(file, code)),
    };

    // ---------------------------------------------------------------------
    // Allocate before touching the file system — C's order exactly.
    //
    // Reference zlib's `gz_open` performs its two allocations FIRST and opens
    // the file LAST: `malloc(sizeof(gz_state))`, then the mode parse, then
    // `malloc(len + 1)` for the path name, and only then `open(path, oflag,
    // 0666)`. Each allocation failure returns `NULL` after freeing what came
    // before it, having created, truncated or `O_EXCL`-claimed nothing.
    //
    // That ordering is observable and worth preserving precisely: `"wb"` opens
    // with `O_CREAT | O_TRUNC`, so allocating after the open would let an
    // out-of-memory condition destroy the contents of a file the caller still
    // holds — a *data-loss* difference from C, not merely a different error
    // code. Both allocations are therefore fallible and both precede the open.
    // ---------------------------------------------------------------------

    // 1. The state itself (C's `malloc(sizeof(gz_state))`). `GzFile::pending()`
    //    stands in for C's as-yet-unassigned `state->fd`, and the path starts
    //    empty because C's `state->path` is likewise still unset here. The real
    //    handle is installed below, once the file is open.
    let Some(mut state) = crate::stream::try_box(GzState {
        // exposed window
        have: 0,
        next: 0,
        pos: 0,
        // identity / configuration
        mode: parsed.mode,
        file: GzFile::pending(),
        path: String::new(),
        size: 0,
        want: GZBUFSIZE,
        in_buf: Vec::new(),
        out_buf: Vec::new(),
        direct: parsed.direct,
        // reading only
        how: How::Look,
        junk: -1,
        again: false,
        in_next: 0,
        in_avail: 0,
        start: 0,
        eof: false,
        past: false,
        // writing only
        level: parsed.level,
        strategy: parsed.strategy,
        reset: false,
        out_pending: 0,
        out_start: 0,
        // shared
        skip: 0,
        err: ReturnCode::Ok,
        msg: None,
        msg_c: None,
        strm: crate::stream::ZStream::new(),
    }) else {
        return Err(abandon_adopted(file, ReturnCode::MemError));
    };

    // 2. The path name kept for error messages (C's `malloc(len + 1)` plus the
    //    `strcpy`/`wcstombs`). C frees the state before returning `NULL` here;
    //    the `Box` does that by dropping as this function returns.
    let Some(path_string) = try_path_string(path) else {
        return Err(abandon_adopted(file, ReturnCode::MemError));
    };
    state.path = path_string;

    // Open (or adopt) the underlying descriptor, mapping the C `oflag`
    // combination onto `OpenOptions` (gzlib.c L228-262).
    let mut handle = match file {
        // `gzdopen` case: adopt the already-open descriptor unchanged.
        Some(f) => f,
        // Open-by-path case.
        None => {
            let mut opts = OpenOptions::new();
            match parsed.mode {
                // Reading: `O_RDONLY`.
                GzMode::Read => {
                    opts.read(true);
                }
                // Writing / appending: `O_WRONLY | O_CREAT`, plus `O_TRUNC` for a
                // fresh write or `O_APPEND` for append. `'x'` adds `O_EXCL`
                // (create-new), which the OS ignores for a read-only open, so it
                // is applied only on this branch.
                GzMode::Write | GzMode::Append => {
                    opts.write(true).create(true);
                    if parsed.mode == GzMode::Append {
                        opts.append(true);
                    } else {
                        opts.truncate(true);
                    }
                    if parsed.exclusive {
                        opts.create_new(true);
                    }
                }
                // `parse_mode` guarantees a concrete read/write/append mode.
                GzMode::None => return Err(ReturnCode::StreamError),
            }
            // `O_NONBLOCK` (`'N'`) is a unix-only open flag with no portable
            // `OpenOptions` setter, so it is applied through the platform
            // extension trait.
            #[cfg(unix)]
            if parsed.nonblock {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(O_NONBLOCK);
            }
            // A failed open corresponds to the C errno path (Z_ERRNO); the FFI
            // layer maps the resulting `Err` to a null `gzFile`.
            opts.open(path).map_err(|_| ReturnCode::ErrNo)?
        }
    };

    // Append fix-up (gzlib.c L272-278): seek to end-of-file so subsequent offset
    // queries stay correct, then treat the stream as a plain writer from here on
    // (the transient `Append` mode never persists on a live `GzState`). The seek
    // result is ignored, matching the C `(void)LSEEK(...)`. C mutates
    // `state->mode` in place at this point, and so does this port.
    if parsed.mode == GzMode::Append {
        let _ = handle.seek(SeekFrom::End(0));
        state.mode = GzMode::Write;
    }

    // Read start anchor (gzlib.c L280-285): remember the current position as the
    // rewind anchor for `gzrewind`/`gzseek`. A non-seekable input (e.g. a pipe)
    // leaves the anchor at `0`. The append fix-up above has already rewritten
    // `Append` to `Write`, so this tests the effective mode exactly as C's
    // `if (state->mode == GZ_READ)` does after its own fix-up.
    if state.mode == GzMode::Read {
        state.start = handle
            .stream_position()
            .ok()
            .and_then(|pos| i64::try_from(pos).ok())
            .unwrap_or(0);
    }

    // Install the descriptor (C's `state->fd = ...`). From here the state is
    // complete and every `Deref` on `state.file` is valid.
    state.file = GzFile::new(handle);

    // Establish the runtime state (clears the window, error, and pending seek;
    // leaves `start`/`want`/configuration intact). gzlib.c L287.
    gz_reset(&mut state);

    Ok(state)
}

/// Returns `code` after giving up any descriptor `gz_open` had adopted —
/// **without closing it**.
///
/// # Why a failing `gzdopen` must not close the caller's descriptor
///
/// In C the adopted descriptor reaches `gz_open` as a plain `int` and is only
/// stored into `state->fd` once the `open`/adopt step is reached. Every earlier
/// failure — a rejected mode, `malloc(sizeof(gz_state))` returning `NULL`,
/// `malloc(len + 1)` for the path name returning `NULL` — therefore returns
/// `NULL` with the caller's descriptor still open, and `gzdopen`'s own
/// `malloc` failure does the same (`gzlib.c`: `if (fd == -1 || (path = malloc(…))
/// == NULL) return NULL;`). No `gzdopen` failure anywhere in reference zlib
/// closes the descriptor it was handed; ownership stays with the caller, who is
/// free to retry or to `close` it.
///
/// Rust would do the opposite by default. The adopted [`File`] *owns* its
/// descriptor, so simply returning `Err` runs its destructor and closes a
/// descriptor the caller still believes it owns — which is worse than a wrong
/// error code: a subsequent `close`, `read`, or `write` by the caller would
/// operate on a freed descriptor number that the OS may already have reissued.
///
/// [`core::mem::forget`] dissolves the wrapper without running the close,
/// leaving the descriptor exactly as the caller passed it. It is a safe function
/// and needs no platform-specific raw-descriptor extraction, so this layer keeps
/// its zero-`unsafe` guarantee (AAP §0.8.1 D-6). The apparent "leak" is only of
/// the [`File`] wrapper, which owns no heap allocation; the descriptor is not
/// leaked at all, because ownership returns to the caller — precisely the C
/// contract.
#[inline]
fn abandon_adopted(file: Option<File>, code: ReturnCode) -> ReturnCode {
    if let Some(adopted) = file {
        core::mem::forget(adopted);
    }
    code
}

/// Renders `path` for the state's error-message field, returning [`None`] if the
/// allocation cannot be satisfied — the fallible equivalent of C's
/// `state->path = malloc(len + 1)` plus its `NULL` check.
///
/// # Why not `path.display().to_string()`
///
/// [`std::path::Display`] is rendered through [`ToString`], which allocates
/// infallibly and **aborts the process** if the allocation fails. C checks that
/// `malloc` and returns `NULL`, so an abort here would replace a recoverable
/// `gzopen` failure with process death (AAP §0.6.5).
///
/// # Byte-for-byte equivalence with the replaced expression
///
/// The output must not change, because it is what `gzerror` reports. On unix a
/// path is an arbitrary byte string that need not be UTF-8, and
/// `Path::display()` renders invalid sequences the same way
/// [`String::from_utf8_lossy`] does — each maximal invalid subsequence becomes
/// one U+FFFD REPLACEMENT CHARACTER. [`std::str::Utf8Chunks`] exposes exactly
/// that decomposition: for every chunk, the valid prefix followed by U+FFFD if
/// (and only if) that chunk had invalid bytes. Iterating chunks and pushing
/// those two pieces therefore reproduces `display().to_string()` exactly, while
/// every growth step goes through [`String::try_reserve`].
///
/// C's path name is the raw byte string with no transformation at all; the lossy
/// rendering is this port's pre-existing, deliberate choice (a Rust `String` is
/// UTF-8 by definition) and is preserved unchanged here. Only the failure
/// behavior differs from the code this replaces.
fn try_path_string(path: &Path) -> Option<String> {
    let raw = path.as_os_str().as_encoded_bytes();

    let mut out = String::new();
    // One reservation for the common all-valid-UTF-8 case; the loop below still
    // reserves for anything it appends, so a short reservation here is only a
    // performance detail, never a correctness one.
    out.try_reserve(raw.len()).ok()?;

    for chunk in raw.utf8_chunks() {
        let valid = chunk.valid();
        out.try_reserve(valid.len()).ok()?;
        out.push_str(valid);

        if !chunk.invalid().is_empty() {
            // `char::REPLACEMENT_CHARACTER` is 3 bytes in UTF-8.
            out.try_reserve(char::REPLACEMENT_CHARACTER.len_utf8())
                .ok()?;
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }

    Some(out)
}

// ===========================================================================
// Public openers (gzlib.c L290-320).
// ===========================================================================

/// Opens the gzip (or transparent) file at `path` with the given mode string —
/// the idiomatic port of C `gzopen` (`gzlib.c` L290-292).
///
/// `mode` follows the zlib grammar accepted by `parse_mode`, e.g. `"rb"` to
/// read, `"wb9"` to write at maximum compression, `"ab"` to append, with
/// optional strategy (`f`/`h`/`R`/`F`) and transparency (`T`/`G`) flags.
///
/// On success a `Box<GzState>` — the safe analogue of the C opaque `gzFile` —
/// is returned; it is closed by the `gzclose*` family (or by being dropped).
///
/// # Errors
///
/// Returns a [`ReturnCode`] describing the failure: [`ReturnCode::StreamError`]
/// for an invalid mode string, or [`ReturnCode::ErrNo`] if the underlying file
/// could not be opened.
pub fn gzopen<P: AsRef<Path>>(path: P, mode: &str) -> Result<Box<GzState>, ReturnCode> {
    gz_open(path.as_ref(), None, mode)
}

/// Large-file (`z_off64_t`) alias for [`gzopen`] — the port of C `gzopen64`
/// (`gzlib.c` L295-297).
///
/// In this crate every offset is already a 64-bit [`i64`], so `gzopen64` is
/// behaviourally identical to [`gzopen`]; the distinct symbol exists only to
/// preserve the C API surface at the FFI boundary. It delegates directly.
///
/// # Errors
///
/// Identical to [`gzopen`].
pub fn gzopen64<P: AsRef<Path>>(path: P, mode: &str) -> Result<Box<GzState>, ReturnCode> {
    gzopen(path, mode)
}

/// Wraps an already-open [`File`] in a gz reader/writer — the idiomatic port of
/// C `gzdopen` (`gzlib.c` L300-315).
///
/// The C prototype takes a raw `int fd`; that raw-descriptor adoption
/// (`File::from_raw_fd`, which is `unsafe`) is performed in `src/ffi/gz.rs`,
/// while this idiomatic entry point takes an owned [`File`] and never touches
/// `unsafe`. A synthetic `<fd:N>` name (derived from the OS descriptor on unix)
/// is recorded for use in error messages, mirroring the C
/// `sprintf(path, "<fd:%d>", fd)`.
///
/// # Errors
///
/// Returns a [`ReturnCode`] for an invalid mode string
/// ([`ReturnCode::StreamError`]); the append fix-up seek is best-effort and does
/// not fail the call.
pub fn gzdopen(file: File, mode: &str) -> Result<Box<GzState>, ReturnCode> {
    // C's `gzdopen` allocates this name itself and bails out before calling
    // `gz_open` if that allocation fails: `if (fd == -1 || (path = malloc(7 + 3 *
    // sizeof(int))) == NULL) return NULL;`. It returns `NULL` without closing the
    // descriptor, so `abandon_adopted` hands `fd` back to the caller intact.
    let Some(name) = fd_path(&file) else {
        return Err(abandon_adopted(Some(file), ReturnCode::MemError));
    };
    gz_open(Path::new(&name), Some(file), mode)
}

/// Capacity reserved for the synthetic `<fd:N>` name, mirroring C's
/// `malloc(7 + 3 * sizeof(int))`.
///
/// C sizes the buffer from `sizeof(int)` rather than from the value, and 19 bytes
/// covers the widest possible rendering: `"<fd:"` (4) plus `i32::MIN` as
/// `"-2147483648"` (11) plus `">"` (1) is 16. Reserving the full C figure up
/// front means the formatting below cannot trigger a reallocation, which is what
/// makes it infallible after the one fallible reservation.
const FD_PATH_CAPACITY: usize = 7 + 3 * core::mem::size_of::<core::ffi::c_int>();

/// Builds the synthetic `<fd:N>` error-message name for [`gzdopen`], returning
/// [`None`] if the allocation cannot be satisfied.
///
/// On unix the OS descriptor number is included (matching the C `<fd:%d>`
/// format); on other platforms the descriptor number is not portably available
/// here, so the generic `<fd>` placeholder is used.
///
/// The result is fallible because C checks the corresponding `malloc` and returns
/// `NULL` on failure; `format!` would abort the process instead (AAP §0.6.5).
#[cfg(unix)]
fn fd_path(file: &File) -> Option<String> {
    use core::fmt::Write as _;
    use std::os::unix::io::AsRawFd;

    let mut name = String::new();
    name.try_reserve_exact(FD_PATH_CAPACITY).ok()?;

    // Infallible: `FD_PATH_CAPACITY` exceeds the longest possible rendering, so
    // no `push_str` inside the formatter can reallocate, and `fmt::Write for
    // String` has no other failure mode — its `write_str` returns `Ok` always.
    let _ = write!(name, "<fd:{}>", file.as_raw_fd());
    Some(name)
}

/// Non-unix fallback for [`fd_path`]: the descriptor number is not portably
/// available, so a generic placeholder is used. Fallible for the same reason as
/// the unix form, so both platforms report an exhausted allocator identically.
#[cfg(not(unix))]
fn fd_path(_file: &File) -> Option<String> {
    let mut name = String::new();
    name.try_reserve_exact(FD_PATH_CAPACITY).ok()?;
    name.push_str("<fd>");
    Some(name)
}

// ===========================================================================
// gzbuffer (gzlib.c L322-343).
// ===========================================================================

/// Sets the internal buffer size for a freshly opened file — the port of C
/// `gzbuffer` (`gzlib.c` L322-343).
///
/// Must be called after opening but **before** any read or write (i.e. before
/// the buffers are lazily allocated); the requested `size` becomes the base
/// buffer size `want` (the doubled buffer used on the read/write
/// hot paths is `size << 1`).
///
/// Returns `0` on success and `-1` on failure, exactly matching the C contract.
/// Failure occurs when the file is not a live reader/writer, when the buffers
/// have already been allocated (`size` `!= 0`), or when `size`
/// is so large it cannot be doubled without overflow. A `size` below `8` is
/// raised to `8` (the minimum the algorithms require) rather than rejected.
///
/// # A large request is accepted here and reported later — deliberately
///
/// The only size-related rejection C performs is the doubling-overflow test, so
/// any `size` up to `UINT_MAX >> 1` is *accepted*, including values whose buffers
/// cannot possibly be allocated. `gzbuffer(file, 0x7fff_ffff)` returns `0`, and
/// the ~2 GiB output buffer plus ~4 GiB doubled input buffer are requested later,
/// on the first read or write, when `gz_look`/`gz_init` allocate lazily. This
/// port keeps that timing exactly rather than pre-rejecting the request: moving
/// the rejection here would make a `gzbuffer` call fail that succeeds in C, and
/// the amount of memory available at the time of the deferred allocation is not
/// knowable now in any case.
///
/// What the deferred failure must *not* do is abort. Both lazy allocators go
/// through the crate-internal fallible `alloc_zeroed` helper and report an
/// unsatisfiable request as `Z_MEM_ERROR` / "out of memory" on the state,
/// exactly as C does when its
/// `malloc` returns `NULL`: the read or write returns its failure value,
/// `gzerror` reports the code and message, the stream stays usable, and `size`
/// remains `0` so a later call may try again. A caller may therefore recover by
/// lowering `want` with another `gzbuffer` and retrying — which is only possible
/// because nothing was allocated and nothing aborted.
#[must_use]
pub fn gzbuffer(state: &mut GzState, mut size: u32) -> i32 {
    // Only meaningful on a live reader or writer (rejects `GZ_NONE`).
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return -1;
    }

    // The buffers must not already have been allocated.
    if state.size != 0 {
        return -1;
    }

    // Must be able to double the size without wrapping (the hot paths use
    // `want << 1`). `size << 1` truncates rather than panics, exactly like the C
    // unsigned shift, so `(size << 1) < size` faithfully detects the overflow.
    if (size << 1) < size {
        return -1;
    }

    // At least two bytes are required by the look-ahead / push-back logic.
    if size < 8 {
        size = 8;
    }

    state.want = size as usize;
    0
}

// ===========================================================================
// gzsetparams (C body in gzwrite.c L630-663; hosted here per the AAP
// function-family mapping).
// ===========================================================================

/// Dynamically updates the compression `level` and `strategy` of a write stream
/// — the port of C `gzsetparams` (`gzwrite.c` L630-663).
///
/// Although the C body lives in `gzwrite.c`, this configuration entry point is
/// hosted in `open.rs` per the AAP's function-family reorganisation. It drives
/// the write path's `gz_comp` /
/// `gz_zero` helpers together with the engine's
/// [`deflate_params`](crate::deflate::deflate_params).
///
/// Returns `0` (`Z_OK`) on success, or a negative C return code on failure
/// (mirroring zlib): [`ReturnCode::StreamError`] if the file is not a live,
/// non-transparent writer without a serious error, or if the writer has already
/// begun compressing and `strategy` is not a valid strategy value; or the
/// recorded stream error if a pending seek or the pre-flush fails.
///
/// # When an out-of-range argument is reported
///
/// C performs **no** range validation of its own here (`gzwrite.c` L630-L663):
/// it records `level` and `strategy` and returns `Z_OK`, leaving `deflateInit2`
/// / `deflateParams` to reject an out-of-range value later. This port keeps that
/// timing exactly, so the point at which a bad argument is reported depends on
/// whether the engine already exists:
///
/// * **Before any I/O** (`size == 0`, no engine yet) both arguments are recorded
///   and `Z_OK` is returned. The value is validated when the deferred
///   `gz_init` runs on the first write, which reports
///   `Z_MEM_ERROR` / "out of memory" exactly as C's `gz_init` does for any
///   `deflateInit2` rejection (C L37-L43); the write then returns `0`, `gzerror`
///   reports the failure, and `gzclose_w` propagates it. Nothing is silently
///   substituted for the caller's value.
/// * **After the engine is live** (`size != 0`) the change is applied
///   immediately, so an invalid `strategy` is refused here with
///   [`ReturnCode::StreamError`] rather than deferred. Note that C reaches
///   `Z_OK` on this path even for an out-of-range `strategy`, because it calls
///   `deflateParams` for its side effect and discards the return value; refusing
///   the call keeps a value the engine cannot honour from being recorded, and
///   keeps `gzsetparams` from reporting success for a request it did not apply.
///
/// # Behavioural note (vs. C)
///
/// The reference C `gzsetparams` lets `deflateParams`' internal `Z_BLOCK` flush
/// emit into the persistent output buffer, to be written by the *next*
/// `gz_comp`. This port's `gz_comp` instead hands each `deflate` call's output to
/// the file immediately, retaining across calls only what the destination
/// declined to accept
/// ([`GzState::out_pending`](crate::gz::state::GzState)). So — when the buffers
/// are live — this function performs the block flush itself (via
/// `gz_comp(Z_BLOCK)`) and delivers it to the file *before* calling
/// [`deflate_params`](crate::deflate::deflate_params). The stream is then on a
/// block boundary with nothing pending, so `deflate_params` emits no bytes and
/// the observable output is byte-identical to C.
#[must_use]
pub fn gzsetparams(state: &mut GzState, level: i32, strategy: i32) -> i32 {
    // Require a live, non-transparent write stream with no *serious* error (a
    // pending non-blocking retry, `again`, is tolerated). C L640-642.
    if state.mode != GzMode::Write
        || (state.err != ReturnCode::Ok && !state.again)
        || state.direct != 0
    {
        return ReturnCode::StreamError.as_c_int();
    }

    // Clear any soft error state (C `gz_error(state, Z_OK, NULL);`, L643).
    state.clear_error();

    // If neither parameter is actually changing, there is nothing to do
    // (C L646-647).
    if level == state.level && strategy == state.strategy {
        return ReturnCode::Ok.as_c_int();
    }

    // Honour a pending forward seek before altering the stream: the gap is
    // realized as zero bytes compressed with the *current* parameters
    // (C L650-651). `gz_zero` records the error code on failure.
    if state.skip != 0 && gz_zero(state).is_err() {
        return state.err.as_c_int();
    }

    // Change the compression parameters for subsequent input, but only once the
    // buffers (and hence the deflate engine) have been allocated (C L654-660).
    if state.size != 0 {
        // Flush what has been produced so far with the previous parameters and
        // drain it to the file. See the "Behavioural note" above for why this is
        // unconditional here rather than gated on buffered input as in C.
        if gz_comp(state, FlushMode::Block).is_err() {
            return state.err.as_c_int();
        }

        // Apply the change to the engine. `strategy` must be a valid value; an
        // invalid one is rejected (matching the C `deflateParams` validation).
        // Because the stream is fully flushed, `deflate_params` produces no
        // output, so its return value is discarded exactly as C discards it.
        let Some(strat) = Strategy::from_c_int(strategy) else {
            return ReturnCode::StreamError.as_c_int();
        };
        let _ = deflate::deflate_params(&mut state.strm, &[], &mut state.out_buf, level, strat);
    }

    // Record the new parameters (C L661-662). C stores them unvalidated, and so
    // does this port: when the engine does not exist yet, the deferred `gz_init`
    // hands the recorded values to `deflate_init2` and reports any rejection as
    // `Z_MEM_ERROR` on the first write (see "When an out-of-range argument is
    // reported" above). No value is ever silently replaced with a default.
    state.level = level;
    state.strategy = strategy;
    ReturnCode::Ok.as_c_int()
}

// ===========================================================================
// Positioning family (gzlib.c L344-510).
//
// The `*64` functions are the real implementations (every offset is a 64-bit
// `i64`); the non-`*64` variants are the `z_off_t`-typed API aliases. On the
// 64-bit targets this crate supports, the platform `z_off_t` is already 64-bit,
// so the aliases delegate directly — the C `ret == (z_off_t)ret ? ret : -1`
// narrowing to a *smaller* `z_off_t` is re-materialised at the FFI boundary
// (`src/ffi/gz.rs`), which owns the C `z_off_t` / `z_off64_t` split.
// ===========================================================================

/// `whence` value selecting an absolute seek from the start of the stream
/// (C `<stdio.h>` `SEEK_SET`).
const SEEK_SET: i32 = 0;

/// `whence` value selecting a seek relative to the current position
/// (C `<stdio.h>` `SEEK_CUR`).
const SEEK_CUR: i32 = 1;

/// Rewinds a read stream to its start anchor — the port of C `gzrewind`
/// (`gzlib.c` L345-363).
///
/// Only valid on a reader with no serious error. Seeks the underlying file back
/// to the position captured when the file was opened
/// (`start`) and resets the runtime state via `gz_reset`, so
/// the next read restarts from the beginning of the gzip data.
///
/// Returns `0` on success and `-1` on failure (wrong mode, a serious error, or a
/// failed seek), matching the C contract.
#[must_use]
pub fn gzrewind(state: &mut GzState) -> i32 {
    // Only meaningful while reading, and only when there is no serious error
    // (a soft `Z_BUF_ERROR` is tolerated). C L353-355.
    if state.mode != GzMode::Read
        || (state.err != ReturnCode::Ok && state.err != ReturnCode::BufError)
    {
        return -1;
    }

    // Back up to the start anchor and start over (C L358-361). `start` is a
    // non-negative file position captured at open time.
    let Ok(anchor) = u64::try_from(state.start) else {
        return -1;
    };
    if state.file.seek(SeekFrom::Start(anchor)).is_err() {
        return -1;
    }
    gz_reset(state);
    0
}

/// Seeks to `offset` interpreted per `whence` — the 64-bit real implementation,
/// the port of C `gzseek64` (`gzlib.c` L366-434).
///
/// Only `SEEK_SET` (absolute) and `SEEK_CUR` (relative) are supported. The seek
/// is expressed in terms of the *uncompressed* stream: on the read path a
/// forward seek is realized lazily by skipping bytes on subsequent reads (and,
/// for a backward seek, by rewinding first), while on the write path a forward
/// seek is realized by writing zero bytes. A seek that lands inside the raw
/// (transparent) region while reading is performed directly on the file.
///
/// Returns the resulting uncompressed position on success, or `-1` on failure
/// (wrong mode, a serious error, an unsupported `whence`, a backward write seek,
/// a seek before the start of the file, or a failed underlying seek).
#[must_use]
pub fn gzseek64(state: &mut GzState, mut offset: i64, whence: i32) -> i64 {
    // Integrity: a live reader/writer with no serious error (C L374-381).
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return -1;
    }
    if state.err != ReturnCode::Ok && state.err != ReturnCode::BufError {
        return -1;
    }

    // Can only seek from the start or relative to the current position
    // (C L384-385).
    if whence != SEEK_SET && whence != SEEK_CUR {
        return -1;
    }

    // Normalize `offset` to a SEEK_CUR specification (C L388-393). For SEEK_CUR
    // any not-yet-applied forward skip is folded in (unless we have already read
    // past EOF), then cleared. All signed offset arithmetic in this function is
    // checked so an extreme `offset`/`pos`/`skip` combination returns the
    // zlib-style `-1` error rather than panicking in debug builds or silently
    // wrapping in release builds.
    if whence == SEEK_SET {
        offset = match offset.checked_sub(state.pos) {
            Some(v) => v,
            None => return -1,
        };
    } else {
        let pending = if state.past { 0 } else { state.skip };
        offset = match offset.checked_add(pending) {
            Some(v) => v,
            None => return -1,
        };
        state.skip = 0;
    }

    // If within the raw area while reading, just go there (C L396-410). The
    // target position `pos + offset` is computed with a checked add; on overflow
    // the fast-path guard is simply false, so control falls through to the
    // general path below (which handles or rejects the seek). This mirrors C's
    // `state->x.pos + offset >= 0` guard without ever wrapping.
    let fast_target = if state.mode == GzMode::Read && state.how == How::Copy {
        // A checked add avoids wrapping; `filter` drops a negative target so the
        // guard matches C's `state->x.pos + offset >= 0`. Expressed as an
        // `Option` rather than an `if let ... && ...` chain, which is unstable
        // before Rust 1.88 (this crate's MSRV is 1.85).
        state.pos.checked_add(offset).filter(|&t| t >= 0)
    } else {
        None
    };
    if let Some(target) = fast_target {
        // Seek relative to the current file position, discounting the bytes
        // already sitting in the output buffer (C `offset - x.have`).
        let delta = match offset.checked_sub(state.have as i64) {
            Some(d) => d,
            None => return -1,
        };
        if state.file.seek(SeekFrom::Current(delta)).is_err() {
            return -1;
        }
        state.have = 0;
        state.eof = false;
        state.past = false;
        state.skip = 0;
        state.clear_error();
        // Drop any buffered-but-unconsumed compressed input (the index form of
        // the C `state->strm.avail_in = 0;`).
        state.in_avail = 0;
        state.in_next = 0;
        state.pos = target;
        return state.pos;
    }

    // Calculate the skip amount, rewinding first for a backward read seek
    // (C L413-421).
    if offset < 0 {
        if state.mode != GzMode::Read {
            // Writing: cannot go backwards.
            return -1;
        }
        // Fold the current position back in (C `offset += state->x.pos`) with a
        // checked add; an out-of-range result is rejected rather than wrapping.
        offset = match offset.checked_add(state.pos) {
            Some(v) => v,
            None => return -1,
        };
        if offset < 0 {
            // Before the start of the file.
            return -1;
        }
        if gzrewind(state) == -1 {
            return -1;
        }
    }

    // If reading, consume what is already in the output buffer first — one fewer
    // check on the later `gzgetc()` fast path (C L424-430). `offset` is
    // non-negative here (any backward seek was rewound above).
    if state.mode == GzMode::Read {
        // n = min(have, offset), guarded by `gt_off` for the offset-type width.
        let n: usize = if gt_off(state.have) || state.have as i64 > offset {
            offset as usize
        } else {
            state.have
        };
        state.have -= n;
        state.next += n;
        // `n` is bounded by `min(have, offset)` (both non-negative here), so
        // `pos + n` and `offset - n` cannot themselves overflow; the checked add
        // keeps every offset computation panic-free and uniform.
        state.pos = match state.pos.checked_add(n as i64) {
            Some(v) => v,
            None => return -1,
        };
        offset -= n as i64;
    }

    // Request the (possibly zero) remaining skip and report where we will be
    // once it is applied (C L433-434). The reported position uses a checked add
    // so an out-of-range result is surfaced as `-1` instead of wrapping.
    state.skip = offset;
    state.pos.checked_add(offset).unwrap_or(-1)
}

/// `z_off_t`-typed alias for [`gzseek64`] — the port of C `gzseek`
/// (`gzlib.c` L437-442).
///
/// Behaviourally identical to [`gzseek64`] in this idiomatic layer (offsets are
/// 64-bit throughout); the narrowing to a smaller platform `z_off_t` is applied
/// by the FFI layer.
#[must_use]
pub fn gzseek(state: &mut GzState, offset: i64, whence: i32) -> i64 {
    gzseek64(state, offset, whence)
}

/// Returns the current uncompressed position — the 64-bit real implementation,
/// the port of C `gztell64` (`gzlib.c` L445-457).
///
/// The reported position accounts for a pending forward seek that has not yet
/// been applied (`pos + skip`, unless we have already read past EOF). Returns
/// `-1` if `state` is not a live reader/writer.
#[must_use]
pub fn gztell64(state: &GzState) -> i64 {
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return -1;
    }
    // `pos + skip` with a checked add so a pending-seek total that would exceed
    // the offset type is reported as `-1` rather than panicking/wrapping.
    let pending = if state.past { 0 } else { state.skip };
    state.pos.checked_add(pending).unwrap_or(-1)
}

/// `z_off_t`-typed alias for [`gztell64`] — the port of C `gztell`
/// (`gzlib.c` L460-465).
#[must_use]
pub fn gztell(state: &GzState) -> i64 {
    gztell64(state)
}

/// Returns the current *compressed* file offset — the 64-bit real
/// implementation, the port of C `gzoffset64` (`gzlib.c` L468-486).
///
/// This is the position within the underlying file, i.e. how many compressed
/// bytes have been read or written. While reading, compressed input that has
/// been buffered but not yet consumed is subtracted so the value reflects the
/// true decode frontier. Returns `-1` if `state` is not a live reader/writer or
/// the underlying position cannot be determined.
#[must_use]
pub fn gzoffset64(state: &mut GzState) -> i64 {
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return -1;
    }

    // Effective offset in the file (C L480-482).
    let offset = match state.file.stream_position() {
        Ok(pos) => match i64::try_from(pos) {
            Ok(off) => off,
            Err(_) => return -1,
        },
        Err(_) => return -1,
    };

    if state.mode == GzMode::Read {
        // Don't count compressed input that is buffered but not yet consumed
        // (C `offset -= state->strm.avail_in;`). The buffered count is converted
        // and subtracted with checked operations so an out-of-range value yields
        // `-1` instead of wrapping. In practice `in_avail` is bounded by the
        // input buffer size and never exceeds `offset`, so this never triggers;
        // the checks keep the offset arithmetic uniformly panic-free.
        let buffered = match i64::try_from(state.in_avail) {
            Ok(v) => v,
            Err(_) => return -1,
        };
        offset.checked_sub(buffered).unwrap_or(-1)
    } else {
        offset
    }
}

/// `z_off_t`-typed alias for [`gzoffset64`] — the port of C `gzoffset`
/// (`gzlib.c` L489-494).
#[must_use]
pub fn gzoffset(state: &mut GzState) -> i64 {
    gzoffset64(state)
}

/// Reports whether a read has attempted to go past the end of the file — the
/// port of C `gzeof` (`gzlib.c` L497-509).
///
/// Returns `1` once a read request has tried to consume data beyond EOF while
/// reading, and `0` otherwise (including for any write stream). Note this
/// reports having *attempted* to read past EOF, not merely having reached it —
/// exactly the C semantics.
#[must_use]
pub fn gzeof(state: &GzState) -> i32 {
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return 0;
    }
    if state.mode == GzMode::Read {
        i32::from(state.past)
    } else {
        0
    }
}

// ===========================================================================
// gzdirect (C body in gzread.c L627-642; hosted here per the AAP
// function-family mapping).
// ===========================================================================

/// Reports whether the file is being read/written transparently (not gzip) —
/// the port of C `gzdirect` (`gzread.c` L627-642).
///
/// Although the C body lives in `gzread.c`, this query is hosted in `open.rs`
/// per the AAP's function-family reorganisation; it drives the read path's
/// `gz_look` look-ahead. If the transparency is not
/// yet known but can be determined — chiefly right after a
/// [`gzopen`]/[`gzdopen`] on a reader — the look-ahead is run to resolve it.
///
/// Returns `1` if the data is transparent (copied straight through) and `0` if a
/// gzip stream is being processed.
#[must_use]
pub fn gzdirect(state: &mut GzState) -> i32 {
    // If the state is not yet known but can be discovered, do so now — mainly
    // for right after a gzopen()/gzdopen() (C L636-639). The look-ahead's result
    // is intentionally ignored; any error it records surfaces on the next read.
    if state.mode == GzMode::Read && state.how == How::Look && state.have == 0 {
        let _ = gz_look(state);
    }

    // 1 if transparent, 0 if processing a gzip stream (C L642).
    i32::from(state.direct == 1)
}

// ===========================================================================
// Error accessors (gzlib.c L512-547).
// ===========================================================================

/// Returns the last error message for the file and, optionally, its numeric
/// code — the port of C `gzerror` (`gzlib.c` L512-528).
///
/// If `errnum` is `Some`, the slot is set to the current error code (the C
/// integer form of [`GzState::err`](crate::gz::state::GzState)). The returned
/// string is `"out of memory"` for [`ReturnCode::MemError`] (which stores no
/// heap message), the stored `"{path}: {detail}"` message when one is present,
/// or the empty string when there is no error message.
///
/// The FFI layer adapts this to the C `int *errnum` out-parameter and a
/// `const char *` return (mapping a null file to a null pointer).
#[must_use]
pub fn gzerror<'a>(state: &'a GzState, errnum: Option<&mut i32>) -> &'a str {
    // A live `GzState` is always `Read` or `Write`; the C integrity check
    // returns NULL for anything else, which this idiomatic layer represents as
    // the empty string (the FFI supplies the actual null-pointer behaviour).
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return "";
    }

    // Report the numeric error code if the caller asked for it (C L523-524).
    if let Some(slot) = errnum {
        *slot = state.err.as_c_int();
    }

    // Return the message text (C L525-527).
    if state.err == ReturnCode::MemError {
        "out of memory"
    } else {
        state.msg.as_deref().unwrap_or("")
    }
}

/// Clears the error and end-of-file state of the file — the port of C
/// `gzclearerr` (`gzlib.c` L531-547).
///
/// On a reader this also clears the `eof`/`past`
/// flags so subsequent reads can proceed past a previously observed EOF; the
/// stored error code and message are reset to the no-error state on both reader
/// and writer.
pub fn gzclearerr(state: &mut GzState) {
    // Integrity check (C L537-539): only meaningful on a live reader/writer.
    if state.mode != GzMode::Read && state.mode != GzMode::Write {
        return;
    }

    // Clear the end-of-file flags when reading (C L542-545).
    if state.mode == GzMode::Read {
        state.eof = false;
        state.past = false;
    }

    // Clear the error code and message (C L546, `gz_error(state, Z_OK, NULL)`).
    state.clear_error();
}

// ===========================================================================
// gz_intmax / GT_OFF (gzlib.c L595-609, gzguts.h L216) — internal.
// ===========================================================================

/// Returns the maximum value of a C `int`, as a 64-bit offset — the port of C
/// `gz_intmax` (`gzlib.c` L595-609).
///
/// zlib computes this portably to cover exotic integer representations; on every
/// platform this crate targets a C `int` is a two's-complement [`i32`], so the
/// value is simply [`i32::MAX`].
#[must_use]
pub(crate) fn gz_intmax() -> i64 {
    i64::from(i32::MAX)
}

/// Faithful port of the C `GT_OFF(x)` guard macro (`gzguts.h` L216):
/// `(sizeof(int) == sizeof(z_off64_t) && (x) > gz_intmax())`.
///
/// It guards a large `usize`→`i64` (offset) conversion: it is true only on a
/// platform where a C `int` is as wide as `z_off64_t` (so a count above
/// [`gz_intmax`] could not be represented as a positive offset). On the 64-bit
/// targets this crate supports, a C `int` ([`core::ffi::c_int`], 4 bytes) is
/// never as wide as [`i64`] (8 bytes), so this is always `false`; it is
/// implemented faithfully so the seek/skip arithmetic in [`gzseek64`] (and the
/// sibling read/write skip paths) matches C on every platform.
#[must_use]
pub(crate) fn gt_off(x: usize) -> bool {
    core::mem::size_of::<core::ffi::c_int>() == core::mem::size_of::<i64>()
        && x > gz_intmax() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `GzState` in `mode` backed by a real, seekable file (the running
    /// test binary, falling back to `/dev/null`) so the positioning and query
    /// helpers can be exercised with **no `unsafe`** and no external fixture.
    ///
    /// The identity path is a fixed `"test"` so error-message assertions are
    /// deterministic. No compression I/O is performed against the file by the
    /// state-only tests; the seek-based tests only reposition it.
    fn test_state(mode: GzMode) -> GzState {
        let file = std::env::current_exe()
            .ok()
            .and_then(|p| File::open(p).ok())
            .or_else(|| File::open("/dev/null").ok())
            .expect("a real, openable file is required for the test GzState");
        GzState {
            have: 0,
            next: 0,
            pos: 0,
            mode,
            file: GzFile::new(file),
            path: String::from("test"),
            size: 0,
            want: GZBUFSIZE,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            direct: 0,
            how: How::Look,
            junk: -1,
            again: false,
            in_next: 0,
            in_avail: 0,
            start: 0,
            eof: false,
            past: false,
            level: Z_DEFAULT_COMPRESSION,
            strategy: Z_DEFAULT_STRATEGY,
            reset: false,
            out_pending: 0,
            out_start: 0,
            skip: 0,
            err: ReturnCode::Ok,
            msg: None,
            msg_c: None,
            strm: crate::stream::ZStream::new(),
        }
    }

    // -- mode-string parsing -------------------------------------------------

    #[test]
    fn parse_mode_read_defaults_to_auto_detect() {
        let p = parse_mode("rb").expect("valid read mode");
        assert_eq!(p.mode, GzMode::Read);
        assert_eq!(p.level, Z_DEFAULT_COMPRESSION);
        assert_eq!(p.strategy, Z_DEFAULT_STRATEGY);
        // Reading with no G/T defaults to auto-detect (transparent assumption).
        assert_eq!(p.direct, 1);
        assert!(!p.exclusive);
    }

    #[test]
    fn parse_mode_write_reads_level_and_strategy() {
        let p = parse_mode("wb9h").expect("valid write mode");
        assert_eq!(p.mode, GzMode::Write);
        assert_eq!(p.level, 9);
        assert_eq!(p.strategy, Z_HUFFMAN_ONLY);
        // Writing with no G/T stays gzip (direct == 0).
        assert_eq!(p.direct, 0);
    }

    #[test]
    fn parse_mode_last_digit_wins() {
        assert_eq!(parse_mode("wb19").expect("valid").level, 9);
        assert_eq!(parse_mode("w5").expect("valid").level, 5);
        assert_eq!(parse_mode("w0").expect("valid").level, 0);
    }

    #[test]
    fn parse_mode_all_strategy_flags() {
        assert_eq!(parse_mode("wf").expect("valid").strategy, Z_FILTERED);
        assert_eq!(parse_mode("wh").expect("valid").strategy, Z_HUFFMAN_ONLY);
        assert_eq!(parse_mode("wR").expect("valid").strategy, Z_RLE);
        assert_eq!(parse_mode("wF").expect("valid").strategy, Z_FIXED);
    }

    #[test]
    fn parse_mode_exclusive_flag_is_recorded() {
        assert!(parse_mode("wx").expect("valid").exclusive);
        assert!(!parse_mode("wb").expect("valid").exclusive);
    }

    #[test]
    fn parse_mode_rejects_read_write_plus() {
        // '+' (simultaneous read and write) is always rejected.
        assert_eq!(parse_mode("rb+").unwrap_err(), ReturnCode::StreamError);
        assert_eq!(parse_mode("wb+").unwrap_err(), ReturnCode::StreamError);
    }

    #[test]
    fn parse_mode_rejects_forced_transparent_read() {
        // 'T' while reading cannot force a transparent read.
        assert_eq!(parse_mode("rT").unwrap_err(), ReturnCode::StreamError);
    }

    #[test]
    fn parse_mode_rejects_gzip_only_write() {
        // 'G' has no meaning when writing or appending.
        assert_eq!(parse_mode("wG").unwrap_err(), ReturnCode::StreamError);
        assert_eq!(parse_mode("aG").unwrap_err(), ReturnCode::StreamError);
    }

    #[test]
    fn parse_mode_requires_a_direction() {
        assert_eq!(parse_mode("b9").unwrap_err(), ReturnCode::StreamError);
        assert_eq!(parse_mode("").unwrap_err(), ReturnCode::StreamError);
    }

    #[test]
    fn parse_mode_gzip_only_read_sets_direct_negative() {
        let p = parse_mode("rG").expect("valid");
        assert_eq!(p.mode, GzMode::Read);
        assert_eq!(p.direct, -1);
    }

    #[test]
    fn parse_mode_transparent_write_sets_direct_one() {
        assert_eq!(parse_mode("wT").expect("valid").direct, 1);
        let ap = parse_mode("aT").expect("valid");
        assert_eq!(ap.mode, GzMode::Append);
        assert_eq!(ap.direct, 1);
    }

    #[test]
    fn parse_mode_last_of_g_or_t_wins() {
        // ...G then T while reading -> ends transparent (1) -> rejected.
        assert_eq!(parse_mode("rGT").unwrap_err(), ReturnCode::StreamError);
        // ...T then G while reading -> ends gzip-only (-1) -> accepted.
        assert_eq!(parse_mode("rTG").expect("valid").direct, -1);
    }

    /// [`validate_mode`] must accept and reject exactly what [`parse_mode`] does.
    ///
    /// The C-ABI `gzdopen` shim relies on this to decide whether a mode is usable
    /// *before* it wraps the caller's raw descriptor in a [`File`], reproducing C's
    /// ordering (`gzlib.c` L150-L197 precede L263) so that a rejected `gzdopen`
    /// never closes the caller's descriptor. Any divergence between the two would
    /// either close a descriptor C leaves open or adopt one C would have refused.
    #[test]
    fn validate_mode_agrees_with_parse_mode() {
        // The six strings reference zlib rejects, verified against the C
        // conformance harness: read+write, forced-transparent read, gzip-only
        // write, and three with no r/w/a at all.
        for mode in ["r+", "rT", "wG", "b", "", "9"] {
            assert_eq!(
                validate_mode(mode).unwrap_err(),
                ReturnCode::StreamError,
                "validate_mode must reject {mode:?}"
            );
            assert!(
                parse_mode(mode).is_err(),
                "parse_mode must agree about {mode:?}"
            );
        }

        // Representative accepted strings across both directions and both
        // transparency settings.
        for mode in ["rb", "wb", "ab", "wb9", "rG", "wT", "rTG", "wbx", "r"] {
            assert!(
                validate_mode(mode).is_ok(),
                "validate_mode must accept {mode:?}"
            );
            assert!(
                parse_mode(mode).is_ok(),
                "parse_mode must agree about {mode:?}"
            );
        }
    }

    // -- gzbuffer ------------------------------------------------------------

    #[test]
    fn gzbuffer_raises_small_size_to_minimum() {
        let mut s = test_state(GzMode::Write);
        assert_eq!(gzbuffer(&mut s, 4), 0);
        assert_eq!(s.want, 8);
    }

    #[test]
    fn gzbuffer_accepts_a_normal_size() {
        let mut s = test_state(GzMode::Read);
        assert_eq!(gzbuffer(&mut s, 4096), 0);
        assert_eq!(s.want, 4096);
    }

    #[test]
    fn gzbuffer_rejects_size_that_cannot_double() {
        let mut s = test_state(GzMode::Write);
        // The top bit is set, so `size << 1` wraps below `size` -> rejected.
        assert_eq!(gzbuffer(&mut s, 0x8000_0000), -1);
        assert_eq!(
            s.want, GZBUFSIZE,
            "want must be left unchanged on rejection"
        );
    }

    #[test]
    fn gzbuffer_rejects_after_buffers_allocated() {
        let mut s = test_state(GzMode::Write);
        s.size = 8192; // simulate buffers already allocated
        assert_eq!(gzbuffer(&mut s, 4096), -1);
    }

    #[test]
    fn gzbuffer_rejects_wrong_mode() {
        let mut s = test_state(GzMode::None);
        assert_eq!(gzbuffer(&mut s, 4096), -1);
    }

    // -- gzeof ---------------------------------------------------------------

    #[test]
    fn gzeof_reflects_past_when_reading() {
        let mut s = test_state(GzMode::Read);
        assert_eq!(gzeof(&s), 0);
        s.past = true;
        assert_eq!(gzeof(&s), 1);
    }

    #[test]
    fn gzeof_is_zero_when_writing() {
        let mut s = test_state(GzMode::Write);
        s.past = true; // irrelevant for a writer
        assert_eq!(gzeof(&s), 0);
    }

    // -- gztell / gztell64 ---------------------------------------------------

    #[test]
    fn gztell_accounts_for_pending_skip() {
        let mut s = test_state(GzMode::Read);
        s.pos = 100;
        s.skip = 25;
        s.past = false;
        assert_eq!(gztell64(&s), 125);
        // Once past EOF, the pending skip is no longer counted.
        s.past = true;
        assert_eq!(gztell64(&s), 100);
    }

    #[test]
    fn gztell_alias_matches_the_64_bit_form() {
        let mut s = test_state(GzMode::Write);
        s.pos = 42;
        s.skip = 0;
        assert_eq!(gztell(&s), gztell64(&s));
        assert_eq!(gztell(&s), 42);
    }

    // -- gzseek / gzseek64 ---------------------------------------------------

    #[test]
    fn gzseek_forward_read_requests_a_skip() {
        let mut s = test_state(GzMode::Read);
        // how == Look (not COPY) and nothing buffered, so the seek becomes a
        // pending skip rather than a direct file move.
        let ret = gzseek64(&mut s, 50, SEEK_SET);
        assert_eq!(ret, 50);
        assert_eq!(s.skip, 50);
        assert_eq!(s.pos, 0, "position advances lazily as the skip is applied");
    }

    #[test]
    fn gzseek_consumes_buffered_output_first() {
        let mut s = test_state(GzMode::Read);
        s.have = 10;
        s.next = 0;
        // Seek forward 4 with 10 bytes buffered: 4 are consumed from the buffer.
        let ret = gzseek64(&mut s, 4, SEEK_CUR);
        assert_eq!(ret, 4);
        assert_eq!(s.have, 6);
        assert_eq!(s.next, 4);
        assert_eq!(s.pos, 4);
        assert_eq!(s.skip, 0);
    }

    #[test]
    fn gzseek_backward_while_writing_is_rejected() {
        let mut s = test_state(GzMode::Write);
        s.pos = 100;
        // SEEK_SET to 50 -> relative -50 -> writing cannot go back.
        assert_eq!(gzseek64(&mut s, 50, SEEK_SET), -1);
    }

    #[test]
    fn gzseek_rejects_unsupported_whence() {
        let mut s = test_state(GzMode::Read);
        assert_eq!(gzseek64(&mut s, 0, 2 /* SEEK_END */), -1);
    }

    #[test]
    fn gzseek_alias_matches_the_64_bit_form() {
        let mut s = test_state(GzMode::Read);
        s.have = 8;
        assert_eq!(gzseek(&mut s, 3, SEEK_CUR), 3);
        assert_eq!(s.pos, 3);
    }

    #[test]
    fn gzseek_copy_mode_seeks_the_file_directly() {
        let mut s = test_state(GzMode::Read);
        s.how = How::Copy;
        s.have = 3;
        s.next = 7;
        let base = s.file.seek(SeekFrom::Start(100)).unwrap_or(0) as i64;
        // Within the raw area: seek SEEK_CUR by 10.
        let ret = gzseek64(&mut s, 10, SEEK_CUR);
        assert_eq!(ret, 10);
        assert_eq!(s.pos, 10);
        assert_eq!(s.have, 0, "buffered output is discarded on a raw seek");
        assert_eq!(s.in_avail, 0);
        assert_eq!(s.skip, 0);
        // The file moved by (offset - have) = 10 - 3 = 7 from its base.
        let now = s.file.stream_position().unwrap_or(0) as i64;
        assert_eq!(now, base + 7);
    }

    // -- gzrewind ------------------------------------------------------------

    #[test]
    fn gzrewind_seeks_to_start_and_resets_state() {
        let mut s = test_state(GzMode::Read);
        s.start = 0;
        let _ = s.file.seek(SeekFrom::Start(64));
        s.pos = 64;
        s.have = 5;
        s.eof = true;
        s.past = true;
        assert_eq!(gzrewind(&mut s), 0);
        assert_eq!(s.pos, 0);
        assert_eq!(s.have, 0);
        assert!(!s.eof);
        assert!(!s.past);
        assert_eq!(s.how, How::Look);
        assert_eq!(s.file.stream_position().unwrap_or(u64::MAX), 0);
    }

    #[test]
    fn gzrewind_rejects_a_writer() {
        let mut s = test_state(GzMode::Write);
        assert_eq!(gzrewind(&mut s), -1);
    }

    #[test]
    fn gzrewind_rejects_a_serious_error() {
        let mut s = test_state(GzMode::Read);
        s.error(ReturnCode::DataError, Some("corrupt"));
        assert_eq!(gzrewind(&mut s), -1);
    }

    // -- gzoffset / gzoffset64 ----------------------------------------------

    #[test]
    fn gzoffset_subtracts_buffered_input_when_reading() {
        let mut s = test_state(GzMode::Read);
        let base = s.file.seek(SeekFrom::Start(200)).unwrap_or(0) as i64;
        s.in_avail = 30;
        assert_eq!(gzoffset64(&mut s), base - 30);
    }

    #[test]
    fn gzoffset_reports_raw_position_when_writing() {
        let mut s = test_state(GzMode::Write);
        let base = s.file.seek(SeekFrom::Start(128)).unwrap_or(0) as i64;
        s.in_avail = 50; // ignored on the write path
        assert_eq!(gzoffset64(&mut s), base);
    }

    // -- gzdirect ------------------------------------------------------------

    #[test]
    fn gzdirect_reports_transparency_without_lookahead() {
        let mut s = test_state(GzMode::Read);
        // how != Look, so the look-ahead is not triggered.
        s.how = How::Copy;
        s.direct = 1;
        assert_eq!(gzdirect(&mut s), 1);
        s.direct = 0;
        assert_eq!(gzdirect(&mut s), 0);
        s.direct = -1; // forced gzip-only is not "transparent"
        assert_eq!(gzdirect(&mut s), 0);
    }

    #[test]
    fn gzdirect_writer_reports_the_direct_flag() {
        let mut s = test_state(GzMode::Write);
        s.direct = 1;
        assert_eq!(gzdirect(&mut s), 1);
        s.direct = 0;
        assert_eq!(gzdirect(&mut s), 0);
    }

    // -- error accessors -----------------------------------------------------

    #[test]
    fn gzerror_reports_code_and_message() {
        let mut s = test_state(GzMode::Read);
        s.error(ReturnCode::DataError, Some("bad data"));
        let mut code = 0;
        let msg = gzerror(&s, Some(&mut code));
        assert_eq!(code, ReturnCode::DataError.as_c_int());
        assert_eq!(msg, "test: bad data");
    }

    /// The detail string and its FFI-facing `CString` mirror must stay exactly
    /// `"<path>: <msg>"`, byte for byte and in lockstep.
    ///
    /// `GzState::error` builds both by hand — reserving the exact capacity, then
    /// pushing the three pieces — rather than with `format!`, so that an exhausted
    /// allocator can be *reported* (C's `gz_error` checks its `malloc` and
    /// downgrades `state->err` to `Z_MEM_ERROR`) instead of aborting the process.
    /// This test pins the rendering that hand construction has to reproduce,
    /// including the mirror's NUL terminator, which `src/ffi/gz.rs` hands to C as
    /// the `gzerror` message pointer.
    #[test]
    fn error_builds_the_detail_and_its_c_mirror_in_lockstep() {
        let mut s = test_state(GzMode::Read);
        s.error(ReturnCode::DataError, Some("bad data"));

        assert_eq!(s.msg.as_deref(), Some("test: bad data"));
        assert_eq!(
            s.msg.as_deref().map(str::len),
            Some("test".len() + 2 + "bad data".len()),
            "the reserved length must equal the rendered length"
        );
        assert_eq!(
            s.msg_c.as_ref().map(|c| c.as_bytes()),
            Some(&b"test: bad data"[..]),
            "the C mirror must carry the same bytes"
        );
        assert_eq!(
            s.msg_c.as_ref().map(|c| c.as_bytes_with_nul()),
            Some(&b"test: bad data\0"[..]),
            "the C mirror must be NUL-terminated for the FFI gzerror"
        );

        // An empty detail is still rendered as "path: ", matching C's
        // unconditional `"%s%s%s"` of path, ": " and msg.
        s.error(ReturnCode::DataError, Some(""));
        assert_eq!(s.msg.as_deref(), Some("test: "));
        assert_eq!(
            s.msg_c.as_ref().map(|c| c.as_bytes_with_nul()),
            Some(&b"test: \0"[..])
        );

        // Replacing the error must replace both halves together, never leaving a
        // stale mirror behind for C to read.
        s.error(ReturnCode::StreamError, Some("later"));
        assert_eq!(s.msg.as_deref(), Some("test: later"));
        assert_eq!(
            s.msg_c.as_ref().map(|c| c.as_bytes()),
            Some(&b"test: later"[..])
        );

        // Clearing drops both.
        s.error(ReturnCode::Ok, None);
        assert!(s.msg.is_none() && s.msg_c.is_none());
    }

    #[test]
    fn gzerror_reports_out_of_memory_literal() {
        let mut s = test_state(GzMode::Read);
        s.error(ReturnCode::MemError, Some("ignored"));
        assert_eq!(gzerror(&s, None), "out of memory");
    }

    #[test]
    fn gzerror_is_empty_without_a_message() {
        let s = test_state(GzMode::Write);
        let mut code = -999;
        let msg = gzerror(&s, Some(&mut code));
        assert_eq!(code, ReturnCode::Ok.as_c_int());
        assert_eq!(msg, "");
    }

    #[test]
    fn gzclearerr_clears_read_eof_and_error() {
        let mut s = test_state(GzMode::Read);
        s.eof = true;
        s.past = true;
        s.error(ReturnCode::DataError, Some("boom"));
        gzclearerr(&mut s);
        assert!(!s.eof);
        assert!(!s.past);
        assert_eq!(s.err, ReturnCode::Ok);
        assert_eq!(s.msg, None);
    }

    // -- gz_intmax / gt_off --------------------------------------------------

    #[test]
    fn gz_intmax_is_int_max() {
        assert_eq!(gz_intmax(), i64::from(i32::MAX));
    }

    #[test]
    fn gt_off_is_false_on_lp64_targets() {
        // A C `int` (4 bytes) is never as wide as `z_off64_t` (8 bytes) on the
        // targets this crate supports, so the guard is always false regardless
        // of the argument.
        assert!(!gt_off(0));
        assert!(!gt_off(usize::MAX));
    }

    // -- openers (integration over a temporary file) -------------------------

    #[test]
    fn gz_open_applies_mode_and_resets_state() {
        let path =
            std::env::temp_dir().join(format!("blitzy_adhoc_gzopen_{}.gz", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Write open: gzip framing, parsed level, buffers not yet allocated.
        let st = gzopen(&path, "wb9").expect("gzopen wb9");
        assert_eq!(st.mode, GzMode::Write);
        assert_eq!(st.level, 9);
        assert_eq!(st.strategy, Z_DEFAULT_STRATEGY);
        assert_eq!(st.direct, 0);
        assert_eq!(st.want, GZBUFSIZE);
        assert_eq!(st.size, 0);
        drop(st);

        // Read open of the now-existing (empty) file: auto-detect + gz_reset.
        let st = gzopen(&path, "rb").expect("gzopen rb");
        assert_eq!(st.mode, GzMode::Read);
        assert_eq!(st.direct, 1);
        assert_eq!(st.how, How::Look);
        assert_eq!(st.junk, -1);
        assert_eq!(st.pos, 0);
        drop(st);

        // Append open: the transient Append mode is converted to Write.
        let st = gzopen64(&path, "ab").expect("gzopen64 ab");
        assert_eq!(st.mode, GzMode::Write);
        drop(st);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzopen_rejects_an_invalid_mode() {
        let path =
            std::env::temp_dir().join(format!("blitzy_adhoc_badmode_{}.gz", std::process::id()));
        // '+' is rejected before the file is touched. Use `.err()` rather than
        // `.unwrap_err()` because the success type `Box<GzState>` does not
        // implement `Debug` (it owns a `File`/`ZStream`), so `unwrap_err` — which
        // would need to format the `Ok` value on failure — cannot be used here.
        assert_eq!(gzopen(&path, "rb+").err(), Some(ReturnCode::StreamError));
        assert!(!path.exists(), "an invalid mode must not create the file");
    }

    #[test]
    fn gzdopen_uses_a_synthetic_fd_path() {
        let path =
            std::env::temp_dir().join(format!("blitzy_adhoc_gzdopen_{}.gz", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("create temp file");
        let st = gzdopen(file, "wb").expect("gzdopen wb");
        assert_eq!(st.mode, GzMode::Write);
        assert!(
            st.path.starts_with("<fd"),
            "expected a synthetic <fd..> path, got {:?}",
            st.path
        );
        drop(st);
        let _ = std::fs::remove_file(&path);
    }
    // -----------------------------------------------------------------------
    // Allocation-failure parity for the two `gz_open` allocations
    // (AAP §0.6.5). C allocates the state and the retained path name BEFORE it
    // opens anything, so neither failure may abort and neither may have created
    // or truncated a file.
    // -----------------------------------------------------------------------

    /// `try_path_string` replaced `path.display().to_string()`, so it must render
    /// byte-for-byte identically — the value is what `gzerror` reports.
    ///
    /// Both branches of the lossy decoder are covered: a plain UTF-8 path, and (on
    /// unix, where a path is an arbitrary byte string) a path containing an
    /// invalid sequence, which must collapse to exactly one U+FFFD per maximal
    /// invalid subsequence just as `Display` does.
    #[test]
    fn try_path_string_matches_display_to_string() {
        for p in [
            "",
            "/tmp/plain.gz",
            "relative/name.gz",
            "with spaces and-punctuation!.gz",
            "unicode-\u{00e9}\u{4e2d}\u{6587}-\u{1f600}.gz",
        ] {
            let path = Path::new(p);
            assert_eq!(
                try_path_string(path).expect("a short path always allocates"),
                path.display().to_string(),
                "rendering of {p:?} must not change"
            );
        }

        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;

            // Each case pairs valid bytes with invalid ones so the chunk iterator
            // has to interleave `push_str` and the replacement character.
            for raw in [
                &b"/tmp/\xff.gz"[..],
                &b"\xff"[..],
                &b"\xff\xfe\xfd"[..],
                &b"a\xffb\xffc"[..],
                &b"/tmp/\xe2\x82.gz"[..],
                &b"\xf0\x9f\x98\x80\xff"[..],
            ] {
                let path = Path::new(OsStr::from_bytes(raw));
                assert_eq!(
                    try_path_string(path).expect("a short path always allocates"),
                    path.display().to_string(),
                    "lossy rendering of {raw:?} must match Display exactly"
                );
            }
        }
    }

    /// The `<fd:N>` name must be unchanged, and the one reservation it makes must
    /// be large enough that the formatting after it cannot reallocate — otherwise
    /// the "fallible once, then infallible" reasoning in `fd_path` would not hold.
    #[test]
    fn fd_path_is_unchanged_and_its_reservation_covers_every_descriptor() {
        // `"<fd:"` (4) + the widest `i32` rendering, `"-2147483648"` (11) + `">"`
        // (1). C reserves `7 + 3 * sizeof(int)`, which is at least as large.
        const WIDEST_FD_NAME: usize = 16;

        // Checked at compile time: if the reservation ever stopped covering the
        // widest name, `fd_path`'s "one fallible reservation, then infallible
        // formatting" reasoning would break, and no runtime test could be relied
        // on to reach the descriptor value that exposes it.
        const { assert!(FD_PATH_CAPACITY >= WIDEST_FD_NAME) };

        assert_eq!(
            format!("<fd:{}>", i32::MIN).len(),
            WIDEST_FD_NAME,
            "the widest rendering must still be {WIDEST_FD_NAME} bytes"
        );

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = File::open("/dev/null").expect("open /dev/null");
            let name = fd_path(&file).expect("a 19-byte reservation always succeeds");
            assert_eq!(name, format!("<fd:{}>", file.as_raw_fd()));
        }
        #[cfg(not(unix))]
        {
            let file = File::open(std::env::current_exe().expect("exe path"))
                .expect("open the test binary");
            assert_eq!(fd_path(&file).expect("reservation succeeds"), "<fd>");
        }
    }

    /// A failing `gzdopen` must hand the caller's descriptor back **open**.
    ///
    /// In C the adopted `fd` is stored into `state->fd` only at the open/adopt
    /// step, so every earlier `return NULL` — a rejected mode, or either `malloc`
    /// failing — leaves the descriptor untouched and owned by the caller. Rust
    /// would close it by default, because the adopted `File` owns it; `gz_open`
    /// therefore routes every pre-open failure through `abandon_adopted`.
    ///
    /// Observed without `unsafe` via a POSIX guarantee: `open` returns the
    /// **lowest-numbered** unused descriptor. If the failed `gzdopen` had closed
    /// the adopted descriptor, the next `open` would immediately reclaim that
    /// number. Seeing a different number proves the descriptor is still open.
    #[cfg(unix)]
    #[test]
    fn a_failing_gzdopen_leaves_the_callers_descriptor_open() {
        use std::os::unix::io::AsRawFd;

        let adopted = File::open("/dev/null").expect("open /dev/null");
        let adopted_fd = adopted.as_raw_fd();

        // `"q"` names no direction, so `parse_mode` rejects it — the pre-open
        // failure path that is reachable deterministically.
        assert_eq!(
            gzdopen(adopted, "q").err(),
            Some(ReturnCode::StreamError),
            "a mode with no direction must be rejected"
        );

        // The descriptor was deliberately leaked rather than closed, so this
        // `open` must NOT be handed the same number back.
        let probe = File::open("/dev/null").expect("open /dev/null again");
        assert_ne!(
            probe.as_raw_fd(),
            adopted_fd,
            "fd {adopted_fd} was closed by the failed gzdopen; C leaves it open"
        );
    }

    /// `gz_open`'s two allocations precede the `open(2)`, so a write-mode open of
    /// an existing file cannot destroy its contents before those allocations have
    /// succeeded.
    ///
    /// The ordering itself is what protects the file (an out-of-memory condition
    /// cannot be forced deterministically without an allocator hook), so this test
    /// pins the two observable consequences of the reordering: the success path
    /// still truncates exactly as before, and a *pre-open* rejection leaves the
    /// existing contents completely intact.
    #[test]
    fn a_pre_open_failure_leaves_an_existing_file_untouched() {
        // A caller-private directory created with `create_dir` (never
        // `create_dir_all`): it fails rather than adopting a name another user may
        // have planted, and on unix it is mode 0700 from the instant it exists, so
        // there is no window in which the payload below could be enumerated or
        // replaced (CWE-377/CWE-59/CWE-367) — the same discipline the write-side
        // tests apply.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zlib_rs_gzopen_order_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .expect("exclusively create a private directory");
        let path = dir.join("payload.gz");
        std::fs::write(&path, b"PRECIOUS").expect("seed the file");

        // A mode with no direction is rejected before any `OpenOptions::open`.
        assert_eq!(
            gzopen(&path, "q").err(),
            Some(ReturnCode::StreamError),
            "a mode with no direction must be rejected"
        );
        assert_eq!(
            std::fs::read(&path).expect("read back"),
            b"PRECIOUS",
            "a failure before the open must not have truncated the file"
        );

        // Control: the success path still applies O_TRUNC, proving the assertion
        // above is about ordering and not about `"wb"` having stopped truncating.
        let st = gzopen(&path, "wb").expect("wb opens");
        drop(st);
        assert!(
            std::fs::read(&path).expect("read back").is_empty(),
            "the success path must still truncate"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
