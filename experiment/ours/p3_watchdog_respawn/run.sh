#!/usr/bin/env bash
# P3 Watchdog and Respawn：每次插件呼叫都設時限（epoch deadline）；逾時或 trap 時丟棄該 instance、換新的繼續。
#
# A. 故障（parse 使用 fault/parse.wasm，每秒 2,000 筆、60 s，第 15 s 送出一筆帶標記的日誌）
#      loop_T1／loop_T2／loop_T5   FAULT_LOOP，時限 T = 1／2／5 s       → 停頓時間應隨 T 成長，之後恢復
#      loop_off                    FAULT_LOOP，watchdog: false（消融）   → parse 永久停住
#      panic／oob／mem             FAULT_PANIC／FAULT_OOB／FAULT_MEM，T = 2 s → 只損失那一筆
#      none                        不注入（對照）
# B. 時限的成本：一般插件、每秒 10,000 筆、60 s，watchdog 開／關的 CPU（交錯 REPS 次）
#
# 用法：./run.sh                 正式：A 8 種 × 3 次 + B 2 × 3 次，約 1 小時
#       QUICK=1 ./run.sh         A 只跑 loop_T1、loop_off、panic（25 s），B 各 1 次 20 s，約 5 分
#       PARTS=A ./run.sh         只跑 A（或 B）
# 結果：results/data/{fault,overhead}/<情境>_r<次>/、results/fault.csv、results/overhead.csv、results/figures/
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"

CASES=(${CASES:-none loop_T1 loop_T2 loop_T5 loop_off panic oob mem})
PARTS=${PARTS:-A B}
REPS=${REPS:-3}
EPS=2000 SECS=60 FAULT_AT=15 FAULT_DRAIN=20
OVERHEAD_RATE=10000 OVERHEAD_SECS=60
if [[ "${QUICK:-0}" == 1 ]]; then
    CASES=(loop_T1 loop_off panic); REPS=1; SECS=25; FAULT_AT=8; FAULT_DRAIN=8; OVERHEAD_SECS=20
fi
export FLUSH=0.1

fault_case() { # <情境> <rep>
    local name=$1 rep=$2 marker timeout=2000 watchdog=true
    case "$name" in
        none) marker="" ;;
        loop_T*) marker=FAULT_LOOP; timeout=$(( ${name#loop_T} * 1000 )) ;;
        loop_off) marker=FAULT_LOOP; watchdog=false ;;
        *) marker="FAULT_${name^^}" ;;
    esac
    local out="$HERE/results/data/fault/${name}_r$rep"
    # watchdog 關閉時 parse 卡住、TCP 回壓會讓負載產生器阻塞；逾時後 fault_gen 仍寫出摘要
    local load="timeout -s TERM $(( SECS + 20 )) python3 -I $COMMON/lib/fault_gen.py --addr @ADDR@ --eps $EPS \
        --secs $SECS --fault-at $FAULT_AT --marker '$marker' --summary $out/generator_summary.json"
    LOAD_CMD=$load SUBST="PARSE=fault/parse.wasm TIMEOUT_MS=$timeout WATCHDOG=$watchdog" DRAIN_SECS=$FAULT_DRAIN \
        PRODUCT=wcm PRODUCT_DIR="$HERE" CONF=forwarder.yaml OUT_DIR=$out RATE=$EPS "$COMMON/lib/run_point.sh" > /dev/null 2>&1
    echo "case=$name marker=$marker timeout_ms=$timeout watchdog=$watchdog" > "$out/p3_params.txt"
    echo "$(date +%T) fault $name r$rep $(cat "$out/state.txt" 2>/dev/null)" | tee -a "$HERE/results/run.log"
}

overhead_case() { # <on|off> <rep>
    local mode=$1 rep=$2 watchdog=true
    [[ $mode == off ]] && watchdog=false
    local out="$HERE/results/data/overhead/watchdog_${mode}_r$rep"
    SUBST="PARSE=parse.wasm TIMEOUT_MS=30000 WATCHDOG=$watchdog" LOAD_SECS=$OVERHEAD_SECS DRAIN_SECS=10 \
        PRODUCT=wcm PRODUCT_DIR="$HERE" CONF=forwarder.yaml OUT_DIR=$out RATE=$OVERHEAD_RATE \
        "$COMMON/lib/run_point.sh" > /dev/null 2>&1
    echo "watchdog=$watchdog rate=$OVERHEAD_RATE load_secs=$OVERHEAD_SECS" > "$out/p3_params.txt"
    echo "$(date +%T) overhead watchdog=$mode r$rep $(head -1 "$out/verdict.txt" 2>/dev/null | cut -c1-100)" | tee -a "$HERE/results/run.log"
}

mkdir -p "$HERE/results"
echo "$(date -Is) start parts=$PARTS cases=${CASES[*]} reps=$REPS" >> "$HERE/results/run.log"
for rep in $(seq "$REPS"); do
    if [[ " $PARTS " == *" A "* ]]; then
        for name in "${CASES[@]}"; do fault_case "$name" "$rep"; done
    fi
    if [[ " $PARTS " == *" B "* ]]; then
        # 開／關的先後每次對調，降低時段漂移的影響
        if (( rep % 2 )); then order=(on off); else order=(off on); fi
        for mode in "${order[@]}"; do overhead_case "$mode" "$rep"; done
    fi
done

"$COMMON/venv/bin/python" "$HERE/analyze.py"
