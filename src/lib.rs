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
use std::{cell::UnsafeCell, fmt, marker::PhantomData, ops::Deref};
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

/// Readers (as indices into `Shared::epochs`) that may still be able to reach a particular
/// retired value.
///
/// We track readers by slab index instead of cloning an `Arc` per tracked reader per write:
/// - `Arc::clone`/`drop` are atomic RMWs on a cacheline the reader is actively writing (the epoch
///   and the refcount live in the same allocation), causing cacheline ping-pong between the writer
///   and readers. Tracking by index keeps the reader's cacheline RMW-free for the writer's entire
///   lifetime of the slot.
/// - It also avoids a per-write allocation when readers are busy reading.
type TrackedReaders = Vec<usize>;

/// A retired value, kept as a raw pointer until it is collected.
///
/// The `Box` is deliberately not re-formed at retire time: creating a `Box` (or any `&mut`) from
/// the pointer retags the allocation as exclusively owned, which is undefined behavior while
/// readers may still be dereferencing the value through their `ReadGuard`s (a data race under
/// Rust's aliasing model, detected by miri). `Writer::collect_prevs()` re-forms the `Box` only
/// once no tracked reader can reach the value anymore.
///
/// The `usize` is the generation at which the value stopped being `active` (`superseded_at`, see
/// `Shared::gen`): a reader whose published `floor` (see `ReaderSlot`) is at least this
/// generation has provably observed a newer value and, by read-read coherence on `active`, can
/// never load this value again — in a current or future read section.
type PrevVal<T> = (*mut T, usize, TrackedReaders);

/// State owned by the (single) writer. Kept in `Shared` (behind an `UnsafeCell`) so dropping it
/// doesn't require the writer to be alive (see `Shared::prevs`).
struct WriterState<T> {
    prevs: Vec<PrevVal<T>>,

    /// Pool of empty tracking vecs, to avoid allocating one per write.
    pool: Vec<TrackedReaders>,

    /// Writer-local mirror of `Shared::gen`: the generation of the value currently `active`.
    /// Only the writer touches this; it is `Release`-published to `Shared::gen` on every
    /// `publish()`.
    gen: usize,

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
        for (val, _, _) in &mut self.prevs {
            drop(unsafe { Box::from_raw(*val) });
        }
    }
}

struct Shared<T> {
    /// Value that readers are expected to read at this time.
    ///
    /// Is really a `Box<T>`, we need `AtomicPtr` so we can load/store it.
    active: atomic::AtomicPtr<T>,

    /// Generation of the currently-`active` value: bumped by the writer on every `publish()` and
    /// stored `Release`. Readers `Acquire`-load it immediately before loading `active` and
    /// publish the result as their `floor` (see `ReaderSlot`), giving the writer a conservative,
    /// monotonically advancing notion of how far each reader's observations have progressed.
    gen: atomic::AtomicUsize,

    /// The per-reader slots (one per reader).
    ///
    /// references to each slot exist:
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
    epochs: Mutex<slab::Slab<Arc<ReaderSlot>>>,

    /// Previous active values along with a vec of readers that may still be able to reach them,
    /// and the generation at which each was superseded (see `PrevVal`).
    // Conceptually, this is a field in `Writer`. We place it in `Shared` to avoid having Writer
    // dropping spin. Because `Shared` is in an `Arc`, by the time drop occurs all `Reader`s will
    // have released their `ReadGuard`s, and we can safely drop the `Vec`.
    //
    // Modifying the content of the `Box<T>` is not permitted until all readers have provably
    // observed a newer value (or are gone). This is enforced by the `ReadGuard`'s lifetime & the
    // floor tracking.
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

/// Per-reader state, published by the reader and examined by the writer.
///
/// `epoch` marks read-section entrance/exit (for the one-guard-at-a-time invariant) and reader
/// liveness (`DEAD` when the reader is dropped).
///
/// `floor` is the generation of the last value this reader observed (see `Shared::gen`): on
/// entering a read section, the reader loads `gen` immediately before loading `active` and
/// publishes that generation here with `Release`.
///
/// The floor is what makes reclamation sound: because a thread's successive loads of `active`
/// are ordered by read-read coherence — they can never go backwards in `active`'s modification
/// order — a reader with `floor >= G` has observed the value published at generation `G` or
/// newer, so
///
///  - its *past* accesses of any value superseded before `G` are ordered before the `Release`
///    floor store (program order), which pairs with the writer's `Acquire` load, making them
///    happen-before the free of those values, and
///  - its *future* read sections can never load a value superseded before `G` again.
///
/// Without this, a reader that ends a section and starts a new one can have its new section's
/// `active` load return an already-retired pointer (legal store-buffering behavior even for
/// acquire loads), and the writer — which can't distinguish "reader moved on" from "reader came
/// back to the old value" — would free the value mid-read (detected by miri; loom's
/// interleaving model cannot express it).
struct ReaderSlot {
    epoch: atomic::AtomicUsize,
    floor: atomic::AtomicUsize,
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
            gen: atomic::AtomicUsize::new(0),
            epochs: Mutex::new(slab::Slab::new()),
            prevs: UnsafeCell::new(WriterState {
                prevs: Vec::new(),
                pool: Vec::new(),
                gen: 0,
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

    /// Scan `prevs` for values no reader can reach anymore, collect them into `out`.
    ///
    /// Requires `epochs` to be locked.
    fn collect_prevs(&self, epochs: &slab::Slab<Arc<ReaderSlot>>, out: &mut Vec<Box<T>>) {
        // We need to move `val` out of `prevs` and into `v`. `extract_if` would work.
        // `retain_mut` doesn't unless we play some unsafe games with pointers in `prev`.
        //
        // FIXME: switch to `extract_if` once it's stable.

        let state = self.state_mut();
        let mut i = 0;
        while i < state.prevs.len() {
            let superseded_at = state.prevs[i].1;
            let tracked = &mut state.prevs[i].2;
            tracked.retain(|idx| {
                // If the slab entry is gone the reader was dropped and swept (sweep only
                // recycles entries no prevs entry refers to), so it cannot reach the value.
                let Some(slot) = epochs.get(*idx) else {
                    return false;
                };

                // `Acquire` pairs with the `Release` stores in `Reader::drop` (`DEAD`) and
                // `Reader::read()` (`floor`): once we observe the reader is dead, or that its
                // floor has reached `superseded_at`, every access it could ever have made to
                // this value happens-before this load (and thus the free below), and — by
                // read-read coherence on `active` — no future read section of that reader can
                // load this value again.
                //
                // Staying tracked otherwise is conservative and sound: a reader that has not
                // observed a newer generation can still be dereferencing this value in a live
                // section, or reach it again in a later section (a stale `active` load can
                // legally return the newly-retired pointer).
                if slot.epoch.load(atomic::Ordering::Acquire) == DEAD {
                    return false;
                }
                slot.floor.load(atomic::Ordering::Acquire) < superseded_at
            });

            if tracked.is_empty() {
                let (val, _, mut t) = state.prevs.remove(i);
                // recycle the tracking vec's allocation
                t.clear();
                state.pool.push(t);
                // SAFETY: no live or future reader can reach `val`: every reader that could
                // has been observed either dead, or with a floor at or past `superseded_at`
                // (the `Acquire` loads above pair with the reader's `Release` stores), so
                // their last access to the value happens-before this point and no guard
                // referencing it can exist or be created. Re-forming (and later dropping) the
                // `Box` here is therefore race-free; doing it any earlier (eg at retire time
                // in `publish()`) would be UB (see `PrevVal`).
                out.push(unsafe { Box::from_raw(val) });
            } else {
                i += 1;
            }
        }
    }

    /// Recycle epoch slab entries belonging to dropped readers.
    ///
    /// Requires `epochs` to be locked.
    fn sweep_dead(state: &mut WriterState<T>, epochs: &mut slab::Slab<Arc<ReaderSlot>>) {
        if !state.saw_dead {
            return;
        }
        state.saw_dead = false;

        // Indices still referenced by any prevs entry must not be recycled (a `prevs` entry
        // re-checks the reader later).
        let referenced: Vec<usize> = {
            let mut r: Vec<usize> = state
                .prevs
                .iter()
                .flat_map(|(_, _, t)| t.iter().copied())
                .collect();
            r.sort_unstable();
            r.dedup();
            r
        };

        let mut to_remove = Vec::new();
        for (idx, slot) in epochs.iter() {
            // Acquire pairs with the `Release` store in `Reader::drop`: the reader's last access
            // to the epoch (and its `Arc`) happens-before we drop the `Arc` in the slab.
            if slot.epoch.load(atomic::Ordering::Acquire) == DEAD
                && !referenced.binary_search(&idx).is_ok()
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

    /// Publish a new value and track the readers that may still be able to reach the value being
    /// replaced.
    ///
    /// Requires `epochs` to be locked.
    fn publish(&self, epochs: &slab::Slab<Arc<ReaderSlot>>, val: Box<T>) {
        // We're the only writer, so `Relaxed` is fine. We avoid a `swap`
        // because that provides extra garuntees we don't need.
        let prev = self.shared.active.load(atomic::Ordering::Relaxed);

        let state = self.state_mut();
        state.gen += 1;
        let gen = state.gen;

        // Half of a Release-Acquire pair, see `Reader::read()` for the `Acquire` half. `Release`
        // ensures that `val` is fully initialized before it is exposed to other threads.
        self.shared
            .active
            .store(Box::into_raw(val), atomic::Ordering::Release);
        // The generation of the value just published (see `Shared::gen`). `Release` pairs with
        // the reader's `Acquire` load: a reader that observes generation `gen` is guaranteed to
        // subsequently observe at least the value published at `gen` when it loads `active`
        // (both stores are `Release`, and the `active` store is sequenced before this one).
        self.shared.gen.store(gen, atomic::Ordering::Release);

        // This fence (and its counterpart in `Reader::read()`) cannot be relaxed to `Acquire`
        // and/or `Release`: reader and writer form the store-buffering (SB) litmus pattern:
        //
        //   reader: store epoch (entry); ... ; load active
        //   writer: store active;            ... ; load reader state
        //
        // With only Acquire/Release ordering, both sides may miss the other's store (each store
        // can sit in the local store buffer while the other's load executes), which would let the
        // writer conclude "no readers" and free a value the reader is about to dereference.
        // Participating in the SeqCst total order on both sides rules that out: whichever
        // side's SeqCst operation comes first in the total order forces the other side's load to
        // observe the preceding store.
        //
        // Note: reclamation soundness is carried by the floor tracking (see `ReaderSlot`), which
        // does not depend on this fence: a stale scan can only err towards tracking more readers
        // (conservative). The fence is kept as defense in depth for the scan's epoch/DEAD
        // handling.
        atomic::fence(atomic::Ordering::SeqCst);

        // add `prev` to our `prevs`, collecting the readers that may still be able to reach it,
        // so we can check on them later.

        let mut remaining_readers = state.pool.pop().unwrap_or_default();

        for (idx, slot) in epochs.iter() {
            // `Acquire` pairs with the `Release` stores in `Reader::read()` (`floor`) and
            // `Reader::drop` (`DEAD`): a reader skipped here can never reach `prev`, and the
            // acquire ordering makes all of that reader's accesses to retired values
            // happen-before any subsequent free of them.
            let v = slot.epoch.load(atomic::Ordering::Acquire);
            if v == DEAD {
                state.saw_dead = true;
                continue;
            }

            // Track any reader whose observations have not provably advanced past `prev`: a
            // reader that has not observed generation `gen` (or newer) may still dereference
            // `prev` — in a read section started before this publish, or in a future section
            // (its next load of `active` may legally still return the just-retired pointer;
            // only read-read coherence bounds it below by its published floor).
            if slot.floor.load(atomic::Ordering::Acquire) < gen {
                remaining_readers.push(idx);
            }
        }

        // Note: `prev` stays a raw pointer here (see `PrevVal`): readers may still hold guards
        // dereferencing it.
        state.prevs.push((prev, gen, remaining_readers));
    }
}

/// Something which can read the value, use `[Writer::reader]` to get one, or clone an existing `Reader`
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    slot: Arc<ReaderSlot>,
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
        let slot = Arc::new(ReaderSlot {
            epoch: atomic::AtomicUsize::new(0),
            floor: atomic::AtomicUsize::new(0),
        });

        {
            let mut epochs = shared.epochs.lock().unwrap();
            // Initialize the floor while holding the lock: publishes are serialized with this
            // insertion by the mutex, so this load observes every generation published before
            // the slot became visible to the writer. The result is a conservative lower bound
            // (a fresh reader has not observed anything); because this load happens-before any
            // `read()` of this reader, read-read coherence prevents the reader from ever
            // loading a value superseded before this generation.
            slot.floor.store(
                shared.gen.load(atomic::Ordering::Relaxed),
                atomic::Ordering::Relaxed,
            );
            // note: the slab entry is not tied to this Reader's lifetime; it is removed by the
            // writer after we mark ourselves DEAD and no prevs entry refers to us.
            epochs.insert(slot.clone());
        }

        Reader {
            shared,
            slot,
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
        let v = self.slot.epoch.load(atomic::Ordering::Relaxed);
        assert!(v & READER_FLAG == 0);

        // NOTE: we can't call `read()` a second time if we leak the previous
        // `ReadGuard`, so we can assume well behaved values.
        self.slot
            .epoch
            .store(v | READER_FLAG, atomic::Ordering::Relaxed);

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

        let g = self.shared.gen.load(atomic::Ordering::Acquire);

        // Pairs with a `Release` in `Writer::write()`, which ensures that we
        // see all the writes writer makes to things we load via `data`.
        //
        // Note: `Consume` is good enough for this operation (ie: wrt `active` we only need to
        // ensure that loads via it have a data dependency on other writes).
        // `Consume` isn't supported by current rust/loom though, so we use the
        // stronger `Acquire`.
        let data = self.shared.active.load(atomic::Ordering::Acquire);

        // Publish our observed generation as the floor. `Release` pairs with the writer's
        // `Acquire` loads in `publish()`/`collect_prevs()`: once the writer observes this, all
        // of our accesses sequenced before it (every completed read section, in particular all
        // dereferences of values superseded before generation `g`) happen-before the writer's
        // collection of those values. Combined with read-read coherence on `active` (which
        // bounds every future read section at or after `g`), this is what makes it safe for
        // the writer to free those values. See `ReaderSlot`.
        //
        // Loading the generation before the value (above) is what makes publishing `g` sound:
        // paired with the writer's `Release` stores in `publish()` (`active`, then `gen`), it
        // guarantees the `active` load returned a value at least as new as `g`, so the floor
        // never over-claims.
        self.slot.floor.store(g, atomic::Ordering::Release);

        ReadGuard {
            reader: self,
            // SAFETY: the value behind `data` can't be freed while we can reach it: the writer
            // only frees values superseded before some reader's published floor, and our floor
            // (published above) tells it our observations have not advanced past this value.
            // No `&mut`s are handed out to the data while it's in the `active` slot.
            data: unsafe { &*data },
        }
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        // Mark our epoch as dead. The writer recycles the slab entry later, once no prevs entry
        // refers to it (see `Writer::sweep_dead()`).
        //
        // `Release` pairs with the writer's `Acquire` load: our accesses to the epoch (and any
        // retired values our floor says we could still reach) happen before the writer treats
        // us as gone and collects values on our behalf.
        //
        // Note: we don't need the lock for this: a dead-marked epoch is simply never tracked
        // again, and the slab entry keeps the epoch alive until the writer recycles it.
        self.slot.epoch.store(DEAD, atomic::Ordering::Release);
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
        // NOTE: this split operation is ok because we are the only writer (others read this value).
        // This is split into 2 operations so that better code can be generated (ie: omitting CAS
        // on archs without atomic add opcodes).
        let v = self.reader.slot.epoch.load(atomic::Ordering::Relaxed);
        assert!(v & READER_FLAG != 0);
        // `Release` pairs with the writer's `Acquire` loads in `Writer::publish()` /
        // `Writer::try_sync()`: it orders this read section's loads from the value before any
        // collection decision the writer bases on our epoch.
        self.reader
            .slot
            .epoch
            .store(v + READER_FLAG, atomic::Ordering::Release);
        // NOTE: adding a fence(SeqCst) here speeds up loom significantly,
        // implying not having it opens up many more execution variants. This
        // implies:
        // - omitting the fence may be useful for perf
        // - omitting the fence opens up lots of ways for our code to be wrong.
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
