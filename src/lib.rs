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
#[cfg(loom)]
use loom::{
    sync::{atomic, Arc, Mutex},
    thread,
};
use std::{cell::UnsafeCell, marker::PhantomData, ops::Deref};
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
}

// If `T` is not Sync, we can't allow Writer (or Reader) to be sent to another thread, as `Writer`
// & `Reader` are essentially references.
unsafe impl<T: Send + Sync> Send for Writer<T> {}
unsafe impl<T: Send + Sync> Sync for Writer<T> {}

struct Shared<T> {
    /// Value that readers are expected to read at this time.
    ///
    /// Is really a `Box<T>`, we need `AtomicPtr` so we can load/store it.
    active: atomic::AtomicPtr<T>,

    /// An array of epochs, one per reader.
    ///
    /// references to each reader's epoch exist:
    ///  - one here
    ///  - one in the `Reader` itself
    ///  - zero or more in `prevs`
    ///
    /// Readers track their index in the slab and use it to remove themselves when they're dropped.
    ///
    /// This is locked when a new reader is created, and when a writer is writing a new value.
    /// Contention is limited as long as we don't create readers too often and/or don't write new
    /// values too often.
    epochs: Mutex<slab::Slab<Arc<atomic::AtomicUsize>>>,

    /// Previous active values along with a vec of readers, each with a snapshot of the epoch at
    /// the time _after_ the previous active value was made inactive and a reference to the
    /// reader's epoch counter so we can determine what epoch that reader is at now.
    // Conceptually, this is a field in `Writer`. We place it in `Shared` to avoid having Writer
    // dropping spin. Because `Shared` is in an `Arc`, by the time drop occurs all `Reader`s will
    // have released their `ReadGuard`s, and we can safely drop the `Vec`.
    //
    // Modifying the content of the `Box<T>` is not permitted until all readers have dropped their
    // references to the `Box<T>`. This is enforced by the `ReadGuard`'s lifetime & the epoch
    // count.
    #[allow(clippy::type_complexity)]
    prevs: UnsafeCell<Vec<(Box<T>, Vec<(usize, Arc<atomic::AtomicUsize>)>)>>,
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        // SAFETY: no other references to `self` can exist at this point, if we've gotten this far
        // all the refs to `self.active` have been dropped because the refcount on the inner Arc
        // has dropped.
        drop(unsafe { Box::from_raw(self.active.load(atomic::Ordering::Relaxed)) })
    }
}

impl<T> Writer<T> {
    fn prevs(&self) -> &Vec<(Box<T>, Vec<(usize, Arc<atomic::AtomicUsize>)>)> {
        // SAFETY: only this `Writer` can access `prevs`.
        unsafe { &*self.shared.prevs.get() }
    }

    fn prevs_mut(&mut self) -> &mut Vec<(Box<T>, Vec<(usize, Arc<atomic::AtomicUsize>)>)> {
        // SAFETY: only this `Writer` can access `prevs`.
        unsafe { &mut *self.shared.prevs.get() }
    }

    /// Create a new `Writer` with an initial value
    ///
    /// The `Writer` can than be used to obtain one or more [`Reader`]s.
    pub fn new(init_val: Box<T>) -> Writer<T> {
        let shared = Arc::new(Shared {
            active: atomic::AtomicPtr::new(Box::into_raw(init_val)),
            epochs: Mutex::new(slab::Slab::new()),
            prevs: UnsafeCell::new(Vec::new()),
        });

        Writer { shared }
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
        !self.prevs().is_empty()
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

        // We need to move `val` out of `prevs` and into `v`. `extract_if` would work.
        // `retain_mut` doesn't unless we play some unsafe games with pointers in `prev`.
        //
        // FIXME: switch to `extract_if` once it's stable.

        let mut i = 0;
        while i < self.prevs().len() {
            let epochs = &mut self.prevs_mut()[i].1;
            epochs.retain(|(prev, epoch)| {
                let new = epoch.load(atomic::Ordering::Relaxed);
                new == *prev
            });

            if epochs.is_empty() {
                // TODO: consider if we require a fence here to ensure all reads
                // have occured before this point.

                // SAFETY: no readers are left (because all have moved to a new
                // epoch). We're removing it from `self.prevs` too, so there
                // won't be another `Box` created for this pointer.
                v.push(self.prevs_mut().remove(i).0);
            } else {
                i += 1;
            }
        }

        v
    }

    /// `try_sync()` repeatedly until all old values are collected
    ///
    /// This spins, and in general should be avoided.
    pub fn sync(&mut self) -> Vec<Box<T>> {
        let mut r = Vec::new();

        while !self.prevs().is_empty() {
            let v = self.try_sync();
            if v.is_empty() {
                // TODO: consider if there's a better way to choose how to yield
                thread::yield_now();
            } else {
                r.extend(v);
            }
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

        self.prevs_mut()
            .push((unsafe { Box::from_raw(prev) }, remaining_readers));
    }
}

/// Something which can read the value, use `[Writer::reader]` to get one
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    epoch: Arc<atomic::AtomicUsize>,
    epoch_index: usize,
    // pointer used so we get !Send/!Sync without the `unsafe impl`s below.
    _marker: PhantomData<*const T>,
}

// SAFETY: if `T` is not `Sync` (ie: if it is a RefCell or has other non-thread safe mutability),
// we can't send it between threads because we can't ensure that the reader won't mutate it.
unsafe impl<T: Send + Sync> Send for Reader<T> {}
unsafe impl<T: Send + Sync> Sync for Reader<T> {}

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
            _marker: PhantomData,
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
        assert!(v & 1 == 0);

        // NOTE: we can't call `read()` a second time if we leak the previous
        // `ReadGuard`, so we can assume well behaved values.
        self.epoch.store(v | 1, atomic::Ordering::Relaxed);

        // Ensure `epoch` store is visible in other threads before we read
        // `active` (so we don't get a garbage pointer)
        // TODO: determine why AquRel isn't enough here
        atomic::fence(atomic::Ordering::SeqCst);

        // Pairs with a `Release` in `Writer::write()`, which ensures that we
        // see all the writes writer makes to things we load via `data`.
        //
        // Note: `Consume` is good enough for this operation (ie: wrt `active` we only need to
        // ensure that loads via it have a data dependency on other writes).
        // `Consume` isn't supported by current rust/loom though, so we use the
        // stronger `Acquire`.
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
        assert!(v & 1 != 0);
        self.reader.epoch.store(v + 1, atomic::Ordering::Release);
        // NOTE: adding a fence(SeqCst) here speeds up loom significantly,
        // implying not having it opens up many more execution variants. This
        // implies:
        // - omitting the fence may be useful for perf
        // - omitting the fence opens up lots of ways for our code to be wrong.
    }
}
