// Check that non-sync types can't be sent between threads via rcu
// edition:2021

use std::cell::RefCell;
use std::thread;

fn main() {
    let (w, mut r1) = local_rcu::slot(RefCell::new(0));

    let t1 = thread::spawn(move || {
        *r1.read().borrow_mut() = 1;
    });
    //~^ ERROR: `RefCell<i32>` cannot be shared between threads safely

    t1.join().unwrap();

    assert_eq!(*w.read().borrow(), 0);
}
