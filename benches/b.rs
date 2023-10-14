use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn send_x_from_1_to_m(base: usize, n: usize, m: usize) {
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
                if i == (base + n) {
                    break;
                }
                prev = i;
            }
        }));
    }

    let tx = std::thread::spawn(move || {
        for i in (base + 1)..=(base + n) {
            tx.write(Box::new(i));
        }
    });

    tx.join().unwrap();
    for rx in rx_t {
        rx.join().unwrap();
    }
}

fn n1000_m10(c: &mut Criterion) {
    c.bench_function("n1000_m10", |b| {
        b.iter(|| send_x_from_1_to_m(black_box(1), 1000, 10))
    });
}

fn n100_m10(c: &mut Criterion) {
    c.bench_function("n100_m10", |b| {
        b.iter(|| send_x_from_1_to_m(black_box(1), 100, 10))
    });
}

criterion_group!(benches, n100_m10, n1000_m10);
criterion_main!(benches);
