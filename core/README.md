# WCM Log Forwarder

以 WebAssembly Component Model（WCM）為插件架構的日誌轉發器。日誌依序經過四個階段，每個階段都是可替換的 WASM 元件：

```
TCP／tail 輸入 → Parse → Filter → Format → Transport → HTTP 端點
```

插件在 Wasmtime 沙箱中執行，由 Rust Host 負責調度與管控：

- **插件出錯不中斷**：呼叫逾時或 trap 時換新 instance 重試，再以二分法找出造成錯誤的資料。
- **資料不遺失**：無法處理或送不出去的日誌都會寫入 `error.txt`。
- **權限受限**：預設不給檔案、網路與環境變數；Transport 只能連到設定的端點。

## 環境需求

| 項目 | 版本 |
|---|---|
| OS | Linux（已在 Ubuntu 24.04／WSL2 驗證） |
| Rust | 1.93.1 |
| 建置工具 | `build-essential` |
| Python | 3.x（只有範例工具需要，不需要額外套件） |

```bash
sudo apt-get update
sudo apt-get install -y build-essential python3
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install 1.93.1
rustup default 1.93.1
```

`plugin/` 內已附編譯好的插件，執行時不需要 WASM 工具鏈。

## 從頭到尾跑一次

以下需要三個終端機，都在 repo 根目錄執行。會用到 TCP 5140 和 8080 兩個連接埠。

### 1. 取得程式碼並建置

```bash
git clone <repo-url>
cd <repo>
cargo build --release
```

第一次約需數分鐘，執行檔在 `target/release/wcm-base-log-forwarder`。

### 2. 啟動接收端（終端機 A）

```bash
python3 examples/sink.py
```

### 3. 啟動 forwarder（終端機 B）

```bash
./target/release/wcm-base-log-forwarder forwarder.yaml
```

看到 `[tcp-input] listening on 0.0.0.0:5140` 代表已就緒。

### 4. 送日誌（終端機 C）

```bash
python3 examples/send_logs.py --count 3000
```

輸出：

```
sent=3000 expected_at_sink=2000 expected_in_error_txt=0
```

範例 filter 只保留 `warn` 以上，所以 3,000 筆會有 2,000 筆送達。終端機 A 的 `total=` 應該到達 2000。

### 5. 停止

在終端機 B 按 `Ctrl-C`。forwarder 會處理完佇列中的資料後結束，並印出每個階段的統計：

```
[summary] parse     batches=... input=3000 output=3000 ...
[summary] filter    batches=... input=3000 output=2000 ...
[summary] format    batches=... input=2000 output=2000 ...
[summary] transport batches=... input=2000 output=2000 ...
[summary] dead-letter records=0
```

接收端在終端機 A 按 `Ctrl-C` 結束。

## 即時觀測

forwarder 每秒在 stderr 輸出一行目前的處理狀況：

```
[status] up=12s input=5000/s | parse in=5000/s out=5000/s q=0 | filter in=5000/s out=3333/s q=0 | format in=3333/s out=3333/s q=0 | transport in=3333/s out=3333/s q=120 | sent=20000 rejected=0 retries=0 dead_letter=0
```

| 欄位 | 意義 |
|---|---|
| `up` | 啟動後經過的秒數 |
| `input` | 每秒收到的日誌筆數 |
| `in`／`out` | 該階段每秒開始處理與輸出的筆數 |
| `q` | 該階段尚未處理完的筆數（佇列中加上處理中）；targettag 分流到多個端點時，Transport 的 `q` 是估計值 |
| `sent` | 累計送達端點的筆數 |
| `rejected` | 累計被隔離拒絕的筆數 |
| `retries` | 累計的送出重試次數 |
| `dead_letter` | 累計寫入 `error.txt` 的筆數 |

判讀方式：

- 某階段的 `q` 持續增加，代表它是目前的瓶頸或卡住了。
- `retries` 持續增加，代表端點暫時無法使用。
- `dead_letter` 增加時，到 `error.txt` 查看被保存的資料。

間隔由 `status_interval_secs` 設定（0 為關閉，可熱更新）。完全閒置時不輸出，有資料積壓或卡住時會持續輸出。

只看狀態：

```bash
./target/release/wcm-base-log-forwarder forwarder.yaml 2>&1 | grep '^\[status\]'
```

## 觀察錯誤處理

### 格式錯誤的日誌

重新啟動 forwarder 和接收端後，混入 5 筆壞資料：

```bash
python3 examples/send_logs.py --count 3000 --bad 5
```

好的日誌仍然送達（2,000 筆），5 筆壞資料被隔離到工作目錄的 `error.txt`：

```bash
grep -c 'broken json' error.txt
```

`error.txt` 會持續累加；重跑前可以先刪除。

### 端點暫時無法使用

讓接收端前 3 個請求回 503：

```bash
python3 examples/sink.py --fail-first 3
```

再啟動 forwarder 並送日誌。forwarder 會印出 `[transport] send failed ... retry 1/8`，等待後重試，資料最終全部送達。

### 檢查插件的權限

只做載入時檢查，不啟動管線：

```bash
./target/release/wcm-base-log-forwarder --check-plugin parse plugin/parse.wasm
./target/release/wcm-base-log-forwarder --check-plugin parse plugin/chaos/parse_chaos.wasm
```

第一個回報 `LOAD_OK`。第二個插件要求了未授權的檔案與網路能力，回報 `LOAD_REFUSED` 並列出違規的 import。

## 設定

設定檔是 `forwarder.yaml`，也可以用第一個參數指定其他路徑。相對的插件路徑以「設定檔所在目錄/plugin/」為基準。

| 欄位 | 說明 | 範例值 |
|---|---|---|
| `plugins.*` | 各階段的 `.wasm` | `parse.wasm` |
| `stages.*` | 是否啟用 filter／format／transport；Transport 需要 Format | `true` |
| `input.mode` | `tcp` 或 `tail` | `tcp` |
| `batch.max_wait_ms` | 輸出 flush 時間窗 | `100` |
| `batch.micro_batch_max_bytes_kib` | 處理批次的位元組上限 | `128` |
| `batch.micro_batch_max_wait_ms` | 處理批次的等待上限 | `5` |
| `batch.stage_timeout_ms` | 每次插件呼叫的時限 | `30000` |
| `batch.mem_limit_mb` | 每個 instance 的記憶體上限 | `256` |
| `batch.plugin_instance_reuse` | 是否跨批重用 instance | `true` |
| `transport.request_timeout_ms` | 單次請求逾時 | `30000` |
| `transport.retry.max_retries` | 端點失敗時的重試次數 | `8` |
| `transport.retry.initial_backoff_ms` | 第一次重試的基準等待 | `1000` |
| `transport.retry.max_backoff_ms` | 重試等待上限 | `30000` |
| `endpoint.<字母>` | 端點 URL；插件回傳的 targettag 每個字母對應一個端點 | `http://127.0.0.1:8080/ingest` |
| `status_interval_secs` | 即時狀態的輸出間隔（秒），0 為關閉 | `1` |
| `capabilities.<stage>` | 授權 `filesystem`、`sockets`（`deny`／`inert`，Transport 的 sockets 另可設 `endpoint`） | 預設全部拒絕 |

設定檔每 `config_reload_secs` 秒重新讀取一次。`max_wait_ms`、`micro_batch_*`、`endpoint`、`status_interval_secs` 下一批生效；更換插件檔案會熱更換；其餘參數需要重新啟動。

用 C、Go、C# 編譯的插件通常會連帶 import 檔案或 socket 介面，需要明確設定才能載入（`inert` 只連結介面，插件仍然存取不到任何資源）：

```yaml
capabilities:
  parse:
    filesystem: inert
    sockets: inert
```

Go 或 C# 插件若跨批累積記憶體或重用時出錯，請設定 `plugin_instance_reuse: false`。

## 錯誤處理

四個階段的行為相同：

| 情況 | 處理方式 |
|---|---|
| 插件回傳錯誤 | 二分找出造成錯誤的資料，其餘照常處理 |
| trap、逾時、記憶體超限 | 換新 instance 重試 1 次，仍失敗才二分 |
| 無法定位的資料 | 寫入 `error.txt` |

Transport 另外依失敗原因區分：

| 情況 | 處理方式 |
|---|---|
| 連線錯誤、逾時、HTTP 408／429／5xx（501 除外） | 指數退避加隨機等待後重試，用完後寫入 `error.txt` |
| 其他 4xx、設定錯誤 | 不重送同一批，二分找出造成錯誤的資料 |

傳送語意是 **at-least-once**：逾時或 trap 時無法得知端點是否已收到，重送可能造成重複。

`error.txt` 的路徑可以用環境變數 `WCM_ERROR_FILE` 指定。

## 測試

```bash
cargo test
```

## 目錄結構

```
src/
├── main.rs            啟動流程與 --check-plugin
├── config.rs          設定檔與熱更新
├── plugin.rs          Wasmtime 引擎、WIT 綁定、插件載入
├── capability.rs      各階段的能力檢查
├── input.rs           TCP／tail 輸入與記憶體背壓
├── isolation.rs       二分隔離
├── dead_letter.rs     寫入 error.txt
├── shutdown.rs        SIGTERM／SIGINT 處理
├── output.rs          啟動資訊與結束統計
├── metrics.rs         計數器與即時狀態
└── pipeline/
    ├── mod.rs         組裝四個階段
    ├── runner.rs      共用的 instance 管理、重試與隔離
    ├── parse.rs
    ├── filter.rs
    ├── format.rs
    └── transport.rs
wit/                   插件介面定義
plugin/                編譯好的範例插件
examples/              範例接收端與日誌產生器
```

## 常見問題

| 訊息 | 原因與處理 |
|---|---|
| `cannot bind 0.0.0.0:5140` | 連接埠被占用，修改 `input.tcp.port` |
| `Address already in use`（接收端） | 改用 `--port`，並同步修改 `endpoint` |
| `unknown field ...` | 設定檔含有已移除的欄位，請對照 `forwarder.yaml` |
| `[capability] 拒絕載入` | 插件要求未授權的能力，見上方 `capabilities` |
| ``failed to read from `.../plugin/...` `` | 找不到插件；相對路徑要放在設定檔旁邊的 `plugin/` 下 |
| `[transport] send failed` 持續出現 | 確認接收端已啟動，且 `endpoint` 的位址正確 |
