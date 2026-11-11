#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Compare replay benchmark results side by side.

usage: summarize-gpu.py <result.json>...

Each file is the output of `replay run` or `linklite-bench replay --cpu|--gpu`. Columns are
labelled with the file names (without the directory and .json). Rows are the clip, dock family and
gamma combinations; the first file is the reference for the checksum comparison. A checksum that
differs from the reference is flagged MISMATCH and makes the script exit 1, since every path must
produce byte-identical records. GPU time and the encoder's own wall time show only for GPU runs.
"""
import json
import os
import sys


def label(path):
    return os.path.splitext(os.path.basename(path))[0]


def main(paths):
    if not paths:
        sys.exit(__doc__)
    runs = []
    for path in paths:
        with open(path) as f:
            runs.append({(r["clip"], r["family"], r["gamma"]): r for r in json.load(f)["results"]})
    labels = [label(p) for p in paths]
    keys = sorted(set().union(*runs))
    width = max(12, *(len(l) for l in labels))
    mismatch = False

    def row(title, cells):
        print(f"  {title:<22}" + "".join(f"{c:>{width + 2}}" for c in cells))

    for key in keys:
        print(f"\n{key[0]} / {key[1]} / gamma {key[2]}")
        row("", labels)
        present = [r.get(key) for r in runs]

        def metric(title, get, fmt="{:.3f}"):
            row(title, [fmt.format(get(r)) if r else "-" for r in present])

        def optional(title, get, fmt="{:.3f}"):
            # Only GPU-fed runs report these; CPU runs show "-".
            values = []
            for r in present:
                try:
                    values.append(fmt.format(get(r)) if r else "-")
                except KeyError:
                    values.append("-")
            if any(v != "-" for v in values):
                row(title, values)

        metric("cpu ms/frame mean", lambda r: r["cpu_ms_per_frame"]["mean"])
        metric("cpu ms/frame p50", lambda r: r["cpu_ms_per_frame"]["p50"])
        metric("cpu ms/frame p95", lambda r: r["cpu_ms_per_frame"]["p95"])
        metric("latency ms p95", lambda r: r["wall_ms_per_frame"]["p95"])
        optional("gpu ms/frame mean", lambda r: r["gpu_ms_per_frame"]["mean"])
        optional("gpu ms/frame p95", lambda r: r["gpu_ms_per_frame"]["p95"])
        for stage in ("submit", "take", "plan", "encode", "pack"):
            metric(f"  {stage} ms/frame", lambda r, s=stage: r["stage_ms_per_frame"][s])
        optional("  gpu_encoder ms/frame", lambda r: r["stage_ms_per_frame"]["gpu_encoder"])
        metric("record bytes", lambda r: r["record_bytes"], "{:,}")
        reference = next((r for r in present if r), None)
        cells = []
        for r in present:
            if not r:
                cells.append("-")
            elif r["checksum"] == reference["checksum"]:
                cells.append(r["checksum"][:12])
            else:
                cells.append("MISMATCH")
                mismatch = True
        row("checksum", cells)
        base = present[0]
        if base and len(present) > 1:
            ratios = [f"{r['cpu_ms_per_frame']['mean'] / base['cpu_ms_per_frame']['mean']:.2f}x" if r and base["cpu_ms_per_frame"]["mean"] else "-" for r in present]
            row("cpu vs first", ratios)

    if mismatch:
        print("\nCHECKSUM MISMATCH: the compared paths did not produce identical records.", file=sys.stderr)
        sys.exit(1)


main(sys.argv[1:])
