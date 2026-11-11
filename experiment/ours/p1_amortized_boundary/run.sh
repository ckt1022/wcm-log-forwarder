#!/usr/bin/env bash
# P1 Amortized Boundary Crossing：每次跨越 host↔WASM 邊界有固定成本，一次呼叫處理多筆來攤提。
#
# 量測：WCM 在飽和負載下的處理量（負載產生器以遠高於容量的速率送出，由 TCP 回壓限速）。
#   A. 筆數上限 k = 1, 4, 16, 64, 256, 1024（位元組上限放到 16 MiB，只由 k 決定批次大小）
#   B. 位元組上限 = 1, 4, 16, 64, 128, 512 KiB（筆數不設限）
#   各在三種日誌大小（json-fields256／fields1k／fields2k）下各跑一次，REPS 次重複，每次依序交錯。
# 預期：A 的最佳 k 隨日誌大小而變；B 在約數十 KiB 之後趨平，且三種大小的轉折點接近 → 預設以位元組為上限。
#
# 用法：./run.sh                         正式：3 次 × 36 點 × 40 s，約 1.5 小時
#       QUICK=1 ./run.sh                 只跑 fields1k 的 k = 1, 16, 256 與 4, 128 KiB，每點 12 s，約 3 分
# 結果：results/data/<組別>/<日誌>_<值>_r<次>/、results/summary.csv、results/figures/
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"
source "$COMMON/products.env"

MODES=(${MODES:-json-fields256 json-fields1k json-fields2k})
KS=(${KS:-1 4 16 64 256 1024})
CAPS_KIB=(${CAPS_KIB:-1 4 16 64 128 512})
REPS=${REPS:-3}
LOAD_SECS=${LOAD_SECS:-40}
SAT_RATE=200000      # 遠高於容量；實際速率由產品的處理量決定
if [[ "${QUICK:-0}" == 1 ]]; then
    MODES=(json-fields1k); KS=(1 16 256); CAPS_KIB=(4 128); REPS=1; LOAD_SECS=12
fi
export FLUSH=0.1 DRAIN_SECS=3

point() { # <組別> <日誌> <值> <rep> <K> <BYTES_KIB>
    local arm=$1 mode=$2 value=$3 rep=$4
    local out="$HERE/results/data/$arm/${mode}_${value}_r$rep"
    # 負載產生器在 LOAD_SECS 後以 SIGINT 結束（飽和時它送不完排定的筆數）
    local load="timeout -s INT $LOAD_SECS $COMMON/bin/gen -rate $SAT_RATE -duration $LOAD_SECS -traffic flat \
        -mode $mode -ts-mode sched -output tcp -tcp-addr @ADDR@ -send-unit batch -batch-lines 500 -flush-ms 20 \
        -conns 1 -log-file $out/gen.log"
    LOAD_CMD=$load SUBST="K=$5 BYTES_KIB=$6" PRODUCT=wcm PRODUCT_DIR="$HERE" CONF=forwarder.yaml \
        OUT_DIR=$out RATE=$SAT_RATE LOAD_SECS=$LOAD_SECS "$COMMON/lib/run_point.sh" > /dev/null 2>&1
    echo "load_secs=$LOAD_SECS mode=$mode arm=$arm value=$value k=$5 bytes_kib=$6" > "$out/p1_params.txt"
    echo "$(date +%T) $arm $mode $value r$rep $(cat "$out/state.txt" 2>/dev/null)" | tee -a "$HERE/results/run.log"
}

mkdir -p "$HERE/results"
echo "$(date -Is) start modes=${MODES[*]} k=${KS[*]} caps=${CAPS_KIB[*]} reps=$REPS load=${LOAD_SECS}s" >> "$HERE/results/run.log"
for rep in $(seq "$REPS"); do
    for mode in "${MODES[@]}"; do
        for k in "${KS[@]}"; do point count "$mode" "$k" "$rep" "$k" 16384; done
        for c in "${CAPS_KIB[@]}"; do point bytes "$mode" "$c" "$rep" 99999 "$c"; done
    done
done

"$COMMON/venv/bin/python" "$HERE/analyze.py"
