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
//! `src/ffi/**` is the only part of the crate permitted to contain `unsafe`, and
//! this is the only fuzz target that crosses into it, so the round trip above is
//! paired with three groups of hostile boundary probes a happy path cannot
//! reach:
//!
//! * **Boundary robustness and the `*Init*_` validation ladder** — a null
//!   `z_streamp`, null and wrong-major `version` strings, a mismatched
//!   `stream_size`, out-of-range `level`/`method`/`windowBits`/`memLevel`/
//!   `strategy`, inconsistent `next_*`/`avail_*` pairs, calls against a
//!   never-initialised (null-`state`) stream, and a double `*End`. Each must come
//!   back as a defined `Z_*` code without unwinding or aborting across the C ABI.
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
//!
//! Every `unsafe` block below carries a `// SAFETY:` comment, and every probe
//! stays inside the space where the C API contract defines an answer: a null
//! pointer, a bad version, a wrong size, a wrong-`kind` stream and a double
//! `*End` are all specified rejections, whereas handing the library a dangling
//! pointer or an `avail_*` count that overstates a real allocation would be a
//! defect in this harness rather than a finding about the library.

use core::alloc::Layout;
use core::cell::Cell;
use core::ffi::{c_char, c_int, c_uint, c_void};
use core::ptr;

use libfuzzer_sys::fuzz_target;
use zlib_rs::ffi::{
    HandleKind, deflate, deflateEnd, deflateInit_, deflateInit2_, inflate, inflateBackEnd,
    inflateBackInit_, inflateEnd, inflateInit_, inflateInit2_, peek_handle_kind, z_stream,
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

/// Asserts that `ret` is one of the nine return codes zlib is allowed to
/// produce. Arbitrary and adversarial input legitimately yields several of them,
/// so membership — not a specific value — is what a probe may demand unless the
/// C contract pins the outcome exactly. Anything outside the set would be an
/// undefined value escaping across the C ABI.
fn assert_legal_code(ret: c_int, what: &str) {
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
// The round trip — unchanged, and the reason this target exists
// ===========================================================================

/// Compresses `data` through the `extern "C"` deflate path and decompresses it
/// back through the `extern "C"` inflate path, asserting byte-for-byte recovery.
///
/// Hoisted out of [`fuzz_target!`] with its statements unchanged so that its
/// early returns — an init failure, or a `Z_FINISH` that did not complete in one
/// shot — cannot skip the boundary probes that run afterwards. The stream being
/// decoded here is produced by the library itself, so unlike the probes below
/// these are hard assertions: a mismatch genuinely is a bug.
fn ffi_round_trip(data: &[u8]) {
    // The valid version string and the correct `stream_size`, unchanged from the
    // original harness. `c"1"` carries the same `[0x31, 0x00]` bytes the previous
    // `b"1\0".as_ptr() as *const c_char` form produced, so what reaches the shim
    // is identical; the literal form simply satisfies `manual_c_str_literals`
    // without suppressing the lint.
    let version = c"1".as_ptr();
    let size = core::mem::size_of::<z_stream>() as c_int;

    // ---- Compress through the C ABI deflate path. -------------------------
    // SAFETY: all-zero `z_stream` is the C `memset` idiom; every field is a
    // valid zero (null pointers, `None` hooks, zero counters).
    let mut ds: z_stream = unsafe { core::mem::zeroed() };
    // SAFETY: `ds` is a valid owned `z_stream`; `version`/`size` are what the C
    // API expects; `DEFAULT_LEVEL` is in range.
    if unsafe { deflateInit_(&mut ds, DEFAULT_LEVEL, version, size) } != Z_OK {
        return;
    }
    // Every `*Init*_` shim tags the opaque handle at offset 0, so the tag is
    // observable the instant initialisation succeeds. Reading it here costs no
    // extra allocation.
    // SAFETY: `ds.state` was just installed by `deflateInit_` as a tagged
    // `#[repr(C)]` handle, so its leading `HandleKind` is readable.
    assert_eq!(
        unsafe { peek_handle_kind(&ds) },
        Some(HandleKind::DEFLATE),
        "deflateInit_ must install a DEFLATE-tagged handle"
    );

    // 1.5x + 128 comfortably exceeds the zlib deflate bound for any input.
    let mut comp = vec![0u8; data.len() + data.len() / 2 + 128];
    ds.next_in = data.as_ptr();
    ds.avail_in = data.len() as c_uint;
    ds.next_out = comp.as_mut_ptr();
    ds.avail_out = comp.len() as c_uint;
    // SAFETY: `next_in`/`next_out` point at the live buffers with matching
    // `avail_*` counts; a single `Z_FINISH` completes in one call given the
    // over-sized output buffer.
    let dret = unsafe { deflate(&mut ds, Z_FINISH) };
    let produced = comp.len() - ds.avail_out as usize;
    // SAFETY: `ds` was initialized above; reclaim its internal state.
    unsafe {
        deflateEnd(&mut ds);
    }
    // SAFETY: `deflateEnd` nulled `ds.state`, so the tag read finds no handle.
    assert!(
        unsafe { peek_handle_kind(&ds) }.is_none(),
        "deflateEnd must clear the state handle"
    );
    if dret != Z_STREAM_END {
        return; // did not finish in one shot (should not happen); not a bug.
    }

    // ---- Decompress through the C ABI inflate path. -----------------------
    // SAFETY: as above.
    let mut is: z_stream = unsafe { core::mem::zeroed() };
    // SAFETY: valid owned `z_stream`; version/size as the C API expects.
    if unsafe { inflateInit_(&mut is, version, size) } != Z_OK {
        return;
    }
    // SAFETY: `is.state` was just installed by `inflateInit_` as a tagged
    // `#[repr(C)]` handle, so its leading `HandleKind` is readable.
    assert_eq!(
        unsafe { peek_handle_kind(&is) },
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
    unsafe {
        inflateEnd(&mut is);
    }
    // SAFETY: `inflateEnd` nulled `is.state`, so the tag read finds no handle.
    assert!(
        unsafe { peek_handle_kind(&is) }.is_none(),
        "inflateEnd must clear the state handle"
    );

    assert_eq!(iret, Z_STREAM_END, "ffi inflate did not reach stream end");
    assert_eq!(got, data.len(), "ffi round-trip length mismatch");
    assert_eq!(&back[..got], data, "ffi round-trip content mismatch");
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
        assert!(
            unsafe { peek_handle_kind(&ds) }.is_none(),
            "a rejected deflateInit_ must install no handle"
        );

        let mut is = zeroed_stream();
        // SAFETY: as above, for the inflate entry points.
        let ret = unsafe { inflateInit_(&mut is, version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "inflateInit_ must reject a bad version/stream_size pair"
        );
        // SAFETY: as above; `inflateInit2_` shares `inflateInit_`'s validation.
        let ret = unsafe { inflateInit2_(&mut is, 15, version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "inflateInit2_ must reject a bad version/stream_size pair"
        );
        // SAFETY: `window` is a live 512-byte buffer matching `windowBits = 9`,
        // and it outlives this call; the version guard rejects before it is used.
        let ret =
            unsafe { inflateBackInit_(&mut is, 9, window.as_mut_ptr(), version, stream_size) };
        assert_eq!(
            ret, Z_VERSION_ERROR,
            "inflateBackInit_ must reject a bad version/stream_size pair"
        );
        // SAFETY: `is` is a valid `z_stream` whose `state` must still be null.
        assert!(
            unsafe { peek_handle_kind(&is) }.is_none(),
            "a rejected inflate init must install no handle"
        );
    }

    // The version and size guards run *before* the null-stream guard, so a call
    // that is wrong in both ways reports `Z_VERSION_ERROR`. That ordering is
    // observable behaviour a caller can depend on, so it is pinned exactly.
    // SAFETY: passing a null `z_streamp` is a defined, rejected call — the shim
    // checks the pointer instead of dereferencing it.
    assert_eq!(
        unsafe { deflateInit_(no_stream, DEFAULT_LEVEL, bad_major, size) },
        Z_VERSION_ERROR,
        "the version guard must precede the null-stream guard in deflateInit_"
    );
    // SAFETY: as above, with a mismatched `stream_size` instead.
    assert_eq!(
        unsafe { inflateInit_(no_stream, good, 0) },
        Z_VERSION_ERROR,
        "the stream_size guard must precede the null-stream guard in inflateInit_"
    );
    // SAFETY: as above; a null window would also be rejected, but only after the
    // version guard has already had its say.
    assert_eq!(
        unsafe { inflateBackInit_(no_stream, 9, ptr::null_mut(), bad_major, size) },
        Z_VERSION_ERROR,
        "the version guard must precede the null-stream guard in inflateBackInit_"
    );

    // A null stream with an otherwise valid call is `Z_STREAM_ERROR`.
    // SAFETY: a null `z_streamp` is checked, never dereferenced.
    assert_eq!(
        unsafe { deflateInit_(no_stream, DEFAULT_LEVEL, good, size) },
        Z_STREAM_ERROR,
        "deflateInit_ must reject a null stream"
    );
    // SAFETY: as above.
    assert_eq!(
        unsafe { inflateInit_(no_stream, good, size) },
        Z_STREAM_ERROR,
        "inflateInit_ must reject a null stream"
    );
    // SAFETY: as above.
    assert_eq!(
        unsafe { inflateBackInit_(no_stream, 9, window.as_mut_ptr(), good, size) },
        Z_STREAM_ERROR,
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
        assert!(
            unsafe { peek_handle_kind(&ds) }.is_none(),
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
        assert!(
            unsafe { peek_handle_kind(&is) }.is_none(),
            "a rejected inflateInit2_ must install no handle"
        );
    }

    // `inflateBackInit_` is raw-only, so it narrows to `8..=15` and additionally
    // requires a caller-supplied window.
    for window_bits in [0, 7, 16, 48, -9] {
        let mut is = zeroed_stream();
        // SAFETY: `window` is a live 512-byte buffer that outlives the call, and
        // the shim rejects on `windowBits` before touching it.
        let ret =
            unsafe { inflateBackInit_(&mut is, window_bits, window.as_mut_ptr(), good, size) };
        assert_eq!(
            ret, Z_STREAM_ERROR,
            "inflateBackInit_ must reject windowBits={window_bits}"
        );
    }
    let mut is = zeroed_stream();
    // SAFETY: a null `window` is a defined, rejected argument — the shim checks
    // the pointer rather than dereferencing it.
    assert_eq!(
        unsafe { inflateBackInit_(&mut is, 9, ptr::null_mut(), good, size) },
        Z_STREAM_ERROR,
        "inflateBackInit_ must reject a null window"
    );
    // SAFETY: `is` is a valid `z_stream` whose `state` must still be null.
    assert!(
        unsafe { peek_handle_kind(&is) }.is_none(),
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

    // SAFETY: every one of these shims checks its `z_streamp` for null before
    // any dereference, so a null argument is a defined, rejected call.
    unsafe {
        assert_eq!(
            deflate(no_stream, Z_FINISH),
            Z_STREAM_ERROR,
            "deflate must reject a null stream"
        );
        assert_eq!(
            inflate(no_stream, Z_FINISH),
            Z_STREAM_ERROR,
            "inflate must reject a null stream"
        );
        assert_eq!(
            deflateEnd(no_stream),
            Z_STREAM_ERROR,
            "deflateEnd must reject a null stream"
        );
        assert_eq!(
            inflateEnd(no_stream),
            Z_STREAM_ERROR,
            "inflateEnd must reject a null stream"
        );
        assert_eq!(
            inflateBackEnd(no_stream),
            Z_STREAM_ERROR,
            "inflateBackEnd must reject a null stream"
        );
    }

    let mut stateless = zeroed_stream();
    let mut out = [0u8; 32];
    stateless.next_out = out.as_mut_ptr();
    stateless.avail_out = out.len() as c_uint;
    // SAFETY: `stateless` is a valid `z_stream` whose `state` is null, so the
    // tag read reports no handle without dereferencing anything.
    assert!(
        unsafe { peek_handle_kind(&stateless) }.is_none(),
        "a never-initialised stream carries no handle tag"
    );
    // SAFETY: `stateless` is a valid `z_stream` with a live output window and an
    // empty input window; each shim finds a null `state`, so it returns
    // `Z_STREAM_ERROR` without reinterpreting the pointer.
    unsafe {
        assert_eq!(
            deflate(&mut stateless, Z_NO_FLUSH),
            Z_STREAM_ERROR,
            "deflate must reject a stream with no engine state"
        );
        assert_eq!(
            inflate(&mut stateless, Z_NO_FLUSH),
            Z_STREAM_ERROR,
            "inflate must reject a stream with no engine state"
        );
        assert_eq!(
            deflateEnd(&mut stateless),
            Z_STREAM_ERROR,
            "deflateEnd must reject a stream with no engine state"
        );
        assert_eq!(
            inflateEnd(&mut stateless),
            Z_STREAM_ERROR,
            "inflateEnd must reject a stream with no engine state"
        );
        assert_eq!(
            inflateBackEnd(&mut stateless),
            Z_STREAM_ERROR,
            "inflateBackEnd must reject a stream with no engine state"
        );
    }
}

/// Drives the `next_*`/`avail_*` entry validation shared by `deflate` and
/// `inflate`.
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
    if unsafe { deflateInit2_(&mut ds, DEFAULT_LEVEL, Z_DEFLATED, 9, 1, 0, good, size) } == Z_OK {
        ds.next_out = ptr::null_mut();
        ds.avail_out = 0;
        // SAFETY: a null `next_out` is a defined, rejected configuration — the
        // shim tests the pointer before bridging it to a slice.
        assert_eq!(
            unsafe { deflate(&mut ds, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "deflate must reject a null next_out"
        );

        ds.next_out = out.as_mut_ptr();
        ds.avail_out = out.len() as c_uint;
        ds.next_in = ptr::null();
        ds.avail_in = 7;
        // SAFETY: a positive `avail_in` with a null `next_in` is likewise
        // rejected before any read; nothing is dereferenced.
        assert_eq!(
            unsafe { deflate(&mut ds, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "deflate must reject a positive avail_in with a null next_in"
        );

        // `avail_in == 0` with a null `next_in` is legal: nothing is read. The
        // outcome is not pinned by the contract, so only membership is asserted.
        ds.avail_in = 0;
        // SAFETY: an empty input window with a live output window is a valid,
        // documented call shape.
        assert_legal_code(
            unsafe { deflate(&mut ds, Z_NO_FLUSH) },
            "deflate with an empty input window",
        );

        // Teardown on every path. The stream is still mid-block, so C's
        // `deflateEnd` reports `Z_DATA_ERROR` here rather than `Z_OK` — a
        // defined outcome either way, so membership is what is asserted, but the
        // handle must be reclaimed regardless.
        // SAFETY: `ds` holds a live deflate handle installed above.
        assert_legal_code(
            unsafe { deflateEnd(&mut ds) },
            "deflateEnd after the deflate buffer probes",
        );
        // SAFETY: `ds` is a valid `z_stream`; its `state` must now be null.
        assert!(
            unsafe { peek_handle_kind(&ds) }.is_none(),
            "deflateEnd must reclaim the handle even when it reports Z_DATA_ERROR"
        );
    }

    let mut is = zeroed_stream();
    // SAFETY: `is` is a valid owned `z_stream` with the expected version pair.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) } == Z_OK {
        is.next_out = ptr::null_mut();
        is.avail_out = 0;
        // SAFETY: a null `next_out` is a defined, rejected configuration.
        assert_eq!(
            unsafe { inflate(&mut is, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "inflate must reject a null next_out"
        );

        is.next_out = out.as_mut_ptr();
        is.avail_out = out.len() as c_uint;
        is.next_in = ptr::null();
        is.avail_in = 7;
        // SAFETY: a positive `avail_in` with a null `next_in` is rejected before
        // any read.
        assert_eq!(
            unsafe { inflate(&mut is, Z_NO_FLUSH) },
            Z_STREAM_ERROR,
            "inflate must reject a positive avail_in with a null next_in"
        );

        is.avail_in = 0;
        // SAFETY: an empty input window with a live output window is valid.
        assert_legal_code(
            unsafe { inflate(&mut is, Z_NO_FLUSH) },
            "inflate with an empty input window",
        );
        // SAFETY: `is` holds a live inflate handle installed above.
        assert_legal_code(
            unsafe { inflateEnd(&mut is) },
            "inflateEnd after the inflate buffer probes",
        );
        // SAFETY: `is` is a valid `z_stream`; its `state` must now be null.
        assert!(
            unsafe { peek_handle_kind(&is) }.is_none(),
            "inflateEnd must reclaim the handle"
        );
    }
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
/// handles of different geometry; every value passed in is a legal one.
fn probe_handle_tag_misuse(window_bits: c_int) {
    let good = version_ok();
    let size = stream_size_ok();

    // ---- A deflate handle rejects both foreign terminators. ----
    let mut ds = zeroed_stream();
    // SAFETY: `ds` is a valid owned `z_stream`; `window_bits` is one of the
    // legal deflate values chosen by the caller and `memLevel = 1` keeps the
    // handle small.
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
        // SAFETY: `ds.state` is a live tagged handle from `deflateInit2_`.
        assert_eq!(
            unsafe { peek_handle_kind(&ds) },
            Some(HandleKind::DEFLATE),
            "deflateInit2_ must install a DEFLATE-tagged handle"
        );
        // SAFETY: both terminators validate the offset-0 tag before reclaiming,
        // so they reject this deflate handle without dropping a wrong-type box.
        unsafe {
            assert_eq!(
                inflateEnd(&mut ds),
                Z_STREAM_ERROR,
                "inflateEnd must reject a deflate handle"
            );
            assert_eq!(
                inflateBackEnd(&mut ds),
                Z_STREAM_ERROR,
                "inflateBackEnd must reject a deflate handle"
            );
        }
        // SAFETY: the rejected calls must have left `state` untouched.
        assert_eq!(
            unsafe { peek_handle_kind(&ds) },
            Some(HandleKind::DEFLATE),
            "a rejected cross-engine End must leave the handle intact"
        );
        // SAFETY: `ds` still holds its live deflate handle.
        assert_eq!(
            unsafe { deflateEnd(&mut ds) },
            Z_OK,
            "deflateEnd must still succeed after the rejected cross-engine calls"
        );
        // SAFETY: `state` is null once reclaimed, so the tag read finds nothing.
        assert!(
            unsafe { peek_handle_kind(&ds) }.is_none(),
            "deflateEnd must clear the state handle"
        );
        // SAFETY: the handle is already gone, so this is the double-`*End` case:
        // a null `state` is rejected rather than freed a second time.
        assert_eq!(
            unsafe { deflateEnd(&mut ds) },
            Z_STREAM_ERROR,
            "a second deflateEnd must be rejected, not repeated"
        );
    }

    // ---- The mirror case: an inflate handle rejects the other two. ----
    let mut is = zeroed_stream();
    // SAFETY: `is` is a valid owned `z_stream` with the expected version pair.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) } == Z_OK {
        // SAFETY: `is.state` is a live tagged handle from `inflateInit2_`.
        assert_eq!(
            unsafe { peek_handle_kind(&is) },
            Some(HandleKind::INFLATE),
            "inflateInit2_ must install an INFLATE-tagged handle"
        );
        // SAFETY: tag-validating terminators; neither may reclaim this handle.
        unsafe {
            assert_eq!(
                deflateEnd(&mut is),
                Z_STREAM_ERROR,
                "deflateEnd must reject an inflate handle"
            );
            assert_eq!(
                inflateBackEnd(&mut is),
                Z_STREAM_ERROR,
                "inflateBackEnd must reject an inflate handle"
            );
        }
        // SAFETY: the rejected calls must have left `state` untouched.
        assert_eq!(
            unsafe { peek_handle_kind(&is) },
            Some(HandleKind::INFLATE),
            "a rejected cross-engine End must leave the inflate handle intact"
        );
        // SAFETY: `is` still holds its live inflate handle.
        assert_eq!(
            unsafe { inflateEnd(&mut is) },
            Z_OK,
            "inflateEnd must still succeed after the rejected cross-engine calls"
        );
        // SAFETY: `state` is null once reclaimed.
        assert!(
            unsafe { peek_handle_kind(&is) }.is_none(),
            "inflateEnd must clear the state handle"
        );
        // SAFETY: the double-`*End` case for inflate.
        assert_eq!(
            unsafe { inflateEnd(&mut is) },
            Z_STREAM_ERROR,
            "a second inflateEnd must be rejected, not repeated"
        );
    }

    // ---- `inflateBack` carries a third, distinct tag. ----
    // Its window is caller-supplied, so the buffer must stay live for the whole
    // handle lifetime; `window` outlives every call below.
    let mut window = [0u8; 1 << 9];
    let mut ib = zeroed_stream();
    // SAFETY: `ib` is a valid owned `z_stream` and `window` addresses exactly
    // `1 << 9` live bytes, matching `windowBits = 9`, for the handle's lifetime.
    if unsafe { inflateBackInit_(&mut ib, 9, window.as_mut_ptr(), good, size) } == Z_OK {
        // SAFETY: `ib.state` is a live tagged handle from `inflateBackInit_`.
        assert_eq!(
            unsafe { peek_handle_kind(&ib) },
            Some(HandleKind::INFLATE_BACK),
            "inflateBackInit_ must install an INFLATE_BACK-tagged handle"
        );
        // SAFETY: `INFLATE_BACK` is deliberately distinct from `INFLATE`, so the
        // plain inflate terminator must reject it too — freeing it as a plain
        // inflate handle would be the layout-mismatched free this tag prevents.
        unsafe {
            assert_eq!(
                inflateEnd(&mut ib),
                Z_STREAM_ERROR,
                "inflateEnd must reject an inflateBack handle"
            );
            assert_eq!(
                deflateEnd(&mut ib),
                Z_STREAM_ERROR,
                "deflateEnd must reject an inflateBack handle"
            );
        }
        // SAFETY: `ib` still holds its live inflateBack handle.
        assert_eq!(
            unsafe { inflateBackEnd(&mut ib) },
            Z_OK,
            "inflateBackEnd must still succeed after the rejected calls"
        );
        // SAFETY: `state` is null once reclaimed.
        assert!(
            unsafe { peek_handle_kind(&ib) }.is_none(),
            "inflateBackEnd must clear the state handle"
        );
    }
}

// ===========================================================================
// N3 — the allocator-hook contract and allocation-failure timing
// ===========================================================================

/// Confirms that a hookless caller gets C's built-in substitution published back
/// onto the stream, and that an active hook is genuinely the engine's allocator,
/// with every block returned through `zfree`.
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
    if unsafe { deflateInit_(&mut hookless, DEFAULT_LEVEL, good, size) } == Z_OK {
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
        assert_eq!(
            unsafe { deflateEnd(&mut hookless) },
            Z_OK,
            "deflateEnd must succeed for a hookless stream"
        );
    }

    // With both hooks installed the engine must carve its buffers from caller
    // memory and hand every block back at `*End`.
    let hooks = HookState::with_budget(usize::MAX);
    let mut ds = zeroed_stream();
    install_hooks(&mut ds, &hooks);
    // SAFETY: `ds` is a valid owned `z_stream` whose `opaque` addresses `hooks`,
    // which outlives every call the engine makes through the hooks below.
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
        assert_eq!(
            hooks.handed_out.get(),
            DEFLATE_INIT_ALLOCATIONS,
            "deflateInit2_ must charge exactly C's allocation count to the caller"
        );
        // SAFETY: `ds` holds a live deflate handle backed by caller memory.
        assert_eq!(
            unsafe { deflateEnd(&mut ds) },
            Z_OK,
            "deflateEnd must succeed for a hook-backed stream"
        );
        hooks.assert_balanced("deflate with an active allocator hook");
    }

    let inflate_hooks = HookState::with_budget(usize::MAX);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &inflate_hooks);
    // SAFETY: as above, for the inflate engine.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) } == Z_OK {
        assert_eq!(
            inflate_hooks.handed_out.get(),
            INFLATE_INIT_ALLOCATIONS,
            "inflateInit2_ must charge exactly C's init allocation count, \
             leaving the window to be allocated lazily"
        );
        // SAFETY: `is` holds a live inflate handle backed by caller memory.
        assert_eq!(
            unsafe { inflateEnd(&mut is) },
            Z_OK,
            "inflateEnd must succeed for a hook-backed stream"
        );
        inflate_hooks.assert_balanced("inflate with an active allocator hook");
    }
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
    assert!(
        unsafe { peek_handle_kind(&ds) }.is_none(),
        "a failed deflateInit2_ must install no handle"
    );
    starved.assert_balanced("deflate init starved of caller memory");

    // The same shortfall applied to inflate: an empty budget fails at init.
    let starved_init = HookState::with_budget(0);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &starved_init);
    // SAFETY: as above, for the inflate state allocation.
    assert_eq!(
        unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) },
        Z_MEM_ERROR,
        "an inflate state allocation that cannot be satisfied must surface \
         Z_MEM_ERROR from inflateInit2_"
    );
    // SAFETY: `is` is a valid `z_stream` that must carry no handle.
    assert!(
        unsafe { peek_handle_kind(&is) }.is_none(),
        "a failed inflateInit2_ must install no handle"
    );
    starved_init.assert_balanced("inflate init starved of caller memory");

    // Build a raw stream whose decode genuinely produces output, so the lazily
    // allocated window is actually needed. A non-empty payload guarantees that;
    // the empty fuzz input, which libFuzzer tries first, falls back to a fixed
    // byte so the probe is productive from a cold corpus.
    let payload: &[u8] = if data.is_empty() {
        b"z"
    } else {
        &data[..data.len().min(256)]
    };
    let mut comp = [0u8; 512];
    let Some(produced) = raw_deflate(payload, &mut comp) else {
        return; // the payload did not finish in one shot; not a bug.
    };

    // Now the timing case that only inflate can show: a budget covering the
    // state object but nothing more initialises cleanly, and `inflate` is where
    // the failure appears.
    let starved_window = HookState::with_budget(INFLATE_INIT_ALLOCATIONS);
    let mut is = zeroed_stream();
    install_hooks(&mut is, &starved_window);
    // SAFETY: `is` is a valid owned `z_stream` whose `opaque` addresses
    // `starved_window`, which outlives every call below.
    if unsafe { inflateInit2_(&mut is, RAW_WINDOW_BITS, good, size) } == Z_OK {
        assert_eq!(
            starved_window.handed_out.get(),
            INFLATE_INIT_ALLOCATIONS,
            "the inflate window must not be charged at init"
        );
        let mut back = [0u8; 384];
        is.next_in = comp.as_ptr();
        is.avail_in = produced as c_uint;
        is.next_out = back.as_mut_ptr();
        is.avail_out = back.len() as c_uint;
        // SAFETY: `next_in`/`next_out` address the live `comp`/`back` buffers
        // with matching `avail_*` counts, and `produced <= comp.len()`.
        let ret = unsafe { inflate(&mut is, Z_NO_FLUSH) };
        if starved_window.refused.get() > 0 {
            // The hook was asked for the window and said no, so the deferred
            // failure must be exactly what came back.
            assert_eq!(
                ret, Z_MEM_ERROR,
                "a refused lazy window allocation must surface Z_MEM_ERROR from \
                 inflate, not from inflateInit2_"
            );
        } else {
            // This decode never needed a window; the outcome is then whatever
            // the stream itself dictates, so only membership is contractual.
            assert_legal_code(ret, "inflate that needed no sliding window");
        }
        // SAFETY: `is` holds a live inflate handle; reclaim it on every path.
        assert_legal_code(
            unsafe { inflateEnd(&mut is) },
            "inflateEnd after the deferred-allocation probe",
        );
        // SAFETY: `is` is a valid `z_stream`; its `state` must now be null.
        assert!(
            unsafe { peek_handle_kind(&is) }.is_none(),
            "inflateEnd must reclaim the handle after a window allocation failure"
        );
        starved_window.assert_balanced("inflate starved of its lazy window");
    }
}

/// Raw-DEFLATE-compresses `payload` into `into` through the C ABI, returning the
/// number of bytes produced, or [`None`] if a single `Z_FINISH` did not complete
/// the stream (which the caller treats as a skip, not a failure).
///
/// Runs on the global allocator so it cannot disturb the hook accounting of the
/// probe that calls it.
fn raw_deflate(payload: &[u8], into: &mut [u8]) -> Option<usize> {
    let mut ds = zeroed_stream();
    // SAFETY: `ds` is a valid owned `z_stream`; `RAW_WINDOW_BITS` and
    // `memLevel = 1` are a legal, deliberately small configuration.
    if unsafe {
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
    } != Z_OK
    {
        return None;
    }
    ds.next_in = payload.as_ptr();
    ds.avail_in = payload.len() as c_uint;
    ds.next_out = into.as_mut_ptr();
    ds.avail_out = into.len() as c_uint;
    // SAFETY: `next_in`/`next_out` address the live `payload`/`into` slices with
    // matching `avail_*` counts.
    let ret = unsafe { deflate(&mut ds, Z_FINISH) };
    let produced = into.len() - ds.avail_out as usize;
    // SAFETY: `ds` holds a live deflate handle; reclaim it before returning on
    // either path.
    unsafe {
        deflateEnd(&mut ds);
    }
    if ret == Z_STREAM_END && produced > 0 {
        Some(produced)
    } else {
        None
    }
}

fuzz_target!(|data: &[u8]| {
    // The round trip runs first, on every input. It keeps its own early returns,
    // which is why it lives in a function: the boundary probes below must run
    // unconditionally rather than be skipped by one of them.
    ffi_round_trip(data);

    // Probe steering comes from the leading input byte, so libFuzzer can reach
    // every shape immediately from an empty corpus instead of having to discover
    // a structured prefix. The payload the round trip compresses is still the
    // whole of `data`, so its behaviour is unchanged.
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

    // N2 — handle tagging and cross-engine `*End` misuse.
    probe_handle_tag_misuse(window_bits);

    // N3 — the allocator-hook contract and allocation-failure timing.
    probe_allocator_balance(window_bits);
    probe_allocator_failure_timing(data);
});
