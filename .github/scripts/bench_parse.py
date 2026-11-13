#!/usr/bin/env python3
"""Parse the output of `cargo bench --bench isolated` into a single JSON record.

Usage: bench_parse.py OUT_JSON ARCH LSCPU_TXT UNAME_LINE ISOLATED_TXT [ISOLATED_TXT ...]

Environment: COMMIT_SHA, COMMIT_DATE (ISO 8601), RUSTC_VERSION (optional).

Emits one record:
  { "arch", "commit", "date", "rustc"?, "hardware", "results": [ {"name","unit","value"} ] }

`hardware` carries the runner's CPU model, vCPU count and uname line, so the
site/notes can attribute numbers to specific runner hardware.

A row found more than once (several runs of the harness) keeps its best value
when it is a time, which is how the harness itself combines its passes, and
lists the value of every run as `runs`: their spread is what bench_compare.py
takes for noise. The other units are exact, and the last value wins.
"""

import json
import os
import re
import sys

# rows look like (the name ends at the first run of two spaces):
# instructions/write(), no readers                      521.0 instr
# ns per read/R=3 plain, writer: write()                25.74 ns
ROW = re.compile(r"^(\S[^/]*/.*?)\s{2,}([0-9]+(?:\.[0-9]+)?) (\S+)\s*$")


def parse_hardware(lscpu_path, uname_line):
    fields = {}
    for line in open(lscpu_path):
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip()
    model = fields.get("Model name") or fields.get("Model") or "unknown cpu"
    cores = fields.get("CPU(s)") or "?"
    return {"cpu_model": model, "cores": cores, "uname": uname_line}


def parse_isolated(path, results):
    """`results`: name -> {"name", "unit", "value"}, in order of first appearance."""
    for line in open(path):
        m = ROW.match(line.rstrip("\n"))
        if not m or line.startswith("#"):
            continue
        name, value, unit = f"isolated/{m.group(1)}", float(m.group(2)), m.group(3)
        row = {"name": name, "unit": unit, "value": value}
        seen = results.get(name)
        if seen and unit == "ns":
            row["runs"] = seen.get("runs", [seen["value"]]) + [value]
            row["value"] = min(row["runs"])
        results[name] = row


def main():
    out_json, arch, lscpu_txt, uname_line, *isolated = sys.argv[1:]

    results = {}
    for path in isolated:
        parse_isolated(path, results)
    if not results:
        sys.exit("no benchmark rows found in " + ", ".join(isolated))

    record = {
        "arch": arch,
        "commit": os.environ["COMMIT_SHA"],
        "date": os.environ["COMMIT_DATE"],
        "hardware": parse_hardware(lscpu_txt, uname_line),
        "results": list(results.values()),
    }
    if os.environ.get("RUSTC_VERSION"):
        record["rustc"] = os.environ["RUSTC_VERSION"]
    with open(out_json, "w") as f:
        json.dump(record, f, indent=1, sort_keys=True)
        f.write("\n")

    print(f"wrote {out_json}: {len(results)} benchmarks for {arch}")


if __name__ == "__main__":
    main()
