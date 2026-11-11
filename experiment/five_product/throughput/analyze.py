#!/usr/bin/env python3
"""吞吐量結果：讀取 results/<產品>_search.txt 與通過速率的資源取樣，輸出 summary.csv 與圖。"""
import csv
import os
import re
import statistics

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
PRODUCTS = ["wcm", "fluentbit-lua", "fluentbit-wasm", "fluentd", "vector"]
LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM)",
          "fluentd": "Fluentd", "vector": "Vector"}


def resource_usage(run_dir):
    """負載期間（排除前後 5 秒）的 CPU 與記憶體中位數。"""
    path = os.path.join(run_dir, "stats.csv")
    if not os.path.exists(path):
        return None, None
    rows = list(csv.DictReader(open(path)))[5:-5]
    if not rows:
        return None, None
    return (statistics.median(float(r["cpu_pct"]) for r in rows),
            statistics.median(float(r["rss_mb"]) for r in rows))


def main():
    summary = []
    for product in PRODUCTS:
        path = os.path.join(RESULTS, f"{product}_search.txt")
        if not os.path.exists(path):
            continue
        text = open(path).read()
        final = re.search(r"capacity=(\d+) first_fail=(\d+) trials=(\d+)", text)
        if not final:
            continue
        capacity, first_fail, trials = map(int, final.groups())
        cpu, mem = resource_usage(os.path.join(RESULTS, "data", product, f"rate_{capacity}")) if capacity else (None, None)
        summary.append({"product": product, "capacity_eps": capacity, "first_fail_eps": first_fail,
                        "trials": trials, "cpu_pct_at_capacity": cpu, "mem_mb_at_capacity": mem})

    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(summary[0].keys()) if summary else ["product"])
        writer.writeheader()
        writer.writerows(summary)

    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    rows = sorted(summary, key=lambda r: r["capacity_eps"], reverse=True)
    fig, ax = plt.subplots(figsize=(8, 4))
    bars = ax.barh([LABELS[r["product"]] for r in rows], [r["capacity_eps"] for r in rows],
                   color=["#2a78d6" if r["product"] == "wcm" else "#c9c8c2" for r in rows])
    ax.bar_label(bars, labels=[f"{r['capacity_eps']:,}" for r in rows], padding=4)
    ax.invert_yaxis()
    ax.set_xlabel("Max sustainable throughput (records/s)")
    ax.set_title("Throughput (1 CPU, 512 MiB, 1 KiB JSON, flush window 0.1 s)")
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig(os.path.join(RESULTS, "figures", "throughput.svg"))
    for r in rows:
        print(f"{r['product']:15} capacity={r['capacity_eps']:>7,}  cpu={r['cpu_pct_at_capacity']}  mem={r['mem_mb_at_capacity']}")


if __name__ == "__main__":
    main()
