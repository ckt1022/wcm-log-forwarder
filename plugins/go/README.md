# Go 插件

使用 [TinyGo](https://tinygo.org/) 把 Go 編譯成 WebAssembly component，綁定由 Bytecode Alliance 的 [wit-bindgen-go](https://github.com/bytecodealliance/go-modules) 產生。標準的 Go 編譯器無法輸出 component，所以需要 TinyGo。

## 環境需求

| 項目 | 版本 | 用途 |
|---|---|---|
| Linux x86_64 | 已在 Ubuntu 24.04（glibc 2.39）驗證 | |
| make、curl、tar、sha256sum | 系統套件 | 建置與下載 |
| Rust（cargo） | 1.93.1 | 只用來安裝 wasm-tools |

不需要事先安裝 Go 或 TinyGo，`make setup-go` 會下載固定版本到 `plugins/.toolchain/`，不影響系統環境。

## 工具與套件版本

| 項目 | 版本 | 來源 | 用途 |
|---|---|---|---|
| Go | 1.27.3 | [go.dev](https://go.dev/dl/go1.27.3.linux-amd64.tar.gz)，以 SHA-256 校驗 | TinyGo 使用的 Go 標準函式庫與工具 |
| TinyGo | 0.43.0（LLVM 22.1.4，內含 wasm-opt） | [GitHub release](https://github.com/tinygo-org/tinygo/releases/tag/v0.43.0)，以 SHA-256 校驗 | 把 Go 編譯成 WebAssembly（支援 Go 1.25–1.27） |
| wasm-tools | 1.251.0 | crates.io，`cargo install --locked` | TinyGo 從 PATH 呼叫它把 core module 包成 component；與 C# 使用同一版 |
| wit-bindgen-go | v0.7.0 | `go install`，以 sum.golang.org 校驗 | 從 WIT 產生 Go 綁定 |
| `go.bytecodealliance.org/cm` | v0.3.0 | Go module proxy，版本與雜湊值記錄在 `go.sum` | 綁定使用的 Component Model 型別（`cm.List`、`cm.Result` 等） |

## 從零編譯

```bash
cd plugins

# 1. 下載並安裝 Go、TinyGo、wasm-tools、wit-bindgen-go 與 Go 模組（約 7 分鐘）
make setup-go

# 2. 產生綁定並編譯四個插件（約 1 分鐘）
make go

# 3. 驗證（需要 forwarder 執行檔）
(cd ../core && cargo build --release)
make verify-go
```

產物在 `out/go/{parse,filter,format,transport}.wasm`。

### 編譯步驟

`make go` 做三件事：

```bash
. scripts/go-env.sh   # 使用 .toolchain 內的 Go、TinyGo 與模組快取

# 1. 組合 WIT：core 的 WIT 加上 go-worlds.wit
cp -r ../core/wit/. build/go/wit
cp go/go-worlds.wit build/go/wit/

# 2. 為四個 world 產生綁定到 go/internal（共用的型別會產生到同一個位置）
wit-bindgen-go generate --world reduction-plugin-go --out go/internal build/go/wit

# 3. TinyGo 編譯成 component
cd go
tinygo build -target=wasip2 -scheduler=none -opt=2 \
    --wit-package ../build/go/wit --wit-world reduction-plugin-go \
    -o ../out/go/filter.wasm ./filter
```

| 選項 | 作用 |
|---|---|
| `-scheduler=none` | 不啟用 goroutine 排程器，省去 asyncify 轉換；吞吐量約為預設的 1.6 倍 |
| `-opt=2` | 以速度優先（TinyGo 預設 `-opt=z` 以體積優先） |

### 為什麼需要 go-worlds.wit

TinyGo 的標準函式庫在 wasip2 下固定會 import `wasi:cli` 等 WASI 介面，world 必須宣告它們才能產生 component。`go-worlds.wit` 為每個階段定義一個 world，內容是 core 的 world 加上 `include wasi:cli/imports@0.2.0`，core 的 WIT 不需要修改。component 最後只會 import 實際用到的介面。

## 權限

TinyGo 的執行環境會 import `wasi:filesystem`（不會實際開檔），四個插件都需要 `filesystem: inert`，列在 `capabilities.yaml`。使用 Go 插件時要把它加進 `forwarder.yaml`：

```yaml
capabilities:
  parse: {filesystem: inert, sockets: deny}
  filter: {filesystem: inert, sockets: deny}
  format: {filesystem: inert, sockets: deny}
  transport: {filesystem: inert, sockets: deny}
```

## 原始碼結構

```
go/
├── go.mod  go.sum       模組 wcm/plugins
├── go-worlds.wit        Go 用的 world 定義
├── capabilities.yaml
├── parse/main.go
├── filter/main.go
├── format/main.go
├── transport/main.go
└── internal/            產生的綁定（不進 git）
```

## 開發一個插件

每個插件是一個 `main` 套件：在 `init()` 裡把實作指定給產生的 `Exports`，`main()` 留空。以 Filter 為例：

```go
package main

import (
	pipelineprocess "wcm/plugins/internal/local/log-process/pipeline-process"
	reductionplugin "wcm/plugins/internal/local/log-process/reduction-plugin"
	world "wcm/plugins/internal/local/log-process/reduction-plugin-go"

	"go.bytecodealliance.org/cm"
)

type filterResult = cm.Result[world.PluginErrorShape, cm.List[reductionplugin.FilterResult], reductionplugin.PluginError]

func init() {
	world.Exports.Filter = filter
	world.Exports.ReportUsage = func() uint64 { return 0 }
}

func filter(entries cm.List[reductionplugin.LogEntry]) filterResult {
	items := entries.Slice()
	results := make([]reductionplugin.FilterResult, len(items))
	for i, entry := range items {
		results[i] = reductionplugin.FilterResult{ID: entry.ID, Keep: entry.Level >= pipelineprocess.LogLevelWarn}
	}
	return cm.OK[filterResult](cm.ToList(results))
}

func main() {}
```

要引入三個產生的套件：

| 套件 | 內容 |
|---|---|
| `<階段>-plugin-go`（上例的 `world`） | `Exports` 與錯誤的 `...Shape` 型別 |
| `<階段>-plugin` | 該階段的型別別名（`LogEntry`、`FilterResult` 等） |
| `pipeline-process` | 共用型別、`LogLevel` 常數、錯誤建構函式 |

| WIT | Go |
|---|---|
| `string` | `string` |
| `list<T>` | `cm.List[T]`：`.Slice()` 取得切片，`cm.ToList(slice)` 建立 |
| `tuple<string, string>` | `[2]string` |
| `enum`（例如 log-level） | 常數，例如 `pipelineprocess.LogLevelWarn` |
| `result<T, E>` | `cm.Result`：成功用 `cm.OK[型別](值)`，失敗用 `cm.Err[型別](錯誤)` |

回傳錯誤：

```go
return cm.Err[parseResult](pipelineprocess.ParseErrorInvalidFormat("JSON parse error"))
```

注意事項：

- 照一般 Go 的寫法即可，不需要手動釋放記憶體。TinyGo 不會回收 host 傳入的參數，記憶體會隨批次累積；host 會在記憶體超過 `instance_recycle_percent`（預設 50%）時自動換新 instance，資料不受影響。
- Transport 透過產生的 `wasi:http` 綁定送出請求，流程見 `transport/main.go`：先呼叫 `Handle()` 再寫入 body。
- 不能使用 goroutine：建置時關閉了排程器（`-scheduler=none`）。插件每次呼叫都是同步處理，不需要它。
- `encoding/json` 可以使用，但 TinyGo 的反射很慢：同樣條件下 Go 的 Parse 每秒約 8,000 筆，Rust 約 37,000 筆。需要更高吞吐量時，Parse 改用其他語言即可（四種語言的插件可以混用）。
- 新增階段或修改 WIT 後，執行 `make go` 會重新產生綁定。
