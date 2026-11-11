#!/usr/bin/env bash
# 自訂邏輯出錯：五個產品的 filter 在處理特定一筆日誌時故障，觀察服務是否中斷、遺失幾筆、多久恢復。
#
# 每個產品的 filter 都加上相同的觸發條件：msg 含下列標記時，在處理這一筆時故障。
#   FAULT_LOOP   無窮迴圈            FAULT_PANIC  拋出例外／panic
#   FAULT_OOB    越界存取            FAULT_MEM    持續配置記憶體
#   （none 為不注入故障的對照組）
# 負載：common/lib/fault_gen.py，每秒 2,000 筆、1 KiB，warn 與 info 交替（filter 保留一半），
#       第 15 秒送出一筆帶標記的日誌，共 60 秒。
#
# 用法：./run.sh [產品 ...]
#       QUICK=1 ./run.sh            每次 20 s、只跑 none 與 FAULT_PANIC，用來檢查流程
# 結果：results/data/<產品>/<故障>/、results/summary.csv、results/figures/
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"
if (( $# )); then PRODUCTS=("$@"); else PRODUCTS=(wcm fluentbit-lua fluentbit-wasm fluentd vector); fi
declare -A CONF=([wcm]=forwarder.yaml [fluentbit-lua]=fluent-bit.conf [fluentbit-wasm]=fluent-bit.conf [fluentd]=fluent.conf [vector]=vector.yaml)

FAULTS=(${FAULTS:-none FAULT_LOOP FAULT_PANIC FAULT_OOB FAULT_MEM})
EPS=2000 SECS=60 FAULT_AT=15
export FLUSH=0.1 DRAIN_SECS=30
if [[ "${QUICK:-0}" == 1 ]]; then FAULTS=(none FAULT_PANIC); SECS=20; FAULT_AT=5; DRAIN_SECS=10; fi

mkdir -p "$HERE/results"
for fault in "${FAULTS[@]}"; do
    for product in "${PRODUCTS[@]}"; do
        out="$HERE/results/data/$product/$fault"
        marker=$([[ "$fault" == none ]] && echo "" || echo "$fault")
        # 產品卡住時 TCP 會回壓，負載產生器可能一直阻塞；逾時後 fault_gen 仍會寫出摘要
        load="timeout -s TERM $(( SECS + 30 )) python3 -I $COMMON/lib/fault_gen.py --addr @ADDR@ --eps $EPS \
              --secs $SECS --fault-at $FAULT_AT --marker '$marker' --summary $out/generator_summary.json"
        LOAD_CMD=$load PRODUCT=$product PRODUCT_DIR="$HERE/products/$product" CONF=${CONF[$product]} \
            OUT_DIR=$out RATE=$EPS "$COMMON/lib/run_point.sh" > /dev/null 2>&1
        echo "$(date +%T) $product $fault $(cat "$out/state.txt" 2>/dev/null)" | tee -a "$HERE/results/run.log"
    done
done

"$COMMON/venv/bin/python" "$HERE/analyze.py"
