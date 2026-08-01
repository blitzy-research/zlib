#![no_main]
//! C FFI deflate -> inflate round-trip harness, and the C ABI boundary probes.
//!
//! Exercises the raw pointer / `z_stream` boundary directly: an arbitrary
//! payload is compressed through the `extern "C"` [`deflate`] path and then
//! decompressed through the `extern "C"` [`inflate`] path. The result must
//! equal the original input byte-for-byte. This stresses the handle lifecycle
//! (`*Init_` / `*End`), the `next_in`/`next_out`/`avail_*` pointer arithmetic,
//! and the `Box::into_raw`/`from_raw` opaque-state plumbing at the boundary.
//!
//! `src/ffi/**` is one of the crate's two sanctioned `unsafe` zones — the other
//! being the private freestanding `no_std_support` runtime block in `src/lib.rs`,
//! which no test can reach — and this is the only fuzz target that crosses into
//! the FFI boundary, so the round trip above is paired with five groups of
//! hostile boundary probes a happy path cannot reach:
//!
//! * **Boundary robustness and the `*Init*_` validation ladder** — a null
//!   `z_streamp`, null and wrong-major `version` strings, a mismatched
//!   `stream_size`, out-of-range `level`/`method`/`windowBits`/`memLevel`/
//!   `strategy`, inconsistent `next_*`/`avail_*` pairs, calls against a
//!   never-initialised (null-`state`) stream, a double `*End`, and the
//!   `inflateSync` marker scan. Each must come back as a defined `Z_*` code
//!   without unwinding or aborting across the C ABI.
//!   The library reaches that guarantee with the `pub(crate)` panic guards in
//!   `src/ffi/types.rs` (`guard_int` and friends), which a separate crate cannot
//!   call — so they are observed here only indirectly, through the return values
//!   of the public shims.
//! * **Handle tagging and cross-engine `*End` misuse** — the `HandleKind`
//!   discriminant every `*Init*_` installs at offset 0 of the opaque `state`
//!   handle, and the `Z_STREAM_ERROR` rejection that tag buys when the wrong
//!   engine's terminator is called. That rejection is what turns a
//!   layout-mismatched free, undefined behaviour in C, into a defined error
//!   return.
//! * **The allocator-hook contract** — `zalloc`, `zfree` and `opaque` are public
//!   `z_stream` fields, so this is the only harness able to drive
//!   caller-supplied memory. It pins the C-shaped hook publication, the
//!   allocation/deallocation balance, and the *point* at which a hook reporting
//!   out-of-memory surfaces `Z_MEM_ERROR` — deflate charges every buffer at
//!   init, inflate defers its sliding window.
//! * **The copy / reset lifecycle** — `deflateCopy`, `inflateCopy` and the five
//!   `*Reset*` entry points, driven at a *fuzzer-chosen mid-stream offset* rather
//!   than only at the deterministic boundaries a unit test can reach. A copy taken
//!   part-way through a stream must continue byte-for-byte identically to the
//!   original, must carve its clone from the source's own `zalloc`, and must be
//!   reclaimable exactly once; a reset must zero the observable counters and leave
//!   the stream behaving like a freshly initialised one. These are also the entry
//!   points where C keeps interior pointers into its own arena, so a deep copy
//!   that got the offset bookkeeping wrong would show up here and nowhere else.
//! * **Arbitrary gzip / auto-detect input across the ABI** — `inflateInit2_(47)`
//!   driven with the unmodified fuzz bytes, so the RFC 1952 header parser is
//!   reached through the C ABI and not only through the safe API. Every other leg
//!   here is zlib- or raw-framed, and the sibling `fuzz_gzip` target now drives
//!   gzip exclusively through the safe streaming engine, so without this probe no
//!   target would cover `40..=47` at the boundary at all. See
//!   [`probe_raw_gzip_autodetect`].
//!
//! Every `unsafe` block below carries its own `// SAFETY:` comment, placed
//! immediately before the block it justifies. Where the call would otherwise sit
//! inside an `assert!`/`assert_eq!` invocation its result is bound to a local
//! first: a comment written above a macro invocation documents the *macro*, not
//! the `unsafe` expression the macro expands around, so the proof would not be
//! attached to the operation it is meant to prove. `fuzz/Cargo.toml` denies
//! `clippy::undocumented_unsafe_blocks` and
//! `clippy::multiple_unsafe_ops_per_block` for this workspace so that pairing is
//! machine-checked rather than merely intended, and so that one block can never
//! carry a single proof for several distinct operations.
//!
//! Every probe also stays inside the space where the C API contract defines an
//! answer — and that space is *documented*, not inferred. The "Defined rejections"
//! sections of the `zlib_rs::ffi::deflate` and `zlib_rs::ffi::inflate` module
//! documentation name a null `z_streamp`, a zeroed or never-initialised stream, a
//! stream whose `*End` has already run, a stream carrying another engine's
//! `HandleKind`, a null `next_out`, a positive `avail_in` paired with a null
//! `next_in`, a null `inflateBack` callback and a null `inflateBackInit_` window as
//! *enforced rejections* rather than as caller errors; `inflateBackInit_`
//! additionally documents that its window-size obligation binds only on the
//! accepting path, so probing the rejected `windowBits` range with a smaller buffer
//! — or with none — is in contract. The `*Init*_` shims separately accept a null or
//! non-`"1"` `version` and a mismatched `stream_size`. Handing the library a
//! dangling pointer, or an `avail_*` count that overstates a real allocation, is
//! outside all of that: it would be a defect in this harness rather than a finding
//! about the library, so no probe here does it.
//!
//! # Two rules every probe here obeys
//!
//! **A valid call is asserted, never used as a gate.** Wherever a probe hands the
//! library an in-range configuration with a valid version pair, `Z_OK` is the only
//! answer the C contract permits, so that is what is asserted. Wrapping the body
//! in `if init == Z_OK` instead makes the probe self-disabling: a regression in
//! valid initialisation, the very thing most worth catching, would skip every
//! assertion inside and report a clean run. The single exception is a
//! *deliberately* starved allocator, whose failure is itself the assertion — and
//! even there the failure must be clean, so the handle is required to be absent
//! and the caller's hook to be back in balance. Every `*End` result is likewise a
//! verdict rather than a value to discard.
//!
//! **Return codes are checked per entry point, not against a blanket set.**
//! [`Contract`] carries the exact codes `zlib.h` documents for each shim, and
//! [`assert_exact_code`] pins the outcome wherever the contract pins it. A single
//! "one of the nine" predicate would accept `Z_ERRNO` from `deflate`,
//! `Z_VERSION_ERROR` from `inflateEnd` or `Z_MEM_ERROR` from `inflateSync` —
//! impossible outcomes that a real ABI defect could produce and that a blanket
//! check silently normalises. [`assert_contract_code`] is reserved for the calls
//! whose outcome the contract genuinely leaves open: the `inflateSync` scan over
//! fuzzer-chosen bytes, and the `inflate` that restarts after it.

use core::alloc::Layout;
use core::cell::Cell;
use core::ffi::{c_char, c_int, c_uint, c_void};
use core::ptr;

use libfuzzer_sys::fuzz_target;
use zlib_rs::ffi::{
    HandleKind, deflate, deflateBound, deflateCopy, deflateEnd, deflateInit_, deflateInit2_,
    deflateReset, deflateResetKeep, inflate, inflateBackEnd, inflateBackInit_, inflateCopy,
    inflateEnd, inflateInit_, inflateInit2_, inflateReset, inflateReset2, inflateResetKeep,
    inflateSync, peek_handle_kind, uLong, z_stream,
};

// zlib return codes / flush modes (ABI-stable integer values).
const Z_OK: c_int = 0;
const Z_STREAM_END: c_int = 1;
const Z_FINISH: c_int = 4;
const DEFAULT_LEVEL: c_int = 6;

// The rest of the ABI-stable integer values the boundary probes assert against:
// the remaining six of the nine documented return codes, the `Z_NO_FLUSH` mode,
// and the only `method` zlib accepts. Observed, never altered.
const Z_NEED_DICT: c_int = 2;
const Z_ERRNO: c_int = -1;
const Z_STREAM_ERROR: c_int = -2;
const Z_DATA_ERROR: c_int = -3;
const Z_MEM_ERROR: c_int = -4;
const Z_BUF_ERROR: c_int = -5;
const Z_VERSION_ERROR: c_int = -6;
const Z_NO_FLUSH: c_int = 0;
const Z_DEFLATED: c_int = 8;

/// `windowBits` for a raw (header-less) stream with a 512-byte window. Raw
/// framing keeps the probe handles small and, driven with [`Z_NO_FLUSH`], is the
/// deterministic way to reach inflate's lazily-allocated window: a wrapped
/// stream that has already absorbed its trailing checksum, and a `Z_FINISH`
/// request that has reached its terminal mode, both skip C's `updatewindow`.
const RAW_WINDOW_BITS: c_int = -9;

/// C `deflateInit2_` charges five allocations to the caller's `zalloc`: the state
/// object, then the window, `prev`, `head` and `pending_buf` working buffers.
/// All five are requested during init, so a hook that runs dry anywhere in that
/// sequence reports `Z_MEM_ERROR` from `deflateInit2_` itself — deflate never
/// defers an allocation to a later call.
const DEFLATE_INIT_ALLOCATIONS: usize = 5;

/// C `inflateInit2_` charges exactly one allocation at init — the state object —
/// and defers the sliding window to the first `inflate` call that needs it. That
/// split is the failure *timing* the allocator probes pin.
const INFLATE_INIT_ALLOCATIONS: usize = 1;

/// Ceiling on the payload the copy probes drive through the engines.
///
/// Those probes compress or decompress the same bytes several times — once up to
/// the cut, then once per branch, then once more to verify the finished stream —
/// so the slice is capped to keep executions-per-second high. It stays this
/// generous because the *copy* contract is what needs room: a fuzzer-chosen cut
/// only lands somewhere interesting if there is a stream long enough to cut.
const COPY_PAYLOAD_MAX: usize = 512;

/// Ceiling on the payload [`probe_reset_family`] drives through the engines.
///
/// Deliberately a quarter of [`COPY_PAYLOAD_MAX`], because that probe is the
/// most expensive one here — two full compressions and four full decodes per
/// execution — and, unlike the copy probes, nothing it asserts depends on the
/// payload length: a reset is a property of the *state*, not of the data volume.
/// This much still spans the wrapper header and several blocks at `memLevel = 1`,
/// which is everything a reset has to re-establish.
const RESET_PAYLOAD_MAX: usize = 128;

/// Hard spin ceiling for the bounded drive loops.
///
/// A `deflate`/`inflate` call that neither consumed input nor produced output has
/// stalled, and the loops detect that directly; this counter is the second guard,
/// so a mistake in *this harness* surfaces as a bounded loop rather than as a
/// libFuzzer timeout misattributed to the library. Every legitimate drive below
/// completes in a handful of spins.
const DRIVE_GUARD: usize = 64;

/// An out-of-range `windowBits` for [`inflateReset2`].
///
/// C's `inflateReset2` decodes `windowBits` exactly as `inflateInit2_` does, so
/// `99` yields `wrap = (99 >> 4) + 5 = 11` and leaves `windowBits` at `99`, which
/// then fails the `8..=15` window bound — the documented `Z_STREAM_ERROR`. It is
/// rejected the same way whether or not gzip support is compiled in, because the
/// `& 15` masking that the gzip ranges rely on applies only below `48`.
const BAD_RESET_WINDOW_BITS: c_int = 99;

/// `windowBits` selecting inflate's zlib-or-gzip auto-detect framing:
/// `32 + 15`, the top of the documented `40..=47` range.
///
/// This is the `windowBits` value that reaches the RFC 1952 header parser —
/// magic, `CM`, `FLG`, `MTIME`, `XFL`, `OS`, the optional EXTRA / NAME /
/// COMMENT / header-CRC fields, and the little-endian CRC-32 + ISIZE trailer —
/// through the C ABI rather than the safe API. An ABI / wire-format constant:
/// observed, never altered.
const WBITS_AUTO: c_int = 47;

/// The fixed output window [`probe_raw_gzip_autodetect`] re-presents on every
/// pass.
///
/// A fixed ceiling rather than a buffer sized from the input is what stops a
/// decompression-bomb stream from dominating the run, and it means no allocation
/// here is ever sized from a fuzzer-supplied length.
const AUTODETECT_OUT_WINDOW: usize = 4096;

/// Maximum number of drain passes [`probe_raw_gzip_autodetect`] will make.
///
/// The same budget the sibling gzip harness uses. It bounds total work so a
/// pathological stream cannot consume the whole CI time slice, and an unbounded
/// harness loop is a harness bug rather than a library finding.
const AUTODETECT_WORK_BUDGET: u32 = 4096;

/// The two-byte RFC 1950 header a `windowBits = 9`, level-6 zlib stream emits.
///
/// CMF is `CM = 8` with `CINFO = 9 - 8 = 1`, giving `0x18`; FLG carries
/// `FLEVEL = 2` for level 6 and the check bits that make the big-endian pair a
/// multiple of 31, giving `0x95`. Pinning the bytes — not merely the length —
/// makes the buffer probe's first productive `deflate` call a wire-format
/// assertion as well as a return-code one.
const ZLIB_HEADER_W9_L6: [u8; 2] = [0x18, 0x95];

/// Length of the deterministic payload the deferred-window probe compresses.
///
/// Fixed, and deliberately far larger than the one-byte output window that probe
/// decodes into: the first `inflate` call therefore *cannot* finish the stream,
/// which is what forces C's `inf_leave` to request the lazily-allocated sliding
/// window. Deriving the length from the fuzz input instead would make that
/// forcing input-dependent — precisely the weakness that let the probe pass
/// without ever reaching the allocation it exists to test.
const WINDOW_PROBE_LEN: usize = 64;

/// `windowBits` for a zlib-wrapped stream with the maximum window, used by the
/// `inflateSync` probe because sync recovery is a wrapped-stream facility.
const WBITS_ZLIB: c_int = 15;

/// Upper bound on the bytes handed to the `inflateSync` scan, so an enormous
/// fuzz input cannot dominate an execution that has many other probes to run.
const MAX_SYNC_SCAN: usize = 64;

/// A byte string carrying the `00 00 FF FF` empty-stored-block marker
/// `inflateSync` searches for, with two bytes of noise on either side.
///
/// The marker is what C's `Z_SYNC_FLUSH` emits, so this is the shape a caller
/// recovering a truncated stream actually meets.
const SYNC_FRAMED: [u8; 8] = [0x11, 0x22, 0x00, 0x00, 0xff, 0xff, 0x33, 0x44];

/// One past the last marker byte in [`SYNC_FRAMED`]. A successful `inflateSync`
/// consumes exactly this many bytes: everything it scanned, up to and including
/// the four-byte marker.
const SYNC_MARKER_END: usize = 6;

/// The valid `"1"` major-version string every `*Init*_` shim expects.
///
/// `c"1"` is a `&'static CStr` whose bytes are `[0x31, 0x00]` — byte-identical to
/// the `b"1\0"` form, with `CStr::as_ptr` already yielding `*const c_char` so no
/// cast is needed. The pointer is `'static`, so returning it is sound.
#[inline]
fn version_ok() -> *const c_char {
    c"1".as_ptr()
}

/// `sizeof(z_stream)`, the `stream_size` every `*Init*_` shim expects.
#[inline]
fn stream_size_ok() -> c_int {
    core::mem::size_of::<z_stream>() as c_int
}

/// A freshly zeroed `z_stream` for the probes below.
fn zeroed_stream() -> z_stream {
    // SAFETY: all-zero `z_stream` is the C `memset` idiom; every field is a
    // valid zero (null pointers, `None` hooks, zero counters).
    unsafe { core::mem::zeroed() }
}

/// Asserts that `ret` is one of the nine return codes zlib is allowed to produce
/// *at all*. This is the outer, ABI-level guard: any other integer is an
/// undefined value escaping across the C ABI, whichever entry point produced it.
///
/// It is deliberately never asserted on its own. "One of the nine" is a far
/// weaker statement than what any individual entry point documents, so
/// [`assert_contract_code`] and [`assert_exact_code`] layer the per-entry-point
/// contract on top of it and are what the probes actually call.
fn assert_abi_integer(ret: c_int, what: &str) {
    assert!(
        matches!(
            ret,
            Z_OK | Z_STREAM_END
                | Z_NEED_DICT
                | Z_ERRNO
                | Z_STREAM_ERROR
                | Z_DATA_ERROR
                | Z_MEM_ERROR
                | Z_BUF_ERROR
                | Z_VERSION_ERROR
        ),
        "{what} returned {ret}, which is not one of the nine zlib return codes"
    );
}

/// The C entry points these probes drive, each carrying the exact set of return
/// codes `zlib.h` documents for it.
///
/// A single "one of the nine" predicate is much too weak to serve as an oracle:
/// it accepts `Z_ERRNO` from `deflate`, `Z_VERSION_ERROR` from `inflateEnd`,
/// `Z_NEED_DICT` from `deflate` and `Z_MEM_ERROR` from `inflateSync` — outcomes
/// the C contract makes impossible. A port that produced one of those would be a
/// real ABI defect, and a blanket membership check normalises it as valid.
/// Naming the entry point pins what each one may actually answer.
#[derive(Clone, Copy)]
enum Contract {
    /// `deflateInit_` and `deflateInit2_`, which share one validation ladder.
    DeflateInit,
    /// `inflateInit_` and `inflateInit2_`, likewise.
    InflateInit,
    /// `inflateBackInit_`.
    InflateBackInit,
    /// `deflate`.
    Deflate,
    /// `inflate`.
    Inflate,
    /// `deflateEnd`.
    DeflateEnd,
    /// `inflateEnd`.
    InflateEnd,
    /// `inflateBackEnd`.
    InflateBackEnd,
    /// `inflateSync`.
    InflateSync,
    /// `deflateCopy` — `Z_OK`, `Z_MEM_ERROR` when the duplicate state cannot be
    /// reserved, `Z_STREAM_ERROR` for an inconsistent source stream.
    DeflateCopy,
    /// `inflateCopy` — the same three codes, for the same reasons.
    InflateCopy,
}

impl Contract {
    /// The C name, used verbatim in every diagnostic so a failure names the
    /// symbol a consumer would be calling.
    fn entry_point(self) -> &'static str {
        match self {
            Self::DeflateInit => "deflateInit_/deflateInit2_",
            Self::InflateInit => "inflateInit_/inflateInit2_",
            Self::InflateBackInit => "inflateBackInit_",
            Self::Deflate => "deflate",
            Self::Inflate => "inflate",
            Self::DeflateEnd => "deflateEnd",
            Self::InflateEnd => "inflateEnd",
            Self::InflateBackEnd => "inflateBackEnd",
            Self::InflateSync => "inflateSync",
            Self::DeflateCopy => "deflateCopy",
            Self::InflateCopy => "inflateCopy",
        }
    }

    /// Every code this entry point is documented to return, and nothing else.
    ///
    /// Sourced from `zlib.h`: the three init families add `Z_VERSION_ERROR` for
    /// the version / `stream_size` ladder and `Z_MEM_ERROR` for a refused
    /// allocation. `deflate` cannot fail for lack of memory — it owns every
    /// buffer from init onwards — whereas `inflate` can, because it defers its
    /// sliding window. `deflateEnd` alone adds `Z_DATA_ERROR`, for a stream
    /// discarded mid-block. `inflateSync` reports `Z_DATA_ERROR` when the input
    /// runs out before a marker appears and `Z_BUF_ERROR` when there was no
    /// input to scan. `Z_ERRNO` appears in none of them: it belongs to the
    /// `gz*` file layer, which has no entry point here.
    fn allowed(self) -> &'static [c_int] {
        match self {
            Self::DeflateInit | Self::InflateInit | Self::InflateBackInit => {
                &[Z_OK, Z_STREAM_ERROR, Z_MEM_ERROR, Z_VERSION_ERROR]
            }
            Self::Deflate => &[Z_OK, Z_STREAM_END, Z_STREAM_ERROR, Z_BUF_ERROR],
            Self::Inflate => &[
                Z_OK,
                Z_STREAM_END,
                Z_NEED_DICT,
                Z_STREAM_ERROR,
                Z_DATA_ERROR,
                Z_MEM_ERROR,
                Z_BUF_ERROR,
            ],
            Self::DeflateEnd => &[Z_OK, Z_STREAM_ERROR, Z_DATA_ERROR],
            Self::InflateEnd | Self::InflateBackEnd => &[Z_OK, Z_STREAM_ERROR],
            Self::InflateSync => &[Z_OK, Z_STREAM_ERROR, Z_DATA_ERROR, Z_BUF_ERROR],
            Self::DeflateCopy | Self::InflateCopy => &[Z_OK, Z_MEM_ERROR, Z_STREAM_ERROR],
        }
    }
}

/// Asserts `ret` is a code `contract`'s entry point is documented to produce.
///
/// Reserved for the calls where the C contract genuinely leaves the outcome
/// open — scanning fuzzer-chosen bytes for a sync marker, for instance, may
/// legitimately find one, run out of input, or have nothing to scan. Everywhere
/// the outcome *is* pinned, [`assert_exact_code`] is the stronger oracle and is
/// used instead.
fn assert_contract_code(ret: c_int, contract: Contract, what: &str) {
    assert_abi_integer(ret, what);
    assert!(
        contract.allowed().contains(&ret),
        "{what}: {} returned {ret}, which is outside the codes it documents: {:?}",
        contract.entry_point(),
        contract.allowed()
    );
}

/// Asserts `ret` is exactly `expected`, having first confirmed `expected` is
/// itself a code `contract`'s entry point may produce.
///
/// The two-step shape is deliberate. The inner check keeps this harness honest:
/// an expectation that has drifted outside the entry point's documented set is a
/// defect in the probe, and it is reported as one rather than masquerading as a
/// library failure.
fn assert_exact_code(ret: c_int, contract: Contract, expected: c_int, what: &str) {
    assert!(
        contract.allowed().contains(&expected),
        "probe error: {what} expects {expected} from {}, which does not document it: {:?}",
        contract.entry_point(),
        contract.allowed()
    );
    assert_abi_integer(ret, what);
    assert_eq!(
        ret,
        expected,
        "{what}: {} returned {ret}, but the C contract pins {expected} here",
        contract.entry_point()
    );
}

// ===========================================================================
// Allocator hooks — caller-supplied memory driven through the public `z_stream`
// ===========================================================================

/// Per-execution allocator bookkeeping, reached by the hooks through
/// [`z_stream::opaque`] rather than through a `static mut`, so concurrent or
/// nested streams cannot share counters.
struct HookState {
    /// Blocks handed out by [`hook_zalloc`].
    handed_out: Cell<usize>,
    /// Blocks released by [`hook_zfree`].
    released: Cell<usize>,
    /// Requests refused because the budget was exhausted.
    refused: Cell<usize>,
    /// Successful allocations still permitted; at `0` the hook reports
    /// out-of-memory, which is how a caller's `zalloc` signals failure in C.
    budget: Cell<usize>,
}

impl HookState {
    /// A hook allowing `budget` further successful allocations.
    fn with_budget(budget: usize) -> Self {
        Self {
            handed_out: Cell::new(0),
            released: Cell::new(0),
            refused: Cell::new(0),
            budget: Cell::new(budget),
        }
    }

    /// Asserts every block handed out was handed back — an imbalance at the
    /// boundary is a leak (which would eventually trip the fuzzer's RSS limit
    /// and masquerade as an out-of-memory finding) or a double free.
    fn assert_balanced(&self, what: &str) {
        assert_eq!(
            self.handed_out.get(),
            self.released.get(),
            "{what}: every caller allocation must be released through zfree"
        );
    }
}

/// Size of the `usize` header each block carries so [`hook_zfree`] can rebuild
/// the exact [`Layout`]. It doubles as the block alignment, which is at least
/// `align_of::<usize>()` and therefore suitable for every element type the
/// engines request.
const HOOK_HEADER: usize = core::mem::size_of::<usize>();

/// C `alloc_func` hook: hands out a `usize`-headed block while the budget
/// allows, and reports out-of-memory by returning null otherwise.
///
/// # Safety
///
/// `opaque` must be null or address a live [`HookState`] that outlives every
/// call the library makes through this hook. The body never panics: an unwind
/// out of a hook would cross the C ABI, which is precisely the undefined
/// behaviour the library's own boundary guards exist to prevent, so every
/// fallible step returns null instead.
unsafe extern "C" fn hook_zalloc(opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
    if opaque.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `opaque` is the `&HookState` installed by `install_hooks`, which
    // lives on the probe's stack frame and therefore outlives this call.
    let state = unsafe { &*(opaque as *const HookState) };
    // Saturating throughout: `overflow-checks = true` in `fuzz/Cargo.toml` would
    // otherwise turn an arithmetic slip in this harness into a false finding.
    let bytes = (items as usize).saturating_mul(size as usize);
    if bytes == 0 || state.budget.get() == 0 {
        state.refused.set(state.refused.get().saturating_add(1));
        return ptr::null_mut();
    }
    let total = bytes.saturating_add(HOOK_HEADER);
    let Ok(layout) = Layout::from_size_align(total, HOOK_HEADER) else {
        state.refused.set(state.refused.get().saturating_add(1));
        return ptr::null_mut();
    };
    // SAFETY: `layout` has a non-zero size, since `total >= HOOK_HEADER > 0`.
    let raw = unsafe { std::alloc::alloc(layout) };
    if raw.is_null() {
        state.refused.set(state.refused.get().saturating_add(1));
        return ptr::null_mut();
    }
    // SAFETY: `raw` owns `total >= HOOK_HEADER` bytes aligned for `usize`, so the
    // leading header is in bounds and correctly aligned to write.
    unsafe { *(raw as *mut usize) = total };
    state.budget.set(state.budget.get().saturating_sub(1));
    state
        .handed_out
        .set(state.handed_out.get().saturating_add(1));
    // SAFETY: `HOOK_HEADER < total`, so the returned pointer stays inside the
    // same allocation and retains `usize` alignment.
    unsafe { raw.add(HOOK_HEADER) as *mut c_void }
}

/// C `free_func` hook: rebuilds the layout from the block header and releases it.
///
/// # Safety
///
/// `opaque` must be null or address the live [`HookState`] paired with
/// [`hook_zalloc`], and `address` must be null or a block that hook returned and
/// that has not been released yet. Panic-free for the same reason.
unsafe extern "C" fn hook_zfree(opaque: *mut c_void, address: *mut c_void) {
    if opaque.is_null() || address.is_null() {
        return;
    }
    // SAFETY: `address` came from `hook_zalloc`, so its `usize` size header
    // occupies the `HOOK_HEADER` bytes immediately before it and stepping back
    // stays inside that same allocation.
    let raw = unsafe { (address as *mut u8).sub(HOOK_HEADER) };
    // SAFETY: `raw` addresses the `usize` header `hook_zalloc` wrote.
    let total = unsafe { *(raw as *const usize) };
    // `hook_zalloc` built this exact layout successfully, so the rebuild cannot
    // fail; returning instead of unwrapping keeps the hook panic-free.
    let Ok(layout) = Layout::from_size_align(total, HOOK_HEADER) else {
        return;
    };
    // SAFETY: `opaque` is the live `&HookState` paired with `hook_zalloc`.
    let state = unsafe { &*(opaque as *const HookState) };
    state.released.set(state.released.get().saturating_add(1));
    // SAFETY: `raw` and `layout` are exactly the pointer and layout
    // `hook_zalloc` allocated, so this is the matching deallocation.
    unsafe { std::alloc::dealloc(raw, layout) };
}

/// Publishes `hooks` as the stream's `zalloc`/`zfree`/`opaque` triple.
///
/// Both halves are always installed together. A *half*-present pair is
/// deliberately never built: C's `*Init*_` prologue completes it by substituting
/// its own built-in for the missing half (and clears `opaque` when it is
/// `zalloc` that was defaulted), so the engine would then allocate with one
/// allocator and free with the other. That mismatch would be a defect introduced
/// by this harness, not a finding about the library.
fn install_hooks(strm: &mut z_stream, hooks: &HookState) {
    strm.zalloc = Some(hook_zalloc);
    strm.zfree = Some(hook_zfree);
    strm.opaque = (hooks as *const HookState) as *mut c_void;
}

// ===========================================================================
// The round trip — the reason this target exists
// ===========================================================================

/// Compresses `data` through the `extern "C"` deflate path and decompresses it
/// back through the `extern "C"` inflate path, asserting byte-for-byte recovery.
///
/// Hoisted out of [`fuzz_target!`] so the boundary probes that follow cannot be
/// skipped by anything that happens here.
///
/// The stream decoded here is produced by the library itself, so unlike the probes
/// below these are hard assertions: a mismatch genuinely is a bug.
///
/// Every step is a hard assertion, with no escape hatch. Both `*Init_` calls are
/// given a valid version pair and an in-range level, so `Z_OK` is the only
/// outcome the C contract allows; the output buffer is checked against the
/// library's own [`deflateBound`], so a single `Z_FINISH` must reach
/// `Z_STREAM_END`; and both `*End` calls are checked for their exact code rather
/// than discarded. Returning early on any of those instead — as this harness used
/// to — turns a regression that breaks valid initialisation, or one that stops a
/// bounded `Z_FINISH` completing, into a silently skipped round trip: the
/// equality assertions at the bottom would simply never run.
fn ffi_round_trip(data: &[u8]) {
    // The valid version string and the correct `stream_size`. `c"1"` is a static
    // C string literal: it carries the bytes `[0x31, 0x00]` with a guaranteed
    // terminating NUL and `'static` storage, which is exactly what the shim's
    // version check reads, and it satisfies `manual_c_str_literals` without
    // suppressing the lint.
    let version = c"1".as_ptr();
    let size = core::mem::size_of::<z_stream>() as c_int;

    // ---- Compress through the C ABI deflate path. -------------------------
    // SAFETY: all-zero `z_stream` is the C `memset` idiom; every field is a
    // valid zero (null pointers, `None` hooks, zero counters).
    let mut ds: z_stream = unsafe { core::mem::zeroed() };
    // SAFETY: `ds` is a valid owned `z_stream`; `version`/`size` are what the C
    // API expects; `DEFAULT_LEVEL` is in range.
    assert_exact_code(
        unsafe { deflateInit_(&mut ds, DEFAULT_LEVEL, version, size) },
        Contract::DeflateInit,
        Z_OK,
        "deflateInit_ with a valid version pair and an in-range level",
    );
    // Every `*Init*_` shim tags the opaque handle at offset 0, so the tag is
    // observable the instant initialisation succeeds. Reading it here costs no
    // extra allocation.
    // SAFETY: `ds.state` was just installed by `deflateInit_` as a tagged
    // `#[repr(C)]` handle, so its leading `HandleKind` is readable.
    let ds_kind = unsafe { peek_handle_kind(&ds) };
    assert_eq!(
        ds_kind,
        Some(HandleKind::DEFLATE),
        "deflateInit_ must install a DEFLATE-tagged handle"
    );

    // 1.5x + 128 comfortably exceeds the zlib deflate bound for any input — and
    // that is not left as a claim: the library's own `deflateBound` is queried
    // for this handle and this payload, so the `Z_STREAM_END` requirement below
    // rests on a checked capacity rather than on arithmetic that happens to be
    // generous. `deflateBound` reads the live handle's wrap and window geometry,
    // so it is called after init.
    let mut comp = vec![0u8; data.len() + data.len() / 2 + 128];
    // SAFETY: `ds` holds the live deflate handle installed above, which is all
    // `deflateBound` inspects.
    let bound = unsafe { deflateBound(&mut ds, data.len() as uLong) };
    assert!(
        comp.len() as uLong >= bound,
        "the round-trip output buffer holds {} bytes but deflateBound requires \
         {bound} for a {}-byte payload",
        comp.len(),
        data.len()
    );
    ds.next_in = data.as_ptr();
    ds.avail_in = data.len() as c_uint;
    ds.next_out = comp.as_mut_ptr();
    ds.avail_out = comp.len() as c_uint;
    // SAFETY: `next_in`/`next_out` point at the live buffers with matching
    // `avail_*` counts; a single `Z_FINISH` completes in one call given the
    // over-sized output buffer.
    let dret = unsafe { deflate(&mut ds, Z_FINISH) };
    let produced = comp.len() - ds.avail_out as usize;
    // Teardown first, so the handle is reclaimed before any verdict can panic —
    // and the terminator's own result is captured, because it is a verdict too.
    // SAFETY: `ds` was initialized above; reclaim its internal state.
    let dend = unsafe { deflateEnd(&mut ds) };
    // SAFETY: `deflateEnd` nulled `ds.state`, so the tag read finds no handle.
    let ds_kind_after_end = unsafe { peek_handle_kind(&ds) };
    assert!(
        ds_kind_after_end.is_none(),
        "deflateEnd must clear the state handle"
    );
    // `comp` is `1.5x + 128`, which strictly exceeds C's `deflateBound` for the
    // default level-6 configuration (`sourceLen + (sourceLen + 7) / 8 +
    // (sourceLen + 63) / 64 + 5 + 6`, about `1.15x + 11`) at every length
    // libFuzzer can supply. A single `Z_FINISH` into a buffer that satisfies
    // `deflateBound` is therefore required to complete the stream — that is
    // exactly what the bound guarantees.
    assert_exact_code(
        dret,
        Contract::Deflate,
        Z_STREAM_END,
        "a single Z_FINISH into a deflateBound-sized buffer",
    );
    // And a completed stream has nothing pending, so its teardown is `Z_OK`; the
    // `Z_DATA_ERROR` that `deflateEnd` reports for a stream abandoned mid-block
    // cannot arise here.
    assert_exact_code(
        dend,
        Contract::DeflateEnd,
        Z_OK,
        "deflateEnd after a completed Z_FINISH",
    );

    // ---- Decompress through the C ABI inflate path. -----------------------
    // SAFETY: `z_stream` is a `#[repr(C)]` struct of raw pointers, `c_uint`
    // counters, `Option<fn>` hook slots and integers, so the all-zero bit
    // pattern is a valid value of every one of its fourteen fields — null
    // pointers, `None` hooks, zero counters — which is the state C's `memset`
    // idiom produces before an `*Init*_` call.
    let mut is: z_stream = unsafe { core::mem::zeroed() };
    // SAFETY: valid owned `z_stream`; version/size as the C API expects.
    assert_exact_code(
        unsafe { inflateInit_(&mut is, version, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit_ with a valid version pair",
    );
    // SAFETY: `is.state` was just installed by `inflateInit_` as a tagged
    // `#[repr(C)]` handle, so its leading `HandleKind` is readable.
    let is_kind = unsafe { peek_handle_kind(&is) };
    assert_eq!(
        is_kind,
        Some(HandleKind::INFLATE),
        "inflateInit_ must install an INFLATE-tagged handle"
    );

    // `max(1)` avoids a zero-length output buffer on the empty-payload edge.
    let mut back = vec![0u8; data.len().max(1)];
    is.next_in = comp.as_ptr();
    is.avail_in = produced as c_uint;
    is.next_out = back.as_mut_ptr();
    is.avail_out = back.len() as c_uint;
    // SAFETY: `next_in`/`next_out` point at the live buffers with matching
    // `avail_*` counts.
    let iret = unsafe { inflate(&mut is, Z_FINISH) };
    let got = back.len() - is.avail_out as usize;
    // SAFETY: `is` was initialized above; reclaim its internal state.
    let iend = unsafe { inflateEnd(&mut is) };
    // SAFETY: `inflateEnd` nulled `is.state`, so the tag read finds no handle.
    let is_kind_after_end = unsafe { peek_handle_kind(&is) };
    assert!(
        is_kind_after_end.is_none(),
        "inflateEnd must clear the state handle"
    );

    assert_exact_code(
        iret,
        Contract::Inflate,
        Z_STREAM_END,
        "a single Z_FINISH decode of the library's own complete stream",
    );
    // `inflateEnd` has no `Z_DATA_ERROR` case at all, so `Z_OK` is the only
    // outcome for a live handle.
    assert_exact_code(
        iend,
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the round-trip decode",
    );
    assert_eq!(got, data.len(), "ffi round-trip length mismatch");
    assert_eq!(&back[..got], data, "ffi round-trip content mismatch");
    // `inflateEnd` reclaims whatever handle it finds and reports `Z_OK`; its only
    // other answer is the `Z_STREAM_ERROR` of a stream carrying no handle, which
    // the successful decode above has already ruled out.
    assert_eq!(
        iend, Z_OK,
        "inflateEnd must report Z_OK for a live inflate handle"
    );
}

// ===========================================================================
// N1 — boundary robustness and the `*Init*_` validation ladder
// ===========================================================================

/// Drives the version / `stream_size` / null-stream ladder of all five versioned
/// init entry points.
///
/// Every rejection here happens before a single byte is allocated, so nothing
/// needs tearing down — which is itself asserted, because an init that reported
/// failure while installing a handle would leak on every execution.
fn probe_init_ladder() {
    let good = version_ok();
    let size = stream_size_ok();
    // A well-formed NUL-terminated string whose major-version digit is `'0'`,
    // not `'1'` — bytes `[0x30, 0x00]`.
    let bad_major = c"0".as_ptr();
    let no_version = ptr::null::<c_char>();
    let no_stream = ptr::null_mut::<z_stream>();
    let mut window = [0u8; 1 << 9];

    // A null `version`, a leading byte other than the major-version digit `'1'`,
    // or a `stream_size` that disagrees with `sizeof(z_stream)` is
    // `Z_VERSION_ERROR` for every init entry point.
    for (version, stream_size) in [
        (no_version, size),
        (bad_major, size),
        (good, 0),
        (good, -1),
        (good, size.wrapping_add(1)),
        (good, size.wrapping_sub(1)),
    ] {
        let mut ds = zeroed_stream();
        // SAFETY: `ds` is a valid owned `z_stream`. `version` is either null or
        // a NUL-terminated literal, which is exactly what the shim's own
        // null-check-then-read-first-byte contract accepts.
        let ret = unsafe { deflateInit_(&mut ds, DEFAULT_LEVEL, version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "deflateInit_ must reject a bad version/stream_size pair"
        );
        // SAFETY: `ds` is a valid `z_stream`; the tag read is a no-op when
        // `state` is null, which is what a rejected init must leave behind.
        let kind = unsafe { peek_handle_kind(&ds) };
        assert!(
            kind.is_none(),
            "a rejected deflateInit_ must install no handle"
        );

        let mut is = zeroed_stream();
        // SAFETY: `is` is a live, exclusively borrowed, all-zero `z_stream` whose
        // `state` is null; `version` is either a null pointer or a static
        // NUL-terminated literal, both of which the shim's null-check-then-read
        // -first-byte contract accepts, and `stream_size` is a plain `c_int`.
        let ret = unsafe { inflateInit_(&mut is, version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "inflateInit_ must reject a bad version/stream_size pair"
        );
        // SAFETY: `is` is still that live all-zero `z_stream` — the rejected call
        // above installed no state — `version` is null or a static NUL-terminated
        // literal, and `15` is a valid zlib `windowBits`.
        let ret = unsafe { inflateInit2_(&mut is, 15, version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "inflateInit2_ must reject a bad version/stream_size pair"
        );
        // The pointer is taken before the proof so the `unsafe` block stays on
        // the line immediately after its `// SAFETY:` comment; wrapped across two
        // lines the comment would document the `let`, not the block.
        let win = window.as_mut_ptr();
        // SAFETY: `win` addresses a live 512-byte buffer matching
        // `windowBits = 9` that outlives this call, and the version guard rejects
        // before it is used at all.
        let ret = unsafe { inflateBackInit_(&mut is, 9, win, version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "inflateBackInit_ must reject a bad version/stream_size pair"
        );
        // SAFETY: `is` is a valid `z_stream` whose `state` must still be null.
        let kind = unsafe { peek_handle_kind(&is) };
        assert!(
            kind.is_none(),
            "a rejected inflate init must install no handle"
        );
    }

    // The version and size guards run *before* the null-stream guard, so a call
    // that is wrong in both ways reports `Z_VERSION_ERROR`. That ordering is
    // observable behaviour a caller can depend on, so it is pinned exactly.
    // SAFETY: passing a null `z_streamp` is a defined, rejected call — the shim
    // checks the pointer instead of dereferencing it.
    let ret = unsafe { deflateInit_(no_stream, DEFAULT_LEVEL, bad_major, size) };
    assert_eq!(
        ret, Z_VERSION_ERROR,
        "the version guard must precede the null-stream guard in deflateInit_"
    );
    // SAFETY: `no_stream` is null, which the shim checks before any dereference;
    // `good` points at a static NUL-terminated version literal and the mismatched
    // `stream_size` of `0` is a plain `c_int`.
    assert_eq!(
        ret, Z_VERSION_ERROR,
        "the stream_size guard must precede the null-stream guard in inflateInit_"
    );
    // SAFETY: `no_stream` is null and checked before any dereference; `bad_major`
    // is a static NUL-terminated literal; the null window pointer is likewise
    // only ever checked, and the version guard rejects before either is used.
    assert_eq!(
        ret, Z_VERSION_ERROR,
        "the version guard must precede the null-stream guard in inflateBackInit_"
    );

    // A null stream with an otherwise valid call is `Z_STREAM_ERROR`.
    // SAFETY: a null `z_streamp` is checked, never dereferenced.
    let ret = unsafe { deflateInit_(no_stream, DEFAULT_LEVEL, good, size) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "deflateInit_ must reject a null stream"
    );
    // SAFETY: `no_stream` is null, which the shim checks before dereferencing it;
    // `good` points at a static NUL-terminated version literal that is readable
    // for the bytes the version guard inspects, and `size` is a plain `c_int`.
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateInit_ must reject a null stream"
    );
    // SAFETY: as for the call above — `no_stream` is checked, never dereferenced,
    // and `good`/`size` are a readable static version literal and a `c_int` — plus
    // `window` is a live, exclusively borrowed 512-byte buffer matching
    // `windowBits = 9` that outlives this call.
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateBackInit_ must reject a null stream"
    );
}

/// Drives out-of-range `deflateInit2_`, `inflateInit2_` and `inflateBackInit_`
/// parameters.
///
/// The rows are the *measured* rejections, not a guess: inflate's `windowBits`
/// domain is deliberately wider than deflate's — `0` means "take the window size
/// from the header", and the `+16`/`+32` gzip and auto-detect bands are legal —
/// so only genuinely invalid values are asserted against.
fn probe_parameter_validation() {
    let good = version_ok();
    let size = stream_size_ok();
    let mut window = [0u8; 1 << 9];

    // (level, method, windowBits, memLevel, strategy) rows that C rejects.
    for (level, method, window_bits, mem_level, strategy) in [
        (10, Z_DEFLATED, 15, 8, 0),
        (-2, Z_DEFLATED, 15, 8, 0),
        (DEFAULT_LEVEL, 9, 15, 8, 0),
        (DEFAULT_LEVEL, 0, 15, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 0, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 7, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 16, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 32, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 48, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, -7, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, -16, 8, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 15, 0, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 15, 10, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 15, -1, 0),
        (DEFAULT_LEVEL, Z_DEFLATED, 15, 8, 5),
        (DEFAULT_LEVEL, Z_DEFLATED, 15, 8, -1),
    ] {
        let mut ds = zeroed_stream();
        // SAFETY: `ds` is a valid owned `z_stream` and `good`/`size` are the
        // version pair the shim expects, so only the parameters under test can
        // fail; no buffer is allocated on the rejection path.
        let ret = unsafe {
            deflateInit2_(
                &mut ds,
                level,
                method,
                window_bits,
                mem_level,
                strategy,
                good,
                size,
            )
        };
        assert_eq!(
            ret, Z_STREAM_ERROR,
            "deflateInit2_ must reject level={level} method={method} \
             windowBits={window_bits} memLevel={mem_level} strategy={strategy}"
        );
        // SAFETY: `ds` is a valid `z_stream` whose `state` must still be null.
        let kind = unsafe { peek_handle_kind(&ds) };
        assert!(
            kind.is_none(),
            "a rejected deflateInit2_ must install no handle"
        );
    }

    // Inflate rejects only these four `windowBits`.
    for window_bits in [7, -7, -16, 48] {
        let mut is = zeroed_stream();
        // SAFETY: `is` is a valid owned `z_stream` with the expected version
        // pair, so only `windowBits` can fail the call.
        let ret = unsafe { inflateInit2_(&mut is, window_bits, good, size) };
        assert_eq!(
            ret, Z_STREAM_ERROR,
            "inflateInit2_ must reject windowBits={window_bits}"
        );
        // SAFETY: `is` is a valid `z_stream` whose `state` must still be null.
        let kind = unsafe { peek_handle_kind(&is) };
        assert!(
            kind.is_none(),
            "a rejected inflateInit2_ must install no handle"
        );
    }

    // `inflateBackInit_` is raw-only, so it narrows to `8..=15` and additionally
    // requires a caller-supplied window.
    for window_bits in [0, 7, 16, 48, -9] {
        let mut is = zeroed_stream();
        let win = window.as_mut_ptr();
        // SAFETY: `win` addresses a live 512-byte buffer that outlives the call.
        // Every `window_bits` in this row is outside `8..=15`, and
        // `inflateBackInit_` documents that its `1 << windowBits` size obligation
        // binds only on the accepting path — so probing the rejected range with a
        // buffer this small is explicitly in contract.
        let ret = unsafe { inflateBackInit_(&mut is, window_bits, win, good, size) };
        assert_eq!(
            ret, Z_STREAM_ERROR,
            "inflateBackInit_ must reject windowBits={window_bits}"
        );
    }
    let mut is = zeroed_stream();
    // SAFETY: a null `window` is a defined, rejected argument — the shim checks
    // the pointer rather than dereferencing it, which `inflateBackInit_`'s own
    // `# Safety` section states outright.
    let ret = unsafe { inflateBackInit_(&mut is, 9, ptr::null_mut(), good, size) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateBackInit_ must reject a null window"
    );
    // SAFETY: `is` is a valid `z_stream` whose `state` must still be null.
    let kind = unsafe { peek_handle_kind(&is) };
    assert!(
        kind.is_none(),
        "a rejected inflateBackInit_ must install no handle"
    );
}

/// Drives every streaming and terminating entry point with a null `z_streamp`,
/// and then with a valid stream that carries no engine state at all.
///
/// The second half is the interesting one: a zeroed `z_stream` has a null
/// `state`, which is also exactly what a stream looks like *after* its `*End`.
/// Both must be rejected rather than reinterpreted, which makes a double `*End`
/// a defined error instead of a double free.
fn probe_null_and_stateless_calls() {
    let no_stream = ptr::null_mut::<z_stream>();

    // Each call gets its own `unsafe` expression and its own proof. One block
    // wrapping five calls would attach a single `// SAFETY:` to five distinct
    // operations, which is the failure `clippy::multiple_unsafe_ops_per_block`
    // names; and a comment above an `assert_eq!` documents the macro rather than
    // the `unsafe` expression inside it, which is what
    // `clippy::undocumented_unsafe_blocks` names. Binding the result first fixes
    // both at once.
    //
    // SAFETY: `deflate` tests its `z_streamp` for null before any dereference, so
    // a null argument is a defined, enforced rejection rather than a violated
    // obligation (`zlib_rs::ffi::deflate`, "Defined rejections").
    let ret = unsafe { deflate(no_stream, Z_FINISH) };
    assert_eq!(ret, Z_STREAM_ERROR, "deflate must reject a null stream");
    // SAFETY: as above, for `inflate` (`zlib_rs::ffi::inflate`).
    let ret = unsafe { inflate(no_stream, Z_FINISH) };
    assert_eq!(ret, Z_STREAM_ERROR, "inflate must reject a null stream");
    // SAFETY: as above, for `deflateEnd`.
    let ret = unsafe { deflateEnd(no_stream) };
    assert_eq!(ret, Z_STREAM_ERROR, "deflateEnd must reject a null stream");
    // SAFETY: as above, for `inflateEnd`.
    let ret = unsafe { inflateEnd(no_stream) };
    assert_eq!(ret, Z_STREAM_ERROR, "inflateEnd must reject a null stream");
    // SAFETY: as above, for `inflateBackEnd`.
    let ret = unsafe { inflateBackEnd(no_stream) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateBackEnd must reject a null stream"
    );

    let mut stateless = zeroed_stream();
    let mut out = [0u8; 32];
    stateless.next_out = out.as_mut_ptr();
    stateless.avail_out = out.len() as c_uint;
    // SAFETY: `stateless` is a valid `z_stream` whose `state` is null, so the
    // tag read reports no handle without dereferencing anything.
    let kind = unsafe { peek_handle_kind(&stateless) };
    assert!(
        kind.is_none(),
        "a never-initialised stream carries no handle tag"
    );
    // A zeroed stream and a stream whose `*End` has already run are the same
    // shape, which is why both appear in the FFI modules' "Defined rejections"
    // list: each shim finds a null `state` and answers `Z_STREAM_ERROR` without
    // reinterpreting the pointer, so a double `*End` is a defined error rather
    // than a double free.
    //
    // SAFETY: `stateless` is a valid `z_stream` with a live output window, an
    // empty input window and a null `state`.
    let ret = unsafe { deflate(&mut stateless, Z_NO_FLUSH) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "deflate must reject a stream with no engine state"
    );
    // SAFETY: as above, for `inflate`.
    let ret = unsafe { inflate(&mut stateless, Z_NO_FLUSH) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflate must reject a stream with no engine state"
    );
    // SAFETY: as above, for `deflateEnd`.
    let ret = unsafe { deflateEnd(&mut stateless) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "deflateEnd must reject a stream with no engine state"
    );
    // SAFETY: as above, for `inflateEnd`.
    let ret = unsafe { inflateEnd(&mut stateless) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateEnd must reject a stream with no engine state"
    );
    // SAFETY: as above, for `inflateBackEnd`.
    let ret = unsafe { inflateBackEnd(&mut stateless) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateBackEnd must reject a stream with no engine state"
    );
}

/// Drives the `next_*`/`avail_*` entry validation shared by `deflate` and
/// `inflate`.
///
/// Both handles are built from in-range parameters with a valid version pair, so
/// initialisation is *required* to succeed rather than used as a gate: wrapping
/// the body in `if init == Z_OK` — as this probe used to — means a regression
/// that broke valid `deflateInit2_`/`inflateInit2_` would skip every assertion
/// below and report a clean run. Every outcome here is likewise pinned to its
/// exact code, including the two that are easy to leave open: the first
/// productive `deflate` call publishes the RFC 1950 header and returns `Z_OK`,
/// and `deflateEnd` on the abandoned stream reports `Z_DATA_ERROR`.
///
/// Deliberate omission: this probe never overstates an `avail_*` count relative
/// to a real allocation and never passes a freed or misaligned pointer. The C
/// API cannot defend against either, so such a call would be a defect here
/// rather than a finding about the library. The reachable, *defined* violations
/// are a null `next_out` — rejected unconditionally, with no `avail_out`
/// qualifier — and a positive `avail_in` paired with a null `next_in`.
fn probe_buffer_validation() {
    let good = version_ok();
    let size = stream_size_ok();
    let mut out = [0u8; 64];

    let mut ds = zeroed_stream();
    // SAFETY: `ds` is a valid owned `z_stream`; a 512-byte window and the
    // smallest `memLevel` keep the handle cheap enough to build every execution.
    assert_exact_code(
        unsafe { deflateInit2_(&mut ds, DEFAULT_LEVEL, Z_DEFLATED, 9, 1, 0, good, size) },
        Contract::DeflateInit,
        Z_OK,
        "deflateInit2_(windowBits = 9, memLevel = 1) for the deflate buffer probes",
    );
    ds.next_out = ptr::null_mut();
    ds.avail_out = 0;
    // SAFETY: a null `next_out` is a defined, rejected configuration — the shim
    // tests the pointer before bridging it to a slice.
    assert_exact_code(
        unsafe { deflate(&mut ds, Z_NO_FLUSH) },
        Contract::Deflate,
        Z_STREAM_ERROR,
        "deflate given a null next_out",
    );

    ds.next_out = out.as_mut_ptr();
    ds.avail_out = out.len() as c_uint;
    ds.next_in = ptr::null();
    ds.avail_in = 7;
    // SAFETY: a positive `avail_in` with a null `next_in` is likewise rejected
    // before any read; nothing is dereferenced.
    assert_exact_code(
        unsafe { deflate(&mut ds, Z_NO_FLUSH) },
        Contract::Deflate,
        Z_STREAM_ERROR,
        "deflate given a positive avail_in with a null next_in",
    );

    // `avail_in == 0` with a null `next_in` is legal: nothing is read. The
    // outcome is not open either, and it used to be left that way. Both
    // rejections above are refused in the prologue, so `last_flush` still holds
    // its post-reset sentinel and C's `RANK(flush) <= RANK(old_flush)` guard does
    // not fire; the stream is zlib-wrapped and still in `INIT_STATE`, so this
    // call publishes the two-byte RFC 1950 header and returns `Z_OK`.
    ds.avail_in = 0;
    // SAFETY: an empty input window with a live output window is a valid,
    // documented call shape.
    assert_exact_code(
        unsafe { deflate(&mut ds, Z_NO_FLUSH) },
        Contract::Deflate,
        Z_OK,
        "deflate with an empty input window on a freshly initialised stream",
    );
    let header_len = out.len() - ds.avail_out as usize;
    assert_eq!(
        &out[..header_len],
        &ZLIB_HEADER_W9_L6[..],
        "the first productive deflate call must publish exactly the RFC 1950 \
         header for windowBits = 9 at level 6"
    );

    // Repeating it is where `RANK(flush) <= RANK(old_flush)` bites: `last_flush`
    // is now `Z_NO_FLUSH`, the input window is still empty and the pending buffer
    // has been drained, so no progress is possible and C answers `Z_BUF_ERROR`.
    // That transition is the entire purpose of the guard, so it is pinned rather
    // than folded into a set of acceptable outcomes.
    // SAFETY: as above; the output window is still live and `avail_out > 0`.
    assert_exact_code(
        unsafe { deflate(&mut ds, Z_NO_FLUSH) },
        Contract::Deflate,
        Z_BUF_ERROR,
        "a repeated no-progress deflate with an empty input window",
    );

    // Teardown. The stream is still mid-block, so C's `deflateEnd` reports
    // `Z_DATA_ERROR` here rather than `Z_OK`. That is pinned too: accepting
    // either would hide a regression that stopped reporting discarded input at
    // all. The handle must be reclaimed regardless of the code.
    // SAFETY: `ds` holds a live deflate handle installed above.
    assert_exact_code(
        unsafe { deflateEnd(&mut ds) },
        Contract::DeflateEnd,
        Z_DATA_ERROR,
        "deflateEnd for a stream abandoned mid-block",
    );
    // SAFETY: `ds` is a valid `z_stream`; its `state` must now be null.
    let kind = unsafe { peek_handle_kind(&ds) };
    assert!(
        kind.is_none(),
        "deflateEnd must reclaim the handle even when it reports Z_DATA_ERROR"
    );

    let mut is = zeroed_stream();
    // SAFETY: `is` is a valid owned `z_stream` with the expected version pair.
    assert_exact_code(
        unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_(raw, windowBits = -9) for the inflate buffer probes",
    );
    is.next_out = ptr::null_mut();
    is.avail_out = 0;
    // SAFETY: a null `next_out` is a defined, rejected configuration.
    assert_exact_code(
        unsafe { inflate(&mut is, Z_NO_FLUSH) },
        Contract::Inflate,
        Z_STREAM_ERROR,
        "inflate given a null next_out",
    );

    is.next_out = out.as_mut_ptr();
    is.avail_out = out.len() as c_uint;
    is.next_in = ptr::null();
    is.avail_in = 7;
    // SAFETY: a positive `avail_in` with a null `next_in` is rejected before any
    // read.
    assert_exact_code(
        unsafe { inflate(&mut is, Z_NO_FLUSH) },
        Contract::Inflate,
        Z_STREAM_ERROR,
        "inflate given a positive avail_in with a null next_in",
    );

    // Unlike deflate, inflate has no header of its own to publish for a raw
    // stream, so an empty input window leaves it with nothing at all to do:
    // `Z_BUF_ERROR` on the very first call, not `Z_OK`.
    is.avail_in = 0;
    // SAFETY: an empty input window with a live output window is valid.
    assert_exact_code(
        unsafe { inflate(&mut is, Z_NO_FLUSH) },
        Contract::Inflate,
        Z_BUF_ERROR,
        "inflate with an empty input window on a raw stream",
    );
    // `inflateEnd` has no `Z_DATA_ERROR` case, so an untouched raw handle tears
    // down cleanly.
    // SAFETY: `is` holds a live inflate handle installed above.
    assert_exact_code(
        unsafe { inflateEnd(&mut is) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the inflate buffer probes",
    );
    // SAFETY: `is` is a valid `z_stream`; its `state` must now be null.
    let kind = unsafe { peek_handle_kind(&is) };
    assert!(kind.is_none(), "inflateEnd must reclaim the handle");
}

/// Drives `inflateSync`, the one entry point here whose outcome the C contract
/// genuinely leaves open, alongside the three shapes where it does not.
///
/// `inflateSync` is the reason a per-entry-point contract table earns its keep
/// rather than being a longer way to write an exact assertion. Scanning
/// fuzzer-chosen bytes for the `00 00 FF FF` stored-block marker may legitimately
/// find one (`Z_OK`), run out of input first (`Z_DATA_ERROR`), or have nothing to
/// scan at all (`Z_BUF_ERROR`) — but it can never report `Z_MEM_ERROR`,
/// `Z_NEED_DICT`, `Z_STREAM_END`, `Z_ERRNO` or `Z_VERSION_ERROR`, all of which a
/// blanket "one of the nine" predicate accepts. The open case therefore uses
/// [`assert_contract_code`], and the three closed ones are pinned exactly:
/// a null `z_streamp` and a null-`state` stream are `Z_STREAM_ERROR`, an empty
/// input window with an empty bit accumulator is `Z_BUF_ERROR`, and a marker
/// planted at a known offset is `Z_OK` with an exact input-consumption count.
fn probe_sync_recovery(data: &[u8]) {
    let good = version_ok();
    let size = stream_size_ok();
    let mut out = [0u8; 64];

    // A null `z_streamp`, and a valid stream carrying no engine state, are both
    // rejected without a dereference — the same null-`state` shape a stream has
    // after its `inflateEnd`.
    // SAFETY: `inflateSync` checks its pointer before any dereference.
    assert_exact_code(
        unsafe { inflateSync(ptr::null_mut()) },
        Contract::InflateSync,
        Z_STREAM_ERROR,
        "inflateSync given a null stream",
    );
    let mut stateless = zeroed_stream();
    // SAFETY: `stateless` is a valid `z_stream` whose `state` is null.
    assert_exact_code(
        unsafe { inflateSync(&mut stateless) },
        Contract::InflateSync,
        Z_STREAM_ERROR,
        "inflateSync given a stream with no engine state",
    );

    // A live handle with nothing to scan: C rejects before searching, because
    // neither the input window nor the bit accumulator holds a byte.
    let mut empty = zeroed_stream();
    // SAFETY: `empty` is a valid owned `z_stream` with the expected version pair.
    assert_exact_code(
        unsafe { inflateInit2_(&mut empty, WBITS_ZLIB, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_(zlib) for the empty-input sync probe",
    );
    empty.next_out = out.as_mut_ptr();
    empty.avail_out = out.len() as c_uint;
    // SAFETY: `next_in` is null with `avail_in == 0`, which is the documented
    // empty-input window; nothing is read.
    assert_exact_code(
        unsafe { inflateSync(&mut empty) },
        Contract::InflateSync,
        Z_BUF_ERROR,
        "inflateSync with an empty input window and an empty bit accumulator",
    );
    // SAFETY: `empty` holds a live inflate handle.
    assert_exact_code(
        unsafe { inflateEnd(&mut empty) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the empty-input sync probe",
    );

    // The open case: fuzzer-chosen bytes, bounded so the scan stays cheap.
    let scanned = &data[..data.len().min(MAX_SYNC_SCAN)];
    let mut junk = zeroed_stream();
    // SAFETY: `junk` is a valid owned `z_stream` with the expected version pair.
    assert_exact_code(
        unsafe { inflateInit2_(&mut junk, WBITS_ZLIB, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_(zlib) for the arbitrary-input sync probe",
    );
    junk.next_in = scanned.as_ptr();
    junk.avail_in = scanned.len() as c_uint;
    junk.next_out = out.as_mut_ptr();
    junk.avail_out = out.len() as c_uint;
    // SAFETY: `next_in` addresses `scanned` with a matching `avail_in`.
    let scan = unsafe { inflateSync(&mut junk) };
    assert_contract_code(scan, Contract::InflateSync, "inflateSync over fuzz input");
    // Whatever the verdict, the scan may only ever have consumed input it was
    // given: a count that grew past `avail_in` would be a pointer-arithmetic bug
    // at the boundary, which is exactly what this target exists to catch.
    assert!(
        junk.avail_in as usize <= scanned.len(),
        "inflateSync reported {} bytes still available from a {}-byte window",
        junk.avail_in,
        scanned.len()
    );
    // SAFETY: `junk` holds a live inflate handle.
    assert_exact_code(
        unsafe { inflateEnd(&mut junk) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the arbitrary-input sync probe",
    );

    // The closed case: a marker planted at a known offset. C consumes everything
    // up to and including the four marker bytes and reports `Z_OK`, leaving the
    // remainder for a restarted `inflate`.
    let mut framed = zeroed_stream();
    // SAFETY: `framed` is a valid owned `z_stream` with the expected version pair.
    assert_exact_code(
        unsafe { inflateInit2_(&mut framed, WBITS_ZLIB, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_(zlib) for the planted-marker sync probe",
    );
    framed.next_in = SYNC_FRAMED.as_ptr();
    framed.avail_in = SYNC_FRAMED.len() as c_uint;
    framed.next_out = out.as_mut_ptr();
    framed.avail_out = out.len() as c_uint;
    // SAFETY: `next_in` addresses the static `SYNC_FRAMED` bytes with a matching
    // `avail_in`, and `next_out` addresses the live `out` buffer.
    assert_exact_code(
        unsafe { inflateSync(&mut framed) },
        Contract::InflateSync,
        Z_OK,
        "inflateSync over a stream carrying a 00 00 FF FF marker",
    );
    assert_eq!(
        framed.avail_in as usize,
        SYNC_FRAMED.len() - SYNC_MARKER_END,
        "inflateSync must consume exactly the {SYNC_MARKER_END} bytes up to and \
         including the marker"
    );
    // C sets up to restart `inflate` on a new block after a successful sync. What
    // that restarted call answers depends on the bytes that follow the marker, so
    // this is a contract-membership check, not an exact one.
    // SAFETY: both windows are still the live buffers configured above.
    assert_contract_code(
        unsafe { inflate(&mut framed, Z_NO_FLUSH) },
        Contract::Inflate,
        "inflate restarted after a successful inflateSync",
    );
    // SAFETY: `framed` holds a live inflate handle.
    assert_exact_code(
        unsafe { inflateEnd(&mut framed) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the planted-marker sync probe",
    );
}

// ===========================================================================
// N2 — handle tagging and cross-engine `*End` misuse
// ===========================================================================

/// Confirms each `*Init*_` installs its own `HandleKind`, that the two foreign
/// terminators reject the handle without touching it, that the matching one then
/// still succeeds, and that a second call is rejected.
///
/// This is the highest-value assertion in the file: in C, freeing a deflate
/// state through `inflateEnd` is a layout-mismatched free — undefined behaviour
/// the API cannot detect. The offset-0 tag turns it into `Z_STREAM_ERROR` with
/// the handle left intact, which is what the follow-up `*End` proves.
///
/// `window_bits` is steered by the fuzz input so successive executions build
/// handles of different geometry; every value passed in is a legal one, so all
/// three `*Init*_` calls are *required* to succeed. They used to gate their own
/// blocks, which meant a regression that broke any one of them would take this —
/// the file's most valuable probe — offline without a single assertion firing.
fn probe_handle_tag_misuse(window_bits: c_int) {
    let good = version_ok();
    let size = stream_size_ok();

    // ---- A deflate handle rejects both foreign terminators. ----
    let mut ds = zeroed_stream();
    // SAFETY: `ds` is a valid owned `z_stream`; `window_bits` is one of the
    // legal deflate values chosen by the caller and `memLevel = 1` keeps the
    // handle small.
    assert_exact_code(
        unsafe {
            deflateInit2_(
                &mut ds,
                DEFAULT_LEVEL,
                Z_DEFLATED,
                window_bits,
                1,
                0,
                good,
                size,
            )
        },
        Contract::DeflateInit,
        Z_OK,
        "deflateInit2_ for the handle-tag probe",
    );
    // SAFETY: `ds.state` is a live tagged handle from `deflateInit2_`.
    let kind = unsafe { peek_handle_kind(&ds) };
    assert_eq!(
        kind,
        Some(HandleKind::DEFLATE),
        "deflateInit2_ must install a DEFLATE-tagged handle"
    );
    // SAFETY: both terminators validate the offset-0 tag before reclaiming,
    // so they reject this deflate handle without dropping a wrong-type box.
    assert_exact_code(
        unsafe { inflateEnd(&mut ds) },
        Contract::InflateEnd,
        Z_STREAM_ERROR,
        "inflateEnd handed a deflate handle",
    );
    // SAFETY: as above, for the third terminator.
    assert_exact_code(
        unsafe { inflateBackEnd(&mut ds) },
        Contract::InflateBackEnd,
        Z_STREAM_ERROR,
        "inflateBackEnd handed a deflate handle",
    );
    // SAFETY: the rejected calls must have left `state` untouched.
    let kind = unsafe { peek_handle_kind(&ds) };
    assert_eq!(
        kind,
        Some(HandleKind::DEFLATE),
        "a rejected cross-engine End must leave the handle intact"
    );
    // SAFETY: `ds` still holds its live deflate handle.
    assert_exact_code(
        unsafe { deflateEnd(&mut ds) },
        Contract::DeflateEnd,
        Z_OK,
        "deflateEnd after the rejected cross-engine calls",
    );
    // SAFETY: `state` is null once reclaimed, so the tag read finds nothing.
    let kind = unsafe { peek_handle_kind(&ds) };
    assert!(kind.is_none(), "deflateEnd must clear the state handle");
    // SAFETY: the handle is already gone, so this is the double-`*End` case:
    // a null `state` is rejected rather than freed a second time.
    assert_exact_code(
        unsafe { deflateEnd(&mut ds) },
        Contract::DeflateEnd,
        Z_STREAM_ERROR,
        "a second deflateEnd on an already-reclaimed stream",
    );

    // ---- The mirror case: an inflate handle rejects the other two. ----
    let mut is = zeroed_stream();
    // SAFETY: `is` is a valid owned `z_stream` with the expected version pair.
    assert_exact_code(
        unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_(raw) for the handle-tag probe",
    );
    // SAFETY: `is.state` is a live tagged handle from `inflateInit2_`.
    let kind = unsafe { peek_handle_kind(&is) };
    assert_eq!(
        kind,
        Some(HandleKind::INFLATE),
        "inflateInit2_ must install an INFLATE-tagged handle"
    );
    // SAFETY: tag-validating terminators; neither may reclaim this handle.
    assert_exact_code(
        unsafe { deflateEnd(&mut is) },
        Contract::DeflateEnd,
        Z_STREAM_ERROR,
        "deflateEnd handed an inflate handle",
    );
    // SAFETY: as above, for the third terminator.
    assert_exact_code(
        unsafe { inflateBackEnd(&mut is) },
        Contract::InflateBackEnd,
        Z_STREAM_ERROR,
        "inflateBackEnd handed an inflate handle",
    );
    // SAFETY: the rejected calls must have left `state` untouched.
    let kind = unsafe { peek_handle_kind(&is) };
    assert_eq!(
        kind,
        Some(HandleKind::INFLATE),
        "a rejected cross-engine End must leave the inflate handle intact"
    );
    // SAFETY: `is` still holds its live inflate handle.
    assert_exact_code(
        unsafe { inflateEnd(&mut is) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the rejected cross-engine calls",
    );
    // SAFETY: `state` is null once reclaimed.
    let kind = unsafe { peek_handle_kind(&is) };
    assert!(kind.is_none(), "inflateEnd must clear the state handle");
    // SAFETY: the double-`*End` case for inflate.
    assert_exact_code(
        unsafe { inflateEnd(&mut is) },
        Contract::InflateEnd,
        Z_STREAM_ERROR,
        "a second inflateEnd on an already-reclaimed stream",
    );

    // ---- `inflateBack` carries a third, distinct tag. ----
    // Its window is caller-supplied, so the buffer must stay live for the whole
    // handle lifetime; `window` outlives every call below.
    let mut window = [0u8; 1 << 9];
    let mut ib = zeroed_stream();
    // SAFETY: `ib` is a valid owned `z_stream` and `window` addresses exactly
    // `1 << 9` live bytes, matching `windowBits = 9`, for the handle's lifetime.
    assert_exact_code(
        unsafe { inflateBackInit_(&mut ib, 9, window.as_mut_ptr(), good, size) },
        Contract::InflateBackInit,
        Z_OK,
        "inflateBackInit_(windowBits = 9) with a caller-supplied window",
    );
    // SAFETY: `ib.state` is a live tagged handle from `inflateBackInit_`.
    let kind = unsafe { peek_handle_kind(&ib) };
    assert_eq!(
        kind,
        Some(HandleKind::INFLATE_BACK),
        "inflateBackInit_ must install an INFLATE_BACK-tagged handle"
    );
    // SAFETY: `INFLATE_BACK` is deliberately distinct from `INFLATE`, so the
    // plain inflate terminator must reject it too — freeing it as a plain
    // inflate handle would be the layout-mismatched free this tag prevents.
    assert_exact_code(
        unsafe { inflateEnd(&mut ib) },
        Contract::InflateEnd,
        Z_STREAM_ERROR,
        "inflateEnd handed an inflateBack handle",
    );
    // SAFETY: as above, for the deflate terminator.
    assert_exact_code(
        unsafe { deflateEnd(&mut ib) },
        Contract::DeflateEnd,
        Z_STREAM_ERROR,
        "deflateEnd handed an inflateBack handle",
    );
    // SAFETY: `ib` still holds its live inflateBack handle.
    assert_exact_code(
        unsafe { inflateBackEnd(&mut ib) },
        Contract::InflateBackEnd,
        Z_OK,
        "inflateBackEnd after the rejected cross-engine calls",
    );
    // SAFETY: `state` is null once reclaimed.
    let kind = unsafe { peek_handle_kind(&ib) };
    assert!(kind.is_none(), "inflateBackEnd must clear the state handle");
}

// ===========================================================================
// N3 — the allocator-hook contract and allocation-failure timing
// ===========================================================================

/// Confirms that a hookless caller gets C's built-in substitution published back
/// onto the stream, and that an active hook is genuinely the engine's allocator,
/// with every block returned through `zfree`.
///
/// All three streams here are given an unlimited budget (`usize::MAX`), so an
/// allocation refusal is not a possible outcome and every `*Init*_` is asserted to
/// return `Z_OK` rather than gating its own accounting checks. The starved
/// counterpart lives in [`probe_allocator_failure_timing`], which is where a
/// refusal *is* the expected result.
fn probe_allocator_balance(window_bits: c_int) {
    let good = version_ok();
    let size = stream_size_ok();

    // With no hooks the `*Init*_` prologue publishes its own built-in pair and
    // clears `opaque`, exactly as C substitutes `zcalloc`/`zcfree` and zeroes
    // `opaque` on the `zalloc` branch. A C caller inspecting the stream
    // afterwards must see that same non-null shape.
    let mut hookless = zeroed_stream();
    // SAFETY: `hookless` is a valid owned `z_stream` with null hooks, which is
    // the ordinary hookless call shape.
    assert_exact_code(
        unsafe { deflateInit_(&mut hookless, DEFAULT_LEVEL, good, size) },
        Contract::DeflateInit,
        Z_OK,
        "deflateInit_ with no caller hooks installed",
    );
    assert!(
        hookless.zalloc.is_some(),
        "a hookless init must publish a built-in zalloc"
    );
    assert!(
        hookless.zfree.is_some(),
        "a hookless init must publish a built-in zfree"
    );
    assert!(
        hookless.opaque.is_null(),
        "substituting zalloc must clear opaque"
    );
    // SAFETY: `hookless` holds a live deflate handle.
    assert_exact_code(
        unsafe { deflateEnd(&mut hookless) },
        Contract::DeflateEnd,
        Z_OK,
        "deflateEnd for an untouched hookless stream",
    );

    // With both hooks installed the engine must carve its buffers from caller
    // memory and hand every block back at `*End`.
    let hooks = HookState::with_budget(usize::MAX);
    let mut ds = zeroed_stream();
    install_hooks(&mut ds, &hooks);
    // SAFETY: `ds` is a valid owned `z_stream` whose `opaque` addresses `hooks`,
    // which outlives every call the engine makes through the hooks below.
    assert_exact_code(
        unsafe {
            deflateInit2_(
                &mut ds,
                DEFAULT_LEVEL,
                Z_DEFLATED,
                window_bits,
                1,
                0,
                good,
                size,
            )
        },
        Contract::DeflateInit,
        Z_OK,
        "deflateInit2_ backed by an unlimited caller hook",
    );
    assert_eq!(
        hooks.handed_out.get(),
        DEFLATE_INIT_ALLOCATIONS,
        "deflateInit2_ must charge exactly C's allocation count to the caller"
    );
    // SAFETY: `ds` holds a live deflate handle backed by caller memory.
    assert_exact_code(
        unsafe { deflateEnd(&mut ds) },
        Contract::DeflateEnd,
        Z_OK,
        "deflateEnd for an untouched hook-backed deflate stream",
    );
    hooks.assert_balanced("deflate with an active allocator hook");

    let inflate_hooks = HookState::with_budget(usize::MAX);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &inflate_hooks);
    // SAFETY: as above, for the inflate engine.
    assert_exact_code(
        unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_ backed by an unlimited caller hook",
    );
    assert_eq!(
        inflate_hooks.handed_out.get(),
        INFLATE_INIT_ALLOCATIONS,
        "inflateInit2_ must charge exactly C's init allocation count, \
         leaving the window to be allocated lazily"
    );
    // SAFETY: `is` holds a live inflate handle backed by caller memory.
    assert_exact_code(
        unsafe { inflateEnd(&mut is) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd for an untouched hook-backed inflate stream",
    );
    inflate_hooks.assert_balanced("inflate with an active allocator hook");
}

/// Confirms that a hook reporting out-of-memory surfaces `Z_MEM_ERROR` — never a
/// silent fall back to the global allocator — and, crucially, that it surfaces at
/// the *same call* C would report it from.
///
/// Deflate requests every buffer during init, so any shortfall fails at
/// `deflateInit2_`. Inflate charges only its state object at init and defers the
/// sliding window, so a budget of exactly [`INFLATE_INIT_ALLOCATIONS`]
/// initialises cleanly and fails later, inside `inflate`. Shifting either way
/// would be an observable behaviour change even though no compressed byte
/// differs.
///
/// The deferred half is only a test if the window request actually happens, and
/// that is not a property of merely producing output: C's `inf_leave` skips
/// `updatewindow` once the decode has run past `CHECK` on a `Z_FINISH` request, so
/// a decode that finishes in one call can leave the window untouched. The probe
/// therefore *forces* the request with a fixed-length payload and a one-byte
/// output window — a first call that cannot finish, with output produced and the
/// mode still short of `CHECK` — and then asserts the refusal count rather than
/// branching on it. Branching was the flaw: `refused == 0` fell through to a
/// membership check, so an execution that never reached the deferred allocation
/// passed just as happily as one that did.
fn probe_allocator_failure_timing(data: &[u8]) {
    let good = version_ok();
    let size = stream_size_ok();

    // A budget short of the full deflate set — including the empty budget, which
    // fails on C's very first `ZALLOC` — must fail at init, not later. Rotating
    // the shortfall from the fuzz input walks the whole request sequence and
    // exercises the release of the buffers that had already succeeded.
    let shortfall = usize::from(data.first().copied().unwrap_or(0)) % DEFLATE_INIT_ALLOCATIONS;
    let starved = HookState::with_budget(shortfall);
    let mut ds = zeroed_stream();
    install_hooks(&mut ds, &starved);
    // SAFETY: `ds` is a valid owned `z_stream` whose `opaque` addresses
    // `starved`, which outlives the call; the hook returns null once its budget
    // is spent, which is how a C caller reports out-of-memory.
    let ret = unsafe { deflateInit2_(&mut ds, DEFAULT_LEVEL, Z_DEFLATED, 9, 1, 0, good, size) };
    assert_eq!(
        ret, Z_MEM_ERROR,
        "a deflate allocation shortfall of {shortfall}/{DEFLATE_INIT_ALLOCATIONS} \
         must surface Z_MEM_ERROR from deflateInit2_ itself"
    );
    // SAFETY: `ds` is a valid `z_stream`; a failed init must leave no handle, so
    // there is nothing to tear down and nothing to leak.
    let kind = unsafe { peek_handle_kind(&ds) };
    assert!(
        kind.is_none(),
        "a failed deflateInit2_ must install no handle"
    );
    starved.assert_balanced("deflate init starved of caller memory");

    // The same shortfall applied to inflate: an empty budget fails at init.
    let starved_init = HookState::with_budget(0);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &starved_init);
    // SAFETY: `is` is a live, exclusively borrowed `z_stream` whose hooks were
    // just installed, so `opaque` points at `starved_init`, which outlives this
    // call; that hook has a zero allocation budget and therefore returns null,
    // which is how a C caller reports out-of-memory. `good`/`size` are the
    // readable static version literal and `size_of::<z_stream>()`, and
    // `RAW_WINDOW_BITS` is a valid raw `windowBits`.
    assert_eq!(
        ret, Z_MEM_ERROR,
        "an inflate state allocation that cannot be satisfied must surface \
         Z_MEM_ERROR from inflateInit2_"
    );
    // SAFETY: `is` is a valid `z_stream` that must carry no handle.
    let kind = unsafe { peek_handle_kind(&is) };
    assert!(
        kind.is_none(),
        "a failed inflateInit2_ must install no handle"
    );
    starved_init.assert_balanced("inflate init starved of caller memory");

    // Build a raw stream whose decode is *forced* to need the lazily allocated
    // window. Producing output is not on its own enough: C's `inf_leave` skips
    // `updatewindow` when the decode has already run past `CHECK` on a `Z_FINISH`
    // request, so a decode that happens to finish in one call can leave the
    // window untouched. What makes the request unavoidable is a first call that
    // *cannot* finish — [`WINDOW_PROBE_LEN`] payload bytes decoded into a
    // one-byte output window stop with output produced and the mode still short
    // of `CHECK`, which is exactly the condition C tests.
    //
    // The payload's length is fixed for that reason, and only its content comes
    // from the fuzz input (with a deterministic ramp filling any shortfall) so the
    // probe walks different block shapes while staying structurally forcing —
    // including on the empty input libFuzzer tries first.
    let mut payload = [0u8; WINDOW_PROBE_LEN];
    for (index, slot) in payload.iter_mut().enumerate() {
        *slot = data.get(index).copied().unwrap_or(index as u8);
    }
    let mut comp = [0u8; 512];
    let produced = raw_deflate(&payload, &mut comp);

    // Now the timing case that only inflate can show: a budget covering the
    // state object but nothing more initialises cleanly, and `inflate` is where
    // the failure appears.
    let starved_window = HookState::with_budget(INFLATE_INIT_ALLOCATIONS);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &starved_window);
    // SAFETY: `is` is a valid owned `z_stream` whose `opaque` addresses
    // `starved_window`, which outlives every call below.
    assert_exact_code(
        unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) },
        Contract::InflateInit,
        Z_OK,
        "inflateInit2_ with a budget of exactly INFLATE_INIT_ALLOCATIONS",
    );
    assert_eq!(
        starved_window.handed_out.get(),
        INFLATE_INIT_ALLOCATIONS,
        "the inflate window must not be charged at init"
    );
    assert_eq!(
        starved_window.refused.get(),
        0,
        "an init within budget must not have had a request refused"
    );
    // One byte of output room: far less than the payload decodes to, so this call
    // cannot reach the end of the stream.
    let mut back = [0u8; 1];
    is.next_in = comp.as_ptr();
    is.avail_in = produced as c_uint;
    is.next_out = back.as_mut_ptr();
    is.avail_out = back.len() as c_uint;
    // SAFETY: `next_in`/`next_out` address the live `comp`/`back` buffers with
    // matching `avail_*` counts, and `produced <= comp.len()`.
    let ret = unsafe { inflate(&mut is, Z_NO_FLUSH) };
    // The window request is now mandatory, so its refusal is asserted rather than
    // tested for. Accepting `refused == 0` — as this probe used to — meant a
    // decode that never reached the deferred allocation still passed, which made
    // the whole timing assertion below optional.
    assert_eq!(
        starved_window.refused.get(),
        1,
        "the first inflate call must have asked the exhausted hook for the \
         sliding window exactly once (payload {WINDOW_PROBE_LEN} bytes, stream \
         {produced} bytes, one byte of output room, inflate returned {ret})"
    );
    assert_eq!(
        starved_window.handed_out.get(),
        INFLATE_INIT_ALLOCATIONS,
        "a refused window must not have been handed out anyway"
    );
    // And the refusal must surface as `Z_MEM_ERROR` from `inflate` — not from
    // `inflateInit2_`, which already returned `Z_OK` above. That two-call split is
    // the failure *timing* a C caller observes.
    assert_exact_code(
        ret,
        Contract::Inflate,
        Z_MEM_ERROR,
        "inflate whose lazy sliding-window allocation the caller's hook refused",
    );
    // SAFETY: `is` holds a live inflate handle; reclaim it on every path.
    assert_exact_code(
        unsafe { inflateEnd(&mut is) },
        Contract::InflateEnd,
        Z_OK,
        "inflateEnd after the refused window allocation",
    );
    // SAFETY: `is` is a valid `z_stream`; its `state` must now be null.
    let kind = unsafe { peek_handle_kind(&is) };
    assert!(
        kind.is_none(),
        "inflateEnd must reclaim the handle after a window allocation failure"
    );
    starved_window.assert_balanced("inflate starved of its lazy window");
}

/// Raw-DEFLATE-compresses `payload` into `into` through the C ABI, returning the
/// number of bytes produced.
///
/// Every step is required to succeed, and nothing here is optional:
/// `deflateInit2_` gets an in-range configuration and a valid version pair, so
/// `Z_OK` is the only permitted answer; `into` is checked against the library's
/// own [`deflateBound`] for this handle and this payload, so one `Z_FINISH` must
/// reach `Z_STREAM_END`; the byte count must be positive; and `deflateEnd` must
/// report `Z_OK`, since a completed stream has nothing buffered to discard.
///
/// Returning [`Option`] instead — as this helper used to — made its sole caller,
/// the deferred-allocation probe, skip itself whenever any of that went wrong.
/// A regression in valid raw-deflate initialisation would therefore have silently
/// disabled the only test of inflate's allocation *timing*.
///
/// Runs on the global allocator so it cannot disturb the hook accounting of the
/// probe that calls it.
fn raw_deflate(payload: &[u8], into: &mut [u8]) -> usize {
    let mut ds = zeroed_stream();
    // SAFETY: `ds` is a valid owned `z_stream`; `RAW_WINDOW_BITS` and
    // `memLevel = 1` are a legal, deliberately small configuration.
    assert_exact_code(
        unsafe {
            deflateInit2_(
                &mut ds,
                DEFAULT_LEVEL,
                Z_DEFLATED,
                RAW_WINDOW_BITS,
                1,
                0,
                version_ok(),
                stream_size_ok(),
            )
        },
        Contract::DeflateInit,
        Z_OK,
        "deflateInit2_(raw, windowBits = -9, memLevel = 1) in raw_deflate",
    );
    // `deflateBound` reads the live handle's wrap and window geometry, so it is
    // queried after init and before the destination is committed to.
    // SAFETY: `ds` holds the live deflate handle installed above.
    let bound = unsafe { deflateBound(&mut ds, payload.len() as uLong) };
    assert!(
        into.len() as uLong >= bound,
        "raw_deflate was given {} bytes of destination but deflateBound requires \
         {bound} for a {}-byte payload",
        into.len(),
        payload.len()
    );
    ds.next_in = payload.as_ptr();
    ds.avail_in = payload.len() as c_uint;
    ds.next_out = into.as_mut_ptr();
    ds.avail_out = into.len() as c_uint;
    // SAFETY: `next_in`/`next_out` address the live `payload`/`into` slices with
    // matching `avail_*` counts.
    let ret = unsafe { deflate(&mut ds, Z_FINISH) };
    let produced = into.len() - ds.avail_out as usize;
    // SAFETY: `ds` holds a live deflate handle; reclaim it before asserting so a
    // diagnostic failure cannot also leak the handle.
    let end = unsafe { deflateEnd(&mut ds) };
    assert_exact_code(
        ret,
        Contract::Deflate,
        Z_STREAM_END,
        "one Z_FINISH into a deflateBound-sized buffer in raw_deflate",
    );
    assert!(
        produced > 0,
        "a completed raw deflate stream for a {}-byte payload cannot be empty",
        payload.len()
    );
    assert_exact_code(
        end,
        Contract::DeflateEnd,
        Z_OK,
        "deflateEnd after a completed Z_FINISH in raw_deflate",
    );
    produced
}

// ===========================================================================
// N4 — the copy / reset lifecycle
// ===========================================================================

/// Outcome of one bounded drive loop.
struct Driven {
    /// Bytes the engine took from the input slice.
    consumed: usize,
    /// Bytes the engine wrote into the output slice.
    produced: usize,
    /// The last code the engine returned.
    code: c_int,
}

/// Drives `strm` through the `extern "C"` [`deflate`] entry point over `input`,
/// writing into `out`, until the engine reports a terminal code, fills `out`, or
/// stops making progress.
///
/// The cursors are re-pointed at the caller's slices *first*, and that is what
/// makes this callable on a stream produced by [`deflateCopy`]: that shim mirrors
/// the whole observable `z_stream`, so a fresh copy's `next_in`/`next_out` still
/// alias the **source's** buffers until they are overwritten here. Driving a copy
/// without re-pointing it would have two engines writing through one pointer —
/// a defect in this harness rather than a finding about the library.
///
/// Safe for any `&mut z_stream`: a null or wrong-`kind` `state` is a defined
/// rejection inside the shim, never a dereference.
fn drive_deflate(strm: &mut z_stream, input: &[u8], out: &mut [u8], flush: c_int) -> Driven {
    strm.next_in = input.as_ptr();
    strm.avail_in = input.len() as c_uint;
    strm.next_out = out.as_mut_ptr();
    strm.avail_out = out.len() as c_uint;

    let mut code;
    let mut spins: usize = 0;
    loop {
        let before_in = strm.avail_in;
        let before_out = strm.avail_out;
        // SAFETY: `next_in`/`next_out` address the live `input`/`out` slices with
        // `avail_*` counts the engine only ever decreases, so both stay in bounds
        // across every spin; a null `state` is rejected, not dereferenced.
        code = unsafe { deflate(strm, flush) };
        spins += 1;

        // A call that moved neither cursor has stalled, and one that filled the
        // output slice cannot continue. Under `Z_NO_FLUSH` a drained input is also
        // the end of the road, because C then has nothing further to do.
        let stalled = strm.avail_in == before_in && strm.avail_out == before_out;
        if code != Z_OK
            || stalled
            || strm.avail_out == 0
            || spins >= DRIVE_GUARD
            || (flush == Z_NO_FLUSH && strm.avail_in == 0)
        {
            break;
        }
    }

    // The engine may only ever consume; asserting it keeps the subtractions below
    // honest instead of letting a saturating fallback hide an accounting bug.
    assert!(
        strm.avail_in as usize <= input.len() && strm.avail_out as usize <= out.len(),
        "deflate must never grow avail_in or avail_out"
    );
    Driven {
        consumed: input.len() - strm.avail_in as usize,
        produced: out.len() - strm.avail_out as usize,
        code,
    }
}

/// The [`inflate`] counterpart of [`drive_deflate`], with the one behavioural
/// difference the C API dictates: the decoder buffers no output of its own, so a
/// `Z_OK` with a drained input always means "needs more input" and ends the drive
/// regardless of the flush mode.
fn drive_inflate(strm: &mut z_stream, input: &[u8], out: &mut [u8], flush: c_int) -> Driven {
    strm.next_in = input.as_ptr();
    strm.avail_in = input.len() as c_uint;
    strm.next_out = out.as_mut_ptr();
    strm.avail_out = out.len() as c_uint;

    let mut code;
    let mut spins: usize = 0;
    loop {
        let before_in = strm.avail_in;
        let before_out = strm.avail_out;
        // SAFETY: `next_in`/`next_out` address the live `input`/`out` slices with
        // `avail_*` counts the engine only ever decreases, so both stay in bounds
        // across every spin; a null `state` is rejected, not dereferenced.
        code = unsafe { inflate(strm, flush) };
        spins += 1;

        let stalled = strm.avail_in == before_in && strm.avail_out == before_out;
        if code != Z_OK
            || stalled
            || strm.avail_in == 0
            || strm.avail_out == 0
            || spins >= DRIVE_GUARD
        {
            break;
        }
    }

    assert!(
        strm.avail_in as usize <= input.len() && strm.avail_out as usize <= out.len(),
        "inflate must never grow avail_in or avail_out"
    );
    Driven {
        consumed: input.len() - strm.avail_in as usize,
        produced: out.len() - strm.avail_out as usize,
        code,
    }
}

/// Raw-inflates `stream` through the C ABI and asserts it recovers `expected`
/// byte-for-byte.
///
/// This is the independent cross-check on the *source* of a copy. Two halves that
/// diverged together would still agree with each other, so only decoding the
/// finished stream catches a copy that damaged the original — the failure mode a
/// port replacing C's interior arena pointers has to rule out.
///
/// Runs on the global allocator, so it cannot disturb the hook accounting of the
/// probe that calls it.
fn assert_raw_round_trip(stream: &[u8], expected: &[u8]) {
    let mut is = zeroed_stream();
    // SAFETY: `is` is a valid owned `z_stream`; `RAW_WINDOW_BITS` matches the
    // window of the encoder that produced `stream`, which is what C requires of a
    // raw decoder.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, version_ok(), stream_size_ok()) } != Z_OK {
        return;
    }
    let mut back = vec![0u8; expected.len() + 16];
    let run = drive_inflate(&mut is, stream, &mut back, Z_FINISH);
    // SAFETY: `is` holds a live inflate handle. Reclaimed before the assertions
    // below so a failure cannot leak on the panic path.
    let end = unsafe { inflateEnd(&mut is) };
    assert_eq!(
        end, Z_OK,
        "inflateEnd must reclaim the verification stream, not {end}"
    );
    assert_eq!(
        run.code, Z_STREAM_END,
        "a stream this library finished must decode to stream end"
    );
    assert_eq!(
        &back[..run.produced],
        expected,
        "a stream this library finished must decode back to its input"
    );
}

/// Takes a [`deflateCopy`] at a fuzzer-chosen point mid-stream and asserts the
/// copy and its source continue byte-for-byte identically.
///
/// This is the property no deterministic test can cover exhaustively: `cut` lands
/// anywhere in the payload, so the branch happens with the match finder, the
/// pending buffer, the bit accumulator and the symbol buffer in an arbitrary
/// combination of states. C `deflateCopy` `zmemcpy`s the whole `z_stream` and then
/// re-derives `ds->sym_buf` from the copied `pending_buf` (`deflate.c` L1368);
/// this port owns its buffers and deep-clones them instead, so getting the
/// offsets or the clone order wrong would surface as a divergence here and
/// nowhere else.
///
/// `cut` is clamped into the payload and `level` is one of the eleven legal
/// values, so a rejection seen here is a finding rather than a bad argument.
fn probe_deflate_copy_convergence(payload: &[u8], cut: usize, level: c_int) {
    let good = version_ok();
    let size = stream_size_ok();

    let hooks = HookState::with_budget(usize::MAX);
    let mut src = zeroed_stream();
    install_hooks(&mut src, &hooks);
    // SAFETY: `src` is a valid owned `z_stream` whose `opaque` addresses `hooks`,
    // which outlives every call below; `RAW_WINDOW_BITS` with `memLevel = 1` is a
    // legal, deliberately small configuration and `level` is in range.
    if unsafe {
        deflateInit2_(
            &mut src,
            level,
            Z_DEFLATED,
            RAW_WINDOW_BITS,
            1,
            0,
            good,
            size,
        )
    } != Z_OK
    {
        return;
    }

    // Both engines get the same deliberately over-sized room, so a mismatch in the
    // comparison below can only come from the engines themselves and never from
    // one of them running out of output space before the other.
    let cap = payload.len() * 2 + 256;
    let mut out_src = vec![0u8; cap];
    let mut out_cpy = vec![0u8; cap];

    // ---- Drive the source up to the cut, then branch the stream. ----
    let cut = cut.min(payload.len());
    let head = drive_deflate(&mut src, &payload[..cut], &mut out_src, Z_NO_FLUSH);
    assert_contract_code(
        head.code,
        Contract::Deflate,
        "deflate driven up to the copy point",
    );

    let handed_before = hooks.handed_out.get();
    let live_before = handed_before - hooks.released.get();
    assert_eq!(
        live_before, DEFLATE_INIT_ALLOCATIONS,
        "a mid-stream deflate stream must still hold exactly its init buffers"
    );

    let mut cpy = zeroed_stream();
    // SAFETY: `cpy` and `src` are distinct, valid, exclusively-owned `z_stream`s —
    // never the same object — and `src` carries a live deflate handle, which is
    // exactly `deflateCopy`'s documented contract.
    let cret = unsafe { deflateCopy(&mut cpy, &mut src) };
    assert_contract_code(
        cret,
        Contract::DeflateCopy,
        "deflateCopy of a live deflate stream",
    );
    if cret != Z_OK {
        // A refused copy must leave `dest` untouched, so there is no handle to
        // reclaim, and every block it had already taken must be back.
        // SAFETY: `cpy` is a valid `z_stream`; a refused copy installs no handle.
        let kind = unsafe { peek_handle_kind(&cpy) };
        assert!(
            kind.is_none(),
            "a refused deflateCopy must install no handle in dest"
        );
        assert_eq!(
            hooks.handed_out.get() - hooks.released.get(),
            live_before,
            "a refused deflateCopy must release every block it had already taken"
        );
        // SAFETY: `src` still holds its live deflate handle; reclaim it.
        let end = unsafe { deflateEnd(&mut src) };
        assert_contract_code(
            end,
            Contract::DeflateEnd,
            "deflateEnd after a refused deflateCopy",
        );
        hooks.assert_balanced("deflate stream whose copy was refused");
        return;
    }

    // SAFETY: `cpy.state` was just installed by `deflateCopy` as a tagged
    // `#[repr(C)]` handle, so its leading `HandleKind` is readable.
    let kind = unsafe { peek_handle_kind(&cpy) };
    assert_eq!(
        kind,
        Some(HandleKind::DEFLATE),
        "a successful deflateCopy must install a DEFLATE-tagged handle"
    );
    // C `deflateCopy` `zmemcpy`s the whole `z_stream`, so every observable field
    // must arrive in the destination.
    assert_eq!(
        cpy.total_in, src.total_in,
        "deflateCopy must mirror total_in"
    );
    assert_eq!(
        cpy.total_out, src.total_out,
        "deflateCopy must mirror total_out"
    );
    assert_eq!(cpy.adler, src.adler, "deflateCopy must mirror adler");
    assert_eq!(
        cpy.data_type, src.data_type,
        "deflateCopy must mirror data_type"
    );
    assert_eq!(
        cpy.opaque, src.opaque,
        "deflateCopy must mirror the allocator opaque"
    );
    // The clone must come out of the *caller's* memory, and must re-request
    // exactly the set of buffers the source holds: C's `deflateCopy` re-issues the
    // state reservation plus window/prev/head/pending_buf, so a caller's `zalloc`
    // sees the same request count for a copy as for an init (AAP §0.6.5).
    assert_eq!(
        hooks.handed_out.get() - handed_before,
        live_before,
        "deflateCopy must re-request exactly the source's buffers, from the \
         source's own zalloc"
    );

    // ---- Both engines now finish the same remaining input, independently. ----
    let tail = &payload[cut..];
    let room = cap - head.produced;
    let cpy_run = drive_deflate(&mut cpy, tail, &mut out_cpy[..room], Z_FINISH);
    let src_run = drive_deflate(&mut src, tail, &mut out_src[head.produced..], Z_FINISH);

    assert_eq!(
        cpy_run.code, src_run.code,
        "a deflateCopy and its source must reach the same code from the same \
         remaining input"
    );
    assert_eq!(
        cpy_run.consumed, src_run.consumed,
        "a deflateCopy and its source must consume the same number of bytes"
    );
    assert_eq!(
        cpy_run.produced, src_run.produced,
        "a deflateCopy and its source must emit the same number of bytes after \
         the copy point"
    );
    assert_eq!(
        &out_cpy[..cpy_run.produced],
        &out_src[head.produced..head.produced + src_run.produced],
        "a deflateCopy and its source must emit byte-identical output after the \
         copy point"
    );

    // SAFETY: `cpy` holds its own live deflate handle; reclaimed exactly once.
    let cpy_end = unsafe { deflateEnd(&mut cpy) };
    // SAFETY: `src` holds its own live deflate handle; reclaimed exactly once.
    let src_end = unsafe { deflateEnd(&mut src) };
    // C's `deflateEnd` reports `Z_DATA_ERROR` when it tears down a stream still in
    // `BUSY_STATE` and `Z_OK` otherwise, freeing the state either way. The two
    // engines are in identical states, so whichever verdict applies must apply to
    // both — that agreement is the contractual part, not the particular value.
    assert_eq!(
        cpy_end, src_end,
        "a deflateCopy and its source must report the same deflateEnd verdict"
    );
    assert!(
        matches!(cpy_end, Z_OK | Z_DATA_ERROR),
        "deflateEnd must report Z_OK, or C's Z_DATA_ERROR for a still-busy \
         stream, not {cpy_end}"
    );
    // SAFETY: `cpy` is a valid `z_stream` whose handle was just reclaimed.
    let kind = unsafe { peek_handle_kind(&cpy) };
    assert!(
        kind.is_none(),
        "deflateEnd must clear the copy's state handle"
    );
    // SAFETY: `src` is a valid `z_stream` whose handle was just reclaimed.
    let kind = unsafe { peek_handle_kind(&src) };
    assert!(
        kind.is_none(),
        "deflateEnd must clear the source's state handle"
    );
    hooks.assert_balanced("deflate copy driven to completion");

    if src_run.code == Z_STREAM_END {
        assert_raw_round_trip(&out_src[..head.produced + src_run.produced], payload);
    }
}

/// Takes an [`inflateCopy`] at a fuzzer-chosen point mid-decode and asserts the
/// copy and its source continue byte-for-byte identically.
///
/// The decoder is where replacing C's interior pointers is load-bearing: C keeps
/// `state->lencode`, `state->distcode` and `state->next` as pointers *into*
/// `state->codes[]`, so a `zmemcpy` of the struct leaves them addressing the
/// source's arena and C's `inflateCopy` has to repair them by hand. This port
/// carries a table-source discriminant plus integer offsets instead, and a cut
/// taken while a dynamic table is half-built is the case that proves the
/// substitution is sound.
fn probe_inflate_copy_convergence(payload: &[u8], cut: usize) {
    let good = version_ok();
    let size = stream_size_ok();

    // Produce a raw stream to decode. `raw_deflate` runs on the global allocator,
    // so it cannot disturb the hook accounting below.
    let mut comp = vec![0u8; payload.len() * 2 + 256];
    // `raw_deflate` asserts its own completion, so there is no "did not finish in
    // one shot" skip here: a payload the library cannot compress in one bounded
    // `Z_FINISH` is a finding, and swallowing it would disable this whole probe.
    let produced = raw_deflate(payload, &mut comp);
    let stream = &comp[..produced];

    let hooks = HookState::with_budget(usize::MAX);
    let mut src = zeroed_stream();
    install_hooks(&mut src, &hooks);
    // SAFETY: `src` is a valid owned `z_stream` whose `opaque` addresses `hooks`,
    // which outlives every call below.
    if unsafe { inflateInit2_(&mut src, RAW_WINDOW_BITS, good, size) } != Z_OK {
        return;
    }

    let cap = payload.len() + 16;
    let mut back_src = vec![0u8; cap];
    let mut back_cpy = vec![0u8; cap];

    // ---- Decode up to the cut, then branch the stream. ----
    let cut = cut.min(stream.len());
    let head = drive_inflate(&mut src, &stream[..cut], &mut back_src, Z_NO_FLUSH);
    assert_contract_code(
        head.code,
        Contract::Inflate,
        "inflate driven up to the copy point",
    );

    let handed_before = hooks.handed_out.get();
    // C's `inflateCopy` re-requests the state reservation and, only when the source
    // already owns one, the sliding window (`inflate.c` L1340-L1346) — so the
    // copy's charge is exactly the set of blocks the source currently holds: one
    // before the window is needed, two once it has been. That deferral is the same
    // timing `INFLATE_INIT_ALLOCATIONS` pins at init (AAP §0.6.5), and which side
    // of it this execution lands on depends on `cut`.
    let live_before = handed_before - hooks.released.get();
    assert!(
        (INFLATE_INIT_ALLOCATIONS..=INFLATE_INIT_ALLOCATIONS + 1).contains(&live_before),
        "a mid-decode inflate stream must hold its state and at most one window, \
         found {live_before} blocks"
    );

    let mut cpy = zeroed_stream();
    // SAFETY: `cpy` and `src` are distinct, valid, exclusively-owned `z_stream`s
    // and `src` carries a live inflate handle with both allocator halves
    // installed — exactly `inflateCopy`'s documented contract.
    let cret = unsafe { inflateCopy(&mut cpy, &mut src) };
    assert_contract_code(
        cret,
        Contract::InflateCopy,
        "inflateCopy of a live inflate stream",
    );
    if cret != Z_OK {
        // SAFETY: `cpy` is a valid `z_stream`; a refused copy installs no handle.
        let kind = unsafe { peek_handle_kind(&cpy) };
        assert!(
            kind.is_none(),
            "a refused inflateCopy must install no handle in dest"
        );
        assert_eq!(
            hooks.handed_out.get() - hooks.released.get(),
            live_before,
            "a refused inflateCopy must release every block it had already taken"
        );
        // SAFETY: `src` still holds its live inflate handle; reclaim it.
        let ret = unsafe { inflateEnd(&mut src) };
        assert_eq!(
            ret, Z_OK,
            "inflateEnd must succeed after a refused inflateCopy"
        );
        hooks.assert_balanced("inflate stream whose copy was refused");
        return;
    }

    // SAFETY: `cpy.state` was just installed by `inflateCopy` as a tagged
    // `#[repr(C)]` handle, so its leading `HandleKind` is readable.
    let kind = unsafe { peek_handle_kind(&cpy) };
    assert_eq!(
        kind,
        Some(HandleKind::INFLATE),
        "a successful inflateCopy must install an INFLATE-tagged handle"
    );
    // C `inflateCopy` `zmemcpy`s the whole `z_stream` too.
    assert_eq!(
        cpy.total_in, src.total_in,
        "inflateCopy must mirror total_in"
    );
    assert_eq!(
        cpy.total_out, src.total_out,
        "inflateCopy must mirror total_out"
    );
    assert_eq!(cpy.adler, src.adler, "inflateCopy must mirror adler");
    assert_eq!(
        cpy.data_type, src.data_type,
        "inflateCopy must mirror data_type"
    );
    assert_eq!(
        cpy.opaque, src.opaque,
        "inflateCopy must mirror the allocator opaque"
    );
    assert_eq!(
        hooks.handed_out.get() - handed_before,
        live_before,
        "inflateCopy must re-request exactly the source's live blocks, from the \
         source's own zalloc"
    );

    // ---- Both engines now decode the same remaining input, independently. ----
    let tail = &stream[cut..];
    let room = cap - head.produced;
    let cpy_run = drive_inflate(&mut cpy, tail, &mut back_cpy[..room], Z_FINISH);
    let src_run = drive_inflate(&mut src, tail, &mut back_src[head.produced..], Z_FINISH);

    assert_eq!(
        cpy_run.code, src_run.code,
        "an inflateCopy and its source must reach the same code from the same \
         remaining input"
    );
    assert_eq!(
        cpy_run.consumed, src_run.consumed,
        "an inflateCopy and its source must consume the same number of bytes"
    );
    assert_eq!(
        cpy_run.produced, src_run.produced,
        "an inflateCopy and its source must produce the same number of bytes \
         after the copy point"
    );
    assert_eq!(
        &back_cpy[..cpy_run.produced],
        &back_src[head.produced..head.produced + src_run.produced],
        "an inflateCopy and its source must produce byte-identical output after \
         the copy point"
    );

    // SAFETY: `cpy` holds its own live inflate handle; reclaimed exactly once.
    let cpy_end = unsafe { inflateEnd(&mut cpy) };
    // SAFETY: `src` holds its own live inflate handle; reclaimed exactly once.
    let src_end = unsafe { inflateEnd(&mut src) };
    assert_eq!(
        cpy_end, Z_OK,
        "inflateEnd must reclaim the copy, not report {cpy_end}"
    );
    assert_eq!(
        src_end, Z_OK,
        "inflateEnd must reclaim the source, not report {src_end}"
    );
    // SAFETY: `cpy` is a valid `z_stream` whose handle was just reclaimed.
    let kind = unsafe { peek_handle_kind(&cpy) };
    assert!(
        kind.is_none(),
        "inflateEnd must clear the copy's state handle"
    );
    // SAFETY: `src` is a valid `z_stream` whose handle was just reclaimed.
    let kind = unsafe { peek_handle_kind(&src) };
    assert!(
        kind.is_none(),
        "inflateEnd must clear the source's state handle"
    );
    hooks.assert_balanced("inflate copy driven to completion");

    if src_run.code == Z_STREAM_END {
        assert_eq!(
            &back_src[..head.produced + src_run.produced],
            payload,
            "a decode that continued past an inflateCopy must still recover the \
             original payload"
        );
    }
}

/// Drives the five `*Reset*` entry points and asserts the C-specified effects: a
/// reset zeroes the observable byte counters and clears `msg`, and a full
/// `deflateReset`/`inflateReset` leaves the stream behaving exactly as a freshly
/// initialised one — *byte*-identically, which is the property that matters for a
/// library whose compressed output must match reference zlib (AAP §0.8.1 D-1).
///
/// `deflateResetKeep` deliberately gets the weaker treatment: it keeps the
/// allocations and does **not** re-emit the wrapper header, so a following stream
/// is legitimately different and only the counter and `msg` contract is assertable.
///
/// `inflateReset2` gets the strongest treatment available, because a reset that
/// silently ignored its `windowBits` would still pass every counter check: the
/// wrapper is switched to zlib, the raw stream must then stop decoding, and
/// switching back must restore it.
fn probe_reset_family(payload: &[u8], window_bits: c_int) {
    let good = version_ok();
    let size = stream_size_ok();
    // See `RESET_PAYLOAD_MAX`: none of the assertions below depend on the payload
    // length, so this probe takes the shorter slice and leaves the long one to the
    // copy probes, whose mid-stream cut genuinely needs the room.
    let payload = &payload[..payload.len().min(RESET_PAYLOAD_MAX)];
    let cap = payload.len() * 2 + 256;

    // ---- deflate: a reset stream must re-compress byte-identically. ----
    let hooks = HookState::with_budget(usize::MAX);
    let mut ds = zeroed_stream();
    install_hooks(&mut ds, &hooks);
    // SAFETY: `ds` is a valid owned `z_stream` whose `opaque` addresses `hooks`,
    // which outlives every call below; `window_bits` is one of the legal values
    // chosen by the caller and `memLevel = 1` keeps the handle small.
    if unsafe {
        deflateInit2_(
            &mut ds,
            DEFAULT_LEVEL,
            Z_DEFLATED,
            window_bits,
            1,
            0,
            good,
            size,
        )
    } == Z_OK
    {
        let mut first = vec![0u8; cap];
        let mut second = vec![0u8; cap];
        let run1 = drive_deflate(&mut ds, payload, &mut first, Z_FINISH);
        assert_contract_code(run1.code, Contract::Deflate, "deflate before deflateReset");

        // SAFETY: `ds` holds a live deflate handle.
        let ret = unsafe { deflateReset(&mut ds) };
        assert_eq!(
            ret, Z_OK,
            "deflateReset must succeed on a live deflate stream"
        );
        assert_eq!(ds.total_in, 0, "deflateReset must zero total_in");
        assert_eq!(ds.total_out, 0, "deflateReset must zero total_out");
        assert!(ds.msg.is_null(), "deflateReset must clear msg");

        let run2 = drive_deflate(&mut ds, payload, &mut second, Z_FINISH);
        assert_eq!(
            run2.code, run1.code,
            "a reset deflate stream must reach the same code as a fresh one"
        );
        assert_eq!(
            run2.produced, run1.produced,
            "a reset deflate stream must emit the same number of bytes"
        );
        assert_eq!(
            &second[..run2.produced],
            &first[..run1.produced],
            "a reset deflate stream must emit byte-identical output"
        );

        // SAFETY: `ds` still holds its live deflate handle.
        let ret = unsafe { deflateResetKeep(&mut ds) };
        assert_eq!(
            ret, Z_OK,
            "deflateResetKeep must succeed on a live deflate stream"
        );
        assert_eq!(ds.total_in, 0, "deflateResetKeep must zero total_in");
        assert_eq!(ds.total_out, 0, "deflateResetKeep must zero total_out");
        assert!(ds.msg.is_null(), "deflateResetKeep must clear msg");

        // A reset stream is no longer `BUSY_STATE`, so this is C's clean teardown.
        // SAFETY: `ds` holds a live, freshly reset deflate handle.
        let ret = unsafe { deflateEnd(&mut ds) };
        assert_eq!(
            ret, Z_OK,
            "deflateEnd must succeed for a freshly reset stream"
        );
        hooks.assert_balanced("deflate reset family");
    }

    // ---- inflate: a reset stream must re-decode identically, and
    // `inflateReset2` must genuinely re-select the wrapper. ----
    let mut comp = vec![0u8; cap];
    // `raw_deflate` asserts its own completion, so there is no "did not finish in
    // one shot" skip here: a payload the library cannot compress in one bounded
    // `Z_FINISH` is a finding, and swallowing it would disable this whole probe.
    let produced = raw_deflate(payload, &mut comp);
    let stream = &comp[..produced];

    let ihooks = HookState::with_budget(usize::MAX);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &ihooks);
    // SAFETY: `is` is a valid owned `z_stream` whose `opaque` addresses `ihooks`,
    // which outlives every call below.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) } != Z_OK {
        return;
    }

    let mut first = vec![0u8; payload.len() + 16];
    let mut second = vec![0u8; payload.len() + 16];
    let run1 = drive_inflate(&mut is, stream, &mut first, Z_FINISH);
    assert_eq!(
        run1.code, Z_STREAM_END,
        "the raw stream this probe built must decode to stream end"
    );
    assert_eq!(
        &first[..run1.produced],
        payload,
        "the raw stream this probe built must decode back to the payload"
    );

    // SAFETY: `is` holds a live inflate handle.
    let ret = unsafe { inflateReset(&mut is) };
    assert_eq!(
        ret, Z_OK,
        "inflateReset must succeed on a live inflate stream"
    );
    assert_eq!(is.total_in, 0, "inflateReset must zero total_in");
    assert_eq!(is.total_out, 0, "inflateReset must zero total_out");
    assert!(is.msg.is_null(), "inflateReset must clear msg");

    let run2 = drive_inflate(&mut is, stream, &mut second, Z_FINISH);
    assert_eq!(
        run2.code, Z_STREAM_END,
        "a reset inflate stream must decode the same stream again"
    );
    assert_eq!(
        &second[..run2.produced],
        &first[..run1.produced],
        "a reset inflate stream must recover byte-identical output"
    );

    // `inflateResetKeep` preserves the window history; the observable counters
    // must still be zeroed.
    // SAFETY: `is` still holds its live inflate handle.
    let ret = unsafe { inflateResetKeep(&mut is) };
    assert_eq!(
        ret, Z_OK,
        "inflateResetKeep must succeed on a live inflate stream"
    );
    assert_eq!(is.total_in, 0, "inflateResetKeep must zero total_in");
    assert_eq!(is.total_out, 0, "inflateResetKeep must zero total_out");
    assert!(is.msg.is_null(), "inflateResetKeep must clear msg");

    // Switch the wrapper to zlib. A raw DEFLATE stream can never pass C's zlib
    // header check — its first four bits are a block header, and a value of `8`
    // (`Z_DEFLATED`) is unreachable there — so only membership is asserted while
    // the point of the step is what follows it.
    let mut third = vec![0u8; payload.len() + 16];
    // SAFETY: `is` still holds its live inflate handle; `15` is the legal zlib
    // `windowBits`.
    let ret = unsafe { inflateReset2(&mut is, 15) };
    assert_eq!(
        ret, Z_OK,
        "inflateReset2 must accept a legal zlib windowBits"
    );
    assert_eq!(is.total_in, 0, "inflateReset2 must zero total_in");
    assert_eq!(is.total_out, 0, "inflateReset2 must zero total_out");
    let zlib_run = drive_inflate(&mut is, stream, &mut third, Z_FINISH);
    assert_contract_code(
        zlib_run.code,
        Contract::Inflate,
        "zlib-framed inflate fed a raw stream",
    );

    // Switching back must restore the raw framing, which is the only way to show
    // the wrap really changed rather than the reset being a counter-only no-op.
    // SAFETY: `is` still holds its live inflate handle.
    let ret = unsafe { inflateReset2(&mut is, RAW_WINDOW_BITS) };
    assert_eq!(
        ret, Z_OK,
        "inflateReset2 must accept a legal raw windowBits"
    );
    let run3 = drive_inflate(&mut is, stream, &mut third, Z_FINISH);
    assert_eq!(
        run3.code, Z_STREAM_END,
        "inflateReset2 back to raw framing must decode the raw stream again"
    );
    assert_eq!(
        &third[..run3.produced],
        &first[..run1.produced],
        "inflateReset2 back to raw framing must recover byte-identical output"
    );

    // An out-of-range `windowBits` is validated *before* anything is mutated, so
    // the handle survives and the stream stays usable.
    // SAFETY: `is` still holds its live inflate handle; the bad value is a
    // documented rejection, not undefined behaviour.
    let ret = unsafe { inflateReset2(&mut is, BAD_RESET_WINDOW_BITS) };
    assert_eq!(
        ret, Z_STREAM_ERROR,
        "inflateReset2 must reject an out-of-range windowBits"
    );
    // SAFETY: `is` is a valid `z_stream`; the rejected reset must not have touched
    // its handle.
    let kind = unsafe { peek_handle_kind(&is) };
    assert_eq!(
        kind,
        Some(HandleKind::INFLATE),
        "a rejected inflateReset2 must leave the handle intact"
    );

    // SAFETY: `is` holds a live inflate handle; reclaimed exactly once.
    let ret = unsafe { inflateEnd(&mut is) };
    assert_eq!(ret, Z_OK, "inflateEnd must succeed after the reset family");
    // SAFETY: `is` is a valid `z_stream` whose handle was just reclaimed.
    let kind = unsafe { peek_handle_kind(&is) };
    assert!(kind.is_none(), "inflateEnd must clear the state handle");
    ihooks.assert_balanced("inflate reset family");
}

/// The rejection half of the lifecycle: every entry point in the copy / reset
/// family must refuse a null stream, a never-initialised stream, an
/// already-reclaimed stream and a stream belonging to the *other* engine — and
/// must leave its arguments exactly as it found them.
///
/// The cross-engine cases matter for the same reason the `*End` ones do: in C a
/// `deflate_state` reached through `inflateReset` is a type-confused write the API
/// cannot detect. The offset-0 `HandleKind` tag turns it into `Z_STREAM_ERROR`
/// with the handle untouched, which the follow-up calls prove.
///
/// Every case is a rejection the C contract defines an answer for, so all of these
/// are exact-value assertions rather than membership tests.
fn probe_copy_and_reset_misuse() {
    let good = version_ok();
    let size = stream_size_ok();
    let null: *mut z_stream = ptr::null_mut();

    // ---- A null `z_streamp` is a documented rejection for the whole family. ----
    // SAFETY: every shim tests its pointer for null before any dereference, so
    // passing one is a defined rejection rather than undefined behaviour.
    let deflate_reset = unsafe { deflateReset(null) };
    assert_eq!(
        deflate_reset, Z_STREAM_ERROR,
        "deflateReset must reject a null stream"
    );
    // SAFETY: as above — a null `z_streamp` is tested before any dereference.
    let deflate_reset_keep = unsafe { deflateResetKeep(null) };
    assert_eq!(
        deflate_reset_keep, Z_STREAM_ERROR,
        "deflateResetKeep must reject a null stream"
    );
    // SAFETY: as above — a null `z_streamp` is tested before any dereference.
    let inflate_reset = unsafe { inflateReset(null) };
    assert_eq!(
        inflate_reset, Z_STREAM_ERROR,
        "inflateReset must reject a null stream"
    );
    // SAFETY: as above — a null `z_streamp` is tested before any dereference.
    let inflate_reset2 = unsafe { inflateReset2(null, 15) };
    assert_eq!(
        inflate_reset2, Z_STREAM_ERROR,
        "inflateReset2 must reject a null stream"
    );
    // SAFETY: as above — a null `z_streamp` is tested before any dereference.
    let inflate_reset_keep = unsafe { inflateResetKeep(null) };
    assert_eq!(
        inflate_reset_keep, Z_STREAM_ERROR,
        "inflateResetKeep must reject a null stream"
    );
    // SAFETY: as above — a null `z_streamp` is tested before any dereference.
    let deflate_copy = unsafe { deflateCopy(null, null) };
    assert_eq!(
        deflate_copy, Z_STREAM_ERROR,
        "deflateCopy must reject two null streams"
    );
    // SAFETY: as above — a null `z_streamp` is tested before any dereference.
    let inflate_copy = unsafe { inflateCopy(null, null) };
    assert_eq!(
        inflate_copy, Z_STREAM_ERROR,
        "inflateCopy must reject two null streams"
    );

    // ---- A never-initialised stream carries no handle, so the family must reject
    // it exactly as C's `deflateStateCheck`/`inflateStateCheck` do — and a copy
    // must reject a null *side* as well as a null pair. ----
    let mut fresh = zeroed_stream();
    let mut dest = zeroed_stream();
    // SAFETY: `fresh` and `dest` are distinct valid owned `z_stream`s whose `state`
    // is null, which is the never-initialised shape C rejects; no shim dereferences
    // a null `state` or a null argument.
    let deflate_reset = unsafe { deflateReset(&mut fresh) };
    assert_eq!(
        deflate_reset, Z_STREAM_ERROR,
        "deflateReset must reject a never-initialised stream"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let deflate_reset_keep = unsafe { deflateResetKeep(&mut fresh) };
    assert_eq!(
        deflate_reset_keep, Z_STREAM_ERROR,
        "deflateResetKeep must reject a never-initialised stream"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let inflate_reset = unsafe { inflateReset(&mut fresh) };
    assert_eq!(
        inflate_reset, Z_STREAM_ERROR,
        "inflateReset must reject a never-initialised stream"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let inflate_reset2 = unsafe { inflateReset2(&mut fresh, 15) };
    assert_eq!(
        inflate_reset2, Z_STREAM_ERROR,
        "inflateReset2 must reject a never-initialised stream"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let inflate_reset_keep = unsafe { inflateResetKeep(&mut fresh) };
    assert_eq!(
        inflate_reset_keep, Z_STREAM_ERROR,
        "inflateResetKeep must reject a never-initialised stream"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let deflate_copy = unsafe { deflateCopy(&mut dest, &mut fresh) };
    assert_eq!(
        deflate_copy, Z_STREAM_ERROR,
        "deflateCopy must reject a never-initialised source"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let inflate_copy = unsafe { inflateCopy(&mut dest, &mut fresh) };
    assert_eq!(
        inflate_copy, Z_STREAM_ERROR,
        "inflateCopy must reject a never-initialised source"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let deflate_copy_2 = unsafe { deflateCopy(null, &mut fresh) };
    assert_eq!(
        deflate_copy_2, Z_STREAM_ERROR,
        "deflateCopy must reject a null destination"
    );
    // SAFETY: as above — a null `state` (or null argument) is tested, never dereferenced.
    let inflate_copy_2 = unsafe { inflateCopy(&mut dest, null) };
    assert_eq!(
        inflate_copy_2, Z_STREAM_ERROR,
        "inflateCopy must reject a null source"
    );
    // SAFETY: `dest` is a valid `z_stream`; every refused copy above must have left
    // it untouched.
    let kind = unsafe { peek_handle_kind(&dest) };
    assert!(
        kind.is_none(),
        "a refused copy must install no handle in dest"
    );

    // ---- Cross-engine misuse: a deflate handle is invisible to the inflate
    // reset/copy family. ----
    let mut ds = zeroed_stream();
    // SAFETY: `ds` is a valid owned `z_stream`; `RAW_WINDOW_BITS` with
    // `memLevel = 1` is a legal, deliberately small configuration.
    if unsafe {
        deflateInit2_(
            &mut ds,
            DEFAULT_LEVEL,
            Z_DEFLATED,
            RAW_WINDOW_BITS,
            1,
            0,
            good,
            size,
        )
    } == Z_OK
    {
        // SAFETY: the inflate family validates the offset-0 tag before reborrowing
        // the handle, so a deflate stream is rejected rather than reinterpreted as
        // an inflate handle; `ds` and `dest` are distinct valid streams.
        let inflate_reset = unsafe { inflateReset(&mut ds) };
        assert_eq!(
            inflate_reset, Z_STREAM_ERROR,
            "inflateReset must reject a deflate handle"
        );
        // SAFETY: as above — the offset-0 tag is validated before any reborrow.
        let inflate_reset2 = unsafe { inflateReset2(&mut ds, 15) };
        assert_eq!(
            inflate_reset2, Z_STREAM_ERROR,
            "inflateReset2 must reject a deflate handle"
        );
        // SAFETY: as above — the offset-0 tag is validated before any reborrow.
        let inflate_reset_keep = unsafe { inflateResetKeep(&mut ds) };
        assert_eq!(
            inflate_reset_keep, Z_STREAM_ERROR,
            "inflateResetKeep must reject a deflate handle"
        );
        // SAFETY: as above — the offset-0 tag is validated before any reborrow.
        let inflate_copy = unsafe { inflateCopy(&mut dest, &mut ds) };
        assert_eq!(
            inflate_copy, Z_STREAM_ERROR,
            "inflateCopy must reject a deflate source"
        );
        // SAFETY: `dest` is a valid `z_stream`; the rejected copy installs nothing.
        let kind = unsafe { peek_handle_kind(&dest) };
        assert!(
            kind.is_none(),
            "a rejected inflateCopy must install no handle in dest"
        );
        // SAFETY: `ds` is a valid `z_stream`; the rejected calls must have left its
        // handle intact.
        let kind = unsafe { peek_handle_kind(&ds) };
        assert_eq!(
            kind,
            Some(HandleKind::DEFLATE),
            "a rejected cross-engine reset must leave the deflate handle intact"
        );
        // The same live handle still copies through its *own* engine, and the copy
        // is an independent stream that has to be reclaimed on its own.
        // SAFETY: `dest` and `ds` are distinct valid streams and `ds` holds a live
        // deflate handle.
        if unsafe { deflateCopy(&mut dest, &mut ds) } == Z_OK {
            // SAFETY: `dest` now owns its own tagged deflate handle.
            let kind = unsafe { peek_handle_kind(&dest) };
            assert_eq!(
                kind,
                Some(HandleKind::DEFLATE),
                "deflateCopy must install a DEFLATE-tagged handle"
            );
            // SAFETY: `dest` holds a live deflate handle that never left `Init`, so
            // this is C's clean teardown.
            let ret = unsafe { deflateEnd(&mut dest) };
            assert_eq!(ret, Z_OK, "deflateEnd must reclaim a copied deflate handle");
        }
        // SAFETY: `ds` still holds its own live deflate handle.
        let ret = unsafe { deflateEnd(&mut ds) };
        assert_eq!(ret, Z_OK, "deflateEnd must reclaim the copy source");
        // SAFETY: the handle is gone, so the family now sees a null `state` — the
        // post-`*End` case a C caller reaches by resetting a closed stream.
        let deflate_reset = unsafe { deflateReset(&mut ds) };
        assert_eq!(
            deflate_reset, Z_STREAM_ERROR,
            "deflateReset must reject an already-reclaimed stream"
        );
        // SAFETY: as above — the reclaimed stream now presents a null `state`.
        let deflate_reset_keep = unsafe { deflateResetKeep(&mut ds) };
        assert_eq!(
            deflate_reset_keep, Z_STREAM_ERROR,
            "deflateResetKeep must reject an already-reclaimed stream"
        );
        // SAFETY: as above — the reclaimed stream now presents a null `state`.
        let deflate_copy = unsafe { deflateCopy(&mut dest, &mut ds) };
        assert_eq!(
            deflate_copy, Z_STREAM_ERROR,
            "deflateCopy must reject an already-reclaimed source"
        );
    }

    // ---- The mirror case: an inflate handle is invisible to the deflate
    // reset/copy family, and `inflateCopy` additionally rejects a source whose
    // allocator pair has been broken. ----
    let mut is = zeroed_stream();
    // SAFETY: `is` is a valid owned `z_stream` with the expected version pair.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) } == Z_OK {
        // SAFETY: the deflate family validates the offset-0 tag first, so an
        // inflate stream is rejected rather than reinterpreted; `is` and `dest` are
        // distinct valid streams.
        let deflate_reset = unsafe { deflateReset(&mut is) };
        assert_eq!(
            deflate_reset, Z_STREAM_ERROR,
            "deflateReset must reject an inflate handle"
        );
        // SAFETY: as above — the offset-0 tag is validated before any reborrow.
        let deflate_reset_keep = unsafe { deflateResetKeep(&mut is) };
        assert_eq!(
            deflate_reset_keep, Z_STREAM_ERROR,
            "deflateResetKeep must reject an inflate handle"
        );
        // SAFETY: as above — the offset-0 tag is validated before any reborrow.
        let deflate_copy = unsafe { deflateCopy(&mut dest, &mut is) };
        assert_eq!(
            deflate_copy, Z_STREAM_ERROR,
            "deflateCopy must reject an inflate source"
        );
        // SAFETY: `dest` is a valid `z_stream`; nothing may have been installed.
        let kind = unsafe { peek_handle_kind(&dest) };
        assert!(
            kind.is_none(),
            "a rejected deflateCopy must install no handle in dest"
        );
        // SAFETY: `is` is a valid `z_stream`; its handle must be intact.
        let kind = unsafe { peek_handle_kind(&is) };
        assert_eq!(
            kind,
            Some(HandleKind::INFLATE),
            "a rejected cross-engine reset must leave the inflate handle intact"
        );

        // C's `inflateStateCheck` rejects a source whose `zalloc` or `zfree` is
        // null (`inflate.c` L90-L91, reached from `inflateCopy` before its
        // `ZALLOC(source, …)`), so this shim must too: a half-present pair would
        // otherwise have the clone taken from the global heap while the caller
        // believes their hook owns it. Clearing a *public* `z_stream` field is
        // exactly what a C caller can do, and the restore below puts it back.
        let saved_zalloc = is.zalloc;
        is.zalloc = None;
        // SAFETY: `dest` and `is` are distinct valid streams; the shim reads the
        // allocator fields and rejects before allocating or calling the now-absent
        // hook.
        let ret = unsafe { inflateCopy(&mut dest, &mut is) };
        assert_eq!(
            ret, Z_STREAM_ERROR,
            "inflateCopy must reject a source with a half-present allocator pair"
        );
        is.zalloc = saved_zalloc;
        // SAFETY: `dest` is a valid `z_stream`; the rejection installs nothing.
        let kind = unsafe { peek_handle_kind(&dest) };
        assert!(
            kind.is_none(),
            "a refused inflateCopy must install no handle in dest"
        );

        // With the pair whole again the same source copies cleanly.
        // SAFETY: `dest` and `is` are distinct valid streams and `is` holds a live
        // inflate handle with both allocator halves installed.
        if unsafe { inflateCopy(&mut dest, &mut is) } == Z_OK {
            // SAFETY: `dest` now owns its own tagged inflate handle.
            let kind = unsafe { peek_handle_kind(&dest) };
            assert_eq!(
                kind,
                Some(HandleKind::INFLATE),
                "inflateCopy must install an INFLATE-tagged handle"
            );
            // SAFETY: `dest` holds a live inflate handle.
            let ret = unsafe { inflateEnd(&mut dest) };
            assert_eq!(ret, Z_OK, "inflateEnd must reclaim a copied inflate handle");
        }
        // SAFETY: `is` still holds its own live inflate handle.
        let ret = unsafe { inflateEnd(&mut is) };
        assert_eq!(ret, Z_OK, "inflateEnd must reclaim the copy source");
        // SAFETY: the handle is gone, so the family now sees a null `state`.
        let inflate_reset = unsafe { inflateReset(&mut is) };
        assert_eq!(
            inflate_reset, Z_STREAM_ERROR,
            "inflateReset must reject an already-reclaimed stream"
        );
        // SAFETY: as above — the reclaimed stream now presents a null `state`.
        let inflate_reset2 = unsafe { inflateReset2(&mut is, 15) };
        assert_eq!(
            inflate_reset2, Z_STREAM_ERROR,
            "inflateReset2 must reject an already-reclaimed stream"
        );
        // SAFETY: as above — the reclaimed stream now presents a null `state`.
        let inflate_reset_keep = unsafe { inflateResetKeep(&mut is) };
        assert_eq!(
            inflate_reset_keep, Z_STREAM_ERROR,
            "inflateResetKeep must reject an already-reclaimed stream"
        );
        // SAFETY: as above — the reclaimed stream now presents a null `state`.
        let inflate_copy = unsafe { inflateCopy(&mut dest, &mut is) };
        assert_eq!(
            inflate_copy, Z_STREAM_ERROR,
            "inflateCopy must reject an already-reclaimed source"
        );
    }
}

// ===========================================================================
// N5 — arbitrary bytes through the raw C-ABI gzip / auto-detect inflate path
// ===========================================================================

/// Drives the **unmodified** fuzz input through the raw C-ABI gzip /
/// auto-detect inflate path: `inflateInit2_(47)`, a `Z_NO_FLUSH` drain loop, and
/// an unconditional `inflateEnd`.
///
/// # Why this leg belongs here
///
/// The sibling `fuzz_gzip` target used to reach the RFC 1952 header parser
/// through exactly these three `extern "C"` symbols. It was migrated to the safe
/// streaming API — correctly, because a harness has no business on the `unsafe`
/// side of the boundary unless probing the boundary *is* its remit — but that
/// left no target driving arbitrary gzip or auto-detect bytes through the C ABI
/// at all. Probing the C ABI is this target's remit, so the leg is restored here
/// rather than lost: the safe harness keeps its expanded gzip coverage and the
/// raw-ABI route is additive to it, not a replacement for it.
///
/// It is a genuinely distinct path. The round trip above and every probe below
/// are zlib-framed or raw-framed, so none of them exercises `inflateInit2_`'s
/// `40..=47` acceptance, the auto-detect discrimination in `inflate_reset2`, or
/// the gzip header state machine reached across the ABI with caller-owned
/// `next_in`/`next_out` pointers and `avail_*` counters.
///
/// # What is asserted
///
/// Nothing about *which* code arbitrary bytes deserve — for untrusted input a
/// clean rejection is the correct answer, so demanding a specific value would
/// manufacture false crashes. What is pinned is the closed set: every return
/// crossing the ABI is one of the nine documented `Z_*` codes, progress never
/// exceeds the buffers handed over, the handle is tagged
/// [`HandleKind::INFLATE`] while it lives and reclaimed afterwards, and the
/// teardown itself returns a defined code.
///
/// A rejected `inflateInit2_` returns early, which is both the original
/// harness's behaviour and the signal that this build has gzip compiled out —
/// the `40..=47` request then fails the `8..=15` bounds test exactly as a C zlib
/// built without `GUNZIP` does. A failed init installs no handle, so there is
/// nothing to tear down.
fn probe_raw_gzip_autodetect(data: &[u8]) {
    let mut strm = zeroed_stream();
    // SAFETY: `strm` is a live, owned, zeroed `z_stream`; `version_ok()` and
    // `stream_size_ok()` are precisely the values the shim validates against.
    let init = unsafe { inflateInit2_(&mut strm, WBITS_AUTO, version_ok(), stream_size_ok()) };
    assert_contract_code(
        init,
        Contract::InflateInit,
        "inflateInit2_ with auto-detect windowBits",
    );
    if init != Z_OK {
        // SAFETY: `strm` is a valid `z_stream`; a rejected init must leave
        // `state` null, so reading the tag is defined and yields `None`.
        let kind = unsafe { peek_handle_kind(&strm) };
        assert!(
            kind.is_none(),
            "a rejected inflateInit2_ must install no handle"
        );
        return;
    }
    // SAFETY: init reported success, so `state` addresses a live tagged handle
    // whose first field is a `HandleKind`.
    let kind = unsafe { peek_handle_kind(&strm) };
    assert_eq!(
        kind,
        Some(HandleKind::INFLATE),
        "a successful inflateInit2_ must install an inflate-tagged handle"
    );

    let mut out = [0u8; AUTODETECT_OUT_WINDOW];
    let mut in_pos = 0usize;
    let mut guard: u32 = 0;
    loop {
        let remaining = &data[in_pos..];
        strm.next_in = remaining.as_ptr();
        // Clamped rather than cast so a hypothetical input wider than `c_uint`
        // under-reports instead of wrapping; libFuzzer caps input at 64 KiB, so
        // in practice the count is exact.
        strm.avail_in = c_uint::try_from(remaining.len()).unwrap_or(c_uint::MAX);
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = AUTODETECT_OUT_WINDOW as c_uint;
        let offered_in = strm.avail_in;
        let offered_out = strm.avail_out;

        // SAFETY: `next_in` addresses the live `remaining` slice and `next_out`
        // the live `out` array, each with an `avail_*` count that does not
        // exceed the buffer it describes, and `strm` holds the handle installed
        // above.
        let ret = unsafe { inflate(&mut strm, Z_NO_FLUSH) };
        assert_contract_code(
            ret,
            Contract::Inflate,
            "inflate with auto-detect windowBits",
        );

        // The engine may only ever have taken from, or written into, what it was
        // given. These are library invariants rather than input-dependent
        // expectations, so a violation is a genuine finding — and they are what
        // keep `in_pos` provably inside `data`, so the re-slice above cannot go
        // out of range on the next pass.
        assert!(
            strm.avail_in <= offered_in,
            "inflate raised avail_in above what was offered"
        );
        assert!(
            strm.avail_out <= offered_out,
            "inflate raised avail_out above what was offered"
        );
        let consumed = (offered_in - strm.avail_in) as usize;
        let produced = (offered_out - strm.avail_out) as usize;
        in_pos = in_pos.saturating_add(consumed);

        // Stop on Z_STREAM_END or any error; only Z_OK continues. Also stop once
        // every input byte has been consumed, which is reference zlib's own
        // `avail_in == 0` test and bounds what a bomb can demand, since output is
        // only pulled while input remains.
        if ret != Z_OK || in_pos >= data.len() {
            break;
        }
        // A pass that neither consumed nor produced cannot be followed by one
        // that does: the decoder is stalled, typically on a truncated stream.
        if consumed == 0 && produced == 0 {
            break;
        }
        guard = guard.saturating_add(1);
        if guard > AUTODETECT_WORK_BUDGET {
            break;
        }
    }

    // SAFETY: `strm` holds the live inflate handle installed above; this runs on
    // every exit path from the loop, including the error, stall and
    // budget-exhausted ones.
    let end = unsafe { inflateEnd(&mut strm) };
    assert_contract_code(
        end,
        Contract::InflateEnd,
        "inflateEnd after the gzip auto-detect probe",
    );
    // SAFETY: `strm` is a valid `z_stream` whose `state` must now be null.
    let kind = unsafe { peek_handle_kind(&strm) };
    assert!(
        kind.is_none(),
        "inflateEnd must reclaim the auto-detect handle"
    );
}

fuzz_target!(|data: &[u8]| {
    // The round trip runs first, on every input. It has no early returns left —
    // every step is a hard assertion — and it stays in a function so the boundary
    // probes below are reached on a distinct stack frame with no shared state.
    ffi_round_trip(data);

    // Probe steering comes from the leading input byte, so libFuzzer can reach
    // every shape immediately from an empty corpus instead of having to discover
    // a structured prefix. The round trip above compresses the whole of `data`,
    // selector byte included, so no input is withheld from it.
    let selector = data.first().copied().unwrap_or(0);
    // One of the legal deflate `windowBits` values, kept small so the probe
    // handles stay cheap and executions-per-second stay high.
    let window_bits = match selector & 0x03 {
        0 => 9,
        1 => 12,
        2 => -9,
        _ => 25,
    };

    // N1 — the validation ladders reject before allocating, so they are cheap
    // enough to run on every execution.
    probe_init_ladder();
    probe_parameter_validation();
    probe_null_and_stateless_calls();
    probe_buffer_validation();
    probe_sync_recovery(data);

    // N2 — handle tagging and cross-engine `*End` misuse.
    probe_handle_tag_misuse(window_bits);

    // N3 — the allocator-hook contract and allocation-failure timing.
    probe_allocator_balance(window_bits);
    probe_allocator_failure_timing(data);

    // N4 — the copy / reset lifecycle. Two independent cut offsets and a
    // compression level come from later input bytes, so the leading selector byte
    // keeps doing its existing job and libFuzzer can steer the copy point
    // anywhere in the payload. Absent bytes fall back to fixed values, which keeps
    // the probes productive from a cold corpus; `min`-clamping inside each probe
    // then guarantees the offsets are always in range whatever the fuzzer picks.
    let deflate_cut = usize::from(u16::from_le_bytes([
        data.get(1).copied().unwrap_or(0),
        data.get(2).copied().unwrap_or(0),
    ]));
    let inflate_cut = usize::from(u16::from_le_bytes([
        data.get(3).copied().unwrap_or(0),
        data.get(4).copied().unwrap_or(0),
    ]));
    // `-1` (the default) plus the ten explicit levels, so every legal value is
    // reachable. An out-of-range level would simply be rejected at init and the
    // probe would then explore nothing.
    let level = (c_int::from(data.get(5).copied().unwrap_or(0)) % 11) - 1;
    let payload = &data[..data.len().min(COPY_PAYLOAD_MAX)];

    probe_deflate_copy_convergence(payload, deflate_cut, level);
    probe_inflate_copy_convergence(payload, inflate_cut);
    probe_reset_family(payload, window_bits);
    probe_copy_and_reset_misuse();

    // N5 — arbitrary bytes through the raw C-ABI gzip / auto-detect inflate path,
    // the one framing no other leg of this target reaches. Driven with the
    // unmodified input so the fuzzer steers the RFC 1952 header parser directly.
    probe_raw_gzip_autodetect(data);
});
