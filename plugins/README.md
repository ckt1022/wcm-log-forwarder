# 插件

forwarder 四個階段（Parse、Filter、Format、Transport）的插件原始碼、工具鏈與建置流程。四種語言都實作同樣的四個插件，可以互相替換。

| 語言 | 說明 | 需要的授權 |
|---|---|---|
| Rust | [rust/README.md](rust/README.md) | 不需要 |
| C | [c/README.md](c/README.md) | 不需要 |
| C# | [csharp/README.md](csharp/README.md) | `filesystem: inert`（.NET 執行環境的連帶 import） |
| Go | [go/README.md](go/README.md) | `filesystem: inert`（TinyGo 執行環境的連帶 import） |

## 結構

```
plugins/
├── Makefile             統一的建置入口
├── toolchain.env        工具的固定版本、下載網址與校驗值
├── scripts/             下載並安裝各語言工具鏈到 .toolchain/
├── verify/verify.py     用 core 的 forwarder 實際驗證插件
├── rust/  c/  csharp/  go/
├── .toolchain/          下載的工具鏈（不進 git）
├── build/               產生的綁定等中間檔（不進 git）
└── out/<語言>/          編譯產物（不進 git）
```

所有工具都安裝在 `.toolchain/` 內，不會修改系統環境或使用者目錄。

## 工具與版本

### 產生綁定與包成 component

| 語言 | 產生綁定 | 包成 component |
|---|---|---|
| Rust | `wit-bindgen` crate 0.58.0（巨集） | rustc 內建的 `wasm-component-ld` |
| C | `wit-bindgen-cli` 0.58.0 | wasi-sdk 29.0 內建的 `wasm-component-ld` |
| C# | componentize-dotnet 內建的 `wit-bindgen` 0.58.0 | componentize-dotnet 內建的 `wasm-tools` 1.251.0 |
| Go | `wit-bindgen-go` v0.7.0 | `wasm-tools` 1.251.0（由 TinyGo 呼叫） |

Rust、C、C# 使用同一版 wit-bindgen，C# 與 Go 使用同一版 wasm-tools；它們內部的 WIT 解析器（`wit-parser` 0.251.0）也相同。Go 沒有官方的 wit-bindgen 後端，使用 Bytecode Alliance 另外維護的 `wit-bindgen-go`。

### 全部工具與套件

| 語言 | 項目 | 版本 | 用途 |
|---|---|---|---|
| Rust | Rust、target `wasm32-wasip2` | 1.93.1 | 編譯器，直接輸出 component |
| | `wit-bindgen` | 0.58.0 | 從 WIT 產生 Rust 綁定 |
| | `serde_json` | 1.0.154 | Parse 解析 JSON |
| C | wasi-sdk | 29.0（clang 21.1.4） | 編譯器、wasi-libc、輸出 component 的連結器 |
| | `wit-bindgen-cli` | 0.58.0 | 從 WIT 產生 C 綁定 |
| | cJSON | 1.7.19 | Parse 解析 JSON |
| C# | .NET SDK | 10.0.114 | 編譯器 |
| | componentize-dotnet（`BytecodeAlliance.Componentize.DotNet.Wasm.SDK`、`…WitBindgen`） | 0.8.0-preview00011 | 產生綁定並輸出 component |
| | `Microsoft.DotNet.ILCompiler.LLVM`（含 linux-x64、wasi-wasm runtime 套件） | 10.0.0-rc.1.26306.1 | NativeAOT：把 C# 編譯成 WebAssembly |
| | `Microsoft.NET.ILLink.Tasks` | 10.0.14 | 裁剪未使用的程式碼 |
| | wasi-sdk | 29.0 | NativeAOT-LLVM 連結時使用 |
| Go | Go | 1.27.3 | TinyGo 使用的標準函式庫與工具 |
| | TinyGo | 0.43.0（LLVM 22.1.4） | 把 Go 編譯成 WebAssembly |
| | `wit-bindgen-go` | v0.7.0 | 從 WIT 產生 Go 綁定 |
| | `go.bytecodealliance.org/cm` | v0.3.0 | 綁定使用的 Component Model 型別 |
| | wasm-tools | 1.251.0 | TinyGo 用它把 core module 包成 component |

版本記錄的位置：

| 語言 | 檔案 |
|---|---|
| Rust | `rust/Cargo.toml`、`rust/Cargo.lock` |
| C、C#、Go 的工具鏈 | `toolchain.env`（含下載網址與 SHA-256／SHA-512） |
| C# 套件 | `csharp/global.json`、`csharp/Directory.Build.props`、各專案的 `packages.lock.json` |
| Go 模組 | `go/go.mod`、`go/go.sum` |

## 介面

介面定義只有一份：`../core/wit/log_plugin.wit`。host 與所有插件共用，`plugins/` 必須和 `core/` 放在同一層。每個階段是一個 WIT world：

| 階段 | world | 匯出的函式 |
|---|---|---|
| Parse | `parser-plugin` | `parse(list<string>) -> result<list<parsed-entry>, parse-error>` |
| Filter | `reduction-plugin` | `filter(list<log-entry>) -> result<list<filter-result>, plugin-error>` |
| Format | `format-plugin` | `format(list<log-entry>) -> result<list<u8>, plugin-error>` |
| Transport | `transport-plugin` | `init(transport-config) -> result<_, plugin-error>`、`transport(list<list<u8>>) -> result<_, plugin-error>` |

每個 world 另外匯出 `report-usage() -> u64`（回傳最近一次呼叫的耗時，host 目前不使用）。

所有語言的插件都要遵守以下規則，驗證會逐項檢查：

| 階段 | 規則 |
|---|---|
| Parse | 一行 JSON 對應一筆：`ts`→timestamp、`level`→level、`msg`→message，其餘欄位與 `att` 內的欄位攤平成 tags，第一個 tag 是 `lang=<語言>`，`targettag` 為 `"A"`。任何一行無法解析時，整批回傳 `invalid-format`，host 會用二分法找出那一行。 |
| Filter | 每筆輸入都要回傳一個 `filter-result`（id 相同），`keep = level >= warn`。 |
| Format | 每筆輸入輸出一個 frame：`[4 位元組 little-endian 長度][資料]`。資料是 json-flat：`{"id":..,"timestamp":..,"level":..,"message":..,<tag>:<value>...}`。 |
| Transport | `init` 收到端點等設定；`transport` 把每行加上換行後以 HTTP POST 送出。非 2xx 時回傳 `processing-failed("server returned HTTP <status>")`，host 依狀態碼決定是否重試。重試由 host 負責。 |

插件預設沒有任何檔案、網路與環境變數的權限；Transport 只能透過 `wasi:http` 連到設定的端點。若插件因為語言的執行環境而連帶 import 了檔案介面，該語言的 `capabilities.yaml` 會列出需要的 `inert` 授權（只連結介面，不給任何資源）。

插件不需要自行處理記憶體回收：重用中的 instance 記憶體超過 `instance_recycle_percent`（預設為上限的 50%）時，host 會換成新的 instance。

## 共同需求

| 項目 | 版本 | 用途 |
|---|---|---|
| Linux x86_64 | 已在 Ubuntu 24.04（glibc 2.39）驗證 | |
| make、curl、tar、sha256sum、sha512sum | 系統套件 | 建置與下載 |
| Python | 3.x | 執行驗證 |
| Rust | 1.93.1 | Rust 插件；C 與 Go 也用它安裝 wit-bindgen、wasm-tools |

```bash
sudo apt-get install -y build-essential curl python3
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install 1.93.1
rustup default 1.93.1
rustup target add wasm32-wasip2
```

## 指令

| 指令 | 說明 | 從零所需時間 |
|---|---|---|
| `make rust` | 編譯 Rust 插件到 `out/rust`，故障測試插件到 `out/fault` | 約 1 分鐘 |
| `make install-rust` | 把 Rust 插件更新到 `../core/plugin` | |
| `make setup-c`、`make c` | 安裝 C 工具鏈、編譯 C 插件 | 約 7 分鐘 |
| `make setup-csharp`、`make csharp` | 安裝 .NET SDK 與套件、編譯 C# 插件 | 約 7 分鐘 |
| `make setup-go`、`make go` | 安裝 Go 工具鏈、編譯 Go 插件 | 約 8 分鐘 |
| `make verify-<語言>` | 用 forwarder 實際驗證 | 各約 15 秒（Rust 約 1 分鐘） |
| `make clean` | 刪除 `out/`、`build/`、產生的綁定與 Rust 的編譯快取（不刪工具鏈） | |

`setup-*` 可重複執行：已安裝且版本正確的項目會略過；下載的檔案校驗失敗時會刪除並停止。

## 驗證

```bash
(cd ../core && cargo build --release)   # 驗證需要 forwarder 執行檔
make verify-<語言>
```

| 項目 | 內容 |
|---|---|
| 載入 | 四個插件能以預設權限（或 `capabilities.yaml` 的授權）載入，未授權時被拒絕 |
| 內容 | 送達的欄位正確，filter 只保留 `warn` 以上 |
| 正常流量 | instance 重用與每批丟棄兩種模式，每筆都送達 |
| 長時間重用 | 記憶體上限 64 MB、輸入約 60 MB，全部送達且不得出現任何 trap |
| 格式錯誤 | 只有格式錯誤的那幾行進 `error.txt` |
| 故障注入（Rust） | 四個階段分別注入 5 種故障，只有造成錯誤的那筆進 `error.txt` |

驗證使用系統分配的空閒連接埠，不會占用 5140 或 8080。
