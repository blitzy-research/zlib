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
/// | `-1` | the stream carries **no** gzip header | the `HEAD` non-gzip branch (`inflate.c` L505-L506) |
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
    /// way: `inflate.c` L505-L506 sits inside `#ifdef GUNZIP`, so a `libz` built
    /// without gzip support cannot produce `-1` either. Mirroring the
    /// preprocessor structure keeps the enum an exact model of the field in
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
    /// `FLAGS` assigned `head->text` (`inflate.c` L523-L524).
    pub(crate) text: bool,
    /// `TIME` assigned `head->time` (`inflate.c` L531-L532).
    pub(crate) time: bool,
    /// `OS` assigned `head->xflags` **and** `head->os` — one C statement pair
    /// under a single guard (`inflate.c` L539-L542).
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
    /// (`inflate.c` L643-L644).
    pub(crate) name_null: bool,
    /// Content bytes `NAME` appended to `head->name` during this call, excluding
    /// the terminator (`inflate.c` L632-L637).
    pub(crate) name_stored: usize,
    /// `NAME` stored the field's terminating NUL into `head->name`. C counts that
    /// NUL against `name_max` like any other byte, so a name that exactly fills
    /// the buffer is left **unterminated** and this stays `false`.
    pub(crate) name_terminated: bool,
    /// `COMMENT`'s no-`FCOMMENT` branch assigned `head->comment = Z_NULL`
    /// (`inflate.c` L665-L666).
    pub(crate) comment_null: bool,
    /// Content bytes `COMMENT` appended to `head->comment` during this call,
    /// excluding the terminator (`inflate.c` L654-L659).
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
    ///   (`inflate.c` L605-L606, L643-L644, L665-L666) that the incremental
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

    /// Finding #10 — `GzHeader`'s public field set is part of the crate's API, so
    /// it is pinned by an **exhaustive** struct expression and an **exhaustive**
    /// destructuring pattern (no `..` in either).
    ///
    /// Every field a caller can name is a compatibility commitment: an exhaustive
    /// struct literal in downstream code stops compiling the moment a field is
    /// added, and a `let Self { .. }` destructuring stops compiling the moment one
    /// is removed. Wire-level decoder observations therefore belong in the
    /// crate-private [`HeaderPublication`] record, not here — which is what
    /// removing the short-lived public `extra_len` field restored.
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
    /// discriminants must be exactly `-1`, `0` and `1` (`inflate.c` L505-L506,
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
}
