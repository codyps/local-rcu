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
