//! Caller-allocator (`zalloc`/`zfree`) buffer bridge — the sanctioned home of
//! the raw-pointer allocation hook.
//!
//! # Why this module exists
//!
//! zlib lets a C caller override allocation through the `z_stream`
//! `zalloc`/`zfree`/`opaque` triple (`zlib.h` L85-L86; AAP §0.6.3). Honoring that
//! contract requires calling a raw C function pointer, materializing the returned
//! raw pointer as a slice, and releasing it through the matching `zfree` on drop
//! — all `unsafe` operations.
//!
//! Per the migration's unsafe-isolation strategy (AAP §0.6.2 / §0.7.2 standard
//! S2), crate `unsafe` is confined to `src/ffi/**` plus the private no-`std`
//! runtime-support block in `src/lib.rs` (the libc-backed global allocator and
//! abort panic handler), and is absent from every compression and decompression
//! module. **All allocator-hook `unsafe` is confined to this file**: it defines
//! [`CForeignBuffer`], the sole implementor of the safe [`ForeignBuffer`]
//! interface consumed by [`crate::stream::AllocBuffer`]. Because `stream.rs` and
//! the compression / decompression engines see only the safe [`ForeignBuffer`]
//! methods, they contain no executable `unsafe` — the raw-pointer work lives
//! exclusively here.
//!
//! # Trusting the hook
//!
//! Every `unsafe` operation below is justified against the construction contract
//! documented on [`crate::stream::AllocHook::new`], which is crate-private
//! precisely so that contract is enforceable. Its four clauses — callable
//! pointers, zlib `zalloc` return semantics, a matching `zfree`, and an `opaque`
//! that outlives every derived buffer — are the premises this module's `SAFETY`
//! comments cite. The only construction site is `src/ffi/types.rs`, converting a
//! `z_stream` whose validity the calling FFI entry point has already established
//! from its own `# Safety` contract. Safe code outside the crate can obtain only
//! [`crate::stream::AllocHook::none`], which cannot reach any code here.
//!
//! # Fallible allocation
//!
//! [`try_alloc_foreign`] returns [`None`] when the caller's `zalloc` reports
//! out-of-memory, when the request cannot be described by a valid Rust
//! [`Layout`], or when its size is unrepresentable in the C `uInt` hook ABI. It
//! never silently falls back to the global allocator; that choice is what lets
//! the deflate/inflate initialization paths surface `Z_MEM_ERROR` exactly as the
//! C library does — preserving C's allocation count and failure timing
//! (AAP §0.6.5) — rather than masking an OOM from a caller who deliberately
//! installed a bounded allocator.
//! [`try_box`] extends the same discipline to the one remaining global
//! allocation (the boxed engine state), so heap exhaustion there is reported as
//! `Z_MEM_ERROR` instead of aborting the process.
//!
//! # `ZALLOC` geometry
//!
//! C never flattens an allocation request: `ZALLOC(strm, items, size)` forwards
//! the caller's own `(items, size)` pair, and reference zlib deliberately picks
//! pairs that describe the *logical* shape of each buffer — for example
//! `ZALLOC(strm, s->w_size, 2 * sizeof(Byte))` for the doubled sliding window
//! (`deflate.c` L458) and `ZALLOC(strm, s->lit_bufsize, LIT_BUFS)` for the
//! pending buffer (`deflate.c` L505). A caller whose allocator pools by size
//! class, budgets per request, or logs its inputs can observe that pair, so
//! [`try_alloc_foreign_items`] takes `items` and `item_size` separately and
//! passes them through untouched. The element count materialized as a slice is
//! derived from the byte product, which is why `items * item_size` must be an
//! exact multiple of `size_of::<T>()`.
//!
//! # Element initialization
//!
//! C `zcalloc` zero-fills after `zalloc`. Zero-filling alone is *not* a sound
//! initialization for an arbitrary `T: Copy + Default` — an all-zero bit pattern
//! need not be a valid value of `T`, let alone equal to `T::default()`. Every
//! element is therefore written with [`Default::default`] before the region is
//! ever exposed as `&[T]`, which is both a valid initialization for any `T` and
//! byte-for-byte identical to C's zero fill for the integer element types the
//! engines actually request.
//!
//!
//! # Validity of the returned region
//!
//! The element bound is `T: Copy + Default`, which on its own guarantees neither
//! that an all-zero bit pattern is a *valid value* of `T` nor that a
//! hook-returned address satisfies `T`'s layout requirements. This module
//! therefore establishes both properties itself before any `&[T]` can exist:
//!
//! * the request is validated through [`Layout::array::<T>`], which rejects a
//!   `count * size_of::<T>()` product that overflows or exceeds `isize::MAX` —
//!   precisely the precondition [`core::slice::from_raw_parts`] imposes;
//! * a zero-sized `T` is rejected outright (it has no C-representable
//!   footprint; the sole caller serves such requests from the global allocator);
//! * the address returned by `zalloc` is checked against `Layout::align`, and a
//!   region that does not satisfy it is handed straight back through the
//!   caller's `zfree` and reported as an allocation failure; and
//! * every element is initialised by *writing a valid `T::default()` value*
//!   rather than by zeroing raw bytes, so the region holds `count` valid `T`s
//!   for **any** `T: Copy + Default`. For the integer buffer types the engines
//!   actually request (`u8` and `u16`) `T::default()` is `0`, so the fill is
//!   byte-for-byte the `zmemzero` that C `zcalloc` performs.

use alloc::boxed::Box;
use core::alloc::Layout;
use core::ffi::{c_uint, c_void};
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::stream::{
    AllocHook, FallibleBoxAlloc, ForeignAlloc, ForeignBuffer, ForeignEngine, ForeignEngineHome,
    ForeignEnginePlace, ZeroValid,
};

/// A working buffer backed by a caller-supplied C `zalloc`/`zfree` pair.
///
/// This is the concrete, `ffi`-local implementor of [`ForeignBuffer`]. It owns a
/// non-null region returned by the hook's `zalloc` and releases it through the
/// same hook's `zfree` on [`Drop`]. All of the raw-pointer `unsafe` — the slice
/// materialization in [`as_slice`](ForeignBuffer::as_slice) /
/// [`as_mut_slice`](ForeignBuffer::as_mut_slice) and the `zfree` in [`Drop`] —
/// is contained in this type so the safe core never touches it (AAP §0.6.2).
///
/// # Type invariants
///
/// Established once by [`try_alloc_foreign_items`] and relied upon by every
/// `unsafe` block in this file:
///
/// * `ptr` is non-null, aligned for `T` (checked against `Layout::align`), and
///   was returned by `hook`'s `zalloc`;
/// * it addresses exactly `len` **initialized** `T` values (initialization is
///   performed there before the buffer is constructed);
/// * `len * size_of::<T>()` is at most `isize::MAX`, so the region is a valid
///   Rust slice length;
/// * `items * item_size == len * size_of::<T>()`, so a clone can reproduce the
///   caller's original `ZALLOC(strm, items, size)` pair exactly;
/// * `hook` is the same hook the region came from, is still
///   [active](AllocHook::is_active), and satisfies the construction contract on
///   [`crate::stream::AllocHook::new`].
struct CForeignBuffer<T: Copy + Default + ZeroValid> {
    /// Non-null, `T`-aligned pointer to `len` initialized `T`s obtained from the
    /// hook's `zalloc` and element-wise filled with valid `T::default()` values
    /// by [`try_alloc_foreign_items`].
    ptr: NonNull<T>,
    /// Element count (not bytes).
    len: usize,
    /// The `items` argument this region was requested with, retained so
    /// [`clone_foreign`](ForeignBuffer::clone_foreign) reproduces the caller's
    /// original `ZALLOC(strm, items, size)` pair rather than a re-derived one.
    items: usize,
    /// The `size` argument this region was requested with (see
    /// [`items`](Self::items)).
    item_size: usize,
    /// The hook whose `zfree` releases [`ptr`](Self::ptr) (via its `opaque`).
    hook: AllocHook,
}

impl<T: Copy + Default + ZeroValid + 'static> ForeignBuffer<T> for CForeignBuffer<T> {
    #[inline]
    fn as_slice(&self) -> &[T] {
        // SAFETY: every `from_raw_parts` precondition is established at
        // construction time by `try_alloc_foreign_items`, which is the only producer of
        // this type:
        //  * `ptr` is non-null (checked with `NonNull::new`) and correctly
        //    aligned for `T` (explicitly checked; a misaligned region is freed
        //    and reported as an allocation failure);
        //  * it addresses `len` contiguous `T`s whose total size is
        //    `Layout::array::<T>(len)`, validated there and therefore no larger
        //    than `isize::MAX`;
        //  * all `len` elements are initialized there by writing a valid
        //    `T::default()` value into every slot, so no all-zero-is-valid
        //    assumption is relied on (the sealed `T: ZeroValid` bound is an
        //    independent compile-time restriction of the element-type set);
        //  * the region stays valid and exclusively owned by `self` until this
        //    buffer's `Drop`, and nothing mutates it for the lifetime of the
        //    returned borrow, so a shared slice for the `&self` borrow is sound.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: identical validity, alignment, size and initialization
        // reasoning as `as_slice` (established by `try_alloc_foreign`), and the
        // `&mut self` borrow additionally guarantees no other reference to the
        // region exists, so a unique slice over the owned region is sound.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    fn clone_foreign(&self) -> Option<Box<dyn ForeignBuffer<T>>> {
        // Allocate a fresh foreign region through the SAME hook (matching C
        // `deflateCopy`, which `ZALLOC`s new buffers) with the SAME `(items,
        // size)` pair the original was requested with, and copy the contents in.
        // On OOM this returns `None`, which `AllocBuffer::try_clone` propagates
        // verbatim so `deflateCopy`/`inflateCopy` surface `Z_MEM_ERROR`. There is
        // deliberately NO global-allocator fallback: a caller who installed a
        // bounded arena must observe the failure rather than silently receive a
        // copy in the global heap (AAP §0.6.3, §0.6.5).
        let mut fresh = try_alloc_foreign_items::<T>(self.hook, self.items, self.item_size)?;
        fresh.as_mut_slice().copy_from_slice(self.as_slice());
        Some(fresh)
    }
}

impl<T: Copy + Default + ZeroValid> Drop for CForeignBuffer<T> {
    fn drop(&mut self) {
        if let Some(zfree) = self.hook.zfree() {
            // SAFETY: `ptr` was returned by this same hook's `zalloc` and has not
            // been freed before — this buffer is its unique owner and `Drop` runs
            // once. Hook contract clauses 3 and 4 guarantee that `zfree` is the
            // deallocator paired with that `zalloc` and that `opaque` is still
            // valid for it now, at the end of the buffer's life. The call is
            // exactly zlib's `ZFREE(strm, addr)` => `(*zfree)(opaque, addr)`.
            unsafe { zfree(self.hook.opaque(), self.ptr.as_ptr() as *mut c_void) };
        }
    }
}

/// A byte buffer the **caller** owns, lent to the engine for the lifetime of one
/// `inflateBack` session.
///
/// # Why this exists
///
/// `inflateBackInit_` is the one zlib entry point whose window is supplied *by
/// the caller* rather than allocated by the library:
///
/// ```text
/// state = ZALLOC(strm, 1, sizeof(struct inflate_state));  /* infback.c L51 */
/// if (state == Z_NULL) return Z_MEM_ERROR;
/// ...
/// state->window = window;                                 /* infback.c L60 */
/// ```
///
/// and `inflateBackEnd` frees only the state (`infback.c` L572-L577) — never the
/// window. A C caller therefore observes exactly **one** `zalloc`, keeps using
/// its own buffer afterwards, and is entitled to place that buffer wherever it
/// likes (a static array, a stack frame, a memory-mapped region). Allocating a
/// replacement window would break all three of those properties at once, which
/// is why this path must never allocate one.
///
/// This type presents the caller's region through the same safe
/// [`ForeignBuffer`] interface [`crate::stream::AllocBuffer::Foreign`] uses, so
/// the decoder reads and writes it as an ordinary slice with no `unsafe` outside
/// this file, while [`Drop`] is **absent** — the region is not ours to release.
///
/// # Type invariants
///
/// Established once by [`borrow_caller_window`], the only producer:
///
/// * `ptr` is non-null and `len` is non-zero;
/// * the region addresses exactly `len` bytes that are valid to read and write.
///   They are **not** initialized by this crate — `borrow_caller_window`
///   deliberately writes nothing, because C's `state->window = window;`
///   (`infback.c` L60) is a bare store. Forming slices over them is still sound:
///   `u8` has no invalid bit pattern and no niche, so whatever the caller left
///   there is a valid inhabitant, and the decoder never *reads* a window byte it
///   has not itself written (`whave` and `wnext` both start at `0` and bound
///   every match copy);
/// * `len <= isize::MAX`, so the region is a valid Rust slice length;
/// * the caller keeps the region valid and grants exclusive access for as long as
///   the `inflateBack` state lives, which is the documented `# Safety` contract of
///   the `inflateBackInit_` shim.
///
/// # Not clonable
///
/// [`clone_foreign`](ForeignBuffer::clone_foreign) returns [`None`]: a borrowed
/// region cannot be duplicated without inventing storage the caller never
/// provided. Nothing needs it to — zlib has no `inflateBackCopy`, so no code path
/// ever clones an `inflateBack` window.
struct CBorrowedBuffer {
    /// Non-null pointer to `len` caller-owned bytes, readable and writable but
    /// not initialized by this crate.
    ptr: NonNull<u8>,
    /// Length of the lent region in bytes.
    len: usize,
}

impl ForeignBuffer<u8> for CBorrowedBuffer {
    #[inline]
    fn as_slice(&self) -> &[u8] {
        // SAFETY: `borrow_caller_window` established every `from_raw_parts`
        // precondition — non-null pointer, `len <= isize::MAX`, and `u8`'s alignment
        // of 1 which every address satisfies. The bytes need not be initialized:
        // `u8` has no invalid bit pattern and no niche, so every byte the caller
        // left behind is a valid inhabitant of the slice's element type. The
        // caller's `# Safety` contract on `inflateBackInit_` guarantees the region
        // stays valid and is not aliased for the life of the state, so a shared
        // slice for this `&self` borrow is sound.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    #[inline]
    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: same validity and length reasoning as `as_slice`, including that
        // uninitialized `u8`s are valid inhabitants; the `&mut self` borrow
        // additionally guarantees no other reference into the region exists on our
        // side, and the caller's `inflateBackInit_` contract guarantees none exists
        // on theirs, so a unique slice is sound.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    fn clone_foreign(&self) -> Option<Box<dyn ForeignBuffer<u8>>> {
        // A lent region has no allocator to re-request it from. See the type-level
        // "Not clonable" note: no zlib entry point copies an `inflateBack` state.
        None
    }
}

// Deliberately **no** `impl Drop for CBorrowedBuffer`. C `inflateBackEnd` frees
// only the state and leaves the caller's window alone (`infback.c` L572-L577);
// releasing it here would be a double free of memory this crate never allocated.

/// Wraps a caller-supplied `inflateBack` window as an [`AllocBuffer`] the engine
/// can use without copying or reallocating it.
///
/// Returns [`None`] — which the `inflateBackInit_` shim reports as
/// `Z_STREAM_ERROR` for a malformed request or `Z_MEM_ERROR` for an exhausted
/// heap — when `window` is null, when `len` is `0` or exceeds `isize::MAX`, or
/// when the small `Box` holding the borrow cannot be allocated.
///
/// # The region is adopted, never initialized
///
/// C stores the pointer and nothing else — `infback.c` L60 is a bare
/// `state->window = window;` — so this function writes **no** bytes to it
/// either. That parity is directly observable: a caller who pre-fills its window
/// and then makes an `inflateBackInit_` call that fails (an exhausted allocator)
/// must find the buffer byte-for-byte as it left it, because C never reached the
/// adoption at all.
///
/// Forming a `&mut [u8]` over the region without initializing it first is sound
/// here for two independent reasons. `u8` has no invalid bit patterns and no
/// niche, so every byte value — however the caller left it — is a valid
/// inhabitant; and the decoder never *reads* a window byte it has not itself
/// written, because `whave` and `wnext` both start at `0` and every match copy
/// is bounded by `whave`. The caller's obligation is therefore only that the
/// region be live, un-aliased and at least `len` bytes long, which is exactly
/// what `zlib.h` already demands of the `window` argument.
///
/// # Safety
///
/// * `window` must be valid for reads and writes of `len` bytes. Those bytes need
///   not be initialized; they must merely be within one live allocation.
/// * The region must remain allocated, and must not be accessed by the caller or
///   aliased by any other pointer, until the `inflateBack` state built from it is
///   destroyed by `inflateBackEnd`.
///
/// These are precisely the obligations `zlib.h` already places on the `window`
/// argument of `inflateBackInit` (`zlib.h` L1682-L1699), restated in Rust terms.
pub(crate) unsafe fn borrow_caller_window(
    window: *mut core::ffi::c_uchar,
    len: usize,
) -> Option<crate::stream::AllocBuffer<u8>> {
    let ptr = NonNull::new(window)?;
    if len == 0 || len > isize::MAX as usize {
        return None;
    }

    // No write of any kind happens here: C's `state->window = window;`
    // (`infback.c` L60) copies a pointer, and matching it is what keeps a
    // caller's pre-filled buffer intact across both a successful and a refused
    // init. The `ForeignBuffer` methods above form their slices over a live,
    // `len`-byte, un-aliased region of `u8` — a type with no invalid bit pattern
    // — and the decoder reads only bytes it has already written (`whave` starts
    // at `0` and bounds every match copy), so no uninitialized byte is ever
    // observed as a value.
    let borrowed = try_box(CBorrowedBuffer { ptr, len })?;
    Some(crate::stream::AllocBuffer::Foreign(borrowed))
}

/// The validated shape of one caller-hook allocation request.
///
/// Built by [`hook_request`] *before* any hook is consulted, so every field is
/// already known to be expressible both as a Rust [`Layout`] and in the C `uInt`
/// hook ABI.
pub(super) struct HookRequest {
    /// Layout of the whole `count`-element region: carries the byte size the
    /// hook must serve and the alignment its answer has to satisfy.
    pub(super) layout: Layout,
    /// The C `uInt items` argument (`zlib.h` L85), forwarded verbatim.
    pub(super) items: c_uint,
    /// The C `uInt size` argument (`zlib.h` L85), forwarded verbatim.
    pub(super) size: c_uint,
    /// Element count the region materializes as, `items * item_size /
    /// size_of::<T>()`.
    pub(super) count: usize,
}

/// Validates an `(items, item_size)` request for `T` against both the Rust slice
/// rules and the C `uInt` hook ABI, **without consulting any hook**.
///
/// Returns [`None`] — an allocation failure, exactly as an OOM would be — for
/// every rejection listed under "Returns" on [`try_alloc_foreign_items`] that is
/// detectable before the call.
///
/// # Why this is a separate function
///
/// * "The hook is not consulted" becomes *structural* rather than merely
///   asserted: this function has no hook to consult.
/// * The gate is generic over an unbounded `T`, so it can be exercised directly
///   for element types the allocator itself never serves — a zero-sized type in
///   particular, which the sealed [`ZeroValid`] element-type set excludes from
///   [`try_alloc_foreign_items`] at compile time.
///
/// # Check order
///
/// Load-bearing, and identical to the order documented on
/// [`try_alloc_foreign_items`]: zero-sized rejection, then the byte product and
/// its element-multiple requirement, then the Rust layout, then the three `uInt`
/// width checks.
pub(super) fn hook_request<T>(items: usize, item_size: usize) -> Option<HookRequest> {
    // A zero-sized element type cannot be expressed to the C hook (`size` would
    // be 0) and needs no allocation at all; treat it as unrepresentable here so
    // the caller's global-allocator arm handles it.
    let elem = core::mem::size_of::<T>();
    if elem == 0 {
        return None;
    }

    // Derive the element count from the byte product the hook will be asked for.
    // A non-multiple of the element size, an overflow, or an empty request cannot
    // be represented as a `&[T]`, so treat each as an allocation failure.
    let total = items.checked_mul(item_size)?;
    if total == 0 || total % elem != 0 {
        return None;
    }
    let count = total / elem;

    // A valid `Layout` is the authoritative proof that `count` elements can be
    // addressed at all: it rejects anything above `isize::MAX` bytes, which a
    // product check alone does not establish. It also carries `T`'s alignment,
    // checked against the hook's answer by the caller.
    let layout = Layout::array::<T>(count).ok()?;
    debug_assert_eq!(
        layout.size(),
        total,
        "layout must describe the byte product"
    );

    // The C hook takes `items: uInt` and `size: uInt` (both `c_uint`) and
    // multiplies them internally (`zlib.h` L85; `zutil.c` `zcalloc`). Guard both
    // arguments *and* the product so a truncated or wrapped size can never reach
    // it; an unrepresentable request is an allocation failure (`None`).
    //
    // The layout and `uInt` validations deliberately overlap, and which one is
    // load-bearing is target-dependent: where `usize` is wider than `c_uint`
    // (64-bit) the `c_uint` guards subsume the layout ceiling, whereas where
    // `isize::MAX` (2^31-1) is *below* `c_uint::MAX` (2^32-1) — every 32-bit
    // target — the byte range 2^31..=2^32-1 is representable to the hook yet can
    // never become a `&[T]`, and `Layout::array` is the only guard that rejects
    // it. Both are therefore required for the portability the crate claims;
    // neither is redundant across the supported target set.
    let items_arg = c_uint::try_from(items).ok()?;
    let size_arg = c_uint::try_from(item_size).ok()?;
    c_uint::try_from(total).ok()?;

    Some(HookRequest {
        layout,
        items: items_arg,
        size: size_arg,
        count,
    })
}

/// Initializes every slot of a freshly allocated region by **writing** a valid
/// [`T::default()`](Default) value into it.
///
/// This is the region initializer [`try_alloc_foreign_items`] applies to the
/// memory a caller's `zalloc` returned. Writing values — rather than memset-ing
/// raw zero bytes — is what makes the initialization correct for *any*
/// `T: Copy + Default` and not merely for types whose all-zero bit pattern happens
/// to be valid; see the "Element-type requirement" section on
/// [`try_alloc_foreign_items`].
///
/// Taking `&mut [MaybeUninit<T>]` keeps this entirely safe code and makes the
/// property directly testable for an element type whose `Default` is *not*
/// all-zero, which no member of the sealed [`ZeroValid`] set can be.
///
/// `T: Copy` guarantees no drop glue, so overwriting an as-yet uninitialized slot
/// runs no destructor.
pub(super) fn fill_default<T: Copy + Default>(slots: &mut [MaybeUninit<T>]) {
    slots.fill(MaybeUninit::new(T::default()));
}

/// Allocates a [`Default`]-initialized buffer of `count` elements of `T` through
/// the caller's active `zalloc`, requesting it as `ZALLOC(opaque, count,
/// size_of::<T>())`.
///
/// This is the shape-preserving convenience wrapper over
/// [`try_alloc_foreign_items`] for buffers whose logical item size *is* the
/// element size (C `prev`/`head`, `ZALLOC(strm, n, sizeof(Pos))`). Buffers whose
/// C request pair differs from `(count, size_of::<T>())` — the doubled window and
/// the pending/symbol buffers — must call [`try_alloc_foreign_items`] directly so
/// the caller's allocator observes the same `(items, size)` arguments C passes.
///
/// # Preconditions
///
/// Only meaningful for an active hook and a non-zero `count`; both are guaranteed
/// by the sole caller ([`AllocBuffer::try_zeroed`](crate::stream::AllocBuffer::try_zeroed)),
/// which handles the null-hook / empty-request fast paths itself.
#[inline]
pub(crate) fn try_alloc_foreign<T: Copy + Default + ZeroValid + 'static>(
    hook: AllocHook,
    count: usize,
) -> Option<Box<dyn ForeignBuffer<T>>> {
    try_alloc_foreign_items::<T>(hook, count, core::mem::size_of::<T>())
}

/// Allocates a [`Default`]-initialized buffer through the caller's active
/// `zalloc`, forwarding the `(items, item_size)` pair verbatim — or [`None`] when
/// the request cannot be honored.
///
/// This is the sanctioned home of the raw allocator-hook invocation and the
/// post-allocation element initialization (reproducing C `zcalloc`'s `zmemzero`).
/// [`crate::stream::AllocBuffer::try_zeroed_items`] delegates here via
/// [`AllocHook::try_alloc_zeroed_items`] so the safe core contains no `unsafe`
/// (AAP §0.6.2). Returning [`None`] on `zalloc` OOM — rather than silently using the
/// global allocator — is what lets the init paths surface `Z_MEM_ERROR` (AAP §0.6.5).
///
/// The materialized slice length is `items * item_size / size_of::<T>()`, so the
/// byte product must be an exact multiple of the element size.
///
/// # Returns
///
/// [`None`] — treated by every caller as `Z_MEM_ERROR` — when
///
/// * `items * item_size` overflows, is zero, or is not a multiple of
///   `size_of::<T>()`;
/// * the resulting element array has no valid [`Layout`] (it would exceed
///   `isize::MAX` bytes, the hard limit for any Rust reference or slice);
/// * either argument is unrepresentable in the C `uInt` hook ABI;
/// * the caller's `zalloc` reports out-of-memory by returning null; or
/// * the caller's `zalloc` returns a region that is not aligned for `T` (the
///   region is handed straight back to `zfree` before reporting the failure).
///
/// # Preconditions
///
/// Only meaningful for an active hook and a non-empty request; both are
/// guaranteed by the sole safe-side caller
/// ([`AllocBuffer::try_zeroed_items`](crate::stream::AllocBuffer::try_zeroed_items)),
/// which handles the null-hook / empty-request fast paths itself. A
/// `debug_assert!` documents and checks the contract without cost in release
/// builds.
pub(crate) fn try_alloc_foreign_items<T: Copy + Default + ZeroValid + 'static>(
    hook: AllocHook,
    items: usize,
    item_size: usize,
) -> Option<Box<dyn ForeignBuffer<T>>> {
    debug_assert!(
        hook.is_active() && items > 0 && item_size > 0,
        "try_alloc_foreign_items is only called for an active hook and a non-empty request"
    );

    // Both halves of an active hook are present, but re-check `zalloc` to obtain
    // the function pointer without unwrapping.
    let zalloc = hook.zalloc()?;

    // Shape and validate the request — zero-sized element type, byte product and
    // its element-multiple requirement, Rust layout, and the three C `uInt` width
    // checks, in that order — before the hook is consulted at all. Every rejection
    // is an allocation failure (`None`).
    let HookRequest {
        layout,
        items: items_arg,
        size: size_arg,
        count,
    } = hook_request::<T>(items, item_size)?;

    // SAFETY: `zalloc` is a caller-supplied `alloc_func` taken from a valid
    // `z_stream`, matching the `zlib.h` L85 signature exactly (opaque cookie,
    // `uInt` items, `uInt` size). It is called with `items * size` already proven
    // to be a representable, non-overflowing byte count. `opaque` is the caller's
    // cookie, forwarded verbatim; only the returned pointer value is inspected
    // here, and the hook itself is not dereferenced in any other way.
    let raw = unsafe { zalloc(hook.opaque(), items_arg, size_arg) } as *mut T;

    // A null return is the zlib out-of-memory signal (AAP §0.6.5): propagate it.
    let ptr = NonNull::new(raw)?;

    // A conforming `alloc_func` returns memory suitably aligned for any type of
    // the requested size, but that is the caller's promise rather than something
    // we can assume: an under-aligned region would make every later
    // `from_raw_parts` and the element writes below unsound. Verify it, and on
    // failure hand the region straight back to the caller's `zfree` (so nothing
    // leaks) and report an allocation failure.
    if ptr.as_ptr().addr() % layout.align() != 0 {
        if let Some(zfree) = hook.zfree() {
            // SAFETY: `raw` was just returned by this hook's `zalloc` and has not
            // been freed or shared with anything — no `CForeignBuffer` was
            // constructed over it. `zfree` is the caller's matching deallocator
            // and `opaque` its cookie, exactly as zlib's `ZFREE(strm, addr)`
            // expands to `(*zfree)(opaque, addr)`.
            unsafe { zfree(hook.opaque(), raw as *mut c_void) };
        }
        return None;
    }

    // SAFETY: every precondition of the slice creation and the `count` element
    // writes below has been
    // checked rather than assumed:
    //  * `ptr` is non-null (`NonNull::new` above);
    //  * `ptr` is aligned for `T` (the check immediately above);
    //  * `ptr` is valid for writes of `count * size_of::<T>()` bytes — that is
    //    the `layout` the hook was asked for, validated by `Layout::array` to be
    //    non-overflowing and within `isize::MAX`, and the hook's `zlib.h` L85
    //    contract is to return a region of at least `items * size` bytes, so the
    //    whole `count`-element region is in bounds and correctly aligned;
    //  * the region is freshly allocated and unaliased, so the writes are
    //    exclusive;
    //  * `T: Copy` has no drop glue, so writing over an as-yet uninitialized
    //    slot runs no destructor.
    // Each slot is initialized by writing a genuine `T::default()` value rather
    // than by memset-ing raw zero bytes, so this path makes **no** assumption
    // that an all-zero bit pattern is a legal `T`. The sealed `T: ZeroValid`
    // bound is not what makes this sound; it is retained as defence in depth — a
    // compile-time restriction of the element-type set to a list audited inside
    // this crate, which a downstream implementor cannot widen. For the types the
    // engines request (`u8` and `u16`, whose `Default` is `0`) the result is
    // byte-for-byte identical to C `zcalloc`'s post-`zalloc` `zmemzero`.
    // `MaybeUninit<T>` has the same layout as `T` and imposes no initialization
    // requirement, so a unique slice of `count` `MaybeUninit<T>` over the freshly
    // allocated, unaliased region is valid; filling it writes one valid value per
    // slot.
    let uninit: &mut [MaybeUninit<T>] =
        unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr().cast::<MaybeUninit<T>>(), count) };
    fill_default(uninit);

    // Build the owner first, then box it **fallibly**. `Box::new` aborts the
    // process when the Rust global heap cannot hold the owner's metadata, which
    // would turn a recoverable condition into process death *after* the caller's
    // `zalloc` had already succeeded — the opposite of C, which reports every
    // failed allocation as `Z_MEM_ERROR` (AAP §0.6.5). Moving the value into
    // `try_box` means a failure drops the owner, and `CForeignBuffer`'s `Drop`
    // hands the region straight back to the caller's `zfree`, so nothing leaks
    // and the caller's allocation accounting stays balanced.
    let owner = CForeignBuffer {
        ptr,
        len: count,
        items,
        item_size,
        hook,
    };
    let boxed: Box<dyn ForeignBuffer<T>> = try_box(owner)?;
    Some(boxed)
}

/// Allocates a single `T` on the Rust global heap **fallibly**, returning [`None`]
/// instead of aborting when the heap is exhausted.
///
/// [`Box::new`] has no fallible counterpart on stable Rust: an allocation failure
/// aborts the process. zlib, by contrast, reports a failed state allocation as
/// `Z_MEM_ERROR` and leaves the caller in control. The engine-state constructors
/// therefore route their one global allocation through here so an exhausted heap
/// surfaces as a return code, preserving C's availability behavior (AAP §0.6.5).
///
/// `value` is dropped normally when the allocation fails, so the working buffers
/// it already owns are released rather than leaked.
pub(crate) fn try_box<T>(value: T) -> Option<Box<T>> {
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        // A zero-sized `T` never touches the allocator, so `Box::new` cannot fail.
        return Some(Box::new(value));
    }

    // SAFETY: `layout` has a non-zero size, which is `alloc`'s only precondition.
    let raw = unsafe { alloc::alloc::alloc(layout) } as *mut T;
    let ptr = NonNull::new(raw)?;

    // SAFETY: `alloc` returned a non-null region of exactly `size_of::<T>()`
    // bytes aligned to `align_of::<T>()` (that is what `Layout::new::<T>()`
    // requested), and the region is uninitialized, so moving `value` into it with
    // `write` (which does not drop the previous contents) is sound.
    unsafe { core::ptr::write(ptr.as_ptr(), value) };

    // SAFETY: `ptr` came from the global allocator with `Layout::new::<T>()` —
    // exactly the layout `Box<T>` deallocates with — and now holds an initialized
    // `T` that nothing else owns, so `Box` may take ownership of it.
    Some(unsafe { Box::from_raw(ptr.as_ptr()) })
}

// ===========================================================================
// Built-in allocator hooks — the crate's `zcalloc` / `zcfree` counterparts
// ===========================================================================
//
// C's three `*Init*_` prologues complete a partially-supplied allocator pair by
// substituting the library's own built-ins for whichever half the caller left
// null: `zcalloc` for a null `zalloc` (also clearing `opaque`) and `zcfree` for
// a null `zfree` (`deflate.c` L400-L414, `inflate.c` L183-L196,
// `infback.c` L37-L50). Reproducing that behavior faithfully requires the crate
// to own an equivalent pair, which is what the two functions below are.
//
// `zcalloc`/`zcfree` are `local:` entries in `zlib.map` — present in the library
// but never exported. The counterparts here mirror that exactly: they are plain
// crate-private Rust items with **no** `#[unsafe(no_mangle)]`, so they add
// nothing to the emitted symbol table and cannot be reached from outside the
// crate. Only their *addresses* ever leave, installed into a caller's
// `z_stream.zalloc`/`zfree` by `crate::ffi::types::init_allocator_prologue`.
//
// They are deliberately implemented over the platform `malloc`/`free` rather
// than the Rust global allocator, because that is precisely what C's built-ins
// are (`zutil.c` L299-L307: `malloc(items * size)` and `free(ptr)`). A caller who
// supplies only one half of the pair therefore ends up with the *same* mixed
// pairing C would give them — their `zfree` releasing a `malloc`'d region, or
// their `zalloc`'d region released by `free` — instead of a pairing that would
// hand a region to an allocator that never owned it. The Rust global allocator
// cannot serve here: `zfree` receives only an address, with no `Layout`, and
// `dealloc` requires the original layout.
//
// Declaring `malloc`/`free` introduces no new link dependency in any
// configuration this crate builds: a hosted build links `std`, which links the
// platform `libc` already, and the only build that does not — the freestanding
// `no_std` `cdylib`/`staticlib` — already requires the same symbols for the
// private `libc`-backed global allocator in `src/lib.rs`'s `no_std_support`
// module. AAP §0.5.2 sanctions exactly this: the platform `libc` already linked
// by any hosted artifact introduces no *additional* C dependency and keeps the
// shipped Rust dependency graph pure.

// The C runtime allocation primitives C's own `zcalloc`/`zcfree` are built on.
unsafe extern "C" {
    /// Platform `void *malloc(size_t size)`.
    fn malloc(size: usize) -> *mut c_void;
    /// Platform `void free(void *ptr)`.
    fn free(ptr: *mut c_void);
}

/// The byte count [`default_zalloc`] will ask `malloc` for, or [`None`] when the
/// request cannot be expressed as an addressable region at all.
///
/// This is deliberately a separate, pure, total function rather than three lines
/// inlined into [`default_zalloc`], for two reasons.
///
/// **It makes the size ceiling structural instead of delegated.** Leaving the
/// ceiling to `malloc` — handing it the full `items * size` product and trusting
/// it to refuse an absurd one — does not establish the property as a *guarantee*,
/// because whether a given `size_t` is refused is a property of the platform
/// allocator, and because an optimizing compiler is entitled to reason about a
/// `malloc` whose result is only ever tested for nullity. Measured on this
/// repository at `opt-level = 3` with `codegen-units = 1` — the crate's own
/// `[profile.release]` — the call to `malloc` for an unrepresentable size was
/// removed outright and the null test folded away, so the property held in a
/// debug build and silently did not hold in the profile that actually ships.
/// Deciding the ceiling here, in ordinary integer arithmetic on values the
/// compiler cannot assume anything about, is what makes the answer identical in
/// every profile and on every target.
///
/// **It makes the guarantee testable without performing an allocation.** The
/// property under test is a statement about arithmetic, so it is verified as one
/// (`default_zalloc_rejects_every_unrepresentable_request`), independent of how
/// much memory the host happens to have and of whether `malloc` is feeling
/// generous.
///
/// # Rejection clauses
///
/// Both are required, and which one is load-bearing is target-dependent — the
/// same complementary pairing already documented on [`hook_request`]:
///
/// 1. `items * size` overflows `usize`. Reachable only where `usize` is no wider
///    than `c_uint`, i.e. on 32-bit targets, where the product of two
///    `0xFFFF_FFFF`s cannot be held at all.
/// 2. the product exceeds `isize::MAX`. This is the ceiling every Rust
///    allocation obeys, the one [`Layout`] enforces for
///    [`hook_request`], and the same rule already applied to a caller-supplied
///    window in [`borrow_caller_window`]. On a 64-bit target it is the *only* clause
///    that rejects the `c_uint` products above `2^63`; on a 32-bit target it
///    additionally rejects the `2^31 ..= 2^32-1` band that `usize` can hold but
///    no Rust reference can span.
///
/// A **zero** product is deliberately *not* rejected. C's `zcalloc` forwards it
/// to `malloc(0)`, whose result — null or a unique non-null pointer — is
/// implementation-defined, and both answers are already handled by every
/// consumer; rejecting it here would be a behavior change this crate has no
/// reason to make. The crate's own paths never produce it, because
/// [`hook_request`] rejects an empty request before the hook is consulted.
#[inline]
#[must_use]
fn default_zalloc_bytes(items: c_uint, size: c_uint) -> Option<usize> {
    // Clause 1 — the product must be representable at all.
    let bytes = (items as usize).checked_mul(size as usize)?;

    // Clause 2 — and must be within the ceiling any addressable region obeys.
    if bytes > isize::MAX as usize {
        return None;
    }

    Some(bytes)
}

/// The crate's counterpart of C `zcalloc` (`zutil.c` L299-L302).
///
/// Substituted for a caller's null `z_stream.zalloc` by
/// [`crate::ffi::types::init_allocator_prologue`], reproducing C's per-half
/// default. Like C's built-in it returns *uninitialized* storage — the zero fill
/// C performs is done by the consumer (`inflate.c` L199 `zmemzero`, and
/// [`try_alloc_foreign_items`] here), not by the hook.
///
/// The one deliberate refinement over C: C evaluates `items * size` in
/// `unsigned` arithmetic, which silently wraps on overflow and can hand back a
/// region far smaller than requested. Measured against a reference C library
/// built from this repository's own `zutil.c`, `zcalloc(_, 0xFFFF_FFFF,
/// 0xFFFF_FFFF)` wraps to `malloc(1)` and answers a 16-exabyte request with a
/// **one-byte** region — a heap overflow waiting for its first write. This
/// instead sizes the request through [`default_zalloc_bytes`] and reports
/// anything it cannot express as an allocation failure (a null return), which
/// every caller already maps to `Z_MEM_ERROR`.
///
/// The refinement is unobservable through the C ABI on any engine path, because
/// [`try_alloc_foreign_items`] rejects a request whose `items * size` exceeds
/// `uInt` before the hook is ever consulted. It is reachable only by a caller who
/// invokes the substituted `z_stream.zalloc` directly with a pair no engine path
/// produces, and for such a caller a null answer is the only one that is not
/// immediately undefined behavior.
///
/// # Safety
///
/// This is an `unsafe extern "C" fn` because it must be assignable to the C
/// `alloc_func` pointer type. It imposes no obligation on its caller beyond the
/// ordinary zlib `zalloc` contract: `opaque` is ignored entirely, and the
/// returned region (when non-null) is at least `items * size` writable bytes
/// that must be released through [`default_zfree`] exactly once.
pub(crate) unsafe extern "C" fn default_zalloc(
    _opaque: *mut c_void,
    items: c_uint,
    size: c_uint,
) -> *mut c_void {
    let Some(bytes) = default_zalloc_bytes(items, size) else {
        // An unrepresentable request is an allocation failure, never a wrapped
        // (and therefore undersized) one. Decided before `malloc` is reached, so
        // the guarantee belongs to this crate rather than to the platform
        // allocator's tolerance or to the optimizer's mood.
        return core::ptr::null_mut();
    };
    // SAFETY: `bytes` is at most `isize::MAX` (just established), and `malloc`
    // accepts any `size_t`, answering with either null (out of memory) or a
    // pointer to `bytes` writable, suitably aligned bytes. No pointer supplied by
    // the caller is dereferenced — `opaque` is ignored.
    unsafe { malloc(bytes) }
}

/// The crate's counterpart of C `zcfree` (`zutil.c` L304-L307).
///
/// Substituted for a caller's null `z_stream.zfree` by
/// [`crate::ffi::types::init_allocator_prologue`]. As in C, `opaque` is ignored
/// and a null address is a no-op (`free(NULL)` is defined to do nothing).
///
/// # Safety
///
/// This is an `unsafe extern "C" fn` because it must be assignable to the C
/// `free_func` pointer type. `address` must be null or a pointer previously
/// returned by [`default_zalloc`] (equivalently, by the platform `malloc`) and
/// not yet released — the same obligation `zlib.h` L86 already places on any
/// `free_func`.
pub(crate) unsafe extern "C" fn default_zfree(_opaque: *mut c_void, address: *mut c_void) {
    // SAFETY: per this function's `# Safety` contract `address` is null or a live
    // region obtained from `default_zalloc`, i.e. from `malloc`, so `free` is the
    // matching deallocator. `free(NULL)` is a defined no-op.
    unsafe { free(address) };
}

// ===========================================================================
// Caller-hook-backed engine states
// ===========================================================================

/// A caller-`zalloc`'d region reserved for one engine state and not yet filled.
///
/// # Why the reservation is a separate object
///
/// Reference zlib charges its allocator for the state *first* and for the working
/// buffers afterwards:
///
/// ```text
/// s = (deflate_state *) ZALLOC(strm, 1, sizeof(deflate_state));  /* deflate.c L305 */
/// if (s == Z_NULL) return Z_MEM_ERROR;
/// ...
/// s->window = (Bytef *) ZALLOC(strm, s->w_size, 2*sizeof(Byte)); /* L346 */
/// ```
///
/// A caller with a bounded arena observes `Z_MEM_ERROR` from whichever of those
/// requests exhausts it, so the *order* is part of the behaviour (AAP §0.6.5).
/// The Rust state value, however, cannot exist until after its buffers do — they
/// are its fields. Reserving the region up front and moving the finished state
/// into it afterwards is what reconciles the two.
///
/// # Type invariants
///
/// Established once by [`try_reserve_engine`], the only producer:
///
/// * `ptr` is non-null, was returned by `hook`'s `zalloc`, and is aligned for `E`;
/// * it addresses at least `size_of::<E>()` writable bytes;
/// * the region is **uninitialized** — nothing is written to it before [`fill`]
///   moves an `E` in, matching C, whose `ZALLOC` is `malloc` and not `calloc`
///   (`zutil.c` `zcalloc`);
/// * `hook` is the hook the region came from and is still
///   [active](AllocHook::is_active).
///
/// [`fill`]: ForeignEngineHome::fill
struct CEngineHome<E> {
    /// Non-null, `E`-aligned pointer to the reserved region.
    ptr: NonNull<E>,
    /// The hook the region came from; its `zfree` releases it.
    hook: AllocHook,
}

impl<E: 'static> ForeignEngineHome<E> for CEngineHome<E> {
    fn fill(self: Box<Self>, engine: E) -> Option<Box<dyn ForeignEngine<E>>> {
        let ptr = self.ptr;
        let hook = self.hook;
        // Allocate the type-erasing owner **before** the region is committed, and
        // fallibly. Doing it in this order is what makes heap exhaustion here
        // reportable instead of fatal: on refusal `self` is still alive and still
        // owns the reservation, so returning `None` runs `CEngineHome::drop` and
        // hands the region back through the caller's `zfree`, while `engine` is
        // dropped by the caller and releases its working buffers through the same
        // hook. That is C's `deflate.c` L505-L514 recovery. The infallible
        // `Box::new` this replaced aborted the process, which zlib never does.
        let owner = try_box(CEngine { ptr, hook })?;
        // Hand ownership of the region to `owner` *without* running this home's
        // `Drop`, which would free the region out from under it. `mem::forget` is
        // the transfer, and neither it nor the `write` below can panic, so `owner`
        // cannot be dropped while it still points at uninitialized memory.
        core::mem::forget(self);
        // SAFETY: the type invariants give a non-null, `E`-aligned pointer to at
        // least `size_of::<E>()` writable, unaliased bytes. `write` moves `engine`
        // in without reading or dropping whatever bit pattern was there, which is
        // required because the region is uninitialized.
        unsafe { ptr.as_ptr().write(engine) };
        Some(owner)
    }
}

impl<E> Drop for CEngineHome<E> {
    /// Releases an *unfilled* reservation.
    ///
    /// Reached when the state could not be built after the region was already
    /// charged — C's own early-return paths, which `ZFREE` the state before
    /// returning `Z_MEM_ERROR` (for example `deflate.c` L505-L514). No `E` was
    /// ever written, so nothing is dropped in place.
    fn drop(&mut self) {
        if let Some(zfree) = self.hook.zfree() {
            // SAFETY: `ptr` came from this same hook's `zalloc`, has not been
            // freed (a filled home is `mem::forget`ten, so this runs only for an
            // unfilled one), and `zfree`/`opaque` are the matching pair from the
            // same `z_stream` — exactly zlib's `ZFREE(strm, addr)`.
            unsafe { zfree(self.hook.opaque(), self.ptr.as_ptr() as *mut c_void) };
        }
    }
}

/// An engine state living in memory the caller's `zalloc` returned.
///
/// This is the concrete, `ffi`-local implementor of [`ForeignEngine`]. It is what
/// makes the caller's arena the real home of a `deflate_state` /
/// `inflate_state` rather than merely being charged for one: the accessors hand
/// out ordinary borrows into that region, so every layer above sees `&E` /
/// `&mut E` and never a pointer (AAP §0.6.2).
///
/// # Type invariants
///
/// Established by [`CEngineHome::fill`], the only producer:
///
/// * `ptr` is non-null, `E`-aligned, and addresses one **initialized** `E`;
/// * this value is that `E`'s unique owner, so the borrows below cannot alias and
///   [`Drop`] runs its destructor exactly once;
/// * `hook` is the hook the region came from and is still
///   [active](AllocHook::is_active).
struct CEngine<E> {
    /// Non-null, `E`-aligned pointer to one live `E`.
    ptr: NonNull<E>,
    /// The hook the region came from; its `zfree` releases it.
    hook: AllocHook,
}

impl<E: 'static> ForeignEngine<E> for CEngine<E> {
    #[inline]
    fn get(&self) -> &E {
        // SAFETY: the type invariants give a non-null, aligned pointer to one
        // initialized `E` that this value uniquely owns, so a shared borrow tied
        // to `&self` cannot alias a live `&mut E`.
        unsafe { self.ptr.as_ref() }
    }

    #[inline]
    fn get_mut(&mut self) -> &mut E {
        // SAFETY: same validity and initialization reasoning as `get`; the
        // `&mut self` borrow additionally guarantees no other reference into the
        // region exists, so a unique borrow is sound.
        unsafe { self.ptr.as_mut() }
    }
}

impl<E> Drop for CEngine<E> {
    /// Runs the engine's destructor **in place**, then hands the region back to
    /// the caller's `zfree`.
    ///
    /// The order matters: the state owns [`AllocBuffer`]s of its own, several of
    /// which may be foreign-backed, and their `zfree` calls must happen while the
    /// state still exists. That is the same order C uses — `deflateEnd` releases
    /// `pending_buf`, `head`, `prev` and `window` and only then the state itself
    /// (`deflate.c` L1104-L1112).
    ///
    /// [`AllocBuffer`]: crate::stream::AllocBuffer
    fn drop(&mut self) {
        // SAFETY: `ptr` addresses one initialized `E` that this value uniquely
        // owns and `Drop` runs once, so the destructor runs exactly once and
        // nothing reads the region afterwards.
        unsafe { core::ptr::drop_in_place(self.ptr.as_ptr()) };
        if let Some(zfree) = self.hook.zfree() {
            // SAFETY: `ptr` came from this same hook's `zalloc` and has not been
            // freed; `zfree`/`opaque` are the matching pair from the same
            // `z_stream`, so this is zlib's `ZFREE(strm, addr)`. The `E` has just
            // been dropped, so the region holds nothing live.
            unsafe { zfree(self.hook.opaque(), self.ptr.as_ptr() as *mut c_void) };
        }
    }
}

/// Requests one engine footprint from `hook` as `(1, size_of::<E>())`.
///
/// Returns [`None`] — reported by every caller as `Z_MEM_ERROR`, the code C
/// returns from its failed state `ZALLOC` — when the request cannot be expressed
/// to the C hook ABI, when `zalloc` reports out-of-memory, when the returned
/// region is misaligned for `E`, or when the small owning cell cannot be boxed.
///
/// Nothing is written to the region: an engine state is moved in whole by
/// [`CEngineHome::fill`], so pre-zeroing would be wasted work C does not do
/// either.
fn try_reserve_engine<E: 'static>(hook: AllocHook) -> Option<Box<dyn ForeignEngineHome<E>>> {
    debug_assert!(
        hook.is_active(),
        "try_reserve_engine is only called for an active hook"
    );

    // Both halves of an active hook are present; re-check to obtain the pointer
    // without unwrapping.
    let zalloc = hook.zalloc()?;

    // Shape and validate `(1, size_of::<E>())` against the Rust layout rules and
    // the C `uInt` hook ABI before the hook is consulted at all. A zero-sized
    // engine is rejected here, which is unreachable for the two real engines.
    let HookRequest {
        layout,
        items: items_arg,
        size: size_arg,
        count,
    } = hook_request::<E>(1, core::mem::size_of::<E>())?;
    debug_assert_eq!(count, 1, "one engine footprint is exactly one element");

    // SAFETY: `zalloc` is a caller-supplied `alloc_func` from a valid `z_stream`
    // with the `zlib.h` L85 signature, called with a validated, non-overflowing
    // `items * size`; `opaque` is the caller's cookie forwarded verbatim. Only
    // the returned pointer value is inspected.
    let raw = unsafe { zalloc(hook.opaque(), items_arg, size_arg) } as *mut E;

    // A null return is the zlib out-of-memory signal (AAP §0.6.5).
    let ptr = NonNull::new(raw)?;

    // A conforming `alloc_func` returns memory suitable for any object of the
    // requested size, but that is the caller's promise rather than something this
    // crate may assume: writing an `E` through an under-aligned pointer would be
    // undefined behaviour. Verify it, and on failure hand the region straight back
    // so nothing leaks.
    if ptr.as_ptr().addr() % layout.align() != 0 {
        if let Some(zfree) = hook.zfree() {
            // SAFETY: `raw` was just returned by this hook's `zalloc`, has not
            // been freed, and no owner was constructed over it; `zfree`/`opaque`
            // are the caller's matching pair.
            unsafe { zfree(hook.opaque(), raw as *mut c_void) };
        }
        return None;
    }

    // The cell is small and its allocation is fallible, so a boxing failure
    // releases the region through `CEngineHome::drop` instead of leaking it.
    let home: Box<CEngineHome<E>> = try_box(CEngineHome { ptr, hook })?;
    Some(home)
}

// ===========================================================================
// Core-declared allocation capabilities, implemented at the boundary
// ===========================================================================
//
// `crate::stream` (layer 5) declares the two capabilities below and this module
// (layer 8) implements them. That is the whole mechanism by which the safe core
// obtains fallible boxing and caller-hook-backed buffers without naming a single
// `crate::ffi` item: the dependency edge for this pair runs `ffi -> stream` only
// and never the reverse (AAP §0.3.1, §0.6.2), while keeping every raw-pointer
// operation inside the designated unsafe boundary (AAP §0.7.2 standard S2).
//
// Both traits are `pub(crate)`, so these blanket implementations are the only
// ones that can ever exist and no downstream crate can substitute a different
// allocation runtime.

impl<T> FallibleBoxAlloc for T {
    #[inline]
    fn try_box_fallible(self) -> Option<Box<Self>> {
        try_box(self)
    }
}

impl<T: Sized + 'static> ForeignEnginePlace for T {
    #[inline]
    fn try_reserve_foreign(hook: AllocHook) -> Option<Box<dyn ForeignEngineHome<Self>>> {
        try_reserve_engine::<Self>(hook)
    }
}

impl<T: Copy + Default + ZeroValid + 'static> ForeignAlloc for T {
    #[inline]
    fn try_alloc_foreign(hook: AllocHook, count: usize) -> Option<Box<dyn ForeignBuffer<Self>>> {
        try_alloc_foreign::<Self>(hook, count)
    }

    #[inline]
    fn try_alloc_foreign_items(
        hook: AllocHook,
        items: usize,
        item_size: usize,
    ) -> Option<Box<dyn ForeignBuffer<Self>>> {
        try_alloc_foreign_items::<Self>(hook, items, item_size)
    }
}

// Shared counted-hook test support
//
// A C `alloc_func`/`free_func` pair is a bare `extern "C"` function pointer and
// therefore cannot capture state; the only channel is the `opaque` cookie. This
// module provides one sound, reusable, *counting* backing store threaded through
// that cookie, so tests can assert exact invocation counts, symmetric cleanup,
// out-of-memory behaviour at a chosen allocation index, and rejection of a
// mis-aligned region.
//
// It lives here — and not next to the tests that use it — for two reasons:
//   * the raw-pointer work it needs is `unsafe`, and `src/ffi/**` is the crate's
//     designated unsafe boundary (AAP §0.6.2 / §0.7.2 standard S2); `src/stream.rs`
//     carries its own module-level unsafe-code denial and could not host it; and
//   * a single shared implementation keeps the allocator-contract assertions in
//     `stream`, `ffi::alloc`, `ffi::deflate` and `ffi::inflate` consistent.
// ===========================================================================

#[cfg(test)]
pub(crate) mod test_hook {
    //! A sound, counting caller-allocator used by the allocator-contract tests.

    use core::ffi::{c_uint, c_void};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::stream::AllocHook;

    /// Bytes reserved ahead of the payload. Holds the total allocation size and
    /// the payload offset so `zfree` can reconstruct the exact [`Layout`] it was
    /// allocated with.
    ///
    /// [`Layout`]: core::alloc::Layout
    const HDR: usize = 16;

    /// Alignment of every allocation this hook makes. Chosen to over-satisfy
    /// every element type in the sealed [`ZeroValid`] set — the engines request
    /// `u8` and `u16`, and the tests below also allocate `u32` buffers — so a
    /// conforming allocation is never rejected for alignment.
    ///
    /// [`ZeroValid`]: crate::stream::ZeroValid
    const ALIGN: usize = 16;

    /// Sentinel budget meaning "never report out-of-memory".
    const UNLIMITED: usize = usize::MAX;

    /// Slots in the request/release order log. Sized for the largest schedule any
    /// single zlib entry point produces — `deflateInit2_`'s five requests, or
    /// `deflateCopy`'s five on top of the source's five — with headroom.
    const LOG_CAP: usize = 16;

    /// Counting state for one hook instance, addressed through the C `opaque`
    /// cookie. All fields are atomics, so the hooks only ever need a shared
    /// reference and the struct is safe to share across the FFI boundary.
    #[derive(Debug)]
    pub(crate) struct HookStats {
        /// Number of `zalloc` calls that returned a non-null region.
        allocs: AtomicUsize,
        /// Number of `zfree` calls with a non-null address.
        frees: AtomicUsize,
        /// Number of `zalloc` calls that reported out-of-memory (null return).
        ooms: AtomicUsize,
        /// Payload bytes currently outstanding (`0` once every region is freed).
        live_bytes: AtomicUsize,
        /// Remaining successful allocations before `zalloc` starts reporting
        /// out-of-memory. [`UNLIMITED`] disables the failure injection.
        budget: AtomicUsize,
        /// When set, `zalloc` deliberately returns a region offset by one byte
        /// so it is mis-aligned for any multi-byte element type.
        misalign: AtomicBool,
        /// Payload address of each successful allocation, indexed by request
        /// number, so a later `zfree` can be attributed back to the request that
        /// produced it. `0` means "slot unused".
        alloc_ptrs: [AtomicUsize; LOG_CAP],
        /// Request number of each released region, in **release order**. This is
        /// what makes C's documented teardown order — "deallocate in reverse
        /// order of allocations" (`deflate.c` L1300) — directly assertable.
        free_seq: [AtomicUsize; LOG_CAP],
    }

    impl HookStats {
        /// A hook backing store that always succeeds and returns correctly
        /// aligned regions.
        pub(crate) const fn new() -> Self {
            Self {
                allocs: AtomicUsize::new(0),
                frees: AtomicUsize::new(0),
                ooms: AtomicUsize::new(0),
                live_bytes: AtomicUsize::new(0),
                budget: AtomicUsize::new(UNLIMITED),
                misalign: AtomicBool::new(false),
                alloc_ptrs: [const { AtomicUsize::new(0) }; LOG_CAP],
                free_seq: [const { AtomicUsize::new(usize::MAX) }; LOG_CAP],
            }
        }

        /// A hook backing store that succeeds for the first `n` allocations and
        /// reports out-of-memory for every one after that.
        pub(crate) const fn with_budget(n: usize) -> Self {
            Self {
                allocs: AtomicUsize::new(0),
                frees: AtomicUsize::new(0),
                ooms: AtomicUsize::new(0),
                live_bytes: AtomicUsize::new(0),
                budget: AtomicUsize::new(n),
                misalign: AtomicBool::new(false),
                alloc_ptrs: [const { AtomicUsize::new(0) }; LOG_CAP],
                free_seq: [const { AtomicUsize::new(usize::MAX) }; LOG_CAP],
            }
        }

        /// Re-arms the failure injection: the next `n` allocations succeed and
        /// every later one reports out-of-memory.
        pub(crate) fn set_budget(&self, n: usize) {
            self.budget.store(n, Ordering::SeqCst);
        }

        /// Makes every subsequent allocation return a deliberately mis-aligned
        /// region, to exercise the boundary's alignment rejection path.
        pub(crate) fn set_misalign(&self, on: bool) {
            self.misalign.store(on, Ordering::SeqCst);
        }

        /// Successful `zalloc` calls so far.
        pub(crate) fn allocs(&self) -> usize {
            self.allocs.load(Ordering::SeqCst)
        }

        /// `zfree` calls so far.
        pub(crate) fn frees(&self) -> usize {
            self.frees.load(Ordering::SeqCst)
        }

        /// `zalloc` calls that reported out-of-memory.
        pub(crate) fn ooms(&self) -> usize {
            self.ooms.load(Ordering::SeqCst)
        }

        /// Payload bytes still outstanding; `0` proves every region was freed.
        pub(crate) fn live_bytes(&self) -> usize {
            self.live_bytes.load(Ordering::SeqCst)
        }

        /// Request numbers of the regions released so far, in release order.
        ///
        /// Request numbers are zero-based and count only *successful*
        /// allocations, so for `deflateInit2_` they name C's five `ZALLOC`s in
        /// order: `0` the state, then `1` window, `2` prev, `3` head, `4`
        /// `pending_buf` (`deflate.c` L440, L458-L460, L505). C's teardown frees
        /// them as `pending_buf, head, prev, window, state` — `[4, 3, 2, 1, 0]` —
        /// because `deflateEnd` deallocates "in reverse order of allocations"
        /// (`deflate.c` L1300-L1306).
        ///
        /// A release whose address was never handed out by this hook, or that
        /// falls beyond [`LOG_CAP`], appears as [`usize::MAX`].
        pub(crate) fn free_order(&self) -> alloc::vec::Vec<usize> {
            let n = self.frees().min(LOG_CAP);
            (0..n)
                .map(|i| self.free_seq[i].load(Ordering::SeqCst))
                .collect()
        }

        /// An [`AllocHook`] with **both** halves installed (so it is *active*)
        /// whose `opaque` cookie addresses `self`.
        ///
        /// `self` must outlive every buffer allocated through the returned hook —
        /// guaranteed at every call site by keeping the `HookStats` alive in an
        /// enclosing scope.
        pub(crate) fn hook(&self) -> AllocHook {
            AllocHook::new(
                Some(counted_zalloc),
                Some(counted_zfree),
                core::ptr::from_ref(self).cast::<c_void>().cast_mut(),
            )
        }

        /// An [`AllocHook`] with **only** `zalloc` installed. Per the has-hook
        /// clause this is *inactive*, so allocation must use the global
        /// allocator and never consult `zalloc`.
        pub(crate) fn zalloc_only_hook(&self) -> AllocHook {
            AllocHook::new(
                Some(counted_zalloc),
                None,
                core::ptr::from_ref(self).cast::<c_void>().cast_mut(),
            )
        }

        /// An [`AllocHook`] with **only** `zfree` installed — likewise inactive.
        pub(crate) fn zfree_only_hook(&self) -> AllocHook {
            AllocHook::new(
                None,
                Some(counted_zfree),
                core::ptr::from_ref(self).cast::<c_void>().cast_mut(),
            )
        }
    }

    std::thread_local! {
        /// `(zalloc, zfree)` call counts for [`BuiltinHookStats`], as
        /// `(successful allocations, non-null frees)`.
        ///
        /// Thread-local rather than a process-wide `static` for two reasons. The
        /// hooks must work with a **null** `opaque` (see [`BuiltinHookStats`]), so
        /// there is no cookie to address per-instance state through; and the test
        /// harness runs each `#[test]` on its own thread, so a thread-local keeps
        /// concurrently running tests from observing one another's calls.
        static BUILTIN_HOOK_CALLS: core::cell::Cell<(usize, usize)> =
            const { core::cell::Cell::new((0, 0)) };
    }

    /// Counting hooks that are layout-compatible with the crate's **built-in**
    /// allocator, for the tests that exercise C's *per-half* substitution.
    ///
    /// [`init_allocator_prologue`](crate::ffi::types::init_allocator_prologue)
    /// completes a partially-supplied pair by installing
    /// [`default_zalloc`](super::default_zalloc) or
    /// [`default_zfree`](super::default_zfree) for the missing half, so the
    /// resulting pair is *mixed*: one caller half and one built-in half. A test
    /// hook for that scenario cannot use [`HookStats`], whose regions come from
    /// the Rust global allocator and carry a private header — handing one of those
    /// to `free`, or a `malloc`'d region to `counted_zfree`, would be undefined
    /// behavior. The two hooks here delegate to the built-ins instead, so either
    /// half composes correctly with the other half's substituted built-in.
    ///
    /// # `opaque` is ignored, exactly as C's built-ins ignore it
    ///
    /// C clears `strm->opaque` whenever it substitutes `zcalloc` for a missing
    /// `zalloc` (`deflate.c` L405-L406), because the cookie belonged to the
    /// allocator being replaced. A caller who supplied only `zfree` therefore has
    /// **their** hook invoked with `opaque == NULL` for the rest of the stream's
    /// life. These hooks reproduce the only design that survives that: they never
    /// read `opaque` at all, and count through the thread-local
    /// [`BUILTIN_HOOK_CALLS`] instead.
    ///
    /// Only call counts are tracked: `free` reveals no size, so there is no
    /// `live_bytes` equivalent (balance is asserted through `allocs`/`frees`).
    #[derive(Debug)]
    pub(crate) struct BuiltinHookStats;

    impl BuiltinHookStats {
        /// Zeroes this thread's counters and returns the handle to read them.
        pub(crate) fn new() -> Self {
            BUILTIN_HOOK_CALLS.with(|c| c.set((0, 0)));
            Self
        }

        /// Successful `zalloc` calls made on this thread since [`Self::new`].
        pub(crate) fn allocs(&self) -> usize {
            BUILTIN_HOOK_CALLS.with(|c| c.get().0)
        }

        /// `zfree` calls with a non-null address made on this thread since
        /// [`Self::new`].
        pub(crate) fn frees(&self) -> usize {
            BUILTIN_HOOK_CALLS.with(|c| c.get().1)
        }

        /// The `alloc_func` to install into a `z_stream.zalloc`.
        pub(crate) fn zalloc_fn(
            &self,
        ) -> extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void {
            builtin_backed_zalloc
        }

        /// The `free_func` to install into a `z_stream.zfree`.
        pub(crate) fn zfree_fn(&self) -> extern "C" fn(*mut c_void, *mut c_void) {
            builtin_backed_zfree
        }
    }

    /// Counts, then forwards to the crate's built-in `zalloc`. `opaque` is
    /// forwarded unread.
    extern "C" fn builtin_backed_zalloc(
        opaque: *mut c_void,
        items: c_uint,
        size: c_uint,
    ) -> *mut c_void {
        // SAFETY: `default_zalloc` ignores `opaque` entirely and imposes no other
        // obligation; it answers with null or a region of `items * size` bytes.
        let raw = unsafe { super::default_zalloc(opaque, items, size) };
        if !raw.is_null() {
            BUILTIN_HOOK_CALLS.with(|c| {
                let (a, f) = c.get();
                c.set((a + 1, f));
            });
        }
        raw
    }

    /// Counts, then forwards to the crate's built-in `zfree`. `opaque` is
    /// forwarded unread.
    extern "C" fn builtin_backed_zfree(opaque: *mut c_void, address: *mut c_void) {
        if address.is_null() {
            return;
        }
        BUILTIN_HOOK_CALLS.with(|c| {
            let (a, f) = c.get();
            c.set((a, f + 1));
        });
        // SAFETY: every non-null `address` reaching this hook was produced either
        // by `builtin_backed_zalloc` or by the `default_zalloc` the prologue
        // substituted — in both cases by `malloc` — so `default_zfree` is its
        // matching deallocator, and each region is released exactly once by the
        // single owner that holds it.
        unsafe { super::default_zfree(opaque, address) };
    }

    /// Recovers the counting state from the C `opaque` cookie.
    ///
    /// # Safety
    ///
    /// `opaque` must be the cookie produced by [`HookStats::hook`] (or one of its
    /// siblings) for a still-live `HookStats`.
    unsafe fn stats<'a>(opaque: *mut c_void) -> &'a HookStats {
        // SAFETY: the caller guarantees `opaque` came from
        // `core::ptr::from_ref(&HookStats)` for a `HookStats` that is still alive,
        // and `HookStats` is entirely atomic so a shared reference permits the
        // mutation the hooks perform.
        unsafe { &*opaque.cast::<HookStats>() }
    }

    /// A conforming `alloc_func` that counts, can inject out-of-memory after a
    /// chosen number of successes, and can deliberately mis-align its result.
    ///
    /// Declared as a *safe* `extern "C" fn` so it coerces to the
    /// `unsafe extern "C" fn` hook type without this module declaring an unsafe
    /// function; the raw-pointer work is inside explicit `unsafe` blocks.
    extern "C" fn counted_zalloc(opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
        // SAFETY: `opaque` is the cookie installed by `HookStats::hook`, whose
        // `HookStats` outlives every buffer allocated through the hook.
        let stats = unsafe { stats(opaque) };

        // Failure injection: consume one unit of budget, or report OOM.
        let budget = stats.budget.load(Ordering::SeqCst);
        if budget == 0 {
            stats.ooms.fetch_add(1, Ordering::SeqCst);
            return core::ptr::null_mut();
        }
        if budget != UNLIMITED {
            stats.budget.store(budget - 1, Ordering::SeqCst);
        }

        let bytes = (items as usize) * (size as usize);
        let off = if stats.misalign.load(Ordering::SeqCst) {
            HDR + 1
        } else {
            HDR
        };
        // One slack byte so the mis-aligned payload still ends inside the region.
        let total = HDR + bytes + 1;
        let layout = core::alloc::Layout::from_size_align(total, ALIGN)
            .expect("test allocation layout is valid");

        // SAFETY: `layout` has a non-zero size (`HDR + bytes + 1 >= 17`) and a
        // power-of-two alignment, which are `alloc_zeroed`'s only requirements.
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if base.is_null() {
            stats.ooms.fetch_add(1, Ordering::SeqCst);
            return core::ptr::null_mut();
        }

        // SAFETY: `base` addresses `total >= 17` writable bytes, so the two
        // header slots are in bounds: `total` occupies `[0, 8)` (naturally
        // aligned, since `base` is `ALIGN`-aligned) and the payload offset
        // occupies `[off - 8, off)` with `off >= 16`, written unaligned because
        // `off` may be odd. Neither slot overlaps the payload at `[off, total)`.
        unsafe {
            base.cast::<usize>().write(total);
            base.add(off - 8).cast::<usize>().write_unaligned(off);
        }

        let request = stats.allocs.fetch_add(1, Ordering::SeqCst);
        stats.live_bytes.fetch_add(bytes, Ordering::SeqCst);

        // SAFETY: `off < total`, so `base + off` is within the allocated region.
        let payload = unsafe { base.add(off) };

        // Log the payload address against its request number so `counted_zfree`
        // can reconstruct the release *order*.
        if request < LOG_CAP {
            stats.alloc_ptrs[request].store(payload as usize, Ordering::SeqCst);
        }

        payload.cast::<c_void>()
    }

    /// The matching `free_func`: reconstructs the original [`Layout`] from the
    /// header and releases the region, so the test allocator leaks nothing and
    /// `live_bytes()` returning `0` is a real balance assertion.
    ///
    /// [`Layout`]: core::alloc::Layout
    extern "C" fn counted_zfree(opaque: *mut c_void, address: *mut c_void) {
        if address.is_null() {
            return;
        }
        // SAFETY: as in `counted_zalloc`.
        let stats = unsafe { stats(opaque) };
        let payload = address.cast::<u8>();

        // SAFETY: `address` was produced by `counted_zalloc`, so the eight bytes
        // immediately before it hold the payload offset (written unaligned), and
        // subtracting that offset recovers the base pointer whose first eight
        // bytes hold the total allocation size.
        let (base, total) = unsafe {
            let off = payload.sub(8).cast::<usize>().read_unaligned();
            let base = payload.sub(off);
            (base, base.cast::<usize>().read())
        };
        let layout = core::alloc::Layout::from_size_align(total, ALIGN)
            .expect("test deallocation layout is valid");

        let release = stats.frees.fetch_add(1, Ordering::SeqCst);
        stats
            .live_bytes
            .fetch_sub(total - HDR - 1, Ordering::SeqCst);

        // Attribute the release back to the request that produced this address.
        if release < LOG_CAP {
            let want = address as usize;
            let request = (0..LOG_CAP)
                .find(|&i| stats.alloc_ptrs[i].load(Ordering::SeqCst) == want)
                .unwrap_or(usize::MAX);
            stats.free_seq[release].store(request, Ordering::SeqCst);
        }

        // SAFETY: `base` and `layout` are exactly the pointer and layout produced
        // by the matching `alloc_zeroed` in `counted_zalloc`, and this is the
        // first and only `dealloc` for that region (each region is freed once,
        // by the single `CForeignBuffer` that owns it).
        unsafe { alloc::alloc::dealloc(base, layout) };
    }
}

// ===========================================================================
// Tests — allocator-hook contract at the raw boundary
//
// Every case below drives `try_alloc_foreign` (and, through it,
// `CForeignBuffer`) with the shared counting hook from `test_hook`. No case
// fabricates an invalid pointer or otherwise relies on undefined behaviour: the
// mis-alignment case is exercised with a *validly allocated* region that is
// merely offset, which the boundary is required to reject and release.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::test_hook::HookStats;
    use super::*;
    use crate::stream::AllocBuffer;

    /// An **active** hook (both halves present) is invoked exactly once per
    /// allocation, produces a zero-filled, usable region, and is balanced by
    /// exactly one `zfree` on drop with no bytes outstanding.
    #[test]
    fn active_hook_allocates_zeroed_and_frees_symmetrically() {
        let stats = HookStats::new();
        let hook = stats.hook();
        assert!(hook.is_active());

        {
            let mut buf: AllocBuffer<u32> =
                AllocBuffer::try_zeroed(16, hook).expect("counting hook succeeds");
            assert_eq!(stats.allocs(), 1);
            assert_eq!(stats.ooms(), 0);
            assert_eq!(stats.live_bytes(), 16 * size_of::<u32>());

            // The region is zero-filled (C `zcalloc`'s `zmemzero`) and writable.
            assert_eq!(&buf[..], &[0u32; 16][..]);
            buf[3] = 0xDEAD_BEEF;
            assert_eq!(buf[3], 0xDEAD_BEEF);
            assert_eq!(buf[2], 0);
        } // <- drop releases through the caller's `zfree`

        assert_eq!(stats.frees(), 1, "zfree must balance zalloc exactly");
        assert_eq!(stats.live_bytes(), 0, "no bytes may stay outstanding");
    }

    /// [`try_box`] — the fallible replacement for [`Box::new`] that keeps a
    /// global-heap failure reportable as `Z_MEM_ERROR` instead of aborting —
    /// must move its value onto the heap intact.
    ///
    /// This is the helper every FFI handle installation routes through
    /// (`deflateInit2_`, `deflateCopy`, `inflateInit2_`, `inflateBackInit_`,
    /// `inflateCopy`, `gzopen`) as well as the `CForeignBuffer` owner above, so
    /// its value-preservation and its zero-sized fast path are both pinned here.
    #[test]
    fn try_box_moves_the_value_onto_the_heap_intact() {
        /// A payload with mixed alignment requirements, so a wrong `Layout`
        /// would show up as a corrupted field rather than passing by luck.
        #[derive(Debug, PartialEq, Eq)]
        struct Payload {
            tag: u64,
            byte: u8,
            words: [u16; 3],
        }

        let boxed = try_box(Payload {
            tag: 0x0123_4567_89ab_cdef,
            byte: 0x5a,
            words: [1, 2, 3],
        })
        .expect("a small allocation must succeed");

        assert_eq!(
            *boxed,
            Payload {
                tag: 0x0123_4567_89ab_cdef,
                byte: 0x5a,
                words: [1, 2, 3],
            },
            "try_box must not disturb the value it moves"
        );
        assert_eq!(
            (&raw const *boxed).addr() % core::mem::align_of::<Payload>(),
            0,
            "the box must satisfy the type's alignment"
        );
        // Dropping the box releases the region through the global allocator with
        // exactly `Layout::new::<Payload>()`, the layout it was allocated with.
        drop(boxed);
    }

    /// A zero-sized `T` never reaches the allocator, so [`try_box`] takes its
    /// infallible fast path and can never report failure.
    #[test]
    fn try_box_succeeds_for_a_zero_sized_type() {
        #[derive(Debug, PartialEq, Eq)]
        struct Zst;

        assert_eq!(*try_box(Zst).expect("a ZST box cannot fail"), Zst);
    }

    /// A **partial** hook is inactive per the has-hook clause: allocation uses
    /// the global allocator and neither half is ever consulted.
    #[test]
    fn partial_hooks_are_inactive_and_never_consulted() {
        let stats = HookStats::new();

        for hook in [stats.zalloc_only_hook(), stats.zfree_only_hook()] {
            assert!(!hook.is_active());
            let buf: AllocBuffer<u16> =
                AllocBuffer::try_zeroed(8, hook).expect("a small global reservation succeeds");
            assert_eq!(&buf[..], &[0u16; 8][..]);
            drop(buf);
        }

        assert_eq!(stats.allocs(), 0, "an inactive hook must not be called");
        assert_eq!(stats.frees(), 0, "an inactive hook must not be called");
        assert_eq!(stats.ooms(), 0);
    }

    /// The "no hook at all" path is the historical global-allocator behaviour.
    #[test]
    fn inactive_none_hook_uses_owned_storage() {
        let buf: AllocBuffer<u8> = AllocBuffer::try_zeroed(32, AllocHook::none())
            .expect("a small global reservation succeeds");
        assert!(!buf.is_foreign());
        assert_eq!(&buf[..], &[0u8; 32][..]);
    }

    /// An active hook reporting out-of-memory yields `None` with **no**
    /// global-allocator fallback, and nothing is freed because nothing was
    /// allocated (AAP §0.6.5).
    #[test]
    fn active_hook_oom_yields_none_with_no_fallback() {
        let stats = HookStats::with_budget(0);
        let hook = stats.hook();

        let buf: Option<AllocBuffer<u8>> = AllocBuffer::try_zeroed(64, hook);
        assert!(
            buf.is_none(),
            "OOM must not fall back to the global allocator"
        );
        assert_eq!(stats.allocs(), 0);
        assert_eq!(stats.ooms(), 1, "the hook must have been consulted once");
        assert_eq!(stats.frees(), 0);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// A zero-length request takes the owned fast path even when the hook is
    /// active, so `zalloc` is never called with a zero size.
    #[test]
    fn zero_count_uses_owned_fast_path_even_with_active_hook() {
        let stats = HookStats::new();
        let buf: AllocBuffer<u32> =
            AllocBuffer::try_zeroed(0, stats.hook()).expect("empty request always succeeds");
        assert!(buf.is_empty());
        assert!(!buf.is_foreign());
        assert_eq!(stats.allocs(), 0);
    }

    /// A request whose element count exceeds the C `uInt` hook ABI is rejected
    /// **before** the hook is consulted, so a truncated size can never reach a
    /// caller's allocator.
    #[test]
    fn count_beyond_c_uint_is_rejected_before_calling_the_hook() {
        let stats = HookStats::new();
        // The smallest count that cannot be expressed in the C `uInt` hook ABI.
        // On a target where `usize` is no wider than `c_uint` no such value
        // exists, so fall back to `usize::MAX`, which the layout check rejects
        // first — either way the hook must not be consulted.
        let over = usize::try_from(c_uint::MAX)
            .ok()
            .and_then(|m| m.checked_add(1))
            .unwrap_or(usize::MAX);

        let buf: Option<AllocBuffer<u8>> = AllocBuffer::try_zeroed(over, stats.hook());
        assert!(buf.is_none());
        assert_eq!(stats.allocs(), 0, "the hook must not be consulted");
        assert_eq!(stats.ooms(), 0, "rejection is not a hook-reported OOM");
    }

    /// A request whose byte size overflows, or exceeds the `isize::MAX` limit a
    /// Rust slice must respect, is rejected by `Layout::array` before the hook
    /// is consulted.
    #[test]
    fn layout_overflow_is_rejected_before_calling_the_hook() {
        let stats = HookStats::new();

        for count in [usize::MAX, usize::MAX / 2, (isize::MAX as usize) / 2 + 1] {
            let buf: Option<AllocBuffer<u32>> = AllocBuffer::try_zeroed(count, stats.hook());
            assert!(buf.is_none(), "count {count} must be rejected");
        }
        assert_eq!(stats.allocs(), 0, "the hook must not be consulted");
        assert_eq!(stats.ooms(), 0);
    }

    /// A hook that returns a **mis-aligned** region — validly allocated, merely
    /// offset by one byte — must be rejected rather than used, and the region
    /// must be handed straight back to the caller's `zfree` so nothing leaks.
    /// The offending pointer is never dereferenced, so no case here relies on
    /// undefined behaviour.
    #[test]
    fn misaligned_region_is_rejected_and_released() {
        let stats = HookStats::new();
        stats.set_misalign(true);

        let buf: Option<AllocBuffer<u32>> = AllocBuffer::try_zeroed(8, stats.hook());
        assert!(
            buf.is_none(),
            "an under-aligned region must be reported as an allocation failure"
        );
        assert_eq!(stats.allocs(), 1, "the hook did return a region");
        assert_eq!(stats.frees(), 1, "the rejected region must be released");
        assert_eq!(stats.live_bytes(), 0, "the rejected region must not leak");

        // A correctly aligned region from the same hook still succeeds, proving
        // the rejection is specific to the alignment fault.
        stats.set_misalign(false);
        let ok: AllocBuffer<u32> =
            AllocBuffer::try_zeroed(8, stats.hook()).expect("aligned region is accepted");
        assert_eq!(&ok[..], &[0u32; 8][..]);
        drop(ok);
        assert_eq!(stats.frees(), 2);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// A single-byte element type has alignment 1, so the deliberately offset
    /// region is still correctly aligned and is therefore accepted — the
    /// alignment check is a real per-type test, not a blanket rejection of the
    /// mis-aligning hook.
    #[test]
    fn byte_buffers_accept_the_offset_region_because_align_of_u8_is_one() {
        let stats = HookStats::new();
        stats.set_misalign(true);

        {
            let buf: AllocBuffer<u8> =
                AllocBuffer::try_zeroed(8, stats.hook()).expect("u8 needs no alignment");
            assert_eq!(&buf[..], &[0u8; 8][..]);
        }
        assert_eq!(stats.allocs(), 1);
        assert_eq!(stats.frees(), 1);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// Every `ZeroValid` element type the engines request comes back holding
    /// `T::default()` in every slot — the value-writing initialization, whose
    /// result for these types is byte-for-byte C `zcalloc`'s zero fill.
    #[test]
    fn every_engine_element_type_is_default_initialized() {
        let stats = HookStats::new();

        let bytes: AllocBuffer<u8> = AllocBuffer::try_zeroed(5, stats.hook()).expect("u8");
        let words: AllocBuffer<u16> = AllocBuffer::try_zeroed(5, stats.hook()).expect("u16");
        let longs: AllocBuffer<u32> = AllocBuffer::try_zeroed(5, stats.hook()).expect("u32");

        assert!(bytes.iter().all(|&v| v == u8::default()));
        assert!(words.iter().all(|&v| v == u16::default()));
        assert!(longs.iter().all(|&v| v == u32::default()));
        assert!(bytes.is_foreign() && words.is_foreign() && longs.is_foreign());

        assert_eq!(stats.allocs(), 3);
        drop((bytes, words, longs));
        assert_eq!(stats.frees(), 3);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// `clone_foreign` allocates through the **same** hook, produces an
    /// independent region, and both regions are released. There is no
    /// global-allocator fallback anywhere on this path.
    #[test]
    fn clone_foreign_uses_the_same_hook_and_is_independent() {
        let stats = HookStats::new();

        {
            let mut original: AllocBuffer<u16> =
                AllocBuffer::try_zeroed(4, stats.hook()).expect("initial allocation");
            original[1] = 0x1234;
            assert!(original.is_foreign());

            let mut copy = original.try_clone().expect("clone allocation");
            assert!(
                copy.is_foreign(),
                "the copy must stay in the caller's arena"
            );
            assert_eq!(stats.allocs(), 2, "the clone must use the caller's zalloc");
            assert_eq!(&copy[..], &original[..]);

            // Mutating the copy leaves the original untouched.
            copy[1] = 0x5678;
            assert_eq!(original[1], 0x1234);
            assert_eq!(copy[1], 0x5678);
        }

        assert_eq!(stats.frees(), 2, "both regions must be released");
        assert_eq!(stats.live_bytes(), 0);
    }

    /// When the caller's allocator is exhausted, `try_clone` reports the failure
    /// instead of relocating the copy into the global heap, and the original
    /// stays fully usable.
    #[test]
    fn clone_foreign_propagates_oom_without_falling_back() {
        // Budget of exactly one: the initial allocation succeeds, the clone's
        // does not.
        let stats = HookStats::with_budget(1);

        {
            let mut original: AllocBuffer<u32> =
                AllocBuffer::try_zeroed(4, stats.hook()).expect("initial allocation");
            original[0] = 7;

            assert!(
                original.try_clone().is_none(),
                "an exhausted arena must produce None, not a global-allocator copy"
            );
            assert_eq!(stats.allocs(), 1);
            assert_eq!(stats.ooms(), 1);

            // The source is untouched and still usable after the failed copy.
            assert_eq!(original[0], 7);
            original[1] = 9;
            assert_eq!(&original[..], &[7, 9, 0, 0][..]);
        }

        assert_eq!(stats.frees(), 1, "only the surviving region is freed");
        assert_eq!(stats.live_bytes(), 0);
    }

    /// An owned (global-allocator) buffer clones into a fully **independent**
    /// region, so mutating the copy leaves the original untouched.
    ///
    /// The clone is still *fallible*: the owned arm goes through
    /// [`Vec::try_reserve_exact`], so global-heap exhaustion is reported as
    /// [`None`] rather than aborting, and the C copy entry points turn that into
    /// `Z_MEM_ERROR` (AAP §0.6.5). This four-byte reservation cannot realistically
    /// fail, which is why the test unwraps it.
    #[test]
    fn owned_try_clone_is_independent() {
        let mut original: AllocBuffer<u8> =
            AllocBuffer::try_zeroed(4, AllocHook::none()).expect("a 4-byte reservation succeeds");
        original[0] = 1;
        let mut copy = original.try_clone().expect("a 4-byte owned clone succeeds");
        assert!(!copy.is_foreign());
        copy[0] = 2;
        assert_eq!(original[0], 1);
        assert_eq!(copy[0], 2);
    }
}

#[cfg(test)]
mod foreign_alloc_tests {
    use super::{
        AllocHook, ForeignBuffer, default_zalloc, default_zalloc_bytes, default_zfree,
        fill_default, hook_request, try_alloc_foreign,
    };
    use crate::stream::AllocBuffer;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::alloc::Layout;
    use core::ffi::{c_uint, c_void};
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Bytes reserved in front of every region handed to the library so the test
    /// `zfree` can recover the payload size and *really* release the memory. 16
    /// keeps the payload 16-byte aligned (the alignment `malloc` guarantees), so
    /// the honest hooks below satisfy any element type the crate uses.
    const HDR: usize = 16;

    /// Byte pattern the honest test `zalloc` leaves in the region it returns. A C
    /// `zalloc` is *not* required to zero (zlib zeroes afterwards itself), so
    /// pre-filling with garbage is both realistic and what makes the
    /// initialization assertions below meaningful.
    const GARBAGE: u8 = 0xAA;

    static ARENA_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static ARENA_FREES: AtomicUsize = AtomicUsize::new(0);
    static MISALIGNED_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static MISALIGNED_FREES: AtomicUsize = AtomicUsize::new(0);
    static NEVER_CALLED: AtomicUsize = AtomicUsize::new(0);
    static OOM_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Layout of the `HDR + bytes` block backing one honest allocation.
    fn block_layout(bytes: usize) -> Layout {
        Layout::from_size_align(HDR + bytes, HDR).expect("test block layout is valid")
    }

    /// An honest caller hook: services the request from the Rust global
    /// allocator, records the payload size in the header, and fills the payload
    /// with [`GARBAGE`].
    unsafe extern "C" fn zalloc_arena(
        _opaque: *mut c_void,
        items: c_uint,
        size: c_uint,
    ) -> *mut c_void {
        ARENA_ALLOCS.fetch_add(1, Ordering::SeqCst);
        let bytes = (items as usize) * (size as usize);
        // SAFETY: `block_layout` has a non-zero size (`HDR` is 16), so `alloc`
        // is being called with a valid, non-zero layout.
        let base = unsafe { alloc::alloc::alloc(block_layout(bytes)) };
        if base.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `base` addresses `HDR + bytes` writable bytes and is 16-byte
        // aligned, so the `usize` header write and the payload fill are both in
        // bounds and aligned.
        unsafe {
            base.cast::<usize>().write(bytes);
            core::ptr::write_bytes(base.add(HDR), GARBAGE, bytes);
            base.add(HDR).cast::<c_void>()
        }
    }

    /// The matching deallocator: recovers the payload size from the header and
    /// releases the whole block, so the tests leak nothing.
    unsafe extern "C" fn zfree_arena(_opaque: *mut c_void, address: *mut c_void) {
        ARENA_FREES.fetch_add(1, Ordering::SeqCst);
        if address.is_null() {
            return;
        }
        // SAFETY: `address` is a payload pointer produced by `zalloc_arena`, so
        // `HDR` bytes before it lies the size header of a live block allocated
        // with `block_layout(bytes)`.
        unsafe {
            let base = address.cast::<u8>().sub(HDR);
            let bytes = base.cast::<usize>().read();
            alloc::alloc::dealloc(base, block_layout(bytes));
        }
    }

    /// A misbehaving caller hook: returns a deliberately odd address, which
    /// cannot satisfy the alignment of any multi-byte element type.
    unsafe extern "C" fn zalloc_misaligned(
        _opaque: *mut c_void,
        items: c_uint,
        size: c_uint,
    ) -> *mut c_void {
        MISALIGNED_ALLOCS.fetch_add(1, Ordering::SeqCst);
        let bytes = (items as usize) * (size as usize);
        // SAFETY: as in `zalloc_arena`; one extra byte is reserved so the odd
        // payload pointer still addresses `bytes` writable bytes.
        let base = unsafe { alloc::alloc::alloc(block_layout(bytes + 1)) };
        if base.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `base` addresses `HDR + bytes + 1` writable, 16-byte aligned
        // bytes, so the header write is aligned and `base + HDR + 1` is in bounds.
        unsafe {
            base.cast::<usize>().write(bytes + 1);
            base.add(HDR + 1).cast::<c_void>()
        }
    }

    unsafe extern "C" fn zfree_misaligned(_opaque: *mut c_void, address: *mut c_void) {
        MISALIGNED_FREES.fetch_add(1, Ordering::SeqCst);
        if address.is_null() {
            return;
        }
        // SAFETY: `address` is `base + HDR + 1` from `zalloc_misaligned`, so the
        // header sits `HDR + 1` bytes below it and records the block's payload.
        unsafe {
            let base = address.cast::<u8>().sub(HDR + 1);
            let bytes = base.cast::<usize>().read();
            alloc::alloc::dealloc(base, block_layout(bytes));
        }
    }

    /// A hook that is genuinely out of memory: it is expected to be consulted and
    /// to report failure, which must surface as an allocation failure rather than
    /// a silent global-allocator substitution.
    unsafe extern "C" fn zalloc_oom(
        _opaque: *mut c_void,
        _items: c_uint,
        _size: c_uint,
    ) -> *mut c_void {
        OOM_CALLS.fetch_add(1, Ordering::SeqCst);
        core::ptr::null_mut()
    }

    unsafe extern "C" fn zfree_oom(_opaque: *mut c_void, _address: *mut c_void) {}

    /// A hook that must never be reached: every test using it expects the request
    /// to be rejected *before* the C allocator is consulted.
    unsafe extern "C" fn zalloc_never(
        _opaque: *mut c_void,
        _items: c_uint,
        _size: c_uint,
    ) -> *mut c_void {
        NEVER_CALLED.fetch_add(1, Ordering::SeqCst);
        core::ptr::null_mut()
    }

    unsafe extern "C" fn zfree_never(_opaque: *mut c_void, _address: *mut c_void) {
        NEVER_CALLED.fetch_add(1, Ordering::SeqCst);
    }

    fn arena_hook() -> AllocHook {
        AllocHook::new(Some(zalloc_arena), Some(zfree_arena), core::ptr::null_mut())
    }

    fn misaligned_hook() -> AllocHook {
        AllocHook::new(
            Some(zalloc_misaligned),
            Some(zfree_misaligned),
            core::ptr::null_mut(),
        )
    }

    fn never_hook() -> AllocHook {
        AllocHook::new(Some(zalloc_never), Some(zfree_never), core::ptr::null_mut())
    }

    fn oom_hook() -> AllocHook {
        AllocHook::new(Some(zalloc_oom), Some(zfree_oom), core::ptr::null_mut())
    }

    /// An element type whose `Default` is **not** the all-zero bit pattern, while
    /// every bit pattern (including all-zero) is a perfectly valid value of it.
    ///
    /// That combination is what makes this type the regression guard for the
    /// soundness defect: both a byte-zeroing fill and a `T::default()` fill are
    /// fully defined behaviour on it, and the two are *observably different*, so
    /// an assertion over the contents distinguishes them without ever relying on
    /// undefined behaviour to fail. `OnlyOne` below cannot serve that purpose —
    /// a single-variant enum has exactly one inhabitant, so the compiler folds
    /// any equality check on it to `true`.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[repr(transparent)]
    struct Marker(u16);

    impl Marker {
        /// The distinctive, non-zero value [`Marker::default`] produces.
        const PATTERN: u16 = 0xBEEF;
    }

    impl Default for Marker {
        fn default() -> Self {
            Marker(Marker::PATTERN)
        }
    }

    /// An element type whose valid representations are **all non-zero**, so an
    /// all-zero region would not hold a single valid value of it. Two variants
    /// are declared deliberately: with only one inhabitant the compiler would
    /// discharge every comparison statically and the assertion would be vacuous.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    #[repr(u8)]
    enum NonZeroTag {
        Lo = 1,
        #[default]
        Hi = 2,
    }

    /// A zero-sized element type: it has no C-representable footprint.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    struct Zst;

    /// The integer buffers the engines actually request must come back holding
    /// zeros — the `zmemzero` C `zcalloc` performs — even though the hook handed
    /// back a region full of `GARBAGE`, and must be released through `zfree`.
    #[test]
    fn integer_buffer_is_zeroed_and_released() {
        let frees_before = ARENA_FREES.load(Ordering::SeqCst);
        {
            let buf: Box<dyn ForeignBuffer<u16>> =
                try_alloc_foreign::<u16>(arena_hook(), 33).expect("honest hook must succeed");
            assert_eq!(buf.as_slice().len(), 33);
            assert!(buf.as_slice().iter().all(|&v| v == 0));
        }
        assert!(
            ARENA_FREES.load(Ordering::SeqCst) > frees_before,
            "dropping the buffer must call the caller's zfree"
        );
    }

    /// Regression guard for the soundness defect this module was hardened for:
    /// the region must be initialized by *writing `T::default()` values*, not by
    /// zeroing raw bytes. `Marker::default()` is `0xBEEF`, so a byte-zeroing fill
    /// would leave every element holding `0` and fail this assertion — while both
    /// outcomes stay fully defined behaviour, which is what makes the guard
    /// trustworthy rather than optimizer-dependent.
    ///
    /// The guard is applied to [`fill_default`], the initializer
    /// `try_alloc_foreign` runs over the region a caller's `zalloc` returned,
    /// because it needs an element type whose `Default` is not all-zero and no
    /// member of the sealed `ZeroValid` set can be one. Exercising the initializer
    /// directly is what keeps the property observable while the second remedy —
    /// the sealed element-type bound on the allocation path itself — stays in
    /// force.
    #[test]
    fn default_valued_elements_are_written_not_byte_zeroed() {
        assert_ne!(Marker::PATTERN, 0, "the guard needs a non-zero default");

        let mut slots = [MaybeUninit::<Marker>::uninit(); 7];
        fill_default(&mut slots);

        // SAFETY: `fill_default` wrote a valid `Marker` into every one of the 7
        // slots immediately above, so each is initialized; `Marker` is `Copy`, so
        // reading one out leaves the slot untouched.
        let vals: Vec<Marker> = slots.iter().map(|s| unsafe { s.assume_init() }).collect();
        assert_eq!(vals.len(), 7);
        assert!(
            vals.iter().all(|&v| v == Marker::default()),
            "every element must hold T::default(), got {vals:?}"
        );
        assert!(
            vals.iter().all(|&v| v != Marker(0)),
            "a byte-zeroing fill must not be able to pass this guard"
        );
    }

    /// The companion validity case: for an element type with no valid all-zero
    /// representation, the initializer must still leave the region holding valid
    /// values. A byte-zeroing fill would materialize the invalid discriminant `0`
    /// here, which is the undefined behaviour this module rules out — twice over,
    /// since such a type additionally cannot satisfy the sealed `ZeroValid` bound
    /// the allocation path requires.
    #[test]
    fn non_zero_discriminant_elements_are_initialized_with_valid_values() {
        // Both variants are inhabited, so the comparison below is a real runtime
        // discriminant check rather than a statically discharged tautology.
        assert_ne!(NonZeroTag::default(), NonZeroTag::Lo);

        let mut slots = [MaybeUninit::<NonZeroTag>::uninit(); 7];
        fill_default(&mut slots);

        // SAFETY: `fill_default` wrote a valid `NonZeroTag` into every one of the
        // 7 slots immediately above, so each is initialized; `NonZeroTag` is
        // `Copy`, so reading one out leaves the slot untouched.
        let vals: Vec<NonZeroTag> = slots.iter().map(|s| unsafe { s.assume_init() }).collect();
        assert_eq!(vals.len(), 7);
        assert!(vals.iter().all(|&v| v == NonZeroTag::default()));
    }

    /// A zero-sized element type is rejected without consulting the C hook (the
    /// caller serves such requests from the global allocator, where they need no
    /// allocation at all).
    ///
    /// Asserted against [`hook_request`], the pre-hook gate `try_alloc_foreign`
    /// runs before it touches `zalloc`: a zero-sized type cannot satisfy the
    /// sealed `ZeroValid` bound on the allocation path, so the gate is the only
    /// place the rejection is reachable — and, having no hook to consult, it makes
    /// "the hook is never called" structural rather than merely observed.
    #[test]
    fn zero_sized_element_type_is_rejected_before_the_hook() {
        assert_eq!(core::mem::size_of::<Zst>(), 0, "the guard needs a ZST");
        assert!(
            hook_request::<Zst>(4, core::mem::size_of::<Zst>()).is_none(),
            "a zero element size is not expressible to the C hook"
        );
        // The rejection is specific to the zero element size, not to the count.
        assert!(
            hook_request::<u8>(4, core::mem::size_of::<u8>()).is_some(),
            "a sized request is accepted"
        );

        // No hook took part in any of the above, and none may be consulted for a
        // request the gate rejects.
        assert_eq!(NEVER_CALLED.load(Ordering::SeqCst), 0);
    }

    /// A request whose byte size cannot be described by `Layout::array` (it
    /// exceeds `isize::MAX`) is rejected without consulting the C hook.
    #[test]
    fn layout_overflowing_request_is_rejected_before_the_hook() {
        assert!(try_alloc_foreign::<u32>(never_hook(), usize::MAX / 2).is_none());
        assert_eq!(NEVER_CALLED.load(Ordering::SeqCst), 0);
    }

    /// A request that is layout-valid but not representable in the C hook's
    /// `uInt` ABI is rejected: `count` itself may overflow `uInt`, and so may the
    /// `items * size` product the hook computes internally.
    #[test]
    fn hook_abi_unrepresentable_requests_are_rejected() {
        // `count` alone exceeds `uInt` (only reachable on a 64-bit target).
        if usize::BITS > c_uint::BITS {
            let too_many = (c_uint::MAX as usize) + 1;
            assert!(try_alloc_foreign::<u8>(never_hook(), too_many).is_none());

            // `items` and `size` each fit, but `items * size` wraps `uInt`: the
            // smallest `u16` count whose byte total exceeds `c_uint::MAX`.
            let elem = core::mem::size_of::<u16>();
            let wrapping = (c_uint::MAX as usize) / elem + 1;
            assert!(c_uint::try_from(wrapping).is_ok(), "items alone must fit");
            assert!(
                wrapping * elem > c_uint::MAX as usize,
                "the product must exceed uInt"
            );
            assert!(try_alloc_foreign::<u16>(never_hook(), wrapping).is_none());
        }
        assert_eq!(NEVER_CALLED.load(Ordering::SeqCst), 0);
    }

    /// A hook that returns an address which cannot satisfy the element type's
    /// alignment is treated as an allocation failure, and the region is handed
    /// straight back through the caller's `zfree` rather than leaked.
    #[test]
    fn misaligned_hook_region_is_rejected_and_returned() {
        let frees_before = MISALIGNED_FREES.load(Ordering::SeqCst);
        assert!(try_alloc_foreign::<u32>(misaligned_hook(), 8).is_none());
        assert!(
            MISALIGNED_FREES.load(Ordering::SeqCst) > frees_before,
            "a rejected region must be released through the caller's zfree"
        );

        // A byte buffer has alignment 1, so the same odd address is acceptable
        // there and the allocation succeeds.
        let buf: Box<dyn ForeignBuffer<u8>> =
            try_alloc_foreign::<u8>(misaligned_hook(), 8).expect("u8 needs no alignment");
        assert_eq!(buf.as_slice(), &[0u8; 8]);
    }

    /// The same contract observed through the **safe entry point** callers
    /// actually use, [`AllocBuffer::try_zeroed`]: a request the C ABI cannot
    /// express must be served from the global allocator — which for an empty
    /// request allocates nothing — instead of being pushed through the hook. This
    /// lives here rather than in `stream.rs` because declaring a C hook requires
    /// `unsafe`, which is confined to this module zone (AAP §0.6.2); the behaviour
    /// under test is `stream.rs`'s.
    ///
    /// The companion zero-sized-element case is stronger than a runtime check: a
    /// zero-sized type cannot satisfy the sealed `ZeroValid` bound, so
    /// `AllocBuffer::<Zst>::try_zeroed` does not compile at all (the `compile_fail`
    /// doctest on `crate::stream::ZeroValid` pins that), and the gate assertion in
    /// `zero_sized_element_type_is_rejected_before_the_hook` covers the path that
    /// would serve it.
    #[test]
    fn try_zeroed_serves_unhookable_requests_without_the_hook() {
        let hook = never_hook();
        assert!(hook.is_active());

        let buf: AllocBuffer<u16> =
            AllocBuffer::try_zeroed(0, hook).expect("an empty request must succeed");
        assert_eq!(buf.len(), 0);
        assert!(matches!(buf, AllocBuffer::Owned(_)));
        assert!(
            hook_request::<u16>(0, core::mem::size_of::<u16>()).is_none() || buf.is_empty(),
            "an empty request is not pushed through the C hook"
        );
        assert_eq!(
            NEVER_CALLED.load(Ordering::SeqCst),
            0,
            "an unhookable request must never reach the C hook"
        );
    }

    /// A request an active hook cannot serve is an allocation failure — [`None`],
    /// which every caller maps to `Z_MEM_ERROR` — and must never degrade into a
    /// global-allocator buffer (AAP §0.6.3 has-hook clause). Also pins *when* the
    /// hook is consulted: a genuine OOM reaches it, an unrepresentable size does
    /// not (AAP §0.6.5 allocation-timing parity).
    #[test]
    fn try_zeroed_reports_unserviceable_hook_requests_as_failure() {
        let hook = oom_hook();
        let calls_before = OOM_CALLS.load(Ordering::SeqCst);

        // Out of memory: the hook is consulted and reports failure.
        let oom: Option<AllocBuffer<u16>> = AllocBuffer::try_zeroed(8, hook);
        assert!(
            oom.is_none(),
            "hook OOM must surface as a failure, not a global-allocator buffer"
        );
        assert_eq!(
            OOM_CALLS.load(Ordering::SeqCst),
            calls_before + 1,
            "a serviceable-looking request must reach the hook"
        );
    }

    /// A size that cannot be expressed to the hook is rejected *before* the hook
    /// is consulted, through the safe entry point as well as the raw one.
    #[test]
    fn try_zeroed_rejects_unrepresentable_sizes_before_the_hook() {
        let huge: Option<AllocBuffer<u32>> = AllocBuffer::try_zeroed(usize::MAX / 2, never_hook());
        assert!(huge.is_none(), "an unrepresentable size must fail");
        assert_eq!(
            NEVER_CALLED.load(Ordering::SeqCst),
            0,
            "an unrepresentable size must be rejected before the hook"
        );
    }

    /// [`default_zalloc`] / [`default_zfree`] — the crate's `zcalloc`/`zcfree`
    /// counterparts that [`crate::ffi::types::init_allocator_prologue`]
    /// substitutes for a caller's missing allocator half.
    ///
    /// The round trip pins the three properties every consumer relies on: the
    /// returned region really is `items * size` writable bytes, `opaque` is
    /// ignored (C's `zcalloc` never reads it, `zutil.c` L299-L302), and the
    /// matching `default_zfree` releases it.
    #[test]
    fn builtin_hooks_round_trip_a_region() {
        const ITEMS: c_uint = 64;
        const SIZE: c_uint = 4;

        // A deliberately non-null `opaque`: neither built-in may dereference it.
        let cookie = core::ptr::without_provenance_mut::<c_void>(0xDEAD_BEEF);

        // SAFETY: `default_zalloc` ignores `opaque` and imposes no obligation on
        // its arguments beyond the ordinary zlib `zalloc` contract.
        let raw = unsafe { default_zalloc(cookie, ITEMS, SIZE) };
        assert!(!raw.is_null(), "malloc must serve a 256-byte request");

        let bytes = (ITEMS as usize) * (SIZE as usize);
        // SAFETY: `raw` is non-null and, per the `zalloc` contract, addresses at
        // least `bytes` writable bytes that nothing else aliases. `u8` needs no
        // alignment beyond 1, which `malloc` always satisfies.
        let region = unsafe { core::slice::from_raw_parts_mut(raw.cast::<u8>(), bytes) };
        region.fill(0xA5);
        assert!(region.iter().all(|&b| b == 0xA5), "the region is writable");

        // SAFETY: `raw` came from `default_zalloc` (i.e. `malloc`) and has not
        // been released; this is its single, matching deallocation.
        unsafe { default_zfree(cookie, raw) };
    }

    /// The sizing decision behind [`default_zalloc`], tested as the arithmetic
    /// statement it is: **every** request that cannot become an addressable
    /// region is rejected, and every request that can is passed through
    /// unchanged.
    ///
    /// Verifying the pure function rather than only the allocator is what makes
    /// the guarantee independent of the host's free memory, of whether `malloc`
    /// chooses to refuse an absurd size, and — the failure this test exists to
    /// prevent — of whether the optimizer decided to keep the `malloc` call at
    /// all. It held in a debug build and did not hold at `opt-level = 3` with one
    /// codegen unit when the ceiling was delegated to `malloc`.
    #[test]
    fn default_zalloc_rejects_every_unrepresentable_request() {
        // The ceiling, expressed the way the guard expresses it.
        let cap = isize::MAX as usize;

        // --- Serviceable requests pass through with the exact product ---------
        for (items, size) in [(0u32, 1u32), (1, 0), (0, 0), (1, 1), (64, 4), (4096, 1024)] {
            assert_eq!(
                default_zalloc_bytes(items, size),
                Some((items as usize) * (size as usize)),
                "a serviceable {items}x{size} request must pass through unchanged"
            );
        }

        // The largest single-item request this target can address at all. On a
        // 64-bit target that is `c_uint::MAX`; on a 32-bit target the ceiling
        // itself, `isize::MAX`, is the smaller of the two.
        let widest = c_uint::try_from(cap.min(c_uint::MAX as usize))
            .expect("the minimum of the two ceilings always fits a c_uint");
        assert_eq!(
            default_zalloc_bytes(1, widest),
            Some(widest as usize),
            "the widest addressable single-item request must be served"
        );

        // --- Unrepresentable requests are rejected ---------------------------
        // `0xFFFF_FFFF * 0xFFFF_FFFF`: C's `zcalloc` wraps this in `unsigned`
        // arithmetic to `malloc(1)` and hands back a one-byte region. Rejected
        // here by clause 2 on a 64-bit target and by clause 1 on a 32-bit one, so
        // both widths answer identically.
        assert_eq!(
            default_zalloc_bytes(c_uint::MAX, c_uint::MAX),
            None,
            "a product that wraps C's `unsigned` multiplication must be rejected"
        );

        // The tightest possible straddle of the ceiling for this target: two
        // requests one `items`-multiple apart, one addressable and one not. This
        // is the case a `checked_mul`-only guard cannot see, because the product
        // is perfectly representable in `usize`.
        #[cfg(target_pointer_width = "64")]
        {
            // 0xFFFF_FFFF * 0x8000_0000 == 9_223_372_034_707_292_160 <= isize::MAX
            assert_eq!(
                default_zalloc_bytes(c_uint::MAX, 0x8000_0000),
                Some(0xFFFF_FFFFusize * 0x8000_0000usize),
                "the largest in-range c_uint product must still be served"
            );
            // 0xFFFF_FFFF * 0x8000_0001 == 9_223_372_039_002_259_455 >  isize::MAX
            let over = 0xFFFF_FFFFusize * 0x8000_0001usize;
            assert!(over > cap, "the companion case must exceed the ceiling");
            assert_eq!(
                default_zalloc_bytes(c_uint::MAX, 0x8000_0001),
                None,
                "a product representable in usize but above isize::MAX must be \
                 rejected — `checked_mul` alone never sees this case"
            );
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(
                default_zalloc_bytes(1, 0x7FFF_FFFF),
                Some(0x7FFF_FFFF),
                "exactly isize::MAX bytes must still be served"
            );
            assert_eq!(
                default_zalloc_bytes(1, 0x8000_0000),
                None,
                "one byte past isize::MAX must be rejected even though `usize` \
                 holds it"
            );
        }

        // Breadth coverage. Every pair below is beyond reach on *both* supported
        // pointer widths — on a 64-bit target by clause 2, on a 32-bit one by
        // clause 1 — so the table needs no `cfg`. The premise of each row is
        // re-established in `u128`, which cannot overflow for any `c_uint` pair
        // and shares no arithmetic with the guard under test, so a mistyped
        // fixture fails loudly instead of passing vacuously.
        for (items, size) in [
            (c_uint::MAX, 0xC000_0000u32), // 13_835_058_052_060_938_240
            (0xC000_0000, c_uint::MAX),    // 13_835_058_052_060_938_240
            (0xE000_0000, 0xE000_0000),    // 14_123_288_431_433_875_456
            (0x9000_0000, 0xF000_0000),    //  9_727_775_195_120_271_360
        ] {
            let exact = u128::from(items) * u128::from(size);
            assert!(
                exact > cap as u128,
                "fixture {items:#X}x{size:#X} is actually within this target's reach"
            );
            assert_eq!(
                default_zalloc_bytes(items, size),
                None,
                "{items:#X}x{size:#X} can never become an addressable region"
            );
        }
    }

    /// A request whose `items * size` product cannot be represented is reported
    /// as an allocation failure (null), never as a wrapped — and therefore
    /// undersized — region. This is the one deliberate refinement over C's
    /// `unsigned` multiplication in `zcalloc`.
    ///
    /// Every operand and every answer is routed through
    /// [`core::hint::black_box`] so the assertions describe what the shipped code
    /// really does: without it a release build is free to fold the whole call
    /// chain to a constant, and the test would then be checking the optimizer's
    /// arithmetic rather than the allocator's guard.
    ///
    /// `default_zfree(_, NULL)` is a no-op, exactly as C's `zcfree` is over
    /// `free(NULL)`.
    #[test]
    fn builtin_zalloc_reports_an_overflowing_product_as_failure() {
        use core::hint::black_box;

        // `0xFFFF_FFFF * 0xFFFF_FFFF` overflows `u32` (C would wrap it to 1 and
        // return a one-byte region) and exceeds `isize::MAX`; on a 32-bit host the
        // `usize` product overflows as well. Both clauses of the guard reject it,
        // so every supported width answers null.
        // SAFETY: as in `builtin_hooks_round_trip_a_region`.
        let raw = unsafe {
            default_zalloc(
                black_box(core::ptr::null_mut()),
                black_box(c_uint::MAX),
                black_box(c_uint::MAX),
            )
        };
        assert!(
            black_box(raw).is_null(),
            "an unrepresentable request must fail, not wrap to a tiny region"
        );

        // A product no reference can span. Which guard rejects it depends on the
        // target: on a 64-bit `usize` the product fits comfortably and only the
        // `isize::MAX` clause catches it, so a `checked_mul`-only guard would
        // forward it to `malloc`; on a 32-bit `usize` the multiplication itself
        // overflows and `checked_mul` rejects it first. Either way the request must
        // fail, which is what the assertion below requires.
        // SAFETY: as above.
        let over = unsafe {
            default_zalloc(
                black_box(core::ptr::null_mut()),
                black_box(c_uint::MAX),
                black_box(0x8000_0001),
            )
        };
        assert!(
            black_box(over).is_null(),
            "a request above isize::MAX must fail without consulting malloc"
        );

        // The refinement must not spill onto serviceable requests: the same
        // function, called through the same optimization-opaque path, still
        // serves a real one.
        // SAFETY: as above.
        let fine = unsafe {
            default_zalloc(
                black_box(core::ptr::null_mut()),
                black_box(16),
                black_box(8),
            )
        };
        assert!(
            !black_box(fine).is_null(),
            "a 128-byte request must still be served"
        );
        // SAFETY: `fine` came from `default_zalloc` (i.e. `malloc`) and has not
        // been released; this is its single, matching deallocation.
        unsafe { default_zfree(core::ptr::null_mut(), fine) };

        // SAFETY: a null address is explicitly permitted and is a no-op.
        unsafe { default_zfree(core::ptr::null_mut(), core::ptr::null_mut()) };
    }

    /// The built-in-backed counting hooks used by the substituted-half tests in
    /// `crate::ffi::deflate` and `crate::ffi::inflate` must themselves be
    /// balanced and interchangeable with the built-ins: a region from
    /// `BuiltinHookStats::zalloc_fn` is releasable by [`default_zfree`], and a
    /// region from [`default_zalloc`] is releasable by
    /// `BuiltinHookStats::zfree_fn`. That cross-compatibility is exactly what
    /// makes C's per-half substitution sound.
    ///
    /// A **null** `opaque` is used throughout, because that is what C's prologue
    /// leaves behind whenever it substitutes `zcalloc`, and therefore what a
    /// caller's surviving `zfree` is actually invoked with.
    #[test]
    fn builtin_backed_counting_hooks_compose_with_the_built_ins() {
        use super::test_hook::BuiltinHookStats;

        let stats = BuiltinHookStats::new();
        let (zalloc, zfree) = (stats.zalloc_fn(), stats.zfree_fn());
        let no_cookie = core::ptr::null_mut::<c_void>();

        // Counting `zalloc` -> built-in `zfree` (the "caller supplied only
        // `zalloc`" pairing).
        let a = zalloc(no_cookie, 32, 2);
        assert!(!a.is_null());
        assert_eq!(stats.allocs(), 1);
        // SAFETY: `a` came from the counting hook, which forwards to
        // `default_zalloc`/`malloc`; this is its single deallocation.
        unsafe { default_zfree(no_cookie, a) };

        // Built-in `zalloc` -> counting `zfree` (the "caller supplied only
        // `zfree`" pairing).
        // SAFETY: `opaque` is ignored by `default_zalloc`.
        let b = unsafe { default_zalloc(no_cookie, 32, 2) };
        assert!(!b.is_null());
        zfree(no_cookie, b);
        assert_eq!(stats.frees(), 1);

        // A null address is a no-op and is not counted.
        zfree(no_cookie, core::ptr::null_mut());
        assert_eq!(stats.frees(), 1, "a null free must not be counted");
    }
}
