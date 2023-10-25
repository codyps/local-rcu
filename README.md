# local-rcu

`local-rcu` provides a Read Copy Update (RCU) like concurency primitive that
optimizies reads and defers returning old values (ie: for dropping/freeing). It
provides a single writer, multiple reader data structure.

Readers don't block writers, slow readers just cause more memory usage (assuming
a continuously updating writer, old values are preserved until there are no
readers examining them).

## Alternatives:

Compared to `local-rcu`:

- [`triple_buffer`](https://crates.io/crates/triple_buffer): requires atomic
  swaps. Allows immediate data free/reuse. Single Consumer.
- `std::sync::mpsc::channel`: MPSC (vs SPMC), multiple values (not just latest) are readable
- [`left-right`](https://crates.io/crates/left-right): Uses single "old" value
  instead of having arbitrary numbers of old values. This means readers can block
  the writer. It requires implementing a concept of "Absorb" to place modification
  of the "old" value into `left-right` internals.
- [`tokio::sync::watch`](https://docs.rs/tokio/latest/tokio/sync/watch/index.html):
  Allows being notified of updates via async, uses a mutex to control access to
  the value. Readers block writers.
- [`tokio::sync::broadcast`](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html):
  supports multiple producers, provides some "history" (previous values) instead
  of only the most recent value, supports async waiting for new values, uses
  Mutexes internally.

## If I want behavior like X, what should I do?

### `left-right`

1. Call `local_rcu::Writer::sync()` before every `local_rcu::Writer::write()` call
2. Modify the old value returned by `local_rcu::Writer::sync()` (ie: do your own
   `Absorb`) and pass it to the `Writer::write()` call.

Note: `Writer::new()` only takes 1 initial value, and `Writer::write()` will try
to return the old value if it can. This means there will need to be extra code
to track the "alternate" value that you'll be updating.

### `tokio::sync::watch`

1. Pair an [`event-listener`](https://crates.io/crates/event-listener) with a `local_rcu::Writer`
2. Have each reader listen for events on `event-listener` to know to do another read.
3. Have the writer signal the `event-listener` **after** a new value is written with `local_rcu::Writer::write()`.
