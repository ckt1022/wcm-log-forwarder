#!/usr/bin/env bash
# 吞吐量：搜尋五個產品各自的最大可持續吞吐量（flush 時間窗 W = 0.1 s）。
#
# 每個速率跑一次試驗（common/lib/run_point.sh），以 common/lib/eval_trial.py 判定是否可持續：
#   負載產生器沒有被卡住、負載結束時積壓不超過 3 秒、送達 ≥ 99.9%、沒有 OOM。
# 搜尋：從起始速率開始，通過就乘以 1.25 直到失敗，再在通過與失敗之間二分，直到差距 < 5%。
#
# 用法：./run.sh [產品 ...]            預設跑五個產品
#       QUICK=1 ./run.sh              每個產品只以一半的起始速率跑 1 次（10 s），用來檢查流程
# 結果：results/data/<產品>/rate_<速率>/、results/<產品>_search.txt，最後執行 analyze.py 產生表與圖
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
LIB="$HERE/../../common/lib"
if (( $# )); then PRODUCTS=("$@"); else PRODUCTS=(wcm fluentbit-lua fluentbit-wasm fluentd vector); fi

# 起始速率（約為先前量到的容量的 0.8 倍；可用環境變數覆寫，例如 START_fluentbit_lua=40000）
declare -A START=([wcm]=20000 [fluentbit-lua]=45000 [fluentbit-wasm]=3500 [fluentd]=28000 [vector]=9000)
declare -A CONF=([wcm]=forwarder.yaml [fluentbit-lua]=fluent-bit.conf [fluentbit-wasm]=fluent-bit.conf [fluentd]=fluent.conf [vector]=vector.yaml)

if [[ "${QUICK:-0}" == 1 ]]; then
    export LOAD_SECS=10 DRAIN_SECS=5; RESOLUTION=0.25; MAX_TRIALS=1
else
    export LOAD_SECS=${LOAD_SECS:-60} DRAIN_SECS=${DRAIN_SECS:-20}; RESOLUTION=0.05; MAX_TRIALS=12
fi
export FLUSH=0.1

trial() { # <產品> <速率>：PASS 回傳 0
    local product=$1 rate=$2
    local out="$HERE/results/data/$product/rate_$rate"
    PRODUCT=$product PRODUCT_DIR="$HERE/products/$product" CONF=${CONF[$product]} OUT_DIR=$out RATE=$rate \
        "$LIB/run_point.sh" > /dev/null 2>&1
    local verdict
    verdict=$(head -1 "$out/verdict.txt" 2>/dev/null || echo "FAIL reason=no_verdict")
    echo "$(date +%T) $product rate=$rate ${verdict:0:200}" | tee -a "$HERE/results/${product}_search.txt"
    [[ "$verdict" == PASS* ]]
}

mkdir -p "$HERE/results"
for product in "${PRODUCTS[@]}"; do
    : > "$HERE/results/${product}_search.txt"
    rate=${START[$product]}
    override="START_${product//-/_}"
    [[ -n "${!override:-}" ]] && rate=${!override}
    [[ "${QUICK:-0}" == 1 ]] && rate=$(( rate / 2 ))
    pass=0 fail=0 trials=0
    # 往上找第一個失敗的速率
    while (( trials < MAX_TRIALS )); do
        trials=$(( trials + 1 ))
        if trial "$product" "$rate"; then
            pass=$rate; (( fail )) && break
            rate=$(awk -v r="$rate" 'BEGIN { printf "%d", r * 1.25 }')
        else
            fail=$rate; (( pass )) && break
            rate=$(awk -v r="$rate" 'BEGIN { printf "%d", r * 0.8 }')
        fi
    done
    # 在通過與失敗之間二分
    while (( pass && fail && trials < MAX_TRIALS )) && awk -v p="$pass" -v f="$fail" -v r="$RESOLUTION" 'BEGIN { exit !((f - p) / f > r) }'; do
        trials=$(( trials + 1 ))
        rate=$(( (pass + fail) / 2 ))
        if trial "$product" "$rate"; then pass=$rate; else fail=$rate; fi
    done
    echo "$product capacity=$pass first_fail=$fail trials=$trials" | tee -a "$HERE/results/${product}_search.txt"
done

"$HERE/../../common/venv/bin/python" "$HERE/analyze.py"
