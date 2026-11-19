#!/usr/bin/env python3
"""可靠性實驗（下游中斷、正常關閉、長時間正確性）的負載產生器。

以固定 EPS 對 TCP 送出 1 KiB JSON 記錄，每筆帶遞增的 "seq"；偶數 seq 為 warn（會被 filter 保留），
奇數為 info（會被丟棄），所以「應送達」= 已送出範圍內的所有偶數 seq。
"ts" 是排程時刻（start + seq/eps，RFC 3339，奈秒），產品被回壓卡住時，等待時間會算進延遲。

用法：seq_gen.py --addr 127.0.0.1:15140 --eps 5000 --secs 150 --summary out.json
收到 SIGTERM 或連線中斷時停止，已完整送出的範圍仍寫進 summary（"terminated": true）。
"""
import argparse
import datetime
import json
import signal
import socket
import time

TZ = datetime.timezone(datetime.timedelta(hours=8))


def ts_string(epoch: float) -> str:
    sec = int(epoch)
    ns = int(round((epoch - sec) * 1e9))
    if ns >= 1_000_000_000:
        sec, ns = sec + 1, ns - 1_000_000_000
    base = datetime.datetime.fromtimestamp(sec, TZ).strftime("%Y-%m-%dT%H:%M:%S")
    return f"{base}.{ns:09d}+08:00"


def build_record(seq: int, ts: str) -> bytes:
    level = "warn" if seq % 2 == 0 else "info"
    prefix = f'{{"ts":"{ts}","level":"{level}","seq":{seq},"msg":"reliability message {seq} '
    suffix = '","att":{"service":"reliability-bench","code":"200"}}'
    pad = 1024 - len(prefix) - len(suffix) - 1
    return (prefix + "x" * pad + suffix + "\n").encode()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--addr", default="127.0.0.1:15140")
    ap.add_argument("--eps", type=int, default=5000)
    ap.add_argument("--secs", type=int, default=60)
    ap.add_argument("--summary", default="generator_summary.json")
    args = ap.parse_args()

    def _term(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, _term)

    host, port = args.addr.split(":")
    sock = socket.create_connection((host, int(port)))
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    total = args.eps * args.secs
    per_tick = max(1, args.eps // 50)          # 每 20 ms 一批
    start_mono, start_wall = time.monotonic(), time.time()
    sent = 0
    max_lag = 0.0
    terminated = False
    try:
        while sent < total:
            target = start_mono + sent / args.eps
            now = time.monotonic()
            if target > now:
                time.sleep(target - now)
            else:
                max_lag = max(max_lag, now - target)
            n = min(per_tick, total - sent)
            buf = bytearray()
            for seq in range(sent, sent + n):
                buf += build_record(seq, ts_string(start_wall + seq / args.eps))
            sock.sendall(buf)
            sent += n
    except (KeyboardInterrupt, BrokenPipeError, ConnectionResetError):
        terminated = True
    elapsed = time.monotonic() - start_mono
    try:
        if not terminated:
            sock.shutdown(socket.SHUT_WR)
    except OSError:
        pass
    sock.close()
    summary = {"sent": sent, "expected": (sent + 1) // 2, "eps_target": args.eps,
               "elapsed_sec": round(elapsed, 3), "planned_sec": args.secs,
               "max_send_lag_sec": round(max_lag, 3), "terminated": terminated,
               "start_wall": start_wall}
    with open(args.summary, "w") as f:
        json.dump(summary, f, indent=2)
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
