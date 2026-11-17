#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: Apache-2.0
"""bench-chart.py OUT.svg REPORT.json [RELEASE...]: the README's benchmark chart.

One panel per workload, one bar per release (every release in the report,
or the ones named), each the median of its samples in a report written from
`scripts/benchmark.py` output. Lower is better in every panel; the newest
release's bar is coloured. The README's chart:

    scripts/bench-chart.py assets/bench.svg docs/benchmarks/linux-2026-11-18.json v0.1.0 v0.1.1
"""

import html
import json
import statistics
import sys

# (scenario, field, title, unit)
PANELS = [
    ("idle", "requests", "No rules", "API requests per 2 s"),
    ("unmatched-scope", "cpu_ms_per_poll", "10 rules, no match", "ms CPU per poll"),
    ("title-scope", "cpu_ms_per_poll", "10 title-scoped rules", "ms CPU per poll"),
    ("dense", "cpu_ms_per_poll", "100 rules, 2,000 lines", "ms CPU per poll"),
]

PANEL_W, BAR_H, GAP, LABEL_W, VALUE_W = 420, 28, 8, 92, 80
BG, BORDER, TEXT, HINT = "#1e1f29", "#44475a", "#e6e6e6", "#8a8fa8"
OURS, OTHERS = "#50fa7b", "#6272a4"
FONT = "font-family=\"-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif\""


def fmt(v, whole=False):
    if whole:
        return f"{v:,.0f}"
    return f"{v:,.0f}" if v >= 100 else f"{v:,.1f}" if v >= 10 else f"{v:,.2f}"


def value(sample, field):
    if field == "requests":
        return sum(sample["requests"].values())
    # A release that makes no poll has no CPU per poll; none of the panels
    # that use this field has such a sample.
    return sample[field]


def main():
    out_path, report_path, named = sys.argv[1], sys.argv[2], sys.argv[3:]
    report = json.load(open(report_path))
    for release in named:
        if release not in report["binaries"]:
            sys.exit(f"{release} is not in {report_path}")
    releases = sorted(named or report["binaries"], reverse=True)
    newest = releases[0]
    panel_h = 48 + len(releases) * (BAR_H + GAP)
    cols = 2
    rows = (len(PANELS) + cols - 1) // cols
    width = cols * PANEL_W + (cols + 1) * 20
    height = 20 + rows * (panel_h + 20)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" {FONT} font-size="16">',
        f'<rect width="100%" height="100%" rx="10" fill="{BG}" stroke="{BORDER}"/>',
    ]
    for i, (scenario, field, title, unit) in enumerate(PANELS):
        x0 = 20 + (i % cols) * (PANEL_W + 20)
        y0 = 20 + (i // cols) * (panel_h + 20)
        values = [
            statistics.median(
                value(s, field)
                for s in report["samples"]
                if s["scenario"] == scenario and s["binary"] == release
            )
            for release in releases
        ]
        top = max(values) or 1
        svg.append(
            f'<text x="{x0}" y="{y0 + 17}" font-weight="600" font-size="17" fill="{TEXT}">'
            f"{html.escape(title)}</text>"
            f'<text x="{x0 + PANEL_W}" y="{y0 + 17}" text-anchor="end" fill="{HINT}" '
            f'font-size="14">{html.escape(unit)}, lower is better</text>'
        )
        bar_w = PANEL_W - LABEL_W - VALUE_W
        for j, (release, v) in enumerate(zip(releases, values)):
            y = y0 + 34 + j * (BAR_H + GAP)
            w = max(2, bar_w * v / top)
            ours = release == newest
            weight = ' font-weight="600"' if ours else ""
            svg.append(
                f'<text x="{x0}" y="{y + 20}" fill="{TEXT}"{weight}>{html.escape(release)}</text>'
                f'<rect x="{x0 + LABEL_W}" y="{y}" width="{w:.1f}" height="{BAR_H}" rx="3" '
                f'fill="{OURS if ours else OTHERS}"/>'
                f'<text x="{x0 + LABEL_W + w + 6:.1f}" y="{y + 20}" fill="{TEXT}">'
                f'{fmt(v, field == "requests")}</text>'
            )
    svg.append("</svg>")
    open(out_path, "w").write("\n".join(svg) + "\n")


if __name__ == "__main__":
    main()
