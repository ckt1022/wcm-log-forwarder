#!/usr/bin/env bash
# Demo 2：插件遇到無窮迴圈時，系統如何處理
#
# parse 換成故障測試插件：msg 含 FAULT_LOOP 時進入無窮迴圈。
# 送出端以每秒 200 筆送 25 秒，第 8 秒送出一筆含 FAULT_LOOP 的日誌。
# 預期：
#   1. 該筆所在的批次卡住，接收端短暫收不到資料（幾秒）。
#   2. 每次插件呼叫的時限是 1 秒（stage_timeout_ms），時間到就中止呼叫、丟掉這個 instance、換新的。
#   3. 以二分法找出是哪一筆造成卡住，只把那一筆寫入 dead-letter（error.txt），同批的其他日誌照常送出。
#   4. 流量恢復，卡住期間累積的日誌補送，除了那一筆之外沒有遺失。
set -uo pipefail
source "$(dirname "$0")/lib/common.sh"
demo_init 02_infinite_loop

title "Demo 2：插件陷入無窮迴圈"
note "parse 插件：msg 含 FAULT_LOOP 時執行 loop {}；每次呼叫的時限 1 秒"

step "啟動 HTTP 接收端與 forwarder"
start_sink
start_forwarder infinite_loop.yaml
watch_forwarder '\[parse-(error|isolate)\]|\[dead-letter\]'

step "送出日誌：每秒 200 筆，共 25 秒；第 8 秒插入一筆 FAULT_LOOP"
py "$LIB/sender.py" --port "$INPUT_PORT" --rate 200 --secs 25 --inject FAULT_LOOP --inject-at 8 \
    --summary "$OUT/sender.json"
stop_all

step "結果"
read -r sent expected < <(py -c "import json,sys; s=json.load(open(sys.argv[1])); print(s['sent'], s['expected_delivered'])" "$OUT/sender.json")
received=$(count_lines "$OUT/received.ndjson")
echo "  送出                          $sent 筆（含 1 筆 FAULT_LOOP）"
echo "  預期送達（不含 FAULT_LOOP）   $expected 筆"
echo "  接收端收到                    $received 筆"
if grep -q "FAULT_LOOP" "$OUT/received.ndjson"; then bad "FAULT_LOOP 那一筆被送出了"; fi
if [[ "$received" == "$expected" ]]; then ok "正常的日誌全部送達，只有造成無窮迴圈的那一筆被隔離"; else bad "與預期不符"; fi
stall=$(py - "$OUT/forwarder.log" <<'EOF'
import re, sys
calls = [l for l in open(sys.argv[1]) if l.startswith("[parse-isolate]")]
for l in calls:
    m = re.search(r"calls=(\d+) trap_calls=(\d+)", l)
    if m:
        print(f"二分隔離呼叫 {m.group(1)} 次，其中 {m.group(2)} 次中止（逾時）")
EOF
)
[[ -n "$stall" ]] && echo "  $stall"
note "停頓的時間 ≈ 時限 1 秒 ×（第一次呼叫 + 重試一次）＋ 二分時逐步縮短的探測時限；"
note "停頓期間收到的日誌都留在佇列中，恢復後一次補送（上面那一秒的高峰）。"

step "dead-letter（$OUT/error.txt）"
if [[ -s "$OUT/error.txt" ]]; then
    sed 's/^/  /' "$OUT/error.txt" | cut -c1-200
else
    bad "error.txt 是空的"
fi
note "完整的 forwarder log 在 $OUT/forwarder.log"
