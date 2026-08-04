//! Idiomatic, safe representation of a gzip header (RFC 1952).
//!
//! This module defines [`GzHeader`], the owned, memory-safe Rust equivalent of
//! the C `gz_header` structure declared in `zlib.h` (fields `text`, `time`,
//! `xflags`, `os`, `extra`/`extra_len`/`extra_max`, `name`/`name_max`,
//! `comment`/`comm_max`, `hcrc`, and `done`). It carries the metadata that is
//! written into a gzip member header by [`deflateSetHeader`] and read back out
//! of one by [`inflateGetHeader`].
//!
//! # Relationship to the C API
//!
//! The C `gz_header` uses raw `Bytef *` pointers paired with explicit length
//! and capacity fields (`extra_len`/`extra_max`, `name_max`, `comm_max`). Those
//! raw-pointer, manual-length concerns belong exclusively to the FFI boundary
//! ([`crate::ffi`], `src/ffi/types.rs`), which mirrors this type as a
//! `#[repr(C)]` struct and performs a straightforward, lossless conversion:
//!
//! | C `gz_header` field                | Rust [`GzHeader`] field      |
//! |------------------------------------|------------------------------|
//! | `int text`                         | [`text`](GzHeader::text): `bool`               |
//! | `uLong time`                       | [`time`](GzHeader::time): `u32`                |
//! | `int xflags`                       | [`xflags`](GzHeader::xflags): `i32`            |
//! | `int os`                           | [`os`](GzHeader::os): `i32`                    |
//! | `Bytef *extra`                     | [`extra`](GzHeader::extra): `Option<Vec<u8>>`  |
//! | `uInt extra_len`                   | crate-private `HeaderPublication::extra_len`   |
//! | `uInt extra_max`                   | [`extra_max`](GzHeader::extra_max): `u32`      |
//! | `Bytef *name`                      | [`name`](GzHeader::name): `Option<Vec<u8>>`    |
//! | `uInt name_max`                    | [`name_max`](GzHeader::name_max): `u32`        |
//! | `Bytef *comment`                   | [`comment`](GzHeader::comment): `Option<Vec<u8>>` |
//! | `uInt comm_max`                    | [`comm_max`](GzHeader::comm_max): `u32`        |
//! | `int hcrc`                         | [`hcrc`](GzHeader::hcrc): `bool`               |
//! | `int done`                         | [`done`](GzHeader::done): `bool`               |
//!
//! A `Z_NULL` C pointer maps to [`None`]; a non-null pointer with an associated
//! length maps to `Some(Vec<u8>)`. C `int` booleans (`0`/non-zero) map to Rust
//! [`bool`].
//!
//! # Conventions and design decisions
//!
//! * **No trailing NUL.** The gzip file format terminates the `name` and
//!   `comment` fields with a zero byte (RFC 1952 §2.3.1). [`GzHeader`] stores
//!   the payload bytes *without* that terminating NUL. Two distinct consumers
//!   restore it, and they must not be confused:
//!   * **Writing a gzip stream.** The deflate encoder's `Name` and `Comment`
//!     header phases (`src/deflate/mod.rs`) emit the stored bytes and then a
//!     `0` past their end, reproducing C's "copy the C string including its
//!     terminator" loop. Termination of the *wire format* is the encoder's job.
//!   * **Converting a C `gz_header`.** The FFI boundary (`src/ffi/types.rs`)
//!     translates between this owned type and the caller's raw `Bytef *`
//!     fields: it reads a caller pointer as a NUL-terminated C string and
//!     drops the terminator, and when writing back into a caller-supplied
//!     buffer it NUL-terminates within `name_max`/`comm_max`. That is C-string
//!     marshalling, not gzip framing.
//! * **`done` is a `bool` here.** The C field is tri-state: `inflateGetHeader`
//!   sets it to `1` when the header is fully parsed and to `-1` when the stream
//!   turns out to be a raw zlib stream with no gzip header. The idiomatic type
//!   models only the "header fully read" flag as a [`bool`]; the `-1` sentinel
//!   travels losslessly to the C caller through the crate-private `HeaderDone`
//!   discriminant recorded in `HeaderPublication`.
//! * **The declared `XLEN` is crate-private.** C's `gz_header.extra_len` reports
//!   the extra field's *declared* 16-bit length even when the copy into `extra`
//!   was clamped to `extra_max`, which is how a C caller detects truncation. That
//!   is a wire-level decoder observation with no idiomatic use — `extra.len()` is
//!   already the authoritative count of captured bytes — so it is carried in
//!   the crate-private `HeaderPublication::extra_len` and published only into the
//!   C caller's struct, keeping [`GzHeader`]'s public shape stable.
//! * **Publication is incremental, not bulk.** Reference zlib writes each
//!   `gz_header` field *inside its own parser state* and stores `name`/`comment`
//!   bytes into the caller's buffers as they arrive, so a caller polling between
//!   `inflate` calls sees exactly the fields the stream has delivered and nothing
//!   more. `HeaderPublication` records which of those assignments a given call
//!   performed so the FFI boundary can reproduce the schedule byte-for-byte.
//! * **Read-only capacities.** [`extra_max`](GzHeader::extra_max),
//!   [`name_max`](GzHeader::name_max), and [`comm_max`](GzHeader::comm_max)
//!   bound how many bytes `inflateGetHeader` will store into the respective
//!   fields while *reading* a header. They are ignored when *writing* a header
//!   with `deflateSetHeader`, and default to `0`.
//!
//! # Examples
//!
//! Build a header to be written with `deflateSetHeader`:
//!
//! ```
//! use zlib_rs::GzHeader;
//!
//! let header = GzHeader::new()
//!     .with_name(b"example.txt".to_vec())
//!     .with_comment(b"generated by zlib-rs".to_vec())
//!     .with_time(1_700_000_000)
//!     .with_os(255) // 255 == "unknown" per RFC 1952
//!     .with_text(true);
//!
//! assert_eq!(header.name.as_deref(), Some(&b"example.txt"[..]));
//! assert!(header.text);
//! assert!(!header.done);
//! ```
//!
//! [`deflateSetHeader`]: https://www.zlib.net/manual.html
//! [`inflateGetHeader`]: https://www.zlib.net/manual.html

use alloc::vec::Vec;

/// Gzip header information exchanged with the deflate/inflate engines.
///
/// `GzHeader` is the idiomatic, owned, fully safe counterpart of the C
/// `gz_header` struct (`zlib.h`). It is used in two directions:
///
/// * **Writing** (`deflateSetHeader`): the caller populates [`text`],
///   [`time`], [`os`], [`extra`], [`name`], [`comment`], and [`hcrc`], and the
///   deflate engine serializes them into the gzip member header. Note that
///   [`xflags`] is *ignored* when writing — the encoder derives the gzip `XFL`
///   byte from the compression level.
/// * **Reading** (`inflateGetHeader`): the inflate engine fills the fields in
///   from a parsed gzip header, honoring the [`extra_max`], [`name_max`], and
///   [`comm_max`] capacity limits, and sets [`done`] to `true` once the header
///   is complete.
///
/// See RFC 1952 for the authoritative definition of each gzip header field.
///
/// [`text`]: GzHeader::text
/// [`time`]: GzHeader::time
/// [`os`]: GzHeader::os
/// [`extra`]: GzHeader::extra
/// [`name`]: GzHeader::name
/// [`comment`]: GzHeader::comment
/// [`hcrc`]: GzHeader::hcrc
/// [`xflags`]: GzHeader::xflags
/// [`extra_max`]: GzHeader::extra_max
/// [`name_max`]: GzHeader::name_max
/// [`comm_max`]: GzHeader::comm_max
/// [`done`]: GzHeader::done
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GzHeader {
    /// `true` if the compressed data is believed to be text.
    ///
    /// Mirrors C `gz_header.text` and the gzip `FLG.FTEXT` bit (RFC 1952).
    pub text: bool,

    /// Modification time (gzip `MTIME`), in seconds since the Unix epoch.
    ///
    /// Mirrors C `gz_header.time`. The gzip `MTIME` field is 32 bits wide, so
    /// this is a [`u32`] (the C API widens it to `uLong`). A value of `0`
    /// means "no timestamp available".
    pub time: u32,

    /// Extra flags (gzip `XFL`, RFC 1952).
    ///
    /// Mirrors C `gz_header.xflags`. Populated when *reading* a header. It is
    /// **ignored when writing**: the deflate encoder sets the `XFL` byte from
    /// the compression level.
    pub xflags: i32,

    /// Operating-system code (gzip `OS`, RFC 1952 §2.3.1).
    ///
    /// Mirrors C `gz_header.os`. RFC 1952 reserves the value `255` for
    /// "unknown".
    ///
    /// Three cases determine what actually reaches the wire:
    ///
    /// * **No header installed.** With no `deflateSetHeader` call the encoder
    ///   writes [`crate::util::OS_CODE`], the platform value C's `zutil.h`
    ///   cascade selects for the same target — `10` on Windows, `19` on Apple,
    ///   `3` (Unix) everywhere else. Reference zlib writes exactly the same byte
    ///   on the same platform, so this is what keeps gzip output byte-identical;
    ///   a hard-coded `3` would diverge from a Windows- or macOS-built `libz`.
    /// * **A header installed.** The encoder writes the low byte of this field
    ///   verbatim (`os & 0xff`), whatever it holds.
    /// * **A default-constructed header.** [`GzHeader::new`] and
    ///   [`GzHeader::default`] leave this field at `0`, so installing one
    ///   unmodified emits `0` (FAT filesystem / MS-DOS), *not* `3` and not
    ///   `255`. Call [`with_os`](GzHeader::with_os) to choose deliberately —
    ///   `with_os(255)` for "unknown".
    ///
    /// When *reading* a header, `inflateGetHeader` stores whatever byte the
    /// stream carried.
    pub os: i32,

    /// Optional gzip "extra" subfield block (`FEXTRA`, RFC 1952).
    ///
    /// Corresponds to the C `gz_header.extra` pointer: [`None`] is a `Z_NULL`
    /// pointer (no extra field), while `Some(bytes)` holds the extra-field bytes
    /// actually stored. When *reading*, that may be **fewer** bytes than the
    /// stream declared, because the copy is clamped to
    /// [`extra_max`](GzHeader::extra_max).
    ///
    /// The stream's **declared** 16-bit `XLEN` — C `gz_header.extra_len`, whose
    /// excess over `extra_max` is a C caller's only truncation signal — is
    /// deliberately *not* a field of this type. It is a wire-level quantity that
    /// only the decoder observes and only a C `gz_header` has room to report, so
    /// it travels as crate-private parser metadata (`HeaderPublication`) and is
    /// published straight into the C caller's `extra_len` by the FFI boundary. An
    /// idiomatic caller reads `extra.as_ref().map_or(0, Vec::len)` for the number
    /// of bytes actually captured, which is the only number a `Vec` can be wrong
    /// about.
    pub extra: Option<Vec<u8>>,

    /// Optional original file name (`FNAME`, RFC 1952).
    ///
    /// Mirrors C `gz_header.name`. [`None`] corresponds to `Z_NULL`. The bytes
    /// are stored **without** the trailing NUL terminator that appears in the
    /// on-disk gzip format; the FFI layer adds/removes it during conversion.
    pub name: Option<Vec<u8>>,

    /// Optional human-readable comment (`FCOMMENT`, RFC 1952).
    ///
    /// Mirrors C `gz_header.comment`. [`None`] corresponds to `Z_NULL`. As with
    /// [`name`](GzHeader::name), the bytes are stored **without** the trailing
    /// NUL terminator.
    pub comment: Option<Vec<u8>>,

    /// `true` if a header CRC-16 is (or will be) present (`FHCRC`, RFC 1952).
    ///
    /// Mirrors C `gz_header.hcrc`. When writing, requests that a header CRC be
    /// emitted; when reading, indicates that one was present.
    pub hcrc: bool,

    /// `true` once the gzip header has been fully parsed while reading.
    ///
    /// Mirrors C `gz_header.done`. `inflateGetHeader` sets this to `true` when
    /// the header is complete. The C field is *tri-state* — it additionally uses
    /// `-1` to signal that the stream turned out to carry no gzip header at all —
    /// and that third value is carried losslessly by the crate-private
    /// `HeaderDone` discriminant so the FFI boundary can publish it verbatim.
    /// Unused when writing.
    pub done: bool,

    /// Read capacity for [`extra`](GzHeader::extra), in bytes.
    ///
    /// Mirrors C `gz_header.extra_max`. Bounds how many bytes
    /// `inflateGetHeader` will store into `extra` while reading a header.
    /// Ignored when writing; defaults to `0`.
    pub extra_max: u32,

    /// Read capacity for [`name`](GzHeader::name), in bytes.
    ///
    /// Mirrors C `gz_header.name_max`. Bounds how many bytes (including the
    /// terminating NUL, in the C accounting) `inflateGetHeader` will store into
    /// `name` while reading a header. Ignored when writing; defaults to `0`.
    pub name_max: u32,

    /// Read capacity for [`comment`](GzHeader::comment), in bytes.
    ///
    /// Mirrors C `gz_header.comm_max`. Bounds how many bytes (including the
    /// terminating NUL, in the C accounting) `inflateGetHeader` will store into
    /// `comment` while reading a header. Ignored when writing; defaults to `0`.
    pub comm_max: u32,
}

impl GzHeader {
    /// Creates a new, empty `GzHeader`.
    ///
    /// This is equivalent to [`GzHeader::default`]: every optional field is
    /// [`None`], the boolean flags ([`text`](GzHeader::text),
    /// [`hcrc`](GzHeader::hcrc), [`done`](GzHeader::done)) are `false`, and all
    /// numeric fields (including [`os`](GzHeader::os)) are `0`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new();
    /// assert_eq!(header, GzHeader::default());
    /// assert!(header.name.is_none());
    /// ```
    #[must_use]
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [`text`](GzHeader::text) flag and returns the updated header.
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new().with_text(true);
    /// assert!(header.text);
    /// ```
    #[must_use]
    #[inline]
    pub fn with_text(mut self, text: bool) -> Self {
        self.text = text;
        self
    }

    /// Sets the modification [`time`](GzHeader::time) and returns the updated
    /// header.
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new().with_time(1_700_000_000);
    /// assert_eq!(header.time, 1_700_000_000);
    /// ```
    #[must_use]
    #[inline]
    pub fn with_time(mut self, time: u32) -> Self {
        self.time = time;
        self
    }

    /// Sets the operating-system code [`os`](GzHeader::os) and returns the
    /// updated header.
    ///
    /// Pass `255` for the RFC 1952 "unknown" operating system.
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new().with_os(255);
    /// assert_eq!(header.os, 255);
    /// ```
    #[must_use]
    #[inline]
    pub fn with_os(mut self, os: i32) -> Self {
        self.os = os;
        self
    }

    /// Sets the file [`name`](GzHeader::name) and returns the updated header.
    ///
    /// The provided bytes should **not** include a trailing NUL terminator; the
    /// deflate encoder appends the RFC 1952 terminator when it writes the gzip
    /// header (and the FFI boundary appends one when marshalling this field
    /// back into a caller's C `gz_header` buffer).
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new().with_name(b"example.txt".to_vec());
    /// assert_eq!(header.name.as_deref(), Some(&b"example.txt"[..]));
    /// ```
    #[must_use]
    #[inline]
    pub fn with_name(mut self, name: impl Into<Vec<u8>>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the [`comment`](GzHeader::comment) and returns the updated header.
    ///
    /// The provided bytes should **not** include a trailing NUL terminator; the
    /// deflate encoder appends the RFC 1952 terminator when it writes the gzip
    /// header (and the FFI boundary appends one when marshalling this field
    /// back into a caller's C `gz_header` buffer).
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new().with_comment(b"hello".to_vec());
    /// assert_eq!(header.comment.as_deref(), Some(&b"hello"[..]));
    /// ```
    #[must_use]
    #[inline]
    pub fn with_comment(mut self, comment: impl Into<Vec<u8>>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Sets the [`extra`](GzHeader::extra) subfield block and returns the
    /// updated header.
    ///
    /// The gzip "extra" field is opaque, length-prefixed binary data (RFC 1952
    /// `FEXTRA`). The stored vector is the single source of truth for its length:
    /// the deflate encoder emits exactly `extra.len()` bytes as the `XLEN` word
    /// and payload, so there is no second length field that could desynchronize
    /// from it.
    ///
    /// # Examples
    ///
    /// ```
    /// use zlib_rs::GzHeader;
    ///
    /// let header = GzHeader::new().with_extra(vec![0x01, 0x02, 0x03]);
    /// assert_eq!(header.extra.as_deref(), Some(&[0x01, 0x02, 0x03][..]));
    /// assert_eq!(header.extra.as_ref().map_or(0, Vec::len), 3);
    /// ```
    #[must_use]
    #[inline]
    pub fn with_extra(mut self, extra: impl Into<Vec<u8>>) -> Self {
        self.extra = Some(extra.into());
        self
    }
}

/// C's tri-state `gz_header.done`, as the decoder assigns it.
///
/// The idiomatic [`GzHeader::done`] is a [`bool`] because only "the header was
/// fully read" is meaningful to a Rust caller. Reference zlib, however, uses the
/// same `int` field for a third value, and a C caller relies on it:
///
/// | C value | Meaning | Assigned at |
/// |---------|---------|-------------|
/// | `0` | not complete yet | `inflateGetHeader` registration (`inflate.c` L1228-L1229) |
/// | `-1` | the stream carries **no** gzip header | the `HEAD` non-gzip branch (`inflate.c` L522-L523) |
/// | `1` | the gzip header is complete | the `HCRC` state (`inflate.c` L686-L689) |
///
/// All three C values are modelled, so the discriminant is a lossless mirror of
/// the field rather than a widened [`bool`]. Whether a given `inflate` call
/// assigned `done` *at all* is a separate question, carried by wrapping this enum
/// in an [`Option`] whose [`None`] means "this call assigned nothing".
///
/// `-1` matters because it is the only way a caller using auto-detect framing
/// (`windowBits = 47`) can distinguish "this was a zlib stream, so there is no
/// gzip header to wait for" from "the gzip header has not arrived yet". Collapsing
/// it to `0` would leave such a caller polling forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub(crate) enum HeaderDone {
    /// C `head->done = -1`: the stream turned out not to carry a gzip header.
    ///
    /// Gated on the `gzip` feature because the C assignment is gated the same
    /// way: `inflate.c` L522-L523 sits inside `#ifdef GUNZIP` (L512-L527), so a
    /// `libz` built without gzip support cannot produce `-1` either. Mirroring
    /// the preprocessor structure keeps the enum an exact model of the field in
    /// *every* configuration rather than a superset in one of them.
    #[cfg(feature = "gzip")]
    NotGzip = -1,
    /// C `head->done = 0`: the header is not complete yet. Written by
    /// `inflateGetHeader` at registration, and by the bulk publisher for a
    /// [`GzHeader`] whose [`done`](GzHeader::done) is still `false`.
    Pending = 0,
    /// C `head->done = 1`: the gzip header was parsed to completion.
    Complete = 1,
}

impl HeaderDone {
    /// The C `int` value of this discriminant, for the FFI boundary.
    #[inline]
    pub(crate) const fn as_c_int(self) -> i32 {
        self as i32
    }
}

/// Which of reference zlib's individual `state->head->…` assignments a single
/// `inflate` call performed.
///
/// # Why this exists
///
/// C does not publish a gzip header in one go. Each field is assigned *inside its
/// own parser state*, writing directly into the caller's `gz_header` and its
/// `name`/`comment`/`extra` buffers as the bytes arrive (`inflate.c`: `FLAGS`
/// assigns `text`, `TIME` assigns `time`, `OS` assigns `xflags` and `os`, `EXLEN`
/// assigns `extra_len` *or* nulls `extra`, `EXTRA`/`NAME`/`COMMENT` append bytes
/// *or* null their pointer, and `HCRC` assigns `hcrc` and `done`). A caller that
/// polls its `gz_header` between calls therefore observes precisely the fields the
/// stream has delivered so far, with its own values still in place everywhere
/// else — including *no* NUL terminator after a partially received name.
///
/// The safe decoder here fills an owned [`GzHeader`] instead, so the FFI boundary
/// would otherwise have to mirror the whole owned value after every call. That
/// bulk write is observably wrong: it zeroes scalars C has not reached, terminates
/// a half-received name, and cannot express `done == -1`. This record closes the
/// gap by reporting, per call, exactly which assignments happened, so the boundary
/// performs exactly those writes (AAP §0.8.1 D-4, standard S5).
///
/// # Per-call, not cumulative
///
/// Every field describes **this** call only. C assigns each scalar once, so a
/// boundary that replays only the current call's assignments reproduces the
/// schedule without tracking history. The byte counters are the number of bytes
/// appended *during this call*, which combined with the owned `Vec`'s new length
/// gives the exact destination offset C wrote at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeaderPublication {
    /// `head->done`, when this call assigned it (`-1` in `HEAD`, `1` in `HCRC`).
    pub(crate) done: Option<HeaderDone>,
    /// `FLAGS` assigned `head->text` (`inflate.c` L568-L569).
    pub(crate) text: bool,
    /// `TIME` assigned `head->time` (`inflate.c` L577-L578).
    pub(crate) time: bool,
    /// `OS` assigned `head->xflags` **and** `head->os` — one C statement pair
    /// under a single guard (`inflate.c` L586-L589).
    pub(crate) os: bool,
    /// `HCRC` assigned `head->hcrc` (`inflate.c` L686-L688).
    pub(crate) hcrc: bool,
    /// `EXLEN` assigned `head->extra_len` from the stream's declared 16-bit
    /// `XLEN` (`inflate.c` L599-L600). Written only when the header actually
    /// carries an `FEXTRA` field, and never clamped to `extra_max` — the excess
    /// is the caller's truncation signal.
    pub(crate) extra_len: Option<u32>,
    /// `EXLEN`'s no-`FEXTRA` branch assigned `head->extra = Z_NULL`
    /// (`inflate.c` L605-L606).
    pub(crate) extra_null: bool,
    /// Bytes `EXTRA` appended to `head->extra` during this call
    /// (`inflate.c` L614-L621).
    pub(crate) extra_stored: usize,
    /// `NAME`'s no-`FNAME` branch assigned `head->name = Z_NULL`
    /// (`inflate.c` L650-L651).
    pub(crate) name_null: bool,
    /// Content bytes `NAME` appended to `head->name` during this call, excluding
    /// the terminator (`inflate.c` L639-L642).
    pub(crate) name_stored: usize,
    /// `NAME` stored the field's terminating NUL into `head->name`. C counts that
    /// NUL against `name_max` like any other byte, so a name that exactly fills
    /// the buffer is left **unterminated** and this stays `false`.
    pub(crate) name_terminated: bool,
    /// `COMMENT`'s no-`FCOMMENT` branch assigned `head->comment = Z_NULL`
    /// (`inflate.c` L672-L673).
    pub(crate) comment_null: bool,
    /// Content bytes `COMMENT` appended to `head->comment` during this call,
    /// excluding the terminator (`inflate.c` L661-L664).
    pub(crate) comment_stored: usize,
    /// `COMMENT` stored the field's terminating NUL into `head->comment`, subject
    /// to the same `comm_max` accounting as
    /// [`name_terminated`](HeaderPublication::name_terminated).
    pub(crate) comment_terminated: bool,
}

impl HeaderPublication {
    /// The record describing a header that is already **complete**, derived from
    /// the owned [`GzHeader`] alone.
    ///
    /// Used by the bulk publisher `write_gz_header_from_idiomatic`, whose
    /// callers hold a finished header and no parser history: every scalar C
    /// assigns has been assigned, every captured byte is present from offset `0`,
    /// and `name`/`comment` carry the terminator C would have stored — subject to
    /// the same "only if it fits within the capacity" rule the publisher applies.
    ///
    /// Two choices keep the bulk publisher's long-standing public behaviour
    /// byte-for-byte intact (standard S5):
    ///
    /// * The declared `XLEN` is reported as `extra.len()`, the number of bytes the
    ///   owned header actually carries. A *finished* [`GzHeader`] is the only
    ///   input here, so no wire-level `XLEN` is available to report instead, and
    ///   the captured length is the closest true statement about it.
    /// * The `*_null` flags stay `false` even for an absent field, so a bulk
    ///   publish never overwrites the C caller's `extra`/`name`/`comment` buffer
    ///   pointers with `Z_NULL`. Nulling is a *decoder* observation
    ///   (`inflate.c` L605-L606, L650-L651, L672-L673) that the incremental
    ///   publisher reports from parser state; inventing it here would destroy a
    ///   caller's buffer pointer.
    #[must_use]
    pub(crate) fn for_completed_header(src: &GzHeader) -> Self {
        Self {
            done: Some(if src.done {
                HeaderDone::Complete
            } else {
                HeaderDone::Pending
            }),
            text: true,
            time: true,
            os: true,
            hcrc: true,
            extra_len: src
                .extra
                .as_ref()
                .map(|extra| u32::try_from(extra.len()).unwrap_or(u32::MAX)),
            extra_null: false,
            extra_stored: src.extra.as_ref().map_or(0, Vec::len),
            name_null: false,
            name_stored: src.name.as_ref().map_or(0, Vec::len),
            name_terminated: true,
            comment_null: false,
            comment_stored: src.comment.as_ref().map_or(0, Vec::len),
            comment_terminated: true,
        }
    }

    /// Folds a **later** record into this one, yielding the record for the two
    /// consecutive engine calls taken together.
    ///
    /// # Why a merge is needed
    ///
    /// A record describes one engine call, while a C caller sees one `inflate`
    /// call. The two coincide everywhere except on the FFI boundary's
    /// header/output overlap path, which splits a single C call into a header phase
    /// and a data phase (and chunks the former) so that no Rust reference over the
    /// caller's window is live while a header byte is stored. The caller must still
    /// observe exactly the assignments C's single call would have made, so the
    /// per-call records are folded back into one.
    ///
    /// The fold matches how each field is produced:
    ///
    /// * Scalar flags are *idempotent* assignments — C writes the field once, from
    ///   whichever state reached it — so they are OR-ed.
    /// * `done` is the tri-state; the later call wins, because a call that reaches
    ///   `HCRC` after an earlier one saw the `HEAD` non-gzip branch is reporting the
    ///   newer truth.
    /// * `extra_len` is the declared `XLEN`, assigned once in `EXLEN`; the later
    ///   `Some` wins for the same reason.
    /// * The byte counters are *counts of bytes appended during the call*, which is
    ///   additive by construction: the publisher derives each destination offset as
    ///   `len - stored` from the owned vector's final length, and that identity
    ///   holds for the summed count exactly as it does for either part.
    ///
    /// The production caller — `inflate_split_over_header` in `src/ffi/inflate.rs` —
    /// is `#[cfg(feature = "gzip")]`, so in a `gzip`-off **non-test** build this
    /// method has no caller and `dead_code` fires. It is deliberately *not* given
    /// the same `cfg` as that caller: its two unit tests below are ungated, so
    /// gating the method would have to gate them too, dropping two tests from the
    /// `--no-default-features`, `no-std` and `std,simd` rows. AAP directive D-5
    /// fixes this suite at "preserved and only ever increased", so the fold stays
    /// compiled and stays covered in every configuration, and the lint is waived
    /// only in the one configuration that legitimately has no production caller.
    #[cfg_attr(not(feature = "gzip"), allow(dead_code))]
    #[must_use]
    pub(crate) fn merged_with(self, later: Self) -> Self {
        Self {
            done: later.done.or(self.done),
            text: self.text || later.text,
            time: self.time || later.time,
            os: self.os || later.os,
            hcrc: self.hcrc || later.hcrc,
            extra_len: later.extra_len.or(self.extra_len),
            extra_null: self.extra_null || later.extra_null,
            extra_stored: self.extra_stored + later.extra_stored,
            name_null: self.name_null || later.name_null,
            name_stored: self.name_stored + later.name_stored,
            name_terminated: self.name_terminated || later.name_terminated,
            comment_null: self.comment_null || later.comment_null,
            comment_stored: self.comment_stored + later.comment_stored,
            comment_terminated: self.comment_terminated || later.comment_terminated,
        }
    }
}

// ===========================================================================
// ForeignGzHeader — the live, borrowed, allocation-free header view
// ===========================================================================

/// A gzip header that is owned by a **C caller**, borrowed for the duration of
/// one engine call.
///
/// # Why this type exists
///
/// C's `deflateSetHeader` stores nothing but a pointer — `strm->state->gzhead =
/// head` (`deflate.c` L717) — and allocates nothing. Every field is then re-read
/// *lazily, at emission time*, inside `deflate()` (`deflate.c` L1092-L1188) and
/// inside `deflateBound` (`deflate.c` L893-L907). Two consequences follow, and
/// both are observable:
///
/// 1. **Mutations are honored.** A caller may set a header, mutate `head->name`,
///    and only then call `deflate()`; C emits the *new* name. An implementation
///    that deep-copied at registration would emit the stale one and produce
///    different gzip bytes — a violation of the byte-identity requirement
///    (AAP §0.8.1 D-1).
/// 2. **Registration cannot fail for want of memory.** C returns only `Z_OK` or
///    `Z_STREAM_ERROR`. A deep copy introduces a `Z_MEM_ERROR` that reference
///    zlib can never produce.
///
/// This type reproduces both properties. It holds **borrowed slices**, never
/// owned buffers, so constructing one allocates nothing, and it is rebuilt from
/// the caller's live struct on every entry point that reads the header.
///
/// # Layering
///
/// The type lives here, in the layer-5 `gz_header` module, so that the layer-6
/// deflate engine can consume it while the raw-pointer work — testing the
/// caller's pointers for null, scanning `name`/`comment` for their terminating
/// NUL, and materializing the slices — stays confined to `src/ffi`, the sole
/// sanctioned `unsafe` zone (AAP §0.6.2). Holding and reading a borrowed slice
/// needs no `unsafe`, so this module and the engine remain `unsafe`-free.
///
/// # Granularity equivalence
///
/// C re-reads the caller's memory *per byte* (`s->gzhead->name[s->gzindex++]`),
/// whereas the FFI materializes these slices once per engine call. The two are
/// equivalent: a caller cannot mutate its header *during* a call, because the C
/// API is synchronous and single-threaded, so per-call and per-byte freshness
/// observe exactly the same bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForeignGzHeader<'a> {
    /// C `head->text`, read at emission (`deflate.c` L1092).
    pub text: bool,
    /// C `head->time`, the MTIME written little-endian (`deflate.c` L1098-L1101).
    pub time: u32,
    /// C `head->os`, written as `os & 0xff` (`deflate.c` L1105).
    pub os: i32,
    /// C `head->hcrc`: when set, a two-byte header CRC-16 is appended
    /// (`deflate.c` L1110, L1188-L1195).
    pub hcrc: bool,
    /// The caller's `extra` field: exactly `head->extra_len & 0xffff` bytes when
    /// `head->extra != Z_NULL`, otherwise [`None`] (`deflate.c` L1106-L1108,
    /// L1118-L1137).
    pub extra: Option<&'a [u8]>,
    /// The caller's `name`, up to but excluding its terminating NUL, or [`None`]
    /// when `head->name == Z_NULL` (`deflate.c` L1145-L1158).
    pub name: Option<&'a [u8]>,
    /// The caller's `comment`, up to but excluding its terminating NUL, or
    /// [`None`] when `head->comment == Z_NULL` (`deflate.c` L1167-L1180).
    pub comment: Option<&'a [u8]>,
}

/// Which gzip header, if any, a deflate stream will emit.
///
/// This is the safe-Rust counterpart of C's single `gz_header *gzhead` field
/// (`deflate.h` L106). C distinguishes only "null" from "some pointer"; Rust
/// must additionally distinguish *who owns the storage*, because the idiomatic
/// API hands the engine an owned [`GzHeader`] while the C ABI lends it a live
/// struct it must not copy (see [`ForeignGzHeader`]).
///
/// The enum is `Clone` and allocation-free in every arm except [`Owned`](Self::Owned),
/// whose clone duplicates the caller's own vectors. `deflateCopy` therefore
/// duplicates an owned header and shares a foreign one — which is precisely
/// what C does, since its `zmemcpy` of `deflate_state` copies the `gzhead`
/// *pointer* (`deflate.c` L1345).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum GzHeaderSlot {
    /// No header was set: C `gzhead == Z_NULL`. The default gzip header is
    /// emitted (`deflate.c` L1072-L1090).
    #[default]
    None,
    /// A header supplied through the idiomatic Rust API and owned by the engine.
    Owned(GzHeader),
    /// A header supplied through the C ABI and owned by the caller. No contents
    /// are stored: they are re-read from the caller's struct on every entry
    /// point that needs them, exactly as C re-reads through its pointer.
    Foreign,
}

impl GzHeaderSlot {
    /// Whether a header is registered at all, i.e. C's `gzhead != Z_NULL`.
    ///
    /// This is the *only* predicate the engine may use to decide between the
    /// default header and a caller-supplied one, because it is the only one C
    /// has.
    #[inline]
    #[must_use]
    pub const fn is_set(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// The owned header, if this slot holds one.
    #[inline]
    #[must_use]
    pub const fn owned(&self) -> Option<&GzHeader> {
        match self {
            Self::Owned(h) => Some(h),
            Self::None | Self::Foreign => None,
        }
    }
}

/// A uniform, borrowed view over whichever header source a stream has.
///
/// Header emission must read identical fields regardless of whether the header
/// is engine-owned or caller-owned, so both arms of [`GzHeaderSlot`] are
/// normalized into this single borrowed shape before any byte is written. That
/// keeps one copy of the emission logic — the copy whose byte-for-byte
/// agreement with `deflate.c` is what the byte-identity gate proves — instead of
/// two that could drift apart.
///
/// Because every payload is a borrowed slice, normalizing allocates nothing.
/// This deliberately replaces an earlier implementation that cloned `extra`,
/// `name`, and `comment` once per emission phase purely to satisfy the borrow
/// checker: those clones were three allocations per gzip member that C does not
/// make, and they could be re-run on every re-entry into a partially emitted
/// phase (AAP §0.6.5 requires allocation-count parity).
#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderFields<'a> {
    /// C `head->text`.
    pub text: bool,
    /// C `head->time`.
    pub time: u32,
    /// C `head->os`.
    pub os: i32,
    /// C `head->hcrc`.
    pub hcrc: bool,
    /// C `head->extra`, exactly `extra_len` bytes, or [`None`] when absent.
    pub extra: Option<&'a [u8]>,
    /// C `head->name` without its NUL, or [`None`] when absent.
    pub name: Option<&'a [u8]>,
    /// C `head->comment` without its NUL, or [`None`] when absent.
    pub comment: Option<&'a [u8]>,
}

impl<'a> HeaderFields<'a> {
    /// Borrows an engine-owned header.
    #[must_use]
    pub fn from_owned(h: &'a GzHeader) -> Self {
        Self {
            text: h.text,
            time: h.time,
            os: h.os,
            hcrc: h.hcrc,
            extra: h.extra.as_deref(),
            name: h.name.as_deref(),
            comment: h.comment.as_deref(),
        }
    }

    /// Borrows a caller-owned header lent for this call.
    #[must_use]
    pub const fn from_foreign(h: &ForeignGzHeader<'a>) -> Self {
        Self {
            text: h.text,
            time: h.time,
            os: h.os,
            hcrc: h.hcrc,
            extra: h.extra,
            name: h.name,
            comment: h.comment,
        }
    }

    /// Resolves a slot plus an optional lent foreign header into one view.
    ///
    /// Returns [`None`] exactly when C would see `gzhead == Z_NULL`. A
    /// [`Foreign`](GzHeaderSlot::Foreign) slot with no lent header also yields
    /// [`None`]: that combination means an entry point that cannot reach the
    /// caller's struct is being asked about it, and emitting a *default* header
    /// is the only safe reading — never inventing fields.
    #[must_use]
    pub fn resolve(slot: &'a GzHeaderSlot, lent: Option<&ForeignGzHeader<'a>>) -> Option<Self> {
        match slot {
            GzHeaderSlot::None => None,
            GzHeaderSlot::Owned(h) => Some(Self::from_owned(h)),
            GzHeaderSlot::Foreign => lent.map(Self::from_foreign),
        }
    }
}

/// A **borrowed, caller-owned** set of gzip-header output buffers that the
/// decoder writes into directly while parsing — the read direction's counterpart
/// of [`ForeignGzHeader`].
///
/// # Why the decoder needs this
///
/// C's `inflate` never accumulates the header anywhere of its own: each decoded
/// byte is stored straight into the caller's buffer through
/// `state->head->name[state->length++]` and friends (`inflate.c` L614-L621,
/// L639-L642, L661-L664), and the guards on those stores re-read the caller's
/// live `extra`/`name`/`comment` pointers *and* their live `extra_max`/
/// `name_max`/`comm_max` capacities on **every** byte. Two consequences follow
/// that a snapshot taken at `inflateGetHeader` time cannot reproduce:
///
/// * A caller may install (or replace, or withdraw) a sink buffer after
///   registering the header but before the bytes arrive, and C honors it.
/// * A caller may change a capacity mid-parse, and C truncates against the new
///   value.
///
/// It also means the decoder allocates nothing for the header. Capturing into
/// owned vectors instead introduces an allocation on a path where C has none,
/// and therefore a `Z_MEM_ERROR` C cannot return.
///
/// # Shape
///
/// Each field is `Some` exactly when the corresponding C pointer is non-null,
/// and the sink's [`capacity`](ForeignByteSink::capacity) is that field's live
/// capacity, so the decoder's store predicate is the ordinary
/// `Option`-plus-bounds test rather than a raw pointer comparison. Holding the
/// borrow for the duration of one engine call is equivalent to C's per-byte
/// re-read, because the synchronous single-threaded C API gives a caller no
/// opportunity to mutate its header *during* a call.
///
/// # Why the buffers are [`ForeignByteSink`]s and not `&mut [u8]`
///
/// C imposes **no disjointness requirement** on `head->extra`, `head->name`,
/// `head->comment` and `strm->next_out`: they are four independent caller
/// pointers, and a program that overlaps them merely gets whatever the
/// interleaved stores leave behind. Three simultaneous `&mut [u8]` over those
/// ranges, by contrast, are instant undefined behaviour the moment they overlap
/// — the aliasing is committed when the references are *created*, before any
/// bounds-checked write runs — so no such reference is ever formed. Each buffer
/// is instead reached through a [`ForeignByteSink`] the FFI boundary backs with a
/// bare pointer and a capacity, and every store is one independent, bounds-tested
/// access. Overlap then behaves exactly as it does in C.
///
/// The type carries no `unsafe`; materializing it from a raw `gz_header` is the
/// FFI boundary's job.
#[derive(Default)]
pub struct ForeignGzHeaderSink<'a> {
    /// The stream's declared `XLEN`, as currently visible in the caller's
    /// `extra_len` field.
    ///
    /// C derives the write offset for the extra field as `head->extra_len -
    /// state->length` (`inflate.c` L616-L617) — that is, from the caller's own
    /// struct, re-read on each pass — so it is part of the live view rather than
    /// engine state.
    pub extra_len: u32,
    /// The caller's `extra` buffer, bounded by its live `extra_max`
    /// (`inflate.c` L614-L621).
    pub extra: Option<&'a mut dyn ForeignByteSink>,
    /// The caller's `name` buffer, bounded by its live `name_max`
    /// (`inflate.c` L639-L642).
    pub name: Option<&'a mut dyn ForeignByteSink>,
    /// The caller's `comment` buffer, bounded by its live `comm_max`
    /// (`inflate.c` L661-L664).
    pub comment: Option<&'a mut dyn ForeignByteSink>,
}

impl core::fmt::Debug for ForeignGzHeaderSink<'_> {
    /// Reports each field's presence and live capacity rather than its contents.
    ///
    /// A [`ForeignByteSink`] may be backed by a bare caller pointer whose bytes
    /// are not known to be initialized, so formatting them would be unsound; the
    /// capacity is the whole of what the decoder's store predicate consults
    /// anyway.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fn cap(sink: Option<&&mut dyn ForeignByteSink>) -> Option<usize> {
            sink.map(|s| s.capacity())
        }
        f.debug_struct("ForeignGzHeaderSink")
            .field("extra_len", &self.extra_len)
            .field("extra_capacity", &cap(self.extra.as_ref()))
            .field("name_capacity", &cap(self.name.as_ref()))
            .field("comment_capacity", &cap(self.comment.as_ref()))
            .finish()
    }
}

/// A bounded byte sink the decoder can write single bytes and runs of bytes into
/// without ever holding a Rust reference over the destination.
///
/// # Why this trait exists
///
/// The gzip header's three variable-length payloads live in **caller** memory
/// reached through the raw `extra`/`name`/`comment` pointers of a C `gz_header`.
/// The C API places no disjointness requirement on them, nor between them and
/// `strm->next_out`, so the decoder must tolerate arbitrary overlap. Materializing
/// them as `&mut [u8]` cannot: overlapping mutable references are undefined
/// behaviour at the moment of creation, independently of whether any write ever
/// lands in the shared bytes.
///
/// This trait is the narrow capability the decoder actually needs — "store this
/// byte at this index if the index is inside the live capacity" — expressed so
/// that the implementation may perform one isolated, bounds-tested access per
/// store. The FFI boundary implements it over a bare pointer and a length
/// (`crate::ffi::types`), which is precisely what C does, so overlap is as
/// well-defined here as it is there.
///
/// Declaring it in this module rather than at the boundary is what keeps
/// `src/inflate/**` free of `unsafe` (AAP §0.8.1 D-6, standard S2): the decoder
/// programs against this safe interface, and the single `unsafe` implementation
/// lives inside the boundary module that is allowed to have one.
///
/// # Contract
///
/// Implementations must be **total**: a store whose target lies at or beyond
/// [`capacity`](Self::capacity) must be refused rather than clamped into a
/// neighbouring byte, because C's guards (`length < head->name_max`,
/// `len < head->extra_max`) drop such bytes outright.
pub trait ForeignByteSink {
    /// The number of bytes that may be stored, counted from index `0`.
    ///
    /// This is the caller's live `extra_max`/`name_max`/`comm_max`; C re-reads it
    /// on every stored byte, so an implementation must report the current value
    /// rather than one captured at registration time.
    fn capacity(&self) -> usize;

    /// Stores `byte` at `index`, reporting whether it was stored.
    ///
    /// Returns `false` — writing nothing — when `index >= self.capacity()`.
    fn store_byte(&mut self, index: usize, byte: u8) -> bool;

    /// Copies as much of `src` as fits starting at `offset`, returning how many
    /// bytes were stored.
    ///
    /// Returns `0` — writing nothing — when `offset >= self.capacity()`;
    /// otherwise stores `min(src.len(), capacity - offset)` bytes.
    fn store_bytes(&mut self, offset: usize, src: &[u8]) -> usize;
}

/// The obvious [`ForeignByteSink`] over a byte buffer the *Rust* side owns: the
/// capacity is the slice length and each store is an ordinary bounds-checked index.
///
/// This is the implementation idiomatic Rust callers and this crate's own tests
/// use, and — because [`ForeignGzHeaderSink`]'s fields are trait objects — the one
/// that makes that type constructible outside the FFI boundary at all. It is *not*
/// the implementation the C ABI uses: a slice over caller memory is exactly the
/// aliasing hazard [`ForeignByteSink`] exists to avoid. For a buffer whose
/// exclusive ownership Rust can see, the borrow is unique by construction and the
/// bounds test is free.
///
/// A newtype rather than `impl ForeignByteSink for [u8]` because a trait object is
/// a thin pointer plus a vtable: `[u8]` is itself unsized, so `&mut [u8]` could
/// never be coerced to `&mut dyn ForeignByteSink` without discarding its length.
#[derive(Debug)]
pub struct SliceSink<'a>(
    /// The borrowed destination; its length is the sink's capacity.
    pub &'a mut [u8],
);

impl ForeignByteSink for SliceSink<'_> {
    #[inline]
    fn capacity(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn store_byte(&mut self, index: usize, byte: u8) -> bool {
        match self.0.get_mut(index) {
            Some(slot) => {
                *slot = byte;
                true
            }
            None => false,
        }
    }

    #[inline]
    fn store_bytes(&mut self, offset: usize, src: &[u8]) -> usize {
        let cap = self.0.len();
        if offset >= cap {
            return 0;
        }
        let room = cap - offset;
        let n = if src.len() > room { room } else { src.len() };
        self.0[offset..offset + n].copy_from_slice(&src[..n]);
        n
    }
}

impl<'a> ForeignGzHeaderSink<'a> {
    /// Stores `byte` at `index` in the `name` buffer if the buffer exists and
    /// the index is within its live capacity, reporting whether it was stored.
    ///
    /// Reproduces C's `if (head != NULL && head->name != NULL && length <
    /// head->name_max) head->name[length++] = byte;` (`inflate.c` L639-L642):
    /// the index advances only on a store, so a name longer than the buffer is
    /// truncated and left unterminated exactly as in C.
    #[inline]
    pub fn store_name(&mut self, index: usize, byte: u8) -> bool {
        Self::store(self.name.as_deref_mut(), index, byte)
    }

    /// Stores `byte` at `index` in the `comment` buffer, bounded by its live
    /// capacity. C `inflate.c` L661-L664; see [`store_name`](Self::store_name).
    #[inline]
    pub fn store_comment(&mut self, index: usize, byte: u8) -> bool {
        Self::store(self.comment.as_deref_mut(), index, byte)
    }

    /// Copies as much of `src` as fits into the `extra` buffer starting at
    /// `offset`, returning how many bytes were stored.
    ///
    /// Reproduces C's clamped `zmemcpy(head->extra + len, next, len + copy >
    /// head->extra_max ? head->extra_max - len : copy)` (`inflate.c`
    /// L614-L621), including the enclosing `len < head->extra_max` guard: an
    /// offset already at or beyond the live capacity stores nothing.
    #[inline]
    pub fn store_extra(&mut self, offset: usize, src: &[u8]) -> usize {
        match self.extra.as_deref_mut() {
            Some(sink) => sink.store_bytes(offset, src),
            None => 0,
        }
    }

    /// Shared bounds-checked single-byte store for `name`/`comment`.
    ///
    /// The trait object's lifetime is elided *separately* from the reborrow's
    /// (`+ '_` rather than the object-lifetime default): `&mut` is invariant in its
    /// pointee, so tying the two together would force the caller's short reborrow
    /// and the view's own `'a` to be equal instead of merely compatible.
    #[inline]
    fn store(sink: Option<&mut (dyn ForeignByteSink + '_)>, index: usize, byte: u8) -> bool {
        match sink {
            Some(sink) => sink.store_byte(index, byte),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty_header() {
        let header = GzHeader::default();

        // All optional fields are absent (equivalent to C `Z_NULL`).
        assert_eq!(header.extra, None);
        assert_eq!(header.name, None);
        assert_eq!(header.comment, None);

        // Boolean flags start cleared.
        assert!(!header.text);
        assert!(!header.hcrc);
        assert!(!header.done);

        // Numeric fields start at zero, including `os` and the read capacities.
        assert_eq!(header.time, 0);
        assert_eq!(header.xflags, 0);
        assert_eq!(header.os, 0);
        assert_eq!(header.extra_max, 0);
        assert_eq!(header.name_max, 0);
        assert_eq!(header.comm_max, 0);
    }

    #[test]
    fn new_matches_default() {
        assert_eq!(GzHeader::new(), GzHeader::default());
    }

    #[test]
    fn builders_round_trip_field_values() {
        let header = GzHeader::new()
            .with_text(true)
            .with_time(1_700_000_000)
            .with_os(255)
            .with_name(b"example.txt".to_vec())
            .with_comment(b"a comment".to_vec())
            .with_extra(vec![0xDE, 0xAD, 0xBE, 0xEF]);

        assert!(header.text);
        assert_eq!(header.time, 1_700_000_000);
        assert_eq!(header.os, 255);
        assert_eq!(header.name.as_deref(), Some(&b"example.txt"[..]));
        assert_eq!(header.comment.as_deref(), Some(&b"a comment"[..]));
        assert_eq!(header.extra.as_deref(), Some(&[0xDE, 0xAD, 0xBE, 0xEF][..]));

        // Builders that were not called leave defaults intact.
        assert!(!header.hcrc);
        assert!(!header.done);
        assert_eq!(header.xflags, 0);
    }

    #[test]
    fn direct_field_assignment_round_trips() {
        // Fields consumed only when *reading* a header are set directly.
        let mut header = GzHeader::new();
        header.xflags = 4;
        header.hcrc = true;
        header.done = true;
        header.extra_max = 32;
        header.name_max = 64;
        header.comm_max = 128;

        assert_eq!(header.xflags, 4);
        assert!(header.hcrc);
        assert!(header.done);
        assert_eq!(header.extra_max, 32);
        assert_eq!(header.name_max, 64);
        assert_eq!(header.comm_max, 128);
    }

    #[test]
    fn accepts_slice_and_vec_inputs() {
        // `impl Into<Vec<u8>>` accepts both owned vectors and borrowed slices.
        let from_slice = GzHeader::new().with_name(&b"slice"[..]);
        let from_vec = GzHeader::new().with_name(b"slice".to_vec());
        assert_eq!(from_slice, from_vec);
        assert_eq!(from_slice.name.as_deref(), Some(&b"slice"[..]));
    }

    #[test]
    fn empty_extra_is_some_not_none() {
        // An empty-but-present extra field is distinct from an absent one.
        let header = GzHeader::new().with_extra(Vec::new());
        assert_eq!(header.extra.as_deref(), Some(&[][..]));
        assert_ne!(header.extra, None);
    }

    #[test]
    fn clone_and_eq_are_consistent() {
        let header = GzHeader::new()
            .with_name(b"clone.bin".to_vec())
            .with_extra(vec![1, 2, 3])
            .with_text(true);
        let cloned = header.clone();

        assert_eq!(header, cloned);

        // Mutating the clone breaks equality, confirming deep, owned data.
        let mut mutated = cloned;
        mutated.text = false;
        assert_ne!(header, mutated);
    }

    #[test]
    fn debug_impl_is_available() {
        // The derived `Debug` impl must render without panicking.
        let header = GzHeader::new().with_name(b"dbg".to_vec());
        let rendered = alloc::format!("{header:?}");
        assert!(rendered.contains("GzHeader"));
    }

    /// `GzHeader`'s public field set is part of the crate's API, so it is pinned by
    /// an **exhaustive** struct expression and an **exhaustive** destructuring
    /// pattern (no `..` in either).
    ///
    /// Every field a caller can name is a compatibility commitment: an exhaustive
    /// struct literal in downstream code stops compiling the moment a field is
    /// added, and a `let Self { .. }` destructuring stops compiling the moment one
    /// is removed. Wire-level decoder observations therefore belong in the
    /// crate-private [`HeaderPublication`] record rather than here. The declared
    /// `XLEN` is the clearest instance: C's `EXLEN` state assigns
    /// `head->extra_len` independently of how many extra bytes were captured, so
    /// that count travels to the C caller through `HeaderPublication` and is
    /// written into the raw `gz_header` at the boundary — it is deliberately not a
    /// field of this thirteen-field public mirror.
    #[test]
    fn public_field_set_is_exactly_the_thirteen_c_mirrored_fields() {
        // Exhaustive construction: adding a field breaks this line.
        let header = GzHeader {
            text: true,
            time: 42,
            xflags: 2,
            os: 3,
            extra: Some(vec![7, 8]),
            name: Some(b"n".to_vec()),
            comment: Some(b"c".to_vec()),
            hcrc: true,
            done: true,
            extra_max: 16,
            name_max: 32,
            comm_max: 64,
        };
        // Exhaustive destructuring: removing or renaming a field breaks this one.
        let GzHeader {
            text,
            time,
            xflags,
            os,
            extra,
            name,
            comment,
            hcrc,
            done,
            extra_max,
            name_max,
            comm_max,
        } = header;
        assert!(text && hcrc && done);
        assert_eq!((time, xflags, os), (42, 2, 3));
        assert_eq!(extra.as_deref(), Some(&[7, 8][..]));
        assert_eq!(name.as_deref(), Some(&b"n"[..]));
        assert_eq!(comment.as_deref(), Some(&b"c"[..]));
        assert_eq!((extra_max, name_max, comm_max), (16, 32, 64));
    }

    /// `HeaderDone` is a lossless mirror of C's tri-state `head->done`, so its
    /// discriminants must be exactly `-1`, `0` and `1` (`inflate.c` L522-L523,
    /// L1228-L1229, L686-L689). The FFI boundary publishes `as_c_int()` verbatim.
    #[test]
    fn header_done_discriminants_match_the_c_field() {
        assert_eq!(HeaderDone::Pending.as_c_int(), 0);
        assert_eq!(HeaderDone::Complete.as_c_int(), 1);
        // `#[repr(i32)]` makes the cast and the accessor agree.
        assert_eq!(HeaderDone::Complete as i32, 1);
        // `-1` exists only where C's own `#ifdef GUNZIP` puts it.
        #[cfg(feature = "gzip")]
        {
            assert_eq!(HeaderDone::NotGzip.as_c_int(), -1);
            assert_eq!(HeaderDone::NotGzip as i32, -1);
        }
    }

    /// A default [`HeaderPublication`] must record **nothing**: a call that
    /// reached no header state may not cause a single write into the caller's
    /// `gz_header`, which is the whole point of the record.
    #[test]
    fn default_header_publication_authorizes_no_write() {
        let p = HeaderPublication::default();
        assert!(p.done.is_none());
        assert!(!p.text && !p.time && !p.os && !p.hcrc);
        assert!(p.extra_len.is_none());
        assert!(!p.extra_null && !p.name_null && !p.comment_null);
        assert_eq!((p.extra_stored, p.name_stored, p.comment_stored), (0, 0, 0));
        assert!(!p.name_terminated && !p.comment_terminated);
    }

    /// The bulk record derived from a finished header must authorize every scalar
    /// C assigns, report the captured extra length, terminate both C strings, and
    /// — critically — **never** null the caller's buffer pointers: nulling is a
    /// decoder observation the incremental publisher reports, and inventing it in
    /// a bulk publish would destroy a caller's buffer pointer.
    #[test]
    fn bulk_header_publication_matches_the_legacy_publisher() {
        let src = GzHeader::new()
            .with_extra(vec![1, 2, 3])
            .with_name(b"n.bin".to_vec());
        let p = HeaderPublication::for_completed_header(&src);

        assert!(p.text && p.time && p.os && p.hcrc);
        assert_eq!(p.done, Some(HeaderDone::Pending), "src.done is false");
        assert_eq!(p.extra_len, Some(3));
        assert_eq!(p.extra_stored, 3);
        assert_eq!(p.name_stored, 5);
        assert_eq!(p.comment_stored, 0, "an absent comment stores nothing");
        assert!(p.name_terminated && p.comment_terminated);
        assert!(
            !p.extra_null && !p.name_null && !p.comment_null,
            "a bulk publish must not overwrite the caller's buffer pointers, \
             even for an absent field"
        );

        // A completed header reports `Complete`, and an absent extra field
        // leaves the C caller's `extra_len` alone.
        let done = GzHeader {
            done: true,
            ..GzHeader::new()
        };
        let p = HeaderPublication::for_completed_header(&done);
        assert_eq!(p.done, Some(HeaderDone::Complete));
        assert_eq!(p.extra_len, None);
    }

    // -- ForeignByteSink / ForeignGzHeaderSink ------------------------------

    /// The slice-backed sink refuses every out-of-range target instead of
    /// clamping it into a neighbouring byte, which is what C's
    /// `length < head->name_max` and `len < head->extra_max` guards do.
    #[test]
    fn a_slice_sink_refuses_out_of_range_targets() {
        let mut buf = [0xAAu8; 4];
        let mut backing = SliceSink(&mut buf[..]);
        let sink: &mut dyn ForeignByteSink = &mut backing;

        assert_eq!(sink.capacity(), 4);
        assert!(sink.store_byte(0, 1), "index 0 is inside the capacity");
        assert!(
            sink.store_byte(3, 2),
            "the last index is inside the capacity"
        );
        assert!(
            !sink.store_byte(4, 3),
            "the first out-of-range index is refused"
        );
        assert!(!sink.store_byte(usize::MAX, 4), "and so is a wild one");

        assert_eq!(sink.store_bytes(2, &[9, 9, 9, 9]), 2, "the copy is clamped");
        assert_eq!(
            sink.store_bytes(4, &[9]),
            0,
            "an offset at the capacity stores nothing"
        );
        assert_eq!(
            sink.store_bytes(usize::MAX, &[9]),
            0,
            "and neither does a wild one"
        );
        assert_eq!(buf, [1, 0xAA, 9, 9]);
    }

    /// An absent field stores nothing and reports it, so the decoder's
    /// `Option`-plus-bounds predicate matches C's `head->name != Z_NULL` test.
    #[test]
    fn an_absent_sink_field_stores_nothing() {
        let mut sink = ForeignGzHeaderSink::default();
        assert!(!sink.store_name(0, b'x'));
        assert!(!sink.store_comment(0, b'x'));
        assert_eq!(sink.store_extra(0, b"xy"), 0);
        assert_eq!(sink.extra_len, 0);
    }

    /// The three payload views are independent: a store to one must not disturb
    /// another, and each is bounded by its own capacity.
    #[test]
    fn each_sink_field_is_bounded_by_its_own_capacity() {
        let mut extra = [0xAAu8; 2];
        let mut name = [0xAAu8; 4];
        let mut comment = [0xAAu8; 1];
        let mut extra_sink = SliceSink(&mut extra[..]);
        let mut name_sink = SliceSink(&mut name[..]);
        let mut comment_sink = SliceSink(&mut comment[..]);
        let mut sink = ForeignGzHeaderSink {
            extra_len: 5,
            extra: Some(&mut extra_sink),
            name: Some(&mut name_sink),
            comment: Some(&mut comment_sink),
        };

        assert_eq!(
            sink.store_extra(0, b"ABCDE"),
            2,
            "clamped to extra's capacity"
        );
        assert!(sink.store_name(3, b'd'));
        assert!(!sink.store_name(4, b'e'), "clamped to name's capacity");
        assert!(sink.store_comment(0, b'!'));
        assert!(
            !sink.store_comment(1, b'?'),
            "clamped to comment's capacity"
        );

        assert_eq!(extra, [b'A', b'B']);
        assert_eq!(name, [0xAA, 0xAA, 0xAA, b'd']);
        assert_eq!(comment, [b'!']);
    }

    /// `Debug` reports capacities, never contents: a sink may be backed by a bare
    /// caller pointer whose bytes are not known to be initialized.
    #[test]
    fn sink_debug_reports_capacities_only() {
        let mut name = [0u8; 7];
        let mut name_sink = SliceSink(&mut name[..]);
        let sink = ForeignGzHeaderSink {
            extra_len: 3,
            extra: None,
            name: Some(&mut name_sink),
            comment: None,
        };
        let rendered = alloc::format!("{sink:?}");
        assert!(rendered.contains("extra_len: 3"), "{rendered}");
        assert!(rendered.contains("name_capacity: Some(7)"), "{rendered}");
        assert!(rendered.contains("extra_capacity: None"), "{rendered}");
        assert!(rendered.contains("comment_capacity: None"), "{rendered}");
    }

    // -- HeaderPublication::merged_with -------------------------------------

    /// Folding two sub-pass records must yield the record for the single C call
    /// they jointly implement: flags OR, counters add, later `Some` wins.
    #[test]
    fn merging_publications_ors_flags_and_sums_counters() {
        let first = HeaderPublication {
            // `Pending` rather than the gzip-only `NotGzip`: what is under test is
            // that the *later* record's `Some` wins, and `Pending` is available in
            // every feature configuration.
            done: Some(HeaderDone::Pending),
            text: true,
            time: false,
            os: false,
            hcrc: false,
            extra_len: Some(4),
            extra_null: false,
            extra_stored: 2,
            name_null: true,
            name_stored: 3,
            name_terminated: false,
            comment_null: false,
            comment_stored: 0,
            comment_terminated: false,
        };
        let second = HeaderPublication {
            done: Some(HeaderDone::Complete),
            text: false,
            time: true,
            os: true,
            hcrc: true,
            extra_len: Some(9),
            extra_null: true,
            extra_stored: 5,
            name_null: false,
            name_stored: 1,
            name_terminated: true,
            comment_null: true,
            comment_stored: 7,
            comment_terminated: true,
        };

        let merged = first.merged_with(second);
        assert_eq!(
            merged.done,
            Some(HeaderDone::Complete),
            "the later call wins"
        );
        assert!(merged.text && merged.time && merged.os && merged.hcrc);
        assert_eq!(merged.extra_len, Some(9), "the later declared XLEN wins");
        assert!(merged.extra_null && merged.name_null && merged.comment_null);
        assert_eq!(merged.extra_stored, 7);
        assert_eq!(merged.name_stored, 4);
        assert_eq!(merged.comment_stored, 7);
        assert!(merged.name_terminated && merged.comment_terminated);
    }

    /// Merging with an empty record is the identity, in both directions — the
    /// common case, since only the pass that reaches a field reports it.
    #[test]
    fn merging_an_empty_publication_changes_nothing() {
        let only = HeaderPublication {
            done: Some(HeaderDone::Complete),
            text: true,
            time: true,
            os: true,
            hcrc: true,
            extra_len: Some(6),
            extra_null: false,
            extra_stored: 6,
            name_null: false,
            name_stored: 9,
            name_terminated: true,
            comment_null: false,
            comment_stored: 4,
            comment_terminated: true,
        };
        let empty = HeaderPublication::default();
        assert_eq!(only.merged_with(empty), only);
        assert_eq!(empty.merged_with(only), only);
    }
}
