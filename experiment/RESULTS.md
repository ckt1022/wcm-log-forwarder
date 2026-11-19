# 實驗結果與說明

本文件整理 `experiment/` 下所有實驗的條件、結果與解讀，分成兩部分：

1. **五產品比較**（[five_product/](five_product/README.md)）：WCM 與四個既有產品在相同條件下的吞吐量、延遲、突發恢復、故障隔離。
2. **Pattern 實驗**（[ours/](ours/README.md)）：只跑 WCM，逐一以開／關或參數掃描驗證五個設計 pattern 的效果。

每節依序為「條件 → 結果（圖 + 表）→ 解讀」。最後一節總結 pattern 的效果與本系統的整體表現，並列出量測限制。

各實驗的設計細節與執行方式見 [README.md](README.md)、[five_product/README.md](five_product/README.md)、[ours/README.md](ours/README.md)；本文件只寫結果。

---

## 0. 實驗環境與共同條件

| 項目 | 設定 |
|---|---|
| 主機 | Windows + WSL2（Linux 6.6.87.2-microsoft-standard-WSL2），Docker 29.8 |
| 資源 | 每個產品一個容器：**1 CPU、512 MiB、不允許 swap、host 網路** |
| 輸入 | 負載產生器 `gen` 以單一 TCP 連線送出 NDJSON；預設每筆 **1 KiB／36 欄位**（`json-fields1k`） |
| 處理 | 五個產品做相同的事：解析 JSON → 只保留 warn 以上（約 **40%**）→ 輸出 json-flat |
| 輸出 | HTTP POST（NDJSON）到同機的接收端 `httpsink`，單一輸出 worker |
| flush 時間窗 W | 五個產品設成相同值（throughput／burst／fault 為 0.1 s，latency 掃描 0.01–5 s） |

| 產品 | 版本 | 自訂邏輯 |
|---|---|---|
| **WCM**（本系統） | 由 `core/` 建置 | WebAssembly Component，parse／filter／format／transport 四個階段各一個插件 |
| Fluent Bit (Lua) | 5.0.9 | Lua filter |
| Fluent Bit (WASM) | 5.0.9 | Rust → wasm32-wasip1 filter |
| Fluentd | v1.16 | Ruby filter plugin |
| Vector | 0.49.0 | VRL remap（fault 實驗另加一個 VRL remap 注入故障） |

**WSL2 時鐘問題與處理方式**：這台 WSL2 的 wall clock 大約每 32 秒向後跳 0.5–0.8 s，monotonic 時鐘也比實際快約 2%。處理方式如下：
- 延遲：以每個量測點記錄的 `clock_drift.csv` 校正（`common/lib/fix_latency.py`）。
- 吞吐量：時鐘跳動曾造成假的 backlog FAIL，所以在時鐘穩定時整組重跑。舊結果保留在 `results_wsl_clockdrift/`。
- burst、fault：以接收端的 monotonic 時間計算，不受跳動影響。

---

# 第一部分：五產品比較

## 1. 最大可持續吞吐量（throughput）

**條件**：每個速率送 60 s。通過就把速率提高 25%，失敗後以二分法逼近，直到上下界相差 5% 以內。同時滿足以下四項才算「可持續」：
- 負載產生器沒有被卡住：達成率 ≥ 99.5%，送出延遲 p99 ≤ 10 ms
- 負載結束時積壓 ≤ 3 秒份
- 送達 ≥ 99.9%
- 沒有 OOM

![吞吐量](five_product/throughput/results/figures/throughput.svg)

| 產品 | 可持續吞吐量（筆／秒） | 第一個失敗的速率 | 達到容量時的 CPU | 記憶體 |
|---|---:|---:|---:|---:|
| Fluent Bit (Lua) | **68,554** | 70,312 | 94.6% | 11 MiB |
| Fluentd | 46,484 | 47,851 | 91.9% | 132 MiB |
| **WCM** | **26,562** | 27,343 | 99.4% | 39 MiB |
| Vector | 14,501 | 14,940 | 91.6% | 34 MiB |
| Fluent Bit (WASM) | 5,468 | 5,638 | 100% | 279 MiB |

**解讀**
- **和其他 WASM 方案比**：WCM 是 Fluent Bit WASM filter 的 **4.9 倍**，記憶體只有它的約 1/7。兩者都用 WASM 跑自訂邏輯。差異主要在 host↔WASM 邊界：
  - Fluent Bit WASM 每筆呼叫一次，而且每筆都要做 msgpack ↔ JSON 轉換。
  - WCM 一次呼叫處理一整批（見 P1）。
- **和原生腳本比**：Fluent Bit Lua 與 Fluentd 比 WCM 快。Lua／Ruby 的 filter 直接在產品的程序內執行，沒有沙箱邊界，也沒有逐階段的隔離。WCM 的四個階段都在 WASM 沙箱中，每個階段之間都有邊界成本；這是取得第 4 節故障隔離與 P5 能力限制所付出的代價。
- WCM 達到容量時 CPU 是 99.4%，表示瓶頸在 CPU，不是 I/O 或回壓設定。

## 2. 延遲對 flush 時間窗（latency）

**條件**：固定每秒 5,000 筆（Fluent Bit WASM 跟不上，改用 4,000 筆）。
- W = 0.01／0.1／0.5／1／2／5 s。
- 每點 180 s，只統計第 30–150 s。
- 3 次重複，依產品交錯執行。

指標：**超額延遲 = p99 − 0.99·W**，也就是扣掉「等時間窗」之後，產品本身多花的時間。接收端在讀完整個 HTTP body 後才記錄接收時間，所以超額延遲包含解析、過濾、組整批、HTTP 傳輸與接收端讀取的時間。

![超額延遲](five_product/latency/results/figures/latency_excess.svg)

圖中 WCM 以星形標記表示，其他產品用圓點；y 軸為對數。

超額延遲（ms，3 次的中位數）：

| 產品 | W=0.01 | 0.1 | 0.5 | 1 | 2 | 5 |
|---|---:|---:|---:|---:|---:|---:|
| **WCM** | 28.8 | 27.3 | 29.0 | 27.6 | 37.8 | 53.8 |
| Fluent Bit (Lua) | 19.4 | 17.3 | 18.5 | 24.3 | 30.6 | 30.4 |
| Fluent Bit (WASM, 4,000/s) | 23.6 | 21.3 | 23.6 | 27.3 | 43.6 | 39.9 |
| Fluentd | 24.3 | 27.7 | 23.7 | 35.0 | 679.9 | 945.1 |
| Vector | 29.7 | 33.4 | 42.9 | 61.7 | 170.6 | 417.8 |

**解讀**

每次 flush 送出約 2,000 × W 筆（每秒 5,000 筆 × 40%）。所以 W 越大，一批越大：W = 5 s 時約 10,000 筆、10 MB 量級。超額延遲可以拆成兩部分：

> 超額延遲 ≈ 固定成本 + 每筆的批次處理成本 × 批次大小

- **W ≤ 1 s**：五個產品都在 17–35 ms，主要是固定成本。同一點的 3 次重複之間就有數 ms 的差異，加上時鐘校正的誤差，所以這一段的排名不具意義。
- **W ≥ 2 s 後分成三群**：

  | 群組 | 超額延遲的變化 | 每多 1,000 筆一批約多花 |
  |---|---|---:|
  | Fluent Bit、WCM | 曲線接近平坦，W = 5 s 時仍在 30–54 ms | Fluent Bit 1–2 ms，WCM 約 3 ms |
  | Vector | 隨批次大小大致線性成長 | 約 40 ms |
  | Fluentd | 在 W = 1 → 2 s 之間跳階（35 → 680 ms），不是線性成長 | — |

  Fluentd 在 W = 5 s 時，p50 只比 W 多約 130 ms，p99 卻多約 900 ms。這比較像偶發的長停頓拉高了尾端。可能的原因是 Ruby GC 或送出大 body 的成本，**目前沒有做 profiling，只是推測**。

## 3. 突發過載後的恢復（burst）

**條件**：
- 基準負載 = 各產品吞吐量容量的 50%。
- 中間插入 **3 倍基準**的負載，持續 5／15／30 s。
- 恢復時間 = 突發結束後，每秒送達筆數連續 5 秒回到基準 ±10% 所需的時間。
- 每個條件只跑 **1 次**。

| 5 s 突發 | 15 s 突發 | 30 s 突發 |
|---|---|---|
| ![](five_product/burst/results/figures/burst_5s.svg) | ![](five_product/burst/results/figures/burst_15s.svg) | ![](five_product/burst/results/figures/burst_30s.svg) |

圖的橫軸是「距突發結束的秒數」，縱軸是「每秒送達 ÷ 基準」，灰色區域是突發期間；WCM 以粗藍線加星形標記表示。

| 產品 | 基準（筆／秒） | 恢復 5 s | 15 s | 30 s | 送達率 | OOM |
|---|---:|---:|---:|---:|---:|---|
| **WCM** | 13,281 | 7.1 s | 9.3 s | 12.3 s | 100% | 無 |
| Fluent Bit (Lua) | 34,277 | 1.8 s | 3.3 s | 3.1 s | 100% | 無 |
| Fluent Bit (WASM) | 2,734 | 3.3 s | 2.3 s | 8.2 s | 100% | 無 |
| Fluentd | 23,242 | 10.2 s | 3.1 s | 3.3 s | 100% | 無 |
| Vector | 7,250 | 12.2 s | 14.3 s | 5.3 s | 100% | 無 |

**解讀**
- **五個產品都 100% 送達，也沒有 OOM**：WCM 的有界 channel 與 TCP 回壓會在過載時讓上游放慢，不會丟資料，也不會讓記憶體無限成長。
- **WCM 的恢復時間隨突發長度增加**（7 → 9 → 12 s）。突發期間積下的資料要靠「容量 − 基準」這段餘裕慢慢消化，WCM 的基準是容量的 50%，所以恢復時間大致與積壓量成正比。
- 其他產品的恢復時間沒有一致的趨勢，例如 Fluentd 5 s 突發要 10.2 s，30 s 突發卻只要 3.3 s。因為每個條件只跑 1 次，**這張表只適合看「有沒有丟資料、會不會 OOM」**，不適合用來排名恢復速度。

## 4. 自訂邏輯故障（fault）

**條件**：
- 每秒 2,000 筆，持續 60 s。
- 第 15 s 送出一筆帶標記的日誌，filter 處理到這一筆時故障。
- WCM 的呼叫時限 T = 2 s；其他產品沒有對應機制，使用預設行為。

| 標記 | 故障 |
|---|---|
| `FAULT_LOOP` | 無窮迴圈 |
| `FAULT_PANIC` | 例外／panic |
| `FAULT_OOB` | 陣列越界或對 nil 取值 |
| `FAULT_MEM` | 持續配置記憶體 |

| 無窮迴圈 | 記憶體爆量 |
|---|---|
| ![](five_product/fault/results/figures/fault_loop.svg) | ![](five_product/fault/results/figures/fault_mem.svg) |
| **panic** | **越界** |
| ![](five_product/fault/results/figures/fault_panic.svg) | ![](five_product/fault/results/figures/fault_oob.svg) |

圖中各產品是「累計收到的日誌筆數」，紅色虛線是注入故障的時刻，灰色點線是應收的累計量（60,000 筆）。WCM 以粗藍線加星形標記表示。故障後曲線**持續上升**表示產品仍在送出日誌，曲線**變成水平**表示從此沒有再收到任何日誌。停止送出的產品曲線常會重疊成同一條水平線，所以圖中另外標註了是哪些產品。

- 無窮迴圈：WCM 在注入後約 7 s 呈水平（等待時限與二分隔離），之後恢復上升，並沿著應收量一直到 59,999 筆。其他四個產品停在約 15,000 筆，之後再也沒有增加。
- 記憶體爆量：WCM 幾乎沒有停頓，其他四個產品都停在 15,000 筆。三個 OOM 的產品，水平線在容器終止處結束。

| 產品 | LOOP | PANIC | OOB | MEM |
|---|---|---|---|---|
| **WCM** | **ok**（停 7 s，損失 1 筆） | **ok**（損失 1 筆） | **ok**（損失 1 筆） | **ok**（停約 1 s，損失 1 筆） |
| Fluent Bit (Lua) | stalled（不再送出） | 多出 15 筆 | 多出 15 筆 | OOM 被終止 |
| Fluent Bit (WASM) | stalled | ok | ok | OOM 被終止 |
| Fluentd | stalled | ok | ok | stalled |
| Vector (VRL) | stalled | ok（0 遺失，該筆原樣送出） | ok（0 遺失，該筆原樣送出） | OOM 被終止 |

**解讀**
- **只有 WCM 四種故障都能恢復**，而且每次只損失觸發故障的那 1 筆。
- **LOOP**：其他四個產品的程序都還活著，但從此不再送出任何資料（stalled），從注入到測試結束約 74 s 都是零輸出。WCM 靠呼叫時限（P3）中斷卡住的呼叫，再靠二分隔離（P4）找出那一筆，約 7 s 後恢復。
- **MEM**：三個產品整個程序 OOM 被終止，Fluentd 則停住。WCM 的每個插件 instance 都有記憶體上限，超過時只有該次呼叫失敗，instance 換新後繼續處理。
- **PANIC／OOB**：多數產品可以處理。Fluent Bit Lua 多送出 15 筆，原因尚未調查。Vector 的 remap 預設 `drop_on_error: false`，出錯的那一筆會原樣往下游送，所以沒有遺失，但送出的是未經處理的原始內容。
- **Vector 的故障以 VRL 撰寫**：VRL 沒有 `while`／`loop` 與遞迴，無法寫出真正的無窮迴圈。LOOP 改以巢狀 `for_each` 產生約 2.8×10^14 次迭代的計算，結果與其他產品相同：停在 15,000 筆，之後不再送出。MEM 以字串連續倍增，同樣 OOM 被終止。換言之，VRL「保證終止」的語言設計，擋不住實際上不會結束的計算與記憶體爆量。先前以 Lua transform 注入的結果是 LOOP stalled、PANIC／OOB 各遺失 1 筆、MEM OOM，與 VRL 版本只差在 PANIC／OOB 的遺失筆數；舊資料保留在 `five_product/fault/results_vector_lua/`。

---

# 第二部分：Pattern 實驗（WCM）

## P1. Amortized Boundary Crossing：批次攤提邊界成本

**問題**：每次 host↔WASM 呼叫都有固定成本，一次處理多少資料才能把成本攤掉？上限應該用「筆數」還是「位元組」來設？

**條件**：
- 以每秒 200,000 筆送出（遠高於容量），由 TCP 回壓限速，量飽和處理量。
- 筆數上限 k = 1…1024，位元組上限 1…512 KiB。
- 三種日誌大小：256 B、1 KiB、2 KiB。
- 3 次重複。

| 筆數上限 | 位元組上限 |
|---|---|
| ![](ours/p1_amortized_boundary/results/figures/p1_count_cap.svg) | ![](ours/p1_amortized_boundary/results/figures/p1_byte_cap.svg) |

![每次呼叫的 KiB 對處理量](ours/p1_amortized_boundary/results/figures/p1_batch_bytes.svg)

處理量（筆／秒，3 次的中位數）：

| 日誌大小 | k = 1（每筆一次呼叫） | 位元組上限 128 KiB（預設） | 512 KiB | 攤提帶來的倍數 |
|---|---:|---:|---:|---:|
| 256 B | 12,014 | 80,385 | 83,967 | **約 7×** |
| 1 KiB | 5,762 | 21,120 | 22,527 | **約 3.9×** |
| 2 KiB | 3,989 | 10,750 | 10,755 | **約 2.7×** |

**解讀**
- **攤提的效果明顯**：每筆呼叫一次時，處理量只有批次處理的 1/3 到 1/7。日誌越小，每筆分到的固定成本占比越高，攤提的效果就越大。
- **決定效果的是每次呼叫的位元組數，不是筆數**：在第三張圖（橫軸為每次呼叫實際的 KiB）上，三種日誌大小的曲線大致重合，都在每次呼叫約 64–128 KiB 時飽和。所以用筆數當上限不恰當：同樣 k = 64，256 B 的日誌每次只有 16 KiB、還沒飽和，2 KiB 的日誌則已經 128 KiB。這支持 core 預設**以位元組為上限（128 KiB）**。
- 128 KiB → 512 KiB 只再多 0–7%，記憶體卻增加：1 KiB 日誌從 84 增加到 116 MiB。128 KiB 是處理量與記憶體之間合理的折衷。
- P1 的 1 KiB、128 KiB 是 21,120 筆／秒，比第 1 節的 26,562 低約 20%。量法不同：P1 以遠超過容量的速率送出，讓 TCP 回壓限速；第 1 節是逐步提高固定速率，直到判定失敗。兩者不宜直接比較。

## P2. Streaming Micro-Batch：資料到達就處理，只有輸出端等 W

**問題**：如果資料要等整個時間窗湊齊才處理，延遲會是「W + 處理一整批的時間」。如果資料一到就以小批次處理，只有輸出端等 W，那麼超額延遲應該和 W 無關。

**條件**：與第 2 節相同（資料共用）。

![P2：p99 與超額延遲對 W](ours/p2_streaming_micro_batch/results/figures/p2_latency_vs_window.svg)

左圖是 p99 對 W，虛線為理想值 0.99·W；右圖是超額延遲。WCM 以粗線加星形標記表示，其他產品是細線加圓點。

| W (s) | 0.01 | 0.1 | 0.5 | 1 | 2 | 5 |
|---|---:|---:|---:|---:|---:|---:|
| WCM p99 (ms) | 38.7 | 126.3 | 524.0 | 1,017.6 | 2,017.8 | 5,003.8 |
| WCM 超額延遲 (ms) | 28.8 | 27.3 | 29.0 | 27.6 | 37.8 | 53.8 |
| WCM CPU | 25% | 23% | 23% | 23% | 29% | 29% |
| WCM 記憶體 | 23 MiB | 17 MiB | 19 MiB | 24 MiB | 31 MiB | 63 MiB |

**解讀**
- **W 從 0.01 s 到 1 s（放大 100 倍），超額延遲都維持在 27–29 ms**，p99 幾乎就是 W 加上一個固定值。這正是 streaming micro-batch 的預期。
- W = 2 s 和 5 s 時，超額延遲小幅增加到 38 ms 和 54 ms。這時一批是 4,000 到 10,000 筆，最後一段 format 與 HTTP 傳送的時間隨批次變長，但增幅仍遠小於 Vector（418 ms）與 Fluentd（945 ms）。
- 對照先前的實驗（舊 `test/compare/pattern_suite`，**本次未重跑**）：W = 1 s 時，整批處理的超額延遲是 292 ms，streaming 是 29 ms。

## P3. Watchdog and Respawn：呼叫時限與 instance 換新

**問題**：插件卡死或 trap 時，系統能不能在有限時間內恢復？時限本身要付出多少 CPU？

**條件**：
- parse 改用故障插件。每秒 2,000 筆，持續 60 s，第 15 s 注入故障，3 次重複。
- 時限成本：一般插件、每秒 10,000 筆，watchdog 開／關各 3 次，交錯執行。

![P3 時間軸](ours/p3_watchdog_respawn/results/figures/p3_timeline.svg)

| 恢復時間對時限 T | watchdog 的 CPU 成本 |
|---|---|
| ![](ours/p3_watchdog_respawn/results/figures/p3_recovery_vs_timeout.svg) | ![](ours/p3_watchdog_respawn/results/figures/p3_overhead.svg) |

| 情境 | 結果 | 最長零輸出 | 恢復時間 | 遺失 |
|---|---|---:|---:|---:|
| 對照（無故障） | ok | 0 s | — | 0 |
| LOOP，T = 1 s | ok | 4 s | 5.2 s | 1 |
| LOOP，T = 2 s | ok | 8 s | 9.1 s | 1 |
| LOOP，T = 5 s | ok | 20 s | 21.2 s | 1 |
| **LOOP，watchdog 關閉** | **stalled** | 到測試結束 | 不會恢復 | **28,480** |
| PANIC | ok | 0 s | 0 s | 1 |
| OOB | ok | 0 s | 0 s | 1 |
| MEM | ok | 約 1 s | 2.1 s | 1 |

恢復時間為 3 次的中位數；結果（outcome）、最長零輸出與遺失筆數在 3 次中都相同。

| watchdog | CPU 中位數 |
|---|---:|
| 關 | 51.5% |
| 開 | 57.2%（**+11%**） |

**解讀**
- **消融結果很清楚**：watchdog 關閉時，一筆無窮迴圈就讓整條 pipeline 永久停住，損失 28,480 筆，也就是注入之後的全部資料。開啟後每次都恢復，而且只損失那 1 筆。
- **停頓時間 ≈ 4·T**，與 T 成正比。原因是二分隔離（P4）的過程中，每一次含有這筆壞資料的呼叫都要等滿 T 才會被中斷；以這個批次大小，大約要中斷 4 次才能把它單獨隔離出來。所以 T 應該設成「正常呼叫時間的上限」，不要設得太寬鬆。
- **trap 類故障（PANIC、OOB）不需要等時限**，instance 換新後立刻恢復。MEM 會碰到 instance 的記憶體上限，約 2 s 恢復。
- **成本**：epoch interruption 讓 CPU 增加約 11%，約 6 個百分點。watchdog 開啟的第 3 次判定為 FAIL，但唯一的原因是 backlog，屬於 WSL 時鐘造成的高估；送達筆數完整，CPU 數據仍然有效。

## P4. Poison Record Bisection：壞資料的二分隔離

**問題**：插件對整批回傳錯誤或 trap 時，如果直接把整批丟進 dead-letter，正常資料會跟著被丟掉。用二分法找出壞的那一筆，能不能救回其他資料？成本是多少？

**條件**：
- 每秒 5,000 筆、1 KiB，持續 40 s，3 次重複。
- 每批最多 128 KiB（約 128 筆）。
- 壞資料是截斷的 JSON，以 0／0.1／1／5% 的比例隨機抽樣。

| 正常資料送達率 | dead-letter 筆數 |
|---|---|
| ![](ours/p4_poison_bisection/results/figures/p4_delivered.svg) | ![](ours/p4_poison_bisection/results/figures/p4_dead_letter.svg) |

| 壞資料比例 | 二分開：正常資料送達 | 二分關：正常資料送達 | 二分關：被連帶丟棄的正常資料 | 每筆壞資料的隔離呼叫次數 |
|---|---:|---:|---:|---:|
| 0% | 100% | 100% | 0 | — |
| 0.1% | **100%** | 88.6% | 22,860 | 13.7 |
| 1% | **100%** | 29.4% | 140,479 | 12.0 |
| 5% | **100%** | **0.27%** | 189,577 | 8.8 |
| 0.1%（panic） | **100%** | 88.6% | 22,839 | 13.6 |

在 filter、format、transport 階段各注入一筆 panic：都只有 1 筆進 dead-letter，其餘 100% 送達。每筆的隔離呼叫次數分別是 14、12、16 次。

**解讀**
- **效果非常大**：不做二分時，只要 1% 的壞資料，正常資料就只剩 29% 送得到；5% 時幾乎全滅，剩 0.27%。開啟二分後，各比例下正常資料都是 **100% 送達**，dead-letter 中只有真正的壞資料。
- **成本**：每筆壞資料約 9–14 次額外呼叫，約為 log₂(批次筆數) 的量級。壞資料比例越高，同一批內常有好幾筆壞資料，可以共用二分的步驟，所以每筆的成本反而下降。CPU 從約 12% 增加到 13–18%。
- 二分關閉時，記憶體上升到 150–210 MiB，因為大量資料寫入 dead-letter。二分開啟時維持在約 15–20 MiB。
- 這個機制對四個階段都有效，與 P3 一起構成第 4 節「只損失 1 筆」的原因。

## P5. Capability-Scoped Stage：逐階段的能力授權

**問題**：每個階段的插件只能使用設定中明確授權的能力嗎？檔案、環境變數、網路都要測。

**條件**：
- A. 載入時檢查：每個插件 × 各種授權組合。
- B. 啟動時拒絕：使用未授權的故障插件啟動。
- C. 執行期探測：故障插件嘗試讀 `/etc/passwd`、讀環境變數、連網路。

P5 沒有圖，結果見 [ours/p5_capability_scoped/results/summary.md](ours/p5_capability_scoped/results/summary.md)。

**A. 載入時**

| 插件 | 無授權 | inert-fs | inert-fs + inert-sockets |
|---|---|---|---|
| 一般插件（4 個階段） | LOAD_OK | LOAD_OK | LOAD_OK |
| 故障插件（4 個階段） | **LOAD_REFUSED**（9 個未授權 import） | **LOAD_REFUSED**（7 個未授權 import） | LOAD_OK |

總計：一般插件 13 個組合全部 LOAD_OK；故障插件 8 個組合未授權時拒絕，5 個組合授權後 OK。

**B. 啟動時**：設定中沒有授權卻使用故障插件時，程序在接收任何資料前就結束，exit code 1，並列出每個未授權的 import。

**C. 執行期**

| 探測 | parse | filter | format | transport |
|---|---|---|---|---|
| 讀 `/etc/passwd` | 拒絕 | 拒絕 | 拒絕 | 拒絕 |
| 讀環境變數 | 空 | 空 | 空 | 空 |
| TCP 到列出的 127.0.0.1:8080 | 拒絕 | 拒絕 | 拒絕 | **連線成功** |
| TCP 到未列出的 127.0.0.1:8081 | — | 拒絕 | 拒絕 | 拒絕 |
| HTTP 到 `localhost:8080`（名稱不同） | — | — | — | 拒絕 |
| decoy（8081）收到的連線數 | 0 | 0 | 0 | 0 |

**解讀**
- 能力限制在**三個時間點**都生效：載入時檢查 import、啟動時拒絕執行、執行期限制實際存取。
- 授權是**逐階段**的：只有 transport 能連網，而且只能連到 endpoint 列出的那一個位址。同一個 socket 換成 `localhost` 的寫法也會被拒絕，因為比對的是設定中的字面值。
- 所有存取都沒有洩漏：decoy 連接埠在整個實驗中沒有收到任何連線。
- 不需要額外能力的一般插件，不給任何授權也能直接載入，所以這個機制不會增加正常使用時的設定負擔。

---

# 總結

## Pattern 的效果

| Pattern | 對照 | 效果 | 代價 |
|---|---|---|---|
| **P1** Amortized Boundary Crossing | 每筆呼叫 vs 批次 | 處理量 **2.7–7 倍**；效果取決於每次呼叫的位元組數，在 64–128 KiB 時飽和 | 批次越大，記憶體越高（128 KiB 時約 84 MiB） |
| **P2** Streaming Micro-Batch | W = 0.01–5 s | W ≤ 1 s 時超額延遲固定在 **27–29 ms**，與 W 無關；W = 5 s 時 54 ms | W 大時最後一段傳送隨批次略增 |
| **P3** Watchdog and Respawn | watchdog 開／關 | 無窮迴圈：從**永久停住、損失 28,480 筆**變成**約 4·T 後恢復、只損失 1 筆**；trap 類故障立即恢復 | CPU **+11%**；停頓時間與 T 成正比 |
| **P4** Poison Record Bisection | 二分開／關 | 5% 壞資料時，正常資料從 **0.27%** 提升到 **100%** 送達 | 每筆壞資料 9–14 次額外呼叫 |
| **P5** Capability-Scoped Stage | 授權有／無 | 未授權的插件在載入時被拒；執行期只有 transport 能連到列出的 endpoint；**沒有任何洩漏** | 一般插件不需設定 |

## 本系統（WCM）的表現

- **效能介於原生腳本與其他 WASM 方案之間**：
  - 吞吐量 26,562 筆／秒，是 Fluent Bit WASM 的 4.9 倍、Vector 的 1.8 倍，但低於 Fluent Bit Lua 與 Fluentd。
  - 差距來自每個階段都在 WASM 沙箱中執行的邊界成本；P1 的批次攤提已經把這個成本壓低了 3–7 倍。
- **延遲穩定**：在各種 W 下都在數十 ms，W = 5 s 時 54 ms，與 Fluent Bit 同級，遠低於 Vector 與 Fluentd。
- **過載時不丟資料**：突發期間 100% 送達、沒有 OOM，但恢復時間比 Fluent Bit 長（7–12 s），因為吞吐量的餘裕較小。
- **故障隔離是最主要的優勢**：五個產品中，只有 WCM 在四種故障下都能恢復，每次只損失 1 筆。其他產品遇到無窮迴圈全部停住，三個產品遇到記憶體爆量會整個程序 OOM。這主要是 P3 + P4 的效果，P5 再進一步限制插件能存取的範圍。

一句話歸納：**WCM 以約 11% 的 CPU（watchdog）與 WASM 邊界成本，換得自訂邏輯出錯時「只損失 1 筆並自動恢復」，以及逐階段的最小權限；P1 與 P2 則把吞吐量與延遲維持在實用的範圍。**

## 量測限制

- **WSL2 時鐘**：延遲已以 `clock_drift.csv` 校正，吞吐量在時鐘穩定時重跑過，但仍可能殘留數 ms 的誤差。正式發表建議在實體 Linux 機器上重跑。
- **重複次數**：latency 與 P1–P4 各 3 次；burst 與 fault 各 1 次，所以 burst 的恢復時間不宜用來排名。
- **Fluent Bit WASM** 在延遲實驗用每秒 4,000 筆，與其他產品不完全可比。
- **延遲的判定**：W ≥ 2 s 時負載結束當下本來就有 W 秒份在等 flush，再加上時鐘倒退，backlog 會被高估。延遲分析把「只因 backlog 而 FAIL」的點也納入，共 90 個點全部可用。
- **原因未經 profiling**：Vector 與 Fluentd 在大 W 時延遲上升的原因、Fluent Bit Lua 在 PANIC／OOB 多出 15 筆的原因，都只是根據設定與曲線推論，沒有用 profiling 驗證。
- **量法不同**：P1 與第 1 節的吞吐量量法不同，數字不宜直接比較（21,120 vs 26,562）。
