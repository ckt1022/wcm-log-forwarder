#!/usr/bin/env python3
"""demo 用的 HTTP 接收端：接收 forwarder 送來的 NDJSON，存到檔案，並每秒在畫面上印出收到的筆數。

用法：sink.py --port 8080 --out received.ndjson
收到 SIGTERM 時印出總數後結束。"""
import argparse
import signal
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def read_body(handler):
    if "chunked" not in (handler.headers.get("Transfer-Encoding") or ""):
        return handler.rfile.read(int(handler.headers.get("Content-Length", 0)))
    body = b""
    while True:
        size = int(handler.rfile.readline().strip() or b"0", 16)
        if size == 0:
            handler.rfile.readline()
            return body
        body += handler.rfile.read(size)
        handler.rfile.readline()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    lock = threading.Lock()
    state = {"total": 0, "this_second": 0, "first": None}
    out = open(args.out, "ab")
    color = sys.stdout.isatty()
    green, red, reset = ("\033[32m", "\033[31m", "\033[0m") if color else ("", "", "")

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            body = read_body(self)
            lines = [line for line in body.split(b"\n") if line]
            with lock:
                out.write(b"".join(line + b"\n" for line in lines))
                out.flush()
                state["total"] += len(lines)
                state["this_second"] += len(lines)
                if state["first"] is None:
                    state["first"] = time.monotonic()
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *_):
            pass

    def ticker():
        # 收到第一筆後開始，每秒印一行；停住時會看到 0
        second = 0
        while True:
            time.sleep(1)
            with lock:
                if state["first"] is None:
                    continue
                n, state["this_second"] = state["this_second"], 0
                total = state["total"]
            second += 1
            bar = "█" * min(50, (n + 9) // 10)
            mark = f"{red}0  ← 沒有資料送達{reset}" if n == 0 else f"{green}{bar}{reset} {n}"
            print(f"  [接收端] 第 {second:3d} 秒  累計 {total:6d} 筆 | {mark}", flush=True)

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)

    def stop(*_):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, stop)
    threading.Thread(target=ticker, daemon=True).start()
    server.serve_forever()
    out.close()


if __name__ == "__main__":
    main()
