//! `left-right` style double-buffering on top of [`local-rcu`].
//!
//! The [`local-rcu`] README describes how to get `left-right` behavior by
//! calling [`local_rcu::Writer::sync()`] before every
//! [`local_rcu::Writer::write()`], absorbing operations into the returned
//! value. This crate packages that recipe:
//!
//! * [`WriteHandle::append()`] absorbs an operation into the non-published
//!   copy (the "alternate") and records it in the operation log.
//! * [`WriteHandle::publish()`] publishes the alternate, blocking until
//!   readers have vacated the previously published copy (as `left-right`'s
//!   `publish()` does).
//! * [`ReadHandle::enter()`] takes a wait-free read of the published copy,
//!   returning `None` once the writer is dropped (like `left-right`'s
//!   `enter()`).
//!
//! For comparison, [`WriteHandle::publish_relaxed()`] drops the left-right
//! blocking discipline entirely: it publishes a clone of the working copy and
//! never waits for readers (native `local-rcu` behavior — memory is bounded
//! by what in-flight readers pin, at the cost of a `T: Clone` per publish).
//!
//! # Differences from the [`left-right`] crate
//!
//! * Reads are wait-free `local-rcu` reads (epoch store + acquire load);
//!   `left-right` reads are also wait-free (epoch bump + pointer load).
//! * As in `left-right`, every operation is applied twice — once when
//!   appended, and again when the stale copy is taken back and "rebased" —
//!   so operations must be deterministic.
//! * Writer drop is signaled to readers (`enter()` returning `None`), as in
//!   `left-right`. As with any closure, a reader may observe closure before
//!   observing the final published operations; unlike `left-right`, the
//!   backing data stays alive until readers drop it.
//! * `left-right` hides the operation log and copy management entirely; this
//!   crate is a thin layer so the moving parts stay visible.
//! * This crate is **not published**; it mainly exists to benchmark the
//!   recipe against `left-right` itself (see `benches/vs_left_right.rs`).
//!
//! # Example
//!
//! ```
//! use local_rcu_left_right::{Absorb, WriteHandle};
//!
//! #[derive(Clone)]
//! struct AddOp(i64);
//!
//! impl Absorb<AddOp> for i64 {
//!     fn absorb(&mut self, op: AddOp) {
//!         *self = self.wrapping_add(op.0);
//!     }
//! }
//!
//! let (mut w, mut r) = WriteHandle::<i64, AddOp>::new();
//! assert_eq!(*r.enter().unwrap(), 0);
//! w.append(AddOp(1));
//! // not published yet:
//! assert_eq!(*r.enter().unwrap(), 0);
//! w.publish();
//! assert_eq!(*r.enter().unwrap(), 1);
//! drop(w);
//! // the writer is gone: no more values will be published
//! assert!(r.enter().is_none());
//! ```
//!
//! [`left-right`]: https://crates.io/crates/left-right

use local_rcu::closing::{ClosingReader, ClosingWriter};
use local_rcu::ReadGuard;
use std::marker::PhantomData;

/// Apply an operation to one copy of the data structure.
///
/// Operations must be deterministic: each is applied twice — once to the
/// copy being written when [`WriteHandle::append`] is called, and again to
/// the stale copy when it is taken back (rebased).
pub trait Absorb<O> {
    /// Apply `op` to this copy.
    fn absorb(&mut self, op: O);
}

/// A clonable handle for reading the published value (like
/// `left_right::ReadHandle`).
pub struct ReadHandle<T> {
    reader: ClosingReader<T>,
}

impl<T> Clone for ReadHandle<T> {
    fn clone(&self) -> Self {
        ReadHandle {
            reader: self.reader.clone(),
        }
    }
}

impl<T> ReadHandle<T> {
    /// Wait-free read of the currently published value, or `None` if the
    /// writer has been dropped (like `left_right::ReadHandle::enter`).
    pub fn enter(&mut self) -> Option<ReadGuard<'_, T>> {
        self.reader.read()
    }

    /// Has the writer been dropped?
    pub fn is_closed(&self) -> bool {
        self.reader.is_closed()
    }
}

/// The (single) writer handle (like `left_right::WriteHandle`).
///
/// Dropping it signals to readers that no further values will be published
/// (as dropping a `left_right::WriteHandle` does).
pub struct WriteHandle<T, O> {
    writer: ClosingWriter<T>,
    /// The non-published copy that operations are absorbed into. `None` when
    /// the previously published copy has not been handed back yet.
    alternate: Option<Box<T>>,
    /// Operations applied to the alternate that the copy which will come
    /// back next does not yet have (the "operation log"). Cleared as those
    /// ops are absorbed into a taken-back (stale) copy.
    log: Vec<O>,
    /// Set when `alternate` is a copy that came back from the writer and
    /// still needs the operation log applied to it.
    stale: bool,
    _op: PhantomData<fn() -> O>,
}

impl<T: Default, O> WriteHandle<T, O> {
    /// Create a pair of handles over `T::default()`, mirroring
    /// `left_right::new()`.
    pub fn new() -> (WriteHandle<T, O>, ReadHandle<T>) {
        let (writer, reader) = local_rcu::closing::slot(T::default());
        (
            WriteHandle {
                writer,
                alternate: Some(Box::new(T::default())),
                log: Vec::new(),
                stale: false,
                _op: PhantomData,
            },
            ReadHandle { reader },
        )
    }
}

impl<T, O: Clone> WriteHandle<T, O>
where
    T: Absorb<O>,
{
    /// Ensure the alternate is present and has every appended operation
    /// applied, blocking (via [`local_rcu::Writer::sync`]) until the
    /// previously published copy is handed back if it hasn't been yet.
    fn ready(&mut self) {
        if self.alternate.is_none() {
            let mut reclaimed = None;
            for old in self.writer.sync() {
                debug_assert!(reclaimed.is_none());
                reclaimed = Some(old);
            }
            // `sync()` spins until the writer's old values are collected, so
            // the previously published copy is always handed back here.
            self.alternate = Some(reclaimed.expect("`sync()` returns the previous value"));
            self.stale = true;
        }

        if self.stale {
            // The alternate came back from the writer: it is a snapshot from
            // one `publish()` ago. Bring it up to date by re-applying the
            // operations that were absorbed into the copy published in its
            // place — `left-right`'s oplog replay.
            let log = std::mem::take(&mut self.log);
            if let Some(alt) = self.alternate.as_deref_mut() {
                for op in log {
                    alt.absorb(op);
                }
            }
            self.stale = false;
        }
    }

    /// Append an operation to the log, absorbing it into the non-published
    /// copy. It is not visible to readers until [`publish`](Self::publish).
    pub fn append(&mut self, op: O) {
        self.ready();
        if let Some(alt) = self.alternate.as_deref_mut() {
            alt.absorb(op.clone());
        }
        self.log.push(op);
    }

    /// Publish the alternate copy, making all appended operations visible to
    /// readers.
    ///
    /// Blocks until readers have vacated the previously published copy, as
    /// `left_right::WriteHandle::publish` does.
    pub fn publish(&mut self) {
        self.ready();
        let alt = self.alternate.take().expect("ready() leaves an alternate");
        for old in self.writer.write(alt) {
            debug_assert!(self.alternate.is_none());
            self.alternate = Some(old);
            // the taken-back copy is a snapshot from one publish ago
            self.stale = true;
        }
    }

    /// Publish without ever blocking for readers.
    ///
    /// Instead of taking the working copy back from the writer (which waits
    /// for readers to vacate it, as [`publish`](Self::publish) and
    /// `left-right` do), this publishes a clone of the working copy and keeps
    /// the working copy. Old snapshots that readers still examine are simply
    /// retained by the writer (as in plain `local-rcu`): memory is bounded by
    /// the values in-flight readers pin, rather than by blocking the writer.
    ///
    /// The trade: a `T: Clone` per publish, and — for large `T` — the clone
    /// cost is proportional to the size of the value.
    pub fn publish_relaxed(&mut self)
    where
        T: Clone,
    {
        // In consistent relaxed use the working copy is always in hand, so
        // this never blocks. After a blocking `publish()` handed the working
        // copy to the writer, this retrieves it and may block once — prefer
        // using one publish flavor consistently.
        self.ready();

        let snapshot = self
            .alternate
            .as_deref()
            .expect("ready() leaves an alternate")
            .clone();
        // `Writer::write()` never blocks: it hands back only values that no
        // reader uses anymore, which we drop (the working copy is canonical).
        drop(self.writer.write(Box::new(snapshot)));

        // The published snapshot has every operation; nothing to rebase later.
        self.log.clear();
    }

    /// Read the currently published value from the writer side (no lock, no
    /// epoch bookkeeping: only the writer replaces the value).
    pub fn read(&self) -> &T {
        self.writer.read()
    }
}
