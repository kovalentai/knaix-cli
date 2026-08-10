#!/usr/bin/env python3
"""Regression suite for build_stats.py.

Standard library only, so CI needs no install step:

    python3 .github/scripts/test_build_stats.py

The history is append-only and the access logs expire at 365 days, so most of
what is asserted here is that a bad day cannot quietly shorten the record.
"""

import datetime
import json
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "build_stats.py"
WORKFLOW = HERE.parent / "workflows" / "download-stats.yml"

# is_rollup is 0 on detail rows and 15 on the rollup rows, which is what
# grouping(version, os, arch, channel) returns when all four are aggregated
# away. Both days have one machine fetching two platforms, so the rollup
# device count is deliberately lower than the sum of the detail rows.
CUBE = """day,kind,version,os,arch,channel,is_rollup,hits,devices
2026-08-08,download,0.5.0,darwin,arm64,homebrew,0,10,8
2026-08-08,download,0.5.0,linux,x86_64,install.sh,0,5,5
2026-08-08,download,,,,,15,15,12
2026-08-08,heartbeat,,,,cli,15,40,40
2026-08-08,failure,,,,,15,2,2
2026-08-09,download,0.6.0,darwin,arm64,homebrew,0,20,18
2026-08-09,download,0.5.0,windows,x86_64,powershell,0,3,3
2026-08-09,download,,,,,15,23,21
2026-08-09,heartbeat,,,,cli,15,45,45
2026-08-09,failure,,,,,15,1,1
"""

# Describes only what the logs still hold: the two August days.
WINDOWS = """total,deduped,active_1d,active_7d,active_30d,downloads_7d,unique_7d,failures_7d,downloads_30d,unique_30d,first_log,latest
38,33,45,60,120,38,33,3,38,33,2026-08-08,2026-08-09
"""

# Schema 1 shape: no unique, no splits. 2026-07-01 has aged out of the log
# window and exists nowhere else. 2026-08-08 is stale and must be overwritten.
PRIOR = {
    "schemaVersion": 1,
    "generated": "2026-08-08T07:20:00Z",
    "days": [
        {"day": "2026-07-01", "downloads": 7, "active": 30, "failures": 0},
        {"day": "2026-08-08", "downloads": 4, "active": 11, "failures": 9},
    ],
}

failures = []


def check(label, actual, expected):
    if actual == expected:
        print(f"  ok   {label}")
    else:
        print(f"  FAIL {label}\n         got      {actual!r}\n         expected {expected!r}")
        failures.append(label)


def build(tmp, prior):
    """Run the builder. prior=None means a declared first run."""
    (tmp / "cube.csv").write_text(CUBE)
    (tmp / "windows.csv").write_text(WINDOWS)
    args = [sys.executable, str(SCRIPT), str(tmp / "cube.csv"), str(tmp / "windows.csv")]
    if prior is None:
        args.append("")
    else:
        (tmp / "prior.json").write_text(json.dumps(prior))
        args.append(str(tmp / "prior.json"))
    args.append(str(tmp))
    return subprocess.run(args, capture_output=True, text=True)


def payloads(tmp):
    return {
        name: json.loads((tmp / name).read_text())
        for name in ("stats.json", "metrics.json", "history.json",
                     "history-recent.json", "history-monthly.json")
    }


print("merge over a prior history")
with tempfile.TemporaryDirectory() as d:
    tmp = Path(d)
    proc = build(tmp, PRIOR)
    check("exits clean", proc.returncode, 0)
    p = payloads(tmp)
    days = {row["day"]: row for row in p["history.json"]["days"]}

    check("aged-out day survives", days["2026-07-01"]["downloads"], 7)
    check("schema 1 row gains unique", days["2026-07-01"]["unique"], 0)
    check("schema 1 row gains empty splits", days["2026-07-01"]["platforms"], {})
    check("stale day is replaced", days["2026-08-08"]["downloads"], 15)
    check("replaced day drops stale failures", days["2026-08-08"]["failures"], 2)

    check("daily unique comes from the rollup row", days["2026-08-08"]["unique"], 12)
    check("and not from summing detail rows", days["2026-08-08"]["unique"] != 13, True)
    check("daily active comes from the heartbeat rollup", days["2026-08-09"]["active"], 45)

    check("platform split", days["2026-08-09"]["platforms"],
          {"darwin/arm64": 20, "windows/x86_64": 3})
    check("channel split", days["2026-08-09"]["channels"],
          {"homebrew": 20, "powershell": 3})
    check("version split", days["2026-08-09"]["versions"], {"0.6.0": 20, "0.5.0": 3})

    check("total spans the history, not the scan", p["stats.json"]["total"], 7 + 15 + 23)
    check("unique sums the daily uniques", p["stats.json"]["unique"], 0 + 12 + 21)
    check("since is the earliest retained day", p["stats.json"]["since"], "2026-07-01")
    check("divergence from the scan is reported", "differ from the scan" in proc.stderr, True)

    check("schemaVersion is 2", p["history.json"]["schemaVersion"], 2)
    check("days are ordered", [r["day"] for r in p["history.json"]["days"]],
          ["2026-07-01", "2026-08-08", "2026-08-09"])

    months = {m["month"]: m for m in p["history-monthly.json"]["months"]}
    check("monthly downloads", months["2026-08"]["downloads"], 38)
    check("monthly unique sums", months["2026-08"]["unique"], 33)
    check("monthly active is a peak, not a sum", months["2026-08"]["activePeak"], 45)
    check("monthly observed-day count", months["2026-08"]["days"], 2)
    check("monthly platforms fold up", months["2026-08"]["platforms"],
          {"darwin/arm64": 30, "linux/x86_64": 5, "windows/x86_64": 3})
    check("monthly carries no versions", "versions" in months["2026-08"], False)

    check("recent window holds every day here", len(p["history-recent.json"]["days"]), 3)

    # Rollup rows in the breakdowns would double the denominator.
    check("platform shares sum to 1",
          round(sum(x["share"] for x in p["metrics.json"]["platforms"]), 4), 1.0)
    check("no empty-named breakdown row",
          all(x["name"] for x in p["metrics.json"]["platforms"]), True)
    check("breakdown counts exclude rollups",
          sum(x["downloads"] for x in p["metrics.json"]["platforms"]), 38)

print("\nfirst run, no prior history")
with tempfile.TemporaryDirectory() as d:
    tmp = Path(d)
    proc = build(tmp, None)
    check("a declared first run is allowed", proc.returncode, 0)
    p = payloads(tmp)
    check("history starts from the scan", len(p["history.json"]["days"]), 2)
    check("total matches the scan exactly", p["stats.json"]["total"], 38)
    check("unique matches the scan exactly", p["stats.json"]["unique"], 33)
    check("no divergence reported", "differ from the scan" in proc.stderr, False)

print("\na long archive survives intact")
with tempfile.TemporaryDirectory() as d:
    tmp = Path(d)
    # Longer than the 365 day log window, so anything that quietly trims to a
    # rolling window loses days here. That trim is the defect this file exists
    # to prevent, and a three-day fixture cannot see it.
    start = datetime.date(2026, 8, 8) - datetime.timedelta(days=399)
    long_prior = {"schemaVersion": 2, "days": [
        {"day": str(start + datetime.timedelta(days=i)), "downloads": 1,
         "unique": 1, "active": 1, "failures": 0}
        for i in range(400)
    ]}
    proc = build(tmp, long_prior)
    check("exits clean", proc.returncode, 0)
    out = json.loads((tmp / "history.json").read_text())["days"]
    check("every carried day survives, plus the new one", len(out), 401)
    check("the oldest day is still there", out[0]["day"], str(start))
    check("since reaches back to it", json.loads((tmp / "stats.json").read_text())["since"],
          str(start))
    check("recent stays bounded", len(json.loads((tmp / "history-recent.json").read_text())["days"]), 90)
    check("monthly spans the archive",
          len(json.loads((tmp / "history-monthly.json").read_text())["months"]) > 12, True)

print("\nidempotence")
with tempfile.TemporaryDirectory() as d:
    tmp = Path(d)
    build(tmp, None)
    first = json.loads((tmp / "history.json").read_text())
    build(tmp, first)
    second = json.loads((tmp / "history.json").read_text())
    check("re-running against its own output is stable", second["days"], first["days"])

print("\nthe archive cannot be shortened")
with tempfile.TemporaryDirectory() as d:
    tmp = Path(d)
    # An archive that exists but carries no days. Publishing over it would
    # look exactly like a first run, which is what makes it dangerous.
    proc = build(tmp, {"schemaVersion": 2, "days": []})
    check("an existing archive with no days is refused", proc.returncode != 0, True)
    check("and says why", "not a first run" in proc.stderr, True)
    check("and writes nothing", (tmp / "history.json").exists(), False)

# The tripwire on merge. Unreachable while merge is a union, which is the
# point of it, so it is exercised directly rather than through the CLI.
sys.path.insert(0, str(HERE))
import build_stats  # noqa: E402

carried = [{"day": "2026-07-01"}, {"day": "2026-07-02"}, {"day": "2026-08-08"}]
check("a correct merge loses nothing",
      build_stats.lost_days(carried, build_stats.merge(carried, {})), [])
check("a union with fresh days loses nothing",
      build_stats.lost_days(carried, build_stats.merge(carried, {"2026-08-09": {"day": "2026-08-09"}})), [])
check("a merge that dropped a day is caught",
      build_stats.lost_days(carried, [{"day": "2026-07-02"}, {"day": "2026-08-08"}]),
      ["2026-07-01"])
check("a merge trimmed to a window is caught",
      build_stats.lost_days(carried, [{"day": "2026-08-08"}]),
      ["2026-07-01", "2026-07-02"])

print("\nexistence is not decided by error prose")
workflow = WORKFLOW.read_text()
check("the fetch asks S3 for a count", "list-objects-v2" in workflow, True)
check("no stderr capture file", "fetch-err" in workflow, False)
check("no error text is matched on",
      any(p in workflow for p in ("NoSuchKey", "Not Found")), False)

print()
if failures:
    print(f"{len(failures)} check(s) failed")
    sys.exit(1)
print("all checks passed")
