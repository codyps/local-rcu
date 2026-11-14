#!/usr/bin/env bash
# Measure two refs on this machine, one right after the other, for a
# comparison (see bench_compare.py).
#
# Usage: bench_ab.sh BASELINE_REF CANDIDATE_REF OUT_DIR
#
# Writes OUT_DIR/base.txt and OUT_DIR/cand.txt (the harness output of every
# round, appended) and OUT_DIR/harness, which says what the baseline was
# measured with:
#
# - `candidate`: the candidate's benches/isolated.rs, built against the
#   baseline's library. Both sides run the same benchmarks, so a change to
#   the harness itself cannot show up as a change in the numbers.
# - `own`: the baseline's own harness, because the candidate's does not build
#   against the baseline (it uses API the change adds). Rows only one side
#   has are reported as new or gone.
# - `none`: the baseline has no harness that builds; base.txt stays empty.
#
# The two sides alternate for ROUNDS rounds (default 10) of one pass each, so
# that a slow stretch of a shared runner does not land on one side only, and
# so that bench_compare.py has enough runs of each side to tell a change from
# the spread between runs. Many short rounds rather than a few long ones:
# what varies is where the threads of a run end up, not the time within one.
# The instruction counts are exact and only taken in the first round.
#
# Each side is built LAYOUTS times (default 5), each time with its functions
# in a different order (lld's --shuffle-sections, seeded 1..LAYOUTS), and the
# rounds go through those builds in turn. Where a function lands decides how
# its loops line up with cache lines and fetch windows, and that alone can
# move a time by ~10% on some CPUs: a change that only renames a function
# reorders the others, and with one build a side it shows up as a change in
# speed. Measured over several layouts, that lands in the spread of each side
# instead, and only what holds whatever the layout is called out. The
# dependencies are built once: only the harness binary is linked again.
set -euo pipefail

base_ref=$1
cand_ref=$2
out=$3
rounds=${ROUNDS:-10}
layouts=${LAYOUTS:-5}
export BENCH_PASSES=${BENCH_PASSES:-1}

root=$(git rev-parse --show-toplevel)
# one target directory for both: the dependencies are built once
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}

work=$(mktemp -d)
cleanup() {
  git -C "$root" worktree remove --force "$work/base" 2>/dev/null || true
  git -C "$root" worktree remove --force "$work/cand" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

git -C "$root" worktree add --quiet --detach "$work/base" "$base_ref"
git -C "$root" worktree add --quiet --detach "$work/cand" "$cand_ref"

mkdir -p "$out"
: > "$out/base.txt"
: > "$out/cand.txt"

mkdir -p "$work/bin"

# build side $1 with function order $2, as $work/bin/$1-$2
build() {
  local exe
  exe=$(
    cd "$work/$1" &&
      cargo rustc --profile bench --bench isolated --message-format=json-render-diagnostics -- \
        -C link-arg=-fuse-ld=lld -C "link-arg=-Wl,--shuffle-sections=.text*=$2" |
      python3 -c '
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
        print(msg["executable"])
'
  ) || return 1
  [ -n "$exe" ] && cp "$exe" "$work/bin/$1-$2"
}

# every layout of side $1
builds() {
  local seed
  for seed in $(seq "$layouts"); do
    build "$1" "$seed" >&2 || return 1
  done
}

builds cand

harness=candidate
cp "$work/cand/benches/isolated.rs" "$work/base/benches/isolated.rs" 2>/dev/null || harness=none
if [ "$harness" = none ] || ! builds base; then
  harness=own
  if ! git -C "$work/base" checkout --quiet -- benches/isolated.rs || ! builds base; then
    harness=none
  fi
fi
echo "$harness" > "$out/harness"
echo "baseline harness: $harness" >&2

for round in $(seq "$rounds"); do
  for side in base cand; do
    if [ "$side" = base ] && [ "$harness" = none ]; then
      continue
    fi
    layout=$(( (round - 1) % layouts + 1 ))
    echo "== round $round: $side, layout $layout" >&2
    (
      cd "$work/$side"
      if [ "$round" != 1 ]; then
        export BENCH_NO_VALGRIND=1
      fi
      "$work/bin/$side-$layout"
    ) | tee -a "$out/$side.txt" >&2
  done
done
