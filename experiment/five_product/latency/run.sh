#!/usr/bin/env bash
# 延遲：固定速率下，比較五個產品在不同輸出 flush 時間窗 W 的延遲。
#
# 每個產品的 flush 時間窗都設成相同的 W，延遲 = 等待時間窗 + 產品本身多花的時間；
# 超額延遲 = p99 − 0.99·W，用來比較產品本身的差異。
# 速率 5,000 筆／秒（Fluent Bit WASM 在 5,000 跟不上，改用 4,000，與其他產品分開比較）。
# 三次重複，每次依產品順序交錯，降低時段漂移的影響。
#
# 用法：./run.sh [產品 ...]
#       QUICK=1 ./run.sh            只跑 W = 0.1、每點 20 s、1 次，用來檢查流程
# 結果：results/data/<產品>/W<W>_r<次>/、results/summary.csv、results/figures/
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
LIB="$HERE/../../common/lib"
if (( $# )); then PRODUCTS=("$@"); else PRODUCTS=(wcm fluentbit-lua fluentbit-wasm fluentd vector); fi

declare -A RATE=([wcm]=5000 [fluentbit-lua]=5000 [fluentbit-wasm]=4000 [fluentd]=5000 [vector]=5000)
declare -A CONF=([wcm]=forwarder.yaml [fluentbit-lua]=fluent-bit.conf [fluentbit-wasm]=fluent-bit.conf [fluentd]=fluent.conf [vector]=vector.yaml)
WINDOWS=(${WINDOWS:-0.01 0.1 0.5 1})

if [[ "${QUICK:-0}" == 1 ]]; then
    export LOAD_SECS=20 DRAIN_SECS=5 LAT_WINDOW=5,15; REPS=1; WINDOWS=(0.1)
else
    export LOAD_SECS=${LOAD_SECS:-180} DRAIN_SECS=${DRAIN_SECS:-20} LAT_WINDOW=${LAT_WINDOW:-30,150}; REPS=${REPS:-3}
fi
export PARSE_TS=true

mkdir -p "$HERE/results"
echo "$(date -Is) start products=${PRODUCTS[*]} windows=${WINDOWS[*]} reps=$REPS load=${LOAD_SECS}s" >> "$HERE/results/run.log"
for rep in $(seq "$REPS"); do
    for w in "${WINDOWS[@]}"; do
        for product in "${PRODUCTS[@]}"; do
            out="$HERE/results/data/$product/W${w}_r$rep"
            FLUSH=$w PRODUCT=$product PRODUCT_DIR="$HERE/products/$product" CONF=${CONF[$product]} \
                OUT_DIR=$out RATE=${RATE[$product]} "$LIB/run_point.sh" > /dev/null 2>&1
            echo "$(date +%T) $product W=$w r$rep $(head -1 "$out/verdict.txt" 2>/dev/null | cut -c1-120)" | tee -a "$HERE/results/run.log"
        done
    done
done

"$HERE/../../common/venv/bin/python" "$HERE/analyze.py"
