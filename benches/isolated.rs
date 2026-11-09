//! Isolated micro-benchmarks that measure reader and writer costs separately, at varying reader
//! counts. Complements `b.rs` (which measures an end-to-end pipeline).
//!
//! Run with: `cargo bench --bench isolated`
//!
//! Note: the stop flag is cacheline-padded: epochs and other small heap allocations can land on
//! adjacent cachelines, and an unpadded control flag sharing a line with a reader's epoch will
//! badly skew results.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[repr(align(64))]
struct PaddedFlag(AtomicBool);
impl PaddedFlag {
    fn load(&self, o: Ordering) -> bool {
        self.0.load(o)
    }
    fn store(&self, v: bool, o: Ordering) {
        self.0.store(v, o)
    }
}

use local_rcu::slot;

/// Reader entry+exit throughput (`read()` + guard drop), with an optional concurrently writing
/// writer.
fn reader_bench(m_readers: usize, writer_active: bool) -> f64 {
    let (mut w, rx) = slot(0u64);
    let stop = Arc::new(PaddedFlag(AtomicBool::new(false)));
    let mut handles = Vec::new();

    for _ in 0..m_readers {
        let mut rx = rx.clone();
        let stop = stop.clone();
        handles.push(thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // full entry+exit cycle
                let g = rx.read();
                black_box(*g);
                n += 1;
                drop(g);
            }
            n
        }));
    }

    let writer = if writer_active {
        let stop = stop.clone();
        Some(thread::spawn(move || {
            let mut i = 1u64;
            while !stop.load(Ordering::Relaxed) {
                w.write_nosync(Box::new(i));
                i += 1;
            }
        }))
    } else {
        None
    };

    thread::sleep(Duration::from_millis(500));
    let t0 = Instant::now();
    thread::sleep(Duration::from_millis(1000));
    let elapsed = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);

    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    if let Some(wt) = writer {
        wt.join().unwrap();
    }
    total as f64 / elapsed / m_readers as f64
}

/// Writer `write()` throughput with `m` readers that are either idle (holding no guard) or
/// continuously reading.
fn writer_bench(m_readers: usize, readers_busy: bool) -> f64 {
    let (mut w, rx) = slot(0u64);
    let stop = Arc::new(PaddedFlag(AtomicBool::new(false)));
    let mut handles = Vec::new();

    for _ in 0..m_readers {
        let mut rx = rx.clone();
        let stop = stop.clone();
        handles.push(thread::spawn(move || {
            if readers_busy {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let g = rx.read();
                    black_box(*g);
                    n += 1;
                    drop(g);
                }
                black_box(n);
            } else {
                while !stop.load(Ordering::Relaxed) {
                    thread::yield_now();
                }
            }
        }));
    }

    thread::sleep(Duration::from_millis(300));

    let t0 = Instant::now();
    let n = 200_000u64;
    for i in 0..n {
        let ret = black_box(w.write(black_box(Box::new(i))));
        // keep old values alive until next iteration, then drop
        drop(ret);
    }
    let elapsed = t0.elapsed().as_secs_f64();

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    n as f64 / elapsed
}

fn main() {
    // reader entry+exit, uncontended and contended by a writer
    for &active in &[false, true] {
        for &m in &[1usize, 4] {
            let rate = reader_bench(m, active);
            println!(
                "reader  m={:<2} writer_active={:<5} {:>12.0} reads/s/reader",
                m, active, rate
            );
        }
    }

    // writer with readers idle vs busy
    for &busy in &[false, true] {
        for &m in &[0usize, 1, 4, 8] {
            if m == 0 && busy {
                continue;
            }
            let rate = writer_bench(m, busy);
            println!(
                "writer  m={:<2} readers_{:<5} {:>12.0} writes/s",
                m,
                if busy { "busy" } else { "idle" },
                rate
            );
        }
    }
}
