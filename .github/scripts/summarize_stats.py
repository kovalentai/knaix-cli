#!/usr/bin/env python3
"""Render metrics.json as the workflow's step summary.

Usage: summarize_stats.py METRICS_JSON
"""

import json
import sys


def table(title, rows):
    if not rows:
        return []
    out = [f"**{title}**", "", "| | downloads | share |", "|---|---:|---:|"]
    for row in rows[:6]:
        out.append(f"| {row['name']} | {row['downloads']} | {row['share']:.0%} |")
    out.append("")
    return out


def main():
    with open(sys.argv[1]) as f:
        m = json.load(f)

    d, a, i = m["downloads"], m["active"], m["installs"]

    lines = [
        "### Knaix CLI fleet health",
        "",
        f"- Downloads: **{d['unique30d']}** unique in the last 30 days "
        f"(the badge figure), {d['last7d']} fetches in the last 7",
        f"- Since logging began ({d['since'] or 'unknown'}): {d['total']} fetches, {d['unique']} unique",
        f"- Active installs: **{a['month']}** monthly, {a['week']} weekly, {a['day']} daily",
        f"- Install failures: {i['failures7d']} of {i['attempts7d']} attempts "
        f"in 7 days ({i['failureRate7d']:.1%})",
        "",
    ]

    lines += table(f"Versions (last {m['windowDays']} days)", m["versions"])
    lines += table("Platforms", m["platforms"])
    lines += table("Channels", m["channels"])

    lines += [
        "Published to `releases.knaix.com/stats.json` (public) and "
        "`/metrics/{metrics,history}.json` (key required).",
    ]

    print("\n".join(lines))


if __name__ == "__main__":
    main()
