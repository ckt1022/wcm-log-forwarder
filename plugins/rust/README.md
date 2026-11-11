# Rust 插件

## 環境需求

| 項目 | 版本 | 來源 |
|---|---|---|
| Rust | 1.93.1 | rustup |
| target | `wasm32-wasip2` | rustup |

`wasm32-wasip2` 會直接產生 component，不需要其他 WASM 工具。

## 套件版本

所有插件是同一個 cargo workspace，版本由 `Cargo.lock` 固定（共 41 個套件，建置時加 `--locked`）。直接使用的套件：

| 套件 | 版本 | 用途 |
|---|---|---|
| `wit-bindgen` | 0.58.0 | 從 WIT 產生綁定（巨集，編譯時執行）；與 C、C# 使用同一版 |
| `serde_json` | 1.0.154 | Parse 解析 JSON |

套件在第一次建置時由 cargo 從 crates.io 下載。

## 從零編譯

```bash
# 1. 安裝 Rust 與 target
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install 1.93.1
rustup default 1.93.1
rustup target add wasm32-wasip2

# 2. 編譯
cd plugins
make rust

# 3. 驗證（需要 forwarder 執行檔）
(cd ../core && cargo build --release)
make verify-rust
```

產物：

| 路徑 | 內容 |
|---|---|
| `out/rust/{parse,filter,format,transport}.wasm` | 一般插件 |
| `out/fault/{parse,filter,format,transport}.wasm` | 故障測試插件 |

相同的原始碼與 `Cargo.lock` 編出的檔案逐位元相同。`make install-rust` 會把它們複製到 `../core/plugin/`。

## 原始碼結構

```
rust/
├── Cargo.toml          workspace 與 release 設定（opt-level = "s"、strip）
├── Cargo.lock
├── parse/  filter/  format/  transport/
│   ├── Cargo.toml      crate-type = ["cdylib"]
│   └── src/lib.rs
└── fault/
    ├── probe.rs        故障注入與能力探測（四個故障插件共用）
    └── parse/  filter/  format/  transport/
```

## 開發一個插件

以 Filter 為例，`src/lib.rs` 由三部分組成：

```rust
// 1. 依 WIT 產生綁定：path 指向 core 的 WIT，world 決定要實作哪個階段
wit_bindgen::generate!({
    path: "../../../core/wit",
    world: "reduction-plugin",
    generate_all,
});

// 產生的型別放在 WIT 套件與介面對應的模組下
use crate::local::log_process::pipeline_process::LogLevel;

// 2. 實作產生的 Guest trait：函式名稱與型別對應 WIT 的 export
struct Component;

impl Guest for Component {
    fn filter(entries: Vec<LogEntry>) -> Result<Vec<FilterResult>, PluginError> {
        Ok(entries
            .iter()
            .map(|e| FilterResult { id: e.id, keep: e.level as u8 >= LogLevel::Warn as u8 })
            .collect())
    }

    fn report_usage() -> u64 {
        0
    }
}

// 3. 匯出
export!(Component);
```

`Cargo.toml`：

```toml
[package]
name = "filter"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = { version = "=0.58.0", default-features = false, features = ["macros", "realloc"] }
```

新增插件時，把 crate 加進 `rust/Cargo.toml` 的 `members`，再把它加進 `Makefile` 的 `rust` 目標。

注意事項：

- 必須遵守 [README](../README.md#介面) 的規則：輸出筆數要和輸入一致，Format 一筆對應一個 frame。
- 回傳 `Err` 代表這批有資料無法處理，host 會二分找出是哪幾筆，所以不要為了一筆壞資料吞掉整批。
- 插件預設沒有檔案、網路與環境變數權限。使用 `std::fs`、`std::net` 會讓 component import 對應的 WASI 介面，預設權限下會被拒絕載入。
- Transport 只能用 `wasi:http`（綁定由 `generate_all` 產生），請參考 `transport/src/lib.rs`。重試由 host 負責，插件只需送一次並如實回報狀態碼。

## 更換 Format 的輸出格式

以 cargo feature 選擇，預設是 `json-flat`，另有 `json-nested`、`syslog`、`protobuf`：

```bash
cd rust
cargo build --release --locked --target wasm32-wasip2 -p format --features syslog
```

## 故障測試插件

`fault/` 下的插件與一般插件相同，另外會依 message 中的標記觸發故障或能力探測，標記列表見 `fault/probe.rs`。它們用到 `std::fs` 與 `std::net`，載入時需要授權：

```yaml
capabilities:
  filter: {filesystem: inert, sockets: inert}
```
