# C# 插件

使用 Bytecode Alliance 的 [componentize-dotnet](https://github.com/bytecodealliance/componentize-dotnet)：建置時自動從 WIT 產生 C# 綁定，再以 NativeAOT-LLVM 把 C# 編譯成 WebAssembly component。

## 環境需求

| 項目 | 版本 | 用途 |
|---|---|---|
| Linux x86_64 | 已在 Ubuntu 24.04（glibc 2.39）驗證 | NativeAOT-LLVM 只提供 linux-x64 與 win-x64 的編譯器 |
| make、curl、tar、sha256sum、sha512sum | 系統套件 | 建置與下載 |
| 磁碟空間 | 約 1.5 GB | .NET SDK、NuGet 套件與 wasi-sdk |

不需要事先安裝 .NET，`make setup-csharp` 會下載固定版本到 `plugins/.toolchain/`，不影響系統環境。

## 工具與套件版本

| 項目 | 版本 | 來源 | 用途 |
|---|---|---|---|
| .NET SDK | 10.0.114 | [官方 tarball](https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.114/dotnet-sdk-10.0.114-linux-x64.tar.gz)，以 SHA-512 校驗；`global.json` 鎖定 | C# 編譯器 |
| wasi-sdk | 29.0 | GitHub release，以 SHA-256 校驗 | NativeAOT-LLVM 連結 WebAssembly 時使用 |
| `BytecodeAlliance.Componentize.DotNet.Wasm.SDK` | 0.8.0-preview00011 | nuget.org | 產生綁定並輸出 component（內含 wit-bindgen 0.58.0、wasm-tools 1.251.0） |
| `BytecodeAlliance.Componentize.DotNet.WitBindgen` | 0.8.0-preview00011 | nuget.org（間接相依） | 上一項使用的 wit-bindgen |
| `Microsoft.DotNet.ILCompiler.LLVM` 與 linux-x64、wasi-wasm 兩個 runtime 套件 | 10.0.0-rc.1.26306.1 | dotnet-experimental feed | NativeAOT：把 C# 編譯成 WebAssembly |
| `Microsoft.NET.ILLink.Tasks` | 10.0.14 | nuget.org（.NET SDK 自動加入） | 裁剪未使用的程式碼 |

所有 NuGet 套件（含間接相依）的版本與雜湊值記錄在各專案的 `packages.lock.json`，還原與建置時以 locked mode 檢查。`Microsoft.DotNet.ILCompiler.LLVM` 只發佈在 Microsoft 的 dotnet-experimental feed，`nuget.config` 已設定好。

componentize-dotnet 與 NativeAOT-LLVM 目前都是預覽版本。

## 從零編譯

```bash
cd plugins

# 1. 下載並安裝 .NET SDK、wasi-sdk，並還原 NuGet 套件（約 7 分鐘）
make setup-csharp

# 2. 編譯四個插件（第一次約 40 秒）
make csharp

# 3. 驗證（需要 forwarder 執行檔）
(cd ../core && cargo build --release)
make verify-csharp
```

產物在 `out/csharp/{parse,filter,format,transport}.wasm`，每個約 2–3 MB（包含 .NET 執行環境）。

### 每個插件的編譯步驟

`make csharp` 對每個階段執行：

```bash
. scripts/dotnet-env.sh   # 使用 .toolchain 內的 SDK 與套件快取
dotnet build csharp/filter -c Release -p:RestoreLockedMode=true \
    -p:WasiSdkRoot=$PWD/.toolchain/wasi-sdk-29.0-x86_64-linux
cp build/csharp/filter/bin/Release/net10.0/wasi-wasm/native/filter.wasm out/csharp/
```

`WasiSdkRoot` 指向 `.toolchain` 內的 wasi-sdk；不指定的話，componentize-dotnet 會自行下載一份到 `~/.wasi-sdk`。

## 權限

.NET 的 WASI 執行環境會 import `wasi:filesystem`（不會實際開檔），四個插件都需要 `filesystem: inert`，列在 `capabilities.yaml`。使用 C# 插件時要把它加進 `forwarder.yaml`：

```yaml
capabilities:
  parse: {filesystem: inert, sockets: deny}
  filter: {filesystem: inert, sockets: deny}
  format: {filesystem: inert, sockets: deny}
  transport: {filesystem: inert, sockets: deny}
```

`inert` 只連結介面，不給任何目錄。

## 原始碼結構

```
csharp/
├── global.json              鎖定 .NET SDK 版本
├── nuget.config             套件來源
├── Directory.Build.props    四個專案共用的設定與套件版本
├── capabilities.yaml
├── parse/      parse.csproj      ParseImpl.cs      packages.lock.json
├── filter/     filter.csproj     FilterImpl.cs     packages.lock.json
├── format/     format.csproj     FormatImpl.cs     packages.lock.json
└── transport/  transport.csproj  TransportImpl.cs  packages.lock.json
```

產生的綁定與編譯中間檔放在 `build/csharp/<階段>/`，不進 git。

## 開發一個插件

### 1. 專案檔

共用設定都在 `Directory.Build.props`，`.csproj` 只需指定 WIT 與 world：

```xml
<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <Wit Include="../../../core/wit" World="reduction-plugin" />
  </ItemGroup>
</Project>
```

### 2. 實作產生的介面

綁定會產生一個介面 `I<World>WorldExports`，每個匯出函式是一個靜態方法。實作類別的命名空間與名稱必須是 `<World>World.<World>WorldExportsImpl`，否則編譯失敗。以 Filter 為例：

```csharp
using PP = ReductionPluginWorld.wit.Imports.local.logProcess.v0_3_0.IPipelineProcessImports;

namespace ReductionPluginWorld;

public class ReductionPluginWorldExportsImpl : IReductionPluginWorldExports
{
    public static List<PP.FilterResult> Filter(List<PP.LogEntry> entries)
    {
        var results = new List<PP.FilterResult>(entries.Count);
        foreach (var entry in entries)
        {
            results.Add(new PP.FilterResult(entry.id, entry.level >= PP.LogLevel.WARN));
        }
        return results;
    }

    public static ulong ReportUsage() => 0;
}
```

| WIT | C# |
|---|---|
| `string` | `string` |
| `list<T>` | `List<T>`；`list<u8>` 為 `byte[]` |
| `tuple<string, string>` | `(string, string)` |
| `enum`（例如 log-level） | C# enum，成員為大寫（`PP.LogLevel.WARN`） |
| `record` | struct，欄位名稱為 camelCase（`entry.id`） |
| `result<T, E>` | 回傳 `T`；錯誤時丟出 `WitException<E>` |

回傳錯誤：

```csharp
throw new WitException<PP.ParseError>(PP.ParseError.InvalidFormat("JSON parse error"), 0);
```

各階段的匯出方法：

| 階段 | 方法 |
|---|---|
| Parse | `List<ParsedEntry> Parse(List<string> rawData)` |
| Filter | `List<FilterResult> Filter(List<LogEntry> structData)` |
| Format | `byte[] Format(List<LogEntry> structData)` |
| Transport | `void Init(TransportConfig config)`、`void Transport(List<byte[]> outputData)` |

### 3. 注意事項

- 輸入參數的記憶體由產生的程式碼釋放，插件不需要處理。
- Transport 透過產生的 `wasi:http` 綁定送出請求，流程見 `transport/TransportImpl.cs`：先呼叫 `Handle()` 再寫入 body。
- 新增專案時，第一次建置會產生 `packages.lock.json`，請一併提交。
- 修改 `Directory.Build.props` 的套件版本後，要刪除各專案的 `packages.lock.json` 並重新執行 `make setup-csharp` 產生新的 lock file。
