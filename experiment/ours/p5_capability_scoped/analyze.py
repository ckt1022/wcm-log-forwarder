#!/usr/bin/env python3
"""P5：整理載入時檢查、啟動時拒絕與執行期探測的結果，輸出 probes.csv 與 summary.md。

執行期探測的結果取自 dut.log 的 PROBE 行（插件自己的回報）；連到 8081 的嘗試另以 decoy.py 的連線紀錄核對。
預期：沒有任何 LEAK；只有 transport 能連到 endpoint 列出的 127.0.0.1:8080。"""
import csv
import glob
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
sys.path.insert(0, os.path.join(HERE, "..", "..", "common", "lib"))
import wcm_log  # noqa: E402


def probes(run_dir):
    rows, seen = [], set()
    for line in open(os.path.join(run_dir, "dut.log"), errors="replace"):
        m = re.search(r"\[fault_(\w+)\] PROBE (\S+) (.*)", line)
        if not m:
            continue
        fields = wcm_log.kv(m.group(3))
        target = fields.get("target") or fields.get("path") or "-"
        label = re.search(r"target=\S+ \(([\w-]+)\)", m.group(3))
        key = (m.group(1), m.group(2), target)
        if key in seen:      # 同一筆可能在重試時探測不只一次
            continue
        seen.add(key)
        rows.append({"stage": m.group(1), "probe": m.group(2), "target": target,
                     "label": label.group(1) if label else "", "outcome": fields.get("outcome", "?"),
                     "detail": m.group(3).split("detail=", 1)[-1][:160] if "detail=" in m.group(3) else
                     f"vars={fields.get('vars')} bytes={fields.get('bytes')}"})
    return rows


def main():
    os.makedirs(RESULTS, exist_ok=True)
    lines = ["# P5 結果", ""]

    path = os.path.join(RESULTS, "load_check.csv")
    if os.path.exists(path):
        checks = list(csv.DictReader(open(path)))
        lines += ["## A. 載入時檢查（--check-plugin）", "", "| 階段 | 插件 | 授權 | 結果 |", "|---|---|---|---|"]
        lines += [f"| {c['stage']} | {c['plugin']} | {c['grant']} | {c['result']} |" for c in checks]
        lines.append("")

    path = os.path.join(RESULTS, "startup_deny.txt")
    if os.path.exists(path):
        text = open(path, errors="replace").read()
        status = wcm_log.kv(text.strip().splitlines()[-1])
        refused = [ln for ln in text.splitlines() if "capability" in ln.lower()][:3]
        verdict = "因能力檢查拒絕載入" if refused else "注意：輸出中沒有 capability 訊息，可能不是能力檢查造成的結束"
        lines += ["## B. 啟動時拒絕（fault 插件、沒有授權）", "",
                  f"exit code {status.get('exit_code')}，{status.get('elapsed_s')} s 內結束；{verdict}。", ""]
        lines += [f"    {ln[:200]}" for ln in refused] + [""]

    all_rows = []
    for run_dir in sorted(glob.glob(os.path.join(RESULTS, "data", "probe_*"))):
        marker = os.path.basename(run_dir)[len("probe_"):]
        if not os.path.exists(os.path.join(run_dir, "dut.log")):
            continue
        for r in probes(run_dir):
            r["marker"] = marker
            all_rows.append(r)
        decoy = os.path.join(run_dir, "decoy.log")
        connections = sum("connection" in ln for ln in open(decoy)) if os.path.exists(decoy) else None
        all_rows.append({"marker": marker, "stage": "decoy.py", "probe": "connections_on_8081", "target": "0.0.0.0:8081",
                         "label": "independent",
                         "outcome": "missing" if connections is None else ("none" if connections == 0 else str(connections)),
                         "detail": ""})
        if not any(r["marker"] == marker and r["stage"] != "decoy.py" for r in all_rows):
            print(f"WARNING {run_dir}: no PROBE lines (WCM did not start or no marker reached a plugin; see dut.log)")
    if all_rows:
        fields = ["marker", "stage", "probe", "target", "label", "outcome", "detail"]
        with open(os.path.join(RESULTS, "probes.csv"), "w", newline="") as f:
            writer = csv.DictWriter(f, fieldnames=fields)
            writer.writeheader()
            writer.writerows(all_rows)
        lines += ["## C. 執行期探測", "", "| 標記 | 階段 | 探測 | 目標 | 結果 |", "|---|---|---|---|---|"]
        lines += [f"| {r['marker']} | {r['stage']} | {r['probe']} | {r['target']} {r['label']} | {r['outcome']} |"
                  for r in all_rows]
        leaks = [r for r in all_rows if r["outcome"] == "LEAK"
                 or (r["outcome"] == "CONNECTED" and not (r["stage"] == "transport" and r["target"] == "127.0.0.1:8080"))]
        lines += ["", f"未預期的存取：{len(leaks)} 筆" + ("" if not leaks else "：" + "; ".join(
            f"{r['stage']} {r['probe']} {r['target']}" for r in leaks))]

    open(os.path.join(RESULTS, "summary.md"), "w").write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
