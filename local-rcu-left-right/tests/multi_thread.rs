//! Multi-threaded consistency checks: readers must always observe the full
//! effect of published batches, never a partially applied one.

use local_rcu_left_right::{Absorb, WriteHandle};
use std::thread;

#[derive(Clone)]
struct Push(u64);

impl Absorb<Push> for Vec<u64> {
    fn absorb(&mut self, op: Push) {
        self.push(op.0);
    }
}

const BATCHES: u64 = 100;
const OPS_PER_BATCH: u64 = 10;
const TOTAL: u64 = BATCHES * OPS_PER_BATCH;
const READERS: usize = 4;

#[test]
fn readers_see_consistent_prefixes() {
    let (mut w, r) = WriteHandle::<Vec<u64>, Push>::new();

    thread::scope(|s| {
        for _ in 0..READERS {
            let mut r = r.clone();
            s.spawn(move || {
                let mut last_len = 0usize;
                // enter() also yields None once the writer drops; the writer
                // here outlives the loop, so that only happens on success
                while let Some(g) = r.enter() {
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
                assert_eq!(last_len, TOTAL as usize);
            });
        }

        for b in 0..BATCHES {
            for i in 0..OPS_PER_BATCH {
                w.append(Push(b * OPS_PER_BATCH + i + 1));
            }
            w.publish();
        }
    });

    assert_eq!(w.read().len(), TOTAL as usize);
}

#[test]
fn many_publishes_without_appends() {
    // publishing with an empty log must not corrupt state
    let (mut w, mut r) = WriteHandle::<Vec<u64>, Push>::new();
    w.append(Push(1));
    w.publish();
    w.publish();
    w.publish();
    assert_eq!(&**r.enter().unwrap(), &[1]);
    w.append(Push(2));
    w.publish();
    assert_eq!(&**r.enter().unwrap(), &[1, 2]);
}

#[test]
fn relaxed_publish_never_takes_the_working_copy_back() {
    let (mut w, mut r) = WriteHandle::<Vec<u64>, Push>::new();
    w.append(Push(1));
    w.publish_relaxed();
    assert_eq!(&**r.enter().unwrap(), &[1]);
    w.append(Push(2));
    w.publish_relaxed();
    assert_eq!(&**r.enter().unwrap(), &[1, 2]);
    // the working copy keeps accumulating without any rebasing
    w.append(Push(3));
    w.publish_relaxed();
    assert_eq!(&**r.enter().unwrap(), &[1, 2, 3]);
    assert_eq!(w.read().len(), 3);
}

#[test]
fn writer_drop_signals_readers() {
    let (mut w, mut r) = WriteHandle::<Vec<u64>, Push>::new();
    let mut r2 = r.clone();

    w.append(Push(1));
    w.publish();
    assert!(!r.is_closed());
    assert_eq!(&**r.enter().unwrap(), &[1]);

    // dropping the writer (or ending the scope owning it) closes the slot
    drop(w);
    assert!(r.is_closed());
    assert!(r.enter().is_none());
    assert!(r2.enter().is_none(), "cloned readers see closure too");
}

#[test]
fn api_parity_with_left_right() {
    // the same sequence driven through the real left-right, for comparison
    use left_right::Absorb as LrAbsorb;

    #[derive(Clone)]
    struct LrPush(u64);
    impl LrAbsorb<LrPush> for Vec<u64> {
        fn absorb_first(&mut self, op: &mut LrPush, _: &Self) {
            self.push(op.0);
        }
        fn absorb_second(&mut self, op: LrPush, _: &Self) {
            self.push(op.0);
        }
        fn drop_first(self: Box<Self>) {}
        fn sync_with(&mut self, first: &Self) {
            *self = first.clone();
        }
    }

    let (mut lw, lr) = left_right::new::<Vec<u64>, LrPush>();
    lw.append(LrPush(1));
    lw.publish();
    lw.append(LrPush(2));
    lw.publish();

    let (mut w, mut r) = WriteHandle::<Vec<u64>, Push>::new();
    w.append(Push(1));
    w.publish();
    w.append(Push(2));
    w.publish();

    assert_eq!(&**r.enter().unwrap(), &*lr.enter().unwrap());
}
