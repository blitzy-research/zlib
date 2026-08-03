//! Internal state for the gzip file-I/O layer (`gz*` API) of the `zlib-rs`
//! crate.
//!
//! This module is the foundational, self-contained core of the gz layer: it
//! defines [`GzState`] — the safe-Rust port of the C `gz_state` /
//! `gzFile_s` structures declared in `gzguts.h` — together with the open-mode
//! ([`GzMode`]) and read look-ahead ([`How`]) enums. Every other `src/gz/*.rs`
//! module (`open`, `read`, `write`, `close`) operates on the [`GzState`]
//! defined here, so this file is authored first and depends only on the crate's
//! lower layers ([`crate::stream`], [`crate::error`]) plus [`std`].
//!
//! # Relationship to the C `gz_state`
//!
//! The C library models an open gzip file as a heap-allocated `gz_state` whose
//! address is handed back to callers as the opaque `gzFile` pointer. Its first
//! member is an embedded `struct gzFile_s x` holding the three fields the fast
//! `gzgetc()` macro reads directly (`have`, `next`, `pos`); the remaining
//! members are private bookkeeping. This port reproduces **every** field, but
//! replaces the unsafe C idioms with safe Rust equivalents:
//!
//! | C construct                      | Rust translation                       |
//! |----------------------------------|----------------------------------------|
//! | `int fd`                         | owned [`File`] (RAII close)            |
//! | `unsigned char *in` / `*out`     | owned [`Vec<u8>`] buffers             |
//! | `unsigned char *next` (moving)   | [`usize`] index into `out_buf`        |
//! | `char *path`                     | owned [`Vec<u8>`] (raw bytes)         |
//! | `char *msg`                      | [`CString`] + [`Option<String>`]      |
//! | `z_stream strm` (in place)       | owned [`ZStream`] (in place)          |
//! | `free`/`inflateEnd`/`close(fd)`  | `Drop` (RAII)                          |
//!
//! Because the moving output cursor `x.next` is modelled as a `usize` **index**
//! into `out_buf` rather than a raw pointer, all buffer access in the sibling
//! modules is plain, bounds-checked slice indexing: the available output data is
//! the slice `out_buf[next .. next + have]`. This keeps the entire gz layer free
//! of `unsafe`. The `#[repr(C)]` `gzFile_s { have, next, pos }` layout that backs
//! the C `gzgetc` macro, and the opaque-pointer round-trip, are reconstructed
//! **only** at the FFI boundary in `src/ffi/gz.rs` — never here.
//!
//! # Feature gating and safety
//!
//! The gz layer is the one part of the crate that requires the standard library
//! (for file I/O), so the whole `gz` module tree is gated behind the `gz-io`
//! Cargo feature, which implies `std` + `gzip`. This module therefore freely
//! uses [`std::fs::File`] and the `std` prelude. It contains **zero `unsafe`**:
//! all raw-fd, raw-pointer, and C-string handling for the FFI `gz*` entry points
//! lives in `src/ffi/gz.rs`, not here.

use std::ffi::CString;
use std::fs::File;

use crate::error::ReturnCode;
use crate::stream::ZStream;

/// Open mode of a gzip file, mirroring the integer `GZ_*` mode constants in
/// `gzguts.h`.
///
/// The discriminants are **load-bearing**: they match the C
/// `#define GZ_NONE 0`, `GZ_APPEND 1`, `GZ_READ 7247`, and `GZ_WRITE 31153`
/// verbatim. The two large, arbitrary-looking values (`7247`, `31153`) are the
/// exact integers zlib uses as a lightweight integrity check on the structure a
/// caller passes back through the `gz*` API, and they are mirrored by the FFI
/// layer, so they must never be altered.
///
/// # The transient [`GzMode::Append`] value
///
/// [`GzMode::Append`] never persists on an open file. It is produced only while
/// `open.rs` parses the mode string (the `'a'` flag). Immediately after a
/// successful append-open, `open.rs` seeks to end-of-file and overwrites the
/// mode with [`GzMode::Write`] — reproducing the C sequence
/// `LSEEK(fd, 0, SEEK_END); state->mode = GZ_WRITE;`. Consequently a live
/// [`GzState`] is only ever observed in [`GzMode::Read`] or [`GzMode::Write`]
/// (and briefly [`GzMode::None`] during construction/teardown); the public
/// `gzerror`/`gzclearerr` integrity check accepts exactly those two persistent
/// modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum GzMode {
    /// No mode set yet (`GZ_NONE = 0`): the structure is not a usable gzip file.
    None = 0,
    /// Append mode (`GZ_APPEND = 1`): a **transient** value replaced by
    /// [`GzMode::Write`] once the file has been opened and seeked to its end.
    Append = 1,
    /// Reading mode (`GZ_READ = 7247`): the file is open for decompression.
    Read = 7247,
    /// Writing mode (`GZ_WRITE = 31153`): the file is open for compression.
    Write = 31153,
}

/// The read look-ahead state, mirroring the `gz_state.how` values in `gzguts.h`
/// (`#define LOOK 0`, `COPY 1`, `GZIP 2`).
///
/// While reading, the layer must first inspect the leading bytes of the input to
/// decide whether it is looking at a gzip member, and then remember that
/// decision across calls. `How` records that decision. It is surfaced at the gz
/// module root at crate visibility (`pub(crate) use state::How;`, matching this
/// type's `pub(crate)` visibility) so the sibling read driver can name it
/// without reaching into this file directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum How {
    /// `LOOK = 0`: look at the input to decide whether it begins a gzip header.
    Look = 0,
    /// `COPY = 1`: copy input bytes straight through (the data is not gzip).
    Copy = 1,
    /// `GZIP = 2`: decompress the input as a gzip stream.
    Gzip = 2,
}

/// The owned OS file handle, with an explicit **release** path so a close can be
/// made *fallible* — the safe-Rust stand-in for the C `int fd` member.
///
/// # Why a newtype rather than a plain [`File`]
///
/// RAII closes a descriptor from [`Drop`], which cannot report failure: the
/// standard library's `File::drop` discards the `close(2)` result. Reference zlib
/// *does* report it — `gzclose_w` returns [`Z_ERRNO`](ReturnCode::ErrNo) when
/// `close(state->fd) == -1` (`gzwrite.c` L695-L696, overriding whatever status
/// the flush accumulated), and `gzclose_r` returns it via
/// `return ret ? Z_ERRNO : err;` (`gzread.c` L665-L667). Matching that requires
/// handing the descriptor *out* of the state so the C-ABI boundary can close it
/// itself and inspect the result.
///
/// This wrapper makes that hand-off explicit while keeping every existing call
/// site unchanged: it [`Deref`](core::ops::Deref)s to [`File`], so
/// `state.file.read(..)`, `.write(..)`, and `.seek(..)` all still work, and only
/// [`release`](Self::release) can take the handle away.
///
/// # Invariant
///
/// The handle is present for the whole useful life of a [`GzState`].
/// [`release`](Self::release) is called exactly once, by the close finalizers in
/// `src/gz/close.rs`, as the last act before the state is dropped — so no code
/// can observe a released handle. The [`Deref`](core::ops::Deref) impls document
/// that as their panic condition; it is unreachable by construction and covered
/// by tests.
///
/// The one other moment at which the handle is absent is *before* the useful
/// life begins: [`pending`](Self::pending) exists so `gz_open` can allocate the
/// state in C's order — struct first, then the path name, then the `open(2)` —
/// and install the handle only once the file is actually open. That window is
/// confined to `gz_open`'s own body and no [`Deref`](core::ops::Deref) occurs
/// inside it.
pub(crate) struct GzFile {
    /// The owned handle, or [`None`] once [`release`](Self::release) has taken it
    /// for an explicit close.
    handle: Option<File>,
}

impl GzFile {
    /// Wraps an owned [`File`] (the descriptor C stores in `state->fd`).
    #[inline]
    pub(crate) const fn new(handle: File) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// Creates a wrapper with **no** handle yet, for the brief window inside
    /// `gz_open` between allocating the state and opening the file.
    ///
    /// Reference zlib allocates `gz_state` *before* it opens anything
    /// (`gzlib.c`: `malloc(sizeof(gz_state))`, then `malloc` for the path name,
    /// then `open`), so a failure of either allocation returns `NULL` having
    /// touched no file at all. Reproducing that order requires a state value that
    /// can exist before its descriptor does — C simply leaves `state->fd`
    /// uninitialised until the `open` succeeds, and this is the safe equivalent.
    ///
    /// The resulting value must have a real handle installed (by assigning
    /// [`GzFile::new`]) before anything dereferences it; `gz_open` does so on the
    /// only path that returns the state to a caller, so no live [`GzState`] is
    /// ever observable in this condition.
    #[inline]
    pub(crate) const fn pending() -> Self {
        Self { handle: None }
    }

    /// Hands the owned handle to the caller so it can perform an explicit,
    /// *fallible* close, returning [`None`] if it was already released.
    ///
    /// After this the wrapper closes nothing: the descriptor's lifetime belongs
    /// entirely to the returned [`File`] (or to whatever raw descriptor the
    /// caller extracts from it). The state must not be used afterwards.
    #[inline]
    pub(crate) fn release(&mut self) -> Option<File> {
        self.handle.take()
    }
}

impl core::ops::Deref for GzFile {
    type Target = File;

    /// # Panics
    ///
    /// If the handle has already been [`release`](Self::release)d. Unreachable by
    /// construction: release happens only in the close finalizers, immediately
    /// before the owning [`GzState`] is dropped.
    #[inline]
    fn deref(&self) -> &File {
        self.handle
            .as_ref()
            .expect("gz file handle used after release")
    }
}

impl core::ops::DerefMut for GzFile {
    /// # Panics
    ///
    /// If the handle has already been [`release`](Self::release)d; see
    /// [`Deref::deref`](core::ops::Deref::deref).
    #[inline]
    fn deref_mut(&mut self) -> &mut File {
        self.handle
            .as_mut()
            .expect("gz file handle used after release")
    }
}

/// The complete internal state of an open gzip file — the safe-Rust port of the
/// C `gz_state` structure from `gzguts.h`.
///
/// A live `GzState` is owned through a `Box<GzState>` (the gz open functions
/// return `Result<Box<GzState>, _>`), which is the safe analogue of the C
/// opaque `gzFile` pointer. The struct is deliberately **not** `#[repr(C)]`:
/// the C-visible `#[repr(C)] gzFile_s { have, next, pos }` window that backs the
/// fast `gzgetc()` macro is reconstructed separately in `src/ffi/gz.rs`. All
/// fields are `pub(crate)` so the sibling `open`/`read`/`write`/`close` modules
/// can build and drive the state directly, while remaining opaque to external
/// callers.
///
/// The fields are grouped exactly as in the C definition: the exposed
/// `gzFile_s` window, then identity/configuration, the reading-only fields, the
/// writing-only fields, and finally the shared seek/error/stream state.
pub struct GzState {
    // -- exposed window (C `struct gzFile_s x`) ------------------------------
    /// Number of output bytes currently available at [`next`](Self::next)
    /// (C `x.have`).
    ///
    /// Together with [`next`](Self::next) this describes the not-yet-delivered
    /// output as the slice `out_buf[next .. next + have]`.
    pub(crate) have: usize,

    /// Offset into [`out_buf`](Self::out_buf) of the next byte to deliver
    /// (read path) or the next free byte (write path) — the safe index form of
    /// the C moving pointer `x.next`.
    ///
    /// Modelling the cursor as an index (rather than a raw pointer) is what
    /// keeps the gz layer free of `unsafe`: every access is bounds-checked slice
    /// indexing.
    pub(crate) next: usize,

    /// Current position in the uncompressed data stream (C `x.pos`,
    /// `z_off64_t`), i.e. the number of uncompressed bytes read or written so
    /// far. Kept as a signed [`i64`] to match the C 64-bit signed offset.
    pub(crate) pos: i64,

    // -- identity / configuration --------------------------------------------
    /// The open mode of the file (C `int mode`); see [`GzMode`].
    pub(crate) mode: GzMode,

    /// The owned OS file handle (replaces the C `int fd`).
    ///
    /// All I/O is performed through the safe [`std::io::Read`],
    /// [`std::io::Write`], and [`std::io::Seek`] traits on this handle, which the
    /// `GzFile` wrapper exposes by [`Deref`](core::ops::Deref). Dropping the
    /// `GzState` closes the descriptor (the RAII replacement for the C
    /// `close(fd)`) *unless* a close finalizer has released it first so the C-ABI
    /// boundary can perform a fallible close — see `GzFile`.
    pub(crate) file: GzFile,

    /// The path (or synthetic `<fd:N>` name from `gzdopen`) used when building
    /// error messages (C `char *path`).
    ///
    /// Held as **raw bytes**, not a [`String`], because these bytes are handed
    /// back verbatim through the C `gzerror` message. On unix a path is an
    /// arbitrary byte string that need not be UTF-8, and C stores it with no
    /// transformation at all (`gzlib.c` L196-L203: `malloc(len + 1)` plus a
    /// `snprintf(..., "%s", path)`). Decoding it into a [`String`] would replace
    /// every invalid subsequence with U+FFFD and therefore change the bytes a C
    /// caller reads out of `gzerror` (finding M6-07). The lossy rendering still
    /// exists — [`error`](Self::error) produces it for the idiomatic
    /// [`msg`](Self::msg) — but it is no longer what the C mirror is built from.
    pub(crate) path: Vec<u8>,

    /// The size of each allocated I/O buffer, or `0` when the buffers have not
    /// been allocated yet (C `unsigned size`).
    ///
    /// A value of `0` is the sentinel the read/write drivers test to lazily
    /// allocate [`in_buf`](Self::in_buf) / [`out_buf`](Self::out_buf) on first
    /// use, exactly as the C code does.
    pub(crate) size: usize,

    /// The caller-requested buffer size (C `unsigned want`), defaulting to
    /// `GZBUFSIZE` (8192) and adjustable via `gzbuffer`.
    ///
    /// This is the base size; the actual allocations are `want` for the
    /// non-doubled buffer and `want << 1` for the doubled one (see
    /// [`in_buf`](Self::in_buf) / [`out_buf`](Self::out_buf)).
    pub(crate) want: usize,

    /// The input buffer (C `unsigned char *in`).
    ///
    /// On the **write** path this is sized `want << 1` (double) so `gzprintf`
    /// has room to format before compressing.
    pub(crate) in_buf: Vec<u8>,

    /// The output buffer (C `unsigned char *out`).
    ///
    /// On the **read** path this is sized `want << 1` (double) to guarantee room
    /// for `gzungetc` push-back and for the raw pass-through copy. The
    /// [`next`](Self::next)/[`have`](Self::have) window indexes into this
    /// buffer.
    pub(crate) out_buf: Vec<u8>,

    /// Tri-state transparency flag (C `int direct`); the `-1`/`0`/`1` values are
    /// preserved exactly, so this is kept as an [`i32`] rather than a `bool`.
    ///
    /// While **reading**: `1` = transparent/auto-detect (copy input directly),
    /// `-1` = force gzip only (the `'G'` open flag), `0` = currently processing
    /// a gzip member. While **writing**: `0` = gzip framing, `1` = transparent
    /// pass-through (the `'T'` open flag).
    pub(crate) direct: i32,

    // -- reading only --------------------------------------------------------
    /// The read look-ahead state (C `int how`); see [`How`].
    pub(crate) how: How,

    /// Trailing-junk classification while reading (C `int junk`): `-1` = at the
    /// start, `1` = a candidate for trailing junk after a gzip member, `0` =
    /// confirmed inside a real gzip stream.
    pub(crate) junk: i32,

    /// `true` if the last I/O returned `EAGAIN`/`EWOULDBLOCK` on a non-blocking
    /// descriptor (C `int again`).
    ///
    /// When set, [`error`](Self::error) does **not** clear
    /// [`have`](Self::have), so a retry can still make progress.
    pub(crate) again: bool,

    /// Offset into [`in_buf`](Self::in_buf) of the next unconsumed *compressed*
    /// input byte — the safe index form of the C `z_stream.next_in` pointer as
    /// used by the read layer.
    ///
    /// In reference zlib the input-buffer cursor lives on the embedded
    /// `z_stream` (`strm.next_in` / `strm.avail_in`). This crate's idiomatic
    /// [`ZStream`] deliberately carries **no** `next_in`/`avail_in` fields —
    /// input is handed to the engine as a slice on every call and progress is
    /// reported back explicitly — so the read driver (`read.rs`) must itself
    /// remember how much of [`in_buf`](Self::in_buf) is still unconsumed between
    /// calls (compressed input frequently survives a call when the output buffer
    /// fills before the input is exhausted). Together with
    /// [`in_avail`](Self::in_avail) this describes the not-yet-decompressed input
    /// as the slice `in_buf[in_next .. in_next + in_avail]`.
    pub(crate) in_next: usize,

    /// Number of unconsumed *compressed* input bytes available at
    /// [`in_next`](Self::in_next) (C `z_stream.avail_in`, relocated onto the gz
    /// state — see [`in_next`](Self::in_next) for why).
    pub(crate) in_avail: usize,

    /// The file position where the gzip data started, used as the rewind anchor
    /// for `gzrewind`/`gzseek` (C `z_off64_t start`).
    pub(crate) start: i64,

    /// `true` once the end of the input file has been reached (C `int eof`).
    pub(crate) eof: bool,

    /// `true` if a read requested data past the end of the file (C `int past`);
    /// this is precisely the condition `gzeof` reports.
    pub(crate) past: bool,

    // -- writing only --------------------------------------------------------
    /// The compression level in effect for writing (C `int level`).
    pub(crate) level: i32,

    /// The compression strategy in effect for writing (C `int strategy`).
    pub(crate) strategy: i32,

    /// `true` if a `deflateReset` is pending after a `Z_FINISH` (C `int reset`),
    /// so the next write reinitialises the deflate stream for a new member.
    pub(crate) reset: bool,

    /// Number of *compressed* bytes already produced by `deflate` but not yet
    /// handed to the operating system. Together with
    /// [`out_start`](Self::out_start) it names the pending window exactly:
    /// `out_buf[out_start .. out_start + out_pending]`.
    ///
    /// # Why this field exists
    ///
    /// Reference zlib tracks the same thing with the pointer pair
    /// `state->x.next` (first unwritten byte) and `strm->next_out` (one past the
    /// last produced byte): `gz_init` seeds `state->x.next = strm->next_out`, and
    /// `gz_comp`'s drain loop `while (strm->next_out > state->x.next)` advances
    /// `state->x.next += writ` after every successful `write(2)`
    /// (`gzwrite.c` L114-L124). Because the cursor lives on the state, a write
    /// that stops early — a short write, or `EAGAIN`/`EWOULDBLOCK` on a
    /// non-blocking descriptor — leaves the unwritten remainder addressable, and
    /// the next `gz*` call resumes exactly where it stopped.
    ///
    /// This crate's idiomatic [`ZStream`] deliberately carries no
    /// `next_out`/`avail_out` fields (output is handed to the engine as a slice
    /// on every call and progress is reported back explicitly), so the write
    /// driver must remember the unwritten remainder itself — the output-side
    /// counterpart of [`in_next`](Self::in_next)/[`in_avail`](Self::in_avail) on
    /// the read side. Without it a stalled drain would abandon the bytes it had
    /// not yet written, the following `deflate` call would overwrite them, and
    /// the file would receive a spliced, undecodable DEFLATE stream.
    ///
    /// # Why a cursor pair rather than a single front-anchored count
    ///
    /// The window is addressed by an explicit start offset because that is what
    /// makes a run of short writes cost `O(N)` in total, exactly as C's
    /// `state->x.next += writ` does. Reducing it to a single count would force
    /// every partial write to slide the unwritten remainder back down to
    /// `out_buf[0]`, so a destination that accepts one byte at a time would copy
    /// `(N-1) + (N-2) + … + 1` bytes to deliver `N` — quadratic work that a
    /// flow-controlled pipe or socket can provoke from ordinary input. Advancing
    /// an offset instead touches no bytes at all.
    ///
    /// `deflate` receives only the free tail beyond the frontier,
    /// `out_buf[out_start + out_pending .. size]`, so output that is still
    /// pending is never handed back to the engine as scratch space.
    pub(crate) out_pending: usize,

    /// Offset into [`out_buf`](Self::out_buf) of the first *compressed* byte the
    /// operating system has not accepted yet — the write-side counterpart of the
    /// read side's [`next`](Self::next), and the port of C's `state->x.next`
    /// pointer as used by the write path (`gzwrite.c` L114-L127).
    ///
    /// # Invariants
    ///
    /// * The pending window is exactly
    ///   `out_buf[out_start .. out_start + out_pending]`, which is always within
    ///   bounds because [`out_pending`](Self::out_pending) only ever counts bytes
    ///   `deflate` produced into `out_buf[..size]`.
    /// * `out_start + out_pending` is the *produced-bytes frontier* — C's
    ///   `strm->next_out` — and never exceeds `size`. `deflate` is handed
    ///   `out_buf[out_start + out_pending .. size]`, i.e. C's `avail_out`, so it
    ///   can never overwrite a byte the operating system has not accepted.
    /// * `out_start` returns to `0` only when the scratch area is both **full**
    ///   and **fully written** — C's `if (strm->avail_out == 0) { strm->avail_out
    ///   = state->size; strm->next_out = state->out; state->x.next = state->out; }`
    ///   (`gzwrite.c` L125-L128). A drain provoked by a *flush* leaves the
    ///   frontier exactly where it was, precisely as C leaves `next_out`.
    ///   `out_pending == 0` therefore does **not** imply `out_start == 0`: it
    ///   implies only that the operating system has accepted everything produced
    ///   so far, i.e. C's `state->x.next == strm->next_out`.
    ///
    /// Because the reclaim is governed solely by the frontier reaching `size` —
    /// never by how the destination chunked its acceptance — the sequence of
    /// output slices handed to the engine, and hence the compressed byte
    /// sequence, is identical to reference zlib's.
    ///
    /// Only `write.rs` mutates this: its drain loop advances it as the operating
    /// system accepts bytes, and its compress loop performs the reclaim under C's
    /// guard. It is additionally zeroed by `gz_init` when the buffers are
    /// (re)allocated and by `open.rs`'s `gz_reset` when a stream starts over.
    pub(crate) out_start: usize,

    // -- shared --------------------------------------------------------------
    /// The pending seek amount, in bytes (C `z_off64_t skip`): data to skip on
    /// the next read, or zeros to write on the next write. Already rewound if
    /// the seek was backwards.
    pub(crate) skip: i64,

    /// The last error code recorded on this file (C `int err`); see
    /// [`ReturnCode`]. Reported to callers by `gzerror`.
    pub(crate) err: ReturnCode,

    /// The last error message (C `char *msg`), or [`None`] when there is no
    /// message.
    ///
    /// Populated by [`error`](Self::error) as `"{path}: {message}"`, with any
    /// non-UTF-8 bytes in the path rendered lossily (U+FFFD). The exact C bytes
    /// live in [`msg_c`](Self::msg_c); this field is the idiomatic Rust view of
    /// the same message. For
    /// [`ReturnCode::MemError`] this is deliberately left [`None`]: the public
    /// `gzerror` synthesises the literal `"out of memory"` instead of storing a
    /// heap string, matching the C behaviour of not allocating while out of
    /// memory.
    pub(crate) msg: Option<String>,

    /// A NUL-terminated C mirror of [`msg`](Self::msg), kept strictly in
    /// lockstep with it by [`error`](Self::error).
    ///
    /// The idiomatic [`msg`](Self::msg) is a Rust [`String`] (not
    /// NUL-terminated), but the C `gzerror` contract requires a stable
    /// `*const c_char` that stays valid until the next `gz*` call on the handle.
    /// Owning the [`CString`] here lets the FFI `gzerror` shim hand back a
    /// pointer into this field with exactly that lifetime — the pointer is
    /// invalidated only when the next recorded error replaces it, matching C.
    ///
    /// It is [`None`] in precisely the cases [`msg`](Self::msg) is [`None`]
    /// (no error, [`ReturnCode::Ok`], a message-less error, or
    /// [`ReturnCode::MemError`], for which the FFI synthesises the literal
    /// `"out of memory"` without allocating).
    pub(crate) msg_c: Option<CString>,

    /// The embedded (de)compression stream (C `z_stream strm`, stored in place —
    /// not a pointer).
    ///
    /// [`GzState`] owns this [`ZStream`] directly; the read/write drivers drive
    /// (de)compression by calling the crate's `inflate`/`deflate` entry points
    /// on `&mut self.strm`. When the `GzState` is dropped, the `ZStream`'s own
    /// `Drop` performs the `inflateEnd`/`deflateEnd`-equivalent teardown.
    pub(crate) strm: ZStream,
}

impl Drop for GzState {
    /// Releases every resource the `GzState` owns.
    ///
    /// The body is intentionally empty because release is fully handled by RAII:
    ///
    /// * `in_buf` / `out_buf` (`Vec<u8>`)
    ///   free their backing storage — subsuming the C `free(state->in)` /
    ///   `free(state->out)`;
    /// * `strm` (`ZStream`) runs its own `Drop`, which performs
    ///   the `inflateEnd`/`deflateEnd`-equivalent teardown of the engine state;
    ///   and
    /// * `file` (`GzFile`) closes the underlying descriptor —
    ///   subsuming the C `close(fd)` — *unless* a close finalizer already
    ///   `released` it so the C-ABI boundary could close it
    ///   explicitly and report a failure as [`Z_ERRNO`](ReturnCode::ErrNo), which
    ///   reference zlib does and an error-discarding `Drop` cannot.
    ///
    /// This explicit `Drop` therefore exists to *document* the RAII contract
    /// (and to give the sibling modules a single place to reason about
    /// teardown); the compiler-generated field drops would achieve the same
    /// release.
    ///
    /// **Flushing is deliberately not performed here.** For the write path the
    /// final `deflate(..., Z_FINISH)` flush — and, crucially, the reporting of
    /// any I/O error it encounters — is done explicitly by `gzclose_w` in
    /// `close.rs`. This matches zlib's contract, where an explicit `gzclose*`
    /// call is required to finalise output; flushing from `Drop` would silently
    /// swallow write errors that the caller must be able to observe.
    fn drop(&mut self) {
        // No manual work: `Vec`, `ZStream`, and `File` each release themselves.
    }
}

/// Decodes `raw` as UTF-8, replacing each maximal invalid subsequence with a
/// single U+FFFD, **fallibly**.
///
/// This is the [`String::from_utf8_lossy`] transformation with every growth step
/// routed through [`String::try_reserve`], so an exhausted allocator produces
/// [`None`] instead of the process abort an infallible allocation would cause.
/// It exists because the raw bytes are the authority — [`GzState::path`] holds a
/// path exactly as the caller supplied it — while the idiomatic
/// [`GzState::msg`] must still be a Rust [`String`].
fn try_lossy_string(raw: &[u8]) -> Option<String> {
    let mut out = String::new();
    // One reservation for the common all-valid-UTF-8 case; the loop still
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

impl GzState {
    /// Records an error on this file — the port of the internal C
    /// `gz_error(gz_statep, int, const char *)` from `gzlib.c`.
    ///
    /// This is implemented as a method on the state (rather than a free function
    /// in `open.rs`) because it only ever touches this state's `err`, `msg`, and
    /// `have` fields. Placing it here lets `read.rs`, `write.rs`, and `open.rs`
    /// all call `self.error(...)` without depending on one another, which
    /// deliberately breaks what would otherwise be a circular module
    /// dependency.
    ///
    /// The behaviour mirrors the C function exactly:
    ///
    /// 1. Any previous message is dropped (`Vec`/`String` frees it — the safe
    ///    analogue of the C `free(state->msg)`).
    /// 2. If the error is **fatal** — not [`ReturnCode::Ok`], not
    ///    [`ReturnCode::BufError`], and not while a non-blocking retry is
    ///    pending ([`again`](Self::again)) — then [`have`](Self::have) is zeroed
    ///    so the fast `gzgetc()` path fails and control funnels back through the
    ///    slow path that consults `err`.
    /// 3. The error code is recorded in [`err`](Self::err).
    /// 4. With no message, there is nothing more to do.
    /// 5. For [`ReturnCode::MemError`] no message is stored (allocating while
    ///    out of memory is exactly what must be avoided); the public `gzerror`
    ///    returns the literal `"out of memory"` for that code.
    /// 6. Otherwise the stored message is `"{path}: {message}"`, matching the C
    ///    `snprintf(..., "%s%s%s", path, ": ", msg)`. The C-facing
    ///    [`msg_c`](Self::msg_c) is assembled from the **raw** path bytes, so it
    ///    is byte-identical to what C produces even for a path that is not valid
    ///    UTF-8; the idiomatic [`msg`](Self::msg) is a lossy decoding of exactly
    ///    those bytes.
    ///
    /// # Parameters
    ///
    /// * `err` — the [`ReturnCode`] to record.
    /// * `msg` — the human-readable detail to attach, or [`None`] to record only
    ///   the code (as the C code does when passed a `NULL` message).
    pub(crate) fn error(&mut self, err: ReturnCode, msg: Option<&str>) {
        // 1. Drop any previously stored message (Rust frees the old `String`).
        //    The FFI-facing NUL-terminated mirror is cleared in lockstep so a
        //    stale detail pointer can never be handed back by `gzerror`.
        self.msg = None;
        self.msg_c = None;

        // 2. If the error is fatal and we are not mid non-blocking retry, zero
        //    `have` so that the fast `gzgetc()` macro path fails and defers to
        //    the error-aware slow path.
        if err != ReturnCode::Ok && err != ReturnCode::BufError && !self.again {
            self.have = 0;
        }

        // 3. Record the error code.
        self.err = err;

        // 4. No message supplied — done.
        let Some(msg) = msg else {
            return;
        };

        // 5. For an out-of-memory error, do not allocate: leave `msg` as `None`
        //    (the public `gzerror` reports the static "out of memory").
        if err == ReturnCode::MemError {
            return;
        }

        // 6. Construct the "path: message" detail string — fallibly, because C
        //    checks this very allocation and *downgrades the reported error* when
        //    it fails:
        //
        //        if ((state->msg = malloc(strlen(state->path) + strlen(msg) + 3))
        //                == NULL) {
        //            state->err = Z_MEM_ERROR;
        //            return;
        //        }
        //
        //    (`gzlib.c`, `gz_error`.) `format!` would instead terminate the
        //    process, which would be a particularly poor failure mode here: this
        //    is the function every other error path calls to *report* itself, so
        //    an abort would replace a diagnosable error with process death at the
        //    exact moment the caller was about to be told what went wrong.
        //
        //    C's request is `strlen(path) + strlen(msg) + 3` — the two strings
        //    plus `": "` plus the NUL. A Rust `String` carries no NUL, so the
        //    exact requirement is two fewer than C's by one byte for the
        //    terminator; `msg_c` below reserves that byte separately.
        //    The C-facing bytes are assembled first, from the **raw** path, so
        //    that `gzerror` reports exactly what C reports even when the path is
        //    not valid UTF-8 (finding M6-07). A real gzip path and error detail
        //    contain no interior NUL, so `CString::new` succeeds; were one ever
        //    present, `.ok()` yields `None` and the FFI `gzerror` falls back to
        //    the empty string rather than exposing a truncated pointer.
        //
        //    The byte buffer is reserved with room for the terminator, so
        //    `CString::new` — which appends the NUL to the `Vec` it is given —
        //    does not reallocate and cannot abort.
        let detail_len = self.path.len() + 2 + msg.len();
        let mut c_bytes: Vec<u8> = Vec::new();
        if c_bytes.try_reserve_exact(detail_len + 1).is_err() {
            self.err = ReturnCode::MemError;
            return;
        }
        // Infallible from here: the exact capacity is already reserved.
        c_bytes.extend_from_slice(&self.path);
        c_bytes.extend_from_slice(b": ");
        c_bytes.extend_from_slice(msg.as_bytes());
        debug_assert_eq!(c_bytes.len(), detail_len);

        // The idiomatic Rust view is the same bytes, decoded lossily. C makes one
        // allocation because a C string *is* the message; this port needs the
        // second buffer only because it also keeps a native `String`, and it is
        // held to the same "check it, do not abort" rule.
        let Some(detail) = try_lossy_string(&c_bytes) else {
            self.err = ReturnCode::MemError;
            return;
        };

        self.msg_c = CString::new(c_bytes).ok();
        self.msg = Some(detail);
    }

    /// Clears any recorded error, returning the file to the
    /// [`ReturnCode::Ok`] / no-message state.
    ///
    /// This is the state-mutating half of the C `gzclearerr`, which calls
    /// `gz_error(state, Z_OK, NULL)`; the caller (`gzclearerr` in `open.rs`) is
    /// responsible for also clearing [`eof`](Self::eof)/[`past`](Self::past) on
    /// the read path. It is likewise used to reset the error state at the start
    /// of the `gz*` entry points.
    pub(crate) fn clear_error(&mut self) {
        self.error(ReturnCode::Ok, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `GzState` suitable for exercising the state-only methods.
    ///
    /// [`GzState::error`] / [`GzState::clear_error`] touch only `err`, `msg`,
    /// `have`, `again`, and `path`, so the remaining fields are filled with
    /// innocuous defaults. A real, always-openable file — the running test
    /// binary, falling back to `/dev/null` — satisfies the [`File`] field with
    /// **no `unsafe`** and no external test dependency; no I/O is performed on
    /// it here.
    fn test_state(path: &str) -> GzState {
        let file = std::env::current_exe()
            .ok()
            .and_then(|p| File::open(p).ok())
            .or_else(|| File::open("/dev/null").ok())
            .expect("a real, openable file is required for the test GzState");
        GzState {
            have: 0,
            next: 0,
            pos: 0,
            mode: GzMode::Read,
            file: GzFile::new(file),
            path: path.as_bytes().to_vec(),
            size: 0,
            want: 0,
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
            level: 0,
            strategy: 0,
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

    #[test]
    fn gz_mode_discriminants_match_gzguts_h() {
        // Exact `#define`s from gzguts.h — mirrored by the FFI layer.
        assert_eq!(GzMode::None as i32, 0);
        assert_eq!(GzMode::Append as i32, 1);
        assert_eq!(GzMode::Read as i32, 7247);
        assert_eq!(GzMode::Write as i32, 31153);
    }

    #[test]
    fn how_discriminants_match_gzguts_h() {
        // Exact LOOK / COPY / GZIP values from gzguts.h.
        assert_eq!(How::Look as u8, 0);
        assert_eq!(How::Copy as u8, 1);
        assert_eq!(How::Gzip as u8, 2);
    }

    #[test]
    fn error_fatal_clears_have_and_sets_message() {
        let mut s = test_state("archive.gz");
        s.have = 42;
        s.error(ReturnCode::DataError, Some("bad data"));
        assert_eq!(s.err, ReturnCode::DataError);
        assert_eq!(s.have, 0, "a fatal error must clear `have`");
        assert_eq!(s.msg.as_deref(), Some("archive.gz: bad data"));
    }

    #[test]
    fn error_ok_preserves_have_and_drops_previous_message() {
        let mut s = test_state("archive.gz");
        s.have = 42;
        s.msg = Some(String::from("stale message"));
        s.error(ReturnCode::Ok, None);
        assert_eq!(s.err, ReturnCode::Ok);
        assert_eq!(s.have, 42, "Z_OK is not fatal, so `have` is preserved");
        assert_eq!(s.msg, None, "the previously stored message must be dropped");
    }

    #[test]
    fn error_buf_error_preserves_have() {
        let mut s = test_state("archive.gz");
        s.have = 42;
        s.error(ReturnCode::BufError, None);
        assert_eq!(s.err, ReturnCode::BufError);
        assert_eq!(
            s.have, 42,
            "Z_BUF_ERROR is explicitly treated as non-fatal for `have`"
        );
        assert_eq!(s.msg, None);
    }

    #[test]
    fn error_again_preserves_have_even_when_fatal() {
        let mut s = test_state("archive.gz");
        s.have = 42;
        s.again = true;
        s.error(ReturnCode::DataError, Some("would block"));
        assert_eq!(s.err, ReturnCode::DataError);
        assert_eq!(
            s.have, 42,
            "a pending non-blocking retry (`again`) must preserve `have`"
        );
        assert_eq!(s.msg.as_deref(), Some("archive.gz: would block"));
    }

    #[test]
    fn error_mem_error_stores_no_message() {
        let mut s = test_state("archive.gz");
        s.have = 42;
        s.error(ReturnCode::MemError, Some("ignored detail"));
        assert_eq!(s.err, ReturnCode::MemError);
        // MemError is fatal (not Ok / not BufError), so `have` is still cleared.
        assert_eq!(s.have, 0);
        // ...but no message is allocated: the public `gzerror` synthesises the
        // literal "out of memory" for this code.
        assert_eq!(s.msg, None);
    }

    #[test]
    fn error_message_uses_path_prefix() {
        let mut s = test_state("/tmp/data.gz");
        s.error(ReturnCode::StreamError, Some("boom"));
        // Format is exactly "{path}: {msg}" (C snprintf "%s%s%s", path, ": ", msg).
        assert_eq!(s.msg.as_deref(), Some("/tmp/data.gz: boom"));
    }

    /// The C-facing message is assembled from the **raw** path bytes, so a path
    /// that is not valid UTF-8 reaches `gzerror` unchanged (finding M6-07).
    ///
    /// C stores the path verbatim (`gzlib.c` L196-L203) and renders the message
    /// with `snprintf(..., "%s%s%s", path, ": ", msg)`, so the bytes a C caller
    /// reads back are the caller's own. Decoding the path into a Rust `String`
    /// first would replace each invalid run with U+FFFD (`ef bf bd`) and change
    /// those bytes.
    #[test]
    fn the_c_message_carries_the_raw_path_bytes() {
        let mut s = test_state("placeholder");
        s.path = b"/tmp/\xff\xfe.gz".to_vec();
        s.error(ReturnCode::DataError, Some("boom"));

        let c_msg = s
            .msg_c
            .as_ref()
            .expect("a C mirror is stored for a non-OOM error")
            .as_bytes();
        assert_eq!(
            c_msg, b"/tmp/\xff\xfe.gz: boom",
            "the C message is the raw path bytes, then \": \", then the detail"
        );
        assert!(
            !c_msg.windows(3).any(|w| w == [0xef, 0xbf, 0xbd]),
            "no U+FFFD may appear anywhere in the C message"
        );

        // The idiomatic Rust view is exactly those bytes, decoded lossily.
        assert_eq!(
            s.msg.as_deref(),
            Some(String::from_utf8_lossy(c_msg).as_ref()),
            "`msg` is the lossy decoding of the C bytes"
        );
        assert_ne!(
            s.msg.as_deref().map(str::as_bytes),
            Some(c_msg),
            "the two renderings genuinely differ here, so the C mirror cannot be \
             the Rust string re-encoded"
        );
    }

    #[test]
    fn clear_error_resets_code_and_message() {
        let mut s = test_state("archive.gz");
        s.error(ReturnCode::DataError, Some("bad"));
        assert_eq!(s.err, ReturnCode::DataError);
        assert!(s.msg.is_some());

        s.clear_error();
        assert_eq!(s.err, ReturnCode::Ok);
        assert_eq!(s.msg, None);
    }

    // -----------------------------------------------------------------------
    // The `GzFile` release hand-off
    // -----------------------------------------------------------------------

    /// [`GzFile`] must behave as the owned handle everywhere except for the one
    /// explicit hand-off: [`GzFile::release`] yields the [`File`] exactly once, and
    /// the released handle is still open so the FFI layer can close it itself and
    /// observe the result (C `close(state->fd)`).
    #[test]
    fn gz_file_releases_its_handle_exactly_once() {
        let mut s = test_state("archive.gz");

        // Before release the wrapper is transparent: `Deref` reaches the `File`.
        assert!(
            s.file.metadata().is_ok(),
            "Deref must reach a live File before release"
        );

        let released = s
            .file
            .release()
            .expect("the first release yields the handle");
        assert!(
            released.metadata().is_ok(),
            "the released descriptor must still be open"
        );

        // A second release yields nothing: the hand-off is single-shot, so the
        // descriptor can never be closed twice through this path.
        assert!(
            s.file.release().is_none(),
            "release must be single-shot so no double close is possible"
        );

        // Dropping the state after a release must not attempt any close — the
        // descriptor's lifetime now belongs entirely to `released`.
        drop(s);
        assert!(
            released.metadata().is_ok(),
            "dropping a released state must not close the handed-off descriptor"
        );
    }

    /// Deref-after-release panics with the documented message. Unreachable in
    /// production (release happens only as the last act of a close finalizer), but
    /// pinned so the invariant fails loudly rather than silently if that changes.
    #[test]
    #[should_panic(expected = "gz file handle used after release")]
    fn deref_after_release_panics() {
        let mut s = test_state("archive.gz");
        let _released = s.file.release().expect("handle present");
        let _ = s.file.metadata();
    }

    // -----------------------------------------------------------------------
    // The non-finishing `Drop` contract
    //
    // `impl Drop for GzState` (above) is deliberately empty of finishing logic:
    // a destructor cannot surface a deferred compression or I/O error, so the
    // final `deflate(..., Z_FINISH)` flush and the gzip trailer are emitted only
    // by an explicit `gzclose`/`gzclose_w`. That mirrors reference zlib, whose
    // `gzclose_w` performs `gz_comp(state, Z_FINISH)` (`gzwrite.c` L685-L686)
    // and reports its error to the caller.
    //
    // The decision reads like a defect to a Rust engineer, and "fixing" it would
    // silently change the bytes this library writes without breaking compilation
    // or any round-trip that closes properly. The pair of tests below pin it from
    // both sides: the negative case proves a dropped writer leaves an
    // *unfinished* member on disk, and the control case proves the very same data
    // round-trips once `gzclose_w` is called. If a future `Drop` ever finished the
    // stream, the negative test fails.
    //
    // Both tests write to a uniquely named temporary file inside a private,
    // freshly created directory guarded by [`TempGz`], which removes the file and
    // the directory on every exit path — normal return, early return, or unwind.
    // -----------------------------------------------------------------------

    use crate::gz::close::gzclose_w;
    use crate::gz::write::gz_write;
    use std::io::Read as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Reduce an arbitrary ambient string to a single safe path component.
    ///
    /// `CLONE_INDEX` is ambient input read from the environment, so its value is
    /// outside this crate's control. Interpolating it into a path unfiltered is a
    /// directory-traversal defect (CWE-22): a value such as
    /// `slot/../../security_target` escapes the temporary directory lexically and
    /// resolves somewhere else entirely.
    ///
    /// Only ASCII alphanumerics, `_`, and `-` survive. That drops every character
    /// which could end the component or refer to a parent — `/`, `\`, `.` (so
    /// `..` collapses away entirely), `:`, NUL, and every non-ASCII byte. The
    /// result is truncated so an absurdly long value cannot push the path past a
    /// filesystem limit, and an input that filters down to nothing becomes `x`, so
    /// the caller always receives a usable component.
    fn safe_component(raw: &str) -> String {
        let filtered: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(32)
            .collect();
        if filtered.is_empty() {
            "x".to_owned()
        } else {
            filtered
        }
    }

    /// Create `path` as a new, private directory, failing if anything is already
    /// there.
    ///
    /// Non-recursive on purpose: unlike `create_dir_all`, this reports
    /// [`std::io::ErrorKind::AlreadyExists`] when the name is taken — including
    /// when it is taken by a symlink someone else planted — which is what lets
    /// [`TempGz::new`] move to the next candidate instead of following the link or
    /// deleting it. On Unix the `0o700` mode is handed to `mkdir(2)` itself, so the
    /// directory is never even briefly group- or world-accessible and there is no
    /// `set_permissions` window to race. Both properties describe the moment of
    /// creation; on non-Unix targets the mode is the platform default.
    fn create_private_dir(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new().mode(0o700).create(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::DirBuilder::new().create(path)
        }
    }

    /// An owned temporary `.gz` file inside a private directory, both removed when
    /// the guard is dropped.
    ///
    /// The **directory** carries the uniqueness: the `blitzy_adhoc_test_` prefix
    /// (so nothing here can be mistaken for a tracked artifact), a
    /// [`safe_component`]-sanitized `CLONE_INDEX`, the process id, a monotonic
    /// counter, and a retry ordinal. That keeps it distinct across parallel test
    /// threads, across concurrent `cargo test` invocations, and across sibling
    /// clones of this repository sharing one `/tmp`.
    ///
    /// Creating the directory with create-new semantics is what makes the file
    /// inside it safe to write. Nothing pre-existing is ever removed — an occupied
    /// candidate name is skipped rather than deleted — so a symlink or file planted
    /// at a predictable path is neither destroyed nor followed. The payload name is
    /// then predictable *within* a directory that did not exist a moment earlier
    /// and, on Unix, was owner-only from `mkdir(2)` onwards; the file is opened
    /// `create_new` regardless, so a name that somehow is taken fails loudly rather
    /// than being truncated. These are creation-time properties: the guard holds
    /// paths rather than open handles, so it makes no claim about the directory
    /// still being the same object later, and on non-Unix targets the directory's
    /// mode is whatever the platform applies.
    struct TempGz {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempGz {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU32 = AtomicU32::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let clone = safe_component(&std::env::var("CLONE_INDEX").unwrap_or_default());
            let tag = safe_component(tag);
            let pid = std::process::id();
            let base = std::env::temp_dir();

            // Retry only advances the candidate name; it never deletes.
            for attempt in 0..64u32 {
                let candidate = base.join(format!(
                    "blitzy_adhoc_test_gzdrop_{tag}_{clone}_{pid}_{n}_{attempt}"
                ));
                match create_private_dir(&candidate) {
                    Ok(()) => {
                        let path = candidate.join("payload.gz");
                        return Self {
                            dir: candidate,
                            path,
                        };
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("failed to create the private temp directory: {e}"),
                }
            }
            panic!("could not find an unused temp directory name after 64 attempts");
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn exists(&self) -> bool {
            self.path.exists()
        }

        /// The bytes currently on disk, or empty if the file does not exist.
        fn bytes(&self) -> Vec<u8> {
            std::fs::read(&self.path).unwrap_or_default()
        }
    }

    impl Drop for TempGz {
        fn drop(&mut self) {
            // The recursion rests on `dir` not having existed before `TempGz::new`
            // created it with create-new semantics, so the removal set starts from a
            // path this guard brought into existence rather than one it adopted, and
            // on Unix from one no other user could enter. It does not rest on the
            // path still resolving to that same directory, which a path-based guard
            // cannot establish. Best effort on every route out, including an
            // unwinding one: failing to clean up must never mask the original
            // failure.
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `safe_component` must collapse every traversal and separator form to a
    /// single harmless component.
    ///
    /// The first case is the traversal shape that matters: interpolated raw,
    /// `slot/../../security_target` names a path two levels above the temporary
    /// directory. Sanitized, it can only ever name a child of that directory.
    #[test]
    fn safe_component_neutralizes_traversal_and_separators() {
        for raw in [
            "slot/../../security_target",
            "../../../etc/passwd",
            "..",
            ".",
            "/absolute",
            "back\\slash",
            "c:\\windows\\system32",
            "with space",
            "semi;colon",
            "new\nline",
            "nul\0byte",
            "tilde~",
            "dollar$sign",
            "\u{00e9}\u{4f60}\u{597d}",
        ] {
            let got = safe_component(raw);
            assert!(!got.is_empty(), "{raw:?} must yield a usable component");
            assert!(
                got.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{raw:?} yielded {got:?}, which still contains a disallowed character"
            );
            assert!(
                !got.contains(".."),
                "{raw:?} yielded {got:?}, still traversing"
            );
            // The decisive property: joining it descends exactly one level.
            let joined = Path::new("/tmp").join(&got);
            assert_eq!(
                joined.parent(),
                Some(Path::new("/tmp")),
                "{raw:?} yielded {got:?}, which does not stay one level below the base"
            );
        }
    }

    /// Inputs that filter down to nothing, and inputs that are far too long, must
    /// still produce a usable bounded component.
    #[test]
    fn safe_component_is_total_and_bounded() {
        assert_eq!(safe_component(""), "x", "an unset variable must still work");
        assert_eq!(safe_component("///"), "x", "separators only");
        assert_eq!(safe_component("...."), "x", "dots only");
        assert_eq!(safe_component("\u{4f60}\u{597d}"), "x", "non-ASCII only");

        let long = "a".repeat(4096);
        assert_eq!(
            safe_component(&long).len(),
            32,
            "an over-long value must be truncated"
        );

        // Characters that are safe are preserved in order.
        assert_eq!(safe_component("clone-07_b"), "clone-07_b");
    }

    /// A [`TempGz`] must own a freshly created, private, single-level child of the
    /// system temporary directory, and must place its payload inside it.
    #[test]
    fn temp_gz_uses_a_private_new_directory_and_create_new_file() {
        let a = TempGz::new("hygiene");
        assert!(a.dir.is_dir(), "the private directory must exist");
        assert_eq!(
            a.dir.parent(),
            Some(std::env::temp_dir().as_path()),
            "the private directory must sit directly under the temp directory"
        );
        assert_eq!(
            a.path().parent(),
            Some(a.dir.as_path()),
            "the payload must live inside the private directory"
        );
        assert!(
            !a.exists(),
            "`TempGz::new` must not create the payload file; `create_new` does"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&a.dir)
                .expect("stat the private directory")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "the directory must be owner-only from the moment it exists"
            );
        }

        // Two guards taken back to back must never collide.
        let b = TempGz::new("hygiene");
        assert_ne!(a.dir, b.dir, "concurrent guards must be distinct");

        // Create-new semantics: the directory name is now taken, so a second
        // attempt at exactly that path must be refused rather than reused.
        let err = create_private_dir(&a.dir).expect_err("the path is already taken");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::AlreadyExists,
            "an occupied name must report AlreadyExists so the caller can skip it"
        );

        // The state builder must open the payload with create-new semantics, and a
        // second attempt on the same path must therefore be refused rather than
        // truncating what the first one wrote.
        let state = drop_contract_write_state(a.path());
        assert!(a.exists(), "the payload file must now exist");
        drop(state);
        let second = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(a.path());
        assert_eq!(
            second.expect_err("the payload already exists").kind(),
            std::io::ErrorKind::AlreadyExists,
            "the payload must never be reopened with truncation"
        );

        let dir_a = a.dir.clone();
        drop(a);
        assert!(!dir_a.exists(), "Drop must remove the private directory");
    }

    /// Builds a fresh write-mode [`GzState`] backed by a real, truncated file.
    ///
    /// `size` is left `0` so the write path lazily allocates its buffers and
    /// initializes the deflate engine on first use, exactly as a real `gzopen`
    /// would — matching C's `state->size = 0` sentinel (`gzguts.h` L172).
    fn drop_contract_write_state(path: &Path) -> Box<GzState> {
        // `create_new(true)` rather than a truncating `File::create`: the file must
        // not already exist, and if something is squatting on the name this must
        // fail loudly instead of truncating it. That is the guarantee relied on
        // here; `TempGz` additionally created the enclosing directory fresh and, on
        // Unix, owner-only, which makes a squatter unlikely rather than impossible.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create a new writable temp file");
        Box::new(GzState {
            have: 0,
            next: 0,
            pos: 0,
            mode: GzMode::Write,
            file: GzFile::new(file),
            path: path.as_os_str().as_encoded_bytes().to_vec(),
            size: 0,
            want: DROP_CONTRACT_WANT,
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
            out_start: 0,
            skip: 0,
            err: ReturnCode::Ok,
            msg: None,
            msg_c: None,
            strm: ZStream::new(),
        })
    }

    /// The write buffer size used by [`drop_contract_write_state`].
    ///
    /// `gz_init` copies `want` into `size`, and `size` is what `gz_write`
    /// compares each incoming write against, so this single constant fixes both
    /// the state under test and the branch-coverage invariant asserted over it.
    const DROP_CONTRACT_WANT: usize = 8192;

    /// Number of payload bytes the two `Drop`-contract tests write.
    ///
    /// Deliberately far larger than the [`DROP_CONTRACT_WANT`]-byte `want`
    /// buffer so that several `gz_comp(Z_NO_FLUSH)` rounds reach the file before
    /// the state is dropped. A payload small enough to sit entirely in `in_buf`
    /// would leave only the 10-byte gzip header on disk, and the negative test
    /// would then prove merely "nothing was written" rather than the much
    /// stronger and more relevant "a real member was started and left
    /// unfinished".
    const DROP_CONTRACT_LEN: usize = 500_000;

    /// Incompressible pseudo-random payload from a fixed-seed LCG.
    ///
    /// Incompressible on purpose: highly compressible input would let `deflate`
    /// buffer almost everything internally under `Z_NO_FLUSH`, so little or
    /// nothing would reach the file before the drop. Deterministic on purpose:
    /// the assertions must not depend on a random seed.
    fn drop_contract_payload() -> Vec<u8> {
        let mut lcg: u32 = 0x1234_5678;
        (0..DROP_CONTRACT_LEN)
            .map(|_| {
                lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (lcg >> 24) as u8
            })
            .collect()
    }

    /// Feeds `payload` through `gz_write` in `chunk` -sized pieces, asserting
    /// full acceptance of each.
    ///
    /// The chunk size selects which branch of `gz_write` runs, and both matter
    /// here. A chunk smaller than `state.size` takes the buffer-then-
    /// compress-when-full branch (C L205-L226); a chunk at least as large takes
    /// the feed-the-engine-directly branch (C L229-L247). The `Drop` contract must
    /// hold on both, so [`assert_bare_drop_leaves_member_unfinished`] drives each.
    fn feed(state: &mut GzState, payload: &[u8], chunk: usize) {
        for piece in payload.chunks(chunk) {
            assert_eq!(
                gz_write(state, piece),
                piece.len(),
                "gz_write must accept the whole chunk"
            );
        }
    }

    /// Decompresses a complete gzip member, or reports how far it got.
    ///
    /// Uses the reference `flate2` decoder (its pure-Rust `miniz_oxide` backend),
    /// so "is this a valid gzip member?" is answered by an independent
    /// implementation rather than by this crate's own inflate.
    fn gunzip(bytes: &[u8]) -> (std::io::Result<()>, Vec<u8>) {
        let mut out = Vec::new();
        let result = flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map(|_| ());
        (result, out)
    }

    /// Drives one bare-drop scenario at the given `gz_write` chunk size and
    /// asserts every property of an unfinished member.
    fn assert_bare_drop_leaves_member_unfinished(tag: &str, chunk: usize) {
        let temp = TempGz::new(tag);
        let payload = drop_contract_payload();

        {
            let mut state = drop_contract_write_state(temp.path());
            feed(&mut state, &payload, chunk);
            // The whole point: no `gzclose_w`, no `gzflush`, no `Z_FINISH`. Only
            // `Drop` runs, and `Drop` must not finish the member.
            drop(state);
        }

        let bytes = temp.bytes();

        // A real member was *started*: the gzip magic and the DEFLATE method byte
        // reached the file, so this test is not passing merely because nothing was
        // written.
        assert!(
            bytes.len() > 1024,
            "the writer must have flushed real compressed output before the drop,              got {} bytes",
            bytes.len()
        );
        assert_eq!(
            &bytes[..3],
            &[0x1f, 0x8b, 0x08],
            "RFC 1952 magic and CM=deflate must be present"
        );
        // The payload must be genuinely incompressible for the assertions below to
        // mean anything: a compressible payload would fit entirely in the write
        // buffer and reach disk only at close, degrading this test into the far
        // weaker "nothing was written". Pinning the *property* rather than the
        // generator's seed keeps the test honest if the payload is ever changed.
        assert!(
            bytes.len() * 5 > payload.len() * 4,
            "payload compressed to {} of {} bytes, so it is not incompressible;              this test needs an incompressible payload to prove that a real member              was started and then abandoned",
            bytes.len(),
            payload.len()
        );

        // ...and it was left *unfinished*. An independent gzip decoder cannot
        // complete it, because the final DEFLATE block and the 8-byte
        // CRC-32/ISIZE trailer were never emitted.
        let (result, recovered) = gunzip(&bytes);
        let error = result.expect_err(
            "a dropped-without-close writer must not leave a decodable gzip member;              if this now succeeds, `Drop for GzState` has started finishing the              stream, which silently swallows the write errors `gzclose_w` exists              to report",
        );
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "the member must fail as truncated, not as corrupt: {error}"
        );
        assert!(
            recovered.len() < payload.len(),
            "an unfinished member cannot yield the whole payload ({} of {})",
            recovered.len(),
            payload.len()
        );
        // Whatever the decoder did recover must still be a correct prefix — the
        // bytes already on disk are valid, they are merely incomplete.
        assert_eq!(
            recovered.as_slice(),
            &payload[..recovered.len()],
            "the truncated member's contents must be a prefix of the payload"
        );

        // The trailer specifically is absent. Reference zlib appends CRC-32 then
        // ISIZE, both little-endian; neither can be sitting at the end of the
        // file, because `Drop` emitted no trailer at all.
        let mut trailer = [0u8; 8];
        trailer[..4].copy_from_slice(&crate::checksum::crc32::crc32(0, &payload).to_le_bytes());
        trailer[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        assert_ne!(
            &bytes[bytes.len() - 8..],
            &trailer[..],
            "no CRC-32/ISIZE trailer may be present after a bare drop"
        );

        assert!(
            temp.exists(),
            "the temporary file must still exist for the guard to remove"
        );
    }

    #[test]
    fn dropping_a_writer_without_gzclose_leaves_the_member_unfinished() {
        // Both `gz_write` branches: buffered small writes, and a single write
        // large enough to be handed straight to the engine. Neither may end up
        // with a finished member, because neither calls `gzclose_w`.
        // Both `gz_write` branches must be exercised. The branch is selected by
        // `buf.len() < state.size` (`src/gz/write.rs`, porting `gzwrite.c`
        // L205-L247): a short write is staged in `in_buf` and drained by
        // `gz_comp`, while a write at or above the buffer size is handed to
        // `gz_comp_slice` directly. A bare drop must leave the member unfinished
        // on *either* path, so the table below is asserted to straddle the
        // predicate -- deleting a row fails the invariant rather than silently
        // shrinking coverage.
        const SCENARIOS: [(&str, usize); 2] = [
            ("nofinish_buffered", 3_000),
            ("nofinish_direct", DROP_CONTRACT_LEN),
        ];

        let threshold = DROP_CONTRACT_WANT;
        assert!(
            SCENARIOS.iter().any(|&(_, chunk)| chunk < threshold),
            "no scenario exercises gz_write's buffered branch (chunk < {threshold})"
        );
        assert!(
            SCENARIOS.iter().any(|&(_, chunk)| chunk >= threshold),
            "no scenario exercises gz_write's direct branch (chunk >= {threshold})"
        );

        // Count what was actually exercised rather than trusting the loop: this
        // guards against the table straddling the predicate while the iteration
        // skips a row.
        let mut buffered = 0_usize;
        let mut direct = 0_usize;
        for (tag, chunk) in SCENARIOS {
            assert_bare_drop_leaves_member_unfinished(tag, chunk);
            if chunk < threshold {
                buffered += 1;
            } else {
                direct += 1;
            }
        }
        assert!(
            buffered > 0 && direct > 0,
            "both gz_write branches must actually run, got {buffered} buffered and \
             {direct} direct"
        );
        assert_eq!(
            buffered + direct,
            SCENARIOS.len(),
            "every scenario in the table must be exercised"
        );
    }

    #[test]
    fn gzclose_w_finishes_the_member_that_a_bare_drop_leaves_unfinished() {
        let temp = TempGz::new("finish");
        let payload = drop_contract_payload();

        let mut state = drop_contract_write_state(temp.path());
        feed(&mut state, &payload, 3_000);
        // The control: the same state, the same payload, closed explicitly.
        assert_eq!(
            gzclose_w(state),
            ReturnCode::Ok.as_c_int(),
            "gzclose_w must report success"
        );

        let bytes = temp.bytes();
        assert_eq!(&bytes[..3], &[0x1f, 0x8b, 0x08]);

        let (result, recovered) = gunzip(&bytes);
        result.expect("an explicitly closed member must decode");
        assert_eq!(
            recovered, payload,
            "the closed member must round-trip byte-for-byte"
        );

        // And the trailer this time *is* the correct CRC-32/ISIZE pair — the
        // exact eight bytes the bare-drop test asserted were absent.
        let mut trailer = [0u8; 8];
        trailer[..4].copy_from_slice(&crate::checksum::crc32::crc32(0, &payload).to_le_bytes());
        trailer[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        assert_eq!(
            &bytes[bytes.len() - 8..],
            &trailer[..],
            "gzclose_w must append the RFC 1952 CRC-32 and ISIZE trailer"
        );

        assert!(temp.exists());
    }

    #[test]
    fn the_temporary_file_guard_cleans_up_on_every_path() {
        // Normal return.
        let path = {
            let temp = TempGz::new("cleanup_ok");
            let mut state = drop_contract_write_state(temp.path());
            feed(&mut state, b"a short write that stays buffered", 3_000);
            drop(state);
            assert!(
                temp.exists(),
                "the file must exist while the guard is alive"
            );
            temp.path().to_path_buf()
        };
        assert!(
            !path.exists(),
            "{} must be removed when the guard is dropped",
            path.display()
        );

        // Unwinding path. `cargo` forces `panic = "unwind"` for the test profile
        // regardless of `[profile.dev] panic = "abort"`, so a guard's `Drop` does
        // run while a test unwinds — which is exactly the path a failing
        // assertion in the two tests above would take.
        let escaped: PathBuf = {
            let leaked = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
            let sink = std::sync::Arc::clone(&leaked);
            let outcome = std::panic::catch_unwind(move || {
                let temp = TempGz::new("cleanup_panic");
                let mut state = drop_contract_write_state(temp.path());
                feed(&mut state, b"pending output that is never finished", 3_000);
                drop(state);
                *sink.lock().expect("poison-free mutex") = temp.path().to_path_buf();
                panic!("deliberate unwind to exercise the guard");
            });
            assert!(outcome.is_err(), "the closure must have unwound");
            leaked.lock().expect("poison-free mutex").clone()
        };
        assert_ne!(escaped, PathBuf::new(), "the path must have been recorded");
        assert!(
            !escaped.exists(),
            "{} must be removed even when the scope unwinds",
            escaped.display()
        );
    }
}
