#!/usr/bin/env python3
"""P4：二分隔離開／關時，正常資料的送達率、dead-letter 筆數、隔離呼叫次數與 CPU。

expected = 負載產生器送出的合法、不帶標記、level 為 warn 的行數（filter 只保留 warn）。
bad = 壞資料筆數（截斷的 JSON 或帶標記的行）。
collateral = 被連帶寫入 dead-letter 的正常資料 = dead-letter − bad（二分開啟時應為 0）。
輸出 summary.csv、figures/p4_delivered.svg、p4_dead_letter.svg。"""
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


def analyse(run_dir):
    params = wcm_log.kv(open(os.path.join(run_dir, "p4_params.txt")).read())
    gen = wcm_log.load_json(run_dir)
    received = wcm_log.received(run_dir)
    expected = gen["expected_normal_delivered"]
    bad = gen["malformed"] + gen["marked"] + (1 if gen["fault_seq"] >= 0 else 0)
    summary = wcm_log.stage_summary(run_dir)
    dead = summary.get("dead_letter")
    calls = int(wcm_log.total(summary, "isolation_calls"))
    cpu, mem = wcm_log.cpu_mem(run_dir, 5, int(params["secs"]) - 2)
    state = wcm_log.state(run_dir)
    return {"case": params["case"], "bisection": params["bisection"], "sent": gen["sent"], "bad": bad,
            "expected": expected, "received": received,
            "delivered_pct": round(100 * received / expected, 3) if expected else "",
            "lost_normal": expected - received, "dead_letter": "" if dead is None else dead,
            "collateral": "" if dead is None else dead - bad,
            "isolation_calls": calls, "calls_per_bad": round(calls / bad, 1) if bad else "",
            "isolated_batches": int(wcm_log.total(summary, "isolated_batches")),
            "cpu_pct": cpu, "mem_max_mib": mem, "alive": state.get("status") == "running"}


def main():
    rows = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "*_r*"))):
        try:
            row = analyse(run_dir)
        except (OSError, KeyError, ValueError, TypeError) as error:
            print(f"skip {run_dir}: {error}")
            continue
        row["rep"] = int(re.search(r"_r(\d+)$", run_dir).group(1))
        rows.append(row)
    if not rows:
        print("no data")
        return
    fields = ["case", "rep", "bisection", "sent", "bad", "expected", "received", "delivered_pct", "lost_normal",
              "dead_letter", "collateral", "isolation_calls", "calls_per_bad", "isolated_batches", "cpu_pct",
              "mem_max_mib", "alive"]
    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)
    for r in rows:
        print(f"{r['case']:18} r{r['rep']} bad={r['bad']:<5} delivered={r['delivered_pct']}% "
              f"dead_letter={r['dead_letter']} collateral={r['collateral']} calls/bad={r['calls_per_bad']} cpu={r['cpu_pct']}%")

    # A：malformed 比例 × 二分開／關
    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    groups = {}
    for r in rows:
        m = re.match(r"malformed_([\d.]+)_(on|off)", r["case"])
        if m:
            groups.setdefault(m.group(2), {}).setdefault(float(m.group(1)), []).append(r)
    if groups:
        for metric, ylabel, name, log in [("delivered_pct", "Normal records delivered (%)", "p4_delivered.svg", False),
                                          ("dead_letter", "Dead-letter records", "p4_dead_letter.svg", True)]:
            fig, ax = plt.subplots(figsize=(6, 4))
            for mode, color in [("on", "#2a78d6"), ("off", "#eda100")]:
                points = groups.get(mode, {})
                xs = sorted(points)
                ys = [statistics.median(r[metric] for r in points[x] if r[metric] != "") for x in xs]
                ax.plot([str(x) for x in xs], ys, marker="o", color=color, linewidth=2, label=f"bisection {mode}")
            if metric == "dead_letter":
                bad = {x: statistics.median(r["bad"] for r in pts) for pts in [groups.get("on", {})] for x, pts in pts.items()}
                xs = sorted(bad)
                ax.plot([str(x) for x in xs], [bad[x] for x in xs], ":", color="#555555", label="malformed records")
            if log:
                ax.set_yscale("symlog")
            ax.set_xlabel("Malformed lines (%)")
            ax.set_ylabel(ylabel)
            ax.set_title("Parse rejects the batch: bisection on vs off")
            ax.legend(frameon=False)
            ax.spines[["top", "right"]].set_visible(False)
            fig.tight_layout()
            fig.savefig(os.path.join(RESULTS, "figures", name))


if __name__ == "__main__":
    main()
