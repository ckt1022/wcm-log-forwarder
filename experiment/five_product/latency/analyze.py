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
        verdict_text = open(os.path.join(run_dir, "verdict.txt")).read().strip() if os.path.exists(os.path.join(run_dir, "verdict.txt")) else "MISSING"
        verdict = verdict_text.split()[0]
        m_reasons = re.search(r"reasons=(\S+)", verdict_text)
        reasons = m_reasons.group(1) if m_reasons else ""
        # 積壓（backlog）是吞吐量的判定條件，不影響固定速率下的延遲：W ≥ 2 s 時負載結束當下本來就有 W 秒份在等 flush，
        # WSL2 wall clock 倒退也會讓 backlog 被高估。只因 backlog 而 FAIL 的點仍納入延遲結果。
        usable = verdict == "PASS" or (verdict == "FAIL" and reasons != "" and
                                       all(x.startswith("backlog=") for x in reasons.split(";")))
        try:
            result = fix_latency.fix(run_dir)
            p99 = result["p99"]
        except Exception as error:  # 缺資料或無法校正
            print(f"skip {run_dir}: {error}")
            continue
        rows.append({"product": product, "window_s": window, "rep": rep, "verdict": verdict,
                     "reasons": reasons, "usable": usable,
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
            if r["product"] == product and r["usable"]:
                points.setdefault(r["window_s"], []).append(r["excess_ms"])
        if not points:
            continue
        windows = sorted(points)
        medians = [statistics.median(points[w]) for w in windows]
        # 本系統以星形標記，其他產品用圓點
        ax.plot(range(len(windows)), medians, label=LABELS[product], color=COLORS[product],
                linewidth=3 if product == "wcm" else 2, zorder=3 if product == "wcm" else 2,
                **(dict(marker="*", markersize=14) if product == "wcm" else dict(marker="o", markersize=6)))
        ax.set_xticks(range(len(windows)), [str(w) for w in windows])
        for w, value in zip(windows, medians):
            print(f"{product:15} W={w:<5} excess p99 median={value:7.1f} ms  (n={len(points[w])})")
    ax.set_yscale("log")
    ax.set_xlabel("Flush window W (s)")
    ax.set_ylabel("Excess latency: p99 − 0.99·W (ms, log scale)")
    ax.set_title("Latency beyond the flush window (5,000 records/s, median of reps)")
    ax.legend(frameon=False)
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig(os.path.join(RESULTS, "figures", "latency_excess.svg"))
    plt.close(fig)

    # 同一份數據的線性 y 軸版本：分組長條圖，每根長條標上數值（ms）
    medians = {}
    for r in rows:
        if r["usable"]:
            medians.setdefault((r["product"], r["window_s"]), []).append(r["excess_ms"])
    windows = sorted({w for _, w in medians})
    products = [p for p in PRODUCTS if any((p, w) in medians for w in windows)]
    width = 0.8 / len(products)
    fig, ax = plt.subplots(figsize=(10, 5))
    for i, product in enumerate(products):
        xs = [k + (i - (len(products) - 1) / 2) * width for k in range(len(windows))]
        values = [statistics.median(medians[(product, w)]) if (product, w) in medians else 0 for w in windows]
        bars = ax.bar(xs, values, width * 0.9, color=COLORS[product], label=LABELS[product],
                      edgecolor="white", linewidth=0.5,
                      hatch="//" if product == "fluentbit-wasm" else None)   # 灰色與綠色相近，加斜線區分
        for x, v in zip(xs, values):
            ax.text(x, v + 3, f"{v:.0f}", ha="center", va="bottom", fontsize=7, rotation=90, color="#333333")
    ax.set_xticks(range(len(windows)), [str(w) for w in windows])
    ax.set_xlabel("Flush window W (s)")
    ax.set_ylabel("Excess latency: p99 − 0.99·W (ms)")
    ax.set_title("Latency beyond the flush window (5,000 records/s, median of reps, linear scale)")
    ax.set_ylim(0, ax.get_ylim()[1] * 1.08)
    ax.yaxis.grid(True, color="#e5e5e5", linewidth=0.8)
    ax.set_axisbelow(True)
    ax.legend(frameon=False, fontsize=8)
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig(os.path.join(RESULTS, "figures", "latency_excess_linear.svg"))


if __name__ == "__main__":
    main()
