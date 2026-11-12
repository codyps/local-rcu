# local-rcu

`local-rcu` provides a Read Copy Update (RCU) like concurency primitive that
optimizies reads and defers returning old values (ie: for dropping/freeing). It
provides a single writer, multiple reader data structure.

Readers don't block writers, slow readers just cause more memory usage (assuming
a continuously updating writer, old values are preserved until there are no
readers examining them).

Readers outside a read section don't block collection of old values. Readers inside
a read section may block collection of old values other than the one they are reading
with low probability.

Readers read a borrowed value (the Reader itself is refcounted, not the value being read). 
Writers provide an entire replacement value (as this is RCU-like).

## If I want behavior like X, what should I do?

### `left-right`

Example: [`examples/left_right.rs`](examples/left_right.rs)

1. Call `local_rcu::Writer::sync()` before every `local_rcu::Writer::write()` call
2. Modify the old value returned by `local_rcu::Writer::sync()` (ie: do your own
   `Absorb`) and pass it to the `Writer::write()` call.

Note: `Writer::new()` only takes 1 initial value, and `Writer::write()` will try
to return the old value if it can. This means there will need to be extra code
to track the "alternate" value that you'll be updating.

### `tokio::sync::watch`

Example: [`examples/watch.rs`](examples/watch.rs)

1. Pair an [`event-listener`](https://crates.io/crates/event-listener) with a `local_rcu::Writer`
2. Have each reader listen for events on `event-listener` to know to do another read.
3. Have the writer signal the `event-listener` **after** a new value is written with `local_rcu::Writer::write()`.

### `arcshift`, or a per-reader cache (`arc-swap`'s / `hazarc`'s `Cache`)

Wrap the reader in a [`local_rcu::cached::CachedReader`](https://docs.rs/local-rcu/latest/local_rcu/cached/struct.CachedReader.html).
It stays in its read section between reads, so reading a value that has not
changed costs one load and one comparison, with no store and no fence.

The price: between reads it holds back the value it last saw and every value
published since. A reader that goes idle should call `CachedReader::release()`
first (a plain `Reader` never holds anything back while idle).

## High Level Internal flow

A `slot` is the shared data structure represented by a single `Writer` and one or more `Readers`. 
A `Writer` is a struct that allows publishing new values (and also handles freeing old ones).
A `Reader` is a struct which allows reading published values.

For a given `slot`, there is state shared between the `Writer` and all its `Readers`. This shared
state is `Arc`'d, and internally it contains a list of per-`Reader` epochs (one per `Reader`)
which is protected by a `Mutex`. `Arc` ref-count changes only occur when cloning/creating more
`Readers`. Locking the epoch list occurs when creating more `Readers` and when the writer
publishes a value.

The `epoch` value modified by the `Reader` and read by the `Writer` is a `usize` which uses 1 bit
to indicate that the reader is currently reading a value (set — with `Release` — before loading
the value), and counts up when it finishes a read. When the `Reader` itself is dropped, the
epoch is set to a `DEAD` sentinel value (which effectively occupies another bit of our usize).

When the `Writer` publishes a value, it stores the new value (`Release`), places a `SeqCst`
fence, and then snapshots all the `Reader` epochs: every reader that appears to be in a read
section is tracked, together with a snapshot of its epoch, alongside the previous value (so
that value stays alive).

`Writer::write_nosync()` only does the first of those steps: it stores the new value and puts
the previous one aside, without a fence and without looking at any reader. The fence and the
snapshot happen at the next `Writer::try_sync()` (or `Writer::write()`), once for all the values
replaced since the last snapshot. They are tracked together as one batch. This makes a
`write_nosync()` about as cheap as a pointer store, and leaves the readers' epochs alone
between snapshots. In return, a reader that is in a read section during the snapshot holds back
the whole batch, including values that were replaced while nobody was reading.

Each time the `Writer` is updated with `Writer::write` (or via explicit request like
`Writer::try_sync()` or `Writer::sync()`), the `Writer` re-examines these previous values: a
value can be freed once every tracked reader's epoch has moved past its snapshot (or the reader
is `DEAD`) — that reader has finished the read section it was tracked in. Only then is the
`Box` re-formed from the internally held raw pointer and handed back.

Why this is sound:

- Each read section enters by storing its epoch with the "reading" flag set, then a `SeqCst`
  fence, then loading the value pointer. The `Writer` mirrors this: it stores the new pointer,
  places a `SeqCst` fence, then scans the epochs. All `SeqCst` fences fall into a single total
  order, so for any read section one of two things holds:
  - The reader's fence comes first. The `Writer`'s scan then sees the reader as reading, so a
    section that might still load the old value gets tracked.
  - The `Writer`'s fence comes first. The reader's load then returns the new value (or a newer
    one), so it cannot load the old value.
  The same holds for a batch: every value in it was replaced before the `Writer`'s fence, so a
  reader whose fence comes later loads something newer than all of them.
- Because of the second case, once the scan has seen a reader in some read section, none of
  that reader's later sections can load the old value. When the tracked epoch changes, the
  value can be freed.
- Every epoch store the `Reader` makes (entering a section, leaving it, and the `DEAD` mark) is
  `Release`, and every epoch load the `Writer` makes is `Acquire`. So whenever the `Writer`
  decides a value can be freed, all of the reader's earlier accesses to it happen-before the
  free.

To avoid synchronization, we bias towards keeping values alive longer (which is fine and correct):
- When the `Writer` scans the epochs it has already published a new value, so some of the
  readers may already be reading the new value. We still track them until their epoch moves.
- Collecting old values is deferred to the next `write()`/`try_sync()`, so a value can outlive
  the last reader that observed it by a little.
- Epochs may wrap around. The `Writer` only compares an epoch with its snapshot for equality.
  If a reader's epoch wraps back to exactly the snapshot value, the `Writer` treats that reader
  as still reading and keeps the old value a little longer, until the epoch moves on.


## Alternatives

Comparisons of `local-rcu` against alternative and other rust "rcu" crates can
be found in [`COMPARISONS.md`](COMPARISONS.md).

