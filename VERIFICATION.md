# Verification findings for the gen/floor protocol

This branch explores an alternative fix for the tracking-protocol soundness
bugs (see `main`: "fix UB: make read sections always observe the current
value (SeqCst active)"), using a generation counter plus per-reader
publication of the observed generation ("floor") instead of SeqCst loads of
`active`.

Design summary: readers publish the generation of the value they last
observed; a retired value is collected once every reader's floor is at or
past the generation that superseded it (or the reader is dead). Soundness
rests on read-read coherence on `active` bounding each reader's future
loads below by its floor. Note the retention semantics differ from the
original/SeqCst protocol: an idle reader pins its last-observed value until
it reads again or is dropped.

## Verification status

- `cargo test` (native): PASS
- `loom.sh`: INCOMPLETE — exceeds 2 hours (original/SeqCst: ~25 s). The
  extra per-reader atomic (`floor`) and the global generation counter
  explode loom's state space.
- full `cargo miri test --workspace`: INCOMPLETE — exceeds 1 hour
  (SeqCst: ~10 min).
- scoped `cargo miri test --test a --test no_leak` (the protocol-critical
  tests): INCOMPLETE — exceeds 40 minutes (SeqCst: ~3 min, clean, also
  verified across 9 scheduler seeds).

## Conclusion

The design is not falsified, but it cannot be verified within practical
CI budgets for this repo (loom runs on every push). The SeqCst protocol on
`main` (released as v1.0.7) verifies cheaply and preserves the original
collection semantics; it is the fix of record. This branch is kept for
reference only.
