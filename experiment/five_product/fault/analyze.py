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
import matplotlib.ticker  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
PRODUCTS = ["wcm", "fluentbit-lua", "fluentbit-wasm", "fluentd", "vector"]
LABELS = {"wcm": "WCM", "fluentbit-lua": "Fluent Bit (Lua)", "fluentbit-wasm": "Fluent Bit (WASM)",
          "fluentd": "Fluentd", "vector": "Vector"}
COLORS = {"wcm": "#2a78d6", "fluentbit-lua": "#1baf7a", "fluentbit-wasm": "#8a8984", "fluentd": "#eda100", "vector": "#e87ba4"}
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
            "longest_stall_s": longest, "alive": alive, "outcome": outcome, "series": series, "fault_at": fault_at,
            "load_end": load_end}


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

    # 每種故障一張圖：各產品累計收到的日誌筆數。故障後曲線持續上升 = 仍在送出；變成水平 = 停止送出。
    # 灰色虛線是應收的累計量（負載期間每秒送達量固定，到負載結束時達到 expected）。
    os.makedirs(os.path.join(RESULTS, "figures"), exist_ok=True)
    for fault in FAULTS[1:]:
        subset = [r for r in rows if r["fault"] == fault]
        if not subset:
            continue
        fig, ax = plt.subplots(figsize=(9, 4.5))
        ref = next((r for r in rows if r["product"] == "wcm" and r["fault"] == "none"), None)
        if ref:
            ax.plot([0, ref["load_end"], max(t for t, _ in ref["series"])], [0, ref["expected"], ref["expected"]],
                    ":", color="#555555", linewidth=1.5, label=f"expected ({ref['expected']:,})")
        for r in sorted(subset, key=lambda x: x["product"] == "wcm"):   # WCM 最後畫，疊在最上層
            wcm = r["product"] == "wcm"
            total, ts, ys = 0, [], []
            for t, n in r["series"]:
                total += n
                ts.append(t)
                ys.append(total)
            ax.plot(ts, ys, color=COLORS[r["product"]], linewidth=3 if wcm else 1.5, zorder=3 if wcm else 2,
                    label=f"{LABELS[r['product']]}: {r['outcome']} ({r['received']:,})",
                    **(dict(marker="*", markersize=11, markevery=6) if wcm else {}))
        # 停止送出的產品曲線常常重疊成同一條水平線，標註是哪些產品
        stopped = [r for r in subset if r["outcome"] not in ("ok", "extra records")]
        if stopped:
            level = max(r["received"] for r in stopped)
            ax.annotate("stopped: " + ", ".join(LABELS[r["product"]] for r in stopped),
                        xy=(subset[0]["fault_at"] + 15, level), xytext=(0, 8), textcoords="offset points",
                        fontsize=8, color="#444444")
        ax.axvline(subset[0]["fault_at"], color="#b03a2e", linestyle="--", linewidth=1, label="fault injected")
        ax.set_xlabel("Seconds since load start")
        ax.set_ylabel("Cumulative records received")
        ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:,.0f}"))
        ax.set_title(f"{fault}: cumulative delivery (flat line = stopped delivering)")
        ax.legend(frameon=False, fontsize=8)
        ax.spines[["top", "right"]].set_visible(False)
        fig.tight_layout()
        fig.savefig(os.path.join(RESULTS, "figures", f"{fault.lower()}.svg"))


if __name__ == "__main__":
    main()
