#!/usr/bin/env python3
"""P2：WCM 的延遲對 flush 時間窗 W。

資料來自 five_product/latency/results/data/wcm/W<W>_r<次>/（以 common/lib/fix_latency.py 校正時鐘跳動）。
統計窗口由環境變數 LAT_WINDOW 指定（預設 30,150 秒；QUICK 資料用 5,15）。
輸出 summary.csv（每個 W 的 p50、p99、超額延遲、CPU、記憶體的中位數）與 figures/p2_latency_vs_window.svg：
左圖 p99 對 W（含 p99 = 0.99·W 的理想線），右圖超額延遲；有其他產品的資料時以細線一併畫出作為參考。"""
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
DATA = os.path.join(HERE, "..", "..", "five_product", "latency", "results", "data")
sys.path.insert(0, os.path.join(HERE, "..", "..", "common", "lib"))
import fix_latency  # noqa: E402
import wcm_log  # noqa: E402

LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM, 4,000/s)",
          "fluentd": "Fluentd", "vector": "Vector"}
COLORS = {"wcm": "#2a78d6", "fluentbit-lua": "#1baf7a", "fluentbit-wasm": "#8a8984", "fluentd": "#eda100", "vector": "#e87ba4"}


def collect(product):
    points = {}
    for run_dir in sorted(glob.glob(os.path.join(DATA, product, "W*_r*"))):
        window = float(re.match(r"W([\d.]+)_r\d+", os.path.basename(run_dir)).group(1))
        verdict_path = os.path.join(run_dir, "verdict.txt")
        if not os.path.exists(verdict_path):
            continue
        verdict_text = open(verdict_path).read().strip()
        m_reasons = re.search(r"reasons=(\S+)", verdict_text)
        # 與 five_product/latency/analyze.py 相同：backlog 是吞吐量的判定條件（W ≥ 2 s 時負載結束當下本來就有
        # W 秒份在等 flush，WSL2 時鐘倒退也會高估 backlog），只因 backlog 而 FAIL 的點仍納入延遲結果。
        backlog_only = (verdict_text.startswith("FAIL") and m_reasons is not None and
                        all(x.startswith("backlog=") for x in m_reasons.group(1).split(";")))
        if not (verdict_text.startswith("PASS") or backlog_only):
            continue
        try:
            lat = fix_latency.fix(run_dir)
        except Exception as error:  # 缺資料或無法校正
            print(f"skip {run_dir}: {error}")
            continue
        if lat.get("p99") is None or lat["p99"] != lat["p99"]:   # 窗口內沒有資料（nan）
            print(f"skip {run_dir}: no latency samples in window (LAT_WINDOW={os.environ.get('LAT_WINDOW', '30,150')})")
            continue
        cpu, mem = wcm_log.cpu_mem(run_dir, 10)
        points.setdefault(window, []).append({"p50": lat["p50"], "p99": lat["p99"],
                                              "excess": lat["p99"] - 990 * window, "cpu": cpu, "mem": mem})
    return {w: {k: statistics.median(r[k] for r in rs if r[k] is not None) for k in rs[0]} | {"n": len(rs)}
            for w, rs in sorted(points.items())}


def main():
    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    data = {p: collect(p) for p in LABELS}
    if not data["wcm"]:
        print(f"no WCM latency data under {DATA}")
        return
    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["product", "window_s", "n", "p50_ms", "p99_ms", "excess_ms", "cpu_pct", "mem_max_mib"])
        for product, points in data.items():
            for w, m in points.items():
                writer.writerow([product, w, m["n"], round(m["p50"], 1), round(m["p99"], 1), round(m["excess"], 1),
                                 m["cpu"], m["mem"]])
                print(f"{product:15} W={w:<5} p99={m['p99']:8.1f} ms excess={m['excess']:7.1f} ms "
                      f"cpu={m['cpu']}% mem={m['mem']} MiB (n={m['n']})")

    fig, (left, right) = plt.subplots(1, 2, figsize=(11, 4))
    windows = sorted(set(w for points in data.values() for w in points))
    left.plot(windows, [990 * w for w in windows], ":", color="#555555", label="0.99·W (window only)")
    for product, points in data.items():
        if not points:
            continue
        ws = list(points)
        wide = product == "wcm"
        style = dict(color=COLORS[product], marker="o", linewidth=3 if wide else 1, alpha=1 if wide else 0.6,
                     label=LABELS[product])
        left.plot(ws, [points[w]["p99"] for w in ws], **style)
        right.plot(ws, [points[w]["excess"] for w in ws], **style)
    for ax in (left, right):
        ax.set_xscale("log")
        ax.set_xlabel("Flush window W (s)")
        ax.spines[["top", "right"]].set_visible(False)
    left.set_yscale("log")
    left.set_ylabel("p99 latency (ms)")
    left.set_title("p99 tracks the window")
    right.set_ylabel("Excess: p99 − 0.99·W (ms)")
    right.set_title("Processing delay beyond the window")
    right.legend(frameon=False, fontsize=8)
    fig.tight_layout()
    fig.savefig(os.path.join(RESULTS, "figures", "p2_latency_vs_window.svg"))


if __name__ == "__main__":
    main()
