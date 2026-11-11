#!/usr/bin/env python3
"""延遲結果：每個量測點以 common/lib/fix_latency.py 計算 p99（校正 WSL2 時鐘跳動），
輸出 summary.csv（每點一列）與圖（各產品 W 對超額延遲的中位數）。"""
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
import fix_latency  # noqa: E402

PRODUCTS = ["wcm", "fluentbit-lua", "fluentbit-wasm", "fluentd", "vector"]
LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM, 4,000/s)",
          "fluentd": "Fluentd", "vector": "Vector"}
COLORS = {"wcm": "#2a78d6", "fluentbit-lua": "#1baf7a", "fluentbit-wasm": "#8a8984", "fluentd": "#eda100", "vector": "#e87ba4"}


def main():
    rows = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "*", "W*_r*"))):
        product = os.path.basename(os.path.dirname(run_dir))
        m = re.match(r"W([\d.]+)_r(\d+)", os.path.basename(run_dir))
        window, rep = float(m.group(1)), int(m.group(2))
        verdict = open(os.path.join(run_dir, "verdict.txt")).read().split()[0] if os.path.exists(os.path.join(run_dir, "verdict.txt")) else "MISSING"
        try:
            result = fix_latency.fix(run_dir)
            p99 = result["p99"]
        except Exception as error:  # 缺資料或無法校正
            print(f"skip {run_dir}: {error}")
            continue
        rows.append({"product": product, "window_s": window, "rep": rep, "verdict": verdict,
                     "p50_ms": result.get("p50"), "p99_ms": p99, "excess_ms": p99 - 990 * window,
                     "clock_steps": result.get("n_steps"), "status": result.get("status")})

    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0].keys()) if rows else ["product"])
        writer.writeheader()
        writer.writerows(rows)

    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    fig, ax = plt.subplots(figsize=(8, 4.5))
    for product in PRODUCTS:
        points = {}
        for r in rows:
            if r["product"] == product and r["verdict"] == "PASS":
                points.setdefault(r["window_s"], []).append(r["excess_ms"])
        if not points:
            continue
        windows = sorted(points)
        medians = [statistics.median(points[w]) for w in windows]
        ax.plot(range(len(windows)), medians, marker="o", label=LABELS[product], color=COLORS[product],
                linewidth=3 if product == "wcm" else 2)
        ax.set_xticks(range(len(windows)), [str(w) for w in windows])
        for w, value in zip(windows, medians):
            print(f"{product:15} W={w:<5} excess p99 median={value:7.1f} ms  (n={len(points[w])})")
    ax.set_xlabel("Flush window W (s)")
    ax.set_ylabel("Excess latency: p99 − 0.99·W (ms)")
    ax.set_title("Latency beyond the flush window (5,000 records/s, median of reps)")
    ax.legend(frameon=False)
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig(os.path.join(RESULTS, "figures", "latency_excess.svg"))


if __name__ == "__main__":
    main()
