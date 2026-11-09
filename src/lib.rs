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

/// Epoch value stored by a reader that has been dropped.
///
/// Because epochs only ever advance by `+2` and are always even, this odd value can never be
/// produced by a live reader, so there is no risk of confusing a live reader with a dead one.
const DEAD: usize = usize::MAX;

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

/// A previous active value, along with a snapshot (per still-existing reader) of the epoch at the
/// time the value was replaced, and the index of that reader's epoch counter (in `Shared::epochs`)
/// so we can determine what epoch the reader is at now.
///
/// We track readers by slab index instead of cloning an `Arc` per tracked reader per write:
/// - `Arc::clone`/`drop` are atomic RMWs on a cacheline the reader is actively writing (the epoch
///   and the refcount live in the same allocation), causing cacheline ping-pong between the writer
///   and readers. Tracking by index keeps the reader's cacheline RMW-free for the writer's entire
///   lifetime of the slot.
/// - It also avoids a per-write allocation when readers are busy reading.
type TrackedReaders = Vec<(usize, usize)>;

type PrevVal<T> = (Box<T>, TrackedReaders);

/// State owned by the (single) writer. Kept in `Shared` (behind an `UnsafeCell`) so dropping it
/// doesn't require the writer to be alive (see `Shared::prevs`).
struct WriterState<T> {
    prevs: Vec<PrevVal<T>>,

    /// Pool of empty tracking vecs, to avoid allocating one per write.
    pool: Vec<TrackedReaders>,

    /// Set when a `DEAD` epoch has been observed and the epoch slab needs a sweep to recycle
    /// dropped readers.
    saw_dead: bool,
}

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
    ///
    /// Readers do _not_ remove themselves when dropped. Instead, they mark their epoch `DEAD`
    /// (which requires no lock), and the writer recycles the slab entry once no `prevs` entry
    /// refers to it.
    ///
    /// This is locked when a new reader is created, and when a writer is writing a new value.
    /// Contention is limited as long as we don't create readers too often and/or don't write new
    /// values too often.
    epochs: Mutex<slab::Slab<Arc<atomic::AtomicUsize>>>,

    /// Previous active values along with a vec of readers, each with a snapshot of the epoch at
    /// the time _after_ the previous active value was made inactive and the index of the
    /// reader's epoch counter so we can determine what epoch that reader is at now.
    // Conceptually, this is a field in `Writer`. We place it in `Shared` to avoid having Writer
    // dropping spin. Because `Shared` is in an `Arc`, by the time drop occurs all `Reader`s will
    // have released their `ReadGuard`s, and we can safely drop the `Vec`.
    //
    // Modifying the content of the `Box<T>` is not permitted until all readers have dropped their
    // references to the `Box<T>`. This is enforced by the `ReadGuard`'s lifetime & the epoch
    // count.
    prevs: UnsafeCell<WriterState<T>>,
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
    /// SAFETY: callers must have exclusive access as the single writer (`&mut self` at the public
    /// API boundary) to mutate this.
    fn state(&self) -> &WriterState<T> {
        // SAFETY: only this `Writer` can access the writer state.
        unsafe { &*self.shared.prevs.get() }
    }

    /// SAFETY: callers must have exclusive access as the single writer (`&mut self` at the public
    /// API boundary) to mutate this.
    #[allow(clippy::mut_from_ref)] // single-writer `UnsafeCell` access; see SAFETY note
    fn state_mut(&self) -> &mut WriterState<T> {
        // SAFETY: only this `Writer` can access the writer state.
        unsafe { &mut *self.shared.prevs.get() }
    }

    /// Create a new `Writer` with an initial value
    ///
    /// The `Writer` can than be used to obtain one or more [`Reader`]s.
    pub fn new(init_val: Box<T>) -> Writer<T> {
        let shared = Arc::new(Shared {
            active: atomic::AtomicPtr::new(Box::into_raw(init_val)),
            epochs: Mutex::new(slab::Slab::new()),
            prevs: UnsafeCell::new(WriterState {
                prevs: Vec::new(),
                pool: Vec::new(),
                saw_dead: false,
            }),
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
        let mut r = Vec::new();

        // Aquire the lock once for the whole operation. Scanning for old values, publishing, and
        // scanning for readers of the value we're replacing.
        let mut epochs = self.shared.epochs.lock().unwrap();

        self.collect_prevs(&epochs, &mut r);
        self.publish(&epochs, val);
        self.collect_prevs(&epochs, &mut r);
        Self::sweep_dead(self.state_mut(), &mut epochs);

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
        !self.state().prevs.is_empty()
    }

    /// Check if we can release previous values and return them
    ///
    /// Does not aquire any locks except the epoch slab lock. Returns after a single scan.
    ///
    /// If you want to wait for all readers to finish proactively, schedule work using a timer to
    /// call this periodically. This is generally not required unless you need to obtain old values
    /// for some special purpose.
    pub fn try_sync(&mut self) -> Vec<Box<T>> {
        let mut v = Vec::new();

        let mut epochs = self.shared.epochs.lock().unwrap();
        self.collect_prevs(&epochs, &mut v);
        Self::sweep_dead(self.state_mut(), &mut epochs);

        v
    }

    /// Scan `prevs` for values whose readers have all moved on, collect them into `out`.
    ///
    /// Requires `epochs` to be locked.
    fn collect_prevs(&self, epochs: &slab::Slab<Arc<atomic::AtomicUsize>>, out: &mut Vec<Box<T>>) {
        // We need to move `val` out of `prevs` and into `v`. `extract_if` would work.
        // `retain_mut` doesn't unless we play some unsafe games with pointers in `prev`.
        //
        // FIXME: switch to `extract_if` once it's stable.

        let state = self.state_mut();
        let mut i = 0;
        while i < state.prevs.len() {
            let tracked = &mut state.prevs[i].1;
            tracked.retain(|(idx, prev)| {
                // Acquire pairs with the `Release` stores in `ReadGuard::drop` and
                // `Reader::drop`. Without it, the reader's loads from the old value are not
                // ordered before the free, which would be a data race on the freed memory
                // (allowed by the memory model on weakly ordered targets; not expressible by
                // loom's interleaving model, so loom can't catch this).
                epochs
                    .get(*idx)
                    .is_some_and(|e| e.load(atomic::Ordering::Acquire) == *prev)
            });

            if tracked.is_empty() {
                let (val, mut t) = state.prevs.remove(i);
                // recycle the tracking vec's allocation
                t.clear();
                state.pool.push(t);
                out.push(val);
            } else {
                i += 1;
            }
        }
    }

    /// Recycle epoch slab entries belonging to dropped readers.
    ///
    /// Requires `epochs` to be locked.
    fn sweep_dead(state: &mut WriterState<T>, epochs: &mut slab::Slab<Arc<atomic::AtomicUsize>>) {
        if !state.saw_dead {
            return;
        }
        state.saw_dead = false;

        // Indices still referenced by any prevs entry must not be recycled (a `prevs` entry
        // re-checks the epoch later).
        let referenced: Vec<usize> = {
            let mut r: Vec<usize> = state
                .prevs
                .iter()
                .flat_map(|(_, t)| t.iter().map(|(i, _)| *i))
                .collect();
            r.sort_unstable();
            r.dedup();
            r
        };

        let mut to_remove = Vec::new();
        for (idx, e) in epochs.iter() {
            // Acquire pairs with the `Release` store in `Reader::drop`: the reader's last access
            // to the epoch (and its `Arc`) happens-before we drop the `Arc` in the slab.
            if e.load(atomic::Ordering::Acquire) == DEAD && !referenced.binary_search(&idx).is_ok()
            {
                to_remove.push(idx);
            }
        }
        for idx in to_remove {
            epochs.remove(idx);
        }
    }

    /// `try_sync()` repeatedly until all old values are collected
    ///
    /// This spins, and in general should be avoided.
    pub fn sync(&mut self) -> Vec<Box<T>> {
        let mut r = Vec::new();

        while !self.state().prevs.is_empty() {
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
        let epochs = self.shared.epochs.lock().unwrap();
        self.publish(&epochs, val);
    }

    /// Publish a new value and track readers of the value being replaced.
    ///
    /// Requires `epochs` to be locked.
    fn publish(&self, epochs: &slab::Slab<Arc<atomic::AtomicUsize>>, val: Box<T>) {
        // We're the only writer, so `Relaxed` is fine. We avoid a `swap`
        // because that provides extra garuntees we don't need.
        let prev = self.shared.active.load(atomic::Ordering::Relaxed);

        // Half of a Release-Acquire pair, see `Reader::read()` for the `Acquire` half. `Release`
        // ensures that `val` is fully initialized before it is exposed to other threads.
        self.shared
            .active
            .store(Box::into_raw(val), atomic::Ordering::Release);

        // This fence (and its counterpart in `Reader::read()`) cannot be relaxed to `Acquire`
        // and/or `Release`: reader and writer form the store-buffering (SB) litmus pattern:
        //
        //   reader: store epoch (entry); ... ; load active
        //   writer: store active;            ... ; load epochs
        //
        // With only Acquire/Release ordering, both sides may miss the other's store (each store
        // can sit in the local store buffer while the other's load executes), which would let the
        // writer conclude "no readers" and free a value the reader is about to dereference.
        // Participating in the SeqCst total order on both sides rules that out: whichever
        // side's SeqCst operation comes first in the total order forces the other side's load to
        // observe the preceding store.
        atomic::fence(atomic::Ordering::SeqCst);

        // add `prev` to our `prevs`, collecting the readers that may still be reading it, so we
        // can retire it later.

        let state = self.state_mut();
        let mut remaining_readers = state.pool.pop().unwrap_or_default();

        for (idx, epoch) in epochs.iter() {
            // This is not where values are freed (see `try_sync()`), so no `Acquire` is needed
            // here. The SeqCst fence above makes the reader's entry visible to us (see
            // `Reader::read()`), and mis-tracking a reader is merely conservative.
            let v = epoch.load(atomic::Ordering::Relaxed);
            if v == DEAD {
                state.saw_dead = true;
            } else if v & READER_FLAG != 0 {
                remaining_readers.push((idx, v));
            }
        }

        state
            .prevs
            .push((unsafe { Box::from_raw(prev) }, remaining_readers));
    }
}

/// Something which can read the value, use `[Writer::reader]` to get one, or clone an existing `Reader`
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    epoch: Arc<atomic::AtomicUsize>,
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
        // note: the slab entry is not tied to this Reader's lifetime; it is removed by the writer
        // after we mark ourselves DEAD and no prevs entry refers to us.
        shared.epochs.lock().unwrap().insert(epoch.clone());

        Reader {
            shared,
            epoch,
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
    pub fn read(&mut self) -> ReadGuard<'_, T> {
        // We're using `Relaxed` because all the ordering needed comes from the `Acquire` on
        // `self.shared.active` below.
        //
        // Note: we split this `add` up because we don't need the consistency `add` provides (we're
        // the only writer).
        let v = self.epoch.load(atomic::Ordering::Relaxed);
        assert!(v & READER_FLAG == 0);

        // NOTE: we can't call `read()` a second time if we leak the previous
        // `ReadGuard`, so we can assume well behaved values.
        self.epoch.store(v | READER_FLAG, atomic::Ordering::Relaxed);

        // Ensure the `epoch` store is visible in other threads before we read
        // `active` (so we don't free-and-reuse a value we're about to dereference).
        //
        // This is the reader half of the store-buffering (SB) pattern with the writer (see
        // `Writer::publish()`); it cannot be weakened:
        // - `Acquire`/`Release` (or an Acq/Rel fence) is insufficient: with only those, both
        //   sides may miss the other's store, which lets the writer free a value the reader is
        //   about to dereference (loom fails).
        // - replacing it with a `SeqCst` store or `SeqCst` RMW (`fetch_or`) also fails loom's
        //   model: only the fence/fence combination is verified to order this pattern.
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
        // Mark our epoch as dead. The writer recycles the slab entry later, once no prevs entry
        // refers to it (see `Writer::sweep_dead()`).
        //
        // `Release` pairs with the writer's `Acquire` load, ensuring our accesses to the epoch
        // (and any old values we may still be tracked against) happen before the epoch memory is
        // reclaimed along with the `Arc`.
        //
        // Note: we don't need the lock for this: a dead-marked epoch is simply never tracked
        // again, and the slab entry keeps the epoch alive until the writer recycles it.
        self.epoch.store(DEAD, atomic::Ordering::Release);
    }
}

/// Allows access to the underlying value
///
/// If this is leaked, the value it points to will also leak.
///
/// This represents a particular version of the value.
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

/// Bit in a reader's epoch indicating it is currently in a read section.
///
/// We use bit 1 (value `2`) rather than bit 0 so that all naturally produced epochs are even,
/// which makes the odd [`DEAD`] sentinel unreachable by counting, structurally preventing a live
/// reader's epoch from ever being confused with a dropped reader's.
const READER_FLAG: usize = 2;

impl<'a, T> Drop for ReadGuard<'a, T> {
    fn drop(&mut self) {
        // NOTE: this split operation is ok because we are the only writer (others read this value).
        // This is split into 2 operations so that better code can be generated (ie: omitting CAS
        // on archs without atomic add opcodes).
        let v = self.reader.epoch.load(atomic::Ordering::Relaxed);
        assert!(v & READER_FLAG != 0);
        // `Release` pairs with the writer's `Acquire` load in `Writer::try_sync()`: it orders
        // this read section's loads from the old value before the writer's free.
        self.reader
            .epoch
            .store(v + READER_FLAG, atomic::Ordering::Release);
        // NOTE: adding a fence(SeqCst) here speeds up loom significantly,
        // implying not having it opens up many more execution variants. This
        // implies:
        // - omitting the fence may be useful for perf
        // - omitting the fence opens up lots of ways for our code to be wrong.
    }
}
