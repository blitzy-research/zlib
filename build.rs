//! Cargo build script for the `zlib-rs` crate: regenerates the CRC-32 lookup
//! tables at build time and, behind an explicit opt-in, wires zlib's `zlib.map`
//! version script into the `cdylib` link.
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
//! # Emitted contract (consumed by `src/checksum/crc32.rs`)
//!
//! The generated `${OUT_DIR}/crc32_tables.rs` defines exactly these items — the
//! names and shapes are a stable contract; do not rename without updating the
//! consuming module:
//!
//! | Symbol                 | Type                 | Meaning                                                   |
//! |------------------------|----------------------|-----------------------------------------------------------|
//! | `CRC_BRAID_N`          | `usize` (= 5)        | Number of interleaved braids used by the braided path.    |
//! | `CRC_BRAID_W`          | `usize` (= 8)        | Bytes per CRC word for the braided path (64-bit words).    |
//! | `CRC_TABLE`            | `[u32; 256]`         | Byte-wise CRC-32 table (`crc_table` in `crc32.h`).         |
//! | `X2N_TABLE`            | `[u32; 32]`          | Powers of x (`x^(2^n) mod p`) for `crc32_combine`.         |
//! | `CRC_BIG_TABLE`        | `[u64; 256]`         | Byte-swapped table for big-endian word processing.         |
//! | `CRC_BRAID_TABLE`      | `[[u32; 256]; 8]`    | Little-endian braid table (`crc_braid_table`, W=8).        |
//! | `CRC_BRAID_BIG_TABLE`  | `[[u64; 256]; 8]`    | Big-endian braid table (`crc_braid_big_table`, W=8).       |
//!
//! # Determinism
//!
//! The tables are pure mathematical constants derived from the reflected
//! CRC-32/IEEE polynomial `0xEDB88320`. This script performs **no** host or
//! target detection: it always emits the fixed `N = 5`, `W = 8` configuration
//! (the zlib default for 64-bit targets) and generates **both** the
//! little-endian and big-endian braid tables so the runtime can select the
//! correct one via `cfg!(target_endian = ...)`. Output is therefore byte-for-byte
//! identical on every build and every platform. The byte-wise `CRC_TABLE` is
//! endianness- and word-size-independent and always provides a correct fallback.
//!
//! # Optional: cdylib symbol versioning (AAP §0.8.2 Divergence 4 / gap D8)
//!
//! The C build links `libz.so` through `zlib.map`, a GNU-ld *version script*
//! that distributes the exported symbols across sixteen ELF version nodes
//! (`ZLIB_1.2.0` through `ZLIB_1.3.2`). The emitted `cdylib` does not need it:
//! all 54 `global:` names in that script are already exported and all 10
//! `local:` names are already hidden, and an unversioned symbol table satisfies
//! ordinary linking, `pkg-config` consumption and `LD_PRELOAD` injection alike
//! — see the "`zlib.map` symbol-versioning contract" section of
//! `src/ffi/mod.rs`. Reproducing the version nodes is therefore an *optional*
//! packaging nicety, wired up here behind an explicit opt-in.
//!
//! ## Opt-in
//!
//! Set `ZLIB_RS_VERSION_SCRIPT` in the environment before building:
//!
//! ```text
//! ZLIB_RS_VERSION_SCRIPT=1 cargo build --release
//! ```
//!
//! Enabling values, compared case-insensitively after trimming: `1`, `true`,
//! `yes`, `on`. Anything else — the variable unset, empty, `0`, `false`, `no`,
//! `off`, or an unrecognized word — leaves the feature off. With the feature
//! off this script emits no link argument whatsoever, so the default build is
//! unchanged: same generated tables, same link line, same symbol table.
//!
//! ## Why it is opt-in rather than default
//!
//! A functional drop-in links successfully without version tags, so the upside
//! is cosmetic, while applying a version script is the single most
//! linker-dependent thing this crate could do. Two concrete limitations were
//! measured here, across both toolchains this crate supports (`rustc` 1.97.1
//! and the pinned MSRV 1.85.0) against GNU `ld` 2.45:
//!
//! * `rustc` already passes a version script of its own to export the
//!   `#[unsafe(no_mangle)]` shims, and that script uses an **anonymous**
//!   version node. GNU `ld` refuses the combination outright — "anonymous
//!   version tag cannot be combined with other version tags" — while the
//!   `rust-lld` that ships with the toolchain accepts it. Which linker runs is
//!   therefore decisive, and it is *not* uniform across supported toolchains:
//!   enabling this option on `rustc` 1.97.1, whose default linker for
//!   `x86_64-unknown-linux-gnu` is `rust-lld`, links cleanly, whereas enabling
//!   it on `rustc` 1.85.0 — this crate's declared MSRV, which links through
//!   `/usr/bin/ld` — fails. Anyone who needs the option on such a toolchain has
//!   to select an LLD-flavoured linker explicitly (for example
//!   `RUSTFLAGS="-Clink-arg=-fuse-ld=lld"`). The *default* build is unaffected
//!   on every toolchain, because with the opt-in absent no version script of
//!   ours is passed at all.
//! * Because `rustc`'s anonymous node is consulted first and the first match
//!   wins, the individual symbols keep the base version. What the option
//!   actually achieves is that `zlib.map`'s sixteen version nodes are recorded
//!   in the shared object's ELF version-definition section (`.gnu.version_d`,
//!   visible through `readelf --version-info`); it does **not** retag `deflate`
//!   as `deflate@@ZLIB_1.2.0`. Per-symbol tags would require `.symver`
//!   directives, which are out of scope for a build script. `rust-lld` reports
//!   every refused reassignment — "attempt to reassign symbol 'compressBound'
//!   of VER_NDX_GLOBAL to version 'ZLIB_1.2.0'" — and `rustc` surfaces those
//!   through its `linker_messages` lint, so expect one warning per listed
//!   symbol and note that a build which promotes warnings to errors will fail
//!   with the opt-in on. That is another reason it stays off by default: the
//!   crate's own `-D warnings` gates must remain clean.
//!
//! Enabling this by default would trade a working build for a cosmetic gain on
//! an untested matrix, which is why AAP §0.8.2 ranks the gap **Low** and defers
//! it behind the cross-platform CI expansion (gap D3).
//!
//! ## Platform gate
//!
//! The argument is emitted only for ELF targets whose linker understands
//! `--version-script`: a `target_os` of `linux` or `android`, excluding the
//! `musl` environment, decided from the `CARGO_CFG_TARGET_OS` and
//! `CARGO_CFG_TARGET_ENV` variables Cargo sets for build scripts. Mach-O's
//! `ld64` (macOS, iOS) has no such flag — it uses `-exported_symbols_list` with
//! an entirely different file format — and Windows uses `.def` files under
//! MSVC, so emitting it for those targets would be a hard link failure. The
//! upstream C build draws the same boundary: `CMakeLists.txt` applies
//! `zlib.map` only when `UNIX AND NOT APPLE AND NOT AIX AND NOT SunOS`.
//!
//! ## `--undefined-version` is required, not decorative
//!
//! `zlib.map` names symbols that exist only in the C implementation: the ten
//! `local:` entries such as `zcalloc`, `z_errmsg` and `inflate_table` are
//! C-internal identifiers with no Rust counterpart. Current linkers default to
//! `--no-undefined-version` (and `rustc` passes it explicitly), which turns
//! every such entry into a hard error — "version script assignment of 'local'
//! to symbol 'zcalloc' failed: symbol not defined". A companion
//! `-Wl,--undefined-version` restores tolerance; the last occurrence of the
//! flag wins and this script's copy is emitted after `rustc`'s. Without it the
//! option cannot link at all, which is why the two arguments are emitted as a
//! pair and neither is useful alone.
//!
//! ## Missing `zlib.map`
//!
//! `Cargo.toml`'s `exclude` list contains `*.map`, so a consumer building the
//! published `.crate` has no `zlib.map` on disk. That is not an error: the
//! feature is skipped, a `cargo:warning` explains why (only when it was
//! explicitly requested), and the build proceeds. `cargo:rerun-if-changed` is
//! emitted for the script only when it exists, because a `rerun-if-changed`
//! pointing at a nonexistent path makes Cargo re-run this build script on every
//! single invocation.
//!
//! ## Invariant
//!
//! Enabling this option must never change the exported symbol *set* — the 95
//! symbols a Linux build defines, with all 54 `zlib.map` globals present and
//! none of the 10 locals leaked. It adds version metadata and nothing else.
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
/// byte-swapped, matching the C layout so the runtime big-endian path indexes
/// them identically.
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
// Consumed by src/checksum/crc32.rs via:
//     include!(concat!(env!(\"OUT_DIR\"), \"/crc32_tables.rs\"));
//
// Emitted symbols:
//   CRC_BRAID_N:         usize            number of braids (5)
//   CRC_BRAID_W:         usize            bytes per CRC word (8)
//   CRC_TABLE:           [u32; 256]       byte-wise CRC-32 table
//   X2N_TABLE:           [u32; 32]        powers of x for crc32_combine
//   CRC_BIG_TABLE:       [u64; 256]       byte-swapped table (big-endian words)
//   CRC_BRAID_TABLE:     [[u32; 256]; 8]  little-endian braid table
//   CRC_BRAID_BIG_TABLE: [[u64; 256]; 8]  big-endian braid table

";

/// Append a `[u32; N]` static array literal to `out`, eight values per line.
fn write_u32_array(out: &mut String, name: &str, vals: &[u32]) {
    let _ = writeln!(out, "#[allow(dead_code)]");
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
    let _ = writeln!(out, "#[allow(dead_code)]");
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
    let _ = writeln!(out, "#[allow(dead_code)]");
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
    let _ = writeln!(out, "#[allow(dead_code)]");
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
// Optional cdylib symbol versioning (AAP §0.8.2 Divergence 4 / gap D8)
//
// Deliberately self-contained and kept apart from the table generation above:
// the two jobs share no state, and this one must never be able to affect the
// bytes written to `${OUT_DIR}/crc32_tables.rs`.
// ---------------------------------------------------------------------------

/// Environment variable that opts in to `zlib.map` version-script wiring.
///
/// See the "Optional: cdylib symbol versioning" section of this file's
/// documentation for the full rationale.
const VERSION_SCRIPT_ENV: &str = "ZLIB_RS_VERSION_SCRIPT";

/// File name of zlib's linker version script, resolved relative to
/// `CARGO_MANIFEST_DIR`.
const VERSION_SCRIPT_FILE: &str = "zlib.map";

/// Return `true` when `value` is one of the accepted affirmative spellings.
///
/// Matching is case-insensitive and ignores surrounding whitespace, so a value
/// supplied by a shell, a CI matrix entry, or a `[env]` table in
/// `.cargo/config.toml` behaves identically. Everything else — an empty string,
/// `0`, `false`, `no`, `off`, or an unrecognized word — counts as "not
/// requested", which keeps the default build indistinguishable from one made
/// before this option existed.
fn version_script_requested(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Return `true` when the target's linker understands `--version-script`.
///
/// Only ELF targets driven by a GNU-ld-compatible linker qualify. Mach-O
/// (`macos`, `ios`, …) rejects the flag outright and expresses the same idea
/// through `-exported_symbols_list` with a different file format; Windows uses
/// `.def` files under MSVC and a separate mechanism under `gnu`. `musl` is
/// excluded conservatively: its toolchains vary, and the option is not worth a
/// link failure on a configuration nobody has verified. This mirrors the
/// boundary the upstream C build draws in `CMakeLists.txt`.
fn target_accepts_version_script(target_os: &str, target_env: &str) -> bool {
    matches!(target_os, "linux" | "android") && target_env != "musl"
}

/// Wire zlib's `zlib.map` version script into the `cdylib` link, but only when
/// it has been explicitly requested and only on targets that can accept it.
///
/// Every exit path is non-fatal by design: this function never panics and never
/// fails the build. When the opt-in is absent it emits no link argument at all,
/// leaving the default build byte-for-byte as it was.
fn emit_version_script() {
    // Emitted unconditionally so that toggling the opt-in re-runs this script;
    // without it, turning the variable on would have no effect until something
    // else invalidated the build-script fingerprint.
    println!("cargo:rerun-if-env-changed={VERSION_SCRIPT_ENV}");

    // Default path: not requested, so emit nothing further. Keeping this first
    // guarantees that an ordinary build's link line is untouched.
    if !env::var(VERSION_SCRIPT_ENV).is_ok_and(|value| version_script_requested(&value)) {
        return;
    }

    // Cargo exports the *target* configuration (not the host's) to build
    // scripts, so these are the right variables to gate a link argument on.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if !target_accepts_version_script(&target_os, &target_env) {
        println!(
            "cargo:warning={VERSION_SCRIPT_ENV} is set, but target_os=\"{target_os}\" \
             target_env=\"{target_env}\" has no GNU-ld-compatible --version-script; \
             leaving the cdylib symbol table unversioned."
        );
        return;
    }

    // `CARGO_MANIFEST_DIR` is always set by Cargo; treat its absence as "not
    // running under Cargo" and skip rather than panic.
    let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") else {
        println!(
            "cargo:warning={VERSION_SCRIPT_ENV} is set, but CARGO_MANIFEST_DIR is not; \
             skipping cdylib symbol versioning."
        );
        return;
    };

    let script = Path::new(&manifest_dir).join(VERSION_SCRIPT_FILE);
    if !script.is_file() {
        // Expected for a consumer of the published crate: `Cargo.toml`'s
        // `exclude` list drops `*.map`, so the script simply is not there.
        println!(
            "cargo:warning={VERSION_SCRIPT_ENV} is set, but {VERSION_SCRIPT_FILE} was not found \
             in the package root (it is excluded from the published crate); \
             leaving the cdylib symbol table unversioned."
        );
        return;
    }

    // Relative here: Cargo resolves `rerun-if-changed` against the package
    // root, and only now that the file is known to exist, because a
    // `rerun-if-changed` on a missing path re-runs this script every time.
    println!("cargo:rerun-if-changed={VERSION_SCRIPT_FILE}");

    // Absolute here: the linker's working directory is not the package root.
    // A non-UTF-8 path cannot be rendered into a line-oriented Cargo directive
    // without lossy substitution, which would hand the linker a path to a file
    // that does not exist, so skip instead of emitting something subtly wrong.
    let Some(script_path) = script.to_str() else {
        println!(
            "cargo:warning={VERSION_SCRIPT_ENV} is set, but the path to {VERSION_SCRIPT_FILE} is \
             not valid UTF-8 and cannot be passed to the linker; leaving the cdylib symbol table \
             unversioned."
        );
        return;
    };

    // `rustc-cdylib-link-arg` — NOT `rustc-link-arg` — is what confines these
    // arguments to the `cdylib` artifact. The unscoped directive would also be
    // appended to every binary link in the package, including the integration
    // test and benchmark executables, where none of zlib's symbols exist and
    // the version script therefore cannot be satisfied. (`cargo::` with two
    // colons is the modern spelling of these directives and is equivalent; the
    // single-colon form is kept for consistency with the rest of this script.)
    //
    // Tolerance first, script second: `zlib.map` names C-internal symbols that
    // have no Rust counterpart, and current linkers reject unmatched version
    // assignments unless `--undefined-version` is in force.
    println!("cargo:rustc-cdylib-link-arg=-Wl,--undefined-version");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={script_path}");
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    // The tables are pure constants, so we only need to regenerate them when
    // this script itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    // Independent, opt-in, and a no-op unless explicitly requested. Runs before
    // table generation so a link-argument decision can never be skipped by an
    // early exit further down.
    emit_version_script();

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR environment variable not set by Cargo");
    let dest = Path::new(&out_dir).join("crc32_tables.rs");

    // Build every table in memory.
    let (crc_table, crc_big_table) = make_crc_tables();
    let x2n_table = make_x2n_table();
    let (braid_ltl, braid_big) = braid(BRAID_N, BRAID_W, &x2n_table);

    // Emit the Rust source.
    let mut out = String::with_capacity(384 * 1024);
    out.push_str(GENERATED_HEADER);
    let _ = writeln!(out, "pub(crate) const CRC_BRAID_N: usize = {BRAID_N};");
    let _ = writeln!(out, "pub(crate) const CRC_BRAID_W: usize = {BRAID_W};");
    out.push('\n');

    write_u32_array(&mut out, "CRC_TABLE", &crc_table);
    write_u32_array(&mut out, "X2N_TABLE", &x2n_table);
    write_u64_array(&mut out, "CRC_BIG_TABLE", &crc_big_table);
    write_braid_u32(&mut out, "CRC_BRAID_TABLE", &braid_ltl);
    write_braid_u64(&mut out, "CRC_BRAID_BIG_TABLE", &braid_big);

    fs::write(&dest, out).unwrap_or_else(|e| panic!("failed to write {}: {e}", dest.display()));
}
