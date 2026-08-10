#!/usr/bin/env python3
"""Turn the two Athena result sets into the published payloads.

Usage: build_stats.py CUBE_CSV WINDOWS_CSV PRIOR_HISTORY OUT_DIR

PRIOR_HISTORY is the history published by the last run, or "" on the first one.
It is merged rather than replaced, because the access logs expire at 365 days
and the query can only ever see that far back. Anything older survives here or
not at all.

Writes stats.json (public badge payload), metrics.json, history.json,
history-recent.json and history-monthly.json into OUT_DIR.
"""

import csv
import datetime
import json
import sys

WINDOW_DAYS = 30
RECENT_DAYS = 90

# Daily fields that are counts, and so add up across days.
COUNTS = ("downloads", "unique", "failures")
# Sparse per-day breakdowns of download hits by dimension.
SPLITS = ("platforms", "channels", "versions")


def read_rows(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def as_int(row, key):
    """Read an integer from either a CSV row or a parsed JSON row."""
    value = row.get(key)
    if isinstance(value, str):
        value = value.strip()
    return int(value) if value not in (None, "") else 0


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


def blank_day(day):
    row = {"day": day, "downloads": 0, "unique": 0, "active": 0, "failures": 0}
    for name in SPLITS:
        row[name] = {}
    return row


def normalise(row):
    """Bring a row published by an older schema up to the current shape."""
    out = blank_day(row.get("day", ""))
    for key in ("downloads", "unique", "active", "failures"):
        out[key] = as_int(row, key)
    for name in SPLITS:
        value = row.get(name)
        if isinstance(value, dict):
            out[name] = {k: int(v) for k, v in value.items() if v}
    return out


def split_cube(rows):
    """Detail rows carry every dimension; rollup rows are one per day and kind.

    The rollup rows are why distinct-device counts are exact per day. Summing
    the detail rows' device counts would count one machine once per platform it
    fetched, which is not what a daily install figure means.
    """
    detail, daily = [], []
    for row in rows:
        (daily if as_int(row, "is_rollup") else detail).append(row)
    return detail, daily


def build_days(daily, detail):
    """One row per day, with sparse dimensional breakdowns.

    Every figure here is exactly recoverable. Counts come from the rollup row
    for that day and kind, and the dimensional splits are download hits, which
    add up cleanly because a hit belongs to exactly one combination.
    """
    days = {}

    for row in daily:
        day = days.setdefault(row["day"], blank_day(row["day"]))
        kind = row["kind"]
        if kind == "download":
            day["downloads"] = as_int(row, "hits")
            day["unique"] = as_int(row, "devices")
        elif kind == "failure":
            day["failures"] = as_int(row, "hits")
        elif kind == "heartbeat":
            day["active"] = as_int(row, "devices")

    for row in detail:
        if row["kind"] != "download":
            continue
        day = days.setdefault(row["day"], blank_day(row["day"]))
        hits = as_int(row, "hits")
        if row["os"] and row["arch"]:
            platform = f"{row['os']}/{row['arch']}"
            day["platforms"][platform] = day["platforms"].get(platform, 0) + hits
        for name, column in (("channels", "channel"), ("versions", "version")):
            value = row[column]
            if value:
                day[name][value] = day[name].get(value, 0) + hits

    return days


def merge(prior, fresh):
    """Prior days, with the ones the query still covers replaced.

    Replaced rather than added to: the query re-derives the same days on every
    run, and the newest day is always partial when first seen.
    """
    merged = {row["day"]: row for row in prior}
    merged.update(fresh)
    return [merged[day] for day in sorted(merged)]


def monthly(days):
    """Monthly rollups, the grain a multi-year chart can actually draw.

    Versions are left out. Which build a machine ran is a question about a
    rollout in progress, answered by the daily rows; which platforms people are
    on is a question about years, and belongs here.
    """
    months = {}
    for row in days:
        key = row["day"][:7]
        month = months.setdefault(
            key,
            {
                "month": key,
                "days": 0,
                "downloads": 0,
                "unique": 0,
                "failures": 0,
                "activePeak": 0,
                "platforms": {},
                "channels": {},
            },
        )
        month["days"] += 1
        for name in COUNTS:
            month[name] += row[name]
        # Distinct devices do not add up across days, so the month carries its
        # highest daily figure rather than a sum that would mean nothing.
        month["activePeak"] = max(month["activePeak"], row["active"])
        for name in ("platforms", "channels"):
            for dimension, hits in row[name].items():
                month[name][dimension] = month[name].get(dimension, 0) + hits
    return [months[key] for key in sorted(months)]


def main():
    cube_path, windows_path, history_path, out_dir = sys.argv[1:5]

    detail, daily = split_cube(read_rows(cube_path))
    windows = read_rows(windows_path)
    w = windows[0] if windows else {}

    now = datetime.datetime.now(datetime.timezone.utc)
    stamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")

    # No try/except here on purpose. The workflow has already distinguished a
    # missing object from a failed read, so anything unreadable at this point
    # is a real fault, and starting a fresh history would quietly discard
    # every day the logs no longer hold.
    prior = []
    if history_path:
        with open(history_path) as f:
            prior = [normalise(row) for row in json.load(f).get("days", [])]

    days = merge(prior, build_days(daily, detail))

    # Cumulative figures come from the history, not from the scan, so they
    # cannot shrink as logs expire. Summing daily uniques is exact because the
    # scan dedupes on request IP, User-Agent and day: the day is inside the
    # key, so one machine downloading on two days is already two.
    total = sum(row["downloads"] for row in days)
    deduped = sum(row["unique"] for row in days)
    first_log = days[0]["day"] if days else ""

    scanned_total, scanned_unique = as_int(w, "total"), as_int(w, "deduped")
    if days and (total, deduped) != (scanned_total, scanned_unique):
        print(
            f"note: history totals {total}/{deduped} differ from the scan's "
            f"{scanned_total}/{scanned_unique}. Expected once days age out of "
            f"the 365 day log window, since the history keeps them and the "
            f"scan cannot. Unexpected before then.",
            file=sys.stderr,
        )

    downloads_7d = as_int(w, "downloads_7d")
    failures_7d = as_int(w, "failures_7d")
    downloads_30d = as_int(w, "downloads_30d")
    unique_30d = as_int(w, "unique_30d")

    # The badge shows a rolling thirty days, not a cumulative count.
    #
    # CloudFront access logging was enabled months after the first release, so
    # everything before that is unlogged and unrecoverable. A cumulative figure
    # labelled "downloads" would be read as lifetime and would understate the
    # project by however many installs happened first. A rolling window makes
    # no claim about a past it cannot see, and it is the same number whether or
    # not anyone knows when logging started.
    #
    # The cumulative figures stay in the payload, paired with `since` so that
    # anything rendering them can say what they count from.
    stats = {
        "schemaVersion": 1,
        "label": "downloads (30d)",
        "message": short(unique_30d),
        "color": "7C5CFF",
        "last30d": {
            "downloads": downloads_30d,
            "unique": unique_30d,
        },
        "total": total,
        "unique": deduped,
        "since": first_log,
        "updated": stamp,
    }

    cutoff = str((now.date() - datetime.timedelta(days=WINDOW_DAYS)))
    recent = [r for r in detail if r["kind"] == "download" and r["day"] >= cutoff]
    recent_total = sum(as_int(r, "hits") for r in recent)

    attempts_7d = downloads_7d + failures_7d

    metrics = {
        "schemaVersion": 1,
        "generated": stamp,
        "windowDays": WINDOW_DAYS,
        "downloads": {
            "total": total,
            "unique": deduped,
            # What the cumulative figures count from. Logging started long
            # after the first release, so they are not lifetime totals.
            "since": first_log,
            "last30d": downloads_30d,
            "unique30d": unique_30d,
            "last7d": downloads_7d,
            # Deduplicated, so it can be compared against a visit count. The
            # raw last7d cannot: it counts fetches, and a machine that fetches
            # twice would read as two installs.
            "unique7d": as_int(w, "unique_7d"),
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

    # Three grains of the same series. A widget refetches on every refresh and
    # cannot afford the archive, and a chart a few hundred points wide cannot
    # draw a decade of days regardless.
    payloads = {
        "stats.json": stats,
        "metrics.json": metrics,
        "history.json": {"schemaVersion": 2, "generated": stamp, "days": days},
        "history-recent.json": {
            "schemaVersion": 2,
            "generated": stamp,
            "days": days[-RECENT_DAYS:],
        },
        "history-monthly.json": {
            "schemaVersion": 2,
            "generated": stamp,
            "months": monthly(days),
        },
    }

    for name, payload in payloads.items():
        with open(f"{out_dir}/{name}", "w") as f:
            json.dump(payload, f, indent=2 if name != "stats.json" else None)
            f.write("\n")
        print(f"{name}: {len(json.dumps(payload))} bytes")

    print(f"history: {len(days)} days, {len(prior)} carried in, {first_log} onward")


if __name__ == "__main__":
    main()
