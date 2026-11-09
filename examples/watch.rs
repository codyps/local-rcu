//! Emulate [`tokio::sync::watch`]-style notifications using `local-rcu`.
//!
//! `tokio::sync::watch` stores the latest value and lets tasks `await`
//! changes to it. `local-rcu` provides only the value half (and unlike
//! `watch`, reads are wait-free and readers never block the writer), so pair
//! it with [`event-listener`] for the notification half:
//!
//! 1. share an [`Event`](event_listener::Event) alongside the
//!    [`Writer`](local_rcu::Writer)
//! 2. have readers `listen()` on the event, then [`read()`](local_rcu::Reader::read)
//!    the new value
//! 3. have the writer signal the event _after_
//!    [`write()`](local_rcu::Writer::write) publishes the new value
//!
//! As with `watch`, a notification means "the value changed", not "here is
//! the value": after waking, always read the latest value (it may already be
//! newer than the one that triggered the notification). Register interest
//! _before_ checking the value, so a notification can't slip past between
//! the check and the wait.
//!
//! This example uses `EventListener::wait()` from plain threads; in async
//! code, `.await` the listener instead.
//!
//! [`tokio::sync::watch`]: https://docs.rs/tokio/latest/tokio/sync/watch/index.html
//! [`event-listener`]: https://crates.io/crates/event-listener

use event_listener::{Event, Listener};
use local_rcu::slot;
use std::thread;

const WRITES: u64 = 100;
const READERS: usize = 4;

fn main() {
    let (mut w, r) = slot(0u64);

    // 1. Pair the writer with an `event-listener` event.
    let changed = Event::new();

    thread::scope(|s| {
        for _ in 0..READERS {
            let mut r = r.clone();
            let changed = &changed;
            s.spawn(move || {
                let mut last = 0u64;
                loop {
                    // 2. Register interest _before_ checking the value: a
                    //    notification for a listener that already exists
                    //    can't be missed, while one sent before registering
                    //    would be lost.
                    let listen = changed.listen();

                    let v = *r.read();
                    if v > last {
                        last = v;
                        // ... react to the new value here ...
                    }
                    if last == WRITES {
                        break;
                    }

                    listen.wait();
                }
            });
        }

        for i in 1..=WRITES {
            w.write(Box::new(i));
            // 3. Signal after the write: everyone woken up here will observe
            //    the new value (or a newer one).
            changed.notify(usize::MAX);
        }
    });

    println!("watch style: {WRITES} updates, {READERS} readers saw them all");
}
