use std::thread;

#[test]
fn send_100_from_1_to_1() {
    let n = 100usize;
    let (mut tx, mut rx) = local_rcu::slot(0usize);

    let tx = thread::Builder::new()
        .name("producer".to_owned())
        .spawn(move || {
            for i in 1..=n {
                tx.write(Box::new(i));
                std::thread::yield_now();
            }
        })
        .unwrap();

    let rx = thread::Builder::new()
        .name("consumer".to_owned())
        .spawn(move || {
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
            }
        })
        .unwrap();

    tx.join().unwrap();
    rx.join().unwrap();
}

#[test]
fn guard_map_and_try_map() {
    use local_rcu::{MappedReadGuard, ReadGuard};

    #[derive(Debug)]
    struct Inner {
        list: Vec<u64>,
    }

    let (mut w, mut r) = local_rcu::slot(Inner {
        list: vec![1, 2, 3],
    });

    // project to a field, then chain into the field's contents
    let g = r.read();
    let list = ReadGuard::map(g, |i| &i.list);
    assert_eq!(&*list, &[1, 2, 3]);
    let second = MappedReadGuard::map(list, |l| &l[1]);
    assert_eq!(*second, 2);
    drop(second);

    // try_map: success hands back a projection, failure hands back the
    // original guard (read section stays open in both cases)
    let g = r.read();
    assert_eq!(*ReadGuard::try_map(g, |i| i.list.get(2)).unwrap(), 3);

    let g = r.read();
    let g = ReadGuard::try_map(g, |i| i.list.get(9)).unwrap_err();
    assert_eq!(g.list, vec![1, 2, 3]);
    drop(g);

    // a mapped guard keeps the read section open: while it lives, the writer
    // publishes new versions but cannot collect the one being examined
    let g = r.read();
    let first = ReadGuard::map(g, |i| &i.list[0]);
    assert_eq!(*first, 1);

    let mut collected = w.write(Box::new(Inner {
        list: vec![9, 9, 9],
    }));
    assert!(collected.is_empty(), "old value is still in use");
    assert_eq!(*first, 1, "mapped guard still sees the old version");

    drop(first);
    // the old version is handed back once no reader can reach it: after this reader has
    // observed the new version, read-read coherence guarantees it can never load the old one
    // again (dropping the guard alone is not enough — a stale `active` load could legally
    // return the retired pointer until the reader has moved on)
    assert_eq!(r.read().list, vec![9, 9, 9]);
    collected.extend(w.try_sync());
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].list, vec![1, 2, 3]);
}

#[test]
fn closing_slot() {
    use local_rcu::closing;

    let (mut w, mut r1) = closing::slot(0usize);

    assert!(!r1.is_closed());
    assert_eq!(*r1.read().unwrap(), 0);

    // Writer methods work through Deref/DerefMut
    w.write(Box::new(1));
    assert_eq!(*r1.read().unwrap(), 1);

    // additional readers (via clone and via the writer) observe the same slot
    // and the same close state
    let mut r2 = r1.clone();
    let mut r3 = w.reader();
    assert_eq!(*r2.read().unwrap(), 1);
    assert_eq!(*r3.read().unwrap(), 1);

    // an explicit close ends the slot
    w.close();
    assert!(r1.is_closed());
    assert!(r2.read().is_none());
    assert!(r3.read().is_none());
}

#[test]
fn closing_reader_terminates_when_writer_drops() {
    use local_rcu::closing;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const READERS: usize = 4;
    const N: usize = 100;

    let (w, r) = closing::slot(0usize);
    let finished = Arc::new(AtomicUsize::new(0));

    let mut readers = Vec::new();
    for _ in 0..READERS {
        let mut r = r.clone();
        let finished = Arc::clone(&finished);
        readers.push(thread::spawn(move || {
            let mut prev = 0;
            // the classic termination pattern: loop until the writer is gone
            while let Some(g) = r.read() {
                assert!(*g >= prev, "value went backwards");
                prev = *g;
            }
            // a reader may observe closure before it observes the writer's
            // final write (the flag does not order against the value), so the
            // last value may be missed: only monotonicity and termination
            // are guaranteed
            assert!(prev <= N);
            finished.fetch_add(1, Ordering::SeqCst);
        }));
    }

    let writer = thread::spawn(move || {
        let mut w = w;
        for i in 1..=N {
            w.write(Box::new(i));
            std::thread::yield_now();
        }
        drop(w);
    });

    writer.join().unwrap();
    for t in readers {
        t.join().unwrap();
    }
    assert_eq!(finished.load(Ordering::SeqCst), READERS);
}

#[test]
fn send_100_from_1_to_m() {
    let n = 1000usize;
    let m = 10usize;
    let (mut tx, rx) = local_rcu::slot(0usize);

    let mut rx_t = Vec::with_capacity(m);
    for i in 0..m {
        let mut rx = rx.clone();
        rx_t.push(
            thread::Builder::new()
                .name(format!("consumer {i} of {m}"))
                .spawn(move || {
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
                    }
                })
                .unwrap(),
        );
    }

    let tx = thread::Builder::new()
        .name("producer".to_owned())
        .spawn(move || {
            for i in 1..=n {
                tx.write(Box::new(i));
                std::thread::yield_now();
            }
        })
        .unwrap();

    tx.join().unwrap();
    for rx in rx_t {
        rx.join().unwrap();
    }
}
