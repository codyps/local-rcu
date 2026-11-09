#! /usr/bin/env bash
set -x

RUSTFLAGS="--cfg loom -D warnings" cargo test --test loom --release "$@"
