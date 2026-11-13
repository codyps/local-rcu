#!/usr/bin/env python3
"""Generate the benchmark-history GitHub Pages site (index.html).

Usage: gen_pages.py [--renames RENAMES_TXT] OUT_HTML DATA_DIR [DATA_DIR ...]

Each DATA_DIR contains one JSON file per recorded run (see bench_parse.py).
The site inlines all records and renders one Chart.js line chart per
benchmark, one series per architecture, over commit date. Only benchmarks
that the most recent record of some architecture has are shown: the ones a
later harness no longer produces stay on the data branches.
"""

import glob
import html
import json
import sys

import bench_renames

CHARTJS = "https://cdn.jsdelivr.net/npm/chart.js@4"

REPO = "https://github.com/codyps/local-rcu"

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
 h2.group { margin: 2.4rem 0 0; padding-top: .6rem; border-top: 1px solid #ddd }
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
Click a commit label below a chart to open that commit.
Lower is better everywhere.</p>
<p><b>instructions</b>, <b>allocations</b>, <b>memory</b> and <b>held back</b> are exact
counts per operation: a step in one of them is a change in the code (or, for
instructions, in the toolchain). <b>ns per read</b> and <b>ns per write</b> are
times from shared runners of varying hardware; look at trends, not points.</p>
<h2>Runners</h2>
<p id="runners"></p>
<div id="hwblocks"></div>
<div id="charts"></div>
<footer>generated __GENERATED__ from __NCOMMITS__ recorded runs</footer>
<script>
const DATA = __DATA__;
const COLORS = { x86_64: "#2563eb", aarch64: "#dc2626" };

// runners summary + full lscpu dumps
{
  const rp = document.getElementById("runners");
  for (const [arch, models] of Object.entries(DATA.runner_counts)) {
    const line = document.createElement("p");
    const dot = document.createElement("span");
    dot.textContent = "\u25cf ";
    dot.style.color = COLORS[arch] || "#16a34a";
    line.appendChild(dot);
    line.appendChild(document.createTextNode(arch + ": "));
    line.appendChild(document.createTextNode(
      Object.entries(models).map(([m, n]) => `${m} (${n} runs)`).join(", ")));
    rp.appendChild(line);
  }
  const hwb = document.getElementById("hwblocks");
  for (const [name, text] of Object.entries(DATA.hardware_files)) {
    const d = document.createElement("details");
    const s = document.createElement("summary");
    s.textContent = name;
    d.appendChild(s);
    const pre = document.createElement("pre");
    pre.textContent = text;
    d.appendChild(pre);
    hwb.appendChild(d);
  }
}

const container = document.getElementById("charts");
// tick labels are canvas-drawn, so emulate links with native listeners:
// clicks in the axis-label band below the plot open that tick's commit
function attachCommitLinks(chart, cvs, commits) {
  const hit = (e) => {
    // fresh geometry: the chart may have been resized since the last event
    const area = chart.chartArea, xs = chart.scales.x;
    if (!xs || !xs.ticks || !xs.ticks.length) return null;
    const x = e.offsetX, y = e.offsetY;
    if (y < area.bottom + 2 || y > chart.height) return null;
    let best = null, bestDx = Infinity;
    for (const t of xs.ticks) {
      const dx = Math.abs(xs.getPixelForValue(t.value) - x);
      if (dx < bestDx) { bestDx = dx; best = t.value; }
    }
    if (best == null || bestDx > 50 || !commits[best]) return null;
    return commits[best];
  };
  cvs.addEventListener("mousemove", (e) => {
    cvs.style.cursor = hit(e) ? "pointer" : "";
  });
  cvs.addEventListener("click", (e) => {
    const sha = hit(e);
    if (sha) window.open(DATA.repo + "/commit/" + sha, "_self");
  });
}
// "isolated/<group>/<what>": one heading per group, the exact counts first
const GROUPS = ["instructions", "allocations", "memory", "held back", "ns per read", "ns per write"];
const groupOf = (name) => name.split("/")[1] || name;
const rank = (name) => {
  const i = GROUPS.indexOf(groupOf(name));
  return i < 0 ? GROUPS.length : i;
};
let lastGroup = null;
for (const name of Object.keys(DATA.charts).sort(
    (a, b) => rank(a) - rank(b) || a.localeCompare(b, undefined, { numeric: true }))) {
  const meta = DATA.charts[name];
  if (groupOf(name) !== lastGroup) {
    lastGroup = groupOf(name);
    const g = document.createElement("h2");
    g.className = "group";
    g.textContent = lastGroup;
    container.appendChild(g);
  }
  const sec = document.createElement("section");
  const h = document.createElement("h3");
  h.textContent = name.split("/").slice(2).join("/") + " ";
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
  const chart = new Chart(cvs, {
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
  attachCommitLinks(chart, cvs, DATA.commits[name]);
}
</script>
</body>
</html>
"""


def main():
    args = sys.argv[1:]
    # results recorded under a name benches/renames.txt renames are charted
    # under the new one
    renames = []
    if args[:1] == ["--renames"]:
        renames = bench_renames.load(args[1])
        args = args[2:]
    out_path = args[0]
    data_dirs = args[1:]

    # (commit, arch, bench name) -> point; later files (re-runs) overwrite
    points = {}
    dates = {}
    units = {}
    models = {}  # (arch, commit) -> cpu model (records may predate hardware capture)
    hardware_files = {}  # filename -> full lscpu text
    nruns = 0
    latest = {}  # arch -> (date, names in that record)
    for path in sorted(p for d in data_dirs for p in glob.glob(d + "/*.json")):
        nruns += 1
        rec = json.load(open(path))
        for r in rec["results"]:
            r["name"] = bench_renames.apply(r["name"], renames)
        if rec["arch"] not in latest or rec["date"] >= latest[rec["arch"]][0]:
            latest[rec["arch"]] = (rec["date"], {r["name"] for r in rec["results"]})
        hw = rec.get("hardware") or {}
        if hw.get("cpu_model"):
            models[(rec["arch"], rec["commit"])] = hw["cpu_model"]
        for r in rec["results"]:
            key = (rec["commit"], rec["arch"], r["name"])
            points[key] = r["value"]
            dates[key] = rec["date"]
            units[r["name"]] = r["unit"]
        # sibling runner dumps (collected next to the records)
        for hwpath in glob.glob(path.rsplit("/", 1)[0] + "/hardware-*.txt"):
            hardware_files[hwpath.rsplit("/", 1)[1]] = open(hwpath).read()

    # runners summary: per arch, which cpu models produced how many runs
    runner_counts = {}
    for (arch, _commit), model in models.items():
        runner_counts.setdefault(arch, {})
        runner_counts[arch][model] = runner_counts[arch].get(model, 0) + 1

    # group per bench name -> arch -> sorted series
    current = set().union(*(names for _date, names in latest.values())) if latest else set()
    charts = {}
    for (commit, arch, name), value in points.items():
        if name not in current:
            continue
        charts.setdefault(name, {}).setdefault(arch, []).append(
            {"commit": commit, "date": dates[(commit, arch, name)], "value": value}
        )

    labels = {}
    commits = {}
    for name, per_arch in charts.items():
        # one tick per unique (date, commit): arches record the same commits,
        # and re-runs of a commit collapse to a single point
        ticks = {}
        for pts in per_arch.values():
            for p in pts:
                ticks[(p["date"], p["commit"])] = None
        ordered = sorted(ticks.keys())
        # full commit shas per tick (labels truncate; clicks need the sha)
        commits[name] = [c for _d, c in ordered]
        # annotate a tick's label with the cpu model when it changed for an
        # arch (hardware transitions are the first suspect when trends break)
        last_model = {}
        built = []
        for d, c in ordered:
            label = f"{d[5:16].replace('T', ' ')} {c[:8]}"
            changed = []
            for arch in per_arch:
                m = models.get((arch, c))
                if m and m != last_model.get(arch):
                    changed.append(m)
                    last_model[arch] = m
            if changed:
                label += " [" + " / ".join(dict.fromkeys(changed)) + "]"
            built.append(label)
        labels[name] = built
        for arch, pts in per_arch.items():
            pts.sort(key=lambda p: (p["date"], p["commit"]))
            # map onto the shared tick list (NaN where this arch has no point)
            by_tick = {(p["date"], p["commit"]): p["value"] for p in pts}
            per_arch[arch] = [
                by_tick.get(tick, float("nan")) for tick in ordered
            ]

    payload = json.dumps(
        {
            "repo": REPO,
            "charts": charts,
            "labels": labels,
            "commits": commits,
            "units": units,
            "runner_counts": runner_counts,
            "hardware_files": hardware_files,
        }
    )

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
