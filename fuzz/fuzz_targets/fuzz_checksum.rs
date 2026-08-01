#![no_main]
//! Checksum invariant harness (Adler-32 and CRC-32).
//!
//! For any input split into two chunks, three computations must agree:
//!
//!   1. one-shot over the whole buffer,
//!   2. incremental (chunk `a`, then chunk `b` seeded by `a`'s result), and
//!   3. `*_combine` folding the two independently-computed running checksums.
//!
//! Divergence is a correctness bug; a panic is a robustness bug. These are the
//! exact algebraic properties zlib consumers rely on for streaming checksums.
//!
//! Beyond that core contract, this harness pins down the parts of the ported
//! checksum layer (the safe-Rust translations of the C `adler32.c` and
//! `crc32.c`) where a plausible-looking port silently diverges from reference
//! zlib. Every dimension below is driven from the fuzzer's own bytes:
//!
//! * **Fuzzer-varied seeds.** The three-way agreement is re-proved for an
//!   arbitrary *starting* checksum, not only for the RFC identity seeds, which
//!   is a strictly stronger property. Note the asymmetry: `*_combine`'s
//!   *second* operand must always be computed from the canonical identity seed,
//!   because the C derivation folds that seed back out (`+ BASE - 1` for
//!   Adler-32). Feeding a varied seed there would be wrong, not stronger.
//! * **Known-answer anchors.** A handful of canonical vectors is re-asserted on
//!   every iteration. They are the cheapest possible tripwire for a regression
//!   in the build-time CRC table generation or in feature selection between the
//!   SIMD and scalar bulk paths.
//! * **Family-specific `*_combine` sentinels.** A negative `len2` is invalid in
//!   both families, but they report it *differently*: Adler-32 yields the
//!   invalid checksum `0xFFFF_FFFF`, whereas CRC-32 yields a zero operator and
//!   therefore `0`. Assuming one shared sentinel is a real porting hazard.
//! * **Empty-slice seed normalisation.** An empty Adler-32 update reduces each
//!   16-bit half modulo `BASE` (so `0xFFFF_FFFF` becomes `0x000E_000E`), while
//!   an empty CRC-32 update returns the seed untouched.
//! * **`NMAX`-crossing lengths.** `NMAX` is where Adler-32's `DO16`
//!   block-reduction loop engages, making it the highest-value length boundary
//!   in the layer. A payload past it is constructed deliberately rather than
//!   hoping the fuzzer offers one from a cold corpus.
//! * **The `size_t` (`*_z`) and split-operator (`crc32_combine_gen` plus
//!   `crc32_combine_op`) spellings**, which the FFI layer exports as distinct C
//!   symbols and which must agree with their primary counterparts.
//!
//! Every checksum entry point is infallible, so unlike the round-trip harnesses
//! there is no legitimate error outcome to tolerate here: every check below is a
//! hard assertion, and a failure is a genuine finding.
//!
//! Scope of this harness: it drives the safe idiomatic checksum API only. It
//! performs no raw-pointer work, never crosses the C ABI boundary, and adds no
//! dependency — the fuzzer's `data` slice is its sole entropy source, so every
//! seed, split index and constructed length below is derived from it.

use libfuzzer_sys::fuzz_target;
use zlib_rs::checksum::{
    adler32, adler32_combine, adler32_z, crc32, crc32_combine, crc32_combine_gen, crc32_combine_op,
    crc32_z,
};

// ===========================================================================
// Constants mirrored from the library and the wire formats.
//
// Every value here is part of the observable behaviour of the C API this crate
// replaces: it is asserted against, never adjusted. `BASE` and `NMAX` are
// private to `zlib_rs::checksum::adler32`, so they are mirrored locally rather
// than imported.
// ===========================================================================

/// Adler-32 identity seed (RFC 1950). The C idiom `adler32(0L, Z_NULL, 0)`
/// returns `1`, so `1` is the value threaded into the first real update.
const ADLER_SEED: u32 = 1;

/// CRC-32 identity seed (RFC 1952). The C idiom `crc32(0L, Z_NULL, 0)` returns
/// `0`.
const CRC_SEED: u32 = 0;

/// `BASE` from `adler32.c`: the largest prime below `65536`. Both Adler-32
/// component sums are kept modulo this value, which is precisely what makes the
/// empty-update normalisation asserted below observable.
const ADLER_BASE: u32 = 65_521;

/// `NMAX` from `adler32.c`: the largest run length Adler-32 accumulates before
/// it must reduce modulo [`ADLER_BASE`]. Buffers longer than this engage the
/// block-boundary loop in `adler32` and the `len2 % BASE` reduction inside
/// `adler32_combine`.
const ADLER_NMAX: usize = 5_552;

/// The invalid Adler-32 value `adler32_combine` returns for a negative `len2`,
/// as a debugging clue — matching reference zlib.
const ADLER_INVALID: u32 = 0xFFFF_FFFF;

/// The operator `crc32_combine_gen` returns for a **zero** `len2`. A zero
/// length is the *identity* operator, emphatically not the zero operator.
const CRC_IDENTITY_OP: u32 = 0x8000_0000;

/// The operator `crc32_combine_gen` returns for a negative `len2`, and the one
/// operator value `crc32_combine_op` rejects. Note that this differs from
/// [`ADLER_INVALID`]: the two checksum families do not share a sentinel.
const CRC_INVALID_OP: u32 = 0;

/// Bytes of `data` consumed as the control header before the payload begins.
///
/// Kept deliberately short so that a cold, unseeded libFuzzer corpus reaches
/// the varied-seed paths within its first few mutations.
const HEADER_LEN: usize = 4;

/// Control word used when `data` is shorter than [`HEADER_LEN`], so that very
/// short inputs still exercise the full payload path instead of being skipped.
const FALLBACK_CONTROL: u32 = 0x9E37_79B9;

/// Bytes appended past [`ADLER_NMAX`] in the constructed long payload, on top of
/// the two whole blocks, so the block loop is followed by a genuine tail.
const LONG_TAIL: usize = 16;

// ===========================================================================
// Helpers
// ===========================================================================

/// Reduces `seed` exactly the way an empty Adler-32 update does.
///
/// Reference zlib routes a non-null zero-length update through its `len < 16`
/// path, which reduces the low half with a single conditional subtraction and
/// the high half modulo `BASE`, rather than echoing the packed seed straight
/// back. A valid running value (both halves already below `BASE`) therefore
/// survives untouched, while an arbitrary seed is normalised — `0xFFFF_FFFF`
/// becomes `0x000E_000E`, and `0xFFF1_FFF1` collapses all the way to `0`.
fn adler_normalised(seed: u32) -> u32 {
    let low = seed & 0xFFFF;
    let high = (seed >> 16) & 0xFFFF;
    // One conditional subtraction suffices: `low <= 0xFFFF < 2 * ADLER_BASE`.
    let low = if low >= ADLER_BASE {
        low - ADLER_BASE
    } else {
        low
    };
    ((high % ADLER_BASE) << 16) | low
}

/// Expands `state` into `len` deterministic bytes with a self-contained
/// `xorshift64*` generator.
///
/// This is how the harness reaches lengths a cold corpus rarely offers without
/// taking on an external RNG dependency. `state` is forced odd so the generator
/// can never be seeded with zero (which would emit an all-zero sequence).
fn expand(state: u64, len: usize) -> Vec<u8> {
    let mut state = state | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        // `wrapping_mul` is the generator's defined final scramble, spelled
        // explicitly because this crate builds with `overflow-checks = true`.
        out.push((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 24) as u8);
    }
    out
}

/// Re-proves the canonical known-answer vectors.
///
/// These are fixed truths of the two wire formats and are independent of
/// `data`, so they are re-asserted on every iteration: they cost almost nothing
/// and they fail immediately if the CRC lookup table, the Adler-32 unrolling, or
/// the active feature row has regressed.
fn assert_known_answers() {
    // The CRC-32/IEEE "check" value, and the canonical Adler-32 of the same
    // string. Both are verified against reference zlib.
    assert_eq!(
        crc32(CRC_SEED, b"123456789"),
        0xCBF4_3926,
        "crc32 check value"
    );
    assert_eq!(
        adler32(ADLER_SEED, b"123456789"),
        0x091E_01DE,
        "adler32 check value"
    );

    // Empty-input identities.
    assert_eq!(adler32(ADLER_SEED, b""), 1, "adler32 of empty input");
    assert_eq!(crc32(CRC_SEED, b""), 0, "crc32 of empty input");

    // The classic Adler-32 worked example, and the 13-byte literal the wider
    // test suite reuses (no trailing NUL).
    assert_eq!(
        adler32(ADLER_SEED, b"Wikipedia"),
        0x11E6_0398,
        "adler32 of \"Wikipedia\""
    );
    assert_eq!(
        adler32(ADLER_SEED, b"hello, hello!"),
        0x2170_0496,
        "adler32 of the 13-byte hello literal"
    );

    // A second widely published CRC-32/IEEE vector.
    assert_eq!(
        crc32(CRC_SEED, b"The quick brown fox jumps over the lazy dog"),
        0x414F_A339,
        "crc32 of the pangram"
    );
}

/// Asserts the one-shot / incremental / `*_combine` three-way agreement for both
/// families at `split`, under arbitrary starting seeds.
///
/// `split` is clamped into `0..=whole.len()`, so a fuzzer-chosen index is always
/// usable — degenerate ends included. Each `*_combine` call computes its
/// *second* operand from the canonical identity seed, because the C derivation
/// folds that seed out of the result; the *first* operand carries the varied
/// seed. The CRC-32 case additionally checks that the precomputed-operator form
/// agrees with the length form.
fn assert_three_way(adler_seed: u32, crc_seed: u32, whole: &[u8], split: usize) {
    let split = split.min(whole.len());
    let (a, b) = whole.split_at(split);
    let len_b = b.len();
    let len2 = len_b as i64;

    // --- Adler-32 ---
    let adler_whole = adler32(adler_seed, whole);
    let adler_a = adler32(adler_seed, a);
    assert_eq!(
        adler32(adler_a, b),
        adler_whole,
        "adler32 incremental mismatch (seed={adler_seed:#010x}, |a|={split}, |b|={len_b})"
    );
    assert_eq!(
        adler32_combine(adler_a, adler32(ADLER_SEED, b), len2),
        adler_whole,
        "adler32_combine mismatch (seed={adler_seed:#010x}, |a|={split}, |b|={len_b})"
    );

    // --- CRC-32 ---
    let crc_whole = crc32(crc_seed, whole);
    let crc_a = crc32(crc_seed, a);
    let crc_b = crc32(CRC_SEED, b);
    assert_eq!(
        crc32(crc_a, b),
        crc_whole,
        "crc32 incremental mismatch (seed={crc_seed:#010x}, |a|={split}, |b|={len_b})"
    );
    let crc_combined = crc32_combine(crc_a, crc_b, len2);
    assert_eq!(
        crc_combined, crc_whole,
        "crc32_combine mismatch (seed={crc_seed:#010x}, |a|={split}, |b|={len_b})"
    );

    // The operator form must reproduce the length form exactly.
    assert_eq!(
        crc32_combine_op(crc_a, crc_b, crc32_combine_gen(len2)),
        crc_combined,
        "crc32_combine_op vs crc32_combine mismatch (|b|={len_b})"
    );
}

/// Asserts that a three-way split folded with two chained `*_combine` calls
/// reproduces the one-shot checksum of the whole buffer.
///
/// Chaining is where a `*_combine` implementation that is merely
/// *self-consistent* rather than correct shows up: the intermediate result is
/// fed straight back in as a first operand, so any residual seed handling in the
/// first fold corrupts the second.
fn assert_chained_combine(adler_seed: u32, crc_seed: u32, whole: &[u8]) {
    let (first, rest) = whole.split_at(whole.len() / 3);
    let (second, third) = rest.split_at(rest.len() / 2);
    let len_second = second.len() as i64;
    let len_third = third.len() as i64;

    let adler_pair = adler32_combine(
        adler32(adler_seed, first),
        adler32(ADLER_SEED, second),
        len_second,
    );
    assert_eq!(
        adler32_combine(adler_pair, adler32(ADLER_SEED, third), len_third),
        adler32(adler_seed, whole),
        "chained adler32_combine mismatch (seed={adler_seed:#010x}, |whole|={})",
        whole.len()
    );

    let crc_pair = crc32_combine(crc32(crc_seed, first), crc32(CRC_SEED, second), len_second);
    assert_eq!(
        crc32_combine(crc_pair, crc32(CRC_SEED, third), len_third),
        crc32(crc_seed, whole),
        "chained crc32_combine mismatch (seed={crc_seed:#010x}, |whole|={})",
        whole.len()
    );
}

/// Asserts that the `size_t`-length spellings agree with their primary
/// counterparts.
///
/// `adler32_z` and `crc32_z` exist because the C API exports them as separate
/// symbols; in this slice-based API both collapse onto the same input type, so
/// any divergence would mean the delegation was broken.
fn assert_size_variants(adler_seed: u32, crc_seed: u32, buf: &[u8]) {
    assert_eq!(
        adler32_z(adler_seed, buf),
        adler32(adler_seed, buf),
        "adler32_z vs adler32 mismatch (seed={adler_seed:#010x}, |buf|={})",
        buf.len()
    );
    assert_eq!(
        crc32_z(crc_seed, buf),
        crc32(crc_seed, buf),
        "crc32_z vs crc32 mismatch (seed={crc_seed:#010x}, |buf|={})",
        buf.len()
    );
}

/// Asserts empty-update seed handling, which differs between the two families.
///
/// Adler-32 normalises its packed halves modulo `BASE`; CRC-32 has no packed
/// halves and returns the seed unchanged. Both behaviours are observable through
/// the C API, so both are pinned.
fn assert_empty_update(adler_seed: u32, crc_seed: u32) {
    let normalised = adler_normalised(adler_seed);
    assert_eq!(
        adler32(adler_seed, b""),
        normalised,
        "adler32 empty-update normalisation (seed={adler_seed:#010x})"
    );
    assert_eq!(
        adler32_z(adler_seed, b""),
        normalised,
        "adler32_z empty-update normalisation (seed={adler_seed:#010x})"
    );
    // Normalising is idempotent: a valid running value survives untouched.
    assert_eq!(
        adler32(normalised, b""),
        normalised,
        "adler32 empty update is not idempotent (seed={adler_seed:#010x})"
    );

    assert_eq!(
        crc32(crc_seed, b""),
        crc_seed,
        "crc32 empty update altered the seed (seed={crc_seed:#010x})"
    );
    assert_eq!(
        crc32_z(crc_seed, b""),
        crc_seed,
        "crc32_z empty update altered the seed (seed={crc_seed:#010x})"
    );
}

/// Asserts the family-specific sentinels for an invalid (negative) `len2`.
///
/// The two families deliberately disagree here, so a single shared expectation
/// would let a porting mistake through: Adler-32 returns the invalid checksum
/// [`ADLER_INVALID`], while CRC-32 produces the rejected operator
/// [`CRC_INVALID_OP`] and therefore a zero result.
fn assert_negative_len2(adler1: u32, crc1: u32, len2: i64) {
    assert!(len2 < 0, "harness bug: {len2} is not a negative length");

    assert_eq!(
        adler32_combine(adler1, ADLER_SEED, len2),
        ADLER_INVALID,
        "adler32_combine negative-len2 sentinel (len2={len2})"
    );
    assert_eq!(
        crc32_combine_gen(len2),
        CRC_INVALID_OP,
        "crc32_combine_gen negative-len2 sentinel (len2={len2})"
    );
    assert_eq!(
        crc32_combine(crc1, CRC_SEED, len2),
        0,
        "crc32_combine negative-len2 sentinel (len2={len2})"
    );
    assert_eq!(
        crc32_combine_op(crc1, CRC_SEED, CRC_INVALID_OP),
        0,
        "crc32_combine_op must reject the zero operator"
    );
}

/// Asserts the zero-`len2` identity and the operator that corresponds to it.
///
/// The second operand is the checksum of an **empty** buffer at the identity
/// seed (Adler-32 to `1`, CRC-32 to `0`), not a bare zero — passing a raw `0` as
/// the second Adler-32 checksum would be incorrect.
fn assert_zero_len2_identity(adler_seed: u32, crc_seed: u32, buf: &[u8]) {
    assert_eq!(
        crc32_combine_gen(0),
        CRC_IDENTITY_OP,
        "crc32_combine_gen(0) must be the identity operator"
    );

    let adler_buf = adler32(adler_seed, buf);
    assert_eq!(
        adler32_combine(adler_buf, adler32(ADLER_SEED, b""), 0),
        adler_buf,
        "adler32_combine with len2=0 is not the identity (seed={adler_seed:#010x})"
    );

    let crc_buf = crc32(crc_seed, buf);
    let crc_empty = crc32(CRC_SEED, b"");
    assert_eq!(
        crc32_combine(crc_buf, crc_empty, 0),
        crc_buf,
        "crc32_combine with len2=0 is not the identity (seed={crc_seed:#010x})"
    );
    assert_eq!(
        crc32_combine_op(crc_buf, crc_empty, CRC_IDENTITY_OP),
        crc_buf,
        "crc32_combine_op with the identity operator is not the identity"
    );
}

fuzz_target!(|data: &[u8]| {
    // (1) Fixed wire-format truths, re-proved on every iteration.
    assert_known_answers();

    // (2) The baseline contract: identity seeds, the whole input, mid-point
    //     split. Preserved exactly as originally written.
    let split = data.len() / 2;
    let (a, b) = data.split_at(split);

    // Adler-32 (seed 1, per RFC 1950).
    let adler_one = adler32(ADLER_SEED, data);
    let adler_inc = adler32(adler32(ADLER_SEED, a), b);
    assert_eq!(adler_one, adler_inc, "adler32 incremental mismatch");
    let adler_comb = adler32_combine(
        adler32(ADLER_SEED, a),
        adler32(ADLER_SEED, b),
        b.len() as i64,
    );
    assert_eq!(adler_one, adler_comb, "adler32_combine mismatch");

    // CRC-32 (seed 0, per RFC 1952).
    let crc_one = crc32(CRC_SEED, data);
    let crc_inc = crc32(crc32(CRC_SEED, a), b);
    assert_eq!(crc_one, crc_inc, "crc32 incremental mismatch");
    let crc_comb = crc32_combine(crc32(CRC_SEED, a), crc32(CRC_SEED, b), b.len() as i64);
    assert_eq!(crc_one, crc_comb, "crc32_combine mismatch");

    // (3) Control header: a short prefix selects the seeds, the split index and
    //     the constructed-payload length; everything after it is payload. When
    //     the input is too short to carry a header, a defined control word is
    //     substituted and the whole input is still used as payload, so the
    //     smallest inputs a cold corpus produces are never wasted.
    let (control, payload) = if data.len() >= HEADER_LEN {
        let (header, payload) = data.split_at(HEADER_LEN);
        let control = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        (control, payload)
    } else {
        (FALLBACK_CONTROL, data)
    };
    // Two independent seeds from one control word; byte-swapping keeps the two
    // families from being exercised with the same bit pattern every iteration.
    let adler_seed = control;
    let crc_seed = control.swap_bytes();

    // (4) The same three-way contract under arbitrary starting seeds, at the
    //     fuzzer's chosen split and at both degenerate ends.
    let picked = (control as usize) % (payload.len() + 1);
    for split in [0, picked, payload.len()] {
        assert_three_way(adler_seed, crc_seed, payload, split);
    }
    assert_size_variants(adler_seed, crc_seed, payload);
    assert_chained_combine(adler_seed, crc_seed, payload);

    // (5) Degenerate inputs and the invalid-length sentinels, for the identity
    //     seeds and for the fuzzer-varied ones.
    assert_empty_update(ADLER_SEED, CRC_SEED);
    assert_empty_update(adler_seed, crc_seed);
    assert_zero_len2_identity(adler_seed, crc_seed, payload);
    // `i64::from(control) + 1` tops out at 2^32, so negating it cannot overflow.
    let derived_negative = -(i64::from(control) + 1);
    for len2 in [-1, i64::MIN, derived_negative] {
        assert_negative_len2(adler_seed, crc_seed, len2);
    }

    // (6) A payload deliberately constructed to cross `NMAX`, which the fuzzer
    //     rarely reaches early in a cold-corpus run. Two whole blocks plus a
    //     tail exercise the block loop repeatedly and then the short-tail path;
    //     the splits straddle the boundary so `a` lands just short of, exactly
    //     on, and just past a whole block. The length stays a few times `NMAX`
    //     to respect the per-target time budget and the RSS cap.
    let long_len = 2 * ADLER_NMAX + LONG_TAIL + (control >> 26) as usize;
    let long = expand(
        u64::from(control).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        long_len,
    );
    assert!(
        long.len() > ADLER_NMAX,
        "constructed payload must cross NMAX"
    );
    for split in [ADLER_NMAX - 1, ADLER_NMAX, ADLER_NMAX + 1] {
        assert_three_way(adler_seed, crc_seed, &long, split);
    }
    assert_size_variants(adler_seed, crc_seed, &long);
    assert_chained_combine(adler_seed, crc_seed, &long);
});
