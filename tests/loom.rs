#[cfg(loom)]
use loom::thread;

#[cfg(loom)]
#[test]
fn loom_reader_dropped_while_tracked() {
    loom::model(|| {
        let (mut tx, mut rx) = local_rcu::slot(0usize);
        let mut rx2 = tx.reader();

        // reader reads the initial value; the write replaces it while the guard is held, so
        // rx gets tracked against the old value.
        let g = rx.read();
        assert_eq!(*g, 0);
        let discarded = tx.write(Box::new(1));
        assert!(discarded.is_empty());

        // rx2 sees the new value
        let g2 = rx2.read();
        assert_eq!(*g2, 1);
        drop(g2);

        // release the guard and drop the reader: it marks its epoch DEAD while still tracked
        drop(g);
        drop(rx);

        // writing again must retire the old values (the dropped reader can't be reading them
        // anymore) and recycle the dead reader's slab slot
        let discarded = tx.write(Box::new(2));
        let mut vals: Vec<usize> = discarded.iter().map(|b| **b).collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![0, 1]);

        // a new reader reuses the recycled slot (exercises index reuse/ABA)
        let mut rx3 = tx.reader();
        let g3 = rx3.read();
        assert_eq!(*g3, 2);
        drop(g3);
        drop(rx3);

        // and everything has been collected
        let discarded = tx.sync();
        assert!(discarded.is_empty());
    });
}

#[cfg(loom)]
#[test]
fn loom_reader_drop_concurrent_with_write() {
    use loom::sync::atomic::{AtomicBool, Ordering};
    use loom::sync::Arc;

    loom::model(|| {
        let (mut tx, mut rx) = local_rcu::slot(0usize);
        let ready = Arc::new(AtomicBool::new(false));
        let go = Arc::new(AtomicBool::new(false));

        let ready2 = ready.clone();
        let go2 = go.clone();
        let t = thread::spawn(move || {
            let g = rx.read();
            ready2.store(true, Ordering::Release);
            while !go2.load(Ordering::Acquire) {
                loom::thread::yield_now();
            }
            // drop guard then the reader; the writer may be scanning/sweeping concurrently
            drop(g);
        });

        while !ready.load(Ordering::Acquire) {
            loom::thread::yield_now();
        }

        let mut discarded = tx.write(Box::new(1)); // reader tracked (or not) against value 0
        go.store(true, Ordering::Release);
        t.join().unwrap();

        // the reader is gone: both old values must be returned, and dead slots recycled
        discarded.extend(tx.write(Box::new(2)));
        let mut vals: Vec<usize> = discarded.iter().map(|b| **b).collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![0, 1]);
        let discarded = tx.sync();
        assert!(discarded.is_empty());
    });
}

#[cfg(loom)]
#[test]
fn loom_send_100_from_1_to_m() {
    loom::model(|| {
        // 5 threads takes way too long
        let n = 2usize;
        let m = 1usize;
        let (mut tx, rx) = local_rcu::slot(0usize);

        let mut rx_t = Vec::with_capacity(m);
        for _ in 0..m {
            let mut rx = rx.clone();
            rx_t.push(thread::spawn(move || {
                let mut prev = 0;
                loop {
                    let i = *rx.read();
                    if prev > i {
                        panic!("{} > {}", prev, i);
                    }
                    if i == n {
                        break;
                    }
                    prev = i;
                    loom::thread::yield_now();
                }
            }));
        }

        let tx = thread::spawn(move || {
            for i in 1..=n {
                tx.write(Box::new(i));
                loom::thread::yield_now();
            }
        });

        tx.join().unwrap();
        for rx in rx_t {
            rx.join().unwrap();
        }
    });
}

#[cfg(loom)]
#[test]
fn loom_send_from_1_to_1() {
    loom::model(|| {
        let n = 2usize;
        let (mut tx, mut rx) = local_rcu::slot(0usize);

        let rx_t = thread::spawn(move || {
            let mut prev = 0;
            loop {
                let i = *rx.read();
                if prev > i {
                    panic!("prev {prev:x} > i {i:x}");
                }
                if i > n {
                    panic!("i {i:x} > n {n:x}");
                }
                if i == n {
                    break;
                }
                prev = i;
                loom::thread::yield_now();
            }
        });

        let tx_t = thread::spawn(move || {
            let mut discarded = Vec::new();
            for i in 1..=n {
                let mut new_discarded = tx.write(Box::new(i));
                for d in &mut new_discarded {
                    **d = 0xdeadbeef;
                }
                discarded.extend(new_discarded);
                loom::thread::yield_now();
            }

            let mut new_discarded = tx.sync();
            for d in &mut new_discarded {
                **d = 0xdeadbeef;
            }
            discarded.extend(new_discarded);
        });

        tx_t.join().unwrap();
        rx_t.join().unwrap();
    });
}

#[cfg(loom)]
#[test]
fn loom_send_1_from_1_to_1() {
    loom::model(|| {
        let (mut tx, mut rx) = local_rcu::slot(0usize);

        let rx_t = thread::spawn(move || loop {
            let i = *rx.read();
            match i {
                0 => {}
                1 => break,
                _ => panic!("unexpected {i}"),
            }
            loom::thread::yield_now();
        });

        tx.write(Box::new(1));

        rx_t.join().unwrap();
    });
}

/// A value that records when it is dropped, so a reader can check it is not looking at a value
/// the writer already got back.
#[cfg(loom)]
struct Tracked {
    v: usize,
    dropped: loom::sync::Arc<loom::sync::atomic::AtomicBool>,
}

#[cfg(loom)]
impl Tracked {
    fn new(v: usize) -> Self {
        Tracked {
            v,
            dropped: loom::sync::Arc::new(loom::sync::atomic::AtomicBool::new(false)),
        }
    }
}

#[cfg(loom)]
impl Drop for Tracked {
    fn drop(&mut self) {
        self.dropped
            .store(true, loom::sync::atomic::Ordering::SeqCst);
    }
}

/// A cached reader keeps reading while the writer publishes and immediately drops whatever it
/// gets back. The reader must never see a dropped value or go backwards, both when it reuses
/// its cached value and when it moves to a new one.
#[cfg(loom)]
#[test]
fn loom_cached_reader_concurrent_with_writes() {
    use local_rcu::cached::CachedReader;
    use loom::sync::atomic::Ordering;

    loom::model(|| {
        let n = 2usize;
        let (mut tx, rx) = local_rcu::slot(Tracked::new(0));
        let mut rx = CachedReader::new(rx);

        let reader = thread::spawn(move || {
            let mut prev = 0;
            for _ in 0..3 {
                let v = rx.read();
                assert!(!v.dropped.load(Ordering::SeqCst), "read a dropped value");
                assert!(v.v >= prev);
                prev = v.v;
                loom::thread::yield_now();
                // still valid after the writer had a chance to run
                assert!(
                    !v.dropped.load(Ordering::SeqCst),
                    "value dropped while borrowed"
                );
            }
            prev
        });

        let writer = thread::spawn(move || {
            for i in 1..=n {
                drop(tx.write(Box::new(Tracked::new(i))));
                loom::thread::yield_now();
            }
            tx
        });

        let mut tx = writer.join().unwrap();
        let last = reader.join().unwrap();
        assert!(last <= n);

        // the reader is gone: everything it held back is returned
        let back = tx.sync();
        assert!(back.iter().all(|b| b.v < n));
        assert!(!tx.has_old_values());
    });
}

/// A pinned cached reader is released (or dropped) while the writer is publishing.
#[cfg(loom)]
#[test]
fn loom_cached_reader_release_concurrent_with_write() {
    use local_rcu::cached::CachedReader;
    use loom::sync::atomic::Ordering;

    loom::model(|| {
        let (mut tx, rx) = local_rcu::slot(Tracked::new(0));
        let mut rx = CachedReader::new(rx);
        // pinned before the writer starts
        assert_eq!(rx.read().v, 0);

        let reader = thread::spawn(move || {
            rx.release();
            let v = rx.read();
            assert!(!v.dropped.load(Ordering::SeqCst), "read a dropped value");
            let seen = v.v;
            loom::thread::yield_now();
            assert!(
                !v.dropped.load(Ordering::SeqCst),
                "value dropped while borrowed"
            );
            seen
        });

        drop(tx.write(Box::new(Tracked::new(1))));
        drop(tx.write(Box::new(Tracked::new(2))));

        let seen = reader.join().unwrap();
        assert!(seen <= 2);
        drop(tx.sync());
        assert!(!tx.has_old_values());
    });
}

/// The writer publishes with `write_nosync()` (no fence, no look at the reader) and collects in
/// one batch with `try_sync()`, dropping whatever comes back, while a reader reads. The reader
/// must never see a dropped value or go backwards.
#[cfg(loom)]
#[test]
fn loom_write_nosync_batch_concurrent_with_reads() {
    use loom::sync::atomic::Ordering;

    loom::model(|| {
        let n = 2usize;
        let (mut tx, mut rx) = local_rcu::slot(Tracked::new(0));

        let reader = thread::spawn(move || {
            let mut prev = 0;
            for _ in 0..2 {
                let v = rx.read();
                assert!(!v.dropped.load(Ordering::SeqCst), "read a dropped value");
                assert!(v.v >= prev);
                prev = v.v;
                loom::thread::yield_now();
                // still valid after the writer had a chance to run
                assert!(
                    !v.dropped.load(Ordering::SeqCst),
                    "value dropped while borrowed"
                );
            }
            prev
        });

        for i in 1..=n {
            tx.write_nosync(Box::new(Tracked::new(i)));
        }
        // one scan for both replaced values
        drop(tx.try_sync());
        loom::thread::yield_now();
        drop(tx.try_sync());

        let last = reader.join().unwrap();
        assert!(last <= n);

        // the reader is gone: everything it held back is returned
        let back = tx.sync();
        assert!(back.iter().all(|b| b.v < n));
        assert!(!tx.has_old_values());
    });
}
