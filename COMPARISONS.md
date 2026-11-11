# Comparisons

How `local-rcu` compares to alternative crates.

Everything here was collected by reading the published source and docs of the
crate version listed in the [last table](#implementation-validation--maturity)
(as of 2026-11-11). Corrections are welcome.

## At a glance

Legend: ✓ yes · ✗ no · ~ partial (see the notes) · — not applicable.
`local-rcu` is included as the baseline row. Rows are grouped: general purpose
alternatives, single value cells, then reclamation frameworks (where you build
the cell yourself).

### Access pattern, read path & blocking

| Crate | Topology | Read path | A read hands out | Writer blocked by readers | Readers blocked by writer | Async updates |
|---|---|---|---|---|---|---|
| `local-rcu` | SPMC | per-reader epoch store + `SeqCst` fence + pointer load; no refcount, no CAS, no writes to shared cachelines | borrow (`ReadGuard`) | no (opt-in via `sync()`) | no | no — pair `event-listener` (README recipe) |
| `RwLock<Arc<T>>` / `Arc<RwLock<T>>` (std) | MPMC | lock acquire (a write to a shared cacheline), optionally + `Arc` clone | lock guard, or owned `Arc` | yes, while a read guard is held | yes, while the write guard is held | no (`tokio::sync::RwLock`: yes) |
| `std::sync::mpsc::channel` | MPSC | queue `recv` | owned `T` (each value is delivered once) | — | — | no |
| `tokio::sync::watch` | MPMC (`Sender` is `Clone`) | `RwLock` read lock | borrow (`Ref`, holds the read lock) | yes, while a `Ref` is held | yes (briefly, during `send`) | yes (`changed()`) |
| `tokio::sync::broadcast` | MPMC | tail mutex + slot lock + clone | owned clone of `T` | no (slow receivers get `Lagged` instead) | briefly (internal locks) | yes |
| `triple_buffer` | SPSC | wait-free: one atomic swap when an update is pending, otherwise a load | borrow (`&T`; `&mut T` also available) | no | no | no |
| `left-right` | SPMC | per-reader epoch `fetch_add` on enter and leave + `SeqCst` fence + pointer load | borrow (`ReadGuard`) | yes — `publish()` waits for every reader to leave the old copy | no | no |
| `swmr-cell` | SPMC | per-reader version store + barrier (fence, or a "light" barrier in read-preferred mode) + pointer load | borrow (`PinGuard`) | no | no | no |
| `read-copy-update` | SPMC | pointer load + per-reader epoch store; nested reads on one `Reader` are just a counter bump | borrow (`Guard`) | no | no | no |
| `arc-swap` | MPMC | `load()`: thread-local debt slot (hazard-pointer like), no refcount unless the slots run out. `load_full()`: adds a refcount increment | `Guard` (Arc-like), or owned `Arc` | no (lock-free; `swap` pays off outstanding debts) | no (wait-free) | no |
| `hazarc` | MPMC | wait-free: thread-local hazard slot + pointer load; refcount only on the fallback path | `ArcBorrow`, or owned `Arc` | no (wait-free `swap` if the thread count is bounded) | no | no |
| `aarc` | MPMC | protected load via `fast-smr`; reference counts are not touched | `Guard` (non-counting; convertible to `Arc`) | no | no | no |
| `arcshift` | MPMC (any handle may update) | `get(&mut self)`: check whether a newer version exists, then deref; a stale handle first advances along a version list | borrow (`&T`) | no | no | no |
| `arcu` | MPMC | epoch counter increment ×2 + `Arc` refcount increment | owned `Arc` (`RcuRef`) | yes, briefly — waits for in-flight reads (which never run user code) | no | no |
| `rcu_cell` | MPMC | `fetch_add` on the shared pointer word (reader count in the low bits) + `Arc` refcount increment + `fetch_sub` | owned `Arc` | yes, briefly — CAS waits for the reader count to reach zero | no | no |
| `rcu_128` | MPMC | `fetch_add` on a shared 128-bit word (pointer + reader count); guard drop is a CAS loop | borrow (`RcuGuard`) | **yes** — `write()` spins until every reader of the old value is done | no | no |
| `rcurs` | MPMC | pointer load + refcount increment in the value | guard (holds a refcount) | no | no | no |
| `rcu-clean` | `BoxRcu`/`RcRcu`: single thread. `ArcRcu`: one writer at a time (panics otherwise). `graceful`: MPMC | `ArcRcu`: pointer load + a borrow count increment once per handle (until `clean()`). `graceful`: pointer load under a `Grace` | borrow (`Deref` / `RcuGuard`) | no | `graceful`: briefly (creating a new `Grace`). `ArcRcu`: no | no |
| `atom_box` | MPMC | hazard pointer acquire (linear in threads × boxes) + store + validate | `LoadGuard` (holds a hazard pointer) | no | no | no |
| `basedrop::SharedCell` | MPMC | shared reader counter `fetch_add`/`fetch_sub` + refcount increment | owned `Shared<T>` | yes, briefly — `set`/`replace` spin until no `get()` is in flight | no | no |
| `concread::CowCell` | MPMC (writers serialised) | mutex lock + `Arc` clone | owned `Arc` (`CowCellReadTxn`) | no | briefly (mutex around the active pointer) | yes (`cowcell::asynch`, tokio) |
| `seqlock` | MPMC (writers serialised) | optimistic: read sequence, copy, re-check, retry | owned copy (`T: Copy`) | no | readers retry while a write is in progress | no |
| `keepcalm` (`SharedMut::new_rcu`) | MPMC | `parking_lot::RwLock` read lock + `Arc` clone | owned `Arc` (read guard) | briefly (lock held only to clone/replace the `Arc`) | briefly | experimental (`read_async`/`write_async`) |
| `tokio_rcu` (`RcuBox`) | MPMC | a single atomic pointer load, inside `rcu_read_lock` (cannot be held across `.await`) | borrow tied to the read-lock guard | no | no | yes — `swap(..).wait().await` yields the old value after a grace period |
| `crossbeam-epoch` | framework | `pin()`: thread-local epoch store + `SeqCst` fence (amortized over nested pins), then pointer loads | `Shared<'g, T>` tied to a `Guard` | no | no | no |
| `ebr` | framework | `pin()` thread-local epoch guard | guard (`defer_drop` only; no pointer type) | no | no | no |
| `seize` | framework | `enter()` guard + protected load | pointer tied to a guard | no | no | no |
| `sdd` | framework | `Guard::new()` (thread-local epoch) + load | `Ptr<'g, T>` tied to a `Guard` | no | no | no |
| `haphazard` | framework | hazard pointer: store + barrier + re-validate load | `&T` tied to a `HazardPointer` | no | no | no |
| `kovan` | framework (+ `Atom<T>` cell) | "one atomic load & one comparison" under a pin | `AtomGuard` | no (wait-free with native 128-bit atomics) | no | no |
| `urcu` | framework (FFI) | liburcu `memb` read-side critical section (`rscs`) on a registered thread | `RcuRef` tied to the critical section | no | no | no |
| `urcu2` | framework (FFI) | liburcu `rcu_read_lock` (bp / mb / memb / qsbr flavors) on a registered thread | reference tied to the read guard | no | no | no |

### Old-value reclamation

| Crate | Technique | Old values kept | Freed when | Old value returned to writer | Manual steps |
|---|---|---|---|---|---|
| `local-rcu` | per-reader epoch grace periods, tracked per old value | arbitrary — until each reader that might see it moves its epoch; a reader that stays in one read section holds back every value replaced meanwhile | on a later `write()`/`try_sync()` after grace; idle readers hold nothing back | yes (`Vec<Box<T>>`) | none |
| `RwLock<Arc<T>>` / `Arc<RwLock<T>>` (std) | lock / refcount | none beyond live `Arc`s | in place, or when the last `Arc` drops (on whichever thread that is) | yes (`mem::replace`) | none |
| `std::sync::mpsc::channel` | queue ownership | all unread values | on receive | — (values are delivered) | none |
| `tokio::sync::watch` | value replaced in place under the lock | latest only | at `send` | yes (`send_replace`) | none |
| `tokio::sync::broadcast` | bounded ring buffer | bounded history (capacity) | on overwrite | no | none |
| `triple_buffer` | three fixed buffers, indices swapped atomically | exactly 3 buffers | never — buffers are reused in place | the writer gets a stale buffer back to overwrite (`input_buffer_mut`) | none (reset the input buffer before reuse) |
| `left-right` | two copies + operation log | exactly 1 alternate copy (so 2× memory) | — (the alternate is updated, not freed) | yes — the alternate is modified in place by replaying the log | implement `Absorb`; every operation is applied twice |
| `swmr-cell` | version based: readers publish the version they pinned | arbitrary — everything at or after the minimum pinned version | on `store()` once a threshold (default 16) of garbage is queued, or `collect()` | no (dropped by the writer; `previous()` gives `&T`) | none (`collect()` optional) |
| `read-copy-update` | per-reader epoch, minimum across readers | arbitrary — everything at or after the oldest reader epoch; an **idle** reader keeps pinning what it last read | on `write()` / `try_reclaim()` | no (dropped by the writer) | none |
| `arc-swap` | refcounting + hazard-like debts | none beyond live `Arc`s/`Guard`s | last reference drops — no grace period (on whichever thread drops last) | yes (`swap` returns the old `Arc`) | none |
| `hazarc` | hazard pointers + refcounting | none beyond live references | last reference drops | yes (`swap` returns the old `Arc`) | none (domain cleanup in `no_std`) |
| `aarc` | `fast-smr` deferred reclamation + refcounting | until no guard protects them | deferred, after the last reference/guard | yes (`swap` returns the old `Arc`) | none |
| `arcshift` | linked list of versions, refcounted per node | bounded: only versions still referenced by a live handle | when no handle references the version; handles advance on `get`/`reload` | no | handles need `&mut` access (or `reload()`) to move forward cheaply |
| `arcu` | `Arc` + epoch counter pool | none beyond live `Arc`s | last `Arc` drops | yes (`replace` returns the old `Arc`) | none |
| `rcu_cell` | `Arc` + reader count packed into the pointer word | none beyond live `Arc`s | last `Arc` drops | yes (`write`/`take` return the old `Arc`) | none |
| `rcu_128` | reader count packed next to the pointer | at most 1 pending (a single "to clear" slot; grace periods are serialised) | by the writer, before `write()` returns | no | none |
| `rcurs` | refcount stored with the value | none beyond live guards | last guard drops | no | none |
| `rcu-clean` | `graceful`: global list tied to live `Grace`s. `ArcRcu`/`RcRcu`/`BoxRcu`: per-value linked list | `graceful`: until every older `Grace` is dropped. Others: until `clean()` | `graceful`: delayed. Others: only in `clean()` | no — never returned | `clean()` (needs `&mut`), no automatic cleaning |
| `atom_box` | hazard pointers | retired list, linear in the number of hazard pointers | once no hazard pointer covers it | yes (`swap` returns a `StoreGuard`) | none |
| `basedrop::SharedCell` | refcount + deferred drop queue | none beyond live `Shared`s | queued when the last `Shared` drops; actually freed on the thread that runs `Collector::collect()` | yes (`replace` returns the old `Shared`) | run a `Collector` somewhere |
| `concread::CowCell` | `Arc` | none beyond live read transactions | last `Arc` drops (`EbrCell` variant: via `crossbeam-epoch`) | no | none |
| `seqlock` | none — value overwritten in place | none | — | — | none |
| `keepcalm` (`SharedMut::new_rcu`) | `Arc` | none beyond live `Arc`s | last `Arc` drops | no | none |
| `tokio_rcu` (`RcuBox`) | quiescent-state based: every tokio worker passes `on_after_task_poll` | until the grace period ends | when the grace period ends | yes (`swap(..).wait().await` returns the `Box`) | run inside its runtime setup (`rcu_block_on`; needs `tokio_unstable`) |
| `crossbeam-epoch` | epoch based: thread-local bags + a global queue | unbounded while any thread stays pinned | deferred — two epoch advances after retirement, on whichever thread collects | no (`defer_destroy`) | retire via the guard |
| `ebr` | epoch based, thread-local bags | bags of `SLOTS` (default 128) items | after the epoch advances | no (`defer_drop`) | none |
| `seize` | Hyaline: retired batches are refcounted across the threads active at retire time | batches (default 32 per thread) | when the last active thread leaves — predictable, no polling | no (`retire`) | retire explicitly |
| `sdd` | epoch based variant; retired values kept in thread-local storage | until unreachable from any guard | deferred, on a later guard/collector pass | yes (`swap` returns `Owned`/`Shared`) | none with the managed pointer types |
| `haphazard` | hazard pointers (folly port) | bounded by the hazard pointer count | on a later reclaim pass once no hazard pointer covers it | no (`retire`) | retire explicitly; `Domain` management |
| `kovan` | wait-free reclamation with bounded memory | bounded; a thread without a live guard holds nothing back | after the reservation ends (`flush()` available) | yes (`Atom::swap` returns `Removed<T>`) | none with `Atom<T>` |
| `urcu` | liburcu grace periods (`memb`) | until a grace period passes | deferred after the old `RcuBox` is dropped (`call_rcu`) | yes (`update` returns the old `RcuBox`) | register each thread |
| `urcu2` | liburcu grace periods (any flavor) | until a grace period passes | `RcuRef::take_ownership` (waits), or deferred cleanup on drop | yes (`take_ownership`) | register each thread; qsbr needs quiescent-state reports |

### Implementation, validation & maturity

`loom` / `miri` are ticked when the published crate runs them in its CI
configuration or ships tests for them.

| Crate | Version | Internal locking | `no_std` | `loom` | `miri` | Notes |
|---|---|---|---|---|---|---|
| `local-rcu` | 1.0.8 | `Mutex` on the epoch list — publish + reader creation only | ✗ | ✓ | ✓ | baseline; README documents the soundness argument |
| `RwLock<Arc<T>>` / `Arc<RwLock<T>>` (std) | — | the lock itself | ✗ | — | — | the usual starting point |
| `std::sync::mpsc::channel` | — | std internals | ✗ | — | — | queue semantics, not a slot |
| `tokio::sync::watch` | 1.54 | `RwLock` around the value | ✗ | ✓ | ✓ | latest-value channel |
| `tokio::sync::broadcast` | 1.54 | `Mutex` on the tail + one per slot | ✗ | ✓ | ✓ | bounded history; values cloned |
| `triple_buffer` | 9.0.1 | none (atomics only) | ✓ (`alloc`) | ✗ | ✓ | exactly one consumer |
| `left-right` | 0.11.8 | `Mutex` on the epoch slab — publish + reader creation | ✗ | ✓ | ✓ | also ASan/LSan; requires implementing `Absorb` |
| `swmr-cell` | 0.3.1 | `Mutex` on the reader list — reader creation + collection | ✓ (`alloc` + `spin`) | ✓ | ~ | closest in design to `local-rcu`; young and little used |
| `read-copy-update` | 0.1.0 | `Mutex` on the reader list and on the reclaim queue | ✗ | ✓ | ✗ | single 2024 release |
| `arc-swap` | 1.9.5 | none (lock-free) | ~ (nightly, `experimental-thread-local`) | ✗ | ✓ | also TSan; 1.9.0 promoted several orderings to `SeqCst` after a flaw in the original proof |
| `hazarc` | 0.2.0 | none | ✓ (custom domains) | ✗ | ✓ (many seeds) | inspired by `arc-swap`; has a `Cache` for repeated loads too |
| `aarc` | 0.4.0 | none | ✗ | ✗ | ~ | built on `fast-smr` |
| `arcshift` | 0.4.10 | none (lock-free) | ✓ | ✓ | ✓ | also `shuttle`; best when updates are rare |
| `arcu` | 0.1.3 | atomics; `RwLock` on the global counter registry (an `RwLock` based variant also exists) | ~ (`std` feature is optional) | ✗ | ✓ (many seeds) | from scryer-prolog |
| `rcu_cell` | 1.2.4 | none — CAS/spin on a tagged pointer shared by readers and writers | ✓ | ✗ | ~ | 64-bit only; hands out std `Arc` |
| `rcu_128` | 0.2.3 | `parking_lot::RwLock` to serialise `update`; writers spin on readers | ✗ | ✗ | ~ | needs 128-bit atomics |
| `rcurs` | 0.2.0 | none | ✓ | ✗ | ✗ | nothing protects the window between loading the pointer and incrementing its refcount, so a concurrent `update` can free the value first (reproduced: segfaults under a `get()`/`update()` stress test) |
| `rcu-clean` | 0.1.8 | `graceful`: `Mutex`. Others: none (custom singly linked list) | ✗ | ✗ | ✗ | 4 variants; last release 2023 |
| `atom_box` | 0.3.0 | none | ✓ | ✓ | ✓ | hazard pointers, not RCU |
| `basedrop` | 0.1.3 | none; writers spin | ✓ | ✗ | ✗ | aimed at real-time audio: drops never run on the reader thread |
| `concread` | 0.5.12 | two `Mutex`es (writer serialisation, active pointer) | ✗ | ✗ | ✓ | also has concurrently readable maps and trees |
| `seqlock` | 0.2.0 | `parking_lot::Mutex` for writers | ✗ | ✗ | ✗ | `T: Copy` only; no allocation at all |
| `keepcalm` | 0.6.0 | `parking_lot::RwLock` | ✗ | ✗ | ✓ | a uniform wrapper over mutex / rwlock / rcu; its "rcu" is `RwLock<Arc<T>>` |
| `tokio_rcu` | 0.6.0 | `Mutex`/`RwLock` on the writer side only | ✗ | ✓ | ✓ | new and small as of 2026; tied to tokio (`tokio_unstable`) |
| `crossbeam-epoch` | 0.9.21 | none (lock-free) | ✓ (`alloc`) | ✓ | ✓ | by far the most widely used |
| `ebr` | 0.2.13 | `Mutex` for maintenance of orphaned bags | ✗ | ✗ | ✗ | low cacheline ping-pong; last release 2024 |
| `seize` | 0.5.1 | `Mutex` for thread registration, not on the read path | ✗ | ✗ | ✓ | Hyaline; used by `papaya` |
| `sdd` | 4.8.11 | none (lock-free) | ✗ | ✓ | ✓ | used by `scc` |
| `haphazard` | 0.1.8 | none | ✓ | ✓ | ✓ | port of folly's implementation; last release 2023 |
| `kovan` | 0.2.6 | none on the read path | ~ (documented; `std` is a default feature) | ✗ | ✓ | also `shuttle`; young, released very frequently |
| `urcu` | 0.0.4 | liburcu | ✗ | — | — | FFI to liburcu (`memb`); LGPL |
| `urcu2` | 0.1.3 | liburcu | ✗ | — | — | FFI to liburcu, all flavors; describes itself as experimental; GPL-3.0 |

## Alternatives

Compared to `local-rcu` ...:

- `std::sync::RwLock<Arc<T>>` / `Arc<RwLock<T>>`: The usual starting point.
  Every read writes to a shared cacheline to take the lock, readers and
  writers block each other, and with `Arc` the old value is freed by whichever
  thread drops the last reference.
- `std::sync::mpsc::channel`: MPSC (vs SPMC), multiple values (not just
  latest) are readable, and each value is seen by one receiver only.
- [`tokio::sync::watch`](https://docs.rs/tokio/latest/tokio/sync/watch/index.html):
  Allows being notified of updates via async, uses an `RwLock` to control
  access to the value. Readers holding a `Ref` block writers, and `send`
  briefly blocks readers. The sender can be cloned (multiple producers) and
  `send_replace` returns the previous value.
- [`tokio::sync::broadcast`](https://docs.rs/tokio/latest/tokio/sync/broadcast/index.html):
  supports multiple producers, provides some "history" (previous values) instead
  of only the most recent value, supports async waiting for new values, uses
  Mutexes internally. Every receiver gets its own clone of each value
  (`T: Clone`), and receivers that fall behind get a `Lagged` error.
- [`triple_buffer`](https://crates.io/crates/triple_buffer): Uses atomic
  swaps of three fixed buffers, wait-free on both sides. Allows immediate data
  free/reuse (the producer gets a stale buffer back to overwrite in place, so
  nothing needs allocating per update). Single Consumer. Supports `no_std`.
- [`left-right`](https://crates.io/crates/left-right): Uses single "old" value
  instead of having arbitrary numbers of old values. Slow readers can prevent
  new values from being published. It requires implementing a concept of
  "Absorb" to place modification of the "old" value into `left-right`
  internals. Uses an "operation log" so that writers don't block on readers
  directly (only publishing is blocked). The read path is close to ours (a
  per-reader epoch and a `SeqCst` fence), but it counts with `fetch_add` on
  both entering and leaving. Keeps two full copies of the data and applies
  every operation twice.
- [`swmr-cell`](https://crates.io/crates/swmr-cell): The closest design to
  `local-rcu`: single writer, per-reader slots, a mutex only for reader
  registration and collection. Readers publish the *version* they pinned and
  the writer frees everything older than the minimum pinned version. As with
  `local-rcu`, a reader that holds one guard for a long time holds back every
  value replaced in the meantime, and an idle reader holds back nothing
  (both measured). Old values are dropped by the writer, not returned, and
  the most recently replaced value is always kept (for `previous()`).
  Collection runs automatically after a threshold of stores. Has a
  "read-preferred" mode that moves the barrier cost from readers to the
  writer, and supports `no_std`.
- [`read-copy-update`](https://crates.io/crates/read-copy-update): Also single
  writer with per-reader epochs. Readers only store their epoch when they
  read, so a reader that goes idle keeps pinning the value it last read (and
  everything newer) until it reads again or is dropped (measured: with one
  idle reader, none of 1000 replaced values were freed). Old values are dropped
  by the writer, not returned. Has `loom` tests. One release, in 2024.
- [`arc-swap`](https://crates.io/crates/arc-swap): Atomically swappable `Arc`,
  any thread may swap. `load()` hands out a `Guard` protected by a
  thread-local "debt" slot (similar to a hazard pointer), which avoids
  touching the reference count unless a thread holds many guards at once;
  `load_full()` hands out an owned `Arc` (refcount inc/dec). Old values are
  freed as soon as the last reference drops (no grace period, but no
  deferred-free bookkeeping either), on whichever thread that happens to be.
  Includes a thread-local `Cache` to speed up repeated loads.
- [`hazarc`](https://crates.io/crates/hazarc): Wait-free `AtomicArc` mixing
  hazard pointers with atomic reference counting (explicitly inspired by
  `arc-swap`), with a wait-free `swap`, custom domains, and `no_std` support.
  Validated with `miri` (over many seeds); it has no `loom` tests.
- [`aarc`](https://crates.io/crates/aarc): `Arc`, `AtomicArc` and a
  non-counting `Guard`, built on the `fast-smr` reclamation crate. Loads don't
  touch reference counts; supports `compare_exchange`.
- [`arcshift`](https://crates.io/crates/arcshift): An `Arc` whose value can be
  updated: an `Arc` over a linked list of versions. Reads through `&mut self`
  are nearly as cheap as a plain `Arc` deref while nothing has changed; shared
  (`&self`) reads of a stale handle are much slower. Memory is bounded by the
  number of live handles. A good fit when updates are rare. Validated with
  `loom`, `shuttle` and `miri`.
- [`basedrop`](https://crates.io/crates/basedrop): `SharedCell` hands out
  refcounted `Shared<T>` values, and drops are queued for a collector thread
  so they never run on a real-time thread. Readers write to a shared counter
  and writers spin while a read is in flight.
- [`concread`](https://crates.io/crates/concread): `CowCell` is a
  mutex-protected `Arc` with serialised, transactional writers (`EbrCell` is
  the `crossbeam-epoch` flavored variant). Has async variants.
- [`seqlock`](https://crates.io/crates/seqlock): For small `Copy` values:
  readers copy the value out and retry if a write overlapped, so there is
  nothing to reclaim at all. Readers can't borrow, and a busy writer makes
  readers retry.
- [`keepcalm`](https://crates.io/crates/keepcalm): A uniform `Shared` /
  `SharedMut` API over mutex, rwlock and "rcu" backings. Its rcu mode is a
  `parking_lot::RwLock<Arc<T>>`: reads take the lock briefly and clone the
  `Arc`, writes clone the value and commit when the write guard drops.

## Compared to other rust "rcu" crates

Compared to `local-rcu` ...:

 - [`crossbeam-epoch`](https://crates.io/crates/crossbeam-epoch). A framework
   rather than a cell. Uses a global shared garbage queue (plus thread-local
   bags), so garbage may be destroyed by any participating thread and is not
   returned to the writer. Has a separate `Guard` concept (like
   `rcu_clean::graceful`) to allow lowering the cost of reads by delaying
   garbage collection more. Epochs are global rather than per slot, so a
   thread that stays pinned holds back the garbage of every data structure
   using the collector. Supports `no_std`. Includes `miri`
   based validation (along with `loom`).
 - [`rcu-clean`](https://crates.io/crates/rcu-clean). Does not include any
   tests using `loom` or other checking tool. Provides multiple mechanisms for
   freeing old values ("graceful": a separated global Arc based method,
   `ArcRcu`: a custom Arc-like for each value, and some not shareable between
   threads: `BoxRcu` & `RcRcu`). "graceful" allows deferring the bookkeeping
   over multiple reads (lowering read cost) by delaying freeing of old data.
   `ArcRcu` increments a shared borrow count the first time a handle is read
   and only decrements it in `clean()`. Old values are not returned to the
   caller. In "graceful" variant, writer can briefly block readers which want
   to create a new "Grace" while an update is occurring. In `ArcRcu`, readers
   aren't blocked by the writer, and a second concurrent `update()` panics.
   Internally uses a custom singly linked list to queue old values for
   freeing. Outside "graceful", `clean()` must be called manually (and needs
   `&mut`), no automatic cleaning exists. Last released in 2023.
 - [`rcu_cell`](https://crates.io/crates/rcu_cell). Docs have improved
   considerably in recent releases. Allows multiple writers and hands out
   standard `Arc`s; writes return the old `Arc`. The pointer word also holds a
   small reader count: every read does a `fetch_add` on it, increments the
   `Arc`'s reference count, then does a `fetch_sub`. Writers compare-exchange
   the same word and wait (briefly) for the reader count to reach zero, so
   readers block writers. In other words: all readers and writers perform
   atomic writes on a shared location, which may result in contention on the
   cache line containing the pointer. Supports `no_std`, 64-bit targets only.
   Does not include any tests using [`loom`](https://crates.io/crates/loom/);
   the tests have `miri` settings but the published CI configuration doesn't
   run it.
 - [`rcu_128`](https://crates.io/crates/rcu_128). Keeps the value pointer and
   a reader count in a single 128-bit atomic. Reads are a `fetch_add` on that
   shared word (and a compare-exchange loop when the guard drops), so this is
   reference counting on a contended cacheline rather than a wait-free read.
   `write()` blocks (spins, then yields) until every reader of the old value
   has dropped its guard, and frees the old value itself. Only one old value
   can be pending at a time. Requires platform support for 128-bit atomics.
 - [`arcu`](https://crates.io/crates/arcu). Arc based RCU using a pool of
   epoch counters to keep the read critical section short (it only covers
   cloning the `Arc`, never user code). Each read increments an epoch counter
   twice and the `Arc` reference count once. Writers wait for in-flight read
   sections, get the old `Arc` back, and replaced values are dropped once the
   last reference disappears. Originated in
   [scryer-prolog](https://github.com/mthom/scryer-prolog/pull/1980).
 - [`rcurs`](https://crates.io/crates/rcurs). A small "oxidized" RCU
   implementation: an atomic pointer to a value with an embedded reference
   count. In 0.2.0 a reader loads the pointer and then increments the count
   with nothing protecting the window in between, so a concurrent `update` can
   free the value first: a stress test with a few threads calling `get()`
   while another calls `update()` reliably segfaults.
 - [`tokio_rcu`](https://crates.io/crates/tokio_rcu). RCU for async Rust with
   tokio (new & small as of 2026), modelled on the linux kernel API
   (`rcu_read_lock`, `synchronize_rcu`). Quiescent-state based: a tokio worker
   is quiescent after each task poll, so readers do no bookkeeping at all (a
   read is one pointer load) but can't hold a reference across `.await`.
   `RcuBox::swap` returns the old `Box` after awaiting a grace period. Needs
   `tokio_unstable` and its own runtime setup. Validated with `loom` and
   `miri`.
 - [`urcu`](https://crates.io/crates/urcu). Safe wrapper of the `memb`
   variant of the userspace RCU library (liburcu). Threads must be registered;
   provides `RcuBox` and an RCU stack.
 - [`urcu2`](https://crates.io/crates/urcu2). A newer safe API to liburcu,
   covering all flavors including qsbr, plus liburcu's hash map, list, queue
   and stack. Describes itself as experimental. GPL-3.0.
 - [`atom_box`](https://crates.io/crates/atom_box). Uses hazard pointers instead
   of RCU: every load acquires a hazard pointer, which is linear in the number
   of threads and boxes. `swap` returns the old value. Supports `no_std`,
   validated with `loom` and `miri`.
 - [`haphazard`](https://crates.io/crates/haphazard). Hazard pointers,
   a port of the implementation in facebook's `folly`. A framework: you manage
   `HazardPointer`s, `Domain`s and retirement yourself. Last released in 2023.
 - [`ebr`](https://crates.io/crates/ebr). Simple epoch-based reclamation
   with low cacheline ping-pong. Only offers `pin()` and `defer_drop()`; the
   pointer handling is up to you.
 - [`seize`](https://crates.io/crates/seize). Fast, predictable memory
   reclamation for concurrent data structures, based on the Hyaline scheme:
   retired batches are reference counted across the threads that were active
   when they were retired, and the last of those threads frees them.
 - [`sdd`](https://crates.io/crates/sdd). Scalable lock-free delayed
   deallocation: an epoch-based variant that keeps retired memory in
   thread-local storage until it is unreachable. Provides owned and
   reference-counted smart pointers on top. Has `loom` support.
 - [`kovan`](https://crates.io/crates/kovan). Wait-free reclamation with
   bounded memory, plus an `Atom<T>` cell with an `arc-swap` like API. Young
   and changing quickly.
