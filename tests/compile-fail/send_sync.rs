// Check that non-sync types can't be sent between threads via rcu
// edition:2021

use std::cell::RefCell;
use std::thread;

// NOTE: we include this so the error points to local source which gives reliable trybuild output
fn spawn<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    thread::spawn(f).join().unwrap();
}

fn main() {
    let (w, mut r1) = local_rcu::slot(RefCell::new(0));

    let t1 = spawn(move || {
        *r1.read().borrow_mut() = 1;
    });
    //~^ ERROR: `RefCell<i32>` cannot be shared between threads safely

    assert_eq!(*w.read().borrow(), 0);
}
