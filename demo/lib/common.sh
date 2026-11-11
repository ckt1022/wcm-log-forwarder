# demo 腳本共用的函式（由各 demo 以 source 載入）
#
# 全部在 Docker 容器中執行（host 網路），主機只需要 Docker 與 bash：
#   demo-wcm    forwarder（映像 wcm-demo:latest，由 ../core 建置）
#   demo-sink   HTTP 接收端（lib/sink.py）      demo-decoy  未列在 endpoint 的接收端（lib/decoy.py，Demo 3）
#   送出端與結果整理也在 Python 容器中執行（lib/sender.py 與各 demo 內嵌的程式）
# 每個 demo 的輸出放在 demo/output/<demo 名稱>/。

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$DEMO_DIR/lib"
source "$LIB/images.env"
INPUT_PORT=5140      # forwarder 接收日誌的 TCP 連接埠
SINK_PORT=8080       # HTTP 接收端；故障測試插件的探測位址固定為 8080／8081，所以不可更改
USER_ARGS=(-u "$(id -u):$(id -g)")

if [[ -t 1 ]]; then
    BOLD=$'\e[1m' DIM=$'\e[2m' RED=$'\e[31m' GREEN=$'\e[32m' YELLOW=$'\e[33m' CYAN=$'\e[36m' RESET=$'\e[0m'
else
    BOLD="" DIM="" RED="" GREEN="" YELLOW="" CYAN="" RESET=""
fi

title() { echo; echo "${BOLD}${CYAN}════ $* ════${RESET}"; }
step()  { echo; echo "${BOLD}▶ $*${RESET}"; }
note()  { echo "${DIM}  $*${RESET}"; }
ok()    { echo "${GREEN}  ✔ $*${RESET}"; }
bad()   { echo "${RED}  ✘ $*${RESET}"; }

# 在 Python 容器中執行（demo 目錄以相同路徑掛載，路徑不需轉換）
py() {
    docker run --rm -i "${USER_ARGS[@]}" --network host -e PYTHONUNBUFFERED=1 -e PYTHONDONTWRITEBYTECODE=1 \
        -v "$DEMO_DIR:$DEMO_DIR" -w "$DEMO_DIR" "$PY_IMAGE" python3 "$@"
}

CONTAINERS=(demo-wcm demo-sink demo-decoy)
LOGS_PID="" WATCH_PID=""

cleanup() {
    [[ -n "$WATCH_PID" ]] && kill "$WATCH_PID" 2>/dev/null
    [[ -n "$LOGS_PID" ]] && kill "$LOGS_PID" 2>/dev/null
    [[ -n "${SINK_LOGS_PID:-}" ]] && kill "$SINK_LOGS_PID" 2>/dev/null
    [[ -n "$WATCH_PID" ]] && pkill -P "$WATCH_PID" 2>/dev/null
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1
    WATCH_PID="" LOGS_PID=""
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# 只建立連線、不送任何資料（送出換行會被 forwarder 當成一筆空日誌）
port_open() { (: > "/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# forwarder 關閉時印出的 dead-letter 筆數
dead_letter_count() { grep -o 'dead-letter records=[0-9]*' "$OUT/forwarder.log" | tail -1 | cut -d= -f2; }

wait_port() { # <port> <秒數>
    for _ in $(seq $(( $2 * 10 ))); do port_open "$1" && return 0; sleep 0.1; done
    return 1
}

# 準備輸出目錄並檢查環境：demo_init <demo 名稱> [額外需要的連接埠 ...]
demo_init() {
    OUT="$DEMO_DIR/output/$1"; shift
    command -v docker >/dev/null || { bad "需要 Docker"; exit 1; }
    docker info >/dev/null 2>&1 || { bad "無法使用 Docker（服務未啟動或沒有權限）"; exit 1; }
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1
    if ! docker image inspect "$WCM_IMAGE" >/dev/null 2>&1 || ! docker image inspect "$PY_IMAGE" >/dev/null 2>&1; then
        step "第一次執行：建置映像（約數分鐘）"
        "$DEMO_DIR/setup.sh" || { bad "建置失敗"; exit 1; }
    fi
    for port in "$INPUT_PORT" "$SINK_PORT" "$@"; do
        if port_open "$port"; then bad "連接埠 $port 已被占用，請先關閉使用它的程式"; exit 1; fi
    done
    rm -rf "$OUT"
    mkdir -p "$OUT"
}

# 啟動 HTTP 接收端；每秒在畫面上印出收到的筆數
start_sink() {
    docker run -d --name demo-sink "${USER_ARGS[@]}" --network host -e PYTHONUNBUFFERED=1 \
        -v "$DEMO_DIR:$DEMO_DIR" "$PY_IMAGE" python3 "$LIB/sink.py" --port "$SINK_PORT" --out "$OUT/received.ndjson" >/dev/null
    docker logs -f demo-sink 2>&1 &
    SINK_LOGS_PID=$!
    disown "$SINK_LOGS_PID"
    wait_port "$SINK_PORT" 20 || { bad "接收端無法啟動"; docker logs demo-sink; exit 1; }
}

# 由 conf/ 的模板產生設定（@INPUT_PORT@、@SINK_PORT@），寫到輸出目錄
render_config() { # <模板> → 印出產生的設定路徑
    local conf="$OUT/${2:-forwarder}.yaml"
    sed -e "s#@INPUT_PORT@#$INPUT_PORT#g" -e "s#@SINK_PORT@#$SINK_PORT#g" "$DEMO_DIR/conf/$1" > "$conf"
    echo "$conf"
}

# 啟動 forwarder 容器；log 寫到 forwarder.log，dead-letter 寫到 error.txt
start_forwarder() { # <模板> [環境變數 KEY=VALUE ...]
    local conf env_args=()
    conf=$(render_config "$1"); shift
    for kv in "$@"; do env_args+=(-e "$kv"); done
    docker run -d --name demo-wcm "${USER_ARGS[@]}" --network host "${env_args[@]}" \
        -e WCM_ERROR_FILE=/out/error.txt -v "$OUT:/out" -v "$conf:/app/forwarder.yaml:ro" "$WCM_IMAGE" >/dev/null
    docker logs -f demo-wcm > "$OUT/forwarder.log" 2>&1 &
    LOGS_PID=$!
    if ! wait_port "$INPUT_PORT" 30; then
        bad "forwarder 沒有啟動，log 如下："; docker logs demo-wcm 2>&1 | sed 's/^/    /'; exit 1
    fi
}

# 在畫面上即時顯示 forwarder log 中符合樣式的行
watch_forwarder() { # <grep -E 樣式>
    ( tail -n +1 -F "$OUT/forwarder.log" 2>/dev/null | grep --line-buffered -E "$1" \
        | sed -u "s/^/${YELLOW}  [forwarder] /; s/\$/${RESET}/" ) &
    WATCH_PID=$!
    disown "$WATCH_PID"
}

# 停止 forwarder（SIGTERM，處理完已收到的資料後結束）與接收端
stop_all() {
    # 送出結束後再顯示約 1 秒（最後一批送達），之後停止畫面上的即時輸出（避免負載結束後的 0 被誤解為停住）
    sleep 1.4
    [[ -n "${SINK_LOGS_PID:-}" ]] && { kill "$SINK_LOGS_PID" 2>/dev/null; SINK_LOGS_PID=""; }
    docker stop -t 20 demo-wcm >/dev/null 2>&1
    [[ -n "$LOGS_PID" ]] && { wait "$LOGS_PID" 2>/dev/null; LOGS_PID=""; }
    sleep 0.5
    [[ -n "$WATCH_PID" ]] && { pkill -P "$WATCH_PID" 2>/dev/null; kill "$WATCH_PID" 2>/dev/null; WATCH_PID=""; }
    docker stop -t 5 demo-sink demo-decoy >/dev/null 2>&1
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1
}

count_lines() { [[ -f "$1" ]] && wc -l < "$1" | tr -d ' ' || echo 0; }
