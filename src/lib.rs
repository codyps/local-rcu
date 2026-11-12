//! SPMC (single producer, multiple consumer), latest value only, wait free, deferred collection of
//! old data.
//!
//! One (1) writer provides a singe value to multiple readers. Readers _can_ see different versions
//! of the datastructure simultaniously unless other synchronization is used.
//!
//! No atomic exchange, compare-and-swap, or atomic math is used: only loads, stores and
//! fences.
//!
//! - Reading is wait free. Each read section does a `Relaxed` load and two `Release` stores
//!   of the reader's own (uncontended) epoch, one `SeqCst` fence, and one `Acquire` load of
//!   the shared value pointer.
//! - Writing aquires an internal mutex to scan for old values to retire. Using
//!   `write_nosync()` (which does not collect old values) is wait free: one store of the value
//!   pointer, without looking at any reader. `try_sync()` then collects in batches.
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
use std::{cell::UnsafeCell, fmt, marker::PhantomData, ops::Deref};
#[cfg(not(loom))]
use std::{
    sync::{atomic, Arc, Mutex},
    thread,
};

pub mod cached;

/// Create a new SPMC slot containing an initial value `init_val`
pub fn slot<T>(init_val: T) -> (Writer<T>, Reader<T>) {
    let w = Writer::new(Box::new(init_val));
    let r = w.reader();
    (w, r)
}

/// Aligns (and so pads) a value to the size of a cache line, so that it never shares one with
/// anything else.
///
/// Without this, two things that different threads write to (or one writes and another reads)
/// can end up next to each other in memory. Every write then takes the cache line away from
/// the other thread: "false sharing". It costs nothing in correctness and a lot in time.
///
/// The alignment follows `crossbeam_utils::CachePadded`: 128 bytes where the hardware fetches
/// cache lines in pairs (x86_64) or where lines of that size exist (aarch64, powerpc64), 64
/// elsewhere.
#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64",
    ),
    repr(align(128))
)]
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64",
    )),
    repr(align(64))
)]
struct CachePadded<V>(V);

impl<V> Deref for CachePadded<V> {
    type Target = V;

    fn deref(&self) -> &V {
        &self.0
    }
}

/// A reader's epoch, shared between the [`Reader`] and the writer's slab of epochs.
///
/// On a cache line of its own: the reader stores to it twice per read section. If it shared a
/// line with another reader's epoch (they are small allocations made one after the other, so
/// an allocator readily places them side by side), those readers would slow each other down
/// on every read without ever touching the same data.
type Epoch = Arc<CachePadded<atomic::AtomicUsize>>;

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

/// Readers (as indices into `Shared::epochs`) that may still be able to reach the values of a
/// [`Retired`] batch, each with a snapshot of its epoch at the time the batch was scanned.
///
/// We track readers by slab index instead of cloning an `Arc` per tracked reader per scan:
/// - `Arc::clone`/`drop` are atomic RMWs on a cacheline the reader is actively writing (the epoch
///   and the refcount live in the same allocation), causing cacheline ping-pong between the writer
///   and readers. Tracking by index keeps the reader's cacheline RMW-free for the writer's entire
///   lifetime of the slot.
/// - It also avoids a per-write allocation when readers are busy reading.
type TrackedReaders = Vec<(usize, usize)>;

/// Retired values that one scan of the reader epochs covers, with the readers that scan found
/// in a read section. See `Writer::track_retired()`.
///
/// `Writer::write()` scans after every value, so its batches hold one value each.
/// `Writer::write_nosync()` does not scan at all: everything it retired since the last scan
/// ends up in one batch, at the next `Writer::try_sync()` or `Writer::write()`.
///
/// The values are kept as raw pointers until they are collected. The `Box` is deliberately not
/// re-formed at retire time: creating a `Box` (or any `&mut`) from the pointer retags the
/// allocation as exclusively owned, which is undefined behavior while readers may still be
/// dereferencing the value through their `ReadGuard`s (a data race under Rust's aliasing model,
/// detected by miri). `Writer::collect_prevs()` re-forms the `Box`es only once all tracked
/// readers have been observed to leave the read section they were tracked in.
struct Retired<T> {
    vals: Vec<*mut T>,
    tracked: TrackedReaders,
}

/// State owned by the (single) writer. Kept in `Shared` (behind an `UnsafeCell`) so dropping it
/// doesn't require the writer to be alive (see `Shared::prevs`).
struct WriterState<T> {
    /// Values that have been replaced, but that no scan of the reader epochs has covered yet.
    /// Any reader may still be using them.
    unscanned: Vec<*mut T>,

    /// Batches of replaced values waiting for the readers tracked with them to move on.
    prevs: Vec<Retired<T>>,

    /// Pool of empty tracking vecs, to avoid allocating one per scan.
    pool: Vec<TrackedReaders>,

    /// Pool of empty value vecs, to avoid allocating one per scan.
    val_pool: Vec<Vec<*mut T>>,

    /// Set when a `DEAD` epoch has been observed and the epoch slab needs a sweep to recycle
    /// dropped readers.
    saw_dead: bool,
}

impl<T> Drop for WriterState<T> {
    fn drop(&mut self) {
        // Free retired values that were never collected (eg the writer was dropped, or
        // `write_nosync()` was used without a later `try_sync()`).
        //
        // SAFETY: `WriterState` is dropped only when the containing `Shared` is, which requires
        // the `Arc` refcount held by every `Writer` and `Reader` to reach zero. Every
        // `ReadGuard` borrows its `Reader`, so no guard (and no `&T` into these values) can
        // exist by then, and the `Release`/`Acquire` ordering on the final `Arc` decrements
        // makes the readers' last accesses happen-before this drop.
        let batched = self.prevs.iter().flat_map(|batch| batch.vals.iter());
        for val in self.unscanned.iter().chain(batched) {
            drop(unsafe { Box::from_raw(*val) });
        }
    }
}

struct Shared<T> {
    /// Value that readers are expected to read at this time.
    ///
    /// Is really a `Box<T>`, we need `AtomicPtr` so we can load/store it.
    ///
    /// On a cache line of its own: every read loads it, and it only changes when a new value
    /// is published. Next to `epochs` it would also be invalidated in every reader each time
    /// the writer locks or unlocks that mutex.
    active: CachePadded<atomic::AtomicPtr<T>>,

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
    epochs: Mutex<slab::Slab<Epoch>>,

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
            active: CachePadded(atomic::AtomicPtr::new(Box::into_raw(init_val))),
            epochs: Mutex::new(slab::Slab::new()),
            prevs: UnsafeCell::new(WriterState {
                unscanned: Vec::new(),
                prevs: Vec::new(),
                val_pool: Vec::new(),
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

        // Old values first, then the scan for the new one: the readers tracked for the value
        // we are about to replace have only just been looked at, there is no point in reading
        // their epochs a second time straight away.
        self.collect_prevs(&epochs, &mut r);
        self.publish(val);
        self.track_retired(&epochs, &mut r);
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
        let state = self.state();
        !state.prevs.is_empty() || !state.unscanned.is_empty()
    }

    /// Check if we can release previous values and return them
    ///
    /// Does not aquire any locks except the epoch slab lock. Returns after a single scan.
    ///
    /// Values replaced by [`Writer::write_nosync()`] since the last scan are looked at here for
    /// the first time. Those that a reader may still be using come back from a later call, once
    /// every reader that was in a read section during this call has left it.
    ///
    /// If you want to wait for all readers to finish proactively, schedule work using a timer to
    /// call this periodically. This is generally not required unless you need to obtain old values
    /// for some special purpose.
    pub fn try_sync(&mut self) -> Vec<Box<T>> {
        let mut v = Vec::new();

        let mut epochs = self.shared.epochs.lock().unwrap();
        self.collect_prevs(&epochs, &mut v);
        self.track_retired(&epochs, &mut v);
        Self::sweep_dead(self.state_mut(), &mut epochs);

        v
    }

    /// Scan `prevs` for batches whose readers have all moved on, collect their values into `out`.
    ///
    /// Requires `epochs` to be locked.
    fn collect_prevs(&self, epochs: &slab::Slab<Epoch>, out: &mut Vec<Box<T>>) {
        // We need to move the values out of `prevs` and into `out`. `extract_if` would work.
        // `retain_mut` doesn't unless we play some unsafe games with pointers in `prev`.
        //
        // FIXME: switch to `extract_if` once it's stable.

        let state = self.state_mut();
        let mut i = 0;
        while i < state.prevs.len() {
            let tracked = &mut state.prevs[i].tracked;
            tracked.retain(|(idx, prev)| {
                // Acquire pairs with the `Release` epoch stores in `ReadGuard::drop`,
                // `Reader::read()` (entry) and `Reader::drop`. Without it, the reader's loads from the old value are not
                // ordered before the free, which would be a data race on the freed memory
                // (allowed by the memory model on weakly ordered targets; not expressible by
                // loom's interleaving model, so loom can't catch this).
                epochs
                    .get(*idx)
                    .is_some_and(|e| e.load(atomic::Ordering::Acquire) == *prev)
            });

            if tracked.is_empty() {
                let Retired {
                    mut vals,
                    mut tracked,
                } = state.prevs.remove(i);
                // SAFETY: every reader that could have been dereferencing one of `vals` was
                // tracked by `track_retired()`, and `tracked` being empty means each has been
                // observed to leave the read section it was tracked in: the `Acquire` load in
                // `retain` pairs with the `Release` epoch stores in `ReadGuard::drop` and
                // `Reader::read()`, so their last access to the values happens-before this
                // point. Such a reader can never load these values again: `track_retired()` saw
                // it in its tracked section *after* the writer's `SeqCst` fence, so every later
                // section of that reader loads a value that replaced them (see "Re-entry" in
                // `track_retired()`). No guard referencing the values can exist or be created.
                // Re-forming (and later dropping) the `Box`es here is therefore race-free;
                // doing it any earlier (eg at retire time in `publish()`) would be UB (see
                // `Retired`).
                out.extend(vals.drain(..).map(|val| unsafe { Box::from_raw(val) }));
                // recycle the allocations
                tracked.clear();
                state.pool.push(tracked);
                state.val_pool.push(vals);
            } else {
                i += 1;
            }
        }
    }

    /// Recycle epoch slab entries belonging to dropped readers.
    ///
    /// Requires `epochs` to be locked.
    fn sweep_dead(state: &mut WriterState<T>, epochs: &mut slab::Slab<Epoch>) {
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
                .flat_map(|batch| batch.tracked.iter().map(|(i, _)| *i))
                .collect();
            r.sort_unstable();
            r.dedup();
            r
        };

        let mut to_remove = Vec::new();
        for (idx, e) in epochs.iter() {
            // Acquire pairs with the `Release` store in `Reader::drop`: the reader's last access
            // to the epoch (and its `Arc`) happens-before we drop the `Arc` in the slab.
            if e.load(atomic::Ordering::Acquire) == DEAD {
                if referenced.binary_search(&idx).is_ok() {
                    // A `prevs` entry still refers to this slot. Sweep again next time, so the
                    // slot is reclaimed once that entry is collected, even if `try_sync()` is
                    // all that runs from now on.
                    state.saw_dead = true;
                } else {
                    to_remove.push(idx);
                }
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

        while self.has_old_values() {
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
    /// This is wait free: one store of the value pointer, with no lock, no fence, and without
    /// looking at any reader. A writer that publishes very often disturbs its readers much less
    /// this way than with [`Writer::write()`], which reads every reader's epoch each time.
    ///
    /// The values replaced this way pile up until [`Writer::try_sync()`] (or `write()`) is
    /// called, so calling one of them now and then is required to avoid leaking old values.
    /// Everything replaced since the previous such call is then tracked as one batch: it is
    /// returned once every reader that was in a read section during that call has left it. A
    /// single reader that stays in a read section therefore holds back the whole batch, even
    /// values that were replaced while nobody was reading.
    ///
    /// In general, `Writer::write()` is a better choice.
    pub fn write_nosync(&mut self, val: Box<T>) {
        self.publish(val);
    }

    /// Publish a new value. The value it replaces is retired, but not tracked yet: that needs
    /// `track_retired()`.
    fn publish(&self, val: Box<T>) {
        // We're the only writer, so `Relaxed` is fine. We avoid a `swap`
        // because that provides extra garuntees we don't need.
        let prev = self.shared.active.load(atomic::Ordering::Relaxed);

        // Half of a Release-Acquire pair, see `Reader::read()` for the `Acquire` half. `Release`
        // ensures that `val` is fully initialized before it is exposed to other threads.
        //
        // This store (and the reader's load) does not need to be `SeqCst`. Keeping readers off
        // `prev` is the job of the fence pair in `track_retired()` and `Reader::read()`. Until
        // that fence, a reader entering a read section may still load `prev` (or an even older
        // value that has not been scanned for either), which is fine: none of them is freed
        // before `track_retired()` has looked for such readers.
        self.shared
            .active
            .store(Box::into_raw(val), atomic::Ordering::Release);

        // Note: `prev` stays a raw pointer here (see `Retired`): readers may still hold guards
        // dereferencing it.
        self.state_mut().unscanned.push(prev);
    }

    /// Find the readers that may still be using the values retired since the last call, and
    /// track them as one batch. If there are none, the values go straight to `out`.
    ///
    /// Requires `epochs` to be locked.
    fn track_retired(&self, epochs: &slab::Slab<Epoch>, out: &mut Vec<Box<T>>) {
        let state = self.state_mut();
        if state.unscanned.is_empty() {
            return;
        }

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
        //
        // The rule behind this (C++11 [atomics.order]): if a store is sequenced before SC fence
        // X, SC fence Y is sequenced before a load of the same location, and X precedes Y in the
        // SC order, then the load sees that store or a later one. For any read section, let F_R
        // be the fence in its `Reader::read()`:
        //
        // - In flight: if F_R precedes this fence, the epoch scan below sees the section's entry
        //   flag (or a later epoch). Any section that might load a retired value is therefore
        //   tracked.
        // - Re-entry: if this fence precedes F_R, the section's load of `active` sees the last
        //   store `publish()` made before this fence (or a later one), so it cannot load any
        //   of the values retired before this fence. Once the scan has seen a reader in a given
        //   section, all of that reader's later sections fall in this case. That is why
        //   `collect_prevs()` may free the batch as soon as the tracked epochs change.
        //
        // One fence serves every value in `unscanned`, however many `publish()` calls retired
        // them: the stores that replaced them are all sequenced before it, and all the argument
        // needs is that a section in the "Re-entry" case loads something newer than each.
        atomic::fence(atomic::Ordering::SeqCst);

        // collect the readers that may still be reading the retired values, so we can free them
        // later.
        let mut remaining_readers = state.pool.pop().unwrap_or_default();

        for (idx, epoch) in epochs.iter() {
            // This must be `Acquire`, not `Relaxed`. A reader we do *not* track here (idle, or
            // dead) can have the values freed straight away, so its earlier accesses to them
            // must happen-before that free. The acquire provides this by pairing with the
            // `Release` epoch stores in `ReadGuard::drop`, `Reader::read()` (entry) and
            // `Reader::drop`. The fence above makes sure a reader that might still load one of
            // the values shows up here as in-section and gets tracked (see "In flight").
            let v = epoch.load(atomic::Ordering::Acquire);
            if v == DEAD {
                state.saw_dead = true;
            } else if v & READER_FLAG != 0 {
                remaining_readers.push((idx, v));
            }
        }

        if remaining_readers.is_empty() {
            // SAFETY: no reader was in a read section after the fence, so (see "In flight"
            // above) no section that could have loaded these values is still running, and
            // (see "Re-entry") none can load them from now on. The `Acquire` loads above order
            // the readers' earlier accesses before this point.
            out.extend(
                state
                    .unscanned
                    .drain(..)
                    .map(|val| unsafe { Box::from_raw(val) }),
            );
            state.pool.push(remaining_readers);
        } else {
            let empty = state.val_pool.pop().unwrap_or_default();
            let vals = std::mem::replace(&mut state.unscanned, empty);
            state.prevs.push(Retired {
                vals,
                tracked: remaining_readers,
            });
        }
    }
}

/// Something which can read the value, use `[Writer::reader]` to get one, or clone an existing `Reader`
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    epoch: Epoch,
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
        let epoch = Arc::new(CachePadded(atomic::AtomicUsize::new(0)));
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
        let data = self.enter();

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

    /// Enter a read section and load the current value.
    ///
    /// The returned pointer stays valid until [`Reader::leave()`] is called. Callers must have
    /// exclusive use of this reader (`&mut` at the public API boundary) and must not already be
    /// in a read section.
    fn enter(&self) -> *const T {
        // `Relaxed` is enough: only this reader stores to its epoch, so it always sees its own
        // latest value.
        //
        // Note: we split this `add` up because we don't need the consistency `add` provides (we're
        // the only writer).
        let v = self.epoch.load(atomic::Ordering::Relaxed);
        assert!(v & READER_FLAG == 0);

        // NOTE: if a `ReadGuard` is leaked (eg `mem::forget`), the flag stays set and the
        // `assert!` above panics on the next `read()`; the epoch is never corrupted.
        //
        // This store must be `Release`, not `Relaxed`. The writer may free a value after
        // reading this epoch (it shows we left a tracked section, or were idle when a value
        // was retired), so our accesses in *earlier* sections must happen-before that free.
        // The `Release` in `ReadGuard::drop` does not cover this case: since C++20, a later
        // plain store from the same thread does not extend a release sequence, so a writer
        // that reads this value would not synchronize with that earlier store. (Accesses in
        // *this* section are covered by the store in `ReadGuard::drop`.)
        self.epoch.store(v | READER_FLAG, atomic::Ordering::Release);

        // Ensure the `epoch` store is visible in other threads before we read
        // `active` (so we don't free-and-reuse a value we're about to dereference).
        //
        // This is the reader half of the store-buffering (SB) pattern with the writer (see
        // `Writer::track_retired()`); it cannot be weakened:
        // - `Acquire`/`Release` (or an Acq/Rel fence) is insufficient: with only those, both
        //   sides may miss the other's store, which lets the writer free a value the reader is
        //   about to dereference (loom fails).
        // - A `SeqCst` store or `SeqCst` RMW (`fetch_or`) instead of the fence fails under
        //   loom, which treats `SeqCst` accesses roughly as `AcqRel`. The fence pair is
        //   something both loom and the C++ fence rules (see `Writer::track_retired()`) can check.
        atomic::fence(atomic::Ordering::SeqCst);

        // Pairs with the `Release` store in `Writer::publish()`, so we see every write the
        // writer made to the data reachable through `data`.
        //
        // The guarantee that this load never returns a value that has been (or may be) freed
        // while we hold the guard comes from the fence above, not from this load's ordering.
        // See "In flight" and "Re-entry" in `Writer::track_retired()`.
        //
        // Note: `Consume` is good enough for this operation (ie: wrt `active` we only need to
        // ensure that loads via it have a data dependency on other writes).
        // `Consume` isn't supported by current rust/loom though, so we use the
        // stronger `Acquire`.
        self.shared.active.load(atomic::Ordering::Acquire)
    }

    /// Leave the read section started by [`Reader::enter()`]. The pointer it returned must not
    /// be used afterwards.
    fn leave(&self) {
        // NOTE: this split operation is ok because we are the only writer (others read this value).
        // This is split into 2 operations so that better code can be generated (ie: omitting CAS
        // on archs without atomic add opcodes).
        let v = self.epoch.load(atomic::Ordering::Relaxed);
        assert!(v & READER_FLAG != 0);
        // `Release` pairs with the writer's `Acquire` epoch loads in `Writer::collect_prevs()`
        // and `Writer::track_retired()`, so this section's reads of the value happen-before any
        // free of it.
        //
        // `wrapping_add`: on 32-bit targets the epoch wraps after about 2^30 read sections, and
        // that must not panic when overflow checks are on. Wrapping is harmless. The writer
        // only compares epochs for equality, so an epoch that wraps back onto a snapshot just
        // keeps the tracked value alive until the epoch changes again, and the odd `DEAD`
        // value is still never reached.
        self.epoch
            .store(v.wrapping_add(READER_FLAG), atomic::Ordering::Release);
        // NOTE: adding a fence(SeqCst) here speeds up loom significantly,
        // implying not having it opens up many more execution variants. This
        // implies:
        // - omitting the fence may be useful for perf
        // - omitting the fence opens up lots of ways for our code to be wrong.
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

impl<T: fmt::Debug> fmt::Debug for ReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
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
        self.reader.leave();
    }
}

impl<'a, T> ReadGuard<'a, T> {
    /// Project the guard onto a part of the guarded value, keeping the read
    /// section (and thus the observed version of the value) alive.
    ///
    /// This is an associated function (like `RefCell`'s `Ref::map`) so that
    /// it is not shadowed by methods of the guarded type reachable through
    /// `Deref`.
    ///
    /// Useful to return a reference to a part of the value from a function,
    /// which is not possible with the plain guard (the guard would be
    /// dropped at the end of the function):
    ///
    /// ```
    /// use local_rcu::{MappedReadGuard, ReadGuard};
    ///
    /// struct Config {
    ///     name: String,
    /// }
    ///
    /// fn name<'a>(r: &'a mut local_rcu::Reader<Config>) -> MappedReadGuard<'a, Config, str> {
    ///     let g = r.read();
    ///     ReadGuard::map(g, |c| &c.name[..])
    /// }
    ///
    /// let (_w, mut r) = local_rcu::slot(Config { name: "local-rcu".to_owned() });
    /// assert_eq!(&*name(&mut r), "local-rcu");
    /// ```
    pub fn map<U: ?Sized, F>(orig: ReadGuard<'a, T>, f: F) -> MappedReadGuard<'a, T, U>
    where
        F: FnOnce(&T) -> &U,
    {
        MappedReadGuard {
            data: f(orig.data),
            _guard: orig,
        }
    }

    /// Like [`ReadGuard::map`], but with a fallible projection: returns the
    /// original guard unchanged when the closure returns `None` (so a miss
    /// does not end the read section).
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let (_w, mut r) = local_rcu::slot(HashMap::from([("risk", 7u32)]));
    ///
    /// let g = r.read();
    /// let v = local_rcu::ReadGuard::try_map(g, |m| m.get("risk")).unwrap();
    /// assert_eq!(*v, 7);
    /// drop(v);
    ///
    /// let g = r.read();
    /// // a miss hands the un-projected guard back:
    /// let g = local_rcu::ReadGuard::try_map(g, |m| m.get("missing")).unwrap_err();
    /// assert_eq!(g.len(), 1);
    /// ```
    pub fn try_map<U: ?Sized, F>(
        orig: ReadGuard<'a, T>,
        f: F,
    ) -> Result<MappedReadGuard<'a, T, U>, ReadGuard<'a, T>>
    where
        F: FnOnce(&T) -> Option<&U>,
    {
        match f(orig.data) {
            Some(data) => Ok(MappedReadGuard { data, _guard: orig }),
            None => Err(orig),
        }
    }
}

/// A [`ReadGuard`] projected onto a part of the guarded value.
///
/// Created by [`ReadGuard::map`], [`ReadGuard::try_map`],
/// [`MappedReadGuard::map`], or [`MappedReadGuard::try_map`]. Dropping it
/// ends the read section, exactly like dropping the guard it was created
/// from: the projection borrows from the guarded value, so it cannot outlive
/// the guard keeping the read section open (which this type owns).
///
/// If this is leaked, the value it points to will also leak (as with
/// [`ReadGuard`]).
pub struct MappedReadGuard<'a, T, U: ?Sized> {
    /// Keeps the read section open (and with it, the observed version).
    _guard: ReadGuard<'a, T>,
    data: &'a U,
}

impl<'a, T, U: ?Sized> MappedReadGuard<'a, T, U> {
    /// Project the guard onto a further part of the referenced data.
    ///
    /// Like [`ReadGuard::map`], this is an associated function so it is not
    /// shadowed by methods of the referenced type reachable through `Deref`.
    pub fn map<U2: ?Sized, F>(orig: MappedReadGuard<'a, T, U>, f: F) -> MappedReadGuard<'a, T, U2>
    where
        F: FnOnce(&U) -> &U2,
    {
        MappedReadGuard {
            data: f(orig.data),
            _guard: orig._guard,
        }
    }

    /// Like [`MappedReadGuard::map`], but with a fallible projection:
    /// returns the original guard unchanged when the closure returns `None`.
    pub fn try_map<U2: ?Sized, F>(
        orig: MappedReadGuard<'a, T, U>,
        f: F,
    ) -> Result<MappedReadGuard<'a, T, U2>, MappedReadGuard<'a, T, U>>
    where
        F: FnOnce(&U) -> Option<&U2>,
    {
        match f(orig.data) {
            Some(data) => Ok(MappedReadGuard {
                data,
                _guard: orig._guard,
            }),
            None => Err(orig),
        }
    }
}

impl<'a, T, U: ?Sized> Deref for MappedReadGuard<'a, T, U> {
    type Target = U;
    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<T, U: ?Sized + fmt::Debug> fmt::Debug for MappedReadGuard<'_, T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// An opt-in API where the writer can signal that no further values will be
/// published, detectable by readers (like `left-right`'s `enter() -> None` or
/// a channel closing).
///
/// This is a thin wrapper around the ordinary [`slot()`] / [`Writer`] /
/// [`Reader`] API: nothing about the core types changes, and readers that do
/// not use [`ClosingReader`] pay nothing. The only cost of opting in is one
/// atomic load of an otherwise uncontended flag per read, and one atomic
/// store when the writer is dropped.
///
/// ```
/// let (mut w, mut r) = local_rcu::closing::slot(0usize);
///
/// assert_eq!(*r.read().unwrap(), 0);
/// w.write(Box::new(1));
/// assert_eq!(*r.read().unwrap(), 1);
///
/// drop(w);
/// // no more values will ever be published:
/// assert!(r.read().is_none());
/// assert!(r.is_closed());
/// ```
///
/// # Semantics
///
/// - [`ClosingWriter::close()`] (or dropping the writer) sets the flag;
///   afterwards every [`ClosingReader::read()`] returns `None`.
/// - The flag is only a signal: the last published value remains valid
///   (and owned) until readers drop it, but [`ClosingReader`] does not give
///   access to it after closure. Use the plain [`Reader`] API instead if
///   readers should keep reading the last value forever.
/// - A read returning `Some` may still be in flight when the writer drops:
///   the guard stays valid, and the *next* read returns `None`.
/// - There is no guarantee that a reader observes the most recently written
///   value before it observes closure: a value published immediately before
///   the writer drops may be missed. If delivering the final value matters,
///   keep the writer alive until readers have acknowledged it (this API has
///   no per-reader version tracking, unlike `tokio::sync::watch`).
pub mod closing {
    use super::{ReadGuard, Reader, Writer};
    use std::ops::{Deref, DerefMut};
    use std::sync::{atomic, Arc};

    /// The "no more values will be published" flag shared between a
    /// [`ClosingWriter`] and all its [`ClosingReader`]s.
    struct CloseState(atomic::AtomicBool);

    impl CloseState {
        fn close(&self) {
            // `Release` so that everything the writer did (including its
            // final writes) is visible to a reader that observes the close.
            self.0.store(true, atomic::Ordering::Release);
        }

        fn is_closed(&self) -> bool {
            self.0.load(atomic::Ordering::Acquire)
        }
    }

    /// Like [`slot()`](crate::slot), but the writer's drop is observable by
    /// its readers.
    pub fn slot<T>(init_val: T) -> (ClosingWriter<T>, ClosingReader<T>) {
        let (w, r) = super::slot(init_val);
        let state = Arc::new(CloseState(atomic::AtomicBool::new(false)));

        (
            ClosingWriter {
                inner: w,
                state: state.clone(),
            },
            ClosingReader { inner: r, state },
        )
    }

    /// A [`Writer`] whose drop (or an explicit
    /// [`close()`](ClosingWriter::close)) signals readers that no further
    /// values will be published.
    ///
    /// All [`Writer`] methods are available through `Deref`/`DerefMut`.
    pub struct ClosingWriter<T> {
        inner: Writer<T>,
        state: Arc<CloseState>,
    }

    impl<T> ClosingWriter<T> {
        /// Create another reader that observes closure.
        ///
        /// (Shadows [`Writer::reader()`], which would produce a plain
        /// [`Reader`] without closure detection.)
        pub fn reader(&self) -> ClosingReader<T> {
            ClosingReader {
                inner: self.inner.reader(),
                state: self.state.clone(),
            }
        }

        /// Consume the writer, signaling readers that no further values
        /// will be published.
        ///
        /// Equivalent to dropping the writer; provided for explicitly
        /// ending a slot at a chosen point.
        pub fn close(self) {
            // `Drop` sets the flag.
        }
    }

    impl<T> Drop for ClosingWriter<T> {
        fn drop(&mut self) {
            self.state.close();
        }
    }

    impl<T> Deref for ClosingWriter<T> {
        type Target = Writer<T>;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }

    impl<T> DerefMut for ClosingWriter<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }

    /// A [`Reader`] that can tell when its [`ClosingWriter`] has been
    /// dropped or closed.
    ///
    /// Cloning produces another reader observing the same slot and the same
    /// close state.
    pub struct ClosingReader<T> {
        inner: Reader<T>,
        state: Arc<CloseState>,
    }

    impl<T> Clone for ClosingReader<T> {
        fn clone(&self) -> Self {
            ClosingReader {
                inner: self.inner.clone(),
                state: self.state.clone(),
            }
        }
    }

    impl<T> ClosingReader<T> {
        /// Like [`Reader::read()`], but returns `None` once the writer has
        /// been dropped or closed.
        ///
        /// The returned guard is the ordinary [`ReadGuard`] (and supports
        /// [`ReadGuard::map`](crate::ReadGuard::map) etc).
        pub fn read(&mut self) -> Option<ReadGuard<'_, T>> {
            if self.state.is_closed() {
                None
            } else {
                Some(self.inner.read())
            }
        }

        /// Has the writer been dropped or closed?
        pub fn is_closed(&self) -> bool {
            self.state.is_closed()
        }
    }
}
