#!/usr/bin/env python3
# eval_trial.py — 判定單次容量試驗是否「可持續」
# 用法: eval_trial.py <out_dir> <rate>
#
# 判定條件(全過 = PASS):
#   C1 gen 未被卡住: gen_avg ≥ 0.995×rate 且 send_lag p99 ≤ 10ms
#   C2 負載結束時 DUT 積壓 ≤ 3 秒份輸出(0.4×rate×3)
#   C3 drain 後 sink 實收 ≥ 99.9% 預期行數(0.4×gen_total)
#   C4 容器未 OOM
import os
import re
import sys


def main():
    out_dir, rate = sys.argv[1], float(sys.argv[2])

    gen_log = open(f"{out_dir}/gen.log").read()
    m = re.search(
        r"\[gen\] done: total=(\d+) bytes=(\d+) elapsed=([\d.]+)s avg=(\d+)/s avg_mb_s=([\d.]+)",
        gen_log)
    if not m:
        print("FAIL reason=gen_no_done_line")
        return 1
    gen_total, gen_bytes = int(m.group(1)), int(m.group(2))
    gen_avg, gen_mbs = float(m.group(4)), float(m.group(5))

    mlag = re.search(r"send_lag: n=\d+ p99<=([\d.]+)ms p999<=([\d.]+)ms max=([\d.]+)ms", gen_log)
    lag_p99 = float(mlag.group(1)) if mlag else -1.0
    lag_max = float(mlag.group(3)) if mlag else -1.0

    sink_epoch = float(open(f"{out_dir}/sink_epoch.txt").read().split()[0])
    gen_start, gen_end = map(float, open(f"{out_dir}/gen_epoch.txt").read().split())

    rows = []
    with open(f"{out_dir}/recv_per_sec.csv") as f:
        next(f)
        for line in f:
            t, lines, byts = line.strip().split(",")
            rows.append((float(t), int(lines), int(byts)))

    sink_total = sum(r[1] for r in rows)
    sink_bytes = sum(r[2] for r in rows)
    recv_by_gen_end = sum(r[1] for r in rows if sink_epoch + r[0] <= gen_end + 0.5)

    # 預期送達比例：filter 只保留 warn 以上（json-fields1k 為 40%）
    keep = float(os.environ.get("KEEP_RATIO", "0.4"))
    expected = keep * gen_total
    backlog = expected - recv_by_gen_end

    oom = open(f"{out_dir}/oom.txt").read().strip() if _exists(f"{out_dir}/oom.txt") else "unknown"

    # 穩態窗 sink 吞吐(gen 開始後 10s ~ gen 結束)
    win = [r for r in rows if gen_start + 10 <= sink_epoch + r[0] <= gen_end]
    win_eps = sum(r[1] for r in win) / len(win) if win else 0.0
    win_mbs = sum(r[2] for r in win) / len(win) / 1e6 if win else 0.0

    c1 = gen_avg >= 0.995 * rate and 0 <= lag_p99 <= 10
    c2 = backlog <= 3 * keep * rate
    c3 = sink_total >= 0.999 * expected
    c4 = oom == "false"

    verdict = "PASS" if (c1 and c2 and c3 and c4) else "FAIL"
    reasons = []
    if not c1:
        reasons.append(f"gen_blocked(avg={gen_avg:.0f}/{rate:.0f},lag_p99={lag_p99}ms)")
    if not c2:
        reasons.append(f"backlog={backlog:.0f}>{3 * keep * rate:.0f}")
    if not c3:
        reasons.append(f"loss(sink={sink_total},expected={expected:.0f})")
    if not c4:
        reasons.append(f"oom={oom}")

    print(f"{verdict} rate={rate:.0f} gen_avg={gen_avg:.0f}/s gen_mb_s={gen_mbs:.3f} "
          f"lag_p99={lag_p99:.0f}ms lag_max={lag_max:.1f}ms "
          f"sink_total={sink_total} expected={expected:.0f} backlog={backlog:.0f} "
          f"win_out_eps={win_eps:.0f} win_out_mb_s={win_mbs:.3f} oom={oom}"
          + (" reasons=" + ";".join(reasons) if reasons else ""))
    return 0 if verdict == "PASS" else 1


def _exists(p):
    import os
    return os.path.exists(p)


if __name__ == "__main__":
    sys.exit(main())
