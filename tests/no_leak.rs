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
            vals.iter()
                .map(|v| Arc::strong_count(v))
                .collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert_eq!(**g1, 1);

        let mut r2 = w.reader();
        let g2 = r2.read();
        w.write(Box::new(vals[2].clone()));
        assert_eq!(
            vals.iter()
                .map(|v| Arc::strong_count(v))
                .collect::<Vec<_>>(),
            [2, 2, 2]
        );
        assert_eq!(**g2, 2);
    }

    assert_eq!(
        vals.iter()
            .map(|v| Arc::strong_count(v))
            .collect::<Vec<_>>(),
        [1, 1, 1]
    );
}
