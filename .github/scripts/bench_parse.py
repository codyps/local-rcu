#!/usr/bin/env python3
"""Parse benchmark output from both bench targets into a single JSON record.

Usage: bench_parse.py CRITERION_TXT ISOLATED_TXT OUT_JSON ARCH [LSCPU_TXT UNAME_LINE]

Environment: COMMIT_SHA, COMMIT_DATE (ISO 8601).

Emits one record:
  { "arch", "commit", "date", "hardware"?, "results": [ {"name","unit","value"} ] }

`hardware` (when LSCPU_TXT/UNAME_LINE are given) carries the runner's CPU
model, vCPU count and uname line, so the site/notes can attribute numbers to
specific runner hardware.
"""

import json
import os
import re
import sys


def parse_hardware(lscpu_path, uname_line):
    fields = {}
    for line in open(lscpu_path):
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip()
    model = fields.get("Model name") or fields.get("Model") or "unknown cpu"
    cores = fields.get("CPU(s)") or "?"
    return {"cpu_model": model, "cores": cores, "uname": uname_line}


def parse_criterion(path, results):
    # lines like:
    # n100_m10                time:   [13.761 ms 14.017 ms 14.254 ms]
    pat = re.compile(
        r"^(\S+)\s+time:\s+\[\s*"
        r"([0-9.]+)\s+(\w+)\s+"
        r"([0-9.]+)\s+(\w+)\s+"
        r"([0-9.]+)\s+(\w+)\s*\]"
    )
    for line in open(path):
        m = pat.match(line)
        if m:
            results.append(
                {
                    # the middle of [lower estimate median upper estimate]
                    "name": f"b/{m.group(1)}",
                    "unit": m.group(5),
                    "value": float(m.group(4)),
                }
            )


def parse_isolated(path, results):
    # lines like:
    # reader  m=1  writer_active=false    178180234 reads/s/reader
    # writer  m=4  readers_busy       4217150 writes/s
    pat = re.compile(r"^(reader|writer)\s+(m=\d+)\s+(\S+)\s+(\d+)\s+(\S+)\s*$")
    for line in open(path):
        m = pat.match(line.rstrip("\n"))
        if m:
            results.append(
                {
                    "name": f"isolated/{m.group(1)} {m.group(2)} {m.group(3)}",
                    "unit": m.group(5),
                    "value": int(m.group(4)),
                }
            )


def main():
    criterion_txt, isolated_txt, out_json, arch = sys.argv[1:5]

    results = []
    parse_criterion(criterion_txt, results)
    parse_isolated(isolated_txt, results)

    record = {
        "arch": arch,
        "commit": os.environ["COMMIT_SHA"],
        "date": os.environ["COMMIT_DATE"],
        "results": results,
    }
    if len(sys.argv) >= 7:
        record["hardware"] = parse_hardware(sys.argv[5], sys.argv[6])
    with open(out_json, "w") as f:
        json.dump(record, f, indent=1, sort_keys=True)
        f.write("\n")

    print(f"wrote {out_json}: {len(results)} benchmarks for {arch}")


if __name__ == "__main__":
    main()
