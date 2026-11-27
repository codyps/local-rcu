#! /usr/bin/env bash
set -x

# local_rcu_loom_consume: the loom we build against (see `[patch.crates-io]` in Cargo.toml)
# has `AtomicPtr::load_consume()`, see `ord::active_load()` in src/lib.rs
RUSTFLAGS="--cfg loom --cfg local_rcu_loom_consume -D warnings" cargo test --test loom --release "$@"
