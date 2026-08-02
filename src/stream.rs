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
//! ## Implementing [`Allocator`] outside this crate
//!
//! [`Allocator`] has **no** required items. Both allocation entry points,
//! [`allocate_zeroed`](Allocator::allocate_zeroed) and
//! [`allocate_zeroed_items`](Allocator::allocate_zeroed_items), as well as
//! [`hook`](Allocator::hook), are *provided* methods that default to the hook
//! path, so a downstream crate implements the trait by overriding whichever of
//! the two allocation methods it wants to serve and leaving
//! [`hook`](Allocator::hook) at its inactive default:
//!
//! ```
//! use zlib_rs::stream::{AllocBuffer, Allocator};
//!
//! #[derive(Default)]
//! struct CountingAllocator {
//!     // A real implementation would use a `Cell`/atomic; kept trivial here.
//! }
//!
//! impl Allocator for CountingAllocator {
//!     fn allocate_zeroed_items<T>(&self, items: usize, item_size: usize) -> Option<AllocBuffer<T>>
//!     where
//!         T: Copy + Default + zlib_rs::stream::ZeroValid + 'static,
//!     {
//!         // Refuse anything larger than 64 KiB to demonstrate a bounded arena.
//!         let bytes = items.checked_mul(item_size)?;
//!         if bytes > 64 * 1024 {
//!             return None;
//!         }
//!         AllocBuffer::try_zeroed_items(items, item_size, self.hook())
//!     }
//! }
//!
//! let alloc = CountingAllocator::default();
//! assert!(alloc.allocate_zeroed_items::<u8>(1, 16).is_some());
//! assert!(alloc.allocate_zeroed_items::<u8>(1, 1 << 20).is_none());
//! ```
//!
//! Every engine buffer and every engine-state footprint is requested through
//! those two methods — never through the global allocator directly — so a
//! downstream implementation genuinely governs a stream's memory. Which method
//! serves which C `ZALLOC` is tabulated on [`Allocator`] itself.
//!
//! ## Three surface properties that are easy to trip over
//!
//! The crate version is **not** a SemVer channel for the Rust API: it mirrors the
//! upstream C release identity the ABI reports — `zlibVersion()` yields
//! `"1.3.2.1-motley"` and `ZLIB_VERNUM` is `0x1321` (AAP §0.6.6) — and SemVer
//! cannot express the four-component motley string, so the crate version is
//! pinned to the C identity by design and says nothing about this module's Rust
//! surface. Three properties of that surface are therefore worth stating outright:
//!
//! * **An *active* hook is constructed only inside the `unsafe` zone.**
//!   [`AllocHook::none`] is the safe, inactive constructor;
//!   [`crate::ffi::types::alloc_hook_from_parts`] is the `unsafe` one, because
//!   building an active hook asserts a four-clause contract about two raw C
//!   function pointers (see that function's `# Safety`) that no safe constructor
//!   could check. It therefore belongs in the crate's only `unsafe` zone
//!   (AAP §0.6.2, directive D-6), and this module keeps `#![deny(unsafe_code)]`.
//! * **Duplicating a buffer is fallible.** [`AllocBuffer`] has no [`Clone`] impl;
//!   [`AllocBuffer::try_clone`] is the way to copy one. Cloning a foreign buffer
//!   re-enters the caller's `zalloc`, which may report out-of-memory, and an
//!   infallible `Clone` could only abort or silently switch allocators. Returning
//!   [`None`] keeps the failure visible and preserves C's allocation count and
//!   failure timing (AAP §0.6.5).
//! * **The element-type set is sealed.** Buffer elements are
//!   `T: Copy + Default + ZeroValid + 'static`. [`ZeroValid`] is a sealed marker
//!   implemented for the twelve integer primitives; the engines request `u8` and
//!   `u16`. Sealing keeps the set fixed and auditable inside this crate — see that
//!   trait for exactly what it does and does not assert.
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
/// materialized from a caller-`zalloc`'d region is therefore fixed here and
/// cannot be widened by a downstream crate.
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
/// initializes that region by **writing a `T::default()` value into every slot**
/// — `fill_default` over a `&mut [MaybeUninit<T>]`, never a memset of raw zero
/// bytes — and only then hands it back as `&[T]` / `&mut [T]`. That step is
/// therefore already correct for any `T: Copy + Default`, including a type whose
/// all-zero bit pattern would be an invalid value, and it does not depend on this
/// marker for its soundness.
///
/// What the bound adds is **defence in depth**: it restricts the element types a
/// foreign region may be materialized as to a small set audited inside this crate,
/// so the two sanctioned remedies for the hazard — value initialization and a
/// narrow element-type set — are both in force rather than one of them. Concretely
/// it means that
///
/// * a change to the foreign path that reverted to zero-filling raw bytes would
///   still be sound for every type the API admits, instead of becoming unsound at
///   a distance; and
/// * a downstream crate cannot widen the set, because [`ZeroValid`] is sealed
///   behind a private supertrait — an `enum`, `bool`, `char`, `NonZero*`,
///   reference or function-pointer element type is rejected at compile time on
///   every call site, whatever its representation.
///
/// [`AllocBuffer::try_zeroed`] is a *safe* function, so keeping that guarantee
/// structural rather than reviewer-enforced is what makes it durable.
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
/// request `u8` and `u16` buffers; the wider set costs nothing and keeps the
/// marker's contract stated in terms of the type property rather than of today's
/// call sites.
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
/// A type outside the sealed set is rejected at compile time. Sealing is what
/// does the rejecting, so this holds for *every* downstream type regardless of its
/// representation — the example below simply picks the case the marker's contract
/// is named for:
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
/// // `OneOnly` is `Copy + Default` but cannot be `ZeroValid`, because the trait
/// // is sealed and no downstream type may implement it. This line fails to
/// // compile.
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
/// A hook is *active* only when **both** `zalloc` and `zfree` are present, since
/// a region obtained from one must be released through the other. Only two
/// configurations can reach this type from a C caller, because the C
/// initialization boundary completes a partial pair before any hook is built:
///
/// * **Neither half supplied** — the hook is inactive and allocation uses the
///   global allocator. That is this port's built-in allocator, exactly as
///   `zcalloc`/`zcfree` are C's, and it is what AAP §0.6.3's "otherwise
///   `std::alloc` is used" clause prescribes for a wholly absent pair.
/// * **Both halves present** — the hook is active and every working buffer is
///   carved from the caller's `zalloc` and released through their `zfree`. An
///   active `zalloc` that reports out-of-memory is propagated as an allocation
///   failure; there is deliberately no global-allocator fallback on that path.
///
/// A pair with **exactly one half supplied** never becomes an `AllocHook`: C's
/// three `*Init*_` prologues substitute the library's own built-in for whichever
/// half is missing — `zcalloc` for a null `zalloc` (also clearing `opaque`),
/// `zcfree` for a null `zfree` (`inflate.c` L183-L196, `deflate.c` L400-L414,
/// `infback.c` L37-L50) — and `crate::ffi::types::init_allocator_prologue`
/// reproduces that substitution before constructing anything, so the hook this
/// type sees is already complete. The caller's own half is honored in full,
/// which is what keeps a deliberately failing `zalloc` observable as
/// `Z_MEM_ERROR` instead of being silently bypassed.
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
    ///    `items * size` writable bytes, aligned for the element type of the
    ///    buffer being reserved, and not aliased by any other live reference.
    ///    The engines request `u8` and `u16`; the sealed [`ZeroValid`] set bounds
    ///    what any other caller in this crate can ask for. A region that does not
    ///    satisfy the alignment is returned through `zfree` and reported as an
    ///    allocation failure rather than used.
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
    /// [`Default`] value — through this (active) hook.
    ///
    /// The request is served by the [`ForeignAlloc`] capability, which this
    /// module *declares* and the sanctioned `crate::ffi::alloc` zone
    /// *implements*; that is where the raw-pointer `unsafe`, the layout
    /// validation, and the alignment check are confined (AAP §0.6.2). The
    /// dependency therefore points one way only — the boundary implements a
    /// core-owned interface and this module names no `ffi` item (AAP §0.3.2 C4).
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
        // C's element-shaped `ZALLOC(strm, n, sizeof(Pos))` splits the request
        // exactly this way, so the hook sees `(count, size_of::<T>())`.
        <T as ForeignAlloc>::try_alloc_foreign(*self, count)
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
        <T as ForeignAlloc>::try_alloc_foreign_items(*self, items, item_size)
    }
}

/// Allocates a single `T` on the Rust global heap **fallibly**, yielding [`None`]
/// instead of aborting when the heap is exhausted.
///
/// [`Box::new`] aborts the process on allocation failure, whereas zlib reports a
/// failed state allocation as `Z_MEM_ERROR`. The engine-state constructors
/// (`DeflateState::new_in`, `InflateState::try_new_in`) route their single global
/// allocation through here so heap exhaustion becomes a return code rather than
/// an abort (AAP §0.6.5). The raw-pointer work lives behind the
/// [`FallibleBoxAlloc`] capability, which this module *declares* and the
/// sanctioned `crate::ffi::alloc` zone *implements*, so this module stays free of
/// `unsafe` and names no `ffi` item (AAP §0.6.2, §0.3.2 C4).
///
/// `value` is dropped normally if the allocation fails, releasing any working
/// buffers it already owns.
#[inline]
#[must_use]
pub(crate) fn try_box<T>(value: T) -> Option<Box<T>> {
    value.try_box_fallible()
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
// Core-declared allocation capabilities — implemented by the `ffi` boundary
// ---------------------------------------------------------------------------
//
// The AAP's layer ordering runs one way for this pair: `ffi` (layer 8) may
// depend on `stream` (layer 5), never the reverse (AAP §0.3.1, §0.6.2). The two
// primitives below need raw-pointer `unsafe`, which is permitted only inside
// `src/ffi/**`, yet they are needed *by* this module. The resolution is
// dependency inversion, the same pattern [`ForeignBuffer`] already uses: this
// module owns the interface, and the boundary supplies the implementation. No
// item under `crate::ffi` is named anywhere in this file, so no `stream -> ffi`
// edge exists (AAP §0.3.2 C4).
//
// Both traits are `pub(crate)`, so they are neither nameable nor implementable
// from outside the crate: the blanket implementations in `crate::ffi::alloc` are
// the only ones that can ever exist, and callers cannot substitute a different
// allocation runtime.

/// Fallible single-value heap boxing — the capability behind [`try_box`].
///
/// Declared here and implemented for every `Sized` type by a blanket
/// implementation in the sanctioned `crate::ffi::alloc` zone, where the
/// `core::alloc` call and the `Box::from_raw` reconstruction are confined.
///
/// Implementations must return [`None`] — never abort — when the global heap
/// cannot satisfy a `Layout::new::<Self>()` request, and must drop `self` in that
/// case so any buffers it owns are released (AAP §0.6.5).
pub(crate) trait FallibleBoxAlloc: Sized {
    /// Moves `self` onto the global heap, or returns [`None`] on exhaustion.
    fn try_box_fallible(self) -> Option<Box<Self>>;
}

/// Foreign (caller-`zalloc`'d) buffer allocation — the capability behind
/// [`AllocHook::try_alloc_zeroed`] and [`AllocHook::try_alloc_zeroed_items`].
///
/// Declared here and implemented for the whole [`ZeroValid`] element set by a
/// blanket implementation in the sanctioned `crate::ffi::alloc` zone, which owns
/// the `(items, item_size)` validation, the hook invocation, the alignment check,
/// the `T::default()` fill, and the `zfree`-on-drop.
///
/// Implementations must forward `(items, item_size)` to the caller's `zalloc`
/// verbatim so the hook observes exactly the arguments C's
/// `ZALLOC(strm, items, size)` passes, and must return [`None`] — with the region
/// already released through `zfree` if one was obtained — rather than falling
/// back to the global allocator (AAP §0.6.2, §0.6.3 has-hook clause, §0.6.5).
pub(crate) trait ForeignAlloc: Copy + Default + ZeroValid + 'static {
    /// Allocates `count` elements through `hook`, requesting them as
    /// `(count, size_of::<Self>())` — the split C uses for its element-shaped
    /// `ZALLOC(strm, n, sizeof(Pos))` requests.
    fn try_alloc_foreign(hook: AllocHook, count: usize) -> Option<Box<dyn ForeignBuffer<Self>>>;

    /// Allocates `items * item_size` bytes through `hook`, materialized as
    /// `items * item_size / size_of::<Self>()` elements holding
    /// [`Default::default`].
    fn try_alloc_foreign_items(
        hook: AllocHook,
        items: usize,
        item_size: usize,
    ) -> Option<Box<dyn ForeignBuffer<Self>>>;
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
/// request (`u8` and `u16`, whose `Default` is `0`) is byte-for-byte the
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
        // Fast path / default: an inactive hook (a C caller who supplied neither
        // half; a half-present pair is *completed* with the crate's built-in at
        // the `*Init*_` boundary, so it arrives here already active), an empty
        // request, or a zero-sized element type.
        // `try_owned` matches C `zcalloc`'s zero fill and
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
        // Zero-sized `T` has no meaningful buffer geometry. Every member of the
        // sealed `ZeroValid` set is a non-zero-sized integer primitive (the engines
        // request `u8` and `u16`), so this is unreachable in practice and is
        // rejected rather than silently mis-sized.
        if elem == 0 {
            return None;
        }
        let total = items.checked_mul(item_size)?;
        if total % elem != 0 {
            return None;
        }
        let count = total / elem;

        // Fast path / default: an inactive hook (neither half supplied; a
        // half-present pair is *completed* with the crate's built-in at the
        // `*Init*_` boundary and so arrives active), or an empty request.
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
/// `zcalloc` performs. [`allocate_zeroed_items`](Allocator::allocate_zeroed_items)
/// is the same operation expressed in C's `(items, size)` shape, and must yield
/// `items * item_size / size_of::<T>()` such elements.
/// [`deallocate`](Allocator::deallocate) consumes a buffer previously produced by
/// the same allocator.
///
/// Both allocation methods are *provided*, so an implementation may override
/// either, both, or neither. The defaults route the request through
/// [`hook`](Allocator::hook), which is what makes overriding `hook` alone
/// sufficient for a hook-forwarding allocator; overriding an allocation method
/// alone is sufficient for an allocator that manages storage itself.
///
/// # Which method the engines call
///
/// The deflate and inflate init, copy, and lazy-window paths allocate **every**
/// working buffer and the state footprint through this trait, choosing the method
/// whose shape matches the corresponding C `ZALLOC`:
///
/// | C request | Method the engines call |
/// |---|---|
/// | `ZALLOC(strm, 1, sizeof(deflate_state))` / `sizeof(struct inflate_state)` | [`allocate_zeroed_items`](Allocator::allocate_zeroed_items) |
/// | `ZALLOC(strm, w_size, 2 * sizeof(Byte))` (the doubled window) | [`allocate_zeroed_items`](Allocator::allocate_zeroed_items) |
/// | `ZALLOC(strm, w_size, sizeof(Pos))` / `ZALLOC(strm, hash_size, sizeof(Pos))` | [`allocate_zeroed`](Allocator::allocate_zeroed) |
/// | `ZALLOC(strm, lit_bufsize, LIT_BUFS)` (`pending_buf`) | [`allocate_zeroed_items`](Allocator::allocate_zeroed_items) |
/// | `ZALLOC(strm, 1 << wbits, sizeof(unsigned char))` (the inflate window) | [`allocate_zeroed`](Allocator::allocate_zeroed) |
///
/// A custom allocator therefore genuinely serves engine memory: it observes the
/// same request count, the same `(items, size)` pairs, and the same ordering a C
/// `zalloc` would (AAP §0.6.3, §0.6.5).
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
    /// are the plain integers `u8` and `u16`, whose [`Default`] is `0`. The bound
    /// is intentionally *not* relied on as a promise that an all-zero bit pattern
    /// is a valid `T`: the foreign path initializes by writing `T::default()`
    /// values.
    ///
    /// # Failure
    ///
    /// Returns [`None`] — which every caller translates into `Z_MEM_ERROR`
    /// (AAP §0.6.5) — for either of the two independent failure sources:
    ///
    /// * **Hook-backed allocation.** An active hook's `zalloc` reported
    ///   out-of-memory by returning null, or the request could not be served
    ///   through that hook soundly: a byte count unrepresentable in the C `uInt`
    ///   hook ABI or as a [`core::alloc::Layout`], or an address that does not
    ///   satisfy `T`'s alignment. There is deliberately **no** fallback to the
    ///   global allocator, so a caller who installed a bounded arena observes the
    ///   failure (AAP §0.6.3 has-hook clause).
    /// * **Global allocation.** With no active hook the buffer is an owned
    ///   [`Vec`], reserved through the fallible
    ///   [`Vec::try_reserve_exact`] path rather than an aborting
    ///   `vec![T::default(); count]`, so an exhausted Rust heap is reported as
    ///   [`None`] instead of aborting the process. This path is therefore *not*
    ///   infallible, even though it is the historical default and succeeds for
    ///   every realistic zlib buffer size.
    ///
    /// The default implementation forwards `(count, size_of::<T>())` to
    /// [`hook`](Allocator::hook) — the split C uses for its element-shaped
    /// `ZALLOC(strm, n, sizeof(Pos))` requests — so an allocator that only
    /// overrides `hook` needs nothing else.
    #[inline]
    fn allocate_zeroed<T>(&self, count: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        AllocBuffer::try_zeroed(count, self.hook())
    }

    /// Allocates a zero-initialised buffer using C's `(items, item_size)` request
    /// shape, materialising `items * item_size / size_of::<T>()` elements.
    ///
    /// Several C `ZALLOC` call sites split a byte count differently from
    /// `(count, size_of::<T>())`: the doubled sliding window is
    /// `ZALLOC(strm, w_size, 2 * sizeof(Byte))`, the pending/symbol buffer is
    /// `ZALLOC(strm, lit_bufsize, LIT_BUFS)`, and each engine state is
    /// `ZALLOC(strm, 1, sizeof(...))`. A caller's `zalloc` sees both arguments, so
    /// forwarding the pair verbatim is required for the hook to observe exactly
    /// what C passes it (AAP §0.6.2, §0.6.5). This method exists so those
    /// requests are expressible through the trait rather than only through the
    /// crate-internal buffer constructor.
    ///
    /// Fails under the same two conditions as
    /// [`allocate_zeroed`](Allocator::allocate_zeroed), plus when
    /// `items * item_size` is not an exact multiple of `size_of::<T>()`.
    ///
    /// The default implementation forwards the pair to
    /// [`hook`](Allocator::hook).
    #[inline]
    fn allocate_zeroed_items<T>(&self, items: usize, item_size: usize) -> Option<AllocBuffer<T>>
    where
        T: Copy + Default + ZeroValid + 'static,
    {
        AllocBuffer::try_zeroed_items(items, item_size, self.hook())
    }

    /// Returns the caller-allocator [`AllocHook`] this allocator forwards to,
    /// or [`AllocHook::none`] (the default) when it uses the global allocator.
    ///
    /// The deflate/inflate init paths read this so a state's lazily- or
    /// eagerly-allocated working buffers can be routed through the caller's
    /// `zalloc`/`zfree` (AAP §0.6.3).
    ///
    /// A Rust-native allocator has no reason to override this: it manages storage
    /// itself and should override
    /// [`allocate_zeroed`](Allocator::allocate_zeroed) /
    /// [`allocate_zeroed_items`](Allocator::allocate_zeroed_items) instead, which
    /// the engines call for every buffer. The default ([`AllocHook::none`]) is
    /// then correct.
    ///
    /// An implementation that genuinely needs to forward to a pair of C
    /// `zalloc`/`zfree` function pointers builds an *active* hook with
    /// [`crate::ffi::types::alloc_hook_from_parts`], the `unsafe` constructor at
    /// the FFI boundary. `AllocHook::new` itself is crate-private on purpose:
    /// constructing an active hook carries raw-pointer obligations that must be
    /// discharged by the caller, so the constructor lives in the layer where
    /// `unsafe` is permitted rather than in this `#![deny(unsafe_code)]` module
    /// (AAP §0.6.2).
    #[inline]
    fn hook(&self) -> AllocHook {
        AllocHook::none()
    }

    /// Whether the engines must charge the **engine-state footprint** to this
    /// allocator as an explicit request, mirroring C's
    /// `ZALLOC(strm, 1, sizeof(deflate_state))` (`deflate.c` L440) and
    /// `ZALLOC(strm, 1, sizeof(struct inflate_state))` (`inflate.c` L198).
    ///
    /// In C the engine state *is* a caller allocation, so a custom `zalloc` sees
    /// it first and a tight budget makes `deflateInit2_`/`inflateInit2_` fail
    /// before any working buffer is requested. Here the state lives in a Rust
    /// [`Box`], so the init paths additionally reserve an equally sized region
    /// through this trait purely to keep that accounting and failure timing
    /// identical (AAP §0.6.5).
    ///
    /// The default is `true`: an allocator that was deliberately installed is
    /// assumed to want C-equivalent accounting. The crate's own
    /// [`DefaultAllocator`] returns `false`, because there the `Box` already *is*
    /// the global-allocator request and a second reservation would double each
    /// stream's fixed overhead — the historical memory bounds must not change.
    /// The FFI allocator returns whether its hook is active, for the same reason:
    /// a C caller who left `zalloc`/`zfree` null must observe exactly the
    /// pre-existing footprint.
    ///
    /// When this returns `false` the reservation is skipped entirely and the
    /// state's reservation field stays empty; a copy
    /// ([`deflateCopy`](crate::deflate::deflate_copy) /
    /// [`inflateCopy`](crate::inflate::inflate_copy)) re-requests exactly what the
    /// source held, so the two paths cannot disagree.
    #[inline]
    fn reserves_state_footprint(&self) -> bool {
        true
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
    // Both allocation methods use the trait default. `hook` yields
    // `AllocHook::none`, so they produce owned, global-allocator `Vec` storage —
    // byte-for-byte the historical behavior — and the sole failure source is an
    // exhausted Rust heap, which `try_reserve_exact` reports as `None` rather
    // than aborting.

    /// `false`: the engine state's `Box` *is* the global-allocator request here,
    /// so reserving a second region of the same size would double every stream's
    /// fixed overhead. The historical memory bounds are preserved (AAP §0.6.5).
    #[inline]
    fn reserves_state_footprint(&self) -> bool {
        false
    }
}

/// An [`Allocator`] that forwards every request to one fixed [`AllocHook`].
///
/// This is the adapter that lets the hook-taking engine constructors
/// (`DeflateState::new_in`, `InflateState::try_new_in`,
/// [`crate::inflate::back::inflate_back_init_in`]) share a single implementation
/// with their allocator-taking counterparts: the engines allocate exclusively
/// through the [`Allocator`] trait, and a bare [`AllocHook`] becomes an
/// `Allocator` by wrapping it here.
///
/// With [`AllocHook::none`] this behaves exactly like [`DefaultAllocator`]
/// (global-allocator storage). With an active hook — built at the FFI boundary
/// with [`crate::ffi::types::alloc_hook_from_parts`] — every buffer is carved
/// from the caller's `zalloc` and released through their `zfree`, and an
/// out-of-memory report propagates as `Z_MEM_ERROR` with no global fallback
/// (AAP §0.6.3 has-hook clause, §0.6.5).
#[derive(Copy, Clone)]
pub struct HookAllocator(AllocHook);

impl fmt::Debug for HookAllocator {
    /// Reports only whether the wrapped hook is active. The hook's function
    /// pointers and `opaque` cookie are deliberately not printed: they are raw
    /// caller-owned values whose addresses carry no useful diagnostic information
    /// and would make debug output non-deterministic.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookAllocator")
            .field("active", &self.0.is_active())
            .finish()
    }
}

impl HookAllocator {
    /// Wraps `hook` as an [`Allocator`].
    ///
    /// Safe: every raw-pointer obligation was discharged when the *active* hook
    /// was constructed (see [`crate::ffi::types::alloc_hook_from_parts`]), and an
    /// inactive hook simply selects the global allocator.
    #[inline]
    #[must_use]
    pub const fn new(hook: AllocHook) -> Self {
        Self(hook)
    }
}

impl Default for HookAllocator {
    /// The global-allocator adapter — equivalent to [`DefaultAllocator`].
    #[inline]
    fn default() -> Self {
        Self(AllocHook::none())
    }
}

impl Allocator for HookAllocator {
    /// Forwards to the wrapped hook, so both allocation methods route through it.
    #[inline]
    fn hook(&self) -> AllocHook {
        self.0
    }

    /// Only an **active** hook is charged for the state footprint. An inactive
    /// hook is the global-allocator path, which must keep
    /// [`DefaultAllocator`]'s footprint exactly (AAP §0.6.5).
    #[inline]
    fn reserves_state_footprint(&self) -> bool {
        self.0.is_active()
    }
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
/// the defaults a [`DeflateState`] owns roughly 256 KiB of working buffers in
/// four allocations — the same count and the same byte total as reference zlib,
/// because the symbol region is overlaid inside `pending_buf` exactly as C
/// overlays it (see its "Memory footprint" section) — and an [`InflateState`] is
/// about 7 KiB plus its on-demand window. Boxing keeps `ZStream` itself small, and it makes this
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
    ///
    /// # Initial value
    ///
    /// A freshly constructed stream reports `0`, matching the `memset`-zeroed
    /// (or statically zero-initialised) `z_stream` that every C caller hands to
    /// `inflateInit`/`deflateInit`. Installing or resetting an *inflate* engine
    /// then publishes the wrapper's initial checksum — but, exactly as in C,
    /// **only when the stream is wrapped**:
    ///
    /// * zlib framing and automatic detection (`windowBits` `8..=15` and `+32`)
    ///   publish `1`, the Adler-32 of the empty input.
    /// * gzip framing (`windowBits` `+16`) publishes `0`, a fresh CRC-32
    ///   accumulator.
    /// * raw framing (`windowBits` `-8..=-15`) publishes *nothing*, so the field
    ///   stays at its constructed `0`. A raw DEFLATE stream carries no checksum,
    ///   and `inflate.c` L108-L109 guards the assignment with
    ///   `if (state->wrap)`; that guard is reproduced verbatim by
    ///   [`inflate_reset_keep`](crate::inflate::inflate_reset_keep), so this
    ///   port and reference zlib agree on the observed value.
    ///
    /// Deflate is unconditional in both C (`deflateResetKeep` always assigns)
    /// and this port, and its seed is `1` for raw as well as zlib framing
    /// because `adler32(0, Z_NULL, 0) == 1`. Only the decoder therefore exhibits
    /// the wrapper-dependent behaviour above.
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

impl ZStream<DefaultAllocator> {
    /// Creates a new stream backed by the [`DefaultAllocator`] (the Rust global
    /// allocator), with no engine installed.
    ///
    /// Every observable field starts at its zero value: both counters at `0`,
    /// [`data_type`](ZStream::data_type) at `Z_BINARY` (`0`),
    /// [`adler`](ZStream::adler) at `0`, the internal `reserved` word at `0`,
    /// and [`msg`](ZStream::msg) at [`None`]. That makes this the exact
    /// idiomatic counterpart to the `memset`-zeroed C `z_stream` a caller hands
    /// to `deflateInit`/`inflateInit`, so a stream observed through this API and
    /// the same stream observed through [`crate::ffi`] report identical values
    /// at every point in its lifetime.
    ///
    /// The checksum seed is deliberately `0` rather than `1`: the wrapper's
    /// initial checksum is published by the engine, and for raw framing C
    /// publishes nothing at all. See [`adler`](ZStream::adler) for the full
    /// per-wrapper contract.
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
            // `0`, not the Adler-32 of the empty input: a C caller reaches
            // `inflateInit`/`deflateInit` with a `memset`-zeroed `z_stream`, and
            // the engine — not the constructor — publishes the wrapper's
            // initial checksum. Seeding `1` here would be observable for raw
            // framing, where C's `if (state->wrap)` guard (`inflate.c`
            // L108-L109) leaves the field untouched forever. See the field docs.
            adler: 0,
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

    /// Mutably borrows the installed decompression engine **together with** a
    /// shared borrow of this stream's allocator, or [`None`].
    ///
    /// `state` and `alloc` are distinct fields, so borrowing one mutably and the
    /// other immutably is sound; the compiler cannot see that through two
    /// separate accessor calls, hence this combined one. It exists so paths that
    /// mutate the decoder *and* need to allocate — the lazy window allocation in
    /// `updatewindow`, reached from `inflate` and `inflateSetDictionary` — can
    /// route their allocation through the [`Allocator`] trait rather than
    /// bypassing it (AAP §0.6.3).
    #[inline]
    #[must_use]
    pub(crate) fn inflate_state_and_allocator(&mut self) -> Option<(&mut InflateState, &A)> {
        match &mut self.state {
            StreamState::Inflate(state) => Some((state, &self.alloc)),
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
    /// resets its own internal fields and publishes `data_type` plus — for
    /// deflate always, and for inflate only when the stream is wrapped — the
    /// wrapper's initial `adler`, while these stream-level fields are cleared
    /// here. See [`adler`](Self::adler) for why raw inflate publishes nothing.
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
        // `0`, matching a `memset`-zeroed C `z_stream`; the engine — not the
        // constructor — publishes the wrapper's initial checksum, and for raw
        // framing it publishes nothing at all.
        assert_eq!(strm.adler, 0);
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
        assert_eq!(strm.adler, 0);
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
    // Caller-allocator hook contract
    //
    // The C `alloc_func`/`free_func` pair is raw-pointer machinery, so the
    // counting backing store lives in the crate's designated unsafe boundary
    // (`crate::ffi::alloc::test_hook`) and is merely *driven* from here. That
    // keeps this file's `#![deny(unsafe_code)]` intact while still asserting
    // exact hook invocation counts and symmetric cleanup.
    // -----------------------------------------------------------------------

    /// [`Allocator::reserves_state_footprint`] decides whether the engines charge
    /// the engine-state footprint to the allocator, and its value must be exactly
    /// "this allocator is not the global-allocator path".
    ///
    /// [`DefaultAllocator`] declines (the state `Box` already *is* the global
    /// request, so a second reservation would double every stream's fixed
    /// overhead), an inactive [`HookAllocator`] behaves identically to it, an
    /// active hook accepts (C charges `ZALLOC(strm, 1, sizeof(deflate_state))`),
    /// and a bare custom implementation accepts by default so a downstream
    /// allocator observes the request C's `zalloc` would see.
    #[test]
    fn state_footprint_reservation_tracks_the_allocator_kind() {
        assert!(
            !DefaultAllocator.reserves_state_footprint(),
            "the global default must not double a stream's fixed overhead"
        );
        assert!(
            !HookAllocator::default().reserves_state_footprint(),
            "an inactive hook is the global path and must match DefaultAllocator"
        );

        let stats = crate::ffi::alloc::test_hook::HookStats::new();
        let active = HookAllocator::new(stats.hook());
        assert!(
            active.reserves_state_footprint(),
            "an active caller hook must be charged for the state, as C charges its zalloc"
        );

        /// A downstream-style allocator: it overrides nothing but the allocation
        /// method, so every other item comes from the trait defaults.
        struct Custom;
        impl Allocator for Custom {}
        assert!(
            Custom.reserves_state_footprint(),
            "a custom allocator must observe the state request by default"
        );
    }

    /// A hook carrying only `zalloc` is **inactive** per the AAP's has-hook
    /// clause (§0.6.3): [`AllocBuffer::try_zeroed`] must use owned
    /// (global-allocator) storage and must never consult the caller's `zalloc`.
    ///
    /// This pins the *type-level* contract, which is what makes the buffer's
    /// backing store and its deallocator provably the same: allocation and
    /// release always come from one allocator, which is exactly what the
    /// `frees() == 0` assertion below observes.
    ///
    /// It is **not** the behavior a C caller who supplies one half gets. C
    /// defaults the two halves **independently** — `deflate.c` L400-L407 installs
    /// `zcalloc` only when `zalloc` is null (also clearing `opaque`), and
    /// L408-L413 installs `zcfree` only when `zfree` is null — and
    /// `crate::ffi::types::init_allocator_prologue` reproduces that substitution
    /// at the C boundary. Such a caller therefore arrives here with a *complete*
    /// pair whose missing half is the crate's own `malloc`/`free`-backed built-in,
    /// and their supplied half is honored in full. A half-present hook is
    /// reachable only from within the crate — as here — or from a stream whose
    /// allocator fields were mutated after initialization, which
    /// `CAllocator::is_half_present` rejects exactly as C's `*StateCheck`
    /// functions do.
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
    /// through the caller and the buffer is released by ordinary drop glue. As
    /// above this pins the type-level contract; a C caller who supplies only
    /// `zfree` has the missing `zalloc` substituted at the initialization
    /// boundary and reaches this type with a complete, active pair.
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

    /// A foreign region holds `T::default()` in every slot, because the boundary
    /// initializes it by *writing* that value rather than by reinterpreting
    /// whatever bytes the hook returned. Asserted for the two element types the
    /// engines request (`u8`, `u16`) and for `u32` as a further member of the
    /// sealed [`ZeroValid`] set, all three of whose `Default` is `0`.
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

    /// Regression guard: the global-allocator path must be **fallible**.
    ///
    /// `vec![T::default(); count]` aborts the process when the request cannot be
    /// satisfied, whereas zlib reports a failed working-buffer allocation as
    /// `Z_MEM_ERROR`. `try_zeroed` reserves fallibly, so an unsatisfiable request
    /// yields [`None`] and the caller can return `Z_MEM_ERROR` (AAP §0.6.5).
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

    /// Regression guard: `try_zeroed_items` computes the element count from the
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

    /// Regression guard: `try_clone` copies an owned buffer's contents exactly and
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
