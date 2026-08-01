//! gzip file-I/O parity tests — a Rust port of the library-exercising behavior
//! of the C reference client `test/minigzip.c`.
//!
//! `minigzip.c` is zlib's minimal `gzip`/`gunzip`/`zcat` client. Its core
//! library exercise is a pair of loops:
//!
//! * `gz_compress(FILE *in, gzFile out)` — read the plaintext in `BUFLEN`-sized
//!   chunks and feed each to `gzwrite`, then finalize with `gzclose`.
//! * `gz_uncompress(gzFile in, FILE *out)` — read decompressed bytes back from
//!   `gzread` in `BUFLEN`-sized chunks until end of file, erroring on a negative
//!   return.
//!
//! plus `file_compress`/`file_uncompress`, which drive those loops through real
//! `<name>` / `<name>.gz` files on disk. This module reproduces that
//! *library-exercising* behavior (it deliberately does **not** port the CLI
//! argument parsing) to prove that the [`zlib_rs`] gz file-I/O layer
//! (`src/gz/`) produces and consumes real gzip files per RFC 1952:
//!
//! 1. a write → read round trip through actual files must reproduce the input
//!    byte-for-byte (several payloads, including one spanning many `BUFLEN`
//!    chunks and an incompressible one);
//! 2. a `zlib_rs`-produced `.gz` must decode with an independent reference
//!    decoder (`flate2`'s pure-Rust `miniz_oxide` backend), and a
//!    `flate2`-produced gzip file must be readable by `zlib_rs`;
//! 3. the error diagnostics of `gz_uncompress` are reproduced — a corrupt gzip
//!    stream is reported as an error, and opening a missing file fails;
//! 4. the *configuration* surface that the C client's command line drives is
//!    honored — the `outmode` string `minigzip` assembles from `-1`..`-9` and
//!    `-f`/`-h`/`-r` (`minigzip.c` L509, L525-L543), plus `gzbuffer`,
//!    `gzsetparams`, and `gzflush`;
//! 5. the descriptor-adoption, item-oriented, positioning, and status surface the
//!    client relies on — `gzdopen` (its stdin/stdout path, `minigzip.c`
//!    L549-L554), `gzfwrite`/`gzfread` (the `fread`/`fwrite` shape of the C
//!    loops), `gzeof`, `gzdirect` including the transparent copy-through of a
//!    non-gzip file, `gzclearerr`, `gzrewind`, `gzseek`, `gztell`, `gzoffset`;
//! 6. the **mandatory-`gzclose` contract**: `GzState`'s `Drop` deliberately
//!    releases resources without finalizing gzip output, so a *writer* that is
//!    dropped instead of closed leaves an unfinished member, while the identical
//!    payload closed with `gzclose_w` round-trips byte-for-byte. That asymmetry
//!    is intended, not a defect — it is the fifth, narrower deliberate choice
//!    described in AAP §0.8.2, with the reasoning in §0.6.3 — and item 6 pins it
//!    from both sides.
//!
//! Together with `tests/regression.rs`, `tests/round_trip.rs`, and
//! `tests/inflate_coverage.rs`, this driver operationalizes the requirement that
//! the port "must pass the official zlib test vectors" (User Constraint 4; see
//! AAP §0.6.7 for the driver-to-oracle mapping): the vectors are not shipped as
//! data files but embedded in the C exercisers, so it is their *assertions* that
//! are ported here, not their code.
//!
//! The whole module is gated behind the `gz-io` Cargo feature (which implies
//! `std` + `gzip`); without it the gz layer does not exist and this file
//! compiles to an empty test set. All test logic is safe Rust — there is **zero
//! `unsafe`** here — and only `std` is used for temporary files (no `tempfile`
//! dependency).

#![cfg(feature = "gz-io")]

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, process};

use zlib_rs::ReturnCode;
use zlib_rs::constants::{
    Z_BEST_COMPRESSION, Z_BEST_SPEED, Z_DEFAULT_STRATEGY, Z_FILTERED, Z_FINISH, Z_FIXED,
    Z_HUFFMAN_ONLY, Z_RLE, Z_SYNC_FLUSH,
};
use zlib_rs::crc32;
use zlib_rs::gz::{
    GzState, gzbuffer, gzclearerr, gzclose, gzclose_r, gzclose_w, gzdirect, gzdopen, gzeof,
    gzerror, gzflush, gzfread, gzfwrite, gzoffset, gzopen, gzread, gzrewind, gzseek, gzsetparams,
    gztell, gzwrite,
};

// ===========================================================================
// Constants mirrored from `minigzip.c`.
// ===========================================================================

/// Working-buffer size for the chunked compress/uncompress loops, mirroring the
/// C `#define BUFLEN 16384` (`minigzip.c` L144). Using this exact size means the
/// large payloads below flow through several `gzwrite`/`gzread` iterations, just
/// as they do in the C client.
const BUFLEN: usize = 16384;

/// gzip filename suffix appended by the `file_compress` path, mirroring the C
/// `#define GZ_SUFFIX ".gz"` (`minigzip.c` L140).
const GZ_SUFFIX: &str = ".gz";

/// zlib's `Z_OK` success code, as returned by the integer-valued gz entry points
/// (`gzclose*`, `gzbuffer`). Named locally so the assertions read like the C
/// checks (`if (gzclose(out) != Z_OK) ...`).
const Z_OK: i32 = 0;

/// Default gz working-buffer size, mirroring the C `#define GZBUFSIZE 8192`
/// (`gzguts.h`). The crate's own copy is `pub(crate)` and therefore not
/// importable from an integration test, so it is restated here rather than
/// hard-coded at each use site; the value is part of the layer's buffering
/// behavior and must never diverge from the C definition.
const GZBUFSIZE: usize = 8192;

/// `whence` value selecting an absolute seek from the start of the stream, the C
/// `<stdio.h>` `SEEK_SET`. The gz layer's own copy is private, so — exactly as
/// `tests/regression.rs` does — it is mirrored here.
const SEEK_SET: i32 = 0;

/// `whence` value selecting a seek relative to the current position, the C
/// `<stdio.h>` `SEEK_CUR`.
const SEEK_CUR: i32 = 1;

// ===========================================================================
// Temporary-file scaffolding (std only — no `tempfile` dependency).
// ===========================================================================

/// A uniquely named temporary directory for a single test, removed on drop.
///
/// Paths are built under [`std::env::temp_dir`] and made unique with the process
/// id, a per-process monotonic counter, and a nanosecond timestamp, so parallel
/// test binaries — and the parallel test threads within one binary — never
/// collide. Cleanup is best-effort: [`Drop`] removes the whole directory tree,
/// and any error (e.g. a test already removed a file) is ignored.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Creates a fresh, uniquely named scratch directory tagged with `tag`.
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "zlib_rs_gzip_compat_{tag}_{}_{nanos}_{seq}",
            process::id()
        ));
        fs::create_dir_all(&dir).expect("create unique scratch directory");
        Self { dir }
    }

    /// Returns the path to `name` inside this scratch directory.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best-effort cleanup; a failure here must never mask a test result.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ===========================================================================
// Payload generators.
// ===========================================================================

/// Builds a highly compressible, repetitive text payload of exactly `target_len`
/// bytes. Used to force multi-chunk (`> BUFLEN`) streaming through the gz layer.
fn repetitive_payload(target_len: usize) -> Vec<u8> {
    const UNIT: &[u8] = b"The quick brown fox jumps over the lazy dog. ";

    let mut v = Vec::with_capacity(target_len + UNIT.len());
    while v.len() < target_len {
        v.extend_from_slice(UNIT);
    }
    v.truncate(target_len);
    v
}

/// Builds a deterministic, effectively incompressible buffer of `len` bytes from
/// a fixed `seed`, using the `rand` dev-dependency (`rand 0.9`). A fixed seed
/// keeps the test hermetic and reproducible across runs.
fn incompressible_payload(len: usize, seed: u64) -> Vec<u8> {
    use rand::RngCore;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    let mut rng = StdRng::seed_from_u64(seed);
    let mut v = vec![0u8; len];
    rng.fill_bytes(&mut v);
    v
}

// ===========================================================================
// gz compress / uncompress helpers (ports of minigzip's inner loops).
// ===========================================================================

/// Port of `minigzip.c`'s `gz_compress` inner loop (`minigzip.c` L369-L392):
/// feed `input` to an already-open gz writer in `BUFLEN`-sized chunks via
/// `gzwrite`, asserting each chunk is fully accepted (`gzwrite` returns the
/// number of uncompressed bytes written, or `0` on error).
///
/// The caller owns opening and closing the handle; closing with `gzclose_w`
/// (not dropping) is what emits the `Z_FINISH` flush and the gzip trailer that
/// finalize a valid member.
fn gz_write_all(out: &mut GzState, input: &[u8]) {
    for chunk in input.chunks(BUFLEN) {
        let written = gzwrite(out, chunk);
        assert_eq!(
            written,
            chunk.len() as i32,
            "gzwrite accepted {written} of {} bytes",
            chunk.len()
        );
    }
}

/// Port of `minigzip.c`'s `gz_uncompress` inner loop (`minigzip.c` L397-L414):
/// read from an already-open gz reader in `BUFLEN`-sized chunks via `gzread`
/// until end of file (`0`), returning the concatenated plaintext. A negative
/// `gzread` return is a decode error and fails the test, mirroring the C
/// `if (len < 0) error(gzerror(in, &err));`.
fn gz_read_all(inp: &mut GzState) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; BUFLEN];
    loop {
        let n = gzread(inp, &mut buf);
        assert!(n >= 0, "gzread reported an error (returned {n})");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

/// End-to-end round trip through a real file: compress `payload` to `path` with
/// the given open `mode` (via [`gz_write_all`] + `gzclose_w`), then read it back
/// (via [`gz_read_all`] + `gzclose_r`), returning the decompressed bytes. This is
/// the `gz_compress` + `gz_uncompress` pairing of `minigzip.c`, run against disk.
fn round_trip(path: &Path, mode: &str, payload: &[u8]) -> Vec<u8> {
    {
        let mut out = gzopen(path, mode).expect("gzopen for writing should succeed");
        gz_write_all(&mut out, payload);
        assert_eq!(gzclose_w(out), Z_OK, "gzclose_w should finalize cleanly");
    }

    let mut inp = gzopen(path, "rb").expect("gzopen for reading should succeed");
    let restored = gz_read_all(&mut inp);
    assert_eq!(gzclose_r(inp), Z_OK, "gzclose_r should close cleanly");
    restored
}

/// Decodes `bytes` as a gzip member with the independent reference decoder
/// (`flate2`'s pure-Rust `miniz_oxide` backend — no C toolchain involved),
/// returning both the outcome and whatever plaintext was recovered before it.
///
/// Both halves matter: a *complete* member must decode with `Ok`, and a member
/// left unfinished must fail while still yielding a valid *prefix* of the
/// payload. Returning the partial output rather than discarding it is what lets
/// the caller assert the prefix property.
///
/// `flate2` is imported inside the function body, matching the deliberate local
/// scoping used by [`gz_output_is_valid_gzip`]: the reference decoder is a
/// dev-only cross-check and never becomes a runtime dependency of the crate.
fn flate2_decode(bytes: &[u8]) -> (std::io::Result<()>, Vec<u8>) {
    use flate2::read::GzDecoder;

    let mut plain = Vec::new();
    let outcome = GzDecoder::new(bytes).read_to_end(&mut plain).map(|_| ());
    (outcome, plain)
}

/// Builds the 8-byte RFC 1952 gzip trailer for `payload`: the CRC-32 of the
/// *uncompressed* data followed by ISIZE, each 4 bytes little-endian
/// (`doc/rfc1952.txt` §2.3.1, and the `crc32`/`ISIZE` pair emitted by
/// `deflate`'s gzip wrap-up).
///
/// `gzclose_w` appends exactly these eight bytes; a writer that is merely
/// dropped never emits them. Comparing against this value is therefore a direct,
/// implementation-independent test for "was this member finalized?".
///
/// ISIZE is defined as the length modulo 2^32, so the narrowing cast is the
/// specified behavior rather than a lossy convenience (every payload here is
/// orders of magnitude below that bound anyway).
fn gzip_trailer(payload: &[u8]) -> [u8; 8] {
    let mut trailer = [0u8; 8];
    trailer[..4].copy_from_slice(&crc32(0, payload).to_le_bytes());
    trailer[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    trailer
}

// ===========================================================================
// Phase 2 — in-memory (through-file) gz round trips (core parity).
// ===========================================================================

/// Core parity: write payloads to a `.gz` file and read them back, asserting the
/// decompressed bytes exactly equal the originals. Covers the canonical
/// `minigzip` literal, an empty payload (a valid empty gzip member), a large
/// repetitive payload spanning multiple `BUFLEN` chunks, and an incompressible
/// random buffer (which forces stored/near-stored DEFLATE blocks).
#[test]
fn gz_write_then_read_round_trip() {
    let scratch = Scratch::new("round_trip");

    // Deliberately non-`BUFLEN`-aligned so the final short chunk is exercised.
    let large = repetitive_payload(BUFLEN * 3 + 123);
    let random = incompressible_payload(40_000, 0x5EED_1234_C0FF_EE01);

    let cases: &[(&str, &[u8])] = &[
        ("hello", &b"hello, hello!"[..]),
        ("empty", &b""[..]),
        ("large", &large[..]),
        ("random", &random[..]),
    ];

    for &(name, payload) in cases {
        let path = scratch.path(&format!("{name}{GZ_SUFFIX}"));
        let restored = round_trip(&path, "wb", payload);
        assert_eq!(
            &restored[..],
            payload,
            "round trip changed the '{name}' payload ({} bytes)",
            payload.len()
        );
    }
}

/// Mirrors `minigzip`'s `gzbuffer` usage: set a non-default internal buffer size
/// on a freshly opened handle (before any I/O allocates the buffers) and confirm
/// the round trip still holds. Also asserts the C contract that `gzbuffer` after
/// I/O has begun is rejected (`-1`).
#[test]
fn gz_buffer_setting() {
    let scratch = Scratch::new("buffer");
    let path = scratch.path("buffered.gz");
    let payload = repetitive_payload(BUFLEN * 2 + 7);

    // A non-default buffer size (smaller than the 8192 default), valid because it
    // is >= 8 and can be doubled without overflow.
    let custom = 4096u32;

    {
        let mut out = gzopen(&path, "wb").expect("gzopen for writing");
        assert_eq!(
            gzbuffer(&mut out, custom),
            0,
            "gzbuffer before any I/O must succeed"
        );
        gz_write_all(&mut out, &payload);
        // The buffers are now allocated, so a second call must be rejected
        // (the C `state->size != 0` guard).
        assert_eq!(
            gzbuffer(&mut out, custom),
            -1,
            "gzbuffer after I/O has begun must fail with -1"
        );
        assert_eq!(gzclose_w(out), Z_OK, "finalize buffered writer");
    }

    let mut inp = gzopen(&path, "rb").expect("gzopen for reading");
    assert_eq!(
        gzbuffer(&mut inp, custom),
        0,
        "gzbuffer on a fresh reader must succeed"
    );
    let restored = gz_read_all(&mut inp);
    assert_eq!(gzclose_r(inp), Z_OK, "close buffered reader");

    assert_eq!(&restored[..], &payload[..], "buffered round trip mismatch");
}

// ===========================================================================
// Phase 3 — named-file compress / uncompress (file_compress / file_uncompress).
// ===========================================================================

/// Port of `file_compress` + `file_uncompress` (`minigzip.c` L421-L484): write a
/// plaintext file `foo`, compress it to `foo.gz` (appending [`GZ_SUFFIX`]), then
/// decompress `foo.gz` back to `foo2`, asserting the final bytes equal the
/// original.
///
/// Deviation from `minigzip`: the C client `unlink`s the source between steps;
/// this test keeps every file so it can compare against the original plaintext
/// (held in memory as well), then relies on [`Scratch`]'s drop for cleanup. The
/// compress step is run at several open "mode" strings carrying a level digit —
/// default (`"wb"`), best speed (`"wb1"`), and best compression (`"wb9"`) — each
/// of which must produce a valid gzip file that round-trips identically.
#[test]
fn file_compress_then_uncompress() {
    let scratch = Scratch::new("file");

    // Plaintext source `foo`, with a distinctive tail so a truncated/misaligned
    // round trip would be caught.
    let original = {
        let mut v = repetitive_payload(BUFLEN + 4096);
        v.extend_from_slice(b"\n-- trailing distinct bytes --\n");
        v
    };
    let src = scratch.path("foo");
    fs::write(&src, &original).expect("write plaintext source `foo`");

    for mode in ["wb", "wb1", "wb9"] {
        // file_compress equivalent: read `foo`, produce `foo.gz`.
        let gz_path = scratch.path(&format!("foo{GZ_SUFFIX}"));
        let input = fs::read(&src).expect("read plaintext source");
        {
            let mut out = gzopen(&gz_path, mode).expect("gzopen `foo.gz` for writing");
            gz_write_all(&mut out, &input);
            assert_eq!(gzclose_w(out), Z_OK, "finalize `foo.gz` at mode {mode}");
        }

        // file_uncompress equivalent: read `foo.gz`, produce `foo2`. Closed via
        // the `gzclose` dispatcher (which routes a reader to `gzclose_r`).
        let dst = scratch.path("foo2");
        let mut inp = gzopen(&gz_path, "rb").expect("gzopen `foo.gz` for reading");
        let restored = gz_read_all(&mut inp);
        assert_eq!(gzclose(inp), Z_OK, "close `foo.gz` reader at mode {mode}");
        fs::write(&dst, &restored).expect("write decompressed `foo2`");

        let final_bytes = fs::read(&dst).expect("read decompressed `foo2`");
        assert_eq!(
            final_bytes, original,
            "named-file round trip mismatch at mode {mode}"
        );

        // Drop the intermediate `.gz` before the next mode (the scratch dir is
        // removed on drop regardless).
        let _ = fs::remove_file(&gz_path);
    }
}

// ===========================================================================
// Phase 4 — interop: a zlib_rs `.gz` is valid gzip, and vice versa.
// ===========================================================================

/// Proves RFC 1952 gzip-format correctness against an independent reference: a
/// `zlib_rs`-produced `.gz` must decode with `flate2`'s `GzDecoder` (its
/// pure-Rust `miniz_oxide` backend, so no C toolchain is involved), and a
/// `flate2`-produced gzip file must be readable by the `zlib_rs` gz reader.
#[test]
fn gz_output_is_valid_gzip() {
    use flate2::Compression;
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;

    let scratch = Scratch::new("interop");
    let payload = repetitive_payload(BUFLEN * 2 + 321);

    // Forward: zlib_rs writes the `.gz`, flate2 decodes it.
    let forward = scratch.path("forward.gz");
    {
        let mut out = gzopen(&forward, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, &payload);
        assert_eq!(gzclose_w(out), Z_OK, "finalize zlib_rs `.gz`");
    }
    let mut decoded = Vec::new();
    {
        let f = fs::File::open(&forward).expect("open zlib_rs `.gz`");
        let mut dec = GzDecoder::new(f);
        dec.read_to_end(&mut decoded)
            .expect("flate2 must decode the zlib_rs `.gz`");
    }
    assert_eq!(
        decoded, payload,
        "flate2 did not reproduce the payload from the zlib_rs `.gz`"
    );

    // Reverse: flate2 writes the `.gz`, zlib_rs reads it.
    let reverse = scratch.path("reverse.gz");
    {
        let f = fs::File::create(&reverse).expect("create flate2 `.gz`");
        let mut enc = GzEncoder::new(f, Compression::best());
        enc.write_all(&payload).expect("flate2 encode payload");
        enc.finish().expect("flate2 finalize gzip stream");
    }
    let mut inp = gzopen(&reverse, "rb").expect("gzopen flate2 `.gz` for reading");
    let restored = gz_read_all(&mut inp);
    assert_eq!(gzclose_r(inp), Z_OK, "close flate2 `.gz` reader");
    assert_eq!(
        &restored[..],
        &payload[..],
        "zlib_rs did not reproduce the payload from the flate2 `.gz`"
    );
}

// ===========================================================================
// Phase 5 — error / edge behavior (mirror minigzip diagnostics).
// ===========================================================================

/// Mirrors `gz_uncompress`'s negative-return diagnostic: a corrupt gzip stream
/// must be reported as an error by the reader.
///
/// A purely random file cannot be used — the gz reader treats input without the
/// gzip magic as a *transparent* stream and copies it through verbatim (not an
/// error) — and a data error before any output is produced is accepted as
/// trailing junk. So we take a fully valid `zlib_rs`-produced `.gz` and corrupt
/// its trailing CRC-32 integrity field: the payload decodes in full (marking the
/// member as genuine, not junk), and then the integrity check fails, yielding a
/// real `Z_DATA_ERROR`. The gzip trailer is the last 8 bytes — CRC-32 then ISIZE,
/// each 4-byte little-endian — so flipping a byte at `len - 8` corrupts the CRC.
#[test]
fn gzread_on_truncated_is_error() {
    let scratch = Scratch::new("corrupt");

    // Produce a fully valid gzip file first (large enough that output is emitted
    // well before the trailer is reached).
    let good = scratch.path("good.gz");
    let payload = repetitive_payload(BUFLEN + 1000);
    {
        let mut out = gzopen(&good, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, &payload);
        assert_eq!(gzclose_w(out), Z_OK, "finalize valid `.gz`");
    }

    // Corrupt the CRC-32 field in the trailer.
    let mut bytes = fs::read(&good).expect("read valid `.gz`");
    assert!(
        bytes.len() > 8,
        "a valid gzip stream must carry an 8-byte trailer"
    );
    let crc_pos = bytes.len() - 8;
    bytes[crc_pos] ^= 0xFF;
    let bad = scratch.path("bad.gz");
    fs::write(&bad, &bytes).expect("write corrupted `.gz`");

    // Reading must surface an error: some call to `gzread` returns a negative
    // value, exactly as `gz_uncompress` detects with `if (len < 0)`.
    let mut inp = gzopen(&bad, "rb").expect("gzopen corrupted `.gz`");
    let mut buf = vec![0u8; BUFLEN];
    let mut saw_error = false;
    loop {
        let n = gzread(&mut inp, &mut buf);
        if n < 0 {
            saw_error = true;
            break;
        }
        if n == 0 {
            break; // clean end of file — no error observed
        }
    }
    assert!(
        saw_error,
        "a corrupted gzip trailer was not reported as a read error"
    );

    // The error must also be observable through `gzerror` (minigzip prints the
    // message via `gzerror(in, &err)`): a non-zero code and a non-empty message.
    let mut errnum = 0i32;
    let msg = gzerror(&inp, Some(&mut errnum));
    assert_eq!(
        errnum,
        ReturnCode::DataError.as_c_int(),
        "expected Z_DATA_ERROR from the corrupted stream, got code {errnum} ({msg:?})"
    );
    assert!(!msg.is_empty(), "gzerror should provide an error message");

    // A read handle whose last error is a data error still closes as Z_OK (only a
    // pending Z_BUF_ERROR is preserved across close).
    assert_eq!(gzclose_r(inp), Z_OK, "close corrupted-stream reader");
}

/// Mirrors `minigzip`'s NULL-handle check after `gzopen`: opening a nonexistent
/// path for reading fails. The idiomatic API returns `Err(ReturnCode::ErrNo)`
/// (the C API returns `NULL` after a failed `open`).
#[test]
fn open_nonexistent_is_error() {
    let scratch = Scratch::new("missing");
    let missing = scratch.path("does_not_exist.gz");
    assert!(
        !missing.exists(),
        "precondition: the target file must be absent"
    );

    match gzopen(&missing, "rb") {
        Ok(_handle) => panic!("gzopen on a nonexistent path unexpectedly succeeded"),
        Err(code) => assert_eq!(
            code,
            ReturnCode::ErrNo,
            "expected Z_ERRNO for a missing file, got {code:?}"
        ),
    }
}

// ===========================================================================
// Phase 6 — the configuration surface behind minigzip's command line.
//
// `minigzip` never exposes a settings API of its own: every flag it accepts is
// translated into either a character of the `gzopen` mode string or a call into
// the gz configuration family. These tests drive that translation directly.
// ===========================================================================

/// Port of `minigzip`'s `outmode` construction (`minigzip.c` L509, L525-L543):
/// the client starts from the literal `"wb6 "`, overwrites `outmode[2]` with the
/// digit of a `-1`..`-9` flag, and overwrites `outmode[3]` with `'f'` for `-f`
/// (`Z_FILTERED`), `'h'` for `-h` (`Z_HUFFMAN_ONLY`), or `'R'` for `-r`
/// (`Z_RLE`), zeroing it when no strategy flag was given.
///
/// Two properties are asserted for every resulting mode string:
///
/// * it produces a **valid gzip member** — one that `zlib_rs` reads back
///   byte-for-byte and that the independent reference decoder also accepts; and
/// * the strategy letter is **not silently ignored**. `Z_HUFFMAN_ONLY` and
///   `Z_RLE` cannot exploit the payload's long-range repetition (RLE only
///   matches at distance 1, and Huffman-only emits no matches at all), so both
///   must yield a strictly *larger* member than the default strategy at the same
///   level. Were the mode characters dropped on the floor, every size would be
///   identical and this assertion would fail — which is precisely the regression
///   a round-trip-only test cannot see.
///
/// The literal `"wb6 "` form is included deliberately: the trailing space is the
/// C initial value before a `-f`/`-h`/`-r` flag overwrites it, and the mode
/// parser must ignore unknown characters exactly as the C code does.
#[test]
fn minigzip_outmode_level_and_strategy_flags() {
    let scratch = Scratch::new("outmode");

    // Long-range repetition (a 45-byte repeating unit) is what separates the
    // default strategy from Z_RLE / Z_HUFFMAN_ONLY, and the length spans several
    // `BUFLEN` chunks so the choice is exercised across many blocks.
    let payload = repetitive_payload(BUFLEN * 2 + 77);

    // Every mode `minigzip` can build, in the shape it builds them.
    let cases: &[(&str, &str)] = &[
        ("wb6", "no flag (outmode[3] zeroed)"),
        ("wb6 ", "literal C initial value; the space must be ignored"),
        ("wb6f", "-f => Z_FILTERED"),
        ("wb6h", "-h => Z_HUFFMAN_ONLY"),
        ("wb6R", "-r => Z_RLE"),
        ("wb1", "-1 => Z_BEST_SPEED"),
        ("wb9", "-9 => Z_BEST_COMPRESSION"),
        ("wb9R", "-9 -r combined"),
    ];

    let mut sizes = Vec::with_capacity(cases.len());
    for &(mode, what) in cases {
        // One file per mode so a failure names the mode that produced it, and so
        // the sizes can be compared afterwards.
        let path = scratch.path(&format!("outmode_{}{GZ_SUFFIX}", mode.trim()));
        let restored = round_trip(&path, mode, &payload);
        assert_eq!(
            &restored[..],
            &payload[..],
            "mode {mode:?} ({what}) did not round trip"
        );

        // The member must also satisfy an independent decoder, so "valid gzip"
        // is not merely this crate agreeing with itself.
        let bytes = fs::read(&path).expect("read the produced `.gz`");
        let (outcome, decoded) = flate2_decode(&bytes);
        outcome.unwrap_or_else(|e| panic!("mode {mode:?} ({what}) is not valid gzip: {e}"));
        assert_eq!(
            decoded, payload,
            "the reference decoder disagreed for mode {mode:?} ({what})"
        );
        assert_eq!(
            &bytes[bytes.len() - 8..],
            &gzip_trailer(&payload)[..],
            "mode {mode:?} ({what}) did not append the RFC 1952 CRC-32/ISIZE trailer"
        );

        sizes.push((mode, bytes.len()));
    }

    let size_of = |wanted: &str| -> usize {
        sizes
            .iter()
            .find(|&&(mode, _)| mode == wanted)
            .map(|&(_, len)| len)
            .unwrap_or_else(|| panic!("mode {wanted:?} was not exercised"))
    };

    // The strategy letters reached the engine.
    let default6 = size_of("wb6");
    assert_eq!(
        size_of("wb6 "),
        default6,
        "the trailing space of the C literal `\"wb6 \"` must be ignored, \
         so it must compress identically to \"wb6\""
    );
    assert!(
        size_of("wb6h") > default6,
        "-h (Z_HUFFMAN_ONLY) emits no matches, so it cannot compress a repetitive \
         payload as well as the default strategy: got {} vs {default6} bytes",
        size_of("wb6h")
    );
    assert!(
        size_of("wb6R") > default6,
        "-r (Z_RLE) only matches at distance 1, so it cannot exploit the 45-byte \
         repeating unit: got {} vs {default6} bytes",
        size_of("wb6R")
    );
    assert!(
        size_of("wb9R") > size_of("wb9"),
        "-r must change the output at level 9 as well: got {} vs {} bytes",
        size_of("wb9R"),
        size_of("wb9")
    );
}

/// Mirrors the mid-stream reconfiguration `gzsetparams` exists for (the gz-layer
/// analogue of `deflateParams` re-dispatch, ported from `gzlib.c`): open at one
/// level, write, change both level and strategy, write more, and prove the
/// resulting single member still decodes to the concatenation byte-for-byte.
///
/// The C contract's rejections are pinned alongside the success path: an invalid
/// strategy and a call on a read handle must both be `Z_STREAM_ERROR`, and
/// neither may poison the stream — the writer must still finalize cleanly
/// afterwards.
#[test]
fn gzsetparams_midstream_round_trip() {
    let scratch = Scratch::new("setparams");
    let path = scratch.path(&format!("params{GZ_SUFFIX}"));

    // Both halves comfortably exceed GZBUFSIZE so the engine is genuinely live
    // (`state.size != 0`) when the parameters change, taking gzsetparams'
    // flush-then-reconfigure branch rather than its "buffers not yet allocated"
    // shortcut.
    let head = repetitive_payload(GZBUFSIZE + 808);
    let tail = incompressible_payload(GZBUFSIZE + 1616, 0x0BAD_F00D_1234_5678);
    let mut whole = head.clone();
    whole.extend_from_slice(&tail);

    {
        let mut out = gzopen(&path, "wb1").expect("gzopen at level 1 for writing");
        gz_write_all(&mut out, &head);

        assert_eq!(
            gzsetparams(&mut out, Z_BEST_COMPRESSION, Z_RLE),
            Z_OK,
            "gzsetparams on a live writer must succeed"
        );
        gz_write_all(&mut out, &tail);

        assert_eq!(
            gzsetparams(&mut out, Z_BEST_SPEED, Z_HUFFMAN_ONLY),
            Z_OK,
            "a second mid-stream reconfiguration must also succeed"
        );

        // An out-of-range strategy is rejected without disturbing the stream.
        // `Z_FIXED` is the largest valid strategy, so one past it is the first
        // invalid value.
        assert_eq!(
            gzsetparams(&mut out, Z_BEST_SPEED, Z_FIXED + 1),
            ReturnCode::StreamError.as_c_int(),
            "an invalid strategy must be rejected with Z_STREAM_ERROR"
        );

        assert_eq!(
            gzclose_w(out),
            Z_OK,
            "the writer must still finalize after a rejected gzsetparams"
        );
    }

    let bytes = fs::read(&path).expect("read the reconfigured `.gz`");
    let (outcome, decoded) = flate2_decode(&bytes);
    outcome.expect("a mid-stream reconfigured member must still be valid gzip");
    assert_eq!(
        decoded, whole,
        "the reference decoder did not reproduce the concatenated payload"
    );
    assert_eq!(
        &bytes[bytes.len() - 8..],
        &gzip_trailer(&whole)[..],
        "the trailer must cover the whole logical payload, not just one half"
    );

    let mut inp = gzopen(&path, "rb").expect("gzopen the reconfigured `.gz`");
    let restored = gz_read_all(&mut inp);

    // A reader is not a legal target for gzsetparams.
    assert_eq!(
        gzsetparams(&mut inp, Z_BEST_SPEED, Z_DEFAULT_STRATEGY),
        ReturnCode::StreamError.as_c_int(),
        "gzsetparams on a read handle must be Z_STREAM_ERROR"
    );
    assert_eq!(gzclose_r(inp), Z_OK, "close the reconfigured reader");
    assert_eq!(
        &restored[..],
        &whole[..],
        "zlib_rs did not reproduce the concatenated payload"
    );

    // Reconfiguring *before* any write — while `state.size` is still 0 and the
    // engine has not been allocated — takes gzsetparams' other branch and must
    // still produce a valid member at the newly requested settings.
    let early = scratch.path(&format!("early{GZ_SUFFIX}"));
    {
        let mut out = gzopen(&early, "wb").expect("gzopen for writing");
        assert_eq!(
            gzsetparams(&mut out, Z_BEST_SPEED, Z_FILTERED),
            Z_OK,
            "gzsetparams before any I/O must succeed"
        );
        gz_write_all(&mut out, &head);
        assert_eq!(gzclose_w(out), Z_OK, "finalize the early-configured writer");
    }
    let (early_outcome, early_decoded) =
        flate2_decode(&fs::read(&early).expect("read the early-configured `.gz`"));
    early_outcome.expect("a member configured before any write must be valid gzip");
    assert_eq!(
        early_decoded, head,
        "the early-configured member did not reproduce its payload"
    );
}

/// C parity for an out-of-range `gzsetparams` argument recorded *before* any
/// I/O: the call itself succeeds, and the invalid value is reported later, when
/// the deferred initialization actually reaches `deflateInit2`.
///
/// This is the exact three-stage sequence a C caller observes, and none of the
/// stages may be short-circuited:
///
/// 1. `gzsetparams(out, 6, 5)` returns `Z_OK`. C's `gzsetparams`
///    (`gzwrite.c` L630-L663) performs **no** range validation at all when the
///    buffers have not been allocated yet — it stores `state->level` and
///    `state->strategy` verbatim and returns `Z_OK`.
/// 2. The first `gzwrite` triggers `gz_init`, whose `deflateInit2` call rejects
///    `strategy < 0 || strategy > Z_FIXED` (`deflate.c` L436). C's `gz_init`
///    maps *every* `deflateInit2` failure to
///    `gz_error(state, Z_MEM_ERROR, "out of memory"); return -1`
///    (`gzwrite.c` L37-L43), so `gzwrite` returns `0` and `gzerror` reports
///    `Z_MEM_ERROR` with the message `"out of memory"`.
/// 3. `gzclose_w` propagates that recorded error rather than `Z_OK`.
///
/// The out-of-range *level* case is asserted alongside it because both
/// parameters travel the identical path: `deflate.c` L436 rejects
/// `level < 0 || level > 9` in the same guard. The positive control — every
/// valid strategy still initializing — closes the loop, proving the rejection is
/// value-driven and not a blanket refusal.
///
/// Silently substituting a default for an unrecognized value here would make the
/// invalid argument invisible to the caller, which is precisely the class of
/// silent behavior change AAP §0.7.2 standard S5 forbids and which §0.8.2 does
/// not list among the port's documented divergences.
#[test]
fn gzsetparams_out_of_range_argument_fails_at_the_deferred_init() {
    let scratch = Scratch::new("setparams_range");
    let payload = repetitive_payload(4000);

    // `Z_FIXED` is the largest valid strategy, so `Z_FIXED + 1` is the first
    // invalid positive value; `-1` is the first invalid negative one. C's guard
    // is `strategy < 0 || strategy > Z_FIXED`, so both ends must be rejected.
    for (tag, level, strategy) in [
        ("strategy_hi", 6, Z_FIXED + 1),
        ("strategy_lo", 6, -1),
        ("level_hi", 10, Z_DEFAULT_STRATEGY),
        ("level_lo", -2, Z_DEFAULT_STRATEGY),
    ] {
        let path = scratch.path(&format!("{tag}{GZ_SUFFIX}"));
        let mut out = gzopen(&path, "wb").expect("gzopen for writing");

        // Stage 1 — recorded verbatim, no validation, exactly as C does.
        assert_eq!(
            gzsetparams(&mut out, level, strategy),
            Z_OK,
            "[{tag}] gzsetparams records level={level} strategy={strategy} \
             without validating it (C gzwrite.c L630-L663)"
        );

        // Stage 2 — the first write triggers `gz_init`, which fails.
        assert_eq!(
            gzwrite(&mut out, &payload),
            0,
            "[{tag}] the write must make no progress once the deferred \
             initialization fails"
        );
        let mut errnum = 0i32;
        let msg = gzerror(&out, Some(&mut errnum));
        assert_eq!(
            errnum,
            ReturnCode::MemError.as_c_int(),
            "[{tag}] C's gz_init reports every deflateInit2 failure as \
             Z_MEM_ERROR (gzwrite.c L37-L43), got {errnum} ({msg:?})"
        );
        assert_eq!(
            msg, "out of memory",
            "[{tag}] gzerror must carry C's exact gz_init message"
        );

        // Stage 3 — the failure survives to close, so a caller that only checks
        // `gzclose_w` still learns about it.
        assert_eq!(
            gzclose_w(out),
            ReturnCode::MemError.as_c_int(),
            "[{tag}] gzclose_w must propagate the recorded failure"
        );
    }

    // Positive control: every valid strategy, paired with the extreme valid
    // levels, still initializes and round-trips. The rejection above is driven
    // by the value, not by the code path.
    for strategy in [
        Z_DEFAULT_STRATEGY,
        Z_FILTERED,
        Z_HUFFMAN_ONLY,
        Z_RLE,
        Z_FIXED,
    ] {
        for level in [Z_BEST_SPEED, Z_BEST_COMPRESSION] {
            let path = scratch.path(&format!("ok_{level}_{strategy}{GZ_SUFFIX}"));
            {
                let mut out = gzopen(&path, "wb").expect("gzopen for writing");
                assert_eq!(
                    gzsetparams(&mut out, level, strategy),
                    Z_OK,
                    "level={level} strategy={strategy} is a valid configuration"
                );
                gz_write_all(&mut out, &payload);
                let mut errnum = 0i32;
                let msg = gzerror(&out, Some(&mut errnum));
                assert_eq!(
                    errnum, 0,
                    "a valid configuration records no error, got {errnum} ({msg:?})"
                );
                assert_eq!(
                    gzclose_w(out),
                    Z_OK,
                    "a valid configuration finalizes cleanly"
                );
            }
            let (outcome, decoded) = flate2_decode(&fs::read(&path).expect("read the valid `.gz`"));
            outcome.expect("a valid configuration must produce valid gzip");
            assert_eq!(
                decoded, payload,
                "level={level} strategy={strategy} did not reproduce its payload"
            );
        }
    }
}

/// Mirrors the `gzflush` usage a streaming client needs: flush part of a member
/// to disk, keep writing into the *same* member, then finalize.
///
/// Three properties are pinned. First, `Z_SYNC_FLUSH` genuinely drains to the
/// file — the on-disk size grows from nothing to a real prefix — which is what
/// distinguishes `gzflush` from the layer's ordinary buffering. Second, a flushed
/// member is still *completable*: `gzclose_w` afterwards yields one member whose
/// trailer covers the whole logical payload and which both decoders reproduce
/// byte-for-byte. Third, the C validation contract holds — an out-of-range
/// `flush` and a call on a read handle are both `Z_STREAM_ERROR`, and a rejected
/// call must not poison the stream.
#[test]
fn gzflush_midstream_then_close_completes_member() {
    let scratch = Scratch::new("flush");
    let path = scratch.path(&format!("flushed{GZ_SUFFIX}"));

    // Split across the buffer boundary so the flush has real buffered input to
    // drain and the continuation crosses into fresh buffer space.
    let head = repetitive_payload(GZBUFSIZE - 1000);
    let tail = repetitive_payload(GZBUFSIZE + 2000);
    let mut whole = head.clone();
    whole.extend_from_slice(&tail);

    {
        let mut out = gzopen(&path, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, &head);

        // Nothing has necessarily reached the file yet; the flush must change
        // that. (The gzip header alone is 10 bytes, so a strict growth check is
        // the honest form of "the flush produced output".)
        let before = fs::metadata(&path).expect("stat the partial `.gz`").len();
        assert_eq!(
            gzflush(&mut out, Z_SYNC_FLUSH),
            Z_OK,
            "gzflush(Z_SYNC_FLUSH) on a live writer must succeed"
        );
        let after = fs::metadata(&path).expect("stat the flushed `.gz`").len();
        assert!(
            after > before,
            "gzflush must drain compressed output to the file: size stayed at {before}"
        );
        assert!(
            after > 10,
            "the flushed file must carry more than the 10-byte gzip header, got {after}"
        );

        // Out-of-range flush values are rejected; `Z_FINISH` is the largest legal
        // one, so one past it and a negative value must both fail.
        assert_eq!(
            gzflush(&mut out, Z_FINISH + 1),
            ReturnCode::StreamError.as_c_int(),
            "a flush value above Z_FINISH must be Z_STREAM_ERROR"
        );
        assert_eq!(
            gzflush(&mut out, -1),
            ReturnCode::StreamError.as_c_int(),
            "a negative flush value must be Z_STREAM_ERROR"
        );

        // The rejected calls must have left the stream usable.
        gz_write_all(&mut out, &tail);
        assert_eq!(
            gzclose_w(out),
            Z_OK,
            "the writer must finalize after a mid-stream flush"
        );
    }

    let bytes = fs::read(&path).expect("read the flushed `.gz`");
    let (outcome, decoded) = flate2_decode(&bytes);
    outcome.expect("a flushed-then-closed member must be valid gzip");
    assert_eq!(
        decoded, whole,
        "the reference decoder did not reproduce the flushed payload"
    );
    assert_eq!(
        &bytes[bytes.len() - 8..],
        &gzip_trailer(&whole)[..],
        "the trailer must cover both the flushed prefix and the continuation"
    );

    let mut inp = gzopen(&path, "rb").expect("gzopen the flushed `.gz`");
    let restored = gz_read_all(&mut inp);

    // Flushing is a write-side operation only.
    assert_eq!(
        gzflush(&mut inp, Z_SYNC_FLUSH),
        ReturnCode::StreamError.as_c_int(),
        "gzflush on a read handle must be Z_STREAM_ERROR"
    );
    assert_eq!(gzclose_r(inp), Z_OK, "close the flushed reader");
    assert_eq!(
        &restored[..],
        &whole[..],
        "zlib_rs did not reproduce the flushed payload"
    );

    // A `Z_FINISH` flush already emits the final block and trailer; the mandatory
    // `gzclose_w` that follows must still report success and must not corrupt the
    // completed member.
    let finished = scratch.path(&format!("finished{GZ_SUFFIX}"));
    {
        let mut out = gzopen(&finished, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, &head);
        assert_eq!(
            gzflush(&mut out, Z_FINISH),
            Z_OK,
            "gzflush(Z_FINISH) must succeed"
        );
        assert_eq!(
            gzclose_w(out),
            Z_OK,
            "gzclose_w after an explicit Z_FINISH must still be Z_OK"
        );
    }
    let (fin_outcome, fin_decoded) =
        flate2_decode(&fs::read(&finished).expect("read the Z_FINISH-flushed `.gz`"));
    fin_outcome.expect("a Z_FINISH-flushed member must be valid gzip");
    assert_eq!(
        fin_decoded, head,
        "the Z_FINISH-flushed member did not reproduce its payload"
    );
}

// ===========================================================================
// Phase 7 — descriptor adoption and the stdio-shaped item API.
// ===========================================================================

/// Port of `minigzip`'s stdin/stdout path (`minigzip.c` L549-L554, L579-L580),
/// which never opens by name in `-c` mode: it wraps an *already-open* descriptor
/// with `gzdopen(fileno(stdout), outmode)` / `gzdopen(fileno(stdin), "rb")`.
///
/// The idiomatic port adopts an owned [`std::fs::File`] instead of a raw `int`
/// (raw descriptors are the FFI layer's concern), so the test opens the file with
/// [`OpenOptions`], hands it over, and checks the adopted handle behaves exactly
/// like a `gzopen`ed one in both directions. The invalid-mode rejection is pinned
/// too: `gzdopen` must validate the mode string before adopting, mirroring the C
/// `NULL` return.
#[test]
fn gzdopen_adopts_an_open_descriptor() {
    let scratch = Scratch::new("dopen");
    let path = scratch.path(&format!("adopted{GZ_SUFFIX}"));
    let payload = repetitive_payload(BUFLEN + 555);

    // Write through an adopted descriptor, at an explicit level, exactly as the
    // C client does with its `outmode`.
    {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open the target file for writing");
        let mut out = gzdopen(file, "wb9").expect("gzdopen for writing should succeed");
        gz_write_all(&mut out, &payload);
        assert_eq!(gzclose_w(out), Z_OK, "finalize the adopted writer");
    }

    let bytes = fs::read(&path).expect("read the adopted-writer `.gz`");
    let (outcome, decoded) = flate2_decode(&bytes);
    outcome.expect("an adopted descriptor must still produce valid gzip");
    assert_eq!(
        decoded, payload,
        "the reference decoder did not reproduce the adopted-writer payload"
    );

    // Read it back through an adopted descriptor as well.
    let file = OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open the target file for reading");
    let mut inp = gzdopen(file, "rb").expect("gzdopen for reading should succeed");
    let restored = gz_read_all(&mut inp);
    assert_eq!(
        &restored[..],
        &payload[..],
        "the adopted reader did not reproduce the payload"
    );

    // Having consumed the whole member, the compressed cursor sits at the end of
    // the file — the C `gzoffset` contract, on an adopted descriptor.
    assert_eq!(
        gzoffset(&mut inp),
        bytes.len() as i64,
        "gzoffset after a complete read must be the compressed file length"
    );
    assert_eq!(gzclose_r(inp), Z_OK, "close the adopted reader");

    // The mode string is validated before the descriptor is adopted: `"zz"`
    // names neither read, write, nor append.
    let orphan = OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open the target file again");
    match gzdopen(orphan, "zz") {
        Ok(_handle) => panic!("gzdopen accepted a mode string with no r/w/a"),
        Err(code) => assert_eq!(
            code,
            ReturnCode::StreamError,
            "expected Z_STREAM_ERROR for an invalid gzdopen mode, got {code:?}"
        ),
    }
}

/// Exercises the item-oriented entry points that mirror the `fread`/`fwrite`
/// shape of `minigzip`'s inner loops (`minigzip.c` L381, L406): `gzfwrite` and
/// `gzfread` take an element size and a count and return the number of *complete
/// items* transferred.
///
/// A payload that is an exact multiple of the item size keeps the accounting
/// unambiguous, so the assertions pin the item counts as well as the bytes. The
/// safe-slice precondition is pinned separately, on its own throwaway handles: C
/// trusts the caller's raw pointer, whereas this port rejects a buffer smaller
/// than `size * nitems` and transfers nothing, so a sizing mistake surfaces
/// instead of being silently clamped.
#[test]
fn gzfwrite_and_gzfread_item_api() {
    /// Item size deliberately not a power of two, so a byte-count/item-count
    /// confusion cannot accidentally agree.
    const ITEM: usize = 7;
    /// Enough items that the transfer spans several GZBUFSIZE-sized buffers.
    const NITEMS: usize = 3000;

    let scratch = Scratch::new("items");
    let path = scratch.path(&format!("items{GZ_SUFFIX}"));
    let payload = repetitive_payload(ITEM * NITEMS);

    {
        let mut out = gzopen(&path, "wb").expect("gzopen for writing");
        assert_eq!(
            gzfwrite(&mut out, &payload, ITEM, NITEMS),
            NITEMS,
            "gzfwrite must report every complete item written"
        );
        assert_eq!(gzclose_w(out), Z_OK, "finalize the item-API writer");
    }

    let (outcome, decoded) = flate2_decode(&fs::read(&path).expect("read the item-API `.gz`"));
    outcome.expect("the item-API member must be valid gzip");
    assert_eq!(
        decoded, payload,
        "the reference decoder did not reproduce the item-API payload"
    );

    {
        let mut inp = gzopen(&path, "rb").expect("gzopen for reading");
        let mut buf = vec![0u8; ITEM * NITEMS];
        assert_eq!(
            gzfread(&mut inp, &mut buf, ITEM, NITEMS),
            NITEMS,
            "gzfread must report every complete item read"
        );
        assert_eq!(buf, payload, "gzfread did not reproduce the payload");
        // The member is fully consumed, so the next read is a clean end of file.
        let mut tail = [0u8; 32];
        assert_eq!(
            gzread(&mut inp, &mut tail),
            0,
            "the item-API read consumed the whole member, so this must be EOF"
        );
        assert_eq!(gzclose_r(inp), Z_OK, "close the item-API reader");
    }

    // Undersized-buffer guards. Each uses a fresh handle: recording the stream
    // error discards buffered state, so these must not share a handle with a
    // transfer whose result is being asserted.
    {
        let guard = scratch.path(&format!("guard_w{GZ_SUFFIX}"));
        let mut out = gzopen(&guard, "wb").expect("gzopen for writing");
        assert_eq!(
            gzfwrite(&mut out, b"ab", 4, 3),
            0,
            "gzfwrite must refuse a buffer smaller than size * nitems"
        );
        // The handle is still closeable; only the transfer was refused.
        let _ = gzclose_w(out);
    }
    {
        let mut inp = gzopen(&path, "rb").expect("gzopen for reading");
        let mut small = [0u8; 4];
        assert_eq!(
            gzfread(&mut inp, &mut small, 8, 4),
            0,
            "gzfread must refuse a buffer smaller than size * nitems"
        );
        let _ = gzclose_r(inp);
    }
}

// ===========================================================================
// Phase 8 — status and positioning queries.
// ===========================================================================

/// Covers the status queries a client uses to drive its loops, and the
/// transparent copy-through path that makes `minigzip`'s `gz_uncompress` usable
/// on a file that is not gzip at all.
///
/// `gzeof` reports having *attempted* to read past the end — not merely having
/// reached it — so it is `0` immediately after the final full read and becomes
/// `1` only once a further read is requested. `gzdirect` reports `0` for a gzip
/// stream and `1` when the layer is copying bytes through verbatim, which is the
/// `How::Copy` look-ahead outcome for a file without the `1f 8b` magic and for a
/// writer opened with the `'T'` (transparent) mode flag. `gzclearerr` resets both
/// the recorded error and the past-EOF flag.
#[test]
fn gzeof_gzdirect_and_transparent_copy_path() {
    let scratch = Scratch::new("status");
    let payload = repetitive_payload(BUFLEN + 3616);

    // ---- A real gzip stream -------------------------------------------------
    let gz_path = scratch.path(&format!("status{GZ_SUFFIX}"));
    {
        let mut out = gzopen(&gz_path, "wb").expect("gzopen for writing");
        // A fresh writer has read nothing and is producing gzip, not raw bytes.
        assert_eq!(gzeof(&out), 0, "gzeof must be 0 for a write handle");
        assert_eq!(
            gzdirect(&mut out),
            0,
            "a plain writer produces a gzip stream, so gzdirect must be 0"
        );
        gz_write_all(&mut out, &payload);
        assert_eq!(gzclose_w(out), Z_OK, "finalize the status writer");
    }
    let compressed_len = fs::metadata(&gz_path).expect("stat the status `.gz`").len() as i64;

    let mut inp = gzopen(&gz_path, "rb").expect("gzopen for reading");
    assert_eq!(
        gzdirect(&mut inp),
        0,
        "the look-ahead must classify a `1f 8b` file as a gzip stream"
    );
    assert_eq!(gzeof(&inp), 0, "nothing has been read past EOF yet");

    let restored = gz_read_all(&mut inp);
    assert_eq!(&restored[..], &payload[..], "gzip read-back mismatch");
    // `gz_read_all` stops on the `0` return, which is itself the read that ran
    // past the end, so the flag is set by the time the loop exits.
    assert_eq!(
        gzeof(&inp),
        1,
        "gzeof must be 1 once a read has requested data beyond the end"
    );
    assert_eq!(
        gztell(&inp),
        payload.len() as i64,
        "gztell must report the uncompressed position"
    );
    assert_eq!(
        gzoffset(&mut inp),
        compressed_len,
        "gzoffset must report the compressed position, which is now end of file"
    );

    // A further read is a clean EOF, not an error.
    let mut probe = [0u8; 16];
    assert_eq!(
        gzread(&mut inp, &mut probe),
        0,
        "reading past the end of a complete member is EOF, not an error"
    );
    {
        let mut errnum = -1i32;
        let msg = gzerror(&inp, Some(&mut errnum));
        assert_eq!(
            errnum, Z_OK,
            "a clean end of stream must leave no error, got {errnum} ({msg:?})"
        );
    }

    // `gzclearerr` clears the past-EOF flag as well as the error slot.
    gzclearerr(&mut inp);
    assert_eq!(gzeof(&inp), 0, "gzclearerr must reset the past-EOF flag");
    {
        let mut errnum = -1i32;
        let msg = gzerror(&inp, Some(&mut errnum));
        assert_eq!(errnum, Z_OK, "gzclearerr must leave Z_OK");
        assert!(
            msg.is_empty(),
            "gzclearerr must clear the message, got {msg:?}"
        );
    }
    assert_eq!(gzclose_r(inp), Z_OK, "close the status reader");

    // ---- A file that is not gzip at all (transparent read) -----------------
    // `minigzip`'s reader copies such input straight through rather than
    // erroring, which is what `gzdirect == 1` reports.
    let plain_path = scratch.path("status_plain.txt");
    fs::write(&plain_path, &payload).expect("write the plaintext file");

    let mut plain = gzopen(&plain_path, "rb").expect("gzopen the plaintext file");
    assert_eq!(
        gzdirect(&mut plain),
        1,
        "a file without the gzip magic must be read transparently"
    );
    let copied = gz_read_all(&mut plain);
    assert_eq!(
        &copied[..],
        &payload[..],
        "the transparent path must copy the bytes through verbatim"
    );
    assert_eq!(gzclose_r(plain), Z_OK, "close the transparent reader");

    // ---- A transparent writer (`'T'`) --------------------------------------
    let raw_path = scratch.path("status_raw.out");
    {
        let mut out = gzopen(&raw_path, "wbT").expect("gzopen a transparent writer");
        gz_write_all(&mut out, &payload);
        assert_eq!(
            gzdirect(&mut out),
            1,
            "the `'T'` mode flag must select a transparent writer"
        );
        assert_eq!(gzclose_w(out), Z_OK, "finalize the transparent writer");
    }
    assert_eq!(
        fs::read(&raw_path).expect("read the transparent output"),
        payload,
        "a transparent writer must emit the payload with no gzip framing"
    );
    // And the transparent output reads back through the same look-ahead.
    let mut back = gzopen(&raw_path, "rb").expect("gzopen the transparent output");
    assert_eq!(gzdirect(&mut back), 1, "transparent output is not gzip");
    let round_tripped = gz_read_all(&mut back);
    assert_eq!(gzclose_r(back), Z_OK, "close the transparent-output reader");
    assert_eq!(
        &round_tripped[..],
        &payload[..],
        "transparent output did not read back verbatim"
    );
}

/// Covers the uncompressed-stream cursor arithmetic: `gztell` reports the
/// position, `gzseek` moves it (absolutely with `SEEK_SET`, relatively with
/// `SEEK_CUR`), and `gzrewind` returns a reader to the anchor captured at open
/// time so the whole member can be re-read.
///
/// The write side is pinned too, because its semantics differ deliberately:
/// `gzrewind` is reader-only and must fail with `-1` on a writer, while a
/// *forward* `gzseek` on a writer is realized by compressing that many zero
/// bytes — so the gap is real data in the member, not a hole in the file.
#[test]
fn gzrewind_gzseek_and_gztell_cursor_arithmetic() {
    let scratch = Scratch::new("cursor");
    let path = scratch.path(&format!("cursor{GZ_SUFFIX}"));
    // Long enough that the seek targets below land well inside the stream and
    // several buffer refills are needed to reach them.
    let payload = repetitive_payload(BUFLEN * 2);

    {
        let mut out = gzopen(&path, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, &payload);
        assert_eq!(gzclose_w(out), Z_OK, "finalize the cursor writer");
    }

    let mut inp = gzopen(&path, "rb").expect("gzopen for reading");
    assert_eq!(gztell(&inp), 0, "a fresh reader is positioned at 0");
    assert_eq!(
        gzoffset(&mut inp),
        0,
        "a fresh reader has consumed no compressed bytes"
    );

    // Read a prefix, then check the reported position.
    const CHUNK: usize = 1000;
    let mut buf = vec![0u8; CHUNK];
    assert_eq!(
        gzread(&mut inp, &mut buf),
        CHUNK as i32,
        "the first read must be satisfied in full"
    );
    assert_eq!(&buf[..], &payload[..CHUNK], "the prefix must match");
    assert_eq!(gztell(&inp), CHUNK as i64, "gztell must follow the read");

    // Absolute forward seek, then read from the new position.
    const TARGET: usize = 5000;
    assert_eq!(
        gzseek(&mut inp, TARGET as i64, SEEK_SET),
        TARGET as i64,
        "an absolute forward seek must report the new position"
    );
    assert_eq!(gztell(&inp), TARGET as i64, "gztell must follow the seek");
    assert_eq!(
        gzread(&mut inp, &mut buf),
        CHUNK as i32,
        "the post-seek read must be satisfied in full"
    );
    assert_eq!(
        &buf[..],
        &payload[TARGET..TARGET + CHUNK],
        "the post-seek read must deliver the bytes at the seek target"
    );
    assert_eq!(gztell(&inp), (TARGET + CHUNK) as i64, "position after read");

    // Relative forward seek.
    assert_eq!(
        gzseek(&mut inp, CHUNK as i64, SEEK_CUR),
        (TARGET + 2 * CHUNK) as i64,
        "a relative seek must be applied to the current position"
    );

    // Having consumed part of the stream, some compressed input has been read.
    let consumed = gzoffset(&mut inp);
    assert!(
        consumed > 0,
        "gzoffset must have advanced after reading, got {consumed}"
    );

    // Rewind to the anchor and re-read the whole member.
    assert_eq!(
        gzrewind(&mut inp),
        Z_OK,
        "gzrewind on a reader must succeed"
    );
    assert_eq!(gztell(&inp), 0, "gzrewind must reset the position to 0");
    let restored = gz_read_all(&mut inp);
    assert_eq!(
        &restored[..],
        &payload[..],
        "the whole member must be readable again after gzrewind"
    );
    assert_eq!(gzclose_r(inp), Z_OK, "close the cursor reader");

    // ---- The write side ----------------------------------------------------
    const GAP: usize = 100;
    let seeked = scratch.path(&format!("seeked{GZ_SUFFIX}"));
    let tail: &[u8] = b"-- after the gap --";
    {
        let mut out = gzopen(&seeked, "wb").expect("gzopen for writing");
        assert_eq!(
            gzrewind(&mut out),
            -1,
            "gzrewind is reader-only and must fail with -1 on a writer"
        );
        assert_eq!(
            gzseek(&mut out, GAP as i64, SEEK_SET),
            GAP as i64,
            "a forward seek on a writer must report the new position"
        );
        assert_eq!(
            gztell(&out),
            GAP as i64,
            "gztell must follow the write seek"
        );
        gz_write_all(&mut out, tail);
        assert_eq!(gzclose_w(out), Z_OK, "finalize the seeked writer");
    }

    // The gap is materialized as compressed zero bytes, so the member decodes to
    // GAP zeros followed by the payload written after the seek.
    let mut expected = vec![0u8; GAP];
    expected.extend_from_slice(tail);
    let (outcome, decoded) = flate2_decode(&fs::read(&seeked).expect("read the seeked `.gz`"));
    outcome.expect("a member written after a forward seek must be valid gzip");
    assert_eq!(
        decoded, expected,
        "a forward write seek must be realized as compressed zero bytes"
    );

    let mut back = gzopen(&seeked, "rb").expect("gzopen the seeked `.gz`");
    let read_back = gz_read_all(&mut back);
    assert_eq!(gzclose_r(back), Z_OK, "close the seeked reader");
    assert_eq!(
        read_back, expected,
        "zlib_rs did not reproduce the zero-filled member"
    );
}

// ===========================================================================
// Phase 9 — the mandatory-`gzclose` contract (the fifth, narrower deliberate
// choice described in AAP §0.8.2).
// ===========================================================================

/// Drives one bare-drop scenario and asserts every property of the unfinished
/// member it leaves, then proves the identical payload survives intact once
/// `gzclose_w` is called.
///
/// `expect_flushed_output` selects how much the payload's own compressibility
/// lets us claim. A highly compressible payload stays inside the write buffer,
/// so a bare drop leaves only the eagerly written 10-byte header; an
/// incompressible payload forces real `Z_NO_FLUSH` output to the file first, so
/// the stronger claim "a real member was *started* and then abandoned" can be
/// made. Both must end up unfinished, which is the point.
///
/// Deliberately size-agnostic beyond that: the exact byte count of a truncated
/// member depends on the level, the strategy, and the payload, none of which this
/// contract is about.
fn assert_bare_drop_leaves_member_unfinished(
    scratch: &Scratch,
    tag: &str,
    payload: &[u8],
    expect_flushed_output: bool,
) {
    // ---- The negative case: drop, do not close -----------------------------
    let abandoned = scratch.path(&format!("{tag}_abandoned{GZ_SUFFIX}"));
    {
        let mut out = gzopen(&abandoned, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, payload);
        // THE ENTIRE POINT OF THIS TEST: the handle is dropped WITHOUT
        // `gzclose_w`. `Drop` releases the buffers, the deflate stream, and the
        // file descriptor, but deliberately emits no `Z_FINISH` and no trailer,
        // so the member on disk is left unfinished.
        drop(out);
    }

    let bytes = fs::read(&abandoned).expect("read the abandoned `.gz`");

    // A member really was started: the header is written eagerly, so the RFC 1952
    // magic and the DEFLATE method byte are present. This is asserted, not
    // merely assumed, so the test cannot pass because nothing at all was written.
    assert!(
        bytes.len() >= 10,
        "[{tag}] the eagerly written gzip header must be on disk, got {} bytes",
        bytes.len()
    );
    assert_eq!(
        &bytes[..3],
        &[0x1f, 0x8b, 0x08],
        "[{tag}] the RFC 1952 magic and CM=deflate must be present"
    );
    if expect_flushed_output {
        assert!(
            bytes.len() > GZBUFSIZE,
            "[{tag}] an incompressible payload must have driven real compressed \
             output to the file before the drop, got only {} bytes",
            bytes.len()
        );
    }

    // ...and it was left unfinished. The independent decoder cannot complete it,
    // because the final DEFLATE block and the 8-byte CRC-32/ISIZE trailer were
    // never emitted.
    let (outcome, recovered) = flate2_decode(&bytes);
    let error = outcome.expect_err(
        "a writer dropped without gzclose_w must not leave a decodable gzip \
         member; if this now succeeds, Drop for GzState has begun finishing the \
         stream, which silently swallows exactly the write errors gzclose_w \
         exists to report",
    );
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof,
        "[{tag}] the member must fail as truncated rather than corrupt: {error}"
    );
    assert!(
        recovered.len() < payload.len(),
        "[{tag}] an unfinished member cannot yield the whole payload ({} of {})",
        recovered.len(),
        payload.len()
    );
    // Whatever was recovered is still a correct prefix: the bytes that did reach
    // the file are valid, they are merely incomplete.
    assert_eq!(
        recovered.as_slice(),
        &payload[..recovered.len()],
        "[{tag}] the truncated member's contents must be a prefix of the payload"
    );

    // The trailer specifically is absent — the eight bytes `gzclose_w` would have
    // appended are not at the end of the file.
    assert_ne!(
        &bytes[bytes.len() - 8..],
        &gzip_trailer(payload)[..],
        "[{tag}] no CRC-32/ISIZE trailer may be present after a bare drop"
    );

    // This crate's own reader agrees, and reports it the way the C layer does.
    // Note the *measured* failure mode: `gzread` does not return a negative
    // value here. It reports a plain end of file (`0`) and records the
    // truncation as `Z_BUF_ERROR` "unexpected end of file" — and when output was
    // produced before the input ran out, the following read clears even that. So
    // the invariant asserted is the one that actually holds and actually matters:
    // the full payload is not recoverable.
    let mut inp = gzopen(&abandoned, "rb").expect("gzopen the abandoned `.gz`");
    let read_back = gz_read_all(&mut inp);
    assert_ne!(
        read_back, payload,
        "[{tag}] the abandoned member must not yield the payload"
    );
    assert!(
        read_back.len() < payload.len(),
        "[{tag}] the abandoned member yielded {} of {} bytes",
        read_back.len(),
        payload.len()
    );
    assert_eq!(
        read_back.as_slice(),
        &payload[..read_back.len()],
        "[{tag}] what was recovered must be a prefix of the payload"
    );
    {
        let mut errnum = i32::MIN;
        let msg = gzerror(&inp, Some(&mut errnum));
        assert!(
            errnum == Z_OK || errnum == ReturnCode::BufError.as_c_int(),
            "[{tag}] a truncated member is reported as Z_OK or Z_BUF_ERROR, \
             got {errnum} ({msg:?})"
        );
    }
    // `gzclose_r` preserves exactly a pending `Z_BUF_ERROR` and otherwise reports
    // `Z_OK` — both are correct here, and which one occurs depends on whether the
    // truncation was discovered on a fetch that produced no output.
    let close = gzclose_r(inp);
    assert!(
        close == Z_OK || close == ReturnCode::BufError.as_c_int(),
        "[{tag}] gzclose_r on a truncated stream must be Z_OK or Z_BUF_ERROR, got {close}"
    );

    // ---- The control: the same payload, closed properly --------------------
    let closed = scratch.path(&format!("{tag}_closed{GZ_SUFFIX}"));
    {
        let mut out = gzopen(&closed, "wb").expect("gzopen for writing");
        gz_write_all(&mut out, payload);
        assert_eq!(
            gzclose_w(out),
            Z_OK,
            "[{tag}] gzclose_w must finalize the member cleanly"
        );
    }
    let good = fs::read(&closed).expect("read the closed `.gz`");
    assert_eq!(
        &good[..3],
        &[0x1f, 0x8b, 0x08],
        "[{tag}] the closed member must also carry the gzip magic"
    );
    let (good_outcome, good_recovered) = flate2_decode(&good);
    good_outcome.expect("an explicitly closed member must decode");
    assert_eq!(
        good_recovered, payload,
        "[{tag}] the closed member must round-trip byte-for-byte"
    );
    // And the trailer this time *is* the correct pair — the exact eight bytes the
    // bare-drop case asserted were absent.
    assert_eq!(
        &good[good.len() - 8..],
        &gzip_trailer(payload)[..],
        "[{tag}] gzclose_w must append the RFC 1952 CRC-32 and ISIZE trailer"
    );
    let mut good_inp = gzopen(&closed, "rb").expect("gzopen the closed `.gz`");
    let good_read_back = gz_read_all(&mut good_inp);
    assert_eq!(
        gzclose_r(good_inp),
        Z_OK,
        "[{tag}] the closed member's reader must close cleanly"
    );
    assert_eq!(
        &good_read_back[..],
        payload,
        "[{tag}] zlib_rs must reproduce the closed member byte-for-byte"
    );
}

/// Pins the mandatory-`gzclose` contract: dropping a *writer* instead of closing
/// it leaves an unfinished gzip member, while the identical payload closed with
/// `gzclose_w` round-trips byte-for-byte.
///
/// **This is a contract test, not a bug report.** `impl Drop for GzState` is
/// intentionally empty of finishing logic (`src/gz/state.rs`): it releases the
/// I/O buffers, the deflate stream, and the file descriptor, but emits no
/// `Z_FINISH` flush and no gzip trailer. The reason is that a destructor cannot
/// surface a deferred compression or I/O error, so `gzclose`/`gzclose_w` remain
/// mandatory to finalize output *and* to report any pending write failure. This
/// is a documented, deliberate divergence from idiomatic Rust cleanup: AAP §0.8.2
/// records it as "a fifth, narrower deliberate choice" alongside its four
/// numbered divergences, and §0.6.3 supplies the reasoning — the alternative,
/// silently discarding a write failure during unwinding, would be strictly worse
/// than matching C's explicit-close contract. §0.8.2 adds that it "must not be
/// 'improved' into an auto-finishing destructor".
///
/// Because the divergence is invisible to any test that closes properly, and
/// because "fixing" it would change the bytes this library writes without
/// breaking compilation or a single round trip, it needs its own gate. The
/// negative half is that gate: were `Drop` ever to finish the stream, this test
/// fails. The positive half is the control that keeps the negative half honest —
/// it proves the payload, the writer, and the reader are all sound, so the
/// negative result can only be attributable to the missing `gzclose_w`.
///
/// Both payload shapes are driven, because they leave materially different files
/// behind (see [`assert_bare_drop_leaves_member_unfinished`]): a compressible
/// payload never escapes the write buffer, whereas an incompressible one forces
/// real compressed output to disk first.
#[test]
fn dropping_writer_without_gzclose_leaves_incomplete_member() {
    let scratch = Scratch::new("noclose");

    // Compressible: several BUFLEN chunks of repeating text, which `deflate`
    // keeps almost entirely inside the write buffer under Z_NO_FLUSH.
    assert_bare_drop_leaves_member_unfinished(
        &scratch,
        "compressible",
        &repetitive_payload(BUFLEN * 2),
        false,
    );

    // Incompressible: a fixed-seed random buffer far larger than GZBUFSIZE, so
    // the buffer overflows repeatedly and genuine compressed output reaches the
    // file before the drop.
    assert_bare_drop_leaves_member_unfinished(
        &scratch,
        "incompressible",
        &incompressible_payload(GZBUFSIZE * 8, 0x5EED_1234_C0FF_EE01),
        true,
    );
}
