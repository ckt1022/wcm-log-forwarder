"""讀取一個量測點的輸出（run_point.sh 的 OUT_DIR），供 experiment/ours 的 analyze.py 共用。"""
import csv
import json
import os
import re
import statistics


def kv(text):
    """'a=1 b=x' → {'a': '1', 'b': 'x'}（值不含空白）。"""
    return dict(m.groups() for m in re.finditer(r"(\w+)=(\S+)", text))


def stage_summary(run_dir):
    """WCM 關閉時印出的 [summary] 行 → {stage: {欄位: 數字}}，另含 'dead_letter'。"""
    out = {}
    path = os.path.join(run_dir, "dut.log")
    if not os.path.exists(path):
        return out
    for line in open(path, errors="replace"):
        m = re.match(r"\[summary\] (\S+)\s+(.*)", line.strip())
        if not m:
            continue
        fields = {k: float(v) for k, v in kv(m.group(2)).items() if re.fullmatch(r"[\d.]+", v)}
        if m.group(1) == "dead-letter":
            out["dead_letter"] = int(fields.get("records", 0))
        else:
            out[m.group(1)] = fields
    return out


def total(summary, field):
    """所有 stage 某欄位的總和（例如 isolation_calls、rejected）。"""
    return sum(v.get(field, 0) for k, v in summary.items() if isinstance(v, dict))


def load_json(run_dir, name="generator_summary.json"):
    path = os.path.join(run_dir, name)
    return json.load(open(path)) if os.path.exists(path) else None


def state(run_dir):
    path = os.path.join(run_dir, "state.txt")
    return kv(open(path).read()) if os.path.exists(path) else {}


def recv_series(run_dir):
    """接收端每秒送達筆數，時間軸換成「負載開始後的秒數」。"""
    sink_epoch = float(open(os.path.join(run_dir, "sink_epoch.txt")).read().split()[0])
    gen_start = float(open(os.path.join(run_dir, "gen_epoch.txt")).read().split()[0])
    return [(float(r["t"]) - (gen_start - sink_epoch), int(r["lines"]))
            for r in csv.DictReader(open(os.path.join(run_dir, "recv_per_sec.csv")))]


def received(run_dir):
    return sum(n for _, n in recv_series(run_dir))


def cpu_mem(run_dir, t_from=None, t_to=None):
    """stats.csv（sample_cgroup_stats.sh：elapsed_s,cpu_pct,...,rss_mb）→ (CPU% 中位數, 記憶體 MiB 最大值)。
    只取取樣開始後 [t_from, t_to] 秒；取樣在負載開始前約 0–1 秒啟動。"""
    path = os.path.join(run_dir, "stats.csv")
    rows = list(csv.DictReader(open(path))) if os.path.exists(path) else []
    if not rows:
        return None, None
    sel = [r for r in rows if (t_from is None or float(r["elapsed_s"]) >= t_from)
           and (t_to is None or float(r["elapsed_s"]) <= t_to)]
    cpu = round(statistics.median(float(r["cpu_pct"]) for r in sel), 1) if sel else None
    mem = round(max(float(r["rss_mb"]) for r in rows), 1)
    return cpu, mem


def stall_and_recovery(series, fault_at, load_end, base_window=5):
    """故障後最長的連續零輸出秒數，以及恢復時間（故障時刻到每秒送達回到故障前中位數 50% 以上的第一秒）。"""
    longest = current = 0
    for t, n in series:
        if fault_at <= t <= load_end:
            current = current + 1 if n == 0 else 0
            longest = max(longest, current)
    before = [n for t, n in series if fault_at - base_window <= t < fault_at]
    base = statistics.median(before) if before else 0
    recovery = None
    dipped = False
    for t, n in series:
        # 只看負載期間（結束前 1 秒）：負載結束後送達自然歸零，不算停頓
        if t < fault_at or t > load_end - 1:
            continue
        if n < 0.5 * base:
            dipped = True
        elif dipped:
            recovery = t - fault_at
            break
    if not dipped:
        recovery = 0.0
    return longest, recovery
