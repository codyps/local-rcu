//! SPMC (single producer, multiple consumer), latest value only, wait free, deferred collection of
//! old data.
//!
//! One (1) writer provides a singe value to multiple readers. Readers _can_ see different versions
//! of the datastructure simultaniously unless other synchronization is used.
//!
//! No atomic exchange, compare-and-swap, math operations are used, only loads &
//! stores (most relaxed, with an Acquire in the reader (per read) and a Release in the
//! writer (per write)).
//!  
//! - Reading wait free: only atomics are loads & stores. 1 atomic relaxed rmw
//!   of no-contention data (only 1 writer), 1 atomic Acquire load of shared data.
//! - Writing aquires an internal mutex to scan for old values to retire. Using
//!   `write_nosync()` (which does not collect old is wait free.
//! - Creating additional readers aquires an internal mutex & clones some `Arc`s.
//! - Returning previously written values is deferred. When using `write()`, old
//!   values are automatically examined to determine if they may still be in use
//!   by a reader. If they are definitely not in use by a reader, the old values
//!   are returned.
//!
#[cfg(loom)]
use loom::{
    sync::{atomic, Arc, Mutex},
    thread,
};
use std::ops::Deref;
#[cfg(not(loom))]
use std::{
    sync::{atomic, Arc, Mutex},
    thread,
};

/// Create a new SPMC slot containing an initial value `init_val`
pub fn slot<T>(init_val: T) -> (Writer<T>, Reader<T>) {
    let w = Writer::new(Box::new(init_val));
    let r = w.reader();
    (w, r)
}

/// Writer for a slot. Can also read the value, and create more readers
///
/// Only 1 of these per slot exists. If multiple writers are needed, wrap this
/// in a mutex.
pub struct Writer<T> {
    shared: Arc<Shared<T>>,
    // we keep these as pointers because they may still be in use, and so
    // freeing them (as would happen on drop by default) would be undefined behavior
    #[allow(clippy::type_complexity)]
    prevs: Vec<(*mut T, Vec<(usize, Arc<atomic::AtomicUsize>)>)>,
}

unsafe impl<T: Send> Send for Writer<T> {}

impl<T> Drop for Writer<T> {
    /// WARNING: this will spin until all readers have dropped their `ReadGuard`s to avoid leaking.
    // Consider if we should provide a way to avoid this (and defer the collection of old values).
    fn drop(&mut self) {
        drop(self.sync());
    }
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

    /// Obtain a reader for the value stored by this writer
    pub fn reader(&self) -> Reader<T> {
        Reader::<T>::new(self.shared.clone())
    }

    /// Write a new value, returning any old values that are no longer in use
    ///
    /// You may get none of the old values back as readers may still exist. The next time you write
    /// (or call `try_sync()`), additional previous values are returned. Old values may be returned
    /// in any order.
    pub fn write(&mut self, val: Box<T>) -> Vec<Box<T>> {
        // scan `self.prev` for things we can discard and discard them.
        let mut r = self.try_sync();

        self.write_nosync(val);

        r.extend(self.try_sync());

        r
    }

    /// Read the current value in this writer.
    ///
    /// This uses a `Relaxed` load, no locking or stricter atomics are required.
    pub fn read(&self) -> &T {
        // Only we can update the value, so `Relaxed` is fine
        // SAFETY:
        // We're the only writer, and a `&mut self` is the only way to replace
        // this with a new value. As a result, the pointer can't become invalid
        // because we've bound its lifetime to `&self`.
        // There are no mutable references, because we only hand out read-only
        // refs to the readers.
        unsafe { &*self.shared.active.load(atomic::Ordering::Relaxed) }
    }

    /// Are there any old values waiting to be collected?
    ///
    /// These may or may not still have readers that are still using them. If the readers for a
    /// particular value have moved on, those old values will be returned by `try_sync()`.
    pub fn has_old_values(&self) -> bool {
        !self.prevs.is_empty()
    }

    /// Check if we can release previous values and return them
    ///
    /// Does not aquire any locks. Returns after a single scan.
    ///
    /// If you want to wait for all readers to finish proactively, schedule work using a timer to
    /// call this periodically. This is generally not required unless you need to obtain old values
    /// for some special purpose.
    pub fn try_sync(&mut self) -> Vec<Box<T>> {
        let mut v = Vec::new();

        self.prevs.retain_mut(|(ptr, epochs)| {
            epochs.retain(|(prev, epoch)| {
                let new = epoch.load(atomic::Ordering::Relaxed);
                println!("new: {:08b}, prev: {:08b}", new, prev);
                new == *prev
            });

            if epochs.is_empty() {
                // TODO: consider if we require a fence here to ensure all reads
                // have occured before this point.

                // SAFETY: no readers are left (because all have moved to a new
                // epoch). We're removing it from `self.prevs` too, so there
                // won't be another `Box` created for this pointer.
                v.push(unsafe { Box::from_raw(*ptr) });
                false
            } else {
                true
            }
        });

        v
    }

    /// `try_sync()` repeatedly until all old values are collected
    ///
    /// This spins, and in general should be avoided.
    pub fn sync(&mut self) -> Vec<Box<T>> {
        let mut r = Vec::new();

        while !self.prevs.is_empty() {
            let v = self.try_sync();
            r.extend(v);

            // TODO: consider if we should skip yielding a bit initially
            thread::yield_now();
        }

        r
    }

    /// Write a new value, without checking if any old values are no longer in use
    ///
    /// If you use this, calling `try_sync()` is required to avoid leaking old values. In general,
    /// `Writer::write()` is a better choice.
    pub fn write_nosync(&mut self, val: Box<T>) {
        // We're the only writer, so `Relaxed` is fine. We avoid a `swap`
        // because that provides extra garuntees we don't need.
        let prev = self.shared.active.load(atomic::Ordering::Relaxed);

        // Half of a Release-Acquire pair, see `Reader::read()` for the `Acquire` half. `Release`
        // ensures that `val` is fully initialized before it is exposed to other threads.
        self.shared
            .active
            .store(Box::into_raw(val), atomic::Ordering::Release);
        // Can be `Release` if the `SeqCst` fence is placed before the epoch
        // iter below (after epochs.lock())
        atomic::fence(atomic::Ordering::SeqCst);

        // add `prev` to `self.prevs`, collect initial remaining readers, and see if we can retire
        // it.

        // NOTE: see if we can predict an initial slab size better than 0
        //
        // NOTE: `Vec` gives us `retain_mut` for collecting these at the end. If
        // `Slab` had retain_mut we could use it instead if it provides better perf.
        let mut remaining_readers = Vec::new();

        // initial scan, locks epochs
        {
            let epochs = self.shared.epochs.lock().unwrap();
            // FIXME: the `epochs.lock()` should already be doing this. Check `loom`.
            // FIXME: determine why anything less than `SeqCst` here causes loom to fail.
            for (_, epoch) in epochs.iter() {
                // This pairs with a `Release` in `Reader::read()`, which ensures all the
                // writes/reads by the reader are retired. We don't need to see the writes done
                // by the caller of `Reader::read()`, so `Relaxed` is sufficient (`Acquire` would
                // ensure we see writes).
                let v = epoch.load(atomic::Ordering::Relaxed);
                if v & 1 != 0 {
                    remaining_readers.push((v, epoch.clone()));
                }
            }
        }

        self.prevs.push((prev, remaining_readers));
    }
}

/// Something which can read the value, use `[Writer::reader]` to get one
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    epoch: Arc<atomic::AtomicUsize>,
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

    /// Read the value
    ///
    /// To avoid leaking values, the return value of this function must be
    /// dropped.
    ///
    /// This function is conceptually an `srcu_read_lock()` and a
    /// `srcu_dereference()`. The `drop` of the return value (`ReadGuard`) is
    /// conceptually a `srcu_read_unlock()`.
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

        // NOTE: we use `(v + 1)|1` instead of `+1` here in case the previous
        // `ReadGuard` was leaked. If the previous `ReadGuard` was leaked, we'll
        // still have an epoch with the low bit set, and we want to keep that
        // low bit set.
        //
        // In the previous leak case, the writer will consider 0 or more
        // previous values in use until the `epoch` changes. If we did just a
        // `|1` we'd only get the epoch to change (still in the leak case) when
        // we drop this new `ReadGuard`. The `+ 1` part ensures we always move
        // to a new epoch.
        //
        // We need this to be visible to the writer before we read `active`.
        // `SeqCst` should ensure that, but perhaps a fence or weaker ordering
        // could be sufficient.
        self.epoch.store(v | 1, atomic::Ordering::Relaxed);
        atomic::fence(atomic::Ordering::SeqCst);

        // Pairs with a `Release` in `Writer::write()`, which ensures that we see all the writes
        // writer makes to things we load via `data`.
        //
        // Note: `Consume` is good enough for this operation (ie: wrt `active` we only need to
        // ensure that loads via it have a data dependency on other writes), but we need the
        // `self.epoch` change to be visible to the writer, so we use `Acquire` here.
        let data = self.shared.active.load(atomic::Ordering::Acquire);

        ReadGuard {
            reader: self,
            // SAFETY: we've told the writer (via the epoch) that we're reading
            // a value (by setting the low bit of the epoch), so it won't delete
            // the value until we update our epoch. We only update our epoch
            // when this `ReadGuard` is dropped. No `&mut`s are handed out to
            // the data while it's in the `active` slot.
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
/// If this is leaked, the value it points to will also leak.
pub struct ReadGuard<'a, T> {
    reader: &'a mut Reader<T>,
    data: &'a T,
}

impl<'a, T> Deref for ReadGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<'a, T> Drop for ReadGuard<'a, T> {
    fn drop(&mut self) {
        // NOTE: this split operation is ok because we are the only writer (others read this value).
        // This is split into 2 operations so that better code can be generated (ie: omitting CAS
        // on archs without atomic add opcodes).
        let v = self.reader.epoch.load(atomic::Ordering::Relaxed);
        self.reader.epoch.store(v + 1, atomic::Ordering::Release);
        // NOTE: adding a fence(SeqCst) here speeds up loom significantly,
        // implying not having it opens up many more execution variants. This
        // implies:
        // - omitting the fence may be useful for perf
        // - omitting the fence opens up lots of ways for our code to be wrong.
    }
}
