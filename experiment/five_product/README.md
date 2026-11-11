# 五產品比較

## 產品

| 產品 | 映像 | 自訂邏輯的寫法 |
|---|---|---|
| WCM | `wcm-log-forwarder:experiment`（由 `core/` 建置） | WebAssembly Component（`plugins/rust`），四個階段各一個插件 |
| Fluent Bit (Lua) | `fluent/fluent-bit:5.0.9` | Lua filter（`filter.lua`） |
| Fluent Bit (WASM) | `fluent/fluent-bit:5.0.9` | WASM filter（Rust，wasm32-wasip1，`filter/`） |
| Fluentd | `fluent/fluentd:v1.16-debian-1` | Ruby filter plugin（`plugins/`） |
| Vector | `timberio/vector:0.49.0-alpine` | VRL remap（fault 實驗另加 Lua transform） |

第三方產品沿用先前正式實驗的版本，方便與既有結果對照。實際使用的映像 digest 記錄在 `../common/images.lock`。

## 公平比較的條件

| 項目 | 設定 |
|---|---|
| 資源 | 每個產品一個容器：1 CPU、512 MiB、不允許 swap、host 網路 |
| 輸入 | 單一 TCP 連線，每行一筆 JSON；預設 1 KiB／36 欄位（`json-fields1k`） |
| 處理 | 五個產品做相同的事：解析 JSON → 只保留 warn 以上（約 40%）→ 輸出 json-flat |
| 輸出 | HTTP POST（NDJSON）到同一台機器的接收端 `httpsink`，單一輸出 worker |
| flush 時間窗 W | 五個產品設成相同的值（throughput、burst、fault 為 0.1 s；latency 掃描 0.01–1 s） |

各產品設定以 `@TCP_PORT@`、`@SINK_PORT@`、`@FLUSH@`、`@FLUSH_MS@`、`@FLUSH_THREAD@` 為佔位符，由 `common/lib/run_point.sh` 代入後存到每個量測點的 `conf.*`。

## 實驗

| 實驗 | 目的 | 量測 | 正式時間 | `QUICK=1` |
|---|---|---|---|---|
| [throughput](throughput/run.sh) | 最大可持續吞吐量 | 每個速率 60 s；通過就提高 25%，失敗後二分到 5% 內 | 約 1.5 小時 | 約 2 分 |
| [latency](latency/run.sh) | 固定 5,000 筆／秒，W = 0.01/0.1/0.5/1 s 的 p99 與超額延遲 | 每點 180 s、取第 30–150 s，3 次交錯 | 約 3.5 小時 | 約 3 分 |
| [burst](burst/run.sh) | 基準負載中插入 3 倍負載 5/15/30 s，量恢復時間與遺失 | 每次約 2–2.5 分 | 約 40 分 | 約 4 分 |
| [fault](fault/run.sh) | filter 處理特定一筆時無窮迴圈、例外、越界、記憶體爆量 | 每秒 2,000 筆、60 s，第 15 s 注入 | 約 45 分 | 約 7 分 |

### 判定與指標

| 實驗 | 判定／指標 |
|---|---|
| throughput | 可持續 = 負載產生器沒有被卡住（達成率 ≥ 99.5%、送出延遲 p99 ≤ 10 ms）、負載結束時積壓 ≤ 3 秒份、送達 ≥ 99.9%、沒有 OOM（`common/lib/eval_trial.py`） |
| latency | 延遲 = 接收時間 − 日誌產生時間；超額延遲 = p99 − 0.99·W。Fluent Bit (WASM) 在 5,000 筆／秒跟不上，改用 4,000 筆／秒，與其他產品分開比較 |
| burst | 恢復時間 = 突發結束後，每秒送達筆數連續 5 秒回到基準水準（±10%）所需的時間；並核對送達筆數 |
| fault | 結果分為：ok（最多損失故障的那 1 筆）、lost data（遺失更多）、extra records（收到的比送出的多）、stalled（程序還在但不再送出）、exited（容器停止或 OOM） |

### fault 實驗的故障

每個產品的 filter 都加上相同的觸發條件：msg 含下列標記時，在處理這一筆時故障。

| 標記 | WCM（fault 插件） | Fluent Bit Lua | Fluent Bit WASM | Fluentd Ruby | Vector Lua |
|---|---|---|---|---|---|
| `FAULT_LOOP` | 無窮迴圈 | `while true do end` | `loop {}` | `loop {}` | `while true do end` |
| `FAULT_PANIC` | `panic!` | `error()` | `panic!` | `raise` | `error()` |
| `FAULT_OOB` | 陣列越界 | 對 nil 取欄位 | 陣列越界 | `[].fetch(1)`（IndexError） | 對 nil 取欄位 |
| `FAULT_MEM` | 持續配置 32 MiB | 持續配置 1 MiB 字串 | 持續配置 1 MiB | 持續配置 1 MiB 字串 | 持續配置 1 MiB 字串 |

WCM 的呼叫時限設為 2 秒（`stage_timeout_ms: 2000`）；其他產品沒有對應的機制，使用預設行為。Fluent Bit Lua 在 fault 實驗使用預設的 `protected_mode true`（Lua 錯誤時丟棄該筆）；throughput、latency、burst 沿用先前實驗的 `false`。

## 每個量測點的輸出

`results/data/<產品>/<量測點>/`：

| 檔案 | 內容 |
|---|---|
| `run_config.txt` | 日期、映像與 digest、速率、資源限制、實際設定 |
| `conf.*` | 代入後的產品設定 |
| `gen.log`、`generator_summary.json` | 負載產生器的紀錄 |
| `recv_per_sec.csv` | 接收端每秒收到的筆數與位元組 |
| `latency_worker_1.csv` | 每筆延遲（latency 實驗） |
| `stats.csv` | 容器每秒的 CPU 與記憶體 |
| `dut.log` | 產品的輸出 |
| `clock_drift.csv` | wall 與 monotonic 時鐘的對照（延遲校正用） |
| `state.txt` | 停止前的容器狀態 |
| `verdict.txt` | 判定結果（throughput、latency） |
