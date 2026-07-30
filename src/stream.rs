//! The idiomatic streaming state object [`ZStream`] — the safe-Rust port of the
//! C `z_stream` (`zlib.h` L88-L110) — together with the crate-local
//! [`Allocator`] abstraction that replaces the C `zalloc`/`zfree` hooks.
//!
//! `ZStream` is the **central owning handle** of the whole library and is
//! re-exported from the crate root (`lib.rs`). Every compression or
//! decompression operation borrows a `&mut ZStream`; the concrete engine logic
//! lives in [`crate::deflate`] and [`crate::inflate`], which operate on the
//! [`DeflateState`] / [`InflateState`] this handle owns.
//!
//! # Relationship to the C `z_stream`
//!
//! The C library models a stream as a `z_stream` struct that the caller
//! allocates and whose opaque `internal_state *state` pointer is filled in by
//! `deflateInit`/`inflateInit` and torn down by `deflateEnd`/`inflateEnd`. That
//! design has two classic hazards this port makes *unrepresentable*:
//!
//! * A `state` pointer that is dangling, `NULL`, or points at the *wrong* kind
//!   of engine (an inflate routine handed a deflate state, or vice-versa).
//! * A manual `zcfree` that is forgotten (leak) or performed twice
//!   (use-after-free / double-free).
//!
//! Here the engine state is owned through a `StreamState` enum that holds
//! *at most one* boxed engine (`None`, `Deflate(Box<…>)`, or
//! `Inflate(Box<…>)`), so "no state", "deflate state", and "inflate state" are
//! distinct, checked cases (AAP §0.3.2 Type-State / Ownership). Because the box
//! and every owned buffer inside it are released by `Drop`, RAII fully subsumes
//! `deflateEnd`/`inflateEnd` (AAP §0.3.2 RAII / §0.6.3 Memory Ownership Model) —
//! there is nothing for the caller to remember to free.
//!
//! ## Field mapping (`z_stream` → [`ZStream`])
//!
//! | C field (`z_stream`)          | Rust                                   |
//! |-------------------------------|----------------------------------------|
//! | `next_in` / `next_out`        | *not stored* — passed as slices to the deflate/inflate methods |
//! | `avail_in` / `avail_out`      | *not stored* — the length of those slices; reconstructed at the FFI boundary |
//! | `total_in` / `total_out`      | [`total_in`](ZStream::total_in) / [`total_out`](ZStream::total_out) (`u64`) |
//! | `msg`                         | [`msg`](ZStream::msg) (`Option<&'static str>`) |
//! | `state`                       | `state` (`StreamState`, owned) |
//! | `zalloc` / `zfree` / `opaque` | `alloc` (`A: Allocator`) |
//! | `data_type`                   | [`data_type`](ZStream::data_type) (`i32`) |
//! | `adler`                       | [`adler`](ZStream::adler) (`u32`) |
//! | `reserved`                    | `reserved` (`u32`, parity only) |
//!
//! The raw `next_in`/`avail_in`/`next_out`/`avail_out` cursor quadruple is
//! **not** mirrored as fields: the idiomatic API accepts input and output as
//! Rust slices (for example `deflate(&mut self, input: &[u8], output: &mut [u8],
//! flush: FlushMode)`), and the running [`total_in`](ZStream::total_in) /
//! [`total_out`](ZStream::total_out) counters record cumulative progress. When
//! the crate is driven through its C ABI, the `ffi` layer rebuilds those slices
//! from the caller's raw `z_stream.next_in`/`avail_in`/… on each call and writes
//! the advanced cursors back afterwards.
//!
//! # Allocation
//!
//! The C `z_stream` carries `zalloc`/`zfree` function pointers so callers can
//! supply a custom allocator (for arenas, memory accounting, or embedded
//! heaps). That capability is modelled by the [`Allocator`] trait: the default
//! [`DefaultAllocator`] routes through the Rust global allocator, while the
//! `ffi` layer supplies a [`CAllocator`](crate::ffi::CAllocator) implementation
//! that forwards to the caller's `zalloc`/`zfree` (AAP §0.6.3, "has-hook
//! clause"). Every working buffer the engines request —
//! [`allocate_zeroed`](Allocator::allocate_zeroed) — is returned as an
//! [`AllocBuffer`], a smart owned region that is **either** a global-allocator
//! [`Vec`] (the null-hook default, byte-for-byte the historical behavior)
//! **or** a foreign region obtained from the caller's `zalloc` and released
//! through their `zfree` when the box drops. The engines only ever see a slice
//! ([`Deref`]/[`DerefMut`]), so which backing store is in play is invisible to
//! the compression/decompression logic and requires **no** `unsafe` on their
//! side.
//!
//! # Safety, `no_std`
//!
//! This module contains **no executable `unsafe`** — no `unsafe` block, no
//! `unsafe fn`, no `unsafe impl` — which `#![deny(unsafe_code)]` below enforces.
//! The word appears here exactly twice, and both occurrences are *declarative*:
//! the [`ZallocFn`] and [`ZfreeFn`] type aliases must spell out
//! `unsafe extern "C" fn` because that is the type of the C hook pointers this
//! module has to name in order to interoperate with them. Naming a function
//! type performs no unsafe operation.
//!
//! The raw-pointer work for a caller-`zalloc`'d region — invoking those C
//! function pointers, materializing a slice over the returned memory, and
//! freeing it on drop — is defined entirely in the sanctioned
//! `crate::ffi::alloc` zone behind the safe [`ForeignBuffer`] trait. The
//! [`Foreign`](AllocBuffer::Foreign) arm holds a `Box<dyn ForeignBuffer<T>>` and
//! delegates every access to that safe interface. Consequently the **core
//! compression engine** (`src/deflate/**`) also contains zero `unsafe` (User
//! Constraint 3 / AAP §0.6.2 and standard S2) — its buffer fields are
//! [`AllocBuffer`]s accessed purely through safe slice operations, and the
//! compression-logic files (`slow.rs`/`stored.rs`/`trees.rs`) remain under
//! `#![deny(unsafe_code)]`.
//!
//! Allocation is **fallible** at the boundary: when a caller installs a bounded
//! allocator whose `zalloc` reports out-of-memory, buffer construction returns
//! [`None`] and the init paths surface `Z_MEM_ERROR` rather than silently using
//! the global allocator — preserving C's allocation count and failure timing
//! (AAP §0.6.3 has-hook clause, §0.6.5). The module is `no_std` + `alloc`
//! compatible: it references only `core`, `alloc`, and the crate's own modules.
#![deny(unsafe_code)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::{c_uint, c_void};
use core::fmt;
use core::ops::{Deref, DerefMut};

use crate::constants::DataType;
use crate::deflate::state::DeflateState;
use crate::error::ReturnCode;
use crate::inflate::state::InflateState;

// ===========================================================================
// Allocator abstraction (replaces the C `zalloc`/`zfree`/`opaque` triple)
// ===========================================================================

// ---------------------------------------------------------------------------
// ZeroValid — the sealed element-type set a foreign zero-fill may materialize
// ---------------------------------------------------------------------------

/// Sealing module for [`ZeroValid`].
///
/// Because [`ZeroValid`] has a private supertrait, it can neither be named nor
/// implemented from outside this crate. The set of element types that may be
/// materialized from a caller-`zalloc`'d, byte-zeroed region is therefore fixed
/// here and cannot be widened by a downstream crate.
mod sealed {
    /// Private sealing marker; see [`super::ZeroValid`].
    pub trait ZeroValidSealed {}
}

/// Marker for element types whose **all-zero bit pattern is a valid value that
/// equals [`Default::default`]**.
///
/// # Why this bound exists
///
/// [`AllocBuffer::try_zeroed`] may route a working buffer's storage through a
/// caller-supplied C `zalloc` hook. The sanctioned `crate::ffi::alloc` bridge
/// byte-zeroes that region and then hands it back as `&[T]` / `&mut [T]`.
/// `T: Copy + Default` alone does **not** license that step:
///
/// * [`Default::default`] may return a non-zero value, in which case a
///   "zeroed" buffer would not hold the type's default at all; and, far worse,
/// * a type with niches — any `enum`, `bool`, `char`, `NonZero*`, a reference,
///   or a function pointer — can have an all-zero byte pattern that is an
///   **invalid** value. A `#[repr(u8)] enum E { A = 1 }` with a hand-written
///   `Default` is a two-line counterexample. Producing a `&[E]` over
///   zeroed storage is immediate undefined behavior, and
///   [`AllocBuffer::try_zeroed`] is a *safe* function, so that UB would be
///   reachable without the caller ever writing `unsafe`.
///
/// Requiring `T: ZeroValid` closes that hole at compile time on every call site,
/// and sealing the trait prevents a downstream crate from adding an unsound
/// implementation.
///
/// # Implementation contract
///
/// A type may implement `ZeroValid` only when **all** of the following hold:
///
/// 1. every bit pattern of the type is a valid value (no niches, no padding
///    bytes, no uninhabited variants);
/// 2. the all-zero bit pattern equals `Self::default()`;
/// 3. the type has no drop glue — guaranteed here by the [`Copy`] supertrait.
///
/// Every primitive integer type satisfies all three, and those are exactly the
/// types implemented below. The compression and decompression engines only ever
/// request `u8`, `u16`, and `u32` buffers.
///
/// # Examples
///
/// The engines' element types are accepted:
///
/// ```
/// # use zlib_rs::stream::{AllocBuffer, AllocHook};
/// let bytes = AllocBuffer::<u8>::try_zeroed(4, AllocHook::none()).unwrap();
/// let words = AllocBuffer::<u16>::try_zeroed(4, AllocHook::none()).unwrap();
/// assert_eq!(&bytes[..], &[0u8; 4]);
/// assert_eq!(&words[..], &[0u16; 4]);
/// ```
///
/// A type whose all-zero bit pattern is **not** a valid value is rejected at
/// compile time, so the unsound path is unreachable rather than merely untested:
///
/// ```compile_fail
/// # use zlib_rs::stream::{AllocBuffer, AllocHook};
/// #[derive(Clone, Copy)]
/// #[repr(u8)]
/// enum OneOnly {
///     One = 1,
/// }
///
/// impl Default for OneOnly {
///     fn default() -> Self {
///         OneOnly::One
///     }
/// }
///
/// // `OneOnly` is `Copy + Default` but not `ZeroValid`: discriminant 0 is not a
/// // valid `OneOnly`. This line fails to compile.
/// let _ = AllocBuffer::<OneOnly>::try_zeroed(1, AllocHook::none());
/// ```
pub trait ZeroValid: sealed::ZeroValidSealed + Copy + Default {}

/// Implements the sealing marker and [`ZeroValid`] for each listed type.
///
/// Only invoked with primitive integer types, which satisfy every clause of the
/// [`ZeroValid`] implementation contract.
macro_rules! impl_zero_valid {
    ($($t:ty),+ $(,)?) => {
        $(
            impl sealed::ZeroValidSealed for $t {}
            impl ZeroValid for $t {}
        )+
    };
}

// Every primitive integer type: all bit patterns valid, no padding, all-zero
// equals `Default::default()`, and no drop glue. Deliberately excludes `bool`,
// `char`, floating-point types, references, function pointers, `NonZero*`, and
// every `enum` — for those, either zero is not a valid value or zero is not the
// `Default`.
impl_zero_valid!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize
);

// ---------------------------------------------------------------------------
// Caller-allocator hook (the C `zalloc`/`zfree`/`opaque` triple)
// ---------------------------------------------------------------------------

/// C `voidpf (*alloc_func)(voidpf opaque, uInt items, uInt size)` — a caller's
/// allocation hook (`zlib.h` L85). Structurally identical to
/// [`crate::ffi::alloc_func`]'s inner function-pointer type.
pub type ZallocFn = unsafe extern "C" fn(*mut c_void, c_uint, c_uint) -> *mut c_void;

/// C `void (*free_func)(voidpf opaque, voidpf address)` — a caller's
/// deallocation hook (`zlib.h` L86).
pub type ZfreeFn = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// A caller-supplied `zalloc`/`zfree`/`opaque` triple, or "none".
///
/// This is the plumbing that lets [`AllocBuffer`] route a working buffer's
/// storage through the caller's C allocator (AAP §0.6.3). It is a plain [`Copy`]
/// value carried by every [`Allocator`] (see [`Allocator::hook`]); the default
/// allocator carries [`AllocHook::none`], so the historical global-allocator
/// path is entirely unchanged.
///
/// A hook is *active* only when **both** `zalloc` and `zfree` are present — a
/// caller that supplies one without the other does not get a usable custom
/// allocator (matching zlib, which uses the built-in allocator unless both are
/// set), and allocation falls back to the global allocator.
///
/// # Constructing one
///
/// Only [`AllocHook::none`] is reachable from outside the crate. Building an
/// *active* hook requires the crate-private `AllocHook::new`, whose
/// documentation states the obligations the raw-pointer code in
/// `crate::ffi::alloc` then relies on. Keeping that constructor private is
/// what makes those obligations enforceable: the sole caller is
/// `src/ffi/types.rs`, converting a `z_stream` whose validity the FFI entry
/// point has already established. An out-of-crate [`Allocator`] implementation
/// therefore cannot smuggle arbitrary C function pointers into `unsafe` code
/// through safe API.
#[derive(Clone, Copy)]
pub struct AllocHook {
    /// The caller's allocation hook (`z_stream.zalloc`), or `None`.
    zalloc: Option<ZallocFn>,
    /// The caller's deallocation hook (`z_stream.zfree`), or `None`.
    zfree: Option<ZfreeFn>,
    /// The caller's private cookie (`z_stream.opaque`), passed to both hooks.
    opaque: *mut c_void,
}

impl AllocHook {
    /// The "no custom allocator" hook: allocation uses the Rust global
    /// allocator, exactly as a C caller leaving `zalloc`/`zfree` as `Z_NULL`.
    #[inline]
    #[must_use]
    pub const fn none() -> Self {
        Self {
            zalloc: None,
            zfree: None,
            opaque: core::ptr::null_mut(),
        }
    }

    /// Builds a hook from a caller's `zalloc`/`zfree`/`opaque` triple.
    ///
    /// # Construction contract
    ///
    /// This constructor is deliberately **crate-private**. It is safe to call,
    /// yet the values it captures are later dereferenced by `unsafe` code in
    /// [`crate::ffi::alloc`], so an *active* hook (both halves present) is only
    /// sound when every clause below holds. Nothing in the type system can
    /// check them, which is exactly why the constructor is not public: the only
    /// caller is `src/ffi/types.rs`, which builds a hook straight out of a
    /// `z_stream` whose validity the FFI entry point has already established
    /// from its own documented `# Safety` contract.
    ///
    /// 1. **Callable pointers.** If `Some`, each of `zalloc`/`zfree` must be a
    ///    live `extern "C"` function with the C `alloc_func`/`free_func`
    ///    signature, callable for as long as any buffer derived from this hook
    ///    is alive.
    /// 2. **zlib `zalloc` semantics.** `zalloc(opaque, items, size)` must
    ///    return either null (out of memory) or a pointer to at least
    ///    `items * size` writable bytes, aligned for any element type the
    ///    engines request (`u8`, `u16`, `u32`), and not aliased by any other
    ///    live reference.
    /// 3. **Matching deallocator.** `zfree` must be the deallocator paired with
    ///    that `zalloc`, and must accept any pointer that `zalloc` returned
    ///    together with the same `opaque`.
    /// 4. **`opaque` lifetime.** `opaque` is forwarded verbatim to both hooks
    ///    and must remain valid for them until every buffer allocated through
    ///    this hook — including buffers produced by a `deflateCopy`-style
    ///    [`Clone`] — has been dropped.
    ///
    /// A hook with `zalloc` or `zfree` set to [`None`] is
    /// [inactive](Self::is_active) and carries no obligation at all: it can
    /// never reach the raw-pointer path. That is the guarantee callers outside
    /// the crate get, since [`AllocHook::none`] is the only constructor they
    /// can reach.
    ///
    /// # What is enforced mechanically
    ///
    /// Every precondition that *can* be checked is checked before the hooks are
    /// used: `crate::ffi::alloc` validates the requested
    /// [`core::alloc::Layout`] (rejecting an overflowing or
    /// `isize::MAX`-exceeding request), rejects a size that is not representable
    /// in the C `uInt` hook ABI, rejects a null return, and rejects a returned
    /// pointer that is not correctly aligned for the element type — releasing it
    /// through `zfree` so nothing leaks and reporting the request as an
    /// allocation failure. A hook that misbehaves in any of those ways therefore
    /// produces `Z_MEM_ERROR`, never undefined behavior. Clauses 1, 3 and 4
    /// above are the residue that cannot be verified mechanically, and they are
    /// precisely the guarantees `zlib.h` already requires of
    /// `alloc_func`/`free_func` (`zlib.h` L85-L86); violating them is a defect in
    /// the caller's allocator, not in this crate.
    #[inline]
    #[must_use]
    pub(crate) const fn new(
        zalloc: Option<ZallocFn>,
        zfree: Option<ZfreeFn>,
        opaque: *mut c_void,
    ) -> Self {
        Self {
            zalloc,
            zfree,
            opaque,
        }
    }

    /// Whether this hook can back a foreign allocation (both halves present).
    #[inline]
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.zalloc.is_some() && self.zfree.is_some()
    }

    /// The caller's allocation hook, if present. Read by the sanctioned
    /// [`crate::ffi::alloc`] bridge to invoke `zalloc`.
    #[inline]
    #[must_use]
    pub(crate) const fn zalloc(&self) -> Option<ZallocFn> {
        self.zalloc
    }

    /// The caller's deallocation hook, if present. Read by the sanctioned
    /// [`crate::ffi::alloc`] bridge to invoke `zfree` on drop.
    #[inline]
    #[must_use]
    pub(crate) const fn zfree(&self) -> Option<ZfreeFn> {
        self.zfree
    }

    /// The caller's private cookie, forwarded verbatim to both hooks.
    #[inline]
    #[must_use]
    pub(crate) const fn opaque(&self) -> *mut c_void {
        self.opaque
    }

    /// Allocates a foreign buffer of `count` elements — each holding `T`'s
    /// [`Default`] value — through this (active) hook, delegating to the
    /// sanctioned [`crate::ffi::alloc`] zone where the raw-pointer `unsafe`, the
    /// layout validation, and the alignment check are confined (AAP §0.6.2).
    ///
    /// Returns [`None`] when the caller's `zalloc` reports out-of-memory, or when
    /// the request cannot be honored soundly (an unrepresentable size, or an
    /// address that does not satisfy `T`'s alignment); there is **no**
    /// global-allocator fallback, so the init paths can surface `Z_MEM_ERROR`
    /// (AAP §0.6.5).
    #[inline]
    pub(crate) fn try_alloc_zeroed<T: Copy + Default + ZeroValid + 'static>(
        &self,
        count: usize,
    ) -> Option<Box<dyn ForeignBuffer<T>>> {
        crate::ffi::alloc::try_alloc_foreign::<T>(*self, count)
    }

    /// Allocates a zero-initialized foreign buffer through this (active) hook,
    /// forwarding the `(items, item_size)` pair to the caller's `zalloc`
    /// verbatim so it sees the same arguments C's `ZALLOC(strm, items, size)`
    /// passes (AAP §0.6.2).
    ///
    /// The materialized element count is `items * item_size / size_of::<T>()`.
    /// Returns [`None`] on caller-`zalloc` out-of-memory, on an unrepresentable
    /// or non-multiple size, or if the returned region is not aligned for `T`;
    /// there is **no** global-allocator fallback (AAP §0.6.5).
    #[inline]
    pub(crate) fn try_alloc_zeroed_items<T: Copy + Default + ZeroValid + 'static>(
        &self,
        items: usize,
        item_size: usize,
    ) -> Option<Box<dyn ForeignBuffer<T>>> {
        crate::ffi::alloc::try_alloc_foreign_items::<T>(*self, items, item_size)
    }
}

/// Allocates a single `T` on the Rust global heap **fallibly**, yielding [`None`]
/// instead of aborting when the heap is exhausted.
///
/// [`Box::new`] aborts the process on allocation failure, whereas zlib reports a
/// failed state allocation as `Z_MEM_ERROR`. The engine-state constructors
/// (`DeflateState::new_in`, `InflateState::try_new_in`) route their single global
/// allocation through here so heap exhaustion becomes a return code rather than
/// an abort (AAP §0.6.5). The raw-pointer work is confined to the sanctioned
/// [`crate::ffi::alloc`] zone, so this module stays free of `unsafe` (AAP §0.6.2).
///
/// `value` is dropped normally if the allocation fails, releasing any working
/// buffers it already owns.
#[inline]
#[must_use]
pub(crate) fn try_box<T>(value: T) -> Option<Box<T>> {
    crate::ffi::alloc::try_box(value)
}

// ---------------------------------------------------------------------------
// ForeignBuffer — the safe interface to a caller-`zalloc`'d region
// ---------------------------------------------------------------------------

/// The safe interface this module uses to access a caller-`zalloc`'d working
/// buffer **without any `unsafe`**.
///
/// The sole implementor is `CForeignBuffer` in the sanctioned
/// `crate::ffi::alloc` zone, which confines the raw-pointer hook invocation,
/// the slice materialization, and the `zfree`-on-drop (migration
/// unsafe-isolation rule / AAP §0.6.2). [`AllocBuffer::Foreign`] holds one as a
/// `Box<dyn ForeignBuffer<T>>` and delegates [`Deref`]/[`DerefMut`] and
/// [`try_clone`](AllocBuffer::try_clone) to these methods, so neither this
/// module nor the compression engines contain any `unsafe` (AAP §0.6.2).
pub trait ForeignBuffer<T: Copy + Default + ZeroValid> {
    /// The buffer contents as a shared slice of `T`.
    fn as_slice(&self) -> &[T];

    /// The buffer contents as a unique (mutable) slice of `T`.
    fn as_mut_slice(&mut self) -> &mut [T];

    /// Deep-clones into a fresh foreign region allocated through the **same**
    /// hook, with the same `(items, item_size)` pair the original was requested
    /// with (matching C `deflateCopy`/`inflateCopy`, which `ZALLOC` their new
    /// buffers), or [`None`] if that allocation reports out-of-memory.
    ///
    /// [`None`] is an **allocation failure**, never a request to substitute
    /// different storage: [`AllocBuffer::try_clone`](AllocBuffer::try_clone)
    /// propagates it verbatim and the copy entry points report `Z_MEM_ERROR`,
    /// exactly as C does. There is deliberately **no** global-allocator fallback:
    /// a caller who installed a bounded allocator must observe the failure rather
    /// than silently receive a copy living in the global heap (AAP §0.6.3,
    /// §0.6.5).
    fn clone_foreign(&self) -> Option<Box<dyn ForeignBuffer<T>>>;
}

// ---------------------------------------------------------------------------
// AllocBuffer — an owned buffer that is either global- or caller-allocated
// ---------------------------------------------------------------------------

/// An owned, zero-initialized buffer of `T` that transparently derefs to a
/// slice, backed **either** by the Rust global allocator (an owned [`Vec`])
/// **or** by a caller-supplied C `zalloc`/`zfree` pair (AAP §0.6.3).
///
/// This is the return type of [`Allocator::allocate_zeroed`]. The compression
/// and decompression engines store their working buffers (window, `pending_buf`,
/// hash tables) as `AllocBuffer`s and use them purely as slices via
/// [`Deref`]/[`DerefMut`]; they never observe which backing store is active and
/// contain no `unsafe`.
///
/// # Backing-store selection
///
/// [`AllocBuffer::try_zeroed`] returns a [`Foreign`](Self::Foreign) region only
/// when the [`AllocHook`] is [active](AllocHook::is_active) and the element count
/// is non-zero; that region is produced by the sanctioned `crate::ffi::alloc`
/// zone. With no hook (or an empty request) it is an [`Owned`](Self::Owned)
/// [`Vec`], byte-for-byte reproducing the historical global-allocator behavior.
///
/// When an active hook's `zalloc` reports out-of-memory (or the request is an
/// unrepresentable size), [`try_zeroed`](Self::try_zeroed) returns [`None`]
/// rather than silently falling back to the global allocator; callers propagate
/// that as `Z_MEM_ERROR` (AAP §0.6.5). A foreign buffer **never** degrades to an owned
/// one: copying is performed by the fallible
/// [`try_clone`](Self::try_clone) — this type deliberately does not implement
/// [`Clone`] — so a caller who installed a bounded arena observes the failure
/// instead of silently receiving a copy in the global heap (AAP §0.6.5).
///
/// # Initialization and `unsafe` isolation
///
/// Both arms hand back `count` elements that hold `T`'s [`Default`] value. The
/// [`Owned`](Self::Owned) arm builds them with `vec![T::default(); count]`; a
/// foreign region is filled inside `crate::ffi::alloc` by *writing valid
/// `T::default()` values*, which for the integer element types the engines
/// request (`u8`, `u16`, `u32`, whose `Default` is `0`) is byte-for-byte the
/// `zmemzero` that C `zcalloc` performs after its `zalloc`. Writing values rather
/// than zeroing bytes is deliberate: the `Copy + Default` bound alone does not
/// make an all-zero bit pattern a *valid* `T`. As an independent, compile-time
/// line of defence the element-type set is additionally sealed by the
/// [`ZeroValid`] marker, so the two sanctioned remedies for that hazard are both
/// in force. That zone also validates the request as a
/// [`core::alloc::Layout`] and verifies the address the hook returns against
/// `T`'s alignment, so a slice over the region is always sound.
/// **This module contains no `unsafe`**: every raw-pointer operation for the
/// [`Foreign`](Self::Foreign) arm is behind the safe [`ForeignBuffer`] interface
/// (AAP §0.6.2).
pub enum AllocBuffer<T: Copy + Default + ZeroValid> {
    /// Global-allocator storage (the default / null-hook path).
    Owned(Vec<T>),
    /// Caller-`zalloc`'d storage, accessed through the safe [`ForeignBuffer`]
    /// interface and released through the caller's `zfree` when the box drops.
    /// The concrete implementor lives in the sanctioned `crate::ffi::alloc`
    /// zone, so this module never touches the raw pointer.
    Foreign(Box<dyn ForeignBuffer<T>>),
}

impl<T: Copy + Default + ZeroValid> AllocBuffer<T> {
    /// Allocates a buffer of `count` elements holding `T`'s [`Default`] value,
    /// routing through the caller's `zalloc` when `hook` is active (see the
    /// type-level [backing-store selection](AllocBuffer#backing-store-selection)).
    ///
    /// Returns [`None`] when an active hook's `zalloc` reports out-of-memory, or
    /// when the request cannot be served through that hook soundly — a size that
    /// is unrepresentable in the C `uInt` hook ABI or as a
    /// [`core::alloc::Layout`], or an address from the hook that does not satisfy
    /// `T`'s alignment — and also when the global-allocator path cannot reserve
    /// `count` elements. Callers propagate every one of those as `Z_MEM_ERROR`
    /// (AAP §0.6.5), exactly as C reports a failed `ZALLOC`. The null-hook,
    /// empty-count, and zero-sized-`T` paths are served from the global allocator,
    /// byte-for-byte reproducing the historical behavior. All raw-pointer work for
    /// the active-hook path is confined to the sanctioned `crate::ffi::alloc` zone
    /// (AAP §0.6.2), so this contains no `unsafe`.
    ///
    /// The hook sees the request as `(count, size_of::<T>())`, which is the pair
    /// C passes for the element-shaped `ZALLOC`s (`prev` and `head`, both
    /// `ZALLOC(strm, n, sizeof(Pos))`). Use
    /// [`try_zeroed_items`](Self::try_zeroed_items) where C uses a different
    /// `(items, size)` split for the same byte count.
    #[must_use]
    pub fn try_zeroed(count: usize, hook: AllocHook) -> Option<Self>
    where
        T: 'static,
    {
        // Fast path / default: no custom allocator, an empty request, or a
        // zero-sized element type. `try_owned` matches C `zcalloc`'s zero fill and
        // is the exact historical (global-allocator) behavior, but reserves
        // fallibly so global-heap exhaustion is reported rather than aborting; for
        // a zero-sized `T` it allocates nothing at all, which is why such a request
        // is served here rather than being pushed through a C hook that has no way
        // to express a zero element size.
        if !hook.is_active() || count == 0 || core::mem::size_of::<T>() == 0 {
            return Self::try_owned(count);
        }
        // Active hook: delegate to the sanctioned `ffi` allocator zone, which
        // validates the layout and the returned address before any slice exists.
        // `None` (OOM / unrepresentable size / unusable address) propagates — no
        // global fallback (AAP §0.6.5).
        hook.try_alloc_zeroed::<T>(count).map(AllocBuffer::Foreign)
    }

    /// Reserves `count` default-initialized elements on the Rust global heap
    /// **fallibly**.
    ///
    /// `vec![T::default(); count]` aborts the process when the heap cannot
    /// satisfy the request, whereas zlib reports a failed working-buffer
    /// allocation as `Z_MEM_ERROR`. Reserving first and filling afterwards turns
    /// an unsatisfiable request (an absurd `count`, or genuine exhaustion) into
    /// [`None`] (AAP §0.6.5). The resulting contents are identical to the historical
    /// `vec![T::default(); count]` spelling.
    fn try_owned(count: usize) -> Option<Self> {
        let mut vec: Vec<T> = Vec::new();
        vec.try_reserve_exact(count).ok()?;
        vec.resize(count, T::default());
        Some(AllocBuffer::Owned(vec))
    }

    /// Allocates a zero-initialized buffer described as `items * item_size`
    /// bytes, forwarding that exact pair to an active `zalloc` so the caller
    /// observes the same arguments C's `ZALLOC(strm, items, size)` passes.
    ///
    /// zlib does not always split a request as `(element_count, element_size)`:
    /// `deflateInit2_` asks for the window as `ZALLOC(strm, s->w_size, 2 *
    /// sizeof(Byte))` (`deflate.c` L458) and the pending buffer as
    /// `ZALLOC(strm, s->lit_bufsize, LIT_BUFS)` (`deflate.c` L505). A bounded
    /// caller allocator can legitimately inspect both arguments, so preserving
    /// the split is part of the has-hook contract (AAP §0.6.3, §0.6.5).
    ///
    /// The materialized element count is `items * item_size / size_of::<T>()`.
    /// Returns [`None`] when the product overflows, when it is not an exact
    /// multiple of `size_of::<T>()`, when an active `zalloc` reports
    /// out-of-memory or an unrepresentable size, or when the global-allocator
    /// path cannot reserve the elements (AAP §0.6.5). There is **no** global fallback
    /// for an active hook.
    #[must_use]
    pub fn try_zeroed_items(items: usize, item_size: usize, hook: AllocHook) -> Option<Self>
    where
        T: 'static,
    {
        let elem = core::mem::size_of::<T>();
        // Zero-sized `T` has no meaningful buffer geometry; the engines only ever
        // request `u8`/`u16`/`u32`, so this is unreachable in practice and is
        // rejected rather than silently mis-sized.
        if elem == 0 {
            return None;
        }
        let total = items.checked_mul(item_size)?;
        if total % elem != 0 {
            return None;
        }
        let count = total / elem;

        // Fast path / default: no custom allocator, or an empty request.
        if !hook.is_active() || count == 0 {
            return Self::try_owned(count);
        }
        // Active hook: delegate to the sanctioned `ffi` allocator zone. `None`
        // (OOM / unrepresentable size) propagates — no global fallback (AAP §0.6.5).
        hook.try_alloc_zeroed_items::<T>(items, item_size)
            .map(AllocBuffer::Foreign)
    }

    /// Fallible, **allocator-preserving** deep copy — the safe analogue of the
    /// `ZALLOC`-then-`zmemcpy` sequence in C `deflateCopy`/`inflateCopy`.
    ///
    /// An [`Owned`](Self::Owned) buffer is copied through a fallible reservation,
    /// so global-heap exhaustion is reported rather than aborting. A
    /// [`Foreign`](Self::Foreign) buffer allocates a fresh region through the
    /// **same** [`AllocHook`] via [`ForeignBuffer::clone_foreign`] — with the same
    /// `(items, item_size)` pair — and copies the contents across, so the copy
    /// lives in the same arena as the original.
    ///
    /// # Why this is fallible instead of [`Clone`]
    ///
    /// AAP §0.6.5 requires that "the clone path must route through the same
    /// `AllocHook` — otherwise a caller that supplied a custom arena would find
    /// the copy living in the global heap". An infallible [`Clone`] cannot honor
    /// that: when the caller's `zalloc` reports out-of-memory mid-copy its only
    /// options are to panic or to silently fall back to the global allocator, and
    /// the fallback turns a caller-visible `Z_MEM_ERROR` into a spurious success —
    /// returning `Z_OK` where C returns `Z_MEM_ERROR` and leaving a
    /// caller-supplied arena silently escaped.
    /// [`AllocBuffer`] therefore deliberately does **not** implement [`Clone`];
    /// this method returns [`None`] on out-of-memory and the C copy entry points
    /// translate that into `Z_MEM_ERROR`. C behaves identically: `deflateCopy`
    /// (`deflate.c` L1317-L1377, L1348-L1350) and `inflateCopy` (`inflate.c`
    /// L1340-L1350) both return `Z_MEM_ERROR` when a `ZALLOC` fails during the
    /// copy, so making the failure visible in the type is what keeps the has-hook
    /// clause (AAP §0.6.3) and allocation-failure parity (AAP §0.6.5) intact.
    #[must_use]
    pub fn try_clone(&self) -> Option<Self> {
        match self {
            AllocBuffer::Owned(v) => {
                let mut fresh: Vec<T> = Vec::new();
                fresh.try_reserve_exact(v.len()).ok()?;
                fresh.extend_from_slice(v);
                Some(AllocBuffer::Owned(fresh))
            }
            // The concrete implementor reproduces the original `(items, size)`
            // pair through the same hook; `None` means the caller's `zalloc`
            // refused. Propagate it verbatim — no global-allocator fallback
            // (AAP §0.6.5).
            AllocBuffer::Foreign(b) => b.clone_foreign().map(AllocBuffer::Foreign),
        }
    }

    /// Wraps an existing [`Vec`] as an [`Owned`](Self::Owned) buffer (used by
    /// tests and by call sites that build a buffer eagerly).
    #[inline]
    #[must_use]
    pub fn from_vec(vec: Vec<T>) -> Self {
        AllocBuffer::Owned(vec)
    }

    /// Number of elements (also reachable as `self.len()` through [`Deref`];
    /// provided inherently so call sites holding the buffer by value are clear).
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            AllocBuffer::Owned(v) => v.len(),
            AllocBuffer::Foreign(b) => b.as_slice().len(),
        }
    }

    /// Whether the buffer has no elements.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the backing store is a caller-`zalloc`'d
    /// [`Foreign`](Self::Foreign) region rather than an
    /// [`Owned`](Self::Owned) [`Vec`].
    ///
    /// Test-only: the engines deliberately cannot observe which backing store is
    /// active, but the allocator-contract tests must assert that the has-hook
    /// clause selected the arm they expect and that a copy stayed in the caller's
    /// arena instead of silently relocating to the global heap.
    #[cfg(test)]
    #[inline]
    #[must_use]
    pub(crate) const fn is_foreign(&self) -> bool {
        matches!(self, AllocBuffer::Foreign(_))
    }
}

impl<T: Copy + Default + ZeroValid> Deref for AllocBuffer<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        match self {
            AllocBuffer::Owned(v) => v.as_slice(),
            // Delegates to the safe `ForeignBuffer` interface; the raw-pointer
            // slice materialization is confined to `crate::ffi::alloc`.
            AllocBuffer::Foreign(b) => b.as_slice(),
        }
    }
}

impl<T: Copy + Default + ZeroValid> DerefMut for AllocBuffer<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            AllocBuffer::Owned(v) => v.as_mut_slice(),
            // As in `deref`, delegating to the safe `ForeignBuffer` interface.
            AllocBuffer::Foreign(b) => b.as_mut_slice(),
        }
    }
}

impl<T: Copy + Default + ZeroValid> Default for AllocBuffer<T> {
    /// An empty [`Owned`](Self::Owned) buffer (no allocation) — the analogue of
    /// C's `state->window = Z_NULL` "not yet allocated" sentinel.
    #[inline]
    fn default() -> Self {
        AllocBuffer::Owned(Vec::new())
    }
}

// NOTE: `AllocBuffer` needs no manual `Drop`. The `Owned` arm's `Vec` frees
// itself through the global allocator, and the `Foreign` arm's
// `Box<dyn ForeignBuffer<T>>` runs its implementor's `Drop` (in the sanctioned
// `crate::ffi::alloc` zone), which calls the caller's `zfree`. Both release paths
// are automatic, so no `unsafe` deallocation lives in this module.

/// Abstraction over the source of the library's heap buffers, replacing the C
/// `alloc_func`/`free_func` pointers (`zlib.h` L85-L86).
///
/// zlib lets callers override allocation via
/// `voidpf (*zalloc)(voidpf opaque, uInt items, uInt size)` and
/// `void (*zfree)(voidpf opaque, voidpf address)`. This trait is the safe Rust
/// equivalent of that customization point: the compression and decompression
/// engines allocate their working buffers (the sliding window, `pending_buf`,
/// and the `head`/`prev` hash tables) through an `Allocator`, and free them
/// through the same value (or, for the default, simply by dropping the owned
/// [`Vec`]).
///
/// The default implementation, [`DefaultAllocator`], uses the Rust global
/// allocator and is what Rust-native callers get. The C drop-in path installs a
/// different implementation in [`crate::ffi`] that forwards to the caller's
/// `zalloc`/`zfree`; that implementation is where the `unsafe` raw-pointer hook
/// invocation is confined. **This module intentionally defines only the trait
/// and the safe global default** — no `unsafe` appears here.
///
/// # Contract
///
/// Implementations must return a buffer of exactly `count` elements from
/// [`allocate_zeroed`](Allocator::allocate_zeroed), every element initialised to
/// its [`Default`] (numerically zero) value — mirroring the zero-fill that C
/// `zcalloc` performs. [`deallocate`](Allocator::deallocate) consumes a buffer
/// previously produced by the same allocator.
pub trait Allocator {
    /// Allocates a contiguous, zero-initialised buffer of `count` elements of
    /// type `T`, returned as an [`AllocBuffer<T>`].
    ///
    /// This is the safe analogue of the C `ZALLOC` macro (which calls the
    /// caller's `zalloc`, then zeroes the region). The returned [`AllocBuffer`]
    /// derefs to a `[T]` slice and, when this allocator carries an active
    /// [`hook`](Allocator::hook), is backed by the caller's `zalloc`/`zfree`;
    /// otherwise it is a global-allocator [`Vec`]. `T` is constrained to
    /// [`Copy`] + [`Default`] so the buffer can be filled with the type's default
    /// value without running arbitrary drop or clone logic, and additionally to
    /// the sealed [`ZeroValid`] marker so the foreign path's element-type set is
    /// restricted at compile time; the buffer element types the engines request
    /// are the plain integer types `u8`, `u16`, and `u32`, all of whose
    /// [`Default`] is `0`. The bound is intentionally *not* relied on as a promise
    /// that an all-zero bit pattern is a valid `T`: the foreign path initializes
    /// by writing `T::default()` values.
    ///
    /// Returns [`None`] when an active hook's `zalloc` reports out-of-memory, or
    /// when the request cannot be served through that hook soundly (an
    /// unrepresentable size, or an address that does not satisfy `T`'s
    /// alignment); callers translate every one of those into `Z_MEM_ERROR`
    /// (AAP §0.6.5). The
    /// convention) and so always returns [`Some`].
    fn allocate_zeroed<T>(&self, count: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static;

    /// Returns the caller-allocator [`AllocHook`] this allocator forwards to,
    /// or [`AllocHook::none`] (the default) when it uses the global allocator.
    ///
    /// The deflate/inflate init paths read this so a state's lazily- or
    /// eagerly-allocated working buffers can be routed through the caller's
    /// `zalloc`/`zfree` (AAP §0.6.3).
    ///
    /// An implementation outside this crate can only return
    /// [`AllocHook::none`], because building an *active* hook needs the
    /// crate-private `AllocHook::new` and its raw-pointer obligations. That is
    /// deliberate: routing buffers through foreign C function pointers is the
    /// FFI boundary's job, and `crate::ffi`'s own allocator is the only
    /// implementation that does it. Overriding this method is therefore
    /// unnecessary for a Rust-native allocator, and the default is correct.
    #[inline]
    fn hook(&self) -> AllocHook {
        AllocHook::none()
    }

    /// Releases a buffer previously produced by
    /// [`allocate_zeroed`](Allocator::allocate_zeroed).
    ///
    /// This is the safe analogue of the C `ZFREE` macro. Because an
    /// [`AllocBuffer`] releases its own storage on [`Drop`] — a global [`Vec`]
    /// through the global allocator, or a foreign region through the caller's
    /// `zfree` — the default implementation simply drops it. The method is
    /// retained for call-site clarity and API symmetry with the C `ZFREE`.
    #[inline]
    fn deallocate<T>(&self, buffer: AllocBuffer<T>)
    where
        T: Copy + Default + ZeroValid,
    {
        // Dropping the `AllocBuffer` runs its `Drop`, which routes to the
        // correct deallocator (global for `Owned`, the caller's `zfree` for
        // `Foreign`). Made explicit for clarity.
        drop(buffer);
    }
}

/// The default [`Allocator`], backed by the Rust global allocator.
///
/// This zero-sized type is installed on every [`ZStream`] created without an
/// explicit allocator (see [`ZStream::new`]). It corresponds to a C caller that
/// left `z_stream.zalloc`/`zfree` as `Z_NULL`, in which case zlib falls back to
/// its built-in `zcalloc`/`zcfree` (ultimately `malloc`/`free`). Here the
/// "built-in" allocator is Rust's global allocator, reached through the standard
/// [`Vec`] APIs.
#[derive(Copy, Clone, Debug, Default)]
pub struct DefaultAllocator;

impl Allocator for DefaultAllocator {
    #[inline]
    fn allocate_zeroed<T>(&self, count: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        // The default allocator has no hook, so this is always an owned,
        // global-allocator `Vec` (byte-for-byte the historical behavior) and
        // therefore always `Some`. Routing through `try_zeroed` with the "none"
        // hook keeps the single source of truth for buffer construction.
        AllocBuffer::try_zeroed(count, AllocHook::none())
    }

    // `hook` uses the trait default (`AllocHook::none`): the global allocator.
}

// ===========================================================================
// StreamState — the owned engine state (replaces the C `internal_state *`)
// ===========================================================================

/// The engine state owned by a [`ZStream`], replacing the C
/// `internal_state *state` pointer (`zlib.h` L99).
///
/// A stream is in exactly one of three states, and the enum makes that a
/// type-level invariant:
///
/// * [`StreamState::None`] — not yet initialised for either direction
///   (equivalent to a C `state == Z_NULL`).
/// * [`StreamState::Deflate`] — owns a [`DeflateState`] for compression.
/// * [`StreamState::Inflate`] — owns an [`InflateState`] for decompression.
///
/// Modelling the two engines as one enum (rather than a pair of
/// `Option<Box<…>>` fields, the shape sketched in AAP §0.6.3) is a deliberate
/// tightening: it is *impossible* to simultaneously hold a deflate and an
/// inflate state, so the C hazard of dispatching an inflate routine on a deflate
/// state — or vice-versa — cannot be expressed. The [`Option`]-returning
/// accessors on [`ZStream`] ([`deflate_state`](ZStream::deflate_state),
/// [`inflate_state`](ZStream::inflate_state), …) recover the ergonomic
/// `Option<&…>` view the AAP describes.
///
/// The variant payloads are [`Box`]ed because the engine states are large: at
/// the defaults a [`DeflateState`] owns roughly 304 KiB of working buffers (see
/// its "Memory footprint" section — this port keeps `sym_buf` separate rather
/// than overlaying it on `pending_buf` as C does, which is what puts the figure
/// above reference zlib's ~256 KiB), and an [`InflateState`] is about 7 KiB plus
/// its on-demand window. Boxing keeps `ZStream` itself small, and it makes this
/// enum the crate's single owner of an engine: `StreamState::Deflate(Box<…>)` /
/// `StreamState::Inflate(Box<…>)` is where the C `internal_state *` pointer
/// went. The engines' own buffers are [`AllocBuffer`]s, so an engine may be
/// backed by the caller's `zalloc`/`zfree` while the `Box` around it is always a
/// global-allocator allocation.
#[derive(Default)]
pub(crate) enum StreamState {
    /// No engine has been initialised (C `state == Z_NULL`).
    #[default]
    None,
    /// A compression engine is installed.
    Deflate(Box<DeflateState>),
    /// A decompression engine is installed.
    Inflate(Box<InflateState>),
}

impl StreamState {
    /// Returns `true` if no engine is installed ([`StreamState::None`]).
    #[inline]
    #[must_use]
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, StreamState::None)
    }

    /// Returns `true` if a compression engine is installed.
    #[inline]
    #[must_use]
    pub(crate) fn is_deflate(&self) -> bool {
        matches!(self, StreamState::Deflate(_))
    }

    /// Returns `true` if a decompression engine is installed.
    #[inline]
    #[must_use]
    pub(crate) fn is_inflate(&self) -> bool {
        matches!(self, StreamState::Inflate(_))
    }

    /// Returns a short, allocation-free label for the active variant, used by
    /// the [`fmt::Debug`] implementation (which must not print the — potentially
    /// non-`Debug` — engine state itself).
    #[inline]
    #[must_use]
    fn kind_str(&self) -> &'static str {
        match self {
            StreamState::None => "None",
            StreamState::Deflate(_) => "Deflate",
            StreamState::Inflate(_) => "Inflate",
        }
    }
}

impl fmt::Debug for StreamState {
    /// Prints only the variant name. Neither [`DeflateState`] nor
    /// [`InflateState`] implements [`Debug`] (they hold large working buffers
    /// whose contents are not useful to dump), so the payload is deliberately
    /// elided.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind_str())
    }
}

// ===========================================================================
// ZStream — the idiomatic port of the C `z_stream`
// ===========================================================================

/// The idiomatic streaming state, mirroring the C `z_stream`
/// (`zlib.h` L88-L110).
///
/// `ZStream` owns its engine state (`StreamState`) and its allocator (`A`),
/// so dropping a `ZStream` releases everything: the boxed engine and all of the
/// working buffers it owns. This is the direct replacement for C's
/// `internal_state *` plus the `zcalloc`/`zcfree`/`deflateEnd`/`inflateEnd`
/// bookkeeping (AAP §0.3.2 / §0.6.3).
///
/// The type parameter `A` selects the allocator; it defaults to
/// [`DefaultAllocator`] (the Rust global allocator) so Rust callers can simply
/// write `ZStream`. The C drop-in path in [`crate::ffi`] instantiates
/// `ZStream<CAllocator>` with an allocator that forwards to the caller's
/// `zalloc`/`zfree`.
///
/// # Cursor fields are intentionally absent
///
/// The C `next_in`/`avail_in`/`next_out`/`avail_out` cursor quadruple is *not*
/// stored. Input and output are passed as slices to the deflate/inflate methods,
/// and only the cumulative [`total_in`](Self::total_in) /
/// [`total_out`](Self::total_out) counters persist between calls. See the
/// [module documentation](crate::stream) for how the FFI layer bridges those
/// cursors.
///
/// # Examples
///
/// ```
/// use zlib_rs::stream::ZStream;
///
/// // A freshly created stream owns no engine yet.
/// let strm = ZStream::new();
/// assert_eq!(strm.total_in, 0);
/// assert!(!strm.has_state());
/// // Dropping `strm` here needs no explicit `deflateEnd`/`inflateEnd`.
/// ```
pub struct ZStream<A: Allocator = DefaultAllocator> {
    /// Total number of input bytes consumed so far (C `uLong total_in`).
    ///
    /// Widened to [`u64`] so the counter is lossless on both 32- and 64-bit
    /// platforms, where C `uLong` is 32 or 64 bits respectively.
    pub total_in: u64,

    /// Total number of output bytes produced so far (C `uLong total_out`).
    ///
    /// Widened to [`u64`] for the same reason as [`total_in`](Self::total_in).
    pub total_out: u64,

    /// Best guess about the data type — binary or text for deflate, or the
    /// decoding state for inflate (C `int data_type`).
    ///
    /// The raw [`i32`] preserves the exact `Z_BINARY` / `Z_TEXT` / `Z_UNKNOWN`
    /// contract at the boundary; [`data_type_enum`](Self::data_type_enum)
    /// offers a typed [`DataType`] view when the value is one of those three.
    pub data_type: i32,

    /// Adler-32 or CRC-32 checksum of the uncompressed data (C `uLong adler`).
    ///
    /// Kept as [`u32`] because both checksums are 32-bit; the C field is
    /// `uLong` only because zlib predates fixed-width integer types.
    pub adler: u32,

    /// Last error message, or [`None`] when there is no error (C
    /// `z_const char *msg`, which is `NULL` when clear).
    ///
    /// Every message zlib can report is a compile-time-constant string (the C
    /// `z_errmsg` table reached through `ERR_MSG`), so a `&'static str` captures
    /// them without allocation and [`None`] models the C `NULL`. Populated via
    /// [`set_msg`](Self::set_msg); see [`ReturnCode::message`].
    pub msg: Option<&'static str>,

    /// Reserved for future use (C `uLong reserved`).
    ///
    /// Retained purely for conceptual parity with `z_stream`; the exact ABI
    /// width is reproduced only by the `#[repr(C)]` mirror in
    /// [`crate::ffi`]. Always `0` here.
    pub(crate) reserved: u32,

    /// The owned engine state (C `internal_state *state`).
    ///
    /// See [`StreamState`] for why this is an enum rather than a raw pointer.
    pub(crate) state: StreamState,

    /// The allocator backing this stream's buffers (C `zalloc`/`zfree`/`opaque`).
    ///
    /// See [`Allocator`] and [`DefaultAllocator`].
    pub(crate) alloc: A,
}

/// The Adler-32 checksum of the empty input (`adler32(0, Z_NULL, 0) == 1`).
///
/// This is the value reference zlib seeds `z_stream.adler` with for the default
/// (zlib-wrapper) framing. A freshly constructed [`ZStream`] adopts this seed;
/// installing or resetting an engine then overwrites [`adler`](ZStream::adler)
/// with the wrapper-appropriate value — this same `1` for Adler-32 / zlib, or
/// `0` for CRC-32 / gzip.
const ADLER32_INIT: u32 = 1;

impl ZStream<DefaultAllocator> {
    /// Creates a new stream backed by the [`DefaultAllocator`] (the Rust global
    /// allocator), with no engine installed.
    ///
    /// All counters start at `0`, [`data_type`](ZStream::data_type) at
    /// `Z_BINARY`, [`adler`](ZStream::adler) at the Adler-32 seed
    /// (`ADLER32_INIT`), and [`msg`](ZStream::msg) at [`None`]. This is the
    /// idiomatic counterpart to a zeroed C `z_stream` prior to
    /// `deflateInit`/`inflateInit`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_allocator(DefaultAllocator)
    }
}

impl Default for ZStream<DefaultAllocator> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Allocator> ZStream<A> {
    /// Creates a new stream backed by the supplied allocator, with no engine
    /// installed.
    ///
    /// This is the general constructor used by [`crate::ffi`] to build a
    /// `ZStream<CAllocator>` whose allocator forwards to the caller's
    /// `zalloc`/`zfree`. Field initialisation matches [`ZStream::new`].
    #[must_use]
    pub fn with_allocator(alloc: A) -> Self {
        ZStream {
            total_in: 0,
            total_out: 0,
            // `Z_BINARY` (0): the zeroed default a C caller would `memset` into
            // `z_stream`; the engine refines it to `Z_BINARY`/`Z_TEXT`
            // (deflate) or the decode state (inflate) as it runs.
            data_type: DataType::Binary.as_c_int(),
            adler: ADLER32_INIT,
            msg: None,
            reserved: 0,
            state: StreamState::None,
            alloc,
        }
    }

    /// Returns a shared reference to this stream's [`Allocator`].
    #[inline]
    #[must_use]
    pub fn allocator(&self) -> &A {
        &self.alloc
    }

    // -- engine-state management (used by the deflate/inflate init routines) --

    /// Installs a compression engine, replacing any previously installed state.
    ///
    /// Called by the deflate initialisation path once it has built the
    /// [`DeflateState`]. Any engine previously held is dropped (its buffers
    /// freed) as part of the assignment — the RAII replacement for the C
    /// `deflateEnd`/`inflateEnd` that would otherwise be required first.
    #[inline]
    pub(crate) fn set_deflate_state(&mut self, state: Box<DeflateState>) {
        self.state = StreamState::Deflate(state);
    }

    /// Installs a decompression engine, replacing any previously installed
    /// state. The dual of [`set_deflate_state`](Self::set_deflate_state).
    #[inline]
    pub(crate) fn set_inflate_state(&mut self, state: Box<InflateState>) {
        self.state = StreamState::Inflate(state);
    }

    /// Removes and returns the current engine state, leaving the stream in the
    /// [`StreamState::None`] state.
    ///
    /// Used by the `deflateEnd`/`inflateEnd` shims: taking the state and letting
    /// the returned value drop performs the teardown. (Simply calling
    /// [`clear_state`](Self::clear_state), or dropping the whole `ZStream`,
    /// achieves the same release.)
    #[inline]
    pub(crate) fn take_state(&mut self) -> StreamState {
        core::mem::take(&mut self.state)
    }

    /// Drops the current engine state, returning the stream to
    /// [`StreamState::None`].
    #[inline]
    pub(crate) fn clear_state(&mut self) {
        self.state = StreamState::None;
    }

    /// Borrows the installed compression engine, or [`None`] if the stream holds
    /// no state or an inflate state.
    #[inline]
    #[must_use]
    pub(crate) fn deflate_state(&self) -> Option<&DeflateState> {
        match &self.state {
            StreamState::Deflate(state) => Some(state),
            _ => None,
        }
    }

    /// Mutably borrows the installed compression engine, or [`None`]. The hot
    /// path for `deflate()`, which repeatedly advances the engine.
    #[inline]
    #[must_use]
    pub(crate) fn deflate_state_mut(&mut self) -> Option<&mut DeflateState> {
        match &mut self.state {
            StreamState::Deflate(state) => Some(state),
            _ => None,
        }
    }

    /// Borrows the installed decompression engine, or [`None`] if the stream
    /// holds no state or a deflate state.
    #[inline]
    #[must_use]
    pub(crate) fn inflate_state(&self) -> Option<&InflateState> {
        match &self.state {
            StreamState::Inflate(state) => Some(state),
            _ => None,
        }
    }

    /// Mutably borrows the installed decompression engine, or [`None`]. The hot
    /// path for `inflate()`.
    #[inline]
    #[must_use]
    pub(crate) fn inflate_state_mut(&mut self) -> Option<&mut InflateState> {
        match &mut self.state {
            StreamState::Inflate(state) => Some(state),
            _ => None,
        }
    }

    // -- public introspection -------------------------------------------------

    /// Returns `true` if an engine (deflate or inflate) is installed.
    #[inline]
    #[must_use]
    pub fn has_state(&self) -> bool {
        !self.state.is_none()
    }

    /// Returns `true` if this stream is initialised for compression.
    #[inline]
    #[must_use]
    pub fn is_deflate(&self) -> bool {
        self.state.is_deflate()
    }

    /// Returns `true` if this stream is initialised for decompression.
    #[inline]
    #[must_use]
    pub fn is_inflate(&self) -> bool {
        self.state.is_inflate()
    }

    /// Returns the typed [`DataType`] view of [`data_type`](Self::data_type),
    /// or [`None`] if the raw value is not one of `Z_BINARY` / `Z_TEXT` /
    /// `Z_UNKNOWN` (for example an inflate decode-state code).
    #[inline]
    #[must_use]
    pub fn data_type_enum(&self) -> Option<DataType> {
        DataType::from_c_int(self.data_type)
    }

    // -- error message bridging (mirrors the C `ERR_MSG` / `strm->msg`) --------

    /// Sets [`msg`](Self::msg) from a [`ReturnCode`], mirroring the C `ERR_MSG`
    /// macro that stores the `z_errmsg` string into `strm->msg`.
    ///
    /// A code whose message is empty (only [`ReturnCode::Ok`]) clears the field
    /// to [`None`], matching the C convention that `msg` is `NULL` when there is
    /// no error.
    pub fn set_msg(&mut self, code: ReturnCode) {
        let message = code.message();
        self.msg = if message.is_empty() {
            None
        } else {
            Some(message)
        };
    }

    /// Sets [`msg`](Self::msg) to an explicit static string.
    ///
    /// Used internally where a specific message is required that is not the
    /// canonical [`ReturnCode`] text (for example the deflate/inflate
    /// parameter-validation diagnostics).
    // Reserved internal API: consumed by this module's unit tests and intended
    // for the deflate/inflate parameter-validation diagnostics. Retained even in
    // build configurations that wire up no production caller, so the dead-code
    // lint is allowed narrowly here rather than dropping tested functionality.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn set_msg_str(&mut self, message: &'static str) {
        self.msg = Some(message);
    }

    /// Clears any error message, restoring [`msg`](Self::msg) to [`None`].
    #[inline]
    pub fn clear_msg(&mut self) {
        self.msg = None;
    }

    /// Resets the stream-level bookkeeping — the progress counters and the error
    /// message — without touching the engine state, allocator,
    /// [`data_type`](Self::data_type), or [`adler`](Self::adler).
    ///
    /// This is the shared portion of `deflateReset`/`inflateReset`: the engine
    /// resets its own internal fields (and sets `adler`/`data_type` to their
    /// wrapper-appropriate values), while these stream-level fields are cleared
    /// here.
    // Reserved internal API: consumed by this module's unit tests and intended
    // as the shared stream-level portion of deflateReset/inflateReset. Retained
    // even in build configurations that wire up no production caller, so the
    // dead-code lint is allowed narrowly here rather than dropping tested code.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn reset_bookkeeping(&mut self) {
        self.total_in = 0;
        self.total_out = 0;
        self.msg = None;
    }
}

impl<A: Allocator> fmt::Debug for ZStream<A> {
    /// Hand-written because neither the engine state nor an arbitrary allocator
    /// `A` is required to implement [`Debug`]: the state is rendered as its
    /// variant name (see `StreamState`'s `Debug`) and the allocator as its
    /// type name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZStream")
            .field("total_in", &self.total_in)
            .field("total_out", &self.total_out)
            .field("data_type", &self.data_type)
            .field("adler", &self.adler)
            .field("msg", &self.msg)
            .field("reserved", &self.reserved)
            .field("state", &self.state)
            .field("allocator", &core::any::type_name::<A>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn new_has_expected_defaults() {
        let strm = ZStream::new();
        assert_eq!(strm.total_in, 0);
        assert_eq!(strm.total_out, 0);
        // `Z_BINARY` == 0, the zeroed default.
        assert_eq!(strm.data_type, 0);
        // Adler-32 of the empty input.
        assert_eq!(strm.adler, 1);
        assert_eq!(strm.msg, None);
        assert_eq!(strm.reserved, 0);
        // No engine yet, and neither direction reports a live state.
        assert!(!strm.has_state());
        assert!(!strm.is_deflate());
        assert!(!strm.is_inflate());
        assert!(strm.deflate_state().is_none());
        assert!(strm.inflate_state().is_none());
    }

    #[test]
    fn default_matches_new() {
        let strm = ZStream::default();
        assert_eq!(strm.total_in, 0);
        assert_eq!(strm.total_out, 0);
        assert_eq!(strm.adler, 1);
        assert!(!strm.has_state());
    }

    #[test]
    fn default_allocator_is_used_by_default() {
        // The `ZStream::new()` path must select `DefaultAllocator`.
        let strm: ZStream = ZStream::new();
        let _alloc: &DefaultAllocator = strm.allocator();
    }

    #[test]
    fn default_allocator_allocates_zeroed_buffers() {
        let alloc = DefaultAllocator;

        // The global-allocator default is infallible, so `allocate_zeroed`
        // always returns `Some`.
        let bytes: AllocBuffer<u8> = alloc.allocate_zeroed(4).expect("infallible");
        assert_eq!(&bytes[..], &[0u8; 4][..]);
        assert_eq!(bytes.len(), 4);

        let words: AllocBuffer<u16> = alloc.allocate_zeroed(3).expect("infallible");
        assert_eq!(&words[..], &[0u16; 3][..]);

        // A zero-length request yields an empty buffer.
        let empty: AllocBuffer<u32> = alloc.allocate_zeroed(0).expect("infallible");
        assert!(empty.is_empty());

        // `deallocate` consumes the buffer (default path just drops it).
        alloc.deallocate(bytes);
        alloc.deallocate(words);
        alloc.deallocate(empty);
    }

    #[test]
    fn install_and_take_inflate_state() {
        let mut strm = ZStream::new();

        // A raw-deflate (wrap = 0) inflate state with a 32 KiB window.
        strm.set_inflate_state(InflateState::new(0, 15));

        assert!(strm.has_state());
        assert!(strm.is_inflate());
        assert!(!strm.is_deflate());
        assert!(strm.inflate_state().is_some());
        // The deflate accessor must not see the inflate state.
        assert!(strm.deflate_state().is_none());

        // Mutable access resolves to the same live state.
        assert!(strm.inflate_state_mut().is_some());

        // Taking the state hands back the `Inflate` variant and empties the
        // stream; the returned box drops here, freeing the state (RAII stands in
        // for `inflateEnd`).
        let taken = strm.take_state();
        assert!(taken.is_inflate());
        assert!(!strm.has_state());
        assert!(strm.inflate_state().is_none());
    }

    #[test]
    fn clear_state_returns_to_none() {
        let mut strm = ZStream::new();
        strm.set_inflate_state(InflateState::new(1, 15));
        assert!(strm.has_state());

        strm.clear_state();
        assert!(!strm.has_state());
        assert!(strm.inflate_state().is_none());
    }

    #[test]
    fn set_inflate_state_replaces_previous() {
        // Installing a second engine drops the first without any explicit
        // teardown — the RAII replacement for a forgotten `inflateEnd`.
        let mut strm = ZStream::new();
        strm.set_inflate_state(InflateState::new(0, 15));
        strm.set_inflate_state(InflateState::new(2, 15));
        assert!(strm.is_inflate());
    }

    #[test]
    fn set_msg_maps_return_codes() {
        let mut strm = ZStream::new();

        strm.set_msg(ReturnCode::StreamError);
        assert_eq!(strm.msg, Some("stream error"));

        strm.set_msg(ReturnCode::DataError);
        assert_eq!(strm.msg, Some("data error"));

        // `Z_OK` has an empty message, which clears the field (C `NULL`).
        strm.set_msg(ReturnCode::Ok);
        assert_eq!(strm.msg, None);
    }

    #[test]
    fn set_msg_str_and_clear() {
        let mut strm = ZStream::new();
        strm.set_msg_str("custom diagnostic");
        assert_eq!(strm.msg, Some("custom diagnostic"));

        strm.clear_msg();
        assert_eq!(strm.msg, None);
    }

    #[test]
    fn data_type_enum_maps_known_and_unknown() {
        let mut strm = ZStream::new();
        // Default `data_type` is `Z_BINARY`.
        assert_eq!(strm.data_type_enum(), Some(DataType::Binary));

        strm.data_type = 1;
        assert_eq!(strm.data_type_enum(), Some(DataType::Text));

        // An inflate decode-state code is not a `DataType`.
        strm.data_type = 99;
        assert_eq!(strm.data_type_enum(), None);
    }

    #[test]
    fn reset_bookkeeping_clears_counters_and_msg_only() {
        let mut strm = ZStream::new();
        strm.total_in = 123;
        strm.total_out = 456;
        strm.data_type = 1;
        strm.adler = 0xDEAD_BEEF;
        strm.set_msg(ReturnCode::BufError);

        strm.reset_bookkeeping();

        assert_eq!(strm.total_in, 0);
        assert_eq!(strm.total_out, 0);
        assert_eq!(strm.msg, None);
        // `data_type` and `adler` are engine-owned and left untouched.
        assert_eq!(strm.data_type, 1);
        assert_eq!(strm.adler, 0xDEAD_BEEF);
    }

    /// An allocator that records, via a borrowed counter, that it was dropped.
    /// Used to prove the `ZStream` drop glue tears down the owned allocator
    /// (and, by the same mechanism, the boxed engine state).
    struct CountingAllocator<'a> {
        drops: &'a AtomicUsize,
    }

    impl Allocator for CountingAllocator<'_> {
        fn allocate_zeroed<T>(&self, count: usize) -> Option<AllocBuffer<T>>
        where
            T: Copy + Default + ZeroValid + 'static,
        {
            Some(AllocBuffer::Owned(alloc::vec![T::default(); count]))
        }
    }

    impl Drop for CountingAllocator<'_> {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn drop_runs_and_releases_owned_allocator() {
        let drops = AtomicUsize::new(0);
        {
            let strm = ZStream::with_allocator(CountingAllocator { drops: &drops });
            assert!(!strm.has_state());
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        // Leaving the scope drops the `ZStream`, which drops its `alloc` field.
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn custom_allocator_allocates_zeroed() {
        let drops = AtomicUsize::new(0);
        let alloc = CountingAllocator { drops: &drops };
        let buf: AllocBuffer<u8> = alloc.allocate_zeroed(8).expect("infallible");
        assert_eq!(&buf[..], &[0u8; 8][..]);
    }

    #[test]
    fn debug_impl_is_stable_without_debug_bounds() {
        // Exercises the hand-written `Debug` (state has no `Debug`, `A` is not
        // bound to `Debug`).
        let strm = ZStream::new();
        let rendered = alloc::format!("{strm:?}");
        assert!(rendered.contains("ZStream"));
        assert!(rendered.contains("total_in"));
        // The `None` engine variant is printed by name.
        assert!(rendered.contains("None"));
    }

    #[test]
    fn stream_state_debug_prints_variant_name() {
        assert_eq!(alloc::format!("{:?}", StreamState::None), "None");
        let inflate = StreamState::Inflate(InflateState::new(0, 15));
        assert_eq!(alloc::format!("{inflate:?}"), "Inflate");
    }

    // -----------------------------------------------------------------------
    // Caller-allocator hook contract (F5 / F1 / F6 remediation)
    //
    // The C `alloc_func`/`free_func` pair is raw-pointer machinery, so the
    // counting backing store lives in the crate's designated unsafe boundary
    // (`crate::ffi::alloc::test_hook`) and is merely *driven* from here. That
    // keeps this file's `#![deny(unsafe_code)]` intact while still asserting
    // exact hook invocation counts and symmetric cleanup.
    // -----------------------------------------------------------------------

    /// A hook carrying only `zalloc` is **inactive** per the has-hook clause:
    /// [`AllocBuffer::try_zeroed`] must use owned (global-allocator) storage and
    /// must never consult the caller's `zalloc`. This mirrors C, which uses its
    /// built-in allocator unless both halves are set (`deflate.c` L401-L414).
    #[test]
    fn zalloc_only_is_inactive_and_uses_owned() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let hook = stats.zalloc_only_hook();
        assert!(
            !hook.is_active(),
            "one half of a hook is not a usable allocator"
        );

        let buf: AllocBuffer<u16> =
            AllocBuffer::try_zeroed(12, hook).expect("the owned path is infallible");

        assert!(
            !buf.is_foreign(),
            "an inactive hook must yield owned storage"
        );
        assert_eq!(buf.len(), 12);
        assert_eq!(&buf[..], &[0u16; 12][..]);
        assert_eq!(stats.allocs(), 0, "zalloc must not be consulted");
        assert_eq!(stats.ooms(), 0);

        drop(buf);
        assert_eq!(stats.frees(), 0, "zfree must not be consulted either");
        assert_eq!(stats.live_bytes(), 0);
    }

    /// A hook carrying only `zfree` is likewise inactive, so nothing is routed
    /// through the caller and the buffer is released by ordinary drop glue.
    #[test]
    fn zfree_only_is_inactive_and_uses_owned() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let hook = stats.zfree_only_hook();
        assert!(
            !hook.is_active(),
            "one half of a hook is not a usable allocator"
        );

        let buf: AllocBuffer<u8> =
            AllocBuffer::try_zeroed(20, hook).expect("the owned path is infallible");

        assert!(
            !buf.is_foreign(),
            "an inactive hook must yield owned storage"
        );
        assert_eq!(&buf[..], &[0u8; 20][..]);
        assert_eq!(stats.allocs(), 0);

        drop(buf);
        assert_eq!(
            stats.frees(),
            0,
            "an owned buffer must not be handed to the caller's zfree"
        );
        assert_eq!(stats.live_bytes(), 0);
    }

    /// [`AllocBuffer::try_clone`] on a foreign buffer allocates through the
    /// **same** hook (two `zalloc`s, two `zfree`s) and yields a fully independent
    /// region, satisfying AAP §0.6.5's same-arena requirement for
    /// `deflateCopy`/`inflateCopy`.
    #[test]
    fn foreign_clone_uses_hook_and_is_independent() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let hook = stats.hook();
        assert!(hook.is_active());

        {
            let mut original: AllocBuffer<u32> =
                AllocBuffer::try_zeroed(6, hook).expect("counting hook succeeds");
            assert!(original.is_foreign());
            original[0] = 0x0101_0101;
            original[5] = 0x0202_0202;
            assert_eq!(stats.allocs(), 1);

            let mut copy = original
                .try_clone()
                .expect("the counting hook has unlimited budget");
            assert!(
                copy.is_foreign(),
                "the copy must stay in the caller's arena, not the global heap"
            );
            assert_eq!(stats.allocs(), 2, "the copy must come from the same zalloc");
            assert_eq!(&copy[..], &original[..], "the copy starts equal");

            // Independence in both directions.
            copy[0] = 0xFFFF_FFFF;
            original[5] = 0x3333_3333;
            assert_eq!(original[0], 0x0101_0101);
            assert_eq!(copy[5], 0x0202_0202);
            assert_eq!(copy[0], 0xFFFF_FFFF);
            assert_eq!(original[5], 0x3333_3333);
        } // <- both regions drop here

        assert_eq!(stats.frees(), 2, "every foreign region must reach zfree");
        assert_eq!(stats.live_bytes(), 0, "the caller's arena must be balanced");
    }

    /// An element count that cannot be represented in the C `uInt` hook ABI is
    /// rejected **before** the hook is invoked, so a truncated `items`/`size`
    /// pair can never reach a caller's allocator.
    #[test]
    fn foreign_allocation_rejects_count_over_c_uint_max_without_calling_hook() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let hook = stats.hook();
        assert!(hook.is_active());

        // The smallest count that overflows `uInt`. Where `usize` is no wider
        // than `c_uint`, no such value exists and `usize::MAX` stands in — the
        // layout check rejects it first, and the assertion below (the hook was
        // never consulted) holds identically, so this never degrades into a
        // vacuous pass.
        let over = usize::try_from(c_uint::MAX)
            .ok()
            .and_then(|m| m.checked_add(1))
            .unwrap_or(usize::MAX);

        let buf: Option<AllocBuffer<u8>> = AllocBuffer::try_zeroed(over, hook);
        assert!(buf.is_none(), "an unrepresentable request must fail");
        assert_eq!(stats.allocs(), 0, "the hook must not be consulted at all");
        assert_eq!(
            stats.ooms(),
            0,
            "rejection happens before the hook, so it is not a reported OOM"
        );
        assert_eq!(stats.frees(), 0);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// A byte size that overflows, or exceeds the `isize::MAX` bound a Rust slice
    /// must respect, is rejected by the layout check before the hook is invoked.
    #[test]
    fn foreign_allocation_rejects_layout_overflow_without_calling_hook() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();

        for count in [usize::MAX, usize::MAX / 3, (isize::MAX as usize) / 2 + 1] {
            let buf: Option<AllocBuffer<u32>> = AllocBuffer::try_zeroed(count, stats.hook());
            assert!(buf.is_none(), "count {count} must be rejected");
        }
        assert_eq!(stats.allocs(), 0, "the hook must not be consulted");
        assert_eq!(stats.ooms(), 0);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// An active hook reporting out-of-memory yields [`None`] so the init paths
    /// can surface `Z_MEM_ERROR`; there is deliberately no global-allocator
    /// fallback (AAP §0.6.5).
    #[test]
    fn foreign_allocation_oom_yields_none_without_global_fallback() {
        let stats = crate::ffi::alloc::test_hook::HookStats::with_budget(0);

        let buf: Option<AllocBuffer<u16>> = AllocBuffer::try_zeroed(128, stats.hook());
        assert!(
            buf.is_none(),
            "OOM must not be masked by the global allocator"
        );
        assert_eq!(stats.ooms(), 1, "the hook reported the failure");
        assert_eq!(stats.allocs(), 0);
        assert_eq!(stats.frees(), 0);
    }

    /// When the caller's arena is exhausted mid-copy, [`AllocBuffer::try_clone`]
    /// reports the failure instead of relocating the copy to the global heap, and
    /// the source stays intact and usable.
    #[test]
    fn foreign_try_clone_propagates_oom_without_global_fallback() {
        // Exactly one successful allocation: the buffer, not its copy.
        let stats = crate::ffi::alloc::test_hook::HookStats::with_budget(1);

        {
            let mut original: AllocBuffer<u16> =
                AllocBuffer::try_zeroed(4, stats.hook()).expect("first allocation succeeds");
            original[2] = 0xBEEF;

            assert!(
                original.try_clone().is_none(),
                "an exhausted arena must yield None, never a global-allocator copy"
            );
            assert_eq!(stats.allocs(), 1);
            assert_eq!(stats.ooms(), 1);

            // The source survives the failed copy unchanged and remains writable.
            assert_eq!(&original[..], &[0, 0, 0xBEEF, 0][..]);
            original[3] = 0x00FF;
            assert_eq!(&original[..], &[0, 0, 0xBEEF, 0x00FF][..]);
        }

        assert_eq!(stats.frees(), 1, "only the one live region is freed");
        assert_eq!(stats.live_bytes(), 0);
    }

    /// A zero-length request never reaches the hook, matching C's refusal to ask
    /// an allocator for zero bytes, and always succeeds.
    #[test]
    fn empty_request_uses_owned_even_with_an_active_hook() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let buf: AllocBuffer<u32> =
            AllocBuffer::try_zeroed(0, stats.hook()).expect("empty requests always succeed");
        assert!(buf.is_empty());
        assert!(!buf.is_foreign());
        assert_eq!(stats.allocs(), 0);
    }

    /// Every element type the engines request is `ZeroValid`, so a byte-zeroed
    /// foreign region holds `T::default()` in every slot — the property that
    /// makes the boundary's zero-fill a correct initialization rather than a
    /// reinterpretation of arbitrary bytes.
    #[test]
    fn zero_valid_element_types_are_default_initialized_through_the_hook() {
        let stats = crate::ffi::alloc::test_hook::HookStats::new();

        let bytes: AllocBuffer<u8> = AllocBuffer::try_zeroed(3, stats.hook()).expect("u8");
        let words: AllocBuffer<u16> = AllocBuffer::try_zeroed(3, stats.hook()).expect("u16");
        let longs: AllocBuffer<u32> = AllocBuffer::try_zeroed(3, stats.hook()).expect("u32");

        assert_eq!(&bytes[..], &[u8::default(); 3][..]);
        assert_eq!(&words[..], &[u16::default(); 3][..]);
        assert_eq!(&longs[..], &[u32::default(); 3][..]);
        assert_eq!(u8::default(), 0);
        assert_eq!(u16::default(), 0);
        assert_eq!(u32::default(), 0);

        assert_eq!(stats.allocs(), 3);
        drop((bytes, words, longs));
        assert_eq!(stats.frees(), 3);
        assert_eq!(stats.live_bytes(), 0);
    }

    /// The default allocator carries no hook, so [`Allocator::allocate_zeroed`]
    /// is infallible and produces owned storage — the historical behaviour is
    /// bit-for-bit unchanged by the hook plumbing.
    #[test]
    fn default_allocator_carries_no_hook_and_allocates_owned() {
        let alloc = DefaultAllocator;
        assert!(!alloc.hook().is_active());

        let buf: AllocBuffer<u8> = alloc
            .allocate_zeroed(64)
            .expect("global allocation is infallible");
        assert!(!buf.is_foreign());
        assert_eq!(&buf[..], &[0u8; 64][..]);
        alloc.deallocate(buf);
    }

    /// F3 regression: the global-allocator path must be **fallible**.
    ///
    /// `vec![T::default(); count]` aborts the process when the request cannot be
    /// satisfied, whereas zlib reports a failed working-buffer allocation as
    /// `Z_MEM_ERROR`. `try_zeroed` now reserves fallibly, so an unsatisfiable
    /// request yields [`None`] and the caller can return `Z_MEM_ERROR` (AAP §0.6.5).
    ///
    /// A `usize::MAX`-element request is used because it can never be satisfied
    /// on any supported target, making the assertion deterministic and
    /// allocation-free in practice (the reservation is rejected on the capacity
    /// computation, before any memory is touched).
    #[test]
    fn try_zeroed_global_path_reports_failure_instead_of_aborting() {
        let none = AllocHook::none();
        assert!(!none.is_active());

        let huge: Option<AllocBuffer<u16>> = AllocBuffer::try_zeroed(usize::MAX, none);
        assert!(
            huge.is_none(),
            "an unsatisfiable global reservation must yield None, not abort"
        );

        // Ordinary requests are unaffected and still zero-filled.
        let ok: AllocBuffer<u16> =
            AllocBuffer::try_zeroed(16, none).expect("a small request succeeds");
        assert_eq!(ok.len(), 16);
        assert!(!ok.is_foreign(), "no hook means an Owned buffer");
        assert!(ok.iter().all(|&w| w == 0));
    }

    /// F4 regression: `try_zeroed_items` computes the element count from the
    /// `items * item_size` byte total and rejects geometries it cannot represent.
    ///
    /// The pair is forwarded to an active `zalloc` verbatim so a bounded caller
    /// sees C's exact arguments; on the global path only the resulting byte count
    /// matters, and these cases pin the arithmetic: an exact multiple of the
    /// element size succeeds, a non-multiple is rejected rather than silently
    /// truncated, and an overflowing product is rejected rather than wrapping.
    #[test]
    fn try_zeroed_items_geometry_is_checked() {
        let none = AllocHook::none();

        // 4 items x 2 bytes = 8 bytes = 4 u16 elements.
        let words: AllocBuffer<u16> =
            AllocBuffer::try_zeroed_items(4, 2, none).expect("exact multiple of size_of::<u16>()");
        assert_eq!(words.len(), 4);

        // 6 items x 1 byte = 6 bytes = 3 u16 elements (still an exact multiple).
        let odd_split: AllocBuffer<u16> =
            AllocBuffer::try_zeroed_items(6, 1, none).expect("6 bytes is 3 u16 elements");
        assert_eq!(odd_split.len(), 3);

        // 3 items x 1 byte = 3 bytes is not a whole number of u16 elements.
        let ragged: Option<AllocBuffer<u16>> = AllocBuffer::try_zeroed_items(3, 1, none);
        assert!(ragged.is_none(), "a partial element must be rejected");

        // An overflowing product must not wrap into a small allocation.
        let overflow: Option<AllocBuffer<u8>> = AllocBuffer::try_zeroed_items(usize::MAX, 2, none);
        assert!(overflow.is_none(), "items * item_size overflow must fail");

        // A zero-byte request is the allocation-free fast path in both spellings.
        let empty: AllocBuffer<u8> =
            AllocBuffer::try_zeroed_items(0, 4, none).expect("empty request succeeds");
        assert!(empty.is_empty());
    }

    /// F2 regression: `try_clone` copies an owned buffer's contents exactly and
    /// keeps it owned, so the fallible copy path is a drop-in for `Clone` on the
    /// global-allocator path.
    ///
    /// The foreign-arm behavior (duplicate through the same hook, report a
    /// refusal) needs C hooks and is covered in `crate::ffi::types`, which is the
    /// module allowed to write them.
    #[test]
    fn try_clone_preserves_owned_contents() {
        let buf = AllocBuffer::from_vec(alloc::vec![1u8, 2, 3, 4]);
        let copy = buf.try_clone().expect("owned copy succeeds");
        assert!(!copy.is_foreign());
        assert_eq!(&copy[..], &buf[..]);

        let empty: AllocBuffer<u32> = AllocBuffer::default();
        let empty_copy = empty.try_clone().expect("empty copy succeeds");
        assert!(empty_copy.is_empty());
    }
}
