# local-rcu

`local-rcu` provides a Read Copy Update (RCU) like concurency primitive that
optimizies reads and defers returning old values (ie: for dropping/freeing). It
provides a single writer, multiple reader data structure.

Readers don't block writers, slow readers just cause more memory usage (assuming
a continuously updating writer, old values are preserved until there are no
readers examining them).

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

## Alternatives:

Compared to `local-rcu` ...:

- [`triple_buffer`](https://crates.io/crates/triple_buffer): Uses atomic
  swaps. Allows immediate data free/reuse. Single Consumer.
- `std::sync::mpsc::channel`: MPSC (vs SPMC), multiple values (not just latest) are readable
- [`left-right`](https://crates.io/crates/left-right): Uses single "old" value
  instead of having arbitrary numbers of old values. Slow readers can prevent
  new values from being published. It requires implementing a concept of
  "Absorb" to place modification of the "old" value into `left-right`
  internals. Uses an "operation log" so that writers don't block on readers
  directly (only publishing is blocked).
- [`tokio::sync::watch`](https://docs.rs/tokio/latest/tokio/sync/watch/index.html):
  Allows being notified of updates via async, uses a mutex to control access to
  the value. Readers block writers.
- [`tokio::sync::broadcast`](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html):
  supports multiple producers, provides some "history" (previous values) instead
  of only the most recent value, supports async waiting for new values, uses
  Mutexes internally.

## Compared to other rust "rcu" crates

Compared to `local-rcu` ...:

 - [`rcu-clean`](https://crates.io/crates/rcu-clean). Does not include any
   tests using `loom` or other checking tool. Provides multiple mechanisms for
   freeing old values ("graceful": a seperated global Arc based method,
   "arcrcu": a custom Arc-like for each value, and some not shareable between
   threads: "boxrcu" & "rccru"). The methods that allow sharing between threads
   both require a reference count to be incremented/decremented. "graceful"
   allows deferring that atomic ref count inc/dec over multiple reads (lowering
   read cost) by delaying freeing of old data. Old values are not returned to
   the caller. In "graceful" variant, writer can briefly block readers which
   want to create a new "Grace" while an update is occuring. In "arcrcu",
   readers aren't blocked by the writer. Internally uses a custom singly linked
   list to queue old values for freeing. In "arcrc", `clean()` must be called
   manually, no automatic cleaning exists. 
 - [`rcu_cell`](https://crates.io/crates/rcu_cell). Missing most documentation.
   Uses atomic compare and exchange. Readers block writers (briefly, before a
   reference is incremented, a bit in the value pointer is used as a lock).
   Allows multiple writers (uses a compare and exchange determines which writer
   is published). Includes a custom ARC-like reference counted interface, which
   uses atomic add and sub. Each written value includes the reference count
   adjacent to the value. Every read does a increment and decrement to the
   reference count. Does not include any tests using
   [`loom`](https://crates.io/crates/loom/) or other checking tool. "Locking"
   of data for reading done by having readers set a bit in the pointer via
   compare exchange. In other words: all readers and writers are performing
   compare exchange on a shared location, which may result in contention on the
   cache line containing the pointer.
