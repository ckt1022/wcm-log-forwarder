#!/usr/bin/env bash
# Demo 3：插件越權存取會被擋下
#
# 使用故障測試插件：msg 含 FAULT_FILE／FAULT_ENV／FAULT_NET 時，插件嘗試
#   讀取主機的 /etc/passwd、列出環境變數、連線到 127.0.0.1:8080（接收端）與 127.0.0.1:8081。
# 第一部分：設定中沒有授權 → 插件 import 了檔案與網路介面，forwarder 在載入時就拒絕，不會啟動。
# 第二部分：授權為 inert（介面存在但沒有任何資源）→ 插件可以執行，但每一次越權存取都失敗；
#           只有 transport 能連到設定中列出的 endpoint（127.0.0.1:8080），8081 沒有收到任何連線。
# forwarder 容器另外設定了環境變數 DEMO_SECRET，用來確認插件看不到 host 程序的環境變數。
set -uo pipefail
source "$(dirname "$0")/lib/common.sh"
demo_init 03_capability_violation 8081

title "Demo 3：插件越權存取"

# ── 第一部分 ──
step "第一部分：插件要求檔案與網路介面，但設定中沒有授權"
note "設定：demo/conf/capability_denied.yaml（沒有 capabilities 區塊 = 全部拒絕）"
conf=$(render_config capability_denied.yaml denied)
start=$(date +%s%N)
docker run --rm "${USER_ARGS[@]}" --network host -v "$conf:/app/forwarder.yaml:ro" "$WCM_IMAGE" > "$OUT/denied.log" 2>&1
code=$?
elapsed=$(awk -v s="$start" -v e="$(date +%s%N)" 'BEGIN { printf "%.2f", (e - s) / 1e9 }')
grep '^\[capability\]' "$OUT/denied.log" | head -6 | cut -c1-150 | sed "s/^/${YELLOW}  /; s/\$/${RESET}/"
if (( code != 0 )); then ok "forwarder 在載入插件時拒絕啟動（exit code $code，${elapsed} 秒），沒有任何資料被處理"; else bad "forwarder 沒有拒絕"; fi

# ── 第二部分 ──
step "第二部分：授權為 inert，讓插件執行並嘗試越權"
note "設定：demo/conf/capability_inert.yaml；另外在 127.0.0.1:8081 啟動一個沒有列在 endpoint 的接收端"
docker run -d --name demo-decoy "${USER_ARGS[@]}" --network host -v "$DEMO_DIR:$DEMO_DIR" "$PY_IMAGE" \
    python3 "$LIB/decoy.py" 8081 "$OUT/decoy.log" >/dev/null
wait_port 8081 20
sleep 0.3; : > "$OUT/decoy.log"     # 清掉上面就緒檢查本身造成的那一次連線
start_sink > /dev/null
start_forwarder capability_inert.yaml DEMO_SECRET=do-not-leak
py "$LIB/sender.py" --port "$INPUT_PORT" --rate 20 --secs 4 --inject FAULT_FILE --inject FAULT_ENV --inject FAULT_NET \
    --inject-at 1 --summary "$OUT/sender.json"
sleep 2
stop_all

step "插件的每一次嘗試（forwarder log 中的 PROBE 行）"
py - "$OUT/forwarder.log" "$OUT/decoy.log" <<'EOF'
import re, sys
color = sys.stdout.isatty()
G, R, Y, X = ("\033[32m", "\033[31m", "\033[33m", "\033[0m") if color else ("", "", "", "")
what = {"file": "讀取檔案", "env": "讀取環境變數", "net": "TCP 連線", "net_tcp": "TCP 連線", "net_http": "HTTP 請求",
        "http_listed": "送出日誌到 endpoint"}
seen, leaks = set(), 0
for line in open(sys.argv[1], errors="replace"):
    m = re.search(r"\[fault_(\w+)\] PROBE (\S+) (.*)", line)
    if not m:
        continue
    stage, kind, rest = m.groups()
    target = re.search(r"(?:target|path)=(\S+)", rest)
    outcome = re.search(r"outcome=(\S+)", rest).group(1)
    key = (stage, kind, target.group(1) if target else "")
    if key in seen:
        continue
    seen.add(key)
    allowed = stage == "transport" and target and target.group(1).startswith("127.0.0.1:8080")
    if outcome == "LEAK" or (outcome in ("CONNECTED", "ACCEPTED") and not allowed):
        leaks += 1
        mark = f"{R}未被擋下{X}"
    elif outcome in ("CONNECTED", "ACCEPTED"):
        mark = f"{G}允許（設定中的 endpoint）{X}"
    else:
        mark = f"{G}擋下{X}"
    detail = f"看到 {re.search(r'vars=(\d+)', rest).group(1)} 個" if kind == "env" else (target.group(1) if target else "")
    print(f"  {stage:9} {what.get(kind, kind)} {detail} → {outcome}，{mark}")
decoy = sum("connection" in l for l in open(sys.argv[2]))
print(f"  {'decoy':9} 127.0.0.1:8081 實際收到的連線 → {decoy} 次，" + (f"{G}沒有任何插件連進來{X}" if decoy == 0 else f"{R}有插件連進來{X}"))
leaks += decoy
print()
print(f"  {G}✔ 所有越權存取都被擋下{X}" if leaks == 0 else f"  {R}✘ {leaks} 次存取沒有被擋下{X}")
EOF
note "完整的 log：$OUT/denied.log（第一部分）、$OUT/forwarder.log（第二部分）"
