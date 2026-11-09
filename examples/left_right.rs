//! Emulate [`left-right`]-style updates using `local-rcu`.
//!
//! [`left-right`] keeps two copies of the data: readers see one copy while
//! the writer absorbs operations into the other (the "alternate"), then
//! publishes it in a single atomic step. Readers never observe partially
//! applied operations.
//!
//! The same structure can be built on `local-rcu`:
//!
//! 1. call [`Writer::sync()`](local_rcu::Writer::sync) before every
//!    [`write()`](local_rcu::Writer::write), and
//! 2. absorb operations into the value `sync()` returns, publishing it with
//!    the next `write()`.
//!
//! [`Writer::new()`](local_rcu::Writer::new) only takes a single initial
//! value, so the alternate copy must be created and tracked by hand: `write()`
//! and `sync()` hand old values back only once no reader still uses them.
//!
//! The value returned by `sync()` is a _stale_ snapshot: it is the value that
//! was retired one round ago, not the one just published. Like `left-right`'s
//! operation log, ops published in the interim must be re-applied
//! ("absorbed") into the alternate before new operations are absorbed.
//!
//! [`left-right`]: https://crates.io/crates/left-right

use local_rcu::slot;
use std::thread;

/// Operations are published in batches; each batch becomes visible to
/// readers atomically.
const BATCHES: u64 = 100;
const OPS_PER_BATCH: u64 = 10;
const TOTAL: u64 = BATCHES * OPS_PER_BATCH;
const READERS: usize = 4;

fn main() {
    let (mut w, r) = slot(Vec::<u64>::new());

    // The alternate: the copy the writer absorbs operations into. Readers
    // never see it until it is published. `slot()` gives us one initial
    // value, so the second copy is created here.
    let mut alternate: Option<Box<Vec<u64>>> = Some(Box::new(Vec::new()));

    // The "operation log" (as in `left-right`): operations that have been
    // published, but not yet absorbed into the alternate.
    let mut unabsorbed: Vec<u64> = Vec::new();

    thread::scope(|s| {
        // Readers verify that every observed snapshot is consistent: the log
        // only grows, entry `i` is always `i + 1`, and batches are never
        // visible half-applied.
        for _ in 0..READERS {
            let mut r = r.clone();
            s.spawn(move || {
                let mut last_len = 0usize;
                loop {
                    let g = r.read();
                    assert!(
                        g.len() >= last_len,
                        "log went backwards ({} < {})",
                        g.len(),
                        last_len
                    );
                    for (i, v) in g.iter().enumerate() {
                        assert_eq!(*v, i as u64 + 1, "torn read");
                    }
                    last_len = g.len();
                    if last_len == TOTAL as usize {
                        break;
                    }
                }
            });
        }

        for b in 0..BATCHES {
            let batch: Vec<u64> = (0..OPS_PER_BATCH)
                .map(|i| b * OPS_PER_BATCH + i + 1)
                .collect();

            // 1. `sync()` before every `write()`: block until readers are
            //    finished with older values, handing them back to us. The
            //    previous round's published value becomes the alternate we
            //    absorb into.
            for old in w.sync() {
                // `write()` below may have already handed this value back, in
                // which case `sync()` returns nothing and we still hold it.
                debug_assert!(alternate.is_none());
                alternate = Some(old);
            }

            let a = alternate
                .as_mut()
                .expect("no alternate: `sync()` should have returned one");

            // The alternate is a stale snapshot: first absorb the operations
            // published since it was retired, then the new ones. Skipping
            // this would lose the interim operations.
            a.extend(unabsorbed.drain(..));
            a.extend(batch.iter().copied());

            // 2. Publish the alternate. It is now the value readers see.
            let published = alternate.take().expect("alternate vanished");
            for old in w.write(published) {
                debug_assert!(alternate.is_none());
                alternate = Some(old);
            }

            // The published value contains `unabsorbed ++ batch`; remember
            // those ops for the alternate's next rebase.
            unabsorbed.extend(batch);
        }
    });

    // Exactly one uncollected value remains: the last retired value, either
    // already handed back by the final `write()` (our alternate), or still
    // held by the writer until a final `sync()` collects it.
    let leftovers = w.sync().len() + usize::from(alternate.is_some());
    assert_eq!(leftovers, 1);
    assert_eq!(w.read().len(), TOTAL as usize);
    println!("left-right style: {BATCHES} batches, all {READERS} readers saw consistent snapshots");
}
