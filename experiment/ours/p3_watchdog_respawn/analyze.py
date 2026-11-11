#!/usr/bin/env python3
"""P3：故障後的停頓與恢復，以及時限（epoch interruption）的 CPU 成本。

fault.csv：每個情境 × 次數的 outcome、遺失筆數、最長零輸出秒數、恢復時間、parse 的隔離呼叫次數與 dead-letter 筆數。
  outcome：ok（最多損失故障的那 1 筆且恢復）、lost data、stalled（程序還在但不再送出）、exited。
  恢復時間 = 注入時刻到每秒送達回到注入前中位數 50% 以上的第一秒。
overhead.csv：watchdog 開／關的 CPU 中位數（負載第 10 秒到結束前 5 秒）與比值。
圖：figures/p3_timeline.svg（各情境每秒送達）、p3_recovery_vs_timeout.svg、p3_overhead.svg。"""
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

ORDER = ["none", "loop_T1", "loop_T2", "loop_T5", "loop_off", "panic", "oob", "mem"]


def expected_deliveries(gen):
    """fault_gen 的 warn 與 info 交替（偶數序號為 warn），帶標記的那筆一律為 warn。"""
    warn = (gen["sent"] + 1) // 2
    if gen.get("fault_seq", -1) >= 0 and gen["fault_seq"] % 2 == 1:
        warn += 1
    return warn


def fault_row(run_dir):
    params = wcm_log.kv(open(os.path.join(run_dir, "p3_params.txt")).read())
    gen = wcm_log.load_json(run_dir)
    series = wcm_log.recv_series(run_dir)
    received = sum(n for _, n in series)
    expected = expected_deliveries(gen)
    fault_at, load_end = gen["fault_at_sec"], gen["elapsed_sec"]
    stall, recovery = wcm_log.stall_and_recovery(series, fault_at, load_end) if gen["marker"] else (0, 0.0)
    state = wcm_log.state(run_dir)
    alive = state.get("status") == "running"
    lost = expected - received
    if not alive:
        outcome = "exited"
    elif gen.get("terminated") or (gen["marker"] and recovery is None):
        outcome = "stalled"
    elif lost <= 1:
        outcome = "ok"
    else:
        outcome = "lost data"
    summary = wcm_log.stage_summary(run_dir)
    parse = summary.get("parse", {})
    cpu, _ = wcm_log.cpu_mem(run_dir, fault_at + 2, load_end) if gen["marker"] else (None, None)
    return {"case": params["case"], "marker": params["marker"] or "-", "timeout_ms": int(params["timeout_ms"]),
            "watchdog": params["watchdog"], "outcome": outcome, "sent": gen["sent"], "expected": expected,
            "received": received, "lost": lost, "longest_stall_s": stall,
            "recovery_s": "" if recovery is None else round(recovery, 1),
            "isolation_calls": int(parse.get("isolation_calls", 0)), "dead_letter": summary.get("dead_letter", ""),
            "cpu_after_fault_pct": cpu, "series": series, "fault_at": fault_at}


def main():
    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    faults = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "fault", "*_r*"))):
        try:
            row = fault_row(run_dir)
        except (OSError, KeyError, ValueError, TypeError) as error:
            print(f"skip {run_dir}: {error}")
            continue
        row["rep"] = int(re.search(r"_r(\d+)$", run_dir).group(1))
        faults.append(row)
    faults.sort(key=lambda r: (ORDER.index(r["case"]) if r["case"] in ORDER else 99, r["rep"]))
    fields = ["case", "rep", "marker", "timeout_ms", "watchdog", "outcome", "sent", "expected", "received", "lost",
              "longest_stall_s", "recovery_s", "isolation_calls", "dead_letter", "cpu_after_fault_pct"]
    with open(os.path.join(RESULTS, "fault.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(faults)
    for r in faults:
        print(f"{r['case']:9} r{r['rep']} {r['outcome']:10} lost={r['lost']:<6} stall={r['longest_stall_s']}s "
              f"recovery={r['recovery_s']}s isolation_calls={r['isolation_calls']} dead_letter={r['dead_letter']}")

    if faults:
        # 每個情境取第 1 次的時間軸
        fig, ax = plt.subplots(figsize=(9, 4))
        shown = set()
        for r in faults:
            if r["case"] in shown or r["case"] == "none":
                continue
            shown.add(r["case"])
            ax.plot([t for t, _ in r["series"]], [n for _, n in r["series"]], label=f"{r['case']}: {r['outcome']}",
                    linewidth=2 if r["case"].startswith("loop") else 1)
        ax.axvline(faults[0]["fault_at"], color="#b03a2e", linestyle="--", linewidth=1, label="fault injected")
        ax.set_xlabel("Seconds since load start")
        ax.set_ylabel("Delivered per second")
        ax.set_title("Delivery after a faulty record (parse stage)")
        ax.legend(frameon=False, fontsize=8)
        ax.spines[["top", "right"]].set_visible(False)
        fig.tight_layout()
        fig.savefig(os.path.join(RESULTS, "figures", "p3_timeline.svg"))

        loops = {}
        for r in faults:
            if r["case"].startswith("loop_T") and r["recovery_s"] != "":
                loops.setdefault(r["timeout_ms"] / 1000, []).append(r["recovery_s"])
        if loops:
            fig, ax = plt.subplots(figsize=(5, 4))
            ts = sorted(loops)
            ax.plot(ts, [statistics.median(loops[t]) for t in ts], marker="o", color="#2a78d6", label="measured")
            ax.plot(ts, ts, ":", color="#555555", label="recovery = T")
            ax.set_xlabel("Call deadline T (s)")
            ax.set_ylabel("Recovery time (s)")
            ax.set_title("FAULT_LOOP: recovery vs deadline")
            ax.legend(frameon=False)
            ax.spines[["top", "right"]].set_visible(False)
            fig.tight_layout()
            fig.savefig(os.path.join(RESULTS, "figures", "p3_recovery_vs_timeout.svg"))

    overhead = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "overhead", "watchdog_*_r*"))):
        try:
            params = wcm_log.kv(open(os.path.join(run_dir, "p3_params.txt")).read())
            cpu, mem = wcm_log.cpu_mem(run_dir, 10, int(params["load_secs"]) - 5)
            verdict = open(os.path.join(run_dir, "verdict.txt")).read().split()[0]
        except (OSError, KeyError, IndexError) as error:
            print(f"skip {run_dir}: {error}")
            continue
        overhead.append({"watchdog": params["watchdog"], "rep": int(re.search(r"_r(\d+)$", run_dir).group(1)),
                         "rate": int(params["rate"]), "verdict": verdict, "cpu_pct": cpu, "mem_max_mib": mem})
    with open(os.path.join(RESULTS, "overhead.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=["watchdog", "rep", "rate", "verdict", "cpu_pct", "mem_max_mib"])
        writer.writeheader()
        writer.writerows(sorted(overhead, key=lambda r: (r["watchdog"], r["rep"])))
    cpu = {w: [r["cpu_pct"] for r in overhead if r["watchdog"] == w and r["cpu_pct"] is not None] for w in ("true", "false")}
    if cpu["true"] and cpu["false"]:
        on, off = statistics.median(cpu["true"]), statistics.median(cpu["false"])
        print(f"overhead: CPU watchdog on={on:.1f}% off={off:.1f}% ratio={on / off:.3f} "
              f"(n={len(cpu['true'])}/{len(cpu['false'])})")
        fig, ax = plt.subplots(figsize=(4.5, 4))
        ax.bar(["on", "off"], [on, off], color=["#2a78d6", "#8a8984"], width=0.5)
        for i, values in enumerate([cpu["true"], cpu["false"]]):
            ax.scatter([i] * len(values), values, color="#222222", s=12, zorder=3)
        ax.set_xlabel("Watchdog (epoch interruption)")
        ax.set_ylabel("CPU % (median, 1 CPU limit)")
        ax.set_title(f"Cost of call deadlines @ {overhead[0]['rate']:,}/s")
        ax.spines[["top", "right"]].set_visible(False)
        fig.tight_layout()
        fig.savefig(os.path.join(RESULTS, "figures", "p3_overhead.svg"))


if __name__ == "__main__":
    main()
