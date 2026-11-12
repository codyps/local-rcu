//! An opt-in reader that makes repeated reads of an unchanged value nearly free, at the price
//! of holding old values back while it is idle.
//!
//! A plain [`Reader`] tells the writer about every read: it enters a read section (a store and
//! a `SeqCst` fence), loads the value, and leaves the section when the guard is dropped. A
//! [`CachedReader`] instead *stays* in its read section between reads and remembers the value
//! it loaded. While the writer has not published anything new, a read is a single load of the
//! shared pointer and a comparison: no store, no fence, and no guard to drop.
//!
//! ```
//! let (mut w, r) = local_rcu::slot(1usize);
//! let mut r = local_rcu::cached::CachedReader::new(r);
//!
//! assert_eq!(*r.read(), 1);
//! assert_eq!(*r.read(), 1); // just a load and a compare
//!
//! w.write(Box::new(2));
//! assert_eq!(*r.read(), 2); // notices the new value and moves to it
//! ```
//!
//! The reference returned by [`CachedReader::read()`] borrows the reader, so it cannot be kept
//! across the next read (which may let go of the value it points to):
//!
//! ```compile_fail
//! let (_w, r) = local_rcu::slot(1usize);
//! let mut r = local_rcu::cached::CachedReader::new(r);
//!
//! let first = r.read();
//! let second = r.read(); // error: `r` is still borrowed by `first`
//! assert_eq!(first, second);
//! ```
//!
//! # The trade-off
//!
//! Staying in a read section is exactly what a long-lived [`ReadGuard`](crate::ReadGuard)
//! does, with the same cost: **between two calls to [`CachedReader::read()`], the writer cannot
//! get back the value this reader last saw, nor any value published since.** A `CachedReader`
//! that reads often is harmless. One that stops reading holds on to an ever-growing list of old
//! values until it reads again, is [released](CachedReader::release) or is dropped. A plain,
//! idle [`Reader`] never holds anything back.
//!
//! So:
//!
//! - Call [`CachedReader::release()`] before going idle for a long time (or drop the reader).
//! - Prefer the plain [`Reader`] when old values must come back to the writer promptly, or
//!   when reads are rare compared to writes: after every write the next read pays the full
//!   price of leaving and re-entering a section.
//!
//! # How it works
//!
//! The reader is in one of two states:
//!
//! - *Released*: not in a read section, nothing cached. `read()` enters a section exactly like
//!   [`Reader::read()`] and caches the pointer it loaded.
//! - *Pinned*: in a read section, with the pointer loaded in that section cached. `read()`
//!   loads the shared pointer and compares it with the cached one. If they are equal the
//!   cached value is returned. Otherwise the reader leaves its section (which lets the writer
//!   collect the cached value) and enters a new one.
//!
//! Soundness needs nothing beyond what [`Reader`] already provides: a value loaded in a read
//! section stays valid until that section is left. In particular the comparison cannot be
//! fooled by a new value allocated at the address of the cached one, because the cached value
//! cannot be freed while this reader is pinned to it.
//!
//! The reader never switches values without leaving its section. Bumping the epoch while
//! staying in the section, or loading the new pointer without re-entering, would let the
//! writer track this reader with an out-of-date epoch snapshot and free a value still in use.

use super::{atomic, Reader};
use std::{fmt, ptr};

/// A [`Reader`] that stays in its read section between reads, so that reading a value that
/// has not changed costs one load and one comparison.
///
/// See the [module documentation](self) for the trade-off this makes.
///
/// Create one from a [`Reader`] with [`CachedReader::new()`] (or `into()`). Cloning gives a
/// new, released reader of the same slot.
pub struct CachedReader<T> {
    reader: Reader<T>,
    /// The value loaded in the read section we are in, or null when we are not in one.
    ///
    /// Invariant: non-null if and only if `reader` is in a read section (its epoch has the
    /// reader flag set), and then it is the pointer `Reader::enter()` returned for that
    /// section.
    cached: *const T,
}

// SAFETY: same reasoning as for `Reader`: this is a reference to a `T` shared with other
// threads. The raw pointer is only what makes these impls not automatic.
unsafe impl<T: Send + Sync> Send for CachedReader<T> {}
unsafe impl<T: Send + Sync> Sync for CachedReader<T> {}

impl<T> CachedReader<T> {
    /// Turn a reader into a cached reader. It starts out released: nothing is held back until
    /// the first [`read()`](CachedReader::read).
    pub fn new(reader: Reader<T>) -> Self {
        CachedReader {
            reader,
            cached: ptr::null(),
        }
    }

    /// Read the latest value.
    ///
    /// Any value whose `write()` completed before this call is visible here (or a newer one).
    ///
    /// The reference borrows the reader, so it has to be given up before the next `read()`.
    /// It stays valid for as long as it is held, like a [`ReadGuard`](crate::ReadGuard) does.
    ///
    /// After this returns, the reader holds back the returned value and everything published
    /// later, until the next `read()`, a [`release()`](CachedReader::release), or its drop.
    #[inline]
    pub fn read(&mut self) -> &T {
        if !self.cached.is_null() {
            // `Relaxed`: this pointer is only compared, never dereferenced. If it equals
            // `cached`, we return the pointer we loaded with `Acquire` when entering the
            // section. The writer's store to `active` cannot be missed for ever either: a
            // store that happens-before this call must be observed by this load.
            //
            // Equal pointers mean the same value: `cached` cannot have been freed (and its
            // address reused) while we are in the section that loaded it.
            let current = self.reader.shared.active.load(atomic::Ordering::Relaxed);
            if ptr::eq(current, self.cached) {
                // SAFETY: we are still in the read section that loaded `cached` (see the
                // invariant on the field), so the writer has not collected it. The reference
                // borrows `self` mutably, so nothing can leave that section while it lives.
                return unsafe { &*self.cached };
            }

            // Something new was published. Leave the section before entering another one,
            // rather than loading the new pointer in place: the writer may have tracked us
            // (with our current epoch) against the cached value, and both freeing that value
            // and protecting the next one rely on the usual store, fence, load sequence of
            // `Reader::enter()`.
            self.release();
        }

        self.cached = self.reader.enter();
        // SAFETY: `enter()` returns a pointer that stays valid until `leave()`, which only
        // `release()` calls, and that needs `&mut self`.
        unsafe { &*self.cached }
    }

    /// Leave the read section, letting the writer collect every value this reader was holding
    /// back. The next [`read()`](CachedReader::read) enters a new section.
    ///
    /// Call this before the reader goes idle for a long time. Does nothing if the reader is
    /// already released.
    pub fn release(&mut self) {
        if !self.cached.is_null() {
            // Clear the cache first so the invariant holds even if `leave()` were to panic.
            self.cached = ptr::null();
            self.reader.leave();
        }
    }

    /// Is this reader currently in a read section, holding old values back?
    pub fn is_pinned(&self) -> bool {
        !self.cached.is_null()
    }

    /// Turn this back into a plain [`Reader`], releasing it first.
    pub fn into_inner(mut self) -> Reader<T> {
        self.release();
        self.reader
    }
}

impl<T> From<Reader<T>> for CachedReader<T> {
    fn from(reader: Reader<T>) -> Self {
        CachedReader::new(reader)
    }
}

impl<T> Clone for CachedReader<T> {
    /// A new, released reader of the same slot.
    fn clone(&self) -> Self {
        CachedReader::new(self.reader.clone())
    }
}

impl<T> fmt::Debug for CachedReader<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedReader")
            .field("pinned", &self.is_pinned())
            .finish()
    }
}

// No `Drop` impl is needed: dropping the inner `Reader` marks its epoch dead, which ends the
// read section as far as the writer is concerned, and `&mut self` receivers guarantee no
// reference returned by `read()` is still alive.
