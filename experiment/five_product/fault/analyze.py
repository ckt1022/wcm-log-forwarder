#!/usr/bin/env python3
"""自訂邏輯出錯的結果：核對遺失筆數、故障後停止輸出的時間與產品狀態，輸出 summary.csv 與時間軸圖。

outcome：ok（最多損失故障的那 1 筆）、lost data（遺失更多）、extra records（收到的比送出的多，可能重複或未被過濾）、
stalled（程序還在但不再送出）、exited（容器停止或 OOM）。"""
import csv
import glob
import json
import os

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
PRODUCTS = ["wcm", "fluentbit-lua", "fluentbit-wasm", "fluentd", "vector"]
LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM)",
          "fluentd": "Fluentd", "vector": "Vector"}
FAULTS = ["none", "FAULT_LOOP", "FAULT_PANIC", "FAULT_OOB", "FAULT_MEM"]


def expected_deliveries(summary):
    """fault_gen 的 warn 與 info 交替（偶數序號為 warn），帶標記的那筆一律為 warn。"""
    sent = summary["sent"]
    warn = (sent + 1) // 2
    if summary.get("fault_seq", -1) >= 0 and summary["fault_seq"] % 2 == 1:
        warn += 1
    return warn


def analyse(run_dir):
    summary = json.load(open(os.path.join(run_dir, "generator_summary.json")))
    state = dict(kv.split("=", 1) for kv in open(os.path.join(run_dir, "state.txt")).read().split()) \
        if os.path.exists(os.path.join(run_dir, "state.txt")) else {}
    sink_epoch = float(open(os.path.join(run_dir, "sink_epoch.txt")).read().split()[0])
    gen_start = float(open(os.path.join(run_dir, "gen_epoch.txt")).read().split()[0])
    series = [(float(r["t"]) - (gen_start - sink_epoch), int(r["lines"]))
              for r in csv.DictReader(open(os.path.join(run_dir, "recv_per_sec.csv")))]
    received = sum(n for _, n in series)
    expected = expected_deliveries(summary)

    # 故障後最長的連續零輸出秒數，以及負載結束前是否恢復輸出
    fault_at, load_end = summary["fault_at_sec"], summary["elapsed_sec"]
    longest = current = 0
    for t, n in series:
        if fault_at <= t <= load_end:
            current = current + 1 if n == 0 else 0
            longest = max(longest, current)
    resumed = any(n > 0 for t, n in series if t > fault_at + longest and t <= load_end + 5) if summary["marker"] else True

    alive = state.get("status") == "running"
    lost = expected - received
    if not alive:
        outcome = "exited" + (" (OOM)" if state.get("oom") == "true" else f" (exit {state.get('exit_code')})")
    elif not resumed or summary.get("terminated"):
        outcome = "stalled"
    elif lost < 0:
        outcome = "extra records"
    elif lost <= 1:
        outcome = "ok"
    else:
        outcome = "lost data"
    return {"sent": summary["sent"], "expected": expected, "received": received, "lost": lost,
            "longest_stall_s": longest, "alive": alive, "outcome": outcome, "series": series, "fault_at": fault_at}


def main():
    rows = []
    for product in PRODUCTS:
        for fault in FAULTS:
            run_dir = os.path.join(RESULTS, "data", product, fault)
            if not os.path.exists(os.path.join(run_dir, "generator_summary.json")):
                continue
            r = analyse(run_dir)
            r.update(product=product, fault=fault)
            rows.append(r)
            print(f"{product:15} {fault:12} {r['outcome']:12} received={r['received']}/{r['expected']} "
                  f"lost={r['lost']} stall={r['longest_stall_s']}s alive={r['alive']}")

    fields = ["product", "fault", "outcome", "sent", "expected", "received", "lost", "longest_stall_s", "alive"]
    with open(os.path.join(RESULTS, "summary.csv"), "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields, extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)

    # 每種故障一張圖：各產品每秒送達的筆數
    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    for fault in FAULTS[1:]:
        subset = [r for r in rows if r["fault"] == fault]
        if not subset:
            continue
        fig, ax = plt.subplots(figsize=(9, 4))
        for r in subset:
            ax.plot([t for t, _ in r["series"]], [n for _, n in r["series"]],
                    label=f"{LABELS[r['product']]}: {r['outcome']}")
        ax.axvline(subset[0]["fault_at"], color="#b03a2e", linestyle="--", linewidth=1, label="fault injected")
        ax.set_xlabel("Seconds since load start")
        ax.set_ylabel("Delivered per second")
        ax.set_title(f"{fault}: delivery over time")
        ax.legend(frameon=False, fontsize=8)
        ax.spines[["top", "right"]].set_visible(False)
        fig.tight_layout()
        fig.savefig(os.path.join(RESULTS, "figures", f"{fault.lower()}.svg"))


if __name__ == "__main__":
    main()
