#!/usr/bin/env bash
# 跑一個量測點：啟動 HTTP 接收端與一個產品的容器，送入負載，收尾後判定是否可持續。
#
# 必要環境變數
#   PRODUCT      wcm | fluentbit-lua | fluentbit-wasm | fluentd | vector
#   PRODUCT_DIR  該產品的目錄（設定模板與 filter 程式碼），以唯讀掛到容器的 /work
#   CONF         設定模板（相對 PRODUCT_DIR）；@TCP_PORT@ @SINK_PORT@ @FLUSH@ @FLUSH_MS@ @FLUSH_THREAD@ 會被替換
#   OUT_DIR      輸出目錄
#   RATE         每秒送出的筆數
# 選用
#   LOAD_SECS(60) DRAIN_SECS(20) FLUSH(0.1，輸出 flush 時間窗，秒)
#   PARSE_TS(false；true 時接收端記錄每筆延遲) KEEP_RATIO(0.4，預期送達比例)
#   LOAD_CMD     自訂負載指令（預設為 gen）；指令中的 @ADDR@ 會換成產品的接收位址
#   DUT_ENV      傳給 WCM 容器的環境變數，空白分隔的 KEY=VALUE
#   SUBST        額外的設定替換，空白分隔的 KEY=VALUE；設定模板中的 @KEY@ 會換成 VALUE（值不可含空白）
#
# 輸出（OUT_DIR）
#   conf.*          實際使用的設定        run_config.txt  映像、參數與條件
#   gen.log         負載產生器的紀錄       recv_per_sec.csv  接收端每秒收到的筆數
#   latency_worker_1.csv（PARSE_TS=true）  stats.csv  容器每秒 CPU 與記憶體
#   dut.log         產品的輸出            clock_drift.csv   wall／monotonic 時鐘對照（延遲校正用）
#   state.txt       停止前的容器狀態（是否仍在執行、exit code、OOM）
#   verdict.txt     PASS／FAIL 與原因
#   error.txt       WCM 的 dead-letter 檔（只有 WCM，且有寫入時才有）
set -uo pipefail
LIB="$(cd "$(dirname "$0")" && pwd)"
COMMON="$(dirname "$LIB")"
source "$COMMON/products.env"

: "${PRODUCT:?}" "${PRODUCT_DIR:?}" "${CONF:?}" "${OUT_DIR:?}" "${RATE:?}"
LOAD_SECS=${LOAD_SECS:-60}
DRAIN_SECS=${DRAIN_SECS:-20}
FLUSH=${FLUSH:-0.1}
PARSE_TS=${PARSE_TS:-false}
export KEEP_RATIO=${KEEP_RATIO:-0.4}
PRODUCT_DIR="$(cd "$PRODUCT_DIR" && pwd)"
CONTAINER="exp-$PRODUCT"

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
rm -f "$OUT_DIR"/*.csv "$OUT_DIR"/*.log "$OUT_DIR"/*.txt "$OUT_DIR"/*.json

# 設定模板 → 實際設定
ext="${CONF##*.}"
CONF_FILE="$OUT_DIR/conf.$ext"
FLUSH_MS=$(awk -v w="$FLUSH" 'BEGIN { printf "%d", w * 1000 }')
# Fluentd 的 flush_thread_interval：W < 1 s 時與 W 相同，W >= 1 s 時固定 0.1（沿用先前正式實驗的設定）
FLUSH_THREAD=$(awk -v w="$FLUSH" 'BEGIN { print (w < 1 ? w : 0.1) }')
sed -e "s/@TCP_PORT@/$TCP_PORT/g" -e "s/@SINK_PORT@/$SINK_PORT/g" -e "s/@FLUSH_MS@/$FLUSH_MS/g" \
    -e "s/@FLUSH_THREAD@/$FLUSH_THREAD/g" -e "s/@FLUSH@/$FLUSH/g" \
    "$PRODUCT_DIR/$CONF" > "$CONF_FILE"
for kv in ${SUBST:-}; do
    key=${kv%%=*} value=${kv#*=}
    sed -i "s|@$key@|$value|g" "$CONF_FILE"
done
if grep -q '@[A-Z_]\+@' "$CONF_FILE"; then
    echo "unreplaced placeholder in $CONF_FILE: $(grep -o '@[A-Z_]\+@' "$CONF_FILE" | sort -u | tr '\n' ' ')" >&2
    exit 2
fi

case "$PRODUCT" in
    wcm) IMAGE=$WCM_IMAGE ;;
    fluentbit-lua | fluentbit-wasm) IMAGE=$FLUENTBIT_IMAGE ;;
    fluentd) IMAGE=$FLUENTD_IMAGE ;;
    vector) IMAGE=$VECTOR_IMAGE ;;
    *) echo "unknown product: $PRODUCT" >&2; exit 2 ;;
esac

{
    echo "date=$(date -Is)"
    echo "product=$PRODUCT image=$IMAGE image_id=$(docker image inspect -f '{{.Id}}' "$IMAGE" 2>/dev/null)"
    echo "rate=$RATE load_secs=$LOAD_SECS drain_secs=$DRAIN_SECS flush=$FLUSH gen_mode=$GEN_MODE"
    echo "cpus=$CPU_LIMIT mem=$MEM_LIMIT keep_ratio=$KEEP_RATIO"
    echo "kernel=$(uname -r) clocksource=$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource 2>/dev/null)"
    echo "----- conf -----"
    cat "$CONF_FILE"
} > "$OUT_DIR/run_config.txt"

SINK_PID="" LOGS_PID="" STATS_PID="" DRIFT_PID="" DONE=0
cleanup() {
    (( DONE )) && return
    DONE=1
    [[ -n "$STATS_PID" ]] && kill "$STATS_PID" 2>/dev/null
    # 停止前的容器狀態：判斷產品在負載期間是否已經中斷
    docker inspect -f 'status={{.State.Status}} exit_code={{.State.ExitCode}} oom={{.State.OOMKilled}} restarts={{.RestartCount}}' \
        "$CONTAINER" > "$OUT_DIR/state.txt" 2>/dev/null
    docker stop -t 10 "$CONTAINER" >/dev/null 2>&1
    [[ "$PRODUCT" == wcm ]] && docker cp "$CONTAINER:/app/error.txt" "$OUT_DIR/error.txt" >/dev/null 2>&1
    docker inspect -f '{{.State.OOMKilled}}' "$CONTAINER" > "$OUT_DIR/oom.txt" 2>/dev/null || echo unknown > "$OUT_DIR/oom.txt"
    docker rm "$CONTAINER" >/dev/null 2>&1
    [[ -n "$LOGS_PID" ]] && kill "$LOGS_PID" 2>/dev/null
    if [[ -n "$SINK_PID" ]]; then
        kill -TERM "$SINK_PID" 2>/dev/null
        for _ in $(seq 40); do kill -0 "$SINK_PID" 2>/dev/null || break; sleep 0.5; done
        kill -9 "$SINK_PID" 2>/dev/null
    fi
    [[ -n "$DRIFT_PID" ]] && kill "$DRIFT_PID" 2>/dev/null
}
trap cleanup EXIT INT TERM
docker rm -f "$CONTAINER" >/dev/null 2>&1

# 1. 時鐘對照與 HTTP 接收端
python3 "$LIB/clock_drift_logger.py" "$OUT_DIR/clock_drift.csv" 0.02 &
DRIFT_PID=$!
date +%s.%N > "$OUT_DIR/sink_epoch.txt"
"$COMMON/bin/httpsink" -port "$SINK_PORT" -out-dir "$OUT_DIR" -parse-ts="$PARSE_TS" > "$OUT_DIR/sink.log" 2>&1 &
SINK_PID=$!

# 2. 產品容器（host 網路；CPU 與記憶體受限，不允許 swap）
LIMITS=(--network host --cpus "$CPU_LIMIT" --memory "$MEM_LIMIT" --memory-swap "$MEM_LIMIT")
case "$PRODUCT" in
    wcm)
        ENV_ARGS=()
        for kv in ${DUT_ENV:-}; do ENV_ARGS+=(-e "$kv"); done
        docker run -d --name "$CONTAINER" "${LIMITS[@]}" "${ENV_ARGS[@]}" \
            -v "$CONF_FILE:/app/forwarder.yaml:ro" "$IMAGE" >/dev/null ;;
    fluentbit-lua | fluentbit-wasm)
        docker run -d --name "$CONTAINER" "${LIMITS[@]}" \
            -v "$PRODUCT_DIR:/work:ro" -v "$CONF_FILE:/etc/fluent-bit.conf:ro" \
            "$IMAGE" -c /etc/fluent-bit.conf >/dev/null ;;
    fluentd)
        docker run -d --name "$CONTAINER" "${LIMITS[@]}" \
            -v "$PRODUCT_DIR/plugins:/fluentd/plugins:ro" -v "$CONF_FILE:/fluentd/etc/fluent.conf:ro" \
            "$IMAGE" >/dev/null ;;
    vector)
        docker run -d --name "$CONTAINER" "${LIMITS[@]}" \
            -v "$PRODUCT_DIR:/work:ro" -v "$CONF_FILE:/etc/vector/vector.yaml:ro" \
            "$IMAGE" --config /etc/vector/vector.yaml >/dev/null ;;
esac
docker logs -f "$CONTAINER" > "$OUT_DIR/dut.log" 2>&1 &
LOGS_PID=$!

for i in $(seq 120); do
    timeout 1 bash -c "echo -n > /dev/tcp/127.0.0.1/$TCP_PORT" 2>/dev/null && break
    if (( i == 120 )); then echo "FAIL reason=not_ready" | tee "$OUT_DIR/verdict.txt"; exit 1; fi
    sleep 1
done
sleep 3
DUT_PID=$(docker inspect -f '{{.State.Pid}}' "$CONTAINER")
"$LIB/sample_cgroup_stats.sh" "$DUT_PID" "$OUT_DIR/stats.csv" "$MEM_LIMIT" "$CPU_LIMIT" &
STATS_PID=$!

# 3. 負載
GEN_START=$(date +%s.%N)
if [[ -n "${LOAD_CMD:-}" ]]; then
    bash -c "${LOAD_CMD//@ADDR@/127.0.0.1:$TCP_PORT}" > "$OUT_DIR/load_cmd.log" 2>&1
else
    timeout -k 5 $(( LOAD_SECS + 90 )) "$COMMON/bin/gen" \
        -rate "$RATE" -duration "$LOAD_SECS" -traffic flat -mode "$GEN_MODE" \
        -ts-mode sched -output tcp -tcp-addr "127.0.0.1:$TCP_PORT" \
        -send-unit batch -batch-lines 500 -flush-ms 20 -conns 1 \
        -log-file "$OUT_DIR/gen.log" > /dev/null 2>&1
fi
echo "$GEN_START $(date +%s.%N)" > "$OUT_DIR/gen_epoch.txt"

# 4. 等待處理完畢、收尾、判定
sleep "$DRAIN_SECS"
cleanup
if [[ -z "${LOAD_CMD:-}" ]]; then
    python3 "$LIB/eval_trial.py" "$OUT_DIR" "$RATE" | tee "$OUT_DIR/verdict.txt"
    grep -q "^PASS" "$OUT_DIR/verdict.txt"
fi
