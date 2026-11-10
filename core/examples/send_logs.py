#!/usr/bin/env python3
"""範例日誌產生器：把 JSON 日誌用 TCP 送進 forwarder。

用法：
  python3 examples/send_logs.py --count 3000
  python3 examples/send_logs.py --count 3000 --bad 5   # 另外混入 5 筆格式錯誤的日誌
"""
import argparse
import json
import socket

LEVELS = ["info", "warn", "error"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=5140)
    parser.add_argument("--count", type=int, default=3000)
    parser.add_argument("--bad", type=int, default=0, help="額外混入的格式錯誤筆數")
    args = parser.parse_args()

    lines = []
    for i in range(args.count):
        record = {"ts": "2026-01-01T00:00:00Z", "level": LEVELS[i % 3], "msg": f"message {i}"}
        lines.append(json.dumps(record))
    step = max(1, args.count // (args.bad + 1))
    for i in range(args.bad):
        lines.insert((i + 1) * step + i, '{"broken json')

    with socket.create_connection((args.host, args.port)) as connection:
        connection.sendall(("\n".join(lines) + "\n").encode())

    # 範例 filter 只保留 warn 以上，info 會被過濾掉
    kept = sum(1 for i in range(args.count) if LEVELS[i % 3] != "info")
    print(f"sent={len(lines)} expected_at_sink={kept} expected_in_error_txt={args.bad}")


if __name__ == "__main__":
    main()
