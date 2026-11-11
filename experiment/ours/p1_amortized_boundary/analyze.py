#!/usr/bin/env python3
"""P1：飽和處理量對批次上限。

處理量（筆／秒）= 每秒送達筆數的中位數（負載第 3 秒到結束前 1 秒）÷ filter 保留比例
（保留比例取自 WCM 關閉時印出的 filter 輸入／輸出筆數）。
輸出 summary.csv、figures/p1_count_cap.svg、p1_byte_cap.svg、p1_batch_bytes.svg（橫軸統一為批次位元組，縱軸為各日誌大小最高值的比例）。"""
import csv
import glob
import os
import re
import statistics
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
sys.path.insert(0, os.path.join(HERE, "..", "..", "common", "lib"))
import wcm_log  # noqa: E402

MODES = ["json-fields256", "json-fields1k", "json-fields2k"]
COLORS = {"json-fields256": "#2a78d6", "json-fields1k": "#1baf7a", "json-fields2k": "#eda100"}


def analyse(run_dir):
    params = wcm_log.kv(open(os.path.join(run_dir, "p1_params.txt")).read())
    load = float(params["load_secs"])
    series = wcm_log.recv_series(run_dir)
    steady = [n for t, n in series if 3 <= t <= load - 1]
    summary = wcm_log.stage_summary(run_dir)
    flt, parse = summary.get("filter", {}), summary.get("parse", {})
    keep = flt.get("output", 0) / flt["input"] if flt.get("input") else 0.4
    record_bytes = parse["bytes"] / parse["input"] if parse.get("input") else None
    delivered = statistics.median(steady) if steady else 0
    cpu, mem = wcm_log.cpu_mem(run_dir, 3, load - 1)
    return {"arm": params["arm"], "mode": params["mode"], "value": int(params["value"]),
            "k": int(params["k"]), "bytes_kib": int(params["bytes_kib"]),
            "delivered_per_s": delivered, "processed_per_s": round(delivered / keep) if keep else 0,
            "keep_ratio": round(keep, 4), "record_bytes": round(record_bytes) if record_bytes else "",
            "parse_batches": int(parse.get("batches", 0)), "parse_records": int(parse.get("input", 0)),
            "cpu_pct": cpu, "mem_max_mib": mem, "alive": wcm_log.state(run_dir).get("status") == "running"}


def median_by(rows, arm, mode):
    points = {}
    for r in rows:
        if r["arm"] == arm and r["mode"] == mode:
            points.setdefault(r["value"], []).append(r)
    return {v: (statistics.median(x["processed_per_s"] for x in rs),
                statistics.median(x["parse_records"] / x["parse_batches"] for x in rs if x["parse_batches"]) if any(x["parse_batches"] for x in rs) else 0,
                statistics.median(x["record_bytes"] for x in rs if x["record_bytes"]) if any(x["record_bytes"] for x in rs) else 0)
            for v, rs in sorted(points.items())}


def main():
    rows = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "*", "*_r*"))):
        if not os.path.exists(os.path.join(run_dir, "p1_params.txt")):
            continue
        try:
            row = analyse(run_dir)
        except (OSError, KeyError, ValueError) as error:
            print(f"skip {run_dir}: {error}")
            continue
        row["rep"] = int(re.search(r"_r(\d+)$", run_dir).group(1))
        rows.append(row)
    if not rows:
        print("no data")
        return
    fields = ["arm", "mode", "value", "rep", "k", "bytes_kib", "processed_per_s", "delivered_per_s", "keep_ratio",
              "record_bytes", "parse_batches", "parse_records", "cpu_pct", "mem_max_mib", "alive"]
    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields)
        writer.writeheader()
        writer.writerows(sorted(rows, key=lambda r: (r["arm"], r["mode"], r["value"], r["rep"])))

    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    best = {}
    for arm, xlabel, name in [("count", "Record cap k (records per call)", "p1_count_cap.svg"),
                              ("bytes", "Byte cap (KiB per call)", "p1_byte_cap.svg")]:
        fig, ax = plt.subplots(figsize=(7, 4))
        for mode in MODES:
            med = median_by(rows, arm, mode)
            if not med:
                continue
            xs = list(med)
            ax.plot(xs, [med[x][0] for x in xs], marker="o", label=mode, color=COLORS[mode], linewidth=2)
            for x in xs:
                print(f"{arm:5} {mode:15} {x:>6}  processed/s={med[x][0]:>9,.0f}  records/call={med[x][1]:>7.1f}")
                best[mode] = max(best.get(mode, 0), med[x][0])
        ax.set_xscale("log", base=2)
        ax.set_xlabel(xlabel)
        ax.set_ylabel("Processed records/s (saturated)")
        ax.set_title(f"WCM throughput vs {arm} cap (1 CPU)")
        ax.legend(frameon=False)
        ax.spines[["top", "right"]].set_visible(False)
        fig.tight_layout()
        fig.savefig(os.path.join(RESULTS, "figures", name))

    # 橫軸統一為每次呼叫的位元組數：兩種上限、三種大小是否落在同一條曲線
    fig, ax = plt.subplots(figsize=(7, 4))
    for mode in MODES:
        for arm, style in [("count", "-"), ("bytes", "--")]:
            med = median_by(rows, arm, mode)
            if not med or not best.get(mode):
                continue
            xs = [med[v][1] * med[v][2] / 1024 for v in med if med[v][1] and med[v][2]]
            ys = [med[v][0] / best[mode] for v in med if med[v][1] and med[v][2]]
            ax.plot(xs, ys, style, marker="o", color=COLORS[mode], label=f"{mode}, {arm} cap")
    ax.set_xscale("log", base=2)
    ax.set_xlabel("Measured KiB per plugin call")
    ax.set_ylabel("Throughput / best for that log size")
    ax.set_title("Amortization depends on bytes per call, not records")
    ax.legend(frameon=False, fontsize=8)
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig(os.path.join(RESULTS, "figures", "p1_batch_bytes.svg"))


if __name__ == "__main__":
    main()
