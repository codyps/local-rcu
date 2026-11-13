#!/usr/bin/env python3
"""Render what a change does to the benchmarks, as markdown.

Usage: bench_compare.py [--renames RENAMES_TXT] BASE_JSON HEAD_JSON [BASE_LABEL HEAD_LABEL]

Both files are records written by bench_parse.py, measured on the same
machine one right after the other. BASE_JSON may be missing (nothing to
compare against): the head's numbers are then listed on their own.

RENAMES_TXT (benches/renames.txt) lists benchmarks that were renamed: a base
result whose name a rename turns into one of the head's is compared against
it, instead of the two showing up as one benchmark gone and another new.

Counts (instructions, allocations, bytes, values) are exact, so any change in
them is reported.

Times come from a shared runner, and the ones with several threads also depend
on which CPUs the threads land on: the same code measured twice can differ by
tens of percent, and some configurations have two speeds they switch between.
Each side is therefore measured many times, alternating (`runs` in the
record). The value shown for a time is the median of its runs, and a time is
only called out when both hold:

- the medians differ by more than TIME_NOISE, and
- a Mann-Whitney U test on the runs of the two sides gives p < TIME_P, that
  is, the runs of one side rank almost entirely below those of the other.

With 10 runs a side and nothing changed, that should call out one row in
several hundred. What it cannot tell apart from a change in the code is a
difference between the two builds that has nothing to do with the change
(code alignment, say): small, steady differences in a single row are not
worth chasing.
"""

import json
import os
import sys

import bench_renames

# relative change of the medians below which a time is not called out
TIME_NOISE = 0.05
# two-sided p of the rank test above which a time is not called out
TIME_P = 0.001
# counts are exact; this only hides rounding in the printed values
COUNT_NOISE = 0.002


def load(path):
    if not os.path.exists(path):
        return None
    return json.load(open(path))


def fmt(value, unit):
    if unit in ("values",):
        return f"{value:,.0f}"
    if unit in ("instr", "bytes"):
        return f"{value:,.1f}".removesuffix(".0")
    return f"{value:,.2f}"


def change(base, head):
    """Relative change, or None when there is no sensible one."""
    if base == head:
        return 0.0
    if base == 0:
        return None
    return (head - base) / base


def pct(c):
    if c is None:
        return "new ≠ 0"
    if c == 0:
        return "="
    return f"{c * 100:+.1f}%"


def cell(value, unit, runs):
    if value is None:
        return "–"
    text = f"{fmt(value, unit)} {unit}"
    if runs and len(runs) > 1:
        text += f" ({fmt(min(runs), unit)} … {fmt(max(runs), unit)})"
    return text


def median(xs):
    xs = sorted(xs)
    mid = len(xs) // 2
    return xs[mid] if len(xs) % 2 else (xs[mid - 1] + xs[mid]) / 2


def rank_test(a, b):
    """Two-sided p of the Mann-Whitney U test for samples `a` and `b`.

    Exact (the distribution of U is counted, not approximated), but for
    samples without ties; a tie counts as half, which is close enough for a
    threshold.
    """
    n, m = len(a), len(b)
    u = sum(1.0 if x < y else 0.5 if x == y else 0.0 for x in a for y in b)
    u = min(u, n * m - u)
    # ways[i][j][k]: orderings of i runs of a and j of b with U == k
    ways = [[[0] * (n * m + 1) for _ in range(m + 1)] for _ in range(n + 1)]
    for i in range(n + 1):
        for j in range(m + 1):
            if i == 0 or j == 0:
                ways[i][j][0] = 1
                continue
            for k in range(i * j + 1):
                # the largest value is one of b (nothing changes), or one of
                # a (it is above all j of b)
                ways[i][j][k] = ways[i][j - 1][k] + (ways[i - 1][j][k - j] if k >= j else 0)
    total = sum(ways[n][m])
    below = sum(ways[n][m][k] for k in range(n * m + 1) if k <= u)
    return min(1.0, 2 * below / total)


def table(rows):
    out = ["| benchmark | base | this | change |", "|---|--:|--:|--:|"]
    for name, unit, base, head, c, base_runs, head_runs in rows:
        b = cell(base, unit, base_runs)
        h = cell(head, unit, head_runs)
        if base is None:
            delta = "new"
        elif head is None:
            delta = "gone"
        else:
            delta = pct(c)
        out.append(f"| `{name}` | {b} | {h} | {delta} |")
    return out


def match_renamed(base_results, head_results, renames):
    """Key the base's results by the names they have in the head.

    A base result is moved to its new name only when the head has that name
    and the base does not already have a result under it. Returns
    the re-keyed results and the renames that moved at least one result.
    """
    keyed, used = {}, []
    for name, r in base_results.items():
        new = bench_renames.apply(name, renames)
        if new != name and new in head_results and new not in base_results and new not in keyed:
            keyed[new] = r
            for i, rename in enumerate(renames):
                if rename not in used and rename[0] in bench_renames.apply(name, renames[:i]):
                    used.append(rename)
        else:
            keyed.setdefault(name, r)
    return keyed, [r for r in renames if r in used]


def main():
    args = sys.argv[1:]
    renames = []
    if args[:1] == ["--renames"]:
        renames = bench_renames.load(args[1])
        args = args[2:]
    base_path, head_path = args[:2]
    base_label, head_label = args[2:4] if len(args) >= 4 else ("base", "this")
    base, head = load(base_path), load(head_path)
    if head is None:
        sys.exit(f"no results in {head_path}")

    hw = head.get("hardware") or {}
    title = f"### {head['arch']}"
    if hw.get("cpu_model"):
        title += f" — {hw['cpu_model']}, {hw.get('cores', '?')} vCPU"
    lines = [title, ""]

    head_results = {r["name"]: r for r in head["results"]}
    base_results, renamed = match_renamed(
        {r["name"]: r for r in (base or {}).get("results", [])}, head_results, renames
    )
    names = list(head_results) + [n for n in base_results if n not in head_results]

    counts, times = [], []
    for name in names:
        b, h = base_results.get(name), head_results.get(name)
        unit = (h or b)["unit"]
        bv = b["value"] if b else None
        hv = h["value"] if h else None
        runs = lambda r: (r.get("runs") or [r["value"]]) if r else None
        if unit == "ns":
            # the record's value is the best run: fine for a history, but the
            # best of a handful of runs is what moves most between two sides
            bv = median(runs(b)) if b else None
            hv = median(runs(h)) if h else None
        c = change(bv, hv) if b and h else None
        row = (name.removeprefix("isolated/"), unit, bv, hv, c, runs(b), runs(h))
        (times if unit == "ns" else counts).append(row)

    def notable(row, noise):
        _name, unit, bv, hv, c, base_runs, head_runs = row
        if bv is None or hv is None:
            return True
        if c is None or abs(c) <= noise:
            return c is None
        return unit != "ns" or rank_test(base_runs, head_runs) < TIME_P

    if base is None:
        lines += [
            f"No baseline: `{base_label}` could not be measured with these benchmarks.",
            "",
        ]
    else:
        changed_counts = [r for r in counts if notable(r, COUNT_NOISE)]
        changed_times = [r for r in times if notable(r, TIME_NOISE)]
        lines.append(
            f"`{base_label}` → `{head_label}`: "
            f"**{len(changed_counts)}** of {len(counts)} exact counts changed, "
            f"**{len(changed_times)}** of {len(times)} times moved beyond "
            "run-to-run noise."
        )
        lines.append("")
        if renamed:
            pieces = ", ".join(f"`{old}` → `{new}`" for old, new in renamed)
            lines += [f"Renamed (`benches/renames.txt`), compared under the new name: {pieces}.", ""]
        if changed_counts:
            lines += ["**Exact counts that changed** (instructions, allocations, bytes, values per operation):", ""]
            lines += table(changed_counts) + [""]
        if changed_times:
            lines += [
                f"**Times that moved** (medians more than {TIME_NOISE * 100:.0f}% apart, "
                f"and a rank test over the runs of both sides gives p < {TIME_P}; "
                "shared runner, so still a hint and not a measurement):",
                "",
            ]
            lines += table(changed_times) + [""]

    lines += [
        "<details><summary>All results (times: median of the runs, best and worst in brackets)</summary>",
        "",
    ]
    lines += table(counts + times)
    lines += ["", "</details>", ""]
    print("\n".join(lines))


if __name__ == "__main__":
    main()
