//! Checksum conformance tests: Adler-32 and CRC-32 known-answer vectors plus
//! `*_combine` parity.
//!
//! This integration test locks down the **bit-exact** behavior of the ported
//! checksum layer ([`zlib_rs::checksum`], derived from the semantics of the C
//! `adler32.c` and `crc32.c`) against canonical known-answer vectors, and
//! verifies that the stream-combining operations (`adler32_combine`,
//! `crc32_combine`, and the `crc32_combine_gen`/`crc32_combine_op` pair)
//! reproduce the checksum of a concatenated stream. Byte-identical checksum
//! output is a defining acceptance criterion of the C -> Rust migration
//! (AAP 0.6.4 "Bit-Exact Wire Format" and AAP 0.6.6 "Numeric-Constant
//! Correctness", enforced by the AAP 0.8.1 preservation directives D-1
//! "compressed output must remain byte-for-byte identical to reference zlib"
//! and D-2 "constants must never be altered": `adler32`/`crc32` and their
//! `*_combine` counterparts must match reference zlib exactly).
//!
//! All expected constants are real, independently verified checksums, e.g. the
//! CRC-32/IEEE "check" value `crc32(0, b"123456789") == 0xCBF4_3926` and the
//! classic Adler-32 example `adler32(1, b"Wikipedia") == 0x11E6_0398`. Every
//! value mirrored from the C sources — Adler-32's `BASE`/`NMAX` and CRC-32's
//! reflected polynomial — is *asserted* here and never adjusted: under D-2 a
//! failing assertion means the expectation is wrong, not the constant.
//!
//! The tests are pure black-box exercises over the public API: no `unsafe`, no
//! internal (`crate::`) paths, and fully deterministic (the randomized
//! cross-checks are driven by a fixed RNG seed so every run is reproducible).

use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use zlib_rs::checksum::{
    adler32, adler32_combine, adler32_z, crc32, crc32_combine, crc32_combine_gen, crc32_combine_op,
    crc32_z, get_crc_table,
};

/// Adler-32 identity / seed. The C idiom `adler = adler32(0L, Z_NULL, 0)`
/// returns `1`, so `1` is the value threaded into the first real update.
const ADLER_SEED: u32 = 1;

/// CRC-32 identity / seed. The C idiom `crc = crc32(0L, Z_NULL, 0)` returns
/// `0`.
const CRC_SEED: u32 = 0;

/// `BASE` from `adler32.c` (`#define BASE 65521U`, annotated there as "largest
/// prime smaller than 65536") — the modulus of *both* Adler-32 component sums.
///
/// This is a D-2 protected constant (AAP 0.8.1, "constants must never be
/// altered"), named directly by AAP 0.6.6 "Numeric-Constant Correctness". It is
/// **mirrored** rather than imported on purpose: the implementation keeps its
/// own `BASE` private to `src/checksum/adler32.rs`, and this file is a pure
/// black-box exercise over the public API (AAP 0.5.2 import discipline), so a
/// local mirror with explicit provenance is the correct construction — exactly
/// as [`NMAX`] below is already handled.
///
/// The mirror is not taken on trust: [`adler32_constants_mirror_the_c_source`]
/// re-derives it from the property the C comment states, and
/// [`adler32_halves_are_reduced_mod_base`] proves the running implementation
/// actually reduces modulo it.
const BASE: u32 = 65521;

/// `NMAX` from `adler32.c` — the largest run length Adler-32 accumulates before
/// it must reduce modulo [`BASE`] (65521). Buffers larger than this exercise the
/// block-boundary path in `adler32` and the `len2 % BASE` reduction inside
/// `adler32_combine`.
///
/// Like [`BASE`] this is a D-2 protected constant (AAP 0.8.1) mirrored from the C
/// source; [`adler32_constants_mirror_the_c_source`] proves `5552` is the only
/// value the overflow bound in `adler32.c` admits.
const NMAX: usize = 5552;

/// Builds the concatenation `a || b` on the heap for the combine cross-checks.
fn cat(a: &[u8], b: &[u8]) -> Vec<u8> {
    [a, b].concat()
}

/// Produces `len` deterministic pseudo-random bytes from a fixed seed, keeping
/// every run of the randomized cross-checks byte-for-byte reproducible.
///
/// The combine identity asserted by the callers holds for *any* byte content,
/// so the test does not depend on the particular sequence `StdRng` emits — only
/// on it being reproducible for a given seed.
fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut buf = vec![0u8; len];
    rng.fill_bytes(&mut buf);
    buf
}

/// Asserts the Adler-32 stream-combine identity for operands `a` and `b`:
/// combining the two independently seeded checksums with `len2 = b.len()` must
/// equal the one-shot checksum of `a || b`, which in turn must equal the
/// streaming update of `b` onto the running checksum of `a`.
fn assert_adler_combine(a: &[u8], b: &[u8]) {
    let ca = adler32(ADLER_SEED, a);
    let cb = adler32(ADLER_SEED, b);
    let whole = adler32(ADLER_SEED, &cat(a, b));

    // Streaming update over the same bytes must match the one-shot result.
    assert_eq!(
        adler32(ca, b),
        whole,
        "adler32 streaming update mismatch (|a|={}, |b|={})",
        a.len(),
        b.len()
    );

    // The combine of the two component checksums must equal the whole-stream
    // checksum, using only `ca`, `cb`, and the length of `b`.
    assert_eq!(
        adler32_combine(ca, cb, b.len() as i64),
        whole,
        "adler32_combine mismatch (|a|={}, |b|={})",
        a.len(),
        b.len()
    );
}

/// Asserts the CRC-32 stream-combine identity for operands `a` and `b`,
/// mirroring [`assert_adler_combine`], and additionally checks that the
/// precomputed-operator form (`crc32_combine_gen` + `crc32_combine_op`) agrees
/// with the length form.
fn assert_crc_combine(a: &[u8], b: &[u8]) {
    let ca = crc32(CRC_SEED, a);
    let cb = crc32(CRC_SEED, b);
    let whole = crc32(CRC_SEED, &cat(a, b));
    let len2 = b.len() as i64;

    // Streaming update over the same bytes must match the one-shot result.
    assert_eq!(
        crc32(ca, b),
        whole,
        "crc32 streaming update mismatch (|a|={}, |b|={})",
        a.len(),
        b.len()
    );

    // The combine of the two component checksums must equal the whole-stream
    // checksum.
    assert_eq!(
        crc32_combine(ca, cb, len2),
        whole,
        "crc32_combine mismatch (|a|={}, |b|={})",
        a.len(),
        b.len()
    );

    // The precomputed-operator form must agree with the length form.
    let op = crc32_combine_gen(len2);
    assert_eq!(
        crc32_combine_op(ca, cb, op),
        crc32_combine(ca, cb, len2),
        "crc32_combine_op vs crc32_combine mismatch (|b|={})",
        b.len()
    );
}

// ===========================================================================
// Adler-32 known-answer vectors
// ===========================================================================

/// `adler32(1, b"")` is the seed value `1` (C: `adler32(0, Z_NULL, 0) == 1`).
#[test]
fn adler32_empty() {
    assert_eq!(adler32(ADLER_SEED, b""), 1);
}

/// A zero-length update **normalizes** the seed; it does not echo it back, and
/// it does not collapse to `1`.
///
/// This is a superset of [`adler32_empty`] and pins a deliberately subtle piece
/// of zlib parity. `adler32.c` orders its guards so that the `len == 1` fast
/// path comes *first*, the `buf == Z_NULL -> return 1L` sentinel comes second,
/// and the `len < 16` short path comes third. A non-null zero-length buffer
/// therefore reaches the third branch, whose byte loop runs zero times and which
/// still performs the closing reduction — one conditional subtraction on the
/// `adler` half and a full modulo on the `sum2` half. The Rust port reproduces
/// that ordering by having no early return for an empty slice at all: a slice
/// can never be null, so the `Z_NULL` sentinel is a pointer concern that belongs
/// exclusively to the FFI boundary and is deliberately absent from this
/// slice-taking API.
///
/// Three consequences are asserted, each of which reference zlib produces too:
///
/// 1. the identity seed survives, so `adler32(1, b"")` is still `1`;
/// 2. any *valid* running value (both halves already `< BASE`) is returned
///    unchanged, which is what makes a zero-length update safe to interleave
///    into a streaming computation;
/// 3. an out-of-range seed is reduced. `0xFFFF_FFFF` unpacks to
///    `adler = sum2 = 0xFFFF`; each half is taken modulo [`BASE`], and since
///    `0xFFFF - BASE == 0xFFFF - 0xFFF1 == 0x000E` the repacked result is
///    `0x000E_000E`.
///
/// Asserting `1` for an arbitrary seed here would be asserting the C
/// null-pointer contract against an API that has no nulls, so it is
/// deliberately *not* asserted (AAP 0.8.1 D-2: match the real contract).
#[test]
fn adler32_empty_slice_normalizes_seed() {
    // (1) The identity seed is a fixed point of the zero-length update.
    assert_eq!(adler32(ADLER_SEED, b""), 1);
    assert_eq!(adler32_z(ADLER_SEED, b""), 1);

    // (2) A valid running value round-trips through a zero-length update. This
    // is the property streaming callers rely on.
    let running = adler32(ADLER_SEED, b"some payload");
    assert_eq!(
        adler32(running, b""),
        running,
        "a zero-length update must not perturb the valid running value {running:#010x}"
    );
    assert_eq!(adler32_z(running, b""), running);

    // (3) Out-of-range seeds are reduced half-by-half modulo `BASE`. The packed
    // expectations below are derived, not guessed: `0xFFFF` reduces to
    // `0xFFFF - BASE == 0x000E`, `BASE` itself (`0xFFF1`) reduces to `0`, and a
    // half already below `BASE` is left alone.
    let normalizations: [(u32, u32); 6] = [
        (0xFFFF_FFFF, 0x000E_000E), // both halves saturated
        (0x0000_FFFF, 0x0000_000E), // low half only
        (0xFFFF_0000, 0x000E_0000), // high half only
        (0xFFF1_FFF1, 0x0000_0000), // both halves exactly `BASE`
        (0x1234_5678, 0x1234_5678), // already-valid running value, untouched
        (0x0000_0000, 0x0000_0000), // zero is already reduced
    ];
    for (seed, expected) in normalizations {
        assert_eq!(
            adler32(seed, b""),
            expected,
            "adler32({seed:#010x}, b\"\") must normalize to {expected:#010x}"
        );
        assert_eq!(
            adler32_z(seed, b""),
            expected,
            "adler32_z({seed:#010x}, b\"\") must agree with adler32"
        );
        // Whatever the seed was, the normalized halves are valid Adler-32
        // component sums, which is the whole point of the reduction.
        let normalized = adler32(seed, b"");
        assert!((normalized & 0xFFFF) < BASE);
        assert!(((normalized >> 16) & 0xFFFF) < BASE);
    }
}

/// The Adler-32 counterpart of the CRC-32/IEEE "check" value:
/// `adler32(1, b"123456789") == 0x091E_01DE`.
///
/// AAP 0.6.6 "Numeric-Constant Correctness" names exactly two live-verified
/// checksum anchors — `crc32("123456789") = 0xcbf43926` (asserted by
/// [`crc32_check_value`]) and `adler32("123456789") = 0x091e01de` — both
/// confirmed through the C ABI drop-in. This test is the second half of that
/// pair.
///
/// `"123456789"` is nine bytes, so it takes the `len < 16` short-input branch of
/// `adler32.c`: the same branch a zero-length update takes (see
/// [`adler32_empty_slice_normalizes_seed`]), which makes this vector a direct
/// check of that branch's accumulate-then-reduce sequence.
#[test]
fn adler32_check_value() {
    assert_eq!(adler32(ADLER_SEED, b"123456789"), 0x091E_01DE);
    // The `size_t`-length C variant must produce the identical value.
    assert_eq!(adler32_z(ADLER_SEED, b"123456789"), 0x091E_01DE);
}

/// The classic Adler-32 worked example: `"Wikipedia" -> 0x11E6_0398`.
#[test]
fn adler32_wikipedia() {
    assert_eq!(adler32(ADLER_SEED, b"Wikipedia"), 0x11E6_0398);
}

/// `"hello, hello!"` ties to the `hello` literal reused across the test suite;
/// the expected value is cross-checked against reference zlib.
#[test]
fn adler32_hello() {
    assert_eq!(adler32(ADLER_SEED, b"hello, hello!"), 0x2170_0496);
}

/// Feeding a buffer in chunks (chained running updates) must equal the one-shot
/// checksum — the streaming contract of `adler32.c`.
#[test]
fn adler32_incremental_equals_oneshot() {
    let whole = b"The quick brown fox jumps over the lazy dog";
    let (p1, rest) = whole.split_at(10);
    let (p2, p3) = rest.split_at(15);

    let chunked = adler32(adler32(adler32(ADLER_SEED, p1), p2), p3);
    assert_eq!(chunked, adler32(ADLER_SEED, whole));
}

/// `adler32_z` (the `size_t`-length C variant) must agree with `adler32`.
#[test]
fn adler32_z_matches_adler32() {
    let inputs: [&[u8]; 3] = [b"", b"Wikipedia", b"hello, hello!"];
    for input in inputs {
        assert_eq!(adler32_z(ADLER_SEED, input), adler32(ADLER_SEED, input));
    }
}

// ===========================================================================
// Adler-32 constant and modular-reduction invariants (AAP 0.6.6, D-2)
// ===========================================================================

/// Pins the two D-2 protected Adler-32 constants mirrored at the top of this
/// file, and — more usefully — re-derives each one from the property that makes
/// it the *only* correct value.
///
/// The direct equalities are deliberate tripwires. If a future change ever
/// "fixes" a failing assertion by editing the local [`BASE`] or [`NMAX`] mirror,
/// this test fails first and names AAP 0.8.1 D-2 as the reason, instead of the
/// edit sliding through unnoticed. The derivations then make the values
/// self-justifying rather than magic:
///
/// * [`BASE`] is prime and is the *largest* prime below `2^16`, which is what
///   `adler32.c`'s own comment claims of `65521U`.
/// * [`NMAX`] is divisible by 16, which `adler32.c` relies on when it computes
///   `n = NMAX / 16` and unrolls a full block into an exact number of `DO16`
///   groups.
/// * [`NMAX`] is the largest `n` satisfying
///   `255 * n * (n + 1) / 2 + (n + 1) * (BASE - 1) <= 2^32 - 1`, the overflow
///   bound stated in `adler32.c` and mirrored in the port's own documentation.
///   Both halves are checked: `NMAX` satisfies it and `NMAX + 1` does not, so
///   the inequality pins `5552` exactly. This is the property that makes the
///   port's plain non-wrapping `u32` arithmetic provably panic-free in debug
///   builds.
#[test]
fn adler32_constants_mirror_the_c_source() {
    // Direct mirrors of `adler32.c` (`#define BASE 65521U`, `#define NMAX 5552`).
    // Protected by AAP 0.8.1 D-2: assert, never adjust.
    assert_eq!(
        BASE, 65521,
        "BASE must mirror adler32.c's `#define BASE 65521U`"
    );
    assert_eq!(
        NMAX, 5552,
        "NMAX must mirror adler32.c's `#define NMAX 5552`"
    );

    /// Trial division; sufficient for the 16-bit range examined here.
    fn is_prime(n: u32) -> bool {
        if n < 2 {
            return false;
        }
        let mut d = 2u32;
        while d * d <= n {
            if n % d == 0 {
                return false;
            }
            d += 1;
        }
        true
    }

    // `BASE` is the largest prime below 2^16: it is prime, and nothing between
    // it and 65536 is.
    assert!(is_prime(BASE), "BASE must be prime");
    for candidate in (BASE + 1)..(1 << 16) {
        assert!(
            !is_prime(candidate),
            "{candidate} is prime, so BASE ({BASE}) would not be the largest prime below 2^16"
        );
    }

    // `NMAX` is an exact number of 16-byte `DO16` groups.
    assert_eq!(
        NMAX % 16,
        0,
        "NMAX must be divisible by 16 for the DO16 unrolling"
    );

    /// Left-hand side of `adler32.c`'s overflow bound, evaluated in `u64` so the
    /// comparison against `2^32 - 1` is itself overflow-free.
    fn worst_case_accumulator(n: u64) -> u64 {
        255 * n * (n + 1) / 2 + (n + 1) * u64::from(BASE - 1)
    }

    let limit = u64::from(u32::MAX);
    let at_nmax = worst_case_accumulator(NMAX as u64);
    let past_nmax = worst_case_accumulator(NMAX as u64 + 1);
    assert!(
        at_nmax <= limit,
        "a full NMAX block must not overflow a u32 accumulator ({at_nmax} > {limit})"
    );
    assert!(
        past_nmax > limit,
        "NMAX + 1 must overflow, otherwise NMAX would not be maximal ({past_nmax} <= {limit})"
    );
}

/// Both 16-bit halves of every Adler-32 the port produces must be strictly less
/// than [`BASE`].
///
/// This proves the `% BASE` reductions of `adler32.c` are present *and* placed
/// correctly, without restating a single opaque magic number: a missing,
/// misplaced, or skipped reduction shows up immediately as a half at or above
/// `65521`. The chosen lengths straddle every branch of the ported routine — the
/// `len == 1` fast path, the `len < 16` short path (which is also where a
/// zero-length update lands), the first complete 16-byte `DO16` group, and the
/// `NMAX` block boundary from one byte below it to four blocks beyond it — so the
/// block-boundary reduction inside the `while rest.len() >= NMAX` loop and the
/// trailing reduction after it are both exercised.
///
/// Each length is additionally checked for streaming consistency: splitting the
/// buffer and chaining two running updates must yield the identical packed
/// value, and its halves must be reduced too.
#[test]
fn adler32_halves_are_reduced_mod_base() {
    let lengths: [usize; 8] = [0, 1, 15, 16, NMAX - 1, NMAX, NMAX + 1, 4 * NMAX + 7];

    for (i, len) in lengths.into_iter().enumerate() {
        let data = deterministic_bytes(0xBA5E_0000 ^ i as u64, len);
        let sum = adler32(ADLER_SEED, &data);

        // Unpack the two component sums: the low half is C's `adler` (s1)
        // accumulator, the high half is C's `sum2` (s2) accumulator.
        let low = sum & 0xFFFF;
        let high = (sum >> 16) & 0xFFFF;
        assert!(
            low < BASE,
            "low half {low} of adler32 {sum:#010x} (len {len}) is not reduced modulo BASE {BASE}"
        );
        assert!(
            high < BASE,
            "high half {high} of adler32 {sum:#010x} (len {len}) is not reduced modulo BASE {BASE}"
        );

        // The `size_t`-length variant shares the implementation, so it must
        // agree bit for bit.
        assert_eq!(
            adler32_z(ADLER_SEED, &data),
            sum,
            "adler32_z disagrees with adler32 at len {len}"
        );

        // Chaining two running updates over the same bytes must be identical,
        // and must itself be reduced.
        let (head, tail) = data.split_at(len / 2);
        let chained = adler32(adler32(ADLER_SEED, head), tail);
        assert_eq!(
            chained, sum,
            "chained update disagrees with the one-shot checksum at len {len}"
        );
        assert!((chained & 0xFFFF) < BASE);
        assert!(((chained >> 16) & 0xFFFF) < BASE);
    }
}

// ===========================================================================
// CRC-32 known-answer vectors
// ===========================================================================

/// `crc32(0, b"")` is the seed value `0` (C: `crc32(0, Z_NULL, 0) == 0`).
#[test]
fn crc32_empty() {
    assert_eq!(crc32(CRC_SEED, b""), 0);
}

/// The standard CRC-32/IEEE "check" value for the ASCII string `"123456789"`.
#[test]
fn crc32_check_value() {
    assert_eq!(crc32(CRC_SEED, b"123456789"), 0xCBF4_3926);
}

/// A second widely published CRC-32/IEEE vector.
#[test]
fn crc32_quick_brown_fox() {
    assert_eq!(
        crc32(CRC_SEED, b"The quick brown fox jumps over the lazy dog"),
        0x414F_A339
    );
}

/// Chunked running updates must equal the one-shot checksum (mirrors the
/// `crc32_z` accumulation contract).
#[test]
fn crc32_incremental_equals_oneshot() {
    let whole = b"The quick brown fox jumps over the lazy dog";
    let (p1, rest) = whole.split_at(10);
    let (p2, p3) = rest.split_at(15);

    let chunked = crc32(crc32(crc32(CRC_SEED, p1), p2), p3);
    assert_eq!(chunked, crc32(CRC_SEED, whole));
}

/// `crc32_z` (the `size_t`-length C variant) must agree with `crc32`.
#[test]
fn crc32_z_matches_crc32() {
    let inputs: [&[u8]; 3] = [
        b"",
        b"123456789",
        b"The quick brown fox jumps over the lazy dog",
    ];
    for input in inputs {
        assert_eq!(crc32_z(CRC_SEED, input), crc32(CRC_SEED, input));
    }
}

// ===========================================================================
// CRC-32 table and polynomial invariants (AAP 0.6.6, D-2)
// ===========================================================================

/// Pins the reflected CRC-32 polynomial `0xEDB88320` through the public
/// `get_crc_table()` entry point.
///
/// The polynomial is a D-2 protected constant (AAP 0.8.1) named by AAP 0.6.6
/// "Numeric-Constant Correctness", and `get_crc_table` is the Rust counterpart of
/// C's `get_crc_table()` — declared in `zlib.h`, exported through `zlib.map`, and
/// returning the 256-entry byte-wise reflected table. Note that the table is
/// generated at build time from the polynomial rather than checked in, so this
/// test also guards the generator.
///
/// Rather than hard-coding opaque table entries, the expected values are
/// **recomputed inside the test from the polynomial constant itself**, using
/// exactly the recurrence `make_crc_table()` runs in `crc32.c`
/// (`p = i; for j in 0..8 { p = if p & 1 { (p >> 1) ^ POLY } else { p >> 1 } }`).
/// That pins `0xEDB88320` structurally: change the polynomial and all 256
/// comparisons move together, so the test cannot be satisfied by a different
/// polynomial.
///
/// # The `crc32` cross-check, and why it is not `crc32(0, &[b]) == table[b]`
///
/// `crc32.c` conditions the register with a one's complement on the way in
/// (`crc = (~crc) & 0xffffffff`) and again on the way out
/// (`return crc ^ 0xffffffff`), around the byte step
/// `crc = (crc >> 8) ^ crc_table[(crc ^ byte) & 0xff]`. The naive identity
/// `crc32(0, &[b]) == table[b]` therefore does **not** hold — measured over all
/// 256 byte values it matches zero times. Both exact relations are derived from
/// that conditioning and both are asserted below:
///
/// * Seeding with `0xFFFF_FFFF` pre-conditions the register to `0`, so a single
///   byte step yields precisely `table[b]` and the exit complement gives
///   `crc32(0xFFFF_FFFF, &[b]) == table[b] ^ 0xFFFF_FFFF`.
/// * Seeding with `0` pre-conditions the register to `0xFFFF_FFFF`, so the step
///   yields `table[b ^ 0xFF] ^ (0xFFFF_FFFF >> 8)` and the exit complement
///   gives `crc32(0, &[b]) == table[b ^ 0xFF] ^ 0xFF00_0000`.
///
/// Both were verified to hold for all 256 byte values before being written here.
#[test]
fn crc_table_encodes_reflected_polynomial() {
    /// `POLY` from `crc32.c` (`#define POLY 0xedb88320`) — the IEEE 802.3 /
    /// gzip CRC-32 polynomial in reflected form, with `x^32` implied. Mirrored
    /// locally because the port keeps its own `POLY` private; a D-2 protected
    /// constant.
    const CRC_POLY_REFLECTED: u32 = 0xEDB8_8320;

    /// Rebuilds one byte-wise table entry directly from the polynomial, mirroring
    /// the inner loop of `make_crc_table()` in `crc32.c`.
    fn expect_entry(n: u32) -> u32 {
        let mut c = n;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                CRC_POLY_REFLECTED ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        c
    }

    let table = get_crc_table();

    // Shape: exactly the 256 entries C's `get_crc_table()` promises.
    assert_eq!(
        table.len(),
        256,
        "the byte-wise CRC table must have 256 entries"
    );

    // A zero register fed a zero byte stays zero.
    assert_eq!(table[0], 0, "table[0] must be 0");

    // `n = 0x80` shifts seven times without a carry-out and then XORs the
    // polynomial into an otherwise-empty register, so `table[128]` *is* the
    // reflected polynomial. This is the most direct possible pin of the value.
    assert_eq!(
        table[128], CRC_POLY_REFLECTED,
        "table[128] must be the reflected polynomial itself"
    );

    // Every entry recomputed from the polynomial alone. Sweeping all 256 is
    // cheap and strictly stronger than spot-checking a handful of indices, so the
    // canonical spread 0, 1, 2, 127, 128, 255 is covered as a subset.
    for n in 0u32..256 {
        assert_eq!(
            table[n as usize],
            expect_entry(n),
            "table[{n}] disagrees with the value derived from POLY {CRC_POLY_REFLECTED:#010x}"
        );
    }

    // Cross-check the table against the running checksum using the two exact
    // relations derived in this test's documentation from zlib's pre- and
    // post-conditioning complements.
    for b in 0u8..=u8::MAX {
        let entry = table[usize::from(b)];

        assert_eq!(
            crc32(0xFFFF_FFFF, &[b]),
            entry ^ 0xFFFF_FFFF,
            "seed 0xFFFF_FFFF pre-conditions the register to 0, so one byte step must be table[{b}]"
        );

        assert_eq!(
            crc32(CRC_SEED, &[b]),
            table[usize::from(b ^ 0xFF)] ^ 0xFF00_0000,
            "seed 0 pre-conditions the register to 0xFFFF_FFFF, so one byte step must be \
             table[{b} ^ 0xFF] ^ 0xFF00_0000"
        );

        // The `size_t`-length variant must agree on the same single byte.
        assert_eq!(
            crc32_z(CRC_SEED, &[b]),
            crc32(CRC_SEED, &[b]),
            "crc32_z disagrees with crc32 for byte {b}"
        );
    }
}

// ===========================================================================
// Combine parity — the highest-value assertions (subtle porting bugs hide here)
// ===========================================================================

/// `adler32_combine` over several operand shapes: empty first, empty second,
/// both empty, and a range of non-trivial sizes. Combining the component
/// checksums must reproduce the checksum of the concatenation.
#[test]
fn adler32_combine_matches_concat() {
    assert_adler_combine(b"", b"");
    assert_adler_combine(b"", b"second operand only");
    assert_adler_combine(b"first operand only", b"");
    assert_adler_combine(b"abc", b"defgh");
    assert_adler_combine(b"The quick brown fox ", b"jumps over the lazy dog");
    assert_adler_combine(b"hello", b", hello!");
}

/// `crc32_combine` over the same operand shapes as
/// [`adler32_combine_matches_concat`]; also exercises the operator form via
/// [`assert_crc_combine`].
#[test]
fn crc32_combine_matches_concat() {
    assert_crc_combine(b"", b"");
    assert_crc_combine(b"", b"second operand only");
    assert_crc_combine(b"first operand only", b"");
    assert_crc_combine(b"abc", b"defgh");
    assert_crc_combine(b"The quick brown fox ", b"jumps over the lazy dog");
    assert_crc_combine(b"hello", b", hello!");
}

/// Precomputing the operator for a fixed length with `crc32_combine_gen` and
/// applying it via `crc32_combine_op` must equal calling `crc32_combine` with
/// that length — and both must equal the direct checksum of the concatenation.
#[test]
fn crc32_combine_op_matches_gen() {
    let a = b"operator-form parity check, part one";
    let b = b"operator-form parity check, the second part";
    let ca = crc32(CRC_SEED, a);
    let cb = crc32(CRC_SEED, b);
    let len2 = b.len() as i64;

    let op = crc32_combine_gen(len2);
    assert_eq!(crc32_combine_op(ca, cb, op), crc32_combine(ca, cb, len2));
    assert_eq!(crc32_combine_op(ca, cb, op), crc32(CRC_SEED, &cat(a, b)));
}

/// Combining with a zero-length second stream must return the first checksum
/// unchanged. The second checksum here is the *seed* of an empty buffer
/// (Adler-32 -> 1, CRC-32 -> 0), NOT a bare zero — passing a raw `0` as the
/// second Adler-32 checksum would be incorrect.
#[test]
fn combine_len2_zero_is_identity() {
    let a = b"some leading data for the identity check";
    let ca_adler = adler32(ADLER_SEED, a);
    let ca_crc = crc32(CRC_SEED, a);

    assert_eq!(
        adler32_combine(ca_adler, adler32(ADLER_SEED, b""), 0),
        ca_adler
    );
    assert_eq!(crc32_combine(ca_crc, crc32(CRC_SEED, b""), 0), ca_crc);
}

/// A second buffer longer than `NMAX` exercises both the block-boundary path in
/// `adler32` and the `len2 % BASE` reduction in `adler32_combine`; the same
/// large operands are cross-checked for CRC-32.
#[test]
fn combine_large_len2_crosses_nmax() {
    let a = deterministic_bytes(0x0102_0304_0506_0708, 4096);
    let b = deterministic_bytes(0x1122_3344_5566_7788, 3 * NMAX + 17);
    assert!(
        b.len() > NMAX,
        "second operand must cross the NMAX boundary"
    );

    assert_adler_combine(&a, &b);
    assert_crc_combine(&a, &b);
}

/// Negative lengths are meaningless; reference zlib returns debugging
/// sentinels — `adler32_combine` yields the invalid checksum `0xFFFF_FFFF`,
/// `crc32_combine_gen` yields `0`, and a zero operator makes
/// `crc32_combine_op`/`crc32_combine` yield `0`.
#[test]
fn combine_negative_len2_sentinels() {
    assert_eq!(adler32_combine(0x1234_5678, ADLER_SEED, -1), 0xFFFF_FFFF);
    assert_eq!(crc32_combine_gen(-1), 0);
    assert_eq!(crc32_combine(0xDEAD_BEEF, CRC_SEED, -1), 0);
    assert_eq!(crc32_combine_op(0xDEAD_BEEF, CRC_SEED, 0), 0);
}

// ===========================================================================
// Deterministic randomized cross-checks (fixed seed => reproducible)
// ===========================================================================

/// Random content across a fixed set of size pairs (including one crossing
/// `NMAX`) must satisfy the Adler-32 combine identity.
#[test]
fn adler32_combine_random_deterministic() {
    let size_pairs: [(usize, usize); 8] = [
        (1, 1),
        (7, 9),
        (16, 16),
        (31, 33),
        (100, 250),
        (1024, 1024),
        (NMAX, 64),
        (64, NMAX + 4096),
    ];
    for (i, (la, lb)) in size_pairs.into_iter().enumerate() {
        let a = deterministic_bytes(0xA11E_0000 ^ i as u64, la);
        let b = deterministic_bytes(0xB22F_0000 ^ i as u64, lb);
        assert_adler_combine(&a, &b);
    }
}

/// Random content across a fixed set of size pairs must satisfy the CRC-32
/// combine identity (length form and operator form).
#[test]
fn crc32_combine_random_deterministic() {
    let size_pairs: [(usize, usize); 8] = [
        (1, 1),
        (7, 9),
        (16, 16),
        (31, 33),
        (100, 250),
        (1024, 1024),
        (4096, 64),
        (64, 8192),
    ];
    for (i, (la, lb)) in size_pairs.into_iter().enumerate() {
        let a = deterministic_bytes(0xC33A_0000 ^ i as u64, la);
        let b = deterministic_bytes(0xD44B_0000 ^ i as u64, lb);
        assert_crc_combine(&a, &b);
    }
}
