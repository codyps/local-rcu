//! Tests for `local_rcu::cached::CachedReader`.
#![cfg(not(loom))]

use local_rcu::cached::CachedReader;
use local_rcu::slot;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// A value that records when it is dropped.
struct Tracked {
    v: usize,
    dropped: Arc<AtomicBool>,
}

impl Tracked {
    fn new(v: usize) -> (Self, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        (
            Tracked {
                v,
                dropped: dropped.clone(),
            },
            dropped,
        )
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn reads_follow_writes() {
    let (mut w, r) = slot(0usize);
    let mut r = CachedReader::new(r);
    assert!(!r.is_pinned());

    assert_eq!(*r.read(), 0);
    assert!(r.is_pinned());
    // unchanged value: same reference again
    let a = r.read() as *const usize;
    let b = r.read() as *const usize;
    assert_eq!(a, b);

    for i in 1..100 {
        w.write(Box::new(i));
        assert_eq!(*r.read(), i);
        assert_eq!(*r.read(), i);
    }
}

#[test]
fn pinned_reader_holds_values_back_until_it_reads() {
    let (mut w, r) = slot(0usize);
    let mut r = CachedReader::new(r);
    assert_eq!(*r.read(), 0);

    // the reader is pinned and idle: nothing comes back
    let mut returned = 0;
    for i in 1..=10 {
        returned += w.write(Box::new(i)).len();
    }
    returned += w.try_sync().len();
    assert_eq!(returned, 0);

    // reading again moves the reader to the latest value and lets go of the rest
    assert_eq!(*r.read(), 10);
    let back = w.try_sync();
    assert_eq!(back.len(), 10);
    let mut vals: Vec<usize> = back.iter().map(|b| **b).collect();
    vals.sort_unstable();
    assert_eq!(vals, (0..10).collect::<Vec<_>>());
    assert!(!w.has_old_values());
}

#[test]
fn released_reader_holds_nothing_back() {
    let (mut w, r) = slot(0usize);
    let mut r = CachedReader::new(r);

    // never read: not pinned
    assert_eq!(w.write(Box::new(1)).len(), 1);

    assert_eq!(*r.read(), 1);
    assert!(w.write(Box::new(2)).is_empty());
    r.release();
    assert!(!r.is_pinned());
    r.release(); // no-op
    assert_eq!(w.try_sync().len(), 1);
    assert_eq!(w.write(Box::new(3)).len(), 1);

    assert_eq!(*r.read(), 3);
}

#[test]
fn value_is_not_dropped_while_borrowed() {
    let (v0, d0) = Tracked::new(0);
    let (mut w, r) = slot(v0);
    let mut r = CachedReader::new(r);

    let seen = r.read();
    let (v1, d1) = Tracked::new(1);
    drop(w.write(Box::new(v1)));
    drop(w.try_sync());
    assert!(!d0.load(Ordering::SeqCst));
    assert_eq!(seen.v, 0);

    // moving on releases the old value
    assert_eq!(r.read().v, 1);
    drop(w.try_sync());
    assert!(d0.load(Ordering::SeqCst));
    assert!(!d1.load(Ordering::SeqCst));
}

#[test]
fn dropping_a_pinned_reader_releases_its_values() {
    let (v0, d0) = Tracked::new(0);
    let (mut w, r) = slot(v0);
    let mut r = CachedReader::new(r);
    assert_eq!(r.read().v, 0);

    let (v1, _d1) = Tracked::new(1);
    assert!(w.write(Box::new(v1)).is_empty());
    drop(r);
    assert_eq!(w.try_sync().len(), 1);
    assert!(d0.load(Ordering::SeqCst));
}

#[test]
fn into_inner_and_clone() {
    let (mut w, r) = slot(0usize);
    let mut c = CachedReader::from(r);
    assert_eq!(*c.read(), 0);

    // a clone starts released and is independent
    let mut c2 = c.clone();
    assert!(!c2.is_pinned());
    assert_eq!(*c2.read(), 0);
    c2.release();

    // back to a plain reader: no longer pinned, and usable as usual
    let mut plain = c.into_inner();
    assert_eq!(w.write(Box::new(1)).len(), 1);
    assert_eq!(*plain.read(), 1);
}

#[test]
fn everything_is_freed_at_the_end() {
    let live = Arc::new(AtomicUsize::new(0));
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let make = |live: &Arc<AtomicUsize>| {
        live.fetch_add(1, Ordering::SeqCst);
        Counted(live.clone())
    };

    {
        let (mut w, r) = slot(make(&live));
        let mut pinned = CachedReader::new(r);
        let _ = pinned.read();
        for _ in 0..20 {
            w.write_nosync(Box::new(make(&live)));
        }
        assert_eq!(live.load(Ordering::SeqCst), 21);
        // writer and pinned reader dropped here, in that order
    }
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

/// Readers and a writer hammer the slot. A reader must never see a dropped value, and never
/// see values go backwards.
#[test]
fn concurrent_readers_never_see_dropped_values() {
    let writes: usize = if cfg!(miri) { 200 } else { 200_000 };
    let n_readers = if cfg!(miri) { 2 } else { 4 };

    let (v0, _) = Tracked::new(0);
    let (mut w, r) = slot(v0);
    let done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..n_readers)
        .map(|id| {
            let mut r = CachedReader::new(r.clone());
            let done = done.clone();
            std::thread::spawn(move || {
                let mut prev = 0;
                let mut n = 0usize;
                loop {
                    let finished = done.load(Ordering::Acquire);
                    let v = r.read();
                    assert!(!v.dropped.load(Ordering::SeqCst), "read a dropped value");
                    assert!(v.v >= prev, "went backwards: {prev} -> {}", v.v);
                    prev = v.v;
                    n += 1;
                    // some readers let go now and then, to mix both paths
                    if id % 2 == 1 && n.is_multiple_of(7) {
                        r.release();
                    }
                    if finished {
                        assert_eq!(prev, writes);
                        break;
                    }
                }
            })
        })
        .collect();
    drop(r);

    for i in 1..=writes {
        let (v, _) = Tracked::new(i);
        drop(w.write(Box::new(v)));
    }
    done.store(true, Ordering::Release);
    for r in readers {
        r.join().unwrap();
    }

    // every reader has dropped: all old values come back
    drop(w.sync());
    assert!(!w.has_old_values());
}
