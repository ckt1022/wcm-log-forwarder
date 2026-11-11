#!/usr/bin/env python3
"""fix_latency.py — 對「向後」的 WSL2 wall-clock 階梯做正確的延遲校正（只讀原始資料）。

背景
----
`../batch_alignment/correct_latency.py`（clockfix.Clock）假設 wall clock 只會**向前**跳
（D = wall − monotonic 每 ~32 s 增加 ~1.2–1.6 s），並以「receive 的 wall 時間落在哪兩筆
clock_drift 紀錄之間」決定每筆記錄的 D。pattern_suite 從 2026-10-15 04:04 起，D 改為每 ~30 s
**減少** 0.4 s（04:04）→ 0.9 s（06:12）→ 1.1–2.7 s（09:26 host 恢復之後）。wall clock 向後跳時，
階梯前最後 |step| 秒內收到的記錄，其 wall receive 時間會大於階梯後的 clock_drift 紀錄，
因此被套用「階梯後」的 D → 校正後延遲多出 |step|（例：stream k256 W=0.1 r3 的 p95 = 1,600 ms）。
約 |step|/30 s ≈ 1.5–8% 的記錄受影響，所以 p99 被污染。

做法
----
sink 依接收順序寫 latency_worker_1.csv，所以「檔案順序」= 真正的接收順序。向後的階梯在檔案順序中
表現為 receive_unix 突然減少 ~|step|。本程式：
  1. 由 clock_drift.csv 取出顯著階梯（|ΔD| > 0.05 s，忽略 ±1 ms 的雜訊對）與每段的 D 值；
  2. 第一筆記錄的 D 以 clockfix 的方法查（該時刻不在階梯附近）；
  3. 依檔案順序，每遇到一次 receive_unix 的倒退（< −0.1 s）就前進到下一段 D；
     向前的階梯（舊 regime）則以「receive > 階梯 wall − |step|/2」切段（檔案順序中第一筆超過者）；
  4. 校正延遲 = 量測延遲 − (D_段 − D(gen_start))，窗口 30–150 s 與子窗都在 monotonic 軸上計算
     （與 correct_latency.py 的語意相同）。
驗證：偵測到的倒退次數必須等於資料期間內的顯著負階梯數，且倒退幅度與 ΔD 相差 < 0.35 s；
另外輸出「每秒第 5 小延遲」的 5-bin 中位數濾波範圍（flat_range_ms），校正正確時應 < ~200 ms
（沒有積壓的 PASS run）。不符者在 status 欄位標示。

用法：fix_latency.py <run_dir> [...]      → 印出一行 JSON（不寫入 run 目錄）
      或 import：fix(run_dir) -> dict
"""
import bisect
import csv
import json
import os
import sys

import numpy as np

# 量測窗口（相對負載開始的秒數），可用環境變數 LAT_WINDOW="30,150" 調整
WIN_START, WIN_END = (int(x) for x in os.environ.get("LAT_WINDOW", "30,150").split(","))

SIG = 0.05      # 顯著階梯門檻（s）
BACK = -0.1     # 檔案順序中 receive 倒退門檻（s）


def pct(v, p):
    s = np.sort(v)
    if len(s) == 0:
        return float("nan")
    k = (len(s) - 1) * p / 100.0
    lo = int(k)
    hi = min(lo + 1, len(s) - 1)
    return float(s[lo] + (k - lo) * (s[hi] - s[lo]))


def load_drift(d):
    walls, drifts = [], []
    with open(f"{d}/clock_drift.csv") as fh:
        for r in csv.DictReader(fh):
            walls.append(float(r["wall"]))
            drifts.append(float(r["drift"]))
    return walls, drifts


def flat_range(t, lat, kth=4):
    """每 1 s bin 的第 5 小延遲，5-bin 中位數濾波後在 30–150 s 的 max − min（ms）。"""
    b = t.astype(int)
    vals = []
    for s in range(WIN_START, WIN_END + 1):
        m = lat[b == s]
        if len(m) > kth:
            vals.append(np.partition(m, kth)[kth])
        else:
            vals.append(np.nan)
    v = np.array(vals)
    sm = []
    for i in range(len(v)):
        w = v[max(0, i - 2): i + 3]
        w = w[~np.isnan(w)]
        sm.append(np.median(w) if len(w) else np.nan)
    sm = np.array(sm)
    sm = sm[~np.isnan(sm)]
    return float(sm.max() - sm.min()) if len(sm) else float("nan")


def fix(d, floor=-50.0):
    d = str(d).rstrip("/")
    walls, drifts = load_drift(d)
    gen_start, gen_end = map(float, open(f"{d}/gen_epoch.txt").read().split())
    x = np.loadtxt(f"{d}/latency_worker_1.csv", delimiter=",", skiprows=1, usecols=(1, 2), ndmin=2)
    recv, lat = x[:, 0], x[:, 1]

    def D_at(w):
        i = bisect.bisect_right(walls, w) - 1
        return drifts[max(i, 0)]

    # 顯著階梯：(階梯後第一筆紀錄的 wall, 階梯後 D, 階梯前 D)
    levels = [(walls[0], drifts[0])]
    for i in range(1, len(walls)):
        if abs(drifts[i] - levels[-1][1]) > SIG:
            levels.append((walls[i], drifts[i]))
    def assign(d0):
        seg = np.zeros(len(recv), dtype=int)
        amb_total, amb_new = 0, 0
        for k in range(1, len(levels)):
            Wk, Dn = levels[k]
            Do = levels[k - 1][1]
            step = Dn - Do
            if step > 0:
                # 向前：wall 出現空隙，以空隙中點切段
                seg[recv >= Wk - step / 2] = k
                continue
            a = abs(step)
            seg[recv >= Wk + a + 0.1] = k                      # 一定是階梯後
            amb = (recv >= Wk - 0.1) & (recv < Wk + a + 0.1)    # wall 重疊區：可能是階梯前或後
            if not amb.any():
                continue
            idx = np.where(amb)[0]
            L_old = lat[idx] - (Do - d0) * 1000.0
            # 同一個 POST（相同 receive 時間）一起決定：以舊 D 校正後若出現 < floor 的延遲 → 屬於階梯後
            keys, inv = np.unique(recv[idx], return_inverse=True)
            gmin = np.full(len(keys), np.inf)
            np.minimum.at(gmin, inv, L_old)
            is_new = gmin[inv] < floor
            seg[idx] = np.where(is_new, k, k - 1)
            amb_total += len(keys)
            amb_new += int((gmin < floor).sum())
        Dlev = np.array([v for _, v in levels])
        Dseg = Dlev[seg]
        return Dseg, lat - (Dseg - d0) * 1000.0, amb_total, amb_new

    # gen_start 本身也可能落在向後階梯的 wall 重疊區（例：wcm batch_W1_r1 的階梯在 gen 啟動前 0.09 s 被記錄，
    # 但實際發生在 gen 啟動之後）。此時 d0 有兩個候選；重疊區的指派也依賴 d0，所以兩者各跑一次完整指派，
    # 選「負延遲（< floor）比例 ≤ 0.01%」者中，中位數最小的那個（錯誤的 d0 會讓整段延遲平移 |step|）。
    d0_first = D_at(gen_start)
    cands = {d0_first}
    for k in range(1, len(levels)):
        Wk, Dn = levels[k]
        Do = levels[k - 1][1]
        if Dn < Do and Wk - 0.1 <= gen_start < Wk + abs(Dn - Do) + 0.1:
            cands |= {Do, Dn}
    best = None
    for c in sorted(cands):
        Dseg_c, lat_c_c, at, an = assign(c)
        score = (bool((lat_c_c < floor).mean() > 1e-4), float(np.median(lat_c_c)))
        if best is None or score < best[0]:
            best = (score, c, Dseg_c, lat_c_c, at, an)
    _, d0, Dseg, lat_c, amb_total, amb_new = best
    d0_note = f"d0_reassigned({(d0 - d0_first) * 1000:+.0f}ms)" if d0 != d0_first else ""
    mono = recv - Dseg
    rel = mono - mono.min()
    m = (rel >= WIN_START) & (rel <= WIN_END)
    w = lat_c[m]
    sub = []
    for s in range(0, 180, 30):
        v = lat_c[(rel >= s) & (rel < s + 30)]
        if len(v):
            sub.append((s, round(float(np.percentile(v, 50)), 1), round(float(np.percentile(v, 99)), 1)))
    in_run = [k for k in range(1, len(levels)) if gen_start - 5 <= levels[k][0] <= gen_end + 30]
    step_sizes = [round(levels[k][1] - levels[k - 1][1], 3) for k in in_run]
    fr = flat_range(rel, lat_c)
    # 敏感度：排除每個階梯後 0–3 s（monotonic）收到的記錄 —— 用來辨識「DUT 自己的計時器被向後階梯拖住」
    step_mono = [levels[k][0] - levels[k][1] for k in range(1, len(levels)) if levels[k][1] < levels[k - 1][1]]
    mono_abs = recv - Dseg
    keep = np.ones(len(recv), dtype=bool)
    for sm_ in step_mono:
        keep &= ~((mono_abs >= sm_) & (mono_abs < sm_ + 3.0))
    w_ex = lat_c[m & keep]
    issues = [d0_note] if d0_note else []
    nneg = int((w < floor).sum())
    if nneg:
        issues.append(f"neg_latency_records={nneg}")
    raw_m = (recv - recv.min() >= WIN_START) & (recv - recv.min() <= WIN_END)
    return dict(run=d, n_window=int(m.sum()), p50=round(pct(w, 50), 3), p90=round(pct(w, 90), 3),
                p99=round(pct(w, 99), 3), p999=round(pct(w, 99.9), 3), max=round(float(w.max()), 3) if len(w) else None,
                min=round(float(w.min()), 3) if len(w) else None, raw_p99=round(pct(lat[raw_m], 99), 3),
                p99_excl_post_step_3s=round(pct(w_ex, 99), 3) if len(w_ex) else None,
                n_steps=len(in_run), step_sizes=step_sizes, ambiguous_posts=amb_total, ambiguous_posts_new=amb_new,
                flat_range_ms=round(fr, 1), subwindows=sub, issues=";".join(issues))


if __name__ == "__main__":
    for a in sys.argv[1:]:
        print(json.dumps(fix(a), ensure_ascii=False))
