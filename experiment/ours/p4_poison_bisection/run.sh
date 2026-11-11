#!/usr/bin/env bash
# P4 Poison Record Bisection：一批失敗時遞迴二分，只把真正有問題的那幾筆寫入 dead-letter，其餘照常送出。
#
# 負載：common/lib/fault_gen.py，每秒 5,000 筆、1 KiB，warn／info 交替（filter 保留一半），40 s。
# A. 壞資料比例：截斷的 JSON 佔 0／0.1／1／5%（parse 對整批回傳錯誤），二分 開／關（batch.bisection）
# B. 會 trap 的資料：0.1% 的行帶 FAULT_PANIC（parse 使用 fault/parse.wasm），二分 開／關
# C. 其他階段：filter／format／transport 換成 fault 插件，第 10 s 送出一筆 FAULT_PANIC，二分 開
# 預期：開啟時正常資料 100% 送達、dead-letter 只有壞資料；關閉時每筆壞資料連帶整批（約 128 筆）進 dead-letter。
#
# 用法：./run.sh                 正式：13 種 × 3 次，約 45 分
#       QUICK=1 ./run.sh         只跑 malformed 1% 開／關與 transport 階段，各 15 s，約 3 分
# 結果：results/data/<情境>_r<次>/（含 error.txt）、results/summary.csv、results/figures/
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"

CASES=(${CASES:-malformed_0_on malformed_0.1_on malformed_1_on malformed_5_on
                 malformed_0_off malformed_0.1_off malformed_1_off malformed_5_off
                 panic_0.1_on panic_0.1_off stage_filter stage_format stage_transport})
REPS=${REPS:-3}
EPS=5000 SECS=40 FAULT_AT=10
if [[ "${QUICK:-0}" == 1 ]]; then
    CASES=(malformed_1_on malformed_1_off stage_transport); REPS=1; SECS=15; FAULT_AT=5
fi
export FLUSH=0.1 DRAIN_SECS=10

run_case() { # <情境> <rep>
    local name=$1 rep=$2
    local parse=parse.wasm filter=filter.wasm format=format.wasm transport=transport.wasm bisection=true
    local gen_args="--marker ''"
    case "$name" in
        malformed_*)
            local pct=${name#malformed_}; pct=${pct%_*}
            gen_args="--marker '' --malformed-rate $(awk -v p="$pct" 'BEGIN { print p / 100 }')" ;;
        panic_*)
            local pct=${name#panic_}; pct=${pct%_*}
            parse=fault/parse.wasm
            gen_args="--marker FAULT_PANIC --fault-at 999999 --marker-rate $(awk -v p="$pct" 'BEGIN { print p / 100 }')" ;;
        stage_filter) filter=fault/filter.wasm; gen_args="--marker FAULT_PANIC" ;;
        stage_format) format=fault/format.wasm; gen_args="--marker FAULT_PANIC" ;;
        stage_transport) transport=fault/transport.wasm; gen_args="--marker FAULT_PANIC" ;;
    esac
    [[ "$name" == *_off ]] && bisection=false
    local out="$HERE/results/data/${name}_r$rep"
    local load="timeout -s TERM $(( SECS + 30 )) python3 -I $COMMON/lib/fault_gen.py --addr @ADDR@ --eps $EPS \
        --secs $SECS --fault-at $FAULT_AT $gen_args --summary $out/generator_summary.json"
    LOAD_CMD=$load SUBST="PARSE=$parse FILTER=$filter FORMAT=$format TRANSPORT=$transport BISECTION=$bisection" \
        PRODUCT=wcm PRODUCT_DIR="$HERE" CONF=forwarder.yaml OUT_DIR=$out RATE=$EPS "$COMMON/lib/run_point.sh" > /dev/null 2>&1
    echo "case=$name bisection=$bisection secs=$SECS parse=$parse filter=$filter format=$format transport=$transport" > "$out/p4_params.txt"
    echo "$(date +%T) $name r$rep $(cat "$out/state.txt" 2>/dev/null)" | tee -a "$HERE/results/run.log"
}

mkdir -p "$HERE/results"
echo "$(date -Is) start cases=${CASES[*]} reps=$REPS secs=$SECS" >> "$HERE/results/run.log"
for rep in $(seq "$REPS"); do
    for name in "${CASES[@]}"; do run_case "$name" "$rep"; done
done

"$COMMON/venv/bin/python" "$HERE/analyze.py"
