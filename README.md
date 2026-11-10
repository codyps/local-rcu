# local-rcu

`local-rcu` provides a Read Copy Update (RCU) like concurency primitive that
optimizies reads and defers returning old values (ie: for dropping/freeing). It
provides a single writer, multiple reader data structure.

Readers don't block writers, slow readers just cause more memory usage (assuming
a continuously updating writer, old values are preserved until there are no
readers examining them).

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
state is `Arc`'d, and internally it contains a list of `Reader`-epochs (one per `Reader`) which is
protected by a `Mutex`. `Arc` ref-count changes only occur when cloning/creating more `Readers`.
Locking the epoch list occurs when creating more `Readers` and when the writer publishes a value.

The `epoch` value modified by the `Reader` and read by the `Writer` is a `usize` which uses 1 bit
to indicate that it is currently reading a value (sets it before reading), and counts up when it
finishes a read. When the `Reader` itself is dropped, it sets the epoch to a `DEAD` sentinel value.

When the `Writer` publishes a value, it exposes the new value in the shared data, and then snapshots
all the `Reader` epochs and tracks the snapshot, the `Reader` itself (so we can later read its current epoch),
and the previous value (so that it stays alive).

Each time `Writer` is updated with `Writer::write` (or via explicit request like `Writer::try_sync()` or
`Writer::sync()`), the `Writer` examines these previous values checking if the current `epoch` published
by the `Readers` is past the epoch of that reader when it was snapshotted. When a previous value has no more
live readers, we drop the value.

To avoid synchronization, we bias towards keeping values alive longer (which is fine and correct):
 - When the `Writer` snapshots the epochs it has already published a new value, so some of the `Reader` epochs it reads may actually be reading the new value (ie: those readers already moved on). We still count them like they are reading the old value because the `Writer` can't tell.

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
