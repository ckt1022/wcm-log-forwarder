# 實驗

WCM log forwarder 的實驗程式碼、設定與結果。所有產品都在 Docker 容器中以相同的資源限制執行，一個腳本跑完一種實驗的全部產品，並產生結果表與圖。

```
experiment/
├── common/                 各實驗共用
│   ├── products.env        產品映像版本與量測條件
│   ├── setup.sh            準備工具、映像與分析環境
│   ├── tools/              負載產生器 gen、HTTP 接收端 httpsink（Go 原始碼）
│   ├── lib/                run_point.sh（跑一個量測點）、判定、延遲校正、資源取樣、故障負載
│   └── wcm/Dockerfile      由 core/ 建置 WCM 映像
├── five_product/           五產品比較，一種實驗一個目錄
│   ├── throughput/         最大可持續吞吐量
│   ├── latency/            不同 flush 時間窗下的延遲
│   ├── burst/              突發過載後的恢復
│   └── fault/              自訂邏輯出錯（無窮迴圈、例外、越界、記憶體）
└── ours/                   論文的實驗，依 pattern 分類（只有 WCM）
    ├── p1_amortized_boundary/      批次上限（筆數 vs 位元組）對處理量
    ├── p2_streaming_micro_batch/   flush 時間窗對延遲
    ├── p3_watchdog_respawn/        呼叫時限、卡死／trap 後的恢復、時限的成本
    ├── p4_poison_bisection/        壞資料的二分隔離開／關
    └── p5_capability_scoped/       各階段的能力授權（載入時、執行期）
```

每個實驗目錄的結構相同：

```
<實驗>/
├── run.sh          一個腳本跑完五個產品，最後呼叫 analyze.py
├── analyze.py      讀取 results/data，產生 summary.csv 與圖
├── products/       五個產品各自的設定與 filter 程式碼
│   ├── wcm/  fluentbit-lua/  fluentbit-wasm/  fluentd/  vector/
└── results/
    ├── data/<產品>/<量測點>/   每個量測點的原始資料
    ├── summary.csv             結果表
    └── figures/                圖
```

## 環境需求

| 項目 | 版本 | 用途 |
|---|---|---|
| Linux x86_64 | 已在 Ubuntu 24.04 驗證；正式量測建議使用實體機 | |
| Docker | 已在 29.8.0 驗證；需要 cgroup v2 | 執行產品容器、建置工具與映像 |
| Python | 3.x，含 venv | 判定、分析與畫圖 |
| bash、awk、curl、timeout | 系統套件 | 腳本 |
| 網路 | 第一次 setup 需要 | 下載映像與套件 |

執行實驗的使用者要能使用 Docker（`sudo usermod -aG docker $USER` 後重新登入）。

## 準備

```bash
experiment/common/setup.sh
```

| 步驟 | 內容 |
|---|---|
| 1 | 以 `golang:1.27.3-bookworm` 編譯 `common/bin/gen` 與 `common/bin/httpsink` |
| 2 | 下載 Fluent Bit、Fluentd、Vector 的映像 |
| 3 | 由 `core/` 建置 WCM 映像（內含 `core/plugin` 的 Rust 插件） |
| 4 | 以 `rust:1.93.1-slim-bookworm` 編譯各實驗的 Fluent Bit WASM filter |
| 5 | 把實際使用的映像 digest 記錄到 `common/images.lock` |
| 6 | 建立 `common/venv`（numpy、matplotlib，版本見 `common/requirements.txt`） |

修改 `core/` 後要重新執行 setup，才會更新 WCM 映像。

## 執行

```bash
cd experiment/five_product/throughput
QUICK=1 ./run.sh        # 短負載，確認流程（約 15 分鐘）
./run.sh                # 正式量測
./run.sh wcm vector     # 只跑指定的產品
```

各實驗的內容、時間與參數見 [five_product/README.md](five_product/README.md) 與 [ours/README.md](ours/README.md)。

## 注意事項

- 量測期間不要執行其他耗資源的程式；正式量測建議在實體機上進行。
- WSL2 的系統時鐘會週期性跳動，延遲以 `common/lib/fix_latency.py` 依 `clock_drift.csv` 校正。實體機上校正量為 0。
- 預設使用 15140（產品接收）與 18080（HTTP 接收端）連接埠，可在 `common/products.env` 修改或以同名環境變數覆寫。P5 另外使用 8080 與 8081。
- 移植到新的 repo 時，`core/`、`plugins/`、`experiment/` 必須是同一層的目錄（setup.sh 以 `../core` 建置 WCM 映像）。所有腳本只使用相對路徑。
- 以 snap 安裝的 Docker 看不到 `/tmp` 底下的檔案，`experiment/` 必須放在家目錄等一般路徑下。
