#!/usr/bin/env python3
"""範例 HTTP 接收端：接收 forwarder 送來的 NDJSON，顯示累計筆數。

用法：
  python3 examples/sink.py                 # 監聽 8080，全部回 200
  python3 examples/sink.py --fail-first 3  # 前 3 個請求回 503，用來觀察重試
"""
import argparse
import threading
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
    parser.add_argument("--fail-first", type=int, default=0, help="前 N 個請求回 503")
    args = parser.parse_args()

    lock = threading.Lock()
    state = {"requests": 0, "records": 0}

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            lines = [line for line in read_body(self).split(b"\n") if line]
            with lock:
                state["requests"] += 1
                status = 503 if state["requests"] <= args.fail_first else 200
                if status == 200:
                    state["records"] += len(lines)
                print(f"request={state['requests']} status={status} lines={len(lines)} total={state['records']}", flush=True)
            self.send_response(status)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *args):
            pass

    print(f"sink listening on 127.0.0.1:{args.port}", flush=True)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print(f"\nsink stopped: requests={state['requests']} records={state['records']}")


if __name__ == "__main__":
    main()
