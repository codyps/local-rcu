#[cfg(loom)]
use loom::thread;

#[cfg(loom)]
#[test]
fn loom_send_100_from_1_to_m() {
    loom::model(|| {
        let n = 5usize;
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
