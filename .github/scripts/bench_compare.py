#!/usr/bin/env python3
"""Render what a change does to the benchmarks, as markdown.

Usage: bench_compare.py BASE_JSON HEAD_JSON [BASE_LABEL HEAD_LABEL]

Both files are records written by bench_parse.py, measured on the same
machine one right after the other. BASE_JSON may be missing (nothing to
compare against): the head's numbers are then listed on their own.

Counts (instructions, allocations, bytes, values) are exact, so any change in
them is reported.

Times come from a shared runner, and the ones with several threads also depend
on which CPUs the threads land on: the same code measured twice can differ by
tens of percent. Each side is therefore measured several times (`runs` in the
record), and a time is only called out when every run of one side is better
than every run of the other, and the best values differ by more than
TIME_NOISE. With 4 runs a side, chance alone separates them like that for
about 1 row in 35.
"""

import json
import os
import sys

# relative change of the best values below which a time is not called out
TIME_NOISE = 0.05
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
        # the best is the value; the worst shows how far apart the runs were
        text += f" (… {fmt(max(runs), unit)})"
    return text


def separated(a, b):
    """Is every run of one side better than every run of the other?"""
    return max(a) < min(b) or max(b) < min(a)


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


def main():
    base_path, head_path = sys.argv[1:3]
    base_label, head_label = sys.argv[3:5] if len(sys.argv) >= 5 else ("base", "this")
    base, head = load(base_path), load(head_path)
    if head is None:
        sys.exit(f"no results in {head_path}")

    hw = head.get("hardware") or {}
    title = f"### {head['arch']}"
    if hw.get("cpu_model"):
        title += f" — {hw['cpu_model']}, {hw.get('cores', '?')} vCPU"
    lines = [title, ""]

    base_results = {r["name"]: r for r in (base or {}).get("results", [])}
    head_results = {r["name"]: r for r in head["results"]}
    names = list(head_results) + [n for n in base_results if n not in head_results]

    counts, times = [], []
    for name in names:
        b, h = base_results.get(name), head_results.get(name)
        unit = (h or b)["unit"]
        bv = b["value"] if b else None
        hv = h["value"] if h else None
        c = change(bv, hv) if b and h else None
        runs = lambda r: (r.get("runs") or [r["value"]]) if r else None
        row = (name.removeprefix("isolated/"), unit, bv, hv, c, runs(b), runs(h))
        (times if unit == "ns" else counts).append(row)

    def notable(row, noise):
        _name, unit, bv, hv, c, base_runs, head_runs = row
        if bv is None or hv is None:
            return True
        if c is None or abs(c) <= noise:
            return c is None
        return unit != "ns" or separated(base_runs, head_runs)

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
        if changed_counts:
            lines += ["**Exact counts that changed** (instructions, allocations, bytes, values per operation):", ""]
            lines += table(changed_counts) + [""]
        if changed_times:
            lines += [
                "**Times that moved** (every run of one side beats every run of "
                f"the other, best values more than {TIME_NOISE * 100:.0f}% apart; "
                "shared runner, so still a hint and not a measurement):",
                "",
            ]
            lines += table(changed_times) + [""]

    lines += [
        "<details><summary>All results (times: best run, and the worst in brackets)</summary>",
        "",
    ]
    lines += table(counts + times)
    lines += ["", "</details>", ""]
    print("\n".join(lines))


if __name__ == "__main__":
    main()
