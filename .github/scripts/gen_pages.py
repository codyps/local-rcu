#!/usr/bin/env python3
"""Generate the benchmark-history GitHub Pages site (index.html).

Usage: gen_pages.py OUT_HTML DATA_DIR [DATA_DIR ...]

Each DATA_DIR contains one JSON file per recorded run (see bench_parse.py).
The site inlines all records and renders one Chart.js line chart per
benchmark, one series per architecture, over commit date.
"""

import glob
import html
import json
import sys

CHARTJS = "https://cdn.jsdelivr.net/npm/chart.js@4"

PAGE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>local-rcu benchmarks</title>
<script src="__CHARTJS__"></script>
<style>
 body { font-family: ui-sans-serif, system-ui, sans-serif; margin: 2rem; background: #fff; color: #111 }
 h1 { font-size: 1.5rem }
 h3 { margin: 1.6rem 0 .2rem; font-family: ui-monospace, monospace; font-weight: 600 }
 /* the canvas wrapper (not the canvas) must carry the height: with
    `maintainAspectRatio: false` Chart.js sizes the canvas to its parent,
    and a parent whose height depends on the canvas grows forever */
 .chartbox { position: relative; height: 14rem; max-width: 56rem; margin-bottom: .5rem }
 .unit { color: #666; font-weight: 400; font-size: .8rem }
 footer { margin-top: 3rem; color: #666; font-size: .8rem }
</style>
</head>
<body>
<h1>local-rcu benchmark history</h1>
<p>One point per commit of <code>main</code> (plus a weekly drift sample).
Shared-runner noise applies; look at trends, not points.</p>
<div id="charts"></div>
<footer>generated __GENERATED__ from __NCOMMITS__ recorded runs</footer>
<script>
const DATA = __DATA__;
const COLORS = { x86_64: "#2563eb", aarch64: "#dc2626" };
const container = document.getElementById("charts");
for (const name of Object.keys(DATA.charts).sort()) {
  const meta = DATA.charts[name];
  const sec = document.createElement("section");
  const h = document.createElement("h3");
  h.textContent = name + " ";
  const u = document.createElement("span");
  u.className = "unit";
  u.textContent = "[" + DATA.units[name] + "]";
  h.appendChild(u);
  const cvs = document.createElement("canvas");
  const box = document.createElement("div");
  box.className = "chartbox";
  box.appendChild(cvs);
  sec.appendChild(h); sec.appendChild(box); container.appendChild(sec);
  const labels = DATA.labels[name];
  new Chart(cvs, {
    type: "line",
    data: {
      labels: labels,
      datasets: Object.entries(meta).map(([arch, pts]) => ({
        label: arch,
        borderColor: COLORS[arch] || "#16a34a",
        backgroundColor: COLORS[arch] || "#16a34a",
        pointRadius: 2,
        borderWidth: 1.5,
        tension: 0.15,
        data: pts,
        spanGaps: true,
      })),
    },
    options: {
      animation: false,
      maintainAspectRatio: false,
      scales: { y: { beginAtZero: false } },
      plugins: { legend: { display: true } },
    },
  });
}
</script>
</body>
</html>
"""


def main():
    out_path = sys.argv[1]
    data_dirs = sys.argv[2:]

    # (commit, arch, bench name) -> point; later files (re-runs) overwrite
    points = {}
    dates = {}
    units = {}
    nruns = 0
    for path in sorted(p for d in data_dirs for p in glob.glob(d + "/*.json")):
        nruns += 1
        rec = json.load(open(path))
        for r in rec["results"]:
            key = (rec["commit"], rec["arch"], r["name"])
            points[key] = r["value"]
            dates[key] = rec["date"]
            units[r["name"]] = r["unit"]

    # group per bench name -> arch -> sorted series
    charts = {}
    for (commit, arch, name), value in points.items():
        charts.setdefault(name, {}).setdefault(arch, []).append(
            {"commit": commit[:8], "date": dates[(commit, arch, name)], "value": value}
        )

    labels = {}
    for name, per_arch in charts.items():
        # one tick per unique (date, commit): arches record the same commits,
        # and re-runs of a commit collapse to a single point
        ticks = {}
        for pts in per_arch.values():
            for p in pts:
                ticks[(p["date"], p["commit"])] = None
        ordered = sorted(ticks.keys())
        labels[name] = [f"{d[5:16].replace('T', ' ')} {c[:8]}" for d, c in ordered]
        for arch, pts in per_arch.items():
            pts.sort(key=lambda p: (p["date"], p["commit"]))
            # map onto the shared tick list (NaN where this arch has no point)
            by_tick = {(p["date"], p["commit"]): p["value"] for p in pts}
            per_arch[arch] = [
                by_tick.get(tick, float("nan")) for tick in ordered
            ]

    payload = json.dumps({"charts": charts, "labels": labels, "units": units})

    import datetime

    page = (
        PAGE.replace("__CHARTJS__", CHARTJS)
        .replace("__DATA__", payload.replace("</", "<\\/"))
        .replace("__GENERATED__", datetime.datetime.now(datetime.timezone.utc).isoformat())
        .replace("__NCOMMITS__", str(nruns))
    )
    import os

    parent = os.path.dirname(os.path.abspath(out_path))
    os.makedirs(parent, exist_ok=True)
    with open(out_path, "w") as f:
        f.write(page)
    print(f"wrote {out_path}: {len(charts)} benchmarks, {nruns} recorded runs")


if __name__ == "__main__":
    main()
