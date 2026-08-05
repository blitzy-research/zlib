//! Global-heap allocation balance across the FFI engine lifecycle with
//! caller-supplied allocator hooks installed.
//!
//! # What this target exists to prove
//!
//! AAP §0.6.3 states the memory-ownership guarantee of this migration in one
//! sentence: caller-supplied allocation is unified with global allocation behind
//! one owning type, and **"no free path exists to forget"**. AAP §0.1.1 states
//! the objective the guarantee serves — replacing *every* instance of manual C
//! memory management with Rust ownership and borrowing semantics.
//!
//! Neither claim is about the caller's arena alone. A hook-backed engine has its
//! state in the caller's memory, but the small owning handles that keep that
//! state type-erased (`ForeignEngineHome`, `ForeignEngine`, `EngineBox`) are
//! deliberately allocated on the **Rust global heap**, precisely so that they do
//! not become extra `zalloc` requests and disturb C's allocation *count* and
//! failure *timing* (AAP §0.6.5). That design decision creates a second balance
//! obligation, on a second heap, that nothing in the suite used to check:
//!
//! * the caller's `zalloc`/`zfree` pair must balance — every block handed out is
//!   handed back; and
//! * the Rust global heap must balance too — every owning handle allocated during
//!   an engine's life is released when the engine ends.
//!
//! This target checks the **second** obligation, which is the one that failed.
//! `CEngineHome::fill` ended in `core::mem::forget(self)` where `self` is a
//! `Box<CEngineHome<E>>`; forgetting the *box* suppressed the home's destructor
//! (intended, so the caller's region is not freed out from under its new owner)
//! but also permanently leaked the box's own 32-byte cell (`NonNull<E>` 8 +
//! `AllocHook` 24). Every successful `deflateInit2_`, `inflateInit2_`,
//! `inflateBackInit_`, `deflateCopy` and `inflateCopy` on the hook path leaked
//! one such cell, and `deflateEnd`/`inflateEnd` could never reclaim it because
//! ownership had already been forgotten. For a long-running C consumer that
//! supplies `zalloc`/`zfree` — the common pattern for embedded and arena-based
//! integrations — growth was unbounded.
//!
//! # Why the existing gates could not see it
//!
//! * [`zlib_rs::DefaultAllocator`] carries an inactive hook, so the default path
//!   never reaches hook-backed placement at all. Every test that does not install
//!   hooks is structurally blind to this class.
//! * A hook-counter check — "every block handed out was handed back" — cannot see
//!   it either: the leaked cell is *not* a hook allocation. It is a Rust global
//!   allocation made on behalf of the hook path.
//! * The crate's `hook_backed_placement_never_uses_infallible_box_new` is a
//!   **static** source scan. It enforces that the placement path allocates
//!   *fallibly*; it says nothing about deallocation.
//!
//! So the only detector was a sanitizer-instrumented fuzz run, executing weekly.
//! This target makes the property a first-class, blocking, every-commit runtime
//! assertion that needs no sanitizer, no nightly toolchain and no fuzzer.
//!
//! # How the measurement works
//!
//! A counting [`GlobalAlloc`] wraps [`System`] and maintains **thread-local** net
//! byte and block counters. Thread-local rather than global is deliberate: the
//! test harness runs each `#[test]` on its own thread, so process-wide counters
//! would let one test's allocations land inside another's measurement window and
//! make every assertion here nondeterministic. The counters are
//! `const`-initialized `Cell`s with no destructor, so reading them allocates
//! nothing and cannot re-enter the allocator, and every access goes through
//! [`LocalKey::try_with`] so an access during thread teardown degrades to a
//! no-op instead of a panic.
//!
//! Each measurement runs the body **twice**: once to warm up — one-time lazy
//! initialization anywhere beneath the call is legitimately not freed and must
//! not be charged to the engine — and once under the meter. Every buffer the body
//! itself creates is dropped inside the body, so the expected net delta is
//! exactly zero. Not "small", not "bounded": zero. A 32-byte-per-engine leak is
//! then unmissable, and [`the_meter_observes_a_deliberate_leak`] proves the meter
//! is not vacuous by planting a leak of exactly that size and shape.
//!
//! Everything here drives the real `extern "C"` shims in [`zlib_rs::ffi`] with a
//! real caller-supplied `zalloc`/`zfree` pair, so what is measured is the same
//! path a C consumer takes.
//!
//! [`GlobalAlloc`]: std::alloc::GlobalAlloc
//! [`System`]: std::alloc::System
//! [`LocalKey::try_with`]: std::thread::LocalKey::try_with

use core::cell::Cell;
use core::ffi::{c_int, c_uint, c_void};
use core::ptr;
use std::alloc::{GlobalAlloc, Layout, System};

use zlib_rs::ReturnCode;
use zlib_rs::constants::{
    DEF_MEM_LEVEL, MAX_WBITS, Z_DEFAULT_COMPRESSION, Z_DEFAULT_STRATEGY, Z_DEFLATED, Z_FINISH,
    Z_NO_FLUSH,
};
use zlib_rs::ffi::{
    deflate as ffi_deflate, deflateCopy, deflateEnd, deflateInit2_, inflate as ffi_inflate,
    inflateBackEnd, inflateBackInit_, inflateCopy, inflateEnd, inflateInit2_, z_stream,
};

// ===========================================================================
// The meter: a counting global allocator with thread-local net counters
// ===========================================================================

thread_local! {
    /// Net live bytes attributable to this thread: incremented on allocation,
    /// decremented on release, adjusted by the delta on reallocation.
    static NET_BYTES: Cell<isize> = const { Cell::new(0) };
    /// Net live blocks attributable to this thread. Reallocation leaves it
    /// unchanged, because it neither creates nor destroys a block.
    static NET_BLOCKS: Cell<isize> = const { Cell::new(0) };
}

/// Adds `bytes` and `blocks` to this thread's counters, ignoring an access that
/// arrives after the thread's local storage has been torn down.
///
/// `try_with` rather than `with` is what makes this safe to call from inside an
/// allocator: a late allocation during thread teardown returns `Err` instead of
/// panicking, and such an allocation is by definition outside every measurement
/// window, so dropping it on the floor loses nothing.
fn account(bytes: isize, blocks: isize) {
    let _ = NET_BYTES.try_with(|c| c.set(c.get().wrapping_add(bytes)));
    let _ = NET_BLOCKS.try_with(|c| c.set(c.get().wrapping_add(blocks)));
}

/// This thread's current `(net_bytes, net_blocks)` reading.
fn reading() -> (isize, isize) {
    let bytes = NET_BYTES.try_with(Cell::get).unwrap_or(0);
    let blocks = NET_BLOCKS.try_with(Cell::get).unwrap_or(0);
    (bytes, blocks)
}

/// [`System`], instrumented with the thread-local counters above.
///
/// Every method forwards to `System` unchanged and only *observes*: the returned
/// pointers, the alignment, the zeroing behaviour and the failure semantics are
/// all `System`'s, so instrumenting the suite cannot change what it measures.
struct CountingAllocator;

// SAFETY: every method delegates to `System`, which upholds the `GlobalAlloc`
// contract; the only additions are counter updates, which touch no allocator
// state, allocate nothing (the counters are `const`-initialized `Cell`s with no
// destructor) and cannot unwind. A null return from `System` is forwarded
// unchanged and deliberately not counted.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded verbatim from this method's own caller,
        // which the `GlobalAlloc` contract already requires to be valid and
        // non-zero-sized.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            account(layout.size() as isize, 1);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc` — the layout is the caller's, forwarded verbatim.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            account(layout.size() as isize, 1);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        account(-(layout.size() as isize), -1);
        // SAFETY: `ptr` and `layout` are the caller's matching pair, which the
        // `GlobalAlloc` contract requires to describe a live block obtained from
        // this same allocator — i.e. from `System`.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`/`layout` describe a live block from this allocator and
        // `new_size` satisfies the contract's rounding rules; both obligations
        // belong to the caller and are forwarded unchanged.
        let fresh = unsafe { System.realloc(ptr, layout, new_size) };
        if !fresh.is_null() {
            // One block in, one block out: only the byte total moves.
            account(new_size as isize - layout.size() as isize, 0);
        }
        fresh
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Runs `body` twice and reports the `(net_bytes, net_blocks)` the **second**
/// run left behind on this thread.
///
/// The first run absorbs any one-time lazy initialization beneath the call, which
/// is legitimately never freed and must not be charged to the engine. The second
/// run is the measurement, and because `body` drops everything it creates the
/// only honest answer is `(0, 0)`.
fn heap_delta_after_warmup(mut body: impl FnMut()) -> (isize, isize) {
    body();
    let (bytes_before, blocks_before) = reading();
    body();
    let (bytes_after, blocks_after) = reading();
    (bytes_after - bytes_before, blocks_after - blocks_before)
}

/// Asserts that `body` is balanced on the Rust global heap.
///
/// `what` names the lifecycle under test so a failure identifies the leaking
/// entry point rather than merely the fact that something leaked. The reported
/// per-block average is what makes the diagnosis immediate: a 32-byte average is
/// the `CEngineHome` cell, and the block count is the number of engines that
/// leaked one.
fn assert_global_heap_balanced(what: &str, body: impl FnMut()) {
    let (bytes, blocks) = heap_delta_after_warmup(body);
    assert_eq!(
        (bytes, blocks),
        (0, 0),
        "{what}: the Rust global heap must return to its starting balance once the \
         engine has ended, but {bytes} byte(s) in {blocks} block(s) survived \
         ({} byte(s) per block). AAP §0.6.3 requires that no free path exists to \
         forget: an owning handle allocated for a hook-backed engine has to be \
         released when that engine ends, or a C consumer that installs \
         zalloc/zfree and cycles streams grows without bound.",
        if blocks == 0 { 0 } else { bytes / blocks }
    );
}

// ===========================================================================
// The caller-supplied allocator hook
// ===========================================================================

/// Bookkeeping for the caller-supplied `zalloc`/`zfree` pair, reached through
/// [`z_stream::opaque`] rather than through a `static`, so tests running
/// concurrently on separate threads cannot share counters.
struct Hook {
    /// Blocks handed out by [`hook_zalloc`].
    handed_out: Cell<usize>,
    /// Blocks released by [`hook_zfree`].
    released: Cell<usize>,
    /// Payload bytes currently held by the library.
    live_bytes: Cell<usize>,
    /// Remaining payload-byte budget; a request that would exceed it is refused
    /// with null, which is how a C caller's `zalloc` reports out-of-memory.
    budget: Cell<usize>,
}

impl Hook {
    /// A hook with an effectively unbounded budget.
    fn unlimited() -> Self {
        Self::with_budget(usize::MAX)
    }

    /// A hook that refuses any request once `budget` payload bytes are live.
    fn with_budget(budget: usize) -> Self {
        Self {
            handed_out: Cell::new(0),
            released: Cell::new(0),
            live_bytes: Cell::new(0),
            budget: Cell::new(budget),
        }
    }

    /// Installs this hook on `strm`, mirroring what a C caller does before
    /// calling an `*Init*_` entry point.
    fn install(&self, strm: &mut z_stream) {
        strm.zalloc = Some(hook_zalloc);
        strm.zfree = Some(hook_zfree);
        strm.opaque = ptr::from_ref(self).cast_mut().cast::<c_void>();
    }

    /// Asserts the caller's arena is balanced: every block handed out was handed
    /// back, and no payload byte is still held.
    ///
    /// This is the *first* of the two balance obligations described in the module
    /// header. It is asserted alongside the global-heap one in every lifecycle
    /// test so a failure distinguishes which heap leaked.
    fn assert_balanced(&self, what: &str) {
        let handed_out = self.handed_out.get();
        let released = self.released.get();
        assert_eq!(
            released, handed_out,
            "{what}: every block the caller's zalloc handed out must come back \
             through zfree (handed_out={handed_out}, released={released})"
        );
        assert_eq!(
            self.live_bytes.get(),
            0,
            "{what}: the caller's arena must hold no live payload bytes once the \
             engine has ended"
        );
    }

    /// Asserts the hook was actually consulted, so a lifecycle test cannot pass
    /// by never reaching the hook path at all.
    fn assert_used(&self, what: &str) {
        assert!(
            self.handed_out.get() > 0,
            "{what}: the caller's zalloc was never called, so this case did not \
             exercise hook-backed placement and proves nothing"
        );
    }
}

/// Bytes of header each hook block carries so [`hook_zfree`] can rebuild the
/// exact [`Layout`]. It doubles as the block alignment, which is therefore at
/// least `align_of::<usize>()` — suitable for every element type the engines
/// request, and for the engine states themselves.
const HOOK_HEADER: usize = core::mem::size_of::<usize>();

/// C `alloc_func`: hands out a `usize`-headed block while the budget allows and
/// reports out-of-memory with null otherwise.
///
/// # Safety
///
/// `opaque` must be null or address a live [`Hook`] that outlives every call the
/// library makes through this pair. The body cannot panic: an unwind out of a
/// hook would cross the C ABI, so every fallible step returns null instead.
unsafe extern "C" fn hook_zalloc(opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
    if opaque.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: per this function's contract `opaque` addresses the live `Hook`
    // that `Hook::install` placed on the stream, which outlives every call.
    let hook = unsafe { &*opaque.cast::<Hook>() };
    let bytes = (items as usize).saturating_mul(size as usize);
    if bytes == 0 || bytes > hook.budget.get().saturating_sub(hook.live_bytes.get()) {
        return ptr::null_mut();
    }
    let Some(total) = bytes.checked_add(HOOK_HEADER) else {
        return ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, HOOK_HEADER) else {
        return ptr::null_mut();
    };
    // SAFETY: `layout` has a non-zero size, since `total >= HOOK_HEADER > 0`.
    let raw = unsafe { System.alloc(layout) };
    if raw.is_null() {
        return ptr::null_mut();
    }
    account(total as isize, 1);
    // SAFETY: `raw` owns `total >= size_of::<usize>()` bytes aligned for `usize`,
    // so the header write is in bounds and aligned.
    unsafe { raw.cast::<usize>().write(total) };
    hook.handed_out.set(hook.handed_out.get() + 1);
    hook.live_bytes.set(hook.live_bytes.get() + bytes);
    // SAFETY: the payload begins one header inside the same allocation.
    unsafe { raw.add(HOOK_HEADER).cast::<c_void>() }
}

/// C `free_func`: rebuilds the layout from the header, refunds the accounting and
/// releases the block. A null address is a no-op, as `free(NULL)` is in C.
///
/// # Safety
///
/// `opaque` must address the same live [`Hook`] that produced `address`, and
/// `address` must be null or a pointer previously returned by [`hook_zalloc`] and
/// not yet released.
unsafe extern "C" fn hook_zfree(opaque: *mut c_void, address: *mut c_void) {
    if address.is_null() || opaque.is_null() {
        return;
    }
    // SAFETY: `address` came from `hook_zalloc`, so its `usize` header sits in
    // the `HOOK_HEADER` bytes immediately before it, inside the same allocation.
    let raw = unsafe { address.cast::<u8>().sub(HOOK_HEADER) };
    // SAFETY: `raw` addresses the header `hook_zalloc` wrote.
    let total = unsafe { raw.cast::<usize>().read() };
    let Ok(layout) = Layout::from_size_align(total, HOOK_HEADER) else {
        return;
    };
    // SAFETY: per this function's contract `opaque` addresses the live `Hook`.
    let hook = unsafe { &*opaque.cast::<Hook>() };
    hook.released.set(hook.released.get() + 1);
    hook.live_bytes
        .set(hook.live_bytes.get() - (total - HOOK_HEADER));
    account(-(total as isize), -1);
    // SAFETY: `raw`/`layout` are exactly the pair `hook_zalloc` allocated with,
    // and this is that block's single release.
    unsafe { System.dealloc(raw, layout) };
}

// ===========================================================================
// Stream construction helpers
// ===========================================================================

/// An all-zero [`z_stream`]: null pointers, absent hooks, zero counters —
/// the state a C caller starts from. `z_stream` is `#[repr(C)]` and has no
/// `Default`, so it is built field by field.
fn zeroed_stream() -> z_stream {
    z_stream {
        next_in: ptr::null(),
        avail_in: 0,
        total_in: 0,
        next_out: ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: ptr::null_mut(),
        state: ptr::null_mut(),
        zalloc: None,
        zfree: None,
        opaque: ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    }
}

/// The `stream_size` argument every versioned `*Init*_` entry point validates.
fn stream_size() -> c_int {
    core::mem::size_of::<z_stream>() as c_int
}

/// A small, genuinely compressible payload — enough input that `deflate` fills
/// its pending buffer and `inflate` allocates its window, so the lifecycle under
/// test is a real one rather than an init/end pair.
fn payload() -> Vec<u8> {
    let mut data = Vec::with_capacity(4096);
    while data.len() < 4096 {
        data.extend_from_slice(b"zlib-rs allocator balance payload; ");
    }
    data.truncate(4096);
    data
}

/// Compresses `data` with the default configuration through the hook-backed C
/// entry points, asserting each step, and returns the compressed bytes.
///
/// Used both as a lifecycle under measurement and as the producer of the input
/// the inflate lifecycles consume.
fn deflate_round(hook: &Hook, data: &[u8]) -> Vec<u8> {
    let mut strm = zeroed_stream();
    hook.install(&mut strm);
    // SAFETY: `strm` is a valid caller-owned `z_stream` with a live hook pair
    // installed, `c"1"` is a valid version string whose first byte matches the
    // library's major version, and `stream_size()` is the true `sizeof(z_stream)`.
    let init = unsafe {
        deflateInit2_(
            &mut strm,
            Z_DEFAULT_COMPRESSION,
            Z_DEFLATED,
            MAX_WBITS,
            DEF_MEM_LEVEL,
            Z_DEFAULT_STRATEGY,
            c"1".as_ptr(),
            stream_size(),
        )
    };
    assert_eq!(
        init,
        ReturnCode::Ok.as_c_int(),
        "deflateInit2_ must succeed with an unbounded caller allocator"
    );

    let mut out = vec![0u8; 8192];
    strm.next_in = data.as_ptr();
    strm.avail_in = data.len() as c_uint;
    strm.next_out = out.as_mut_ptr();
    strm.avail_out = out.len() as c_uint;
    // SAFETY: `strm` holds a live deflate state and the two buffer windows above
    // are valid for the lengths reported in `avail_in`/`avail_out`.
    let ret = unsafe { ffi_deflate(&mut strm, Z_FINISH) };
    assert_eq!(
        ret,
        ReturnCode::StreamEnd.as_c_int(),
        "deflate(Z_FINISH) must complete the stream in one call for a {}-byte input",
        data.len()
    );
    let produced = out.len() - strm.avail_out as usize;
    out.truncate(produced);

    // SAFETY: `strm` holds a live deflate state that has not yet been ended.
    let end = unsafe { deflateEnd(&mut strm) };
    assert_eq!(end, ReturnCode::Ok.as_c_int(), "deflateEnd must succeed");
    out
}

/// Decompresses `compressed` through the hook-backed C entry points, asserting
/// each step and that the result equals `expected`.
fn inflate_round(hook: &Hook, compressed: &[u8], expected: &[u8]) {
    let mut strm = zeroed_stream();
    hook.install(&mut strm);
    // SAFETY: as in `deflate_round` — a valid caller-owned stream with a live
    // hook pair, a matching version byte and the true stream size.
    let init = unsafe { inflateInit2_(&mut strm, MAX_WBITS, c"1".as_ptr(), stream_size()) };
    assert_eq!(
        init,
        ReturnCode::Ok.as_c_int(),
        "inflateInit2_ must succeed with an unbounded caller allocator"
    );

    let mut out = vec![0u8; expected.len() + 64];
    strm.next_in = compressed.as_ptr();
    strm.avail_in = compressed.len() as c_uint;
    strm.next_out = out.as_mut_ptr();
    strm.avail_out = out.len() as c_uint;
    // SAFETY: `strm` holds a live inflate state and both buffer windows are valid
    // for the lengths reported to the engine.
    let ret = unsafe { ffi_inflate(&mut strm, Z_FINISH) };
    assert_eq!(
        ret,
        ReturnCode::StreamEnd.as_c_int(),
        "inflate(Z_FINISH) must reach the end of the stream"
    );
    let produced = out.len() - strm.avail_out as usize;
    assert_eq!(
        &out[..produced],
        expected,
        "the decompressed bytes must equal the original input"
    );

    // SAFETY: `strm` holds a live inflate state that has not yet been ended.
    let end = unsafe { inflateEnd(&mut strm) };
    assert_eq!(end, ReturnCode::Ok.as_c_int(), "inflateEnd must succeed");
}

// ===========================================================================
// Meter self-check — the gate must not be vacuous
// ===========================================================================

/// The meter observes a deliberate leak of exactly the defect's size and shape.
///
/// Without this, every assertion in this file could pass because the meter never
/// counts anything. It plants one 32-byte allocation — the size of the
/// `CEngineHome` cell that leaked — confirms the meter reports exactly
/// `(32, 1)`, and then reclaims it so the process stays clean.
#[test]
fn the_meter_observes_a_deliberate_leak() {
    /// Same shape as the cell that leaked: one pointer plus a three-word hook.
    struct Cell32 {
        _ptr: *mut u8,
        _zalloc: Option<unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void>,
        _zfree: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
        _opaque: *mut c_void,
    }
    assert_eq!(
        core::mem::size_of::<Cell32>(),
        32,
        "the probe must mirror the 32-byte cell the defect leaked"
    );

    let (before_bytes, before_blocks) = reading();
    let leaked = Box::into_raw(Box::new(Cell32 {
        _ptr: ptr::null_mut(),
        _zalloc: Some(hook_zalloc),
        _zfree: Some(hook_zfree),
        _opaque: ptr::null_mut(),
    }));
    let (during_bytes, during_blocks) = reading();
    assert_eq!(
        (during_bytes - before_bytes, during_blocks - before_blocks),
        (32, 1),
        "the meter must observe a 32-byte, one-block leak; if it cannot see this, \
         every balance assertion in this file is vacuous"
    );

    // SAFETY: `leaked` came from `Box::into_raw` on this same thread and has not
    // been reclaimed, so rebuilding the box is its single, matching release.
    drop(unsafe { Box::from_raw(leaked) });
    let (after_bytes, after_blocks) = reading();
    assert_eq!(
        (after_bytes - before_bytes, after_blocks - before_blocks),
        (0, 0),
        "reclaiming the planted allocation must return the meter to its baseline"
    );
}

// ===========================================================================
// One lifecycle per hook-backed entry point
// ===========================================================================

/// `deflateInit2_` → `deflate(Z_FINISH)` → `deflateEnd` leaks nothing.
#[test]
fn deflate_lifecycle_leaves_both_heaps_balanced() {
    let data = payload();
    let mut compressed_len = 0usize;
    assert_global_heap_balanced("deflateInit2_/deflate/deflateEnd", || {
        let hook = Hook::unlimited();
        compressed_len = deflate_round(&hook, &data).len();
        hook.assert_used("deflateInit2_/deflate/deflateEnd");
        hook.assert_balanced("deflateInit2_/deflate/deflateEnd");
    });
    assert!(
        compressed_len > 0 && compressed_len < data.len(),
        "the measured lifecycle must have actually compressed something \
         ({compressed_len} bytes from {})",
        data.len()
    );
}

/// `inflateInit2_` → `inflate(Z_FINISH)` → `inflateEnd` leaks nothing.
///
/// This is the entry point whose leak the fuzzer reported, reached through
/// `ZStream::take` → `try_reserve_engine` → `CEngineHome::fill`.
#[test]
fn inflate_lifecycle_leaves_both_heaps_balanced() {
    let data = payload();
    let compressed = deflate_round(&Hook::unlimited(), &data);
    assert_global_heap_balanced("inflateInit2_/inflate/inflateEnd", || {
        let hook = Hook::unlimited();
        inflate_round(&hook, &compressed, &data);
        hook.assert_used("inflateInit2_/inflate/inflateEnd");
        hook.assert_balanced("inflateInit2_/inflate/inflateEnd");
    });
}

/// `inflateBackInit_` → `inflateBackEnd` leaks nothing.
///
/// `inflateBack` takes its window from the caller, so the state reservation is
/// the *only* thing this pair charges the hook for — which makes it the tightest
/// possible test of hook-backed placement.
#[test]
fn inflate_back_lifecycle_leaves_both_heaps_balanced() {
    assert_global_heap_balanced("inflateBackInit_/inflateBackEnd", || {
        let hook = Hook::unlimited();
        let mut window = vec![0u8; 1 << 15];
        let mut strm = zeroed_stream();
        hook.install(&mut strm);
        // SAFETY: `strm` is a valid caller-owned stream with a live hook pair,
        // `window` is `1 << 15` writable bytes matching the requested
        // `windowBits`, the version byte matches and the size is exact.
        let init = unsafe {
            inflateBackInit_(
                &mut strm,
                15,
                window.as_mut_ptr(),
                c"1".as_ptr(),
                stream_size(),
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "inflateBackInit_ must succeed with an unbounded caller allocator"
        );
        // SAFETY: `strm` holds a live inflateBack state that has not been ended.
        let end = unsafe { inflateBackEnd(&mut strm) };
        assert_eq!(
            end,
            ReturnCode::Ok.as_c_int(),
            "inflateBackEnd must succeed"
        );
        hook.assert_used("inflateBackInit_/inflateBackEnd");
        hook.assert_balanced("inflateBackInit_/inflateBackEnd");
    });
}

/// `deflateCopy` allocates a second engine, and ending both leaks nothing.
///
/// AAP §0.6.5 requires the copy to route through the *same* hook, so this
/// lifecycle charges the caller twice and must balance twice.
#[test]
fn deflate_copy_lifecycle_leaves_both_heaps_balanced() {
    let data = payload();
    assert_global_heap_balanced("deflateCopy", || {
        let hook = Hook::unlimited();
        let mut source = zeroed_stream();
        hook.install(&mut source);
        // SAFETY: a valid caller-owned stream with a live hook pair, a matching
        // version byte and the true stream size.
        let init = unsafe {
            deflateInit2_(
                &mut source,
                Z_DEFAULT_COMPRESSION,
                Z_DEFLATED,
                MAX_WBITS,
                DEF_MEM_LEVEL,
                Z_DEFAULT_STRATEGY,
                c"1".as_ptr(),
                stream_size(),
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "deflateInit2_ must succeed"
        );

        // Put some history into the source so the copy has real state to carry.
        let mut scratch = vec![0u8; 8192];
        source.next_in = data.as_ptr();
        source.avail_in = data.len() as c_uint;
        source.next_out = scratch.as_mut_ptr();
        source.avail_out = scratch.len() as c_uint;
        // SAFETY: `source` holds a live deflate state and both windows are valid.
        let ret = unsafe { ffi_deflate(&mut source, Z_NO_FLUSH) };
        assert_eq!(
            ret,
            ReturnCode::Ok.as_c_int(),
            "a Z_NO_FLUSH deflate must make progress without ending the stream"
        );

        let mut dest = zeroed_stream();
        // SAFETY: `dest` is a valid, caller-owned zeroed stream and `source`
        // holds a live deflate state; `deflateCopy` reads one and initializes the
        // other.
        let copied = unsafe { deflateCopy(&mut dest, &mut source) };
        assert_eq!(
            copied,
            ReturnCode::Ok.as_c_int(),
            "deflateCopy must succeed with an unbounded caller allocator"
        );

        // SAFETY: both streams hold live, distinct deflate states.
        let end_dest = unsafe { deflateEnd(&mut dest) };
        // SAFETY: as above.
        let end_source = unsafe { deflateEnd(&mut source) };
        // Both streams are mid-compression — `Z_NO_FLUSH` left them in the busy
        // state and neither was finished — so C's
        // `return status == BUSY_STATE ? Z_DATA_ERROR : Z_OK` (deflate.c, tail of
        // `deflateEnd`) makes `Z_DATA_ERROR` the *correct* answer for both, and
        // this port reproduces it. `Z_DATA_ERROR` from `deflateEnd` still means
        // the state was released, which is exactly what the surrounding balance
        // assertion goes on to check.
        assert_eq!(
            (end_dest, end_source),
            (
                ReturnCode::DataError.as_c_int(),
                ReturnCode::DataError.as_c_int()
            ),
            "ending a copy and an original that are both still busy must report \
             Z_DATA_ERROR on each, per C's deflateEnd status contract, while \
             still releasing both states"
        );
        hook.assert_used("deflateCopy");
        hook.assert_balanced("deflateCopy");
    });
}

/// `inflateCopy` allocates a second engine, and ending both leaks nothing.
#[test]
fn inflate_copy_lifecycle_leaves_both_heaps_balanced() {
    let data = payload();
    let compressed = deflate_round(&Hook::unlimited(), &data);
    assert_global_heap_balanced("inflateCopy", || {
        let hook = Hook::unlimited();
        let mut source = zeroed_stream();
        hook.install(&mut source);
        // SAFETY: a valid caller-owned stream with a live hook pair, a matching
        // version byte and the true stream size.
        let init = unsafe { inflateInit2_(&mut source, MAX_WBITS, c"1".as_ptr(), stream_size()) };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "inflateInit2_ must succeed"
        );

        // Decode a prefix so the source owns a window before it is copied.
        let mut scratch = vec![0u8; 1024];
        source.next_in = compressed.as_ptr();
        source.avail_in = compressed.len() as c_uint;
        source.next_out = scratch.as_mut_ptr();
        source.avail_out = scratch.len() as c_uint;
        // SAFETY: `source` holds a live inflate state and both windows are valid.
        let ret = unsafe { ffi_inflate(&mut source, Z_NO_FLUSH) };
        assert!(
            ret == ReturnCode::Ok.as_c_int() || ret == ReturnCode::StreamEnd.as_c_int(),
            "a prefix decode must return Z_OK or Z_STREAM_END, got {ret}"
        );

        let mut dest = zeroed_stream();
        // SAFETY: `dest` is a valid caller-owned zeroed stream and `source` holds
        // a live inflate state.
        let copied = unsafe { inflateCopy(&mut dest, &mut source) };
        assert_eq!(
            copied,
            ReturnCode::Ok.as_c_int(),
            "inflateCopy must succeed with an unbounded caller allocator"
        );

        // SAFETY: both streams hold live, distinct inflate states.
        let end_dest = unsafe { inflateEnd(&mut dest) };
        // SAFETY: as above.
        let end_source = unsafe { inflateEnd(&mut source) };
        assert_eq!(
            (end_dest, end_source),
            (ReturnCode::Ok.as_c_int(), ReturnCode::Ok.as_c_int()),
            "ending both the copy and the original must succeed"
        );
        hook.assert_used("inflateCopy");
        hook.assert_balanced("inflateCopy");
    });
}

// ===========================================================================
// The property a single lifecycle cannot show: repetition must not grow
// ===========================================================================

/// Cycling engines does not grow the global heap — the unbounded-growth claim.
///
/// A single leaked cell is 32 bytes, which is easy to dismiss. What made the
/// defect serious is that it recurred per engine, so a long-running C consumer
/// that supplies `zalloc`/`zfree` and cycles streams grew without bound. This
/// runs sixteen full mixed lifecycles under one meter reading: a per-engine leak
/// shows up multiplied, and the per-block average in the failure message names
/// the leaking allocation directly.
#[test]
fn repeated_engine_cycles_do_not_grow_the_global_heap() {
    const CYCLES: usize = 16;
    let data = payload();
    let compressed = deflate_round(&Hook::unlimited(), &data);

    assert_global_heap_balanced("sixteen mixed engine cycles", || {
        for _ in 0..CYCLES {
            let hook = Hook::unlimited();
            let round = deflate_round(&hook, &data);
            assert!(!round.is_empty(), "each cycle must produce output");
            inflate_round(&hook, &compressed, &data);
            hook.assert_balanced("sixteen mixed engine cycles");
        }
    });
}

// ===========================================================================
// Failure paths: a refused allocation must leak nothing either
// ===========================================================================

/// A refused state reservation reports `Z_MEM_ERROR` and leaks nothing.
///
/// The recovery path is the mirror image of the success path — the reservation is
/// still owned, so it goes back through the caller's `zfree` — and it has its own
/// owning handles to release. AAP §0.6.5 requires the refusal to be *reported*
/// rather than fatal, so this also pins that the boundary never aborts here.
#[test]
fn a_refused_state_reservation_leaks_nothing() {
    assert_global_heap_balanced("refused state reservation", || {
        // Far below any engine state, so the very first request fails.
        let hook = Hook::with_budget(64);
        let mut strm = zeroed_stream();
        hook.install(&mut strm);
        // SAFETY: a valid caller-owned stream with a live (deliberately starved)
        // hook pair, a matching version byte and the true stream size.
        let init = unsafe { inflateInit2_(&mut strm, MAX_WBITS, c"1".as_ptr(), stream_size()) };
        assert_eq!(
            init,
            ReturnCode::MemError.as_c_int(),
            "a budget below the state size must be reported as Z_MEM_ERROR"
        );
        assert!(
            strm.state.is_null(),
            "a failed init must leave no state installed"
        );
        hook.assert_balanced("refused state reservation");
    });
}

/// A refused *window* allocation — after a successful init — leaks nothing.
///
/// Here the state reservation succeeded, so the hook-backed owning handles do
/// exist by the time the failure happens; the teardown that follows
/// `inflateEnd` has to release them all.
#[test]
fn a_refused_window_allocation_leaks_nothing() {
    // The inflate state reservation the caller is charged at init, plus one byte
    // short of the raw 8-bit window (`1 << 8`) that `inflate` allocates lazily —
    // the same split `tests/inflate_coverage.rs::mem_limit_forces_mem_error`
    // uses to pin the failure timing itself.
    //
    // The charge is C's `sizeof(struct inflate_state)` — what
    // `InflateState::C_LAYOUT_SIZE` computes from the field-exact `#[repr(C)]`
    // mirror — and deliberately *not* `size_of::<InflateState>()`. An allocator
    // sized from C's header must serve this port exactly as it serves reference
    // zlib, so the request the hook sees is shaped `(1, C_STATE_SIZE)`. The two
    // numbers differ (the Rust struct carries owned-buffer handles C keeps as
    // bare pointers), and budgeting from the Rust size would leave the window
    // enough headroom to succeed — which is the whole point this test pins.
    let state_size = zlib_rs::inflate::InflateState::C_LAYOUT_SIZE;
    assert_global_heap_balanced("refused window allocation", || {
        let hook = Hook::with_budget(state_size + (1 << 8) - 1);
        let mut strm = zeroed_stream();
        hook.install(&mut strm);
        // SAFETY: a valid caller-owned stream with a live capped hook pair, a
        // matching version byte and the true stream size.
        let init = unsafe { inflateInit2_(&mut strm, -8, c"1".as_ptr(), stream_size()) };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "a budget that fits the state must let inflateInit2_ succeed"
        );

        // The minimal raw-DEFLATE fragment that drives the engine to grow its
        // window (mirrors the `\x63\x00` feed in `infcover.c`'s mem coverage).
        let input = [0x63u8, 0x00];
        let mut out = [0u8; 1];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: `strm` holds a live inflate state and both windows are valid.
        let ret = unsafe { ffi_inflate(&mut strm, Z_NO_FLUSH) };
        assert_eq!(
            ret,
            ReturnCode::MemError.as_c_int(),
            "a window allocation the caller refuses must surface as Z_MEM_ERROR"
        );

        // SAFETY: `strm` still holds the live inflate state built at init.
        let end = unsafe { inflateEnd(&mut strm) };
        assert_eq!(
            end,
            ReturnCode::Ok.as_c_int(),
            "inflateEnd must still release a stream whose window allocation failed"
        );
        hook.assert_used("refused window allocation");
        hook.assert_balanced("refused window allocation");
    });
}

// ===========================================================================
// The hook-free path, for contrast
// ===========================================================================

/// The default (no-hook) path is balanced too.
///
/// This is the path all the other suites exercise. It is included so that a
/// future regression can be localized: if this fails as well, the defect is in
/// ownership generally rather than in hook-backed placement specifically.
#[test]
fn the_hook_free_lifecycle_leaves_the_global_heap_balanced() {
    let data = payload();
    assert_global_heap_balanced("default allocator lifecycle", || {
        let mut strm = zeroed_stream();
        // SAFETY: a valid caller-owned stream with no hooks installed (the
        // library substitutes its own built-ins), a matching version byte and the
        // true stream size.
        let init = unsafe {
            deflateInit2_(
                &mut strm,
                Z_DEFAULT_COMPRESSION,
                Z_DEFLATED,
                MAX_WBITS,
                DEF_MEM_LEVEL,
                Z_DEFAULT_STRATEGY,
                c"1".as_ptr(),
                stream_size(),
            )
        };
        assert_eq!(
            init,
            ReturnCode::Ok.as_c_int(),
            "deflateInit2_ must succeed"
        );

        let mut out = vec![0u8; 8192];
        strm.next_in = data.as_ptr();
        strm.avail_in = data.len() as c_uint;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as c_uint;
        // SAFETY: `strm` holds a live deflate state and both windows are valid.
        let ret = unsafe { ffi_deflate(&mut strm, Z_FINISH) };
        assert_eq!(
            ret,
            ReturnCode::StreamEnd.as_c_int(),
            "deflate(Z_FINISH) must complete the stream in one call"
        );
        // SAFETY: `strm` holds a live deflate state that has not been ended.
        let end = unsafe { deflateEnd(&mut strm) };
        assert_eq!(end, ReturnCode::Ok.as_c_int(), "deflateEnd must succeed");
    });
}
