#!/usr/bin/env python3
"""Render a human-readable git-note summary of one commit's benchmark records.

Usage: bench_note.py COMMIT_SHA DATA_DIR [DATA_DIR ...]

Prints the note text to stdout (one stanza per arch recorded for the commit).
Intended for `git notes add -F` in CI, so numbers are visible with the commit
(`git show`, GitHub commit page).
"""

import json
import sys


def main():
    sha, *dirs = sys.argv[1:]
    lines = [
        "Benchmarks recorded by CI. Counts (instr, allocs, bytes, values) are exact;",
        "times (ns) are from shared runners and indicative only.",
        "",
    ]
    found = False
    for d in dirs:
        try:
            rec = json.load(open(f"{d}/{sha}.json"))
        except FileNotFoundError:
            continue
        found = True
        hw = rec.get("hardware") or {}
        title = rec["arch"]
        if hw.get("cpu_model"):
            title += f" ({hw['cpu_model']}, {hw.get('cores', '?')} vCPU)"
        lines.append(f"{title}:")
        width = max(len(r["name"]) for r in rec["results"])
        for r in rec["results"]:
            value = f"{r['value']:,}" if isinstance(r["value"], int) else f"{r['value']:g}"
            lines.append(f"  {r['name']:<{width}}  {value} {r['unit']}")
        lines.append("")
    if not found:
        sys.exit(f"no benchmark records for {sha}")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
