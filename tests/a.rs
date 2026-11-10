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
    collected.extend(w.try_sync());
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].list, vec![1, 2, 3]);
    assert_eq!(r.read().list, vec![9, 9, 9]);
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
