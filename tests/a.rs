#[test]
fn send_100_from_1_to_1() {
    let n = 100usize;
    let (mut tx, mut rx) = local_rcu::slot(0usize);

    let tx = std::thread::spawn(move || {
        for i in 1..=n {
            tx.write(Box::new(i));
            std::thread::yield_now();
        }
    });

    let rx = std::thread::spawn(move || {
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
    });

    tx.join().unwrap();
    rx.join().unwrap();
}

#[test]
fn send_100_from_1_to_m() {
    let n = 1000usize;
    let m = 10usize;
    let (mut tx, rx) = local_rcu::slot(0usize);

    let mut rx_t = Vec::with_capacity(m);
    for _ in 0..m {
        let mut rx = rx.clone();
        rx_t.push(std::thread::spawn(move || {
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
        }));
    }

    let tx = std::thread::spawn(move || {
        for i in 1..=n {
            tx.write(Box::new(i));
            std::thread::yield_now();
        }
    });

    tx.join().unwrap();
    for rx in rx_t {
        rx.join().unwrap();
    }
}
