#!/usr/bin/env python3
"""可靠性結果：依序號核對每個 run 的遺失、重複、亂序，並整理各實驗的指標與圖。

應送達 = 已送出範圍內的偶數 seq（warn）；收到奇數 seq 表示過濾失效（unexpected）。
WCM 重試用完的資料會寫入 dead-letter（error.txt），另外計算，不算遺失也不算送達。

用法：analyze.py [outage|shutdown|soak ...]（預設全部有資料的實驗）
輸出：results/<實驗>/summary.csv、results/<實驗>/figures/
"""
import csv
import glob
import json
import os
import re
import statistics
import sys

import numpy as np

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
PRODUCTS = ["wcm", "fluentbit-lua", "fluentbit-wasm", "fluentd", "vector"]
LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM, 4,000/s)",
          "fluentd": "Fluentd", "vector": "Vector"}
COLORS = {"wcm": "#2a78d6", "fluentbit-lua": "#1baf7a", "fluentbit-wasm": "#8a8984", "fluentd": "#eda100", "vector": "#e87ba4"}


def style(product):
    wcm = product == "wcm"
    return dict(color=COLORS[product], linewidth=2.5 if wcm else 1.5, zorder=3 if wcm else 2,
                label=LABELS[product], **(dict(marker="*", markersize=9, markevery=0.08) if wcm else {}))


def read_seqs(run_dir):
    """依接收順序的 seq；中斷實驗有兩個接收端（sink2 在後）。"""
    parts = [np.fromfile(p, dtype="<u4") for p in (f"{run_dir}/seq.bin", f"{run_dir}/sink2/seq.bin") if os.path.exists(p)]
    return np.concatenate(parts) if parts else np.zeros(0, dtype="<u4")


def dead_letter_seqs(run_dir):
    path = f"{run_dir}/error.txt"
    if not os.path.exists(path):
        return 0
    return len(re.findall(rb'"seq":\s*"?\d+', open(path, "rb").read()))


def check(run_dir):
    summary = json.load(open(f"{run_dir}/generator_summary.json"))
    sent = summary["sent"]
    seqs = read_seqs(run_dir)
    uniq = np.unique(seqs)
    even = uniq[(uniq % 2 == 0) & (uniq < sent)]
    odd = uniq[uniq % 2 == 1]
    running_max = np.maximum.accumulate(seqs) if len(seqs) else seqs
    out_of_order = int((seqs[1:] < running_max[:-1]).sum()) if len(seqs) > 1 else 0
    state = dict(kv.split("=", 1) for kv in open(f"{run_dir}/state.txt").read().split()) \
        if os.path.exists(f"{run_dir}/state.txt") else {}
    stats = list(csv.DictReader(open(f"{run_dir}/stats.csv"))) if os.path.exists(f"{run_dir}/stats.csv") else []
    return {"sent": sent, "expected": summary["expected"], "received": int(len(seqs)),
            "lost": summary["expected"] - int(len(even)), "duplicates": int(len(seqs) - len(uniq)),
            "unexpected": int(len(odd)), "out_of_order": out_of_order, "dead_letter": dead_letter_seqs(run_dir),
            "gen_elapsed_s": summary["elapsed_sec"], "gen_max_lag_s": summary["max_send_lag_sec"],
            "terminated": summary["terminated"],
            "rss_peak_mb": max((float(r["rss_mb"]) for r in stats), default=float("nan")),
            "oom": state.get("oom", "?"), "status": state.get("status", "?"), "exit_code": state.get("exit_code", "?"),
            "_stats": stats, "_summary": summary}


def runs(exp):
    for product in PRODUCTS:
        for run_dir in sorted(glob.glob(f"{HERE}/results/{exp}/data/{product}/*")):
            if os.path.exists(f"{run_dir}/generator_summary.json"):
                yield product, os.path.basename(run_dir), run_dir


def write_summary(exp, rows, fields):
    path = f"{HERE}/results/{exp}/summary.csv"
    with open(path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields, extrasaction="ignore")
        w.writeheader()
        w.writerows(rows)
    for r in rows:
        print("  ".join(f"{k}={r[k]}" for k in fields))


BASE_FIELDS = ["product", "run", "sent", "expected", "received", "lost", "duplicates", "unexpected",
               "out_of_order", "dead_letter", "rss_peak_mb", "oom", "exit_code"]


def outage():
    rows, fig, ax = [], *plt.subplots(figsize=(9, 4.5))
    for product, name, run_dir in runs("outage"):
        r = check(run_dir)
        t_sink1 = float(open(f"{run_dir}/sink_epoch.txt").read().split()[0])
        ev = {k: float(v) for k, v in (ln.split() for ln in open(f"{run_dir}/outage.txt"))}
        gen_start = float(open(f"{run_dir}/gen_epoch.txt").read().split()[0])
        steady = r["_summary"]["eps_target"] / 2
        # 每秒送達：兩個接收端接在一起，時間軸為「距接收端重新啟動的秒數」
        series = []
        for path, epoch in ((f"{run_dir}/recv_per_sec.csv", t_sink1),
                            (f"{run_dir}/sink2/recv_per_sec.csv", float(open(f"{run_dir}/sink2/sink_epoch.txt").read()))):
            if os.path.exists(path):
                series += [(epoch + float(x["t"]) - ev["restart"], int(x["lines"])) for x in csv.DictReader(open(path))]
        after = [(t, n) for t, n in series if t >= 0]
        recovery = None
        for i in range(len(after) - 4):
            if all(abs(n - steady) <= 0.1 * steady for _, n in after[i:i + 5]):
                recovery = round(after[i][0], 1)
                break
        # 中斷期間（停止到重新啟動 + 恢復）的記憶體峰值
        rss_outage = max((float(x["rss_mb"]) for x in r["_stats"]), default=float("nan"))
        r.update(product=product, run=name, recovery_s=recovery, outage_s=round(ev["restart"] - ev["stopped"], 1),
                 gen_blocked_s=round(r["gen_elapsed_s"] - r["_summary"]["planned_sec"], 1), rss_peak_mb=rss_outage)
        rows.append(r)
        if name == "r1":
            ax.plot([t for t, _ in series], [n / steady for _, n in series], **style(product))
    ax.axvspan(-30, 0, color="#dddddd", label="receiver down (30 s)")
    ax.set_xlim(-50, 120)
    ax.set_xlabel("Seconds since receiver restarted")
    ax.set_ylabel("Delivered per second ÷ steady rate")
    ax.set_title("Downstream outage: delivery before, during and after (r1)")
    ax.legend(frameon=False, fontsize=8)
    ax.spines[["top", "right"]].set_visible(False)
    save(fig, "outage", "outage.svg")
    write_summary("outage", rows, BASE_FIELDS + ["outage_s", "recovery_s", "gen_blocked_s"])


def shutdown():
    rows = []
    for product, name, run_dir in runs("shutdown"):
        r = check(run_dir)
        sd = open(f"{run_dir}/shutdown.txt").read().split() if os.path.exists(f"{run_dir}/shutdown.txt") else []
        r.update(product=product, run=name, window_s=float(name.split("_")[0][1:]),
                 stop_s=round(float(sd[3]) - float(sd[1]), 2) if len(sd) == 4 else None)
        rows.append(r)
    write_summary("shutdown", rows, BASE_FIELDS + ["window_s", "stop_s"])


def soak():
    rows = []
    fig, axes = plt.subplots(3, 1, figsize=(9, 9), sharex=True)
    for product, name, run_dir in runs("soak"):
        r = check(run_dir)
        st = r["_stats"]
        t = np.array([float(x["elapsed_s"]) for x in st]) / 60
        rss = np.array([float(x["rss_mb"]) for x in st])
        cpu = np.array([float(x["cpu_pct"]) for x in st])
        # 記憶體：前 10 分鐘（暖機）之後的線性斜率，以及最後 5 分鐘 − 第 10–15 分鐘
        m = t >= 10
        slope = float(np.polyfit(t[m], rss[m], 1)[0] * 60) if m.sum() > 10 else float("nan")
        early = float(np.median(rss[(t >= 10) & (t < 15)])) if ((t >= 10) & (t < 15)).any() else float("nan")
        late = float(np.median(rss[t >= t.max() - 5]))
        # 延遲：每 5 分鐘的 p50／p99（時鐘已校正；clock_drift 的顯著階梯數一併記錄）
        lat_rows = []
        lat_path = f"{run_dir}/latency_worker_1.csv"
        if os.path.exists(lat_path):
            import pandas as pd
            d = pd.read_csv(lat_path, usecols=["receive_unix", "latency_ms"])
            minutes = (d["receive_unix"] - d["receive_unix"].min()) / 60
            for k in range(0, 60, 5):
                v = d["latency_ms"][(minutes >= k) & (minutes < k + 5)].to_numpy()
                if len(v):
                    lat_rows.append((k + 2.5, float(np.percentile(v, 50)), float(np.percentile(v, 99))))
            whole = d["latency_ms"][minutes >= 1].to_numpy()
            r["p99_ms"] = round(float(np.percentile(whole, 99)), 1)
            r["p99_spread_ms"] = round(max(x[2] for x in lat_rows) - min(x[2] for x in lat_rows), 1)
        drift = [float(x["drift"]) for x in csv.DictReader(open(f"{run_dir}/clock_drift.csv"))]
        r["clock_steps"] = sum(1 for a, b in zip(drift, drift[1:]) if abs(b - a) > 0.05)
        cpu_bins = [float(np.mean(cpu[(t >= k) & (t < k + 5)])) for k in range(0, 60, 5) if ((t >= k) & (t < k + 5)).any()]
        r.update(product=product, run=name, rss_10_15min_mb=round(early, 1), rss_last5min_mb=round(late, 1),
                 rss_slope_mb_per_h=round(slope, 2), cpu_mean=round(float(cpu[t >= 1].mean()), 1),
                 cpu_5min_spread=round(max(cpu_bins) - min(cpu_bins), 1) if cpu_bins else None)
        rows.append(r)
        axes[0].plot(t, rss, **style(product))
        axes[1].plot(t, np.convolve(cpu, np.ones(30) / 30, mode="same"), **style(product))
        if lat_rows:
            axes[2].plot([x[0] for x in lat_rows], [x[2] for x in lat_rows], marker="o" if product != "wcm" else "*",
                         **{k: v for k, v in style(product).items() if k not in ("marker", "markersize", "markevery")})
    axes[0].set_ylabel("RSS (MiB)")
    axes[1].set_ylabel("CPU % (30 s mean)")
    axes[2].set_ylabel("p99 latency per 5 min (ms)")
    axes[2].set_yscale("log")
    axes[2].set_xlabel("Minutes since load start")
    axes[0].set_title("1-hour soak at 5,000 records/s: memory, CPU and latency stay flat?")
    axes[0].legend(frameon=False, fontsize=8, ncol=2)
    for a in axes:
        a.spines[["top", "right"]].set_visible(False)
    save(fig, "soak", "soak.svg")
    write_summary("soak", rows, BASE_FIELDS + ["rss_10_15min_mb", "rss_last5min_mb", "rss_slope_mb_per_h",
                                               "cpu_mean", "cpu_5min_spread", "p99_ms", "p99_spread_ms", "clock_steps"])


def save(fig, exp, name):
    os.makedirs(f"{HERE}/results/{exp}/figures", exist_ok=True)
    fig.tight_layout()
    fig.savefig(f"{HERE}/results/{exp}/figures/{name}")
    plt.close(fig)


if __name__ == "__main__":
    todo = sys.argv[1:] or [e for e in ("outage", "shutdown", "soak") if os.path.isdir(f"{HERE}/results/{e}/data")]
    for exp in todo:
        print(f"== {exp}")
        {"outage": outage, "shutdown": shutdown, "soak": soak}[exp]()
