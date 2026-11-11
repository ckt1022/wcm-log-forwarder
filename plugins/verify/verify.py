#!/usr/bin/env python3
"""用 core 的 forwarder 實際執行插件，確認每筆日誌不是正確送達就是寫入 error.txt。

用法：python3 verify/verify.py <language>
"""
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGINS = os.path.dirname(HERE)
BINARY = os.path.join(PLUGINS, "..", "core", "target", "release", "wcm-base-log-forwarder")
STAGES = ("parse", "filter", "format", "transport")
LEVELS = ("info", "warn", "error")
ID_PATTERN = re.compile(r"id\d{6}")
FAULT_GRANT = "{filesystem: inert, sockets: inert}"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Sink:
    def __init__(self, port):
        self.lines = []
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                body = self.read_body().decode("utf8", "replace")
                with owner.lock:
                    owner.lines += [line for line in body.split("\n") if line]
                self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def read_body(self):
                if "chunked" not in (self.headers.get("Transfer-Encoding") or ""):
                    return self.rfile.read(int(self.headers.get("Content-Length", 0)))
                body = b""
                while True:
                    size = int(self.rfile.readline().strip() or b"0", 16)
                    if size == 0:
                        self.rfile.readline()
                        return body
                    body += self.rfile.read(size)
                    self.rfile.readline()

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


def record(i, suffix=""):
    return {"ts": "2026-01-01T00:00:00Z", "level": LEVELS[i % 3], "msg": f"id{i:06d}{suffix}"}


def make_lines(count, marks=None, malformed=(), padding=0):
    marks = marks or {}
    lines = []
    for i in range(count):
        if i in malformed:
            lines.append(f"{{broken id{i:06d}")
            continue
        fields = record(i, marks.get(i, ""))
        if padding:
            fields["att"] = {"pad": "x" * padding, "n": i, "ok": True}
        if i in marks:
            fields["level"] = "error"  # 故障資料一定要通過 filter，才走得到後面的階段
        lines.append(json.dumps(fields))
    return lines


def dead_letter_records(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf8", errors="replace") as f:
        return [line for line in f if not line.startswith("[")]


def language_grants(language):
    """plugins/<language>/capabilities.yaml：該語言插件需要的權限（沒有檔案代表全部預設拒絕）。"""
    path = os.path.join(PLUGINS, language, "capabilities.yaml")
    if not os.path.exists(path):
        return ""
    with open(path) as f:
        return "".join(line for line in f if not line.lstrip().startswith("#"))


def grant_args(grants, stage):
    match = re.search(rf"{stage}:\s*{{filesystem:\s*(\w+),\s*sockets:\s*(\w+)}}", grants)
    if not match:
        return []
    args = []
    if match.group(1) == "inert":
        args.append("inert-fs")
    if match.group(2) == "inert":
        args.append("inert-sockets")
    return args


def run(language, name, lines, overrides=None, grants=None, expected_records=0, reuse=True, mem_limit_mb=128):
    overrides = overrides or {}
    if grants is None:
        grants = language_grants(language)
    root = tempfile.mkdtemp(prefix=f"verify-{name}-")
    plugin_dir = os.path.join(root, "plugin")
    os.makedirs(plugin_dir)
    for stage in STAGES:
        source = overrides.get(stage, os.path.join(PLUGINS, "out", language, f"{stage}.wasm"))
        shutil.copy(source, os.path.join(plugin_dir, f"{stage}.wasm"))
    port_in, port_out = free_port(), free_port()
    config = os.path.join(root, "forwarder.yaml")
    with open(config, "w") as f:
        f.write(f"""plugins: {{parse: parse.wasm, filter: filter.wasm, format: format.wasm, transport: transport.wasm}}
stages: {{filter: true, format: true, transport: true}}
input: {{mode: tcp, tcp: {{host: "127.0.0.1", port: {port_in}}}}}
batch: {{mem_limit_mb: {mem_limit_mb}, max_wait_ms: 100, channel_capacity: 50000, max_format_chunk: 10000, max_transport_bytes: 131072, transport_workers: 2, stage_timeout_ms: 1000, plugin_instance_reuse: {str(reuse).lower()}}}
transport: {{connect_timeout_ms: 300, request_timeout_ms: 700, retry: {{max_retries: 3, initial_backoff_ms: 20, max_backoff_ms: 100}}}}
endpoint: {{A: "http://127.0.0.1:{port_out}/ingest"}}
status_interval_secs: 0
{grants}
""")
    sink = Sink(port_out)
    error_file = os.path.join(root, "error.txt")
    log_path = os.path.join(root, "forwarder.log")
    with open(log_path, "w") as log:
        process = subprocess.Popen([BINARY, config], stdout=log, stderr=log, env=dict(os.environ, WCM_ERROR_FILE=error_file))
    for _ in range(100):
        try:
            connection = socket.create_connection(("127.0.0.1", port_in))
            break
        except OSError:
            if process.poll() is not None:
                raise SystemExit(f"forwarder exited at startup:\n{open(log_path).read()}")
            time.sleep(0.1)
    connection.sendall(("\n".join(lines) + "\n").encode())
    connection.close()

    deadline = time.time() + 60
    while time.time() < deadline:
        with sink.lock:
            delivered = len(sink.lines)
        if delivered + len(dead_letter_records(error_file)) >= expected_records:
            break
        time.sleep(0.1)
    time.sleep(0.3)
    process.send_signal(signal.SIGTERM)
    code = process.wait(timeout=40)
    sink.stop()
    result = {"code": code, "delivered": list(sink.lines), "dead": dead_letter_records(error_file), "log": open(log_path).read()}
    shutil.rmtree(root, ignore_errors=True)
    return result


def expected_delivered(count, rejected, marks):
    """filter 只保留 warn 以上；帶 marker 的資料一律是 error。"""
    return {f"id{i:06d}" for i in range(count) if i not in rejected and (i in marks or LEVELS[i % 3] != "info")}


def check(results, name, result, count, rejected, marks):
    want = expected_delivered(count, rejected, marks)
    want_dead = {f"id{i:06d}" for i in rejected}
    got = [m for line in result["delivered"] for m in ID_PATTERN.findall(line)]
    dead = {m for line in result["dead"] for m in ID_PATTERN.findall(line)}
    missing = want - set(got)
    extra = set(got) - want
    duplicates = len(got) - len(set(got))
    passed = result["code"] == 0 and not missing and not extra and not duplicates and dead == want_dead
    print(
        f"{'PASS' if passed else 'FAIL'} {name}: delivered={len(set(got))}/{len(want)} "
        f"error_txt={len(dead)}/{len(want_dead)} missing={len(missing)} extra={len(extra)} dup={duplicates} exit={result['code']}"
    )
    if not passed:
        print("\n".join(line[:220] for line in result["log"].splitlines()[-12:]))
    results.append(passed)


def check_load(results, plugin, stage, grant_args, expected):
    output = subprocess.run([BINARY, "--check-plugin", stage, plugin, *grant_args], capture_output=True, text=True).stdout
    passed = output.startswith(expected)
    print(f"{'PASS' if passed else 'FAIL'} load {stage} {' '.join(grant_args) or 'default'}: {output.splitlines()[0][:40]}")
    results.append(passed)


def verify_content(results, language):
    """檢查送達內容的欄位，而不只是筆數。"""
    result = run(language, "content", make_lines(3), expected_records=2)
    rows = [json.loads(line) for line in result["delivered"]]
    text = json.dumps(rows)
    passed = result["code"] == 0 and len(rows) == 2 and "id000001" in text and "id000002" in text and "id000000" not in text
    print(f"{'PASS' if passed else 'FAIL'} content: {result['delivered'][:1]}"[:200])
    results.append(passed)


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    if not os.path.exists(BINARY):
        raise SystemExit("找不到 core 的執行檔，請先在 core/ 執行 cargo build --release")
    language = sys.argv[1]
    results = []
    count = 6000
    fault = os.path.join(PLUGINS, "out", "fault")

    grants = language_grants(language)
    for stage in STAGES:
        args = grant_args(grants, stage)
        plugin = os.path.join(PLUGINS, "out", language, f"{stage}.wasm")
        if args:
            check_load(results, plugin, stage, [], "LOAD_REFUSED")
        check_load(results, plugin, stage, args, "LOAD_OK")
    verify_content(results, language)
    for name, reuse in (("normal reuse", True), ("normal discard", False)):
        kept = len(expected_delivered(count, set(), {}))
        check(results, name, run(language, name.replace(" ", "_"), make_lines(count), expected_records=kept, reuse=reuse), count, set(), {})

    # 長時間重用：記憶體上限較小、輸入量是上限的數倍；插件若沒有釋放記憶體就會 trap
    records = 60000
    kept = len(expected_delivered(records, set(), {}))
    result = run(language, "long_reuse", make_lines(records, padding=1000), expected_records=kept, mem_limit_mb=64)
    check(results, "long reuse (64 MB limit, ~60 MB input)", result, records, set(), {})
    traps = [line for line in result["log"].splitlines() if "-trap" in line or "-isolate]" in line]
    print(f"{'PASS' if not traps else 'FAIL'} long reuse without trap: traps={len(traps)}")
    results.append(not traps)

    malformed = {5, 2500, 5999}
    expected = len(expected_delivered(count, malformed, {})) + len(malformed)
    result = run(language, "malformed", make_lines(count, malformed=malformed), expected_records=expected)
    check(results, "malformed lines", result, count, malformed, {})

    if language == "rust":
        for stage in STAGES:
            plugin = os.path.join(fault, f"{stage}.wasm")
            check_load(results, plugin, stage, [], "LOAD_REFUSED")
            check_load(results, plugin, stage, ["inert-fs", "inert-sockets"], "LOAD_OK")
            grants = f"capabilities: {{{stage}: {FAULT_GRANT}}}\n"
            for marker in ("FAULT_ERR", "FAULT_PANIC", "FAULT_OOB", "FAULT_MEM", "FAULT_LOOP"):
                records = 300 if stage == "transport" and marker == "FAULT_LOOP" else 1500
                marks = {77: marker}
                expected = len(expected_delivered(records, {77}, marks)) + 1
                result = run(language, f"{stage}-{marker}", make_lines(records, marks), {stage: plugin}, grants, expected)
                check(results, f"{stage} {marker}", result, records, {77}, marks)

    failed = results.count(False)
    print(f"ALL PASS ({len(results)})" if not failed else f"FAILED {failed}/{len(results)}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
