#!/usr/bin/env python3
"""未列在 endpoint 的 TCP 接收端：記錄每一次連線（插件若繞過限制連進來，這裡會留下紀錄）。
用法：decoy.py <port> <log>；收到 SIGTERM 結束。"""
import signal
import socket
import sys
import time

port, log = int(sys.argv[1]), sys.argv[2]
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", port))
srv.listen(16)
with open(log, "a") as out:
    out.write(f"listening port={port}\n")
    out.flush()
    while True:
        conn, addr = srv.accept()
        out.write(f"{time.time():.3f} connection from {addr[0]}:{addr[1]}\n")
        out.flush()
        conn.close()
