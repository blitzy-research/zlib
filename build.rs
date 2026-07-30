//! Cargo build script for the `zlib-rs` crate: regenerates the CRC-32 lookup
//! tables at build time. That is its only job — it emits no link arguments and
//! influences nothing outside `${OUT_DIR}`.
//!
//! # Why this exists
//!
//! In the C zlib baseline the header `crc32.h` (~9,400 lines) is a checked-in,
//! machine-generated set of CRC-32 lookup tables produced by the
//! `make_crc_table()` routine in `crc32.c`. Rather than ship a giant literal
//! table file, this build script reimplements `make_crc_table()` in safe,
//! host-independent Rust and emits the equivalent Rust source into Cargo's
//! `OUT_DIR`. The checksum module then pulls the tables in with:
//!
//! ```ignore
//! include!(concat!(env!("OUT_DIR"), "/crc32_tables.rs"));
//! ```
//!
//! The regenerated tables are **bit-identical** to the values in `crc32.h`, so
//! the pure-Rust scalar CRC-32 path and the `crc32_combine` math produce output
//! that matches reference zlib exactly. (The SIMD hot path is delegated to the
//! `crc32fast` crate behind the `simd` feature; these tables back the scalar
//! fallback and the combine operations.)
//!
//! # Emitted contract
//!
//! The generated `${OUT_DIR}/crc32_tables.rs` defines exactly the seven items
//! below. The names and shapes are a stable contract; do not rename one without
//! updating `src/checksum/crc32.rs`.
//!
//! `src/checksum/crc32.rs` is the sole consumer; the table below records each
//! item's shape alongside the C artifact it reproduces.
//!
//! | Symbol                 | Type                 | Consumed by / C counterpart                                          |
//! |------------------------|----------------------|----------------------------------------------------------------------|
//! | `CRC_TABLE`            | `[u32; 256]`         | The scalar reflected byte-wise loop and `get_crc_table` (`crc_table`). |
//! | `X2N_TABLE`            | `[u32; 32]`          | `crc32_combine`'s GF(2) arithmetic (`x^(2^n) mod p`).                  |
//! | `CRC_BRAID_N`          | `usize` (= 5)        | `N`, the braid count of the braided word loop.                        |
//! | `CRC_BRAID_W`          | `usize` (= 8)        | `W`, bytes per CRC word in that loop.                                 |
//! | `CRC_BIG_TABLE`        | `[u64; 256]`         | `crc_big_table`, the big-endian word companion.                       |
//! | `CRC_BRAID_TABLE`      | `[[u32; 256]; 8]`    | `crc_braid_table` (W = 8), little-endian braid.                       |
//! | `CRC_BRAID_BIG_TABLE`  | `[[u64; 256]; 8]`    | `crc_braid_big_table` (W = 8), big-endian braid.                      |
//!
//! All seven are consumed: `CRC_TABLE` and `X2N_TABLE` by the byte-wise loop,
//! `get_crc_table`, and the combine routines; the remaining five by the braided
//! word-at-a-time path (`crc32.rs`'s `braid` module, the port of the `#ifdef W`
//! fast path in `crc32.c`). No item is emitted with an `#[allow(dead_code)]`
//! attribute — suppressing that warning is the consuming module's decision, and
//! it grants the allowance only in the `simd` configuration, where the braided
//! path is not compiled because the hot loop is `crc32fast`'s. Consequently a
//! `--no-default-features` build **proves** every generated artifact is
//! consumed: dropping one produces a warning, which CI's `-D warnings` promotes
//! to an error.
//!
//! Emitting all seven keeps this script a complete, auditable port of C
//! `make_crc_table()` — every table the C header contains is reproduced and can
//! be diffed against it.
//!
//! # Determinism
//!
//! The tables are pure mathematical constants derived from the reflected
//! CRC-32/IEEE polynomial `0xEDB88320`. This script performs **no** host or
//! target detection: it always emits the fixed `N = 5`, `W = 8` configuration
//! (the zlib default for 64-bit targets) and generates both braid tables
//! unconditionally, so its output is byte-for-byte identical on every build and
//! every platform. Endianness is therefore not a *generation-time* concern: both
//! braid variants are always emitted, and the choice between them is made at
//! *consumption* time by `crc32.rs` with `cfg!(target_endian)`. The byte-wise
//! `CRC_TABLE` is endianness- and word-size-independent, so it needs no variant.
//!
//! # No cdylib symbol versioning (AAP §0.8.2 Divergence 4 / gap D8)
//!
//! The C build links `libz.so` through `zlib.map`, a GNU-ld *version script*
//! that distributes the exported symbols across sixteen ELF version nodes
//! (`ZLIB_1.2.0` through `ZLIB_1.3.2`). **This script deliberately does not
//! reproduce those nodes, by any mechanism, under any configuration.** It emits
//! no `cargo:rustc-link-arg` and no `cargo:rustc-cdylib-link-arg` directive at
//! all, and it reads no environment variable other than the `OUT_DIR` Cargo
//! sets for it. Consequently the emitted `cdylib` carries an unversioned symbol
//! table, which is precisely the divergence AAP §0.8.2 records as Divergence 4
//! and instructs be *kept* rather than "fixed":
//!
//! > cdylib symbol versioning is not applied. […] The symbol *set* is exactly
//! > right (96 declared, 1 platform-gated, 95 emitted, 54/54 global coverage,
//! > 0/10 local leakage); only the version tags are absent. […] it is correctly
//! > ranked Low because a drop-in replacement links successfully without it, and
//! > the change carries linker-portability risk that must be gated behind the
//! > expanded CI matrix (D3).
//!
//! Nothing is lost by the omission: all 54 `global:` names in `zlib.map` are
//! already exported and all 10 `local:` names are already hidden, and an
//! unversioned symbol table satisfies ordinary linking, `pkg-config`
//! consumption and `LD_PRELOAD` injection alike — see the "`zlib.map`
//! symbol-versioning contract" section of `src/ffi/mod.rs`.
//!
//! ## Why not even an opt-in
//!
//! An environment-gated opt-in was implemented, measured, and removed. It is
//! recorded here so the measurements are not lost and the experiment is not
//! repeated by accident. Passing `-Wl,--undefined-version` plus
//! `-Wl,--version-script=zlib.map` as `cdylib` link arguments was observed to
//! be *either* fatal *or* ineffective, depending only on which linker the
//! active toolchain happens to drive:
//!
//! * On the pinned MSRV, `rustc 1.85.0`, which links through GNU `ld` 2.45, the
//!   build **fails outright**: `rustc` already passes a version script of its
//!   own to export the `#[unsafe(no_mangle)]` shims and that script uses an
//!   *anonymous* version node, so GNU `ld` reports "anonymous version tag
//!   cannot be combined with other version tags" together with "unable to find
//!   version dependency `ZLIB_1.2.3.5`" for each inherited node, and
//!   `collect2` exits non-zero. A build script that can break the MSRV gate on
//!   a stray environment variable is not a safe thing to ship.
//! * On `rustc 1.97.1`, whose default linker for `x86_64-unknown-linux-gnu` is
//!   `rust-lld`, the link succeeds but achieves nothing measurable: because
//!   `rustc`'s anonymous node is consulted first and the first match wins, every
//!   listed symbol keeps the base version. `rust-lld` reports each refused
//!   reassignment — "attempt to reassign symbol 'compressBound' of
//!   VER_NDX_GLOBAL to version 'ZLIB_1.2.0'" — `rustc` surfaces those through
//!   its `linker_messages` lint (so a `-D warnings` gate fails), and the
//!   resulting `libzlib_rs.so` carries **zero** per-symbol version tags:
//!   `nm -D --defined-only libzlib_rs.so | grep -c @` returns `0`, and
//!   `readelf --dyn-syms` shows `deflate` and `compressBound` as plain
//!   `GLOBAL DEFAULT` entries. Real per-symbol tags would require `.symver`
//!   directives in the source, which no build script can supply.
//!
//! So the option could not be turned on where it would have linked usefully,
//! and turning it on where it linked at all produced warnings instead of
//! version tags. Removing it also removes three defects the branch carried by
//! construction: an unrecognized value of its opt-in variable was silently
//! treated as "off" rather than rejected; linker capability was *inferred* from
//! `CARGO_CFG_TARGET_OS`/`CARGO_CFG_TARGET_ENV` rather than probed, which is
//! exactly the inference the two measurements above falsify (same `target_os`,
//! opposite outcomes); and the manifest path was interpolated verbatim into a
//! line-oriented Cargo directive and a comma-delimited `-Wl` argument, where a
//! newline, comma, or `=` in the path would have altered the link line.
//!
//! ## What a working implementation looked like, and why it is still not here
//!
//! One route was found that does apply `zlib.map` correctly, and it is recorded
//! because it is the starting point for whoever re-opens D8 — not because it is
//! wanted today. The insight is that no linker flag can reset or override a
//! version script that is already in force, so the linker has to be made to see
//! exactly *one* script, and that script has to be ours. Under an opt-in, three
//! things were produced inside `${OUT_DIR}`:
//!
//! 1. A derived script, `zlib.map` byte-for-byte plus exactly one extra pattern,
//!    `rust_*;`, added to the `local:` list of the base `ZLIB_1.2.0` node.
//!    `zlib.map` itself is a read-only reference artifact of the retained C
//!    baseline (AAP §0.4.1.12) and is never rewritten.
//! 2. A small POSIX `sh` wrapper linker with the absolute path of the real linker
//!    baked in. It rewrites the first `--version-script=…` argument to point at
//!    the derived script, drops any further ones, turns `--no-undefined-version`
//!    into `--undefined-version`, and `exec`s the real linker. If the derived
//!    script is unreadable it forwards the arguments untouched, so a problem
//!    inside the wrapper degrades to an ordinary unversioned link.
//! 3. The link arguments `-B<OUT_DIR>/…` and, when a real `ld.bfd` could be
//!    resolved, `-fuse-ld=bfd`. `-B` is how a `cc`/`clang` driver is told where
//!    to find its subprograms; the flavour flag is what stops a toolchain whose
//!    default is `rust-lld` from never consulting the wrapper at all.
//!
//! Measured on `x86_64-unknown-linux-gnu` for both the pinned MSRV 1.85.0
//! (`/usr/bin/ld` 2.45) and stable 1.97.1 (flavour switched to `bfd`): the build
//! exits 0 with no warnings; `nm -D --defined-only` reports the same **95**
//! exported `T` symbols as a default build, name for name;
//! `readelf --version-info` shows all **16** `ZLIB_*` definitions with the
//! inheritance chain from `ZLIB_1.2.0` intact; and exactly **54** symbols carry
//! an `@@ZLIB_x.y.z` tag — the 54 `global:` names — while the remaining 41 stay
//! unversioned-global, which is how a distribution `libz.so.1` built from the
//! same script behaves. So the claim above that per-symbol tags need `.symver`
//! directives holds only while `rustc`'s own script remains in force; substitute
//! the script and GNU `ld` does produce them.
//!
//! Three details cost real time to establish and are easy to get wrong:
//!
//! * `rustc`'s script ends in `local: *;`, a catch-all. `zlib.map`'s only
//!   wildcard is `local: _*;`, which hides Rust's mangled names (`_ZN…`, `_R…`)
//!   and the `__rust_*` hooks but **not** `rust_begin_unwind`,
//!   `rust_eh_personality` or `rust_panic`, so substituting `zlib.map` wholesale
//!   exports 98 symbols instead of 95. The one added `rust_*;` pattern restores
//!   the exact baseline set. A `local: *;` catch-all must **not** be used
//!   instead: `zlib.map` does not name `deflate`, `inflate`, `compress`,
//!   `gzopen`, `adler32`, `crc32` or 35 other entry points at all, and a
//!   catch-all would hide every one of them.
//! * A `local:` section may not precede `global:` inside a version node — GNU
//!   `ld` 2.45 reports `syntax error in VERSION script` — so the pattern has to
//!   be inserted into the *existing* `local:` list rather than a fresh section.
//! * `--undefined-version` is required rather than decorative: the ten `local:`
//!   entries in `zlib.map` (`zcalloc`, `z_errmsg`, `inflate_table`, …) are
//!   C-internal names with no Rust counterpart, and `--no-undefined-version`
//!   turns each of them into a hard error.
//!
//! What that route costs is the reason it is not here. It needs a Unix *host*, an
//! executable script written into `${OUT_DIR}`, a shell-out to
//! `cc -print-prog-name=…` to locate the real linker, a trial link performed at
//! build time to prove the host honours `-B`, and the standing assumption that
//! `rustc` links through a `cc`/`clang` driver. That is a large, deeply
//! host-dependent apparatus in a build script, in service of a gap AAP §0.10.1
//! ranks **Low**, and not one line of it is exercised by any CI row — which is
//! precisely the D3 precondition. A build script that reaches for the shell and
//! the linker on a stray environment variable is not a safe thing to ship, so
//! the apparatus stays out and the divergence stays recorded.
//!
//! ## If it is ever wanted
//!
//! Re-opening D8 is a deliberate, reviewed change and it belongs behind gap D3,
//! the cross-platform CI matrix, exactly as AAP §0.8.2 requires — never behind
//! an environment variable that no CI job exercises. Whoever lands it owns
//! four things: selecting an LLD-flavoured linker explicitly on toolchains
//! whose default `ld` refuses the anonymous-node combination; emitting
//! `.symver` directives, because link arguments alone provably do not produce
//! per-symbol tags; keeping the exported symbol *set* unchanged (95 symbols on
//! Linux, all 54 `zlib.map` globals present, none of the 10 locals leaked); and
//! a CI row that actually links and inspects the result on every platform the
//! change claims to support. Note also that `Cargo.toml`'s `exclude` list
//! contains `*.map`, so `zlib.map` is absent from the published `.crate`
//! altogether — any such work must tolerate its absence rather than assume it.
//! `.cargo/config.toml` states the companion half of this boundary: link
//! arguments do not belong there either.
//!
//! # Constraints
//!
//! Pure `std` only — no external crates, no build-dependencies, and zero
//! `unsafe`. All generation runs on the host at build time using plain integer
//! arithmetic, mirroring `crc32.c`'s `make_crc_table()`, `multmodp()`,
//! `x2nmodp()`, `byte_swap()`, and `braid()`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// The CRC-32 polynomial, reflected, with the `x^32` term implied.
///
/// This is `0xEDB88320`, the reflected form of the IEEE 802.3 CRC-32 polynomial
/// (`POLY` in `crc32.c`).
const POLY: u32 = 0xedb8_8320;

/// Number of interleaved braids (`N` in `crc32.c`). The zlib default is 5.
const BRAID_N: usize = 5;

/// Number of bytes in a CRC "word" (`W` in `crc32.c`). We emit the 64-bit
/// configuration (`W = 8`), matching the `#if W == 8` branch of `crc32.h`.
const BRAID_W: usize = 8;

// ---------------------------------------------------------------------------
// Core polynomial arithmetic (faithful port of crc32.c helpers)
// ---------------------------------------------------------------------------

/// Return `a(x)` multiplied by `b(x)` modulo `p(x)`, where `p(x)` is the
/// reflected CRC polynomial.
///
/// This is a carry-less (GF(2)) multiply-and-reduce, ported directly from
/// `multmodp()` in `crc32.c`. For speed the C routine requires that `a` is not
/// zero; every call site in this script upholds that invariant, so the loop is
/// guaranteed to terminate at the lowest set bit of `a`.
///
/// All intermediate values fit in 32 bits, so `u32` arithmetic reproduces the
/// C `uLong` computation exactly.
fn multmodp(a: u32, mut b: u32) -> u32 {
    // `m` walks a single set bit from bit 31 down to the lowest set bit of `a`.
    let mut m: u32 = 1 << 31;
    let mut p: u32 = 0;
    loop {
        if (a & m) != 0 {
            p ^= b;
            // Once we have consumed the lowest set bit of `a`, we are done.
            if (a & (m - 1)) == 0 {
                break;
            }
        }
        m >>= 1;
        // Multiply `b` by x modulo p(x): a right shift, reducing on carry-out.
        b = if (b & 1) != 0 {
            (b >> 1) ^ POLY
        } else {
            b >> 1
        };
    }
    p
}

/// Return `x^(n * 2^k) modulo p(x)`.
///
/// Port of `x2nmodp()` in `crc32.c`. Requires that `x2n_table` has been
/// initialized. `n` is non-negative in every use here.
fn x2nmodp(mut n: u64, mut k: u32, x2n_table: &[u32; 32]) -> u32 {
    let mut p: u32 = 1 << 31; // x^0 == 1
    while n != 0 {
        if (n & 1) != 0 {
            p = multmodp(x2n_table[(k & 31) as usize], p);
        }
        n >>= 1;
        k = k.wrapping_add(1);
    }
    p
}

/// Reverse the eight bytes of a 64-bit word.
///
/// Port of the `W == 8` branch of `byte_swap()` in `crc32.c`, used to convert
/// between little- and big-endian representations of a CRC word. A 32-bit CRC
/// value that is zero-extended to 64 bits and passed through this function ends
/// up byte-swapped in the **high** 32 bits (the low 32 bits become zero), which
/// is exactly how `crc32.c` builds the big-endian tables.
fn byte_swap64(word: u64) -> u64 {
    ((word & 0xff00_0000_0000_0000) >> 56)
        | ((word & 0x00ff_0000_0000_0000) >> 40)
        | ((word & 0x0000_ff00_0000_0000) >> 24)
        | ((word & 0x0000_00ff_0000_0000) >> 8)
        | ((word & 0x0000_0000_ff00_0000) << 8)
        | ((word & 0x0000_0000_00ff_0000) << 24)
        | ((word & 0x0000_0000_0000_ff00) << 40)
        | ((word & 0x0000_0000_0000_00ff) << 56)
}

// ---------------------------------------------------------------------------
// Table construction (faithful port of crc32.c make_crc_table / braid)
// ---------------------------------------------------------------------------

/// Build the byte-wise CRC-32 table and its byte-swapped (big-endian) companion.
///
/// For each byte value `i`, the CRC of that byte is computed with the standard
/// shift-register method, exactly as in `make_crc_table()`.
fn make_crc_tables() -> ([u32; 256], [u64; 256]) {
    let mut crc_table = [0u32; 256];
    let mut crc_big_table = [0u64; 256];
    for i in 0..256u32 {
        let mut p = i;
        for _ in 0..8 {
            p = if (p & 1) != 0 {
                (p >> 1) ^ POLY
            } else {
                p >> 1
            };
        }
        crc_table[i as usize] = p;
        crc_big_table[i as usize] = byte_swap64(u64::from(p));
    }
    (crc_table, crc_big_table)
}

/// Build the table of powers of x used to combine CRC-32 values.
///
/// `x2n_table[n]` holds `x^(2^n) mod p(x)`; entry 0 is `x^1` and each subsequent
/// entry is the previous one squared modulo `p(x)`. Port of the `x2n_table`
/// initialization in `make_crc_table()`.
fn make_x2n_table() -> [u32; 32] {
    let mut x2n_table = [0u32; 32];
    let mut p: u32 = 1 << 30; // x^1
    x2n_table[0] = p;
    for slot in x2n_table.iter_mut().skip(1) {
        p = multmodp(p, p);
        *slot = p;
    }
    x2n_table
}

/// Build the little- and big-endian braid tables for the given `n` and word
/// size `w`.
///
/// Port of `braid()` in `crc32.c`. Each of the `w` sub-tables holds, for every
/// possible byte value, the sparse CRC contribution of that byte at the braid's
/// position. The big-endian sub-tables are stored in reverse position order and
/// byte-swapped, reproducing the C layout so the emitted tables can be diffed
/// against `crc32.h` entry for entry.
///
/// Both results are emitted unconditionally and both are read by
/// `src/checksum/crc32.rs`'s `braid` module, which picks the little- or
/// big-endian set at consumption time with `cfg!(target_endian)` (see the
/// "Emitted contract" section above).
fn braid(n: usize, w: usize, x2n_table: &[u32; 32]) -> (Vec<[u32; 256]>, Vec<[u64; 256]>) {
    let mut ltl = vec![[0u32; 256]; w];
    let mut big = vec![[0u64; 256]; w];
    for k in 0..w {
        // Exponent for this braid position, in bits: (n*w + 3 - k) * 8.
        let exponent = ((n * w + 3 - k) << 3) as u64;
        let p = x2nmodp(exponent, 0, x2n_table);
        ltl[k][0] = 0;
        big[w - 1 - k][0] = 0;
        for i in 1..256u32 {
            let q = multmodp(i << 24, p);
            ltl[k][i as usize] = q;
            big[w - 1 - k][i as usize] = byte_swap64(u64::from(q));
        }
    }
    (ltl, big)
}

// ---------------------------------------------------------------------------
// Rust source emission
// ---------------------------------------------------------------------------

/// Header written at the top of the generated `crc32_tables.rs`.
///
/// Uses plain `//` comments (not `//!`) because the file is pulled in with
/// `include!` and inner doc comments would attach to the including module.
const GENERATED_HEADER: &str = "\
// crc32_tables.rs -- CRC-32 lookup tables for the zlib-rs crate.
//
// GENERATED FILE - DO NOT EDIT.
// Produced at build time by build.rs (a faithful Rust port of the C
// make_crc_table() routine in crc32.c). The values are bit-identical to the
// checked-in C header crc32.h and must remain so for wire-format compatibility.
//
// Included by src/checksum/crc32.rs via:
//     include!(concat!(env!(\"OUT_DIR\"), \"/crc32_tables.rs\"));
//
// Emitted symbols. All seven are imported by that module: the first two below
// are read in every configuration, and the remaining five drive the braided
// word-at-a-time path, which is compiled whenever the `simd` feature is off. No
// item carries #[allow(dead_code)] -- the consuming module grants that allowance
// itself, and only for the `simd` configuration.
//   CRC_TABLE:           [u32; 256]       byte-wise CRC-32 table       [always]
//   X2N_TABLE:           [u32; 32]        powers of x for crc32_combine[always]
//   CRC_BRAID_N:         usize            number of braids (5)         [no-simd]
//   CRC_BRAID_W:         usize            bytes per CRC word (8)       [no-simd]
//   CRC_BIG_TABLE:       [u64; 256]       byte-swapped table (BE words)[no-simd]
//   CRC_BRAID_TABLE:     [[u32; 256]; 8]  little-endian braid table    [no-simd]
//   CRC_BRAID_BIG_TABLE: [[u64; 256]; 8]  big-endian braid table       [no-simd]

";

/// Append a `[u32; N]` static array literal to `out`, eight values per line.
///
/// No `#[allow(dead_code)]` is emitted: whether an unconsumed table is a
/// warning is the consuming module's decision, and `src/checksum/crc32.rs`
/// grants that allowance only in the `simd` configuration (see the
/// "Emitted contract" note above).
fn write_u32_array(out: &mut String, name: &str, vals: &[u32]) {
    let _ = writeln!(out, "pub(crate) static {}: [u32; {}] = [", name, vals.len());
    for chunk in vals.chunks(8) {
        out.push_str("    ");
        for (i, v) in chunk.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "0x{v:08x},");
        }
        out.push('\n');
    }
    out.push_str("];\n\n");
}

/// Append a `[u64; N]` static array literal to `out`, four values per line.
fn write_u64_array(out: &mut String, name: &str, vals: &[u64]) {
    let _ = writeln!(out, "pub(crate) static {}: [u64; {}] = [", name, vals.len());
    for chunk in vals.chunks(4) {
        out.push_str("    ");
        for (i, v) in chunk.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let _ = write!(out, "0x{v:016x},");
        }
        out.push('\n');
    }
    out.push_str("];\n\n");
}

/// Append a `[[u32; 256]; W]` static braid table literal to `out`.
fn write_braid_u32(out: &mut String, name: &str, tbl: &[[u32; 256]]) {
    let _ = writeln!(
        out,
        "pub(crate) static {}: [[u32; 256]; {}] = [",
        name,
        tbl.len()
    );
    for sub in tbl {
        out.push_str("    [\n");
        for chunk in sub.chunks(8) {
            out.push_str("        ");
            for (i, v) in chunk.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                let _ = write!(out, "0x{v:08x},");
            }
            out.push('\n');
        }
        out.push_str("    ],\n");
    }
    out.push_str("];\n\n");
}

/// Append a `[[u64; 256]; W]` static braid table literal to `out`.
fn write_braid_u64(out: &mut String, name: &str, tbl: &[[u64; 256]]) {
    let _ = writeln!(
        out,
        "pub(crate) static {}: [[u64; 256]; {}] = [",
        name,
        tbl.len()
    );
    for sub in tbl {
        out.push_str("    [\n");
        for chunk in sub.chunks(4) {
            out.push_str("        ");
            for (i, v) in chunk.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                let _ = write!(out, "0x{v:016x},");
            }
            out.push('\n');
        }
        out.push_str("    ],\n");
    }
    out.push_str("];\n\n");
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Name of the generated file written into `OUT_DIR`.
const GENERATED_FILE: &str = "crc32_tables.rs";

/// The Cargo directives this build script emits, in order.
///
/// There is exactly **one**, and its narrowness is a contract rather than an
/// omission. The tables are pure constants, so they only need regenerating when
/// this script itself changes; nothing else about the build depends on anything
/// this script can observe. In particular there is no `rustc-link-arg`, no
/// `rustc-cdylib-link-arg`, no `rerun-if-env-changed`, and no `cargo:warning` —
/// see the "No cdylib symbol versioning" section of this file's documentation
/// for why, and `.cargo/config.toml` for the companion half of that boundary.
fn unconditional_directives() -> [String; 1] {
    ["cargo:rerun-if-changed=build.rs".to_owned()]
}

/// Render the complete text of `${OUT_DIR}/crc32_tables.rs`.
///
/// Pure: no environment, no filesystem, no randomness, and no dependence on the
/// host or target configuration — both endian forms of every table are always
/// emitted and the consumer selects between them with `cfg!(target_endian)`
/// (AAP §0.3.1). Two calls therefore always produce byte-identical output, which
/// is what makes the generated artifact reproducible.
fn render_tables() -> String {
    // Build every table in memory.
    let (crc_table, crc_big_table) = make_crc_tables();
    let x2n_table = make_x2n_table();
    let (braid_ltl, braid_big) = braid(BRAID_N, BRAID_W, &x2n_table);

    // Emit the Rust source.
    let mut out = String::with_capacity(384 * 1024);
    out.push_str(GENERATED_HEADER);
    // No `#[allow(dead_code)]` is emitted for any item. Whether an unconsumed
    // table is a warning is the *consuming* module's decision, and
    // `src/checksum/crc32.rs` grants that allowance only in the `simd`
    // configuration — with `simd` off its braided word-at-a-time path reads all
    // seven of these, so the compiler itself proves nothing generated is unused.
    let _ = writeln!(out, "pub(crate) const CRC_BRAID_N: usize = {BRAID_N};");
    let _ = writeln!(out, "pub(crate) const CRC_BRAID_W: usize = {BRAID_W};");
    out.push('\n');

    write_u32_array(&mut out, "CRC_TABLE", &crc_table);
    write_u32_array(&mut out, "X2N_TABLE", &x2n_table);
    write_u64_array(&mut out, "CRC_BIG_TABLE", &crc_big_table);
    write_braid_u32(&mut out, "CRC_BRAID_TABLE", &braid_ltl);
    write_braid_u64(&mut out, "CRC_BRAID_BIG_TABLE", &braid_big);

    out
}

/// Render the tables and write them into `dir`, returning the path written.
///
/// Panics only on a genuine I/O failure, which must fail the build: a missing or
/// truncated generated file would produce a confusing `include!` error much
/// later in the compilation.
fn write_tables(dir: &Path) -> std::path::PathBuf {
    let dest = dir.join(GENERATED_FILE);
    fs::write(&dest, render_tables())
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", dest.display()));
    dest
}

// `#[cfg(not(test))]` rather than an `allow`: compiling this file with
// `rustc --test build.rs` supplies its own entry point, and a `main` the harness
// never calls would be reported as dead code.
#[cfg(not(test))]
fn main() {
    for directive in unconditional_directives() {
        println!("{directive}");
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR environment variable not set by Cargo");
    write_tables(Path::new(&out_dir));
}

// ---------------------------------------------------------------------------
// Tests
//
// This file is a build script, so `cargo test` never compiles it. The tests
// below are reached by compiling it as its own test binary:
//
//     rustc --edition 2024 --test build.rs -o <bin> && <bin>
//
// which is exactly what the `build-script-tests` CI job does. Everything here
// is pure `std` and creates files only under a uniquely named subdirectory of
// the system temporary directory, which it removes on the way out.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// This build script's own source, for the emission-contract tests below.
    const SELF_SRC: &str = include_str!("build.rs");

    /// The part of `SELF_SRC` that Cargo actually compiles and runs, i.e.
    /// everything above the `cfg(test)` module, with every whole-line comment
    /// removed.
    ///
    /// Both halves matter. Excluding the test module keeps a test's own scratch
    /// helpers out of the answer, and excluding comments means a directive
    /// spelling cannot be found — or hidden — in prose. This file documents the
    /// directives it deliberately does *not* emit, at length, so a naive
    /// substring search over the raw text would be satisfied by the very
    /// documentation that explains the absence.
    fn script_body() -> String {
        SELF_SRC
            .split("\n#[cfg(test)]\n")
            .next()
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // -----------------------------------------------------------------------
    // Emission contract
    //
    // AAP §0.8.2 Divergence 4 keeps cdylib symbol versioning UNAPPLIED, and
    // §0.10.1 ranks the corresponding gap D8 Low and defers it behind the
    // cross-platform CI matrix (gap D3). An earlier revision of this script
    // wired `zlib.map` in behind an environment variable; it failed the MSRV
    // build outright and bound zero symbols where it did link. These tests pin
    // the removal, because otherwise it is observable only by reading the file
    // — and a build script that quietly regrows a link argument, an opt-in
    // variable, or a `cargo:warning` is precisely the change no other gate in
    // this repository would catch.
    // -----------------------------------------------------------------------

    #[test]
    fn exactly_one_cargo_directive_is_emitted_and_it_is_the_documented_one() {
        let directives = unconditional_directives();
        assert_eq!(
            directives.len(),
            1,
            "the emission contract is exactly one directive; a second one changes \
             what every consumer's build depends on"
        );
        assert_eq!(directives[0], "cargo:rerun-if-changed=build.rs");
    }

    #[test]
    fn the_script_emits_no_link_wiring_and_reads_no_opt_in_variable() {
        let code = script_body();
        // Assembled at run time from fragments so this test's own source cannot
        // satisfy the search it performs.
        let needles = [
            format!("rustc-{}link-arg", ""),
            format!("rustc-{}link-arg", "cdylib-"),
            format!("rerun-if-{}changed", "env-"),
            format!("cargo:{}", "warning"),
            format!("--{}-script", "version"),
            format!("-fuse-{}", "ld"),
            format!("ZLIB_RS_{}_SCRIPT", "VERSION"),
            format!("{}.map", "zlib"),
        ];
        for needle in needles {
            assert!(
                !code.contains(&needle),
                "build.rs must contain no `{needle}` in executable code: gap D8 is \
                 deferred behind gap D3 (AAP §0.8.2 Divergence 4), so this script \
                 emits no link wiring and consults no opt-in"
            );
        }
    }

    #[test]
    fn the_script_itself_is_pure_std_with_no_build_dependencies_and_no_unsafe() {
        let code = script_body();
        // Assembled at run time so this test's own source cannot satisfy it.
        assert!(
            !code.contains(&format!("extern {}", "crate")),
            "AAP §0.5.2 requires this script to be pure `std`: no external crate \
             may be linked into it"
        );
        assert!(
            !code.contains(&format!("un{}", "safe")),
            "AAP §0.5.2 requires ZERO unsafe in this script; the generated tables \
             are produced with plain integer arithmetic"
        );

        // The other half of "no build-dependencies" lives in the manifest: a
        // `[build-dependencies]` table would link a crate into this script no
        // matter what its own source says.
        let manifest = include_str!("Cargo.toml");
        let table = format!("[{}-dependencies]", "build");
        for line in manifest.lines() {
            assert_ne!(
                line.trim(),
                table,
                "AAP §0.5.2 forbids a `{table}` table; `cc`, `bindgen`, and \
                 `pkg-config` are rejected additions, and this script must keep \
                 needing none of them"
            );
        }
    }

    #[test]
    fn the_emitted_file_name_is_the_one_the_consumer_includes() {
        // Producer/consumer drift is the whole reason this job exists: `cargo
        // test` never compiles `build.rs`, so a rename here and a stale
        // `include!` there would only surface as a confusing compile error in an
        // unrelated module.
        let consumer = include_str!("src/checksum/crc32.rs");
        let needle = alloc_format(&GENERATED_FILE.to_owned());
        assert!(
            consumer.contains(&needle),
            "src/checksum/crc32.rs must `include!` `{GENERATED_FILE}` out of \
             OUT_DIR; found no `{needle}`"
        );
        // And the writer must put it exactly there.
        let scratch = Scratch::new("dest-name");
        let dest = write_tables(&scratch.path);
        assert_eq!(
            dest.file_name().and_then(|n| n.to_str()),
            Some(GENERATED_FILE)
        );
    }

    /// The `concat!` argument `src/checksum/crc32.rs` uses to name the generated
    /// file, built here rather than written out so the two cannot drift.
    fn alloc_format(name: &str) -> String {
        format!("\"/{name}\"")
    }

    #[test]
    fn the_only_environment_variable_this_script_reads_is_out_dir() {
        let code = script_body();
        let reads: Vec<&str> = code
            .match_indices("env::var(")
            .map(|(i, _)| {
                let rest = &code[i + "env::var(".len()..];
                rest.split(')').next().unwrap_or_default().trim()
            })
            .collect();
        assert_eq!(
            reads,
            ["\"OUT_DIR\""],
            "`OUT_DIR` is the one variable Cargo guarantees a build script. Reading \
             any other makes the generated artifacts depend on ambient state that \
             no CI row sets and no gate observes"
        );
    }

    // -----------------------------------------------------------------------
    // Generated-table schema
    // -----------------------------------------------------------------------

    /// Reduce an arbitrary ambient string to a single safe path component.
    ///
    /// `CLONE_INDEX` is ambient input: it is read from the environment, so its
    /// value is outside this build script's control. Interpolating it into a path
    /// unfiltered is a directory-traversal defect (CWE-22) — a value such as
    /// `slot/../../security_target` escapes the temporary directory lexically and
    /// resolves somewhere else entirely.
    ///
    /// Only ASCII alphanumerics, `_`, and `-` survive. That drops every character
    /// which could end the component or refer to a parent: `/`, `\`, `.`
    /// (so `..` collapses away entirely), `:`, NUL, and every non-ASCII byte. The
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
    /// `AlreadyExists` when the name is taken — including when it is taken by a
    /// symlink an attacker planted — which is what lets [`Scratch::new`] skip to
    /// the next candidate instead of following the link. On Unix the `0o700` mode
    /// is handed to `mkdir(2)` itself, so the directory is never even briefly
    /// group- or world-accessible; there is no `set_permissions` window to race.
    fn create_private_dir(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            fs::DirBuilder::new().mode(0o700).create(path)
        }
        #[cfg(not(unix))]
        {
            fs::DirBuilder::new().create(path)
        }
    }

    /// A scratch directory unique to this process and call, so the schema tests
    /// are safe to run in parallel and alongside sibling clones of this
    /// repository (each of which sets its own `CLONE_INDEX`).
    ///
    /// The directory is created with create-new semantics inside the system
    /// temporary directory, and its name is assembled only from a
    /// [`safe_component`]-sanitized `CLONE_INDEX`, the process id, a monotonic
    /// counter, and a caller tag. Nothing pre-existing is ever removed: an
    /// occupied candidate name is skipped rather than deleted, so a symlink or
    /// directory planted at a predictable path can neither be destroyed nor
    /// followed (CWE-22 / CWE-367).
    struct Scratch {
        path: std::path::PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU32 = AtomicU32::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let clone = safe_component(&env::var("CLONE_INDEX").unwrap_or_default());
            let tag = safe_component(tag);
            let pid = std::process::id();
            let base = env::temp_dir();

            // Retry only advances the candidate name; it never deletes.
            for attempt in 0..64u32 {
                let candidate = base.join(format!(
                    "blitzy_adhoc_test_buildrs_{tag}_{clone}_{pid}_{n}_{attempt}"
                ));
                match create_private_dir(&candidate) {
                    Ok(()) => return Self { path: candidate },
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("failed to create the scratch directory: {e}"),
                }
            }
            panic!("could not find an unused scratch directory name after 64 attempts");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Safe to recurse: this directory did not exist before `Scratch::new`
            // created it with create-new semantics, so it cannot be a pre-existing
            // path or a symlink into one. Best effort on every path, including a
            // panicking one: leaving a directory behind is not worth masking the
            // original failure.
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// `safe_component` must collapse every traversal and separator form to a
    /// single harmless component.
    ///
    /// The first case is the exact payload the review cited: with the raw value
    /// interpolated, `slot/../../security_target` escaped the temporary directory
    /// lexically and resolved to `/security_target_<pid>_0`. Sanitized, it can only
    /// ever name a child of the temporary directory.
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
        let got = safe_component(&long);
        assert_eq!(got.len(), 32, "an over-long value must be truncated");

        // Characters that are safe are preserved in order.
        assert_eq!(safe_component("clone-07_b"), "clone-07_b");
    }

    /// A `Scratch` must be a freshly created, private, single-level child of the
    /// system temporary directory — never a pre-existing path.
    #[test]
    fn scratch_creates_a_private_new_directory_under_temp() {
        let a = Scratch::new("hygiene");
        assert!(a.path.is_dir(), "the scratch directory must exist");
        assert_eq!(
            a.path.parent(),
            Some(env::temp_dir().as_path()),
            "the scratch directory must sit directly under the temp directory"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&a.path)
                .expect("stat scratch")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "the scratch directory must be owner-only from the moment it exists"
            );
        }

        // Two scratches taken back to back must never collide, and creating one
        // must never adopt an existing directory.
        let b = Scratch::new("hygiene");
        assert_ne!(a.path, b.path, "concurrent scratches must be distinct");

        // Create-new semantics: the name `Scratch` chose is now taken, so a second
        // attempt at exactly that path must be refused rather than reused.
        let err = create_private_dir(&a.path).expect_err("the path is already taken");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::AlreadyExists,
            "an occupied name must report AlreadyExists so the caller can skip it"
        );

        let path_a = a.path.clone();
        drop(a);
        assert!(!path_a.exists(), "Drop must remove the scratch directory");
    }

    /// Parse every `pub(crate) const|static NAME: TYPE = ...;` declaration out of
    /// the generated source, returning `(keyword, name, type)` triples in
    /// declaration order.
    fn declared_items(src: &str) -> Vec<(String, String, String)> {
        let mut items = Vec::new();
        for line in src.lines() {
            let line = line.trim_start();
            let Some(rest) = line.strip_prefix("pub(crate) ") else {
                continue;
            };
            let (keyword, rest) = if let Some(r) = rest.strip_prefix("const ") {
                ("const", r)
            } else if let Some(r) = rest.strip_prefix("static ") {
                ("static", r)
            } else {
                continue;
            };
            let Some((name, rest)) = rest.split_once(':') else {
                continue;
            };
            let Some((ty, _)) = rest.split_once('=') else {
                continue;
            };
            items.push((
                keyword.to_owned(),
                name.trim().to_owned(),
                ty.trim().to_owned(),
            ));
        }
        items
    }

    /// Every `0x…` literal appearing in the generated source, in order.
    fn hex_literals(src: &str) -> Vec<u64> {
        let bytes = src.as_bytes();
        let mut out = Vec::with_capacity(4096);
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'0' && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                let start = i + 2;
                let mut end = start;
                while end < bytes.len() && (bytes[end].is_ascii_hexdigit() || bytes[end] == b'_') {
                    end += 1;
                }
                let digits: String = src[start..end].chars().filter(|c| *c != '_').collect();
                if !digits.is_empty() {
                    out.push(
                        u64::from_str_radix(&digits, 16)
                            .unwrap_or_else(|e| panic!("bad hex literal {digits:?}: {e}")),
                    );
                }
                i = end;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Slice out the hex literals belonging to a single named declaration.
    fn literals_of(src: &str, name: &str) -> Vec<u64> {
        let start = src
            .find(&format!("pub(crate) const {name}:"))
            .or_else(|| src.find(&format!("pub(crate) static {name}:")))
            .unwrap_or_else(|| panic!("{name} is missing from the generated source"));
        // A declaration runs until whichever comes first: the next declaration or
        // the attribute introducing it.
        let body = &src[start..];
        let end = ["\npub(crate) const ", "\npub(crate) static ", "\n#[allow("]
            .iter()
            .filter_map(|sep| body[1..].find(sep))
            .min()
            .map_or(body.len(), |off| off + 1);
        hex_literals(&body[..end])
    }

    #[test]
    fn the_generated_file_declares_exactly_the_seven_promised_items() {
        let scratch = Scratch::new("schema");
        let dest = write_tables(&scratch.path);
        assert_eq!(
            dest,
            scratch.path.join("crc32_tables.rs"),
            "the generated file name is part of the contract with the `include!` in \
             src/checksum/crc32.rs"
        );
        assert_eq!(GENERATED_FILE, "crc32_tables.rs");

        let src = fs::read_to_string(&dest).expect("failed to read the generated file");
        let items = declared_items(&src);

        // Order, names and types are all pinned: the consumer's `use` list and its
        // compile-time schema contract depend on every one of them.
        assert_eq!(
            items,
            vec![
                ("const", "CRC_BRAID_N", "usize"),
                ("const", "CRC_BRAID_W", "usize"),
                ("static", "CRC_TABLE", "[u32; 256]"),
                ("static", "X2N_TABLE", "[u32; 32]"),
                ("static", "CRC_BIG_TABLE", "[u64; 256]"),
                ("static", "CRC_BRAID_TABLE", "[[u32; 256]; 8]"),
                ("static", "CRC_BRAID_BIG_TABLE", "[[u64; 256]; 8]"),
            ]
            .into_iter()
            .map(|(k, n, t)| (k.to_owned(), n.to_owned(), t.to_owned()))
            .collect::<Vec<_>>(),
            "the seven promised outputs, their storage class, their order and their \
             types are all part of the contract with src/checksum/crc32.rs"
        );

        // No item may be emitted twice, and nothing beyond the seven may appear.
        let names: BTreeSet<&str> = items.iter().map(|(_, n, _)| n.as_str()).collect();
        assert_eq!(names.len(), 7, "the seven names must be distinct");

        // Suppression is per item and confined to the five contract-only outputs.
        // `CRC_TABLE` and `X2N_TABLE` must carry none: both are read at run time,
        // so a dead-code warning on either is a signal, not noise.
        // No item may be suppressed. All seven generated artifacts are consumed by
        // src/checksum/crc32.rs (the braided word-at-a-time path reads five of
        // them whenever `simd` is off), so the allowance decision belongs to that
        // module -- which grants it only for the `simd` configuration. Count
        // attribute LINES, not raw substring hits: the generated header's own
        // prose names the attribute, and a substring count would include it.
        assert_eq!(
            src.lines()
                .filter(|l| l.trim() == "#[allow(dead_code)]")
                .count(),
            0,
            "the generated file must not suppress dead-code warnings for any item"
        );
        for (name, allowed) in [
            ("CRC_BRAID_N", false),
            ("CRC_BRAID_W", false),
            ("CRC_TABLE", false),
            ("X2N_TABLE", false),
            ("CRC_BIG_TABLE", false),
            ("CRC_BRAID_TABLE", false),
            ("CRC_BRAID_BIG_TABLE", false),
        ] {
            let decl = src
                .find(&format!("pub(crate) const {name}:"))
                .or_else(|| src.find(&format!("pub(crate) static {name}:")))
                .unwrap_or_else(|| panic!("{name} is missing"));
            // The attribute, if present, is the line immediately above.
            let has_allow = src[..decl].trim_end().ends_with("#[allow(dead_code)]");
            assert_eq!(has_allow, allowed, "dead-code suppression on {name}");
        }
        assert!(
            !src.contains("#![allow"),
            "the generated file must not carry an inner allow attribute: it is \
             included into a module that denies unsafe code and warns on missing docs"
        );
        assert!(
            !src.contains("unsafe"),
            "the generated tables must be plain constants (AAP §0.6.2)"
        );
    }

    #[test]
    fn the_generated_tables_have_the_promised_dimensions() {
        let scratch = Scratch::new("dims");
        let src = fs::read_to_string(write_tables(&scratch.path)).expect("read");

        assert_eq!(literals_of(&src, "CRC_TABLE").len(), 256);
        assert_eq!(literals_of(&src, "X2N_TABLE").len(), 32);
        assert_eq!(literals_of(&src, "CRC_BIG_TABLE").len(), 256);
        assert_eq!(literals_of(&src, "CRC_BRAID_TABLE").len(), 8 * 256);
        assert_eq!(literals_of(&src, "CRC_BRAID_BIG_TABLE").len(), 8 * 256);

        // The braid geometry constants must agree with the emitted dimensions, or
        // a consumer indexing by `CRC_BRAID_W` would run off the end.
        assert!(src.contains(&format!("pub(crate) const CRC_BRAID_N: usize = {BRAID_N};")));
        assert!(src.contains(&format!("pub(crate) const CRC_BRAID_W: usize = {BRAID_W};")));
        assert_eq!(BRAID_N, 5, "crc32.c's N (AAP §0.3.1)");
        assert_eq!(BRAID_W, 8, "crc32.c's W (AAP §0.3.1)");
        assert_eq!(literals_of(&src, "CRC_BRAID_TABLE").len(), BRAID_W * 256);
    }

    #[test]
    fn the_generated_tables_carry_the_reference_anchor_entries() {
        let scratch = Scratch::new("anchors");
        let src = fs::read_to_string(write_tables(&scratch.path)).expect("read");

        // Reflected CRC-32 table, verified against crc32.h in the retained C
        // baseline and against `crc32("123456789") == 0xcbf43926`.
        let crc = literals_of(&src, "CRC_TABLE");
        for (index, expected) in [
            (0_usize, 0x0000_0000_u64),
            (1, 0x7707_3096),
            (2, 0xee0e_612c),
            (3, 0x9909_51ba),
            (255, 0x2d02_ef8d),
        ] {
            assert_eq!(crc[index], expected, "CRC_TABLE[{index}]");
        }

        // x^(2^n) mod p(x), the operators `crc32_combine_gen` selects from.
        let x2n = literals_of(&src, "X2N_TABLE");
        for (index, expected) in [
            (0_usize, 0x4000_0000_u64),
            (1, 0x2000_0000),
            (2, 0x0800_0000),
            (3, 0x0080_0000),
            (4, 0x0000_8000),
            (30, 0xc40b_a6d0),
            (31, 0xc4e2_2c3c),
        ] {
            assert_eq!(x2n[index], expected, "X2N_TABLE[{index}]");
        }

        let big = literals_of(&src, "CRC_BIG_TABLE");
        assert_eq!(big[1], 0x9630_0777_0000_0000);
        assert_eq!(big[255], 0x8def_022d_0000_0000);

        let braid = literals_of(&src, "CRC_BRAID_TABLE");
        assert_eq!(braid[1], 0xaf44_9247, "CRC_BRAID_TABLE[0][1]");
        assert_eq!(braid[7 * 256 + 1], 0x36f2_90f3, "CRC_BRAID_TABLE[7][1]");
        assert_eq!(braid[7 * 256 + 255], 0xf437_7108, "CRC_BRAID_TABLE[7][255]");

        let braid_big = literals_of(&src, "CRC_BRAID_BIG_TABLE");
        assert_eq!(
            braid_big[1], 0xf390_f236_0000_0000,
            "CRC_BRAID_BIG_TABLE[0][1]"
        );
        assert_eq!(
            braid_big[7 * 256 + 1],
            0x4792_44af_0000_0000,
            "CRC_BRAID_BIG_TABLE[7][1]"
        );
        assert_eq!(
            braid_big[7 * 256 + 255],
            0x6575_94e9_0000_0000,
            "CRC_BRAID_BIG_TABLE[7][255]"
        );
    }

    #[test]
    fn the_big_endian_tables_are_byte_swapped_companions_of_the_little_endian_ones() {
        let scratch = Scratch::new("swap");
        let src = fs::read_to_string(write_tables(&scratch.path)).expect("read");

        let crc = literals_of(&src, "CRC_TABLE");
        let big = literals_of(&src, "CRC_BIG_TABLE");
        for i in 0..256 {
            assert_eq!(big[i], crc[i].swap_bytes(), "CRC_BIG_TABLE[{i}]");
        }

        // `braid()` emits the big-endian tables with the word index reversed, so
        // that both forms are indexed identically by the consumer.
        let braid = literals_of(&src, "CRC_BRAID_TABLE");
        let braid_big = literals_of(&src, "CRC_BRAID_BIG_TABLE");
        for k in 0..BRAID_W {
            for i in 0..256 {
                assert_eq!(
                    braid_big[(BRAID_W - 1 - k) * 256 + i],
                    braid[k * 256 + i].swap_bytes(),
                    "CRC_BRAID_BIG_TABLE[{}][{i}]",
                    BRAID_W - 1 - k
                );
            }
        }
    }

    #[test]
    fn rendering_is_deterministic_and_independent_of_the_destination() {
        // Reproducibility of the generated artifact: two renders in the same
        // process, and two writes into different directories, must agree exactly.
        assert_eq!(
            render_tables(),
            render_tables(),
            "render_tables must be pure"
        );

        let a = Scratch::new("det_a");
        let b = Scratch::new("det_b");
        let first = fs::read_to_string(write_tables(&a.path)).expect("read");
        let second = fs::read_to_string(write_tables(&b.path)).expect("read");
        assert_eq!(first, second);
        assert_eq!(first, render_tables());

        // Writing twice into the same directory must overwrite, not append.
        let again = fs::read_to_string(write_tables(&a.path)).expect("read");
        assert_eq!(again, first);
    }

    #[test]
    fn the_generated_header_documents_its_provenance_and_forbids_editing() {
        let scratch = Scratch::new("header");
        let src = fs::read_to_string(write_tables(&scratch.path)).expect("read");

        assert!(src.starts_with("//"), "the file must open with its banner");

        // Scope every banner assertion to the banner itself — everything before the
        // first emitted item — so a name that survives only in its own declaration
        // cannot stand in for its documentation.
        let banner_end = ["\n#[allow(", "\npub(crate) const ", "\npub(crate) static "]
            .iter()
            .filter_map(|sep| src.find(sep))
            .min()
            .expect("the generated file must contain at least one item");
        let banner = &src[..banner_end];
        assert!(
            !banner.contains("pub(crate)"),
            "the banner slice must stop before the first item"
        );

        for fragment in ["build.rs", "crc32.c", "DO NOT EDIT", "crc32.h", "OUT_DIR"] {
            assert!(
                banner.contains(fragment),
                "the generated banner must mention {fragment:?}"
            );
        }
        // The banner must name every symbol it promises, so a reader of the
        // generated file sees the same seven-item contract the consumer imports.
        for (name, ty) in [
            ("CRC_BRAID_N", "usize"),
            ("CRC_BRAID_W", "usize"),
            ("CRC_TABLE", "[u32; 256]"),
            ("X2N_TABLE", "[u32; 32]"),
            ("CRC_BIG_TABLE", "[u64; 256]"),
            ("CRC_BRAID_TABLE", "[[u32; 256]; 8]"),
            ("CRC_BRAID_BIG_TABLE", "[[u64; 256]; 8]"),
        ] {
            let line = banner
                .lines()
                .find(|l| l.contains(&format!("{name}:")))
                .unwrap_or_else(|| panic!("the banner must document the emitted symbol {name}"));
            assert!(
                line.contains(ty),
                "the banner entry for {name} must record its type {ty}, got {line:?}"
            );
        }
        // Several of the promised names are substrings of one another
        // (`CRC_TABLE` of `CRC_BIG_TABLE`, `CRC_BRAID_TABLE` of
        // `CRC_BRAID_BIG_TABLE`), so a `contains` check alone could be satisfied by
        // a neighbouring line. Require exactly one banner line per symbol, so a
        // dropped line cannot be absorbed by another.
        assert_eq!(
            banner
                .lines()
                .filter(|l| l.contains("CRC_") || l.contains("X2N_"))
                .count(),
            7,
            "the banner must carry exactly one line per emitted symbol"
        );
        // The generated tables are only meaningful for zlib's reflected
        // polynomial; a different one would silently produce valid-looking but
        // incompatible checksums.
        assert_eq!(POLY, 0xedb8_8320, "crc32.c's reflected polynomial");
    }

    // -----------------------------------------------------------------------
    // Cross-checks against the algorithms the tables are derived from
    // -----------------------------------------------------------------------

    #[test]
    fn the_generated_crc_table_reproduces_the_canonical_check_value() {
        // `crc32("123456789") == 0xcbf43926` is the standard CRC-32 check value
        // and the vector the C baseline is verified against, so computing it from
        // the emitted table proves the table itself, not just its anchors.
        let (table, _) = make_crc_tables();
        let mut crc = 0xffff_ffff_u32;
        for byte in b"123456789" {
            crc = table[usize::from((crc as u8) ^ byte)] ^ (crc >> 8);
        }
        assert_eq!(crc ^ 0xffff_ffff, 0xcbf4_3926);
    }

    #[test]
    fn the_generated_x2n_table_matches_repeated_squaring() {
        // X2N_TABLE[n] is x^(2^n) mod p(x); each entry must therefore be the
        // square of its predecessor under the same modulus.
        let table = make_x2n_table();
        assert_eq!(table.len(), 32);
        for n in 1..table.len() {
            assert_eq!(
                table[n],
                multmodp(table[n - 1], table[n - 1]),
                "X2N_TABLE[{n}] must be X2N_TABLE[{}] squared",
                n - 1
            );
        }
        // `x2nmodp` starts from x^0 == 1, which in this reflected representation is
        // the high bit, and multiplies in one table entry per set bit of `n`.
        const IDENTITY: u32 = 1 << 31;
        assert_eq!(
            x2nmodp(0, 0, &table),
            IDENTITY,
            "x^(2^k * 0) is the multiplicative identity"
        );
        for entry in table {
            assert_eq!(
                multmodp(entry, IDENTITY),
                entry,
                "x^0 must be a true identity under multmodp"
            );
        }

        // One set bit selects exactly one table entry, offset by `k`. `k == 3` is
        // the byte-shift step `crc32_combine_gen` uses, because a length in bytes
        // is a length in bits times 2^3.
        assert_eq!(x2nmodp(1, 0, &table), table[0]);
        assert_eq!(x2nmodp(2, 0, &table), table[1]);
        assert_eq!(x2nmodp(1, 3, &table), table[3]);
        assert_eq!(x2nmodp(1 << 28, 3, &table), table[31]);

        // Two set bits compose the two corresponding entries, in either order.
        assert_eq!(x2nmodp(3, 0, &table), multmodp(table[0], table[1]));
        assert_eq!(x2nmodp(3, 0, &table), multmodp(table[1], table[0]));
    }

    #[test]
    fn byte_swapping_is_an_involution_on_the_emitted_words() {
        let (table, big) = make_crc_tables();
        for i in 0..256 {
            assert_eq!(byte_swap64(big[i]), u64::from(table[i]));
            assert_eq!(byte_swap64(byte_swap64(big[i])), big[i]);
        }
    }
}
