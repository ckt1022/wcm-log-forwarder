# 論文實驗（依 pattern 分類）

五個 pattern 各一個目錄。每個目錄只放有明確對照（開／關、參數掃描）或論文需要的存在性證據的實驗。
WCM 都在 Docker 容器中執行（1 CPU、512 MiB、不允許 swap、host 網路），與 [five_product](../five_product/README.md) 共用 `common/` 的工具、映像與量測流程。

| 目錄 | Pattern | 對照 | 主要指標 | 正式時間 | `QUICK=1` |
|---|---|---|---|---|---|
| [p1_amortized_boundary](p1_amortized_boundary/run.sh) | Amortized Boundary Crossing | 筆數上限 k = 1…1024 對位元組上限 1…512 KiB，三種日誌大小 | 飽和處理量（筆／秒） | 約 1.5 小時 | 約 3 分 |
| [p2_streaming_micro_batch](p2_streaming_micro_batch/run.sh) | Streaming Micro-Batch | flush 時間窗 W = 0.01／0.1／0.5／1 s | p99、超額延遲（p99 − 0.99·W） | 約 45 分 | 約 1 分 |
| [p3_watchdog_respawn](p3_watchdog_respawn/run.sh) | Watchdog and Respawn | 呼叫時限 T = 1／2／5 s、watchdog 關閉；PANIC／OOB／MEM；watchdog 開／關的 CPU | 停頓秒數、恢復時間、遺失、CPU | 約 1 小時 | 約 5 分 |
| [p4_poison_bisection](p4_poison_bisection/run.sh) | Poison Record Bisection | 二分開／關 × 壞資料 0／0.1／1／5%；trap 資料 0.1%；filter／format／transport 的故障 | 正常資料送達率、dead-letter、隔離呼叫次數、CPU | 約 45 分 | 約 3 分 |
| [p5_capability_scoped](p5_capability_scoped/run.sh) | Capability-Scoped Stage | 授權 無／inert／endpoint；載入時、啟動時、執行期 | LOAD_OK／REFUSED、PROBE 結果、decoy 連線數 | 約 3 分 | 同左 |

P3 與 P4 的消融使用 core 的兩個實驗開關（預設開啟，只供實驗）：

| 設定 | 關閉時 |
|---|---|
| `batch.watchdog: false` | 不啟用 epoch interruption，插件呼叫沒有時限 |
| `batch.bisection: false` | 失敗的整批直接寫入 dead-letter，不找出是哪一筆 |

## 執行

先完成 `experiment/common/setup.sh`（見 [../README.md](../README.md)）。修改 `core/` 後要重新執行 setup，WCM 映像才會更新。

```bash
cd experiment/ours/p3_watchdog_respawn
QUICK=1 ./run.sh        # 確認流程
./run.sh                # 正式量測
```

每個 `run.sh` 最後都會呼叫同目錄的 `analyze.py`；只想重新分析時直接執行 `../../common/venv/bin/python analyze.py`。
每個量測點的原始資料在 `results/data/`，檔案說明見 [five_product/README.md](../five_product/README.md#每個量測點的輸出)；
WCM 的量測點另外有 `error.txt`（dead-letter）與 `dut.log` 結尾的 `[summary]` 行（各階段的筆數、隔離呼叫次數、dead-letter 筆數）。

## 各實驗的設計

### P1 Amortized Boundary Crossing

每次 host↔WASM 呼叫有固定成本，一次處理多筆來攤提。負載產生器以每秒 200,000 筆送出（遠高於容量），由 TCP 回壓限速，量 WCM 的飽和處理量。

- **筆數上限**：`micro_batch_records` = 1, 4, 16, 64, 256, 1024，位元組上限放到 16 MiB。
- **位元組上限**：`micro_batch_max_bytes_kib` = 1, 4, 16, 64, 128, 512，筆數不設限。
- 日誌大小：`json-fields256`、`json-fields1k`、`json-fields2k`，3 次重複。
- 處理量 = 每秒送達筆數的中位數 ÷ filter 保留比例。
- 圖：`p1_batch_bytes.svg` 把兩種上限、三種大小畫在同一個橫軸（每次呼叫的 KiB）上。預期曲線重合：攤提取決於每次呼叫的位元組數，所以 core 預設以位元組為上限（128 KiB）。

### P2 Streaming Micro-Batch

資料到達就以小批次處理，只有輸出端依 W 緩衝。直接呼叫 `five_product/latency/run.sh wcm`，條件與五產品比較相同：5,000 筆／秒，每點 180 s，取第 30–150 s，3 次。

- 預期 p99 ≈ W + 固定的處理延遲，超額延遲不隨 W 成長。
- 有其他產品的延遲資料時，圖中以細線一併畫出作為參考。
- 已跑過完整的五產品延遲實驗時，用 `ANALYZE_ONLY=1 ./run.sh`。

### P3 Watchdog and Respawn

parse 改用 `fault/parse.wasm`。每秒 2,000 筆、60 s，第 15 s 送出一筆帶標記的日誌，3 次。

| 情境 | 標記 | 設定 | 預期 |
|---|---|---|---|
| none | — | T = 2 s | 對照 |
| loop_T1／T2／T5 | FAULT_LOOP | T = 1／2／5 s | 停頓時間隨 T 成長，之後恢復，只損失那一筆 |
| loop_off | FAULT_LOOP | `watchdog: false` | parse 永久停住（stalled） |
| panic／oob／mem | FAULT_PANIC／OOB／MEM | T = 2 s | 只損失那一筆，幾乎沒有停頓 |

另外量時限本身的成本：一般插件、每秒 10,000 筆、60 s，watchdog 開／關各 3 次（交錯），比較 CPU 中位數。

### P4 Poison Record Bisection

每秒 5,000 筆、1 KiB、40 s，3 次。每批最多 128 KiB（約 128 筆），等待上限 50 ms，使批次大致填滿。壞資料是每一行獨立以該比例隨機抽樣（`fault_gen.py --seed`，預設 1，可重現）；均勻間隔會讓 1% 時每一批都必定含壞資料，高估二分關閉的損失。

| 情境 | 壞資料 | 預期（二分開） | 預期（二分關） |
|---|---|---|---|
| malformed_<比例>_on／off | 0／0.1／1／5% 的行是截斷的 JSON，parse 對整批回傳錯誤 | 正常資料 100% 送達，dead-letter = 壞資料筆數 | 每筆壞資料連帶整批進 dead-letter |
| panic_0.1_on／off | 0.1% 的行帶 FAULT_PANIC，parse 插件 trap | 同上 | 同上 |
| stage_filter／format／transport | 該階段換成 fault 插件，第 10 s 一筆 FAULT_PANIC | dead-letter 1 筆，其餘照常 | — |

`calls_per_bad`（隔離呼叫次數 ÷ 壞資料筆數）是二分的成本，約為 log₂(批次筆數) 的量級。

### P5 Capability-Scoped Stage

| 部分 | 作法 | 預期 |
|---|---|---|
| A. 載入時檢查 | 映像內每個插件 × 授權（無、inert-fs、inert-fs + inert-sockets、transport 另加 endpoint-sockets），執行 `--check-plugin` | 一般插件不需授權即可載入；fault 插件 import 了檔案與 socket 介面，未授權時 LOAD_REFUSED |
| B. 啟動時拒絕 | fault 插件、設定中沒有任何授權 | 程序在接收資料前結束，exit code ≠ 0 |
| C. 執行期探測 | 四個階段都用 fault 插件（parse／filter／format 給 inert，transport 給 endpoint），送出 FAULT_FILE／ENV／NET | 讀檔與環境變數都失敗或為空；只有 transport 能連到 endpoint 列出的 127.0.0.1:8080；decoy（8081）沒有任何連線 |

fault 插件探測的位址固定為 8080／8081，執行 P5 時這兩個連接埠必須空著。

## 沒有放進來的實驗

以下實驗在舊的 `test/` 目錄中，沒有明確對照，或已不適用於目前的 core，所以不移植。

| 實驗 | 原位置 | 不移植的原因 |
|---|---|---|
| 整批處理（batch mode）對 streaming | `test/compare/pattern_suite` | core 已移除 batch mode。論文引用先前結果：W = 1 s 時，batch 的超額延遲 292 ms，streaming 29 ms（`test/compare/pattern_suite/ANALYSIS.md` §2） |
| 自適應 k | `test/ours/adaptive_k` | core 沒有收入自適應 k；P1 的位元組上限已涵蓋「批次大小與日誌大小無關」的結論 |
| 每筆呼叫的 microbenchmark、copy overhead | `test/ours/per_record_invocation`、`copy_overhead` | 以原生程式量測，需要修改過的 host，與容器中的完整 pipeline 不可比；P1 的 k = 1 已涵蓋 |
| 原生（無 WASM）基準線 | `test/ours/baselines` | 不是任何 pattern 的開／關對照 |
| 低流量延遲（C／Rust 插件） | `test/ours/latency-benchmark-*` | 已由 P2 與五產品延遲取代 |
| 插件記憶體回收（instance 換新） | — | 需要會洩漏記憶體的插件（例如 TinyGo）；改由 `plugins/verify` 驗證 |
