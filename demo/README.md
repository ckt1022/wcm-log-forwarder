# Demo

三個獨立的展示腳本，每個約 30 秒。全部在 Docker 容器中執行，主機只需要 Docker 與 bash。

| 腳本 | 展示內容 |
|---|---|
| [01_normal_forwarding.sh](01_normal_forwarding.sh) | 正常的轉發流程：日誌經過 parse → filter → format → transport 送到接收端，info 被過濾，其餘全部送達 |
| [02_infinite_loop.sh](02_infinite_loop.sh) | 插件陷入無窮迴圈：流量暫停約 3 秒，時限到後中止並換新的 instance，只隔離造成問題的那一筆，之後恢復並補送 |
| [03_capability_violation.sh](03_capability_violation.sh) | 插件越權：未授權時拒絕載入；授權為 inert 時，讀檔、讀環境變數、連線全部失敗，只有 transport 能連到設定的 endpoint |

## 需求

| 項目 | 說明 |
|---|---|
| Docker | 執行的使用者要能使用 Docker（`docker info` 不報錯） |
| bash | 4 以上 |
| 網路 | 第一次建置映像時需要 |
| 連接埠 | 5140、8080、8081 必須空著（故障測試插件的探測位址固定為 8080／8081） |

`demo/` 要和 `core/` 放在同一層：WCM 映像由 `../core` 的原始碼與 `../core/plugin` 的插件建置。

## 執行

```bash
cd demo
./setup.sh                      # 建置 wcm-demo:latest、下載 Python 映像（第一次約數分鐘；未執行時，第一個 demo 會自動執行）
./01_normal_forwarding.sh
./02_infinite_loop.sh
./03_capability_violation.sh
```

修改 `core/` 或 `core/plugin/` 後，重新執行 `./setup.sh` 才會更新映像。

## 執行時的畫面

- `[接收端]`：每秒收到的筆數與累計筆數，停住時顯示 0。
- `[送出端]`：送出特殊日誌（FAULT_LOOP 等）的時間與內容。
- `[forwarder]`：forwarder log 中與該 demo 有關的行（例如隔離的紀錄）。

每個 demo 結束時印出結果與判定（✔／✘），完整輸出在 `output/<demo>/`：

| 檔案 | 內容 |
|---|---|
| `forwarder.yaml` | 這次使用的設定（由 `conf/` 的模板產生） |
| `forwarder.log` | forwarder 的完整輸出，結尾的 `[summary]` 為各階段的統計 |
| `received.ndjson` | 接收端收到的每一筆日誌 |
| `error.txt` | dead-letter：被隔離的日誌 |
| `sender.json` | 送出筆數、預期送達筆數、特殊日誌的內容 |
| `denied.log`、`decoy.log` | Demo 3：拒絕載入的訊息、8081 收到的連線紀錄 |

## 各 demo 的細節

### 1. 正常的轉發流程

送出端每秒 200 筆、共 10 秒，level 依序為 info、warn、error。forwarder 使用一般插件（`conf/normal.yaml`）：

1. parse 解析 JSON。
2. filter 只保留 warn 以上。
3. format 轉成 json-flat：`msg` 改名為 `message`，`att` 內的欄位攤平到同一層。
4. transport 以 HTTP POST 送到接收端。

預期結果：接收端收到 1,333 筆（2,000 筆的 2/3），dead-letter 為 0。結果中會對照同一筆日誌的輸入與輸出。

### 2. 無窮迴圈

parse 換成故障測試插件（`fault/parse.wasm`），msg 含 `FAULT_LOOP` 時執行 `loop {}`。每次插件呼叫的時限是 1 秒（`conf/infinite_loop.yaml` 的 `stage_timeout_ms`）。第 8 秒送出一筆 FAULT_LOOP 後：

1. 該批卡住，接收端出現約 2–3 秒的 0。
2. 時限到後中止呼叫、丟掉這個 instance、換新的；重試一次仍逾時。
3. 以二分法找出那一筆，寫入 `error.txt`，同批其他日誌照常處理。
4. 恢復後，停頓期間累積的日誌一次補送（畫面上那一秒的高峰）。

最後核對：除了 FAULT_LOOP 那一筆，所有應送達的日誌都送達。

### 3. 越權存取

四個階段都用故障測試插件。msg 含下列標記時，插件嘗試越權：

| 標記 | 插件的動作 |
|---|---|
| `FAULT_FILE` | 讀取 `/etc/passwd` |
| `FAULT_ENV` | 列出環境變數（forwarder 容器設了 `DEMO_SECRET`） |
| `FAULT_NET` | 連到 127.0.0.1:8080 與 8081 |

| 部分 | 設定 | 預期 |
|---|---|---|
| 一 | `conf/capability_denied.yaml`：沒有任何授權 | forwarder 在載入插件時就拒絕啟動，列出未授權的 import |
| 二 | `conf/capability_inert.yaml`：parse／filter／format 給 inert，transport 的 sockets 給 endpoint | 插件可以執行，但讀檔失敗、看到 0 個環境變數、連線被拒；只有 transport 能連到設定中的 endpoint（8080）；8081 上的 decoy 收到 0 次連線 |

decoy 的連線紀錄不依賴插件的回報，是獨立的證據。
