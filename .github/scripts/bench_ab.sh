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
set -euo pipefail

base_ref=$1
cand_ref=$2
out=$3
rounds=${ROUNDS:-10}
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

builds() {
  (cd "$work/$1" && cargo bench --bench isolated --no-run) >&2
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
    echo "== round $round: $side" >&2
    (
      cd "$work/$side"
      if [ "$round" != 1 ]; then
        export BENCH_NO_VALGRIND=1
      fi
      cargo bench --bench isolated
    ) | tee -a "$out/$side.txt" >&2
  done
done
