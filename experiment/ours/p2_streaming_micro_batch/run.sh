#!/usr/bin/env bash
# P2 Streaming Micro-Batch：資料到達就以小批次處理，只有輸出端依 flush 時間窗 W 緩衝。
# 預期：p99 ≈ W + 固定的處理延遲，超額延遲（p99 − 0.99·W）不隨 W 增加。
#
# 量測與 five_product/latency 相同（5,000 筆／秒、W = 0.01/0.1/0.5/1 s），直接呼叫它只跑 WCM，
# 資料寫在 five_product/latency/results/data/wcm/；若已跑過完整的五產品延遲實驗，可用 ANALYZE_ONLY=1 只做分析。
# 與「整批處理後才送出」的舊模式（batch mode）的比較已移除，引用先前結果（見 ../README.md）。
#
# 用法：./run.sh                    正式：4 個 W × 3 次 × 180 s，約 45 分
#       QUICK=1 ./run.sh            只跑 W = 0.1、20 s，約 1 分
#       ANALYZE_ONLY=1 ./run.sh     不量測，只分析既有資料
# 結果：results/summary.csv、results/figures/p2_latency_vs_window.svg
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"
# 延遲的統計窗口須與量測時相同（five_product/latency：正式 30–150 s，QUICK 5–15 s）
[[ "${QUICK:-0}" == 1 ]] && export LAT_WINDOW=${LAT_WINDOW:-5,15}
if [[ "${ANALYZE_ONLY:-0}" != 1 ]]; then
    "$HERE/../../five_product/latency/run.sh" wcm
fi
mkdir -p "$HERE/results"
"$COMMON/venv/bin/python" "$HERE/analyze.py"
