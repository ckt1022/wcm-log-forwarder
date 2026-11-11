#!/usr/bin/env bash
# 突發恢復：穩定負載中插入一段短時間的過載，量測恢復時間與是否遺失。
#
# 基準負載為各產品容量的約一半；第 30 秒起另一條連線再送 2 倍基準速率（總負載 3 倍），持續 B 秒。
# 恢復時間 = 突發結束後，到每秒送達筆數連續 5 秒回到基準水準（±10%）為止。
#
# 用法：./run.sh [產品 ...]
#       QUICK=1 ./run.sh            只跑 B = 5 s，突發前 10 s、突發後觀察 20 s，用來檢查流程
# 結果：results/data/<產品>/burst<B>s/、results/summary.csv、results/figures/
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"
source "$COMMON/products.env"
if (( $# )); then PRODUCTS=("$@"); else PRODUCTS=(wcm fluentbit-lua fluentbit-wasm fluentd vector); fi

# 基準速率：有 throughput 結果時用量到的容量的一半，否則用下列估計值（可用 BASE_<產品>=... 覆寫）
declare -A BASE=([wcm]=12000 [fluentbit-lua]=28000 [fluentbit-wasm]=2000 [fluentd]=17000 [vector]=5500)
declare -A CONF=([wcm]=forwarder.yaml [fluentbit-lua]=fluent-bit.conf [fluentbit-wasm]=fluent-bit.conf [fluentd]=fluent.conf [vector]=vector.yaml)
BURSTS=(${BURSTS:-5 15 30})
LEAD=30          # 突發前的穩定時間
RECOVERY=90      # 突發後保留的觀察時間
export FLUSH=0.1 DRAIN_SECS=10
if [[ "${QUICK:-0}" == 1 ]]; then BURSTS=(5); LEAD=10; RECOVERY=20; DRAIN_SECS=5; fi

mkdir -p "$HERE/results"
for b in "${BURSTS[@]}"; do
    for product in "${PRODUCTS[@]}"; do
        base=${BASE[$product]}
        # 已跑過 throughput 時，改用量到的容量的一半
        measured=$(awk -F, -v p="$product" '$1 == p { print $2 }' "$HERE/../throughput/results/summary.csv" 2>/dev/null)
        [[ -n "$measured" && "$measured" -gt 0 ]] && base=$(( measured / 2 ))
        override="BASE_${product//-/_}"
        [[ -n "${!override:-}" ]] && base=${!override}
        out="$HERE/results/data/$product/burst${b}s"
        total=$(( LEAD + b + RECOVERY ))
        gen_args="-traffic flat -mode $GEN_MODE -ts-mode sched -output tcp -tcp-addr @ADDR@ -send-unit batch -batch-lines 500 -flush-ms 20 -conns 1"
        # 基準負載持續整段時間；第 LEAD 秒起另一條連線加送 2 倍基準速率
        load="$COMMON/bin/gen -rate $base -duration $total $gen_args -log-file $out/gen.log > /dev/null 2>&1 &
              sleep $LEAD
              $COMMON/bin/gen -rate $(( base * 2 )) -duration $b $gen_args -log-file $out/gen_burst.log > /dev/null 2>&1
              wait"
        LOAD_CMD=$load PRODUCT=$product PRODUCT_DIR="$HERE/products/$product" \
            CONF=${CONF[$product]} OUT_DIR=$out RATE=$base "$COMMON/lib/run_point.sh" > /dev/null 2>&1
        echo "base_rate=$base burst_rate=$(( base * 3 )) lead=$LEAD burst_secs=$b" > "$out/burst_params.txt"
        echo "$(date +%T) $product burst=${b}s base=$base" | tee -a "$HERE/results/run.log"
    done
done

"$COMMON/venv/bin/python" "$HERE/analyze.py"
