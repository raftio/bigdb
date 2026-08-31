#!/usr/bin/env python3
"""Render the README benchmark charts to SVG.

The numbers here are transcribed from bench/results/REPORT.md; edit them there
first, then re-run this to regenerate docs/img/*.svg:

    python3 bench/charts/generate.py

One file per chart, not one per theme. The background is transparent and every
ink is a mid-tone that clears 3.6:1 on both GitHub surfaces (#ffffff and
#0d1117), because an <img> cannot see the theme of the page hosting it -
`prefers-color-scheme` reports the OS setting, so a two-file <picture> swap
renders the dark variant on a light page whenever the two disagree.

Stdlib only, no dependencies.
"""

from __future__ import annotations

import math
import os

OUT = os.path.join(os.path.dirname(__file__), "..", "..", "docs", "img")

FONT = 'system-ui,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif'

# Contrast against #ffffff / #0d1117, in that order.
INK = "#6e6d68"     # titles and the emphasised engine   5.18 / 3.65
INK2 = "#7f7e77"    # values and category labels         4.08 / 4.64
MUTED = "#86857e"   # unit captions                      3.70 / 5.11
RULE = "#8f8e88"    # baselines, at 45% so they recede on either surface
ACCENT = "#2a78d6"  # big                                4.42 / 4.29
QUIET = "#8f8e88"   # every other engine                 3.29 / 5.76
SLOWER = "#e34948"  # diverging: big behind              3.95 / 4.79


# -------------------------------------------------------------- primitives --

def esc(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, *, size, fill, anchor="middle", weight=400, tabular=False):
    extra = ' style="font-variant-numeric:tabular-nums"' if tabular else ""
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" '
            f'font-weight="{weight}" text-anchor="{anchor}"{extra}>{esc(s)}</text>')


def column(x, base_y, h, w, fill, *, up=True):
    """A column growing from a baseline: square at the baseline, 4px round at
    the data end."""
    h = max(h, 1.5)
    r = min(4.0, w / 2.0, h / 2.0)
    if up:
        top = base_y - h
        d = (f"M{x:.1f},{base_y:.1f} V{top + r:.1f} A{r:.1f},{r:.1f} 0 0 1 {x + r:.1f},{top:.1f} "
             f"H{x + w - r:.1f} A{r:.1f},{r:.1f} 0 0 1 {x + w:.1f},{top + r:.1f} "
             f"V{base_y:.1f} Z")
    else:
        bot = base_y + h
        d = (f"M{x:.1f},{base_y:.1f} V{bot - r:.1f} A{r:.1f},{r:.1f} 0 0 0 {x + r:.1f},{bot:.1f} "
             f"H{x + w - r:.1f} A{r:.1f},{r:.1f} 0 0 0 {x + w:.1f},{bot - r:.1f} "
             f"V{base_y:.1f} Z")
    return f'<path d="{d}" fill="{fill}"/>'


def hrule(y, x0, x1, stroke=RULE):
    return (f'<path d="M{x0:.1f},{y:.1f} H{x1:.1f}" stroke="{stroke}" '
            f'stroke-opacity="0.45" stroke-width="1" shape-rendering="crispEdges"/>')


def svg(w, h, body, label):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
            f'viewBox="0 0 {w} {h}" role="img" aria-label="{esc(label)}" '
            f'font-family=\'{FONT}\'>\n' + "\n".join(body) + "\n</svg>\n")


def write(stem: str, markup: str):
    path = os.path.join(OUT, f"{stem}.svg")
    with open(path, "w") as f:
        f.write(markup)
    print("wrote", os.path.normpath(path))


# ------------------------------------------------------------------- data --

# big against the analytical engines: the six queries summed, in microseconds.
ANALYTICAL_TOTAL = [("big", 21019), ("duckdb", 21323),
                    ("datafusion", 51034), ("clickhouse", 97866)]

# (query, big µs, duckdb µs)
QUERIES = [("count_ge", 2842, 1895), ("intersect", 4334, 7321),
           ("sum", 3420, 2559), ("group_by", 2333, 3345),
           ("top_n", 1742, 3171), ("distinct", 6348, 3032)]

KV = ["big", "redb", "lmdb", "fjall", "sqlite"]

PANELS = [
    ("Ingest, 100k records at full durability", "milliseconds · lower is better",
     [1415, 3150, 3157, 2335, 2663], lambda v: f"{v:,} ms"),
    ("Removals, 10k of 100k records", "milliseconds · lower is better",
     [25, 967, 346, 216, 359], lambda v: f"{v:,} ms"),
    ("Uncompacted file size", "MiB · lower is better",
     [0.6, 8.5, 10.6, 97.1, 2.8], lambda v: f"{v:g} MiB"),
    ("Point reads, single thread", "thousands/sec · HIGHER is better",
     [59, 464, 558, 426, 100], lambda v: f"{v:,}k/s"),
]


# ------------------------------------------------- chart 1: analytical sum --

def analytical_total() -> str:
    slot, thick, margin = 88, 24, 28
    rows = ANALYTICAL_TOTAL
    plot_w = slot * len(rows)
    W = margin * 2 + plot_w
    base_y, col_max = 264, 158
    H = 302

    peak = max(v for _, v in rows)
    b = [text(margin, 28, "All six analytical queries, summed", size=15,
              fill=INK, anchor="start", weight=600),
         text(margin, 50, "microseconds · lower is better", size=12,
              fill=MUTED, anchor="start"),
         hrule(base_y, margin, margin + plot_w)]

    for i, (name, v) in enumerate(rows):
        cx = margin + slot * i + slot / 2
        h = col_max * v / peak
        lead = name == "big"
        b.append(column(cx - thick / 2, base_y, h, thick,
                        ACCENT if lead else QUIET))
        b.append(text(cx, base_y - h - 9, f"{v:,}µs", size=12,
                      fill=INK if lead else INK2, tabular=True))
        b.append(text(cx, base_y + 20, name, size=12,
                      fill=INK if lead else INK2, weight=600 if lead else 400))

    return svg(W, H, b, "All six analytical queries summed: big 21,019µs, "
                        "duckdb 21,323µs, datafusion 51,034µs, clickhouse 97,866µs")


# ------------------------------------------------- chart 2: big vs duckdb --

def vs_duckdb() -> str:
    slot, thick, margin = 96, 24, 28
    plot_w = slot * len(QUERIES)
    W = margin * 2 + plot_w
    zero_y, arm = 226, 132
    H = 414
    span = math.log(2.2)

    b = [text(margin, 28, "big against duckdb, one query at a time", size=15,
              fill=INK, anchor="start", weight=600),
         text(margin, 50, "above the line big wins the query · below it duckdb does",
              size=12, fill=MUTED, anchor="start"),
         text(margin, 68, "column length is log-scaled, so ahead and behind are symmetric",
              size=11, fill=MUTED, anchor="start"),
         hrule(zero_y, margin, margin + plot_w)]

    for i, (name, big_us, duck_us) in enumerate(QUERIES):
        cx = margin + slot * i + slot / 2
        ratio = duck_us / big_us
        ahead = ratio >= 1.0
        factor = ratio if ahead else 1.0 / ratio
        h = arm * math.log(factor) / span
        b.append(column(cx - thick / 2, zero_y, h, thick,
                        ACCENT if ahead else SLOWER, up=ahead))
        lbl = f"{factor:.2f}× {'faster' if ahead else 'slower'}"
        b.append(text(cx, zero_y - h - 9 if ahead else zero_y + h + 17, lbl,
                      size=11.5, fill=INK, tabular=True))
        b.append(text(cx, H - 16, name, size=12, fill=INK2))

    b.append(text(margin, zero_y - 8, "1.00× — dead heat", size=11,
                  fill=MUTED, anchor="start"))
    return svg(W, H, b, "big against duckdb per query: intersect 1.69x, group_by "
                        "1.43x and top_n 1.82x faster; count_ge 1.50x, sum 1.34x "
                        "and distinct 2.09x slower")


# ---------------------------------------- chart 3: the transactional four --

def transactional() -> str:
    slot, thick = 68, 22
    pad, gutter, inset = 12, 36, 12
    plot_w = slot * len(KV)
    pw = inset * 2 + plot_w
    W = pad * 2 + pw * 2 + gutter
    head, col_max = 60, 104
    ph = head + col_max + 34
    H = pad * 2 + ph * 2 + 30

    b = []
    for idx, (title, sub, values, fmt) in enumerate(PANELS):
        px = pad + (idx % 2) * (pw + gutter)
        py = pad + (idx // 2) * (ph + 30)
        x0 = px + inset
        base_y = py + head + col_max
        peak = max(values)

        b.append(text(px, py + 13, title, size=13, fill=INK,
                      anchor="start", weight=600))
        b.append(text(px, py + 30, sub, size=11, fill=MUTED, anchor="start"))
        b.append(hrule(base_y, x0, x0 + plot_w))

        for i, (name, v) in enumerate(zip(KV, values)):
            cx = x0 + slot * i + slot / 2
            h = col_max * v / peak
            lead = name == "big"
            b.append(column(cx - thick / 2, base_y, h, thick,
                            ACCENT if lead else QUIET))
            b.append(text(cx, base_y - h - 8, fmt(v), size=11,
                          fill=INK if lead else INK2, tabular=True))
            b.append(text(cx, base_y + 18, name, size=11,
                          fill=INK if lead else INK2, weight=600 if lead else 400))

    return svg(W, H, b, "Ingest, removals, uncompacted file size and single-thread "
                        "point reads for big, redb, lmdb, fjall and sqlite")


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    write("analytical-total", analytical_total())
    write("big-vs-duckdb", vs_duckdb())
    write("transactional-four", transactional())
