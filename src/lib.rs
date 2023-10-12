/// SPMC (single producer, multiple consumer), latest value only, wait free, deferred collection of
/// old data.
///
/// One (1) writer provides a singe value to multiple readers. Readers _can_ see different versions
/// of the datastructure simultaniously unless other synchronization is used.
///
/// - Reading is optimized and wait free: 1 atomic relaxed rmw of no-contention data (only 1
///   writer), 1 atomic Acquire load of shared data.
/// - Writing is wait free (various relaxed atomic loads, 1 atomic Release store of shared data)
/// - Freeing previously sent data is deferred.
///
/// Alternatives:
///
///  - `triple_buffer`: requires atomic swaps. Not wait free. Allows immediate data free/reuse.
///  - `std::sync::mpsc::channel`: MPSC (vs SPMC), multiple values (not just latest) are readable
///  - `tokio::sync::watch`: ...
///  - `left-right`: ...
///  - `tokio::sync::broadcast`: ...
///
use std::ops::Deref;
use std::sync::{atomic, Arc, Mutex};

// Ideas:
//
// - make `sync_top()` and `sync_bottom()` public safely (hard because we have a single `prev` list
//   we want to retire all at once.
// - abandon the "all at once" retire of `prev`, track epochs of readers for each `prev` so we've
//   got a better chance of retiring _some_ of them. ie: this would garuntee that we make forward
//   progress even if we never call `sync(SyncKind::Strong)`. This means more overhead, and it's
//   not clear we can do both the "all at once" and "at least the old ones" in the same data
//   structure.
// - split out the garbage collection into some pluggable op that can defer it if desired (and
//   allow us to wait in a different thread, on a async task, do either the "all at once" or "more
//   exact" waiting, etc.

pub struct Writer<T> {
    shared: Arc<Shared<T>>,
    // we keep these as pointers because they may still be in use, and so freeing them would be
    // undefined behavior
    prevs: Vec<(*mut T, slab::Slab<(usize, Arc<atomic::AtomicUsize>)>)>,
}

impl<T> Drop for Writer<T> {
    fn drop(&mut self) {
        drop(self.sync(SyncKind::Strong));
    }
}

/// What kind of sync to perform
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncKind {
    /// Don't yield at all, exit as soon as we've scanned for any readers that are still reading.
    NoYield,
    /// Yield once, exit as soon as we've scanned for any readers that are still reading the same
    /// thing.
    Weak,
    ///
    Strong,
}

struct Shared<T> {
    // to a Box<Value<T>>
    active: atomic::AtomicPtr<T>,
    epochs: Mutex<slab::Slab<Arc<atomic::AtomicUsize>>>,
}

impl<T> Writer<T> {
    pub fn new(init_val: Box<T>) -> Writer<T> {
        let shared = Arc::new(Shared {
            active: atomic::AtomicPtr::new(Box::into_raw(init_val)),
            epochs: Mutex::new(slab::Slab::new()),
        });

        Writer {
            shared,
            prevs: Vec::new(),
        }
    }

    pub fn reader(&self) -> Reader<T> {
        Reader::<T>::new(self.shared.clone())
    }

    pub fn read(&self) -> &T {
        // Only we can update the value, so `Relaxed` is fine
        unsafe { &*self.shared.active.load(atomic::Ordering::Relaxed) }
    }

    /// Perform a `sync` and a `write` in one step. This is what you want unless you're doing
    /// explicit `sync` calls.
    ///
    /// If you don't want to re-use old data, and just want things to work without leaking memory,
    /// use this.
    pub fn sync_write(&mut self, val: Box<T>) {
        let remaining_readers = self.sync_top();

        // We're the only writer, so `Relaxed` is fine
        let prev = self.shared.active.load(atomic::Ordering::Relaxed);

        // Half of a Release-Consume pair, see `Reader::read()` for the `Consume` half. This
        // ensures that `val` is fully initialized before we update the pointer.
        self.shared
            .active
            .store(Box::into_raw(val), atomic::Ordering::Release);

        // NOTE: we're being careful here because we can't retire `prev` in this sync because
        // between `sync_top()` and `active.store()`, readers might have begun a new read-cycle on
        // `prev`.
        if self.sync_bottom(remaining_readers, SyncKind::Weak) {
            let prevs = self.prevs.take();
            // safe because `*mut T` and `Box<T>` have the same layout
            drop(unsafe { std::mem::transmute(prev) })
        }

        self.prevs.push(prev);

        // NOTE: we could scan here to see if we're done reading again.
    }

    /// Perform a `write` without any `sync` calls, will consume memory unless `sync()` is called
    /// at some point.
    ///
    /// This is useful for cases where you're collecting previous values for some re-use via
    /// explict
    pub fn write(&mut self, val: Box<T>) {
        // We're the only writer, so `Relaxed` is fine
        let prev = self.shared.active.load(atomic::Ordering::Relaxed);

        // Half of a Release-Consume pair, see `Reader::read()` for the `Consume` half. This
        // ensures that `val` is fully initialized before we update the pointer.
        self.shared
            .active
            .store(Box::into_raw(val), atomic::Ordering::Release);
        self.prevs.push(prev);
    }

    fn sync_top(&mut self) -> slab::Slab<(usize, Arc<atomic::AtomicUsize>)> {
        // Nothing out there that the readers might hold onto
        if self.prevs.is_empty() {
            return slab::Slab::new();
        }

        // NOTE: 0 is a reasonable estimate for the length given we do this at the start of the
        // next write (instead of immediately after swapping)
        //
        // NOTE: we're using `slab` here just so we have faster removal. We don't need it's other
        // features.
        // TODO: check that `slab` has faster removal than `Vec` when using `retain()`.
        let mut remaining_readers = slab::Slab::new();

        // initial scan, locks epochs
        {
            let epochs = self.shared.epochs.lock();
            for (_, epoch) in &epochs {
                // This pairs with a `Release` in `Reader::read()`, which ensures all the
                // writes/reads by the reader are retired. We don't need to see the writes done
                // by the caller of `Reader::read()`, so `Relaxed` is sufficient (`Acquire` would
                // ensure we see writes).
                let v = epoch.load(atomic::Ordering::Relaxed);
                if v & 1 != 0 {
                    remaining_readers.insert((v, epoch.clone()));
                }
            }
        }

        remaining_readers
    }

    fn sync_bottom(
        &mut self,
        remaining_readers: slab::Slab<(usize, Arc<atomic::AtomicUsize>)>,
        sync_kind: SyncKind,
    ) -> bool {
        if remaining_readers.is_empty() {
            return false;
        }

        // wait for the value in each remaining reader's epoch to be different
        loop {
            std::thread::yield_now();

            remaining_readers.retain(|v, epoch| {
                // see comment in the initial scan for memory ordering details
                let new_v = epoch.load(atomic::Ordering::Relaxed);
                new_v == v
            });

            if remaining_readers.is_empty() {
                return true;
            }

            if sync_kind == SyncKind::Weak {
                return false;
            }
        }
    }

    /// Wait for all readers to finish
    ///
    /// This is done internally by `write()` as needed, and exposed here only if you really want to
    /// save memory by freeing the old data before the next `write()`.
    ///
    /// This will spin until all readers have finished reading the current value, with only
    /// `yield_now()` calls to allow other threads to run.
    ///
    /// If `weak`, don't block indefinitely, instead return after one `yield_now()`
    pub fn sync(&mut self, sync_kind: SyncKind) -> Vec<Box<T>> {
        // Nothing out there that the readers might hold onto
        if self.prevs.is_empty() {
            return Vec::new();
        }

        let remaining_readers = self.sync_top();

        if remaining_readers.is_empty() {
            return Vec::new();
        }

        if sync_kind == SyncKind::NoYield {
            return Vec::new();
        }

        if !self.sync_bottom(remaining_readers, sync_kind) {
            return Vec::new();
        }

        let prevs = self.prevs.take();

        // safe because `*mut T` and `Box<T>` have the same layout
        unsafe { std::mem::transmute(prevs) }
    }
}

/// Something which can read the value, use `[Writer::reader]` to get one
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    epoch: Arc<AtomicUsize>,
    epoch_index: usize,
}

impl<T> Clone for Reader<T> {
    fn clone(&self) -> Reader<T> {
        Reader::<T>::new(self.shared.clone())
    }
}

impl<T> Reader<T> {
    fn new(shared: Arc<Shared<T>>) -> Reader<T> {
        let epoch = Arc::new(atomic::AtomicUsize::new(0));
        let epoch_index = shared.epochs.lock().unwrap().insert(epoch.clone());

        Reader {
            shared,
            epoch,
            epoch_index,
        }
    }

    /// Read the value, avoid leaking `ReadGuard` and drop `ReadGuard` to as soon as possible.
    ///
    /// Leaking `ReadGuard` will cause the `Writer::sync()` (and thus `Writer::write()`) to stall
    /// forever. `Writer::sync()` (and thus `Writer::write()`) will stall until `ReadGuard` is
    /// dropped.
    pub fn read(&mut self) -> ReadGuard<T> {
        // We're using `Relaxed` because all the ordering needed comes from the `Acquire` on
        // `self.shared.active` below.
        //
        // Note: we split this `add` up because we don't need the consistency `add` provides (we're
        // the only writer).
        //
        // TODO: check that compilers emit better code on various archs for this split version vs a
        // merged `add` op.
        let v = self.epoch.load(atomic::Ordering::Relaxed);
        self.epoch.store(v | 1, atomic::Ordering::Relaxed);

        // Pairs with a `Release` in `Writer::write()`, which ensures that we see all the writes
        // writer makes to things we load via `data`.
        //
        // Note: `Consume` is good enough for this operation (ie: wrt `active` we only need to
        // ensure that loads via it have a data dependency on other writes), but we need the
        // `self.epoch` change to be visible to the writer, so we use `Acquire` here.
        let data = self.shared.active.load(atomic::Ordering::Acquire);

        ReadGuard {
            reader: self,
            data: unsafe { &*data },
        }
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        self.shared.epochs.lock().unwrap().remove(self.epoch_index);
    }
}

/// Allows access to the underlying value
///
/// If this is leaked, then the Writer will stall forever.
pub struct ReadGuard<'a, T> {
    reader: &'a mut Reader<T>,
    data: &'a T,
}

impl<'a, T> Deref for ReadGuard<'a, T> {
    fn deref(&self) -> &T {
        self.data
    }
}

impl<'a, T> Drop for ReadGuard<'a, T> {
    fn drop(&mut self) {
        // NOTE: we're the only writer, so we could split this into a load + store if that
        // generates better code.
        self.reader.epoch.fetch_add(1, atomic::Ordering::Release);
    }
}
