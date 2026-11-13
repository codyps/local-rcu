use local_rcu::slot;
use std::sync::Arc;

#[test]
fn no_leak() {
    let vals = [Arc::new(1), Arc::new(2), Arc::new(3)];

    {
        let (mut w, mut r1) = slot(vals[0].clone());

        let g1 = r1.read();
        w.write(Box::new(vals[1].clone()));
        assert_eq!(
            vals.iter().map(Arc::strong_count).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert_eq!(**g1, 1);

        let mut r2 = w.reader();
        let g2 = r2.read();
        w.write(Box::new(vals[2].clone()));
        assert_eq!(
            vals.iter().map(Arc::strong_count).collect::<Vec<_>>(),
            [2, 2, 2]
        );
        assert_eq!(**g2, 2);
    }

    assert_eq!(
        vals.iter().map(Arc::strong_count).collect::<Vec<_>>(),
        [1, 1, 1]
    );
}

/// `write_nosync()` does not look at readers; `try_sync()` tracks everything replaced since the
/// last scan as one batch.
#[test]
fn write_nosync_batches() {
    let vals: Vec<_> = (0..5).map(Arc::new).collect();
    let counts = |vals: &[Arc<i32>]| vals.iter().map(Arc::strong_count).collect::<Vec<_>>();

    {
        let (mut w, mut r) = slot(vals[0].clone());

        // nobody is reading: the first scan returns everything that was replaced
        w.write_nosync(Box::new(vals[1].clone()));
        w.write_nosync(Box::new(vals[2].clone()));
        assert!(w.has_old_values());
        assert_eq!(counts(&vals), [2, 2, 2, 1, 1]);
        let back = w.try_sync();
        assert_eq!(back.iter().map(|b| ***b).collect::<Vec<_>>(), [0, 1]);
        drop(back);
        assert!(!w.has_old_values());
        assert_eq!(counts(&vals), [1, 1, 2, 1, 1]);

        // a reader in a read section holds back the whole batch, until it leaves the section
        let g = r.read();
        assert_eq!(**g, 2);
        w.write_nosync(Box::new(vals[3].clone()));
        w.write_nosync(Box::new(vals[4].clone()));
        assert!(w.try_sync().is_empty());
        assert!(w.try_sync().is_empty());
        assert_eq!(**g, 2);
        assert_eq!(counts(&vals), [1, 1, 2, 2, 2]);
        drop(g);
        let back = w.try_sync();
        assert_eq!(back.iter().map(|b| ***b).collect::<Vec<_>>(), [2, 3]);
        drop(back);
        assert!(!w.has_old_values());
        assert_eq!(**r.read(), 4);

        // never collected: freed with the slot
        w.write_nosync(Box::new(vals[0].clone()));
        w.write_nosync(Box::new(vals[1].clone()));
    }

    assert_eq!(counts(&vals), [1, 1, 1, 1, 1]);
}

/// `write_into()` and `try_sync_into()` append to a `Vec` the caller keeps.
#[test]
fn write_into_appends() {
    let (mut w, mut r) = slot(0u32);
    let mut old = Vec::new();

    w.write_into(Box::new(1), &mut old);
    assert_eq!(old.iter().map(|b| **b).collect::<Vec<_>>(), [0]);

    // a guard holds 1 back: nothing is added, and what is there is left alone
    let g = r.read();
    w.write_into(Box::new(2), &mut old);
    w.try_sync_into(&mut old);
    assert_eq!(old.len(), 1);
    assert_eq!(*g, 1);
    drop(g);

    w.try_sync_into(&mut old);
    assert_eq!(old.iter().map(|b| **b).collect::<Vec<_>>(), [0, 1]);
    assert!(!w.has_old_values());
}

/// More readers in a read section than a batch tracks inline, and more values in a batch
/// than it holds inline.
#[test]
fn many_readers_and_values_per_batch() {
    let vals: Vec<_> = (0..40).map(Arc::new).collect();
    let live = |vals: &[Arc<i32>]| vals.iter().filter(|v| Arc::strong_count(v) > 1).count();

    {
        let (mut w, r) = slot(vals[0].clone());
        let mut readers: Vec<_> = (0..9).map(|_| r.clone()).collect();
        let mut guards: Vec<_> = readers.iter_mut().map(|r| r.read()).collect();

        // one value at a time, nine readers tracked for each
        for v in &vals[1..10] {
            assert!(w.write(Box::new(v.clone())).is_empty());
        }
        // then batches of ten values
        for v in &vals[10..30] {
            w.write_nosync(Box::new(v.clone()));
            if **v % 10 == 9 {
                assert!(w.try_sync().is_empty());
            }
        }
        assert_eq!(live(&vals), 30);

        // the guards go one by one: nothing comes back before the last
        while guards.len() > 1 {
            guards.pop();
            assert!(w.try_sync().is_empty());
        }
        guards.pop();
        assert_eq!(w.try_sync().len(), 29);
        assert!(!w.has_old_values());
        assert_eq!(live(&vals), 1);
        drop(guards);

        // and again, now that the pools hold the allocations of those batches
        let mut guards: Vec<_> = readers.iter_mut().map(|r| r.read()).collect();
        for v in &vals[30..40] {
            w.write_nosync(Box::new(v.clone()));
        }
        assert!(w.try_sync().is_empty());
        guards.clear();
        assert_eq!(w.try_sync().len(), 10);
    }

    assert_eq!(live(&vals), 0);
}
