//! The `gzclose` family of the gzip file-I/O layer.
//!
//! This module is the faithful Rust port of the C `gzclose.c` dispatcher
//! (`gzclose.c` L11-L23) together with the two direction-specific finalizers
//! that live in the read and write translation units of reference zlib:
//!
//! * [`gzclose_r`] &larr; C `gzclose_r` (`gzread.c` L645-L668)
//! * [`gzclose_w`] &larr; C `gzclose_w` (`gzwrite.c` L667-L700)
//! * [`gzclose`]   &larr; C `gzclose`   (`gzclose.c` L11-L23)
//!
//! In reference zlib these three entry points are deliberately split across
//! three source files so that a program linking only the reader or only the
//! writer does not drag in the other half of the library. That link-time
//! concern does not exist for a single Rust crate, so — per the migration's
//! function-family reorganization — all three are consolidated here while
//! preserving the exact C control flow and return-code contract required for
//! API/FFI parity.
//!
//! # Ownership &amp; RAII
//!
//! The C `gzFile` is an opaque pointer that the `gzclose*` functions *free*.
//! In this safe layer the close functions instead **consume an owned handle**,
//! [`Box<GzState>`]. Returning from a close function drops that box, and the
//! [`Drop`] implementation for [`GzState`] performs the resource teardown that
//! C does by hand:
//!
//! * the input/output [`Vec<u8>`](alloc::vec::Vec) buffers free their storage
//!   (C `free(state->in)` / `free(state->out)`);
//! * the embedded stream runs its own `Drop`, which is the
//!   `inflateEnd`/`deflateEnd` equivalent; and
//! * the owned [`File`](std::fs::File) closes the descriptor (C `close(fd)`).
//!
//! Consequently the functions here contribute only the parts that RAII *cannot*
//! express: the read/write **direction dispatch**, the mandatory write-side
//! `Z_FINISH` flush that finalizes the gzip member (the sliding-window `Drop`
//! deliberately does *not* flush, so that write errors remain observable), and
//! the reconstruction of zlib's integer return-code contract.
//!
//! # The `close(2)` result and the `*_release` variants
//!
//! There is one thing RAII cannot express at all: reference zlib reports a
//! failing `close(2)` as [`Z_ERRNO`](ReturnCode::ErrNo). `gzclose_w` does so with
//! `if (close(state->fd) == -1) ret = Z_ERRNO;` (`gzwrite.c` L695-L696), which
//! *overrides* whatever status the finalize flush accumulated, and `gzclose_r`
//! does so with `return ret ? Z_ERRNO : err;` (`gzread.c` L665-L667) — two
//! spellings of the same rule: a failing close yields `Z_ERRNO`, a succeeding one
//! yields the accumulated status. [`File`]'s [`Drop`] discards the `close(2)`
//! result outright, and this module cannot call `close(2)` itself because it
//! contains no `unsafe`.
//!
//! Each finalizer therefore exists in two forms:
//!
//! * [`gzclose_r`] / [`gzclose_w`] / [`gzclose`] — the public idiomatic API. They
//!   perform every step C performs *except* the fallible close, then drop the
//!   released [`File`] so RAII closes it, discarding the result. An idiomatic
//!   Rust caller who wants to observe a close failure closes the descriptor
//!   itself; these functions keep the infallible, ergonomic contract.
//! * [`gzclose_r_release`] / [`gzclose_w_release`] / [`gzclose_release`] — the
//!   crate-internal forms that perform the identical work but hand the still-open
//!   [`File`] back to the caller alongside the accumulated status, so the C-ABI
//!   shim in `src/ffi/gz.rs` can close it explicitly and apply C's precedence.
//!
//! Splitting the finalizers this way is what keeps the fallible close — and its
//! single `unsafe` `close(2)` call — inside the FFI boundary (AAP §0.6.2, §0.8.1
//! D-6) while still honoring the C return-code contract on the C-ABI path. It
//! also leaves [`GzState`]'s [`Drop`] non-finishing, as AAP §0.8.2 Divergence 5
//! requires: `gzclose` remains mandatory precisely because a destructor cannot
//! surface either the finalize-flush error or the close error.
//!
//! # Direction validation precedes ownership
//!
//! Every function here consumes its handle by value, so the wrong-direction
//! rejection — [`Z_STREAM_ERROR`](ReturnCode::StreamError) — necessarily drops
//! the handle it was given. C does the opposite: its `state->mode` test precedes
//! every `free` and the `close` (`gzread.c` L650-L651, `gzwrite.c` L677-L678), so
//! a wrong-direction call is a pure no-op and the caller still holds a valid,
//! fully live `gzFile` to retry with the correct closer. The C-ABI shim
//! reproduces that by reading the direction through a *borrow* and reclaiming the
//! owning `Box` only once it matches; see `take_for_close` in `src/ffi/gz.rs`.
//! Only an idiomatic Rust caller — who owns the [`Box<GzState>`] by value and
//! therefore cannot double-free or use-after-free it — can reach the consuming
//! rejection path in this module.
//!
//! The `#[unsafe(no_mangle)] extern "C"` shim in `src/ffi/gz.rs` bridges the raw
//! `gzFile` pointer to these owned-handle functions: it validates the pointer
//! (rejecting `NULL` with `Z_STREAM_ERROR`, the check that C performs at the top
//! of each function), validates the direction, and only then reconstructs the
//! `Box`. Because that pointer bookkeeping is confined to the FFI layer, this
//! module contains **zero `unsafe`**, enforced by the `#![deny(unsafe_code)]`
//! attribute below.

#![deny(unsafe_code)]

use std::fs::File;

use crate::constants::FlushMode;
use crate::error::ReturnCode;
use crate::gz::read::finish_read;
use crate::gz::state::{GzMode, GzState};
use crate::gz::write::{gz_comp, gz_zero};

/// Finalizes and closes a gzip file opened for **writing** — port of C
/// `gzclose_w` (`gzwrite.c` L667-L700).
///
/// This is the sole place a write stream is finalized. It flushes any buffered
/// input with `Z_FINISH`, emitting the final DEFLATE block and the gzip trailer
/// (the CRC-32 and ISIZE fields), then releases the handle via [`Drop`].
///
/// The handle is consumed by value: on return the [`Box<GzState>`] is dropped,
/// which frees the I/O buffers, ends the deflate stream, and closes the file
/// descriptor (the RAII replacement for the C `deflateEnd`/`free`/`close`
/// sequence).
///
/// # Return value
///
/// The raw zlib integer return code (see [`ReturnCode::as_c_int`]), following
/// the C precedence exactly:
///
/// * [`Z_STREAM_ERROR`](ReturnCode::StreamError) if the handle was not opened
///   for writing;
/// * otherwise [`Z_OK`](ReturnCode::Ok) on success, or the stream's recorded
///   error code if the pending-seek zero-fill or the `Z_FINISH` flush failed
///   (a later error in this sequence overrides an earlier one, matching C).
///
/// Reference zlib additionally reports [`Z_ERRNO`](ReturnCode::ErrNo) when
/// `close(fd)` itself fails (C L695-L696), overriding the accumulated status.
/// This function closes the descriptor through [`File`]'s [`Drop`], which
/// discards the `close(2)` result, so it never returns `Z_ERRNO` — the value is
/// always the accumulated stream status, which equals the C result whenever the
/// close succeeds. Callers that must observe a close failure use
/// `gzclose_w_release`, which returns the still-open [`File`] instead; that is
/// what the `gzclose`/`gzclose_w` C-ABI shims do, so the C ABI *does* report
/// `Z_ERRNO`. See the module documentation for why the split exists.
pub fn gzclose_w(file: Box<GzState>) -> i32 {
    let (ret, handle) = gzclose_w_release(file);

    // Close through RAII, discarding the `close(2)` result. This is the only
    // difference from the C contract on the idiomatic path.
    drop(handle);

    ret
}

/// Finalizes a gzip write handle and **releases** its file descriptor instead of
/// closing it — the `Z_ERRNO`-capable half of C `gzclose_w`
/// (`gzwrite.c` L667-L700).
///
/// Performs every step of [`gzclose_w`] except the final close: the direction
/// check, the pending-seek zero fill, the mandatory `Z_FINISH` flush, and the
/// buffer/stream teardown. The still-open [`File`] is handed back so the caller
/// can close it explicitly and apply C's precedence — a failing close yields
/// [`Z_ERRNO`](ReturnCode::ErrNo), a succeeding one yields the returned status.
///
/// # Return value
///
/// A pair of the accumulated raw zlib return code (identical to what
/// [`gzclose_w`] returns) and the released descriptor:
///
/// * `(Z_STREAM_ERROR, None)` if the handle was not opened for writing. Nothing
///   is finalized and no descriptor is released — but note that this function
///   consumes the handle, so the descriptor closes when the box drops. The C-ABI
///   shim never reaches this arm: it validates the direction through a borrow
///   before taking ownership, exactly as C tests `state->mode` before any `free`.
/// * `(status, Some(file))` otherwise, where `status` is `Z_OK` or the stream's
///   recorded error if the zero fill or the `Z_FINISH` flush failed (a later
///   error overrides an earlier one, matching C).
pub(crate) fn gzclose_w_release(mut file: Box<GzState>) -> (i32, Option<File>) {
    // C L676-L677: reject a handle that is not open for writing. C performs this
    // test before any `free` or `close`, so the caller's handle survives intact;
    // the C-ABI shim reproduces that by validating the direction through a borrow
    // before reclaiming the box, which is why this arm is unreachable from the
    // C ABI (see the module docs).
    if file.mode != GzMode::Write {
        return (ReturnCode::StreamError.as_c_int(), None);
    }

    // C L671: the running result, defaulting to success.
    let mut ret = ReturnCode::Ok;

    // C L682-L683: honor a pending forward seek by compressing the requested
    // run of zero bytes before finishing. `gz_comp(Z_FINISH)` alone does not
    // drain a pending `skip`, so this step is required for byte-for-byte parity
    // with a zlib stream produced after `gzseek`. `gz_zero` records its own
    // error into `state.err`, which we then surface.
    if file.skip != 0 && gz_zero(&mut file).is_err() {
        ret = file.err;
    }

    // C L685-L686: the mandatory finalize flush. `gz_comp` sets `state.err` on
    // failure, so reading `file.err` here mirrors the C `ret = state->err`.
    if gz_comp(&mut file, FlushMode::Finish).is_err() {
        ret = file.err;
    }

    // C L688-L694: `if (state->size) { deflateEnd(...); free(state->out); }
    // free(state->in); gz_error(state, Z_OK, NULL); free(state->path);` — all
    // performed by the field `Drop`s when the box below is released. Hand the
    // descriptor out *first* so it survives that teardown, leaving the caller to
    // perform C L695-L698 (`if (close(state->fd) == -1) ret = Z_ERRNO;` followed
    // by `free(state)`). Releasing before the drop also preserves C's ordering:
    // the buffers are freed before the descriptor is closed.
    let handle = file.file.release();
    drop(file);

    (ret.as_c_int(), handle)
}

/// Closes a gzip file opened for **reading** — port of C `gzclose_r`
/// (`gzread.c` L645-L668).
///
/// The decompressor holds no unflushed output, so — unlike the write path —
/// there is nothing to finalize; the entire resource release (the C
/// `inflateEnd`/`free`/`close`) is performed by [`Drop`] when the consumed
/// [`Box<GzState>`] goes out of scope.
///
/// # Return value
///
/// The raw zlib integer return code (see [`ReturnCode::as_c_int`]):
///
/// * [`Z_STREAM_ERROR`](ReturnCode::StreamError) if the handle was not opened
///   for reading;
/// * [`Z_BUF_ERROR`](ReturnCode::BufError) if the stream's last recorded error
///   was a buffer error (C preserves this one code across close), otherwise
///   [`Z_OK`](ReturnCode::Ok).
///
/// Reference zlib additionally reports [`Z_ERRNO`](ReturnCode::ErrNo) when
/// `close(fd)` fails (C `return ret ? Z_ERRNO : err;`). This function closes the
/// descriptor through [`File`]'s [`Drop`], which discards the `close(2)` result,
/// so it never returns `Z_ERRNO` — the value is always the accumulated read
/// status, which equals the C result whenever the close succeeds. Callers that
/// must observe a close failure use `gzclose_r_release`, which returns the
/// still-open [`File`] instead; that is what the `gzclose`/`gzclose_r` C-ABI
/// shims do, so the C ABI *does* report `Z_ERRNO`. See the module documentation
/// for why the split exists.
pub fn gzclose_r(file: Box<GzState>) -> i32 {
    let (ret, handle) = gzclose_r_release(file);

    // Close through RAII, discarding the `close(2)` result. This is the only
    // difference from the C contract on the idiomatic path.
    drop(handle);

    ret
}

/// Closes a gzip read handle's stream and **releases** its file descriptor
/// instead of closing it — the `Z_ERRNO`-capable half of C `gzclose_r`
/// (`gzread.c` L645-L668).
///
/// Performs every step of [`gzclose_r`] except the final close: the direction
/// check, the `Z_BUF_ERROR`-preserving status computation, and the buffer/stream
/// teardown. The still-open [`File`] is handed back so the caller can close it
/// explicitly and apply C's `return ret ? Z_ERRNO : err;` precedence.
///
/// # Return value
///
/// A pair of the accumulated raw zlib return code (identical to what
/// [`gzclose_r`] returns) and the released descriptor:
///
/// * `(Z_STREAM_ERROR, None)` if the handle was not opened for reading. Nothing
///   is torn down and no descriptor is released — but note that this function
///   consumes the handle, so the descriptor closes when the box drops. The C-ABI
///   shim never reaches this arm: it validates the direction through a borrow
///   before taking ownership, exactly as C tests `state->mode` before any `free`.
/// * `(status, Some(file))` otherwise, where `status` is
///   [`Z_BUF_ERROR`](ReturnCode::BufError) if that was the stream's last recorded
///   error and [`Z_OK`](ReturnCode::Ok) in every other case.
pub(crate) fn gzclose_r_release(mut file: Box<GzState>) -> (i32, Option<File>) {
    // C L650-L651: reject a handle that is not open for reading. C performs this
    // test before any `free` or `close`, so the caller's handle survives intact;
    // the C-ABI shim reproduces that by validating the direction through a borrow
    // before reclaiming the box, which is why this arm is unreachable from the
    // C ABI (see the module docs).
    if file.mode != GzMode::Read {
        return (ReturnCode::StreamError.as_c_int(), None);
    }

    // C L662: `err = state->err == Z_BUF_ERROR ? Z_BUF_ERROR : Z_OK;`.
    // Delegated to the read side's [`finish_read`], which computes exactly this
    // status (a pending `Z_BUF_ERROR` is preserved, otherwise `Z_OK`), keeping
    // the read-specific finalization owned by `read.rs`.
    let status = finish_read(&file);

    // C L656-L664: `if (state->size) { inflateEnd(...); free(state->out);
    // free(state->in); } gz_error(state, Z_OK, NULL); free(state->path);` — all
    // performed by the field `Drop`s when the box below is released. Hand the
    // descriptor out *first* so it survives that teardown, leaving the caller to
    // perform C L665-L667 (`ret = close(state->fd); free(state); return ret ?
    // Z_ERRNO : err;`). Releasing before the drop also preserves C's ordering:
    // the buffers are freed before the descriptor is closed.
    let handle = file.file.release();
    drop(file);

    (status.as_c_int(), handle)
}

/// Closes a gzip file, dispatching to the reader or writer finalizer — port of
/// C `gzclose` (`gzclose.c` L11-L23).
///
/// The C function first rejects a `NULL` handle with
/// [`Z_STREAM_ERROR`](ReturnCode::StreamError); that check is performed at the
/// FFI boundary (`src/ffi/gz.rs`), where the raw `gzFile` pointer is validated
/// before the owning [`Box<GzState>`] is reconstructed. By the time control
/// reaches this function the handle is therefore known to be non-null.
///
/// Dispatch mirrors C exactly: a read handle goes to [`gzclose_r`] and every
/// other mode goes to [`gzclose_w`]. [`GzMode::Append`] is normalized to
/// [`GzMode::Write`] at open time, so in practice the fall-through arm sees only
/// write handles; routing any non-read mode to [`gzclose_w`] preserves the C
/// `state->mode == GZ_READ ? gzclose_r(file) : gzclose_w(file)` ternary.
///
/// # Return value
///
/// The raw zlib integer return code produced by the selected finalizer. As with
/// [`gzclose_r`] and [`gzclose_w`], the descriptor is closed through [`File`]'s
/// [`Drop`] and a close failure is therefore not reported; `gzclose_release` is
/// the variant that makes it observable, and it is what the C-ABI shim uses.
pub fn gzclose(file: Box<GzState>) -> i32 {
    let (ret, handle) = gzclose_release(file);

    // Close through RAII, discarding the `close(2)` result. This is the only
    // difference from the C contract on the idiomatic path.
    drop(handle);

    ret
}

/// Closes a gzip file and **releases** its file descriptor instead of closing it
/// — the `Z_ERRNO`-capable half of C `gzclose` (`gzclose.c` L11-L23).
///
/// Dispatches to [`gzclose_r_release`] or [`gzclose_w_release`] using the same
/// `state->mode == GZ_READ ? gzclose_r(file) : gzclose_w(file)` ternary C uses,
/// and forwards their `(status, descriptor)` pair unchanged so the caller can
/// perform the fallible close and apply C's precedence.
pub(crate) fn gzclose_release(file: Box<GzState>) -> (i32, Option<File>) {
    match file.mode {
        GzMode::Read => gzclose_r_release(file),
        _ => gzclose_w_release(file),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the close family: direction dispatch, the C return-code
    //! contract, and — for the write path — that the mandatory `Z_FINISH` flush
    //! produces a complete, independently decodable gzip member.
    //!
    //! Write-path tests finalize a stream and then decompress the resulting file
    //! with the reference `flate2` decoder (its pure-Rust `miniz_oxide` backend),
    //! proving that valid gzip (RFC 1952) framing — header, DEFLATE body, and
    //! CRC-32/ISIZE trailer — is emitted. All buffer handling here is safe Rust;
    //! there is **zero `unsafe`** in this module.

    use super::*;

    use crate::gz::state::{GzFile, How};
    use crate::gz::write::gz_write;
    use crate::stream::ZStream;
    use std::fs::File;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use flate2::read::GzDecoder;

    /// Generates a unique, process- and counter-tagged temporary file path. The
    /// `blitzy_adhoc_test_` prefix keeps these files out of any commit and makes
    /// them trivial to clean up.
    fn temp_path(tag: &str) -> PathBuf {
        static CTR: AtomicU32 = AtomicU32::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "blitzy_adhoc_test_gzclose_{tag}_{}_{n}.gz",
            std::process::id()
        ));
        p
    }

    /// Builds a fresh write-mode [`GzState`] backed by a real, truncated temp
    /// file. `size` is left `0` so the finalize path lazily allocates buffers
    /// and initializes the deflate stream, exactly as a real `gzopen` would.
    fn write_state(path: &Path) -> Box<GzState> {
        let file = File::create(path).expect("create writable temp file");
        Box::new(GzState {
            have: 0,
            next: 0,
            pos: 0,
            mode: GzMode::Write,
            file: GzFile::new(file),
            path: path.display().to_string(),
            size: 0,
            want: 8192,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            direct: 0,
            how: How::Look,
            junk: 0,
            again: false,
            in_next: 0,
            in_avail: 0,
            start: 0,
            eof: false,
            past: false,
            level: 6,
            strategy: 0,
            reset: false,
            out_pending: 0,
            skip: 0,
            err: ReturnCode::Ok,
            msg: None,
            msg_c: None,
            strm: ZStream::new(),
        })
    }

    /// Builds a read-mode [`GzState`] backed by a real (materialized) temp file
    /// opened read-only, with the given last-recorded error code so the status
    /// mapping can be exercised.
    fn read_state(path: &Path, err: ReturnCode) -> Box<GzState> {
        File::create(path).expect("materialize temp file");
        let file = File::open(path).expect("open readable temp file");
        Box::new(GzState {
            have: 0,
            next: 0,
            pos: 0,
            mode: GzMode::Read,
            file: GzFile::new(file),
            path: path.display().to_string(),
            size: 0,
            want: 8192,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            direct: 0,
            how: How::Look,
            junk: 0,
            again: false,
            in_next: 0,
            in_avail: 0,
            start: 0,
            eof: false,
            past: false,
            level: 6,
            strategy: 0,
            reset: false,
            out_pending: 0,
            skip: 0,
            err,
            msg: None,
            msg_c: None,
            strm: ZStream::new(),
        })
    }

    /// Reads the whole file at `path`, then removes it, returning the bytes.
    fn read_and_remove(path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).expect("read compressed output");
        let _ = std::fs::remove_file(path);
        bytes
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

    #[test]
    fn gzclose_w_finalizes_empty_member() {
        // Closing a write handle that received no data must still emit a valid,
        // empty gzip member (header + empty block + zeroed trailer).
        let path = temp_path("empty");
        let ret = gzclose_w(write_state(&path));
        assert_eq!(
            ret,
            ReturnCode::Ok.as_c_int(),
            "an empty finalize returns Z_OK"
        );
        let bytes = read_and_remove(&path);
        assert_eq!(
            gunzip(&bytes),
            Vec::<u8>::new(),
            "the empty gzip member decodes to no bytes"
        );
    }

    #[test]
    fn gzclose_finalizes_written_data_via_dispatch() {
        // `gzclose` on a write handle must dispatch to `gzclose_w`, whose
        // `Z_FINISH` flush yields a member decodable by an independent decoder.
        const DATA: &[u8] = b"The quick brown fox jumps over the lazy dog.\n";
        let path = temp_path("data");
        let mut file = write_state(&path);
        let n = gz_write(&mut file, DATA);
        assert_eq!(n, DATA.len(), "all input bytes are accepted");

        let ret = gzclose(file);
        assert_eq!(
            ret,
            ReturnCode::Ok.as_c_int(),
            "finalize through gzclose returns Z_OK"
        );

        let bytes = read_and_remove(&path);
        assert_eq!(gunzip(&bytes), DATA, "the data round-trips exactly");
    }

    #[test]
    fn gzclose_r_returns_ok_on_clean_read() {
        let path = temp_path("read_ok");
        let ret = gzclose_r(read_state(&path, ReturnCode::Ok));
        assert_eq!(ret, ReturnCode::Ok.as_c_int());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzclose_r_preserves_buf_error() {
        // C L662: a lingering Z_BUF_ERROR is the one code preserved across close.
        let path = temp_path("read_buf");
        let ret = gzclose_r(read_state(&path, ReturnCode::BufError));
        assert_eq!(ret, ReturnCode::BufError.as_c_int());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzclose_r_maps_other_errors_to_ok() {
        // C L662: any non-buffer error collapses to Z_OK at close time.
        let path = temp_path("read_data_err");
        let ret = gzclose_r(read_state(&path, ReturnCode::DataError));
        assert_eq!(ret, ReturnCode::Ok.as_c_int());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzclose_r_rejects_a_write_handle() {
        let path = temp_path("wrongdir_r");
        let ret = gzclose_r(write_state(&path));
        assert_eq!(ret, ReturnCode::StreamError.as_c_int());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzclose_w_rejects_a_read_handle() {
        let path = temp_path("wrongdir_w");
        let ret = gzclose_w(read_state(&path, ReturnCode::Ok));
        assert_eq!(ret, ReturnCode::StreamError.as_c_int());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzclose_dispatches_a_read_handle_to_the_reader() {
        // Routing proof: a read handle carrying Z_BUF_ERROR must come back as
        // Z_BUF_ERROR (the reader's contract); had it gone to `gzclose_w` the
        // wrong-direction check would have produced Z_STREAM_ERROR instead.
        let path = temp_path("dispatch_read");
        let ret = gzclose(read_state(&path, ReturnCode::BufError));
        assert_eq!(ret, ReturnCode::BufError.as_c_int());
        let _ = std::fs::remove_file(&path);
    }

    // -- the descriptor-releasing variants ----------------------------------

    /// Confirms a released handle is genuinely **still open**: an `fstat` through
    /// it must succeed. This is what lets the FFI layer call `close(2)` itself and
    /// report a failure as `Z_ERRNO`.
    fn assert_still_open(file: &File) {
        assert!(
            file.metadata().is_ok(),
            "the released descriptor must still be open"
        );
    }

    #[test]
    fn gzclose_w_release_finalizes_and_hands_back_the_descriptor() {
        let path = temp_path("release_w");
        let mut state = write_state(&path);
        assert_eq!(gz_write(&mut state, b"released write path"), 19);

        let (ret, released) = gzclose_w_release(state);
        assert_eq!(ret, ReturnCode::Ok.as_c_int());
        let file = released.expect("a writer releases its descriptor");
        assert_still_open(&file);
        drop(file);

        // The finalize flush ran, so the member on disk is complete.
        assert_eq!(
            gunzip(&read_and_remove(&path)),
            b"released write path".to_vec()
        );
    }

    #[test]
    fn gzclose_r_release_hands_back_the_descriptor_and_maps_the_status() {
        // Clean read -> Z_OK, descriptor released and still open.
        let path = temp_path("release_r_ok");
        let (ret, released) = gzclose_r_release(read_state(&path, ReturnCode::Ok));
        assert_eq!(ret, ReturnCode::Ok.as_c_int());
        assert_still_open(&released.expect("a reader releases its descriptor"));
        let _ = std::fs::remove_file(&path);

        // A pending Z_BUF_ERROR is preserved, exactly as in the non-release form.
        let path = temp_path("release_r_buf");
        let (ret, released) = gzclose_r_release(read_state(&path, ReturnCode::BufError));
        assert_eq!(ret, ReturnCode::BufError.as_c_int());
        assert_still_open(&released.expect("a reader releases its descriptor"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn release_variants_reject_the_wrong_direction_without_releasing() {
        // C tests `state->mode` before any teardown, so a refusal releases nothing
        // and the FFI layer's `finish_close` has no descriptor to close — which is
        // what makes `Z_STREAM_ERROR` outrank a close failure.
        let path = temp_path("release_wrong_r");
        let (ret, released) = gzclose_r_release(write_state(&path));
        assert_eq!(ret, ReturnCode::StreamError.as_c_int());
        assert!(released.is_none(), "a refusal must release no descriptor");
        let _ = std::fs::remove_file(&path);

        let path = temp_path("release_wrong_w");
        let (ret, released) = gzclose_w_release(read_state(&path, ReturnCode::Ok));
        assert_eq!(ret, ReturnCode::StreamError.as_c_int());
        assert!(released.is_none(), "a refusal must release no descriptor");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn gzclose_release_dispatches_like_gzclose() {
        // Reader: routed to the read finalizer, so a pending Z_BUF_ERROR survives.
        let path = temp_path("release_dispatch_r");
        let (ret, released) = gzclose_release(read_state(&path, ReturnCode::BufError));
        assert_eq!(ret, ReturnCode::BufError.as_c_int());
        assert_still_open(&released.expect("reader releases its descriptor"));
        let _ = std::fs::remove_file(&path);

        // Writer: routed to the write finalizer, which finalizes the member.
        let path = temp_path("release_dispatch_w");
        let mut state = write_state(&path);
        assert_eq!(gz_write(&mut state, b"dispatch"), 8);
        let (ret, released) = gzclose_release(state);
        assert_eq!(ret, ReturnCode::Ok.as_c_int());
        assert_still_open(&released.expect("writer releases its descriptor"));
        assert_eq!(gunzip(&read_and_remove(&path)), b"dispatch".to_vec());
    }

    /// The public wrappers must remain byte-for-byte equivalent to the release
    /// variants plus an RAII drop — same status, same on-disk output.
    #[test]
    fn public_wrappers_match_the_release_variants() {
        let payload = b"wrapper equivalence";

        let path_a = temp_path("equiv_wrapper");
        let mut state = write_state(&path_a);
        assert_eq!(gz_write(&mut state, payload), payload.len());
        let wrapper_ret = gzclose_w(state);

        let path_b = temp_path("equiv_release");
        let mut state = write_state(&path_b);
        assert_eq!(gz_write(&mut state, payload), payload.len());
        let (release_ret, released) = gzclose_w_release(state);
        drop(released);

        assert_eq!(wrapper_ret, release_ret);
        assert_eq!(read_and_remove(&path_a), read_and_remove(&path_b));
    }
}
