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
                }
            }));
        }

        let tx = thread::spawn(move || {
            for i in 1..=n {
                tx.write(Box::new(i));
            }
        });

        tx.join().unwrap();
        for rx in rx_t {
            rx.join().unwrap();
        }
    });
}
