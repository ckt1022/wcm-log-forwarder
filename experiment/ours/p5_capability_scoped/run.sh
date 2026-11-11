#!/usr/bin/env bash
# P5 Capability-Scoped Stage：每個階段各自連結 WASI 介面；載入時檢查插件的 import，未授權就拒絕載入；
# 授權為 inert 時介面存在但不給任何資源；transport 的 sockets 只能連到 endpoint 列出的位址。
#
# A. 載入時檢查：映像內每個插件（一般與 fault）× 授權（無、inert-fs、inert-fs + inert-sockets、
#    transport 另加 inert-fs + endpoint-sockets），執行 `--check-plugin`，記錄 LOAD_OK／LOAD_REFUSED。
# B. 啟動時拒絕：fault 插件、不給任何授權，記錄程序是否在送入資料前就結束。
# C. 執行期探測：四個階段都用 fault 插件（forwarder.yaml 的授權），分別送出 FAULT_FILE／FAULT_ENV／FAULT_NET，
#    插件嘗試讀 /etc/passwd、列出環境變數、連到 127.0.0.1:8080（endpoint，接收端）與 8081（未列出，decoy.py）。
#    容器另外設定一個環境變數 P5_SECRET，用來確認插件看不到 host 的環境。
#    插件自己的回報（dut.log 的 PROBE 行）之外，decoy.py 的連線紀錄是獨立的證據。
# 注意：fault 插件探測的位址固定為 8080／8081，這兩個連接埠在執行時必須空著。
#
# 用法：./run.sh            約 3 分（QUICK=1 相同，量測本身很短）
# 結果：results/load_check.csv、results/startup_deny.txt、results/data/probe_<標記>/、results/probes.csv、results/summary.md
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
COMMON="$HERE/../../common"
source "$COMMON/products.env"
RESULTS="$HERE/results"
mkdir -p "$RESULTS"

# ── A. 載入時檢查 ──
echo "stage,plugin,grant,result,reason" > "$RESULTS/load_check.csv"
for stage in parse filter format transport; do
    grants=("" "inert-fs" "inert-fs inert-sockets")
    [[ $stage == transport ]] && grants+=("inert-fs endpoint-sockets")
    for plugin in "$stage.wasm" "fault/$stage.wasm"; do
        for grant in "${grants[@]}"; do
            output=$(docker run --rm --network none --entrypoint /usr/local/bin/wcm-base-log-forwarder "$WCM_IMAGE" \
                --check-plugin "$stage" "/app/plugin/$plugin" $grant 2>&1)
            result=$(grep -o '^LOAD_[A-Z]*' <<< "$output" | head -1)
            reason=$(grep -v '^LOAD_' <<< "$output" | head -2 | tr '\n,"' '  ' | cut -c1-200)
            echo "$stage,$plugin,${grant:-none},${result:-ERROR},\"$reason\"" | tee -a "$RESULTS/load_check.csv"
        done
    done
done

# ── B. 啟動時拒絕（設定中沒有任何授權） ──
deny_conf="$RESULTS/startup_deny.yaml"
sed -e '/^capabilities:/,$d' -e "s/@TCP_PORT@/$TCP_PORT/; s/@SINK_PORT@/8080/; s/@FLUSH_MS@/100/" \
    "$HERE/forwarder.yaml" > "$deny_conf"
start=$(date +%s.%N)
timeout 30 docker run --rm --network host -v "$deny_conf:/app/forwarder.yaml:ro" "$WCM_IMAGE" > "$RESULTS/startup_deny.txt" 2>&1
code=$?
echo "exit_code=$code elapsed_s=$(awk -v s="$start" -v e="$(date +%s.%N)" 'BEGIN { printf "%.2f", e - s }')" \
    | tee -a "$RESULTS/startup_deny.txt"

# ── C. 執行期探測 ──
for marker in FAULT_FILE FAULT_ENV FAULT_NET; do
    out="$RESULTS/data/probe_$marker"
    mkdir -p "$out"
    load="python3 -I $HERE/decoy.py 8081 $out/decoy.log & decoy=\$!; sleep 0.5;
          python3 -I $COMMON/lib/fault_gen.py --addr @ADDR@ --eps 200 --secs 8 --fault-at 3 --marker $marker \
              --summary $out/generator_summary.json;
          sleep 3; kill \$decoy"
    SINK_PORT=8080 DUT_ENV="P5_SECRET=visible-to-host-only" LOAD_CMD=$load PRODUCT=wcm PRODUCT_DIR="$HERE" CONF=forwarder.yaml OUT_DIR=$out RATE=200 \
        DRAIN_SECS=3 FLUSH=0.1 "$COMMON/lib/run_point.sh" > /dev/null 2>&1
    echo "$(date +%T) probe $marker $(grep -c 'PROBE' "$out/dut.log" 2>/dev/null) probe lines, decoy connections: $(grep -c connection "$out/decoy.log" 2>/dev/null)" \
        | tee -a "$RESULTS/run.log"
done

"$COMMON/venv/bin/python" "$HERE/analyze.py"
