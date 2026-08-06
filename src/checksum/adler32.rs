//! Adler-32 checksum computation.
//!
//! This module is a faithful, memory-safe Rust port of the reference zlib
//! `adler32.c`. It computes the Adler-32 checksum that zlib-format streams
//! (RFC 1950) carry as their trailing 4-byte integrity field, and it is also
//! exposed through the public API because it is useful to applications on its
//! own.
//!
//! The implementation is pure integer arithmetic. It contains **no `unsafe`**
//! code, performs **no heap allocation**, and depends only on `core`, so it is
//! usable in `no_std` builds (this file requires no imports at all). Its output
//! is bit-identical to reference zlib for every input, which is the defining
//! acceptance criterion for the migration.
//!
//! # Algorithm
//!
//! Adler-32 maintains two 16-bit running sums, conventionally called `s1` and
//! `s2`, packed into a single [`u32`] as `(s2 << 16) | s1`:
//!
//! * `s1` is `1` plus the sum of every input byte, taken modulo `BASE`.
//! * `s2` is the running sum of `s1` after each byte, taken modulo `BASE`.
//!
//! To keep the sums from overflowing a `u32`, the input is processed in blocks
//! of at most `NMAX` bytes and both sums are reduced modulo `BASE` after
//! each block. Within a block the per-byte updates are unrolled in groups of
//! sixteen, mirroring the reference C `DO16` macro in `adler32.c`. Because the
//! arithmetic result is invariant to *where* the reductions are inserted
//! (reducing `s1` by a multiple of `BASE` shifts every later `s2` increment by
//! a multiple of `BASE`, leaving both halves unchanged modulo `BASE`), this
//! unrolling is a pure throughput optimization and remains bit-exact with the
//! plain byte-at-a-time formulation.

/// Largest prime smaller than `65536`.
///
/// Both component sums of the Adler-32 checksum are kept modulo this value.
const BASE: u32 = 65521;

/// Largest block length `n` for which the running sums cannot overflow a
/// 32-bit accumulator before the next reduction modulo [`BASE`].
///
/// Formally, `NMAX` is the largest `n` satisfying
/// `255 * n * (n + 1) / 2 + (n + 1) * (BASE - 1) <= 2^32 - 1`. Processing the
/// input in blocks no larger than `NMAX` bytes is precisely what makes the
/// plain (non-wrapping) `u32` arithmetic in this module provably free of
/// overflow — and therefore free of debug-mode panics.
const NMAX: usize = 5552;

/// Updates a running Adler-32 checksum with the bytes in `buf` and returns the
/// updated checksum.
///
/// An Adler-32 value is in the range of a 32-bit unsigned integer. A fresh
/// checksum is started from the required initial value `1`; passing an empty
/// slice takes the same zero-length path as reference zlib, normalizing each
/// 16-bit half modulo `BASE`. A valid running checksum (both halves already
/// `< BASE`) is therefore returned unchanged, so `adler32(1, b"")` returns `1`.
///
/// An Adler-32 checksum is almost as reliable as a CRC-32 but can be computed
/// much faster.
///
/// This mirrors the C `adler32(adler, buf, len)` entry point. The C contract of
/// returning the initial value `1` for a `NULL` buffer is handled at the FFI
/// boundary, where a null pointer can occur; in this idiomatic slice-based API
/// there is no null, so an empty slice is treated exactly like C's non-null
/// zero-length buffer.
///
/// # Examples
///
/// ```
/// # use zlib_rs::checksum::adler32::adler32;
/// let mut sum = adler32(1, b"");     // required initial value
/// sum = adler32(sum, b"123456789");  // fold in more data
/// assert_eq!(sum, 0x091e_01de);      // canonical Adler-32 of "123456789"
/// ```
#[must_use]
pub fn adler32(adler: u32, buf: &[u8]) -> u32 {
    adler32_z(adler, buf)
}

/// Updates a running Adler-32 checksum with the bytes in `buf` and returns the
/// updated checksum.
///
/// This is identical to [`adler32`] and exists to mirror the C `adler32_z`
/// entry point, which accepts a `size_t` length rather than an `unsigned int`
/// length. In this slice-based API both collapse to `&[u8]`, so [`adler32`]
/// simply delegates here. Both names are retained because the FFI layer exposes
/// them as distinct C symbols.
#[must_use]
pub fn adler32_z(adler: u32, buf: &[u8]) -> u32 {
    // Split the checksum into its two 16-bit component sums.
    let mut sum2 = (adler >> 16) & 0xffff;
    let mut adler = adler & 0xffff;

    // NOTE: there is deliberately no early return for an empty slice. Reference
    // zlib normalizes a non-null zero-length update through its `len < 16`
    // path — it reduces `adler` with a single conditional subtraction and
    // `sum2` modulo `BASE` — rather than echoing the packed seed back. The
    // `buf.len() < 16` branch below runs its byte loop zero times and performs
    // exactly that reduction, so `adler32(1, b"")` stays `1`, a valid running
    // value is returned unchanged, and an arbitrary seed such as `0xffff_ffff`
    // normalizes to `0x000e_000e` — matching zlib for every public seed. (The
    // C `buf == Z_NULL` → `1` sentinel is a null-pointer concern handled at the
    // FFI boundary; a slice is never null.)

    // Single-byte fast path: at most one conditional subtraction is needed for
    // each sum, so the more expensive modulo operations are avoided entirely.
    if buf.len() == 1 {
        adler += u32::from(buf[0]);
        if adler >= BASE {
            adler -= BASE;
        }
        sum2 += adler;
        if sum2 >= BASE {
            sum2 -= BASE;
        }
        return adler | (sum2 << 16);
    }

    // Short input (fewer than 16 bytes): accumulate directly, then reduce once.
    // `adler` grows by at most `15 * 255`, so a single conditional subtraction
    // reduces it; `sum2` can grow larger and needs a full modulo.
    if buf.len() < 16 {
        for &byte in buf {
            adler += u32::from(byte);
            sum2 += adler;
        }
        if adler >= BASE {
            adler -= BASE;
        }
        sum2 %= BASE;
        return adler | (sum2 << 16);
    }

    // General case: process the input in blocks of at most `NMAX` bytes so the
    // per-byte updates accumulate without overflowing a `u32` before the
    // reduction at the end of each block (`NMAX` is chosen precisely so plain,
    // non-wrapping arithmetic never overflows and never panics). Within each
    // full block the updates are unrolled sixteen at a time via [`do16`],
    // mirroring the reference C `DO16` macro; `NMAX` is divisible by 16, so a
    // full block is an exact number of 16-byte groups.
    let mut rest = buf;
    while rest.len() >= NMAX {
        let (block, tail) = rest.split_at(NMAX);
        for group in block.chunks_exact(16) {
            do16(&mut adler, &mut sum2, group);
        }
        adler %= BASE;
        sum2 %= BASE;
        rest = tail;
    }

    // Remaining bytes (fewer than `NMAX`): the whole 16-byte groups are still
    // unrolled through `DO16`, and the final short tail (fewer than 16 bytes)
    // is folded one byte at a time. A single reduction of each sum then
    // suffices; it is skipped entirely when nothing remains, mirroring the C
    // `if (len)` guard that avoids a redundant modulo.
    if !rest.is_empty() {
        let mut groups = rest.chunks_exact(16);
        for group in groups.by_ref() {
            do16(&mut adler, &mut sum2, group);
        }
        for &byte in groups.remainder() {
            adler += u32::from(byte);
            sum2 += adler;
        }
        adler %= BASE;
        sum2 %= BASE;
    }

    // Recombine the two component sums into the packed checksum.
    adler | (sum2 << 16)
}

/// Combines two Adler-32 checksums into one.
///
/// Given two byte sequences `seq1` and `seq2` with lengths `len1` and `len2`
/// and Adler-32 checksums `adler1` and `adler2` respectively, this returns the
/// Adler-32 checksum of the concatenation `seq1 || seq2`, requiring only
/// `adler1`, `adler2`, and `len2` — the length of the second sequence.
///
/// `len2` is a signed integer to mirror the C `z_off_t` / `z_off64_t` contract
/// (a single Rust `i64` covers both C widths). A negative `len2` has no
/// meaning; as a debugging aid this returns the invalid checksum `0xffff_ffff`,
/// matching the reference implementation.
#[must_use]
pub fn adler32_combine(adler1: u32, adler2: u32, len2: i64) -> u32 {
    // For a negative length, return an invalid Adler-32 value as a clue for
    // debugging (matches reference zlib behavior).
    if len2 < 0 {
        return 0xffff_ffff;
    }

    // Reduce the length modulo BASE; `rem` is the effective offset of `seq2`.
    let rem = (len2 % i64::from(BASE)) as u32;

    let sum1_lo = adler1 & 0xffff;

    // Compute `rem * sum1_lo mod BASE` through a `u64` intermediate. The product
    // can reach `65520 * 65535 = 4_293_388_200`: under `2^32`, but close enough
    // that the wider multiply removes any risk of a debug-mode overflow panic.
    let mut sum2 = ((u64::from(rem) * u64::from(sum1_lo)) % u64::from(BASE)) as u32;

    let mut sum1 = sum1_lo + (adler2 & 0xffff) + BASE - 1;
    sum2 += ((adler1 >> 16) & 0xffff) + ((adler2 >> 16) & 0xffff) + BASE - rem;

    // These conditional reductions reproduce the reference derivation exactly.
    // The two consecutive `sum1` checks, and the `2 * BASE` then `BASE` checks
    // on `sum2`, are intentional and required across the full input domain.
    if sum1 >= BASE {
        sum1 -= BASE;
    }
    if sum1 >= BASE {
        sum1 -= BASE;
    }
    if sum2 >= (BASE << 1) {
        sum2 -= BASE << 1;
    }
    if sum2 >= BASE {
        sum2 -= BASE;
    }

    sum1 | (sum2 << 16)
}

/// Folds sixteen consecutive input bytes into the running Adler-32 component
/// sums with the update sequence fully unrolled, mirroring the reference C
/// `DO16` macro from `adler32.c` (which expands to `{ adler += buf[i]; sum2 +=
/// adler; }` for `i` in `0..16`).
///
/// `chunk` is always a 16-byte group produced by `chunks_exact(16)`; binding it
/// to a fixed-size array reference performs a single length check and then lets
/// the compiler prove every index is in bounds, eliding the per-byte bounds
/// checks on the hot path. This routine is pure integer arithmetic and contains
/// no `unsafe`.
#[inline(always)]
fn do16(adler: &mut u32, sum2: &mut u32, chunk: &[u8]) {
    let bytes: &[u8; 16] = chunk
        .try_into()
        .expect("adler32 DO16 group must be exactly 16 bytes");

    // Accumulate through locals so the sixteen dependent updates stay in
    // registers; the two out-parameters are written back once at the end.
    let mut a = *adler;
    let mut s = *sum2;
    a += u32::from(bytes[0]);
    s += a;
    a += u32::from(bytes[1]);
    s += a;
    a += u32::from(bytes[2]);
    s += a;
    a += u32::from(bytes[3]);
    s += a;
    a += u32::from(bytes[4]);
    s += a;
    a += u32::from(bytes[5]);
    s += a;
    a += u32::from(bytes[6]);
    s += a;
    a += u32::from(bytes[7]);
    s += a;
    a += u32::from(bytes[8]);
    s += a;
    a += u32::from(bytes[9]);
    s += a;
    a += u32::from(bytes[10]);
    s += a;
    a += u32::from(bytes[11]);
    s += a;
    a += u32::from(bytes[12]);
    s += a;
    a += u32::from(bytes[13]);
    s += a;
    a += u32::from(bytes[14]);
    s += a;
    a += u32::from(bytes[15]);
    s += a;
    *adler = a;
    *sum2 = s;
}

#[cfg(test)]
mod tests {
    use super::{BASE, NMAX, adler32, adler32_combine, adler32_z};

    // All expected values below were produced by reference zlib
    // (Python's `zlib.adler32`), which performs the identical half-split, so an
    // arbitrary initial value is directly comparable.

    #[test]
    fn empty_slice_returns_input_unchanged() {
        // The required initial value for a fresh checksum.
        assert_eq!(adler32(1, b""), 1);
        // A *valid* running value (both halves already < BASE) is returned
        // unchanged by an empty update.
        assert_eq!(adler32(0xdead_beef, b""), 0xdead_beef);
        assert_eq!(adler32_z(0, b""), 0);
    }

    #[test]
    fn empty_slice_normalizes_arbitrary_seed_like_zlib() {
        // An empty (non-null) update takes reference zlib's zero-length path,
        // which reduces each 16-bit half modulo BASE rather than echoing the seed
        // back unreduced. Expected values are from reference zlib
        // (`zlib.adler32(b"", seed)`).
        assert_eq!(adler32(0xffff_ffff, b""), 0x000e_000e);
        assert_eq!(adler32(0x0000_ffff, b""), 0x0000_000e);
        assert_eq!(adler32(0xffff_0000, b""), 0x000e_0000);
        assert_eq!(adler32_z(0xffff_ffff, b""), 0x000e_000e);
        // Feeding an already-normalized value back is a fixed point.
        assert_eq!(adler32(0x000e_000e, b""), 0x000e_000e);
    }

    #[test]
    fn known_reference_vectors() {
        assert_eq!(adler32(1, b"a"), 0x0062_0062);
        assert_eq!(adler32(1, b"abc"), 0x024d_0127);
        assert_eq!(adler32(1, b"123456789"), 0x091e_01de);
        assert_eq!(adler32(1, b"Wikipedia"), 0x11e6_0398);
        // 15 bytes exercises the `len < 16` short path.
        assert_eq!(adler32(1, b"abcdefghijklmno"), 0x2fb7_0619);
        // 16 bytes crosses into the general (block) path.
        assert_eq!(adler32(1, b"abcdefghijklmnop"), 0x3640_0689);
    }

    #[test]
    fn adler32_and_adler32_z_agree() {
        let data = b"The quick brown fox jumps over the lazy dog";
        for len in 0..=data.len() {
            assert_eq!(adler32(1, &data[..len]), adler32_z(1, &data[..len]));
        }
    }

    #[test]
    fn byte_at_a_time_matches_single_pass() {
        // Repeatedly exercising the single-byte fast path must compose to the
        // same result as one bulk call — this validates running updates.
        let data = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut running = 1;
        for &byte in data {
            running = adler32(running, &[byte]);
        }
        assert_eq!(running, adler32(1, data));
    }

    #[test]
    fn split_update_matches_whole() {
        // For any split buf = a || b: adler32(adler32(1, a), b) == adler32(1, buf).
        let whole = b"The quick brown fox jumps over the lazy dog";
        for split in 0..=whole.len() {
            let (a, b) = whole.split_at(split);
            assert_eq!(adler32(adler32(1, a), b), adler32(1, whole));
        }
        assert_eq!(adler32(1, whole), 0x5bdc_0fda);
    }

    #[test]
    fn large_buffer_matches_reference() {
        // 20_000 bytes (> NMAX) confirms block-boundary reductions are correct.
        let mut big = [0u8; 20_000];
        for (i, byte) in big.iter_mut().enumerate() {
            *byte = ((i * 37 + 11) & 0xff) as u8;
        }
        assert_eq!(adler32(1, &big), 0xdca8_ea4b);
    }

    #[test]
    fn block_boundary_no_overflow() {
        // Worst-case all-0xff blocks with an extreme initial value stress the
        // NMAX overflow bound. These must not panic and must stay bit-exact.
        let ff_nmax = [0xffu8; NMAX];
        assert_eq!(adler32(0xffff_ffff, &ff_nmax), 0x0bab_9b99);
        assert_eq!(adler32(1, &ff_nmax), 0xf18f_9b8c);

        // Crossing exactly one block boundary.
        let ff_cross = [0xffu8; NMAX + 100];
        assert_eq!(adler32(0xffff_ffff, &ff_cross), 0x7e65_ff35);

        // Crossing two block boundaries.
        let ff_two = [0xffu8; 2 * NMAX + 7];
        assert_eq!(adler32(1, &ff_two), 0x9d7b_3e1f);
    }

    #[test]
    fn general_path_do16_matches_reference() {
        // Directly exercises the DO16-unrolled general path at and around the
        // NMAX block boundary: exactly NMAX (whole 16-groups, no tail), a short
        // (<16) tail, one extra full group, a mid-size tail, exact multi-block,
        // multi-block with a tail, and three full blocks. Expected values are
        // reference zlib (`zlib.adler32`) for `((i*37+11) & 0xff)`.
        fn pattern(n: usize) -> Vec<u8> {
            (0..n).map(|i| ((i * 37 + 11) & 0xff) as u8).collect()
        }
        let cases: [(usize, u32); 7] = [
            (NMAX, 0x6b2c_cc6f),         // exactly one block; no remainder
            (NMAX + 15, 0xa4f9_d3d1),    // block + <16-byte tail
            (NMAX + 16, 0x797f_d477),    // block + one extra whole group
            (NMAX + 100, 0x6c4e_fee9),   // block + 6 groups + 4-byte tail
            (2 * NMAX, 0xdcf9_9aec),     // exactly two blocks
            (2 * NMAX + 9, 0x6546_9f63), // two blocks + short tail
            (3 * NMAX, 0x5276_6869),     // three blocks
        ];
        for (n, expected) in cases {
            let data = pattern(n);
            assert_eq!(adler32(1, &data), expected, "bulk mismatch at n={n}");
            // Self-consistency: folding the same bytes one at a time (the
            // single-byte fast path) must compose to the unrolled bulk result,
            // independent of any external reference.
            let mut running = 1;
            for &byte in &data {
                running = adler32(running, &[byte]);
            }
            assert_eq!(running, expected, "byte-at-a-time mismatch at n={n}");
        }
    }

    #[test]
    fn combine_matches_concatenation() {
        let whole = b"The quick brown fox jumps over the lazy dog";
        let (a, b) = whole.split_at(20);
        let c1 = adler32(1, a);
        let c2 = adler32(1, b);
        assert_eq!(c1, 0x4c62_0734);
        assert_eq!(c2, 0x69d6_08a7);
        assert_eq!(adler32_combine(c1, c2, b.len() as i64), adler32(1, whole));
        assert_eq!(adler32_combine(c1, c2, b.len() as i64), 0x5bdc_0fda);
    }

    #[test]
    fn combine_large_matches_concatenation() {
        let mut big = [0u8; 20_000];
        for (i, byte) in big.iter_mut().enumerate() {
            *byte = ((i * 37 + 11) & 0xff) as u8;
        }
        let (a, b) = big.split_at(8_000);
        let combined = adler32_combine(adler32(1, a), adler32(1, b), b.len() as i64);
        assert_eq!(combined, adler32(1, &big));
        assert_eq!(combined, 0xdca8_ea4b);
    }

    #[test]
    fn combine_with_empty_second_sequence_is_identity() {
        // Combining with a zero-length second sequence returns the first checksum.
        let c1 = adler32(1, b"The quick brown fox ");
        let empty = adler32(1, b"");
        assert_eq!(adler32_combine(c1, empty, 0), c1);
    }

    #[test]
    fn combine_negative_length_returns_invalid_marker() {
        assert_eq!(adler32_combine(0x4c62_0734, 0x69d6_08a7, -1), 0xffff_ffff);
        assert_eq!(adler32_combine(1, 1, -12345), 0xffff_ffff);
    }

    #[test]
    fn constants_have_expected_values() {
        assert_eq!(BASE, 65_521);
        assert_eq!(NMAX, 5_552);
    }

    // -----------------------------------------------------------------------
    // Virtual 64-bit combine-length coverage.
    //
    // `adler32_combine` takes an `i64` length so a single entry point covers
    // both C `z_off_t` and `z_off64_t`, and the only use it makes of that length
    // is the reduction `len2 % BASE`. Every concatenation test above uses a
    // length that fits comfortably in a `u32`, so a 32-bit truncation, a
    // reduction performed at the wrong width, or a sign mix-up would pass all of
    // them. The anchors below close that gap: each was produced by
    // `adler32_combine64` in reference zlib 1.3.2.1-motley, compiled from the C
    // sources retained in this repository with `-D_LARGEFILE64_SOURCE=1`.
    // -----------------------------------------------------------------------

    /// The two checksums every 64-bit anchor combines: the Adler-32 values of
    /// the halves of "The quick brown fox jumps over the lazy dog" split at 20,
    /// themselves pinned by `combine_matches_concatenation` above.
    const CA1: u32 = 0x4c62_0734;
    /// Second half of the anchor pair; see [`CA1`].
    const CA2: u32 = 0x69d6_08a7;

    /// `(len2, reference adler32_combine64(CA1, CA2, len2))` sampled across the
    /// whole non-negative `i64` domain: zero, one, the real 22/23-byte split
    /// lengths, either side of `BASE`, the second multiple of `BASE`, every
    /// power of two from `2^28` to `2^62`, either side of `2^32`, and both ends
    /// of `i64::MAX`.
    const ADLER_COMBINE_ANCHORS: [(i64, u32); 21] = [
        (0, 0xb638_0fda),
        (1, 0xbd6b_0fda),
        (22, 0x54a9_0fda),
        (23, 0x5bdc_0fda),
        (65_520, 0xaf05_0fda),
        (65_521, 0xb638_0fda),
        (65_522, 0xbd6b_0fda),
        (131_041, 0xaf05_0fda),
        (131_042, 0xb638_0fda),
        (1_i64 << 28, 0xeb78_0fda),
        ((1_i64 << 29) - 1, 0x1994_0fda),
        (1_i64 << 29, 0x20c7_0fda),
        ((1_i64 << 29) + 1, 0x27fa_0fda),
        (1_i64 << 30, 0x8b47_0fda),
        (1_i64 << 31, 0x6056_0fda),
        ((1_i64 << 32) - 1, 0x0341_0fda),
        (1_i64 << 32, 0x0a74_0fda),
        ((1_i64 << 32) + 1, 0x11a7_0fda),
        (1_i64 << 62, 0xf62d_0fda),
        (i64::MAX - 1, 0x27cb_0fda),
        (i64::MAX, 0x2efe_0fda),
    ];

    #[test]
    fn combine_boundary_lengths_match_reference() {
        for (len2, expected) in ADLER_COMBINE_ANCHORS {
            assert_eq!(
                adler32_combine(CA1, CA2, len2),
                expected,
                "adler32_combine at len2={len2}"
            );
        }
    }

    #[test]
    fn combine_at_i64_max_matches_reference() {
        // Called out separately from the table because `i64::MAX` is where a
        // width defect is most likely to surface: its residue mod `BASE` is
        // 58_072, which no shorter length used anywhere in this file produces.
        assert_eq!(adler32_combine(CA1, CA2, i64::MAX), 0x2efe_0fda);
        assert_eq!(adler32_combine(CA1, CA2, i64::MAX - 1), 0x27cb_0fda);

        // A 32-bit truncation of `len2` is precisely the defect these anchors
        // exist to catch: `i64::MAX as u32` is `u32::MAX`, whose residue is 224
        // rather than 58_072, so the two must not agree.
        assert_ne!(
            adler32_combine(CA1, CA2, i64::MAX),
            adler32_combine(CA1, CA2, i64::from(u32::MAX)),
            "a 32-bit truncation of len2 must be observable"
        );
    }

    #[test]
    fn combine_depends_on_len2_only_modulo_base() {
        // `len2` enters the derivation solely as `len2 % BASE`, so every anchor
        // must be reproduced by its own reduced length. Checking the identity
        // against the reference values (rather than against the implementation
        // alone) means neither side can drift unnoticed.
        for (len2, expected) in ADLER_COMBINE_ANCHORS {
            let reduced = len2 % i64::from(BASE);
            assert!(reduced < i64::from(BASE) && reduced >= 0);
            assert_eq!(
                adler32_combine(CA1, CA2, reduced),
                expected,
                "reduced length {reduced} must reproduce the len2={len2} anchor"
            );
        }

        // Non-vacuity: reducing at 32 bits first would map `2^32` to zero, and
        // the two lengths genuinely differ, so the identity above has teeth.
        assert_ne!(
            adler32_combine(CA1, CA2, 1_i64 << 32),
            adler32_combine(CA1, CA2, 0)
        );
    }

    #[test]
    fn combine_real_split_around_base_matches_whole() {
        // The same modulus boundary, now crossed with *real* concatenated data
        // so the `rem` reduction and the conditional `sum1`/`sum2` corrections
        // are driven by genuine checksums. `len2` sweeps `BASE - 1`, `BASE`
        // (residue zero) and `BASE + 1` (residue one). Expected values are
        // reference zlib over the byte pattern `((i * 131 + 7) & 0xff)`.
        const L1: usize = 1_000;
        /// Adler-32 of the fixed 1_000-byte first sequence.
        const A1: u32 = 0x1347_efec;
        let cases: [(usize, u32, u32); 3] = [
            (65_520, 0x43ea_811a, 0x6737_7114),
            (65_521, 0xc593_81a9, 0xd8da_71a3),
            (65_522, 0x475d_81bb, 0x4a9e_71b5),
        ];

        for (l2, expected_a2, expected_whole) in cases {
            let data: Vec<u8> = (0..L1 + l2).map(|i| ((i * 131 + 7) & 0xff) as u8).collect();
            let (first, second) = data.split_at(L1);
            let a1 = adler32(1, first);
            let a2 = adler32(1, second);
            let whole = adler32(1, &data);
            assert_eq!(a1, A1, "first-sequence checksum at l2={l2}");
            assert_eq!(a2, expected_a2, "second-sequence checksum at l2={l2}");
            assert_eq!(whole, expected_whole, "whole-sequence checksum at l2={l2}");
            assert_eq!(
                adler32_combine(a1, a2, l2 as i64),
                whole,
                "combine must equal the whole-sequence checksum at l2={l2}"
            );
        }
    }

    #[test]
    fn combine_reduction_ladder_is_fully_exercised() {
        // The four conditional subtractions at the end of `adler32_combine` are
        // not interchangeable: `sum1` can reach just under `3 * BASE` and so
        // needs BOTH of its reductions, while `sum2` can reach just under
        // `4 * BASE` and therefore needs the `2 * BASE` reduction *before* the
        // `BASE` one. Collapsing the `2 * BASE` test into a second `BASE` test
        // leaves every `sum2` in `[3 * BASE, 4 * BASE)` unreduced.
        //
        // Reaching that band requires both operands to carry a near-maximal
        // second sum, which no short literal string produces. Each case below is
        // a *real* concatenation of `0xff` bytes located with the reference C
        // implementation, and each is annotated with the band it drives. Expected
        // values are reference zlib (`adler32` / `adler32_combine64`).
        //
        // `(n1, n2, adler1, adler2, whole, sum1 band, sum2 band)` where a band of
        // `n` means the pre-reduction value lies in `[n * BASE, (n + 1) * BASE)`.
        let cases: [(usize, usize, u32, u32, u32, u32, u32); 6] = [
            (3_083, 1, 0xffd4_ff9b, 0x0100_0100, 0x008c_00a9, 2, 3),
            (3_083, 2, 0xffd4_ff9b, 0x02ff_01ff, 0x0234_01a8, 2, 3),
            (3_083, 300, 0xffd4_ff9b, 0xb90f_2ae4, 0x52fe_2a8d, 2, 3),
            (100, 1, 0xa7c7_639d, 0x0100_0100, 0x0c72_649c, 1, 2),
            (1_000, 65_521, 0xe6e9_e446, 0x0000_0001, 0xe6e9_e446, 1, 1),
            (2, 3, 0x02ff_01ff, 0x05fd_02fe, 0x0ef6_04fc, 1, 1),
        ];

        for (n1, n2, expected_a1, expected_a2, expected_whole, band1, band2) in cases {
            let data = vec![0xffu8; n1 + n2];
            let (first, second) = data.split_at(n1);
            let a1 = adler32(1, first);
            let a2 = adler32(1, second);
            let whole = adler32(1, &data);
            assert_eq!(a1, expected_a1, "adler1 for n1={n1}");
            assert_eq!(a2, expected_a2, "adler2 for n2={n2}");
            assert_eq!(whole, expected_whole, "whole for n1={n1} n2={n2}");

            // Premise check: confirm this case really drives the band it claims,
            // so the ladder coverage cannot silently degrade if a value is edited.
            let rem = u64::try_from(n2).expect("length fits u64") % u64::from(BASE);
            let base = u64::from(BASE);
            let sum1_pre = u64::from(a1 & 0xffff) + u64::from(a2 & 0xffff) + base - 1;
            let sum2_pre = (rem * u64::from(a1 & 0xffff)) % base
                + u64::from((a1 >> 16) & 0xffff)
                + u64::from((a2 >> 16) & 0xffff)
                + base
                - rem;
            assert_eq!(
                sum1_pre / base,
                u64::from(band1),
                "n1={n1} n2={n2}: sum1 band changed (pre-reduction {sum1_pre})"
            );
            assert_eq!(
                sum2_pre / base,
                u64::from(band2),
                "n1={n1} n2={n2}: sum2 band changed (pre-reduction {sum2_pre})"
            );

            assert_eq!(
                adler32_combine(a1, a2, n2 as i64),
                whole,
                "combine must equal the whole checksum for n1={n1} n2={n2}"
            );
        }
    }
}
