#!/usr/bin/env python3
"""clock_drift_logger.py <out_csv> [interval_s] — 記錄 wall clock 相對 CLOCK_MONOTONIC 的偏移 D = time.time() − time.monotonic()。
WSL2 在 clocksource=tsc 時，wall clock 會被週期性「跳躍」校正（實測每 ~30 s +1.59 s），
而 gen 的 ts（start.Add(offset)）不受影響、sink 的 receive time（time.Now）受影響，
因此延遲需扣掉 D(receive) − D(gen_start)。詳見 correct_latency.py。"""
import sys, time
out = open(sys.argv[1], "w", buffering=1)
iv = float(sys.argv[2]) if len(sys.argv) > 2 else 0.05
out.write("wall,mono,drift\n")
last = None
while True:
    w, m = time.time(), time.monotonic()
    d = w - m
    if last is None or abs(d - last) > 0.0005:
        out.write(f"{w:.6f},{m:.6f},{d:.6f}\n"); last = d
    time.sleep(iv)
