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
