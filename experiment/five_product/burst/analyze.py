#!/usr/bin/env python3
"""突發恢復結果：由接收端每秒筆數計算恢復時間，並核對送達筆數，輸出 summary.csv 與時間軸圖。"""
import csv
import glob
import os
import re

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
KEEP = 0.4          # filter 保留 warn 以上的比例
STABLE_SECS = 5     # 連續幾秒回到基準水準才算恢復
TOLERANCE = 0.10
LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM)",
          "fluentd": "Fluentd", "vector": "Vector"}


def gen_total(path):
    m = re.search(r"\[gen\] done: total=(\d+)", open(path).read()) if os.path.exists(path) else None
    return int(m.group(1)) if m else 0


def analyse(run_dir):
    params = dict(kv.split("=") for kv in open(os.path.join(run_dir, "burst_params.txt")).read().split())
    base, lead, burst = int(params["base_rate"]), int(params["lead"]), int(params["burst_secs"])
    sink_epoch = float(open(os.path.join(run_dir, "sink_epoch.txt")).read().split()[0])
    gen_start = float(open(os.path.join(run_dir, "gen_epoch.txt")).read().split()[0])
    rows = list(csv.DictReader(open(os.path.join(run_dir, "recv_per_sec.csv"))))
    series = [(float(r["t"]), int(r["lines"])) for r in rows]

    burst_end = gen_start - sink_epoch + lead + burst
    target = KEEP * base
    recovery = None
    for i, (t, _) in enumerate(series):
        if t < burst_end:
            continue
        window = [n for _, n in series[i:i + STABLE_SECS]]
        if len(window) == STABLE_SECS and all(abs(n - target) <= TOLERANCE * target for n in window):
            recovery = t - burst_end
            break

    sent = gen_total(os.path.join(run_dir, "gen.log")) + gen_total(os.path.join(run_dir, "gen_burst.log"))
    received = sum(n for _, n in series)
    return {"base_rate": base, "burst_secs": burst, "recovery_s": round(recovery, 1) if recovery is not None else None,
            "sent": sent, "received": received, "expected": round(KEEP * sent),
            "delivered_ratio": round(received / (KEEP * sent), 4) if sent else None,
            "oom": open(os.path.join(run_dir, "oom.txt")).read().strip() if os.path.exists(os.path.join(run_dir, "oom.txt")) else "?",
            "series": series, "burst_end": burst_end, "target": target}


def main():
    results = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "*", "burst*s"))):
        if not os.path.exists(os.path.join(run_dir, "burst_params.txt")):
            continue
        r = analyse(run_dir)
        r["product"] = os.path.basename(os.path.dirname(run_dir))
        results.append(r)
        print(f"{r['product']:15} burst={r['burst_secs']:>2}s recovery={r['recovery_s']}s "
              f"received={r['received']}/{r['expected']} ({r['delivered_ratio']}) oom={r['oom']}")

    fields = ["product", "base_rate", "burst_secs", "recovery_s", "sent", "received", "expected", "delivered_ratio", "oom"]
    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(results)

    # 每種突發長度一張圖：各產品的每秒送達筆數（以各自的基準水準正規化）
    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    for burst in sorted({r["burst_secs"] for r in results}):
        fig, ax = plt.subplots(figsize=(9, 4))
        for r in (x for x in results if x["burst_secs"] == burst):
            ax.plot([t - r["burst_end"] for t, _ in r["series"]], [n / r["target"] for _, n in r["series"]],
                    label=f"{LABELS.get(r['product'], r['product'])} (recovery {r['recovery_s']} s)")
        ax.axvspan(-burst, 0, color="#eeeeee", label=f"burst ({burst} s, 3× base)")
        ax.axhline(1.0, color="#888888", linewidth=1, linestyle="--")
        ax.set_xlabel("Seconds since burst end")
        ax.set_ylabel("Delivered per second / base level")
        ax.set_title(f"Recovery after a {burst} s burst")
        ax.legend(frameon=False, fontsize=8)
        ax.spines[["top", "right"]].set_visible(False)
        fig.tight_layout()
        fig.savefig(os.path.join(RESULTS, "figures", f"burst_{burst}s.svg"))


if __name__ == "__main__":
    main()
