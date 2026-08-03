//! gzip (`.gz`) stdio-like file I/O layer — the `gz*` API of the `zlib-rs`
//! crate.
//!
//! This module is the root of the `src/gz/` tree and the idiomatic, memory-safe
//! Rust port of the C gzip file-I/O sources — `gzlib.c`, `gzread.c`,
//! `gzwrite.c`, `gzclose.c`, and their shared internal header `gzguts.h`. It is
//! layered directly over [`std::fs::File`] + [`std::io`] and is driven by the
//! crate's own deflate/inflate engines ([`crate::deflate`] / [`crate::inflate`])
//! rather than by any C code, so the shipped artifact carries zero C dependency.
//!
//! # Feature gating
//!
//! The gz layer is the one part of the crate that fundamentally requires the
//! standard library (for filesystem access), so the entire module is compiled
//! only when the `gz-io` Cargo feature is enabled — `src/lib.rs` wires it in
//! with `#[cfg(feature = "gz-io")] pub mod gz;`. Because `gz-io` implies `std`
//! (and `gzip`), items here may freely use [`std`] without an additional
//! `#[cfg(feature = "std")]` guard; finer-grained gating is applied inside the
//! submodules only where genuinely needed.
//!
//! # Structure: private submodules + curated re-exports
//!
//! The concrete behavior lives in five sibling submodules, each a port of one C
//! translation unit (plus the shared state ported from `gzguts.h`):
//!
//! | Submodule | C source    | Responsibility                                   |
//! |-----------|-------------|--------------------------------------------------|
//! | `state`   | `gzguts.h`  | [`GzState`] / [`GzMode`] / `How` — the open file |
//! | `open`    | `gzlib.c`   | open, configure, position, and error inspection  |
//! | `read`    | `gzread.c`  | decompressing reads + [`std::io::Read`] adapter  |
//! | `write`   | `gzwrite.c` | compressing writes + [`std::io::Write`] adapter  |
//! | `close`   | `gzclose.c` | flush-and-close teardown                         |
//!
//! Unlike the sibling engine layers (which expose their submodules with
//! `pub mod`), these submodules are declared **private** and the public surface
//! is assembled here through explicit re-exports. This yields a single, curated
//! `zlib_rs::gz::<fn>` façade for downstream Rust users and one stable point for
//! the FFI shim layer (`src/ffi/gz.rs`) to depend on via `crate::gz::*`. The C
//! idiom `#include "gzguts.h"` therefore becomes `use crate::gz::state::GzState;`
//! internally, while external callers see only the re-exported API. Sibling
//! submodules still reference one another by absolute path (for example
//! `crate::gz::read::gz_look`), which resolves because each submodule is a
//! descendant of this (public) `gz` module even though the submodules are
//! private.
//!
//! # Idiomatic Rust API vs. C-compatible functions
//!
//! Two complementary surfaces are provided:
//!
//! * A **first-class Rust API**: [`GzState`] implements [`std::io::Read`],
//!   [`std::io::BufRead`], and [`std::io::Write`], so an open file behaves like
//!   any other reader or writer, and is opened with [`gzopen`] / [`gzdopen`].
//!   Its [`Drop`] impl only **releases resources** — it frees the I/O buffers,
//!   ends the deflate/inflate stream, and closes the file descriptor — but it
//!   deliberately does **not** finalize gzip output: it emits no `Z_FINISH`
//!   flush and surfaces no deferred write error. Dropping a *writer* therefore
//!   leaves a truncated, invalid member, so a write handle must be closed
//!   explicitly with [`gzclose`] / [`gzclose_w`] to produce a complete, valid
//!   gzip stream and to observe any pending write error.
//! * A set of **C-compatible entry points** (`gzread`, `gzwrite`, `gzgetc`, …)
//!   that mirror the exact zlib prototypes. These are re-exported publicly (and
//!   are also what the FFI boundary consumes via `crate::gz::*`); for everyday
//!   Rust code the trait-based API above is usually more convenient.
//!
//! # Safety
//!
//! This layer contains **zero `unsafe`**. The moving C output pointer `x.next`
//! is modelled as a bounds-checked [`usize`] index, owned [`std::fs::File`] and
//! `Vec<u8>` buffers replace the raw `fd`/`in`/`out` members, and [`Drop`]
//! subsumes the manual `inflateEnd`/`deflateEnd`/`close` teardown. All
//! raw-pointer, raw-fd, and C-string handling for the FFI `gz*` shims lives in
//! `src/ffi/gz.rs`, never here. Targets the Rust 2024 edition, MSRV 1.85.0.

// ---------------------------------------------------------------------------
// Shared constant (ported from `gzguts.h`).
// ---------------------------------------------------------------------------

/// Default gz I/O buffer size — the direct port of the C `#define GZBUFSIZE
/// 8192` from `gzguts.h`.
///
/// This is the default value of a file's requested buffer size (`want`) and is
/// **doubled** for the working buffers: the output buffer when reading and the
/// input buffer when writing. As the C header notes, "this and twice this must
/// be able to fit in an unsigned type" — a constraint trivially satisfied by
/// [`usize`].
///
/// It is defined here at the module root (rather than in a submodule) because it
/// is shared across the `open`, `read`, and `write` families: `open.rs` seeds a
/// new file's `want` from `crate::gz::GZBUFSIZE`, and the read/write drivers
/// size their working buffers from that `want`.
pub(crate) const GZBUFSIZE: usize = 8192;

// ---------------------------------------------------------------------------
// Shared fallible-allocation helper.
// ---------------------------------------------------------------------------

/// Allocates a zero-filled `Vec<u8>` of `len` bytes, returning [`None`] if the
/// allocation cannot be satisfied.
///
/// # Why this exists
///
/// Every gz buffer in reference zlib comes from `malloc`, and a `NULL` return is
/// a *recoverable* condition that the C code reports as `Z_MEM_ERROR` /
/// "out of memory" (`gzread.c` L295-L305 in `gz_look`, `gzwrite.c` L14-L29 in
/// `gz_init`). Rust's `vec![0u8; len]` has no such path. It terminates the
/// process in both of its failure modes: a `len` beyond the `isize::MAX` byte
/// ceiling panics with "capacity overflow", and a `len` the allocator merely
/// cannot satisfy calls `handle_alloc_error`, which aborts. Since both crate
/// profiles set `panic = "abort"`, neither is even catchable. Terminating is not
/// a stricter version of returning `Z_MEM_ERROR`; it is a different, observable
/// behavior that a C caller cannot intercept, so using `vec!` for a
/// caller-influenced size would break the allocation-failure parity the port is
/// required to preserve (AAP §0.6.5).
///
/// The size is caller-influenced in exactly the way that matters: `gzbuffer`
/// accepts any `want` up to `UINT_MAX >> 1` — which is correct C parity, since C
/// validates the request only against that bound — and the buffers are then
/// sized `want` and `want << 1`. A legitimate `gzbuffer(file, 0x7fff_ffff)`
/// therefore asks for ~2 GiB + ~4 GiB on the next write. Reference zlib answers
/// that with `Z_MEM_ERROR`; so must this port.
///
/// [`Vec::try_reserve_exact`] provides the fallible half. The subsequent
/// [`Vec::resize`] cannot itself fail: the exact capacity is already reserved,
/// so `resize` only writes zeroes into memory this call already owns.
///
/// # Placement
///
/// It lives at the module root because both halves of the layer need it — the
/// read driver for the `gz_look` buffers and the write driver for the `gz_init`
/// buffers — and a single definition keeps the two paths bit-for-bit consistent
/// in how they fail.
pub(crate) fn alloc_zeroed(len: usize) -> Option<Vec<u8>> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, 0);
    Some(v)
}

// ---------------------------------------------------------------------------
// Submodule declarations.
//
// These are intentionally PRIVATE: the public API is assembled from the
// re-exports below, giving a single curated `zlib_rs::gz::*` façade and one
// dependency point for `src/ffi/gz.rs`. `state` is the foundational module —
// every other submodule operates on the `GzState` it defines — though the
// declaration order is cosmetic to the compiler (and normalized alphabetically
// by `rustfmt`, matching the sibling engine modules).
// ---------------------------------------------------------------------------
mod close;
mod open;
mod read;
mod state;
mod write;

// ---------------------------------------------------------------------------
// Public re-exports — the gz* API and the FFI-visible state types.
// ---------------------------------------------------------------------------

/// Open-file state ([`GzState`]) and its open-mode enum ([`GzMode`]).
///
/// `GzState` carries the [`std::io::Read`] / [`std::io::BufRead`] /
/// [`std::io::Write`] / [`Drop`] impls, so re-exporting the type alone gives
/// downstream users the full idiomatic API. `src/ffi/gz.rs` uses these to build
/// the opaque `gzFile` handle and its `#[repr(C)] gzFile_s` mirror.
pub use state::{GzMode, GzState};

// The read look-ahead mode (`LOOK`/`COPY`/`GZIP` in C) is an internal detail
// surfaced only at crate visibility so the layer (and the FFI shim) can name it;
// it is not part of the public API. The `pub(crate)` re-export matches the
// type's own `pub(crate)` visibility (a `pub use` of it would fail to compile).
//
// `#[allow(unused_imports)]`: the re-export exists so that a consumer OUTSIDE
// this module — `src/ffi/gz.rs` is the intended one — can name the look-ahead
// mode as `crate::gz::How`, because the `state` submodule is private and offers
// no other path from the FFI layer. As it stands the shim has no need for it:
// `src/ffi/gz.rs` never mentions `How`, and this module's sibling `close.rs`
// reaches the type by its in-layer path `crate::gz::state::How` instead. The
// only reference to the re-exported name is in this module's own
// `#[cfg(test)]` block, which is a separate compilation target, so the
// library-only target sees no consumer at all and the unused-import lint would
// fire without the allow. The re-export is kept because the layer contract
// specifies it as the single sanctioned cross-layer path to `How`.
#[allow(unused_imports)]
pub(crate) use state::How;

/// Open / configure / position / error-inspection family, ported from
/// `gzlib.c`. Every function is part of the public zlib API and is re-exported
/// publicly so callers use `zlib_rs::gz::gzopen(...)` and friends.
pub use open::{
    gzbuffer, gzclearerr, gzdirect, gzdopen, gzeof, gzerror, gzoffset, gzoffset64, gzopen,
    gzopen64, gzrewind, gzseek, gzseek64, gzsetparams, gztell, gztell64,
};

/// Read family, ported from `gzread.c` — the C-compatible reading entry points.
/// Re-exported publicly so callers may use `zlib_rs::gz::gzread(...)`, and the
/// FFI shim layer can reach them via `crate::gz::*`. The idiomatic alternative
/// is the [`std::io::Read`] / [`std::io::BufRead`] impls on [`GzState`].
pub use read::{gzfread, gzgetc, gzgetc_, gzgets, gzread, gzungetc};

/// Write family, ported from `gzwrite.c` — the C-compatible writing entry
/// points. Re-exported publicly (and reachable by the FFI shim layer via
/// `crate::gz::*`); the idiomatic alternative is the [`std::io::Write`] impl on
/// [`GzState`]. Both the formatting entry point `gzprintf` and its idiomatic
/// [`core::fmt::Arguments`]-based `gzvprintf` form are surfaced.
pub use write::{gzflush, gzfwrite, gzprintf, gzputc, gzputs, gzvprintf, gzwrite};

/// Close family, ported from `gzclose.c`. Explicit teardown mirroring
/// `gzclose` / `gzclose_r` / `gzclose_w`. Only [`gzclose_w`] finalizes a write
/// stream — it emits the `Z_FINISH` flush and the gzip trailer and surfaces any
/// pending write error. Simply dropping a [`GzState`] releases its buffers,
/// stream, and file descriptor but does **not** finalize gzip output, so a
/// writer must be closed explicitly to produce a valid stream.
pub use close::{gzclose, gzclose_r, gzclose_w};

// The descriptor-releasing close finalizers, surfaced at crate visibility for
// `src/ffi/gz.rs` only.
//
// They exist because reference zlib's C contract depends on an operation this
// layer cannot perform: reporting a failing `close(2)` as `Z_ERRNO`
// (`gzread.c` L665-L667, `gzwrite.c` L695-L696), which needs an `unsafe` libc
// call. That `unsafe` belongs to the FFI boundary (AAP §0.6.2, §0.8.1 D-6), so
// this layer supplies the safe half — "finalize everything and hand me the
// still-open descriptor" — and the shim supplies the `unsafe` remainder.
pub(crate) use close::{gzclose_r_release, gzclose_release, gzclose_w_release};

/// The descriptor-flag contract between this layer and `src/ffi/gz.rs`: unix
/// only, because both flags it carries are POSIX descriptor bits.
#[cfg(unix)]
pub(crate) use open::{DescriptorRequest, descriptor_request};

// The path-opening entry points, surfaced at crate visibility for
// `src/ffi/gz.rs`. Both are portable — `gzopen`, `gzopen64` and `gzopen_w` reach
// them wherever `gz-io` is enabled — so neither carries a platform gate.
pub(crate) use open::{gzopen_bytes, gzopen64_bytes};

// The descriptor-adopting opener, the mode pre-validator and the file slot,
// surfaced at crate visibility for the descriptor-adopting half of
// `src/ffi/gz.rs` — hence the `cfg`, which is a correctness statement rather
// than warning suppression.
//
// `validate_mode` exists so an invalid `gzdopen` mode is rejected *before* the
// caller's descriptor is adopted (`gzlib.c` L150-L197 precede L263), which must
// happen before the `unsafe` `File::from_raw_fd` / `File::from_raw_handle`;
// `gzdopen_bytes` is the safe remainder of that adoption, and `GzFile` is what
// the shim installs the adopted descriptor into once allocation succeeds. All
// three consumers live inside the `#[cfg(any(unix, windows))]` `gzdopen` /
// `dopen_state` pair, and the shim's own import of `GzFile` is already
// `#[cfg(all(any(unix, windows), feature = "gz-io"))]` — adopting a raw C `int`
// descriptor is `from_raw_fd` on Unix and `_get_osfhandle` plus
// `from_raw_handle` on Windows, with no portable `std` equivalent anywhere
// else, so the fallback `gzdopen` returns null unconditionally and reaches none
// of the three names.
//
// Without these gates all three re-exports are genuinely unused on such a
// target and the compiler says so, which is a real signal, not noise: an
// unconditional `pub(crate) use` claims a crate-wide consumer that does not
// exist there. Gating states the platform contract instead of muting the
// diagnostic, so no `#[allow(unused_imports)]` is needed anywhere and the
// warnings-denied CI gates stay meaningful.
#[cfg(any(unix, windows))]
pub(crate) use open::{gzdopen_bytes, validate_mode};
#[cfg(any(unix, windows))]
pub(crate) use state::GzFile;

#[cfg(test)]
pub(crate) mod test_temp {
    //! Hardened temporary-file support shared by every `gz`-family test module.
    //!
    //! # Why this module exists
    //!
    //! The gzip layer is the only part of this crate that touches the filesystem,
    //! so it is the only part whose tests need real files on disk. The obvious way
    //! to get one — name it `…_<pid>.gz` in [`std::env::temp_dir`] and open it with
    //! [`std::fs::File::create`] — is unsafe on a shared `/tmp` in three separate
    //! ways, and every one of them was present across six test surfaces in this
    //! crate before this module replaced them:
    //!
    //! * **CWE-377, insecure temporary file.** The name is derived entirely from
    //!   public information, so any other user on the host can compute it before
    //!   the test runs.
    //! * **CWE-59, link following.** `File::create` opens `O_CREAT | O_TRUNC`
    //!   *without* `O_EXCL` and follows a final-component symlink, so a link
    //!   planted at the predicted name redirects the write to a target of the
    //!   planter's choosing — and truncates it on the way.
    //! * **CWE-367, time-of-check/time-of-use.** Any `exists()`-then-create or
    //!   `remove_file`-then-create sequence has a window between the two calls.
    //!   [`std::fs::create_dir_all`] is the same defect in directory form: it
    //!   succeeds when the path is *already* a directory — or a symlink to one —
    //!   silently adopting a tree the test did not create and will later remove
    //!   recursively.
    //!
    //! # How the hazards are removed
    //!
    //! Uniqueness and exclusivity are carried by a **directory**, not by a
    //! filename. [`create_private_dir`] issues a single non-recursive `mkdir(2)`,
    //! which is atomic, never follows a symlink for the final component, and fails
    //! with [`io::ErrorKind::AlreadyExists`] rather than adopting whatever is
    //! already there. On Unix the `0o700` mode is applied by that same syscall, so
    //! there is no interval in which the directory is group- or world-accessible
    //! and no `set_permissions` call to race. An occupied candidate name is
    //! **skipped, never deleted**, so a planted symlink is neither followed nor
    //! destroyed. Files are then created inside that fresh directory with
    //! [`create_new_file`] (`O_CREAT | O_EXCL`), which cannot truncate and cannot
    //! follow a link. Cleanup is [`Drop`]-owned so it also runs when an assertion
    //! unwinds, and it removes only a directory this guard brought into existence.
    //!
    //! These are *creation-time* guarantees. The guards hold paths rather than open
    //! handles, so they make no claim that a path still resolves to the same object
    //! later; on non-Unix targets the directory mode is the platform default, which
    //! is acceptable because Windows already gives each user a private `%TEMP%`.
    //!
    //! # Provenance
    //!
    //! Consolidated from the two independently hardened implementations that were
    //! already in this crate — `src/gz/write.rs` (private-directory `TempFile` with
    //! [`Deref`](core::ops::Deref) to [`Path`]) and `src/gz/state.rs`
    //! ([`safe_component`] plus the retrying `TempGz`) — so that the remaining
    //! `gz` test surfaces share one audited implementation instead of each
    //! re-deriving it. It lives here, at the `gz` module root, because that is the
    //! nearest common ancestor of every consumer; `pub(crate)` and `#[cfg(test)]`
    //! keep it out of every shipped artifact. `src/ffi/alloc.rs`'s
    //! `pub(crate) mod test_hook` is the established precedent for this shape.

    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Bounded retry budget for finding an unused directory name.
    ///
    /// Each attempt only advances the candidate name, so exhausting the budget
    /// means 64 distinct names were all simultaneously occupied — a broken
    /// environment rather than a collision, and worth failing loudly for.
    const MAX_ATTEMPTS: u32 = 64;

    /// Process-wide counter making concurrently-created directories distinct
    /// within a single test binary, where the pid is shared by every thread.
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Reduces an arbitrary string to a single safe path component.
    ///
    /// Tags reach this module from module-local literals, but `CLONE_INDEX` is
    /// ambient input read from the environment and therefore outside this crate's
    /// control. Interpolating such a value into a path unfiltered is a
    /// directory-traversal defect (CWE-22): a value like
    /// `slot/../../security_target` escapes the temporary directory lexically and
    /// resolves somewhere else entirely.
    ///
    /// Only ASCII alphanumerics, `_`, and `-` survive, which drops every character
    /// that could terminate the component or refer to a parent — `/`, `\`, `.` (so
    /// `..` collapses away completely), `:`, NUL, and every non-ASCII byte. The
    /// result is truncated so an absurdly long value cannot push the path past a
    /// filesystem limit, and an input filtering down to nothing becomes `x`, so the
    /// function is total and every caller receives a usable component.
    pub(crate) fn safe_component(raw: &str) -> String {
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

    /// Creates `path` as a new directory, private to the current user, failing if
    /// anything already occupies the name.
    ///
    /// Non-recursive on purpose. Unlike [`std::fs::create_dir_all`] this reports
    /// [`io::ErrorKind::AlreadyExists`] when the name is taken — including when it
    /// is taken by a symlink — which is what lets the callers move to the next
    /// candidate instead of following the link or deleting it. On Unix the `0o700`
    /// mode is handed to `mkdir(2)` itself, so the directory is never even briefly
    /// group- or world-accessible.
    ///
    /// # Errors
    ///
    /// Whatever `mkdir(2)` reports, unchanged, so callers can distinguish a name
    /// collision from a genuine failure.
    pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new().mode(0o700).create(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::DirBuilder::new().create(path)
        }
    }

    /// Creates `path` exclusively for writing: it must not already exist, no
    /// symlink at that name is followed, and nothing is truncated.
    ///
    /// # Panics
    ///
    /// If the file cannot be created exclusively. Inside a
    /// [`TempDir`]-owned directory that is a genuine environment failure, since the
    /// directory did not exist a moment earlier.
    pub(crate) fn create_new_file(path: &Path) -> File {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap_or_else(|e| panic!("exclusively create {}: {e}", path.display()))
    }

    /// Builds the candidate directory name for attempt `attempt`.
    ///
    /// The `blitzy_adhoc_test_` prefix marks it as a validation artifact that must
    /// never be committed. The remaining components — a sanitized caller tag, the
    /// sanitized `CLONE_INDEX`, the process id, a monotonic counter, and the retry
    /// ordinal — keep the name distinct across parallel test threads, across
    /// concurrent `cargo test` invocations, and across sibling clones of this
    /// repository sharing one `/tmp`.
    fn candidate(tag: &str, serial: u32, attempt: u32) -> PathBuf {
        let clone = safe_component(&std::env::var("CLONE_INDEX").unwrap_or_default());
        let tag = safe_component(tag);
        let pid = std::process::id();
        std::env::temp_dir().join(format!(
            "blitzy_adhoc_test_{tag}_{clone}_{pid}_{serial}_{attempt}"
        ))
    }

    /// An exclusively created, caller-private directory, removed with its contents
    /// when the guard drops.
    ///
    /// This is the unit of ownership: the directory is what was created
    /// exclusively, so names *inside* it may be plain and readable. Use
    /// [`Self::child`] to name a path within it and [`Self::create_child`] to
    /// materialize one.
    pub(crate) struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        /// Creates a fresh private directory tagged with `tag`.
        ///
        /// # Panics
        ///
        /// If no unused name can be created within [`MAX_ATTEMPTS`], or if
        /// `mkdir(2)` fails for a reason other than the name being taken. A
        /// collision is retried rather than reported, and nothing pre-existing is
        /// ever removed.
        pub(crate) fn new(tag: &str) -> Self {
            let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
            for attempt in 0..MAX_ATTEMPTS {
                let dir = candidate(tag, serial, attempt);
                match create_private_dir(&dir) {
                    Ok(()) => return Self { dir },
                    // The name is taken — possibly by a planted symlink. Skip it;
                    // never follow it and never delete it.
                    Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create private temp dir {}: {e}", dir.display()),
                }
            }
            panic!("no private temp directory available after {MAX_ATTEMPTS} attempts");
        }

        /// The directory itself.
        pub(crate) fn path(&self) -> &Path {
            &self.dir
        }

        /// Names — without creating — a path inside this directory.
        pub(crate) fn child(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }

        /// Creates a file inside this directory with create-new semantics and
        /// returns its path, discarding the handle.
        ///
        /// # Panics
        ///
        /// If the file already exists or cannot be created.
        pub(crate) fn create_child(&self, name: &str) -> PathBuf {
            let path = self.child(name);
            drop(create_new_file(&path));
            path
        }

        /// Creates a file inside this directory with create-new semantics and
        /// writes `contents` into it, returning its path.
        ///
        /// # Panics
        ///
        /// If the file already exists, cannot be created, or cannot be written.
        pub(crate) fn write_child(&self, name: &str, contents: &[u8]) -> PathBuf {
            use std::io::Write as _;
            let path = self.child(name);
            let mut file = create_new_file(&path);
            file.write_all(contents)
                .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
            file.flush()
                .unwrap_or_else(|e| panic!("flush {}: {e}", path.display()));
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // The recursion rests on `dir` not having existed before `new` created
            // it with create-new semantics, so the removal set starts from a path
            // this guard brought into existence rather than one it adopted, and on
            // Unix from one no other user could enter. Best effort on every route
            // out, including an unwinding one: failing to clean up must never mask
            // the failure that triggered the unwind.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A single named path inside its own private [`TempDir`].
    ///
    /// Dereferences to [`Path`], so it can be passed anywhere a `&Path` is
    /// expected. The file is *named* but not created, because the gzip entry points
    /// under test are themselves what must create it; [`Self::create`] materializes
    /// it exclusively for the tests that need it to pre-exist.
    pub(crate) struct TempFile {
        dir: TempDir,
        path: PathBuf,
    }

    impl TempFile {
        /// Names `payload.gz` inside a fresh private directory tagged with `tag`.
        ///
        /// # Panics
        ///
        /// See [`TempDir::new`].
        pub(crate) fn new(tag: &str) -> Self {
            Self::named(tag, "payload.gz")
        }

        /// Names `name` inside a fresh private directory tagged with `tag`.
        ///
        /// # Panics
        ///
        /// See [`TempDir::new`].
        pub(crate) fn named(tag: &str, name: &str) -> Self {
            let dir = TempDir::new(tag);
            let path = dir.child(name);
            Self { dir, path }
        }

        /// The named path.
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }

        /// Another path inside the *same* private directory, for tests needing two
        /// destinations.
        pub(crate) fn sibling(&self, name: &str) -> PathBuf {
            self.dir.child(name)
        }

        /// Materializes the file exclusively and returns its handle, for tests that
        /// need it to already exist.
        ///
        /// # Panics
        ///
        /// If it already exists or cannot be created.
        pub(crate) fn create(&self) -> File {
            create_new_file(&self.path)
        }

        /// Whether the file currently exists.
        pub(crate) fn exists(&self) -> bool {
            self.path.exists()
        }

        /// The bytes currently on disk, or empty when the file does not exist.
        pub(crate) fn bytes(&self) -> Vec<u8> {
            std::fs::read(&self.path).unwrap_or_default()
        }
    }

    impl core::ops::Deref for TempFile {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.path
        }
    }

    /// Lets a guard stand in for a path at the generic [`std::fs`] entry points.
    ///
    /// [`Deref`](core::ops::Deref) alone is not enough there: deref coercion fires
    /// when the parameter type is concretely `&Path`, but `std::fs::read`,
    /// `std::fs::write`, and `File::open` are generic over `AsRef<Path>`, and a
    /// generic bound is never satisfied by coercion. Implementing the trait keeps
    /// call sites reading as they did before hardening.
    impl AsRef<Path> for TempFile {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.dir
        }
    }
}

#[cfg(test)]
mod tests {
    //! Module-root smoke tests: verify the load-bearing shared constant and that
    //! the curated re-export façade resolves to the underlying submodule items.

    /// `GZBUFSIZE` must match the C `#define GZBUFSIZE 8192` byte-for-byte: the
    /// read/write buffer sizing (and hence the streaming/buffering behavior)
    /// depends on it.
    #[test]
    fn gzbufsize_matches_c_define() {
        assert_eq!(super::GZBUFSIZE, 8192);
    }

    /// The shared buffer allocator must *report* an unsatisfiable request rather
    /// than aborting, because that is the difference between reproducing C's
    /// `Z_MEM_ERROR` and killing the caller's process (AAP §0.6.5).
    ///
    /// The failing length is chosen so the rejection is a **capacity overflow**,
    /// decided by arithmetic before the allocator is consulted: `usize::MAX - 1`
    /// exceeds the `isize::MAX` byte ceiling every Rust allocation is bounded by,
    /// so the call returns in microseconds and no memory is ever requested. A
    /// test that instead asked for a merely enormous-but-representable size (a
    /// few GiB, say) would be non-deterministic — under Linux overcommit it can
    /// *succeed* — and could get the test runner OOM-killed rather than failed.
    #[test]
    fn alloc_zeroed_reports_failure_instead_of_aborting() {
        // Succeeds and is genuinely zero-filled at the requested length.
        let ok = super::alloc_zeroed(super::GZBUFSIZE).expect("a normal request succeeds");
        assert_eq!(ok.len(), super::GZBUFSIZE);
        assert!(ok.iter().all(|&b| b == 0), "the buffer must be zero-filled");

        // A zero-length request is legal and yields an empty buffer — the shape
        // `gz_init` produces for a `direct` stream's unused output buffer.
        assert_eq!(
            super::alloc_zeroed(0)
                .expect("a zero-length request succeeds")
                .len(),
            0
        );

        // Unsatisfiable by arithmetic, on 32-bit and 64-bit alike.
        assert!(
            super::alloc_zeroed(usize::MAX - 1).is_none(),
            "an unallocatable length must return None, not abort"
        );
    }

    /// Compile-time proof that the curated re-export façade resolves: importing
    /// every re-exported name fails to compile if any re-export path is wrong.
    /// The imports are intentionally unused at runtime, so the `unused_imports`
    /// lint is locally allowed.
    #[test]
    fn reexports_resolve() {
        #[allow(unused_imports)]
        use super::{
            GzMode, GzState, How, gzbuffer, gzclearerr, gzclose, gzclose_r, gzclose_w, gzdirect,
            gzdopen, gzeof, gzerror, gzflush, gzfread, gzfwrite, gzgetc, gzgetc_, gzgets, gzoffset,
            gzoffset64, gzopen, gzopen64, gzprintf, gzputc, gzputs, gzread, gzrewind, gzseek,
            gzseek64, gzsetparams, gztell, gztell64, gzungetc, gzvprintf, gzwrite,
        };

        // Also exercise the value-carrying items so the load-bearing `GzMode`
        // and `How` discriminants (mirrored verbatim from `gzguts.h`) are
        // asserted, not merely named.
        assert_eq!(GzMode::None as i32, 0);
        assert_eq!(GzMode::Append as i32, 1);
        assert_eq!(GzMode::Read as i32, 7247);
        assert_eq!(GzMode::Write as i32, 31153);
        assert_eq!(How::Look as u8, 0);
        assert_eq!(How::Copy as u8, 1);
        assert_eq!(How::Gzip as u8, 2);
    }

    /// [`safe_component`](super::test_temp::safe_component) must collapse every
    /// traversal and separator form to one harmless component.
    ///
    /// The first case is the shape that matters: interpolated raw,
    /// `slot/../../security_target` names a path two levels *above* the temporary
    /// directory. Sanitized, it can only ever name a child of it.
    #[test]
    fn safe_component_neutralizes_traversal_and_separators() {
        use super::test_temp::safe_component;
        use std::path::Path;

        // Exact outcomes for the shapes that matter, so a change in filtering
        // policy is visible and not merely "still safe".
        assert_eq!(
            safe_component("slot/../../security_target"),
            "slotsecurity_target"
        );
        assert_eq!(safe_component(".."), "x");
        assert_eq!(safe_component("../.."), "x");
        assert_eq!(safe_component("/etc/passwd"), "etcpasswd");
        assert_eq!(safe_component(r"..\..\windows"), "windows");
        assert_eq!(safe_component("a:b"), "ab");
        assert_eq!(safe_component("a\0b"), "ab");
        assert_eq!(safe_component("na\u{ef}ve"), "nave");
        // Ordinary values pass through untouched, `_` and `-` included.
        assert_eq!(safe_component("004"), "004");
        assert_eq!(safe_component("clone_index-7"), "clone_index-7");

        // The decisive property, asserted over every shape: joining the result to a
        // base descends exactly one level, so no input can escape the directory.
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
            assert!(
                !got.contains(".."),
                "{raw:?} yielded {got:?}, still traversing"
            );
            assert_eq!(
                Path::new("/tmp").join(&got).parent(),
                Some(Path::new("/tmp")),
                "{raw:?} yielded {got:?}, which does not stay one level below the base"
            );
        }
    }

    /// `safe_component` must be total and bounded: never empty, never longer than
    /// 32 characters, and always a single component whatever it is given.
    #[test]
    fn safe_component_is_total_and_bounded() {
        use super::test_temp::safe_component;
        use std::path::Path;

        for raw in [
            "",
            "...",
            "////",
            "\0\0",
            "🙂🙂🙂",
            &"z".repeat(500),
            "../../../../../../etc/shadow",
        ] {
            let out = safe_component(raw);
            assert!(!out.is_empty(), "must never be empty for {raw:?}");
            assert!(out.chars().count() <= 32, "must be bounded for {raw:?}");
            assert_eq!(
                Path::new(&out).components().count(),
                1,
                "must be exactly one component for {raw:?}"
            );
            assert!(
                out.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "must contain only safe characters for {raw:?}"
            );
        }
    }

    /// The private-directory guard must create exclusively, refuse to adopt an
    /// occupied name, and remove exactly what it created.
    ///
    /// The `AlreadyExists` assertion is the load-bearing one: it is the difference
    /// between skipping a planted symlink and following it. `create_dir_all` — the
    /// call this helper replaced across the gzip test surfaces — returns `Ok` here
    /// instead, silently adopting the tree.
    #[test]
    fn private_dir_creation_is_exclusive_and_cleanup_is_scoped() {
        use super::test_temp::{TempDir, create_private_dir};
        use std::io::ErrorKind;

        let recorded;
        {
            let dir = TempDir::new("modroot_exclusive");
            recorded = dir.path().to_path_buf();
            assert!(
                recorded.is_dir(),
                "the directory exists while the guard lives"
            );

            // Re-creating the very same name must be refused, not adopted.
            let again = create_private_dir(dir.path());
            let err = again.expect_err("an occupied name must not be adopted");
            assert_eq!(
                err.kind(),
                ErrorKind::AlreadyExists,
                "exclusive creation must report AlreadyExists"
            );

            // `create_dir_all`, by contrast, happily adopts it — which is exactly
            // the defect this helper exists to remove.
            assert!(
                std::fs::create_dir_all(dir.path()).is_ok(),
                "create_dir_all adopts an existing directory, so it cannot be used here"
            );

            // Children are created exclusively and a second attempt fails.
            let child = dir.create_child("first.gz");
            assert!(child.is_file());
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&child)
                    .is_err(),
                "create_new must refuse an existing child"
            );

            // Contents round-trip through the writing helper.
            let written = dir.write_child("payload.bin", b"hardened");
            assert_eq!(std::fs::read(&written).expect("read back"), b"hardened");

            // On Unix the mode is owner-only from `mkdir(2)` onwards.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(dir.path())
                    .expect("stat the private dir")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o700, "the directory must be owner-only");
            }
        }
        // Drop removed the directory and everything the test put inside it.
        assert!(
            !recorded.exists(),
            "the guard must remove the directory it created"
        );
    }

    /// Two guards must never collide, and the file guard must name a path inside
    /// its own directory without creating it until asked.
    #[test]
    fn temp_file_guards_are_distinct_and_create_on_demand() {
        use super::test_temp::TempFile;

        let a = TempFile::new("modroot_distinct");
        let b = TempFile::new("modroot_distinct");
        assert_ne!(a.path(), b.path(), "two guards must not share a path");
        assert_ne!(
            a.path().parent(),
            b.path().parent(),
            "each guard owns its own directory"
        );

        // Named but not yet created: the entry point under test is what creates it.
        assert!(!a.exists(), "the payload is named, not created");
        assert!(a.bytes().is_empty(), "a missing file reads as empty");

        drop(a.create());
        assert!(a.exists(), "create() materializes it exclusively");

        // A sibling lands in the same private directory.
        let sib = a.sibling("other.gz");
        assert_eq!(sib.parent(), a.path().parent());

        // `Deref<Target = Path>` lets a guard stand in for a `&Path`.
        let as_path: &std::path::Path = &a;
        assert_eq!(as_path, a.path());

        let recorded = (a.path().to_path_buf(), b.path().to_path_buf());
        drop(a);
        drop(b);
        assert!(!recorded.0.exists() && !recorded.1.exists());
    }
}
