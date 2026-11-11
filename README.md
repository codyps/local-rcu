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
- [`arc-swap`](https://crates.io/crates/arc-swap): Atomically swappable `Arc`.
  Every read hands out an owned `Arc` (refcount inc/dec per read) instead of a
  borrow, so old values are freed as soon as the last reference drops (no grace
  period, but no deferred-free bookkeeping either). Includes a thread-local
  `Cache` to speed up repeated loads.
- [`hazarc`](https://crates.io/crates/hazarc): Wait-free `AtomicArc` mixing
  hazard pointers with atomic reference counting (explicitly inspired by
  `arc-swap`), supports `no_std` and is validated with `loom` and `miri`.
- [`aarc`](https://crates.io/crates/aarc): A family of atomically updatable
  `Arc` types with different read/write trade-offs.
- [`swmr-cell`](https://crates.io/crates/swmr-cell): Single-writer
  multiple-reader cell with wait-free reads and version-based garbage
  collection of old values.

## Compared to other rust "rcu" crates

Compared to `local-rcu` ...:

 - [`crossbeam-epoch`](https://crates.io/crates/crossbeam-epoch). Uses a global
   shared garbage queue. Has a seperate `Guard` concept (like
   `rcu_clean::graceful`) to allow lowering the cost of reads by delaying
   garbage collection more. Includes `miri` based validation (along with
   `loom`) 
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
 - [`rcu_cell`](https://crates.io/crates/rcu_cell). Docs have improved
   considerably in recent releases.
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
 - [`atom_box`](https://crates.io/crates/atom_box). Uses hazard pointers instead of RCU.
 - [`ebr`](https://crates.io/crates/ebr). Simple epoch-based reclamation
   with low cacheline ping-pong.
 - [`seize`](https://crates.io/crates/seize). Fast, predictable memory
   reclamation for concurrent data structures, hybridizing epoch and
   hazard-pointer techniques.
 - [`sdd`](https://crates.io/crates/sdd). Scalable lock-free delayed
   deallocation: memory is not tracked per-object, but reclaimed wholesale
   after a delay.
 - [`haphazard`](https://crates.io/crates/haphazard). Hazard pointers,
   a port of the implementation in facebook's `folly`.
 - [`arcu`](https://crates.io/crates/arcu). Arc based RCU using a pool of
   epoch counters to keep the read critical section short; replaced values
   are dropped automatically once the last reference disappears.
   Originated in [scryer-prolog](https://github.com/mthom/scryer-prolog/pull/1980).
 - [`rcu_128`](https://crates.io/crates/rcu_128). Keeps the value pointer and
   a tag in a single 128-bit atomic, providing wait-free reads without
   reference counting. Requires platform support for 128-bit atomics.
 - [`urcu`](https://crates.io/crates/urcu). Safe wrapper of the `memb`
   variant of the userspace RCU library (liburcu).
 - [`urcu2`](https://crates.io/crates/urcu2). A newer safe API to liburcu,
   including qsbr flavors.
 - [`read-copy-update`](https://crates.io/crates/read-copy-update). Provides a
   read-copy-update "locking" primitive.
 - [`rcurs`](https://crates.io/crates/rcurs). A small "oxidized" RCU
   implementation.
 - [`tokio_rcu`](https://crates.io/crates/tokio_rcu). RCU for async Rust with
   tokio (new & small as of 2026).
 - [`keepcalm`](https://crates.io/crates/keepcalm)
