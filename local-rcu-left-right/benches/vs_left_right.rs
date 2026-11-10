//! Benchmark the `local-rcu`-based left-right emulation against the real
//! `left-right` crate, using the same operation type on both.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use local_rcu_left_right::{Absorb, WriteHandle};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// The operation both implementations absorb: a wrapping increment.
#[derive(Clone)]
struct AddOp(i64);

impl Absorb<AddOp> for i64 {
    fn absorb(&mut self, op: AddOp) {
        *self = self.wrapping_add(op.0);
    }
}

impl left_right::Absorb<AddOp> for i64 {
    fn absorb_first(&mut self, op: &mut AddOp, _: &Self) {
        *self = self.wrapping_add(op.0);
    }
    fn absorb_second(&mut self, op: AddOp, _: &Self) {
        *self = self.wrapping_add(op.0);
    }
    fn drop_first(self: Box<Self>) {}
    fn sync_with(&mut self, first: &Self) {
        *self = *first;
    }
}

const APPENDS_PER_PUBLISH: usize = 100;

/// Spins up reader threads that hammer the structure until dropped.
struct ReaderLoad {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl ReaderLoad {
    fn spawn<Make, Run>(n: usize, make: Make) -> ReaderLoad
    where
        Make: Fn() -> Run,
        Run: FnMut() + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let handles = (0..n)
            .map(|_| {
                let mut run = make();
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        run();
                    }
                })
            })
            .collect();
        ReaderLoad { stop, handles }
    }
}

impl Drop for ReaderLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for h in self.handles.drain(..) {
            h.join().unwrap();
        }
    }
}

fn bench_writer(c: &mut Criterion) {
    let mut group = c.benchmark_group(format!("writer: {APPENDS_PER_PUBLISH}x append + publish"));

    for readers in [0usize, 4] {
        group.bench_with_input(
            BenchmarkId::new("local-rcu-left-right", format!("{readers} readers")),
            &readers,
            |b, _| {
                let (mut w, r) = WriteHandle::<i64, AddOp>::new();
                let _load = ReaderLoad::spawn(readers, || {
                    let mut r = r.clone();
                    move || {
                        black_box(*r.enter().unwrap());
                    }
                });
                b.iter(|| {
                    for _ in 0..APPENDS_PER_PUBLISH {
                        w.append(AddOp(1));
                    }
                    w.publish();
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("local-rcu-left-right relaxed", format!("{readers} readers")),
            &readers,
            |b, _| {
                let (mut w, r) = WriteHandle::<i64, AddOp>::new();
                let _load = ReaderLoad::spawn(readers, || {
                    let mut r = r.clone();
                    move || {
                        black_box(*r.enter().unwrap());
                    }
                });
                b.iter(|| {
                    for _ in 0..APPENDS_PER_PUBLISH {
                        w.append(AddOp(1));
                    }
                    // never blocks on readers; publishes a clone instead
                    w.publish_relaxed();
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("left-right", format!("{readers} readers")),
            &readers,
            |b, _| {
                let (mut w, r) = left_right::new::<i64, AddOp>();
                let _load = ReaderLoad::spawn(readers, || {
                    let r = r.clone();
                    move || {
                        black_box(*r.enter().unwrap());
                    }
                });
                b.iter(|| {
                    for _ in 0..APPENDS_PER_PUBLISH {
                        w.append(AddOp(1));
                    }
                    w.publish();
                });
            },
        );
    }

    group.finish();
}

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("read (writer idle)");

    group.bench_function("local-rcu-left-right", |b| {
        let (_w, mut r) = WriteHandle::<i64, AddOp>::new();
        b.iter(|| {
            black_box(*r.enter().unwrap());
        });
    });

    group.bench_function("left-right", |b| {
        let (_w, r) = left_right::new::<i64, AddOp>();
        b.iter(|| {
            black_box(*r.enter().unwrap());
        });
    });

    group.finish();
}

criterion_group!(benches, bench_writer, bench_read);
criterion_main!(benches);
