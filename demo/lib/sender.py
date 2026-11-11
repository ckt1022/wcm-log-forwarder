#!/usr/bin/env python3
"""demo 用的日誌產生器：以固定速率用 TCP 送出 JSON 日誌，可在指定時間插入一筆特殊日誌。

日誌的 level 依序為 info、warn、error 循環；範例 filter 只保留 warn 以上（約 2/3）。
用法：sender.py --rate 200 --secs 10 [--inject "FAULT_LOOP" --inject-at 8] --summary out.json
結束時把送出筆數與預期送達筆數寫到 --summary。"""
import argparse
import json
import socket
import time

LEVELS = ["info", "warn", "error"]
SERVICES = ["checkout", "payment", "inventory"]


def record(seq, level, msg):
    return json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "level": level,
                       "service": SERVICES[seq % 3], "seq": seq, "msg": msg,
                       "att": {"region": "tw", "latency_ms": str(10 + seq % 90)}}) + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=5140)
    parser.add_argument("--rate", type=int, default=200)
    parser.add_argument("--secs", type=int, default=10)
    parser.add_argument("--inject", action="append", default=[], help="插入一筆 msg 含此字串的日誌（可重複）")
    parser.add_argument("--inject-at", type=float, default=5)
    parser.add_argument("--summary", required=True)
    args = parser.parse_args()

    sock = socket.create_connection(("127.0.0.1", args.port))
    total = args.rate * args.secs
    tick = max(1, args.rate // 20)          # 每 50 ms 送一批
    sent = expected = seq = 0
    injected = []
    samples = {}       # 前 20 筆的原始內容，用來對照 forwarder 的輸出
    start = time.monotonic()
    while seq < total:
        wait = start + seq / args.rate - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        buf = []
        if args.inject and time.monotonic() - start >= args.inject_at:
            for marker in args.inject:
                line = record(seq, "warn", f"{marker} (special record)")
                buf.append(line)
                injected.append(line.strip())
                print(f"  [送出端] 第 {time.monotonic() - start:4.1f} 秒送出特殊日誌：{line.strip()[:110]}", flush=True)
            args.inject = []
        for _ in range(min(tick, total - seq)):
            level = LEVELS[seq % 3]
            line = record(seq, level, f"order {seq} processed")
            buf.append(line)
            if seq < 20:
                samples[seq] = line.strip()
            expected += level != "info"
            seq += 1
        sock.sendall("".join(buf).encode())
        sent += len(buf)
    sock.close()
    json.dump({"sent": sent, "normal": total, "expected_delivered": expected, "injected": injected, "samples": samples},
              open(args.summary, "w"), ensure_ascii=False, indent=2)


if __name__ == "__main__":
    main()
