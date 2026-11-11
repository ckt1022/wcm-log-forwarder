# C 插件

## 環境需求

| 項目 | 版本 | 用途 |
|---|---|---|
| Linux x86_64 | 已在 Ubuntu 24.04（glibc 2.39）驗證 | wasi-sdk 的預編譯版本 |
| make、curl、tar、sha256sum | 系統套件 | 建置與下載 |
| Rust（cargo） | 1.93.1 | 安裝 wit-bindgen CLI |

## 工具與套件版本

全部記錄在 `../toolchain.env`，由 `make setup-c` 下載到 `plugins/.toolchain/`，不會動到系統環境。

| 項目 | 版本 | 來源 | 用途 |
|---|---|---|---|
| wasi-sdk | 29.0（clang 21.1.4） | [GitHub release](https://github.com/WebAssembly/wasi-sdk/releases/tag/wasi-sdk-29)，以 SHA-256 校驗 | clang、wasi-libc，以及直接產生 component 的 `wasm-component-ld` |
| wit-bindgen-cli | 0.58.0 | crates.io，`cargo install --locked` | 從 WIT 產生 C 綁定 |
| cJSON | 1.7.19 | [GitHub tag v1.7.19](https://github.com/DaveGamble/cJSON/tree/v1.7.19)，以 SHA-256 校驗 | Parse 解析 JSON |

不需要 wasm-tools，也不需要 WASI preview1 adapter：以 `wasm32-wasip2` 編譯時，wasi-sdk 的連結器直接輸出 component。

## 從零編譯

```bash
# 1. 共同需求（見 ../README.md）：make、curl、Rust 1.93.1
cd plugins

# 2. 下載並安裝 C 工具鏈（約 7 分鐘，大部分時間在編譯 wit-bindgen）
make setup-c

# 3. 編譯四個插件
make c

# 4. 驗證（需要 forwarder 執行檔）
(cd ../core && cargo build --release)
make verify-c
```

產物在 `out/c/{parse,filter,format,transport}.wasm`。

`make setup-c` 可重複執行：已安裝且版本正確的項目會略過；下載的檔案校驗失敗時會刪除並停止。

### 每個插件的編譯步驟

`make c` 對每個階段做兩步（以 Filter 為例）：

```bash
# 1. 從 WIT 產生綁定：reduction_plugin.h／.c，以及記錄 component 型別的 reduction_plugin_component_type.o
.toolchain/bin/wit-bindgen c --world reduction-plugin --out-dir build/c/filter ../core/wit

# 2. 與手寫的原始碼一起編譯成 component
.toolchain/wasi-sdk-29.0-x86_64-linux/bin/clang \
    --target=wasm32-wasip2 -mexec-model=reactor -O2 -Wall \
    -Ibuild/c/filter \
    c/filter/filter.c build/c/filter/reduction_plugin.c build/c/filter/reduction_plugin_component_type.o \
    -o out/c/filter.wasm
```

`-mexec-model=reactor` 代表插件沒有 `main`，只提供匯出函式。Parse 另外加入 cJSON：`-I.toolchain/cjson-1.7.19 .toolchain/cjson-1.7.19/cJSON.c`。

## 權限

四個插件都只 import 介面定義中的 WASI 介面，在預設權限（檔案、網路、環境變數全部拒絕）下就能載入，不需要額外授權。

## 原始碼結構

```
c/
├── parse/parse.c          使用 cJSON
├── filter/filter.c
├── format/format.c
└── transport/transport.c  使用 wasi:http
```

產生的綁定放在 `build/c/<階段>/`，每次編譯重新產生，不進 git。

## 開發一個插件

### 1. 找出要實作的函式

產生的標頭檔宣告了匯出函式，名稱是 `exports_<world>_<函式>`。例如 `build/c/filter/reduction_plugin.h`：

```c
bool exports_reduction_plugin_filter(reduction_plugin_list_log_entry_t *struct_data,
                                     reduction_plugin_list_filter_result_t *ret,
                                     reduction_plugin_plugin_error_t *err);
uint64_t exports_reduction_plugin_report_usage(void);
```

WIT 的 `result<T, E>` 對應成回傳 `bool`：成功時填 `ret` 並回傳 `true`，失敗時填 `err` 並回傳 `false`。

| WIT | C |
|---|---|
| `string` | `<world>_string_t { uint8_t *ptr; size_t len; }`，不以 `\0` 結尾 |
| `list<T>` | `{ T *ptr; size_t len; }` |
| `enum`（例如 log-level） | `uint8_t`，常數為 `LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_WARN` 等 |
| `variant`（例如 plugin-error） | `{ uint8_t tag; union val; }` |

### 2. 記憶體規則

| 對象 | 規則 |
|---|---|
| 輸入參數 | **由插件釋放**。產生的包裝函式不會釋放輸入；忘記釋放時，記憶體會隨批次累積，host 會在超過 `instance_recycle_percent` 時換新 instance（資料不受影響，但每次換新都有建立成本） |
| 回傳值（`ret`、`err`） | 用 `malloc` 配置或用 `<world>_string_dup` 複製；回傳後由產生的 `post_return` 釋放 |

wit-bindgen 產生的釋放函式有兩個缺陷（0.58.0 與目前最新的 0.63.0 都一樣），必須自己補：

| 產生的函式 | 缺陷 | 處理方式 |
|---|---|---|
| `*_list_log_entry_free`、`*_list_parsed_entry_free` | 不釋放每筆的 `tags` | 先對每筆呼叫 `*_list_tuple2_string_string_free(&entry.tags)` |
| `*_list_list_u8_free` | 只釋放外層陣列，不釋放每一行 | 先對每行呼叫 `*_list_u8_free` |

各插件裡的 `free_entries`、`free_lines` 就是做這件事。C 本來就要自行管理記憶體，所以建議照做；驗證的統計會顯示 `recycled_instances`，數值不為 0 代表有記憶體沒有釋放。

Filter 的完整範例：

```c
#include "reduction_plugin.h"
#include <stdlib.h>

static void free_entries(reduction_plugin_list_log_entry_t *entries) {
    for (size_t i = 0; i < entries->len; i++) {
        reduction_plugin_list_tuple2_string_string_free(&entries->ptr[i].tags);
    }
    reduction_plugin_list_log_entry_free(entries);
}

bool exports_reduction_plugin_filter(reduction_plugin_list_log_entry_t *entries,
                                     reduction_plugin_list_filter_result_t *ret,
                                     reduction_plugin_plugin_error_t *err) {
    (void)err;
    ret->len = entries->len;
    ret->ptr = malloc((entries->len ? entries->len : 1) * sizeof(reduction_plugin_filter_result_t));
    for (size_t i = 0; i < entries->len; i++) {
        ret->ptr[i].id = entries->ptr[i].id;
        ret->ptr[i].keep = entries->ptr[i].level >= LOCAL_LOG_PROCESS_PIPELINE_PROCESS_LOG_LEVEL_WARN;
    }
    free_entries(entries);
    return true;
}

uint64_t exports_reduction_plugin_report_usage(void) {
    return 0;
}
```

### 3. 其他注意事項

- 用到檔案或網路的 libc 函式（例如 `fopen`、`socket`）會讓插件 import 對應的 WASI 介面，預設權限下無法載入。
- Transport 透過產生的 `wasi_http_*` 函式送出請求，流程見 `transport.c` 的 `send_http_request`：先呼叫 `handle()` 再寫入 body，body 才不受 4096 位元組的緩衝限制。
- 新增階段或修改 WIT 後，執行 `make c` 會重新產生綁定。
