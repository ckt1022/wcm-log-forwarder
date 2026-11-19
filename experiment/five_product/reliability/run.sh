#!/usr/bin/env bash
# 可靠性：下游中斷、正常關閉、長時間正確性。每筆日誌帶序號（common/lib/seq_gen.py），
# 接收端依接收順序記錄序號（httpsink -seq-out），以此計算遺失、重複與亂序。
#
#   outage    負載 150 s，第 40 s 停止接收端、30 s 後重新啟動；W = 1 s，2 次
#   shutdown  負載 30 s，結束 0.5 s 後 docker stop -t 30（SIGTERM，30 s 後 SIGKILL）；W = 1／5 s，各 3 次
#   soak      負載 1 小時；W = 1 s，1 次；另記錄延遲
#
# 各產品的重試設定已對齊（products/ 內註明）：退避 1 s 起、上限 30 s，重試次數不設上限或足以涵蓋中斷。
# 速率與延遲實驗相同：5,000 筆／秒（Fluent Bit WASM 4,000）。
#
# 用法：./run.sh outage|shutdown|soak|smoke [產品 ...]（smoke：每產品 10 s 的流程檢查）
# 結果：results/<實驗>/data/<產品>/<條件>/，之後執行 analyze.py
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
LIB="$HERE/../../common/lib"
EXP=${1:?outage|shutdown|soak}; shift
if (( $# )); then PRODUCTS=("$@"); else PRODUCTS=(wcm fluentbit-lua fluentbit-wasm fluentd vector); fi

declare -A RATE=([wcm]=5000 [fluentbit-lua]=5000 [fluentbit-wasm]=4000 [fluentd]=5000 [vector]=5000)
declare -A CONF=([wcm]=forwarder.yaml [fluentbit-lua]=fluent-bit.conf [fluentbit-wasm]=fluent-bit.conf [fluentd]=fluent.conf [vector]=vector.yaml)
export SINK_ARGS="-seq-out @SINKDIR@/seq.bin"
OUTR="$HERE/results/$EXP"
mkdir -p "$OUTR"

point() {   # point <產品> <條件名稱> <W> <負載秒數> <其他環境變數...>
    local product=$1 name=$2 w=$3 secs=$4; shift 4
    local out="$OUTR/data/$product/$name"
    local gen="python3 -I $LIB/seq_gen.py --addr @ADDR@ --eps ${RATE[$product]} --secs $secs --summary $out/generator_summary.json"
    local load=$gen
    if [[ $EXP == shutdown ]]; then
        # 負載送完 0.5 s 後送出 SIGTERM；記錄送出時刻與容器真正結束的時刻
        load="$gen; sleep 0.5; t0=\$(date +%s.%N); docker stop -t 30 exp-$product >/dev/null;
              echo \"sigterm \$t0 exited \$(date +%s.%N)\" > $out/shutdown.txt"
    fi
    mkdir -p "$out"
    env "$@" LOAD_CMD="$load" FLUSH=$w PRODUCT=$product PRODUCT_DIR="$HERE/products/$product" \
        CONF=${CONF[$product]} OUT_DIR=$out RATE=${RATE[$product]} "$LIB/run_point.sh" > /dev/null 2>&1
    echo "$(date +%T) $EXP $product $name $(tr -d '\n' < "$out/generator_summary.json" 2>/dev/null | tr -s ' ' | cut -c1-110)" \
        | tee -a "$OUTR/run.log"
}

echo "$(date -Is) start $EXP products=${PRODUCTS[*]}" >> "$OUTR/run.log"
case $EXP in
    outage)
        for rep in 1 2; do for p in "${PRODUCTS[@]}"; do
            point "$p" "r$rep" 1 150 SINK_OUTAGE=40:30 DRAIN_SECS=90 PARSE_TS=false
        done; done ;;
    shutdown)
        for rep in 1 2 3; do for w in 1 5; do for p in "${PRODUCTS[@]}"; do
            point "$p" "W${w}_r$rep" "$w" 30 DRAIN_SECS=3 PARSE_TS=false
        done; done; done ;;
    smoke)   # 流程檢查：每個產品 10 s，確認序號能完整通過
        for p in "${PRODUCTS[@]}"; do
            point "$p" "r1" 1 10 DRAIN_SECS=5 PARSE_TS=true
        done ;;
    soak)
        for p in "${PRODUCTS[@]}"; do
            point "$p" "r1" 1 3600 DRAIN_SECS=30 PARSE_TS=true
        done ;;
    *) echo "unknown experiment: $EXP" >&2; exit 2 ;;
esac
echo "$(date -Is) done $EXP" >> "$OUTR/run.log"
