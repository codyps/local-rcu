"""Benchmark renames, read from `benches/renames.txt` (see there for the format).

Used by bench_compare.py and gen_pages.py so that a benchmark keeps its
results across a rename.
"""

import os

SEP = " -> "


def load(path):
    """The renames in `path` as (old, new) pairs, in order; none if it is missing."""
    if not path or not os.path.exists(path):
        return []
    renames = []
    for n, line in enumerate(open(path, encoding="utf-8"), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        old, sep, new = line.partition(SEP)
        old, new = old.strip(), new.strip()
        if not sep or not old or not new:
            raise SystemExit(f"{path}:{n}: expected `old{SEP}new`, got {line!r}")
        renames.append((old, new))
    return renames


def apply(name, renames):
    """`name` with every rename applied, in order."""
    for old, new in renames:
        name = name.replace(old, new)
    return name
