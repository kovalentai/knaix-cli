#!/usr/bin/env python3
"""Turn the two Athena result sets into the three published payloads.

Usage: build_stats.py CUBE_CSV WINDOWS_CSV OUT_DIR

Writes stats.json (public badge payload), metrics.json and history.json
(private fleet health) into OUT_DIR.
"""

import csv
import datetime
import json
import sys

WINDOW_DAYS = 30


def read_rows(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def as_int(row, key):
    value = (row.get(key) or "").strip()
    return int(value) if value else 0


def short(n):
    """Badge-sized rendering. 1234 -> 1.2k, 1234567 -> 1.2M."""
    if n < 1000:
        return str(n)
    if n < 1_000_000:
        k = n / 1000
        return f"{k:.1f}k" if k < 10 else f"{k:.0f}k"
    m = n / 1_000_000
    return f"{m:.1f}M" if m < 10 else f"{m:.0f}M"


def share(part, whole):
    return round(part / whole, 4) if whole else 0.0


def breakdown(rows, key, total):
    """Downloads grouped by one dimension, largest first."""
    totals = {}
    for row in rows:
        totals[key(row)] = totals.get(key(row), 0) + as_int(row, "hits")
    return [
        {"name": name, "downloads": hits, "share": share(hits, total)}
        for name, hits in sorted(totals.items(), key=lambda kv: (-kv[1], kv[0]))
        if name
    ]


def build_history(cube):
    """Daily series.

    Only exactly-recoverable figures go in here. Download and failure counts
    are request counts, which sum cleanly across the cube's dimensions. Active
    installs are distinct devices, which do not sum, but heartbeat rows carry
    no version, platform or channel and so collapse to one row per day, making
    that row's device count the exact daily figure.
    """
    days = {}
    for row in cube:
        day = days.setdefault(
            row["day"], {"day": row["day"], "downloads": 0, "active": 0, "failures": 0}
        )
        kind = row["kind"]
        if kind == "download":
            day["downloads"] += as_int(row, "hits")
        elif kind == "failure":
            day["failures"] += as_int(row, "hits")
        elif kind == "heartbeat":
            day["active"] += as_int(row, "devices")
    return [days[d] for d in sorted(days)]


def main():
    cube_path, windows_path, out_dir = sys.argv[1], sys.argv[2], sys.argv[3]

    cube = read_rows(cube_path)
    windows = read_rows(windows_path)
    w = windows[0] if windows else {}

    now = datetime.datetime.now(datetime.timezone.utc)
    stamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    total = as_int(w, "total")
    deduped = as_int(w, "deduped")
    downloads_7d = as_int(w, "downloads_7d")
    failures_7d = as_int(w, "failures_7d")

    # The badge payload keeps the shape and the grain it has always had. The
    # sites read this file, so a redefinition here would silently restate a
    # public number.
    stats = {
        "schemaVersion": 1,
        "label": "downloads",
        "message": short(deduped),
        "color": "7C5CFF",
        "total": total,
        "unique": deduped,
        "updated": stamp,
    }

    history = build_history(cube)

    cutoff = str((now.date() - datetime.timedelta(days=WINDOW_DAYS)))
    recent = [r for r in cube if r["kind"] == "download" and r["day"] >= cutoff]
    recent_total = sum(as_int(r, "hits") for r in recent)

    attempts_7d = downloads_7d + failures_7d

    metrics = {
        "schemaVersion": 1,
        "generated": stamp,
        "windowDays": WINDOW_DAYS,
        "downloads": {
            "total": total,
            "unique": deduped,
            "last7d": downloads_7d,
        },
        "active": {
            "day": as_int(w, "active_1d"),
            "week": as_int(w, "active_7d"),
            "month": as_int(w, "active_30d"),
        },
        "installs": {
            "attempts7d": attempts_7d,
            "failures7d": failures_7d,
            "failureRate7d": share(failures_7d, attempts_7d),
        },
        "versions": breakdown(recent, lambda r: r["version"], recent_total),
        "platforms": breakdown(
            recent,
            lambda r: f"{r['os']}/{r['arch']}" if r["os"] and r["arch"] else "",
            recent_total,
        ),
        "channels": breakdown(recent, lambda r: r["channel"], recent_total),
        # Published alongside the numbers so that whoever reads them later,
        # including us, does not have to reconstruct what they actually count.
        "caveats": {
            "active": (
                "Distinct request IP plus User-Agent on /latest-version, counted only "
                "for the CLI's own check-in. The CLI sends no User-Agent, so this is "
                "effectively distinct IP: an office behind one NAT reads as one "
                "install, and a laptop on three networks reads as three."
            ),
            "unique": (
                "Distinct IP, User-Agent and day. The same machine downloading on two "
                "days counts twice, which is what keeps a resumed download from "
                "counting twice within one day."
            ),
        },
    }

    payloads = {
        "stats.json": stats,
        "metrics.json": metrics,
        "history.json": {
            "schemaVersion": 1,
            "generated": stamp,
            "days": history,
        },
    }

    for name, payload in payloads.items():
        with open(f"{out_dir}/{name}", "w") as f:
            json.dump(payload, f, indent=2 if name != "stats.json" else None)
            f.write("\n")
        print(f"{name}: {len(json.dumps(payload))} bytes")


if __name__ == "__main__":
    main()
