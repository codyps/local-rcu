#! /usr/bin/env bash
set -x

RUSTFLAGS="--cfg loom" cargo test --test loom --release "$@"
