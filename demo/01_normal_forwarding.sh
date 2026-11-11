#!/usr/bin/env bash
# Demo 1：正常的轉發流程
#
# 送出端以每秒 200 筆送 10 秒（共 2,000 筆，level 依序 info／warn／error），
# forwarder 的四個階段：parse 解析 JSON → filter 只留 warn 以上 → format 轉成 json-flat → transport 以 HTTP 送到接收端。
# 預期：接收端收到 warn 與 error 共約 1,333 筆，info 被過濾，沒有任何資料進 dead-letter。
set -uo pipefail
source "$(dirname "$0")/lib/common.sh"
demo_init 01_normal_forwarding

title "Demo 1：正常的轉發流程"
note "送出端 → [parse → filter → format → transport] → HTTP 接收端"

step "啟動 HTTP 接收端（127.0.0.1:$SINK_PORT）與 forwarder（TCP 127.0.0.1:$INPUT_PORT）"
start_sink
start_forwarder normal.yaml
sed -n '/=== WCM/,/=====/p' "$OUT/forwarder.log" | sed 's/^/  /'

step "送出日誌：每秒 200 筆，共 10 秒"
py "$LIB/sender.py" --port "$INPUT_PORT" --rate 200 --secs 10 --summary "$OUT/sender.json"
stop_all

step "結果"
read -r sent expected < <(py -c "import json,sys; s=json.load(open(sys.argv[1])); print(s['sent'], s['expected_delivered'])" "$OUT/sender.json")
received=$(count_lines "$OUT/received.ndjson")
dead=$(dead_letter_count)
echo "  送出            $sent 筆"
echo "  過濾掉（info）  $(( sent - expected )) 筆"
echo "  預期送達        $expected 筆"
echo "  接收端收到      $received 筆"
echo "  dead-letter     $dead 筆"
if [[ "$received" == "$expected" && "$dead" == 0 ]]; then ok "全部送達，沒有遺失"; else bad "與預期不符"; fi

step "一筆日誌經過 forwarder 前後的樣子"
py - "$OUT/received.ndjson" "$OUT/sender.json" <<'EOF'
import json, sys
samples = json.load(open(sys.argv[2]))["samples"]
for line in open(sys.argv[1]):
    out = json.loads(line)
    if str(out.get("seq")) in samples:
        break
print("  輸入（送出端送出的原始日誌）：")
print("    " + samples[str(out["seq"])])
print("  輸出（接收端收到的 json-flat：msg 改名為 message，att 內的欄位攤平到同一層，加上 id）：")
print("    " + json.dumps(out, ensure_ascii=False))
print("  info 等級的日誌（例如 seq 0、3、6）被 filter 過濾，不會出現在接收端。")
EOF

step "forwarder 各階段的統計（關閉時印出）"
grep '^\[summary\]' "$OUT/forwarder.log" | sed -E 's/ (salvaged|budget_exhausted|delivery_retries|recycled_instances|wasm_mem_peak_mb|isolation_calls|isolated_batches|rejected)=[0-9.]+//g; s/^/  /'
note "完整的輸出在 $OUT"
