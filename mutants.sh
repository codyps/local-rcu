#! /usr/bin/env bash
#
# Check that the orderings and fences the safety argument rests on are all needed, and that
# the tests can tell when one is missing.
#
# Each mutant is a build with `--cfg local_rcu_mutant="<name>"`, which weakens one ordering
# or leaves out one fence or step (see `ord` in src/lib.rs for the orderings and fences;
# the step is in `Reader::hold_nothing()`). For every mutant, the test named here
# has to fail, and for the right reason: a failed loom model, or undefined behavior found
# by miri. A mutant that survives means the test no longer covers that ordering, or that
# the ordering is stronger than it has to be.
#
#   ./mutants.sh                   all mutants
#   ./mutants.sh no_reader_fence   only the ones named
#
# MUTANTS_SEEDS         miri seeds to try per mutant (default 0..32)
# MUTANTS_CARGO_MIRI    how to run miri (default "cargo miri"; eg "cargo +nightly miri")
#
# miri finds these by picking thread schedules at random, one per seed. The seeds are fixed,
# so a run is repeatable, but a new miri may schedule differently: if a mutant that used to
# be caught survives after a toolchain update, try more seeds before looking for a bug.

set -u

seeds=${MUTANTS_SEEDS:-0..32}
read -r -a cargo_miri <<<"${MUTANTS_CARGO_MIRI:-cargo miri}"

# mutant, tool, test binary, test
mutants=(
	"no_reader_fence       loom loom loom_send_from_1_to_1"
	"no_writer_fence       loom loom loom_send_from_1_to_1"
	"enter_store_relaxed   miri a    try_sync_never_frees_a_value_in_use"
	"leave_store_relaxed   miri a    reentry_never_observes_collected_value"
	"drop_store_relaxed    miri a    reader_drop_orders_reads_before_collection"
	"active_load_relaxed   miri a    reentry_never_observes_collected_value"
	"publish_store_relaxed miri a    reentry_never_observes_collected_value"
	"scan_load_relaxed     miri a    reentry_never_observes_collected_value"
	"collect_load_relaxed  miri a    try_sync_never_frees_a_value_in_use"
	"held_load_relaxed     miri cached concurrent_readers_never_see_dropped_values"
	"no_hold_nothing       miri cached concurrent_readers_never_see_dropped_values"
)

# mutant builds get a target directory of their own: they must not replace the normal ones
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}/mutants

log=$(mktemp)
trap 'rm -f "$log"' EXIT

survived=()
ran=0
for entry in "${mutants[@]}"; do
	read -r name tool bin test <<<"$entry"
	if [ $# -gt 0 ] && [[ " $* " != *" $name "* ]]; then
		continue
	fi
	ran=$((ran + 1))

	cfg="--cfg local_rcu_mutant=\"$name\""
	case $tool in
	loom)
		RUSTFLAGS="--cfg loom --cfg local_rcu_loom_consume $cfg" \
			cargo test --release --test "$bin" -- --exact "$test" >"$log" 2>&1
		status=$?
		caught="test result: FAILED"
		;;
	miri)
		RUSTFLAGS="$cfg" MIRIFLAGS="-Zmiri-many-seeds=$seeds" \
			"${cargo_miri[@]}" test --test "$bin" -- --exact "$test" >"$log" 2>&1
		status=$?
		caught="Undefined Behavior"
		;;
	*)
		echo "$name: unknown tool $tool" >&2
		exit 2
		;;
	esac

	if [ $status -ne 0 ] && grep -q "$caught" "$log"; then
		echo "caught    $name ($tool: $test)"
	else
		echo "SURVIVED  $name ($tool: $test), exit status $status, end of output:"
		tail -n 30 "$log" | sed 's/^/    /'
		survived+=("$name")
	fi
done

if [ $ran -eq 0 ]; then
	echo "no such mutant: $*" >&2
	exit 2
fi
if [ ${#survived[@]} -gt 0 ]; then
	echo "${#survived[@]} of $ran mutants were not caught: ${survived[*]}" >&2
	exit 1
fi
echo "all $ran mutants caught"
